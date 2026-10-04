//! pyrographer: the command-line tool for flashing and recovering embedded devices.
//!
//! A thin consumer of `pyrographer-core`. It parses a subcommand, calls the
//! matching verb, and renders the result to the terminal. Core never prints, so
//! this binary writes every line of output. That includes the progress events a
//! long verb emits, and the hint an error carries.

#![forbid(unsafe_code)]

use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::time::Instant;

use pyrographer_core::agent::{DfuAgent, FlashAgent, RockusbAgent};
use pyrographer_core::block::{self, BlockDevice};
use pyrographer_core::bootstrap::ingenic::{IngenicLoader, Stage};
use pyrographer_core::bootstrap::starfive::{self, UartBootRequest};
use pyrographer_core::codec::console as console_codec;
use pyrographer_core::codec::rkboot;
use pyrographer_core::codec::rkfw;
use pyrographer_core::codec::rockusb::{ResetMode, StorageMedium};
use pyrographer_core::codec::splhdr::Origin;
use pyrographer_core::console::{self, Seen};
use pyrographer_core::discovery::{self, DeviceInfo, Mode};
use pyrographer_core::fill::FillReport;
use pyrographer_core::firmware::{self, Package};
use pyrographer_core::image::{SyncReader, SyncWriter};
use pyrographer_core::layout::Layout;
use pyrographer_core::partition::{Partition, PartitionTable, TableFormat};
use pyrographer_core::progress::{Cancel, Progress};
use pyrographer_core::recovery::{self, RecoveryPlan, RecoveryRequest};
use pyrographer_core::soc::Soc;
use pyrographer_core::transport::{DEFAULT_BAUD, SerialTransport, UsbTransport};
use pyrographer_core::uboot::{BootPlan, Gadget, GadgetDevice, UBoot};
use pyrographer_core::verbs::{
    ClonePlan, FirmwarePlan, FirmwareWrite, LoaderCapability, ParamAuthorSource, ParamMedium,
    SegmentedPlan, TableAction, Touches, WritePlan,
};
use pyrographer_core::{Error, Result, verbs};

const HELP: &str = "\
pyrographer - flashing and recovery for embedded devices

USAGE:
    pyrographer <COMMAND> [OPTIONS]

    The command comes first. `-h`/`--help` prints this guide, and
    `-V`/`--version` prints the version.

COMMANDS:
    list [--blocks]               List connected boards and the mode each is in,
                                  and with --blocks, this machine's disks
    db --loader <file>            Upload a loader to bring a maskrom board to
                                  loader mode
    usbboot --stage1 <file>       Bootstrap an Ingenic XBurst boot-ROM board to
                                  DFU mode (see USBBOOT OPTIONS)
    info                          Report the flash geometry of the connected device
    chipver                       Ask the loader which SoC it is running on
    capability                    Ask the loader what it says it can do
    storage                       Ask which storage medium the loader is addressing
    partitions                    Print the device's partition table
    dump <lba> <sectors> <file>   Read sectors from the device into a file
                                  (refuses to overwrite <file> without --force)
    flash <lba> <file>            Write a file to the flash at <lba>
    clone                         Copy one board's whole flash onto another
    repair-table                  Rewrite a damaged GPT copy from the intact one:
                                  primary from backup, or backup from primary
                                  (needs --soc, see WRITE OPTIONS)
    repair-param                  Rewrite a damaged Rockchip parameter copy from an
                                  intact one, on raw NAND, which keeps several
                                  copies (needs --soc, see WRITE OPTIONS)
    author-param                  Write a fresh Rockchip parameter table from a
                                  layout (needs --soc, see AUTHOR OPTIONS)
    author-gpt                    Write a fresh GPT from a layout (needs --soc,
                                  see AUTHOR OPTIONS)
    write-idb --loader <file>     Write a loader's ID block at sector 64: the
                                  first stage the BootROM reads (needs --soc)
    firmware-info <file>          Read and check a Rockchip firmware package
                                  (update.img), and report what it holds
    flash-firmware <file>         Write a Rockchip firmware package: every
                                  partition image, the GPT its parameter
                                  describes, and the ID block (needs --soc)
    verify <lba> <file>           Compare the flash at <lba> against a file
    reset                         Reboot the connected device, or end its session
                                  another way (see RESET OPTIONS)
    recover                       Write a StarFive JH7110 board's boot flash over
                                  a serial line (see RECOVER OPTIONS)
    uartboot                      Boot a StarFive JH7110 board into U-Boot over a
                                  serial line, writing nothing (see UARTBOOT
                                  OPTIONS)
    console                       Watch a serial console for the text that says a
                                  board worked, or that it did not (see CONSOLE
                                  OPTIONS)
    uboot                         Drive a U-Boot prompt over a serial line: start a
                                  rockusb or mass-storage gadget, boot from somewhere
                                  else for one boot, or type one command (see UBOOT
                                  OPTIONS)
    help                          Show this help

OPTIONS:
    --device <bus>:<address>      Which board to act on, as `list` prints it.
                                  Needed only when more than one is connected.
    --device <node>               Which block device to act on: a device node,
                                  as in /dev/sdb (see BLOCK DEVICES). Never
                                  optional: a disk is acted on only when it is
                                  named.

    --force                       For dump: overwrite the output file if it
                                  already exists (otherwise dump refuses).

    --loader <file>               The loader blob `db` uploads: an rkbin
                                  `_loader.bin` for the board's SoC. The SoC
                                  knowledge lives in this file, not in pyrographer.
                                  `db` prints the SoC the container says it was
                                  built for before uploading anything. `db` and
                                  `write-idb` also take a firmware package
                                  (update.img), and use the loader inside it.

    --code471 <file>              Raw download-boot stages for `db`, in place of
    --code472 <file>              --loader: the bare 471 (DRAM init) and 472
                                  (loader) payloads with no container around
                                  them, as mainline U-Boot's binman emits them
                                  (u-boot-rockchip-usb471.bin / -usb472.bin).
                                  Give one or both. 471 is always sent first.

    --partition <name>            Act on the partition the device's own table
                                  calls <name>, instead of on an LBA. Takes the
                                  place of <lba> (and, for dump, of <sectors>):

                                      pyrographer dump   --partition boot boot.img
                                      pyrographer flash  --partition boot boot.img
                                      pyrographer verify --partition boot boot.img

DB OPTIONS (db):
    --soc <soc>                   The SoC the board is, like rk3576. An rkbin
                                  container names the SoC it was built for, and
                                  db checks that name against this one. A file
                                  built for another SoC is refused before a byte
                                  is uploaded. Without it the container's claim
                                  is printed and nothing is checked.
                                  It is a weaker gate than the write path's.
                                  Whoever built the file wrote the claim, so the
                                  gate catches a wrong file, not a false claim. A
                                  maskrom board answers no chipver and its SRAM
                                  cannot be read back, so this is the only check
                                  there is. Bare --code471/--code472 stages carry
                                  no container and so name no SoC. Nothing is
                                  checked for those.

BLOCK DEVICES:
    An SD card in a reader, or a board that has come up as mass storage, is a
    target for `info`, `partitions`, `dump`, `flash`, `verify`, `clone` and the
    table commands. They run the same plan and the same mandatory
    window-by-window read-back as on a board. Name it by its node:
    --device /dev/sdb.

    The operating system also owns a block device. pyrographer asks the kernel
    for exclusive use of it. The kernel refuses if anything else holds it, such
    as a mounted filesystem, an active swap, or a volume stacked on it, and names
    the remedy. Reads and read-backs bypass the page cache, so what comes back is
    the device's answer and not a page the host is holding.

    Every disk the running system rests on is refused outright, transitively
    and with no override: `/` is routinely a volume over a container over a
    partition, and the disk underneath all of it is refused too.

    There is no loader on a disk, so --soc is refused here. The guards above
    protect the write instead. `list --blocks` needs no privilege. Every other
    command opens the device, even to build a plan, and opening it needs root or
    membership of the `disk` group. Linux only.

WRITE OPTIONS (flash, clone, repair-table, repair-param, author-param, author-gpt,
               write-idb, flash-firmware):
    --soc <soc>                   The SoC the board being written is, like rk3576.
                                  Every write is gated on the running loader's own
                                  answer matching it, byte for byte. Without it,
                                  the plan is shown and the write is refused. For
                                  clone, this names the destination.
    --dry-run                     Show what the write would touch, and stop
    --yes                         Do not ask for confirmation

RESET OPTIONS (reset):
    --mode <mode>                 What the board does once it acknowledges. The
                                  mode is the reset command's subcode.
                                  reset     Reboot. The default, and the only mode
                                            a board has answered.
                                  msc       Reboot into USB mass storage, where the
                                            host operating system owns the board as
                                            a block device and rockusb does not
                                            answer. [COMMUNITY]
                                  poweroff  Power off rather than reboot.
                                            [COMMUNITY]
                                  maskrom   Reboot into maskrom. `db` then uploads
                                            a loader to bring it back.
                                            [COMMUNITY]
                                  The three tagged modes are the reference tools'
                                  subcodes, untried on a board. None writes flash.

AUTHOR OPTIONS (author-param, author-gpt):
    --medium emmc|nand            author-param only: where the parameter copies go,
                                  and what a layout's offsets count from. eMMC keeps
                                  one copy at sector 0x2000, and raw NAND keeps
                                  several from sector 0. A GPT is absolute and takes
                                  no medium.
    --layout <file>               A native-format layout: one partition per line,
                                  `name first_lba sectors [type] [uuid=<GUID>]`, hex
                                  or decimal, `-` to grow to the end, `#` for a
                                  comment. For author-gpt the type, the uuid=, and a
                                  `disk-guid <GUID>` line are read. author-param
                                  ignores them.
    --mtdparts <file>             A file holding a board's own `mtdparts=` line.
    --from-block <file>           author-param only: an existing parameter's whole
                                  text, framed verbatim, with FIRMWARE_VER and every
                                  other key kept.
                                  author-param takes one of --layout, --mtdparts or
                                  --from-block. author-gpt takes one of --layout or
                                  --mtdparts.

CLONE OPTIONS:
    --from <bus>:<address>        The board to copy (read only)
    --to <bus>:<address>          The board to overwrite

RECOVER OPTIONS (StarFive JH7110, over a serial line):
    --port <path>                 The serial port, e.g. /dev/ttyUSB0 or COM3
    --agent <file>                The recovery agent (jh7110-recovery-*.bin)
    --spl <file>                  The SPL to write: u-boot-spl.bin, which is
                                  headered here, or u-boot-spl.bin.normal.out
    --uboot <file>                The U-Boot payload to write, sent as-is
    --dry-run                     Show what the recovery would write, and stop
    --yes                         Do not ask for confirmation

    recover writes the board's QSPI NOR flash: the SPL at 0x0 and U-Boot at
    0x100000. It writes at least one of --spl or --uboot, and an SPL needs
    --uboot beside it, because the agent also writes a copy of the SPL inside
    the U-Boot region. The board must be strapped into UART recovery. recover
    types at the agent's menu only when the agent asks, and reads the agent's
    verdict after every file. Unlike every other write, a StarFive recovery is
    not read back, because the agent cannot read flash. OTP fuse burning is
    never offered.

UARTBOOT OPTIONS (StarFive JH7110, over a serial line):
    --port <path>                 The serial port, e.g. /dev/ttyUSB0 or COM3
    --spl <file>                  A mainline SPL built with
                                  CONFIG_SPL_YMODEM_SUPPORT: u-boot-spl.bin, or
                                  u-boot-spl.bin.normal.out
    --uboot <file>                The u-boot.itb that SPL loads
    --prompt <text>               U-Boot's prompt (default \"=> \")

    uartboot sends the SPL to the board's BootROM and U-Boot to the SPL, and
    stops U-Boot at its prompt. Nothing is written to the board, which must be
    strapped into UART recovery. `uboot --gadget ums` on the same port then hands
    the board's eMMC to this machine as a disk, and the block-device commands
    write it with a read-back of every window.

CONSOLE OPTIONS (watch a serial line):
    --port <path>                 The serial port, e.g. /dev/ttyUSB0 or COM3
    --baud <rate>                 The console's rate (default 115200)
    --expect <pattern>            Text that means it worked. Repeatable.
    --or-fail <pattern>           Text that means the board reported a failure.
                                  Repeatable. A match exits non-zero.
    --reads <n>                   How many reads to spend waiting (default 30). Each
                                  read waits about a second on a quiet line, and
                                  returns as soon as bytes arrive.

    Patterns are matched as bytes, with no line splitting and no normalization, so
    a trailing space is part of the pattern. To write a byte the shell would remove
    or interpret, use \\n, \\r, \\t, \\0, \\\\, or \\xNN. The pattern the board
    printed first is the one reported, whatever order the patterns were given in.

    console reports what appeared and does not assert a result. If nothing matches
    within the budget, console reports that the pattern did not appear. The cause
    can be a failure, a slow boot, a wrong baud rate, or a console on another UART.

UBOOT OPTIONS (drive a bootloader prompt over a serial line):
    --port <path>                 The serial port the board's console is on
    --baud <rate>                 Its rate (default 115200)
    --prompt <text>               The prompt to match (default \"=> \"). Boards
                                  use different prompts, and pyrographer keeps no
                                  catalog of them.
    --reads <n>                   How many reads to spend on each wait (default 30)

    --gadget rockusb|ums          Start a U-Boot gadget, handing the board's flash
                                  to the USB side: rockusb answers as a loader in
                                  `list`, and ums (USB mass storage) appears as a
                                  disk in `list --blocks`. This completes the
                                  RAM-boot that `db` or `uartboot` starts.
    --gadget-dev <if>:<index>     Which block device the gadget exposes (default
                                  mmc:0). Write <controller>:<if>:<index> when the
                                  gadget is not on USB controller 0.

    --boot-from <targets>         Set U-Boot's boot_targets and boot, for this boot
                                  only. Nothing is saved: U-Boot keeps its
                                  environment in RAM until `saveenv` writes it, and
                                  --boot-from never sends `saveenv`. The next reset
                                  restores the board's own order. --dry-run asks the
                                  board what it boots from now, and stops. --yes
                                  skips the question.

    --cmd <line>                  Type one command at the prompt and print what came
                                  back. Warning: this is ungated. U-Boot runs
                                  whatever you type, `saveenv` included, with no
                                  plan and no confirmation.

    uboot takes exactly one of --gadget, --boot-from, and --cmd. Every form first
    sends a bare newline to interrupt autoboot. That stops a countdown, and on a
    board already at a prompt it only produces another prompt.

USBBOOT OPTIONS (Ingenic XBurst boot ROM -> DFU):
    --stage1 <file>               The DRAM-init stage (the SPL), uploaded first
    --stage1-addr <hex>           Where stage1 loads and runs, e.g. 0x80000000
    --stage2 <file>               The DFU-capable U-Boot, uploaded after stage1
    --stage2-addr <hex>           Where stage2 loads and runs
    --dram-settle-ms <n>          Wait after stage1 for DRAM to come up
                                  (default 2000)

    Supply both stages and both addresses for your board. usbboot has no
    default addresses. On success
    the board re-enumerates as a DFU device (a108:4d44). Run `list` to find it.
    The whole upload sequence is unverified against hardware.

console, uboot and uartboot take no --device. Name the serial port with --port,
as for recover. uboot completes the RAM-boot that db or uartboot starts. db loads
a U-Boot into DRAM over USB, and uartboot over the serial port. Either way the
board then answers on its serial port rather than on the bus.

Every device-bound command except db and usbboot needs a device in loader mode.
db needs one in maskrom, and usbboot one in an Ingenic boot ROM. Each of the two
brings a board to a usable mode.

clone needs both boards named, even when only two are connected. That choice
decides which board is overwritten, so clone does not infer it.

flash, clone, repair-table, repair-param, author-param, author-gpt, write-idb, and
flash-firmware overwrite a board. They read back every window they write, and
that cannot be turned off. Nothing they do can be undone.

flash-firmware writes a Rockchip firmware package (update.img) as one plan. It
reads the whole package first and checks it: the archive's checksum, the loader
the ID block is built from, and every hash the ID block's header records. Then it
writes each partition image into the partition the package's parameter names, a
GPT built from that parameter, and the ID block, last. A board in maskrom takes
`db --loader update.img` first. firmware-info makes the same checks with no
device.

write-idb builds the ID block from a loader container's flash stages, each placed
where the container's own RKNS header says, and writes it at sector 64. It writes
through a loader that claims NEW_IDB, which `capability` reports.

flash refuses a firmware package or a loader container written raw, because
neither boots that way.

repair-table rewrites whichever copy of a GPT is damaged from the intact one: a
damaged primary from the backup in the last sector, or a stale, damaged, or
missing backup from the primary. repair-param does the same for a Rockchip
parameter block, which raw NAND keeps several copies of. An eMMC keeps a single
copy, so a damaged one there has no other copy to rebuild from. Author a fresh
table instead. Both leave the intact copy untouched, and neither authors a table
from nothing.

author-param writes a fresh Rockchip parameter table from a layout you supply. A
layout builds a minimal block. --from-block frames an existing parameter's whole
text, keeping FIRMWARE_VER and the rest.

author-gpt writes a fresh GPT from the same native or mtdparts layout: a
protective MBR, a primary, and a backup at the end of the device. Each
partition's type comes from its type token (esp, linux, swap, or a raw type
GUID), and the default is Linux data. The disk GUID and each unique GUID are
synthesized, so authoring the same layout twice yields the same table. To pin a
specific GUID, add a `disk-guid` line or a `uuid=` attribute to the layout.

A write names its target in one of two ways, and they are not equally safe. An
<lba> is a number you supply. --partition names a partition in the device's own
table, which records where the partition ends. Only --partition can refuse an
image too big to fit. Either way, the plan names the partitions the write lands
in before anything is written.";

/// A run that did not succeed: a usage error, a core failure, or a finding.
///
/// The CLI is responsible for its own command line, and not for a device. The two
/// are separate variants. A mistyped sector count is therefore never reported as
/// an [`Error::Io`], with the hint for a device that does not answer.
#[derive(Debug)]
enum CliError {
    /// The command line was wrong: an unknown command, a missing or unparsable
    /// argument, or a request this build cannot carry out. Nothing was opened and
    /// nothing was sent.
    Usage(String),

    /// A device or a file failed, as reported by core. Only this variant carries
    /// a [`hint`](Error::hint).
    Core(Error),

    /// The command ran to a conclusion, and what it found is not success.
    ///
    /// Nothing malfunctioned. A `console` watch saw the pattern that means the far
    /// end reported a failure. That is a finding about the board, not a fault in
    /// pyrographer or in the request. It is an error only to a script, because the
    /// process exits non-zero. It prints as a plain sentence, with no `error:`
    /// prefix and no hint after it.
    Finding(String),
}

impl From<Error> for CliError {
    fn from(err: Error) -> Self {
        CliError::Core(err)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(message) => write!(f, "{message}"),
            CliError::Core(err) => write!(f, "{err}"),
            CliError::Finding(message) => write!(f, "{message}"),
        }
    }
}

