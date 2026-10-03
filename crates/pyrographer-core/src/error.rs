//! Error and result types.
//!
//! Errors carry enough structure for a caller to react, and an optional
//! [`hint`](Error::hint): an actionable next step in prose. Core never prints, so
//! the hint is returned as data. The CLI writes it after the error, and the GUI
//! shows it in a callout.
//!
//! Each variant names one thing that went wrong, and every variant has a caller
//! that raises it. A distinction the code does not draw has no variant. The
//! variant is added with the code that draws the distinction.

use std::fmt;
use std::time::Duration;

/// The result type used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// An error from a pyrographer operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No device matching the request was found.
    DeviceNotFound,

    /// The device was found, but the operating system denied access to it.
    AccessDenied {
        /// The device's USB vendor ID.
        vendor_id: u16,
        /// The device's USB product ID.
        product_id: u16,
    },

    /// The device is held by another process or by a kernel driver.
    Busy(String),

    /// The device left the bus.
    Disconnected,

    /// The device did not answer a transfer before the host stopped waiting.
    ///
    /// The host cannot tell a wedged device from a slow one, except by how long it
    /// waits. This error means the host's deadline expired, and the device reported
    /// no fault. What the device does next is unknown, so the agent that hit the
    /// timeout is [`Desynchronized`](Error::Desynchronized) from then on.
    Timeout {
        /// What was being moved: `"bulk IN"`, `"control OUT"`.
        what: &'static str,
        /// How long the host waited.
        waited: Duration,
    },

    /// A failure from the underlying USB or serial transport.
    Transport(String),

    /// A protocol frame or response was malformed, or was well-formed and left the
    /// exchange with no valid next step.
    ///
    /// One example is a device answering a DFU `GETSTATUS` with a state a download
    /// never leaves. Another is a device answering with the same busy state until
    /// the poll budget runs out.
    Protocol(String),

    /// The host can no longer tell where the device is in the conversation, so
    /// the agent that raised this refuses every further command.
    ///
    /// A command is several transfers, and a failure partway through can leave the
    /// device with bytes still to send. The next command would read them as its own
    /// reply and misdiagnose the device. An agent that is desynchronized therefore
    /// stays desynchronized until the device is reopened.
    Desynchronized,

    /// The device is not in a mode that can serve the requested operation.
    WrongMode {
        /// The mode the device is in.
        found: &'static str,
        /// The mode the operation needs.
        needed: &'static str,
    },

    /// The device accepted a command and reported that it failed.
    CommandFailed {
        /// The command that failed.
        command: &'static str,
        /// The device's `bCSWStatus` byte.
        status: u8,
    },

    /// The flash differs from the image.
    ///
    /// The conversation with the device is intact. The flash and the image
    /// disagree, which is the outcome a read-back exists to find. It is therefore
    /// not a [`Protocol`](Error::Protocol) error.
    VerifyMismatch {
        /// Where the first difference is, in bytes from the start of the range
        /// that was compared.
        offset: u64,
        /// The byte the flash holds.
        found: u8,
        /// The byte the image holds.
        expected: u8,
    },

    /// A write checked only after it was committed read back different from what
    /// was sent.
    ///
    /// It is [`VerifyMismatch`](Error::VerifyMismatch)'s counterpart for a backend
    /// whose protocol serves no read while a write is in flight, as
    /// [`ReadBack::AfterCommit`](crate::agent::ReadBack::AfterCommit) describes. It
    /// names the window, not the byte. The check compares a digest of what was sent
    /// with a digest of what came back. The image was streamed once and never held,
    /// so no expected bytes remain to point at. The window says where to look.
    ///
    /// The region **was written**. A per-window mismatch stops before the next
    /// window goes out, but this one is found after the whole region is committed.
    /// The flash holds something other than what was intended, and the region must
    /// be written again.
    CommitMismatch {
        /// Where the first differing window starts, in bytes from the start of the
        /// range that was written.
        offset: u64,
        /// How many bytes that window covers.
        window_bytes: u64,
    },

    /// The wrong-loader gate's refusal: asked `K_FW_GET_CHIP_VER`, the loader
    /// answered as something other than the SoC the write was planned for.
    ///
    /// The loader that answered is the loader that would have done the writing. A
    /// loader for the wrong SoC writes to the wrong offsets, and reports a plausible
    /// status for every command. Nothing was written. The gate compares the answer
    /// with the whole reply pinned for the named SoC, byte for byte. It compares
    /// measured bytes and decodes no meaning from them, as [`Soc`](crate::soc::Soc)
    /// explains.
    LoaderMismatch {
        /// The SoC the write was planned for, canonical: `"rk3576"`.
        named: &'static str,
        /// The reply pinned for that SoC, whole.
        expected: Vec<u8>,
        /// What this loader answered, whole.
        answered: Vec<u8>,
    },

    /// The loader file names a different SoC from the one the upload was aimed at.
    ///
    /// It is [`LoaderMismatch`](Error::LoaderMismatch)'s counterpart one stage
    /// earlier, and the only gate the maskrom download-boot can have. Nothing was
    /// uploaded. A maskrom board answers no chip-version query, and its SRAM cannot
    /// be read back. Once the wrong loader is running, nothing is left to compare.
    /// The check is therefore made against the container's own claim, before a byte
    /// goes out.
    ///
    /// This gate is weaker than the loader gate. Whoever built the file wrote the
    /// claim. It catches the wrong file picked from a disk that holds several, and
    /// it is not proof of what the blob does.
    LoaderBlobMismatch {
        /// The SoC the upload was aimed at, canonical: `"rk3576"`.
        named: &'static str,
        /// The chip field pinned for that SoC.
        expected: Vec<u8>,
        /// What this container's chip field actually holds.
        found: Vec<u8>,
    },

    /// A partition table was found on the flash, and it fails its own CRC or
    /// structural validation.
    ///
    /// The conversation with the device is intact, and the content of the flash is
    /// wrong. It is therefore not a [`Protocol`](Error::Protocol) error.
    ///
    /// It is also distinct from a device with no table at all. A device with no
    /// table is a finding a caller can act on. A device whose table has a signature
    /// but a CRC that disagrees with it has damage. Reporting that as "no
    /// partitions" would hide it behind an empty list.
    CorruptTable {
        /// The format the table announced itself as: `"GPT"`, `"Rockchip
        /// parameter"`.
        format: &'static str,
        /// The check the table failed, and the values it compared.
        detail: String,
    },

    /// The device has a partition table, and it has no partition by that name.
    NoSuchPartition {
        /// The name that was asked for.
        wanted: String,
        /// The names the table does have, in the order the table lists them.
        available: Vec<String>,
    },

    /// The caller asked for something the backend cannot address.
    ///
    /// Examples are a read that is not a whole number of sectors, and a range that
    /// runs past the last sector the protocol can name. Nothing malfunctioned, and
    /// nothing was sent. The request itself is wrong, and only the caller can fix
    /// it.
    InvalidRequest(String),

    /// A failure reading or writing the local file or buffer an operation streams
    /// to.
    Io(String),

    /// The host tool itself failed: the task driving an operation died before
    /// it reported an outcome.
    ///
    /// The fault is pyrographer's own, so it says nothing about the device or the
    /// request. The GUI raises it for a worker task that panics. A task that dies
    /// silently leaves its button spinning forever, and a reported fault can be
    /// acted on.
    Internal(String),

    /// The caller canceled the operation.
    Canceled,

    /// The operation is recognized but not yet implemented.
    NotImplemented(&'static str),
}

