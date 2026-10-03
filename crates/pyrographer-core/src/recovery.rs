//! StarFive JH7110 UART recovery: the `recover` verb and its serial driver.
//!
//! This is the outlier among the backends. Every other backend that drives a
//! board reaches it over USB. The JH7110 BootROM has no USB at all, and recovers over a **serial
//! line** as an XMODEM receiver. This backend therefore consumes the [`Serial`] seam,
//! not [`Transport`](crate::transport::Transport). It has no
//! [`FlashAgent`](crate::agent::FlashAgent) and none of the uniform block verbs.
//! Its capability is **write-only and bootloader-scoped**, so it has a verb of its
//! own, [`recover`], in place of the uniform verbs.
//!
//! # The recovery-agent flow
//!
//! pyrographer drives StarFive's official recovery path, through the recovery
//! agent:
//!
//! 1. XMODEM the recovery agent (`jh7110-recovery-*.bin`, pre-headered) into the
//!    ROM. It runs from SRAM and presents a menu.
//! 2. For each thing to write, type the menu digit, then XMODEM the file. The
//!    digits are SPL to QSPI NOR (`0`) or eMMC (`1`), and U-Boot to NOR (`2`) or
//!    eMMC (`3`).
//! 3. Exit the menu (`5`).
//!
//! **Menu option `4`, OTP fuse burning, is never offered.** It is irreversible, and
//! outside a flashing tool's mandate. [`menu_option`] cannot produce a `4`, so no
//! path through this module reaches it.
//!
//! The SPL is sent as a `.normal.out`. This module builds the 1024-byte [`splhdr`]
//! header for the chosen medium, because the eMMC variant differs from the flash
//! one. Its header carries a deliberately wrong CRC, so the ROM falls through to the
//! backup copy. The recovery agent is already headered, and the U-Boot FIT payload
//! is already an image, so both are sent as they are.
//!
//! # No read-back
//!
//! Every other write in pyrographer reads back every window it writes, and the
//! write path is built on that invariant. **The StarFive recovery protocol cannot
//! read flash at all.** The ROM in recovery is a pure XMODEM *receiver*. Every menu
//! option of the closed-source agent is an "update", with no read, dump or verify.
//! The per-block XMODEM `ACK` confirms the *transfer*, not the *stored contents*.
//!
//! This backend therefore cannot keep the read-back invariant, and says so.
//! [`caps`] reports `can_verify: false`, and the [`RecoveryPlan`] a person confirms
//! states it again. A StarFive recovery is unverified in a way a Rockchip write is
//! not.
//!
//! # Tests without a board
//!
//! The XMODEM framing is a sans-I/O codec, [`xmodem`]. The sender state machine
//! here consumes [`Serial`]. Tests pin it against a scripted serial that models the
//! ROM, as a scripted transport pins [`bootstrap`](crate::bootstrap). The state
//! machine covers the `C` handshake and per-block `ACK`, resending on `NAK`, and
//! tolerating the ROM's `NAK` bursts.
//!
//! These are **\[UNVERIFIED\]** until a board settles them:
//!
//! - Whether the ROM's real `C`, `ACK` and `NAK` timing matches these assumptions
//! - Whether the closed agent transfers each file by XMODEM
//! - Whether it accepts 1 KB blocks
//! - Whether the menu-digit handshake needs a trailing newline
//!
//! [`Serial`]: crate::transport::Serial

use crate::codec::{splhdr, xmodem};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::transport::Serial;
use crate::{Error, Result};

/// The menu digit that exits the recovery agent.
const MENU_EXIT: u8 = 5;

/// How many reads to spend waiting for the receiver's `C` before giving up.
///
/// Each read carries the [`Serial`] transport's own deadline, so this bounds a
/// handshake against a receiver that never asks for CRC mode. Such a receiver is a
/// board that is not in UART recovery, or not powered into the strap. Without the
/// bound, the sender would read forever.
const HANDSHAKE_READS: u32 = 64;

/// How many resends of one block (or the `EOT`) before the transfer is declared
/// failed.
///
/// This ROM's XMODEM is known to over-`NAK`, so a low cap would abort a transfer
/// that a patient sender would complete. A cap is still required. Retrying forever
/// into a receiver that never accepts the block makes a recovery hang instead of
/// fail. A stuck transfer is reported as a failure, and the sender does not retry
/// into a brick.
const MAX_RESENDS: u32 = 20;

/// How many reads to spend waiting for an `ACK`/`NAK` before treating the silence
/// as a retransmit trigger. Bounds a wait against a receiver that has gone quiet.
const ACK_READS: u32 = 32;

/// Which boot medium a recovery writes to.
///
/// The medium selects both the agent menu option and the SPL header variant. Both
/// derive from this one value, so they cannot disagree. An SPL headered for flash
/// but sent to the eMMC slot, or the reverse, can brick a board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryTarget {
    /// QSPI NOR flash. SPL to menu option `0`, U-Boot to `2`.
    NorFlash,
    /// eMMC. SPL to menu option `1`, U-Boot to `3`.
    Emmc,
}