/// The result of a subcommand: it ran, or it ended in one of the [`CliError`]
/// variants.
type Run = std::result::Result<(), CliError>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // A finding is not a fault: the command worked and the board answered
        // with the thing that was being watched for. It exits non-zero because a
        // script needs it to, and it says only what was seen.
        Err(CliError::Finding(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("error: {err}");
            // Only core diagnoses a device, so only core has a next step to
            // offer. A usage message is its own next step. Core's hint names the
            // action, and the line after it names the command that takes it.
            if let CliError::Core(err) = err {
                if let Some(hint) = err.hint() {
                    eprintln!("\nhint: {hint}");
                }
                if let Some(step) = error_next_step(&err) {
                    eprintln!("{step}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Run {
    let mut args = pico_args::Arguments::from_env();

    // `--version` and top-level `--help` are answered before the subcommand is
    // read, so they work with nothing after them.
    if args.contains(["-V", "--version"]) {
        println!("pyrographer {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = args
        .subcommand()
        .map_err(|e| CliError::Usage(format!("{e}\n\n{HELP}")))?;

    // `-h`/`--help` anywhere prints the guide -- including `pyrographer flash -h`,
    // which the per-subcommand parsers would otherwise have died on. Stripped here,
    // before dispatch, so it reaches every subcommand the same way.
    if args.contains(["-h", "--help"]) {
        println!("{HELP}");
        return Ok(());
    }

    match command.as_deref() {
        Some("list") => cmd_list(args),
        Some("db") => cmd_db(args),
        Some("usbboot") => cmd_usbboot(args),
        Some("info") => cmd_info(args),
        Some("chipver") => cmd_chipver(args),
        Some("capability") => cmd_capability(args),
        Some("storage") => cmd_storage(args),
        Some("partitions") => cmd_partitions(args),
        Some("dump") => cmd_dump(args),
        Some("flash") => cmd_flash(args),
        Some("verify") => cmd_verify(args),
        Some("reset") => cmd_reset(args),
        Some("clone") => cmd_clone(args),
        Some("repair-table") => cmd_repair_table(args),
        Some("repair-param") => cmd_repair_param(args),
        Some("author-param") => cmd_author_param(args),
        Some("author-gpt") => cmd_author_gpt(args),
        Some("write-idb") => cmd_write_idb(args),
        Some("firmware-info") => cmd_firmware_info(args),
        Some("flash-firmware") => cmd_flash_firmware(args),
        Some("recover") => cmd_recover(args),
        Some("uartboot") => cmd_uartboot(args),
        Some("console") => cmd_console(args),
        Some("uboot") => cmd_uboot(args),
        Some("help") => {
            println!("{HELP}");
            Ok(())
        }
        // No subcommand. With a clean command line that is a plain help request;
        // but a leading dash-argument (`--device 003:12 info`) makes `subcommand()`
        // stop before the verb, and silently printing help would then swallow
        // everything after it. So a bare invocation prints help, and a non-empty one
        // says the subcommand has to come first.
        None => {
            let left = args.finish();
            if left.is_empty() {
                println!("{HELP}");
                Ok(())
            } else {
                Err(CliError::Usage(format!(
                    "put the subcommand first, before any options: pyrographer <command> \
                     [options]\n\n{HELP}"
                )))
            }
        }
        Some(other) => Err(CliError::Usage(format!(
            "unknown command '{other}'\n\n{HELP}"
        ))),
    }
}

/// Refuse anything left on the command line that nothing read.
///
/// pico-args returns what it is asked for and leaves the rest unread. Without
/// this check, a stray argument is silently ignored. On a command that overwrites
/// a board, that changes what is written: `flash --partition boot 64 boot.img`
/// would take `64` as the file to write, and never mention `boot.img`.
fn no_more(args: pico_args::Arguments) -> std::result::Result<(), CliError> {
    let left = args.finish();
    if left.is_empty() {
        return Ok(());
    }

    let left: Vec<String> = left
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    Err(CliError::Usage(format!(
        "unexpected arguments: {}\n\n{HELP}",
        left.join(" ")
    )))
}

fn cmd_list(mut args: pico_args::Arguments) -> Run {
    let blocks_too = args.contains("--blocks");
    no_more(args)?;

    let devices = verbs::list()?;
    if devices.is_empty() {
        println!("No boards found.");
        // Said here and not always: somebody with a card reader plugged in and
        // no board has found nothing, and the answer to what they are looking
        // for is one flag away. Somebody who can see their board does not need
        // to be told about disks.
        if !blocks_too {
            println!("`list --blocks` also lists the disks this machine has.");
        }
    }
    for device in devices {
        println!(
            "{:04x}:{:04x}  {:<12}  bus {} address {}  (bcdUSB {:04x})",
            device.vendor_id,
            device.product_id,
            device.mode.name(),
            device.bus_id,
            device.device_address,
            device.bcd_usb,
        );
    }

    // Behind a flag, and deliberately. A board in maskrom or DFU is on this bus
    // because somebody put it there, and listing it is telling them what they
    // already did. Every disk on the machine is a different matter: the list
    // includes the one this is running from, and printing it beside a board with
    // no distinction is the beginning of the mistake this backend exists to
    // refuse.
    if !blocks_too {
        return Ok(());
    }
    let blocks = blockcli::list()?;
    if blocks.is_empty() {
        println!("\nNo block devices found.");
        return Ok(());
    }
    println!("\nBlock devices:");
    println!("{}", block_lines(&blocks));
    println!(
        "\nA block device is acted on only when it is named: --device <node>.\n\
         `[refused: running system]` cannot be opened at all, and has no override.\n\
         `[read-only: no write]` and `[no write]` still dump, verify and list partitions."
    );
    Ok(())
}

fn cmd_info(mut args: pico_args::Arguments) -> Run {
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    no_more(args)?;

    print!("{}", device.describe());

    let info = pollster::block_on(async {
        let mut agent = device.open().await?;
        verbs::info(&mut agent).await
    })?;

    if let Some(id) = &info.chip_id {
        let hex: Vec<String> = id.iter().map(|b| format!("{b:02x}")).collect();
        println!("Flash ID:     {}", hex.join(" "));
    }
    println!(
        "Flash size:   {} ({} sectors of {} bytes)",
        human_bytes(info.size_bytes),
        info.size_bytes / u64::from(info.sector_size),
        info.sector_size,
    );
    Ok(())
}

/// Print the device's partition table.
///
/// A device with no table is reported as a normal device, not as a failure. A
/// board holding a raw image, or a blank one, has no table and is not broken. A
/// table that is present and fails its own checksum is a failure, and the error
/// says what failed. An empty list in its place would hide a half-written table
/// from a person about to write to the board again.
fn cmd_partitions(mut args: pico_args::Arguments) -> Run {
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    no_more(args)?;

    print!("{}", device.describe());

    let (table, sector_size) = pollster::block_on(async {
        let mut agent = device.open().await?;
        let table = verbs::partitions(&mut agent).await?;
        Ok::<_, Error>((table, agent.sector_size()))
    })?;

    let Some(table) = table else {
        println!("\nThis device has no partition table.");
        return Ok(());
    };

    print!("{}", render_partitions(&table, sector_size));
    Ok(())
}

/// Render a partition table for a person deciding what to dump or overwrite.
///
/// The columns are the ones a flashing tool is asked for: a partition's name,
/// where it begins, and how far it runs. A GPT also records type and instance
/// GUIDs and attribute bits. None of those changes what a person does next, so
/// none of them is shown.
fn render_partitions(table: &PartitionTable, sector_size: u32) -> String {
    let mut out = String::new();

    // A table recovered from the backup leads with why: the partitions are good,
    // but the device's primary copy is damaged, and a person reading a table off
    // a board they are about to write to should know that before the list.
    if let Some(recovery) = &table.recovery {
        out.push_str(&format!(
            "\nWarning: the primary GPT is damaged. These partitions were recovered from\n         \
             {}.\n         Primary GPT: {}\n",
            recovery.recovered_from, recovery.primary_detail,
        ));
    }

    out.push_str(&format!(
        "\nPartition table: {} ({} partitions)\n\n  {:<20} {:>12} {:>12}  {}\n",
        table.format.name(),
        table.partitions.len(),
        "NAME",
        "FIRST LBA",
        "SECTORS",
        "SIZE"
    ));

    for partition in &table.partitions {
        out.push_str(&format!(
            "  {:<20} {:>12} {:>12}  {}\n",
            partition.name,
            partition.first_lba,
            partition.sectors,
            human_bytes(partition.sectors.saturating_mul(u64::from(sector_size))),
        ));
    }

    out.push_str(
        "\nA partition can be named instead of an LBA:  --partition <name>\n\
         The table records where each partition ends, so on a write, only a name\n\
         can refuse an image too big to fit in it.\n",
    );
    out
}

/// Ask the loader what SoC it is running on, and print its answer raw.
///
/// The bytes are printed and not decoded. The armed wrong-loader gate compares the
/// whole reply against pinned bytes, not a decoded shape. The raw bytes are what
/// pins a new SoC, and what the gate itself checks.
///
/// Both hex and ASCII are shown, because either can make a reply legible. A chip
/// identifier that reads as text shows in the ASCII column (an RK3576's reads
/// "6753", byte-reversed). An identifier that does not read as text shows in the
/// hex.
fn cmd_chipver(mut args: pico_args::Arguments) -> Run {
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    device.usb("chipver")?;
    no_more(args)?;

    print!("{}", device.describe());

    let version = pollster::block_on(async {
        let mut agent = device.open().await?;
        verbs::chip_version(&mut agent).await
    })?;

    println!("Chip version: {} bytes", version.len());
    println!("  hex         {}", hex(&version));
    println!("  ascii       {}", ascii(&version));
    Ok(())
}

/// Ask the loader what it says it can do, and print the flags it set.
///
/// The flags are the device's own account of itself, which is a separate question
/// from what pyrographer implements. They are printed as the loader's claims, and
/// nothing is gated on them. Nothing here writes.
fn cmd_capability(mut args: pico_args::Arguments) -> Run {
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    no_more(args)?;

    print!("{}", device.describe());

    let capability = pollster::block_on(async {
        let mut agent = device.open().await?;
        verbs::capability(&mut agent).await
    })?;

    let Some(capability) = capability else {
        println!("\nThis device does not answer a capability query.");
        return Ok(());
    };

    println!("Capability:   {}", hex(capability.raw()));

    let flags = [
        ("direct LBA", capability.direct_lba()),
        ("vendor storage", capability.vendor_storage()),
        ("first 4M access", capability.first_4m_access()),
        ("read LBA", capability.read_lba()),
        ("read COM log", capability.read_com_log()),
        ("read IDB config", capability.read_idb_config()),
        ("read secure mode", capability.read_secure_mode()),
        ("new IDB", capability.new_idb()),
        ("switch storage", capability.switch_storage()),
    ];
    for (name, set) in flags {
        println!("  {} {name}", if set { "yes" } else { " no" });
    }

    // A flag the table cannot name is a finding, not noise: the loader set a bit
    // pyrographer has no account of, and saying so is more use than hiding it.
    let unnamed = capability.unnamed_bits();
    if unnamed.iter().any(|byte| *byte != 0) {
        println!(
            "\nThis loader set bits pyrographer has no name for: {}",
            hex(&unnamed)
        );
    }
    Ok(())
}

/// Ask which storage medium the loader is currently addressing.
///
/// Every LBA the other verbs take is an offset into this medium. On a board with
/// more than one medium populated, the answer says which one a sector number
/// addresses. Nothing here writes or switches the medium. pyrographer reports
/// which medium is live and does not change it.
fn cmd_storage(mut args: pico_args::Arguments) -> Run {
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    device.usb("storage")?;
    no_more(args)?;

    print!("{}", device.describe());

    let medium = pollster::block_on(async {
        let mut agent = device.open().await?;
        verbs::storage_medium(&mut agent).await
    })?;

    match medium {
        Some(StorageMedium::Unknown(index)) => println!(
            "Storage:      index {index}, which pyrographer has no name for\n\n\
             The loader answered, and the answer is outside the table taken from the\n\
             reference tools. The index is what it reported."
        ),
        Some(medium) => println!("Storage:      {}", medium.name()),
        None => println!("\nThis device addresses no single storage medium."),
    }
    Ok(())
}

/// Render bytes as spaced hex.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render bytes as printable ASCII, standing in a dot for everything else.
///
/// It prints one character per byte, so it does not align column for column with
/// the hex line printed before it. It exists only to be read against that hex.
fn ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Where a verb acts: a place a person worked out, or a place the device names.
///
/// The two are not equally safe. An LBA is arithmetic a person did, and nothing
/// can check it. A name is resolved against the device's own table, which records
/// where the partition ends as well as where it begins. A name is therefore the
/// only form that can refuse an image too big to fit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// A raw logical block address.
    Lba(u64),
    /// A partition, by the name the device's table gives it.
    Partition(String),
}

/// Read a `--partition` option, if there is one.
///
/// It must be read before the positional arguments. pico-args takes free
/// arguments from the front of whatever is left, so an option still in the vector
/// would be read as a free argument.
fn partition_option(
    args: &mut pico_args::Arguments,
) -> std::result::Result<Option<String>, CliError> {
    args.opt_value_from_str("--partition")
        .map_err(|e| CliError::Usage(format!("--partition names a partition: {e}")))
}

/// Where `dump` reads from: an LBA and a count, or a partition, whose own extent
/// is both.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DumpTarget {
    /// A raw range: a first sector and how many.
    Range { lba: u64, sectors: u64 },
    /// A partition. It takes no sector count, because the device's table already
    /// records one.
    Partition(String),
}

/// What `dump` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct DumpArgs {
    target: DumpTarget,
    path: String,
}

/// Read `dump`'s arguments.
///
/// With `--partition`, the command takes neither an LBA nor a sector count. The
/// device's table records both, and a count a person restates is a count they can
/// get wrong.
///
/// A missing or unparsable argument is a usage error only. The command line is
/// wrong, no device has been opened, and no file has been created.
fn parse_dump(args: &mut pico_args::Arguments) -> std::result::Result<DumpArgs, CliError> {
    let target = match partition_option(args)? {
        Some(name) => DumpTarget::Partition(name),
        None => {
            let lba = args
                .free_from_str()
                .map_err(|e| CliError::Usage(format!("dump needs an LBA to start at: {e}")))?;
            let sectors = args
                .free_from_str()
                .map_err(|e| CliError::Usage(format!("dump needs a sector count: {e}")))?;
            DumpTarget::Range { lba, sectors }
        }
    };

    let path = args
        .free_from_str()
        .map_err(|e| CliError::Usage(format!("dump needs a file to write: {e}")))?;

    Ok(DumpArgs { target, path })
}

fn cmd_dump(mut args: pico_args::Arguments) -> Run {
    let selector = device_option(&mut args)?;
    let force = args.contains("--force");
    let DumpArgs { target, path } = parse_dump(&mut args)?;
    no_more(args)?;

    // **The output file is not created here.** `File::create` truncates, and a
    // dump that cannot find its board, or that names a partition the board does
    // not have, would otherwise have destroyed yesterday's image before it ever
    // touched the device. The host's files are guarded as carefully as the board's
    // flash: an existing file is refused up front unless `--force` says overwrite,
    // and the file itself is created only once the read is about to run.
    if !force && std::path::Path::new(&path).exists() {
        return Err(CliError::Usage(format!(
            "{path} already exists. Pass --force to overwrite it"
        )));
    }

    let device = choose_target(selector.as_ref())?;
    let mut render = ProgressRenderer::new("Reading", "Read");
    // The token is here so the seam is honest: the GUI flips it from a button,
    // and a signal handler would flip it here. The CLI has none yet, so Ctrl-C
    // still ends the process outright.
    let cancel = Cancel::new();

    let fill = pollster::block_on(async {
        let mut agent = device.open().await?;

        let (lba, sectors) = match &target {
            DumpTarget::Range { lba, sectors } => {
                // A DFU board addresses named regions, not a device-wide LBA, so a
                // raw range names nothing it can serve. Refused before the file is
                // created, pointing at the form that works.
                if !agent.caps().can_address_raw_lba {
                    return Err(raw_lba_unsupported("dump"));
                }
                (*lba, *sectors)
            }
            DumpTarget::Partition(name) => {
                // Resolved against the device's own table, and then said out
                // loud: a person who named the wrong partition should see the
                // range before the read runs, not work it out from the file
                // afterwards.
                let partition = verbs::find_partition(&mut agent, name).await?;
                println!(
                    "Partition:    {} (LBA {}, {} sectors, {})",
                    partition.name,
                    partition.first_lba,
                    partition.sectors,
                    // Saturating: the sector count came off the device, and a
                    // table that named a partition running to the end of the
                    // 64-bit address space would overflow a byte count. The read
                    // itself is bounded by the flash's geometry; only this
                    // rendering of it could be made to panic.
                    human_bytes(
                        partition
                            .sectors
                            .saturating_mul(u64::from(agent.sector_size()))
                    ),
                );
                (partition.first_lba, partition.sectors)
            }
        };

        // The device answered and the range is resolved, so the read is going to
        // run: now the file is created. The image seam is async, because the
        // browser's file APIs are; core ships the adapter that presents a blocking
        // writer across it, so the CLI hands over a `File` and nothing else changes.
        let file =
            File::create(&path).map_err(|e| Error::Io(format!("cannot create {path}: {e}")))?;
        let mut out = SyncWriter::new(BufWriter::new(file));

        let outcome = verbs::dump(
            &mut agent,
            lba,
            sectors,
            &mut out,
            &mut |event| render.render(event),
            &cancel,
        )
        .await;

        if outcome.is_err() {
            // The file was created, so a mid-dump failure has left a partial image
            // on disk. Name it, rather than let a person trust a truncated dump.
            eprintln!("Warning: {path} holds only a partial dump.");
        }
        outcome
    })?;

    // No flush here: `dump` commits the sink before it returns, because a
    // browser's writable stream holds bytes until it is told not to and a dump
    // that returned before they landed would report a file that is not there.
    println!("Wrote {path}");
    report_fill(&fill);
    Ok(())
}

/// What `flash` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct FlashArgs {
    /// Where the image lands: an LBA, or a partition the device names.
    target: Target,
    path: String,
    /// Which board, when more than one is connected.
    device: Option<DeviceSelector>,
    /// Skip the confirmation prompt. A script passes it, and so does a person
    /// with no terminal to be asked on.
    yes: bool,
    /// Produce the plan and stop. The dry run is the write's own code path,
    /// ended a step early.
    dry_run: bool,
    /// The SoC the caller says this board is, for the wrong-loader gate.
    /// Parsed up front, so a name with no pinned reply fails before a device
    /// is opened.
    soc: Option<Soc>,
}

/// Read `flash`'s target, its file, and its flags.
///
/// It takes no sector count, for the same reason `verify` takes none. The file's
/// length is how much is written, and a length a person restates is a length
/// they can get wrong.
fn parse_flash(args: &mut pico_args::Arguments) -> std::result::Result<FlashArgs, CliError> {
    // The flags come out first. pico-args reads free arguments off the front of
    // what is left, so a flag still sitting in the vector would be read as the
    // LBA -- and an LBA that failed to parse is a better outcome than one that
    // did not.
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let device = device_option(args)?;
    let soc = soc_option(args)?;

    let target = match partition_option(args)? {
        Some(name) => Target::Partition(name),
        None => Target::Lba(
            args.free_from_str()
                .map_err(|e| CliError::Usage(format!("flash needs an LBA to write at: {e}")))?,
        ),
    };

    let path = args
        .free_from_str()
        .map_err(|e| CliError::Usage(format!("flash needs a file to write: {e}")))?;

    Ok(FlashArgs {
        target,
        path,
        device,
        yes,
        dry_run,
        soc,
    })
}

/// Which ending `reset` asks the board for, defaulting to the plain reboot.
///
/// It is parsed the way `--soc` is. Core owns the names, and a name that is not
/// one of them is a usage error rather than a device fault. The refusal lists
/// the names there are. A `reset` with no `--mode` reboots the board.
fn reset_mode_option(args: &mut pico_args::Arguments) -> std::result::Result<ResetMode, CliError> {
    match args.opt_value_from_str::<_, String>("--mode") {
        Ok(Some(name)) => {
            ResetMode::parse(&name).map_err(|e| CliError::Usage(format!("--mode: {e}")))
        }
        Ok(None) => Ok(ResetMode::default()),
        Err(e) => Err(CliError::Usage(format!("--mode names a reset mode: {e}"))),
    }
}

/// Read `--soc <name>`, the SoC a write is gated against, if it was given.
///
/// It is parsed here rather than at the plan. A name with no pinned chipver reply
/// gives the gate nothing to compare against. It is therefore refused before any
/// device is opened, and the refusal lists the pinned names.
fn soc_option(args: &mut pico_args::Arguments) -> std::result::Result<Option<Soc>, CliError> {
    match args.opt_value_from_str::<_, String>("--soc") {
        // A name with no pinned reply is a usage problem -- the person named a SoC
        // the gate cannot compare against -- not a device fault, so it is classed
        // `Usage`, not `Core`.
        Ok(Some(name)) => Soc::parse(&name)
            .map(Some)
            .map_err(|e| CliError::Usage(format!("--soc: {e}. {RUN_CHIPVER}"))),
        Ok(None) => Ok(None),
        Err(e) => Err(CliError::Usage(format!(
            "--soc takes the board's SoC, like rk3576: {e}"
        ))),
    }
}

fn cmd_flash(mut args: pico_args::Arguments) -> Run {
    let FlashArgs {
        target,
        path,
        device: selector,
        yes,
        dry_run,
        soc,
    } = parse_flash(&mut args)?;
    no_more(args)?;

    // A package or a loader container written raw is refused before a device is
    // chosen. `flash` makes the same refusal on the image's first bytes.
    refuse_container(&path)?;

    let file = File::open(&path).map_err(|e| Error::Io(format!("cannot open {path}: {e}")))?;
    let image_bytes = file
        .metadata()
        .map_err(|e| Error::Io(format!("cannot measure {path}: {e}")))?
        .len();
    let mut image = SyncReader::new(BufReader::new(file));

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Writing", "Wrote");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;

        // The plan first, always. It asks the device and sends nothing that
        // changes it, so this much happens whether or not anybody says yes.
        //
        // The two ways of aiming a write differ in what they can check, and not
        // in what they are gated on: both produce a plan, both need it confirmed,
        // and both read back every window. What naming a partition buys is that
        // core can refuse an image too big for it -- which the LBA form cannot
        // do, because an LBA does not know where anything ends.
        let plan = match &target {
            Target::Lba(lba) => {
                // As in `dump` and `verify`: a DFU board serves a named region,
                // not a raw LBA. Core's plan refuses it too, and this refusal
                // names the flag that works.
                if !agent.caps().can_address_raw_lba {
                    return Err(raw_lba_unsupported("flash").into());
                }
                verbs::plan_write(&mut agent, *lba, image_bytes, soc).await?
            }
            Target::Partition(name) => {
                verbs::plan_write_partition(&mut agent, name, image_bytes, soc).await?
            }
        };
        print!("{}", render_plan(&device, &path, &plan));

        if dry_run {
            println!("Dry run: nothing was written.");
            return Ok(());
        }

        // The gate, asked rather than tripped over. It refuses the same write on
        // the same answer a moment later, so asking somebody to confirm one
        // first would be asking them to agree to something that was never going
        // to happen. They have already read the plan, and the plan carries the
        // comparison this refuses on.
        if let Some(refused) = verbs::plan_refusal(&agent, &plan) {
            return Err(refused.into());
        }

        if !confirmed(yes)? {
            println!("Nothing was written.");
            return Ok(());
        }

        // `confirm` consumes the plan, so the value that unlocks the write is
        // the one the person just read. Core cannot be reached any other way.
        verbs::flash(
            &mut agent,
            plan.confirm(),
            &mut image,
            &mut |event| render.render(event),
            &cancel,
        )
        .await?;

        println!("Wrote {path} and read every window of it back.");
        Ok::<(), CliError>(())
    })?;

    Ok(())
}

/// How many partitions a plan lists before it stops naming them one by one.
///
/// A `clone` overwrites the whole device, so its plan lists every partition on
/// the board, and an Android layout can run to thirty. The cap keeps the plan
/// screen readable. The line that replaces the rest says how many were dropped,
/// because a list that stopped silently would read as a shorter list.
const PARTITIONS_LISTED: usize = 12;

/// Render what a planned write lands in.
///
/// The plan reads the partition table to print this `touches` line. "LBA 16384,
/// 8192 sectors" is arithmetic, and a person cannot check it against anything they
/// know. "The whole of `uboot`" is a fact about their board, and they can check
/// it. This line is how a person catches a write aimed at the wrong place. A
/// flashing tool without a table cannot print it.
///
/// It also states what it does not know. A device with no table, and a device
/// whose table is damaged, both still get a planned write, and the plan says so
/// plainly. An empty list of partitions would read as "this write touches
/// nothing", on a board where it can touch everything.
fn render_touches(touches: &Touches) -> String {
    let lines: Vec<String> = match touches {
        Touches::NoTable => {
            vec!["nothing that can be named: this device has no partition table".to_string()]
        }

        Touches::UnreadableTable { format, detail } => vec![
            format!("unknown: the {format} on this device is damaged,"),
            "so the plan cannot name what these sectors hold:".to_string(),
            detail.clone(),
        ],

        Touches::Partitions(overlaps) if overlaps.is_empty() => {
            vec!["no partition: these sectors lie outside every one of them".to_string()]
        }

        Touches::Partitions(overlaps) => {
            let mut lines: Vec<String> = overlaps
                .iter()
                .take(PARTITIONS_LISTED)
                .map(|overlap| {
                    let extent = if overlap.is_whole() {
                        format!("the whole of it ({} sectors)", overlap.total)
                    } else {
                        format!("{} of its {} sectors", overlap.covered, overlap.total)
                    };
                    format!("{:<16} {extent}", overlap.name)
                })
                .collect();

            if let Some(dropped) = overlaps.len().checked_sub(PARTITIONS_LISTED)
                && dropped > 0
            {
                lines.push(format!("... and {dropped} more"));
            }
            lines
        }
    };

    // The first line carries the label, and the rest line up under its value.
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| match index {
            0 => format!("  touches      {line}\n"),
            _ => format!("               {line}\n"),
        })
        .collect()
}

/// Render a [`WritePlan`] for a person about to decide whether to allow it.
///
/// It shows what a person needs to see that this is not the write they meant,
/// before the write runs:
///
/// - Which board
/// - Which loader is running on it
/// - Where the bytes land
/// - Which of the board's partitions they land in, and how much of each
///
/// The loader's answer is shown raw, because the raw bytes are what the gate
/// compares. The refusal turns on the whole reply matching the bytes pinned for
/// the named SoC, not on a decoded shape. A decoded reading would show a person
/// something other than what is checked. The verdict is the next line of the
/// plan, from [`verbs::plan_refusal`], the same function the write enforces.
fn render_plan(device: &Chosen, path: &str, plan: &WritePlan) -> String {
    let sector_size = u64::from(plan.flash.sector_size);
    let last_lba = (plan.lba + plan.sectors).saturating_sub(1);
    let first_byte = plan.lba * sector_size;
    let last_byte = ((plan.lba + plan.sectors) * sector_size).saturating_sub(1);
    let total = plan.image_bytes + plan.padding_bytes;

    let padding = if plan.padding_bytes == 0 {
        String::new()
    } else {
        format!(
            ", padded with {} to {}",
            human_bytes(plan.padding_bytes),
            human_bytes(total)
        )
    };

    format!(
        "\n\
         This will overwrite {} of {}.\n\
         \n  \
         image        {path}\n               \
         {}{padding}\n  \
         LBA range    {} through {last_lba} ({} sectors)\n  \
         byte range   {first_byte} through {last_byte}\n\
         {}  \
         {:<12} {} ({} sectors of {sector_size} bytes)\n\
         {}\
         \n\
         {}\n\
         Nothing here can be undone.\n",
        human_bytes(total),
        device.subject(),
        human_bytes(plan.image_bytes),
        plan.lba,
        plan.sectors,
        render_touches(&plan.touches),
        device.medium_word(),
        human_bytes(plan.flash.size_bytes),
        plan.flash.size_bytes / sector_size,
        device.render_gate(&plan.soc, &plan.chip_version),
        plan.read_back.describe(),
    )
}

impl Chosen {
    /// What the plan says is about to be overwritten, in the sentence that
    /// names it.
    fn subject(&self) -> String {
        match self {
            Chosen::Usb(device) => {
                format!(
                    "flash on {:04x}:{:04x}",
                    device.vendor_id, device.product_id
                )
            }
            // A disk is not flash, whatever it is made of, and calling it that
            // would be the one line of the plan a person reads to check they are
            // pointed at the right thing.
            Chosen::Block(device) => device.node.clone(),
        }
    }

    /// Refuse a `--soc` that this device has no use for.
    ///
    /// `--soc` arms the wrong-loader gate, and a block device has no loader to
    /// gate on. The flag is refused rather than ignored. The plan renders no SoC
    /// line for a block device, so a person who passed `--soc` would see no sign
    /// of it. They could then read the write as gated on the SoC, and it is not.
    /// The refusal tells them so.
    fn reject_soc(&self, soc: Option<Soc>) -> std::result::Result<(), CliError> {
        match (self, soc) {
            (Chosen::Block(device), Some(soc)) => Err(CliError::Usage(format!(
                "--soc {} gates a write on the answer a loader gives, and `{}` is a block \
                 device with no loader on it to ask. This write is guarded instead by the \
                 kernel's exclusive open, and no disk the running system rests on can be \
                 opened. Drop --soc.",
                soc.name(),
                device.node
            ))),
            _ => Ok(()),
        }
    }

    /// The device named as fully as it can be, for a plan that has to tell two
    /// devices apart.
    ///
    /// A clone with its source and destination swapped overwrites the board it
    /// was meant to copy. Two lines that look alike stop a person catching that.
    /// This is therefore the long form, with everything that distinguishes one
    /// device from another of the same kind.
    fn coordinates(&self) -> String {
        match self {
            Chosen::Usb(device) => format!(
                "{:04x}:{:04x} on bus {} address {}",
                device.vendor_id, device.product_id, device.bus_id, device.device_address
            ),
            Chosen::Block(device) => match &device.model {
                Some(model) => format!("{} ({model})", device.node),
                None => device.node.clone(),
            },
        }
    }

    /// The device spelled the way `--device` takes it.
    ///
    /// It is the one identifier a person is asked to type. It uses the CLI's own
    /// spelling rather than a form invented for the prompt. The two spellings are
    /// `003:12` for a board and `/dev/sdb` for a disk, and the leading slash
    /// separates them.
    fn selector_form(&self) -> String {
        match self {
            Chosen::Usb(device) => format!("{}:{}", device.bus_id, device.device_address),
            Chosen::Block(device) => device.node.clone(),
        }
    }

    /// The device alone, where the sentence around it already says what part of
    /// it is being written.
    fn subject_short(&self) -> String {
        match self {
            Chosen::Usb(device) => {
                format!("{:04x}:{:04x}", device.vendor_id, device.product_id)
            }
            Chosen::Block(device) => device.node.clone(),
        }
    }

    /// The label on the geometry line: what the sectors belong to.
    fn medium_word(&self) -> &'static str {
        match self {
            Chosen::Usb(_) => "flash",
            Chosen::Block(_) => "device",
        }
    }

    /// The gate's own lines of the plan.
    ///
    /// The wrong-loader gate compares a loader's answer against pinned bytes. On
    /// a board, those two lines are the evidence and the verdict. A block device
    /// has no loader, so there is no answer to print. The USB form would print
    /// *the write will be refused until a SoC is named*. That is false for this
    /// backend, and would teach a person to distrust the line.
    ///
    /// The block form prints what does guard this write. The disks the running
    /// system rests on are refused outright, and the kernel has granted this
    /// device exclusively. That grant exists before any plan does, because
    /// [`crate::blockcli::open`] produced the agent.
    fn render_gate(&self, soc: &Option<Soc>, chip_version: &[u8]) -> String {
        match self {
            Chosen::Usb(_) => format!(
                "  loader says  {}  \"{}\"\n  named SoC    {}\n",
                hex(chip_version),
                ascii(chip_version),
                render_gate(soc, chip_version)
            ),
            Chosen::Block(device) => format!(
                "  held         exclusively by this command. The kernel granted `{}` with \
                 nothing else\n               holding it. No disk the running system rests on \
                 can be opened.\n",
                device.name
            ),
        }
    }
}

/// Render the wrong-loader gate's line of the plan: the SoC the write was
/// planned for, and whether the loader's answer matches it.
///
/// It is the verdict a person reads next to the evidence. It uses the same
/// comparison the write enforces (see [`verbs::plan_refusal`]). It takes the two
/// gate fields rather than a plan, so a plain write's plan and a table write's
/// plan render it the same way.
fn render_gate(soc: &Option<Soc>, chip_version: &[u8]) -> String {
    match soc {
        Some(soc) if soc.matches(chip_version) => {
            format!("{}: the loader's answer matches", soc.name())
        }
        Some(soc) => format!(
            "{}: the loader's answer does not match, so the write will be refused",
            soc.name()
        ),
        None => "none: the write will be refused until a SoC is named (--soc)".to_string(),
    }
}

/// Ask whether to go ahead, unless `yes` already answered.
///
/// This function is the CLI's whole implementation of explicit gating. Core
/// never reads stdin, because it cannot know whether anyone is there. Consent is
/// therefore a value the CLI produces and passes in, and this is where it is
/// produced.
///
/// With no terminal to ask on and no `--yes`, the CLI refuses rather than
/// assumes. A pipe is not agreement to a write.
fn confirmed(yes: bool) -> std::result::Result<bool, CliError> {
    confirmed_that(yes, NO_TERMINAL_FOR_WRITE)
}

/// What to say when a write needs an answer and there is no terminal to ask on.
const NO_TERMINAL_FOR_WRITE: &str = "this command overwrites flash and needs confirmation, but no terminal is attached. Pass \
     --yes to confirm the write, or --dry-run to print the plan only.";

/// The same message for a boot override, which overwrites nothing and says so.
const NO_TERMINAL_FOR_BOOT: &str = "this command changes the board's boot order for one boot and needs confirmation, but no \
     terminal is attached. It overwrites nothing. Pass --yes to confirm the change, or --dry-run \
     to print the plan only.";

