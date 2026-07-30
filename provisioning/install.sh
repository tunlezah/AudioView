#!/usr/bin/env bash
#
# LP Frame installer — clean Raspberry Pi OS Lite to working device.
#
# Run it as many times as you like: every step checks the state it wants
# before changing anything, so re-running after a failure resumes rather
# than doubling up. `--dry-run` prints the plan and touches nothing, which
# is also how CI exercises this file.
#
# What it deliberately will not do without being asked twice:
#   * repartition anything (see make-writable-partition.sh)
#   * enable the overlay root (see lpframe-ro)
#   * overwrite an existing /etc/lpframe/config.toml
#
# docs/BUILD.md is the guide this implements; read that first.

set -euo pipefail

# --- what we are installing -------------------------------------------------

SHAIRPORT_VERSION="4.3.7"
NQPTP_VERSION="1.2.4"

LPFRAME_USER="lpframe"
BIN_DIR="/usr/bin"
LOCAL_BIN_DIR="/usr/local/bin"
CONF_DIR="/etc/lpframe"
STATE_DIR="/var/lib/lpframe"
SHARE_DIR="/usr/share/lpframe"
UNIT_DIR="/etc/systemd/system"
TMPFILES_DIR="/usr/lib/tmpfiles.d"

# Our own binaries, and the units that reference them by these names.
LPFRAME_BINARIES=(artd lprender lpctl)
LPFRAME_UNITS=(lpframe-artd.service lpframe-lprender.service)
VENDOR_UNITS=(shairport-sync.service nqptp.service)

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

# --- options ----------------------------------------------------------------

DRY_RUN=0
ASSUME_YES=0
BUILD=1
BINARY_DIR=""
DO_SHAIRPORT=1
DO_BOOT_CONFIG=1
DO_ENABLE=1
DO_LPFRAME=1
UNINSTALL=0
PANEL_MODE=""

usage() {
    cat <<'EOF'
Usage: sudo ./provisioning/install.sh [options]

  --dry-run           print every action, change nothing (does not need root)
  --yes               do not prompt
  --no-build          do not run cargo; take binaries from --binary-dir
  --binary-dir DIR    where prebuilt artd/lprender/lpctl are
                      (default: target/release, or target/<triple>/release)
  --skip-lpframe      the .deb already installed the binaries, config and
                      placeholder; do everything else
  --skip-shairport    leave shairport-sync and nqptp alone
  --skip-boot-config  do not touch config.txt
  --no-enable         install everything but do not start the services
  --panel WxH@R       force a mode for a panel whose EDID does not advertise
                      its own, e.g. --panel 1920x1920@60
  --uninstall         remove services, binaries and units; keep /var/lib/lpframe
  -h, --help          this

Read docs/BUILD.md before the first run on a new device.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --yes|-y) ASSUME_YES=1 ;;
        --no-build) BUILD=0 ;;
        --binary-dir) BINARY_DIR="${2:?--binary-dir needs a path}"; shift ;;
        --skip-lpframe) DO_LPFRAME=0; BUILD=0 ;;
        --skip-shairport) DO_SHAIRPORT=0 ;;
        --skip-boot-config) DO_BOOT_CONFIG=0 ;;
        --no-enable) DO_ENABLE=0 ;;
        --panel) PANEL_MODE="${2:?--panel needs WxH@R}"; shift ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

# --- output -----------------------------------------------------------------

if [[ -t 1 ]]; then
    BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; YELLOW=$'\033[33m'
    GREEN=$'\033[32m'; RESET=$'\033[0m'
else
    BOLD=""; DIM=""; RED=""; YELLOW=""; GREEN=""; RESET=""
fi

STEP=0
step()  { STEP=$((STEP + 1)); printf '\n%s[%d] %s%s\n' "$BOLD" "$STEP" "$*" "$RESET"; }
info()  { printf '    %s\n' "$*"; }
skip()  { printf '    %s· %s%s\n' "$DIM" "$*" "$RESET"; }
ok()    { printf '    %s✓ %s%s\n' "$GREEN" "$*" "$RESET"; }
warn()  { printf '    %s! %s%s\n' "$YELLOW" "$*" "$RESET" >&2; }
die()   { printf '\n%serror: %s%s\n' "$RED" "$*" "$RESET" >&2; exit 1; }

