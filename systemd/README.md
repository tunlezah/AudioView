# systemd units

Four units, installed to `/etc/systemd/system/`. They implement DESIGN §8.1
(ordering) and §8.2 (users and hardening).

| Unit | Ordering |
|---|---|
| `nqptp.service` | `After=network.target` |
| `lpframe-artd.service` | `After=network-online.target`, **`Before=shairport-sync.service`** |
| `shairport-sync.service` | `After=nqptp.service lpframe-artd.service` |
| `lpframe-lprender.service` | `After=lpframe-artd.service multi-user.target` |

## The ordering constraint that must not be tidied up

`lpframe-artd.service` declares `Before=shairport-sync.service`. This looks
backwards — the daemon that *reads* the metadata pipe starting before the one
that *writes* it — and it is the single easiest thing in this directory to
"fix" into a bug.

**shairport-sync discards metadata written while no reader is attached to the
FIFO.** It does not buffer it and it does not retry. If shairport-sync wins
the race, everything it emits before `artd` opens the pipe is gone: the
`abeg`, the first `mdst`…`mden` bundle, and the first `PICT`. `artd` creates
the FIFO itself for exactly this reason.

The symptom is not a crash. It is *the first track after power-on has no
art*, and only after power-on, and only sometimes — depending on how the boot
happened to interleave. That is an expensive bug to diagnose from the far
side, so it is prevented by ordering instead.

Do not replace this with a `Requires=`, an `ExecStartPre` sleep, or a
`Restart=` and hope. The ordering is the fix.

## What is deliberately *not* declared

`lpframe-artd` and `lpframe-lprender` do **not** `Requires=` each other, in
either direction. Each restarts independently:

- The renderer starts before the daemon exists on most boots, sits on a black
  screen, and connects when the socket appears.
- `artd` restarting does not blank the panel. The renderer keeps the current
  artwork, reconnects with exponential backoff capped at 5s, and is correct
  again on the first message it receives.

That last point is why the IPC protocol publishes a full snapshot on every
change rather than deltas (DESIGN §5.3). A `Requires=` would couple two
services that the protocol was designed to decouple, and would turn a
five-second renderer reconnect into a restart of the audio path.

## Hardening notes

Two settings are stated differently here from the sketch in DESIGN §8.2, on
purpose:

- **`DeviceAllow=char-drm rw`** rather than a specific `/dev/dri/cardN`. The
  card numbering moves when the v3d render node enumerates first (DESIGN
  §6.2), so naming a node would break on a kernel update.
- **`DeviceAllow=char-gpiochip rw`** rather than `/dev/gpiochip0`. Pi 5 moved
  the 40-pin header to the RP1 pin controller and the `gpiochipN` numbering
  has shifted between kernel releases; `artd` resolves the chip by label
  (DESIGN §5.5).

`PrivateTmp=no` on `lpframe-artd` and `shairport-sync` is required, not an
oversight: they share the metadata FIFO through the real `/tmp`.

## Not done yet

- **No `ExecStop` dropping the amp GPIO.** DESIGN §5.5 calls for `artd` to
  drive the trigger line low on shutdown. There is no GPIO backend until
  milestone 6, so there is nothing to drive. The external pull-down resistor
  the build guide requires is what actually covers the crash case anyway.
- `shairport-sync.service` and `nqptp.service` here are ours, and replace
  whatever the upstream builds install. They assume `/usr/local/bin`, which
  is where the from-source build in DESIGN §2.3 puts them. The installer that
  reconciles this with a distro package lands with milestone 8.
- Nothing installs these files yet; `provisioning/install.sh` is milestone 8.

## Trying them by hand

```bash
sudo install -m 0644 systemd/*.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now lpframe-artd lpframe-lprender
systemd-analyze verify /etc/systemd/system/lpframe-artd.service
```

To confirm the ordering actually resolved the way you expect:

```bash
systemctl list-dependencies --before lpframe-artd.service
systemd-analyze critical-chain lpframe-lprender.service
```

## The amplifier trigger needs a resistor

`artd` drives the GPIO line inactive on `SIGTERM` and again when the line
request is dropped, so `systemctl stop` and a clean reboot both leave the
amplifier off.

Neither covers `SIGKILL`, a kernel panic, or the Pi losing power. In all
three the kernel releases the line and the pin reverts to being an input,
with nothing driving it — and no software runs at that moment to help.

**The optocoupler input must have an external pull-down resistor** holding it
in the amplifier-off state (a pull-*up* if `power.amp.active_low` is set).
10k to ground is typical. Without it a crash can leave an amplifier powered
indefinitely, which is the one failure in this project that costs electricity
and annoys neighbours rather than merely showing the wrong picture.
