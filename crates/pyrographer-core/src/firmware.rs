//! A Rockchip firmware package (`update.img`), read once from front to back.
//!
//! A package is gigabytes, and the image seam is a stream. So [`read`] takes the
//! file in one forward pass, in the order the bytes lie. The `RKFW` header comes
//! first, then the loader, then the `RKAF` archive's header. Every byte of the
//! archive then goes into its checksum. As the pass goes by, it keeps what a write
//! needs: the loader, the parameter, and the first bytes of each partition image.
//! Memory holds one window, as it does for every long verb.
//!
//! Before a package is planned, the pass makes these checks:
//!
//! - The archive's trailing checksum holds. A truncated or damaged download is the
//!   case it catches, and it covers every byte a write takes from the archive.
//! - The ID block builds from the package's loader, every hash its header records
//!   checked, as [`idb`] describes.
//! - The archive's `bootloader` entry is the same bytes as the loader the `RKFW`
//!   header points at. Two loaders that disagree leave no way to say which one the
//!   package means.
//! - The `parameter` entry is a block whose checksum holds.
//! - No partition image is an Android sparse image, which has to be expanded as it
//!   is written, and which pyrographer does not expand.
//!
//! The write reads the file again, forward, through [`ForwardReader`]: each
//! partition image streamed from where it sits, in the order the file holds them.
//! [`verbs::plan_firmware`](crate::verbs::plan_firmware) makes the plan, and
//! [`codec::rkfw`](crate::codec::rkfw) documents the format and its evidence.

use crate::codec::crc::crc32_rockchip_update;
use crate::codec::gpt;
use crate::codec::idb::{self, IdBlock};
use crate::codec::rkboot::{self, LoaderImage};
use crate::codec::rkfw::{self, Archive, PackageHeader};
use crate::codec::rkparam;
use crate::image::{BoxFuture, ImageReader};
use crate::layout::{Layout, LayoutPartition};
use crate::progress::{Cancel, Progress, ProgressSink};
use crate::{Error, Result};

/// How much the pass reads between progress events and cancellation checks.
const WINDOW_BYTES: usize = 1 << 20;

/// The largest loader container the pass holds in memory.
///
/// A loader is held whole, because the ID block is built from it. The RK3576
/// container is under a megabyte, so a header claiming more than this is damage
/// rather than a loader. **\[WEAK\]**
const MAX_LOADER_BYTES: u64 = 16 << 20;

/// The four bytes an Android sparse image begins with: its magic, `0xED26FF3A`,
/// little-endian. **\[DOC\]**
const SPARSE_MAGIC: [u8; 4] = [0x3a, 0xff, 0x26, 0xed];

/// The sector a package's offsets count, in bytes.
///
/// An `mtdparts` list counts 512-byte sectors, and so does an ID block. A package
/// is planned only onto a device whose sectors are that size.
pub const SECTOR_LEN: u32 = 512;

/// A firmware package, read and checked.
#[derive(Debug, Clone)]
pub struct Package {
    /// The file's length, in bytes.
    pub file_bytes: u64,
    /// The `RKFW` header.
    pub header: PackageHeader,
    /// The `RKAF` archive's header: its model, its manufacturer, and its entries.
    pub archive: Archive,
    /// The loader container the `RKFW` header points at.
    pub loader: LoaderImage,
    /// The ID block built from the loader, every hash checked.
    pub id_block: IdBlock,
    /// The text of the `parameter` entry, its checksum checked.
    pub parameter: String,
    /// The entries written to a partition, in the order the file holds them.
    pub images: Vec<PartitionImage>,
    /// The entries not written, each with the reason.
    pub skipped: Vec<Skipped>,
    /// The 32 bytes after the archive, which the reference tool reads as an MD5, or
    /// `None` where the file ends sooner.
    ///
    /// What the MD5 covers is unmeasured, so it is reported and not checked. The
    /// archive's checksum and the ID block's hashes cover what a write takes.
    pub trailer: Option<Vec<u8>>,
}

/// One partition image a package carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionImage {
    /// The partition it is written to, which is the entry's name.
    pub name: String,
    /// The path the packing tool took it from, such as `Image/boot.img`.
    pub path: String,
    /// Where its bytes begin, counted from the start of the file.
    pub offset: u64,
    /// How many bytes it holds.
    pub bytes: u64,
}

