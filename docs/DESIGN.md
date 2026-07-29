# LP Frame — Design Document

**Status:** Signed off; implementation in progress
**Repo:** `tunlezah/AudioView`
**Product:** A vinyl-LP-sized wall/shelf display that shows the cover art of whatever is currently AirPlaying to it, and nothing else.

---

## 1. Summary

LP Frame is a single-purpose appliance. A Raspberry Pi runs an AirPlay 2 receiver, feeds audio to a USB DAC, raises a GPIO line to wake an amplifier, and renders the current track's album art full-bleed on a square panel with GPU crossfades. There is no UI, no text, no chrome. When music stops, the screen fades to black and the panel is powered down.

The one non-obvious feature is **artwork enrichment**: AirPlay delivers cover art at roughly 500×500, which looks soft on a 1920×1920 panel. We look the album up against public catalogues, fetch art at up to 3000×3000, verify it actually matches, and hot-swap it in behind a short crossfade.

### Design principles

1. **The AirPlay art is the source of truth.** Enrichment is best-effort decoration. Network down, API down, no match — the device behaves identically minus sharpness.
2. **The renderer is dumb.** It has no notion of AirPlay, tracks, or the network. It receives "display this file" and "go to sleep". Every policy decision lives in `artd`.
3. **Idle costs nothing.** A static image means zero page flips, zero GPU work. A dark screen means the CRTC is off.
4. **Everything runs on a laptop.** No component requires a Pi to build, run, or test.

### Terminology

| Term | Meaning |
|---|---|
| *pipe* | The shairport-sync metadata FIFO, default `/tmp/shairport-sync-metadata` |
| *item* | One XML-framed metadata record on the pipe |
| *bundle* | A group of `core` items delimited by `mdst`/`mden`, describing one track |
| *revision* | Monotonic counter identifying a distinct artwork image to display |
| *enrichment* | Replacing AirPlay art with a higher-resolution copy from a public catalogue |

---

## 2. Hardware and OS decisions

### 2.1 Target hardware

| | Primary | Fallback | Development |
|---|---|---|---|
| Board | Raspberry Pi 5 (4GB+) | Raspberry Pi 4B (2GB+) | any x86_64 Linux |
| Panel | 1920×1920 square via HDMI→eDP board | same | Waveshare 4" 720×720 HDMI, or an SDL2 window |
| Audio | USB Audio Class 2 DAC (e.g. FiiO KA-series) | same | any ALSA device / `null` |
| Amp trigger | 1× GPIO → optocoupler → 12V trigger | same | stub GPIO backend |
| Network | Ethernet preferred, Wi-Fi fallback | same | whatever |

### 2.2 OS: Raspberry Pi OS Lite 64-bit — *not* Alpine

**Recommendation: Raspberry Pi OS Lite (Debian trixie base), 64-bit.**

The image-size argument for Alpine is real but small — we are talking a few hundred MB on a card that will be 16GB minimum. What we would pay for it is significant:

- **Mesa v3d/vc4.** Raspberry Pi ships and tests a specific Mesa against a specific kernel and firmware. On the Pi 5 the BCM2712 display pipeline (`vc4` driver, "MOP"/"MOPLET" blocks) is still seeing active upstream work; being on the vendor's tested combination is worth a lot when the entire product is one GLES surface on a KMS plane. Alpine's Mesa is fine on desktop GPUs and is not a tested configuration here.
- **Firmware and device tree.** `dtoverlay=vc4-kms-v3d`, custom HDMI timings for a non-standard 1920×1920 mode, `cma=` sizing, and the Pi 5's RP1 pin controller all assume Pi OS's `config.txt` / `firmware` tooling. Reproducing this on Alpine is possible and is a maintenance liability forever.
- **musl vs glibc.** shairport-sync with AirPlay 2 pulls in libplist, libsodium, libgcrypt, and ffmpeg. All build on musl, none are *tested* on musl by their maintainers. This is exactly the class of bug that shows up as intermittent audio dropouts three weeks in.
- **Tooling.** `libgpiod` v2, `alsa-utils`, `raspi-config`'s overlayfs toggle, `rpi-eeprom`, `pinctrl`, `vcgencmd` — all first-class on Pi OS.

We recover most of Alpine's benefit through configuration rather than distro choice: Lite has no desktop, we mask what we don't need, we use overlayfs for a read-only root, and we set journald to volatile storage. Expect a ~1.2GB installed footprint and ~15s boot to first frame.

**Base release:** trixie-based Raspberry Pi OS Lite (64-bit). Bookworm works and is a documented fallback; the installer detects and adapts.

### 2.3 shairport-sync: build from source

Distro packages historically ship without `--with-airplay-2` because nqptp is a separate daemon. We pin and build both from source, verified by `shairport-sync -V` reporting `AirPlay2`. The installer checks for a distro package with AP2 already compiled in and uses it if present, so this cost disappears when Debian catches up.

---

## 3. Architecture

```
                    ┌───────────────────────────────────────────────┐
   iPhone / Mac     │  Raspberry Pi                                 │
   ──AirPlay 2──────┼─▶ nqptp (PTP clock, root, udp/319+320)        │
                    │        │                                      │
                    │        ▼                                      │
                    │   shairport-sync ──ALSA──▶ USB DAC ──▶ Amp    │
                    │        │                                ▲     │
                    │        │ metadata FIFO                  │     │
                    │        ▼                            12V trig  │
                    │   ┌─────────┐                            │    │
                    │   │  artd   │──libgpiod──────────────────┘    │
                    │   │         │                                 │
                    │   │ parser  │◀──HTTPS──▶ iTunes Search API    │
                    │   │ state   │            MusicBrainz + CAA    │
                    │   │ enrich  │                                 │
                    │   │ power   │──▶ /var/lib/lpframe/cache/      │
                    │   └────┬────┘──▶ /run/lpframe/art/  (tmpfs)   │
                    │        │                                      │
                    │   NDJSON over AF_UNIX                         │
                    │   /run/lpframe/artd.sock                      │
                    │        │                                      │
                    │        ▼                                      │
                    │   lprender ──KMS/DRM + GBM + EGL + GLES3──▶ 🖼 │
                    └───────────────────────────────────────────────┘
```

### 3.1 Why this split

`artd` and `lprender` are separate processes because they have incompatible failure and latency profiles. The renderer must never block: a stalled HTTPS request or a slow SD-card write cannot be allowed to drop a frame. Conversely, a renderer crash (GPU driver wedge, hotplug race) must not lose session state or leave the amp powered on. Separate processes with a supervised socket between them gives us independent restart, and lets us run either one alone during development.

They are not separate because of language boundaries — both are Rust.

### 3.2 Language: Rust for both

A single Cargo workspace, one toolchain, one cross-compilation story, shared types for the IPC schema so the protocol cannot drift between the two ends.

- `artd`: `tokio` async runtime, `reqwest` (rustls) for HTTP, `serde`, `image`/`zune-jpeg` for decode, `rusqlite` (bundled) for the cache index, `gpiocdev` for libgpiod v2 character-device GPIO.
- `lprender`: `drm-rs`, `gbm`, `khronos-egl`, `glow` for GLES3, `sdl2` behind a feature flag for the dev backend. No async runtime — a hand-rolled `epoll` loop, because frame pacing wants explicit control.

C was considered for `lprender`. The DRM/GBM/EGL crates are thin bindings over the same libraries, so we lose nothing, and we gain the shared protocol crate.

### 3.3 Repository layout

```
AudioView/
├── Cargo.toml                  # workspace
├── crates/
│   ├── lpframe-proto/          # IPC message types (serde), shared by both ends
│   ├── lpframe-config/         # TOML config schema + loader + validation
│   ├── lpframe-image/          # OKLab, perceptual hash, resampling (both ends)
│   ├── spmeta/                 # shairport-sync metadata pipe parser (pure lib)
│   ├── artd/                   # metadata + artwork + power daemon
│   ├── lprender/               # KMS/DRM renderer (+ SDL2 dev backend)
│   └── lpcapture/              # fixture capture / replay tool
├── fixtures/
│   ├── sessions/               # captured pipe byte streams + golden event logs
│   └── art/                    # content-addressed PICT payloads (git-lfs)
├── systemd/
├── provisioning/
│   ├── install.sh
│   └── pi-gen/
├── docs/
│   ├── DESIGN.md               # this file
│   └── BUILD.md                # full OS build + install guide (written last)
└── Makefile
```

