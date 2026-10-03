//! The Rockchip rockusb command layer.
//!
//! A rockusb command travels in a USB Mass-Storage Bulk-Only Transport envelope,
//! which [`bot`](super::bot) frames. The command block a CBW carries is laid out
//! like a SCSI 10-byte CDB: an opcode, a big-endian address, and a big-endian
//! transfer length. The `_10` in the LBA opcode names refers to that layout.
//!
//! The two layers differ in byte order. The CBW's own fields are little-endian,
//! and the address and length in the command block are **big-endian**.
//!
//! The module is sans-I/O framing and touches no transport.

use crate::{Error, Result};

/// The size of a rockusb logical sector, in bytes.
///
/// Every LBA opcode addresses and counts in these, whatever page or block size the
/// medium itself uses.
pub const SECTOR_SIZE: u32 = 512;

/// A rockusb `K_FW_*` operation code.
///
/// Each value is corroborated before it is added. The values agree across U-Boot's
/// portable `rockusb` gadget, which speaks the same wire protocol as a Rockchip
/// loader, and the reference tools. **\[DOC\]**
///
/// The two LBA opcodes are consecutive: read is `0x14`, and write is `0x15`. They
/// take the same 10-byte command block. Only the opcode byte and the direction of
/// the data phase differ, so one byte separates a read from a destructive write.
///
/// The erase opcode is deliberately absent. Rockchip's rkdeveloptool names LBA erase
/// `ERASE_LBA`, `0x25`, and U-Boot's `f_rockusb.h` gives the same value as
/// `K_FW_LBA_ERASE_10`. Both name `0x16` as a different command, the system-disk
/// erase. **\[DOC\]**
///
/// rkflashtool issues that `0x16` for its erase. **\[COMMUNITY\]**
///
/// The opcode is added to this enum, and sent, only once a board has confirmed what
/// the command does to a range. An erase with the wrong range semantics destroys the
/// data it was aimed at, and is not reported as a failed command.
///
/// An opcode in this enum is one pyrographer sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    /// `K_FW_TEST_UNIT_READY`: check that a loader is running and answering.
    TestUnitReady = 0x00,
    /// `K_FW_READ_FLASH_ID`: read the flash chip's identifier.
    ReadFlashId = 0x01,
    /// `K_FW_LBA_READ_10`: read sectors.
    LbaRead = 0x14,
    /// `K_FW_LBA_WRITE_10`: write sectors.
    LbaWrite = 0x15,
    /// `K_FW_READ_FLASH_INFO`: read flash geometry.
    ReadFlashInfo = 0x1a,
    /// `K_FW_GET_CHIP_VER`: ask the loader what SoC it is running on.
    GetChipVer = 0x1b,
    /// `K_FW_GET_STORAGE_MEDIA`: ask which medium the loader is addressing.
    GetStorageMedia = 0x2b,
    /// `K_FW_READ_CAPABILITY`: ask the loader what it says it can do.
    ReadCapability = 0xaa,
    /// `K_FW_RESET`: reboot the device.
    Reset = 0xff,
}

impl Opcode {
    /// The command's name, for an error message a person reads.
    pub fn name(self) -> &'static str {
        match self {
            Opcode::TestUnitReady => "test unit ready",
            Opcode::ReadFlashId => "read flash ID",
            Opcode::LbaRead => "LBA read",
            Opcode::LbaWrite => "LBA write",
            Opcode::ReadFlashInfo => "read flash info",
            Opcode::GetChipVer => "get chip version",
            Opcode::GetStorageMedia => "get storage media",
            Opcode::ReadCapability => "read capability",
            Opcode::Reset => "reset",
        }
    }

    /// The `bCBWCBLength` declared to a Rockchip loader for this command block.
    ///
    /// It is ten for the commands that carry an LBA and a sector count. That is the
    /// SCSI 10-byte CDB the `_10` in their names refers to. It is six for the ones
    /// that carry neither.
    ///
    /// The bytes on the wire do not depend on this. The CBW's command-block region
    /// is a fixed sixteen bytes either way, and the tail is zero. Only the declared
    /// length changes. The value matches what the reference tools declare, in case a
    /// loader checks it. **\[COMMUNITY\]**
    pub fn cdb_len(self) -> usize {
        match self {
            Opcode::LbaRead | Opcode::LbaWrite => 10,
            Opcode::TestUnitReady
            | Opcode::ReadFlashId
            | Opcode::ReadFlashInfo
            | Opcode::GetChipVer
            | Opcode::GetStorageMedia
            | Opcode::ReadCapability
            | Opcode::Reset => 6,
        }
    }
}

