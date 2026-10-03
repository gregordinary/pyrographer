//! The RKBOOT loader container, and the payload the maskrom download-boot
//! uploads from it.
//!
//! A Rockchip loader (`_loader.bin`, the file `db` takes) is a small container. It
//! lists the code sections and says where each goes. This module parses that
//! container into the two sections a bootstrap needs: the 471 DRAM-init blobs and
//! the 472 loader.
//!
//! It then prepares each section for the wire: the section's bytes exactly as the
//! container stores them, plus a trailing CRC-16. One zero byte of padding goes
//! before the CRC where the length requires it. The reference tool's upload path
//! sends stored bytes verbatim on every chip. `CRKBoot::GetEntryData` is a copy,
//! and `RKU_DeviceRequest` adds only the pad rule and the CRC. This module does the
//! same, and any scrambling a section needs is already in the file.
//!
//! Parsing and payload preparation are sans-I/O. [`bootstrap`](crate::bootstrap)
//! carries these bytes to the device over control transfers. This module produces
//! exactly the byte string that module sends, so the tests pin every byte with no
//! hardware attached.
//!
//! The container layout and payload rules match the reference implementation
//! (rkdeveloptool `RKBoot.cpp`, `RKComm.cpp`). The tests pin both against the real
//! reference loader and against constructed fixtures.

use crate::codec::crc::crc16_ccitt_false;
use crate::{Error, Result};

/// The container tag on current loaders: `"LDR "`, little-endian.
const TAG_LDR: u32 = 0x2052_444c;
/// The container tag on older loaders: `"BOOT"`, little-endian. The header that
/// follows is the same under both tags. Only the tag differs.
const TAG_BOOT: u32 = 0x544f_4f42;

/// The size the entry descriptors declare and that this parser requires at
/// minimum: `u8` size, `u32` type, 40-byte name, then three `u32`s.
const ENTRY_SIZE: usize = 57;

/// The control-transfer chunk size the BootROM reads the payload in. A chunk
/// shorter than this marks the end of a section. [`bootstrap`](crate::bootstrap)
/// splits the payload into chunks of this size. The reference tool sends 4096-byte
/// pieces in `RKU_DeviceRequest`.
pub const CHUNK_SIZE: usize = 4096;

// Header field offsets. The header size varies, but these descriptor fields sit
// at fixed offsets within it (rkdeveloptool's `STRUCT_RKBOOT_HEAD`), the same for
// both tags.
const OFF_TAG: usize = 0;
/// The chip the container says it is for: four bytes at offset 21, the
/// `emSupportChip` field of `STRUCT_RKBOOT_HEAD`. It is read raw and never
/// decoded, as [`LoaderImage::chip`] explains.
const OFF_CHIP: usize = 21;
/// The width of that field.
const CHIP_LEN: usize = 4;
const OFF_471_COUNT: usize = 25;
const OFF_471_OFFSET: usize = 26;
const OFF_471_ENTRY_SIZE: usize = 30;
const OFF_472_COUNT: usize = 31;
const OFF_472_OFFSET: usize = 32;
const OFF_472_ENTRY_SIZE: usize = 36;
const OFF_RC4_FLAG: usize = 44;
/// The smallest header that carries every descriptor field above.
const HEADER_MIN: usize = OFF_RC4_FLAG + 1;

// Entry field offsets, relative to the start of one 57-byte entry.
const E_NAME: usize = 5;
const E_NAME_LEN: usize = 40;
const E_DATA_OFFSET: usize = 45;
const E_DATA_SIZE: usize = 49;
const E_DATA_DELAY: usize = 53;

/// One code section from the container: a named blob, and how long to wait after
/// uploading it.
#[derive(Debug, Clone)]
pub struct CodeBlob {
    /// The entry's name, as the container spells it, such as `UsbHead`. It is a
    /// label for a person and a log, and the upload does not depend on it.
    pub name: String,
    /// The raw section bytes, as the file stores them. [`download_payload`] adds the
    /// pad byte, where the length needs one, and the trailing CRC. Neither is stored
    /// here, because a test compares against these same bytes.
    pub data: Vec<u8>,
    /// Milliseconds to wait after this section is uploaded, from the container.
    /// For a 471 DRAM-init blob, it is the time the controller needs to bring memory
    /// up. If the delay is not honored, the next stage can jump into memory that is
    /// not yet ready.
    pub delay_ms: u32,
}

