//! The partition table: what is where on a device's flash.
//!
//! [`FlashAgent`] addresses sectors and has no knowledge of what they hold, and the
//! verbs move bytes. This module turns a sector number into a name.
//!
//! Two table formats share one uniform [`PartitionTable`]. Every modern Rockchip
//! board boots from the UEFI [`gpt`]. Older ones use Rockchip's own [`rkparam`]
//! block. Sans-I/O codecs parse both. Three functions here touch a device: [`read`],
//! [`read_gpt_repair_source`] and [`read_param_repair_source`]. All three only read.
//!
//! # Write plans
//!
//! The table also lets a write plan state **what the write would destroy**.
//!
//! `flash 64 boot.img` is a write to sector 64, and the sector number says nothing
//! more. It does not say that sector 64 is the first sector of `uboot`. Nor does it
//! say that an image of the wrong size would run out of `uboot` and into `trust`.
//! [`PartitionTable::overlaps`] answers that, and [`WritePlan`] carries the answer.
//! A person reads it before confirming the write. Without a table, a flashing tool
//! cannot make this check.
//!
//! [`WritePlan`]: crate::verbs::WritePlan

use crate::agent::{FlashAgent, FlashInfo};
use crate::codec::{dfu_alt, gpt, rkparam};
use crate::transport::Transport;
use crate::{Error, Result};

/// Which format a device's partition table is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFormat {
    /// The UEFI GUID Partition Table.
    Gpt,
    /// Rockchip's parameter block, whose `CMDLINE` names the partitions.
    RockchipParam,
    /// A DFU board's alt-settings, read from its interface descriptors rather than
    /// from the flash. It is not a stored table, so it has no checksum to verify and
    /// no copy to repair. The device is authoritative about what it exposes.
    /// [`dfu_alt`] describes it.
    DfuAltInfo,
}

impl TableFormat {
    /// The format's name, for a person reading a report.
    pub fn name(self) -> &'static str {
        match self {
            TableFormat::Gpt => "GPT",
            TableFormat::RockchipParam => "Rockchip parameter",
            TableFormat::DfuAltInfo => "DFU alt-settings",
        }
    }
}

/// One partition, in the form every table format can express.
///
/// It has the three fields every format has: a name, where it starts, and how long
/// it is. Each format records more. GPT has type and instance GUIDs and attribute
/// bits, and a Rockchip parameter carries a kernel command line around its
/// partition list. The codecs parse all of it.
///
/// This type carries none of the extras, because nothing that consumes it reads
/// them. A uniform type carrying both formats' extras would promise fields that a
/// third format cannot fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// The name its table gives it: `uboot`, `trust`, `rootfs`.
    pub name: String,
    /// The first sector it occupies.
    pub first_lba: u64,
    /// How many sectors it occupies.
    ///
    /// A count, not an end. GPT records an inclusive last sector, and a Rockchip
    /// parameter records a length. Each codec converts its form to a count once, so
    /// a caller does no subtraction.
    pub sectors: u64,
}

impl Partition {
    /// One sector past the last one the partition occupies.
    ///
    /// It saturates. For a partition that runs to the end of the 64-bit sector
    /// address, it returns `u64::MAX` rather than wrapping to zero. A wrap would make
    /// that partition look empty to every range check that consults it. One of
    /// those checks decides whether a write lands in it.
    pub fn end_lba(&self) -> u64 {
        self.first_lba.saturating_add(self.sectors)
    }

    /// Refuse an image of `image_bytes` that does not fit inside this partition.
    ///
    /// A write aimed by partition name gets this check, and a write aimed at a
    /// hand-computed LBA does not. A partition is a length as well as a place. An
    /// image too big for its target partition does not stop at the partition's end.
    /// It runs into whatever follows, such as the next bootloader stage on a
    /// Rockchip board. The device raises no objection, because both are good
    /// sectors. Only the table can catch it.
    ///
    /// It refuses with [`Error::InvalidRequest`] rather than truncating. An image
    /// that does not fit was not meant for this partition, or the partition was not
    /// meant for this image. A truncated write would leave the board broken in a way
    /// that is harder to find.
    ///
    /// It compares in sectors, because the write occupies whole sectors whatever
    /// the image's length.
    pub fn must_hold(&self, image_bytes: u64, sector_size: u32) -> Result<()> {
        let sectors = image_bytes.div_ceil(u64::from(sector_size));
        if sectors > self.sectors {
            return Err(Error::InvalidRequest(format!(
                "the image is {image_bytes} bytes, which needs {sectors} sectors, and the \
                 partition '{}' is {} sectors long. Writing it there would overrun the end of \
                 '{}' into whatever follows it",
                self.name, self.sectors, self.name
            )));
        }
        Ok(())
    }
}

/// A device's partition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionTable {
    /// Which format it was read from.
    pub format: TableFormat,
    /// The partitions, in the order the table lists them, which can differ from
    /// their order on the flash.
    pub partitions: Vec<Partition>,
    /// How the table was recovered after its primary copy failed its checks, or
    /// `None`.
    ///
    /// `None` is the ordinary case: the primary copy answered, and its checksums
    /// held. `Some` means the primary was damaged, and the partitions here came from
    /// a redundant copy instead. That copy is the [backup GPT] a UEFI table keeps in
    /// the device's last sector. The partitions are good, and the device's primary
    /// table is damaged. A front-end reports both facts to a person.
    ///
    /// [backup GPT]: read
    pub recovery: Option<TableRecovery>,
}

/// Where a partition table came from, after its primary copy proved damaged and a
/// backup answered instead.
///
/// This is a successful read carrying a caution. The partitions are the intact
/// copy's, and a write can be planned against them. The device's table is not
/// healthy, because its primary copy is damaged. A repair rebuilds that copy from
/// the intact one, and [`read_gpt_repair_source`] reads what the repair needs. A
/// device where no copy passes its checks is an [`Error::CorruptTable`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRecovery {
    /// What was wrong with the primary copy: the detail from the
    /// [`Error::CorruptTable`] the primary raised. The caution can then say why the
    /// primary was rejected.
    pub primary_detail: String,
    /// Where the good copy was found, in words for a person: "the backup GPT in
    /// the device's last sector."
    pub recovered_from: &'static str,
}

impl PartitionTable {
    /// The partition called `name`.
    ///
    /// **The name is matched exactly.** A near miss is an [`Error::NoSuchPartition`]
    /// that lists the names the table does have. The caller is on its way to a
    /// `dump` or a write. A tool that resolved `Boot` to `boot` would also resolve
    /// `boot` to `boot_a` on a board that had both. Retyping a name from the error
    /// costs a person seconds, and a wrong guess can cost a board.
    ///
    /// A name that matches two partitions is an [`Error::InvalidRequest`], for the
    /// same reason. GPT permits duplicate names, and picking the first of the two
    /// would pick the right one half the time.
    pub fn find(&self, name: &str) -> Result<&Partition> {
        let mut matches = self
            .partitions
            .iter()
            .filter(|partition| partition.name == name);

        let Some(found) = matches.next() else {
            return Err(Error::NoSuchPartition {
                wanted: name.to_string(),
                available: self.names(),
            });
        };

        if matches.next().is_some() {
            return Err(Error::InvalidRequest(format!(
                "this device's table has more than one partition named '{name}', and pyrographer \
                 will not guess which one you meant. Address the one you want by its LBA"
            )));
        }

        Ok(found)
    }

    /// The partitions' names, in the order the table lists them.
    pub fn names(&self) -> Vec<String> {
        self.partitions
            .iter()
            .map(|partition| partition.name.clone())
            .collect()
    }

    /// Which partitions the `sectors` sectors starting at `lba` land in, and how
    /// much of each.
    ///
    /// A write plan asks this. **An empty answer does not mean the range is safe.**
    /// It means the range is in no partition. On a Rockchip board, that region
    /// holds the bootloader, so such a write is either exactly what was meant or a
    /// write into a gap.
    ///
    /// Partitions come back in their order on the flash, whatever order the table
    /// lists them in. A person then reads "this write covers the end of `uboot` and
    /// the start of `trust`" in the device's own order.
    pub fn overlaps(&self, lba: u64, sectors: u64) -> Vec<Overlap> {
        let end = lba.saturating_add(sectors);

        let mut overlaps: Vec<Overlap> = self
            .partitions
            .iter()
            .filter_map(|partition| {
                // The intersection of the two half-open ranges. `first` is below
                // `last` exactly when they meet at all, so a partition the write
                // merely abuts -- ending on the sector the other begins at --
                // does not appear, which is right: nothing of it is written.
                let first = lba.max(partition.first_lba);
                let last = end.min(partition.end_lba());
                let covered = last.checked_sub(first).filter(|&n| n > 0)?;

                Some(Overlap {
                    name: partition.name.clone(),
                    first_lba: partition.first_lba,
                    covered,
                    total: partition.sectors,
                })
            })
            .collect();

        overlaps.sort_by_key(|overlap| overlap.first_lba);
        overlaps
    }
}

