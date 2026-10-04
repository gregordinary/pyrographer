# Reading and writing flash

These four commands copy or compare an image between a file and a device's flash, or between
two devices:

- `dump` reads a range of flash into a file.
- `flash` writes an image to a range of flash, then reads it back to check the result.
- `verify` compares flash with a file, and changes neither.
- `clone` copies the whole of one board's flash onto another board.

A write permanently replaces the existing data on its target. Before each write, pyrographer
prints a plan of the changes and proceeds only on explicit confirmation. Each command streams a
window at a time, so no image has to fit in memory.

## Dumping flash

`dump` reads a range of sectors into a file:

```sh
pyrographer dump 0 2048 boot.img
```

```text
Reading 1.00 MiB...
  100.0%  1.00 MiB of 1.00 MiB  4.21 MiB/s
Read 1.00 MiB in 0.2s.
Wrote boot.img
```

The arguments are the starting LBA, the number of sectors, and the output file.
`--partition <name>` replaces the first two.

`dump` refuses an output file that already exists. To overwrite it, pass `--force`. If a dump
fails partway, `dump` warns that the file holds only a partial image.

### Constant-fill warnings

If a large region reads back as one repeated byte, with a success status, `dump` prints a
warning and still writes the image:

```text
Warning: 84.00 MiB from sector 65536 (32.00 MiB in) read back as constant 0xcc.
         Constant fill reported as a successful read can be a silent read failure rather than data. If this region holds data, read it again another way, such as through mass storage or the board's console, before you trust the image.
```

Some loaders answer a read past a certain point with a buffer they never filled. Every sector
holds the same byte, and the read reports success. A dump across such a region looks complete
and is not. The fill is stable, so reading again does not reveal it.

The warning does not stop the dump. The same fill can be valid data on another board, so you
decide whether the region is trustworthy.

A run of `0x00` or `0xff` is how erased or unallocated flash normally reads, so `dump` reports
it as a note rather than a warning:

```text
Note: 12.00 GiB read back as constant 0x00 or 0xff, across 2 regions, consistent with erased or unallocated flash.
```

Both messages go to standard error. A silent read failure that returns `0x00` or `0xff` looks the
same as blank flash, so it produces the note and not the warning.

## Writing an image to flash

`flash` writes an image to the device, permanently replacing the data in the range it covers. It
first prints a plan of the write, and writes only once you confirm it:

```sh
pyrographer flash --soc rk3576 16384 u-boot.img
```

```text

This will overwrite 4.01 MiB of flash on 2207:350e.

  image        u-boot.img
               4.01 MiB, padded with 1.52 KiB to 4.01 MiB
  LBA range    16384 through 24589 (8206 sectors)
  byte range   8388608 through 12590079
  touches      uboot            the whole of it (8192 sectors)
               trust            14 of its 8192 sectors
  flash        58.24 GiB (122142720 sectors of 512 bytes)
  loader says  36 37 35 33 00 00 00 00 00 00 00 00 00 00 00 00  "6753............"
  named SoC    rk3576: the loader's answer matches

Each window is read back and compared before the next one is written. A mismatch stops the write at that window.
Nothing here can be undone.
Proceed? [y/N]
```

pyrographer builds the plan by querying the device, through the same code path as the write.
`--dry-run` prints the plan and stops there, and `--yes` skips the question. If no terminal is
attached and `--yes` is absent, `flash` refuses.

