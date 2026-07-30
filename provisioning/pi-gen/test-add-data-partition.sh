#!/usr/bin/env bash
#
# Test add-data-partition.sh against a real partition table.
#
# It is the one thing in this repository that writes a partition table, so it
# gets a test that actually writes one rather than an argument that it would
# work. Everything here operates on sparse files and needs no root and no
# loop device, which is the whole reason add-data-partition.sh was written to
# use `mke2fs -d` and `sfdisk` on a file.
#
# Run: ./provisioning/pi-gen/test-add-data-partition.sh   (part of `make provisioning-check`)

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/add-data-partition.sh"

PATH="/sbin:/usr/sbin:$PATH"
for tool in sfdisk mke2fs dumpe2fs parted; do
    command -v "$tool" >/dev/null || { echo "SKIP: $tool is not installed"; exit 0; }
done

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0
check() {
    if [[ "$2" == "$3" ]]; then
        printf '    ok   %s\n' "$1"
        pass=$((pass + 1))
    else
        printf '    FAIL %s\n         expected: %s\n         got:      %s\n' "$1" "$3" "$2" >&2
        exit 1
    fi
}

# --- a pi-gen-shaped image, with a populated root -----------------------------
#
# The shape export-image produces — 8MiB alignment, a FAT boot partition,
# then ext4 root — at a fraction of the size. Only the geometry is under
# test, and a full-size fixture spends two minutes hashing zeroes.
#
# Expectations are derived from these numbers rather than written out, so
# changing the fixture cannot quietly turn an assertion into a tautology.

MIB=$((1024 * 1024))
SECT=512
BOOT_START=$((8 * MIB / SECT))
BOOT_SECTORS=$((32 * MIB / SECT))
ROOT_START=$((BOOT_START + BOOT_SECTORS))
ROOT_SECTORS=$((128 * MIB / SECT))
DATA_MIB=32

IMG="$WORK/test.img"
truncate -s $(((ROOT_START + ROOT_SECTORS) * SECT)) "$IMG"
parted --script "$IMG" mklabel msdos
parted --script "$IMG" unit s mkpart primary fat32 "$BOOT_START" "$((ROOT_START - 1))"
parted --script "$IMG" unit s mkpart primary ext4 "$ROOT_START" "$((ROOT_START + ROOT_SECTORS - 1))"

mkdir -p "$WORK/rootsrc/etc"
echo "if this changes, the script wrote outside its own partition" >"$WORK/rootsrc/etc/canary"
truncate -s $((ROOT_SECTORS * SECT)) "$WORK/root.ext4"
mke2fs -q -t ext4 -L rootfs -d "$WORK/rootsrc" -F "$WORK/root.ext4"
dd if="$WORK/root.ext4" of="$IMG" bs=$SECT seek="$ROOT_START" conv=notrunc,sparse status=none

root_digest() {
    dd if="$IMG" bs=$SECT skip="$ROOT_START" count="$ROOT_SECTORS" status=none |
        sha256sum | cut -d' ' -f1
}
root_before="$(root_digest)"

echo "  add-data-partition.sh"
"$SCRIPT" "$IMG" --size "$DATA_MIB" >/dev/null

# --- what came out ------------------------------------------------------------

json="$(sfdisk --json "$IMG")"
check "three partitions" "$(grep -c '"node"' <<<"$json")" "3"

# Exactly where root ends. Anywhere later wastes the gap; anywhere earlier
# means the arithmetic is wrong in the direction that destroys data.
start="$(awk '/"start"/ { gsub(/[^0-9]/, "", $2); v = $2 } END { print v }' <<<"$json")"
check "partition 3 starts where root ends" "$start" "$((ROOT_START + ROOT_SECTORS))"

size="$(awk '/"size"/ { gsub(/[^0-9]/, "", $2); v = $2 } END { print v }' <<<"$json")"
check "partition 3 is the requested size" "$size" "$((DATA_MIB * MIB / SECT))"

# The filesystem, read straight back out of the image at that offset — which
# also proves the offset the script wrote to is the offset the table claims.
dd if="$IMG" bs=$SECT skip="$start" count="$size" of="$WORK/data.ext4" status=none
header() { dumpe2fs -h "$WORK/data.ext4" 2>/dev/null | awk -F':[[:space:]]*' "/$1/ { print \$2 }"; }

check "labelled lpframe" "$(header 'Filesystem volume name')" "lpframe"
check "filesystem is clean" "$(header 'Filesystem state')" "clean"
# Five percent of a cache partition held back for root is a hundred megabytes
# of album art withheld for nothing.
check "no reserved blocks" "$(header 'Reserved block count')" "0"

# The property that matters most: appending must not disturb what is already
# there. A partition-table script that is even slightly wrong about offsets
# corrupts the root filesystem, and the symptom is a card that will not boot.
check "root partition byte-identical" "$(root_digest)" "$root_before"

# --- idempotency ---------------------------------------------------------------

before="$(sha256sum "$IMG" | cut -d' ' -f1)"
"$SCRIPT" "$IMG" --size "$DATA_MIB" >/dev/null
check "second run changes nothing" "$(sha256sum "$IMG" | cut -d' ' -f1)" "$before"

# --- it refuses what it does not understand -------------------------------------

ODD="$WORK/odd.img"
truncate -s 64M "$ODD"
parted --script "$ODD" mklabel msdos
parted --script "$ODD" unit s mkpart primary ext4 2048 65535
if "$SCRIPT" "$ODD" --size 8 >/dev/null 2>&1; then
    echo "    FAIL accepted a layout it was not written for" >&2
    exit 1
fi
printf '    ok   refuses a layout it does not recognise\n'
pass=$((pass + 1))

printf '\n  %d checks passed\n' "$pass"
