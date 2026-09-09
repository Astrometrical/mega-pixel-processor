//! Streaming TIFF/BigTIFF reader.
//!
//! Opens a TIFF (classic or BigTIFF), validates its header against the
//! colour/sample-format subset this reader supports, and decodes it
//! chunk-by-chunk (strip or tile) into de-interleaved per-channel row
//! bands, without ever holding the whole image in memory. Each chunk is
//! decoded exactly once. This is the read-side counterpart to
//! [`super::tiff`], which only writes.
//!
//! # Accepted colour
//!
//! Grayscale, gray+alpha, RGB and RGBA pass straight through. Three further
//! storage forms are accepted because unwinding them is exact or
//! spec-defined, so it costs nothing in fidelity:
//!
//! * **Palette colour** — the `ColorMap` tag is an exact lookup table from
//!   index to 16-bit RGB, so a palette image reads as RGB with
//!   [`SampleFormat::U16`] samples.
//! * **`YCbCr`** — TIFF 6.0 section 21 defines the transform and its
//!   inverse in terms of the file's own `YCbCrCoefficients` and
//!   `ReferenceBlackWhite` tags, so it inverts to RGB exactly (up to
//!   rounding back into the sample type).
//! * **Planar (separate) sample storage** — a purely mechanical
//!   de-interleave; the samples themselves are untouched.
//!
//! CMYK and CIE/ICC Lab stay rejected. Converting either to RGB faithfully
//! needs ICC colour management, which this crate does not do, and an
//! unmanaged conversion would silently misrepresent the pixel values —
//! exactly the failure mode the linear-data discipline exists to prevent.
//!
//! Designed from the public TIFF 6.0 and BigTIFF specifications and the
//! `tiff` crate's own (0.11.3) public API and doc comments — never from
//! PixInsight's PCL module sources (see the clean-room rule in
//! `CLAUDE.md`).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tiff::ColorType as TiffColorType;
use tiff::decoder::ifd::Value;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;

use crate::{Error, Result};

/// TIFF `PhotometricInterpretation` = 1: BlackIsZero grayscale.
const PHOTOMETRIC_BLACK_IS_ZERO: u16 = 1;

/// TIFF `PhotometricInterpretation` = 3: palette colour (RGBPalette).
const PHOTOMETRIC_PALETTE: u16 = 3;

/// TIFF `YCbCrCoefficients` (tag 529): three RATIONALs, the luma weights of
/// red, green and blue. Not in the `tiff` crate's [`Tag`] enum, so it is
/// addressed by number.
const TAG_YCBCR_COEFFICIENTS: u16 = 529;

/// TIFF `ReferenceBlackWhite` (tag 532): six RATIONALs, the (black, white)
/// reference pair of each of the three channels. Not in the `tiff` crate's
/// [`Tag`] enum, so it is addressed by number.
const TAG_REFERENCE_BLACK_WHITE: u16 = 532;

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

/// A `Read + Seek` view of a file with one byte range substituted in flight.
///
/// This exists for exactly one purpose: palette colour. The `tiff` crate's
/// decoder refuses a `PhotometricInterpretation::RGBPalette` image outright —
/// `colortype()` fails and so does every chunk read — so there is no way to
/// ask it even for the raw `ColorMap` indices. Presenting the file's
/// photometric tag as `BlackIsZero` instead makes the decoder hand those
/// indices back as ordinary single-channel samples, with all of its strip,
/// tile, predictor and compression handling intact; [`TiffReader`] then
/// expands them through the file's own (unmodified) `ColorMap` tag. Nothing
/// on disk is touched — the substitution lives entirely in [`Read::read`].
#[derive(Debug)]
struct PatchedReader<R> {
    /// The underlying file.
    inner: R,
    /// Offset of the substituted range and the bytes to report there.
    patch: Option<(u64, Vec<u8>)>,
    /// Current file position, tracked so `read` knows which range it just
    /// returned and can decide whether the patch overlaps it.
    pos: u64,
}

impl<R: Read + Seek> PatchedReader<R> {
    /// Wrap `inner`, substituting `patch`'s bytes at its offset. `inner`
    /// must be positioned at the start of the file.
    fn new(inner: R, patch: Option<(u64, Vec<u8>)>) -> Self {
        Self {
            inner,
            patch,
            pos: 0,
        }
    }
}