---

## 4. `spmeta` — the metadata parser

### 4.1 Wire format

shairport-sync writes a stream of XML-framed items to the FIFO. Verified against `shairport-sync-metadata-reader`, whose parser scans for:

```
<item><type>%8x</type><code>%8x</code><length>%zu</length>
```

Zero-length item:

```
<item><type>73736e63</type><code>70626567</code><length>0</length></item>
```

Item with payload:

```
<item><type>636f7265</type><code>6d696e6d</code><length>11</length>
<data encoding="base64">
SGVsbG8gV29ybGQ=</data></item>
```

`type` and `code` are FourCCs rendered as 8 hex digits. `length` is the *decoded* byte length. The base64 payload is line-wrapped. This is a byte stream, not a document — there is no root element, and a reader must be resilient to starting mid-item (which happens on every reconnect).

### 4.2 Parser design

A `#![forbid(unsafe_code)]` pull parser over a growable byte buffer, driven by `feed(&[u8]) -> impl Iterator<Item = Result<MetaItem>>`. No allocation per item beyond the payload itself.

Recovery rule: on any framing error, scan forward for the next literal `<item>` and resume, emitting a `ParseError` event so we can count them in metrics. Starting mid-stream therefore costs at most one dropped item.

Hard limits, because this is parsing attacker-adjacent input (anything on the LAN can AirPlay to us): `length` is rejected above `max_item_bytes` (default 8 MiB, covering any plausible cover art), the reassembly buffer is capped, and base64 decode is streaming rather than buffered-then-decoded.

```rust
pub struct MetaItem {
    pub kind: FourCc,          // b"ssnc" | b"core"
    pub code: FourCc,
    pub payload: Bytes,        // empty for zero-length items
}
```

A second layer, `spmeta::Decoder`, turns `MetaItem`s into typed events, decoding DMAP payloads by code:

```rust
pub enum MetaEvent {
    ActiveBegin, ActiveEnd,                 // abeg / aend
    PlayBegin, PlayEnd, PlayFlush,          // pbeg / pend / pfls
    PlayResume, FirstFrame,                 // prsm / pffr
    BundleStart, BundleEnd,                 // mdst / mden
    PictureStart, PictureEnd,               // pcst / pcen
    Picture(Bytes),                         // PICT (may be zero-length = "no art")
    Progress { start: u32, current: u32, end: u32 },   // prgr
    Volume { airplay_db: f32, .. },         // pvol
    ClientName(String), UserAgent(String),  // snam / snua
    Stall,                                  // stal
    Core(CoreField),                        // asal/asar/minm/…
    Unknown { kind: FourCc, code: FourCc, len: usize },
}
```

### 4.3 Codes we consume

**`ssnc` (shairport-sync itself)**

| Code | Meaning | Used for |
|---|---|---|
| `abeg` / `aend` | Active mode entered / exited | Amp trigger, session lifecycle |
| `pbeg` / `pend` | Play stream begin / end | Display wake / sleep |
| `pffr` | First frame received and validly timed | Confirms audio is actually flowing |
| `pfls` | Play stream flush | Seek or pause |
| `prsm` | Play stream resume | Resume |
| `mdst` / `mden` | Metadata bundle start / end | Atomic track commit |
| `pcst` / `pcen` | Picture transmission start / end | Artwork framing |
| `PICT` | Cover art payload (JPEG or PNG) | The whole point |
| `prgr` | Progress, RTP timestamps | Position, liveness |
| `pvol` | Volume in dB | Diagnostics only |
| `snam` / `snua` | Client device name / user agent | Diagnostics, status page |
| `stal` | Metadata reception stall | Watchdog input |
| `svip` / `clip` | Server / client IP | Diagnostics |
| `conn` / `disc` | AirPlay 2 client connect / disconnect | Session lifecycle corroboration |

**`core` (DMAP/DAAP, per track)**

`minm` title · `asar` artist · `asal` album · `asaa` album artist · `asgn` genre · `ascp` composer · `astm` duration (ms) · `astn` track number · `mper` persistent track ID · `ascm` comment · `asdt` description

**Deliberately ignored:** `copl` (RTSP plist), `cmod`, `cdid`, `cmac`, `daid`, `acre`, `dapo`, `sdsc`, `odsc`, `phb0`, `phbt`, `styp`. Some are remote-control plumbing we do not need (we are a receiver, not a controller); others are undocumented. `paus` / `pres` appear in newer builds and are *probably* AirPlay 2 pause/resume, but their exact semantics relative to `pfls`/`prsm` are unverified — this is precisely what the fixture captures are for, and until a real capture confirms them the state machine does not depend on them. They are logged as `Unknown`, and adding them later is a one-line change.

### 4.4 Tests

Table-driven unit tests for framing (truncated items, split base64, garbage prefix, oversized length, interleaved whitespace), plus fixture replay: feed `fixtures/sessions/*.pipe` byte-by-byte *and* in random-sized chunks, assert the emitted event sequence equals the golden `*.events.json`. Chunk-size fuzzing is not optional — the FIFO delivers arbitrary boundaries and this is where naive parsers break.

---

## 5. `artd` — metadata, artwork, and power daemon

### 5.1 Pipe consumption vs. session hooks

**Open question 1 answered: consume the pipe, do not use `run_this_before_play_begins` / `run_this_after_play_ends`.**

Reasons:

1. **We need the pipe anyway.** Track metadata and artwork only exist there. Hooks would be a *second* event source that must be reconciled with the first, and reconciling two clocks is strictly worse than reading one.
2. **Ordering.** Hook processes are forked and scheduled independently. There is no guarantee that a hook's side effect lands before or after the corresponding pipe items. With one source, ordering is the stream order, by definition.
3. **Hooks can stall audio.** With `wait_for_completion` set, shairport-sync blocks on the hook before starting playback. A slow script becomes audible latency. Without it, ordering gets worse.
4. **Resolution.** The pipe distinguishes `abeg` (active mode entered) from `pbeg` (stream starting) from `pffr` (audio genuinely flowing). Hooks collapse this to one edge. See §5.1.1 — this distinction is what makes correct amp control possible.

#### 5.1.1 `abeg`/`aend` vs `pbeg`/`pend` — and why the amp uses the former

These are two different scopes, and confusing them produces a device that clicks its amplifier relay between every track.

| | `pbeg` / `pend` | `abeg` / `aend` |
|---|---|---|
| Scope | One audio stream | The whole listening session |
| AirPlay 2 behaviour | Fires around **each track** | Fires once at the start, once at the end |
| Trailing edge | Immediate | Delayed by `active_state_timeout` (default 10s) |
| Four-track listen | 4 × begin/end pairs | 1 × begin/end pair |

shairport-sync enters *active mode* when audio first arrives and stays active across the gaps between tracks. When audio stops it starts a timer; if audio resumes before `active_state_timeout` expires it never leaves active mode, and only a real stop produces `aend`. Upstream is explicit that play events "have been superseded by Active/Inactive events, which works better in AirPlay 2 operation."

Consequences for us:

- **Amp trigger keys off `abeg`/`aend`.** Built-in hysteresis, one relay transition per listening session. Our own `off_delay` (10 min) would mask the flapping anyway, but relying on a long delay to paper over a wrong signal is the kind of thing that breaks when someone shortens the delay.
- **Display blanking keys off `Idle`, which is entered on `aend` — not on `pend`.** Between tracks the state machine sits in `Active`, so the blank countdown never starts mid-album. This is load-bearing: keying the blank timer off `pend` would start a countdown between every track.
- **`pbeg`/`pend`/`pffr` still drive the display wake and the playing/paused indication**, because there we *want* per-track resolution.
- We set `active_state_timeout` explicitly in the shipped shairport-sync config rather than inheriting the default, so our timing assumptions are written down in one place.