/// A parsed loader, reduced to what a bootstrap needs.
///
/// The container also holds a third table, the flash-stage `loader` blobs. A
/// flash write lays those down, and they play no part in bringing a maskrom board
/// to loader mode, so this parser skips them. The image holds the 471 and 472
/// sections in upload order, the container's chip field, and its RC4 flag.
#[derive(Debug, Clone)]
pub struct LoaderImage {
    /// The DRAM-init sections, uploaded first with `wIndex = 0x0471`.
    pub code_471: Vec<CodeBlob>,
    /// The loader sections, uploaded after with `wIndex = 0x0472`.
    pub code_472: Vec<CodeBlob>,
    /// The chip the container says it is for, as four raw bytes. It is `None` for a
    /// loader assembled from bare stage files, which carry no container and so make
    /// no claim.
    ///
    /// It is read and reported, never decoded. On the containers measured, it holds
    /// the SoC's ASCII digits byte-reversed. `rk3576` stores `"6753"`, the same four
    /// bytes that SoC's loader answers `K_FW_GET_CHIP_VER` with. The two match
    /// because `boot_merger` writes the field from the `[CHIP_NAME] NAME=` line of
    /// the `.ini` it builds from.
    ///
    /// That match is observed on four RK3576 containers, and is not a rule. Older
    /// containers under the `BOOT` tag can hold an enumerated device type here
    /// instead, and none has been measured. This module therefore returns the bytes
    /// unchanged, and [`soc`](crate::soc) decides whether any pinned SoC claims
    /// them. The field's offset and width are **\[DOC\]**, from the reference
    /// tool's struct. Anything the four measured containers do not cover is
    /// **\[UNVERIFIED\]**.
    ///
    /// It is the container's claim about itself, written by whoever built the file,
    /// and no chip attests to it. It catches a person picking the wrong file. It
    /// does not prove what the blob does once it runs in SRAM.
    pub chip: Option<[u8; CHIP_LEN]>,
    /// The container's RC4-disable flag: `true` means this SoC's BootROM takes
    /// plain data where older BootROMs took RC4-scrambled data.
    ///
    /// It does not change what goes on the wire, because the download-boot sends
    /// each section's stored bytes verbatim on every chip. The flag governs whether
    /// the on-flash ID-block build re-scrambles, and pyrographer has no ID-block
    /// build. It is parsed as a fact about the file, and reported but not
    /// consulted.
    pub rc4_disabled: bool,
}

/// Milliseconds a raw 471 section settles after upload. It is the delay the real
/// RK3576 container asks for after each of its own 471 stages. The
/// hardware-verified upload ran with it.
const RAW_DELAY_471_MS: u32 = 1;

/// Milliseconds a raw 472 section settles after upload: zero, as in the real
/// RK3576 container. The 472 jump tears down the USB device, so nothing follows
/// it to wait for.
const RAW_DELAY_472_MS: u32 = 0;

impl LoaderImage {
    /// A loader built from raw section files rather than an RKBOOT container.
    ///
    /// The download-boot stages exist outside containers too. Mainline U-Boot's
    /// `CONFIG_ROCKCHIP_MASKROM_IMAGE` has binman emit them as two bare files,
    /// `u-boot-rockchip-usb471.bin` and `u-boot-rockchip-usb472.bin`. A mainline
    /// U-Boot is RAM-booted from maskrom this way, with nothing written to flash.
    /// Each pair is a display name and the file's bytes, sent exactly as given. The
    /// wire treatment (pad rule, trailing CRC, chunking) is the same for a
    /// container and a bare file.
    ///
    /// A raw file carries no post-upload delay. This function uses the delays the
    /// real RK3576 container asks for: 1 ms after a 471 stage, and none after the
    /// 472 stage. The hardware-verified upload ran with those values. The RC4 flag
    /// is reported as disabled, because there is no container to read it from. The
    /// flag never changes what goes on the wire.
    pub fn from_raw(
        code_471: Option<(String, Vec<u8>)>,
        code_472: Option<(String, Vec<u8>)>,
    ) -> Self {
        let blob = |(name, data): (String, Vec<u8>), delay_ms: u32| CodeBlob {
            name,
            data,
            delay_ms,
        };
        Self {
            code_471: code_471
                .map(|section| vec![blob(section, RAW_DELAY_471_MS)])
                .unwrap_or_default(),
            code_472: code_472
                .map(|section| vec![blob(section, RAW_DELAY_472_MS)])
                .unwrap_or_default(),
            chip: None,
            rc4_disabled: true,
        }
    }
}

