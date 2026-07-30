#!/bin/bash -e
#
# Host side: stage everything the chroot script needs.
#
# pi-gen runs this with ROOTFS_DIR pointing at the image's root. Nothing is
# emulated here, so this is the cheap half — copying. The expensive half,
# which runs under qemu, is 01-run-chroot.sh.

STAGING="${ROOTFS_DIR}/tmp/lpframe-build"

install -d "${STAGING}"

# The package, cross-built on the host by build-image.sh. Building the Rust
# workspace inside the emulated chroot instead would turn a one-minute
# cross-compile into the longest step in the image build by a wide margin.
install -m 0644 files/lpframe.deb "${STAGING}/lpframe.deb"

# The installer and the units, so the image is built by running the same
# script a person runs by hand. If install.sh has a bug, the image has that
# bug too — which is the correct coupling. The alternative is a second,
# untested description of how to set up a device.
cp -a files/provisioning "${STAGING}/provisioning"
cp -a files/systemd "${STAGING}/systemd"

# A note on the FAT partition, which is the one place a locked-out user can
# still read from any machine that can flash a card.
install -m 0644 files/README-FIRST.txt "${ROOTFS_DIR}/boot/firmware/lpframe-README.txt"
