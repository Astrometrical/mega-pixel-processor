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

use tiff::encoder::compression::{
    CompressionAlgorithm, Compressor, Deflate, DeflateLevel, Lzw, Packbits, Uncompressed,
};
use tiff::encoder::{TiffEncoder, TiffKind, TiffKindBig, TiffKindStandard};
use tiff::tags::Tag;

use super::options::{AlphaMode, Compression, Container, Layout, ResolutionUnit, TiffWriteOptions};
use super::sample::Sample;
use crate::formats::PlanarRows;
use crate::formats::tiff::BitDepth;
use crate::pipeline::{self, Pipeline};
use crate::{Error, Result};

/// Target uncompressed bytes per strip; rows per strip derives from this.
const TARGET_BLOCK_BYTES: u64 = 4 << 20;

/// Most worker threads the encoder will use. Compression on this many cores
/// already outruns any disk the file could be going to, and each thread
/// keeps two blocks in flight, so more would only cost memory.
const MAX_ENCODE_THREADS: usize = 16;

/// Knobs the encoder is parameterised on internally. Production callers get
/// [`EncodeTuning::default`]; tests use small blocks to force multi-block
/// output from a small image, and pin thread-count independence.
#[derive(Debug, Clone, Copy)]
struct EncodeTuning {
    /// Target uncompressed bytes per strip.
    target_block_bytes: u64,
    /// Gather+compress worker threads; the writer is the calling thread.
    threads: usize,
}

impl Default for EncodeTuning {
    fn default() -> Self {
        Self {
            target_block_bytes: TARGET_BLOCK_BYTES,
            threads: pipeline::worker_count(usize::MAX, MAX_ENCODE_THREADS),
        }
    }
}

/// Encode-progress observer: called as `(rows_done, rows_total)` each time a
/// whole block row (a strip, or a full row of tiles) has reached the file,
/// where `rows_total` is the image height. The final call always reports
/// `rows_done == rows_total`. The plain [`write_tiff`]/[`write_tiff_native`]
/// entry points report nothing.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, u64);

