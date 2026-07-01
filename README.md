# gopher-spot

Spotify Connect receiver + Gopher UI for Mac OS 9. Runs 100% on the home
`debene` Kubernetes cluster, LAN-only. See [PROMPT.md](PROMPT.md) for the full
design brief.

Two pods, two MetalLB LoadBalancer IPs:

- **audio-stream** — `librespot | ffmpeg` → MP3 128k CBR at `:8000/spotify.mp3`.
  Audion on OS 9 stays parked on this URL.
- **gopher-server** — geomyidae + the `gopher-spot` dcgi, Gopher on `:70`.
  TurboGopher browses/searches/controls; it never sees the MP3.

This project does **not** touch the VPS `gopher.debene.dev`. Homelab k8s only.

## LoadBalancer IPs (fill in after first apply)

MetalLB pool turned out to be `10.0.100.x` (the PROMPT guessed `10.0.10.x`).

| Service        | Port | LAN IP          | `.lan` name (Technitium) |
|----------------|------|-----------------|--------------------------|
| audio-stream   | 8000 | `10.0.100.___`  | `audio-stream.lan`       |
| gopher-server  | 70   | `10.0.100.112`  | `gopher-spot.lan`        |

```sh
kubectl -n gopher-spot get svc -o wide   # read EXTERNAL-IP here
```

---

## Design decisions (answers to the three PROMPT questions)

1. **librespot: build from source.** No official static multi-arch binaries
   exist, and building lets us `--no-default-features` to drop every system
   audio backend (alsa/pulse/rodio/portaudio/jack) *and* libmdns. The pipe
   backend is always compiled and is all we need. Smaller image, no C audio
   deps. Pinned to `LIBRESPOT_VERSION` in the Dockerfile.

2. **gopher-server: one image.** geomyidae + the dcgi binary in a single image;
   geomyidae execs the dcgi by `.dcgi` extension + exec bit (the sibling
   `gopher-askthedeck` pattern). A split Deployment would need a shared
   filesystem or a network-exec shim for zero real benefit. (Implemented in
   Fio B.)

3. **librespot cache: emptyDir.** A PVC would be RWO and pin the pod to the node
   holding the volume — directly fighting the "scheduler decides, nothing
   pinned" constraint. In credentials mode there's nothing worth persisting: on
   restart the entrypoint re-seeds `credentials.json` from the Secret into the
   fresh emptyDir, no re-login. emptyDir it is. (See Discovery, below.)

## Discovery — credentials mode (decided)

The PROMPT's Fio A story assumes the pod shows up in the phone's Spotify Connect
list via **zeroconf** (mDNS). **That can't work from an overlay pod without
`hostNetwork`** — which is forbidden. mDNS is link-local multicast
(`224.0.0.251:5353`); it originates in the pod's netns and never crosses the CNI
boundary onto the LAN, and MetalLB only forwards the one TCP port (`:8000`).

So the deployment runs in **`credentials` mode** (decided): librespot logs into
Spotify's access point with a cached `credentials.json` and shows up as a Connect
device *through Spotify's backend*, needing only outbound HTTPS. `LIBRESPOT_MODE`
selects the mode:

- **`credentials`** — in-cluster default (`deploy/audio-stream.yaml`). Reads
  `credentials.json` from the `librespot-credentials` Secret (mounted read-only
  at `/seed`, copied into the writable `/cache` emptyDir by the entrypoint).
- **`zeroconf`** — local `docker run --network host` testing only; useless from
  inside the cluster.

### Seeding `librespot-credentials` (one-shot, on a LAN box)

```sh
# Run librespot locally in zeroconf mode so your phone can see it:
librespot --name gopher-spot --cache ./c --disable-audio-cache --backend pipe > /dev/null &
# On the phone: Spotify > Connect > pick "gopher-spot". librespot writes ./c/credentials.json.
kill %1

# Create the Secret from that file:
kubectl -n gopher-spot create secret generic librespot-credentials \
  --from-file=credentials.json=./c/credentials.json
```

(This is the player identity, distinct from the Web API OAuth Secret in Fio C.
`deploy/secrets.yaml.template` documents both.)

## Image size note

The `<40MB` target for audio-stream is unlikely with alpine's `ffmpeg` apk,
which pulls ~30-40MB of libav*/lame/protocol shared objects. Expect ~50-60MB.
Getting under 40MB means a hand-built minimal static ffmpeg for both arches (no
multi-arch apk exists) — a rabbit hole I did not chase. Say the word if the size
matters more than build simplicity.

---

## Fio A — audio-stream

```sh
# 1. build + push multi-arch (needs docker login ghcr.io)
./scripts/buildx.sh audio

# 2. check MetalLB has a free pool address BEFORE applying
kubectl get ipaddresspools -A -n metallb-system

# 3. seed the player credentials Secret (see "Seeding" above) — the pod won't
#    start without it, since credentials mode mounts it
kubectl apply -f deploy/namespace.yaml
kubectl -n gopher-spot create secret generic librespot-credentials \
  --from-file=credentials.json=./c/credentials.json

# 4. apply
kubectl apply -f deploy/audio-stream.yaml

# 5. read the assigned LB IP (if <pending> >60s, describe svc + check MetalLB
#    BEFORE assuming an app bug — per PROMPT)
kubectl -n gopher-spot get svc audio-stream -o wide

# 6. transfer playback from the phone to the "gopher-spot" device, then open
#    http://<lb-ip>:8000/spotify.mp3 in VLC.
```

Local smoke test without k8s:

```sh
docker build -f docker/audio-stream.Dockerfile -t gopher-spot-audio .
docker run --rm --network host -e LIBRESPOT_MODE=zeroconf gopher-spot-audio  # pick from phone
# open http://localhost:8000/spotify.mp3 in VLC
```