/// What [`Opcode::Reset`] asks the device to do: reboot, reboot into another mode,
/// or power off.
///
/// `K_FW_RESET` selects a mode, not only a reboot. The command block's second byte
/// chooses what the device does once it has acknowledged. Every other command
/// pyrographer issues leaves that subcode at zero.
///
/// Only [`Reset`](Self::Reset) has run on hardware. The other three are the
/// reference tools' values, which agree across those tools and have not been tried
/// on a board. Each variant carries its own tag, and a front-end asks
/// [`untried`](Self::untried) instead of deciding for itself. **\[COMMUNITY\]**
///
/// None of them writes flash, so none passes through the write gate. Each one ends
/// the connection and leaves the board in a state pyrographer cannot follow. The
/// front-ends therefore say where the board goes before sending one.
///
/// This enum carries only the subcodes pyrographer sends. Subcode `4` (disconnect
/// from USB) is not one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ResetMode {
    /// Subcode `0`: reboot, into whatever the board boots by default.
    ///
    /// It is the only mode a board has answered, and the default.
    #[default]
    Reset = 0,
    /// Subcode `1`: reboot into the USB mass-storage class.
    ///
    /// The board reappears as a block device the host operating system owns, outside
    /// the rockusb path. The next verb then has no loader to send to, and the host
    /// operating system, not pyrographer, controls the medium. **\[COMMUNITY\]**
    MassStorage = 1,
    /// Subcode `2`: power off instead of rebooting. **\[COMMUNITY\]**
    PowerOff = 2,
    /// Subcode `3`: reboot into maskrom.
    ///
    /// It returns the board by software to the mode
    /// [`download_boot`](crate::verbs::download_boot) uploads a loader into.
    /// Without it, a board reaches maskrom by a shorted pin or a held button.
    /// **\[COMMUNITY\]**
    Maskrom = 3,
}

impl ResetMode {
    /// Every mode, in subcode order, for a front-end that offers the choice.
    pub const ALL: [ResetMode; 4] = [
        ResetMode::Reset,
        ResetMode::MassStorage,
        ResetMode::PowerOff,
        ResetMode::Maskrom,
    ];

    /// The name a caller spells to select this mode.
    pub fn name(self) -> &'static str {
        match self {
            ResetMode::Reset => "reset",
            ResetMode::MassStorage => "msc",
            ResetMode::PowerOff => "poweroff",
            ResetMode::Maskrom => "maskrom",
        }
    }

    /// What the device is being asked to do, as a label on a control.
    pub fn describe(self) -> &'static str {
        match self {
            ResetMode::Reset => "Reboot",
            ResetMode::MassStorage => "Reboot into USB mass storage",
            ResetMode::PowerOff => "Power off",
            ResetMode::Maskrom => "Reboot into maskrom",
        }
    }

    /// What the board is doing, in the sentence both front-ends report.
    ///
    /// Both front-ends print this sentence rather than writing their own, as they do
    /// with [`plan_refusal`](crate::verbs::plan_refusal). A reset is therefore
    /// described in the same words wherever it is sent.
    pub fn outcome(self) -> &'static str {
        match self {
            ResetMode::Reset => "The board is rebooting.",
            ResetMode::MassStorage => {
                "The board is rebooting into USB mass storage. It then appears to the host \
                 operating system as a block device, not a rockusb device. pyrographer cannot \
                 reach it over rockusb until it is reset again."
            }
            ResetMode::PowerOff => {
                "The board is powering off. Nothing answers until it is powered back on."
            }
            ResetMode::Maskrom => {
                "The board is rebooting into maskrom, where no loader runs. Upload a loader to \
                 return it to loader mode."
            }
        }
    }

    /// Whether this mode is corroborated but unpinned, meaning no board has answered
    /// it.
    ///
    /// A front-end asks this method instead of deciding for itself, so the caution it
    /// shows matches the tag recorded on the variant.
    pub fn untried(self) -> bool {
        !matches!(self, ResetMode::Reset)
    }

    /// Parse a mode by [`name`](Self::name).
    ///
    /// Matching ignores case and surrounding whitespace. An unknown name returns
    /// [`Error::InvalidRequest`] listing every valid name, in the same form as a
    /// refusal from [`Soc::parse`](crate::soc::Soc::parse).
    pub fn parse(name: &str) -> Result<ResetMode> {
        let trimmed = name.trim().to_ascii_lowercase();
        ResetMode::ALL
            .iter()
            .copied()
            .find(|mode| mode.name() == trimmed)
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "'{name}' is not a reset mode. The reset modes are: {}",
                    ResetMode::ALL
                        .iter()
                        .map(|mode| mode.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }
}

/// The length of a rockusb command block, in bytes.
pub const CDB_LEN: usize = 16;