/// An entry a package carries that is not written, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The entry's name.
    pub name: String,
    /// The entry's path.
    pub path: String,
    /// Why it is not written, in words for a person.
    pub why: &'static str,
}

impl Package {
    /// The trailer as text, where it is 32 hexadecimal digits, as an MD5 written
    /// out is.
    pub fn trailer_text(&self) -> Option<&str> {
        let trailer = self.trailer.as_deref()?;
        trailer
            .iter()
            .all(u8::is_ascii_hexdigit)
            .then(|| std::str::from_utf8(trailer).ok())
            .flatten()
    }

    /// The GPT layout the package's parameter describes, on a part of
    /// `flash_sectors` 512-byte sectors.
    ///
    /// The parameter has to say `TYPE: GPT`. That line tells a tool to compile the
    /// partitions into a GPT, and makes their offsets true LBAs. A parameter without
    /// it is written as a `PARM` block whose offsets depend on the medium, and a
    /// package carrying one is refused.
    ///
    /// Two lines of the parameter shape the table beyond the `mtdparts` list:
    ///
    /// - **`uuid:<name>=<GUID>` pins that partition's unique GUID.** A kernel
    ///   command line that finds its root by `PARTUUID` depends on it. A line that
    ///   names no partition, or whose GUID does not parse, is refused.
    /// - **The partition whose size is `-` grows to the GPT's last usable sector**,
    ///   not to the end of the part, because the backup GPT holds the last sectors.
    pub fn layout(&self, flash_sectors: u64) -> Result<Layout> {
        let lines: Vec<(&str, &str)> = self
            .parameter
            .lines()
            .filter_map(|line| line.trim().split_once(':'))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect();

        let gpt = lines
            .iter()
            .any(|(key, value)| *key == "TYPE" && value.eq_ignore_ascii_case("GPT"));
        if !gpt {
            return Err(Error::InvalidRequest(
                "firmware package: its parameter has no 'TYPE: GPT' line, so its partitions are \
                 the legacy kind, written as a PARM block at offsets that depend on the medium. \
                 pyrographer writes a package whose parameter describes a GPT"
                    .to_string(),
            ));
        }

        let cmdline = lines
            .iter()
            .find(|(key, _)| *key == "CMDLINE")
            .map(|(_, value)| *value)
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "firmware package: its parameter has no CMDLINE line, so it lists no \
                     partitions"
                        .to_string(),
                )
            })?;
        let mtdparts = cmdline
            .split_whitespace()
            .find_map(|token| token.strip_prefix("mtdparts="))
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "firmware package: its parameter's CMDLINE has no mtdparts= in it".to_string(),
                )
            })?;

        let (_, last_usable) = gpt::usable_range(flash_sectors, SECTOR_LEN as usize)?;
        let partitions =
            rkparam::parse_mtdparts_growing_to(mtdparts, 0, flash_sectors, last_usable + 1)?;
        let mut layout = Layout {
            partitions: partitions
                .into_iter()
                .map(|part| LayoutPartition {
                    name: part.name,
                    first_lba: part.first_lba,
                    sectors: part.sectors,
                    kind: None,
                    unique_guid: None,
                })
                .collect(),
            disk_guid: None,
        };

        // `uuid:` is a key of its own, so its value is `<name>=<GUID>`.
        for (_, pin) in lines.iter().filter(|(key, _)| *key == "uuid") {
            let (name, guid) = pin.split_once('=').ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "firmware package: the parameter line 'uuid:{pin}' is not uuid:<name>=<GUID>"
                ))
            })?;
            let (name, guid) = (name.trim(), guid.trim());
            gpt::Guid::parse(guid).map_err(|error| {
                Error::InvalidRequest(format!(
                    "firmware package: the parameter pins partition '{name}' to an invalid GUID: \
                     {error}"
                ))
            })?;
            let partition = layout
                .partitions
                .iter_mut()
                .find(|part| part.name == name)
                .ok_or_else(|| {
                    Error::InvalidRequest(format!(
                        "firmware package: the parameter pins a GUID for '{name}', which is not \
                         one of its partitions"
                    ))
                })?;
            partition.unique_guid = Some(guid.to_string());
        }

        Ok(layout)
    }
}

