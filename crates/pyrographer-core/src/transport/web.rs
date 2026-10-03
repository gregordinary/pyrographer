//! What the two browser transports share.
//!
//! [`WebUsbTransport`](super::WebUsbTransport) and
//! [`WebSerialTransport`](super::WebSerialTransport) implement two different seams
//! over two unrelated browser APIs. Both have three needs in common:
//!
//! - A JavaScript exception must be turned into a message.
//! - A promise must be turned into a `Result`.
//! - Neither API gives a transfer a deadline, so both build one from
//!   `setTimeout`.
//!
//! This module implements each of the three once.
//!
//! Nothing in this module is public outside [`transport`](super). It serves the
//! two browser implementations and is not part of the seam they implement.

use std::time::Duration;

use js_sys::Promise;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::{Error, Result};

/// The message a JavaScript exception carries, in as much detail as it gives.
pub(super) fn describe(error: &JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.to_string()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| format!("{error:?}"))
}

/// Await a promise that resolves with nothing.
///
/// A rejection becomes [`Error::Transport`], and its message names `what` was
/// being done.
pub(super) async fn await_js<T>(promise: Promise<T>, what: &str) -> Result<()>
where
    T: wasm_bindgen::convert::FromWasmAbi + 'static,
{
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|e| Error::Transport(format!("{what} failed: {}", describe(&e))))
}

/// The value [`after`]'s promise resolves with, which nothing else produces.
///
/// A bare `setTimeout` resolves with `undefined`. That value would work as the
/// deadline signal for a WebUSB transfer, which always resolves with a result
/// object. It would also work for a stream read, which resolves with a
/// `{value, done}` record. It is **wrong for a stream write**, which resolves with
/// `undefined` on success. A transport that read `undefined` as its deadline would
/// report every completed write as a timeout.
///
/// The timer therefore resolves with a value no transfer on either API can
/// produce. No USB result, read record or write's `undefined` is a string, so
/// [`timed_out`] is exact for all four ways a race can settle.
const DEADLINE_MARKER: &str = "pyrographer:deadline";

/// Whether a raced promise settled because [`after`] fired.
pub(super) fn timed_out(settled: &JsValue) -> bool {
    settled.as_string().is_some_and(|s| s == DEADLINE_MARKER)
}

/// A promise that resolves with [`DEADLINE_MARKER`] after `how_long`.
///
/// Both transports build their deadlines from this promise. Neither WebUSB nor
/// Web Serial times a transfer out, and a promise that never settles is a job
/// that never ends. The cancellation token is checked at window boundaries, and a
/// transfer in progress is not at one. A cancel button therefore cannot reach a
/// host stuck inside a transfer. Each transfer is raced against this promise,
/// which bounds its wait, and [`timed_out`] reads which of the two settled.
pub(super) fn after(how_long: Duration) -> Result<Promise<JsValue>> {
    let window = web_sys::window()
        .ok_or_else(|| Error::Transport("there is no window to set a timer on".to_string()))?;

    let millis = i32::try_from(how_long.as_millis()).map_err(|_| {
        Error::InvalidRequest("that deadline is longer than a browser timer can hold".to_string())
    })?;

    Ok(Promise::new(&mut |resolve, reject| {
        // The timer is not cleared when the transfer wins. It fires later and
        // resolves a promise that has already settled, which is a no-op.
        //
        // `setTimeout` passes its trailing arguments on to the callback, so the
        // marker is what `resolve` is called with.
        //
        // If the timer cannot be armed at all, the deadline would never fire and a
        // hung transfer racing it would hang forever -- the one hole in the
        // every-transfer-is-deadlined invariant. So a failed `setTimeout` rejects
        // the promise, which surfaces as a transport error rather than an
        // unbounded wait.
        if window
            .set_timeout_with_callback_and_timeout_and_arguments_1(
                &resolve,
                millis,
                &JsValue::from_str(DEADLINE_MARKER),
            )
            .is_err()
        {
            let _ = reject.call1(
                &JsValue::NULL,
                &JsValue::from_str("could not arm the transfer deadline timer"),
            );
        }
    }))
}
