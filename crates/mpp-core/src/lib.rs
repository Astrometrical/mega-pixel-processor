//! Mega Pixel Processor engine library.
//!
//! `mpp-core` is the UI-agnostic engine behind the `mpp` CLI; the future
//! PixInsight file-format worker and any GUI reuse it. See `docs/DESIGN.md`
//! for the founding decisions and phase plan.
//!
//! Intended public API surface (nothing implemented yet — this crate is the
//! project scaffold):
//!
//! - `formats` — image readers/writers: libvips-backed formats plus native
//!   XISF (and possibly FITS) support, streaming at multi-GB scale.
//! - `wcs` — astrometric solutions: native carriage where containers support
//!   it, and the canonical JSON sidecar where they don't.
//! - `dzi` — Deep Zoom pyramid export and emission of the static,
//!   self-hostable HTML viewer.

#![warn(missing_docs)]

pub mod error;
pub use error::{Error, Result};

pub mod formats;