/// Read and check the firmware package in `image`, `file_bytes` long.
///
/// It reads the file once, front to back, and reports progress over the bytes it
/// reads. It stops between windows once `cancel` is set. Every check the module
/// documentation lists is made here, and each failure is an
/// [`Error::InvalidRequest`] naming what failed. A package is user input, and only
/// the person who supplied it can supply another.
pub async fn read(
    image: &mut dyn ImageReader,
    file_bytes: u64,
    progress: ProgressSink<'_>,
    cancel: &Cancel,
) -> Result<Package> {
    let mut reader = ForwardReader::new(image);

    if file_bytes < rkfw::HEADER_LEN as u64 {
        return Err(refused(format!(
            "the file is {file_bytes} bytes, too short for the {}-byte RKFW header",
            rkfw::HEADER_LEN
        )));
    }
    let header = rkfw::parse_header(&reader.read_vec(rkfw::HEADER_LEN as u64).await?)?;
    let loader_range = header.loader;
    let archive_range = header.archive;
    let (Some(loader_end), Some(archive_end)) = (loader_range.end(), archive_range.end()) else {
        return Err(refused(
            "the RKFW header's ranges run past any byte that can be counted".to_string(),
        ));
    };

    // The pass reads forward, so the loader has to come before the archive, and
    // both have to be in the file.
    if loader_range.offset < rkfw::HEADER_LEN as u64 || loader_range.len == 0 {
        return Err(refused(format!(
            "the RKFW header places the loader at byte {} for {} bytes, which is not after the \
             header",
            loader_range.offset, loader_range.len
        )));
    }
    if loader_range.len > MAX_LOADER_BYTES {
        return Err(refused(format!(
            "the RKFW header gives the loader {} bytes, more than the {MAX_LOADER_BYTES} a loader \
             container is",
            loader_range.len
        )));
    }
    if archive_range.offset < loader_end {
        return Err(refused(
            "the RKFW header places the archive before the end of the loader. pyrographer reads a \
             package front to back, loader first"
                .to_string(),
        ));
    }
    if archive_end > file_bytes {
        return Err(refused(format!(
            "the RKFW header places the archive's end at byte {archive_end}, and the file is \
             {file_bytes} bytes. The file is incomplete"
        )));
    }

    progress(Progress::Started {
        total_bytes: archive_end,
    });

    reader.skip_to(loader_range.offset).await?;
    let loader_bytes = reader.read_vec(loader_range.len).await?;
    let loader = rkboot::parse(&loader_bytes)?;
    let id_block = idb::build(&loader)?;

    reader.skip_to(archive_range.offset).await?;
    let archive_header = reader.read_vec(rkfw::ARCHIVE_HEADER_LEN as u64).await?;
    let archive = rkfw::parse_archive_header(&archive_header)?;
    if archive.length + rkfw::ARCHIVE_CRC_LEN as u64 > archive_range.len {
        return Err(refused(format!(
            "the RKAF header gives the archive {} bytes and its checksum four more, past the {} \
             bytes the RKFW header gives it",
            archive.length, archive_range.len
        )));
    }

    // What the pass keeps as it goes by: the parameter and the `bootloader` entry
    // whole, and the first bytes of every other entry with bytes of its own.
    let mut captures: Vec<Capture> = Vec::new();
    for (index, entry) in archive.entries.iter().enumerate() {
        if entry.is_self() || entry.size == 0 {
            continue;
        }
        let len = match entry.name.as_str() {
            "parameter" => {
                let most = (rkparam::HEADER_LEN + rkparam::MAX_TEXT_LEN + rkparam::CRC_LEN) as u64;
                if entry.size > most {
                    return Err(refused(format!(
                        "the parameter entry is {} bytes, past the {most} a parameter block is",
                        entry.size
                    )));
                }
                entry.size
            }
            "bootloader" => {
                if entry.size != loader_range.len {
                    return Err(refused(format!(
                        "the archive's bootloader entry is {} bytes and the loader the RKFW header \
                         points at is {}. Two loaders that disagree leave no way to say which one \
                         the package means",
                        entry.size, loader_range.len
                    )));
                }
                entry.size
            }
            _ => entry.size.min(SPARSE_MAGIC.len() as u64),
        };
        captures.push(Capture {
            entry: index,
            start: entry.position,
            bytes: Vec::with_capacity(len as usize),
            len,
        });
    }

    // The archive, into its checksum: the header already read, then the rest.
    let mut crc = crc32_rockchip_update(0, &archive_header);
    let mut at = rkfw::ARCHIVE_HEADER_LEN as u64;
    let mut window = vec![0u8; WINDOW_BYTES];
    while at < archive.length {
        if cancel.is_canceled() {
            return Err(Error::Canceled);
        }
        let n = (archive.length - at).min(WINDOW_BYTES as u64) as usize;
        let chunk = &mut window[..n];
        reader.read(chunk).await?;
        crc = crc32_rockchip_update(crc, chunk);
        for capture in &mut captures {
            capture.observe(at, chunk);
        }
        at += n as u64;
        progress(Progress::Advanced {
            done_bytes: archive_range.offset + at,
            total_bytes: archive_end,
        });
    }

    let stored = u32::from_le_bytes(
        reader
            .read_vec(rkfw::ARCHIVE_CRC_LEN as u64)
            .await?
            .try_into()
            .unwrap_or_default(),
    );
    if crc != stored {
        return Err(refused(format!(
            "the archive's checksum does not hold: its {} bytes give {crc:#010x}, and the \
             archive records {stored:#010x}. The file is damaged or incomplete",
            archive.length
        )));
    }

    // The trailer, where the file has one.
    let trailer = if archive_end + rkfw::TRAILER_LEN as u64 <= file_bytes {
        reader.skip_to(archive_end).await?;
        Some(reader.read_vec(rkfw::TRAILER_LEN as u64).await?)
    } else {
        None
    };
    progress(Progress::Finished {
        done_bytes: archive_end,
    });

    // Each entry, now that the bytes it needed have gone by.
    let mut parameter = None;
    let mut images: Vec<PartitionImage> = Vec::new();
    let mut skipped = Vec::new();
    for (index, entry) in archive.entries.iter().enumerate() {
        let captured = captures
            .iter()
            .find(|capture| capture.entry == index)
            .map(|capture| capture.bytes.as_slice());
        let skip = |why| Skipped {
            name: entry.name.clone(),
            path: entry.path.clone(),
            why,
        };

        if entry.is_self() {
            skipped.push(skip("the entry marks the package itself"));
        } else if entry.name == "parameter" {
            let block = captured.unwrap_or_default();
            parameter = Some(rkparam::text(block).map_err(|error| {
                refused(format!(
                    "its parameter entry is not an intact block: {error}"
                ))
            })?);
        } else if entry.name == "bootloader" {
            if captured.unwrap_or_default() != loader_bytes.as_slice() {
                return Err(refused(
                    "the archive's bootloader entry differs from the loader the RKFW header points \
                     at. Two loaders that disagree leave no way to say which one the package means"
                        .to_string(),
                ));
            }
        } else if entry.size == 0 {
            skipped.push(skip("the entry carries no bytes"));
        } else if entry.name == "package-file" {
            skipped.push(skip("the entry is the packing tool's own list of files"));
        } else {
            if captured.is_some_and(|first| first.starts_with(&SPARSE_MAGIC)) {
                return Err(refused(format!(
                    "partition image '{}' ({}) is an Android sparse image, which has to be \
                     expanded as it is written. pyrographer writes images raw, and does not expand \
                     one",
                    entry.name, entry.path
                )));
            }
            if images.iter().any(|image| image.name == entry.name) {
                return Err(refused(format!(
                    "two entries are named '{}', so the package carries two images for one \
                     partition",
                    entry.name
                )));
            }
            images.push(PartitionImage {
                name: entry.name.clone(),
                path: entry.path.clone(),
                offset: archive_range.offset + entry.position,
                bytes: entry.size,
            });
        }
    }

    let parameter = parameter.ok_or_else(|| {
        refused("the archive carries no parameter entry, so it names no partitions".to_string())
    })?;
    images.sort_by_key(|image| image.offset);

    Ok(Package {
        file_bytes,
        header,
        archive,
        loader,
        id_block,
        parameter,
        images,
        skipped,
        trailer,
    })
}

