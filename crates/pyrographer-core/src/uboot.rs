//! The U-Boot driver: a bootloader prompt, driven over a serial line.
//!
//! Like [`recovery`](crate::recovery), this driver consumes a [`Serial`] transport
//! directly, with no [`FlashAgent`](crate::agent::FlashAgent). A U-Boot prompt
//! addresses no sectors, reports no geometry, and carries no partition table. The
//! driver therefore offers no block interface, and a front-end has no block verbs
//! to gray out for it.
//!
//! # The RAM-boot flow
//!
//! This driver runs after a bootstrap. [`bootstrap::download_boot`] loads a U-Boot
//! into DRAM over USB. That U-Boot answers on the serial line, and this driver
//! drives it there. The two halves of the RAM-boot flow meet here. A maskrom board
//! is given a mainline U-Boot over USB. The console then tells it to start a
//! [`Gadget`] that exposes the board's flash to the host.
//!
//! # Prompts
//!
//! The prompt varies by board: `=> `, `U-Boot> `, or a board-specific
//! `CONFIG_SYS_PROMPT`. pyrographer keeps no catalog of boards, just as it leaves
//! loader blobs for the user to supply. [`DEFAULT_PROMPT`] is therefore a default,
//! and [`UBoot::with_prompt`] names another.
//!
//! # Exit status
//!
//! U-Boot's hush shell sets `$?`. Whether a given build has hush or the simple
//! parser depends on its build configuration, which pyrographer cannot read from
//! outside. Nothing here therefore depends on `$?`. A caller that must judge success
//! judges the returned text. **\[UNVERIFIED\]**
//!
//! # The boot override
//!
//! [`UBoot::plan_boot_override`] and [`UBoot::boot_override`] set `boot_targets` and
//! run `boot`. They send **no `saveenv`**, by design. U-Boot keeps its environment
//! in RAM until `saveenv` writes it to storage. The override therefore lasts until
//! the next reset, and the boot order in storage is unchanged. **\[DOC\]**
//!
//! The property holds by construction, not by a check: this module has no `saveenv`
//! operation, so no path it composes can emit one. The StarFive backend makes OTP
//! fuse burning unreachable the same way, by never offering it. [`UBoot::run`]
//! types whatever a caller passes it. A front-end that exposes [`UBoot::run`]
//! exposes a raw prompt, and says so.
//!
//! The plan is **async**, unlike [`plan_recover`](crate::recovery::plan_recover),
//! because it queries the board. It runs `printenv boot_targets`, and reports the
//! current order beside the order it would set.
//! [`plan_write`](crate::verbs::plan_write) likewise asks the device rather than
//! assuming.
//!
//! [`bootstrap::download_boot`]: crate::bootstrap::download_boot
//! [`Serial`]: crate::transport::Serial

use crate::codec::console::{Console, text};
use crate::console::{self, ConsoleSink, DEFAULT_READS};
use crate::progress::Cancel;
use crate::transport::Serial;
use crate::{Error, Result};

/// The prompt mainline U-Boot presents unless a board's config says otherwise.
pub const DEFAULT_PROMPT: &str = "=> ";

/// The countdown banner U-Boot prints while it waits to be interrupted.
///
/// Matched as a prefix of the line, without the countdown digits after it, because
/// some builds rewrite those in place with backspaces. **\[DOC\]**
pub const AUTOBOOT_BANNER: &[u8] = b"Hit any key to stop autoboot";

/// The environment variable that names U-Boot's boot order.
pub const BOOT_TARGETS: &str = "boot_targets";

/// How many reads to spend finding out whether a gadget command *returned*.
///
/// It is small on purpose, because its exhaustion is the **success** signal. A
/// running gadget holds the console and never returns the prompt. A prompt that does
/// arrive means the command returned, and a gadget command that returns has failed.
/// See [`UBoot::start_gadget`].
const GADGET_SETTLE_READS: u32 = 3;

/// A U-Boot gadget that hands a board's flash to the host over USB.
///
/// Both take the console for as long as they run, and both take the same three
/// arguments, a [`GadgetDevice`]. They differ in what the host sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gadget {
    /// `rockusb`: the board appears to `list` as a loader-mode device.
    ///
    /// Mainline's gadget does not answer `K_FW_READ_FLASH_INFO`, so `info` and the
    /// partition verbs fail against it. A read by LBA asks for no flash
    /// information.
    Rockusb,
    /// `ums`, USB mass storage: the board's flash appears to the host as a disk,
    /// which the Block backend reads.
    ///
    /// On an RK3576, this gadget read the whole eMMC from a RAM-booted U-Boot, past
    /// the 32 MiB point where rkbin's loader returns fill.
    Ums,
}