impl Error {
    /// An actionable next step for this failure, or `None`.
    ///
    /// A hint is prose for a person, built from the error's own fields. The udev
    /// rule an [`AccessDenied`](Error::AccessDenied) hint prescribes names the
    /// vendor ID the device itself reported. The advice is therefore correct for
    /// every vendor pyrographer scans for.
    ///
    /// The USB enumeration advice rests on how a boot-mode board behaves. A board
    /// in a boot mode is always a USB-2 gadget, driven by a small USB stack in its
    /// on-chip boot code. A modern all-xHCI host handles it differently from the
    /// EHCI controllers those stacks were validated against. A powered USB-2 hub
    /// inline is the fix that works most often.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::AccessDenied { vendor_id, .. } => Some(format!(
                "the operating system denied access to the device.\n\
                 On Linux, grant access with a udev rule covering its vendor ID:\n\
                 \n    \
                 SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{vendor_id:04x}\", MODE=\"0660\", \
                 TAG+=\"uaccess\"\n\
                 \n\
                 Write it to /etc/udev/rules.d/99-pyrographer.rules, then reload and replug:\n\
                 \n    \
                 sudo udevadm control --reload-rules && sudo udevadm trigger\n\
                 \n\
                 A vendor-wide rule also covers SoCs that do not exist yet, and a per-product-ID \
                 rule does not. Running pyrographer with sudo also works."
            )),
            Error::Desynchronized => Some(
                "the host can no longer tell where the device is in the protocol exchange, so \
                 this connection refuses every further command. Reopen the device. If reopening \
                 does not clear the error, replug the board."
                    .to_string(),
            ),
            Error::CorruptTable { format, .. } => Some(format!(
                "pyrographer cannot determine the partition layout of this device, so operations \
                 that name a partition cannot run. Operations that take a raw LBA (dump, flash, \
                 and verify) do not read the table and still work. A write through them is \
                 planned against the flash geometry alone, so its plan cannot show which \
                 partition the write lands in.\n\
                 \n\
                 A {format} keeps more than one copy, so a repair can rewrite a damaged copy from \
                 an intact one. A repair refuses, and says why, when no intact copy remains. In \
                 that case, author a fresh table from a layout you supply. Dump the device first, \
                 so the damaged table is preserved."
            )),
            Error::LoaderBlobMismatch { named, .. } => Some(format!(
                "check which loader file was chosen. A {named} board needs a {named} loader, and \
                 this file names a different SoC. If the file is the right one, its container was \
                 built with a different chip name, and the pinned bytes need review. Report what \
                 the file holds."
            )),
            Error::LoaderMismatch { named, .. } => Some(format!(
                "either this board is not a {named}, or the loader running on it is not a \
                 {named} loader. First check which board is connected. Then read the loader's \
                 chip version to see what it answers. If the board is a {named}, that reply comes \
                 from a loader build that is not pinned yet. Report the reply."
            )),
            // `found: "maskrom"` is raised only by the open-time probe: the flag
            // claimed maskrom, and nothing answered TEST_UNIT_READY on the bulk
            // endpoints. A BootROM presents the same endpoints a loader does,
            // with nothing behind them, so this is what a maskrom board looks
            // like from the probe's side.
            Error::WrongMode {
                found: "maskrom", ..
            } => Some(
                "the mode flag reports maskrom, and nothing answered a rockusb probe on the bulk \
                 endpoints. A maskrom board behaves this way: the BootROM presents the same \
                 endpoints a loader does, but nothing serves them. Upload a loader to bring the \
                 board to loader mode. If a loader is running, the failed probe points to the \
                 USB path instead. A powered USB-2 hub inline is the fix that works most often."
                    .to_string(),
            ),
            Error::Busy(_) => Some(
                "another process or a kernel driver holds the device. Close any other flashing \
                 tool, then replug the board."
                    .to_string(),
            ),
            Error::Disconnected | Error::Transport(_) | Error::Timeout { .. } => Some(
                "the device stopped answering.\n\
                 \n\
                 If it connects over USB: a board in a boot mode is a USB-2 gadget with a minimal \
                 BootROM USB stack. Modern hosts use xHCI, which drives those stacks differently \
                 from the EHCI controllers they were validated against. Try these fixes in order, \
                 most effective first. Put a powered USB-2 hub between the PC and the board. Use \
                 a known-good USB-2 data cable. Use a rear-panel port rather than a front-panel \
                 port, a dock, or a hub with an active retimer.\n\
                 \n\
                 If it connects over a serial line (StarFive recovery): check that the board is \
                 strapped into UART recovery and powered on. Check that TX, RX, and GND are \
                 wired, and that TX and RX are not swapped. Check that the adapter is set to \
                 115200 8N1."
                    .to_string(),
            ),
            Error::Internal(_) => Some(
                "this is a fault in pyrographer itself, not in the device or the request. The \
                 details are printed to the terminal pyrographer was started from, if any. Start \
                 pyrographer from a terminal to capture them, and include them in a report."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DeviceNotFound => write!(f, "no matching device found"),
            Error::AccessDenied {
                vendor_id,
                product_id,
            } => write!(f, "access denied opening {vendor_id:04x}:{product_id:04x}"),
            Error::Busy(m) => write!(f, "device is busy: {m}"),
            Error::Disconnected => write!(f, "device left the bus"),
            Error::Timeout { what, waited } => write!(
                f,
                "the device did not answer {what} within {:.1}s",
                waited.as_secs_f64()
            ),
            Error::Transport(m) => write!(f, "transport error: {m}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Desynchronized => {
                write!(f, "the device and the host are no longer synchronized")
            }
            Error::WrongMode { found, needed } => {
                write!(
                    f,
                    "device is in {found} mode, and this operation needs {needed} mode"
                )
            }
            Error::CommandFailed { command, status } => {
                write!(f, "device reported {command} failed (status {status})")
            }
            Error::VerifyMismatch {
                offset,
                found,
                expected,
            } => write!(
                f,
                "flash differs from the image at byte {offset}: read {found:#04x}, expected \
                 {expected:#04x}"
            ),
            Error::CommitMismatch {
                offset,
                window_bytes,
            } => write!(
                f,
                "the flash differs from what was written, in the {window_bytes}-byte window at \
                 byte {offset}. This backend can be read back only after a write is committed, so \
                 the difference was found after the region was written. The region does not hold \
                 the image and must be written again"
            ),
            Error::LoaderMismatch {
                named,
                expected,
                answered,
            } => write!(
                f,
                "the loader does not match {named}, the SoC this write was planned for. Loaders \
                 for {named} answer {}, and this loader answered {}. A loader for the wrong SoC \
                 writes to the wrong offsets, so nothing was written",
                bytes_for_reading(expected),
                bytes_for_reading(answered)
            ),
            Error::LoaderBlobMismatch {
                named,
                expected,
                found,
            } => write!(
                f,
                "this loader file is not for {named}, the SoC the upload names. Containers built \
                 for {named} carry {}, and this file's chip field holds {}. A loader built for \
                 another SoC would run the wrong code in SRAM without reporting an error, so \
                 nothing was uploaded",
                bytes_for_reading(expected),
                bytes_for_reading(found)
            ),
            Error::CorruptTable { format, detail } => write!(
                f,
                "the {format} partition table on this device is damaged: {detail}"
            ),
            Error::NoSuchPartition { wanted, available } => {
                write!(f, "this device has no partition named '{wanted}'")?;
                if available.is_empty() {
                    write!(f, ". Its table lists no partitions")
                } else {
                    write!(f, ". Its partitions are: {}", available.join(", "))
                }
            }
            Error::InvalidRequest(m) => write!(f, "invalid request: {m}"),
            Error::Io(m) => write!(f, "I/O error: {m}"),
            Error::Internal(m) => write!(f, "internal fault: {m}"),
            Error::Canceled => write!(f, "canceled"),
            Error::NotImplemented(m) => write!(f, "not yet implemented: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// A byte string as a person reads one: hex pairs, then the printable ASCII.
///
/// It matches the format `chipver` prints, because [`Error::LoaderMismatch`] asks
/// a person to compare its bytes with that command's output.
fn bytes_for_reading(bytes: &[u8]) -> String {
    let hex = bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{hex} (\"{ascii}\")")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint carries the vendor ID from the error that raised it. A rule naming
    /// the wrong vendor looks correct and grants access to nothing.
    #[test]
    fn the_access_denied_hint_prescribes_a_rule_for_the_vendor_that_was_denied() {
        let rockchip = Error::AccessDenied {
            vendor_id: 0x2207,
            product_id: 0x350e,
        };
        let hint = rockchip.hint().expect("access denied carries a hint");
        assert!(hint.contains("2207"), "{hint}");

        // The vendor Ingenic support will bring. Nothing about the hint is
        // Rockchip-shaped, so it needs no change when that lands.
        let ingenic = Error::AccessDenied {
            vendor_id: 0xa108,
            product_id: 0x4d44,
        };
        let hint = ingenic.hint().expect("access denied carries a hint");
        assert!(hint.contains("a108"), "{hint}");
        assert!(
            !hint.contains("2207"),
            "an Ingenic device was told to write a Rockchip rule: {hint}"
        );
    }

    /// A timeout gets the same advice as any device that stopped answering,
    /// whatever made it stop. That advice addresses this failure: a BootROM USB
    /// stack on a host that drives it differently from the host it was validated
    /// against.
    #[test]
    fn a_timeout_is_told_what_a_device_that_stopped_answering_is_told() {
        let timeout = Error::Timeout {
            what: "bulk IN",
            waited: Duration::from_secs(10),
        };

        let hint = timeout.hint().expect("a device gone quiet has a next step");
        assert!(hint.contains("powered USB-2 hub"), "{hint}");
        assert!(timeout.to_string().contains("bulk IN"), "{timeout}");

        // And it is not a cancellation: nobody asked for this.
        assert!(Error::Canceled.hint().is_none());
    }

    /// A desynchronized agent refuses every further command, and only a fresh
    /// connection recovers. Reopening is a next step, so the error carries it as a
    /// hint.
    #[test]
    fn a_desynchronized_agent_is_told_to_reopen_the_device() {
        let hint = Error::Desynchronized
            .hint()
            .expect("falling out of step has a way out");
        assert!(hint.contains("Reopen the device"), "{hint}");
    }

    /// A hint is a next step, not a restatement of the error. Only the caller
    /// knows what to do about a request it got wrong, or an image the flash
    /// disagrees with. These errors therefore carry no hint.
    #[test]
    fn a_failure_with_no_next_step_offers_none() {
        assert!(Error::Canceled.hint().is_none());
        assert!(Error::NotImplemented("rockusb write").hint().is_none());
        assert!(
            Error::InvalidRequest("not a whole sector".to_string())
                .hint()
                .is_none()
        );
        assert!(
            Error::VerifyMismatch {
                offset: 0,
                found: 0x00,
                expected: 0xff,
            }
            .hint()
            .is_none()
        );
    }
}
