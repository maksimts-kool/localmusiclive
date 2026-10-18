"""Indexes the clips in the media folder with ffprobe and makes thumbnails."""

import asyncio
import json
import logging
import os
import time
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from .db import DB
from .settings import Settings

log = logging.getLogger("panel.library")


@dataclass
class ScanResult:
    added: int = 0
    updated: int = 0
    missing: int = 0
    failed: int = 0

    @property
    def changed(self) -> bool:
        return bool(self.added or self.updated or self.missing)


def _walk(root: Path, extensions: tuple[str, ...], ignored: tuple[str, ...]) -> dict[str, tuple[int, float]]:
    found: dict[str, tuple[int, float]] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        rel_dir = Path(dirpath).relative_to(root)
        dirnames[:] = [
            d for d in dirnames
            if not d.startswith(".") and not (rel_dir == Path(".") and d in ignored)
        ]
        for name in filenames:
            if name.startswith(".") or not name.lower().endswith(extensions):
                continue
            full = Path(dirpath) / name
            try:
                st = full.stat()
            except OSError:
                continue
            found[(rel_dir / name).as_posix()] = (st.st_size, st.st_mtime)
    return found


def _tag(tags: dict, *names: str) -> str:
    lowered = {k.lower(): v for k, v in tags.items()}
    for name in names:
        value = str(lowered.get(name, "")).strip()
        if value:
            return value
    return ""


def describe(rel_path: str, probe: dict) -> dict | None:
    fmt = probe.get("format", {})
    streams = probe.get("streams", [])
    video = next(
        (s for s in streams
         if s.get("codec_type") == "video" and not s.get("disposition", {}).get("attached_pic")),
        None,
    )
    try:
        duration = float(fmt.get("duration") or (video or {}).get("duration") or 0)
    except ValueError:
        duration = 0.0
    if video is None or duration < 1.0:
        return None

    tags = dict(fmt.get("tags") or {})
    for stream in streams:
        tags = {**(stream.get("tags") or {}), **tags}

    artist = _tag(tags, "artist", "album_artist", "performer")
    title = _tag(tags, "title")
    stem = PurePosixPath(rel_path).stem
    if not title:
        if " - " in stem:
            guess_artist, title = (part.strip() for part in stem.split(" - ", 1))
            artist = artist or guess_artist
        else:
            title = stem

    return {
        "artist": artist,
        "title": title,
        "album": _tag(tags, "album"),
        "duration": round(duration, 3),
        "width": video.get("width"),
        "height": video.get("height"),
    }


class Library:
    def __init__(self, db: DB, settings: Settings):
        self.db = db
        self.settings = settings
        self._probe_sem = asyncio.Semaphore(4)
        self._thumb_sem = asyncio.Semaphore(2)
        self._scan_lock = asyncio.Lock()
        self.scanning = False
        settings.thumbs_dir.mkdir(parents=True, exist_ok=True)

    def abs_path(self, rel_path: str) -> str:
        return str(self.settings.media_root / rel_path)

    def thumb_path(self, track_id: int) -> Path:
        return self.settings.thumbs_dir / f"{track_id}.jpg"

    async def _run(self, *args: str, timeout: float = 60) -> tuple[int, bytes]:
        proc = await asyncio.create_subprocess_exec(
            *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL
        )
        try:
            out, _ = await asyncio.wait_for(proc.communicate(), timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
            return -1, b""
        return proc.returncode or 0, out

    async def probe(self, rel_path: str) -> dict | None:
        async with self._probe_sem:
            code, out = await self._run(
                "ffprobe", "-v", "error", "-print_format", "json",
                "-show_format", "-show_streams", self.abs_path(rel_path),
            )
        if code != 0:
            return None
        try:
            return describe(rel_path, json.loads(out))
        except ValueError:
            return None

    async def make_thumb(self, track_id: int, rel_path: str, duration: float) -> bool:
        target = self.thumb_path(track_id)
        at = f"{min(duration * 0.3, 60.0):.2f}"
        async with self._thumb_sem:
            code, _ = await self._run(
                "ffmpeg", "-v", "error", "-y", "-ss", at, "-i", self.abs_path(rel_path),
                "-frames:v", "1", "-vf", "scale=320:-2", "-q:v", "5", str(target),
            )
        return code == 0 and target.exists()

    async def scan(self) -> ScanResult:
        async with self._scan_lock:
            self.scanning = True
            try:
                return await self._scan()
            finally:
                self.scanning = False

    async def _scan(self) -> ScanResult:
        result = ScanResult()
        root = self.settings.media_root
        if not root.is_dir():
            log.warning("Media folder %s does not exist", root)
            return result

        found = await asyncio.to_thread(
            _walk, root, self.settings.video_extensions, self.settings.ignored_folders
        )
        known = {row["path"]: row for row in self.db.all("SELECT * FROM tracks")}

        gone = [row["id"] for path, row in known.items() if path not in found and not row["missing"]]
        if gone:
            self.db.run(
                f"UPDATE tracks SET missing = 1 WHERE id IN ({','.join('?' * len(gone))})", *gone
            )
            result.missing = len(gone)

        todo = [
            (path, size, mtime)
            for path, (size, mtime) in found.items()
            if path not in known
            or known[path]["size"] != size
            or abs(known[path]["mtime"] - mtime) > 0.001
            or known[path]["missing"]
        ]

        async def index(path: str, size: int, mtime: float) -> None:
            info = await self.probe(path)
            if info is None:
                result.failed += 1
                return
            folder = str(PurePosixPath(path).parent)
            row = known.get(path)
            if row is None:
                cur = self.db.run(
                    "INSERT INTO tracks (path, folder, artist, title, album, duration, width, height,"
                    " size, mtime, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    path, "" if folder == "." else folder, info["artist"], info["title"],
                    info["album"], info["duration"], info["width"], info["height"],
                    size, mtime, time.time(),
                )
                track_id = cur.lastrowid
                result.added += 1
            else:
                track_id = row["id"]
                self.db.run(
                    "UPDATE tracks SET artist = ?, title = ?, album = ?, duration = ?, width = ?,"
                    " height = ?, size = ?, mtime = ?, missing = 0 WHERE id = ?",
                    info["artist"], info["title"], info["album"], info["duration"],
                    info["width"], info["height"], size, mtime, track_id,
                )
                result.updated += 1
            has_thumb = await self.make_thumb(track_id, path, info["duration"])
            self.db.run("UPDATE tracks SET has_thumb = ? WHERE id = ?", int(has_thumb), track_id)

        await asyncio.gather(*(index(*item) for item in todo))
        if result.changed or result.failed:
            log.info(
                "Library scan: %d added, %d updated, %d missing, %d unreadable",
                result.added, result.updated, result.missing, result.failed,
            )
        return result
