#!/bin/sh
# gopher-server entrypoint: generate the static stream.pls from the runtime
# $AUDIO_STREAM_URL (the audio-stream LoadBalancer, LAN-reachable by the Mac),
# then run geomyidae in the foreground.
#
# The .pls MUST point at a LAN address the OS 9 box can dial directly — the
# audio-stream LB IP, NOT a cluster-internal DNS name (the Mac can't resolve
# cluster DNS). Set AUDIO_STREAM_URL to http://<audio-stream-lb-ip>:8000/spotify.mp3.
set -eu

: "${AUDIO_STREAM_URL:=http://audio-stream.lan:8000/spotify.mp3}"

# geomyidae substitutes the `server`/`port` tokens in every link with $GOPHER_HOST
# (-h) and the listen port. On the LAN we want links to dial back the
# gopher-server LB IP, so set GOPHER_HOST to that IP (or gopher-spot.lan). If
# unset, geomyidae falls back to the container's own address (links won't follow
# from the Mac) — so this is effectively required in-cluster.
GOPHER_HOST="${GOPHER_HOST:-}"

cat > /srv/spot/stream.pls <<EOF
[playlist]
NumberOfEntries=1
File1=${AUDIO_STREAM_URL}
Title1=gopher-spot
Length1=-1
Version=2
EOF
echo "gopher-server: stream.pls -> ${AUDIO_STREAM_URL}" >&2

set -- geomyidae -d -b /srv -p 70
[ -n "$GOPHER_HOST" ] && set -- "$@" -h "$GOPHER_HOST"
echo "gopher-server: exec $*" >&2
exec "$@"
