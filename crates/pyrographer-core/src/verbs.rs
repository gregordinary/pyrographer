//! The verbs: high-level operations written once for every backend.
//!
//! `list` needs no device. Every other verb is generic over the [`Transport`], so
//! one implementation covers every backend in the [`FlashAgent`] enum.
//!
//! The long verbs stream. A Rockchip eMMC can exceed 100 GiB, so [`dump`] moves one
//! window at a time from the device into a caller-supplied sink. [`verify`]
//! compares one window at a time against a caller-supplied source. Neither holds
//! the image in memory. The window is also the unit at which progress is reported
//! and cancellation is checked.
//!
//! A verb therefore consumes two async seams and implements neither. Device bytes
//! cross [`Transport`]. Image bytes cross [`ImageReader`] and [`ImageWriter`],
//! which a file implements under the CLI and a `Blob` under the web flasher.
//! [`crate::image`] explains why that seam is async rather than [`std::io`].
//!
//! ## The gated write path
//!
//! A write to flash runs in four steps: plan, confirmation, write and read-back. A
//! write cannot skip any of them.
//!
//! The plan comes from [`plan_write`], which takes a raw LBA that somebody worked
//! out, or from [`plan_write_partition`], which takes a partition name. Either one
//! asks the device for its geometry, its loader's answer to which SoC it is on, and
//! its partition table. It sends only read commands, and returns a [`WritePlan`].
//! Producing the plan and stopping there is the dry run.
//!
//! The table is read however the range was chosen, so every plan reports which
//! partitions the write would land in. *"The whole of `uboot`"* is a description a
//! person can recognize as wrong, and *"sector 16384"* is not. A name also gives the
//! range an end, so [`plan_write_partition`] refuses an image too big for the
//! partition it names.
//!
//! A front-end can learn whether a write will be refused before it asks a person to
//! confirm one. [`write_refusal`] answers before any plan exists, and
//! [`plan_refusal`] answers for the write a plan describes. On a backend with a
//! loader, this is the wrong-loader gate: the loader's answer must equal the reply
//! pinned for the SoC the caller named.
//!
//! Confirmation is a value. [`WritePlan::confirm`] consumes the plan and returns a
//! [`ConfirmedWrite`], and [`flash`] accepts nothing else. A write nobody agreed to
//! is therefore a compile error, not a matter for code review.
//!
//! [`flash`] makes the same check [`plan_refusal`] makes, through the same function.
//! The refusal a person is shown and the refusal the write makes therefore cannot
//! differ. Just before the first window, it asks the loader for its SoC again and
//! checks that answer too. Only then does it write.
//!
//! Every write is read back, and the read-back cannot be turned off.
//! [`FlashAgent::read_back`] decides when it runs. A per-window read-back stops at
//! the first window that differs, with [`Error::VerifyMismatch`]. A read-back after
//! commit compares a CRC-32 of each window once the device has committed the
//! region, and reports a difference as [`Error::CommitMismatch`]. A backend that
//! cannot read back at all is refused before any byte is written.
//!
//! [`clone`] and [`write_table`] follow the same path with a different source. A
//! clone is planned by [`plan_clone`] and confirmed as a [`ConfirmedClone`]. A table
//! write is planned as a [`SegmentedPlan`] and confirmed as a
//! [`ConfirmedSegmentedWrite`], and [`segmented_refusal`] answers for it. Both write
//! through the same write-then-read-back loop as [`flash`].

use crate::agent::{AddressCeiling, FlashAgent, FlashInfo, ReadBack};
use crate::bootstrap;
use crate::bootstrap::ingenic::IngenicLoader;
use crate::codec::crc::crc32;
use crate::codec::gpt;
use crate::codec::idb::{self, IdbImage};
use crate::codec::ingenic_boot::CpuInfo;
use crate::codec::rkboot::LoaderImage;
use crate::codec::rkfw;
use crate::codec::rkparam;
use crate::codec::rockusb;
#[cfg(not(target_arch = "wasm32"))]
use crate::discovery::{self, DeviceInfo};
use crate::fill::{FillReport, FillScanner};
use crate::firmware::{self, ForwardReader, Package};
use crate::image::{BoxFuture, ImageReader, ImageWriter, SyncReader};
use crate::layout::Layout;
use crate::partition::{
    self, GptRepair, GptRepairDirection, Overlap, ParamRepair, Partition, PartitionTable,
    TableFormat,
};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::soc::Soc;
use crate::transport::Transport;
use crate::{Error, Result};

/// How much a streaming verb moves between progress events and cancellation
/// checks.
///
/// At this size the per-window overhead is negligible against the transfer. A
/// canceled dump still stops promptly, and a progress bar advances often enough to
/// show that the transfer is running.
const WINDOW_BYTES: u64 = 1 << 20;

/// What went wrong with an image, without the sentence around it.
///
/// The two halves of an image failure are known in two places. The image seam
/// knows the cause, such as a file that ran out or a `Blob` that could not be
/// sliced. The verb knows the offset it had reached and the length it was
/// promised. The seam therefore reports the cause alone, the verb writes the
/// sentence, and this function joins the two.
///
/// An error other than [`Error::Io`] came from somewhere other than the image, and
/// is rendered unchanged.
fn cause(err: Error) -> String {
    match err {
        Error::Io(message) => message,
        other => other.to_string(),
    }
}

/// List connected devices in a boot or recovery state.
///
/// It is the one verb that needs no device. It is native-only, because a browser
/// cannot scan a bus, as [`crate::discovery`] explains.
///
/// It scans each vendor in turn and concatenates the results. A Rockchip board and
/// an Ingenic camera on the same host therefore both appear. Each carries the
/// [`Vendor`](crate::discovery::Vendor) that its verbs dispatch on.
#[cfg(not(target_arch = "wasm32"))]
pub fn list() -> Result<Vec<DeviceInfo>> {
    let mut devices = discovery::list_rockchip()?;
    devices.extend(discovery::list_ingenic()?);
    Ok(devices)
}

/// Report flash geometry from a connected device.
pub async fn info<T: Transport>(agent: &mut FlashAgent<T>) -> Result<FlashInfo> {
    agent.info().await
}

/// Ask the running loader what SoC it is on, and return its answer raw.
///
/// This is the evidence the wrong-loader refusal compares, asked on its own. It
/// sends one read command, changes nothing, and returns the bytes as they arrived.
///
/// The reply is returned undecoded, because the gate compares it whole. The gate
/// matches it byte for byte against the reply pinned for a named SoC
/// ([`soc`](crate::soc)), and decodes no fields. The same raw value pins a new
/// SoC: run this against a board whose part is known, and record the answer.
pub async fn chip_version<T: Transport>(agent: &mut FlashAgent<T>) -> Result<Vec<u8>> {
    agent.chip_version().await
}

/// Ask the running loader what it says it can do.
///
/// It is the device's counterpart to [`caps`](FlashAgent::caps). `caps` is
/// pyrographer's account of what a backend implements, and this is the device's
/// account of what it will serve. A backend with nothing to report answers `None`.
///
/// It sends a read command that changes nothing, so any board can be asked. The
/// answer is a report, and no verb refuses on it. [`FlashAgent::capability`]
/// explains why a flag that no board has been seen to set is not grounds for a
/// refusal.
pub async fn capability<T: Transport>(
    agent: &mut FlashAgent<T>,
) -> Result<Option<rockusb::Capability>> {
    agent.capability().await
}

/// Ask which storage medium the backend is currently addressing.
///
/// Every LBA a verb takes is an offset into one medium. On a board with more than
/// one medium populated, the same sector number names more than one place. This is
/// the device's own answer to which one, so a dump can name the medium it read. A
/// backend that addresses no single medium answers `None`.
pub async fn storage_medium<T: Transport>(
    agent: &mut FlashAgent<T>,
) -> Result<Option<rockusb::StorageMedium>> {
    agent.storage_medium().await
}

/// Bring a maskrom board to loader mode by uploading `loader` into it.
///
/// It acts on a bare [`Transport`] rather than a [`FlashAgent`]. A maskrom board
/// has no reachable flash, so there is no agent to hold until this verb makes the
/// flash reachable. Before a byte goes out, it asks [`loader_blob_refusal`] whether
/// the file claims the SoC named in `soc`. It then uploads the loader's 471 and 472
/// sections over the download-boot control transfers, as [`crate::bootstrap`]
/// describes. It reports through `progress`, and stops once `cancel` is set.
///
/// On success, the board re-enumerates in loader mode as a *different* USB device
/// from the one uploaded to. This is verified against a real RK3576 (2026-07-17).
/// The `0x0472` jump tore down the maskrom device, and a loader appeared at a new
/// address about three seconds later.
///
/// The caller finds the loader again with a fresh discovery. A scripted transport
/// cannot model one device leaving the bus and another arriving, so that step is
/// exercised on hardware and not in the tests.
pub async fn download_boot<T: Transport>(
    transport: &mut T,
    loader: &LoaderImage,
    soc: Option<Soc>,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    if let Some(refusal) = loader_blob_refusal(soc, loader) {
        return Err(refusal);
    }
    bootstrap::download_boot(transport, loader, progress, cancel).await
}

/// Why this loader file would be refused for this SoC, or `None`.
///
/// It is asked over the file alone, with no device open. It compares the
/// container's own claim about which SoC it was built for, before a byte goes out.
///
/// It is the only gate the maskrom bootstrap can have, and it is weaker than the
/// write path's. [`plan_refusal`] compares a *loader's* answer against pinned
/// bytes. A maskrom board gives no such answer: it serves no chip-version query,
/// and its SRAM cannot be read back. Once the wrong blob is running, nothing is left
/// to ask.
///
/// The claim was written by whoever built the file. This check therefore catches
/// **the wrong file picked**, the mistake a person makes at a bench with a
/// directory full of `_loader.bin` files. It does not attest to what the file
/// contains, and a file that passes is not thereby known good.
///
/// `None` means the upload goes ahead. It covers three situations, and only the
/// first is a check that passed:
///
/// - The container claims the named SoC.
/// - **No SoC was named.** The upload is then the explicit, ungated act a person
///   chose.
/// - **Nothing can be judged.** A loader built from bare stage files carries no
///   container, and so makes no claim. Or the named SoC has no pinned container
///   sample to compare against.
///
/// [`download_boot`] enforces this answer by calling this function, so what a
/// front-end shows and what the upload does cannot differ.
pub fn loader_blob_refusal(soc: Option<Soc>, loader: &LoaderImage) -> Option<Error> {
    container_claim_refusal(soc, loader.chip)
}

/// [`loader_blob_refusal`] over the container's claim alone, so a firmware plan,
/// which keeps the claim and not the container, judges it the same way.
fn container_claim_refusal(soc: Option<Soc>, chip: Option<[u8; 4]>) -> Option<Error> {
    let soc = soc?;
    let chip = chip?;
    match soc.claimed_by_container(&chip) {
        Some(true) | None => None,
        Some(false) => Some(Error::LoaderBlobMismatch {
            named: soc.name(),
            // `claimed_by_container` answered `Some`, so a pinned value is there.
            expected: soc.container_chip().unwrap_or_default().to_vec(),
            found: chip.to_vec(),
        }),
    }
}

/// Bring an Ingenic XBurst board from its boot ROM to DFU mode by uploading a
/// loader.
///
/// It is the Ingenic counterpart of [`download_boot`], and also acts on a bare
/// [`Transport`] rather than a [`FlashAgent`]. A board in its boot ROM has no
/// reachable flash until a DFU-capable U-Boot runs on it. This verb uploads the
/// loader's two stages over the `VR_*` bootstrap protocol, as
/// [`bootstrap::ingenic`] describes. It reports through `progress`, and stops once
/// `cancel` is set.
///
/// It returns the SoC's identifying magic, read at the one point in the flow where
/// it is readable. The caller records it, and can later pin a `soc` entry from it.
///
/// On success, the board re-enumerates as a DFU gadget, a *different* USB device.
/// The caller finds it again with a fresh discovery, as after a Rockchip loader
/// re-enumerates. The whole upload sequence is **\[UNVERIFIED\]**: it has not yet
/// run on an Ingenic board.
pub async fn ingenic_download_boot<T: Transport>(
    transport: &mut T,
    loader: &IngenicLoader,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<CpuInfo> {
    bootstrap::ingenic::download_boot(transport, loader, progress, cancel).await
}

/// Read `sectors` sectors from `lba` and stream them to `out`.
///
/// It emits [`Progress`] as it goes, and stops between windows once `cancel` is
/// set. A canceled dump therefore leaves the device idle, not partway through a
/// command. The bytes already written to `out` stay written, and the caller decides
/// what to do with a partial image.
///
/// Every window is also scanned for constant fill, and the returned [`FillReport`]
/// holds what the scan found. A large run of one repeated byte with a success
/// status is what a silent read failure looks like. A dump across one looks
/// complete and is not, as [`crate::fill`] explains. The report is empty on a
/// healthy read, and a caller may ignore it.
pub async fn dump<T: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    sectors: u64,
    out: &mut dyn ImageWriter,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<FillReport> {
    let sector_size = u64::from(agent.sector_size());
    // Checked, because `total_bytes` is a promise: `Progress::Started` announces
    // it and a caller renders a percentage against it, so a silent wrap would
    // make the promise a lie before the read failed for reasons of its own. It
    // also bounds every accumulator in the loop below, which is why none of them
    // needs a check of its own.
    let total_bytes = sectors.checked_mul(sector_size).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "{sectors} sectors of {sector_size} bytes is more bytes than a byte count can hold"
        ))
    })?;
    let window_sectors = (WINDOW_BYTES / sector_size).max(1);

    progress(Progress::Started { total_bytes });

    let mut buf = vec![0u8; (window_sectors * sector_size) as usize];
    let mut scanner = FillScanner::new(agent.sector_size());
    let mut at = lba;
    let mut remaining = sectors;
    let mut done_bytes = 0u64;

    while remaining > 0 {
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }

        let n = remaining.min(window_sectors);
        let window = &mut buf[..(n * sector_size) as usize];

        agent.read(at, window).await?;
        scanner.observe(at, window);
        out.write_all(window)
            .await
            .map_err(|e| Error::Io(format!("cannot write the dump: {}", cause(e))))?;

        at += n;
        remaining -= n;
        done_bytes += n * sector_size;
        progress(Progress::Advanced {
            done_bytes,
            total_bytes,
        });
    }

    out.flush()
        .await
        .map_err(|e| Error::Io(format!("cannot flush the dump: {}", cause(e))))?;
    progress(Progress::Finished { done_bytes });
    Ok(scanner.finish())
}

/// Read the device's partition table, or `None` for a device with no table.
///
/// `Ok(None)` is a finding, not a failure. A board holding a raw image, or a blank
/// one, has no table and is not broken. A table that is present and fails
/// validation is [`Error::CorruptTable`], so a damaged table is never reported as
/// an empty list of partitions.
///
/// A GPT with a damaged primary copy and an intact backup is returned recovered,
/// with [`PartitionTable::recovery`] set. The partitions are the backup's, and the
/// recovery note says the primary is damaged. Only a GPT with no intact copy is
/// [`Error::CorruptTable`], as [`partition::read`] describes.
///
/// [`PartitionTable::recovery`]: crate::partition::PartitionTable::recovery
///
/// Only read commands are sent.
pub async fn partitions<T: Transport>(agent: &mut FlashAgent<T>) -> Result<Option<PartitionTable>> {
    let flash = agent.info().await?;
    partition::read(agent, &flash).await
}

/// The partition called `name`, from the device's own table.
///
/// The name is matched exactly, for the reason [`PartitionTable::find`] gives. A
/// device with no table cannot resolve a name. Unlike [`partitions`], this function
/// therefore returns an error for a device with no table.
pub async fn find_partition<T: Transport>(
    agent: &mut FlashAgent<T>,
    name: &str,
) -> Result<Partition> {
    let flash = agent.info().await?;
    let table = read_table(agent, &flash).await?;
    Ok(table.require()?.find(name)?.clone())
}

/// What the device answered when it was asked for its partition table.
///
/// There are three outcomes, not two. A device with no table and a device whose
/// table is damaged are different findings. A write goes ahead on either, and its
/// plan says which one it found.
enum Table {
    /// The device has a table, and it was read.
    Read(PartitionTable),
    /// The device has no table, which is a valid state for a device.
    Absent,
    /// The device has a table, and it fails validation.
    ///
    /// It is carried as a value, not raised as an error. A write is **not** refused
    /// on a damaged table, because writing a fresh table is how a damaged one is
    /// repaired. A refusal here would leave the tool able to diagnose the damage
    /// and unable to repair it. The plan states the damage instead, and the person
    /// decides.
    Damaged {
        /// The format the table announced itself as.
        format: &'static str,
        /// What failed validation.
        detail: String,
    },
}

impl Table {
    /// The table, for an operation that cannot go on without one.
    ///
    /// Resolving a name needs a table. Planning a write does not, because the plan
    /// uses the table only to *describe* the write. That is the difference between
    /// this method and [`touches`](Self::touches).
    fn require(&self) -> Result<&PartitionTable> {
        match self {
            Table::Read(table) => Ok(table),
            Table::Absent => Err(Error::InvalidRequest(
                "this device has no partition table, so it has no partition to name. Address \
                 the sectors you want by their LBA"
                    .to_string(),
            )),
            Table::Damaged { format, detail } => Err(Error::CorruptTable {
                format,
                detail: detail.clone(),
            }),
        }
    }

    /// What the table says about the `sectors` sectors starting at `lba`.
    fn touches(&self, lba: u64, sectors: u64) -> Touches {
        match self {
            Table::Read(table) => Touches::Partitions(table.overlaps(lba, sectors)),
            Table::Absent => Touches::NoTable,
            Table::Damaged { format, detail } => Touches::UnreadableTable {
                format,
                detail: detail.clone(),
            },
        }
    }
}

/// Read the device's partition table, keeping a damaged one rather than raising
/// it.
///
/// A device with no table and a device with a damaged one are different findings.
/// This is the one place that turns that difference into data a plan can carry.
/// Any other failure is returned as an error, such as a device that stopped
/// answering or a desynchronized agent. That failure ends the plan, so there is
/// nothing for the plan to carry.
async fn read_table<T: Transport>(agent: &mut FlashAgent<T>, flash: &FlashInfo) -> Result<Table> {
    match partition::read(agent, flash).await {
        Ok(Some(table)) => Ok(Table::Read(table)),
        Ok(None) => Ok(Table::Absent),
        Err(Error::CorruptTable { format, detail }) => Ok(Table::Damaged { format, detail }),
        Err(other) => Err(other),
    }
}

/// What the device's partition table says about the range a write covers.
///
/// It names what the write would overwrite. A write plan reads the device's
/// partition table to produce it, so the table is read before every write and not
/// only on request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touches {
    /// The device has no partition table, so the plan cannot say what the range
    /// holds.
    ///
    /// The write is planned against the flash's geometry alone.
    NoTable,

    /// The device has a table that fails validation, so pyrographer cannot say what
    /// the range holds.
    ///
    /// The write is planned anyway, and is not refused, because writing a fresh
    /// table is how a damaged one is repaired. A person about to overwrite the
    /// board still needs to know that the partition map is missing. The plan
    /// therefore states that, rather than showing an empty map.
    UnreadableTable {
        /// The format the table announced itself as.
        format: &'static str,
        /// What failed validation.
        detail: String,
    },

    /// The table was read, and these are the partitions the range lands in.
    ///
    /// **An empty list does not mean the write is safe.** It means the range is in
    /// no partition. On a Rockchip board, the bootloader lives outside every
    /// partition. Such a write is therefore either exactly what was meant or a
    /// write into a gap. A caller presents it as a range outside every partition.
    Partitions(Vec<Overlap>),
}

/// One contiguous run of bytes that a multi-segment write lays down.
///
/// A table write can cover more than one range. Repairing a Rockchip parameter
/// rewrites every damaged copy of it. Authoring a GPT lays down the primary near
/// the front, and the backup in the last sector. Each of those runs is a segment,
/// such as a rebuilt GPT copy or a parameter block. A person consents to the whole
/// set at once. Each segment therefore carries a description for the plan to show,
/// where it lands, and the exact bytes that go there.
///
/// The bytes are held whole, in memory. A table copy is a header and a small entry
/// array, or a kilobyte of text, not a flash image. It therefore travels with the
/// plan. A clone's whole-flash source, by contrast, must be read from a device as
/// the write runs. The write pads the final sector, exactly as a single-image write
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// What this run is, in words for a person, such as "the primary GPT (sector 1
    /// onward)" or "the parameter copy at sector 0x400".
    pub what: String,
    /// The first sector it lands on.
    pub lba: u64,
    /// The exact bytes written there.
    ///
    /// Their length is what the plan reports and what the write lays down before
    /// padding the final sector.
    pub bytes: Vec<u8>,
    /// Which of the device's partitions this range lands in, for the plan to show.
    ///
    /// It is the same [`Touches`] a plain write's plan carries. A table write lands
    /// in the metadata region outside every partition, so this is usually
    /// [`Touches::Partitions`] with an empty list. The plan states that explicitly,
    /// so a person does not have to infer it.
    pub touches: Touches,
}

/// What a write would do, in full, before any of it happens.
///
/// [`plan_write`] or [`plan_write_partition`] produces it. Both query the device
/// and send nothing that changes it. A caller renders the plan, and a person reads
/// it. Only then does [`WritePlan::confirm`] turn it into the [`ConfirmedWrite`]
/// that [`flash`] accepts. Producing a plan and stopping there is the dry run,
/// which is the write's own code path stopped one step short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePlan {
    /// The first sector the write touches.
    pub lba: u64,
    /// How many bytes of the image are written.
    pub image_bytes: u64,
    /// How many zero bytes pad the final sector out to a whole one.
    ///
    /// The protocol addresses sectors, so an image that does not end on a sector
    /// boundary still occupies the whole of its last one.
    pub padding_bytes: u64,
    /// How many sectors the write touches, image and padding together.
    pub sectors: u64,
    /// The geometry of the flash that contains the range, as the device reports it.
    pub flash: FlashInfo,
    /// The running loader's raw answer to which SoC it is on.
    ///
    /// The loader that answers this is the loader that would do the writing. This is
    /// the evidence the wrong-loader refusal compares, and the plan carries it for
    /// the person deciding whether to confirm. It stays raw with the gate armed.
    /// [`plan_refusal`] compares it against the exact reply pinned for
    /// [`soc`](WritePlan::soc). A front-end renders the same bytes, so a person sees
    /// what the gate compared.
    pub chip_version: Vec<u8>,
    /// The SoC the caller named this write for, or `None`.
    ///
    /// It is the other half of the wrong-loader comparison. `Some` means the name
    /// has a pinned reply, because [`Soc::parse`] refuses every other name. `None`
    /// means no SoC was named. A write to a board is then refused rather than
    /// guessing one. A block device has no loader, and the gate exempts it.
    /// The SoC travels with the plan, so the plan a person confirms is the plan the
    /// gate checks.
    pub soc: Option<Soc>,
    /// Which of the device's partitions the write would land in, and how much of
    /// each.
    ///
    /// This is the part of the plan a person checks against their intent.
    /// `LBA 16384, 8192 sectors` is a fact about arithmetic. *"The whole of `uboot`,
    /// and the first 100 sectors of `trust`"* is a fact about the board. A person
    /// who reads it before confirming can catch a write aimed at the wrong place.
    pub touches: Touches,
    /// When this write would be proved to have landed.
    ///
    /// The read-back is mandatory on every backend, but it runs at different points
    /// on different backends, and the point changes what a failure costs.
    /// [`PerWindow`](ReadBack::PerWindow) stops at the first bad window, so a
    /// mismatch costs one window. [`AfterCommit`](ReadBack::AfterCommit) cannot read
    /// back until the region is committed, so a mismatch is found with the whole
    /// region already written.
    ///
    /// The plan carries this so a person learns it before confirming. StarFive
    /// recovery shows its `can_verify: false` to the person confirming it for the
    /// same reason.
    pub read_back: ReadBack,
}

impl WritePlan {
    /// Confirm this plan.
    ///
    /// It consumes the plan, so one confirmation permits one write and cannot be
    /// reused for a second.
    pub fn confirm(self) -> ConfirmedWrite {
        ConfirmedWrite(self)
    }
}

/// A [`WritePlan`] a caller has agreed to, and the only thing [`flash`] takes.
///
/// The library never reads stdin, so consent is a value. The caller produces it
/// from a plan the caller has seen. The CLI produces it from a terminal prompt or
/// `--yes`, and the GUI from its plan screen. The library does not distinguish the
/// two.
// Deliberately not `Clone`: a confirmation is consumed by `confirm()`, so one yes
// buys exactly one write. Cloning it would mint a second write from a single act of
// consent, which is the one thing the whole plan/confirm gate exists to prevent.
#[derive(Debug)]
pub struct ConfirmedWrite(WritePlan);

impl ConfirmedWrite {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &WritePlan {
        &self.0
    }
}

/// Plan a write of `image_bytes` bytes starting at `lba`: the dry run.
///
/// It asks the device for its geometry, its loader's account of itself, and its
/// partition table. It checks the range against the geometry and the backend's
/// address ceiling. It records the loader's answer for the wrong-loader gate, and
/// reports what the range covers in the table. It sends nothing that changes the
/// device, so a caller may plan freely. A write that cannot be carried
/// out is refused here, with an error.
///
/// A raw LBA is planned against the table, but not *gated* on it. The table is read
/// so the plan can say what the range holds, as [`Touches`] describes. A device
/// with no table, or a damaged one, is planned for all the same. Writing a fresh
/// table is how a damaged one is repaired.
///
/// A device with no device-wide LBA space is refused before it is asked anything,
/// as [`raw_lba_refusal`] describes. Its regions are written by name, through
/// [`plan_write_partition`].
pub async fn plan_write<T: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    image_bytes: u64,
    soc: Option<Soc>,
) -> Result<WritePlan> {
    if let Some(why) = raw_lba_refusal(agent) {
        return Err(Error::InvalidRequest(format!(
            "{why}. Aim the write at a partition by name instead"
        )));
    }
    let read_back = agent.read_back();
    let ceiling = agent.address_ceiling();
    let (flash, chip_version, table) = survey(agent).await?;
    plan(
        lba,
        image_bytes,
        flash,
        chip_version,
        soc,
        &table,
        read_back,
        ceiling,
    )
}

/// Plan a write into the partition called `name`: the dry run.
///
/// The range comes from the device's own table, not from a number somebody worked
/// out. An image too big for the partition is refused, as [`Partition::must_hold`]
/// describes. A raw LBA cannot be checked that way, because it does not say where
/// the partition ends.
///
/// A name needs a table. Unlike [`plan_write`], this function therefore refuses a
/// device with no table, or one whose table fails validation.
pub async fn plan_write_partition<T: Transport>(
    agent: &mut FlashAgent<T>,
    name: &str,
    image_bytes: u64,
    soc: Option<Soc>,
) -> Result<WritePlan> {
    let read_back = agent.read_back();
    let ceiling = agent.address_ceiling();
    let (flash, chip_version, table) = survey(agent).await?;

    let partition = table.require()?.find(name)?;
    partition.must_hold(image_bytes, flash.sector_size)?;
    let lba = partition.first_lba;

    plan(
        lba,
        image_bytes,
        flash,
        chip_version,
        soc,
        &table,
        read_back,
        ceiling,
    )
}

