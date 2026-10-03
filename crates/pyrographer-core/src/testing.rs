//! Scripting a Rockchip or Ingenic DFU device, for the tests in this crate's
//! modules and for the crates that depend on this one.
//!
//! [`ScriptedTransport`](crate::transport::testing::ScriptedTransport) asserts the
//! exact bytes of every transfer. A test written against it therefore pins the wire
//! format as well as the control flow. Writing such a script by hand is tedious, so
//! these functions build the conversations: one command, one read, a whole
//! partition table.
//!
//! They are defined here rather than in one module's tests, because three modules
//! use them, and one shared definition keeps the three from drifting apart. The
//! `testing` feature makes them public for the same reason it makes the transport
//! public. The GUI's jobs drive these verbs, and a test of a job without a board
//! must script the board.
//!
//! `agent`'s own tests spell some conversations out byte by byte, deliberately,
//! because those tests pin the wire format. A test that compared a CBW with one
//! built by the same code would assert nothing.

use crate::codec::bot::{self, Direction};
use crate::codec::dfu::{self, Functional};
use crate::codec::rockusb::{self, Opcode};
use crate::codec::{gpt, rkparam};
use crate::transport::testing::Step;

/// The CBW the host must send for one command.
pub fn cbw(
    tag: u32,
    data_len: u32,
    direction: Direction,
    opcode: Opcode,
    address: u32,
    sectors: u16,
) -> Step {
    cbw_with_subcode(tag, data_len, direction, opcode, 0, address, sectors)
}

/// The CBW the host must send for one command carrying a subcode.
///
/// [`Opcode::Reset`] is the only command that carries one. The subcode alone
/// distinguishes rebooting a board from powering it off. A test that scripts a
/// reset therefore names the subcode byte, not the intent.
#[allow(clippy::too_many_arguments)]
pub fn cbw_with_subcode(
    tag: u32,
    data_len: u32,
    direction: Direction,
    opcode: Opcode,
    subcode: u8,
    address: u32,
    sectors: u16,
) -> Step {
    let cdb = rockusb::build_cdb_with_subcode(opcode, subcode, address, sectors);
    Step::ExpectWrite(bot::build_cbw(tag, data_len, direction, &cdb[..opcode.cdb_len()]).to_vec())
}

/// The `ExpectControlOut` for a DFU host-to-device request built by `setup`.
pub fn dfu_out(setup: dfu::Setup, data: Vec<u8>) -> Step {
    Step::ExpectControlOut {
        request_type: setup.request_type,
        request: setup.request,
        value: setup.value,
        index: setup.index,
        data,
    }
}

/// The `ExpectControlIn` for a DFU device-to-host request, answered with `reply`.
pub fn dfu_in(setup: dfu::Setup, reply: Vec<u8>) -> Step {
    Step::ExpectControlIn {
        request_type: setup.request_type,
        request: setup.request,
        value: setup.value,
        index: setup.index,
        length: setup.length,
        reply,
    }
}

/// A 6-byte `GETSTATUS` reply carrying `status` and `state`, with a zero poll
/// timeout so a scripted poll waits no real time.
pub fn dfu_status_bytes(status: dfu::Status, state: dfu::State) -> Vec<u8> {
    dfu::GetStatus {
        status,
        poll_timeout_ms: 0,
        state,
        string_index: 0,
    }
    .to_bytes()
    .to_vec()
}

/// The `GETSTATUS` exchange a scripted device answers with `state` and an `Ok`
/// status.
pub fn dfu_ok(interface: u16, state: dfu::State) -> Step {
    dfu_in(
        dfu::get_status(interface),
        dfu_status_bytes(dfu::Status::Ok, state),
    )
}

/// The standard `SET_INTERFACE` a DFU read or write issues to select an
/// alt-setting.
///
/// It is a standard request, type `0x01` and request `0x0b`, carrying the alt in
/// `wValue` and the interface in `wIndex`.
pub fn dfu_set_interface(interface: u16, alt: u16) -> Step {
    Step::ExpectControlOut {
        request_type: 0x01,
        request: 0x0b,
        value: alt,
        index: interface,
        data: Vec::new(),
    }
}

/// A DFU functional descriptor for a scripted device, with the capability bits
/// spelled out.
///
/// `attributes` decides whether a scripted board can be written, and whether the
/// write can be read back. A test that depends on either names the bits it
/// scripts, instead of inheriting a default. [`dfu_capable`] gives the ordinary
/// full-capability descriptor.
pub fn dfu_functional(transfer_size: u16, attributes: u8) -> Functional {
    Functional {
        attributes,
        detach_timeout_ms: 0,
        transfer_size,
        dfu_version: 0x0110,
    }
}

