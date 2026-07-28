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
| `lprender` | Fullscreen KMS/DRM + GLES3 renderer (SDL2 and headless dev backends) |
| `lpframe-image` | OKLab conversion, perceptual hashing and resampling, shared by `artd` and `lprender` |
| `lpcapture` | Records real AirPlay sessions to replayable fixtures |

## Status

Design signed off. See **[docs/DESIGN.md](docs/DESIGN.md)**.

| # | Milestone | |
|---|---|---|
| 1 | `spmeta` parser + `lpcapture` | done |
| 2 | `artd` core | done |
| 3 | `lprender` static/slideshow | done — DRM path unverified on hardware |
| 4 | Integration | done |
| 5 | Enrichment | done — gate thresholds unvalidated against real cover art |
| 6 | Power management | next |
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

### Running the renderer without a Pi

`lprender` needs EGL, GLES, GBM, DRM and SDL2 development packages:

```bash
sudo apt-get install -y libegl-dev libgles-dev libgbm-dev libdrm-dev \
                        libsdl2-dev libegl1-mesa-dev
```

Cycle a directory of images through the real render path — crossfades,
letterboxing, blurred background and all:

```bash
# In a window
cargo run -p lprender -- --config provisioning/config.toml \
    --config-local /nonexistent --backend sdl2 --size 720x720 \
    --slideshow ./pictures --interval 5

# Or to PNG files, with no display at all (this is what CI runs)
cargo run -p lprender -- --config provisioning/config.toml \
    --config-local /nonexistent --backend headless --size 400x225 \
    --slideshow ./pictures --interval 2 --frames 120 --dump /tmp/frames
```

On the device, `--probe` reports the card, connector and modes actually
available, and whether the configured mode exists — run it before anything
else on a new panel.

### Running both together

With no `--slideshow`, `lprender` follows `artd` over the socket at
`ipc.socket` and shows whatever is playing. Start them in either order — they
are ordered but not bound to each other, so the renderer waits on a black
screen until the daemon appears, and survives it restarting.

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

./target/debug/artd --config /tmp/lp/config.toml --config-local /nonexistent &
./target/debug/lprender --config /tmp/lp/config.toml --config-local /nonexistent \
    --backend sdl2 --size 720x720 &

# Replay a recorded session; the window crossfades through the album
./target/debug/lpcapture replay fixtures/sessions/album.pipe --to /tmp/lp/metadata
```

Kill and restart either process while the other runs: the protocol publishes
a full snapshot on every change, so the renderer is correct again on the
first message after it reconnects. `--backend headless --dump /tmp/frames`
does the same with no display at all.

On a device, `systemd/` has the units and `systemd/README.md` explains the
one ordering constraint that must not be tidied up.
