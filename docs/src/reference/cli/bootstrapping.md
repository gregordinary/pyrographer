# Bootstrapping a board

A board running its on-chip boot code serves no flash commands. The commands on this page upload
code that does serve them, over the vendor's own download protocol. `db` uploads to a Rockchip
BootROM, and `usbboot` to an Ingenic boot ROM. Neither command writes flash. A StarFive board's
BootROM has no USB, and `uartboot` boots it into U-Boot over its serial line instead, as
[Serial lines](serial.md#booting-a-starfive-board-into-u-boot) describes.

## Bringing a maskrom board to loader mode

`db` brings a Rockchip board from maskrom mode, where the BootROM runs, to loader mode. It takes
a Rockchip loader container, rkbin's `*_loader.bin` for the SoC, and uploads it over the maskrom
download-boot protocol.

`list` shows the board in maskrom mode:

```sh
pyrographer list
```

```text
2207:350e  maskrom       bus 003 address 12  (bcdUSB 0200)
```

`db` uploads the loader, and the board comes back in loader mode:

```sh
pyrographer db --soc rk3576 --loader rk3576_spl_loader_v1.12.108.bin
```

```text
This loader's chip field holds 36 37 35 33 "6753" (rk3576).
Uploading 420.01 KiB...
  100.0%  420.01 KiB of 420.01 KiB  455.10 KiB/s
Uploaded 420.01 KiB in 0.9s.
Loader uploaded. A new Rockchip device appeared at 003:13. `list` can still report it as
maskrom, because this loader keeps the bcdUSB flag even. The next command sent to it checks
that a loader answers.
```

`db` parses the file before it opens any device, and refuses a file that is not an RKBOOT
loader. It also refuses a board that is not in maskrom mode. A Rockchip firmware package
(`update.img`) carries a loader, and `--loader` takes the package itself, as
[A board in maskrom](firmware.md#a-board-in-maskrom) shows.

The container names the SoC it was built for, and `db` prints that claim first. With `--soc`,
`db` checks the claim, and refuses a file built for another SoC before it uploads a byte.
Without `--soc`, `db` prints the claim and checks nothing.

This check is weaker than the [wrong-loader gate](flash.md#the-wrong-loader-gate) on a write.
Whoever built the file wrote the claim, so the check catches a wrong file chosen by mistake, and
not a false claim. It is the only check available, because a maskrom board does not answer `chipver`.

When an upload appears to go wrong, two behaviors matter. Both are measured on an RK3576:

- **A corrupt upload fails silently.** The BootROM acknowledges every transfer of a bad section.
  It checks the section's trailing CRC after the last chunk, and discards a bad section without
  reporting an error. The failure surfaces one stage later, as an upload that stalls. A BootROM
  left with a partial upload also refuses a fresh one. Power-cycle the board and put it back
  into maskrom before you retry.
- **The board re-enumerates slowly.** After the final stage, the loader tears down the maskrom
  USB device. The board comes back as a new device at a new address, somewhat over three
  seconds later. `db` watches for it for ten seconds. If the board has not reappeared by then,
  `db` tells you to run `list`, and reports no error.

The board that comes back can still carry an even `bcdUSB` flag, because the RK3576 SPL loader
never sets it. The verbs resolve that by asking, as [Listing devices](index.md#listing-devices)
describes.

### Raw stages

Mainline U-Boot's `CONFIG_ROCKCHIP_MASKROM_IMAGE` has binman emit the download-boot stages as
two bare files:

- `u-boot-rockchip-usb471.bin`, the DRAM init
- `u-boot-rockchip-usb472.bin`, the SPL and the full FIT after it

`db` sends those in place of a container:

```sh
pyrographer db --code471 u-boot-rockchip-usb471.bin --code472 u-boot-rockchip-usb472.bin
```

The result is a mainline U-Boot running entirely from RAM. Because nothing is written to flash,
the board's storage can be untouched, broken or not yet trusted. From that prompt, U-Boot's
gadgets hand the flash to the host, and its distro boot can test-boot a system before it is
installed.

You can give one stage or both, and `db` always sends stage 471 first. The chunking, the pad rule
and the trailing CRC are the same as for a container. A container's sections are these same
payloads, each under a name. Bare stages carry no container, so they name no SoC. `db` reports
that nothing checks which board they are for.

On an RK3576, a 3 MiB 472 stage packed in a `boot_merger` container and sent with `--loader`
RAM-boots to a U-Boot prompt. The raw form sends the same payloads, and is `[UNVERIFIED]`
against hardware. [RAM-booting U-Boot from maskrom](../../guides/maskrom-uboot.md) gives the
whole procedure. It includes the load-address check that catches a 472 stage the BootROM will
not run.

A board booted this way answers on its serial console, and does not return to the USB bus by
itself. At its U-Boot prompt, [`uboot`](serial.md#driving-a-u-boot-prompt) tells it to start a
gadget. The board then appears in `list` as a loader, or its flash appears in `list --blocks`
as a disk.

## Bringing an Ingenic board to DFU

`usbboot` brings an Ingenic XBurst board from its boot ROM to DFU mode. The Ingenic backend
targets the XBurst2 camera SoCs: the T31, T40 and T41. These SoCs are found in IP cameras such
as the Wyze Cam v3, which is built on a T31. The whole upload sequence is `[UNVERIFIED]` against
hardware.

`usbboot` uploads two stages: a DRAM-init SPL, then a DFU-capable U-Boot.

```sh
pyrographer usbboot --stage1 spl.bin --stage1-addr "$SPL_ADDR" \
    --stage2 u-boot.bin --stage2-addr "$UBOOT_ADDR"
```

Supply the two stages and their load addresses for your board. `usbboot` has no default
addresses. `--dram-settle-ms` sets how long `usbboot` waits
after stage 1 for DRAM to come up. The default is 2000 ms, a community figure. `[COMMUNITY]`

On success, `usbboot` prints the CPU info the boot ROM reported. The board then re-enumerates as
a DFU device, `a108:4d44`.

A DFU board addresses named regions, its DFU alt-settings, rather than a device-wide LBA.
`partitions` lists those regions, and `dump`, `verify` and `flash` aim at one with `--partition`.
They refuse a raw LBA. `clone` and the partition-table commands refuse a DFU board outright,
because each addresses a device-wide LBA. All three reads are `[UNVERIFIED]` on a DFU board. A
DFU write is built, and refuses at the [wrong-loader gate](flash.md#the-wrong-loader-gate)
because no Ingenic SoC is pinned.
