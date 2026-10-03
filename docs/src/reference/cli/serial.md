# Serial lines

Three commands work over a serial line rather than over USB. `recover` writes a bootloader to a
StarFive board. `console` watches what a board prints on its serial console, and `uboot` drives a
bootloader prompt.

## Recovering a StarFive board

StarFive's JH7110 boards, such as the VisionFive 2 and the Milk-V Mars CM, recover over a serial
line. Their BootROM has no USB, and acts as an XMODEM receiver. The board therefore does not
appear in `list`, and takes no `--device`. Strap the board into UART recovery, power it on, and
name the serial port with `--port`.

`recover` uploads StarFive's recovery agent into SRAM. It then writes the bootloader through the
agent's menu, to QSPI NOR flash or to eMMC:

```sh
pyrographer recover --port /dev/ttyUSB0 --target flash \
    --agent jh7110-recovery-20230322.bin --spl u-boot-spl.bin
```

```text

This will recover a StarFive JH7110 board over /dev/ttyUSB0, writing to QSPI NOR flash.

  agent        174.50 KiB (uploaded into SRAM first)
  SPL          145.00 KiB (agent menu option 0, to QSPI NOR flash)

Warning: this write is not read back. The recovery protocol cannot read
flash. Each block's acknowledgment confirms that the board received it,
not that the flash holds it.
Nothing here can be undone.
Proceed? [y/N]
```

The whole recovery is `[UNVERIFIED]` against hardware.

At least one of `--spl` and `--uboot` is required:

- `--spl` takes a raw `u-boot-spl.bin`, and `recover` wraps it in the 1024-byte StarFive header.
- `--uboot` takes a U-Boot FIT payload and sends it unchanged.

Give both to write the whole boot chain, SPL first. `--dry-run` prints the plan and stops
without opening the port, and `--yes` skips the confirmation, as they do for `flash`.

**`--target` must name the medium the board boots from.** An SPL for NOR flash and one for eMMC
differ in their header. Sending one to the other's slot leaves the board unable to boot.
`recover` builds the header for the medium you name.

**The write is not read back.** The recovery protocol cannot read flash. The receiver
acknowledges only that it received the bytes, which does not prove the flash holds them. Every
other write in pyrographer reads each window back. The plan states this before you confirm, and
`recover` repeats it at the end:

```text
Recovery complete. The transfer was acknowledged, but this board's write cannot be read back.
Power off, return the boot strap to normal, and power on.
```

The recovery agent's menu also offers to burn OTP fuses. No option of `recover` reaches that
entry, because burning a fuse is permanent.

