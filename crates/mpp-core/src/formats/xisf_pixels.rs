//! Conversion of a decoded XISF image block to normalized planar `f32`.
//!
//! Input is the block's uncompressed bytes (still in the block's byte order
//! and pixel storage model). Output is one plane per channel, samples
//! normalized to `[0, 1]` by the image's representable range: `(v − offset −
//! lower) / (upper − lower)`, with `bounds` when declared and the format's
//! default otherwise ([`XisfHeader::effective_bounds`]). Floating-point
//! samples in the default `0:1` range with no `offset` are copied
//! bit-for-bit (NaN included), so a compressed copy of a file decodes
//! exactly like the uncompressed original served from the mmap.

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

/// Convert `raw` (the uncompressed block, `w·h·c` samples) into `out`
/// (`w·h·c` planar normalized f32). `raw` is byte-swapped in place for a
/// big-endian block.
pub(crate) fn to_planar_f32(
    h: &XisfHeader,
    raw: &mut [u8],
    out: &mut [f32],
) -> std::result::Result<(), String> {
    if let Some(why) = unsupported(h) {
        return Err(why);
    }
    let bps = h.sample_format.bytes_per_sample();
    let plane = (h.width * h.height) as usize;
    let channels = h.channels as usize;
    let n = plane * channels;
    if raw.len() != n * bps || out.len() != n {
        return Err(format!(
            "pixel buffer sizes {} / {} do not match {}x{}x{} {:?}",
            raw.len(),
            out.len(),
            h.width,
            h.height,
            h.channels,
            h.sample_format
        ));
    }
    if h.block.byte_order == ByteOrder::Big && bps > 1 {
        for s in raw.chunks_exact_mut(bps) {
            s.reverse();
        }
    }
    let (lo, hi) = h.effective_bounds();
    let float = matches!(
        h.sample_format,
        SampleFormat::Float32 | SampleFormat::Float64
    );
    let identity = float && lo == 0.0 && hi == 1.0 && h.offset == 0.0;
    let (shift, scale) = (h.offset + lo, 1.0 / (hi - lo));
    let sample = |k: usize| -> f32 {
        let b = &raw[k * bps..(k + 1) * bps];
        let v = match h.sample_format {
            SampleFormat::UInt8 => b[0] as f64,
            SampleFormat::UInt16 => u16::from_le_bytes([b[0], b[1]]) as f64,
            SampleFormat::UInt32 => u32::from_le_bytes(b.try_into().unwrap()) as f64,
            SampleFormat::Float32 => {
                let f = f32::from_le_bytes(b.try_into().unwrap());
                if identity {
                    return f;
                }
                f as f64
            }
            SampleFormat::Float64 => {
                let f = f64::from_le_bytes(b.try_into().unwrap());
                if identity {
                    return f as f32;
                }
                f
            }
            _ => unreachable!("refused above"),
        };
        ((v - shift) * scale) as f32
    };
    match h.pixel_storage {
        PixelStorage::Planar => {
            for (k, o) in out.iter_mut().enumerate() {
                *o = sample(k);
            }
        }
        PixelStorage::Normal => {
            for (c, dst) in out.chunks_exact_mut(plane).enumerate() {
                for (p, o) in dst.iter_mut().enumerate() {
                    *o = sample(p * channels + c);
                }
            }
        }
    }
    Ok(())
}