/// A loader read from a file, and where in the file it was found.
#[derive(Debug, Clone)]
pub struct FoundLoader {
    /// The loader container, parsed.
    pub loader: LoaderImage,
    /// Whether the file was a firmware package, and the loader the one inside it.
    pub in_package: bool,
}

/// Read a loader from `image`, `file_bytes` long: a loader container, or the
/// loader inside a firmware package.
///
/// A firmware package carries the loader it was built with. A caller that wants a
/// loader, to upload one or to build an ID block, can take the package itself.
/// Nobody then extracts the loader by hand. Only the package's header and its loader are
/// read, because a package is gigabytes. A file that is neither, or a container
/// larger than a loader is, is an [`Error::InvalidRequest`].
pub async fn read_loader(image: &mut dyn ImageReader, file_bytes: u64) -> Result<FoundLoader> {
    let mut reader = ForwardReader::new(image);
    let head = reader
        .read_vec(file_bytes.min(rkfw::HEADER_LEN as u64))
        .await?;

    if rkfw::identify(&head) == Some(rkfw::Container::Package) {
        let range = rkfw::parse_header(&head)?.loader;
        let end = range
            .end()
            .filter(|&end| end <= file_bytes)
            .ok_or_else(|| {
                refused(format!(
                    "the RKFW header places a {}-byte loader at byte {}, and the file ends at byte \
                 {file_bytes}",
                    range.len, range.offset
                ))
            })?;
        if range.len > MAX_LOADER_BYTES || range.offset < head.len() as u64 || end < range.offset {
            return Err(refused(format!(
                "the RKFW header places a {}-byte loader at byte {}, which is not a loader \
                 container's place or size",
                range.len, range.offset
            )));
        }
        reader.skip_to(range.offset).await?;
        let bytes = reader.read_vec(range.len).await?;
        return Ok(FoundLoader {
            loader: rkboot::parse(&bytes)?,
            in_package: true,
        });
    }

    if file_bytes > MAX_LOADER_BYTES {
        return Err(Error::InvalidRequest(format!(
            "loader file: the file is {file_bytes} bytes, more than a loader container is, and \
             it is not a firmware package"
        )));
    }
    let mut bytes = head;
    bytes.extend(reader.read_vec(file_bytes - bytes.len() as u64).await?);
    Ok(FoundLoader {
        loader: rkboot::parse(&bytes)?,
        in_package: false,
    })
}