impl From<RecoveryTarget> for splhdr::Target {
    fn from(target: RecoveryTarget) -> Self {
        match target {
            RecoveryTarget::NorFlash => splhdr::Target::NorFlash,
            RecoveryTarget::Emmc => splhdr::Target::Emmc,
        }
    }
}

/// Which object a recovery stage writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    /// The second-stage program loader (`u-boot-spl.bin`), headered per target.
    Spl,
    /// The U-Boot FIT payload (`visionfive2_fw_payload.img` / `u-boot.itb`), as-is.
    UBoot,
}

impl StageKind {
    /// A label for a message a person reads.
    pub fn name(self) -> &'static str {
        match self {
            StageKind::Spl => "SPL",
            StageKind::UBoot => "U-Boot",
        }
    }
}

/// The recovery agent's menu digit for writing `kind` to `target`.
///
/// This mapping is the only place a write's menu digit is produced, and it produces
/// only `0`..=`3`. No `(kind, target)` pair names option `4` (OTP). The exit option,
/// `5`, is a separate constant the driver sends to leave the menu. OTP is therefore
/// unreachable by construction, with no guard that a later change could remove.
pub fn menu_option(kind: StageKind, target: RecoveryTarget) -> u8 {
    match (kind, target) {
        (StageKind::Spl, RecoveryTarget::NorFlash) => 0,
        (StageKind::Spl, RecoveryTarget::Emmc) => 1,
        (StageKind::UBoot, RecoveryTarget::NorFlash) => 2,
        (StageKind::UBoot, RecoveryTarget::Emmc) => 3,
    }
}

/// What a StarFive recovery backend can and cannot do.
///
/// It is the serial-recovery counterpart of [`Caps`](crate::agent::Caps), and is
/// kept separate from it on purpose. [`Caps`](crate::agent::Caps) describes a
/// block backend, with erase, partitions and a medium, and StarFive recovery is not
/// one. Adding `can_verify` to it would make every block backend answer a question
/// that only this backend raises. The capability therefore lives here, with the
/// backend it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryCaps {
    /// Whether a write can be read back and verified.
    ///
    /// Always `false`. The recovery protocol has no read path, so a write is
    /// confirmed only by the per-block XMODEM `ACK`. The `ACK` guarantees that the
    /// bytes were *received*, not that the flash *holds* them. A front-end shows
    /// this field, so a person knows a StarFive recovery is unverified in a way a
    /// Rockchip write is not.
    pub can_verify: bool,
}

/// What a StarFive recovery backend supports.
pub fn caps() -> RecoveryCaps {
    RecoveryCaps { can_verify: false }
}

/// Everything a recovery would write, borrowed for the length of the call.
///
/// The agent is always sent first. At least one of [`spl`](Self::spl) or
/// [`uboot`](Self::uboot) must be present, and [`plan_recover`] refuses a request
/// with neither. With both present, the SPL is written before U-Boot, in boot-chain
/// order. Both go to the one [`target`](Self::target).
pub struct RecoveryRequest<'a> {
    /// The boot medium both stages are written to.
    pub target: RecoveryTarget,
    /// The recovery agent (`jh7110-recovery-*.bin`), pre-headered, sent as-is.
    pub agent: &'a [u8],
    /// The raw `u-boot-spl.bin` body. This module builds its `.normal.out` header
    /// for [`target`](Self::target), and the caller supplies the unheadered SPL.
    /// [`plan_recover`] refuses an SPL that already carries a header.
    pub spl: Option<&'a [u8]>,
    /// The U-Boot FIT payload, already an image, sent as-is.
    pub uboot: Option<&'a [u8]>,
}

/// One stage a recovery would carry out, as the plan describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedStage {
    /// What this stage writes.
    pub kind: StageKind,
    /// The agent menu digit it is sent under.
    pub menu_option: u8,
    /// How many payload bytes XMODEM will send. For an SPL, this includes the
    /// 1024-byte header this module prepends, so it is not the size of the file on
    /// disk. XMODEM's framing and the padding in its last block are not counted.
    pub image_bytes: u64,
}

/// What a recovery would do, in full, before any of it happens.
///
/// It is the StarFive counterpart of [`WritePlan`](crate::verbs::WritePlan), and
/// gated the same way. [`plan_recover`] produces it and sends nothing. A caller
/// renders it for a person to read. Only [`RecoveryPlan::confirm`] turns it into
/// the [`ConfirmedRecovery`] that [`recover`] accepts. Producing a plan and stopping
/// there is the dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// The boot medium being written.
    pub target: RecoveryTarget,
    /// How many bytes the recovery agent upload will send.
    pub agent_bytes: u64,
    /// The stages, in the order they will be carried out.
    pub stages: Vec<PlannedStage>,
    /// Whether the write is read back and verified. **Always `false`** for
    /// StarFive, because the protocol cannot read flash. It is a field of the plan,
    /// as well as of [`RecoveryCaps`], so that a front-end shows it to the person
    /// confirming the recovery. [`WritePlan`](crate::verbs::WritePlan) carries the
    /// loader's own account of the SoC for the same reason.
    pub verified: bool,
}