/// Ask the device everything a plan is made of, once.
///
/// It asks for the geometry, the loader's own answer to what it is running on, and
/// the partition table. All three are read commands, so this function is on the
/// dry run's path and changes nothing. A plan built without asking the device
/// would be a guess, and the write path admits no guesses.
///
/// Every question to the device is asked here, so that [`plan`] can be a pure
/// function of the answers. The two entry points, an LBA and a partition name,
/// therefore share one geometry check.
async fn survey<T: Transport>(agent: &mut FlashAgent<T>) -> Result<(FlashInfo, Vec<u8>, Table)> {
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;
    let table = read_table(agent, &flash).await?;
    Ok((flash, chip_version, table))
}

/// Work out what a write would do, from what the device said.
///
/// It is pure. Every question was asked in [`survey`], and this function only
/// computes and checks over the answers. Both ways of aiming a write call it, so
/// the geometry check is written once and both make it the same way.
// A plan is a function of everything the device said, and every argument here is
// one of those answers. Grouping them behind a struct would be a type that exists
// to satisfy a count rather than to name anything.
#[allow(clippy::too_many_arguments)]
fn plan(
    lba: u64,
    image_bytes: u64,
    flash: FlashInfo,
    chip_version: Vec<u8>,
    soc: Option<Soc>,
    table: &Table,
    read_back: ReadBack,
    ceiling: AddressCeiling,
) -> Result<WritePlan> {
    let sector_size = u64::from(flash.sector_size);

    // A device reports a real sector size; a hand-built `FlashInfo` might not, and a
    // zero would panic the sector arithmetic below three divisions from here. Caught
    // at the plan, where the message points at the geometry rather than at a
    // division deep in the padding math.
    if sector_size == 0 {
        return Err(Error::InvalidRequest(
            "the device reports a sector size of zero, which is not a geometry a write can be \
             planned against"
                .to_string(),
        ));
    }

    // A zero-byte image writes nothing. It is a plausible shell accident -- a
    // truncated download, an empty file -- and reporting it as a successful no-op
    // (the CLI renders it as an inverted range, "LBA 64 through 63") hides the
    // mistake. Refused at the plan, before anything is confirmed.
    if image_bytes == 0 {
        return Err(Error::InvalidRequest(
            "the image is zero bytes, so there is nothing to write. A truncated or empty file \
             is refused rather than reported as a successful no-op"
                .to_string(),
        ));
    }

    // Checked, as `dump` checks the same shape: an image within a sector of
    // `u64::MAX` would make the padded byte count wrap. A file is never that
    // large, so this bites only a direct caller of the library, but the padding
    // is a promise the plan makes and a wrapped one is a lie -- so it is refused
    // rather than computed.
    let sectors = image_bytes.div_ceil(sector_size);
    let padded = sectors.checked_mul(sector_size).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "an image of {image_bytes} bytes, padded to a whole sector, is more bytes than a byte \
             count can hold"
        ))
    })?;
    let padding_bytes = padded - image_bytes;

    // A write that runs off the end of the part is refused entire rather than
    // truncated to fit. A caller that asked for something impossible is a caller
    // whose intent is not known, and guessing at it over flash is how boards die.
    let flash_sectors = flash.size_bytes / sector_size;
    let end = lba.checked_add(sectors).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "a write of {sectors} sectors from LBA {lba} runs past any sector that can be counted"
        ))
    })?;

    // How far the backend's own addressing reaches is the backend's answer, not
    // this function's: rockusb carries a 32-bit LBA, the DFU agent packs an
    // alt-setting index into the high bits, and a block device is bounded by
    // nothing but the geometry checked just below. A ceiling borrowed from one
    // backend and applied to another refuses writes the device could serve --
    // a disk past 2 TiB, a DFU partition past alt-setting 0 -- and says something
    // untrue about why. Asked here, so a range the wire cannot carry is refused
    // before the first window goes out rather than mid-write at the agent's own
    // range check, with earlier windows already overwritten.
    if end > ceiling.past_last {
        let why = ceiling.why;
        return Err(Error::InvalidRequest(format!(
            "a write ending at sector {end} runs past sector {}, and {why}",
            ceiling.past_last
        )));
    }

    if end > flash_sectors {
        return Err(Error::InvalidRequest(format!(
            "a write of {sectors} sectors from LBA {lba} ends at sector {end}, past the \
             {flash_sectors} sectors the device reports"
        )));
    }

    // Worked out after the range is known to be possible, because a write that
    // cannot happen has nothing to land in.
    let touches = table.touches(lba, sectors);

    Ok(WritePlan {
        lba,
        image_bytes,
        padding_bytes,
        sectors,
        flash,
        chip_version,
        soc,
        touches,
        read_back,
    })
}

/// Write a confirmed image to flash, and read back every window of it.
///
/// It streams from `image`, which supplies exactly the `image_bytes` the plan names.
/// Memory holds one window, not one image, so the read-back holds for a 100 GiB
/// image. [`FlashAgent::read_back`] decides when it runs. Under
/// [`ReadBack::PerWindow`], each window is read back before the next one is
/// written, so a bad block surfaces at the window where it happened. Under
/// [`ReadBack::AfterCommit`], the region is committed first, then read back window
/// by window against a CRC-32 of each window sent.
///
/// The read-back cannot be turned off, and there is no flag for it. Without it, a
/// write reports success on a status the device sent before the block was
/// committed. Both eMMC and NAND can send such a status.
///
/// [`Progress`] counts image bytes. Under per-window read-back, a window counts
/// only once it is both written *and* verified. A rate rendered from it is
/// therefore roughly half the raw bus throughput, because the work is the write
/// and the read-back together.
///
/// A difference stops the write. Under per-window read-back it is
/// [`Error::VerifyMismatch`], at that window. After a commit it is
/// [`Error::CommitMismatch`], naming the window, with the whole region written.
/// Nothing is retried, because a silent retry can make a failing eMMC look healthy.
/// Nothing is rolled back, because the overwritten flash is gone. The caller
/// decides whether the cause is a bad block, the wrong image, or a board to
/// replace.
///
/// # The wrong-loader gate
///
/// A loader match is a precondition of every write to a board. A block device has
/// no loader, and the gate exempts it. The loader the host is talking to must have
/// said what it is running on. The answer must match the SoC the caller named
/// when the write was planned. The comparison is exact: the reply
/// against the bytes pinned for that SoC, as [`Soc`] describes.
///
/// It is enforced here, through the same check [`plan_refusal`] makes, so nothing
/// is written once it fails. Before the first window, the loader in hand is asked
/// again, and its answer is checked the same way. A plan that named no SoC is
/// [`Error::InvalidRequest`]. A loader answering as something else is
/// [`Error::LoaderMismatch`].
pub async fn flash<T: Transport>(
    agent: &mut FlashAgent<T>,
    write: ConfirmedWrite,
    image: &mut dyn ImageReader,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    loader_match(agent, write.plan())?;
    let plan = write.plan();

    // The image's first bytes, judged before anything is written. A container a
    // tool unpacks is refused here by the same function a front-end asks, and the
    // bytes read to judge it go to the flash first, so nothing is read twice.
    let head_len = plan.image_bytes.min(CONTAINER_MAGIC_LEN as u64) as usize;
    let mut head = [0u8; CONTAINER_MAGIC_LEN];
    image
        .read_exact(&mut head[..head_len])
        .await
        .map_err(|e| Error::Io(format!("cannot read the image at byte 0: {}", cause(e))))?;
    if let Some(refusal) = image_refusal(&head[..head_len]) {
        return Err(refusal);
    }
    let mut image = Replayed {
        head: &head[..head_len],
        rest: image,
    };

    // ...and again on the loader actually in hand, in case it is not the one that
    // planned. One read command before the first window.
    reverify_loader(agent, plan).await?;
    // The source names no device, so nothing constrains its transport parameter
    // and it is named here. `T` is as good as any type: `ImageSource::Local`
    // holds no agent, so no value of it is ever built. Its fill report is empty
    // by construction -- a local image crosses no transport -- so it is dropped:
    // a flash reads back the destination, and the source it wrote from is a file.
    write_verified::<T, T>(
        agent,
        plan.lba,
        plan.image_bytes,
        plan.sectors,
        ImageSource::Local(&mut image),
        progress,
        cancel,
    )
    .await?;
    Ok(())
}

/// How many of an image's first bytes [`image_refusal`] judges.
pub const CONTAINER_MAGIC_LEN: usize = 4;

/// Why an image that begins with `first` would be refused, or `None`.
///
/// A Rockchip firmware package, its `RKAF` archive and an RKBOOT loader container
/// are each a container a tool unpacks. Written raw, none of them boots, and the
/// write would have replaced whatever was there with something that cannot run.
/// `first` is the image's first [`CONTAINER_MAGIC_LEN`] bytes. An ID block, which
/// begins `RKNS`, is written raw at sector 64 and is not refused.
///
/// [`flash`] enforces this answer on the image it is given, through this function,
/// before a byte goes out. A front-end asks it of a file's first bytes before it
/// plans. A person is then not asked to confirm a write that will be refused.
pub fn image_refusal(first: &[u8]) -> Option<Error> {
    let container = rkfw::identify(first)?;
    let unpack = match container {
        rkfw::Container::Package | rkfw::Container::Archive => {
            "A package is written as a whole: its partition images, its partition table and its \
             ID block, each where it belongs"
        }
        rkfw::Container::Loader => {
            "Its flash stages are laid out as an ID block, written at sector 64, and its USB \
             stages are uploaded to a board in maskrom"
        }
    };
    Some(Error::InvalidRequest(format!(
        "the image is {}, which a tool unpacks before anything of it goes to the flash. Written \
         raw, it does not boot. {unpack}",
        container.describe()
    )))
}

/// An image whose first bytes were already read, read again from the start.
///
/// [`flash`] reads an image's first bytes to judge them, and the image seam has no
/// way back. This serves those bytes first, then the rest of the image.
struct Replayed<'a, 'b> {
    head: &'a [u8],
    rest: &'b mut dyn ImageReader,
}

impl ImageReader for Replayed<'_, '_> {
    fn read_exact<'c>(&'c mut self, buf: &'c mut [u8]) -> BoxFuture<'c, Result<()>> {
        Box::pin(async move {
            let from_head = self.head.len().min(buf.len());
            buf[..from_head].copy_from_slice(&self.head[..from_head]);
            self.head = &self.head[from_head..];
            if from_head < buf.len() {
                self.rest.read_exact(&mut buf[from_head..]).await?;
            }
            Ok(())
        })
    }
}

/// Why a write to this device would be refused before a plan exists, or `None`.
///
/// It is the half of the wrong-loader gate a caller can ask with only an agent and
/// a SoC name. It asks whether a SoC has been named at all. [`Soc::parse`] has
/// already refused every name without a pinned reply, so `Some(soc)` here means
/// the gate can be armed. The comparison itself needs the device's answer, which a
/// plan carries, so a caller asks [`plan_refusal`] once a plan exists.
///
/// A DFU board that cannot be read back at all is also refused here. A block device
/// runs no loader, so the answer for one is the block agent's own
/// [`write_refusal`](crate::block::BlockAgent::write_refusal).
///
/// The GUI grays out the write button on this answer and shows the reason. The CLI
/// does not ask a person to confirm a write it already knows it will refuse.
///
/// `None` means the gate has what it needs. It does not promise that the write goes
/// through, because the loader still has to answer as the named SoC.
pub fn write_refusal<T: Transport>(agent: &FlashAgent<T>, soc: Option<Soc>) -> Option<String> {
    match agent {
        FlashAgent::Rockusb(_) => match soc {
            None => Some(
                "rockusb write: name the board's SoC. The write proceeds only when the loader's \
                 chip-version reply matches it"
                    .to_string(),
            ),
            Some(_) => None,
        },
        // The DFU write path is built -- one download session per region, committed
        // and then read back whole ([`ReadBack::AfterCommit`]). What refuses is the
        // gate in front of it, and it refuses for two different reasons that are
        // worth telling apart: a device that could not be read back at all, and a
        // board whose SoC nothing has pinned.
        FlashAgent::Dfu(_) => match agent.read_back() {
            // About the device, and true however the gate is armed, so it is
            // answered first.
            ReadBack::Impossible(why) => Some(format!(
                "DFU write: {why}. pyrographer does not write to a device it cannot read back"
            )),
            _ => match soc {
                None => Some(
                    "DFU write: every write requires a loader match, and the match requires a \
                     named SoC. No Ingenic SoC is pinned against hardware yet, so no SoC name can \
                     arm the gate. Pinning one requires reading a bootstrapped board's identity \
                     on the bench. Reading with dump, verify, and partitions works in the meantime"
                        .to_string(),
                ),
                // A named SoC is a pinned one by construction, and no Ingenic part
                // is. So this is a Rockchip name aimed at a DFU board, and the gate
                // settles it against the device's own answer rather than here: a
                // DFU board answers the chip-version query with no bytes, which
                // matches no pinned reply.
                Some(_) => None,
            },
        },
        // A block device runs no loader, so the question this gate asks -- is
        // the program on the board the one for this SoC -- has no counterpart.
        // Its equivalent danger is writing the wrong *disk*, and the guard for
        // that is in [`crate::block::open`]: it refuses every device the running
        // system rests on with no override, and asks the kernel for exclusive use
        // before it hands back an agent at all.
        //
        // But an agent opened only to *read* was never asked those, because a read
        // has no such danger -- a write-protected card is the safest thing there
        // is to image, and refusing to open one would make this backend unable to
        // read the media it is most obviously for. So the agent carries what it was
        // opened for, and answers here in the sentence `block::write_refusal`
        // produced, which is what grays the button out and says why.
        FlashAgent::Block(agent) => agent.write_refusal(),
    }
}

/// Why an erase on this device would be refused, or `None`.
///
/// It is the counterpart of [`write_refusal`]. [`Caps::can_erase`] tells a caller
/// *whether* to gray out the erase button. This function gives the reason, in the
/// backend's own words, so a front-end quotes it rather than writing its own. A
/// front-end can therefore show a disabled `erase` with its reason, rather than
/// hiding the button.
///
/// [`Caps::can_erase`]: crate::agent::Caps::can_erase
pub fn erase_refusal<T: Transport>(agent: &FlashAgent<T>) -> Option<&'static str> {
    match agent {
        FlashAgent::Rockusb(_) => Some(
            "rockusb erase: no board has confirmed what the erase command does to a range. \
             Rockchip's rkdeveloptool names it ERASE_LBA, 0x25. An erase with the wrong range \
             semantics is not reported as a failed command, and it can destroy the board. \
             Settling it requires a probe through the write path on an expendable board, and \
             that probe is not written.",
        ),
        FlashAgent::Dfu(_) => Some(
            "DFU erase: DFU has no whole-region erase that pyrographer drives. The device erases \
             as needed during a download, so there is no separate erase to offer.",
        ),
        FlashAgent::Block(_) => Some(
            "block erase: the block layer presents storage that is always writable, with no erase \
             to drive. A write overwrites the data directly. A device that erases internally, \
             such as an SSD or an SD card, manages its own erasure.",
        ),
    }
}

/// Why a write that addresses this device by raw LBA would be refused, or `None`.
///
/// Three kinds of write address a device-wide LBA space: a write aimed at a sector
/// number, a clone, and every partition-table write. A partition table sits at fixed
/// sectors of that space. A device whose [`Caps::can_address_raw_lba`] is false has
/// no such space. A DFU board is one. Its flash is reachable only as named regions,
/// so a sector number given to it names no sector of its flash.
///
/// [`plan_write`], [`plan_clone`], every table plan and [`write_table`] enforce this
/// answer through this function. A front-end that grays a control on it therefore
/// grays the same writes the verbs refuse. A write aimed at a partition by name
/// ([`plan_write_partition`]) asks nothing of it, because the device itself names
/// the region.
///
/// [`Caps::can_address_raw_lba`]: crate::agent::Caps::can_address_raw_lba
pub fn raw_lba_refusal<T: Transport>(agent: &FlashAgent<T>) -> Option<&'static str> {
    (!agent.caps().can_address_raw_lba).then_some(
        "this board has no device-wide LBA space. It reaches its flash only by named region \
         (its DFU alt-settings)",
    )
}

/// The refusal a partition-table write meets on a device with no device-wide LBA
/// space, as [`raw_lba_refusal`] answers it.
fn table_without_raw_lba<T: Transport>(agent: &FlashAgent<T>) -> Result<()> {
    match raw_lba_refusal(agent) {
        Some(why) => Err(Error::InvalidRequest(format!(
            "{why}. A partition table sits at fixed sectors of a device-wide LBA space, so this \
             board has nowhere to write one"
        ))),
        None => Ok(()),
    }
}

/// Why this planned write would be refused, plan in hand, or `None`.
///
/// This is the whole wrong-loader gate, which a caller can ask before any write.
/// A loader is a program for one SoC, running on whatever board it was uploaded to.
/// The wrong loader enumerates, answers, and writes to the wrong offsets, with a
/// plausible status behind every command. The gate therefore compares the loader's
/// own answer, [`WritePlan::chip_version`], against the exact reply pinned for the
/// SoC the caller named. It refuses on anything short of byte equality.
///
/// The USB descriptors cannot answer this question. A product ID names a family,
/// the family list is not exhaustive, and the bcdUSB flag can misreport the mode.
///
/// [`flash`] and [`clone`] enforce this answer through the same function. The
/// refusal a front-end shows a person and the refusal the write makes therefore
/// cannot differ. `None` means the write would go ahead.
pub fn plan_refusal<T: Transport>(agent: &FlashAgent<T>, plan: &WritePlan) -> Option<Error> {
    loader_refusal(agent, plan.soc, &plan.chip_version)
}

/// The wrong-loader gate as a question, over the two fields it compares.
///
/// The fields are the SoC the caller named and the loader's own [`chip_version`]
/// answer. [`plan_refusal`] asks it over a [`WritePlan`]. A multi-segment write
/// asks it over its own gate fields, through [`segmented_refusal`]. The comparison
/// is in one function, so a table write and a plain write gate the same way on the
/// same board.
///
/// [`chip_version`]: WritePlan::chip_version
fn loader_refusal<T: Transport>(
    agent: &FlashAgent<T>,
    soc: Option<Soc>,
    chip_version: &[u8],
) -> Option<Error> {
    // A block device has no loader to be wrong. Comparing its (empty) chip
    // version against a pinned SoC would refuse every block write for a reason
    // that does not apply, and naming a SoC for a disk is a category error rather
    // than a safety measure. What this gate does ask on that backend is whether
    // the agent in hand can write at all -- an agent opened for reading answers
    // that here rather than at the first `write_at`, so a front-end can gray the
    // button and the plan can be refused before anything is confirmed.
    if let FlashAgent::Block(block) = agent {
        return block.write_refusal().map(Error::InvalidRequest);
    }
    match soc {
        None => write_refusal(agent, None).map(Error::InvalidRequest),
        Some(soc) if soc.matches(chip_version) => None,
        Some(soc) => Some(Error::LoaderMismatch {
            named: soc.name(),
            expected: soc.pinned_reply().to_vec(),
            answered: chip_version.to_vec(),
        }),
    }
}

/// The wrong-loader gate as the write path enforces it: the [`plan_refusal`]
/// check, returned as a `Result`.
fn loader_match<T: Transport>(agent: &FlashAgent<T>, plan: &WritePlan) -> Result<()> {
    loader_match_fields(agent, plan.soc, &plan.chip_version)
}

/// [`loader_match`] over the gate fields, so a multi-segment write enforces the
/// same gate as a plain write, through the same code.
fn loader_match_fields<T: Transport>(
    agent: &FlashAgent<T>,
    soc: Option<Soc>,
    chip_version: &[u8],
) -> Result<()> {
    match loader_refusal(agent, soc, chip_version) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// Ask the loader about to be written through for its SoC again, and gate on that
/// answer rather than the one the plan recorded.
///
/// [`plan_refusal`] and [`loader_match`] compare [`WritePlan::chip_version`], the
/// reply [`survey`] read when the plan was made. That binds the gate to the loader
/// that *planned*. The loader that *writes* is the same one only if the caller
/// passes the same agent. A call such as
/// `flash(&mut other_agent, plan_from_this_agent.confirm(), ..)` would pass on the
/// planning agent's answer and then write through a different one.
///
/// Before the first window, the write therefore re-issues `chip_version` on the
/// executing agent. It runs the same byte-exact comparison against the SoC the plan
/// named. This costs one read command, and proves that the loader that answers is
/// the loader that will write. It also covers the time between plan and write, in
/// which a board can re-enumerate or a loader can be swapped.
async fn reverify_loader<T: Transport>(agent: &mut FlashAgent<T>, plan: &WritePlan) -> Result<()> {
    reverify_loader_soc(agent, plan.soc).await
}

/// [`reverify_loader`] over the SoC alone, so a multi-segment write re-checks the
/// loader in hand against the same pinned reply a plain write does.
async fn reverify_loader_soc<T: Transport>(
    agent: &mut FlashAgent<T>,
    soc: Option<Soc>,
) -> Result<()> {
    let answered = agent.chip_version().await?;
    // The same question `plan_refusal` answers, over the reply that just came back
    // rather than the one the plan recorded. It is asked through `loader_refusal`
    // rather than decided here, so that the refusal a front-end is shown and the
    // refusal the write makes come from one place and cannot say different
    // things. What makes that load-bearing rather than tidy is the backend that
    // has no loader: a block device answers the chip-version query with no bytes
    // and names no SoC by design, so a second opinion here would refuse a write
    // `plan_refusal` had just passed.
    match loader_refusal(agent, soc, &answered) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

/// Where the bytes a write puts on the flash come from.
///
/// To the write path, a local image and another board's flash are the same thing:
/// something that fills a window. This enum is the only difference between
/// [`flash`] and [`clone`]. The write-then-read-back loop is therefore written
/// once, and its read-back holds identically for both.
///
/// The source carries its own transport parameter, separate from the
/// destination's. A clone can copy a board on one bus to a board on another, and
/// the two need not share a transport type. A single parameter for both would rule
/// such a pairing out.
enum ImageSource<'a, S: Transport> {
    /// A local image, such as a file, a buffer, or a `Blob` in a browser tab.
    ///
    /// It never crosses a [`Transport`], so a silent read failure cannot reach a
    /// write through it. There is nothing to scan for constant fill.
    Local(&'a mut dyn ImageReader),
    /// Another device's flash: the source of a clone.
    Device {
        /// The board being copied.
        agent: &'a mut FlashAgent<S>,
        /// The sector the next window comes from.
        at: u64,
        /// Scans each window read from the source for constant fill, as [`dump`]
        /// scans its own reads.
        ///
        /// A clone reads the source over a [`Transport`], so it can hit the same
        /// silent read failure a dump can. In a clone, the fill byte is not only
        /// returned to the caller: it is *written onto the destination's flash*.
        /// The [`FillReport`] is returned so a caller can warn of fill copied onto
        /// the destination.
        scanner: FillScanner,
    },
}

impl<'a, S: Transport> ImageSource<'a, S> {
    /// A source that reads another board's flash from sector `at`.
    ///
    /// The scanner is sized to the source's own sectors, because the runs it
    /// reports are keyed to the source's own LBAs. The source is the board a person
    /// would read again another way, not the board just written.
    fn device(agent: &'a mut FlashAgent<S>, at: u64) -> Self {
        let scanner = FillScanner::new(agent.sector_size());
        ImageSource::Device { agent, at, scanner }
    }

    /// Fill `buf` with the source's next bytes, and return how many were the
    /// source's own.
    ///
    /// `taken` is how many bytes the write has consumed so far, and `total` is how
    /// many the plan says there are. Past the end of the source, the buffer is
    /// zero padding. The protocol addresses sectors, so an image that does not end
    /// on a sector boundary still occupies the whole of its last sector. The tail
    /// of that sector is zeroed rather than left holding the buffer's old contents.
    async fn fill(&mut self, taken: u64, total: u64, buf: &mut [u8]) -> Result<usize> {
        let wanted = (total - taken).min(buf.len() as u64) as usize;

        match self {
            ImageSource::Local(image) => {
                image.read_exact(&mut buf[..wanted]).await.map_err(|e| {
                    Error::Io(format!(
                        "cannot read the image at byte {taken}: {}. It is shorter than the \
                         {total} bytes the plan was made against",
                        cause(e)
                    ))
                })?;
            }
            ImageSource::Device { agent, at, scanner } => {
                // A clone's plan refuses a sector-size mismatch, so a window is a
                // whole number of the source's sectors as well as the
                // destination's, and `wanted` is the whole buffer.
                agent.read(*at, buf).await?;
                // Watched before the tail is zeroed below, so the scanner sees
                // exactly what the source returned rather than padding this side
                // added -- and keyed on the source's LBA, so a run it finds
                // names where on the *source* the fill is.
                scanner.observe(*at, buf);
                *at += buf.len() as u64 / u64::from(agent.sector_size());
            }
        }

        buf[wanted..].fill(0);
        Ok(wanted)
    }

    /// The constant fill the source read back, once the write is over.
    ///
    /// It is empty for a [`Local`](Self::Local) source, which crosses no transport
    /// and so cannot fail this way. For a [`Device`](Self::Device) source, it is
    /// what the scanner accumulated window by window, and a clone returns it.
    fn into_report(self) -> FillReport {
        match self {
            ImageSource::Local(_) => FillReport::default(),
            ImageSource::Device { scanner, .. } => scanner.finish(),
        }
    }
}

/// Write the plan's image one window at a time, reading back each window before
/// the next one goes out.
///
/// This is the write path itself, run after the gate has passed. [`flash`] and
/// [`clone`] are both this function with a different [`ImageSource`]. It
/// overwrites flash only through [`write_windows`], the one loop in the crate that
/// does.
///
/// It is a separate function so that tests can reach it while the gate in front
/// of the verbs refuses. Otherwise the write path could not run until a board
/// arrived, and it would be unchecked the first time it ran on one.
///
/// The returned [`FillReport`] is the source's, not the destination's. A
/// [`Device`](ImageSource::Device) source is read over a transport, and can return
/// constant fill the same way a [`dump`] can. A clone that copied the fill wrote it
/// onto real flash. The report is empty for a [`Local`](ImageSource::Local)
/// source. A run that ends early, on a mismatch or a cancellation, returns that
/// error and no report.
async fn write_verified<T: Transport, S: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    image_bytes: u64,
    sectors: u64,
    source: ImageSource<'_, S>,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<FillReport> {
    progress(Progress::Started {
        total_bytes: image_bytes,
    });
    // One run, so the bytes done in it are the bytes done overall: the callback
    // hands `done` straight through as the absolute progress.
    let report = write_windows(
        agent,
        lba,
        image_bytes,
        sectors,
        source,
        &mut |done| {
            progress(Progress::Advanced {
                done_bytes: done,
                total_bytes: image_bytes,
            })
        },
        cancel,
    )
    .await?;
    progress(Progress::Finished {
        done_bytes: image_bytes,
    });
    Ok(report)
}

/// Write each of `segments` in turn, reading back every window, as one operation.
///
/// It is the multi-segment counterpart of [`write_verified`], with the same safety
/// properties. Each segment goes through the same [`write_windows`] loop, so each
/// is read back exactly as a single image is. The first window that fails
/// verification stops the whole write at that window. The only difference is that
/// several runs are laid down in sequence under one consent. A repair rewrites
/// several damaged copies, and an authored table lays down each of its copies.
///
/// Progress covers the segments together. [`Started`](Progress::Started) names the
/// image bytes across all of them, [`Advanced`](Progress::Advanced) counts them
/// cumulatively, and [`Finished`](Progress::Finished) is sent once. A person
/// watching a progress bar sees one write for one confirmation, not a series of
/// separate ones.
///
/// The caller must enforce the gate first, exactly as [`flash`] enforces it before
/// [`write_verified`]. This function overwrites flash and does not gate. The loader
/// match therefore stays in the verb, [`write_table`], which makes the same check
/// [`segmented_refusal`] answers for a front-end, so the two cannot differ. Each
/// segment's bytes are a local image, so nothing here crosses a transport to be
/// scanned for fill.
async fn write_segments<T: Transport>(
    agent: &mut FlashAgent<T>,
    segments: &[Segment],
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let sector_size = u64::from(agent.sector_size());
    let total_bytes: u64 = segments.iter().map(|s| s.bytes.len() as u64).sum();

    progress(Progress::Started { total_bytes });

    let mut done_before = 0u64;
    for segment in segments {
        let image_bytes = segment.bytes.len() as u64;
        let sectors = image_bytes.div_ceil(sector_size);
        let mut image = SyncReader::new(segment.bytes.as_slice());
        // The runs before this one are already on the flash, so their bytes are
        // added to whatever this run reports to keep the bar cumulative.
        write_windows::<T, T>(
            agent,
            segment.lba,
            image_bytes,
            sectors,
            ImageSource::Local(&mut image),
            &mut |done| {
                progress(Progress::Advanced {
                    done_bytes: done_before + done,
                    total_bytes,
                })
            },
            cancel,
        )
        .await?;
        done_before += image_bytes;
    }

    progress(Progress::Finished {
        done_bytes: total_bytes,
    });
    Ok(())
}

/// Read a committed region back and check it against the digests of what was
/// sent.
///
/// It is the second half of [`ReadBack::AfterCommit`]. It reads the region back in
/// the same windows the write used, so window `i` read back is window `i` sent. It
/// compares a four-byte CRC-32 per window rather than the window itself. The image
/// was streamed once and never held, so no copy of it is left to compare against.
///
/// **Cancellation is not honored here, deliberately.** The bytes are already on the
/// flash when this runs. Stopping now would leave a written board unchecked, which
/// is the outcome the mandatory read-back exists to prevent. A caller that wants to
/// stop has to wait for the check. Each transfer it makes carries the transport's
/// own deadline, so a wedged device still ends the check instead of hanging it.
async fn verify_committed<T: Transport>(
    agent: &mut FlashAgent<T>,
    start: u64,
    sectors: u64,
    window_sectors: u64,
    sector_size: u64,
    sent: &[u32],
    buffer: &mut [u8],
) -> Result<()> {
    // The region the device says it committed and the region the write produced
    // digests for have to be the same region, and that is settled before a byte is
    // read back. Checked in both directions on purpose: a device that committed
    // *more* than was sent leaves digests to spare, and one that committed *less*
    // would otherwise run the digests out and return having checked a prefix --
    // and a partial check is not a check. Nothing above this can cause either, so
    // whichever way it falls it is the device disagreeing about what it was told.
    let windows = sectors.div_ceil(window_sectors);
    if windows != sent.len() as u64 {
        return Err(Error::Protocol(format!(
            "the device committed {sectors} sectors, which is {windows} windows, but the write \
             produced digests for {}. The region to be checked is not the region that was \
             written",
            sent.len()
        )));
    }

    let mut at = start;
    let mut remaining = sectors;
    let mut offset = 0u64;

    for expected in sent {
        let n = remaining.min(window_sectors);
        let bytes = (n * sector_size) as usize;
        let back = &mut buffer[..bytes];
        agent.read(at, back).await?;

        if crc32(back) != *expected {
            return Err(Error::CommitMismatch {
                offset,
                window_bytes: bytes as u64,
            });
        }

        at += n;
        remaining -= n;
        offset += bytes as u64;
    }

    Ok(())
}

/// The window-by-window write-then-read-back loop, without the
/// [`Started`](Progress::Started) and [`Finished`](Progress::Finished) events.
///
/// It is the one place in the crate where flash is overwritten, and the one place
/// the mandatory read-back runs. Both kinds of write share it: a single image
/// ([`write_verified`]) and each run of a multi-segment table write
/// ([`write_segments`]). The read-back therefore holds identically for both.
///
/// It reports the image bytes done *in this run* to `advanced` as each window
/// lands verified. The caller turns that into absolute progress: a single write
/// passes it through, and a segmented write adds the runs before it. The
/// [`FillReport`] is the source's, as in [`write_verified`]. It is empty for a
/// [`Local`](ImageSource::Local) source, which every table segment is.
async fn write_windows<T: Transport, S: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    image_bytes: u64,
    sectors: u64,
    source: ImageSource<'_, S>,
    advanced: &mut dyn FnMut(u64),
    cancel: &Cancel,
) -> Result<FillReport> {
    let outcome =
        write_windows_inner(agent, lba, image_bytes, sectors, source, advanced, cancel).await;

    // A backend that holds a write until it is told to commit -- DFU, and only DFU
    // -- is left holding one by any failure above, and the session it is holding
    // outlives the error: a later write aimed at another region would be refused
    // for a reason nobody could act on short of reopening the device. So the
    // session is abandoned here, blind, on every backend.
    //
    // The abandon's own failure is dropped on purpose. The error being unwound
    // from is what happened to the write, and replacing it with whatever went
    // wrong tidying up afterwards would hide it. A session tidied away is not a
    // write that succeeded.
    if outcome.is_err() && agent.holds_uncommitted_write() {
        let _ = agent.abandon_write().await;
    }

    outcome
}