impl Gadget {
    /// Every gadget, in the order a front-end offers them.
    pub const ALL: [Gadget; 2] = [Gadget::Rockusb, Gadget::Ums];

    /// The U-Boot command that starts this gadget, which is also its name on a
    /// command line.
    pub fn command_word(self) -> &'static str {
        match self {
            Gadget::Rockusb => "rockusb",
            Gadget::Ums => "ums",
        }
    }

    /// Read a gadget by its command word.
    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|gadget| gadget.command_word() == name)
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "'{name}' is not a gadget pyrographer starts. It starts rockusb, which \
                     answers as a loader, and ums, which appears to the host as a disk"
                ))
            })
    }
}

/// Which block device a gadget command exposes: U-Boot's `<controller>
/// <interface> <index>`.
///
/// These are the three arguments of `rockusb 0 mmc 0` and `ums 0 mmc 0`.
/// pyrographer knows no board's storage layout, so the caller names the device and
/// nothing infers it. The default is controller 0, `mmc`, device 0. That default
/// suits an eMMC board, and matches the book's guide to RAM-booting U-Boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GadgetDevice {
    /// The USB controller index the gadget runs on.
    pub controller: u32,
    /// The block interface: `mmc`, `nvme`, `scsi`.
    pub interface: String,
    /// Which device of that interface.
    pub index: u32,
}

impl Default for GadgetDevice {
    fn default() -> Self {
        Self {
            controller: 0,
            interface: "mmc".to_string(),
            index: 0,
        }
    }
}

impl GadgetDevice {
    /// Read `mmc:0`, or `0:mmc:0` for a gadget on a USB controller other than the
    /// first.
    ///
    /// The interface must be alphanumeric, so a device spec cannot carry a second
    /// command into the line it is formatted into.
    pub fn parse(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.split(':').collect();
        let (controller, interface, index) = match parts.as_slice() {
            [interface, index] => ("0", *interface, *index),
            [controller, interface, index] => (*controller, *interface, *index),
            _ => {
                return Err(Error::InvalidRequest(format!(
                    "'{spec}' is not a device: write it <interface>:<index>, as in mmc:0, or \
                     <controller>:<interface>:<index> when the gadget is not on USB controller 0"
                )));
            }
        };

        if interface.is_empty() || !interface.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::InvalidRequest(format!(
                "'{interface}' is not a U-Boot block interface. Use an interface name such as \
                 mmc, nvme, or scsi"
            )));
        }
        let number = |what: &str, value: &str| {
            value.parse::<u32>().map_err(|_| {
                Error::InvalidRequest(format!("the {what} in '{spec}' is not a number"))
            })
        };

        Ok(Self {
            controller: number("controller", controller)?,
            interface: interface.to_string(),
            index: number("device index", index)?,
        })
    }

    /// The line that starts `gadget` on this device.
    pub fn command(&self, gadget: Gadget) -> String {
        format!(
            "{} {} {} {}",
            gadget.command_word(),
            self.controller,
            self.interface,
            self.index
        )
    }
}

/// What a boot override would change, before any of it happens.
///
/// It is the console counterpart of [`WritePlan`](crate::verbs::WritePlan) and
/// [`RecoveryPlan`](crate::recovery::RecoveryPlan). Like a write plan, it is
/// produced by asking the target. A U-Boot prompt reports its current environment,
/// so the plan carries the current boot order beside the order the override would
/// set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    /// The order the board reports right now, or `None` for a build where
    /// `boot_targets` is not set at all.
    pub current: Option<String>,
    /// The order the override would set.
    pub targets: String,
    /// Whether the change outlives the next reset. **Always `false`**, because the
    /// environment stays in RAM until `saveenv` writes it, and no path here emits
    /// `saveenv`. It is a field of the plan, so that a front-end shows it to the
    /// person confirming the override. [`RecoveryPlan::verified`] is a field for
    /// the same reason.
    ///
    /// [`RecoveryPlan::verified`]: crate::recovery::RecoveryPlan::verified
    pub persistent: bool,
}