/// Parse an RKBOOT container into its 471 and 472 sections.
///
/// The parser follows the container's own structure. The tag identifies the
/// container, and the descriptor fields at their fixed offsets give the number and
/// location of the sections. Each section's data is copied from the range its
/// entry points at.
///
/// A file that is not an RKBOOT container, or whose descriptors point past its own
/// end, returns [`Error::InvalidRequest`]. The file is not a loader, and only the
/// person who supplied it can fix that.
pub fn parse(bytes: &[u8]) -> Result<LoaderImage> {
    let tag = le_u32(bytes, OFF_TAG)?;
    if tag != TAG_LDR && tag != TAG_BOOT {
        return Err(malformed(format!(
            "not an RKBOOT loader: the first four bytes are {tag:#010x}, \
             not \"LDR \" ({TAG_LDR:#010x}) or \"BOOT\" ({TAG_BOOT:#010x})"
        )));
    }
    if bytes.len() < HEADER_MIN {
        return Err(malformed(format!(
            "the header is {} bytes, too short for an RKBOOT descriptor",
            bytes.len()
        )));
    }

    let code_471 = parse_table(
        bytes,
        OFF_471_COUNT,
        OFF_471_OFFSET,
        OFF_471_ENTRY_SIZE,
        "471",
    )?;
    let code_472 = parse_table(
        bytes,
        OFF_472_COUNT,
        OFF_472_OFFSET,
        OFF_472_ENTRY_SIZE,
        "472",
    )?;
    let rc4_disabled = bytes[OFF_RC4_FLAG] != 0;
    // Infallible: `HEADER_MIN` is past the end of this field, and the length
    // check above has already run.
    let chip = bytes
        .get(OFF_CHIP..OFF_CHIP + CHIP_LEN)
        .and_then(|field| field.try_into().ok());

    Ok(LoaderImage {
        code_471,
        code_472,
        chip,
        rc4_disabled,
    })
}

/// Parse one entry table (471 or 472) into its blobs.
fn parse_table(
    bytes: &[u8],
    count_off: usize,
    offset_off: usize,
    entry_size_off: usize,
    which: &'static str,
) -> Result<Vec<CodeBlob>> {
    let count = usize::from(byte(bytes, count_off)?);
    let table = le_u32(bytes, offset_off)? as usize;
    let stride = usize::from(byte(bytes, entry_size_off)?);
    if stride < ENTRY_SIZE {
        return Err(malformed(format!(
            "the {which} entries are {stride} bytes, smaller than the {ENTRY_SIZE} \
             an RKBOOT entry needs"
        )));
    }

    (0..count)
        .map(|i| {
            // Every offset here is a `usize` sum of `u32` values read from the file.
            // On a 64-bit host they cannot overflow; on wasm32, where `usize` is
            // 32-bit and the maskrom flow ships, a malformed file with offsets near
            // `u32::MAX` can -- and a wrap in `table + i*stride` would relocate an
            // entry to a small bogus offset and *misparse* rather than refuse. So
            // every sum is checked, and an overflow is a malformed file like any
            // other bad offset.
            let base = i
                .checked_mul(stride)
                .and_then(|step| table.checked_add(step))
                .ok_or_else(|| malformed(format!("{which} entry {i} offset overflows a usize")))?;
            let base_end = base.checked_add(ENTRY_SIZE);
            let entry = base_end
                .and_then(|end| bytes.get(base..end))
                .ok_or_else(|| {
                    malformed(format!(
                        "{which} entry {i} at offset {base} runs past the {} bytes of the file",
                        bytes.len()
                    ))
                })?;
            let name = utf16_name(&entry[E_NAME..E_NAME + E_NAME_LEN]);
            let data_offset = le_u32(entry, E_DATA_OFFSET)? as usize;
            let data_size = le_u32(entry, E_DATA_SIZE)? as usize;
            let delay_ms = le_u32(entry, E_DATA_DELAY)?;
            let data = data_offset
                .checked_add(data_size)
                .and_then(|end| bytes.get(data_offset..end))
                .ok_or_else(|| {
                    malformed(format!(
                        "{which} entry {i} ({name:?}) points at {data_size} bytes from offset \
                         {data_offset}, past the {} bytes of the file",
                        bytes.len()
                    ))
                })?
                .to_vec();
            Ok(CodeBlob {
                name,
                data,
                delay_ms,
            })
        })
        .collect()
}