impl RecoveryPlan {
    /// Say yes to this plan.
    ///
    /// It consumes the plan, so one confirmation authorizes one recovery and cannot
    /// be replayed.
    pub fn confirm(self) -> ConfirmedRecovery {
        ConfirmedRecovery(self)
    }
}

/// A [`RecoveryPlan`] a caller has agreed to, and the only thing [`recover`]
/// takes.
///
/// It holds consent as a value, produced from a plan the caller has seen. It has
/// the same shape as [`ConfirmedWrite`](crate::verbs::ConfirmedWrite).
// Not `Clone`: one confirmation, one recovery -- see
// [`ConfirmedWrite`](crate::verbs::ConfirmedWrite).
#[derive(Debug)]
pub struct ConfirmedRecovery(RecoveryPlan);

impl ConfirmedRecovery {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &RecoveryPlan {
        &self.0
    }
}

/// Plan a recovery: the dry run.
///
/// It is pure and touches no serial line, because a serial receiver in recovery
/// has nothing to report. It cannot report geometry or a partition table, and it
/// cannot be read. The plan therefore describes what the caller asked for, and says
/// that the write will not be verified. It makes two refusals up front: an SPL that
/// already carries a header, and a recovery that would write neither an SPL nor a
/// U-Boot.
pub fn plan_recover(request: &RecoveryRequest<'_>) -> Result<RecoveryPlan> {
    let mut stages = Vec::new();

    if let Some(spl) = request.spl {
        // **Refuse a pre-headered SPL.** StarFive distributes
        // `u-boot-spl.bin.normal.out`, already carrying the 1024-byte header;
        // handed here as the raw SPL it would be headered a *second* time, and a
        // double-headered SPL does not boot -- on the one write path that cannot
        // read back to catch it. The caller supplies the raw `u-boot-spl.bin`, and
        // [`recover`] adds the header for the chosen medium.
        if splhdr::looks_headered(spl) {
            return Err(Error::InvalidRequest(
                "this SPL already carries a StarFive .normal.out header, so it appears to be a \
                 u-boot-spl.bin.normal.out. Use the raw, un-headered u-boot-spl.bin instead. \
                 pyrographer adds the header for the chosen medium, and a second header produces \
                 an SPL that will not boot."
                    .to_string(),
            ));
        }
        stages.push(PlannedStage {
            kind: StageKind::Spl,
            menu_option: menu_option(StageKind::Spl, request.target),
            // The header is prepended before the body, so the wire size is both.
            image_bytes: splhdr::HEADER_LEN as u64 + spl.len() as u64,
        });
    }
    if let Some(uboot) = request.uboot {
        stages.push(PlannedStage {
            kind: StageKind::UBoot,
            menu_option: menu_option(StageKind::UBoot, request.target),
            image_bytes: uboot.len() as u64,
        });
    }

    if stages.is_empty() {
        return Err(Error::InvalidRequest(
            "a recovery must write at least one of an SPL or a U-Boot payload, and the request \
             named neither"
                .to_string(),
        ));
    }

    Ok(RecoveryPlan {
        target: request.target,
        agent_bytes: request.agent.len() as u64,
        stages,
        verified: caps().can_verify,
    })
}

