//! XMODEM framing: the control bytes and the data block that the JH7110 BootROM
//! and its recovery agent receive over UART.
//!
//! The JH7110 recovery link is XMODEM-CRC end to end. The BootROM, and the agent
//! it loads, are the *receivers*, and pyrographer is the *sender*. This module is
//! the pure byte layout of what the sender puts on the wire: the control-byte
//! alphabet and the data block.
//!
//! [`recovery`](crate::recovery) implements the sender's state machine. It waits
//! for the receiver's `C`, sends the blocks, and ends with `EOT`. It resends a
//! block on `NAK`, and tolerates the ROM's `NAK` bursts. The division follows the
//! one between [`rkboot`](super::rkboot) for the bytes and
//! [`bootstrap`](crate::bootstrap) for the I/O.
//!
//! A block is built sans-I/O, so tests pin its framing byte for byte with no serial
//! port attached. The CRC is [`crc16_xmodem`], CRC-16/XMODEM with polynomial
//! `0x1021`, the polynomial the Rockchip maskrom trailer also uses. It is
//! transmitted **big-endian, high byte first**, the opposite of the order a sender
//! with a little-endian habit writes. **\[DOC\]**
//!
//! # Block size
//!
//! The recovery-agent path is driven with 128-byte (`SOH`) blocks. The receiver's
//! `C` guarantees CRC-16 mode. It does not guarantee that the closed-source agent
//! accepts 1 KB (`STX`) blocks. Uploaders known to work with this agent use
//! 128-byte blocks. Whether the agent takes 1 KB blocks is **\[UNVERIFIED\]**.
//!
//! This module therefore frames only the 128-byte block. The 1 KB block and the
//! YMODEM block-0 header belong to the direct-SPL and running-U-Boot paths, which
//! pyrographer does not drive.

use crate::codec::crc::crc16_xmodem;

/// `SOH`: begins a 128-byte data block.
pub const SOH: u8 = 0x01;
/// `STX`: begins a 1024-byte data block. It is defined so that bytes read from a
/// wire can be matched against it. This module does not frame a 1024-byte block
/// (see the module documentation).
pub const STX: u8 = 0x02;
/// `EOT`: end of transfer. The sender sends it after the last block, and the
/// receiver answers [`ACK`].
pub const EOT: u8 = 0x04;
/// `ACK`: the receiver accepted the last block (or the `EOT`).
pub const ACK: u8 = 0x06;
/// `NAK`: the receiver wants the last block again. On this ROM, it also arrives
/// unsolicited and in bursts. The sender in [`recovery`](crate::recovery) tolerates
/// the bursts, and does not treat each `NAK` as a retransmit request.
pub const NAK: u8 = 0x15;
/// `CAN`: cancel. Two in a row conventionally abort a transfer.
pub const CAN: u8 = 0x18;
/// `C` (`0x43`): the receiver's request for CRC-16 mode. The ROM streams these
/// while it waits for the first block. The sender begins on the first one, and
/// discards the rest.
pub const CRC_REQUEST: u8 = b'C';
/// `SUB` (`0x1A`): the pad byte. The final block is padded out to a full 128
/// bytes with it.
pub const PAD: u8 = 0x1A;

/// The data payload of one 128-byte block.
pub const BLOCK_DATA: usize = 128;

/// The whole 128-byte block on the wire: a three-byte header of `SOH`, the
/// sequence number and its complement, then 128 data bytes and the CRC-16.
pub const BLOCK_LEN: usize = 1 + 1 + 1 + BLOCK_DATA + 2;

