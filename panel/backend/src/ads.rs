//! Ad breaks: short clips from media/ads/ played between songs.
//!
//! Any video in media/ads/ is an ad; the folder is not part of the library.
//! The panel can keep it filled with the newest commercials from advertunes.ru,
//! an encyclopedia of Russian TV ads whose videos are on VK Video (yt-dlp
//! downloads them). The site doesn't say where a commercial aired, so the
//! channel logo in the recording tells (logos.rs): Первый канал and Россия 1
//! come first.
//!
//! Breaks are worked out from the play history, so a rebuild of the schedule
//! puts them in the same places: after every `every` songs, `per_break` ads.
//! The ads follow a shuffled order that only changes once all have played.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::Datelike;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::library;
use crate::logos;
use crate::settings::Settings;
use crate::timeline::Slot;
use crate::util::{round_to, run};
use crate::youtube::{download_formats, safe_filename};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdSettings {
    pub enabled: bool,
    /// An ad break after this many songs.
    pub every: i64,
    /// Ads in each break.
    pub per_break: i64,
    /// Keep the newest commercials from advertunes.ru in the folder.
    pub advertunes: bool,
    /// How many of them.
    pub keep: i64,
    /// Commercials seen on Первый канал and Россия 1 first; others only make up the number.
    pub prefer_channels: bool,
    /// Only commercials from this year and last year.
    pub new_only: bool,
}