# Run a command, or print it under --dry-run. Everything that changes the
# system goes through this, which is what makes --dry-run trustworthy rather
# than a claim.
run() {
    if [[ $DRY_RUN -eq 1 ]]; then
        printf '    %s$ %s%s\n' "$DIM" "$*" "$RESET"
    else
        "$@"
    fi
}

# Append a line to a file if it is not already there. Idempotent by
# inspection rather than by marker comment, so a hand edit that happens to
# say the same thing is respected.
ensure_line() {
    local line="$1" file="$2"
    if [[ -f "$file" ]] && grep -qxF -- "$line" "$file"; then
        skip "already in $(basename "$file"): $line"
        return
    fi
    info "adding to $file: $line"
    if [[ $DRY_RUN -eq 0 ]]; then
        printf '%s\n' "$line" >>"$file"
    fi
}

# Keep the first version of a file we are about to edit, and only the first:
# a second run must not overwrite the backup with the already-modified copy.
#
# Not the no-clobber flag to cp: coreutils warns that its behaviour is
# non-portable and may change, and a warning printed in the middle of an
# installer is a thing people stop reading.
backup_once() {
    local file="$1" backup="$1.pre-lpframe"
    if [[ -e "$backup" ]]; then
        skip "backup already exists: $backup"
    elif [[ -e "$file" ]]; then
        info "keeping the original as $backup"
        run cp -p "$file" "$backup"
    fi
}

confirm() {
    [[ $ASSUME_YES -eq 1 || $DRY_RUN -eq 1 ]] && return 0
    local answer
    read -r -p "    $1 [y/N] " answer
    [[ "$answer" =~ ^[Yy] ]]
}

# --- preflight --------------------------------------------------------------

OS_ID=""; OS_CODENAME=""; PI_MODEL=""; BOOT_CONFIG=""

preflight() {
    step "Checking the machine"

    if [[ $DRY_RUN -eq 0 && $EUID -ne 0 ]]; then
        die "run this with sudo (or --dry-run to see what it would do)"
    fi

    local arch; arch="$(uname -m)"
    case "$arch" in
        aarch64) ok "64-bit ARM" ;;
        x86_64)
            warn "x86_64 — this is a development box, not a device."
            warn "Binaries and units install, but there is no DRM panel or GPIO here."
            confirm "Carry on anyway?" || die "stopped"
            ;;
        armv7l|armv6l)
            die "32-bit userland. LP Frame is 64-bit only — reflash with the 64-bit image."
            ;;
        *) warn "unrecognised architecture $arch; continuing" ;;
    esac

    if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091
        OS_ID="$(. /etc/os-release && echo "${ID:-}")"
        OS_CODENAME="$(. /etc/os-release && echo "${VERSION_CODENAME:-}")"
        info "os: ${OS_ID:-unknown} ${OS_CODENAME:-}"
        case "$OS_CODENAME" in
            trixie) ok "the tested base" ;;
            bookworm) warn "bookworm is the documented fallback; trixie is what this is tested on" ;;
            "") ;;
            *) warn "untested release ${OS_CODENAME}; DESIGN §2.2 targets trixie" ;;
        esac
    fi

    if [[ -r /proc/device-tree/model ]]; then
        PI_MODEL="$(tr -d '\0' </proc/device-tree/model)"
        info "board: $PI_MODEL"
        case "$PI_MODEL" in
            *"Raspberry Pi 5"*) ok "primary target" ;;
            *"Raspberry Pi 4"*) ok "documented fallback" ;;
            *"Raspberry Pi"*) warn "untested Pi model; the RP1/BCM2711 GPIO paths may differ" ;;
        esac
    fi

    # Where config.txt lives moved in bookworm; both layouts are still out
    # there and picking the wrong one writes a file the firmware never reads.
    for candidate in /boot/firmware/config.txt /boot/config.txt; do
        if [[ -f "$candidate" ]]; then BOOT_CONFIG="$candidate"; break; fi
    done
    if [[ -n "$BOOT_CONFIG" ]]; then
        info "boot config: $BOOT_CONFIG"
    elif [[ $DO_BOOT_CONFIG -eq 1 ]]; then
        warn "no config.txt found; skipping the boot configuration step"
        DO_BOOT_CONFIG=0
    fi

    if [[ -n "$PANEL_MODE" && ! "$PANEL_MODE" =~ ^[0-9]+x[0-9]+@[0-9]+$ ]]; then
        die "--panel wants WIDTHxHEIGHT@RATE, e.g. 1920x1920@60 (got '$PANEL_MODE')"
    fi
}