**Abrupt client disconnect** is the failure mode hooks are usually reached for, so it needs a real answer:

- **Normal case.** shairport-sync's RTSP teardown emits `pend` then `aend`. On AirPlay 2, keepalive loss takes a few seconds to tens of seconds. Handled by the state machine directly.
- **Silent case.** Sender vanishes (phone in a Faraday cage, Wi-Fi drop) and shairport-sync hasn't timed out yet. A **watchdog** covers it: in `Playing` with no pipe activity and no `prgr` for `stall_timeout` (default 15s), or on `stal`, transition to `Paused`; after `session_timeout` (default 60s), transition to `Idle`.
- **Catastrophic case.** shairport-sync itself dies or restarts. Our `read()` on the FIFO returns **EOF** the moment the last writer closes. We treat EOF as an unambiguous "writer gone" → force `Idle` → reopen the FIFO (blocking, which parks until a writer appears).

That last point is a genuine fork in the design, so to be explicit: a common trick is to hold a dummy `O_WRONLY` descriptor on the FIFO so `read()` never returns EOF and you get one uninterrupted stream. **We deliberately do not do this.** The EOF is the single most reliable signal we get that shairport-sync is gone, and trading it for a tidier read loop would leave the amp powered on and the screen lit after a crash. We take the reopen churn.

Cost of a false `Idle` is near zero: the amp-off delay is minutes, the display-off delay is minutes, and any subsequent pipe activity instantly re-wakes both.

`artd` starts *before* shairport-sync (systemd ordering) and creates the FIFO itself, because shairport-sync drops metadata written when no reader is attached — without this we lose the first bundle of every boot.

### 5.2 State machine

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Active: abeg  (or pbeg, if sender omits abeg)
    Active --> Starting: pbeg
    Starting --> Playing: pffr
    Starting --> Active: pend
    Playing --> Paused: pfls / stal / stall_timeout
    Paused --> Playing: prsm / pffr / prgr advancing
    Playing --> Active: pend
    Paused --> Active: pend
    Active --> Idle: aend / session_timeout
    Playing --> Idle: EOF / session_timeout
    Paused --> Idle: EOF / session_timeout
```

Notes:

- **The `Active ⇄ Starting ⇄ Playing` loop is the normal case, not an edge case.** Per §5.1.1, `pbeg`/`pend` cycle once per track, so a four-track album traverses that loop four times inside a single `abeg`…`aend` envelope. Nothing outside the loop — amp state, display blanking — may be driven from those transitions.
- **Senders that never emit `abeg`/`aend`.** These are AirPlay-2-era codes; AirPlay 1 senders and some third-party ones skip them. `pbeg` therefore implicitly enters `Active` first. Symmetrically, `pend` with no `aend` within `session_timeout` falls through to `Idle`.
- **`Starting` exists** so we don't wake the panel for a session that connects and never plays. If `pffr` never arrives, we stay dark.
- **Metadata bundles are transactional.** `core` items between `mdst` and `mden` accumulate into a pending track; the track is committed — and a `track_changed` event emitted — only at `mden`. A bundle interrupted by `pend` is discarded. This prevents the classic flicker where artist updates one frame before title.
- **Artwork is independent of the bundle.** `PICT` typically arrives after `mden`, bracketed by `pcst`/`pcen`. A zero-length `PICT` means "this track has no art" and clears to the configured placeholder.
- **Track identity** is `mper` when present, otherwise `sha256(normalize(artist) ‖ normalize(album) ‖ normalize(title))`. Identity drives "is this actually a new track?" — repeated identical bundles (common) must not retrigger a crossfade.

### 5.3 IPC: NDJSON over a Unix socket

`SOCK_STREAM` at `/run/lpframe/artd.sock`, mode `0660`, owner `lpframe:lpframe`. One newline-delimited JSON object per message, UTF-8, no embedded newlines. Multiple concurrent clients (renderer, status page, `lpctl` debug CLI).

**Design choice: full-snapshot-on-every-change, not deltas.** Every state change publishes the complete state object with an incremented `seq`. The renderer holds no history and reconstructs nothing; it diffs the snapshot against what it is currently showing. A client that connects late, reconnects, or misses a message is immediately correct. The messages are small (a few hundred bytes) and change a handful of times per track — there is no efficiency argument for deltas here, and delta protocols are where resync bugs live.

**Server → client**

```json
{"v":1,"type":"state","seq":42,"ts":"2026-07-27T10:14:02.113Z","state":{
  "playback": "playing",
  "session": {
    "active": true,
    "client_name": "Ben's iPhone",
    "user_agent": "AirPlay/845.5.1"
  },
  "track": {
    "id": "b1946ac92492d234",
    "title": "Teardrop",
    "artist": "Massive Attack",
    "album": "Mezzanine",
    "album_artist": "Massive Attack",
    "genre": "Electronica",
    "duration_ms": 330000,
    "track_number": 3
  },
  "artwork": {
    "revision": 7,
    "path": "/run/lpframe/art/0007-4f3a9c1e.jpg",
    "sha256": "4f3a9c1e…",
    "width": 3000,
    "height": 3000,
    "source": "itunes",
    "is_upgrade": true,
    "placeholder": false
  },
  "progress": {"position_ms": 12480, "duration_ms": 330000},
  "volume": {"airplay_db": -14.5, "muted": false},
  "power": {"amp": true, "display": "on"}
}}
```

| Field | Notes |
|---|---|
| `playback` | `idle` \| `active` \| `starting` \| `playing` \| `paused` |
| `track` | `null` when unknown. Any string field may be absent. |
| `artwork.revision` | Monotonic. **The renderer's sole trigger for a crossfade.** |
| `artwork.path` | File on tmpfs. Bytes never traverse the socket. |
| `artwork.is_upgrade` | `true` when this replaces the *same* album's art with a higher-resolution copy. The renderer uses `enrichment_crossfade_ms` (250ms) rather than the full 600ms, because it is the same picture getting sharper, not a new record. |
| `artwork.source` | `airplay` \| `itunes` \| `coverartarchive` \| `cache` \| `placeholder` |
| `power.display` | `on` \| `ambient` \| `off` — a *request*; the renderer owns the transition. |

Also emitted: `{"type":"hello","v":1,"server":"artd","version":"0.1.0"}` on connect (immediately followed by a full `state`), `{"type":"pong"}`, and `{"type":"log","level":"warn","msg":"…"}` for surfacing enrichment failures to the status page.

**Client → server**

```json
{"v":1,"type":"hello","client":"lprender","proto":1}
{"v":1,"type":"get_state"}
{"v":1,"type":"ping"}
{"v":1,"type":"set_display","value":"on"}        // manual override, expires after 5 min
{"v":1,"type":"inject_artwork","path":"/tmp/x.jpg"}   // --debug builds only
```

Version negotiation: `v` is the envelope version. A client sending an unknown `proto` gets a `hello` with the server's supported range and the connection stays open at the highest mutually supported version. Unknown message types and unknown fields are ignored, not fatal — this is how we add a second metadata source later without a flag day.

**Artwork file lifecycle.** Images are written to `/run/lpframe/art/` (tmpfs) as `NNNN-<sha8>.jpg`, written to a temp name and `rename()`d so a reader never sees a partial file. `artd` retains the last 4 revisions and unlinks older ones after a 60-second grace period, so the renderer can never have a path pulled out from under it mid-decode.

### 5.4 Artwork enrichment

**Open question 3 answered: both gates, in sequence — text match selects the candidate, perceptual match authorises the swap.**

Text matching alone gets "Greatest Hits" wrong in ways that are viscerally annoying on a wall. Histogram matching alone rejects legitimate regional cover variants and different masters, which is the common case for exactly the albums where a high-res upgrade matters most. Neither is sufficient; together they are cheap and strict.

```
track committed (mden)
        │
        ▼
  display AirPlay art immediately          ◀── always, unconditionally, revision++
        │
        ▼
  cache lookup: key = sha256(norm(artist) ‖ norm(album))
        │
        ├── hit ──▶ verify file, display it (source=cache) ──▶ done
        ├── negative-cache hit (< 7 days) ──▶ done, no network
        │
        ▼
  rate limiter (token bucket, 15/min iTunes) + single-flight per key
        │
        ▼
  ① iTunes Search: entity=album, term="artist album", limit=10, country=<config>
        │
        ▼
  ② TEXT GATE
     normalise: lowercase, strip diacritics, strip "(Deluxe Edition)",
                "- Remastered 2011", "[Explicit]", collapse punctuation
     Jaro-Winkler: artist ≥ 0.90  AND  album ≥ 0.85
        │  fail → try MusicBrainz release-group → Cover Art Archive front-1200
        │         (1 req/s, UA with contact string) → same gate → else give up
        ▼
  ③ FETCH: rewrite artworkUrl100's "100x100bb.jpg" → "3000x3000bb.jpg"
     Apple serves the largest available if the request exceeds it; 3000×3000 is
     the documented ceiling. Fall back 1500 → 1000 → 600 on 404.
        │
        ▼
  ④ SIZE GATE: candidate's min dimension ≥ 1.5 × AirPlay art's min dimension.
     A 600×600 "upgrade" over 500×500 art is not worth a swap.
        │
        ▼
  ⑤ PERCEPTUAL GATE  (both must pass)
     downscale both to 32×32, convert to OKLab
     (a) dHash Hamming distance ≤ 12 / 64
     (b) dominant-colour histogram cosine similarity ≥ 0.85
     BYPASS: skip ⑤ if text scores are near-exact (artist ≥ 0.97 AND album ≥ 0.97)
             — covers legitimate alternate cover art for an unambiguous album
        │
        ▼
  ⑥ SWAP: write to cache + tmpfs, revision++, is_upgrade=true
     renderer crossfades over 250ms