/// [`write_windows`] without the cleanup on failure.
///
/// The loop can therefore use `?` throughout. [`write_windows`] abandons a held
/// session once, in the one place that sees every error this loop returns.
async fn write_windows_inner<T: Transport, S: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    image_bytes: u64,
    sectors: u64,
    mut source: ImageSource<'_, S>,
    advanced: &mut dyn FnMut(u64),
    cancel: &Cancel,
) -> Result<FillReport> {
    // Asked once, before a byte goes out, and never matched on again: the
    // difference between the protocols is this value, not a second copy of this
    // loop.
    let read_back = agent.read_back();
    if let ReadBack::Impossible(why) = read_back {
        return Err(Error::NotImplemented(why));
    }

    let sector_size = u64::from(agent.sector_size());
    let window_sectors = (WINDOW_BYTES / sector_size).max(1);

    // Two buffers, one window each: what we mean to write, and what the flash
    // says it holds. Never an image, at either end.
    let mut window = vec![0u8; (window_sectors * sector_size) as usize];
    let mut readback = vec![0u8; (window_sectors * sector_size) as usize];
    // One digest per window written, kept only where the check has to wait for a
    // commit. Four bytes a window: a 100 GiB image at a 1 MiB window is 400 KiB of
    // them, which is the price of a check this backend can make no earlier.
    let mut sent_digests: Vec<u32> = Vec::new();
    let mut at = lba;
    let mut done_bytes = 0u64;
    let mut remaining = sectors;

    while remaining > 0 {
        // Between windows, never inside one: a canceled write stops at a
        // command boundary and never leaves the device partway through a
        // command. What is already written stays written -- cancellation is not
        // an undo, and the report says how far it got.
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }

        let n = remaining.min(window_sectors);
        let out = &mut window[..(n * sector_size) as usize];

        // The source's bytes for this window, and then the padding. Only the
        // final sector of the final window can be short, so `done_bytes` is the
        // count of image bytes and the offset into the write alike.
        let from_image = source.fill(done_bytes, image_bytes, out).await?;

        agent.write(at, out).await?;

        match read_back {
            // The read-back, in the same window, before the next one goes out. The
            // padding is compared too: core knows exactly what it put there, so
            // there is no reason to verify less than it wrote.
            ReadBack::PerWindow => {
                let back = &mut readback[..out.len()];
                agent.read(at, back).await?;
                if let Some(offset) = back.iter().zip(out.iter()).position(|(a, b)| a != b) {
                    return Err(Error::VerifyMismatch {
                        offset: done_bytes + offset as u64,
                        found: back[offset],
                        expected: out[offset],
                    });
                }
            }
            // Nothing is on the flash to read yet, so what is kept is a digest of
            // what went out -- four bytes a window, whatever the window's size, so
            // this stays bounded for an image larger than memory the way the
            // buffers above do. The comparison happens below, once the device has
            // committed.
            ReadBack::AfterCommit => sent_digests.push(crc32(out)),
            // Refused before the loop.
            ReadBack::Impossible(_) => unreachable!("refused before any byte went out"),
        }

        at += n;
        remaining -= n;
        done_bytes += from_image as u64;
        advanced(done_bytes);
    }

    // The write is sent; on a backend that holds it, this is what puts it on the
    // flash, and it reports the region the device was actually told about.
    let committed = agent.finish_write().await?;

    if read_back == ReadBack::AfterCommit {
        let Some((start, written_sectors)) = committed else {
            return Err(Error::Protocol(
                "the backend reads back only after committing, but it committed no region, so \
                 there is nothing to check the write against"
                    .to_string(),
            ));
        };
        verify_committed(
            agent,
            start,
            written_sectors,
            window_sectors,
            sector_size,
            &sent_digests,
            &mut readback,
        )
        .await?;
    }

    Ok(source.into_report())
}

/// Read the flash back from `lba` and compare it against `image`.
///
/// It is windowed, like [`dump`]. One window is read from the device and compared
/// with the next `image` bytes, so neither side is ever held whole in memory.
/// `image_bytes` is how much of the image to compare, and it is what [`Progress`]
/// counts.
///
/// A difference is [`Error::VerifyMismatch`], not a protocol error. The exchange
/// with the device worked as designed, and the flash's contents are the answer.
/// [`flash`]'s mandatory read-back relies on the same distinction. Flash that
/// differs from the image is a result. A failed exchange with the device produces
/// no result, and is reported as a different error.
///
/// The device addresses sectors. An image that ends partway through a sector
/// therefore has its last sector read in full, and compared only over the bytes the
/// image has.
///
/// Like [`dump`], it scans what the device returns for constant fill, and returns a
/// [`FillReport`]. A verify that *passes* over a filled region can mislead. It means
/// only that the image agrees with the device there. If the image was itself dumped
/// across the same silent read failure, two copies of the same fill agree. No data
/// is confirmed, as [`crate::fill`] explains.
///
/// A mismatch returns before the report is finished, because the verify has already
/// failed.
pub async fn verify<T: Transport>(
    agent: &mut FlashAgent<T>,
    lba: u64,
    image: &mut dyn ImageReader,
    image_bytes: u64,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<FillReport> {
    let sector_size = u64::from(agent.sector_size());
    let window_bytes = (WINDOW_BYTES / sector_size).max(1) * sector_size;

    progress(Progress::Started {
        total_bytes: image_bytes,
    });

    let mut from_flash = vec![0u8; window_bytes as usize];
    let mut from_image = vec![0u8; window_bytes as usize];
    let mut scanner = FillScanner::new(agent.sector_size());
    let mut at = lba;
    let mut done_bytes = 0u64;

    while done_bytes < image_bytes {
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }

        // The image's bytes in this window, and the whole sectors that hold them.
        let want = (image_bytes - done_bytes).min(window_bytes);
        let sectors = want.div_ceil(sector_size);
        let read = &mut from_flash[..(sectors * sector_size) as usize];
        agent.read(at, read).await?;
        scanner.observe(at, read);

        let expected = &mut from_image[..want as usize];
        image.read_exact(expected).await.map_err(|e| {
            Error::Io(format!(
                "cannot read the image at byte {done_bytes}: {}. It is shorter than the \
                 {image_bytes} bytes it was said to hold",
                cause(e)
            ))
        })?;

        // `zip` stops at the image's bytes, which is what leaves the padding of a
        // final part-sector out of the comparison: it is flash we never claimed
        // anything about.
        if let Some(offset) = read.iter().zip(expected.iter()).position(|(a, b)| a != b) {
            return Err(Error::VerifyMismatch {
                offset: done_bytes + offset as u64,
                found: read[offset],
                expected: expected[offset],
            });
        }

        at += sectors;
        done_bytes += want;
        progress(Progress::Advanced {
            done_bytes,
            total_bytes: image_bytes,
        });
    }

    progress(Progress::Finished { done_bytes });
    Ok(scanner.finish())
}

/// What a clone would do, in full, before any of it happens.
///
/// A clone is a write, so it is gated like one. [`plan_clone`] produces this plan
/// by querying both devices and changing neither. A caller renders the plan, and a
/// person reads it. Only then does [`ClonePlan::confirm`] turn it into the
/// [`ConfirmedClone`] that [`clone`] accepts. Producing a plan and stopping there
/// is the dry run.
///
/// It reports both ends, because a person can get either one wrong. A clone with
/// the source and the destination swapped destroys the board that was meant to be
/// copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonePlan {
    /// The flash being copied, as the source device reports it.
    ///
    /// No command a clone sends to the source changes it.
    pub source: FlashInfo,
    /// The write the destination would take: where the bytes land, how many
    /// there are, and which loader is answering for them.
    pub destination: WritePlan,
}

impl ClonePlan {
    /// Confirm this plan.
    ///
    /// It consumes the plan, so one confirmation permits one clone and cannot be
    /// reused for a second.
    pub fn confirm(self) -> ConfirmedClone {
        ConfirmedClone(self)
    }
}

/// A [`ClonePlan`] a caller has agreed to, and the only thing [`clone`] takes.
// Not `Clone`, for the same reason [`ConfirmedWrite`] is not: one confirmation, one
// clone.
#[derive(Debug)]
pub struct ConfirmedClone(ClonePlan);

impl ConfirmedClone {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &ClonePlan {
        &self.0
    }
}

/// Plan a clone of the whole of `src`'s flash onto `dst`: the dry run.
///
/// It queries both devices, and sends nothing that changes either. The destination
/// is planned exactly as [`plan_write`] plans any other write, with the source's
/// flash as the image. A source larger than the destination is refused there, not
/// truncated to fit. Two devices with different sector sizes are also refused.
///
/// A clone reads and writes a whole device by raw LBA. A device on either end with
/// no device-wide LBA space is therefore refused first, before either is asked
/// anything, as [`raw_lba_refusal`] describes.
pub async fn plan_clone<S: Transport, D: Transport>(
    src: &mut FlashAgent<S>,
    dst: &mut FlashAgent<D>,
    soc: Option<Soc>,
) -> Result<ClonePlan> {
    for (end, refusal) in [
        ("source", raw_lba_refusal(src)),
        ("destination", raw_lba_refusal(dst)),
    ] {
        if let Some(why) = refusal {
            return Err(Error::InvalidRequest(format!(
                "a clone copies a whole device by raw LBA, and the {end} cannot take part: {why}. \
                 Copy a partition with a dump and a write instead"
            )));
        }
    }

    // A clone copies bytes, and bytes do not care what size sector they came
    // from -- but the source is read a window at a time, and a window that is a
    // whole number of the destination's sectors need not be a whole number of
    // the source's. Rather than quietly misalign the tail of a clone, this
    // refuses. A rockusb board addresses 512-byte sectors and a 4Kn disk 4096-byte
    // ones, so a clone between the two meets it.
    if src.sector_size() != dst.sector_size() {
        return Err(Error::InvalidRequest(format!(
            "the source addresses {}-byte sectors and the destination {}-byte sectors. A \
             clone requires both devices to use the same sector size",
            src.sector_size(),
            dst.sector_size()
        )));
    }

    let source = src.info().await?;
    let destination = plan_write(dst, 0, source.size_bytes, soc).await?;

    Ok(ClonePlan {
        source,
        destination,
    })
}

/// Copy the whole of one board's flash onto another, verifying every window as
/// it lands.
///
/// Each device has its own transport type parameter, so a board on one bus can be
/// cloned to a board on another. One type parameter for both would tie the two
/// ends to one transport type.
///
/// This is [`flash`] with the source's flash in place of the image, and it runs
/// the same code. It shares the windowing, the mandatory read-back of the
/// destination, and cancellation at a window boundary. A destination that does not
/// hold what was sent to it is the same [`Error::VerifyMismatch`]. Memory holds one
/// window, so a 100 GiB part clones in a megabyte of RAM.
///
/// The read-back reads the *destination*. The source is read once and compared
/// with nothing, because it is what is being copied. The source is read over a
/// transport, so it is scanned for constant fill. The returned [`FillReport`]
/// describes the source's constant fill, as [`dump`] does for its own reads.
///
/// A silent read failure on a dump produces an image a person can still choose not
/// to trust. On a clone, the fill byte is written onto the destination's flash
/// before anyone sees it. The report is a caution, not a failure: the clone
/// succeeded, in that the destination holds what the source returned. Over a
/// filled region, both boards then hold the same fill and no data. The report is
/// empty on a healthy read.
///
/// # The wrong-loader gate
///
/// A clone writes, so it is gated exactly as [`flash`] is. The gate checks the
/// *destination's* loader, because the destination is what gets written. The SoC
/// named to [`plan_clone`] names the destination.
pub async fn clone<S: Transport, D: Transport>(
    src: &mut FlashAgent<S>,
    dst: &mut FlashAgent<D>,
    confirmed: ConfirmedClone,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<FillReport> {
    let plan = confirmed.plan();

    // The destination's loader, because the destination is what gets written.
    // Nothing gates the source: reading a board cannot hurt it. Checked twice --
    // against the plan's recorded answer, then against the destination loader in
    // hand -- so a plan made against one board and confirmed through another (a
    // swapped `src`/`dst`, say) cannot pass on the wrong loader's answer.
    loader_match(dst, &plan.destination)?;
    reverify_loader(dst, &plan.destination).await?;

    let dest = &plan.destination;
    write_verified(
        dst,
        dest.lba,
        dest.image_bytes,
        dest.sectors,
        ImageSource::device(src, 0),
        progress,
        cancel,
    )
    .await
}

/// Plan a repair of the device's GPT: the dry run.
///
/// It reads the device's geometry, the loader's account of itself, and both GPT
/// copies. It then plans to rewrite whichever copy is damaged from the intact one.
/// A damaged primary is rebuilt from the backup. A stale, damaged or missing backup
/// is rebuilt from the primary. It sends nothing that changes the device, so a
/// caller may plan freely.
///
/// The plan is a [`SegmentedPlan`], the type a parameter repair and an authoring
/// also produce. Every table write is therefore gated, confirmed and executed
/// through one path, [`write_table`]. A GPT repair is a single segment: the one
/// rebuilt copy, with its [`what`](Segment::what) naming which copy it is.
///
/// It refuses the states it cannot act on, each with its own reason:
///
/// - The device has no device-wide LBA space, so it has no fixed sectors for a
///   table, as [`raw_lba_refusal`] describes. Nothing is asked of it.
/// - Both copies pass validation and agree, so there is nothing to repair.
/// - The device has no GPT, so there is no table to repair. Writing a fresh one
///   from a layout is authoring.
/// - The primary is damaged and the backup is gone too, so the device holds no copy
///   to repair from. That is [`Error::CorruptTable`].
///
/// A repair rebuilds a damaged copy from an intact one, and never invents a table.
///
/// The parameter counterpart is [`plan_repair_param`], and authoring is
/// [`plan_author_param`].
pub async fn plan_repair_table<T: Transport>(
    agent: &mut FlashAgent<T>,
    soc: Option<Soc>,
) -> Result<SegmentedPlan> {
    table_without_raw_lba(agent)?;
    let ceiling = agent.address_ceiling();
    let read_back = agent.read_back();
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;

    let source =
        match partition::read_gpt_repair_source(agent, &flash).await? {
            GptRepair::Repairable(source) => source,
            GptRepair::Healthy => return Err(Error::InvalidRequest(
                "both copies of this device's GPT are intact and agree, so there is nothing to \
                 repair"
                    .to_string(),
            )),
            GptRepair::NoGpt => {
                return Err(Error::InvalidRequest(
                    "this device has no GPT to repair. A repair rewrites a damaged copy from the \
                 intact one, and this device has no copy. To write a fresh table from a \
                 partition layout, author a GPT instead"
                        .to_string(),
                ));
            }
            GptRepair::Unrepairable { detail } => {
                return Err(Error::CorruptTable {
                    format: "GPT",
                    detail: format!(
                        "the primary GPT is damaged and the backup cannot repair it: {detail}"
                    ),
                });
            }
        };

    // Which copy is being rewritten (the segment's own name), and which is the
    // source (the action's). The source phrase for a primary repair keeps "backup
    // GPT" so a caller reading the plan sees the same words the read path used.
    let (rewriting, source_desc) = match source.direction {
        GptRepairDirection::PrimaryFromBackup => (
            "the primary GPT (sector 1 onward)",
            "the backup GPT in the device's last sector",
        ),
        GptRepairDirection::BackupFromPrimary => (
            "the backup GPT in the device's last sector",
            "the primary GPT at sector 1",
        ),
    };

    // The write lands in the GPT metadata region, outside every partition, so this
    // table -- the intact copy's -- is what says the write touches no partition,
    // honestly, rather than a `NoTable` that would deny the very table the repair
    // restores. The geometry and range checks every write gets are made all the same.
    let table = Table::Read(PartitionTable {
        format: TableFormat::Gpt,
        partitions: source.partitions.clone(),
        recovery: None,
    });
    let segment = table_segment(
        rewriting.to_string(),
        source.rebuilt.lba,
        source.rebuilt.bytes,
        &flash,
        &table,
        ceiling,
    )?;

    Ok(SegmentedPlan {
        format: TableFormat::Gpt,
        action: TableAction::Repair {
            source: source_desc.to_string(),
        },
        partitions: source.partitions,
        flash,
        chip_version,
        soc,
        read_back,
        segments: vec![segment],
    })
}

/// What a table write would do, in full, before any of it happens.
///
/// A table write repairs damaged copies of a table, or authors a fresh one. This is
/// the multi-segment counterpart of [`WritePlan`], for a write that can cover more
/// than one range. A caller renders the plan, and a person reads it. Only then does
/// [`SegmentedPlan::confirm`] turn it into the [`ConfirmedSegmentedWrite`] that
/// [`write_table`] accepts. Producing a plan and stopping there is the dry run.
///
/// It carries the loader evidence the wrong-loader gate compares. That is the same
/// gate [`flash`] runs, asked here by [`segmented_refusal`]. It also carries what a
/// person decides on: the [`segments`](Self::segments), with what each is and where
/// it lands, and the partitions the resulting table holds.
///
/// Every table sits at fixed sectors of a device-wide LBA space. A device without
/// one is refused before any table plan asks it anything, and again by
/// [`write_table`], as [`raw_lba_refusal`] describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentedPlan {
    /// The format of the table being written.
    pub format: TableFormat,
    /// Whether this repairs a table the board has or authors a fresh one, and the
    /// detail a person reads to tell which.
    pub action: TableAction,
    /// The partitions the written table will hold, in table order, which is how a
    /// person recognizes the table.
    ///
    /// For a repair, these are the intact copy's. For an authoring, they are the
    /// supplied layout's.
    pub partitions: Vec<Partition>,
    /// The geometry of the flash that contains the segments, as the device reports
    /// it.
    pub flash: FlashInfo,
    /// The loader's raw answer to which SoC it is on.
    ///
    /// It is the evidence the wrong-loader gate compares, exactly as
    /// [`WritePlan::chip_version`] is.
    pub chip_version: Vec<u8>,
    /// When this write can be proved to have landed, in the backend's own words.
    ///
    /// It is the same fact [`WritePlan::read_back`] carries, for the same reason.
    /// The read-back is mandatory, and the protocol decides *when* it happens. The
    /// plan a person confirms states which, so no screen or progress panel has to
    /// assume the rockusb answer.
    pub read_back: ReadBack,
    /// The SoC the caller named this write for, or `None`.
    ///
    /// It is the other half of the wrong-loader comparison. A write to a board with
    /// none is refused rather than guessed. A block device has no loader, and the
    /// gate exempts it.
    pub soc: Option<Soc>,
    /// The runs the write lays down, in the order it writes them.
    ///
    /// A repair has one per damaged copy, and an authored table has one per copy.
    pub segments: Vec<Segment>,
}

/// Whether a [`SegmentedPlan`] repairs a table or authors one, with the detail a
/// person reads to tell which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableAction {
    /// Rewrite the damaged copies from an intact one.
    ///
    /// The intact copy is described in words, such as "an intact parameter copy
    /// (offsets from sector 0x2000)".
    Repair {
        /// The intact copy the damaged ones are rebuilt from.
        source: String,
    },
    /// Lay down a fresh table from a supplied layout, on a device with no table or
    /// over the table it has.
    Author,
}

impl SegmentedPlan {
    /// Confirm this plan.
    ///
    /// It consumes the plan, so one confirmation permits one write and cannot be
    /// reused for a second.
    pub fn confirm(self) -> ConfirmedSegmentedWrite {
        ConfirmedSegmentedWrite(self)
    }
}

/// A [`SegmentedPlan`] a caller has agreed to, and the only thing [`write_table`]
/// takes.
// Not `Clone`, for the reason [`ConfirmedWrite`] is not: one yes, one write.
#[derive(Debug)]
pub struct ConfirmedSegmentedWrite(SegmentedPlan);

impl ConfirmedSegmentedWrite {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &SegmentedPlan {
        &self.0
    }
}

/// Why this planned table write would be refused, plan in hand, or `None`.
///
/// It is the wrong-loader gate over a [`SegmentedPlan`]: the multi-segment
/// counterpart of [`plan_refusal`], making the same comparison through the same
/// code. A loader that would lay a GPT or a parameter block at the wrong offsets is
/// refused. So is one that would write a boot image there. `None` means the write
/// would go ahead. [`write_table`] enforces this answer.
pub fn segmented_refusal<T: Transport>(
    agent: &FlashAgent<T>,
    plan: &SegmentedPlan,
) -> Option<Error> {
    loader_refusal(agent, plan.soc, &plan.chip_version)
}

