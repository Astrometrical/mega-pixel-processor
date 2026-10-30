//! Conversion of a decoded XISF image block to normalized planar `f32`, and
//! to the native layout.
//!
//! Input is the block's uncompressed bytes (still in the block's byte order
//! and pixel storage model). The f32 output is one plane per channel,
//! samples normalized to `[0, 1]` by the image's representable range:
//! `(v − offset − lower) / (upper − lower)`, with `bounds` when declared and
//! the format's default otherwise ([`XisfHeader::effective_bounds`]).
//! Floating-point samples in the default `0:1` range with no `offset` are
//! copied bit-for-bit (NaN included), so a compressed copy of a file decodes
//! exactly like the uncompressed original served from the mmap.
//!
//! The native layout is the same samples rearranged, not converted: planar,
//! little-endian, in the stored sample format. [`Normalise`] converts it a
//! row at a time to exactly the values the f32 decode gives.

use super::SampleFormat;
use super::xisf::{ColorSpace, PixelStorage, XisfHeader};
use super::xisf_block::ByteOrder;

/// Why `h`'s pixels cannot be decoded at all, if they cannot: UInt64 and
/// complex samples and the CIE L*a*b* color space are refused by name.
pub(crate) fn unsupported(h: &XisfHeader) -> Option<String> {
    match h.sample_format {
        SampleFormat::UInt64 => Some("UInt64 pixel samples are not supported".into()),
        SampleFormat::Complex32 | SampleFormat::Complex64 => Some(format!(
            "complex pixel samples ({:?}) are not supported",
            h.sample_format
        )),
        _ if h.color_space == ColorSpace::CieLab => {
            Some("CIELab color space images are not supported".into())
        }
        _ => None,
    }
}

/// Whether the block's uncompressed bytes are already the planar
/// normalized f32 output (Float32, planar, little-endian, default `0:1`
/// range, no offset), so it can be decompressed straight into the output.
pub(crate) fn is_identity_layout(h: &XisfHeader) -> bool {
    h.sample_format == SampleFormat::Float32
        && h.pixel_storage == PixelStorage::Planar
        && h.block.byte_order == ByteOrder::Little
        && h.effective_bounds() == (0.0, 1.0)
        && h.offset == 0.0
}

/// Whether the block's uncompressed bytes are already in the native layout
/// (planar, little-endian, in the stored sample format), so it decompresses
/// straight into a native buffer, or is one in place when uncompressed.
pub(crate) fn is_native_layout(h: &XisfHeader) -> bool {
    h.pixel_storage == PixelStorage::Planar && h.block.byte_order == ByteOrder::Little
}

/// How an image's stored sample values become normalized f32: `(v − offset
/// − lower) / (upper − lower)`, except floating-point samples in the default
/// `0:1` range with no `offset`, which are taken as they are (Float32
/// bit-for-bit, NaN included).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Normalise {
    format: SampleFormat,
    shift: f64,
    scale: f64,
    identity: bool,
}

impl Normalise {
    /// The rule for `h`'s samples: its format, `bounds` and `offset`.
    pub(crate) fn of(h: &XisfHeader) -> Self {
        let (lo, hi) = h.effective_bounds();
        let float = matches!(
            h.sample_format,
            SampleFormat::Float32 | SampleFormat::Float64
        );
        Normalise {
            format: h.sample_format,
            shift: h.offset + lo,
            scale: 1.0 / (hi - lo),
            identity: float && lo == 0.0 && hi == 1.0 && h.offset == 0.0,
        }
    }

    /// The sample format converted.
    pub(crate) fn format(self) -> SampleFormat {
        self.format
    }

    #[inline]
    fn value(self, v: f64) -> f32 {
        ((v - self.shift) * self.scale) as f32
    }

    /// One sample from its bytes `b`, in either byte order.
    #[inline]
    fn sample(self, b: &[u8], big: bool) -> f32 {
        macro_rules! read {
            ($t:ty) => {{
                let b = b.try_into().unwrap();
                if big {
                    <$t>::from_be_bytes(b)
                } else {
                    <$t>::from_le_bytes(b)
                }
            }};
        }
        match self.format {
            SampleFormat::UInt8 => self.value(b[0] as f64),
            SampleFormat::UInt16 => self.value(read!(u16) as f64),
            SampleFormat::UInt32 => self.value(read!(u32) as f64),
            SampleFormat::Float32 => {
                let f: f32 = read!(f32);
                if self.identity {
                    f
                } else {
                    self.value(f as f64)
                }
            }
            SampleFormat::Float64 => {
                let f: f64 = read!(f64);
                if self.identity {
                    f as f32
                } else {
                    self.value(f)
                }
            }
            _ => unreachable!("refused at open"),
        }
    }