impl Default for AdSettings {
    fn default() -> Self {
        AdSettings {
            enabled: false,
            every: 3,
            per_break: 1,
            advertunes: true,
            keep: 30,
            prefer_channels: true,
            new_only: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ad {
    /// Absolute path, as ffplayout sees it.
    pub path: String,
    pub title: String,
    pub duration: f64,
}

pub fn slot(ad: &Ad) -> Slot {
    Slot { title: ad.title.clone(), ..Slot::new("ad", &ad.path, 0.0, ad.duration, ad.duration) }
}

// --- the folder -------------------------------------------------------------------

/// "Дикси — Прилипал, хватай! (2026) [at3442].mp4" → "Дикси — Прилипал, хватай! (2026)".
fn title_from(rel_path: &str) -> String {
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let stem = match stem.rsplit_once(" [") {
        Some((name, id)) if id.ends_with(']') && !name.trim().is_empty() => name,
        _ => stem,
    };
    stem.trim().to_string()
}

async fn probe(abs_path: &str) -> Option<f64> {
    let args: Vec<String> = ["-v", "error", "-print_format", "json", "-show_format", "-show_streams", abs_path]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = run("ffprobe", &args, Duration::from_secs(60), false).await;
    if out.code != 0 {
        return None;
    }
    let probe: Value = serde_json::from_slice(&out.stdout).ok()?;
    library::describe(abs_path, &probe).map(|info| info.duration)
}

pub struct Ads {
    s: Arc<Settings>,
    http: reqwest::Client,
    /// Probe results by relative path, with the size and mtime they belong to.
    known: Mutex<HashMap<String, (i64, f64, Option<f64>)>>,
    list: Mutex<Arc<Vec<Ad>>>,
    scan_lock: tokio::sync::Mutex<()>,
    fetch_lock: tokio::sync::Mutex<()>,
}

impl Ads {
    pub fn new(s: Arc<Settings>) -> Ads {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent("localmusiclive")
            .build()
            .expect("HTTP client");
        Ads {
            s,
            http,
            known: Mutex::new(HashMap::new()),
            list: Mutex::new(Arc::new(Vec::new())),
            scan_lock: tokio::sync::Mutex::new(()),
            fetch_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn known(&self) -> MutexGuard<'_, HashMap<String, (i64, f64, Option<f64>)>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The ads, by file name.
    pub fn list(&self) -> Arc<Vec<Ad>> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn fetching(&self) -> bool {
        self.fetch_lock.try_lock().is_err()
    }

    /// Reads the folder again; true if the ads changed.
    pub async fn scan(&self) -> bool {
        let _guard = self.scan_lock.lock().await;
        let dir = self.s.ads_dir();
        let found = if dir.is_dir() { library::walk(&dir) } else { HashMap::new() };
        let mut ads = Vec::new();
        for (rel, (size, mtime)) in &found {
            let cached = self.known().get(rel).filter(|k| k.0 == *size && k.1 == *mtime).map(|k| k.2);
            let abs = dir.join(rel).to_string_lossy().into_owned();
            let duration = match cached {
                Some(duration) => duration,
                None => {
                    let duration = probe(&abs).await;
                    if duration.is_none() {
                        tracing::warn!("Not a playable ad: ads/{rel}");
                    }
                    self.known().insert(rel.clone(), (*size, *mtime, duration));
                    duration
                }
            };
            if let Some(duration) = duration.filter(|d| too_long(*d)) {
                if cached.is_none() {
                    tracing::warn!("Longer than {MAX_SECONDS} s, not played: ads/{rel} ({duration:.0} s)");
                }
            } else if let Some(duration) = duration {
                ads.push(Ad { path: abs, title: title_from(rel), duration: round_to(duration, 3) });
            }
        }
        self.known().retain(|rel, _| found.contains_key(rel));
        ads.sort_by(|a, b| a.path.cmp(&b.path));
        let mut list = self.list.lock().unwrap_or_else(|e| e.into_inner());
        if **list == ads {
            return false;
        }
        tracing::info!("{} ads in {}", ads.len(), dir.display());
        *list = Arc::new(ads);
        true
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.text().await?)
    }

    fn pages_file(&self) -> PathBuf {
        self.s.data_dir.join("advertunes.json")
    }

    fn load_pages(&self) -> BTreeMap<u64, Page> {
        std::fs::read(self.pages_file()).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
    }

    fn save_pages(&self, pages: &BTreeMap<u64, Page>) {
        let result = serde_json::to_vec(pages)
            .map_err(anyhow::Error::from)
            .and_then(|data| Ok(std::fs::write(self.pages_file(), data)?));
        if let Err(err) = result {
            tracing::warn!("Could not save what advertunes.ru pages hold: {err:#}");
        }
    }

    /// Fills the folder with `keep` commercials from advertunes.ru, newest
    /// first (and from Первый канал and Россия 1 first when preferred), and
    /// removes ones it downloaded earlier that are no longer among them.
    /// Returns how many were added and removed.
    pub async fn fetch_advertunes(&self, settings: &AdSettings) -> Result<(usize, usize)> {
        let Ok(_guard) = self.fetch_lock.try_lock() else { return Ok((0, 0)) };
        let keep = settings.keep.max(1) as usize;
        let min_year = settings.new_only.then(|| chrono::Local::now().year() - 1);
        let dir = self.s.ads_dir();
        std::fs::create_dir_all(&dir)?;
        let before: HashMap<u64, PathBuf> = std::fs::read_dir(&dir)?
            .flatten()
            .filter_map(|e| Some((advertunes_id(&e.file_name().to_string_lossy())?, e.path())))
            .collect();
        let mut on_disk = before.clone();
        let mut pages = self.load_pages();
        let mut seen: Vec<u64> = Vec::new();
        let mut downloads = 0;

        // Page ids grow as commercials are added; the listing is newest first.
        for number in 1..=MAX_PAGES {
            let url = if number == 1 { format!("{ADVERTUNES}/") } else { format!("{ADVERTUNES}/page/{number}/") };
            let mut links: Vec<(u64, String)> = Vec::new();
            for (id, link) in listing(&self.get_text(&url).await?) {
                if !seen.contains(&id) && !links.iter().any(|(n, _)| *n == id) {
                    links.push((id, link));
                }
            }
            if links.is_empty() {
                break;
            }
            links.sort_by(|a, b| b.0.cmp(&a.0));
            let mut recent = false;
            for (id, link) in links {
                seen.push(id);
                let mut page = match pages.get(&id) {
                    Some(page) => page.clone(),
                    None => {
                        tokio::time::sleep(POLITE).await;
                        match self.get_text(&link).await {
                            Ok(html) => Page::from_html(&html),
                            Err(err) => {
                                tracing::warn!("Could not read {link}: {err:#}");
                                continue;
                            }
                        }
                    }
                };
                recent |= page.is_new(min_year);
                // The channel is known once the video is here.
                if page.usable(min_year) && page.channel.is_none() && downloads < MAX_DOWNLOADS {
                    if !on_disk.contains_key(&id) {
                        downloads += 1;
                        match self.download(id, &page, &dir).await {
                            Ok(Some(path)) => {
                                on_disk.insert(id, path);
                            }
                            Ok(None) => page.too_long = true,
                            Err(err) => tracing::warn!("Could not download the ad on {link}: {err:#}"),
                        }
                    }
                    if let Some(path) = on_disk.get(&id) {
                        let duration = probe(&path.to_string_lossy()).await.unwrap_or(0.0);
                        let channel = logos::channel_of(path, duration).await.unwrap_or(OTHER);
                        page.channel = Some(channel.to_string());
                        page.duration = Some(duration);
                    }
                }
                // Remembered before lengths were: measure the file if it is here.
                if page.duration.is_none() && page.channel.is_some() {
                    if let Some(path) = on_disk.get(&id) {
                        page.duration = probe(&path.to_string_lossy()).await;
                    }
                }
                pages.insert(id, page);
                self.save_pages(&pages);
            }
            let found = seen.iter().filter_map(|id| pages.get(id)).filter(|p| p.counts(settings, min_year)).count();
            // Enough, or the listing has gone back to old commercials.
            if found >= keep || (min_year.is_some() && !recent) {
                break;
            }
            tokio::time::sleep(POLITE).await;
        }

        let wanted = choose(seen.iter().filter_map(|id| Some((*id, pages.get(id)?))), keep, settings, min_year);
        let mut added = 0;
        for id in &wanted {
            if !on_disk.contains_key(id) {
                let page = &pages[id];
                match self.download(*id, page, &dir).await {
                    Ok(Some(path)) => {
                        on_disk.insert(*id, path);
                    }
                    Ok(None) => {}
                    Err(err) => tracing::warn!("Could not download the ad {}: {err:#}", page.title),
                }
            }
            if on_disk.contains_key(id) && !before.contains_key(id) {
                tracing::info!("New ad: {} ({})", pages[id].title, pages[id].channel.as_deref().unwrap_or(OTHER));
                added += 1;
            }
        }
        let wanted: HashSet<u64> = wanted.into_iter().collect();
        let mut removed = 0;
        for (id, path) in &on_disk {
            if !wanted.contains(id) && std::fs::remove_file(path).is_ok() && before.contains_key(id) {
                removed += 1;
            }
        }
        Ok((added, removed))
    }

    /// Downloads a commercial; None if it is longer than MAX_SECONDS.
    async fn download(&self, id: u64, page: &Page, dir: &Path) -> Result<Option<PathBuf>> {
        let Some(video) = &page.video else { return Ok(None) };
        let args: Vec<String> = [
            "--no-warnings",
            "--ignore-config",
            "--no-playlist",
            "-f",
            &download_formats(self.s.stream_height as i64),
            "--merge-output-format",
            "mp4",
            "--match-filter",
            &format!("duration<={MAX_SECONDS}"),
            "--paths",
            &format!("home:{}", dir.display()),
            "--paths",
            &format!("temp:{}", dir.join(".incoming").display()),
            "-o",
            &format!(".at{id}.%(ext)s"),
            "--no-mtime",
            "--print",
            "after_move:filepath",
            video,
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        let out = run(&self.s.ytdlp, &args, Duration::from_secs(300), true).await;
        if out.code != 0 {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("yt-dlp: {}", err.trim().lines().last().unwrap_or("failed"));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Nothing printed: longer than MAX_SECONDS.
        let Some(path) = stdout.lines().map(str::trim).rfind(|l| !l.is_empty()).map(PathBuf::from) else {
            return Ok(None);
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let target = dir.join(format!("{} [at{id}].{ext}", safe_filename(&page.title)));
        tokio::fs::rename(&path, &target).await?;
        Ok(Some(target))
    }
}

/// What a commercial page on advertunes.ru holds, remembered in
/// data/advertunes.json so later runs fetch and download nothing twice.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Page {
    pub title: String,
    /// Year in the title: when the commercial is from.
    pub year: Option<i32>,
    /// VK Video page; None if the page has no video.
    pub video: Option<String>,
    /// Logo in the picture: "perviy", "rossiya1" or "other"; None until downloaded.
    pub channel: Option<String>,
    pub too_long: bool,
    /// Seconds, once downloaded.
    pub duration: Option<f64>,
}

impl Page {
    fn from_html(html: &str) -> Page {
        let title = headline(html).unwrap_or_default();
        Page { year: year_of(&title), title, video: vk_video(html), ..Page::default() }
    }

    fn is_new(&self, min_year: Option<i32>) -> bool {
        min_year.is_none_or(|min| self.year.is_some_and(|year| year >= min))
    }

    fn usable(&self, min_year: Option<i32>) -> bool {
        self.video.is_some() && !self.too_long && !self.duration.is_some_and(too_long) && self.is_new(min_year)
    }

    fn preferred(&self) -> bool {
        matches!(self.channel.as_deref(), Some(logos::PERVIY | logos::ROSSIYA1))
    }

    /// Counts towards the number to keep, without filling up with other channels.
    fn counts(&self, settings: &AdSettings, min_year: Option<i32>) -> bool {
        self.usable(min_year) && self.channel.is_some() && (!settings.prefer_channels || self.preferred())
    }
}

/// The commercials to keep: the newest, from Первый канал and Россия 1 first
/// when they are preferred, other channels only to make up the number.
fn choose<'a>(
    pages: impl Iterator<Item = (u64, &'a Page)>,
    keep: usize,
    settings: &AdSettings,
    min_year: Option<i32>,
) -> Vec<u64> {
    let mut usable: Vec<(u64, &Page)> = pages.filter(|(_, p)| p.usable(min_year) && p.channel.is_some()).collect();
    usable.sort_by(|a, b| b.0.cmp(&a.0));
    if settings.prefer_channels {
        usable.sort_by_key(|(_, p)| !p.preferred()); // stable: newest first within each group
    }
    usable.into_iter().take(keep).map(|(id, _)| id).collect()
}

// --- advertunes.ru ------------------------------------------------------------------

/// «Энциклопедия Российской ТВ рекламы»: one page per commercial, the video on VK Video.
const ADVERTUNES: &str = "https://advertunes.ru";
const MAX_PAGES: usize = 40;
/// Downloads per run to find out channels; the rest wait for the next run.
const MAX_DOWNLOADS: usize = 120;
const OTHER: &str = "other";
const POLITE: Duration = Duration::from_millis(500);
/// Ads may last a minute at most.
pub const MAX_SECONDS: f64 = 60.0;

/// Longer than an ad may be (half a second of slack for rounding).
fn too_long(duration: f64) -> bool {
    duration > MAX_SECONDS + 0.5
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

fn unescape(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"&(#\d+|#x[0-9a-fA-F]+|[a-z]+);")
        .replace_all(text, |c: &regex::Captures| {
            let entity = &c[1];
            let code = if let Some(hex) = entity.strip_prefix("#x") {
                u32::from_str_radix(hex, 16).ok()
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse().ok()
            } else {
                match entity {
                    "amp" => Some(38),
                    "quot" => Some(34),
                    "apos" => Some(39),
                    "lt" => Some(60),
                    "gt" => Some(62),
                    "nbsp" => Some(32),
                    "laquo" => Some(171),
                    "raquo" => Some(187),
                    "mdash" => Some(8212),
                    "ndash" => Some(8211),
                    _ => None,
                }
            };
            code.and_then(char::from_u32).map_or_else(|| c[0].to_string(), String::from)
        })
        .into_owned()
}

/// Commercial pages linked from a listing page: id and URL.
fn listing(html: &str) -> Vec<(u64, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Commercials sit under a one- or two-letter section (/rd/diksi/…); news under /novosti/.
    regex(&RE, r#"https://advertunes\.ru/[a-z]{1,2}/[^"'/\s]+/(\d+)-[^"'/\s]+\.html"#)
        .captures_iter(html)
        .filter_map(|c| Some((c[1].parse().ok()?, c[0].to_string())))
        .collect()
}

/// The page's VK Video player, as a video page URL yt-dlp can read (it
/// can't read the embedded player's URL).
fn vk_video(html: &str) -> Option<String> {
    static EMBED: OnceLock<Regex> = OnceLock::new();
    static IDS: OnceLock<Regex> = OnceLock::new();
    let embed = regex(&EMBED, r#"itemprop="embedUrl"\s+content="([^"]+)""#).captures(html)?;
    let embed = unescape(&embed[1]);
    let ids = regex(&IDS, r"oid=(-?\d+)&id=(\d+)|/video(-?\d+)_(\d+)").captures(&embed)?;
    let (oid, id) = match (ids.get(1), ids.get(2)) {
        (Some(oid), Some(id)) => (oid.as_str(), id.as_str()),
        _ => (ids.get(3)?.as_str(), ids.get(4)?.as_str()),
    };
    Some(format!("https://vkvideo.ru/video{oid}_{id}"))
}

/// "Реклама Дикси — Прилипал, хватай! (2026)" → "Дикси — Прилипал, хватай! (2026)".
fn headline(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let raw = regex(&RE, r#"itemprop="headline"\s+content="([^"]+)""#).captures(html)?;
    let text = unescape(&raw[1]);
    let text = text.trim();
    let text = text.strip_prefix("Реклама ").unwrap_or(text).trim();
    (!text.is_empty()).then(|| text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// "Дикси — Прилипал, хватай! (2026)" → 2026.
fn year_of(title: &str) -> Option<i32> {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\((\d{4})\)\s*$").captures(title)?[1].parse().ok()
}

/// The page id in a downloaded file's name ("… [at3442].mp4").
fn advertunes_id(file_name: &str) -> Option<u64> {
    let (_, rest) = file_name.rsplit_once(" [at")?;
    rest.split_once(']')?.0.parse().ok()
}

// --- breaks --------------------------------------------------------------------------

/// Where the last break left off, from the play history.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BreakState {
    /// Songs since the last ad.
    pub songs_since: i64,
    /// Ads at the end of the history (the break on air).
    pub in_break: i64,
    /// Ads played so far; picks the next one.
    pub played: u64,
}

impl BreakState {
    pub fn from_history(conn: &Connection) -> Result<BreakState> {
        let mut stmt = conn.prepare("SELECT kind FROM history ORDER BY id DESC LIMIT 500")?;
        let kinds: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let played: i64 = conn.query_row("SELECT COUNT(*) FROM history WHERE kind = 'ad'", [], |r| r.get(0))?;
        let in_break = kinds.iter().take_while(|k| *k == "ad").count() as i64;
        let songs_since = if in_break > 0 { 0 } else { kinds.iter().take_while(|k| *k != "ad").count() as i64 };
        Ok(BreakState { songs_since, in_break, played: played.max(0) as u64 })
    }
}

/// Play order of the ads for one round: a shuffle that never starts with the
/// ad the previous round ended on.
fn round_order(round: u64, len: usize) -> Vec<usize> {
    let shuffled = |round: u64| {
        let mut order: Vec<usize> = (0..len).collect();
        order.shuffle(&mut StdRng::seed_from_u64(round));
        order
    };
    let mut order = shuffled(round);
    if round > 0 && len > 2 && shuffled(round - 1).last() == order.first() {
        order.swap(0, 1);
    }
    order
}

pub fn pick(ads: &[Ad], played: u64) -> Option<&Ad> {
    if ads.is_empty() {
        return None;
    }
    let len = ads.len() as u64;
    let order = round_order(played / len, ads.len());
    ads.get(order[(played % len) as usize])
}

/// The songs with an ad break after every `every` of them.
pub struct WithAds<I: Iterator<Item = Slot>> {
    songs: I,
    ads: Arc<Vec<Ad>>,
    every: i64,
    per_break: i64,
    state: BreakState,
    pending: Option<Slot>,
}

impl<I: Iterator<Item = Slot>> WithAds<I> {
    pub fn new(songs: I, settings: &AdSettings, ads: Arc<Vec<Ad>>, state: BreakState) -> WithAds<I> {
        let ads = if settings.enabled { ads } else { Arc::new(Vec::new()) };
        WithAds { songs, ads, every: settings.every.max(1), per_break: settings.per_break.max(1), state, pending: None }
    }

    /// A break only goes where a song follows it.
    fn song_follows(&mut self) -> bool {
        if self.pending.is_none() {
            self.pending = self.songs.next();
        }
        self.pending.is_some()
    }
}

impl<I: Iterator<Item = Slot>> Iterator for WithAds<I> {
    type Item = Slot;

    fn next(&mut self) -> Option<Slot> {
        let st = self.state;
        let due = if st.in_break > 0 { st.in_break < self.per_break } else { st.songs_since >= self.every };
        if due && !self.ads.is_empty() && self.song_follows() {
            let ad = pick(&self.ads, st.played).expect("an ad").clone();
            self.state = BreakState { songs_since: 0, in_break: st.in_break + 1, played: st.played + 1 };
            return Some(slot(&ad));
        }
        let song = self.pending.take().or_else(|| self.songs.next())?;
        self.state = BreakState { songs_since: self.state.songs_since + 1, in_break: 0, played: self.state.played };
        Some(song)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: i64) -> Slot {
        Slot { track_id: Some(id), ..Slot::new("auto", &format!("/media/{id}.mp4"), 0.0, 100.0, 100.0) }
    }

    fn ads(n: usize) -> Arc<Vec<Ad>> {
        Arc::new((0..n).map(|i| Ad { path: format!("/media/ads/{i}.mp4"), title: format!("Ad {i}"), duration: 10.0 }).collect())
    }

    fn on(every: i64, per_break: i64) -> AdSettings {
        AdSettings { enabled: true, every, per_break, ..AdSettings::default() }
    }

    fn kinds(slots: impl Iterator<Item = Slot>) -> String {
        slots.map(|s| if s.kind == "ad" { 'A' } else { 's' }).collect()
    }

    #[test]
    fn a_break_after_every_n_songs() {
        let it = WithAds::new((0..7).map(song), &on(3, 2), ads(3), BreakState::default());
        assert_eq!(kinds(it), "sssAAsssAAs");
    }

    #[test]
    fn continues_from_the_history() {
        // Two songs played since the last break: one more, then ads.
        let state = BreakState { songs_since: 2, in_break: 0, played: 5 };
        assert_eq!(kinds(WithAds::new((0..3).map(song), &on(3, 1), ads(3), state)), "sAss");
        // First of two ads on air: the second one comes next.
        let state = BreakState { songs_since: 0, in_break: 1, played: 5 };
        assert_eq!(kinds(WithAds::new((0..2).map(song), &on(3, 2), ads(3), state)), "Ass");
    }

    #[test]
    fn no_break_without_a_song_after_it_or_without_ads() {
        assert_eq!(kinds(WithAds::new((0..3).map(song), &on(3, 1), ads(2), BreakState::default())), "sss");
        assert_eq!(kinds(WithAds::new((0..4).map(song), &on(1, 1), ads(0), BreakState::default())), "ssss");
        let off = AdSettings { enabled: false, ..on(1, 1) };
        assert_eq!(kinds(WithAds::new((0..4).map(song), &off, ads(2), BreakState::default())), "ssss");
    }

    #[test]
    fn same_plan_on_every_rebuild() {
        let state = BreakState { songs_since: 1, in_break: 0, played: 11 };
        let plan = |_| WithAds::new((0..20).map(song), &on(2, 2), ads(4), state).map(|s| s.path).collect::<Vec<_>>();
        assert_eq!(plan(0), plan(1));
    }

    #[test]
    fn every_ad_once_per_round_and_no_repeat_between_rounds() {
        let list = ads(5);
        let played: Vec<&str> = (0..50).map(|n| pick(&list, n).unwrap().path.as_str()).collect();
        for round in played.chunks(5) {
            assert_eq!(round.iter().collect::<HashSet<_>>().len(), 5);
        }
        assert!(played.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn titles_from_file_names() {
        assert_eq!(title_from("Дикси — Прилипал, хватай! (2026) [at3442].mp4"), "Дикси — Прилипал, хватай! (2026)");
        assert_eq!(title_from("sub/Cola.mov"), "Cola");
        assert_eq!(advertunes_id("Дикси — Прилипал [at3442].mp4"), Some(3442));
        assert_eq!(advertunes_id("Cola [abc].mp4"), None);
        assert_eq!(advertunes_id("Cola.mp4"), None);
    }

    #[test]
    fn reads_advertunes_pages() {
        let list = r#"<a href="https://advertunes.ru/rd/diksi/3442-diksi_prilipal_hvatay.html">
            <a href="https://advertunes.ru/novosti/3427-avtomatizacija.html">
            <a href="https://advertunes.ru/m/m_m_39_s/1823-ememdems_mms_eksperiment.html">"#;
        let ids: Vec<u64> = listing(list).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, [3442, 1823]);

        let page = r#"<meta itemprop="headline" content="Реклама Эмемдемс (M&amp;Ms) — Эксперимент (2020)">
            <meta itemprop="embedUrl" content="https://vkvideo.ru/video_ext.php?oid=-4024629&amp;id=456248723&amp;hash=4da2&amp;hd=3">"#;
        assert_eq!(headline(page).as_deref(), Some("Эмемдемс (M&Ms) — Эксперимент (2020)"));
        assert_eq!(vk_video(page).as_deref(), Some("https://vkvideo.ru/video-4024629_456248723"));
        let page = r#"<meta itemprop="embedUrl" content="https://vkvideo.ru/video-4024629_456252360">"#;
        assert_eq!(vk_video(page).as_deref(), Some("https://vkvideo.ru/video-4024629_456252360"));
        assert_eq!(vk_video("<p>no video</p>"), None);
        assert_eq!(unescape("a &#171;b&#187; &quot;c&quot; &unknown;"), "a «b» \"c\" &unknown;");
    }

    fn page(year: i32, channel: &str) -> Page {
        Page {
            title: format!("Ad ({year})"),
            year: Some(year),
            video: Some("https://vkvideo.ru/video-1_2".into()),
            channel: Some(channel.into()),
            too_long: false,
            duration: Some(30.0),
        }
    }

    #[test]
    fn prefers_perviy_and_rossiya1_then_fills_with_others() {
        let pages = vec![
            (10, page(2026, "other")),
            (9, page(2026, "perviy")),
            (8, page(2019, "rossiya1")),
            (7, page(2026, "rossiya1")),
            (6, page(2026, "other")),
            (5, Page { video: None, ..page(2026, "perviy") }),
            (4, Page { too_long: true, ..page(2026, "perviy") }),
            (2, Page { duration: Some(75.0), ..page(2026, "perviy") }),
            (1, Page { duration: Some(60.4), ..page(2026, "other") }),
            (3, Page { channel: None, ..page(2026, "perviy") }),
        ];
        let all = || pages.iter().map(|(id, p)| (*id, p));
        let s = AdSettings::default();
        assert_eq!(choose(all(), 3, &s, Some(2025)), [9, 7, 10]);
        assert_eq!(choose(all(), 10, &s, Some(2025)), [9, 7, 10, 6, 1]);
        assert_eq!(choose(all(), 10, &s, None), [9, 8, 7, 10, 6, 1]);
        let any = AdSettings { prefer_channels: false, ..s.clone() };
        assert_eq!(choose(all(), 2, &any, Some(2025)), [10, 9]);
        assert!(pages[1].1.counts(&s, Some(2025)) && !pages[0].1.counts(&s, Some(2025)));
        assert!(pages[0].1.counts(&any, Some(2025)));
    }

    #[test]
    fn years_from_titles() {
        assert_eq!(year_of("Дикси — Прилипал, хватай! (2026)"), Some(2026));
        assert_eq!(year_of("МТС — УЖЕ 5G ( Дмитрий Нагиев) (2026)"), Some(2026));
        assert_eq!(year_of("Без года"), None);
        let html = r#"<meta itemprop="headline" content="Реклама Чудо - Воздушное Чудо (2007)">"#;
        assert_eq!(Page::from_html(html).year, Some(2007));
    }
}
