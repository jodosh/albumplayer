# The web UI. Built first so a change to Rust code does not rebuild it, and
# vice versa.
FROM node:24-slim AS ui

WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
# Optional dependencies must be kept: Vite's bundler ships its native binary as
# a platform-specific optional package, and omitting them breaks the build.
# Scripts are skipped so Playwright does not download a browser into the image.
RUN npm ci --ignore-scripts
COPY ui/ ./
RUN npm run build

# Build both binaries: the server, and the CLI used for scanning and enrichment
# via `docker exec`.
FROM rust:1-trixie AS builder

WORKDIR /build
# rusqlite compiles SQLite from source, so a C toolchain is required.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# The desktop playback engine needs GStreamer and is useless in a container, so
# only the server and CLI are built here.
RUN cargo build --release --no-default-features \
        -p albumplayer-server -p albumplayer-cli

FROM debian:trixie-slim

# ffmpeg does the loudness analysis; ca-certificates lets the artwork fetcher
# reach MusicBrainz over TLS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg \
        ca-certificates \
        curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/albumplayer-server /usr/local/bin/
COPY --from=builder /build/target/release/albumplayer /usr/local/bin/
COPY --from=ui /ui/dist /app/ui

# Sensible defaults for the container layout. Override in compose as needed.
ENV ALBUMPLAYER_MUSIC_ROOT=/music \
    ALBUMPLAYER_DATA_DIR=/data \
    ALBUMPLAYER_ART_DIR=/data/art \
    ALBUMPLAYER_UI_DIR=/app/ui \
    ALBUMPLAYER_BIND=0.0.0.0:8080

# Runs unprivileged. The music volume only ever needs to be readable.
RUN useradd --system --uid 10001 --create-home albumplayer

# Create the data directory *and give it to that user* before dropping
# privileges. Docker seeds a fresh named volume from the image's directory,
# ownership included, so without this the volume arrives owned by root and the
# server cannot create its database. (Rootless Podman remaps UIDs and hides the
# problem, which is why this has to be explicit rather than discovered.)
RUN mkdir -p /data /data/art && chown -R 10001:10001 /data

USER 10001:10001

VOLUME ["/data"]
EXPOSE 8080

# The first scan can take minutes on a large share, hence the long start period.
HEALTHCHECK --interval=30s --timeout=5s --start-period=300s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

ENTRYPOINT ["/usr/local/bin/albumplayer-server"]
