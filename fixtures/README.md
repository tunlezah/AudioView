# Fixtures

Recorded and synthetic shairport-sync metadata streams, used to test the
parser and (from milestone 2) `artd`'s state machine without hardware.

```
sessions/NAME.pipe          byte stream, artwork externalised
sessions/NAME.events.json   golden decoded event log
sessions/NAME.timing        optional: recorded inter-read delays
art/<sha256>.bin            artwork payloads, content-addressed
```

## Why artwork is externalised

A real capture is mostly base64 cover art — a few tracks runs to tens of
megabytes. `lpcapture pack` replaces each `PICT` payload with a `sha256`
reference and writes the bytes to `art/`; `unpack` reverses it. The transform
is verified lossless on every write and again in CI, because a fixture that
quietly differed from what came down the pipe would weaken every test built
on it.

Everything else in the stream is preserved verbatim, **including a truncated
final item** — that is the interesting part of a capture that ends in a crash.

## Recording a real session

The synthetic fixtures are a baseline, not a substitute. A few code semantics
(`paus`/`pres` in particular, see DESIGN §4.3) stay unverified until we have
captures from a real phone.

Run this on the Pi with shairport-sync already working:

```bash
# 1. Point shairport-sync's metadata pipe at lpcapture and artd at the fanout,
#    so the device keeps working while you record.
lpcapture tee \
    --input  /tmp/shairport-sync-metadata \
    --output fixtures/sessions/apple-music-album.pipe \
    --fanout /run/lpframe/metadata-fanout

# 2. Play something from the phone. Cover the interesting cases:
#    a full album, a skip, a pause longer than 10s, walking out of Wi-Fi range.
# 3. Ctrl-C.

# 4. Shrink it and generate the golden log.
lpcapture pack   fixtures/sessions/apple-music-album.pipe
lpcapture golden fixtures/sessions/apple-music-album.pipe

# 5. Read the golden log before committing — it is the reviewable artefact.
lpcapture dump   fixtures/sessions/apple-music-album.pipe
```

Captures are flushed per read, so a Ctrl-C never truncates the file below
what actually arrived.

**Check before committing:** captures contain your device name, your local IP
addresses, and what you were listening to. `sessions/*.events.json` shows all
of it in one screen.

## Replaying

```bash
# Into a FIFO, at the recorded pace
lpcapture replay fixtures/sessions/album.pipe --to /run/lpframe/metadata-fanout --realtime

# Four times faster
lpcapture replay fixtures/sessions/album.pipe --to /tmp/pipe --realtime --speed 4

# In small chunks, to exercise a reader's handling of item boundaries
lpcapture replay fixtures/sessions/album.pipe --chunk 7 > /tmp/out
```

## Synthetic set

Regenerate with `make fixtures`. Committed bytes are checked against the
generator in CI, so these cannot be edited by hand.

| Fixture | Shape it exercises |
|---|---|
| `album` | Normal play. `pbeg`/`pend` cycle per track inside one `abeg`…`aend` envelope |
| `no-artwork` | Zero-length `PICT` — the track explicitly has no art |
| `pause-resume` | `pfls` then `prsm` mid-track |
| `track-skip` | Rapid skipping; the shape that makes a naive amp trigger chatter |
| `abrupt-disconnect` | Sender vanishes: no `pend`/`aend`, stream stops mid-item |
| `airplay1-no-active` | Sender that never emits `abeg`/`aend` |
| `client-handover` | A second sender takes over with no clean end from the first |
| `unknown-codes` | Unmodelled `ssnc` and `core` codes mixed into a normal session |

Artwork in the synthetic set is generated PNG, not real cover art: no
licensing questions, and each track gets a perceptually distinct image, which
the enrichment gates will need at milestone 5.