/// Ask, unless `yes` already answered.
///
/// `no_terminal` is what a non-interactive run is told. It differs per caller
/// because what is agreed to differs: a flash overwrites a board, and a boot
/// override changes volatile RAM for one boot. A person who sees a prompt describe
/// the wrong act learns to stop reading prompts.
fn confirmed_that(yes: bool, no_terminal: &str) -> std::result::Result<bool, CliError> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(no_terminal.to_string()));
    }

    eprint!("Proceed? [y/N] ");
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| Error::Io(format!("cannot read the answer: {e}")))?;

    // Only an explicit yes is a yes. Everything else, including an empty line and
    // a closed stdin, is a no.
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Ask for the destination to be typed back, unless `yes` already answered.
///
/// It is the heavier of the two confirmations, and `clone` uses it. A `flash` has
/// one device, and a `[y/N]` against a plan naming it is a real act of agreement.
/// A clone has two devices. Its worst mistake is the source and the destination
/// swapped. Careful reading does not catch that, because both boards are real and
/// both halves of the plan are true.
///
/// A keystroke agrees to whichever plan is on the screen. Typing `003:12` agrees
/// to one device, and a person who has the two swapped types the wrong one. The
/// window's clone gate confirms the same way, for the same reason.
///
/// `--yes` still answers it. A person automating a clone has already spelled both
/// devices out on the command line, which is the typed coordinate by another
/// route.
fn confirmed_by_typing(yes: bool, destination: &Chosen) -> std::result::Result<bool, CliError> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(NO_TERMINAL_FOR_CLONE.to_string()));
    }

    let expected = destination.selector_form();
    eprint!("Type the destination ({expected}) to confirm, or anything else to stop: ");
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| Error::Io(format!("cannot read the answer: {e}")))?;

    // Exact, and not case-folded: a device node is case-sensitive and a bus
    // coordinate has no case to fold. Only the surrounding whitespace a terminal
    // adds is forgiven.
    Ok(answer.trim() == expected)
}

/// What a non-interactive `clone` is told. It differs from what a `flash` is
/// told, because a clone involves two devices and overwrites one of them.
const NO_TERMINAL_FOR_CLONE: &str = "this command overwrites the destination device with the source and needs confirmation, \
     but no terminal is attached. Pass --yes to confirm the clone, or --dry-run to print the \
     plan only.";

/// What `clone` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct CloneArgs {
    from: DeviceSelector,
    to: DeviceSelector,
    yes: bool,
    dry_run: bool,
    /// The SoC the *destination* is, for the wrong-loader gate: the destination
    /// is the board that gets written.
    soc: Option<Soc>,
}

/// Read `clone`'s two devices and its two flags.
///
/// Both devices must be named, and neither is inferred. Even with exactly two
/// boards connected, no guess is safe. The difference between the two is which
/// one is overwritten, and a tool that picks for the person picks wrong half the
/// time.
fn parse_clone(args: &mut pico_args::Arguments) -> std::result::Result<CloneArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let soc = soc_option(args)?;

    let from: DeviceSelector = args.value_from_str("--from").map_err(|e| {
        CliError::Usage(format!(
            "clone needs --from <bus>:<address>, the board to copy: {e}"
        ))
    })?;
    let to: DeviceSelector = args.value_from_str("--to").map_err(|e| {
        CliError::Usage(format!(
            "clone needs --to <bus>:<address>, the board to overwrite: {e}"
        ))
    })?;

    if from == to {
        return Err(CliError::Usage(format!(
            "--from and --to are both {from}. A board cannot be cloned onto itself"
        )));
    }

    Ok(CloneArgs {
        from,
        to,
        yes,
        dry_run,
        soc,
    })
}

fn cmd_clone(mut args: pico_args::Arguments) -> Run {
    let CloneArgs {
        from,
        to,
        yes,
        dry_run,
        soc,
    } = parse_clone(&mut args)?;
    no_more(args)?;

    let source = choose_target(Some(&from))?;
    let destination = choose_target(Some(&to))?;
    // The destination's, because the destination is what gets written and the
    // gate is about what is running on it. A block source under a board
    // destination is an ordinary gated write.
    destination.reject_soc(soc)?;

    let mut render = ProgressRenderer::new("Cloning", "Cloned");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut src = source.open().await?;
        let mut dst = destination.open().await?;

        // A clone spans a whole device by raw LBA on both sides, so a board that
        // reaches its flash only by named region (a DFU board) can be neither.
        // Refused before the plan, so neither board is touched.
        if !src.caps().can_address_raw_lba || !dst.caps().can_address_raw_lba {
            return Err(clone_raw_lba_unsupported().into());
        }

        // Both devices are asked, neither is changed. The source is only ever
        // read, so nothing about a clone can hurt the board being copied.
        let plan = verbs::plan_clone(&mut src, &mut dst, soc).await?;
        print!("{}", render_clone_plan(&source, &destination, &plan));

        if dry_run {
            println!("Dry run: nothing was written.");
            return Ok(());
        }

        // The destination's gate, because the destination is what gets written.
        // Asked before anybody is asked to confirm, for the reason `flash` asks.
        if let Some(refused) = verbs::plan_refusal(&dst, &plan.destination) {
            return Err(refused.into());
        }

        if !confirmed_by_typing(yes, &destination)? {
            println!("Nothing was written.");
            return Ok(());
        }

        let fill = verbs::clone(
            &mut src,
            &mut dst,
            plan.confirm(),
            &mut |event| render.render(event),
            &cancel,
        )
        .await?;

        println!("Cloned, and read every window of it back.");
        // The fill is the *source's*, read a window at a time as it was copied.
        // A clone across a silent read failure wrote that fill onto the
        // destination, so surfacing it here is surfacing what the destination now
        // holds -- the same warning a dump gets, on the board just written.
        report_fill(&fill);
        Ok::<(), CliError>(())
    })?;

    Ok(())
}

/// Render a [`ClonePlan`] for a person about to allow one board to overwrite
/// another.
///
/// It spells out both ends. The mistake it exists to catch is the costliest one:
/// the source and the destination swapped, which overwrites the board the person
/// meant to copy.
fn render_clone_plan(source: &Chosen, destination: &Chosen, plan: &ClonePlan) -> String {
    let write = &plan.destination;
    let sector_size = u64::from(write.flash.sector_size);
    let last_lba = (write.lba + write.sectors).saturating_sub(1);

    format!(
        "\n\
         This will overwrite the whole of {}.\n\
         \n  \
         source       {}\n               \
         --from {}\n               \
         {} ({} sectors of {} bytes)\n  \
         destination  {}\n               \
         --to {}\n               \
         {} ({} sectors of {sector_size} bytes)\n  \
         LBA range    {} through {last_lba} ({} sectors)\n\
         {}\
         {}\
         \n\
         The source is only read. {}\n\
         Nothing here can be undone.\n",
        destination.coordinates(),
        source.coordinates(),
        // The spelling each device was named by, on both halves. It is what the
        // confirmation asks to be typed back, and a coordinate a person is asked
        // for and never shown is one they have to go and look up -- which is how
        // a confirmation turns into a copy from the wrong line.
        source.selector_form(),
        human_bytes(plan.source.size_bytes),
        plan.source.size_bytes / u64::from(plan.source.sector_size),
        plan.source.sector_size,
        destination.coordinates(),
        destination.selector_form(),
        human_bytes(write.flash.size_bytes),
        write.flash.size_bytes / sector_size,
        write.lba,
        write.sectors,
        // The destination's, because the destination is what gets overwritten --
        // and a clone overwrites all of it, so this is every partition the
        // device has. That is a long list on an Android layout, and it is the
        // right list: it is what the clone destroys.
        render_touches(&write.touches),
        // The destination's gate too, for the same reason.
        destination.render_gate(&write.soc, &write.chip_version),
        // And the destination's read-back, in the backend's own words, because
        // the destination is what is read back.
        write.read_back.describe(),
    )
}

/// What `repair-table` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct RepairArgs {
    /// Which board, when more than one is connected.
    device: Option<DeviceSelector>,
    /// Skip the confirmation prompt.
    yes: bool,
    /// Produce the plan and stop.
    dry_run: bool,
    /// The SoC the board is, for the wrong-loader gate. A repair writes flash, so
    /// it is gated exactly as `flash` is.
    soc: Option<Soc>,
}

/// Read the flags `repair-table` and `repair-param` share.
///
/// It takes no target and no file. A repair rewrites a damaged copy of the table
/// from an intact copy on the same device, so the device is all it acts on.
fn parse_repair_table(
    args: &mut pico_args::Arguments,
) -> std::result::Result<RepairArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let device = device_option(args)?;
    let soc = soc_option(args)?;

    Ok(RepairArgs {
        device,
        yes,
        dry_run,
        soc,
    })
}

fn cmd_repair_table(mut args: pico_args::Arguments) -> Run {
    let RepairArgs {
        device: selector,
        yes,
        dry_run,
        soc,
    } = parse_repair_table(&mut args)?;
    no_more(args)?;

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Repairing", "Repaired");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;
        // The plan first, always. It reads the device -- both GPT copies among the
        // rest -- and sends nothing that changes it. A device with nothing to
        // repair (a healthy primary, or no GPT at all) is refused here, before
        // anybody is asked to confirm anything.
        let plan = verbs::plan_repair_table(&mut agent, soc).await?;
        execute_table_write(
            &mut agent,
            &device,
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;

    Ok(())
}

fn cmd_repair_param(mut args: pico_args::Arguments) -> Run {
    let RepairArgs {
        device: selector,
        yes,
        dry_run,
        soc,
    } = parse_repair_table(&mut args)?;
    no_more(args)?;

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Repairing", "Repaired");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;
        // The parameter's copies are scanned; a damaged one with an intact sibling
        // is repairable, and a lone eMMC copy gone bad is not -- refused here,
        // before anyone confirms.
        let plan = verbs::plan_repair_param(&mut agent, soc).await?;
        execute_table_write(
            &mut agent,
            &device,
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;

    Ok(())
}

/// Show, gate, confirm and run a planned table write through the one
/// [`SegmentedPlan`] path.
///
/// It serves every table write: a repair or an authoring, of a GPT or a
/// parameter block. Each is the same four steps:
///
/// 1. The plan, already made by the caller and passed in
/// 2. The gate, asked before anyone is prompted
/// 3. The confirmation
/// 4. The write, which reads back every window
///
/// Sharing the steps here keeps a repair and an authoring from drifting apart in
/// how they gate and what they print.
async fn execute_table_write(
    agent: &mut FlashAgent<UsbTransport>,
    device: &Chosen,
    plan: SegmentedPlan,
    dry_run: bool,
    yes: bool,
    render: &mut ProgressRenderer,
    cancel: &Cancel,
) -> std::result::Result<(), CliError> {
    print!("{}", render_segmented_plan(device, &plan));

    if dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }

    // A table write is a write, gated on the same loader match. Asked before the
    // prompt, so nobody confirms a write that was never going to run.
    if let Some(refused) = verbs::segmented_refusal(agent, &plan) {
        return Err(refused.into());
    }

    if !confirmed(yes)? {
        println!("Nothing was written.");
        return Ok(());
    }

    // The closing line, worked out before `confirm` consumes the plan.
    let closing = closing_line(&plan);
    verbs::write_table(
        agent,
        plan.confirm(),
        &mut |event| render.render(event),
        cancel,
    )
    .await?;

    println!("{closing}");
    Ok(())
}

/// The line printed once a table write has finished, naming what it did.
fn closing_line(plan: &SegmentedPlan) -> String {
    let format = plan.format.name();
    let copies = plan.segments.len();
    match &plan.action {
        TableAction::Repair { .. } if copies == 1 => {
            format!(
                "Rewrote {} and read every window of it back.",
                plan.segments[0].what
            )
        }
        TableAction::Repair { .. } => {
            format!("Rewrote {copies} {format} copies and read every window back.")
        }
        TableAction::Author => {
            let plural = if copies == 1 { "copy" } else { "copies" };
            format!(
                "Authored the {format} table across {copies} {plural} and read every window back."
            )
        }
    }
}

/// Render one labeled, indented column of the plan: a label, then its lines. The
/// first line follows the label, and the rest are indented to align with it.
fn render_column(label: &str, lines: &[String]) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| match index {
            0 => format!("  {label:<11}  {line}\n"),
            _ => format!("               {line}\n"),
        })
        .collect()
}

/// Render a list of partitions for a plan, as names and lengths.
///
/// The list is capped the way [`render_touches`] caps the partitions a write
/// lands in, with the same "and N more".
fn render_partition_lines(partitions: &[Partition]) -> Vec<String> {
    if partitions.is_empty() {
        return vec!["(none: the table names no partitions)".to_string()];
    }

    let mut lines: Vec<String> = partitions
        .iter()
        .take(PARTITIONS_LISTED)
        .map(|partition| format!("{:<16} {} sectors", partition.name, partition.sectors))
        .collect();
    if let Some(dropped) = partitions.len().checked_sub(PARTITIONS_LISTED)
        && dropped > 0
    {
        lines.push(format!("... and {dropped} more"));
    }
    lines
}

/// Render a [`SegmentedPlan`], a repair or an authoring, for a person about to
/// allow it.
///
/// A table write is a write, so this shows what a write's plan shows:
///
/// - Which board, and which loader is running on it
/// - The size and sector count of the flash the table is written to
/// - The read-back note
/// - The wrong-loader verdict
///
/// It also shows what is particular to a table write. That is whether it repairs
/// or authors, the segments it lays down and where, and the partitions the
/// resulting table holds. A person recognizes the table by its partitions.
fn render_segmented_plan(device: &Chosen, plan: &SegmentedPlan) -> String {
    let sector_size = u64::from(plan.flash.sector_size);
    let format = plan.format.name();

    let (verb, seg_label, part_label, head_end, footer_lead) = match &plan.action {
        TableAction::Repair { source } => (
            "repair",
            "rewriting",
            "restores",
            format!(",\nfrom {source}.\n"),
            "The intact copy is not touched. ",
        ),
        TableAction::Author => ("author", "writing", "partitions", ".\n".to_string(), ""),
    };

    // Each segment as its "what" line and an "at LBA" line beneath it.
    let seg_lines: Vec<String> = plan
        .segments
        .iter()
        .flat_map(|segment| {
            let sectors = (segment.bytes.len() as u64).div_ceil(sector_size);
            let last = (segment.lba + sectors).saturating_sub(1);
            [
                segment.what.clone(),
                format!(
                    "LBA {} through {last} ({sectors} sectors of {sector_size} bytes)",
                    segment.lba
                ),
            ]
        })
        .collect();

    format!(
        "\n\
         This will {verb} the {format} table on {}{head_end}\n\
         {}{}  \
         {:<12} {} ({} sectors of {sector_size} bytes)\n\
         {}\
         \n\
         {footer_lead}{}\n\
         Nothing here can be undone.\n",
        device.subject_short(),
        render_column(seg_label, &seg_lines),
        render_column(part_label, &render_partition_lines(&plan.partitions)),
        device.medium_word(),
        human_bytes(plan.flash.size_bytes),
        plan.flash.size_bytes / sector_size,
        device.render_gate(&plan.soc, &plan.chip_version),
        plan.read_back.describe(),
    )
}

/// Where the partition layout an `author-param` reads comes from.
#[derive(Debug, PartialEq, Eq)]
enum AuthorSource {
    /// A native-format layout file: `name first_lba sectors [type]` lines.
    Layout(String),
    /// A file of `mtdparts=` text, a board's own partition line.
    Mtdparts(String),
    /// A file of an existing parameter's whole text, framed verbatim, keys and all.
    Block(String),
}

/// What `author-param` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct AuthorParamArgs {
    device: Option<DeviceSelector>,
    yes: bool,
    dry_run: bool,
    soc: Option<Soc>,
    medium: ParamMedium,
    source: AuthorSource,
}

/// Read `author-param`'s flags: the medium, the layout source, and the write gate.
fn parse_author_param(
    args: &mut pico_args::Arguments,
) -> std::result::Result<AuthorParamArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let device = device_option(args)?;
    let soc = soc_option(args)?;

    let medium =
        match args
            .opt_value_from_str::<_, String>("--medium")
            .map_err(|e| CliError::Usage(format!("author-param needs --medium emmc|nand: {e}")))?
        {
            Some(value) => match value.as_str() {
                "emmc" => ParamMedium::Emmc,
                "nand" => ParamMedium::Nand,
                other => {
                    return Err(CliError::Usage(format!(
                        "--medium is emmc or nand, not '{other}'"
                    )));
                }
            },
            None => return Err(CliError::Usage(
                "author-param needs --medium emmc|nand: it decides where the copies go and, for a \
                 layout, what its offsets count from"
                    .to_string(),
            )),
        };

    // Exactly one source, so a person cannot half-say which table to write.
    let layout = args
        .opt_value_from_str::<_, String>("--layout")
        .map_err(|e| CliError::Usage(format!("--layout takes a file: {e}")))?;
    let mtdparts = args
        .opt_value_from_str::<_, String>("--mtdparts")
        .map_err(|e| CliError::Usage(format!("--mtdparts takes a file: {e}")))?;
    let block = args
        .opt_value_from_str::<_, String>("--from-block")
        .map_err(|e| CliError::Usage(format!("--from-block takes a file: {e}")))?;

    let source =
        match (layout, mtdparts, block) {
            (Some(path), None, None) => AuthorSource::Layout(path),
            (None, Some(path), None) => AuthorSource::Mtdparts(path),
            (None, None, Some(path)) => AuthorSource::Block(path),
            (None, None, None) => return Err(CliError::Usage(
                "author-param needs a layout: --layout <file> (native format), --mtdparts <file> \
                 (a board's mtdparts= line), or --from-block <file> (an existing parameter's whole \
                 text)"
                    .to_string(),
            )),
            _ => {
                return Err(CliError::Usage(
                    "author-param takes exactly one of --layout, --mtdparts, or --from-block"
                        .to_string(),
                ));
            }
        };

    Ok(AuthorParamArgs {
        device,
        yes,
        dry_run,
        soc,
        medium,
        source,
    })
}

fn cmd_author_param(mut args: pico_args::Arguments) -> Run {
    let AuthorParamArgs {
        device: selector,
        yes,
        dry_run,
        soc,
        medium,
        source,
    } = parse_author_param(&mut args)?;
    no_more(args)?;

    let read = |path: &str| -> Result<String> {
        std::fs::read_to_string(path).map_err(|e| Error::Io(format!("cannot read {path}: {e}")))
    };

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Writing", "Wrote");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;

        // The layout front-ends resolve `-` and validate against the geometry, so
        // the device's own sector count is read first. `plan_author_param` reads it
        // again -- an idempotent read command -- which keeps the plan a pure
        // function of one survey.
        let flash = agent.info().await?;
        let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

        // A layout is parsed here; text is handed to the verb whole. Building the
        // plan borrows from these, so they outlive the call.
        let layout;
        let text;
        let param_source = match &source {
            AuthorSource::Layout(path) => {
                layout = Layout::parse_native(&read(path)?, flash_sectors)?;
                ParamAuthorSource::Layout(&layout)
            }
            AuthorSource::Mtdparts(path) => {
                layout = Layout::parse_mtdparts(&read(path)?, medium.base_lba(), flash_sectors)?;
                ParamAuthorSource::Layout(&layout)
            }
            AuthorSource::Block(path) => {
                text = read(path)?;
                ParamAuthorSource::Text(&text)
            }
        };

        let plan = verbs::plan_author_param(&mut agent, param_source, medium, soc).await?;
        execute_table_write(
            &mut agent,
            &device,
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;

    Ok(())
}

/// Where the partition layout an `author-gpt` reads comes from.
///
/// The native format or a board's `mtdparts` line, the two absolute-LBA layout
/// front-ends. A GPT is not built from parameter text, so it takes no
/// `--from-block`. A GPT has no medium base, so an `mtdparts` line is resolved at
/// base zero, and its offsets are the absolute sectors a GPT addresses.
#[derive(Debug, PartialEq, Eq)]
enum GptSource {
    /// A native-format layout file, with its optional types and GUID overrides.
    Layout(String),
    /// A file of `mtdparts=` text, resolved as absolute LBAs.
    Mtdparts(String),
}

/// What `author-gpt` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct AuthorGptArgs {
    device: Option<DeviceSelector>,
    yes: bool,
    dry_run: bool,
    soc: Option<Soc>,
    source: GptSource,
}

/// Read `author-gpt`'s flags: the layout source and the write gate.
///
/// Unlike `author-param`, it takes no `--medium` (a GPT is absolute) and no
/// `--from-block` (a GPT is not built from parameter text).
fn parse_author_gpt(
    args: &mut pico_args::Arguments,
) -> std::result::Result<AuthorGptArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let device = device_option(args)?;
    let soc = soc_option(args)?;

    // Exactly one source, so a person cannot half-say which table to write.
    let layout = args
        .opt_value_from_str::<_, String>("--layout")
        .map_err(|e| CliError::Usage(format!("--layout takes a file: {e}")))?;
    let mtdparts = args
        .opt_value_from_str::<_, String>("--mtdparts")
        .map_err(|e| CliError::Usage(format!("--mtdparts takes a file: {e}")))?;

    let source =
        match (layout, mtdparts) {
            (Some(path), None) => GptSource::Layout(path),
            (None, Some(path)) => GptSource::Mtdparts(path),
            (None, None) => return Err(CliError::Usage(
                "author-gpt needs a layout: --layout <file> (native format, with types and GUID \
                 overrides) or --mtdparts <file> (a board's mtdparts= line)"
                    .to_string(),
            )),
            (Some(_), Some(_)) => {
                return Err(CliError::Usage(
                    "author-gpt takes exactly one of --layout or --mtdparts".to_string(),
                ));
            }
        };

    Ok(AuthorGptArgs {
        device,
        yes,
        dry_run,
        soc,
        source,
    })
}

