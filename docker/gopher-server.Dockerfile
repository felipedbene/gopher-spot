# gopher-server: geomyidae serving :70, with the gopher-spot dcgi driving all
# /spot/* selectors. Alpine, multi-arch (linux/amd64 + linux/arm64) — buildx
# builds each arch natively (musl), no cross-compile.
#
#   docker build -f docker/gopher-server.Dockerfile -t gopher-spot-server .
#   docker run --rm -p 7070:70 -e AUDIO_STREAM_URL=http://10.0.10.8:8000/spotify.mp3 gopher-spot-server
#   lynx gopher://127.0.0.1:7070/
#
# Routing (confirmed against geomyidae CGI.md): geomyidae runs `spot/index.dcgi`
# for any non-existent /spot/* selector, passing $search $arguments $host $port
# $traversal $selector and interpreting stdout as a gophermap. The root menu is a
# baked static /srv/index.gph; stream.pls is a real file (raw, type-s) generated
# at startup from $AUDIO_STREAM_URL.

# --- 1. Build the dcgi binary (musl static; gopher-core is std-only) --------
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev git ca-certificates
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release && strip target/release/gopher-spot

# --- 2. Build geomyidae from source (TLS off; we serve plain gopher only) ----
# Not in Alpine repos; clone the canonical bitreich source over git:// (needs
# port 9418 egress at build). Plain POSIX C, compiles under musl.
FROM alpine:3.20 AS geo
RUN apk add --no-cache git make gcc musl-dev
ARG GEOMYIDAE_REF=v0.99
RUN git clone git://bitreich.org/geomyidae /g \
 && cd /g && git checkout "$GEOMYIDAE_REF" \
 && make TLS_CFLAGS= TLS_LDFLAGS=

# --- 3. Runtime -------------------------------------------------------------
FROM alpine:3.20
# libcap only to stamp a file capability so `nobody` can bind :70 (privileged
# port) without running as root; the package itself isn't needed at runtime.
RUN apk add --no-cache libcap

COPY --from=geo /g/geomyidae /usr/local/bin/geomyidae
COPY --from=build /src/target/release/gopher-spot /usr/local/bin/gopher-spot
COPY docker/gopher-server-entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh

# Bake the static root menu, and the /spot dcgi wrapper geomyidae execs.
RUN mkdir -p /srv/spot \
 && /usr/local/bin/gopher-spot root > /srv/index.gph \
 && printf '%s\n' '#!/bin/sh' 'exec /usr/local/bin/gopher-spot dcgi "$@"' \
      > /srv/spot/index.dcgi \
 && chmod 0755 /srv/spot/index.dcgi \
 # stream.pls is written at startup from $AUDIO_STREAM_URL, so nobody must own
 # the dir it lives in.
 && chown -R nobody:nobody /srv/spot

# File capability: bind :70 as an unprivileged user. NET_BIND_SERVICE must also
# stay in the container's bounding set (see deploy securityContext).
RUN setcap 'cap_net_bind_service=+ep' /usr/local/bin/geomyidae

# Points the generated stream.pls at the audio-stream LB; overridden per-deploy.
ENV AUDIO_STREAM_URL="http://audio-stream.lan:8000/spotify.mp3"

USER nobody:nobody
EXPOSE 70
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
