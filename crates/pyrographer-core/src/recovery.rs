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
//! agent, and writes the board's QSPI NOR flash:
//!
//! 1. Send the recovery agent (`jh7110-recovery-*.bin`) to the ROM by XMODEM. It
//!    runs from SRAM, brings up DRAM, and prints a menu.
//! 2. For each file, choose its menu entry, wait until the agent asks for the file,
//!    send it by XMODEM, and read the agent's verdict. The SPL goes under menu
//!    entry `0`, at flash offset `0x0`. U-Boot goes under entry `2`, at `0x10_0000`.
//! 3. Choose entry `5`, which halts the agent.
//!
//! # Driven by the agent's own text
//!
//! The agent reads a line of input whenever it is not receiving a file, and echoes
//! it. Only a carriage return ends a line. A line that reads exactly `4` opens its
//! OTP fuse menu, where almost any next line burns fuses from values compiled into
//! the agent. Bytes sent at the wrong moment are therefore typing, and can be
//! permanent. The driver sends nothing on a timer or on a guess:
//!
//! - It starts a file only after the receiver has asked for it. The ROM asks with
//!   its `C` storm, and the agent prints `send file by xmodem` and then streams
//!   `C`s. Both print a `C` in their banners, `(C)StarFive` and `CPU freq`, so a
//!   single `C` is not a request. The request is a run of ten, the count Milk-V's
//!   flashing tool waits for.
//! - It types a line only at the agent's main prompt, `select the function to
//!   test: `, and every line it types is a digit and a carriage return. Before each
//!   choice it sends an empty line, which the agent answers by drawing its menu
//!   again. A stray byte left in the agent's line, such as a repeated `EOT`, is
//!   cleared that way rather than joined to the digit.
//! - The digits it types are `0`, `2` and `5`. [`menu_option`] produces only `0`
//!   and `2`, and no path through this module types a `4`.
//! - During a transfer, an answer from the agent that is not an XMODEM control byte
//!   stops the transfer at once (see [`modem`]).
//! - Every wait also watches for the OTP menu's own text. If it appears, the driver
//!   stops and sends nothing more, and returns [`Error::FuseMenuOpened`].
//!
//! # Why flash, and not eMMC
//!
//! The agent also offers to write the SPL and U-Boot to eMMC, and this module does
//! not use those entries. The agent writes the eMMC SPL at sector 0, over the
//! protective MBR, the GPT header and the partition entries. It writes U-Boot at
//! 3 MiB, where no StarFive disk image keeps it. The ROM's eMMC boot mode is
//! deprecated, and a Mars CM cannot select it from its connector at all.
//!
//! An eMMC is written as a disk instead.
//! [`bootstrap::starfive`](crate::bootstrap::starfive) RAM-boots a U-Boot, and its
//! USB mass-storage gadget hands the eMMC to the Block backend. That backend reads
//! back every window it writes.
//!
//! # The SPL and its backup copy
//!
//! The SPL is sent as a `.normal.out`, and [`splhdr::prepare`] makes one from a raw
//! `u-boot-spl.bin` or checks one that came with its header. The agent writes it at
//! `0x0` and a second time at the backup address in its header, `0x20_0000` by
//! default. That address lies inside the U-Boot region. A recovery that writes the
//! SPL therefore writes U-Boot after it, as StarFive's own procedure does.
//! [`plan_recover`] refuses an SPL on its own whose backup copy would land in
//! U-Boot.
//!
//! # No read-back
//!
//! Every other write in pyrographer reads back every window it writes, and the
//! write path is built on that invariant. **The StarFive recovery protocol cannot
//! read flash at all.** The ROM in recovery is a pure XMODEM *receiver*. Every menu
//! entry of the closed-source agent is an "update", with no read, dump or verify.
//!
//! The per-block XMODEM `ACK` confirms the *transfer*. The agent's verdict,
//! `updata success` or `updata fail`, reports whether its write routine returned
//! without an error. The agent reads nothing back to reach it. A reported failure
//! is an error here, and a reported success is not a read-back.
//!
//! This backend therefore cannot keep the read-back invariant, and says so.
//! [`caps`] reports `can_verify: false`, and the [`RecoveryPlan`] a person confirms
//! states it again. A StarFive recovery is unverified in a way a Rockchip write is
//! not.
//!
//! # Tests without a board
//!
//! The XMODEM framing is a sans-I/O codec, [`xmodem`](crate::codec::xmodem), and
//! the agent's text is matched by the [`console`](crate::codec::console) codec.
//! Tests pin the whole sequence against a scripted serial that prints what the ROM
//! and the agent print. The agent's behavior is read from its code. Whether the
//! real ROM's `C`, `ACK` and `NAK` timing matches these assumptions is
//! **\[UNVERIFIED\]** until a board settles it.
//!
//! [`Serial`]: crate::transport::Serial

use crate::codec::console::{Console, Match, text};
use crate::codec::crc::crc32;
use crate::codec::splhdr::{self, Origin};
use crate::console::{self, ConsoleSink, Patience};
use crate::modem::{self, Receiver};
use crate::progress::{Cancel, ProgressSink};
use crate::transport::Serial;
use crate::{Error, Result};

/// Where the agent writes the SPL in flash, as a byte offset.
pub const SPL_OFFSET: u64 = 0;
/// Where the agent writes U-Boot in flash, as a byte offset. It is the start of
/// the U-Boot region in StarFive's flash layout and in mainline U-Boot's.
pub const UBOOT_OFFSET: u64 = 0x10_0000;
/// The flash the agent writes, which it treats as one 16 MiB part whatever the
/// part reports.
pub const FLASH_BYTES: u64 = 16 * 1024 * 1024;
/// The largest SPL the agent writes to flash. It refuses a larger one only after
/// the whole transfer, so the plan refuses it first.
pub const SPL_MAX: u64 = 1024 * 1024;
/// The largest U-Boot the agent writes to flash, refused at the plan for the same
/// reason as [`SPL_MAX`].
pub const UBOOT_MAX: u64 = 15 * 1024 * 1024;

