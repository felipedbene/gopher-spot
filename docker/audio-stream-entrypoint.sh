#!/bin/sh
# audio-stream: an Icecast radio fed by librespot.
#
# Why Icecast and not a bare `ffmpeg -listen 1`: the listener socket must be
# ALWAYS up (MacAST parks on it), serve MANY clients, and survive idle/pauses/
# track changes. `-listen 1` did none of that — it only served one client, only
# while a track was actively producing PCM, and dropped on every gap ("connection
# refused"). Icecast is a persistent streaming server: clients connect once and
# stay; a silence fallback covers the gaps.
#
# Pipeline:
#   librespot --backend pipe  →  ffmpeg (s16le → MP3)  →  Icecast /spotify.mp3
#   ffmpeg (anullsrc → MP3)   →  Icecast /silence.mp3   (always-on fallback)
# /spotify.mp3 has fallback-mount=/silence.mp3 + fallback-override, so when the
# live source drops (idle > source-timeout) listeners hear silence, and snap back
# to live when a track plays. Clients only ever dial :8000/spotify.mp3.
set -eu

LIBRESPOT_MODE="${LIBRESPOT_MODE:-credentials}"
LIBRESPOT_NAME="${LIBRESPOT_NAME:-gopher-spot}"
LIBRESPOT_BITRATE="${LIBRESPOT_BITRATE:-320}"
LIBRESPOT_CACHE="${LIBRESPOT_CACHE:-/cache}"
MP3_BITRATE="${MP3_BITRATE:-128k}"
# Source password for the internal librespot→ffmpeg→Icecast link. Localhost-only,
# so a fixed default is fine; override if paranoid.
ICECAST_SOURCE_PASS="${ICECAST_SOURCE_PASS:-gopher-spot-src}"

# --- librespot args by discovery mode (see README "Discovery") --------------
librespot_common="--backend pipe --name ${LIBRESPOT_NAME} \
  --bitrate ${LIBRESPOT_BITRATE} --device-type speaker"
case "$LIBRESPOT_MODE" in
  credentials)
    LIBRESPOT_SEED="${LIBRESPOT_SEED:-/seed/credentials.json}"
    if [ ! -f "$LIBRESPOT_CACHE/credentials.json" ] && [ -f "$LIBRESPOT_SEED" ]; then
      mkdir -p "$LIBRESPOT_CACHE"
      cp "$LIBRESPOT_SEED" "$LIBRESPOT_CACHE/credentials.json"
      echo "audio-stream: seeded credentials.json from $LIBRESPOT_SEED" >&2
    fi
    if [ ! -f "$LIBRESPOT_CACHE/credentials.json" ]; then
      echo "audio-stream: no credentials.json at $LIBRESPOT_CACHE (seed the librespot-credentials Secret)" >&2
      exit 65
    fi
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

# --- Icecast config (written to a writable dir; runs as nobody) -------------
IC=/tmp/icecast
mkdir -p "$IC/log"
cat > "$IC/icecast.xml" <<EOF
<icecast>
  <limits>
    <clients>20</clients>
    <sources>4</sources>
    <source-timeout>10</source-timeout>
    <burst-size>65536</burst-size>
    <queue-size>1048576</queue-size>
  </limits>
  <authentication>
    <source-password>${ICECAST_SOURCE_PASS}</source-password>
    <admin-user>admin</admin-user>
    <admin-password>${ICECAST_SOURCE_PASS}</admin-password>
  </authentication>
  <hostname>localhost</hostname>
  <listen-socket><port>8000</port></listen-socket>
  <mount type="normal">
    <mount-name>/spotify.mp3</mount-name>
    <fallback-mount>/silence.mp3</fallback-mount>
    <fallback-override>1</fallback-override>
    <fallback-when-full>1</fallback-when-full>
    <public>0</public>
  </mount>
  <mount type="normal">
    <mount-name>/silence.mp3</mount-name>
    <public>0</public>
  </mount>
  <paths>
    <logdir>${IC}/log</logdir>
    <webroot>/usr/share/icecast/web</webroot>
    <adminroot>/usr/share/icecast/admin</adminroot>
    <pidfile>${IC}/icecast.pid</pidfile>
  </paths>
  <logging><loglevel>2</loglevel></logging>
  <security><chroot>0</chroot></security>
</icecast>
EOF

ice_url() { echo "icecast://source:${ICECAST_SOURCE_PASS}@127.0.0.1:8000/$1"; }

echo "audio-stream: icecast :8000 (mount /spotify.mp3, fallback /silence.mp3); librespot mode=$LIBRESPOT_MODE name=$LIBRESPOT_NAME" >&2
icecast -c "$IC/icecast.xml" &
sleep 3

# Always-on silence source -> /silence.mp3 (what listeners hear when idle).
(
  while true; do
    ffmpeg -hide_banner -loglevel error -re -f lavfi -i anullsrc=r=44100:cl=stereo \
      -c:a libmp3lame -b:a "$MP3_BITRATE" -write_xing 0 -f mp3 -legacy_icecast 1 \
      -content_type audio/mpeg "$(ice_url silence.mp3)" || true
    sleep 2
  done
) &

# Live source: librespot | ffmpeg -> /spotify.mp3. On idle librespot stops
# producing, ffmpeg stalls, Icecast's source-timeout drops the mount and listeners
# fail over to silence; a track resumes the loop and snaps them back.
while true; do
  librespot "$@" \
    | ffmpeg -hide_banner -loglevel warning -re -f s16le -ar 44100 -ac 2 -i pipe:0 \
             -c:a libmp3lame -b:a "$MP3_BITRATE" -write_xing 0 -f mp3 -legacy_icecast 1 \
             -content_type audio/mpeg "$(ice_url spotify.mp3)" || true
  echo "audio-stream: live source ended, respawning in 2s" >&2
  sleep 2
done
