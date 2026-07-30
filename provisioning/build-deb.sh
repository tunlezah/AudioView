#!/usr/bin/env bash
#
# Build a .deb of LP Frame itself — binaries, config, units, placeholder.
#
# Not of shairport-sync or nqptp: those are upstream's to package, install.sh
# builds them when the distro's copy has no AirPlay 2, and vendoring someone
# else's daemon into our package would make us responsible for its security
# updates.
#
# The point of the .deb is that a device does not need a Rust toolchain. A Pi
# takes about ten minutes to build this workspace and roughly a gigabyte of
# disk to do it; a cross-build on a laptop takes under a minute.
#
#   ./provisioning/build-deb.sh                        # for this machine
#   ./provisioning/build-deb.sh --target aarch64-unknown-linux-gnu
#   ./provisioning/build-deb.sh --no-build --binary-dir path/to/binaries
#
# Then, on the device:
#   sudo apt install ./lpframe_<version>_arm64.deb

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=""
BUILD=1
BINARY_DIR=""
OUT_DIR="$REPO_ROOT/dist"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET="${2:?--target needs a triple}"; shift ;;
        --no-build) BUILD=0 ;;
        --binary-dir) BINARY_DIR="${2:?--binary-dir needs a path}"; shift ;;
        --out-dir) OUT_DIR="${2:?--out-dir needs a path}"; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

command -v dpkg-deb >/dev/null || { echo "dpkg-deb is missing (apt install dpkg-dev)" >&2; exit 1; }

# The crates inherit `version.workspace = true`, so the number lives in the
# root manifest's [workspace.package] and nowhere else.
VERSION="$(awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/                     { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
        gsub(/^version[[:space:]]*=[[:space:]]*"|"[[:space:]]*$/, ""); print; exit
    }
' "$REPO_ROOT/Cargo.toml")"
[[ -n "$VERSION" ]] ||
    { echo "could not read [workspace.package] version from Cargo.toml" >&2; exit 1; }

case "${TARGET:-$(uname -m)}" in
    aarch64*|arm64) ARCH="arm64" ;;
    x86_64*|amd64)  ARCH="amd64" ;;
    *) echo "unsupported target ${TARGET:-$(uname -m)}" >&2; exit 1 ;;
esac

# The renderer is built WITHOUT backend-sdl2, which is a default feature.
#
# SDL2 is the development backend — a window on a desktop. Linking it drags
# in X11, Wayland, PulseAudio, ALSA, dbus and the audio codecs behind
# libsndfile: fifty shared libraries instead of nine, none of which exist on
# a Raspberry Pi OS Lite image, all of which apt would then have to install
# onto an appliance that has no desktop and never opens a window.
LPRENDER_FEATURES=(--no-default-features --features backend-drm)

if [[ $BUILD -eq 1 ]]; then
    echo "building lpframe $VERSION for $ARCH"
    target_args=()
    [[ -n "$TARGET" ]] && target_args=(--target "$TARGET")
    (
        cd "$REPO_ROOT"
        cargo build --release "${target_args[@]}" -p artd -p lpctl
        cargo build --release "${target_args[@]}" -p lprender "${LPRENDER_FEATURES[@]}"
    )
    if [[ -n "$TARGET" ]]; then
        BINARY_DIR="${BINARY_DIR:-$REPO_ROOT/target/$TARGET/release}"
    else
        BINARY_DIR="${BINARY_DIR:-$REPO_ROOT/target/release}"
    fi
fi
BINARY_DIR="${BINARY_DIR:-$REPO_ROOT/target/release}"

for binary in artd lprender lpctl; do
    [[ -x "$BINARY_DIR/$binary" ]] || {
        echo "missing $BINARY_DIR/$binary — build first, or pass --binary-dir" >&2
        exit 1
    }
done

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

install -d "$STAGE/DEBIAN"
install -d "$STAGE/usr/bin" "$STAGE/usr/local/bin" "$STAGE/usr/share/lpframe"
install -d "$STAGE/etc/lpframe" "$STAGE/etc/systemd/system" "$STAGE/usr/lib/tmpfiles.d"
install -d "$STAGE/usr/share/doc/lpframe"

for binary in artd lprender lpctl; do
    install -m 0755 "$BINARY_DIR/$binary" "$STAGE/usr/bin/$binary"