/// A range of the archive the pass keeps as it goes by.
struct Capture {
    /// The entry it belongs to, by index.
    entry: usize,
    /// Where it begins, counted from the start of the archive.
    start: u64,
    /// How many bytes to keep.
    len: u64,
    /// The bytes kept so far.
    bytes: Vec<u8>,
}

impl Capture {
    /// Keep whatever part of `chunk`, which begins `at` bytes into the archive, this
    /// capture covers.
    fn observe(&mut self, at: u64, chunk: &[u8]) {
        let end = self.start + self.len;
        let chunk_end = at + chunk.len() as u64;
        let from = self.start.max(at);
        let to = end.min(chunk_end);
        if from < to {
            self.bytes
                .extend_from_slice(&chunk[(from - at) as usize..(to - at) as usize]);
        }
    }
}

/// An [`ImageReader`] that knows how far it has read, and can move forward.
///
/// The image seam is a stream with no seek. A package is read front to back, so a
/// forward move is enough: [`skip_to`](Self::skip_to) reads and discards up to an
/// offset. It refuses to go back, because a stream cannot.
pub struct ForwardReader<'a> {
    image: &'a mut dyn ImageReader,
    at: u64,
}

impl<'a> ForwardReader<'a> {
    /// A reader at the start of `image`.
    pub fn new(image: &'a mut dyn ImageReader) -> Self {
        Self { image, at: 0 }
    }

    /// How many bytes have been read so far.
    pub fn position(&self) -> u64 {
        self.at
    }

    /// Read and discard up to `offset`.
    ///
    /// An offset behind the reader is an [`Error::InvalidRequest`]: the stream has
    /// already gone past it.
    pub async fn skip_to(&mut self, offset: u64) -> Result<()> {
        if offset < self.at {
            return Err(Error::InvalidRequest(format!(
                "the image is read front to back, and byte {offset} is behind byte {}, where the \
                 read has reached",
                self.at
            )));
        }
        let mut discard = vec![0u8; WINDOW_BYTES.min((offset - self.at) as usize)];
        while self.at < offset {
            let n = (offset - self.at).min(discard.len() as u64) as usize;
            self.read(&mut discard[..n]).await?;
        }
        Ok(())
    }

    /// Fill `buf` from the stream.
    async fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        let at = self.at;
        self.image.read_exact(buf).await.map_err(|error| {
            let cause = match error {
                Error::Io(message) => message,
                other => other.to_string(),
            };
            Error::Io(format!(
                "cannot read the firmware package at byte {at}: {cause}"
            ))
        })?;
        self.at += buf.len() as u64;
        Ok(())
    }

    /// Read `len` bytes into a new buffer.
    async fn read_vec(&mut self, len: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        self.read(&mut buf).await?;
        Ok(buf)
    }
}

