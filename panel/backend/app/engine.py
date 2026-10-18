"""Keeps ffplayout's playlists in step with the queue.

ffplayout reports what it plays four times a second. When the clip changes,
the engine records history, takes the clip off the queue or autoplay list and
rebuilds the playlists for today and tomorrow (see timeline.py). Queue edits
rebuild the same way; ffplayout picks the change up at the next clip
boundary, so the clip on air is never interrupted.

Skip trims the clip on air in the playlist so the next clip is due now, then
asks ffplayout to resync with the playlist ("reset"). ffplayout's own "next"
would shift its clock instead, which breaks the hand-over at midnight.
"""

import asyncio
import io
import json
import logging
import os
import random
import time
from collections import Counter, deque
from dataclasses import dataclass, field, replace
from datetime import date as Date, datetime, timedelta
from typing import Any, Iterator
from zoneinfo import ZoneInfo

from .db import DB
from .ffplayout import FFplayout, FFplayoutError
from .library import Library
from . import overlay
from .settings import Settings
from .timeline import DAY, MIN_PART, Slot, gap, layout

log = logging.getLogger("panel.engine")

# After a skip, the skipped clip stays scheduled this much past "now", so
# ffplayout starts the next clip from its first frame.
SKIP_LEAD = 0.5
# Don't bother skipping a clip with less than this left.
MIN_SKIP_REMAINING = 2.0
# A fresh schedule's time placeholder ends this far in the past. If ffplayout
# lands inside the placeholder it plays a 60 s black dummy instead.
GAP_BACKDATE = 0.3
FRESH_START_COOLDOWN = 8.0
# ffplayout shifts its clock by an hour on daylight-saving changes; while it
# does, playlist rewrites can't line up and skip falls back to ffplayout's own.
MAX_SHIFT = 2.0
# A clip that resumes within this many seconds after playout stopped is the
# same play, not a new history entry.
RESUME_WINDOW = 600.0
# Check queue files on disk for this many autoplay picks too; deeper picks
# rely on the periodic library scan.
CHECK_FILES_AHEAD = 3

AUTOPLAY_DEFAULTS = {
    "enabled": True,
    "mode": "shuffle",  # shuffle | least_played | in_order
    "folder": "",
    "avoid_repeat": 20,
}

OVERLAY_DEFAULTS = {
    "enabled": True,
    # "start_end": "Now playing" as a song starts, "Up next" before it ends.
    # "always": the "Now playing" card the whole time.
    "mode": "start_end",
    "start_seconds": 15,
    "end_seconds": 20,
    "show_next": True,
    "position": "bottom-left",
    "margin": 48,
    "scale": 100,
    "accent_color": "#ff5c7a",
    "background_color": "#0e1118",
    "background_opacity": 0.9,
    "stroke": True,
    "stroke_color": "#ffffff",
    "stroke_opacity": 0.16,
    "stroke_width": 1.5,
    "radius": 18,
    "shadow": True,
    "show_thumbnail": True,
    "show_progress": True,
}
OVERLAY_POSITIONS = (
    "bottom-left", "bottom-center", "bottom-right", "top-left", "top-center", "top-right",
)
# Card fades in/out over this long; "Up next" is gone this long before the cut.
CARD_FADE = 0.5
CARD_END_GAP = 0.2
# Where the panel puts the card for ffplayout, relative to the media folder.
CARD_FILE = ".overlay/card.png"


class EngineError(Exception):
    pass


@dataclass
class Current:
    date: str  # playlist the clip belongs to
    index: int
    slot: Slot | None  # None: not something the panel scheduled
    prefix: list[Slot]  # that playlist up to and including the clip
    source: str
    elapsed: float  # seconds since the slot's scheduled start, at received_at
    received_at: float
    shift: float
    history_id: int | None = None

    @property
    def managed(self) -> bool:
        return self.slot is not None and self.slot.kind != "gap"


@dataclass
class Plan:
    date: str
    today: list[Slot]
    tomorrow: list[Slot]
    index: int  # clip on air within `today`


@dataclass
class Status:
    ffplayout: str = "connecting"  # connecting | ok | error | reconnecting
    running: bool = False
    mode: str | None = None
    output: str | None = None
    error: str | None = None
    warnings: list[str] = field(default_factory=list)


def label(track: Any) -> str:
    if track is None:
        return ""
    artist, title = track["artist"], track["title"]
    return f"{artist} – {title}" if artist else title