done
# Stripping is worth about 60% of the installed size, and the device is not
# where anyone debugs this.
if command -v strip >/dev/null && [[ -z "$TARGET" || "$ARCH" == "$(dpkg --print-architecture)" ]]; then
    strip "$STAGE"/usr/bin/* 2>/dev/null || true
elif [[ "$ARCH" == "arm64" ]] && command -v aarch64-linux-gnu-strip >/dev/null; then
    aarch64-linux-gnu-strip "$STAGE"/usr/bin/* 2>/dev/null || true
fi

install -m 0755 "$REPO_ROOT/provisioning/lpframe-rw" "$STAGE/usr/local/bin/lpframe-rw"
install -m 0755 "$REPO_ROOT/provisioning/lpframe-ro" "$STAGE/usr/local/bin/lpframe-ro"
install -m 0755 "$REPO_ROOT/provisioning/make-writable-partition.sh" \
    "$STAGE/usr/local/bin/lpframe-make-writable-partition"
install -m 0644 "$REPO_ROOT/provisioning/placeholder.png" "$STAGE/usr/share/lpframe/placeholder.png"
install -m 0644 "$REPO_ROOT/provisioning/lpframe.tmpfiles.conf" "$STAGE/usr/lib/tmpfiles.d/lpframe.conf"
install -m 0644 "$REPO_ROOT/systemd/lpframe-artd.service" "$STAGE/etc/systemd/system/"
install -m 0644 "$REPO_ROOT/systemd/lpframe-lprender.service" "$STAGE/etc/systemd/system/"
install -m 0644 "$REPO_ROOT/docs/BUILD.md" "$STAGE/usr/share/doc/lpframe/BUILD.md"
install -m 0644 "$REPO_ROOT/systemd/README.md" "$STAGE/usr/share/doc/lpframe/systemd.md"

# Shipped as a conffile, so dpkg offers a three-way merge on upgrade rather
# than silently replacing an edited configuration.
install -m 0644 "$REPO_ROOT/provisioning/config.toml" "$STAGE/etc/lpframe/config.toml"
echo "/etc/lpframe/config.toml" >"$STAGE/DEBIAN/conffiles"

INSTALLED_KB="$(du -ks "$STAGE" | cut -f1)"

# Depends is what the binaries actually link, checked with ldd, plus one
# thing ldd cannot see: libgl1-mesa-dri holds the Gallium drivers Mesa opens
# with dlopen at runtime. Without it EGL initialises against no driver and
# the renderer fails at the first context creation with nothing useful said.
#
# Deliberately absent: libgpiod. artd talks to /dev/gpiochip* through
# gpiocdev, which implements the character-device ioctls in Rust and links
# nothing. An earlier draft required libgpiod2, which does not exist in
# trixie at all — it ships libgpiod 2.x as libgpiod3 — so the package would
# simply have refused to install. The gpiod tools are a Recommends because
# docs/BUILD.md uses them to test a trigger, not because anything links them.
cat >"$STAGE/DEBIAN/control" <<EOF
Package: lpframe
Version: $VERSION
Section: sound
Priority: optional
Architecture: $ARCH
Maintainer: LP Frame <nobody@localhost>
Installed-Size: $INSTALLED_KB
Depends: libc6, libgcc-s1, adduser, systemd, libegl1, libgbm1, libdrm2, libgl1-mesa-dri
Recommends: shairport-sync, avahi-daemon, alsa-utils, raspi-config, gpiod
Description: Album art frame for AirPlay
 A vinyl-LP-sized wall display that shows the cover art of whatever is
 currently AirPlaying to it, and nothing else. Renders full-bleed on
 KMS/DRM with GPU crossfades, upgrades AirPlay's ~500px art against public
 catalogues, and drives an amplifier over a 12V trigger.
 .
 This package does not include an AirPlay receiver. shairport-sync must be
 built with --with-airplay-2 --with-metadata; see
 /usr/share/doc/lpframe/BUILD.md.
EOF

cat >"$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

case "$1" in
configure)
    if ! getent passwd lpframe >/dev/null; then
        adduser --system --group --no-create-home --home /var/lib/lpframe \
                --shell /usr/sbin/nologin lpframe
    fi
    # The units name these as SupplementaryGroups=, so a missing one is a
    # service that will not start rather than one that degrades.
    for group in video render audio gpio; do
        getent group "$group" >/dev/null || addgroup --system "$group"
        adduser lpframe "$group" >/dev/null 2>&1 || true
    done

    install -d -m 0750 -o lpframe -g lpframe /var/lib/lpframe
    install -d -m 0750 -o lpframe -g lpframe /var/lib/lpframe/cache

    systemd-tmpfiles --create /usr/lib/tmpfiles.d/lpframe.conf || true
    systemctl daemon-reload || true

    # Enabled but not started: the boot configuration (KMS overlay, CMA,
    # getty@tty1 masked) may not be in place yet, and a renderer that cannot
    # get DRM master would just restart-loop in the log. install.sh starts
    # them once it has done that work.
    systemctl enable lpframe-artd.service lpframe-lprender.service || true

    if [ -d /run/systemd/system ] && systemctl is-active --quiet lpframe-artd.service; then
        systemctl restart lpframe-artd.service || true
        systemctl restart lpframe-lprender.service || true
    fi
    ;;
esac

exit 0
EOF

cat >"$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
remove|deconfigure)
    # artd drives the amplifier trigger inactive on SIGTERM, so stopping it
    # properly here is what stops a package removal leaving an amp powered.
    systemctl stop lpframe-lprender.service || true
    systemctl stop lpframe-artd.service || true
    systemctl disable lpframe-artd.service lpframe-lprender.service || true
    ;;
esac
exit 0
EOF

cat >"$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
purge)
    # /var/lib/lpframe holds the artwork cache, the settings written from the
    # web interface and the password hash. Purge means purge.
    rm -rf /var/lib/lpframe
    ;;
esac
[ -d /run/systemd/system ] && systemctl daemon-reload || true
exit 0
EOF

chmod 0755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

mkdir -p "$OUT_DIR"
DEB="$OUT_DIR/lpframe_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$DEB" >/dev/null

echo "$DEB"
dpkg-deb --info "$DEB" | sed -n '1,4p'
echo
echo "install it on the device with:"
echo "    sudo apt install ./$(basename "$DEB")"