/// One partition a range of sectors lands in, and how much of it that range
/// covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    /// The partition's name.
    pub name: String,
    /// The partition's first sector, which orders the overlaps.
    pub first_lba: u64,
    /// How many of the partition's sectors the range covers.
    pub covered: u64,
    /// How many sectors the partition has in all.
    pub total: u64,
}

impl Overlap {
    /// Whether the range covers the whole of the partition.
    ///
    /// A `flash` of a boot image is expected to overwrite the whole of `boot`.
    /// Overwriting nine tenths of it is the sign of the wrong image.
    pub fn is_whole(&self) -> bool {
        self.covered == self.total
    }
}

/// Read the device's partition table, or `None` for a device with no table.
///
/// `Ok(None)` is a finding rather than a failure. A board holding a raw image, or a
/// blank one, has no table and is not broken. A table that is present and fails its
/// checks is [`Error::CorruptTable`]. Reporting damage as absence would hide a
/// half-written table behind an empty list, on a device somebody is about to write
/// to.
///
/// `flash` is the geometry [`info`](FlashAgent::info) reported. Every caller has
/// already asked for it, so it is passed in. A Rockchip parameter can name a
/// partition that runs to the end of the flash, and only the flash's size gives
/// that end.
///
/// **Only read commands are sent.** This function is on the dry run's path, and the
/// dry run changes nothing. A DFU board's table is built from its alt-settings, and
/// no command is sent at all.
///
/// # Which table
///
/// GPT is looked for first, at the sector it must be in, and a GPT found there is
/// conclusive. Nothing but a GPT carries `EFI PART` and a CRC that agrees with it.
/// Only a device without one is searched for a Rockchip parameter. A board can
/// carry a stale parameter block from an earlier install and the GPT it boots from.
/// The GPT is the current one.
///
/// # A damaged primary GPT
///
/// GPT keeps two copies: the primary near the front, and a backup in the device's
/// last sector. A primary can carry the signature and still fail its checks. The
/// backup is then read, and a backup that passes its checks is used. The table
/// comes back with [`PartitionTable::recovery`] set, so a caller can report that
/// the primary is damaged. Only a device where neither copy passes is an
/// [`Error::CorruptTable`].
///
/// The backup is located by the device's geometry (the last sector), not by the
/// primary's record of where it is, because the primary's fields cannot be trusted.
///
/// A primary whose signature is gone entirely reads as a device with no GPT, and
/// the parameter search runs. A damaged primary is reported only while its
/// signature survives.
pub async fn read<T: Transport>(
    agent: &mut FlashAgent<T>,
    flash: &FlashInfo,
) -> Result<Option<PartitionTable>> {
    // A DFU board hands its partitions over as alt-settings in its descriptors, not
    // as a table on the flash, so there is nothing to probe: the table is built
    // from what the agent already knows, and no read command is sent. This is the
    // third partition source, and it is checked first because a device that has it
    // has no on-flash table to look for.
    let dfu_sector_size = agent.sector_size();
    if let Some(alts) = agent.dfu_alt_settings() {
        return Ok(Some(dfu_table(alts, dfu_sector_size)));
    }

    let sector_size = agent.sector_size() as usize;

    let mut sector = vec![0u8; sector_size];
    agent.read(gpt::HEADER_LBA, &mut sector).await?;

    if gpt::has_signature(&sector) {
        return Ok(Some(read_gpt(agent, &sector, sector_size, flash).await?));
    }

    read_param(agent, flash, sector_size).await
}

/// Build a DFU board's partition table from its alt-settings.
///
/// This is the third partition source, beside the on-flash [`gpt`] and [`rkparam`]
/// tables, and the simplest. A DFU board's partitions are the alt-settings it
/// exposes, so there is no stored table to parse and no checksum to verify. The
/// device is authoritative.
///
/// Each alt-setting becomes a partition placed at its own slice of the LBA space
/// (see [`dfu_alt`]). A names-only alt-setting reports a zero sector count, which
/// means its extent is unknown, not that it is empty. The table never carries a
/// `recovery` and is never a `CorruptTable`, because there is no copy to go bad.
fn dfu_table(alts: &[dfu_alt::AltSetting], sector_size: u32) -> PartitionTable {
    PartitionTable {
        format: TableFormat::DfuAltInfo,
        partitions: alts
            .iter()
            .map(|alt| Partition {
                name: alt.name.clone(),
                first_lba: dfu_alt::alt_base_lba(alt.index),
                sectors: alt.sectors(sector_size),
            })
            .collect(),
        recovery: None,
    }
}

/// Read a GPT whose primary signature has been seen, with the backup as the
/// fallback for a damaged primary.
///
/// The primary is tried first. If it passes its checks, it is the table, and
/// [`PartitionTable::recovery`] is `None`. If it fails with
/// [`Error::CorruptTable`], the backup is consulted. That is the only error
/// [`read_gpt_copy`] raises that is the table's fault rather than the device's.
/// Any other error is the device failing, and is returned unchanged.
async fn read_gpt<T: Transport>(
    agent: &mut FlashAgent<T>,
    primary_sector: &[u8],
    sector_size: usize,
    flash: &FlashInfo,
) -> Result<PartitionTable> {
    match read_gpt_copy(agent, primary_sector, sector_size).await {
        Ok(partitions) => Ok(PartitionTable {
            format: TableFormat::Gpt,
            partitions,
            recovery: None,
        }),
        Err(Error::CorruptTable { detail, .. }) => {
            read_gpt_backup(agent, sector_size, flash, detail).await
        }
        Err(other) => Err(other),
    }
}

/// Recover the table from the backup GPT in the last sector, after the primary
/// proved damaged, or report that the backup cannot replace it.
///
/// `primary_detail` is what was wrong with the primary. A recovery carries it to
/// say why the primary was rejected, and a both-copies-damaged error names both
/// failures. The specification puts the backup header in the device's last sector.
/// It is read there rather than at the primary's
/// [`backup_lba`](gpt::Header::backup_lba), because the primary's own fields are in
/// doubt.
async fn read_gpt_backup<T: Transport>(
    agent: &mut FlashAgent<T>,
    sector_size: usize,
    flash: &FlashInfo,
    primary_detail: String,
) -> Result<PartitionTable> {
    let corrupt = |detail: String| Error::CorruptTable {
        format: "GPT",
        detail,
    };

    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);
    // The primary carried a signature, so its read succeeded, so the device has
    // at least the two sectors that took -- `checked_sub` cannot underflow here
    // in practice, and refuses rather than wraps if a hand-built `FlashInfo`
    // reports a device with no last sector to hold a backup.
    let Some(backup_lba) = flash_sectors.checked_sub(1) else {
        return Err(corrupt(format!(
            "{primary_detail}, and the device reports no sectors, so there is no last sector to \
             hold a backup GPT"
        )));
    };

    let mut backup = vec![0u8; sector_size];
    agent.read(backup_lba, &mut backup).await?;

    // A backup that is not there -- no signature -- does not save the primary,
    // and the finding is the primary's damage.
    if !gpt::has_signature(&backup) {
        return Err(corrupt(format!(
            "the primary GPT is damaged ({primary_detail}), and the backup GPT header expected in \
             the last sector (LBA {backup_lba}) is absent"
        )));
    }

    match read_gpt_copy(agent, &backup, sector_size).await {
        Ok(partitions) => Ok(PartitionTable {
            format: TableFormat::Gpt,
            partitions,
            recovery: Some(TableRecovery {
                primary_detail,
                recovered_from: "the backup GPT in the device's last sector",
            }),
        }),
        // The backup carried a signature and then did not check out either: both
        // copies are damaged, and there is no table to be had. Name both, because
        // "primary and backup both corrupt" is a worse and more specific finding
        // than either alone.
        Err(Error::CorruptTable {
            detail: backup_detail,
            ..
        }) => Err(corrupt(format!(
            "the primary GPT is damaged ({primary_detail}), and so is the backup in the last \
             sector ({backup_detail})"
        ))),
        Err(other) => Err(other),
    }
}

/// Parse one GPT copy: its header from `header_sector`, then its entry array from
/// the device. It returns the partitions the copy holds, or the
/// [`Error::CorruptTable`] a damaged copy raises.
///
/// The primary and the backup share one layout in different sectors, so both are
/// read here. The backup is a byte-identical entry array with a header that names
/// its own location. Any other error is the device failing, and this function
/// returns it without interpreting it.
async fn read_gpt_copy<T: Transport>(
    agent: &mut FlashAgent<T>,
    header_sector: &[u8],
    sector_size: usize,
) -> Result<Vec<Partition>> {
    read_gpt_copy_bytes(agent, header_sector, sector_size)
        .await
        .map(|(_array, partitions)| partitions)
}

