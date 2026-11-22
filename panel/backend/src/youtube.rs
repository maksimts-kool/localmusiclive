//! Search YouTube and download clips into the library with yt-dlp.
//!
//! Downloads land in media/youtube/ as "Artist - Title [video id].mp4". yt-dlp
//! works in a hidden folder next to it, so the library scan never sees a
//! half-finished file. A finished clip is indexed right away and, if asked,
//! queued or set to play next.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::future::join_all;
use regex::Regex;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

use crate::db::{self, Db, Track};
use crate::discover::{self, Source};
use crate::engine::{Engine, EngineError};
use crate::hub::Hub;
use crate::levels;
use crate::library::Library;
use crate::settings::Settings;
use crate::util::{now, run};

pub static VIDEO_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap());
static URL_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/|embed/|live/)|youtu\.be/)([A-Za-z0-9_-]{11})").unwrap()
});
/// "(Official Video)", "[4K Remaster]", "(Lyrics)" and the like.
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*[(\[{【][^)\]}】]*?\b(official|video|audio|lyrics?|lyric video|visuali[sz]er|clip|hd|hq|4k|remaster(?:ed)?|mv|m/v)\b[^)\]}】]*[)\]}】]",
    )
    .unwrap()
});
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s{2,}").unwrap());
static PATH_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([A-Za-z0-9_-]{11})\]\.[^./]+$").unwrap());
static ID_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[[A-Za-z0-9_-]{11}\]$").unwrap());
static TOPIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*-\s*Topic$").unwrap());
static VEVO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s*VEVO$").unwrap());
static UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\\/:*?"<>|\x00-\x1f]"#).unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static ERROR_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ERROR:\s*(\[[^\]]+\]\s*[A-Za-z0-9_-]+:\s*)?").unwrap());

const SEPARATORS: [&str; 6] = [" - ", " – ", " — ", " -- ", " | ", " : "];
const QUOTES: &[char] = &['"', '\'', '“', '”', '‘', '’', '«', '»'];
const SEARCH_RESULTS: usize = 15;
const PARALLEL_DOWNLOADS: usize = 2;
/// A finished download leaves the list after this long (failed ones stay).
const AUTO_DISMISS: Duration = Duration::from_secs(5);
/// New-song suggestions: built from this many most-played songs and artists,
/// kept this long unless refreshed.
const SUGGEST_SEEDS: usize = 6;
const SUGGEST_ARTISTS: usize = 3;
/// ...drawn at random (weighted by plays) from this many top songs and artists,
/// so each refresh starts from a different mix of favourites.
const SUGGEST_SEED_POOL: i64 = 20;
const SUGGEST_ARTIST_POOL: i64 = 8;
const SUGGEST_LIMIT: usize = 20;
/// Songs shown in this many previous lists are left out of a refresh.
const SUGGEST_REMEMBER: usize = 3;
const SUGGEST_MIX_LENGTH: usize = 25;
const SUGGEST_TTL: f64 = 6.0 * 3600.0;

pub fn video_id_from(text: &str) -> Option<String> {
    let text = text.trim();
    if VIDEO_ID.is_match(text) {
        return Some(text.to_string());
    }
    URL_ID.captures(text).map(|c| c[1].to_string())
}

/// The video id in a downloaded file's path ("... [id].mp4").
pub fn id_in_path(path: &str) -> Option<String> {
    PATH_ID.captures(path).map(|c| c[1].to_string())
}

/// "Artist - Title [id]" → "Artist - Title".
pub fn strip_id_suffix(stem: &str) -> &str {
    match ID_SUFFIX.find(stem) {
        Some(m) => &stem[..m.start()],
        None => stem,
    }
}

pub fn clean_channel(name: &str) -> String {
    let name = TOPIC.replace(name, "");
    VEVO.replace(&name, "").trim().to_string()
}

fn unquote(text: &str) -> String {
    text.trim_matches(QUOTES).to_string()
}

