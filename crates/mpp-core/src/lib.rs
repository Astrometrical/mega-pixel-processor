//! Mega Pixel Processor engine library.
//!
//! `mpp-core` is the UI-agnostic engine behind the `mpp` CLI; the future
//! PixInsight file-format worker and any GUI reuse it. See `docs/DESIGN.md`
//! for the founding decisions and phase plan.
//!
//! Public API surface, always available (the lean core format consumers
//! such as Mega Merge Mosaic build with `default-features = false`):
//!
//! - [`formats`] — image readers/writers: native XISF reading and FITS
//!   header reading.
//! - [`header`] — the format-neutral [`header::Header`] (ordered FITS
//!   keywords plus XISF properties) that the readers return.
//! - [`astrometry`] — [`astrometry::AstrometricSolution`], the full
//!   astrometric solution model (PixInsight's standard and legacy blocks,
//!   distortion included) decoded from a [`header::Header`].
//! - [`wcs`] — astrometric solutions: the linear WCS model, extraction
//!   from PixInsight XISF properties, and the canonical JSON sidecar.
//!
//! Behind Cargo features (all on by default):
//!
//! - `tiff` — `formats::tiff_read` / `formats::tiff_write`: the
//!   TIFF/BigTIFF reader and strip-streaming writer.
//! - `convert` (implies `tiff`) — `convert`: one-call conversion
//!   orchestration used by the CLI.
//! - `publish` (implies `convert`) — `vips` (discovery and spawning of the
//!   official `vips` binary for mainstream-format export and DZI `dzsave`),
//!   `viewer` (the static, self-hostable Astrometrical viewer page) and
//!   `publish` (routes any supported input to a DZI tile pyramid or a
//!   single full-resolution image, plus the viewer page and WCS sidecar).

#![warn(missing_docs)]

pub mod error;
pub use error::{Error, Result};

pub mod formats;

pub mod header;

pub mod astrometry;

#[cfg(feature = "tiff")]
pub(crate) mod pipeline;

pub mod wcs;

#[cfg(feature = "convert")]
pub mod convert;

#[cfg(feature = "publish")]
pub mod vips;

#[cfg(feature = "publish")]
pub mod viewer;

#[cfg(feature = "publish")]
pub mod publish;

#[cfg(test)]
mod testsupport;
