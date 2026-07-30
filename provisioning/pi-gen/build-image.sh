#!/usr/bin/env bash
#
# Build a flashable LP Frame image (DESIGN §10, milestone 9).
#
# Wraps pi-gen: cross-builds our package, drops stage-lpframe into a pinned
# checkout, runs the build, then appends the /var/lib/lpframe partition.
#
# The image carries no credentials of any kind — no user password, no SSH
# host keys, no web password. Raspberry Pi Imager's customisation supplies
# the first two at flash time and artd generates the third on first boot.
# An appliance image with a default login is a device on somebody's network
# with a password everyone knows.
#
#   sudo ./provisioning/pi-gen/build-image.sh
#
# Wants an amd64 or arm64 Debian/Ubuntu host, root, and about 12GB of disk
# and an hour. Most of that hour is shairport-sync compiling under qemu,
# because Debian's package is built without AirPlay 2.

set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$REPO_ROOT/provisioning/pi-gen"

# Pinned, not a branch. An image build that silently follows someone else's
# default branch is not reproducible and is not something to hand anyone.
# From RPi-Distro/pi-gen, branch arm64 — the 64-bit images come from there;
# master hard-codes ARCH=armhf.
PI_GEN_REF="${PI_GEN_REF:-ca8aeed0ae300c2a89f55ce9617d5f96a27e99e5}"
PI_GEN_REPO="${PI_GEN_REPO:-https://github.com/RPi-Distro/pi-gen.git}"

WORK="${WORK:-$REPO_ROOT/build/pi-gen}"
DEB=""
BUILD_DEB=1
DATA_MIB=2048
COMPRESS=1
HOSTNAME_DEFAULT="lpframe"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --deb) DEB="${2:?--deb needs a path}"; BUILD_DEB=0; shift ;;
        --work) WORK="${2:?--work needs a path}"; shift ;;
        --data-size) DATA_MIB="${2:?--data-size needs MiB}"; shift ;;
        --hostname) HOSTNAME_DEFAULT="${2:?--hostname needs a name}"; shift ;;
        --no-compress) COMPRESS=0 ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ -t 1 ]]; then BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'
else BOLD=""; DIM=""; RESET=""; fi
step() { printf '\n%s==> %s%s\n' "$BOLD" "$*" "$RESET"; }
say()  { printf '    %s\n' "$*"; }
die()  { printf '\nerror: %s\n' "$*" >&2; exit 1; }

# --- preflight --------------------------------------------------------------

step "Checking the host"

[[ $EUID -eq 0 ]] || die "pi-gen needs root (loop devices, chroot, binfmt). Use sudo."

