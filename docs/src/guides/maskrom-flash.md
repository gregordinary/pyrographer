# Flashing a disk image from maskrom

A whole-disk image, such as an OpenWrt release for one board, holds the boot chain and the
partitions together, laid out from sector 0. This guide writes one to a Rockchip board's eMMC
from maskrom. `db` brings the board to loader mode, and `flash` plans the write, writes it and
reads it back.

Two steps in this guide write to storage: [Writing the image](#writing-the-image) and
[Clearing a stale backup GPT](#clearing-a-stale-backup-gpt). Each permanently replaces the data
it covers. Every other step leaves storage as it is.

The steps and output in this guide are measured on a NanoPi R6S, an RK3588S board, on
2026-10-05. The image was OpenWrt 25.12.5, and the loader was FriendlyELEC's.
[Rockchip RK3588](../boards/rk3588.md) lists everything else measured on that board. The same
steps apply to a board whose SoC is pinned, `rk3576` or `rk3588`. On any other board, each step
is `[UNVERIFIED]`.

## Requirements

Have these before you start:

- A loader container for the SoC, either rkbin's `*_loader.bin` or a board vendor's
  `MiniLoaderAll.bin`. FriendlyELEC's RK3588 eflasher image carries the one used here.
- The disk image for the board
- A USB cable from the host to the port the board's maskrom answers on. On the R6S, that is a
  USB A-to-A cable to its USB 3.0 Type-A port.
- USB access for pyrographer, set up as [Permissions](../getting-started/installing.md#permissions) describes

## Unpacking the image

`flash` writes the bytes it is given, so a compressed image is unpacked first. OpenWrt ships its
images compressed with gzip, followed by metadata:

```sh
gzip -dc openwrt-25.12.5-rockchip-armv8-friendlyarm_nanopi-r6s-squashfs-sysupgrade.img.gz > openwrt.img
```

`gzip` reports "decompression OK, trailing garbage ignored", and exits with status 2. The
trailing bytes are OpenWrt's metadata, and the image is whole. This one unpacks to 176160768
bytes.

## Loading the loader

Put the board into maskrom, as its board page describes. For the R6S, see
[Entering maskrom](../boards/rk3588.md#entering-maskrom). `list` then shows the BootROM:

```sh
pyrographer list
```

```text
2207:350b  maskrom       bus 003 address 7  (bcdUSB 0200)
```

Upload the loader, naming the SoC so that `db` checks the file's claim:

```sh
pyrographer db --soc rk3588 --loader MiniLoaderAll.bin
```

```text
This loader's chip field holds 38 38 35 33 "8853" (rk3588).
Uploading 166.01 KiB...
Uploaded 166.01 KiB in 0.3s.
Loader uploaded. A new Rockchip device appeared at 003:8. `list` can still report it as
maskrom, because this loader keeps the bcdUSB flag even. The next command sent to it checks
that a loader answers.
```

Check that the loader answers, and read the size of the eMMC:

```sh
pyrographer chipver
pyrographer info
```

```text
Device:       2207:350b (maskrom)
Chip version: 16 bytes
  hex         38 38 35 33 ff ff ff ff ff ff ff ff ff ff ff ff
  ascii       8853............
Device:       2207:350b (maskrom)
Flash ID:     45 4d 4d 43 20
Flash size:   28.91 GiB (60620800 sectors of 512 bytes)
```

The heading still reads maskrom, because this loader keeps the even flag. The replies come from
the loader. Note the sector count, which [Clearing a stale backup GPT](#clearing-a-stale-backup-gpt)
uses.

If every command after the upload reports maskrom, no loader answered. Power-cycle the board
into maskrom and upload again, as
[Bringing a maskrom board to loader mode](../reference/cli/bootstrapping.md#bringing-a-maskrom-board-to-loader-mode)
describes.

## Planning the write

A whole-disk image starts at sector 0. Print the plan without writing anything:

```sh
pyrographer flash --soc rk3588 --dry-run 0 openwrt.img
```

```text
This will overwrite 168.00 MiB of flash on 2207:350b.

  image        openwrt.img
               168.00 MiB
  LBA range    0 through 344063 (344064 sectors)
  byte range   0 through 176160767
  touches                       311296 of its 59965440 sectors
  flash        28.91 GiB (60620800 sectors of 512 bytes)
  loader says  38 38 35 33 ff ff ff ff ff ff ff ff ff ff ff ff  "8853............"
  named SoC    rk3588: the loader's answer matches

Each window is read back and compared before the next one is written. A mismatch stops the write at that window.
Nothing here can be undone.
Dry run: nothing was written.
```

Check two lines. The `named SoC` line says the loader's answer matches the SoC you named. The
`touches` line names what the write lands in. Here it is the one partition the eMMC held before,
which had no name.

## Writing the image

Run the same command without `--dry-run`, and answer `y`:

```sh
pyrographer flash --soc rk3588 0 openwrt.img
```

```text
Writing 168.00 MiB...
Wrote 168.00 MiB in 16.8s.
Wrote openwrt.img and read every window of it back.
```

## Clearing a stale backup GPT

A GPT keeps a backup copy in the disk's last 33 sectors. An image laid out with an MBR, as
OpenWrt's is, replaces the primary GPT and leaves that backup behind. Disk tools such as
`sgdisk` then report a damaged primary, and offer to restore it from the backup. That restore
writes the old partitions over the image's MBR.

Skip this step if the disk held no GPT before the write. Otherwise, write 33 zero sectors at the
end of the eMMC. Their first LBA is the sector count from `info`, less 33:

```sh
head -c 16896 /dev/zero > zero-33.bin
pyrographer flash --soc rk3588 60620767 zero-33.bin
```

The plan for this write reports that the device has no partition table:

```text
  touches      nothing that can be named: this device has no partition table
```

pyrographer reads a disk whose primary GPT signature is gone as having no GPT. It does not read
an MBR, so it reports no table at all.

## Verifying and booting

Compare the eMMC with the image, through a fresh open:

```sh
pyrographer verify 0 openwrt.img
```

```text
Verifying 168.00 MiB...
Verified 168.00 MiB in 17.7s.
Flash matches openwrt.img.
Note: 158.27 MiB read back as constant 0x00 or 0xff, across 4 regions, consistent with erased or unallocated flash.
```

The note describes the image's own empty space. This image compresses to 9 MB, so most of its
168 MiB is blank.

Reboot the board out of the loader:

```sh
pyrographer reset
```

```text
The board is rebooting.
```

The board leaves the bus and boots from the eMMC. The R6S came up in OpenWrt 25.12.5. If the
BootROM finds nothing it can boot, it falls back to maskrom, and the board appears in `list`
again `[DOC]`.