`flash` also refuses a Rockchip firmware package and a loader container, which do not boot when
written raw. [Firmware files written raw](firmware.md#firmware-files-written-raw) shows the
refusal and the commands that write each.

### The touches line

Read the `touches` line before you confirm. It names the partitions the range lands in, which
you can check against what you intended. The LBA range is arithmetic, and is correct whether or
not it is what you meant. In the plan above, the write covers the whole of `uboot` and 14
sectors of `trust`. If you meant to write `boot`, answer `n`.

The image in this plan is 14 sectors too long for `uboot`. The device accepts the write, because
the sectors after `uboot` are valid sectors. pyrographer refuses the same write aimed by name,
as [Addressing a partition by name](partition-tables.md#addressing-a-partition-by-name) shows.

On a board with no partition table, the plan says so:

```text
  touches      nothing that can be named: this device has no partition table
```

On a board whose table is damaged, the plan says so. It still plans the write, because writing
a fresh table is how a damaged one is repaired:

```text
  touches      unknown: the GPT on this device is damaged,
               so the plan cannot name what these sectors hold:
               the header's CRC is 0x2ab4f019, and the header says 0x00000000
```

### Read-back

`flash` writes each window, reads it back from the flash, and compares it with what was sent,
before the next window goes out. The read-back is mandatory, and no flag turns it off. Both eMMC
and NAND can report success before a block is committed, so only a read-back confirms a write.

Progress counts a window once it is written and verified, so the reported rate is roughly half
the raw bus throughput.

A difference stops the write and names the byte:

```sh
pyrographer flash --soc rk3576 64 boot.img --yes
```

```text
error: flash differs from the image at byte 777: read 0xff, expected 0x5a
```

pyrographer neither retries nor rolls back. A retry that succeeds can make a failing eMMC look
healthy. The windows already written stay written, and the data they replaced is gone. You
decide whether the difference is a bad block, the wrong image, or a failing board.

### Read-back on a DFU board

The per-window read-back applies to a rockusb board and to a block device. On a DFU board,
`flash` writes and commits the whole range, then reads it back and checks it. A DFU board, such
as an Ingenic board brought up by `usbboot`, holds every block of a download until the session
ends. It serves no read until then. The [wrong-loader gate](#the-wrong-loader-gate) refuses a
write to an Ingenic DFU board, because no Ingenic SoC is pinned.

The check is still mandatory, and still holds for an image larger than memory. A difference is
found only once the whole range is written, not after one window of it. The plan's closing lines
state which read-back applies:

```text
This device can be read back only after the write is committed. The whole range is written,
then read back and compared. The comparison cannot be skipped, and a mismatch is found with the
whole range already written.
```

The image streams once and is never held in memory, so the error names the window that differs
rather than the byte. It also states that the range was written:

```text
error: the flash differs from what was written, in the 1048576-byte window at byte 0. This
backend can be read back only after a write is committed, so the difference was found after the
region was written. The region does not hold the image and must be written again
```

`flash` refuses a board that cannot be read back at all. A DFU device that detaches as it commits
is one, because the device that returns is not the one the write went through.

### The wrong-loader gate

Every write to a board is checked against the loader's answer to which SoC it runs on, shown on
the plan's `loader says` line. Name the board's SoC with `--soc`. pyrographer compares the
answer with the reply pinned for that SoC, and refuses the write on any difference. A loader is
built for one SoC, but runs on whatever board it was uploaded to. The wrong loader writes to the
wrong offsets, and returns a plausible status for every command.

The comparison is exact: the whole reply must equal, byte for byte, the reply a real board of
that SoC gave. Nothing is decoded or guessed. `rk3576` is the only SoC pinned.

The gate refuses in three cases, each before anything is written. The first is a write with no
SoC named:

```sh
pyrographer flash 64 boot.img
```

```text
[... the plan, with:  named SoC    none: the write will be refused until a SoC is named (--soc) ...]

error: invalid request: rockusb write: name the board's SoC. The write proceeds only when the
loader's chip-version reply matches it
```

The second is a SoC with no pinned reply. pyrographer refuses it before opening a device, and the
error lists the pinned names.

The third is a loader that answers as a different SoC from the one you named:

```text
error: the loader does not match rk3576, the SoC this write was planned for. Loaders for
rk3576 answer 36 37 35 33 ... ("6753..."), and this loader answered 38 38 35 33 ...
("8853..."). A loader for the wrong SoC writes to the wrong offsets, so nothing was written
```

The first and third refusals come after the plan and before the question. The plan still prints
in full, and its `named SoC` line carries the gate's verdict. pyrographer does not ask you to
confirm a write it will refuse.

## Verifying flash against an image

`verify` reads flash and compares it with a file, a window at a time:

```sh
pyrographer verify 0 boot.img
```

```text
Verifying 1.00 MiB...
  100.0%  1.00 MiB of 1.00 MiB  4.05 MiB/s
Verified 1.00 MiB in 0.2s.
Flash matches boot.img.
```

The arguments are the starting LBA and the file, or `--partition <name>` and the file. The
file's length sets how far the comparison runs, so there is no sector count. A difference names
the byte:

```sh
pyrographer verify 0 other.img
```

```text
error: flash differs from the image at byte 1234: read 0xaa, expected 0xff
```

An image that ends partway through a sector is compared only over the bytes it has. The device
still reads that last sector in full.

`verify` checks for constant fill as `dump` does, and prints the same warning. A passing verify
over a filled region does not confirm the data. If the image was dumped across the same silent
read failure, both copies hold the same fill and agree.

## Cloning one board onto another

`clone` copies the whole of one board's flash onto another, and overwrites everything on the
destination. It works as `dump` and `flash` in one pass, a window at a time, with no
intermediate file. A 64 GiB part clones in a megabyte of memory.

```sh
pyrographer clone --soc rk3576 --from 003:12 --to 003:14
```

```text

This will overwrite the whole of 2207:350e on bus 003 address 14.

  source       2207:350e on bus 003 address 12
               --from 003:12
               58.24 GiB (122142720 sectors of 512 bytes)
  destination  2207:350e on bus 003 address 14
               --to 003:14
               58.24 GiB (122142720 sectors of 512 bytes)
  LBA range    0 through 122142719 (122142720 sectors)
  touches      uboot            the whole of it (8192 sectors)
               trust            the whole of it (8192 sectors)
               misc             the whole of it (8192 sectors)
               boot             the whole of it (229376 sectors)
               rootfs           the whole of it (121872384 sectors)
  loader says  36 37 35 33 00 00 00 00 00 00 00 00 00 00 00 00  "6753............"
  named SoC    rk3576: the loader's answer matches

The source is only read. Each window is read back and compared before the next one is written. A mismatch stops the write at that window.
Nothing here can be undone.
Type the destination (003:14) to confirm, or anything else to stop:
```

The `touches` list names every partition on the destination in full, because a clone overwrites
all of it.

Name both boards with `--from` and `--to`. `clone` infers neither, even with exactly two boards
connected, because that choice decides which board is overwritten. `--dry-run` and `--yes` work
as they do for `flash`.

### Typed confirmation

To confirm a clone, type the destination's coordinate, `003:14` in the plan above. Any other answer
stops the clone. The plan prints both boards in the form `--from` and `--to` take, so the
coordinate to type is on the screen. The window asks for the same confirmation.

A `flash` names one device, so a `y` against a plan naming it is enough. A clone names two, and
the mistake to guard against is a swapped source and destination. Reading the plan does not
reveal a swap, because both halves of it are accurate. A `y` confirms whatever plan is on the
screen, and a typed coordinate confirms one device. If you believe the other board is the
destination, you type its coordinate instead, and the clone stops.

### What a clone reads and writes

The source is only read, so a clone does not write to the board being copied. The destination is
read back and compared window by window, as a `flash` target is. `clone` refuses a source larger
than the destination rather than truncating it, because a truncated clone is not a bootable
board.

`clone` checks the source for constant fill as `dump` does. With `dump`, a fill affects only the
image file, which you can discard. A clone writes the fill onto the destination before the
warning appears. The warning therefore prints at the end of the clone, as a finding about the
board just written:

```text
Cloned, and read every window of it back.
Warning: 84.00 MiB from sector 65536 (32.00 MiB in) read back as constant 0xcc.
         Constant fill reported as a successful read can be a silent read failure rather than data. If this region holds data, read it again another way, such as through mass storage or the board's console, before you trust the image.
```

Like `flash`, `clone` is gated on the [loader match](#the-wrong-loader-gate). `--soc` names the
destination's SoC, because the destination is the board that is written.
