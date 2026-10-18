# ffplayout, built from source with live-logo.patch applied.
#
# The patch makes ffplayout reload its logo image whenever the file changes.
# The panel uses that to draw the on-screen song card (heading, title, artist,
# countdown, rounded corners) as a transparent PNG and update it every second.
# Stock ffplayout reads the logo once per clip and its text overlay is a single
# plain line style.
#
# Upgrading: bump FFPLAYOUT_VERSION; if the patch no longer applies, port the
# LiveLogo hunk in backend/engine/src/playout.rs to the new version.
ARG FFPLAYOUT_VERSION=2.6.0

FROM debian:trixie-slim AS source
ARG FFPLAYOUT_VERSION
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "v${FFPLAYOUT_VERSION}" https://github.com/ffplayout/ffplayout.git /src
COPY live-logo.patch /tmp/live-logo.patch
RUN cd /src && git apply /tmp/live-logo.patch

FROM rust:slim-trixie AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang \
        libclang-dev \
        libavcodec-dev \
        libavdevice-dev \
        libavfilter-dev \
        libavformat-dev \
        libavutil-dev \
        libsqlite3-dev \
        libswresample-dev \
        libswscale-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
# ffplayout's build.rs builds its web UI with npm, so Node is needed here too.
COPY --from=node:24-bookworm-slim /usr/local/bin/node /usr/local/bin/node
COPY --from=node:24-bookworm-slim /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && node --version && npm --version
COPY --from=source /src /src
WORKDIR /src
# Faster than the upstream release profile (fat LTO, one codegen unit), and
# the difference doesn't matter for one channel.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.npm \
    cargo build --release --package ffplayout --no-default-features --features embed_frontend \
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
ENV DB=/config/ffplayout.db \
    LISTEN=0.0.0.0:8787 \
    LOG_TO_CONSOLE=true

VOLUME ["/config", "/playlists", "/media"]
EXPOSE 8787

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/api/setup >/dev/null || exit 1

CMD ["ffplayout"]
