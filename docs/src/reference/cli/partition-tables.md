# Partition tables

pyrographer reads two partition-table formats, GPT and Rockchip's `parameter` block. It repairs
a damaged copy of either from an intact one, and writes a fresh table from a layout. A table also
lets you aim `dump`, `flash` and `verify` at a partition by its name.

## Reading the partition table

`partitions` prints the board's partition table:

```sh
pyrographer partitions
```

```text
Device:       2207:350e (loader)

Partition table: GPT (5 partitions)

  NAME                    FIRST LBA      SECTORS  SIZE
  uboot                       16384         8192  4.00 MiB
  trust                       24576         8192  4.00 MiB
  misc                        32768         8192  4.00 MiB
  boot                        40960       229376  112.00 MiB
  rootfs                     270336    121872384  58.11 GiB

A partition can be named instead of an LBA:  --partition <name>
The table records where each partition ends, so on a write, only a name
can refuse an image too big to fit in it.
```

A board carries one of two table formats. Modern Rockchip boards use a standard GPT. Older ones
use Rockchip's own parameter block, whose `CMDLINE` line lists the partitions. pyrographer
detects the format by probing for each in turn: `EFI PART` at sector 1, then a `PARM` block
where one is kept.

pyrographer does not read an MBR. A disk laid out with an MBR alone reads as having no partition
table, and is aimed by LBA. OpenWrt's image for a Rockchip board is one such disk.

A board with no partition table is in a valid state. `partitions` says so, and succeeds:

```text
Device:       2207:350e (loader)

This device has no partition table.
```

### Damaged tables

A GPT keeps two copies: the primary near the front of the device, and the backup in its last
sector. If the primary is damaged and the backup is intact, `partitions` prints a warning, then
lists the backup's partitions:

```text
Device:       2207:350e (loader)

Warning: the primary GPT is damaged. These partitions were recovered from
         the backup GPT in the device's last sector.
         Primary GPT: the header's CRC is 0x2ab4f019, and the header says 0x00000000

Partition table: GPT (5 partitions)

  NAME                    FIRST LBA      SECTORS  SIZE
  uboot                       16384         8192  4.00 MiB
  ...
```

pyrographer reads the backup from the device's last sector. It ignores the damaged primary's
record of where the backup is. A primary whose signature is gone entirely reads as no GPT, and
the backup is not consulted.

If neither copy is intact, `partitions` fails with an error rather than printing an empty list.
The error tells you the table is damaged, for example half-written, before you write to the
board again:

```text
error: the GPT partition table on this device is damaged: the primary GPT is damaged
(the header's CRC is 0x2ab4f019, and the header says 0x00000000), and the backup GPT header
expected in the last sector (LBA 122142719) is absent
```