/// Best guess at (artist, song title) for a YouTube video.
pub fn split_title(title: &str, channel: &str, artist: Option<&str>, track: Option<&str>) -> (String, String) {
    if let (Some(artist), Some(track)) = (artist.filter(|a| !a.is_empty()), track.filter(|t| !t.is_empty())) {
        return (artist.split(',').next().unwrap_or("").trim().to_string(), track.trim().to_string());
    }
    let stripped = NOISE.replace_all(title, "");
    let text = SPACES.replace_all(&stripped, " ");
    let text = text.trim_matches(|c| " -–—|".contains(c));
    for sep in SEPARATORS {
        if let Some((left, right)) = text.split_once(sep) {
            if !left.trim().is_empty() && !right.trim().is_empty() {
                return (unquote(left.trim()), unquote(right.trim()));
            }
        }
    }
    (clean_channel(channel), unquote(text.trim()))
}

pub fn safe_filename(text: &str) -> String {
    let text = UNSAFE.replace_all(text, "");
    let text = WHITESPACE.replace_all(&text, " ");
    let text: String = text.trim_matches([' ', '.']).chars().take(120).collect();
    if text.is_empty() {
        "video".into()
    } else {
        text
    }
}

/// Best video plus best audio, optionally no taller than max_height.
pub fn download_formats(max_height: i64) -> String {
    if max_height > 0 {
        let cap = format!("[height<={max_height}]");
        format!("bv*{cap}+ba/b{cap}/bv*+ba/b")
    } else {
        "bv*+ba/b".into()
    }
}

/// How strongly a library song points to new ones: its plays, by its level.
fn favourite(track: &Track) -> i64 {
    (track.play_count + 1) * levels::favourite_factor(track.level)
}

pub fn thumbnail(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg")
}

fn error_message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| l.starts_with("ERROR")).collect();
    let lines = if lines.is_empty() { stderr.trim().lines().collect() } else { lines };
    let message = lines.last().copied().unwrap_or("yt-dlp failed");
    ERROR_PREFIX.replace(message, "").chars().take(300).collect()
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn is_live(entry: &Value) -> bool {
    matches!(str_field(entry, "live_status"), "is_live" | "is_upcoming" | "post_live")
}

struct Job {
    id: i64,
    video_id: String,
    title: String,
    /// "none" | "queue" | "next"
    then: String,
    /// waiting | downloading | adding | done | error
    status: &'static str,
    progress: f64,
    error: Option<String>,
    track_id: Option<i64>,
    created_at: f64,
    task: Option<AbortHandle>,
}

impl Job {
    fn public(&self) -> Value {
        json!({
            "id": self.id,
            "video_id": self.video_id,
            "title": self.title,
            "then": self.then,
            "status": self.status,
            "progress": self.progress,
            "error": self.error,
            "track_id": self.track_id,
            "created_at": self.created_at,
            "thumb": thumbnail(&self.video_id),
        })
    }

    fn active(&self) -> bool {
        matches!(self.status, "waiting" | "downloading" | "adding")
    }
}

#[derive(Default)]
struct Inner {
    jobs: BTreeMap<i64, Job>,
    next_id: i64,
    last_push: f64,
    /// (library size, made at, result)
    suggest_cache: Option<(i64, f64, Value)>,
    recently_shown: VecDeque<HashSet<String>>,
}

pub struct YouTube {
    s: Arc<Settings>,
    db: Arc<Db>,
    library: Arc<Library>,
    engine: Arc<Engine>,
    hub: Hub,
    inner: Mutex<Inner>,
    slots: Semaphore,
    suggest_lock: tokio::sync::Mutex<()>,
}

