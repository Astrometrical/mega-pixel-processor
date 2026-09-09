//! IFD-level TIFF/BigTIFF writer.
//!
//! Built directly on the `tiff` crate's `DirectoryEncoder` and its public
//! compressors rather than its `ImageEncoder`, because `ImageEncoder` has no
//! tile support and fixes the colour type at compile time through a marker
//! trait. Writing the IFD ourselves buys tiled output, classic-or-BigTIFF
//! from one code path, all five sample formats, and alpha, at the cost of
//! owning the tag set — see the tests, which assert on raw tag values as
//! well as decoded pixels.

pub mod options;
pub mod sample;

pub use options::{
    AlphaMode, Compression, Container, Layout, Resolution, ResolutionUnit, TiffWriteOptions,
};
pub use sample::Sample;
