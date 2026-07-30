# Building an LP Frame

From an empty SD card to a picture frame that shows whatever is playing.

Budget an evening. Most of it is waiting: the OS image writes in a few
minutes, `shairport-sync` takes about fifteen to build on a Pi 5, and the LP
Frame workspace another ten. [Route A](#route-a--install-a-built-package)
builds nothing on the device and takes about five minutes, at the cost of
needing a second Linux machine.

There is one step that can leave you with a dark screen and no way in
([forcing a mode](#4-get-a-picture)), and one that can leave an amplifier
powered on indefinitely ([the trigger](#7-the-amplifier-trigger)). Both are
marked. Everything else is recoverable by re-running the installer.

**Contents**

1. [What you need](#1-what-you-need)
2. [Flash the OS](#2-flash-the-os)
3. [First boot](#3-first-boot)
4. [Get a picture](#4-get-a-picture)
5. [Install LP Frame](#5-install-lp-frame)
6. [The DAC](#6-the-dac)
7. [The amplifier trigger](#7-the-amplifier-trigger)
8. [Play something](#8-play-something)
9. [The web interface](#9-the-web-interface)
10. [Lock it down](#10-lock-it-down)
11. [Updating](#11-updating)
12. [When it does not work](#12-when-it-does-not-work)
13. [Appendix: what ends up where](#appendix-what-ends-up-where)

---

## 1. What you need

| | | |
|---|---|---|
| **Board** | Raspberry Pi 5, 4GB | A Pi 4B 2GB works and is the documented fallback. Not a Zero — the renderer wants a real GPU and enrichment wants RAM. |
| **Storage** | 16GB A2 microSD, or a USB SSD | A2 for the random-write rating. The card ends up mostly read-only, but the build is not kind to it. |
| **Panel** | Square LCD + eDP/HDMI driver board | See [below](#about-the-panel). |
| **Audio** | USB Audio Class 2 DAC | Anything the kernel takes without a driver. A FiiO KA-series or similar is the reference. |
| **Power** | Official 27W USB-C for a Pi 5 | A Pi 5 with a USB DAC and a panel draws more than a phone charger will give, and the failure mode is corruption rather than a shutdown. |
| **Network** | Ethernet | Wi-Fi works. AirPlay 2 over Wi-Fi is fussier about multicast than it should be, and a frame on a wall is usually near a socket anyway. |
| **Trigger** *(optional)* | Optocoupler, 2 resistors, 12V supply | [Section 7](#7-the-amplifier-trigger). Skip it entirely if your amplifier has no trigger input. |

### About the panel

The intent is a 1:1 panel the size of an LP sleeve — about 12″, 1920×1920.
Those exist but are awkward to buy, so **LP Frame does not require one**. Any
resolution, aspect ratio, orientation and connector works: the artwork is
drawn in the largest square that fits and `render.background` fills the rest
(blurred, by default). A 1080p monitor in landscape looks deliberate rather
than broken.

Two things to know before ordering:

- **Square panels usually arrive as a bare LCD plus a driver board**, and the
  board's EDID is often wrong or absent. That is what
  [step 4](#4-get-a-picture) is about.
- **Check the panel is not 8-bit dithered from 6.** Album art is mostly large
  flat gradients, which is the one thing that shows banding.

For development, a Waveshare 4″ 720×720 HDMI panel is what this was written
against, and an ordinary monitor is fine for everything but the final look.

---

## 2. Flash the OS

**Raspberry Pi OS Lite, 64-bit.** Not Desktop — there is no compositor here,
`lprender` takes the display directly. Not 32-bit; the installer will refuse.

Use [Raspberry Pi Imager](https://www.raspberrypi.com/software/). Choose
*Raspberry Pi OS (other)* → *Raspberry Pi OS Lite (64-bit)*, then open the
settings gear before writing and set:

- **hostname** — `lpframe`. This is what you will type into a browser later,
  as `lpframe.local`.
- **enable SSH**, with a public key if you have one
- **username and password**
- **Wi-Fi**, if you are not using Ethernet
- **locale and timezone**

### Leave room for the data partition — do this now

Raspberry Pi OS expands its root partition to fill the card on first boot.
That is usually what you want, and here it is not: LP Frame ends up with a
read-only root and a small writable partition for its settings and artwork
cache, and once the root has eaten the card there is nowhere to put one.
Making room later means taking the card to another machine.

So, after Imager finishes and **before the first boot**, with the card still
in the machine that wrote it, open the small FAT partition (it mounts as
`bootfs`) and edit `cmdline.txt`. Delete this fragment, leaving the rest of
the line alone:

```
init=/usr/lib/raspberrypi-sys-mods/firstboot
```

It is all one line. Do not introduce a newline — everything after a line
break is silently ignored, and the symptom is a Pi that does not boot with no
clue as to why.

The root filesystem then stays at its image size, about 3GB, and the rest of
the card is left unallocated for [step 10](#10-lock-it-down).

> Skipping this is not fatal. You may decide you do not want a read-only root
> at all, or shrink the partition offline later —
> `make-writable-partition.sh` prints the exact commands when it finds no
> free space.

---

## 3. First boot

Insert the card, connect Ethernet and power, and give it a minute.

```bash
ssh <your-username>@lpframe.local
```

If the name does not resolve, find it by MAC prefix (`b8:27:eb`, `dc:a6:32`,
`d8:3a:dd` and `2c:cf:67` are Raspberry Pi) or in your router's lease table.

Bring it up to date and reboot:

```bash
sudo apt update && sudo apt full-upgrade -y
sudo reboot
```

Confirm the free space survived:

```bash
lsblk
df -h /
```

You want a root partition of roughly 3GB on a card that is much larger. If
root has filled the card, the `cmdline.txt` edit did not take — see the note
at the end of [step 2](#2-flash-the-os).

---

## 4. Get a picture

> **This is the step that can lock you out.** A forced mode a panel cannot
> display gives a black screen with nothing in the log. It is recoverable —
> the boot partition is FAT and mounts on any machine — but recovery means
> taking the card out. Read to the end of this section first.

Connect the panel and reboot. Then look at what the kernel found:

```bash
for c in /sys/class/drm/card*-*; do printf '%s: %s\n' "$(basename "$c")" "$(cat "$c/status")"; done
```

A connector reading `connected` means the panel was detected. Now the modes
it claims:

```bash
cat /sys/class/drm/card*-HDMI-A-1/modes
```

**If your panel's native resolution is in that list**, you are done — leave
`display.mode = "auto"` and skip to [step 5](#5-install-lp-frame).

**If the list is empty, wrong, or the panel is dark**, its driver board is
not presenting usable EDID and the mode has to be forced. The installer does
this for you:

```bash
sudo ./provisioning/install.sh --panel 1920x1920@60      # in step 5
```

which appends a kernel parameter to `cmdline.txt`:

```
video=HDMI-A-1:1920x1920M@60D
```

`M` asks the kernel to compute CVT timings for that size — which is what
these driver boards expect — and `D` forces digital output, so a panel
reporting no EDID at all is still driven.

> Older guides tell you to set `hdmi_group`, `hdmi_mode` and `hdmi_timings`
> in `config.txt`. Do not. Those are firmware-level settings from before the
> KMS driver; with `vc4-kms-v3d` the kernel owns modesetting, and on a Pi 5
> the firmware ignores them entirely. `video=` is what both boards obey.

**Before rebooting into a forced mode**, know how to undo it: power off, put
the card in another machine, open the FAT partition, and copy
`cmdline.txt.pre-lpframe` over `cmdline.txt`. The installer writes that
backup before it touches anything.

Once LP Frame is installed, the renderer will tell you what it can see, which
is more useful than the sysfs files:

```bash
lprender --config /etc/lpframe/config.toml \
         --config-local /var/lib/lpframe/config.local.toml --probe
```

It lists every card, connector and mode, and says whether the mode you
configured actually exists.

---

## 5. Install LP Frame

Two routes. **Route A** builds on another machine and copies a `.deb` over —
about five minutes, and the device never needs a Rust toolchain. **Route B**
builds on the Pi: slower, simpler, no second machine.

Either way, get the source:

```bash
git clone https://github.com/tunlezah/AudioView.git
cd AudioView
```

### Route A — install a built package

On a Linux machine with Rust:

```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu

./provisioning/build-deb.sh --target aarch64-unknown-linux-gnu
# → dist/lpframe_0.1.0_arm64.deb

scp dist/lpframe_0.1.0_arm64.deb lpframe.local:
```

On the Pi:

```bash
sudo apt install ./lpframe_0.1.0_arm64.deb
```

That installs the binaries, units, configuration and placeholder, creates the
`lpframe` user, and enables the services without starting them. It does
**not** install an AirPlay receiver or touch your boot configuration, so run
the installer for the rest:

```bash
sudo ./provisioning/install.sh --skip-lpframe
```

`--skip-lpframe` says the package already supplied the binaries, the
configuration and the placeholder, so the installer does only the parts the
package deliberately leaves alone: the AirPlay receiver, the boot
configuration, and starting everything. Add `--panel WxH@R` if
[step 4](#4-get-a-picture) said you need it.

> `lprender` needs the arm64 EGL/GLES/GBM/DRM libraries to link. If the
> cross-build fails on those, build the daemon on your laptop and let the Pi
> build the renderer — or use Route B, which is why it exists.

### Route B — build on the device

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

sudo ./provisioning/install.sh --panel 1920x1920@60
```

Drop `--panel` if the panel's EDID was fine.

### What the installer does

Run it with `--dry-run` first if you would like to read the plan; it prints
every command and changes nothing.

1. Checks the board, OS release and architecture, and says so when any of
   them is not what this was tested against.
2. Installs packages and creates the `lpframe` system user in `video`,
   `render`, `audio` and `gpio`.
3. Builds `shairport-sync` and `nqptp` from source — **unless** the installed
   `shairport-sync` already reports `AirPlay2`, in which case it leaves it
   alone. Distributions have historically shipped it without AirPlay 2
   because `nqptp` is a separate daemon; when yours catches up, this step
   disappears by itself.
4. Builds and installs `artd`, `lprender` and `lpctl`.
5. Writes `/etc/shairport-sync.conf` with metadata and cover art enabled.
6. Installs the systemd units and the tmpfiles fragment.
7. Sets `dtoverlay=vc4-kms-v3d,cma-256`, points journald at RAM, masks
   `getty@tty1` so it cannot hold DRM master, and forces the panel mode if
   you asked for one.
8. Enables and starts everything, checks it came up, and prints the generated
   web password.

It is safe to run repeatedly. It will not overwrite an
`/etc/lpframe/config.toml` you have edited — it puts the new one beside it as
`config.toml.new` and tells you.

Then:

```bash
sudo reboot
```

---

## 6. The DAC

Plug it in and find its name:

```bash
aplay -l
```

```
card 1: KA11 [FiiO KA11], device 0: USB Audio [USB Audio]
```

The stable name — the one that survives the card being renumbered at the next
boot, which it will be — is the `CARD=` form:

```bash
aplay -L | grep '^hw:CARD'
```

Set it in `/etc/lpframe/config.toml`:

```toml
[audio]
alsa_device = "hw:CARD=KA11,DEV=0"
```

and in `/etc/shairport-sync.conf`:

```
alsa = {
    output_device = "hw:CARD=KA11,DEV=0";
};
```

Test it before involving AirPlay:

```bash
speaker-test -D hw:CARD=KA11,DEV=0 -c 2 -t sine -l 1
```

Then restart the receiver:

```bash
sudo systemctl restart shairport-sync
```

**On hardware volume.** Leave `mixer = ""` unless you know the DAC's
attenuator is well behaved. With it empty, shairport-sync attenuates in
software at high precision, which on a modern DAC is inaudible and avoids the
class of bug where an AirPlay volume change and a hardware mixer disagree
about where the volume actually is.

---

## 7. The amplifier trigger

Skip this section entirely if your amplifier has no 12V trigger input. LP
Frame works fine without one — set `power.amp.enabled = false`.

> **The one failure here costs electricity rather than pixels.** Fit the
> pull-down resistor, or a crashed Pi can leave your amplifier powered on
> indefinitely.

### The circuit

GPIO17 (physical pin 11) drives an optocoupler LED; the optocoupler's output
transistor switches 12V to the amplifier's trigger jack. The optocoupler is
what keeps the amplifier's ground away from the Pi's.

```
   Pi header                    PC817 (or any 4-pin optocoupler)
   ─────────                    ────────────────────────────────
   pin 11 ───[ 330R ]──────────┤1 anode          collector 4├──── +12V
   GPIO17        │              │                            │
               [ 10k ]          │                            │
                  │             │                            │
   pin 9  ────────┴────────────┤2 cathode          emitter 3├──┬─── trigger tip
   GND                          └────────────────────────────┘  │
                                                             [ 10k ]
   12V supply ground ────────────────────────────────────────────┴─── trigger sleeve
```

- **330R** sets the LED current to about 5mA at 3.3V. Comfortably inside a
  GPIO pin's rating and plenty for a PC817.
- **10k from GPIO17 to ground is not optional.** `artd` drives the line
  inactive on `SIGTERM` and again when it releases the line, so `systemctl
  stop` and a clean reboot both leave the amplifier off. Nothing runs on
  `SIGKILL`, a kernel panic, or the Pi losing power: the kernel releases the
  line, the pin reverts to an input, and with nothing holding it there is no
  defined state. The resistor is the only thing covering those cases. If you
  set `power.amp.active_low = true`, this becomes a pull-*up* to 3.3V
  instead.
- **10k across the trigger output** stops that line floating while the
  optocoupler is off. Many amplifiers have one internally; adding it costs
  nothing and removes the question.
- **The 12V supply is yours to provide.** An amplifier's trigger input
  expects to be *fed* 12V, not to supply it. A small wall wart or a 5V→12V
  boost module both work. Many trigger inputs will happily take 5V or even
  3.3V, in which case you can drive them from the Pi's own supply and skip
  the extra brick — check your amplifier's manual before assuming.

### Configure and test

```toml
[power.amp]
enabled = true
gpio_chip = "auto"          # resolved by label, not number — see below
gpio_line = 17
active_low = false
pulse = "0ms"               # >0 emits a momentary pulse instead of holding
on_event = "session_begin"
off_delay = "10m"
```

`gpio_chip = "auto"` picks the chip by *label* (`pinctrl-rp1` on a Pi 5,
`pinctrl-bcm2835` on a Pi 4), because the `gpiochipN` numbers have moved
between kernel releases and hard-coding one gives you a device that stops
working after an update. To see what is on yours:

```bash
gpiodetect
```

Test it with nothing attached, watching a multimeter or an LED. Stop the
daemon first — two things cannot hold the same line:

```bash
sudo systemctl stop lpframe-artd
gpioset --mode=time --sec=2 $(gpiodetect | grep -m1 -o '^gpiochip[0-9]*') 17=1
sudo systemctl start lpframe-artd
```

Then, from the Diagnostics page or the command line:

```bash
lpctl amp on
lpctl amp off
```

Each of those waits for the daemon to acknowledge before returning, so a
command that comes back has been carried out rather than merely sent.

**`on_event`.** `session_begin` fires once when a listening session starts
and once when it ends, with about ten seconds of hysteresis either side.
`play_begin` fires per *track*, which under AirPlay 2 means the relay clicks
between every song — which is why `session_begin` is the default.
`off_delay` then holds the amplifier up for ten minutes after the music
stops, so flipping between albums does not power-cycle it.

---

## 8. Play something

```bash
systemctl status lpframe-artd lpframe-lprender shairport-sync nqptp
```

All four should be `active (running)`. Watch the state machine:

```bash
lpctl watch
```

Now pick **LP Frame** as an AirPlay target on a phone or from macOS and play
something. In `lpctl watch` you should see, in order: a session beginning,
track metadata, then an artwork revision. On the panel the art fades in — and
a second or two later fades again, very slightly sharper, as the enrichment
pipeline replaces AirPlay's ~500px thumbnail with the full-size cover.

If the *first* track after a boot has no art but later ones do, that is the
one ordering constraint in `systemd/README.md`: `shairport-sync` discards
metadata written while nothing is reading the pipe, so `artd` has to be
attached first. `Before=shairport-sync.service` on `lpframe-artd.service` is
what prevents it, and something has removed it.

---

## 9. The web interface

Open **`http://lpframe.local:8730/`**.

The password was printed by the installer, and also:

```bash
sudo lpctl web-password
```

It is a hundred bits of entropy in four readable groups. Only the Argon2id
hash is kept in the configuration; the plaintext is in
`/var/lib/lpframe/web-password.txt`, mode 0600. To force a new one, delete
`web.password_hash` from `/var/lib/lpframe/config.local.toml` and restart
`artd`.

Three pages: **Now Playing** (a live view of the same snapshot the renderer
gets), **Settings**, and **Diagnostics** — counters, plus a note for every
enrichment decision with the scores behind it.

Settings are labelled with what each needs before it takes effect: *live*,
*needs the renderer restarted*, or *needs the daemon restarted*. The page
offers a button for the latter two.

Changing `display.rotation` or `display.mode` starts a fifteen-second
countdown. Unless you click **Keep this**, the old value comes back — which is
what stops a wrong mode on a device with no keyboard being a reflash.

**What the authentication is, and is not.** It keeps other people and other
devices on your network out of your listening history and your settings. It
is plain HTTP: the password crosses the network in the clear on every login,
and the session cookie on every request. It does nothing against someone
capturing traffic on your LAN. If that matters, set
`web.bind = "127.0.0.1:8730"` and reach it over a tunnel:

```bash
ssh -L 8730:localhost:8730 lpframe.local
```

**Do not port-forward it.** There is no WAN mode and no cloud component. The
full threat model, including what is deliberately not defended against, is at
the top of `crates/artd/src/web/auth.rs`.

---

## 10. Lock it down

Optional, and worth doing on anything that gets switched off at the wall.
Under a read-only root an unclean power cut cannot corrupt the filesystem,
because nothing is being written to corrupt.

**Order matters.** Data partition first, then the overlay. `lpframe-ro`
refuses to run in the wrong order, because enabling the overlay while
`/var/lib/lpframe` is still on the root filesystem gives you a device that
appears to work and forgets every setting, the generated password and the
whole artwork cache at each power cycle.

### The data partition

```bash
sudo ./provisioning/make-writable-partition.sh
```

It creates a 2GB ext4 partition in the free space you left in
[step 2](#2-flash-the-os), moves `/var/lib/lpframe` onto it, and mounts it by
label from `/etc/fstab` with `nofail` — so a card that loses the partition
still boots and says so, rather than not booting.

It only ever appends to free space and will not shrink a filesystem. If there
is none, it stops and prints the two ways to make some.

### The overlay

```bash
sudo lpframe-ro
sudo reboot
```

Afterwards `/` is an overlay: writes go to RAM and vanish at power off.
`/var/lib/lpframe` stays writable, so the web interface still works and
settings still persist. Check both:

```bash
findmnt /                    # → overlay
findmnt /var/lib/lpframe     # → /dev/mmcblk0p3
```

To change anything on the system afterwards:

```bash
sudo lpframe-rw && sudo reboot     # do the work
sudo lpframe-ro && sudo reboot     # put it back
```

A device left writable will eventually be corrupted by a power cut mid-write,
and the symptom is a filesystem that will not mount rather than anything
naming the cause. Put it back.

---

## 11. Updating

```bash
sudo lpframe-rw && sudo reboot          # only if the overlay is on
cd AudioView && git pull
sudo ./provisioning/install.sh --skip-boot-config
sudo lpframe-ro && sudo reboot
```

Or with a package built elsewhere:

```bash
sudo apt install ./lpframe_<version>_arm64.deb
```

`/etc/lpframe/config.toml` is a dpkg conffile, so an upgrade asks before
replacing one you have edited. `/var/lib/lpframe/config.local.toml` —
everything the web interface has written — is never touched by either route
and survives a `remove`. Only `apt purge` deletes it.

To go back:

```bash
sudo ./provisioning/install.sh --uninstall
```

That stops and removes the services, binaries and units, and leaves
`/etc/lpframe`, `/var/lib/lpframe` and your boot configuration alone. It
prints the commands for those if you want them gone too.

---

## 12. When it does not work

Everything logs to the journal. Start here:

```bash
journalctl -u lpframe-artd -u lpframe-lprender -u shairport-sync -b --no-pager
```

| Symptom | Where to look |
|---|---|
| **Black screen, renderer restarting** | `journalctl -u lpframe-lprender -b`. Usually DRM master is held by something else — check `getty@tty1` is masked — or the configured mode does not exist. Run `lprender --probe`. |
| **Black screen, renderer running, `lpctl watch` shows artwork** | It has the card but is drawing nothing visible. Check `display.rotation` and `display.mode`, and try `render.background = "dominant"` to see whether *anything* reaches the panel. |
| **Frame does not appear as an AirPlay target** | `systemctl status shairport-sync nqptp avahi-daemon`. AirPlay 2 needs `nqptp` running and mDNS working. Confirm the build: `shairport-sync -V` must contain `AirPlay2`. |
| **Appears, but connecting fails** | Almost always `nqptp`. It binds UDP 319 and 320 and must be the only thing doing so. |
| **Audio drops or crackles** | Try another USB port — on a Pi 5, a USB 3 one — and check `dmesg` for XHCI resets. A DAC sharing a hub with anything else is a common cause. |
| **First track after boot has no art, later ones do** | The ordering constraint. `systemctl show lpframe-artd -p Before` must list `shairport-sync.service`. See `systemd/README.md`. |
| **No art at all, ever** | Check `metadata` and `include_cover_art` in `/etc/shairport-sync.conf`, and that `pipe_name` matches `device.metadata_pipe`. Then `lpctl watch` while playing: if the state machine sees tracks but no artwork, the sender is not providing any. |
| **Art appears but never sharpens** | Enrichment. The Diagnostics counters say which gate rejected it: `text_rejections` means the catalogue lookup did not match, `perceptual_rejections` means it found something that did not look like the same cover. |
| **Settings do not survive a reboot** | The overlay is on and `/var/lib/lpframe` is not its own partition. `findmnt /var/lib/lpframe`. See [step 10](#10-lock-it-down). |
| **Web page unreachable** | `systemctl status lpframe-artd` — a refused bind fails the whole daemon deliberately, and the log says why. Usually `web.auth = false` with a non-loopback `web.bind`. |
| **Amplifier stays on** | Check the pull-down resistor is fitted, then `pinctrl get 17` on a Pi 5, or `gpioinfo`, to see the line's actual state. |

`lpctl` is the quickest way to see what the daemon thinks:

```bash
lpctl watch            # live state, one line per change
lpctl status --json    # the current snapshot
lpctl amp on|off       # override the trigger
lpctl display on|ambient|off
lpctl ping             # is the daemon answering at all
lpctl web-password     # reprint it
```

---

## Appendix: what ends up where

| Path | |
|---|---|
| `/usr/bin/{artd,lprender,lpctl}` | the binaries |
| `/usr/local/bin/{shairport-sync,nqptp}` | when built from source |
| `/usr/local/bin/lpframe-{rw,ro}` | overlay toggles |
| `/etc/lpframe/config.toml` | the package's configuration; a dpkg conffile |
| `/var/lib/lpframe/config.local.toml` | what the web interface writes; wins per key |
| `/var/lib/lpframe/web-password.txt` | mode 0600, the generated password |
| `/var/lib/lpframe/cache/` | the enrichment cache, capped by `cache.max_bytes` |
| `/run/lpframe/artd.sock` | the snapshot socket |
| `/run/lpframe/art/` | current artwork, on tmpfs — image bytes never cross the socket |
| `/usr/share/lpframe/placeholder.png` | shown when a track has no cover art |
| `/etc/systemd/system/lpframe-*.service` | the units |
| `/usr/lib/tmpfiles.d/lpframe.conf` | creates `/run/lpframe` at boot |
| `/etc/shairport-sync.conf` | written once by the installer, then yours |
| `/boot/firmware/config.txt.pre-lpframe` | your original, before the installer touched it |

Configuration is layered: `/etc/lpframe/config.toml` is the package's, and
`/var/lib/lpframe/config.local.toml` overrides it per key. Only the second is
writable under a read-only root, which is the entire reason there are two.
See DESIGN §7.1.

---

## Building an image instead

Everything above produces one device. To produce many — or to reproduce this
one exactly — the pieces are scriptable: `build-deb.sh` makes the package,
`install.sh --no-build` consumes it, and neither needs interaction with
`--yes`. A [pi-gen](https://github.com/RPi-Distro/pi-gen) stage wrapping
those two into a flashable `.img` is milestone 9 and is not written yet.

Until then, the practical approach is to build one device, get it exactly
right, and image the card:

```bash
# on another machine, card inserted, nothing mounted
sudo dd if=/dev/sdX of=lpframe.img bs=4M status=progress
```

Shrink it afterwards with [PiShrink](https://github.com/Drewsif/PiShrink) if
you intend to write it to smaller cards.

The image carries the generated web password hash and the SSH host keys.
Regenerate both on any device flashed from it, or every one of them will
share a password and a host identity:

```bash
sudo rm -f /var/lib/lpframe/web-password.txt /etc/ssh/ssh_host_*
sudo sed -i '/^password_hash/d' /var/lib/lpframe/config.local.toml
sudo dpkg-reconfigure openssh-server
sudo systemctl restart lpframe-artd
```
