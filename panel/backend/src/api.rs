//! The HTTP API and the browser WebSocket.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use regex::Regex;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::ServeDir;

use crate::categories::{categories_of, clean_categories, fold, to_json};
use crate::db::{self, Db};
use crate::engine::{merged, track_json, Engine, EngineError};
use crate::ffplayout::FfError;
use crate::hub::Hub;
use crate::library::Library;
use crate::overlay::POSITIONS;
use crate::settings::Settings;
use crate::youtube::YouTube;

#[derive(Clone)]
pub struct App {
    pub s: Arc<Settings>,
    pub db: Arc<Db>,
    pub library: Arc<Library>,
    pub engine: Arc<Engine>,
    pub youtube: Arc<YouTube>,
    pub hub: Hub,
}

// --- errors ---------------------------------------------------------------------

/// An error answer: `{"detail": "..."}` with a status code.
pub struct ApiError(StatusCode, String);

impl ApiError {
    fn invalid(message: impl Into<String>) -> ApiError {
        ApiError(StatusCode::UNPROCESSABLE_ENTITY, message.into())
    }

    fn not_found(message: impl Into<String>) -> ApiError {
        ApiError(StatusCode::NOT_FOUND, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> ApiError {
        if let Some(e) = err.downcast_ref::<EngineError>() {
            return ApiError(StatusCode::CONFLICT, e.0.clone());
        }
        if let Some(e) = err.downcast_ref::<FfError>() {
            return ApiError(StatusCode::BAD_GATEWAY, e.0.clone());
        }
        tracing::error!("Request failed: {err:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> ApiError {
        anyhow::Error::from(err).into()
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

// --- input checks -----------------------------------------------------------------

enum Rule {
    Bool,
    Int(i64, i64),
    Float(f64, f64),
    Choice(&'static [&'static str]),
    Color,
    Text(usize),
    TextList(usize),
    IntList,
}

static COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());

/// A number, or text holding one (Pydantic accepted both).
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn check(key: &str, value: &Value, rule: &Rule) -> Result<Value, ApiError> {
    let bad = |what: &str| ApiError::invalid(format!("{key}: {what}"));
    match rule {
        Rule::Bool => value.as_bool().map(Value::from).ok_or_else(|| bad("must be true or false")),
        Rule::Int(lo, hi) => {
            let n = number(value)
                .filter(|f| f.fract() == 0.0)
                .map(|f| f as i64)
                .ok_or_else(|| bad("must be a whole number"))?;
            if n < *lo || n > *hi {
                return Err(bad(&format!("must be between {lo} and {hi}")));
            }
            Ok(Value::from(n))
        }
        Rule::Float(lo, hi) => {
            let n = number(value).ok_or_else(|| bad("must be a number"))?;
            if n < *lo || n > *hi {
                return Err(bad(&format!("must be between {lo} and {hi}")));
            }
            Ok(Value::from(n))
        }
        Rule::Choice(options) => match value.as_str() {
            Some(s) if options.contains(&s) => Ok(Value::from(s)),
            _ => Err(bad(&format!("must be one of {}", options.join(", ")))),
        },
        Rule::Color => match value.as_str() {
            Some(s) if COLOR.is_match(s) => Ok(Value::from(s)),
            _ => Err(bad("must be a colour like #ff5c7a")),
        },
        Rule::Text(max) => match value.as_str() {
            Some(s) if s.chars().count() <= *max => Ok(Value::from(s)),
            Some(_) => Err(bad(&format!("must be at most {max} characters"))),
            None => Err(bad("must be text")),
        },
        Rule::TextList(max) => {
            let items = value.as_array().ok_or_else(|| bad("must be a list"))?;
            if items.len() > *max {
                return Err(bad(&format!("must have at most {max} items")));
            }
            if items.iter().any(|i| !i.is_string()) {
                return Err(bad("must be a list of text"));
            }
            Ok(value.clone())
        }
        Rule::IntList => {
            let items = value.as_array().ok_or_else(|| bad("must be a list"))?;
            if items.iter().any(|i| i.as_i64().is_none()) {
                return Err(bad("must be a list of whole numbers"));
            }
            Ok(value.clone())
        }
    }
}

/// The known, non-null fields of a JSON body, checked; others are ignored.
fn clean(body: &Value, rules: &[(&'static str, Rule)]) -> Result<Map<String, Value>, ApiError> {
    let obj = body.as_object().ok_or_else(|| ApiError::invalid("Expected a JSON object"))?;
    let mut out = Map::new();
    for (key, rule) in rules {
        if let Some(value) = obj.get(*key).filter(|v| !v.is_null()) {
            out.insert(key.to_string(), check(key, value, rule)?);
        }
    }
    Ok(out)
}

fn parse_body(bytes: &Bytes) -> Result<Value, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::invalid(format!("Invalid JSON: {e}")))
}

fn required<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a Value, ApiError> {
    map.get(key).ok_or_else(|| ApiError::invalid(format!("{key}: field required")))
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or("")
}

fn path_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse().map_err(|_| ApiError::invalid(format!("{raw:?} is not a number")))
}

fn query_int(q: &HashMap<String, String>, key: &str, default: i64, lo: i64, hi: i64) -> Result<i64, ApiError> {
    let Some(raw) = q.get(key) else { return Ok(default) };
    let n: i64 = raw.trim().parse().map_err(|_| ApiError::invalid(format!("{key}: must be a whole number")))?;
    if n < lo || n > hi {
        return Err(ApiError::invalid(format!("{key}: must be between {lo} and {hi}")));
    }
    Ok(n)
}

fn query_bool(q: &HashMap<String, String>, key: &str) -> Result<bool, ApiError> {
    match q.get(key).map(|v| v.to_lowercase()) {
        None => Ok(false),
        Some(v) if ["1", "true", "yes", "on", "t", "y"].contains(&v.as_str()) => Ok(true),
        Some(v) if ["0", "false", "no", "off", "f", "n"].contains(&v.as_str()) => Ok(false),
        Some(_) => Err(ApiError::invalid(format!("{key}: must be true or false"))),
    }
}

// --- routes -------------------------------------------------------------------------

pub fn router(app: App) -> Router {
    let static_dir = app.s.static_dir.clone();
    let mut router = Router::new()
        .route("/api/state", get(state))
        .route("/api/health", get(health))
        .route("/ws", get(websocket))
        .route("/api/library", get(list_library))
        .route("/api/library/folders", get(list_folders))
        .route("/api/categories", get(list_categories))
        .route("/api/tracks/{track_id}/categories", put(put_track_categories))
        .route("/api/categories/bulk", post(bulk_categories))
        .route("/api/library/rescan", post(rescan))
        .route("/api/tracks/{track_id}", delete(delete_track))
        .route("/api/thumbs/{file}", get(thumb))
        .route("/api/queue", post(queue_add).put(queue_reorder).delete(queue_clear))
        .route("/api/queue/{queue_id}", delete(queue_remove))
        .route("/api/skip", post(skip))
        .route("/api/youtube/search", get(youtube_search))
        .route("/api/youtube/suggestions", get(youtube_suggestions))
        .route("/api/youtube/downloads", post(youtube_download))
        .route("/api/youtube/downloads/{job_id}", delete(youtube_dismiss))
        .route("/api/playout/{command}", post(playout))
        .route("/api/settings/autoplay", put(put_autoplay))
        .route("/api/settings/overlay", put(put_overlay))
        .route("/api/overlay/preview", post(overlay_preview))
        .route("/api/history", get(history))
        .with_state(app);
    if static_dir.is_dir() {
        router = router.fallback_service(ServeDir::new(static_dir));
    }
    router
}

// --- state ----------------------------------------------------------------------------

async fn state(State(app): State<App>) -> ApiResult {
    Ok(Json(app.engine.snapshot()?))
}

async fn health(State(app): State<App>) -> Json<Value> {
    let status = app.engine.status();
    Json(json!({"ok": true, "ffplayout": status.ffplayout, "running": status.running}))
}

async fn websocket(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| serve_socket(socket, app))
}

async fn send(socket: &mut WebSocket, text: String) -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(5), socket.send(Message::Text(text.into()))).await, Ok(Ok(())))
}

