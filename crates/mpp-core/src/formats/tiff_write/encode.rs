//! The strip/tile encode loop and the tag set it emits.
//!
//! The output sample format comes from [`TiffWriteOptions::bit_depth`], not
//! from the source's own sample type: the converter's sources are unit-range
//! `f32` (XISF planes) whatever depth the caller asked for. Sources reach the
//! encode loop through the crate-internal `RowSource` trait, which adapts a
//! [`PlanarRows`] of one type to the writer's output type `O`.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use tiff::encoder::{TiffEncoder, TiffKind, TiffKindBig, TiffKindStandard};
use tiff::tags::Tag;

use super::options::{AlphaMode, Container, Layout, TiffWriteOptions};
use super::sample::Sample;
use crate::formats::PlanarRows;
use crate::formats::tiff::BitDepth;
use crate::{Error, Result};

/// Target uncompressed bytes per strip; rows per strip derives from this.
const TARGET_BLOCK_BYTES: u64 = 4 << 20;

/// A source of image rows, converted to the writer's output sample type.
///
/// Two adapters implement this: [`UnitF32Source`] for the unit-range `f32`
/// sources the converter produces (XISF planes, DZI input), which converts
/// each sample to `O`; and [`NativeSource`] for a source already in the
/// output's own type, which copies verbatim. Keeping them separate structs
/// rather than blanket impls avoids a coherence conflict when `O` happens to
/// be `f32`.
pub(crate) trait RowSource<O: Sample> {
    /// Width in pixels.
    fn width(&self) -> u64;
    /// Height in pixels.
    fn height(&self) -> u64;
    /// Channel count.
    fn channels(&self) -> u64;
    /// Append row `y` of channel `c`, converted to `O`, to `out`.
    fn row_into(&self, c: u64, y: u64, out: &mut Vec<O>);
}

/// Adapter for a unit-range `f32` source; converts through
/// [`Sample::from_unit_f32`]. For `O = f32` that conversion is the identity,
/// so the common passthrough case costs nothing beyond the copy the
/// interleave step would make anyway.
struct UnitF32Source<'a, I: ?Sized>(&'a I);

/// Adapter for a source already in the output sample type; copies verbatim,
/// with no conversion and no loss.
struct NativeSource<'a, I: ?Sized>(&'a I);

impl<O: Sample, I: PlanarRows<f32> + ?Sized> RowSource<O> for UnitF32Source<'_, I> {
    fn width(&self) -> u64 {
        self.0.width()
    }
    fn height(&self) -> u64 {
        self.0.height()
    }
    fn channels(&self) -> u64 {
        self.0.channels()
    }
    fn row_into(&self, c: u64, y: u64, out: &mut Vec<O>) {
        out.extend(self.0.row(c, y).iter().map(|&v| O::from_unit_f32(v)));
    }
}

impl<O: Sample, I: PlanarRows<O> + ?Sized> RowSource<O> for NativeSource<'_, I> {
    fn width(&self) -> u64 {
        self.0.width()
    }
    fn height(&self) -> u64 {
        self.0.height()
    }
    fn channels(&self) -> u64 {
        self.0.channels()
    }
    fn row_into(&self, c: u64, y: u64, out: &mut Vec<O>) {
        out.extend_from_slice(self.0.row(c, y));
    }
}

/// Write `img` to `path` as a TIFF or BigTIFF per `opts`, streaming block by
/// block in constant memory. Samples arrive in the unit-range `f32` domain
/// and are converted to the output sample format [`TiffWriteOptions::bit_depth`]
/// selects.
///
/// Options are validated first, so an impossible request fails before the
/// output file is created. Grayscale, gray+alpha, RGB and RGBA are
/// supported; planar source planes are interleaved to TIFF's chunky order.
pub fn write_tiff(img: &impl PlanarRows<f32>, path: &Path, opts: &TiffWriteOptions) -> Result<()> {
    write_tiff_with_target(img, path, opts, TARGET_BLOCK_BYTES)
}

