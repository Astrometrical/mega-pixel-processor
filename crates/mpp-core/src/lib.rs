//! Mega Pixel Processor engine library.
//!
//! `mpp-core` is the UI-agnostic engine behind the `mpp` CLI; the future
//! PixInsight file-format worker and any GUI reuse it. See `docs/DESIGN.md`
//! for the founding decisions and phase plan.
//!
//! Public API surface:
//!
//! - [`formats`] — image readers/writers: native XISF reading and
//!   strip-streaming BigTIFF writing today; libvips-backed formats and the
//!   DZI pipeline arrive with later phase-1 plans.
//! - [`wcs`] — astrometric solutions: the linear WCS model, extraction
//!   from PixInsight XISF properties, and the canonical JSON sidecar.
//! - [`convert`] — one-call conversion orchestration used by the CLI.

#![warn(missing_docs)]

pub mod error;
pub use error::{Error, Result};

pub mod formats;

pub mod wcs;

pub mod convert;

#[cfg(test)]
mod testsupport;