fn cmd_author_gpt(mut args: pico_args::Arguments) -> Run {
    let AuthorGptArgs {
        device: selector,
        yes,
        dry_run,
        soc,
        source,
    } = parse_author_gpt(&mut args)?;
    no_more(args)?;

    let read = |path: &str| -> Result<String> {
        std::fs::read_to_string(path).map_err(|e| Error::Io(format!("cannot read {path}: {e}")))
    };

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Writing", "Wrote");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;

        // The layout front-ends resolve `-` and validate against the geometry, so the
        // device's own sector count is read first. `plan_author_gpt` reads it again --
        // an idempotent read command -- keeping the plan a pure function of one survey.
        let flash = agent.info().await?;
        let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

        // A GPT is absolute, so an mtdparts line is resolved at base zero: its offsets
        // are the sectors a GPT addresses from, with no eMMC reserved-region base a
        // parameter would count from.
        let layout = match &source {
            GptSource::Layout(path) => Layout::parse_native(&read(path)?, flash_sectors)?,
            GptSource::Mtdparts(path) => Layout::parse_mtdparts(&read(path)?, 0, flash_sectors)?,
        };

        let plan = verbs::plan_author_gpt(&mut agent, &layout, soc).await?;
        execute_table_write(
            &mut agent,
            &device,
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;

    Ok(())
}

/// Read a loader container from `path`, or the loader inside a firmware package.
///
/// A firmware package carries the loader it was built with, so `db` and `write-idb`
/// take the package itself and nobody extracts the loader by hand. Core reads only
/// the package's header and its loader, because a package is gigabytes.
fn read_loader(path: &str) -> std::result::Result<rkboot::LoaderImage, CliError> {
    let file = File::open(path).map_err(|e| Error::Io(format!("cannot open {path}: {e}")))?;
    let file_bytes = file
        .metadata()
        .map_err(|e| Error::Io(format!("cannot measure {path}: {e}")))?
        .len();
    let mut image = SyncReader::new(BufReader::new(file));
    let found = pollster::block_on(firmware::read_loader(&mut image, file_bytes))?;
    if found.in_package {
        println!("Using the loader inside the firmware package {path}.");
    }
    Ok(found.loader)
}

/// Say what a loader file claims to be for, before anything is opened.
///
/// A container names the SoC it was built for, and printing it turns "which of
/// these six loaders is the RK3576 one" from a guess into a thing a person can
/// read. A file that claims nothing, bare stage blobs, says so rather than staying
/// silent, because silence would read as a check that passed.
fn print_loader_claim(loader: &rkboot::LoaderImage) {
    match loader.chip {
        Some(_) => println!(
            "This loader's chip field holds {}.",
            loader_claim(loader.chip)
        ),
        None => println!(
            "These are bare stage files, which carry no container and so name no SoC. \
             Nothing checks which board they are for."
        ),
    }
}

/// A loader container's claim about its SoC, as a plan or a report prints it.
fn loader_claim(chip: Option<[u8; 4]>) -> String {
    match chip {
        Some(chip) => {
            let claim = match pyrographer_core::soc::by_container_chip(&chip) {
                Some(named) => named.name().to_string(),
                None => "no pinned SoC".to_string(),
            };
            format!("{} \"{}\" ({claim})", hex(&chip), ascii(&chip))
        }
        None => "nothing: bare stage files carry no container".to_string(),
    }
}

/// Refuse a file that is a container a tool unpacks, before a device is chosen.
///
/// Core's [`verbs::image_refusal`] judges the file's first bytes, and `flash`
/// makes the same refusal on the image it is given. Asked here first, the person
/// is told which command writes what they picked.
fn refuse_container(path: &str) -> std::result::Result<(), CliError> {
    let mut head = Vec::with_capacity(verbs::CONTAINER_MAGIC_LEN);
    File::open(path)
        .and_then(|file| {
            file.take(verbs::CONTAINER_MAGIC_LEN as u64)
                .read_to_end(&mut head)
        })
        .map_err(|e| Error::Io(format!("cannot read {path}: {e}")))?;
    let Some(refusal) = verbs::image_refusal(&head) else {
        return Ok(());
    };
    let next = match rkfw::identify(&head) {
        Some(rkfw::Container::Package) => {
            "Run `pyrographer flash-firmware <file>` to write a firmware package."
        }
        Some(rkfw::Container::Archive) => {
            "A bare archive is the inner layer of an update.img. Run `pyrographer \
             flash-firmware` on the package it came from."
        }
        Some(rkfw::Container::Loader) | None => {
            "Run `pyrographer write-idb --loader <file>` to write its ID block, or \
             `pyrographer db --loader <file>` to upload it to a board in maskrom."
        }
    };
    Err(CliError::Usage(format!("{refusal}. {next}")))
}

/// Read and check the firmware package at `path`: the one forward pass, with its
/// progress on the terminal.
///
/// The whole file is read before a device is chosen, so a damaged or incomplete
/// download is refused with nothing opened.
fn read_firmware(path: &str) -> std::result::Result<Package, CliError> {
    let file = File::open(path).map_err(|e| Error::Io(format!("cannot open {path}: {e}")))?;
    let file_bytes = file
        .metadata()
        .map_err(|e| Error::Io(format!("cannot measure {path}: {e}")))?
        .len();
    let mut image = SyncReader::new(BufReader::new(file));
    let mut render = ProgressRenderer::new("Checking", "Checked");
    let cancel = Cancel::new();
    Ok(pollster::block_on(firmware::read(
        &mut image,
        file_bytes,
        &mut |event| render.render(event),
        &cancel,
    ))?)
}

/// Read and check a firmware package, and report what it holds. No device.
fn cmd_firmware_info(mut args: pico_args::Arguments) -> Run {
    let path: String = args.free_from_str().map_err(|e| {
        CliError::Usage(format!(
            "firmware-info needs a firmware package to read: {e}"
        ))
    })?;
    no_more(args)?;

    let package = read_firmware(&path)?;
    print!("{}", render_package(&path, &package));
    Ok(())
}

/// The size `firmware-info` lays a package's parameter out against.
///
/// It is far larger than any eMMC, so every offset fits, and the partition that
/// grows can be recognized and described as growing rather than given a length.
const NOMINAL_SECTORS: u64 = 1 << 40;

/// Render a checked firmware package for `firmware-info`.
fn render_package(path: &str, package: &Package) -> String {
    let header = &package.header;
    let archive = &package.archive;
    let release = header.release;

    let mut rows: Vec<(&str, Vec<String>)> = vec![
        ("model", vec![archive.model.clone()]),
        ("made by", vec![archive.manufacturer.clone()]),
        (
            "version",
            vec![format!(
                "{}, built {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                rkfw::version_text(header.version),
                release.year,
                release.month,
                release.day,
                release.hour,
                release.minute,
                release.second
            )],
        ),
        (
            "chip field",
            vec![format!("{} (reported, not compared)", hex(&header.chip))],
        ),
        (
            "loader",
            vec![format!(
                "{} at byte {}, naming {}",
                human_bytes(header.loader.len),
                header.loader.offset,
                loader_claim(package.loader.chip)
            )],
        ),
        (
            "archive",
            vec![format!(
                "{} at byte {}, and its checksum holds",
                human_bytes(header.archive.len),
                header.archive.offset
            )],
        ),
        (
            "trailer",
            vec![match (package.trailer_text(), &package.trailer) {
                (Some(text), _) => format!("MD5 {text}, reported and not checked"),
                (None, Some(bytes)) => format!("{}, reported and not checked", hex(bytes)),
                (None, None) => "none".to_string(),
            }],
        ),
    ];

    let id_block = &package.id_block;
    let mut idb_lines = vec![format!(
        "{} sectors, laid out by the {} header, every hash checked",
        id_block.sectors(),
        id_block.header_stage
    )];
    idb_lines.extend(id_block.images.iter().map(render_id_block_image));
    rows.push(("ID block", idb_lines));

    let partitions = match package.layout(NOMINAL_SECTORS) {
        Ok(layout) => {
            let (_, last_usable) =
                pyrographer_core::codec::gpt::usable_range(NOMINAL_SECTORS, 512).unwrap_or((0, 0));
            layout
                .partitions
                .iter()
                .map(|part| {
                    let extent = if part.first_lba + part.sectors == last_usable + 1 {
                        format!("from LBA {}, growing to fill the device", part.first_lba)
                    } else {
                        format!("{} sectors at LBA {}", part.sectors, part.first_lba)
                    };
                    match &part.unique_guid {
                        Some(guid) => format!("{:<16} {extent}, GUID {guid}", part.name),
                        None => format!("{:<16} {extent}", part.name),
                    }
                })
                .collect()
        }
        Err(error) => vec![error.to_string()],
    };
    rows.push(("partitions", partitions));

    rows.push((
        "images",
        package
            .images
            .iter()
            .map(|image| {
                format!(
                    "{:<16} {} from {}",
                    image.name,
                    human_bytes(image.bytes),
                    image.path
                )
            })
            .collect(),
    ));
    if !package.skipped.is_empty() {
        rows.push((
            "not written",
            package
                .skipped
                .iter()
                .map(|skipped| format!("{}: {}", skipped.name, skipped.why))
                .collect(),
        ));
    }

    let body: String = rows
        .iter()
        .map(|(label, lines)| render_column(label, lines))
        .collect();
    format!(
        "\n{path}: a Rockchip firmware package, {}.\n\n{body}\nEvery check passed: the \
         archive's checksum, the two copies of the loader, the ID block's hashes, and the \
         parameter's checksum.\n",
        human_bytes(package.file_bytes)
    )
}

/// One image of an ID block, as a plan or a report lists it.
fn render_id_block_image(image: &pyrographer_core::codec::idb::IdbImage) -> String {
    let load = image
        .load_address
        .map(|address| format!(", loads at {address:#010x}"))
        .unwrap_or_default();
    format!(
        "{} at sector {}, {} sectors{load}",
        image.stage, image.sector, image.sectors
    )
}

/// Write a firmware package: every partition image, the GPT its parameter
/// describes, and the ID block.
fn cmd_flash_firmware(mut args: pico_args::Arguments) -> Run {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let selector = device_option(&mut args)?;
    let soc = soc_option(&mut args)?;
    let path: String = args.free_from_str().map_err(|e| {
        CliError::Usage(format!(
            "flash-firmware needs a firmware package to write: {e}"
        ))
    })?;
    no_more(args)?;

    // The package first, read and checked whole, before a device is chosen.
    let package = read_firmware(&path)?;

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Writing", "Wrote");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;
        let plan = verbs::plan_firmware(&mut agent, &package, soc).await?;
        execute_firmware_write(
            &mut agent,
            &device,
            Some(&path),
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;
    Ok(())
}

/// Write a loader's ID block at sector 64, and nothing else.
fn cmd_write_idb(mut args: pico_args::Arguments) -> Run {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");
    let selector = device_option(&mut args)?;
    let soc = soc_option(&mut args)?;
    let loader_path: String = args.value_from_str("--loader").map_err(|e| {
        CliError::Usage(format!(
            "write-idb needs --loader <file>, a loader container or a firmware package: {e}"
        ))
    })?;
    no_more(args)?;

    // The loader is read, its claim printed and judged, and its ID block built,
    // before a device is chosen: a file that cannot make one has cost no open.
    let loader = read_loader(&loader_path)?;
    print_loader_claim(&loader);
    if let Some(refusal) = verbs::loader_blob_refusal(soc, &loader) {
        return Err(refusal.into());
    }
    pyrographer_core::codec::idb::build(&loader)?;

    let device = choose_target(selector.as_ref())?;
    device.reject_soc(soc)?;
    let mut render = ProgressRenderer::new("Writing", "Wrote");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut agent = device.open().await?;
        let plan = verbs::plan_write_id_block(&mut agent, &loader, soc).await?;
        execute_firmware_write(
            &mut agent,
            &device,
            None,
            plan,
            dry_run,
            yes,
            &mut render,
            &cancel,
        )
        .await
    })?;
    Ok(())
}

/// The steps a firmware plan goes through once it exists: show it, stop for a dry
/// run, ask the gate, ask the person, write.
///
/// `package` is the path of the package a package plan streams from. The file is
/// opened again for the write and read forward from its first byte.
#[allow(clippy::too_many_arguments)]
async fn execute_firmware_write(
    agent: &mut FlashAgent<UsbTransport>,
    device: &Chosen,
    package: Option<&str>,
    plan: FirmwarePlan,
    dry_run: bool,
    yes: bool,
    render: &mut ProgressRenderer,
    cancel: &Cancel,
) -> std::result::Result<(), CliError> {
    print!("{}", render_firmware_plan(device, package, &plan));

    if dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }

    // Asked before the prompt, so nobody confirms a write that was never going to
    // run. The write makes the same refusal from the same function.
    if let Some(refused) = verbs::firmware_refusal(agent, &plan) {
        return Err(refused.into());
    }

    if !confirmed(yes)? {
        println!("Nothing was written.");
        return Ok(());
    }

    let closing = match &plan.what {
        FirmwareWrite::Package { .. } => format!(
            "Wrote {} runs from {} and read every window back.",
            plan.runs.len(),
            package.unwrap_or("the package")
        ),
        FirmwareWrite::IdBlock => {
            "Wrote the ID block at sector 64 and read every window of it back.".to_string()
        }
    };

    match package {
        Some(path) => {
            let file =
                File::open(path).map_err(|e| Error::Io(format!("cannot open {path}: {e}")))?;
            let mut image = SyncReader::new(BufReader::new(file));
            verbs::write_firmware(
                agent,
                plan.confirm(),
                Some(&mut image),
                &mut |event| render.render(event),
                cancel,
            )
            .await?;
        }
        None => {
            verbs::write_firmware(
                agent,
                plan.confirm(),
                None,
                &mut |event| render.render(event),
                cancel,
            )
            .await?;
        }
    }

    println!("{closing}");
    Ok(())
}

/// Render a [`FirmwarePlan`] for a person about to allow it.
///
/// A firmware write is a write, so this shows what a write's plan shows: which
/// device, its geometry, the gate's evidence and verdict, and when the write is
/// read back. It also shows what is particular to it: every run in the order it is
/// written, the images the ID block holds and where, the partitions the table
/// holds afterward, the entries left out and why, what the loader file claims, and
/// what the running loader said it can do.
fn render_firmware_plan(device: &Chosen, package: Option<&str>, plan: &FirmwarePlan) -> String {
    let sector_size = u64::from(plan.flash.sector_size);
    let (head, table_note) = match &plan.what {
        FirmwareWrite::Package {
            model,
            manufacturer,
            version,
        } => (
            format!(
                "This will write the firmware package {} to {}:\n{model} by {manufacturer}, \
                 version {}.\n",
                package.unwrap_or("(unnamed)"),
                device.subject_short(),
                rkfw::version_text(*version)
            ),
            "The package's partition table replaces the device's.",
        ),
        FirmwareWrite::IdBlock => (
            format!(
                "This will write an ID block to {}.\n",
                device.subject_short()
            ),
            "The partition table is left as it is.",
        ),
    };

    let run_lines: Vec<String> = plan
        .runs
        .iter()
        .flat_map(|run| {
            let last = (run.lba + run.sectors).saturating_sub(1);
            [
                run.what.clone(),
                format!(
                    "LBA {} through {last} ({} sectors), {}",
                    run.lba,
                    run.sectors,
                    human_bytes(run.bytes)
                ),
            ]
        })
        .collect();
    let mut id_block_lines = vec![format!(
        "laid out by the {} header, every hash checked",
        plan.id_block_header
    )];
    id_block_lines.extend(plan.id_block_images.iter().map(render_id_block_image));

    let mut out = format!("\n{head}\n");
    out += &render_column("writing", &run_lines);
    out += &render_column("ID block", &id_block_lines);
    out += &render_column("partitions", &render_partition_lines(&plan.partitions));
    if !plan.skipped.is_empty() {
        let skipped: Vec<String> = plan
            .skipped
            .iter()
            .map(|skipped| format!("{}: {}", skipped.name, skipped.why))
            .collect();
        out += &render_column("not written", &skipped);
    }
    out += &render_column("loader file", &[loader_claim(plan.loader_chip)]);
    let capability = match &plan.capability {
        LoaderCapability::NoLoader => None,
        LoaderCapability::Answered(capability) if capability.new_idb() => {
            Some("NEW_IDB set: the loader writes an RKNS ID block".to_string())
        }
        LoaderCapability::Answered(_) => {
            Some("NEW_IDB not set: the write will be refused".to_string())
        }
        LoaderCapability::NotAnswered(why) => {
            Some(format!("no answer ({why}): the write will be refused"))
        }
    };
    if let Some(line) = capability {
        out += &render_column("capability", &[line]);
    }
    out += &format!(
        "  {:<12} {} ({} sectors of {sector_size} bytes)\n",
        device.medium_word(),
        human_bytes(plan.flash.size_bytes),
        plan.flash.size_bytes / sector_size
    );
    out += &device.render_gate(&plan.soc, &plan.chip_version);
    out += &format!(
        "\n{table_note} {}\nNothing here can be undone.\n",
        plan.read_back.describe()
    );
    out
}

/// What `verify` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct VerifyArgs {
    target: Target,
    path: String,
}

/// Read `verify`'s target and its file.
///
/// It takes no sector count. The file's length is how much of the flash the
/// comparison covers. A length the caller restates is a length the caller can
/// get wrong.
fn parse_verify(args: &mut pico_args::Arguments) -> std::result::Result<VerifyArgs, CliError> {
    let target = match partition_option(args)? {
        Some(name) => Target::Partition(name),
        None => Target::Lba(
            args.free_from_str()
                .map_err(|e| CliError::Usage(format!("verify needs an LBA to start at: {e}")))?,
        ),
    };

    let path = args
        .free_from_str()
        .map_err(|e| CliError::Usage(format!("verify needs a file to compare against: {e}")))?;

    Ok(VerifyArgs { target, path })
}

fn cmd_verify(mut args: pico_args::Arguments) -> Run {
    let selector = device_option(&mut args)?;
    let VerifyArgs { target, path } = parse_verify(&mut args)?;
    no_more(args)?;

    let file = File::open(&path).map_err(|e| Error::Io(format!("cannot open {path}: {e}")))?;
    let image_bytes = file
        .metadata()
        .map_err(|e| Error::Io(format!("cannot measure {path}: {e}")))?
        .len();
    let mut image = SyncReader::new(BufReader::new(file));

    let device = choose_target(selector.as_ref())?;
    let mut render = ProgressRenderer::new("Verifying", "Verified");
    let cancel = Cancel::new();

    let fill = pollster::block_on(async {
        let mut agent = device.open().await?;

        let lba = match &target {
            Target::Lba(lba) => {
                // As in `dump`: a DFU board serves a named region, not a raw LBA.
                if !agent.caps().can_address_raw_lba {
                    return Err(raw_lba_unsupported("verify"));
                }
                *lba
            }
            Target::Partition(name) => {
                let partition = verbs::find_partition(&mut agent, name).await?;
                // An image bigger than the partition cannot be what is in the
                // partition, so the answer is no, and saying that is better than
                // comparing on into the next partition and reporting a mismatch
                // at whatever byte the two first happen to differ at.
                partition.must_hold(image_bytes, agent.sector_size())?;
                println!(
                    "Partition:    {} (LBA {}, {} sectors)",
                    partition.name, partition.first_lba, partition.sectors,
                );
                partition.first_lba
            }
        };

        verbs::verify(
            &mut agent,
            lba,
            &mut image,
            image_bytes,
            &mut |event| render.render(event),
            &cancel,
        )
        .await
    })?;

    println!("Flash matches {path}.");
    report_fill(&fill);
    Ok(())
}

fn cmd_reset(mut args: pico_args::Arguments) -> Run {
    // Parsed before a device is chosen, so a name that is not a mode is caught as
    // a usage problem and nothing is scanned or opened.
    let mode = reset_mode_option(&mut args)?;
    let device = choose_target(device_option(&mut args)?.as_ref())?;
    device.usb("reset")?;
    no_more(args)?;

    // Said before the command goes out, not after. A board asked to power off may
    // be gone before anything else is printed, and a caution a person reads only
    // once the board has already acted is not a caution.
    if mode.untried() {
        println!(
            "{}: this subcode comes from the reference tools, which agree on it, and is untried \
             on a board. It writes no flash. [COMMUNITY]",
            mode.name()
        );
    }

    pollster::block_on(async {
        let mut agent = device.open().await?;
        verbs::reset(&mut agent, mode).await
    })?;
    println!("{}", mode.outcome());
    if let Some(step) = reset_next_step(mode) {
        println!("{step}");
    }
    Ok(())
}

/// The command that carries out a core outcome's or hint's next step.
///
/// Core's words are front-end neutral, because the window shows them too. They
/// describe an action ("upload a loader") rather than name a command. A person at
/// a prompt needs the command, so the CLI prints it after core's sentence.
const RUN_DB: &str = "Run `pyrographer db --loader <file>` to upload a loader.";

/// The command that reads a loader's chip version, named where core's words say
/// to read one.
const RUN_CHIPVER: &str = "Run `pyrographer chipver` to read a loader's chip version.";

/// What to run after `reset`, for the one mode whose outcome is an action to take.
fn reset_next_step(mode: ResetMode) -> Option<&'static str> {
    match mode {
        ResetMode::Maskrom => Some(RUN_DB),
        ResetMode::Reset | ResetMode::MassStorage | ResetMode::PowerOff => None,
    }
}

/// What to run after a core error's hint, where the hint names an action and the
/// CLI has a command for it.
///
/// Keyed on the error, so a front-end-neutral hint ("upload a loader", "read the
/// loader's chip version", "repair or author a table") reaches a CLI user with the
/// command that does it.
fn error_next_step(err: &Error) -> Option<&'static str> {
    match err {
        Error::WrongMode {
            found: "maskrom", ..
        } => Some(RUN_DB),
        Error::LoaderMismatch { .. } => Some(RUN_CHIPVER),
        Error::CorruptTable { format, .. } if *format == TableFormat::Gpt.name() => Some(
            "Run `pyrographer repair-table` to repair the GPT, or `pyrographer author-gpt` to \
             author a fresh one.",
        ),
        Error::CorruptTable { format, .. } if *format == TableFormat::RockchipParam.name() => Some(
            "Run `pyrographer repair-param` to repair the parameter table, or `pyrographer \
                 author-param` to author a fresh one.",
        ),
        _ => None,
    }
}

/// Where `db` gets its sections: an RKBOOT container, or the raw stages bare.
#[derive(Debug, PartialEq, Eq)]
enum DbSource {
    /// An rkbin `_loader.bin`: the 471 and 472 sections come out of the
    /// container, with the delays it asks for.
    Container(String),
    /// Bare 471/472 payload files with no container around them, as mainline
    /// U-Boot's binman emits (`u-boot-rockchip-usb471.bin`, `-usb472.bin`).
    /// At least one is present, and 471 is always sent first.
    Raw {
        /// The DRAM-init stage, if given.
        code_471: Option<String>,
        /// The loader stage, if given.
        code_472: Option<String>,
    },
}

/// Read `db`'s source: `--loader`, or `--code471`/`--code472` in its place.
///
/// The two forms are exclusive. A container names its own sections, so raw files
/// beside it can only contradict it. One of the two is required, because a `db`
/// with nothing to upload has no meaning.
fn parse_db(args: &mut pico_args::Arguments) -> std::result::Result<DbSource, CliError> {
    let loader: Option<String> = args
        .opt_value_from_str("--loader")
        .map_err(|e| CliError::Usage(format!("--loader names a file: {e}")))?;
    let code_471: Option<String> = args
        .opt_value_from_str("--code471")
        .map_err(|e| CliError::Usage(format!("--code471 names a file: {e}")))?;
    let code_472: Option<String> = args
        .opt_value_from_str("--code472")
        .map_err(|e| CliError::Usage(format!("--code472 names a file: {e}")))?;

    match (loader, code_471, code_472) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(CliError::Usage(
            "db takes --loader, or the raw --code471/--code472 stages in its place, not both"
                .to_string(),
        )),
        (Some(path), None, None) => Ok(DbSource::Container(path)),
        (None, None, None) => Err(CliError::Usage(format!(
            "db needs --loader <file>, or --code471/--code472 <file>\n\n{HELP}"
        ))),
        (None, code_471, code_472) => Ok(DbSource::Raw { code_471, code_472 }),
    }
}

/// Upload a loader to a maskrom board, bringing it to loader mode.
///
/// The loader file is parsed before a device is chosen. A file that is not a
/// loader is therefore caught as a usage error, and nothing is opened. The board
/// must be in maskrom, because a loader-mode board has already been through this.
/// After the upload, the board re-enumerates, and the CLI waits for it and
/// reports.
fn cmd_db(mut args: pico_args::Arguments) -> Run {
    let selector = device_option(&mut args)?;
    let soc = soc_option(&mut args)?;
    let source = parse_db(&mut args)?;
    no_more(args)?;

    // Read and parse before opening anything: a file that is not a loader is a
    // usage problem, and one caught here has cost no device an open. A raw
    // stage has no structure to parse -- whatever the file holds is what the
    // BootROM gets -- so for those the only thing to catch here is a file that
    // cannot be read.
    let loader = match source {
        DbSource::Container(path) => read_loader(&path)?,
        DbSource::Raw { code_471, code_472 } => {
            let read = |path: String| -> Result<(String, Vec<u8>)> {
                let bytes = std::fs::read(&path)
                    .map_err(|e| Error::Io(format!("cannot read {path}: {e}")))?;
                Ok((path, bytes))
            };
            rkboot::LoaderImage::from_raw(
                code_471.map(read).transpose()?,
                code_472.map(read).transpose()?,
            )
        }
    };

    // Say what the file claims, before anything is opened and whether or not a
    // SoC was named.
    print_loader_claim(&loader);
    // Refuse a file built for another SoC before a device is opened. The upload
    // is the one write-shaped act with nothing to read back afterwards, so this
    // is the only place the question can be asked at all.
    if let Some(refusal) = verbs::loader_blob_refusal(soc, &loader) {
        return Err(refusal.into());
    }

    // `db` uploads a loader over the maskrom download-boot protocol, so it names a board or
    // nothing. Shadowed rather than renamed: everything below wants the board.
    let device = choose_target(selector.as_ref())?;
    let device = device.usb("db")?;
    if device.vendor != pyrographer_core::discovery::Vendor::Rockchip {
        return Err(Error::NotImplemented(
            "db uploads a Rockchip loader. Bootstrap an Ingenic board to DFU with `usbboot` \
             instead",
        )
        .into());
    }
    if device.mode != Mode::Maskrom {
        return Err(Error::WrongMode {
            found: device.mode.name(),
            needed: "maskrom",
        }
        .into());
    }

    // Snapshot the bus before the upload, so the loader that reappears can be told
    // from a board that was connected all along -- on a two-board bench, matching by
    // "same bus, different address" alone would find the other board.
    let present_before = verbs::list().unwrap_or_default();

    let mut render = ProgressRenderer::new("Uploading", "Uploaded");
    let cancel = Cancel::new();

    pollster::block_on(async {
        let mut transport = UsbTransport::open_maskrom(device).await?;
        verbs::download_boot(
            &mut transport,
            &loader,
            soc,
            &mut |event| render.render(event),
            &cancel,
        )
        .await
    })?;

    // The board tears down its maskrom USB device on the 0x0472 jump and comes
    // back as a different device -- on a real RK3576, somewhat over three
    // seconds later. Waiting for it here is best effort: it matches a Rockchip
    // device on the same bus at a new address, not a loader-mode flag, because
    // the RK3576 SPL loader keeps the flag even and would never match one.
    // `device` was refused above unless it is Rockchip, so matching its vendor
    // is matching Rockchip.
    //
    // The watcher sends the new device nothing, so the message claims no more
    // than a new device on the bus. Whether a loader answers is settled by the
    // next command: every verb probes a maskrom-flagged board before trusting
    // it.
    match await_loader(device, &present_before) {
        Some(found) => println!(
            "Loader uploaded. A new Rockchip device appeared at {}:{}. `list` can still \
             report it as maskrom, because this loader keeps the bcdUSB flag even. The next \
             command sent to it checks that a loader answers.",
            found.bus_id, found.device_address
        ),
        None => println!(
            "Loader uploaded. The board did not reappear within ten seconds. Run `list` to \
             check. A full U-Boot answers on the board's serial port rather than on the bus, \
             and `console` reads it."
        ),
    }
    Ok(())
}

/// The default DRAM settle after stage1, in milliseconds.
///
/// It is core's own [`pyrographer_core::bootstrap::ingenic::DEFAULT_SETTLE_MS`],
/// so the CLI and the GUI start from one number rather than two that can drift
/// apart. `--dram-settle-ms` overrides it.
///
/// thingino-dfu's T31 profile waits 2000 ms for the memory controller to come up
/// before stage2 is loaded into DRAM. **\[COMMUNITY\]**
const DEFAULT_SETTLE_MS: u32 = pyrographer_core::bootstrap::ingenic::DEFAULT_SETTLE_MS;

/// What `usbboot` reads off the command line: the two stages, and where each loads.
#[derive(Debug, PartialEq, Eq)]
struct UsbBootArgs {
    /// The DRAM-init SPL, uploaded first and run from SRAM.
    stage1: String,
    /// Where stage1 loads and runs.
    stage1_addr: u32,
    /// The DFU U-Boot, uploaded after stage1 and run from DRAM.
    stage2: Option<String>,
    /// Where stage2 loads and runs. When `stage2` is given, this is required.
    stage2_addr: Option<u32>,
    /// Milliseconds to settle after stage1 while DRAM comes up.
    settle_ms: u32,
}

/// Parse a `0x`-prefixed (or bare) 32-bit hex address.
fn parse_u32_hex(s: &str) -> std::result::Result<u32, String> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(digits, 16).map_err(|e| format!("'{s}' is not a 32-bit hex address: {e}"))
}

/// Read `usbboot`'s stages and addresses.
///
/// stage1 is always required, because there is no bootstrap without DRAM init. A
/// stage2 and its address go together. A U-Boot with nowhere to load, or a load
/// address with no U-Boot, is refused as a command line with no meaning.
fn parse_usbboot(args: &mut pico_args::Arguments) -> std::result::Result<UsbBootArgs, CliError> {
    let stage1: String = args.value_from_str("--stage1").map_err(|e| {
        CliError::Usage(format!(
            "usbboot needs --stage1 <file>, the DRAM-init SPL: {e}"
        ))
    })?;
    let stage1_addr: u32 = args
        .value_from_fn("--stage1-addr", parse_u32_hex)
        .map_err(|e| CliError::Usage(format!("usbboot needs --stage1-addr <hex>: {e}")))?;
    let stage2: Option<String> = args
        .opt_value_from_str("--stage2")
        .map_err(|e| CliError::Usage(format!("--stage2 names a file: {e}")))?;
    let stage2_addr: Option<u32> = args
        .opt_value_from_fn("--stage2-addr", parse_u32_hex)
        .map_err(|e| CliError::Usage(format!("--stage2-addr takes a hex address: {e}")))?;
    let settle_ms: u32 = args
        .opt_value_from_str("--dram-settle-ms")
        .map_err(|e| CliError::Usage(format!("--dram-settle-ms is a number of milliseconds: {e}")))?
        .unwrap_or(DEFAULT_SETTLE_MS);

    match (&stage2, stage2_addr) {
        (Some(_), None) => Err(CliError::Usage(
            "--stage2 needs --stage2-addr <hex>, where the U-Boot loads and runs".to_string(),
        )),
        (None, Some(_)) => Err(CliError::Usage(
            "--stage2-addr was given with no --stage2 to place".to_string(),
        )),
        _ => Ok(UsbBootArgs {
            stage1,
            stage1_addr,
            stage2,
            stage2_addr,
            settle_ms,
        }),
    }
}