/// [`write_tiff`], parameterized on the target block size — split out so
/// tests can force multi-block output without a multi-gigabyte image.
pub(crate) fn write_tiff_with_target(
    img: &impl PlanarRows<f32>,
    path: &Path,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
) -> Result<()> {
    let container = opts.validate(img.width(), img.height(), img.channels())?;
    let src = UnitF32Source(img);
    let mut buf = create(path)?;
    // One arm per output sample format: `O` is chosen here and nowhere else.
    let result = match opts.bit_depth {
        BitDepth::U8 => {
            encode_container::<u8, _, _>(container, &src, &mut buf, opts, target_block_bytes, path)
        }
        BitDepth::U16 => {
            encode_container::<u16, _, _>(container, &src, &mut buf, opts, target_block_bytes, path)
        }
        BitDepth::U32 => {
            encode_container::<u32, _, _>(container, &src, &mut buf, opts, target_block_bytes, path)
        }
        BitDepth::F32 => {
            encode_container::<f32, _, _>(container, &src, &mut buf, opts, target_block_bytes, path)
        }
        BitDepth::F64 => {
            encode_container::<f64, _, _>(container, &src, &mut buf, opts, target_block_bytes, path)
        }
    };
    finish_file(result, buf, path)
}

/// [`write_tiff`] for a source already in the output's own sample type: the
/// samples are copied verbatim, with no conversion and no loss.
///
/// Errors unless [`TiffWriteOptions::bit_depth`] equals `T`'s own depth — a
/// mismatch here means the caller intended a conversion, which this entry
/// point does not do.
pub fn write_tiff_native<T: Sample>(
    img: &impl PlanarRows<T>,
    path: &Path,
    opts: &TiffWriteOptions,
) -> Result<()> {
    if opts.bit_depth != T::DEPTH {
        return Err(Error::invalid(format!(
            "write_tiff_native writes the source's own samples, but the source is \
             {:?} and the requested output is {:?}; use write_tiff to convert",
            T::DEPTH,
            opts.bit_depth
        )));
    }
    let container = opts.validate(img.width(), img.height(), img.channels())?;
    let src = NativeSource(img);
    let mut buf = create(path)?;
    let result =
        encode_container::<T, _, _>(container, &src, &mut buf, opts, TARGET_BLOCK_BYTES, path);
    finish_file(result, buf, path)
}

/// Create the output file, wrapped for buffered writing.
fn create(path: &Path) -> Result<BufWriter<File>> {
    let file = File::create(path).map_err(|e| Error::io(path, e))?;
    Ok(BufWriter::new(file))
}

/// Flush a successful encode, or delete the part-written file after a failed
/// one — a truncated TIFF where a reader might find it is worse than none.
fn finish_file(result: Result<()>, mut buf: BufWriter<File>, path: &Path) -> Result<()> {
    if result.is_err() {
        drop(buf);
        let _ = std::fs::remove_file(path);
        return result;
    }
    buf.flush().map_err(|e| Error::io(path, e))?;
    Ok(())
}

/// Resolve the container kind `K` and run [`encode`].
///
/// `validate` resolves [`Container::Auto`] to `Classic` or `Big` and never
/// returns `Auto`, so the fallback arm is BigTIFF.
fn encode_container<O, W, S>(
    container: Container,
    src: &S,
    writer: &mut W,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
    path: &Path,
) -> Result<()>
where
    O: Sample,
    W: Write + Seek,
    S: RowSource<O> + ?Sized,
{
    match container {
        Container::Classic => {
            encode::<TiffKindStandard, O, _, _>(src, writer, opts, target_block_bytes, path)
        }
        _ => encode::<TiffKindBig, O, _, _>(src, writer, opts, target_block_bytes, path),
    }
}

/// Photometric interpretation for a colour-channel count: 1 = BlackIsZero,
/// 2 = RGB (TIFF 6.0 §8).
fn photometric(colour_channels: u64) -> u16 {
    if colour_channels == 1 { 1 } else { 2 }
}