/// The menu digit that halts the recovery agent.
const MENU_EXIT: u8 = 5;

/// The key the agent's line reader takes as the end of a line. It is a carriage
/// return alone. A line feed is stored in the line and does not end it.
const ENTER: u8 = b'\r';

/// The agent's main prompt. It ends in a space, with no newline.
const MAIN_PROMPT: &[u8] = b"select the function to test: ";
/// What the agent prints when its OTP fuse menu opens: the menu's title, and its
/// prompt, which differs from the main prompt only by its capital letter.
const FUSE_MENU: [&[u8]; 2] = [b"otp updata", b"Select the function to test: "];
/// What the agent prints after a menu choice, before it asks for the file.
const RECEIVING: &[u8] = b"send file by xmodem";
/// A receiver's request for a file: a run of `C`s, long enough that the `C` in a
/// banner is not mistaken for one.
const CRC_RUN: &[u8] = b"CCCCCCCCCC";
/// The agent's verdict after a write it completed.
const SUCCESS: &[u8] = b"updata success";
/// The agent's verdict after a write that failed, including one too large to
/// write.
const FAILURE: &[u8] = b"updata fail";
/// What the agent prints when it halts.
const HALTED: &[u8] = b"END OF SECONDBOOT";
/// What the agents print when they cannot bring up DRAM, where they stage every
/// file. Two of them stop there, and the third shows its menu all the same.
const NO_DRAM: [&[u8]; 3] = [
    b"DDR init failed",
    b"fail to get DDR size",
    b"test ddr fail",
];

/// The magic a FIT image begins with, a flattened devicetree's.
const FIT_MAGIC: [u8; 4] = [0xd0, 0x0d, 0xfe, 0xed];

/// How long to wait for the ROM to ask for the agent. A person powers the board
/// on after starting the recovery, and the line is quiet until then. A board that
/// prints a lot and never asks is not in UART recovery.
const ROM_START: Patience = Patience {
    silent_reads: 60,
    max_bytes: 4096,
};
/// How long to wait for the agent to bring up DRAM and print its menu.
const AGENT_START: Patience = Patience {
    silent_reads: 30,
    max_bytes: 64 * 1024,
};
/// How long to wait for the agent to answer a line at its menu.
const MENU: Patience = Patience {
    silent_reads: 10,
    max_bytes: 16 * 1024,
};
/// How long to wait for the agent's receiver to start asking for the file.
const RECEIVER_START: Patience = Patience {
    silent_reads: 10,
    max_bytes: 4096,
};
/// How long to wait for the agent to halt.
const HALT: Patience = Patience {
    silent_reads: 5,
    max_bytes: 4096,
};

/// How long to wait for the agent's verdict on a file of `image_bytes`.
///
/// A flash write erases 64 KiB blocks, and an erase prints nothing. The agent can
/// therefore fall quiet for a while in the middle of a healthy write. The wait
/// allows thirty seconds of silence, and two more for every 64 KiB block the
/// image covers. The agent prints a dot per 256-byte page it programs, so the
/// output is bounded by the image's size.
fn verdict_patience(image_bytes: u64) -> Patience {
    let blocks = image_bytes.div_ceil(64 * 1024);
    Patience {
        silent_reads: 30 + 2 * u32::try_from(blocks).unwrap_or(u32::MAX / 4),
        max_bytes: image_bytes / 64 + 64 * 1024,
    }
}

/// Which object a recovery stage writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    /// The second-stage program loader, sent as a `.normal.out`.
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

/// The recovery agent's menu digit for writing `kind` to flash.
///
/// This mapping is the only place a write's menu digit is produced, and it produces
/// only `0` and `2`. The exit, `5`, is a separate constant the driver types to halt
/// the agent. No path through this module produces `4`, the OTP fuse menu. The
/// agent's eMMC entries, `1` and `3`, are not produced either, for the reason the
/// module documentation gives.
pub fn menu_option(kind: StageKind) -> u8 {
    match kind {
        StageKind::Spl => 0,
        StageKind::UBoot => 2,
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
    /// Always `false`. The recovery protocol has no read path. The per-block
    /// XMODEM `ACK` guarantees that the bytes were *received*, and the agent's
    /// verdict that its write routine returned. Neither says the flash *holds*
    /// them. A front-end shows this field, so a person knows a StarFive recovery is
    /// unverified in a way a Rockchip write is not.
    pub can_verify: bool,
}

/// What a StarFive recovery backend supports.
pub fn caps() -> RecoveryCaps {
    RecoveryCaps { can_verify: false }
}

/// Everything a recovery would write, borrowed for the length of the call.
///
/// The agent is always sent first. At least one of [`spl`](Self::spl) or
/// [`uboot`](Self::uboot) must be present, and an SPL needs U-Boot beside it.
/// [`plan_recover`] refuses a request that breaks either rule. With both present,
/// the SPL is written before U-Boot.
pub struct RecoveryRequest<'a> {
    /// The recovery agent (`jh7110-recovery-*.bin`), headered, sent as-is.
    pub agent: &'a [u8],
    /// The SPL: a raw `u-boot-spl.bin`, which this module headers, or a
    /// `u-boot-spl.bin.normal.out`, whose header is checked and kept.
    pub spl: Option<&'a [u8]>,
    /// The U-Boot FIT payload, sent as-is.
    pub uboot: Option<&'a [u8]>,
}