    /// Convert the little-endian samples `src` into `out`, one per element;
    /// the same values [`Self::sample`] gives, a format at a time so each
    /// loop is a straight conversion.
    pub(crate) fn convert_le(self, src: &[u8], out: &mut [f32]) {
        debug_assert_eq!(src.len(), out.len() * self.format.bytes_per_sample());
        let id = self.identity;
        match self.format {
            SampleFormat::UInt8 => {
                for (o, &b) in out.iter_mut().zip(src) {
                    *o = self.value(b as f64);
                }
            }
            SampleFormat::UInt16 => {
                for (o, b) in out.iter_mut().zip(src.as_chunks::<2>().0) {
                    *o = self.value(u16::from_le_bytes(*b) as f64);
                }
            }
            SampleFormat::UInt32 => {
                for (o, b) in out.iter_mut().zip(src.as_chunks::<4>().0) {
                    *o = self.value(u32::from_le_bytes(*b) as f64);
                }
            }
            SampleFormat::Float32 => {
                for (o, b) in out.iter_mut().zip(src.as_chunks::<4>().0) {
                    let f = f32::from_le_bytes(*b);
                    *o = if id { f } else { self.value(f as f64) };
                }
            }
            SampleFormat::Float64 => {
                for (o, b) in out.iter_mut().zip(src.as_chunks::<8>().0) {
                    let f = f64::from_le_bytes(*b);
                    *o = if id { f as f32 } else { self.value(f) };
                }
            }
            _ => unreachable!("refused at open"),
        }
    }
}

/// `raw` holds `w·h·c` samples and `out_len` matches it (`out_bps` bytes a
/// sample of output).
fn check_sizes(h: &XisfHeader, raw: usize, out_len: usize, out_bps: usize) -> Result<(), String> {
    let n = (h.width * h.height * h.channels) as usize;
    if raw != n * h.sample_format.bytes_per_sample() || out_len != n * out_bps {
        return Err(format!(
            "pixel buffer sizes {raw} / {out_len} do not match {}x{}x{} {:?}",
            h.width, h.height, h.channels, h.sample_format
        ));
    }
    Ok(())
}

/// Convert `raw` (the uncompressed block, `w·h·c` samples in the block's
/// byte order) into `out` (`w·h·c` planar normalized f32).
pub(crate) fn to_planar_f32(
    h: &XisfHeader,
    raw: &[u8],
    out: &mut [f32],
) -> std::result::Result<(), String> {
    if let Some(why) = unsupported(h) {
        return Err(why);
    }
    check_sizes(h, raw.len(), out.len(), 1)?;
    let norm = Normalise::of(h);
    let bps = h.sample_format.bytes_per_sample();
    let big = h.block.byte_order == ByteOrder::Big;
    match h.pixel_storage {
        PixelStorage::Planar if !big => norm.convert_le(raw, out),
        PixelStorage::Planar => {
            for (o, b) in out.iter_mut().zip(raw.chunks_exact(bps)) {
                *o = norm.sample(b, true);
            }
        }
        PixelStorage::Normal => {
            let (plane, channels) = ((h.width * h.height) as usize, h.channels as usize);
            for (c, dst) in out.chunks_exact_mut(plane).enumerate() {
                for (p, o) in dst.iter_mut().enumerate() {
                    let k = (p * channels + c) * bps;
                    *o = norm.sample(&raw[k..k + bps], big);
                }
            }
        }
    }
    Ok(())
}

/// Rearrange `raw` (the uncompressed block in its byte order and storage
/// model) into `out` in the native layout: planar, little-endian, the same
/// sample format, values untouched.
pub(crate) fn to_native_planar(
    h: &XisfHeader,
    raw: &[u8],
    out: &mut [u8],
) -> std::result::Result<(), String> {
    if let Some(why) = unsupported(h) {
        return Err(why);
    }
    let bps = h.sample_format.bytes_per_sample();
    check_sizes(h, raw.len(), out.len(), bps)?;
    let big = h.block.byte_order == ByteOrder::Big;
    match h.pixel_storage {
        PixelStorage::Planar if !big => out.copy_from_slice(raw),
        PixelStorage::Planar => {
            for (o, s) in out.chunks_exact_mut(bps).zip(raw.chunks_exact(bps)) {
                o.copy_from_slice(s);
                o.reverse();
            }
        }
        PixelStorage::Normal => {
            let (plane, channels) = ((h.width * h.height) as usize, h.channels as usize);
            for (c, dst) in out.chunks_exact_mut(plane * bps).enumerate() {
                for (p, o) in dst.chunks_exact_mut(bps).enumerate() {
                    let k = (p * channels + c) * bps;
                    o.copy_from_slice(&raw[k..k + bps]);
                    if big {
                        o.reverse();
                    }
                }
            }
        }
    }
    Ok(())
}