/// Build one 128-byte XMODEM-CRC block for sequence number `seq`.
///
/// The block is `SOH`, `seq`, `255 - seq`, then 128 data bytes, then the
/// CRC-16/XMODEM of those 128 bytes, high byte first. `chunk` supplies the data. A
/// `chunk` shorter than 128 bytes is padded to a full block with [`PAD`], and only
/// the last chunk is shorter. The CRC covers the padding, because it is computed
/// over the bytes sent.
///
/// The caller advances the sequence number. XMODEM numbers blocks from 1 and wraps
/// at 256, so it is a `u8`, and the complement is `255 - seq`. The
/// [`recovery`](crate::recovery) sender owns the counter and the wrap.
///
/// # Panics
///
/// Panics on a `chunk` longer than [`BLOCK_DATA`]. The sender chunks the image by
/// exactly that size, so a longer chunk is a programming error, not a runtime
/// condition. The panic in [`rc4`](super::rc4::rc4) on an empty key is the same
/// kind of error.
pub fn block(seq: u8, chunk: &[u8]) -> Vec<u8> {
    assert!(
        chunk.len() <= BLOCK_DATA,
        "an XMODEM block carries {BLOCK_DATA} data bytes, and this chunk has {}",
        chunk.len()
    );

    let mut out = Vec::with_capacity(BLOCK_LEN);
    out.push(SOH);
    out.push(seq);
    out.push(255 - seq);

    let start = out.len();
    out.extend_from_slice(chunk);
    out.resize(start + BLOCK_DATA, PAD);

    let crc = crc16_xmodem(&out[start..start + BLOCK_DATA]);
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
        let data = [0xa5u8; BLOCK_DATA];
        let out = block(1, &data);

        assert_eq!(out.len(), BLOCK_LEN);
        assert_eq!(out[0], SOH);
        assert_eq!(out[1], 1, "sequence number");
        assert_eq!(out[2], 254, "its ones' complement");
        assert_eq!(&out[3..3 + BLOCK_DATA], &data[..]);

        let crc = crc16_xmodem(&data);
        assert_eq!(
            &out[3 + BLOCK_DATA..],
            &crc.to_be_bytes(),
            "the CRC is transmitted high byte first"
        );
    }

    /// A short final chunk is padded to a full 128 bytes with SUB. The CRC covers the
    /// padding, because the padding is on the wire. An implementation that computed
    /// the CRC over the real bytes alone, and sent the padding, would give the
    /// receiver a checksum it cannot reproduce.
    #[test]
    fn a_short_final_chunk_is_padded_with_sub_and_the_crc_covers_it() {
        let chunk = [0x11u8, 0x22, 0x33];
        let out = block(7, &chunk);

        assert_eq!(out.len(), BLOCK_LEN);
        assert_eq!(&out[3..6], &chunk[..]);
        assert!(
            out[6..3 + BLOCK_DATA].iter().all(|&b| b == PAD),
            "the tail is padded with SUB"
        );

        let mut framed = chunk.to_vec();
        framed.resize(BLOCK_DATA, PAD);
        let crc = crc16_xmodem(&framed);
        assert_eq!(&out[3 + BLOCK_DATA..], &crc.to_be_bytes());
    }

    /// The sequence byte and its complement always sum to 255. The receiver checks
    /// that sum to confirm that the block number arrived intact. The sum must hold
    /// across the wrap at 256, where the number is 0 and the complement 255.
    #[test]
    fn the_sequence_and_its_complement_sum_to_255_across_the_wrap() {
        for seq in [1u8, 2, 128, 255, 0] {
            let out = block(seq, &[0u8; BLOCK_DATA]);
            assert_eq!(out[1], seq);
            assert_eq!(u16::from(out[1]) + u16::from(out[2]), 255);
        }
    }

    /// An all-zero data block has a zero CRC-16 (init zero, no final xor), so both of
    /// its CRC bytes are zero. That value reads the same in either byte order, so
    /// this test pins the CRC itself apart from the byte-order question.
    #[test]
    fn an_all_zero_block_carries_a_zero_crc() {
        let out = block(1, &[0u8; BLOCK_DATA]);
        assert_eq!(&out[3 + BLOCK_DATA..], &[0x00, 0x00]);
    }

    #[test]
    #[should_panic(expected = "128 data bytes")]
    fn a_chunk_larger_than_a_block_is_a_programming_error() {
        block(1, &[0u8; BLOCK_DATA + 1]);
    }
}
