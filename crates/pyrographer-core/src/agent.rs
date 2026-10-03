//! The uniform block interface every backend implements.
//!
//! The backend set is closed and known at compile time, so [`FlashAgent`] is an
//! enum rather than a `dyn` trait object. Its async methods therefore need no
//! `dyn`-compatibility workaround and no `async-trait` dependency. Each backend is
//! generic over its [`Transport`], so a scripted mock can replace hardware in tests.

use crate::block::BlockAgent;
use crate::codec::bot::{self, CswStatus, Direction};
use crate::codec::dfu::{self, Functional, GetStatus, State};
use crate::codec::dfu_alt::{self, AltSetting};
use crate::codec::rockusb::{self, Opcode, ResetMode};
use crate::transport::{Control, Transport};
use crate::{Error, Result};

/// The storage medium a backend is writing to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// Raw NOR flash.
    Nor,
    /// Raw NAND flash.
    Nand,
    /// An eMMC device.
    Emmc,
    /// An SD card.
    Sd,
}

impl Medium {
    /// The medium's display name.
    ///
    /// It is the counterpart of [`rockusb::StorageMedium::name`], for the medium a
    /// backend reports with its geometry. Every LBA a verb takes is an offset into
    /// one medium. A size or a sector count identifies a place only together with
    /// the medium, so a front-end shows this name beside them.
    pub fn name(self) -> &'static str {
        match self {
            Medium::Nor => "NOR flash",
            Medium::Nand => "NAND flash",
            Medium::Emmc => "eMMC",
            Medium::Sd => "SD",
        }
    }
}

/// When a backend can prove that what it wrote is what the flash holds.
///
/// Every write is read back, and the read-back cannot be turned off. When it runs
/// depends on the protocol. The write path asks [`FlashAgent::read_back`] once,
/// before the first byte goes out, and runs one write loop for every answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadBack {
    /// Each window is read back from the flash before the next one is written. A
    /// mismatch stops the write at that window, so at most one window is written
    /// before the mismatch is found.
    ///
    /// rockusb works this way, and it gives the strongest guarantee of the three.
    /// The device serves a read at any moment, and a write is committed by the time
    /// its command returns.
    PerWindow,
    /// Nothing can be read back until the whole region is committed. After the
    /// commit, every window of the region is compared with what was sent.
    ///
    /// DFU works this way, because the protocol allows nothing else. A DFU download
    /// is one session per region. The device holds the blocks until the zero-length
    /// block that ends the session. It serves no `UPLOAD` mid-download, and cannot
    /// resume a session partway through. There is therefore nothing to read back
    /// per window.
    ///
    /// The check is still mandatory, and it still holds for an image larger than
    /// memory. What changes is the point at which a mismatch is found: after the
    /// whole region is written, rather than after one window of it. More of the
    /// flash can therefore be overwritten before the mismatch is found. A write plan
    /// states this before a person confirms it.
    AfterCommit,
    /// The device cannot be read back after a write, so a write cannot be checked.
    /// The string gives the reason, in the backend's own words.
    ///
    /// A DFU device that is not manifestation-tolerant is one example. It leaves the
    /// bus as it commits, and what returns to the bus is a new device this agent is
    /// not attached to. pyrographer refuses a write to such a device.
    Impossible(&'static str),
}

impl ReadBack {
    /// One sentence, for a person confirming a write, that says when the write is
    /// checked.
    ///
    /// Both front-ends print this sentence rather than writing their own, so a
    /// write is described in the same words wherever it is confirmed.
    /// [`plan_refusal`](crate::verbs::plan_refusal) is one function for the same
    /// reason.
    pub fn describe(self) -> &'static str {
        match self {
            ReadBack::PerWindow => {
                // The sentence names no medium, not "the flash": one of the
                // backends it speaks for is a disk, and it is the backend on
                // which a person most needs to believe the line.
                "Each window is read back and compared before the next one is written. A \
                 mismatch stops the write at that window."
            }
            ReadBack::AfterCommit => {
                "This device can be read back only after the write is committed. The whole \
                 range is written, then read back and compared. The comparison cannot be skipped, \
                 and a mismatch is found with the whole range already written."
            }
            ReadBack::Impossible(why) => why,
        }
    }
}

/// How far a backend's own addressing reaches, and why it stops there.
///
/// Every backend's addressing reaches less far than the `u64` LBA the verbs pass.
/// Each stops for its own reason:
///
/// - rockusb carries a 32-bit LBA in its command block.
/// - The DFU agent packs an alt-setting index and a block offset into one number.
/// - A block device stops only at its own geometry.
///
/// The write plan asks the backend for its ceiling once, in [`plan`], as it asks
/// for [`ReadBack`]. One ceiling applied to every backend would refuse writes a
/// device can serve, and give a false reason for the refusal.
///
/// [`plan`]: crate::verbs::plan_write
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressCeiling {
    /// One past the last sector this backend can name.
    ///
    /// A range can end exactly here, because a range's end is exclusive. A range
    /// that ends past it cannot be addressed.
    pub past_last: u64,
    /// The reason for the ceiling, in the backend's own words.
    ///
    /// It is written as the final clause of a refusal that names the sector past
    /// the ceiling. The refusal therefore names this backend's own limit.
    pub why: &'static str,
}

/// What a backend supports. A front-end uses it to refuse or disable the verbs a
/// board cannot perform.
#[derive(Debug, Clone)]
pub struct Caps {
    /// Whether the backend can erase.
    pub can_erase: bool,
    /// Whether the backend exposes a partition table.
    pub has_partitions: bool,
    /// The storage medium, where known without an [`info`](FlashAgent::info) call.
    pub medium: Option<Medium>,
    /// Whether the backend can verify a write by reading it back, and therefore
    /// whether the read-only `verify` verb can run.
    ///
    /// It is true for every backend that can read. `verify` reads the device and
    /// compares it with an image, and a write reads back what it wrote, at the point
    /// [`FlashAgent::read_back`] names. It is `false` only for a write-only path with
    /// no read-back, and no backend in this set is one. StarFive's serial recovery is
    /// write-only. It is not a [`FlashAgent`], so it carries its own `can_verify` in
    /// [`RecoveryCaps`](crate::recovery::RecoveryCaps).
    ///
    /// It reports that the read-back ran, not that the bytes are durable. On a board
    /// in a bootstrap mode the two coincide, because pyrographer owns the wire and
    /// nothing else addresses the device during a write. The host operating system
    /// also owns a block device. A cached read would return what was just written,
    /// and a mounted filesystem could overwrite it afterward. The Block backend
    /// therefore bypasses the cache and holds the device exclusively, so `true`
    /// means the same thing for a disk as for a board.
    pub can_verify: bool,
    /// Whether the backend addresses a flat whole-device LBA space, or only
    /// device-named regions that a read is aimed at by partition name.
    ///
    /// With a flat space, a raw-LBA [`dump`](crate::verbs::dump) or a
    /// [`clone`](crate::verbs::clone) can span the device. It is true for rockusb,
    /// whose loader addresses the whole flash by LBA. It is false for DFU, which
    /// addresses a *named alt-setting* from block zero and has no device-wide
    /// address. A `--partition` aim resolves to an alt-setting the backend can
    /// serve. A raw LBA or a whole-device clone names nothing it can serve. When this
    /// is false, a front-end therefore disables both.
    pub can_address_raw_lba: bool,
}

/// Flash geometry reported by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashInfo {
    /// Total size in bytes.
    pub size_bytes: u64,
    /// Logical sector size in bytes: the unit the LBA verbs address in.
    pub sector_size: u32,
    /// The storage medium, where the backend can tell.
    pub medium: Option<Medium>,
    /// The flash chip's identifier, where the backend can read one.
    pub chip_id: Option<Vec<u8>>,
}

/// The data phase of one rockusb command.
#[derive(Debug, Clone, Copy)]
enum DataPhase<'a> {
    /// No data phase.
    None,
    /// Read exactly this many bytes from the device.
    ///
    /// The reply's length is part of the command's definition: five bytes of flash
    /// ID, or eleven of flash info. A device that sends fewer has not answered the
    /// command, and the agent returns [`Error::Protocol`].
    In(usize),
    /// Read up to this many bytes from the device, and accept fewer.
    ///
    /// Used by the one command whose reply length the protocol does not define. It
    /// is measured on one SoC: the RK3576 loader answers 16 bytes.
    /// Bulk-Only Transport lets a device end a data-in phase early with a short
    /// packet, and report the shortfall as residue in the CSW. A short answer
    /// therefore leaves both ends at a command boundary, as a full one does.
    ///
    /// Where the length is known, a short answer is an error, and
    /// [`In`](DataPhase::In) returns one. Where the length is unknown, a short
    /// answer is how many bytes the loader had.
    /// [`chip_version`](RockusbAgent::chip_version) is the only caller, so its reply
    /// length is measured rather than assumed.
    ///
    /// A device that sends *more* than was asked for is still refused, by the
    /// [`Transport`] itself. Surplus bytes, which the host cannot account for, do
    /// not get through this phase either.
    InAtMost(usize),
    /// Send these bytes to the device.
    ///
    /// The destructive direction. It has no counterpart to a short read. The host
    /// either put the bytes on the wire or it did not, and a transport that could
    /// not returns [`Error::Transport`]. The CSW reports what the device did with
    /// the bytes. Only the read-back in `verbs::flash` reports what reached the
    /// flash, so that read-back cannot be turned off.
    Out(&'a [u8]),
}

/// The most sectors one LBA command moves.
///
/// The protocol's sector-count field is 16 bits wide, but the loader's own buffer
/// sets the real limit, and no loader advertises it. The reference tools have used
/// a block size of 32 sectors (16 KiB) against Rockchip loaders for years, so it is
/// known to be safe. A larger value needs a measurement against hardware first,
/// and then changes only this constant. **\[COMMUNITY\]**
const MAX_CHUNK_SECTORS: u16 = 32;

/// One past the last sector the protocol's 32-bit LBA field can name.
///
/// The last sector it can reach is `0xFFFF_FFFF`, so a transfer can end exactly
/// here, but not beyond it.
const PAST_LAST_SECTOR: u64 = 1 << 32;

/// The budget of `GETSTATUS` polls for one DFU download block's commit.
///
/// A DFU device answers each poll with its state and a `bwPollTimeout`, the time
/// the host waits before polling again. The protocol lets a device answer
/// `dfuDNLOAD-BUSY` indefinitely, even with a `bwPollTimeout` of zero. A host that
/// waits only for a state change then spins forever.
///
/// Like the other waits in this crate ([`recovery`]'s reads, [`console`]'s), this
/// one spends a budget rather than waiting for the device to end the conversation.
/// Each poll is a real control transfer with the transport's own deadline. A device
/// working through a block spends a handful of polls, and one that never finishes
/// is stopped instead of hanging the host.
///
/// Cancellation takes effect at the next window boundary in [`verbs::flash`], as it
/// does everywhere else. The bounded poll guarantees that the boundary is reached.
///
/// [`recovery`]: crate::recovery
/// [`console`]: crate::console
/// [`verbs::flash`]: crate::verbs::flash
const DOWNLOAD_BLOCK_POLLS: u32 = 1024;

/// The budget of `GETSTATUS` polls for the commit at the end of a DFU download.
///
/// The counterpart of [`DOWNLOAD_BLOCK_POLLS`] for the whole region. During
/// manifestation the device erases and programs everything it has been holding.
/// It is the longest wait of a write, so it gets the larger budget.
const MANIFEST_POLLS: u32 = 4096;

/// Check that `bytes` starting at `lba` is a range the protocol can address, and
/// return the base LBA as the command block carries it.
///
/// Read and write make the same two checks, so both make them here. The length
/// must be a whole number of sectors, and the range must fit the 32-bit sector
/// address. `what` names the operation, so the error message says which one failed.
///
/// The whole range is checked once, rather than against a counter that advances
/// as each chunk lands. A running check would let a doomed transfer move most of
/// its bytes before failing. For a write, that overwrites most of the range before
/// the refusal. A running check would also fail the last chunk of a transfer that
/// ends exactly on the final addressable sector, which the protocol allows.
fn addressable(lba: u64, bytes: usize, what: &str) -> Result<u32> {
    let sector_size = rockusb::SECTOR_SIZE as usize;
    if !bytes.is_multiple_of(sector_size) {
        return Err(Error::InvalidRequest(format!(
            "a {what} of {bytes} bytes is not a whole number of {sector_size}-byte sectors"
        )));
    }

    let sectors = (bytes / sector_size) as u64;
    u32::try_from(lba)
        .ok()
        .filter(|base| u64::from(*base) + sectors <= PAST_LAST_SECTOR)
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "a {what} of {sectors} sectors from LBA {lba} runs past the 32-bit sector address \
                 the protocol carries"
            ))
        })
}

/// Check the device's transfer accounting against what actually moved.
///
/// Bulk-Only Transport has the device report, in the CSW, how many of the
/// announced bytes it did *not* transfer. The announced length minus that residue
/// is the device's own claim about how much it moved. This function compares the
/// claim with the number of bytes that arrived.
///
/// Only one direction of disagreement is refused.
///
/// A device that claims it moved **less than arrived** has disowned bytes it sent.
/// The host cannot treat those bytes as an answer, because the device has said
/// they are not one, so this is refused. The typical case is a read that comes back
/// full while the device reports a shortfall. It is the failure the
/// [`fill`](crate::fill) check looks for, reported in the device's own numbers
/// instead of in its data. This check needs no guess about the data.
///
/// A device that claims it moved **more than arrived** is usually a loader that
/// never fills the field in. Its residue stays zero, and a short reply contradicts
/// it. The reference tools ignore residue entirely, so a loader that gets this
/// direction wrong is likely, and harmless. The length of a data phase is already
/// checked directly where it is known ([`DataPhase::In`]). It is deliberately not
/// checked where the length is unknown ([`DataPhase::InAtMost`]).
///
/// Refusing here would add nothing, and would turn away a working loader over a
/// field it never maintained. The check therefore passes a correct device and a
/// careless one. It fails only a device that reports having done less than it did.
///
/// That reasoning covers the IN direction, where the bytes are the device's. On an
/// OUT phase the same arithmetic catches a different failure, which is also refused.
/// There `delivered` is what the *host* put on the wire. A shortfall means the
/// device did not accept all of it, so the write did not land whole. Only the error
/// message differs. `direction` selects it, so the message names the party whose
/// bytes are in question.
fn check_residue(
    opcode: Opcode,
    direction: Direction,
    announced: u32,
    residue: u32,
    delivered: u32,
) -> Result<()> {
    if residue > announced {
        return Err(Error::Protocol(format!(
            "{} reports {residue} bytes of {announced} untransferred, which is more than it was \
             asked for",
            opcode.name()
        )));
    }

    let claimed = announced - residue;
    if claimed < delivered {
        let whose = match direction {
            Direction::In => "the device under-reports the bytes it sent",
            Direction::Out => "the device did not accept all the bytes the host sent",
        };
        return Err(Error::Protocol(format!(
            "{} moved {delivered} bytes but reports having moved only {claimed} of the {announced} \
             it announced: {whose}",
            opcode.name()
        )));
    }

    Ok(())
}

/// The Rockchip rockusb loader agent.
///
/// Every command has the same three phases: a CBW out, an optional data phase, and
/// a CSW in. All commands therefore run through one command routine, and each
/// operation chooses only an opcode and a data phase.
pub struct RockusbAgent<T: Transport> {
    transport: T,
    /// The tag of the last command. A CSW echoes the tag, so a reply can be matched
    /// to the command it answers.
    tag: u32,
    /// Whether the host has lost track of where the device is in the
    /// conversation. It is set for the duration of every command, and cleared only
    /// on the two endings that leave host and device synchronized. When a command
    /// ends any other way, it stays set and the agent is finished. See
    /// [`command`](Self::command).
    desynchronized: bool,
}