/// Build a rockusb command block, to be carried in a [`bot`](super::bot) CBW.
///
/// The layout follows a SCSI 10-byte CDB:
///
/// | Offset | Size | Field                                          |
/// |--------|------|------------------------------------------------|
/// | 0      | 1    | opcode                                         |
/// | 1      | 1    | subcode, zero for every command but [`Opcode::Reset`] |
/// | 2      | 4    | address, **big-endian**, an LBA for the LBA opcodes |
/// | 6      | 1    | reserved                                       |
/// | 7      | 2    | length in sectors, **big-endian**              |
/// | 9      | 7    | reserved                                       |
///
/// Commands with no address or length, such as [`Opcode::ReadFlashInfo`], pass
/// zero for both, and the device ignores the fields.
pub fn build_cdb(opcode: Opcode, address: u32, sectors: u16) -> [u8; CDB_LEN] {
    build_cdb_with_subcode(opcode, 0, address, sectors)
}

/// Build a rockusb command block carrying a subcode in its second byte.
///
/// [`Opcode::Reset`] is the one command pyrographer sends with a non-zero subcode.
/// The subcode is the [`ResetMode`], and that one byte selects between rebooting a
/// board and powering it off. Every other command goes through [`build_cdb`],
/// which calls this with a subcode of zero.
pub fn build_cdb_with_subcode(
    opcode: Opcode,
    subcode: u8,
    address: u32,
    sectors: u16,
) -> [u8; CDB_LEN] {
    let mut cdb = [0u8; CDB_LEN];
    cdb[0] = opcode as u8;
    cdb[1] = subcode;
    cdb[2..6].copy_from_slice(&address.to_be_bytes());
    cdb[7..9].copy_from_slice(&sectors.to_be_bytes());
    cdb
}

/// The length of the payload a [`Opcode::ReadFlashId`] command returns.
pub const FLASH_ID_LEN: usize = 5;

/// The length of the payload a [`Opcode::ReadFlashInfo`] command returns.
pub const FLASH_INFO_LEN: usize = 11;

/// How many bytes a [`Opcode::GetChipVer`] command asks the loader for.
///
/// The reference tools ask for sixteen. The length is a request, and the agent
/// accepts a shorter reply. It hands the bytes back uninterpreted. A loader that
/// answers with fewer bytes has reported its true reply length, and the agent keeps
/// that length as measured. **\[COMMUNITY\]**
///
/// There is deliberately no `parse_chip_version`. An RK3576 answers with its ASCII
/// digits byte-reversed, then twelve zeros, pinned in [`soc`](crate::soc). The
/// wrong-loader gate, a precondition of every write, compares the whole reply
/// against pinned bytes and never a decoded field. A parser would add nothing to
/// that comparison, so the reply stays raw bytes.
pub const CHIP_VER_LEN: usize = 16;

/// The length of the payload a [`Opcode::ReadCapability`] command returns.
pub const CAPABILITY_LEN: usize = 8;

/// What a loader says it can do, as [`Opcode::ReadCapability`] reports it.
///
/// The reply is eight bytes of flags, and is the running loader's claim about
/// itself. [`Caps`](crate::agent::Caps) is a different claim: what a pyrographer
/// backend implements. A loader that does not set [`read_lba`](Self::read_lba)
/// does not serve the read path, whatever the backend implements.
///
/// The flag names and bit positions come from the reference tools. No board has
/// answered this command yet, so what a given loader sets is unpinned.
/// **\[COMMUNITY\]**
///
/// Bits outside the named table are kept, and [`unnamed_bits`](Self::unnamed_bits)
/// returns them. A flag this table does not name still reaches the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    raw: [u8; CAPABILITY_LEN],
}

impl Capability {
    /// Byte index and mask of every flag this table names.
    ///
    /// [`unnamed_bits`](Self::unnamed_bits) clears these pairs from the reply. Each
    /// accessor carries its own copy of its pair. Two tests keep the copies in step.
    /// One sets each flag alone and reads it back. The other sets every named bit
    /// and expects no unnamed bits.
    const NAMED: [(usize, u8); 9] = [
        (0, 0x01),
        (0, 0x02),
        (0, 0x04),
        (0, 0x08),
        (0, 0x20),
        (0, 0x40),
        (0, 0x80),
        (1, 0x01),
        (1, 0x02),
    ];

    fn flag(&self, byte: usize, mask: u8) -> bool {
        self.raw[byte] & mask == mask
    }

    /// The loader addresses flash by logical block directly.
    pub fn direct_lba(&self) -> bool {
        self.flag(0, 0x01)
    }

    /// The loader exposes Rockchip's vendor storage area.
    pub fn vendor_storage(&self) -> bool {
        self.flag(0, 0x02)
    }

