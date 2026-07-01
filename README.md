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
docker run --rm --network host gopher-spot-audio   # zeroconf mode; pick from phone
# open http://localhost:8000/spotify.mp3 in VLC
```
