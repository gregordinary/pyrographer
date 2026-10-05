# The window

`pyrographer-gui` is pyrographer's desktop application. It runs on the same core as the CLI, so
it makes the same checks and the same refusals. What the [command-line reference](cli/index.md)
says a command does holds here too. Where the CLI prints the plan for a write, the window shows
it as a screen.

The GUI is outside the workspace's default members, so build and run it by name:

```sh
cargo run -p pyrographer-gui
```

## Tabs

The window has two tabs, one for each kind of target:

| Tab | What it drives |
|---|---|
| Boards and disks | Anything whose flash pyrographer reads and writes: a board in a boot mode, or one of this machine's disks. These have sectors, geometry and a partition table, so every verb applies. |
| Serial | Anything on a serial line: a StarFive board in UART recovery, or a bootloader prompt. These have no sectors and no partition table, so the flash verbs do not apply. |

While a plan waits for your confirmation, the tab bar is hidden. A write plan takes the whole
window, so the only ways out of it are to cancel or to confirm. Either way, the tab you were on
returns.

### Jobs

A job keeps running across a change of tab. Each running job appears on a strip at the bottom of the
window, with its own *Cancel*. The strip names the tab that holds the job's full detail: byte
counts, the transfer rate, or a console transcript. When no job is running, the strip is hidden.

The strip stays on screen during a plan, and offers no way out of the plan. A job that was
already running continues behind the plan, and its *Cancel* is the only control over it. If a
second plan is waiting behind the one on screen, the screen says so. Canceling the first plan
then does not move you into the second without notice.

### Sections that open and close

A name drawn in the accent color, with a triangle before it, opens and closes the section under
it. Click the name to open the section, and click it again to close it. The name stays where it
is, so the control that closed a section is the one that opens it again. A screen reader
announces each one as a button, expanded or collapsed.

Some sections start closed: **Disks**, **Partition table**, **Firmware**, and both sections on
the **Serial** tab. They hold tools for a particular job, and open when you ask for them.

### Forms

In a form, every label stands in one column and every field and button starts on one line.
Under the fields is the button that acts on them. A section's headings are announced as
headings, so a screen reader can move from one to the next.

## Choosing a board

The list shows every Rockchip and Ingenic board in a boot or recovery mode. While no job is
running, the window rescans the bus, so a board you plug in appears. Boards pyrographer cannot
drive are listed too. Each row shows the board's mode:

| Mode | What it means |
|---|---|
| `loader` | A loader is running and the flash is reachable. |
| `maskrom` | The bcdUSB flag reports the BootROM. Some loaders present the same even flag, the RK3576 SPL and the RK3588 usbplug among them, so *Open* probes the board to confirm the mode. A board running its BootROM needs a loader first: select the board and use *Upload loader...*. |
| `mass storage` | The board re-enumerated as a USB mass-storage device, and the host operating system owns it as a disk. Open it from [Disks](#disks). |
| `boot ROM` | An Ingenic device is running its USB boot ROM, which has no flash commands. [Bootstrap it](#bootstrapping-an-ingenic-board) to DFU first. |
| `DFU` | An Ingenic device is running a DFU-capable U-Boot. Its flash is reachable as named alt-settings. |

If the list is empty, it says so. If you have not opened **Disks** yet, it also points you there. A
card reader is listed under **Disks**, not in the board list.

If a board does not open, the cause is most often permissions. The error says so and gives the
udev rule that fixes it, as the CLI does.

## Disks

An SD card in a reader is a disk the host operating system owns. So is a board that has come up as
mass storage. A disk opens in the same slot as a board, and the same verbs work on it, including the
**Partition table** section. [Block devices](cli/block-devices.md) covers what differs about a
device the operating system owns.

**Disks** is a section that stays closed until you open it, the window's form of
`list --blocks`. The window keeps disks in their own section, apart from boards, so that a disk
is not selected by accident.

The Block backend is built for Linux. Every build draws the **Disks** section. On another
platform, the section names the platform the backend is built for, rather than showing an empty
list.

The window reads the disk list as the section opens and on *Rescan*. The list changes only then, so
the rows stay in place while you choose one. A device with no medium, such as an empty card reader
or an unbound loop device, gets no row of its own. Such devices are counted and named on one line,
so they cannot push the card you are looking for out of view.

### Refusals on a disk

Each row shows any refusal that applies to it before you pick it. Only the running-system
refusal replaces the row's buttons:

- **The running system.** A disk the running system rests on shows its refusal in place of
  buttons, and cannot be opened at all. There is no override. The refusal is transitive, so it
  also covers the disk that holds a volume or encrypted container the system runs from. A write to
  one of these disks can pass its read-back and still bring the system down minutes later, with
  no error reported.
- **A mounted card.** The exclusive open refuses it, and the error names the mount and the
  remedy.
- **A write-protected card.** It opens for reading. Its write controls are grayed out where the
  verbs are, with the reason beside them.

### Opening a disk

Opening a disk holds it exclusively, with the host's cache bypassed, for as long as it stays
open. The open also lists the disks again. If the device at that node no longer matches the row,
the open is refused. This catches a card removed while its row was on screen, and another device
that has taken its name.

Listing disks needs no privilege, because everything the rows show comes from `/sys` and
`/proc`. *Open* needs root, or membership of the `disk` group. Without either, the open fails and
the error says so.

### Confirming a disk write

To confirm a disk write, type the disk's node path, such as `/dev/sdb`, as a board write asks for
its bus address. The plan's last rows show the exclusive open and the running-system guard, where
a board's plan shows its loader's answer.

The SoC field is not drawn for a disk, because a disk runs no loader. A SoC name left over from a
board you had open does not carry into a disk's plan. *Chip version* and the reset modes are
drawn disabled, with the reason.

## Uploading a loader

A board in maskrom mode runs the BootROM, which serves no flash commands, so the board needs a
loader first. Select the board, and the panel under it offers *Upload loader...*. It runs the
same upload as the CLI's `db`. It asks for the loader file, rkbin's `*_loader.bin` for the SoC
or a board vendor's `MiniLoaderAll.bin`. It parses the file as soon as you pick it. A file that
is not an RKBOOT loader is refused at the dialog, before any device is opened.

The upload runs as a job, with a progress bar and a *Cancel* that stops it at the next chunk.

The loader container names the SoC it was built for. The upload panel shows that claim while the
upload runs, and the report shows it again. Both give the raw bytes and the SoC pinned for them,
as `db` does. If the SoC field names a SoC, a file built for another SoC is refused before the
upload starts.

The claim is written by whoever built the file. It catches a file picked by mistake, and does not
prove what the loader does once it runs.

### Raw stages

**Raw stages (no container)**, in the same panel, uploads bare stage files, matching the CLI's
`db --code471` and `--code472`. It starts closed. Mainline U-Boot's binman emits bare
`u-boot-rockchip-usb471.bin` and `u-boot-rockchip-usb472.bin` files with no container, which
*Upload loader...* refuses. The form takes them as two separate files, *Stage 471* and
*Stage 472*. Stage 471 initializes DRAM, and stage 472 runs after it. At least one is required,
and 471 always goes first.

Raw stages load a full U-Boot into the board's DRAM over USB. The board then answers on its
serial port rather than on the bus, as [The serial console](#the-serial-console) describes. Bare
stages carry no container, so they name no SoC. The form states that nothing checks which board
they are for.

### After the upload

On success, the maskrom USB device disconnects, and the board re-enumerates as a new device at a
new address. On an RK3576, this takes somewhat over three seconds, and on an RK3588S, under one.
The window clears the old selection and rescans. When the board reappears, select it. It can
still be listed as `maskrom`, because some loaders present an even bcdUSB flag, and *Open*
probes the board to confirm the mode.

The container parse, the upload and the re-enumeration are verified on an RK3576 and an RK3588S
through the CLI's `db`. The window's own run of this flow is `[UNVERIFIED]` against hardware.

## Bootstrapping an Ingenic board

An Ingenic board in its boot ROM has no flash commands. Select the board, and the
**Bootstrap to DFU** form under it brings the board to DFU. The form starts open, because the
upload is the only thing a boot ROM can do. It is the window's counterpart of the CLI's
`usbboot`. The form has these fields:

| Field | What it holds |
|---|---|
| Stage 1 | The DRAM-init SPL, which is required |
| Stage 1 address | Where the SPL loads and runs |
| Stage 2 | The DFU-capable U-Boot. Without it, the board only initializes DRAM and does not re-enumerate. |
| Stage 2 address | Where U-Boot loads and runs in DRAM |
| DRAM settle | The wait in milliseconds after stage 1 for DRAM to come up. The community value is around 2000 ms. |

The load addresses default to thingino-dfu's family-wide values, `0x80001800` for the SPL and
`0x80100000` for U-Boot. If your build links its stages elsewhere, change them. *Bootstrap...*
runs the upload as a job. On success, the board re-enumerates as a DFU device, and its flash is
reachable as named alt-settings.

This flow is `[UNVERIFIED]` against hardware.

## Reading a board

The **Read** part of the panel under an open board holds five buttons. Each one queries the board
and shows the answer:

- *Flash info* reports the flash ID and geometry, and names the storage medium on the geometry
  line.
- *Partitions* lists the partitions under the board. Read it first, because it lets you aim every
  other verb by partition name.
- *Chip version* shows the loader's answer about which SoC it is on.
- *Capability* shows the loader's own report of what it supports, including any flag pyrographer
  has no name for. Nothing is gated on it.
- *Storage medium* names the medium that the sector numbers on this screen are offsets into. On a
  board with more than one medium, the same sector number names more than one place.

*Chip version*, *Capability* and *Storage medium* use the vendor protocol. On a disk, they are
drawn disabled, with the reason.

The partition list always shows sector counts. It shows byte sizes only once the sector size is
known: after *Flash info*, or on a device whose backend knows its own block size. The window
does not assume a 512-byte sector, because on a 4Kn disk that assumption makes every size eight
times too small.

### Reset modes

The **Device** part of the panel holds the button that ends the session, after its *Reset mode*
menu. The mode decides whether the board comes back, comes back as a disk, or stays off. The
button's label names the selected ending: *Reboot*, *Reboot into USB mass storage*, *Power off*,
or *Reboot into maskrom*.

Any mode other than the plain reboot shows a caution. No board has answered those subcodes. They
write no flash, but each one leaves the board in a mode the open session cannot drive.

### Disabled controls

*Erase*, in the **Device** part, is drawn disabled, with the reason under it. No board has confirmed what the erase
command does to a range, and an erase with the wrong range semantics destroys data rather than
failing.

These other refusals also show their reason in plain view, beside the control:

- A disk the running system rests on
- A disk the kernel is holding
- *Chip version* and the reset modes on a disk
- A clone onto a board that has no whole-device image to receive

Help text that is not a refusal, such as what a mode is or what a field expects, appears on
hover. It is also attached to the control for a screen reader. [Accessibility](accessibility.md)
covers both.

## Aiming a verb

*By partition* is the default. The name resolves against the board's own partition table, which
records where the partition ends. It is therefore the only form that can refuse an image too large
to fit. With a raw LBA, an image can run past the end of `uboot` and into `trust`.

*By LBA* takes a sector number you work out yourself, which pyrographer cannot check.

## Dumping

*Dump to file...* asks where to save the image, then streams it a window at a time. The image never
has to fit in memory, so a 58 GiB eMMC dumps in a megabyte of RAM. The progress bar shows bytes and
a rate. *Cancel* stops at the next window boundary, never partway through a command, so the board is
not left in the middle of a transfer. The file keeps what was already written.

## Writing

A write permanently replaces the existing data in the range it covers. It runs only after you
have reviewed its plan and confirmed it.

*Plan a write...* asks the board for its geometry, its partition table, and its loader's answer
about which SoC it is on. It then shows the plan. The plan is the dry run: the same code path as
the write, stopped one step short.

The plan names:

- The destination board
- The sector range and the byte range
- The padding out to the last whole sector
- The flash the range sits in
- The loader's raw answer, and the gate's verdict on it
- When the write is read back and compared
- The partitions the write lands in, and how much of each

The last of these is the line to check:

```text
  touches      uboot            the whole of it (8192 sectors)
               trust            100 of its 8192 sectors
```

It is the one line you can check against what you know about your board. A range such as "LBA
16384 through 24675" is correct arithmetic whether or not it is the range you meant. "The whole
of `uboot`, and 100 sectors of `trust`" names what the write replaces. If you meant to write
`boot`, cancel the plan.

### Typed confirmation

To confirm, type the destination's bus address, such as `003:12`. It is the same string the
device list shows and the CLI's `--device` takes. Each board has a different address, so the
confirmation cannot be given out of habit, as a button click or a fixed word can. A clone with
its source and destination swapped asks for a different string. Reading the plan cannot catch
that mistake, because both boards are real and every line of the plan is accurate.

After you confirm, the write is read back and compared at the point the plan's *checked* row
names. On a board in loader mode and on a disk, each window is read back before the next one is
sent. A difference stops the write and names the byte. A DFU board can be read back only after
the whole range is committed, and a difference names the window. The read-back cannot be turned
off.

## Cloning

A clone copies one device's whole flash onto another, and replaces everything on the destination.
You choose both ends explicitly, even with only two devices connected, because the only difference
between them is which one is overwritten.

Every row, board or disk, has two buttons. *Clone from* picks the device to copy, and *Use*
picks the device to overwrite. Each gets its own panel with its own *Open*, and a clone needs
both open. The source's panel comes first, in the order the plan lists them, and has a *Forget*
that releases the device. The source is only read.

*Plan a clone...* shows both ends before you confirm anything. You confirm with the
destination's address, so a clone with its ends swapped asks for a different string, as
[Typed confirmation](#typed-confirmation) describes.

A clone addresses both devices by raw LBA. A DFU board has no device-wide LBA space, so it can be
neither end of a clone. With a DFU board at either end, the button is disabled, and the reason
is shown below it.

## Partition tables

A partition table is stored in flash, so rewriting one is a write, with the same plan and
confirmation. Repair and authoring are in the **Partition table** section of an open board. The
section starts closed: click its name to open it. Repair and authoring are for a board whose
table is already damaged or wrong. An ordinary dump or flash needs neither.

A partition table sits at fixed sectors of a device-wide LBA space. A DFU board reaches its
flash only by named region and has no such space. On a DFU board, the repair buttons and
*Author table...* are drawn disabled, and the reason is shown above them.

### Repairing a damaged table

A GPT and a Rockchip parameter table each keep more than one copy. *Repair GPT...* and *Repair
parameter...* each rebuild a damaged copy from an intact one:

- A damaged primary GPT is rebuilt from the backup in the last sector, and a stale backup from
  the primary.
- A damaged parameter copy is rebuilt from one of the several copies raw NAND keeps.

Choose the button for the table's format. A table damaged enough to need repair can be too
damaged for its format to be detected.

The intact copy is never written. Each button plans first, like every write. If the copies
already agree, or the board has no table of that format, there is nothing to repair. The plan
says so, and nothing is written. An eMMC keeps a single parameter copy, so a damaged one there
has no other copy to be rebuilt from. Author a fresh table instead.

### Authoring a table

**Author a fresh table**, inside the section, opens a form with these fields:

| Field | Choices |
|---|---|
| Format | GPT, or Rockchip parameter |
| From | Native layout, mtdparts line, or parameter text |
| File | The layout file |
| Medium | eMMC or raw NAND, for a Rockchip parameter table only |

Parameter text is an existing parameter block's whole text, kept verbatim, so `FIRMWARE_VER` and
every other key are preserved. It is offered for a parameter table only. The medium decides where
the copies go and what a layout's offsets count from. A GPT's offsets are absolute, so it needs
no medium.

*Author table...* then plans the write, with the same plan screen and typed confirmation as any
other write. The plan names each copy it writes and the partitions the new table holds. Every
window is read back as it is written.

## Firmware

The **Firmware** section of an open board writes a Rockchip firmware package, or a loader's ID
block alone. Like the partition table section, it starts closed. Both writes use the same plan
screen and typed confirmation as any other write. Neither has run against a board
`[UNVERIFIED]`.

*Plan firmware write...* takes the package chosen in its *Package* row. The whole package is
read and checked before the board is asked anything, so a large package shows progress first.
The plan then lists every run in the order it is written:

1. Each partition image, into the partition the package's parameter names
2. The primary GPT and the backup
3. The ID block, at sector 64, last

The plan also names the ID block's images, the partitions the board holds afterward, and the
entries left out. It shows what the loader file claims, and whether the running loader claims
`NEW_IDB`. [Firmware packages and the ID block](cli/firmware.md) describes each check and each
refusal.

*Plan ID block write...* takes the file chosen in its *Loader* row: a loader container, or a
firmware package whose loader is used. It writes the ID block alone, and leaves the partition
table as it is.

A firmware package and a loader container do not boot when written raw. *Plan a write...*
refuses either as soon as it reads the file's first bytes, before the board is asked anything.

On a DFU board, both firmware buttons are disabled, and the reason is shown above them.

## Recovering a StarFive board

StarFive's JH7110 boards, such as the VisionFive 2 and the Milk-V Mars CM, recover over a serial
line. Their BootROM has no USB and acts as an XMODEM receiver, so the board does not appear in the
device list. Recovery is the **StarFive recovery (serial)** section of the **Serial** tab, which
starts closed.

Strap the board into UART recovery, connect a USB-serial adapter, and fill in the form:

1. Name the port the adapter appears on, such as `/dev/ttyUSB0` or `COM3`.
2. Choose the files. A recovery sends the recovery agent, `jh7110-recovery-*.bin`, first. Choose
   an SPL, as `u-boot-spl.bin` or `u-boot-spl.bin.normal.out`, and a U-Boot payload.

In the web flasher, *Choose port...* takes the place of the path field and opens the browser's
chooser, as [The web flasher](#the-web-flasher) describes.

The section offers two jobs, and both start once you power the board on:

- *Plan recovery...* writes the board's QSPI NOR flash through the recovery agent. It needs the
  agent and at least one of the SPL and U-Boot, and an SPL needs U-Boot beside it.
- *Boot U-Boot in RAM* sends the SPL and U-Boot without the agent, and writes nothing. It is
  described below.

### Writing the boot flash

*Plan recovery...* shows what will be written and where. The SPL goes at `0x0` and U-Boot at
`0x100000`, and the agent writes a backup copy of the SPL at `0x200000`. The plan also states the
one way a StarFive recovery differs from every other write:

> This board's write is not read back.

The recovery protocol cannot read flash. Each block is acknowledged as it is received, and the
agent reports whether each write finished. Neither proves the flash holds the file. The plan
states this before you confirm.

To confirm, type the port path, as a board write asks for its bus address. The window sends the
agent and then each file, and shows what the agent prints as it writes. It reports a write the
agent says failed as an error, in the agent's own words. Afterward, power off the board, set the
boot strap back to normal, and power on.

The window types at the agent's menu only when the agent asks. The agent also offers OTP fuse
burning, and the window never chooses it, because a burned fuse cannot be reversed. If the fuse
menu ever appears, the job stops and tells you to power the board off. The whole recovery is
`[UNVERIFIED]` against hardware.

### Booting U-Boot in RAM

*Boot U-Boot in RAM* sends the SPL to the BootROM, and the SPL loads U-Boot over the same serial
line. The window then stops U-Boot at its prompt, which it recognizes by the prompt the serial
console's form names, `=> ` by default. Nothing is written, so there is no plan to confirm. The SPL must be a mainline SPL built to load
U-Boot from the UART.

From there, the serial console starts U-Boot's mass-storage gadget on the same port. The board's
eMMC then appears under **Disks**, where a write reads back every window, as
[Writing a StarFive eMMC as a disk](cli/serial.md#writing-a-starfive-emmc-as-a-disk) describes.
The RAM boot is `[UNVERIFIED]` against hardware.

## The serial console

The serial console drives a bootloader prompt over a serial line. Its section is on the
**Serial** tab after StarFive recovery, and you reach it the same way, by naming a port. A serial
line does not announce what is connected, so the **Serial console** section starts closed. The
serial console is `[UNVERIFIED]` against hardware.

### Watching the console

*Watch the console...* reads what the board prints and sends nothing to it. Enter the text that
means the board worked and the text that means it did not, one pattern per line. The transcript
streams into the window as it arrives, so a session that is waiting does not look like a hung one.

The window reports the pattern the board printed first, from either box. A failure pattern is
reported as a finding in the caution color, not as an error. The board answered with the text you
were watching for.

Patterns match as bytes, so a trailing space is part of the pattern. For a byte you cannot type,
use an escape from the set [Watching a serial console](cli/serial.md#watching-a-serial-console)
lists.

### Driving a U-Boot prompt

**U-Boot prompt**, a part of the section that starts closed, holds the remaining controls. Each row
is a field and the buttons that use it. The fields are the prompt string, the *Gadget device* a
gadget exposes, the *Boot order* to set, and the *Command* to type.

#### Gadgets

*Start the rockusb gadget* and *Start the mass-storage gadget* complete the RAM-boot that a
maskrom upload starts. A board RAM-booted into a full U-Boot answers on its serial port, and does
not reappear on the bus by itself. The upload's success message says so, and points here.

Each button tells that U-Boot to expose the board's flash over USB. The rockusb gadget returns the
board to the device list. The mass-storage gadget brings its flash up under **Disks** on the
*Boards and disks* tab.

#### Boot from

*Boot from...* sets U-Boot's boot order and boots, for one boot only. Its plan asks the board
first: it runs `printenv boot_targets` and shows the answer beside the order that will be set.

Nothing is saved. U-Boot keeps its environment in RAM until `saveenv` writes it, and *Boot
from...* never sends one. The next reset restores the board's own order.

The override asks for a plain yes rather than a typed address, and appears as a panel instead of
taking the whole window. It involves no second board and destroys nothing, so the typed address
that guards against a swapped clone is not needed.

#### Command

*Command* types one line at the prompt. **It is ungated, and the window says so in the caution
color.** U-Boot runs whatever you type, `saveenv` included, with no plan and no confirmation.

## The wrong-loader gate

Every write to a board is gated on the loader's own answer about which SoC it runs on. You name
the board's SoC in the **SoC field** on the form. The gate compares the loader's raw answer, the
plan's `loader says` line, with the reply a real board of that SoC gave, byte for byte. Nothing
is decoded and nothing is guessed. Two SoCs are pinned, the RK3576 and the RK3588. [The wrong-loader
gate](cli/flash.md#the-wrong-loader-gate) explains why the comparison is exact.

The gate refuses in three cases, each as early as it can:

- A SoC the gate does not know is flagged beside the field as you type it, because no reply is
  pinned to compare against.
- If no SoC is named, the plan still renders, and its `named SoC` line carries the verdict. The
  write is refused until you name a SoC.
- If the loader answers as a different SoC, the write is refused, and the refusal shows both byte
  strings.

The refusals appear after the plan and before the confirmation prompt. You are never asked to
confirm a write that pyrographer already knows it will refuse.

## The web flasher

The same crate compiles to WebAssembly and runs in a browser as the web flasher, with nothing to
install. It runs on the same core, with the same verbs, the same write path and the same refusals.
The web flasher compiles, and is `[UNVERIFIED]` in a browser.

It needs a Chromium-based browser (Chrome, Edge, or Chromium itself) and a secure origin, either
HTTPS or `localhost`. On Windows, the board needs a WinUSB driver, as it does for the native
tools.

### Board permissions

A web page cannot scan a USB bus. The first time, you pick a board from the browser's own device
chooser, which opens only in response to your click. The chooser returns only the board you picked,
and grants the page lasting permission for it.

After that, the list shows the boards you have already allowed. A board you have never picked on
this page does not appear, however it is plugged in. The permission outlives the tab, so the list
survives a reload. The page reads the list on load, after you allow a new board, and on
*Refresh*. You can revoke a permission in your browser's site settings.

A dump streams straight to a file you choose, so a 58 GiB eMMC does not have to fit in the tab.

### Confirming by picking again

To confirm a write, pick the destination again in the browser's chooser. A browser gives a page
a device object rather than a bus address, so there is no short, stable string to type. Only your
click can open the chooser, so the page cannot make the pick for you. A row in the remembered list
cannot serve as confirmation, because the page draws those rows itself.

### Serial ports

In the web flasher, the StarFive recovery and serial console sections replace the port field with a
*Choose port...* button. It opens the browser's port chooser. A page cannot be given a port path, as
it cannot be given a bus address.

The port chooser is unfiltered, unlike the board chooser. The device plugged into your machine is
a USB-serial adapter, whose vendor ID names FTDI or WCH and never the board. The window names a
port by the adapter's USB IDs, such as "USB-serial adapter 0403:6001". To confirm a recovery,
pick the port again, as you do for a write.

### Ingenic bootstrap in the browser

The Ingenic bootstrap runs in the web flasher too, with one extra step. After the bootstrap, choose
the board again in the browser's chooser, as you did for the boot ROM. A browser grants permission
per device, and the DFU gadget your board comes back as is a different device. Ingenic puts the mode
in the product ID, so the gadget enumerates at `4d44`, apart from the boot ROM's own product ID. The
permission you granted the boot ROM does not carry over, so the DFU device does not appear in the
list until you choose it.
