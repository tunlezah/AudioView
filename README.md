# LP Frame

A vinyl-LP-sized wall display that shows the cover art of whatever is currently AirPlaying to it, and nothing else.

Runs on a Raspberry Pi with a square panel, drives a USB DAC, and wakes an amplifier over a 12V trigger. Album art is rendered full-bleed on KMS/DRM with GPU crossfades, and upgraded from ~500×500 AirPlay art to up to 3000×3000 by looking the album up against public catalogues.

## Components

| | |
|---|---|
| `shairport-sync` + `nqptp` | AirPlay 2 receiver → ALSA → USB DAC (configured, not written here) |
| `spmeta` | Parser for the shairport-sync metadata pipe |
| `artd` | Metadata state machine, artwork enrichment, amp/display power |
| `lprender` | Fullscreen KMS/DRM + GLES3 renderer |
| `lpcapture` | Records real AirPlay sessions to replayable fixtures |

## Status

Design phase. See **[docs/DESIGN.md](docs/DESIGN.md)**.

Build guide (`docs/BUILD.md`) lands with milestone 7.