impl YouTube {
    pub fn new(s: Arc<Settings>, db: Arc<Db>, library: Arc<Library>, engine: Arc<Engine>, hub: Hub) -> YouTube {
        YouTube {
            s,
            db,
            library,
            engine,
            hub,
            inner: Mutex::new(Inner { next_id: 1, ..Default::default() }),
            slots: Semaphore::new(PARALLEL_DOWNLOADS),
            suggest_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- yt-dlp ----------------------------------------------------------------

    fn base_args(&self) -> Vec<String> {
        let mut args = vec!["--no-warnings".to_string(), "--ignore-config".to_string()];
        if self.s.cookies_file().exists() {
            args.push("--cookies".into());
            args.push(self.s.cookies_file().to_string_lossy().into_owned());
        }
        args
    }

    async fn ytdlp(&self, args: &[&str], timeout: Duration) -> (i32, String, String) {
        let mut all = self.base_args();
        all.extend(args.iter().map(|a| a.to_string()));
        let out = run(&self.s.ytdlp, &all, timeout, true).await;
        (out.code, String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    pub async fn start(&self) {
        if self.s.ytdlp_auto_update {
            // YouTube changes often and old yt-dlp releases stop working.
            let out = run(&self.s.ytdlp, &["--update".into()], Duration::from_secs(120), true).await;
            if out.code != 0 {
                let err = String::from_utf8_lossy(&out.stderr);
                let tail: String = err.trim().chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
                tracing::warn!("Could not update yt-dlp: {tail}");
            }
        }
        let (code, out, _) = self.ytdlp(&["--version"], Duration::from_secs(20)).await;
        let version = (code == 0).then(|| out.trim().to_string());
        tracing::info!("yt-dlp {}", version.as_deref().unwrap_or("is not available"));
    }

    // --- search ----------------------------------------------------------------

    fn existing(&self, video_id: &str) -> Option<Track> {
        let conn = self.db.lock();
        db::tracks(&conn, "SELECT * FROM tracks WHERE missing = 0 AND instr(path, ?) > 0", [format!("[{video_id}].")])
            .ok()
            .and_then(|t| t.into_iter().next())
    }

    pub async fn search(&self, query: &str) -> Result<Vec<Value>, String> {
        let entries: Vec<Value> = if let Some(video_id) = video_id_from(query) {
            let url = format!("https://www.youtube.com/watch?v={video_id}");
            let (code, out, err) = self.ytdlp(&["-J", "--no-playlist", &url], Duration::from_secs(60)).await;
            if code != 0 {
                return Err(error_message(&err));
            }
            serde_json::from_str(&out).map(|v| vec![v]).map_err(|e| e.to_string())?
        } else {
            let target = format!("ytsearch{SEARCH_RESULTS}:{query}");
            let (code, out, err) = self.ytdlp(&["-J", "--flat-playlist", &target], Duration::from_secs(60)).await;
            if code != 0 {
                return Err(error_message(&err));
            }
            let data: Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
            data.get("entries").and_then(Value::as_array).cloned().unwrap_or_default()
        };

        let mut results = Vec::new();
        for entry in entries {
            if entry.is_null() || is_live(&entry) {
                continue;
            }
            let vid = str_field(&entry, "id");
            if !VIDEO_ID.is_match(vid) {
                continue;
            }
            let channel = [str_field(&entry, "channel"), str_field(&entry, "uploader")]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("");
            let title = str_field(&entry, "title");
            results.push(json!({
                "id": vid,
                "title": if title.is_empty() { vid } else { title },
                "channel": channel,
                "duration": entry.get("duration"),
                "views": entry.get("view_count"),
                "thumb": thumbnail(vid),
                "track_id": self.existing(vid).map(|t| t.id),
            }));
        }
        Ok(results)
    }

    // --- suggestions -----------------------------------------------------------

    async fn entries(&self, target: &str, extra: &[&str]) -> Vec<Value> {
        let mut args = vec!["-J", "--flat-playlist"];
        args.extend_from_slice(extra);
        args.push(target);
        let (code, out, err) = self.ytdlp(&args, Duration::from_secs(60)).await;
        if code != 0 {
            tracing::info!("Suggestions: yt-dlp failed for {target}: {}", error_message(&err));
            return Vec::new();
        }
        serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get("entries").and_then(Value::as_array).cloned())
            .map(|e| e.into_iter().filter(|x| !x.is_null()).collect())
            .unwrap_or_default()
    }

    fn seeds(&self) -> Result<Vec<Track>> {
        let conn = self.db.lock();
        // Favourites: played songs and every Отлично song, never Плохо ones.
        let favour = levels::FAVOURITE_SQL;
        let rows = db::tracks(
            &conn,
            &format!(
                "SELECT * FROM tracks WHERE missing = 0 AND {favour} > 0 AND (play_count > 0 OR level = {great}) \
                 ORDER BY (play_count + 1) * {favour} DESC, last_played_at DESC LIMIT ?",
                great = levels::GREAT
            ),
            [SUGGEST_SEED_POOL],
        )?;
        let mut rng = rand::rng();
        let weights: Vec<f64> = rows.iter().map(|r| favourite(r) as f64).collect();
        let mut rows = discover::weighted_sample(rows, &weights, SUGGEST_SEEDS, &mut rng);
        if rows.len() < SUGGEST_SEEDS {
            // Little history yet: fill up with random newer songs.
            let taken: Vec<i64> = if rows.is_empty() { vec![0] } else { rows.iter().map(|r| r.id).collect() };
            let sql = format!(
                "SELECT * FROM tracks WHERE missing = 0 AND level != {} AND id NOT IN ({}) \
                 ORDER BY added_at DESC LIMIT {SUGGEST_SEED_POOL}",
                levels::BAD,
                db::marks(taken.len())
            );
            let newer = db::tracks(&conn, &sql, rusqlite::params_from_iter(taken.iter()))?;
            let want = SUGGEST_SEEDS - rows.len();
            let ones = vec![1.0; newer.len()];
            rows.extend(discover::weighted_sample(newer, &ones, want, &mut rng));
        }
        rows.sort_by_key(|r| std::cmp::Reverse(favourite(r)));
        Ok(rows)
    }

    fn artists(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.db.lock();
        // Plays of Отлично songs count three times, of Плохо ones not at all.
        let mut stmt = conn.prepare(&format!(
            "SELECT artist, SUM(play_count * {}) AS plays FROM tracks \
             WHERE missing = 0 AND artist != '' GROUP BY artist COLLATE NOCASE \
             HAVING plays > 0 ORDER BY plays DESC LIMIT ?",
            levels::FAVOURITE_SQL
        ))?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([SUGGEST_ARTIST_POOL], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let weights: Vec<f64> = rows.iter().map(|r| r.1 as f64).collect();
        Ok(discover::weighted_sample(rows, &weights, SUGGEST_ARTISTS, &mut rand::rng()))
    }

    async fn mix(&self, row: &Track, sem: &Semaphore) -> Source {
        let _permit = sem.acquire().await;
        let mut vid = id_in_path(&row.path);
        if vid.is_none() {
            let name = if row.artist.is_empty() { row.title.clone() } else { format!("{} - {}", row.artist, row.title) };
            let found = self.entries(&format!("ytsearch1:{name}"), &[]).await;
            vid = found.first().map(|e| str_field(e, "id").to_string());
        }
        let mut entries = Vec::new();
        if let Some(vid) = vid.filter(|v| VIDEO_ID.is_match(v)) {
            let url = format!("https://www.youtube.com/watch?v={vid}&list=RD{vid}");
            let end = SUGGEST_MIX_LENGTH.to_string();
            entries = self.entries(&url, &["--playlist-end", &end]).await;
        }
        let boost = if row.level == levels::GREAT { 1.5 } else { 1.0 };
        Source { kind: "mix", name: row.title.clone(), plays: row.play_count, boost, entries }
    }

    async fn by_artist(&self, artist: &str, plays: i64, sem: &Semaphore) -> Source {
        let _permit = sem.acquire().await;
        let entries = self.entries(&format!("ytsearch12:{artist} music video"), &[]).await;
        Source { kind: "artist", name: artist.to_string(), plays, boost: 1.0, entries }
    }

    /// New songs to download, from the most-played songs in the library.
    ///
    /// Kept until songs are added or deleted, or until refreshed;
    /// a refresh starts from other favourites and skips recently shown songs.
    pub async fn suggestions(&self, refresh: bool) -> Result<Value> {
        let _guard = self.suggest_lock.lock().await;
        // A new or deleted song makes a new list; plays alone don't.
        let key: i64 = self.db.lock().query_row("SELECT COUNT(*) FROM tracks WHERE missing = 0", [], |r| r.get(0))?;
        let cached = self.inner().suggest_cache.clone();
        if let Some((cached_key, made, result)) = cached {
            if !refresh && cached_key == key && now() - made < SUGGEST_TTL {
                return Ok(self.fresh(result));
            }
        }
        let seeds = self.seeds()?;
        let artists = self.artists()?;

        let sem = Semaphore::new(3);
        let mixes = join_all(seeds.iter().map(|r| self.mix(r, &sem)));
        let by_artist = join_all(artists.iter().map(|(a, p)| self.by_artist(a, *p, &sem)));
        let (mut sources, more) = futures::join!(mixes, by_artist);
        sources.extend(more);

        let (have_ids, have_keys, artist_songs, disliked) = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare("SELECT artist, title, path, level FROM tracks")?;
            let rows: Vec<(String, String, String, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let have_ids: HashSet<String> = rows.iter().filter_map(|(_, _, p, _)| id_in_path(p)).collect();
            let have_keys: HashSet<String> = rows.iter().map(|(a, t, _, _)| discover::song_key(a, t)).collect();
            // Liked songs per artist; artists with only Плохо songs are disliked.
            let mut artist_songs: HashMap<String, i64> = HashMap::new();
            let mut disliked: HashSet<String> = HashSet::new();
            for (artist, _, _, level) in &rows {
                if !artist.is_empty() {
                    let key = artist.to_lowercase();
                    if *level == levels::BAD {
                        disliked.insert(key);
                    } else {
                        *artist_songs.entry(key).or_default() += 1;
                    }
                }
            }
            disliked.retain(|a| !artist_songs.contains_key(a));
            (have_ids, have_keys, artist_songs, disliked)
        };

        let ranked = discover::rank(
            &sources,
            &have_ids,
            &have_keys,
            &artist_songs,
            &disliked,
            |title, channel| split_title(title, channel, None, None),
            SUGGEST_LIMIT * 4,
        );
        let result = {
            let mut inner = self.inner();
            let shown: HashSet<String> = inner.recently_shown.iter().flatten().cloned().collect();
            let ranked = discover::fresh_picks(ranked, &shown, SUGGEST_LIMIT, &mut rand::rng());
            inner.recently_shown.push_back(ranked.iter().filter_map(|i| i.get("id")?.as_str().map(String::from)).collect());
            while inner.recently_shown.len() > SUGGEST_REMEMBER {
                inner.recently_shown.pop_front();
            }
            let items: Vec<Value> = ranked
                .iter()
                .map(|item| {
                    let item = Value::Object(item.clone());
                    let id = str_field(&item, "id");
                    let title = str_field(&item, "title");
                    let channel = [str_field(&item, "channel"), str_field(&item, "uploader")]
                        .into_iter()
                        .find(|s| !s.is_empty())
                        .unwrap_or("");
                    json!({
                        "id": id,
                        "title": if title.is_empty() { id } else { title },
                        "channel": channel,
                        "duration": item.get("duration"),
                        "views": item.get("view_count"),
                        "thumb": thumbnail(id),
                        "reasons": item.get("reasons"),
                    })
                })
                .collect();
            let result = json!({
                "based_on": seeds.iter().map(|r| json!({
                    "id": r.id, "title": r.title, "artist": r.artist, "plays": r.play_count, "level": r.level,
                })).collect::<Vec<_>>(),
                "items": items,
                "created_at": now(),
            });
            inner.suggest_cache = Some((key, now(), result.clone()));
            result
        };
        Ok(self.fresh(result))
    }

    /// Drops suggestions downloaded since they were made.
    fn fresh(&self, mut result: Value) -> Value {
        if let Some(items) = result.get_mut("items").and_then(Value::as_array_mut) {
            items.retain(|i| self.existing(str_field(i, "id")).is_none());
        }
        result
    }

    // --- downloads -------------------------------------------------------------

    pub fn state(&self) -> Vec<Value> {
        self.inner().jobs.values().rev().map(Job::public).collect()
    }

    async fn push(&self, force: bool) {
        let t = now();
        {
            let mut inner = self.inner();
            if !force && t - inner.last_push <= 1.0 {
                return;
            }
            inner.last_push = t;
        }
        self.engine.broadcast().await;
    }

    fn update(&self, job_id: i64, f: impl FnOnce(&mut Job)) {
        if let Some(job) = self.inner().jobs.get_mut(&job_id) {
            f(job);
        }
    }

    pub async fn download(self: &Arc<Self>, video_id: &str, then: &str, title: Option<&str>) -> Result<Value> {
        if !VIDEO_ID.is_match(video_id) {
            return Err(EngineError::new("That is not a YouTube video id"));
        }
        let job_id = {
            let mut inner = self.inner();
            if let Some(job) = inner.jobs.values_mut().find(|j| j.video_id == video_id && j.active()) {
                if then != "none" {
                    job.then = then.to_string();
                }
                return Ok(job.public());
            }
            // The title shown until yt-dlp reports the real one.
            let id = inner.next_id;
            inner.next_id += 1;
            inner.jobs.insert(
                id,
                Job {
                    id,
                    video_id: video_id.to_string(),
                    title: title.filter(|t| !t.is_empty()).unwrap_or(video_id).to_string(),
                    then: then.to_string(),
                    status: "waiting",
                    progress: 0.0,
                    error: None,
                    track_id: None,
                    created_at: now(),
                    task: None,
                },
            );
            id
        };
        if let Some(track) = self.existing(video_id) {
            // Already in the library: no download, just queue it.
            self.update(job_id, |job| {
                job.title = track.label();
                job.track_id = Some(track.id);
            });
            self.finish(job_id).await?;
        } else {
            let this = Arc::clone(self);
            let handle = tokio::spawn(async move { this.run_download(job_id).await });
            self.update(job_id, |job| job.task = Some(handle.abort_handle()));
        }
        self.push(true).await;
        let public = self.inner().jobs.get(&job_id).map(Job::public);
        public.ok_or_else(|| anyhow!("The download disappeared"))
    }

    async fn finish(self: &Arc<Self>, job_id: i64) -> Result<()> {
        let (then, track_id) = {
            let inner = self.inner();
            let job = inner.jobs.get(&job_id).ok_or_else(|| anyhow!("The download was dismissed"))?;
            (job.then.clone(), job.track_id)
        };
        if let (true, Some(track_id)) = (then != "none", track_id) {
            self.engine.queue_add(track_id, then == "next", None).await?;
        }
        self.update(job_id, |job| {
            job.status = "done";
            job.progress = 1.0;
        });
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(AUTO_DISMISS).await;
            let removed = {
                let mut inner = this.inner();
                let done = inner.jobs.get(&job_id).is_some_and(|j| j.status == "done");
                done && inner.jobs.remove(&job_id).is_some()
            };
            if removed {
                this.push(true).await;
            }
        });
        Ok(())
    }

    fn tmp_dir(&self) -> PathBuf {
        self.s.data_dir.join("tmp")
    }

    async fn run_download(self: Arc<Self>, job_id: i64) {
        let video_id = match self.inner().jobs.get(&job_id) {
            Some(job) => job.video_id.clone(),
            None => return,
        };
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let info_file = self.tmp_dir().join(format!("{video_id}.info.json"));
        let outcome = self.download_steps(job_id, &url, &info_file).await;
        match outcome {
            Ok(rel) => tracing::info!("Downloaded {url} → {rel}"),
            Err(err) => {
                let message = format!("{err:#}");
                tracing::warn!("Download of {url} failed: {message}");
                self.update(job_id, |job| {
                    job.status = "error";
                    job.error = Some(message);
                });
            }
        }
        let _ = std::fs::remove_file(&info_file);
        self.cleanup(&video_id);
        self.push(true).await;
    }

    async fn download_steps(self: &Arc<Self>, job_id: i64, url: &str, info_file: &PathBuf) -> Result<String> {
        let formats = download_formats(self.s.youtube_max_height);
        std::fs::create_dir_all(self.tmp_dir())?;
        let (path, artist, title) = {
            let _slot = self.slots.acquire().await?;
            self.update(job_id, |job| job.status = "downloading");
            self.push(true).await;

            let (code, out, err) = self.ytdlp(&["-J", "--no-playlist", "-f", &formats, url], Duration::from_secs(60)).await;
            if code != 0 {
                return Err(anyhow!(error_message(&err)));
            }
            let info: Value = serde_json::from_str(&out)?;
            if crate::ffplayout::truthy(info.get("is_live").unwrap_or(&Value::Null))
                || matches!(str_field(&info, "live_status"), "is_live" | "is_upcoming")
            {
                return Err(anyhow!("Live streams can't be downloaded"));
            }
            let duration = info.get("duration").and_then(Value::as_f64).unwrap_or(0.0);
            if duration > (self.s.youtube_max_minutes * 60) as f64 {
                return Err(anyhow!("Longer than {} minutes (YOUTUBE_MAX_MINUTES)", self.s.youtube_max_minutes));
            }
            let channel = [str_field(&info, "channel"), str_field(&info, "uploader")]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("");
            let (artist, title) = split_title(
                str_field(&info, "title"),
                channel,
                info.get("artist").and_then(Value::as_str),
                info.get("track").and_then(Value::as_str),
            );
            let label = if artist.is_empty() { title.clone() } else { format!("{artist} – {title}") };
            self.update(job_id, |job| job.title = label);
            std::fs::write(info_file, out)?;
            self.push(true).await;

            let streams = info.get("requested_formats").and_then(Value::as_array).map(|f| f.len()).unwrap_or(1).max(1);
            let path = self.fetch(job_id, info_file, &formats, &artist, &title, streams).await?;
            (path, artist, title)
        };

        self.update(job_id, |job| job.status = "adding");
        self.push(true).await;
        let rel = path
            .strip_prefix(&self.s.media_root)
            .map_err(|_| anyhow!("yt-dlp saved the file outside the media folder"))?
            .to_string_lossy()
            .into_owned();
        let track_id = self.library.add_file(&rel, &artist, &title).await?;
        let Some(track_id) = track_id else {
            return Err(anyhow!("The downloaded file could not be read"));
        };
        self.update(job_id, |job| job.track_id = Some(track_id));
        self.hub.broadcast(&json!({"type": "library"}));
        self.finish(job_id).await?;
        Ok(rel)
    }

    async fn fetch(&self, job_id: i64, info_file: &PathBuf, formats: &str, artist: &str, title: &str, streams: usize) -> Result<PathBuf> {
        let name = safe_filename(&if artist.is_empty() { title.to_string() } else { format!("{artist} - {title}") })
            .replace('%', "%%");
        let download_dir = self.s.download_dir();
        let mut args = self.base_args();
        args.extend(
            [
                "--load-info-json".to_string(),
                info_file.to_string_lossy().into_owned(),
                "-f".into(),
                formats.into(),
                "--merge-output-format".into(),
                "mp4/mkv".into(),
                "--paths".into(),
                format!("home:{}", download_dir.display()),
                "--paths".into(),
                format!("temp:{}", download_dir.join(".incoming").display()),
                "-o".into(),
                format!("{name} [%(id)s].%(ext)s"),
                "--no-mtime".into(),
                "--newline".into(),
                // --print would otherwise silence it
                "--progress".into(),
                "--progress-template".into(),
                "download:[progress] %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s".into(),
                "--print".into(),
                "after_move:filepath".into(),
            ],
        );
        let mut child = Command::new(&self.s.ytdlp)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        });

        let (mut stage, mut last_fraction, mut last) = (0usize, 0.0f64, None::<PathBuf>);
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("[progress]") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let number = |p: &&str| (!matches!(*p, "NA" | "None")).then(|| p.parse::<f64>().ok()).flatten();
                let done = parts.first().and_then(number).unwrap_or(0.0);
                let total = parts.iter().skip(1).find_map(number).unwrap_or(0.0);
                let fraction = if total > 0.0 { done / total } else { 0.0 };
                if fraction + 0.2 < last_fraction {
                    stage += 1; // next stream (video, then audio)
                }
                last_fraction = fraction;
                let progress = ((stage.min(streams - 1) as f64 + fraction) / streams as f64).min(0.99);
                self.update(job_id, |job| job.progress = progress);
                self.push(false).await;
            } else if line.starts_with('/') {
                last = Some(PathBuf::from(line));
            }
        }
        let status = child.wait().await?;
        let err = stderr_task.await.unwrap_or_default();
        match last {
            Some(path) if status.success() && path.exists() => Ok(path),
            _ => Err(anyhow!(error_message(&err))),
        }
    }

    fn cleanup(&self, video_id: &str) {
        let incoming = self.s.download_dir().join(".incoming");
        let Ok(entries) = std::fs::read_dir(incoming) else { return };
        let marker = format!("[{video_id}]");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().contains(&marker) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    pub async fn dismiss(&self, job_id: i64) {
        let job = self.inner().jobs.remove(&job_id);
        if let Some(job) = job {
            if let Some(task) = job.task.filter(|t| !t.is_finished()) {
                // Aborting drops yt-dlp (kill_on_drop); tidy up what it left.
                task.abort();
                let _ = std::fs::remove_file(self.tmp_dir().join(format!("{}.info.json", job.video_id)));
                self.cleanup(&job.video_id);
            }
        }
        self.push(true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(title: &str, channel: &str) -> (String, String) {
        split_title(title, channel, None, None)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn artist_and_title_from_video_title() {
        assert_eq!(
            split("Daft Punk - Get Lucky (Official Audio) ft. Pharrell Williams, Nile Rodgers", ""),
            pair("Daft Punk", "Get Lucky ft. Pharrell Williams, Nile Rodgers")
        );
        assert_eq!(
            split("Daft Punk : Get Lucky // Grammys Rehearsal (HD)", ""),
            pair("Daft Punk", "Get Lucky // Grammys Rehearsal")
        );
        assert_eq!(split("Queen – \"Bohemian Rhapsody\" [Official Video Remastered]", ""), pair("Queen", "Bohemian Rhapsody"));
    }

    #[test]
    fn channel_is_artist_when_title_has_no_separator() {
        assert_eq!(split("Bad Guy (Official Music Video)", "BillieEilishVEVO"), pair("BillieEilish", "Bad Guy"));
        assert_eq!(split("Midnight City", "M83 - Topic"), pair("M83", "Midnight City"));
    }

    #[test]
    fn youtube_music_metadata_wins() {
        assert_eq!(
            split_title("whatever", "x", Some("ABBA, Someone"), Some("Dancing Queen")),
            pair("ABBA", "Dancing Queen")
        );
    }

    #[test]
    fn video_id_from_urls() {
        for text in [
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?list=PL1&v=dQw4w9WgXcQ&t=3",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id_from(text).as_deref(), Some("dQw4w9WgXcQ"), "{text}");
        }
        assert_eq!(video_id_from("never gonna give you up"), None);
    }

    #[test]
    fn safe_filenames() {
        assert_eq!(safe_filename("AC/DC - Back In Black: \"Live\"?"), "ACDC - Back In Black Live");
    }

    #[test]
    fn formats() {
        assert_eq!(download_formats(0), "bv*+ba/b");
        assert_eq!(download_formats(1080), "bv*[height<=1080]+ba/b[height<=1080]/bv*+ba/b");
    }

    #[test]
    fn ids_in_paths() {
        assert_eq!(id_in_path("youtube/Queen - Radio Ga Ga [azdwsXLmrHE].mp4").as_deref(), Some("azdwsXLmrHE"));
        assert_eq!(strip_id_suffix("Queen - Radio Ga Ga [azdwsXLmrHE]"), "Queen - Radio Ga Ga");
        assert_eq!(error_message("WARNING: x\nERROR: [youtube] abcdefghijk: Video unavailable"), "Video unavailable");
    }
}