impl BootPlan {
    /// Whether the board already boots in the order this would set, so the
    /// override changes nothing.
    pub fn is_no_change(&self) -> bool {
        self.current.as_deref() == Some(self.targets.as_str())
    }

    /// Say yes to this plan.
    ///
    /// It consumes the plan, so one confirmation authorizes one override and cannot
    /// be replayed. [`RecoveryPlan::confirm`] has the same shape.
    ///
    /// **The confirmation is a plain yes, not a typed coordinate.** A typed
    /// `bus:address` catches a clone with its source and destination swapped, a
    /// mistake careful reading misses. An override has no second board and destroys
    /// nothing. It changes volatile RAM on one board for one boot. A typed
    /// confirmation for a harmless act would teach people to skim the one that
    /// guards a destructive act.
    ///
    /// [`RecoveryPlan::confirm`]: crate::recovery::RecoveryPlan::confirm
    pub fn confirm(self) -> ConfirmedBoot {
        ConfirmedBoot(self)
    }
}

/// A [`BootPlan`] a caller has agreed to, and the only thing
/// [`UBoot::boot_override`] takes.
// Not `Clone`: one confirmation, one override.
#[derive(Debug)]
pub struct ConfirmedBoot(BootPlan);

impl ConfirmedBoot {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &BootPlan {
        &self.0
    }
}

/// A U-Boot prompt on the other end of a serial line.
///
/// It owns the line and the [`Console`] accumulated from it. The cursor into that
/// transcript stops one command's output from being read as the next command's.
/// It is generic over [`Serial`], so a scripted serial stands in for a board.
pub struct UBoot<S: Serial> {
    serial: S,
    console: Console,
    prompt: String,
    reads: u32,
}

impl<S: Serial> UBoot<S> {
    /// Take a serial line that a U-Boot is on, at the default prompt and read
    /// budget.
    ///
    /// Nothing is sent and nothing is read. The driver only holds the line. A
    /// caller usually calls [`interrupt_autoboot`](Self::interrupt_autoboot)
    /// first.
    pub fn new(serial: S) -> Self {
        Self {
            serial,
            console: Console::new(),
            prompt: DEFAULT_PROMPT.to_string(),
            reads: DEFAULT_READS,
        }
    }

    /// Drive a board whose prompt is not `=> `.
    pub fn with_prompt(mut self, prompt: &str) -> Self {
        self.prompt = prompt.to_string();
        self
    }

    /// Spend a different number of reads on each wait. See
    /// [`DEFAULT_READS`].
    pub fn with_reads(mut self, reads: u32) -> Self {
        self.reads = reads;
        self
    }

    /// The prompt this driver is matching.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// The transcript so far, and the cursor into it.
    pub fn console(&self) -> &Console {
        &self.console
    }

    /// Give the serial line back, so a caller can watch what a `boot` brought up.
    pub fn into_serial(self) -> S {
        self.serial
    }

    /// Stop the autoboot countdown and reach the prompt.
    ///
    /// It sends a bare line end ([`LINE_END`](crate::console::LINE_END), a carriage
    /// return), which stops the countdown and also submits an empty command line.
    /// The call is therefore idempotent against a board already at a prompt. An
    /// arbitrary keystroke would instead become the first character of a command.
    ///
    /// It then waits on the countdown banner and the prompt together, and the
    /// earliest occurrence wins. A board already at a prompt never prints the
    /// banner, and a board still counting down has not printed the prompt yet.
    ///
    /// If the banner arrives first, the driver sends a second line end before it
    /// waits for the prompt. The first line end can go out before the board is
    /// reading. An extra empty line at a prompt only produces another prompt.
    pub async fn interrupt_autoboot(
        &mut self,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        self.serial.write_all(console::LINE_END).await?;

        let patterns: [&[u8]; 2] = [self.prompt.as_bytes(), AUTOBOOT_BANNER];
        let found = console::wait_for(
            &mut self.serial,
            &mut self.console,
            &patterns,
            self.reads,
            sink,
            cancel,
        )
        .await
        .map_err(|err| self.no_prompt(err))?;
        self.console.consume(&found);

        // The prompt itself: nothing more to do.
        if found.pattern == 0 {
            return Ok(());
        }

        // The countdown. Ask again, now that the board is certainly listening.
        self.serial.write_all(console::LINE_END).await?;
        self.wait_for_prompt(sink, cancel).await
    }