/// [`read_gpt_copy`], returning the raw entry-array bytes as well as the partitions.
///
/// A repair needs the array bytes. It rebuilds the other copy with a byte-for-byte
/// copy of the array, which carries across the GUIDs and attribute bits the uniform
/// [`Partition`] drops. [`read_gpt_copy`] drops the bytes, for the readers that only
/// want the names.
async fn read_gpt_copy_bytes<T: Transport>(
    agent: &mut FlashAgent<T>,
    header_sector: &[u8],
    sector_size: usize,
) -> Result<(Vec<u8>, Vec<Partition>)> {
    let header = gpt::parse_header(header_sector)?;

    // The entry array is read from where the header says it is, and for as many
    // sectors as the header says it needs -- rather than from the sector that
    // usually follows, for the 16 KiB it usually takes. The header is the
    // authority on its own layout, and `parse_header` has already refused a
    // header whose numbers would make this read run away.
    let array_bytes = header.entry_array_len().div_ceil(sector_size) * sector_size;
    let mut array = vec![0u8; array_bytes];
    agent.read(header.entry_array_lba, &mut array).await?;

    let partitions = gpt::parse_entries(&header, &array)?
        .into_iter()
        .map(|entry| Partition {
            // GPT's inclusive end becomes a count here, and this is the only
            // place it does.
            sectors: entry.sectors(),
            first_lba: entry.first_lba,
            name: entry.name,
        })
        .collect();
    Ok((array, partitions))
}

/// Which copy of a GPT a repair rewrites, and which intact copy it rebuilds from.
///
/// GPT keeps two copies, and a repair runs from the intact copy to the damaged one.
/// The board boots from the primary, so a backup repair is the safer of the two. It
/// writes only the redundant copy at the end of the disk, never the primary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptRepairDirection {
    /// The primary is damaged, and the intact backup is the source.
    PrimaryFromBackup,
    /// The backup is stale, damaged, or absent, and the intact primary is the
    /// source.
    BackupFromPrimary,
}

/// What reading a device for a GPT repair found.
///
/// A repair rewrites the damaged copy from the intact one. It acts only on
/// [`Repairable`](Self::Repairable), whose source carries the direction.
///
/// The other three variants leave nothing to repair, or nothing to repair from.
/// They are distinct so that a person who asked for a repair learns why it does
/// not run. Both copies healthy, no table at all, and no intact copy left are
/// different answers.
///
/// A device that stops answering mid-read is an [`Err`], not one of these
/// variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GptRepair {
    /// One copy is damaged and the other is intact. This carries the damaged copy
    /// rebuilt from the intact one, the direction of the repair, and the partitions
    /// it restores.
    Repairable(GptRepairSource),
    /// Both copies pass their checks and agree. There is nothing to repair.
    Healthy,
    /// There is no primary GPT signature, so there is no GPT to repair. Writing a
    /// fresh table from a layout is authoring, a separate operation.
    NoGpt,
    /// The primary is damaged, and the backup is absent or damaged too. There is no
    /// intact copy to rebuild the primary from. A damaged backup is never
    /// unrepairable, because the backup is checked only after the primary passes.
    Unrepairable {
        /// What was wrong, so the refusal can say why the repair cannot run.
        detail: String,
    },
}

/// The material a GPT repair writes: the rebuilt copy, which direction the repair
/// runs, and the partitions it restores.
///
/// The [`rebuilt`](Self::rebuilt) bytes are the damaged copy, built byte-faithfully
/// from the intact one by [`gpt::rebuild_primary_from_backup`] or
/// [`gpt::rebuild_backup_from_primary`]. The [`partitions`](Self::partitions) are
/// what a plan shows a person, so they can recognize the table. Both come from the
/// intact copy, so the list describes exactly the bytes that are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptRepairSource {
    /// Which copy is being rewritten, and which is the source.
    pub direction: GptRepairDirection,
    /// The copy rebuilt from the intact one, ready to write.
    pub rebuilt: gpt::RebuiltGpt,
    /// The partitions the rebuilt table carries, in table order.
    pub partitions: Vec<Partition>,
}

/// Read what a GPT repair needs: which copy is damaged, and the intact copy to
/// rebuild it from.
///
/// It is the read half of the repair, and a counterpart of [`read`]. It sends only
/// read commands and changes nothing, so a dry run can call it. It reads the
/// primary, and then the backup, whichever way the damage runs. A damaged primary
/// is rebuilt from the backup. With an intact primary, the backup is checked, and a
/// stale, damaged or missing backup is rebuilt from the primary.
///
/// The four outcomes are the [`GptRepair`] variants. A device that fails a read is
/// an [`Err`], not one of them.
///
/// `flash` is the geometry, passed in for the same reason [`read`] takes it. The
/// backup is located by geometry in the device's last sector, as [`read`] locates
/// it, and only the flash's size gives that sector.
pub async fn read_gpt_repair_source<T: Transport>(
    agent: &mut FlashAgent<T>,
    flash: &FlashInfo,
) -> Result<GptRepair> {
    let sector_size = agent.sector_size() as usize;
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

    let mut primary = vec![0u8; sector_size];
    agent.read(gpt::HEADER_LBA, &mut primary).await?;

    // No signature is no GPT -- a different finding from a damaged one, and not a
    // thing this repairs. A blank primary is where a fresh table would be authored,
    // which this is not.
    if !gpt::has_signature(&primary) {
        return Ok(GptRepair::NoGpt);
    }

    // The primary decides which way the repair runs. A primary that announces
    // itself and then checks out sends the repair toward the backup; a damaged one
    // sends it toward the primary. A read that failed for any other reason is the
    // device failing, and comes straight back out.
    match read_gpt_copy_bytes(agent, &primary, sector_size).await {
        Ok((primary_array, partitions)) => {
            repair_backup_if_needed(
                agent,
                &primary,
                &primary_array,
                partitions,
                flash_sectors,
                sector_size,
            )
            .await
        }
        Err(Error::CorruptTable { .. }) => {
            repair_primary_from_backup(agent, flash_sectors, sector_size).await
        }
        Err(other) => Err(other),
    }
}

/// Rebuild a damaged primary from the backup, or report that the backup cannot
/// replace it.
///
/// It reads the backup in the last sector. A backup that passes its checks yields
/// the primary rebuilt from it. A backup that is absent or damaged is
/// [`GptRepair::Unrepairable`], because no intact copy is left.
async fn repair_primary_from_backup<T: Transport>(
    agent: &mut FlashAgent<T>,
    flash_sectors: u64,
    sector_size: usize,
) -> Result<GptRepair> {
    let unrepairable = |detail: String| GptRepair::Unrepairable { detail };

    let Some(backup_lba) = flash_sectors.checked_sub(1) else {
        return Ok(unrepairable(
            "the device reports no sectors, so there is no last sector to hold a backup GPT"
                .to_string(),
        ));
    };

    let mut backup_header = vec![0u8; sector_size];
    agent.read(backup_lba, &mut backup_header).await?;
    if !gpt::has_signature(&backup_header) {
        return Ok(unrepairable(format!(
            "the backup GPT header expected in the last sector (LBA {backup_lba}) is absent"
        )));
    }

    let (backup_array, partitions) =
        match read_gpt_copy_bytes(agent, &backup_header, sector_size).await {
            Ok(copy) => copy,
            Err(Error::CorruptTable { detail, .. }) => {
                return Ok(unrepairable(format!(
                    "the backup GPT is damaged too ({detail})"
                )));
            }
            Err(other) => return Err(other),
        };

    // Build the primary from the backup's own bytes. A backup that checks out but
    // leaves no room for a primary array -- an odd geometry -- is the last way this
    // cannot go ahead, and it surfaces here as the rebuild refusing.
    let rebuilt = match gpt::rebuild_primary_from_backup(
        &backup_header,
        &backup_array,
        backup_lba,
        sector_size,
    ) {
        Ok(rebuilt) => rebuilt,
        Err(Error::CorruptTable { detail, .. }) => return Ok(unrepairable(detail)),
        Err(other) => return Err(other),
    };

    Ok(GptRepair::Repairable(GptRepairSource {
        direction: GptRepairDirection::PrimaryFromBackup,
        rebuilt,
        partitions,
    }))
}