async fn serve_socket(mut socket: WebSocket, app: App) {
    let mut rx = app.hub.subscribe();
    let first = match app.engine.snapshot() {
        Ok(data) => json!({"type": "state", "data": data}).to_string(),
        Err(err) => {
            tracing::warn!("Could not build the state for a browser: {err:#}");
            return;
        }
    };
    if !send(&mut socket, first).await {
        return;
    }
    loop {
        tokio::select! {
            message = rx.recv() => match message {
                Ok(text) => {
                    if !send(&mut socket, text.to_string()).await {
                        break;
                    }
                }
                // Too slow to keep up: the next state message brings it back in step.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

// --- library --------------------------------------------------------------------------

const UNCATEGORIZED: &str = "__none__";

fn sort_clause(sort: &str) -> Option<&'static str> {
    Some(match sort {
        "artist" => "artist COLLATE NOCASE, title COLLATE NOCASE",
        "title" => "title COLLATE NOCASE, artist COLLATE NOCASE",
        "added" => "added_at DESC",
        "played" => "play_count DESC, last_played_at DESC",
        "recent" => "last_played_at IS NULL, last_played_at DESC",
        _ => return None,
    })
}

fn library_filter(q: &str, folder: &str, category: &str) -> (String, Vec<String>) {
    let mut clauses = vec!["missing = 0".to_string()];
    let mut args = Vec::new();
    for word in q.split_whitespace() {
        let like = format!("%{}%", word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        clauses.push(
            "(artist LIKE ? ESCAPE '\\' OR title LIKE ? ESCAPE '\\' \
             OR album LIKE ? ESCAPE '\\' OR path LIKE ? ESCAPE '\\' OR categories LIKE ? ESCAPE '\\')"
                .into(),
        );
        args.extend(std::iter::repeat_n(like, 5));
    }
    let folder = folder.trim_matches('/');
    if !folder.is_empty() {
        clauses.push("(folder = ? OR folder LIKE ?)".into());
        args.push(folder.to_string());
        args.push(format!("{folder}/%"));
    }
    if category == UNCATEGORIZED {
        clauses.push("json_array_length(categories) = 0".into());
    } else if !category.is_empty() {
        clauses.push("EXISTS (SELECT 1 FROM json_each(tracks.categories) WHERE value = ? COLLATE NOCASE)".into());
        args.push(category.to_string());
    }
    (clauses.join(" AND "), args)
}

async fn list_library(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    let sort = match q.get("sort") {
        None => "artist",
        Some(s) => s.as_str(),
    };
    let order = sort_clause(sort)
        .ok_or_else(|| ApiError::invalid("sort: must be one of artist, title, added, played, recent"))?;
    let limit = query_int(&q, "limit", 100, 1, 500)?;
    let offset = query_int(&q, "offset", 0, 0, i64::MAX)?;
    let (clause, args) = library_filter(get("q"), get("folder"), get("category"));

    let conn = app.db.lock();
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM tracks WHERE {clause}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    let mut all_args: Vec<rusqlite::types::Value> = args.into_iter().map(Into::into).collect();
    all_args.push(limit.into());
    all_args.push(offset.into());
    let rows = db::tracks(
        &conn,
        &format!("SELECT * FROM tracks WHERE {clause} ORDER BY {order} LIMIT ? OFFSET ?"),
        rusqlite::params_from_iter(all_args.iter()),
    )?;
    Ok(Json(json!({"total": total, "items": rows.iter().map(track_json).collect::<Vec<_>>()})))
}

async fn list_folders(State(app): State<App>) -> ApiResult {
    let conn = app.db.lock();
    let mut stmt = conn.prepare("SELECT DISTINCT folder FROM tracks WHERE missing = 0 AND folder != ''")?;
    let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut folders = BTreeSet::new();
    for folder in rows {
        let parts: Vec<&str> = folder.split('/').collect();
        for i in 0..parts.len() {
            folders.insert(parts[..=i].join("/"));
        }
    }
    let mut folders: Vec<String> = folders.into_iter().collect();
    folders.sort_by_key(|f| f.to_lowercase());
    Ok(Json(json!(folders)))
}

/// Songs per category, named as first seen.
fn category_counts(conn: &rusqlite::Connection) -> Result<Vec<(String, i64)>, ApiError> {
    let mut stmt = conn.prepare("SELECT categories FROM tracks WHERE missing = 0 AND categories != '[]'")?;
    let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut order: Vec<String> = Vec::new();
    let mut counts: HashMap<String, (String, i64)> = HashMap::new();
    for row in rows {
        for name in categories_of(&row) {
            let key = fold(&name);
            let entry = counts.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                (name.clone(), 0)
            });
            entry.1 += 1;
        }
    }
    Ok(order.into_iter().filter_map(|k| counts.remove(&k)).collect())
}

async fn list_categories(State(app): State<App>) -> ApiResult {
    let mut counts = category_counts(&app.db.lock())?;
    counts.sort_by_key(|(name, _)| fold(name));
    Ok(Json(json!(counts.iter().map(|(name, count)| json!({"name": name, "count": count})).collect::<Vec<_>>())))
}

async fn put_track_categories(State(app): State<App>, Path(track_id): Path<String>, body: Bytes) -> ApiResult {
    let track_id = path_id(&track_id)?;
    let body = clean(&parse_body(&body)?, &[("categories", Rule::TextList(50))])?;
    let values: Vec<String> = required(&body, "categories")?
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let names = {
        let conn = app.db.lock();
        if db::track(&conn, track_id)?.is_none() {
            return Err(ApiError::not_found("That track is not in the library"));
        }
        let existing: Vec<String> = category_counts(&conn)?.into_iter().map(|c| c.0).collect();
        let names = clean_categories(values.iter().map(String::as_str), existing.iter().map(String::as_str));
        conn.execute("UPDATE tracks SET categories = ? WHERE id = ?", rusqlite::params![to_json(&names), track_id])?;
        names
    };
    app.engine.categories_changed(false).await?;
    Ok(Json(json!({"categories": names})))
}

/// Add or remove one category on every song matching the library filter.
async fn bulk_categories(State(app): State<App>, body: Bytes) -> ApiResult {
    let body = clean(
        &parse_body(&body)?,
        &[
            ("q", Rule::Text(10_000)),
            ("folder", Rule::Text(10_000)),
            ("category", Rule::Text(10_000)),
            ("add", Rule::Text(10_000)),
            ("remove", Rule::Text(10_000)),
        ],
    )?;
    let changed = {
        let conn = app.db.lock();
        let existing: Vec<String> = category_counts(&conn)?.into_iter().map(|c| c.0).collect();
        let add = clean_categories([text(&body, "add")], existing.iter().map(String::as_str));
        let remove = fold(text(&body, "remove").trim());
        if add.is_empty() && remove.is_empty() {
            return Err(ApiError::invalid("Name a category to add or remove"));
        }
        let (clause, args) = library_filter(text(&body, "q"), text(&body, "folder"), text(&body, "category"));
        db::tx(&conn, |conn| {
            let mut stmt = conn.prepare(&format!("SELECT id, categories FROM tracks WHERE {clause}"))?;
            let rows: Vec<(i64, String)> = stmt
                .query_map(rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut changed = 0;
            for (id, categories) in rows {
                let names = categories_of(&categories);
                let mut updated: Vec<String> = if remove.is_empty() {
                    names.clone()
                } else {
                    names.iter().filter(|n| fold(n) != remove).cloned().collect()
                };
                if let Some(new) = add.first() {
                    if updated.iter().all(|n| fold(n) != fold(new)) {
                        updated.push(new.clone());
                        updated = clean_categories(updated.iter().map(String::as_str), std::iter::empty());
                    }
                }
                if updated != names {
                    conn.execute("UPDATE tracks SET categories = ? WHERE id = ?", rusqlite::params![to_json(&updated), id])?;
                    changed += 1;
                }
            }
            Ok(changed)
        })?
    };
    if changed > 0 {
        app.engine.categories_changed(true).await?;
    }
    Ok(Json(json!({"changed": changed})))
}

async fn rescan(State(app): State<App>) -> (StatusCode, Json<Value>) {
    if app.library.scanning() {
        return (StatusCode::ACCEPTED, Json(json!({"started": false})));
    }
    let engine = app.engine.clone();
    tokio::spawn(async move {
        if let Err(err) = engine.rescan().await {
            tracing::error!("Library scan failed: {err:#}");
        }
    });
    (StatusCode::ACCEPTED, Json(json!({"started": true})))
}

/// Deletes the song's video file from the media folder.
async fn delete_track(State(app): State<App>, Path(track_id): Path<String>) -> ApiResult {
    let removed = app.engine.remove_track(path_id(&track_id)?).await?;
    Ok(Json(json!({"removed": removed})))
}

async fn thumb(State(app): State<App>, Path(file): Path<String>) -> Result<Response, ApiError> {
    let id = file.strip_suffix(".jpg").ok_or_else(|| ApiError::not_found("Not Found"))?;
    let path = app.library.thumb_path(path_id(id)?);
    let bytes = tokio::fs::read(path).await.map_err(|_| ApiError::not_found("Not Found"))?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "max-age=604800")], bytes).into_response())
}

