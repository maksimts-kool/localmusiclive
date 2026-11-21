"""Search YouTube and download clips into the library with yt-dlp.

Downloads land in media/youtube/ as "Artist - Title [video id].mp4". yt-dlp
works in a hidden folder next to it, so the library scan never sees a
half-finished file. A finished clip is indexed right away and, if asked,
queued or set to play next.
"""

import asyncio
import json
import logging
import random
import re
import sys
import time
from collections import deque
from dataclasses import dataclass, field, fields
from pathlib import Path
from typing import Any

from . import discover
from .db import DB
from .library import Library
from .settings import Settings

log = logging.getLogger("panel.youtube")

YTDLP = [sys.executable, "-m", "yt_dlp", "--no-warnings", "--ignore-config"]
VIDEO_ID = re.compile(r"^[A-Za-z0-9_-]{11}$")
URL_ID = re.compile(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/|embed/|live/)|youtu\.be/)([A-Za-z0-9_-]{11})")
# "(Official Video)", "[4K Remaster]", "(Lyrics)" and the like.
NOISE = re.compile(
    r"\s*[(\[{【][^)\]}】]*?\b(official|video|audio|lyrics?|lyric video|visuali[sz]er|clip|"
    r"hd|hq|4k|remaster(?:ed)?|mv|m/v)\b[^)\]}】]*[)\]}】]",
    re.IGNORECASE,
)
SEPARATORS = (" - ", " – ", " — ", " -- ", " | ", " : ")
QUOTES = "\"'“”‘’«»"
SEARCH_RESULTS = 15
PARALLEL_DOWNLOADS = 2
# A finished download leaves the list after this long (failed ones stay).
AUTO_DISMISS_SECONDS = 5.0
# New-song suggestions: built from this many most-played songs and artists,
# kept this long unless refreshed.
SUGGEST_SEEDS = 6
SUGGEST_ARTISTS = 3
# ...drawn at random (weighted by plays) from this many top songs and artists,
# so each refresh starts from a different mix of favourites.
SUGGEST_SEED_POOL = 20
SUGGEST_ARTIST_POOL = 8
SUGGEST_LIMIT = 20
# Songs shown in this many previous lists are left out of a refresh.
SUGGEST_REMEMBER = 3
SUGGEST_MIX_LENGTH = 25
SUGGEST_TTL = 6 * 3600
PATH_ID = re.compile(r"\[([A-Za-z0-9_-]{11})\]\.[^./]+$")


def video_id_from(text: str) -> str | None:
    text = text.strip()
    if VIDEO_ID.match(text):
        return text
    match = URL_ID.search(text)
    return match.group(1) if match else None


def clean_channel(name: str) -> str:
    name = re.sub(r"\s*-\s*Topic$", "", name or "")
    return re.sub(r"\s*VEVO$", "", name, flags=re.IGNORECASE).strip()


def split_title(title: str, channel: str = "", artist: str | None = None, track: str | None = None) -> tuple[str, str]:
    """Best guess at (artist, song title) for a YouTube video."""
    if artist and track:
        return artist.split(",")[0].strip(), track.strip()
    text = re.sub(r"\s{2,}", " ", NOISE.sub("", title)).strip(" -–—|")
    for sep in SEPARATORS:
        if sep in text:
            left, right = text.split(sep, 1)
            if left.strip() and right.strip():
                return left.strip().strip(QUOTES), right.strip().strip(QUOTES)
    return clean_channel(channel), text.strip(QUOTES)


def safe_filename(text: str) -> str:
    text = re.sub(r'[\\/:*?"<>|\x00-\x1f]', "", text)
    return re.sub(r"\s+", " ", text).strip(" .")[:120] or "video"


def download_formats(max_height: int = 0) -> str:
    """Best video plus best audio, optionally no taller than max_height."""
    if max_height > 0:
        cap = f"[height<={max_height}]"
        return f"bv*{cap}+ba/b{cap}/bv*+ba/b"
    return "bv*+ba/b"


def thumbnail(video_id: str) -> str:
    return f"https://i.ytimg.com/vi/{video_id}/mqdefault.jpg"


