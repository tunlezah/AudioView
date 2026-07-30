#!/usr/bin/env python3
"""Generate provisioning/placeholder.png.

The shipped configuration points `render.placeholder` at a file, and a device
that reaches a track with no cover art shows it. Without one the renderer
fades to black and logs a warning per track, which looks like a fault.

Everything here is radial, so anti-aliasing is analytic — coverage from the
distance to each edge — rather than supersampled. That keeps a
dependency-free generator down to one pass over a million pixels, and the
edges are cleaner than a 2x box filter would give.

Checked in as a PNG as well as a script: the installer must not need Python.
"""

import struct
import zlib
from math import cos, pi, sqrt

SIZE = 1024

BACKDROP = (0x0B, 0x0B, 0x0D)
VINYL = (0x16, 0x16, 0x1A)
GROOVE = (0x1F, 0x1F, 0x25)
RIM = (0x2E, 0x2E, 0x36)
LABEL = (0x42, 0x33, 0x22)
LABEL_EDGE = (0x55, 0x42, 0x2D)

# Fractions of the image width.
R_DISC = 0.440
R_RIM = 0.433
R_LABEL = 0.152
R_HOLE = 0.0135

# One groove every this many pixels of radius, at 1024 wide.
GROOVE_PITCH = 5.0


def mix(a, b, t):
    """Blend two colours. `t` of 0 is `a`, 1 is `b`."""
    t = 0.0 if t < 0.0 else 1.0 if t > 1.0 else t
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def edge(distance, radius, softness=1.0):
    """Coverage of a disc of `radius` at `distance` from its centre.

    Linear across one pixel of the boundary, which for a circle this size is
    indistinguishable from a proper filter and costs nothing.
    """
    t = (radius - distance) / softness + 0.5
    return 0.0 if t < 0.0 else 1.0 if t > 1.0 else t


def main():
    centre = (SIZE - 1) / 2.0
    r_disc, r_rim = R_DISC * SIZE, R_RIM * SIZE
    r_label, r_hole = R_LABEL * SIZE, R_HOLE * SIZE

    rows = bytearray()
    for y in range(SIZE):
        dy = y - centre
        rows.append(0)  # PNG filter type 0 (None) for this scanline
        row = bytearray()
        for x in range(SIZE):
            dx = x - centre
            d = sqrt(dx * dx + dy * dy)

            # Grooves: a shallow cosine in radius, fading out towards the
            # label so the transition into it is not a hard ring.
            phase = 0.5 - 0.5 * cos(2.0 * pi * d / GROOVE_PITCH)
            reach = edge(d, r_rim, 24.0) * (1.0 - edge(d, r_label + 18.0, 26.0))
            colour = mix(VINYL, GROOVE, phase * 0.75 * reach)

            # A brighter rim, so the record reads as an object rather than a
            # hole, and the light catches its outer edge.
            colour = mix(colour, RIM, (1.0 - edge(d, r_rim, 14.0)) * 0.9)

            # The label, with its own slightly lighter edge.
            colour = mix(colour, LABEL_EDGE, edge(d, r_label + 2.0))
            colour = mix(colour, LABEL, edge(d, r_label - 2.0))

            # Backdrop outside the disc, and through the spindle hole.
            colour = mix(BACKDROP, colour, edge(d, r_disc))
            colour = mix(colour, BACKDROP, edge(d, r_hole))

            row += bytes(colour)
        rows += row

    def chunk(kind, payload):
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(
            ">I", zlib.crc32(body) & 0xFFFFFFFF
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(rows), 9))
    png += chunk(b"IEND", b"")

    out = __file__.rsplit("/", 1)[0] + "/placeholder.png"
    with open(out, "wb") as f:
        f.write(png)
    print(f"wrote {out} ({len(png)} bytes, {SIZE}x{SIZE})")


if __name__ == "__main__":
    main()
