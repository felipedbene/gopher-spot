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
#   librespot --backend pipe  →  FIFO  →  ffmpeg (s16le → MP3)  →  Icecast /spotify.mp3
#   ffmpeg (anullsrc → MP3)              →  Icecast /silence.mp3   (always-on fallback)
# /spotify.mp3 has fallback-mount=/silence.mp3 + fallback-override, so when the
# live source drops (idle > source-timeout) listeners hear silence, and snap back
# to live when a track plays. Clients only ever dial :8000/spotify.mp3.
#
# Why the FIFO (and not a bare `librespot | ffmpeg` pipe): when librespot idles it
# STOPS producing PCM but STAYS ALIVE, so ffmpeg stalls; Icecast drops the source
# after source-timeout; and when a track resumes ffmpeg writes to the now-closed
# Icecast socket → "Broken pipe", ffmpeg dies. In a bare pipe librespot survives
# ffmpeg's death, so `librespot | ffmpeg` never returns and the `while` loop never
# respawns — the live mount stays dead until a pod restart. Routing PCM through a
# FIFO lets us hold librespot's PID and, the moment ffmpeg exits, KILL librespot so
# the loop tears the whole chain down and respawns it with a FRESH Icecast source.
set -eu

LIBRESPOT_MODE="${LIBRESPOT_MODE:-credentials}"
LIBRESPOT_NAME="${LIBRESPOT_NAME:-gopher-spot}"
LIBRESPOT_BITRATE="${LIBRESPOT_BITRATE:-320}"
LIBRESPOT_CACHE="${LIBRESPOT_CACHE:-/cache}"
# Start at full scale. librespot applies its software volume to the pipe samples,
# and its DEFAULT initial volume (~50%) on a LOGARITHMIC taper sounds very quiet —
# and the /cache emptyDir is wiped every pod start, so it never remembers louder.
# The stream should leave librespot at unity gain; do any attenuation downstream
# (the MacAST client, or the /spot/control volume command). Override if desired.
LIBRESPOT_VOLUME="${LIBRESPOT_VOLUME:-100}"
MP3_BITRATE="${MP3_BITRATE:-128k}"
# Source password for the internal librespot→ffmpeg→Icecast link. Localhost-only,
# so a fixed default is fine; override if paranoid.
ICECAST_SOURCE_PASS="${ICECAST_SOURCE_PASS:-gopher-spot-src}"

# --- librespot args by discovery mode (see README "Discovery") --------------
librespot_common="--backend pipe --name ${LIBRESPOT_NAME} \
  --bitrate ${LIBRESPOT_BITRATE} --device-type speaker \
  --initial-volume ${LIBRESPOT_VOLUME}"
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
    <!-- Tolerate slow inter-song track loads without dropping the live source
         (a drop forces a failover→silence + a full live-chain respawn). Long
         genuine idle/pause still exceeds this and correctly fails to silence. -->
    <source-timeout>20</source-timeout>
    <!-- Keep listeners near the live edge. burst-size is the backlog sent on
         connect (prebuffer): ~1s at 128k. queue-size caps how far a slightly-slow
         client may drift before Icecast trims it: ~16s instead of the old ~64s
         (a full song). Lower these further only if the client keeps up cleanly;
         if MacAST underruns/disconnects, it can't sustain the bitrate — drop
         MP3_BITRATE instead. -->
    <burst-size>16384</burst-size>
    <queue-size>262144</queue-size>
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

# Live source: librespot -> FIFO -> ffmpeg -> /spotify.mp3.
#
# librespot writes PCM into the FIFO in the background so we keep its PID; ffmpeg
# reads the FIFO in the foreground, so this loop iterates the instant ffmpeg exits.
# ffmpeg only exits when its Icecast source socket has died — i.e. after a long
# idle/pause let Icecast time the source out and a resuming track hit the stale
# socket ("Broken pipe"). We then KILL librespot (it survives ffmpeg's death and
# would otherwise sit there writing into a dead pipe forever) and respawn the whole
# chain: a fresh librespot + fresh ffmpeg get a brand-new /spotify.mp3 source, and
# fallback-override snaps parked listeners from silence back to live.
#
# No `-re` on the pipe input: librespot already produces PCM at realtime, so `-re`
# would only add a second throttle and risk a catch-up burst after a gap.
FIFO=/tmp/spotify.pcm
[ -p "$FIFO" ] || mkfifo "$FIFO"
while true; do
  librespot "$@" > "$FIFO" &
  LR=$!
  # `-re` throttles output to real time. For raw PCM (no embedded timestamps) it
  # paces by SAMPLE COUNT, so there's no post-gap catch-up burst — it just stops
  # ffmpeg from flooding Icecast's queue, which is what makes listeners drift a
  # whole song behind the live edge.
  ffmpeg -hide_banner -loglevel warning -re -f s16le -ar 44100 -ac 2 -i "$FIFO" \
         -c:a libmp3lame -b:a "$MP3_BITRATE" -write_xing 0 -f mp3 -legacy_icecast 1 \
         -content_type audio/mpeg "$(ice_url spotify.mp3)" || true
  echo "audio-stream: live encoder exited, tearing down librespot + respawning in 2s" >&2
  kill "$LR" 2>/dev/null || true
  wait "$LR" 2>/dev/null || true
  sleep 2
done
