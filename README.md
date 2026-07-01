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

| Service        | Port | LAN IP        | `.lan` name (Technitium) |
|----------------|------|---------------|--------------------------|
| audio-stream   | 8000 | `10.0.10.___` | `audio-stream.lan`       |
| gopher-server  | 70   | `10.0.10.___` | `gopher-spot.lan`        |

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

3. **librespot credentials: emptyDir.** A PVC would be RWO and pin the pod to
   the node holding the volume — directly fighting the "scheduler decides,
   nothing pinned" constraint — to save a one-time ~10s re-login. Not worth the
   state. emptyDir it is. (This interacts with Discovery, below.)

## Discovery — read this before Fio A validation

The PROMPT's Fio A validation assumes the pod appears in the phone's Spotify
Connect list via **zeroconf** (mDNS). **That does not work from an overlay pod
without `hostNetwork`** — and `hostNetwork` is forbidden by the constraints.
mDNS is link-local multicast (`224.0.0.251:5353`); it originates in the pod's
network namespace and never crosses the CNI boundary onto the LAN. MetalLB only
forwards the one TCP port (`:8000`), not discovery multicast.

So there are two modes, selected by the `LIBRESPOT_MODE` env var:

- **`zeroconf`** (image default) — works for local `docker run` testing only.
  Discoverable on the same L2 host, useless from inside the cluster.
- **`credentials`** — librespot logs into Spotify's access point with a cached
  `credentials.json` and shows up as a Connect device *through Spotify's
  backend*, needing only outbound HTTPS. **This is the in-cluster mode.** Seed
  `credentials.json` once by running librespot locally in zeroconf mode, picking
  it from your phone, and copying the resulting `credentials.json` into the
  `librespot-credentials` Secret (see `deploy/secrets.yaml.template`). Then set
  `LIBRESPOT_MODE=credentials` and project the Secret into `/cache` (commented
  stanza in `deploy/audio-stream.yaml`).

**Open decision for you:** confirm we go credentials-mode in-cluster (my
recommendation), or tell me your CNI actually bridges the pods onto the LAN
subnet (macvlan / Cilium native routing on `10.0.10.x`), in which case zeroconf
could work and I'll adjust. I did not build/push/apply anything yet.

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

# 3. apply
kubectl apply -f deploy/namespace.yaml
kubectl apply -f deploy/audio-stream.yaml

# 4. read the assigned LB IP (if <pending> >60s, describe svc + check MetalLB
#    BEFORE assuming an app bug — per PROMPT)
kubectl -n gopher-spot get svc audio-stream -o wide

# 5. once credentials-mode is set up: transfer playback from the phone to the
#    "gopher-spot" device, then open http://<lb-ip>:8000/spotify.mp3 in VLC.
```

Local smoke test without k8s:

```sh
docker build -f docker/audio-stream.Dockerfile -t gopher-spot-audio .
docker run --rm --network host gopher-spot-audio   # zeroconf mode; pick from phone
# open http://localhost:8000/spotify.mp3 in VLC
```