impl<T: Transport> RockusbAgent<T> {
    /// Create an agent over an established transport.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            tag: 0,
            desynchronized: false,
        }
    }

    /// Run one rockusb command with no subcode, and return its data phase.
    ///
    /// Every command except [`Opcode::Reset`] leaves the command block's second
    /// byte at zero, so they all take this path. It calls
    /// [`command_with_subcode`](Self::command_with_subcode) with a zero subcode, and
    /// reset calls that directly.
    async fn command(
        &mut self,
        opcode: Opcode,
        address: u32,
        sectors: u16,
        data: DataPhase<'_>,
    ) -> Result<Vec<u8>> {
        self.command_with_subcode(opcode, 0, address, sectors, data)
            .await
    }

    /// Run one rockusb command and return its data phase.
    ///
    /// A command has three phases, and the device drives two of them. A failure
    /// partway through can leave the device with bytes still to send, such as an
    /// unread CSW or the tail of a data phase. The host cannot tell how many. The
    /// next command would read those bytes as its own reply, and fail a command
    /// that did nothing wrong.
    ///
    /// The agent is therefore poisoned for the duration of the command. Only an
    /// ending that leaves host and device synchronized clears it: a CSW that
    /// answers *this* command and reports that it passed or failed. Every other
    /// ending leaves the agent poisoned:
    ///
    /// - A short data phase
    /// - A malformed or mismatched CSW
    /// - A transport failure
    /// - A phase error, which is the device reporting that it lost its place
    ///
    /// After such an ending, every command is refused with
    /// [`Error::Desynchronized`] before anything is sent.
    ///
    /// Recovering in place would need a Bulk-Only Mass Storage Reset. The rockusb
    /// loader only borrows the mass-storage framing, and whether it implements that
    /// class request is **\[UNVERIFIED\]**. Refusing assumes nothing about the
    /// device. If hardware shows that the loader implements the request, a recovery
    /// path belongs in this function.
    async fn command_with_subcode(
        &mut self,
        opcode: Opcode,
        subcode: u8,
        address: u32,
        sectors: u16,
        data: DataPhase<'_>,
    ) -> Result<Vec<u8>> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        // Poisoned from here to the endings that clear it, so every `?` below --
        // and every panic-free path out that is not one of those endings --
        // leaves the agent refusing rather than guessing.
        self.desynchronized = true;

        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;

        let (direction, data_len) = match data {
            // With no data phase the direction bit is ignored, and OUT is the
            // conventional filler.
            DataPhase::None => (Direction::Out, 0),
            DataPhase::In(len) | DataPhase::InAtMost(len) => (Direction::In, len as u32),
            DataPhase::Out(bytes) => (Direction::Out, bytes.len() as u32),
        };

        // The command block is sixteen bytes, but the CBW declares only as many
        // as the command uses, which is what the reference tools declare.
        let cdb = rockusb::build_cdb_with_subcode(opcode, subcode, address, sectors);
        let cbw = bot::build_cbw(tag, data_len, direction, &cdb[..opcode.cdb_len()]);
        self.transport.write_bulk(&cbw).await?;

        let received = match data {
            DataPhase::None => Vec::new(),
            DataPhase::In(len) => {
                let bytes = self.transport.read_bulk(len).await?;
                if bytes.len() != len {
                    return Err(Error::Protocol(format!(
                        "{} returned {} of the {len} bytes it announced",
                        opcode.name(),
                        bytes.len()
                    )));
                }
                bytes
            }
            // Short is not wrong here: the length was the question. See
            // [`DataPhase::InAtMost`].
            DataPhase::InAtMost(len) => self.transport.read_bulk(len).await?,
            DataPhase::Out(bytes) => {
                self.transport.write_bulk(bytes).await?;
                Vec::new()
            }
        };

        let csw = bot::parse_csw(&self.transport.read_bulk(bot::CSW_LEN).await?)?;
        if csw.tag != tag {
            return Err(Error::Protocol(format!(
                "a CSW tagged {:#x} answered the command tagged {tag:#x}. The device and the host \
                 are no longer synchronized",
                csw.tag
            )));
        }

        match csw.status {
            // The three phases ran to the end and the CSW answers this command,
            // so whichever way it went, both ends are back at a command boundary.
            CswStatus::Passed => {
                self.desynchronized = false;
                // Cleared *before* the accounting check on purpose. A residue
                // that disagrees with the data phase is this command's reply
                // being wrong, not the connection being lost: the CSW arrived,
                // it answers this command, and both ends are at a boundary. The
                // agent stays usable and the command fails.
                let delivered = match data {
                    DataPhase::None => 0,
                    DataPhase::In(_) | DataPhase::InAtMost(_) => received.len() as u32,
                    // The host either put every byte on the wire or the write
                    // returned an error, so what was delivered is what was sent.
                    DataPhase::Out(bytes) => bytes.len() as u32,
                };
                let direction = match data {
                    DataPhase::Out(_) => Direction::Out,
                    _ => Direction::In,
                };
                check_residue(opcode, direction, data_len, csw.residue, delivered)?;
                Ok(received)
            }
            CswStatus::Failed => {
                self.desynchronized = false;
                Err(Error::CommandFailed {
                    command: opcode.name(),
                    status: 1,
                })
            }
            // A phase error is the device reporting that the host asked for
            // something it could not sequence. It is the one status that says
            // nothing about where the device now is, so the agent stays poisoned.
            CswStatus::PhaseError => Err(Error::CommandFailed {
                command: opcode.name(),
                status: 2,
            }),
        }
    }

    /// Ask whether a loader is running and answering.
    pub async fn test_unit_ready(&mut self) -> Result<()> {
        self.command(Opcode::TestUnitReady, 0, 0, DataPhase::None)
            .await?;
        Ok(())
    }

    /// Whether this agent is desynchronized from its device. See
    /// [`command`](Self::command).
    fn is_desynchronized(&self) -> bool {
        self.desynchronized
    }

    /// Read flash geometry.
    async fn info(&mut self) -> Result<FlashInfo> {
        let chip_id = self
            .command(
                Opcode::ReadFlashId,
                0,
                0,
                DataPhase::In(rockusb::FLASH_ID_LEN),
            )
            .await?;

        let payload = self
            .command(
                Opcode::ReadFlashInfo,
                0,
                0,
                DataPhase::In(rockusb::FLASH_INFO_LEN),
            )
            .await?;
        let flash = rockusb::parse_flash_info(&payload)?;

        Ok(FlashInfo {
            size_bytes: flash.size_bytes(),
            sector_size: rockusb::SECTOR_SIZE,
            // rockusb reports geometry, not what kind of part is behind it.
            // Nothing in the flash-info payload distinguishes an eMMC from raw
            // NAND with confidence, so the backend does not guess.
            medium: None,
            chip_id: Some(chip_id),
        })
    }

    /// Ask the running loader what SoC it is on, and return its answer raw.
    ///
    /// `K_FW_GET_CHIP_VER` is the loader's own report of the SoC it runs on. The
    /// wrong-loader refusal rests on it, because the loader that answers it is the
    /// loader that would do the writing. A loader built for another SoC writes to
    /// the wrong offsets, with a plausible CSW behind every command. The USB
    /// descriptors cannot substitute: a PID names a family, and the family list is
    /// not exhaustive.
    ///
    /// The bytes are returned uninterpreted, because the gate compares the whole
    /// reply with pinned bytes and never decodes it. An RK3576's reply is pinned in
    /// [`soc`](crate::soc). This method reads whatever length the loader sends and
    /// returns it raw. The same bytes pin a new SoC and are what the armed gate
    /// checks.
    async fn chip_version(&mut self) -> Result<Vec<u8>> {
        self.command(
            Opcode::GetChipVer,
            0,
            0,
            DataPhase::InAtMost(rockusb::CHIP_VER_LEN),
        )
        .await
    }

    /// Ask the running loader what it says it can do.
    ///
    /// This differs from [`Caps`], which is pyrographer's claim about what a
    /// backend implements. This is the loader's claim about itself. The two can
    /// disagree: pyrographer implements the read path, but a loader that does not
    /// set [`read_lba`](rockusb::Capability::read_lba) will not serve it.
    ///
    /// Nothing gates on this, and it deliberately does not gate the read path. No
    /// board has answered the command. A refusal keyed on a flag nobody has seen set
    /// would be a guess, and could turn away a working loader. The answer is only
    /// reported. When hardware shows what a real loader answers, it can become a
    /// precondition.
    async fn capability(&mut self) -> Result<rockusb::Capability> {
        let payload = self
            .command(
                Opcode::ReadCapability,
                0,
                0,
                DataPhase::In(rockusb::CAPABILITY_LEN),
            )
            .await?;
        rockusb::parse_capability(&payload)
    }

    /// Ask which storage medium the loader is currently addressing.
    ///
    /// rockusb addresses one medium at a time, and every LBA is an offset into that
    /// medium. On a board with both an eMMC and a SPI NOR, one sector number names
    /// two different places. This command returns the device's own answer about
    /// which medium it is addressing.
    ///
    /// The Rockchip `parameter` codec has to *infer* whether the medium is eMMC or
    /// NAND, to decide whether its offsets need the eMMC fixup. This answer does not
    /// replace that inference, because an error in the parameter path can brick a
    /// board. It lets the two accounts be compared. A disagreement between the
    /// loader's answer and what the table implies needs a person's attention.
    async fn storage_medium(&mut self) -> Result<rockusb::StorageMedium> {
        let payload = self
            .command(
                Opcode::GetStorageMedia,
                0,
                0,
                DataPhase::In(rockusb::STORAGE_MEDIA_LEN),
            )
            .await?;
        rockusb::parse_storage_media(&payload)
    }

    /// Fill `buf` with sectors starting at `lba`, splitting the request into
    /// commands the loader accepts.
    ///
    /// The whole range is validated before the first command goes out, so a read
    /// the protocol cannot address fails before it touches the device.
    async fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        let base = addressable(lba, buf.len(), "read")?;
        let sector_size = rockusb::SECTOR_SIZE as usize;

        // Each chunk's LBA comes from the base, so once the range is known to fit
        // there is no counter left to overflow.
        for (index, chunk) in buf
            .chunks_mut(MAX_CHUNK_SECTORS as usize * sector_size)
            .enumerate()
        {
            let at = base + index as u32 * u32::from(MAX_CHUNK_SECTORS);
            let sectors = (chunk.len() / sector_size) as u16;
            let data = self
                .command(Opcode::LbaRead, at, sectors, DataPhase::In(chunk.len()))
                .await?;
            chunk.copy_from_slice(&data);
        }
        Ok(())
    }

    /// Write `data` to the sectors starting at `lba`, splitting the request into
    /// commands the loader accepts.
    ///
    /// It mirrors [`read`](Self::read) deliberately: the same range check, the same
    /// chunking, and the same command block with one byte changed. The whole range
    /// is validated before the first command goes out, so a write the protocol
    /// cannot address fails before it touches the device. That matters more for a
    /// write than for a read, because a write that fails halfway has already
    /// destroyed what it overwrote.
    ///
    /// This is the raw block interface, and it carries **no gate**. It does not
    /// check that a person agreed to the write, or that the loader is the right one
    /// for the board. It does not check that what landed is what was sent.
    /// [`verbs::flash`] makes those checks, so a caller writes through it instead.
    /// It takes a `ConfirmedWrite` that only a plan can produce, and it reads back
    /// every window it writes.
    ///
    /// [`verbs::flash`]: crate::verbs::flash
    async fn write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        let base = addressable(lba, data.len(), "write")?;
        let sector_size = rockusb::SECTOR_SIZE as usize;

        for (index, chunk) in data
            .chunks(MAX_CHUNK_SECTORS as usize * sector_size)
            .enumerate()
        {
            let at = base + index as u32 * u32::from(MAX_CHUNK_SECTORS);
            let sectors = (chunk.len() / sector_size) as u16;
            self.command(Opcode::LbaWrite, at, sectors, DataPhase::Out(chunk))
                .await?;
        }
        Ok(())
    }

    /// End the session in the [`ResetMode`] the caller names.
    ///
    /// The agent does not outlive the command in any mode. A device that reboots
    /// has left the bus, so a successful reset ends this connection as surely as a
    /// failed one. The three modes other than a plain reboot leave the device where
    /// pyrographer cannot follow it.
    ///
    /// The mode is the command block's subcode, and nothing else about the command
    /// changes. One handling of `Disconnected` therefore covers all four modes.
    async fn reset(&mut self, mode: ResetMode) -> Result<()> {
        match self
            .command_with_subcode(Opcode::Reset, mode as u8, 0, 0, DataPhase::None)
            .await
        {
            Ok(_) => Ok(()),
            // The device acts as it acknowledges, so it often leaves the bus
            // before the CSW arrives. That is the command working, not failing.
            Err(Error::Disconnected) => Ok(()),
            Err(other) => Err(other),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl<T: Transport> RockusbAgent<T> {
    /// The agent's transport, so a test can assert that the conversation it
    /// scripted ran to the end.
    ///
    /// It is behind the `testing` feature, as the scripted transport is. The GUI's
    /// job tests hand an agent to a job, get it back, and check what the job sent to
    /// the device.
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

/// The USB DFU 1.1 backend: read and write one alt-setting at a time.
///
/// This is the protocol engine an Ingenic board speaks once a DFU-capable U-Boot
/// runs on it. [`RockusbAgent`] uses Bulk-Only Transport and addresses whole-device
/// LBAs. DFU uses control transfers to an interface, and addresses a named
/// alt-setting from block zero. An alt-setting is a region of flash that the
/// device's own U-Boot exposes. The sans-I/O [`dfu`] codec does the framing, and
/// this agent is the state machine that sequences it.
///
/// # The primitives
///
/// A DFU transfer is a sequence of fixed-size blocks paced by the device. This agent
/// exposes that sequence one block at a time:
///
/// - [`download_block`](Self::download_block) sends one write block, then polls
///   `GETSTATUS` until the device is ready for the next. It honors the
///   `bwPollTimeout` back-pressure the device sets, which keeps the host from
///   running ahead of a device that erases slowly.
/// - [`finish_download`](Self::finish_download) sends the zero-length block that
///   ends a write, and waits out manifestation.
/// - [`upload_block`](Self::upload_block) reads one block back. A block shorter
///   than the transfer size is the end of the region.
///
/// The verb that consumes this agent owns the loop over these, as it does for the
/// rockusb read and write. It chunks an image by
/// [`transfer_size`](Self::transfer_size), and streams it a window at a time under a
/// cancellation token.
///
/// Selecting *which* alt-setting a transfer addresses is the transport's job,
/// because it is a standard `SET_INTERFACE` request. How a real device expects that
/// request to be issued is **\[UNVERIFIED\]**. This engine drives the transfer on
/// whatever alt-setting is current.
///
/// # Recovering in place
///
/// Unlike rockusb, DFU has an in-protocol reset. A device that reports an error
/// stays in `dfuERROR` until a `DFU_CLRSTATUS` ([`clear_status`](Self::clear_status))
/// returns it to `dfuIDLE`. The agent therefore keeps two kinds of failure apart.
///
/// A device that *answers* a `GETSTATUS` with an error status has an intact
/// conversation. The failure is reported, the agent is not desynchronized, and
/// `clear_status` can recover it. A *transport* failure, such as a timeout, a stall
/// or a device off the bus, leaves the host unable to say where the device is. The
/// agent is then [`Desynchronized`](Error::Desynchronized), and refuses every
/// command until the device is reopened. [`RockusbAgent`] follows the same rule.
pub struct DfuAgent<T: Transport> {
    transport: T,
    /// The DFU interface number, carried in `wIndex` on every request.
    interface: u16,
    /// What the device said about itself in its DFU functional descriptor.
    ///
    /// It is kept whole rather than reduced to a transfer size, because the other
    /// three bits decide what a *write* can promise. `bitCanDnload` says whether the
    /// device accepts a write at all. `bitManifestTolerant` says whether the device
    /// is still on the bus afterward to be read back. Without them, the write path
    /// would have to guess both.
    functional: Functional,
    /// The alt-settings the device exposes, which are its partitions in the DFU
    /// sense. They are read from the interface's descriptors at open, and
    /// [`partition::read`](crate::partition::read) builds the DFU backend's
    /// partition table from them. The list is empty for an agent that drives only
    /// the block primitives, as the `download_block` and `upload_block` tests do.
    alts: Vec<AltSetting>,
    /// Where a sequential read has reached, so that consecutive read windows
    /// continue the DFU block counter rather than restart it. It is `None` between
    /// reads, and after any read that did not continue the previous one. See
    /// [`read`](Self::read).
    read_cursor: Option<ReadCursor>,
    /// Where a download in flight has reached, for the same reason as
    /// [`read_cursor`](Self::read_cursor), and with the same sequential rule. With
    /// no download open, it is `None`. The first [`write`](Self::write) into a
    /// region sets it, and [`finish_write`](Self::finish_write) clears it. A set
    /// cursor is how the caller sees an unfinished download.
    write_cursor: Option<WriteCursor>,
    /// Whether the host has lost track of where the device is. It is set while a
    /// command is in flight, and cleared only on an ending that leaves host and
    /// device synchronized. A transport failure leaves it set, and the agent
    /// finished. See the type docs.
    desynchronized: bool,
}

/// Where a DFU sequential read has reached.
///
/// DFU `UPLOAD` reads a region as a sequence of blocks from its start. The verbs
/// read a window at a time, so their reads have to continue one block counter
/// rather than restart it each window. This carries that counter, the alt-setting
/// it belongs to, and the LBA the next block answers. With these,
/// [`DfuAgent::read`] can tell a continuation of the last read from a fresh start.
struct ReadCursor {
    /// The alt-setting the sequence is reading.
    alt: u8,
    /// The DFU block number the next `UPLOAD` asks for. It wraps at 2^16, as a DFU
    /// block counter does. A region larger than that is read as the device tracks
    /// the wrap.
    next_block: u16,
    /// The LBA the next block answers, so a following read whose start equals this
    /// is recognized as continuing the sequence.
    next_lba: u64,
}

/// Where a DFU download in flight has reached.
///
/// The write counterpart of [`ReadCursor`], with a stricter reason to exist. A DFU
/// download is one session per region. The block counter starts at zero at the
/// region's start, and each following block takes the next number. The device
/// commits nothing until the zero-length block that ends the session.
///
/// A session cannot be opened partway through, because whether a device honors a
/// non-zero starting block is **\[UNVERIFIED\]**. The read path declines to rely
/// on it for the same reason. A write that does not continue the last one is
/// therefore refused rather than sent to the wrong offset.
struct WriteCursor {
    /// The alt-setting the download is writing.
    alt: u8,
    /// The DFU block number the next `DNLOAD` carries. It wraps at 2^16, as a DFU
    /// block counter does.
    next_block: u16,
    /// The LBA the next block writes, so a following write whose start equals this
    /// is recognized as continuing the session.
    next_lba: u64,
    /// The LBA the session began at. It identifies the region [`finish_write`]
    /// commits and the caller then reads back.
    ///
    /// [`finish_write`]: DfuAgent::finish_write
    start_lba: u64,
}

impl<T: Transport> DfuAgent<T> {
    /// Create an agent over an established transport.
    ///
    /// `interface` is the DFU interface number, `functional` its DFU functional
    /// descriptor, which carries the advertised
    /// [`Functional::transfer_size`](crate::codec::dfu::Functional::transfer_size),
    /// and `alts` the alt-settings it exposes. All three are read from the device's
    /// descriptors at open. Pass an empty `alts` for an agent that only drives the
    /// block primitives. The [`FlashAgent`]-facing [`read`](Self::read) and
    /// [`info`](Self::info) need the real list.
    pub fn new(
        transport: T,
        interface: u16,
        functional: Functional,
        alts: Vec<AltSetting>,
    ) -> Self {
        Self {
            transport,
            interface,
            functional,
            alts,
            read_cursor: None,
            write_cursor: None,
            desynchronized: false,
        }
    }

    /// The device's DFU functional descriptor: its transfer size and capability bits.
    pub fn functional(&self) -> Functional {
        self.functional
    }

    /// The largest block a transfer can carry. The caller chunks an image by this.
    pub fn transfer_size(&self) -> u16 {
        self.functional.transfer_size
    }

    /// The DFU backend's logical sector size, which is its
    /// [`transfer_size`](Self::transfer_size).
    ///
    /// One sector is one DFU block, so an LBA is a block number. The verbs' sector
    /// arithmetic therefore matches the blocks on the wire.
    pub fn sector_size(&self) -> u32 {
        u32::from(self.functional.transfer_size)
    }

    /// The alt-settings the device exposes, which are its partitions.
    pub fn alt_settings(&self) -> &[AltSetting] {
        &self.alts
    }

    /// Report what a DFU board's descriptors say about its flash.
    ///
    /// DFU has no geometry command, so this is not a device read. The block size is
    /// the [`transfer_size`](Self::transfer_size), and the total is the sum of the
    /// exposed alt-setting sizes. The total is zero for a board that named its
    /// partitions without ranges, which reports the extent as unknown rather than
    /// guessing one. A DFU board reports no medium or chip ID, so those are `None`.
    pub fn info(&self) -> FlashInfo {
        let size_bytes: u64 = self.alts.iter().filter_map(|alt| alt.size).sum();
        FlashInfo {
            size_bytes,
            sector_size: u32::from(self.functional.transfer_size),
            medium: None,
            chip_id: None,
        }
    }

    /// Whether this agent is desynchronized from its device. The type
    /// documentation says what recovers it and what does not.
    pub fn is_desynchronized(&self) -> bool {
        self.desynchronized
    }

    /// Issue one DFU request, returning the data-in bytes for a read and nothing
    /// for a write. The [`Setup`](dfu::Setup) carries the direction.
    async fn send(&mut self, setup: dfu::Setup, data: &[u8]) -> Result<Vec<u8>> {
        if setup.is_in() {
            self.transport
                .control(Control::In {
                    request_type: setup.request_type,
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    length: setup.length,
                })
                .await
        } else {
            self.transport
                .control(Control::Out {
                    request_type: setup.request_type,
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    data,
                })
                .await
        }
    }

    /// Read and parse one `GETSTATUS` reply. It leaves the desynchronized flag
    /// alone, because the calling operation owns that flag.
    async fn read_status(&mut self) -> Result<GetStatus> {
        let reply = self.send(dfu::get_status(self.interface), &[]).await?;
        GetStatus::parse(&reply)
    }

    /// Read the device's DFU status once.
    ///
    /// The write and read primitives poll status on their own, so this is a
    /// standalone query. A transport failure leaves the agent desynchronized. The
    /// device's own error *status* is returned in the [`GetStatus`], not as an
    /// error, because a device that answered is still synchronized.
    pub async fn status(&mut self) -> Result<GetStatus> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;
        let status = self.read_status().await?;
        self.desynchronized = false;
        Ok(status)
    }

    /// Clear a `dfuERROR` and return the device to `dfuIDLE`.
    ///
    /// This is the in-protocol recovery from a device-reported error. It cannot
    /// recover a [`Desynchronized`](Error::Desynchronized) agent. There the host has
    /// lost its place in the conversation, and only reopening the device fixes that.
    /// On a desynchronized agent it therefore returns the error without sending
    /// anything.
    pub async fn clear_status(&mut self) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;
        self.send(dfu::clear_status(self.interface), &[]).await?;
        self.desynchronized = false;
        Ok(())
    }

    /// Send one write block and wait until the device is ready for the next.
    ///
    /// `block_num` is the DFU block counter the caller increments per block. `data`
    /// is at most [`transfer_size`](Self::transfer_size) bytes. A longer `data` is a
    /// caller error, refused before anything is sent.
    ///
    /// After the `DNLOAD`, this polls the device with `GETSTATUS` until it reports
    /// `dfuDNLOAD-IDLE`. It waits out each `bwPollTimeout` a busy device sets, so the
    /// host never runs ahead of a slow erase. If the device reports an error status,
    /// the block is refused with the device's own reason, and the agent stays
    /// recoverable. A transport failure finishes the agent.
    pub async fn download_block(&mut self, block_num: u16, data: &[u8]) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        if data.len() > self.functional.transfer_size as usize {
            return Err(Error::InvalidRequest(format!(
                "a DFU download block is {} bytes, over the device's {}-byte transfer size",
                data.len(),
                self.functional.transfer_size
            )));
        }
        self.desynchronized = true;

        self.send(
            dfu::download(self.interface, block_num, data.len() as u16),
            data,
        )
        .await?;

        for _ in 0..DOWNLOAD_BLOCK_POLLS {
            let status = self.read_status().await?;
            if status.status.is_error() {
                // The device answered and named its own reason: the conversation is
                // intact and `clear_status` can recover it, so this is a reported
                // failure, not a loss of the thread.
                self.desynchronized = false;
                return Err(Error::CommandFailed {
                    command: "DFU download",
                    status: status.status as u8,
                });
            }
            match status.state {
                // Still writing the last block: wait out the device's own timeout,
                // then ask again.
                State::DownloadBusy | State::DownloadSync => {
                    crate::progress::settle(status.poll_timeout_ms).await;
                }
                // Ready for the next block.
                State::DownloadIdle => {
                    self.desynchronized = false;
                    return Ok(());
                }
                other => {
                    // A valid answer, but not a state a download leaves the device
                    // in. It answered, so it is not desynchronized; it is just not
                    // where a write expects it.
                    self.desynchronized = false;
                    return Err(Error::Protocol(format!(
                        "after a DFU download block the device is in {}, not dfuDNLOAD-IDLE",
                        other.name()
                    )));
                }
            }
        }

        // The budget is spent and the device is still busy. It answered every
        // poll, so nothing timed out and nothing is desynchronized -- it simply
        // never finished, and a caller told the truth about that can reopen or
        // power-cycle the board.
        self.desynchronized = false;
        Err(Error::Protocol(format!(
            "the device stayed busy through {DOWNLOAD_BLOCK_POLLS} DFU status polls after a \
             download block without reaching dfuDNLOAD-IDLE"
        )))
    }

    /// End a write: send the zero-length block that signals end-of-download, then
    /// wait out manifestation.
    ///
    /// `block_num` is the next block counter after the last real block. The device
    /// commits the firmware (`dfuMANIFEST`), and this polls until it is done. The
    /// device ends either back at `dfuIDLE`, for a manifestation-tolerant device, or
    /// at `dfuMANIFEST-WAIT-RESET`, for one about to re-enumerate. Both mean the write
    /// finished. A caller that needs the device back on the bus reopens it.
    pub async fn finish_download(&mut self, block_num: u16) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;

        self.send(dfu::download(self.interface, block_num, 0), &[])
            .await?;

        for _ in 0..MANIFEST_POLLS {
            let status = self.read_status().await?;
            if status.status.is_error() {
                self.desynchronized = false;
                return Err(Error::CommandFailed {
                    command: "DFU manifest",
                    status: status.status as u8,
                });
            }
            match status.state {
                State::ManifestSync | State::Manifest => {
                    crate::progress::settle(status.poll_timeout_ms).await;
                }
                State::ManifestWaitReset | State::DfuIdle => {
                    self.desynchronized = false;
                    return Ok(());
                }
                other => {
                    self.desynchronized = false;
                    return Err(Error::Protocol(format!(
                        "after the final DFU download block the device is in {}, not manifesting",
                        other.name()
                    )));
                }
            }
        }

        self.desynchronized = false;
        Err(Error::Protocol(format!(
            "the device stayed in manifestation through {MANIFEST_POLLS} DFU status polls without \
             finishing the commit"
        )))
    }

    /// Read one block back from the current alt-setting.
    ///
    /// `block_num` is the block counter the caller increments per block. The device
    /// returns up to [`transfer_size`](Self::transfer_size) bytes. A short reply, of
    /// fewer bytes down to none, marks the end of the region and is not an error. A
    /// read loop stops on it. Reading back blocks this way makes a DFU board
    /// verifiable, where the StarFive serial path is not.
    pub async fn upload_block(&mut self, block_num: u16) -> Result<Vec<u8>> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;
        let data = self
            .send(
                dfu::upload(self.interface, block_num, self.functional.transfer_size),
                &[],
            )
            .await?;
        self.desynchronized = false;
        Ok(data)
    }

    /// Select an alt-setting, so the next transfer addresses that region.
    ///
    /// This is a standard USB `SET_INTERFACE` request, not a DFU-class one. It
    /// therefore goes through [`Transport::select_alt_setting`] rather than the
    /// [`dfu`] codec. It picks which of the device's alt-settings (its partitions)
    /// the following `UPLOAD` and `DNLOAD` blocks read or write. It also resets the
    /// device's transfer to that region's start.
    ///
    /// The transport decides which request reaches the device, so the request is
    /// not built here. A native transport sends the standard request itself. A
    /// browser requires `selectAlternateInterface()`, so that its own view of the
    /// interface stays current. The seam's default is the standard request, so the
    /// scripted transport in this module's tests sees the bytes the native
    /// transport sends.
    ///
    /// How a real U-Boot DFU gadget expects an alt-setting to be selected is
    /// **\[UNVERIFIED\]**. `SET_INTERFACE` is the standard mechanism and the
    /// conservative assumption. A transport failure finishes the agent, as it does
    /// for any other interrupted command.
    async fn select_alt(&mut self, alt: u8) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;
        self.transport
            .select_alt_setting(self.interface, alt)
            .await?;
        self.desynchronized = false;
        Ok(())
    }

    /// Fill `buf` with the region an LBA names, streaming DFU `UPLOAD` blocks.
    ///
    /// It is the [`FlashAgent`]-facing read, over the alt-setting addressing the
    /// [`dfu_alt`] codec packs into the LBA. The high bits name an alt-setting, and
    /// the low bits a block offset within it, as [`dfu_alt::split_lba`] describes.
    /// [`sector_size`](Self::sector_size) is the transfer size, so `buf.len()` must
    /// be a whole number of DFU blocks, one sector each.
    ///
    /// DFU `UPLOAD` reads sequentially from a region's start, and so does this
    /// method. A read at an alt-setting boundary (block offset zero) selects the
    /// alt-setting and starts the block counter at zero. A read that continues
    /// where the last one ended carries the counter on.
    /// [`dump`](crate::verbs::dump) and [`verify`](crate::verbs::verify) read in
    /// exactly this window-by-window pattern.
    ///
    /// A read that starts inside a region without continuing the previous one is
    /// **refused**, rather than served wrong bytes. Whether a board honors an
    /// arbitrary starting block is **\[UNVERIFIED\]**. Every DFU device supports
    /// the sequential model.
    ///
    /// A short block marks the end of the region. It is the protocol's own signal,
    /// and the device returns to `dfuIDLE` on sending it. The rest of `buf` is
    /// zero-filled, and the read stops without asking for another block. The verbs
    /// pad a partition's final sector regardless. A request after the end of the
    /// region stalls a strict device. A device that does not check its block
    /// counter instead returns a silent second copy of the region's start.
    ///
    /// The sequence ends with the short block, so the next read must begin at a
    /// region's start.
    pub async fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }

        let block_bytes = self.functional.transfer_size as usize;
        if block_bytes == 0 {
            return Err(Error::Protocol(
                "the device advertised a DFU transfer size of zero, so it can address no blocks"
                    .to_string(),
            ));
        }
        if !buf.len().is_multiple_of(block_bytes) {
            return Err(Error::InvalidRequest(format!(
                "a DFU read of {} bytes is not a whole number of {block_bytes}-byte blocks",
                buf.len()
            )));
        }

        let (alt_wide, offset_sectors) = dfu_alt::split_lba(lba);
        let alt = u8::try_from(alt_wide)
            .ok()
            .filter(|alt| usize::from(*alt) < self.alts.len())
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "LBA {lba} names DFU alt-setting {alt_wide}, past the {} this device exposes",
                    self.alts.len()
                ))
            })?;

        if offset_sectors == 0 {
            // The start of a region: select it -- which resets the device's
            // transfer -- and begin the block counter at zero.
            self.select_alt(alt).await?;
            self.read_cursor = Some(ReadCursor {
                alt,
                next_block: 0,
                next_lba: lba,
            });
        } else {
            // Inside a region: only a read continuing the last one can be served,
            // because the counter is where the sequence left it.
            let continues = matches!(&self.read_cursor, Some(cursor) if cursor.alt == alt && cursor.next_lba == lba);
            if !continues {
                return Err(Error::InvalidRequest(format!(
                    "a DFU read starting inside alt-setting {alt} (LBA {lba}) does not continue the \
                     previous read. A DFU region is read sequentially from its start"
                )));
            }
        }

        // Established either way by here. Taken out so the `UPLOAD` calls below can
        // borrow the agent; restored when the read completes, and dropped if one
        // fails -- in which case the agent is finished and the cursor is moot.
        let mut cursor = self.read_cursor.take().expect("a cursor is set above");
        let mut filled = 0;
        while filled < buf.len() {
            let slot = &mut buf[filled..filled + block_bytes];
            let data = self.upload_block(cursor.next_block).await?;
            let got = data.len().min(slot.len());
            slot[..got].copy_from_slice(&data[..got]);

            // A **short** block is the protocol's end-of-region signal, and the
            // device returns to `dfuIDLE` on sending one. So the rest of the buffer
            // is zero and the loop stops here: asking for the next block after
            // end-of-region is at best a stall, and on a device that does not check
            // the block counter it is the start of a *fresh* upload of the same
            // region, handed back as though it were the continuation. The cursor is
            // dropped with it -- there is no sequence left to continue, so the next
            // read has to start a region over, which is the rule this agent already
            // holds callers to.
            if got < block_bytes {
                buf[filled + got..].fill(0);
                return Ok(());
            }

            cursor.next_block = cursor.next_block.wrapping_add(1);
            cursor.next_lba += 1;
            filled += block_bytes;
        }
        self.read_cursor = Some(cursor);
        Ok(())
    }

    /// Write `data` to the region an LBA names, streaming DFU `DNLOAD` blocks.
    ///
    /// It is the [`FlashAgent`]-facing write, and the mirror of [`read`](Self::read).
    /// It uses the same alt-setting-packed LBA, the same block-sized sectors, and
    /// the same sequential rule. A write at an alt-setting boundary selects the
    /// alt-setting, and opens a download session at block zero. A write that
    /// continues where the last one ended carries the counter on.
    ///
    /// Any other write is **refused**. A device that does not honor a non-zero
    /// starting block would silently write to the region's start instead.
    /// **\[UNVERIFIED\]**
    ///
    /// **This method commits nothing.** A DFU device holds a download until the
    /// zero-length block that ends it. This method leaves the session open, and
    /// [`finish_write`](Self::finish_write) commits it. The write path therefore
    /// cannot read back the window it just sent. Until the session finishes there is
    /// nothing on the flash to read, and the device serves no `UPLOAD` mid-download.
    pub async fn write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        if !self.functional.can_download() {
            return Err(Error::NotImplemented(
                "the DFU interface does not advertise DNLOAD, so this device does not accept a \
                 write at all",
            ));
        }

        let block_bytes = self.functional.transfer_size as usize;
        if block_bytes == 0 {
            return Err(Error::Protocol(
                "the device advertised a DFU transfer size of zero, so it can address no blocks"
                    .to_string(),
            ));
        }
        if !data.len().is_multiple_of(block_bytes) {
            return Err(Error::InvalidRequest(format!(
                "a DFU write of {} bytes is not a whole number of {block_bytes}-byte blocks",
                data.len()
            )));
        }

        let (alt_wide, offset_sectors) = dfu_alt::split_lba(lba);
        let alt = u8::try_from(alt_wide)
            .ok()
            .filter(|alt| usize::from(*alt) < self.alts.len())
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "LBA {lba} names DFU alt-setting {alt_wide}, past the {} this device exposes",
                    self.alts.len()
                ))
            })?;

        if offset_sectors == 0 {
            // The start of a region: select it, which resets the device's transfer,
            // and open the session at block zero. A session already open on another
            // region is not silently continued -- that would commit one region's
            // blocks into another -- so it is refused, and the refusal names the two
            // ways out. The write path takes the second of them on its own, in
            // [`verbs::flash`]'s unwind; a caller driving the agent directly has
            // both.
            //
            // [`verbs::flash`]: crate::verbs::flash
            if let Some(open) = &self.write_cursor
                && open.alt != alt
            {
                return Err(Error::InvalidRequest(format!(
                    "a DFU download is still open on alt-setting {}, holding blocks the device \
                     has not been told to commit. Commit it with `finish_write`, or abandon it \
                     with `abandon_download`, before writing alt-setting {alt}",
                    open.alt
                )));
            }
            self.select_alt(alt).await?;
            self.write_cursor = Some(WriteCursor {
                alt,
                next_block: 0,
                next_lba: lba,
                start_lba: lba,
            });
        } else {
            let continues = matches!(&self.write_cursor, Some(cursor) if cursor.alt == alt && cursor.next_lba == lba);
            if !continues {
                return Err(Error::InvalidRequest(format!(
                    "a DFU write starting inside alt-setting {alt} (LBA {lba}) does not continue \
                     the open download. A DFU region is written sequentially from its start"
                )));
            }
        }

        let mut cursor = self.write_cursor.take().expect("a cursor is set above");
        let mut sent = 0;
        while sent < data.len() {
            let block = &data[sent..sent + block_bytes];
            // A failed block leaves the cursor dropped: the session is in an unknown
            // place and the next write must start a region over rather than continue
            // from a counter nobody can trust.
            self.download_block(cursor.next_block, block).await?;
            cursor.next_block = cursor.next_block.wrapping_add(1);
            cursor.next_lba += 1;
            sent += block_bytes;
        }
        self.write_cursor = Some(cursor);
        Ok(())
    }

    /// Commit the open download and report the region it wrote.
    ///
    /// It sends the zero-length block that ends the session, and waits out
    /// manifestation ([`finish_download`](Self::finish_download)). The returned
    /// range is the LBA the session began at, and how many sectors it carried. The
    /// caller reads that range back, so the region checked is the one the device was
    /// sent, not one recomputed from the plan.
    ///
    /// With no download open, it returns `None`, so a second commit is harmless
    /// rather than an error. A write path that finished and then unwinds therefore
    /// does not fail on the way out.
    pub async fn finish_write(&mut self) -> Result<Option<(u64, u64)>> {
        let Some(cursor) = self.write_cursor.as_ref() else {
            return Ok(None);
        };
        let start_lba = cursor.start_lba;
        let sectors = cursor.next_lba - cursor.start_lba;
        let next_block = cursor.next_block;

        // The cursor is dropped only once the commit has actually happened. A
        // device that answers the zero-length block with an error status leaves the
        // agent recoverable but the session uncommitted, and that is exactly the
        // state [`has_open_download`](Self::has_open_download) exists to name -- so
        // dropping the cursor first would have the agent report a write that ended
        // where one stopped partway.
        self.finish_download(next_block).await?;
        self.write_cursor = None;
        Ok(Some((start_lba, sectors)))
    }

    /// Abandon an open download without committing it.
    ///
    /// `DFU_ABORT` is the protocol's own way to end a session without finishing it.
    /// The device drops what it is holding and returns to `dfuIDLE`. The next write
    /// begins at a region's start with a fresh block counter. A write that failed
    /// partway calls this, so the agent it failed on is usable again. Without it the
    /// session stays open, and a later write to another region is refused until the
    /// device is reopened.
    ///
    /// The blocks already sent are not un-written. The host cannot know what the
    /// device did with them before the failure. This abandons the *session*, which
    /// is the part the host can fix.
    ///
    /// With no download open, it does nothing, so an unwind can call it
    /// unconditionally. It cannot help a [`Desynchronized`](Error::Desynchronized)
    /// agent, because only reopening the device can. On such an agent it drops the
    /// cursor and returns that error, without sending a request.
    pub async fn abandon_download(&mut self) -> Result<()> {
        if self.write_cursor.take().is_none() {
            return Ok(());
        }
        if self.desynchronized {
            return Err(Error::Desynchronized);
        }
        self.desynchronized = true;
        self.send(dfu::abort(self.interface), &[]).await?;
        self.desynchronized = false;
        Ok(())
    }

    /// Whether a download is open and uncommitted.
    ///
    /// The caller uses this to tell a write that ended from one that stopped
    /// partway. An open session means the device was sent blocks it has not been
    /// told to commit.
    pub fn has_open_download(&self) -> bool {
        self.write_cursor.is_some()
    }
}