    /// The loader permits access to the first 4 MiB of the medium.
    pub fn first_4m_access(&self) -> bool {
        self.flag(0, 0x04)
    }

    /// The loader serves [`Opcode::LbaRead`].
    pub fn read_lba(&self) -> bool {
        self.flag(0, 0x08)
    }

    /// The loader can hand back its serial-console log.
    pub fn read_com_log(&self) -> bool {
        self.flag(0, 0x20)
    }

    /// The loader can hand back the ID-block configuration.
    pub fn read_idb_config(&self) -> bool {
        self.flag(0, 0x40)
    }

    /// The loader can report whether the part is in secure mode.
    pub fn read_secure_mode(&self) -> bool {
        self.flag(0, 0x80)
    }

    /// The loader uses the newer ID-block layout.
    pub fn new_idb(&self) -> bool {
        self.flag(1, 0x01)
    }

    /// The loader serves [`Opcode::GetStorageMedia`]'s write counterpart.
    pub fn switch_storage(&self) -> bool {
        self.flag(1, 0x02)
    }

    /// The reply exactly as it arrived.
    pub fn raw(&self) -> &[u8; CAPABILITY_LEN] {
        &self.raw
    }

    /// Bits the loader set that this table has no name for.
    ///
    /// All zero for a reply this table fully names. A non-zero result is a flag this
    /// table does not name, for the caller to report.
    pub fn unnamed_bits(&self) -> [u8; CAPABILITY_LEN] {
        let mut rest = self.raw;
        for (byte, mask) in Self::NAMED {
            rest[byte] &= !mask;
        }
        rest
    }
}

/// Parse the payload of a [`Opcode::ReadCapability`] command.
pub fn parse_capability(bytes: &[u8]) -> Result<Capability> {
    let raw: [u8; CAPABILITY_LEN] = bytes.try_into().map_err(|_| {
        Error::Protocol(format!(
            "capability is {} bytes, expected {CAPABILITY_LEN}",
            bytes.len()
        ))
    })?;
    Ok(Capability { raw })
}

/// The length of the payload a [`Opcode::GetStorageMedia`] command returns.
pub const STORAGE_MEDIA_LEN: usize = 4;

/// The storage medium a loader is currently addressing.
///
/// rockusb addresses one medium at a time, and every LBA it takes is an offset into
/// that medium. On a board with more than one populated, such as an eMMC and a SPI
/// NOR, the same sector number names two different places. An LBA is meaningful
/// only together with the medium this reports.
///
/// The indices are U-Boot's `BOOT_TYPE_*` bit positions, taken from the reference
/// tools. **\[COMMUNITY\]**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMedium {
    /// Raw NAND.
    Nand,
    /// eMMC.
    Emmc,
    /// The first SD/MMC controller.
    Sd0,
    /// The second SD/MMC controller.
    Sd1,
    /// SPI NOR flash.
    SpiNor,
    /// SPI NAND flash.
    SpiNand,
    /// RAM, which is what a loader running from DRAM reports.
    Ram,
    /// Raw NAND behind the MTD block layer.
    MtdBlkNand,
    /// SPI NAND behind the MTD block layer.
    MtdBlkSpiNand,
    /// SPI NOR behind the MTD block layer.
    MtdBlkSpiNor,
    /// SATA.
    Sata,
    /// PCIe.
    Pcie,
    /// UFS.
    Ufs,
    /// An index this table has no name for.
    ///
    /// The loader is still addressing a medium, so the reply is kept with its index
    /// instead of being refused.
    Unknown(u8),
}

impl StorageMedium {
    /// The medium for a `BOOT_TYPE_*` bit position.
    pub fn from_index(index: u8) -> Self {
        match index {
            0 => StorageMedium::Nand,
            1 => StorageMedium::Emmc,
            2 => StorageMedium::Sd0,
            3 => StorageMedium::Sd1,
            4 => StorageMedium::SpiNor,
            5 => StorageMedium::SpiNand,
            6 => StorageMedium::Ram,
            7 => StorageMedium::MtdBlkNand,
            8 => StorageMedium::MtdBlkSpiNand,
            9 => StorageMedium::MtdBlkSpiNor,
            10 => StorageMedium::Sata,
            11 => StorageMedium::Pcie,
            12 => StorageMedium::Ufs,
            other => StorageMedium::Unknown(other),
        }
    }

