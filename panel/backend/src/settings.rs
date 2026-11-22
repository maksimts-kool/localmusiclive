use std::env;
use std::path::PathBuf;

pub const VIDEO_EXTENSIONS: &[&str] = &[".mp4", ".mkv", ".webm", ".mov", ".m4v", ".avi", ".ts", ".flv"];
/// Top-level folders in /media that are not part of the library. ffplayout's
/// default filler lives in filler/, ad breaks play from ads/.
pub const IGNORED_FOLDERS: &[&str] = &["filler", "00-assets", ADS_FOLDER];
pub const ADS_FOLDER: &str = "ads";

#[derive(Clone, Debug)]
pub struct Settings {
    pub ffplayout_url: String,
    pub ffplayout_user: String,
    pub ffplayout_password: String,
    pub ffplayout_mail: String,
    pub channel_id: i64,

    /// Empty: ffplayout writes HLS, which the watch service serves on the LAN.
    /// Set: ffplayout pushes to this RTMP URL (stream key included) instead.
    pub rtmp_url: String,
    pub watch_port: i64,
    /// Short segments and a short playlist keep viewers close to live. ffplayout
    /// puts a keyframe every 2 s, so segments can't be shorter than that.
    pub hls_segment_seconds: i64,
    pub hls_list_size: i64,
    pub stream_width: u32,
    pub stream_height: u32,
    pub stream_fps: i64,
    pub video_bitrate_kbps: i64,
    pub audio_bitrate_kbps: i64,
    pub x264_preset: String,

    /// Day boundary used for ffplayout's daily playlists. Midnight unless testing.
    pub day_start: String,
    pub timezone: String,

    /// Both containers mount the clips at this path, so playlist paths match.
    pub media_root: PathBuf,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub rescan_minutes: i64,

    /// YouTube search and download (yt-dlp).
    pub youtube_max_minutes: i64,
    /// 0 = best available resolution.
    pub youtube_max_height: i64,
    pub ytdlp_auto_update: bool,
    pub ytdlp: String,
    pub port: u16,
}

fn text(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn int(name: &str, default: i64) -> i64 {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => value.trim().parse().unwrap_or_else(|_| {
            tracing::warn!("{name}={value:?} is not a whole number; using {default}");
            default
        }),
        _ => default,
    }
}

impl Settings {
    pub fn from_env() -> Settings {
        Settings {
            ffplayout_url: text("FFPLAYOUT_URL", "http://ffplayout:8787").trim_end_matches('/').to_string(),
            ffplayout_user: text("FFPLAYOUT_USER", "admin"),
            ffplayout_password: text("FFPLAYOUT_PASSWORD", ""),
            ffplayout_mail: text("FFPLAYOUT_MAIL", "admin@localhost.invalid"),
            channel_id: int("FFPLAYOUT_CHANNEL", 1),
            rtmp_url: text("RTMP_URL", "").trim().to_string(),
            watch_port: int("WATCH_PORT", 8090),
            hls_segment_seconds: int("HLS_SEGMENT_SECONDS", 2),
            hls_list_size: int("HLS_LIST_SIZE", 10),
            stream_width: int("STREAM_WIDTH", 1280).max(16) as u32,
            stream_height: int("STREAM_HEIGHT", 720).max(16) as u32,
            stream_fps: int("STREAM_FPS", 30),
            video_bitrate_kbps: int("VIDEO_BITRATE_KBPS", 3000),
            audio_bitrate_kbps: int("AUDIO_BITRATE_KBPS", 128),
            x264_preset: text("X264_PRESET", "veryfast"),
            day_start: text("DAY_START", "00:00:00"),
            timezone: text("TZ", "Etc/UTC"),
            media_root: PathBuf::from(text("MEDIA_ROOT", "/media")),
            data_dir: PathBuf::from(text("DATA_DIR", "/data")),
            static_dir: PathBuf::from(text("STATIC_DIR", "/app/static")),
            rescan_minutes: int("LIBRARY_RESCAN_MINUTES", 5),
            youtube_max_minutes: int("YOUTUBE_MAX_MINUTES", 20),
            youtube_max_height: int("YOUTUBE_MAX_HEIGHT", 0),
            ytdlp_auto_update: text("YTDLP_AUTO_UPDATE", "true").to_lowercase() != "false",
            ytdlp: text("YTDLP", "yt-dlp"),
            port: int("PORT", 8080) as u16,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("panel.db")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.media_root.join("youtube")
    }

    pub fn ads_dir(&self) -> PathBuf {
        self.media_root.join(ADS_FOLDER)
    }

    pub fn cookies_file(&self) -> PathBuf {
        self.data_dir.join("youtube-cookies.txt")
    }

    pub fn thumbs_dir(&self) -> PathBuf {
        self.data_dir.join("thumbs")
    }

    pub fn day_start_sec(&self) -> f64 {
        let mut parts = self.day_start.split(':');
        let h: f64 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0.0);
        let m: f64 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0.0);
        let s: f64 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0.0);
        h * 3600.0 + m * 60.0 + s
    }
}