/// Bootstrap an Ingenic XBurst boot-ROM board to DFU mode.
///
/// It is the Ingenic counterpart of [`cmd_db`]. It uploads a two-stage loader
/// over the `VR_*` protocol, where `db` uploads a Rockchip loader over the
/// download-boot. The stages are read before a device is opened, so an
/// unreadable file costs no device an open. There is no container to parse, so
/// whatever the file holds is what the boot ROM gets. Unlike `db`, the boot ROM
/// speaks over a bulk pair as well as endpoint 0, so the device is opened with
/// the ordinary [`UsbTransport::open`].
///
/// On success, the board reports its SoC magic. It is the one point in the flow
/// where the identity is readable. The CLI prints it for the person. Running this
/// against a real board is also how the `soc` entry the write gate needs is
/// pinned.
fn cmd_usbboot(mut args: pico_args::Arguments) -> Run {
    let selector = device_option(&mut args)?;
    let UsbBootArgs {
        stage1,
        stage1_addr,
        stage2,
        stage2_addr,
        settle_ms,
    } = parse_usbboot(&mut args)?;
    no_more(args)?;

    let stage1_data =
        std::fs::read(&stage1).map_err(|e| Error::Io(format!("cannot read {stage1}: {e}")))?;
    let stage2 = match (stage2, stage2_addr) {
        (Some(path), Some(addr)) => {
            let data =
                std::fs::read(&path).map_err(|e| Error::Io(format!("cannot read {path}: {e}")))?;
            Some(Stage {
                name: path,
                data,
                load_address: addr,
                entry_address: addr,
                settle_ms: 0,
            })
        }
        _ => None,
    };

    let loader = IngenicLoader {
        stage1: Stage {
            name: stage1,
            data: stage1_data,
            load_address: stage1_addr,
            entry_address: stage1_addr,
            settle_ms,
        },
        stage2,
    };

    // `usbboot` uploads a loader over the Ingenic boot ROM's protocol, so it names a board or
    // nothing. Shadowed rather than renamed: everything below wants the board.
    let device = choose_target(selector.as_ref())?;
    let device = device.usb("usbboot")?;
    if device.vendor != discovery::Vendor::Ingenic {
        return Err(Error::NotImplemented(
            "usbboot bootstraps an Ingenic XBurst board, and this device is not one",
        )
        .into());
    }
    if device.mode != Mode::BootRom {
        return Err(Error::WrongMode {
            found: device.mode.name(),
            needed: "boot ROM",
        }
        .into());
    }

    let mut render = ProgressRenderer::new("Uploading", "Uploaded");
    let cancel = Cancel::new();

    let cpu = pollster::block_on(async {
        let mut transport = UsbTransport::open(device).await?;
        verbs::ingenic_download_boot(
            &mut transport,
            &loader,
            &mut |event| render.render(event),
            &cancel,
        )
        .await
    })?;

    // The magic is the thing worth printing: it is the SoC's own account of itself,
    // and running this on real hardware is how that account is first read -- the
    // value a `soc` entry is later pinned from.
    println!(
        "Bootstrapped. The board reported CPU info {} (\"{}\").\n\
         The board then re-enumerates as a DFU device ({:04x}:{:04x}). Run `list` to find it.",
        hex(&cpu.magic),
        cpu.text(),
        discovery::INGENIC_VID,
        discovery::INGENIC_DFU_PID
    );
    Ok(())
}

/// What `recover` reads off the command line.
///
/// A StarFive recovery names a serial port rather than a bus device. There is
/// nothing to scan, so there is nothing to select with `--device`. It also names
/// the recovery agent to upload, and at least one thing to write.
#[derive(Debug, PartialEq, Eq)]
struct RecoverArgs {
    /// The serial port path: `/dev/ttyUSB0`, `COM3`.
    port: String,
    /// The recovery agent (`jh7110-recovery-*.bin`), uploaded first.
    agent: String,
    /// The SPL, raw or headered. Optional, but at least one of this and `uboot`
    /// is required.
    spl: Option<String>,
    /// A U-Boot FIT payload, sent as-is. Optional, on the same terms as `spl`.
    uboot: Option<String>,
    /// Skip the confirmation prompt.
    yes: bool,
    /// Produce the plan and stop, without opening the port.
    dry_run: bool,
}

/// Read `recover`'s port and files.
///
/// A missing or unparsable option is a usage error only. No port has been opened
/// and no board has been touched. This function also checks that at least one of
/// `--spl` or `--uboot` is present, because a recovery that writes nothing has no
/// meaning.
fn parse_recover(args: &mut pico_args::Arguments) -> std::result::Result<RecoverArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");

    let port: String = args
        .value_from_str("--port")
        .map_err(|e| CliError::Usage(format!("recover needs --port <path>: {e}")))?;

    let agent: String = args.value_from_str("--agent").map_err(|e| {
        CliError::Usage(format!(
            "recover needs --agent <file>, the jh7110-recovery-*.bin: {e}"
        ))
    })?;

    let spl: Option<String> = args
        .opt_value_from_str("--spl")
        .map_err(|e| CliError::Usage(format!("--spl names a u-boot-spl.bin: {e}")))?;
    let uboot: Option<String> = args
        .opt_value_from_str("--uboot")
        .map_err(|e| CliError::Usage(format!("--uboot names a U-Boot payload: {e}")))?;

    if spl.is_none() && uboot.is_none() {
        return Err(CliError::Usage(
            "recover writes at least one of --spl or --uboot, and the command named neither"
                .to_string(),
        ));
    }

    Ok(RecoverArgs {
        port,
        agent,
        spl,
        uboot,
        yes,
        dry_run,
    })
}

/// Recover a StarFive JH7110 board over a serial line.
///
/// Unlike the USB verbs, this opens no bus device. A serial port is a path the
/// person named, so the plan is built and shown without touching the port. The
/// port is opened only once the write is confirmed.
///
/// This plan alone says that the write will not be read back. The protocol
/// cannot read flash, and a person recovering a bootloader must know that before
/// they agree to it.
fn cmd_recover(mut args: pico_args::Arguments) -> Run {
    let RecoverArgs {
        port,
        agent,
        spl,
        uboot,
        yes,
        dry_run,
    } = parse_recover(&mut args)?;
    no_more(args)?;

    // Read the files whole. These are bootloaders -- tens of KB to a few MB -- and
    // XMODEM sends a whole file, so unlike an eMMC image there is nothing here
    // that will not fit in memory.
    let agent_bytes =
        std::fs::read(&agent).map_err(|e| Error::Io(format!("cannot read {agent}: {e}")))?;
    let spl_bytes = read_optional(spl.as_deref())?;
    let uboot_bytes = read_optional(uboot.as_deref())?;

    let request = RecoveryRequest {
        agent: &agent_bytes,
        spl: spl_bytes.as_deref(),
        uboot: uboot_bytes.as_deref(),
    };

    // Pure: it opens no port and sends nothing. The dry run is this and no more.
    let plan = recovery::plan_recover(&request)?;
    print!("{}", render_recovery_plan(&port, &plan));

    if dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }

    if !confirmed(yes)? {
        println!("Nothing was written.");
        return Ok(());
    }

    let mut render = ProgressRenderer::new("Sending", "Sent");
    let cancel = Cancel::new();

    let mut serial = SerialTransport::open(&port)?;
    eprintln!("Waiting for the board. Power it on, strapped into UART recovery.");
    pollster::block_on(recovery::recover(
        &mut serial,
        &request,
        plan.confirm(),
        &mut |event| render.render(event),
        &mut echo_console,
        &cancel,
    ))?;

    println!(
        "\nRecovery complete. The agent reported every write as done. This board's write \
         cannot be read back."
    );
    println!("Power off, return the boot strap to normal, and power on.");
    Ok(())
}

/// What `uartboot` reads off the command line.
#[derive(Debug, PartialEq, Eq)]
struct UartbootArgs {
    /// The serial port path: `/dev/ttyUSB0`, `COM3`.
    port: String,
    /// The SPL, raw or headered, built to load U-Boot by YMODEM.
    spl: String,
    /// The U-Boot FIT the SPL loads.
    uboot: String,
    /// The prompt the U-Boot being sent presents.
    prompt: String,
}

/// Read `uartboot`'s port, files and prompt.
fn parse_uartboot(args: &mut pico_args::Arguments) -> std::result::Result<UartbootArgs, CliError> {
    let port: String = args
        .value_from_str("--port")
        .map_err(|e| CliError::Usage(format!("uartboot needs --port <path>: {e}")))?;
    let spl: String = args.value_from_str("--spl").map_err(|e| {
        CliError::Usage(format!(
            "uartboot needs --spl <file>, an SPL built with CONFIG_SPL_YMODEM_SUPPORT: {e}"
        ))
    })?;
    let uboot: String = args.value_from_str("--uboot").map_err(|e| {
        CliError::Usage(format!("uartboot needs --uboot <file>, a u-boot.itb: {e}"))
    })?;
    let prompt: String = args
        .opt_value_from_str("--prompt")
        .map_err(|e| CliError::Usage(format!("--prompt takes the prompt to match: {e}")))?
        .unwrap_or_else(|| pyrographer_core::uboot::DEFAULT_PROMPT.to_string());
    Ok(UartbootArgs {
        port,
        spl,
        uboot,
        prompt,
    })
}

/// Boot a StarFive JH7110 board into U-Boot over a serial line, writing nothing.
///
/// It is the serial counterpart of `db`: it leaves a full U-Boot running in DRAM,
/// stopped at its prompt, where `uboot` takes over on the same port. A RAM boot
/// writes nothing, so it asks for no confirmation, as `db` asks for none.
fn cmd_uartboot(mut args: pico_args::Arguments) -> Run {
    let UartbootArgs {
        port,
        spl,
        uboot,
        prompt,
    } = parse_uartboot(&mut args)?;
    no_more(args)?;

    let spl_bytes =
        std::fs::read(&spl).map_err(|e| Error::Io(format!("cannot read {spl}: {e}")))?;
    let uboot_bytes =
        std::fs::read(&uboot).map_err(|e| Error::Io(format!("cannot read {uboot}: {e}")))?;
    let request = UartBootRequest {
        spl: &spl_bytes,
        uboot: &uboot_bytes,
    };

    let plan = starfive::plan_uart_boot(&request)?;
    println!(
        "\nBooting U-Boot in RAM on a StarFive JH7110 board over {port}. Nothing is written to \
         the board.\n\n  \
         SPL          {}, sent to the BootROM, {}\n  \
         U-Boot       {}, sent to the SPL by YMODEM\n",
        human_bytes(plan.spl_bytes),
        origin_words(plan.spl_origin),
        human_bytes(plan.uboot_bytes),
    );

    let mut render = ProgressRenderer::new("Sending", "Sent");
    let cancel = Cancel::new();
    let mut serial = SerialTransport::open(&port)?;
    eprintln!("Waiting for the board. Power it on, strapped into UART recovery.");
    pollster::block_on(starfive::uart_boot(
        &mut serial,
        &request,
        &prompt,
        &mut |event| render.render(event),
        &mut echo_console,
        &cancel,
    ))?;

    println!(
        "\nU-Boot is running in RAM, stopped at its prompt. Nothing was written to the board."
    );
    println!(
        "Run `pyrographer uboot --port {port} --gadget ums` to hand its eMMC to this machine as a \
         disk."
    );
    Ok(())
}

/// How an SPL's header came to be, in the words a plan uses.
fn origin_words(origin: Origin) -> &'static str {
    match origin {
        Origin::Headered => "its header checked",
        Origin::HeaderedHere => "headered here",
    }
}

/// Read a file named by an optional path, keeping the `None` when there is none.
///
/// A recovery's SPL and U-Boot are each optional. This maps over the option and
/// lifts the I/O error out of it.
fn read_optional(path: Option<&str>) -> std::result::Result<Option<Vec<u8>>, CliError> {
    path.map(|p| std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}"))))
        .transpose()
        .map_err(CliError::from)
}

/// Render a [`RecoveryPlan`] for a person about to recover a board.
///
/// It names the port, and what is uploaded and written where. It says where the
/// SPL's backup copy lands. It also states the one fact that sets a StarFive
/// recovery apart from every other write: it is not read back. The protocol
/// cannot read flash, so the plan states that plainly, as a warning.
fn render_recovery_plan(port: &str, plan: &RecoveryPlan) -> String {
    let mut out = format!(
        "\n\
         This will write the boot flash of a StarFive JH7110 board over {port}.\n\
         \n  \
         agent        {}, sent to the BootROM first\n",
        human_bytes(plan.agent_bytes),
    );

    for stage in &plan.stages {
        let origin = match stage.origin {
            Some(origin) => format!(", {}", origin_words(origin)),
            None => String::new(),
        };
        out.push_str(&format!(
            "  {:<12} {} at {:#x}, agent menu entry {}{origin}\n",
            stage.kind.name(),
            human_bytes(stage.image_bytes),
            stage.offset,
            stage.menu_option,
        ));
    }

    if let Some(backup) = plan.describe_backup() {
        out.push_str(&format!("\n{backup}\n"));
    }

    if !plan.verified {
        out.push_str(
            "\n\
             Warning: this write is not read back. The recovery protocol cannot read\n\
             flash. Each block's acknowledgment confirms that the board received it,\n\
             and the agent's verdict that its write finished, not that the flash\n\
             holds it.\n",
        );
    }
    out.push_str("Nothing here can be undone.\n");
    out
}

/// Echo a console transcript to the terminal as it arrives.
///
/// Core never prints, so the CLI passes this to the console verbs as their
/// [`ConsoleSink`]. It writes to **stderr**, because a transcript is what was
/// observed and not what the command answered. `uboot --cmd`'s output and
/// `console`'s verdict go to stdout, where a script reads them.
///
/// `codec::console::text` drops carriage returns and turns the countdown's
/// backspaces into `.`. A transcript piped into a log therefore stays legible,
/// rather than rewriting the line it lands on.
///
/// [`ConsoleSink`]: pyrographer_core::console::ConsoleSink
fn echo_console(bytes: &[u8]) {
    eprint!("{}", console_codec::text(bytes));
    let _ = std::io::stderr().flush();
}

/// Read a repeated pattern option (`--expect`, `--or-fail`) into the bytes it
/// names.
///
/// Matching is byte-oriented. The escapes `codec::console::unescape` defines let
/// a person write a byte the shell would otherwise remove or interpret.
fn pattern_option(
    args: &mut pico_args::Arguments,
    name: &'static str,
) -> std::result::Result<Vec<Vec<u8>>, CliError> {
    let typed: Vec<String> = args
        .values_from_str(name)
        .map_err(|e| CliError::Usage(format!("{name} takes a pattern: {e}")))?;
    typed
        .iter()
        .map(|text| console::pattern(text).map_err(CliError::from))
        .collect()
}

/// A pattern as a person wrote it, for a line a person reads back.
fn show_pattern(pattern: &[u8]) -> String {
    console_codec::text(pattern)
}

/// The serial-line options every console verb shares.
struct LineArgs {
    /// The serial port path: `/dev/ttyUSB0`, `COM3`.
    port: String,
    /// The rate to open it at.
    baud: u32,
    /// How many reads to spend on each wait.
    reads: u32,
}

/// Read `--port`, `--baud`, and `--reads`.
fn parse_line(args: &mut pico_args::Arguments) -> std::result::Result<LineArgs, CliError> {
    let port: String = args
        .value_from_str("--port")
        .map_err(|e| CliError::Usage(format!("this needs --port <path>: {e}")))?;
    let baud: u32 = args
        .opt_value_from_str("--baud")
        .map_err(|e| CliError::Usage(format!("--baud takes a rate, like 115200: {e}")))?
        .unwrap_or(DEFAULT_BAUD);
    let reads: u32 = args
        .opt_value_from_str("--reads")
        .map_err(|e| CliError::Usage(format!("--reads takes a count: {e}")))?
        .unwrap_or(console::DEFAULT_READS);

    if reads == 0 {
        return Err(CliError::Usage(
            "--reads must be at least 1. With 0, the command waits for no output".to_string(),
        ));
    }
    Ok(LineArgs { port, baud, reads })
}

/// What `console` reads off the command line.
struct ConsoleArgs {
    line: LineArgs,
    expect: Vec<Vec<u8>>,
    fail: Vec<Vec<u8>>,
}

/// Read `console`'s port and patterns.
fn parse_console(args: &mut pico_args::Arguments) -> std::result::Result<ConsoleArgs, CliError> {
    let expect = pattern_option(args, "--expect")?;
    let fail = pattern_option(args, "--or-fail")?;
    let line = parse_line(args)?;

    if expect.is_empty() && fail.is_empty() {
        return Err(CliError::Usage(
            "console needs a pattern to watch for: --expect <pattern>, or --or-fail <pattern> \
             for the text that means the board reported a failure"
                .to_string(),
        ));
    }
    Ok(ConsoleArgs { line, expect, fail })
}

/// Watch a serial console for the text that says it worked, or that it did not.
///
/// This is the passive half of the console. It needs no prompt, no echo
/// handling, and no login or credentials. A node that runs its own self-test at
/// boot needs this from a host: the node prints its verdict, and this command
/// reads it.
///
/// **It reports what appeared, and does not assert a result.** An `--or-fail`
/// pattern exits non-zero, as an unattended run needs, but the message says only
/// what was seen. A budget spent with nothing matching is reported as the pattern
/// not appearing, not as a board failure. The cause can be any of these:
///
/// - A failure
/// - A slow boot
/// - A wrong baud rate
/// - A console on another UART
fn cmd_console(mut args: pico_args::Arguments) -> Run {
    let ConsoleArgs { line, expect, fail } = parse_console(&mut args)?;
    no_more(args)?;

    let cancel = Cancel::new();
    let watched = pollster::block_on(async {
        let mut serial = SerialTransport::open_at(&line.port, line.baud)?;
        console::watch(
            &mut serial,
            &expect,
            &fail,
            line.reads,
            &mut echo_console,
            &cancel,
        )
        .await
    })?;

    let seen = show_pattern(&watched.pattern);
    match watched.seen {
        Seen::Expected => {
            println!("\nSaw \"{seen}\" on {}.", line.port);
            Ok(())
        }
        // A finding, not a fault: the board answered, and what it said was the
        // thing being watched for. Non-zero, because that is what it is for.
        Seen::Failed => Err(CliError::Finding(format!(
            "\nSaw \"{seen}\" on {}, the pattern that means the board reported a failure.",
            line.port
        ))),
    }
}

/// Which of `uboot`'s three forms was asked for.
///
/// Exactly one is taken, as `db` takes `--loader` or the raw
/// `--code471`/`--code472` stages and never both.
enum UbootAction {
    /// Start a gadget on the named block device.
    Gadget(Gadget, GadgetDevice),
    /// Set `boot_targets` for one boot, and boot.
    BootFrom(String),
    /// Type one line at the prompt and print what came back.
    Command(String),
}

/// What `uboot` reads off the command line.
struct UbootArgs {
    line: LineArgs,
    /// The prompt to match. Boards use different prompts, so this is a parameter.
    prompt: String,
    action: UbootAction,
    /// Skip the confirmation a boot override asks for.
    yes: bool,
    /// Produce the boot-override plan and stop.
    dry_run: bool,
}

/// Read `uboot`'s port, prompt, and the one form it was asked for.
fn parse_uboot(args: &mut pico_args::Arguments) -> std::result::Result<UbootArgs, CliError> {
    let yes = args.contains("--yes");
    let dry_run = args.contains("--dry-run");

    let gadget: Option<String> = args
        .opt_value_from_str("--gadget")
        .map_err(|e| CliError::Usage(format!("--gadget names a gadget, rockusb or ums: {e}")))?;
    let gadget_dev: Option<String> = args
        .opt_value_from_str("--gadget-dev")
        .map_err(|e| CliError::Usage(format!("--gadget-dev names a device, like mmc:0: {e}")))?;
    let boot_from: Option<String> = args
        .opt_value_from_str("--boot-from")
        .map_err(|e| CliError::Usage(format!("--boot-from names a boot order: {e}")))?;
    let command: Option<String> = args
        .opt_value_from_str("--cmd")
        .map_err(|e| CliError::Usage(format!("--cmd takes one command line: {e}")))?;

    let prompt: String = args
        .opt_value_from_str("--prompt")
        .map_err(|e| CliError::Usage(format!("--prompt takes the prompt to match: {e}")))?
        .unwrap_or_else(|| pyrographer_core::uboot::DEFAULT_PROMPT.to_string());

    let named = [gadget.is_some(), boot_from.is_some(), command.is_some()]
        .iter()
        .filter(|named| **named)
        .count();
    if named != 1 {
        return Err(CliError::Usage(
            "uboot does one of three things, and takes exactly one of them: --gadget <gadget>, \
             --boot-from <targets>, or --cmd <line>"
                .to_string(),
        ));
    }
    if gadget_dev.is_some() && gadget.is_none() {
        return Err(CliError::Usage(
            "--gadget-dev names the block device a gadget exposes, so it requires --gadget"
                .to_string(),
        ));
    }

    let action = if let Some(gadget) = gadget {
        let gadget = Gadget::parse(&gadget)?;
        let device = match gadget_dev.as_deref() {
            Some(spec) => GadgetDevice::parse(spec)?,
            None => GadgetDevice::default(),
        };
        UbootAction::Gadget(gadget, device)
    } else if let Some(targets) = boot_from {
        UbootAction::BootFrom(targets)
    } else {
        UbootAction::Command(command.expect("exactly one form was named"))
    };

    let line = parse_line(args)?;
    Ok(UbootArgs {
        line,
        prompt,
        action,
        yes,
        dry_run,
    })
}

/// How many reads to spend watching a board get under way after `boot`.
///
/// It is long enough for a person to see the first banners, and short enough that
/// the command ends instead of acting as a terminal.
const BOOT_WATCH_READS: u32 = 6;

/// Drive a U-Boot prompt over a serial line.
///
/// Every form starts the same way. It opens the port, interrupts the autoboot,
/// and reaches the prompt. The interrupt is a bare newline, which is idempotent
/// against a board already at a prompt. The one form that was named then runs.
fn cmd_uboot(mut args: pico_args::Arguments) -> Run {
    let UbootArgs {
        line,
        prompt,
        action,
        yes,
        dry_run,
    } = parse_uboot(&mut args)?;
    no_more(args)?;

    let cancel = Cancel::new();
    let serial = SerialTransport::open_at(&line.port, line.baud)?;
    let mut uboot = UBoot::new(serial)
        .with_prompt(&prompt)
        .with_reads(line.reads);

    pollster::block_on(uboot.interrupt_autoboot(&mut echo_console, &cancel))?;
    eprintln!("\nAt the prompt.");

    match action {
        UbootAction::Gadget(gadget, device) => {
            let sent = pollster::block_on(uboot.start_gadget(
                gadget,
                &device,
                &mut echo_console,
                &cancel,
            ))?;
            println!("\nRan `{sent}`. The gadget is running, so the prompt does not return.");
            match gadget {
                Gadget::Rockusb => println!("Run `pyrographer list` to find the board on USB."),
                Gadget::Ums => {
                    println!("Run `pyrographer list --blocks` to find the board's flash as a disk.")
                }
            }
            Ok(())
        }

        UbootAction::Command(command) => {
            let output = pollster::block_on(uboot.run(&command, &mut echo_console, &cancel))?;
            // The answer goes to stdout, where a script reads it; the transcript
            // went to stderr on the way past.
            println!("{}", console_codec::text(&output));
            Ok(())
        }

        UbootAction::BootFrom(targets) => {
            let plan =
                pollster::block_on(uboot.plan_boot_override(&targets, &mut echo_console, &cancel))?;
            print!("{}", render_boot_plan(&line.port, &plan));

            if dry_run {
                println!("Dry run: the board was asked what it boots from, and nothing was set.");
                return Ok(());
            }
            if !confirmed_that(yes, NO_TERMINAL_FOR_BOOT)? {
                println!("Nothing was changed.");
                return Ok(());
            }

            pollster::block_on(async {
                uboot
                    .boot_override(plan.confirm(), &mut echo_console, &cancel)
                    .await?;
                // What the board says as it comes up. Best-effort: the override
                // has already gone out, so a line that goes quiet here is the
                // board booting, not a failure of the thing that was asked for.
                let _ = uboot
                    .drain(BOOT_WATCH_READS, &mut echo_console, &cancel)
                    .await;
                Ok::<(), Error>(())
            })?;

            println!("\nThe board is booting from {targets}, for this boot only.");
            println!("Nothing was saved, so the next reset restores the order it had.");
            Ok(())
        }
    }
}

/// Render a [`BootPlan`] for a person about to change what a board boots from.
///
/// It shows what the board boots from now beside what this would set. It also
/// states what sets this override apart from every other write in pyrographer:
/// it is not saved. It is confirmed with a plain yes rather than a typed
/// coordinate, because there is no second board and nothing is overwritten.
fn render_boot_plan(port: &str, plan: &BootPlan) -> String {
    let mut out = format!(
        "\n\
         This will change what the board on {port} boots from, for one boot.\n\
         \n  \
         boots from now   {}\n  \
         would boot from  {}\n",
        match &plan.current {
            Some(current) => current.clone(),
            None => "(this build sets no boot_targets at all)".to_string(),
        },
        plan.targets,
    );

    if plan.is_no_change() {
        out.push_str("\nThat is the order it already boots in, so this changes nothing.\n");
    }
    if !plan.persistent {
        out.push_str(
            "\n\
             Note: the change is not saved. U-Boot keeps its environment in RAM until\n\
             `saveenv` writes it to storage, and the override never sends `saveenv`. The\n\
             board's own boot order is untouched, and the next reset restores it. The\n\
             board boots immediately after the order is set.\n",
        );
    }
    out
}

/// How a person names the device to act on.
///
/// There are two spellings, for two kinds of device. A board is named by the bus
/// and address the operating system gave it, exactly as `list` prints them.
/// Naming one is copying a column, not composing it. It is deliberately not named
/// by product ID, because two identical boards share one, and telling them apart
/// is why the selector exists. A block device is named by its node, such as
/// `/dev/sdb`, the name the rest of the machine already uses for it.
///
/// The leading slash separates the two spellings, and nothing else is needed. A
/// bus and address never begins with one, and a device node always does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeviceSelector {
    /// A board on the USB bus, by bus and address.
    Usb {
        /// The bus, as `list` prints it: `003`.
        bus_id: String,
        /// The address on that bus.
        device_address: u8,
    },
    /// A block device the host operating system also owns, by its node path.
    Block(String),
}

impl DeviceSelector {
    /// Whether this names `device`.
    fn names(&self, device: &DeviceInfo) -> bool {
        match self {
            DeviceSelector::Usb {
                bus_id,
                device_address,
            } => device.bus_id == *bus_id && device.device_address == *device_address,
            DeviceSelector::Block(_) => false,
        }
    }
}

