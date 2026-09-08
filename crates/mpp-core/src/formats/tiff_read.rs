//! Streaming TIFF/BigTIFF reader.
//!
//! Opens a TIFF (classic or BigTIFF), validates its header against the
//! colour/sample-format subset this reader supports, and decodes it
//! chunk-by-chunk (strip or tile) into de-interleaved per-channel row
//! bands, without ever holding the whole image in memory. Each chunk is
//! decoded exactly once. This is the read-side counterpart to
//! [`super::tiff`], which only writes.
//!
//! Designed from the public TIFF 6.0 and BigTIFF specifications and the
//! `tiff` crate's own (0.11.3) public API and doc comments — never from
//! PixInsight's PCL module sources (see the clean-room rule in
//! `CLAUDE.md`).

use std::fs::File;
use std::path::{Path, PathBuf};

use tiff::ColorType as TiffColorType;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::{Error, Result};

/// Native sample format of a decoded TIFF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 8-bit unsigned integer samples.
    U8,
    /// 16-bit unsigned integer samples.
    U16,
    /// 32-bit unsigned integer samples.
    U32,
    /// 32-bit IEEE float samples.
    F32,
    /// 64-bit IEEE float samples.
    F64,
}

impl SampleFormat {
    /// Size of one sample of this format, in bytes.
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Short name used on the IPC wire (`"u8"`, `"u16"`, `"u32"`, `"f32"`,
    /// `"f64"`).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

/// Nominal colour interpretation (alpha counted separately, see
/// [`TiffInfo::alpha_channels`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorClass {
    /// A single colour channel.
    Gray,
    /// Three colour channels, red/green/blue.
    Rgb,
}

/// Geometry + format of a TIFF, read from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TiffInfo {
    /// Image width in pixels.
    pub width: u64,
    /// Image height in pixels.
    pub height: u64,
    /// Nominal colour interpretation.
    pub color: ColorClass,
    /// Colour channel count: 1 for [`ColorClass::Gray`], 3 for
    /// [`ColorClass::Rgb`].
    pub color_channels: u32,
    /// Alpha channel count: 0 or 1.
    pub alpha_channels: u32,
    /// Native sample format shared by every channel.
    pub sample_format: SampleFormat,
}

impl TiffInfo {
    /// Total channel count: colour channels plus alpha.
    pub fn total_channels(&self) -> u32 {
        self.color_channels + self.alpha_channels
    }
}

/// A streaming TIFF/BigTIFF reader.
///
/// Opens a file and validates its header up front (see [`TiffReader::open`]);
/// [`TiffReader::read_bands`] then decodes the image chunk-by-chunk (strip or
/// tile, whichever the file uses) and hands the caller de-interleaved,
/// native-endian per-channel row bands in chunk order. Peak memory is one
/// decoded chunk (plus, for tiled images, one row of tile bands) — never the
/// whole image.
#[derive(Debug)]
pub struct TiffReader {
    decoder: Decoder<File>,
    info: TiffInfo,
    path: PathBuf,
}

impl TiffReader {
    /// Open `path` and read+validate the first image's header.
    ///
    /// Errors on palette/CMYK/YCbCr/Lab/Multiband colour, signed/complex
    /// samples, or a bit depth this reader does not recognize.
    pub fn open(path: &Path) -> Result<TiffReader> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        let mut decoder =
            Decoder::new(file).map_err(|e| Error::format(path, format!("TIFF header: {e}")))?;

        let (width, height) = decoder
            .dimensions()
            .map_err(|e| Error::format(path, format!("TIFF dimensions: {e}")))?;

        // A zero-dimension image has no pixels to read and, worse, would let
        // a consumer that sizes bands as `bytes / (width * sample_bytes)`
        // (the module's read sink does exactly this) divide by zero. Reject
        // it here so it surfaces as a clean, named error at open time.
        if width == 0 || height == 0 {
            return Err(Error::format(
                path,
                format!("image has a zero dimension ({width}x{height}); nothing to read"),
            ));
        }

        let color_type = decoder
            .colortype()
            .map_err(|e| Error::format(path, format!("unsupported TIFF colour type: {e}")))?;

