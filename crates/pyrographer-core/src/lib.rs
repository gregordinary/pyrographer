//! The library behind pyrographer, a flashing and recovery toolkit for embedded
//! devices such as single-board computers and IP cameras.
//!
//! It supports Rockchip and Ingenic XBurst devices over USB, and StarFive JH7110
//! recovery over a serial line. On Linux, it also supports block devices the host
//! operating system shares, such as an SD card in a reader.
//!
//! # Structure
//!
//! [`discovery`] finds devices and classifies the boot mode each one is in.
//! [`bootstrap`] brings a board from its on-chip boot code to a mode where flash is
//! reachable. From there, [`agent::FlashAgent`] is the uniform block interface
//! every flash backend implements, and the [`verbs`] are written once against it.
//!
//! Bytes cross two async seams. [`transport`] carries them to a device, over USB or
//! a serial line. [`image`] carries them to or from a file, a buffer or a browser's
//! `Blob`. A long verb consumes both seams and implements neither, one window at a
//! time. Every byte layout on the wire or on the flash is a sans-I/O codec in
//! [`codec`].
//!
//! [`firmware`] reads a Rockchip firmware package (`update.img`) in one forward
//! pass, checks it, and hands the verbs what a write of it needs.
//!
//! Two serial drivers have no `FlashAgent`. [`recovery`] is StarFive's write-only
//! recovery, with a verb of its own, and [`modem`] is the XMODEM and YMODEM sender
//! under it. [`console`] watches a serial console, and [`uboot`] drives a U-Boot
//! prompt through it. That prompt is where a bootstrap that RAM-boots U-Boot hands
//! off: the Rockchip maskrom bootstrap over USB, and the StarFive one over the
//! serial line.
//!
//! Core never prints and never reads a clock, so every front-end uses it unchanged,
//! and so does a test harness. A long operation emits [`progress`] events and takes
//! a cancellation token, and the caller renders them.
//!
//! # Portability
//!
//! The crate compiles for `wasm32`, apart from two native-only items:
//!
//! - [`transport::UsbTransport`] speaks to the operating system's USB stack, which
//!   a browser does not expose. WebUSB implements the same seam in a browser.
//! - [`verbs::list`] scans the bus. A browser cannot, and WebUSB returns only the
//!   device a person picked from a chooser.
//!
//! Both are `cfg`-gated to native targets. The rest of the crate is the same code
//! on both.
//!
//! # Example
//!
//! Reading a Rockchip board's flash geometry:
//!
//! ```no_run
//! use pyrographer_core::agent::{FlashAgent, RockusbAgent};
//! use pyrographer_core::discovery::Mode;
//! use pyrographer_core::transport::UsbTransport;
//! use pyrographer_core::verbs;
//!
//! # fn main() -> pyrographer_core::Result<()> {
//! let device = verbs::list()?.into_iter().find(|d| d.mode == Mode::Loader).unwrap();
//!
//! pollster::block_on(async {
//!     let transport = UsbTransport::open(&device).await?;
//!     let mut agent = FlashAgent::Rockusb(RockusbAgent::new(transport));
//!     let info = verbs::info(&mut agent).await?;
//!     println!("{} bytes", info.size_bytes);
//!     Ok(())
//! })
//! # }
//! ```

// The async methods on the transport and agent traits do not require `Send`
// futures: native builds drive them with a single-threaded `block_on`, and the
// web build is single-threaded by construction. Allowing this lint keeps the
// trait definitions free of desugaring boilerplate.
#![allow(async_fn_in_trait)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod agent;
pub mod block;
pub mod bootstrap;
pub mod codec;
pub mod console;
pub mod discovery;
pub mod error;
pub mod fill;
pub mod firmware;
pub mod image;
pub mod layout;
pub mod modem;
pub mod partition;
pub mod progress;
pub mod recovery;
pub mod soc;
pub mod transport;
pub mod uboot;
pub mod verbs;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use error::{Error, Result};
