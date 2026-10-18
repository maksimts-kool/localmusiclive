import asyncio
import logging
from contextlib import asynccontextmanager
from typing import Literal

from fastapi import FastAPI, HTTPException, Query, WebSocket, WebSocketDisconnect
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field

from .db import DB
from .engine import OVERLAY_POSITIONS, Engine, EngineError, track_json
from .ffplayout import FFplayout, FFplayoutError
from .library import Library
from .settings import settings

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


@asynccontextmanager
async def lifespan(_: FastAPI):
    if not settings.ffplayout_password:
        log.error("FFPLAYOUT_PASSWORD is not set")
    await engine.start()
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


@app.get("/api/library")
def list_library(
    q: str = "",
    folder: str = "",
    sort: Literal["artist", "title", "added", "played", "recent"] = "artist",
    limit: int = Query(100, ge=1, le=500),
    offset: int = Query(0, ge=0),
) -> dict:
    where, args = ["missing = 0"], []
    for word in q.split():
        like = "%" + word.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_") + "%"
        where.append(
            "(artist LIKE ? ESCAPE '\\' OR title LIKE ? ESCAPE '\\'"
            " OR album LIKE ? ESCAPE '\\' OR path LIKE ? ESCAPE '\\')"
        )
        args += [like] * 4
    if folder := folder.strip("/"):
        where.append("(folder = ? OR folder LIKE ?)")
        args += [folder, folder + "/%"]
    clause = " AND ".join(where)
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


@app.post("/api/library/rescan", status_code=202)
async def rescan() -> dict:
    if library.scanning:
        return {"started": False}
    asyncio.create_task(engine.rescan())
    return {"started": True}


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


# --- playout and settings ----------------------------------------------------


@app.post("/api/playout/{command}")
async def playout(command: Literal["start", "stop", "restart"]) -> dict:
    await run(engine.playout(command))
    return {"ok": True}


class AutoplaySettings(BaseModel):
    enabled: bool | None = None
    mode: Literal["shuffle", "least_played", "in_order"] | None = None
    folder: str | None = None
    avoid_repeat: int | None = Field(None, ge=0, le=500)


Color = Field(None, pattern=r"^#[0-9a-fA-F]{6}$")


class OverlaySettings(BaseModel):
    enabled: bool | None = None
    show_next: bool | None = None
    position: Literal[tuple(OVERLAY_POSITIONS)] | None = None  # type: ignore[valid-type]
    margin: int | None = Field(None, ge=0, le=400)
    font_family: str | None = Field(None, min_length=1, max_length=80)
    font_size: int | None = Field(None, ge=10, le=120)
    text_color: str | None = Color
    background: bool | None = None
    background_color: str | None = Color
    background_opacity: float | None = Field(None, ge=0, le=1)
    padding: int | None = Field(None, ge=0, le=80)


@app.put("/api/settings/autoplay")
async def put_autoplay(body: AutoplaySettings) -> dict:
    await run(engine.set_autoplay(body.model_dump(exclude_none=True)))
    return engine.autoplay_settings()


@app.put("/api/settings/overlay")
async def put_overlay(body: OverlaySettings) -> dict:
    await run(engine.set_overlay(body.model_dump(exclude_none=True)))
    return engine.overlay_settings()


@app.get("/api/fonts")
async def fonts() -> list[str]:
    try:
        return await ff.request("GET", "/api/text/fonts")
    except FFplayoutError:
        return ["DejaVu Sans"]


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
