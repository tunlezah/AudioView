#!/usr/bin/env bash
#
# Append a third partition to a built image, for /var/lib/lpframe.
#
# The image needs somewhere writable that survives a read-only root, or the
# device forgets its settings, its generated web password and its artwork
# cache at every power cycle (DESIGN §8.3). On a hand-built device
# make-writable-partition.sh carves that out of free space on the card; an
# image can simply ship it.
#
# This only ever operates on a file. It never touches a block device, it
# never needs a loop device, and it never needs root: `mke2fs -d` builds a
# populated filesystem image directly, and sfdisk edits a partition table in
# a regular file quite happily. That means it can be tested for real rather
# than reasoned about, which for anything that writes a partition table is
# the difference between confidence and hope.
#
# The filesystem is left empty apart from its label. `StateDirectory=lpframe`
# in lpframe-artd.service sets the ownership when the daemon first starts, so
# there is nothing here that has to know what uid the image gave lpframe.
#
#   ./add-data-partition.sh path/to/image.img [--size MiB]

set -euo pipefail

SIZE_MIB=2048
LABEL="lpframe"
IMAGE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --size) SIZE_MIB="${2:?--size needs MiB}"; shift ;;
        --label) LABEL="${2:?--label needs a name}"; shift ;;
        -h|--help)
            echo "Usage: $0 IMAGE.img [--size MiB] [--label NAME]"
            exit 0 ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) IMAGE="$1" ;;
    esac
    shift
done

[[ -n "$IMAGE" ]] || { echo "Usage: $0 IMAGE.img [--size MiB]" >&2; exit 2; }
[[ -f "$IMAGE" ]] || { echo "no such image: $IMAGE" >&2; exit 1; }

# mke2fs and sfdisk live in /sbin, off a non-root PATH on Debian.
PATH="/sbin:/usr/sbin:$PATH"
for tool in sfdisk mke2fs truncate; do
    command -v "$tool" >/dev/null || { echo "$tool is missing" >&2; exit 1; }
done

say() { printf '    %s\n' "$*"; }

# --- refuse to do it twice --------------------------------------------------

existing="$(sfdisk --json "$IMAGE" | grep -c '"node"' || true)"
if [[ "$existing" -ge 3 ]]; then
    say "$IMAGE already has $existing partitions; leaving it alone"
    exit 0
fi
if [[ "$existing" -ne 2 ]]; then
    echo "expected the 2-partition layout pi-gen produces, found $existing" >&2
    exit 1
fi

before_bytes="$(stat -c %s "$IMAGE")"
say "image:  $IMAGE ($((before_bytes / 1024 / 1024)) MiB, $existing partitions)"

# --- make room --------------------------------------------------------------

# sfdisk --append needs the space to exist in the file before it will place a
# partition there; a partition table pointing past the end of its own image
# is not something to hand anybody.
truncate -s "+${SIZE_MIB}M" "$IMAGE"
say "grew the file by ${SIZE_MIB} MiB"

# --- the filesystem ---------------------------------------------------------

# Built separately and copied in, rather than mkfs'd through a loop device:
# no privileges, and the result is byte-for-byte reproducible.
FS="$(mktemp --suffix=.ext4)"
trap 'rm -f "$FS"' EXIT
truncate -s "${SIZE_MIB}M" "$FS"

# -m 0: no reserved blocks. Five percent held back for root is a hundred
# megabytes of album art withheld for no reason on a partition that holds
# one application's cache.
mke2fs -q -t ext4 -m 0 -L "$LABEL" -F "$FS" >/dev/null
say "made an empty ext4 labelled '$LABEL'"

# --- the partition ----------------------------------------------------------

# An empty start field means "the next free extent", so the two existing
# entries are read and not rewritten.
printf ',%sM,L\n' "$SIZE_MIB" | sfdisk --append --no-reread --no-tell-kernel "$IMAGE" >/dev/null
say "appended partition 3"

start_sector="$(sfdisk --json "$IMAGE" | awk '/"start"/ { gsub(/[^0-9]/, "", $2); v = $2 } END { print v }')"
sector_bytes="$(sfdisk --json "$IMAGE" | awk -F'[: ,]+' '/"sectorsize"/ { print $3 }')"
: "${sector_bytes:=512}"
[[ "$start_sector" =~ ^[0-9]+$ ]] || { echo "could not read the new partition's start" >&2; exit 1; }

dd if="$FS" of="$IMAGE" bs="$sector_bytes" seek="$start_sector" conv=notrunc,sparse status=none
say "wrote the filesystem at sector $start_sector"

# --- say what we produced ---------------------------------------------------

echo
sfdisk -l "$IMAGE" 2>/dev/null | sed 's/^/    /'
echo
say "$(( $(stat -c %s "$IMAGE") / 1024 / 1024 )) MiB total"
