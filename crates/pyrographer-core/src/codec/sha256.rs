//! SHA-256, the hash an `RKNS` ID block names its images by.
//!
//! An `RKNS` header records a SHA-256 for each image it describes, and one for its
//! own first 1536 bytes. [`idb`](super::idb) checks every one of them before an ID
//! block is planned. An image laid at the wrong place, or unscrambled by mistake, is
//! therefore refused rather than written. The hashes are the ID
//! block's own account of what it holds, and this module is what reads that account.
//!
//! It is FIPS 180-4's algorithm, written out: a 64-byte block, a message schedule
//! of 64 words, and 64 rounds over eight working words. This module is pure
//! arithmetic: it performs no I/O, and it does not interpret the bytes it is given.
//! The published test vectors pin it, and so do vectors at every padding boundary.
//!
//! It takes a whole buffer. The images it hashes are a loader's flash stages, under
//! a megabyte in total and already in memory.

/// The length of a SHA-256 digest, in bytes.
pub const DIGEST_LEN: usize = 32;

/// The length of the block SHA-256 compresses at a time, in bytes.
const BLOCK_LEN: usize = 64;

/// The initial hash value: the first 32 bits of the fractional parts of the square
/// roots of the first eight primes (FIPS 180-4, section 5.3.3).
const INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The round constants: the first 32 bits of the fractional parts of the cube roots
/// of the first 64 primes (FIPS 180-4, section 4.2.2).
///
/// A wrong entry changes every digest, so the published vectors catch one.
const ROUND: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 digest of `bytes`.
///
/// The message is padded as FIPS 180-4 section 5.1.1 describes. One `1` bit goes
/// first, then zeros until the length is 56 bytes short of a whole block. The
/// message length in bits follows, as a big-endian `u64`. The padded message is
/// compressed a block at a time.
pub fn sha256(bytes: &[u8]) -> [u8; DIGEST_LEN] {
    let mut state = INITIAL;

    let (blocks, tail) = bytes.as_chunks::<BLOCK_LEN>();
    for block in blocks {
        compress(&mut state, block);
    }

    // The tail, the padding bit, and the length. They fill one block, or two when
    // the tail leaves no room for the eight length bytes after the padding bit.
    let mut last = [0u8; 2 * BLOCK_LEN];
    last[..tail.len()].copy_from_slice(tail);
    last[tail.len()] = 0x80;
    let padded = if tail.len() < BLOCK_LEN - 8 {
        BLOCK_LEN
    } else {
        2 * BLOCK_LEN
    };
    let bits = (bytes.len() as u64).wrapping_mul(8);
    last[padded - 8..padded].copy_from_slice(&bits.to_be_bytes());
    for block in last[..padded].as_chunks::<BLOCK_LEN>().0 {
        compress(&mut state, block);
    }

    let mut digest = [0u8; DIGEST_LEN];
    for (out, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *out = word.to_be_bytes();
    }
    digest
}

/// Compress one block into the running state (FIPS 180-4, section 6.2.2).
fn compress(state: &mut [u32; 8], block: &[u8; BLOCK_LEN]) {
    // The message schedule: the block's sixteen big-endian words, then 48 more
    // mixed from them.
    let mut schedule = [0u32; 64];
    for (word, bytes) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_be_bytes(*bytes);
    }
    for t in 16..64 {
        let s0 = schedule[t - 15].rotate_right(7)
            ^ schedule[t - 15].rotate_right(18)
            ^ (schedule[t - 15] >> 3);
        let s1 = schedule[t - 2].rotate_right(17)
            ^ schedule[t - 2].rotate_right(19)
            ^ (schedule[t - 2] >> 10);
        schedule[t] = schedule[t - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[t - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for t in 0..64 {
        let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(big_s1)
            .wrapping_add(choose)
            .wrapping_add(ROUND[t])
            .wrapping_add(schedule[t]);
        let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = big_s0.wrapping_add(majority);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(add);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A digest as the lowercase hex a published vector prints.
    fn hex(digest: [u8; DIGEST_LEN]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The published vectors from FIPS 180-4's examples, which pin the algorithm
    /// against an external reference rather than its own output.
    #[test]
    fn the_published_vectors_come_out_right() {
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// A million `a`s, the long published vector: many blocks, and a length that
    /// needs more than 16 bits.
    #[test]
    fn the_long_published_vector_comes_out_right() {
        assert_eq!(
            hex(sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    /// Every length where the padding changes shape: the length bytes just fitting
    /// in the last block, just not fitting, and a tail of zero. A padding rule off
    /// by one byte passes the short vectors above and fails one of these. The
    /// expected values are Python's `hashlib` over the same bytes.
    #[test]
    fn every_padding_boundary_comes_out_right() {
        let message = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 31 + 7) as u8).collect() };
        let expected = [
            (
                55,
                "8aa994584139d128848eeebc4e815639ba5ab6e6e39574195a63ac4f14f7c43b",
            ),
            (
                56,
                "ad574708f75c044c9b85de64cb568ee7711ff4f36448c6242f053ba8f6cc2b63",
            ),
            (
                63,
                "280ed3e8ff1df845b2e7dfe6ac6cee817bef20e783cc65abc41b818b4d2fe076",
            ),
            (
                64,
                "c6ab9724ade5b6a7a1edfffb12f3aa9181351355af8fd08c919952ad211339dd",
            ),
            (
                65,
                "788367c73c7ddf4c53f65e68cc0d943e6227ab55b0e78ba63ace822b1c6301c0",
            ),
            (
                119,
                "3d610547d68216dedf7435a4fb6260353911f6b3fd3f18805ddb8be285d726fe",
            ),
            (
                120,
                "1f80156a804cb7862ad113e8200e9d74499723e7c7854d5f48776d3148e09656",
            ),
        ];
        for (n, digest) in expected {
            assert_eq!(hex(sha256(&message(n))), digest, "{n} bytes");
        }
    }
}