/// One stage a recovery would carry out, as the plan describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedStage {
    /// What this stage writes.
    pub kind: StageKind,
    /// The agent menu digit it is sent under.
    pub menu_option: u8,
    /// How many bytes the transfer carries. For an SPL this is the headered image,
    /// so it is not the size of a raw file on disk. XMODEM's framing and the
    /// padding in its last block are not counted.
    pub image_bytes: u64,
    /// Where in flash the agent writes it, as a byte offset.
    pub offset: u64,
    /// Where the agent writes a second copy, for the SPL alone: the backup address
    /// its header carries.
    pub backup_offset: Option<u64>,
    /// For the SPL, whether its header came with the file or is built here.
    pub origin: Option<Origin>,
    /// The CRC-32 of what the transfer carries. [`recover`] refuses a request
    /// whose files do not match it, so a confirmation agrees to these bytes.
    pub crc32: u32,
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
    /// How many bytes the recovery agent upload will send.
    pub agent_bytes: u64,
    /// The CRC-32 of the agent, which binds the confirmation to it as
    /// [`PlannedStage::crc32`] binds each stage.
    pub agent_crc32: u32,
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

    /// One sentence on where the SPL's backup copy lands, for both front-ends to
    /// show. `None` when the plan writes no SPL.
    pub fn describe_backup(&self) -> Option<String> {
        let spl = self.stages.iter().find(|s| s.kind == StageKind::Spl)?;
        let backup = spl.backup_offset?;
        let copy = (backup, backup + spl.image_bytes);
        let uboot = self
            .stages
            .iter()
            .find(|s| s.kind == StageKind::UBoot)
            .map(|s| (s.offset, s.offset + s.image_bytes));
        Some(match uboot {
            Some(uboot) if overlaps(copy, uboot) => format!(
                "The agent writes a backup copy of the SPL at {backup:#x}, inside the U-Boot \
                 region. U-Boot is written after it and overwrites that copy, as StarFive's own \
                 procedure does."
            ),
            _ => format!(
                "The agent writes a backup copy of the SPL at {backup:#x}, where the ROM looks if \
                 the copy at 0x0 fails its check."
            ),
        })
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
/// cannot be read. It checks everything that can be checked in the files instead,
/// and refuses before a port is opened:
///
/// - An agent that is not a headered image with a matching CRC, or that does not
///   carry the recovery agent's menu.
/// - A recovery that would write neither an SPL nor a U-Boot.
/// - An SPL whose header does not check, or that is a FIT image, the shape of a
///   U-Boot payload.
/// - A U-Boot payload that is not a FIT image.
/// - An SPL over [`SPL_MAX`] or a U-Boot over [`UBOOT_MAX`], which the agent
///   would refuse only after the whole transfer.
/// - An SPL with no U-Boot beside it, when the SPL's backup copy lands in the
///   U-Boot region.
pub fn plan_recover(request: &RecoveryRequest<'_>) -> Result<RecoveryPlan> {
    check_agent(request.agent)?;

    if request.spl.is_none() && request.uboot.is_none() {
        return Err(Error::InvalidRequest(
            "a recovery must write at least one of an SPL or a U-Boot payload, and the request \
             named neither"
                .to_string(),
        ));
    }

    let mut stages = Vec::new();

    if let Some(file) = request.spl {
        let prepared = prepare_spl(file)?;
        let header = splhdr::parse_header(&prepared.image)?;
        let image_bytes = prepared.image.len() as u64;
        if image_bytes > SPL_MAX {
            return Err(Error::InvalidRequest(format!(
                "this SPL is {image_bytes} bytes with its header, and the recovery agent writes \
                 an SPL of at most {SPL_MAX} bytes to flash"
            )));
        }

        let backup = u64::from(header.bofs);
        let copy = (backup, backup + image_bytes);
        if request.uboot.is_none() && overlaps(copy, (UBOOT_OFFSET, FLASH_BYTES)) {
            return Err(Error::InvalidRequest(format!(
                "an SPL is written with U-Boot beside it. The agent writes a backup copy of the \
                 SPL at {backup:#x}, which is inside the U-Boot region, so an SPL written alone \
                 breaks the U-Boot already on the board. Give the U-Boot payload as well, and it \
                 is written after the SPL, as StarFive's own procedure does"
            )));
        }

        stages.push(PlannedStage {
            kind: StageKind::Spl,
            menu_option: menu_option(StageKind::Spl),
            image_bytes,
            offset: SPL_OFFSET,
            backup_offset: Some(backup),
            origin: Some(prepared.origin),
            crc32: crc32(&prepared.image),
        });
    }

    if let Some(uboot) = request.uboot {
        check_uboot(uboot)?;
        let image_bytes = uboot.len() as u64;
        if image_bytes > UBOOT_MAX {
            return Err(Error::InvalidRequest(format!(
                "this U-Boot payload is {image_bytes} bytes, and the recovery agent writes one of \
                 at most {UBOOT_MAX} bytes to flash"
            )));
        }
        stages.push(PlannedStage {
            kind: StageKind::UBoot,
            menu_option: menu_option(StageKind::UBoot),
            image_bytes,
            offset: UBOOT_OFFSET,
            backup_offset: None,
            origin: None,
            crc32: crc32(uboot),
        });
    }

    Ok(RecoveryPlan {
        agent_bytes: request.agent.len() as u64,
        agent_crc32: crc32(request.agent),
        stages,
        verified: caps().can_verify,
    })
}

/// Carry out a confirmed recovery over `serial`.
///
/// It uploads the agent, writes the confirmed plan's stages in order, and halts the
/// agent. Each stage chooses the stage's menu entry, sends the file once the agent
/// asks for it, and reads the agent's verdict. A reported failure stops the
/// recovery with [`Error::AgentWriteFailed`], and nothing more is written.
///
/// Progress is reported per transfer. What the ROM and the agent print outside a
/// transfer goes to `sink` as it arrives. A long flash write is then seen to be
/// working. [`cancel`](Cancel) is checked between blocks and between waits, never
/// inside a block.
///
/// The bytes come from `request`, and `confirmed` supplies the consent and the
/// order. A request whose files differ from the ones the plan was made from returns
/// [`Error::InvalidRequest`] before any byte goes out.
///
/// There is no read-back, because the protocol has no read path (see the module
/// documentation), and the confirmed plan states this.
pub async fn recover<S: Serial>(
    serial: &mut S,
    request: &RecoveryRequest<'_>,
    confirmed: ConfirmedRecovery,
    progress: ProgressSink<'_>,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let plan = confirmed.plan();

    // **The consent covers these bytes and no others.** The plan carries a CRC of
    // every file it describes, so planning the request again and comparing tells a
    // request that is not the one a person agreed to. This path cannot read back
    // to catch a wrong file afterward.
    if plan_recover(request)? != *plan {
        return Err(Error::InvalidRequest(
            "these files are not the ones this recovery was planned for. Plan again with the \
             files you mean to write"
                .to_string(),
        ));
    }

    let mut session = Session {
        serial,
        console: Console::new(),
        sink,
        cancel,
    };

    session.upload_agent(request.agent, progress).await?;
    session.reach_menu().await?;

    for stage in &plan.stages {
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        let image = match stage.kind {
            StageKind::Spl => {
                let spl = request.spl.ok_or_else(|| missing(stage.kind))?;
                prepare_spl(spl)?.image
            }
            StageKind::UBoot => request.uboot.ok_or_else(|| missing(stage.kind))?.to_vec(),
        };
        session.write(stage, &image, progress).await?;
    }

    session.halt().await
}

/// A recovery's conversation with the ROM and the agent.
struct Session<'a, S> {
    serial: &'a mut S,
    /// Everything the board has printed, and how far the driver has read it.
    console: Console,
    sink: ConsoleSink<'a>,
    cancel: &'a Cancel,
}