@dataclass
class Job:
    id: int
    video_id: str
    title: str
    then: str  # "none" | "queue" | "next"
    status: str = "waiting"  # waiting | downloading | adding | done | error
    progress: float = 0.0
    error: str | None = None
    track_id: int | None = None
    created_at: float = field(default_factory=time.time)
    task: asyncio.Task | None = field(default=None, repr=False)

    def public(self) -> dict:
        # Not asdict(): it would try to deep-copy the task.
        data = {f.name: getattr(self, f.name) for f in fields(self) if f.name != "task"}
        data["thumb"] = thumbnail(self.video_id)
        return data


class YouTube:
    def __init__(self, settings: Settings, db: DB, library: Library, engine: Any, hub: Any):
        self.s = settings
        self.db = db
        self.library = library
        self.engine = engine
        self.hub = hub
        self.jobs: dict[int, Job] = {}
        self.version: str | None = None
        self._next_id = 1
        self._slots = asyncio.Semaphore(PARALLEL_DOWNLOADS)
        self._last_push = 0.0
        self._suggest_cache: tuple[int, float, dict] | None = None
        self._suggest_lock = asyncio.Lock()
        self._recently_shown: deque[set[str]] = deque(maxlen=SUGGEST_REMEMBER)
        self._timers: set[asyncio.Task] = set()

    # --- yt-dlp ----------------------------------------------------------------

    def _base_args(self) -> list[str]:
        args = list(YTDLP)
        if self.s.cookies_file.exists():
            args += ["--cookies", str(self.s.cookies_file)]
        return args

    async def _run(self, *args: str, timeout: float = 60) -> tuple[int, str, str]:
        proc = await asyncio.create_subprocess_exec(
            *self._base_args(), *args,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        try:
            out, err = await asyncio.wait_for(proc.communicate(), timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            return -1, "", "yt-dlp timed out"
        return proc.returncode or 0, out.decode(errors="replace"), err.decode(errors="replace")

    @staticmethod
    def _error(stderr: str) -> str:
        lines = [l for l in stderr.splitlines() if l.startswith("ERROR")] or stderr.strip().splitlines()
        message = lines[-1] if lines else "yt-dlp failed"
        return re.sub(r"^ERROR:\s*(\[[^\]]+\]\s*[A-Za-z0-9_-]+:\s*)?", "", message)[:300]

    async def start(self) -> None:
        if self.s.ytdlp_auto_update:
            # YouTube changes often and old yt-dlp releases stop working.
            proc = await asyncio.create_subprocess_exec(
                sys.executable, "-m", "pip", "install", "-q", "-U", "--root-user-action=ignore",
                "--disable-pip-version-check", "yt-dlp[default,deno]",
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE,
            )
            _, err = await proc.communicate()
            if proc.returncode:
                log.warning("Could not update yt-dlp: %s", err.decode(errors="replace").strip()[-300:])
        code, out, _ = await self._run("--version", timeout=20)
        self.version = out.strip() if code == 0 else None
        log.info("yt-dlp %s", self.version or "is not available")

    # --- search ----------------------------------------------------------------

    def existing(self, video_id: str) -> Any:
        return self.db.one(
            "SELECT * FROM tracks WHERE missing = 0 AND instr(path, ?) > 0", f"[{video_id}]."
        )

    async def search(self, query: str) -> list[dict]:
        if video_id := video_id_from(query):
            code, out, err = await self._run(
                "-J", "--no-playlist", f"https://www.youtube.com/watch?v={video_id}"
            )
            entries = [json.loads(out)] if code == 0 else []
        else:
            code, out, err = await self._run(
                "-J", "--flat-playlist", f"ytsearch{SEARCH_RESULTS}:{query}"
            )
            entries = json.loads(out).get("entries", []) if code == 0 else []
        if code != 0:
            raise RuntimeError(self._error(err))

        results = []
        for entry in entries:
            if not entry or entry.get("live_status") in ("is_live", "is_upcoming", "post_live"):
                continue
            vid = entry.get("id")
            if not vid or not VIDEO_ID.match(vid):
                continue
            track = self.existing(vid)
            results.append({
                "id": vid,
                "title": entry.get("title") or vid,
                "channel": entry.get("channel") or entry.get("uploader") or "",
                "duration": entry.get("duration"),
                "views": entry.get("view_count"),
                "thumb": thumbnail(vid),
                "track_id": track["id"] if track else None,
            })
        return results

    # --- suggestions -----------------------------------------------------------

    async def _entries(self, target: str, *extra: str) -> list[dict]:
        code, out, err = await self._run("-J", "--flat-playlist", *extra, target, timeout=60)
        if code != 0:
            log.info("Suggestions: yt-dlp failed for %s: %s", target, self._error(err))
            return []
        try:
            return [e for e in json.loads(out).get("entries") or [] if e]
        except ValueError:
            return []

    def _seeds(self) -> list:
        rows = self.db.all(
            "SELECT * FROM tracks WHERE missing = 0 AND play_count > 0"
            " ORDER BY play_count DESC, last_played_at DESC LIMIT ?", SUGGEST_SEED_POOL,
        )
        rows = discover.weighted_sample(rows, [r["play_count"] for r in rows], SUGGEST_SEEDS)
        if len(rows) < SUGGEST_SEEDS:
            # Little history yet: fill up with random newer songs.
            taken = [r["id"] for r in rows] or [0]
            newer = self.db.all(
                f"SELECT * FROM tracks WHERE missing = 0 AND id NOT IN ({','.join('?' * len(taken))})"
                " ORDER BY added_at DESC LIMIT ?", *taken, SUGGEST_SEED_POOL,
            )
            rows += random.sample(newer, min(len(newer), SUGGEST_SEEDS - len(rows)))
        return sorted(rows, key=lambda r: -r["play_count"])

    def _artists(self) -> list:
        rows = self.db.all(
            "SELECT artist, SUM(play_count) AS plays FROM tracks"
            " WHERE missing = 0 AND artist != '' GROUP BY artist COLLATE NOCASE"
            " HAVING plays > 0 ORDER BY plays DESC LIMIT ?", SUGGEST_ARTIST_POOL,
        )
        return discover.weighted_sample(rows, [r["plays"] for r in rows], SUGGEST_ARTISTS)

    async def suggestions(self, refresh: bool = False) -> dict:
        """New songs to download, from the most-played songs in the library.

        Kept until songs are added or deleted, or until refreshed;
        a refresh starts from other favourites and skips recently shown songs.
        """
        async with self._suggest_lock:
            # A new or deleted song makes a new list; plays alone don't.
            key = self.db.one("SELECT COUNT(*) AS n FROM tracks WHERE missing = 0")["n"]
            cached = self._suggest_cache
            if not refresh and cached and cached[0] == key and time.time() - cached[1] < SUGGEST_TTL:
                return self._fresh(cached[2])
            seeds = self._seeds()
            artists = self._artists()

            sem = asyncio.Semaphore(3)

            async def mix(row) -> discover.Source:
                async with sem:
                    match = PATH_ID.search(row["path"])
                    vid = match.group(1) if match else None
                    if vid is None:
                        name = f"{row['artist']} - {row['title']}" if row["artist"] else row["title"]
                        found = await self._entries(f"ytsearch1:{name}")
                        vid = found[0].get("id") if found else None
                    entries = []
                    if vid and VIDEO_ID.match(vid):
                        entries = await self._entries(
                            f"https://www.youtube.com/watch?v={vid}&list=RD{vid}",
                            "--playlist-end", str(SUGGEST_MIX_LENGTH),
                        )
                    return discover.Source("mix", row["title"], row["play_count"], entries)

            async def by_artist(row) -> discover.Source:
                async with sem:
                    entries = await self._entries(f"ytsearch12:{row['artist']} music video")
                    return discover.Source("artist", row["artist"], row["plays"], entries)

            sources = await asyncio.gather(*(mix(r) for r in seeds), *(by_artist(r) for r in artists))

            tracks = self.db.all("SELECT artist, title, path FROM tracks")
            have_ids = {m.group(1) for t in tracks if (m := PATH_ID.search(t["path"]))}
            have_keys = {discover.song_key(t["artist"], t["title"]) for t in tracks}
            artist_songs: dict[str, int] = {}
            for t in tracks:
                if t["artist"]:
                    artist_songs[t["artist"].casefold()] = artist_songs.get(t["artist"].casefold(), 0) + 1

            ranked = discover.rank(
                sources, have_ids, have_keys, artist_songs, split_title, limit=SUGGEST_LIMIT * 4
            )
            ranked = discover.fresh_picks(ranked, set().union(*self._recently_shown), SUGGEST_LIMIT)
            self._recently_shown.append({item["id"] for item in ranked})
            result = {
                "based_on": [
                    {"id": r["id"], "title": r["title"], "artist": r["artist"], "plays": r["play_count"]}
                    for r in seeds
                ],
                "items": [
                    {
                        "id": item["id"],
                        "title": item.get("title") or item["id"],
                        "channel": item.get("channel") or item.get("uploader") or "",
                        "duration": item.get("duration"),
                        "views": item.get("view_count"),
                        "thumb": thumbnail(item["id"]),
                        "reasons": item["reasons"],
                    }
                    for item in ranked
                ],
                "created_at": time.time(),
            }
            self._suggest_cache = (key, time.time(), result)
            return self._fresh(result)

    def _fresh(self, result: dict) -> dict:
        """Drops suggestions downloaded since they were made."""
        items = [i for i in result["items"] if not self.existing(i["id"])]
        return {**result, "items": items}

    # --- downloads -------------------------------------------------------------

    def state(self) -> list[dict]:
        return [job.public() for job in sorted(self.jobs.values(), key=lambda j: -j.id)]

    async def _push(self, force: bool = False) -> None:
        now = time.time()
        if force or now - self._last_push > 1.0:
            self._last_push = now
            await self.engine.broadcast()

    async def download(self, video_id: str, then: str, title: str | None = None) -> Job:
        if not VIDEO_ID.match(video_id):
            raise ValueError("That is not a YouTube video id")
        for job in self.jobs.values():
            if job.video_id == video_id and job.status in ("waiting", "downloading", "adding"):
                if then != "none":
                    job.then = then
                return job

        # The title shown until yt-dlp reports the real one.
        job = Job(self._next_id, video_id, title=title or video_id, then=then)
        self._next_id += 1
        self.jobs[job.id] = job
        if track := self.existing(video_id):
            # Already in the library: no download, just queue it.
            job.title = f"{track['artist']} – {track['title']}" if track["artist"] else track["title"]
            job.track_id = track["id"]
            await self._finish(job)
        else:
            job.task = asyncio.create_task(self._download(job))
        await self._push(force=True)
        return job

    async def _finish(self, job: Job) -> None:
        if job.then != "none" and job.track_id is not None:
            await self.engine.queue_add(job.track_id, play_next=job.then == "next")
        job.status = "done"
        job.progress = 1.0
        timer = asyncio.create_task(self._auto_dismiss(job.id))
        self._timers.add(timer)
        timer.add_done_callback(self._timers.discard)

    async def _auto_dismiss(self, job_id: int) -> None:
        await asyncio.sleep(AUTO_DISMISS_SECONDS)
        job = self.jobs.get(job_id)
        if job is not None and job.status == "done":
            self.jobs.pop(job_id, None)
            await self._push(force=True)

    async def _download(self, job: Job) -> None:
        url = f"https://www.youtube.com/watch?v={job.video_id}"
        formats = download_formats(self.s.youtube_max_height)
        tmp = self.s.data_dir / "tmp"
        tmp.mkdir(parents=True, exist_ok=True)
        info_file = tmp / f"{job.video_id}.info.json"
        try:
            async with self._slots:
                job.status = "downloading"
                await self._push(force=True)

                code, out, err = await self._run("-J", "--no-playlist", "-f", formats, url)
                if code != 0:
                    raise RuntimeError(self._error(err))
                info = json.loads(out)
                if info.get("is_live") or info.get("live_status") in ("is_live", "is_upcoming"):
                    raise RuntimeError("Live streams can't be downloaded")
                duration = info.get("duration") or 0
                if duration > self.s.youtube_max_minutes * 60:
                    raise RuntimeError(
                        f"Longer than {self.s.youtube_max_minutes} minutes (YOUTUBE_MAX_MINUTES)"
                    )
                artist, title = split_title(
                    info.get("title", ""), info.get("channel") or info.get("uploader") or "",
                    info.get("artist"), info.get("track"),
                )
                job.title = f"{artist} – {title}" if artist else title
                info_file.write_text(json.dumps(info))
                await self._push(force=True)

                path = await self._fetch(job, info_file, formats, artist, title, info)

            job.status = "adding"
            await self._push(force=True)
            rel = path.relative_to(self.s.media_root).as_posix()
            job.track_id = await self.library.add_file(rel, artist, title)
            if job.track_id is None:
                raise RuntimeError("The downloaded file could not be read")
            await self.hub.broadcast({"type": "library"})
            await self._finish(job)
            log.info("Downloaded %s → %s", url, rel)
        except asyncio.CancelledError:
            job.status, job.error = "error", "Cancelled"
            raise
        except Exception as exc:
            job.status, job.error = "error", str(exc) or exc.__class__.__name__
            log.warning("Download of %s failed: %s", url, job.error)
        finally:
            info_file.unlink(missing_ok=True)
            self._cleanup(job.video_id)
            await self._push(force=True)

    async def _fetch(self, job: Job, info_file: Path, formats: str, artist: str, title: str, info: dict) -> Path:
        name = safe_filename(f"{artist} - {title}" if artist else title).replace("%", "%%")
        streams = len(info.get("requested_formats") or [None])
        proc = await asyncio.create_subprocess_exec(
            *self._base_args(),
            "--load-info-json", str(info_file),
            "-f", formats,
            "--merge-output-format", "mp4/mkv",
            "--paths", f"home:{self.s.download_dir}",
            "--paths", f"temp:{self.s.download_dir / '.incoming'}",
            "-o", f"{name} [%(id)s].%(ext)s",
            "--no-mtime",
            "--newline",
            "--progress",  # --print would otherwise silence it
            "--progress-template", "download:[progress] %(progress.downloaded_bytes)s "
            "%(progress.total_bytes)s %(progress.total_bytes_estimate)s",
            "--print", "after_move:filepath",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        stage, last_fraction, final = 0, 0.0, None
        try:
            assert proc.stdout and proc.stderr
            stderr_task = asyncio.create_task(proc.stderr.read())
            async for raw in proc.stdout:
                line = raw.decode(errors="replace").strip()
                if line.startswith("[progress]"):
                    parts = line.split()[1:]
                    done = float(parts[0]) if parts[0] not in ("NA", "None") else 0.0
                    total = next((float(p) for p in parts[1:] if p not in ("NA", "None")), 0.0)
                    fraction = done / total if total else 0.0
                    if fraction + 0.2 < last_fraction:
                        stage += 1  # next stream (video, then audio)
                    last_fraction = fraction
                    job.progress = min((min(stage, streams - 1) + fraction) / streams, 0.99)
                    await self._push()
                elif line.startswith("/"):
                    final = Path(line)
            await proc.wait()
            err = (await stderr_task).decode(errors="replace")
        except asyncio.CancelledError:
            proc.kill()
            await proc.wait()
            raise
        if proc.returncode != 0 or final is None or not final.exists():
            raise RuntimeError(self._error(err))
        return final

    def _cleanup(self, video_id: str) -> None:
        incoming = self.s.download_dir / ".incoming"
        if incoming.is_dir():
            for leftover in incoming.iterdir():
                if f"[{video_id}]" in leftover.name:
                    leftover.unlink(missing_ok=True)

    async def dismiss(self, job_id: int) -> None:
        job = self.jobs.pop(job_id, None)
        if job and job.task and not job.task.done():
            job.task.cancel()
        await self._push(force=True)