# --- packages ---------------------------------------------------------------

APT_RUNTIME=(
    alsa-utils
    avahi-daemon
    ca-certificates
    libgpiod2
    # gpiodetect and gpioset — how docs/BUILD.md has you test the trigger
    # before trusting it with an amplifier.
    "gpiod|libgpiod-utils"
    # The 64-bit-time_t transition renamed this between bookworm and trixie,
    # and bookworm is a release this installer claims to support.
    "libasound2t64|libasound2"
)

# Only needed when building shairport-sync or LP Frame here.
APT_BUILD=(
    build-essential git pkg-config autoconf automake libtool
    libpopt-dev libconfig-dev libasound2-dev libavahi-client-dev
    libssl-dev libsoxr-dev libplist-dev libsodium-dev libgcrypt20-dev
    libavcodec-dev libavformat-dev libavutil-dev uuid-dev xxd
    libegl-dev libgles-dev libgbm-dev libdrm-dev
)

# Resolve "a|b|c" to the first name apt actually knows about.
#
# Alternatives rather than a release check, because the mapping is not
# one-to-one with the release: a device upgraded in place, or one on
# Raspberry Pi OS's own staging repositories, can have either name.
pick_pkg() {
    local choice
    for choice in ${1//|/ }; do
        if apt-cache show "$choice" >/dev/null 2>&1; then
            printf '%s' "$choice"
            return
        fi
    done
    # Nothing matched — hand back the first so the failure names something
    # real rather than a pipe-separated string nobody can apt-get.
    printf '%s' "${1%%|*}"
}

apt_install() {
    local want=() missing=() pkg
    for pkg in "$@"; do
        want+=("$(pick_pkg "$pkg")")
    done
    for pkg in "${want[@]}"; do
        if ! dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "ok installed"; then
            missing+=("$pkg")
        fi
    done
    if [[ ${#missing[@]} -eq 0 ]]; then
        skip "already present: ${want[*]}"
        return
    fi
    info "installing: ${missing[*]}"
    run env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
}

install_packages() {
    step "Installing packages"
    if ! command -v apt-get >/dev/null; then
        warn "no apt-get; install the dependencies listed in docs/BUILD.md by hand"
        return
    fi
    run apt-get update -qq
    apt_install "${APT_RUNTIME[@]}"
    if [[ $BUILD -eq 1 || $DO_SHAIRPORT -eq 1 ]]; then
        apt_install "${APT_BUILD[@]}"
    fi
}

# --- users and directories --------------------------------------------------

create_user() {
    step "Creating the service account"
    if id -u "$LPFRAME_USER" >/dev/null 2>&1; then
        skip "user $LPFRAME_USER exists"
    else
        info "creating system user $LPFRAME_USER"
        run useradd --system --no-create-home --home-dir "$STATE_DIR" \
            --shell /usr/sbin/nologin "$LPFRAME_USER"
    fi

    # video and render for the DRM nodes, audio for ALSA, gpio for the amp
    # trigger. The units name gpio, video and render as SupplementaryGroups=,
    # so a missing group there is a unit that will not start.
    for group in video render audio gpio; do
        if ! getent group "$group" >/dev/null; then
            info "creating group $group"
            run groupadd --system "$group"
        fi
        if id -nG "$LPFRAME_USER" 2>/dev/null | tr ' ' '\n' | grep -qx "$group"; then
            skip "$LPFRAME_USER already in $group"
        else
            info "adding $LPFRAME_USER to $group"
            run usermod -aG "$group" "$LPFRAME_USER"
        fi
    done
}

create_directories() {
    step "Creating directories"
    # /var/lib/lpframe holds the cache and config.local.toml, and must be
    # writable when the root filesystem is not — which is the whole reason
    # the configuration is split in two (DESIGN §7.1).
    for dir in "$CONF_DIR" "$SHARE_DIR"; do
        run install -d -m 0755 -o root -g root "$dir"
    done
    run install -d -m 0750 -o "$LPFRAME_USER" -g "$LPFRAME_USER" "$STATE_DIR"
    run install -d -m 0750 -o "$LPFRAME_USER" -g "$LPFRAME_USER" "$STATE_DIR/cache"
    ok "$CONF_DIR, $STATE_DIR, $SHARE_DIR"
}

# --- shairport-sync ---------------------------------------------------------

# True when a shairport-sync that already speaks AirPlay 2 is installed.
have_airplay2() {
    command -v shairport-sync >/dev/null 2>&1 &&
        shairport-sync -V 2>/dev/null | grep -q "AirPlay2"
}

build_from_git() {
    local name="$1" url="$2" version="$3"; shift 3
    local src="/usr/local/src/$name"

    info "building $name $version"
    if [[ -d "$src/.git" ]]; then
        run git -C "$src" fetch --tags --depth 1 origin "$version"
    else
        run install -d -m 0755 /usr/local/src
        run git clone --depth 1 --branch "$version" "$url" "$src"
    fi
    run git -C "$src" checkout -q "$version"
    run env -C "$src" autoreconf -fi
    run env -C "$src" ./configure "$@"
    run env -C "$src" make -j"$(nproc)"
    run env -C "$src" make install
}

install_shairport() {
    step "AirPlay 2 receiver"
    if [[ $DO_SHAIRPORT -eq 0 ]]; then
        skip "--skip-shairport"
        return
    fi
    if have_airplay2; then
        ok "shairport-sync already reports AirPlay2: $(shairport-sync -V 2>/dev/null | head -c 60)…"
        skip "nothing to build — DESIGN §2.3 says use the packaged one when it has AP2"
        return
    fi
    if command -v shairport-sync >/dev/null 2>&1; then
        warn "the installed shairport-sync has no AirPlay 2 support; building $SHAIRPORT_VERSION over it"
    fi

    # nqptp first: shairport-sync's configure does not need it, but a device
    # with the receiver and no timing daemon fails at the first connection
    # rather than at install time, which is a worse place to find out.
    build_from_git nqptp https://github.com/mikebrady/nqptp.git "$NQPTP_VERSION"
    build_from_git shairport-sync https://github.com/mikebrady/shairport-sync.git \
        "$SHAIRPORT_VERSION" \
        --sysconfdir=/etc --with-alsa --with-soxr --with-avahi --with-ssl=openssl \
        --with-systemd --with-airplay-2 --with-metadata --with-apple-alac

    if [[ $DRY_RUN -eq 0 ]] && ! have_airplay2; then
        die "built shairport-sync but it does not report AirPlay2 — check the configure output above"
    fi
    ok "shairport-sync $SHAIRPORT_VERSION and nqptp $NQPTP_VERSION"
}

configure_shairport() {
    step "Configuring shairport-sync"
    local conf="/etc/shairport-sync.conf"
    local name alsa
    name="$(config_value 'device.name' 'LP Frame')"
    alsa="$(config_value 'audio.alsa_device' 'default')"

    if [[ -f "$conf" ]] && grep -q "LP Frame installer" "$conf"; then
        skip "$conf already written by this installer"
        return
    fi
    backup_once "$conf"

    info "writing $conf"
    if [[ $DRY_RUN -eq 0 ]]; then
        cat >"$conf" <<EOF
// Written by the LP Frame installer. Edit freely; it is not rewritten unless
// this header line is removed.
general = {
    name = "$name";
    // Metadata is the entire point of this device. The pipe is the real
    // /tmp, shared with artd, which is why neither service has PrivateTmp.
};
metadata = {
    enabled = "yes";
    include_cover_art = "yes";
    pipe_name = "$(config_value 'device.metadata_pipe' '/tmp/shairport-sync-metadata')";
    pipe_timeout = 5000;
};
alsa = {
    output_device = "$alsa";
};
sessioncontrol = {
    // artd keys the amplifier off active/inactive rather than per-track
    // play events, so the default 10s hysteresis is what stops the relay
    // clicking between songs (DESIGN §5.1.1).
    active_state_timeout = 10.0;
};
EOF
    fi
    ok "$conf (AirPlay name: $name, ALSA device: $alsa)"
}

# Read a dotted key out of the shipped config. Not a TOML parser: it handles
# the flat `key = "value"` lines this one file actually contains, and falls
# back to the default for anything it cannot see.
config_value() {
    local key="$1" fallback="$2"
    local section="${key%.*}" leaf="${key##*.}"
    local file="$CONF_DIR/config.toml"
    [[ -f "$file" ]] || file="$REPO_ROOT/provisioning/config.toml"
    [[ -f "$file" ]] || { printf '%s' "$fallback"; return; }

    local found
    found="$(awk -v sect="[$section]" -v key="$leaf" '
        /^[[:space:]]*\[/ { in_section = ($0 ~ "^[[:space:]]*\\" sect); next }
        in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
            sub(/^[^=]*=[[:space:]]*/, ""); sub(/[[:space:]]*#.*$/, "")
            gsub(/^"|"$/, ""); print; exit
        }
    ' "$file")"
    printf '%s' "${found:-$fallback}"
}

# --- LP Frame itself --------------------------------------------------------

build_lpframe() {
    step "Building LP Frame"
    if [[ $DO_LPFRAME -eq 0 ]]; then
        skip "--skip-lpframe: the package supplied the binaries"
        return
    fi
    if [[ $BUILD -eq 0 ]]; then
        skip "--no-build"
        return
    fi
    command -v cargo >/dev/null 2>&1 ||
        die "cargo not found. Install Rust (https://rustup.rs) or use --no-build --binary-dir DIR."

    info "cargo build --release (this takes a while on a Pi — 10 minutes is normal)"
    run env -C "$REPO_ROOT" cargo build --release --workspace
    ok "built"
}

# Where the binaries ended up, for --no-build and for a cross-compiled tree.
resolve_binary_dir() {
    if [[ -n "$BINARY_DIR" ]]; then
        printf '%s' "$BINARY_DIR"
        return
    fi
    local candidates=(
        "$REPO_ROOT/target/release"
        "$REPO_ROOT/target/aarch64-unknown-linux-gnu/release"
    )
    for dir in "${candidates[@]}"; do
        if [[ -x "$dir/artd" ]]; then printf '%s' "$dir"; return; fi
    done
    printf '%s' "${candidates[0]}"
}

install_lpframe() {
    step "Installing LP Frame"
    if [[ $DO_LPFRAME -eq 0 ]]; then
        for binary in "${LPFRAME_BINARIES[@]}"; do
            [[ $DRY_RUN -eq 1 || -x "$BIN_DIR/$binary" ]] ||
                die "--skip-lpframe was given but $BIN_DIR/$binary is not there. \
Install the .deb first, or drop the flag."
        done
        skip "--skip-lpframe: ${LPFRAME_BINARIES[*]} already in $BIN_DIR"
        return
    fi
    local from; from="$(resolve_binary_dir)"
    info "from $from"

    for binary in "${LPFRAME_BINARIES[@]}"; do
        if [[ $DRY_RUN -eq 0 && ! -x "$from/$binary" ]]; then
            die "$from/$binary is missing. Build first, or pass --binary-dir."
        fi
        run install -m 0755 -o root -g root "$from/$binary" "$BIN_DIR/$binary"
    done
    ok "${LPFRAME_BINARIES[*]} → $BIN_DIR"

    # The one file that is never overwritten. It is the package's copy of the
    # defaults, but an operator may have edited it, and the web interface
    # writes to config.local.toml precisely so that this one can be replaced
    # — so replacing it silently would still be the wrong default.
    if [[ -f "$CONF_DIR/config.toml" ]]; then
        if cmp -s "$REPO_ROOT/provisioning/config.toml" "$CONF_DIR/config.toml"; then
            skip "config.toml unchanged"
        else
            run install -m 0644 "$REPO_ROOT/provisioning/config.toml" "$CONF_DIR/config.toml.new"
            warn "kept your $CONF_DIR/config.toml; the new one is beside it as config.toml.new"
            warn "diff them: diff -u $CONF_DIR/config.toml{,.new}"
        fi
    else
        run install -m 0644 "$REPO_ROOT/provisioning/config.toml" "$CONF_DIR/config.toml"
        ok "$CONF_DIR/config.toml"
    fi

    run install -m 0644 "$REPO_ROOT/provisioning/placeholder.png" "$SHARE_DIR/placeholder.png"
    run install -m 0755 "$REPO_ROOT/provisioning/lpframe-rw" "$LOCAL_BIN_DIR/lpframe-rw"
    run install -m 0755 "$REPO_ROOT/provisioning/lpframe-ro" "$LOCAL_BIN_DIR/lpframe-ro"
    ok "placeholder, lpframe-rw, lpframe-ro"
}

install_units() {
    step "Installing systemd units"
    for unit in "${LPFRAME_UNITS[@]}"; do
        run install -m 0644 "$REPO_ROOT/systemd/$unit" "$UNIT_DIR/$unit"
    done

    # The vendored shairport-sync and nqptp units assume /usr/local/bin,
    # which is where the from-source build puts them. When the distro package
    # supplied a working AirPlay 2 build instead, its own units are correct
    # and ours would point at binaries that are not there.
    if have_airplay2 && [[ ! -x /usr/local/bin/shairport-sync ]]; then
        skip "keeping the packaged shairport-sync and nqptp units"
    else
        for unit in "${VENDOR_UNITS[@]}"; do
            run install -m 0644 "$REPO_ROOT/systemd/$unit" "$UNIT_DIR/$unit"
        done
        info "installed our shairport-sync and nqptp units (they expect /usr/local/bin)"
    fi

    run install -d -m 0755 "$TMPFILES_DIR"
    run install -m 0644 "$REPO_ROOT/provisioning/lpframe.tmpfiles.conf" \
        "$TMPFILES_DIR/lpframe.conf"

    run systemctl daemon-reload
    run systemd-tmpfiles --create "$TMPFILES_DIR/lpframe.conf"
    ok "units and tmpfiles installed"
}

# --- boot configuration -----------------------------------------------------

configure_boot() {
    step "Boot configuration"
    if [[ $DO_BOOT_CONFIG -eq 0 ]]; then
        skip "--skip-boot-config"
        return
    fi

    backup_once "$BOOT_CONFIG"

    # The full KMS driver, with CMA sized for scanout. GBM allocates the
    # scanout buffers out of CMA, and 1920×1920 double-buffered at 32bpp is
    # 29 MiB before any decode — the Pi 4's default of 64 MiB is tight and
    # the failure mode is an allocation error at the first page flip.
    #
    # Pi OS Lite already ships a bare `dtoverlay=vc4-kms-v3d`, so this
    # replaces that line rather than appending: two dtoverlay lines for the
    # same overlay is not a longer version of one.
    replace_overlay "vc4-kms-v3d" "dtoverlay=vc4-kms-v3d,cma-256"

    if [[ -n "$PANEL_MODE" ]]; then
        add_custom_mode "$PANEL_MODE"
    else
        info "no --panel given; the panel's own EDID decides the mode"
        info "run 'lprender --probe' after rebooting to see what it advertised"
    fi
    ok "$BOOT_CONFIG"

    # journald on an SD card is a wear problem and a disk-full problem, and
    # the device has no logs worth keeping across a reboot anyway (§8.3).
    local drop="/etc/systemd/journald.conf.d/lpframe.conf"
    run install -d -m 0755 /etc/systemd/journald.conf.d
    if [[ -f "$drop" ]]; then
        skip "journald already configured"
    else
        info "writing $drop (volatile storage, 32M cap)"
        if [[ $DRY_RUN -eq 0 ]]; then
            cat >"$drop" <<'EOF'
# LP Frame: logs live in RAM. An appliance on an SD card should not be
# writing a journal to it, and nothing here is worth reading after a reboot
# (DESIGN §8.3). `journalctl -f` still works exactly as usual.
[Journal]
Storage=volatile
RuntimeMaxUse=32M
EOF
        fi
    fi

    # getty on tty1 fights lprender for DRM master. Masking it is why the
    # renderer's unit can be After=multi-user.target and simply take the
    # card (DESIGN §6.2).
    if systemctl is-enabled getty@tty1.service >/dev/null 2>&1; then
        info "masking getty@tty1 so it cannot hold DRM master"
        run systemctl mask getty@tty1.service
    else
        skip "getty@tty1 already masked or absent"
    fi
}

# Replace a `dtoverlay=<name>...` line, or append it if there is none.
#
# config.txt is append-only in most people's hands, and an overlay listed
# twice is loaded twice. Editing in place keeps the file something a human
# can still read after several installer runs.
replace_overlay() {
    local overlay="$1" line="$2"
    if [[ -f "$BOOT_CONFIG" ]] && grep -qxF -- "$line" "$BOOT_CONFIG"; then
        skip "already set: $line"
        return
    fi
    if [[ -f "$BOOT_CONFIG" ]] && grep -qE "^[[:space:]]*dtoverlay=$overlay(,|$)" "$BOOT_CONFIG"; then
        info "replacing the existing $overlay overlay line with: $line"
        run sed -i -E "s|^[[:space:]]*dtoverlay=$overlay(,.*)?$|$line|" "$BOOT_CONFIG"
        return
    fi
    ensure_line "$line" "$BOOT_CONFIG"
}

# Force a mode for a panel whose EDID does not advertise its native one —
# the common case for the eDP driver boards square panels come on.
#
# This goes in cmdline.txt as a kernel `video=` parameter, not in config.txt
# as `hdmi_timings`. The legacy `hdmi_*` settings are firmware-level: with
# the full KMS driver the kernel owns modesetting, and on the Pi 5 the
# firmware ignores them outright. `video=` is what both boards actually obey.
#
# `M` asks the kernel to compute CVT timings for the given size, which is
# what these driver boards expect, and `D` forces digital output so a panel
# that reports no EDID at all still gets driven.
add_custom_mode() {
    local spec="$1"
    local w="${spec%%x*}" rest="${spec#*x}"
    local h="${rest%%@*}" rate="${rest##*@}"

    local cmdline; cmdline="$(dirname "$BOOT_CONFIG")/cmdline.txt"
    if [[ ! -f "$cmdline" ]]; then
        warn "no $cmdline; cannot force a mode. Set display.mode in config.toml instead."
        return
    fi

    local connector; connector="$(config_value 'display.connector' 'auto')"
    [[ "$connector" == "auto" ]] && connector="HDMI-A-1"
    local param="video=$connector:${w}x${h}M@${rate}D"

    info "forcing $connector to ${w}×${h}@${rate}Hz via $cmdline"
    backup_once "$cmdline"

    if grep -qF -- "$param" "$cmdline"; then
        skip "already set: $param"
    elif grep -qE "video=$connector:" "$cmdline"; then
        info "replacing the existing video= for $connector"
        run sed -i -E "s|video=$connector:[^[:space:]]*|$param|" "$cmdline"
    else
        # cmdline.txt is one line, and a newline in it silently truncates
        # every parameter after the break.
        info "appending $param"
        [[ $DRY_RUN -eq 0 ]] && sed -i "1s|\$| $param|" "$cmdline"
    fi

    warn "a forced mode a panel cannot do gives a dark screen with no error."
    warn "If that happens, put $cmdline.pre-lpframe back from another machine —"
    warn "the boot partition is FAT and mounts anywhere."
}

# --- services ---------------------------------------------------------------

enable_services() {
    step "Enabling services"
    if [[ $DO_ENABLE -eq 0 ]]; then
        skip "--no-enable"
        return
    fi
    run systemctl enable --now avahi-daemon.service
    for unit in nqptp.service lpframe-artd.service shairport-sync.service lpframe-lprender.service; do
        run systemctl enable "$unit"
    done
    # Started in dependency order by systemd, but named here in the order
    # they matter so a failure reads sensibly in the log.
    run systemctl restart lpframe-artd.service
    run systemctl restart nqptp.service shairport-sync.service
    run systemctl restart lpframe-lprender.service
    ok "enabled and started"
}

verify() {
    step "Checking it came up"
    if [[ $DRY_RUN -eq 1 ]]; then
        skip "dry run"
        return
    fi

    local failed=0
    for unit in nqptp.service lpframe-artd.service shairport-sync.service lpframe-lprender.service; do
        if systemctl is-active --quiet "$unit"; then
            ok "$unit"
        else
            warn "$unit is not running: journalctl -u $unit -n 30 --no-pager"
            failed=1
        fi
    done

    if [[ -x "$BIN_DIR/artd" ]]; then
        if "$BIN_DIR/artd" --config "$CONF_DIR/config.toml" \
            --config-local "$STATE_DIR/config.local.toml" --check-config >/dev/null 2>&1; then
            ok "configuration is valid"
        else
            warn "artd rejects the configuration:"
            "$BIN_DIR/artd" --config "$CONF_DIR/config.toml" \
                --config-local "$STATE_DIR/config.local.toml" --check-config || true
            failed=1
        fi
    fi

    local password="$STATE_DIR/web-password.txt"
    if [[ -f "$password" ]]; then
        local bind; bind="$(config_value 'web.bind' '0.0.0.0:8730')"
        printf '\n    %sweb interface%s  http://%s.local:%s/\n' \
            "$BOLD" "$RESET" "$(hostname)" "${bind##*:}"
        printf '    %spassword%s       %s\n' "$BOLD" "$RESET" "$(cat "$password")"
        printf '    %s(also: lpctl web-password)%s\n' "$DIM" "$RESET"
    fi

    return $failed
}

# --- uninstall --------------------------------------------------------------

uninstall() {
    step "Removing LP Frame"
    warn "this leaves $STATE_DIR alone — your settings, password hash and artwork cache"
    confirm "Remove the services, binaries and units?" || die "stopped"

    for unit in lpframe-lprender.service shairport-sync.service lpframe-artd.service nqptp.service; do
        run systemctl disable --now "$unit" 2>/dev/null || true
    done
    for unit in "${LPFRAME_UNITS[@]}" "${VENDOR_UNITS[@]}"; do
        run rm -f "$UNIT_DIR/$unit"
    done
    run rm -f "$TMPFILES_DIR/lpframe.conf"
    run systemctl daemon-reload

    for binary in "${LPFRAME_BINARIES[@]}"; do
        run rm -f "$BIN_DIR/$binary"
    done
    run rm -f "$LOCAL_BIN_DIR/lpframe-rw" "$LOCAL_BIN_DIR/lpframe-ro"
    run rm -rf "$SHARE_DIR"

    info "left in place: $CONF_DIR, $STATE_DIR, $BOOT_CONFIG, and getty@tty1's mask"
    info "to finish: sudo rm -rf $CONF_DIR $STATE_DIR && sudo systemctl unmask getty@tty1"
    ok "removed"
}

# --- go ---------------------------------------------------------------------

main() {
    printf '%sLP Frame installer%s' "$BOLD" "$RESET"
    [[ $DRY_RUN -eq 1 ]] && printf ' %s(dry run — nothing will change)%s' "$DIM" "$RESET"
    printf '\n'

    preflight

    if [[ $UNINSTALL -eq 1 ]]; then
        uninstall
        return 0
    fi

    install_packages
    create_user
    create_directories
    install_shairport
    build_lpframe
    install_lpframe
    configure_shairport
    install_units
    configure_boot
    enable_services

    local status=0
    verify || status=$?

    printf '\n%sDone.%s\n' "$BOLD" "$RESET"
    if [[ $DO_BOOT_CONFIG -eq 1 && $DRY_RUN -eq 0 ]]; then
        printf '    %sReboot to pick up the boot configuration: sudo reboot%s\n' "$DIM" "$RESET"
    fi
    printf '    %sMaking the root filesystem read-only is a separate, later step —%s\n' "$DIM" "$RESET"
    printf '    %ssee "Locking it down" in docs/BUILD.md.%s\n' "$DIM" "$RESET"
    return $status
}

main
