# localmusiclive

A 24/7 music-video channel, run from a Mac with Docker and watched on your own
network, for example in a Samsung (Tizen) TV's browser.
[ffplayout](https://github.com/ffplayout/ffplayout) does the playout and
streaming; a small control panel owns the queue: drag to reorder, skip,
autoplay when the queue runs dry, and a "Now · Next" lower third on the stream.

```
 browser ──WebSocket──▶ panel (FastAPI + Svelte, :8080) ──REST──▶ ffplayout (:8787) ──HLS──▶ watch (nginx, :8090) ──▶ TV
                            ▲          SQLite in /data               │
                            └──────── server-sent events ────────────┘
                     both read the clips from ./media (mounted at /media)
```

## Quick start

1. Copy the settings and fill them in:

   ```bash
   cp .env.example .env
   ```

   Set `FFPLAYOUT_PASSWORD`. Nothing else is needed: the stream is served on
   your network (see [Watching](#watching-on-a-tv)).

2. Put video clips in `media/` (subfolders are fine). Tag them with artist and
   title, or name them `Artist - Title.mp4`.

3. Start the containers:

   ```bash
   docker compose up -d --build
   ```

   The first build compiles ffplayout from source (with a small patch, see
   [Song card](#song-card)) and builds the panel; it takes several minutes.

4. Open the panel at <http://localhost:8080> and press **Start**.

The panel sets up ffplayout on first run (admin account, paths, playlist mode,
output). ffplayout's own admin UI stays available at <http://localhost:8787>; log
in with `FFPLAYOUT_USER` / `FFPLAYOUT_PASSWORD`.

## Watching on a TV

There is no streaming service in between, so nothing gets muted, claimed or
taken down: ffplayout writes the stream as HLS and the `watch` container serves
it on port 8090 (`WATCH_PORT`) to anything on your network. Only the stream and
its player page are reachable there; the panel and ffplayout stay on
`BIND_ADDRESS` (this Mac only, by default).

Find the Mac's address with `ipconfig getifaddr en0` (Wi-Fi; try `en1` for
Ethernet), say `192.168.1.20`, then on the TV:

- **Browser** (Samsung's Internet app on Tizen): open `http://192.168.1.20:8090`.
  It plays full screen; if the TV blocks sound on autoplay, a Play button shows,
  press OK on the remote. Add it as a bookmark or to the home screen. If the
  player stutters on an older TV, try `http://192.168.1.20:8090/?native=1`,
  which uses the TV's own HLS player instead of hls.js.
- **IPTV app** (any app from the TV's app store that takes an M3U playlist URL):
  add `http://192.168.1.20:8090/channel.m3u`. The raw stream is
  `http://192.168.1.20:8090/live/stream.m3u8`, for VLC or any other player.

The status bar in the panel shows "On your network" with a link to the watch
page. The first time, macOS may ask whether Docker may accept incoming
connections; allow it. Give the Mac a fixed address in your router so the
bookmark keeps working. Viewers are about 20 seconds behind (three 6-second HLS
segments; ffplayout's UI has the segment settings).

To watch away from home, put the Mac on a VPN such as Tailscale (also on the
phone or laptop you watch on) rather than forwarding the port to the internet.

### Streaming to a server instead

`RTMP_URL` (stream key included, e.g. `rtmp://owncast.local:1935/live/KEY`) makes
ffplayout push to an RTMP server instead, such as a self-hosted
[Owncast](https://owncast.online) or a platform. ffplayout has one output at a
time, so the watch page has no stream while it's set. Hosted platforms (YouTube,
Twitch, Kick, Facebook) all act on copyright claims, so music videos you don't
have rights to will be muted or taken down there.

## Using the panel

- **Library** (right): search, filter by folder, sort. `+` adds to the end of the
  queue, the list icon plays it next. Double-click a row to queue it.
- **Up next**: drag the handle to reorder, the bin removes. Times show when each
  song will start. Below the queue is the autoplay preview.
- **Skip** cuts to the next song immediately.
- **Settings** (gear): autoplay order (shuffle, least played, library order), a
  folder to draw from and how many songs before a repeat; the song card's timing,
  position, size and look, with a live preview.
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
`Artist - Title [video id].mkv`. It takes the best video and best audio YouTube
offers (typically VP9 or AV1 with Opus, hence MKV); set `YOUTUBE_MAX_HEIGHT`
(for example `1080`) to cap the resolution, since 4K files are large and decoding
them costs CPU while the Mac is also encoding the stream. Artist and title
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

## Song card

The stream shows a card at the start of each song ("Now playing", with the title,
artist, thumbnail, time to the next song and a progress bar) and again just
before it ends ("Up next", with a countdown). By default it shows for the first
15 and the last 20 seconds, fading in and out; it can also stay on all the time.
Position, size, accent and background colours, border, corner radius, shadow,
thumbnail and progress bar are set in the panel's settings.

In between the cards, a thin progress line runs along the bottom edge of the
picture, the width of the screen, in the accent colour (it crossfades with the
cards). Turn it off or change its thickness in the settings; "Mid-song" in the
preview shows it. With the card set to show all the time, there is no line.

The panel draws the card and line as a transparent PNG (`media/.overlay/card.png`)
up to ten times a second while it fades, once a second otherwise for the countdown,
and up to four times a second while the line moves.
ffplayout shows it through its logo slot. Stock ffplayout reads the logo only
once per clip, so the ffplayout image is built from source with
`ffplayout/live-logo.patch`, which reloads the logo whenever the file changes.
When upgrading `FFPLAYOUT_VERSION`, check that the patch still applies.

The card uses Inter, which covers Latin, Cyrillic and Greek; titles in other
scripts (e.g. Japanese) show as boxes.

## What controls what

| Setting | Where |
|---|---|
| Output (HLS or `RTMP_URL`), resolution, frame rate, video/audio bitrate, x264 preset | `.env`, applied to ffplayout each time the panel starts |
| Admin login, ports (incl. `WATCH_PORT`), media folder, time zone, YouTube download limits | `.env` |
| Queue, autoplay, song card | the panel |
| ffplayout's logo slot | the panel (it shows the song card), so don't set a logo in ffplayout |
| Everything else in ffplayout (loudness, fillers, …) | ffplayout's UI at :8787 |

Change `.env`, then `docker compose up -d` to apply. Two things are set only on
the very first run: the ffplayout password (change it in ffplayout's UI *and*
`.env` afterwards) and the loudness target, which the panel sets once to −14 LUFS
(the usual level for streamed music; ffplayout defaults to −23).

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
- The ffplayout image is built from the release's source tag
  (`FFPLAYOUT_VERSION` in `.env`) plus `live-logo.patch`. ffplayout 2.x's own
  `docker/` folder only has build environments, no runtime image.

## Development

```bash
# backend tests
cd panel/backend && python3 -m pytest tests
# frontend with hot reload, against a panel on localhost:8080
cd panel/frontend && npm install && npm run dev
```

To try changes without touching the live channel, run a second stack under
another project name, with other ports, no `RTMP_URL` and a test media folder
(shell variables override `.env`):

```bash
MEDIA_DIR=/path/to/test-clips RTMP_URL= PANEL_PORT=18080 FFPLAYOUT_PORT=18787 WATCH_PORT=18090 docker compose -p lmltest up -d --build
```

Remove it afterwards with `docker compose -p lmltest down -v`.

The queue table already records who requested a song (`requested_by`), ready for
viewer requests later.