        let (color, color_channels, alpha_channels, bits) = match color_type {
            TiffColorType::Gray(b) => (ColorClass::Gray, 1, 0, b),
            TiffColorType::GrayA(b) => (ColorClass::Gray, 1, 1, b),
            TiffColorType::RGB(b) => (ColorClass::Rgb, 3, 0, b),
            TiffColorType::RGBA(b) => (ColorClass::Rgb, 3, 1, b),
            other => {
                return Err(Error::format(
                    path,
                    format!("unsupported TIFF colour type: {other:?}"),
                ));
            }
        };

        // TIFF SampleFormat tag: 1 = unsigned int (default when absent),
        // 2 = signed int, 3 = IEEE float, 4 = void/complex. Written with one
        // value per sample (e.g. 3 identical entries for RGB); we require a
        // uniform format across channels and read just the first entry.
        let sample_format_tag = decoder
            .find_tag_unsigned_vec::<u16>(Tag::SampleFormat)
            .map_err(|e| Error::format(path, format!("TIFF SampleFormat tag: {e}")))?
            .and_then(|v| v.first().copied())
            .unwrap_or(1);

        // This reader de-interleaves chunky (interleaved) chunks only; a
        // planar-configuration file (each chunk holds one channel's worth
        // of samples, not one pixel's) would silently mis-decode under the
        // interleaved assumption below, so reject it explicitly instead.
        let planar_config = decoder
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
            .map_err(|e| Error::format(path, format!("TIFF PlanarConfiguration tag: {e}")))?
            .unwrap_or(1);
        if planar_config != 1 {
            return Err(Error::format(
                path,
                "unsupported TIFF planar configuration: separate (planar) sample storage is not supported",
            ));
        }

        let sample_format = match (sample_format_tag, bits) {
            (1, 8) => SampleFormat::U8,
            (1, 16) => SampleFormat::U16,
            (1, 32) => SampleFormat::U32,
            (3, 32) => SampleFormat::F32,
            (3, 64) => SampleFormat::F64,
            (2, b) => {
                return Err(Error::format(
                    path,
                    format!("signed integer samples are not supported ({b}-bit)"),
                ));
            }
            (1 | 3, b) => {
                return Err(Error::format(
                    path,
                    format!("unsupported TIFF bit depth: {b}"),
                ));
            }
            (fmt, b) => {
                return Err(Error::format(
                    path,
                    format!(
                        "unsupported TIFF sample format code {fmt} ({b}-bit; complex/void samples are not supported)"
                    ),
                ));
            }
        };

        let info = TiffInfo {
            width: u64::from(width),
            height: u64::from(height),
            color,
            color_channels,
            alpha_channels,
            sample_format,
        };

