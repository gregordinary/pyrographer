//! XMODEM and YMODEM framing: the control bytes and the blocks that the JH7110
//! BootROM, its recovery agent, and a U-Boot SPL receive over UART.
//!
//! Every JH7110 serial path is a file sent to a receiver. The BootROM, the recovery
//! agent it loads, and a mainline SPL are the *receivers*, and pyrographer is the
//! *sender*. This module is the pure byte layout of what the sender puts on the
//! wire. That is the control-byte alphabet, the data block in both sizes, and the
//! two YMODEM batch blocks.
//!
//! The sender's state machine is the I/O half, in [`modem`](crate::modem). It
//! waits for the receiver's start, sends the blocks, and ends with `EOT`. It resends
//! a block on request, and tolerates the ROM's `NAK` bursts. The division follows
//! the one between [`rkboot`](super::rkboot) for the bytes and
//! [`bootstrap`](crate::bootstrap) for the I/O.
//!
//! A block is built sans-I/O, so tests pin its framing byte for byte with no serial
//! port attached. The CRC is [`crc16_xmodem`], CRC-16/XMODEM with polynomial
//! `0x1021` and a zero seed. It is transmitted **big-endian, high byte first**, the
//! opposite of the order a sender with a little-endian habit writes. **\[DOC\]**
//!
//! # Block sizes
//!
//! A block carries 128 bytes after `SOH` or 1024 bytes after `STX`. The BootROM,
//! the recovery agents and U-Boot's receiver accept both, block by block.
//! [`modem`](crate::modem) sends 128-byte blocks to the ROM and to the recovery
//! agent, and 1024-byte blocks to an SPL by YMODEM. The module documentation there
//! says why.
//!
//! # YMODEM
//!
//! YMODEM is XMODEM with a block numbered 0 before the file, which carries the
//! file's name and length. A second, empty block 0 after the file ends the batch.
//! [`ymodem_header`] and [`ymodem_end`] build the two. A receiver that knows the
//! length drops the padding in the last data block, so the file arrives at its own
//! size.

use crate::codec::crc::crc16_xmodem;

/// `SOH`: begins a 128-byte data block.
pub const SOH: u8 = 0x01;
/// `STX`: begins a 1024-byte data block.
pub const STX: u8 = 0x02;
/// `EOT`: end of transfer. The sender sends it after the last block, and the
/// receiver answers [`ACK`].
pub const EOT: u8 = 0x04;
/// `ACK`: the receiver accepted the last block (or the `EOT`).
pub const ACK: u8 = 0x06;
/// `NAK`: the receiver wants the last block again. On the JH7110 ROM, it also
/// arrives unsolicited and in bursts. The sender in [`modem`](crate::modem)
/// tolerates the bursts, and does not treat each `NAK` as a retransmit request.
pub const NAK: u8 = 0x15;
/// `CAN`: cancel. Two in a row conventionally abort a transfer.
pub const CAN: u8 = 0x18;
/// `C` (`0x43`): the receiver's request for CRC-16 mode. A receiver streams these
/// while it waits for the first block. U-Boot's receiver also sends one to ask for
/// a block again, where another receiver sends a [`NAK`].
pub const CRC_REQUEST: u8 = b'C';
/// `SUB` (`0x1A`): the pad byte. The final block is padded out to its full size
/// with it.
pub const PAD: u8 = 0x1A;

/// How much data one block carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSize {
    /// 128 bytes, after [`SOH`]: the block of the original XMODEM.
    Small,
    /// 1024 bytes, after [`STX`]: the block of XMODEM-1K and YMODEM.
    Large,
}

impl BlockSize {
    /// How many data bytes a block of this size carries.
    pub const fn data_len(self) -> usize {
        match self {
            BlockSize::Small => 128,
            BlockSize::Large => 1024,
        }
    }

    /// The byte that begins a block of this size.
    pub const fn start(self) -> u8 {
        match self {
            BlockSize::Small => SOH,
            BlockSize::Large => STX,
        }
    }

    /// How many bytes a block of this size occupies on the wire.
    ///
    /// A block is a three-byte header, the data and a two-byte CRC. The header is
    /// the start byte, the sequence number and its complement.
    pub const fn wire_len(self) -> usize {
        3 + self.data_len() + 2
    }
}

/// The longest file name [`ymodem_header`] carries.
///
/// Block 0 holds the name, a NUL, the length in decimal and a second NUL, in 128
/// bytes. A `u64` length is at most 20 digits, so this bound always leaves room for
/// it.
pub const YMODEM_NAME_MAX: usize = 128 - 1 - 20 - 1;