/// Plan a repair of the device's Rockchip parameter table: the dry run.
///
/// It is the parameter counterpart of [`plan_repair_table`], which repairs a GPT.
/// It reads the device's geometry, the loader's account of itself, and every
/// parameter copy. Where a damaged copy has an intact sibling, it plans to rewrite
/// each damaged copy from the intact one. It sends nothing that changes the device.
///
/// It refuses the states it cannot act on, each with its reason:
///
/// - The device has no device-wide LBA space, so it has no fixed sectors for a
///   table, as [`raw_lba_refusal`] describes. Nothing is asked of it.
/// - Every copy passes validation, so there is nothing to repair.
/// - The device has no parameter table, so there is none to repair. Authoring one
///   is a different operation.
/// - Copies are damaged, and no intact copy is left. That is
///   [`Error::CorruptTable`]. On an eMMC, this is the usual form of a damaged
///   parameter, because an eMMC keeps a single copy.
///
/// [`ParamRepair`] describes the cases.
pub async fn plan_repair_param<T: Transport>(
    agent: &mut FlashAgent<T>,
    soc: Option<Soc>,
) -> Result<SegmentedPlan> {
    table_without_raw_lba(agent)?;
    let ceiling = agent.address_ceiling();
    let read_back = agent.read_back();
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;

    let source = match partition::read_param_repair_source(agent, &flash).await? {
        ParamRepair::Repairable(source) => source,
        ParamRepair::Healthy => {
            return Err(Error::InvalidRequest(
                "every copy of this device's Rockchip parameter is intact, so there is nothing \
                 to repair"
                    .to_string(),
            ));
        }
        ParamRepair::NoTable => {
            return Err(Error::InvalidRequest(
                "this device has no Rockchip parameter table to repair. To write a fresh table \
             from a layout, author a parameter table instead"
                    .to_string(),
            ));
        }
        ParamRepair::Unrepairable { detail } => {
            return Err(Error::CorruptTable {
                format: "Rockchip parameter",
                detail: format!("the parameter cannot be repaired from its own copies: {detail}"),
            });
        }
    };

    // The table the repaired copies restore -- for the plan a person reads, and
    // for the touches each segment reports.
    let table = Table::Read(PartitionTable {
        format: TableFormat::RockchipParam,
        partitions: source.partitions.clone(),
        recovery: None,
    });

    let segments = source
        .damaged_lbas
        .iter()
        .map(|&lba| {
            table_segment(
                format!("the Rockchip parameter copy at sector {lba:#x}"),
                lba,
                source.block.clone(),
                &flash,
                &table,
                ceiling,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SegmentedPlan {
        format: TableFormat::RockchipParam,
        action: TableAction::Repair {
            source: format!(
                "an intact Rockchip parameter copy (offsets counted from sector {:#x})",
                source.base_lba
            ),
        },
        partitions: source.partitions,
        flash,
        chip_version,
        soc,
        read_back,
        segments,
    })
}

/// Which medium a Rockchip parameter table is authored for.
///
/// It decides both the base the table's offsets are counted from, and where its
/// copies are written. It is the one fact about a parameter table that pyrographer
/// cannot read from the device. `K_FW_READ_FLASH_INFO` reports geometry, not the
/// kind of part. A legacy parameter's offsets count from sector `0x2000` on an
/// eMMC, and from zero on raw NAND. Authoring writes a fresh table, so the person
/// chooses the medium.
///
/// StarFive recovery asks for NOR or eMMC for the same reason: the medium cannot be
/// inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMedium {
    /// An eMMC: one copy at [`EMMC_BASE_LBA`](rkparam::EMMC_BASE_LBA), with
    /// offsets counted from there.
    Emmc,
    /// Raw NAND: the block is written several times, starting at sector zero, with
    /// offsets counted from zero.
    Nand,
}

impl ParamMedium {
    /// The base a parameter's offsets are counted from on this medium:
    /// [`EMMC_BASE_LBA`](rkparam::EMMC_BASE_LBA) for an eMMC, and zero for NAND.
    ///
    /// A front-end that reads an `mtdparts` line needs it to resolve the layout's
    /// offsets to absolute sectors on this medium.
    pub fn base_lba(self) -> u64 {
        match self {
            ParamMedium::Emmc => rkparam::EMMC_BASE_LBA,
            ParamMedium::Nand => 0,
        }
    }

    /// The base the offsets are counted from, and the sectors each copy is written
    /// to, on a part of `flash_sectors` sectors.
    ///
    /// A copy that would lie past the end of a small part is dropped, not written
    /// past the end.
    fn base_and_copies(self, flash_sectors: u64) -> (u64, Vec<u64>) {
        match self {
            ParamMedium::Emmc => {
                let base = rkparam::EMMC_BASE_LBA;
                let copies = rkparam::LOCATIONS
                    .iter()
                    .filter(|location| location.base_lba == base && location.lba < flash_sectors)
                    .map(|location| location.lba)
                    .collect();
                (base, copies)
            }
            ParamMedium::Nand => {
                let copies = rkparam::LOCATIONS
                    .iter()
                    .filter(|location| location.base_lba == 0 && location.lba < flash_sectors)
                    .map(|location| location.lba)
                    .collect();
                (0, copies)
            }
        }
    }
}

/// Where the parameter block an authoring lays down comes from.
///
/// The two sources differ in what the block carries beyond its partitions. A
/// [`Layout`](Self::Layout) is a partition list a person supplied, and the block
/// built from it is *minimal*: a `CMDLINE` and nothing else. [`Text`](Self::Text)
/// is an existing parameter's whole text, framed verbatim, so `FIRMWARE_VER`,
/// `MACHINE_MODEL` and every other key it carries are kept.
///
/// A layout authors a table from scratch. Text writes back a board's own table with
/// all its keys, as a `dump` of the parameter region would return it.
pub enum ParamAuthorSource<'a> {
    /// A partition layout: a fresh, minimal block is built from it.
    Layout(&'a Layout),
    /// An existing parameter's text, framed verbatim: the `KEY: value` lines, with
    /// `CMDLINE` among them.
    Text(&'a str),
}

/// Plan authoring a Rockchip parameter table from `source`: the dry run.
///
/// It builds a parameter block from a partition layout or from an existing
/// parameter's text, as [`ParamAuthorSource`] describes. It plans a write of the
/// block to every copy the `medium` keeps: one on an eMMC, several on raw NAND. It
/// sends nothing that changes the device. The block carries Rockchip's checksum,
/// so the planned table is one the board's own loader will read, as
/// [`rkparam::frame`] describes.
///
/// For either source, the partitions are validated against the geometry first.
/// Partitions that overlap or run off the end are refused before the write, rather
/// than found by the device during it.
///
/// From a layout, the offsets are rendered against the medium's base
/// ([`rkparam::render_mtdparts`]). A partition placed before that base cannot be
/// expressed, and is refused. From text, the block is parsed back at the medium's
/// base, to read out and check the partitions it names. Text whose offsets do not
/// suit the medium is therefore refused here, not discovered on the board.
///
/// A layout builds a *minimal* block: the partition list and nothing else. A
/// parameter's other keys belong to the board, not to a layout. Authoring from text
/// keeps them.
pub async fn plan_author_param<T: Transport>(
    agent: &mut FlashAgent<T>,
    source: ParamAuthorSource<'_>,
    medium: ParamMedium,
    soc: Option<Soc>,
) -> Result<SegmentedPlan> {
    table_without_raw_lba(agent)?;
    let ceiling = agent.address_ceiling();
    let read_back = agent.read_back();
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

    let (base_lba, copy_lbas) = medium.base_and_copies(flash_sectors);
    if copy_lbas.is_empty() {
        return Err(Error::InvalidRequest(format!(
            "the device has {flash_sectors} sectors, too few to hold a parameter copy anywhere its \
             medium keeps one"
        )));
    }

    // The block to write, and the partitions it holds -- from whichever source.
    let (block, partitions) = match source {
        ParamAuthorSource::Layout(layout) => {
            layout.validate(flash_sectors)?;

            // The layout's partitions, in the codec's shape, rendered to an
            // `mtdparts` line against the base and framed into a minimal block.
            let parts: Vec<rkparam::Partition> = layout
                .partitions
                .iter()
                .map(|part| rkparam::Partition {
                    name: part.name.clone(),
                    first_lba: part.first_lba,
                    sectors: part.sectors,
                })
                .collect();
            let mtdparts = rkparam::render_mtdparts(&parts, base_lba)?;
            let block = rkparam::frame(&format!("CMDLINE: mtdparts={mtdparts}\n"))?;

            let partitions = parts
                .into_iter()
                .map(|part| Partition {
                    name: part.name,
                    first_lba: part.first_lba,
                    sectors: part.sectors,
                })
                .collect();
            (block, partitions)
        }
        ParamAuthorSource::Text(text) => {
            // Frame the text verbatim, then read it back at the medium's base to
            // check its checksum and offsets and to show what it holds. A block
            // whose text has no partition list, or whose offsets do not suit the
            // medium, is refused here by the same parse a board's own block gets.
            let block = rkparam::frame(text)?;
            let param = rkparam::parse(&block, base_lba, flash_sectors)?;
            let partitions: Vec<Partition> = param
                .partitions
                .into_iter()
                .map(|part| Partition {
                    name: part.name,
                    first_lba: part.first_lba,
                    sectors: part.sectors,
                })
                .collect();

            // The same placement check the layout path gets, for a text a person
            // may have edited by hand. Asked of the partitions directly: the check
            // is about where they sit, and there is no layout here to ask it of.
            crate::layout::check_placement(&partitions, flash_sectors)?;
            (block, partitions)
        }
    };

    let table = Table::Read(PartitionTable {
        format: TableFormat::RockchipParam,
        partitions: partitions.clone(),
        recovery: None,
    });

    let segments = copy_lbas
        .iter()
        .map(|&lba| {
            table_segment(
                format!("the Rockchip parameter copy at sector {lba:#x}"),
                lba,
                block.clone(),
                &flash,
                &table,
                ceiling,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SegmentedPlan {
        format: TableFormat::RockchipParam,
        action: TableAction::Author,
        partitions,
        flash,
        chip_version,
        soc,
        read_back,
        segments,
    })
}

/// The [`gpt::derive_guid`] domain that synthesizes a disk GUID.
///
/// It is kept apart from the partition domain, so a disk GUID and a partition's
/// unique GUID cannot coincide, whatever material each is derived from.
const GPT_DOMAIN_DISK: u8 = 0;

/// The [`gpt::derive_guid`] domain that synthesizes a partition's unique GUID.
const GPT_DOMAIN_PARTITION: u8 = 1;

/// Resolve a partition layout into a fresh GPT.
///
/// Each type token maps to a type GUID. Each unique GUID, and the disk GUID, is
/// taken from the layout or synthesized. The table is then laid out. This is the
/// pure core of [`plan_author_gpt`]. It consults no device, so a caller can author
/// the bytes from geometry alone, for a preview or a test.
///
/// `flash_sectors` and `sector_size` are the device's. The layout is validated
/// against them before a byte is laid out: each partition must fit, and must not
/// overlap another. The GPT-specific checks are [`gpt::author`]'s: the reserved
/// front and back, the 128-entry ceiling, and the name field.
///
/// **The GUIDs are deterministic unless the layout pins them.** A partition's
/// unique GUID and the table's disk GUID are the person's where the layout pins
/// them ([`disk_guid`](Layout::disk_guid), [`unique_guid`]). Otherwise
/// [`gpt::derive_guid`] synthesizes them from the layout. An unpinned table
/// therefore authors the same bytes every time, and a pinned one carries exactly
/// the GUIDs asked for.
///
/// The disk GUID is resolved first, because a synthesized partition GUID mixes it
/// in. The same partition name in two different tables therefore gets two different
/// unique GUIDs.
///
/// [`unique_guid`]: crate::layout::LayoutPartition::unique_guid
pub fn author_gpt(
    layout: &Layout,
    flash_sectors: u64,
    sector_size: usize,
) -> Result<gpt::AuthoredGpt> {
    // Fit and overlap first: the same checks a parameter authoring makes. The
    // reserved-region checks gpt::author adds are the GPT's own.
    layout.validate(flash_sectors)?;

    // The disk GUID: the person's override, or synthesized from the whole layout so
    // different tables get different disk GUIDs.
    let disk_guid = match &layout.disk_guid {
        Some(text) => gpt::Guid::parse(text).map_err(|error| {
            Error::InvalidRequest(format!("the layout's disk-guid is invalid: {error}"))
        })?,
        None => {
            let mut material = Vec::new();
            for part in &layout.partitions {
                material.extend_from_slice(part.name.as_bytes());
                material.extend_from_slice(&part.first_lba.to_le_bytes());
                material.extend_from_slice(&part.sectors.to_le_bytes());
            }
            gpt::derive_guid(GPT_DOMAIN_DISK, &material)
        }
    };

    // Each partition: its type from the token, its unique GUID the person's override
    // or a synthesis mixing in the disk GUID and its own place in the table.
    let partitions = layout
        .partitions
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let type_guid = gpt::type_guid_for(part.kind.as_deref())?;
            let unique_guid = match &part.unique_guid {
                Some(text) => gpt::Guid::parse(text).map_err(|error| {
                    Error::InvalidRequest(format!(
                        "the uuid= for partition '{}' is invalid: {error}",
                        part.name
                    ))
                })?,
                None => {
                    let mut material = Vec::new();
                    material.extend_from_slice(&disk_guid.0);
                    material.extend_from_slice(&(index as u32).to_le_bytes());
                    material.extend_from_slice(part.name.as_bytes());
                    material.extend_from_slice(&part.first_lba.to_le_bytes());
                    material.extend_from_slice(&part.sectors.to_le_bytes());
                    gpt::derive_guid(GPT_DOMAIN_PARTITION, &material)
                }
            };
            Ok(gpt::AuthoredPartition {
                name: part.name.clone(),
                first_lba: part.first_lba,
                sectors: part.sectors,
                type_guid,
                unique_guid,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    gpt::author(&partitions, disk_guid, flash_sectors, sector_size)
}

/// Plan authoring a fresh GPT from `layout`: the dry run.
///
/// It is the GPT counterpart of [`plan_author_param`], and the authoring
/// counterpart of [`plan_repair_table`]. It reads the device's geometry and the
/// loader's account of itself. It authors the table from the layout with
/// [`author_gpt`], and plans a write of its two copies. The primary (protective MBR,
/// header, and entry array) goes from sector 0, and the backup at the end of the
/// device. It sends nothing that changes the device.
///
/// The plan is a [`SegmentedPlan`] with [`TableAction::Author`]. It is gated,
/// confirmed and written through [`write_table`], the one path every table write
/// takes. An authored GPT is therefore read back window by window, as every table
/// write is.
pub async fn plan_author_gpt<T: Transport>(
    agent: &mut FlashAgent<T>,
    layout: &Layout,
    soc: Option<Soc>,
) -> Result<SegmentedPlan> {
    table_without_raw_lba(agent)?;
    let ceiling = agent.address_ceiling();
    let read_back = agent.read_back();
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;
    let sector_size = flash.sector_size as usize;
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

    let authored = author_gpt(layout, flash_sectors, sector_size)?;

    // The partitions the authored table holds, in the layout's order -- what a person
    // recognizes it by, and what each segment reports it lands beside (which, for the
    // metadata a table write is, is no partition at all).
    let partitions: Vec<Partition> = layout
        .partitions
        .iter()
        .map(|part| Partition {
            name: part.name.clone(),
            first_lba: part.first_lba,
            sectors: part.sectors,
        })
        .collect();
    let table = Table::Read(PartitionTable {
        format: TableFormat::Gpt,
        partitions: partitions.clone(),
        recovery: None,
    });

    let primary = table_segment(
        "the primary GPT (protective MBR, header, and entry array from sector 0)".to_string(),
        authored.primary.lba,
        authored.primary.bytes,
        &flash,
        &table,
        ceiling,
    )?;
    let backup = table_segment(
        "the backup GPT in the device's last sectors".to_string(),
        authored.backup.lba,
        authored.backup.bytes,
        &flash,
        &table,
        ceiling,
    )?;

    Ok(SegmentedPlan {
        format: TableFormat::Gpt,
        action: TableAction::Author,
        partitions,
        flash,
        chip_version,
        soc,
        read_back,
        segments: vec![primary, backup],
    })
}

/// Build one segment of a table write, validating its range and reading what it
/// lands in from `table`.
///
/// A table segment gets exactly the geometry checks a plain write gets. The range
/// covers whole sectors, starts at an addressable sector, and ends inside both the
/// flash and the backend's own [`AddressCeiling`]. This function therefore runs
/// each segment through [`plan`], as [`plan_write`] does, and keeps its
/// [`touches`](WritePlan::touches).
///
/// The gate fields [`plan`] also takes do not apply to a segment, because a
/// segmented write gates once, on the device. An empty chip version and no SoC
/// therefore go in, and the loader fields of the result are dropped.
fn table_segment(
    what: String,
    lba: u64,
    bytes: Vec<u8>,
    flash: &FlashInfo,
    table: &Table,
    ceiling: AddressCeiling,
) -> Result<Segment> {
    let image_bytes = bytes.len() as u64;
    let sized = plan(
        lba,
        image_bytes,
        flash.clone(),
        Vec::new(),
        None,
        table,
        ReadBack::PerWindow,
        ceiling,
    )?;
    Ok(Segment {
        what,
        lba,
        bytes,
        touches: sized.touches,
    })
}

/// Write a confirmed table, either a repair's copies or an authored table's, and
/// read back every window it writes.
///
/// It runs the same write path as every other destructive verb. Its wrong-loader
/// gate is [`segmented_refusal`], enforced here and checked again on the loader in
/// hand. The mandatory window-by-window read-back is made by the loop every write
/// shares. The bytes travel on the confirmed plan, so what lands is exactly the
/// table a person saw. A [`Error::VerifyMismatch`] stops the write at that window,
/// as it does any other write.
pub async fn write_table<T: Transport>(
    agent: &mut FlashAgent<T>,
    confirmed: ConfirmedSegmentedWrite,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let plan = confirmed.plan();
    table_without_raw_lba(agent)?;

    // The same gate `flash` runs, against the recorded answer and then the loader
    // in hand -- a table write puts bytes on the flash, and the wrong loader puts
    // them at the wrong offsets as readily for a table as for anything else.
    loader_match_fields(agent, plan.soc, &plan.chip_version)?;
    reverify_loader_soc(agent, plan.soc).await?;

    write_segments(agent, &plan.segments, progress, cancel).await
}

/// What a firmware write lays down, in full, before any of it happens.
///
/// It is the plan for writing a Rockchip firmware package ([`plan_firmware`]), or a
/// loader's ID block alone ([`plan_write_id_block`]). A package write lays down
/// many runs under one consent: every partition image, a fresh GPT, and the ID
/// block. Each run is written and read back window by window, by the loop every
/// write shares. The first window that fails stops the whole write there.
///
/// It carries the loader evidence the wrong-loader gate compares, as [`WritePlan`]
/// does. It also carries two answers that gate an ID block. One is the loader
/// container's own claim about its SoC, and the other is the running loader's
/// capability reply. [`firmware_refusal`] asks all of them, and [`write_firmware`]
/// enforces the same answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwarePlan {
    /// What is being written.
    pub what: FirmwareWrite,
    /// The runs, in the order the write lays them down.
    pub runs: Vec<Run>,
    /// The partitions the device's table holds once the write is done.
    ///
    /// For a package, these are the parameter's, and the write lays the table down.
    /// For an ID block alone, they are the device's own, which the write leaves as
    /// they are.
    pub partitions: Vec<Partition>,
    /// The package's entries the write does not lay down, each with the reason.
    pub skipped: Vec<firmware::Skipped>,
    /// The flash stage the ID block's `RKNS` header came from.
    pub id_block_header: String,
    /// The images the ID block holds, as its header lists them.
    pub id_block_images: Vec<IdbImage>,
    /// The loader container's own claim about which SoC it was built for, raw.
    ///
    /// [`loader_blob_refusal`] judges a container's claim before an upload. A
    /// firmware plan judges it the same way, before the ID block built from that
    /// container is written.
    pub loader_chip: Option<[u8; 4]>,
    /// What the running loader said it can do, as far as an ID block is concerned.
    pub capability: LoaderCapability,
    /// The geometry of the flash, as the device reports it.
    pub flash: FlashInfo,
    /// The loader's raw answer to which SoC it is on. The wrong-loader gate
    /// compares it, as it compares [`WritePlan::chip_version`].
    pub chip_version: Vec<u8>,
    /// The SoC the caller named this write for, or `None`.
    pub soc: Option<Soc>,
    /// When the write is proved to have landed, in the backend's own words.
    pub read_back: ReadBack,
}

/// What a [`FirmwarePlan`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareWrite {
    /// A whole firmware package: its partition images, a GPT from its parameter,
    /// and the ID block from its loader.
    Package {
        /// The board model the package names.
        model: String,
        /// The manufacturer the package names.
        manufacturer: String,
        /// The package's version, as stored.
        /// [`version_text`](rkfw::version_text) renders it.
        version: u32,
    },
    /// A loader's ID block alone, at sector 64, with the partition table left as
    /// it is.
    IdBlock,
}

/// One contiguous run a [`FirmwarePlan`] lays down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// What the run is, in words for a person, such as "partition 'boot' from
    /// Image/boot.img".
    pub what: String,
    /// The first sector it lands on.
    pub lba: u64,
    /// How many bytes it writes, before the last sector is padded.
    pub bytes: u64,
    /// How many sectors it touches, padding included.
    pub sectors: u64,
    /// Where its bytes come from.
    pub source: RunSource,
    /// Which of the resulting table's partitions the run lands in.
    pub touches: Touches,
}

/// Where a [`Run`]'s bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSource {
    /// Bytes the plan holds: a GPT copy, or the ID block. They are small and travel
    /// with the plan, so what lands is exactly what a person saw.
    Held(Vec<u8>),
    /// A range of the firmware package, streamed from the file as the write runs.
    /// A partition image is gigabytes and is never held.
    Package {
        /// Where the range begins, counted from the start of the file.
        offset: u64,
    },
}

/// What the running loader said it can do, as far as writing an ID block goes.
///
/// Rockchip's rkdeveloptool writes an `RKNS` ID block only through a loader that
/// claims `NEW_IDB`. That is byte 1 bit 0 of the `K_FW_READ_CAPABILITY` reply, and
/// the tool refuses the write without it. **\[DOC\]**
///
/// [`firmware_refusal`] asks the same question, and refuses on the same answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderCapability {
    /// The device runs no loader, so there is nothing to ask. A block device is one.
    NoLoader,
    /// The loader answered.
    Answered(rockusb::Capability),
    /// The loader failed the query, with the reason. The agent is still in step
    /// with the device.
    NotAnswered(String),
}

impl FirmwarePlan {
    /// Confirm this plan.
    ///
    /// It consumes the plan, so one confirmation permits one write.
    pub fn confirm(self) -> ConfirmedFirmwareWrite {
        ConfirmedFirmwareWrite(self)
    }

    /// The bytes every run writes, together.
    pub fn total_bytes(&self) -> u64 {
        self.runs.iter().map(|run| run.bytes).sum()
    }

    /// Whether a run streams from the package, so the write needs the file again.
    pub fn needs_package(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run.source, RunSource::Package { .. }))
    }
}

/// A [`FirmwarePlan`] a caller has agreed to, and the only thing [`write_firmware`]
/// takes.
// Not `Clone`, for the reason [`ConfirmedWrite`] is not: one yes, one write.
#[derive(Debug)]
pub struct ConfirmedFirmwareWrite(FirmwarePlan);

impl ConfirmedFirmwareWrite {
    /// The plan that was confirmed.
    pub fn plan(&self) -> &FirmwarePlan {
        &self.0
    }
}

/// Plan writing the firmware `package` to the device: the dry run.
///
/// It reads the device's geometry, the loader's account of itself, and the
/// loader's capability reply. It sends nothing that changes the device. From the
/// package it lays out the runs, in the order [`write_firmware`] writes them:
///
/// 1. Every partition image, aimed at the partition the parameter names for it, in
///    the order the file holds them. The write streams them from the file, which
///    reads forward and never seeks, so the order is the file's.
/// 2. The primary GPT and the backup, authored from the parameter by
///    [`Package::layout`] and [`author_gpt`].
/// 3. The ID block, at sector 64, last.
///
/// Each of these is refused here, before anything is confirmed:
///
/// - A device with no device-wide LBA space, as [`raw_lba_refusal`] describes, or
///   one whose sectors are not 512 bytes, which every offset in a package counts
/// - A parameter that is not a GPT one, or a layout the device cannot hold
/// - An image entry that names no partition, or that is larger than its partition
/// - Two runs that overlap, and an ID block that lands inside a partition
pub async fn plan_firmware<T: Transport>(
    agent: &mut FlashAgent<T>,
    package: &Package,
    soc: Option<Soc>,
) -> Result<FirmwarePlan> {
    firmware_needs_raw_lba(agent)?;
    firmware_needs_512_byte_sectors(agent)?;
    let read_back = agent.read_back();
    let ceiling = agent.address_ceiling();
    let flash = agent.info().await?;
    let chip_version = agent.chip_version().await?;
    let capability = ask_capability(agent).await?;
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

    let layout = package.layout(flash_sectors)?;
    let authored = author_gpt(&layout, flash_sectors, firmware::SECTOR_LEN as usize)?;
    let partitions = layout.as_partitions();
    let table = Table::Read(PartitionTable {
        format: TableFormat::Gpt,
        partitions: partitions.clone(),
        recovery: None,
    });
    let resulting = table.require()?;

    let mut runs = Vec::with_capacity(package.images.len() + 3);
    for image in &package.images {
        let partition = resulting.find(&image.name).map_err(|_| {
            Error::InvalidRequest(format!(
                "firmware package: entry '{}' ({}) names no partition in the package's parameter, \
                 so it has nowhere to go. Its partitions are {}",
                image.name,
                image.path,
                resulting.names().join(", ")
            ))
        })?;
        partition.must_hold(image.bytes, flash.sector_size)?;
        runs.push(run(
            format!("partition '{}' from {}", image.name, image.path),
            partition.first_lba,
            image.bytes,
            RunSource::Package {
                offset: image.offset,
            },
            &flash,
            &table,
            ceiling,
        )?);
    }
    runs.push(run(
        "the primary GPT (protective MBR, header, and entry array from sector 0)".to_string(),
        authored.primary.lba,
        authored.primary.bytes.len() as u64,
        RunSource::Held(authored.primary.bytes),
        &flash,
        &table,
        ceiling,
    )?);
    runs.push(run(
        "the backup GPT in the device's last sectors".to_string(),
        authored.backup.lba,
        authored.backup.bytes.len() as u64,
        RunSource::Held(authored.backup.bytes),
        &flash,
        &table,
        ceiling,
    )?);
    runs.push(id_block_run(&package.id_block, &flash, &table, ceiling)?);
    runs_apart(&runs)?;

    Ok(FirmwarePlan {
        what: FirmwareWrite::Package {
            model: package.archive.model.clone(),
            manufacturer: package.archive.manufacturer.clone(),
            version: package.header.version,
        },
        runs,
        partitions,
        skipped: package.skipped.clone(),
        id_block_header: package.id_block.header_stage.clone(),
        id_block_images: package.id_block.images.clone(),
        loader_chip: package.loader.chip,
        capability,
        flash,
        chip_version,
        soc,
        read_back,
    })
}