/// Check the backup against an intact primary, and rebuild a stale, damaged or
/// missing backup from it.
///
/// This is the safer direction, because the primary the board boots from is never
/// written. A backup that is present, passes its checks and holds the same table as
/// the primary leaves nothing to repair. Otherwise the primary is authoritative,
/// and the backup is rebuilt from it.
async fn repair_backup_if_needed<T: Transport>(
    agent: &mut FlashAgent<T>,
    primary_header: &[u8],
    primary_array: &[u8],
    partitions: Vec<Partition>,
    flash_sectors: u64,
    sector_size: usize,
) -> Result<GptRepair> {
    let healthy = match flash_sectors.checked_sub(1) {
        // No last sector means no backup at all; the rebuild below is the authority
        // on whether one can be laid out, and it refuses if it cannot.
        None => false,
        Some(backup_lba) => {
            is_backup_healthy(agent, primary_header, backup_lba, sector_size).await?
        }
    };
    if healthy {
        return Ok(GptRepair::Healthy);
    }

    let rebuilt = match gpt::rebuild_backup_from_primary(
        primary_header,
        primary_array,
        flash_sectors,
        sector_size,
    ) {
        Ok(rebuilt) => rebuilt,
        Err(Error::CorruptTable { detail, .. }) => return Ok(GptRepair::Unrepairable { detail }),
        Err(other) => return Err(other),
    };

    Ok(GptRepair::Repairable(GptRepairSource {
        direction: GptRepairDirection::BackupFromPrimary,
        rebuilt,
        partitions,
    }))
}

/// Whether the backup is present, passes its checks, and holds the same table as
/// the primary.
///
/// All three conditions must hold. An absent or damaged backup is not healthy. A
/// backup that passes its own checks but describes a different table from the
/// primary is stale. The primary is authoritative, so a stale backup is rebuilt
/// too. A tool that wrote only the primary leaves a valid but old backup behind,
/// and a CRC check alone would call it healthy.
async fn is_backup_healthy<T: Transport>(
    agent: &mut FlashAgent<T>,
    primary_header: &[u8],
    backup_lba: u64,
    sector_size: usize,
) -> Result<bool> {
    let mut backup_header = vec![0u8; sector_size];
    agent.read(backup_lba, &mut backup_header).await?;
    if !gpt::has_signature(&backup_header) {
        return Ok(false); // absent
    }

    // Its header and array both have to check out on their own.
    match read_gpt_copy_bytes(agent, &backup_header, sector_size).await {
        Ok(_) => {}
        Err(Error::CorruptTable { .. }) => return Ok(false), // damaged
        Err(other) => return Err(other),
    }

    // And it has to be the same table the primary holds, or it is stale. Both
    // headers parsed cleanly to get here, so the comparison cannot fault.
    let primary = gpt::parse_header(primary_header)?;
    let backup = gpt::parse_header(&backup_header)?;
    Ok(headers_agree(&primary, &backup))
}

/// Whether two GPT headers describe the same table.
///
/// It compares the fields that are identical in a healthy GPT's two copies. Those
/// fields differ once one copy is stale.
///
/// The sector fields (`current_lba`, `backup_lba` and `entry_array_lba`) are not
/// compared, because each copy names its own position and the two always differ.
/// The other fields describe the table itself. A difference in the entry-array CRC
/// shows that the two copies hold different partitions.
fn headers_agree(a: &gpt::Header, b: &gpt::Header) -> bool {
    a.disk_guid == b.disk_guid
        && a.first_usable_lba == b.first_usable_lba
        && a.last_usable_lba == b.last_usable_lba
        && a.entry_count == b.entry_count
        && a.entry_len == b.entry_len
        && a.entry_array_crc == b.entry_array_crc
}

/// Look for a Rockchip parameter block in each of the sectors one is written to.
///
/// Rockchip writes the parameter more than once (eight times on raw NAND), so that
/// a bad block in one copy does not cost the board its table. Each copy is tried in
/// turn, and the first that passes its checks is the answer. A copy that carries
/// the magic and fails its CRC is not fatal alone, because the next copy can be
/// intact. If every copy present is damaged, the read fails with the last copy's
/// error.
///
/// Each copy is probed with a single sector, which is enough to see the magic and
/// read the block's length. Only a sector that carries the magic costs a second
/// read. Probing all nine locations with a parameter's 64 KiB maximum would move
/// half a megabyte to find a kilobyte of text.
async fn read_param<T: Transport>(
    agent: &mut FlashAgent<T>,
    flash: &FlashInfo,
    sector_size: usize,
) -> Result<Option<PartitionTable>> {
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);
    let mut probe = vec![0u8; sector_size];
    let mut damaged = None;

    for location in rkparam::LOCATIONS {
        // A copy that would sit past the end of a small part is not a copy that
        // has gone missing; there was never room for it. Nothing to read, and
        // nothing to conclude.
        if location.lba >= flash_sectors {
            continue;
        }

        agent.read(location.lba, &mut probe).await?;
        if !rkparam::has_magic(&probe) {
            continue;
        }

        match read_param_at(agent, location, &probe, sector_size, flash_sectors).await {
            Ok(table) => return Ok(Some(table)),
            // A read that failed is the device failing, and no other copy is
            // going to fare better; only a copy that was *damaged* is worth
            // stepping over.
            Err(error @ Error::CorruptTable { .. }) => damaged = Some(error),
            Err(error) => return Err(error),
        }
    }

    match damaged {
        // Every copy that was there was damaged, and there is no table to be had.
        Some(error) => Err(error),
        // No copy carried the magic at all -- and there was no GPT either, or we
        // would not have looked here. The device has no partition table, which is
        // a thing a device is allowed to be.
        None => Ok(None),
    }
}

/// Read the parameter block whose first sector is `probe`, at `location`.
///
/// `location` carries both the sector that holds the block and the base its
/// partition offsets are counted from. On an eMMC the base is not zero. It is the
/// one value in this format that can place a partition 4 MiB from where it really
/// is. [`rkparam::parse`] describes it in full.
async fn read_param_at<T: Transport>(
    agent: &mut FlashAgent<T>,
    location: &rkparam::Location,
    probe: &[u8],
    sector_size: usize,
    flash_sectors: u64,
) -> Result<PartitionTable> {
    // The header says how long the block is, so the read is sized by the block
    // and not by the cap on it.
    let block_sectors = rkparam::block_len(probe)?.div_ceil(sector_size);

    let mut block = vec![0u8; block_sectors * sector_size];
    agent.read(location.lba, &mut block).await?;

    let param = rkparam::parse(&block, location.base_lba, flash_sectors)?;

    Ok(PartitionTable {
        format: TableFormat::RockchipParam,
        partitions: param
            .partitions
            .into_iter()
            .map(|part| Partition {
                name: part.name,
                first_lba: part.first_lba,
                sectors: part.sectors,
            })
            .collect(),
        // The parameter format keeps several copies too, and `read_param` steps
        // over a damaged one for the next -- but which copy answered is not
        // surfaced by this read path today, so this stays `None`. Which copies
        // are damaged is surfaced by [`read_param_repair_source`], on the repair
        // path, where it is what a repair acts on. The recovery a plain read tells
        // a caller about is the GPT backup.
        recovery: None,
    })
}

/// What reading a device for a Rockchip parameter repair found.
///
/// It is the parameter counterpart of [`GptRepair`], and the repair works the same
/// way: a damaged copy is rewritten from an intact one. It relies on the several
/// copies rkflashtool writes on raw NAND, so a bad block in one leaves the others.
/// Those are the intact copies a repair rebuilds from.
///
/// **An eMMC board carries a single parameter copy.** A damaged one there has no
/// intact sibling, and comes back [`Unrepairable`](ParamRepair::Unrepairable), as
/// a GPT with no backup does. Authoring a fresh block is the fix.
///
/// A device that stops answering mid-read is an [`Err`], not one of these
/// variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamRepair {
    /// One or more copies carried the magic and failed their CRC, and an intact
    /// copy shares their layout. This carries the intact block, the sectors of the
    /// damaged copies to rewrite with it, and the partitions it restores.
    Repairable(ParamRepairSource),
    /// An intact copy exists, and no damaged copy shares its base. There is nothing
    /// to repair.
    ///
    /// The base is that of the first intact copy in scan order. A damaged copy that
    /// counts from a different base is left alone, as
    /// [`read_param_repair_source`] explains, and the answer is still `Healthy`.
    ///
    /// A parameter has no primary-and-backup authority as a GPT has. Two intact
    /// copies carrying different tables therefore cannot be ranked here. Neither is
    /// known to be the stale one, and rewriting the wrong one would make the damage
    /// worse. Intact copies are left alone whether they agree or not. Only a copy
    /// that carries the magic and then fails its CRC is repaired.
    ///
    /// This is the one way parameter repair falls short of [`GptRepair`]. It is a
    /// limit of the format, which keeps no record of which copy is newer.
    Healthy,
    /// No copy carries the parameter magic, so there is no table to repair.
    /// Authoring a fresh one from a layout is a separate operation.
    NoTable,
    /// Copies are damaged, and none is intact: a single eMMC copy gone bad, or every
    /// NAND copy corrupt. There is nothing intact to rebuild from.
    Unrepairable {
        /// What was wrong, so the refusal can say why the repair cannot run.
        detail: String,
    },
}