On Linux, opening a serial port requires your user to be in the `dialout` group, or in `uucp` on
some distributions. If the open is refused, pyrographer says so. The window has the same recovery
flow, as [Recovering a StarFive board](../gui.md#recovering-a-starfive-board) describes.

## Watching a serial console

`console` watches a board's serial console for text you name, such as the verdict of a self-test
the board runs at boot. Give the port, the text that means the board worked (`--expect`), and the
text that means it did not (`--or-fail`):

```sh
pyrographer console --port /dev/ttyUSB0 --expect 'selftest: PASS' --or-fail 'selftest: FAIL'
```

```text
[    0.000000] Booting Linux ...
selftest: PASS

Saw "selftest: PASS" on /dev/ttyUSB0.
```

The console watch is `[UNVERIFIED]` against hardware.

`console` writes the transcript to standard error as it arrives, and the verdict to standard
output. A script can read the verdict and log the transcript. If an `--or-fail` pattern
appears, `console` exits non-zero. Its message is then a plain sentence rather than an `error:`,
because the board reported the failure and pyrographer ran correctly.

Both options can repeat. `console` reports whichever pattern the board printed first, in
whatever order the options were given. If a board prints `FAIL` at stage one and `PASS` at stage
two, `console` reports the `FAIL`.

Patterns match as bytes, with no line splitting and no normalization. A trailing space is part
of the pattern. To write a byte that your shell removes or interprets, use one of these escapes:

- `\n`, `\r` and `\t`
- `\0`
- `\\`
- `\xNN`, a byte in hex

`console` refuses an unknown escape as it reads the option, so a typo cannot become a pattern
that never matches.

`--reads <n>` sets how long `console` waits, counted in reads rather than seconds. On a quiet
line, each read waits about a second. It returns as soon as bytes arrive. `--baud <rate>` sets
the line's baud rate, which defaults to 115200.

If no pattern matches within the read budget, `console` reports that the pattern did not appear.
It does not claim the board failed. The cause can be a failure, a slow boot, a wrong baud rate,
or a console on another UART:

```sh
pyrographer console --port /dev/ttyUSB0 --expect PASS --reads 5
```

```text
error: protocol error: nothing matching "PASS" arrived on the console within 5 reads. Recent
console output:
U-Boot SPL 2026.04
```

`console` reads only what a board prints by itself. It does not drive a login prompt, because a
login needs credentials and pyrographer handles none.

## Driving a U-Boot prompt

`uboot` drives a U-Boot prompt, the last stage before an operating system, over the same serial
port. Every form first sends a bare newline to interrupt autoboot. That stops a countdown, and on
a board already at a prompt it only produces another prompt.

The `uboot` driver is `[UNVERIFIED]` against hardware.

### Starting a gadget

`--gadget` starts a USB gadget at the U-Boot prompt, which exposes the board's flash over USB.
It completes the RAM-boot that `db` starts, once `db` has put a full mainline U-Boot into DRAM
over USB. With `--gadget rockusb`, the board returns to `list` as a loader:

```sh
pyrographer uboot --port /dev/ttyUSB0 --gadget rockusb
```

```text
Hit any key to stop autoboot:  2
=>
At the prompt.
rockusb 0 mmc 0

Ran `rockusb 0 mmc 0`. The gadget is running, so the prompt does not return.
Run `pyrographer list` to find the board on USB.
```

A running gadget holds the console and never returns to the prompt. `uboot` therefore treats a
prompt that does not come back as success. If the prompt does come back, the command has
returned, for example on an unknown command or a missing device. `uboot` then reports what
U-Boot printed as the failure.

`--gadget ums` starts U-Boot's USB mass-storage gadget instead. The board's flash then appears in
`list --blocks` as a disk, and the [block-device verbs](block-devices.md) read and write it. On an
RK3576, `dd` read the whole eMMC through this gadget, past the 32 MiB point where rkbin's loader
returns constant fill.

Mainline U-Boot's rockusb gadget does not answer `K_FW_READ_FLASH_INFO`, so `info`, `partitions`
and `--partition` fail against it. A `dump` by LBA requests no flash information, so this
failure does not affect it. Whether that gadget reads past the 32 MiB point is `[UNVERIFIED]`.

`--gadget-dev` names the block device the gadget exposes, and defaults to `mmc:0`. If the gadget
is not on USB controller 0, use the form `<controller>:<interface>:<index>`.

### Overriding the boot order

`--boot-from` sets what the board boots from, for one boot:

```sh
pyrographer uboot --port /dev/ttyUSB0 --boot-from mmc0
```

```text

This will change what the board on /dev/ttyUSB0 boots from, for one boot.

  boots from now   mmc1 mmc0 usb0 pxe dhcp
  would boot from  mmc0

Note: the change is not saved. U-Boot keeps its environment in RAM until
`saveenv` writes it to storage, and the override never sends `saveenv`. The
board's own boot order is untouched, and the next reset restores it. The
board boots immediately after the order is set.
Proceed? [y/N]
```

**The override is never saved.** pyrographer has no `saveenv` operation at all. The board's own
boot order is untouched in every case, and the next reset restores it.

To build the plan, `uboot` asks the board: it runs `printenv boot_targets` and shows the answer
beside the new order. `--dry-run` asks that question and stops.

The confirmation is a plain yes, not a typed coordinate, because an override has no second board
and destroys nothing.

### Running one command

`--cmd` types one command at the prompt and prints U-Boot's response:

```sh
pyrographer uboot --port /dev/ttyUSB0 --cmd 'mmc info'
```

```text
Device: mmc@2a330000
Manufacturer ID: 15
Capacity: 58.2 GiB
```

**`--cmd` is ungated.** U-Boot runs exactly what you type, `saveenv` included, with no plan and
no confirmation. A `saveenv` writes U-Boot's environment to the board's storage. `--cmd` is the
one command in pyrographer that works this way, and its help text says so.

`--prompt` names the prompt of a board whose prompt is not `=> `, such as `U-Boot> `, or whatever
`CONFIG_SYS_PROMPT` was built with. The trailing space is part of the prompt. pyrographer keeps
no catalog of board prompts, so give the one the transcript shows.

The window has the same console flow, as [The serial console](../gui.md#the-serial-console)
describes.