/// Carry out a confirmed recovery over `serial`.
///
/// It uploads the agent, carries out the confirmed plan's stages in order, and
/// exits the menu. Each stage selects a menu digit and XMODEMs the file, with the
/// SPL headered for the target. Progress is reported per transfer. [`cancel`](Cancel)
/// is checked between blocks, so a canceled recovery stops at a block boundary and
/// never inside a block.
///
/// The bytes come from `request`, and `confirmed` supplies the consent and the
/// order. The two must describe the same recovery: `confirmed` must come from a
/// [`plan_recover`] over this same request. [`flash`] trusts its image to match the
/// [`ConfirmedWrite`](crate::verbs::ConfirmedWrite) it was planned against in the
/// same way. A request whose target differs from the plan's returns
/// [`Error::InvalidRequest`] before any byte goes out.
///
/// There is no read-back, because the protocol has no read path (see the module
/// documentation). The per-block `ACK` alone confirms the write, and the confirmed
/// plan states this.
///
/// [`flash`]: crate::verbs::flash
pub async fn recover<S: Serial>(
    serial: &mut S,
    request: &RecoveryRequest<'_>,
    confirmed: ConfirmedRecovery,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let plan = confirmed.plan();

    // **The menu slots and the SPL header must derive from one target.** The menu
    // digits come from the plan (`stage.menu_option`, chosen at plan time); the SPL
    // header is built below from the *plan's* target for exactly that reason. This
    // refuses if the request handed a different target than the plan was made
    // against -- a NOR-headered SPL sent to the eMMC menu slot has a valid CRC and
    // cannot be read back, which is the brick this module is built to make
    // impossible. The two front-ends keep the pair together; a library caller must
    // too.
    if request.target != plan.target {
        return Err(Error::InvalidRequest(format!(
            "this recovery was planned for the {:?} target but the request names {:?}. \
             Plan and run against the same medium",
            plan.target, request.target
        )));
    }

    // Upload the recovery agent. It is pre-headered, so it goes as-is.
    send_xmodem(serial, request.agent, progress, cancel).await?;

    for stage in &plan.stages {
        // Checked at the top of each stage, not only inside the XMODEM send: a
        // cancel flipped between stages would otherwise still select the next menu
        // digit and wait out the whole handshake before noticing.
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        select_option(serial, stage.menu_option).await?;
        match stage.kind {
            StageKind::Spl => {
                // The SPL is the one thing this module headers, and it is the only
                // way to produce the eMMC variant correctly. Headered for the
                // *plan's* target, the same value the menu slot above came from.
                let spl = request.spl.ok_or_else(|| {
                    Error::InvalidRequest(
                        "the confirmed plan writes an SPL, but the request carries none"
                            .to_string(),
                    )
                })?;
                let image = splhdr::build(spl, plan.target.into());
                send_xmodem(serial, &image, progress, cancel).await?;
            }
            StageKind::UBoot => {
                let uboot = request.uboot.ok_or_else(|| {
                    Error::InvalidRequest(
                        "the confirmed plan writes a U-Boot payload, but the request carries none"
                            .to_string(),
                    )
                })?;
                send_xmodem(serial, uboot, progress, cancel).await?;
            }
        }
    }

    // Leave the agent at its menu rather than in a half-driven state.
    select_option(serial, MENU_EXIT).await
}

/// Send `data` to the receiver by XMODEM-CRC, 128-byte blocks.
///
/// The sender state machine:
///
/// - Wait for the receiver's `C`
/// - Send each block, and wait for its `ACK`
/// - Resend the *same* block on a `NAK`
/// - End with `EOT`
///
/// [`xmodem::block`] frames the blocks. This function does the I/O and the control
/// flow.
async fn send_xmodem<S: Serial>(
    serial: &mut S,
    data: &[u8],
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    wait_for_crc_request(serial).await?;

    let total_bytes = data.len() as u64;
    progress(Progress::Started { total_bytes });

    let mut seq: u8 = 1;
    let mut done: u64 = 0;
    for chunk in data.chunks(xmodem::BLOCK_DATA) {
        // Between blocks, never inside one: a canceled recovery stops at a block
        // boundary, so the receiver is never left partway through a block.
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }

        let frame = xmodem::block(seq, chunk);
        send_until_acked(serial, &frame, "block").await?;

        seq = seq.wrapping_add(1);
        done += chunk.len() as u64;
        progress(Progress::Advanced {
            done_bytes: done,
            total_bytes,
        });
    }

    send_until_acked(serial, &[xmodem::EOT], "end-of-transfer").await?;
    progress(Progress::Finished { done_bytes: done });
    Ok(())
}

/// Wait for the receiver to ask for CRC mode by streaming `C`.
///
/// The ROM emits `C` continuously until it gets a block. The sender begins on the
/// first `C`, and the rest are stale. This function therefore returns at the first
/// `C`, and does not drain the storm. [`await_response`] ignores the
/// leftover `C`s while it waits for the first block's `ACK`, which is where they
/// arrive.
///
/// It waits, and does not fail at the first quiet moment. A person straps a board
/// and powers it on, and the `C` storm starts then. A read that timed out because
/// the board was not yet ready is part of that wait, so the loop reads again. Bytes
/// that are not a `C` are ignored the same way. They include a stray byte, and a
/// checksum-mode `NAK` this CRC-only sender does not honor. Only [`HANDSHAKE_READS`]
/// reads with no `C`, quiet or not, mean a board that is not in UART recovery.
///
/// That outcome returns an error that says so. A read that fails for any other
/// reason, such as a disconnected port, is a real failure and is returned at once.
async fn wait_for_crc_request<S: Serial>(serial: &mut S) -> Result<()> {
    let mut buf = [0u8; 64];
    for _ in 0..HANDSHAKE_READS {
        match serial.read(&mut buf).await {
            Ok(n) if buf[..n].contains(&xmodem::CRC_REQUEST) => return Ok(()),
            // Non-`C` bytes, or a read that waited out its deadline with nothing:
            // keep waiting for the board.
            Ok(_) | Err(Error::Timeout { .. }) => continue,
            Err(other) => return Err(other),
        }
    }
    Err(Error::Protocol(
        "the receiver never asked for CRC mode: no `C` arrived on the serial line. Check that \
         the board is strapped into UART recovery and was powered on in that mode."
            .to_string(),
    ))
}