```

**Strictness is configurable** — `enrichment.strictness = "off" | "text_only" | "text_and_visual" | "strict"`, default `text_and_visual`. `strict` removes the near-exact bypass in ⑤. This is the knob to reach for if wrong-album swaps ever show up in real use.

**Failure handling.** Any network error keeps the AirPlay art and schedules a retry with exponential backoff, capped at 3 attempts per track, abandoned on track change. HTTP 429 honours `Retry-After` and pauses the whole bucket. Offline is not an error state — it is the normal state of a device on a flaky Wi-Fi network, and it must be silent.

**Rate limiting.** iTunes fair-use is ~20 req/min per IP; we budget 15. MusicBrainz requires ≤1 req/s and a User-Agent carrying a contact address, taken from config. Enrichment runs on a bounded worker pool (2 tasks) so it can never starve the pipe reader.

**Privacy.** Enrichment transmits artist and album strings to Apple and/or MusicBrainz. `enrichment.enabled` (default `true`) turns it off entirely; this is documented prominently in the config file, not buried.

**Cache.** `/var/lib/lpframe/cache/<aa>/<sha256>.jpg` with an SQLite index (`key, sha256, bytes, width, height, source, created_at, last_access`). LRU eviction to `cache.max_bytes` (default 2 GiB), swept on startup and hourly. Negative entries carry a 7-day TTL. SQLite rather than a bare directory because eviction needs an atomic, crash-safe index, and rebuilding LRU order from `stat()` after a power cut is exactly the kind of thing that quietly stops working.

### 5.5 Power management

**Amplifier trigger.** One GPIO line via libgpiod v2 character device.

Pi 5 moved the 40-pin header to the RP1 pin controller and the `gpiochipN` numbering has shifted between kernel releases. **Resolve the chip by label, never by index** — `pinctrl-rp1` on Pi 5, `pinctrl-bcm2835` on Pi 4. Config accepts `chip = "auto"` (probe for a header controller) or an explicit label.

```toml
[power.amp]
gpio_chip   = "auto"
gpio_line   = 17
active_low  = false
pulse_ms    = 0        # 0 = hold level while on; >0 = momentary pulse for toggle-style amps
on_event    = "session_begin"   # abeg — one transition per listening session (§5.1.1)
off_delay   = "10m"             # after entering Idle
```

`on_event = "play_begin"` (`pbeg`) is available but **not recommended**: it fires per track, so the relay would cycle between songs. It exists for amplifiers whose trigger input is genuinely momentary.

**Hardware requirement, documented in the build guide:** the optocoupler input must have a pull-down (or pull-up, if `active_low`) resistor to a safe state. When `artd` exits, the kernel releases the GPIO request and the line reverts to its default — without an external resistor, a crash could leave the amplifier powered indefinitely. `artd` also drives the line off in its `ExecStop` and on `SIGTERM`, but that only covers clean shutdown; the resistor covers the rest.

Both amp and display transitions are debounced (default 2s) so a track skip that briefly passes through `Idle` cannot chatter a relay.

**Display power** is requested by `artd` and executed by `lprender`:

| Time in `Idle` | `power.display` | Renderer behaviour |
|---|---|---|
| 0 | `on` | Artwork at full brightness |
| > `ambient_after` (default off) | `ambient` | Blurred, dimmed last artwork |
| > `blank_after` (default 5m) | `off` | Fade to black over 1s, then CRTC `ACTIVE=0` |

The clock starts on entry to `Idle` (i.e. on `aend`), **not** on `pend` — see §5.1.1. Gaps between tracks leave the state machine in `Active`, where the display stays fully on and no countdown runs.

Wake is instant on `abeg`/`pbeg`: CRTC on, fade in from black.

---

## 6. `lprender` — the renderer

### 6.1 DRM atomic vs. legacy

**Open question 4 answered: atomic modesetting.**

The framing of "atomic vs. legacy" is slightly misleading on this stack — `vc4` is an atomic driver, and the legacy ioctls are emulated on top of it by `drm_atomic_helper`. Choosing legacy does not get us a simpler path to the hardware; it gets us a compatibility shim over the same path, with less control. Concretely, atomic gives us:

- **`TEST_ONLY` commits.** We can validate a modeset before applying it. On hotplug into an unknown panel this is the difference between a clean fallback and a black screen.
- **A single commit** for CRTC + plane + framebuffer, so mode changes are glitch-free.
- **`CRTC.ACTIVE = 0`** for true panel-off. This is a cleaner and more reliable way to reach <2W than legacy connector-level DPMS on the vc4 path.
- **Non-blocking commits with page-flip events** on the DRM fd, which is what our epoll loop is built around.

**Gamma LUT: we do not use it.** Fading to black via the CRTC gamma ramp is the traditional trick, but gamma support across `vc4` on BCM2711 and BCM2712 is inconsistent, and a fade that silently no-ops on one board and works on the other is a bad dependency for the single most visible transition in the product. Instead, **fades are a `uniform float uGlobalFade` multiply in the fragment shader.** This is exact, identical on the Pi and on the SDL2 dev backend, and unit-testable with golden images. Only once the fade reaches zero do we issue the atomic `ACTIVE=0` commit.

One known Pi 5 issue avoided by construction: `DRM_MODE_PAGE_FLIP_ASYNC` is broken on the Pi 5's v3d/vc4 stack (raspberrypi/linux#5828). We only ever issue vsync-synchronised non-blocking flips, so this never bites us.

### 6.2 Initialisation

1. Enumerate `/dev/dri/card*`, select by `drmGetVersion().name == "vc4"` (dev backend: `--card` override). Never hardcode `card0` — the numbering moves when the v3d render node enumerates first.
2. `DRM_CLIENT_CAP_ATOMIC` + `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
3. First connected connector (or `display.connector` from config); its preferred mode, or `display.mode` if overridden — needed for a non-standard 1920×1920 panel whose EDID may be wrong or absent.
4. GBM device + surface, `GBM_FORMAT_XRGB8888`, `SCANOUT | RENDERING`.
5. EGL via `EGL_PLATFORM_GBM_KHR`, GLES 3.0 context, `eglSwapInterval(1)`.
6. Initial atomic commit: `ALLOW_MODESET`, CRTC active, primary plane full-screen, first framebuffer black.