// --- queue ----------------------------------------------------------------------------

async fn queue_add(State(app): State<App>, body: Bytes) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = clean(&parse_body(&body)?, &[("track_id", Rule::Int(i64::MIN, i64::MAX)), ("play_next", Rule::Bool)])?;
    let track_id = required(&body, "track_id")?.as_i64().unwrap_or_default();
    let play_next = body.get("play_next").and_then(Value::as_bool).unwrap_or(false);
    let id = app.engine.queue_add(track_id, play_next, None).await?;
    Ok((StatusCode::CREATED, Json(json!({"id": id}))))
}

async fn queue_reorder(State(app): State<App>, body: Bytes) -> ApiResult {
    let body = clean(&parse_body(&body)?, &[("ids", Rule::IntList)])?;
    let ids: Vec<i64> = required(&body, "ids")?.as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    app.engine.queue_reorder(&ids).await?;
    Ok(Json(json!({"ok": true})))
}

async fn queue_remove(State(app): State<App>, Path(queue_id): Path<String>) -> ApiResult {
    app.engine.queue_remove(path_id(&queue_id)?).await?;
    Ok(Json(json!({"ok": true})))
}

async fn queue_clear(State(app): State<App>) -> ApiResult {
    app.engine.queue_clear().await?;
    Ok(Json(json!({"ok": true})))
}

