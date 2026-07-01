# gopher-spot

**Spotify Connect receiver + UI Gopher pra Mac OS 9. Roda 100% no cluster
Kubernetes de casa, LAN-only.**

## Contexto

Cluster caseiro `debene`, Kubernetes vanilla (kubeadm 1.36), 5 nós mistos
amd64/arm64. MetalLB configurado com pool na LAN (10.0.10.x).

Já rodo Gopher em produção em `gopher.debene.dev:70` mas isso é **outra
coisa** — VPS RackNerd, geomyidae + WireGuard, fora do escopo deste projeto.
Este projeto **não toca no VPS**. Tudo aqui é homelab k8s.

Já existem, no ecossistema Rust Gopher: `gopher-cta`, `gopher-blog`,
`gopher-core` (biblioteca compartilhada — importar como dep),
`gopher-askthedeck` (dcgi Rust, layout de referência). Ver esses repos pra
convenções.

## Arquitetura

```
                ┌── Mac OS 9 (QEMU na Mac Studio 10.0.1.10) ──┐
                │                                              │
                │   TurboGopher              Audion 3          │
                │      │                       │               │
                └──────┼───────────────────────┼───────────────┘
                       │                       │
                  gopher:70                HTTP MP3:8000
                       │                       │
    ┌──────────────────┼───────────────────────┼──────────────┐
    │  k8s cluster (debene)                                    │
    │                                                          │
    │   ┌──────────────────────┐   ┌────────────────────────┐  │
    │   │ gopher-server         │   │ audio-stream          │  │
    │   │  Pod                  │   │  Pod                  │  │
    │   │   geomyidae container │   │   librespot|ffmpeg    │  │
    │   │   + gopher-spot dcgi  │   │                       │  │
    │   │  Service LoadBalancer │   │  Service LoadBalancer │  │
    │   │   IP LAN :70          │   │   IP LAN :8000        │  │
    │   └──────────┬────────────┘   └────────────────────────┘  │
    │              │                                             │
    │              └────────── Spotify Web API (via cluster      │
    │                          egress, HTTPS out)                │
    └──────────────────────────────────────────────────────────┘
```

**Dois Services LoadBalancer, dois IPs da pool MetalLB.** Anota ambos no
README depois do primeiro apply. Não usa DNS externo — LAN-only, IPs
diretos são suficientes. Se quiser dar nome, usa `.lan` no seu DNS interno
(Technitium 10.0.1.7): `gopher-spot.lan`, `audio-stream.lan`.

## Objetivo

1. **Pod `audio-stream`**: um container, `librespot --backend pipe | ffmpeg`
   produzindo MP3 128 kbps CBR em HTTP `:8000/spotify.mp3`. Service
   LoadBalancer da pool MetalLB.
2. **Pod `gopher-server`**: geomyidae + binário `gopher-spot` dcgi. Serve
   Gopher em `:70` na LAN via LoadBalancer. Controla playback via Spotify
   Web API (librespot cru não expõe API HTTP local).
3. **Multi-arch buildx** obrigatório: `linux/amd64` + `linux/arm64`. Cluster
   é misto (intel* + zima + ultra2 em amd64, orion em arm64). Sem surpresa
   de schedule.
4. **Sem `hostNetwork`, sem `nodeSelector`, sem pin de nó.** Scheduler
   decide onde roda.
5. **UI Gopher**: Mac OS 9 nunca vê MP3 pelo Gopher. Audion fica aberto num
   bookmark do stream. TurboGopher só busca/navega/controla. Menu inclui
   selector tipo `s` com `.pls` como conveniência pra reabrir Audion.

## Deliverables

Repo novo `gopher-spot`, layout:

```
gopher-spot/
├── README.md
├── PROMPT.md                         # este arquivo
├── Cargo.toml
├── src/                              # binário dcgi
├── docker/
│   ├── audio-stream.Dockerfile       # librespot + ffmpeg (multi-arch)
│   └── gopher-server.Dockerfile      # geomyidae + dcgi (multi-arch)
├── deploy/
│   ├── namespace.yaml                # ns: gopher-spot
│   ├── audio-stream.yaml             # Deployment + Service LB
│   ├── gopher-server.yaml            # Deployment + Service LB + ConfigMap
│   ├── secrets.yaml.template         # Spotify OAuth creds (não commitar)
│   └── kustomization.yaml
├── scripts/
│   ├── buildx.sh                     # multi-arch build + push
│   ├── spotify-oauth-init.sh         # one-shot OAuth → gera Secret
│   └── validate-turbogopher.md
```

### Container `audio-stream`

Alpine base. Multi-stage: builda librespot do source com features mínimas
(pipe backend + rodio *desabilitado*), copia binário strip pro final stage
com ffmpeg estático. Imagem alvo < 40 MB.

Entrypoint:

```sh
librespot --backend pipe --name gopher-spot --bitrate 320 \
          --device-type speaker --disable-audio-cache \
  | ffmpeg -re -f s16le -ar 44100 -ac 2 -i pipe:0 \
           -c:a libmp3lame -b:a 128k -f mp3 \
           -listen 1 -content_type audio/mpeg \
           http://0.0.0.0:8000/spotify.mp3
```

### Container `gopher-server`

Alpine base. Instala `geomyidae` (compilar do source, é pequeno). Copia
binário `gopher-spot` dcgi (buildado em stage anterior via `cargo build
--release`). geomyidae config aponta `/spot/*` selectors pra dcgi executável
via `-e`.

### Deployments

