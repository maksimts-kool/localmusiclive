import json
import sqlite3
from contextlib import contextmanager
from pathlib import Path
from typing import Any

SCHEMA = """
CREATE TABLE IF NOT EXISTS tracks (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,          -- relative to the media root
    folder TEXT NOT NULL DEFAULT '',
    artist TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    album TEXT NOT NULL DEFAULT '',
    duration REAL NOT NULL,
    width INTEGER,
    height INTEGER,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    has_thumb INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,
    added_at REAL NOT NULL,
    play_count INTEGER NOT NULL DEFAULT 0,
    last_played_at REAL,
    categories TEXT NOT NULL DEFAULT '[]'  -- JSON list of names
);

CREATE TABLE IF NOT EXISTS queue (
    id INTEGER PRIMARY KEY,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    added_at REAL NOT NULL,
    requested_by TEXT
);

-- Autoplay picks, planned ahead so "Next" stays stable between rebuilds.
CREATE TABLE IF NOT EXISTS upcoming (
    id INTEGER PRIMARY KEY,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS history (
    id INTEGER PRIMARY KEY,
    track_id INTEGER REFERENCES tracks(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    started_at REAL NOT NULL,
    ended_at REAL,
    skipped INTEGER NOT NULL DEFAULT 0,
    requested_by TEXT
);

-- Every playlist version written to ffplayout, used to recognise what is on air.
CREATE TABLE IF NOT EXISTS plans (
    id INTEGER PRIMARY KEY,
    date TEXT NOT NULL,
    created_at REAL NOT NULL,
    slots TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_queue_position ON queue(position);
CREATE INDEX IF NOT EXISTS idx_upcoming_position ON upcoming(position);
CREATE INDEX IF NOT EXISTS idx_history_started ON history(started_at);
CREATE INDEX IF NOT EXISTS idx_plans_date ON plans(date, id);
"""


class DB:
    """Thin wrapper over one SQLite connection, used from the event loop only."""

    def __init__(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(path, isolation_level=None, check_same_thread=False)
        self.conn.row_factory = sqlite3.Row
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA foreign_keys=ON")
        self.conn.executescript(SCHEMA)
        self._migrate()

    def _migrate(self) -> None:
        columns = {row["name"] for row in self.all("PRAGMA table_info(tracks)")}
        if "categories" not in columns:
            self.run("ALTER TABLE tracks ADD COLUMN categories TEXT NOT NULL DEFAULT '[]'")

    def all(self, sql: str, *args: Any) -> list[sqlite3.Row]:
        return self.conn.execute(sql, args).fetchall()

    def one(self, sql: str, *args: Any) -> sqlite3.Row | None:
        return self.conn.execute(sql, args).fetchone()

    def run(self, sql: str, *args: Any) -> sqlite3.Cursor:
        return self.conn.execute(sql, args)

    @contextmanager
    def tx(self):
        self.conn.execute("BEGIN")
        try:
            yield
        except BaseException:
            self.conn.execute("ROLLBACK")
            raise
        self.conn.execute("COMMIT")

    def get_setting(self, key: str, default: Any = None) -> Any:
        row = self.one("SELECT value FROM settings WHERE key = ?", key)
        return json.loads(row["value"]) if row else default

    def set_setting(self, key: str, value: Any) -> None:
        self.run(
            "INSERT INTO settings (key, value) VALUES (?, ?) "
            "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            key,
            json.dumps(value),
        )