#[cfg(any(test, feature = "testing"))]
impl<T: Transport> DfuAgent<T> {
    /// The agent's transport, so a test can assert that the scripted conversation
    /// ran to the end. It is behind the `testing` feature, like [`RockusbAgent`]'s.
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

/// The uniform block interface, dispatched over the closed backend set. A further
/// backend is added as a new variant.
pub enum FlashAgent<T: Transport> {
    /// The Rockchip rockusb backend.
    Rockusb(RockusbAgent<T>),
    /// The USB DFU 1.1 backend: an Ingenic board once a DFU-capable U-Boot runs on
    /// it. It reads (`dump`, `verify`, `partitions`) over `UPLOAD`. Its write is
    /// built and refused at the gate, because no Ingenic SoC is pinned. The
    /// addressing has not been checked against hardware. [`DfuAgent`] describes it.
    Dfu(DfuAgent<T>),
    /// A raw block device the host operating system also owns.
    ///
    /// Every other backend in the set talks to a board in a bootstrap mode. There
    /// pyrographer owns the wire, and a read-back is the device's answer. This one
    /// shares its device with the kernel. It holds the device `O_EXCL`, so nothing
    /// else can write to it. It reads back uncached, so the answer comes from the
    /// device rather than from a page the host holds. [`crate::block`] describes it.
    ///
    /// It carries no transport type parameter, because it has no transport: the
    /// device is a file. Only its constructor is native, so on `wasm32` the variant
    /// adds only a branch that is never taken.
    Block(BlockAgent),
}

impl<T: Transport> FlashAgent<T> {
    /// The logical sector size this backend addresses in.
    ///
    /// It is known without asking the device, so a caller can size a buffer before
    /// running any command.
    pub fn sector_size(&self) -> u32 {
        match self {
            FlashAgent::Rockusb(_) => rockusb::SECTOR_SIZE,
            FlashAgent::Dfu(agent) => agent.sector_size(),
            // A property of the device rather than a constant of the protocol:
            // a 4Kn disk addresses in 4096-byte blocks and a 512e one in 512.
            FlashAgent::Block(agent) => agent.sector_size(),
        }
    }