    /// Run one command and return what it printed.
    ///
    /// It writes the line, **consumes the echo**, reads to the next prompt, and
    /// returns what lay between. A console echoes what is typed before the far end
    /// acts on it, so the bytes after a write begin with the command itself.
    /// Consuming that echo explicitly is cheaper and less fragile than a sentinel,
    /// which would have to be told apart from its own echo. It also needs no
    /// cooperation from the far end.
    ///
    /// The line terminators after the echo and before the prompt are trimmed, so
    /// the output is the text of the answer. Nothing else is interpreted. No exit
    /// status is read (see the module documentation), so a caller that must judge
    /// success judges this text. An empty command is refused, because an empty line
    /// only produces another prompt.
    pub async fn run(
        &mut self,
        command: &str,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<Vec<u8>> {
        if command.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "a console command cannot be empty. An empty line only produces another prompt"
                    .to_string(),
            ));
        }
        self.send(command, sink, cancel).await?;

        let prompt: [&[u8]; 1] = [self.prompt.as_bytes()];
        let found = console::wait_for(
            &mut self.serial,
            &mut self.console,
            &prompt,
            self.reads,
            sink,
            cancel,
        )
        .await
        .map_err(|err| self.no_prompt(err))?;

        let output = trim_ends(self.console.before(&found)).to_vec();
        self.console.consume(&found);
        Ok(output)
    }

    /// Start `gadget` on `device`, exposing the board's flash over USB.
    ///
    /// This is the last step of the RAM-boot flow. It starts the gadget through
    /// which a mainline U-Boot, brought up from maskrom, exposes its flash to the
    /// host. On success, a [`Gadget::Rockusb`] board appears to `list` as a
    /// loader-mode device. A [`Gadget::Ums`] board appears to `list --blocks` as a
    /// disk.
    ///
    /// **Here a spent budget is the success signal, by design.** A running gadget
    /// owns the console and never returns to the prompt. A prompt that does come
    /// back means the command returned, for example on an unknown command or a
    /// missing device. The driver then returns [`Error::Protocol`] carrying what
    /// U-Boot printed. Nothing else can be checked from the serial side. The caller
    /// confirms on the USB bus that the gadget enumerated.
    ///
    /// On success, it returns the command line that was sent, so a caller can show
    /// exactly what was typed.
    pub async fn start_gadget(
        &mut self,
        gadget: Gadget,
        device: &GadgetDevice,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<String> {
        let command = device.command(gadget);
        self.send(&command, sink, cancel).await?;

        let prompt: [&[u8]; 1] = [self.prompt.as_bytes()];
        let returned = console::look_for(
            &mut self.serial,
            &mut self.console,
            &prompt,
            GADGET_SETTLE_READS,
            sink,
            cancel,
        )
        .await?;

        match returned {
            Some(found) => {
                let said = text(trim_ends(self.console.before(&found)));
                self.console.consume(&found);
                Err(Error::Protocol(format!(
                    "U-Boot came back to the prompt after `{command}`, so no gadget is running. It \
                     said: {}",
                    if said.is_empty() {
                        "nothing at all".to_string()
                    } else {
                        said
                    }
                )))
            }
            // The prompt never came back, which is what a gadget holding the
            // console looks like.
            None => Ok(command),
        }
    }

    /// Ask the board for its current boot order, and report what an override would
    /// set. This is the dry run.
    ///
    /// It is async because it asks the board. It runs `printenv boot_targets` and
    /// reads the answer. A build with no `boot_targets` at all
    /// reports [`None`] as the current order, which is a finding and not a failure.
    ///
    /// `targets` is checked here, before anything is set. A boot order is words
    /// separated by spaces, and anything else returns [`Error::InvalidRequest`]. The
    /// check stops a second command from being smuggled into the `setenv` line this
    /// composes.
    pub async fn plan_boot_override(
        &mut self,
        targets: &str,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<BootPlan> {
        let targets = check_targets(targets)?;
        let printed = self
            .run(&format!("printenv {BOOT_TARGETS}"), sink, cancel)
            .await?;

        Ok(BootPlan {
            current: read_variable(&printed, BOOT_TARGETS),
            targets,
            // No path in this module emits `saveenv`; the environment stays in RAM.
            persistent: false,
        })
    }

    /// Set the boot order and boot, for this boot only.
    ///
    /// It sends two lines: `setenv boot_targets <targets>`, then `boot`. It sends
    /// **no `saveenv`**, so the stored order is unchanged and the next reset
    /// restores it.
    ///
    /// `boot` does not return to a prompt, because the board is booting and the
    /// console belongs to whatever comes up. Nothing is waited for after it. To see
    /// what it brought up, a caller takes the line back with
    /// [`into_serial`](Self::into_serial) and watches it, or calls
    /// [`drain`](Self::drain).
    pub async fn boot_override(
        &mut self,
        confirmed: ConfirmedBoot,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        let targets = &confirmed.plan().targets;
        // Re-checked, not trusted: a `ConfirmedBoot` is consent, not validation,
        // and this is the line the value is formatted into.
        let targets = check_targets(targets)?;

        self.run(&format!("setenv {BOOT_TARGETS} {targets}"), sink, cancel)
            .await?;
        self.send("boot", sink, cancel).await
    }

    /// Read whatever the board says next for `reads` reads, streaming it to `sink`.
    ///
    /// A caller uses this to watch a `boot` start. It matches nothing and waits for
    /// nothing. Silence costs a read, so a board that sends nothing ends the drain
    /// once `reads` are spent. What arrives goes to `sink` and to the transcript.
    pub async fn drain(
        &mut self,
        reads: u32,
        sink: ConsoleSink<'_>,
        cancel: &Cancel,
    ) -> Result<()> {
        // A pattern that cannot occur, so the loop spends its whole budget: this is
        // the read loop used for its side effect, which is the transcript.
        console::look_for(
            &mut self.serial,
            &mut self.console,
            &[],
            reads,
            sink,
            cancel,
        )
        .await?;
        Ok(())
    }

    /// Write a line and consume the echo of it, leaving the cursor at the start of
    /// whatever the far end says next.
    async fn send(&mut self, command: &str, sink: ConsoleSink<'_>, cancel: &Cancel) -> Result<()> {
        console::send_line(&mut self.serial, command).await?;

        let echo: [&[u8]; 1] = [command.as_bytes()];
        let found = console::look_for(
            &mut self.serial,
            &mut self.console,
            &echo,
            self.reads,
            sink,
            cancel,
        )
        .await?;

        let Some(found) = found else {
            return Err(Error::Protocol(format!(
                "the console did not echo `{command}`, so its output cannot be told apart from \
                 the command itself. A U-Boot prompt echoes what is typed. If the console does \
                 not echo, the cause can be a different boot stage, a different baud rate, or a \
                 console on another UART. What arrived:\n{}",
                self.console.tail_text()
            )));
        };
        self.console.consume(&found);
        Ok(())
    }

    /// Wait for the prompt and step past it.
    async fn wait_for_prompt(&mut self, sink: ConsoleSink<'_>, cancel: &Cancel) -> Result<()> {
        let prompt: [&[u8]; 1] = [self.prompt.as_bytes()];
        let found = console::wait_for(
            &mut self.serial,
            &mut self.console,
            &prompt,
            self.reads,
            sink,
            cancel,
        )
        .await
        .map_err(|err| self.no_prompt(err))?;
        self.console.consume(&found);
        Ok(())
    }

    /// Add the most common cause to the error for a prompt that never came.
    ///
    /// Only a spent budget is rewritten. A port that disconnected keeps its own
    /// error, because a disconnected line is not a board with a different prompt.
    fn no_prompt(&self, err: Error) -> Error {
        let Error::Protocol(message) = err else {
            return err;
        };
        Error::Protocol(format!(
            "{message}\nThe expected prompt is {:?}. Boards use different prompts, such as \
             `=> `, `U-Boot> `, or a board-specific CONFIG_SYS_PROMPT. If the transcript above \
             shows the board's prompt, name it.",
            self.prompt
        ))
    }
}

/// Check a boot order, and return the tidied form that will be set.
///
/// A boot order is words separated by spaces (`mmc0 mmc1 usb0 pxe dhcp`), and
/// anything else is refused. The refusal is a safety check, not tidiness, because
/// this value is formatted into a `setenv` line. A `;` or `&&` inside it would add
/// a second command on a path that no gate guards.
fn check_targets(targets: &str) -> Result<String> {
    let tidied = targets.split_whitespace().collect::<Vec<_>>().join(" ");
    if tidied.is_empty() {
        return Err(Error::InvalidRequest(
            "name the boot order to set, as in `mmc0` or `mmc0 usb0`".to_string(),
        ));
    }
    if let Some(bad) = tidied
        .chars()
        .find(|&c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ' '))
    {
        return Err(Error::InvalidRequest(format!(
            "a boot order is words separated by spaces, like `mmc0 usb0`, and {bad:?} cannot be \
             part of one"
        )));
    }
    Ok(tidied)
}

