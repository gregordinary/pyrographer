//! The surplus of a stream chunk larger than the read that asked for it.
//!
//! A browser's `ReadableStream` delivers whatever has arrived as one chunk, and
//! that chunk can be larger than the caller's buffer. The surplus must be held and
//! served before the stream is asked for another chunk. The native serial
//! transport has no surplus: the operating system holds the line's bytes, and
//! `read(2)` takes only what the buffer has room for.
//!
//! The buffer is kept apart from [`webserial`](super::webserial) and works on
//! plain byte slices, as every sans-I/O codec in the crate does. Code tied to
//! `Uint8Array` runs only in a browser. This code is tested on every commit.
//!
//! The buffer is the one place in that transport where bytes can be **lost,
//! duplicated, or reordered**. It sits on the read side, so a fault here corrupts
//! what the host hears. The [`recovery`](crate::recovery) sender reads only the
//! receiver's `C`, `ACK` and `NAK`. A duplicated or reordered `ACK` can move it past
//! a block the board has not accepted. The [`console`](crate::console) and
//! [`uboot`](crate::uboot) drivers read the board's text, and a fault garbles the
//! transcript they match against.
//!
//! It is compiled for `wasm32`, where [`webserial`](super::webserial) uses it,
//! and under `test` on every target.

/// The tail of a chunk, waiting for the next read.
#[derive(Debug, Default)]
pub(super) struct ChunkBuffer {
    held: Vec<u8>,
}

impl ChunkBuffer {
    /// Whether any bytes are held.
    ///
    /// A caller must check this before it reads from the stream. Reading another
    /// chunk while bytes are still held here puts the bytes out of order.
    pub(super) fn has_bytes(&self) -> bool {
        !self.held.is_empty()
    }

    /// Fill `buf` from the held bytes, and return how many were copied.
    pub(super) fn take(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.held.len());
        buf[..n].copy_from_slice(&self.held[..n]);
        self.held.drain(..n);
        n
    }

    /// Fill `buf` from the front of `chunk`, and hold whatever did not fit.
    ///
    /// A chunk is asked for only after [`has_bytes`](Self::has_bytes) returns
    /// `false`, so nothing is held on entry. The function asserts that rather than
    /// appending silently. A second chunk appended behind a first is the
    /// reordering this type prevents.
    pub(super) fn fill(&mut self, chunk: &[u8], buf: &mut [u8]) -> usize {
        debug_assert!(
            self.held.is_empty(),
            "a chunk was taken while {} bytes were still held",
            self.held.len()
        );
        let n = buf.len().min(chunk.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        self.held.extend_from_slice(&chunk[n..]);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkBuffer;

    /// Drain a chunk through reads of `read_size` bytes, as a driver does, and
    /// return the bytes that came out.
    fn drain(chunk: &[u8], read_size: usize) -> Vec<u8> {
        let mut held = ChunkBuffer::default();
        let mut out = Vec::new();
        let mut buf = vec![0u8; read_size];

        let n = held.fill(chunk, &mut buf);
        out.extend_from_slice(&buf[..n]);

        while held.has_bytes() {
            let n = held.take(&mut buf);
            assert!(n > 0, "a non-empty buffer served nothing");
            out.extend_from_slice(&buf[..n]);
        }

        out
    }

    #[test]
    fn a_chunk_smaller_than_the_read_is_served_whole_and_holds_nothing() {
        let mut held = ChunkBuffer::default();
        let mut buf = [0u8; 64];

        assert_eq!(held.fill(&[1, 2, 3], &mut buf), 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert!(!held.has_bytes());
    }

    #[test]
    fn a_chunk_exactly_the_read_holds_nothing() {
        let mut held = ChunkBuffer::default();
        let mut buf = [0u8; 4];

        assert_eq!(held.fill(&[1, 2, 3, 4], &mut buf), 4);
        assert_eq!(&buf, &[1, 2, 3, 4]);
        assert!(!held.has_bytes());
    }

    /// Every byte a chunk carried comes back out once, in the order it arrived.
    /// That is the property this type provides. A `C` storm split across reads is
    /// this case.
    #[test]
    fn a_chunk_larger_than_the_read_comes_back_whole_and_in_order() {
        let chunk: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(drain(&chunk, 64), chunk);
    }

    /// The recovery sender reads 64 bytes at a time, and the console reads 512.
    /// The browser's buffer is 4 KiB. A chunk that is a whole multiple of the read
    /// is therefore the ordinary case, not an edge case.
    #[test]
    fn a_chunk_that_divides_evenly_leaves_nothing_behind() {
        let chunk: Vec<u8> = (0..512u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(drain(&chunk, 64), chunk);

        let mut held = ChunkBuffer::default();
        let mut buf = [0u8; 64];
        for _ in 0..8 {
            held.fill(&chunk, &mut buf);
            while held.has_bytes() {
                held.take(&mut buf);
            }
        }
        assert!(!held.has_bytes());
    }

    /// No driver here makes a one-byte read. It is the size at which an
    /// off-by-one in the drain shows, so the case is asserted rather than assumed.
    #[test]
    fn a_one_byte_read_still_sees_every_byte_in_order() {
        let chunk: Vec<u8> = (0..40u32).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(drain(&chunk, 1), chunk);
    }

    #[test]
    fn an_empty_chunk_serves_nothing_and_holds_nothing() {
        let mut held = ChunkBuffer::default();
        let mut buf = [0u8; 16];

        assert_eq!(held.fill(&[], &mut buf), 0);
        assert!(!held.has_bytes());
    }

    /// A zero-length read takes nothing and loses nothing. The transport refuses a
    /// zero-length read before it reaches the buffer. If one ever arrives, the
    /// buffer keeps the whole chunk, and this test pins that.
    #[test]
    fn a_zero_length_read_holds_the_whole_chunk() {
        let mut held = ChunkBuffer::default();
        let mut empty: [u8; 0] = [];

        assert_eq!(held.fill(&[1, 2, 3], &mut empty), 0);
        assert!(held.has_bytes());

        let mut buf = [0u8; 8];
        assert_eq!(held.take(&mut buf), 3);
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert!(!held.has_bytes());
    }

    #[test]
    fn taking_from_an_empty_buffer_serves_nothing() {
        let mut held = ChunkBuffer::default();
        let mut buf = [0u8; 8];

        assert_eq!(held.take(&mut buf), 0);
    }
}
