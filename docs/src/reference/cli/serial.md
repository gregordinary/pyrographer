# Serial lines

Four commands work over a serial line rather than over USB. `recover` writes a bootloader to a
StarFive board's flash, and `uartboot` boots a StarFive board into U-Boot without writing
anything. `console` watches what a board prints on its serial console, and `uboot` drives a
bootloader prompt.

## Recovering a StarFive board

StarFive's JH7110 boards, such as the VisionFive 2 and the Milk-V Mars CM, recover over a serial
line. Their BootROM has no USB, and acts as an XMODEM receiver. The board therefore does not
appear in `list`, and takes no `--device`. Strap the board into UART recovery, and name the
serial port with `--port`. `recover` then waits for you to power the board on.

`recover` sends StarFive's recovery agent to the BootROM. It then writes the board's QSPI NOR
flash through the agent's menu:

```sh
pyrographer recover --port /dev/ttyUSB0 --agent jh7110-recovery-20230322.bin \
    --spl u-boot-spl.bin.normal.out --uboot u-boot.itb
```

```text

This will write the boot flash of a StarFive JH7110 board over /dev/ttyUSB0.

  agent        161.41 KiB, sent to the BootROM first
  SPL          137.72 KiB at 0x0, agent menu entry 0, its header checked
  U-Boot       1.24 MiB at 0x100000, agent menu entry 2

The agent writes a backup copy of the SPL at 0x200000, inside the U-Boot region. U-Boot is written after it and overwrites that copy, as StarFive's own procedure does.

Warning: this write is not read back. The recovery protocol cannot read
flash. Each block's acknowledgment confirms that the board received it,
and the agent's verdict that its write finished, not that the flash
holds it.
Nothing here can be undone.
Proceed? [y/N]
```

The whole recovery is `[UNVERIFIED]` against hardware.

`recover` takes three files:

- `--agent` takes the recovery agent, a `jh7110-recovery-*.bin` from StarFive's Tools
  repository. `recover` checks its header, and that it carries the agent's menu.
- `--spl` takes the SPL, as a raw `u-boot-spl.bin` or as a `u-boot-spl.bin.normal.out`.
  `recover` adds the 1024-byte StarFive header to a raw file, and checks the header a
  `.normal.out` already carries.
- `--uboot` takes the U-Boot payload, a FIT image such as `u-boot.itb`, and sends it unchanged.

At least one of `--spl` and `--uboot` is required, and `--spl` requires `--uboot` as well. The
agent writes a second copy of the SPL at 2 MiB, inside the U-Boot region. An SPL written alone
therefore breaks the U-Boot already on the board. With both files, the SPL is written first and
U-Boot second, as StarFive's own procedure does.

`recover` refuses a file larger than the agent writes: 1 MiB for the SPL with its header, and
15 MiB for U-Boot. `--dry-run` prints the plan and stops without opening the port. `--yes` skips
the confirmation, as it does for `flash`.

**`recover` types at the agent only when the agent asks.** Whenever the agent is not receiving a
file, it reads a line of input, and it reads stray bytes as typing. `recover` waits for the
agent's prompt before it types a menu choice, and for the agent to ask for a file before it sends
one. If the agent answers a block with text, `recover` stops at once.

**OTP fuses are never touched.** The agent's menu also offers to burn the SoC's OTP fuses, which
cannot be undone. `recover` never chooses that entry. If the fuse menu ever appears, `recover`
stops, sends nothing more, and tells you to power the board off.

**The write is not read back.** The recovery protocol cannot read flash. After each file the agent
reports `updata success` or `updata fail`, and `recover` reports a failure as an error. Neither
verdict proves that the flash holds the file. Every other write in pyrographer reads each window
back. The plan states this before you confirm, and `recover` repeats it at the end:

```text
Recovery complete. The agent reported every write as done. This board's write cannot be read back.
Power off, return the boot strap to normal, and power on.
```

`recover` writes the QSPI NOR flash and not the eMMC. The agent's eMMC entries write the SPL over
the start of the disk, where its partition table is. To write a board's eMMC, boot U-Boot in RAM
and write the eMMC as a disk, as the next section describes.

On Linux, opening a serial port requires your user to be in the `dialout` group, or in `uucp` on
some distributions. If the open is refused, pyrographer says so. The window has the same recovery
flow, as [Recovering a StarFive board](../gui.md#recovering-a-starfive-board) describes.

## Booting a StarFive board into U-Boot

`uartboot` brings a StarFive board up into U-Boot in RAM, and writes nothing. It sends an SPL to
the BootROM, and the SPL loads U-Boot over the same serial line by YMODEM. `uartboot` then stops
U-Boot at its prompt. It is the serial counterpart of `db`, which loads U-Boot into a Rockchip
board over USB.

```sh
pyrographer uartboot --port /dev/ttyUSB0 \
    --spl u-boot-spl.bin.normal.out --uboot u-boot.itb
```

```text

Booting U-Boot in RAM on a StarFive JH7110 board over /dev/ttyUSB0. Nothing is written to the board.

  SPL          137.72 KiB, sent to the BootROM, its header checked
  U-Boot       1.24 MiB, sent to the SPL by YMODEM

U-Boot is running in RAM, stopped at its prompt. Nothing was written to the board.
Run `pyrographer uboot --port /dev/ttyUSB0 --gadget ums` to hand its eMMC to this machine as a disk.
```

The SPL must be a mainline U-Boot SPL built with `CONFIG_SPL_YMODEM_SUPPORT`, which mainline's
VisionFive 2 configuration sets. With the board still strapped for UART recovery, that SPL loads
U-Boot from the serial line. StarFive's own SPL loads U-Boot from SPI flash instead, and
`uartboot` reports the device the SPL names. `--prompt` names a U-Boot prompt other than `=> `.

`uartboot` is `[UNVERIFIED]` against hardware.

### Writing a StarFive eMMC as a disk

A RAM-booted U-Boot built with `CONFIG_CMD_USB_MASS_STORAGE` hands the board's eMMC to this
machine as a disk. Mainline's VisionFive 2 configuration does not enable it, so U-Boot has to be
built with it. The [block-device commands](block-devices.md) then write the disk, and read back
every window they write:

1. Strap the board into UART recovery. Connect the serial adapter, and a USB cable from this
   machine to the board's USB device port.
2. Run `uartboot`, and power the board on.
3. Run `uboot --gadget ums` on the same port. If the eMMC is not `mmc:0`, name it with
   `--gadget-dev`. `uboot --cmd 'mmc list'` lists the board's MMC devices.
4. Run `list --blocks` to find the disk, and write it with `flash --device`.

The route is `[UNVERIFIED]` against hardware.

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
