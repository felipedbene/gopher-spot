# audio-stream: librespot (pipe backend) | ffmpeg -> MP3 128k CBR over HTTP :8000
#
# Multi-arch (linux/amd64 + linux/arm64). buildx selects the native rust:alpine
# and alpine per target platform, so there is no cross-compile — each arch
# builds librespot natively. See scripts/buildx.sh.
#
#   docker build -f docker/audio-stream.Dockerfile -t gopher-spot-audio .
#   docker run --rm -p 8000:8000 gopher-spot-audio
#   # then open http://localhost:8000/spotify.mp3 in VLC
#
# Alpine base per PROMPT. NOTE on the <40MB target: librespot strips to ~12MB,
# but alpine's `ffmpeg` apk drags in libav* + lame + the network protocol libs
# (~30-40MB of shared objects). Realistic final image is ~50-60MB. Hitting <40MB
# would mean a hand-built minimal static ffmpeg for BOTH arches (a rabbit hole
# with no multi-arch apk). Flagged in README; not chased here.

# --- 1. Build librespot from source, minimal features ----------------------
# --no-default-features drops the alsa/pulse/rodio/portaudio/jack backends AND
# libmdns (zeroconf discovery). The pipe backend is always compiled and needs no
# feature. We run in *credentials* mode (Spotify AP login, appears as a Connect
# device via Spotify's backend) rather than LAN zeroconf — see README "Discovery"
# for why that matters under the no-hostNetwork constraint.
FROM rust:alpine AS build
RUN apk add --no-cache \
      musl-dev pkgconfig \
      openssl-dev openssl-libs-static \
      protobuf-dev protoc \
      cmake make g++ git
# librespot's TLS via openssl-sys, statically linked against musl.
ENV OPENSSL_STATIC=1 OPENSSL_LIB_DIR=/usr/lib OPENSSL_INCLUDE_DIR=/usr/include
ARG LIBRESPOT_VERSION=0.6.0
RUN cargo install librespot \
      --version ${LIBRESPOT_VERSION} \
      --no-default-features \
      --locked \
      --root /out \
 && strip /out/bin/librespot \
 && /out/bin/librespot --version

# --- 2. Runtime: alpine + ffmpeg + the librespot binary + entrypoint --------
FROM alpine:3.20
RUN apk add --no-cache ffmpeg
COPY --from=build /out/bin/librespot /usr/local/bin/librespot
COPY docker/audio-stream-entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh
# 8000 is unprivileged, so we run as nobody (no cap needed to bind it).
USER nobody:nobody
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
