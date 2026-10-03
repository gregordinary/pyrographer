//! The image seam: where a write's bytes come from, and where a dump's bytes go.
//!
//! It is the other end of every long verb. [`transport`](crate::transport) is the
//! seam facing the device, and this is the seam facing the image. Both are async
//! because the browser's APIs are.
//!
//! A file picked from a browser's chooser is a `Blob`. It yields its bytes through
//! a promise and offers no blocking read. An image seam built on
//! [`std::io::Read`] would leave the web flasher one way through
//! [`verbs::flash`](crate::verbs::flash): buffer the whole image in memory first.
//! That breaks the windowed design's invariant, that no verb holds the image in
//! memory. It also makes a whole-eMMC dump in a tab impossible, not merely slow.
//!
//! [`std::io::Read`] exists on `wasm32`. A core whose image seam used it would
//! therefore pass `cargo clippy --target wasm32-unknown-unknown`, and no browser
//! could use it. That gate proves core compiles for the browser, and cannot prove
//! it works there. This is the one seam where the two differ.
//!
//! # The two implementations
//!
//! Natively, an image is a file. [`SyncReader`] and [`SyncWriter`] wrap anything
//! that implements [`std::io::Read`] or [`std::io::Write`], so the CLI and the
//! native GUI each hand over a `File`. In the browser, the traits are implemented
//! over a `Blob`, sliced a window at a time. A verb sees neither implementation.

use std::future::Future;
use std::io::{Read, Write};
use std::pin::Pin;

use crate::{Error, Result};

/// A boxed future, returned by the image seam's methods.
///
/// A verb takes its image as a `&mut dyn`, so one verb serves a file, a buffer and
/// a `Blob`. A trait with an `async fn` is not `dyn`-compatible, so this seam
/// returns a boxed future instead, with no `async-trait` dependency.
/// [`Transport`](crate::transport::Transport) needs no box. The cost is one
/// allocation per window, and a window is megabytes.
///
/// There is no `Send` bound. A native job drives the verb to completion on one
/// thread, and the browser is single-threaded by construction. Nothing here
/// crosses a thread mid-await.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Where a write's bytes come from.
///
/// Implemented over a file natively and a `Blob` in the browser. A verb sees
/// neither.
pub trait ImageReader {
    /// Fill `buf` entirely, or fail.
    ///
    /// A short read is a failure, not a finding. A plan is made against an image
    /// of a stated length, and a source that runs out contradicts the plan the
    /// write was confirmed against. The failure is [`Error::Io`], because the image
    /// is at fault and the device did nothing.
    fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<()>>;
}

/// Where a dump's bytes go.
pub trait ImageWriter {
    /// Write the whole of `buf`, or fail.
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>>;

    /// Commit everything written so far.
    ///
    /// The method has no default, so every implementation must commit its own
    /// buffered bytes. A default would silently skip the sink that most needs it.
    /// A browser's writable stream holds bytes until it is told to commit them. A
    /// dump that returned before they landed would report a file that is not
    /// there. A buffered file behaves the same way.
    fn flush(&mut self) -> BoxFuture<'_, Result<()>>;
}

/// An [`ImageReader`] over anything that reads synchronously: a `File`, a
/// `Cursor`, a slice.
///
/// The native implementation of the seam. A read blocks the thread that drives it.
/// A file read blocks anyway, and the native USB transport blocks by design, as
/// `transport/usb.rs` explains. Under `pollster`, on a job's own thread, the
/// block costs nothing.
pub struct SyncReader<R: Read> {
    inner: R,
}

impl<R: Read> SyncReader<R> {
    /// Present `reader` to a verb as an image.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// The reader back.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> ImageReader for SyncReader<R> {
    fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<()>> {
        // The cause, and only the cause. The verb calling this knows the byte it
        // was at and how many it was promised, and it says so; saying it here
        // too would say it twice.
        Box::pin(async move {
            self.inner
                .read_exact(buf)
                .map_err(|e| Error::Io(e.to_string()))
        })
    }
}

/// An [`ImageWriter`] over anything that writes synchronously: a `File`, a
/// `Vec<u8>`.
pub struct SyncWriter<W: Write> {
    inner: W,
}

impl<W: Write> SyncWriter<W> {
    /// Present `writer` to a verb as a place to put an image.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// The writer back.
    ///
    /// A dump flushes before it returns, so what comes back here holds every byte
    /// the verb wrote.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> ImageWriter for SyncWriter<W> {
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.inner
                .write_all(buf)
                .map_err(|e| Error::Io(e.to_string()))
        })
    }

    fn flush(&mut self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.inner.flush().map_err(|e| Error::Io(e.to_string())) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verb sees only the adapter, so this test uses it as a verb does. It fills
    /// one window, and then another.
    #[test]
    fn a_sync_reader_hands_a_verb_its_windows() {
        let image: Vec<u8> = (0..=255).collect();
        let mut reader = SyncReader::new(image.as_slice());

        let mut window = [0u8; 128];
        pollster::block_on(reader.read_exact(&mut window)).expect("the image has 256 bytes");
        assert_eq!(window[0], 0);
        assert_eq!(window[127], 127);

        pollster::block_on(reader.read_exact(&mut window)).expect("and 128 more");
        assert_eq!(window[0], 128);
        assert_eq!(window[127], 255);
    }

    /// An image that runs out mid-window contradicts the plan the write was
    /// confirmed against. The read fails rather than filling the rest of the window
    /// with whatever the buffer held.
    #[test]
    fn a_sync_reader_that_runs_out_fails_rather_than_filling_the_rest() {
        let image = vec![0xaa; 100];
        let mut reader = SyncReader::new(image.as_slice());

        let mut window = [0u8; 512];
        let error = pollster::block_on(reader.read_exact(&mut window))
            .expect_err("the image is 100 bytes and the window is 512");
        assert!(matches!(error, Error::Io(_)), "{error:?}");
    }

    /// A dump's bytes are not written until they are flushed, and a verb flushes.
    /// A browser's writable stream buffers the way the `BufWriter` here does. That
    /// is why `flush` is required on the trait and has no default.
    #[test]
    fn a_sync_writer_takes_a_verbs_windows_and_flushes_them() {
        let mut sink = SyncWriter::new(std::io::BufWriter::new(Vec::new()));

        pollster::block_on(sink.write_all(&[1, 2, 3])).expect("the sink takes them");
        pollster::block_on(sink.flush()).expect("and commits them");

        let buffered = sink.into_inner();
        assert_eq!(buffered.buffer(), b"", "the flush emptied the buffer");
        assert_eq!(buffered.into_inner().expect("nothing is left"), [1, 2, 3]);
    }
}