/// What a wait saw: which pattern, and the line it appeared on.
struct Seen {
    /// The index of the pattern among those waited for.
    index: usize,
    /// The text of the line the pattern ends, from its start to the pattern's end.
    line: String,
}

impl<S: Serial> Session<'_, S> {
    /// Wait for one of `patterns`, and watch for the OTP fuse menu throughout.
    ///
    /// The earliest in the stream wins. If it is the fuse menu, the wait returns
    /// [`Error::FuseMenuOpened`] and the driver sends nothing more. A wait that
    /// runs out of patience returns `None`.
    async fn wait(&mut self, patterns: &[&[u8]], patience: Patience) -> Result<Option<Seen>> {
        let mut watched: Vec<&[u8]> = patterns.to_vec();
        watched.extend_from_slice(&FUSE_MENU);
        let found = console::look_for_patiently(
            &mut *self.serial,
            &mut self.console,
            &watched,
            patience,
            &mut *self.sink,
            self.cancel,
        )
        .await?;
        let Some(found) = found else {
            return Ok(None);
        };
        if found.pattern >= patterns.len() {
            return Err(Error::FuseMenuOpened {
                transcript: self.console.tail_text(),
            });
        }
        let line = line_of(&self.console, &found);
        self.console.consume(&found);
        Ok(Some(Seen {
            index: found.pattern,
            line,
        }))
    }

    /// [`wait`](Self::wait), with a wait that runs out of patience made an error
    /// that opens with `what` and carries the transcript.
    async fn expect(&mut self, patterns: &[&[u8]], patience: Patience, what: &str) -> Result<Seen> {
        match self.wait(patterns, patience).await? {
            Some(seen) => Ok(seen),
            None => Err(Error::Protocol(format!(
                "{what} Recent console output:\n{}",
                self.console.tail_text()
            ))),
        }
    }

    /// Wait for the ROM to ask for a file, and send it the agent.
    async fn upload_agent(&mut self, agent: &[u8], progress: ProgressSink<'_>) -> Result<()> {
        send_to_rom(
            &mut *self.serial,
            &mut self.console,
            agent,
            progress,
            &mut *self.sink,
            self.cancel,
        )
        .await
    }

    /// Wait for the agent to bring up DRAM and print its menu.
    async fn reach_menu(&mut self) -> Result<()> {
        let patterns = [MAIN_PROMPT, NO_DRAM[0], NO_DRAM[1], NO_DRAM[2]];
        let seen = self
            .expect(
                &patterns,
                AGENT_START,
                "the recovery agent did not reach its menu.",
            )
            .await?;
        if seen.index > 0 {
            return Err(Error::Protocol(format!(
                "the recovery agent could not bring up the board's DRAM, where it stages every \
                 file, so nothing was written. It said: {}",
                seen.line
            )));
        }
        Ok(())
    }

    /// Type an empty line, then `option` as a line, at the agent's main prompt.
    ///
    /// The driver is at a prompt whenever this is called, because every step
    /// before it ends by waiting for one. The empty line clears whatever the
    /// agent's line holds, and the agent draws its menu again in answer.
    async fn choose(&mut self, option: u8) -> Result<()> {
        self.serial.write_all(&[ENTER]).await?;
        self.expect(
            &[MAIN_PROMPT],
            MENU,
            "the recovery agent did not draw its menu again.",
        )
        .await?;
        self.serial.write_all(&[b'0' + option, ENTER]).await
    }

    /// Write one stage: choose its entry, send the file when asked, and read the
    /// verdict.
    async fn write(
        &mut self,
        stage: &PlannedStage,
        image: &[u8],
        progress: ProgressSink<'_>,
    ) -> Result<()> {
        self.choose(stage.menu_option).await?;

        let seen = self
            .expect(
                &[RECEIVING, MAIN_PROMPT],
                MENU,
                "the recovery agent did not answer the menu choice.",
            )
            .await?;
        if seen.index == 1 {
            return Err(Error::Protocol(format!(
                "the recovery agent did not take menu entry {} and drew its menu again. Nothing \
                 was sent.",
                stage.menu_option
            )));
        }
        self.expect(
            &[CRC_RUN],
            RECEIVER_START,
            "the recovery agent said it would receive a file, and did not ask for it.",
        )
        .await?;

        modem::send_xmodem(
            &mut *self.serial,
            &mut self.console,
            image,
            Receiver::RecoveryAgent,
            progress,
            &mut *self.sink,
            self.cancel,
        )
        .await?;

        let seen = self
            .expect(
                &[SUCCESS, FAILURE],
                verdict_patience(image.len() as u64),
                "the recovery agent received the file and did not report whether it wrote it.",
            )
            .await?;
        if seen.index == 1 {
            return Err(Error::AgentWriteFailed {
                stage: stage.kind.name(),
                said: seen.line,
            });
        }

        // The agent draws its menu after every write. Reading up to its prompt
        // leaves the driver at the prompt, where the next choice starts.
        self.expect(
            &[MAIN_PROMPT],
            MENU,
            "the recovery agent did not return to its menu after the write.",
        )
        .await?;
        Ok(())
    }

    /// Halt the agent, so nothing typed at the port later reaches its menu.
    ///
    /// The writes are done and judged by now, so an agent that does not say it
    /// halted is not an error. The fuse menu still is.
    async fn halt(&mut self) -> Result<()> {
        self.choose(MENU_EXIT).await?;
        self.wait(&[HALTED], HALT).await?;
        Ok(())
    }
}

