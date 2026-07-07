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
# Radio-style continuation when the queue empties (see the --autoplay note below).
LIBRESPOT_AUTOPLAY="${LIBRESPOT_AUTOPLAY:-on}"
MP3_BITRATE="${MP3_BITRATE:-128k}"
# Source password for the internal librespot→ffmpeg→Icecast link. Localhost-only,
# so a fixed default is fine; override if paranoid.
ICECAST_SOURCE_PASS="${ICECAST_SOURCE_PASS:-gopher-spot-src}"

# --- librespot args by discovery mode (see README "Discovery") --------------
# --autoplay on: when the queue runs dry (e.g. a single track played via the Web
# API with no album/playlist context), librespot resolves a radio station from
# the last track and keeps playing instead of stopping ("no more tracks left in
# queue"). Makes single-track plays behave like a radio.
librespot_common="--backend pipe --name ${LIBRESPOT_NAME} \
  --bitrate ${LIBRESPOT_BITRATE} --device-type speaker \
  --initial-volume ${LIBRESPOT_VOLUME} --autoplay ${LIBRESPOT_AUTOPLAY}"
case "$LIBRESPOT_MODE" in
  credentials)
    LIBRESPOT_SEED="${LIBRESPOT_SEED:-/seed/credentials.json}"
    if [ ! -f "$LIBRESPOT_CACHE/credentials.json" ] && [ -f "$LIBRESPOT_SEED" ]; then
      mkdir -p "$LIBRESPOT_CACHE"
      cp "$LIBRESPOT_SEED" "$LIBRESPOT_CACHE/credentials.json"
      chmod 600 "$LIBRESPOT_CACHE/credentials.json" 2>/dev/null || true
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
    <!-- Freeze-immunity cushion for Casquinha (design/SPEC-burst.md in the
         casquinha repo). burst-size is the backlog replayed on connect: ~256 KB
         = ~16s at 128k, enough to fill the client's decode rings in the first
         second so a preemptive MP decode task can play through cooperative-loop
         freezes (menu tracking, window drags). queue-size is how far a stalled
         client may lag before Icecast disconnects it: ~2 MB = ~2 min (the old
         256 KB dropped clients frozen ~36s: "stream closed by server").
         Casquinha is the only listener class and trims to the live edge
         client-side, so the burst adds no perceived latency there; a client
         that does NOT trim will start ~16s behind live. Memory cost is
         per-listener worst-case ~2 MB, trivial at <clients>20</clients>. -->
    <burst-size>262144</burst-size>
    <queue-size>2097152</queue-size>
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

# --- Internal drainer: the invariant "zero listeners never stops playback" ---
#
# The bug (observed in the fio S2 validation): with a track playing but NO client
# on /spotify.mp3, the live chain wedges and librespot goes idle — Spotify then
# marks the gopher-spot device as no-longer-playing. Chain of backpressure: with
# no listener Icecast stops draining the ffmpeg source socket, the `-re`-paced
# ffmpeg blocks on write, so it stops reading the FIFO, so librespot blocks
# writing PCM into the FIFO, so its playback stalls and the Connect device idles.
# (During S2 this was worked around by hand — a manual `curl` kept it alive.)
#
# Fix: keep ONE internal listener permanently attached to /spotify.mp3, reading
# and discarding. Icecast always has a client to serve, so it always drains the
# source, so ffmpeg and librespot never block — a playing track keeps playing
# whether or not MacAST (or anyone) is connected. This makes zero *external*
# listeners a non-event, which is the fio S3/1 contract. The drainer costs one
# Icecast client slot (of <clients>20</clients>) and ~16 KB/s it throws away.
#
# It reconnects if the stream drops (a live-chain respawn or an Icecast restart
# closes the socket -> wget returns -> we loop). /spotify.mp3 always has bytes to
# give (live, or the silence fallback), so wget never idle-times-out mid-read.
(
  while true; do
    wget -q -O /dev/null "http://127.0.0.1:8000/spotify.mp3" || true
    sleep 1
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
#
# Watchdog (fio S3/1), two layers, both prefer the simplest mechanism that works:
#   1. This `while true` loop IS the self-healer for the live chain: kill ffmpeg
#      (or let it die on a stale socket) and the loop tears down librespot and
#      respawns the whole chain within ~2 s + librespot's reconnect. No external
#      supervisor needed.
#   2. Total Icecast death (the one thing this loop can't fix — it'd just spin on
#      "connection refused") is caught k8s-natively by the Deployment's
#      livenessProbe (TCP :8000): a few failed probes restart the container. We
#      deliberately do NOT probe "is the live mount flowing", because legitimate
#      pause/idle also stops the live source — a TCP probe on Icecast is the
#      simplest check that distinguishes "server dead" from "nothing playing".
FIFO=/tmp/spotify.pcm
[ -p "$FIFO" ] || mkfifo "$FIFO"
# GS-08: a chain that dies FAST repeatedly (Icecast up but persistently rejecting
# the source — wrong source password, slots full, mount misconfig) used to
# respawn every 2s forever, re-logging librespot into Spotify each time (a login
# storm), while the TCP livenessProbe stayed green because Icecast itself lives.
# Fast deaths (< FAST_SECS) now back off exponentially (cap 60s), and after
# MAX_FAST_FAILS in a row the script exits non-zero so the container restart
# makes the wedge visible to Kubernetes. A healthy run (chain survived >=
# FAST_SECS — normal cycles live minutes to hours) resets both. Legitimate
# pause/idle never trips this: ffmpeg blocks on the FIFO instead of exiting.
FAST_SECS=10
MAX_FAST_FAILS=10
FAILS=0
DELAY=2
while true; do
  STARTED=$(date +%s)
  librespot "$@" > "$FIFO" &
  LR=$!
  # `-re` throttles output to real time. For raw PCM (no embedded timestamps) it
  # paces by SAMPLE COUNT, so there's no post-gap catch-up burst — it just stops
  # ffmpeg from flooding Icecast's queue, which is what makes listeners drift a
  # whole song behind the live edge.
  ffmpeg -hide_banner -loglevel warning -re -f s16le -ar 44100 -ac 2 -i "$FIFO" \
         -c:a libmp3lame -b:a "$MP3_BITRATE" -write_xing 0 -f mp3 -legacy_icecast 1 \
         -content_type audio/mpeg "$(ice_url spotify.mp3)" || true
  kill "$LR" 2>/dev/null || true
  wait "$LR" 2>/dev/null || true
  if [ $(( $(date +%s) - STARTED )) -ge "$FAST_SECS" ]; then
    FAILS=0
    DELAY=2
  else
    FAILS=$((FAILS + 1))
    if [ "$FAILS" -ge "$MAX_FAST_FAILS" ]; then
      echo "audio-stream: live chain died $FAILS times in <${FAST_SECS}s each while icecast is up -- exiting for a container restart" >&2
      exit 1
    fi
    DELAY=$((DELAY * 2))
    if [ "$DELAY" -gt 60 ]; then
      DELAY=60
    fi
  fi
  echo "audio-stream: live encoder exited, tearing down librespot + respawning in ${DELAY}s (fast fails: $FAILS)" >&2
  sleep "$DELAY"
done