        Ok(TiffReader {
            decoder,
            info,
            path: path.to_path_buf(),
        })
    }

    /// The geometry and native sample format read from the header.
    pub fn info(&self) -> &TiffInfo {
        &self.info
    }

    /// Decode the image chunk-by-chunk (strip or tile) and invoke `sink`
    /// once per (channel, contiguous row band) with the de-interleaved
    /// native-endian sample bytes for that single channel. `bytes.len() ==
    /// rows as usize * width as usize * sample_format.bytes()`. Channels are
    /// `0..total_channels()`; bands cover every row of every channel exactly
    /// once. Order is chunk order (top-down for strips; for tiled images, a
    /// full grid row of tiles is decoded and de-tiled into full-width bands
    /// before any band of that tile-row is emitted, so at most one tile-row
    /// of chunks is held at a time).
    #[allow(clippy::type_complexity)]
    pub fn read_bands(
        &mut self,
        sink: &mut dyn FnMut(u32, u64, u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let path = self.path.clone();
        let total_channels = self.info.total_channels();
        let sample_format = self.info.sample_format;
        let width = self.info.width;

        match self.decoder.get_chunk_type() {
            ChunkType::Strip => self.read_strips(&path, total_channels, sample_format, width, sink),
            ChunkType::Tile => self.read_tiles(&path, total_channels, sample_format, width, sink),
        }
    }

    /// [`Self::read_bands`] for a stripped image: every strip spans the
    /// full image width, so each decoded chunk maps directly to one band
    /// per channel.
    #[allow(clippy::type_complexity)]
    fn read_strips(
        &mut self,
        path: &Path,
        total_channels: u32,
        sample_format: SampleFormat,
        width: u64,
        sink: &mut dyn FnMut(u32, u64, u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let strip_count = self
            .decoder
            .strip_count()
            .map_err(|e| Error::format(path, format!("TIFF strip count: {e}")))?;
        let (_, rows_per_strip) = self.decoder.chunk_dimensions();

        for i in 0..strip_count {
            let (w_c, h_c) = self.decoder.chunk_data_dimensions(i);
            if u64::from(w_c) != width {
                return Err(Error::format(
                    path,
                    format!("strip {i} width {w_c} does not match image width {width}"),
                ));
            }
            let start_row = u64::from(i) * u64::from(rows_per_strip);
            let result = self
                .decoder
                .read_chunk(i)
                .map_err(|e| Error::format(path, format!("TIFF strip {i}: {e}")))?;
            let planes = deinterleave(path, result, total_channels, sample_format)?;
            for (ch, bytes) in planes.iter().enumerate() {
                sink(ch as u32, start_row, h_c, bytes)?;
            }
        }
        Ok(())
    }

    /// [`Self::read_bands`] for a tiled image: a tile only spans part of a
    /// row, so a full grid row of tiles is decoded and de-tiled into
    /// per-channel full-width row bands before those bands are emitted.
    /// Only one tile-row's worth of chunks is held in memory at a time.
    #[allow(clippy::type_complexity)]
    fn read_tiles(
        &mut self,
        path: &Path,
        total_channels: u32,
        sample_format: SampleFormat,
        width: u64,
        sink: &mut dyn FnMut(u32, u64, u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let sample_bytes = sample_format.bytes();
        let (tile_w, tile_h) = self.decoder.chunk_dimensions();
        let tile_count = self
            .decoder
            .tile_count()
            .map_err(|e| Error::format(path, format!("TIFF tile count: {e}")))?;
        if tile_count == 0 || tile_w == 0 {
            return Ok(());
        }
        let tiles_across = (width as u32).div_ceil(tile_w);
        let tiles_down = tile_count.div_ceil(tiles_across);

        for tr in 0..tiles_down {
            let mut row_bands: Vec<Vec<u8>> = Vec::new();
            let mut band_rows: u32 = 0;
            for tc in 0..tiles_across {
                let idx = tr * tiles_across + tc;
                let (w_c, h_c) = self.decoder.chunk_data_dimensions(idx);
                band_rows = h_c;
                if row_bands.is_empty() {
                    row_bands = (0..total_channels)
                        .map(|_| vec![0u8; h_c as usize * width as usize * sample_bytes])
                        .collect();
                }
                let result = self
                    .decoder
                    .read_chunk(idx)
                    .map_err(|e| Error::format(path, format!("TIFF tile {idx}: {e}")))?;
                let planes = deinterleave(path, result, total_channels, sample_format)?;
                let x_off = u64::from(tc) * u64::from(tile_w);
                for (ch, src) in planes.iter().enumerate() {
                    let dst = &mut row_bands[ch];
                    for row in 0..h_c as usize {
                        let src_off = row * w_c as usize * sample_bytes;
                        let dst_off = (row * width as usize + x_off as usize) * sample_bytes;
                        dst[dst_off..dst_off + w_c as usize * sample_bytes]
                            .copy_from_slice(&src[src_off..src_off + w_c as usize * sample_bytes]);
                    }
                }
            }
            let start_row = u64::from(tr) * u64::from(tile_h);
            for (ch, bytes) in row_bands.iter().enumerate() {
                sink(ch as u32, start_row, band_rows, bytes)?;
            }
        }
        Ok(())
    }
}

/// De-interleave one decoded chunk's samples into one contiguous
/// native-endian byte buffer per channel, row-major within the chunk.
fn deinterleave(
    path: &Path,
    result: DecodingResult,
    total_channels: u32,
    sample_format: SampleFormat,
) -> Result<Vec<Vec<u8>>> {
    let channels = total_channels as usize;
    let sample_bytes = sample_format.bytes();

    macro_rules! planes_from {
        ($data:expr) => {{
            let data = $data;
            let mut planes: Vec<Vec<u8>> = (0..channels)
                .map(|_| Vec::with_capacity(data.len() / channels.max(1) * sample_bytes))
                .collect();
            for (i, v) in data.iter().enumerate() {
                planes[i % channels].extend_from_slice(&v.to_ne_bytes());
            }
            planes
        }};
    }

    let planes = match (result, sample_format) {
        (DecodingResult::U8(d), SampleFormat::U8) => planes_from!(d),
        (DecodingResult::U16(d), SampleFormat::U16) => planes_from!(d),
        (DecodingResult::U32(d), SampleFormat::U32) => planes_from!(d),
        (DecodingResult::F32(d), SampleFormat::F32) => planes_from!(d),
        (DecodingResult::F64(d), SampleFormat::F64) => planes_from!(d),
        (other, fmt) => {
            return Err(Error::format(
                path,
                format!(
                    "internal error: decoded chunk sample type {other:?} does not match declared format {fmt:?}"
                ),
            ));
        }
    };
    Ok(planes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tiff::encoder::{TiffEncoder, colortype};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-tiff-read-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_rgb16(path: &Path, w: u32, h: u32) {
        let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
        let mut data = vec![0u16; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 3) as usize;
                data[i] = (x as u16).wrapping_mul(7);
                data[i + 1] = (y as u16).wrapping_mul(11);
                data[i + 2] = ((x + y) as u16).wrapping_mul(13);
            }
        }
        enc.write_image::<colortype::RGB16>(w, h, &data).unwrap();
    }

    #[test]
    fn open_reports_rgb16_geometry() {
        let dir = tmpdir("rgb16-geom");
        let path = dir.join("in.tif");
        write_rgb16(&path, 40, 30);
        let r = TiffReader::open(&path).unwrap();
        let info = r.info();
        assert_eq!((info.width, info.height), (40, 30));
        assert_eq!(info.color, ColorClass::Rgb);
        assert_eq!(info.color_channels, 3);
        assert_eq!(info.alpha_channels, 0);
        assert_eq!(info.sample_format, SampleFormat::U16);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_bands_reassembles_rgb16_planes() {
        let (w, h) = (40u32, 30u32);
        let dir = tmpdir("rgb16-bands");
        let path = dir.join("in.tif");
        write_rgb16(&path, w, h);
        let mut r = TiffReader::open(&path).unwrap();
        // Reassemble planar channels from the emitted bands.
        let mut planes = vec![vec![0u16; (w * h) as usize]; 3];
        r.read_bands(&mut |ch, start_row, rows, bytes| {
            assert_eq!(bytes.len(), rows as usize * w as usize * 2);
            for rr in 0..rows as u64 {
                for x in 0..w as u64 {
                    let b = ((rr * w as u64 + x) * 2) as usize;
                    let v = u16::from_ne_bytes([bytes[b], bytes[b + 1]]);
                    let px = ((start_row + rr) * w as u64 + x) as usize;
                    planes[ch as usize][px] = v;
                }
            }
            Ok(())
        })
        .unwrap();
        // Channel 1 (green) is y*11 across the row.
        assert_eq!(planes[1][5 * w as usize], 5u16.wrapping_mul(11));
        assert_eq!(planes[0][3], 3u16.wrapping_mul(7));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_rejects_cmyk() {
        // The `tiff` crate's encoder has no `colortype::Palette*` marker
        // type (it cannot write a palette image), so this exercises the
        // same "reject unsupported colour type" branch with CMYK instead —
        // another colour type explicitly out of scope per the design spec's
        // colour-mapping table. `TiffReader::open` must still fail with a
        // message naming the unsupported colour type.
        let dir = tmpdir("cmyk");
        let path = dir.join("in.tif");
        let mut enc = TiffEncoder::new(std::fs::File::create(&path).unwrap()).unwrap();
        let data = vec![0u8; 4 * 2 * 2]; // 2x2 pixels, 4 samples/pixel (C,M,Y,K)
        enc.write_image::<colortype::CMYK8>(2, 2, &data).unwrap();

        let err = TiffReader::open(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("cmyk"),
            "expected error to name the unsupported colour type, got: {msg}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_rejects_signed_integer_samples() {
        let dir = tmpdir("signed");
        let path = dir.join("in.tif");
        let mut enc = TiffEncoder::new(std::fs::File::create(&path).unwrap()).unwrap();
        let data = vec![0i16; 4];
        enc.write_image::<colortype::GrayI16>(2, 2, &data).unwrap();

        let err = TiffReader::open(&path).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("signed"),
            "expected error to mention signed samples, got: {err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_reports_gray8() {
        let dir = tmpdir("gray8");
        let path = dir.join("in.tif");
        let mut enc = TiffEncoder::new(std::fs::File::create(&path).unwrap()).unwrap();
        let data: Vec<u8> = (0..16u8).collect();
        enc.write_image::<colortype::Gray8>(4, 4, &data).unwrap();

        let r = TiffReader::open(&path).unwrap();
        assert_eq!(r.info().color, ColorClass::Gray);
        assert_eq!(r.info().sample_format, SampleFormat::U8);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_bands_reassembles_rgba8_including_alpha() {
        // Alpha is in v1 read scope (RGBA -> 4 channels): prove the alpha
        // plane round-trips end-to-end alongside the colour planes, with a
        // distinct per-pixel alpha value so a dropped/duplicated alpha shows
        // up as a mismatch.
        let (w, h) = (5u32, 3u32);
        let dir = tmpdir("rgba8");
        let path = dir.join("in.tif");
        let mut enc = TiffEncoder::new(std::fs::File::create(&path).unwrap()).unwrap();
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = (x as u8).wrapping_mul(3).wrapping_add(1); // R
                data[i + 1] = (y as u8).wrapping_mul(5).wrapping_add(2); // G
                data[i + 2] = ((x + y) as u8).wrapping_mul(7).wrapping_add(3); // B
                data[i + 3] = ((x * w + y) as u8).wrapping_mul(11).wrapping_add(4); // A
            }
        }
        enc.write_image::<colortype::RGBA8>(w, h, &data).unwrap();

        let mut r = TiffReader::open(&path).unwrap();
        assert_eq!(r.info().color, ColorClass::Rgb);
        assert_eq!(r.info().color_channels, 3);
        assert_eq!(r.info().alpha_channels, 1);
        assert_eq!(r.info().total_channels(), 4);
        assert_eq!(r.info().sample_format, SampleFormat::U8);

        let mut planes = vec![vec![0u8; (w * h) as usize]; 4];
        r.read_bands(&mut |ch, start_row, rows, bytes| {
            assert_eq!(bytes.len(), rows as usize * w as usize);
            for rr in 0..rows as u64 {
                for x in 0..w as u64 {
                    let px = ((start_row + rr) * w as u64 + x) as usize;
                    planes[ch as usize][px] = bytes[(rr * w as u64 + x) as usize];
                }
            }
            Ok(())
        })
        .unwrap();

        // Verify every plane, alpha (channel 3) included, against the source.
        for y in 0..h {
            for x in 0..w {
                let src = ((y * w + x) * 4) as usize;
                let px = (y * w + x) as usize;
                for ch in 0..4 {
                    assert_eq!(
                        planes[ch][px],
                        data[src + ch],
                        "channel {ch} pixel ({x},{y}) mismatch"
                    );
                }
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_rejects_zero_dimension() {
        // A zero-dimension image must be rejected at open time with a clear,
        // named error rather than reaching a consumer that computes
        // `band_bytes / (width * sample_bytes)` and divides by zero. The
        // `tiff` crate's encoder cannot itself write a 0-dimension image, so
        // build a minimal classic TIFF by hand with ImageWidth = 0.
        let dir = tmpdir("zero-dim");
        let path = dir.join("in.tif");

        struct Entry {
            tag: u16,
            ty: u16,
            count: u32,
            value: [u8; 4],
        }
        fn short(v: u16) -> [u8; 4] {
            let mut b = [0u8; 4];
            b[0..2].copy_from_slice(&v.to_le_bytes());
            b
        }
        // A 0-wide, 1-tall, 1 strip (empty) BlackIsZero Gray8 image.
        let entries = vec![
            Entry {
                tag: 256,
                ty: 3,
                count: 1,
                value: short(0),
            }, // ImageWidth = 0
            Entry {
                tag: 257,
                ty: 3,
                count: 1,
                value: short(1),
            }, // ImageLength = 1
            Entry {
                tag: 258,
                ty: 3,
                count: 1,
                value: short(8),
            }, // BitsPerSample
            Entry {
                tag: 259,
                ty: 3,
                count: 1,
                value: short(1),
            }, // Compression: none
            Entry {
                tag: 262,
                ty: 3,
                count: 1,
                value: short(1),
            }, // Photometric
            Entry {
                tag: 273,
                ty: 4,
                count: 1,
                value: [0; 4],
            }, // StripOffsets (empty)
            Entry {
                tag: 277,
                ty: 3,
                count: 1,
                value: short(1),
            }, // SamplesPerPixel
            Entry {
                tag: 278,
                ty: 3,
                count: 1,
                value: short(1),
            }, // RowsPerStrip
            Entry {
                tag: 279,
                ty: 4,
                count: 1,
                value: [0; 4],
            }, // StripByteCounts = 0
        ];
        let ifd_start = 8u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for e in &entries {
            bytes.extend_from_slice(&e.tag.to_le_bytes());
            bytes.extend_from_slice(&e.ty.to_le_bytes());
            bytes.extend_from_slice(&e.count.to_le_bytes());
            bytes.extend_from_slice(&e.value);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes()); // next IFD: none
        std::fs::write(&path, bytes).unwrap();

        let err = TiffReader::open(&path).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("zero dimension")
                || msg.contains("0x1")
                || msg.contains("nothing to read"),
            "expected error to name the zero dimension, got: {err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_bands_reassembles_gray32float() {
        let dir = tmpdir("gray32f");
        let path = dir.join("in.tif");
        let mut enc = TiffEncoder::new(std::fs::File::create(&path).unwrap()).unwrap();
        let data: Vec<f32> = (0..16).map(|i| i as f32 * 0.5).collect();
        enc.write_image::<colortype::Gray32Float>(4, 4, &data)
            .unwrap();

        let mut r = TiffReader::open(&path).unwrap();
        assert_eq!(r.info().sample_format, SampleFormat::F32);
        let mut out = vec![0f32; 16];
        r.read_bands(&mut |ch, start_row, rows, bytes| {
            assert_eq!(ch, 0);
            for rr in 0..rows as u64 {
                for x in 0..4u64 {
                    let b = ((rr * 4 + x) * 4) as usize;
                    let v = f32::from_ne_bytes(bytes[b..b + 4].try_into().unwrap());
                    out[((start_row + rr) * 4 + x) as usize] = v;
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(out, data);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Write a minimal, uncompressed, classic (32-bit offsets) little-endian
    /// TIFF with tiled Gray8 data. The `tiff` crate's encoder has no tile
    /// support (only `write_image`'s strip-based path), so this builds the
    /// file byte-for-byte per the TIFF 6.0 spec's IFD layout (tag, type,
    /// count, value/offset — 12 bytes per entry, sorted ascending by tag)
    /// to exercise the tiled `read_bands` path. `w`/`h` must be exact
    /// multiples of `tile_w`/`tile_h` (no edge-tile clipping in this
    /// fixture).
    fn write_minimal_tiled_gray8(
        path: &Path,
        w: u32,
        h: u32,
        tile_w: u32,
        tile_h: u32,
        pixels: &[u8],
    ) {
        assert_eq!(pixels.len(), (w * h) as usize);
        assert_eq!(w % tile_w, 0);
        assert_eq!(h % tile_h, 0);
        let tiles_across = w / tile_w;
        let tiles_down = h / tile_h;
        let tile_count = tiles_across * tiles_down;

        struct Entry {
            tag: u16,
            ty: u16,
            count: u32,
            value: [u8; 4],
        }
        fn short(v: u16) -> [u8; 4] {
            let mut b = [0u8; 4];
            b[0..2].copy_from_slice(&v.to_le_bytes());
            b
        }

        // Tags in ascending numeric order, as the TIFF spec requires.
        let mut entries = vec![
            Entry {
                tag: 256,
                ty: 3,
                count: 1,
                value: short(w as u16),
            }, // ImageWidth
            Entry {
                tag: 257,
                ty: 3,
                count: 1,
                value: short(h as u16),
            }, // ImageLength
            Entry {
                tag: 258,
                ty: 3,
                count: 1,
                value: short(8),
            }, // BitsPerSample
            Entry {
                tag: 259,
                ty: 3,
                count: 1,
                value: short(1),
            }, // Compression: none
            Entry {
                tag: 262,
                ty: 3,
                count: 1,
                value: short(1),
            }, // Photometric: BlackIsZero
            Entry {
                tag: 277,
                ty: 3,
                count: 1,
                value: short(1),
            }, // SamplesPerPixel
            Entry {
                tag: 322,
                ty: 3,
                count: 1,
                value: short(tile_w as u16),
            }, // TileWidth
            Entry {
                tag: 323,
                ty: 3,
                count: 1,
                value: short(tile_h as u16),
            }, // TileLength
            Entry {
                tag: 324,
                ty: 4,
                count: tile_count,
                value: [0; 4],
            }, // TileOffsets (patched below)
            Entry {
                tag: 325,
                ty: 4,
                count: tile_count,
                value: [0; 4],
            }, // TileByteCounts (patched below)
            Entry {
                tag: 339,
                ty: 3,
                count: 1,
                value: short(1),
            }, // SampleFormat: uint
        ];

        let ifd_start = 8u32;
        let ifd_bytes = 2 + entries.len() as u32 * 12 + 4;
        let tile_offsets_off = ifd_start + ifd_bytes;
        let tile_bytes_off = tile_offsets_off + tile_count * 4;
        let tile_data_start = tile_bytes_off + tile_count * 4;

        let tile_len = (tile_w * tile_h) as usize;
        let mut tile_offsets = Vec::with_capacity(tile_count as usize);
        let mut tile_data = Vec::with_capacity(tile_count as usize * tile_len);
        for tr in 0..tiles_down {
            for tc in 0..tiles_across {
                tile_offsets.push(tile_data_start + tile_data.len() as u32);
                for row in 0..tile_h {
                    let y = tr * tile_h + row;
                    for col in 0..tile_w {
                        let x = tc * tile_w + col;
                        tile_data.push(pixels[(y * w + x) as usize]);
                    }
                }
            }
        }

        entries[8]
            .value
            .copy_from_slice(&tile_offsets_off.to_le_bytes());
        entries[9]
            .value
            .copy_from_slice(&tile_bytes_off.to_le_bytes());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        assert_eq!(bytes.len() as u32, ifd_start);

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for e in &entries {
            bytes.extend_from_slice(&e.tag.to_le_bytes());
            bytes.extend_from_slice(&e.ty.to_le_bytes());
            bytes.extend_from_slice(&e.count.to_le_bytes());
            bytes.extend_from_slice(&e.value);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset: none
        assert_eq!(bytes.len() as u32, tile_offsets_off);

        for off in &tile_offsets {
            bytes.extend_from_slice(&off.to_le_bytes());
        }
        assert_eq!(bytes.len() as u32, tile_bytes_off);
        for _ in 0..tile_count {
            bytes.extend_from_slice(&(tile_len as u32).to_le_bytes());
        }
        assert_eq!(bytes.len() as u32, tile_data_start);
        bytes.extend_from_slice(&tile_data);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn read_bands_reassembles_tiled_gray8() {
        let dir = tmpdir("tiled");
        let path = dir.join("in.tif");
        let pixels: Vec<u8> = (0..16u8).collect();
        write_minimal_tiled_gray8(&path, 4, 4, 2, 2, &pixels);

        let mut r = TiffReader::open(&path).unwrap();
        assert_eq!((r.info().width, r.info().height), (4, 4));
        assert_eq!(r.info().color, ColorClass::Gray);
        assert_eq!(r.info().sample_format, SampleFormat::U8);

        let mut out = vec![0u8; 16];
        r.read_bands(&mut |ch, start_row, rows, bytes| {
            assert_eq!(ch, 0);
            assert_eq!(bytes.len(), rows as usize * 4);
            for rr in 0..rows as u64 {
                for x in 0..4u64 {
                    out[((start_row + rr) * 4 + x) as usize] = bytes[(rr * 4 + x) as usize];
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(out, pixels);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
