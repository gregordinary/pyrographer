//! Sans-I/O codecs: pure byte layouts with no transport.
//!
//! These build and parse byte layouts and perform no I/O. They are therefore
//! unit-tested by asserting exact bytes, with no hardware attached. The module
//! groups them by that property, not by the kind of layout they describe.
//!
//! # Wire formats
//!
//! [`bot`] is the vendor-neutral USB Mass-Storage envelope, and [`rockusb`] is the
//! Rockchip command block carried inside it. [`dfu`] is the standard USB DFU 1.1
//! class protocol an Ingenic board speaks once a DFU-capable U-Boot is running. DFU
//! uses control transfers to an interface, not a bulk pair. [`dfu`] therefore frames
//! SETUP fields rather than a command block.
//!
//! # On-flash formats
//!
//! [`gpt`] is the UEFI GUID Partition Table, and [`rkparam`] is Rockchip's own
//! parameter block. A device stores these, and they are read back through the block
//! interface like any other sectors. They are parsed here for the same reason the
//! wire formats are: they have an endianness and a checksum, and testing them needs
//! no device.
//!
//! [`dfu_alt`] is the third partition source, a DFU board's own alt-settings. The
//! device reports these as interface strings and does not store them on the flash.
//! This codec therefore parses a name and an optional range. It also owns the LBA
//! packing that lets a set of alt-settings share the verbs' one flat address space.
//!
//! # Bootstrap
//!
//! [`rkboot`] is the loader container `db` takes, and the payload the maskrom
//! download-boot uploads from it: verbatim section bytes plus a trailing CRC-16.
//! The upload sends stored bytes, and does not use [`rc4`].
//!
//! # The ID block and the firmware package
//!
//! [`idb`] lays out the ID block a BootROM reads from sector 64, from a loader
//! container's flash stages. It unscrambles them with [`rc4`], places each image
//! where its `RKNS` header says, and checks every [`sha256`] the header records.
//! [`rkfw`] is the firmware package an SDK build ends in (`update.img`): an `RKFW`
//! header around an `RKAF` archive of partition images.
//!
//! [`ingenic_boot`] is the XBurst boot ROM's `VR_*` vendor command layer. Its six
//! endpoint-0 control requests upload a DRAM-init stage and a DFU-capable U-Boot
//! into memory and jump to it. The board then re-enumerates as a DFU gadget and
//! speaks [`dfu`]. Like [`rkboot`], this layer ends once the board is in a state a
//! flash agent can drive.
//!
//! # StarFive recovery
//!
//! [`xmodem`] is the block the JH7110 BootROM, its recovery agent and a U-Boot SPL
//! receive over UART, and the two YMODEM batch blocks. [`splhdr`] is the 1024-byte
//! header the ROM requires on the first image it loads.
//!
//! # Console
//!
//! [`console`] is the accumulator and byte-oriented matcher a serial console is read
//! through. It holds a transcript, a cursor into it, and the rule that the earliest
//! occurrence in the stream wins. Console output carries no framing, so this codec
//! builds none. It owns the search, which is the part of reading a console that a
//! test can exercise without a board.
//!
//! # Checksums
//!
//! [`crc`] is the checksum arithmetic for the on-flash tables, the maskrom payload,
//! the firmware package's archive, and the StarFive header and blocks. Its four
//! checksums are defined together because they are easy to confuse. Two CRC-32s
//! have polynomials one bit apart, and two CRC-16s differ only in their seed.
//! [`sha256`] is the hash an ID block's header names its images by.

pub mod bot;
pub mod console;
pub mod crc;
pub mod dfu;
pub mod dfu_alt;
pub mod gpt;
pub mod idb;
pub mod ingenic_boot;
pub mod rc4;
pub mod rkboot;
pub mod rkfw;
pub mod rkparam;
pub mod rockusb;
pub mod sha256;
pub mod splhdr;
pub mod xmodem;