/// A [`ProgressFn`] that ignores every report.
fn no_progress(_rows_done: u64, _rows_total: u64) {}

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
/// block in memory bounded by the thread count, not the image size: blocks
/// are gathered and compressed on worker threads (every core up to 16) and
/// written in file order, at most two per worker in flight. Samples arrive
/// in the unit-range `f32` domain and are converted to the output sample
/// format [`TiffWriteOptions::bit_depth`] selects.
///
/// Options are validated first, so an impossible request fails before the
/// output file is created. Grayscale, gray+alpha, RGB and RGBA are
/// supported; planar source planes are interleaved to TIFF's chunky order.
pub fn write_tiff(
    img: &(impl PlanarRows<f32> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
) -> Result<()> {
    write_tiff_tuned(img, path, opts, EncodeTuning::default(), &mut no_progress)
}

/// [`write_tiff`], reporting progress to `progress` after every block row
/// (see [`ProgressFn`]).
pub fn write_tiff_with_progress(
    img: &(impl PlanarRows<f32> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
    progress: ProgressFn<'_>,
) -> Result<()> {
    write_tiff_tuned(img, path, opts, EncodeTuning::default(), progress)
}

/// [`write_tiff`], parameterized on the target block size — split out so
/// tests can force multi-block output without a multi-gigabyte image.
#[cfg(test)]
pub(crate) fn write_tiff_with_target(
    img: &(impl PlanarRows<f32> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
) -> Result<()> {
    write_tiff_with_target_and_progress(img, path, opts, target_block_bytes, &mut no_progress)
}

/// [`write_tiff_with_target`] with a progress observer.
#[cfg(test)]
fn write_tiff_with_target_and_progress(
    img: &(impl PlanarRows<f32> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let tuning = EncodeTuning {
        target_block_bytes,
        ..EncodeTuning::default()
    };
    write_tiff_tuned(img, path, opts, tuning, progress)
}

/// The one body behind every `write_tiff*` entry point above.
fn write_tiff_tuned(
    img: &(impl PlanarRows<f32> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
    tuning: EncodeTuning,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let container = resolve_container(
        opts,
        path,
        opts.validate(img.width(), img.height(), img.channels())?,
    );
    let src = UnitF32Source(img);
    let mut buf = create(path)?;
    // One arm per output sample format: `O` is chosen here and nowhere else.
    let result = match opts.bit_depth {
        BitDepth::U8 => {
            encode_container::<u8, _, _>(container, &src, &mut buf, opts, tuning, path, progress)
        }
        BitDepth::U16 => {
            encode_container::<u16, _, _>(container, &src, &mut buf, opts, tuning, path, progress)
        }
        BitDepth::U32 => {
            encode_container::<u32, _, _>(container, &src, &mut buf, opts, tuning, path, progress)
        }
        BitDepth::F32 => {
            encode_container::<f32, _, _>(container, &src, &mut buf, opts, tuning, path, progress)
        }
        BitDepth::F64 => {
            encode_container::<f64, _, _>(container, &src, &mut buf, opts, tuning, path, progress)
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
    img: &(impl PlanarRows<T> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
) -> Result<()> {
    write_tiff_native_with_progress(img, path, opts, &mut no_progress)
}

/// [`write_tiff_native`], reporting progress to `progress` after every
/// block row (see [`ProgressFn`]).
pub fn write_tiff_native_with_progress<T: Sample>(
    img: &(impl PlanarRows<T> + Sync),
    path: &Path,
    opts: &TiffWriteOptions,
    progress: ProgressFn<'_>,
) -> Result<()> {
    if opts.bit_depth != T::DEPTH {
        return Err(Error::invalid(format!(
            "write_tiff_native writes the source's own samples, but the source is \
             {:?} and the requested output is {:?}; use write_tiff to convert",
            T::DEPTH,
            opts.bit_depth
        )));
    }
    let container = resolve_container(
        opts,
        path,
        opts.validate(img.width(), img.height(), img.channels())?,
    );
    let src = NativeSource(img);
    let mut buf = create(path)?;
    let result = encode_container::<T, _, _>(
        container,
        &src,
        &mut buf,
        opts,
        EncodeTuning::default(),
        path,
        progress,
    );
    finish_file(result, buf, path)
}

/// True for the two extensions that name BigTIFF specifically.
///
/// `.tif`/`.tiff` say nothing about which container is inside and are
/// deliberately excluded: those are the ones `Container::Auto`'s size
/// heuristic is for.
fn declares_bigtiff(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("btf") || e.eq_ignore_ascii_case("tf8"))
}

/// Let a BigTIFF-declaring output extension override [`Container::Auto`]'s
/// size heuristic.
///
/// `.btf` and `.tf8` are container declarations, not just TIFF-family
/// extensions — the PixInsight module claims them as BigTIFF and
/// `integration/pixinsight/README.md` documents them that way — so writing
/// a classic TIFF into one because the payload happened to fit under 4 GB
/// would contradict the name the caller chose. Honouring the extension
/// beats the size heuristic here; a BigTIFF is legal at any size, whereas a
/// classic TIFF in a `.btf` is a file whose name lies about its contents.
///
/// An explicit [`Container::Classic`] is left alone rather than upgraded or
/// rejected: the caller named the container in so many words and
/// [`TiffWriteOptions::validate`] already accepted it, so overriding it
/// would be the writer second-guessing an explicit request. Only `Auto` —
/// the "you decide" answer — is open to the extension's opinion.
fn resolve_container(opts: &TiffWriteOptions, path: &Path, validated: Container) -> Container {
    if opts.container == Container::Auto
        && validated == Container::Classic
        && declares_bigtiff(path)
    {
        return Container::Big;
    }
    validated
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
    tuning: EncodeTuning,
    path: &Path,
    progress: ProgressFn<'_>,
) -> Result<()>
where
    O: Sample,
    W: Write + Seek,
    S: RowSource<O> + Sync + ?Sized,
{
    match container {
        Container::Classic => {
            encode::<TiffKindStandard, O, _, _>(src, writer, opts, tuning, path, progress)
        }
        _ => encode::<TiffKindBig, O, _, _>(src, writer, opts, tuning, path, progress),
    }
}

/// Build the block compressor and the `Compression` tag value for `opts`.
///
/// Each block gets a fresh encoder inside `Compressor::write_to`, which is
/// what makes strips and tiles independently decodable as TIFF requires.
///
/// The `tiff` crate's `DeflateLevel` has three variants, so the 1-9 level
/// the options carry buckets into fast, balanced and best.
fn compressor_for(opts: &TiffWriteOptions) -> (Compressor, u16) {
    match opts.compression {
        Compression::None => (Compressor::Uncompressed(Uncompressed), 1),
        Compression::Lzw => (Compressor::Lzw(Lzw), 5),
        Compression::Deflate => {
            let level = match opts.effective_deflate_level() {
                1..=3 => DeflateLevel::Fast,
                7..=9 => DeflateLevel::Best,
                _ => DeflateLevel::Balanced,
            };
            (Compressor::Deflate(Deflate::with_level(level)), 8)
        }
        Compression::Packbits => (Compressor::Packbits(Packbits), 32773),
    }
}

/// Apply TIFF's horizontal differencing predictor (tag 317 = 2) in place.
///
/// Each sample becomes the difference from the sample `channels` positions
/// earlier in the same row, so runs of similar values compress far better.
/// The difference is taken on whole *samples*, not bytes: for 16- and 32-bit
/// data a byte-wise difference would be a different (and wrong) transform.
///
/// Integer sample types only — [`TiffWriteOptions::validate`] rejects the
/// predictor for float formats, since differencing IEEE bit patterns is not
/// what tag 317 means, and [`Sample::wrapping_sub_sample`] is unreachable
/// for them.
fn predict_horizontal<O: Sample>(block: &mut [O], samples_per_row: usize, channels: usize) {
    if samples_per_row == 0 || channels >= samples_per_row {
        return;
    }
    for row in block.chunks_exact_mut(samples_per_row) {
        // Walk backwards so each subtraction sees its original predecessor.
        for i in (channels..samples_per_row).rev() {
            row[i] = row[i].wrapping_sub_sample(row[i - channels]);
        }
    }
}

/// One strip or tile to encode, in file order.
#[derive(Debug, Clone, Copy)]
struct BlockSpec {
    /// Kind and position.
    kind: BlockKind,
    /// Rows of the image complete once this block is written — set on every
    /// strip and on the last tile of each tile row, so progress is reported
    /// per block row in both layouts.
    rows_done_after: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum BlockKind {
    /// Rows `[start, start + n)`, full width.
    Strip { start: u64, n: u64 },
    /// The `size x size` tile whose top-left pixel is `(tx, ty)`.
    Tile { tx: u64, ty: u64, size: u64 },
}

/// Every block of the image in the order the file stores them: strips top
/// to bottom, or tiles row-major. Returns the specs and, for strips, the
/// `RowsPerStrip` tag value.
fn plan_blocks(
    opts: &TiffWriteOptions,
    w: u64,
    h: u64,
    row_bytes: u64,
    target_block_bytes: u64,
) -> (Vec<BlockSpec>, Option<u32>) {
    match opts.layout {
        Layout::Strips => {
            let n_rows = (target_block_bytes / row_bytes.max(1)).clamp(1, h);
            let specs = (0..h)
                .step_by(n_rows as usize)
                .map(|start| {
                    let n = n_rows.min(h - start);
                    BlockSpec {
                        kind: BlockKind::Strip { start, n },
                        rows_done_after: Some(start + n),
                    }
                })
                .collect();
            (specs, Some(u32::try_from(n_rows).unwrap_or(u32::MAX)))
        }
        Layout::Tiles { size } => {
            let size64 = u64::from(size);
            let last_tx = ((w - 1) / size64) * size64;
            let mut specs = Vec::new();
            for ty in (0..h).step_by(size as usize) {
                for tx in (0..w).step_by(size as usize) {
                    specs.push(BlockSpec {
                        kind: BlockKind::Tile {
                            tx,
                            ty,
                            size: size64,
                        },
                        rows_done_after: (tx == last_tx).then_some((ty + size64).min(h)),
                    });
                }
            }
            (specs, None)
        }
    }
}

/// Per-worker state for turning a [`BlockSpec`] into its on-disk bytes:
/// gather, predict, serialise, compress. Each worker owns one, so the
/// reusable buffers are allocated once per thread rather than once per
/// block, and each block gets a fresh compressor state inside
/// `Compressor::write_to`, which is what makes strips and tiles
/// independently decodable as TIFF requires.
struct BlockEncoder<O> {
    /// `None` when uncompressed: the raw bytes are the output, no copy.
    compressor: Option<Compressor>,
    /// Whether to apply the horizontal differencing predictor.
    predictor: bool,
    /// Interleaved samples in one row of a block: `width * channels` for a
    /// strip, `tile size * channels` for a tile — the predictor's row length.
    samples_per_row: usize,
    /// Channel count — the predictor's stride, in samples.
    channels: usize,
    /// Interleaved, typed samples of the block being built.
    block: Vec<O>,
    /// One row buffer per channel for the gather.
    rows: Vec<Vec<O>>,
}

impl<O: Sample> BlockEncoder<O> {
    fn new(opts: &TiffWriteOptions, w: u64, c: u64, samples_per_row: usize) -> Self {
        Self {
            compressor: match opts.compression {
                Compression::None => None,
                _ => Some(compressor_for(opts).0),
            },
            predictor: opts.predictor,
            samples_per_row,
            channels: c as usize,
            block: Vec::new(),
            rows: (0..c).map(|_| Vec::with_capacity(w as usize)).collect(),
        }
    }

    /// The bytes to store for `spec`: native-order samples, differenced if
    /// the predictor is on, compressed if compression is on.
    fn encode<S: RowSource<O> + ?Sized>(
        &mut self,
        src: &S,
        spec: &BlockSpec,
    ) -> tiff::TiffResult<Vec<u8>> {
        match spec.kind {
            BlockKind::Strip { start, n } => {
                gather_strip(src, start, n, &mut self.rows, &mut self.block)
            }
            BlockKind::Tile { tx, ty, size } => {
                gather_tile(src, tx, ty, size, &mut self.rows, &mut self.block)
            }
        }
        if self.predictor {
            predict_horizontal(&mut self.block, self.samples_per_row, self.channels);
        }
        // Samples are `Pod` and the file is native-order (see `sample.rs`),
        // so the block's bytes are its serialisation: one copy out of the
        // reusable buffer, no per-sample work.
        let raw: &[u8] = bytemuck::cast_slice(&self.block);
        match &mut self.compressor {
            None => Ok(raw.to_vec()),
            Some(compressor) => {
                let mut packed = Vec::with_capacity(raw.len() / 2);
                compressor.write_to(&mut packed, raw)?;
                Ok(packed)
            }
        }
    }
}

/// Where each block landed in the file: the `StripOffsets`/`StripByteCounts`
/// (or tile) lists the IFD needs, in block order.
struct BlockTables<K: TiffKind> {
    offsets: Vec<K::OffsetType>,
    byte_counts: Vec<K::OffsetType>,
}

/// Encode every block of `blocks` through `tuning.threads` workers and write
/// them to `dir` in order on the calling thread, returning where each block
/// landed. Progress is reported as each block row completes.
///
/// Compute and I/O overlap: while the writer is blocked in `write_data` on
/// block `i`, the workers are already encoding `i + 1 ..= i + window`. The
/// window is two blocks per worker — enough to keep every worker busy while
/// the writer catches up, small enough that a 16-thread encode holds under
/// 32 raw blocks (~128 MiB at the default block size) at any moment.
#[allow(clippy::too_many_arguments)]
fn encode_blocks<K, O, W, S>(
    src: &S,
    dir: &mut tiff::encoder::DirectoryEncoder<'_, W, K>,
    blocks: &[BlockSpec],
    opts: &TiffWriteOptions,
    tuning: EncodeTuning,
    samples_per_row: usize,
    path: &Path,
    progress: ProgressFn<'_>,
) -> Result<BlockTables<K>>
where
    K: TiffKind,
    O: Sample,
    W: Write + Seek,
    S: RowSource<O> + Sync + ?Sized,
{
    let (w, h, c) = (src.width(), src.height(), src.channels());
    let threads = tuning.threads.clamp(1, blocks.len().max(1));
    let pipeline = Pipeline::<Vec<u8>>::new(blocks.len(), threads * 2);
    let mut offsets = Vec::with_capacity(blocks.len());
    let mut byte_counts = Vec::with_capacity(blocks.len());

    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..threads {
            let pipeline = &pipeline;
            let mut enc = BlockEncoder::<O>::new(opts, w, c, samples_per_row);
            scope.spawn(move || {
                while let Some(idx) = pipeline.claim() {
                    match enc.encode(src, &blocks[idx]) {
                        Ok(bytes) => pipeline.complete(idx, bytes),
                        Err(e) => {
                            pipeline.fail(format!("TIFF encode: {e}"));
                            return;
                        }
                    }
                }
            });
        }

        for (idx, spec) in blocks.iter().enumerate() {
            let bytes = pipeline.take(idx).map_err(|msg| Error::format(path, msg))?;
            let written = (|| -> tiff::TiffResult<()> {
                let offset = dir.write_data(&bytes[..])?;
                offsets.push(K::convert_offset(offset)?);
                // Byte counts are the same width as offsets in both
                // container kinds; `OffsetType: TryFrom<usize>` is the
                // crate's own idiom for this (see its
                // `ImageEncoder::write_strip`).
                byte_counts.push(K::OffsetType::try_from(bytes.len())?);
                Ok(())
            })();
            if let Err(e) = written {
                // Wake any worker parked on the window so the scope can
                // join them, then surface the I/O error.
                pipeline.fail(format!("TIFF encode: {e}"));
                return Err(Error::format(path, format!("TIFF encode: {e}")));
            }
            pipeline.taken(idx);
            if let Some(rows) = spec.rows_done_after {
                progress(rows, h);
            }
        }
        Ok(())
    })?;

    Ok(BlockTables {
        offsets,
        byte_counts,
    })
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
    tuning: EncodeTuning,
    path: &Path,
    progress: ProgressFn<'_>,
) -> Result<()>
where
    K: TiffKind,
    O: Sample,
    W: Write + Seek,
    S: RowSource<O> + Sync + ?Sized,
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

    // Samples per row of a block: full image width for a strip, the tile
    // edge for a tile — the predictor's row length.
    let samples_per_block_row = match opts.layout {
        Layout::Strips => w * c,
        Layout::Tiles { size } => u64::from(size) * c,
    } as usize;
    let (_, compression_tag) = compressor_for(opts);

    // -- pixel data ---------------------------------------------------------
    let row_bytes = w * c * u64::from(O::BITS_PER_SAMPLE) / 8;
    let (blocks, rows_per_strip) = plan_blocks(opts, w, h, row_bytes, tuning.target_block_bytes);
    let tables = encode_blocks::<K, O, &mut W, S>(
        src,
        &mut dir,
        &blocks,
        opts,
        tuning,
        samples_per_block_row,
        path,
        progress,
    )?;

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
    // TIFF 6.0 §18: ExtraSamples describes every sample beyond the ones the
    // photometric interpretation accounts for. We write at most one, the
    // alpha channel, and its value says whether the colour channels are
    // premultiplied by it.
    match opts.alpha {
        AlphaMode::None => {}
        AlphaMode::Associated => dir
            .write_tag(Tag::ExtraSamples, &[1u16][..])
            .map_err(terr)?,
        AlphaMode::Unassociated => dir
            .write_tag(Tag::ExtraSamples, &[2u16][..])
            .map_err(terr)?,
    }
    dir.write_tag(Tag::PlanarConfiguration, 1u16)
        .map_err(terr)?; // chunky
    dir.write_tag(Tag::Compression, compression_tag)
        .map_err(terr)?;
    if opts.predictor {
        dir.write_tag(Tag::Predictor, 2u16).map_err(terr)?;
    }

    match (opts.layout, rows_per_strip) {
        (Layout::Strips, Some(n_rows)) => {
            dir.write_tag(Tag::RowsPerStrip, n_rows).map_err(terr)?;
            dir.write_tag(Tag::StripOffsets, K::convert_slice(&tables.offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::StripByteCounts, K::convert_slice(&tables.byte_counts))
                .map_err(terr)?;
        }
        (Layout::Tiles { size }, _) => {
            dir.write_tag(Tag::TileWidth, size).map_err(terr)?;
            dir.write_tag(Tag::TileLength, size).map_err(terr)?;
            dir.write_tag(Tag::TileOffsets, K::convert_slice(&tables.offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::TileByteCounts, K::convert_slice(&tables.byte_counts))
                .map_err(terr)?;
        }
        _ => unreachable!("rows_per_strip is Some exactly for Layout::Strips"),
    }

    // -- embedded metadata ----------------------------------------------
    // Written only when requested, so a default save keeps producing the
    // same minimal IFD it always did.
    if let Some(icc) = &opts.icc_profile {
        // Tag 34675 is UNDEFINED-typed, but `impl TiffValue for [u8]`
        // declares Type::BYTE, so `write_tag` would label it wrongly. Build
        // the entry with an explicit type instead and splice it in.
        let entry = dir
            .write_entry_bytes(tiff::tags::Type::UNDEFINED, &icc[..])
            .map_err(terr)?;
        let mut extra = tiff::Directory::empty();
        extra.extend([(Tag::IccProfile, entry)]);
        dir.extend_from(&extra);
    }
    for (tag, value) in [
        (Tag::ImageDescription, &opts.description),
        (Tag::Copyright, &opts.copyright),
        (Tag::Software, &opts.software),
    ] {
        if let Some(text) = value {
            // Pass the bare string: `impl TiffValue for str` appends the
            // terminating NUL itself and *rejects* a value that already
            // contains one, or any non-ASCII byte. `TiffWriteOptions::validate`
            // rejects such values before the file is created, so this
            // should never actually hit that rejection path.
            dir.write_tag(tag, text.as_str()).map_err(terr)?;
        }
    }
    if let Some(res) = &opts.resolution {
        // RATIONAL tags: a denominator of 1000 keeps the common values
        // (72, 300, 1200 dpi) exact.
        let rat = |v: f64| tiff::encoder::Rational {
            n: (v * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32,
            d: 1000,
        };
        dir.write_tag(Tag::XResolution, rat(res.x)).map_err(terr)?;
        dir.write_tag(Tag::YResolution, rat(res.y)).map_err(terr)?;
        let unit: u16 = match res.unit {
            ResolutionUnit::Inch => 2,
            ResolutionUnit::Centimeter => 3,
        };
        dir.write_tag(Tag::ResolutionUnit, unit).map_err(terr)?;
    }

    dir.finish().map_err(terr)?;
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
///
/// The block is gathered as typed samples rather than bytes so the
/// horizontal predictor can difference whole samples; serialisation to
/// native-order bytes is a slice cast in [`BlockEncoder::encode`], after it.
fn gather_strip<O: Sample, S: RowSource<O> + ?Sized>(
    src: &S,
    start: u64,
    n: u64,
    rows: &mut [Vec<O>],
    out: &mut Vec<O>,
) {
    let w = src.width() as usize;
    let c = rows.len();
    out.clear();
    out.resize(n as usize * w * c, O::from_unit_f32(0.0));
    for (i, y) in (start..start + n).enumerate() {
        fetch_rows(src, y, rows);
        interleave_row(rows, &mut out[i * w * c..(i + 1) * w * c], 0, w);
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
    out: &mut Vec<O>,
) {
    let (w, h) = (src.width(), src.height());
    let c = rows.len();
    let size = size as usize;
    let zero = O::from_unit_f32(0.0);
    out.clear();
    out.resize(size * size * c, zero);
    // Columns of the image this tile actually covers; the rest stays zero.
    let cols = (w.saturating_sub(tx) as usize).min(size);
    for (i, y) in (ty..ty + size as u64).enumerate() {
        if y >= h {
            break; // rows below the image are already zero
        }
        fetch_rows(src, y, rows);
        interleave_row(
            rows,
            &mut out[i * size * c..(i + 1) * size * c],
            tx as usize,
            cols,
        );
    }
}

/// Interleave `cols` samples starting at column `x0` of every channel's
/// row buffer into `out`, pixel-major (`out[x * c + ch]`). One channel is a
/// straight copy; more are a strided scatter over `chunks_exact_mut`, which
/// keeps the inner loop free of bounds checks.
fn interleave_row<O: Sample>(rows: &[Vec<O>], out: &mut [O], x0: usize, cols: usize) {
    let c = rows.len();
    if c == 1 {
        out[..cols].copy_from_slice(&rows[0][x0..x0 + cols]);
        return;
    }
    for (ch, row) in rows.iter().enumerate() {
        for (px, v) in out[..cols * c].chunks_exact_mut(c).zip(&row[x0..x0 + cols]) {
            px[ch] = *v;
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

    /// BigTIFF magic on a little-endian host: `II` (byte order) then version
    /// 43 = `0x2B`, little-endian. Classic TIFF is `II` then version 42 =
    /// `0x2A`. A big-endian host writes `MM` and the version big-endian
    /// instead — the `tiff` crate takes the file's byte-order mark from
    /// `target_endian` — so every assertion on these bytes is gated on the
    /// host's endianness.
    #[cfg(target_endian = "little")]
    fn magic(path: &std::path::Path) -> [u8; 4] {
        let head = std::fs::read(path).unwrap();
        [head[0], head[1], head[2], head[3]]
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn container_choice_shows_in_the_file_header() {
        // Supersedes the old `an_explicit_big_container_writes_bigtiff_magic`:
        // that test covered explicit Big plus the Auto-picks-classic contrast;
        // this one folds in the explicit Classic case too, so there is one
        // coherent set of container tests instead of two overlapping ones.
        let dir = tmpdir("container");
        let img = Synth::new(8, 8, 1);

        let classic = dir.join("classic.tif");
        write_tiff(
            &img,
            &classic,
            &TiffWriteOptions {
                container: Container::Classic,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            magic(&classic),
            [0x49, 0x49, 0x2A, 0x00],
            "classic TIFF magic"
        );

        let big = dir.join("big.tif");
        write_tiff(
            &img,
            &big,
            &TiffWriteOptions {
                container: Container::Big,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(magic(&big), [0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");

        // Auto on a small image picks classic (spec finding 3: we own
        // .tif/.tiff now, so a small save must stay universally readable).
        let auto = dir.join("auto.tif");
        write_tiff(&img, &auto, &TiffWriteOptions::default()).unwrap();
        assert_eq!(
            magic(&auto),
            [0x49, 0x49, 0x2A, 0x00],
            "Auto picks classic when it fits"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn a_bigtiff_extension_overrides_auto_for_a_small_image() {
        let dir = tmpdir("btf-ext");
        let img = Synth::new(4, 4, 1);
        // `.btf` and `.tf8` name BigTIFF, so `Auto` must not pick classic
        // for them however small the payload is. `.TF8` proves the match is
        // case-insensitive.
        for name in ["out.btf", "out.tf8", "out.TF8"] {
            let out = dir.join(name);
            write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();
            assert_eq!(
                magic(&out),
                [0x49, 0x49, 0x2B, 0x00],
                "BigTIFF magic in {name}"
            );
        }
        // An explicit `Classic` is an explicit request, so the extension
        // does not override it.
        let explicit = dir.join("explicit.btf");
        write_tiff(
            &img,
            &explicit,
            &TiffWriteOptions {
                container: Container::Classic,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            magic(&explicit),
            [0x49, 0x49, 0x2A, 0x00],
            "an explicit Container::Classic stays classic"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_compression_round_trips_the_same_pixels() {
        use crate::formats::tiff_write::options::Compression;
        let dir = tmpdir("compress");
        let img = Synth::new(37, 23, 1); // deliberately not a block multiple
        for (name, compression, tag) in [
            ("none", Compression::None, 1u64),
            ("lzw", Compression::Lzw, 5),
            ("deflate", Compression::Deflate, 8),
            ("packbits", Compression::Packbits, 32773),
        ] {
            let out = dir.join(format!("{name}.tif"));
            let opts = TiffWriteOptions {
                compression,
                ..Default::default()
            };
            // A small target forces several blocks, so this also proves
            // each block is compressed independently.
            write_tiff_with_target(&img, &out, &opts, 512).unwrap();

            let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
            assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), tag, "{name}");
            match dec.read_image().unwrap() {
                DecodingResult::F32(data) => {
                    assert_eq!(data.len(), 37 * 23, "{name}");
                    for (i, v) in data.iter().enumerate() {
                        assert!(
                            (v - i as f32 / (37.0 * 23.0)).abs() < 1e-7,
                            "{name} sample {i}"
                        );
                    }
                }
                other => panic!("{name}: expected F32, got {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compression_actually_shrinks_compressible_data() {
        use crate::formats::tiff_write::options::Compression;
        // A constant image compresses hard; an incompressible one would
        // make this assertion meaningless.
        struct Flat(u64, u64);
        impl crate::formats::PlanarRows<u8> for Flat {
            fn width(&self) -> u64 {
                self.0
            }
            fn height(&self) -> u64 {
                self.1
            }
            fn channels(&self) -> u64 {
                1
            }
            fn row(&self, _c: u64, _y: u64) -> &[u8] {
                const ZEROS: [u8; 512] = [0; 512];
                &ZEROS[..512]
            }
        }
        let dir = tmpdir("shrink");
        let img = Flat(512, 512);
        let plain = dir.join("plain.tif");
        let packed = dir.join("packed.tif");
        write_tiff_native(
            &img,
            &plain,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                ..Default::default()
            },
        )
        .unwrap();
        write_tiff_native(
            &img,
            &packed,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                compression: Compression::Deflate,
                ..Default::default()
            },
        )
        .unwrap();
        let plain_len = std::fs::metadata(&plain).unwrap().len();
        let packed_len = std::fs::metadata(&packed).unwrap().len();
        assert!(
            packed_len * 4 < plain_len,
            "deflate should compress a constant image hard: {packed_len} vs {plain_len}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_predictor_differences_whole_samples_not_bytes() {
        // Two rows of 2-pixel RGB u16. A byte-wise difference would touch
        // the low and high halves of each sample independently and produce
        // entirely different numbers, so this pins the typed transform that
        // TIFF tag 317 actually specifies.
        let mut block: Vec<u16> = vec![1000, 2000, 3000, 1005, 2010, 3015, 10, 20, 30, 5, 15, 25];
        predict_horizontal(&mut block, 6, 3);
        assert_eq!(
            block,
            vec![1000, 2000, 3000, 5, 10, 15, 10, 20, 30, 65531, 65531, 65531],
            "each sample differences against the one `channels` earlier, wrapping"
        );
    }

    #[test]
    fn the_predictor_is_written_and_round_trips() {
        use crate::formats::tiff_write::options::Compression;
        let dir = tmpdir("predictor");
        let out = dir.join("out.tif");
        let img = Synth::new(16, 16, 3);
        let opts = TiffWriteOptions {
            bit_depth: BitDepth::U16,
            compression: Compression::Lzw,
            predictor: true,
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::Predictor).unwrap(), 2);
        match dec.read_image().unwrap() {
            DecodingResult::U16(data) => {
                assert_eq!(data.len(), 16 * 16 * 3);
                assert_eq!(data[0], 0);
                // Sample 767 of 768 -> round(767/768 * 65535).
                assert_eq!(data[767], 65450);
                // Every sample, not just the ends: a byte-wise predictor
                // decodes to plausible-looking garbage in the middle of a
                // row, so the endpoints alone would not catch it. Decoded
                // sample `j` is pixel `j / 3` of channel `j % 3`, and
                // `Synth` gives that `(ch * 256 + p) / 768`.
                for (j, &v) in data.iter().enumerate() {
                    let (p, ch) = (j / 3, j % 3);
                    // Through the same unit-range conversion the writer
                    // uses, so this asserts on the predictor rather than on
                    // f32 rounding, which `sample.rs` tests separately.
                    let want = u16::from_unit_f32((ch * 256 + p) as f32 / 768.0);
                    assert_eq!(v, want, "sample {j}");
                }
            }
            other => panic!("expected U16, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tiled_output_writes_tile_tags_and_round_trips() {
        use crate::formats::tiff_write::options::Layout;
        let dir = tmpdir("tiles");
        let out = dir.join("out.tif");
        // 300x200 with 128-pixel tiles: 3 tile columns x 2 tile rows, so
        // the right column and bottom row are both partial, including the
        // bottom-right corner tile which is partial in both axes.
        let img = Synth::new(300, 200, 1);
        let opts = TiffWriteOptions {
            layout: Layout::Tiles { size: 128 },
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::TileWidth).unwrap(), 128);
        assert_eq!(dec.get_tag_u64(Tag::TileLength).unwrap(), 128);
        assert_eq!(dec.get_tag_u64_vec(Tag::TileOffsets).unwrap().len(), 6);
        assert_eq!(dec.get_tag_u64_vec(Tag::TileByteCounts).unwrap().len(), 6);
        // Every tile is full size on disk, edge tiles included.
        for count in dec.get_tag_u64_vec(Tag::TileByteCounts).unwrap() {
            assert_eq!(count, 128 * 128 * 4);
        }
        assert!(
            dec.get_tag(Tag::StripOffsets).is_err(),
            "no strip tags in a tiled file"
        );

        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 300 * 200);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 60_000.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A planar source of an arbitrary sample type, so the wide formats can
    /// be exercised without going through the unit-range f32 conversion.
    struct Typed<T> {
        w: u64,
        h: u64,
        c: u64,
        planes: Vec<Vec<T>>,
    }

    impl<T: Copy> crate::formats::PlanarRows<T> for Typed<T> {
        fn width(&self) -> u64 {
            self.w
        }
        fn height(&self) -> u64 {
            self.h
        }
        fn channels(&self) -> u64 {
            self.c
        }
        fn row(&self, c: u64, y: u64) -> &[T] {
            let start = (y * self.w) as usize;
            &self.planes[c as usize][start..start + self.w as usize]
        }
    }

    #[test]
    fn rgba_writes_extra_samples_and_keeps_four_channels() {
        use crate::formats::tiff_write::options::AlphaMode;
        let dir = tmpdir("rgba");
        for (mode, expected) in [
            (AlphaMode::Unassociated, 2u64),
            (AlphaMode::Associated, 1u64),
        ] {
            let out = dir.join(format!("{expected}.tif"));
            let img = Synth::new(6, 5, 4);
            let opts = TiffWriteOptions {
                alpha: mode,
                ..Default::default()
            };
            write_tiff(&img, &out, &opts).unwrap();

            let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
            assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 4);
            assert_eq!(
                dec.get_tag_u64_vec(Tag::ExtraSamples).unwrap(),
                vec![expected]
            );
            assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 2);
            assert_eq!(
                dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(),
                vec![32, 32, 32, 32]
            );
            match dec.read_image().unwrap() {
                DecodingResult::F32(d) => assert_eq!(d.len(), 6 * 5 * 4),
                other => panic!("expected F32, got {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gray_plus_alpha_stays_black_is_zero() {
        use crate::formats::tiff_write::options::AlphaMode;
        let dir = tmpdir("graya");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 2);
        write_tiff(
            &img,
            &out,
            &TiffWriteOptions {
                alpha: AlphaMode::Unassociated,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 1);
        assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 2);
        assert_eq!(dec.get_tag_u64_vec(Tag::ExtraSamples).unwrap(), vec![2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u32_samples_round_trip_exactly() {
        // The whole point of protocol v3: a u32 source must survive
        // unchanged, including values f32 cannot represent.
        let dir = tmpdir("u32");
        let out = dir.join("out.tif");
        let values: Vec<u32> = vec![0, 1, 16_777_217, 4_000_000_001, u32::MAX, 12345];
        let img = Typed {
            w: 6,
            h: 1,
            c: 1,
            planes: vec![values.clone()],
        };
        write_tiff_native(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::U32,
                ..Default::default()
            },
        )
        .unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(), vec![32]);
        assert_eq!(dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(), vec![1]);
        match dec.read_image().unwrap() {
            DecodingResult::U32(d) => assert_eq!(d, values),
            other => panic!("expected U32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn f64_samples_round_trip_exactly() {
        let dir = tmpdir("f64");
        let out = dir.join("out.tif");
        let values: Vec<f64> = vec![0.0, 1.0, std::f64::consts::PI, 1e-300, -2.5];
        let img = Typed {
            w: 5,
            h: 1,
            c: 1,
            planes: vec![values.clone()],
        };
        write_tiff_native(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::F64,
                ..Default::default()
            },
        )
        .unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(), vec![64]);
        assert_eq!(dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(), vec![3]);
        match dec.read_image().unwrap() {
            DecodingResult::F64(d) => assert_eq!(d, values),
            other => panic!("expected F64, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tiled_output_compresses_per_tile() {
        use crate::formats::tiff_write::options::{Compression, Layout};
        let dir = tmpdir("tiles-lzw");
        let out = dir.join("out.tif");
        let img = Synth::new(300, 200, 3);
        let opts = TiffWriteOptions {
            layout: Layout::Tiles { size: 256 },
            compression: Compression::Lzw,
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), 5);
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 300 * 200 * 3);
                // Per-sample, not just per-length: a compressor-state leak
                // across tiles (e.g. a stale `packed` buffer prefixing a
                // later tile's bytes with an earlier tile's) would still
                // decode to the right sample count but wrong pixels from
                // the second tile onward, which a length-only check misses.
                let total = (300 * 200 * 3) as f32;
                for (j, &v) in data.iter().enumerate() {
                    let (p, ch) = (j / 3, j % 3);
                    let want = (ch * 300 * 200 + p) as f32 / total;
                    assert!((v - want).abs() < 1e-7, "sample {j}");
                }
            }
            other => panic!("expected F32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_tags_are_written_when_requested() {
        use crate::formats::tiff_write::options::{Resolution, ResolutionUnit};
        let dir = tmpdir("meta");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 1);
        let icc: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let opts = TiffWriteOptions {
            icc_profile: Some(icc.clone()),
            description: Some("M31 mosaic".to_string()),
            copyright: Some("Copyright (c) 2026 Astrometrical".to_string()),
            software: Some("Mega Pixel Processor 0.1.0".to_string()),
            resolution: Some(Resolution {
                x: 300.0,
                y: 300.0,
                unit: ResolutionUnit::Inch,
            }),
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u8_vec(Tag::IccProfile).unwrap(), icc);
        assert_eq!(
            dec.get_tag_ascii_string(Tag::ImageDescription).unwrap(),
            "M31 mosaic"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Copyright).unwrap(),
            "Copyright (c) 2026 Astrometrical"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Software).unwrap(),
            "Mega Pixel Processor 0.1.0"
        );
        // `tiff` 0.11.3's `Value::into_f64` only matches `Value::Double`, not
        // `Value::Rational` — a RATIONAL-typed tag never converts to f64
        // through `get_tag_f64` (confirmed by reading `decoder::ifd::Value`
        // in the vendored source: `into_f64` has no `Rational` arm and falls
        // through to `InvalidTypeForTag`). So this reads the raw
        // numerator/denominator instead of asserting through `get_tag_f64`.
        for tag in [Tag::XResolution, Tag::YResolution] {
            match dec.get_tag(tag).unwrap() {
                tiff::decoder::ifd::Value::Rational(n, d) => {
                    assert_eq!((n, d), (300_000, 1000), "{tag:?}");
                    assert_eq!(n as f64 / d as f64, 300.0, "{tag:?} as f64");
                }
                other => panic!("expected {tag:?} to be Rational, got {other:?}"),
            }
        }
        assert_eq!(dec.get_tag_u64(Tag::ResolutionUnit).unwrap(), 2); // inch
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_tags_are_absent_when_not_requested() {
        let dir = tmpdir("nometa");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 1);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        for tag in [
            Tag::IccProfile,
            Tag::ImageDescription,
            Tag::Copyright,
            Tag::Software,
            Tag::XResolution,
        ] {
            assert!(dec.get_tag(tag).is_err(), "{tag:?} should be absent");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn strip_progress_reports_rows_written_monotonically_up_to_height() {
        // 3x7 gray f32 with a 64-byte target: 5 rows per strip, so two
        // strips and therefore two progress reports, the last one complete.
        let dir = tmpdir("progress-strips");
        let out = dir.join("out.tif");
        let img = Synth::new(3, 7, 1);
        let mut seen: Vec<(u64, u64)> = Vec::new();
        write_tiff_with_target_and_progress(
            &img,
            &out,
            &TiffWriteOptions::default(),
            64,
            &mut |done, total| seen.push((done, total)),
        )
        .unwrap();
        assert_eq!(seen, vec![(5, 7), (7, 7)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tile_progress_reports_rows_after_each_completed_tile_row() {
        use crate::formats::tiff_write::options::Layout;
        let dir = tmpdir("progress-tiles");
        let out = dir.join("out.tif");
        // 300x200 in 128-pixel tiles: two tile rows, the second partial.
        let img = Synth::new(300, 200, 1);
        let opts = TiffWriteOptions {
            layout: Layout::Tiles { size: 128 },
            ..Default::default()
        };
        let mut seen: Vec<(u64, u64)> = Vec::new();
        write_tiff_with_progress(&img, &out, &opts, &mut |done, total| {
            seen.push((done, total))
        })
        .unwrap();
        assert_eq!(seen, vec![(128, 200), (200, 200)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn native_write_reports_progress_too() {
        let dir = tmpdir("progress-native");
        let out = dir.join("out.tif");
        let img = NativeSynth {
            w: 3,
            h: 7,
            plane: vec![40_000u16; 21],
        };
        let opts = TiffWriteOptions {
            bit_depth: BitDepth::U16,
            ..Default::default()
        };
        let mut seen: Vec<(u64, u64)> = Vec::new();
        write_tiff_native_with_progress(&img, &out, &opts, &mut |done, total| {
            seen.push((done, total))
        })
        .unwrap();
        // One 4 MiB strip holds the whole image, so a single, complete report.
        assert_eq!(seen, vec![(7, 7)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn parallel_encode_is_byte_identical_to_single_threaded() {
        // 64-byte blocks on a 5x37 RGB image: many strips, most threads
        // busy at once, and a partial final strip. Block order in the file
        // must not depend on which worker finished first.
        let dir = tmpdir("parallel-strips");
        let img = Synth::new(5, 37, 3);
        let mut files = Vec::new();
        for threads in [1usize, 4, 7] {
            let out = dir.join(format!("t{threads}.tif"));
            let tuning = EncodeTuning {
                target_block_bytes: 64,
                threads,
            };
            write_tiff_tuned(
                &img,
                &out,
                &TiffWriteOptions::default(),
                tuning,
                &mut |_, _| {},
            )
            .unwrap();
            files.push(std::fs::read(&out).unwrap());
        }
        assert_eq!(files[0], files[1], "4 threads differ from 1");
        assert_eq!(files[0], files[2], "7 threads differ from 1");
        assert!(
            files[0].len() > 5 * 37 * 3 * 4,
            "the file should hold all the pixels"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parallel_compressed_tiles_round_trip_with_progress_in_order() {
        use crate::formats::tiff_write::options::{Compression, Layout};
        // 300x200 RGB u16 in 128-pixel deflate tiles: six tiles, two tile
        // rows, encoded on four threads. Pixels must land where they belong
        // and the two progress reports must still arrive in order.
        let dir = tmpdir("parallel-tiles");
        let out = dir.join("out.tif");
        let img = Synth::new(300, 200, 3);
        let opts = TiffWriteOptions {
            bit_depth: BitDepth::U16,
            layout: Layout::Tiles { size: 128 },
            compression: Compression::Deflate,
            ..Default::default()
        };
        let mut seen = Vec::new();
        let tuning = EncodeTuning {
            target_block_bytes: TARGET_BLOCK_BYTES,
            threads: 4,
        };
        write_tiff_tuned(&img, &out, &opts, tuning, &mut |d, t| seen.push((d, t))).unwrap();
        assert_eq!(seen, vec![(128, 200), (200, 200)]);

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(data) => {
                assert_eq!(data.len(), 300 * 200 * 3);
                for y in [0u64, 127, 128, 199] {
                    for x in [0u64, 127, 128, 255, 256, 299] {
                        for c in 0..3u64 {
                            let want = u16::from_unit_f32(img.row(c, y)[x as usize]);
                            let got = data[((y * 300 + x) * 3 + c) as usize];
                            assert_eq!(got, want, "pixel ({x},{y}) channel {c}");
                        }
                    }
                }
            }
            other => panic!("expected U16 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