async fn skip(State(app): State<App>) -> ApiResult {
    app.engine.skip().await?;
    Ok(Json(json!({"ok": true})))
}

// --- YouTube --------------------------------------------------------------------------

async fn youtube_search(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let query = q.get("q").map(String::as_str).unwrap_or("");
    let length = query.chars().count();
    if !(1..=200).contains(&length) {
        return Err(ApiError::invalid("q: must be 1 to 200 characters"));
    }
    match app.youtube.search(query).await {
        Ok(results) => Ok(Json(json!(results))),
        Err(err) => Err(ApiError(StatusCode::BAD_GATEWAY, format!("YouTube search failed: {err}"))),
    }
}

async fn youtube_suggestions(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    Ok(Json(app.youtube.suggestions(query_bool(&q, "refresh")?).await?))
}

async fn youtube_download(State(app): State<App>, body: Bytes) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body = clean(
        &parse_body(&body)?,
        &[("video_id", Rule::Text(11)), ("then", Rule::Choice(&["none", "queue", "next"])), ("title", Rule::Text(300))],
    )?;
    let video_id = required(&body, "video_id")?.as_str().unwrap_or("");
    if !crate::youtube::VIDEO_ID.is_match(video_id) {
        return Err(ApiError::invalid("video_id: not a YouTube video id"));
    }
    let then = body.get("then").and_then(Value::as_str).unwrap_or("queue");
    let title = body.get("title").and_then(Value::as_str);
    let job = app.youtube.download(video_id, then, title).await?;
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn youtube_dismiss(State(app): State<App>, Path(job_id): Path<String>) -> ApiResult {
    app.youtube.dismiss(path_id(&job_id)?).await;
    Ok(Json(json!({"ok": true})))
}