/// Plan writing the ID block built from `loader` at sector 64, and nothing else:
/// the dry run.
///
/// It is pyrographer's form of rkdeveloptool's `ul`. The block is laid out from the
/// container's `RKNS` header and checked against every hash it records, as [`idb`]
/// describes. The block written is therefore the one its header describes. The
/// device's partition table is read so the plan can say the block lands in no
/// partition, and is left as it is. A block that would land inside a partition is
/// refused.
pub async fn plan_write_id_block<T: Transport>(
    agent: &mut FlashAgent<T>,
    loader: &LoaderImage,
    soc: Option<Soc>,
) -> Result<FirmwarePlan> {
    firmware_needs_raw_lba(agent)?;
    firmware_needs_512_byte_sectors(agent)?;
    let id_block = idb::build(loader)?;
    let read_back = agent.read_back();
    let ceiling = agent.address_ceiling();
    let (flash, chip_version, table) = survey(agent).await?;
    let capability = ask_capability(agent).await?;

    let partitions = match &table {
        Table::Read(read) => read.partitions.clone(),
        Table::Absent | Table::Damaged { .. } => Vec::new(),
    };
    let runs = vec![id_block_run(&id_block, &flash, &table, ceiling)?];

    Ok(FirmwarePlan {
        what: FirmwareWrite::IdBlock,
        runs,
        partitions,
        skipped: Vec::new(),
        id_block_header: id_block.header_stage,
        id_block_images: id_block.images,
        loader_chip: loader.chip,
        capability,
        flash,
        chip_version,
        soc,
        read_back,
    })
}

/// Why this planned firmware write would be refused, plan in hand, or `None`.
///
/// It asks, in order:
///
/// 1. The wrong-loader gate, as [`plan_refusal`] asks it of a [`WritePlan`].
/// 2. Whether the loader container claims the SoC the caller named, as
///    [`loader_blob_refusal`] asks it before an upload. The ID block is built from
///    that container, and the wrong one writes another SoC's first stage.
/// 3. Whether the running loader claims `NEW_IDB`, as the reference tool asks
///    before it writes an `RKNS` ID block. A loader that answers without it, or
///    fails the query, is refused. A device that runs no loader is not asked.
///
/// [`write_firmware`] enforces this answer through this function, so the refusal a
/// person is shown and the refusal the write makes cannot differ.
pub fn firmware_refusal<T: Transport>(agent: &FlashAgent<T>, plan: &FirmwarePlan) -> Option<Error> {
    if let Some(refusal) = loader_refusal(agent, plan.soc, &plan.chip_version) {
        return Some(refusal);
    }
    if let Some(refusal) = container_claim_refusal(plan.soc, plan.loader_chip) {
        return Some(refusal);
    }
    match &plan.capability {
        LoaderCapability::NoLoader => None,
        LoaderCapability::Answered(capability) if capability.new_idb() => None,
        LoaderCapability::Answered(_) => Some(Error::InvalidRequest(
            "the running loader's capability reply does not set NEW_IDB, and an RKNS ID block is \
             written only through a loader that claims it. Rockchip's own tool refuses the same \
             write for the same reason"
                .to_string(),
        )),
        LoaderCapability::NotAnswered(why) => Some(Error::InvalidRequest(format!(
            "the running loader did not answer the capability query ({why}), so it has not \
             claimed NEW_IDB. An RKNS ID block is written only through a loader that claims it"
        ))),
    }
}

