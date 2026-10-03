# Command line

The pages in this part cover the command-line tool, one group of commands to a page. Each
section gives what to type, what it prints, and what it refuses. This page covers finding a
device and naming one.

## Listing devices

Connect a board in its boot or recovery mode, then list the detected devices:

```sh
pyrographer list
```

```text
2207:350e  loader        bus 003 address 12  (bcdUSB 0201)
```

`list --blocks` also lists the machine's own disks, which [Block devices](block-devices.md)
covers.

`list` needs no permission to open a device, because it reads everything it prints from
descriptors the operating system has already cached. Discovery scans by vendor ID, so it finds
any Rockchip SoC in device mode, including a part pyrographer has no entry for.

The mode column is a claim, read from the low bit of `bcdUSB`. An odd flag means a loader, and
pyrographer trusts it. An even flag means either the BootROM or a loader that does not set the
flag, such as the RK3576 SPL loader.

The device-bound commands settle an even flag by asking. They open the board and send one probe
command, which a running loader answers. A BootROM presents the same bulk endpoints with nothing
serving them, so it fails the probe. The command then refuses, names the mode as maskrom, and
points to [`db`](bootstrapping.md#bringing-a-maskrom-board-to-loader-mode).

## Choosing between boards

If one board is connected, every device-bound command acts on it without being told which. If
more than one is connected, name the board with `--device` and the bus and address that `list`
prints:

```sh
pyrographer list
```

```text
2207:350e  loader        bus 003 address 12  (bcdUSB 0201)
2207:350b  loader        bus 003 address 14  (bcdUSB 0201)
```

```sh
pyrographer info --device 003:14
```

The bus and address distinguish two identical boards, which share a product ID.

`--device` also names a block device by its node, as in `--device /dev/sdb`. The leading slash
distinguishes the two forms. pyrographer never chooses a block device by default, so `--device`
is the only way to select one.
