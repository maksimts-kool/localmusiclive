# ffplayout, built from source with live-logo.patch applied.
#
# The patch makes ffplayout reload its logo image whenever the file changes.
# The panel uses that to draw the on-screen song card (heading, title, artist,
# countdown, rounded corners) as a transparent PNG and update it while it plays.
# Stock ffplayout only picks up a changed logo file at the next clip, and its
# text overlay is a single plain line style.
#
# Version: the latest ffplayout release, looked up on every build. Set
# FFPLAYOUT_VERSION (e.g. 2.9.0) to build a specific one instead. If a new
# release breaks the patch, the build stops at `git apply`: port the LiveLogo
# watcher in crates/engine/src/graphics/program.rs and source.rs to it, or set
# FFPLAYOUT_VERSION to the last release that worked.
ARG FFPLAYOUT_VERSION=

# Finds the version to build. The release JSON changes all the time (download
# counts), but only the version number is passed on, so the clone and build
# below stay cached until there is a new release.
FROM debian:trixie-slim AS release
ARG FFPLAYOUT_VERSION
ADD https://api.github.com/repos/ffplayout/ffplayout/releases/latest /tmp/latest-release.json
RUN version="${FFPLAYOUT_VERSION#v}" \
    && if [ -z "$version" ]; then \
        version="$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^",]*\)".*/\1/p' /tmp/latest-release.json | head -n 1)"; \
    fi \
    && test -n "$version" \
    && echo "$version" > /version

FROM debian:trixie-slim AS source
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
COPY --from=release /version /version
RUN version="$(cat /version)" \
    && echo "Building ffplayout $version" \
    && git clone --depth 1 --branch "v${version}" https://github.com/ffplayout/ffplayout.git /src
COPY live-logo.patch /tmp/live-logo.patch
RUN cd /src && git apply /tmp/live-logo.patch \
    || { echo "live-logo.patch does not apply to this ffplayout release; see the top of ffplayout/Dockerfile" >&2; exit 1; }

FROM rust:slim-trixie AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang \
        libclang-dev \
        libavcodec-dev \
        libavformat-dev \
        libavutil-dev \
        libsqlite3-dev \
        libswresample-dev \
        libswscale-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
# ffplayout's build.rs builds its web UI with npm, so Node is needed here too.
# The runtime links Debian's FFmpeg libraries, which include the SVT-AV1 encoder.
COPY --from=node:24-bookworm-slim /usr/local/bin/node /usr/local/bin/node
COPY --from=node:24-bookworm-slim /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && node --version && npm --version
COPY --from=source /src /src
WORKDIR /src
# Faster than the upstream release profile (fat LTO, one codegen unit), and
# the difference doesn't matter for one channel.
# The web UI is built here rather than by ffplayout's build.rs: with the cargo
# cache below, build.rs may not run again for a fresh source tree, which then
# has no UI to embed.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    FFPLAYOUT_FRONTEND_PREBUILT=1
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.npm \
    npm ci && npm run build-only \
    && cargo build --release --package ffplayout --no-default-features --features embed_frontend \
    && cp target/release/ffplayout /usr/local/bin/ffplayout

FROM debian:trixie-slim
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        ffmpeg \
        fonts-dejavu-core \
        fonts-inter \
        fonts-noto-core \
        libsqlite3-0 \
        tzdata \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/ffplayout /usr/bin/ffplayout
COPY --from=source /src/assets/logo.png /src/assets/dummy.vtt /usr/share/ffplayout/
RUN mkdir -p /usr/share/ffplayout/db /var/log/ffplayout && ffplayout --version

# DB is read by ffplayout from the environment (--db).
# glibc malloc: few per-thread arenas, and buffers of 1 MB or more always go
# straight back to the system when freed. The song card is decoded about ten
# times a second on one thread and freed on another; without this, memory
# grows by ~30 MB a minute and playout gets slower over time.
ENV DB=/config/ffplayout.db \
    LISTEN=0.0.0.0:8787 \
    LOG_TO_CONSOLE=true \
    MALLOC_ARENA_MAX=2 \
    MALLOC_MMAP_THRESHOLD_=1048576

VOLUME ["/config", "/playlists", "/media"]
EXPOSE 8787

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/api/setup >/dev/null || exit 1

CMD ["ffplayout"]
