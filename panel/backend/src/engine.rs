//! Keeps ffplayout's playlists in step with the queue.
//!
//! ffplayout reports what it plays four times a second. When the clip changes,
//! the engine records history, takes the clip off the queue or autoplay list and
//! rebuilds the playlists for today and tomorrow (see timeline.rs). Queue edits
//! rebuild the same way; ffplayout picks the change up at the next clip
//! boundary, so the clip on air is never interrupted.
//!
//! Skip trims the clip on air in the playlist so the next clip is due now, then
//! asks ffplayout to resync with the playlist ("reset"). ffplayout's own "next"
//! would shift its clock instead, which breaks the hand-over at midnight.
//!
//! Locking: `lock` serialises commands, like one-at-a-time edits of the
//! schedule, and may be held across awaits. `st` (engine state) and the
//! database are std mutexes held only for short synchronous work; the database
//! may be locked while holding `st`, never the other way round.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Days, NaiveDate, Timelike};
use chrono_tz::Tz;
use rand::seq::IndexedRandom;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::ads::{AdSettings, Ads, BreakState, WithAds};
use crate::levels;
use crate::db::{self, Db, Track};
use crate::ffplayout::{truthy, Event, EventLines, FFplayout, FfError};
use crate::hub::Hub;
use crate::library::Library;
use crate::overlay::{self, Card, Line, OverlaySettings, QueueCard, QueueItem};
use crate::settings::Settings;
use crate::timeline::{gap, layout, r3, Slot, DAY, MIN_PART};
use crate::util::{now, round_to};
use crate::youtube::YouTube;

/// After a skip, the skipped clip stays scheduled this much past "now", so
/// ffplayout starts the next clip from its first frame.
const SKIP_LEAD: f64 = 0.5;
/// Don't bother skipping a clip with less than this left.
const MIN_SKIP_REMAINING: f64 = 2.0;
/// A fresh schedule's time placeholder ends this far in the past. If ffplayout
/// lands inside the placeholder it plays a 60 s black dummy instead.
const GAP_BACKDATE: f64 = 0.3;
const FRESH_START_COOLDOWN: f64 = 8.0;
/// ffplayout shifts its clock by an hour on daylight-saving changes; while it
/// does, playlist rewrites can't line up and skip falls back to ffplayout's own.
const MAX_SHIFT: f64 = 2.0;
/// A clip that resumes within this many seconds after playout stopped is the
/// same play, not a new history entry.
const RESUME_WINDOW: f64 = 600.0;
/// Playlist versions kept per day, to recognise what ffplayout plays.
const PLANS_PER_DAY: i64 = 8;
/// Check queue files on disk for this many autoplay picks too; deeper picks
/// rely on the periodic library scan.
const CHECK_FILES_AHEAD: usize = 3;

/// Card fades in/out over this long; "Up next" is gone this long before the cut.
const CARD_FADE: f64 = 0.5;
const CARD_END_GAP: f64 = 0.2;
/// The progress line moves at most this often (seconds), to spare the encoder.
const LINE_STEP: f64 = 0.25;
/// The progress line shows the song on air and at most this many queued songs.
const LINE_MAX_QUEUED: usize = 9;
/// The queue card dims over this long once its bright time is up.
const QUEUE_DIM_FADE: f64 = 1.5;
/// Where the panel puts the card for ffplayout, relative to the media folder.
const CARD_FILE: &str = ".overlay/card.png";

/// A command that can't be done in the current state; the API answers 409.
#[derive(Debug)]
pub struct EngineError(pub String);