/// Wait for the JH7110 ROM to ask for a file, then send it `image` by XMODEM.
///
/// The ROM asks with a run of `C`s, and prints `(C)StarFive` between runs, so the
/// request is [`CRC_RUN`] and not a single `C`. A person powers the board on after
/// starting, so the wait lasts through a minute of silence. Both StarFive paths
/// begin here: the recovery sends the agent, and the RAM boot sends an SPL.
pub(crate) async fn send_to_rom<S: Serial>(
    serial: &mut S,
    console: &mut Console,
    image: &[u8],
    progress: ProgressSink<'_>,
    sink: ConsoleSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let found = console::look_for_patiently(
        &mut *serial,
        console,
        &[CRC_RUN],
        ROM_START,
        &mut *sink,
        cancel,
    )
    .await?;
    let Some(found) = found else {
        return Err(Error::Protocol(format!(
            "the board never asked for a file: ten `C`s in a row did not arrive. Check that the \
             board is strapped into UART recovery and was powered on in that mode, and that TX, \
             RX and GND are wired. Recent console output:\n{}",
            console.tail_text()
        )));
    };
    console.consume(&found);
    modem::send_xmodem(
        serial,
        console,
        image,
        Receiver::BootRom,
        progress,
        sink,
        cancel,
    )
    .await
}

/// The text of the line a match ends, from the line's start to the match's end.
fn line_of(console: &Console, found: &Match) -> String {
    let before = console.before(found);
    let start = before
        .iter()
        .rposition(|&b| b == b'\n' || b == b'\r')
        .map_or(0, |i| i + 1);
    let mut line = before[start..].to_vec();
    line.extend_from_slice(console.matched(found));
    text(&line).trim().to_string()
}

/// Check that `agent` is StarFive's recovery agent: a headered image whose CRC
/// matches, and which carries the agent's menu.
fn check_agent(agent: &[u8]) -> Result<()> {
    splhdr::check(agent).map_err(|error| match error {
        Error::InvalidRequest(why) => {
            Error::InvalidRequest(format!("the recovery agent is not usable: {why}"))
        }
        other => other,
    })?;
    if !agent.windows(MAIN_PROMPT.len()).any(|w| w == MAIN_PROMPT) {
        return Err(Error::InvalidRequest(
            "this file is a StarFive image, and not the recovery agent: it does not carry the \
             agent's menu. The agent is a jh7110-recovery-*.bin from StarFive's Tools repository"
                .to_string(),
        ));
    }
    Ok(())
}

/// Make the SPL ready to send, refusing a file that is plainly something else.
pub(crate) fn prepare_spl(file: &[u8]) -> Result<splhdr::Prepared> {
    if file.starts_with(&FIT_MAGIC) {
        return Err(Error::InvalidRequest(
            "this SPL file is a FIT image, which is the shape of a U-Boot payload. The SPL is \
             u-boot-spl.bin, or u-boot-spl.bin.normal.out with its header"
                .to_string(),
        ));
    }
    splhdr::prepare(file)
}

/// Check that a U-Boot payload is a FIT image, as both StarFive's and mainline's
/// are.
pub(crate) fn check_uboot(uboot: &[u8]) -> Result<()> {
    if uboot.starts_with(&FIT_MAGIC) {
        return Ok(());
    }
    let more = if splhdr::looks_headered(uboot) {
        " It carries a StarFive header, which makes it an SPL or the recovery agent."
    } else {
        ""
    };
    Err(Error::InvalidRequest(format!(
        "this U-Boot payload is not a FIT image. StarFive's visionfive2_fw_payload.img and \
         mainline's u-boot.itb both are.{more}"
    )))
}