impl std::str::FromStr for DeviceSelector {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.starts_with('/') {
            return Ok(DeviceSelector::Block(s.to_string()));
        }
        let (bus_id, address) = s.split_once(':').ok_or_else(|| {
            format!(
                "'{s}' is neither a bus and an address, as in 003:12, nor a block device node, \
                 as in /dev/sdb"
            )
        })?;
        let device_address = address
            .parse()
            .map_err(|_| format!("'{address}' is not a device address"))?;

        Ok(DeviceSelector::Usb {
            bus_id: bus_id.to_string(),
            device_address,
        })
    }
}

impl fmt::Display for DeviceSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceSelector::Usb {
                bus_id,
                device_address,
            } => write!(f, "{bus_id}:{device_address}"),
            DeviceSelector::Block(node) => write!(f, "{node}"),
        }
    }
}

/// Read a `--device` option, if there is one.
///
/// It must be read before the positional arguments. pico-args takes free
/// arguments from the front of whatever is left, so an option still in the vector
/// would be read as a free argument.
fn device_option(
    args: &mut pico_args::Arguments,
) -> std::result::Result<Option<DeviceSelector>, CliError> {
    args.opt_value_from_str("--device")
        .map_err(|e| CliError::Usage(format!("--device names a device as bus:address: {e}")))
}

/// The device a verb acts on, once it has been chosen.
///
/// The two kinds differ in who else uses them. A board in a bootstrap mode is
/// pyrographer's alone, and nothing else on the host addresses it. A block device
/// is shared with the operating system, which mounts filesystems on it and caches
/// its sectors. It is the only target on which a mistake destroys the machine
/// pyrographer is running on, rather than the board.
///
/// The two share the agent. [`FlashAgent::Block`] carries no transport, so both
/// open into the same `FlashAgent<UsbTransport>`, and every verb in this file is
/// written once for both.
enum Chosen {
    /// A board on the USB bus.
    Usb(DeviceInfo),
    /// A block device the host operating system also owns.
    Block(BlockDevice),
}

impl Chosen {
    /// The `Device:` heading a command prints before it does anything.
    fn describe(&self) -> String {
        match self {
            Chosen::Usb(device) => format!(
                "Device:       {:04x}:{:04x} ({})\n",
                device.vendor_id,
                device.product_id,
                device.mode.name()
            ),
            Chosen::Block(device) => {
                let model = device
                    .model
                    .as_deref()
                    .map(|m| format!("  {m}"))
                    .unwrap_or_default();
                let mut out = format!(
                    "Device:       {}{model}  {} ({})\n",
                    device.node,
                    human_bytes(device.bytes),
                    device.bus.name(),
                );
                // Said before anything runs, because it is the difference
                // between this target and every other one: somebody else is
                // using it. The open is what refuses; this is what explains the
                // refusal before it arrives.
                if !device.mounts.is_empty() {
                    out.push_str(&format!("Mounted:      {}\n", device.mounts.join(", ")));
                }
                out
            }
        }
    }

    /// Open the device and build the agent every verb takes.
    ///
    /// A disk is opened for the **widest access it allows**. That is read and
    /// write where nothing refuses a write to it, and read only where something
    /// does. It lets `dump` image a card with its lock switch on. Such a card is
    /// the safest thing there is to image, and a common reason to reach for this
    /// tool. `flash` on the same card is still refused by
    /// [`pyrographer_core::verbs::write_refusal`], in the sentence that says why.
    ///
    /// The refusal is made at the plan instead of at the open. A write still meets
    /// every guard `block::write_refusal` names, before anything is confirmed.
    ///
    /// `block::open` also lists the disk again. If the disk is not the one
    /// `choose_block` described, the open refuses it. In the CLI, the gap between
    /// listing and opening is milliseconds. In the window, it lasts as long as a
    /// person looks at the screen. `sdb` can be reassigned in either gap, so the
    /// check lives in one place for both.
    async fn open(&self) -> Result<FlashAgent<UsbTransport>> {
        match self {
            Chosen::Usb(device) => open_loader(device).await,
            // Not async, and not awaited: opening a file is a syscall that
            // returns. The seam is async because a USB device's is.
            Chosen::Block(device) => Ok(FlashAgent::Block(blockcli::open(device)?)),
        }
    }

    /// The USB board this names, for the commands that can only mean one.
    ///
    /// These commands speak a vendor protocol to a board:
    ///
    /// - `chipver`
    /// - `storage`
    /// - `reset`
    /// - `db`
    /// - `usbboot`
    ///
    /// A block device runs no such protocol, so it has nothing to ask and nothing
    /// to answer. Naming one for these commands is a usage error. It is refused
    /// here in one place, rather than producing an empty answer in five.
    fn usb(&self, verb: &str) -> std::result::Result<&DeviceInfo, CliError> {
        match self {
            Chosen::Usb(device) => Ok(device),
            Chosen::Block(device) => Err(CliError::Usage(format!(
                "`{verb}` speaks a vendor protocol to a board, and `{}` is a block device the \
                 operating system owns. There is no loader on it to ask.",
                device.node
            ))),
        }
    }
}

/// The block layer, where there is one.
///
/// The Block backend runs on Linux only, and this module is the one place the CLI
/// says so. Every other platform builds the same command surface, and refuses at
/// the open with a sentence. A flag hidden behind a `cfg` would leave a person
/// wondering whether they typed it wrong.
mod blockcli {
    use pyrographer_core::Result;
    use pyrographer_core::block::BlockDevice;

    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    pub fn list() -> Result<Vec<BlockDevice>> {
        pyrographer_core::block::list()
    }

    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    pub fn open(device: &BlockDevice) -> Result<pyrographer_core::block::BlockAgent> {
        pyrographer_core::block::open(device)
    }

    /// The refusal both halves give where there is no backend.
    ///
    /// An empty list would say *this machine has no disks*, which is false. It
    /// would send a person looking for the disk they can see, instead of telling
    /// them the backend is not built on this platform.
    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    fn unbuilt<T>() -> Result<T> {
        Err(pyrographer_core::Error::NotImplemented(
            "the Block backend is built for Linux only. On this platform, it is not measured \
             whether an exclusive open is enforced, or whether a read-back comes from the device \
             or a cache. pyrographer does not offer a write without those guarantees",
        ))
    }

    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    pub fn list() -> Result<Vec<BlockDevice>> {
        unbuilt()
    }

    #[cfg(not(all(target_os = "linux", not(target_arch = "wasm32"))))]
    pub fn open(_device: &BlockDevice) -> Result<pyrographer_core::block::BlockAgent> {
        unbuilt()
    }
}

/// The device to act on: the one named, or the only board there is.
///
/// With nothing named and one board connected, that board is what was meant.
/// With nothing named and several connected, the CLI cannot know which was
/// intended. That is a usage error for the person at the keyboard to resolve, so
/// the error lists what is connected, in the form `--device` takes.
///
/// **A block device is never chosen by default, whatever the count.** Every
/// machine has disks. On a laptop with one, "the only device there is" is the
/// disk the system is running from. Only a block device a person names is acted
/// on.
fn choose_target(selector: Option<&DeviceSelector>) -> std::result::Result<Chosen, CliError> {
    if let Some(DeviceSelector::Block(node)) = selector {
        return choose_block(node).map(Chosen::Block);
    }
    choose_device(selector).map(Chosen::Usb)
}

/// The block device with this node, out of the ones the kernel lists.
///
/// The node is matched exactly. A near miss is a usage error that lists the
/// devices there are, in the form `--device` takes. That list matters more on
/// this backend than elsewhere. The failure it prevents is a person typing the
/// name of a disk that is not the one in their hand.
fn choose_block(node: &str) -> std::result::Result<BlockDevice, CliError> {
    let devices = blockcli::list()?;
    if let Some(found) = devices.iter().find(|device| device.node == node) {
        return Ok(found.clone());
    }
    if devices.is_empty() {
        return Err(CliError::Usage(format!(
            "no block devices are listed, so `{node}` is not one of them"
        )));
    }
    Err(CliError::Usage(format!(
        "`{node}` is not a block device on this machine. These are:\n{}",
        block_lines(&devices)
    )))
}

