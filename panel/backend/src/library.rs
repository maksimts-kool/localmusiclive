//! Indexes the clips in the media folder with ffprobe and makes thumbnails.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::future::join_all;
use rusqlite::params;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

use crate::db::{self, Db, Track};
use crate::settings::{Settings, IGNORED_FOLDERS, VIDEO_EXTENSIONS};
use crate::util::{now, round_to, run};

#[derive(Default, Debug, Clone, Copy)]
pub struct ScanResult {
    pub added: usize,
    pub updated: usize,
    pub missing: usize,
    pub failed: usize,
}

impl ScanResult {
    pub fn changed(&self) -> bool {
        self.added + self.updated + self.missing > 0
    }
}

/// Every video file below `root` (relative path → size, mtime).
fn walk(root: &Path) -> HashMap<String, (i64, f64)> {
    let mut found = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            // Like os.walk: symlinked folders are listed but not entered.
            let meta = if kind.is_symlink() { std::fs::metadata(&path) } else { entry.metadata() };
            let Ok(meta) = meta else { continue };
            if meta.is_dir() {
                if !kind.is_symlink() && !(rel_dir.is_empty() && IGNORED_FOLDERS.contains(&name.as_str())) {
                    stack.push((path, rel));
                }
                continue;
            }
            let lower = name.to_lowercase();
            if !VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            found.insert(rel, (meta.len() as i64, mtime));
        }
    }
    found
}

