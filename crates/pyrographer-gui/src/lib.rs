//! pyrographer-gui: one crate, a native window and a web flasher.
//!
//! The same code draws a desktop window and, compiled to `wasm32`, a browser
//! flasher that installs nothing. It is a thin consumer of `pyrographer-core`,
//! which is the product. This crate holds no protocol, no codec and no write path
//! of its own.
//!
//! The crate is in four parts, split so that the reasoning sits where the tests pin
//! it:
//!
//! - [`state`] holds the connection state machine, the job lifecycle, the
//!   plan-to-confirmation flow, and the comparison that decides whether a person
//!   agreed to a write. **No egui type appears in it.** It is tested against core's
//!   scripted transport, so the part of the front-end that holds the reasoning is
//!   the part that is pinned.
//! - [`ui`] draws. It is kept thin, so the code that is not tested holds none of the
//!   reasoning.
//! - [`app`] runs the frame loop, and turns a button into a task.
//! - [`platform`] holds the three seams that differ between a window and a tab:
//!   acquiring a board, acquiring a serial port, and the image file. It also holds
//!   how each build spawns a job.
//!
//! # The gate
//!
//! The GUI takes the CLI's four steps unchanged: plan, confirm, write window by
//! window with each window read back, and report. It runs the one write path and
//! adds no second one. The GUI draws the plan as a screen, and a person cannot
//! skip it. **The confirm button mints a `ConfirmedWrite`, a `ConfirmedClone` or a
//! `ConfirmedSegmentedWrite`, and nothing else does.** An unconfirmed write is
//! therefore a compile error, as it is in the CLI, rather than something a code
//! review has to catch.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod app;
pub mod platform;
pub mod state;
pub mod theme;
pub mod ui;

pub use app::App;

/// Start the web flasher on a canvas.
///
/// This is the browser's entry point. A tab has no `main`, so this crate is a
/// library with a thin binary rather than a binary. The page calls this function,
/// eframe takes the canvas, and from then on it runs the same app the desktop window
/// runs.
///
/// The canvas is found by id, so a page can place it anywhere.
///
/// **\[UNVERIFIED\]** against a real browser. The code this entry point depends on
/// (the WebUSB transport, and the `Blob` behind the image seam) compiles, and is not
/// yet checked against a browser.
///
/// # Errors
///
/// If the page has no canvas by that id, or eframe cannot take it.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub async fn start(canvas_id: String) -> std::result::Result<(), wasm_bindgen::JsValue> {
    use wasm_bindgen::JsCast;

    let canvas = web_sys::window()
        .ok_or_else(|| wasm_bindgen::JsValue::from_str("there is no window"))?
        .document()
        .ok_or_else(|| wasm_bindgen::JsValue::from_str("there is no document"))?
        .get_element_by_id(&canvas_id)
        .ok_or_else(|| {
            wasm_bindgen::JsValue::from_str(&format!("the page has no element '{canvas_id}'"))
        })?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;

    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))),
        )
        .await
}
