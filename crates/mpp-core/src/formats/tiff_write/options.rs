//! Write options for [`crate::formats::tiff_write`] and their validation.
//!
//! Validation is deliberately separate from encoding: every rule the spec
//! states about legal option combinations is checked here, before a single
//! byte is written, so an impossible request fails immediately with an
//! actionable message rather than part-way through a multi-gigabyte encode.

use crate::formats::tiff::BitDepth;
use crate::{Error, Result};

/// Largest payload a classic (32-bit) TIFF can address, with room left for
/// the header, the IFD, and its out-of-line values.
const CLASSIC_TIFF_PAYLOAD_LIMIT: u64 = (4u64 << 30) - (16 << 20);

/// Compression applied to each strip or tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression (default). Fastest, largest, and what every reader
    /// supports.
    #[default]
    None,
    /// LZW (TIFF compression 5). Lossless, widely supported.
    Lzw,
    /// Deflate/zlib (TIFF compression 8) at [`TiffWriteOptions::deflate_level`].
    Deflate,
    /// PackBits run-length encoding (TIFF compression 32773).
    Packbits,
}

/// Which TIFF container variant to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Container {
    /// Classic TIFF when the payload fits, BigTIFF otherwise (default).
    #[default]
    Auto,
    /// Classic 32-bit-offset TIFF. Fails validation if the payload cannot
    /// fit; the widest reader compatibility.
    Classic,
    /// BigTIFF (64-bit offsets). Required past 4 GB.
    Big,
}

/// How pixel data is divided into independently addressed blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Full-width horizontal strips, sized to roughly 4 MiB each (default).
    #[default]
    Strips,
    /// Square tiles of `size` pixels per edge. Wanted by the Astrometrical
    /// large-image load path, which reads sub-regions rather than rows.
    Tiles {
        /// Tile edge in pixels; a power of two in `128..=1024`.
        size: u32,
    },
}

/// Whether the last channel is an alpha channel, and how it relates to the
/// colour channels (TIFF's `ExtraSamples` tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// No alpha channel; 1 or 3 channels (default).
    #[default]
    None,
    /// Alpha is not premultiplied into the colour channels
    /// (`ExtraSamples = 2`). The usual choice for a mask.
    Unassociated,
    /// Alpha is premultiplied into the colour channels (`ExtraSamples = 1`).
    Associated,
}

/// Unit the resolution values are expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionUnit {
    /// Pixels per inch (default; TIFF `ResolutionUnit = 2`).
    #[default]
    Inch,
    /// Pixels per centimetre (TIFF `ResolutionUnit = 3`).
    Centimeter,
}

/// Physical resolution written to `XResolution`/`YResolution`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolution {
    /// Horizontal resolution in pixels per [`Resolution::unit`].
    pub x: f64,
    /// Vertical resolution in pixels per [`Resolution::unit`].
    pub y: f64,
    /// Unit the two values are expressed in.
    pub unit: ResolutionUnit,
}

/// Options for `crate::formats::tiff_write::write_tiff` (added by a later
/// task; not yet an intra-doc link until it exists).
#[derive(Debug, Clone, Default)]
pub struct TiffWriteOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
    /// Container variant.
    pub container: Container,
    /// Strip or tile layout.
    pub layout: Layout,
    /// Per-block compression.
    pub compression: Compression,
    /// Deflate level, 1-9; ignored unless [`Compression::Deflate`]. `None`
    /// selects the default level 6.
    pub deflate_level: Option<u8>,
    /// Apply the horizontal differencing predictor (TIFF tag 317). Integer
    /// sample formats only.
    pub predictor: bool,
    /// Whether the final channel is alpha, and its association.
    pub alpha: AlphaMode,
    /// ICC profile bytes to embed in tag 34675.
    pub icc_profile: Option<Vec<u8>>,
    /// `ImageDescription` (tag 270).
    pub description: Option<String>,
    /// `Copyright` (tag 33432).
    pub copyright: Option<String>,
    /// `Software` (tag 305).
    pub software: Option<String>,
    /// Physical resolution; omitted from the file when `None`.
    pub resolution: Option<Resolution>,
}