/// The block devices, one to a line, named the way `--device` names them.
fn block_lines(devices: &[BlockDevice]) -> String {
    devices
        .iter()
        .map(|device| {
            // The note says what is refused, not that the device is. A
            // write-protected card takes no write and reads perfectly, and
            // "[refused]" beside it would send somebody looking for a fault in
            // the one thing that is working.
            let refusal = match block::write_refusal(device) {
                Some(_) if device.carries_running_system => "  [refused: running system]",
                Some(_) if device.read_only => "  [read-only: no write]",
                Some(_) => "  [no write]",
                None if device.is_mounted() => "  [mounted]",
                None => "",
            };
            // Trimmed, because the bus column is padded for the note that
            // follows it and a device with no note would otherwise carry the
            // padding to the end of the line.
            format!(
                "  {:<14} {:>10}  {:<8}{}",
                device.node,
                human_bytes(device.bytes),
                device.bus.name(),
                refusal
            )
            .trim_end()
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The USB board to act on: the one named, or the only one there is.
fn choose_device(selector: Option<&DeviceSelector>) -> std::result::Result<DeviceInfo, CliError> {
    let mut devices = verbs::list()?;

    // Nothing connected is a device problem, not a usage one, whether or not a
    // device was named -- and it is the one that carries the hint about the udev
    // rule, which is the likeliest reason a board that *is* plugged in does not
    // appear here.
    if devices.is_empty() {
        return Err(Error::DeviceNotFound.into());
    }

    let Some(selector) = selector else {
        return match devices.len() {
            1 => Ok(devices.remove(0)),
            n => Err(CliError::Usage(format!(
                "{n} devices are connected and none was named. Pass --device to say which:\n{}",
                connected(&devices)
            ))),
        };
    };

    devices
        .iter()
        .position(|device| selector.names(device))
        .map(|at| devices.remove(at))
        .ok_or_else(|| {
            CliError::Usage(format!(
                "no connected device is {selector}. These are connected:\n{}",
                connected(&devices)
            ))
        })
}

/// The connected devices, one to a line, named the way `--device` names them.
///
/// It is only called with at least one device. No device at all is
/// [`Error::DeviceNotFound`], which carries a hint this list does not.
fn connected(devices: &[DeviceInfo]) -> String {
    devices
        .iter()
        .map(|device| {
            format!(
                "  {}:{}  {:04x}:{:04x}  {}",
                device.bus_id,
                device.device_address,
                device.vendor_id,
                device.product_id,
                device.mode.name()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait for a board to reappear after a `db` upload.
///
/// A maskrom board re-enumerates on the 0x0472 jump as a *different* USB device.
/// It cannot be matched by the address it had, because that address is gone. It
/// cannot be matched by a loader-mode flag either. The RK3576 SPL loader keeps
/// bcdUSB even, so the device that comes back can be flagged maskrom while
/// answering as a loader.
///
/// The strongest signal is a device of the same vendor on the same bus, at an
/// address **not on the bus before the upload**. [`is_returned_board`] decides it.
/// That is a new device, not a second board that was connected all along.
/// `present_before` is the snapshot the caller took before uploading. On a
/// two-board bench (the clone setup), it stops the CLI reporting the *other*
/// board's coordinates, which a person would then type into `--device`.
///
/// This polls for the device and gives up quietly, because a miss is a thing to
/// report, not an error to raise. The window is generous. On a real RK3576, more
/// than three seconds passed between the upload returning and the loader
/// enumerating. A watcher that gives up early reports a working upload as
/// uncertain.
fn await_loader(before: &DeviceInfo, present_before: &[DeviceInfo]) -> Option<DeviceInfo> {
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(devices) = verbs::list()
            && let Some(found) = devices
                .into_iter()
                .find(|d| is_returned_board(d, before, present_before))
        {
            return Some(found);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    None
}

/// Whether `found` can be the board that left the bus as `before`.
///
/// It must be the same vendor, on the same bus, at an address that was not on the
/// bus in `present_before`. The listing carries every vendor pyrographer scans
/// for. Without the vendor check, an Ingenic board plugged in during the wait would
/// be reported as the Rockchip loader that came back.
fn is_returned_board(
    found: &DeviceInfo,
    before: &DeviceInfo,
    present_before: &[DeviceInfo],
) -> bool {
    found.vendor == before.vendor
        && found.bus_id == before.bus_id
        && found.device_address != before.device_address
        && !present_before
            .iter()
            .any(|p| p.bus_id == found.bus_id && p.device_address == found.device_address)
}

/// Open a device's flash agent, refusing only what cannot serve one.
///
/// A Rockchip device goes to [`open_rockchip_loader`], and an Ingenic device to
/// [`open_ingenic_dfu`]. For a Rockchip device, the mode in the descriptors is a
/// claim, not a finding. The bcdUSB flag is odd
/// on a loader that sets it. The RK3576 SPL loader does not set it, and runs
/// rockusb behind the even flag of the maskrom it replaced.
///
/// The open itself does not settle an even flag either. The RK3576 BootROM
/// presents the same vendor interface and bulk pair a loader does, with nothing
/// serving the endpoints. Claiming the interface therefore succeeds on both. One
/// probe command settles it: a loader answers `TEST_UNIT_READY`, and a BootROM's
/// dead endpoints fault. A board that fails the probe is refused as maskrom, and
/// the hint names the upload that changes that.
///
/// Mass storage is the one Rockchip mode the descriptors name reliably, because
/// the class is in the interface descriptor. It is refused without an open,
/// because that board's flash belongs to the operating system's block layer.
async fn open_loader(device: &DeviceInfo) -> Result<FlashAgent<UsbTransport>> {
    match device.vendor {
        discovery::Vendor::Rockchip => open_rockchip_loader(device).await,
        discovery::Vendor::Ingenic => open_ingenic_dfu(device).await,
    }
}

/// Open a Rockchip device and probe it for a loader.
///
/// It is the Rockchip half of [`open_loader`]. The bcdUSB mode flag is a claim,
/// so a maskrom-flagged board is opened and sent a `TEST_UNIT_READY` before it is
/// trusted. Only mass storage, which the interface class names reliably, is
/// refused without an open.
async fn open_rockchip_loader(device: &DeviceInfo) -> Result<FlashAgent<UsbTransport>> {
    if device.mode == Mode::MassStorage {
        return Err(Error::WrongMode {
            found: device.mode.name(),
            needed: "loader",
        });
    }
    let transport = UsbTransport::open(device).await?;
    let mut agent = RockusbAgent::new(transport);
    if device.mode == Mode::Maskrom && agent.test_unit_ready().await.is_err() {
        return Err(Error::WrongMode {
            found: "maskrom",
            needed: "loader",
        });
    }
    Ok(FlashAgent::Rockusb(agent))
}

/// Open an Ingenic board's DFU gadget for the flash path.
///
/// It is the Ingenic half of [`open_loader`]. Only a board already in DFU mode
/// has reachable flash. A boot-ROM board is brought to DFU first with `usbboot`,
/// which uploads a DFU-capable U-Boot. A mass-storage gadget belongs to the
/// operating system's block layer. A DFU board is opened with
/// [`UsbTransport::open_dfu`], which reads its alt-settings from its descriptors.
/// The alt-settings are its partitions, and the agent returns them as its table.
async fn open_ingenic_dfu(device: &DeviceInfo) -> Result<FlashAgent<UsbTransport>> {
    match device.mode {
        Mode::Dfu => {
            let opened = UsbTransport::open_dfu(device).await?;
            Ok(FlashAgent::Dfu(DfuAgent::new(
                opened.transport,
                opened.interface,
                opened.functional,
                opened.alts,
            )))
        }
        Mode::BootRom => Err(Error::InvalidRequest(
            "this Ingenic board is in its USB boot ROM, which has no reachable flash. Bring it up \
             to DFU first with `usbboot`, which uploads a DFU-capable U-Boot. Then dump or verify \
             its partitions"
                .to_string(),
        )),
        // MassStorage, or the Rockchip-only maskrom/loader modes an Ingenic board
        // never classifies as: none of them is a DFU gadget.
        other => Err(Error::WrongMode {
            found: other.name(),
            needed: "DFU",
        }),
    }
}

/// The refusal a raw-LBA `dump` or `verify` gets on a board that reaches its
/// flash only by named region.
///
/// A DFU board is one: it is addressed by alt-setting rather than by a
/// device-wide LBA. The refusal points at the form that works.
fn raw_lba_unsupported(verb: &str) -> Error {
    Error::InvalidRequest(format!(
        "this board cannot serve a {verb} by raw LBA. It addresses named regions (its DFU \
         alt-settings), not a device-wide LBA. Aim the {verb} with --partition <name> instead"
    ))
}

/// The refusal a `clone` gets when either board reaches its flash only by named
/// region.
///
/// A clone copies a whole device by raw LBA. A board without raw LBA addressing
/// has no whole-device image to copy or to receive.
fn clone_raw_lba_unsupported() -> Error {
    Error::InvalidRequest(
        "clone copies a whole device by raw LBA. One of these boards addresses its flash by named \
         region (a DFU alt-setting) instead, so it has no whole-device image to copy. Use a \
         partition-scoped dump and flash instead"
            .to_string(),
    )
}

/// The width the in-place progress line is padded to, past the longest line it
/// prints (`  100.0%  116.48 GiB of 116.48 GiB  26.00 MiB/s` is ~50 columns).
///
/// A shorter frame, rewound with `\r`, then overwrites the tail of the previous
/// one with spaces.
const PROGRESS_WIDTH: usize = 64;

/// Renders [`Progress`] events to the terminal.
///
/// The clock lives in the CLI, not in core. Core reports bytes moved, and only
/// the caller knows what a second looks like or whether anyone is watching. Core
/// does not know which verb is running either, so the words come from the
/// caller. `doing` is for the line that opens, and `did` for the one that closes.
struct ProgressRenderer {
    doing: &'static str,
    did: &'static str,
    started: Instant,
    interactive: bool,
}

impl ProgressRenderer {
    fn new(doing: &'static str, did: &'static str) -> Self {
        Self {
            doing,
            did,
            started: Instant::now(),
            interactive: std::io::stderr().is_terminal(),
        }
    }

    fn render(&mut self, event: Progress) {
        match event {
            Progress::Started { total_bytes } => {
                self.started = Instant::now();
                eprintln!("{} {}...", self.doing, human_bytes(total_bytes));
            }
            Progress::Advanced {
                done_bytes,
                total_bytes,
            } => {
                if !self.interactive {
                    return;
                }
                let elapsed = self.started.elapsed().as_secs_f64();
                let rate = if elapsed > 0.0 {
                    done_bytes as f64 / elapsed
                } else {
                    0.0
                };
                let percent = if total_bytes > 0 {
                    done_bytes as f64 / total_bytes as f64 * 100.0
                } else {
                    100.0
                };
                // Padded to a stable width and rewound with `\r`. A bare `\r`
                // returns the cursor without clearing the line, so a frame
                // shorter than the one before it leaves the old frame's tail
                // showing (a fast rate settling from "12.3 MiB/s" to "2.27
                // MiB/s" would strand "iB/s"). Padding past the longest line
                // this prints overwrites that residue with spaces.
                let line = format!(
                    "  {percent:5.1}%  {} of {}  {}/s",
                    human_bytes(done_bytes),
                    human_bytes(total_bytes),
                    human_bytes(rate as u64),
                );
                eprint!("\r{line:<PROGRESS_WIDTH$}");
                let _ = std::io::stderr().flush();
            }
            Progress::Finished { done_bytes } => {
                let elapsed = self.started.elapsed();
                if self.interactive {
                    eprintln!();
                }
                eprintln!(
                    "{} {} in {:.1}s.",
                    self.did,
                    human_bytes(done_bytes),
                    elapsed.as_secs_f64()
                );
            }
        }
    }
}

/// Format a byte count the way a person reads one.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Report a read that came back as constant fill.
///
/// Constant fill reported as a successful read is what a silent read failure
/// looks like. The device answered with a buffer it never filled, so the image
/// *looks* complete. A run of any byte but `0x00` or `0xff` gets a warning. A
/// blank run gets a note only, because erased or unallocated flash ordinarily
/// reads that way. See [`pyrographer_core::fill`].
///
/// The lines go to stderr, and the "Wrote" or "matches" line goes to stdout. A
/// redirected image is unaffected, and a person still sees the lines.
fn report_fill(fill: &FillReport) {
    for line in fill_messages(fill) {
        eprintln!("{line}");
    }
}

/// The lines [`report_fill`] prints, built as data so a test can pin the exact
/// words. The words are the part that protects a person.
fn fill_messages(fill: &FillReport) -> Vec<String> {
    let mut lines = Vec::new();

    for run in fill.suspicious() {
        lines.push(format!(
            "Warning: {} from sector {} ({} in) read back as constant {:#04x}.",
            human_bytes(run.bytes()),
            run.first_lba(),
            human_bytes(run.first_byte()),
            run.byte(),
        ));
        lines.push(
            "         Constant fill reported as a successful read can be a silent read \
             failure rather than data. If this region holds data, read it again another \
             way, such as through mass storage or the board's console, before you trust \
             the image."
                .to_string(),
        );
    }

    let blank: Vec<_> = fill.blank().collect();
    if !blank.is_empty() {
        let total: u64 = blank.iter().map(|run| run.bytes()).sum();
        lines.push(format!(
            "Note: {} read back as constant 0x00 or 0xff, across {} region{}, consistent \
             with erased or unallocated flash.",
            human_bytes(total),
            blank.len(),
            if blank.len() == 1 { "" } else { "s" },
        ));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyrographer_core::agent::FlashInfo;
    use pyrographer_core::agent::ReadBack;
    use pyrographer_core::codec::splhdr::Origin;
    use pyrographer_core::partition::{Overlap, Partition, TableFormat, TableRecovery};
    use pyrographer_core::recovery::{PlannedStage, StageKind};
    use pyrographer_core::verbs::Segment;
    use std::ffi::OsString;

    /// A command line, as pico-args sees one after the subcommand is taken.
    fn arguments(free: &[&str]) -> pico_args::Arguments {
        pico_args::Arguments::from_vec(free.iter().map(OsString::from).collect())
    }

    #[test]
    fn byte_counts_read_the_way_a_person_reads_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(122_142_720 * 512), "58.24 GiB");
    }

    /// The words a fill finding is rendered into are the part that protects a
    /// person, so this test pins them. A poison run warns, and names its byte and
    /// where it starts. A blank run is only noted. The report is built through the
    /// real scanner, not by hand.
    #[test]
    fn a_fill_report_warns_on_poison_and_only_notes_blank() {
        use pyrographer_core::fill::FillScanner;

        // Real 512-byte sectors: a 1 MiB window is 2048 sectors, exactly the
        // report threshold, and sector 65536 is 32 MiB into the flash.
        let mut scanner = FillScanner::new(512);
        let poison = vec![0xcc; 1 << 20];
        scanner.observe(65536, &poison); // suspicious, at sector 65536 (32 MiB in)
        let blank = vec![0x00; 1 << 20];
        scanner.observe(70000, &blank); // blank, and non-contiguous so its own run
        let messages = fill_messages(&scanner.finish());
        let text = messages.join("\n");

        assert!(text.contains("Warning:"), "{text}");
        assert!(
            text.contains("sector 65536"),
            "the poison run is placed: {text}"
        );
        assert!(text.contains("(32.00 MiB in)"), "and where that is: {text}");
        assert!(text.contains("0xcc"), "and named: {text}");
        assert!(
            text.contains("silent read failure"),
            "the caution is spoken: {text}"
        );
        assert!(
            text.contains("Note:") && text.contains("0x00 or 0xff"),
            "the blank run is noted, not warned about: {text}"
        );
        assert!(
            !text.contains("Warning: 1.00 MiB from sector 70000"),
            "the blank run does not get a warning: {text}"
        );
    }

    /// The chip-version reply is rendered and not decoded, so the rendering is
    /// all a person has to go on. It must be faithful: every byte shown, in
    /// order, with nothing collapsed and nothing dropped.
    #[test]
    fn a_chip_version_is_shown_byte_for_byte_in_both_columns() {
        let reply = [0x38, 0x38, 0x35, 0x33, 0x00, 0xff];
        assert_eq!(hex(&reply), "38 38 35 33 00 ff");
        assert_eq!(ascii(&reply), "8853..");
    }

    /// A byte that is not printable is a dot, not a gap. The two columns stay in
    /// step, so a reply that is all zeros still shows its length rather than
    /// looking like an empty answer.
    #[test]
    fn an_unprintable_byte_is_a_dot_and_not_a_gap() {
        assert_eq!(ascii(&[0x00, 0x7f, 0x80, 0x0a]), "....");
        assert_eq!(ascii(&[]), "");
        assert_eq!(hex(&[]), "");
        // A space is printable, and a chip identifier padded with them would be
        // lost if it were not.
        assert_eq!(ascii(b"RK3588 "), "RK3588 ");
    }

    #[test]
    fn dump_reads_its_three_positional_arguments() {
        let args = parse_dump(&mut arguments(&["0", "2048", "boot.img"])).expect("a valid dump");
        assert_eq!(
            args,
            DumpArgs {
                target: DumpTarget::Range {
                    lba: 0,
                    sectors: 2048
                },
                path: "boot.img".to_string(),
            }
        );
    }

    /// Naming a partition takes the place of the LBA and the sector count. The
    /// device's table records both, and a count a person restates is a count they
    /// can get wrong. The only positional argument left is the file.
    #[test]
    fn a_dump_of_a_named_partition_asks_for_neither_an_lba_nor_a_count() {
        let args = parse_dump(&mut arguments(&["--partition", "boot", "boot.img"]))
            .expect("a valid dump of a partition");
        assert_eq!(
            args,
            DumpArgs {
                target: DumpTarget::Partition("boot".to_string()),
                path: "boot.img".to_string(),
            }
        );
    }

    /// A sector count that is not a number is a command-line error. The CLI
    /// reports it as its own, rather than as a core error. As a core error, it
    /// would be printed as an I/O failure, with whatever hint core attaches to
    /// one. That would be advice about cables, for a typo.
    #[test]
    fn a_dump_argument_that_does_not_parse_is_a_usage_problem_not_a_device_one() {
        let error = parse_dump(&mut arguments(&["0", "lots", "boot.img"]))
            .expect_err("'lots' is not a sector count");

        let CliError::Usage(message) = error else {
            panic!("a bad argument is a usage problem: {error:?}");
        };
        assert!(message.contains("sector count"), "{message}");
    }

    #[test]
    fn a_dump_missing_its_file_is_a_usage_problem() {
        let error =
            parse_dump(&mut arguments(&["0", "2048"])).expect_err("there is nowhere to write");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    #[test]
    fn db_takes_a_container() {
        let source = parse_db(&mut arguments(&["--loader", "loader.bin"])).expect("a container");
        assert_eq!(source, DbSource::Container("loader.bin".to_string()));
    }

    /// The raw form takes either stage alone, or both. The protocol sends the two
    /// files binman emits as separate sections, and needs at least one.
    #[test]
    fn db_takes_raw_stages_together_or_alone() {
        let both = parse_db(&mut arguments(&[
            "--code471",
            "usb471.bin",
            "--code472",
            "usb472.bin",
        ]))
        .expect("both stages");
        assert_eq!(
            both,
            DbSource::Raw {
                code_471: Some("usb471.bin".to_string()),
                code_472: Some("usb472.bin".to_string()),
            }
        );

        let one = parse_db(&mut arguments(&["--code472", "usb472.bin"])).expect("one stage");
        assert_eq!(
            one,
            DbSource::Raw {
                code_471: None,
                code_472: Some("usb472.bin".to_string()),
            }
        );
    }

    /// A container names its own sections, so raw files beside it can only
    /// contradict it. The combination is refused rather than resolved.
    #[test]
    fn db_refuses_a_container_and_raw_stages_together() {
        let error = parse_db(&mut arguments(&[
            "--loader",
            "loader.bin",
            "--code471",
            "usb471.bin",
        ]))
        .expect_err("the forms are exclusive");
        let CliError::Usage(message) = error else {
            panic!("exclusivity is a usage problem: {error:?}");
        };
        assert!(message.contains("not both"), "{message}");
    }

    /// `--gadget` names either gadget by the command that starts it, and the
    /// block device defaults to the eMMC.
    #[test]
    fn uboot_starts_either_gadget() {
        for (name, expected) in [("rockusb", Gadget::Rockusb), ("ums", Gadget::Ums)] {
            let parsed = parse_uboot(&mut arguments(&[
                "--port",
                "/dev/ttyUSB0",
                "--gadget",
                name,
            ]))
            .expect("a gadget");
            let UbootAction::Gadget(gadget, device) = parsed.action else {
                panic!("--gadget {name} is the gadget form");
            };
            assert_eq!(gadget, expected);
            assert_eq!(device, GadgetDevice::default());
        }
    }

    #[test]
    fn uboot_refuses_a_gadget_it_does_not_start() {
        let result = parse_uboot(&mut arguments(&[
            "--port",
            "/dev/ttyUSB0",
            "--gadget",
            "dfu",
        ]));
        assert!(result.is_err(), "dfu is not a gadget uboot starts");
    }

    #[test]
    fn db_with_nothing_to_upload_is_a_usage_problem() {
        let error = parse_db(&mut arguments(&[])).expect_err("nothing to upload");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    #[test]
    fn usbboot_reads_two_stages_and_their_addresses() {
        let args = parse_usbboot(&mut arguments(&[
            "--stage1",
            "spl.bin",
            "--stage1-addr",
            "0x80000000",
            "--stage2",
            "uboot.bin",
            "--stage2-addr",
            "0x80100000",
        ]))
        .expect("a full two-stage bootstrap");
        assert_eq!(
            args,
            UsbBootArgs {
                stage1: "spl.bin".to_string(),
                stage1_addr: 0x8000_0000,
                stage2: Some("uboot.bin".to_string()),
                stage2_addr: Some(0x8010_0000),
                settle_ms: DEFAULT_SETTLE_MS,
            }
        );
    }

    /// A stage1-only bootstrap is valid. It initializes DRAM and stops, and takes
    /// the default DRAM settle.
    #[test]
    fn usbboot_takes_a_stage1_alone() {
        let args = parse_usbboot(&mut arguments(&[
            "--stage1",
            "spl.bin",
            "--stage1-addr",
            "0x80000000",
        ]))
        .expect("stage1 alone");
        assert_eq!(args.stage2, None);
        assert_eq!(args.stage2_addr, None);
        assert_eq!(args.settle_ms, DEFAULT_SETTLE_MS);
    }

    /// A stage2 with no address, or an address with no stage2, is refused as a
    /// command line with no meaning.
    #[test]
    fn usbboot_pairs_stage2_with_its_address() {
        let no_addr = parse_usbboot(&mut arguments(&[
            "--stage1",
            "spl.bin",
            "--stage1-addr",
            "0x80000000",
            "--stage2",
            "uboot.bin",
        ]))
        .expect_err("a stage2 with nowhere to load");
        assert!(matches!(no_addr, CliError::Usage(_)), "{no_addr:?}");

        let no_stage = parse_usbboot(&mut arguments(&[
            "--stage1",
            "spl.bin",
            "--stage1-addr",
            "0x80000000",
            "--stage2-addr",
            "0x80100000",
        ]))
        .expect_err("an address with no stage2");
        assert!(matches!(no_stage, CliError::Usage(_)), "{no_stage:?}");
    }

    #[test]
    fn usbboot_needs_a_stage1() {
        let error = parse_usbboot(&mut arguments(&[])).expect_err("no stage1");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    /// The DRAM settle can be overridden, for a board whose memory comes up at a
    /// pace other than the community default.
    #[test]
    fn usbboot_settle_can_be_overridden() {
        let args = parse_usbboot(&mut arguments(&[
            "--stage1",
            "spl.bin",
            "--stage1-addr",
            "0x0",
            "--dram-settle-ms",
            "500",
        ]))
        .expect("a valid override");
        assert_eq!(args.settle_ms, 500);
    }

    /// Addresses parse with or without the `0x` prefix. A value too big for 32
    /// bits is refused rather than truncated, because a wrong load address can
    /// brick a bootstrap.
    #[test]
    fn a_hex_address_parses_prefixed_or_bare_and_refuses_overflow() {
        assert_eq!(parse_u32_hex("0x80000000"), Ok(0x8000_0000));
        assert_eq!(parse_u32_hex("80000000"), Ok(0x8000_0000));
        assert_eq!(parse_u32_hex("0X1F"), Ok(0x1f));
        assert!(parse_u32_hex("100000000").is_err(), "past 32 bits");
        assert!(parse_u32_hex("nope").is_err());
    }

    /// `verify` takes no sector count. The file is the image, so its length is
    /// how far the comparison runs, and there is nothing for the caller to get
    /// wrong.
    #[test]
    fn verify_reads_an_lba_and_a_file_and_asks_for_no_length() {
        let args = parse_verify(&mut arguments(&["64", "boot.img"])).expect("a valid verify");
        assert_eq!(
            args,
            VerifyArgs {
                target: Target::Lba(64),
                path: "boot.img".to_string(),
            }
        );

        let args = parse_verify(&mut arguments(&["--partition", "boot", "boot.img"]))
            .expect("a valid verify of a partition");
        assert_eq!(args.target, Target::Partition("boot".to_string()));
    }

    #[test]
    fn a_verify_missing_its_file_is_a_usage_problem() {
        let error =
            parse_verify(&mut arguments(&["64"])).expect_err("there is nothing to compare against");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    #[test]
    fn flash_reads_an_lba_and_a_file_and_defaults_to_asking() {
        let args = parse_flash(&mut arguments(&["64", "boot.img"])).expect("a valid flash");
        assert_eq!(
            args,
            FlashArgs {
                target: Target::Lba(64),
                path: "boot.img".to_string(),
                device: None,
                yes: false,
                dry_run: false,
                soc: None,
            }
        );
    }

    /// `--soc` arms the wrong-loader gate. A name nobody has pinned is refused at
    /// the parse, before a device is opened, and the refusal lists the pinned
    /// names.
    #[test]
    fn flash_reads_the_soc_and_refuses_an_unpinned_one_up_front() {
        let args = parse_flash(&mut arguments(&["--soc", "rk3576", "64", "boot.img"]))
            .expect("a pinned SoC parses");
        assert_eq!(args.soc.map(|soc| soc.name()), Some("rk3576"));

        let error = parse_flash(&mut arguments(&["--soc", "rk3588", "64", "boot.img"]))
            .expect_err("no board has pinned rk3588");
        let message = format!("{error}");
        assert!(message.contains("rk3576"), "{message}");
        assert!(
            message.contains("pyrographer chipver"),
            "it names the command that reads a reply to pin: {message}"
        );
    }

    /// A write can be aimed by name, and then the command line carries no LBA.
    /// An LBA is a number a person worked out, and a name is one the device gave.
    #[test]
    fn a_flash_of_a_named_partition_takes_no_lba() {
        let args = parse_flash(&mut arguments(&["--partition", "uboot", "u-boot.img"]))
            .expect("a valid flash into a partition");

        assert_eq!(args.target, Target::Partition("uboot".to_string()));
        assert_eq!(args.path, "u-boot.img");
        assert!(!args.yes, "and it still asks");
    }

    /// When a partition is named, the flags still come off before the
    /// positionals, so `--yes` cannot be mistaken for the file.
    #[test]
    fn a_flag_beside_a_named_partition_is_not_mistaken_for_the_file() {
        let args = parse_flash(&mut arguments(&[
            "--yes",
            "--partition",
            "boot",
            "boot.img",
        ]))
        .expect("the flag is a flag");

        assert_eq!(args.target, Target::Partition("boot".to_string()));
        assert_eq!(args.path, "boot.img");
        assert!(args.yes);
    }

    /// A device is named the way `list` prints it, and nothing else parses. A
    /// selector that silently accepted something it did not understand would
    /// point a destructive command at whatever it happened to match.
    #[test]
    fn a_device_is_named_by_its_bus_and_address() {
        let selector: DeviceSelector = "003:12".parse().expect("a bus and an address");
        assert_eq!(
            selector,
            DeviceSelector::Usb {
                bus_id: "003".to_string(),
                device_address: 12
            }
        );
        assert_eq!(selector.to_string(), "003:12");

        assert!("003".parse::<DeviceSelector>().is_err(), "no address");
        assert!("003:xii".parse::<DeviceSelector>().is_err(), "not a number");
        assert!("003:999".parse::<DeviceSelector>().is_err(), "not a u8");
    }

    /// A selector names one device and not another, and the bus and the address
    /// together decide which. Two boards on one bus differ only in the address,
    /// and that is the case the selector exists for.
    #[test]
    fn a_selector_names_exactly_one_of_two_boards_on_the_same_bus() {
        let selector: DeviceSelector = "003:14".parse().unwrap();

        let mut other = a_device(); // 003:12
        assert!(!selector.names(&other));

        other.device_address = 14;
        assert!(selector.names(&other));

        other.bus_id = "004".to_string();
        assert!(!selector.names(&other), "same address, different bus");
    }

    /// Both boards must be named. With exactly two connected, a guess is
    /// possible, but the two guesses differ in which board is overwritten.
    /// `clone` therefore does not guess.
    #[test]
    fn clone_names_both_boards_and_refuses_to_infer_either() {
        let args = parse_clone(&mut arguments(&["--from", "003:12", "--to", "003:14"]))
            .expect("a valid clone");
        assert_eq!(
            args,
            CloneArgs {
                from: "003:12".parse().unwrap(),
                to: "003:14".parse().unwrap(),
                yes: false,
                dry_run: false,
                soc: None,
            }
        );

        let error = parse_clone(&mut arguments(&["--from", "003:12"]))
            .expect_err("there is no destination");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");

        let error =
            parse_clone(&mut arguments(&["--to", "003:14"])).expect_err("there is no source");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    /// A board cloned onto itself would read a window, write it back over itself,
    /// and report success. That operation looks like it worked and means nothing.
    /// It is a mistake, so it is refused.
    #[test]
    fn a_board_cannot_be_cloned_onto_itself() {
        let error = parse_clone(&mut arguments(&["--from", "003:12", "--to", "003:12"]))
            .expect_err("the source and the destination are the same board");

        let CliError::Usage(message) = error else {
            panic!("naming one board twice is a usage problem: {error:?}");
        };
        assert!(
            message.contains("cannot be cloned onto itself"),
            "{message}"
        );
    }

    /// The clone plan names both boards and says which one is about to be
    /// overwritten. A swapped source and destination is the costliest mistake, so
    /// the rendering must make clear which is which.
    #[test]
    fn the_clone_plan_says_which_board_gets_overwritten() {
        let source = a_device(); // 2207:350e on 003:12

        let mut destination = a_device();
        destination.product_id = 0x350b;
        destination.device_address = 14;

        let plan = ClonePlan {
            source: FlashInfo {
                size_bytes: 8 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            destination: WritePlan {
                lba: 0,
                image_bytes: 8 * 512,
                padding_bytes: 0,
                sectors: 8,
                flash: FlashInfo {
                    size_bytes: 122_142_720 * 512,
                    sector_size: 512,
                    medium: None,
                    chip_id: None,
                },
                chip_version: vec![0x38, 0x38, 0x35, 0x33],
                soc: None,
                touches: Touches::Partitions(vec![Overlap {
                    name: "rootfs".to_string(),
                    first_lba: 0,
                    covered: 8,
                    total: 8,
                }]),
                read_back: ReadBack::PerWindow,
            },
        };

        let rendered = render_clone_plan(&Chosen::Usb(source), &Chosen::Usb(destination), &plan);

        assert!(
            rendered.contains("This will overwrite the whole of 2207:350b on bus 003 address 14"),
            "which board dies: {rendered}"
        );
        assert!(
            rendered.contains("source       2207:350e on bus 003 address 12"),
            "which board is copied: {rendered}"
        );
        assert!(
            rendered.contains("destination  2207:350b on bus 003 address 14"),
            "and which is written: {rendered}"
        );
        assert!(
            rendered.contains("The source is only read"),
            "what is safe: {rendered}"
        );
        assert!(
            rendered.contains(ReadBack::PerWindow.describe()),
            "when the destination is checked, in the backend's words: {rendered}"
        );
        assert!(
            rendered.contains("Nothing here can be undone"),
            "and what is not: {rendered}"
        );
    }

    /// The board that comes back after a `db` upload is matched by vendor as well
    /// as by place.
    ///
    /// The listing carries every vendor pyrographer scans for. An Ingenic board
    /// plugged in on the same bus during the wait is a new device at a new address.
    /// It is not the Rockchip loader that came back, and reporting its coordinates
    /// would send the next command to the wrong board.
    #[test]
    fn the_board_that_comes_back_is_the_same_vendor_at_a_new_address() {
        let before = DeviceInfo {
            mode: Mode::Maskrom,
            bcd_usb: 0x0200,
            ..a_device()
        };
        let other_board = DeviceInfo {
            device_address: 13,
            ..a_device()
        };
        let present_before = [before.clone(), other_board.clone()];

        let loader = DeviceInfo {
            device_address: 15,
            ..a_device()
        };
        assert!(
            is_returned_board(&loader, &before, &present_before),
            "a new Rockchip device on the same bus"
        );

        let ingenic = DeviceInfo {
            vendor: pyrographer_core::discovery::Vendor::Ingenic,
            vendor_id: 0xa108,
            product_id: 0x4d44,
            device_address: 16,
            ..a_device()
        };
        assert!(
            !is_returned_board(&ingenic, &before, &present_before),
            "a new device of another vendor is not the board that left"
        );
        assert!(
            !is_returned_board(&other_board, &before, &present_before),
            "nor is a board that was on the bus all along"
        );
        assert!(
            !is_returned_board(
                &DeviceInfo {
                    bus_id: "004".to_string(),
                    ..loader.clone()
                },
                &before,
                &present_before
            ),
            "nor a new board on another bus"
        );
    }

    /// The flags are taken out of the command line before the positional
    /// arguments are read. A `--yes` still in the vector would be read as the
    /// LBA, and `flash --yes 64 boot.img` would be refused as an unparsable sector.
    /// This test pins the order the arguments are read in.
    #[test]
    fn a_flag_before_the_positionals_is_not_mistaken_for_the_lba() {
        let args = parse_flash(&mut arguments(&["--yes", "64", "boot.img"]))
            .expect("the flag is a flag, not the LBA");
        assert_eq!(args.target, Target::Lba(64));
        assert_eq!(args.path, "boot.img");
        assert!(args.yes);
        assert!(!args.dry_run);

        let args = parse_flash(&mut arguments(&["--dry-run", "0", "u-boot.img"]))
            .expect("the same, wherever the flag sits");
        assert_eq!(args.target, Target::Lba(0));
        assert!(args.dry_run);
        assert!(!args.yes);
    }

    /// A block device is named by its node, and a board by its bus and address.
    /// The leading slash alone tells them apart.
    #[test]
    fn a_block_device_is_named_by_its_node() {
        let selector: DeviceSelector = "/dev/sdb".parse().expect("a device node");
        assert_eq!(selector, DeviceSelector::Block("/dev/sdb".to_string()));
        assert_eq!(selector.to_string(), "/dev/sdb");

        // It names no board, whatever the board is: the two namespaces do not
        // overlap, and a selector that matched one of each would make `clone`
        // able to confuse them.
        assert!(!selector.names(&a_device()));

        // A bare kernel name is neither spelling, and the error says both.
        let error = "sdb".parse::<DeviceSelector>().expect_err("neither form");
        assert!(error.contains("/dev/sdb"), "{error}");
        assert!(error.contains("003:12"), "{error}");
    }

    /// The heading a command prints for a block device names what decides whether
    /// it is the right one. That is its name, its size, and the bus it is on.
    #[test]
    fn the_heading_for_a_block_device_names_it_and_its_size() {
        let described = Chosen::Block(a_card()).describe();
        assert!(described.contains("/dev/sdb"), "{described}");
        assert!(described.contains("29.72 GiB"), "{described}");
        assert!(described.contains("usb"), "{described}");
        assert!(
            !described.contains("Mounted"),
            "nothing is mounted on it: {described}"
        );

        let mut mounted = a_card();
        mounted.mounts = vec!["/media/x/BOOT".to_string()];
        let described = Chosen::Block(mounted).describe();
        assert!(
            described.contains("Mounted:      /media/x/BOOT"),
            "somebody else is using it, said before anything runs: {described}"
        );
    }

    /// The commands that speak a vendor protocol refuse a block device, and name
    /// themselves while doing it.
    #[test]
    fn a_vendor_command_refuses_a_block_device() {
        let block = Chosen::Block(a_card());
        let error = block.usb("chipver").expect_err("a disk runs no loader");
        let CliError::Usage(message) = error else {
            panic!("a person named the wrong kind of device, which is a usage problem");
        };
        assert!(message.contains("chipver"), "{message}");
        assert!(message.contains("/dev/sdb"), "{message}");

        assert!(
            a_chosen_board().usb("chipver").is_ok(),
            "a board is what they are for"
        );
    }

    /// `--soc` arms the wrong-loader gate, and a block device has no loader to
    /// gate on. The flag is refused rather than ignored. The plan prints no SoC
    /// line for a block device. A dropped flag would leave a person believing in
    /// a gate that is not there.
    #[test]
    fn a_soc_named_for_a_block_write_is_refused_rather_than_ignored() {
        let block = Chosen::Block(a_card());
        let soc = Soc::parse("rk3576").expect("pinned");

        assert!(
            block.reject_soc(None).is_ok(),
            "naming none is the normal case"
        );
        let error = block
            .reject_soc(Some(soc))
            .expect_err("there is no loader on a disk");
        let CliError::Usage(message) = error else {
            panic!("naming a SoC for a disk is a usage problem");
        };
        assert!(message.contains("no loader"), "{message}");
        assert!(
            message.contains("Drop --soc"),
            "it says what to do: {message}"
        );

        assert!(
            a_chosen_board().reject_soc(Some(soc)).is_ok(),
            "a board is exactly what --soc is for"
        );
    }

    /// The write plan for a block device says what guards it, and omits what does
    /// not apply.
    ///
    /// The USB plan's last two lines are the loader's answer and the verdict on
    /// it. Rendered here, they would print "the write will be refused until a SoC
    /// is named", which is false on this backend. A plan line that is reliably
    /// wrong teaches a person to skip the plan.
    #[test]
    fn a_block_write_plan_names_the_disk_and_omits_the_loader_gate() {
        let plan = WritePlan {
            lba: 2048,
            image_bytes: 4096,
            padding_bytes: 0,
            sectors: 8,
            flash: FlashInfo {
                size_bytes: 31_914_983_424,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            // What a block device answers: nothing, and no SoC was named.
            chip_version: Vec::new(),
            soc: None,
            touches: Touches::NoTable,
            read_back: ReadBack::PerWindow,
        };

        let rendered = render_plan(&Chosen::Block(a_card()), "boot.img", &plan);
        assert!(
            rendered.contains("This will overwrite 4.00 KiB of /dev/sdb."),
            "the disk is named in the sentence that says what dies: {rendered}"
        );
        assert!(
            !rendered.contains("loader says") && !rendered.contains("named SoC"),
            "there is no loader here to say anything: {rendered}"
        );
        assert!(
            rendered.contains("held") && rendered.contains("exclusively"),
            "what guards it instead: {rendered}"
        );
        assert!(
            rendered.contains("running system"),
            "and the refusal that has no override: {rendered}"
        );
        assert!(
            rendered.contains("device      "),
            "a disk's sectors are not flash: {rendered}"
        );
    }

    /// A clone onto a block device names both ends in full, so a person can catch
    /// a swapped source and destination.
    #[test]
    fn a_clone_onto_a_disk_names_both_ends() {
        let plan = ClonePlan {
            source: FlashInfo {
                size_bytes: 8 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            destination: WritePlan {
                lba: 0,
                image_bytes: 8 * 512,
                padding_bytes: 0,
                sectors: 8,
                flash: FlashInfo {
                    size_bytes: 31_914_983_424,
                    sector_size: 512,
                    medium: None,
                    chip_id: None,
                },
                chip_version: Vec::new(),
                soc: None,
                touches: Touches::NoTable,
                read_back: ReadBack::PerWindow,
            },
        };

        let rendered = render_clone_plan(&a_chosen_board(), &Chosen::Block(a_card()), &plan);
        assert!(
            rendered.contains("This will overwrite the whole of /dev/sdb (Generic Storage Device)"),
            "which device dies: {rendered}"
        );
        assert!(
            rendered.contains("source       2207:350e on bus 003 address 12"),
            "and which is only read: {rendered}"
        );
    }

    /// The listing marks the disks that cannot be written. The line a person
    /// copies from it is the line they are about to act on.
    #[test]
    fn the_block_listing_marks_what_cannot_be_written() {
        let card = a_card();
        let mut system = a_card();
        system.name = "nvme0n1".to_string();
        system.node = "/dev/nvme0n1".to_string();
        system.carries_running_system = true;

        let lines = block_lines(&[card, system]);
        let mut rows = lines.lines();
        let first = rows.next().expect("the card");
        assert!(first.contains("/dev/sdb"), "{first}");
        assert!(
            !first.contains("refused"),
            "nothing is wrong with it: {first}"
        );
        // No trailing padding: the bus column is padded for a note that is not
        // there.
        assert_eq!(first, first.trim_end());

        let second = rows.next().expect("the system disk");
        assert!(
            second.contains("[refused: running system]"),
            "the one that would destroy the machine: {second}"
        );
    }

    /// The board `a_device` returns, as the thing a verb acts on.
    fn a_chosen_board() -> Chosen {
        Chosen::Usb(a_device())
    }

    /// A block device that is nobody's system disk: an SD card in a reader.
    ///
    /// It is built by hand rather than read from this machine. A test that
    /// depends on what is plugged into the machine running it can pass for the
    /// wrong reason.
    fn a_card() -> BlockDevice {
        BlockDevice {
            name: "sdb".to_string(),
            node: "/dev/sdb".to_string(),
            bytes: 31_914_983_424,
            logical_block: 512,
            physical_block: 512,
            removable: true,
            read_only: false,
            bus: pyrographer_core::block::Bus::Usb,
            model: Some("Generic Storage Device".to_string()),
            mounts: Vec::new(),
            carries_running_system: false,
        }
    }

    fn a_device() -> DeviceInfo {
        DeviceInfo {
            vendor: pyrographer_core::discovery::Vendor::Rockchip,
            vendor_id: 0x2207,
            product_id: 0x350e,
            bcd_usb: 0x0201,
            mode: Mode::Loader,
            bus_id: "003".to_string(),
            device_address: 12,
        }
    }

    /// The plan is the last thing a person reads before flash is overwritten.
    /// Every fact they need to catch a mistake must be in it:
    ///
    /// - Which board
    /// - Which loader is answering on it
    /// - Where the bytes land, in sectors and in bytes
    /// - How much of the image is padding
    /// - How big the part is
    #[test]
    fn the_plan_shows_everything_a_person_needs_to_refuse_it() {
        let plan = WritePlan {
            lba: 64,
            image_bytes: 1000,
            padding_bytes: 24,
            sectors: 2,
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: vec![0x38, 0x38, 0x35, 0x33],
            soc: None,
            touches: Touches::Partitions(vec![Overlap {
                name: "uboot".to_string(),
                first_lba: 64,
                covered: 2,
                total: 8192,
            }]),
            read_back: ReadBack::PerWindow,
        };

        let rendered = render_plan(&a_chosen_board(), "boot.img", &plan);

        assert!(rendered.contains("2207:350e"), "the board: {rendered}");
        assert!(rendered.contains("boot.img"), "the image: {rendered}");
        assert!(
            rendered.contains("LBA range    64 through 65 (2 sectors)"),
            "where it lands: {rendered}"
        );
        assert!(
            rendered.contains("byte range   32768 through 33791"),
            "and in bytes: {rendered}"
        );
        assert!(
            rendered.contains("padded with 24 B"),
            "what is not the image: {rendered}"
        );
        assert!(
            rendered.contains("58.24 GiB"),
            "how big the part is: {rendered}"
        );
        assert!(
            rendered.contains("38 38 35 33") && rendered.contains("8853"),
            "which loader is answering: {rendered}"
        );
        assert!(
            rendered.contains("named SoC    none: the write will be refused"),
            "and the gate's verdict on it: {rendered}"
        );
        assert!(
            rendered.contains("Nothing here can be undone"),
            "what it costs to be wrong: {rendered}"
        );
        assert!(
            rendered.contains("read back and compared before the next one is written"),
            "and what it does to catch it: {rendered}"
        );
    }

    /// The plan says when the write is checked, in the backend's own words. A
    /// board that can be checked only after committing says so here, while a
    /// person is deciding whether to agree, and not afterwards.
    #[test]
    fn the_plan_says_when_a_write_would_be_checked() {
        let mut plan = WritePlan {
            lba: 64,
            image_bytes: 1000,
            padding_bytes: 24,
            sectors: 2,
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: Vec::new(),
            soc: None,
            touches: Touches::NoTable,
            read_back: ReadBack::PerWindow,
        };
        let per_window = render_plan(&a_chosen_board(), "boot.img", &plan);
        assert!(
            per_window.contains("stops the write at that window"),
            "{per_window}"
        );

        plan.read_back = ReadBack::AfterCommit;
        let after = render_plan(&a_chosen_board(), "boot.img", &plan);
        assert!(
            after.contains("can be read back only after the write is committed"),
            "{after}"
        );
        assert!(
            after.contains("already written"),
            "and what that costs: {after}"
        );
    }

    /// An image that fills its last sector has no padding, and the plan says
    /// nothing about padding. A "padded with 0 B" would be noise on the screen
    /// where noise costs the most.
    #[test]
    fn a_plan_with_no_padding_does_not_mention_padding() {
        let plan = WritePlan {
            lba: 0,
            image_bytes: 1024,
            padding_bytes: 0,
            sectors: 2,
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: Vec::new(),
            soc: None,
            touches: Touches::NoTable,
            read_back: ReadBack::PerWindow,
        };

        let rendered = render_plan(&a_chosen_board(), "boot.img", &plan);
        assert!(!rendered.contains("padded"), "{rendered}");
        assert!(rendered.contains("1.00 KiB"), "{rendered}");
    }

    /// `repair-table` reads its flags and defaults to asking, like every other
    /// write. An unpinned SoC is refused up front, because the parse runs the
    /// same `--soc` reader `flash` does.
    #[test]
    fn repair_table_reads_its_flags_and_refuses_an_unpinned_soc() {
        let args =
            parse_repair_table(&mut arguments(&["--soc", "rk3576"])).expect("a valid repair");
        assert_eq!(args.soc.map(|soc| soc.name()), Some("rk3576"));
        assert!(!args.yes, "it asks by default");
        assert!(!args.dry_run);
        assert_eq!(args.device, None);

        let error = parse_repair_table(&mut arguments(&["--soc", "rk3588"]))
            .expect_err("no board has pinned rk3588");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    /// `author-param` needs a medium and exactly one layout source, and reads the
    /// same `--soc` gate every write does.
    #[test]
    fn author_param_reads_a_medium_and_one_layout_source() {
        let args = parse_author_param(&mut arguments(&[
            "--soc",
            "rk3576",
            "--medium",
            "nand",
            "--layout",
            "board.txt",
        ]))
        .expect("a valid author");
        assert_eq!(args.medium, ParamMedium::Nand);
        assert_eq!(args.source, AuthorSource::Layout("board.txt".to_string()));
        assert_eq!(args.soc.map(|soc| soc.name()), Some("rk3576"));

        // No medium is a usage error: it decides where the copies go.
        assert!(matches!(
            parse_author_param(&mut arguments(&["--layout", "board.txt"])),
            Err(CliError::Usage(_))
        ));

        // Two sources at once is refused rather than half-obeyed.
        assert!(matches!(
            parse_author_param(&mut arguments(&[
                "--medium",
                "emmc",
                "--layout",
                "board.txt",
                "--from-block",
                "param.txt",
            ])),
            Err(CliError::Usage(_))
        ));
    }

    /// `author-gpt` takes exactly one layout source, and the same `--soc` gate
    /// every write does. Unlike `author-param`, it needs no medium.
    #[test]
    fn author_gpt_reads_one_layout_source() {
        let args = parse_author_gpt(&mut arguments(&[
            "--soc",
            "rk3576",
            "--layout",
            "board.txt",
        ]))
        .expect("a valid author");
        assert_eq!(args.source, GptSource::Layout("board.txt".to_string()));
        assert_eq!(args.soc.map(|soc| soc.name()), Some("rk3576"));
        assert!(!args.yes, "it asks by default");

        // mtdparts is the other source.
        let args = parse_author_gpt(&mut arguments(&["--mtdparts", "cmdline.txt"]))
            .expect("a valid author");
        assert_eq!(args.source, GptSource::Mtdparts("cmdline.txt".to_string()));

        // No source at all is a usage error.
        assert!(matches!(
            parse_author_gpt(&mut arguments(&["--soc", "rk3576"])),
            Err(CliError::Usage(_))
        ));

        // Two sources at once is refused rather than half-obeyed.
        assert!(matches!(
            parse_author_gpt(&mut arguments(&[
                "--layout",
                "board.txt",
                "--mtdparts",
                "cmdline.txt",
            ])),
            Err(CliError::Usage(_))
        ));
    }

    /// The repair plan is the last thing a person reads before a GPT is
    /// overwritten. It names everything they need to see that this is not the
    /// board they meant:
    ///
    /// - The copy it rewrites
    /// - The copy it rebuilds from
    /// - The partitions it restores
    /// - The loader answering
    /// - The wrong-loader verdict
    #[test]
    fn the_repair_plan_shows_the_copy_it_rewrites_and_what_it_restores() {
        let plan = SegmentedPlan {
            format: TableFormat::Gpt,
            action: TableAction::Repair {
                source: "the backup GPT in the device's last sector".to_string(),
            },
            partitions: vec![
                Partition {
                    name: "uboot".to_string(),
                    first_lba: 16384,
                    sectors: 8192,
                },
                Partition {
                    name: "trust".to_string(),
                    first_lba: 24576,
                    sectors: 8192,
                },
            ],
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            // The pinned RK3576 reply, so the gate's verdict reads as a match.
            chip_version: vec![0x36, 0x37, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            soc: Some(Soc::parse("rk3576").expect("pinned")),
            read_back: pyrographer_core::agent::ReadBack::PerWindow,
            segments: vec![Segment {
                what: "the primary GPT (sector 1 onward)".to_string(),
                lba: 1,
                bytes: vec![0u8; 1024],
                touches: Touches::Partitions(Vec::new()),
            }],
        };

        let rendered = render_segmented_plan(&a_chosen_board(), &plan);

        assert!(rendered.contains("2207:350e"), "the board: {rendered}");
        assert!(
            rendered.contains("repair the GPT table"),
            "the act: {rendered}"
        );
        assert!(
            rendered.contains("primary GPT"),
            "the copy rewritten: {rendered}"
        );
        assert!(
            rendered.contains("backup GPT in the device's last sector"),
            "the copy rebuilt from: {rendered}"
        );
        assert!(
            rendered.contains("uboot") && rendered.contains("trust"),
            "what it restores: {rendered}"
        );
        assert!(
            rendered.contains("LBA 1 through 2"),
            "where it lands: {rendered}"
        );
        assert!(
            rendered.contains("36 37 35 33") && rendered.contains("6753"),
            "which loader is answering: {rendered}"
        );
        assert!(
            rendered.contains("rk3576: the loader's answer matches"),
            "the gate's verdict: {rendered}"
        );
        assert!(
            rendered.contains("intact copy is not touched"),
            "the safety net: {rendered}"
        );
        // The backend's own account of when the write is proved to have landed,
        // not a sentence this renderer wrote: a table write is a write, and
        // `ReadBack` is where that fact is worded once.
        assert!(
            rendered.contains("read back and compared before the next one is written"),
            "when it is checked: {rendered}"
        );
        assert!(
            rendered.contains("Nothing here can be undone"),
            "what it costs to be wrong: {rendered}"
        );
    }

    /// The plan reads the partition table to print the `touches` line. A person
    /// cannot check "LBA 16384, 8192 sectors" against anything they know. They
    /// can check "the whole of uboot". If they meant to write `boot`, that line
    /// is where they stop.
    #[test]
    fn the_plan_names_the_partitions_the_write_would_land_in() {
        let touches = Touches::Partitions(vec![
            Overlap {
                name: "uboot".to_string(),
                first_lba: 16384,
                covered: 8192,
                total: 8192,
            },
            Overlap {
                name: "trust".to_string(),
                first_lba: 24576,
                covered: 100,
                total: 8192,
            },
        ]);

        let rendered = render_touches(&touches);

        assert!(
            rendered.contains("uboot") && rendered.contains("the whole of it (8192 sectors)"),
            "the partition it fills: {rendered}"
        );
        assert!(
            rendered.contains("trust") && rendered.contains("100 of its 8192 sectors"),
            "and the one it runs into: {rendered}"
        );
    }

    /// A write outside every partition overlaps nothing, and "nothing" must not
    /// render as a blank. On a Rockchip board, the sectors outside every
    /// partition hold the bootloader. Such a write is either exactly what was
    /// meant or a write into a gap, and the plan says it cannot tell which.
    #[test]
    fn a_write_that_lands_in_no_partition_says_so_rather_than_saying_nothing() {
        let rendered = render_touches(&Touches::Partitions(Vec::new()));
        assert!(rendered.contains("no partition"), "{rendered}");
        assert!(rendered.contains("outside"), "{rendered}");
    }

    /// pyrographer can fail to know in two different ways. A board with no table
    /// is a normal board. A board whose table fails its checksum is damaged.
    /// Rendering that as an empty list would tell a person about to overwrite it
    /// that nothing was there.
    #[test]
    fn a_plan_that_cannot_read_the_table_says_which_kind_of_cannot() {
        let rendered = render_touches(&Touches::NoTable);
        assert!(rendered.contains("no partition table"), "{rendered}");

        let rendered = render_touches(&Touches::UnreadableTable {
            format: "GPT",
            detail: "the header's CRC is 0x1 and the header says 0x2".to_string(),
        });
        assert!(rendered.contains("GPT"), "{rendered}");
        assert!(rendered.contains("is damaged"), "{rendered}");
        assert!(rendered.contains("CRC"), "the detail: {rendered}");
    }

    /// A clone overwrites every partition on the board, and an Android layout can
    /// run to thirty. The list is capped so the screen stays readable. The line
    /// that replaces the rest says how many, because a list that stopped silently
    /// would read as a shorter list.
    #[test]
    fn a_long_list_of_partitions_is_capped_and_says_how_many_it_dropped() {
        let touches = Touches::Partitions(
            (0..PARTITIONS_LISTED + 5)
                .map(|i| Overlap {
                    name: format!("part{i}"),
                    first_lba: i as u64 * 1024,
                    covered: 1024,
                    total: 1024,
                })
                .collect(),
        );

        let rendered = render_touches(&touches);
        assert!(rendered.contains("part0"), "{rendered}");
        assert!(
            rendered.contains(&format!("part{}", PARTITIONS_LISTED - 1)),
            "the last one shown: {rendered}"
        );
        assert!(
            !rendered.contains(&format!("part{PARTITIONS_LISTED}")),
            "the first one dropped: {rendered}"
        );
        assert!(rendered.contains("... and 5 more"), "{rendered}");
    }

    /// A table is rendered as what a person acts on: a name, a place, and a
    /// length. A GPT also records type and instance GUIDs, and neither changes
    /// what a person does next.
    #[test]
    fn a_partition_table_is_rendered_as_names_places_and_lengths() {
        let table = PartitionTable {
            format: TableFormat::Gpt,
            partitions: vec![Partition {
                name: "boot".to_string(),
                first_lba: 32768,
                sectors: 229_376,
            }],
            recovery: None,
        };

        let rendered = render_partitions(&table, 512);
        assert!(rendered.contains("GPT"), "the format: {rendered}");
        assert!(rendered.contains("boot"), "the name: {rendered}");
        assert!(rendered.contains("32768"), "where it starts: {rendered}");
        assert!(rendered.contains("229376"), "how far it runs: {rendered}");
        assert!(rendered.contains("112.00 MiB"), "and in bytes: {rendered}");
        // A healthy table leads with the list, not a warning.
        assert!(
            !rendered.contains("Warning"),
            "no caution on a healthy table: {rendered}"
        );
    }

    /// A table recovered from the backup leads with the reason: the partitions
    /// are the backup's, and the device's primary copy is damaged. A person
    /// reading a table from a board they are about to write to must see that
    /// before the list.
    #[test]
    fn a_recovered_partition_table_warns_that_the_primary_is_damaged() {
        let table = PartitionTable {
            format: TableFormat::Gpt,
            partitions: vec![Partition {
                name: "boot".to_string(),
                first_lba: 32768,
                sectors: 229_376,
            }],
            recovery: Some(TableRecovery {
                primary_detail: "the header's CRC is 0x00000000, and the header says 0xdeadbeef"
                    .to_string(),
                recovered_from: "the backup GPT in the device's last sector",
            }),
        };

        let rendered = render_partitions(&table, 512);
        assert!(rendered.contains("Warning"), "it warns: {rendered}");
        assert!(
            rendered.contains("primary GPT is damaged"),
            "it names what is wrong: {rendered}"
        );
        assert!(
            rendered.contains("backup GPT in the device's last sector"),
            "it says where the good copy was: {rendered}"
        );
        assert!(
            rendered.contains("CRC"),
            "it carries the detail: {rendered}"
        );
        // And the partitions are still shown -- the recovery is a caution beside a
        // real table, not a refusal.
        assert!(
            rendered.contains("boot"),
            "the partitions still list: {rendered}"
        );
    }

    /// pico-args returns what it is asked for and leaves the rest, so an argument
    /// nobody read would be silently ignored. `flash --partition boot 64 boot.img`
    /// would take `64` as the file to write, and never mention `boot.img`. On a
    /// command that overwrites a board, a stray argument is refused.
    #[test]
    fn an_argument_nothing_reads_is_a_usage_problem_and_not_a_shrug() {
        let mut args = arguments(&["--partition", "boot", "64", "boot.img"]);
        let parsed = parse_flash(&mut args).expect("the partition and a file parse");

        // `64` was read as the file, and `boot.img` is sitting there unread.
        assert_eq!(parsed.path, "64");

        let error = no_more(args).expect_err("boot.img was never read");
        let CliError::Usage(message) = error else {
            panic!("a stray argument is a usage problem: {error:?}");
        };
        assert!(message.contains("boot.img"), "{message}");

        // And a command line where everything was read is fine.
        let mut args = arguments(&["--partition", "boot", "boot.img"]);
        parse_flash(&mut args).expect("a valid flash");
        no_more(args).expect("nothing is left over");
    }

    /// A recovery names a port, an agent, and at least one thing to write. It
    /// selects no bus device, because there is nothing to scan. A serial port is a
    /// path, not a device on a bus.
    #[test]
    fn recover_reads_its_port_agent_and_files() {
        let args = parse_recover(&mut arguments(&[
            "--port",
            "/dev/ttyUSB0",
            "--agent",
            "recovery.bin",
            "--spl",
            "spl.bin",
            "--uboot",
            "u-boot.itb",
        ]))
        .expect("a valid recovery");

        assert_eq!(
            args,
            RecoverArgs {
                port: "/dev/ttyUSB0".to_string(),
                agent: "recovery.bin".to_string(),
                spl: Some("spl.bin".to_string()),
                uboot: Some("u-boot.itb".to_string()),
                yes: false,
                dry_run: false,
            }
        );

        // One stage alone parses. Whether it is a valid recovery is the plan's
        // call, which looks at the files.
        let args = parse_recover(&mut arguments(&[
            "--port", "COM3", "--agent", "a.bin", "--uboot", "u.itb",
        ]))
        .expect("a U-Boot-only recovery");
        assert_eq!(args.spl, None);
        assert_eq!(args.uboot.as_deref(), Some("u.itb"));
    }

    /// A recovery that writes neither an SPL nor a U-Boot has no meaning. It is
    /// refused as a usage error before any port is opened.
    #[test]
    fn a_recover_with_neither_spl_nor_uboot_is_a_usage_problem() {
        let error = parse_recover(&mut arguments(&[
            "--port",
            "/dev/ttyUSB0",
            "--agent",
            "recovery.bin",
        ]))
        .expect_err("there is nothing to write");

        let CliError::Usage(message) = error else {
            panic!("nothing to write is a usage problem: {error:?}");
        };
        assert!(message.contains("--spl or --uboot"), "{message}");
    }

    /// `recover` writes the boot flash and takes no medium. A `--target` left over
    /// from habit is not read, and is refused as an argument nothing used rather
    /// than ignored.
    #[test]
    fn a_recover_target_is_not_an_option() {
        let mut args = arguments(&[
            "--port",
            "/dev/ttyUSB0",
            "--target",
            "emmc",
            "--agent",
            "recovery.bin",
            "--uboot",
            "u-boot.itb",
        ]);
        parse_recover(&mut args).expect("the options recover does take");
        assert!(no_more(args).is_err(), "--target is left over, and refused");
    }

    /// Every other write is read back, and a StarFive recovery cannot be. The
    /// plan a person confirms must say so plainly. It also names the port, and
    /// where each stage goes and under which menu entry.
    #[test]
    fn the_recovery_plan_says_where_each_stage_goes_and_that_it_is_not_read_back() {
        let plan = RecoveryPlan {
            agent_bytes: 160 * 1024,
            agent_crc32: 0,
            stages: vec![
                PlannedStage {
                    kind: StageKind::Spl,
                    menu_option: 0,
                    image_bytes: 128 * 1024,
                    offset: 0,
                    backup_offset: Some(0x20_0000),
                    origin: Some(Origin::HeaderedHere),
                    crc32: 0,
                },
                PlannedStage {
                    kind: StageKind::UBoot,
                    menu_option: 2,
                    image_bytes: 3 * 1024 * 1024,
                    offset: 0x10_0000,
                    backup_offset: None,
                    origin: None,
                    crc32: 0,
                },
            ],
            verified: false,
        };

        let rendered = render_recovery_plan("/dev/ttyUSB0", &plan);

        assert!(rendered.contains("/dev/ttyUSB0"), "the port: {rendered}");
        assert!(rendered.contains("boot flash"), "the medium: {rendered}");
        assert!(
            rendered.contains("at 0x0, agent menu entry 0, headered here"),
            "the SPL stage: {rendered}"
        );
        assert!(
            rendered.contains("at 0x100000, agent menu entry 2"),
            "the U-Boot stage: {rendered}"
        );
        assert!(
            rendered.contains("backup copy of the SPL at 0x200000"),
            "where the backup copy lands: {rendered}"
        );
        assert!(
            rendered.contains("Warning: this write is not read back"),
            "the one line it exists for: {rendered}"
        );
        assert!(
            rendered.contains("Nothing here can be undone"),
            "and what it costs: {rendered}"
        );
    }

    /// A RAM boot names a port and both files, and the prompt defaults to
    /// mainline's.
    #[test]
    fn uartboot_reads_its_port_files_and_prompt() {
        let args = parse_uartboot(&mut arguments(&[
            "--port",
            "/dev/ttyUSB0",
            "--spl",
            "u-boot-spl.bin.normal.out",
            "--uboot",
            "u-boot.itb",
        ]))
        .expect("a valid RAM boot");
        assert_eq!(
            args,
            UartbootArgs {
                port: "/dev/ttyUSB0".to_string(),
                spl: "u-boot-spl.bin.normal.out".to_string(),
                uboot: "u-boot.itb".to_string(),
                prompt: "=> ".to_string(),
            }
        );

        let error = parse_uartboot(&mut arguments(&["--port", "/dev/ttyUSB0", "--spl", "s"]))
            .expect_err("no U-Boot");
        assert!(matches!(error, CliError::Usage(_)), "{error:?}");
    }

    /// Core's hints name an action, because the window shows them too. Keyed on
    /// the error, the CLI names the command for that action on the next line. A
    /// person at a prompt still learns what to type.
    #[test]
    fn a_neutral_hint_is_followed_by_the_command_that_carries_it_out() {
        let maskrom = Error::WrongMode {
            found: "maskrom",
            needed: "loader",
        };
        assert!(maskrom.hint().is_some(), "core offers the action");
        assert_eq!(error_next_step(&maskrom), Some(RUN_DB));
        assert!(RUN_DB.contains("pyrographer db --loader"), "{RUN_DB}");

        let mismatch = Error::LoaderMismatch {
            named: "rk3576",
            expected: vec![0x36, 0x37, 0x35, 0x33],
            answered: vec![0x38, 0x38, 0x35, 0x33],
        };
        assert_eq!(error_next_step(&mismatch), Some(RUN_CHIPVER));
        assert!(RUN_CHIPVER.contains("pyrographer chipver"), "{RUN_CHIPVER}");

        let gpt = Error::CorruptTable {
            format: TableFormat::Gpt.name(),
            detail: "the header's CRC is 0x1 and the header says 0x2".to_string(),
        };
        let step = error_next_step(&gpt).expect("a damaged GPT has commands");
        assert!(
            step.contains("pyrographer repair-table") && step.contains("pyrographer author-gpt"),
            "{step}"
        );

        let param = Error::CorruptTable {
            format: TableFormat::RockchipParam.name(),
            detail: "no copy checks".to_string(),
        };
        let step = error_next_step(&param).expect("a damaged parameter has commands");
        assert!(
            step.contains("pyrographer repair-param") && step.contains("pyrographer author-param"),
            "{step}"
        );

        // A loader-mode refusal of a mass-storage board names no upload: the
        // board needs no loader, and the line would send somebody the wrong way.
        let mass_storage = Error::WrongMode {
            found: "mass storage",
            needed: "loader",
        };
        assert_eq!(error_next_step(&mass_storage), None);
        assert_eq!(error_next_step(&Error::DeviceNotFound), None);
    }

    /// A reset into maskrom leaves the board needing a loader, and core's outcome
    /// says so in words. The CLI follows it with the command. No other mode leaves
    /// a step to take.
    #[test]
    fn a_reset_into_maskrom_names_the_upload_command() {
        assert!(
            ResetMode::Maskrom.outcome().contains("Upload a loader"),
            "core names the action: {}",
            ResetMode::Maskrom.outcome()
        );
        assert_eq!(reset_next_step(ResetMode::Maskrom), Some(RUN_DB));
        for mode in [
            ResetMode::Reset,
            ResetMode::MassStorage,
            ResetMode::PowerOff,
        ] {
            assert_eq!(reset_next_step(mode), None, "{}", mode.name());
        }
    }

    /// A firmware plan as one of the scripted RK3576 plans would carry it: a
    /// partition image, both GPT copies, and the ID block, with the loader's
    /// capability reply set as the test needs.
    fn a_firmware_plan(what: FirmwareWrite, capability: LoaderCapability) -> FirmwarePlan {
        use pyrographer_core::codec::idb::IdbImage;
        use pyrographer_core::verbs::{Run, RunSource};

        let run = |what: &str, lba: u64, sectors: u64, source: RunSource| Run {
            what: what.to_string(),
            lba,
            bytes: sectors * 512,
            sectors,
            source,
            touches: Touches::Partitions(Vec::new()),
        };
        FirmwarePlan {
            what,
            runs: vec![
                run(
                    "partition 'boot' from Image/boot.img",
                    0xa000,
                    2048,
                    RunSource::Package { offset: 4096 },
                ),
                run("the primary GPT", 0, 34, RunSource::Held(vec![0; 34 * 512])),
                run(
                    "the ID block, 3 images laid out by its RKNS header",
                    64,
                    704,
                    RunSource::Held(vec![0; 704 * 512]),
                ),
            ],
            partitions: vec![Partition {
                name: "boot".to_string(),
                first_lba: 0xa000,
                sectors: 0x20000,
            }],
            skipped: vec![pyrographer_core::firmware::Skipped {
                name: "package-file".to_string(),
                path: "package-file".to_string(),
                why: "the entry is the packing tool's own list of files",
            }],
            id_block_header: "FlashHead".to_string(),
            id_block_images: vec![IdbImage {
                stage: "FlashBoost".to_string(),
                sector: 8,
                sectors: 8,
                load_address: Some(0x3ffc_0000),
            }],
            loader_chip: Some(*b"6753"),
            capability,
            flash: FlashInfo {
                size_bytes: 122_142_720 * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: vec![0x36, 0x37, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            soc: Some(Soc::parse("rk3576").expect("pinned")),
            read_back: ReadBack::PerWindow,
        }
    }

    /// A package plan shows everything a person checks before the board's boot
    /// chain, table and partitions are replaced: the package, every run and where
    /// it lands, the ID block's images, the resulting table, what is left out,
    /// what the loader file claims, what the loader said it can do, and the gate.
    #[test]
    fn the_firmware_plan_shows_every_run_and_what_gates_it() {
        let new_idb = pyrographer_core::codec::rockusb::parse_capability(&[0, 1, 0, 0, 0, 0, 0, 0])
            .expect("eight bytes");
        let plan = a_firmware_plan(
            FirmwareWrite::Package {
                model: "RK3576".to_string(),
                manufacturer: "rockchip".to_string(),
                version: 0x0102_0003,
            },
            LoaderCapability::Answered(new_idb),
        );
        let rendered = render_firmware_plan(&a_chosen_board(), Some("update.img"), &plan);

        for wanted in [
            "firmware package update.img to 2207:350e",
            "RK3576 by rockchip, version 1.2.3",
            "partition 'boot' from Image/boot.img",
            "LBA 40960 through 43007 (2048 sectors)",
            "LBA 64 through 767 (704 sectors)",
            "FlashBoost at sector 8, 8 sectors, loads at 0x3ffc0000",
            "laid out by the FlashHead header, every hash checked",
            "package-file: the entry is the packing tool's own list of files",
            "36 37 35 33 \"6753\" (rk3576)",
            "NEW_IDB set",
            "rk3576: the loader's answer matches",
            "The package's partition table replaces the device's",
            "Nothing here can be undone",
        ] {
            assert!(
                rendered.contains(wanted),
                "missing {wanted:?} in:\n{rendered}"
            );
        }
    }

    /// An ID block plan says the table is left alone, and a loader that did not
    /// answer the capability query is shown as the refusal it becomes.
    #[test]
    fn an_id_block_plan_shows_the_table_is_left_and_the_capability_refusal() {
        let plan = a_firmware_plan(
            FirmwareWrite::IdBlock,
            LoaderCapability::NotAnswered("the command failed".to_string()),
        );
        let rendered = render_firmware_plan(&a_chosen_board(), None, &plan);
        assert!(
            rendered.contains("write an ID block to 2207:350e"),
            "{rendered}"
        );
        assert!(rendered.contains("left as it is"), "{rendered}");
        assert!(
            rendered.contains("no answer (the command failed): the write will be refused"),
            "{rendered}"
        );
    }

    /// `flash` refuses a container by its first bytes, before a device is chosen,
    /// and names the command that writes what was picked. An ordinary image, and
    /// one too short to begin a container, pass.
    #[test]
    fn flash_refuses_a_container_before_choosing_a_device() {
        let dir = std::env::temp_dir();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(format!("pyrographer-cli-{name}-{}", std::process::id()));
            std::fs::write(&path, bytes).expect("a scratch file");
            path.display().to_string()
        };

        let package = write("package", b"RKFW and the rest");
        let error = refuse_container(&package).expect_err("a package");
        assert!(error.to_string().contains("flash-firmware"), "{error}");

        let loader = write("loader", b"LDR and the rest");
        let error = refuse_container(&loader).expect_err("a loader container");
        assert!(error.to_string().contains("write-idb"), "{error}");

        let image = write("image", b"ANDROID! boot image");
        refuse_container(&image).expect("an ordinary image");
        let tiny = write("tiny", b"RK");
        refuse_container(&tiny).expect("two bytes begin nothing");

        for path in [package, loader, image, tiny] {
            let _ = std::fs::remove_file(path);
        }
    }
}
