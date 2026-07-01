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
# We build from the librespot **dev branch (0.8.0)**, NOT the last crates.io
# release (0.6.0): after a Spotify server-side change (~Nov 2025, librespot
# #1623) 0.6.0 can no longer load ANY track ("not available in any supported
# format") — auth works, playback is dead. The dev branch fixes it (verified:
# it loads + streams). Pinned to a commit for reproducibility; bump when a fixed
# release lands.
#
# --no-default-features drops the alsa/pulse/rodio/portaudio/jack backends AND
# libmdns (zeroconf) — we use the always-compiled pipe backend + credentials mode
# (see README "Discovery"). On dev the TLS backend must be selected explicitly,
# so we pick rustls-webpki (ring, musl-friendly, host-independent CA bundle — no
# openssl needed anymore).
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev pkgconfig protobuf-dev protoc cmake make g++ git
ARG LIBRESPOT_REV=db1ef7ab8c5ebd78edea0ba20f34feb21bd0e195
RUN cargo install librespot \
      --git https://github.com/librespot-org/librespot --rev ${LIBRESPOT_REV} \
      --no-default-features --features rustls-tls-webpki-roots \
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
