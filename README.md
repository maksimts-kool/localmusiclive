# localmusiclive

A 24/7 music-video channel for YouTube Live, run from a Mac with Docker.
[ffplayout](https://github.com/ffplayout/ffplayout) does the playout and
streaming; a small control panel owns the queue: drag to reorder, skip,
autoplay when the queue runs dry, and a "Now · Next" lower third on the stream.

```
 browser ──WebSocket──▶ panel (FastAPI + Svelte, :8080) ──REST──▶ ffplayout (:8787) ──RTMP──▶ YouTube
                            ▲          SQLite in /data               │
                            └──────── server-sent events ────────────┘
                     both read the clips from ./media (mounted at /media)
```

## Quick start

1. Copy the settings and fill them in:

   ```bash
   cp .env.example .env
   ```

   Set `FFPLAYOUT_PASSWORD` and, when you're ready to go live, `YOUTUBE_STREAM_KEY`
   (YouTube Studio → Go live → Stream). Without a key, ffplayout stays in HLS
   preview mode, so you can try everything without going live.

2. Put video clips in `media/` (subfolders are fine). Tag them with artist and
   title, or name them `Artist - Title.mp4`.

3. Start both containers:

   ```bash
   docker compose up -d --build
   ```

   The first build downloads ffplayout and builds the panel; it takes a few minutes.

4. Open the panel at <http://localhost:8080> and press **Start**.

The panel sets up ffplayout on first run (admin account, paths, playlist mode,
output). ffplayout's own admin UI stays available at <http://localhost:8787>; log
in with `FFPLAYOUT_USER` / `FFPLAYOUT_PASSWORD`.

Without a stream key, watch the preview with any HLS player, for example:

```bash
ffplay http://localhost:8787/public/1/live/stream.m3u8
```

## Using the panel

- **Library** (right): search, filter by folder, sort. `+` adds to the end of the
  queue, the list icon plays it next. Double-click a row to queue it.
- **Up next**: drag the handle to reorder, the bin removes. Times show when each
  song will start. Below the queue is the autoplay preview.
- **Skip** cuts to the next song immediately.
- **Settings** (gear): autoplay order (shuffle, least played, library order), a
  folder to draw from and how many songs before a repeat; on-screen text position,
  font, size, colours and box, with a preview.
- **History**: what played, whether it was queued or autoplay, and skips.
- **YouTube** (tab next to Library): search YouTube or paste a link. `+` downloads
  the video and queues it, the list icon downloads it and plays it next, the arrow
  only downloads. Progress shows at the top of the tab; downloaded clips join the
  library like any other file. See [YouTube downloads](#youtube-downloads).

New files in `media/` are picked up every `LIBRARY_RESCAN_MINUTES` (default 5),
or right away with the rescan button.

## YouTube downloads

The panel uses [yt-dlp](https://github.com/yt-dlp/yt-dlp) (with Deno, which
yt-dlp needs for YouTube) to fetch videos into `media/youtube/` as
`Artist - Title [video id].mp4`. It picks H.264 up to your stream height, because
decoding it is cheap and the Mac is already encoding the stream. Artist and title
come from the video title ("Daft Punk - Get Lucky (Official Video)" becomes
*Daft Punk* / *Get Lucky*), or from the channel name when the title has no
artist. A video already in the library is queued without downloading it again.

- yt-dlp updates itself each time the panel starts (`YTDLP_AUTO_UPDATE`), since
  YouTube regularly breaks older versions.
- Videos longer than `YOUTUBE_MAX_MINUTES` (default 20) and live streams are
  refused.
- If YouTube asks yt-dlp to "sign in to confirm you're not a bot", export your
  YouTube cookies in Netscape format (for example with a "cookies.txt" browser
  extension) and copy them into the panel:

  ```bash
  docker compose cp cookies.txt panel:/data/youtube-cookies.txt
  ```

- **Rights:** downloading from YouTube is against YouTube's Terms of Service
  except where YouTube offers a download option, and restreaming music videos you
  don't have rights to will trigger Content ID claims, muted streams or strikes on
  your channel. Use it for videos you own or are licensed to stream.

## What controls what

| Setting | Where |
|---|---|
| Stream key, resolution, frame rate, video/audio bitrate, x264 preset | `.env`, applied to ffplayout each time the panel starts |
| Admin login, ports, media folder, time zone, YouTube download limits | `.env` |
| Queue, autoplay, on-screen text | the panel |
| Everything else in ffplayout (logo, loudness, fillers, …) | ffplayout's UI at :8787 |

Change `.env`, then `docker compose up -d` to apply. Two things are set only on
the very first run: the ffplayout password (change it in ffplayout's UI *and*
`.env` afterwards) and two defaults the panel applies once, which are logo off and
loudness target −14 LUFS (YouTube's level; ffplayout defaults to −23).

Output is H.264 (x264, `veryfast`, constant bitrate) + AAC with a 2-second
keyframe interval, which ffplayout always uses for stream output. Docker on a Mac
can't use the GPU, so encoding runs on the CPU: 720p30 at 3000 kbit/s is
comfortable on Apple Silicon; for 1080p30 set `STREAM_WIDTH=1920`,
`STREAM_HEIGHT=1080` and about 5000 kbit/s, and keep an eye on CPU use.

## Folders and data

| Path | What |
|---|---|
| `./media` → `/media` | Your clips. ffplayout plays from it; the panel indexes it and saves YouTube downloads to `media/youtube/`. |
| `./media/filler/` | Not in the library. ffplayout plays clips from here to fill gaps. |
| volume `panel-data` | Panel database (queue, history, settings) and thumbnails |
| volumes `ffplayout-config`, `playlists` | ffplayout's database, logs and daily playlists |

`docker compose down -v` deletes the volumes and starts everything from scratch.

## How it works

ffplayout plays one JSON playlist per day and works out what should be on air from
the clock: a clip's start time is midnight plus the lengths of all clips before it.
The panel therefore never touches what has already played. Whenever the clip
changes or you edit the queue, it keeps today's playlist up to and including the
current clip and rewrites everything after it (queue first, then autoplay picks),
plus all of tomorrow. ffplayout picks the change up at the next clip boundary, so
the current song is never interrupted.

- **Skip** shortens the current clip in the playlist so the next one is due now,
  then tells ffplayout to resync with the playlist. ffplayout's own "next" would
  shift its clock instead, which breaks the hand-over at midnight.
- **Midnight**: the song that crosses midnight is split in two. The first part ends
  today's playlist and the second part, starting where the first stopped, opens
  tomorrow's, so the song plays through.
- **Live updates**: ffplayout pushes playout status to the panel four times a
  second (server-sent events); the panel pushes state to the browser over a
  WebSocket.
- **Login renewal**: the panel logs in to ffplayout's API and renews its token
  before it expires (and logs in again if it has to).

The core of this is `panel/backend/app/timeline.py` (with tests) and
`panel/backend/app/engine.py`.

## Notes and limits

- **Daylight-saving nights**: ffplayout shifts its clock by an hour at the change.
  For the rest of that day, Skip uses ffplayout's own "next", and the hand-over at
  the following midnight may hiccup once.
- After a queue edit, the start of the song after next can be trimmed by a fraction
  of a second when ffplayout reloads the playlist.
- If ffplayout or the panel restarts, playout resumes where the schedule says it
  should be. Queued songs that never got to play stay in the queue.
- The ffplayout image installs the official release `.deb` (`FFPLAYOUT_VERSION` in
  `.env`). ffplayout 2.x's own `docker/` folder only has build environments, no
  runtime image.
- ffplayout 2.x draws the text overlay with its own renderer, not FFmpeg's
  `drawtext`; fonts listed in the settings are the ones installed in the
  ffplayout image (DejaVu and Noto).

## Development

```bash
# backend tests
cd panel/backend && python3 -m pytest tests
# frontend with hot reload, against a panel on localhost:8080
cd panel/frontend && npm install && npm run dev
```

The queue table already records who requested a song (`requested_by`), ready for
viewer requests from YouTube live chat later.