fn tag(tags: &HashMap<String, String>, names: &[&str]) -> String {
    names
        .iter()
        .filter_map(|name| tags.get(*name))
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration: f64,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Names and length of a clip from ffprobe's JSON; None if it has no video.
pub fn describe(rel_path: &str, probe: &Value) -> Option<Info> {
    let empty = Value::Null;
    let fmt = probe.get("format").unwrap_or(&empty);
    let streams = probe.get("streams").and_then(Value::as_array).cloned().unwrap_or_default();
    let video = streams.iter().find(|s| {
        s.get("codec_type").and_then(Value::as_str) == Some("video")
            && !s.pointer("/disposition/attached_pic").map(crate::ffplayout::truthy).unwrap_or(false)
    })?;
    let duration = as_f64(fmt.get("duration"))
        .filter(|d| *d != 0.0)
        .or_else(|| as_f64(video.get("duration")))
        .unwrap_or(0.0);
    if duration < 1.0 {
        return None;
    }

    // Container tags win over stream tags, earlier streams over later ones.
    let mut tags: HashMap<String, String> = HashMap::new();
    let sources = std::iter::once(fmt.get("tags")).chain(streams.iter().map(|s| s.get("tags")));
    for source in sources.flatten() {
        if let Some(map) = source.as_object() {
            for (key, value) in map {
                let text = match value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                tags.entry(key.to_lowercase()).or_insert(text);
            }
        }
    }

    let mut artist = tag(&tags, &["artist", "album_artist", "performer"]);
    let mut title = tag(&tags, &["title"]);
    // Downloads are named "Artist - Title [video id]".
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let stem = match file.rfind('.') {
        Some(dot) if dot > 0 => &file[..dot],
        _ => file,
    };
    let stem = crate::youtube::strip_id_suffix(stem);
    if title.is_empty() {
        if let Some((guess_artist, guess_title)) = stem.split_once(" - ") {
            title = guess_title.trim().to_string();
            if artist.is_empty() {
                artist = guess_artist.trim().to_string();
            }
        } else {
            title = stem.to_string();
        }
    }

    Some(Info {
        artist,
        title,
        album: tag(&tags, &["album"]),
        duration: round_to(duration, 3),
        width: video.get("width").and_then(Value::as_i64),
        height: video.get("height").and_then(Value::as_i64),
    })
}

enum Indexed {
    Added(i64),
    Updated(i64),
    Failed,
}

pub struct Library {
    db: Arc<Db>,
    s: Arc<Settings>,
    probe_sem: Semaphore,
    thumb_sem: Semaphore,
    scan_lock: Mutex<()>,
    scanning: AtomicBool,
}

impl Library {
    pub fn new(db: Arc<Db>, s: Arc<Settings>) -> Library {
        let _ = std::fs::create_dir_all(s.thumbs_dir());
        Library {
            db,
            s,
            probe_sem: Semaphore::new(4),
            thumb_sem: Semaphore::new(2),
            scan_lock: Mutex::new(()),
            scanning: AtomicBool::new(false),
        }
    }

    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    pub fn abs_path(&self, rel_path: &str) -> String {
        self.s.media_root.join(rel_path).to_string_lossy().into_owned()
    }

    pub fn thumb_path(&self, track_id: i64) -> PathBuf {
        self.s.thumbs_dir().join(format!("{track_id}.jpg"))
    }

    async fn probe(&self, rel_path: &str) -> Option<Info> {
        let out = {
            let _permit = self.probe_sem.acquire().await.ok()?;
            let args: Vec<String> = ["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]
                .iter()
                .map(|s| s.to_string())
                .chain([self.abs_path(rel_path)])
                .collect();
            run("ffprobe", &args, Duration::from_secs(60), false).await
        };
        if out.code != 0 {
            return None;
        }
        let probe: Value = serde_json::from_slice(&out.stdout).ok()?;
        describe(rel_path, &probe)
    }

    async fn make_thumb(&self, track_id: i64, rel_path: &str, duration: f64) -> bool {
        let target = self.thumb_path(track_id);
        let at = format!("{:.2}", (duration * 0.3).min(60.0));
        let _permit = self.thumb_sem.acquire().await;
        let args: Vec<String> = [
            "-v", "error", "-y", "-ss", &at, "-i", &self.abs_path(rel_path),
            "-frames:v", "1", "-vf", "scale=320:-2", "-q:v", "5", &target.to_string_lossy(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = run("ffmpeg", &args, Duration::from_secs(60), false).await;
        out.code == 0 && target.exists()
    }

    async fn index(&self, path: &str, size: i64, mtime: f64, row: Option<&Track>) -> Result<Indexed> {
        let Some(info) = self.probe(path).await else { return Ok(Indexed::Failed) };
        let folder = match path.rfind('/') {
            Some(slash) => &path[..slash],
            None => "",
        };
        let outcome = {
            let conn = self.db.lock();
            match row {
                None => {
                    conn.execute(
                        "INSERT INTO tracks (path, folder, artist, title, album, duration, width, height,\
                         size, mtime, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                        params![
                            path, folder, info.artist, info.title, info.album, info.duration,
                            info.width, info.height, size, mtime, now()
                        ],
                    )?;
                    Indexed::Added(conn.last_insert_rowid())
                }
                Some(row) => {
                    conn.execute(
                        "UPDATE tracks SET artist = ?, title = ?, album = ?, duration = ?, width = ?,\
                         height = ?, size = ?, mtime = ?, missing = 0 WHERE id = ?",
                        params![
                            info.artist, info.title, info.album, info.duration, info.width,
                            info.height, size, mtime, row.id
                        ],
                    )?;
                    Indexed::Updated(row.id)
                }
            }
        };
        let track_id = match outcome {
            Indexed::Added(id) | Indexed::Updated(id) => id,
            Indexed::Failed => unreachable!(),
        };
        let has_thumb = self.make_thumb(track_id, path, info.duration).await;
        self.db
            .lock()
            .execute("UPDATE tracks SET has_thumb = ? WHERE id = ?", params![has_thumb as i64, track_id])?;
        Ok(outcome)
    }

    /// Index one file now (e.g. a finished download) with the given names.
    pub async fn add_file(&self, rel_path: &str, artist: &str, title: &str) -> Result<Option<i64>> {
        let _guard = self.scan_lock.lock().await;
        let meta = std::fs::metadata(self.s.media_root.join(rel_path))?;
        let mtime = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let row = {
            let conn = self.db.lock();
            db::tracks(&conn, "SELECT * FROM tracks WHERE path = ?", [rel_path])?.into_iter().next()
        };
        let track_id = match self.index(rel_path, meta.len() as i64, mtime, row.as_ref()).await? {
            Indexed::Added(id) | Indexed::Updated(id) => id,
            Indexed::Failed => return Ok(None),
        };
        // Kept until the file itself changes; scans only re-read changed files.
        self.db.lock().execute(
            "UPDATE tracks SET artist = ?, title = ? WHERE id = ?",
            params![artist, title, track_id],
        )?;
        Ok(Some(track_id))
    }

    pub async fn scan(&self) -> Result<ScanResult> {
        let _guard = self.scan_lock.lock().await;
        self.scanning.store(true, Ordering::Relaxed);
        let result = self.scan_locked().await;
        self.scanning.store(false, Ordering::Relaxed);
        result
    }

    async fn scan_locked(&self) -> Result<ScanResult> {
        let mut result = ScanResult::default();
        let root = self.s.media_root.clone();
        if !root.is_dir() {
            tracing::warn!("Media folder {} does not exist", root.display());
            return Ok(result);
        }

        let found = tokio::task::spawn_blocking(move || walk(&root)).await?;
        let known: HashMap<String, Track> = {
            let conn = self.db.lock();
            db::tracks(&conn, "SELECT * FROM tracks", [])?.into_iter().map(|t| (t.path.clone(), t)).collect()
        };

        let gone: Vec<i64> =
            known.iter().filter(|(path, t)| !found.contains_key(*path) && !t.missing).map(|(_, t)| t.id).collect();
        if !gone.is_empty() {
            let sql = format!("UPDATE tracks SET missing = 1 WHERE id IN ({})", db::marks(gone.len()));
            self.db.lock().execute(&sql, rusqlite::params_from_iter(gone.iter()))?;
            result.missing = gone.len();
        }

        let todo: Vec<(&String, i64, f64)> = found
            .iter()
            .filter(|(path, (size, mtime))| match known.get(*path) {
                None => true,
                Some(t) => t.size != *size || (t.mtime - mtime).abs() > 0.001 || t.missing,
            })
            .map(|(path, (size, mtime))| (path, *size, *mtime))
            .collect();

        let outcomes = join_all(todo.iter().map(|(path, size, mtime)| self.index(path, *size, *mtime, known.get(*path)))).await;
        for outcome in outcomes {
            match outcome {
                Ok(Indexed::Added(_)) => result.added += 1,
                Ok(Indexed::Updated(_)) => result.updated += 1,
                Ok(Indexed::Failed) => result.failed += 1,
                Err(err) => {
                    tracing::warn!("Could not index a clip: {err:#}");
                    result.failed += 1;
                }
            }
        }
        if result.changed() || result.failed > 0 {
            tracing::info!(
                "Library scan: {} added, {} updated, {} missing, {} unreadable",
                result.added, result.updated, result.missing, result.failed
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_from_tags_or_file_name() {
        let probe = json!({
            "format": {"duration": "212.5", "tags": {"ARTIST": "Daft Punk", "title": "Get Lucky"}},
            "streams": [{"codec_type": "video", "width": 1920, "height": 1080}],
        });
        let info = describe("x/whatever.mp4", &probe).unwrap();
        assert_eq!((info.artist.as_str(), info.title.as_str()), ("Daft Punk", "Get Lucky"));
        assert_eq!((info.duration, info.width), (212.5, Some(1920)));

        let probe = json!({"format": {"duration": "200"}, "streams": [{"codec_type": "video"}]});
        let info = describe("youtube/Queen - Radio Ga Ga [azdwsXLmrHE].mp4", &probe).unwrap();
        assert_eq!((info.artist.as_str(), info.title.as_str()), ("Queen", "Radio Ga Ga"));
    }

    #[test]
    fn skips_audio_only_and_tiny_clips() {
        let cover = json!({"format": {"duration": "200"},
            "streams": [{"codec_type": "video", "disposition": {"attached_pic": 1}}]});
        assert!(describe("a.mp4", &cover).is_none());
        let tiny = json!({"format": {"duration": "0.5"}, "streams": [{"codec_type": "video"}]});
        assert!(describe("a.mp4", &tiny).is_none());
    }

    #[test]
    fn walk_skips_hidden_and_ignored_folders() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["a.mp4", "sub/b.MKV", "sub/notes.txt", ".hidden/c.mp4", "filler/d.mp4", "sub/filler/e.mp4"] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, b"x").unwrap();
        }
        let mut found: Vec<String> = walk(dir.path()).into_keys().collect();
        found.sort();
        assert_eq!(found, ["a.mp4", "sub/b.MKV", "sub/filler/e.mp4"]);
    }
}