/// Whether two half-open byte ranges share a byte.
fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The error for a confirmed stage whose file the request does not carry.
fn missing(kind: StageKind) -> Error {
    Error::InvalidRequest(format!(
        "the confirmed plan writes the {}, and the request carries none",
        kind.name()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::xmodem::{self, BlockSize};
    use crate::progress::Progress;
    use crate::transport::testing::{ScriptedSerial, SerialStep};

    /// The menu as the agent draws it, ending at its prompt.
    const MENU_TEXT: &[u8] = b"\r\n****************** JH7110 program tool ******************\r\n\
        0: update 2ndboot/SPL in flash\r\n1: update 2ndboot/SPL in emmc\r\n\
        2: update fw_verif/uboot in flash\r\n3: update fw_verif/uboot in emmc\r\n\
        4: update otp, caution!!!!\r\n5: exit\r\n\
        NOTE: current xmodem receive buff = 0x40000000, 'load 0x********' to change.\r\n\
        select the function to test: ";

    /// An agent as the plan checks one: headered, with its menu in the body.
    fn agent() -> Vec<u8> {
        let mut body = b"agent code ".to_vec();
        body.extend_from_slice(MAIN_PROMPT);
        body.extend_from_slice(&[0x13; 200]);
        splhdr::build(&body)
    }

    /// A raw SPL: code, not a header and not a FIT.
    fn spl() -> Vec<u8> {
        (0..300u32).map(|i| (i * 7 + 3) as u8).collect()
    }

    /// A U-Boot payload: a FIT, so it begins with the devicetree magic.
    fn uboot() -> Vec<u8> {
        let mut fit = FIT_MAGIC.to_vec();
        fit.extend((0..400u32).map(|i| (i * 13) as u8));
        fit
    }

    /// The receiver's side of a clean XMODEM send of `data` in 128-byte blocks: an
    /// `ACK` for each block and for the `EOT`.
    fn acked(data: &[u8]) -> Vec<SerialStep> {
        let mut steps = Vec::new();
        for (i, chunk) in data.chunks(128).enumerate() {
            let seq = ((i + 1) % 256) as u8;
            steps.push(SerialStep::ExpectTx(xmodem::block(
                BlockSize::Small,
                seq,
                chunk,
            )));
            steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        }
        steps.push(SerialStep::ExpectTx(vec![xmodem::EOT]));
        steps.push(SerialStep::Rx(vec![xmodem::ACK]));
        steps
    }

    /// The ROM in UART recovery, the agent sent to it, and the agent at its menu.
    fn agent_at_menu(agent: &[u8]) -> Vec<SerialStep> {
        // The banner's own `C` comes first, and a quiet moment. A sender that
        // started on it would write here, where the script expects a read.
        let mut steps = vec![
            SerialStep::Rx(b"(C)StarFive\r\nC".to_vec()),
            SerialStep::Timeout,
            SerialStep::Rx(b"CCCCCCCCCCCC".to_vec()),
        ];
        steps.extend(acked(agent));
        // The agent's own banner holds a `C` too, before its menu.
        steps.push(SerialStep::Rx(
            b"\r\nCPU freq: 1250MHz\r\nidcode: 0x1860C8\r\nDDR clk 2133M, size 4GB\r\n".to_vec(),
        ));
        steps.push(SerialStep::Rx(MENU_TEXT.to_vec()));
        steps
    }

    /// Choosing menu entry `digit`: an empty line, the menu drawn again, the digit
    /// and a carriage return, and the agent asking for the file.
    fn choose(digit: u8) -> Vec<SerialStep> {
        let mut asked = vec![digit, b'\r', b'\n'];
        asked.extend_from_slice(b"send file by xmodem\r\n");
        asked.extend_from_slice(b"CCCCCCCCCCCC");
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(MENU_TEXT.to_vec()),
            SerialStep::ExpectTx(vec![digit, b'\r']),
            SerialStep::Rx(asked),
        ]
    }

    /// The agent writing a file to flash and reporting `verdict`, then its menu.
    fn verdict(verdict: &[u8]) -> Vec<SerialStep> {
        let mut said = b"updata first section\r\n....".to_vec();
        said.extend_from_slice(verdict);
        said.extend_from_slice(b"\r\n");
        vec![
            SerialStep::Rx(said),
            SerialStep::Timeout,
            SerialStep::Rx(MENU_TEXT.to_vec()),
        ]
    }

    /// Halting the agent with entry `5`.
    fn halt() -> Vec<SerialStep> {
        vec![
            SerialStep::ExpectTx(b"\r".to_vec()),
            SerialStep::Rx(MENU_TEXT.to_vec()),
            SerialStep::ExpectTx(b"5\r".to_vec()),
            SerialStep::Rx(b"5\r\nEND OF SECONDBOOT\r\n".to_vec()),
        ]
    }

    /// Run a planned recovery of `request` against `steps`.
    fn run(request: &RecoveryRequest<'_>, steps: Vec<SerialStep>) -> (Result<()>, Vec<u8>) {
        let plan = plan_recover(request).expect("a plan");
        let mut serial = ScriptedSerial::new(steps);
        let mut said = Vec::new();
        let result = pollster::block_on(recover(
            &mut serial,
            request,
            plan.confirm(),
            &mut |_| {},
            &mut |bytes| said.extend_from_slice(bytes),
            &Cancel::new(),
        ));
        if result.is_ok() {
            serial.assert_drained();
        }
        (result, said)
    }

    /// The whole recovery, on the wire.
    ///
    /// The agent goes to the ROM once ten `C`s have arrived, and not on the `C` in
    /// the ROM's banner. Each file goes once the agent has said `send file by
    /// xmodem` and asked, and each choice is an empty line and then the digit with
    /// a carriage return. The SPL that crosses the wire is the headered image. The
    /// SPL is written before U-Boot, and the agent is halted at the end. The
    /// scripted serial asserts every byte written, and panics on a write where it
    /// expects a read.
    #[test]
    fn a_recovery_follows_the_agents_own_text() {
        let agent = agent();
        let spl = spl();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: Some(&spl),
            uboot: Some(&uboot),
        };

        let mut steps = agent_at_menu(&agent);
        steps.extend(choose(b'0'));
        steps.extend(acked(&splhdr::build(&spl)));
        steps.extend(verdict(b"updata success"));
        steps.extend(choose(b'2'));
        steps.extend(acked(&uboot));
        steps.extend(verdict(b"updata success"));
        steps.extend(halt());

        let (result, said) = run(&request, steps);
        result.expect("the recovery follows the script");
        let said = text(&said);
        assert!(said.contains("updata first section"), "{said}");
        assert!(said.contains("END OF SECONDBOOT"), "{said}");
    }

    /// The plan for that recovery: the SPL under entry 0 at 0x0 with its backup at
    /// 2 MiB, headered here, then U-Boot under entry 2 at 1 MiB. Neither is read
    /// back. The backup sentence says whether U-Boot covers the backup copy.
    #[test]
    fn the_plan_names_each_stage_where_it_goes_and_that_it_is_not_read_back() {
        let agent = agent();
        let spl = spl();
        let uboot = uboot();
        let plan = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&spl),
            uboot: Some(&uboot),
        })
        .expect("a plan");

        assert!(!plan.verified, "a StarFive recovery is never verified");
        assert_eq!(plan.agent_bytes, agent.len() as u64);
        let [first, second] = &plan.stages[..] else {
            panic!("two stages: {:?}", plan.stages);
        };
        assert_eq!(
            (first.kind, first.menu_option, first.offset),
            (StageKind::Spl, 0, 0)
        );
        assert_eq!(
            first.image_bytes,
            (splhdr::HEADER_LEN + spl.len()) as u64,
            "the size is the headered image that crosses the wire"
        );
        assert_eq!(first.backup_offset, Some(0x20_0000));
        assert_eq!(first.origin, Some(Origin::HeaderedHere));
        assert_eq!(
            (second.kind, second.menu_option, second.offset),
            (StageKind::UBoot, 2, 0x10_0000)
        );
        // This U-Boot ends far short of 2 MiB, so the backup copy stays whole.
        let backup = plan.describe_backup().expect("an SPL is written");
        assert!(backup.contains("where the ROM looks"), "{backup}");

        // A real U-Boot runs past 2 MiB, and covers the backup copy.
        let mut large = uboot.clone();
        large.resize(0x12_0000, 0);
        let plan = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&spl),
            uboot: Some(&large),
        })
        .expect("a plan");
        let backup = plan.describe_backup().expect("an SPL is written");
        assert!(backup.contains("overwrites that copy"), "{backup}");
    }

    /// A U-Boot payload alone is a recovery, and is written under entry 2.
    #[test]
    fn a_u_boot_alone_is_written_under_entry_2() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.extend(choose(b'2'));
        steps.extend(acked(&uboot));
        steps.extend(verdict(b"updata success"));
        steps.extend(halt());
        run(&request, steps).0.expect("a U-Boot-only recovery");
    }

    /// **The agent's verdict is read.** It reports a failed write, and the
    /// recovery stops there with the agent's own line. U-Boot is not written after
    /// a failed SPL, and the agent is not halted: the script ends at the verdict,
    /// so anything more would run off it.
    #[test]
    fn a_write_the_agent_reports_failed_stops_the_recovery_with_its_words() {
        let agent = agent();
        let spl = spl();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: Some(&spl),
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.extend(choose(b'0'));
        steps.extend(acked(&splhdr::build(&spl)));
        steps.push(SerialStep::Rx(
            b"updata first section\r\n..spi flash program error\r\nupdata fail\r\n".to_vec(),
        ));

        let (result, _) = run(&request, steps);
        let error = result.expect_err("the agent said the write failed");
        let Error::AgentWriteFailed { stage, said } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*stage, "SPL");
        assert_eq!(said, "updata fail");
    }

    /// The size refusal the agent prints is a failure verdict too, and its whole
    /// line is the reason given.
    #[test]
    fn the_agents_size_refusal_is_reported_as_its_whole_line() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.extend(choose(b'2'));
        steps.extend(acked(&uboot));
        steps.push(SerialStep::Rx(
            b"ERROR: 0x1000000, exceeds the available size in flash, updata fail\r\n".to_vec(),
        ));
        let (result, _) = run(&request, steps);
        let Err(Error::AgentWriteFailed { said, .. }) = result else {
            panic!("{result:?}");
        };
        assert!(said.starts_with("ERROR: 0x1000000"), "{said}");
    }

    /// **The fuse menu stops everything.** If the agent's OTP menu appears at any
    /// point, the driver stops and sends nothing more. Here it appears where the
    /// main menu was expected after the empty line.
    #[test]
    fn the_fuse_menu_appearing_stops_the_recovery_and_nothing_more_is_sent() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.push(SerialStep::ExpectTx(b"\r".to_vec()));
        steps.push(SerialStep::Rx(
            b"\r\n*************** JH7110  otp updata **********************\r\n\
              14: quit\r\nSelect the function to test: "
                .to_vec(),
        ));
        let (result, _) = run(&request, steps);
        let error = result.expect_err("the fuse menu is open");
        assert!(matches!(error, Error::FuseMenuOpened { .. }), "{error:?}");
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("power the board off now"))
        );
    }

    /// An agent that cannot bring up DRAM stops the recovery before a choice is
    /// typed, even when it goes on to show its menu.
    #[test]
    fn an_agent_whose_dram_test_failed_writes_nothing() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = vec![SerialStep::Rx(b"CCCCCCCCCCCC".to_vec())];
        steps.extend(acked(&agent));
        steps.push(SerialStep::Rx(
            b"End init lpddr4, test ddr fail\r\n".to_vec(),
        ));
        steps.push(SerialStep::Rx(MENU_TEXT.to_vec()));
        let (result, _) = run(&request, steps);
        let Err(Error::Protocol(message)) = result else {
            panic!("{result:?}");
        };
        assert!(message.contains("DRAM"), "{message}");
    }

    /// A choice the agent did not take brings its menu back instead of `send file
    /// by xmodem`. The recovery stops, and no file goes out.
    #[test]
    fn a_choice_the_agent_did_not_take_sends_no_file() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.push(SerialStep::ExpectTx(b"\r".to_vec()));
        steps.push(SerialStep::Rx(MENU_TEXT.to_vec()));
        steps.push(SerialStep::ExpectTx(b"2\r".to_vec()));
        steps.push(SerialStep::Rx(MENU_TEXT.to_vec()));
        let (result, _) = run(&request, steps);
        let Err(Error::Protocol(message)) = result else {
            panic!("{result:?}");
        };
        assert!(message.contains("did not take menu entry 2"), "{message}");
    }

    /// A board that is not in UART recovery prints, and never asks for a file. The
    /// wait ends once it has printed more than the ROM ever would, and nothing is
    /// sent.
    #[test]
    fn a_board_that_never_asks_for_a_file_is_refused() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let steps = vec![SerialStep::Rx(vec![b'x'; 4096])];
        let (result, _) = run(&request, steps);
        let Err(Error::Protocol(message)) = result else {
            panic!("{result:?}");
        };
        assert!(message.contains("never asked for a file"), "{message}");
    }

    /// Progress is reported per transfer: the agent, then each file.
    #[test]
    fn progress_is_reported_for_each_transfer() {
        let agent = agent();
        let uboot = uboot();
        let request = RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        };
        let mut steps = agent_at_menu(&agent);
        steps.extend(choose(b'2'));
        steps.extend(acked(&uboot));
        steps.extend(verdict(b"updata success"));
        steps.extend(halt());

        let plan = plan_recover(&request).unwrap();
        let mut serial = ScriptedSerial::new(steps);
        let mut started = Vec::new();
        pollster::block_on(recover(
            &mut serial,
            &request,
            plan.confirm(),
            &mut |event| {
                if let Progress::Started { total_bytes } = event {
                    started.push(total_bytes);
                }
            },
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("a recovery");
        assert_eq!(started, vec![agent.len() as u64, uboot.len() as u64]);
    }

    /// **An SPL alone is refused** when its backup copy lands in the U-Boot region,
    /// as the default one does. The agent would write that copy into the middle of
    /// the U-Boot already on the board.
    #[test]
    fn an_spl_alone_is_refused_because_its_backup_copy_breaks_u_boot() {
        let agent = agent();
        let spl = spl();
        let error = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&spl),
            uboot: None,
        })
        .expect_err("an SPL alone");
        let Error::InvalidRequest(message) = error else {
            panic!("{error:?}");
        };
        assert!(message.contains("0x200000"), "{message}");
        assert!(message.contains("U-Boot payload as well"), "{message}");
    }

    /// An SPL that comes with its header is checked and kept, never headered a
    /// second time. One whose header does not check is refused.
    #[test]
    fn a_headered_spl_is_kept_and_a_damaged_one_refused() {
        let agent = agent();
        let uboot = uboot();
        let headered = splhdr::build(&spl());
        let plan = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&headered),
            uboot: Some(&uboot),
        })
        .expect("a .normal.out is accepted");
        assert_eq!(plan.stages[0].origin, Some(Origin::Headered));
        assert_eq!(plan.stages[0].image_bytes, headered.len() as u64);

        let mut damaged = headered.clone();
        *damaged.last_mut().unwrap() ^= 0xff;
        let error = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&damaged),
            uboot: Some(&uboot),
        })
        .expect_err("its CRC no longer matches");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The files are checked for what they are. An agent that is not headered, a
    /// headered image that is not the agent, a U-Boot that is not a FIT, and an SPL
    /// that is a FIT are all refused, which catches two files picked in each
    /// other's place.
    #[test]
    fn files_that_are_not_what_they_are_given_as_are_refused() {
        let agent = agent();
        let spl = spl();
        let uboot = uboot();
        let not_an_agent = splhdr::build(&spl);

        // An agent, an SPL, a U-Boot, and what is wrong with them.
        type Case<'a> = (&'a [u8], Option<&'a [u8]>, Option<&'a [u8]>, &'a str);
        let cases: [Case<'_>; 4] = [
            (&spl, None, Some(&uboot), "agent not headered"),
            (
                &not_an_agent,
                None,
                Some(&uboot),
                "an SPL given as the agent",
            ),
            (&agent, None, Some(&not_an_agent), "an SPL given as U-Boot"),
            (
                &agent,
                Some(&uboot),
                Some(&uboot),
                "U-Boot given as the SPL",
            ),
        ];
        for (agent, spl, uboot, case) in cases {
            let error = plan_recover(&RecoveryRequest { agent, spl, uboot }).expect_err(case);
            assert!(
                matches!(error, Error::InvalidRequest(_)),
                "{case}: {error:?}"
            );
        }
    }

    /// The agent's size limits are checked at the plan: 1 MiB for the SPL with its
    /// header, and 15 MiB for U-Boot. The agent itself refuses only after the whole
    /// transfer. A file exactly at the limit is accepted, as the agent accepts it.
    #[test]
    fn the_agents_size_limits_are_checked_at_the_plan() {
        let agent = agent();
        let uboot = uboot();

        let at_limit = vec![0x13u8; SPL_MAX as usize - splhdr::HEADER_LEN];
        plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&at_limit),
            uboot: Some(&uboot),
        })
        .expect("an SPL of exactly 1 MiB with its header");

        let over = vec![0x13u8; SPL_MAX as usize - splhdr::HEADER_LEN + 1];
        let error = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: Some(&over),
            uboot: Some(&uboot),
        })
        .expect_err("one byte over");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let mut big = uboot.clone();
        big.resize(UBOOT_MAX as usize + 1, 0);
        let error = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&big),
        })
        .expect_err("U-Boot one byte over");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A recovery that would write nothing is refused at the plan.
    #[test]
    fn a_recovery_that_writes_nothing_is_refused() {
        let agent = agent();
        let error = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: None,
        })
        .expect_err("nothing to write");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// **A recovery run with other files than it was planned for is refused before
    /// a byte is sent.** The confirmation agreed to the planned bytes. The scripted
    /// serial holds no steps, so any read or write would panic.
    #[test]
    fn a_recovery_run_with_other_files_than_planned_is_refused() {
        let agent = agent();
        let uboot = uboot();
        let confirmed = plan_recover(&RecoveryRequest {
            agent: &agent,
            spl: None,
            uboot: Some(&uboot),
        })
        .unwrap()
        .confirm();

        let mut other = uboot.clone();
        other.push(0);
        let mut serial = ScriptedSerial::new(vec![]);
        let error = pollster::block_on(recover(
            &mut serial,
            &RecoveryRequest {
                agent: &agent,
                spl: None,
                uboot: Some(&other),
            },
            confirmed,
            &mut |_| {},
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("not the planned files");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// No menu entry this module produces is the fuse menu or an eMMC entry: the
    /// stages map to `0` and `2`, and the exit is `5`.
    #[test]
    fn the_only_menu_entries_typed_are_0_2_and_5() {
        assert_eq!(menu_option(StageKind::Spl), 0);
        assert_eq!(menu_option(StageKind::UBoot), 2);
        assert_eq!(MENU_EXIT, 5);
    }

    /// The backend reports that it cannot verify a write.
    #[test]
    fn recovery_advertises_that_it_cannot_verify() {
        assert!(!caps().can_verify);
    }
}