def card_phase(position: float, until_next: float | None, settings: dict) -> tuple[str, float] | None:
    """Which card the stream shows ("now" or "next") and how opaque it is.

    `position` is how far into the song we are, `until_next` how long until the
    next song starts (None if nothing follows).
    """
    if not settings["enabled"]:
        return None
    has_next = settings["show_next"] and until_next is not None
    if settings["mode"] == "always":
        return "now", min(1.0, max(position, 0.0) / CARD_FADE)
    if has_next and CARD_END_GAP < until_next <= settings["end_seconds"]:
        shown = settings["end_seconds"] - until_next
        return "next", min(1.0, shown / CARD_FADE, (until_next - CARD_END_GAP) / CARD_FADE)
    if 0 <= position < settings["start_seconds"]:
        opacity = min(1.0, position / CARD_FADE, (settings["start_seconds"] - position) / CARD_FADE)
        if until_next is not None:
            opacity = min(opacity, max(until_next - CARD_END_GAP, 0.0) / CARD_FADE)
        return ("now", opacity) if opacity > 0 else None
    return None


def clock(seconds: float) -> str:
    seconds = max(0, int(seconds + 0.999))
    return f"{seconds // 60}:{seconds % 60:02d}"


def shorten(text: str, limit: int = 56) -> str:
    return text if len(text) <= limit else text[: limit - 1].rstrip() + "…"


def track_json(row: Any) -> dict:
    return {
        "id": row["id"],
        "artist": row["artist"],
        "title": row["title"],
        "album": row["album"],
        "path": row["path"],
        "folder": row["folder"],
        "duration": row["duration"],
        "play_count": row["play_count"],
        "last_played_at": row["last_played_at"],
        "thumb": f"/api/thumbs/{row['id']}.jpg?v={int(row['mtime'])}" if row["has_thumb"] else None,
    }


class Picker:
    """Chooses autoplay tracks."""

    def __init__(self, pool: list, settings: dict, recent: deque, planned: Counter):
        self.mode = settings["mode"]
        self.pool = sorted(pool, key=lambda t: t["path"])
        self.order = {t["id"]: i for i, t in enumerate(self.pool)}
        self.recent = recent
        self.counts = Counter({t["id"]: t["play_count"] for t in self.pool}) + planned

    def pick(self, window: int) -> Any:
        if self.mode == "in_order":
            last = next((tid for tid in reversed(self.recent) if tid in self.order), None)
            track = self.pool[(self.order[last] + 1) % len(self.pool) if last is not None else 0]
        else:
            avoid = set(list(self.recent)[-window:]) if window > 0 else set()
            choices = [t for t in self.pool if t["id"] not in avoid] or self.pool
            if self.mode == "least_played":
                fewest = min(self.counts[t["id"]] for t in choices)
                choices = [t for t in choices if self.counts[t["id"]] == fewest]
            track = random.choice(choices)
        self.counts[track["id"]] += 1
        return track