    /// Whether this agent is desynchronized from its device, and therefore refuses
    /// every further command.
    ///
    /// A GUI holds an agent across errors, so [`Error::Desynchronized`] is a state
    /// of the connection as well as a failure. This method reports a finished
    /// connection as soon as it is finished, rather than at the next command. A CLI
    /// has no need to ask, because an error ends the process.
    ///
    /// Nothing recovers the agent in place. The device must be reopened or
    /// replugged, as the hint on [`Error::Desynchronized`] says.
    pub fn is_desynchronized(&self) -> bool {
        match self {
            FlashAgent::Rockusb(agent) => agent.is_desynchronized(),
            FlashAgent::Dfu(agent) => agent.is_desynchronized(),
            FlashAgent::Block(agent) => agent.is_desynchronized(),
        }
    }

    /// Report flash geometry.
    pub async fn info(&mut self) -> Result<FlashInfo> {
        match self {
            FlashAgent::Rockusb(agent) => agent.info().await,
            // DFU has no geometry command; what is known comes from the
            // descriptors, so this needs no device read.
            FlashAgent::Dfu(agent) => Ok(agent.info()),
            // Read from sysfs before the device was opened, unprivileged. The
            // open would have failed if it had gone stale underneath.
            FlashAgent::Block(agent) => Ok(agent.info()),
        }
    }

    /// The running loader's raw answer to which SoC it is on.
    ///
    /// The wrong-loader refusal rests on this answer. A Rockchip loader is a program
    /// bootstrapped onto a board, and only its own answer says which SoC it runs
    /// on. A loader for another SoC enumerates, answers, and writes to the wrong
    /// offsets, with a plausible status behind every command. The USB descriptors
    /// cannot catch it, because a product ID names a family, and the family list is
    /// not exhaustive.
    ///
    /// The bytes are returned uninterpreted, deliberately. The wrong-loader gate
    /// compares the whole reply with pinned bytes and never decodes it, so a parser
    /// here would add only a guess. A person pinning a new board reads the raw
    /// reply, and the armed gate checks the same raw reply. An RK3576's reply is
    /// pinned in [`soc`](crate::soc).
    ///
    /// A DFU board and a block device have no loader to ask, and return no bytes.
    pub async fn chip_version(&mut self) -> Result<Vec<u8>> {
        match self {
            FlashAgent::Rockusb(agent) => agent.chip_version().await,
            // A DFU board has no chip-version query: DFU has no such command, and
            // the SoC identity was read once during bootstrap (VR_GET_CPU_INFO) on
            // a device that has since re-enumerated. So it answers with **no
            // bytes**, which is honest -- there is nothing to ask -- and keeps the
            // write path uniform: a plan is built and rendered like any other, and
            // an empty reply matches no pinned SoC, so the wrong-loader gate refuses
            // it exactly as it refuses a stranger. DFU writes stay refused at the
            // gate (no Ingenic SoC is pinned), and this is never the thing that
            // refuses them.
            FlashAgent::Dfu(_) => Ok(Vec::new()),
            // There is no loader. A block device is not a board running a
            // program that might be for the wrong SoC -- it is storage the host
            // is already addressing, and the question the wrong-loader gate asks
            // has no counterpart here. What replaces it is the refusal of every
            // device the running system rests on, made before the device is
            // opened and with no override; see [`crate::block::write_refusal`].
            FlashAgent::Block(_) => Ok(Vec::new()),
        }
    }

    /// The running loader's own report of what it can do.
    ///
    /// A rockusb loader answers this. No other backend in the set has an
    /// equivalent, so the others return `None` rather than an error. `None` means
    /// that this kind of device makes no such claim about itself.
    pub async fn capability(&mut self) -> Result<Option<rockusb::Capability>> {
        match self {
            FlashAgent::Rockusb(agent) => agent.capability().await.map(Some),
            // DFU describes itself in its descriptors and its functional
            // descriptor, not through a capability command. What is knowable is
            // already known by the time an agent exists.
            FlashAgent::Dfu(_) => Ok(None),
            FlashAgent::Block(_) => Ok(None),
        }
    }

    /// Which storage medium the backend is currently addressing.
    ///
    /// A rockusb loader can be pointed at any of several media, and answers which
    /// one is current. A DFU board cannot. Its alt-settings *are* the regions, each
    /// named by the device's own U-Boot, so it has no single current medium to
    /// report. A block device returns `None` too.
    pub async fn storage_medium(&mut self) -> Result<Option<rockusb::StorageMedium>> {
        match self {
            FlashAgent::Rockusb(agent) => agent.storage_medium().await.map(Some),
            FlashAgent::Dfu(_) => Ok(None),
            // The medium is the device, and the operating system has already
            // named it. There is no second one to be pointed at.
            FlashAgent::Block(_) => Ok(None),
        }
    }