**DRM master and VT-less boot.** `lprender` runs as a systemd service after `multi-user.target` with `getty@tty1` masked. `fbcon` is not a DRM master, so becoming master is uncontended in practice; we still retry `drmSetMaster` with backoff on `EACCES` (covers a stray `plymouth` or a leftover session). Kernel cmdline gets `logo.nologo consoleblank=0 vt.global_cursor_default=0`, and `config.txt` gets `disable_splash=1`, so nothing else ever draws to the panel. Because there is no VT switching, we do not need to handle `DROP_MASTER`/`SET_MASTER` cycles — but the handler is implemented anyway and is what makes the renderer restartable without a reboot.

**Hotplug.** A `libudev` monitor on the `drm` subsystem, its fd in the epoll set. On a `change` event: re-probe the connector. Disconnected → tear down GBM/EGL, release master, and idle in a reconnect loop. Reconnected with a different mode → full re-init. This is the code path that turns a panel that was merely unplugged into a device that recovers, instead of one that needs a power cycle.

### 6.3 Render pipeline

**Square, resolution-agnostic.** Let `s = min(mode.hdisplay, mode.vdisplay)`. The art area is an `s × s` square centred in the framebuffer. A 1:1 panel is full-bleed; anything else has a remainder, and §6.3.1 covers what goes in it.

Rotation (`display.rotation = 0|90|180|270`) is applied in the vertex shader rather than via the plane `rotation` property — the property is not guaranteed present on every plane, and the shader path is identical on the dev backend. Odd rotations swap the width/height inputs to the square computation.

**Aspect handling within the square.** Cover art is nominally square but not always. `render.fit = "cover" | "contain"`, default `cover` — crop to fill, because a full-bleed LP sleeve is the entire point.

#### 6.3.1 Panel compatibility

True 1:1 panels are scarce and expensive, so the renderer must look deliberate on whatever hardware is actually available — a 16:9 monitor turned portrait, a spare 4:3 panel, a small DSI screen, or a TV. Nothing here is specific to square displays; the square art area is a *policy*, and the policy is configurable.

**What fills the remainder** (`render.background`, only visible on non-1:1 panels):

| Value | Result |
|---|---|
| `black` | Hard letterbox/pillarbox. Purest, and correct for a framed LP sleeve. |
| `blur` *(default)* | The artwork scaled to fill the whole panel, heavily blurred and dimmed to `background_dim` (default 0.35), with the sharp square floating over it. The same treatment Apple TV and Plex use for non-matching aspect ratios, and the reason it is the default: it makes a 16:9 panel look intentional rather than broken. Reuses the dual-Kawase chain already written for ambient mode, so it costs no new code. |
| `dominant` | Flat fill in the artwork's dominant colour (OKLab k-means, k=3). Cheapest, and good on e-ink-ish or low-bandwidth panels. |
| `gradient` | Vertical gradient between the two leading dominant colours. |

The background is rendered once per artwork change and cached in an FBO, not recomputed per frame, so it does not affect the "static image = zero page flips" property.

**Escaping the square entirely.** `render.square = false` drops the square constraint and applies `fit` against the full panel — `cover` fills a 16:9 screen edge to edge by cropping the top and bottom off the sleeve, `contain` shows the whole sleeve with a background. For the LP-frame use case `square = true` is right; for someone reusing this on a widescreen monitor it is not.

**Modes and EDID.**

- `display.mode = "auto"` takes the connector's preferred mode. `"highest"` takes the highest resolution at the highest refresh. An explicit `"1920x1920@60"` overrides both, and is validated with a `TEST_ONLY` atomic commit before being applied — an unsupported mode is reported rather than producing a black screen.
- Panels with absent or wrong EDID — common on cheap HDMI→eDP driver boards, and a live risk for the 1920×1920 target — are handled by the explicit mode plus documented `hdmi_timings` in `config.txt`. The build guide walks through deriving those from the panel datasheet with `cvt`/`gtf` and verifying with `kmsprint`.
- Multiple connected connectors: `display.connector = "auto"` picks the first connected; name it explicitly (`"HDMI-A-1"`, `"DSI-1"`) when that is ambiguous. We never mirror or span.
- **Non-HDMI panels work unchanged.** DSI and DPI panels (including the Waveshare round and square DSI range) present as ordinary KMS connectors, so the entire render path is identical. This is a benefit of going straight to KMS rather than through a compositor.
- `display.margin_percent` insets everything by a percentage, for TVs that overscan.

**Scaling quality.** Mipmaps plus anisotropic filtering where the extension is present. Upscaling matters more than downscaling here: a 1080p or 4K TV receiving 500×500 AirPlay art looks genuinely bad, which is a second, independent argument for the enrichment pipeline. When the source is smaller than the art area by more than 2×, we apply a mild Catmull-Rom upscale in the fragment shader instead of plain bilinear.

**Compatibility matrix** (all exercised via the SDL2 backend at arbitrary window sizes in CI):

| Panel | Result |
|---|---|
| 1920×1920, 720×720 (1:1) | Full-bleed, no background visible |
| 1920×1080, 3840×2160 (16:9) | Square art centred, blurred background either side |
| 1080×1920 (portrait 9:16) | Square art centred, background above and below |
| 1024×768, 800×600 (4:3) | Square art centred, narrow background margins |
| 3440×1440 (21:9) | Square art centred, wide background margins |
| 480×480, 720×720 DSI round/square | Full-bleed |
| Rotated 90/270 | Dimensions swapped before the square computation |

**Texture path.** Decode happens on a worker thread; the render thread never touches libjpeg.

1. `artd` publishes a new `artwork.revision`.
2. Worker decodes, and downscales on the CPU to `min(3000, GL_MAX_TEXTURE_SIZE, 2 × s)` — no value in a 3000² texture on a 720² panel, and it costs memory we care about.
3. Upload via a **pixel buffer object** (GLES3), then `glGenerateMipmap`. Mipmaps matter: minifying 3000² to 1920² without them aliases visibly on fine label text.
4. `glFenceSync` after upload; the render thread polls with `glClientWaitSync(0)` once per frame.
5. **The crossfade does not begin until the fence signals.** The new image is therefore always fully resident before the first blended frame — which is how we guarantee no dropped frames rather than hoping.

Memory: two 1920² RGBA textures with mipmaps ≈ 40 MB. Comfortable on a Pi 5. On a Pi 4 this comes out of CMA, so the build guide sets `cma=256M` minimum.

**Crossfade.**

```glsl
uniform sampler2D uPrev, uNext;
uniform float uMix;         // 0→1, cubic ease-in-out
uniform float uGlobalFade;  // 1 = visible, 0 = black
void main() {
    vec3 c = mix(texture(uPrev, vUv).rgb, texture(uNext, vUv).rgb, uMix);
    fragColor = vec4(c * uGlobalFade, 1.0);
}
```

600ms default, cubic ease-in-out, 60fps, driven by the monotonic clock rather than a frame counter so a dropped frame shortens the fade instead of stretching it.

**Frame pacing — render on demand.** This is the single most important power decision in the renderer. A static image means **zero page flips**: the scanout hardware keeps displaying the last framebuffer indefinitely, at no CPU or GPU cost. The event loop is an `epoll` over `{drm_fd, artd_socket, udev_monitor, timerfd}` and blocks indefinitely when nothing is animating. We drive at vsync only during a crossfade, a fade to/from black, Ken Burns, or ambient mode. A naive 60fps loop would cost several watts continuously for a device that shows the same picture for four minutes at a time.

**Ken Burns** (`render.ken_burns = false` by default): scale 1.00 → 1.04 over `render.ken_burns_period` (default 180s) with slight drift, cubic-eased at the turnarounds. Default off because it forces continuous rendering and therefore continuous power draw — the user should opt into that trade knowingly.

**Ambient mode** (`render.ambient = false` by default): the last artwork, heavily blurred and dimmed to ~12%. Implemented as a dual-Kawase downsample/upsample chain (roughly 4 half-resolution passes) rather than a true separable Gaussian, which would be far more expensive at 1920² on a VideoCore. Refreshed at 1–5 fps since nothing moves.

### 6.4 Development and test backends

