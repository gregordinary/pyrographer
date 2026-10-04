# Introduction

> [!WARNING]
> pyrographer is under active development, and many of its features are untested on real
> hardware. No write to a board has run on hardware yet. A write that goes wrong can leave a
> board unable to boot, or destroy the data on a disk. Use pyrographer at your own risk, and
> only on a device you can afford to lose. In this book, `[UNVERIFIED]` marks a feature that
> passes its tests against scripted devices and has not yet run on a real one.

pyrographer is a flashing and recovery toolkit for embedded devices, such as single-board
computers and IP cameras, written entirely in safe Rust. It comprises a command-line tool, a
desktop application and a Rust library, built on the same core. The desktop application also
builds for the browser as the web flasher, which is `[UNVERIFIED]`.

## Supported devices

pyrographer supports three SoC families, and block devices on Linux:

- **Rockchip**, over USB, through the SoC's boot and recovery modes.
- **StarFive JH7110**, over serial, through the SoC's UART recovery mode.
- **Ingenic XBurst**, over USB. These SoCs are found in IP cameras such as the Wyze Cam v3.
  pyrographer brings the boot ROM up to DFU, then reads the device's flash.
- **Block devices**, on Linux, such as an SD card in a reader or a board that presents its
  storage as USB mass storage.

The architecture leaves room for a Broadcom (CM4) backend, which is not yet built.
[Hardware support](getting-started/hardware-support.md) lists what has run on real hardware.

## Core operations

pyrographer provides four core operations:

- `dump` reads a device's flash to an image file.
- `flash` writes an image to a device, then reads it back to check the result.
- `clone` copies one device's flash directly to another.
- `verify` compares a device's flash against an image.

They run on Rockchip boards and on block devices. An Ingenic device in DFU supports `dump`,
`verify` and `partitions`, aimed by partition name. Its write is built, and refused until an
Ingenic SoC is pinned against real hardware.

A write permanently replaces the existing data on its target. Before each write, pyrographer
presents a plan of the changes and proceeds only on explicit confirmation. The plan names the
device and each partition the write touches.

## Disks

A disk is shared with the host operating system, which can mount filesystems on it and cache
its sectors. pyrographer therefore takes exclusive use of a disk, and reads its writes back
uncached. It refuses every disk the running system depends on, with no override.

## Bootstrapping and recovery

Four commands serve one vendor each:

- `db` uploads a loader to a Rockchip board in maskrom, the BootROM's USB download mode.
- `usbboot` uploads two stages to an Ingenic boot ROM, which brings the device up in DFU.
- `recover` writes a bootloader to a StarFive board's flash over serial. The recovery mode cannot
  read flash, so the write is not read back.
- `uartboot` boots a StarFive board into U-Boot over serial, and writes nothing.

## Bootloader prompts

Two commands work with a board's bootloader prompt over a serial line:

- `console` watches a board's console for the text that marks success or failure.
- `uboot` drives a U-Boot prompt. It can present the board's storage over USB, boot from
  another source once, or send a single command.

pyrographer works up to the bootloader prompt. A login prompt is outside its scope.

## How this book is organized

The book has five parts:

- [Getting started](getting-started/installing.md) covers installing pyrographer, and what has
  run on real hardware.
- [Guides](guides/maskrom-uboot.md) walk through one task from start to finish.
- [Boards](boards/rk3576.md) hold what is measured on each SoC, and on each board built on it.
- [Reference](reference/cli/index.md) covers each command, and each flow in the window.
- [Design](design/overview.md) describes the library's seams, for a developer who calls it.