/// Prepare one section's bytes for upload: the stored bytes, the pad rule, and
/// a trailing CRC-16.
///
/// The data goes on the wire exactly as the container stores it. The reference
/// upload transforms nothing: `RKU_DeviceRequest` operates on `GetEntryData`'s
/// verbatim copy. The reference adds two things, and so does this function:
///
/// - **The pad rule.** A section whose length is one byte short of a whole
///   [`CHUNK_SIZE`] gets one zero byte appended before the CRC. The CRC then never
///   travels split across a chunk boundary as a one-byte tail
///   (`RKU_DeviceRequest`, `case 4095`). The CRC covers the pad byte.
/// - **The CRC.** A CRC-16 over everything before it, appended big-endian:
///   polynomial `0x1021`, init `0xffff`, unreflected, with no final xor. The
///   BootROM checks it per section, and silently rejects the section on a
///   mismatch. The transfers still complete. The failure shows later, as a
///   BootROM that never runs the code it was sent.
///
/// A payload that lands on an exact multiple of [`CHUNK_SIZE`] needs one more wire
/// step, a one-byte terminating transfer. [`bootstrap`](crate::bootstrap), which
/// owns the transfers, sends it.
pub fn download_payload(data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(data.len() + 3);
    payload.extend_from_slice(data);

    if payload.len() % CHUNK_SIZE == CHUNK_SIZE - 1 {
        payload.push(0);
    }

    let crc = crc16_ccitt_false(&payload);
    payload.push((crc >> 8) as u8);
    payload.push((crc & 0xff) as u8);
    payload
}

/// Decode an entry name: up to 20 UTF-16LE code units, NUL-terminated.
///
/// The rule matches the GPT partition name's. Whatever follows the first NUL is
/// padding. A malformed unit becomes the replacement character, not an error,
/// because the name is only a label.
fn utf16_name(bytes: &[u8]) -> String {
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0);

    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Read one byte at `offset`, or a malformed-file error.
fn byte(bytes: &[u8], offset: usize) -> Result<u8> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| short(bytes, offset, 1))
}

/// Read a little-endian `u32` at `offset`, or a malformed-file error.
fn le_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let field: [u8; 4] = bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| short(bytes, offset, 4))?;
    Ok(u32::from_le_bytes(field))
}

/// The error for a file too short to hold a field the parser reached for.
fn short(bytes: &[u8], offset: usize, len: usize) -> Error {
    malformed(format!(
        "a {len}-byte field at offset {offset} runs past the {} bytes of the file",
        bytes.len()
    ))
}

