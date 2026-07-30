#!/usr/bin/env bash
#
# Give /var/lib/lpframe a partition of its own.
#
# This is what makes the overlay root survivable. Under the overlay the root
# filesystem's writes are discarded at power off, so without a partition here
# the device forgets its settings, its generated web password and its whole
# artwork cache at every power cycle (DESIGN §8.3). It is not optional once
# the web interface exists.
#
# It writes a partition table, so:
#   * it refuses to touch anything but free space at the end of the disk
#   * it never shrinks a filesystem — that cannot be done to a mounted root,
#     and pretending otherwise is how people lose cards. If there is no free
#     space, this prints the offline recipe and stops.
#   * it asks before each irreversible step, unless --yes
#
# Run it BEFORE `lpframe-ro`. `lpframe-ro` refuses to enable the overlay
# until this has been done.

set -euo pipefail

STATE_DIR="/var/lib/lpframe"
SIZE_MIB="${SIZE_MIB:-2048}"
LABEL="lpframe"
ASSUME_YES=0
DRY_RUN=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes|-y) ASSUME_YES=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --size) SIZE_MIB="${2:?--size needs MiB}"; shift ;;
        -h|--help)
            sed -n '2,22p' "$0" | sed 's/^# \?//'
            echo
            echo "Usage: sudo $0 [--size MiB] [--yes] [--dry-run]"
            exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ -t 1 ]]; then
    BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; RESET=$'\033[0m'
else
    BOLD=""; DIM=""; RED=""; RESET=""
fi

info() { printf '    %s\n' "$*"; }
step() { printf '\n%s%s%s\n' "$BOLD" "$*" "$RESET"; }
die()  { printf '\n%serror: %s%s\n' "$RED" "$*" "$RESET" >&2; exit 1; }
run()  {
    if [[ $DRY_RUN -eq 1 ]]; then printf '    %s$ %s%s\n' "$DIM" "$*" "$RESET"
    else "$@"; fi
}
confirm() {
    [[ $ASSUME_YES -eq 1 || $DRY_RUN -eq 1 ]] && return 0
    local answer; read -r -p "    $1 [y/N] " answer
    [[ "$answer" =~ ^[Yy] ]]
}

# sfdisk, partx and mkfs.ext4 live in /sbin, which Debian does not put on a
# non-root PATH — so without this, `--dry-run` as an ordinary user reports
# every tool missing and tells you to install packages you already have.
PATH="/sbin:/usr/sbin:$PATH"

[[ $DRY_RUN -eq 1 || $EUID -eq 0 ]] || die "run this with sudo"
for tool in sfdisk partx mkfs.ext4 findmnt lsblk; do
    command -v "$tool" >/dev/null || die "$tool is missing (apt install util-linux e2fsprogs)"
done

# --- where are we -----------------------------------------------------------

step "Looking at the disk"

root_source="$(findmnt -no SOURCE /)"
# /dev/mmcblk0p2 -> /dev/mmcblk0 ; /dev/sda2 -> /dev/sda
disk="/dev/$(lsblk -no PKNAME "$root_source")"
[[ -b "$disk" ]] || die "could not work out which disk / is on (got '$disk')"
info "root:  $root_source"
info "disk:  $disk"

existing="$(findmnt -no TARGET --target "$STATE_DIR" 2>/dev/null || echo /)"
if [[ "$existing" == "$STATE_DIR" ]]; then
    info "$STATE_DIR is already its own mount — nothing to do"
    findmnt --target "$STATE_DIR"
    exit 0
fi

# The size of the last free extent, in sectors.
#
# `sfdisk --list-free` prints a banner, a units line, a "Start End Sectors
# Size" header and then the extents. Selecting rows by leading whitespace
# picks the *header*, whose third field is the word "Sectors" — which reads
# as zero free space and makes this script refuse on every disk. Select by
# "the first and third fields are numbers" instead.
sector_bytes="$(cat "/sys/block/$(basename "$disk")/queue/logical_block_size" 2>/dev/null || echo 512)"
free_sectors="$(sfdisk --list-free "$disk" 2>/dev/null | awk '
    $1 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { last = $3 }
    END { print last + 0 }
')"
[[ "$free_sectors" =~ ^[0-9]+$ ]] || free_sectors=0
free_mib=$(( free_sectors * sector_bytes / 1024 / 1024 ))
info "free:  ${free_mib} MiB unallocated at the end of the disk"

if (( free_mib < SIZE_MIB )); then
    cat >&2 <<EOF

${RED}There is not enough unallocated space (${free_mib} MiB free, ${SIZE_MIB} MiB wanted).${RESET}