/// Send `frame` and resend it until the receiver `ACK`s, or give up.
///
/// All of the ROM's over-`NAK` tolerance is here. A `NAK`, however many arrive
/// together, means: resend the identical bytes once, and wait again. A burst of
/// `NAK`s causes one resend, not one per `NAK`, and never advances the stream. Only
/// an `ACK` moves on. A receiver that `NAK`s [`MAX_RESENDS`] times will not
/// complete the transfer, and the function returns an error instead of retrying
/// forever.
///
/// `what` names what is being sent, a block or the end-of-transfer, for the error
/// message a person reads on failure.
async fn send_until_acked<S: Serial>(serial: &mut S, frame: &[u8], what: &str) -> Result<()> {
    for _ in 0..=MAX_RESENDS {
        serial.write_all(frame).await?;
        match await_response(serial).await? {
            Response::Acked => return Ok(()),
            // Resend the same bytes. The loop does not touch the sequence number
            // or the data, which is the invariant a NAK burst must not break.
            Response::Retransmit => continue,
        }
    }
    Err(Error::Protocol(format!(
        "the receiver did not accept a {what} after {MAX_RESENDS} resends. This ROM's XMODEM is \
         known to send NAKs aggressively. A transfer that does not complete is reported rather \
         than retried, because further retries risk bricking the board. If the board booted from \
         an SD image, flash from a running U-Boot instead, which is the documented fallback."
    )))
}

/// What the receiver said after a block.
enum Response {
    /// It accepted the block, and the sender moves on.
    Acked,
    /// It wants the block again, or has gone quiet long enough to warrant a
    /// resend. Either way the *same* block is sent again.
    Retransmit,
}

/// Read the receiver's reply to a block and classify it.
///
/// An `ACK` anywhere in what was read wins. A receiver that emitted leftover `C`s
/// and then an `ACK` has accepted the block. Otherwise a `NAK`, alone or in a
/// burst, is a single retransmit request. A `CAN` means the receiver aborted, and
/// returns [`Error::Protocol`] with no retry. Silence that lasts long enough is a
/// retransmit trigger, the classic XMODEM sender timeout.
async fn await_response<S: Serial>(serial: &mut S) -> Result<Response> {
    let mut buf = [0u8; 64];
    for _ in 0..ACK_READS {
        match serial.read(&mut buf).await {
            Ok(n) => {
                let chunk = &buf[..n];
                if chunk.contains(&xmodem::ACK) {
                    return Ok(Response::Acked);
                }
                if chunk.contains(&xmodem::CAN) {
                    return Err(Error::Protocol(
                        "the receiver canceled the transfer (CAN)".to_string(),
                    ));
                }
                if chunk.contains(&xmodem::NAK) {
                    return Ok(Response::Retransmit);
                }
                // Leftover `C`s or line noise: keep reading for the real reply.
            }
            // The host's patience ran out with nothing to show. In XMODEM that is
            // itself a resend trigger, bounded by the caller's resend cap.
            Err(Error::Timeout { .. }) => return Ok(Response::Retransmit),
            Err(other) => return Err(other),
        }
    }
    Ok(Response::Retransmit)
}