// --- playout and settings ---------------------------------------------------------------

async fn playout(State(app): State<App>, Path(command): Path<String>) -> ApiResult {
    if !["start", "stop", "restart"].contains(&command.as_str()) {
        return Err(ApiError::invalid("command: must be start, stop or restart"));
    }
    app.engine.playout(&command).await?;
    Ok(Json(json!({"ok": true})))
}

fn autoplay_rules() -> Vec<(&'static str, Rule)> {
    vec![
        ("enabled", Rule::Bool),
        ("mode", Rule::Choice(&["shuffle", "least_played", "in_order"])),
        ("folder", Rule::Text(10_000)),
        ("categories", Rule::TextList(50)),
        ("avoid_repeat", Rule::Int(0, 500)),
    ]
}

fn overlay_rules() -> Vec<(&'static str, Rule)> {
    vec![
        ("enabled", Rule::Bool),
        ("mode", Rule::Choice(&["start_end", "always"])),
        ("start_seconds", Rule::Int(0, 600)),
        ("end_seconds", Rule::Int(0, 600)),
        ("show_next", Rule::Bool),
        ("position", Rule::Choice(&POSITIONS)),
        ("margin", Rule::Int(0, 400)),
        ("scale", Rule::Int(50, 250)),
        ("accent_color", Rule::Color),
        ("background_color", Rule::Color),
        ("background_opacity", Rule::Float(0.0, 1.0)),
        ("stroke", Rule::Bool),
        ("stroke_color", Rule::Color),
        ("stroke_opacity", Rule::Float(0.0, 1.0)),
        ("stroke_width", Rule::Float(0.5, 8.0)),
        ("radius", Rule::Int(0, 60)),
        ("shadow", Rule::Bool),
        ("show_thumbnail", Rule::Bool),
        ("show_progress", Rule::Bool),
        ("progress_line", Rule::Bool),
        ("line_height", Rule::Int(1, 20)),
        ("queue_card", Rule::Bool),
        ("queue_rows", Rule::Int(1, 8)),
        ("queue_bright_seconds", Rule::Int(0, 600)),
        ("queue_dim_opacity", Rule::Float(0.0, 1.0)),
    ]
}