/// Build one XMODEM-CRC block of `size` for sequence number `seq`.
///
/// The block is the start byte, `seq`, `255 - seq`, then the data, then the
/// CRC-16/XMODEM of the data, high byte first. `chunk` supplies the data. A `chunk`
/// shorter than the block is padded to its full size with [`PAD`], and only the
/// last chunk of a file is shorter. The CRC covers the padding, because it is
/// computed over the bytes sent.
///
/// The caller advances the sequence number. XMODEM numbers data blocks from 1 and
/// wraps at 256, so it is a `u8`, and the complement is `255 - seq`. The
/// [`modem`](crate::modem) sender owns the counter and the wrap.
///
/// # Panics
///
/// Panics on a `chunk` longer than the block. The sender chunks a file by exactly
/// that size, so a longer chunk is a programming error, not a runtime condition.
/// The panic in [`rc4`](super::rc4::rc4) on an empty key is the same kind of
/// error.
pub fn block(size: BlockSize, seq: u8, chunk: &[u8]) -> Vec<u8> {
    framed(size, seq, chunk, PAD)
}

/// Build YMODEM's block 0 for a file: its name and its length.
///
/// The block is a 128-byte block numbered 0, holding the name, a NUL, the length in
/// decimal, a NUL, and NULs to the end. The receiver takes the length from it and
/// drops the padding of the last data block. U-Boot's receiver skips the name, so
/// the name only has to be well formed.
///
/// # Panics
///
/// Panics on a name longer than [`YMODEM_NAME_MAX`] bytes, or one that contains a
/// NUL. The sender names the file itself, so either is a programming error.
pub fn ymodem_header(name: &str, len: u64) -> Vec<u8> {
    assert!(
        name.len() <= YMODEM_NAME_MAX && !name.contains('\0'),
        "a YMODEM file name is at most {YMODEM_NAME_MAX} bytes with no NUL, and this one is \
         {name:?}"
    );
    let mut data = Vec::with_capacity(BlockSize::Small.data_len());
    data.extend_from_slice(name.as_bytes());
    data.push(0);
    data.extend_from_slice(len.to_string().as_bytes());
    data.push(0);
    framed(BlockSize::Small, 0, &data, 0)
}

/// Build the empty block 0 that ends a YMODEM batch.
///
/// After a file's `EOT`, the receiver asks for the next file's block 0. A block 0
/// of NULs names no file, which says that the batch is over.
pub fn ymodem_end() -> Vec<u8> {
    framed(BlockSize::Small, 0, &[], 0)
}