Selected by Cargo feature, chosen at runtime by `--backend`:

- `backend-drm` (default) — the real thing.
- `backend-sdl2` — an SDL2 window with a GLES3 context. Same GL code, same shaders, same state machine, same socket client. Fake DPMS is a window title change plus a black clear. This is how the renderer is developed without a Pi on the desk.
- `backend-headless` — EGL surfaceless/pbuffer, renders to an FBO, dumps PNGs. Used by CI: run a crossfade, dump frames at t = 0, 0.25, 0.5, 0.75, 1.0, compare against golden images with a small perceptual tolerance. Shader regressions get caught without hardware.

Standalone mode for all three: `lprender --slideshow ./pictures --interval 5s` runs the full render path with no `artd` at all, which is milestone 3's deliverable and stays useful forever as a smoke test.

---

## 7. Configuration and the web interface

### 7.1 Layered config files

Two files, merged, with the second winning per-key:

| File | Owner | Writable |
|---|---|---|
| `/etc/lpframe/config.toml` | The package. Ships with defaults and installer-set values. | No (read-only root under overlayfs) |
| `/var/lib/lpframe/config.local.toml` | The web interface. Only keys the user has actually changed. | Yes |

This split exists because of overlayfs. With a read-only root, anything written to `/etc` evaporates at reboot — so a web UI that edited `/etc/lpframe/config.toml` would appear to work and then silently forget everything on power-cycle. Keeping user changes on the writable partition, as a sparse override file, also makes "reset this setting to default" a deletion rather than a guess about what the default was, and keeps `/etc` diffable against the package.

Both daemons load the merged view. Validated on load; a malformed config fails the service loudly rather than silently reverting to defaults.

### 7.2 Config file

```toml
[device]
name = "LP Frame"              # AirPlay advertised name
timezone = "Europe/London"

[audio]
alsa_device = "hw:CARD=KA11,DEV=0"
mixer = "PCM"                  # "" to disable hardware volume

[display]
connector = "auto"             # or "HDMI-A-1", "DSI-1"
mode = "auto"                  # auto | highest | "1920x1920@60"
rotation = 0                   # 0 | 90 | 180 | 270
card = "auto"
margin_percent = 0             # inset, for TVs that overscan

[render]
square = true                  # false = fill the panel, ignore the 1:1 constraint
fit = "cover"                  # cover | contain
background = "blur"            # black | blur | dominant | gradient  (non-1:1 panels only)
background_dim = 0.35
crossfade_ms = 600
enrichment_crossfade_ms = 250
ken_burns = false
ken_burns_period = "180s"
ambient = false
ambient_dim = 0.12
placeholder = "/usr/share/lpframe/placeholder.png"

[enrichment]
enabled = true
strictness = "text_and_visual" # off | text_only | text_and_visual | strict
# MusicBrainz requires a contactable User-Agent, so it cannot be on by
# default; adding it without `contact` is rejected at load.
sources = ["itunes"]
itunes_country = "GB"
max_dimension = 3000
contact = ""                   # required by MusicBrainz; unused if only itunes
rate_limit_per_min = 15

[cache]
dir = "/var/lib/lpframe/cache"
max_bytes = "2GiB"
negative_ttl = "7d"

[power.amp]
enabled = true
gpio_chip = "auto"
gpio_line = 17
active_low = false
pulse_ms = 0
on_event = "session_begin"     # session_begin | play_begin
off_delay = "10m"
debounce = "2s"

[power.display]
blank_after = "5m"
ambient_after = "off"          # or e.g. "30s"
fade_out_ms = 1000

[timeouts]
stall = "15s"
session = "60s"

[ipc]
socket = "/run/lpframe/artd.sock"

[web]
enabled = true
bind = "0.0.0.0:8730"
auth = true                    # password generated on first start
# password_hash is written to config.local.toml on first start. Argon2id
# only; the plaintext goes to /var/lib/lpframe/web-password.txt (0600).
mdns = true                    # adds an _http._tcp record via Avahi
insecure_no_auth = false       # permits a LAN bind with auth off

[logging]
level = "info"
```

### 7.3 The web interface

Served by `artd` on `http://lpframe.local:8730` (mDNS-advertised via the Avahi stack AirPlay already needs, so there is no IP address to hunt for).

This was originally scoped as a loopback debug page. It is now a real component: with no buttons, no screen controls, and no keyboard, it is the only way to configure a running device short of SSH.

**Pages**

1. **Now Playing** — live artwork, artist/album/title, playback state, artwork source badge (`airplay` / `itunes` / `cache`), and whether enrichment upgraded, is pending, or was rejected.
2. **Settings** — the config, grouped and typed. Timings, ambient mode, enrichment, display fit/background/rotation, amp behaviour. Overridden values are marked, each with a one-click reset to default.
3. **Diagnostics** — recent event log, every enrichment decision with its match scores and the reason for rejection, cache size and hit rate, GPIO state, the negotiated DRM mode and connector, service health.

**Transport.** The state snapshot that already goes to `lprender` over the Unix socket is re-broadcast to browsers as Server-Sent Events on `/api/events`. Same payload, same `seq`, one broadcaster. SSE rather than WebSocket because reconnection is built into `EventSource` and we only need one direction.

```
GET   /api/session               whether a login is needed, and whether we have one
POST  /api/login                 {"password": "..."} → session cookie
POST  /api/logout                drops the session server-side
GET   /api/state                 current snapshot
GET   /api/events                SSE stream of snapshots
GET   /api/config                merged config + which keys are overridden + defaults
PATCH /api/config                sparse update; validated, then written to config.local.toml
POST  /api/config/reset          {"keys": ["render.ambient"]} → drop the override
POST  /api/config/confirm        {"id": 3} → keep a display change the countdown would undo
GET   /api/artwork/current       the current image, from the published path only
POST  /api/actions/<action>      amp_on | amp_off | display_wake | display_sleep
                                 | restart_renderer (= reprobe_display) | restart_daemon
```

Everything except `/`, `/api/session` and `/api/login` requires a session. `cache_clear` from the original list is **not implemented**: emptying the cache means emptying a SQLite index and the files it points at while the enrichment pipeline holds both open, and doing that from the web layer would leave the two disagreeing. It answers 501 saying so. The sweeper already enforces `cache.max_bytes`.

**Applying changes.** Writes go to `config.local.toml` atomically (temp file, `fsync`, `rename`, `fsync` the directory) and only after parsing and validating the merged result. The running daemon is told only about a change that reached the disk. Settings fall into three tiers, labelled as such in the UI:

- **Live** — `timeouts.*`, `power.display.*`, and the `power.amp.*` policy (delays, debounce, trigger event). `artd` re-reads these on every timer evaluation, and the idle timers are re-armed from the moment of the change.
- **Needs `lprender` restart** — everything under `display.*` and `render.*`.
- **Needs `artd` restart** — `ipc.*`, `web.*`, `cache.*`, `enrichment.*`, `device.*`, `audio.*`, `logging.*`, and the three amp keys that describe the GPIO line itself.

This is narrower than originally planned, and deliberately labelled honestly. The plan had the render settings — ambient, fit, background, crossfade — as live; they are not, because `lprender` loads the configuration once at startup and the snapshot protocol carries no render policy. Enrichment is the same story for a different reason: the pipeline owns an open cache and a sweeper task, and rebuilding it under a running daemon would leave the old one sweeping the same database. Both are one click in the UI, which offers a restart for the two non-live tiers, and says what to run instead when systemd is not managing the device.

**Confirm-or-revert on display changes.** Applying `display.rotation` or `display.mode` starts a 15-second countdown; if you do not click *Keep this*, it reverts. Borrowed from desktop display settings, for the same reason: a wrong mode on a headless appliance with no input device is otherwise a reflash. The countdown lives in the daemon, not the browser, so closing the tab or losing the display does not strand the change. The revert restores the previous *override*, not the default — so undoing a bad rotation returns you to the last one you confirmed. It is a configuration write, so it works with or without systemd; only the renderer restart that makes it visible needs one.

