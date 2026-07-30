#!/bin/bash -e
#
# Inside the image, under qemu. This is the slow part: shairport-sync and
# nqptp are compiled from source here, because Debian's shairport-sync is
# built without AirPlay 2 (trixie ships 4.3.7-1 with no libplist, libsodium,
# libgcrypt or ffmpeg among its dependencies, and no relationship to nqptp).
#
# Note that the version number matches ours exactly, so `shairport-sync -V`
# reporting 4.3.7 says nothing about whether AirPlay 2 is in it. install.sh
# checks for the AirPlay2 marker rather than the version, which is why it
# does the right thing on a device where the distro package is already
# installed.

STAGING=/tmp/lpframe-build

export DEBIAN_FRONTEND=noninteractive

apt-get -o Acquire::Retries=3 install -y "${STAGING}/lpframe.deb"

# --skip-lpframe: the package above supplied the binaries, config, units and
#   placeholder, so the installer does the rest.
# --yes: nothing here can answer a prompt.
#
# There is no running systemd in a chroot. install.sh detects that and
# enables the units without starting them — `systemctl enable` is a
# filesystem operation and works offline, which is exactly what an image
# wants.
"${STAGING}/provisioning/install.sh" --skip-lpframe --yes

# Stop the root partition expanding to fill the card on first boot.
#
# The image ships a third partition for /var/lib/lpframe immediately after
# root, so an expand would run straight into it. The mechanism is one bare
# word in cmdline.txt: resize_early in the initramfs starts with
# `grep -q ' resize' /proc/cmdline` and exits if it is absent.
#
# (It would in fact refuse anyway — it only resizes when root is partition 2
# and nothing follows it — but relying on someone else's safety check to
# protect our data partition is not a plan.)
sed -i 's/ resize\b//g' /boot/firmware/cmdline.txt
grep -q ' resize' /boot/firmware/cmdline.txt && {
	echo "cmdline.txt still asks for a resize; refusing to ship this image" >&2
	exit 1
}

# Mount the data partition by label. make-writable-partition.sh writes the
# same line on a hand-built device; here the filesystem is created by
# add-data-partition.sh after pi-gen has finished, and systemd's
# StateDirectory=lpframe fixes the ownership when artd first starts.
#
# nofail, deliberately: a card whose third partition did not survive being
# written should boot and complain, not drop to an emergency shell on a
# device with no keyboard.
if ! grep -q '/var/lib/lpframe' /etc/fstab; then
	echo 'LABEL=lpframe  /var/lib/lpframe  ext4  defaults,noatime,nofail  0  2' >>/etc/fstab
fi

# Nothing of ours should have run yet. artd generates the web password on
# first start, and an image carrying one would give every device flashed
# from it the same credentials.
if [ -e /var/lib/lpframe/web-password.txt ] || [ -s /var/lib/lpframe/config.local.toml ]; then
	echo "the build generated device state; that must not be baked into an image" >&2
	exit 1
fi

apt-get clean
rm -rf "${STAGING}"