## Fio B — gopher-server + dcgi skeleton

geomyidae serves `/srv`. Routing (verified against geomyidae's CGI.md):

- `/` → static baked `/srv/index.gph` (the root menu).
- `/spot/*` → `/srv/spot/index.dcgi` (a one-line wrapper → `gopher-spot dcgi`),
  which geomyidae runs as the `index.dcgi` fallback for any non-existent path
  under `/spot`, passing `$search $arguments $host $port $traversal $selector`
  and interpreting stdout as a gophermap. The dcgi routes on the selector.
- `/spot/stream.pls` → a **real file** (raw, type-`s`), generated at startup from
  `$AUDIO_STREAM_URL`. It must be a file, not dcgi output, because a `.pls` is
  served verbatim, not interpreted as a menu.

Fio B endpoints are mock (no Web API yet). Verified locally with lynx: root menu,
all `/spot/*` mock routes, the type-7 search passing its query to the dcgi, the
raw PLS, and the unknown-route fallback all render as clean tabless gophermaps.

```sh
./scripts/buildx.sh server
kubectl apply -f deploy/gopher-server.yaml   # ConfigMap + Deployment(2) + Service LB
kubectl -n gopher-spot get svc gopher-server -o wide
# then: lynx gopher://<gopher-server-lb-ip>:70/   and validate in TurboGopher (Fio D)
```

Local smoke test:

```sh
docker build -f docker/gopher-server.Dockerfile -t gopher-spot-server .
docker run --rm -p 7070:70 -e AUDIO_STREAM_URL=http://10.0.10.8:8000/spotify.mp3 \
  -e GOPHER_HOST=127.0.0.1 gopher-spot-server
lynx gopher://127.0.0.1:7070/1/
```

Two gotchas worth noting:

- **Privileged port :70 as non-root.** geomyidae carries a file capability
  (`setcap cap_net_bind_service=+ep`) so `nobody` can bind :70; the Deployment
  keeps `NET_BIND_SERVICE` in the bounding set and `allowPrivilegeEscalation:
  true` (required for a file cap to raise).
- **`GOPHER_HOST`.** geomyidae stamps this into every link's host token. In-
  cluster it must be the gopher-server LB IP (or `gopher-spot.lan`), else links
  won't follow from the Mac. Chicken-and-egg: apply once, read the assigned IP,
  set it in the ConfigMap, re-apply.

### MacRoman note

Fio B display text is all ASCII, so UTF-8 == MacRoman on the wire. Fio C echoes
Spotify track/artist names (accents, smart quotes), so the whole rendered
gophermap is transcoded to MacRoman at the IO edge (`macroman::encode` in
`main.rs`): ASCII — including every structural `[ ] | \t \n` byte — is identity,
and only accented display bytes are remapped (e.g. `ção` → `8d 8b 6f`, not the
UTF-8 `c3a7 c3a3`). Unmappable codepoints (CJK, emoji) become `?`.

## Fio C — OAuth + Web API

The dcgi drives the Spotify Web API with **blocking ureq** (a per-request dcgi
wants no async runtime) against the `gopher-spot` Connect device. `net` is a
default feature; `cargo test --no-default-features` builds the pure offline core.

Endpoints: `/spot/now` (currently-playing), `/spot/search` (type-7 → tracks as
`/spot/track/{id}` links + artist/album context), `/spot/track/{id}` (detail +
`>> Tocar agora` → `/spot/play?uri=...`), `/spot/play?uri=...` (PUT play on the
device), `/spot/control/{pause,next,prev,vol/N}`.

**Caching is on disk, not "in memory".** A dcgi is exec'd per request, so an
in-process cache never survives between requests — the PROMPT's "cache em memória"
is realized as a file TTL cache in `$SPOT_STATE_DIR` (an emptyDir): access token
(`expires_in − 60s`), search (5 min), devices (30 s). Per-replica, which is fine.

**Graceful degradation.** The `spotify-oauth` Secret is `optional: true` in
`envFrom`; with no creds the dcgi serves the Fio B mock menus instead of crashing.

### OAuth (one-shot, on a LAN box with a browser)

Set the app's redirect URI to exactly `http://127.0.0.1:8888/callback`, then:

```sh
SPOTIFY_CLIENT_ID=... SPOTIFY_CLIENT_SECRET=... ./scripts/spotify-oauth-init.sh
# opens an auth URL, catches the callback on :8888, mints a refresh token,
# writes deploy/secrets.yaml (gitignored)
kubectl apply -f deploy/secrets.yaml
kubectl rollout restart deployment/gopher-server -n gopher-spot
```

Scopes: `user-read-private user-read-playback-state user-modify-playback-state
user-read-currently-playing playlist-read-private playlist-read-collaborative
user-library-read`.

**Validation** (needs audio-stream up so the `gopher-spot` device exists): from
OS 9, Buscar → "chico buarque" → "Construção" → `>> Tocar agora`, hear it in
Audion (parked on the audio LB bookmark).

## Fio D — playlists + OS 9 validation

`/spot/playlists` (the user's playlists, 20/page) and `/spot/playlists/{id}` (its
tracks → `/spot/track/{id}` detail → play). Both cached 60s. Pagination is
`?offset=N` with `<< Pagina anterior` / `>> Proxima pagina` links that appear only
when the window leaves items on a side. The type-`s` `.pls` reopen item works
(static file from `AUDIO_STREAM_URL`).

Manual TurboGopher validation is a checklist in
[`scripts/validate-turbogopher.md`](scripts/validate-turbogopher.md) — including a
recipe to run a local server on the QEMU host's vmnet IP so the OS 9 guest can
reach it without cluster routing. (The blog post is skipped for now, per the
maintainer.)