**Auth and threat model.** Default is LAN-bound with a password, because the alternative — an unauthenticated page on the LAN — exposes listening history and lets any device on the network toggle outbound API calls.

- On first start with `web.auth = true` and no hash, `artd` generates a 100-bit passphrase from the OS entropy source, writes it to `/var/lib/lpframe/web-password.txt` (0600), logs it once at `warn`, and stores only an **Argon2id** hash (m=19456 KiB, t=2, p=1 — the OWASP minimum) in `config.local.toml`. `lpctl web-password` reprints it. The hash is never returned by any endpoint and cannot be set through one.
- Session cookie `lpframe_session`: 32 bytes of OS entropy, held server-side and compared in constant time, `HttpOnly` + `SameSite=Strict` + `Path=/`, 30-day expiry. **No `Secure` flag** — the device serves plain HTTP, and the browser would drop a `Secure` cookie on every request. Sessions do not survive a restart.
- CSRF: `SameSite=Strict` plus a required `X-LPFrame-Request` header on every mutating request. A cross-origin form post cannot set a custom header, and anything that could becomes a preflight we never approve.
- Login is rate-limited to 5 attempts per minute per client address (the TCP peer; `X-Forwarded-For` is deliberately not trusted). A wrong password and an unknown session return the identical 401.
- `web.auth = false` with a non-loopback bind **refuses to start**, unless `web.insecure_no_auth = true` says so deliberately — for a device behind something else that authenticates. That combination logs a shouting warning at every startup.
- **Plain HTTP, stated plainly:** this protects against other people and devices casually reaching the page on your network. It does *not* protect against someone who can passively sniff your LAN — the password crosses in the clear. If that is in your threat model, set `web.bind = "127.0.0.1:8730"` and use an SSH tunnel, or front it with a TLS-terminating reverse proxy. We do not ship self-signed TLS; it trains people to click through certificate warnings and buys nothing here.
- **Do not port-forward this.** The build guide says so in a box. There is no WAN mode, no remote access feature, and no cloud component.

**mDNS.** `web.mdns` spawns `avahi-publish` to add an `_http._tcp` service record, and that is all it does. Avahi is on the device for AirPlay already and publishes the hostname, so `http://lpframe.local:8730` resolves either way; the record only adds discovery. Absent or failing `avahi-publish` is warned about once and otherwise ignored.

**Implementation.** `axum` inside `artd`, one embedded HTML file with inline CSS and vanilla JS — no npm, no build step, and critically no CDN references, since the device is frequently offline and a settings page that needs internet access to render would be useless exactly when you need it. Roughly 25 KB, dark theme. Everything the daemon sends is written with `textContent`, never as markup: track titles come off the AirPlay pipe and the sender chooses them.

---

## 8. System integration

### 8.1 Units and ordering

| Unit | Ordering | Notes |
|---|---|---|
| `nqptp.service` | `After=network.target` | Root; binds UDP 319/320 |
| `lpframe-artd.service` | `After=network-online.target`, **`Before=shairport-sync.service`** | Creates the FIFO and attaches before any metadata can be written |
| `shairport-sync.service` | `After=nqptp.service lpframe-artd.service`, `Wants=` both | |
| `lpframe-lprender.service` | `After=lpframe-artd.service multi-user.target` | |

All `Restart=always`, `RestartSec=2`, with `StartLimitIntervalSec=0` so a genuinely broken dependency doesn't permanently wedge the appliance.

The `Before=shairport-sync` ordering is the subtle one and is worth calling out: shairport-sync discards metadata written while no reader is attached to the FIFO. Getting this backwards loses the first metadata bundle of every boot, which presents as "the first track after power-on has no art" — an intermittent bug that is very annoying to diagnose after the fact.

`artd` and `lprender` do **not** `Requires=` each other. Either can restart independently; the renderer reconnects to the socket with backoff and receives a full state snapshot on reconnect, which is precisely why the protocol is snapshot-based.

### 8.2 Users and hardening

A `lpframe` system user in groups `video`, `render`, `gpio`, `audio`.

```ini
# lpframe-artd.service
User=lpframe
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=no                  # needs the real /tmp for the metadata FIFO
ReadWritePaths=/var/lib/lpframe /run/lpframe
DeviceAllow=/dev/gpiochip0 rw
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
MemoryMax=512M
```

```ini
# lpframe-lprender.service
User=lpframe
SupplementaryGroups=video render
DeviceAllow=char-drm rw
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
RestrictAddressFamilies=AF_UNIX
```

### 8.3 SD-card longevity

- **Overlayfs root** (`raspi-config` → Performance → Overlay File System) for production. `/var/lib/lpframe` moves to a small dedicated writable partition so the artwork cache **and the web interface's `config.local.toml`** survive reboots. This partition is not optional once the web UI exists — without it, every setting change is forgotten at power-cycle.
- `journald` `Storage=volatile`, `RuntimeMaxUse=32M`.
- `logrotate` for anything on disk; `noatime` mount options.
- Helper scripts `lpframe-rw` / `lpframe-ro` to toggle the overlay for maintenance.

### 8.4 Web interface

See §7.3. `artd` binds the listener itself, so there is no extra unit. Because `config.local.toml` lives on the writable partition, the interface keeps working under a read-only root — this is the main reason for the config split.

---

## 9. Testing

| Layer | Approach |
|---|---|
| `spmeta` framing | Table-driven units: truncation, split base64, garbage prefix, oversized `length`, mid-stream start, random chunk boundaries |
| `spmeta` decode | Fixture replay → golden `*.events.json` |
| `artd` state machine | Fixture replay with a fake clock; assert transition sequence and emitted NDJSON against golden files |
| `artd` power | Fake GPIO backend recording `(timestamp, level)`; assert debounce and delays with the fake clock |
| Enrichment | A local mock catalogue for iTunes/MB/CAA — a hand-rolled `tokio` listener rather than `wiremock`, because it also has to synthesise images at arbitrary sizes, count requests and refuse connections outright. Gate tests build correct-match and known-wrong-match image pairs in-process. **No test reaches the real internet**, and no cassettes are recorded from it |
| Cache | LRU eviction, crash-safety (kill mid-write, assert index consistency) |
| `lprender` | Headless EGL golden images at fixed crossfade points; state-machine tests against a mock `artd`. Golden images rendered at every aspect ratio in the §6.3.1 matrix, so a layout regression on 16:9 is caught without a 16:9 panel |
| Web interface | API contract tests; config-layering round-trips (override → merge → reset); auth (rate limit, cookie flags, constant-time compare); validation rejects bad config without corrupting `config.local.toml`; confirm-or-revert times out correctly |
| Integration | `docker-compose`: real shairport-sync fed a synthetic AirPlay stream, real `artd`, headless `lprender` |

CI (GitHub Actions): x86_64 build + full test suite + clippy + rustfmt; aarch64 cross-build producing a `.deb` artifact. Golden-image tests run under `llvmpipe`.

### 9.1 `lpcapture`

The fixture tool. It sits between shairport-sync and `artd` so real sessions can be recorded from a phone and replayed forever:

```bash
# Record a live session while artd keeps working
lpcapture tee --in /tmp/shairport-sync-metadata \
              --out fixtures/sessions/apple-music-album.pipe \
              --fanout /run/lpframe/metadata-fanout

# Replay into artd
lpcapture replay fixtures/sessions/apple-music-album.pipe \
          --to /run/lpframe/metadata-fanout --speed 4x

# Regenerate golden event logs after an intentional parser change
lpcapture golden fixtures/sessions/*.pipe
```

Recordings are raw bytes — no interpretation, so the fixtures stay valid even if our parser is wrong. Timing is recorded in a sidecar so replay can reproduce real inter-item gaps (which is how we test the stall watchdog).

**Cover art makes captures enormous.** `PICT` payloads are extracted into `fixtures/art/<sha256>.jpg` and replaced in the `.pipe` file with a reference; `lpcapture replay` reassembles them. `--strip-art` substitutes a tiny synthetic JPEG for fixtures we want in git without LFS. Without this, a five-track capture is ~50 MB of base64 in the repo.

---

## 10. Implementation plan

Following the requested order. Each milestone ends with something demonstrable.

