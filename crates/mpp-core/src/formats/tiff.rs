//! Strip-streaming BigTIFF writer.
//!
//! Always writes BigTIFF (the 64-bit TIFF variant): the whole point of the
//! converter is escaping classic TIFF's 4 GB limit, and every consumer we
//! target (Photoshop, libvips, libtiff ≥ 4.x) reads BigTIFF. Pixel data is
//! interleaved from the reader's planar planes strip by strip — memory use
//! is one strip (~`TARGET_STRIP_BYTES`), independent of image size.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use tiff::encoder::{TiffEncoder, colortype};

use crate::formats::xisf::XisfImage;
use crate::{Error, Result};

/// Target uncompressed strip size; rows per strip is derived from this.
const TARGET_STRIP_BYTES: u64 = 4 << 20;

/// Output sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    /// 32-bit IEEE float samples, passthrough from XISF Float32 (default).
    #[default]
    F32,
    /// 16-bit unsigned samples; input clamped to [0, 1] and scaled.
    U16,
}

/// Options for [`write_bigtiff`].
#[derive(Debug, Clone, Default)]
pub struct TiffWriteOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
}

/// Convert a unit-range f32 sample to u16 (clamped, rounded).
fn to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// Write `img` to `path` as an uncompressed BigTIFF, streaming strip by
/// strip in constant memory. Grayscale (1 channel) and RGB (3 channels)
/// are supported; XISF planar planes are interleaved to TIFF chunky order.
pub fn write_bigtiff(img: &XisfImage, path: &Path, opts: &TiffWriteOptions) -> Result<()> {
    write_bigtiff_with_target(img, path, opts, TARGET_STRIP_BYTES)
}

/// [`write_bigtiff`], parameterized on the target strip size — split out so
/// tests can force multi-strip (including a partial last strip) output
/// without needing a multi-gigabyte image.
fn write_bigtiff_with_target(
    img: &XisfImage,
    path: &Path,
    opts: &TiffWriteOptions,
    target_strip_bytes: u64,
) -> Result<()> {
    let w = u32::try_from(img.width())
        .map_err(|_| Error::format(path, "image width exceeds TIFF limits"))?;
    let h = u32::try_from(img.height())
        .map_err(|_| Error::format(path, "image height exceeds TIFF limits"))?;
    let channels = img.channels();
    let terr = |e: tiff::TiffError| Error::format(path, format!("TIFF encode: {e}"));

    let file = File::create(path).map_err(|e| Error::io(path, e))?;
    let mut buf = BufWriter::new(file);
    let mut enc = TiffEncoder::new_big(&mut buf).map_err(terr)?;

    // One code path per (channels, depth) combination; the macro exists
    // because each combination needs a different tiff colortype marker
    // type, which cannot be chosen at runtime.
    macro_rules! stream {
        ($ct:ty, $sample:ty, $map:expr) => {{
            let mut image = enc.new_image::<$ct>(w, h).map_err(terr)?;
            let row_bytes = w as u64 * channels * std::mem::size_of::<$sample>() as u64;
            let rows = (target_strip_bytes / row_bytes.max(1)).clamp(1, h.max(1) as u64) as u32;
            image.rows_per_strip(rows).map_err(terr)?;
            let mut strip: Vec<$sample> = Vec::new();
            let mut y: u64 = 0;
            while image.next_strip_sample_count() > 0 {
                let n = image.next_strip_sample_count() as usize;
                let rows_here = n / (w as usize * channels as usize);
                strip.clear();
                for yy in y..y + rows_here as u64 {
                    let planes: Vec<&[f32]> = (0..channels).map(|c| img.row(c, yy)).collect();
                    for x in 0..w as usize {
                        for p in &planes {
                            strip.push($map(p[x]));
                        }
                    }
                }
                image.write_strip(&strip).map_err(terr)?;
                y += rows_here as u64;
            }
            image.finish().map_err(terr)?;
        }};
    }

    match (channels, opts.bit_depth) {
        (1, BitDepth::F32) => stream!(colortype::Gray32Float, f32, |v: f32| v),
        (3, BitDepth::F32) => stream!(colortype::RGB32Float, f32, |v: f32| v),
        (1, BitDepth::U16) => stream!(colortype::Gray16, u16, to_u16),
        (3, BitDepth::U16) => stream!(colortype::RGB16, u16, to_u16),
        (c, _) => {
            return Err(Error::format(
                path,
                format!("unsupported channel count {c} (grayscale or RGB only)"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::xisf::XisfImage;
    use crate::testsupport::synth_xisf;
    use std::path::PathBuf;
    use tiff::decoder::{Decoder, DecodingResult};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-tiff-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_bigtiff_gray_f32() {
        let dir = tmpdir("gray");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 5, 4, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        // BigTIFF magic: little-endian "II", version 43.
        let head = std::fs::read(&out).unwrap();
        assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00]);

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (5, 4));
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 20);
                assert_eq!(data[0], 0.0);
                assert!((data[19] - 19.0 / 20.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_bigtiff_rgb_interleaved() {
        let dir = tmpdir("rgb");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 3, 2, 3, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                // Planar XISF (plane size 6) interleaves to RGBRGB…:
                // pixel 0 = planes' sample 0 = [0, 6, 12] / 18.
                assert_eq!(data.len(), 18);
                assert!((data[0] - 0.0 / 18.0).abs() < 1e-7);
                assert!((data[1] - 6.0 / 18.0).abs() < 1e-7);
                assert!((data[2] - 12.0 / 18.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u16_output_scales_unit_range() {
        let dir = tmpdir("u16");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 4, 4, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        let opts = TiffWriteOptions {
            bit_depth: BitDepth::U16,
        };
        write_bigtiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(data) => {
                assert_eq!(data[0], 0);
                // sample 15 = 15/16 → round(15/16 * 65535) = 61439.
                assert_eq!(data[15], 61439);
            }
            other => panic!("expected U16 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strip_streaming_covers_multi_strip_images() {
        // 3x7 gray f32: row_bytes = 3*4 = 12; with a forced 64-byte target,
        // rows_per_strip = 64/12 = 5, so a 7-row image spans two strips:
        // 5 rows then a partial 2-row strip. Exercises `y` carrying across
        // iterations, the partial last strip, and `strip` buffer reuse
        // across multiple `write_strip` calls.
        let dir = tmpdir("strips");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 3, 7, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff_with_target(&img, &out, &TiffWriteOptions::default(), 64).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                let total = (3 * 7) as f32;
                assert_eq!(data.len(), 21);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / total).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unsupported_channel_count() {
        let dir = tmpdir("chans");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 2, 2, 2, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        assert!(write_bigtiff(&img, &out, &TiffWriteOptions::default()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
