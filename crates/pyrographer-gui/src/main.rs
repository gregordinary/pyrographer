//! The native window.
//!
//! The application is a library, so this binary only opens a window. The web build
//! has no binary at all: wasm-bindgen links against the cdylib, and the browser calls
//! the entry point. Everything else therefore lives in the library crate.

#![forbid(unsafe_code)]

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    eframe::run_native(
        "pyrographer",
        eframe::NativeOptions {
            // **The window asks for a size the layout was drawn for.** Left to
            // itself eframe falls back to 800x600, which is tighter than the verb
            // surface wants -- a board's panel and its verbs measure about 640
            // points together, before the header and the tabs above them.
            //
            // eframe clamps this to the monitor on its own
            // (`clamp_size_to_monitor_size` defaults on, and it clamps in points,
            // so a HiDPI panel is handled), which is what keeps the request from
            // opening a window larger than the screen. It clamps to the *largest*
            // monitor rather than the one the window lands on.
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100.0, 820.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(pyrographer_gui::App::new(cc.egui_ctx.clone())))),
    )
}

/// The `main` that a `wasm32` build of the binary target compiles.
///
/// A browser runs no binary, and the web flasher's entry point is in the library. The
/// clippy gate on `wasm32` checks the library alone, with `--lib`. A `wasm32` build of
/// the whole crate also compiles this binary target, and that build needs a `main`.
#[cfg(target_arch = "wasm32")]
fn main() {}