| # | Milestone | Deliverable |
|---|---|---|
| 1 | `spmeta` parser | **Done.** Library + fixture tests, `lpcapture`, synthetic fixture set, CI. |
| 2 | `artd` core | **Done.** State machine, IPC server, AirPlay-art-only path, read-only web pages, `lpctl`. Power state is computed and published as *intent*; the GPIO backend lands in milestone 6. |
| 3 | `lprender` static/slideshow | **Done.** Shared GLES3 renderer, layout, scene, decode, slideshow; SDL2, headless and KMS/DRM backends; 16 golden images across the panel matrix. The DRM present path (GBM, atomic commit, page flip, `ACTIVE=0` blanking) is implemented but **unverified against hardware** — no DRM node in CI. |
| 4 | Integration | **Done.** Renderer driven by `artd` over the socket, reconnecting with backoff; systemd units with the `Before=shairport-sync` ordering; an end-to-end test running a real daemon, a replayed session and the headless renderer. Play from a phone → art appears and crossfades. **This is the first end-to-end device**, subject to milestone 3's outstanding DRM present path. |
| 5 | Enrichment | **Done.** iTunes + MusicBrainz/CAA, text and perceptual gates, SQLite-indexed cache with LRU eviction and negative entries, token-bucket rate limiting, single-flight, a two-task worker pool, and enrichment counters on the diagnostics page. Every test runs against a local mock catalogue. The gate thresholds are the design's numbers and have **not** been calibrated against a corpus of real cover art. |
| 6 | Power management | **Done.** libgpiod v2 amp trigger via `gpiocdev`, chip selection by label, hold and momentary-pulse modes, minimum interval between transitions, amp driven off on SIGTERM. Display blanking, ambient and CRTC-off were already in place from milestones 2–3. **Line driving is unverified against hardware** — no `/dev/gpiochip*` and no `gpio-sim` in CI. |
| 7 | Web interface | Config layering, settings/diagnostics UI, auth, confirm-or-revert. |
| 8 | Provisioning | `install.sh`, `.deb`, systemd units, overlayfs, writable partition, and the full `docs/BUILD.md` from clean flash to working device. |
| 9 | *Bonus* | `pi-gen` stage producing a flashable image, built in CI. |

Milestones 2 and 3 are independent and can be built in either order or in parallel.

The **read-only Now Playing and Diagnostics pages land early, at milestone 2** — once the snapshot broadcaster exists, exposing it over SSE is nearly free, and having a live view of the state machine makes milestones 4–6 substantially easier to debug. Only the settings-editing half waits for milestone 7, since it needs the config layering and auth to be right.

---

## 11. Risks

| Risk | Mitigation |
|---|---|
| 1920×1920 is not a standard mode; the eDP board's EDID may be wrong or absent | `display.mode` override plus documented `config.txt` custom timings (`hdmi_timings`). Verify with `kmsprint` early — **before** committing to the panel. |
| Pi 4 CMA exhaustion with large textures | CPU downscale cap, `cma=256M`, documented. Pi 5 is unaffected. |
| Enrichment picks the wrong album | Two independent gates, `strict` mode, and a status page showing every rejection with its scores. Worst case: `enrichment.enabled = false` and the device still works. |
| iTunes Search API changes or rate-limits harder | It is a fallback-tolerant path by construction. MusicBrainz + CAA is a second source; both failing degrades to AirPlay art. |
| USB DAC clock/quirk issues (dropouts, wrong rates) | Build guide covers `snd-usb-audio` quirks, `nrpacks`, and a `speaker-test` verification step before anything else is configured. |
| AirPlay 2 needs specific ports | 7000/tcp, 319+320/udp, 5353 mDNS. Documented; installer configures the firewall if one is present. |
| shairport-sync AP2 build drift | Pin to a tagged release, record the exact `./configure` line, verify `-V` reports `AirPlay2`. |
| SD-card wear | Overlayfs root, volatile journald, cache on a separate partition. |
| Web UI is an unauthenticated-by-default footgun | Auth on by default with an installer-generated password; explicit threat model and a "do not port-forward" warning in the build guide (§7.3). |
| A bad display setting bricks a headless device | `TEST_ONLY` validation before applying, plus 15-second confirm-or-revert on rotation and mode changes. |

---

## 12. Decisions requiring sign-off

Summarising the four open questions, plus one I am adding:

1. **Pipe vs. hooks** → **Pipe only.** One event source, finer-grained states, no risk of hooks stalling audio. Abrupt disconnect handled by a stall/session watchdog plus treating FIFO EOF as "shairport-sync is gone" (§5.1).
2. **Alpine vs. Raspberry Pi OS Lite** → **Raspberry Pi OS Lite 64-bit.** The image-size saving does not pay for losing vendor-tested Mesa/KMS on a still-moving Pi 5 display stack, glibc-tested AirPlay 2 dependencies, and first-class libgpiod/ALSA/firmware tooling (§2.2).
3. **Confidence-gated vs. histogram-gated swaps** → **Both, in sequence.** Text match (Jaro-Winkler on normalised artist/album) selects the candidate; perceptual match (dHash + OKLab dominant-colour histogram) authorises the swap, with a near-exact-text bypass so legitimate alternate covers still upgrade. Strictness is configurable (§5.4).
4. **Atomic vs. legacy DRM** → **Atomic.** Legacy is a shim over the same atomic helpers on `vc4`, so it buys nothing; atomic gives `TEST_ONLY` validation, glitch-free single-commit modesets, and clean `ACTIVE=0` panel-off. **Fades are done in the fragment shader, not via gamma LUT**, because vc4 gamma support is inconsistent across BCM2711/2712 (§6.1).
5. **Added: snapshot IPC, not deltas.** Every state change sends the full state object. Late joiners and reconnects are correct by construction. Messages are small and infrequent enough that there is no efficiency case for deltas (§5.3).

### Settled in review

- **Amp-on trigger** → `abeg`. Confirmed after establishing that `pbeg`/`pend` fire per *track* in AirPlay 2 (§5.1.1); `pbeg` would cycle the relay between songs.
- **Idle timings** → 5 min to blank, 10 min to amp-off, and adjustable from the web interface.
- **Ambient mode** → off by default, toggleable in the web interface.
- **Enrichment** → on by default, toggleable in the web interface.
- **Web interface** → promoted from an optional loopback debug page to a first-class component (§7.3), which pulled in the layered config design (§7.1) and made the writable partition mandatory (§8.3).
- **Panel compatibility** → the square art area is a policy, not an assumption (§6.3.1). Any resolution, aspect, orientation, or connector type; blurred-fill background by default so non-1:1 panels look deliberate.

---

## 13. Non-goals for v1

No touch UI. No local library playback. No Spotify Connect — though `artd`'s internal boundary between "metadata source" and "state machine" is a trait, so a second source is an additive change rather than a refactor. No custom iOS app; public catalogue APIs replace Pentaton's proprietary full-resolution side channel. No remote or cloud access to the web interface — LAN only, by design.

---

## Appendix A — References

- shairport-sync metadata format and codes — [shairport-sync-metadata-reader](https://github.com/mikebrady/shairport-sync-metadata-reader)
- Active/Inactive vs Play events, `active_state_timeout` — [shairport-sync Events.md](https://github.com/mikebrady/shairport-sync/blob/master/ADVANCED%20TOPICS/Events.md)
- [iTunes Search API](https://developer.apple.com/library/archive/documentation/AudioVideo/Conceptual/iTuneSearchAPI/index.html) — 3000×3000 artwork ceiling, ~20 req/min fair use
- [drm/vc4 kernel documentation](https://docs.kernel.org/gpu/vc4.html)
- [BCM2712 / Pi 5 display support in vc4](https://patchew.org/linux/20241025-drm-vc4-2712-support-v2-0-35efa83c8fc0@raspberrypi.com/)
- [raspberrypi/linux#5828](https://github.com/raspberrypi/linux/issues/5828) — async page flip broken on Pi 5
- [Cover Art Archive API](https://musicbrainz.org/doc/Cover_Art_Archive/API)
