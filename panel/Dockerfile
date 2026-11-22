FROM node:24-bookworm-slim AS frontend
WORKDIR /src
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

FROM rust:1-slim-bookworm AS backend
WORKDIR /src
COPY backend/Cargo.toml backend/Cargo.lock ./
COPY backend/src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/panel /panel

# yt-dlp's standalone build (no Python needed) for this machine's CPU.
FROM debian:bookworm-slim AS ytdlp
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in arm64) asset=yt-dlp_linux_aarch64 ;; *) asset=yt-dlp_linux ;; esac \
    && curl -fsSL -o /yt-dlp "https://github.com/yt-dlp/yt-dlp/releases/latest/download/$asset" \
    && chmod 755 /yt-dlp

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg fonts-inter \
    && rm -rf /var/lib/apt/lists/*
# yt-dlp needs a JavaScript runtime for YouTube; it finds deno on the PATH.
COPY --from=denoland/deno:bin /deno /usr/local/bin/deno
COPY --from=ytdlp /yt-dlp /usr/local/bin/yt-dlp
COPY --from=backend /panel /usr/local/bin/panel
COPY --from=frontend /src/dist /app/static
# glibc malloc: few per-thread arenas, and buffers of 1 MB or more (video
# frames for the song card) always go straight back to the system when freed,
# instead of staying reserved after every redraw.
ENV MALLOC_ARENA_MAX=2 \
    MALLOC_MMAP_THRESHOLD_=1048576
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD ["panel", "healthcheck"]
CMD ["panel"]
