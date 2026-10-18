import os
from dataclasses import dataclass, field
from pathlib import Path


def _int(name: str, default: int) -> int:
    value = os.environ.get(name, "").strip()
    return int(value) if value else default


@dataclass(frozen=True)
class Settings:
    ffplayout_url: str = os.environ.get("FFPLAYOUT_URL", "http://ffplayout:8787").rstrip("/")
    ffplayout_user: str = os.environ.get("FFPLAYOUT_USER", "admin")
    ffplayout_password: str = os.environ.get("FFPLAYOUT_PASSWORD", "")
    ffplayout_mail: str = os.environ.get("FFPLAYOUT_MAIL", "admin@localhost.invalid")
    channel_id: int = _int("FFPLAYOUT_CHANNEL", 1)

    youtube_stream_key: str = os.environ.get("YOUTUBE_STREAM_KEY", "").strip()
    youtube_rtmp_url: str = os.environ.get(
        "YOUTUBE_RTMP_URL", "rtmp://a.rtmp.youtube.com/live2"
    ).rstrip("/")
    stream_width: int = _int("STREAM_WIDTH", 1280)
    stream_height: int = _int("STREAM_HEIGHT", 720)
    stream_fps: int = _int("STREAM_FPS", 30)
    video_bitrate_kbps: int = _int("VIDEO_BITRATE_KBPS", 3000)
    audio_bitrate_kbps: int = _int("AUDIO_BITRATE_KBPS", 128)
    x264_preset: str = os.environ.get("X264_PRESET", "veryfast")

    # Day boundary used for ffplayout's daily playlists. Midnight unless testing.
    day_start: str = os.environ.get("DAY_START", "00:00:00")
    timezone: str = os.environ.get("TZ", "Etc/UTC")

    # Both containers mount the clips at this path, so playlist paths match.
    media_root: Path = Path(os.environ.get("MEDIA_ROOT", "/media"))
    data_dir: Path = Path(os.environ.get("DATA_DIR", "/data"))
    static_dir: Path = Path(os.environ.get("STATIC_DIR", "/app/static"))
    rescan_minutes: int = _int("LIBRARY_RESCAN_MINUTES", 5)

    video_extensions: tuple[str, ...] = field(
        default=(".mp4", ".mkv", ".webm", ".mov", ".m4v", ".avi", ".ts", ".flv")
    )
    # Top-level folders in /media that are not part of the library. ffplayout's
    # default filler lives in filler/.
    ignored_folders: tuple[str, ...] = field(default=("filler", "00-assets"))

    @property
    def db_path(self) -> Path:
        return self.data_dir / "panel.db"

    @property
    def thumbs_dir(self) -> Path:
        return self.data_dir / "thumbs"

    @property
    def day_start_sec(self) -> float:
        h, m, s = (self.day_start.split(":") + ["0", "0"])[:3]
        return int(h) * 3600 + int(m) * 60 + float(s)


settings = Settings()
