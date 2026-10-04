//! RC4, and the fixed key that Rockchip's tooling scrambles with.
//!
//! RC4 is a symmetric stream cipher. The same operation encrypts and decrypts,
//! because both XOR the data with the same keystream. This module is pure
//! arithmetic: it performs no I/O, and it does not interpret the bytes it is given.
//!
//! Rockchip uses RC4 as a light scramble, re-keyed every 512 bytes, and not for
//! secrecy. The key is public and hard-coded into every flashing tool.
//!
//! **pyrographer's upload does not use it.** The maskrom download-boot path
//! ([`rkboot::download_payload`](super::rkboot::download_payload)) sends each code
//! section's bytes verbatim, plus a trailing CRC-16 and nothing else.
//! rkdeveloptool's `RKU_DeviceRequest` does the same. The rkbin RKBOOT containers
//! carry an `rc4_disabled` flag, and the RK3576 loader sets it. The BootROM takes
//! the bytes as sent.
//!
//! The per-512-byte re-key belongs to the on-flash *ID block*. A loader container
//! stores its flash stages scrambled this way, block by block, and
//! [`idb`](super::idb) unscrambles them before laying out an ID block. The key is
//! measured, not only reported. Unscrambling the RK3576 container's DRAM-init stage
//! with it yields, byte for byte, the 471 blob that BootROM accepts over USB.
//! The legacy ID block's first sector, which begins `55 AA F0 0F`, gives a second
//! known-plaintext vector. **\[COMMUNITY\]**

/// The 16-byte RC4 key that Rockchip's tooling scrambles the on-flash ID block with.
///
/// The key is hard-coded into every reference tool and provides no secrecy. It
/// belongs to the ID-block path. pyrographer's loader upload sends verbatim bytes
/// and does not use it.
///
/// A known-plaintext block identifies the key. The legacy ID block's first sector
/// begins `55 AA F0 0F`, which this key encrypts to `3B 8C DC FC`, and a test
/// asserts that result. The same sixteen bytes appear in rkdeveloptool, rkflashtool and xrock,
/// and in the BootROM. **\[COMMUNITY\]**
pub const MASKROM_RC4_KEY: [u8; 16] = [
    0x7c, 0x4e, 0x03, 0x04, 0x55, 0x05, 0x09, 0x07, 0x2d, 0x2c, 0x7b, 0x38, 0x17, 0x0d, 0x17, 0x11,
];

/// RC4-encrypt or decrypt `data` under `key`, which are the same operation.
///
/// It runs the two textbook phases. The key-scheduling algorithm permutes a
/// 256-byte state from the key. The pseudo-random generation algorithm then steps
/// through that state to produce one keystream byte per input byte, XORed in. There
/// is no initialization vector and no nonce, so a given key and length always
/// produce the same keystream. Rockchip re-keys per 512-byte block for that reason,
/// rather than running one stream across a whole image.
///
/// # Panics
///
/// Panics on an empty `key`, for which no key schedule exists. Every caller in this
/// crate passes [`MASKROM_RC4_KEY`]. An empty key is therefore a programming error,
/// not a runtime condition.
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    assert!(!key.is_empty(), "RC4 needs a non-empty key");

    // Key-scheduling algorithm: seed the state 0..=255, then permute it by the
    // key, which repeats to cover all 256 positions.
    let mut state: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }

    // Pseudo-random generation: one keystream byte per input byte, XORed in.
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0u8;
    let mut j = 0u8;
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(state[usize::from(i)]);
        state.swap(usize::from(i), usize::from(j));
        let k = state[usize::from(state[usize::from(i)].wrapping_add(state[usize::from(j)]))];
        out.push(byte ^ k);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public RC4 test vectors. They pin the cipher against an external
    /// reference rather than its own output. `"Key"` over `"Plaintext"` is the
    /// canonical one.
    #[test]
    fn known_rc4_vectors_come_out_right() {
        assert_eq!(
            rc4(b"Key", b"Plaintext"),
            [0xbb, 0xf3, 0x16, 0xe8, 0xd9, 0x40, 0xaf, 0x0a, 0xd3]
        );
        assert_eq!(rc4(b"Wiki", b"pedia"), [0x10, 0x21, 0xbf, 0x04, 0x20]);
    }

    /// The maskrom key against the one plaintext the protocol pins it with. The
    /// first four bytes of the 512-byte ID block, `55 AA F0 0F`, encrypt to
    /// `3B 8C DC FC`. That vector confirms the sixteen bytes of `MASKROM_RC4_KEY`.
    /// **\[COMMUNITY\]**
    #[test]
    fn the_maskrom_key_matches_the_id_block_vector() {
        assert_eq!(
            rc4(&MASKROM_RC4_KEY, &[0x55, 0xaa, 0xf0, 0x0f]),
            [0x3b, 0x8c, 0xdc, 0xfc]
        );
    }

    /// A stream cipher is its own inverse: encrypting twice returns the input.
    /// One function therefore serves both directions. This test catches a
    /// refactor that breaks the symmetry.
    #[test]
    fn encrypting_twice_is_the_identity() {
        let plain = b"the loader upload does not need this to round-trip, but it must";
        let once = rc4(&MASKROM_RC4_KEY, plain);
        let twice = rc4(&MASKROM_RC4_KEY, &once);
        assert_eq!(twice, plain);
        assert_ne!(once, plain, "and it actually scrambled");
    }
}
