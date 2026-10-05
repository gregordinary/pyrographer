# Hardware support

This page lists what has run against real hardware, and on which board. Everything else is
built and tested against a scripted device, which replays the bytes a real device sends. The
reference pages carry the same evidence tags beside each command they describe.

The board results here come from two Rockchip boards. The H96 Max M9 is an RK3576 TV box, and
the NanoPi R6S is an RK3588S router board. [Rockchip RK3576](../boards/rk3576.md) and
[Rockchip RK3588](../boards/rk3588.md) hold what is measured on each.

## Evidence tags

A claim with no tag is measured on a named board, or pinned by a test that needs no hardware. A
weaker claim carries one of these tags:

| Tag | Meaning |
|---|---|
| `[DOC]` | Published by the vendor, or in a specification |
| `[COMMUNITY]` | From community tools or reports, with no vendor source |
| `[UNVERIFIED]` | Built and tested here, and not yet checked against hardware |
| `[WEAK]` | A value chosen by reasoning, such as a timeout, with no measurement behind it |
| `[CONTESTED]` | The sources disagree |

## Rockchip

| Command | RK3576, H96 Max M9 | RK3588S, NanoPi R6S |
|---|---|---|
| `list` | Measured. The mode probe tells the BootROM from a loader. | Measured. The mode probe finds the loader. |
| `info` | Measured. It agrees with rkdeveloptool field for field. | Measured |
| `chipver` | Measured. The reply is pinned as `rk3576`. | Measured. The reply is pinned as `rk3588`. |
| `capability`, `storage` | `[UNVERIFIED]` | Measured |
| `partitions` | Measured. It reads the board's 16-partition GPT. | Measured |
| `dump` | Measured. A 4 MiB dump is byte-identical to rkdeveloptool's. | Measured. Three ranges are byte-identical to a raw read. |
| `db --loader` | Measured, with rkbin's SPL loader and with a mainline U-Boot container. | Measured, with FriendlyELEC's `MiniLoaderAll.bin` |
| `db --code471 --code472` | `[UNVERIFIED]` | `[UNVERIFIED]` |
| `reset` | Measured in the default mode. The other three modes are `[COMMUNITY]`. | Measured in the default mode |
| `flash`, `verify` | `[UNVERIFIED]`. No write to this board has run. | Measured. A 168 MiB image is written and read back, then verified through a fresh open. |
| `clone` | `[UNVERIFIED]` | `[UNVERIFIED]` |
| `repair-table`, `repair-param`, `author-gpt`, `author-param` | `[UNVERIFIED]` | `[UNVERIFIED]` |
| `write-idb`, `flash-firmware` | `[UNVERIFIED]`. The ID block layout is measured from rkbin's loader. | `[UNVERIFIED]` |
| `firmware-info` | `[UNVERIFIED]`. No package from a vendor build has been read. | `[UNVERIFIED]` |

The wrong-loader gate is armed for `rk3576` and `rk3588`, so a write to any other Rockchip SoC is
refused. The RK3588S write followed
[Flashing a disk image from maskrom](../guides/maskrom-flash.md). Erasing is refused. The erase opcode is published by Rockchip `[DOC]`, but no board has
confirmed what the command does to a range.

Through rkbin's SPL loader, the H96 Max M9 answers every read past its first 32 MiB with constant
fill. [The 32 MiB read wall](../boards/rk3576.md#the-32-mib-read-wall) says how to read the rest.
The NanoPi R6S reads to the last sector of its eMMC.

## Mainline U-Boot on a Rockchip board

These results come from
[RAM-booting mainline U-Boot from maskrom](../guides/maskrom-uboot.md), with U-Boot v2026.04:

| Step | On hardware |
|---|---|
| RAM-booting U-Boot with `db` | Measured |
| Reading the whole eMMC through U-Boot's mass-storage gadget | Measured with `dd`. A pyrographer `dump` through the gadget is `[UNVERIFIED]`. |
| `info` and `partitions` through U-Boot's rockusb gadget | Fails. The gadget does not answer the flash-information command. |
| Writing the eMMC through either gadget | `[UNVERIFIED]` |
| Booting from USB | `[UNVERIFIED]` |

The U-Boot commands in these steps ran from a terminal program. `console` and `uboot`, which
drive the same prompt, are `[UNVERIFIED]` against hardware.

## Ingenic XBurst

The Ingenic backend targets the XBurst2 camera SoCs: the T31, T40 and T41. The Wyze Cam v3,
for example, is built on a T31.

`usbboot` is `[UNVERIFIED]`, and so are `dump`, `verify` and `partitions` on a DFU device.
Writing is built, and refused until an Ingenic SoC is pinned against real hardware.

## StarFive JH7110

`recover` and `uartboot` are `[UNVERIFIED]`, because no JH7110 board has run them. How the
recovery agent reads its menu, and what it prints, are read from the agent's own code. The
BootROM's handshake timing is unsettled. So is whether U-Boot's mass-storage gadget enumerates
through a Mars CM carrier's USB port.

## Block devices

Block devices run on Linux. The bench under `bench/` in the repository runs the Block backend
against a loop device, and settles these:

- The exclusive open, and its refusal of a device the kernel holds
- A GPT written by another tool, read back through the partition codecs
- `dump`, and the range checks on a live device
- A write with its window-by-window read-back
- A `verify` through a fresh open, which finds the written bytes

A loop device keeps its data in a file in the host's page cache. So durability on a physical
medium is `[UNVERIFIED]`, and so are USB media and 4 KiB native sectors.

## The window and the web flasher

Every result on this page was produced through the command-line tool. The window runs the same
verbs on the same core. Its own flows are `[UNVERIFIED]` against hardware. The web flasher
compiles, and is `[UNVERIFIED]` in a browser.

## Host operating systems

Every result on this page was measured on a Linux host. The USB and serial crates pyrographer
builds on also support Windows and macOS. pyrographer on either is `[UNVERIFIED]`.
