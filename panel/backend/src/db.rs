use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{Map, Value};

const SCHEMA: &str = "
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
    level INTEGER NOT NULL DEFAULT 1    -- 2 Отлично, 1 Хорошо, 0 Плохо
);

CREATE TABLE IF NOT EXISTS queue (
    id INTEGER PRIMARY KEY,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    added_at REAL NOT NULL,
    requested_by TEXT
);

-- Autoplay picks, planned ahead so \"Next\" stays stable between rebuilds.
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
";

/// One SQLite connection behind a mutex. Lock it only for short, synchronous
/// work and never across an `.await`.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        // Older libraries: songs start out Хорошо. (A leftover `categories` column is unused.)
        let has_level: bool =
            conn.prepare("SELECT 1 FROM pragma_table_info('tracks') WHERE name = 'level'")?.exists([])?;
        if !has_level {
            conn.execute("ALTER TABLE tracks ADD COLUMN level INTEGER NOT NULL DEFAULT 1", [])?;
        }
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get_setting(&self, key: &str) -> Option<Value> {
        setting(&self.lock(), key)
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        self.lock().execute(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }
}

/// A stored setting, on a connection that is already locked.
pub fn setting(conn: &Connection, key: &str) -> Option<Value> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// Runs `f` in a transaction: committed if it returns Ok, rolled back otherwise.
pub fn tx<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN")?;
    match f(conn) {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: i64,
    pub path: String,
    pub folder: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration: f64,
    pub size: i64,
    pub mtime: f64,
    pub has_thumb: bool,
    pub missing: bool,
    pub play_count: i64,
    pub last_played_at: Option<f64>,
    pub level: i64,
}

impl Track {
    pub fn from_row(row: &Row) -> rusqlite::Result<Track> {
        Ok(Track {
            id: row.get("id")?,
            path: row.get("path")?,
            folder: row.get("folder")?,
            artist: row.get("artist")?,
            title: row.get("title")?,
            album: row.get("album")?,
            duration: row.get("duration")?,
            size: row.get("size")?,
            mtime: row.get("mtime")?,
            has_thumb: row.get::<_, i64>("has_thumb")? != 0,
            missing: row.get::<_, i64>("missing")? != 0,
            play_count: row.get("play_count")?,
            last_played_at: row.get("last_played_at")?,
            level: row.get("level")?,
        })
    }

    /// "Artist – Title", or just the title.
    pub fn label(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} – {}", self.artist, self.title)
        }
    }
}

pub fn tracks(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(args, Track::from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn track(conn: &Connection, id: i64) -> Result<Option<Track>> {
    Ok(conn.query_row("SELECT * FROM tracks WHERE id = ?", [id], Track::from_row).optional()?)
}

/// A row as a JSON object keyed by column name.
pub fn row_json(row: &Row) -> rusqlite::Result<Value> {
    let mut map = Map::new();
    let stmt = row.as_ref();
    for i in 0..stmt.column_count() {
        let name = stmt.column_name(i)?.to_string();
        let value = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::from(n),
            ValueRef::Real(f) => Value::from(f),
            ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(_) => Value::Null,
        };
        map.insert(name, value);
    }
    Ok(Value::Object(map))
}

/// "?,?,?" for an IN (...) list.
pub fn marks(n: usize) -> String {
    vec!["?"; n.max(1)].join(",")
}
