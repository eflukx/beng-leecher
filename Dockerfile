# syntax=docker/dockerfile:1

# ---- Stage 1: build a fully static (musl) binary ---------------------------
# rust:alpine targets x86_64-unknown-linux-musl by default, which links
# statically (crt-static) — exactly what a scratch image needs. Our TLS stack
# is rustls + ring with webpki-roots bundled into the binary, so no OpenSSL and
# no CA-certificate files are required at runtime.
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev build-base perl
WORKDIR /src

# Pre-build dependencies as a cacheable layer (keyed on the manifests only).
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src

# Build the real binary. Drop the dummy artifact and bump the source mtime so
# cargo doesn't mistake the stale build for an up-to-date one (Docker COPY can
# set older mtimes than the cached object).
COPY src ./src
RUN rm -f target/release/beng_leecher target/release/deps/beng_leecher* \
    && touch src/main.rs \
    && cargo build --release

# ---- Stage 2: static ffmpeg + ffprobe --------------------------------------
# Prebuilt, statically-linked ffmpeg binaries that run fine in a scratch image.
FROM mwader/static-ffmpeg:7.1 AS ffmpeg

# ---- Stage 3: scratch runtime ----------------------------------------------
FROM scratch
# Command::new("ffmpeg") needs a PATH to search; scratch has none by default.
ENV PATH=/usr/bin
COPY --from=ffmpeg /ffmpeg /ffprobe /usr/bin/
COPY --from=build /src/target/release/beng_leecher /usr/bin/beng_leecher

# Cache (/data/downloads) and media library (/data/media) live here; mount a
# volume on /data to persist them. WORKDIR also creates the directory.
WORKDIR /data
EXPOSE 3380
ENTRYPOINT ["/usr/bin/beng_leecher"]
CMD ["-a", "0.0.0.0:3380", "-m", "/data/media", "-t", "60m"]