/// Write a confirmed firmware plan, and read back every window it writes.
///
/// It runs the same write path as every other destructive verb. Its gate is
/// [`firmware_refusal`], enforced here, and the loader in hand is asked again for
/// its SoC before the first window. Each run goes through the loop every write
/// shares, so each is read back exactly as a single image is. The first window that
/// fails stops the whole write there. Nothing is retried and nothing is
/// rolled back, as [`flash`] explains.
///
/// `package` is the firmware package the plan was made from, read again from its
/// first byte. The partition images stream from it, forward. A plan that writes an
/// ID block alone needs none, and takes `None`. Progress covers every run as one
/// write.
pub async fn write_firmware<T: Transport>(
    agent: &mut FlashAgent<T>,
    confirmed: ConfirmedFirmwareWrite,
    package: Option<&mut dyn ImageReader>,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<()> {
    let plan = confirmed.plan();
    firmware_needs_raw_lba(agent)?;
    if let Some(refusal) = firmware_refusal(agent, plan) {
        return Err(refusal);
    }
    if plan.needs_package() && package.is_none() {
        return Err(Error::InvalidRequest(
            "this plan streams partition images from a firmware package, and no package was \
             given to the write"
                .to_string(),
        ));
    }
    reverify_loader_soc(agent, plan.soc).await?;

    let total_bytes = plan.total_bytes();
    progress(Progress::Started { total_bytes });

    let mut reader = package.map(ForwardReader::new);
    let mut done_before = 0u64;
    for run in &plan.runs {
        let mut advanced = |done| {
            progress(Progress::Advanced {
                done_bytes: done_before + done,
                total_bytes,
            })
        };
        match &run.source {
            RunSource::Held(bytes) => {
                let mut image = SyncReader::new(bytes.as_slice());
                write_windows::<T, T>(
                    agent,
                    run.lba,
                    run.bytes,
                    run.sectors,
                    ImageSource::Local(&mut image),
                    &mut advanced,
                    cancel,
                )
                .await?;
            }
            RunSource::Package { offset } => {
                // Checked before the gate; the plan's runs are its own.
                let Some(reader) = reader.as_mut() else {
                    unreachable!("a plan that streams from the package was given one");
                };
                reader.skip_to(*offset).await?;
                write_windows::<T, T>(
                    agent,
                    run.lba,
                    run.bytes,
                    run.sectors,
                    ImageSource::Local(reader),
                    &mut advanced,
                    cancel,
                )
                .await?;
            }
        }
        done_before += run.bytes;
    }

    progress(Progress::Finished {
        done_bytes: total_bytes,
    });
    Ok(())
}

/// Refuse a firmware write on a device with no device-wide LBA space.
fn firmware_needs_raw_lba<T: Transport>(agent: &FlashAgent<T>) -> Result<()> {
    match raw_lba_refusal(agent) {
        Some(why) => Err(Error::InvalidRequest(format!(
            "{why}. Firmware lays a partition table and an ID block at fixed sectors of a \
             device-wide LBA space, so this board has nowhere to write them"
        ))),
        None => Ok(()),
    }
}

/// Refuse a firmware write on a device whose sectors are not 512 bytes.
///
/// An `mtdparts` list counts 512-byte sectors, and so does an ID block, and the
/// BootROM reads the ID block from byte 32768. On a device with larger sectors,
/// every one of those numbers would land somewhere else.
fn firmware_needs_512_byte_sectors<T: Transport>(agent: &FlashAgent<T>) -> Result<()> {
    let sector = agent.sector_size();
    if sector != firmware::SECTOR_LEN {
        return Err(Error::InvalidRequest(format!(
            "this device's sectors are {sector} bytes. Firmware counts 512-byte sectors, in its \
             partition offsets and its ID block, so on this device each would land somewhere \
             else"
        )));
    }
    Ok(())
}

/// Ask the running loader what it can do, keeping a failed query as an answer.
///
/// A loader that fails the query and stays in step with the device is an answer:
/// [`firmware_refusal`] refuses on it. A query that leaves the agent out of step is
/// a failure of the plan itself, and is returned as one.
async fn ask_capability<T: Transport>(agent: &mut FlashAgent<T>) -> Result<LoaderCapability> {
    match agent.capability().await {
        Ok(Some(capability)) => Ok(LoaderCapability::Answered(capability)),
        Ok(None) => Ok(LoaderCapability::NoLoader),
        Err(error) if !agent.is_desynchronized() => {
            Ok(LoaderCapability::NotAnswered(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

/// Build one [`Run`], with the geometry checks every write gets, and what it lands
/// in from `table`.
fn run(
    what: String,
    lba: u64,
    bytes: u64,
    source: RunSource,
    flash: &FlashInfo,
    table: &Table,
    ceiling: AddressCeiling,
) -> Result<Run> {
    // The same pure `plan` every write is sized by, so a run gets exactly the range
    // and ceiling checks a single write gets. The gate fields do not apply: a
    // firmware write gates once, on the device.
    let sized = plan(
        lba,
        bytes,
        flash.clone(),
        Vec::new(),
        None,
        table,
        ReadBack::PerWindow,
        ceiling,
    )?;
    Ok(Run {
        what,
        lba,
        bytes,
        sectors: sized.sectors,
        source,
        touches: sized.touches,
    })
}

/// The ID block's run, at sector 64. A block that would land inside a partition is
/// refused: the write would destroy what the partition holds, and the partition's
/// next write would destroy the block.
fn id_block_run(
    id_block: &idb::IdBlock,
    flash: &FlashInfo,
    table: &Table,
    ceiling: AddressCeiling,
) -> Result<Run> {
    let run = run(
        format!(
            "the ID block, {} images laid out by its RKNS header",
            id_block.images.len()
        ),
        idb::LBA,
        id_block.bytes.len() as u64,
        RunSource::Held(id_block.bytes.clone()),
        flash,
        table,
        ceiling,
    )?;
    if let Touches::Partitions(overlaps) = &run.touches
        && let Some(first) = overlaps.first()
    {
        return Err(Error::InvalidRequest(format!(
            "the ID block fills sectors {} to {}, and partition '{}' lies across them. The block \
             belongs outside every partition",
            idb::LBA,
            idb::LBA + run.sectors - 1,
            first.name
        )));
    }
    Ok(run)
}

/// Refuse a plan whose runs overlap: one would overwrite another.
fn runs_apart(runs: &[Run]) -> Result<()> {
    let mut order: Vec<&Run> = runs.iter().collect();
    order.sort_by_key(|run| run.lba);
    for pair in order.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        if after.lba < before.lba + before.sectors {
            return Err(Error::InvalidRequest(format!(
                "{} and {} overlap at sector {}, so one would overwrite the other",
                before.what, after.what, after.lba
            )));
        }
    }
    Ok(())
}

/// End a connected device's session the way `mode` asks for.
///
/// [`ResetMode::Reset`](rockusb::ResetMode::Reset) is the plain reboot, and the
/// only mode a board has answered. The other three are corroborated and untried.
/// Each mode leaves the device in a state this connection cannot reach. Each
/// therefore has a name of its own, rather than being folded into one verb that
/// reboots.
///
/// The agent does not survive any of them. A caller holding one drops it and
/// rediscovers the device, as after a device leaves the bus on its own.
pub async fn reset<T: Transport>(
    agent: &mut FlashAgent<T>,
    mode: rockusb::ResetMode,
) -> Result<()> {
    agent.reset(mode).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::RockusbAgent;
    use crate::image::{SyncReader, SyncWriter};
    use crate::testing::{
        CHIP_VERSION, FLASH_SECTORS, a_boards_partitions, gpt_entry, param_block,
        scripted_chip_version, scripted_chip_version_of, scripted_gpt, scripted_info,
        scripted_info_of, scripted_no_table, scripted_plan, scripted_plan_with_gpt, scripted_read,
        scripted_transfer, scripted_write, sector,
    };
    use crate::transport::testing::{ScriptedTransport, Step};

    /// The ceiling a rockusb agent answers, for the `plan` tests that call the
    /// pure function directly rather than through an agent.
    const ROCKUSB_CEILING: AddressCeiling = AddressCeiling {
        past_last: 1u64 << 32,
        why: "the 32-bit LBA the rockusb command block carries cannot address it",
    };

    /// A scratch file standing in for a block device, and the agent over it.
    ///
    /// See [`crate::block::agent_over_file`] for what this does and does not
    /// prove. The file is named for the test that asked for it, so a failed test
    /// leaves an identifiable file behind rather than a bare number.
    #[cfg(target_os = "linux")]
    fn a_block_agent(
        what: &str,
        sectors: u64,
    ) -> (
        std::path::PathBuf,
        FlashAgent<crate::transport::testing::ScriptedTransport>,
    ) {
        a_block_agent_for(what, sectors, crate::block::Access::Write)
    }

    /// [`a_block_agent`] over a chosen access, for the read-only half.
    #[cfg(target_os = "linux")]
    fn a_block_agent_for(
        what: &str,
        sectors: u64,
        access: crate::block::Access,
    ) -> (
        std::path::PathBuf,
        FlashAgent<crate::transport::testing::ScriptedTransport>,
    ) {
        let path =
            std::env::temp_dir().join(format!("pyrographer-{what}-{}.img", std::process::id()));
        std::fs::write(&path, vec![0u8; (sectors * 512) as usize]).expect("a scratch file");
        let agent =
            crate::block::agent_over_file(&path, sectors * 512, 512, access).expect("an agent");
        (path, FlashAgent::Block(agent))
    }

    /// An image whose bytes are recognizable at a glance and differ from blank
    /// flash in every sector.
    #[cfg(target_os = "linux")]
    fn an_image(bytes: usize) -> Vec<u8> {
        (0..bytes).map(|i| (i % 251) as u8).collect()
    }

    /// A disk opened for reading reads, and refuses a write with the reason.
    ///
    /// A card with its lock switch on is the safest thing there is to image, and a
    /// common reason to use this backend. The open therefore does not ask a read
    /// the write refusals. Instead, the agent carries what it was opened for.
    /// `write_refusal` answers from it before anything is confirmed, and `flash`
    /// refuses from the same answer.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_disk_opened_for_reading_dumps_and_refuses_the_write() {
        let (path, mut agent) = a_block_agent_for("read-only", 256, crate::block::Access::Read);

        // The read half works: this is the whole point of opening at all.
        let mut out = Vec::new();
        pollster::block_on(dump(
            &mut agent,
            0,
            8,
            &mut SyncWriter::new(&mut out),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("a disk opened for reading is one that reads");
        assert_eq!(out.len(), 8 * 512);

        // The write half is refused, asked and enforced from the same answer.
        let why = write_refusal(&agent, None).expect("a read-only agent will not take a write");
        assert!(why.contains("reading"), "{why}");

        let plan = pollster::block_on(plan_write(&mut agent, 0, 4096, None))
            .expect("a plan is a read, and reads are what this agent does");
        let refused = plan_refusal(&agent, &plan).expect("the gate refuses before confirmation");
        assert!(matches!(refused, Error::InvalidRequest(_)), "{refused:?}");

        let image = an_image(4096);
        let error = pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(std::io::Cursor::new(image)),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("and the write path refuses it too");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let _ = std::fs::remove_file(&path);
    }

    /// A block write names no SoC, and neither the plan nor the write refuses it
    /// for that.
    ///
    /// A block device is exempt from the gate. It has no loader to be the wrong
    /// one, and the guard that replaces the gate ran at the open. This test pins
    /// that the exemption holds in *both* places the question is asked. If the
    /// write path agreed with `plan_refusal` and then refused anyway, the plan a
    /// front-end shows a person would be a guess.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_block_write_that_names_no_soc_is_planned_and_carried_out() {
        let (path, mut agent) = a_block_agent("no-soc", 256);
        let image = an_image(4096);

        let plan = pollster::block_on(plan_write(&mut agent, 8, image.len() as u64, None))
            .expect("a block device plans a write");
        assert!(
            plan_refusal(&agent, &plan).is_none(),
            "a block write names no SoC by design"
        );

        pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the write the plan promised");

        pollster::block_on(verify(
            &mut agent,
            8,
            &mut SyncReader::new(image.as_slice()),
            image.len() as u64,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("what was written is what is there");

        let on_disk = std::fs::read(&path).expect("the file");
        assert_eq!(&on_disk[8 * 512..8 * 512 + image.len()], &image[..]);
        assert!(
            on_disk[..8 * 512].iter().all(|b| *b == 0),
            "the write landed where the plan said and nowhere else"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The comparison the read-back makes catches a device that disagrees with the
    /// image.
    ///
    /// `flash` reads every window back before the next goes out. Observing that
    /// needs a device that returns the wrong bytes, and a file cannot. This test
    /// therefore makes the same comparison through `verify`, over a region in which
    /// one sector was overwritten after the write.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_block_verify_fails_when_the_device_disagrees_with_the_image() {
        let (path, mut agent) = a_block_agent("mismatch", 256);
        let image = an_image(4096);

        let plan = pollster::block_on(plan_write(&mut agent, 0, image.len() as u64, None))
            .expect("a plan");
        pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the write");

        let poison = vec![0xA5u8; 512];
        pollster::block_on(agent.write(2, &poison)).expect("a raw write, no gate and no read-back");

        let error = pollster::block_on(verify(
            &mut agent,
            0,
            &mut SyncReader::new(image.as_slice()),
            image.len() as u64,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the device no longer holds the image");
        assert!(
            matches!(error, Error::VerifyMismatch { offset, .. } if offset == 2 * 512),
            "{error:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A write that runs past the end of the device is refused at the plan,
    /// against the geometry the device itself reported.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_block_write_past_the_end_of_the_device_is_refused_before_anything_is_written() {
        let (path, mut agent) = a_block_agent("past-the-end", 64);
        let error = pollster::block_on(plan_write(&mut agent, 60, 8 * 512, None))
            .expect_err("60 + 8 sectors is past a 64-sector device");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        std::fs::remove_file(&path).ok();
    }

    /// A scripted answer to one 32-sector LBA read, filled with one byte.
    ///
    /// Thirty-two sectors is the most one command moves, so a longer read is split
    /// into units of this size.
    fn scripted_chunk(tag: u32, lba: u64, fill: u8) -> Vec<Step> {
        scripted_read(tag, lba, vec![fill; 32 * 512])
    }

    /// A loader file built for another SoC is refused, and nothing goes on the
    /// wire.
    ///
    /// The gate checks the file rather than the device, because once the upload
    /// starts there is no maskrom board left to ask.
    #[test]
    fn a_loader_file_for_another_soc_is_refused_before_any_transfer() {
        let mut loader = crate::codec::rkboot::LoaderImage::from_raw(
            Some(("471".to_string(), vec![1, 2, 3])),
            Some(("472".to_string(), vec![4, 5])),
        );
        loader.chip = Some(*b"8853");
        let soc = Soc::parse("rk3576").expect("pinned");

        let refusal = loader_blob_refusal(Some(soc), &loader).expect("it refuses");
        assert!(
            matches!(
                refusal,
                Error::LoaderBlobMismatch {
                    named: "rk3576",
                    ..
                }
            ),
            "it names the SoC the upload was aimed at: {refusal}"
        );

        // An empty script: any transfer at all is a test failure, and
        // `assert_drained` confirms none was even attempted.
        let mut transport = ScriptedTransport::new(vec![]);
        let error = pollster::block_on(download_boot(
            &mut transport,
            &loader,
            Some(soc),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the upload is refused");
        assert!(matches!(error, Error::LoaderBlobMismatch { .. }));
        transport.assert_drained();
    }

    /// The three ways the gate answers "go ahead", which are not all a check that
    /// passed.
    ///
    /// Each is a separate case, because turning any of them into a refusal would
    /// break a workflow that has nothing wrong with it.
    #[test]
    fn the_loader_gate_passes_a_match_and_everything_it_cannot_judge() {
        let soc = Soc::parse("rk3576").expect("pinned");
        let raw = || {
            crate::codec::rkboot::LoaderImage::from_raw(
                Some(("471".to_string(), vec![1])),
                Some(("472".to_string(), vec![2])),
            )
        };

        let mut claims_rk3576 = raw();
        claims_rk3576.chip = Some(*b"6753");
        assert!(
            loader_blob_refusal(Some(soc), &claims_rk3576).is_none(),
            "the container claims the SoC that was named"
        );

        assert!(
            loader_blob_refusal(None, &claims_rk3576).is_none(),
            "no SoC named: the upload stays the explicit ungated act it has always been"
        );

        assert!(
            loader_blob_refusal(Some(soc), &raw()).is_none(),
            "bare stage files carry no container and so make no claim to check"
        );

        // A container claiming a SoC nothing has pinned is judged against the SoC
        // that *was* named, and that comparison is real: it is a mismatch, not an
        // unjudgeable case. The unjudgeable case is a named SoC with no pinned
        // container value, which no pinned entry currently has -- so it is
        // covered in `soc`, over `claimed_by_container` directly.
        let mut claims_stranger = raw();
        claims_stranger.chip = Some([0x50, 0x00, 0x00, 0x00]);
        assert!(
            loader_blob_refusal(Some(soc), &claims_stranger).is_some(),
            "an unrecognized claim is still not the claim that was named"
        );
    }

    /// A dump of `windows` MiB, scripted chunk by chunk.
    ///
    /// One 1 MiB window is 64 LBA reads of 32 sectors each.
    fn scripted_dump(windows: u32) -> ScriptedTransport {
        let chunks_per_window = 64;
        let steps = (0..windows * chunks_per_window)
            .flat_map(|i| scripted_chunk(i + 1, u64::from(i) * 32, i as u8))
            .collect();
        ScriptedTransport::new(steps)
    }

    #[test]
    fn dump_streams_to_the_sink_and_reports_progress_per_window() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(scripted_dump(2)));
        let mut out = SyncWriter::new(Vec::new());
        let mut events = Vec::new();

        let report = pollster::block_on(dump(
            &mut agent,
            0,
            2 * 2048, // two 1 MiB windows, in 512-byte sectors
            &mut out,
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("dump should succeed");

        // Each 16 KiB chunk is a different byte, so nothing reaches the 1 MiB
        // fill threshold: varied data is not flagged.
        assert!(report.is_empty());
        assert_eq!(out.into_inner().len(), 2 * 1024 * 1024);
        assert_eq!(
            events,
            vec![
                Progress::Started {
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Advanced {
                    done_bytes: 1024 * 1024,
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Advanced {
                    done_bytes: 2 * 1024 * 1024,
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Finished {
                    done_bytes: 2 * 1024 * 1024
                },
            ]
        );
    }

    #[test]
    fn a_dump_canceled_mid_flight_stops_at_the_next_window() {
        // The script has only one window's worth of steps. The token is flipped
        // once that window lands, so if cancellation were ignored the transport
        // would panic on running out of script -- which makes this a real test
        // and not a tautology.
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(scripted_dump(1)));
        let cancel = Cancel::new();
        let mut out = SyncWriter::new(Vec::new());

        let error = pollster::block_on(dump(
            &mut agent,
            0,
            100 * 2048, // ask for 100 MiB
            &mut out,
            &mut |event| {
                if let Progress::Advanced { .. } = event {
                    cancel.cancel();
                }
            },
            &cancel,
        ))
        .expect_err("the dump was canceled");

        assert!(matches!(error, Error::Canceled));
        // The window that had already landed is still written out.
        assert_eq!(out.into_inner().len(), 1024 * 1024);
    }

    #[test]
    fn a_canceled_token_stops_a_dump_before_it_touches_the_device() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        let cancel = Cancel::new();
        cancel.cancel();

        let error = pollster::block_on(dump(
            &mut agent,
            0,
            2048,
            &mut SyncWriter::new(Vec::new()),
            &mut |_| {},
            &cancel,
        ))
        .expect_err("the dump was canceled before it began");

        assert!(matches!(error, Error::Canceled));
    }

    /// A scripted dump that reads back as `byte` across all `sectors`.
    ///
    /// This is the read wall in miniature. The device answers every read with a
    /// success status and a buffer of fill, so a dump that trusted the status would
    /// look complete. The dump returns the finding, so a caller can report it.
    fn scripted_constant_dump(sectors: u32, byte: u8) -> ScriptedTransport {
        let chunks = sectors / 32;
        let steps = (0..chunks)
            .flat_map(|i| scripted_chunk(i + 1, u64::from(i) * 32, byte))
            .collect();
        ScriptedTransport::new(steps)
    }

    #[test]
    fn a_dump_across_constant_fill_reports_it_as_suspicious() {
        // 2 MiB, all 0xcc: past the 1 MiB threshold, and not a blank value.
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(scripted_constant_dump(4096, 0xcc)));
        let mut out = SyncWriter::new(Vec::new());

        let report = pollster::block_on(dump(
            &mut agent,
            0,
            4096,
            &mut out,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the dump itself succeeds -- the fill is a finding, not a failure");

        assert!(report.has_suspicious());
        let run = *report.suspicious().next().expect("one suspicious run");
        assert_eq!(run.first_lba(), 0);
        assert_eq!(run.sectors(), 4096);
        assert_eq!(run.byte(), 0xcc);
        assert_eq!(run.bytes(), 2 * 1024 * 1024);

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    #[test]
    fn a_dump_of_blank_flash_is_reported_as_blank_not_suspicious() {
        // 2 MiB of 0x00: past the threshold, but the ordinary look of unallocated
        // flash, so it is noted and not warned about.
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(scripted_constant_dump(4096, 0x00)));

        let report = pollster::block_on(dump(
            &mut agent,
            0,
            4096,
            &mut SyncWriter::new(Vec::new()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the dump succeeds");

        assert!(!report.has_suspicious());
        assert_eq!(report.blank().count(), 1);
        assert_eq!(report.blank().next().unwrap().byte(), 0x00);

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A difference is reported as a `VerifyMismatch`, with the three facts a
    /// caller needs to act on it.
    ///
    /// The offset is into the image, not into the window the difference landed in.
    /// A protocol error would say the exchange with the device failed, but the
    /// exchange is what produced the answer. The write path's read-back depends on
    /// telling those two apart.
    #[test]
    fn verify_reports_a_difference_as_a_mismatch_and_names_the_byte() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_chunk(1, 0, 0xaa),
        )));

        let mut image = vec![0xaa; 32 * 512];
        image[1234] = 0xff;

        let error = pollster::block_on(verify(
            &mut agent,
            0,
            &mut SyncReader::new(image.as_slice()),
            image.len() as u64,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the image does not match the flash");

        assert!(
            matches!(
                error,
                Error::VerifyMismatch {
                    offset: 1234,
                    found: 0xaa,
                    expected: 0xff,
                }
            ),
            "{error:?}"
        );
    }

    /// Two windows are compared one at a time, never both in memory, with progress
    /// reported the way `dump` reports it.
    ///
    /// The script pins this. A `verify` that read the whole range in one command
    /// would put a different CBW on the wire, and the transport would fail the test.
    #[test]
    fn verify_streams_window_by_window_and_reports_progress() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(scripted_dump(2)));
        let mut events = Vec::new();

        // What `scripted_dump` answers with: chunk `i` comes back filled with
        // `i`, and there are 64 chunks to a 1 MiB window.
        let image: Vec<u8> = (0..2 * 64u32)
            .flat_map(|i| std::iter::repeat_n(i as u8, 32 * 512))
            .collect();
        assert_eq!(image.len(), 2 * 1024 * 1024);

        let report = pollster::block_on(verify(
            &mut agent,
            0,
            &mut SyncReader::new(image.as_slice()),
            image.len() as u64,
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("the image matches the flash");

        // The device returned varied data, chunk by chunk, so there is no fill
        // finding to sit beside the match.
        assert!(report.is_empty());
        assert_eq!(
            events,
            vec![
                Progress::Started {
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Advanced {
                    done_bytes: 1024 * 1024,
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Advanced {
                    done_bytes: 2 * 1024 * 1024,
                    total_bytes: 2 * 1024 * 1024
                },
                Progress::Finished {
                    done_bytes: 2 * 1024 * 1024
                },
            ]
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The device addresses sectors, and an image need not end on a sector boundary.
    ///
    /// The last sector is read in full, because no read can be shorter, and compared
    /// only over the bytes the image has. The image makes no claim about the flash
    /// beyond them. Here that flash differs deliberately, so a comparison that ran
    /// to the end of the sector would fail.
    #[test]
    fn verify_of_an_image_that_ends_mid_sector_compares_only_the_image() {
        let mut sector = vec![0xaa; 512];
        sector[412..].fill(0xff); // flash past the image, and unlike it

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_read(1, 0, sector),
        )));

        let image = vec![0xaa; 412];
        pollster::block_on(verify(
            &mut agent,
            0,
            &mut SyncReader::new(image.as_slice()),
            image.len() as u64,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the image matches every byte it claims");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// An image shorter than its stated length means the caller's file is wrong, not
    /// that the flash disagrees with it.
    ///
    /// A `VerifyMismatch` here would blame the board for the host's arithmetic.
    #[test]
    fn verify_against_an_image_that_runs_out_is_an_io_error_not_a_mismatch() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_read(1, 0, vec![0xaa; 512]),
        )));

        let image = vec![0xaa; 100];
        let error = pollster::block_on(verify(
            &mut agent,
            0,
            &mut SyncReader::new(image.as_slice()),
            512, // it is not 512 bytes long
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the image ran out");

        assert!(matches!(error, Error::Io(_)), "{error:?}");
    }

    /// The plan is the dry run: it asks the device, works out what the write would
    /// touch, and stops.
    ///
    /// A person reads what it reports before confirming, so this test checks every
    /// field. That includes the loader's own answer to what it is running on, which
    /// is the evidence the person is confirming against.
    #[test]
    fn a_plan_says_exactly_what_the_write_would_touch() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let plan = pollster::block_on(plan_write(&mut agent, 100, 1000, None)).expect("a plan");

        assert_eq!(plan.lba, 100);
        assert_eq!(plan.image_bytes, 1000);
        // 1000 bytes is two sectors, the second of them 24 bytes short of full.
        assert_eq!(plan.sectors, 2);
        assert_eq!(plan.padding_bytes, 24);
        assert_eq!(plan.flash.size_bytes, FLASH_SECTORS * 512);
        assert_eq!(plan.chip_version, CHIP_VERSION);

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A write that runs off the end of the part is refused whole, not truncated to
    /// fit.
    ///
    /// A caller that asked for something impossible has an intent that is not known.
    #[test]
    fn a_plan_that_runs_past_the_end_of_the_flash_is_refused() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let error = pollster::block_on(plan_write(&mut agent, FLASH_SECTORS - 1, 4096, None))
            .expect_err("eight sectors do not fit in the one that is left");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A zero-byte image writes nothing, and is refused at the plan rather than
    /// rendered as a successful no-op over an inverted range.
    #[test]
    fn a_zero_byte_image_is_refused_at_the_plan() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let error = pollster::block_on(plan_write(&mut agent, 0, 0, None))
            .expect_err("zero bytes is nothing to write");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The gate, end to end.
    ///
    /// A plan is the only source of a `ConfirmedWrite`, and a `ConfirmedWrite` is
    /// the only thing `flash` takes, so the compiler enforces that somebody
    /// confirmed. A plan that named no SoC is refused, because the gate does not
    /// guess.
    ///
    /// It refuses having written nothing. The script holds only the plan's
    /// commands. A `flash` that reached the write would run past the end of the
    /// script and panic.
    #[test]
    fn a_write_that_named_no_soc_refuses_having_written_nothing() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let plan = pollster::block_on(plan_write(&mut agent, 0, 1024, None)).expect("a plan");
        let confirmed = plan.confirm();
        assert_eq!(confirmed.plan().sectors, 2);

        let image = vec![0u8; 1024];
        let error = pollster::block_on(flash(
            &mut agent,
            confirmed,
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("no SoC was named, so the gate has nothing to compare");

        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The armed gate, refusing: a loader that answers as a different SoC than the
    /// plan named writes nothing.
    ///
    /// The script ends at the plan's commands, so a write that got past the gate
    /// would run past the end and panic. The error carries both byte strings,
    /// because the person who sees it needs to see what the gate compared.
    #[test]
    fn a_write_refuses_a_loader_that_answers_as_a_different_soc() {
        // "8853": the answer reference material reports for an RK3588 -- a real
        // loader, on the wrong board.
        let stranger = [0x38, 0x38, 0x35, 0x33];
        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version_of(3, &stranger));
        steps.extend(scripted_no_table(4));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_write(&mut agent, 0, 1024, Some(soc))).expect("a plan");

        let image = vec![0u8; 1024];
        let error = pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the loader answered as an RK3588, and the plan named rk3576");

        match error {
            Error::LoaderMismatch {
                named,
                expected,
                answered,
            } => {
                assert_eq!(named, "rk3576");
                assert_eq!(expected, soc.pinned_reply());
                assert_eq!(answered, stranger);
            }
            other => panic!("the gate refuses with the comparison it made: {other:?}"),
        }

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The armed gate, passing: the loader answers as the SoC the plan named, and
    /// the write goes through.
    ///
    /// It is still written, read back and compared, because passing the gate permits
    /// the write and does not replace the read-back.
    #[test]
    fn a_write_planned_for_the_soc_the_loader_answers_as_goes_through() {
        let image = vec![0x5a; 2 * 512];
        // The plan's commands run the agent's tag counter to 13. Then `flash`
        // re-asks the loader (command 14) before writing, so the write is command
        // 15 and its read-back 16.
        let mut steps = scripted_plan(1);
        steps.extend(scripted_chip_version(14));
        steps.extend(scripted_write(15, 0, image.clone()));
        steps.extend(scripted_read(16, 0, image.clone()));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_write(&mut agent, 0, image.len() as u64, Some(soc)))
            .expect("a plan");

        pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the loader is the one the write was planned for");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// What a caller is told before it asks a person to confirm, and what the
    /// write does after confirmation, are one answer from one call.
    ///
    /// A front-end grays out its write button on the first answer, and produces
    /// its `ConfirmedWrite` against the second. If the two came from different
    /// rules, a person could type a confirmation for a write that was never going
    /// to happen. Worse, a person might not be asked to confirm a write that was.
    #[test]
    fn the_refusal_a_caller_is_shown_is_the_refusal_the_write_makes() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));
        let refused = write_refusal(&agent, None).expect("no SoC named is a standing refusal");

        let plan = pollster::block_on(plan_write(&mut agent, 0, 1024, None)).expect("a plan");
        let image = vec![0u8; 1024];
        let error = pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(image.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("and refuses on the same answer when the write is attempted");

        assert!(
            matches!(&error, Error::InvalidRequest(why) if why == &refused),
            "the write refused for a different reason than the caller was shown: {error:?}"
        );
    }

    /// A local-image source, as `flash` builds one.
    ///
    /// The source names no device, so nothing constrains its transport parameter,
    /// and the caller must name one. `flash` names its own, and a test names the
    /// scripted one.
    fn local<'a>(image: &'a mut dyn ImageReader) -> ImageSource<'a, ScriptedTransport> {
        ImageSource::Local(image)
    }

    /// Drive [`write_verified`] from a whole [`WritePlan`].
    ///
    /// [`write_verified`] takes the plan's three write fields as separate
    /// parameters. The tests that pin the shared window loop say where and how much
    /// to write with a plan, so this helper unpacks one.
    async fn write_verified_from_plan<S: Transport>(
        agent: &mut FlashAgent<ScriptedTransport>,
        plan: &WritePlan,
        source: ImageSource<'_, S>,
        progress: ProgressSink<'_>,
        cancel: &Cancel,
    ) -> Result<FillReport> {
        write_verified(
            agent,
            plan.lba,
            plan.image_bytes,
            plan.sectors,
            source,
            progress,
            cancel,
        )
        .await
    }

    // --- The DFU write path: one session, committed, then read back whole -----

    use crate::codec::dfu;
    use crate::codec::dfu_alt::AltSetting;
    use std::io::Cursor;

    /// The transfer size the DFU write tests drive, which is also the DFU
    /// backend's sector size.
    const DFU_XFER: u16 = 64;
    const DFU_IFACE: u16 = 0;

    /// A scripted DFU board exposing one alt-setting of `blocks` blocks.
    fn dfu_board(blocks: u64, attributes: u8, steps: Vec<Step>) -> FlashAgent<ScriptedTransport> {
        FlashAgent::Dfu(crate::agent::DfuAgent::new(
            ScriptedTransport::new(steps),
            DFU_IFACE,
            crate::testing::dfu_functional(DFU_XFER, attributes),
            vec![AltSetting {
                index: 0,
                name: "boot".to_string(),
                size: Some(blocks * u64::from(DFU_XFER)),
            }],
        ))
    }

    /// The steps a DFU write of `image` puts on the wire.
    ///
    /// The write selects the region, then sends a `DNLOAD` and a ready poll per
    /// block. It ends with the zero-length block and manifestation.
    fn dfu_write_steps(image: &[u8]) -> Vec<Step> {
        let mut steps = vec![crate::testing::dfu_set_interface(DFU_IFACE, 0)];
        for (index, block) in image.chunks(DFU_XFER as usize).enumerate() {
            steps.push(crate::testing::dfu_out(
                dfu::download(DFU_IFACE, index as u16, block.len() as u16),
                block.to_vec(),
            ));
            steps.push(crate::testing::dfu_ok(DFU_IFACE, dfu::State::DownloadIdle));
        }
        let last = image.len().div_ceil(DFU_XFER as usize) as u16;
        steps.push(crate::testing::dfu_out(
            dfu::download(DFU_IFACE, last, 0),
            Vec::new(),
        ));
        steps.push(crate::testing::dfu_ok(DFU_IFACE, dfu::State::DfuIdle));
        steps
    }

    /// The steps the post-commit read-back puts on the wire: select the region
    /// again, then an `UPLOAD` per block answering with `served`.
    fn dfu_readback_steps(served: &[u8]) -> Vec<Step> {
        let mut steps = vec![crate::testing::dfu_set_interface(DFU_IFACE, 0)];
        for (index, block) in served.chunks(DFU_XFER as usize).enumerate() {
            steps.push(crate::testing::dfu_in(
                dfu::upload(DFU_IFACE, index as u16, DFU_XFER),
                block.to_vec(),
            ));
        }
        steps
    }

    /// The whole DFU write.
    ///
    /// The blocks stream into one download session, and the zero-length block
    /// commits it. Only then is the region read back and checked, which is
    /// [`ReadBack::AfterCommit`] in the order it happens. The script asserts every
    /// byte. It therefore pins that the read-back comes *after* the commit and
    /// covers the *whole* region, not only that some check ran.
    #[test]
    fn a_dfu_write_commits_the_session_and_then_reads_the_region_back() {
        let image = vec![0x5a; 4 * DFU_XFER as usize];

        let mut steps = dfu_write_steps(&image);
        steps.extend(dfu_readback_steps(&image));
        let mut agent = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, steps);

        let mut reader = SyncReader::new(Cursor::new(image.clone()));
        pollster::block_on(write_verified(
            &mut agent,
            0,
            image.len() as u64,
            4,
            ImageSource::<ScriptedTransport>::Local(&mut reader),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the region reads back as it was written");
    }

    /// A region that reads back different is a
    /// [`CommitMismatch`](Error::CommitMismatch), which names the window rather than
    /// the byte.
    ///
    /// The image was streamed once and never held, so no byte is left to point at.
    /// The whole region **was** written, which a per-window mismatch does not imply,
    /// and the error has to say so.
    #[test]
    fn a_dfu_region_that_reads_back_different_is_a_commit_mismatch() {
        let image = vec![0x5a; 4 * DFU_XFER as usize];
        let mut wrong = image.clone();
        wrong[200] ^= 0xff; // one bit, in the third block

        let mut steps = dfu_write_steps(&image);
        steps.extend(dfu_readback_steps(&wrong));
        let mut agent = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, steps);

        let mut reader = SyncReader::new(Cursor::new(image.clone()));
        let error = pollster::block_on(write_verified(
            &mut agent,
            0,
            image.len() as u64,
            4,
            ImageSource::<ScriptedTransport>::Local(&mut reader),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the flash does not hold what was sent");

        let Error::CommitMismatch {
            offset,
            window_bytes,
        } = error
        else {
            panic!("a post-commit difference is a CommitMismatch, not {error:?}");
        };
        assert_eq!(offset, 0, "the whole region is one window here");
        assert_eq!(window_bytes, image.len() as u64);
    }

    /// A device that detaches as it commits is refused before a byte goes out.
    ///
    /// The script is empty, so a write that sent anything would fail on the
    /// transport rather than on the assertion. pyrographer does not start a write
    /// it cannot check.
    #[test]
    fn a_dfu_device_that_cannot_be_read_back_is_refused_before_it_is_written() {
        let image = vec![0x5a; 4 * DFU_XFER as usize];
        let mut agent = dfu_board(4, crate::testing::DFU_DETACHES_ON_COMMIT, Vec::new());

        let mut reader = SyncReader::new(Cursor::new(image.clone()));
        let error = pollster::block_on(write_verified(
            &mut agent,
            0,
            image.len() as u64,
            4,
            ImageSource::<ScriptedTransport>::Local(&mut reader),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("an unverifiable write is not made");
        assert!(matches!(error, Error::NotImplemented(_)), "{error:?}");
    }

    /// The DFU write refusal names the gate that is shut.
    ///
    /// The write path is built, so the refusal names the gate rather than missing
    /// code. The two reasons a DFU write is refused are told apart, because they
    /// need different remedies. An unpinned SoC needs bench work. A device that
    /// cannot be read back is a property of that device.
    #[test]
    fn the_dfu_write_refusal_names_which_gate_is_shut() {
        let capable = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());
        let why = write_refusal(&capable, None).expect("no Ingenic SoC is pinned");
        assert!(
            why.contains("No Ingenic SoC is pinned"),
            "the gate that is actually shut: {why}"
        );
        assert!(
            !why.contains("not wired in"),
            "the write path is built now: {why}"
        );

        let detaches = dfu_board(4, crate::testing::DFU_DETACHES_ON_COMMIT, Vec::new());
        let why = write_refusal(&detaches, None).expect("it could not be read back");
        assert!(
            why.contains("manifestation-tolerant"),
            "the device's own limit comes first: {why}"
        );
        assert!(
            why.contains("cannot read back"),
            "and what pyrographer does about it: {why}"
        );
    }

    /// The plan a person reads carries when the check happens, because that decides
    /// whether a mismatch costs one window or the whole region.
    ///
    /// A rockusb plan says per window, and a DFU plan says after the commit.
    #[test]
    fn a_plan_says_when_the_write_would_be_checked() {
        let mut dfu = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());
        let plan = pollster::block_on(plan_write_partition(&mut dfu, "boot", 64, None))
            .expect("a DFU plan needs no transfers");
        assert_eq!(plan.read_back, ReadBack::AfterCommit);

        let rockusb =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));
        assert_eq!(rockusb.read_back(), ReadBack::PerWindow);
    }

    /// The raw-LBA refusal answers from the capability, so the two cannot disagree.
    ///
    /// A front-end grays a control on one and the verbs refuse on the other. A board
    /// that a raw-LBA write could reach, refused anyway, or the reverse, is the drift
    /// this pins.
    #[test]
    fn the_raw_lba_refusal_follows_the_capability() {
        let rockusb = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(Vec::new())));
        assert!(rockusb.caps().can_address_raw_lba);
        assert_eq!(raw_lba_refusal(&rockusb), None);

        let dfu = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());
        assert!(!dfu.caps().can_address_raw_lba);
        let why = raw_lba_refusal(&dfu).expect("a DFU board has no device-wide LBA space");
        assert!(why.contains("named region"), "{why}");

        #[cfg(target_os = "linux")]
        {
            let (path, disk) = a_block_agent("raw-lba-refusal", 8);
            assert!(disk.caps().can_address_raw_lba);
            assert_eq!(raw_lba_refusal(&disk), None);
            std::fs::remove_file(&path).ok();
        }
    }

    /// A board with no device-wide LBA space refuses every write that addresses one,
    /// before it is asked anything.
    ///
    /// A DFU board reaches its flash only by named region. A sector number and a
    /// partition table both address a device-wide LBA space. Without this refusal,
    /// pyrographer's packed DFU addressing would land a GPT authored for the board
    /// inside its first alt-setting. The script is empty, so a single question to
    /// the device would fail on the transport rather than on the assertion.
    ///
    /// A write aimed at a region by name still plans, because the device names the
    /// region.
    #[test]
    fn a_board_with_no_device_wide_lba_space_refuses_every_write_that_addresses_one() {
        let board = || dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());
        let refused = |result: Result<SegmentedPlan>, what: &str| {
            let error = result.expect_err(what);
            assert!(
                matches!(error, Error::InvalidRequest(_)),
                "{what}: {error:?}"
            );
            assert!(
                error.to_string().contains("nowhere to write one"),
                "{what}: {error}"
            );
        };
        let layout = Layout::parse_native("boot 0x2000 0x100\n", FLASH_SECTORS).unwrap();

        let error =
            pollster::block_on(plan_write(&mut board(), 0, 64, None)).expect_err("a raw-LBA write");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        assert!(error.to_string().contains("by name"), "{error}");

        refused(
            pollster::block_on(plan_repair_table(&mut board(), None)),
            "a GPT repair",
        );
        refused(
            pollster::block_on(plan_repair_param(&mut board(), None)),
            "a parameter repair",
        );
        refused(
            pollster::block_on(plan_author_param(
                &mut board(),
                ParamAuthorSource::Layout(&layout),
                ParamMedium::Emmc,
                None,
            )),
            "a parameter authoring",
        );
        refused(
            pollster::block_on(plan_author_gpt(&mut board(), &layout, None)),
            "a GPT authoring",
        );

        pollster::block_on(plan_write_partition(&mut board(), "boot", 64, None))
            .expect("a write aimed by name still plans");
    }

    /// A confirmed table write is refused on a board with no device-wide LBA space,
    /// even though no plan for one could have been made.
    ///
    /// The plan is the front door, and `write_table` is where the write itself is
    /// enforced. A plan made on one board and handed to another would otherwise
    /// write its table into whatever the second board's addressing makes of it.
    #[test]
    fn a_table_write_handed_to_a_board_with_no_device_wide_lba_space_is_refused() {
        let plan = SegmentedPlan {
            format: TableFormat::Gpt,
            action: TableAction::Author,
            partitions: Vec::new(),
            flash: FlashInfo {
                size_bytes: FLASH_SECTORS * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: CHIP_VERSION.to_vec(),
            soc: None,
            read_back: ReadBack::PerWindow,
            segments: vec![Segment {
                what: "the primary GPT".to_string(),
                lba: 0,
                bytes: vec![0u8; 512],
                touches: Touches::NoTable,
            }],
        };
        let mut dfu = dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());

        let error = pollster::block_on(write_table(
            &mut dfu,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("nothing is written");
        assert!(
            error.to_string().contains("nowhere to write one"),
            "{error}"
        );
    }

    /// A clone with a board on either end that has no device-wide LBA space is
    /// refused, and the refusal names which end.
    ///
    /// The two scripts are empty, so neither board is asked anything. The DFU
    /// board's 64-byte sector also differs from the rockusb board's 512. The
    /// refusal is about the addressing and not the sector size, which pins that it
    /// is asked first.
    #[test]
    fn a_clone_with_a_board_that_has_no_device_wide_lba_space_is_refused() {
        let dfu = || dfu_board(4, crate::testing::DFU_FULLY_CAPABLE, Vec::new());
        let rockusb = || FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(Vec::new())));

        let error = pollster::block_on(plan_clone(&mut dfu(), &mut rockusb(), None))
            .expect_err("a DFU source");
        let words = error.to_string();
        assert!(
            words.contains("the source cannot take part") && words.contains("named region"),
            "{words}"
        );

        let error = pollster::block_on(plan_clone(&mut rockusb(), &mut dfu(), None))
            .expect_err("a DFU destination");
        let words = error.to_string();
        assert!(
            words.contains("the destination cannot take part") && words.contains("named region"),
            "{words}"
        );
    }

    /// A plan for the image the write tests use: eight sectors at LBA 64.
    fn plan_for(image_bytes: u64) -> WritePlan {
        plan_at(64, image_bytes)
    }

    /// The same, at whatever LBA the test needs.
    ///
    /// A clone writes from LBA 0.
    ///
    /// The plan says the device has no partition table. The write path does not
    /// mind, because nothing in `write_verified` consults the table. The table
    /// serves the plan a person reads, and the tests of that plan build it from a
    /// device rather than by hand.
    fn plan_at(lba: u64, image_bytes: u64) -> WritePlan {
        WritePlan {
            lba,
            image_bytes,
            padding_bytes: image_bytes.div_ceil(512) * 512 - image_bytes,
            sectors: image_bytes.div_ceil(512),
            flash: FlashInfo {
                size_bytes: FLASH_SECTORS * 512,
                sector_size: 512,
                medium: None,
                chip_id: None,
            },
            chip_version: CHIP_VERSION.to_vec(),
            soc: None,
            touches: Touches::NoTable,
            read_back: ReadBack::PerWindow,
        }
    }

    /// The write path, end to end and in order.
    ///
    /// The image goes out, and then the same range is read back and compared before
    /// anything else is written. The script pins the order. A write that verified
    /// later, or not at all, would put different bytes on the wire, and the
    /// transport would fail the test.
    #[test]
    fn a_write_reads_back_every_window_before_the_next_one_goes_out() {
        let image = vec![0x5a; 8 * 512];
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            [
                scripted_write(1, 64, image.clone()),
                scripted_read(2, 64, image.clone()),
            ]
            .concat(),
        )));

        let mut events = Vec::new();
        pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_for(image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("the flash returned what was written");

        assert_eq!(
            events,
            vec![
                Progress::Started { total_bytes: 4096 },
                Progress::Advanced {
                    done_bytes: 4096,
                    total_bytes: 4096,
                },
                Progress::Finished { done_bytes: 4096 },
            ]
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The device addresses sectors, and an image need not end on a sector boundary,
    /// so the final sector is padded with zeros.
    ///
    /// The read-back compares that sector in full, padding included. Core knows
    /// exactly what it put there, so it verifies everything it wrote.
    ///
    /// The scripted write asserts the padded bytes. This pins that the tail of the
    /// last sector is zeroed, not left holding the buffer's old contents.
    #[test]
    fn an_image_that_ends_mid_sector_is_padded_with_zeros_and_verified_in_full() {
        let image = vec![0xaa; 1000];

        // What must actually land: the image, then 24 zero bytes to fill out the
        // second sector.
        let mut on_the_wire = image.clone();
        on_the_wire.resize(1024, 0);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            [
                scripted_write(1, 64, on_the_wire.clone()),
                scripted_read(2, 64, on_the_wire),
            ]
            .concat(),
        )));

        let plan = plan_for(1000);
        assert_eq!(plan.padding_bytes, 24);

        let mut events = Vec::new();
        pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan,
            local(&mut SyncReader::new(image.as_slice())),
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("the flash returned what was written, padding and all");

        // Progress counts image bytes, so the padding is not in the total: it is
        // work the caller did not ask for and should not be billed for.
        assert_eq!(
            events.first(),
            Some(&Progress::Started { total_bytes: 1000 })
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished { done_bytes: 1000 })
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The read-back catches a wrong byte.
    ///
    /// The flash reads back holding one byte other than what was sent, and that is a
    /// `VerifyMismatch`. It names the byte, as an offset from the start of the
    /// write. It is not a protocol error, because the exchange with the device
    /// worked and its answer is the result.
    ///
    /// The write stops there. The script has nothing after the failing window,
    /// so a write that carried on would panic the transport.
    #[test]
    fn a_read_back_that_disagrees_with_the_image_stops_the_write_and_names_the_byte() {
        let image = vec![0x5a; 8 * 512];

        let mut from_flash = image.clone();
        from_flash[777] = 0xff; // a bad block, or the wrong image, or a dying part

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            [
                scripted_write(1, 64, image.clone()),
                scripted_read(2, 64, from_flash),
            ]
            .concat(),
        )));

        let error = pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_for(image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the flash does not hold what was written to it");

        assert!(
            matches!(
                error,
                Error::VerifyMismatch {
                    offset: 777,
                    found: 0xff,
                    expected: 0x5a,
                }
            ),
            "{error:?}"
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The read-back holds across a window boundary.
    ///
    /// A 2 MiB image is two 1 MiB windows, and each is written and read back
    /// *before the next goes out*. The script is therefore window 1's writes, then
    /// its reads, then window 2's writes, then its reads. A write that read back
    /// late, or verified one window against another's bytes, would send the
    /// commands in a different order. The transport would then fail the test. Only
    /// a second window shows this, so the single-window test cannot.
    #[test]
    fn a_write_across_two_windows_reads_each_back_before_the_next_goes_out() {
        let fill = 0xcc;
        let window = 1usize << 20; // WINDOW_BYTES
        let image = vec![fill; 2 * window];

        // Window 1 at LBA 64 (tags 1..=128), window 2 at LBA 64 + 2048 (tags
        // 129..=256): the second window follows the first by exactly one window of
        // sectors.
        let mut steps = scripted_write_window(1, 64, fill);
        steps.extend(scripted_write_window(129, 64 + 2048, fill));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let mut events = Vec::new();
        pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_at(64, image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("both windows landed and read back");

        let total = 2 * window as u64;
        assert_eq!(
            events.first(),
            Some(&Progress::Started { total_bytes: total })
        );
        assert!(
            events.contains(&Progress::Advanced {
                done_bytes: window as u64,
                total_bytes: total,
            }),
            "the first window's completion is reported before the second's: {events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished { done_bytes: total })
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A mismatch in the *second* window names an absolute offset, not one
    /// relative to the window.
    ///
    /// The first window lands clean. The second window's read-back disagrees five
    /// bytes in, and the reported offset is `1 MiB + 5`, not `5`. This pins the
    /// `done_bytes + offset` arithmetic, which a single-window test cannot, because
    /// `done_bytes` is still zero there.
    #[test]
    fn a_mismatch_in_the_second_window_names_the_absolute_offset() {
        let fill = 0xcc;
        let window = 1usize << 20;
        let image = vec![fill; 2 * window];

        // Window 1 clean.
        let mut steps = scripted_write_window(1, 64, fill);

        // Window 2: the writes all land, but the read-back of its first chunk comes
        // back with byte 5 flipped. The whole window is read (all 64 chunks) into
        // one buffer before the compare, so every read is still consumed.
        let w2_lba = 64 + 2048;
        let chunk = || vec![fill; 32 * 512];
        let at = |i: u32| w2_lba + u64::from(i) * 32;
        steps.extend((0..64u32).flat_map(|i| scripted_write(129 + i, at(i), chunk())));
        let mut bad = chunk();
        bad[5] = 0xff;
        steps.extend(scripted_read(193, at(0), bad));
        steps.extend((1..64u32).flat_map(|i| scripted_read(193 + i, at(i), chunk())));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let error = pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_at(64, image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the second window did not read back what was written");

        assert!(
            matches!(
                error,
                Error::VerifyMismatch {
                    offset,
                    found: 0xff,
                    expected: 0xcc,
                } if offset == window as u64 + 5
            ),
            "the offset must be absolute (1 MiB + 5), not window-relative: {error:?}"
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// One 1 MiB window of a write: the 64 commands the agent splits it into,
    /// and then the 64 reads that check them.
    fn scripted_write_window(first_tag: u32, first_lba: u64, fill: u8) -> Vec<Step> {
        let chunk = || vec![fill; 32 * 512];
        let at = |i: u32| first_lba + u64::from(i) * 32;
        let writes = (0..64u32).flat_map(|i| scripted_write(first_tag + i, at(i), chunk()));
        let reads = (0..64u32).flat_map(|i| scripted_read(first_tag + 64 + i, at(i), chunk()));
        writes.chain(reads).collect()
    }

    /// A canceled write stops at a window boundary, between commands, never partway
    /// through one.
    ///
    /// The script holds a single window, so a write that ignored the token would run
    /// past the end of the script and panic.
    ///
    /// What was written stays written, because cancellation is not an undo. The
    /// flash is partly written, and the progress the caller saw says how far the
    /// write got.
    #[test]
    fn a_canceled_write_stops_at_the_next_window_boundary() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_write_window(1, 64, 0xcc),
        )));

        let cancel = Cancel::new();
        let image = vec![0xcc; 4 * 1024 * 1024]; // four windows asked for
        let mut done = 0u64;

        let error = pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_for(image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |event| {
                if let Progress::Advanced { done_bytes, .. } = event {
                    done = done_bytes;
                    cancel.cancel();
                }
            },
            &cancel,
        ))
        .expect_err("the write was canceled");

        assert!(matches!(error, Error::Canceled), "{error:?}");
        assert_eq!(
            done,
            1024 * 1024,
            "the window that landed is the window that counted"
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// An image shorter than the plan was made against means the caller's file is
    /// wrong, not that the flash disagrees with it.
    ///
    /// A `VerifyMismatch` would blame the board for the host's arithmetic. On a
    /// write, that is the difference between "your image is truncated" and "your
    /// board is failing".
    #[test]
    fn a_write_whose_image_runs_out_is_an_io_error_not_a_mismatch() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(Vec::new())));

        let image = vec![0xaa; 100];
        let error = pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_for(4096), // the plan says 4096 bytes; the image has 100
            local(&mut SyncReader::new(image.as_slice())),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the image ran out");

        assert!(matches!(error, Error::Io(_)), "{error:?}");
    }

    /// The image is read before anything is written.
    ///
    /// An image that is too short is therefore caught before the first command goes
    /// out, not after the flash has been half overwritten. The empty script proves
    /// it, because a write that reached the transport would panic.
    #[test]
    fn an_image_that_runs_out_is_caught_before_the_flash_is_touched() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(Vec::new())));

        let image: Vec<u8> = Vec::new();
        pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_for(512),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("there is no image at all");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A clone plan reports both ends, because a person can get them backwards.
    ///
    /// A clone with the source and destination swapped destroys the board that was
    /// meant to be copied. The source is read, and the destination is planned
    /// exactly as any other write is.
    #[test]
    fn a_clone_plan_reports_the_source_it_reads_and_the_write_it_would_make() {
        // A small source: eight sectors, which is 4 KiB.
        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_info_of(1, 8),
        )));
        let mut dst =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let plan = pollster::block_on(plan_clone(&mut src, &mut dst, None)).expect("a plan");

        assert_eq!(plan.source.size_bytes, 8 * 512);
        // The whole source, from the first sector of the destination.
        assert_eq!(plan.destination.lba, 0);
        assert_eq!(plan.destination.image_bytes, 8 * 512);
        assert_eq!(plan.destination.sectors, 8);
        assert_eq!(plan.destination.padding_bytes, 0);
        // And it is the destination's loader that answered, because the
        // destination is the board that would be written to.
        assert_eq!(plan.destination.chip_version, CHIP_VERSION);

        let FlashAgent::Rockusb(src) = &src else {
            unreachable!("the test built a Rockusb agent")
        };
        let FlashAgent::Rockusb(dst) = &dst else {
            unreachable!("the test built a Rockusb agent")
        };
        src.transport().assert_drained();
        dst.transport().assert_drained();
    }

    /// A source that does not fit in the destination is refused whole, never
    /// truncated.
    ///
    /// A caller who asked to copy a part into one too small to hold it has an intent
    /// that is not known. Half a clone is not a bootable board.
    #[test]
    fn a_clone_into_a_part_too_small_to_hold_the_source_is_refused() {
        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_info_of(1, FLASH_SECTORS as u32 + 1),
        )));
        let mut dst =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let error = pollster::block_on(plan_clone(&mut src, &mut dst, None))
            .expect_err("one sector more than the destination holds");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The clone itself.
    ///
    /// The source's flash is read a window at a time and written to the destination.
    /// The *destination* is then read back and compared. The source is what is being
    /// copied, so there is nothing to check it against.
    ///
    /// The test uses two scripted transports, so it also pins that the two devices
    /// are driven independently. The second transport parameter exists for that,
    /// and a board on one bus cloned to a board on another needs it.
    #[test]
    fn a_clone_streams_the_source_into_the_destination_and_reads_the_destination_back() {
        let content = vec![0x77; 8 * 512];

        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_read(1, 0, content.clone()),
        )));
        let mut dst = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            [
                scripted_write(1, 0, content.clone()),
                scripted_read(2, 0, content.clone()),
            ]
            .concat(),
        )));

        let mut events = Vec::new();
        pollster::block_on(write_verified_from_plan(
            &mut dst,
            &plan_at(0, content.len() as u64),
            ImageSource::device(&mut src, 0),
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect("the destination holds what the source held");

        assert_eq!(
            events,
            vec![
                Progress::Started { total_bytes: 4096 },
                Progress::Advanced {
                    done_bytes: 4096,
                    total_bytes: 4096,
                },
                Progress::Finished { done_bytes: 4096 },
            ]
        );

        let FlashAgent::Rockusb(src) = &src else {
            unreachable!("the test built a Rockusb agent")
        };
        let FlashAgent::Rockusb(dst) = &dst else {
            unreachable!("the test built a Rockusb agent")
        };
        src.transport().assert_drained();
        dst.transport().assert_drained();
    }

    /// A clone that does not land is a `VerifyMismatch` against the destination,
    /// exactly as a `flash` would be.
    ///
    /// There is one write path, so the read-back is the same one.
    #[test]
    fn a_clone_whose_destination_does_not_hold_what_was_sent_is_a_mismatch() {
        let content = vec![0x77; 8 * 512];

        let mut landed = content.clone();
        landed[9] = 0x00; // the destination disagrees

        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_read(1, 0, content.clone()),
        )));
        let mut dst = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            [
                scripted_write(1, 0, content.clone()),
                scripted_read(2, 0, landed),
            ]
            .concat(),
        )));

        let error = pollster::block_on(write_verified_from_plan(
            &mut dst,
            &plan_at(0, content.len() as u64),
            ImageSource::device(&mut src, 0),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the destination does not hold what was written to it");

        assert!(
            matches!(
                error,
                Error::VerifyMismatch {
                    offset: 9,
                    found: 0x00,
                    expected: 0x77,
                }
            ),
            "{error:?}"
        );
    }

    /// A clone scans its source for constant fill.
    ///
    /// A clone reads its source over a transport, so it can hit the same silent
    /// read failure a [`dump`] can. A clone that did writes the fill byte onto the
    /// destination. The source is therefore scanned, and the [`FillReport`] is
    /// returned.
    ///
    /// The source reads back 2 MiB of constant `0xcc`. That is past the 1 MiB
    /// threshold and not a blank value, so the run is reported as suspicious. The
    /// run is keyed to the **source's** LBA. The destination is written at LBA 64
    /// and the source is read from LBA 0. A run reported at 0 is therefore keyed to
    /// the board that was read, which a person would check another way. The clone
    /// itself succeeds, because the fill is a caution, not a failure.
    #[test]
    fn a_clone_across_constant_fill_on_the_source_reports_it_keyed_to_the_source() {
        // 2 MiB of 0xcc off the source: two 1 MiB windows, 4096 sectors, tags
        // 1..=128 at LBAs 0, 32, ... on the source's own transport.
        let mut src = FlashAgent::Rockusb(RockusbAgent::new(scripted_constant_dump(4096, 0xcc)));

        // The destination takes that fill at LBA 64: two windows written and read
        // back, since the source is whole sectors of one byte there is no padding.
        let mut steps = scripted_write_window(1, 64, 0xcc);
        steps.extend(scripted_write_window(129, 64 + 2048, 0xcc));
        let mut dst = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let report = pollster::block_on(write_verified_from_plan(
            &mut dst,
            &plan_at(64, 2 * 1024 * 1024),
            ImageSource::device(&mut src, 0),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the clone lands -- the fill is a finding, not a failure");

        assert!(report.has_suspicious());
        let run = *report.suspicious().next().expect("one suspicious run");
        assert_eq!(run.byte(), 0xcc);
        assert_eq!(run.sectors(), 4096);
        assert_eq!(
            run.first_lba(),
            0,
            "the run is keyed to the source's LBA (0), not the destination's (64)"
        );

        let FlashAgent::Rockusb(src) = &src else {
            unreachable!("the test built a Rockusb agent")
        };
        let FlashAgent::Rockusb(dst) = &dst else {
            unreachable!("the test built a Rockusb agent")
        };
        src.transport().assert_drained();
        dst.transport().assert_drained();
    }

    /// A local-image source crosses no transport, so a `flash` cannot fail this way
    /// and its source is not scanned.
    ///
    /// The report a local source returns is empty, even across a region that a
    /// device read would have flagged. The bytes here are 2 MiB of `0xcc`, which is
    /// suspicious from a device and ordinary from a file.
    #[test]
    fn a_local_source_is_not_scanned_for_fill() {
        let image = vec![0xcc; 2 * 1024 * 1024];

        let mut steps = scripted_write_window(1, 64, 0xcc);
        steps.extend(scripted_write_window(129, 64 + 2048, 0xcc));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let report = pollster::block_on(write_verified_from_plan(
            &mut agent,
            &plan_at(64, image.len() as u64),
            local(&mut SyncReader::new(image.as_slice())),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the write lands");

        assert!(
            report.is_empty(),
            "a file is not a device; there is no read failure to find in it"
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A clone is a write, so it is gated like one, on the *destination's* loader.
    ///
    /// The destination is the board that gets overwritten. A clone that named no SoC
    /// is refused where `flash` is, and for the same reason. It is refused having
    /// read nothing and written nothing. Both scripts hold only the plan's commands.
    /// A clone that reached either device would run a transport past the end of its
    /// script and panic.
    #[test]
    fn a_clone_that_named_no_soc_refuses_having_touched_neither_board() {
        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_info_of(1, 8),
        )));
        let mut dst =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let plan = pollster::block_on(plan_clone(&mut src, &mut dst, None)).expect("a plan");

        let error = pollster::block_on(clone(
            &mut src,
            &mut dst,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("no SoC was named for the destination, so the gate has nothing to compare");

        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let FlashAgent::Rockusb(src) = &src else {
            unreachable!("the test built a Rockusb agent")
        };
        let FlashAgent::Rockusb(dst) = &dst else {
            unreachable!("the test built a Rockusb agent")
        };
        src.transport().assert_drained();
        dst.transport().assert_drained();
    }

    /// The plan names the partitions a write lands in.
    ///
    /// A write to sector 16384 is a fact about arithmetic. "The whole of `uboot`"
    /// is a fact about the board. The plan carries the second, because a person
    /// reads the plan before confirming, and can recognize the second as wrong.
    #[test]
    fn a_plan_says_which_partitions_the_write_would_destroy() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        // 4 MiB at sector 16384: the whole of `uboot`, and nothing else.
        let plan =
            pollster::block_on(plan_write(&mut agent, 16384, 8192 * 512, None)).expect("a plan");

        let Touches::Partitions(overlaps) = &plan.touches else {
            panic!("the device has a table: {:?}", plan.touches);
        };
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].name, "uboot");
        assert!(overlaps[0].is_whole());

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The plan shows an image that runs past the end of the partition it was
    /// aimed at.
    ///
    /// The device does not object, because the sectors after `uboot` are valid
    /// sectors. Only the table says that they belong to `trust`.
    #[test]
    fn a_plan_says_when_a_write_would_run_out_of_one_partition_and_into_the_next() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        // A hundred sectors more than `uboot` holds.
        let plan =
            pollster::block_on(plan_write(&mut agent, 16384, 8292 * 512, None)).expect("a plan");

        let Touches::Partitions(overlaps) = &plan.touches else {
            panic!("the device has a table: {:?}", plan.touches);
        };
        assert_eq!(overlaps.len(), 2);
        assert_eq!(overlaps[0].name, "uboot");
        assert!(overlaps[0].is_whole());
        assert_eq!(overlaps[1].name, "trust");
        assert_eq!(overlaps[1].covered, 100);
        assert!(
            !overlaps[1].is_whole(),
            "the front of it, and not the whole"
        );
    }

    /// An image within a sector of `u64::MAX` would wrap the padded byte count.
    ///
    /// The padding is a figure the plan reports, and a wrapped one would be false. A
    /// file is never that large, so only a direct library caller can hit this. The
    /// check is the one `dump` makes, so the plan is no weaker than the dump.
    #[test]
    fn a_plan_whose_padded_byte_count_cannot_be_counted_is_refused() {
        // No device is asked: the check is arithmetic, ahead of any I/O.
        let table = Table::Absent;
        let flash = FlashInfo {
            size_bytes: FLASH_SECTORS * 512,
            sector_size: 512,
            medium: None,
            chip_id: None,
        };

        let error = plan(
            0,
            u64::MAX,
            flash,
            Vec::new(),
            None,
            &table,
            ReadBack::PerWindow,
            ROCKUSB_CEILING,
        )
        .expect_err("padding u64::MAX to a whole sector overflows");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// The 32-bit ceiling is rockusb's, not the plan's.
    ///
    /// A block device is addressed by a `u64` byte offset, so a disk larger than 2
    /// TiB is bounded only by its own geometry. A plan that borrowed rockusb's
    /// ceiling would refuse the write, with a reason about a protocol that is not in
    /// use.
    #[test]
    fn a_backend_that_addresses_past_two_tebibytes_is_planned_past_it() {
        let table = Table::Absent;
        // 4 TB of 512-byte sectors: past 2^32 of them.
        let flash_sectors = 8_000_000_000u64;
        let flash = FlashInfo {
            size_bytes: flash_sectors * 512,
            sector_size: 512,
            medium: None,
            chip_id: None,
        };
        let lba = 6_000_000_000u64;

        let plan_over_block = plan(
            lba,
            4096,
            flash.clone(),
            Vec::new(),
            None,
            &table,
            ReadBack::PerWindow,
            AddressCeiling {
                past_last: u64::MAX,
                why: "no sector address can name it",
            },
        )
        .expect("a block device addresses this sector, so the plan stands");
        assert_eq!(plan_over_block.lba, lba);

        let refused = plan(
            lba,
            4096,
            flash,
            Vec::new(),
            None,
            &table,
            ReadBack::PerWindow,
            ROCKUSB_CEILING,
        )
        .expect_err("rockusb cannot name this sector");
        let Error::InvalidRequest(why) = &refused else {
            panic!("{refused:?}");
        };
        assert!(why.contains("32-bit LBA"), "{why}");
    }

    /// The DFU agent packs an alt-setting index into an LBA's high bits, so every
    /// partition past the first is addressed above 2^40.
    ///
    /// Under rockusb's ceiling, none of them could be planned. Under DFU's own, they
    /// can.
    #[test]
    fn a_dfu_alt_setting_past_the_first_is_planned() {
        let table = Table::Absent;
        let lba = crate::codec::dfu_alt::alt_base_lba(3);
        let flash = FlashInfo {
            size_bytes: u64::MAX / 2,
            sector_size: 512,
            medium: None,
            chip_id: None,
        };

        let planned = plan(
            lba,
            4096,
            flash,
            Vec::new(),
            None,
            &table,
            ReadBack::AfterCommit,
            AddressCeiling {
                past_last: crate::codec::dfu_alt::PAST_LAST_LBA,
                why: "no DFU alt-setting's slice of the packed address space reaches it",
            },
        )
        .expect("alt-setting 3 is inside the packed address space");
        assert_eq!(planned.lba, lba);
    }

    /// A device with no table is planned for against its geometry alone.
    ///
    /// The plan says the table is missing, rather than showing an empty list of
    /// partitions. An empty list would read as "this write touches nothing", on a
    /// board where the write can touch everything.
    #[test]
    fn a_plan_on_a_device_with_no_table_says_there_is_no_table() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let plan = pollster::block_on(plan_write(&mut agent, 64, 4096, None)).expect("a plan");
        assert_eq!(plan.touches, Touches::NoTable);
        assert_eq!(plan.lba, 64, "and the write is planned all the same");
    }

    /// A damaged table does not refuse the write.
    ///
    /// Writing a fresh table is how a damaged one is repaired. A refusal here would
    /// leave the tool able to diagnose the damage and unable to repair it. The plan
    /// says the map is missing instead, and the person decides.
    #[test]
    fn a_plan_on_a_device_whose_table_is_damaged_says_so_and_plans_the_write_anyway() {
        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));

        // A GPT whose signature is intact and whose CRC is not: damage, and not
        // absence.
        let (mut header, _) = crate::testing::gpt_table(&[gpt_entry(16384, 24575, "uboot")]);
        header[0x28] ^= 0xff;
        steps.extend(scripted_read(4, crate::codec::gpt::HEADER_LBA, header));
        // The damaged primary sends the read to the backup in the last sector,
        // which here is blank -- so the backup does not save it and the table
        // stays unreadable, which is what this test is about.
        steps.extend(scripted_read(5, FLASH_SECTORS - 1, vec![0u8; 512]));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let plan = pollster::block_on(plan_write(&mut agent, 0, 4096, None))
            .expect("a broken table does not stop a write; it is how one gets fixed");

        let Touches::UnreadableTable { format, detail } = &plan.touches else {
            panic!("the table is damaged: {:?}", plan.touches);
        };
        assert_eq!(*format, "GPT");
        assert!(detail.contains("CRC"), "{detail}");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A write aimed by name takes its range from the device's own table, so there
    /// is no LBA for anybody to get wrong.
    #[test]
    fn a_write_into_a_named_partition_takes_its_range_from_the_device() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        let plan = pollster::block_on(plan_write_partition(&mut agent, "trust", 4096, None))
            .expect("the device has a `trust`");

        assert_eq!(plan.lba, 24576, "where the device says `trust` begins");
        assert_eq!(plan.sectors, 8);

        let Touches::Partitions(overlaps) = &plan.touches else {
            panic!("the device has a table");
        };
        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].name, "trust");
        assert!(!overlaps[0].is_whole(), "8 sectors of its 8192");
    }

    /// Naming a partition lets the plan refuse an image too big for it.
    ///
    /// Such an image would run past the end of the partition and into whatever
    /// follows it. A raw LBA cannot catch that, because it does not say where the
    /// partition ends. The image is refused, not truncated. An image that does not
    /// fit is the wrong image or the wrong partition. Writing as much as fits would
    /// leave a board broken in a way that is harder to find.
    #[test]
    fn an_image_too_big_for_the_partition_it_names_is_refused() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        // `uboot` is 8192 sectors. This is one sector more.
        let error = pollster::block_on(plan_write_partition(&mut agent, "uboot", 8193 * 512, None))
            .expect_err("it does not fit in `uboot`");

        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        assert!(error.to_string().contains("uboot"), "{error}");

        // And exactly filling it is not too big: the refusal is a bound, not a
        // blanket.
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));
        pollster::block_on(plan_write_partition(&mut agent, "uboot", 8192 * 512, None))
            .expect("an image that exactly fills the partition fits it");
    }

    /// A name that matches no partition is refused, and the error lists the names
    /// the device does have.
    ///
    /// A person can then correct the name without searching for it.
    #[test]
    fn a_name_that_names_no_partition_is_refused_and_the_real_names_are_offered() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        let error = pollster::block_on(plan_write_partition(&mut agent, "rootfs", 4096, None))
            .expect_err("this board has no `rootfs`");

        let Error::NoSuchPartition { wanted, available } = &error else {
            panic!("a name that names nothing: {error:?}");
        };
        assert_eq!(wanted, "rootfs");
        assert_eq!(available, &["uboot", "trust", "boot"]);
    }

    /// Naming a partition on a device that has no table asks for a place that cannot
    /// be found.
    ///
    /// Unlike a plan against a raw LBA, which goes ahead, this plan is refused.
    #[test]
    fn naming_a_partition_on_a_device_with_no_table_is_refused() {
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));

        let error = pollster::block_on(plan_write_partition(&mut agent, "boot", 4096, None))
            .expect_err("there is no table to find a `boot` in");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// `partitions` reports the table, and reports a missing table as an absence,
    /// not as a failure.
    ///
    /// A board holding a raw image has no table and is not broken.
    #[test]
    fn partitions_reports_the_table_and_reports_no_table_as_no_table() {
        let mut steps = scripted_info(1);
        steps.extend(scripted_gpt(3, &a_boards_partitions()));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let table = pollster::block_on(partitions(&mut agent))
            .expect("nothing failed")
            .expect("the device has a table");
        assert_eq!(table.names(), ["uboot", "trust", "boot"]);

        let mut steps = scripted_info(1);
        steps.extend(scripted_no_table(3));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        assert!(
            pollster::block_on(partitions(&mut agent))
                .expect("nothing failed")
                .is_none()
        );
    }

    /// A clone overwrites the whole of the destination, so its plan names every
    /// partition on the board, whole.
    ///
    /// The largest possible write gets the same account as any other.
    #[test]
    fn a_clone_plan_says_it_would_destroy_every_partition_on_the_destination() {
        let mut src = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_info_of(1, FLASH_SECTORS as u32),
        )));
        let mut dst = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(
            scripted_plan_with_gpt(1, &a_boards_partitions()),
        )));

        let plan = pollster::block_on(plan_clone(&mut src, &mut dst, None)).expect("a plan");

        let Touches::Partitions(overlaps) = &plan.destination.touches else {
            panic!("the destination has a table");
        };
        assert_eq!(overlaps.len(), 3);
        assert!(
            overlaps.iter().all(Overlap::is_whole),
            "all of all of them: {overlaps:?}"
        );
    }

    /// `total_bytes` is what `Progress::Started` announces, and what a caller
    /// renders a percentage against.
    ///
    /// A sector count whose byte count does not fit in a `u64` is refused. Wrapping
    /// it would make every later progress event false.
    #[test]
    fn a_dump_whose_byte_count_cannot_be_counted_is_refused_before_any_io() {
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(vec![])));
        let mut events = Vec::new();

        let error = pollster::block_on(dump(
            &mut agent,
            0,
            u64::MAX, // at 512 bytes a sector, this wraps
            &mut SyncWriter::new(Vec::new()),
            &mut |event| events.push(event),
            &Cancel::new(),
        ))
        .expect_err("more bytes than a byte count holds");

        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        assert!(
            events.is_empty(),
            "no total is announced when the total is the thing that is wrong"
        );
    }

    /// A board with a damaged primary and an intact backup, and the reads a repair
    /// plan makes to establish that.
    ///
    /// The reads are the geometry, the loader, the damaged primary, and then the
    /// backup. It returns the steps and the backup bytes a repair rebuilds from. A
    /// test computes from them the exact primary the write is expected to lay down.
    fn scripted_repairable(entries: &[Vec<u8>]) -> (Vec<Step>, Vec<u8>, Vec<u8>, u64) {
        let (mut primary, _) = crate::testing::gpt_table(entries);
        primary[0x28] ^= 0xff; // the primary's own header CRC no longer holds

        let backup_lba = FLASH_SECTORS - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(entries, backup_lba, backup_array_lba);
        let backup_array_sector = crate::testing::sector(&backup_array);

        let mut steps = scripted_info(1); // 1, 2
        steps.extend(scripted_chip_version(3)); // 3
        steps.extend(scripted_read(4, crate::codec::gpt::HEADER_LBA, primary));
        steps.extend(scripted_read(5, backup_lba, backup_header.clone()));
        steps.extend(scripted_read(
            6,
            backup_array_lba,
            backup_array_sector.clone(),
        ));

        (steps, backup_header, backup_array_sector, backup_lba)
    }

    /// The plan reads the device and reports the repair it would make.
    ///
    /// It names the copy it rewrites, the copy it rebuilds from, and the partitions
    /// it restores. The partitions are the backup's, and a person recognizes the
    /// table by them.
    #[test]
    fn a_repair_plan_reports_the_copy_it_would_rewrite_and_what_it_restores() {
        let entries = a_boards_partitions();
        let (steps, ..) = scripted_repairable(&entries);
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_repair_table(&mut agent, Some(soc))).expect("a plan");

        assert_eq!(plan.format, TableFormat::Gpt);
        assert!(matches!(plan.action, TableAction::Repair { .. }));
        assert_eq!(
            plan.partitions
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["uboot", "trust", "boot"]
        );
        assert_eq!(plan.segments.len(), 1, "a GPT repair is one segment");
        assert_eq!(plan.segments[0].lba, crate::codec::gpt::HEADER_LBA);
        // The segment is whole sectors -- a header and its array.
        assert_eq!(plan.segments[0].bytes.len() % 512, 0);
        // And the gate has what it needs: the loader answered as the SoC named.
        assert!(segmented_refusal(&agent, &plan).is_none());

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The repair, end to end: plan, confirm, and the rebuilt primary is written at
    /// sector 1 and read back before the call returns.
    ///
    /// The scripted write asserts the exact bytes. This pins that a repair puts the
    /// primary the codec rebuilt on the flash, and reads it back as every write
    /// does.
    #[test]
    fn a_repair_writes_the_rebuilt_primary_and_reads_it_back() {
        let entries = a_boards_partitions();
        let (mut steps, backup_header, backup_array, backup_lba) = scripted_repairable(&entries);

        // The very bytes the codec will rebuild, so the scripted write and its
        // read-back assert them. The plan's tags run to 6; `repair_table` re-asks
        // the loader (7), then writes (8) and reads it back (9).
        let rebuilt = crate::codec::gpt::rebuild_primary_from_backup(
            &backup_header,
            &backup_array,
            backup_lba,
            512,
        )
        .expect("a well-formed backup")
        .bytes;

        steps.extend(scripted_chip_version(7));
        steps.extend(scripted_write(8, 1, rebuilt.clone()));
        steps.extend(scripted_read(9, 1, rebuilt.clone()));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_repair_table(&mut agent, Some(soc))).expect("a plan");
        assert_eq!(plan.segments[0].bytes.len(), rebuilt.len());

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the loader is the one the repair was planned for");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A device whose copies both pass validation and agree has nothing to repair.
    ///
    /// The plan says so, rather than rewriting a table that was already good. Both
    /// copies are read, the backup included, because a repair looks for a stale
    /// backup as well as a damaged primary.
    #[test]
    fn repairing_a_healthy_table_is_refused_with_nothing_to_do() {
        let entries = a_boards_partitions();
        let backup_lba = FLASH_SECTORS - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        steps.extend(scripted_gpt(4, &entries)); // a healthy primary: header, then array
        steps.extend(scripted_read(6, backup_lba, backup_header)); // and an agreeing backup
        steps.extend(scripted_read(
            7,
            backup_array_lba,
            crate::testing::sector(&backup_array),
        ));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let error = pollster::block_on(plan_repair_table(&mut agent, Some(soc)))
            .expect_err("both copies are intact and agree");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// The repair in the other direction, end to end.
    ///
    /// A healthy primary with an absent backup rewrites the *backup* from the
    /// primary. The write lands at the end of the disk, not at sector 1, and is
    /// read back like any other. The scripted write asserts the exact bytes, so
    /// this pins that a backup repair puts the codec's rebuilt backup where the
    /// backup goes.
    #[test]
    fn a_backup_repair_writes_the_rebuilt_backup_at_the_end_and_reads_it_back() {
        let entries = a_boards_partitions();
        let (primary_header, primary_array) = crate::testing::gpt_table(&entries);
        let backup_lba = FLASH_SECTORS - 1;
        let backup_array_lba = backup_lba - 1;

        let rebuilt = crate::codec::gpt::rebuild_backup_from_primary(
            &primary_header,
            &primary_array,
            FLASH_SECTORS,
            512,
        )
        .expect("a well-formed primary")
        .bytes;

        // Plan reads: info (1,2), loader (3), healthy primary (4,5), absent backup
        // (6). Then repair_table re-asks the loader (7) and writes (8) + reads back
        // (9) at the backup's own sector.
        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        steps.extend(scripted_gpt(4, &entries));
        steps.extend(scripted_read(6, backup_lba, vec![0u8; 512])); // backup absent
        steps.extend(scripted_chip_version(7));
        steps.extend(scripted_write(8, backup_array_lba, rebuilt.clone()));
        steps.extend(scripted_read(9, backup_array_lba, rebuilt.clone()));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_repair_table(&mut agent, Some(soc))).expect("a plan");
        assert_eq!(
            plan.segments[0].lba, backup_array_lba,
            "the backup lands at the end of the disk, not sector 1"
        );
        assert!(
            plan.segments[0].what.contains("backup"),
            "the plan names the backup as the copy rewritten: {}",
            plan.segments[0].what
        );

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the loader is the one the repair was planned for");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A repair is a write, so it is gated like one.
    ///
    /// A plan that named no SoC is refused by `write_table` with the same answer a
    /// front-end is shown, and nothing is written. The plan's reads drain the
    /// script, and the write's commands never begin.
    #[test]
    fn a_repair_that_named_no_soc_refuses_having_written_nothing() {
        let entries = a_boards_partitions();
        let (steps, ..) = scripted_repairable(&entries);
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let plan = pollster::block_on(plan_repair_table(&mut agent, None)).expect("a plan");
        let shown = write_refusal(&agent, plan.soc).expect("no SoC named is a refusal");

        let error = pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("a repair with no SoC named refuses like any other write");
        assert!(
            matches!(&error, Error::InvalidRequest(why) if why == &shown),
            "{error:?}"
        );

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A repair also refuses a loader that answers as a different SoC than the plan
    /// named.
    ///
    /// The wrong loader writes a GPT to the wrong offsets as readily as anything
    /// else. `reverify_loader_soc` asks again before the first byte, so nothing is
    /// written.
    #[test]
    fn a_repair_refuses_a_loader_that_answers_as_a_different_soc() {
        let entries = a_boards_partitions();
        let (mut steps, ..) = scripted_repairable(&entries);
        // The re-ask before the write answers as an RK3588, not the RK3576 the
        // plan named. It is command 7, and the write never reaches command 8.
        let rk3588 = [0x38, 0x38, 0x35, 0x33, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        steps.extend(scripted_chip_version_of(7, &rk3588));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let soc = Soc::parse("rk3576").expect("pinned");
        let plan = pollster::block_on(plan_repair_table(&mut agent, Some(soc))).expect("a plan");

        let error = pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("the loader is not the one the repair was planned for");
        assert!(matches!(error, Error::LoaderMismatch { .. }), "{error:?}");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    // ---- Multi-segment table writes: parameter repair and authoring ----

    /// The parameter block `plan_author_param` frames from a layout, so a test can
    /// script the write and read-back of it.
    fn authored_param_block(layout: &Layout, base: u64) -> Vec<u8> {
        let parts: Vec<rkparam::Partition> = layout
            .partitions
            .iter()
            .map(|part| rkparam::Partition {
                name: part.name.clone(),
                first_lba: part.first_lba,
                sectors: part.sectors,
            })
            .collect();
        let mtdparts = rkparam::render_mtdparts(&parts, base).unwrap();
        rkparam::frame(&format!("CMDLINE: mtdparts={mtdparts}\n")).unwrap()
    }

    /// The reads a parameter scan makes across every copy location, in
    /// [`rkparam::LOCATIONS`] order.
    ///
    /// A location that `blocks` does not name reads as a blank sector. A location
    /// it names gets a probe and a full read.
    fn scripted_param_scan(first_tag: u32, blocks: &[(u64, Vec<u8>)]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut tag = first_tag;
        for location in rkparam::LOCATIONS {
            match blocks.iter().find(|(lba, _)| *lba == location.lba) {
                Some((_, block)) => {
                    steps.extend(scripted_read(tag, location.lba, sector(block)));
                    steps.extend(scripted_read(tag + 1, location.lba, sector(block)));
                    tag += 2;
                }
                None => {
                    steps.extend(scripted_read(tag, location.lba, vec![0u8; 512]));
                    tag += 1;
                }
            }
        }
        steps
    }

    /// Authoring a parameter on an eMMC.
    ///
    /// A layout becomes a block, planned and written to the one copy an eMMC keeps.
    /// Like every write, it is read back before the call returns.
    #[test]
    fn authoring_a_parameter_on_emmc_plans_and_writes_one_verified_copy() {
        let layout =
            Layout::parse_native("uboot 0x4000 0x2000\ntrust 0x6000 0x2000\n", FLASH_SECTORS)
                .unwrap();
        let block = authored_param_block(&layout, rkparam::EMMC_BASE_LBA);

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        // confirm -> write_table: reverify the loader, then write and read back the
        // one eMMC copy.
        steps.extend(scripted_chip_version(4));
        steps.extend(scripted_write(5, rkparam::EMMC_BASE_LBA, sector(&block)));
        steps.extend(scripted_read(6, rkparam::EMMC_BASE_LBA, sector(&block)));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let soc = Soc::parse("rk3576").expect("pinned");

        let plan = pollster::block_on(plan_author_param(
            &mut agent,
            ParamAuthorSource::Layout(&layout),
            ParamMedium::Emmc,
            Some(soc),
        ))
        .expect("a plan");

        assert_eq!(plan.format, TableFormat::RockchipParam);
        assert!(matches!(plan.action, TableAction::Author));
        assert_eq!(plan.segments.len(), 1, "an eMMC keeps one copy");
        assert_eq!(plan.segments[0].lba, rkparam::EMMC_BASE_LBA);
        assert_eq!(plan.partitions.len(), 2);
        assert!(segmented_refusal(&agent, &plan).is_none());

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the copy was written and read back");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// Authoring a parameter on raw NAND writes every copy.
    ///
    /// The format relies on its redundancy, so authoring lays the block down eight
    /// times, and verifies each copy.
    #[test]
    fn authoring_a_parameter_on_nand_writes_every_copy() {
        let layout = Layout::parse_native("boot 0x2000 0x100\n", FLASH_SECTORS).unwrap();
        let block = authored_param_block(&layout, 0);

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        steps.extend(scripted_chip_version(4)); // reverify
        let mut tag = 5;
        for location in &rkparam::LOCATIONS[1..] {
            steps.extend(scripted_write(tag, location.lba, sector(&block)));
            steps.extend(scripted_read(tag + 1, location.lba, sector(&block)));
            tag += 2;
        }

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let soc = Soc::parse("rk3576").expect("pinned");

        let plan = pollster::block_on(plan_author_param(
            &mut agent,
            ParamAuthorSource::Layout(&layout),
            ParamMedium::Nand,
            Some(soc),
        ))
        .expect("a plan");
        assert_eq!(
            plan.segments.len(),
            8,
            "rkflashtool writes the block eight times"
        );

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("every copy written and read back");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// Authoring a parameter from text keeps its other keys.
    ///
    /// A block built from an existing parameter's whole text carries
    /// `FIRMWARE_VER` and the other keys across verbatim. Authoring from a layout
    /// builds a minimal block instead. The partitions are still read out and shown,
    /// at the medium's base.
    #[test]
    fn authoring_a_parameter_from_text_preserves_its_keys() {
        let text = "FIRMWARE_VER: 8.1\n\
                    MACHINE_MODEL: RK3399\n\
                    CMDLINE: mtdparts=rk29xxnand:0x2000@0x2000(uboot)\n";
        let block = rkparam::frame(text).unwrap();

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        steps.extend(scripted_chip_version(4)); // reverify
        steps.extend(scripted_write(5, rkparam::EMMC_BASE_LBA, sector(&block)));
        steps.extend(scripted_read(6, rkparam::EMMC_BASE_LBA, sector(&block)));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let soc = Soc::parse("rk3576").expect("pinned");

        let plan = pollster::block_on(plan_author_param(
            &mut agent,
            ParamAuthorSource::Text(text),
            ParamMedium::Emmc,
            Some(soc),
        ))
        .expect("a plan");

        // The partitions are read out of the text, at the eMMC base: uboot's
        // 0x2000 offset resolves to 0x4000 absolute.
        assert_eq!(plan.partitions.len(), 1);
        assert_eq!(plan.partitions[0].name, "uboot");
        assert_eq!(plan.partitions[0].first_lba, 0x4000);
        // And the block carries the other keys, which a layout could not.
        assert!(
            plan.segments[0]
                .bytes
                .windows(b"FIRMWARE_VER".len())
                .any(|window| window == b"FIRMWARE_VER"),
            "the block kept the parameter's other keys"
        );

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the copy was written and read back");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// Repairing a parameter rewrites the damaged copy from the intact one.
    ///
    /// The board is NAND, with one good copy and one damaged copy. The good block
    /// is written verbatim to the damaged copy's sector, and read back.
    #[test]
    fn repairing_a_parameter_rewrites_the_damaged_copy_from_the_intact_one() {
        let good = param_block("CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n");
        let mut damaged = good.clone();
        damaged[rkparam::HEADER_LEN + 1] ^= 0xff;

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        // The scan: an intact copy at 0, a damaged one at 0x400.
        steps.extend(scripted_param_scan(
            4,
            &[(0x0000, good.clone()), (0x0400, damaged)],
        ));
        // confirm -> write_table: reverify, then rewrite 0x400 from the intact block.
        steps.extend(scripted_chip_version(15));
        steps.extend(scripted_write(16, 0x0400, good.clone()));
        steps.extend(scripted_read(17, 0x0400, good.clone()));

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let soc = Soc::parse("rk3576").expect("pinned");

        let plan = pollster::block_on(plan_repair_param(&mut agent, Some(soc))).expect("a plan");
        assert!(matches!(plan.action, TableAction::Repair { .. }));
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(plan.segments[0].lba, 0x0400);

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the damaged copy was rewritten and read back");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    /// A table write that named no SoC is refused by the gate before a confirmation
    /// is collected, exactly as a plain write is.
    ///
    /// The plan is made, but [`segmented_refusal`] says it will not go through, so
    /// nobody is asked to confirm a write that cannot happen.
    #[test]
    fn a_table_write_that_named_no_soc_is_refused_before_it_is_confirmed() {
        let layout = Layout::parse_native("boot 0x2000 0x100\n", FLASH_SECTORS).unwrap();

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));

        let plan = pollster::block_on(plan_author_param(
            &mut agent,
            ParamAuthorSource::Layout(&layout),
            ParamMedium::Nand,
            None,
        ))
        .expect("a plan is still produced");
        assert!(segmented_refusal(&agent, &plan).is_some());

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    // ---- GPT authoring ----

    /// Authoring a GPT is deterministic, and pinned GUIDs override it.
    ///
    /// The test pins four properties:
    ///
    /// - The same layout authors the same table twice, so an authored table
    ///   verifies against a re-author of its layout.
    /// - The type token picks the type GUID.
    /// - An unpinned partition gets a synthesized unique GUID, distinct from its
    ///   neighbor's.
    /// - A pinned disk GUID and unique GUID come through verbatim.
    #[test]
    fn authoring_a_gpt_is_deterministic_and_honors_overrides() {
        let layout = Layout::parse_native(
            "uboot 0x8000 0x2000 esp\nrootfs 0xa000 0x2000\n",
            FLASH_SECTORS,
        )
        .unwrap();

        let once = author_gpt(&layout, FLASH_SECTORS, 512).expect("a well-formed layout");
        let twice = author_gpt(&layout, FLASH_SECTORS, 512).unwrap();
        assert_eq!(once, twice, "the same layout authors the same table");

        let header = gpt::parse_header(&once.primary.bytes[512..1024]).unwrap();
        let entries = gpt::parse_entries(&header, &once.primary.bytes[1024..]).unwrap();
        assert_eq!(entries[0].type_guid, gpt::EFI_SYSTEM, "esp -> EFI system");
        assert_eq!(
            entries[1].type_guid,
            gpt::LINUX_DATA,
            "no type -> the default"
        );
        assert_ne!(
            entries[0].unique_guid, entries[1].unique_guid,
            "each partition gets its own synthesized GUID"
        );

        // Overrides come through untouched.
        let pinned = Layout::parse_native(
            "disk-guid 01234567-89AB-CDEF-0123-456789ABCDEF\n\
             uboot 0x8000 0x2000 uuid=0FC63DAF-8483-4772-8E79-3D69D8477DE4\n",
            FLASH_SECTORS,
        )
        .unwrap();
        let table = author_gpt(&pinned, FLASH_SECTORS, 512).unwrap();
        assert_eq!(
            table.disk_guid,
            gpt::Guid::parse("01234567-89AB-CDEF-0123-456789ABCDEF").unwrap()
        );
        let header = gpt::parse_header(&table.primary.bytes[512..1024]).unwrap();
        let entries = gpt::parse_entries(&header, &table.primary.bytes[1024..]).unwrap();
        assert_eq!(
            entries[0].unique_guid,
            gpt::Guid::parse("0FC63DAF-8483-4772-8E79-3D69D8477DE4").unwrap()
        );
    }

    /// Authoring a GPT plans and writes both copies, and reads each back.
    ///
    /// A layout becomes a fresh table: a protective MBR, a primary, and a backup.
    /// It is planned as two segments and written through the one table-write path,
    /// with every window verified. The segments span more than one 32-sector
    /// command. This also pins that a table write is split into commands and read
    /// back exactly as a boot-image write is.
    #[test]
    fn authoring_a_gpt_plans_and_writes_both_copies_verified() {
        let layout = Layout::parse_native(
            "uboot 0x8000 0x2000 linux\nrootfs 0xa000 0x40000 linux\n",
            FLASH_SECTORS,
        )
        .unwrap();

        // Reproduce the authoring the plan will do, to script its exact bytes.
        let authored = author_gpt(&layout, FLASH_SECTORS, 512).expect("a well-formed layout");

        let mut steps = scripted_info(1);
        steps.extend(scripted_chip_version(3));
        steps.extend(scripted_chip_version(4)); // reverify on the confirmed write
        let (writes, tag) =
            scripted_transfer(5, authored.primary.lba, &authored.primary.bytes, true);
        steps.extend(writes);
        let (reads, tag) =
            scripted_transfer(tag, authored.primary.lba, &authored.primary.bytes, false);
        steps.extend(reads);
        let (writes, tag) =
            scripted_transfer(tag, authored.backup.lba, &authored.backup.bytes, true);
        steps.extend(writes);
        let (reads, _tag) =
            scripted_transfer(tag, authored.backup.lba, &authored.backup.bytes, false);
        steps.extend(reads);

        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let soc = Soc::parse("rk3576").expect("pinned");

        let plan =
            pollster::block_on(plan_author_gpt(&mut agent, &layout, Some(soc))).expect("a plan");

        assert_eq!(plan.format, TableFormat::Gpt);
        assert!(matches!(plan.action, TableAction::Author));
        assert_eq!(plan.segments.len(), 2, "a primary and a backup");
        assert_eq!(
            plan.segments[0].lba, 0,
            "the primary carries the protective MBR from sector 0"
        );
        assert_eq!(plan.partitions.len(), 2);
        assert!(segmented_refusal(&agent, &plan).is_none());

        pollster::block_on(write_table(
            &mut agent,
            plan.confirm(),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("both copies written and read back");

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }

    // A firmware package small enough for a scratch file.
    fn a_small_package() -> Vec<u8> {
        crate::testing::firmware_package_for_a_small_disk()
    }

    /// The sectors of the scratch disk a firmware test writes to: 4 MiB.
    #[cfg(target_os = "linux")]
    const FIRMWARE_DISK_SECTORS: u64 = 0x2000;

    /// A whole package, planned and written onto a disk, lands where its parameter
    /// says: each image in its partition, a GPT the codec reads back with the
    /// pinned GUID and the growing partition ending at the last usable sector, and
    /// an ID block at sector 64 that checks against its own header.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_firmware_package_lands_where_its_parameter_says_and_reads_back() {
        let (path, mut agent) = a_block_agent("firmware", FIRMWARE_DISK_SECTORS);
        let bytes = a_small_package();
        let package = crate::firmware::tests::read_package(&bytes).expect("intact");

        let plan = pollster::block_on(plan_firmware(&mut agent, &package, None))
            .expect("a package that fits plans");
        assert!(
            firmware_refusal(&agent, &plan).is_none(),
            "a disk has no loader to refuse"
        );
        assert_eq!(plan.capability, LoaderCapability::NoLoader);
        let order: Vec<(u64, bool)> = plan
            .runs
            .iter()
            .map(|run| (run.lba, matches!(run.source, RunSource::Package { .. })))
            .collect();
        assert_eq!(
            order,
            [
                (0x800, true),
                (0xc00, true),
                (0, false),
                (FIRMWARE_DISK_SECTORS - 33, false),
                (64, false),
            ],
            "the images in file order, then both GPT copies, then the ID block last"
        );
        assert_eq!(plan.skipped.len(), 1, "the packing tool's list");

        let mut source = SyncReader::new(bytes.as_slice());
        pollster::block_on(write_firmware(
            &mut agent,
            plan.confirm(),
            Some(&mut source),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("the write lands and reads back");
        drop(agent);

        let disk = std::fs::read(&path).expect("the disk");
        let at = |lba: u64| (lba * 512) as usize;
        assert!(disk[at(0x800)..at(0x800) + 5000].iter().all(|&b| b == 0x55));
        assert!(
            disk[at(0x800) + 5000..at(0x800) + 5120]
                .iter()
                .all(|&b| b == 0),
            "padding"
        );
        assert!(disk[at(0xc00)..at(0xc00) + 3000].iter().all(|&b| b == 0x66));

        let id_block = &disk[at(64)..at(64) + package.id_block.bytes.len()];
        assert_eq!(id_block, package.id_block.bytes.as_slice());
        idb::check(id_block).expect("the ID block checks against its own header");

        let header = gpt::parse_header(&disk[at(1)..at(2)]).expect("a primary GPT");
        let entries = gpt::parse_entries(&header, &disk[at(2)..at(34)]).expect("its entry array");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["uboot", "boot", "rootfs"]);
        assert_eq!(entries[2].last_lba, FIRMWARE_DISK_SECTORS - 34);
        assert_eq!(
            entries[2].unique_guid,
            gpt::Guid::parse("614e0000-0000-4b53-8000-1d28000054a9").expect("a GUID")
        );

        let _ = std::fs::remove_file(&path);
    }

    /// An image larger than the partition its parameter names is refused at the
    /// plan, before anything is confirmed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_package_image_larger_than_its_partition_is_refused_at_the_plan() {
        let (path, mut agent) = a_block_agent("firmware-too-big", FIRMWARE_DISK_SECTORS);
        let mut package = crate::firmware::tests::read_package(&a_small_package()).expect("intact");
        package.images[0].bytes = 0x401 * 512;
        let error = pollster::block_on(plan_firmware(&mut agent, &package, None))
            .expect_err("too big for uboot");
        assert!(error.to_string().contains("'uboot'"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    /// An image entry that names no partition has nowhere to go, and is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_package_entry_for_no_partition_is_refused_at_the_plan() {
        let (path, mut agent) = a_block_agent("firmware-no-partition", FIRMWARE_DISK_SECTORS);
        let mut package = crate::firmware::tests::read_package(&a_small_package()).expect("intact");
        package.images[1].name = "recovery".to_string();
        let error = pollster::block_on(plan_firmware(&mut agent, &package, None))
            .expect_err("no recovery partition");
        assert!(error.to_string().contains("names no partition"), "{error}");
        let _ = std::fs::remove_file(&path);
    }

    /// A plan that streams images needs the package again at the write, and one
    /// given none is refused before anything is written.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_package_write_with_no_package_is_refused() {
        let (path, mut agent) = a_block_agent("firmware-no-source", FIRMWARE_DISK_SECTORS);
        let package = crate::firmware::tests::read_package(&a_small_package()).expect("intact");
        let plan = pollster::block_on(plan_firmware(&mut agent, &package, None)).expect("plan");
        let error = pollster::block_on(write_firmware(
            &mut agent,
            plan.confirm(),
            None,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("no package");
        assert!(error.to_string().contains("no package"), "{error}");
        let disk = std::fs::read(&path).expect("the disk");
        assert!(disk.iter().all(|&b| b == 0), "nothing was written");
        let _ = std::fs::remove_file(&path);
    }

    /// The ID block alone lands at sector 64 and leaves the rest of the disk as it
    /// was.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_id_block_alone_lands_at_sector_64() {
        let (path, mut agent) = a_block_agent("id-block", 256);
        let loader =
            crate::codec::rkboot::parse(&crate::firmware::tests::a_loader()).expect("a container");
        let plan = pollster::block_on(plan_write_id_block(&mut agent, &loader, None))
            .expect("an ID block plans");
        assert_eq!(plan.what, FirmwareWrite::IdBlock);
        assert_eq!(plan.runs.len(), 1);
        assert_eq!(plan.runs[0].lba, 64);
        assert!(!plan.needs_package());

        pollster::block_on(write_firmware(
            &mut agent,
            plan.confirm(),
            None,
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect("written and read back");
        drop(agent);

        let disk = std::fs::read(&path).expect("the disk");
        let built = idb::build(&loader).expect("builds");
        assert_eq!(
            &disk[64 * 512..64 * 512 + built.bytes.len()],
            built.bytes.as_slice()
        );
        assert!(
            disk[..64 * 512].iter().all(|&b| b == 0),
            "nothing before it"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The ID block plan asks a rockusb loader for its capability reply. A reply
    /// without `NEW_IDB`, and a failed query, are both refused, with the reason;
    /// a reply that sets it passes. The refusal is asked of the plan and is the
    /// one the write makes.
    #[test]
    fn a_rockusb_id_block_needs_a_loader_that_claims_new_idb() {
        use crate::codec::bot::Direction;
        use crate::codec::rockusb::Opcode;
        use crate::testing::{cbw, csw_failed, csw_passed};

        let soc = Soc::parse("rk3576").expect("pinned");
        let loader =
            crate::codec::rkboot::parse(&crate::firmware::tests::a_loader()).expect("a container");
        let plan_with = |reply: [u8; 8], failed: bool| {
            // The plan's reads take tags 1 to 13, so the capability query is 14.
            let mut steps = scripted_plan(1);
            steps.push(cbw(14, 8, Direction::In, Opcode::ReadCapability, 0, 0));
            steps.push(Step::Reply(reply.to_vec()));
            steps.push(if failed {
                csw_failed(14)
            } else {
                csw_passed(14)
            });
            let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
            let plan = pollster::block_on(plan_write_id_block(&mut agent, &loader, Some(soc)))
                .expect("a plan");
            (agent, plan)
        };

        let (agent, plan) = plan_with([0, 0x01, 0, 0, 0, 0, 0, 0], false);
        assert!(firmware_refusal(&agent, &plan).is_none(), "NEW_IDB is set");

        let (agent, plan) = plan_with([0xff, 0x02, 0, 0, 0, 0, 0, 0], false);
        let refused = firmware_refusal(&agent, &plan).expect("NEW_IDB is clear");
        assert!(refused.to_string().contains("NEW_IDB"), "{refused}");

        let (agent, plan) = plan_with([0; 8], true);
        assert!(matches!(plan.capability, LoaderCapability::NotAnswered(_)));
        let refused = firmware_refusal(&agent, &plan).expect("no answer");
        assert!(refused.to_string().contains("did not answer"), "{refused}");
    }

    /// The loader container's own claim is judged as an upload judges it: a
    /// container built for another SoC is refused before its ID block is written.
    #[test]
    fn an_id_block_from_another_socs_container_is_refused() {
        use crate::codec::bot::Direction;
        use crate::codec::rockusb::Opcode;
        use crate::testing::{cbw, csw_passed};

        let soc = Soc::parse("rk3576").expect("pinned");
        let mut loader =
            crate::codec::rkboot::parse(&crate::firmware::tests::a_loader()).expect("a container");
        loader.chip = Some(*b"8853");
        let mut steps = scripted_plan(1);
        steps.push(cbw(14, 8, Direction::In, Opcode::ReadCapability, 0, 0));
        steps.push(Step::Reply(vec![0, 0x01, 0, 0, 0, 0, 0, 0]));
        steps.push(csw_passed(14));
        let mut agent = FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)));
        let plan = pollster::block_on(plan_write_id_block(&mut agent, &loader, Some(soc)))
            .expect("a plan");
        let refused = firmware_refusal(&agent, &plan).expect("the wrong container");
        assert!(
            matches!(refused, Error::LoaderBlobMismatch { .. }),
            "{refused:?}"
        );
    }

    /// A container a tool unpacks is refused by its first bytes, and an ID block
    /// is not.
    #[test]
    fn image_refusal_names_the_containers_a_raw_write_refuses() {
        assert!(image_refusal(b"RKFW").is_some());
        assert!(image_refusal(b"RKAF").is_some());
        assert!(image_refusal(b"LDR ").is_some());
        assert!(image_refusal(b"BOOT").is_some());
        assert!(
            image_refusal(b"RKNS").is_none(),
            "an ID block is written raw"
        );
        assert!(image_refusal(&[0u8; 4]).is_none());
        assert!(
            image_refusal(b"RK").is_none(),
            "too short to begin anything"
        );
    }

    /// `flash` refuses a firmware package written raw, from its first bytes,
    /// before a command reaches the device. The empty script after the plan proves
    /// it: a write, or even the loader re-check, would find nothing scripted.
    #[test]
    fn flash_refuses_a_package_written_raw_before_touching_the_device() {
        let soc = Soc::parse("rk3576").expect("pinned");
        let bytes = a_small_package();
        let mut agent =
            FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(scripted_plan(1))));
        let plan = pollster::block_on(plan_write(&mut agent, 0x800, bytes.len() as u64, Some(soc)))
            .expect("a plan");
        let error = pollster::block_on(flash(
            &mut agent,
            plan.confirm(),
            &mut SyncReader::new(bytes.as_slice()),
            &mut |_| {},
            &Cancel::new(),
        ))
        .expect_err("a package is not written raw");
        assert!(error.to_string().contains("firmware package"), "{error}");
        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
    }
}
