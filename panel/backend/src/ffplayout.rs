//! Client for ffplayout's REST API and its server-sent playout events.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use reqwest::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::util::now;

/// Access tokens live 45 minutes; renew them well before that.
const RENEW_BEFORE_SECONDS: f64 = 10.0 * 60.0;
const TIMEOUT: Duration = Duration::from_secs(15);
/// The event stream sends four events a second; this long without one means it is dead.
pub const EVENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// An error from ffplayout or the connection to it; the API answers 502 for it.
#[derive(Debug)]
pub struct FfError(pub String);

impl fmt::Display for FfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FfError {}

impl From<reqwest::Error> for FfError {
    fn from(err: reqwest::Error) -> FfError {
        let mut message = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(inner) = source {
            message = format!("{message}: {inner}");
            source = inner.source();
        }
        FfError(message)
    }
}

type Result<T> = std::result::Result<T, FfError>;

fn token_expiry(token: &str) -> f64 {
    let payload = token.split('.').nth(1).unwrap_or("");
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|claims| claims.get("exp").and_then(Value::as_f64))
        .unwrap_or_else(|| now() + 30.0 * 60.0)
}

#[derive(Default)]
struct Tokens {
    access: Option<String>,
    refresh: Option<String>,
    expires_at: f64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Json(Value),
    Text(String),
}

pub struct FFplayout {
    base_url: String,
    user: String,
    password: String,
    channel: i64,
    http: reqwest::Client,
    tokens: Mutex<Tokens>,
}

impl FFplayout {
    pub fn new(base_url: &str, user: &str, password: &str, channel: i64) -> FFplayout {
        FFplayout {
            base_url: base_url.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            channel,
            http: reqwest::Client::builder()
                .connect_timeout(TIMEOUT)
                .build()
                .expect("HTTP client"),
            tokens: Mutex::new(Tokens::default()),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // --- auth ----------------------------------------------------------------

    async fn login(&self, tokens: &mut Tokens) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/auth/login"))
            .timeout(TIMEOUT)
            .json(&json!({"username": self.user, "password": self.password}))
            .send()
            .await?;
        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            return Err(FfError(format!("ffplayout login failed ({}): {text}", status.as_u16())));
        }
        store_tokens(tokens, resp.json().await?)
    }

    pub async fn login_now(&self) -> Result<()> {
        let mut tokens = self.tokens.lock().await;
        self.login(&mut tokens).await
    }

    async fn renew(&self, tokens: &mut Tokens) -> Result<()> {
        if let Some(refresh) = tokens.refresh.clone() {
            let resp = self
                .http
                .post(self.url("/auth/refresh"))
                .timeout(TIMEOUT)
                .json(&json!({"refresh": refresh}))
                .send()
                .await?;
            if resp.status() == StatusCode::OK {
                return store_tokens(tokens, resp.json().await?);
            }
            tracing::info!("Token refresh rejected ({}), logging in again", resp.status().as_u16());
        }
        self.login(tokens).await
    }

    async fn token(&self, force: bool) -> Result<String> {
        let mut tokens = self.tokens.lock().await;
        if force || tokens.access.is_none() || tokens.expires_at - now() < RENEW_BEFORE_SECONDS {
            self.renew(&mut tokens).await?;
        }
        tokens.access.clone().ok_or_else(|| FfError("No ffplayout access token".into()))
    }

    async fn send(&self, method: &Method, path: &str, body: Option<&Value>, timeout: Option<Duration>) -> Result<Response> {
        let mut resp = None;
        for attempt in 0..2 {
            let token = self.token(attempt > 0).await?;
            let mut req = self.http.request(method.clone(), self.url(path)).bearer_auth(token);
            if let Some(timeout) = timeout {
                req = req.timeout(timeout);
            }
            if let Some(body) = body {
                req = req.json(body);
            }
            let r = req.send().await?;
            let unauthorized = r.status() == StatusCode::UNAUTHORIZED;
            resp = Some(r);
            if !unauthorized {
                break;
            }
        }
        Ok(resp.expect("at least one attempt"))
    }

