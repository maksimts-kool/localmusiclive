import asyncio
import json
import logging
from contextlib import asynccontextmanager
from typing import Literal

from fastapi import FastAPI, HTTPException, Query, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse, Response
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

from .categories import categories_of, clean_categories
from .db import DB
from .engine import OVERLAY_POSITIONS, Engine, EngineError, track_json
from .ffplayout import FFplayout, FFplayoutError
from .library import Library
from .settings import settings
from .youtube import YouTube

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s")
log = logging.getLogger("panel")
logging.getLogger("httpx").setLevel(logging.WARNING)


class Hub:
    """Browser WebSocket connections."""

    def __init__(self) -> None:
        self.sockets: set[WebSocket] = set()

    @property
    def count(self) -> int:
        return len(self.sockets)

    async def broadcast(self, message: dict) -> None:
        async def send(socket: WebSocket) -> None:
            try:
                await asyncio.wait_for(socket.send_json(message), 5)
            except Exception:
                self.sockets.discard(socket)

        await asyncio.gather(*(send(s) for s in list(self.sockets)))


db = DB(settings.db_path)
ff = FFplayout(
    settings.ffplayout_url, settings.ffplayout_user, settings.ffplayout_password, settings.channel_id
)
library = Library(db, settings)
hub = Hub()
engine = Engine(settings, db, ff, library, hub)
youtube = YouTube(settings, db, library, engine, hub)
engine.downloads = youtube


@asynccontextmanager
async def lifespan(app: FastAPI):
    if not settings.ffplayout_password:
        log.error("FFPLAYOUT_PASSWORD is not set")
    await engine.start()
    # Keep a reference so the task isn't garbage-collected.
    app.state.youtube_setup = asyncio.create_task(youtube.start())
    yield
    await engine.stop()
    await ff.close()


app = FastAPI(title="localmusiclive panel", lifespan=lifespan)


async def run(action):
    try:
        return await action
    except EngineError as exc:
        raise HTTPException(409, str(exc)) from exc
    except FFplayoutError as exc:
        raise HTTPException(502, str(exc)) from exc


# --- state -------------------------------------------------------------------


@app.get("/api/state")
def state() -> dict:
    return engine.snapshot()


@app.get("/api/health")
def health() -> dict:
    return {"ok": True, "ffplayout": engine.status.ffplayout, "running": engine.status.running}


@app.websocket("/ws")
async def websocket(socket: WebSocket) -> None:
    await socket.accept()
    hub.sockets.add(socket)
    try:
        await socket.send_json({"type": "state", "data": engine.snapshot()})
        while True:
            await socket.receive_text()
    except WebSocketDisconnect:
        pass
    finally:
        hub.sockets.discard(socket)


# --- library -----------------------------------------------------------------

SORTS = {
    "artist": "artist COLLATE NOCASE, title COLLATE NOCASE",
    "title": "title COLLATE NOCASE, artist COLLATE NOCASE",
    "added": "added_at DESC",
    "played": "play_count DESC, last_played_at DESC",
    "recent": "last_played_at IS NULL, last_played_at DESC",
}


UNCATEGORIZED = "__none__"


def library_filter(q: str, folder: str, category: str) -> tuple[str, list]:
    where, args = ["missing = 0"], []
    for word in q.split():
        like = "%" + word.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
        where.append(
            "(artist LIKE ? ESCAPE '\\' OR title LIKE ? ESCAPE '\\'"
            " OR album LIKE ? ESCAPE '\\' OR path LIKE ? ESCAPE '\\' OR categories LIKE ? ESCAPE '\\')"
        )
        args += [like] * 5
    if folder := folder.strip("/"):
        where.append("(folder = ? OR folder LIKE ?)")
        args += [folder, folder + "/%"]
    if category == UNCATEGORIZED:
        where.append("json_array_length(categories) = 0")
    elif category:
        where.append("EXISTS (SELECT 1 FROM json_each(tracks.categories) WHERE value = ? COLLATE NOCASE)")
        args.append(category)
    return " AND ".join(where), args


@app.get("/api/library")
def list_library(
    q: str = "",
    folder: str = "",
    category: str = "",
    sort: Literal["artist", "title", "added", "played", "recent"] = "artist",
    limit: int = Query(100, ge=1, le=500),
    offset: int = Query(0, ge=0),
) -> dict:
    clause, args = library_filter(q, folder, category)
    total = db.one(f"SELECT COUNT(*) AS n FROM tracks WHERE {clause}", *args)["n"]
    rows = db.all(
        f"SELECT * FROM tracks WHERE {clause} ORDER BY {SORTS[sort]} LIMIT ? OFFSET ?",
        *args, limit, offset,
    )
    return {"total": total, "items": [track_json(r) for r in rows]}