async fn put_autoplay(State(app): State<App>, body: Bytes) -> ApiResult {
    let values = clean(&parse_body(&body)?, &autoplay_rules())?;
    app.engine.set_autoplay(&values).await?;
    Ok(Json(serde_json::to_value(app.engine.autoplay_settings()).map_err(anyhow::Error::from)?))
}

async fn put_overlay(State(app): State<App>, body: Bytes) -> ApiResult {
    let values = clean(&parse_body(&body)?, &overlay_rules())?;
    app.engine.set_overlay(&values).await?;
    Ok(Json(serde_json::to_value(app.engine.overlay_settings()).map_err(anyhow::Error::from)?))
}

async fn overlay_preview(State(app): State<App>, body: Bytes) -> Result<Response, ApiError> {
    let body = parse_body(&body)?;
    let mut rules = overlay_rules();
    rules.push(("card", Rule::Choice(&["now", "next", "line"])));
    let mut values = clean(&body, &rules)?;
    let card = values.remove("card").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "now".into());
    let settings = merged(&app.engine.overlay_settings(), &values)?;
    let engine = app.engine.clone();
    let png = tokio::task::spawn_blocking(move || engine.preview_card(&settings, &card))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], png).into_response())
}

async fn history(State(app): State<App>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let limit = query_int(&q, "limit", 100, 1, 1000)?;
    let offset = query_int(&q, "offset", 0, 0, i64::MAX)?;
    let conn = app.db.lock();
    let mut stmt = conn.prepare(
        "SELECT history.*, tracks.artist, tracks.title FROM history \
         LEFT JOIN tracks ON tracks.id = history.track_id \
         ORDER BY history.id DESC LIMIT ? OFFSET ?",
    )?;
    let rows: Vec<Value> = stmt.query_map([limit, offset], db::row_json)?.collect::<rusqlite::Result<_>>()?;
    Ok(Json(json!(rows)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_filter_escapes_like_wildcards() {
        let (clause, args) = library_filter("50%  off_", "/rock/80s/", "");
        assert!(clause.starts_with("missing = 0 AND ("));
        assert_eq!(args[0], "%50\\%%");
        assert_eq!(args[5], "%off\\_%");
        assert_eq!(&args[10..], ["rock/80s", "rock/80s/%"]);
        let (clause, args) = library_filter("", "", UNCATEGORIZED);
        assert!(clause.ends_with("json_array_length(categories) = 0") && args.is_empty());
    }

    #[test]
    fn checks_bodies_like_the_old_api() {
        let body = json!({"scale": 120.0, "accent_color": "#00ff00", "stroke_width": 2, "mode": null, "extra": 1});
        let values = clean(&body, &overlay_rules()).unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(values["scale"], json!(120));
        assert_eq!(values["stroke_width"], json!(2.0));
        assert!(!values.contains_key("mode") && !values.contains_key("extra"));
        let lax = clean(&json!({"scale": "120", "stroke_width": " 2.5"}), &overlay_rules()).unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!((lax["scale"].clone(), lax["stroke_width"].clone()), (json!(120), json!(2.5)));
        for bad in [json!({"scale": 10}), json!({"scale": "abc"}), json!({"scale": 1.5}), json!({"accent_color": "red"}), json!({"position": "middle"})] {
            assert!(clean(&bad, &overlay_rules()).is_err(), "{bad}");
        }
    }

    #[test]
    fn library_filter_finds_songs_in_sqlite() {
        let db = Db::memory().unwrap();
        let conn = db.lock();
        for (path, folder, categories) in [("rock/a.mp4", "rock", "[\"Rock\"]"), ("pop/b.mp4", "pop", "[]")] {
            conn.execute(
                "INSERT INTO tracks (path, folder, title, duration, size, mtime, added_at, categories) \
                 VALUES (?, ?, ?, 1, 1, 0, 0, ?)",
                rusqlite::params![path, folder, path, categories],
            )
            .unwrap();
        }
        let count = |q: &str, folder: &str, category: &str| -> i64 {
            let (clause, args) = library_filter(q, folder, category);
            conn.query_row(&format!("SELECT COUNT(*) FROM tracks WHERE {clause}"), rusqlite::params_from_iter(args.iter()), |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("", "", ""), 2);
        assert_eq!(count("", "rock", ""), 1);
        assert_eq!(count("", "", "rock"), 1);
        assert_eq!(count("", "", UNCATEGORIZED), 1);
        assert_eq!(count("b.mp4", "", ""), 1);
    }
}