    /// The medium's name, for a person reading it.
    pub fn name(self) -> &'static str {
        match self {
            StorageMedium::Nand => "NAND",
            StorageMedium::Emmc => "eMMC",
            StorageMedium::Sd0 => "SD0",
            StorageMedium::Sd1 => "SD1",
            StorageMedium::SpiNor => "SPI NOR",
            StorageMedium::SpiNand => "SPI NAND",
            StorageMedium::Ram => "RAM",
            StorageMedium::MtdBlkNand => "MTD block NAND",
            StorageMedium::MtdBlkSpiNand => "MTD block SPI NAND",
            StorageMedium::MtdBlkSpiNor => "MTD block SPI NOR",
            StorageMedium::Sata => "SATA",
            StorageMedium::Pcie => "PCIe",
            StorageMedium::Ufs => "UFS",
            StorageMedium::Unknown(_) => "unrecognized",
        }
    }
}

/// Parse the payload of a [`Opcode::GetStorageMedia`] command.
///
/// The reply is a little-endian `u32` with exactly one bit set, and the bit's
/// position is the medium. A reply with no bit set, or several, returns
/// [`Error::Protocol`] and is not resolved to the lowest bit. rockusb addresses one
/// medium at a time, so a reply that names two does not say which medium an LBA
/// addresses.
pub fn parse_storage_media(bytes: &[u8]) -> Result<StorageMedium> {
    let raw: [u8; STORAGE_MEDIA_LEN] = bytes.try_into().map_err(|_| {
        Error::Protocol(format!(
            "storage media is {} bytes, expected {STORAGE_MEDIA_LEN}",
            bytes.len()
        ))
    })?;
    let mask = u32::from_le_bytes(raw);
    if mask.count_ones() != 1 {
        return Err(Error::Protocol(format!(
            "storage media is {mask:#010x}, which names {} media. The reply is one-hot and \
             addresses exactly one",
            mask.count_ones()
        )));
    }
    Ok(StorageMedium::from_index(mask.trailing_zeros() as u8))
}

/// Flash geometry, as rockusb reports it.
///
/// The payload of a [`Opcode::ReadFlashInfo`] command: eleven packed
/// little-endian bytes. Several fields describe raw NAND and read as zero on an
/// eMMC, which is what most Rockchip boards boot from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashInfo {
    /// Total size, in [`SECTOR_SIZE`]-byte sectors.
    pub sector_count: u32,
    /// Erase-block size, in sectors.
    pub block_size_sectors: u16,
    /// Page size, in sectors.
    pub page_size_sectors: u8,
    /// ECC strength, in bits.
    pub ecc_bits: u8,
    /// Access time.
    pub access_time: u8,
    /// Manufacturer code, which [`FlashInfo::manufacturer_name`] names.
    pub manufacturer: u8,
    /// Chip-select mask: one bit per populated flash chip select.
    pub chip_select_mask: u8,
}

impl FlashInfo {
    /// Total size in bytes.
    pub fn size_bytes(&self) -> u64 {
        u64::from(self.sector_count) * u64::from(SECTOR_SIZE)
    }

    /// The manufacturer's name, or `None` for a code outside the eight this table
    /// covers.
    ///
    /// This is the raw-NAND manufacturer table, and is meaningful only on a NAND
    /// part. On an eMMC the field reads zero, which this table maps to Samsung. The
    /// table comes from the reference tools, not a specification. **\[COMMUNITY\]**
    pub fn manufacturer_name(&self) -> Option<&'static str> {
        Some(match self.manufacturer {
            0 => "Samsung",
            1 => "Toshiba",
            2 => "Hynix",
            3 => "Infineon",
            4 => "Micron",
            5 => "Renesas",
            6 => "Intel",
            7 => "SanDisk",
            _ => return None,
        })
    }
}