/// The attribute bits of a device that downloads, uploads, and stays attached
/// through manifestation: a board the write path can both write and read back.
///
/// `bitCanDnload | bitCanUpload | bitManifestTolerant`.
pub const DFU_FULLY_CAPABLE: u8 = 0b0000_0111;

/// The attribute bits of a device that downloads and uploads but **detaches as it
/// manifests**.
///
/// A write through it cannot be read back in the same session.
///
/// `bitCanDnload | bitCanUpload`, without `bitManifestTolerant`.
pub const DFU_DETACHES_ON_COMMIT: u8 = 0b0000_0011;

/// A DFU functional descriptor for a fully capable scripted device.
pub fn dfu_capable(transfer_size: u16) -> Functional {
    dfu_functional(transfer_size, DFU_FULLY_CAPABLE)
}

/// The CSW a device sends to report that a command passed.
pub fn csw_passed(tag: u32) -> Step {
    csw(tag, 0x00)
}

/// The CSW a device sends to report that a command failed.
pub fn csw_failed(tag: u32) -> Step {
    csw(tag, 0x01)
}

/// The CSW a device sends to report that a command passed, carrying `residue`.
///
/// `residue` is the number of announced bytes the device reports it did not
/// transfer. [`csw_passed`] reports zero, as a device that completed the transfer
/// does. This function scripts a device that did not.
pub fn csw_passed_with_residue(tag: u32, residue: u32) -> Step {
    let Step::Reply(mut bytes) = csw(tag, 0x00) else {
        unreachable!("csw builds a Reply")
    };
    bytes[8..12].copy_from_slice(&residue.to_le_bytes());
    Step::Reply(bytes)
}

/// A CSW answering `tag` with `status` and a residue of zero.
fn csw(tag: u32, status: u8) -> Step {
    let mut csw = vec![0u8; bot::CSW_LEN];
    csw[0..4].copy_from_slice(b"USBS");
    csw[4..8].copy_from_slice(&tag.to_le_bytes());
    csw[12] = status;
    Step::Reply(csw)
}

/// One LBA read: the CBW the host must send, the bytes the device answers with,
/// and a passing CSW.
///
/// `body` is a whole number of sectors, and its length is what the command asks
/// for.
pub fn scripted_read(tag: u32, lba: u64, body: Vec<u8>) -> Vec<Step> {
    let sectors = (body.len() / 512) as u16;
    vec![
        cbw(
            tag,
            body.len() as u32,
            Direction::In,
            Opcode::LbaRead,
            lba as u32,
            sectors,
        ),
        Step::Reply(body),
        csw_passed(tag),
    ]
}

/// One LBA write: the CBW, the bytes themselves, and a passing CSW.
///
/// The body is asserted, so a test written against this pins the bytes that reach
/// the flash, not only that a write happened.
pub fn scripted_write(tag: u32, lba: u64, body: Vec<u8>) -> Vec<Step> {
    let sectors = (body.len() / 512) as u16;
    vec![
        cbw(
            tag,
            body.len() as u32,
            Direction::Out,
            Opcode::LbaWrite,
            lba as u32,
            sectors,
        ),
        Step::ExpectWrite(body),
        csw_passed(tag),
    ]
}

/// The commands the agent issues for an LBA transfer of `body`, split into the
/// 32-sector chunks a single read or write is capped at.
///
/// [`scripted_read`] and [`scripted_write`] frame one command. A transfer longer
/// than 32 sectors (16 KiB) becomes several commands, each addressed at the sector
/// where the previous one ended. A scripted transport must expect exactly that
/// sequence. `write` chooses the direction. The tags run consecutively from
/// `first_tag`, and the function returns the next free tag, so a caller can chain a
/// read-back after a write. A table write larger than a chunk, such as an authored
/// GPT's 34-sector primary, is scripted with this function.
pub fn scripted_transfer(first_tag: u32, lba: u64, body: &[u8], write: bool) -> (Vec<Step>, u32) {
    // The agent caps a command at 32 sectors (its `MAX_CHUNK_SECTORS`).
    const CHUNK: usize = 32 * 512;
    let mut steps = Vec::new();
    let mut tag = first_tag;
    let mut at = lba;
    for chunk in body.chunks(CHUNK) {
        let these = if write {
            scripted_write(tag, at, chunk.to_vec())
        } else {
            scripted_read(tag, at, chunk.to_vec())
        };
        steps.extend(these);
        at += (chunk.len() / 512) as u64;
        tag += 1;
    }
    (steps, tag)
}

/// A whole sector, holding `body` and zeros after it.
///
/// A device returns this for a one-sector read of data smaller than a sector.
pub fn sector(body: &[u8]) -> Vec<u8> {
    let mut sector = body.to_vec();
    sector.resize(512, 0);
    sector
}

