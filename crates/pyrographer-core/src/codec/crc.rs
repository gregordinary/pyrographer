//! The checksums the codecs' formats carry.
//!
//! Two of them are called CRC-32, and they are not interchangeable. [`crc32`] is
//! the standard one, which [`gpt`] uses because the UEFI specification requires
//! it. [`crc32_rockchip`] is Rockchip's own, which [`rkparam`] uses because
//! Rockchip's tools use it. It differs from the standard one by **one bit of one
//! constant**. A table checked with the wrong one is reported as corrupt, with no
//! visible cause. A table written with the wrong one carries a checksum the board's
//! own bootloader rejects.
//!
//! The two are defined together because the likely error is a call to "the CRC
//! function" that picks the wrong one.
//!
//! The two CRC-16s carry the same risk in a 16-bit register. [`crc16_ccitt_false`]
//! is the `0xFFFF`-seeded trailer Rockchip's maskrom download-boot checks per code
//! section, as [`rkboot`] builds it. [`crc16_xmodem`] is its zero-seeded sibling,
//! the per-block checksum of the XMODEM protocol StarFive recovery receives, as
//! [`xmodem`] frames it. They share a polynomial and differ in one seed, and each
//! receiver silently rejects the other's checksums. They are defined together for
//! the same reason.
//!
//! This module is pure arithmetic: it performs no I/O, and it does not interpret
//! the bytes it is given.
//!
//! [`gpt`]: super::gpt
//! [`rkparam`]: super::rkparam
//! [`rkboot`]: super::rkboot
//! [`xmodem`]: super::xmodem

/// The reflected form of the standard CRC-32 polynomial `0x04C11DB7`.
///
/// The bit-reversed constant pairs with a right-shifting loop. That pairing makes
/// [`crc32`] the *reflected* algorithm: bits enter at the top of the register and
/// leave at the bottom.
const POLYNOMIAL: u32 = 0xedb8_8320;

/// Rockchip's polynomial: `0x04C10DB7`.
///
/// The value is intended, and it is not the standard `0x04C11DB7`. **Bit 12
/// differs, and no other bit does.** It is used unreflected, MSB-first. It
/// therefore appears here in its plain form, while [`POLYNOMIAL`] appears reversed.
///
/// rkdeveloptool and rkflashtool both carry it. rkdeveloptool's table is
/// introduced by the comment `crc32 factor 0x04C10DB7`, and rkflashtool's
/// unrolled loop shifts against the same constant. **\[DOC\]**
const POLYNOMIAL_ROCKCHIP: u32 = 0x04c1_0db7;

/// CRC-32/ISO-HDLC over `bytes`: the checksum a UEFI GPT carries.
///
/// It is also called CRC-32/IEEE, and it is the one zlib, PNG and gzip compute.
/// Four parameters distinguish it from the dozen other checksums also called
/// "CRC-32", [`crc32_rockchip`] among them:
///
/// - Reflected input
/// - Reflected output
/// - An initial value of all ones
/// - A final complement
///
/// Its published check value, the CRC of the ASCII digits `123456789`, is
/// `0xCBF43926`, and a test asserts that value. The check value identifies which
/// variant this is.
///
/// It is computed a bit at a time, with no lookup table. Its inputs are GPT headers
/// and entry arrays, SPL bodies, and the 1 MiB windows a DFU write is read back in.
/// Each of them crosses USB or a serial line, which takes far longer than this
/// loop. A table would add speed that no caller here needs, at the cost of 256
/// constants that must be correct.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;

    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            // The bit shifted out decides whether the polynomial is applied,
            // so it is read before the shift discards it.
            let carry = crc & 1;
            crc >>= 1;
            if carry != 0 {
                crc ^= POLYNOMIAL;
            }
        }
    }

    !crc
}

/// Rockchip's CRC-32 over `bytes`: the checksum a `PARM` parameter block carries.
///
/// Each of its parameters differs from [`crc32`]:
///
/// | | [`crc32`] | this |
/// |---|---|---|
/// | polynomial | `0x04C11DB7` | **`0x04C10DB7`** |
/// | bit order | reflected | **unreflected, MSB first** |
/// | initial value | all ones | **zero** |
/// | final step | complement | **none** |
///
/// rkdeveloptool, Rockchip's own published tool, carries these parameters.
/// **\[DOC\]**
///
/// This function and [`crc32`] give different answers over the same bytes, and
/// bytes checked with the wrong one are reported as damaged. Over the ASCII text
/// `FIRMWARE_VER: 6.0.0\n`, this returns `0x67F0CA39`, where [`crc32`] returns
/// `0xA3FAD3FE`. A test asserts both values. If either function is changed to
/// compute the other's checksum, that test fails.
///
/// The stored value is little-endian, like every other integer in the parameter
/// block's header. [`rkparam`](super::rkparam) handles the byte order.
pub fn crc32_rockchip(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;

    for &byte in bytes {
        // Unreflected: the byte enters at the top of the register, not the
        // bottom, and the loop below shifts left rather than right.
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            let carry = crc & 0x8000_0000;
            crc <<= 1;
            if carry != 0 {
                crc ^= POLYNOMIAL_ROCKCHIP;
            }
        }
    }

    crc
}

