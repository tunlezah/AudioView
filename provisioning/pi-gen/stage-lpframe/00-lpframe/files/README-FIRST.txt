LP Frame
========

This card holds a Raspberry Pi OS Lite image with LP Frame already installed.
You are reading this from the small FAT partition, which mounts on any
machine — which is also where to come back to if the device stops talking to
you.

BEFORE THE FIRST BOOT — this is not optional
--------------------------------------------

The image has NO user account, NO password and SSH is OFF. That is
deliberate: an appliance image with a default login is a device on somebody's
network with a password everyone knows.

Set them with Raspberry Pi Imager's customisation (the gear icon) when you
write the card: hostname, username and password, SSH, and Wi-Fi if you are
not using Ethernet.

If you already wrote the card without doing that, you do not have to start
again. Create two files here, on this partition:

  ssh
      Empty. Its presence turns SSH on.

  userconf.txt
      One line:  username:encrypted-password
      Generate the second half on another machine with:
          echo 'yourpassword' | openssl passwd -6 -stdin

There is no login prompt on the screen. The renderer owns the display, so
the text console is switched off — the screen shows album art and nothing
else, by design.

Once it boots
-------------

  http://<hostname>.local:8730/

The web interface password is generated on this device at first boot and
printed once in the log. To read it back:

  sudo lpctl web-password

Every device generates its own. Nothing secret is baked into this image.

The rest
--------

Full guide, including the amplifier trigger wiring and what to check when
something does not work:

  /usr/share/doc/lpframe/BUILD.md   (on the device)
  https://github.com/tunlezah/AudioView/blob/main/docs/BUILD.md

Partitions on this card:

  1  FAT    /boot/firmware      this partition
  2  ext4   /                   the system
  3  ext4   /var/lib/lpframe    settings, web password, artwork cache

The root partition does not expand to fill the card, because partition 3 sits
immediately after it. Space beyond partition 3 is unused; that is the cost of
having a writable partition that survives a read-only root.