Raspberry Pi OS expands the root partition to fill the card on first boot,
which leaves nothing at the end. Shrinking it back cannot be done while it is
mounted, and this script will not try.

Two ways forward. The first is much easier if you have not deployed yet:

${BOLD}Reflash, and stop the auto-expand.${RESET}
  Flash the image, then before the first boot, on the machine that flashed
  it, edit the FAT boot partition's cmdline.txt and delete this fragment:

      init=/usr/lib/raspberrypi-sys-mods/firstboot

  The root stays at its image size and the rest of the card is left free.
  Boot, run install.sh, then run this script.

${BOLD}Or shrink it offline, from another Linux machine.${RESET}
  Power the Pi down, put the card in another machine, and with the card
  ${BOLD}not mounted${RESET} (replace /dev/sdX with the card, and check twice):

      sudo e2fsck -f /dev/sdX2
      sudo resize2fs /dev/sdX2 6G
      sudo parted /dev/sdX resizepart 2 7GiB

  Then put the card back and run this script on the Pi.

Nothing has been changed.
EOF
    exit 1
fi

# --- do it ------------------------------------------------------------------

step "Plan"
info "create a ${SIZE_MIB} MiB ext4 partition labelled '$LABEL' in the free space"
info "move the current contents of $STATE_DIR onto it"
info "mount it at $STATE_DIR via /etc/fstab, by label"
printf '\n    %sThis edits the partition table on %s.%s\n' "$BOLD" "$disk" "$RESET"
printf '    %sIt only appends to free space, but back the card up first if it matters.%s\n' \
    "$DIM" "$RESET"
confirm "Go ahead?" || die "stopped; nothing changed"

step "Creating the partition"
# `sfdisk --append` with an empty start takes the next free extent, so the
# existing entries are read but never rewritten.
sfdisk_spec=",${SIZE_MIB}M,L"
if [[ $DRY_RUN -eq 1 ]]; then
    printf '    %s$ echo "%s" | sfdisk --append %s%s\n' "$DIM" "$sfdisk_spec" "$disk" "$RESET"
else
    printf '%s\n' "$sfdisk_spec" | sfdisk --append "$disk"
fi
run partx -u "$disk"
run udevadm settle

new_part="$(lsblk -lnpo NAME "$disk" | tail -1)"
[[ $DRY_RUN -eq 1 ]] && new_part="${disk}pN"
info "new partition: $new_part"

step "Making a filesystem"
# No reserved blocks: this partition holds one application's cache, and 5%
# held back for root is 100 MiB of album art for no purpose here.
run mkfs.ext4 -q -m 0 -L "$LABEL" "$new_part"

step "Moving $STATE_DIR onto it"
run systemctl stop lpframe-artd.service 2>/dev/null || true
run install -d -m 0755 /mnt/lpframe-new
run mount "$new_part" /mnt/lpframe-new
if [[ -d "$STATE_DIR" ]] && [[ -n "$(ls -A "$STATE_DIR" 2>/dev/null || true)" ]]; then
    info "copying $(du -sh "$STATE_DIR" 2>/dev/null | cut -f1) of existing state"
    run cp -a "$STATE_DIR/." /mnt/lpframe-new/
else
    info "$STATE_DIR is empty; nothing to copy"
fi
run chown lpframe:lpframe /mnt/lpframe-new
run chmod 0750 /mnt/lpframe-new
run umount /mnt/lpframe-new
run rmdir /mnt/lpframe-new

step "Mounting it at boot"
run install -d -m 0750 "$STATE_DIR"
fstab_line="LABEL=$LABEL  $STATE_DIR  ext4  defaults,noatime,nofail  0  2"
if grep -q "[[:space:]]${STATE_DIR}[[:space:]]" /etc/fstab 2>/dev/null; then
    info "fstab already has an entry for $STATE_DIR; leaving it alone"
else
    info "adding to /etc/fstab: $fstab_line"
    # `nofail` on purpose: a device that will not boot because a cache
    # partition is missing is a worse failure than one that boots without its
    # settings and says so in the log.
    [[ $DRY_RUN -eq 0 ]] && printf '%s\n' "$fstab_line" >>/etc/fstab
fi
run systemctl daemon-reload
run mount "$STATE_DIR"
run systemctl start lpframe-artd.service 2>/dev/null || true

step "Done"
if [[ $DRY_RUN -eq 0 ]]; then
    findmnt --target "$STATE_DIR" || true
    df -h "$STATE_DIR" | tail -1
fi
cat <<EOF

    $STATE_DIR is now on its own partition and survives an overlay root.

    Next:  sudo lpframe-ro && sudo reboot
EOF