impl<R: Read> Read for PatchedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some((offset, bytes)) = &self.patch {
            // Overwrite whatever part of the patched range this read covered.
            // A read can straddle either end of it, so intersect explicitly.
            let (read_start, read_end) = (self.pos, self.pos + n as u64);
            let (patch_start, patch_end) = (*offset, *offset + bytes.len() as u64);
            let lo = read_start.max(patch_start);
            let hi = read_end.min(patch_end);
            if lo < hi {
                let dst = (lo - read_start) as usize;
                let src = (lo - patch_start) as usize;
                let len = (hi - lo) as usize;
                buf[dst..dst + len].copy_from_slice(&bytes[src..src + len]);
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for PatchedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

/// The first IFD's `PhotometricInterpretation` entry: what it says, and
/// where its (always inline) value sits in the file.
#[derive(Debug, Clone, Copy)]
struct PhotometricEntry {
    /// The interpretation code the file declares.
    code: u16,
    /// File offset of the entry's inline value field.
    value_offset: u64,
    /// Width of that value in bytes: 2 for SHORT, 4 for LONG.
    value_len: usize,
    /// True when the file is little-endian (`II`).
    little_endian: bool,
}

impl PhotometricEntry {
    /// The `(offset, bytes)` patch that makes this entry read as `code`.
    fn patch_to(&self, code: u16) -> (u64, Vec<u8>) {
        let bytes = if self.value_len == 2 {
            if self.little_endian {
                code.to_le_bytes().to_vec()
            } else {
                code.to_be_bytes().to_vec()
            }
        } else if self.little_endian {
            u32::from(code).to_le_bytes().to_vec()
        } else {
            u32::from(code).to_be_bytes().to_vec()
        };
        (self.value_offset, bytes)
    }
}

/// Locate the first IFD's `PhotometricInterpretation` entry by walking the
/// file header and directory by hand, per TIFF 6.0 / BigTIFF.
///
/// This runs *before* the `tiff` decoder is constructed, because the decoder
/// gives no way to find a tag's file offset — and for a palette image no way
/// to read the pixels at all (see [`PatchedReader`]). Anything unexpected
/// (a header this reader does not recognize, a photometric entry that is not
/// a single SHORT/LONG, a short read) yields `None` rather than an error:
/// the decoder is then constructed unpatched and gets to report the problem
/// in its own words.
fn scan_photometric(file: &mut File) -> io::Result<Option<PhotometricEntry>> {
    /// Tag 262.
    const TAG_PHOTOMETRIC: u16 = 262;

    file.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 16];
    if file.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let little_endian = match &head[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(None),
    };
    let u16at = |b: &[u8]| {
        let v = [b[0], b[1]];
        if little_endian {
            u16::from_le_bytes(v)
        } else {
            u16::from_be_bytes(v)
        }
    };
    let u32at = |b: &[u8]| {
        let v = [b[0], b[1], b[2], b[3]];
        if little_endian {
            u32::from_le_bytes(v)
        } else {
            u32::from_be_bytes(v)
        }
    };
    let u64at = |b: &[u8]| {
        let v = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        if little_endian {
            u64::from_le_bytes(v)
        } else {
            u64::from_be_bytes(v)
        }
    };

    // Classic TIFF: magic 42, a 4-byte first-IFD offset at byte 4, 2-byte
    // entry count, 12-byte entries with the value inline at entry+8.
    // BigTIFF: magic 43, offset size 8 at byte 4, a 8-byte first-IFD offset
    // at byte 8, 8-byte entry count, 20-byte entries with the value inline
    // at entry+12.
    let (ifd_offset, count_len, entry_len, value_field) = match u16at(&head[2..4]) {
        42 => (u64::from(u32at(&head[4..8])), 2usize, 12usize, 8usize),
        43 => {
            if u16at(&head[4..6]) != 8 || u16at(&head[6..8]) != 0 {
                return Ok(None);
            }
            (u64at(&head[8..16]), 8usize, 20usize, 12usize)
        }
        _ => return Ok(None),
    };

    file.seek(SeekFrom::Start(ifd_offset))?;
    let mut count_buf = vec![0u8; count_len];
    if file.read_exact(&mut count_buf).is_err() {
        return Ok(None);
    }
    let count = if count_len == 8 {
        u64at(&count_buf)
    } else {
        u64::from(u16at(&count_buf))
    };
    // A directory cannot legitimately hold more entries than there are tag
    // numbers; a larger count means a corrupt file, not a long directory.
    if count > u64::from(u16::MAX) {
        return Ok(None);
    }

    let mut entry = vec![0u8; entry_len];
    for i in 0..count {
        let entry_offset = ifd_offset + count_len as u64 + i * entry_len as u64;
        file.seek(SeekFrom::Start(entry_offset))?;
        if file.read_exact(&mut entry).is_err() {
            return Ok(None);
        }
        if u16at(&entry[0..2]) != TAG_PHOTOMETRIC {
            continue;
        }
        // Types 3 (SHORT) and 4 (LONG); a count of 1 is the only legal form,
        // and both fit inline, left-justified in the value field.
        let value_len = match u16at(&entry[2..4]) {
            3 => 2usize,
            4 => 4usize,
            _ => return Ok(None),
        };
        let value_count = if count_len == 8 {
            u64at(&entry[4..12])
        } else {
            u64::from(u32at(&entry[4..8]))
        };
        if value_count != 1 {
            return Ok(None);
        }
        let field = &entry[value_field..];
        let code = if value_len == 2 {
            u16at(field)
        } else {
            match u16::try_from(u32at(field)) {
                Ok(c) => c,
                Err(_) => return Ok(None),
            }
        };
        return Ok(Some(PhotometricEntry {
            code,
            value_offset: entry_offset + value_field as u64,
            value_len,
            little_endian,
        }));
    }
    Ok(None)
}

/// A TIFF `ColorMap`: 256 red entries, then 256 green, then 256 blue, each
/// a 16-bit value. Only 8-bit indices are supported, so the table is always
/// 3 * 256 entries long.
#[derive(Debug)]
struct PaletteMap {
    /// The flat R-then-G-then-B table, 768 entries.
    entries: Vec<u16>,
}

impl PaletteMap {
    /// The 16-bit RGB triple `index` maps to.
    fn lookup(&self, index: u8) -> [u16; 3] {
        let i = usize::from(index);
        [
            self.entries[i],
            self.entries[256 + i],
            self.entries[512 + i],
        ]
    }
}

/// TIFF 6.0 section 21's `YCbCr` -> RGB inverse, parameterized by the
/// file's own `YCbCrCoefficients` and `ReferenceBlackWhite` tags (or, when
/// those are absent, the CCIR 601-1 defaults the spec prescribes).
#[derive(Debug, Clone, Copy)]
struct YCbCrTransform {
    /// Luma coefficients for red, green and blue. Default (0.299, 0.587,
    /// 0.114) — CCIR 601-1.
    luma: [f32; 3],
    /// `ReferenceBlackWhite` as a (black, white) pair per channel. Default
    /// (0, 255) for luma and (128, 255) for both chroma channels.
    reference: [[f32; 2]; 3],
}

impl Default for YCbCrTransform {
    fn default() -> Self {
        Self {
            luma: [0.299, 0.587, 0.114],
            reference: [[0.0, 255.0], [128.0, 255.0], [128.0, 255.0]],
        }
    }
}

impl YCbCrTransform {
    /// Convert one 8-bit `Y`/`Cb`/`Cr` triple to 8-bit RGB.
    ///
    /// Two steps, both straight from the spec: undo the reference
    /// black/white scaling into the coding ranges (255 for luma, 127 for
    /// chroma at 8 bits), then undo the luma/chroma mixing. The result is
    /// clamped, because a file may legitimately encode chroma that lands
    /// outside the RGB cube.
    fn to_rgb(self, y: u8, cb: u8, cr: u8) -> [u8; 3] {
        let descale = |v: u8, ch: usize, coding_range: f32| {
            let [black, white] = self.reference[ch];
            let span = white - black;
            if span == 0.0 {
                0.0
            } else {
                (f32::from(v) - black) * coding_range / span
            }
        };
        let [lr, lg, lb] = self.luma;
        let y = descale(y, 0, 255.0);
        let cb = descale(cb, 1, 127.0);
        let cr = descale(cr, 2, 127.0);

        let r = cr * (2.0 - 2.0 * lr) + y;
        let b = cb * (2.0 - 2.0 * lb) + y;
        // Green is recovered from the luma identity Y = lr*R + lg*G + lb*B.
        let g = if lg == 0.0 {
            y
        } else {
            (y - lb * b - lr * r) / lg
        };

        [clamp_u8(r), clamp_u8(g), clamp_u8(b)]
    }
}

/// Round to the nearest integer and clamp into `u8`.
fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// The per-chunk pixel transform a file needs before its decoded samples
/// become the reader's output channels.
#[derive(Debug)]
enum Transform {
    /// The decoded samples already *are* the output channels; only
    /// de-interleaving is needed.
    Direct,
    /// The decoded samples are 8-bit `ColorMap` indices, expanded to three
    /// 16-bit RGB channels.
    Palette(PaletteMap),
    /// The decoded samples are 8-bit `Y`/`Cb`/`Cr` triples, converted to
    /// three 8-bit RGB channels.
    YCbCr(YCbCrTransform),
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
    decoder: Decoder<PatchedReader<File>>,
    info: TiffInfo,
    path: PathBuf,
    /// What each decoded chunk's samples must become.
    transform: Transform,
    /// True when the file stores each channel in its own chunks
    /// (`PlanarConfiguration = 2`) rather than interleaving them, so a chunk
    /// carries one channel rather than one pixel's worth of samples. Only
    /// ever set with [`Transform::Direct`]: palette storage is
    /// single-sample, which makes planar meaningless, and planar `YCbCr` is
    /// rejected at open time.
    planar: bool,
}

impl TiffReader {
    /// Open `path` and read+validate the first image's header.
    ///
    /// Grayscale, gray+alpha, RGB and RGBA are read directly; palette
    /// colour, `YCbCr` and planar (separate-plane) storage are unwound into
    /// them; see the module docs. Errors on CMYK or Lab colour (both need
    /// colour management this crate does not do), on multi-band storage that
    /// is neither, on signed/complex samples, and on a bit depth this reader
    /// does not recognize.
    pub fn open(path: &Path) -> Result<TiffReader> {
        let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

        // A palette image has to be presented to the decoder as BlackIsZero
        // to get its raw indices out at all; see `PatchedReader`. Everything
        // else is handed over untouched.
        let photometric = scan_photometric(&mut file).map_err(|e| Error::io(path, e))?;
        let is_palette = photometric.is_some_and(|p| p.code == PHOTOMETRIC_PALETTE);
        let patch = photometric
            .filter(|p| p.code == PHOTOMETRIC_PALETTE)
            .map(|p| p.patch_to(PHOTOMETRIC_BLACK_IS_ZERO));
        file.rewind().map_err(|e| Error::io(path, e))?;

        let mut decoder = Decoder::new(PatchedReader::new(file, patch))
            .map_err(|e| Error::format(path, format!("TIFF header: {e}")))?;

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

        // TIFF SampleFormat tag: 1 = unsigned int (default when absent),
        // 2 = signed int, 3 = IEEE float, 4 = void/complex. Written with one
        // value per sample (e.g. 3 identical entries for RGB); we require a
        // uniform format across channels and read just the first entry.
        let sample_format_tag = decoder
            .find_tag_unsigned_vec::<u16>(Tag::SampleFormat)
            .map_err(|e| Error::format(path, format!("TIFF SampleFormat tag: {e}")))?
            .and_then(|v| v.first().copied())
            .unwrap_or(1);

        // PlanarConfiguration 2 stores each channel in its own chunks
        // instead of interleaving them into pixels. That is a mechanical
        // de-interleave, handled in `read_strips`/`read_tiles`.
        let planar_config = decoder
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
            .map_err(|e| Error::format(path, format!("TIFF PlanarConfiguration tag: {e}")))?
            .unwrap_or(1);

        let (color, color_channels, alpha_channels, sample_format, transform) = if is_palette {
            // With the photometric tag presented as BlackIsZero, a palette
            // image decodes as single-channel indices.
            match color_type {
                TiffColorType::Gray(8) => {}
                other => {
                    return Err(Error::format(
                        path,
                        format!(
                            "unsupported palette TIFF: only 8-bit ColorMap indices are supported \
                             (this file decodes as {other:?})"
                        ),
                    ));
                }
            }
            if sample_format_tag != 1 {
                return Err(Error::format(
                    path,
                    format!(
                        "unsupported palette TIFF: ColorMap indices must be unsigned integers \
                         (SampleFormat tag is {sample_format_tag})"
                    ),
                ));
            }
            let entries = decoder
                .get_tag_u16_vec(Tag::ColorMap)
                .map_err(|e| Error::format(path, format!("TIFF ColorMap tag: {e}")))?;
            if entries.len() != 3 * 256 {
                return Err(Error::format(
                    path,
                    format!(
                        "malformed palette TIFF: ColorMap has {} entries, expected {} \
                         (3 * 256 for 8-bit indices)",
                        entries.len(),
                        3 * 256
                    ),
                ));
            }
            // ColorMap entries are 16-bit, so RGB16 is the lossless target.
            (
                ColorClass::Rgb,
                3,
                0,
                SampleFormat::U16,
                Transform::Palette(PaletteMap { entries }),
            )
        } else {
            match color_type {
                TiffColorType::Gray(b) => (
                    ColorClass::Gray,
                    1,
                    0,
                    resolve_sample_format(path, sample_format_tag, b)?,
                    Transform::Direct,
                ),
                TiffColorType::GrayA(b) => (
                    ColorClass::Gray,
                    1,
                    1,
                    resolve_sample_format(path, sample_format_tag, b)?,
                    Transform::Direct,
                ),
                TiffColorType::RGB(b) => (
                    ColorClass::Rgb,
                    3,
                    0,
                    resolve_sample_format(path, sample_format_tag, b)?,
                    Transform::Direct,
                ),
                TiffColorType::RGBA(b) => (
                    ColorClass::Rgb,
                    3,
                    1,
                    resolve_sample_format(path, sample_format_tag, b)?,
                    Transform::Direct,
                ),
                TiffColorType::YCbCr(8) => {
                    if sample_format_tag != 1 {
                        return Err(Error::format(
                            path,
                            format!(
                                "unsupported YCbCr TIFF: samples must be unsigned integers \
                                 (SampleFormat tag is {sample_format_tag})"
                            ),
                        ));
                    }
                    // The inverse mixes all three channels of one pixel, so
                    // it needs them in the same chunk. Planar YCbCr would
                    // need three chunks buffered against each other, which
                    // this streaming reader deliberately does not do.
                    if planar_config != 1 {
                        return Err(Error::format(
                            path,
                            "unsupported TIFF: YCbCr stored planar (PlanarConfiguration = 2) \
                             is not supported; the YCbCr -> RGB inverse needs all three \
                             channels of a pixel in one chunk",
                        ));
                    }
                    (
                        ColorClass::Rgb,
                        3,
                        0,
                        SampleFormat::U8,
                        Transform::YCbCr(read_ycbcr_transform(path, &mut decoder)?),
                    )
                }
                TiffColorType::YCbCr(b) => {
                    return Err(Error::format(
                        path,
                        format!(
                            "unsupported YCbCr TIFF: only 8-bit YCbCr is supported (this file is {b}-bit)"
                        ),
                    ));
                }
                other @ (TiffColorType::CMYK(_)
                | TiffColorType::CMYKA(_)
                | TiffColorType::Lab(_)) => {
                    return Err(Error::format(
                        path,
                        format!(
                            "unsupported TIFF colour type {other:?}: converting it to RGB \
                             faithfully needs ICC colour management, which this reader does \
                             not do, and an unmanaged conversion would silently misrepresent \
                             the pixel values"
                        ),
                    ));
                }
                other => {
                    return Err(Error::format(
                        path,
                        format!("unsupported TIFF colour type: {other:?}"),
                    ));
                }
            }
        };

        let total_channels = color_channels + alpha_channels;
        // Planar storage only means anything when the *file* stores more than
        // one sample per pixel, which is exactly the `Transform::Direct` case:
        // a palette file is single-sample whatever its PlanarConfiguration
        // says (and its output channel count is the ColorMap's, not the
        // file's), and planar YCbCr was rejected above.
        let planar =
            planar_config != 1 && total_channels > 1 && matches!(transform, Transform::Direct);

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
            transform,
            planar,
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
        if strip_count == 0 {
            return Ok(());
        }
        let (_, rows_per_strip) = self.decoder.chunk_dimensions();

        // Planar storage repeats the whole strip grid once per channel, in
        // channel order, so `strip_count` is a whole multiple of it.
        let strips_per_plane = if self.planar {
            if strip_count % total_channels != 0 {
                return Err(Error::format(
                    path,
                    format!(
                        "malformed planar TIFF: {strip_count} strips is not a whole multiple of \
                         the {total_channels} channels"
                    ),
                ));
            }
            strip_count / total_channels
        } else {
            strip_count
        };
        // Chunky chunks carry every channel; planar chunks carry exactly one.
        let chunk_channels = if self.planar { 1 } else { total_channels };

        for i in 0..strip_count {
            let (w_c, h_c) = self.decoder.chunk_data_dimensions(i);
            if u64::from(w_c) != width {
                return Err(Error::format(
                    path,
                    format!("strip {i} width {w_c} does not match image width {width}"),
                ));
            }
            let start_row = u64::from(i % strips_per_plane) * u64::from(rows_per_strip);
            let result = self
                .decoder
                .read_chunk(i)
                .map_err(|e| Error::format(path, format!("TIFF strip {i}: {e}")))?;
            let planes =
                chunk_planes(path, result, chunk_channels, sample_format, &self.transform)?;
            if self.planar {
                sink(i / strips_per_plane, start_row, h_c, &planes[0])?;
            } else {
                for (ch, bytes) in planes.iter().enumerate() {
                    sink(ch as u32, start_row, h_c, bytes)?;
                }
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
        // Planar storage repeats the whole tile grid once per channel, in
        // channel order, so `tile_count` is a whole multiple of it.
        let tiles_per_plane = if self.planar {
            if tile_count % total_channels != 0 {
                return Err(Error::format(
                    path,
                    format!(
                        "malformed planar TIFF: {tile_count} tiles is not a whole multiple of \
                         the {total_channels} channels"
                    ),
                ));
            }
            tile_count / total_channels
        } else {
            tile_count
        };
        let tiles_across = (width as u32).div_ceil(tile_w);
        let tiles_down = tiles_per_plane.div_ceil(tiles_across);
        // Chunky chunks carry every channel; planar chunks carry exactly one.
        let chunk_channels = if self.planar { 1 } else { total_channels };
        let grids = if self.planar { total_channels } else { 1 };

        for plane in 0..grids {
            for tr in 0..tiles_down {
                let mut row_bands: Vec<Vec<u8>> = Vec::new();
                let mut band_rows: u32 = 0;
                for tc in 0..tiles_across {
                    let idx = plane * tiles_per_plane + tr * tiles_across + tc;
                    let (w_c, h_c) = self.decoder.chunk_data_dimensions(idx);
                    band_rows = h_c;
                    if row_bands.is_empty() {
                        row_bands = (0..chunk_channels)
                            .map(|_| vec![0u8; h_c as usize * width as usize * sample_bytes])
                            .collect();
                    }
                    let result = self
                        .decoder
                        .read_chunk(idx)
                        .map_err(|e| Error::format(path, format!("TIFF tile {idx}: {e}")))?;
                    let planes =
                        chunk_planes(path, result, chunk_channels, sample_format, &self.transform)?;
                    let x_off = u64::from(tc) * u64::from(tile_w);
                    for (ch, src) in planes.iter().enumerate() {
                        let dst = &mut row_bands[ch];
                        for row in 0..h_c as usize {
                            let src_off = row * w_c as usize * sample_bytes;
                            let dst_off = (row * width as usize + x_off as usize) * sample_bytes;
                            dst[dst_off..dst_off + w_c as usize * sample_bytes].copy_from_slice(
                                &src[src_off..src_off + w_c as usize * sample_bytes],
                            );
                        }
                    }
                }
                let start_row = u64::from(tr) * u64::from(tile_h);
                if self.planar {
                    sink(plane, start_row, band_rows, &row_bands[0])?;
                } else {
                    for (ch, bytes) in row_bands.iter().enumerate() {
                        sink(ch as u32, start_row, band_rows, bytes)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Map the TIFF `SampleFormat` tag and a bit depth onto this reader's
/// [`SampleFormat`], or explain why the pair is out of scope.
fn resolve_sample_format(path: &Path, sample_format_tag: u16, bits: u8) -> Result<SampleFormat> {
    match (sample_format_tag, bits) {
        (1, 8) => Ok(SampleFormat::U8),
        (1, 16) => Ok(SampleFormat::U16),
        (1, 32) => Ok(SampleFormat::U32),
        (3, 32) => Ok(SampleFormat::F32),
        (3, 64) => Ok(SampleFormat::F64),
        (2, b) => Err(Error::format(
            path,
            format!("signed integer samples are not supported ({b}-bit)"),
        )),
        (1 | 3, b) => Err(Error::format(
            path,
            format!("unsupported TIFF bit depth: {b}"),
        )),
        (fmt, b) => Err(Error::format(
            path,
            format!(
                "unsupported TIFF sample format code {fmt} ({b}-bit; complex/void samples are not supported)"
            ),
        )),
    }
}

/// Read a RATIONAL-valued tag as a flat list of `f32`s, or `None` if the tag
/// is absent. Used for `YCbCrCoefficients` and `ReferenceBlackWhite`, which
/// the `tiff` crate models as `Value::Rational` but offers no float
/// accessor for.
fn rational_tag<R: Read + Seek>(decoder: &mut Decoder<R>, tag: u16) -> Option<Vec<f32>> {
    /// One RATIONAL as a float; a zero denominator is not a number the spec
    /// admits, so treat it as "unreadable" and fall back to the defaults.
    fn ratio(numerator: u32, denominator: u32) -> Option<f32> {
        (denominator != 0).then(|| numerator as f32 / denominator as f32)
    }

    let value = decoder
        .find_tag(Tag::from_u16_exhaustive(tag))
        .ok()
        .flatten()?;
    match value {
        Value::Rational(n, d) => ratio(n, d).map(|v| vec![v]),
        Value::List(items) => items
            .into_iter()
            .map(|v| match v {
                Value::Rational(n, d) => ratio(n, d),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Build the `YCbCr` -> RGB inverse for this file, from its
/// `YCbCrCoefficients` and `ReferenceBlackWhite` tags where present and the
/// TIFF 6.0 defaults where not.
///
/// A tag that is present but malformed (wrong length, a zero denominator, a
/// degenerate luma set) is refused rather than quietly replaced by the
/// default, because using the default in its place would misrepresent the
/// pixel values just as surely as an unmanaged CMYK conversion would.
fn read_ycbcr_transform<R: Read + Seek>(
    path: &Path,
    decoder: &mut Decoder<R>,
) -> Result<YCbCrTransform> {
    let mut transform = YCbCrTransform::default();

    if let Some(coefficients) = rational_tag(decoder, TAG_YCBCR_COEFFICIENTS) {
        let ok = coefficients.len() == 3
            && coefficients.iter().all(|c| c.is_finite())
            && coefficients[1] != 0.0;
        if !ok {
            return Err(Error::format(
                path,
                format!("malformed TIFF YCbCrCoefficients tag: {coefficients:?}"),
            ));
        }
        transform.luma = [coefficients[0], coefficients[1], coefficients[2]];
    }

    if let Some(reference) = rational_tag(decoder, TAG_REFERENCE_BLACK_WHITE) {
        if reference.len() != 6 || !reference.iter().all(|v| v.is_finite()) {
            return Err(Error::format(
                path,
                format!("malformed TIFF ReferenceBlackWhite tag: {reference:?}"),
            ));
        }
        transform.reference = [
            [reference[0], reference[1]],
            [reference[2], reference[3]],
            [reference[4], reference[5]],
        ];
    }

    Ok(transform)
}

/// Turn one decoded chunk into one contiguous native-endian byte buffer per
/// output channel, row-major within the chunk.
///
/// `chunk_channels` is how many output channels this chunk carries: every
/// channel for a chunky file, exactly one for a planar file. `sample_format`
/// is the reader's *output* format, which for a palette image is wider than
/// the decoded samples (8-bit indices in, 16-bit RGB out).
fn chunk_planes(
    path: &Path,
    result: DecodingResult,
    chunk_channels: u32,
    sample_format: SampleFormat,
    transform: &Transform,
) -> Result<Vec<Vec<u8>>> {
    match transform {
        Transform::Direct => deinterleave(path, result, chunk_channels, sample_format),
        Transform::Palette(map) => expand_palette(path, result, map),
        Transform::YCbCr(t) => convert_ycbcr(path, result, t),
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

/// Expand a chunk of 8-bit `ColorMap` indices into three native-endian
/// 16-bit RGB planes.
fn expand_palette(path: &Path, result: DecodingResult, map: &PaletteMap) -> Result<Vec<Vec<u8>>> {
    let DecodingResult::U8(indices) = result else {
        return Err(Error::format(
            path,
            format!("internal error: palette chunk decoded as {result:?}, expected 8-bit indices"),
        ));
    };
    let mut planes: Vec<Vec<u8>> = (0..3)
        .map(|_| Vec::with_capacity(indices.len() * 2))
        .collect();
    for &index in &indices {
        let rgb = map.lookup(index);
        for (plane, value) in planes.iter_mut().zip(rgb) {
            plane.extend_from_slice(&value.to_ne_bytes());
        }
    }
    Ok(planes)
}

/// Convert a chunk of interleaved 8-bit `Y`/`Cb`/`Cr` triples into three
/// 8-bit RGB planes.
fn convert_ycbcr(
    path: &Path,
    result: DecodingResult,
    transform: &YCbCrTransform,
) -> Result<Vec<Vec<u8>>> {
    let DecodingResult::U8(samples) = result else {
        return Err(Error::format(
            path,
            format!("internal error: YCbCr chunk decoded as {result:?}, expected 8-bit samples"),
        ));
    };
    if samples.len() % 3 != 0 {
        return Err(Error::format(
            path,
            format!(
                "malformed YCbCr TIFF: chunk holds {} samples, not a whole number of pixels",
                samples.len()
            ),
        ));
    }
    let pixels = samples.len() / 3;
    let mut planes: Vec<Vec<u8>> = (0..3).map(|_| Vec::with_capacity(pixels)).collect();
    for triple in samples.chunks_exact(3) {
        let rgb = transform.to_rgb(triple[0], triple[1], triple[2]);
        for (plane, value) in planes.iter_mut().zip(rgb) {
            plane.push(value);
        }
    }
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

    /// Write a minimal palette-colour TIFF: 8-bit indices plus a 256-entry
    /// ColorMap. Test-only; the production writer never emits palette.
    ///
    /// Each of the three ColorMap blocks gets its own distinct ramp, so a
    /// reader that confused the red/green/blue blocks — or that failed to
    /// look the index up at all — cannot pass.
    fn write_palette_fixture(path: &Path) {
        write_palette_fixture_with(path, 1);
    }

    /// [`write_palette_fixture`], with an explicit `PlanarConfiguration`.
    /// A palette image stores one sample per pixel, so the tag is
    /// meaningless for it either way — which is the point of the variant.
    fn write_palette_fixture_with(path: &Path, planar_config: u16) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        // 4x2 image, indices 0..8.
        let indices: Vec<u8> = (0u8..8).collect();
        let offset = dir.write_data(&indices[..]).unwrap();

        // ColorMap: 3 * 256 u16 entries, R then G then B. Entry i maps to
        // (i * 257, 1000 + 3i, 60000 - 5i) — three unrelated ramps, so a
        // mis-read palette cannot pass as correct.
        let mut map = vec![0u16; 3 * 256];
        for i in 0..256usize {
            map[i] = (i * 257) as u16;
            map[256 + i] = (1000 + 3 * i) as u16;
            map[512 + i] = (60000 - 5 * i) as u16;
        }

        dir.write_tag(Tag::ImageWidth, 4u32).unwrap();
        dir.write_tag(Tag::ImageLength, 2u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16][..]).unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 1u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 3u16).unwrap(); // palette
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, planar_config)
            .unwrap();
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &[offset as u32][..])
            .unwrap();
        dir.write_tag(Tag::StripByteCounts, &[8u32][..]).unwrap();
        dir.write_tag(Tag::ColorMap, &map[..]).unwrap();
        dir.finish().unwrap();
    }

    /// The pixel values [`write_palette_fixture`] encodes: index `i` at
    /// pixel `i`, resolved through the ColorMap.
    fn palette_expected(pixel: usize) -> [u16; 3] {
        [
            (pixel * 257) as u16,
            (1000 + 3 * pixel) as u16,
            (60000 - 5 * pixel) as u16,
        ]
    }

    /// Write a minimal separate-planes (`PlanarConfiguration = 2`) RGB8
    /// TIFF: 4x4, two rows per strip, so each of the three planes spans two
    /// strips and the file holds six in plane-major order (all of red's
    /// strips, then green's, then blue's — the order TIFF 6.0 prescribes).
    /// Test-only; the production writer only ever emits chunky.
    ///
    /// Multiple strips per plane are the point: a reader that derived a
    /// band's start row from the raw strip index rather than its position
    /// within its own plane would place the later strips' rows outside the
    /// image.
    fn write_planar_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        // Channel c, pixel p -> planar_expected(c, p); each channel occupies
        // its own value range so a swapped channel is unmistakable.
        let mut offsets = Vec::new();
        for c in 0..3usize {
            for strip in 0..2usize {
                let rows: Vec<u8> = (0..8usize)
                    .map(|i| planar_expected(c, strip * 8 + i))
                    .collect();
                offsets.push(dir.write_data(&rows[..]).unwrap() as u32);
            }
        }

        dir.write_tag(Tag::ImageWidth, 4u32).unwrap();
        dir.write_tag(Tag::ImageLength, 4u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8][..])
            .unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 3u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 2u16).unwrap(); // RGB
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 2u16).unwrap(); // separate
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &offsets[..]).unwrap();
        dir.write_tag(Tag::StripByteCounts, &[8u32; 6][..]).unwrap();
        dir.write_tag(Tag::SampleFormat, &[1u16, 1, 1][..]).unwrap();
        dir.finish().unwrap();
    }

    /// The sample [`write_planar_fixture`] stores for channel `c` at pixel
    /// `p` (row-major over the whole 4x4 image).
    fn planar_expected(c: usize, p: usize) -> u8 {
        (10 + c * 90 + p) as u8
    }

    /// Write a minimal uncompressed CMYK TIFF. Test-only; CMYK is out of
    /// scope for this reader by design, and the production writer never
    /// emits it.
    fn write_cmyk_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        // 2x2 pixels, 4 samples each (C, M, Y, K).
        let data: Vec<u8> = (0u8..16).collect();
        let offset = dir.write_data(&data[..]).unwrap();

        dir.write_tag(Tag::ImageWidth, 2u32).unwrap();
        dir.write_tag(Tag::ImageLength, 2u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8, 8][..])
            .unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 4u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 5u16).unwrap(); // CMYK
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 1u16).unwrap();
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &[offset as u32][..])
            .unwrap();
        dir.write_tag(Tag::StripByteCounts, &[16u32][..]).unwrap();
        dir.finish().unwrap();
    }