/// The reads a partition-table lookup makes on a device that has no table.
///
/// The reads are the GPT probe, then every sector a Rockchip parameter block is
/// written to. All of them return zeros, as on a device whose leading sectors are
/// blank. It has no `EFI PART` at sector 1, and no `PARM` at any location a `PARM`
/// is kept.
///
/// The lookup takes ten commands here, which is its worst case. On a device with a
/// GPT, the first command answers it.
pub fn scripted_no_table(first_tag: u32) -> Vec<Step> {
    let mut steps = scripted_read(first_tag, gpt::HEADER_LBA, vec![0u8; 512]);
    for (index, location) in rkparam::LOCATIONS.iter().enumerate() {
        steps.extend(scripted_read(
            first_tag + 1 + index as u32,
            location.lba,
            vec![0u8; 512],
        ));
    }
    steps
}

/// One GPT partition entry, as a device's table stores it.
///
/// Its `type_guid` is not all zero, which marks the entry as used rather than an
/// empty slot.
pub fn gpt_entry(first_lba: u64, last_lba: u64, name: &str) -> Vec<u8> {
    let mut raw = vec![0u8; gpt::ENTRY_LEN];
    raw[0x00..0x10].copy_from_slice(&[0x0f; 16]); // a type GUID
    raw[0x10..0x20].copy_from_slice(&[0xab; 16]); // a unique GUID
    raw[0x20..0x28].copy_from_slice(&first_lba.to_le_bytes());
    raw[0x28..0x30].copy_from_slice(&last_lba.to_le_bytes());

    for (slot, unit) in raw[0x38..0x80]
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(name.encode_utf16())
    {
        *slot = unit.to_le_bytes();
    }
    raw
}

/// A GPT's primary header sector and entry array, with every CRC correct.
///
/// They are returned in the order a table lookup reads them. The primary header is
/// in sector 1 and its array in sector 2, so this is [`gpt_copy`] at those two
/// locations.
pub fn gpt_table(entries: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    gpt_copy(entries, 1, 2)
}

/// One GPT copy's header, for a copy whose header is at `current_lba` and whose
/// entry array is at `array_lba`, with every CRC correct.
///
/// The primary and the backup are the same layout in different sectors. The
/// primary is `gpt_copy(entries, 1, 2)`, and the backup names the device's last
/// sector and its own array before it. The entry array is byte-identical between
/// the two, so its CRC is the same in both headers. Only the sector fields, and
/// therefore the header CRC, differ. It returns the header sector and the array.
pub fn gpt_copy(entries: &[Vec<u8>], current_lba: u64, array_lba: u64) -> (Vec<u8>, Vec<u8>) {
    let array = entries.concat();

    let mut header = vec![0u8; 512];
    header[0x00..0x08].copy_from_slice(gpt::SIGNATURE);
    header[0x08..0x0c].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
    header[0x0c..0x10].copy_from_slice(&(gpt::HEADER_LEN as u32).to_le_bytes());
    header[0x18..0x20].copy_from_slice(&current_lba.to_le_bytes()); // this header's sector
    header[0x28..0x30].copy_from_slice(&34u64.to_le_bytes()); // first usable
    header[0x48..0x50].copy_from_slice(&array_lba.to_le_bytes()); // the entry array's sector
    header[0x50..0x54].copy_from_slice(&(entries.len() as u32).to_le_bytes());
    header[0x54..0x58].copy_from_slice(&(gpt::ENTRY_LEN as u32).to_le_bytes());
    header[0x58..0x5c].copy_from_slice(&crate::codec::crc::crc32(&array).to_le_bytes());

    // The header's own CRC is computed last, over the header with the field it
    // goes in still zeroed -- which is what it held when the CRC was taken.
    let crc = crate::codec::crc::crc32(&header[..gpt::HEADER_LEN]);
    header[0x10..0x14].copy_from_slice(&crc.to_le_bytes());

    (header, array)
}

/// The two reads a table lookup makes of a device carrying a GPT.
///
/// The first reads the header at sector 1. The second reads the entry array from
/// the sector the header names.
pub fn scripted_gpt(first_tag: u32, entries: &[Vec<u8>]) -> Vec<Step> {
    let (header, array) = gpt_table(entries);
    let array_sectors = array.len().div_ceil(512).max(1);

    let mut steps = scripted_read(first_tag, gpt::HEADER_LBA, header);
    steps.extend(scripted_read(
        first_tag + 1,
        2, // where `gpt_table` put the array
        {
            let mut padded = array;
            padded.resize(array_sectors * 512, 0);
            padded
        },
    ));
    steps
}

/// The sectors of the eMMC [`scripted_info`] describes: 122,142,720 sectors of 512
/// bytes, about 58.2 GiB. That is the capacity of a 64 GB-class eMMC.
pub const FLASH_SECTORS: u64 = 122_142_720;