/// Parse the payload of a [`Opcode::ReadFlashInfo`] command.
pub fn parse_flash_info(bytes: &[u8]) -> Result<FlashInfo> {
    if bytes.len() != FLASH_INFO_LEN {
        return Err(Error::Protocol(format!(
            "flash info is {} bytes, expected {FLASH_INFO_LEN}",
            bytes.len()
        )));
    }

    Ok(FlashInfo {
        sector_count: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        block_size_sectors: u16::from_le_bytes([bytes[4], bytes[5]]),
        page_size_sectors: bytes[6],
        ecc_bits: bytes[7],
        access_time: bytes[8],
        manufacturer: bytes[9],
        chip_select_mask: bytes[10],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each subcode is spelled out as a literal byte, not derived from the enum. A
    /// test that wrote `ResetMode::PowerOff as u8` on both sides would pin nothing.
    /// The four values are the reference tools', and pyrographer must not renumber
    /// them.
    #[test]
    fn each_reset_mode_is_the_subcode_the_reference_tools_send() {
        assert_eq!(ResetMode::Reset as u8, 0);
        assert_eq!(ResetMode::MassStorage as u8, 1);
        assert_eq!(ResetMode::PowerOff as u8, 2);
        assert_eq!(ResetMode::Maskrom as u8, 3);
    }

    /// The whole command block for every mode, byte for byte. The opcode is
    /// `0xff`, and the subcode is the only byte that differs between rebooting a
    /// board and powering it off. This test pins that byte.
    #[test]
    fn a_reset_command_block_differs_only_in_its_subcode() {
        for (mode, subcode) in [
            (ResetMode::Reset, 0x00),
            (ResetMode::MassStorage, 0x01),
            (ResetMode::PowerOff, 0x02),
            (ResetMode::Maskrom, 0x03),
        ] {
            let cdb = build_cdb_with_subcode(Opcode::Reset, mode as u8, 0, 0);
            let mut expected = [0u8; CDB_LEN];
            expected[0] = 0xff;
            expected[1] = subcode;
            assert_eq!(cdb, expected, "{}", mode.name());
        }
    }

    /// The subcode is written to byte 1 and leaves the address and length
    /// unchanged. A reset leaves those fields at zero, but a future subcoded
    /// command would not.
    #[test]
    fn a_subcode_leaves_the_rest_of_the_block_alone() {
        let plain = build_cdb(Opcode::LbaRead, 0x0102_0304, 0x0020);
        let subcoded = build_cdb_with_subcode(Opcode::LbaRead, 0x07, 0x0102_0304, 0x0020);
        assert_eq!(subcoded[1], 0x07);
        assert_eq!(plain[..1], subcoded[..1]);
        assert_eq!(plain[2..], subcoded[2..]);
    }

    /// `build_cdb` is `build_cdb_with_subcode` with a subcode of zero, and every
    /// command that is not a reset goes through it.
    #[test]
    fn build_cdb_sends_no_subcode() {
        for opcode in [
            Opcode::TestUnitReady,
            Opcode::ReadFlashId,
            Opcode::LbaRead,
            Opcode::LbaWrite,
            Opcode::ReadFlashInfo,
            Opcode::GetChipVer,
            Opcode::GetStorageMedia,
            Opcode::ReadCapability,
            Opcode::Reset,
        ] {
            assert_eq!(build_cdb(opcode, 1, 2)[1], 0x00, "{}", opcode.name());
            assert_eq!(
                build_cdb(opcode, 1, 2),
                build_cdb_with_subcode(opcode, 0, 1, 2),
                "{}",
                opcode.name()
            );
        }
    }

    /// A reset with no mode named is the plain reboot, subcode `0`, which is the
    /// one mode a board has answered.
    #[test]
    fn the_default_mode_is_a_plain_reboot() {
        assert_eq!(ResetMode::default(), ResetMode::Reset);
        assert_eq!(ResetMode::default() as u8, 0);
        assert!(!ResetMode::Reset.untried());
    }

    /// Only the plain reboot has met a board. The other three are corroborated
    /// and unpinned, and a front-end reads that from `untried` instead of deciding
    /// it again.
    #[test]
    fn every_mode_but_the_plain_reboot_is_untried() {
        for mode in ResetMode::ALL {
            assert_eq!(mode.untried(), mode != ResetMode::Reset, "{}", mode.name());
        }
    }

    #[test]
    fn a_reset_mode_round_trips_through_its_name() {
        for mode in ResetMode::ALL {
            assert_eq!(ResetMode::parse(mode.name()).expect("its own name"), mode);
        }
        assert_eq!(
            ResetMode::parse("  MASKROM ").expect("trimmed, folded"),
            ResetMode::Maskrom
        );
    }

    /// The refusal lists every valid mode, so a caller who gave a wrong name
    /// learns the valid ones from the error.
    #[test]
    fn an_unknown_reset_mode_is_refused_with_the_list() {
        let error = ResetMode::parse("reboot").expect_err("not a mode name");
        let Error::InvalidRequest(message) = error else {
            panic!("a bad mode name is a usage problem");
        };
        for mode in ResetMode::ALL {
            assert!(message.contains(mode.name()), "{message}");
        }
    }

    /// A front-end builds its choice from `ALL`. A mode added to the enum and left
    /// out of `ALL` would be one no GUI could select.
    #[test]
    fn all_holds_every_mode_in_subcode_order() {
        for (index, mode) in ResetMode::ALL.iter().enumerate() {
            assert_eq!(*mode as u8, index as u8);
        }
    }

    #[test]
    fn cdb_puts_the_address_and_length_in_big_endian() {
        // The one detail most likely to be got wrong: the CBW around this block
        // is little-endian, but the block itself is not.
        let cdb = build_cdb(Opcode::LbaRead, 0x0102_0304, 0x0020);
        assert_eq!(cdb[0], 0x14); // K_FW_LBA_READ_10
        assert_eq!(cdb[1], 0x00); // subcode
        assert_eq!(&cdb[2..6], &[0x01, 0x02, 0x03, 0x04]); // address, big-endian
        assert_eq!(cdb[6], 0x00); // reserved
        assert_eq!(&cdb[7..9], &[0x00, 0x20]); // 32 sectors, big-endian
        assert_eq!(&cdb[9..], &[0u8; 7]); // reserved tail
    }

    #[test]
    fn cdb_for_a_command_with_no_operands_is_just_the_opcode() {
        let cdb = build_cdb(Opcode::ReadFlashInfo, 0, 0);
        assert_eq!(cdb[0], 0x1a);
        assert_eq!(&cdb[1..], &[0u8; 15]);
    }

    #[test]
    fn opcodes_have_their_protocol_values() {
        assert_eq!(Opcode::TestUnitReady as u8, 0x00);
        assert_eq!(Opcode::ReadFlashId as u8, 0x01);
        assert_eq!(Opcode::LbaRead as u8, 0x14);
        assert_eq!(Opcode::LbaWrite as u8, 0x15);
        assert_eq!(Opcode::ReadFlashInfo as u8, 0x1a);
        assert_eq!(Opcode::GetChipVer as u8, 0x1b);
        assert_eq!(Opcode::GetStorageMedia as u8, 0x2b);
        assert_eq!(Opcode::ReadCapability as u8, 0xaa);
        assert_eq!(Opcode::Reset as u8, 0xff);
    }

    /// Read and write take the same command block, and only the opcode byte
    /// distinguishes them. A transposed opcode writes where a caller asked to read,
    /// so it has a test of its own.
    #[test]
    fn a_write_command_block_is_a_read_command_block_with_one_byte_changed() {
        let read = build_cdb(Opcode::LbaRead, 0x0102_0304, 64);
        let write = build_cdb(Opcode::LbaWrite, 0x0102_0304, 64);

        assert_eq!(read[0], 0x14);
        assert_eq!(write[0], 0x15);
        assert_eq!(&read[1..], &write[1..]);
        assert_eq!(Opcode::LbaWrite.cdb_len(), 10);
    }

    #[test]
    fn the_declared_cdb_length_matches_what_the_reference_tools_declare() {
        // rkflashtool packs the flags, this length, and the opcode into one
        // word: 0x8000_0601 for read-flash-ID, 0x8000_0a14 for LBA read.
        assert_eq!(Opcode::ReadFlashId.cdb_len(), 6);
        assert_eq!(Opcode::ReadFlashInfo.cdb_len(), 6);
        assert_eq!(Opcode::TestUnitReady.cdb_len(), 6);
        assert_eq!(Opcode::GetChipVer.cdb_len(), 6);
        assert_eq!(Opcode::Reset.cdb_len(), 6);
        assert_eq!(Opcode::LbaRead.cdb_len(), 10);
    }

    /// The chip-version command carries no operands: it asks the loader about
    /// itself, not about a range of flash. A stray address or sector count in
    /// the block would send operands no caller asked for.
    #[test]
    fn the_chip_version_command_block_carries_no_operands() {
        let cdb = build_cdb(Opcode::GetChipVer, 0, 0);
        assert_eq!(cdb[0], 0x1b);
        assert_eq!(&cdb[1..], &[0u8; 15]);
    }

    #[test]
    fn a_declared_length_never_truncates_a_meaningful_byte() {
        // Slicing the block to its declared length must not cut off the address
        // or the sector count, which is the whole risk in declaring less than 16.
        let cdb = build_cdb(Opcode::LbaRead, 0xdead_beef, 0x1234);
        let declared = &cdb[..Opcode::LbaRead.cdb_len()];
        assert_eq!(&declared[2..6], &0xdead_beefu32.to_be_bytes());
        assert_eq!(&declared[7..9], &0x1234u16.to_be_bytes());
        // The bytes past a six-byte command's declared length are zero anyway,
        // so nothing is lost there either.
        let cdb = build_cdb(Opcode::ReadFlashInfo, 0, 0);
        assert_eq!(&cdb[Opcode::ReadFlashInfo.cdb_len()..], &[0u8; 10]);
    }

    #[test]
    fn flash_info_parses_a_scripted_payload() {
        // 0x0D3E_0000 sectors = 222,167,040 sectors = ~106 GiB, an eMMC-sized part.
        let bytes = [
            0x00, 0x00, 0x3e, 0x0d, // sector_count, little-endian
            0x00, 0x04, // block_size_sectors = 1024
            0x04, // page_size_sectors
            0x00, // ecc_bits
            0x28, // access_time = 40
            0x00, // manufacturer
            0x01, // chip_select_mask
        ];
        let info = parse_flash_info(&bytes).unwrap();
        assert_eq!(
            info,
            FlashInfo {
                sector_count: 0x0d3e_0000,
                block_size_sectors: 1024,
                page_size_sectors: 4,
                ecc_bits: 0,
                access_time: 40,
                manufacturer: 0,
                chip_select_mask: 1,
            }
        );
        assert_eq!(info.size_bytes(), 0x0d3e_0000 * 512);
    }

    #[test]
    fn flash_info_rejects_a_wrong_length() {
        assert!(matches!(
            parse_flash_info(&[0u8; 10]),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            parse_flash_info(&[0u8; 12]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn capability_decodes_each_named_flag_on_its_own() {
        // One flag at a time, so a transposed bit shows up as two failures
        // rather than canceling out.
        let only = |byte: usize, mask: u8| {
            let mut raw = [0u8; CAPABILITY_LEN];
            raw[byte] = mask;
            parse_capability(&raw).unwrap()
        };

        assert!(only(0, 0x01).direct_lba());
        assert!(only(0, 0x02).vendor_storage());
        assert!(only(0, 0x04).first_4m_access());
        assert!(only(0, 0x08).read_lba());
        assert!(only(0, 0x20).read_com_log());
        assert!(only(0, 0x40).read_idb_config());
        assert!(only(0, 0x80).read_secure_mode());
        assert!(only(1, 0x01).new_idb());
        assert!(only(1, 0x02).switch_storage());

        // And each flag reads only itself: setting direct-LBA must not light up
        // the read-LBA bit, which is the confusion that would matter most.
        let direct = only(0, 0x01);
        assert!(!direct.read_lba());
        assert!(!direct.switch_storage());
    }

    #[test]
    fn capability_keeps_bits_the_table_cannot_name() {
        // 0x10 in byte 0 and 0x04 in byte 1 are unclaimed by the flag table.
        let raw = [0x18, 0x06, 0, 0, 0, 0, 0, 0];
        let caps = parse_capability(&raw).unwrap();
        assert!(caps.read_lba(), "0x08 is still decoded");
        assert!(caps.switch_storage(), "0x02 in byte 1 is still decoded");
        assert_eq!(caps.unnamed_bits(), [0x10, 0x04, 0, 0, 0, 0, 0, 0]);
        assert_eq!(caps.raw(), &raw, "the reply survives decoding intact");
    }

    #[test]
    fn a_fully_named_capability_has_no_leftover_bits() {
        let caps = parse_capability(&[0xef, 0x03, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(caps.unnamed_bits(), [0u8; CAPABILITY_LEN]);
    }

    #[test]
    fn capability_rejects_a_wrong_length() {
        assert!(matches!(
            parse_capability(&[0u8; 7]),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            parse_capability(&[0u8; 9]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn storage_media_decodes_the_one_hot_bit_position() {
        let at = |index: u32| parse_storage_media(&(1u32 << index).to_le_bytes()).unwrap();
        assert_eq!(at(0), StorageMedium::Nand);
        assert_eq!(at(1), StorageMedium::Emmc);
        assert_eq!(at(4), StorageMedium::SpiNor);
        assert_eq!(at(6), StorageMedium::Ram);
        assert_eq!(at(12), StorageMedium::Ufs);
        // Past the table, and still an answer rather than a failure.
        assert_eq!(at(13), StorageMedium::Unknown(13));
        assert_eq!(at(31), StorageMedium::Unknown(31));
    }

    #[test]
    fn storage_media_is_little_endian() {
        // eMMC is bit 1, so the byte that carries it is the first one. Reading
        // this big-endian would report bit 25 -- an Unknown -- and the mistake
        // would be silent.
        assert_eq!(
            parse_storage_media(&[0x02, 0x00, 0x00, 0x00]).unwrap(),
            StorageMedium::Emmc
        );
    }

    #[test]
    fn storage_media_refuses_a_reply_that_is_not_one_hot() {
        // Two media set: the device has not said which one an LBA would mean.
        assert!(matches!(
            parse_storage_media(&[0x03, 0, 0, 0]),
            Err(Error::Protocol(_))
        ));
        // None set.
        assert!(matches!(
            parse_storage_media(&[0, 0, 0, 0]),
            Err(Error::Protocol(_))
        ));
        assert!(matches!(
            parse_storage_media(&[0, 0, 0]),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn manufacturer_names_cover_the_table_and_stop() {
        let named = |code| {
            parse_flash_info(&[0, 0, 0, 0, 0, 0, 0, 0, 0, code, 0])
                .unwrap()
                .manufacturer_name()
        };
        assert_eq!(named(0), Some("Samsung"));
        assert_eq!(named(7), Some("SanDisk"));
        assert_eq!(named(8), None);
    }
}
