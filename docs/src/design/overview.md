# Design overview

pyrographer keeps vendor-specific code small, so its operations generalize across boards. Each
section on this page names one part of the library and what it owns.

## Transport

`Transport` moves bytes between the host and a device over USB or serial. It is async, so the
code that consumes it runs unchanged over native transports (nusb, serial2) and browser ones
(WebUSB, Web Serial).

## Image seam

The image seam is the other end of a long verb: the source of a write's bytes, and the
destination of a dump's. It is async for the same reason `Transport` is. A file picked in a
browser is a `Blob`, which yields its bytes through a promise and offers no blocking read.

A verb therefore consumes two async seams and implements neither. Bytes come off a device and
go out to a file, a buffer or a `Blob`, one window at a time. The image is never held in memory.

## Bootstrap

Bootstrap brings a newly detected device from its on-chip boot code to a state where its flash
is reachable. It is the one per-vendor part, and it is small.

## FlashAgent

`FlashAgent` is the uniform block interface every backend provides: read, write, erase and
info. The verbs consume `FlashAgent` rather than `Transport`, because one backend has no
transport. A **raw block device the host operating system owns** is a file, and its agent reads
and writes that file directly.

## Read-back

Each backend decides the point at which a write is proved to have landed, and the write plan
names it. The rockusb backend reads back each window before it sends the next. A DFU board holds
every block until the session ends, so its region is streamed, committed and then read back
whole. In both cases the read-back is mandatory, and its memory use stays bounded.

## Partitions

The partition layer maps a sector number to a name, and a name to a range the device itself
supplied. It reads two formats, the UEFI GPT and Rockchip's own parameter block, and one uniform
type covers both.

It also writes both formats, through the same gated, read-back write path every other write
uses. It repairs a damaged copy from an intact one, and authors a fresh table from a layout you
supply.

Each format keeps more than one copy, and the reader uses the redundancy. A table that fails its
own checksum is reported as damaged, not as absent. A half-written table therefore stays visible
to whoever writes to the board next.

A write can be aimed at a partition by name. That is the only form that knows where the
partition ends. It is therefore the only one that can refuse an image too large to fit.
Whichever way a write is aimed, its plan names the partitions it will land in, and how much of
each. That plan is shown before the write is confirmed.

## Verbs

The verbs are written once against `FlashAgent` and cannot tell the backends apart, so `dump`,
`flash`, `clone` and `verify` work on every backend.

## Serial drivers

Two drivers consume the `Serial` seam directly, and have no `FlashAgent`. One is StarFive's UART
recovery, a write-only XMODEM sender. The other is the **serial console**, which drives a
bootloader prompt.

Neither addresses sectors or carries a partition table, so neither offers the uniform verbs.
Each has a verb of its own, rather than the uniform set with every verb disabled.

The console connects the USB and serial halves. A bootstrap loads U-Boot into DRAM: the Rockchip
maskrom bootstrap over USB, or the StarFive one over the serial line. The console then instructs
that U-Boot, at its own prompt, to answer on the bus.

## Codecs

Every byte layout is a pure, sans-I/O codec, unit-tested without hardware. This covers both the
layouts that cross the wire and those stored on the flash.

## Capabilities

Each backend advertises its capabilities, so the tool refuses or grays out the verbs a board
cannot perform, and states why. The reason is written once, in the layer that decides it, and
every front-end quotes it.