/// The loader's answer to which SoC it is on.
///
/// It is the reply a real RK3576 gave: "6753", the SoC's ASCII digits
/// byte-reversed, then zeros. The scripted board is therefore an RK3576, and a plan
/// made against it arms the wrong-loader gate for `Soc::parse("rk3576")`. A plan
/// carries the bytes uninterpreted. Only the gate compares them, and it compares
/// them whole.
pub const CHIP_VERSION: [u8; 16] = [
    0x36, 0x37, 0x35, 0x33, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The two commands `info` issues, answering with a 64 GB-class eMMC.
pub fn scripted_info(tag: u32) -> Vec<Step> {
    scripted_info_of(tag, FLASH_SECTORS as u32)
}

/// The same commands, for a part of the size a test needs.
///
/// A clone reads its source in full, so a scripted source is small.
pub fn scripted_info_of(tag: u32, sectors: u32) -> Vec<Step> {
    let mut flash_info = vec![0u8; rockusb::FLASH_INFO_LEN];
    flash_info[0..4].copy_from_slice(&sectors.to_le_bytes()); // sectors
    flash_info[4..6].copy_from_slice(&1024u16.to_le_bytes()); // block size

    vec![
        cbw(
            tag,
            rockusb::FLASH_ID_LEN as u32,
            Direction::In,
            Opcode::ReadFlashId,
            0,
            0,
        ),
        Step::Reply(vec![0x45, 0x4d, 0x4d, 0x43, 0x20]),
        csw_passed(tag),
        cbw(
            tag + 1,
            rockusb::FLASH_INFO_LEN as u32,
            Direction::In,
            Opcode::ReadFlashInfo,
            0,
            0,
        ),
        Step::Reply(flash_info),
        csw_passed(tag + 1),
    ]
}

/// The chip-version command a plan issues, and the loader's answer.
pub fn scripted_chip_version(tag: u32) -> Vec<Step> {
    scripted_chip_version_of(tag, &CHIP_VERSION)
}

/// The same command, answered with `reply`, for any SoC a test needs.
///
/// The wrong-loader gate's tests script a loader that answers as a different SoC
/// from the one the plan named. [`scripted_chip_version`] always answers as an
/// RK3576.
pub fn scripted_chip_version_of(tag: u32, reply: &[u8]) -> Vec<Step> {
    vec![
        cbw(
            tag,
            rockusb::CHIP_VER_LEN as u32,
            Direction::In,
            Opcode::GetChipVer,
            0,
            0,
        ),
        Step::Reply(reply.to_vec()),
        csw_passed(tag),
    ]
}

/// Everything a write plan asks a device: the geometry, the loader naming its SoC,
/// and the partition table.
///
/// Every one of them is a read, because the dry run sends nothing that changes the
/// device. If a plan issued a write, the scripted transport would catch it.
///
/// This is the device with no table. Most tests use it, because their subject is
/// the write. [`scripted_plan_with_gpt`] scripts a device with a table.
pub fn scripted_plan(tag: u32) -> Vec<Step> {
    let mut steps = scripted_info(tag); // tag, tag + 1
    steps.extend(scripted_chip_version(tag + 2)); // tag + 2
    steps.extend(scripted_no_table(tag + 3)); // tag + 3 .. tag + 12
    steps
}

/// The same reads, on a board that carries a GPT.
///
/// The two reads of the table replace the ten that establish there is none.
pub fn scripted_plan_with_gpt(tag: u32, entries: &[Vec<u8>]) -> Vec<Step> {
    let mut steps = scripted_info(tag);
    steps.extend(scripted_chip_version(tag + 2));
    steps.extend(scripted_gpt(tag + 3, entries));
    steps
}

/// A Rockchip board's partitions, as a GPT records them: a bootloader region
/// outside every partition, then contiguous partitions.
pub fn a_boards_partitions() -> Vec<Vec<u8>> {
    vec![
        gpt_entry(16384, 24575, "uboot"),
        gpt_entry(24576, 32767, "trust"),
        gpt_entry(32768, 262143, "boot"),
    ]
}

/// A Rockchip parameter block, checksummed with
/// [`crc32_rockchip`](crate::codec::crc::crc32_rockchip).
///
/// `text` is the parameter text: the `KEY: value` lines, one of which is the
/// `CMDLINE` that carries the partitions.
pub fn param_block(text: &str) -> Vec<u8> {
    let text = text.as_bytes();

    let mut block = Vec::new();
    block.extend_from_slice(rkparam::MAGIC);
    block.extend_from_slice(&(text.len() as u32).to_le_bytes());
    block.extend_from_slice(text);
    block.extend_from_slice(&crate::codec::crc::crc32_rockchip(text).to_le_bytes());
    block.resize(block.len().next_multiple_of(512), 0);
    block
}