    /// An authorised request; the JSON answer, the text if it isn't JSON, or Null if empty.
    pub async fn request(&self, method: Method, path: &str, ok: &[u16], body: Option<&Value>) -> Result<Value> {
        let resp = self.send(&method, path, body, Some(TIMEOUT)).await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        if !ok.contains(&status) {
            let text: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            return Err(FfError(format!("{method} {path} → {status}: {text}")));
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.request(Method::GET, path, &[200], None).await
    }

    // --- setup and configuration ------------------------------------------------

    pub async fn setup_required(&self) -> Result<bool> {
        let resp = self.http.get(self.url("/api/setup")).timeout(TIMEOUT).send().await?;
        let resp = resp.error_for_status()?;
        let data: Value = resp.json().await?;
        Ok(data.get("required").map(truthy).unwrap_or(false))
    }

    pub async fn complete_setup(&self, mail: &str, storage: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url("/api/setup"))
            .timeout(TIMEOUT)
            .json(&json!({
                "username": self.user,
                "mail": mail,
                "password": self.password,
                "two_factor": false,
                "logs": "/config/logs",
                "playlists": "/playlists",
                "public": "/config/public",
                "storage": storage,
                "shared": false,
                "smtp_server": "",
                "smtp_user": "",
                "smtp_password": "",
                "smtp_starttls": false,
                "smtp_port": 465,
            }))
            .send()
            .await?;
        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            return Err(FfError(format!("ffplayout setup failed ({}): {text}", status.as_u16())));
        }
        Ok(())
    }

    pub async fn get_config(&self) -> Result<Value> {
        self.get(&format!("/api/playout/config/{}", self.channel)).await
    }

    pub async fn put_config(&self, config: &Value) -> Result<Value> {
        self.request(Method::PUT, &format!("/api/playout/config/{}", self.channel), &[200], Some(config))
            .await
    }

    /// One record per output mode (hls, stream, desktop); config.output.id picks one.
    pub async fn outputs(&self) -> Result<Vec<Value>> {
        match self.get(&format!("/api/playout/outputs/{}", self.channel)).await? {
            Value::Array(items) => Ok(items),
            _ => Ok(Vec::new()),
        }
    }

    pub async fn get_channel(&self) -> Result<Value> {
        self.get(&format!("/api/channel/{}", self.channel)).await
    }

    pub async fn patch_channel(&self, channel: &Value) -> Result<()> {
        self.request(Method::PATCH, &format!("/api/channel/{}", self.channel), &[200], Some(channel))
            .await?;
        Ok(())
    }

    // --- playout ---------------------------------------------------------------

    pub async fn process(&self, command: &str) -> Result<Value> {
        let path = format!("/api/control/{}/process", self.channel);
        self.request(Method::POST, &path, &[200], Some(&json!({"command": command}))).await
    }

    pub async fn playout(&self, control: &str) -> Result<Value> {
        let path = format!("/api/control/{}/playout", self.channel);
        self.request(Method::POST, &path, &[200], Some(&json!({"control": control}))).await
    }

    pub async fn send_text(&self, preset: &Value) -> Result<()> {
        let path = format!("/api/control/{}/text", self.channel);
        self.request(Method::POST, &path, &[200], Some(preset)).await?;
        Ok(())
    }

    pub async fn save_playlist(&self, date: &str, program: &[Value]) -> Result<()> {
        // 409 means the stored playlist is already identical; nothing to do.
        let body = json!({"channel": "Channel 1", "date": date, "program": program});
        self.request(Method::POST, &format!("/api/playlist/{}", self.channel), &[200, 409], Some(&body))
            .await?;
        Ok(())
    }

    pub async fn delete_playlist(&self, date: &str) -> Result<()> {
        let path = format!("/api/playlist/{}/{date}", self.channel);
        self.request(Method::DELETE, &path, &[200, 204, 400, 404], None).await?;
        Ok(())
    }

    /// Playout status, pushed by ffplayout four times a second. Read the
    /// answer with [`EventLines`].
    pub async fn events(&self) -> Result<Response> {
        let uuid = self.request(Method::POST, "/api/generate-uuid", &[200], None).await?;
        let uuid = uuid.get("uuid").and_then(Value::as_str).unwrap_or_default().to_string();
        let path = format!("/data/event/{}?endpoint=playout&uuid={uuid}", self.channel);
        let resp = self.http.get(self.url(&path)).send().await?;
        if resp.status() != StatusCode::OK {
            return Err(FfError(format!("Event stream refused: {}", resp.status().as_u16())));
        }
        Ok(resp)
    }
}

fn store_tokens(tokens: &mut Tokens, data: Value) -> Result<()> {
    let access = data
        .get("access")
        .and_then(Value::as_str)
        .ok_or_else(|| FfError(format!("Unexpected login response: {data}")))?;
    tokens.access = Some(access.to_string());
    tokens.refresh = data.get("refresh").and_then(Value::as_str).map(str::to_string);
    tokens.expires_at = token_expiry(access);
    Ok(())
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Splits a server-sent event stream into its `data:` payloads.
pub struct EventLines {
    resp: Response,
    buffer: Vec<u8>,
}

impl EventLines {
    pub fn new(resp: Response) -> EventLines {
        EventLines { resp, buffer: Vec::new() }
    }

    /// The next event, or None when the stream ends.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        loop {
            while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                let Some(data) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.starts_with('{') {
                    match serde_json::from_str(data) {
                        Ok(value) => return Ok(Some(Event::Json(value))),
                        Err(err) => return Err(FfError(format!("Bad playout event: {err}"))),
                    }
                }
                return Ok(Some(Event::Text(data.to_string())));
            }
            match tokio::time::timeout(EVENT_READ_TIMEOUT, self.resp.chunk()).await {
                Err(_) => return Err(FfError("No playout events for 30 s".into())),
                Ok(Err(err)) => return Err(err.into()),
                Ok(Ok(None)) => return Ok(None),
                Ok(Ok(Some(chunk))) => self.buffer.extend_from_slice(&chunk),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jwt_expiry() {
        // {"exp": 1700000000}
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1700000000}"#);
        assert_eq!(token_expiry(&format!("h.{payload}.s")), 1_700_000_000.0);
        assert!(token_expiry("garbage") > now());
    }
}