/// Read `name=value` out of what `printenv` printed.
///
/// U-Boot answers an unset variable with `## Error: "boot_targets" not defined`,
/// which carries no `name=`. That answer reads as [`None`]: the finding that this
/// build has no boot order at all, not a failure.
fn read_variable(printed: &[u8], name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let printed = text(printed);
    let at = printed.find(&needle)?;
    let rest = &printed[at + needle.len()..];
    let line = rest.split('\n').next().unwrap_or(rest);
    Some(line.trim().to_string())
}

/// Trim the line terminators a command's output is bracketed by.
///
/// One follows the echo, and one precedes the prompt. Neither is part of what the
/// command printed.
fn trim_ends(bytes: &[u8]) -> &[u8] {
    let is_end = |b: u8| b == b'\r' || b == b'\n';
    let mut from = 0;
    let mut to = bytes.len();
    while from < to && is_end(bytes[from]) {
        from += 1;
    }
    while to > from && is_end(bytes[to - 1]) {
        to -= 1;
    }
    &bytes[from..to]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    /// The bytes a console sends back for a typed `command`: the echo, the output,
    /// and the next prompt. A real U-Boot sends the same sequence, so a
    /// driver that misreads it fails here.
    fn echoed(command: &str, output: &str) -> SerialStep {
        SerialStep::Rx(format!("{command}\r\n{output}{DEFAULT_PROMPT}").into_bytes())
    }

    /// The line a driver types, as the scripted serial asserts it: the command and
    /// its terminator, exactly.
    fn typed(command: &str) -> SerialStep {
        SerialStep::ExpectTx(format!("{command}\r").into_bytes())
    }

    /// A board that is counting down. The bare line end stops the countdown. The
    /// board produced the banner first, so the driver answers it with a second line
    /// end and reaches the prompt.
    #[test]
    fn autoboot_is_interrupted_on_a_board_that_is_counting_down() {
        let steps = vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"Hit any key to stop autoboot:  2 ".to_vec()),
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        pollster::block_on(uboot.interrupt_autoboot(&mut |_| {}, &Cancel::new()))
            .expect("the countdown was interrupted");
    }

    /// A board already at a prompt never prints the banner, and the same call must
    /// work there. The driver therefore sends a line end. It submits an empty command
    /// line and gets another prompt, and does not become the first character of a
    /// command.
    #[test]
    fn autoboot_is_interrupted_on_a_board_already_at_a_prompt() {
        let steps = vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(b"\r\n=> ".to_vec()),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        pollster::block_on(uboot.interrupt_autoboot(&mut |_| {}, &Cancel::new()))
            .expect("an empty line brings the prompt back");
        // Nothing was left unread: one newline, one prompt.
        uboot.into_serial().assert_drained();
    }

    /// A command's output is what lay between the echo and the next prompt. It
    /// excludes the command, the prompt, and the line terminators on either side.
    #[test]
    fn a_command_returns_what_lay_between_its_echo_and_the_prompt() {
        let steps = vec![
            typed("version"),
            echoed("version", "U-Boot 2026.04 (Aug 22 2026 - 10:14:03)\r\n"),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let output =
            pollster::block_on(uboot.run("version", &mut |_| {}, &Cancel::new())).expect("it ran");
        assert_eq!(output, b"U-Boot 2026.04 (Aug 22 2026 - 10:14:03)");
    }

    /// The cursor, end to end. Of two commands in a row, the second must read its
    /// own output. It must not find the first command's prompt in the transcript
    /// and return at once.
    #[test]
    fn a_second_command_reads_its_own_output_and_not_the_first_prompt() {
        let steps = vec![
            typed("printenv boot_targets"),
            echoed("printenv boot_targets", "boot_targets=mmc0 usb0\r\n"),
            typed("version"),
            echoed("version", "U-Boot 2026.04\r\n"),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let cancel = Cancel::new();
        let (first, second) = pollster::block_on(async {
            let first = uboot
                .run("printenv boot_targets", &mut |_| {}, &cancel)
                .await?;
            let second = uboot.run("version", &mut |_| {}, &cancel).await?;
            Ok::<_, Error>((first, second))
        })
        .expect("both ran");
        assert_eq!(first, b"boot_targets=mmc0 usb0");
        assert_eq!(second, b"U-Boot 2026.04");
    }

    /// A far end that does not echo leaves the driver no way to tell a command from
    /// its answer. The error names the missing echo, and does not report a missing
    /// prompt.
    #[test]
    fn a_console_that_does_not_echo_is_named_as_the_problem() {
        let steps = vec![
            typed("version"),
            SerialStep::Rx(b"U-Boot 2026.04\r\n=> ".to_vec()),
            SerialStep::Timeout,
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps)).with_reads(2);
        let result = pollster::block_on(uboot.run("version", &mut |_| {}, &Cancel::new()));
        let Err(Error::Protocol(message)) = result else {
            panic!("a console that does not echo is a protocol failure: {result:?}");
        };
        assert!(message.contains("did not echo"), "{message}");
        assert!(message.contains("U-Boot 2026.04"), "{message}");
    }

    /// The plan asks the board rather than assuming, and reports the board's answer
    /// beside what it would set. Planning writes nothing.
    #[test]
    fn the_plan_reports_the_order_the_board_believes_now() {
        let steps = vec![
            typed("printenv boot_targets"),
            echoed(
                "printenv boot_targets",
                "boot_targets=mmc1 mmc0 usb0 pxe dhcp\r\n",
            ),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let plan =
            pollster::block_on(uboot.plan_boot_override("mmc0", &mut |_| {}, &Cancel::new()))
                .expect("the board answered");
        assert_eq!(plan.current.as_deref(), Some("mmc1 mmc0 usb0 pxe dhcp"));
        assert_eq!(plan.targets, "mmc0");
        assert!(!plan.persistent, "the environment stays in RAM");
        assert!(!plan.is_no_change());
    }

    /// A build with no `boot_targets` is a finding, not a failure. U-Boot reports
    /// the missing variable in prose, and the plan reports that nothing is set. It
    /// does not invent an order.
    #[test]
    fn a_board_with_no_boot_order_reports_none_rather_than_failing() {
        let steps = vec![
            typed("printenv boot_targets"),
            echoed(
                "printenv boot_targets",
                "## Error: \"boot_targets\" not defined\r\n",
            ),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let plan =
            pollster::block_on(uboot.plan_boot_override("mmc0", &mut |_| {}, &Cancel::new()))
                .expect("the board answered");
        assert_eq!(plan.current, None);
    }

    /// **The override sends `setenv` and `boot`, and nothing else.** The scripted
    /// serial asserts every byte written. A `saveenv` added to this path would make
    /// a volatile change permanent, and fails here. This test pins the property the
    /// module states: there is no `saveenv` operation, and no path composes one.
    #[test]
    fn the_override_sets_the_order_and_boots_without_saving_it() {
        let steps = vec![
            typed("printenv boot_targets"),
            echoed("printenv boot_targets", "boot_targets=mmc1 usb0\r\n"),
            typed("setenv boot_targets mmc0"),
            echoed("setenv boot_targets mmc0", ""),
            typed("boot"),
            SerialStep::Rx(b"boot\r\nstarting USB...".to_vec()),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let cancel = Cancel::new();
        pollster::block_on(async {
            let plan = uboot
                .plan_boot_override("mmc0", &mut |_| {}, &cancel)
                .await?;
            uboot
                .boot_override(plan.confirm(), &mut |_| {}, &cancel)
                .await
        })
        .expect("the override went out");
        uboot.into_serial().assert_drained();
    }

    /// A boot order is words and spaces. Anything else would be carried into the
    /// `setenv` line this composes. The refusal comes before the board is asked
    /// anything.
    #[test]
    fn a_boot_order_carrying_a_second_command_is_refused() {
        for bad in ["mmc0; saveenv", "mmc0 && saveenv", "", "   "] {
            let mut uboot = UBoot::new(ScriptedSerial::new(vec![]));
            let result =
                pollster::block_on(uboot.plan_boot_override(bad, &mut |_| {}, &Cancel::new()));
            assert!(
                matches!(result, Err(Error::InvalidRequest(_))),
                "{bad:?} was not refused: {result:?}"
            );
        }
    }

    /// A running gadget never returns the prompt, so a wait for the prompt that
    /// runs out is the success signal.
    #[test]
    fn a_gadget_that_holds_the_console_is_reported_as_started() {
        let steps = vec![
            typed("rockusb 0 mmc 0"),
            SerialStep::Rx(b"rockusb 0 mmc 0\r\n".to_vec()),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let command = pollster::block_on(uboot.start_gadget(
            Gadget::Rockusb,
            &GadgetDevice::default(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the gadget has the console");
        assert_eq!(command, "rockusb 0 mmc 0");
    }

    /// Mass storage starts and succeeds the same way. `ums` prints its LUN line and
    /// keeps the console.
    #[test]
    fn a_mass_storage_gadget_is_started_by_its_own_command() {
        let steps = vec![
            typed("ums 0 mmc 0"),
            SerialStep::Rx(
                b"ums 0 mmc 0\r\nUMS: LUN 0, dev mmc 0, hwpart 0, sector 0x0, count 0xe8f8000\r\n"
                    .to_vec(),
            ),
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Timeout,
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let command = pollster::block_on(uboot.start_gadget(
            Gadget::Ums,
            &GadgetDevice::default(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the gadget has the console");
        assert_eq!(command, "ums 0 mmc 0");
        uboot.into_serial().assert_drained();
    }

    /// A prompt that comes back means the command returned, and a gadget command
    /// that returns has failed. The error carries what U-Boot printed.
    #[test]
    fn a_gadget_command_that_returns_is_a_failure_carrying_what_u_boot_said() {
        let steps = vec![
            typed("rockusb 0 mmc 0"),
            echoed(
                "rockusb 0 mmc 0",
                "Unknown command 'rockusb' - try 'help'\r\n",
            ),
        ];
        let mut uboot = UBoot::new(ScriptedSerial::new(steps));
        let result = pollster::block_on(uboot.start_gadget(
            Gadget::Rockusb,
            &GadgetDevice::default(),
            &mut |_| {},
            &Cancel::new(),
        ));
        let Err(Error::Protocol(message)) = result else {
            panic!("a gadget that did not start is a protocol failure: {result:?}");
        };
        assert!(message.contains("Unknown command 'rockusb'"), "{message}");
    }

    /// A caller writes a device spec in the two forms U-Boot's own command takes.
    /// An interface that is not a word is refused, because it is formatted into a
    /// command line.
    #[test]
    fn a_gadget_device_reads_the_forms_it_says_it_does() {
        assert_eq!(
            GadgetDevice::parse("mmc:0")
                .unwrap()
                .command(Gadget::Rockusb),
            "rockusb 0 mmc 0"
        );
        assert_eq!(
            GadgetDevice::parse("1:nvme:2")
                .unwrap()
                .command(Gadget::Ums),
            "ums 1 nvme 2"
        );
        assert_eq!(
            GadgetDevice::default().command(Gadget::Ums),
            "ums 0 mmc 0",
            "the default is the line the maskrom guide types"
        );
        assert!(GadgetDevice::parse("mmc").is_err());
        assert!(GadgetDevice::parse("mmc 0; saveenv:0").is_err());
        assert!(GadgetDevice::parse("mmc:zero").is_err());
    }

    /// A gadget is named by the command that starts it, and nothing else is one.
    #[test]
    fn a_gadget_is_read_by_its_command_word() {
        for gadget in Gadget::ALL {
            assert_eq!(Gadget::parse(gadget.command_word()).unwrap(), gadget);
        }
        for bad in ["", "dfu", "Rockusb", "ums;saveenv"] {
            assert!(
                matches!(Gadget::parse(bad), Err(Error::InvalidRequest(_))),
                "{bad:?} was read as a gadget"
            );
        }
    }

    /// An empty command only asks for another prompt, which is not what a caller
    /// running a command intends.
    #[test]
    fn an_empty_command_is_refused() {
        let mut uboot = UBoot::new(ScriptedSerial::new(vec![]));
        let result = pollster::block_on(uboot.run("  ", &mut |_| {}, &Cancel::new()));
        assert!(
            matches!(result, Err(Error::InvalidRequest(_))),
            "{result:?}"
        );
    }
}
