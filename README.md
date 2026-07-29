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
| 6 | Power management | done — GPIO line driving unverified on hardware |
| 7 | Web interface | done — see the note on live settings below |
| 8 | Provisioning + `docs/BUILD.md` | next |

## The web interface

`http://lpframe.local:8730` — Now Playing, Settings and Diagnostics, served by
`artd` itself. It is the only way to configure a running device short of SSH,
so it is on by default, bound to the LAN by default, and authenticated.

**Password.** On the first start with `web.auth = true`, `artd` generates a
passphrase, prints it once in the log, writes it to
`/var/lib/lpframe/web-password.txt` (mode 0600), and stores only an Argon2id
hash in `config.local.toml`. `lpctl web-password` reprints it. To force a new
one, delete `web.password_hash` from `/var/lib/lpframe/config.local.toml` and
restart `artd`.

**What the authentication is for, and what it is not.** It keeps other people
and other devices on your network out of your listening history and your
settings. It is **plain HTTP**: the password crosses the network in the clear
on every login and the session cookie on every request, so it does nothing
against someone who can capture traffic on your LAN. If that matters, set
`web.bind = "127.0.0.1:8730"` and use an SSH tunnel, or put a TLS-terminating
reverse proxy in front. **Do not port-forward it** — there is no WAN mode and
no cloud component. Login attempts are limited per address, and password
verification is capped at two at a time device-wide so that a login flood
costs the daemon 38 MiB rather than as much RAM as the Pi has. The full threat
model, including what is deliberately not defended against, is at the top of
`crates/artd/src/web/auth.rs`.

Turning `web.auth` off while bound to anything but loopback makes `artd`
refuse to start. If something in front of it is doing the authentication, say
so with `web.insecure_no_auth = true`; the daemon then warns at every startup.

**Settings tiers.** Each setting is labelled with what it needs before it takes
effect: *live* (timeouts, display blanking, amp delays — applied in-process),
*needs `lprender` restart* (everything under `display.` and `render.`), or
*needs `artd` restart* (`ipc.`, `web.`, `cache.`, `enrichment.`, and the GPIO
line). The page offers one-click restarts for the latter two, and says what to
run instead when systemd is not managing the device. Note that this is
narrower than `docs/DESIGN.md` originally planned: the render settings are not
live, because `lprender` reads the configuration once at startup.

Changing `display.rotation` or `display.mode` starts a 15-second countdown in
the daemon. Unless you click *Keep this*, the previous value comes back —
which is what stops a wrong mode on a headless device being a reflash.

`web.mdns` adds an `_http._tcp` service record via `avahi-publish` and nothing
more. The hostname resolves through Avahi either way.

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
auth = false          # loopback only, so nothing is exposed by this
EOF

./target/debug/artd --config /tmp/lp/config.toml --config-local /tmp/lp/local.toml &
./target/debug/lpctl --socket /tmp/lp/artd.sock watch &

# Feed it a recorded session
./target/debug/lpcapture replay fixtures/sessions/album.pipe --to /tmp/lp/metadata
```

Then open <http://127.0.0.1:8730/>. The `[web] bind` above is loopback, so no
password is needed to reach it; add `auth = true` to exercise the login, and
the generated password appears in artd's log and in `/tmp/lp/web-password.txt`.
On a real device, point `metadata_pipe` at shairport-sync's pipe instead of
replaying.

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
auth = false          # loopback only, so nothing is exposed by this
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