/// Frame `chunk` as a block of `size` numbered `seq`, padding with `fill`.
fn framed(size: BlockSize, seq: u8, chunk: &[u8], fill: u8) -> Vec<u8> {
    let data_len = size.data_len();
    assert!(
        chunk.len() <= data_len,
        "an XMODEM block of this size carries {data_len} data bytes, and this chunk has {}",
        chunk.len()
    );

    let mut out = Vec::with_capacity(size.wire_len());
    out.push(size.start());
    out.push(seq);
    out.push(255 - seq);

    let start = out.len();
    out.extend_from_slice(chunk);
    out.resize(start + data_len, fill);

    let crc = crc16_xmodem(&out[start..start + data_len]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block's layout: three header bytes, 128 data bytes and two CRC bytes, in
    /// that order. A sender that swapped the sequence and its complement would still
    /// produce 133 bytes. So would a sender that put the CRC low byte first. The test
    /// therefore asserts each field, not only the length.
    #[test]
    fn a_block_is_soh_seq_complement_data_and_a_big_endian_crc() {
        let data = [0xa5u8; 128];
        let out = block(BlockSize::Small, 1, &data);

        assert_eq!(out.len(), 133);
        assert_eq!(out.len(), BlockSize::Small.wire_len());
        assert_eq!(out[0], SOH);
        assert_eq!(out[1], 1, "sequence number");
        assert_eq!(out[2], 254, "its ones' complement");
        assert_eq!(&out[3..131], &data[..]);

        let crc = crc16_xmodem(&data);
        assert_eq!(
            &out[131..],
            &crc.to_be_bytes(),
            "the CRC is transmitted high byte first"
        );
    }

    /// A 1 KB block is the same shape with `STX` in front and 1024 data bytes. The
    /// CRC covers all 1024.
    #[test]
    fn a_large_block_is_stx_and_1024_data_bytes() {
        let data: Vec<u8> = (0..1024u32).map(|i| (i * 3 + 1) as u8).collect();
        let out = block(BlockSize::Large, 9, &data);

        assert_eq!(out.len(), 1029);
        assert_eq!(out.len(), BlockSize::Large.wire_len());
        assert_eq!(out[0], STX);
        assert_eq!((out[1], out[2]), (9, 246));
        assert_eq!(&out[3..1027], &data[..]);
        assert_eq!(&out[1027..], &crc16_xmodem(&data).to_be_bytes());
    }

    /// A short final chunk is padded to a full block with SUB. The CRC covers the
    /// padding, because the padding is on the wire. An implementation that computed
    /// the CRC over the real bytes alone, and sent the padding, would give the
    /// receiver a checksum it cannot reproduce.
    #[test]
    fn a_short_final_chunk_is_padded_with_sub_and_the_crc_covers_it() {
        let chunk = [0x11u8, 0x22, 0x33];
        for size in [BlockSize::Small, BlockSize::Large] {
            let out = block(size, 7, &chunk);
            let data_end = 3 + size.data_len();

            assert_eq!(out.len(), size.wire_len());
            assert_eq!(&out[3..6], &chunk[..]);
            assert!(
                out[6..data_end].iter().all(|&b| b == PAD),
                "the tail is padded with SUB"
            );

            let mut framed = chunk.to_vec();
            framed.resize(size.data_len(), PAD);
            assert_eq!(&out[data_end..], &crc16_xmodem(&framed).to_be_bytes());
        }
    }

    /// The sequence byte and its complement always sum to 255. The receiver checks
    /// that sum to confirm that the block number arrived intact. The sum must hold
    /// across the wrap at 256, where the number is 0 and the complement 255.
    #[test]
    fn the_sequence_and_its_complement_sum_to_255_across_the_wrap() {
        for seq in [1u8, 2, 128, 255, 0] {
            let out = block(BlockSize::Small, seq, &[0u8; 128]);
            assert_eq!(out[1], seq);
            assert_eq!(u16::from(out[1]) + u16::from(out[2]), 255);
        }
    }

    /// An all-zero data block has a zero CRC-16 (init zero, no final xor), so both of
    /// its CRC bytes are zero. That value reads the same in either byte order, so
    /// this test pins the CRC itself apart from the byte-order question.
    #[test]
    fn an_all_zero_block_carries_a_zero_crc() {
        let out = block(BlockSize::Small, 1, &[0u8; 128]);
        assert_eq!(&out[131..], &[0x00, 0x00]);
    }

    /// YMODEM's block 0 is a 128-byte block numbered 0, with the name, a NUL, the
    /// length in decimal and a NUL, padded with NULs rather than SUB. U-Boot's
    /// receiver reads the length with a number parser that stops at the NUL. Padding
    /// with SUB would put bytes after the length that no parser expects.
    #[test]
    fn a_ymodem_header_names_the_file_and_its_length_in_decimal() {
        let out = ymodem_header("u-boot.itb", 1_234_567);

        assert_eq!(out.len(), 133);
        assert_eq!((out[0], out[1], out[2]), (SOH, 0, 255));
        let data = &out[3..131];
        let expected = b"u-boot.itb\x001234567\x00";
        assert_eq!(&data[..expected.len()], &expected[..]);
        assert!(
            data[expected.len()..].iter().all(|&b| b == 0),
            "block 0 is padded with NUL"
        );
        assert_eq!(&out[131..], &crc16_xmodem(data).to_be_bytes());
    }

    /// The batch ends with a block 0 of NULs, whose CRC is therefore zero.
    #[test]
    fn the_ymodem_end_block_is_an_empty_block_0() {
        let out = ymodem_end();
        assert_eq!((out[0], out[1], out[2]), (SOH, 0, 255));
        assert!(out[3..].iter().all(|&b| b == 0), "{out:?}");
    }

    #[test]
    #[should_panic(expected = "128 data bytes")]
    fn a_chunk_larger_than_a_block_is_a_programming_error() {
        block(BlockSize::Small, 1, &[0u8; 129]);
    }

    #[test]
    #[should_panic(expected = "YMODEM file name")]
    fn a_ymodem_name_too_long_for_block_0_is_a_programming_error() {
        ymodem_header(&"x".repeat(YMODEM_NAME_MAX + 1), 1);
    }
}
