//! Mega Pixel Processor engine library.
//!
//! `mpp-core` is the UI-agnostic engine behind the `mpp` CLI; the future
//! PixInsight file-format worker and any GUI reuse it. See `docs/DESIGN.md`
//! for the founding decisions and phase plan.
//!
//! Public API surface:
//!
//! - [`formats`] — image readers/writers: native XISF reading and
//!   strip-streaming TIFF/BigTIFF writing; vips-backed mainstream-format export
//!   (PNG/JPEG/WebP/AVIF/JXL, via [`vips`]) and DZI tile-pyramid export are
//!   both implemented today.
//! - [`header`] — the format-neutral [`header::Header`] (ordered FITS
//!   keywords plus XISF properties) that the readers return.
//! - [`astrometry`] — [`astrometry::AstrometricSolution`], the full
//!   astrometric solution model (PixInsight's standard and legacy blocks,
//!   distortion included) decoded from a [`header::Header`].
//! - [`wcs`] — astrometric solutions: the linear WCS model, extraction
//!   from PixInsight XISF properties, and the canonical JSON sidecar.
//! - [`convert`] — one-call conversion orchestration used by the CLI.
//! - [`vips`] — discovery and spawning of the official `vips` binary
//!   (mainstream-format conversion and DZI `dzsave` export).
//! - [`viewer`] — emission of the static, self-hostable Astrometrical
//!   viewer page.
//! - [`publish`] — one-call publish orchestration: routes any supported
//!   input to a DZI tile pyramid or a single full-resolution image, plus
//!   the branded viewer page and WCS sidecar.

#![warn(missing_docs)]

pub mod error;
pub use error::{Error, Result};

pub mod formats;

pub mod header;

pub mod astrometry;

pub(crate) mod pipeline;

pub mod wcs;

pub mod convert;

pub mod vips;

pub mod viewer;

pub mod publish;

#[cfg(test)]
mod testsupport;
