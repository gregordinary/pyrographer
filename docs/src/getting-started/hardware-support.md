# Hardware support

This page lists what has run against real hardware, and on which board. Everything else is
built and tested against a scripted device, which replays the bytes a real device sends. The
reference pages carry the same evidence tags beside each command they describe.

Every board result here comes from one board, the H96 Max M9, an RK3576 TV box.
[Rockchip RK3576](../boards/rk3576.md) holds what is measured on it.

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

| Command | On hardware |
|---|---|
| `list` | Measured. The mode probe tells the BootROM from a loader. |
| `info` | Measured. It agrees with rkdeveloptool field for field. |
| `chipver` | Measured. The reply is pinned as `rk3576`. |
| `capability`, `storage` | `[UNVERIFIED]` |
| `partitions` | Measured. It reads the board's 16-partition GPT. |
| `dump` | Measured. A 4 MiB dump is byte-identical to rkdeveloptool's. |
| `db --loader` | Measured, with rkbin's SPL loader and with a mainline U-Boot container. |
| `db --code471 --code472` | `[UNVERIFIED]` |
| `reset` | Measured in the default mode. The other three modes are `[COMMUNITY]`. |
| `flash`, `verify`, `clone` | `[UNVERIFIED]`. No write to a board has run. |
| `repair-table`, `repair-param`, `author-gpt`, `author-param` | `[UNVERIFIED]` |

The wrong-loader gate is armed for `rk3576` alone, so a write to any other Rockchip SoC is
refused. Erasing is refused. The erase opcode is published by Rockchip `[DOC]`, but no board has
confirmed what the command does to a range.

Through rkbin's SPL loader, the H96 Max M9 answers every read past its first 32 MiB with constant
fill. [The 32 MiB read wall](../boards/rk3576.md#the-32-mib-read-wall) says how to read the rest.

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

`recover` is `[UNVERIFIED]`. The ROM's timing, the block size the recovery agent takes, and the
newline its menu expects are all unsettled.

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