class Engine:
    def __init__(self, settings: Settings, db: DB, ff: FFplayout, library: Library, hub: Any):
        self.s = settings
        self.db = db
        self.ff = ff
        self.library = library
        self.hub = hub
        self.tz = ZoneInfo(settings.timezone)
        self.lock = asyncio.Lock()
        self.status = Status()
        self.current: Current | None = None
        self.plan: Plan | None = None
        self._last_key: tuple | None = None
        self._latest_event: tuple[float, Any] | None = None
        self._event_flag = asyncio.Event()
        self._written: dict[str, str] = {}
        self._plan_cache: dict[int, list[Slot]] = {}
        self._last_fresh = 0.0
        self._fresh_failures = 0
        self._interrupted: tuple[str, int, int, float] | None = None
        self._skip_pending = False
        self._card_key: tuple | None = None
        self._text_cleared = False
        self._tasks: list[asyncio.Task] = []
        self.downloads: Any = None  # YouTube downloader, set by main

    # --- lifecycle -------------------------------------------------------------

    async def start(self) -> None:
        self._tasks = [
            asyncio.create_task(self._run_events(), name="events"),
            asyncio.create_task(self._process_events(), name="process"),
            asyncio.create_task(self._housekeeping(), name="housekeeping"),
            asyncio.create_task(self._rescan_loop(), name="rescan"),
            asyncio.create_task(self._overlay_loop(), name="overlay"),
        ]

    async def stop(self) -> None:
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)

    # --- time ------------------------------------------------------------------

    def playlist_date(self, ts: float) -> str:
        local = datetime.fromtimestamp(ts, self.tz) - timedelta(seconds=self.s.day_start_sec)
        return local.date().isoformat()

    def day_pos(self, ts: float) -> float:
        local = datetime.fromtimestamp(ts, self.tz)
        sod = local.hour * 3600 + local.minute * 60 + local.second + local.microsecond / 1e6
        return (sod - self.s.day_start_sec) % DAY

    @staticmethod
    def next_date(date: str) -> str:
        return (Date.fromisoformat(date) + timedelta(days=1)).isoformat()

    @staticmethod
    def _elapsed(cur: Current) -> float:
        return cur.elapsed + (time.time() - cur.received_at)

    # --- settings --------------------------------------------------------------

    def autoplay_settings(self) -> dict:
        return {**AUTOPLAY_DEFAULTS, **self.db.get_setting("autoplay", {})}

    def overlay_settings(self) -> dict:
        return {**OVERLAY_DEFAULTS, **self.db.get_setting("overlay", {})}

    # --- ffplayout setup -------------------------------------------------------

    async def _bootstrap(self) -> None:
        while True:
            try:
                if await self.ff.setup_required():
                    log.info("Running ffplayout first-time setup")
                    await self.ff.complete_setup(self.s.ffplayout_mail, str(self.s.media_root))
                await self.ff.login()
                await self._configure_ffplayout()
                self.status.ffplayout = "ok"
                self.status.error = None
                await self.broadcast()
                return
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # keep retrying until ffplayout is reachable
                log.warning("ffplayout not ready: %s", exc)
                self.status.ffplayout = "error"
                self.status.error = str(exc)
                await self.broadcast()
                await asyncio.sleep(5)

    async def _configure_ffplayout(self) -> None:
        """Apply the settings the panel owns; leave everything else to ffplayout's UI."""
        cfg = await self.ff.get_config()
        before = json.dumps(cfg, sort_keys=True)

        cfg["playlist"].update(day_start=self.s.day_start, length="24:00:00", infinit=False)
        cfg["processing"]["mode"] = "playlist"
        out = cfg["output"]
        if self.s.youtube_stream_key:
            # ffplayout keeps one output record per mode; selecting the
            # "stream" record by id is what switches the mode.
            stream = next((o for o in await self.ff.outputs() if o.get("name") == "stream"), None)
            if stream is None:
                raise FFplayoutError("ffplayout has no stream output to configure")
            out.update(
                id=stream["id"],
                mode="stream",
                stream_type="rtmp",
                stream_url=f"{self.s.youtube_rtmp_url}/{self.s.youtube_stream_key}",
            )
        out.update(
            width=self.s.stream_width,
            height=self.s.stream_height,
            fps=float(self.s.stream_fps),
            audio_bitrate=self.s.audio_bitrate_kbps,
        )
        if out.get("video_codec", "libx264") == "libx264":
            options = dict(out.get("video_options") or {})
            options.update(
                preset=self.s.x264_preset,
                rate_control="cbr",
                maxrate=str(self.s.video_bitrate_kbps),
            )
            options.pop("quality", None)
            out["video_options"] = options

        # The song card is drawn by the panel and shown through ffplayout's logo
        # slot (see live-logo.patch). ffplayout only accepts a logo file that exists.
        overlay.write(overlay.BLANK, self.s.media_root / CARD_FILE)
        self._card_key = None
        cfg["processing"].update(
            add_logo=True, logo=CARD_FILE, logo_position="0:0", logo_opacity=1.0, logo_scale=""
        )

        if not self.db.get_setting("ffplayout_defaults_applied"):
            # One-time default; change it in ffplayout's UI afterwards.
            # YouTube plays music at about -14 LUFS; ffplayout defaults to -23.
            cfg["audio"]["loudness_target_lufs"] = -14.0

        restart = False
        if json.dumps(cfg, sort_keys=True) != before:
            result = await self.ff.put_config(cfg)
            restart = bool(isinstance(result, dict) and result.get("requires_restart"))
            log.info("Updated ffplayout configuration")
        self.db.set_setting("ffplayout_defaults_applied", True)

        channel = await self.ff.get_channel()
        if channel.get("timezone") != self.s.timezone:
            channel["timezone"] = self.s.timezone
            await self.ff.patch_channel(channel)
            restart = True
            log.info("Set ffplayout channel time zone to %s", self.s.timezone)

        warnings = []
        if channel.get("storage") != str(self.s.media_root):
            warnings.append(
                f"ffplayout's storage is {channel.get('storage')}, but the panel expects "
                f"{self.s.media_root}. Clips will not be found."
            )
        self.status.warnings = warnings
        self.status.mode = out.get("mode")
        url = out.get("stream_url") or ""
        self.status.output = (
            url.rsplit("/", 1)[0] + "/••••" + url[-4:] if out.get("mode") == "stream" and url else None
        )

        if restart and await self.ff.process("status") == "active":
            log.info("Restarting playout to apply the new configuration")
            await self.ff.process("restart")

    # --- playout events --------------------------------------------------------

    async def _run_events(self) -> None:
        await self._bootstrap()
        while True:
            try:
                async for event in self.ff.events():
                    if event == "connected":
                        continue
                    self._latest_event = (time.time(), event)
                    self._event_flag.set()
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                log.warning("Lost ffplayout event stream: %s", exc)
            self.status.ffplayout = "reconnecting"
            self._latest_event = (time.time(), None)
            self._event_flag.set()
            await asyncio.sleep(2)

    async def _process_events(self) -> None:
        # Separate from the reader so slow work never backs up ffplayout's
        # event queue (it drops clients whose queue fills up).
        while True:
            await self._event_flag.wait()
            self._event_flag.clear()
            if self._latest_event is None:
                continue
            received, event = self._latest_event
            try:
                if isinstance(event, dict):
                    await self._on_event(event, received)
                elif event == "not running" or event is None:
                    await self._on_stopped(connection_lost=event is None)
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("Failed to handle playout event")

    async def _on_stopped(self, connection_lost: bool) -> None:
        if connection_lost:
            # Playout may well continue; keep the current clip so a reconnect
            # doesn't count it as a new play.
            await self.broadcast()
            return
        changed = self.status.running or self.status.ffplayout != "ok"
        self.status.ffplayout = "ok"
        if self.status.running:
            async with self.lock:
                cur = self.current
                if cur is not None and cur.history_id is not None:
                    self._interrupted = (cur.date, cur.index, cur.history_id, time.time())
                self._close_history(time.time())
                self.current = None
                self._last_key = None
            self.status.running = False
            log.info("Playout stopped")
        if changed:
            await self.broadcast()

    async def _on_event(self, event: dict, received: float) -> None:
        if not self.status.running or self.status.ffplayout != "ok":
            self.status.running = True
            self.status.ffplayout = "ok"
            self._text_cleared = False
            log.info("Playout is running")

        index = event.get("index")
        if index is None:
            return
        source = (event.get("media") or {}).get("source") or ""
        elapsed = float(event.get("elapsed") or 0.0)
        # ffplayout compares against seconds since midnight, so clips scheduled
        # past midnight (day start other than 00:00) report a day too little.
        if elapsed < -DAY / 2:
            elapsed += DAY
        elif elapsed > DAY / 2:
            elapsed -= DAY
        shift = float(event.get("shift") or 0.0)
        # When the clip started on ffplayout's (possibly shifted) clock.
        date_guess = self.playlist_date(received - elapsed + shift + 0.5)

        key = (date_guess, index, source)
        cur = self.current
        if key == self._last_key and cur is not None:
            cur.elapsed, cur.received_at, cur.shift = elapsed, received, shift
            return

        self._last_key = key
        async with self.lock:
            await self._on_clip(index, source, elapsed, shift, received, date_guess)
        await self.broadcast()

    def _plans(self, date: str) -> Iterator[list[Slot]]:
        rows = self.db.all(
            "SELECT id, slots FROM plans WHERE date = ? ORDER BY id DESC LIMIT 8", date
        )
        for row in rows:
            if row["id"] not in self._plan_cache:
                self._plan_cache[row["id"]] = [Slot.from_dict(s) for s in json.loads(row["slots"])]
            yield self._plan_cache[row["id"]]

    def _match(self, index: int, source: str, date_guess: str) -> tuple[str, list[Slot]] | None:
        guess = Date.fromisoformat(date_guess)
        for offset in (0, -1, 1):
            date = (guess + timedelta(days=offset)).isoformat()
            for slots in self._plans(date):
                if index < len(slots) and slots[index].path == source:
                    return date, slots
        return None

    async def _on_clip(
        self, index: int, source: str, elapsed: float, shift: float, received: float, date_guess: str
    ) -> None:
        match = self._match(index, source, date_guess)
        prev = self.current
        date, slot, prefix = date_guess, None, []
        if match:
            date, slots = match
            slot, prefix = slots[index], slots[: index + 1]

        started_at = received - elapsed
        continues = (
            slot is not None and slot.part == 1 and prev is not None
            and prev.slot is not None and prev.slot.track_id == slot.track_id
        )
        history_id = prev.history_id if continues else None
        interrupted, self._interrupted = self._interrupted, None
        if (
            prev is None and slot is not None and interrupted is not None
            and interrupted[:2] == (date, index) and time.time() - interrupted[3] < RESUME_WINDOW
        ):
            # Playout restarted in the middle of this clip: same play, not a new one.
            history_id = interrupted[2]
            self.db.run("UPDATE history SET ended_at = NULL WHERE id = ?", history_id)
        elif not continues:
            self._close_history(started_at)
            if slot is not None and slot.kind != "gap":
                history_id = self._start_history(slot, started_at)

        self.current = Current(date, index, slot, prefix, source, elapsed, received, shift, history_id)
        if self.current.managed:
            log.info("On air: %s", slot.title or slot.path)
            self._fresh_failures = 0
            await self._safe_rebuild()
        else:
            self.plan = None
            log.info("On air: %s", "ffplayout placeholder" if not source else source)
            if self._has_material():
                await self._fresh_start("placeholder or unknown clip on air")

    def _start_history(self, slot: Slot, started_at: float) -> int:
        with self.db.tx():
            requested_by = None
            if slot.queue_id is not None:
                row = self.db.one("SELECT requested_by FROM queue WHERE id = ?", slot.queue_id)
                requested_by = row["requested_by"] if row else None
                self.db.run("DELETE FROM queue WHERE id = ?", slot.queue_id)
            if slot.auto_id is not None:
                self.db.run("DELETE FROM upcoming WHERE id = ?", slot.auto_id)
            if slot.track_id is not None:
                self.db.run(
                    "UPDATE tracks SET play_count = play_count + 1, last_played_at = ? WHERE id = ?",
                    started_at, slot.track_id,
                )
            cur = self.db.run(
                "INSERT INTO history (track_id, kind, started_at, requested_by) VALUES (?, ?, ?, ?)",
                slot.track_id, slot.kind, started_at, requested_by,
            )
            return cur.lastrowid

    def _close_history(self, ended_at: float) -> None:
        cur = self.current
        if cur is not None and cur.history_id is not None:
            self.db.run(
                "UPDATE history SET ended_at = ?, skipped = ? WHERE id = ? AND ended_at IS NULL",
                ended_at, int(self._skip_pending), cur.history_id,
            )
        self._skip_pending = False

    # --- scheduling ------------------------------------------------------------

    def _slot(self, track: Any, kind: str, **refs: Any) -> Slot:
        return Slot(
            kind=kind,
            path=self.library.abs_path(track["path"]),
            seek=0.0,
            out=track["duration"],
            duration=track["duration"],
            track_id=track["id"],
            title=label(track),
            **refs,
        )

    def _eligible(self) -> tuple[dict[int, Any], list[Any], dict]:
        tracks = {row["id"]: row for row in self.db.all("SELECT * FROM tracks WHERE missing = 0")}
        settings = self.autoplay_settings()
        folder = settings["folder"].strip("/")
        pool = [
            t for t in tracks.values()
            if not folder or t["folder"] == folder or t["folder"].startswith(folder + "/")
        ]
        return tracks, pool, settings

    def _has_material(self) -> bool:
        if self.db.one("SELECT 1 FROM queue JOIN tracks ON tracks.id = queue.track_id WHERE missing = 0"):
            return True
        _, pool, settings = self._eligible()
        return settings["enabled"] and bool(pool)

    def _candidates(self) -> Iterator[Slot]:
        """Queue first, then autoplay picks, as many as the layout asks for."""
        tracks, pool, settings = self._eligible()
        recent: deque = deque(maxlen=500)
        for row in reversed(self.db.all(
            "SELECT track_id FROM history WHERE track_id IS NOT NULL ORDER BY id DESC LIMIT 100"
        )):
            recent.append(row["track_id"])

        for row in self.db.all("SELECT * FROM queue ORDER BY position, id"):
            track = tracks.get(row["track_id"])
            if track is None or not os.path.exists(self.library.abs_path(track["path"])):
                continue
            recent.append(track["id"])
            yield self._slot(track, "queue", queue_id=row["id"])

        if not settings["enabled"] or not pool:
            return
        in_pool = {t["id"] for t in pool}
        window = max(0, min(int(settings["avoid_repeat"]), len(pool) - 1))
        planned: Counter = Counter()
        checked = 0

        def usable(track: Any) -> bool:
            nonlocal checked
            if track is None or track["id"] not in in_pool:
                return False
            if window and track["id"] in list(recent)[-window:]:
                return False
            if checked < CHECK_FILES_AHEAD:
                checked += 1
                return os.path.exists(self.library.abs_path(track["path"]))
            return True

        for row in self.db.all("SELECT * FROM upcoming ORDER BY position, id"):
            track = tracks.get(row["track_id"])
            if not usable(track):
                self.db.run("DELETE FROM upcoming WHERE id = ?", row["id"])
                continue
            recent.append(track["id"])
            planned[track["id"]] += 1
            yield self._slot(track, "auto", auto_id=row["id"])

        picker = Picker(pool, settings, recent, planned)
        position = (self.db.one("SELECT MAX(position) AS p FROM upcoming")["p"] or 0) + 1
        while True:
            track = picker.pick(window)
            cur = self.db.run(
                "INSERT INTO upcoming (track_id, position) VALUES (?, ?)", track["id"], position
            )
            position += 1
            recent.append(track["id"])
            yield self._slot(track, "auto", auto_id=cur.lastrowid)

    async def _write(self, date: str, slots: list[Slot]) -> None:
        program = [s.to_media() for s in slots]
        blob = json.dumps(program)
        if self._written.get(date) != blob:
            if slots:
                await self.ff.save_playlist(date, program)
            else:
                await self.ff.delete_playlist(date)
            self._written[date] = blob

        data = json.dumps([s.to_dict() for s in slots])
        latest = self.db.one("SELECT slots FROM plans WHERE date = ? ORDER BY id DESC LIMIT 1", date)
        if latest is None or latest["slots"] != data:
            self.db.run(
                "INSERT INTO plans (date, created_at, slots) VALUES (?, ?, ?)", date, time.time(), data
            )

    async def _rebuild(self, skip: bool = False) -> bool:
        cur = self.current
        if cur is None or not cur.managed:
            if self.status.running and self._has_material():
                await self._fresh_start("new material while idle")
            return False

        prefix = [replace(s) for s in cur.prefix]
        if skip:
            played = max(self._elapsed(cur), 0.0)
            last = prefix[-1]
            if last.length - played < MIN_SKIP_REMAINING:
                return False
            prefix[-1] = replace(last, out=round(last.seek + played + SKIP_LEAD, 3), full_out=None)

        with self.db.tx():
            today, tomorrow = layout(prefix, self._candidates())
        await self._write(cur.date, today)
        await self._write(self.next_date(cur.date), tomorrow)
        cur.prefix, cur.slot = prefix, prefix[-1]
        self.plan = Plan(cur.date, today, tomorrow, cur.index)

        if skip:
            self._skip_pending = True
            await self.ff.playout("reset")
        return True

    async def _safe_rebuild(self) -> None:
        try:
            await self._rebuild()
        except FFplayoutError as exc:
            log.warning("Could not update ffplayout's playlist: %s", exc)
            self.status.error = str(exc)

    async def _fresh_start(self, reason: str, reset: bool = True) -> None:
        """Schedule from now on, ignoring whatever ffplayout plays."""
        now = time.time()
        # If resyncing keeps failing (for example ffplayout can't find the
        # clips), stop interrupting the stream every few seconds.
        cooldown = FRESH_START_COOLDOWN if self._fresh_failures < 3 else 300.0
        if now - self._last_fresh < cooldown:
            return
        self._last_fresh = now
        if reset and self.status.running:
            self._fresh_failures += 1
        date, pos = self.playlist_date(now), self.day_pos(now)
        placeholder = pos - GAP_BACKDATE
        prefix = [gap(placeholder)] if placeholder >= MIN_PART else []

        with self.db.tx():
            today, tomorrow = layout(prefix, self._candidates())
        if not any(s.kind != "gap" for s in today + tomorrow):
            return
        log.info("Starting a fresh schedule: %s", reason)
        await self._write(date, today)
        await self._write(self.next_date(date), tomorrow)
        self.plan = Plan(date, today, tomorrow, len(prefix) - 1)
        if reset and self.status.running:
            await self.ff.playout("reset")

    def _following(self) -> Iterator[Slot]:
        """Slots after the clip on air, in play order."""
        plan, cur = self.plan, self.current
        if plan is None:
            return
        if cur is not None and cur.managed:
            start = plan.index + 1
        else:
            # Fresh schedule: everything after the placeholder.
            start = next((i for i, s in enumerate(plan.today) if s.kind != "gap"), len(plan.today))
        yield from plan.today[start:]
        yield from plan.tomorrow

    def _next_slot(self) -> Slot | None:
        return next((s for s in self._following() if s.kind != "gap" and s.part == 0), None)

    # --- overlay ---------------------------------------------------------------

    def _track(self, track_id: int | None) -> Any:
        if track_id is None:
            return None
        return self.db.one("SELECT * FROM tracks WHERE id = ?", track_id)

    def _card_for(self, phase: str, opacity: float, position: float, until_next: float | None,
                  length: float, now_slot: Slot | None, next_slot: Slot | None) -> overlay.Card | None:
        slot = next_slot if phase == "next" else now_slot
        if slot is None:
            return None
        track = self._track(slot.track_id)
        title = track["title"] if track else slot.title
        artist = track["artist"] if track else ""
        thumb = self.library.thumb_path(track["id"]) if track and track["has_thumb"] else None
        if phase == "next":
            status, time_left = "Starts in {time}", until_next or 0
        elif until_next is not None:
            status, time_left = "Next song in {time}", until_next
        else:
            status, time_left = "{time} left", length - position
        return overlay.Card(
            heading="Up next" if phase == "next" else "Now playing",
            title=title,
            artist=artist,
            status=status,
            time=clock(time_left),
            progress=position / length if length > 0 else None,
            thumb=thumb,
            opacity=round(opacity, 2),
        )

    def _current_card(self, settings: dict) -> overlay.Card | None:
        cur = self.current
        if not self.status.running or cur is None or not cur.managed:
            return None
        position = cur.slot.seek + self._elapsed(cur)
        # The next song starts where this one ends, which for a clip split at
        # midnight is the end of tomorrow's part.
        length = cur.slot.full_out or cur.slot.out
        nxt = self._next_slot()
        until_next = length - position if nxt else None
        phase = card_phase(position, until_next, settings)
        if phase is None:
            return None
        return self._card_for(phase[0], phase[1], position, until_next, length, cur.slot, nxt)

    def preview_card(self, settings: dict, phase: str) -> bytes:
        """The card as it would look now, for the settings dialog."""
        cur = self.current
        now_slot = cur.slot if cur is not None and cur.managed else None
        nxt = self._next_slot()
        sample = Slot(kind="auto", path="", seek=0, out=212, duration=212, title="Artist – Song title")
        card = self._card_for(
            phase, 1.0, 64.0, 148.0, 212.0, now_slot or sample, nxt or now_slot or sample
        )
        image = overlay.render(card, {**OVERLAY_DEFAULTS, **settings}, (self.s.stream_width, self.s.stream_height))
        buffer = io.BytesIO()
        image.save(buffer, format="PNG", compress_level=1)
        return buffer.getvalue()

    async def _update_overlay(self) -> None:
        if not self.status.running:
            return
        if not self._text_cleared:
            # The card replaced ffplayout's plain text overlay; clear any left over.
            try:
                await self.ff.send_text({
                    "channel_id": self.s.channel_id, "name": "localmusiclive", "text": "",
                    "use_filename": False, "font_family": "Inter", "font_weight": "normal",
                    "filename_regex": "", "position_x": "0", "position_y": "0",
                    "font_size": 24.0, "line_spacing": 4.0, "text_color": "#ffffff",
                    "text_opacity": 1.0, "background_enabled": False,
                    "background_color": "#000000", "background_opacity": 0.0,
                    "background_padding": 0, "opacity": 1.0, "scroll_direction": "none",
                    "scroll_speed": 100, "scroll_repeat": -1,
                    "fade_in_seconds": 0.0, "fade_out_seconds": 0.0,
                })
                self._text_cleared = True
            except FFplayoutError as exc:
                log.debug("Could not clear the text overlay: %s", exc)

        settings = self.overlay_settings()
        card = self._current_card(settings)
        key = None if card is None else (
            card.heading, card.title, card.artist, card.status, card.time, card.opacity,
            round(card.progress or 0, 3), str(card.thumb), json.dumps(settings, sort_keys=True),
        )
        if key == self._card_key:
            return
        self._card_key = key
        size = (self.s.stream_width, self.s.stream_height)
        target = self.s.media_root / CARD_FILE
        image = overlay.BLANK if card is None else await asyncio.to_thread(overlay.render, card, settings, size)
        await asyncio.to_thread(overlay.write, image, target)

    async def _overlay_loop(self) -> None:
        # Fades need ~10 updates a second; otherwise the card changes once a
        # second (countdown) and is only redrawn when something visible changes.
        while True:
            await asyncio.sleep(0.1)
            try:
                await self._update_overlay()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("Overlay update failed")

    # --- commands --------------------------------------------------------------

    async def _after_change(self) -> None:
        await self._safe_rebuild()
        await self._update_overlay()

    async def queue_add(self, track_id: int, play_next: bool = False, requested_by: str | None = None) -> int:
        async with self.lock:
            if not self.db.one("SELECT 1 FROM tracks WHERE id = ? AND missing = 0", track_id):
                raise EngineError("That track is not in the library")
            bound = self.db.one("SELECT MIN(position) AS lo, MAX(position) AS hi FROM queue")
            position = (bound["lo"] or 0) - 1 if play_next else (bound["hi"] or 0) + 1
            cur = self.db.run(
                "INSERT INTO queue (track_id, position, added_at, requested_by) VALUES (?, ?, ?, ?)",
                track_id, position, time.time(), requested_by,
            )
            await self._after_change()
        await self.broadcast()
        return cur.lastrowid

    async def queue_remove(self, queue_id: int) -> None:
        async with self.lock:
            self.db.run("DELETE FROM queue WHERE id = ?", queue_id)
            await self._after_change()
        await self.broadcast()

    async def queue_reorder(self, ids: list[int]) -> None:
        async with self.lock:
            existing = [row["id"] for row in self.db.all("SELECT id FROM queue ORDER BY position, id")]
            ordered = [i for i in ids if i in existing] + [i for i in existing if i not in ids]
            with self.db.tx():
                for position, queue_id in enumerate(ordered):
                    self.db.run("UPDATE queue SET position = ? WHERE id = ?", position, queue_id)
            await self._after_change()
        await self.broadcast()

    async def queue_clear(self) -> None:
        async with self.lock:
            self.db.run("DELETE FROM queue")
            await self._after_change()
        await self.broadcast()

    async def skip(self) -> None:
        async with self.lock:
            cur = self.current
            if not self.status.running or cur is None:
                raise EngineError("Nothing is playing")
            if not cur.managed:
                self._last_fresh = 0.0
                await self._fresh_start("skip")
            elif abs(cur.shift) > MAX_SHIFT:
                self._skip_pending = True
                await self.ff.playout("next")
            elif not await self._rebuild(skip=True):
                raise EngineError("This clip is about to end anyway")

    async def set_autoplay(self, values: dict) -> None:
        async with self.lock:
            settings = {**self.autoplay_settings(), **values}
            self.db.set_setting("autoplay", settings)
            # Re-pick with the new rules.
            self.db.run("DELETE FROM upcoming")
            await self._after_change()
        await self.broadcast()

    async def set_overlay(self, values: dict) -> None:
        async with self.lock:
            self.db.set_setting("overlay", {**self.overlay_settings(), **values})
            await self._update_overlay()
        await self.broadcast()

    async def playout(self, command: str) -> None:
        async with self.lock:
            if command == "start" and not self.status.running:
                self._last_fresh = 0.0
                if self._has_material():
                    await self._fresh_start("starting playout", reset=False)
            await self.ff.process(command)

    async def rescan(self) -> None:
        result = await self.library.scan()
        if result.changed:
            await self.hub.broadcast({"type": "library"})
            async with self.lock:
                await self._after_change()
            await self.broadcast()

    # --- background ------------------------------------------------------------

    async def _rescan_loop(self) -> None:
        while True:
            try:
                await self.rescan()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("Library scan failed")
            await asyncio.sleep(max(1, self.s.rescan_minutes) * 60)

    async def _housekeeping(self) -> None:
        tick = 0
        while True:
            await asyncio.sleep(10)
            tick += 1
            try:
                if self.status.running and self.current is not None and not self.current.managed:
                    async with self.lock:
                        if self._has_material():
                            await self._fresh_start("still on a placeholder")
                if tick % 360 == 0:
                    self._prune_plans()
                await self.broadcast()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("Housekeeping failed")

    def _prune_plans(self) -> None:
        cutoff = (Date.today() - timedelta(days=3)).isoformat()
        self.db.run("DELETE FROM plans WHERE date < ?", cutoff)
        self.db.run(
            "DELETE FROM plans WHERE id NOT IN ("
            " SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY date ORDER BY id DESC) AS n"
            " FROM plans) WHERE n <= 8)"
        )
        live = {row["id"] for row in self.db.all("SELECT id FROM plans")}
        self._plan_cache = {k: v for k, v in self._plan_cache.items() if k in live}

    # --- state for the browser -------------------------------------------------

    def snapshot(self) -> dict:
        now = time.time()
        cur = self.current

        queue_rows = self.db.all("SELECT * FROM queue ORDER BY position, id")
        history_rows = self.db.all("SELECT * FROM history ORDER BY id DESC LIMIT 30")
        following = list(self._following())
        ids = {r["track_id"] for r in queue_rows} | {r["track_id"] for r in history_rows}
        ids |= {s.track_id for s in following[:200] if s.track_id}
        if cur is not None and cur.slot is not None and cur.slot.track_id:
            ids.add(cur.slot.track_id)
        ids.discard(None)
        tracks = {}
        if ids:
            marks = ",".join("?" * len(ids))
            tracks = {r["id"]: r for r in self.db.all(f"SELECT * FROM tracks WHERE id IN ({marks})", *ids)}

        def tj(track_id: int | None) -> dict | None:
            row = tracks.get(track_id)
            return track_json(row) if row else None

        now_playing = None
        remaining = 0.0
        if cur is not None and cur.managed:
            elapsed = self._elapsed(cur)
            remaining = max(cur.slot.length - elapsed, 0.0)
            now_playing = {
                "track": tj(cur.slot.track_id),
                "kind": cur.slot.kind,
                "position": cur.slot.seek + elapsed,
                "length": cur.slot.full_out or cur.slot.duration,
                "remaining": remaining,
            }
        elif cur is not None:
            now_playing = {"track": None, "kind": "placeholder", "position": 0, "length": 0, "remaining": 0}

        eta: dict[tuple[str, int], float] = {}
        upcoming = []
        t = remaining
        for slot in following:
            if slot.kind != "gap" and slot.part == 0:
                if slot.queue_id is not None:
                    eta[("queue", slot.queue_id)] = t
                elif slot.auto_id is not None and len(upcoming) < 10:
                    upcoming.append({"id": slot.auto_id, "track": tj(slot.track_id), "eta": t})
            t += slot.length
            if len(upcoming) >= 10 and len(eta) >= len(queue_rows):
                break

        nxt = self._next_slot()
        history = [
            {
                "id": r["id"],
                "track": tj(r["track_id"]),
                "kind": r["kind"],
                "started_at": r["started_at"],
                "ended_at": r["ended_at"],
                "skipped": bool(r["skipped"]),
                "requested_by": r["requested_by"],
            }
            for r in history_rows
            if cur is None or r["id"] != cur.history_id
        ]

        return {
            "server_time": now,
            "status": {
                "ffplayout": self.status.ffplayout,
                "running": self.status.running,
                "mode": self.status.mode,
                "output": self.status.output,
                "error": self.status.error,
                "warnings": self.status.warnings,
                "scanning": self.library.scanning,
                "resolution": [self.s.stream_width, self.s.stream_height],
                "library_size": self.db.one("SELECT COUNT(*) AS n FROM tracks WHERE missing = 0")["n"],
            },
            "now": now_playing,
            "next": {"track": tj(nxt.track_id), "kind": nxt.kind} if nxt else None,
            "queue": [
                {
                    "id": r["id"],
                    "track": tj(r["track_id"]),
                    "eta": eta.get(("queue", r["id"])),
                    "requested_by": r["requested_by"],
                }
                for r in queue_rows
            ],
            "upcoming": upcoming,
            "history": history,
            "autoplay": self.autoplay_settings(),
            "overlay": self.overlay_settings(),
            "downloads": self.downloads.state() if self.downloads else [],
        }

    async def broadcast(self) -> None:
        if self.hub.count:
            await self.hub.broadcast({"type": "state", "data": self.snapshot()})
