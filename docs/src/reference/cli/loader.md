# Asking a loader

The commands on this page ask a running Rockchip loader about itself and its flash. None of them
writes flash. `chipver`, `storage` and `reset` refuse a block device by name, because a disk runs
no loader.

## Reading flash geometry

`info` reports the loader's flash ID and the size of the flash:

```sh
pyrographer info
```

```text
Device:       2207:350e (loader)
Flash ID:     45 4d 4d 43 20
Flash size:   58.24 GiB (122142720 sectors of 512 bytes)
```

## Reading the chip version

`chipver` asks the running loader which SoC it is on, and prints the reply as received:

```sh
pyrographer chipver
```

```text
Device:       2207:350e (loader)
Chip version: 16 bytes
  hex         36 37 35 33 00 00 00 00 00 00 00 00 00 00 00 00
  ascii       6753............
```

This reply is from an RK3576: the SoC's four ASCII digits byte-reversed, then zeros. An RK3588
answers `38 38 35 33` and twelve bytes of `0xff`. `chipver` sends nothing that changes the
device, so you can run it on any board in loader mode.

Every write is gated on this reply. The [wrong-loader gate](flash.md#the-wrong-loader-gate)
compares the whole reply with the exact bytes a real board returned, and decodes nothing. The
two replies above differ after their digits, which is why the gate compares all 16 bytes. A USB
product ID names only a family, so the loader is the one authority on which SoC it runs on.

To pin a new SoC, run `chipver` on a board whose SoC you know. The reply and the SoC together
make a new entry in core's `soc` module.

## Reading the loader's capabilities

`capability` asks the loader which features it serves:

```sh
pyrographer capability
```

```text
Device:       2207:350b (maskrom)
Capability:   3f 07 00 00 00 00 00 00
  yes direct LBA
  yes vendor storage
  yes first 4M access
  yes read LBA
  yes read COM log
   no read IDB config
   no read secure mode
  yes new IDB
  yes switch storage

This loader set bits pyrographer has no name for: 10 04 00 00 00 00 00 00
```

This reply is from the usbplug loader on an RK3588 board. That loader keeps the even `bcdUSB`
flag, so the heading reads maskrom although a loader answered, as
[Listing devices](index.md#listing-devices) describes.

The flags are the loader's own account of itself, and can disagree with what pyrographer
implements. A loader that does not set `read LBA` does not serve the read path, however well the
host implements it.

Writing an ID block is gated on `new IDB`, as
[Writing the ID block alone](firmware.md#writing-the-id-block-alone) describes. Nothing else is
gated on these flags. pyrographer reports a bit it has no name for, rather than ignoring it.

## Reading the storage medium

`storage` asks the loader which storage medium is active:

```sh
pyrographer storage
```

```text
Device:       2207:350e (loader)
Storage:      eMMC
```

A rockusb loader addresses one storage medium at a time, and every LBA is an offset into that
medium. On a board with both an eMMC and a SPI NOR, sector 16384 names two different places.

`storage` reports the medium and leaves it as it is. Like `chipver` and `capability`, it sends
nothing that changes the device.

## Rebooting and reset modes

`reset` reboots the board:

```sh
pyrographer reset
```

```text
The board is rebooting.
```

`--mode` chooses what the board does once it has acknowledged the command, by setting the
command's subcode:

| `--mode` | What the board does |
| -------- | ------------------- |
| `reset` | Reboots. The default, and the only mode a board has answered. |
| `msc` | Reboots into USB mass storage. The host operating system then owns it as a block device, and rockusb does not answer. `[COMMUNITY]` |
| `poweroff` | Powers off instead of rebooting. `[COMMUNITY]` |
| `maskrom` | Reboots into maskrom, the mode a shorted pin otherwise reaches. `db` then brings a loader back. `[COMMUNITY]` |

```sh
pyrographer reset --mode maskrom
```

```text
maskrom: this subcode comes from the reference tools, which agree on it, and is untried on a board. It writes no flash. [COMMUNITY]
The board is rebooting into maskrom, where no loader runs. Upload a loader to return it to loader mode.
Run `pyrographer db --loader <file>` to upload a loader.
```

The three tagged modes use the reference tools' subcodes, which agree with one another and are
untried on a board. None of them writes flash, so none is behind the write gate. Before it
sends one of them, `reset` prints a caution that the mode is untried and writes no flash. Each
of these modes ends the session where the verbs cannot follow, and `reset` says so once the
board has acted. `reset` refuses a mode name outside these four before it opens a device.
