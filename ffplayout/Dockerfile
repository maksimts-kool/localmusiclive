# ffplayout runtime image.
#
# ffplayout v2's own docker/ folder only contains build environments (they
# produce the .deb and binaries), not a runtime image, so this installs the
# official release .deb on Debian trixie. Trixie ships FFmpeg 7.1, which
# matches the FFmpeg 7 libraries the release is linked against.
FROM debian:trixie-slim

ARG FFPLAYOUT_VERSION=2.6.0
ARG TARGETARCH

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        ffmpeg \
        fonts-dejavu-core \
        fonts-noto-core \
        tzdata \
    && curl -fsSL -o /tmp/ffplayout.deb \
        "https://github.com/ffplayout/ffplayout/releases/download/v${FFPLAYOUT_VERSION}/ffplayout_v${FFPLAYOUT_VERSION}-1_${TARGETARCH}.deb" \
    && apt-get install -y --no-install-recommends /tmp/ffplayout.deb \
    && rm -rf /var/lib/apt/lists/* /tmp/ffplayout.deb \
    && ffplayout --version

# DB is read by ffplayout from the environment (--db).
ENV DB=/config/ffplayout.db \
    LISTEN=0.0.0.0:8787 \
    LOG_TO_CONSOLE=true

VOLUME ["/config", "/playlists", "/media"]
EXPOSE 8787

HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8787/api/setup >/dev/null || exit 1

CMD ["ffplayout"]
