# RAM-booting mainline U-Boot from maskrom

A Rockchip BootROM in maskrom takes two stages over USB: one that brings up DRAM, and one it
runs from DRAM. Mainline U-Boot builds both. `pyrographer db` uploads them, and the board
comes up at a U-Boot prompt, running from RAM.

The RAM boot writes nothing to the board's storage, and a power cycle ends it. A failed upload
leaves the board in maskrom, and [Upload failures](#upload-failures) describes the retry. Of
the steps in this guide, only [Writing the eMMC](#writing-the-emmc) writes to storage. That
write permanently replaces the data it covers.

The prompt reaches the whole eMMC, whatever the eMMC holds. From it, you can back up flash the
board's loader cannot read, test-boot an operating system from USB, and recover a board that
boots nothing. Through rkbin's SPL loader, the H96 Max M9, an RK3576 board, answers every read
at or past sector 65536 (32 MiB) with constant `0xCC`. Each of those reads reports success. The
RAM-booted U-Boot reads past this read wall and returns the data the sectors hold.

The board behavior in this guide is measured on that one board, running U-Boot v2026.04, on
2026-07-18. [Rockchip RK3576](../boards/rk3576.md) lists everything else measured on it. On
another SoC, the procedure is `[UNVERIFIED]`, and the load-address check takes that SoC's DRAM
base. pyrographer's `uboot` command, which drives the console in these steps, is
`[UNVERIFIED]` against hardware. You can type the same lines by hand in a terminal program.

## Requirements

Have these before you start:

- A U-Boot build for the board with `CONFIG_ROCKCHIP_MASKROM_IMAGE=y`, built as
  [U-Boot's Rockchip documentation](https://docs.u-boot.org/en/latest/board/rockchip/rockchip.html)
  describes
- The board's serial console on a USB-serial adapter, at 1500000 baud on the RK3576
- A way to enter maskrom at power-on, such as a maskrom button or the eMMC clock line shorted
  to ground
- USB access for pyrographer, set up as [Permissions](../getting-started/installing.md#permissions) describes
- For the container form, an rkbin checkout with its `boot_merger` tool

## The two stages

With `CONFIG_ROCKCHIP_MASKROM_IMAGE` set, binman writes two files beside the storage image:

| File | Stage | Contents | Size on the RK3576 |
|---|---|---|---|
| `u-boot-rockchip-usb471.bin` | 471 | The DRAM-init blob from rkbin, which the build takes from `ROCKCHIP_TPL` | 77 KiB |
| `u-boot-rockchip-usb472.bin` | 472 | SPL, then a FIT holding BL31, U-Boot proper and the device tree | 3.0 MiB |

The BootROM runs the 471 stage to bring up DRAM. It then loads the 472 stage into DRAM and
jumps to it. SPL boots the FIT from RAM, where the 472 stage already put it.

### Checking the load address

Check the 472 stage before you upload it. On the RK3576, the 8-byte word at offset 8 must
read `0x40000000`, the start of DRAM:

```sh
od -A x -t x8 -j 8 -N 8 u-boot-rockchip-usb472.bin
```

```text
000008 0000000040000000
000010
```

A stage whose word reads `0x40800000` uploads in full, and the board stays in maskrom. That
value is U-Boot proper's base, `CONFIG_TEXT_BASE`. In a stage that boots, the word holds the
SPL's own base, `CONFIG_SPL_TEXT_BASE`. If the check reads anything other than the DRAM base,
fix the U-Boot build before you upload.

## Preparing the upload

`db` uploads the stages packed in an RKBOOT container, or as the two raw files. The container
is the form that booted the RK3576.

### A container

rkbin's `boot_merger` packs the stages into an RKBOOT container, the format rkbin's own
loaders ship in. In an rkbin checkout, copy in the two stages and the SoC's stock recipe:

```sh
cp ../u-boot/u-boot-rockchip-usb471.bin ../u-boot/u-boot-rockchip-usb472.bin .
cp RKBOOT/RK3576MINIALL.ini RK3576UBOOT.ini
```

In `RK3576UBOOT.ini`, point the two CODE sections and the output at the stages:

```ini
[CODE471_OPTION]
NUM=1
Path1=u-boot-rockchip-usb471.bin
Sleep=1
[CODE472_OPTION]
NUM=1
Path1=u-boot-rockchip-usb472.bin
[OUTPUT]
PATH=rk3576_uboot_maskrom.bin
```

Leave the LOADER sections as rkbin ships them. `db` uploads only the CODE sections. Then
build the container:

```sh
tools/boot_merger RK3576UBOOT.ini
```

The container carries the SoC that the recipe's `[CHIP_NAME]` names. `db --soc rk3576` checks
that claim before the upload starts.

### Raw stages

`db` also sends the two files as binman wrote them, with no rkbin tooling:

```sh
pyrographer db --code471 u-boot-rockchip-usb471.bin --code472 u-boot-rockchip-usb472.bin
```

The raw form sends the same payloads as the container. It names no SoC, so `--soc` has nothing
to check. The raw form is `[UNVERIFIED]` on hardware, because the RK3576 boot used the
container.

## Booting the board

1. Put the board in maskrom and connect its USB port. `pyrographer list` shows it as
   `maskrom`.

2. In a second terminal, start `uboot` on the console before you upload:

   ```sh
   pyrographer uboot --port /dev/ttyUSB0 --baud 1500000 --reads 120 --cmd 'mmc info'
   ```

   It streams what the board prints. When U-Boot's countdown appears, `uboot` stops it and
   runs `mmc info`.

3. In the first terminal, upload the container:

   ```sh
   pyrographer db --soc rk3576 --loader rk3576_uboot_maskrom.bin
   ```

   For the raw form, run the command from [Raw stages](#raw-stages) instead. `db` then reports
   that the board did not reappear within ten seconds. This is expected, because the RAM-booted
   U-Boot answers on its serial console.

4. Read the console in the second terminal. SPL prints `Trying to boot from RAM`, which
   proves the BootROM ran the 472 stage. U-Boot proper's banner and countdown follow, and
   then the `mmc info` answer.

On the RK3576, `mmc info` reports the eMMC, `mmc@2a330000` at 116.5 GiB, as device 0. If it
reports `Card did not respond to voltage select! : -110` instead, device 0 is an empty slot.
Find the eMMC with `mmc list`, and use its number wherever this guide writes `mmc 0`. For
`uboot --gadget`, pass that number as `--gadget-dev mmc:<n>`.

U-Boot now waits at its prompt, running from RAM. A power cycle ends it.

### Upload failures

**The console stops before `Trying to boot from RAM`, and `list` still shows the board in
maskrom.** The BootROM took the 472 stage and did not run it. Check the stage as
[Checking the load address](#checking-the-load-address) describes, then power-cycle into
maskrom and upload again. On the RK3576, a stage with `0x40800000` at offset 8 uploaded whole
and failed this way.

**The upload stalls partway.** A BootROM that received part of an earlier upload accepts no new
one. Power-cycle into maskrom and run `db` again.

## Reading the whole eMMC

U-Boot presents the eMMC to the host through either of two USB gadgets, the USB device
functions that U-Boot runs on the board. The mass-storage gadget is the one measured on the
RK3576.

### Over USB mass storage

`ums 0 mmc 0` presents the eMMC to the host as a USB disk, and pyrographer reads it as it
reads any disk. To back up the eMMC:

1. Start the gadget from the prompt:

   ```sh
   pyrographer uboot --port /dev/ttyUSB0 --baud 1500000 --gadget ums
   ```

   `uboot` types `ums 0 mmc 0`. A running gadget keeps the console, so a prompt that does
   not come back means the gadget started.

2. Find the disk. It is the `usb` disk the size of the eMMC, 116.48 GiB on the RK3576:

   ```sh
   pyrographer list --blocks
   ```

   If the desktop mounts the board's partitions, unmount them. `dump` refuses a disk that
   anything else holds.

3. Read the sector count, then dump the whole disk. Both commands open the disk, which needs
   root:

   ```sh
   sudo pyrographer info --device /dev/sdb
   sudo pyrographer dump --device /dev/sdb 0 244285440 factory.img
   ```

   Replace `/dev/sdb` with the disk that `list` found, and `244285440` with the count that
   `info` printed.

4. Stop the gadget with Ctrl+C at the console, from a terminal program such as `picocom`. A
   power cycle also stops it, and ends the RAM-booted U-Boot with it.

`dump` checks every window for the constant fill that a silent read failure leaves, and warns
where it finds a run. A whole-disk dump with no fill warning holds no run of the `0xCC` that
rkbin's loader returns. [Constant-fill warnings](../reference/cli/flash.md#constant-fill-warnings)
describes the warning and its limit.

On the RK3576, `dd` read all 244285440 sectors through this gadget at ~30 MB/s, in about 70
minutes. Sector 65536 read as AArch64 boot code. A pyrographer `dump` through the same gadget
is `[UNVERIFIED]`.

### Over rockusb

`rockusb 0 mmc 0` presents the eMMC as a rockusb device, which pyrographer reads over USB
without root. `uboot` starts it:

```sh
pyrographer uboot --port /dev/ttyUSB0 --baud 1500000 --gadget rockusb
```

Mainline's rockusb gadget does not answer `K_FW_READ_FLASH_INFO`, so `info`, `partitions` and
`dump --partition` fail against it. When asked for it, U-Boot prints
`Rockusb command 1a not support yet`. A `dump` given an LBA and a sector count asks for no
flash information.

Whether this gadget reads past sector 65536 is `[UNVERIFIED]`. Before any longer read, dump
2 MiB from that sector:

```sh
pyrographer dump 65536 4096 wall.img
```

If `dump` warns of fill from sector 65536, this gadget shares rkbin's read wall. Dump through
mass storage instead.

## Writing the eMMC

Over mass storage, the eMMC is a block device. `flash` writes it through the same plan,
confirmation and window-by-window read-back as any disk. The write permanently replaces the
existing data on the sectors it covers. A write through the RAM-booted U-Boot is
`[UNVERIFIED]` on the RK3576.

To write `factory.img` to the eMMC, starting at sector 0:

```sh
sudo pyrographer flash --device /dev/sdb 0 factory.img
```

## Test-booting from USB

The RAM-booted U-Boot boots an operating system from a USB drive, and the eMMC stays
untouched. Plug the drive in, then set the boot order for one boot:

```sh
pyrographer uboot --port /dev/ttyUSB0 --baud 1500000 --boot-from usb0
```

`uboot` shows the board's current boot order beside the order it sets, and asks before it
boots. It saves nothing to the board.
[Overriding the boot order](../reference/cli/serial.md#overriding-the-boot-order)
describes the plan. A USB boot from the RAM-booted U-Boot is `[UNVERIFIED]` on the RK3576.