impl ImageReader for ForwardReader<'_> {
    fn read_exact<'b>(&'b mut self, buf: &'b mut [u8]) -> BoxFuture<'b, Result<()>> {
        Box::pin(async move {
            self.image.read_exact(buf).await?;
            self.at += buf.len() as u64;
            Ok(())
        })
    }
}

/// Wrap a reason as the package's refusal.
fn refused(detail: String) -> Error {
    Error::InvalidRequest(format!("firmware package: {detail}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::image::SyncReader;
    use crate::testing::{PackageEntry as Entry, firmware_package as package};

    pub(crate) fn a_loader() -> Vec<u8> {
        crate::testing::loader_container()
    }

    pub(crate) fn a_parameter() -> Vec<u8> {
        crate::testing::sdk_parameter()
    }

    pub(crate) fn a_package() -> Vec<u8> {
        crate::testing::firmware_package_for_a_board()
    }

    /// Read `file` through the seam, as a front-end does.
    pub(crate) fn read_package(file: &[u8]) -> Result<Package> {
        let mut reader = SyncReader::new(file);
        pollster::block_on(read(
            &mut reader,
            file.len() as u64,
            &mut |_| {},
            &Cancel::new(),
        ))
    }

    /// A whole package reads back: the loader, the ID block built from it, the
    /// parameter's text, each partition image where it sits, and what is not
    /// written with the reason.
    #[test]
    fn a_package_reads_and_checks_in_one_pass() {
        let file = a_package();
        let package = read_package(&file).expect("the package is intact");

        assert_eq!(package.archive.model, "RK3576");
        assert_eq!(package.id_block.sectors(), 16);
        assert!(package.parameter.contains("TYPE: GPT"));
        let names: Vec<&str> = package.images.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["uboot", "boot"]);
        let uboot = &package.images[0];
        let at = uboot.offset as usize;
        assert!(file[at..at + 5000].iter().all(|&b| b == 0x55));
        assert_eq!(package.skipped.len(), 1);
        assert_eq!(package.skipped[0].name, "package-file");
        assert_eq!(
            package.trailer_text(),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    /// One flipped byte anywhere in the archive fails its checksum, and the
    /// package is refused before anything is planned.
    #[test]
    fn a_damaged_archive_fails_its_checksum() {
        let mut file = a_package();
        let header = rkfw::parse_header(&file).expect("package");
        let inside = (header.archive.offset + 2048 + 100) as usize;
        file[inside] ^= 1;
        let err = read_package(&file).expect_err("damaged");
        assert!(err.to_string().contains("checksum does not hold"), "{err}");
    }

    /// A file cut short is refused as incomplete, not read past its end.
    #[test]
    fn a_truncated_package_is_refused() {
        let file = a_package();
        let err = read_package(&file[..file.len() - 100]).expect_err("truncated");
        assert!(err.to_string().contains("incomplete"), "{err}");
    }

    /// A `bootloader` entry that differs from the RKFW loader is refused.
    #[test]
    fn two_loaders_that_disagree_are_refused() {
        let loader = a_loader();
        let mut other = loader.clone();
        let last = other.len() - 1;
        other[last] ^= 1;
        let file = package(
            &loader,
            &[
                Entry {
                    name: "parameter",
                    path: "p",
                    data: a_parameter(),
                },
                Entry {
                    name: "bootloader",
                    path: "b",
                    data: other,
                },
            ],
        );
        let err = read_package(&file).expect_err("two loaders");
        assert!(
            err.to_string().contains("bootloader entry differs"),
            "{err}"
        );
    }

    /// An Android sparse image is refused by name rather than written raw.
    #[test]
    fn a_sparse_image_is_refused() {
        let mut sparse = vec![0u8; 100];
        sparse[..4].copy_from_slice(&SPARSE_MAGIC);
        let file = package(
            &a_loader(),
            &[
                Entry {
                    name: "parameter",
                    path: "p",
                    data: a_parameter(),
                },
                Entry {
                    name: "super",
                    path: "Image/super.img",
                    data: sparse,
                },
            ],
        );
        let err = read_package(&file).expect_err("sparse");
        assert!(err.to_string().contains("sparse"), "{err}");
    }

    /// A package with no parameter names no partitions.
    #[test]
    fn a_package_without_a_parameter_is_refused() {
        let file = package(
            &a_loader(),
            &[Entry {
                name: "boot",
                path: "b",
                data: vec![1; 10],
            }],
        );
        let err = read_package(&file).expect_err("no parameter");
        assert!(err.to_string().contains("no parameter"), "{err}");
    }

    /// The pass stops between windows once canceled.
    #[test]
    fn a_canceled_read_stops() {
        let file = a_package();
        let cancel = Cancel::new();
        cancel.cancel();
        let mut reader = SyncReader::new(file.as_slice());
        let outcome =
            pollster::block_on(read(&mut reader, file.len() as u64, &mut |_| {}, &cancel));
        assert!(matches!(outcome, Err(Error::Canceled)), "{outcome:?}");
    }

    /// The layout is the parameter's, with true LBAs, the growing partition ending
    /// at the GPT's last usable sector, and rootfs's GUID pinned.
    #[test]
    fn the_layout_is_the_parameters_gpt() {
        let package = read_package(&a_package()).expect("intact");
        let flash_sectors = 0x10_0000;
        let layout = package.layout(flash_sectors).expect("a GPT parameter");

        let parts: Vec<(&str, u64, u64)> = layout
            .partitions
            .iter()
            .map(|p| (p.name.as_str(), p.first_lba, p.sectors))
            .collect();
        let rootfs_end = flash_sectors - 34 + 1;
        assert_eq!(
            parts,
            [
                ("uboot", 0x4000, 0x2000),
                ("boot", 0x6000, 0x10000),
                ("rootfs", 0x16000, rootfs_end - 0x16000),
            ]
        );
        assert_eq!(
            layout.partitions[2].unique_guid.as_deref(),
            Some("614e0000-0000-4b53-8000-1d28000054a9")
        );
        assert_eq!(layout.partitions[0].unique_guid, None);
    }

    /// A parameter without `TYPE: GPT` is the legacy kind, refused by name.
    #[test]
    fn a_legacy_parameter_is_refused() {
        let mut package = read_package(&a_package()).expect("intact");
        package.parameter = package.parameter.replace("TYPE: GPT\n", "");
        let err = package.layout(0x10_0000).expect_err("legacy");
        assert!(err.to_string().contains("TYPE: GPT"), "{err}");
    }

    /// A `uuid:` line naming no partition is refused rather than dropped.
    #[test]
    fn a_guid_pinned_for_no_partition_is_refused() {
        let mut package = read_package(&a_package()).expect("intact");
        package.parameter = package.parameter.replace("uuid:rootfs", "uuid:system");
        let err = package.layout(0x10_0000).expect_err("no such partition");
        assert!(err.to_string().contains("'system'"), "{err}");
    }

    /// The forward reader moves forward and refuses to go back.
    #[test]
    fn the_forward_reader_skips_forward_only() {
        let bytes: Vec<u8> = (0..=255).collect();
        let mut source = SyncReader::new(bytes.as_slice());
        let mut reader = ForwardReader::new(&mut source);
        pollster::block_on(reader.skip_to(100)).expect("forward");
        let mut one = [0u8; 1];
        pollster::block_on(reader.read_exact(&mut one)).expect("reads");
        assert_eq!(one[0], 100);
        assert_eq!(reader.position(), 101);
        assert!(pollster::block_on(reader.skip_to(50)).is_err());
    }

    /// A loader comes out of a bare container, and out of a package, the same; a
    /// file that is neither is refused.
    #[test]
    fn a_loader_is_read_from_a_container_or_from_inside_a_package() {
        let read = |file: &[u8]| {
            let mut reader = SyncReader::new(file);
            pollster::block_on(read_loader(&mut reader, file.len() as u64))
        };
        let container = a_loader();
        let bare = read(&container).expect("a container");
        assert!(!bare.in_package);
        let packaged = read(&a_package()).expect("a package");
        assert!(packaged.in_package);
        assert_eq!(
            bare.loader.flash_stages.len(),
            packaged.loader.flash_stages.len()
        );
        assert_eq!(
            bare.loader.flash_stages[0].data,
            packaged.loader.flash_stages[0].data
        );
        assert!(read(b"neither a loader nor a package").is_err());
    }
}