**audio-stream**:
- `replicas: 1` (librespot só tolera 1 device com o mesmo nome).
- `strategy: Recreate` (stream único, rolling não faz sentido).
- Requests: 100m CPU / 128Mi. Limits: 500m / 256Mi.
- Cache credentials do librespot: **emptyDir** por default (aceito
  re-zeroconf uma vez ao reiniciar, 10s no celular). Se você (CC) achar
  incômodo, PVC de 100Mi montado em `/cache` é OK.

**gopher-server**:
- `replicas: 2` (stateless, geomyidae não guarda estado).
- Requests: 50m CPU / 64Mi. Limits: 200m / 128Mi.
- Env vars do dcgi: `AUDIO_STREAM_URL` (o IP do Service audio-stream),
  Spotify OAuth via `envFrom: secretRef`.

### Services

Ambos `type: LoadBalancer`. MetalLB atribui da pool.

**Nota importante pro Fio A**: verificar se MetalLB tem pool disponível
antes de aplicar. `kubectl get ipaddresspools -A -n metallb-system`. Se
Service ficar `<pending>`, checar `kubectl describe svc` e configuração
MetalLB antes de assumir bug de app.

### Multi-arch build

Registry `ghcr.io/felipedbene/*` (padrão do cluster).

### dcgi `gopher-spot` — selectors

```
iSpotify pelo Gopher, safadinho.
i
1Now Playing              /spot/now
7Buscar                   /spot/search
1Minhas playlists         /spot/playlists
1Controles                /spot/control
sReabrir stream (Audion)  /spot/stream.pls
```

Endpoints:
- `/spot/now` → `GET /v1/me/player/currently-playing` → texto formatado.
- `/spot/search` (tipo 7) → `GET /v1/search?type=track,album,artist&limit=20`.
- `/spot/track/{id}` → detalhes + selector "▶ Tocar agora" →
  `/spot/play?uri=spotify:track:{id}`.
- `/spot/play?uri=...` → `PUT /v1/me/player/play` com `device_id` do
  `gopher-spot` (achado via `GET /v1/me/player/devices`, cache 30s).
- `/spot/control/{pause,next,prev,vol/N}` → Web API endpoints correspondentes.
- `/spot/playlists` → `GET /v1/me/playlists` (paginado, 20/página).
- `/spot/playlists/{id}` → `GET /v1/playlists/{id}/tracks`.
- `/spot/stream.pls` → PLS estático apontando pro `AUDIO_STREAM_URL` env.

Deps prováveis: `gopher-core`, `reqwest` (async), `serde`, `serde_json`,
`oauth2`, `tokio`.

### OAuth

`scripts/spotify-oauth-init.sh` roda local 1x. Authorization Code flow,
redirect `http://localhost:8888/callback`. Scopes:

```
user-read-private user-read-playback-state user-modify-playback-state
user-read-currently-playing playlist-read-private playlist-read-collaborative
user-library-read
```

Output: preenche `deploy/secrets.yaml.template` → `secrets.yaml` com
client_id, client_secret, refresh_token. `kubectl apply -f secrets.yaml`.
Dcgi lê refresh no startup, renova access em memória.

## Constraints

- **Rust** pro dcgi. Async (tokio).
- **Vanilla k8s** (kubeadm 1.36). YAML puro, kustomize OK. **Nunca K3s.**
- **LoadBalancer** via MetalLB (já configurado).
- **Multi-arch buildx** obrigatório.
- **Sem `hostNetwork`, sem `nodeSelector`.** Scheduler decide.
- **LAN-only.** Sem Ingress, sem TLS, sem publicação externa.
- **Nunca force-delete de pod em nó com GPU** (regra permanente do cluster).
- **Line width Gopher**: display ≤ 66 chars (RFC 1436). MacRoman-safe.
- **Secrets**: `secrets.yaml` no `.gitignore`. Só `secrets.yaml.template`
  commitado.
- **Cache Web API**: search 5min em memória, playlists 60s, devices 30s.

## Non-goals

- Não toca no VPS `gopher.debene.dev`.
- Não reimplementa Spotify Connect (usa librespot).
- Não faz GUI em nenhum lugar. Só Gopher.
- Não suporta Spotify Free (librespot exige Premium).
- Não implementa gapless entre faixas.
- Não implementa cover art (fio futuro, talvez).
- Não expõe publicamente. LAN-only.

## Fios (1 sub-fio = 1 commit)

### Fio A — audio-stream pod
Dockerfile multi-arch. Deployment + Service LoadBalancer. Verifica MetalLB
pool antes de aplicar. Decide emptyDir vs PVC.
Commit: `fio A: audio-stream pod (librespot | ffmpeg, LB service, multi-arch)`

### Fio B — gopher-server pod + dcgi skeleton
Dockerfile multi-arch. Deployment + Service :70. Menu raiz estático.
Commit: `fio B: gopher-server pod + dcgi skeleton`

### Fio C — OAuth + Web API (search, control, now-playing)
Script OAuth init. Secret k8s. Renova token. search + track + play + control.
Commit: `fio C: OAuth + web api (search, control, now-playing)`

### Fio D — Playlists + validação OS 9 + post no blog
`/spot/playlists`. Selector `s`. Testa OS 9 QEMU. Doc quirks. Blog post.
Commit: `fio D: playlists + validação OS 9 + blog post`

## Regras de comportamento

- Zoeira BR OK, commits em inglês técnico.
- Se algo cheirar mais gambiarra que a média, para e pergunta.
- Se librespot falhar em algo prometido, para e pergunta.
- Respeita os caches definidos.
- Se um Service ficar `<pending>` >60s no Fio A, para e checa MetalLB.
