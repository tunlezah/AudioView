# LP Frame

A vinyl-LP-sized wall display that shows the cover art of whatever is currently AirPlaying to it, and nothing else.

Runs on a Raspberry Pi, drives a USB DAC, and wakes an amplifier over a 12V trigger. Album art is rendered full-bleed on KMS/DRM with GPU crossfades, and upgraded from ~500×500 AirPlay art to up to 3000×3000 by looking the album up against public catalogues.

Square panels are the intent, but any resolution, aspect ratio, orientation, or connector works — non-square displays get the artwork centred over a blurred fill. Configured from a local web page at `http://lpframe.local:8730`.

## Components

| | |
|---|---|
| `shairport-sync` + `nqptp` | AirPlay 2 receiver → ALSA → USB DAC (configured, not written here) |
| `spmeta` | Parser for the shairport-sync metadata pipe |
| `artd` | Metadata state machine, artwork enrichment, amp/display power, web interface |
| `lprender` | Fullscreen KMS/DRM + GLES3 renderer |
| `lpcapture` | Records real AirPlay sessions to replayable fixtures |

## Status

Design signed off. See **[docs/DESIGN.md](docs/DESIGN.md)**.

| # | Milestone | |
|---|---|---|
| 1 | `spmeta` parser + `lpcapture` | done |
| 2 | `artd` core | done |
| 3 | `lprender` static/slideshow | next |
| 4 | Integration | |
| 5 | Enrichment | |
| 6 | Power management | |
| 7 | Web interface | |
| 8 | Provisioning + `docs/BUILD.md` | |

## Developing

Everything builds and tests on an ordinary Linux box — no Pi required.

```bash
make check      # fmt, clippy, tests, fixture verification (what CI runs)
make fixtures   # regenerate synthetic fixtures and golden logs
```

See [fixtures/README.md](fixtures/README.md) for recording real AirPlay
sessions and replaying them.

### Running artd without a Pi

```bash
cargo build --workspace
mkdir -p /tmp/lp/art
cat > /tmp/lp/config.toml <<EOF
[device]
metadata_pipe = "/tmp/lp/metadata"
[ipc]
socket = "/tmp/lp/artd.sock"
art_dir = "/tmp/lp/art"
[web]
bind = "127.0.0.1:8730"
EOF

./target/debug/artd --config /tmp/lp/config.toml --config-local /tmp/lp/local.toml &
./target/debug/lpctl --socket /tmp/lp/artd.sock watch &

# Feed it a recorded session
./target/debug/lpcapture replay fixtures/sessions/album.pipe --to /tmp/lp/metadata
```

Then open <http://127.0.0.1:8730/> for Now Playing and diagnostics. On a real
device, point `metadata_pipe` at shairport-sync's pipe instead of replaying.
