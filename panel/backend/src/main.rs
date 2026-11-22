//! localmusiclive panel: owns the queue and keeps ffplayout's playlists, the
//! on-screen song card and the browsers in step with it.

mod ads;
mod api;
mod db;
mod discover;
mod engine;
mod ffplayout;
mod hub;
mod levels;
mod library;
mod logos;
mod overlay;
mod raster;
mod settings;
mod timeline;
mod util;
mod youtube;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use crate::ads::Ads;
use crate::db::Db;
use crate::engine::Engine;
use crate::ffplayout::FFplayout;
use crate::hub::Hub;
use crate::library::Library;
use crate::settings::Settings;
use crate::youtube::YouTube;

/// `panel healthcheck`: exit 0 if the panel answers on /api/health (for Docker).
fn healthcheck(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(4)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
    if stream.write_all(b"GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.lines().next().is_some_and(|status| status.split_whitespace().nth(1) == Some("200"))
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let s = Arc::new(Settings::from_env());
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if healthcheck(s.port) { 0 } else { 1 });
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,reqwest=warn")))
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    if s.ffplayout_password.is_empty() {
        tracing::error!("FFPLAYOUT_PASSWORD is not set");
    }
    let db = Arc::new(Db::open(&s.db_path())?);
    let ff = Arc::new(FFplayout::new(&s.ffplayout_url, &s.ffplayout_user, &s.ffplayout_password, s.channel_id));
    let library = Arc::new(Library::new(db.clone(), s.clone()));
    let hub = Hub::default();
    let ads = Arc::new(Ads::new(s.clone()));
    let engine = Arc::new(Engine::new(s.clone(), db.clone(), ff, library.clone(), ads, hub.clone()));
    let youtube = Arc::new(YouTube::new(s.clone(), db.clone(), library.clone(), engine.clone(), hub.clone()));
    engine.set_downloads(&youtube);

    engine.start();
    {
        let youtube = youtube.clone();
        let engine = engine.clone();
        tokio::spawn(async move {
            // Commercials are downloaded with yt-dlp, so after its update.
            youtube.start().await;
            if engine.ad_settings().enabled {
                engine.fetch_ads();
            }
        });
    }

    let app = api::router(api::App { s: s.clone(), db, library, engine: engine.clone(), youtube, hub });
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", s.port)).await?;
    tracing::info!("Panel listening on port {}", s.port);
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    engine.stop();
    Ok(())
}
