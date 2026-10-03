# Block devices

pyrographer drives block devices on Linux. A block device is an SD card in a reader, or a board
that has come up as USB mass storage. The host operating system also owns these devices: it
mounts their filesystems and caches their sectors.

Every verb that reads or writes flash works on a block device, including the
[partition-table commands](partition-tables.md). A write goes through the same plan,
confirmation and mandatory read-back as a write to a board. The commands that address a loader
or a boot ROM refuse a block device by name.

`list` prints boards, and `list --blocks` also prints the machine's disks:

```sh
pyrographer list --blocks
```

```text
No boards found.

Block devices:
  /dev/nvme0n1     1.82 TiB  nvme      [refused: running system]
  /dev/sdb        29.72 GiB  usb
  /dev/sdc        14.86 GiB  usb       [read-only: no write]

A block device is acted on only when it is named: --device <node>.
`[refused: running system]` cannot be opened at all, and has no override.
`[read-only: no write]` and `[no write]` still dump, verify and list partitions.
```

pyrographer chooses a single connected board by default, but never a disk. On a machine with
one disk, that disk is the one the system runs from. Name the disk with `--device`:

```sh
pyrographer dump --device /dev/sdb 0 2048 head.img
```

```text
Reading 1.00 MiB...
Read 1.00 MiB in 0.1s.
Wrote head.img
```

On a board in a bootstrap mode, pyrographer is the only program addressing the device, and a
read-back is a round trip to the flash. A block device is shared with the operating system. So
pyrographer opens it exclusively, bypasses the page cache, and refuses every disk the running
system rests on. The next three sections cover each of these.

## Exclusive open

pyrographer asks the kernel for exclusive use of the device. If anything else holds the device,
the kernel refuses the open. That covers a mounted filesystem, an active swap, and a volume
stacked on the device. The kernel's check is broader than any check pyrographer can make
itself, and its refusal names the remedy:

```sh
pyrographer flash --device /dev/sdb 0 debian.img
```

```text
error: invalid request: `sdb` is in use: the kernel refused an exclusive open. Something holds
it, such as a mounted filesystem, a volume stacked on it, or swap. Unmount or deactivate it first
(mounted at /media/you/BOOT)
```

## Cache bypass

Reads bypass the page cache, so the read-back returns what the device holds and not a page the
host has cached. A read served from the cache matches the image whether or not the write reached
the device.

## The running system

**pyrographer refuses every disk the running system rests on, outright and with no override.**
The refusal covers reads as well as writes. A write to one of these disks can pass its read-back
and still bring the machine down later, with no error reported. So pyrographer makes this
refusal before anything is opened.

The refusal is transitive. `/` is often a volume in an encrypted container on a partition.
pyrographer follows that stack to the physical disk and refuses it, not only the device `/` is
mounted from.

```sh
pyrographer flash --device /dev/nvme0n1 0 debian.img
```

```text
error: invalid request: `nvme0n1` holds the running system, so pyrographer will not write to it.
A write would corrupt this machine without reporting an error at the time. There is no override
```

Removable media is not part of the running system. The exclusive open refuses a card mounted at
`/media/you/BOOT`, and names a remedy. The running-system refusal has neither a remedy nor an
override.

## Write-protected cards

pyrographer opens a card with its lock switch on for reading. The read verbs work on it, and a
write is refused with the reason:

```sh
pyrographer dump --device /dev/sdc 0 2048 head.img
pyrographer flash --device /dev/sdc 0 debian.img
```

```text
Reading 1.00 MiB...
Read 1.00 MiB in 0.1s.
Wrote head.img
error: invalid request: `sdc` is marked read-only by the kernel, so it cannot be written
```

## The block-device plan

A block device runs no loader, so the [wrong-loader gate](flash.md#the-wrong-loader-gate) has
nothing to compare. pyrographer refuses `--soc` on a block device, rather than ignoring it:

```sh
pyrographer flash --device /dev/sdb --soc rk3576 0 boot.img
```

```text
error: --soc rk3576 gates a write on the answer a loader gives, and `/dev/sdb` is a block
device with no loader on it to ask. This write is guarded instead by the kernel's exclusive
open, and no disk the running system rests on can be opened. Drop --soc.
```

In place of the loader's answer, the plan's `held` row states what guards the write:

```sh
pyrographer flash --device /dev/sdb 2048 boot.img
```

```text

This will overwrite 4.00 MiB of /dev/sdb.

  image        boot.img
               4.00 MiB
  LBA range    2048 through 10239 (8192 sectors)
  byte range   1048576 through 5242879
  touches      nothing that can be named: this device has no partition table
  device       29.72 GiB (62333952 sectors of 512 bytes)
  held         exclusively by this command. The kernel granted `sdb` with nothing else
               holding it. No disk the running system rests on can be opened.

Each window is read back and compared before the next one is written. A mismatch stops the
write at that window.
Nothing here can be undone.
Proceed? [y/N]
```

## Privilege

`list --blocks` needs no privilege. It reads everything it shows from `/sys` and `/proc`,
including the running-system refusal.

Every other command opens the device first, and that includes building a plan, even under
`--dry-run`. Opening the device needs root, or membership of the `disk` group. Without
privilege, the open fails with this error:

```text
error: I/O error: `sdb` cannot be opened without root or membership of the `disk` group.
Listing it needs no privilege, but planning or writing opens it
```

## Durability

On a device the operating system also owns, a write that passes its read-back can still be
overwritten. If a filesystem on the device is mounted elsewhere, the kernel can later write its
cached copy back over the new bytes. That behavior is measured on Windows. On Linux, the
exclusive open prevents it, which is why the exclusive open has no override either.