missing=()
for tool in debootstrap parted kpartx qemu-aarch64-static capsh bsdtar zerofree \
            mkdosfs mke2fs quilt pigz xxd arch-test rsync file bc git curl xz; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [[ ${#missing[@]} -gt 0 ]]; then
    cat >&2 <<EOF
error: missing tools: ${missing[*]}

On Debian or Ubuntu:

    sudo apt-get install -y quilt parted qemu-user-static debootstrap zerofree \\
        zip dosfstools libarchive-tools libcap2-bin grep rsync xz-utils curl \\
        xxd file git kmod bc gpg pigz arch-test kpartx

EOF
    exit 1
fi

# binfmt is what lets the arm64 chroot run at all. Without it the first
# chroot command fails with "Exec format error", which does not obviously
# name the cause.
if [[ ! -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]]; then
    say "registering qemu-aarch64 with binfmt_misc"
    if command -v update-binfmts >/dev/null; then
        update-binfmts --enable qemu-aarch64 || true
    fi
    [[ -e /proc/sys/fs/binfmt_misc/qemu-aarch64 ]] ||
        die "no binfmt_misc handler for aarch64. On a host with systemd:
    sudo systemctl restart systemd-binfmt
or install qemu-user-static and try again."
fi
say "aarch64 emulation is registered"

# --- the package ------------------------------------------------------------

step "Building the package"

if [[ $BUILD_DEB -eq 1 ]]; then
    command -v cargo >/dev/null ||
        die "cargo not found. Install Rust, or pass a prebuilt package with --deb."
    say "cross-building for aarch64 (a minute or so)"
    "$REPO_ROOT/provisioning/build-deb.sh" --target aarch64-unknown-linux-gnu >/dev/null
    DEB="$(ls -t "$REPO_ROOT"/dist/lpframe_*_arm64.deb 2>/dev/null | head -1)"
fi

[[ -n "$DEB" && -f "$DEB" ]] || die "no package to install (looked for $DEB)"
say "package: $DEB"

# --- assemble the pi-gen tree ------------------------------------------------

step "Preparing pi-gen"

mkdir -p "$WORK"
PI_GEN="$WORK/pi-gen"

if [[ -d "$PI_GEN/.git" ]]; then
    say "reusing $PI_GEN"
    git -C "$PI_GEN" fetch --quiet origin "$PI_GEN_REF" 2>/dev/null || true
else
    say "cloning pi-gen at ${PI_GEN_REF:0:12}"
    git init --quiet "$PI_GEN"
    git -C "$PI_GEN" remote add origin "$PI_GEN_REPO" 2>/dev/null || true
    git -C "$PI_GEN" fetch --quiet --depth 1 origin "$PI_GEN_REF"
fi
git -C "$PI_GEN" -c advice.detachedHead=false checkout --quiet --force "$PI_GEN_REF"
say "pi-gen at $(git -C "$PI_GEN" rev-parse --short HEAD)"

# What we build on. Checked rather than assumed: if pi-gen reorganises its
# stages, the useful failure is here and by name, not an hour later.
for required in stage0 stage1 stage2 export-image; do
    [[ -d "$PI_GEN/$required" ]] ||
        die "pi-gen ${PI_GEN_REF:0:12} has no $required — this stage was written against a different layout"
done

# Lite only. stage3 upwards is the desktop; skipping a stage that is already
# gone is not an error.
for desktop in stage3 stage4 stage5; do
    [[ -d "$PI_GEN/$desktop" ]] || continue
    touch "$PI_GEN/$desktop/SKIP" "$PI_GEN/$desktop/SKIP_IMAGES"
done

# stage2 is built — it is Lite — but must not export its own image, or the
# build spends an extra ten minutes and several gigabytes producing a plain
# Raspberry Pi OS Lite alongside ours.
touch "$PI_GEN/stage2/SKIP_IMAGES"

rm -rf "$PI_GEN/stage-lpframe"
cp -a "$HERE/stage-lpframe" "$PI_GEN/stage-lpframe"

# pi-gen silently skips a NN-run.sh that is not executable — it logs "Skip
# ... (not executable)" among thousands of other lines and produces an image
# with none of our software in it.
chmod +x "$PI_GEN/stage-lpframe/prerun.sh" \
         "$PI_GEN/stage-lpframe"/*/[0-9][0-9]-run.sh

STAGE_FILES="$PI_GEN/stage-lpframe/00-lpframe/files"
install -m 0644 "$DEB" "$STAGE_FILES/lpframe.deb"
rm -rf "$STAGE_FILES/provisioning" "$STAGE_FILES/systemd"
mkdir -p "$STAGE_FILES/provisioning"
# The installer and what it installs. Not the whole repository: an image
# build should not carry the source tree, the fixtures or the git history
# into the rootfs even temporarily.
for f in install.sh config.toml placeholder.png lpframe.tmpfiles.conf \
         lpframe-rw lpframe-ro make-writable-partition.sh; do
    cp -a "$REPO_ROOT/provisioning/$f" "$STAGE_FILES/provisioning/$f"
done
cp -a "$REPO_ROOT/systemd" "$STAGE_FILES/systemd"
say "staged the installer, the units and the package"

cat >"$PI_GEN/config" <<EOF
IMG_NAME=lpframe
RELEASE=trixie
TARGET_HOSTNAME=$HOSTNAME_DEFAULT
STAGE_LIST="stage0 stage1 stage2 stage-lpframe"

# Raw, because add-data-partition.sh has to append a third partition before
# anything is compressed.
DEPLOY_COMPRESSION=none

# Deliberately unset: FIRST_USER_PASS, ENABLE_SSH, PUBKEY_SSH_FIRST_USER.
# With no password set, Raspberry Pi OS requires the flasher to supply one —
# through Imager's customisation, or a userconf.txt on the boot partition.
# See stage-lpframe/00-lpframe/files/README-FIRST.txt, which ships on the
# boot partition where a locked-out user can still read it.
LOCALE_DEFAULT=en_GB.UTF-8
KEYBOARD_KEYMAP=gb
KEYBOARD_LAYOUT="English (UK)"
TIMEZONE_DEFAULT=Europe/London
EOF
say "wrote $PI_GEN/config"

# --- build -------------------------------------------------------------------

step "Building the image"
say "this is the long part; shairport-sync compiles under emulation"
say "log: $PI_GEN/work/lpframe/build.log"

( cd "$PI_GEN" && ./build.sh )

IMG="$(ls -t "$PI_GEN"/deploy/*.img 2>/dev/null | head -1)"
[[ -n "$IMG" ]] || die "pi-gen finished but produced no .img in $PI_GEN/deploy"
say "built $(basename "$IMG")"

# --- the data partition -------------------------------------------------------

step "Adding the /var/lib/lpframe partition"
"$HERE/add-data-partition.sh" "$IMG" --size "$DATA_MIB"

# --- ship ---------------------------------------------------------------------

step "Finishing"
mkdir -p "$REPO_ROOT/dist"
FINAL="$REPO_ROOT/dist/$(basename "$IMG")"
mv "$IMG" "$FINAL"

if [[ $COMPRESS -eq 1 ]]; then
    say "compressing (several minutes; the image is mostly zeroes)"
    xz -T0 -6 --force "$FINAL"
    FINAL="$FINAL.xz"
fi

sha256sum "$FINAL" >"$FINAL.sha256"

echo
printf '    %s%s%s\n' "$BOLD" "$FINAL" "$RESET"
printf '    %s%s%s\n' "$DIM" "$(cat "$FINAL.sha256" | cut -c1-64)" "$RESET"
echo
cat <<EOF
    Write it with Raspberry Pi Imager, and use the gear icon to set the
    hostname, a username and password, SSH and Wi-Fi. The image has no
    account and no password of its own — without that customisation there
    is no way to log in, because the text console is off so the renderer
    can own the display.
EOF