    /// Fill `buf` with sectors starting at logical block address `lba`.
    ///
    /// `buf.len()` must be a whole number of [`sector_size`](Self::sector_size)
    /// sectors. The backend splits the request into as many commands as the
    /// protocol needs, and the caller sees one read.
    pub async fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            FlashAgent::Rockusb(agent) => agent.read(lba, buf).await,
            FlashAgent::Dfu(agent) => agent.read(lba, buf).await,
            FlashAgent::Block(agent) => agent.read_at(lba, buf),
        }
    }

    /// Write `data` starting at logical block address `lba`.
    ///
    /// `data.len()` must be a whole number of [`sector_size`](Self::sector_size)
    /// sectors. The backend splits the request into as many commands as the
    /// protocol needs, and the caller sees one write.
    ///
    /// **This is the raw block interface, and it carries no gate.** It checks no
    /// confirmation, no loader, and no read-back of what landed in the flash.
    /// [`verbs::flash`] makes those checks, and a caller writes through it. It takes
    /// a `ConfirmedWrite` that only a plan can produce, and it reads back every
    /// window it writes. This method exists because that verb calls it.
    ///
    /// [`verbs::flash`]: crate::verbs::flash
    pub async fn write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        match self {
            FlashAgent::Rockusb(agent) => agent.write(lba, data).await,
            // The DFU write streams `DNLOAD` blocks into an open session and
            // commits nothing; `finish_write` is what commits. See
            // [`ReadBack::AfterCommit`], which is the consequence for the verb.
            FlashAgent::Dfu(agent) => agent.write(lba, data).await,
            FlashAgent::Block(agent) => agent.write_at(lba, data),
        }
    }

    /// Commit a write that the backend holds uncommitted, and report the region.
    ///
    /// On rockusb, it is a no-op returning `None`, because the write is on the
    /// flash by the time the command returns. On a block device, it flushes the
    /// device's own write cache, which `O_DIRECT` does not bypass, and returns
    /// `None`. Each window was already read back as it landed. On DFU, it sends the
    /// zero-length block that ends the download, and waits out manifestation. The
    /// returned `(lba, sectors)` is the region the device was sent. Under
    /// [`ReadBack::AfterCommit`], the write path reads that region back, rather
    /// than a range recomputed from the plan.
    ///
    /// It is safe to call with nothing open, so a write path unwinding after a
    /// failure does not fail again on the way out.
    pub async fn finish_write(&mut self) -> Result<Option<(u64, u64)>> {
        match self {
            FlashAgent::Rockusb(_) => Ok(None),
            FlashAgent::Dfu(agent) => agent.finish_write().await,
            // `O_DIRECT` puts a write past the host's page cache but not past
            // the device's own. This is what makes it durable rather than merely
            // submitted -- and it returns no region, because the read-back
            // already happened window by window.
            FlashAgent::Block(agent) => agent.flush().map(|()| None),
        }
    }

    /// Whether this backend is holding a write it has not been told to commit.
    ///
    /// Only DFU can. Its download is a session, and the device holds the blocks
    /// sent in a session until the zero-length block that ends it. rockusb and a
    /// block device have no session, so neither holds a write that a failure would
    /// leave uncommitted.
    ///
    /// The caller uses this to tell a write that ended from one that stopped
    /// partway. [`abandon_write`](Self::abandon_write) handles the second case.
    pub fn holds_uncommitted_write(&self) -> bool {
        match self {
            FlashAgent::Rockusb(_) | FlashAgent::Block(_) => false,
            FlashAgent::Dfu(agent) => agent.has_open_download(),
        }
    }

    /// Abandon a held write without committing it, so the agent is usable again.
    ///
    /// It is the counterpart of [`finish_write`](Self::finish_write), for a write
    /// that is not going to finish. On a backend that holds nothing, it does
    /// nothing, so a write path unwinding after a failure can call it
    /// unconditionally. On DFU, it sends `DFU_ABORT`.
    ///
    /// It does not un-write the bytes already sent, and nothing can. The caller
    /// keeps the error it was unwinding from, because an abandoned session is not a
    /// successful write.
    pub async fn abandon_write(&mut self) -> Result<()> {
        match self {
            FlashAgent::Rockusb(_) | FlashAgent::Block(_) => Ok(()),
            FlashAgent::Dfu(agent) => agent.abandon_download().await,
        }
    }

    /// When this backend can prove that a write landed.
    ///
    /// [`ReadBack`] describes the answers. The write path asks only here, rather
    /// than matching on the backend, so one write loop serves every protocol.
    pub fn read_back(&self) -> ReadBack {
        match self {
            FlashAgent::Rockusb(_) => ReadBack::PerWindow,
            FlashAgent::Dfu(agent) => {
                let functional = agent.functional();
                if !functional.can_upload() {
                    ReadBack::Impossible(
                        "the DFU interface does not advertise UPLOAD, so a write to it cannot be \
                         read back and checked",
                    )
                } else if !functional.manifestation_tolerant() {
                    ReadBack::Impossible(
                        "the DFU interface is not manifestation-tolerant. The device leaves the \
                         bus when it commits a write, so the write cannot be read back through \
                         the connection that made it",
                    )
                } else {
                    ReadBack::AfterCommit
                }
            }
            // A block device is readable at any moment, so each window is
            // checked before the next goes out. That this is honest rests on
            // the open: `O_DIRECT` means the read is answered by the device
            // rather than by a page the host is holding, and `O_EXCL` means
            // nothing else is writing underneath. Neither is optional, and
            // without them a read-back here would confirm a write that a
            // mounted filesystem later overwrites -- measured, not feared.
            FlashAgent::Block(_) => ReadBack::PerWindow,
        }
    }

    /// How far this backend can address, and why it stops there.
    ///
    /// [`AddressCeiling`] describes the answer. The write plan asks only here, once,
    /// before any geometry check. A range the wire cannot carry is therefore refused
    /// at the plan, with the backend's real reason. Without this check, the agent's
    /// own range check would fail mid-write, with earlier windows already
    /// overwritten.
    pub fn address_ceiling(&self) -> AddressCeiling {
        match self {
            // The `K_FW_LBA_READ_10`/`K_FW_LBA_WRITE_10` command blocks carry the
            // LBA in four bytes, which is what `addressable` enforces per command.
            FlashAgent::Rockusb(_) => AddressCeiling {
                past_last: PAST_LAST_SECTOR,
                why: "the 32-bit LBA the rockusb command block carries cannot address it",
            },
            // The DFU agent has no flat address space of its own: an LBA names an
            // alt-setting in its high bits and a block within it in the low
            // `ALT_LBA_SHIFT`. A `SET_INTERFACE` alternate is one byte, so
            // alt-setting 255 is the last, and its slice ends the space.
            FlashAgent::Dfu(_) => AddressCeiling {
                past_last: dfu_alt::PAST_LAST_LBA,
                why: "no DFU alt-setting's slice of the packed address space reaches it",
            },
            // A block device is addressed by byte offset in a `u64`, so nothing
            // but the device's own sector count bounds it -- and that check is
            // the next one the plan makes.
            FlashAgent::Block(_) => AddressCeiling {
                past_last: u64::MAX,
                why: "no sector address can name it",
            },
        }
    }

    /// Erase `n` sectors starting at logical block address `lba`.
    ///
    /// Every backend in the set refuses it with [`Error::NotImplemented`].
    pub async fn erase(&mut self, _lba: u64, _n: u64) -> Result<()> {
        match self {
            FlashAgent::Rockusb(_) => Err(Error::NotImplemented("rockusb erase")),
            FlashAgent::Dfu(_) => Err(Error::NotImplemented(
                "DFU has no whole-region erase that pyrographer drives. The device erases as \
                 needed during a download",
            )),
            FlashAgent::Block(_) => Err(Error::NotImplemented(
                "a block device has no erase to drive: the block layer presents storage that is \
                 always writable, and a write overwrites it directly",
            )),
        }
    }

    /// End the session in the given [`ResetMode`].
    ///
    /// The mode is a rockusb notion, that protocol's `K_FW_RESET` subcode, and DFU
    /// has nothing to map it onto. A DFU board therefore refuses every mode with
    /// [`Error::NotImplemented`], rather than silently doing the one thing it could.
    /// A block device refuses too, because it has no session to end.
    pub async fn reset(&mut self, mode: ResetMode) -> Result<()> {
        match self {
            FlashAgent::Rockusb(agent) => agent.reset(mode).await,
            FlashAgent::Dfu(_) => Err(Error::NotImplemented(
                "DFU reset is not implemented. Power-cycle or detach a DFU board instead of \
                 resetting it through pyrographer",
            )),
            FlashAgent::Block(_) => Err(Error::NotImplemented(
                "a block device has no reset: it is storage, not a board in a bootstrap mode, and \
                 there is no session to end",
            )),
        }
    }

    /// Report what this backend supports.
    ///
    /// A front-end disables verbs on the strength of this, so it claims only what
    /// the backend does. Every backend in this set reads, and every one exposes a
    /// partition table. [`partition::read`] finds a rockusb or block device's table
    /// by reading sectors, and takes a DFU board's from its alt-settings. The
    /// rockusb backend does not erase. Its opcode is published, as
    /// [`rockusb::Opcode`] records, but no board has confirmed what the command does
    /// to a range.
    ///
    /// `has_partitions` says the backend can *look* for a table. It does not promise
    /// that the board has one. [`partition::read`] returns `Ok(None)` for a device
    /// with no table, which is a finding rather than a failure.
    ///
    /// [`partition::read`]: crate::partition::read
    pub fn caps(&self) -> Caps {
        match self {
            FlashAgent::Rockusb(_) => Caps {
                can_erase: false,
                has_partitions: true,
                medium: None,
                // The loader reads, so it verifies -- and it reads the whole flash
                // by LBA, so a raw-LBA dump and a clone span the device.
                can_verify: true,
                can_address_raw_lba: true,
            },
            // DFU reads (so it verifies: `verify` reads and compares, and a write
            // reads back), and it exposes a partition table -- its alt-settings.
            // But it addresses a *named alt-setting*, not a device-wide LBA, so a
            // raw-LBA dump or a clone names nothing it can serve; a front-end grays
            // those out on `can_address_raw_lba`. Erase is the device's own during
            // a download, not a verb here.
            FlashAgent::Dfu(_) => Caps {
                can_erase: false,
                has_partitions: true,
                medium: None,
                can_verify: true,
                can_address_raw_lba: false,
            },
            // It reads, so it verifies, and it addresses the whole device by
            // LBA, so a raw dump and a clone both mean something. `can_verify`
            // carries its ordinary meaning here only because the open held the
            // device exclusively and uncached; see [`crate::block`].
            FlashAgent::Block(agent) => agent.caps(),
        }
    }

    /// The DFU alt-settings this backend exposes, or `None` for a backend without
    /// them.
    ///
    /// It is the DFU partition source. A DFU board's partitions are its
    /// alt-settings, which the device reports as interface strings rather than
    /// storing them on the flash. [`partition::read`] therefore takes the table from
    /// here, rather than probing sectors for a GPT or a Rockchip parameter block. A
    /// backend whose table is on the flash returns `None`, and [`partition::read`]
    /// probes for that table instead. [`crate::codec::dfu_alt`] describes the
    /// format.
    ///
    /// [`partition::read`]: crate::partition::read
    pub fn dfu_alt_settings(&self) -> Option<&[AltSetting]> {
        match self {
            FlashAgent::Rockusb(_) => None,
            FlashAgent::Dfu(agent) => Some(agent.alt_settings()),
            // Its table is on the device, where the probe belongs.
            FlashAgent::Block(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::bot::CBW_LEN;
    use crate::testing::{cbw, cbw_with_subcode, csw_failed, csw_passed, csw_passed_with_residue};
    use crate::transport::testing::{ScriptedTransport, Step};

    /// The exact 31 bytes an LBA read must put on the wire.
    ///
    /// The bytes are spelled out rather than built with the codec, so the test pins
    /// the format independently of it. A sign error in the CBW or the command block
    /// then fails here, rather than canceling out.
    #[test]
    fn an_lba_read_puts_exactly_these_bytes_on_the_wire() {
        let cdb = rockusb::build_cdb(Opcode::LbaRead, 0x0000_0064, 32);
        let cbw = bot::build_cbw(1, 16384, Direction::In, &cdb[..Opcode::LbaRead.cdb_len()]);
        assert_eq!(
            cbw,
            [
                0x55, 0x53, 0x42, 0x43, // "USBC"
                0x01, 0x00, 0x00, 0x00, // tag 1, little-endian
                0x00, 0x40, 0x00, 0x00, // 16384 bytes, little-endian
                0x80, // IN
                0x00, // LUN 0
                0x0a, // bCBWCBLength: ten, as the reference tools declare
                0x14, // K_FW_LBA_READ_10
                0x00, // subcode
                0x00, 0x00, 0x00, 0x64, // LBA 100, big-endian
                0x00, // reserved
                0x00, 0x20, // 32 sectors, big-endian
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // zeroed tail
            ]
        );
    }

    /// An eleven-byte flash-info payload describing a 64 GB-class eMMC.
    fn flash_info_payload() -> Vec<u8> {
        let mut payload = vec![0u8; rockusb::FLASH_INFO_LEN];
        payload[0..4].copy_from_slice(&122_142_720u32.to_le_bytes()); // sectors
        payload[4..6].copy_from_slice(&1024u16.to_le_bytes()); // block size
        payload[6] = 4; // page size
        payload[8] = 40; // access time
        payload
    }

    #[test]
    fn info_issues_read_flash_id_then_read_flash_info() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0x45, 0x4d, 0x4d, 0x43, 0x20]),
            csw_passed(1),
            cbw(2, 11, Direction::In, Opcode::ReadFlashInfo, 0, 0),
            Step::Reply(flash_info_payload()),
            csw_passed(2),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let info = pollster::block_on(agent.info()).expect("info should succeed");

        assert_eq!(info.size_bytes, 122_142_720 * 512);
        assert_eq!(info.sector_size, 512);
        assert_eq!(
            info.chip_id.as_deref(),
            Some(&[0x45, 0x4d, 0x4d, 0x43, 0x20][..])
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// A command whose data phase times out finishes the agent.
    ///
    /// It does so as any other interrupted conversation does. A caller holding the
    /// agent must stop using it, rather than send the next command to a
    /// desynchronized device. The scripted `Step::Timeout` is the USB counterpart
    /// of the serial one. It lets this ending be pinned in the agent with no board.
    /// Without it, no test outside the transport could reach this ending.
    #[test]
    fn a_command_that_times_out_finishes_the_agent() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Timeout, // the device never answers the read
        ]);
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));

        let error = pollster::block_on(agent.info()).expect_err("the read timed out");
        assert!(matches!(error, Error::Timeout { .. }), "{error:?}");
        assert!(
            agent.is_desynchronized(),
            "a timed-out command leaves the agent finished, not merely failed"
        );

        // Every command after it refuses before anything reaches the wire.
        let next =
            pollster::block_on(agent.chip_version()).expect_err("the agent is finished for good");
        assert!(matches!(next, Error::Desynchronized), "{next:?}");
    }

    #[test]
    fn capability_decodes_the_loaders_own_answer() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 8, Direction::In, Opcode::ReadCapability, 0, 0),
            // direct LBA (0x01) + read LBA (0x08), and switch-storage in byte 1.
            Step::Reply(vec![0x09, 0x02, 0, 0, 0, 0, 0, 0]),
            csw_passed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let caps = pollster::block_on(agent.capability())
            .expect("capability should succeed")
            .expect("a rockusb loader answers");

        assert!(caps.direct_lba());
        assert!(caps.read_lba());
        assert!(caps.switch_storage());
        assert!(!caps.vendor_storage());
        assert_eq!(caps.unnamed_bits(), [0u8; rockusb::CAPABILITY_LEN]);

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    #[test]
    fn storage_medium_reports_what_the_loader_is_addressing() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 4, Direction::In, Opcode::GetStorageMedia, 0, 0),
            Step::Reply(vec![0x02, 0x00, 0x00, 0x00]), // bit 1: eMMC
            csw_passed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let medium = pollster::block_on(agent.storage_medium())
            .expect("get storage media should succeed")
            .expect("a rockusb loader answers");
        assert_eq!(medium, rockusb::StorageMedium::Emmc);
    }

    /// A DFU board asked the same two questions returns no answer, rather than
    /// failing. The verbs are uniform across backends, and only the answers differ.
    #[test]
    fn a_dfu_board_makes_no_capability_or_medium_claim() {
        let mut agent = dfu_flash_agent(vec![], vec![]);
        assert!(
            pollster::block_on(agent.capability())
                .expect("asking is fine")
                .is_none()
        );
        assert!(
            pollster::block_on(agent.storage_medium())
                .expect("asking is fine")
                .is_none()
        );
    }

    /// A device that under-reports the bytes it sent is refused.
    ///
    /// It sends every byte it announced and reports success, but its CSW says it
    /// moved fewer. A caller told only "success" would keep bytes the device itself
    /// says are not an answer. This is the residue counterpart of the fill check.
    /// The fill check catches false data that looks plausible. This one catches a
    /// device whose own numbers contradict its data.
    #[test]
    fn a_full_data_phase_with_a_nonzero_residue_is_refused() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0x45, 0x4d, 0x4d, 0x43, 0x20]),
            csw_passed_with_residue(1, 2),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let error = pollster::block_on(agent.info()).expect_err("the accounting disagrees");
        match &error {
            Error::Protocol(message) => {
                assert!(message.contains("moved 5 bytes"), "{message}");
                assert!(message.contains("only 3"), "{message}");
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }

        // The CSW arrived and answers this command, so both ends are still at a
        // command boundary: this is a bad reply, not a lost connection.
        assert!(
            !agent.is_desynchronized(),
            "a disagreement about the transfer does not tear the conversation"
        );
    }

    /// A read that comes back full while the device reports a shortfall is refused.
    ///
    /// This is the case the residue check exists for. A window of flash arrives
    /// looking complete, and the device's own numbers say it did not serve it.
    #[test]
    fn a_read_the_device_reports_as_short_is_refused_even_when_it_looks_complete() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 4 * 512, Direction::In, Opcode::LbaRead, 65536, 4),
            // A full window of plausible-looking bytes...
            Step::Reply(vec![0xcc; 4 * 512]),
            // ...that the device says it did not actually serve.
            csw_passed_with_residue(1, 4 * 512),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let mut buf = vec![0u8; 4 * 512];
        let error = pollster::block_on(agent.read(65536, &mut buf))
            .expect_err("the device disowned the window");
        assert!(
            matches!(&error, Error::Protocol(message) if message.contains("under-reports")),
            "{error:?}"
        );
    }

    #[test]
    fn a_residue_larger_than_the_request_is_refused() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0x45, 0x4d, 0x4d, 0x43, 0x20]),
            csw_passed_with_residue(1, 9),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let error = pollster::block_on(agent.info()).expect_err("nine of five is nonsense");
        assert!(
            matches!(&error, Error::Protocol(message) if message.contains("more than it was asked")),
            "{error:?}"
        );
    }

    /// A short data phase is legitimate only for the command whose reply length is
    /// unknown. A device that stops short, and reports the shortfall consistently,
    /// is answering rather than failing.
    #[test]
    fn a_short_reply_whose_residue_matches_it_is_accepted() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 16, Direction::In, Opcode::GetChipVer, 0, 0),
            Step::Reply(vec![0x36, 0x37, 0x35, 0x33]),
            csw_passed_with_residue(1, 12),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let version =
            pollster::block_on(agent.chip_version()).expect("a short reply is the answer");
        assert_eq!(version, vec![0x36, 0x37, 0x35, 0x33]);
    }

    /// The same short reply with the residue left at zero is still the answer.
    ///
    /// That is a loader that never maintains the field, which the reference tools
    /// tolerate because they ignore it entirely. Tolerating this direction costs
    /// nothing, because the length is already checked directly wherever it is
    /// known.
    #[test]
    fn a_short_reply_from_a_loader_that_ignores_residue_is_still_the_answer() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 16, Direction::In, Opcode::GetChipVer, 0, 0),
            Step::Reply(vec![0x36, 0x37, 0x35, 0x33]),
            csw_passed_with_residue(1, 0),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let version =
            pollster::block_on(agent.chip_version()).expect("a careless residue is not a failure");
        assert_eq!(version, vec![0x36, 0x37, 0x35, 0x33]);
    }

    #[test]
    fn read_splits_a_request_into_chunks_the_loader_accepts() {
        // 40 sectors is more than MAX_CHUNK_SECTORS, so it must go out as a
        // 32-sector command followed by an 8-sector one, with the LBA advanced.
        let chunk_bytes = MAX_CHUNK_SECTORS as usize * 512;
        let transport = ScriptedTransport::new(vec![
            cbw(
                1,
                chunk_bytes as u32,
                Direction::In,
                Opcode::LbaRead,
                100,
                32,
            ),
            Step::Reply(vec![0xaa; chunk_bytes]),
            csw_passed(1),
            cbw(2, 8 * 512, Direction::In, Opcode::LbaRead, 132, 8),
            Step::Reply(vec![0xbb; 8 * 512]),
            csw_passed(2),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let mut buf = vec![0u8; 40 * 512];
        pollster::block_on(agent.read(100, &mut buf)).expect("read should succeed");

        assert!(buf[..chunk_bytes].iter().all(|&b| b == 0xaa));
        assert!(buf[chunk_bytes..].iter().all(|&b| b == 0xbb));

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// The destructive command, on the wire.
    ///
    /// The CBW announces data going out rather than coming in, the direction bit
    /// is clear, and the bytes follow the command block. The script asserts each
    /// of these, because a malformed write cannot be recovered once it reaches the
    /// device.
    #[test]
    fn a_write_announces_its_bytes_outbound_and_then_sends_them() {
        let data = vec![0x5a; 8 * 512];
        let transport = ScriptedTransport::new(vec![
            cbw(1, 8 * 512, Direction::Out, Opcode::LbaWrite, 100, 8),
            Step::ExpectWrite(data.clone()),
            csw_passed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        pollster::block_on(agent.write(100, &data)).expect("write should succeed");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// A write longer than one command splits exactly as a read does.
    ///
    /// Each chunk is 32 sectors, the LBA advances with each chunk, and each chunk's
    /// bytes follow its own command block. A chunk that carried the wrong slice of
    /// the image would put the right bytes at the wrong offset, and no CSW would
    /// report it.
    #[test]
    fn a_write_splits_into_chunks_the_loader_accepts_and_advances_the_lba() {
        let chunk_bytes = MAX_CHUNK_SECTORS as usize * 512;
        let head = vec![0xaa; chunk_bytes];
        let tail = vec![0xbb; 8 * 512];
        let mut data = head.clone();
        data.extend_from_slice(&tail);

        let transport = ScriptedTransport::new(vec![
            cbw(
                1,
                chunk_bytes as u32,
                Direction::Out,
                Opcode::LbaWrite,
                100,
                32,
            ),
            Step::ExpectWrite(head),
            csw_passed(1),
            cbw(2, 8 * 512, Direction::Out, Opcode::LbaWrite, 132, 8),
            Step::ExpectWrite(tail),
            csw_passed(2),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        pollster::block_on(agent.write(100, &data)).expect("write should succeed");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// A write the protocol cannot address is refused before any byte goes out.
    ///
    /// The script is empty, so a command that reached the transport would panic.
    /// That proves nothing was overwritten before the refusal. For a read, refusing
    /// early only keeps things tidy. For a write, it separates a failed operation
    /// from a partly overwritten flash.
    #[test]
    fn a_write_the_protocol_cannot_address_is_refused_before_any_io() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));

        let error = pollster::block_on(agent.write(0, &[0u8; 100]))
            .expect_err("100 bytes is not a whole sector");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let error = pollster::block_on(agent.write(0xffff_ffff, &[0u8; 2 * 512]))
            .expect_err("the second sector is past the end of the address space");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The two LBA opcodes differ by one byte, and the direction bit by one bit.
    /// Read and write share the range check and the chunking, so this test pins
    /// that they still differ in the opcode and the direction bit.
    #[test]
    fn a_read_and_a_write_of_the_same_range_differ_only_where_they_should() {
        let read = cbw(1, 512, Direction::In, Opcode::LbaRead, 64, 1);
        let write = cbw(1, 512, Direction::Out, Opcode::LbaWrite, 64, 1);

        let (Step::ExpectWrite(read), Step::ExpectWrite(write)) = (read, write) else {
            unreachable!("cbw builds an expected write");
        };
        assert_eq!(read[12], 0x80, "a read's data phase comes in");
        assert_eq!(write[12], 0x00, "a write's data phase goes out");
        assert_eq!(read[15], 0x14);
        assert_eq!(write[15], 0x15);
        // Same tag, same length, same LBA, same sector count: everything else is
        // the same command.
        assert_eq!(read[..12], write[..12]);
        assert_eq!(read[13..15], write[13..15]);
        assert_eq!(read[16..], write[16..]);
    }

    #[test]
    fn a_failing_status_surfaces_as_a_named_command_failure() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 5]),
            csw_failed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let error = pollster::block_on(agent.info()).expect_err("the device failed the command");

        assert!(matches!(
            error,
            Error::CommandFailed {
                command: "read flash ID",
                status: 1
            }
        ));
    }

    #[test]
    fn a_mismatched_csw_tag_is_a_protocol_error() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 5]),
            csw_passed(99), // the device answered a command we never sent
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let error = pollster::block_on(agent.info()).expect_err("the tag does not match");
        assert!(matches!(error, Error::Protocol(_)));
    }

    /// The request itself is wrong, and no protocol malfunctioned, so the error is
    /// `InvalidRequest`. The script is empty, so a command sent before the check
    /// would panic the transport.
    #[test]
    fn a_read_that_is_not_a_whole_number_of_sectors_is_refused_before_any_io() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        let mut buf = [0u8; 100];
        let error = pollster::block_on(agent.read(0, &mut buf)).expect_err("100 is not a sector");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    #[test]
    fn reset_treats_a_device_that_reboots_before_the_csw_as_success() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 0, Direction::Out, Opcode::Reset, 0, 0),
            Step::Disconnect,
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        pollster::block_on(agent.reset(ResetMode::Reset))
            .expect("a reboot mid-command is the command working");
    }

    /// Every mode, through the whole stack rather than the codec alone.
    ///
    /// The scripted transport expects a CBW that [`cbw_with_subcode`] builds from
    /// the mode number. The test therefore pins that the mode a caller names is
    /// the byte the device is sent. The four modes differ only in that byte, and it
    /// decides whether a board comes back or powers off.
    #[test]
    fn each_reset_mode_puts_its_subcode_on_the_wire() {
        for (mode, subcode) in [
            (ResetMode::Reset, 0x00u8),
            (ResetMode::MassStorage, 0x01),
            (ResetMode::PowerOff, 0x02),
            (ResetMode::Maskrom, 0x03),
        ] {
            let transport = ScriptedTransport::new(vec![
                cbw_with_subcode(1, 0, Direction::Out, Opcode::Reset, subcode, 0, 0),
                csw_passed(1),
            ]);

            let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
            pollster::block_on(agent.reset(mode)).unwrap_or_else(|error| {
                panic!("{} should have been accepted: {error:?}", mode.name())
            });
        }
    }

    /// A device that leaves the bus mid-command means the command worked. That
    /// holds for every mode, including the modes meant to take the device off the
    /// bus. A board that powers off has no CSW to send.
    #[test]
    fn a_device_that_leaves_under_any_mode_is_the_mode_working() {
        for mode in ResetMode::ALL {
            let transport = ScriptedTransport::new(vec![
                cbw_with_subcode(1, 0, Direction::Out, Opcode::Reset, mode as u8, 0, 0),
                Step::Disconnect,
            ]);

            let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
            pollster::block_on(agent.reset(mode))
                .unwrap_or_else(|error| panic!("{} left the bus: {error:?}", mode.name()));
        }
    }

    #[test]
    fn a_command_with_no_data_phase_announces_no_data() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 0, Direction::Out, Opcode::TestUnitReady, 0, 0),
            csw_passed(1),
        ]);

        let mut agent = RockusbAgent::new(transport);
        pollster::block_on(agent.test_unit_ready()).expect("the loader is ready");
        agent.transport.assert_drained();
    }

    /// The chip-version command, on the wire.
    ///
    /// The command asks for [`CHIP_VER_LEN`] bytes and returns exactly what came
    /// in, uninterpreted. The reply here has the shape a board is expected to answer
    /// with. The test makes no claim about what the bytes mean, because the command
    /// makes none.
    ///
    /// [`CHIP_VER_LEN`]: rockusb::CHIP_VER_LEN
    #[test]
    fn chip_version_asks_the_loader_and_hands_back_what_it_said() {
        let answer: Vec<u8> = (0..rockusb::CHIP_VER_LEN as u8).collect();
        let transport = ScriptedTransport::new(vec![
            cbw(
                1,
                rockusb::CHIP_VER_LEN as u32,
                Direction::In,
                Opcode::GetChipVer,
                0,
                0,
            ),
            Step::Reply(answer.clone()),
            csw_passed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let version = pollster::block_on(agent.chip_version()).expect("the loader answered");

        assert_eq!(version, answer);
        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// A short chip-version reply is a measurement, not a failure.
    ///
    /// Sixteen bytes is what the reference tools ask for, and what the RK3576 loader
    /// answers. A loader on another SoC can have fewer to give, and has then answered
    /// the question rather than failed the command. The bytes come back with their
    /// count, and the agent stays synchronized, as the second command proves.
    ///
    /// Compare `a_short_data_phase_leaves_the_agent_refusing_every_later_command`.
    /// Where the reply's length is part of the command's definition, a short reply
    /// is a protocol error and the agent is finished. Where the length is unknown, a
    /// short reply is the answer.
    #[test]
    fn a_chip_version_shorter_than_was_asked_for_is_the_answer_not_a_failure() {
        let transport = ScriptedTransport::new(vec![
            cbw(
                1,
                rockusb::CHIP_VER_LEN as u32,
                Direction::In,
                Opcode::GetChipVer,
                0,
                0,
            ),
            // Four of the sixteen bytes the host asked for, and a CSW that says
            // the command passed: a device ending a data phase early, which BOT
            // allows and this command has to be able to hear.
            Step::Reply(vec![0x38, 0x38, 0x35, 0x33]),
            csw_passed(1),
            cbw(2, 0, Direction::Out, Opcode::TestUnitReady, 0, 0),
            csw_passed(2),
        ]);

        let mut agent = RockusbAgent::new(transport);
        let version = pollster::block_on(agent.chip_version()).expect("four bytes is an answer");
        assert_eq!(version, [0x38, 0x38, 0x35, 0x33]);

        pollster::block_on(agent.test_unit_ready())
            .expect("a short data phase the protocol allows leaves the conversation intact");
        agent.transport.assert_drained();
    }

    /// A long chip-version reply is refused, though a short one is accepted.
    ///
    /// A loader that sends more than was asked for leaves the host with bytes it
    /// cannot account for. The surplus would be misread as the next command's
    /// reply. It is therefore an error here, as it is everywhere else, and the agent
    /// that received it is finished.
    #[test]
    fn a_chip_version_longer_than_was_asked_for_is_still_a_protocol_error() {
        let transport = ScriptedTransport::new(vec![
            cbw(
                1,
                rockusb::CHIP_VER_LEN as u32,
                Direction::In,
                Opcode::GetChipVer,
                0,
                0,
            ),
            Step::Reply(vec![0xcc; rockusb::CHIP_VER_LEN + 1]),
        ]);

        let mut agent = RockusbAgent::new(transport);
        let error = pollster::block_on(agent.chip_version()).expect_err("the loader over-answered");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");

        let error = pollster::block_on(agent.test_unit_ready())
            .expect_err("the host cannot say how many bytes the device still holds");
        assert!(matches!(error, Error::Desynchronized), "{error:?}");
    }

    #[test]
    fn the_cbw_is_the_length_the_specification_says() {
        assert_eq!(
            bot::build_cbw(
                1,
                0,
                Direction::Out,
                &rockusb::build_cdb(Opcode::Reset, 0, 0)
            )
            .len(),
            CBW_LEN
        );
    }

    /// The last sector the protocol can name is `0xFFFF_FFFF`, and a read can end
    /// on it. Each chunk's LBA is derived from the base rather than from an
    /// advancing counter. That read therefore returns the bytes it fetched, instead
    /// of filling the buffer and then failing on an increment nothing uses.
    #[test]
    fn a_read_ending_on_the_last_addressable_sector_succeeds() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 512, Direction::In, Opcode::LbaRead, 0xffff_ffff, 1),
            Step::Reply(vec![0xab; 512]),
            csw_passed(1),
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        let mut buf = [0u8; 512];
        pollster::block_on(agent.read(0xffff_ffff, &mut buf))
            .expect("the last sector is addressable, so a read may end on it");

        assert!(buf.iter().all(|&b| b == 0xab));
        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport.assert_drained();
    }

    /// A read one sector further cannot finish. The script is empty, so any command
    /// sent before the range check makes the transport panic. That proves the range
    /// is checked before the first command rather than partway through.
    #[test]
    fn a_read_running_past_the_last_sector_is_refused_before_any_io() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        let mut buf = [0u8; 2 * 512];
        let error = pollster::block_on(agent.read(0xffff_ffff, &mut buf))
            .expect_err("the second sector is past the end of the address space");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A front-end disables a button on `Caps`, so `Caps` must say exactly what
    /// pressing the button makes the agent do. [`verbs::erase_refusal`] must give
    /// the reason on the same terms, because a front-end disables the button on one
    /// and prints the other beside it.
    ///
    /// [`verbs::erase_refusal`]: crate::verbs::erase_refusal
    #[test]
    fn caps_claims_nothing_the_agent_will_refuse() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        let caps = agent.caps();

        let refused = matches!(
            pollster::block_on(agent.erase(0, 1)),
            Err(Error::NotImplemented(_))
        );
        assert_eq!(
            caps.can_erase, !refused,
            "caps.can_erase must say exactly what erase does"
        );
        assert_eq!(
            caps.can_erase,
            crate::verbs::erase_refusal(&agent).is_none(),
            "a backend that will not erase must be able to say why"
        );
        // The backend can look for a table, which is all this claims: finding a
        // table is reading sectors, and reading sectors is what this backend is.
        // Whether a given board *has* one is the board's answer, and
        // `partition::read` gives it as `Ok(None)`.
        assert!(caps.has_partitions);
    }

    /// A data phase that comes back short leaves the device holding bytes the host
    /// cannot count, at least the CSW it still intends to send. The agent has no
    /// way back to a command boundary, so it refuses every later command. It
    /// refuses before anything goes on the wire. The script is exhausted, so a
    /// second command that reached the transport would panic instead of returning
    /// the asserted error.
    #[test]
    fn a_short_data_phase_leaves_the_agent_refusing_every_later_command() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 3]), // three of the five bytes it announced
        ]);

        let mut agent = RockusbAgent::new(transport);
        let error = pollster::block_on(agent.info()).expect_err("the data phase came back short");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");

        let error = pollster::block_on(agent.test_unit_ready())
            .expect_err("the agent fell out of step and cannot be trusted with another command");
        assert!(matches!(error, Error::Desynchronized), "{error:?}");
        agent.transport.assert_drained();
    }

    /// A caller that holds an agent across errors, as a GUI does, can ask whether
    /// the connection is finished before it sends another command. The answer
    /// matches what the next command would return, and no command is sent. The
    /// script is exhausted, so a command that reached the transport here would
    /// panic.
    #[test]
    fn an_agent_that_has_fallen_out_of_step_says_so_when_it_is_asked() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 3]), // three of the five bytes it announced
        ]);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
        assert!(!agent.is_desynchronized(), "a fresh agent is in step");

        pollster::block_on(agent.info()).expect_err("the data phase came back short");
        assert!(agent.is_desynchronized(), "and this one is finished");

        let error = pollster::block_on(agent.info())
            .expect_err("which is what it says when it is asked to do anything");
        assert!(matches!(error, Error::Desynchronized), "{error:?}");
    }

    /// The same holds for a CSW that answers a command nobody sent. That CSW
    /// belongs to another exchange, and the CSW for this command can still be on
    /// its way.
    #[test]
    fn a_csw_answering_the_wrong_command_leaves_the_agent_refusing_every_later_command() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 5]),
            csw_passed(99),
        ]);

        let mut agent = RockusbAgent::new(transport);
        pollster::block_on(agent.info()).expect_err("the tag does not match");

        let error = pollster::block_on(agent.test_unit_ready())
            .expect_err("a CSW for another command means the host has lost the thread");
        assert!(matches!(error, Error::Desynchronized), "{error:?}");
        agent.transport.assert_drained();
    }

    /// A failed command does *not* poison the agent.
    ///
    /// A device reporting that a command failed has answered it. All three phases
    /// ran, the CSW is the one this command asked for, and both ends are back at a
    /// command boundary. Refusing to continue would turn every legitimate refusal
    /// from the device into a dead connection.
    #[test]
    fn a_command_the_device_reports_as_failed_leaves_the_agent_usable() {
        let transport = ScriptedTransport::new(vec![
            cbw(1, 5, Direction::In, Opcode::ReadFlashId, 0, 0),
            Step::Reply(vec![0; 5]),
            csw_failed(1),
            cbw(2, 0, Direction::Out, Opcode::TestUnitReady, 0, 0),
            csw_passed(2),
        ]);

        let mut agent = RockusbAgent::new(transport);
        let error = pollster::block_on(agent.info()).expect_err("the device failed the command");
        assert!(matches!(error, Error::CommandFailed { .. }), "{error:?}");
        assert!(
            !agent.is_desynchronized(),
            "the device answered; both ends are back at a command boundary"
        );

        pollster::block_on(agent.test_unit_ready())
            .expect("a command the device answered leaves the conversation intact");
        agent.transport.assert_drained();
    }

    /// A device that sends more than was asked for has desynchronized from the
    /// host, and trimming the surplus would hide that. The reply here opens with
    /// thirteen bytes that form a valid passing CSW. A transport that trimmed would
    /// report success, and leave the leftover bytes to be misread as the next
    /// command's answer. The surplus must be the error, not the bytes before it.
    #[test]
    fn a_device_that_answers_with_more_than_was_asked_for_is_a_protocol_error() {
        let Step::Reply(mut reply) = csw_passed(1) else {
            unreachable!("csw_passed builds a reply");
        };
        assert_eq!(reply.len(), bot::CSW_LEN);
        reply.extend_from_slice(&[0xff; 7]); // seven bytes nobody asked for

        let transport = ScriptedTransport::new(vec![
            cbw(1, 0, Direction::Out, Opcode::TestUnitReady, 0, 0),
            Step::Reply(reply),
        ]);

        let mut agent = RockusbAgent::new(transport);
        let error =
            pollster::block_on(agent.test_unit_ready()).expect_err("the device over-answered");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    }

    // --- DFU agent ---
    //
    // The DFU backend rides control transfers, so its conversations are scripted
    // with `ExpectControlOut` (DNLOAD, CLRSTATUS) and `ExpectControlIn` (GETSTATUS,
    // UPLOAD), and the expected SETUP fields come from the `dfu` codec's own
    // builders -- so a test pins the agent's sequencing against the same bytes the
    // codec pins.

    /// The DFU interface and transfer size these tests drive.
    const DFU_IFACE: u16 = 0;
    const DFU_XFER: u16 = 64;

    fn dfu_agent(steps: Vec<Step>) -> DfuAgent<ScriptedTransport> {
        DfuAgent::new(
            ScriptedTransport::new(steps),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            Vec::new(),
        )
    }

    /// The `SET_INTERFACE` a DFU read issues to select an alt-setting, as a
    /// scripted control-OUT.
    ///
    /// It is a standard request (type `0x01`, request `0x0b`) that carries the
    /// alt-setting in `wValue` and the interface in `wIndex`.
    fn set_interface(alt: u16) -> Step {
        Step::ExpectControlOut {
            request_type: 0x01,
            request: 0x0b,
            value: alt,
            index: DFU_IFACE,
            data: Vec::new(),
        }
    }

    /// A DFU `FlashAgent` over `alts` and a scripted conversation.
    fn dfu_flash_agent(alts: Vec<AltSetting>, steps: Vec<Step>) -> FlashAgent<ScriptedTransport> {
        FlashAgent::Dfu(DfuAgent::new(
            ScriptedTransport::new(steps),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            alts,
        ))
    }

    /// The `ExpectControlOut` for a DFU write request built by `setup`.
    fn dfu_out(setup: dfu::Setup, data: Vec<u8>) -> Step {
        Step::ExpectControlOut {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            data,
        }
    }

    /// The `ExpectControlIn` for a DFU read request, answered with `reply`.
    fn dfu_in(setup: dfu::Setup, reply: Vec<u8>) -> Step {
        Step::ExpectControlIn {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
            reply,
        }
    }

    /// A 6-byte GETSTATUS reply carrying `status` and `state`, with a zero poll
    /// timeout so a scripted poll waits no real time.
    fn dfu_status(status: dfu::Status, state: State) -> Vec<u8> {
        GetStatus {
            status,
            poll_timeout_ms: 0,
            state,
            string_index: 0,
        }
        .to_bytes()
        .to_vec()
    }

    /// A DFU write sends a `DNLOAD` per block, and polls `GETSTATUS` until the
    /// device is ready for the next, waiting out a busy device. It ends with the
    /// zero-length block and manifestation. The whole sequence is pinned against the
    /// codec's own request bytes.
    #[test]
    fn a_dfu_write_sends_blocks_polls_each_ready_then_manifests() {
        let block0 = vec![0x11; DFU_XFER as usize];
        let block1 = vec![0x22; 10]; // a short final block

        let steps = vec![
            // Block 0: written, then a busy status, then ready.
            dfu_out(
                dfu::download(DFU_IFACE, 0, block0.len() as u16),
                block0.clone(),
            ),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::DownloadBusy),
            ),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::DownloadIdle),
            ),
            // Block 1: written, ready straight away.
            dfu_out(
                dfu::download(DFU_IFACE, 1, block1.len() as u16),
                block1.clone(),
            ),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::DownloadIdle),
            ),
            // Finish: the zero-length block, then manifestation to idle.
            dfu_out(dfu::download(DFU_IFACE, 2, 0), Vec::new()),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::Manifest),
            ),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::DfuIdle),
            ),
        ];

        let mut agent = dfu_agent(steps);
        pollster::block_on(async {
            agent.download_block(0, &block0).await.expect("block 0");
            agent.download_block(1, &block1).await.expect("block 1");
            agent.finish_download(2).await.expect("manifest");
        });
        assert!(!agent.is_desynchronized());
        agent.transport().assert_drained();
    }

    /// A device that reports an error status has the block refused with its own
    /// reason. Because it *answered*, it stays synchronized, and `clear_status`
    /// recovers it in place. rockusb has no such in-protocol recovery.
    #[test]
    fn a_dfu_download_error_is_reported_and_the_agent_stays_recoverable() {
        let block = vec![0x11; 8];
        let steps = vec![
            dfu_out(
                dfu::download(DFU_IFACE, 0, block.len() as u16),
                block.clone(),
            ),
            dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Write, State::DfuError),
            ),
            // The recovery: CLRSTATUS returns the device to idle.
            dfu_out(dfu::clear_status(DFU_IFACE), Vec::new()),
        ];

        let mut agent = dfu_agent(steps);
        let err = pollster::block_on(agent.download_block(0, &block))
            .expect_err("the device reported errWRITE");
        assert!(
            matches!(
                err,
                Error::CommandFailed {
                    command: "DFU download",
                    status: 3 // errWRITE
                }
            ),
            "{err:?}"
        );
        assert!(
            !agent.is_desynchronized(),
            "a device that answered is still in step"
        );

        pollster::block_on(agent.clear_status()).expect("CLRSTATUS recovers a reported error");
        agent.transport().assert_drained();
    }

    /// A DFU read returns blocks until a short one, with fewer bytes than the
    /// transfer size, signals the end of the region. The short block is the end,
    /// not an error.
    #[test]
    fn a_dfu_upload_reads_blocks_until_a_short_one_ends_the_region() {
        let full = vec![0xaa; DFU_XFER as usize];
        let tail = vec![0xbb; 12];
        let steps = vec![
            dfu_in(dfu::upload(DFU_IFACE, 0, DFU_XFER), full.clone()),
            dfu_in(dfu::upload(DFU_IFACE, 1, DFU_XFER), tail.clone()),
        ];

        let mut agent = dfu_agent(steps);
        let (b0, b1) = pollster::block_on(async {
            let b0 = agent.upload_block(0).await.expect("block 0");
            let b1 = agent.upload_block(1).await.expect("block 1");
            (b0, b1)
        });
        assert_eq!(b0, full);
        assert_eq!(b1, tail);
        assert!(
            b1.len() < DFU_XFER as usize,
            "a short block is the end of the region"
        );
        agent.transport().assert_drained();
    }

    /// A transport failure during the status poll leaves the host unable to say
    /// where the device is, so the agent is finished. A device-reported error, by
    /// contrast, is recoverable. Every later command is refused before anything is
    /// sent.
    #[test]
    fn a_dfu_download_whose_status_poll_times_out_finishes_the_agent() {
        let block = vec![0x11; 8];
        let steps = vec![
            dfu_out(
                dfu::download(DFU_IFACE, 0, block.len() as u16),
                block.clone(),
            ),
            Step::Timeout, // GETSTATUS never answers
        ];

        let mut agent = dfu_agent(steps);
        let err = pollster::block_on(agent.download_block(0, &block))
            .expect_err("the status poll timed out");
        assert!(matches!(err, Error::Timeout { .. }), "{err:?}");
        assert!(
            agent.is_desynchronized(),
            "a torn poll leaves the agent finished, not merely failed"
        );

        let next = pollster::block_on(agent.status()).expect_err("the agent is finished for good");
        assert!(matches!(next, Error::Desynchronized), "{next:?}");
    }

    /// A block larger than the device's transfer size is a caller error, refused
    /// before any byte is sent. The script is empty, so a transfer that reached the
    /// transport would panic.
    #[test]
    fn a_dfu_download_block_over_the_transfer_size_is_refused_before_any_io() {
        let mut agent = dfu_agent(vec![]);
        let too_big = vec![0u8; DFU_XFER as usize + 1];
        let err = pollster::block_on(agent.download_block(0, &too_big))
            .expect_err("over the transfer size");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    // --- DFU as a FlashAgent ---

    /// An alt-setting, for the FlashAgent-facing tests.
    fn alt(index: u8, name: &str, size: Option<u64>) -> AltSetting {
        AltSetting {
            index,
            name: name.to_string(),
            size,
        }
    }

    /// The DFU read, over the alt-setting addressing.
    ///
    /// A read at a region's start selects the alt-setting and reads block zero. A
    /// read that continues where the last one ended carries the block counter on
    /// *without* selecting again. That is the window-by-window pattern `dump`
    /// produces. The `SET_INTERFACE` value is the alt-setting the LBA names. A
    /// partition table that placed the region wrong would therefore select the
    /// wrong alt-setting here.
    #[test]
    fn a_dfu_read_selects_the_alt_setting_then_continues_the_block_counter() {
        // Two alt-settings; the read aims at the second, index 1.
        let alts = vec![alt(0, "boot", Some(64)), alt(1, "rootfs", Some(128))];
        let base = dfu_alt::alt_base_lba(1);
        let first = vec![0xaa; DFU_XFER as usize];
        let second = vec![0xbb; DFU_XFER as usize];

        let steps = vec![
            // Window one: select alt 1, then block 0.
            set_interface(1),
            dfu_in(dfu::upload(DFU_IFACE, 0, DFU_XFER), first.clone()),
            // Window two continues: block 1, and no second SET_INTERFACE.
            dfu_in(dfu::upload(DFU_IFACE, 1, DFU_XFER), second.clone()),
        ];

        let mut agent = dfu_flash_agent(alts, steps);
        let mut buf = vec![0u8; DFU_XFER as usize];
        pollster::block_on(agent.read(base, &mut buf)).expect("the first window reads");
        assert_eq!(buf, first);
        pollster::block_on(agent.read(base + 1, &mut buf)).expect("the second window continues");
        assert_eq!(buf, second);

        let FlashAgent::Dfu(agent) = &agent else {
            unreachable!("built a DFU agent")
        };
        agent.transport().assert_drained();
    }

    /// A short block ends the read, and nothing is requested after it.
    ///
    /// The buffer here is three blocks, and the device serves one and a half. A
    /// loop that only zero-filled and continued would ask for block 2 after the
    /// device had returned to `dfuIDLE`. A strict device would stall. A device that
    /// does not check its block counter would return a second copy of the region's
    /// start, as though it were the continuation. The scripted transport is asserted
    /// drained, so an extra `UPLOAD` fails the test rather than being answered.
    #[test]
    fn a_dfu_read_stops_at_the_first_short_block() {
        let alts = vec![alt(0, "data", Some(80))];
        let head = vec![0xcc; DFU_XFER as usize];
        let tail = vec![0xdd; 16];

        let steps = vec![
            set_interface(0),
            dfu_in(dfu::upload(DFU_IFACE, 0, DFU_XFER), head.clone()),
            dfu_in(dfu::upload(DFU_IFACE, 1, DFU_XFER), tail.clone()),
        ];

        let mut agent = dfu_flash_agent(alts, steps);
        let mut buf = vec![0x99; 3 * DFU_XFER as usize];
        pollster::block_on(agent.read(0, &mut buf)).expect("the region reads and then ends");

        assert_eq!(&buf[..64], &head[..]);
        assert_eq!(&buf[64..80], &tail[..]);
        assert!(
            buf[80..].iter().all(|&b| b == 0),
            "everything past the end of the region is zero, not what the buffer held"
        );

        let FlashAgent::Dfu(dfu) = &agent else {
            unreachable!("built a DFU agent")
        };
        dfu.transport().assert_drained();

        // The sequence ended with the short block, so the next read has to start a
        // region over rather than continuing a counter the device has forgotten.
        let error = pollster::block_on(agent.read(2, &mut [0u8; DFU_XFER as usize]))
            .expect_err("there is no sequence left to continue");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A short block, with fewer bytes than the transfer size, is the end of the
    /// region. Its sector is zero-filled rather than left holding whatever the
    /// buffer had. The region here is 80 bytes with a 64-byte block, so the second
    /// block is 16 bytes and the read pads the rest of that sector.
    #[test]
    fn a_dfu_read_zero_fills_a_short_final_block() {
        let alts = vec![alt(0, "data", Some(80))];
        let head = vec![0xcc; DFU_XFER as usize];
        let tail = vec![0xdd; 16];

        let steps = vec![
            set_interface(0),
            dfu_in(dfu::upload(DFU_IFACE, 0, DFU_XFER), head.clone()),
            dfu_in(dfu::upload(DFU_IFACE, 1, DFU_XFER), tail.clone()),
        ];

        let mut agent = dfu_flash_agent(alts, steps);
        let mut buf = vec![0u8; 2 * DFU_XFER as usize];
        pollster::block_on(agent.read(0, &mut buf)).expect("the region reads");

        assert_eq!(&buf[..64], &head[..]);
        assert_eq!(&buf[64..80], &tail[..]);
        assert!(
            buf[80..].iter().all(|&b| b == 0),
            "the short block's sector is zero-padded"
        );

        let FlashAgent::Dfu(agent) = &agent else {
            unreachable!("built a DFU agent")
        };
        agent.transport().assert_drained();
    }

    /// A read whose LBA names an alt-setting the device does not expose is refused
    /// before any transfer. The script is empty, so a `SET_INTERFACE` that reached
    /// the transport would panic. This check stops an out-of-range LBA from
    /// selecting a region that does not exist. A DFU board disables the raw-LBA form
    /// that could produce one.
    #[test]
    fn a_dfu_read_of_an_alt_setting_the_device_does_not_have_is_refused_before_any_io() {
        let alts = vec![alt(0, "only", Some(64))];
        let mut agent = dfu_flash_agent(alts, vec![]);
        let mut buf = vec![0u8; DFU_XFER as usize];
        let err = pollster::block_on(agent.read(dfu_alt::alt_base_lba(5), &mut buf))
            .expect_err("there is no alt-setting 5");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// A read that starts inside a region without continuing the previous read
    /// cannot be served by a sequential upload. It is refused rather than returning
    /// the wrong bytes. The script is empty, so nothing goes out.
    #[test]
    fn a_dfu_read_that_does_not_continue_the_previous_one_is_refused_before_any_io() {
        let alts = vec![alt(0, "region", Some(4 * DFU_XFER as u64))];
        let mut agent = dfu_flash_agent(alts, vec![]);
        let mut buf = vec![0u8; DFU_XFER as usize];
        // Two sectors in, with no prior read to continue.
        let err = pollster::block_on(agent.read(2, &mut buf))
            .expect_err("a mid-region jump is not sequential");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// DFU `info` is not a device read. The sector size is the transfer size, and
    /// the total is the sum of the exposed alt-setting sizes. An alt-setting that
    /// named no range contributes nothing, so a board that names only its
    /// partitions reports what it knows and no more.
    #[test]
    fn dfu_info_reports_the_transfer_size_and_summed_alt_sizes() {
        let alts = vec![
            alt(0, "a", Some(100)),
            alt(1, "b", Some(200)),
            alt(2, "c", None),
        ];
        let mut agent = dfu_flash_agent(alts, vec![]);
        let info = pollster::block_on(agent.info()).expect("info needs no device read");
        assert_eq!(info.sector_size, u32::from(DFU_XFER));
        assert_eq!(info.size_bytes, 300);
        assert_eq!(info.medium, None);
    }

    /// The DFU capabilities, on which a front-end disables buttons:
    ///
    /// - It reads, so it verifies.
    /// - It exposes a partition table, its alt-settings.
    /// - It addresses no raw LBA, so a raw-LBA dump and a clone are disabled.
    /// - It does not erase.
    #[test]
    fn dfu_caps_reads_and_verifies_but_addresses_no_raw_lba() {
        let agent = dfu_flash_agent(vec![alt(0, "boot", Some(64))], vec![]);
        let caps = agent.caps();
        assert!(caps.can_verify, "DFU reads, so it verifies");
        assert!(caps.has_partitions, "its alt-settings are its partitions");
        assert!(
            !caps.can_address_raw_lba,
            "DFU addresses a named alt-setting, not a raw LBA"
        );
        assert!(!caps.can_erase);
        assert!(
            agent.dfu_alt_settings().is_some(),
            "a DFU agent is the partition source for its own table"
        );
    }

    /// Erase and reset refuse on a DFU board, cleanly and without touching the
    /// device. A write is not refused at the agent, because the write path's gate
    /// refuses it. This test pins the operations the agent itself refuses.
    #[test]
    fn dfu_erase_and_reset_still_refuse() {
        let mut agent = dfu_flash_agent(vec![alt(0, "boot", Some(64))], vec![]);
        assert!(matches!(
            pollster::block_on(agent.erase(0, 1)),
            Err(Error::NotImplemented(_))
        ));
        assert!(matches!(
            pollster::block_on(agent.reset(ResetMode::Reset)),
            Err(Error::NotImplemented(_))
        ));
    }

    /// A write to a device whose descriptor does not claim `bitCanDnload` is refused
    /// before any byte goes out. The device has said it does not accept a write, so
    /// the agent does not try one.
    #[test]
    fn a_dfu_device_that_does_not_claim_download_refuses_a_write() {
        let mut agent = DfuAgent::new(
            ScriptedTransport::new(vec![]),
            DFU_IFACE,
            crate::testing::dfu_functional(DFU_XFER, 0b0000_0110), // upload + tolerant, no download
            vec![alt(0, "boot", Some(64))],
        );
        assert!(matches!(
            pollster::block_on(agent.write(0, &[0u8; DFU_XFER as usize])),
            Err(Error::NotImplemented(_))
        ));
    }

    /// The three read-back strategies, decided by the descriptor rather than by the
    /// backend's name. A rockusb board reads back per window. A DFU board that stays
    /// on the bus through manifestation reads back after committing. One that
    /// detaches as it commits cannot be read back at all, and says so.
    #[test]
    fn the_read_back_strategy_comes_from_what_the_device_claims() {
        let rockusb = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        assert_eq!(rockusb.read_back(), ReadBack::PerWindow);

        let tolerant = dfu_flash_agent(vec![alt(0, "boot", Some(64))], vec![]);
        assert_eq!(tolerant.read_back(), ReadBack::AfterCommit);

        let detaches = FlashAgent::Dfu(DfuAgent::new(
            ScriptedTransport::new(vec![]),
            DFU_IFACE,
            crate::testing::dfu_functional(DFU_XFER, crate::testing::DFU_DETACHES_ON_COMMIT),
            vec![alt(0, "boot", Some(64))],
        ));
        let ReadBack::Impossible(why) = detaches.read_back() else {
            panic!("a device that detaches as it commits cannot be read back");
        };
        assert!(why.contains("manifestation-tolerant"), "{why}");

        let no_upload = FlashAgent::Dfu(DfuAgent::new(
            ScriptedTransport::new(vec![]),
            DFU_IFACE,
            crate::testing::dfu_functional(DFU_XFER, 0b0000_0101), // download + tolerant, no upload
            vec![alt(0, "boot", Some(64))],
        ));
        assert!(matches!(no_upload.read_back(), ReadBack::Impossible(_)));
    }

    /// A DFU write streams blocks into an open session and commits nothing until it
    /// is finished. When the write call returns, the region is therefore not on the
    /// flash, so the read-back has to wait. The block counter continues across
    /// windows rather than restarting. `finish_write` reports the region the device
    /// was sent.
    #[test]
    fn a_dfu_write_streams_one_session_and_commits_only_at_the_end() {
        let first = vec![0xa1; DFU_XFER as usize];
        let second = vec![0xb2; DFU_XFER as usize];

        let mut agent = DfuAgent::new(
            ScriptedTransport::new(vec![
                set_interface(0),
                dfu_out(dfu::download(DFU_IFACE, 0, DFU_XFER), first.clone()),
                dfu_in(
                    dfu::get_status(DFU_IFACE),
                    dfu_status(dfu::Status::Ok, State::DownloadIdle),
                ),
                dfu_out(dfu::download(DFU_IFACE, 1, DFU_XFER), second.clone()),
                dfu_in(
                    dfu::get_status(DFU_IFACE),
                    dfu_status(dfu::Status::Ok, State::DownloadIdle),
                ),
                // The zero-length block, and manifestation.
                dfu_out(dfu::download(DFU_IFACE, 2, 0), Vec::new()),
                dfu_in(
                    dfu::get_status(DFU_IFACE),
                    dfu_status(dfu::Status::Ok, State::DfuIdle),
                ),
            ]),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            vec![alt(0, "boot", Some(2 * u64::from(DFU_XFER)))],
        );

        // Two windows, written separately: the counter carries on.
        pollster::block_on(agent.write(0, &first)).expect("the region's first block");
        assert!(
            agent.has_open_download(),
            "nothing is committed until the session is finished"
        );
        pollster::block_on(agent.write(1, &second)).expect("and the second, continuing");

        let committed = pollster::block_on(agent.finish_write()).expect("the session commits");
        assert_eq!(
            committed,
            Some((0, 2)),
            "the region reported is the one the device was told about"
        );
        assert!(!agent.has_open_download());
        assert_eq!(
            pollster::block_on(agent.finish_write()).expect("committing twice is harmless"),
            None,
            "the second commit reports no region"
        );
    }

    /// A commit that fails leaves the session open, and the agent reports it.
    ///
    /// The device answered the zero-length block with an error status. The
    /// conversation is intact, and the blocks it was sent were never committed.
    /// `has_open_download` reports exactly this state. Dropping the cursor first
    /// would report a write that ended where one stopped partway. `DFU_ABORT` then
    /// ends the session, and the write path's unwind sends it.
    #[test]
    fn a_commit_that_fails_leaves_the_download_open_until_it_is_abandoned() {
        let block = vec![0x5a; DFU_XFER as usize];

        let mut agent = DfuAgent::new(
            ScriptedTransport::new(vec![
                set_interface(0),
                dfu_out(dfu::download(DFU_IFACE, 0, DFU_XFER), block.clone()),
                dfu_in(
                    dfu::get_status(DFU_IFACE),
                    dfu_status(dfu::Status::Ok, State::DownloadIdle),
                ),
                // The zero-length block that ends the session, answered with the
                // device's own error rather than manifestation.
                dfu_out(dfu::download(DFU_IFACE, 1, 0), Vec::new()),
                dfu_in(
                    dfu::get_status(DFU_IFACE),
                    dfu_status(dfu::Status::NotDone, State::DfuError),
                ),
                // The way out, and the only one the host has.
                dfu_out(dfu::abort(DFU_IFACE), Vec::new()),
            ]),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            vec![alt(0, "boot", Some(4 * u64::from(DFU_XFER)))],
        );

        pollster::block_on(agent.write(0, &block)).expect("the block goes out");
        let error = pollster::block_on(agent.finish_write())
            .expect_err("the device refused the commit and said why");
        assert!(matches!(error, Error::CommandFailed { .. }), "{error:?}");
        assert!(
            agent.has_open_download(),
            "the blocks were sent and never committed, so the session is still open"
        );

        pollster::block_on(agent.abandon_download()).expect("DFU_ABORT abandons it");
        assert!(!agent.has_open_download());
    }

    /// A device can answer every poll promptly, report that it is still busy, and
    /// ask for no wait. Such a device spins a host that only watches for a state
    /// change. The poll budget stops it. Each poll is a real transfer, so a working
    /// device spends a handful. A device that never finishes is refused, rather than
    /// hanging the job thread where Cancel cannot reach it.
    #[test]
    fn a_device_that_never_leaves_download_busy_spends_its_poll_budget_and_stops() {
        let block = vec![0xc3; DFU_XFER as usize];
        let mut steps = vec![
            set_interface(0),
            dfu_out(dfu::download(DFU_IFACE, 0, DFU_XFER), block.clone()),
        ];
        // One more busy reply than the budget allows, so a loop that ignored the
        // budget would keep going rather than running the script out.
        for _ in 0..=DOWNLOAD_BLOCK_POLLS {
            steps.push(dfu_in(
                dfu::get_status(DFU_IFACE),
                dfu_status(dfu::Status::Ok, State::DownloadBusy),
            ));
        }

        let mut agent = DfuAgent::new(
            ScriptedTransport::new(steps),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            vec![alt(0, "boot", Some(4 * u64::from(DFU_XFER)))],
        );

        let error = pollster::block_on(agent.write(0, &block))
            .expect_err("a device that is busy forever is not one a write can wait out");
        assert!(matches!(error, Error::Protocol(_)), "{error:?}");
        assert!(
            !agent.is_desynchronized(),
            "the device answered every poll, so the conversation is intact"
        );
    }

    /// A DFU region is written sequentially from its start, for the same reason it
    /// is read that way. Whether a device honors a non-zero starting block is
    /// unverified, and a device that ignores one would write to the region's start
    /// instead. A write that starts inside a region without continuing the open
    /// session is therefore refused rather than sent.
    #[test]
    fn a_dfu_write_that_does_not_continue_the_session_is_refused() {
        let mut agent = DfuAgent::new(
            ScriptedTransport::new(vec![]),
            DFU_IFACE,
            crate::testing::dfu_capable(DFU_XFER),
            vec![alt(0, "boot", Some(4 * u64::from(DFU_XFER)))],
        );

        let block = vec![0u8; DFU_XFER as usize];
        let error = pollster::block_on(agent.write(2, &block))
            .expect_err("block 2 of a region nothing has opened");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A DFU board has no chip-version query, so it returns no bytes rather than an
    /// error. A DFU write can therefore be planned and refused at the gate like any
    /// other. It does not fail partway through a survey on a command it lacks. An
    /// empty reply matches no pinned SoC, so the gate stays shut.
    #[test]
    fn dfu_chip_version_answers_with_no_bytes() {
        let mut agent = dfu_flash_agent(vec![alt(0, "boot", Some(64))], vec![]);
        let reply = pollster::block_on(agent.chip_version()).expect("DFU answers, with nothing");
        assert!(reply.is_empty(), "DFU has no chip-version bytes to give");
    }
}
