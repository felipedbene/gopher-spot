#!/bin/sh
# audio-stream entrypoint: librespot (pipe backend, raw s16le on stdout) piped
# into ffmpeg, which transcodes to MP3 128k CBR and serves it over HTTP on
# :8000/spotify.mp3 for exactly one client at a time (-listen 1).
#
# Restart loop: ffmpeg's single-listener HTTP server exits when its one client
# disconnects, which SIGPIPEs librespot. Rather than let the pod crash-loop on
# every VLC/Audion reconnect, we respawn the pair. k8s would restart the pod
# anyway; this is just gentler and keeps the Connect device registered longer.
set -eu

# --- Discovery mode --------------------------------------------------------
# zeroconf:    librespot advertises via mDNS on the LAN; you pick it from the
#              phone's Spotify Connect list. WARNING: from an overlay pod with
#              no hostNetwork, the mDNS multicast never reaches the LAN, so this
#              mode does NOT work in-cluster. Kept for local `docker run` tests.
# credentials: librespot logs into Spotify's AP with a cached credentials.json
#              (seeded once, mounted as a Secret at $LIBRESPOT_CACHE) and shows
#              up as a Connect device via Spotify's backend — no LAN mDNS. This
#              is the in-cluster mode. See README "Discovery".
LIBRESPOT_MODE="${LIBRESPOT_MODE:-zeroconf}"
LIBRESPOT_NAME="${LIBRESPOT_NAME:-gopher-spot}"
LIBRESPOT_BITRATE="${LIBRESPOT_BITRATE:-320}"
LIBRESPOT_CACHE="${LIBRESPOT_CACHE:-/cache}"
MP3_BITRATE="${MP3_BITRATE:-128k}"

librespot_common="--backend pipe --name ${LIBRESPOT_NAME} \
  --bitrate ${LIBRESPOT_BITRATE} --device-type speaker"

case "$LIBRESPOT_MODE" in
  credentials)
    # credentials.json lives at $LIBRESPOT_CACHE/credentials.json (Secret).
    set -- $librespot_common --cache "$LIBRESPOT_CACHE" --disable-audio-cache
    ;;
  zeroconf)
    set -- $librespot_common --disable-audio-cache
    ;;
  *)
    echo "unknown LIBRESPOT_MODE=$LIBRESPOT_MODE (want zeroconf|credentials)" >&2
    exit 64
    ;;
esac

echo "audio-stream: librespot mode=$LIBRESPOT_MODE name=$LIBRESPOT_NAME -> mp3 $MP3_BITRATE on :8000/spotify.mp3" >&2

while true; do
  librespot "$@" \
    | ffmpeg -hide_banner -loglevel warning \
             -re -f s16le -ar 44100 -ac 2 -i pipe:0 \
             -c:a libmp3lame -b:a "$MP3_BITRATE" -f mp3 \
             -listen 1 -content_type audio/mpeg \
             http://0.0.0.0:8000/spotify.mp3 || true
  echo "audio-stream: pipeline exited, respawning in 2s" >&2
  sleep 2
done