impl EngineError {
    pub fn new(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(EngineError(message.into()))
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoplaySettings {
    pub enabled: bool,
    /// shuffle | least_played | in_order
    pub mode: String,
    pub folder: String,
    /// Song levels to pick from; empty means every song.
    pub levels: Vec<i64>,
    pub avoid_repeat: i64,
}

impl Default for AutoplaySettings {
    fn default() -> Self {
        AutoplaySettings {
            enabled: true,
            mode: "shuffle".into(),
            folder: String::new(),
            levels: vec![levels::GREAT, levels::GOOD],
            avoid_repeat: 20,
        }
    }
}

fn autoplay_from(conn: &Connection) -> AutoplaySettings {
    db::setting(conn, "autoplay").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn ads_from(conn: &Connection) -> AdSettings {
    db::setting(conn, "ads").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

fn overlay_from(conn: &Connection) -> OverlaySettings {
    db::setting(conn, "overlay").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// `current` with `values` on top.
pub fn merged<T: Serialize + for<'de> Deserialize<'de>>(current: &T, values: &Map<String, Value>) -> Result<T> {
    let mut all = serde_json::to_value(current)?;
    if let Some(obj) = all.as_object_mut() {
        for (key, value) in values {
            obj.insert(key.clone(), value.clone());
        }
    }
    Ok(serde_json::from_value(all)?)
}

#[derive(Clone, Debug)]
struct Current {
    /// Playlist the clip belongs to.
    date: String,
    index: i64,
    /// None: not something the panel scheduled.
    slot: Option<Slot>,
    /// That playlist up to and including the clip.
    prefix: Arc<Vec<Slot>>,
    /// Seconds since the slot's scheduled start, at received_at.
    elapsed: f64,
    received_at: f64,
    shift: f64,
    history_id: Option<i64>,
}

impl Current {
    fn managed(&self) -> bool {
        self.slot.as_ref().is_some_and(|s| !s.is_gap())
    }

    fn elapsed_now(&self) -> f64 {
        self.elapsed + (now() - self.received_at)
    }
}

struct Plan {
    today: Vec<Slot>,
    tomorrow: Vec<Slot>,
    /// Clip on air within `today`.
    index: i64,
}

/// Slots after the clip on air, in play order.
fn following(plan: &Plan, managed: bool) -> impl Iterator<Item = &Slot> {
    let start = if managed {
        (plan.index + 1).max(0) as usize
    } else {
        // Fresh schedule: everything after the placeholder.
        plan.today.iter().position(|s| !s.is_gap()).unwrap_or(plan.today.len())
    };
    plan.today.iter().skip(start).chain(plan.tomorrow.iter())
}

fn next_slot(plan: Option<&Plan>, managed: bool) -> Option<Slot> {
    following(plan?, managed).find(|s| !s.is_gap() && s.part == 0).cloned()
}

/// The first song (not an ad) after the clip on air, and how long after
/// `remaining` (what is left of the clip on air today) it starts.
fn next_song(plan: Option<&Plan>, managed: bool, remaining: f64) -> Option<(Slot, f64)> {
    let mut at = remaining;
    for slot in following(plan?, managed) {
        if !slot.is_gap() && slot.part == 0 && !slot.is_ad() {
            return Some((slot.clone(), at));
        }
        at += slot.length();
    }
    None
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    /// connecting | ok | error | reconnecting
    pub ffplayout: String,
    pub running: bool,
    pub mode: Option<String>,
    pub output: Option<String>,
    /// HLS mode: the watch service's port.
    pub watch_port: Option<i64>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            ffplayout: "connecting".into(),
            running: false,
            mode: None,
            output: None,
            watch_port: None,
            error: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Clone)]
enum Incoming {
    Event(Value),
    Text(String),
    Lost,
}

struct State {
    status: Status,
    current: Option<Current>,
    plan: Option<Arc<Plan>>,
    last_key: Option<(String, i64, String)>,
    /// Last playlist sent to ffplayout, by date (only recent dates).
    written: HashMap<String, String>,
    /// Parsed playlist versions by id, with their date.
    plan_cache: HashMap<i64, (String, Arc<Vec<Slot>>)>,
    last_fresh: f64,
    fresh_failures: u32,
    interrupted: Option<(String, i64, i64, f64)>,
    skip_pending: bool,
    card_key: Option<String>,
    /// What the queue card last showed, since when, and whether it was new then.
    queue_sig: Option<(String, i64, Vec<Option<i64>>)>,
    queue_since: Instant,
    queue_appeared: bool,
    text_cleared: bool,
}

/// Which card the stream shows ("now" or "next") and how opaque it is.
///
/// `position` is how far into the song we are, `until_next` how long until the
/// next song starts (None if nothing follows).
pub fn card_phase(position: f64, until_next: Option<f64>, s: &OverlaySettings) -> Option<(&'static str, f64)> {
    if !s.enabled {
        return None;
    }
    let (start, end) = (s.start_seconds as f64, s.end_seconds as f64);
    if s.mode == "always" {
        return Some(("now", (position.max(0.0) / CARD_FADE).min(1.0)));
    }
    if let Some(until) = until_next.filter(|_| s.show_next) {
        if CARD_END_GAP < until && until <= end {
            let shown = end - until;
            return Some(("next", 1f64.min(shown / CARD_FADE).min((until - CARD_END_GAP) / CARD_FADE)));
        }
    }
    if (0.0..start).contains(&position) {
        let mut opacity = 1f64.min(position / CARD_FADE).min((start - position) / CARD_FADE);
        if let Some(until) = until_next {
            opacity = opacity.min((until - CARD_END_GAP).max(0.0) / CARD_FADE);
        }
        return (opacity > 0.0).then_some(("now", opacity));
    }
    None
}

/// How visible the full-width progress line is. It takes over as the
/// "Now playing" card fades out and hands back as "Up next" fades in.
pub fn line_opacity(position: f64, until_next: Option<f64>, length: f64, s: &OverlaySettings) -> f64 {
    if !s.progress_line || length <= 0.0 {
        return 0.0;
    }
    let mut opacity = 1f64.min(position / CARD_FADE).min((length - position) / CARD_FADE);
    if s.enabled {
        if s.mode == "always" {
            return 0.0;
        }
        opacity = opacity.min((position - s.start_seconds as f64 + CARD_FADE) / CARD_FADE);
        if let Some(until) = until_next.filter(|_| s.show_next) {
            opacity = opacity.min((until - s.end_seconds as f64 + CARD_FADE) / CARD_FADE);
        }
    }
    opacity.clamp(0.0, 1.0)
}

/// How visible the queue card is `age` seconds after the song or the queue
/// last changed. It fades in (from nothing if it just `appeared`, else from
/// dimmed), stays bright for queue_bright_seconds, then dims.
pub fn queue_opacity(age: f64, appeared: bool, s: &OverlaySettings) -> f64 {
    let dim = s.queue_dim_opacity.clamp(0.0, 1.0);
    let start = if appeared { 0.0 } else { dim };
    let bright = s.queue_bright_seconds as f64;
    let level = if age < CARD_FADE {
        start + (1.0 - start) * age.max(0.0) / CARD_FADE
    } else if age < bright {
        1.0
    } else {
        1.0 - (1.0 - dim) * ((age - bright) / QUEUE_DIM_FADE).min(1.0)
    };
    round_to(level.clamp(0.0, 1.0), 2)
}

pub fn clock(seconds: f64) -> String {
    let seconds = ((seconds + 0.999) as i64).max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

pub fn track_json(t: &Track) -> Value {
    json!({
        "id": t.id,
        "artist": t.artist,
        "title": t.title,
        "album": t.album,
        "path": t.path,
        "folder": t.folder,
        "duration": t.duration,
        "play_count": t.play_count,
        "last_played_at": t.last_played_at,
        "level": t.level,
        "thumb": t.has_thumb.then(|| format!("/api/thumbs/{}.jpg?v={}", t.id, t.mtime as i64)),
    })
}

/// Chooses autoplay tracks; better-liked songs come up more often.
struct Picker {
    mode: String,
    pool: Vec<Track>,
    order: HashMap<i64, usize>,
    counts: HashMap<i64, i64>,
}

impl Picker {
    fn new(mut pool: Vec<Track>, mode: &str, planned: &HashMap<i64, i64>) -> Picker {
        pool.sort_by(|a, b| a.path.cmp(&b.path));
        let order = pool.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        let mut counts: HashMap<i64, i64> = pool.iter().map(|t| (t.id, t.play_count)).collect();
        for (id, n) in planned {
            *counts.entry(*id).or_default() += n;
        }
        Picker { mode: mode.to_string(), pool, order, counts }
    }

    fn pick(&mut self, window: usize, recent: &VecDeque<i64>) -> Track {
        let track = if self.mode == "in_order" {
            let last = recent.iter().rev().find(|id| self.order.contains_key(id));
            let index = last.map(|id| (self.order[id] + 1) % self.pool.len()).unwrap_or(0);
            self.pool[index].clone()
        } else {
            let avoid: HashSet<i64> = recent.iter().rev().take(window).copied().collect();
            let mut choices: Vec<&Track> = self.pool.iter().filter(|t| !avoid.contains(&t.id)).collect();
            if choices.is_empty() {
                choices = self.pool.iter().collect();
            }
            let weight = |t: &Track| levels::autoplay_weight(t.level);
            if self.mode == "least_played" {
                // Plays per weight: an Отлично song is due again after a third as many plays.
                let due = |t: &Track| self.counts.get(&t.id).copied().unwrap_or(0) as f64 / weight(t);
                let fewest = choices.iter().map(|t| due(t)).fold(f64::INFINITY, f64::min);
                choices.retain(|t| due(t) <= fewest + 1e-9);
                (*choices.choose(&mut rand::rng()).expect("non-empty pool")).clone()
            } else {
                (*choices.choose_weighted(&mut rand::rng(), |t| weight(t)).expect("non-empty pool")).clone()
            }
        };
        *self.counts.entry(track.id).or_default() += 1;
        track
    }
}

fn make_slot(library: &Library, track: &Track, kind: &str) -> Slot {
    Slot {
        track_id: Some(track.id),
        title: track.label(),
        ..Slot::new(kind, &library.abs_path(&track.path), 0.0, track.duration, track.duration)
    }
}

/// Tracks in the library, and those autoplay may pick from.
fn eligible(conn: &Connection, settings: &AutoplaySettings) -> Result<(HashMap<i64, Track>, Vec<Track>)> {
    let tracks: HashMap<i64, Track> =
        db::tracks(conn, "SELECT * FROM tracks WHERE missing = 0", [])?.into_iter().map(|t| (t.id, t)).collect();
    let folder = settings.folder.trim_matches('/');
    let pool = tracks
        .values()
        .filter(|t| folder.is_empty() || t.folder == folder || t.folder.starts_with(&format!("{folder}/")))
        .filter(|t| settings.levels.is_empty() || settings.levels.contains(&t.level))
        .cloned()
        .collect();
    Ok((tracks, pool))
}

fn has_material(conn: &Connection, settings: &AutoplaySettings) -> Result<bool> {
    let queued = conn
        .prepare("SELECT 1 FROM queue JOIN tracks ON tracks.id = queue.track_id WHERE missing = 0")?
        .exists([])?;
    if queued {
        return Ok(true);
    }
    Ok(settings.enabled && !eligible(conn, settings)?.1.is_empty())
}

/// Queue first, then autoplay picks, as many as the layout asks for.
struct Candidates<'a> {
    conn: &'a Connection,
    library: &'a Library,
    tracks: HashMap<i64, Track>,
    pool: Vec<Track>,
    in_pool: HashSet<i64>,
    mode: String,
    window: usize,
    recent: VecDeque<i64>,
    queue: std::vec::IntoIter<(i64, i64)>,
    upcoming: std::vec::IntoIter<(i64, i64)>,
    planned: HashMap<i64, i64>,
    checked: usize,
    picker: Option<Picker>,
    position: i64,
    autoplay: bool,
    stage: u8,
    error: Option<anyhow::Error>,
}

impl<'a> Candidates<'a> {
    fn new(conn: &'a Connection, library: &'a Library, settings: &AutoplaySettings) -> Result<Candidates<'a>> {
        let (tracks, pool) = eligible(conn, settings)?;
        let mut recent: VecDeque<i64> = VecDeque::new();
        let mut stmt =
            conn.prepare("SELECT track_id FROM history WHERE track_id IS NOT NULL ORDER BY id DESC LIMIT 100")?;
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        recent.extend(ids.into_iter().rev());
        let pairs = |sql: &str| -> Result<Vec<(i64, i64)>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        };
        let queue = pairs("SELECT id, track_id FROM queue ORDER BY position, id")?;
        let upcoming = pairs("SELECT id, track_id FROM upcoming ORDER BY position, id")?;
        let window = settings.avoid_repeat.min(pool.len() as i64 - 1).max(0) as usize;
        Ok(Candidates {
            conn,
            library,
            in_pool: pool.iter().map(|t| t.id).collect(),
            autoplay: settings.enabled && !pool.is_empty(),
            tracks,
            pool,
            mode: settings.mode.clone(),
            window,
            recent,
            queue: queue.into_iter(),
            upcoming: upcoming.into_iter(),
            planned: HashMap::new(),
            checked: 0,
            picker: None,
            position: 0,
            stage: 0,
            error: None,
        })
    }

    fn remember(&mut self, track_id: i64) {
        self.recent.push_back(track_id);
        if self.recent.len() > 500 {
            self.recent.pop_front();
        }
    }

    fn exists(&self, track: &Track) -> bool {
        Path::new(&self.library.abs_path(&track.path)).exists()
    }

    fn usable(&mut self, track: Option<&Track>) -> bool {
        let Some(track) = track else { return false };
        if !self.in_pool.contains(&track.id) {
            return false;
        }
        if self.window > 0 && self.recent.iter().rev().take(self.window).any(|id| *id == track.id) {
            return false;
        }
        if self.checked < CHECK_FILES_AHEAD {
            self.checked += 1;
            return self.exists(track);
        }
        true
    }

    fn step(&mut self) -> Result<Option<Slot>> {
        loop {
            match self.stage {
                0 => match self.queue.next() {
                    Some((queue_id, track_id)) => {
                        let Some(track) = self.tracks.get(&track_id).cloned() else { continue };
                        if !self.exists(&track) {
                            continue;
                        }
                        self.remember(track.id);
                        return Ok(Some(Slot { queue_id: Some(queue_id), ..make_slot(self.library, &track, "queue") }));
                    }
                    None => self.stage = if self.autoplay { 1 } else { 3 },
                },
                1 => match self.upcoming.next() {
                    Some((auto_id, track_id)) => {
                        let track = self.tracks.get(&track_id).cloned();
                        if !self.usable(track.as_ref()) {
                            self.conn.execute("DELETE FROM upcoming WHERE id = ?", [auto_id])?;
                            continue;
                        }
                        let track = track.expect("usable track");
                        self.remember(track.id);
                        *self.planned.entry(track.id).or_default() += 1;
                        return Ok(Some(Slot { auto_id: Some(auto_id), ..make_slot(self.library, &track, "auto") }));
                    }
                    None => {
                        self.picker = Some(Picker::new(std::mem::take(&mut self.pool), &self.mode, &self.planned));
                        let max: Option<i64> =
                            self.conn.query_row("SELECT MAX(position) FROM upcoming", [], |r| r.get(0))?;
                        self.position = max.unwrap_or(0) + 1;
                        self.stage = 2;
                    }
                },
                2 => {
                    let window = self.window;
                    let track = self.picker.as_mut().expect("picker").pick(window, &self.recent);
                    self.conn.execute(
                        "INSERT INTO upcoming (track_id, position) VALUES (?, ?)",
                        params![track.id, self.position],
                    )?;
                    let auto_id = self.conn.last_insert_rowid();
                    self.position += 1;
                    self.remember(track.id);
                    return Ok(Some(Slot { auto_id: Some(auto_id), ..make_slot(self.library, &track, "auto") }));
                }
                _ => return Ok(None),
            }
        }
    }
}

impl Iterator for Candidates<'_> {
    type Item = Slot;

    fn next(&mut self) -> Option<Slot> {
        match self.step() {
            Ok(slot) => slot,
            Err(err) => {
                self.error = Some(err);
                self.stage = 3;
                None
            }
        }
    }
}

pub struct Engine {
    s: Arc<Settings>,
    db: Arc<Db>,
    ff: Arc<FFplayout>,
    library: Arc<Library>,
    ads: Arc<Ads>,
    hub: Hub,
    tz: Tz,
    lock: tokio::sync::Mutex<()>,
    /// Card updates come from the overlay loop and from commands; one at a time.
    overlay_lock: tokio::sync::Mutex<()>,
    st: Mutex<State>,
    events: watch::Sender<Option<(f64, Incoming)>>,
    downloads: OnceLock<Weak<YouTube>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Engine {
    pub fn new(
        s: Arc<Settings>,
        db: Arc<Db>,
        ff: Arc<FFplayout>,
        library: Arc<Library>,
        ads: Arc<Ads>,
        hub: Hub,
    ) -> Engine {
        let tz = s.timezone.parse::<Tz>().unwrap_or_else(|_| {
            tracing::warn!("Unknown time zone {:?}; using UTC", s.timezone);
            Tz::UTC
        });
        Engine {
            s,
            db,
            ff,
            library,
            ads,
            hub,
            tz,
            lock: tokio::sync::Mutex::new(()),
            overlay_lock: tokio::sync::Mutex::new(()),
            st: Mutex::new(State {
                status: Status::default(),
                current: None,
                plan: None,
                last_key: None,
                written: HashMap::new(),
                plan_cache: HashMap::new(),
                last_fresh: 0.0,
                fresh_failures: 0,
                interrupted: None,
                skip_pending: false,
                card_key: None,
                queue_sig: None,
                queue_since: Instant::now(),
                queue_appeared: false,
                text_cleared: false,
            }),
            events: watch::channel(None).0,
            downloads: OnceLock::new(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn set_downloads(&self, youtube: &Arc<YouTube>) {
        let _ = self.downloads.set(Arc::downgrade(youtube));
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    // --- lifecycle -------------------------------------------------------------

    pub fn start(self: &Arc<Self>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let this = Arc::clone(self);
        tasks.push(tokio::spawn(async move { this.run_events().await }));
        let this = Arc::clone(self);
        tasks.push(tokio::spawn(async move { this.process_events().await }));
        let this = Arc::clone(self);
        tasks.push(tokio::spawn(async move { this.housekeeping().await }));
        let this = Arc::clone(self);
        tasks.push(tokio::spawn(async move { this.rescan_loop().await }));
        let this = Arc::clone(self);
        tasks.push(tokio::spawn(async move { this.overlay_loop().await }));
    }

    pub fn stop(&self) {
        for task in self.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            task.abort();
        }
    }

    pub fn status(&self) -> Status {
        self.st().status.clone()
    }

    // --- time ------------------------------------------------------------------

    fn local(&self, ts: f64) -> DateTime<Tz> {
        let secs = ts.floor();
        let nanos = ((ts - secs) * 1e9) as u32;
        DateTime::from_timestamp(secs as i64, nanos).unwrap_or_default().with_timezone(&self.tz)
    }

    fn playlist_date(&self, ts: f64) -> String {
        let shifted = self.local(ts) - chrono::Duration::milliseconds((self.s.day_start_sec() * 1000.0) as i64);
        shifted.date_naive().format("%Y-%m-%d").to_string()
    }

    fn day_pos(&self, ts: f64) -> f64 {
        let local = self.local(ts);
        let sod = local.num_seconds_from_midnight() as f64 + local.nanosecond() as f64 / 1e9;
        (sod - self.s.day_start_sec()).rem_euclid(DAY)
    }

    fn shift_date(date: &str, days: i64) -> String {
        let Ok(d) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else { return date.to_string() };
        let d = if days >= 0 { d.checked_add_days(Days::new(days as u64)) } else { d.checked_sub_days(Days::new((-days) as u64)) };
        d.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| date.to_string())
    }

    fn next_date(date: &str) -> String {
        Self::shift_date(date, 1)
    }

    // --- settings --------------------------------------------------------------

    pub fn autoplay_settings(&self) -> AutoplaySettings {
        autoplay_from(&self.db.lock())
    }

    pub fn ad_settings(&self) -> AdSettings {
        ads_from(&self.db.lock())
    }

    pub fn overlay_settings(&self) -> OverlaySettings {
        overlay_from(&self.db.lock())
    }

    // --- ffplayout setup -------------------------------------------------------

    async fn bootstrap(&self) {
        loop {
            let attempt = async {
                if self.ff.setup_required().await? {
                    tracing::info!("Running ffplayout first-time setup");
                    self.ff.complete_setup(&self.s.ffplayout_mail, &self.s.media_root.to_string_lossy()).await?;
                }
                self.ff.login_now().await?;
                self.configure_ffplayout().await
            };
            match attempt.await {
                Ok(()) => {
                    {
                        let mut st = self.st();
                        st.status.ffplayout = "ok".into();
                        st.status.error = None;
                    }
                    self.broadcast().await;
                    return;
                }
                Err(err) => {
                    // Keep retrying until ffplayout is reachable.
                    tracing::warn!("ffplayout not ready: {err:#}");
                    {
                        let mut st = self.st();
                        st.status.ffplayout = "error".into();
                        st.status.error = Some(format!("{err:#}"));
                    }
                    self.broadcast().await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Apply the settings the panel owns; leave everything else to ffplayout's UI.
    async fn configure_ffplayout(&self) -> Result<()> {
        fn section<'a>(cfg: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
            let obj = cfg.as_object_mut().ok_or_else(|| FfError("ffplayout sent no configuration".into()))?;
            let entry = obj.entry(key.to_string()).or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            Ok(entry.as_object_mut().expect("object"))
        }

        let s = &self.s;
        let mut cfg = self.ff.get_config().await?;
        let before = cfg.clone();

        let playlist = section(&mut cfg, "playlist")?;
        playlist.insert("day_start".into(), json!(s.day_start));
        playlist.insert("length".into(), json!("24:00:00"));
        playlist.insert("infinit".into(), json!(false));
        section(&mut cfg, "processing")?.insert("mode".into(), json!("playlist"));

        // ffplayout keeps one output record per mode; selecting a record by id
        // is what switches the mode.
        let mode = if s.rtmp_url.is_empty() { "hls" } else { "stream" };
        let record = self
            .ff
            .outputs()
            .await?
            .into_iter()
            .find(|o| o.get("name").and_then(Value::as_str) == Some(mode))
            .ok_or_else(|| FfError(format!("ffplayout has no {mode} output to configure")))?;
        let out = section(&mut cfg, "output")?;
        out.insert("id".into(), record.get("id").cloned().unwrap_or(Value::Null));
        out.insert("mode".into(), json!(mode));
        if mode == "stream" {
            out.insert("stream_type".into(), json!("rtmp"));
            out.insert("stream_url".into(), json!(s.rtmp_url));
        } else {
            // The config still carries the stream record's URL after a switch.
            let name = record.get("hls_playlist_name").and_then(Value::as_str).filter(|n| !n.is_empty());
            out.insert("stream_url".into(), json!(""));
            out.insert("hls_playlist_name".into(), json!(name.unwrap_or("stream")));
            out.insert("hls_segment_duration".into(), json!(s.hls_segment_seconds.max(1)));
            out.insert("hls_list_size".into(), json!(s.hls_list_size.max(3)));
        }
        out.insert("width".into(), json!(s.stream_width));
        out.insert("height".into(), json!(s.stream_height));
        out.insert("fps".into(), json!(s.stream_fps as f64));
        out.insert("audio_bitrate".into(), json!(s.audio_bitrate_kbps));
        let x264 = match out.get("video_codec") {
            None => true,
            Some(codec) => codec.as_str() == Some("libx264"),
        };
        if x264 {
            let mut options = out.get("video_options").and_then(Value::as_object).cloned().unwrap_or_default();
            options.insert("preset".into(), json!(s.x264_preset));
            options.insert("rate_control".into(), json!("cbr"));
            options.insert("maxrate".into(), json!(s.video_bitrate_kbps.to_string()));
            options.remove("quality");
            out.insert("video_options".into(), Value::Object(options));
        }
        let out_mode = out.get("mode").and_then(Value::as_str).map(str::to_string);
        let out_url = out.get("stream_url").and_then(Value::as_str).unwrap_or("").to_string();

        // The song card is drawn by the panel and shown through ffplayout's logo
        // slot (see live-logo.patch). ffplayout only accepts a logo file that exists.
        overlay::write(&overlay::blank().to_png(), &s.media_root.join(CARD_FILE))?;
        self.st().card_key = None;
        let processing = section(&mut cfg, "processing")?;
        processing.insert("add_logo".into(), json!(true));
        processing.insert("logo".into(), json!(CARD_FILE));
        processing.insert("logo_position".into(), json!("0:0"));
        processing.insert("logo_opacity".into(), json!(1.0));
        processing.insert("logo_scale".into(), json!(""));

        if !self.db.get_setting("ffplayout_defaults_applied").as_ref().is_some_and(truthy) {
            // One-time default; change it in ffplayout's UI afterwards.
            // Streamed music usually plays at about -14 LUFS; ffplayout defaults to -23.
            section(&mut cfg, "audio")?.insert("loudness_target_lufs".into(), json!(-14.0));
        }

        // Some changes (e.g. switching between HLS and RTMP) make ffplayout stop
        // the channel; bring back playout that was running.
        let was_active = self.ff.process("status").await?.as_str() == Some("active");
        let mut restart = false;
        if cfg != before {
            let result = self.ff.put_config(&cfg).await?;
            restart = result.get("requires_restart").is_some_and(truthy);
            tracing::info!("Updated ffplayout configuration");
        }
        self.db.set_setting("ffplayout_defaults_applied", &json!(true))?;

        let mut channel = self.ff.get_channel().await?;
        if channel.get("timezone").and_then(Value::as_str) != Some(s.timezone.as_str()) {
            channel["timezone"] = json!(s.timezone);
            self.ff.patch_channel(&channel).await?;
            restart = true;
            tracing::info!("Set ffplayout channel time zone to {}", s.timezone);
        }

        let mut warnings = Vec::new();
        let storage = channel.get("storage").and_then(Value::as_str).unwrap_or("");
        let media_root = s.media_root.to_string_lossy();
        if storage != media_root {
            let shown = channel.get("storage").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
            warnings.push(format!(
                "ffplayout's storage is {}, but the panel expects {media_root}. Clips will not be found.",
                shown.unwrap_or_else(|| "None".into())
            ));
        }
        {
            let mut st = self.st();
            st.status.warnings = warnings;
            st.status.mode = out_mode.clone();
            st.status.output = (out_mode.as_deref() == Some("stream") && !out_url.is_empty()).then(|| {
                let head = out_url.rsplit_once('/').map(|(h, _)| h).unwrap_or(&out_url);
                let tail: String = out_url.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                format!("{head}/••••{tail}")
            });
            st.status.watch_port = (out_mode.as_deref() == Some("hls")).then_some(s.watch_port);
        }

        if was_active {
            if self.ff.process("status").await?.as_str() != Some("active") {
                tracing::info!("Starting playout again after the configuration change");
                self.ff.process("start").await?;
            } else if restart {
                tracing::info!("Restarting playout to apply the new configuration");
                self.ff.process("restart").await?;
            }
        }
        Ok(())
    }

    // --- playout events --------------------------------------------------------

    async fn run_events(self: Arc<Self>) {
        self.bootstrap().await;
        loop {
            match self.ff.events().await {
                Ok(resp) => {
                    let mut lines = EventLines::new(resp);
                    loop {
                        match lines.next().await {
                            Ok(Some(Event::Text(text))) if text == "connected" => continue,
                            Ok(Some(Event::Text(text))) => self.events.send_replace(Some((now(), Incoming::Text(text)))),
                            Ok(Some(Event::Json(value))) => self.events.send_replace(Some((now(), Incoming::Event(value)))),
                            Ok(None) => break,
                            Err(err) => {
                                tracing::warn!("Lost ffplayout event stream: {err}");
                                break;
                            }
                        };
                    }
                }
                Err(err) => tracing::warn!("Lost ffplayout event stream: {err}"),
            }
            self.st().status.ffplayout = "reconnecting".into();
            self.events.send_replace(Some((now(), Incoming::Lost)));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn process_events(self: Arc<Self>) {
        // Separate from the reader so slow work never backs up ffplayout's
        // event queue (it drops clients whose queue fills up). Only the latest
        // event matters; older ones are skipped.
        let mut rx = self.events.subscribe();
        while rx.changed().await.is_ok() {
            let latest = rx.borrow_and_update().clone();
            let Some((received, incoming)) = latest else { continue };
            let result = match incoming {
                Incoming::Event(event) => self.on_event(&event, received).await,
                Incoming::Text(text) if text == "not running" => self.on_stopped(false).await,
                Incoming::Text(_) => Ok(()),
                Incoming::Lost => self.on_stopped(true).await,
            };
            if let Err(err) = result {
                tracing::error!("Failed to handle playout event: {err:#}");
            }
        }
    }

    async fn on_stopped(&self, connection_lost: bool) -> Result<()> {
        if connection_lost {
            // Playout may well continue; keep the current clip so a reconnect
            // doesn't count it as a new play.
            self.broadcast().await;
            return Ok(());
        }
        let (changed, running) = {
            let mut st = self.st();
            let changed = st.status.running || st.status.ffplayout != "ok";
            st.status.ffplayout = "ok".into();
            (changed, st.status.running)
        };
        if running {
            {
                let _guard = self.lock.lock().await;
                {
                    let mut st = self.st();
                    let interrupted = st
                        .current
                        .as_ref()
                        .and_then(|cur| cur.history_id.map(|h| (cur.date.clone(), cur.index, h, now())));
                    if interrupted.is_some() {
                        st.interrupted = interrupted;
                    }
                }
                self.close_history(now())?;
                let mut st = self.st();
                st.current = None;
                st.last_key = None;
            }
            self.st().status.running = false;
            tracing::info!("Playout stopped");
        }
        if changed {
            self.broadcast().await;
        }
        Ok(())
    }

    async fn on_event(&self, event: &Value, received: f64) -> Result<()> {
        {
            let mut st = self.st();
            if !st.status.running || st.status.ffplayout != "ok" {
                st.status.running = true;
                st.status.ffplayout = "ok".into();
                st.text_cleared = false;
                tracing::info!("Playout is running");
            }
        }

        let Some(index) = event.get("index").and_then(Value::as_i64) else { return Ok(()) };
        let source = event.pointer("/media/source").and_then(Value::as_str).unwrap_or("").to_string();
        let mut elapsed = event.get("elapsed").and_then(Value::as_f64).unwrap_or(0.0);
        // ffplayout compares against seconds since midnight, so clips scheduled
        // past midnight (day start other than 00:00) report a day too little.
        if elapsed < -DAY / 2.0 {
            elapsed += DAY;
        } else if elapsed > DAY / 2.0 {
            elapsed -= DAY;
        }
        let shift = event.get("shift").and_then(Value::as_f64).unwrap_or(0.0);
        // When the clip started on ffplayout's (possibly shifted) clock.
        let date_guess = self.playlist_date(received - elapsed + shift + 0.5);

        let key = (date_guess.clone(), index, source.clone());
        {
            let mut st = self.st();
            if st.last_key.as_ref() == Some(&key) {
                if let Some(cur) = st.current.as_mut() {
                    cur.elapsed = elapsed;
                    cur.received_at = received;
                    cur.shift = shift;
                    return Ok(());
                }
            }
            st.last_key = Some(key);
        }
        {
            let _guard = self.lock.lock().await;
            self.on_clip(index, &source, elapsed, shift, received, &date_guess).await?;
        }
        self.broadcast().await;
        Ok(())
    }

    fn plans(&self, date: &str) -> Result<Vec<Arc<Vec<Slot>>>> {
        let rows: Vec<(i64, String)> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare("SELECT id, slots FROM plans WHERE date = ? ORDER BY id DESC LIMIT ?")?;
            let rows = stmt.query_map(params![date, PLANS_PER_DAY], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut st = self.st();
        // Older versions of this day are no longer looked at.
        let ids: HashSet<i64> = rows.iter().map(|r| r.0).collect();
        st.plan_cache.retain(|id, (d, _)| d != date || ids.contains(id));
        Ok(rows
            .into_iter()
            .map(|(id, slots)| {
                st.plan_cache
                    .entry(id)
                    .or_insert_with(|| (date.to_string(), Arc::new(serde_json::from_str(&slots).unwrap_or_default())))
                    .1
                    .clone()
            })
            .collect())
    }

    fn match_plan(&self, index: i64, source: &str, date_guess: &str) -> Result<Option<(String, Arc<Vec<Slot>>)>> {
        let Ok(i) = usize::try_from(index) else { return Ok(None) };
        for offset in [0, -1, 1] {
            let date = Self::shift_date(date_guess, offset);
            for slots in self.plans(&date)? {
                if slots.get(i).is_some_and(|s| s.path == source) {
                    return Ok(Some((date, slots)));
                }
            }
        }
        Ok(None)
    }

    async fn on_clip(&self, index: i64, source: &str, elapsed: f64, shift: f64, received: f64, date_guess: &str) -> Result<()> {
        let matched = self.match_plan(index, source, date_guess)?;
        let prev = self.st().current.clone();
        let (date, slot, prefix) = match matched {
            Some((date, slots)) => {
                let i = index as usize;
                (date, Some(slots[i].clone()), Arc::new(slots[..=i].to_vec()))
            }
            None => (date_guess.to_string(), None, Arc::new(Vec::new())),
        };

        let started_at = received - elapsed;
        let continues = match (&slot, prev.as_ref().and_then(|p| p.slot.as_ref())) {
            (Some(slot), Some(prev_slot)) => slot.part == 1 && prev_slot.track_id == slot.track_id,
            _ => false,
        };
        let mut history_id = if continues { prev.as_ref().and_then(|p| p.history_id) } else { None };
        let interrupted = self.st().interrupted.take();
        let resumed = match (&prev, &slot, &interrupted) {
            (None, Some(_), Some((d, i, h, at))) if *d == date && *i == index && now() - at < RESUME_WINDOW => Some(*h),
            _ => None,
        };
        if let Some(h) = resumed {
            // Playout restarted in the middle of this clip: same play, not a new one.
            history_id = Some(h);
            self.db.lock().execute("UPDATE history SET ended_at = NULL WHERE id = ?", [h])?;
        } else if !continues {
            self.close_history(started_at)?;
            if let Some(slot) = slot.as_ref().filter(|s| !s.is_gap()) {
                history_id = Some(self.start_history(slot, started_at)?);
            }
        }

        let current = Current {
            date,
            index,
            slot: slot.clone(),
            prefix,
            elapsed,
            received_at: received,
            shift,
            history_id,
        };
        let managed = current.managed();
        self.st().current = Some(current);
        if managed {
            let slot = slot.expect("managed slot");
            tracing::info!("On air: {}", if slot.title.is_empty() { &slot.path } else { &slot.title });
            self.st().fresh_failures = 0;
            self.safe_rebuild().await?;
        } else {
            self.st().plan = None;
            tracing::info!("On air: {}", if source.is_empty() { "ffplayout placeholder" } else { source });
            if self.has_material()? {
                self.fresh_start("placeholder or unknown clip on air", true).await?;
            }
        }
        Ok(())
    }

    fn start_history(&self, slot: &Slot, started_at: f64) -> Result<i64> {
        let conn = self.db.lock();
        db::tx(&conn, |conn| {
            let mut requested_by: Option<String> = None;
            if let Some(queue_id) = slot.queue_id {
                requested_by = conn
                    .query_row("SELECT requested_by FROM queue WHERE id = ?", [queue_id], |r| r.get(0))
                    .optional()?
                    .flatten();
                conn.execute("DELETE FROM queue WHERE id = ?", [queue_id])?;
            }
            if let Some(auto_id) = slot.auto_id {
                conn.execute("DELETE FROM upcoming WHERE id = ?", [auto_id])?;
            }
            if let Some(track_id) = slot.track_id {
                conn.execute(
                    "UPDATE tracks SET play_count = play_count + 1, last_played_at = ? WHERE id = ?",
                    params![started_at, track_id],
                )?;
            }
            conn.execute(
                "INSERT INTO history (track_id, kind, started_at, requested_by) VALUES (?, ?, ?, ?)",
                params![slot.track_id, slot.kind, started_at, requested_by],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    fn close_history(&self, ended_at: f64) -> Result<()> {
        let (history_id, skipped) = {
            let mut st = self.st();
            let history_id = st.current.as_ref().and_then(|c| c.history_id);
            (history_id, std::mem::take(&mut st.skip_pending))
        };
        if let Some(history_id) = history_id {
            self.db.lock().execute(
                "UPDATE history SET ended_at = ?, skipped = ? WHERE id = ? AND ended_at IS NULL",
                params![ended_at, skipped as i64, history_id],
            )?;
        }
        Ok(())
    }

    // --- scheduling ------------------------------------------------------------

    fn has_material(&self) -> Result<bool> {
        let settings = self.autoplay_settings();
        has_material(&self.db.lock(), &settings)
    }

    /// Lays out today after `prefix` and all of tomorrow, picking autoplay songs as needed.
    fn layout_from(&self, prefix: Vec<Slot>) -> Result<(Vec<Slot>, Vec<Slot>)> {
        let settings = self.autoplay_settings();
        let ads = self.ads.list();
        let conn = self.db.lock();
        db::tx(&conn, |conn| {
            let mut candidates = Candidates::new(conn, &self.library, &settings)?;
            let breaks = BreakState::from_history(conn)?;
            let result = layout(prefix, WithAds::new(&mut candidates, &ads_from(conn), ads, breaks));
            match candidates.error.take() {
                Some(err) => Err(err),
                None => Ok(result),
            }
        })
    }

    async fn write_plan(&self, date: &str, slots: &[Slot]) -> Result<()> {
        let program: Vec<Value> = slots.iter().map(Slot::to_media).collect();
        let blob = serde_json::to_string(&program)?;
        let changed = self.st().written.get(date) != Some(&blob);
        if changed {
            if slots.is_empty() {
                self.ff.delete_playlist(date).await?;
            } else {
                self.ff.save_playlist(date, &program).await?;
            }
            let mut st = self.st();
            st.written.insert(date.to_string(), blob);
            // Only today's and tomorrow's playlists change; forget older days.
            let yesterday = Self::shift_date(date, -1);
            st.written.retain(|d, _| *d >= yesterday);
        }

        let data = serde_json::to_string(slots)?;
        let conn = self.db.lock();
        let latest: Option<String> = conn
            .query_row("SELECT slots FROM plans WHERE date = ? ORDER BY id DESC LIMIT 1", [date], |r| r.get(0))
            .optional()?;
        if latest.as_deref() != Some(data.as_str()) {
            conn.execute(
                "INSERT INTO plans (date, created_at, slots) VALUES (?, ?, ?)",
                params![date, now(), data],
            )?;
            conn.execute(
                "DELETE FROM plans WHERE date = ? AND id NOT IN \
                 (SELECT id FROM plans WHERE date = ? ORDER BY id DESC LIMIT ?)",
                params![date, date, PLANS_PER_DAY],
            )?;
        }
        Ok(())
    }

    async fn rebuild(&self, skip: bool) -> Result<bool> {
        let (cur, running) = {
            let st = self.st();
            (st.current.clone(), st.status.running)
        };
        let Some(cur) = cur.filter(Current::managed) else {
            if running && self.has_material()? {
                self.fresh_start("new material while idle", true).await?;
            }
            return Ok(false);
        };

        let mut prefix: Vec<Slot> = (*cur.prefix).clone();
        if skip {
            let played = cur.elapsed_now().max(0.0);
            let Some(last) = prefix.last_mut() else { return Ok(false) };
            if last.length() - played < MIN_SKIP_REMAINING {
                return Ok(false);
            }
            last.out = r3(last.seek + played + SKIP_LEAD);
            last.full_out = None;
        }

        let (today, tomorrow) = self.layout_from(prefix.clone())?;
        self.write_plan(&cur.date, &today).await?;
        self.write_plan(&Self::next_date(&cur.date), &tomorrow).await?;
        {
            let mut st = self.st();
            if let Some(c) = st.current.as_mut().filter(|c| c.date == cur.date && c.index == cur.index) {
                c.slot = prefix.last().cloned();
                c.prefix = Arc::new(prefix);
            }
            st.plan = Some(Arc::new(Plan { today, tomorrow, index: cur.index }));
        }

        if skip {
            self.st().skip_pending = true;
            self.ff.playout("reset").await?;
        }
        Ok(true)
    }

    async fn safe_rebuild(&self) -> Result<()> {
        match self.rebuild(false).await {
            Ok(_) => Ok(()),
            Err(err) if err.is::<FfError>() => {
                tracing::warn!("Could not update ffplayout's playlist: {err}");
                self.st().status.error = Some(err.to_string());
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Schedule from now on, ignoring whatever ffplayout plays.
    async fn fresh_start(&self, reason: &str, reset: bool) -> Result<()> {
        let t = now();
        let running = {
            let mut st = self.st();
            // If resyncing keeps failing (for example ffplayout can't find the
            // clips), stop interrupting the stream every few seconds.
            let cooldown = if st.fresh_failures < 3 { FRESH_START_COOLDOWN } else { 300.0 };
            if t - st.last_fresh < cooldown {
                return Ok(());
            }
            st.last_fresh = t;
            if reset && st.status.running {
                st.fresh_failures += 1;
            }
            st.status.running
        };
        let (date, pos) = (self.playlist_date(t), self.day_pos(t));
        let placeholder = pos - GAP_BACKDATE;
        let prefix = if placeholder >= MIN_PART { vec![gap(placeholder)] } else { Vec::new() };
        let index = prefix.len() as i64 - 1;

        let (today, tomorrow) = self.layout_from(prefix)?;
        if !today.iter().chain(&tomorrow).any(|s| !s.is_gap()) {
            return Ok(());
        }
        tracing::info!("Starting a fresh schedule: {reason}");
        self.write_plan(&date, &today).await?;
        self.write_plan(&Self::next_date(&date), &tomorrow).await?;
        self.st().plan = Some(Arc::new(Plan { today, tomorrow, index }));
        if reset && running {
            self.ff.playout("reset").await?;
        }
        Ok(())
    }

    /// The clip on air, the plan and whether playout runs, without holding the lock.
    fn view(&self) -> (Option<Current>, Option<Arc<Plan>>, bool) {
        let st = self.st();
        (st.current.clone(), st.plan.clone(), st.status.running)
    }

    // --- overlay ---------------------------------------------------------------

    fn track(&self, track_id: Option<i64>) -> Option<Track> {
        db::track(&self.db.lock(), track_id?).ok().flatten()
    }

    /// The card for `phase`: "now", "next" or "ad" (shown all through an ad,
    /// with the song after the break). `next` is the next song, the seconds
    /// until it starts and whether ads play before it.
    fn card_for(
        &self,
        phase: &str,
        opacity: f64,
        position: f64,
        length: f64,
        now_slot: Option<&Slot>,
        next: Option<(&Slot, f64, bool)>,
    ) -> Option<Card> {
        const AFTER_ADS: &str = "После рекламы через {time}";
        let (slot, heading, status, time_left) = match (phase, next) {
            ("ad", Some((song, until, _))) => (song, "Реклама", AFTER_ADS, until),
            ("ad", None) => (now_slot?, "Реклама", "Осталось {time}", length - position),
            ("next", Some((song, until, ads))) => {
                (song, "Далее", if ads { AFTER_ADS } else { "Начнётся через {time}" }, until)
            }
            ("next", None) => return None,
            (_, Some((_, until, _))) => (now_slot?, "Сейчас играет", "Следующая песня через {time}", until),
            (_, None) => (now_slot?, "Сейчас играет", "Осталось {time}", length - position),
        };
        let track = self.track(slot.track_id);
        Some(Card {
            heading: heading.into(),
            title: track.as_ref().map(|t| t.title.clone()).unwrap_or_else(|| slot.title.clone()),
            artist: track.as_ref().map(|t| t.artist.clone()).unwrap_or_default(),
            status: status.into(),
            time: clock(time_left),
            progress: (length > 0.0).then(|| position / length),
            thumb: track.as_ref().filter(|t| t.has_thumb).map(|t| self.library.thumb_path(t.id)),
            opacity: round_to(opacity, 2),
        })
    }

    /// The queued songs that follow the one on air, in play order, each with
    /// how long after the song on air ends it starts.
    fn queued_slots(plan: Option<&Plan>, managed: bool) -> Vec<(Slot, f64)> {
        let Some(plan) = plan else { return Vec::new() };
        let mut slots = Vec::new();
        let mut offset = 0.0;
        for slot in following(plan, managed) {
            if slot.part != 0 {
                continue; // counted in full with its first part
            }
            if slot.is_gap() {
                offset += slot.length();
                continue;
            }
            if slot.is_ad() {
                offset += slot.end() - slot.seek;
                continue;
            }
            if slot.kind != "queue" {
                break; // the queue always plays first, then autoplay
            }
            slots.push((slot.clone(), offset));
            offset += slot.end() - slot.seek;
        }
        slots
    }

    /// `remaining`: seconds until the song on air ends.
    fn queue_card(&self, slots: &[(Slot, f64)], remaining: f64, s: &OverlaySettings, opacity: f64) -> QueueCard {
        let rows = s.queue_rows.max(1) as usize;
        let items = slots
            .iter()
            .take(rows)
            .enumerate()
            .map(|(i, (slot, offset))| {
                let track = self.track(slot.track_id);
                QueueItem {
                    title: track.as_ref().map(|t| t.title.clone()).unwrap_or_else(|| slot.title.clone()),
                    artist: track.map(|t| t.artist).unwrap_or_default(),
                    time: clock(remaining + offset),
                    color: overlay::segment_color(i + 1, s),
                }
            })
            .collect();
        QueueCard { items, more: slots.len().saturating_sub(rows), opacity }
    }

    /// The song card, the progress line and the queue card to show right now.
    fn current_overlay(&self, s: &OverlaySettings) -> (Option<Card>, Option<Line>, Option<QueueCard>) {
        let (cur, plan, running) = self.view();
        let Some(cur) = cur.filter(|c| running && c.managed()) else {
            self.st().queue_sig = None;
            return (None, None, None);
        };
        let slot = cur.slot.clone().expect("managed slot");
        let plan = plan.as_deref();
        let position = slot.seek + cur.elapsed_now();
        // The next song starts where this one ends, which for a clip split at
        // midnight is the end of tomorrow's part.
        let length = slot.end();
        let nxt = next_slot(plan, true);
        let until_next = nxt.as_ref().map(|_| length - position);
        let song = next_song(plan, true, slot.out - position);
        let next = song.as_ref().map(|(song, at)| (song, *at, nxt.as_ref().is_some_and(Slot::is_ad)));
        let card = if slot.is_ad() {
            s.enabled.then(|| self.card_for("ad", 1.0, position, length, Some(&slot), next)).flatten()
        } else {
            card_phase(position, until_next, s)
                .and_then(|(phase, opacity)| self.card_for(phase, opacity, position, length, Some(&slot), next))
        };
        let queued = Self::queued_slots(plan, true);

        let opacity = line_opacity(position, until_next, length, s);
        let line = (!slot.is_ad() && opacity > 0.0 && length > 0.0).then(|| {
            let lengths = std::iter::once(length)
                .chain(queued.iter().take(LINE_MAX_QUEUED).map(|(q, _)| q.end() - q.seek))
                .collect();
            Line { lengths, position: position - position.rem_euclid(LINE_STEP), opacity }
        });

        let queue = if s.queue_card && !queued.is_empty() {
            let sig = (cur.date.clone(), cur.index, queued.iter().map(|(q, _)| q.queue_id).collect::<Vec<_>>());
            let (since, appeared) = {
                let mut st = self.st();
                if st.queue_sig.as_ref() != Some(&sig) {
                    st.queue_appeared = st.queue_sig.is_none();
                    st.queue_sig = Some(sig);
                    st.queue_since = Instant::now();
                }
                (st.queue_since, st.queue_appeared)
            };
            let level = queue_opacity(since.elapsed().as_secs_f64(), appeared, s);
            Some(self.queue_card(&queued, length - position, s, level))
        } else {
            self.st().queue_sig = None;
            None
        };
        (card, line, queue)
    }

    /// The overlay as it would look now, for the settings dialog: the song
    /// card ("now"/"next") or, for "line", the progress line, with the queue
    /// card bright beside the song card and dimmed beside the line.
    pub fn preview_card(&self, s: &OverlaySettings, phase: &str) -> Vec<u8> {
        let size = (self.s.stream_width, self.s.stream_height);
        let (cur, plan, _) = self.view();
        let managed = cur.as_ref().is_some_and(Current::managed);
        let mut queued = if managed { Self::queued_slots(plan.as_deref(), true) } else { Vec::new() };
        if queued.is_empty() {
            queued = vec![
                (Slot { title: "Заказанная песня".into(), ..Slot::new("queue", "", 0.0, 180.0, 180.0) }, 0.0),
                (Slot { title: "Ещё одна песня".into(), ..Slot::new("queue", "", 0.0, 245.0, 245.0) }, 180.0),
            ];
        }
        let queue = s.queue_card.then(|| {
            let dim = if phase == "line" { s.queue_dim_opacity } else { 1.0 };
            // Same moment as the sample song card: 64 s into a 212 s song.
            self.queue_card(&queued, 148.0, s, dim)
        });
        let image = if phase == "line" {
            let lengths = std::iter::once(212.0)
                .chain(queued.iter().take(LINE_MAX_QUEUED).map(|(q, _)| q.end() - q.seek))
                .collect();
            let line = Line { lengths, position: 64.0, opacity: 1.0 };
            overlay::compose(None, Some(&line), s, size, queue.as_ref())
        } else {
            let now_slot = cur.as_ref().filter(|_| managed).and_then(|c| c.slot.clone()).filter(|s| !s.is_ad());
            let nxt = next_song(plan.as_deref(), managed, 0.0).map(|(song, _)| song);
            let sample = Slot { title: "Исполнитель – Название песни".into(), ..Slot::new("auto", "", 0.0, 212.0, 212.0) };
            let shown_now = now_slot.clone().unwrap_or_else(|| sample.clone());
            let shown_next = nxt.or(now_slot).unwrap_or(sample);
            let card = s
                .enabled
                .then(|| self.card_for(phase, 1.0, 64.0, 212.0, Some(&shown_now), Some((&shown_next, 148.0, false))))
                .flatten();
            overlay::compose(card.as_ref(), None, s, size, queue.as_ref())
        };
        image.to_png()
    }

    async fn update_overlay(self: &Arc<Self>) -> Result<()> {
        let _guard = self.overlay_lock.lock().await;
        self.draw_overlay().await
    }

    async fn draw_overlay(self: &Arc<Self>) -> Result<()> {
        let (running, text_cleared) = {
            let st = self.st();
            (st.status.running, st.text_cleared)
        };
        if !running {
            return Ok(());
        }
        if !text_cleared {
            // The card replaced ffplayout's plain text overlay; clear any left over.
            let preset = json!({
                "channel_id": self.s.channel_id, "name": "localmusiclive", "text": "",
                "use_filename": false, "font_family": "Inter", "font_weight": "normal",
                "filename_regex": "", "position_x": "0", "position_y": "0",
                "font_size": 24.0, "line_spacing": 4.0, "text_color": "#ffffff",
                "text_opacity": 1.0, "background_enabled": false,
                "background_color": "#000000", "background_opacity": 0.0,
                "background_padding": 0, "opacity": 1.0, "scroll_direction": "none",
                "scroll_speed": 100, "scroll_repeat": -1,
                "fade_in_seconds": 0.0, "fade_out_seconds": 0.0,
            });
            match self.ff.send_text(&preset).await {
                Ok(()) => self.st().text_cleared = true,
                Err(err) => tracing::debug!("Could not clear the text overlay: {err}"),
            }
        }

        let settings = self.overlay_settings();
        let size = (self.s.stream_width, self.s.stream_height);
        let (card, line, queue) = self.current_overlay(&settings);
        let card_key = card.as_ref().map(|c| {
            (&c.heading, &c.title, &c.artist, &c.status, &c.time, c.opacity, round_to(c.progress.unwrap_or(0.0), 3), &c.thumb)
        });
        let line_key = line.as_ref().map(|l| {
            let lengths: Vec<f64> = l.lengths.iter().map(|n| round_to(*n, 1)).collect();
            (overlay::line_pixels(l, size), round_to(l.opacity, 2), lengths)
        });
        let queue_key = queue.as_ref().map(|q| (q.opacity, q.more, &q.items));
        let key = format!("{card_key:?}|{line_key:?}|{queue_key:?}|{settings:?}");
        {
            let mut st = self.st();
            if st.card_key.as_deref() == Some(key.as_str()) {
                return Ok(());
            }
            st.card_key = Some(key);
        }
        let target = self.s.media_root.join(CARD_FILE);
        tokio::task::spawn_blocking(move || {
            let image = overlay::compose(card.as_ref(), line.as_ref(), &settings, size, queue.as_ref());
            overlay::write(&image.to_png(), &target)
        })
        .await??;
        Ok(())
    }

    async fn overlay_loop(self: Arc<Self>) {
        // Ticks once per stream frame so fades and slides are smooth; the frame
        // is only redrawn when something visible changes, so outside of fades
        // the card changes once a second (countdown) and the progress line
        // every LINE_STEP.
        let fps = self.s.stream_fps.clamp(10, 60) as f64;
        let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / fps));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Err(err) = self.update_overlay().await {
                tracing::error!("Overlay update failed: {err:#}");
            }
        }
    }

    // --- commands --------------------------------------------------------------

    async fn after_change(self: &Arc<Self>) -> Result<()> {
        self.safe_rebuild().await?;
        self.update_overlay().await
    }

    pub async fn queue_add(self: &Arc<Self>, track_id: i64, play_next: bool, requested_by: Option<&str>) -> Result<i64> {
        let id = {
            let _guard = self.lock.lock().await;
            let id = {
                let conn = self.db.lock();
                let known = conn.prepare("SELECT 1 FROM tracks WHERE id = ? AND missing = 0")?.exists([track_id])?;
                if !known {
                    return Err(EngineError::new("That track is not in the library"));
                }
                let (lo, hi): (Option<i64>, Option<i64>) =
                    conn.query_row("SELECT MIN(position), MAX(position) FROM queue", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
                let position = if play_next { lo.unwrap_or(0) - 1 } else { hi.unwrap_or(0) + 1 };
                conn.execute(
                    "INSERT INTO queue (track_id, position, added_at, requested_by) VALUES (?, ?, ?, ?)",
                    params![track_id, position, now(), requested_by],
                )?;
                conn.last_insert_rowid()
            };
            self.after_change().await?;
            id
        };
        self.broadcast().await;
        Ok(id)
    }

    pub async fn queue_remove(self: &Arc<Self>, queue_id: i64) -> Result<()> {
        {
            let _guard = self.lock.lock().await;
            self.db.lock().execute("DELETE FROM queue WHERE id = ?", [queue_id])?;
            self.after_change().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    pub async fn queue_reorder(self: &Arc<Self>, ids: &[i64]) -> Result<()> {
        {
            let _guard = self.lock.lock().await;
            {
                let conn = self.db.lock();
                let mut stmt = conn.prepare("SELECT id FROM queue ORDER BY position, id")?;
                let existing: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                let mut ordered: Vec<i64> = Vec::new();
                for id in ids.iter().chain(&existing) {
                    if existing.contains(id) && !ordered.contains(id) {
                        ordered.push(*id);
                    }
                }
                db::tx(&conn, |conn| {
                    for (position, queue_id) in ordered.iter().enumerate() {
                        conn.execute("UPDATE queue SET position = ? WHERE id = ?", params![position as i64, queue_id])?;
                    }
                    Ok(())
                })?;
            }
            self.after_change().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    pub async fn queue_clear(self: &Arc<Self>) -> Result<()> {
        {
            let _guard = self.lock.lock().await;
            self.db.lock().execute("DELETE FROM queue", [])?;
            self.after_change().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    /// Deletes a song's video file and takes it out of the library.
    ///
    /// The row stays (marked missing) so history keeps its name; a later scan
    /// brings it back if the file reappears. Returns the song's name.
    pub async fn remove_track(self: &Arc<Self>, track_id: i64) -> Result<String> {
        let name = {
            let _guard = self.lock.lock().await;
            let row = {
                let conn = self.db.lock();
                db::tracks(&conn, "SELECT * FROM tracks WHERE id = ? AND missing = 0", [track_id])?.into_iter().next()
            };
            let Some(row) = row else { return Err(EngineError::new("That song is not in the library")) };
            let on_air = self.st().current.as_ref().and_then(|c| c.slot.as_ref()).is_some_and(|s| s.track_id == Some(track_id));
            if on_air {
                return Err(EngineError::new("This song is on air now. Skip it first, then remove it."));
            }

            let root = self.s.media_root.canonicalize().unwrap_or_else(|_| self.s.media_root.clone());
            let path = root.join(&row.path);
            let path = path.canonicalize().unwrap_or(path);
            let escapes = Path::new(&row.path).components().any(|c| matches!(c, std::path::Component::ParentDir));
            if escapes || !path.starts_with(&root) {
                return Err(EngineError::new("That file is outside the media folder"));
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(EngineError::new(format!("Could not delete the file: {err}"))),
            }
            let _ = std::fs::remove_file(self.library.thumb_path(track_id));

            {
                let conn = self.db.lock();
                db::tx(&conn, |conn| {
                    conn.execute("UPDATE tracks SET missing = 1, has_thumb = 0 WHERE id = ?", [track_id])?;
                    conn.execute("DELETE FROM queue WHERE track_id = ?", [track_id])?;
                    conn.execute("DELETE FROM upcoming WHERE track_id = ?", [track_id])?;
                    Ok(())
                })?;
            }
            tracing::info!("Deleted {}", row.path);
            self.after_change().await?;
            row.label()
        };
        self.broadcast().await;
        Ok(name)
    }

    pub async fn skip(self: &Arc<Self>) -> Result<()> {
        let _guard = self.lock.lock().await;
        let (cur, _, running) = self.view();
        let Some(cur) = cur.filter(|_| running) else { return Err(EngineError::new("Nothing is playing")) };
        if !cur.managed() {
            self.st().last_fresh = 0.0;
            self.fresh_start("skip", true).await?;
        } else if cur.shift.abs() > MAX_SHIFT {
            self.st().skip_pending = true;
            self.ff.playout("next").await?;
        } else if !self.rebuild(true).await? {
            return Err(EngineError::new("This clip is about to end anyway"));
        }
        Ok(())
    }

    pub async fn set_autoplay(self: &Arc<Self>, values: &Map<String, Value>) -> Result<()> {
        {
            let _guard = self.lock.lock().await;
            let settings = merged(&self.autoplay_settings(), values)?;
            self.db.set_setting("autoplay", &serde_json::to_value(&settings)?)?;
            // Re-pick with the new rules.
            self.db.lock().execute("DELETE FROM upcoming", [])?;
            self.after_change().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    /// Song levels changed: refresh libraries and the plan.
    ///
    /// Planned autoplay picks stay unless their song may no longer be picked.
    /// One song's change (`song`: track id and level) skips the library
    /// reload, which would scroll open library lists back to the top; browsers
    /// update that row instead, and the plan is only redone if the song is in it.
    pub async fn levels_changed(self: &Arc<Self>, song: Option<(i64, i64)>) -> Result<()> {
        let replan = match song {
            None => {
                self.hub.broadcast(&json!({"type": "library"}));
                true
            }
            Some((id, level)) => {
                self.hub.broadcast(&json!({"type": "level", "id": id, "level": level}));
                self.db.lock().prepare("SELECT 1 FROM upcoming WHERE track_id = ?")?.exists([id])?
            }
        };
        if replan {
            let _guard = self.lock.lock().await;
            self.after_change().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    pub async fn set_ads(self: &Arc<Self>, values: &Map<String, Value>) -> Result<()> {
        let settings = {
            let _guard = self.lock.lock().await;
            let settings = merged(&self.ad_settings(), values)?;
            self.db.set_setting("ads", &serde_json::to_value(&settings)?)?;
            self.after_change().await?;
            settings
        };
        if settings.enabled {
            self.fetch_ads();
        }
        self.broadcast().await;
        Ok(())
    }

    /// In the background: brings the commercials from advertunes.ru up to
    /// date if they are turned on, then reads the ads folder again.
    pub fn fetch_ads(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let settings = this.ad_settings();
            if settings.advertunes {
                match this.ads.fetch_advertunes(&settings).await {
                    Ok((0, 0)) => {}
                    Ok((added, removed)) => {
                        tracing::info!("advertunes.ru: {added} new commercials, {removed} older ones removed")
                    }
                    Err(err) => tracing::warn!("Could not update the commercials from advertunes.ru: {err:#}"),
                }
            }
            if this.ads.scan().await {
                let _guard = this.lock.lock().await;
                if let Err(err) = this.after_change().await {
                    tracing::error!("Could not plan the new ads: {err:#}");
                }
            }
            this.broadcast().await;
        });
    }

    pub async fn set_overlay(self: &Arc<Self>, values: &Map<String, Value>) -> Result<()> {
        {
            let _guard = self.lock.lock().await;
            let settings = merged(&self.overlay_settings(), values)?;
            self.db.set_setting("overlay", &serde_json::to_value(&settings)?)?;
            self.update_overlay().await?;
        }
        self.broadcast().await;
        Ok(())
    }

    pub async fn playout(self: &Arc<Self>, command: &str) -> Result<()> {
        let _guard = self.lock.lock().await;
        if command == "start" && !self.st().status.running {
            self.st().last_fresh = 0.0;
            if self.has_material()? {
                self.fresh_start("starting playout", false).await?;
            }
        }
        self.ff.process(command).await?;
        Ok(())
    }

    pub async fn rescan(self: &Arc<Self>) -> Result<()> {
        let result = self.library.scan().await?;
        let ads_changed = self.ads.scan().await;
        if result.changed() {
            self.hub.broadcast(&json!({"type": "library"}));
        }
        if result.changed() || ads_changed {
            {
                let _guard = self.lock.lock().await;
                self.after_change().await?;
            }
            self.broadcast().await;
        }
        Ok(())
    }

    // --- background ------------------------------------------------------------

    async fn rescan_loop(self: Arc<Self>) {
        loop {
            if let Err(err) = self.rescan().await {
                tracing::error!("Library scan failed: {err:#}");
            }
            tokio::time::sleep(Duration::from_secs(self.s.rescan_minutes.max(1) as u64 * 60)).await;
        }
    }

    async fn housekeeping(self: Arc<Self>) {
        let mut tick: u64 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            tick += 1;
            let result: Result<()> = async {
                let (cur, _, running) = self.view();
                if running && cur.is_some_and(|c| !c.managed()) {
                    let _guard = self.lock.lock().await;
                    if self.has_material()? {
                        self.fresh_start("still on a placeholder", true).await?;
                    }
                }
                if tick % 360 == 0 {
                    self.prune_plans()?;
                }
                // New commercials, once a day.
                if tick % 8640 == 0 && self.ad_settings().enabled {
                    self.fetch_ads();
                }
                self.broadcast().await;
                Ok(())
            }
            .await;
            if let Err(err) = result {
                tracing::error!("Housekeeping failed: {err:#}");
            }
        }
    }

    fn prune_plans(&self) -> Result<()> {
        let today = self.local(now()).date_naive();
        let cutoff = today.checked_sub_days(Days::new(3)).unwrap_or(today).format("%Y-%m-%d").to_string();
        let live: HashSet<i64> = {
            let conn = self.db.lock();
            conn.execute("DELETE FROM plans WHERE date < ?", [cutoff])?;
            conn.execute(
                "DELETE FROM plans WHERE id NOT IN (\
                 SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY date ORDER BY id DESC) AS n \
                 FROM plans) WHERE n <= ?)",
                [PLANS_PER_DAY],
            )?;
            let mut stmt = conn.prepare("SELECT id FROM plans")?;
            let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ids
        };
        self.st().plan_cache.retain(|id, _| live.contains(id));
        Ok(())
    }

    // --- state for the browser -------------------------------------------------

    pub fn snapshot(&self) -> Result<Value> {
        let t = now();
        let (cur, plan, status) = {
            let st = self.st();
            (st.current.clone(), st.plan.clone(), st.status.clone())
        };
        let managed = cur.as_ref().is_some_and(Current::managed);
        let following: Vec<&Slot> = plan.as_deref().map(|p| following(p, managed).collect()).unwrap_or_default();
        let downloads = self.downloads.get().and_then(Weak::upgrade).map(|y| y.state()).unwrap_or_default();

        let conn = self.db.lock();
        type QueueRow = (i64, i64, Option<String>);
        type HistoryRow = (i64, Option<i64>, String, f64, Option<f64>, bool, Option<String>);
        let queue_rows: Vec<QueueRow> = {
            let mut stmt = conn.prepare("SELECT id, track_id, requested_by FROM queue ORDER BY position, id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let history_rows: Vec<HistoryRow> = {
            let mut stmt = conn.prepare(
                "SELECT id, track_id, kind, started_at, ended_at, skipped, requested_by \
                 FROM history WHERE kind != 'ad' ORDER BY id DESC LIMIT 30",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? != 0, r.get(6)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        let mut ids: HashSet<i64> = queue_rows.iter().map(|r| r.1).collect();
        ids.extend(history_rows.iter().filter_map(|r| r.1));
        ids.extend(following.iter().take(200).filter_map(|s| s.track_id));
        if let Some(id) = cur.as_ref().and_then(|c| c.slot.as_ref()).and_then(|s| s.track_id) {
            ids.insert(id);
        }
        let tracks: HashMap<i64, Track> = if ids.is_empty() {
            HashMap::new()
        } else {
            let sql = format!("SELECT * FROM tracks WHERE id IN ({})", db::marks(ids.len()));
            db::tracks(&conn, &sql, rusqlite::params_from_iter(ids.iter()))?.into_iter().map(|t| (t.id, t)).collect()
        };
        let tj = |id: Option<i64>| -> Value { id.and_then(|id| tracks.get(&id)).map(track_json).unwrap_or(Value::Null) };

        let mut remaining = 0.0;
        let now_playing = match &cur {
            Some(c) if managed => {
                let slot = c.slot.as_ref().expect("managed slot");
                let elapsed = c.elapsed_now();
                remaining = (slot.length() - elapsed).max(0.0);
                json!({
                    "track": tj(slot.track_id),
                    "kind": slot.kind,
                    "title": slot.title,
                    "position": slot.seek + elapsed,
                    "length": slot.full_out.unwrap_or(slot.duration),
                    "remaining": remaining,
                })
            }
            Some(_) => json!({"track": null, "kind": "placeholder", "position": 0, "length": 0, "remaining": 0}),
            None => Value::Null,
        };

        let mut eta: HashMap<i64, f64> = HashMap::new();
        let mut upcoming = Vec::new();
        let mut at = remaining;
        for slot in &following {
            if !slot.is_gap() && slot.part == 0 {
                if let Some(queue_id) = slot.queue_id {
                    eta.insert(queue_id, at);
                } else if let (Some(auto_id), true) = (slot.auto_id, upcoming.len() < 10) {
                    upcoming.push(json!({"id": auto_id, "track": tj(slot.track_id), "eta": at}));
                }
            }
            at += slot.length();
            if upcoming.len() >= 10 && eta.len() >= queue_rows.len() {
                break;
            }
        }

        let nxt = next_slot(plan.as_deref(), managed);
        let current_history = cur.as_ref().and_then(|c| c.history_id);
        let history: Vec<Value> = history_rows
            .iter()
            .filter(|r| cur.is_none() || Some(r.0) != current_history)
            .map(|r| {
                json!({
                    "id": r.0,
                    "track": tj(r.1),
                    "kind": r.2,
                    "started_at": r.3,
                    "ended_at": r.4,
                    "skipped": r.5,
                    "requested_by": r.6,
                })
            })
            .collect();
        let library_size: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE missing = 0", [], |r| r.get(0))?;
        let mut ads = serde_json::to_value(ads_from(&conn))?;
        ads["clips"] = json!(self.ads.list().len());
        ads["fetching"] = json!(self.ads.fetching());

        Ok(json!({
            "server_time": t,
            "status": {
                "ffplayout": status.ffplayout,
                "running": status.running,
                "mode": status.mode,
                "output": status.output,
                "watch_port": status.watch_port,
                "error": status.error,
                "warnings": status.warnings,
                "scanning": self.library.scanning(),
                "resolution": [self.s.stream_width, self.s.stream_height],
                "library_size": library_size,
            },
            "now": now_playing,
            "next": nxt.map(|n| json!({"track": tj(n.track_id), "kind": n.kind, "title": n.title})),
            "queue": queue_rows.iter().map(|r| json!({
                "id": r.0,
                "track": tj(Some(r.1)),
                "eta": eta.get(&r.0),
                "requested_by": r.2,
            })).collect::<Vec<_>>(),
            "upcoming": upcoming,
            "history": history,
            "autoplay": autoplay_from(&conn),
            "overlay": overlay_from(&conn),
            "ads": ads,
            "downloads": downloads,
        }))
    }

    pub async fn broadcast(&self) {
        if self.hub.count() == 0 {
            return;
        }
        match self.snapshot() {
            Ok(data) => self.hub.broadcast(&json!({"type": "state", "data": data})),
            Err(err) => tracing::warn!("Could not build the state for browsers: {err:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> OverlaySettings {
        OverlaySettings::default() // start 15 s, end 20 s
    }

    fn phase(position: f64, until_next: Option<f64>, s: &OverlaySettings) -> Option<&'static str> {
        card_phase(position, until_next, s).map(|p| p.0)
    }

    #[test]
    fn now_playing_only_at_the_start() {
        assert_eq!(phase(0.6, Some(200.0), &s()), Some("now"));
        assert_eq!(phase(14.9, Some(200.0), &s()), Some("now"));
        assert_eq!(phase(15.1, Some(200.0), &s()), None);
        assert_eq!(phase(120.0, Some(100.0), &s()), None);
    }

    #[test]
    fn up_next_before_the_end() {
        assert_eq!(phase(200.0, Some(20.0), &s()), Some("next"));
        assert_eq!(phase(200.0, Some(0.5), &s()), Some("next"));
        assert_eq!(phase(200.0, Some(0.1), &s()), None);
    }

    #[test]
    fn fades() {
        assert_eq!(card_phase(0.25, Some(200.0), &s()), Some(("now", 0.5)));
        assert_eq!(card_phase(14.75, Some(200.0), &s()), Some(("now", 0.5)));
        assert_eq!(card_phase(7.0, Some(200.0), &s()), Some(("now", 1.0)));
        assert_eq!(card_phase(200.0, Some(19.75), &s()).unwrap().1, 0.5);
        assert!((card_phase(200.0, Some(0.45), &s()).unwrap().1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn nothing_next_means_no_up_next() {
        assert_eq!(phase(200.0, None, &s()), None);
        assert_eq!(phase(3.0, None, &s()), Some("now"));
        assert_eq!(phase(200.0, Some(10.0), &OverlaySettings { show_next: false, ..s() }), None);
    }

    #[test]
    fn short_song_prefers_up_next() {
        assert_eq!(phase(5.0, Some(12.0), &s()), Some("next"));
    }

    #[test]
    fn always_and_disabled() {
        assert_eq!(card_phase(100.0, Some(100.0), &OverlaySettings { mode: "always".into(), ..s() }), Some(("now", 1.0)));
        assert_eq!(phase(1.0, Some(100.0), &OverlaySettings { enabled: false, ..s() }), None);
    }

    #[test]
    fn clock_rounds_up() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(59.2), "1:00");
        assert_eq!(clock(222.0), "3:42");
    }

    #[test]
    fn line_between_the_cards() {
        // 200 s song, next one follows: hidden under both cards, crossfades at the edges.
        assert_eq!(line_opacity(5.0, Some(195.0), 200.0, &s()), 0.0);
        assert_eq!(line_opacity(14.75, Some(185.25), 200.0, &s()), 0.5);
        assert_eq!(line_opacity(100.0, Some(100.0), 200.0, &s()), 1.0);
        assert!((line_opacity(200.0 - 19.75, Some(19.75), 200.0, &s()) - 0.5).abs() < 1e-9);
        assert_eq!(line_opacity(190.0, Some(10.0), 200.0, &s()), 0.0);
    }

    #[test]
    fn line_without_up_next_runs_to_the_end() {
        assert_eq!(line_opacity(190.0, None, 200.0, &s()), 1.0);
        assert_eq!(line_opacity(190.0, Some(10.0), 200.0, &OverlaySettings { show_next: false, ..s() }), 1.0);
        assert_eq!(line_opacity(199.75, None, 200.0, &s()), 0.5);
    }

    #[test]
    fn line_without_card_and_off() {
        assert_eq!(line_opacity(3.0, Some(197.0), 200.0, &OverlaySettings { enabled: false, ..s() }), 1.0);
        assert_eq!(line_opacity(100.0, Some(100.0), 200.0, &OverlaySettings { mode: "always".into(), ..s() }), 0.0);
        assert_eq!(line_opacity(100.0, Some(100.0), 200.0, &OverlaySettings { progress_line: false, ..s() }), 0.0);
    }

    #[test]
    fn queue_card_brightens_then_dims() {
        let q = OverlaySettings { queue_bright_seconds: 8, queue_dim_opacity: 0.4, ..s() };
        assert_eq!(queue_opacity(0.0, true, &q), 0.0); // new card fades in from nothing
        assert_eq!(queue_opacity(0.0, false, &q), 0.4); // existing card brightens from dimmed
        assert_eq!(queue_opacity(0.25, true, &q), 0.5);
        assert_eq!(queue_opacity(4.0, true, &q), 1.0);
        let fading = queue_opacity(8.5, true, &q);
        assert!(0.4 < fading && fading < 1.0);
        assert_eq!(queue_opacity(30.0, true, &q), 0.4);
    }

    fn library(dir: &Path) -> (Arc<Db>, Library) {
        let db = Arc::new(Db::memory().unwrap());
        let settings = Settings { media_root: dir.to_path_buf(), data_dir: dir.join("data"), ..Settings::from_env() };
        (db.clone(), Library::new(db, Arc::new(settings)))
    }

    fn add_track(db: &Db, dir: &Path, name: &str, duration: f64) -> i64 {
        std::fs::write(dir.join(name), b"x").unwrap();
        let conn = db.lock();
        conn.execute(
            "INSERT INTO tracks (path, duration, size, mtime, added_at, title) VALUES (?, ?, 1, 0, 0, ?)",
            params![name, duration, name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn candidates_play_the_queue_then_plan_autoplay_picks() {
        let dir = tempfile::tempdir().unwrap();
        let (db, library) = library(dir.path());
        let a = add_track(&db, dir.path(), "a.mp4", 100.0);
        let b = add_track(&db, dir.path(), "b.mp4", 100.0);
        let c = add_track(&db, dir.path(), "c.mp4", 100.0);
        db.lock().execute("INSERT INTO queue (track_id, position, added_at) VALUES (?, 1, 0)", [b]).unwrap();
        let settings = AutoplaySettings { mode: "in_order".into(), avoid_repeat: 0, ..Default::default() };

        let conn = db.lock();
        let slots: Vec<Slot> = Candidates::new(&conn, &library, &settings).unwrap().take(4).collect();
        assert_eq!(slots[0].kind, "queue");
        assert_eq!(slots[0].track_id, Some(b));
        // In order after the queued song (b): c, then a, b again.
        let auto: Vec<Option<i64>> = slots[1..].iter().map(|s| s.track_id).collect();
        assert_eq!(auto, [Some(c), Some(a), Some(b)]);
        let planned: i64 = conn.query_row("SELECT COUNT(*) FROM upcoming", [], |r| r.get(0)).unwrap();
        assert_eq!(planned, 3);

        // Planned picks come back the same next time, before new ones.
        let again: Vec<Option<i64>> =
            Candidates::new(&conn, &library, &settings).unwrap().skip(1).take(3).map(|s| s.track_id).collect();
        assert_eq!(again, auto);
    }

    #[test]
    fn autoplay_leaves_out_plokho_and_favours_otlichno() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _library) = library(dir.path());
        let great = add_track(&db, dir.path(), "great.mp4", 100.0);
        let good = add_track(&db, dir.path(), "good.mp4", 100.0);
        let bad = add_track(&db, dir.path(), "bad.mp4", 100.0);
        let conn = db.lock();
        conn.execute("UPDATE tracks SET level = ? WHERE id = ?", params![levels::GREAT, great]).unwrap();
        conn.execute("UPDATE tracks SET level = ? WHERE id = ?", params![levels::BAD, bad]).unwrap();

        let (_, pool) = eligible(&conn, &AutoplaySettings::default()).unwrap();
        let mut ids: Vec<i64> = pool.iter().map(|t| t.id).collect();
        ids.sort();
        assert_eq!(ids, [great, good]);
        let every = AutoplaySettings { levels: Vec::new(), ..Default::default() };
        assert_eq!(eligible(&conn, &every).unwrap().1.len(), 3);

        let recent = VecDeque::new();
        let mut shuffle = Picker::new(pool.clone(), "shuffle", &HashMap::new());
        let greats = (0..400).filter(|_| shuffle.pick(0, &recent).id == great).count();
        assert!((240..=360).contains(&greats), "{greats} of 400");
        let mut least = Picker::new(pool, "least_played", &HashMap::new());
        let greats = (0..40).filter(|_| least.pick(0, &recent).id == great).count();
        assert!((28..=32).contains(&greats), "{greats} of 40");
    }

    #[test]
    fn candidates_skip_missing_files_and_respect_autoplay_off() {
        let dir = tempfile::tempdir().unwrap();
        let (db, library) = library(dir.path());
        let a = add_track(&db, dir.path(), "a.mp4", 100.0);
        std::fs::remove_file(dir.path().join("a.mp4")).unwrap();
        db.lock().execute("INSERT INTO queue (track_id, position, added_at) VALUES (?, 1, 0)", [a]).unwrap();
        let off = AutoplaySettings { enabled: false, ..Default::default() };
        let conn = db.lock();
        assert_eq!(Candidates::new(&conn, &library, &off).unwrap().count(), 0);
        // Still counts as material: the next library scan marks the file missing.
        assert!(has_material(&conn, &off).unwrap());
    }

    #[test]
    fn ad_breaks_continue_from_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = library(dir.path());
        let conn = db.lock();
        let add = |kind: &str| conn.execute("INSERT INTO history (kind, started_at) VALUES (?, 0)", [kind]).unwrap();
        assert_eq!(BreakState::from_history(&conn).unwrap(), BreakState::default());
        for kind in ["auto", "ad", "ad", "queue", "auto"] {
            add(kind);
        }
        assert_eq!(BreakState::from_history(&conn).unwrap(), BreakState { songs_since: 2, in_break: 0, played: 2 });
        add("ad");
        assert_eq!(BreakState::from_history(&conn).unwrap(), BreakState { songs_since: 0, in_break: 1, played: 3 });
    }

    #[test]
    fn caches_stay_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let (db, library) = library(dir.path());
        let s = Arc::new(Settings { media_root: dir.path().to_path_buf(), ..Settings::from_env() });
        let ff = Arc::new(FFplayout::new("http://127.0.0.1:9", "", "", 1));
        let ads = Arc::new(Ads::new(s.clone()));
        let engine = Engine::new(s, db.clone(), ff, Arc::new(library), ads, Hub::default());
        // Many versions of one day, each loaded once: storage and cache keep the newest.
        for i in 0..30 {
            let slots = vec![gap(100.0 + i as f64)];
            let data = serde_json::to_string(&slots).unwrap();
            db.lock()
                .execute("INSERT INTO plans (date, created_at, slots) VALUES ('2026-01-01', 0, ?)", [data])
                .unwrap();
            assert_eq!(engine.plans("2026-01-01").unwrap()[0][0].out, 100.0 + i as f64);
        }
        assert!(engine.st().plan_cache.len() <= PLANS_PER_DAY as usize);
        // Sent playlists are remembered for recent dates only.
        let mut st = engine.st();
        for day in 1..=20 {
            let date = format!("2026-01-{day:02}");
            st.written.insert(date.clone(), String::new());
            let yesterday = Engine::shift_date(&date, -1);
            st.written.retain(|d, _| *d >= yesterday);
        }
        assert_eq!(st.written.len(), 2);
    }

    #[test]
    fn settings_merge_keeps_unspecified_values() {
        let current = AutoplaySettings::default();
        let values = json!({"mode": "least_played"}).as_object().unwrap().clone();
        let next = merged(&current, &values).unwrap();
        assert_eq!(next.mode, "least_played");
        assert_eq!(next.avoid_repeat, 20);
        // Settings saved before levels: the old category filter is dropped.
        let old: AutoplaySettings = serde_json::from_value(json!({"mode": "in_order", "categories": ["Rock"]})).unwrap();
        assert_eq!((old.mode.as_str(), old.levels), ("in_order", vec![levels::GREAT, levels::GOOD]));
        let stored = json!({"start_seconds": 5});
        let overlay: OverlaySettings = serde_json::from_value(stored).unwrap();
        assert_eq!((overlay.start_seconds, overlay.end_seconds), (5, 20));
    }
}