@app.get("/api/library/folders")
def list_folders() -> list[str]:
    folders = set()
    for row in db.all("SELECT DISTINCT folder FROM tracks WHERE missing = 0 AND folder != ''"):
        parts = row["folder"].split("/")
        folders.update("/".join(parts[: i + 1]) for i in range(len(parts)))
    return sorted(folders, key=str.lower)


def category_counts() -> dict[str, int]:
    counts: dict[str, int] = {}
    names: dict[str, str] = {}
    for row in db.all("SELECT categories FROM tracks WHERE missing = 0 AND categories != '[]'"):
        for name in categories_of(row):
            key = name.casefold()
            names.setdefault(key, name)
            counts[key] = counts.get(key, 0) + 1
    return {names[k]: n for k, n in counts.items()}


@app.get("/api/categories")
def list_categories() -> list[dict]:
    counts = category_counts()
    return [{"name": n, "count": counts[n]} for n in sorted(counts, key=str.casefold)]


class TrackCategories(BaseModel):
    categories: list[str] = Field(max_length=50)


@app.put("/api/tracks/{track_id}/categories")
async def put_track_categories(track_id: int, body: TrackCategories) -> dict:
    row = db.one("SELECT * FROM tracks WHERE id = ?", track_id)
    if row is None:
        raise HTTPException(404, "That track is not in the library")
    names = clean_categories(body.categories, category_counts())
    db.run("UPDATE tracks SET categories = ? WHERE id = ?", json.dumps(names, ensure_ascii=False), track_id)
    await engine.categories_changed(reload_library=False)
    return {"categories": names}


class BulkCategories(BaseModel):
    """Add or remove one category on every song matching the library filter."""

    q: str = ""
    folder: str = ""
    category: str = ""
    add: str | None = None
    remove: str | None = None


@app.post("/api/categories/bulk")
async def bulk_categories(body: BulkCategories) -> dict:
    add = clean_categories([body.add or ""], category_counts())
    remove = (body.remove or "").strip().casefold()
    if not add and not remove:
        raise HTTPException(422, "Name a category to add or remove")
    clause, args = library_filter(body.q, body.folder, body.category)
    changed = 0
    with db.tx():
        for row in db.all(f"SELECT id, categories FROM tracks WHERE {clause}", *args):
            names = categories_of(row)
            updated = [n for n in names if n.casefold() != remove] if remove else names
            if add and all(n.casefold() != add[0].casefold() for n in updated):
                updated = clean_categories(updated + add)
            if updated != names:
                db.run(
                    "UPDATE tracks SET categories = ? WHERE id = ?",
                    json.dumps(updated, ensure_ascii=False), row["id"],
                )
                changed += 1
    if changed:
        await engine.categories_changed()
    return {"changed": changed}


@app.post("/api/library/rescan", status_code=202)
async def rescan() -> dict:
    if library.scanning:
        return {"started": False}
    asyncio.create_task(engine.rescan())
    return {"started": True}


@app.delete("/api/tracks/{track_id}")
async def delete_track(track_id: int) -> dict:
    """Deletes the song's video file from the media folder."""
    return {"removed": await run(engine.remove_track(track_id))}


@app.get("/api/thumbs/{track_id}.jpg")
def thumb(track_id: int) -> FileResponse:
    path = library.thumb_path(track_id)
    if not path.exists():
        raise HTTPException(404)
    return FileResponse(path, media_type="image/jpeg", headers={"Cache-Control": "max-age=604800"})


# --- queue -------------------------------------------------------------------


class QueueAdd(BaseModel):
    track_id: int
    play_next: bool = False


class QueueOrder(BaseModel):
    ids: list[int]


@app.post("/api/queue", status_code=201)
async def queue_add(body: QueueAdd) -> dict:
    return {"id": await run(engine.queue_add(body.track_id, body.play_next))}


@app.put("/api/queue")
async def queue_reorder(body: QueueOrder) -> dict:
    await run(engine.queue_reorder(body.ids))
    return {"ok": True}


@app.delete("/api/queue/{queue_id}")
async def queue_remove(queue_id: int) -> dict:
    await run(engine.queue_remove(queue_id))
    return {"ok": True}


@app.delete("/api/queue")
async def queue_clear() -> dict:
    await run(engine.queue_clear())
    return {"ok": True}


@app.post("/api/skip")
async def skip() -> dict:
    await run(engine.skip())
    return {"ok": True}