/// Select a recovery-agent menu option by sending its ASCII digit.
///
/// Whether the agent wants a trailing newline after the digit is **\[UNVERIFIED\]**,
/// because the agent is closed-source and no document states it. The digit is sent
/// alone, which is what a menu that reads a single keypress expects. If a board
/// shows that it needs a terminator, the terminator is added here.
async fn select_option<S: Serial>(serial: &mut S, option: u8) -> Result<()> {
    serial.write_all(&[b'0' + option]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    /// Receiver steps for a clean XMODEM send of `data`: a short `C` storm, then an
    /// `ACK` for each block and for the `EOT`.
    fn scripted_send(data: &[u8]) -> Vec<SerialStep> {
        let mut steps = vec![SerialStep::Rx(vec![xmodem::CRC_REQUEST; 4])];
        let mut seq = 1u8;
        for chunk in data.chunks(xmodem::BLOCK_DATA) {
            steps.push(SerialStep::ExpectTx(xmodem::block(seq, chunk)));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
            seq = seq.wrapping_add(1);
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    /// A driver for `send_xmodem` alone, so the framing and handshake are pinned
    /// without the recovery orchestration around them.
    fn run_send(data: &[u8], steps: Vec<SerialStep>) -> Result<()> {
        let mut serial = ScriptedSerial::new(steps);
        let result =
            pollster::block_on(send_xmodem(&mut serial, data, &mut |_| {}, &Cancel::new()));
        if result.is_ok() {
            serial.assert_drained();
        }
        result
    }

    /// The receiver's ACKs for a send. Each block's sequence number is derived from
    /// its index by modular arithmetic (1, 2, ..., 255, 0, 1, ...), not from the
    /// sender's own running `wrapping_add`. An independent formula therefore pins
    /// the wrap at 256. A sender that got the wrap wrong would disagree with it
    /// here, and could not agree by construction.
    fn scripted_send_indexed_seqs(data: &[u8]) -> Vec<SerialStep> {
        let mut steps = vec![SerialStep::Rx(vec![xmodem::CRC_REQUEST; 4])];
        for (i, chunk) in data.chunks(xmodem::BLOCK_DATA).enumerate() {
            let seq = ((i + 1) % 256) as u8;
            steps.push(SerialStep::ExpectTx(xmodem::block(seq, chunk)));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    /// An end-to-end send that crosses the block-255 wrap. It sends 257 blocks, so
    /// the sender's own counter produces 1..255, 0, 1. The block codec pins the wrap
    /// in isolation. Only a transfer this long puts a block numbered 0, and then 1
    /// again, on the wire from `send_xmodem`'s counter. The expectation numbers
    /// blocks by index, so it cross-checks the counter and does not mirror the same
    /// `wrapping_add`.
    #[test]
    fn a_send_crosses_the_block_255_wrap() {
        let data = vec![0xa9u8; 257 * xmodem::BLOCK_DATA];
        run_send(&data, scripted_send_indexed_seqs(&data)).expect("the wrap was crossed cleanly");
    }

    /// A multi-block send, on the wire: the handshake, then a block per 128 bytes
    /// numbered from 1, then `EOT`. The scripted serial asserts every byte, so a
    /// block built wrong (a bad CRC, a transposed sequence) fails here.
    #[test]
    fn a_send_frames_the_handshake_blocks_and_eot() {
        // 300 bytes is three blocks: two full and one padded.
        let data: Vec<u8> = (0..300u32).map(|i| (i * 5 + 1) as u8).collect();
        run_send(&data, scripted_send(&data)).expect("a clean send follows the script");
    }

    /// A one-block file still handshakes, sends block 1, and ends with `EOT`.
    #[test]
    fn a_short_send_is_a_single_block_and_eot() {
        let data = vec![0x42u8; 10];
        run_send(&data, scripted_send(&data)).expect("a short send follows the script");
    }

    /// This test pins the NAK tolerance. The receiver answers block 1 with a burst
    /// of NAKs. A strict "one NAK, resend one block" sender would resend once per
    /// NAK and desynchronize. A naive sender that advanced on any reply would send
    /// block 2 with block 1 unacknowledged. The correct sender resends the
    /// identical block 1 exactly once and waits again, as the single second
    /// `ExpectTx(block 1)` asserts. The ACK then arrives, and the sender moves on.
    #[test]
    fn a_nak_burst_resends_the_same_block_once_and_does_not_advance() {
        let data: Vec<u8> = (0..200u32).map(|i| i as u8).collect(); // two blocks
        let steps = vec![
            SerialStep::Rx(vec![xmodem::CRC_REQUEST; 4]),
            SerialStep::ExpectTx(xmodem::block(1, &data[..128])),
            // A burst of spurious NAKs, all in one read.
            SerialStep::Rx(vec![xmodem::NAK, xmodem::NAK, xmodem::NAK]),
            // The identical block 1 again -- same sequence, same bytes -- and only
            // once, no matter that three NAKs arrived.
            SerialStep::ExpectTx(xmodem::block(1, &data[..128])),
            SerialStep::Rx(vec![xmodem::ACK]),
            // Only now does block 2 go out.
            SerialStep::ExpectTx(xmodem::block(2, &data[128..])),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_send(&data, steps).expect("a NAK burst is tolerated, not doubled or advanced past");
    }

    /// Leftover `C`s from the handshake storm arrive while the sender waits for the
    /// first block's ACK. They are ignored, and an ACK in the same read still
    /// accepts the block. A sender that treated a `C` as an error, or advanced on
    /// it, would fail against the ROM this module targets.
    #[test]
    fn leftover_handshake_cs_before_an_ack_are_ignored() {
        let data = vec![0x7eu8; 64];
        let steps = vec![
            SerialStep::Rx(vec![xmodem::CRC_REQUEST]),
            SerialStep::ExpectTx(xmodem::block(1, &data)),
            // Stale Cs, then the real ACK, in one read.
            SerialStep::Rx(vec![xmodem::CRC_REQUEST, xmodem::CRC_REQUEST, xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_send(&data, steps).expect("leftover Cs do not derail the wait for an ACK");
    }

    /// A receiver that answers with bytes but never a `C`, or stays silent, is a
    /// board that is not in UART recovery. After [`HANDSHAKE_READS`] such reads, the
    /// sender returns a Protocol error and transmits no blocks. The script carries
    /// exactly that many non-`C` reads and no `ExpectTx`. A sender that began
    /// transmitting would write past the end of the script and panic.
    #[test]
    fn a_receiver_that_never_requests_crc_mode_is_refused() {
        let steps = (0..HANDSHAKE_READS)
            .map(|_| SerialStep::Rx(vec![0x00]))
            .collect();
        let mut serial = ScriptedSerial::new(steps);
        let error = pollster::block_on(send_xmodem(
            &mut serial,
            &[0u8; 64],
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("no C ever arrived");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    /// The handshake waits for the board. A person straps a board and powers it
    /// on, and the line is quiet for a moment before the `C` storm begins. A read
    /// that times out before the board is ready is part of the wait, not a failure.
    /// Here the line times out twice, then streams `C`, and the send proceeds. A
    /// handshake that gave up on the first quiet read would fail a board that was
    /// about to answer.
    #[test]
    fn the_handshake_waits_through_quiet_reads_for_the_board() {
        let data = vec![0x33u8; 20];
        let steps = vec![
            SerialStep::Timeout,
            SerialStep::Timeout,
            SerialStep::Rx(vec![xmodem::CRC_REQUEST; 3]),
            SerialStep::ExpectTx(xmodem::block(1, &data)),
            SerialStep::Rx(vec![xmodem::ACK]),
            SerialStep::ExpectTx(vec![xmodem::EOT]),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        run_send(&data, steps).expect("a board that is slow to start is waited for");
    }

    /// A read that fails for a reason other than a timeout, such as a disconnected
    /// port, is a real failure and is not waited through. The line drops during the
    /// handshake, and the error is returned at once, without spending the rest of
    /// the read bound.
    #[test]
    fn a_dropped_line_during_the_handshake_is_a_failure_not_a_wait() {
        let mut serial = ScriptedSerial::new(vec![SerialStep::Disconnect]);
        let error = pollster::block_on(send_xmodem(
            &mut serial,
            &[0u8; 64],
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the port went away");
        assert!(matches!(error, Error::Disconnected), "{error:?}");
    }

    /// Cancellation stops the send at a block boundary. The script has the
    /// handshake and one block. The token is set as that block's progress is
    /// reported. A send that ignored cancellation would then try a second block and
    /// run off the end of the script.
    #[test]
    fn a_canceled_send_stops_at_the_next_block_boundary() {
        let data = vec![0x11u8; 300]; // three blocks
        let steps = vec![
            SerialStep::Rx(vec![xmodem::CRC_REQUEST; 4]),
            SerialStep::ExpectTx(xmodem::block(1, &data[..128])),
            SerialStep::Rx(vec![xmodem::ACK]),
        ];
        let mut serial = ScriptedSerial::new(steps);
        let cancel = Cancel::new();
        let error = pollster::block_on(send_xmodem(
            &mut serial,
            &data,
            &mut |event| {
                if let Progress::Advanced { .. } = event {
                    cancel.cancel();
                }
            },
            &cancel,
        ))
        .expect_err("the send was canceled after the first block");
        assert!(matches!(error, Error::Canceled), "{error:?}");
    }

    /// The whole recovery, scripted:
    ///
    /// - Upload the agent
    /// - Pick the SPL-to-flash menu digit
    /// - Send the SPL as a `.normal.out`
    /// - Exit
    ///
    /// The SPL that crosses the wire is the headered image this module builds from
    /// the raw body, not the raw body. The `ExpectTx` blocks over `splhdr::build`
    /// assert this.
    #[test]
    fn a_recovery_uploads_the_agent_then_headers_and_sends_the_spl() {
        let agent = vec![0xa9u8; 200]; // two blocks
        let spl_body = vec![0x5au8; 50];
        let framed_spl = splhdr::build(&spl_body, splhdr::Target::NorFlash);

        let mut steps = scripted_send(&agent);
        steps.push(SerialStep::ExpectTx(vec![b'0'])); // SPL to NOR flash
        steps.extend(scripted_send(&framed_spl));
        steps.push(SerialStep::ExpectTx(vec![b'5'])); // exit

        let request = RecoveryRequest {
            target: RecoveryTarget::NorFlash,
            agent: &agent,
            spl: Some(&spl_body),
            uboot: None,
        };
        let plan = plan_recover(&request).expect("a plan");
        assert!(!plan.verified, "a StarFive recovery is never verified");
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.stages[0].menu_option, 0);
        assert_eq!(
            plan.stages[0].image_bytes,
            (splhdr::HEADER_LEN + spl_body.len()) as u64,
            "the plan's size is the headered size that crosses the wire"
        );

        let mut serial = ScriptedSerial::new(steps);
        pollster::block_on(recover(
            &mut serial,
            &request,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the recovery follows the script");
        serial.assert_drained();
    }

    /// An eMMC SPL and a U-Boot payload make two stages. The SPL goes under menu
    /// `1`, headered for eMMC (the sentinel-CRC variant). U-Boot goes under `3`,
    /// sent as-is. The order is SPL, then U-Boot.
    #[test]
    fn a_recovery_writes_spl_then_uboot_to_the_named_target() {
        let agent = vec![0x01u8; 10];
        let spl_body = vec![0x02u8; 20];
        let uboot = vec![0x03u8; 20];
        let framed_spl = splhdr::build(&spl_body, splhdr::Target::Emmc);

        let mut steps = scripted_send(&agent);
        steps.push(SerialStep::ExpectTx(vec![b'1'])); // SPL to eMMC
        steps.extend(scripted_send(&framed_spl));
        steps.push(SerialStep::ExpectTx(vec![b'3'])); // U-Boot to eMMC
        steps.extend(scripted_send(&uboot));
        steps.push(SerialStep::ExpectTx(vec![b'5'])); // exit

        let request = RecoveryRequest {
            target: RecoveryTarget::Emmc,
            agent: &agent,
            spl: Some(&spl_body),
            uboot: Some(&uboot),
        };
        let plan = plan_recover(&request).expect("a plan");
        assert_eq!(
            plan.stages.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![StageKind::Spl, StageKind::UBoot],
            "SPL is written before U-Boot"
        );

        let mut serial = ScriptedSerial::new(steps);
        pollster::block_on(recover(
            &mut serial,
            &request,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("a two-stage recovery follows the script");
        serial.assert_drained();
    }

    /// A recovery that would write nothing is refused at the plan, before a serial
    /// line is opened. A request with neither an SPL nor a U-Boot has no content.
    #[test]
    fn a_recovery_that_writes_nothing_is_refused() {
        let agent = vec![0u8; 8];
        let request = RecoveryRequest {
            target: RecoveryTarget::NorFlash,
            agent: &agent,
            spl: None,
            uboot: None,
        };
        let error = plan_recover(&request).expect_err("nothing to write");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// **A pre-headered `.normal.out` supplied as the SPL is refused, not
    /// double-headered.** StarFive distributes `u-boot-spl.bin.normal.out`, which
    /// already carries the 1024-byte header. Supplied as the raw SPL, it would be
    /// headered a second time, and a double-headered SPL does not boot. This path
    /// cannot read back to detect that. A raw SPL, whose first bytes are not the
    /// header's fixed markers, plans without complaint.
    #[test]
    fn a_pre_headered_spl_is_refused_rather_than_double_headered() {
        let agent = vec![0u8; 8];
        let raw_spl = vec![0x5au8; 64];

        let ok = RecoveryRequest {
            target: RecoveryTarget::NorFlash,
            agent: &agent,
            spl: Some(&raw_spl),
            uboot: None,
        };
        plan_recover(&ok).expect("a raw SPL is what the plan expects");

        let headered = splhdr::build(&raw_spl, splhdr::Target::NorFlash);
        let doubled = RecoveryRequest {
            target: RecoveryTarget::NorFlash,
            agent: &agent,
            spl: Some(&headered),
            uboot: None,
        };
        let error = plan_recover(&doubled).expect_err("a pre-headered SPL must be refused");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// **A recovery run against a different target than it was planned for is
    /// refused before a byte is sent.** The menu digits come from the plan, and the
    /// SPL header from the plan's target. Without the refusal, a request naming a
    /// different medium would send a NOR-headered SPL to the eMMC menu slot (or the
    /// reverse) with a valid CRC. This path cannot read back to detect that. The
    /// scripted serial holds no steps, so any transfer would run it off the end and
    /// panic.
    #[test]
    fn a_recovery_run_against_a_different_target_than_planned_is_refused() {
        let agent = vec![0xa9u8; 8];
        let spl = vec![0x5au8; 8];

        let planned = RecoveryRequest {
            target: RecoveryTarget::NorFlash,
            agent: &agent,
            spl: Some(&spl),
            uboot: None,
        };
        let confirmed = plan_recover(&planned).expect("a plan").confirm();

        let mismatched = RecoveryRequest {
            target: RecoveryTarget::Emmc,
            agent: &agent,
            spl: Some(&spl),
            uboot: None,
        };
        let mut serial = ScriptedSerial::new(vec![]);
        let error = pollster::block_on(recover(
            &mut serial,
            &mismatched,
            confirmed,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("plan and request name different targets");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// OTP is unreachable by construction. No `(kind, target)` pair maps to menu
    /// option `4`, so no request to this module reaches the fuse-burning option.
    /// This test pins the mapping to `0..=3`.
    #[test]
    fn no_menu_option_reaches_otp() {
        for kind in [StageKind::Spl, StageKind::UBoot] {
            for target in [RecoveryTarget::NorFlash, RecoveryTarget::Emmc] {
                assert!(
                    menu_option(kind, target) <= 3,
                    "a stage produced a menu option above 3"
                );
            }
        }
    }

    /// The backend reports that it cannot verify a write. A front-end reads this
    /// to gray out or annotate what StarFive cannot do.
    #[test]
    fn recovery_advertises_that_it_cannot_verify() {
        assert!(!caps().can_verify);
    }
}