/// CRC-16/CCITT-FALSE (`0xFFFF`-seeded) over `bytes`: the trailer the Rockchip
/// maskrom download-boot appends to each code section.
///
/// The seed is the parameter to get right. The reference implementation
/// initializes to `0xFFFF` (rkdeveloptool `CRC_CCITT`). The RK3576 BootROM
/// silently rejects trailers from the zero-seeded [`crc16_xmodem`]. The transfers
/// complete and the ROM discards the section. The failure appears one stage later,
/// as DRAM that was never initialized.
///
/// The published check value, the CRC of the ASCII digits `123456789`, is
/// `0x29B1`, and a test asserts that value. [`rkboot`](super::rkboot) appends the
/// result big-endian. This function returns a plain `u16`.
pub fn crc16_ccitt_false(bytes: &[u8]) -> u16 {
    crc16(0xffff, bytes)
}

/// CRC-16/XMODEM (zero-seeded) over `bytes`: the per-block checksum of the
/// XMODEM protocol that StarFive JH7110 recovery receives.
///
/// It has the same polynomial and bit direction as [`crc16_ccitt_false`], and only
/// the seed differs. The two are not interchangeable, because each receiver checks
/// its own variant. The published check value is `0x31C3`.
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    crc16(0, bytes)
}

/// The shared implementation of [`crc16_ccitt_false`] and [`crc16_xmodem`]:
/// polynomial `0x1021`, MSB-first (unreflected), no final XOR, from `seed`.
///
/// It belongs to the same family as [`crc32_rockchip`], in a register half as
/// wide, and uses the same bit direction. It therefore shifts left and reads its
/// carry from the top bit.
fn crc16(seed: u16, bytes: &[u8]) -> u16 {
    let mut crc = seed;

    for &byte in bytes {
        // Unreflected, like [`crc32_rockchip`]: the byte enters at the top of the
        // register, and the loop shifts left rather than right.
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            let carry = crc & 0x8000;
            crc <<= 1;
            if carry != 0 {
                crc ^= 0x1021;
            }
        }
    }

    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every CRC-32 variant has a published check value, the CRC of the ASCII
    /// digits `123456789`, and the check value tells the variants apart. This one
    /// is `0xCBF43926`, which identifies CRC-32/ISO-HDLC. A wrong reflection or
    /// initial value would still produce a plausible 32-bit number over any other
    /// input. Only this constant would detect it.
    #[test]
    fn the_check_value_is_the_one_that_names_this_crc() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    /// Published vectors, which pin the loop against an external reference rather
    /// than its own output.
    #[test]
    fn known_vectors_come_out_right() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xe8b7_be43);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414f_a339
        );
    }

    /// Each pair differs in one property: the first only in byte order, the other
    /// two only in length. A CRC that ignored order or length would give one pair
    /// equal answers. This test asserts that none does.
    #[test]
    fn a_crc_depends_on_every_byte_and_on_their_order() {
        assert_ne!(crc32(b"ab"), crc32(b"ba"));
        assert_ne!(crc32(b"a"), crc32(b"a\0"));
        assert_ne!(crc32(&[0u8; 4]), crc32(&[0u8; 8]));
    }

    /// Rockchip's CRC, against a line of real parameter text. The value is what
    /// its reference tools compute. If the value is wrong, every board's parameter
    /// block reads as damaged.
    #[test]
    fn the_rockchip_crc_matches_what_its_reference_tools_compute() {
        assert_eq!(crc32_rockchip(b"FIRMWARE_VER: 6.0.0\n"), 0x67f0_ca39);
        assert_eq!(crc32_rockchip(b""), 0x0000_0000, "no init, no final xor");
    }

    /// The test that keeps the two CRC-32s apart.
    ///
    /// The two differ in one bit of one constant, and both are called CRC-32. The
    /// likely error is a call to "the CRC function" that picks the wrong one. Over
    /// the same bytes the two must disagree. If a refactor points one of them at
    /// the other's polynomial, this test fails.
    #[test]
    fn the_two_crcs_are_not_the_same_function() {
        let text = b"FIRMWARE_VER: 6.0.0\n";
        assert_eq!(crc32_rockchip(text), 0x67f0_ca39, "Rockchip's");
        assert_eq!(crc32(text), 0xa3fa_d3fe, "the standard one");
        assert_ne!(crc32_rockchip(text), crc32(text));

        // And they disagree on the check value that names the standard one, which
        // is the single most likely thing for a mistaken implementation to get
        // accidentally right.
        assert_ne!(crc32_rockchip(b"123456789"), 0xcbf4_3926);
    }

    /// The two CRC-16s have published check values, and the values tell them
    /// apart. The `0xFFFF`-seeded CCITT-FALSE that the Rockchip BootROM checks
    /// gives `0x29B1`. The zero-seeded XMODEM that StarFive recovery checks gives
    /// `0x31C3`. The two are one seed apart, and the RK3576 BootROM silently
    /// rejects sections sealed with the XMODEM variant.
    #[test]
    fn the_two_crc16_seeds_are_told_apart_by_their_check_values() {
        assert_eq!(crc16_ccitt_false(b"123456789"), 0x29b1);
        assert_eq!(crc16_ccitt_false(b""), 0xffff, "the seed, untouched");

        assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
        assert_eq!(crc16_xmodem(b""), 0x0000, "no seed, no final xor");

        assert_ne!(
            crc16_ccitt_false(b"123456789"),
            crc16_xmodem(b"123456789"),
            "the seeds are not interchangeable"
        );
    }
}