# --- YouTube -----------------------------------------------------------------


@app.get("/api/youtube/search")
async def youtube_search(q: str = Query(..., min_length=1, max_length=200)) -> list[dict]:
    try:
        return await youtube.search(q)
    except RuntimeError as exc:
        raise HTTPException(502, f"YouTube search failed: {exc}") from exc


@app.get("/api/youtube/suggestions")
async def youtube_suggestions(refresh: bool = False) -> dict:
    return await youtube.suggestions(refresh)


class Download(BaseModel):
    video_id: str = Field(pattern=r"^[A-Za-z0-9_-]{11}$")
    then: Literal["none", "queue", "next"] = "queue"
    title: str | None = Field(None, max_length=300)


@app.post("/api/youtube/downloads", status_code=202)
async def youtube_download(body: Download) -> dict:
    job = await run(youtube.download(body.video_id, body.then, body.title))
    return job.public()


@app.delete("/api/youtube/downloads/{job_id}")
async def youtube_dismiss(job_id: int) -> dict:
    await youtube.dismiss(job_id)
    return {"ok": True}


# --- playout and settings ----------------------------------------------------


@app.post("/api/playout/{command}")
async def playout(command: Literal["start", "stop", "restart"]) -> dict:
    await run(engine.playout(command))
    return {"ok": True}


class AutoplaySettings(BaseModel):
    enabled: bool | None = None
    mode: Literal["shuffle", "least_played", "in_order"] | None = None
    folder: str | None = None
    categories: list[str] | None = Field(None, max_length=50)
    avoid_repeat: int | None = Field(None, ge=0, le=500)


Color = Field(None, pattern=r"^#[0-9a-fA-F]{6}$")


class OverlaySettings(BaseModel):
    enabled: bool | None = None
    mode: Literal["start_end", "always"] | None = None
    start_seconds: int | None = Field(None, ge=0, le=600)
    end_seconds: int | None = Field(None, ge=0, le=600)
    show_next: bool | None = None
    position: Literal[OVERLAY_POSITIONS] | None = None  # type: ignore[valid-type]
    margin: int | None = Field(None, ge=0, le=400)
    scale: int | None = Field(None, ge=50, le=250)
    accent_color: str | None = Color
    background_color: str | None = Color
    background_opacity: float | None = Field(None, ge=0, le=1)
    stroke: bool | None = None
    stroke_color: str | None = Color
    stroke_opacity: float | None = Field(None, ge=0, le=1)
    stroke_width: float | None = Field(None, ge=0.5, le=8)
    radius: int | None = Field(None, ge=0, le=60)
    shadow: bool | None = None
    show_thumbnail: bool | None = None
    show_progress: bool | None = None
    progress_line: bool | None = None
    line_height: int | None = Field(None, ge=1, le=20)
    queue_card: bool | None = None
    queue_rows: int | None = Field(None, ge=1, le=8)
    queue_bright_seconds: int | None = Field(None, ge=0, le=600)
    queue_dim_opacity: float | None = Field(None, ge=0, le=1)


class OverlayPreview(OverlaySettings):
    card: Literal["now", "next", "line"] = "now"


@app.put("/api/settings/autoplay")
async def put_autoplay(body: AutoplaySettings) -> dict:
    await run(engine.set_autoplay(body.model_dump(exclude_none=True)))
    return engine.autoplay_settings()


@app.put("/api/settings/overlay")
async def put_overlay(body: OverlaySettings) -> dict:
    await run(engine.set_overlay(body.model_dump(exclude_none=True)))
    return engine.overlay_settings()


@app.post("/api/overlay/preview")
async def overlay_preview(body: OverlayPreview) -> Response:
    settings_ = {**engine.overlay_settings(), **body.model_dump(exclude_none=True, exclude={"card"})}
    png = await asyncio.to_thread(engine.preview_card, settings_, body.card)
    return Response(png, media_type="image/png", headers={"Cache-Control": "no-store"})


@app.get("/api/history")
def history(limit: int = Query(100, ge=1, le=1000), offset: int = Query(0, ge=0)) -> list[dict]:
    rows = db.all(
        "SELECT history.*, tracks.artist, tracks.title FROM history"
        " LEFT JOIN tracks ON tracks.id = history.track_id"
        " ORDER BY history.id DESC LIMIT ? OFFSET ?",
        limit, offset,
    )
    return [dict(r) for r in rows]


if settings.static_dir.is_dir():
    app.mount("/", StaticFiles(directory=settings.static_dir, html=True), name="static")