impl TiffWriteOptions {
    /// Check every option against the image geometry and resolve
    /// [`Container::Auto`] to a concrete container.
    ///
    /// Returns the container the encoder should write. Errors name the
    /// offending option and what would make it legal.
    pub fn validate(&self, width: u64, height: u64, channels: u64) -> Result<Container> {
        if width == 0 || height == 0 {
            return Err(Error::invalid(format!(
                "image dimensions must both be nonzero (got {width}x{height})"
            )));
        }
        if !(1..=4).contains(&channels) {
            return Err(Error::invalid(format!(
                "channel count {channels} is not supported (1-4: gray, gray+alpha, RGB, RGBA)"
            )));
        }

        let has_alpha = self.alpha != AlphaMode::None;
        let alpha_channels = if has_alpha { 1 } else { 0 };
        let colour = channels - alpha_channels;
        if has_alpha && channels < 2 {
            return Err(Error::invalid(format!(
                "an alpha association needs at least 2 channels (got {channels})"
            )));
        }
        if colour != 1 && colour != 3 {
            return Err(Error::invalid(format!(
                "{channels} channels with alpha={:?} leaves {colour} colour channel(s); \
                 expected 1 (gray) or 3 (RGB)",
                self.alpha
            )));
        }

        if self.predictor && matches!(self.bit_depth, BitDepth::F32 | BitDepth::F64) {
            return Err(Error::invalid(
                "the horizontal predictor is defined for integer samples only; \
                 turn it off or choose an 8/16/32-bit unsigned sample format"
                    .to_string(),
            ));
        }

        if let Layout::Tiles { size } = self.layout
            && (!(128..=1024).contains(&size) || !size.is_power_of_two())
        {
            return Err(Error::invalid(format!(
                "tile size {size} is not supported (a power of two in 128..=1024)"
            )));
        }

        if let Some(level) = self.deflate_level
            && !(1..=9).contains(&level)
        {
            return Err(Error::invalid(format!(
                "deflate level {level} is out of range (1-9)"
            )));
        }

        // Uncompressed payload; compression only ever shrinks it, so a
        // classic container that fits this bound always fits the real file.
        let payload = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(channels))
            .and_then(|v| v.checked_mul(self.bit_depth.bytes() as u64))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "image {width}x{height}x{channels} overflows the addressable payload size"
                ))
            })?;
        let fits_classic = payload <= CLASSIC_TIFF_PAYLOAD_LIMIT;

        match self.container {
            Container::Auto => Ok(if fits_classic {
                Container::Classic
            } else {
                Container::Big
            }),
            Container::Classic if !fits_classic => Err(Error::invalid(format!(
                "a classic TIFF cannot address this image ({payload} bytes of pixel data \
                 exceeds the 4 GB limit); choose BigTIFF"
            ))),
            other => Ok(other),
        }
    }

    /// Deflate level to use, defaulting to 6.
    pub fn effective_deflate_level(&self) -> u8 {
        self.deflate_level.unwrap_or(6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TiffWriteOptions {
        TiffWriteOptions::default()
    }

    #[test]
    fn default_is_uncompressed_stripped_auto_f32() {
        let o = opts();
        assert_eq!(o.bit_depth, BitDepth::F32);
        assert_eq!(o.compression, Compression::None);
        assert_eq!(o.container, Container::Auto);
        assert_eq!(o.layout, Layout::Strips);
        assert_eq!(o.alpha, AlphaMode::None);
    }

    #[test]
    fn auto_picks_classic_for_a_small_image() {
        let o = opts();
        // 1000 x 1000 RGB f32 = 12 MB, comfortably inside classic TIFF.
        assert_eq!(o.validate(1000, 1000, 3).unwrap(), Container::Classic);
    }

    #[test]
    fn auto_picks_big_past_the_four_gigabyte_bound() {
        let o = opts();
        // 40000 x 30000 RGB f32 = 14.4 GB.
        assert_eq!(o.validate(40_000, 30_000, 3).unwrap(), Container::Big);
    }

    #[test]
    fn explicit_classic_rejects_an_oversized_image() {
        let o = TiffWriteOptions {
            container: Container::Classic,
            ..opts()
        };
        let err = o.validate(40_000, 30_000, 3).unwrap_err().to_string();
        assert!(
            err.contains("BigTIFF"),
            "error should point the user at BigTIFF, got: {err}"
        );
    }

    #[test]
    fn predictor_is_rejected_for_float_samples() {
        let o = TiffWriteOptions {
            predictor: true,
            bit_depth: BitDepth::F32,
            ..opts()
        };
        assert!(o.validate(16, 16, 1).is_err());
        let o = TiffWriteOptions {
            predictor: true,
            bit_depth: BitDepth::U16,
            ..opts()
        };
        assert!(o.validate(16, 16, 1).is_ok());
    }

    #[test]
    fn tile_size_must_be_a_power_of_two_in_range() {
        for (size, ok) in [
            (64u32, false),
            (128, true),
            (256, true),
            (1024, true),
            (2048, false),
            (300, false),
        ] {
            let o = TiffWriteOptions {
                layout: Layout::Tiles { size },
                ..opts()
            };
            assert_eq!(o.validate(4096, 4096, 1).is_ok(), ok, "tile size {size}");
        }
    }

    #[test]
    fn alpha_and_channel_count_must_agree_both_ways() {
        let with_alpha = TiffWriteOptions {
            alpha: AlphaMode::Unassociated,
            ..opts()
        };
        assert!(with_alpha.validate(8, 8, 2).is_ok());
        assert!(with_alpha.validate(8, 8, 4).is_ok());
        assert!(with_alpha.validate(8, 8, 1).is_err());
        assert!(with_alpha.validate(8, 8, 3).is_err());
        // 2 and 4 channels without an alpha association would be a multiband
        // image, which this writer does not produce.
        assert!(opts().validate(8, 8, 2).is_err());
        assert!(opts().validate(8, 8, 4).is_err());
        assert!(opts().validate(8, 8, 1).is_ok());
        assert!(opts().validate(8, 8, 3).is_ok());
    }

    #[test]
    fn channel_count_outside_one_to_four_is_rejected() {
        assert!(opts().validate(8, 8, 0).is_err());
        assert!(opts().validate(8, 8, 5).is_err());
    }
}
