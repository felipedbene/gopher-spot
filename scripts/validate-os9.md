# Validating gopher-spot on Mac OS 9

The Gopher side must render clean in the **OS 9 gopher client** — no MacRoman
garbage, links that follow, item types that look right. The reference client here
is **Netscape Communicator's built-in Gopher browser** (the location bar takes
`gopher://<host>:70/`); **TurboGopher** works the same way where noted. Audio comes
out of **MacAST** parked on the stream. This is the manual validation loop; fill in
the "Observed" boxes as you go.

## 0. Point the client at a server

Two ways to get a reachable server in front of the OS 9 guest:

**A. The real cluster** (needs the guest's network to route to `10.0.100.x`):
in Netscape, open `gopher://10.0.100.112:70/`. (TurboGopher: `Gopher ▸ Another
Gopher…` → host `10.0.100.112`, port `70`.)

**B. A local instance on the QEMU host** (no cluster routing needed). On the Mac
that runs the guest, publish the server on the vmnet interface the guest can see
(`192.168.64.1` here) and set `GOPHER_HOST` to that same IP so links follow:

```sh
docker run --rm -p 192.168.64.1:7070:70 \
  -e GOPHER_HOST=192.168.64.1 \
  -e AUDIO_STREAM_URL=http://192.168.64.1:8000/spotify.mp3 \
  ghcr.io/felipedbene/gopher-spot-server:latest
```

Then open `gopher://192.168.64.1:7070/` (TurboGopher: host `192.168.64.1`, port
`7070`).

> `GOPHER_HOST` must equal the address the guest dials, or link-following breaks:
> geomyidae stamps it into every link's host token. Wrong host = "server not
> found" when you click into `/spot/*`.

## 1. Root menu

Expect five items with the right gopher item types:

| Item                    | Type | Rendered as        |
|-------------------------|------|--------------------|
| Now Playing             | 1    | menu / folder      |
| Buscar                  | 7    | search             |
| Minhas playlists        | 1    | menu / folder      |
| Controles               | 1    | menu / folder      |
| Reabrir stream (MacAST) | s    | sound              |

- [ ] Observed: ______________________________________________

## 2. Search (type 7)

Select **Buscar** → the client opens a query box → type `chico buarque` → Enter.
Expect a list of tracks, each linking to `/spot/track/{id}`.

- [ ] Query box appears (type-7 works)
- [ ] Results render; accents in track names are clean (see §5)
- [ ] Observed: ______________________________________________

## 3. Play a track

Open a track → **>> Tocar agora** → expect the "Mandando tocar" confirmation.
Audio comes out of **MacAST** (parked on the audio stream bookmark), NOT the gopher
client — the Gopher side never carries MP3.

- [ ] Playback starts on the gopher-spot device
- [ ] Observed: ______________________________________________

## 4. Controls & playlists

- [ ] `Controles` → Pause / Proxima / Anterior / Volume act on playback
- [ ] `Minhas playlists` → lists playlists; opening one lists its tracks
- [ ] Pagination: `>> Proxima pagina` / `<< Pagina anterior` appear only when
      there are more than 20 items and move the window
- [ ] Observed: ______________________________________________

## 5. The type-s PLS item (MacAST reopen)

**Reabrir stream (MacAST)** is a type-`s` (sound) item whose selector
`/spot/stream.pls` returns a raw `.pls` pointing at `AUDIO_STREAM_URL`. The intent
is a one-click way to (re)open the stream in MacAST.

Known quirk to check: whether the fetched `.pls` opens in MacAST depends on the
OS 9 file-type/creator or helper-app mapping in the browser. If it downloads as a
text blob instead of launching MacAST:

- Record what actually happens (download? open? which app?).
- The `.pls` content is correct regardless (verify: it lists `File1=<audio LB>`).
- Fallback that always works: keep MacAST pointed at the audio LB directly
  (`http://10.0.100.113:8000/spotify.mp3`); this item is a convenience, not the
  primary path.

- [ ] Observed behavior: ______________________________________

## 6. MacRoman + line width

The dcgi transcodes output to MacRoman at the IO edge, so accented names should
render correctly (á é í ó ú ã õ ç…), not as mojibake.

- [ ] Accented track/artist names look right (e.g. "Construção", not "ConstruÃ§Ã£o")
- [ ] No line wraps oddly — displays are clipped to 66 columns
- [ ] Non-Latin names (if any) show `?` rather than garbage (expected)
- [ ] Observed: ______________________________________________

## 7. Known client gotchas (pre-filled; confirm/extend)

- **Caching:** gopher clients cache fetched menus aggressively. After changing
  playback, re-fetch (don't trust a stale "Now Playing"). In Netscape use Reload;
  in TurboGopher close and reopen the item.
- **Ports:** both clients are fine with non-70 ports; enter them explicitly
  (`gopher://host:7070/` in Netscape).
- **Empty menus:** an all-`i` (info-only) menu with no selectable items is normal
  for confirmation/error pages — shown as plain lines.
- Add anything you hit: ______________________________________