/// The material a Rockchip parameter repair writes: the intact copy's block, the
/// sectors of the damaged copies it rewrites, and the partitions the table holds.
///
/// A parameter copy keeps no record of its own location, unlike a GPT header, which
/// names its own sector and its twin's. A copy is therefore position-independent.
/// The intact copy's [`block`](Self::block) bytes are written verbatim to each
/// damaged copy's sector, and nothing else changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamRepairSource {
    /// The intact copy's block, the exact bytes written to each damaged copy.
    pub block: Vec<u8>,
    /// The sectors of the damaged copies to rewrite, each with the same
    /// [`block`](Self::block).
    pub damaged_lbas: Vec<u64>,
    /// The base the live copies count their offsets from, eMMC's or NAND's, so a
    /// plan can report which layout the table is in.
    pub base_lba: u64,
    /// The partitions the intact copy holds, in the order it names them.
    pub partitions: Vec<Partition>,
}

/// Read what a Rockchip parameter repair needs: the damaged copies, and the
/// intact copy to rewrite them from.
///
/// It is the parameter counterpart of [`read_gpt_repair_source`]. It sends only
/// read commands and changes nothing, so a dry run can call it. It looks in every
/// sector a parameter is written to, and classifies each copy that carries the
/// magic as intact or damaged. When a damaged copy has an intact sibling counting
/// from the same base, it returns the intact block to rewrite the damaged copies
/// with. The four outcomes are the [`ParamRepair`] variants, and a device that
/// fails a read is an [`Err`], not one of them.
///
/// The base a copy counts from distinguishes eMMC from NAND, as in the plain
/// parameter read. Only copies sharing the base of the first intact copy are
/// treated as its siblings. A stale block in the NAND region of a board that boots
/// from eMMC counts from a different base. Rewriting it with an eMMC block would
/// place its partitions 4 MiB from where they belong, so it is left alone.
///
/// `flash` is the geometry, passed in for the same reason [`read`] takes it. A copy
/// whose sector is past the end of a small part was never there to be damaged.
pub async fn read_param_repair_source<T: Transport>(
    agent: &mut FlashAgent<T>,
    flash: &FlashInfo,
) -> Result<ParamRepair> {
    let sector_size = agent.sector_size() as usize;
    let flash_sectors = flash.size_bytes / u64::from(flash.sector_size);

    /// One copy the scan found, classified by whether it passes its checks.
    enum Found {
        Intact {
            base_lba: u64,
            block: Vec<u8>,
            partitions: Vec<Partition>,
        },
        Damaged {
            lba: u64,
            base_lba: u64,
            detail: String,
        },
    }

    let mut probe = vec![0u8; sector_size];
    let mut found: Vec<Found> = Vec::new();

    for location in rkparam::LOCATIONS {
        // A copy that would sit past the end of a small part was never there;
        // nothing to read, nothing to conclude.
        if location.lba >= flash_sectors {
            continue;
        }

        agent.read(location.lba, &mut probe).await?;
        if !rkparam::has_magic(&probe) {
            continue;
        }

        // A copy that carries the magic but whose header will not even give up a
        // length is damaged, and costs no second read.
        let block_len = match rkparam::block_len(&probe) {
            Ok(len) => len,
            Err(Error::CorruptTable { detail, .. }) => {
                found.push(Found::Damaged {
                    lba: location.lba,
                    base_lba: location.base_lba,
                    detail,
                });
                continue;
            }
            Err(other) => return Err(other),
        };

        let block_sectors = block_len.div_ceil(sector_size);
        let mut block = vec![0u8; block_sectors * sector_size];
        agent.read(location.lba, &mut block).await?;

        match rkparam::parse(&block, location.base_lba, flash_sectors) {
            Ok(param) => {
                // The block, trimmed to its own length: the padding out to the
                // sector is not part of it, and a repair writes the block and lets
                // the write pad the sector, exactly as the source copy is laid out.
                block.truncate(block_len);
                let partitions = param
                    .partitions
                    .into_iter()
                    .map(|part| Partition {
                        name: part.name,
                        first_lba: part.first_lba,
                        sectors: part.sectors,
                    })
                    .collect();
                found.push(Found::Intact {
                    base_lba: location.base_lba,
                    block,
                    partitions,
                });
            }
            Err(Error::CorruptTable { detail, .. }) => found.push(Found::Damaged {
                lba: location.lba,
                base_lba: location.base_lba,
                detail,
            }),
            Err(other) => return Err(other),
        }
    }

    if found.is_empty() {
        return Ok(ParamRepair::NoTable);
    }

    // The live copy is the first intact one, in scan order -- eMMC before NAND,
    // the same precedence [`read_param`] gives them.
    let source = found.iter().find_map(|copy| match copy {
        Found::Intact {
            base_lba,
            block,
            partitions,
        } => Some((*base_lba, block.clone(), partitions.clone())),
        Found::Damaged { .. } => None,
    });

    let Some((base_lba, block, partitions)) = source else {
        // Copies announced themselves and none is intact: there is nothing to
        // rebuild from. Name the first damage, the way a both-copies-damaged GPT
        // does.
        let detail = found
            .iter()
            .find_map(|copy| match copy {
                Found::Damaged { detail, .. } => Some(detail.clone()),
                Found::Intact { .. } => None,
            })
            .expect("a non-empty scan with no intact copy holds a damaged one");
        return Ok(ParamRepair::Unrepairable {
            detail: format!("no intact Rockchip parameter copy remains: {detail}"),
        });
    };

    // The damaged copies sharing the live base are the ones a repair rewrites; a
    // damaged copy counting from a different base belongs to a layout this intact
    // block is not the source for.
    let damaged_lbas: Vec<u64> = found
        .iter()
        .filter_map(|copy| match copy {
            Found::Damaged {
                lba,
                base_lba: base,
                ..
            } if *base == base_lba => Some(*lba),
            _ => None,
        })
        .collect();

    if damaged_lbas.is_empty() {
        return Ok(ParamRepair::Healthy);
    }

    Ok(ParamRepair::Repairable(ParamRepairSource {
        block,
        damaged_lbas,
        base_lba,
        partitions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::RockusbAgent;
    use crate::codec::rkparam;
    use crate::testing::{
        gpt_entry, param_block, scripted_gpt, scripted_no_table, scripted_read, sector,
    };
    use crate::transport::testing::{ScriptedTransport, Step};

    /// The geometry of the part these tests read a table from: 122,142,720 sectors
    /// of 512 bytes, about 58.2 GiB. That is the capacity of a 64 GB-class eMMC.
    fn a_flash() -> FlashInfo {
        FlashInfo {
            size_bytes: 122_142_720 * 512,
            sector_size: 512,
            medium: None,
            chip_id: None,
        }
    }

    /// An agent over a scripted conversation.
    fn agent(steps: Vec<Step>) -> FlashAgent<ScriptedTransport> {
        FlashAgent::Rockusb(RockusbAgent::new(ScriptedTransport::new(steps)))
    }

    /// Read the table from a scripted device, and assert that the whole script was
    /// played out. A lookup that read too few sectors, or too many, then fails here
    /// rather than on a board.
    fn read_from(steps: Vec<Step>) -> Result<Option<PartitionTable>> {
        let mut agent = agent(steps);
        let table = pollster::block_on(read(&mut agent, &a_flash()));

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
        table
    }

    /// A DFU board's table is its alt-settings, and reading it touches no flash.
    /// The partitions come from the descriptors the agent already holds, so the
    /// script is empty. A probe read that reached the transport would panic. A
    /// sized alt-setting reports its sector count. A names-only one reports zero,
    /// which means its extent is unknown, not that it is empty.
    #[test]
    fn a_dfu_boards_table_is_its_alt_settings_and_touches_no_flash() {
        use crate::agent::DfuAgent;
        use crate::codec::dfu_alt::{self, AltSetting};

        let alts = vec![
            AltSetting {
                index: 0,
                name: "uboot".to_string(),
                size: Some(256 * 1024),
            },
            AltSetting {
                index: 1,
                name: "rootfs".to_string(),
                size: None,
            },
        ];
        // Transfer size 512, so a sector is 512 bytes and uboot is 512 sectors.
        let mut agent = FlashAgent::Dfu(DfuAgent::new(
            ScriptedTransport::new(vec![]),
            0,
            crate::testing::dfu_capable(512),
            alts,
        ));

        let table = pollster::block_on(read(&mut agent, &a_flash()))
            .expect("a DFU table needs no flash read")
            .expect("the board exposes alt-settings");

        assert_eq!(table.format, TableFormat::DfuAltInfo);
        assert_eq!(table.names(), ["uboot", "rootfs"]);
        assert_eq!(table.partitions[0].first_lba, dfu_alt::alt_base_lba(0));
        assert_eq!(table.partitions[0].sectors, 256 * 1024 / 512);
        assert_eq!(table.partitions[1].first_lba, dfu_alt::alt_base_lba(1));
        assert_eq!(
            table.partitions[1].sectors, 0,
            "a names-only alt-setting reports an unknown extent, not an empty one"
        );

        let FlashAgent::Dfu(agent) = &agent else {
            unreachable!("built a DFU agent")
        };
        agent.transport().assert_drained();
    }

    /// A GPT read from a device: the header at sector 1, then the entry array where
    /// the header says it is. The partitions come back with their inclusive ends
    /// turned into counts.
    #[test]
    fn a_gpt_is_read_off_the_device_that_carries_one() {
        let table = read_from(scripted_gpt(
            1,
            &[
                gpt_entry(16384, 24575, "uboot"),
                gpt_entry(24576, 32767, "trust"),
            ],
        ))
        .expect("a well-formed GPT")
        .expect("the device has one");

        assert_eq!(table.format, TableFormat::Gpt);
        assert_eq!(table.names(), ["uboot", "trust"]);
        assert_eq!(table.partitions[0].first_lba, 16384);
        assert_eq!(table.partitions[0].sectors, 8192);
    }

    /// A device with no GPT is searched for a Rockchip parameter. The sector the
    /// block was found in decides how its offsets are read. This one is at the eMMC
    /// base, so `uboot`, written `@0x2000` in the text, is at LBA 0x4000. That is
    /// 4 MiB further on than the text appears to say.
    ///
    /// Those 4 MiB separate writing the bootloader from writing over the parameter
    /// block, and no part of the text says which is meant. See `rkparam::parse`.
    #[test]
    fn a_parameter_is_read_when_there_is_no_gpt_and_its_base_says_where_it_is() {
        let block = param_block(
            "CMDLINE: mtdparts=rk29xxnand:0x00002000@0x00002000(uboot),\
             0x00002000@0x00004000(trust)\n",
        );

        // No GPT at sector 1, and then the parameter at the sector an eMMC keeps
        // one in -- probed with a sector, and then read in full.
        let mut steps = scripted_read(1, gpt::HEADER_LBA, vec![0u8; 512]);
        steps.extend(scripted_read(2, rkparam::EMMC_BASE_LBA, sector(&block)));
        steps.extend(scripted_read(3, rkparam::EMMC_BASE_LBA, sector(&block)));

        let table = read_from(steps)
            .expect("a well-formed parameter")
            .expect("the device has one");

        assert_eq!(table.format, TableFormat::RockchipParam);
        assert_eq!(table.names(), ["uboot", "trust"]);
        assert_eq!(
            table.partitions[0].first_lba,
            0x2000 + rkparam::EMMC_BASE_LBA,
            "the offsets are counted from the base the block was found at"
        );
    }

    /// A device with neither table has no partition table, and that is a finding
    /// rather than a failure. A board holding a raw image, or a blank one, is not
    /// broken.
    ///
    /// It takes ten reads to establish. Every sector a parameter is kept in must be
    /// checked before "there is none" is an answer rather than a guess.
    #[test]
    fn a_device_with_neither_table_has_no_partition_table() {
        let table = read_from(scripted_no_table(1)).expect("nothing failed");
        assert!(table.is_none());
    }

    /// A damaged primary GPT is recovered from the backup. GPT keeps a second copy
    /// in the device's last sector. When the primary carries the signature and
    /// then fails its checks, `read` uses that copy. The partitions come back from
    /// the backup, and the table records that the primary was damaged. A person is
    /// then told "your primary is corrupt and your backup is intact".
    #[test]
    fn a_damaged_primary_gpt_is_recovered_from_the_backup() {
        let entries = [gpt_entry(16384, 24575, "uboot")];

        // A primary that announces itself and then fails its own header CRC.
        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff; // move first_usable_lba; the header CRC no longer holds

        // An intact backup in the last sector, its array in the sector before it.
        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);

        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, backup_header));
        steps.extend(scripted_read(3, backup_array_lba, sector(&backup_array)));

        let table = read_from(steps)
            .expect("the backup is intact")
            .expect("a table was recovered");

        assert_eq!(table.format, TableFormat::Gpt);
        assert_eq!(table.names(), ["uboot"]);
        let recovery = table
            .recovery
            .expect("the table was recovered from the backup");
        assert!(
            recovery.primary_detail.contains("CRC"),
            "the caution names why the primary was rejected: {}",
            recovery.primary_detail
        );
        assert!(recovery.recovered_from.contains("backup"));
    }

    /// A damaged primary is not treated as a missing one, and does not fall
    /// through to the parameter search. When the backup is gone too, there is no
    /// table to read. The result is an [`Error::CorruptTable`] naming both copies,
    /// not the parameter block and not a blank list. Either of those would hide a
    /// half-written table from somebody about to write to the board again.
    #[test]
    fn a_gpt_with_a_damaged_primary_and_no_backup_is_reported_as_corrupt() {
        let entries = [gpt_entry(16384, 24575, "uboot")];

        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff;

        let backup_lba = a_flash().size_bytes / 512 - 1;

        // The backup sector is blank -- no signature. The script has the primary
        // and the backup read and nothing else: a lookup that fell through to the
        // parameter search would run the transport off the end of it.
        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, vec![0u8; 512]));

        let error =
            read_from(steps).expect_err("the primary is damaged and there is no backup to recover");

        assert!(
            matches!(error, Error::CorruptTable { format: "GPT", .. }),
            "{error:?}"
        );
    }

    /// When both copies carry a signature and then fail their checksums, the error
    /// names both. A board whose primary and backup are both damaged is in a worse
    /// state than either alone, and the finding says so.
    #[test]
    fn a_gpt_with_both_copies_damaged_names_both_in_the_error() {
        let entries = [gpt_entry(16384, 24575, "uboot")];

        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff;

        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (mut backup_header, _) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);
        backup_header[0x30] ^= 0xff; // move last_usable_lba; the backup's header CRC breaks too

        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, backup_header));

        let error = read_from(steps).expect_err("neither copy checks out");

        let Error::CorruptTable {
            format: "GPT",
            detail,
        } = error
        else {
            panic!("both copies damaged is a corrupt GPT: {error:?}");
        };
        assert!(
            detail.contains("primary") && detail.contains("backup"),
            "the finding names both copies: {detail}"
        );
    }

    /// Rockchip writes the parameter eight times on raw NAND, so that a bad block
    /// in one copy does not cost the board its table. A copy that does not parse is
    /// therefore skipped, and the next one is tried. A reader that gave up at the
    /// first damaged copy would discard that redundancy.
    #[test]
    fn a_damaged_parameter_copy_is_stepped_over_for_the_next_one() {
        let good = param_block("CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n");

        let mut damaged = good.clone();
        damaged[rkparam::HEADER_LEN + 1] ^= 0xff; // a byte of the text; the CRC no longer holds

        // No GPT. The eMMC copy is damaged -- probed, read, and refused -- and
        // the first NAND copy behind it is intact.
        let mut steps = scripted_read(1, gpt::HEADER_LBA, vec![0u8; 512]);
        steps.extend(scripted_read(2, rkparam::EMMC_BASE_LBA, sector(&damaged)));
        steps.extend(scripted_read(3, rkparam::EMMC_BASE_LBA, sector(&damaged)));
        steps.extend(scripted_read(4, 0, sector(&good)));
        steps.extend(scripted_read(5, 0, sector(&good)));

        let table = read_from(steps)
            .expect("the second copy is intact")
            .expect("the device has a table");

        assert_eq!(table.names(), ["boot"]);
        // And it was read as raw NAND, because that is the copy that answered:
        // its offsets are counted from zero, not from the eMMC base.
        assert_eq!(table.partitions[0].first_lba, 0x200);
    }

    /// A board whose every copy is damaged has no table to read. It is reported as
    /// damaged rather than as a board with no table.
    #[test]
    fn a_parameter_whose_every_copy_is_damaged_is_reported_as_damaged() {
        let mut damaged = param_block("CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n");
        damaged[rkparam::HEADER_LEN + 1] ^= 0xff;

        let mut steps = scripted_read(1, gpt::HEADER_LBA, vec![0u8; 512]);
        let mut tag = 2;
        for location in rkparam::LOCATIONS {
            steps.extend(scripted_read(tag, location.lba, sector(&damaged)));
            steps.extend(scripted_read(tag + 1, location.lba, sector(&damaged)));
            tag += 2;
        }

        let error = read_from(steps).expect_err("not one copy checks out");
        assert!(
            matches!(
                error,
                Error::CorruptTable {
                    format: "Rockchip parameter",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    /// Read the repair source from a scripted device, and drain the whole script. A
    /// read too many or too few then fails here.
    fn repair_source_from(steps: Vec<Step>) -> Result<GptRepair> {
        let mut agent = agent(steps);
        let outcome = pollster::block_on(read_gpt_repair_source(&mut agent, &a_flash()));

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
        outcome
    }

    /// The repairable case. A repair acts on a primary that carries the signature
    /// and then fails its CRC, with an intact backup. The result carries the
    /// backup's partitions and a rebuilt primary. The rebuilt primary parses as a
    /// healthy GPT that names the backup's sector.
    #[test]
    fn a_damaged_primary_with_an_intact_backup_is_repairable() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff; // move first_usable_lba; the header CRC no longer holds

        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);

        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, backup_header));
        steps.extend(scripted_read(3, backup_array_lba, sector(&backup_array)));

        let outcome = repair_source_from(steps).expect("the device answered every read");
        let GptRepair::Repairable(source) = outcome else {
            panic!("a damaged primary over an intact backup is repairable: {outcome:?}");
        };

        assert_eq!(source.direction, GptRepairDirection::PrimaryFromBackup);
        assert_eq!(source.partitions.len(), 1);
        assert_eq!(source.partitions[0].name, "uboot");
        assert_eq!(source.rebuilt.lba, gpt::HEADER_LBA);
        let header =
            gpt::parse_header(&source.rebuilt.bytes[..512]).expect("a healthy rebuilt primary");
        assert_eq!(header.current_lba, gpt::HEADER_LBA);
        assert_eq!(
            header.backup_lba, backup_lba,
            "and it names the backup by geometry"
        );
    }

    /// Two healthy copies in agreement leave nothing to repair. The repair source
    /// reads the backup even for a passing primary, so that a stale or damaged
    /// backup is noticed. When both pass their checks and describe the same table,
    /// the answer is `Healthy`.
    #[test]
    fn both_copies_healthy_and_in_agreement_have_nothing_to_repair() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);

        // The primary (header at 1, array at 2), and then the backup behind it.
        let mut steps = scripted_gpt(1, &entries);
        steps.extend(scripted_read(3, backup_lba, backup_header));
        steps.extend(scripted_read(4, backup_array_lba, sector(&backup_array)));

        assert_eq!(
            repair_source_from(steps).expect("answered"),
            GptRepair::Healthy
        );
    }

    /// The backup repair. A healthy primary with an absent backup is a board with
    /// one good copy. The repair rewrites the backup from the primary. That is the
    /// safer direction, which never touches the primary the board boots from. The
    /// rebuilt backup puts its header on the device's last sector.
    #[test]
    fn a_healthy_primary_with_an_absent_backup_repairs_the_backup() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;

        let mut steps = scripted_gpt(1, &entries); // primary healthy
        steps.extend(scripted_read(3, backup_lba, vec![0u8; 512])); // backup sector blank

        let outcome = repair_source_from(steps).expect("answered");
        let GptRepair::Repairable(source) = outcome else {
            panic!("a healthy primary over an absent backup repairs the backup: {outcome:?}");
        };
        assert_eq!(source.direction, GptRepairDirection::BackupFromPrimary);
        assert_eq!(source.partitions[0].name, "uboot");
        let run_sectors = (source.rebuilt.bytes.len() / 512) as u64;
        assert_eq!(
            source.rebuilt.lba + run_sectors - 1,
            backup_lba,
            "the rebuilt backup's header lands on the last sector"
        );
    }

    /// A healthy primary with a damaged backup is repaired the same way: the backup
    /// is rewritten from the primary. The damaged backup costs no array read,
    /// because its header fails first.
    #[test]
    fn a_healthy_primary_with_a_damaged_backup_repairs_the_backup() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (mut backup_header, _) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);
        backup_header[0x30] ^= 0xff; // the backup header CRC no longer holds

        let mut steps = scripted_gpt(1, &entries);
        steps.extend(scripted_read(3, backup_lba, backup_header));

        let outcome = repair_source_from(steps).expect("answered");
        let GptRepair::Repairable(source) = outcome else {
            panic!("a damaged backup under a healthy primary is repaired: {outcome:?}");
        };
        assert_eq!(source.direction, GptRepairDirection::BackupFromPrimary);
    }

    /// A stale backup is repaired too, from the primary. A tool that rewrote only
    /// the primary leaves a backup that passes its own checks and describes a
    /// different table. The primary is authoritative, so the backup is rewritten
    /// from it. The repaired backup carries the primary's partitions, not the stale
    /// backup's.
    #[test]
    fn a_healthy_primary_with_a_stale_backup_repairs_the_backup_from_the_primary() {
        let primary_entries = [gpt_entry(16384, 24575, "uboot")];
        let stale_entries = [gpt_entry(16384, 40959, "uboot_bigger")]; // a different table
        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (backup_header, backup_array) =
            crate::testing::gpt_copy(&stale_entries, backup_lba, backup_array_lba);

        let mut steps = scripted_gpt(1, &primary_entries);
        steps.extend(scripted_read(3, backup_lba, backup_header));
        steps.extend(scripted_read(4, backup_array_lba, sector(&backup_array)));

        let outcome = repair_source_from(steps).expect("answered");
        let GptRepair::Repairable(source) = outcome else {
            panic!("a stale backup is repaired from the primary: {outcome:?}");
        };
        assert_eq!(source.direction, GptRepairDirection::BackupFromPrimary);
        assert_eq!(
            source.partitions[0].name, "uboot",
            "the repaired backup is the primary's table, not the stale one's"
        );
    }

    /// A device with no GPT signature has no primary to repair. That is a different
    /// answer from a damaged primary, and a repair does not invent a table to fix
    /// it.
    #[test]
    fn a_device_with_no_gpt_has_no_primary_to_repair() {
        let steps = scripted_read(1, gpt::HEADER_LBA, vec![0u8; 512]);
        assert_eq!(
            repair_source_from(steps).expect("answered"),
            GptRepair::NoGpt
        );
    }

    /// A damaged primary with no backup cannot be repaired. There is no intact copy
    /// to rebuild from, and a repair does not invent one.
    #[test]
    fn a_damaged_primary_with_no_backup_cannot_be_repaired() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff;

        let backup_lba = a_flash().size_bytes / 512 - 1;
        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, vec![0u8; 512]));

        let outcome = repair_source_from(steps).expect("answered");
        assert!(
            matches!(outcome, GptRepair::Unrepairable { .. }),
            "{outcome:?}"
        );
    }

    /// A damaged primary with a damaged backup cannot be repaired from the device's
    /// own copies. Both copies are bad, and there is nothing intact to copy.
    #[test]
    fn a_damaged_primary_and_a_damaged_backup_cannot_be_repaired() {
        let entries = [gpt_entry(16384, 24575, "uboot")];
        let (mut primary, _) = crate::testing::gpt_table(&entries);
        primary[0x28] ^= 0xff;

        let flash_sectors = a_flash().size_bytes / 512;
        let backup_lba = flash_sectors - 1;
        let backup_array_lba = backup_lba - 1;
        let (mut backup_header, _) =
            crate::testing::gpt_copy(&entries, backup_lba, backup_array_lba);
        backup_header[0x30] ^= 0xff; // the backup header CRC breaks too

        let mut steps = scripted_read(1, gpt::HEADER_LBA, primary);
        steps.extend(scripted_read(2, backup_lba, backup_header));

        let outcome = repair_source_from(steps).expect("answered");
        assert!(
            matches!(outcome, GptRepair::Unrepairable { .. }),
            "{outcome:?}"
        );
    }

    /// Script the parameter scan: a read of every sector a parameter is kept in, in
    /// [`rkparam::LOCATIONS`] order. Each `lba` given a block gets a second read of
    /// the block itself. A location with no block reads back blank.
    ///
    /// [`read_param_repair_source`] looks in every copy, not only until the first
    /// good one. The script therefore drives the whole scan, and `assert_drained`
    /// catches a read too many or too few.
    fn scripted_param_scan(blocks: &[(u64, Option<Vec<u8>>)]) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut tag = 1u32;
        for location in rkparam::LOCATIONS {
            let block = blocks
                .iter()
                .find(|(lba, _)| *lba == location.lba)
                .and_then(|(_, block)| block.clone());
            match block {
                Some(block) => {
                    // The probe (one sector), then the full block read.
                    steps.extend(scripted_read(tag, location.lba, sector(&block)));
                    steps.extend(scripted_read(tag + 1, location.lba, sector(&block)));
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

    /// Read the parameter repair source from a scripted device, and drain the whole
    /// script.
    fn param_repair_from(steps: Vec<Step>) -> Result<ParamRepair> {
        let mut agent = agent(steps);
        let outcome = pollster::block_on(read_param_repair_source(&mut agent, &a_flash()));

        let FlashAgent::Rockusb(agent) = &agent else {
            unreachable!("the test built a Rockusb agent")
        };
        agent.transport().assert_drained();
        outcome
    }

    /// A good NAND parameter copy, and the same copy with a byte of its text
    /// flipped so that it fails its CRC.
    fn a_nand_copy() -> (Vec<u8>, Vec<u8>) {
        let good = param_block("CMDLINE: mtdparts=rk29xxnand:0x100@0x200(boot)\n");
        let mut damaged = good.clone();
        damaged[rkparam::HEADER_LEN + 1] ^= 0xff;
        (good, damaged)
    }

    /// The repairable case on NAND. rkflashtool writes the parameter several times.
    /// A damaged copy with an intact sibling counting from the same base is
    /// therefore repaired. The repair writes the intact block to the damaged copy's
    /// sector.
    #[test]
    fn a_damaged_nand_copy_is_repairable_from_an_intact_sibling() {
        let (good, damaged) = a_nand_copy();

        // 0x2000 (the eMMC slot) blank; an intact copy at 0; a damaged copy at
        // 0x400; the rest blank.
        let outcome = param_repair_from(scripted_param_scan(&[
            (0x0000, Some(good.clone())),
            (0x0400, Some(damaged)),
        ]))
        .expect("the device answered every read");

        let ParamRepair::Repairable(source) = outcome else {
            panic!("a damaged NAND copy over an intact sibling is repairable: {outcome:?}");
        };
        assert_eq!(source.base_lba, 0, "the copies count from the NAND base");
        assert_eq!(
            source.damaged_lbas,
            [0x0400],
            "the damaged copy is rewritten"
        );
        assert_eq!(source.partitions.len(), 1);
        assert_eq!(source.partitions[0].name, "boot");
        // The block written is the intact copy's, trimmed to its own length.
        assert_eq!(source.block, &good[..source.block.len()]);
        assert!(rkparam::has_magic(&source.block));
    }

    /// A single eMMC copy has no sibling. rkdeveloptool writes exactly one parameter
    /// copy on an eMMC, so a damaged one cannot be repaired from the device's own
    /// copies. That is the same finding as a GPT with no backup. Authoring fixes
    /// it.
    #[test]
    fn a_damaged_lone_emmc_copy_is_unrepairable() {
        let (_good, damaged) = a_nand_copy();

        // The one copy an eMMC has, at the eMMC base, damaged; nothing behind it.
        let outcome = param_repair_from(scripted_param_scan(&[(
            rkparam::EMMC_BASE_LBA,
            Some(damaged),
        )]))
        .expect("answered");

        assert!(
            matches!(outcome, ParamRepair::Unrepairable { .. }),
            "{outcome:?}"
        );
    }

    /// Every copy that answered passes its checks, so there is nothing to repair,
    /// even with only one copy present.
    #[test]
    fn a_parameter_with_no_damaged_copy_has_nothing_to_repair() {
        let (good, _damaged) = a_nand_copy();

        let outcome =
            param_repair_from(scripted_param_scan(&[(0x0000, Some(good))])).expect("answered");

        assert_eq!(outcome, ParamRepair::Healthy);
    }

    /// No copy carries the magic, so there is no parameter table to repair. That is
    /// a different answer from a damaged table, and a repair does not invent a table
    /// to fix it.
    #[test]
    fn no_parameter_copy_anywhere_is_no_table() {
        let outcome = param_repair_from(scripted_param_scan(&[])).expect("answered");
        assert_eq!(outcome, ParamRepair::NoTable);
    }

    /// A table shaped like a Rockchip board's: a bootloader region outside any
    /// partition, and then partitions flush against one another.
    fn a_table() -> PartitionTable {
        PartitionTable {
            format: TableFormat::Gpt,
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
                Partition {
                    name: "boot".to_string(),
                    first_lba: 32768,
                    sectors: 229376,
                },
            ],
            recovery: None,
        }
    }

    #[test]
    fn a_partition_is_found_by_the_name_its_table_gives_it() {
        let table = a_table();
        let boot = table.find("boot").expect("the table has one");

        assert_eq!(boot.first_lba, 32768);
        assert_eq!(boot.sectors, 229376);
        assert_eq!(boot.end_lba(), 262144);
    }

    /// Names match exactly. A tool that resolved `Boot` to `boot` would also
    /// resolve `boot` to `boot_a` on a board that had both. A wrong guess here can
    /// cost a board. The error lists the names, so the refusal costs a person
    /// only a retype.
    #[test]
    fn a_name_that_is_nearly_right_is_refused_and_the_real_names_are_offered() {
        let table = a_table();

        let error = table.find("Boot").expect_err("the table says 'boot'");
        let Error::NoSuchPartition { wanted, available } = error else {
            panic!("a name that names nothing: {error:?}");
        };
        assert_eq!(wanted, "Boot");
        assert_eq!(available, ["uboot", "trust", "boot"]);

        // And the error renders the way out.
        let rendered = Error::NoSuchPartition {
            wanted: "Boot".to_string(),
            available: table.names(),
        }
        .to_string();
        assert!(rendered.contains("uboot, trust, boot"), "{rendered}");
    }

    /// GPT permits two partitions with one name, and picking the first of them
    /// would pick the right one half the time. On the way to a write, `find`
    /// refuses rather than guess.
    #[test]
    fn a_name_that_matches_two_partitions_is_refused_rather_than_guessed_at() {
        let mut table = a_table();
        table.partitions.push(Partition {
            name: "boot".to_string(),
            first_lba: 262144,
            sectors: 229376,
        });

        let error = table.find("boot").expect_err("there are two of them");
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }

    /// A write to sector 16384 of an 8192-sector image covers the whole of `uboot`.
    /// A person who meant to write `uboot` expects to read that, and a person who
    /// meant `boot` needs to see it.
    #[test]
    fn a_write_that_fills_one_partition_reports_it_whole() {
        let overlaps = a_table().overlaps(16384, 8192);

        assert_eq!(overlaps.len(), 1);
        assert_eq!(overlaps[0].name, "uboot");
        assert_eq!(overlaps[0].covered, 8192);
        assert_eq!(overlaps[0].total, 8192);
        assert!(overlaps[0].is_whole());
    }

    /// An image too big for its target partition runs off the end of it and into
    /// the next one. The device would not refuse this, because both are good
    /// sectors. Only the table can say that the end of the write lands in
    /// `trust`.
    #[test]
    fn a_write_that_runs_off_the_end_of_a_partition_reports_the_one_behind_it() {
        // 8192 sectors of `uboot`, and 100 more.
        let overlaps = a_table().overlaps(16384, 8292);

        assert_eq!(overlaps.len(), 2);

        assert_eq!(overlaps[0].name, "uboot");
        assert!(overlaps[0].is_whole(), "the whole of it");

        assert_eq!(overlaps[1].name, "trust");
        assert_eq!(overlaps[1].covered, 100);
        assert_eq!(overlaps[1].total, 8192);
        assert!(!overlaps[1].is_whole(), "and the front of the next");
    }

    /// A partition the write only abuts is not a partition the write touches. An
    /// off-by-one here would report every write as landing in the partition after
    /// it. A person who sees that warning on every write learns to ignore it.
    #[test]
    fn a_write_ending_exactly_where_a_partition_begins_does_not_touch_it() {
        let overlaps = a_table().overlaps(16384, 8192);
        assert_eq!(overlaps.len(), 1, "it ends on trust's first sector");

        // One sector more, and it does.
        let overlaps = a_table().overlaps(16384, 8193);
        assert_eq!(overlaps.len(), 2);
        assert_eq!(overlaps[1].covered, 1);
    }

    /// The bootloader region on a Rockchip board is outside every partition, so a
    /// write there overlaps nothing. An empty answer means "in no partition", which
    /// is either exactly what was meant or a write into a gap. It does not mean
    /// "safe", and the caller does not render it as safe.
    #[test]
    fn a_write_outside_every_partition_overlaps_nothing() {
        assert!(a_table().overlaps(64, 8192).is_empty());
    }

    /// The overlaps are ordered by their position on the flash, not by the order
    /// the table lists them in. A person then reads "this covers the end of `uboot`
    /// and the start of `trust`" in the device's own order.
    #[test]
    fn overlaps_come_back_in_the_order_they_sit_on_the_flash() {
        let table = PartitionTable {
            format: TableFormat::Gpt,
            partitions: vec![
                Partition {
                    name: "last".to_string(),
                    first_lba: 2048,
                    sectors: 1024,
                },
                Partition {
                    name: "first".to_string(),
                    first_lba: 1024,
                    sectors: 1024,
                },
            ],
            recovery: None,
        };

        let overlaps = table.overlaps(0, 4096);
        assert_eq!(overlaps[0].name, "first");
        assert_eq!(overlaps[1].name, "last");
    }

    /// A whole-device write covers every partition, whole. A `clone` makes that
    /// write, and its plan lists every partition rather than reporting nothing for
    /// the largest write there is.
    #[test]
    fn a_write_over_the_whole_device_covers_every_partition() {
        let overlaps = a_table().overlaps(0, u64::MAX);

        assert_eq!(overlaps.len(), 3);
        assert!(
            overlaps.iter().all(Overlap::is_whole),
            "all of all of them: {overlaps:?}"
        );
    }
}
