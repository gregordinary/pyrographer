//! Progress reporting and cancellation for long operations.
//!
//! Core never prints and never reads a clock. A long verb reports what it has
//! moved by emitting [`Progress`] events to a sink the caller supplies. It checks
//! a [`Cancel`] token between windows, so the caller can stop a transfer already
//! running. The caller renders percentages, throughput and progress bars.
//! Throughput needs a clock, and only the caller reads one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// An event a long-running verb emits as it works.
///
/// Counts are in bytes, not sectors, so a caller rendering a progress bar needs
/// no backend's sector size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// The operation has begun and will move `total_bytes` in all.
    Started {
        /// Bytes the operation will move.
        total_bytes: u64,
    },
    /// A window completed.
    Advanced {
        /// Bytes moved so far, counting from the start of the operation.
        done_bytes: u64,
        /// Bytes the operation will move.
        total_bytes: u64,
    },
    /// The operation finished.
    Finished {
        /// Bytes moved in all.
        done_bytes: u64,
    },
}

/// Where a verb sends its [`Progress`] events.
///
/// It is a borrowed closure rather than a channel, so core needs no dependency to
/// carry one. The CLI renders straight to the terminal. The GUI
/// writes into the state its next frame reads, and a test collects into a `Vec`.
/// Pass `&mut |_| {}` to ignore progress.
pub type ProgressSink<'a> = &'a mut dyn FnMut(Progress);

/// A cancellation token shared between a caller and a running verb.
///
/// Clones share one flag. The caller sets it with [`Cancel::cancel`] from
/// anywhere, such as a GUI button or a signal handler. The verb checks it at its
/// next window boundary and returns [`Error::Canceled`](crate::Error::Canceled).
/// Canceling therefore stops the operation between transfers, never partway
/// through one, so the device is never left mid-command.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// Create a token that has not been canceled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask any verb holding a clone of this token to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been asked for.
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Pause the current task for `millis` milliseconds, honoring the device's pace.
///
/// Devices set their own pace. A maskrom board needs a DRAM settle after a stage,
/// and a DFU device returns a `bwPollTimeout` the host must wait before its next
/// request. Every such wait is spent here, so it is spent the same way everywhere.
/// A `millis` of zero returns at once. That is the common case, and it keeps a hot
/// poll loop from touching the clock at all.
///
/// The native build blocks the thread. The USB transport blocks the same way, and
/// under `pollster` it costs nothing. The web build cannot block a thread. It
/// yields to the event loop for `millis` through a `setTimeout`-backed await, then
/// resumes. The device-facing flows that use this are **\[UNVERIFIED\]** in a
/// browser, but the delay is not silently dropped.
pub(crate) async fn settle(millis: u32) {
    if millis == 0 {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(std::time::Duration::from_millis(u64::from(millis)));
    #[cfg(target_arch = "wasm32")]
    {
        let Some(window) = web_sys::window() else {
            return;
        };
        let millis = i32::try_from(millis).unwrap_or(i32::MAX);
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, millis);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_flag() {
        let held_by_caller = Cancel::new();
        let held_by_verb = held_by_caller.clone();
        assert!(!held_by_verb.is_canceled());
        held_by_caller.cancel();
        assert!(held_by_verb.is_canceled());
    }
}