    /// Write a minimal uncompressed ICC L*a*b* TIFF. Test-only; Lab is out
    /// of scope for this reader by design.
    fn write_lab_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        let data: Vec<u8> = (0u8..12).collect(); // 2x2 pixels, 3 samples each
        let offset = dir.write_data(&data[..]).unwrap();

        dir.write_tag(Tag::ImageWidth, 2u32).unwrap();
        dir.write_tag(Tag::ImageLength, 2u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8][..])
            .unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 3u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 9u16).unwrap(); // IccLab
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 1u16).unwrap();
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &[offset as u32][..])
            .unwrap();
        dir.write_tag(Tag::StripByteCounts, &[12u32][..]).unwrap();
        dir.write_tag(Tag::SampleFormat, &[1u16, 1, 1][..]).unwrap();
        dir.finish().unwrap();
    }

    /// Write a minimal uncompressed, unsubsampled 8-bit YCbCr TIFF holding
    /// four pixels that are pure red, pure green, pure blue and white, in
    /// that order. The `Y`/`Cb`/`Cr` bytes below are those four RGB colours
    /// run through the forward CCIR 601-1 transform and rounded, so the
    /// reader's inverse has a known answer to land on. Test-only.
    fn write_ycbcr_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        #[rustfmt::skip]
        let data: Vec<u8> = vec![
             76,  85, 255, // (255,   0,   0)
            150,  44,  21, // (  0, 255,   0)
             29, 255, 107, // (  0,   0, 255)
            255, 128, 128, // (255, 255, 255)
        ];
        let offset = dir.write_data(&data[..]).unwrap();

        dir.write_tag(Tag::ImageWidth, 2u32).unwrap();
        dir.write_tag(Tag::ImageLength, 2u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8][..])
            .unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 3u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 6u16).unwrap(); // YCbCr
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 1u16).unwrap();
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &[offset as u32][..])
            .unwrap();
        dir.write_tag(Tag::StripByteCounts, &[12u32][..]).unwrap();
        dir.write_tag(Tag::ChromaSubsampling, &[1u16, 1][..])
            .unwrap();
        dir.finish().unwrap();
    }

    /// Drain a reader into one `Vec<u8>` per channel, indexed by pixel.
    fn collect_planes(reader: &mut TiffReader) -> Vec<Vec<u8>> {
        let w = reader.info().width as usize;
        let h = reader.info().height as usize;
        let sample_bytes = reader.info().sample_format.bytes();
        let channels = reader.info().total_channels() as usize;
        let mut planes = vec![vec![0u8; w * h * sample_bytes]; channels];
        reader
            .read_bands(&mut |ch, start_row, rows, bytes| {
                assert_eq!(
                    bytes.len(),
                    rows as usize * w * sample_bytes,
                    "band (ch {ch}, row {start_row}) has the wrong length"
                );
                let off = start_row as usize * w * sample_bytes;
                planes[ch as usize][off..off + bytes.len()].copy_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        planes
    }

    #[test]
    fn palette_input_is_expanded_to_rgb() {
        let dir = tmpdir("palette");
        let src = dir.join("pal.tif");
        write_palette_fixture(&src);

        let mut reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        assert_eq!(reader.info().alpha_channels, 0);
        // ColorMap entries are 16-bit, so RGB16 is the lossless target.
        assert_eq!(reader.info().sample_format, SampleFormat::U16);
        assert_eq!((reader.info().width, reader.info().height), (4, 2));

        // The decoded pixels must be the ColorMap lookups, not the indices.
        let planes = collect_planes(&mut reader);
        for pixel in 0..8usize {
            let expected = palette_expected(pixel);
            for (ch, want) in expected.iter().enumerate() {
                let b = pixel * 2;
                let got = u16::from_ne_bytes([planes[ch][b], planes[ch][b + 1]]);
                assert_eq!(
                    got, *want,
                    "channel {ch} pixel {pixel}: expected {want}, got {got}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn planar_storage_is_accepted() {
        let dir = tmpdir("planar");
        let src = dir.join("planar.tif");
        write_planar_fixture(&src);

        let mut reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        assert_eq!(reader.info().alpha_channels, 0);
        assert_eq!(reader.info().sample_format, SampleFormat::U8);
        assert_eq!((reader.info().width, reader.info().height), (4, 4));

        // Every pixel of every plane must come back where it was written:
        // the right channel, at the right row within that channel.
        let planes = collect_planes(&mut reader);
        for (c, plane) in planes.iter().enumerate() {
            for (p, got) in plane.iter().enumerate() {
                assert_eq!(
                    *got,
                    planar_expected(c, p),
                    "channel {c} pixel {p} mismatch"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn planar_bands_cover_every_row_of_every_channel_once() {
        let dir = tmpdir("planar-cover");
        let src = dir.join("planar.tif");
        write_planar_fixture(&src);

        let mut reader = TiffReader::open(&src).unwrap();
        let mut seen = vec![vec![0u32; 4]; 3];
        reader
            .read_bands(&mut |ch, start_row, rows, _| {
                for r in 0..rows as u64 {
                    seen[ch as usize][(start_row + r) as usize] += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            seen,
            vec![vec![1u32; 4]; 3],
            "each row emitted exactly once"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ycbcr_input_is_converted_to_rgb() {
        let dir = tmpdir("ycbcr");
        let src = dir.join("ycbcr.tif");
        write_ycbcr_fixture(&src);

        let mut reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        assert_eq!(reader.info().alpha_channels, 0);
        assert_eq!(reader.info().sample_format, SampleFormat::U8);

        // The fixture's four pixels are red, green, blue and white encoded
        // as YCbCr; inverting the transform must land back on them. The
        // tolerance covers only the two roundings (RGB -> YCbCr in the
        // fixture, YCbCr -> RGB in the reader); untransformed samples would
        // be off by hundreds.
        let expected = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255]];
        let planes = collect_planes(&mut reader);
        for (pixel, want) in expected.iter().enumerate() {
            for ch in 0..3usize {
                let got = planes[ch][pixel];
                let diff = i32::from(got) - i32::from(want[ch]);
                assert!(
                    diff.abs() <= 2,
                    "channel {ch} pixel {pixel}: expected ~{}, got {got}",
                    want[ch]
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cmyk_and_lab_are_still_rejected_with_a_reason() {
        let dir = tmpdir("cmyk-reason");

        for (name, write, kind) in [
            ("cmyk.tif", write_cmyk_fixture as fn(&Path), "cmyk"),
            ("lab.tif", write_lab_fixture as fn(&Path), "lab"),
        ] {
            let src = dir.join(name);
            write(&src);
            let msg = TiffReader::open(&src).unwrap_err().to_string();
            assert!(
                msg.contains("colour management") || msg.contains("color management"),
                "rejection should explain why {kind} is refused, got: {msg}"
            );
            assert!(
                msg.to_lowercase().contains(kind),
                "rejection should name the colour type, got: {msg}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Write a minimal separate-planes RGB8 TIFF that is *tiled* rather
    /// than stripped: 4x4 pixels in 2x2 tiles, so each plane holds four
    /// tiles and the file holds twelve in plane-major order. Test-only.
    fn write_planar_tiled_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        let mut offsets = Vec::new();
        for c in 0..3usize {
            for tr in 0..2usize {
                for tc in 0..2usize {
                    let mut tile = Vec::with_capacity(4);
                    for row in 0..2usize {
                        for col in 0..2usize {
                            let (y, x) = (tr * 2 + row, tc * 2 + col);
                            tile.push(planar_expected(c, y * 4 + x));
                        }
                    }
                    offsets.push(dir.write_data(&tile[..]).unwrap() as u32);
                }
            }
        }

        dir.write_tag(Tag::ImageWidth, 4u32).unwrap();
        dir.write_tag(Tag::ImageLength, 4u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16, 8, 8][..])
            .unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 3u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 2u16).unwrap(); // RGB
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 2u16).unwrap(); // separate
        dir.write_tag(Tag::TileWidth, 2u32).unwrap();
        dir.write_tag(Tag::TileLength, 2u32).unwrap();
        dir.write_tag(Tag::TileOffsets, &offsets[..]).unwrap();
        dir.write_tag(Tag::TileByteCounts, &[4u32; 12][..]).unwrap();
        dir.write_tag(Tag::SampleFormat, &[1u16, 1, 1][..]).unwrap();
        dir.finish().unwrap();
    }

    #[test]
    fn planar_tiled_storage_is_accepted() {
        let dir = tmpdir("planar-tiled");
        let src = dir.join("planar-tiled.tif");
        write_planar_tiled_fixture(&src);

        let mut reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        assert_eq!((reader.info().width, reader.info().height), (4, 4));

        let planes = collect_planes(&mut reader);
        for (c, plane) in planes.iter().enumerate() {
            for (p, got) in plane.iter().enumerate() {
                assert_eq!(
                    *got,
                    planar_expected(c, p),
                    "channel {c} pixel {p} mismatch"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn palette_ignores_planar_configuration() {
        // A palette image is single-sample, so PlanarConfiguration = 2 says
        // nothing about it — the reader must not mistake its three *output*
        // channels for three stored planes.
        let dir = tmpdir("palette-planar");
        let src = dir.join("pal.tif");
        write_palette_fixture_with(&src, 2);

        let mut reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        let planes = collect_planes(&mut reader);
        for pixel in 0..8usize {
            let expected = palette_expected(pixel);
            for (ch, want) in expected.iter().enumerate() {
                let b = pixel * 2;
                let got = u16::from_ne_bytes([planes[ch][b], planes[ch][b + 1]]);
                assert_eq!(got, *want, "channel {ch} pixel {pixel}");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
