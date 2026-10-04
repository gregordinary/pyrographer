# Firmware packages and the ID block

A Rockchip SDK build ends in one file, a firmware package usually called `update.img`. It
carries a loader, a partition table, and an image for each partition. The commands on this page
check a package, write one to a board, and write the ID block a loader builds. None of them has
run against a board `[UNVERIFIED]`, and no package from a vendor build has been read yet.

## Checking a firmware package

`firmware-info` reads a package from front to back and checks it. It opens no device:

```sh
pyrographer firmware-info update.img
```

```text
Checking 12.57 MiB...
Checked 12.57 MiB in 0.1s.

update.img: a Rockchip firmware package, 12.57 MiB.

  model        RK3576
  made by      rockchip
  version      1.0.0, built 2026-10-04 12:00:00
  chip field   38 00 00 00 (reported, not compared)
  loader       772.49 KiB at byte 102, naming 36 37 35 33 "6753" (rk3576)
  archive      11.81 MiB at byte 791135, and its checksum holds
  trailer      MD5 4da475d9293b2235b480e827eca3f63c, reported and not checked
  ID block     704 sectors, laid out by the FlashHead header, every hash checked
               FlashBoost at sector 8, 8 sectors, loads at 0x3ffc0000
               FlashData at sector 16, 160 sectors, loads at 0x3ff81000
               FlashBoot at sector 176, 528 sectors
  partitions   uboot            16384 sectors at LBA 16384
               misc             8192 sectors at LBA 32768
               boot             131072 sectors at LBA 40960
               recovery         262144 sectors at LBA 172032
               rootfs           from LBA 434176, growing to fill the device, GUID 614e0000-0000-4b53-8000-1d28000054a9
  images       uboot            4.00 MiB from Image/uboot.img
               misc             48.00 KiB from Image/misc.img
               boot             1.00 MiB from Image/boot.img
               rootfs           6.00 MiB from Image/rootfs.img
  not written  package-file: the entry is the packing tool's own list of files

Every check passed: the archive's checksum, the two copies of the loader, the ID block's hashes, and the parameter's checksum.
```

The package above was packed for this page around rkbin's RK3576 loader. A package is refused
when any of these checks fails:

- The archive's trailing checksum. It catches a truncated or damaged download, and it covers
  every byte a write takes from the archive.