Commands that take a raw LBA still work on such a board. With no intact copy to repair from,
[author a fresh table](#authoring-a-partition-table).

## Repairing a damaged partition table

`repair-table` rewrites a damaged GPT copy from the intact one, in either direction:

- A damaged primary is rebuilt from the backup, the case `partitions` warns about.
- A stale, damaged or missing backup is rebuilt from the primary.

Either way, the device ends with two intact copies. A repair is a write like `flash`. It
overwrites the damaged copy, and cannot be undone. The loader must match the SoC you name with
`--soc`, and `repair-table` reads back every window. `--dry-run` and `--yes` work as they do for
`flash`.

This repair rebuilds a damaged primary from the backup:

```sh
pyrographer repair-table --soc rk3576
```

```text

This will repair the GPT table on 2207:350e,
from the backup GPT in the device's last sector.

  rewriting    the primary GPT (sector 1 onward)
               LBA 1 through 33 (33 sectors of 512 bytes)
  restores     uboot            8192 sectors
               trust            8192 sectors
               misc             8192 sectors
               boot             229376 sectors
               rootfs           121872384 sectors
  flash        58.24 GiB (122142720 sectors of 512 bytes)
  loader says  36 37 35 33 00 00 00 00 00 00 00 00 00 00 00 00  "6753............"
  named SoC    rk3576: the loader's answer matches

The intact copy is not touched. Each window is read back and compared before the next one is written. A mismatch stops the write at that window.
Nothing here can be undone.
Proceed? [y/N] y
Repairing 16.50 KiB...
  100.0%  16.50 KiB of 16.50 KiB  1.63 MiB/s
Repaired 16.50 KiB in 0.1s.
Rewrote the primary GPT (sector 1 onward) and read every window of it back.
```

The other direction rewrites the backup in the last sectors of the disk. It is the safer of the
two, because it never writes the primary the board boots from. A stale backup passes its
checksums but describes an older layout, such as one a tool left behind after rewriting only the
primary. `repair-table` treats a stale backup as damaged, because the primary is authoritative.

The rewritten copy is byte-faithful. `repair-table` copies the intact copy's partition entries
exactly, so every partition keeps its type GUID and unique GUID. It never writes the intact copy,
so one good copy survives throughout the repair.

If both copies are intact and agree, or the device has no GPT, there is nothing to repair.
`repair-table` says so and writes nothing:

```sh
pyrographer repair-table --soc rk3576
```

```text
error: invalid request: both copies of this device's GPT are intact and agree, so there is
nothing to repair
```

`repair-param` does the same for Rockchip's parameter block. Raw NAND keeps several copies of
it, so `repair-param` rewrites a damaged copy from an intact one. An eMMC keeps a single copy,
with no other copy to rebuild it from. On an eMMC, [author a fresh
table](#authoring-a-partition-table) instead.

## Authoring a partition table

`author-gpt` and `author-param` write a fresh table from a layout you supply. Authoring is the
fix for a board with no intact copy anywhere. Each is a write like `flash`. It is gated on the
loader match, reads back every window, and cannot be undone.

Both read the same native layout, one partition per line:

```text
name first_lba sectors [type] [uuid=<GUID>]
```

Numbers are hex or decimal. A `-` in place of `sectors` grows the partition to the end of the
device. For `author-gpt` it stops at the last sector before the backup GPT. `#` starts a
comment. Both commands also read a board's own `mtdparts=` line from a
file given with `--mtdparts`.

A layout file, `rk3576.layout`:

```text
# name    first_lba  sectors  type
uboot     0x4000     0x2000   linux
trust     0x6000     0x2000   linux
boot      0x8000     0x38000  linux
rootfs    0x40000    -        linux
```

```sh
pyrographer author-gpt --soc rk3576 --layout rk3576.layout
```

`author-gpt` writes a protective MBR, a primary, and a backup in the device's last sectors. A
partition's type comes from its type token:

- `linux` or `data`, for Linux data
- `esp` or `efi`, for an EFI system partition
- `swap`, for Linux swap
- `jh7110-spl`, for the partition a StarFive JH7110 boot ROM loads its SPL from in SD mode
- A raw type GUID, for any other type

A partition with no type token is Linux data.

`author-gpt` synthesizes the disk GUID and each partition's unique GUID from the layout.
Authoring the same layout twice therefore yields the same table, and a re-author verifies
against an earlier dump. To pin a specific value, add a `disk-guid <GUID>` line or a `uuid=`
attribute.

`author-param` writes a Rockchip parameter block, and requires `--medium emmc` or
`--medium nand`. The medium decides where the copies go and what a layout's offsets count from.
On eMMC, the block has one copy at sector `0x2000`. On raw NAND, it has several, from sector 0.
A GPT's offsets are absolute, so `author-gpt` takes no medium.

`author-param` also takes `--from-block <file>`, the whole text of an existing parameter block.
It frames that text verbatim, so `FIRMWARE_VER` and every other key are kept. A block built from
a layout is minimal, and carries none of them.

Both commands check the table as they build the plan, before anything is written. They refuse
each of these:

- A partition in a region the table format reserves
- A partition that runs past the end of the device
- Two partitions that overlap
- A partition of zero sectors
- Two partitions with the same name

Names must be unique because `--partition` aims a write by name, at a range whose end is known.
[Addressing a partition by name](#addressing-a-partition-by-name) describes it.

### The JH7110 eMMC fix-up

A StarFive JH7110 boot disk carries two words in its first two sectors that the GPT does not
own. In eMMC mode, the boot ROM reads a header from the start of the disk. A sentinel fails the
header's check, and the ROM loads its SPL from a backup address instead. `spl_tool -i` writes
both words: the backup address at byte `0x4`, and the sentinel `0x5A5A5A5A` at byte `0x290`.

`author-gpt` writes a fresh protective MBR over the first word, and `repair-table` rewrites the
sector that holds the second. Both commands keep the fix-up a disk already carries, and the plan
says so on a `keeps` line. The line names the backup address and the partition it falls in:

```text
  keeps        This write keeps the JH7110 boot ROM's eMMC fix-up that the disk carries. The ROM loads its SPL from byte 0x200000, inside partition 'spl'.
```

A disk without the sentinel gets no fix-up, and its plan has no `keeps` line. A repair of the
backup GPT writes nothing near the fix-up.

## Addressing a partition by name

Every command that takes an `<lba>` also takes `--partition <name>` in its place. The range then
comes from the device's own table:

```sh
pyrographer dump --partition boot boot.img
```

```text
Partition:    boot (LBA 40960, 229376 sectors, 112.00 MiB)
Reading 112.00 MiB...
  100.0%  112.00 MiB of 112.00 MiB  4.18 MiB/s
Read 112.00 MiB in 26.8s.
Wrote boot.img
```

With `--partition`, `dump` takes no sector count, because the table records each partition's
length.

On a write, a name is the safer form. An LBA is a number you supply, and pyrographer cannot check
it against a partition. A name resolves against the table, which records where the partition
ends, so only a name can refuse an image too big to fit:

```sh
pyrographer flash --partition uboot too-big.img
```

```text
error: invalid request: the image is 5242880 bytes, which needs 10240 sectors, and the
partition 'uboot' is 8192 sectors long. Writing it there would overrun the end of 'uboot'
into whatever follows it
```

At a raw LBA, the same image runs through the end of `uboot` and into `trust`. The board accepts the
write, because the sectors after a partition are valid sectors.

Names match exactly. If the board has no partition with the name you give, the error lists the
names it has:

```sh
pyrographer flash --partition Boot boot.img
```

```text
error: this device has no partition named 'Boot'. Its partitions are: uboot, trust, misc, boot,
rootfs
```

pyrographer does not correct a near miss such as `Boot` for `boot`. On a board with both `boot`
and `boot_a`, a guessed match can aim a write at the wrong partition.