/// Stream the pixel data as strips or tiles and write the IFD describing it.
fn encode<K, O, W, S>(
    src: &S,
    writer: &mut W,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
    path: &Path,
) -> Result<()>
where
    K: TiffKind,
    O: Sample,
    W: Write + Seek,
    S: RowSource<O> + ?Sized,
{
    let terr = |e: tiff::TiffError| Error::format(path, format!("TIFF encode: {e}"));
    let (w, h, c) = (src.width(), src.height(), src.channels());
    let w32 =
        u32::try_from(w).map_err(|_| Error::format(path, "image width exceeds TIFF limits"))?;
    let h32 =
        u32::try_from(h).map_err(|_| Error::format(path, "image height exceeds TIFF limits"))?;
    let has_alpha = opts.alpha != AlphaMode::None;
    let colour_channels = if has_alpha { c - 1 } else { c };

    let mut enc: TiffEncoder<&mut W, K> = TiffEncoder::new_generic(writer).map_err(terr)?;
    let mut dir = enc.image_directory().map_err(terr)?;

    let mut offsets: Vec<K::OffsetType> = Vec::new();
    let mut byte_counts: Vec<K::OffsetType> = Vec::new();
    let mut raw = Vec::<u8>::new();
    // One reusable row buffer per channel: interleaving needs every
    // channel's row `y` at once, and this way the gather allocates once for
    // the whole image rather than once per row.
    let mut rows: Vec<Vec<O>> = (0..c).map(|_| Vec::with_capacity(w as usize)).collect();

    // -- pixel data ---------------------------------------------------------
    let rows_per_strip = match opts.layout {
        Layout::Strips => {
            let row_bytes = w * c * u64::from(O::BITS_PER_SAMPLE) / 8;
            let n_rows = (target_block_bytes / row_bytes.max(1)).clamp(1, h);
            for start in (0..h).step_by(n_rows as usize) {
                let n = n_rows.min(h - start);
                gather_strip(src, start, n, &mut rows, &mut raw);
                write_block::<K, _>(&mut dir, &raw, &mut offsets, &mut byte_counts)
                    .map_err(terr)?;
            }
            Some(u32::try_from(n_rows).unwrap_or(u32::MAX))
        }
        Layout::Tiles { size } => {
            let size64 = u64::from(size);
            for ty in (0..h).step_by(size as usize) {
                for tx in (0..w).step_by(size as usize) {
                    gather_tile(src, tx, ty, size64, &mut rows, &mut raw);
                    write_block::<K, _>(&mut dir, &raw, &mut offsets, &mut byte_counts)
                        .map_err(terr)?;
                }
            }
            None
        }
    };

    // -- tags ---------------------------------------------------------------
    dir.write_tag(Tag::ImageWidth, w32).map_err(terr)?;
    dir.write_tag(Tag::ImageLength, h32).map_err(terr)?;
    let bits: Vec<u16> = vec![O::BITS_PER_SAMPLE; c as usize];
    dir.write_tag(Tag::BitsPerSample, &bits[..]).map_err(terr)?;
    let formats: Vec<u16> = vec![O::TIFF_SAMPLE_FORMAT; c as usize];
    dir.write_tag(Tag::SampleFormat, &formats[..])
        .map_err(terr)?;
    dir.write_tag(Tag::SamplesPerPixel, c as u16)
        .map_err(terr)?;
    dir.write_tag(Tag::PhotometricInterpretation, photometric(colour_channels))
        .map_err(terr)?;
    dir.write_tag(Tag::PlanarConfiguration, 1u16)
        .map_err(terr)?; // chunky
    dir.write_tag(Tag::Compression, 1u16).map_err(terr)?; // none, for now

    match (opts.layout, rows_per_strip) {
        (Layout::Strips, Some(n_rows)) => {
            dir.write_tag(Tag::RowsPerStrip, n_rows).map_err(terr)?;
            dir.write_tag(Tag::StripOffsets, K::convert_slice(&offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::StripByteCounts, K::convert_slice(&byte_counts))
                .map_err(terr)?;
        }
        (Layout::Tiles { size }, _) => {
            dir.write_tag(Tag::TileWidth, size).map_err(terr)?;
            dir.write_tag(Tag::TileLength, size).map_err(terr)?;
            dir.write_tag(Tag::TileOffsets, K::convert_slice(&offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::TileByteCounts, K::convert_slice(&byte_counts))
                .map_err(terr)?;
        }
        _ => unreachable!("rows_per_strip is Some exactly for Layout::Strips"),
    }

    dir.finish().map_err(terr)?;
    Ok(())
}

/// Append one block of pixel data to the file, recording its offset and
/// length for the strip/tile offset tags.
fn write_block<K, W>(
    dir: &mut tiff::encoder::DirectoryEncoder<'_, W, K>,
    bytes: &[u8],
    offsets: &mut Vec<K::OffsetType>,
    byte_counts: &mut Vec<K::OffsetType>,
) -> tiff::TiffResult<()>
where
    K: TiffKind,
    W: Write + Seek,
{
    let offset = dir.write_data(bytes)?;
    offsets.push(K::convert_offset(offset)?);
    // Byte counts are the same width as offsets in both container kinds;
    // `OffsetType: TryFrom<usize>` is the crate's own idiom for this
    // (see its `ImageEncoder::write_strip`).
    byte_counts.push(K::OffsetType::try_from(bytes.len())?);
    Ok(())
}

/// Fill `rows` with row `y` of every channel, converted to `O`.
fn fetch_rows<O: Sample, S: RowSource<O> + ?Sized>(src: &S, y: u64, rows: &mut [Vec<O>]) {
    for (c, buf) in rows.iter_mut().enumerate() {
        buf.clear();
        src.row_into(c as u64, y, buf);
    }
}

/// Interleave rows `[start, start + n)` of every channel into `out`.
fn gather_strip<O: Sample, S: RowSource<O> + ?Sized>(
    src: &S,
    start: u64,
    n: u64,
    rows: &mut [Vec<O>],
    out: &mut Vec<u8>,
) {
    let w = src.width() as usize;
    out.clear();
    for y in start..start + n {
        fetch_rows(src, y, rows);
        for x in 0..w {
            for buf in rows.iter() {
                buf[x].push_ne(out);
            }
        }
    }
}

/// Interleave one `size x size` tile whose top-left pixel is `(tx, ty)` into
/// `out`, zero-padding the parts that fall outside the image — TIFF 6.0 §15
/// requires every tile to be full size, including the edge ones.
fn gather_tile<O: Sample, S: RowSource<O> + ?Sized>(
    src: &S,
    tx: u64,
    ty: u64,
    size: u64,
    rows: &mut [Vec<O>],
    out: &mut Vec<u8>,
) {
    let (w, h, c) = (src.width(), src.height(), src.channels());
    let zero = O::from_unit_f32(0.0);
    out.clear();
    for y in ty..ty + size {
        if y < h {
            fetch_rows(src, y, rows);
            for x in tx..tx + size {
                for buf in rows.iter() {
                    if x < w { buf[x as usize] } else { zero }.push_ne(out);
                }
            }
        } else {
            for _ in 0..size * c {
                zero.push_ne(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tiff::BitDepth;
    use std::path::PathBuf;
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;

    /// An in-memory planar image whose sample `(c, y, x)` is a distinct,
    /// predictable value — so a transposed, mis-strided or mis-interleaved
    /// write cannot accidentally produce the expected pixels.
    struct Synth {
        w: u64,
        h: u64,
        c: u64,
        planes: Vec<Vec<f32>>,
    }

    impl Synth {
        fn new(w: u64, h: u64, c: u64) -> Self {
            let total = (w * h * c) as f32;
            let planes = (0..c)
                .map(|ch| {
                    (0..w * h)
                        .map(|i| (ch * w * h + i) as f32 / total)
                        .collect()
                })
                .collect();
            Self { w, h, c, planes }
        }
    }

    impl crate::formats::PlanarRows<f32> for Synth {
        fn width(&self) -> u64 {
            self.w
        }
        fn height(&self) -> u64 {
            self.h
        }
        fn channels(&self) -> u64 {
            self.c
        }
        fn row(&self, c: u64, y: u64) -> &[f32] {
            let start = (y * self.w) as usize;
            &self.planes[c as usize][start..start + self.w as usize]
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-tw-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_gray_f32_strips_readable_by_the_decoder() {
        let dir = tmpdir("gray-f32");
        let out = dir.join("out.tif");
        let img = Synth::new(5, 4, 1);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (5, 4));
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 20);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 20.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rgb_planes_interleave_to_chunky_order() {
        let dir = tmpdir("rgb");
        let out = dir.join("out.tif");
        let img = Synth::new(3, 2, 3);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 18);
                // Pixel 0 gathers sample 0 of each plane: 0/18, 6/18, 12/18.
                assert!((data[0] - 0.0 / 18.0).abs() < 1e-7);
                assert!((data[1] - 6.0 / 18.0).abs() < 1e-7);
                assert!((data[2] - 12.0 / 18.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_strip_output_covers_a_partial_final_strip() {
        // 3x7 gray f32: row_bytes = 12; a 64-byte target gives 5 rows per
        // strip, so a 7-row image is one full strip plus a 2-row remainder.
        let dir = tmpdir("strips");
        let out = dir.join("out.tif");
        let img = Synth::new(3, 7, 1);
        write_tiff_with_target(&img, &out, &TiffWriteOptions::default(), 64).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        let offsets = dec.get_tag_u64_vec(Tag::StripOffsets).unwrap();
        let counts = dec.get_tag_u64_vec(Tag::StripByteCounts).unwrap();
        assert_eq!(offsets.len(), 2, "expected two strips");
        assert_eq!(counts, vec![5 * 3 * 4, 2 * 3 * 4]);
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 21);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 21.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u16_and_u8_scale_the_unit_range() {
        let dir = tmpdir("ints");
        let img = Synth::new(4, 4, 1);

        let out16 = dir.join("o16.tif");
        write_tiff(
            &img,
            &out16,
            &TiffWriteOptions {
                bit_depth: BitDepth::U16,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out16).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(d) => {
                assert_eq!(d[0], 0);
                assert_eq!(d[15], 61439); // round(15/16 * 65535)
            }
            other => panic!("expected U16, got {other:?}"),
        }

        let out8 = dir.join("o8.tif");
        write_tiff(
            &img,
            &out8,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out8).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U8(d) => {
                assert_eq!(d[0], 0);
                assert_eq!(d[8], 128);
                assert_eq!(d[15], 239);
            }
            other => panic!("expected U8, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn core_tags_are_written_with_spec_values() {
        let dir = tmpdir("tags");
        let out = dir.join("out.tif");
        let img = Synth::new(8, 8, 3);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(
            dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(),
            vec![32, 32, 32]
        );
        assert_eq!(
            dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(),
            vec![3, 3, 3]
        );
        assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 3);
        assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 2); // RGB
        assert_eq!(dec.get_tag_u64(Tag::PlanarConfiguration).unwrap(), 1); // chunky
        assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), 1); // none
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_an_invalid_option_combination_before_creating_the_file() {
        let dir = tmpdir("reject");
        let out = dir.join("never.tif");
        let img = Synth::new(4, 4, 2); // 2 channels with no alpha association
        assert!(write_tiff(&img, &out, &TiffWriteOptions::default()).is_err());
        assert!(
            !out.exists(),
            "a rejected write must not leave a file behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A planar `u16` source, for the verbatim [`write_tiff_native`] path.
    /// Values deliberately exceed anything the unit-range conversion could
    /// produce from the same numbers, so a stray conversion would show up.
    struct NativeSynth {
        w: u64,
        h: u64,
        plane: Vec<u16>,
    }

    impl crate::formats::PlanarRows<u16> for NativeSynth {
        fn width(&self) -> u64 {
            self.w
        }
        fn height(&self) -> u64 {
            self.h
        }
        fn channels(&self) -> u64 {
            1
        }
        fn row(&self, _c: u64, y: u64) -> &[u16] {
            let start = (y * self.w) as usize;
            &self.plane[start..start + self.w as usize]
        }
    }

    #[test]
    fn native_source_samples_are_copied_verbatim() {
        let dir = tmpdir("native");
        let out = dir.join("out.tif");
        let img = NativeSynth {
            w: 4,
            h: 2,
            plane: (0..8u16).map(|i| i * 9000 + 3).collect(),
        };
        write_tiff_native(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::U16,
                ..Default::default()
            },
        )
        .unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(d) => {
                assert_eq!(d, (0..8u16).map(|i| i * 9000 + 3).collect::<Vec<_>>());
            }
            other => panic!("expected U16, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn native_write_rejects_a_depth_that_would_need_conversion() {
        let dir = tmpdir("native-mismatch");
        let out = dir.join("never.tif");
        let img = NativeSynth {
            w: 4,
            h: 2,
            plane: vec![0; 8],
        };
        let err = write_tiff_native(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("write_tiff"),
            "the error should point at the converting entry point, got: {err}"
        );
        assert!(
            !out.exists(),
            "a rejected write must not leave a file behind"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