- The ID block. It is built from the package's loader, and every hash its header records is
  checked. [The ID block](#writing-the-id-block-alone) explains how.
- The two copies of the loader. A package carries its loader twice, once in the outer header
  and once in the archive, and the two must be the same bytes.
- The parameter's own checksum.
- An Android sparse image. pyrographer writes images raw, and a sparse image has to be expanded
  as it is written.

The trailer is reported and not checked, because what its MD5 covers is unmeasured. The chip
field is reported and not compared, for the same reason. The loader's own container names its
SoC, and that claim is the one the write judges.

## Writing a firmware package

`flash-firmware` writes a whole package as one plan:

```sh
pyrographer flash-firmware --soc rk3576 update.img
```

```text
This will write the firmware package update.img to 2207:350e:
RK3576 by rockchip, version 1.0.0.

  writing      partition 'uboot' from Image/uboot.img
               LBA 16384 through 24575 (8192 sectors), 4.00 MiB
               partition 'misc' from Image/misc.img
               LBA 32768 through 32863 (96 sectors), 48.00 KiB
               partition 'boot' from Image/boot.img
               LBA 40960 through 43008 (2049 sectors), 1.00 MiB
               partition 'rootfs' from Image/rootfs.img
               LBA 434176 through 446463 (12288 sectors), 6.00 MiB
               the primary GPT (protective MBR, header, and entry array from sector 0)
               LBA 0 through 33 (34 sectors), 17.00 KiB
               the backup GPT in the device's last sectors
               LBA 122142687 through 122142719 (33 sectors), 16.50 KiB
               the ID block, 3 images laid out by its RKNS header
               LBA 64 through 767 (704 sectors), 352.00 KiB
  ID block     laid out by the FlashHead header, every hash checked
               FlashBoost at sector 8, 8 sectors, loads at 0x3ffc0000
               FlashData at sector 16, 160 sectors, loads at 0x3ff81000
               FlashBoot at sector 176, 528 sectors
  partitions   uboot            16384 sectors
               misc             8192 sectors
               boot             131072 sectors
               recovery         262144 sectors
               rootfs           121708511 sectors
  not written  package-file: the entry is the packing tool's own list of files
  loader file  36 37 35 33 "6753" (rk3576)
  capability   NEW_IDB set: the loader writes an RKNS ID block
  flash        58.24 GiB (122142720 sectors of 512 bytes)
  loader says  36 37 35 33 00 00 00 00 00 00 00 00 00 00 00 00  "6753............"
  named SoC    rk3576: the loader's answer matches

The package's partition table replaces the device's. Each window is read back and compared before the next one is written. A mismatch stops the write at that window.
Nothing here can be undone.
Proceed? [y/N]
```

`flash-firmware` reads and checks the whole package before it chooses a device. It then plans
the write, and writes the runs in the order the plan lists them:

1. Every partition image, in the order the file holds them. The file is read forward once more,
   and each image streams from it into its partition.
2. The primary GPT and the backup.
3. The ID block, at sector 64, last.

Each run is written and read back window by window, as `flash` reads back a single image. The
first window that fails stops the whole write there. `--dry-run` prints the plan and stops, and
`--yes` answers the prompt.

### The partition table the package lays down

The GPT comes from the package's own parameter, and the parameter has to say `TYPE: GPT`. That
line makes the offsets in its `mtdparts` list the sectors a GPT addresses. Two more of its lines
shape the table:

- A `uuid:<name>=<GUID>` line pins that partition's unique GUID. An SDK pins `rootfs` this way,
  because the kernel command line finds its root by that GUID.
- The partition whose size is `-` grows to the GPT's last usable sector. The backup GPT holds
  the device's last 33 sectors, so the partition stops short of them.

Every other GUID is derived from the layout, as `author-gpt` derives one. A parameter without
`TYPE: GPT` describes a table written as a Rockchip parameter block instead, and is refused.

### What it refuses

A firmware write is refused before anything is confirmed in each of these cases:

- The wrong-loader gate refuses it, as it refuses any write. `--soc` names the board's SoC.
- The package's loader container claims a different SoC from the one `--soc` names. The ID block
  is built from that container.
- The running loader does not claim `NEW_IDB`, or does not answer the capability query.
  Rockchip's own rkdeveloptool writes this kind of ID block only through a loader that claims
  it. `capability` prints the loader's answer.
- An image entry names no partition in the parameter, or is larger than its partition.
- Two runs overlap, or the ID block lands inside a partition.
- The device's sectors are not 512 bytes. Every offset in a package counts 512-byte sectors.

On a block device, such as an SD card in a reader, no loader is asked, and `--soc` is refused as
it is for `flash`.

### A board in maskrom

A board in maskrom takes `db` first. `db` takes the package itself and uploads the loader inside
it, so nobody extracts the loader by hand:

```sh
pyrographer db --soc rk3576 --loader update.img
```

```text
Using the loader inside the firmware package update.img.
This loader's chip field holds 36 37 35 33 "6753" (rk3576).
```

## Writing the ID block alone

`write-idb` builds the ID block from a loader container and writes it at sector 64, the place
the BootROM reads first. It leaves the partition table as it is:

```sh
pyrographer write-idb --soc rk3576 --loader rk3576_spl_loader_v1.12.108.bin
```

`--loader` also takes a firmware package, and uses the loader inside it. The plan has the same
rows as a package plan, with one run.

A loader container carries the ID block's pieces, its flash stages, and not the block itself.
One stage is an `RKNS` header. It lists each image the block holds, where it sits, and its
SHA-256. pyrographer places each image where the header says, and finds the stage for each image
by its hash, not by its name. Every hash, including the header's own, is checked before
anything is planned.

On the RK3576 container, that layout differs from the one rkdeveloptool's `ul` writes. `ul`
joins three stages by name and leaves out a fourth, `FlashBoost`, which the header places first.
[Rockchip RK3576](../../boards/rk3576.md#the-id-block) has the measured layout.

pyrographer builds the `RKNS` layout only. A container whose stages carry no `RKNS` header uses
an older layout, and is refused by name. So is a container that wants its stages scrambled on
the flash.

## Firmware files written raw

A firmware package and a loader container are each unpacked by a tool before any of their bytes
go to the flash. Written raw, neither boots, so `flash` refuses both:

```sh
pyrographer flash 0 update.img
```

```text
error: invalid request: the image is a Rockchip firmware package (RKFW, an update.img), which a tool unpacks before anything of it goes to the flash. Written raw, it does not boot. A package is written as a whole: its partition images, its partition table and its ID block, each where it belongs. Run `pyrographer flash-firmware <file>` to write a firmware package.
```

The refusal reads the file's first four bytes. A prebuilt ID block, which begins `RKNS`, is not
refused, so `flash 64 idbloader.img` writes one as it is.