/// Wrap a reason as the malformed-loader error. A loader file is user input, so a
/// bad one is [`Error::InvalidRequest`]: nothing malfunctioned, and only the
/// caller can supply a different file.
fn malformed(detail: String) -> Error {
    Error::InvalidRequest(format!("loader file: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but valid RKBOOT container: `code_471` and `code_472` each a
    /// list of `(name, data, delay_ms)`, and the rc4 flag. The layout matches what
    /// `parse` reads: a `HEADER_MIN`-byte header, then the two entry tables, then
    /// the section data each entry points at.
    fn build(
        tag: u32,
        code_471: &[(&str, &[u8], u32)],
        code_472: &[(&str, &[u8], u32)],
        rc4_flag: u8,
    ) -> Vec<u8> {
        let entries: Vec<&(&str, &[u8], u32)> = code_471.iter().chain(code_472).collect();
        let header_size = HEADER_MIN;
        let table_471 = header_size;
        let table_472 = table_471 + code_471.len() * ENTRY_SIZE;
        let data_start = table_472 + code_472.len() * ENTRY_SIZE;

        // Lay out the section data after both tables, recording each blob's offset.
        let mut blob_at = Vec::new();
        let mut cursor = data_start;
        for (_, data, _) in &entries {
            blob_at.push(cursor);
            cursor += data.len();
        }
        let total = cursor;

        let mut buf = vec![0u8; total];
        buf[OFF_TAG..OFF_TAG + 4].copy_from_slice(&tag.to_le_bytes());
        buf[OFF_471_COUNT] = code_471.len() as u8;
        buf[OFF_471_OFFSET..OFF_471_OFFSET + 4].copy_from_slice(&(table_471 as u32).to_le_bytes());
        buf[OFF_471_ENTRY_SIZE] = ENTRY_SIZE as u8;
        buf[OFF_472_COUNT] = code_472.len() as u8;
        buf[OFF_472_OFFSET..OFF_472_OFFSET + 4].copy_from_slice(&(table_472 as u32).to_le_bytes());
        buf[OFF_472_ENTRY_SIZE] = ENTRY_SIZE as u8;
        buf[OFF_RC4_FLAG] = rc4_flag;
        buf[OFF_CHIP..OFF_CHIP + CHIP_LEN].copy_from_slice(b"6753");

        for (i, (name, data, delay)) in entries.iter().enumerate() {
            let entry = if i < code_471.len() {
                table_471 + i * ENTRY_SIZE
            } else {
                table_472 + (i - code_471.len()) * ENTRY_SIZE
            };
            buf[entry] = ENTRY_SIZE as u8;
            for (u, unit) in name.encode_utf16().enumerate() {
                let at = entry + E_NAME + u * 2;
                buf[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
            buf[entry + E_DATA_OFFSET..entry + E_DATA_OFFSET + 4]
                .copy_from_slice(&(blob_at[i] as u32).to_le_bytes());
            buf[entry + E_DATA_SIZE..entry + E_DATA_SIZE + 4]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            buf[entry + E_DATA_DELAY..entry + E_DATA_DELAY + 4]
                .copy_from_slice(&delay.to_le_bytes());
            buf[blob_at[i]..blob_at[i] + data.len()].copy_from_slice(data);
        }
        buf
    }

    #[test]
    fn parse_pulls_out_both_sections_with_names_and_delays() {
        let bytes = build(
            TAG_LDR,
            &[("UsbHead", &[0x11, 0x22], 30), ("FlashHead", &[0x33], 0)],
            &[("Loader", &[0x44, 0x55, 0x66], 5)],
            1,
        );
        let loader = parse(&bytes).expect("a well-formed container parses");

        assert!(loader.rc4_disabled, "flag byte 1 means RC4 disabled");
        assert_eq!(loader.code_471.len(), 2);
        assert_eq!(loader.code_472.len(), 1);
        assert_eq!(loader.code_471[0].name, "UsbHead");
        assert_eq!(
            loader.chip,
            Some(*b"6753"),
            "the RK3576 container names its SoC in the chip field: the digits of \"3576\" \
             byte-reversed, the same four bytes an RK3576 loader answers chipver with"
        );
        assert_eq!(loader.code_471[0].data, [0x11, 0x22]);
        assert_eq!(loader.code_471[0].delay_ms, 30);
        assert_eq!(loader.code_471[1].name, "FlashHead");
        assert_eq!(loader.code_472[0].name, "Loader");
        assert_eq!(loader.code_472[0].data, [0x44, 0x55, 0x66]);
    }

    #[test]
    fn the_old_boot_tag_parses_the_same_as_the_new_ldr_tag() {
        let sections = [("UsbHead", &[0xaa, 0xbb][..], 0)];
        let ldr = parse(&build(TAG_LDR, &sections, &[], 0)).expect("LDR");
        let boot = parse(&build(TAG_BOOT, &sections, &[], 0)).expect("BOOT");
        assert_eq!(ldr.code_471[0].name, boot.code_471[0].name);
        assert_eq!(ldr.code_471[0].data, boot.code_471[0].data);
    }

    /// A raw pair builds the same shape a parsed container does, with the
    /// container-pinned delays. Either stage alone leaves the other list empty.
    #[test]
    fn from_raw_builds_sections_with_the_container_pinned_delays() {
        let loader = LoaderImage::from_raw(
            Some(("usb471.bin".to_string(), vec![0x11, 0x22])),
            Some(("usb472.bin".to_string(), vec![0x33])),
        );
        assert_eq!(loader.code_471.len(), 1);
        assert_eq!(loader.code_471[0].name, "usb471.bin");
        assert_eq!(loader.code_471[0].data, [0x11, 0x22]);
        assert_eq!(loader.code_471[0].delay_ms, RAW_DELAY_471_MS);
        assert_eq!(loader.code_472.len(), 1);
        assert_eq!(loader.code_472[0].delay_ms, RAW_DELAY_472_MS);
        assert!(loader.rc4_disabled);

        let only_472 = LoaderImage::from_raw(None, Some(("usb472.bin".to_string(), vec![0x44])));
        assert!(only_472.code_471.is_empty());
        assert_eq!(only_472.code_472.len(), 1);
    }

    #[test]
    fn a_file_that_is_not_a_container_is_refused() {
        let err = parse(b"this is not a loader at all, really").expect_err("bad tag");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    #[test]
    fn an_entry_pointing_past_the_file_is_refused_rather_than_panicking() {
        let mut bytes = build(TAG_LDR, &[("UsbHead", &[0x11], 0)], &[], 0);
        // Overwrite the first entry's data size with something enormous.
        let entry = HEADER_MIN;
        bytes[entry + E_DATA_SIZE..entry + E_DATA_SIZE + 4]
            .copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        let err = parse(&bytes).expect_err("out-of-range data");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// A file too short even to hold the four-byte tag is refused, not indexed
    /// past its end.
    #[test]
    fn a_file_too_short_to_hold_a_tag_is_refused() {
        let err = parse(&[0x4c, 0x44]).expect_err("two bytes cannot hold a four-byte tag");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// A valid tag but a header too short for the entry-table fields is refused.
    #[test]
    fn a_header_shorter_than_the_descriptor_is_refused() {
        let mut bytes = vec![0u8; HEADER_MIN - 1];
        bytes[OFF_TAG..OFF_TAG + 4].copy_from_slice(&TAG_LDR.to_le_bytes());
        let err = parse(&bytes).expect_err("too short for an RKBOOT descriptor");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// An entry stride smaller than an entry itself is refused rather than
    /// walking entries that overlap.
    #[test]
    fn an_entry_stride_smaller_than_an_entry_is_refused() {
        let mut bytes = build(TAG_LDR, &[("UsbHead", &[0x11], 0)], &[], 0);
        bytes[OFF_471_ENTRY_SIZE] = (ENTRY_SIZE - 1) as u8;
        let err = parse(&bytes).expect_err("a stride below the entry size");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    /// An entry-table offset that points past the end of the file is refused
    /// rather than panicking (and, on wasm32, rather than wrapping to a small
    /// bogus offset and misparsing).
    #[test]
    fn an_entry_table_offset_past_the_file_is_refused() {
        let mut bytes = build(TAG_LDR, &[("UsbHead", &[0x11], 0)], &[], 0);
        bytes[OFF_471_OFFSET..OFF_471_OFFSET + 4].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        let err = parse(&bytes).expect_err("a table offset past EOF");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    }

    #[test]
    fn download_payload_without_rc4_is_the_data_plus_a_big_endian_crc() {
        let data = [0x01, 0x02, 0x03, 0x04];
        let payload = download_payload(&data);
        let crc = crc16_ccitt_false(&data);
        assert_eq!(&payload[..4], &data);
        assert_eq!(payload[4], (crc >> 8) as u8);
        assert_eq!(payload[5], (crc & 0xff) as u8);
    }

    /// The pad rule: data one byte short of a whole chunk gets one zero byte
    /// before the CRC. The CRC then never travels split across a chunk boundary
    /// as a one-byte tail (the reference's `case 4095`). The CRC covers the pad,
    /// and the payload ends two bytes past the chunk boundary.
    #[test]
    fn download_payload_pads_data_one_short_of_a_chunk_before_the_crc() {
        let data = vec![0x77; CHUNK_SIZE - 1];
        let payload = download_payload(&data);

        let mut padded = data.clone();
        padded.push(0);
        let crc = crc16_ccitt_false(&padded);

        assert_eq!(payload.len(), CHUNK_SIZE + 2);
        assert_eq!(&payload[..CHUNK_SIZE - 1], &data[..]);
        assert_eq!(payload[CHUNK_SIZE - 1], 0, "the pad byte");
        assert_eq!(payload[CHUNK_SIZE], (crc >> 8) as u8);
        assert_eq!(payload[CHUNK_SIZE + 1], (crc & 0xff) as u8);
    }

    /// The chip field is read out of the header, raw.
    #[test]
    fn a_container_carries_the_chip_it_was_built_for() {
        let bytes = build(TAG_LDR, &[("UsbHead", &[1, 2, 3], 0)], &[("L", &[4], 0)], 1);
        let loader = parse(&bytes).expect("the fixture parses");
        assert_eq!(
            loader.chip,
            Some(*b"6753"),
            "the four bytes at offset 21, as stored"
        );
    }

    /// Whatever the field holds comes back unchanged. The parser does not judge
    /// whether a chip value is plausible. On containers nobody has measured, the
    /// field can hold something other than ASCII: an older `BOOT`-tagged file can
    /// hold an enumerated device type there. Reading it raw lets [`crate::soc`]
    /// answer "no pinned SoC claims this" instead of the codec guessing.
    #[test]
    fn an_unrecognizable_chip_field_is_carried_rather_than_judged() {
        let mut bytes = build(TAG_LDR, &[("UsbHead", &[1], 0)], &[("L", &[2], 0)], 1);
        bytes[OFF_CHIP..OFF_CHIP + CHIP_LEN].copy_from_slice(&[0x50, 0x00, 0x00, 0x00]);
        let loader = parse(&bytes).expect("a strange chip field is not a malformed file");
        assert_eq!(loader.chip, Some([0x50, 0x00, 0x00, 0x00]));
    }

    /// Bare stage files carry no container, so they claim no chip. That differs
    /// from claiming an unrecognized chip, and the gate treats the two
    /// differently.
    #[test]
    fn raw_stages_claim_no_chip() {
        let loader = LoaderImage::from_raw(
            Some(("471".to_string(), vec![1, 2, 3])),
            Some(("472".to_string(), vec![4, 5])),
        );
        assert_eq!(loader.chip, None);
    }

    /// The real reference loader parses as expected. The file is not committed
    /// with the crate, so a checkout without it skips this test instead of
    /// failing. Where the file exists (the developer's tree), it catches container
    /// quirks the constructed fixtures cannot anticipate.
    #[test]
    fn the_real_rk3576_loader_parses_as_expected() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../reference/rk3576_spl_loader_v1.12.108.bin"
        );
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: reference loader not present at {path}");
            return;
        };
        let loader = parse(&bytes).expect("the real RK3576 loader parses");
        assert_eq!(loader.code_471.len(), 2, "471 sections");
        assert_eq!(loader.code_472.len(), 1, "472 sections");
        assert!(
            loader.rc4_disabled,
            "the RK3576 container flags RC4 off: its ROM takes the stored bytes as they are"
        );
        assert_eq!(loader.code_471[0].name, "UsbHead");
        assert!(
            !loader.code_471[0].data.is_empty() && !loader.code_472[0].data.is_empty(),
            "the sections carry data"
        );
    }
}
