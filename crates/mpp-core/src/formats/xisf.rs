//! Minimal XISF reader (PixInsight's native format).
//!
//! Supports the monolithic XISF subset PixInsight emits for processed
//! images: one `<Image>` element, planar pixel storage, little-endian
//! Float32 samples, uncompressed attachment data block. Anything else
//! errors clearly; compression and other sample formats come later.
//!
//! Spec: <https://pixinsight.com/doc/docs/XISF-1.0-spec/XISF-1.0-spec.html>

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use quick_xml::events::Event;

use super::xisf_block::{BlockAttrs, BlockLocation, ByteOrder, DataBlock, try_zeroed};
use super::{FitsKeyword, PropertyValue, SampleFormat, XisfProperty};
use crate::header::Header;
use crate::{Error, Result};

const SIGNATURE: &[u8; 8] = b"XISF0100";
/// Prefix of PixInsight's private solution lookup-table cache.
const GRID_CACHE_PREFIX: &str = "PCL:AstrometricSolution:Grid:";

/// Parsed metadata for the image block of an XISF file.
#[derive(Debug, Clone)]
pub struct XisfHeader {
    /// Image width in pixels.
    pub width: u64,
    /// Image height in pixels.
    pub height: u64,
    /// Number of channels (planar planes).
    pub channels: u64,
    /// Per-sample data type.
    pub sample_format: SampleFormat,
    /// Byte offset of the image's data block within the file (0 for an
    /// embedded block).
    pub data_offset: u64,
    /// Stored (possibly compressed) byte size of the image's data block.
    pub data_size: u64,
    /// The image's data block: location, byte order, compression, checksum.
    pub block: DataBlock,
    /// Pixel storage model (`pixelStorage`, default planar).
    pub pixel_storage: PixelStorage,
    /// Color space (`colorSpace`, default gray).
    pub color_space: ColorSpace,
    /// Declared representable range (`bounds`), if any; see
    /// [`XisfHeader::effective_bounds`].
    pub bounds: Option<(f64, f64)>,
    /// `offset` attribute: subtracted from every sample (default 0).
    pub offset: f64,
    /// The image's `id` attribute, if any.
    pub image_id: Option<String>,
    /// Number of `<Image>` elements in the unit.
    pub image_count: usize,
    /// The `id` of every `<Image>` in the unit, in order.
    pub image_ids: Vec<Option<String>>,
    /// All `<FITSKeyword>` cards of the image element, in header order.
    pub fits_keywords: Vec<FitsKeyword>,
    /// All `<Property>` elements in the header (image-level and file-level).
    /// Attachment-located f64 vectors/matrices are resolved by [`XisfImage::open`].
    pub properties: Vec<XisfProperty>,
    /// Data blocks of attachment-located vector/matrix properties, by index
    /// into `properties`, still to be read and decoded.
    pub(crate) property_blocks: Vec<(usize, DataBlock)>,
}

impl XisfHeader {
    /// The representable sample range: `bounds` when declared, else the
    /// format's default — `[0, 1]` for floating point (the spec requires
    /// `bounds` there; PixInsight writes `0:1`), `[0, 2^k − 1]` for k-bit
    /// unsigned integers.
    pub fn effective_bounds(&self) -> (f64, f64) {
        self.bounds.unwrap_or(match self.sample_format {
            SampleFormat::UInt8 => (0.0, 255.0),
            SampleFormat::UInt16 => (0.0, 65535.0),
            SampleFormat::UInt32 => (0.0, 4294967295.0),
            SampleFormat::UInt64 => (0.0, 18446744073709551615.0),
            _ => (0.0, 1.0),
        })
    }
}

/// XISF pixel storage model (`pixelStorage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelStorage {
    /// Each channel stored as a contiguous plane (the default).
    Planar,
    /// Channels interleaved pixel by pixel.
    Normal,
}

/// XISF color space (`colorSpace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    /// Grayscale (the default).
    Gray,
    /// RGB.
    Rgb,
    /// CIE L*a*b*.
    CieLab,
}

/// A parsed `<Image>` element, before the selected one becomes the header.
struct ImageElement {
    width: u64,
    height: u64,
    channels: u64,
    sample_format: SampleFormat,
    pixel_storage: PixelStorage,
    color_space: ColorSpace,
    bounds: Option<(f64, f64)>,
    offset: f64,
    id: Option<String>,
    location: String,
    byte_order: Option<String>,
    compression: Option<String>,
    subblocks: Option<String>,
    checksum: Option<String>,
    /// The `<Data>` child of an embedded image block.
    data: Option<EmbeddedData>,
    fits_keywords: Vec<FitsKeyword>,
}

impl ImageElement {
    fn parse(path: &Path, e: &quick_xml::events::BytesStart) -> Result<ImageElement> {
        let err = |m: String| Error::format(path, m);
        let geometry = attr_string(path, e, b"geometry")?
            .ok_or_else(|| err("<Image> missing geometry".into()))?;
        let dims: Vec<u64> = geometry
            .split(':')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect();
        let (width, height, channels) = match dims.as_slice() {
            [w, h, c] if *w > 0 && *h > 0 && *c > 0 => (*w, *h, *c),
            _ => {
                return Err(err(format!(
                    "unsupported geometry '{geometry}' (need W:H:C)"
                )));
            }
        };
        let sample_format_s = attr_string(path, e, b"sampleFormat")?
            .ok_or_else(|| err("<Image> missing sampleFormat".into()))?;
        let sample_format = SampleFormat::parse(sample_format_s.trim())
            .ok_or_else(|| err(format!("unknown sampleFormat '{sample_format_s}'")))?;
        let pixel_storage = match attr_string(path, e, b"pixelStorage")?
            .as_deref()
            .map(str::trim)
        {
            None | Some("Planar") => PixelStorage::Planar,
            Some("Normal") => PixelStorage::Normal,
            Some(other) => return Err(err(format!("unsupported pixelStorage '{other}'"))),
        };
        let color_space = match attr_string(path, e, b"colorSpace")?
            .as_deref()
            .map(str::trim)
        {
            None | Some("Gray") => ColorSpace::Gray,
            Some("RGB") => ColorSpace::Rgb,
            Some("CIELab") => ColorSpace::CieLab,
            Some(other) => return Err(err(format!("unsupported colorSpace '{other}'"))),
        };
        let bounds = match attr_string(path, e, b"bounds")? {
            None => None,
            Some(b) => Some(
                b.split_once(':')
                    .and_then(|(lo, hi)| Some((lo.trim().parse().ok()?, hi.trim().parse().ok()?)))
                    .filter(|(lo, hi): &(f64, f64)| lo.is_finite() && hi.is_finite() && hi > lo)
                    .ok_or_else(|| err(format!("bad bounds '{b}' (need lower:upper)")))?,
            ),
        };
        let offset = match attr_string(path, e, b"offset")? {
            None => 0.0,
            Some(o) => o
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| err(format!("bad offset '{o}'")))?,
        };
        Ok(ImageElement {
            width,
            height,
            channels,
            sample_format,
            pixel_storage,
            color_space,
            bounds,
            offset,
            id: attr_string(path, e, b"id")?,
            location: attr_string(path, e, b"location")?
                .ok_or_else(|| err("<Image> missing location".into()))?,
            byte_order: attr_string(path, e, b"byteOrder")?,
            compression: attr_string(path, e, b"compression")?,
            subblocks: attr_string(path, e, b"subblocks")?,
            checksum: attr_string(path, e, b"checksum")?,
            data: None,
            fits_keywords: Vec::new(),
        })
    }

    /// The header for this (selected) image of a unit of `image_count`.
    fn into_header(
        self,
        path: &Path,
        image_count: usize,
        image_ids: Vec<Option<String>>,
    ) -> Result<XisfHeader> {
        let embedded = self.location.trim() == "embedded";
        let (compression, subblocks, text, text_encoding) = match (&self.data, embedded) {
            (Some(d), true) => (
                d.compression.as_deref(),
                d.subblocks.as_deref(),
                Some(d.text.as_str()),
                d.encoding.as_deref(),
            ),
            _ => (
                self.compression.as_deref(),
                self.subblocks.as_deref(),
                None,
                None,
            ),
        };
        let block = DataBlock::parse(&BlockAttrs {
            location: &self.location,
            byte_order: self.byte_order.as_deref(),
            compression,
            subblocks,
            checksum: self.checksum.as_deref(),
            text,
            text_encoding,
        })
        .map_err(|e| Error::format(path, format!("image block: {e}")))?;
        let (data_offset, data_size) = match &block.location {
            BlockLocation::Attachment { offset, size } => (*offset, *size),
            BlockLocation::Inline(b) => (0, b.len() as u64),
        };
        Ok(XisfHeader {
            width: self.width,
            height: self.height,
            channels: self.channels,
            sample_format: self.sample_format,
            data_offset,
            data_size,
            block,
            pixel_storage: self.pixel_storage,
            color_space: self.color_space,
            bounds: self.bounds,
            offset: self.offset,
            image_id: self.id,
            image_count,
            image_ids,
            fits_keywords: self.fits_keywords,
            properties: Vec::new(),
            property_blocks: Vec::new(),
        })
    }
}

/// Options for opening an XISF unit.
#[derive(Debug, Clone)]
pub struct XisfOpenOptions {
    /// Which `<Image>` of the unit to read (0-based; default the first).
    pub image_index: usize,
    /// Verify declared data-block checksums before using a block (default
    /// `true`). A mismatch is an error; turn this off only to salvage data.
    pub verify_checksums: bool,
}

impl Default for XisfOpenOptions {
    fn default() -> Self {
        XisfOpenOptions {
            image_index: 0,
            verify_checksums: true,
        }
    }
}

/// A memory-mapped, read-only XISF image exposing planar f32 channel data.
///
/// An uncompressed, attached, planar, little-endian Float32 image in the
/// default `0:1` range is served zero-copy from the mapping
/// ([`Self::is_mapped`]). Any other image a baseline decoder reads is
/// decoded on request: [`Self::decode_into`] into a caller buffer (e.g. a
/// mapped cache file), or [`Self::load`] into memory, after which the
/// channel/row accessors work as for a mapped image.
pub struct XisfImage {
    path: PathBuf,
    mmap: Mmap,
    header: XisfHeader,
    pixels: Pixels,
    verify_checksums: bool,
}

/// Where an [`XisfImage`]'s planar f32 samples are.
enum Pixels {
    /// In the file mapping, at `header.data_offset`.
    Mapped,
    /// Decoded into memory by [`XisfImage::load`].
    Decoded(Vec<f32>),
    /// Not decoded yet.
    Pending,
}

impl XisfImage {
    /// Open and validate a monolithic XISF file, resolving attachment-located
    /// vector/matrix properties (e.g. astrometric solution data) so
    /// downstream code sees decoded values. Pixels are not decoded here (see
    /// the type docs); a mapped image's block checksum is verified now.
    /// Errors on malformed headers and on images no decoder here reads
    /// (UInt64 or complex samples, CIE L*a*b*, distributed units).
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, &XisfOpenOptions::default())
    }

    /// [`Self::open`] with explicit options.
    pub fn open_with(path: &Path, opts: &XisfOpenOptions) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        // SAFETY: read-only map; we accept that truncation by another process
        // during a run is undefined, as is conventional for mmap-based readers.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| Error::io(path, e))?;
        let mut header = parse_header(path, &mmap, opts.verify_checksums, opts.image_index)?;

        // Resolve attachment-located numeric vectors/matrices (small blocks —
        // e.g. astrometric solution data) so downstream code sees decoded values.
        for (i, block) in std::mem::take(&mut header.property_blocks) {
            let prop = &mut header.properties[i];
            let value = block
                .stored(&mmap)
                .map_err(|e| Error::format(path, format!("property {}: {e}", prop.id)))
                .and_then(|stored| {
                    decode_property_block(path, prop, &block, stored, opts.verify_checksums)
                });
            lenient(prop, value)?;
        }

        if let BlockLocation::Attachment { offset, size } = header.block.location {
            offset
                .checked_add(size)
                .filter(|&end| end <= mmap.len() as u64)
                .ok_or_else(|| Error::format(path, "data block extends past end of file"))?;
        }
        if let Some(why) = super::xisf_pixels::unsupported(&header) {
            return Err(Error::format(path, why));
        }
        // The declared (uncompressed) size must be exactly the geometry's,
        // checked before anything is allocated from it.
        let expected = header
            .width
            .checked_mul(header.height)
            .and_then(|n| n.checked_mul(header.channels))
            .and_then(|n| n.checked_mul(header.sample_format.bytes_per_sample() as u64));
        if expected != Some(header.block.uncompressed_len()) {
            return Err(Error::format(
                path,
                format!(
                    "data block size {} does not match geometry {}x{}x{} {:?}",
                    header.block.uncompressed_len(),
                    header.width,
                    header.height,
                    header.channels,
                    header.sample_format
                ),
            ));
        }

        let mapped = header.block.compression.is_none()
            && matches!(header.block.location, BlockLocation::Attachment { .. })
            && header.pixel_storage == PixelStorage::Planar
            && header.block.byte_order == ByteOrder::Little
            && header.sample_format == SampleFormat::Float32
            && header.effective_bounds() == (0.0, 1.0)
            && header.offset == 0.0
            // PixInsight aligns attachments to 4096 bytes; an unaligned one
            // cannot back a zero-copy f32 view, so it is decoded instead.
            && header.data_offset % 4 == 0;
        let pixels = if mapped {
            if opts.verify_checksums
                && let Some(c) = &header.block.checksum
            {
                let stored = header
                    .block
                    .stored(&mmap)
                    .map_err(|e| Error::format(path, e))?;
                c.verify(stored)
                    .map_err(|e| Error::format(path, format!("image block: {e}")))?;
            }
            Pixels::Mapped
        } else {
            Pixels::Pending
        };

        Ok(Self {
            path: path.to_path_buf(),
            mmap,
            header,
            pixels,
            verify_checksums: opts.verify_checksums,
        })
    }

    /// The file this image was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parsed header metadata (geometry, FITS keywords, properties).
    pub fn header(&self) -> &XisfHeader {
        &self.header
    }

    /// Image width in pixels.
    pub fn width(&self) -> u64 {
        self.header.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> u64 {
        self.header.height
    }

    /// Number of channels (planar planes).
    pub fn channels(&self) -> u64 {
        self.header.channels
    }

    /// Whether the pixels are served zero-copy from the file mapping.
    pub fn is_mapped(&self) -> bool {
        matches!(self.pixels, Pixels::Mapped)
    }

    /// Decode the whole image into `out` (`width·height·channels` samples,
    /// planar, normalized to `[0, 1]`): verify the block checksum (unless
    /// disabled at open), decompress, honour byte order, storage model,
    /// sample format, `bounds` and `offset`. Works for every image, mapped
    /// or not.
    pub fn decode_into(&self, out: &mut [f32]) -> Result<()> {
        let h = &self.header;
        let n = (h.width * h.height * h.channels) as usize;
        if out.len() != n {
            return Err(Error::invalid(format!(
                "decode buffer holds {} samples, the image has {n}",
                out.len()
            )));
        }
        match &self.pixels {
            Pixels::Mapped => {
                let plane = (h.width * h.height) as usize;
                for c in 0..h.channels {
                    let c = c as usize;
                    out[c * plane..(c + 1) * plane].copy_from_slice(self.channel(c as u64));
                }
                return Ok(());
            }
            Pixels::Decoded(v) => {
                out.copy_from_slice(v);
                return Ok(());
            }
            Pixels::Pending => {}
        }
        let err = |e: String| Error::format(&self.path, format!("image block: {e}"));
        let verify = self.verify_checksums;
        let Some(c) = &h.block.compression else {
            // Uncompressed but not mappable: convert straight from the file.
            let stored = h.block.stored(&self.mmap).map_err(err)?;
            if verify && let Some(sum) = &h.block.checksum {
                sum.verify(stored).map_err(err)?;
            }
            return super::xisf_pixels::to_planar_f32(h, stored, out).map_err(err);
        };
        let len = usize::try_from(h.block.uncompressed_len())
            .map_err(|_| err("too large for this platform".into()))?;
        let mut scratch = match c.item_size {
            Some(_) => Some(try_zeroed::<u8>(len).map_err(err)?),
            None => None,
        };
        if super::xisf_pixels::is_identity_layout(h) {
            // The decompressed bytes *are* the planar f32 output: decode in
            // place, holding at most one scratch buffer besides `out`.
            let bytes: &mut [u8] = bytemuck::cast_slice_mut(out);
            return h
                .block
                .decode_into(&self.mmap, verify, bytes, scratch.as_deref_mut())
                .map_err(err);
        }
        let mut raw = try_zeroed::<u8>(len).map_err(err)?;
        h.block
            .decode_into(&self.mmap, verify, &mut raw, scratch.as_deref_mut())
            .map_err(err)?;
        drop(scratch);
        super::xisf_pixels::to_planar_f32(h, &raw, out).map_err(err)
    }

    /// Decode the pixels into memory if they are not mapped (a no-op when
    /// they are), so [`Self::channel`], [`Self::row`] and
    /// [`super::PlanarRows`] work.
    pub fn load(&mut self) -> Result<()> {
        if matches!(self.pixels, Pixels::Pending) {
            let h = &self.header;
            let mut v = try_zeroed::<f32>((h.width * h.height * h.channels) as usize)
                .map_err(|e| Error::format(&self.path, e))?;
            self.decode_into(&mut v)?;
            self.pixels = Pixels::Decoded(v);
        }
        Ok(())
    }

    /// One full channel plane (row-major, top-down): zero-copy when mapped.
    ///
    /// # Panics
    /// If the pixels are neither mapped nor loaded ([`Self::load`]).
    pub fn channel(&self, c: u64) -> &[f32] {
        assert!(c < self.header.channels, "channel {c} out of range");
        let plane = (self.header.width * self.header.height) as usize;
        match &self.pixels {
            Pixels::Mapped => {
                let start = self.header.data_offset as usize + c as usize * plane * 4;
                bytemuck::cast_slice(&self.mmap[start..start + plane * 4])
            }
            Pixels::Decoded(v) => &v[c as usize * plane..(c as usize + 1) * plane],
            Pixels::Pending => panic!("XISF pixels not loaded: call load() or decode_into()"),
        }
    }

    /// One row of one channel (see [`Self::channel`]).
    pub fn row(&self, c: u64, y: u64) -> &[f32] {
        assert!(y < self.header.height, "row {y} out of range");
        let w = self.header.width as usize;
        let plane = self.channel(c);
        &plane[y as usize * w..(y as usize + 1) * w]
    }

    /// Advise the OS that access will be sequential (large streaming scans).
    /// A no-op where `madvise(2)` is unavailable (e.g. Windows).
    pub fn advise_sequential(&self) {
        #[cfg(unix)]
        let _ = self.mmap.advise(memmap2::Advice::Sequential);
    }
}

impl std::fmt::Debug for XisfImage {
    /// The file, geometry and pixel state; never the pixels or the mapping.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = &self.header;
        f.debug_struct("XisfImage")
            .field("path", &self.path)
            .field("geometry", &(h.width, h.height, h.channels))
            .field("sample_format", &h.sample_format)
            .field(
                "pixels",
                &match self.pixels {
                    Pixels::Mapped => "mapped",
                    Pixels::Decoded(_) => "decoded",
                    Pixels::Pending => "pending",
                },
            )
            .finish()
    }
}

impl super::PlanarRows<f32> for XisfImage {
    fn width(&self) -> u64 {
        self.width()
    }

    fn height(&self) -> u64 {
        self.height()
    }

    fn channels(&self) -> u64 {
        self.channels()
    }

    fn row(&self, c: u64, y: u64) -> &[f32] {
        self.row(c, y)
    }
}

/// The FITS keywords and properties of the XISF file at `path`, without its
/// pixels.
///
/// Reads only the XML header and the attachment blocks of vector/matrix
/// properties, so it is cheap on multi-GB files and accepts every sample
/// format (unlike [`XisfImage::open`], which reads Float32 only). The other
/// limits of the module docs still apply: compressed or non-planar images
/// are refused.
///
/// PixInsight's private `PCL:AstrometricSolution:Grid:*` properties, a
/// lookup-table cache of the astrometric solution (about 20 MB on a 16 MP
/// frame), are dropped without reading their attachments: no decoder uses
/// them, and a [`Header`] is meant to travel. [`XisfImage::open`] keeps them.
/// The header's image size is the `<Image geometry>`.
pub fn read_header(path: &Path) -> Result<Header> {
    read_header_with(path, &XisfOpenOptions::default())
}

/// [`read_header`] with explicit options.
pub fn read_header_with(path: &Path, opts: &XisfOpenOptions) -> Result<Header> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let io = |e| Error::io(path, e);
    let mut lead = [0u8; 16];
    if file.read_exact(&mut lead).is_err() || &lead[0..8] != SIGNATURE {
        return Err(Error::format(
            path,
            "not a monolithic XISF file (bad signature)",
        ));
    }
    let header_len = u32::from_le_bytes(lead[8..12].try_into().unwrap()) as usize;
    let file_len = file.metadata().map_err(io)?.len();
    if file_len < 16 + header_len as u64 {
        return Err(Error::format(path, "truncated XISF header"));
    }
    let mut bytes = lead.to_vec();
    bytes.resize(16 + header_len, 0);
    file.read_exact(&mut bytes[16..]).map_err(io)?;
    let mut header = parse_header(path, &bytes, opts.verify_checksums, opts.image_index)?;

    for (i, block) in std::mem::take(&mut header.property_blocks) {
        let prop = &mut header.properties[i];
        // PixInsight's private lookup-table cache of the solution: tens of MB
        // that no decoder reads (dropped below), so never read it either.
        if prop.id.starts_with(GRID_CACHE_PREFIX) {
            continue;
        }
        let Some((off, size)) = prop.location else {
            continue;
        };
        let value = if off.checked_add(size).is_none_or(|end| end > file_len) {
            Err(Error::format(
                path,
                format!("property {}: attachment out of bounds", prop.id),
            ))
        } else {
            let mut data = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(off)).map_err(io)?;
            file.read_exact(&mut data).map_err(io)?;
            decode_property_block(path, prop, &block, &data, opts.verify_checksums)
        };
        lenient(prop, value)?;
    }
    // ...and a Header travels between nodes.
    header
        .properties
        .retain(|p| !p.id.starts_with(GRID_CACHE_PREFIX));
    Ok(Header::from(&header))
}

fn parse_header(path: &Path, mmap: &[u8], verify: bool, image_index: usize) -> Result<XisfHeader> {
    if mmap.len() < 16 || &mmap[0..8] != SIGNATURE {
        return Err(Error::format(
            path,
            "not a monolithic XISF file (bad signature)",
        ));
    }
    let header_len = u32::from_le_bytes(mmap[8..12].try_into().unwrap()) as usize;
    if mmap.len() < 16 + header_len {
        return Err(Error::format(path, "truncated XISF header"));
    }
    let xml = &mmap[16..16 + header_len];

    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut images: Vec<ImageElement> = Vec::new();
    // Index into `images` of the <Image> being read.
    let mut current: Option<usize> = None;
    // Whether the current image's <Data> (embedded pixel block) is open.
    let mut image_data_open = false;
    // Depth inside a <Thumbnail> subtree, all of which is skipped.
    let mut skip = 0u32;
    // Every property, unparsed, with its owning image (None: file level),
    // in document order. Only the file-level ones and the selected image's
    // are decoded, after the walk: another image's bad block must not stop
    // this one from opening, nor cost its decode.
    let mut properties: Vec<(Option<usize>, PendingProperty, String)> = Vec::new();
    // <Property> currently being read: (partial parse, accumulated text,
    // nesting depth of unrelated child elements to skip).
    let mut pending: Option<(PendingProperty, String, u32)> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if skip > 0 {
                    skip += 1;
                } else if let Some((partial, _, depth)) = pending.as_mut() {
                    if *depth == 0 && name == b"Data" {
                        partial.data = Some(EmbeddedData::parse(path, &e)?);
                    }
                    *depth += 1; // nested element inside <Property> (e.g. <Data>)
                } else {
                    match name {
                        b"Property" => {
                            pending = Some((parse_property_start(path, &e)?, String::new(), 0));
                        }
                        b"Image" => {
                            images.push(ImageElement::parse(path, &e)?);
                            current = Some(images.len() - 1);
                        }
                        b"Thumbnail" => skip = 1,
                        b"Data" if current.is_some() => {
                            images[current.unwrap()].data = Some(EmbeddedData::parse(path, &e)?);
                            image_data_open = true;
                        }
                        b"FITSKeyword" if current.is_some() => {
                            images[current.unwrap()]
                                .fits_keywords
                                .push(parse_fits_keyword(path, &e)?);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.local_name();
                let name = name.as_ref();
                if skip > 0 {
                } else if let Some((partial, _, depth)) = pending.as_mut() {
                    // A self-closing <Data/> is an empty embedded block.
                    if *depth == 0 && name == b"Data" {
                        partial.data = Some(EmbeddedData::parse(path, &e)?);
                    }
                } else {
                    match name {
                        b"Property" => {
                            let partial = parse_property_start(path, &e)?;
                            properties.push((current, partial, String::new()));
                        }
                        b"Image" => images.push(ImageElement::parse(path, &e)?),
                        b"Data" if current.is_some() => {
                            images[current.unwrap()].data = Some(EmbeddedData::parse(path, &e)?);
                        }
                        b"FITSKeyword" if current.is_some() => {
                            images[current.unwrap()]
                                .fits_keywords
                                .push(parse_fits_keyword(path, &e)?);
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if skip > 0 {
                    // inside a <Thumbnail>
                } else if let Some((partial, text, depth)) = pending.as_mut() {
                    let s = t
                        .unescape()
                        .map_err(|e| Error::format(path, format!("bad XML text: {e}")))?;
                    match (*depth, partial.data.as_mut()) {
                        (0, _) => text.push_str(&s),
                        (1, Some(data)) => data.text.push_str(&s),
                        _ => {}
                    }
                } else if image_data_open && let Some(data) = images[current.unwrap()].data.as_mut()
                {
                    let s = t
                        .unescape()
                        .map_err(|e| Error::format(path, format!("bad XML text: {e}")))?;
                    data.text.push_str(&s);
                }
            }
            Ok(Event::CData(t)) => {
                if skip > 0 {
                } else if let Some((partial, text, depth)) = pending.as_mut() {
                    let s = String::from_utf8_lossy(&t);
                    match (*depth, partial.data.as_mut()) {
                        (0, _) => text.push_str(&s),
                        (1, Some(data)) => data.text.push_str(&s),
                        _ => {}
                    }
                } else if image_data_open && let Some(data) = images[current.unwrap()].data.as_mut()
                {
                    data.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::End(e)) => {
                if skip > 0 {
                    skip -= 1;
                } else if let Some((_, _, depth)) = pending.as_mut() {
                    if *depth > 0 {
                        *depth -= 1;
                    } else {
                        let (partial, text, _) = pending.take().unwrap();
                        properties.push((current, partial, text));
                    }
                } else {
                    match e.local_name().as_ref() {
                        b"Data" => image_data_open = false,
                        b"Image" => current = None,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::format(path, format!("XML parse error: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    let image_count = images.len();
    if image_count == 0 {
        return Err(Error::format(path, "no <Image> element in XISF header"));
    }
    if image_index >= image_count {
        return Err(Error::format(
            path,
            format!("requested image {image_index}, but the unit has {image_count} image(s)"),
        ));
    }
    let image_ids = images.iter().map(|i| i.id.clone()).collect();
    let selected = images.swap_remove(image_index);
    let mut header = selected.into_header(path, image_count, image_ids)?;
    for (owner, partial, text) in properties {
        if owner.is_none_or(|o| o == image_index) {
            let (prop, block) = finish_property(path, partial, &text, verify)?;
            if let Some(block) = block {
                header
                    .property_blocks
                    .push((header.properties.len(), block));
            }
            header.properties.push(prop);
        }
    }
    Ok(header)
}

/// The `<Data>` child of an `embedded` property: its block attributes and
/// encoded text.
struct EmbeddedData {
    encoding: Option<String>,
    compression: Option<String>,
    subblocks: Option<String>,
    text: String,
}

impl EmbeddedData {
    fn parse(path: &Path, e: &quick_xml::events::BytesStart) -> Result<EmbeddedData> {
        Ok(EmbeddedData {
            encoding: attr_string(path, e, b"encoding")?,
            compression: attr_string(path, e, b"compression")?,
            subblocks: attr_string(path, e, b"subblocks")?,
            text: String::new(),
        })
    }
}

/// `<Property>` attributes captured before its element text is known.
struct PendingProperty {
    id: String,
    type_: String,
    value_attr: Option<String>,
    location: Option<String>,
    length: Option<u64>,
    rows: Option<u32>,
    cols: Option<u32>,
    byte_order: Option<String>,
    compression: Option<String>,
    subblocks: Option<String>,
    checksum: Option<String>,
    data: Option<EmbeddedData>,
}

fn parse_property_start(path: &Path, e: &quick_xml::events::BytesStart) -> Result<PendingProperty> {
    fn parse_num<T: std::str::FromStr>(s: Option<String>) -> Option<T> {
        s.and_then(|s| s.parse().ok())
    }
    Ok(PendingProperty {
        id: attr_string(path, e, b"id")?.unwrap_or_default(),
        type_: attr_string(path, e, b"type")?.unwrap_or_default(),
        value_attr: attr_string(path, e, b"value")?,
        location: attr_string(path, e, b"location")?,
        length: parse_num(attr_string(path, e, b"length")?),
        rows: parse_num(attr_string(path, e, b"rows")?),
        cols: parse_num(attr_string(path, e, b"columns")?),
        byte_order: attr_string(path, e, b"byteOrder")?,
        compression: attr_string(path, e, b"compression")?,
        subblocks: attr_string(path, e, b"subblocks")?,
        checksum: attr_string(path, e, b"checksum")?,
        data: None,
    })
}

/// Element type of an XISF numeric vector/matrix.
#[derive(Clone, Copy)]
enum NumKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

/// `(element kind, element byte width, is_matrix)` for an XISF numeric
/// vector/matrix type name (`I32Vector`, `F64Matrix`, …); `None` otherwise.
fn numeric_array_type(type_: &str) -> Option<(NumKind, usize, bool)> {
    let (base, is_matrix) = match (type_.strip_suffix("Vector"), type_.strip_suffix("Matrix")) {
        (Some(b), _) => (b, false),
        (None, Some(b)) => (b, true),
        (None, None) => return None,
    };
    let (kind, width) = match base {
        "I8" => (NumKind::I8, 1),
        "UI8" => (NumKind::U8, 1),
        "I16" => (NumKind::I16, 2),
        "UI16" => (NumKind::U16, 2),
        "I32" => (NumKind::I32, 4),
        "UI32" => (NumKind::U32, 4),
        "I64" => (NumKind::I64, 8),
        "UI64" => (NumKind::U64, 8),
        "F32" => (NumKind::F32, 4),
        "F64" => (NumKind::F64, 8),
        _ => return None,
    };
    Some((kind, width, is_matrix))
}

/// Decode little-endian elements of `kind` into f64s (integers convert
/// exactly up to 2^53; `bytes.len()` must be a multiple of the width).
fn decode_numbers(kind: NumKind, bytes: &[u8]) -> Vec<f64> {
    macro_rules! conv {
        ($t:ty) => {
            bytes
                .chunks_exact(std::mem::size_of::<$t>())
                .map(|c| <$t>::from_le_bytes(c.try_into().unwrap()) as f64)
                .collect()
        };
    }
    match kind {
        NumKind::I8 => conv!(i8),
        NumKind::U8 => conv!(u8),
        NumKind::I16 => conv!(i16),
        NumKind::U16 => conv!(u16),
        NumKind::I32 => conv!(i32),
        NumKind::U32 => conv!(u32),
        NumKind::I64 => conv!(i64),
        NumKind::U64 => conv!(u64),
        NumKind::F32 => conv!(f32),
        NumKind::F64 => conv!(f64),
    }
}

/// Decode a completed `<Property>` from its attributes and element text.
///
/// Shapes handled (all three occur in PixInsight files):
/// - scalars via the `value` attribute (or element text as fallback),
/// - String/TimePoint via element text or `value`,
/// - every numeric vector/matrix type (`F64Vector`, `I32Vector`, `F32Matrix`,
///   …, decoded to f64) via `location="inline:base64|inline:hex"` (payload is
///   the element text) or `location="attachment:offset:size"` (value left
///   with empty data; resolved from the file by the caller).
///
/// Types we do not decode become [`PropertyValue::Unread`] (or `Str` when a
/// plain `value` attribute is present).
fn finish_property(
    path: &Path,
    p: PendingProperty,
    text: &str,
    verify: bool,
) -> Result<(XisfProperty, Option<DataBlock>)> {
    let id = p.id.clone();
    let type_ = p.type_.clone();
    match finish_property_strict(path, p, text, verify) {
        Err(_) if is_lenient_array(&type_) => Ok((
            XisfProperty {
                id,
                type_,
                value: PropertyValue::Unread,
                location: None,
            },
            None,
        )),
        r => r,
    }
}

/// True for numeric vector/matrix types other than f64. Before they were
/// decoded these read as [`PropertyValue::Unread`]; a malformed one still
/// does, rather than failing the whole file. F64 arrays stay strict.
fn is_lenient_array(type_: &str) -> bool {
    matches!(numeric_array_type(type_), Some((kind, _, _)) if !matches!(kind, NumKind::F64))
}

/// Store a resolved attachment value; a failure fails the read for f64
/// arrays and leaves the others `Unread` (see [`is_lenient_array`]).
fn lenient(prop: &mut XisfProperty, value: Result<PropertyValue>) -> Result<()> {
    match value {
        Ok(v) => prop.value = v,
        Err(_) if is_lenient_array(&prop.type_) => {
            prop.value = PropertyValue::Unread;
            prop.location = None;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// [`finish_property`] without the leniency for non-f64 arrays.
fn finish_property_strict(
    path: &Path,
    p: PendingProperty,
    text: &str,
    verify: bool,
) -> Result<(XisfProperty, Option<DataBlock>)> {
    let err = |msg: String| Error::format(path, msg);
    // Leading/trailing white space around a scalar is irrelevant (spec 7).
    let scalar = |p: &PendingProperty| -> String {
        p.value_attr.as_deref().unwrap_or(text).trim().to_string()
    };

    let mut location = None;
    let mut attached = None;
    let value = match p.type_.as_str() {
        "Float32" | "Float64" => PropertyValue::F64(
            scalar(&p)
                .parse()
                .map_err(|_| err(format!("property {}: bad float value", p.id)))?,
        ),
        "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" | "UInt64" => {
            PropertyValue::I64(
                scalar(&p)
                    .parse()
                    .map_err(|_| err(format!("property {}: bad integer value", p.id)))?,
            )
        }
        "Boolean" => match scalar(&p).as_str() {
            "1" | "true" => PropertyValue::I64(1),
            "0" | "false" => PropertyValue::I64(0),
            v => return Err(err(format!("property {}: bad boolean '{v}'", p.id))),
        },
        // PixInsight serializes a String as a data block when block
        // checksums are on (inline base64, or attached and compressed).
        "String" | "TimePoint" if p.location.is_some() => {
            let block =
                property_block(&p, text).map_err(|e| err(format!("property {}: {e}", p.id)))?;
            match &block.location {
                BlockLocation::Inline(_) => {
                    let bytes = block
                        .decode(&[], verify)
                        .map_err(|e| err(format!("property {}: {e}", p.id)))?;
                    PropertyValue::Str(String::from_utf8_lossy(&bytes).into_owned())
                }
                BlockLocation::Attachment { offset, size } => {
                    location = Some((*offset, *size));
                    attached = Some(block);
                    PropertyValue::Str(String::new()) // resolved by the caller
                }
            }
        }
        "String" | "TimePoint" => PropertyValue::Str(
            p.value_attr
                .clone()
                .unwrap_or_else(|| text.trim().to_string()),
        ),
        t if numeric_array_type(t).is_some() => {
            let (kind, width, is_matrix) = numeric_array_type(t).unwrap();
            let n = if !is_matrix {
                p.length
                    .ok_or_else(|| err(format!("property {}: missing length", p.id)))?
                    as usize
            } else {
                let (r, c) = p
                    .rows
                    .zip(p.cols)
                    .ok_or_else(|| err(format!("property {}: missing rows/columns", p.id)))?;
                (r as usize) * (c as usize)
            };
            let block =
                property_block(&p, text).map_err(|e| err(format!("property {}: {e}", p.id)))?;
            let expected = (n as u64).checked_mul(width as u64);
            if expected != Some(block.uncompressed_len()) {
                return Err(err(format!(
                    "property {}: data block is {} bytes, expected {}",
                    p.id,
                    block.uncompressed_len(),
                    expected.map_or_else(
                        || "more than 2^64 (length overflow)".to_string(),
                        |e| e.to_string()
                    )
                )));
            }
            let data: Vec<f64> = match &block.location {
                BlockLocation::Inline(_) => {
                    let bytes = block
                        .decode(&[], verify)
                        .map_err(|e| err(format!("property {}: {e}", p.id)))?;
                    decode_numbers_ordered(kind, width, block.byte_order, bytes)
                }
                BlockLocation::Attachment { offset, size } => {
                    location = Some((*offset, *size));
                    attached = Some(block);
                    Vec::new() // resolved from the file by the caller
                }
            };
            if !is_matrix {
                PropertyValue::F64Vec(data)
            } else {
                PropertyValue::F64Mat {
                    rows: p.rows.unwrap(),
                    cols: p.cols.unwrap(),
                    data,
                }
            }
        }
        _ => match p.value_attr.clone() {
            Some(v) => PropertyValue::Str(v),
            None => PropertyValue::Unread,
        },
    };

    Ok((
        XisfProperty {
            id: p.id,
            type_: p.type_,
            value,
            location,
        },
        attached,
    ))
}

/// The data block of a `<Property>` that has a `location` (inline,
/// embedded with its `<Data>` child, or attached).
fn property_block(p: &PendingProperty, text: &str) -> std::result::Result<DataBlock, String> {
    let (location, text_encoding, compression, subblocks, block_text) =
        match (p.location.as_deref(), &p.data) {
            (Some("embedded"), Some(d)) => (
                "embedded",
                d.encoding.as_deref(),
                d.compression.as_deref(),
                d.subblocks.as_deref(),
                Some(d.text.as_str()),
            ),
            (Some(loc), _) => (
                loc,
                None,
                p.compression.as_deref(),
                p.subblocks.as_deref(),
                Some(text),
            ),
            (None, _) => return Err("data block without a location".into()),
        };
    DataBlock::parse(&BlockAttrs {
        location,
        byte_order: p.byte_order.as_deref(),
        compression,
        subblocks,
        checksum: p.checksum.as_deref(),
        text: block_text,
        text_encoding,
    })
}

/// Decode elements of `kind` (`width` bytes each) stored in `order`.
fn decode_numbers_ordered(
    kind: NumKind,
    width: usize,
    order: ByteOrder,
    mut bytes: Vec<u8>,
) -> Vec<f64> {
    if order == ByteOrder::Big {
        for c in bytes.chunks_exact_mut(width) {
            c.reverse();
        }
    }
    decode_numbers(kind, &bytes)
}

/// Decode an attached vector/matrix property's block from its stored bytes
/// (checksum, decompression, byte order) into the property's value.
fn decode_property_block(
    path: &Path,
    prop: &XisfProperty,
    block: &DataBlock,
    stored: &[u8],
    verify: bool,
) -> Result<PropertyValue> {
    let mut bytes = block
        .decode_stored(stored, verify)
        .map_err(|e| Error::format(path, format!("property {}: {e}", prop.id)))?;
    if matches!(prop.type_.as_str(), "String" | "TimePoint") {
        return Ok(PropertyValue::Str(
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    if block.byte_order == ByteOrder::Big {
        let (_, width, _) = numeric_array_type(&prop.type_).ok_or_else(|| {
            Error::format(
                path,
                format!("property {}: not a numeric vector/matrix", prop.id),
            )
        })?;
        for c in bytes.chunks_exact_mut(width) {
            c.reverse();
        }
    }
    decode_attached_numbers(&bytes, path, prop)
}

/// Minimal standard-alphabet base64 decoder (payloads here are tens of bytes;
/// not worth a dependency). Ignores whitespace and padding.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .map(|b| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    Some(
        digits
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&[hi, lo]| (hi << 4) | lo)
            .collect(),
    )
}

/// Typed decoder for an attachment-located numeric vector/matrix property:
/// decodes its attachment `bytes` per the property's XISF type and returns
/// the filled-in value.
fn decode_attached_numbers(
    bytes: &[u8],
    path: &Path,
    prop: &XisfProperty,
) -> Result<PropertyValue> {
    let (kind, width, _) = numeric_array_type(&prop.type_).ok_or_else(|| {
        Error::format(
            path,
            format!("property {}: not a numeric vector/matrix", prop.id),
        )
    })?;
    if !bytes.len().is_multiple_of(width) {
        return Err(Error::format(
            path,
            format!(
                "property {}: attachment size {} not a multiple of {width}",
                prop.id,
                bytes.len()
            ),
        ));
    }
    let data = decode_numbers(kind, bytes);
    match &prop.value {
        PropertyValue::F64Vec(_) => Ok(PropertyValue::F64Vec(data)),
        PropertyValue::F64Mat { rows, cols, .. } => Ok(PropertyValue::F64Mat {
            rows: *rows,
            cols: *cols,
            data,
        }),
        _ => Err(Error::format(
            path,
            format!("property {}: not a vector/matrix", prop.id),
        )),
    }
}

fn attr_string(
    path: &Path,
    e: &quick_xml::events::BytesStart,
    key: &[u8],
) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr.map_err(|err| Error::format(path, format!("bad XML attribute: {err}")))?;
        if attr.key.local_name().as_ref() == key {
            let v = attr
                .decode_and_unescape_value(quick_xml::Decoder {})
                .map_err(|err| Error::format(path, format!("bad XML attribute value: {err}")))?;
            return Ok(Some(v.into_owned()));
        }
    }
    Ok(None)
}

fn parse_fits_keyword(path: &Path, e: &quick_xml::events::BytesStart) -> Result<FitsKeyword> {
    Ok(FitsKeyword {
        name: attr_string(path, e, b"name")?.unwrap_or_default(),
        value: attr_string(path, e, b"value")?.unwrap_or_default(),
        comment: attr_string(path, e, b"comment")?.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`finish_property`] for the unit tests: verify checksums, drop the
    /// attached-block side channel.
    fn finish(path: &Path, p: PendingProperty, text: &str) -> Result<XisfProperty> {
        finish_property(path, p, text, true).map(|(prop, _)| prop)
    }

    /// Build a minimal monolithic XISF file: 3x2 image, 2 channels, Float32.
    fn synth_xisf(dir: &Path) -> PathBuf {
        let (w, h, c) = (3u64, 2u64, 2u64);
        let data_offset = 512u64;
        let data_size = w * h * c * 4;
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf"><Image geometry="{w}:{h}:{c}" sampleFormat="Float32" colorSpace="Gray" location="attachment:{data_offset}:{data_size}"><FITSKeyword name="OBJECT" value="'Test'" comment="test object"/></Image></xisf>"#
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        bytes.resize(data_offset as usize, 0);
        for i in 0..(w * h * c) {
            bytes.extend_from_slice(&(i as f32).to_le_bytes());
        }
        let path = dir.join("synth.xisf");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn reads_synthetic_xisf() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = synth_xisf(&dir);

        let panel = XisfImage::open(&path).unwrap();
        assert_eq!(panel.width(), 3);
        assert_eq!(panel.height(), 2);
        assert_eq!(panel.channels(), 2);
        assert_eq!(panel.header().fits_keywords.len(), 1);
        assert_eq!(panel.header().fits_keywords[0].name, "OBJECT");

        // Channel 0 = values 0..6, channel 1 = 6..12; row 1 of channel 1 = [9,10,11].
        assert_eq!(panel.channel(0), &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(panel.row(1, 1), &[9.0, 10.0, 11.0]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Base64-encode f64s little-endian (test helper mirroring what PixInsight
    /// writes for `location="inline:base64"` properties).
    fn b64_f64s(data: &[f64]) -> String {
        let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        b64(&bytes)
    }

    /// Standard-alphabet base64 with padding.
    fn b64(bytes: &[u8]) -> String {
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
            for i in 0..4 {
                if i <= chunk.len() {
                    s.push(ALPHA[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    /// Synthetic XISF exercising all property shapes: scalar `value` attrs,
    /// element text, inline base64 vectors/matrices, and an attachment-located
    /// vector whose payload lives at byte 256.
    fn synth_xisf_with_properties(dir: &Path) -> PathBuf {
        let (w, h, c) = (2u64, 2u64, 1u64);
        let data_offset = 2048u64;
        let data_size = w * h * c * 4;
        let att = [10.5f64, -20.25, 30.0];
        let vec2 = [4627.5f64, 9155.0];
        let mat = [-4.4e-4f64, 0.0, 0.0, 4.4e-4];
        let xml = format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
                r#"<Image geometry="{w}:{h}:{c}" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}">"#,
                r#"<Property id="P:Scalar" type="Float64" value="1.5"/>"#,
                r#"<Property id="P:Int" type="Int32" value="-7"/>"#,
                r#"<Property id="P:Time" type="TimePoint" value="2026-07-24T03:43:50.946Z"/>"#,
                r#"<Property id="P:Text" type="String">hello &amp; world</Property>"#,
                r#"<Property id="P:Vec" type="F64Vector" length="2" location="inline:base64">{vec_b64}</Property>"#,
                r#"<Property id="P:Mat" type="F64Matrix" rows="2" columns="2" location="inline:base64">{mat_b64}</Property>"#,
                r#"<Property id="P:Att" type="F64Vector" length="3" location="attachment:1536:24"/>"#,
                r#"<Property id="P:Odd" type="I32Vector" length="2" location="inline:base64">AAAAAAEAAAA=</Property>"#,
                r#"<Property id="P:Cplx" type="C64Vector" length="1" location="inline:base64">AAAAAAAAAAAAAAAAAAAAAA==</Property>"#,
                r#"</Image></xisf>"#,
            ),
            w = w,
            h = h,
            c = c,
            off = data_offset,
            size = data_size,
            vec_b64 = b64_f64s(&vec2),
            mat_b64 = b64_f64s(&mat),
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        assert!(
            bytes.len() <= 1536,
            "header must fit below the attachment payload"
        );
        bytes.resize(1536, 0);
        for v in att {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.resize(data_offset as usize, 0);
        for i in 0..(w * h * c) {
            bytes.extend_from_slice(&(i as f32).to_le_bytes());
        }
        let path = dir.join("synth-props.xisf");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn parses_properties_in_all_shapes() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-props-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = synth_xisf_with_properties(&dir);

        let panel = XisfImage::open(&path).unwrap();
        let props = &panel.header().properties;
        let get = |id: &str| {
            props
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("property {id} missing"))
        };

        assert_eq!(get("P:Scalar").value.as_f64(), Some(1.5));
        assert_eq!(get("P:Scalar").type_, "Float64");
        assert_eq!(get("P:Int").value, PropertyValue::I64(-7));
        assert_eq!(
            get("P:Time").value.as_str(),
            Some("2026-07-24T03:43:50.946Z")
        );
        assert_eq!(get("P:Text").value.as_str(), Some("hello & world"));
        assert_eq!(get("P:Vec").value.as_f64_vec(), Some(&[4627.5, 9155.0][..]));
        let (rows, cols, data) = get("P:Mat").value.as_f64_mat().unwrap();
        assert_eq!((rows, cols), (2, 2));
        assert_eq!(data, &[-4.4e-4, 0.0, 0.0, 4.4e-4]);

        let att = get("P:Att");
        assert_eq!(
            att.location,
            Some((1536, 24)),
            "attachment offset/size exposed"
        );
        assert_eq!(
            att.value.as_f64_vec(),
            Some(&[10.5, -20.25, 30.0][..]),
            "attachment-located f64 vector resolved on open"
        );

        assert_eq!(
            get("P:Odd").value,
            PropertyValue::F64Vec(vec![0.0, 1.0]),
            "integer vectors decode to f64"
        );
        assert_eq!(get("P:Odd").type_, "I32Vector");
        assert_eq!(
            get("P:Cplx").value,
            PropertyValue::Unread,
            "undecoded types stay Unread"
        );

        // Image data still reads with properties present.
        assert_eq!(panel.channel(0), &[0.0, 1.0, 2.0, 3.0]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decodes_real_pixinsight_inline_base64() {
        // Exact bytes captured from a real MosaicByCoordinates panel header
        // (PCL:AstrometricSolution:LinearTransformationMatrix).
        let bytes = base64_decode("qj/NbcwTPb8AAAAAAAAAAAAAAAAAAAAAqj/NbcwTPT8=").unwrap();
        let vals: Vec<f64> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| f64::from_le_bytes(*c))
            .collect();
        assert_eq!(
            vals,
            vec![-4.436849683786501e-4, 0.0, 0.0, 4.436849683786501e-4]
        );
    }

    #[test]
    fn rejects_bad_signature() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.xisf");
        std::fs::write(&path, b"NOTXISF0 garbage").unwrap();
        assert!(XisfImage::open(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_header_resolves_attachments_without_reading_pixels() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = synth_xisf_with_properties(&dir);
        let full = XisfImage::open(&path).unwrap();
        let h = read_header(&path).unwrap();
        assert_eq!(h, Header::from(full.header()));
        assert_eq!(
            h.property("P:Att").unwrap().value.as_f64_vec(),
            Some(&[10.5, -20.25, 30.0][..])
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_header_accepts_any_sample_format() {
        // A UInt16 image: not mapped (it needs decoding), and the header reads.
        let dir = std::env::temp_dir().join(format!("mpp-xisf-u16-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let xml = concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="2:2:1" sampleFormat="UInt16" colorSpace="Gray" location="attachment:1024:8">"#,
            r#"<FITSKeyword name="FOCALLEN" value="1645.0" comment="[mm] Focal length"/>"#,
            r#"<Property id="Observation:Object:Name" type="String">NGC 3199</Property>"#,
            r#"</Image></xisf>"#,
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        bytes.resize(1032, 0);
        let path = dir.join("u16.xisf");
        std::fs::write(&path, bytes).unwrap();
        assert!(!XisfImage::open(&path).unwrap().is_mapped());
        let h = read_header(&path).unwrap();
        assert_eq!(h.keywords.len(), 1);
        assert_eq!(
            h.keyword("FOCALLEN").unwrap().typed_value().as_f64(),
            Some(1645.0)
        );
        assert_eq!(
            h.property("Observation:Object:Name")
                .unwrap()
                .value
                .as_str(),
            Some("NGC 3199")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_header_rejects_bad_signature_and_out_of_bounds_attachments() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-hdrbad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.xisf");
        std::fs::write(&path, b"NOTXISF0 garbage").unwrap();
        assert!(read_header(&path).is_err());
        let xml = concat!(
            r#"<xisf version="1.0"><Image geometry="1:1:1" sampleFormat="Float32" location="attachment:512:4">"#,
            r#"<Property id="P:Far" type="F64Vector" length="2" location="attachment:9999:16"/>"#,
            r#"</Image></xisf>"#,
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        bytes.resize(516, 0);
        std::fs::write(&path, bytes).unwrap();
        let e = read_header(&path).unwrap_err().to_string();
        assert!(e.contains("out of bounds"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn pending(
        id: &str,
        type_: &str,
        location: Option<&str>,
        length: Option<u64>,
        rows: Option<u32>,
        cols: Option<u32>,
    ) -> PendingProperty {
        PendingProperty {
            id: id.into(),
            type_: type_.into(),
            value_attr: None,
            location: location.map(str::to_string),
            length,
            rows,
            cols,
            byte_order: None,
            compression: None,
            subblocks: None,
            checksum: None,
            data: None,
        }
    }

    #[test]
    fn i32vector_inline_decodes_to_f64vec_keeping_type() {
        let bytes: Vec<u8> = [0i32, 5, 12, -3]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let p = finish(
            Path::new("x"),
            pending(
                "AstrometricSolution:DistortionModel:ImageToProjection:Local:X:NodeOffsets",
                "I32Vector",
                Some("inline:base64"),
                Some(4),
                None,
                None,
            ),
            &b64(&bytes),
        )
        .unwrap();
        assert_eq!(p.type_, "I32Vector");
        assert_eq!(p.value, PropertyValue::F64Vec(vec![0.0, 5.0, 12.0, -3.0]));
    }

    #[test]
    fn f32matrix_inline_decodes_row_major() {
        let bytes: Vec<u8> = [1.5f32, 2.0, 3.0, 4.0, 5.0, 6.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let p = finish(
            Path::new("x"),
            pending(
                "m",
                "F32Matrix",
                Some("inline:base64"),
                None,
                Some(2),
                Some(3),
            ),
            &b64(&bytes),
        )
        .unwrap();
        assert_eq!(
            p.value,
            PropertyValue::F64Mat {
                rows: 2,
                cols: 3,
                data: vec![1.5, 2.0, 3.0, 4.0, 5.0, 6.0]
            }
        );
    }

    #[test]
    fn i32vector_attachment_size_uses_element_width() {
        // 4 elements × 4 bytes = 16 bytes; an 8-byte-per-element assumption
        // would reject it.
        let p = finish(
            Path::new("x"),
            pending(
                "v",
                "I32Vector",
                Some("attachment:4096:16"),
                Some(4),
                None,
                None,
            ),
            "",
        )
        .unwrap();
        assert_eq!(p.location, Some((4096, 16)));
        let wrong = || {
            pending(
                "v",
                "I32Vector",
                Some("attachment:4096:32"),
                Some(4),
                None,
                None,
            )
        };
        assert!(finish_property_strict(Path::new("x"), wrong(), "", true).is_err());
        // A malformed non-f64 array reads as Unread rather than failing.
        let p = finish(Path::new("x"), wrong(), "").unwrap();
        assert_eq!((p.value, p.location), (PropertyValue::Unread, None));
    }

    #[test]
    fn multiline_string_property_keeps_newline() {
        let p = finish(
            Path::new("x"),
            pending(
                "AstrometricSolution:DistortionModel:ImageToProjection:Terms",
                "String",
                None,
                None,
                None,
                None,
            ),
            "Local\nFallback",
        )
        .unwrap();
        assert_eq!(p.value, PropertyValue::Str("Local\nFallback".into()));
    }

    /// A 2×2 Float32 XISF whose header holds `props_xml` (attachments, if
    /// any, at byte 1024; pixels at 2048).
    fn synth_with(dir: &Path, name: &str, props_xml: &str, att: &[u8]) -> PathBuf {
        let xml = format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
                r#"<Image geometry="2:2:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:2048:16">"#,
                r#"<FITSKeyword name="NAXIS1" value="4944" comment="stale"/>"#,
                "{}",
                r#"</Image></xisf>"#,
            ),
            props_xml
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        assert!(bytes.len() <= 1024);
        bytes.resize(1024, 0);
        bytes.extend_from_slice(att);
        bytes.resize(2048 + 16, 0);
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn read_header_drops_the_pixinsight_grid_cache_and_records_the_geometry() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-grid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = synth_with(
            &dir,
            "grid.xisf",
            concat!(
                r#"<Property id="PCL:AstrometricSolution:Grid:ImageToProjection:GridX" type="F64Matrix" rows="1" columns="2" location="attachment:1024:16"/>"#,
                r#"<Property id="PCL:AstrometricSolution:Grid:ImageToProjection:Delta" type="Float64" value="8"/>"#,
                r#"<Property id="PCL:AstrometricSolution:Generation:Engine" type="String" value="DDM"/>"#,
            ),
            &[0u8; 16],
        );
        let h = read_header(&path).unwrap();
        assert!(
            !h.properties
                .iter()
                .any(|p| p.id.starts_with("PCL:AstrometricSolution:Grid:")),
            "{:?}",
            h.properties
        );
        assert!(
            h.property("PCL:AstrometricSolution:Generation:Engine")
                .is_some()
        );
        assert_eq!(h.image_size, Some((2, 2)), "geometry, not the stale NAXIS1");
        // XisfImage keeps every property.
        let img = XisfImage::open(&path).unwrap();
        assert_eq!(img.header().properties.len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_non_f64_arrays_degrade_to_unread_but_f64_still_fails() {
        let dir = std::env::temp_dir().join(format!("mpp-xisf-lenient-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = synth_with(
            &dir,
            "lenient.xisf",
            concat!(
                // 3 elements claimed, 2 encoded.
                r#"<Property id="P:Short" type="I32Vector" length="3" location="inline:base64">AAAAAAEAAAA=</Property>"#,
                r#"<Property id="P:Where" type="UI16Matrix" rows="1" columns="2" location="embedded"/>"#,
                r#"<Property id="P:NoLen" type="F32Vector" location="inline:base64">AAAAAA==</Property>"#,
                r#"<Property id="P:Far" type="I32Vector" length="2" location="attachment:999999:8"/>"#,
            ),
            &[],
        );
        let img = XisfImage::open(&path).unwrap();
        for id in ["P:Short", "P:Where", "P:NoLen", "P:Far"] {
            let p = img.header().properties.iter().find(|p| p.id == id).unwrap();
            assert_eq!(p.value, PropertyValue::Unread, "{id}");
        }
        assert!(read_header(&path).is_ok());
        let bad = synth_with(
            &dir,
            "bad-f64.xisf",
            r#"<Property id="P:Short" type="F64Vector" length="3" location="inline:base64">AAAAAAAAAAA=</Property>"#,
            &[],
        );
        assert!(XisfImage::open(&bad).is_err(), "F64 behaviour unchanged");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    mod blocks {
        //! Property data blocks: compression, checksums, embedded, byte order.
        use super::super::*;
        use crate::formats::xisf_codec::Codec;
        use crate::testkit::{BlockSpec, Placement, PropSpec, XisfBuilder, tiny_image};

        fn tmp(tag: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("mpp-xisf-blocks-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn f64s(v: &[f64]) -> Vec<u8> {
            v.iter().flat_map(|x| x.to_le_bytes()).collect()
        }

        fn matrix() -> Vec<f64> {
            (0..600).map(|i| (i / 3) as f64 * 0.25 - 7.0).collect()
        }

        fn unit(prop: PropSpec) -> XisfBuilder {
            XisfBuilder {
                images: vec![tiny_image()],
                properties: vec![prop],
                root_xml: String::new(),
            }
        }

        fn compressed_matrix(bad_checksum: bool) -> XisfBuilder {
            unit(PropSpec {
                id: "AstrometricSolution:ControlPoints:Celestial",
                type_: "F64Matrix",
                shape: r#" rows="300" columns="2""#.into(),
                data: f64s(&matrix()),
                attrs: String::new(),
                block: BlockSpec {
                    placement: Placement::Attached,
                    codec: Some((Codec::Zstd, Some(8), 1 << 20)),
                    checksum: Some("sha-256"),
                    bad_checksum,
                },
            })
        }

        fn prop<'a>(props: &'a [XisfProperty], id: &str) -> &'a XisfProperty {
            props
                .iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("no {id}"))
        }

        #[test]
        fn compressed_property_block_decodes() {
            let d = tmp("cmp");
            let p = d.join("a.xisf");
            compressed_matrix(false).write(&p);
            let img = XisfImage::open(&p).unwrap();
            let v = &prop(
                &img.header().properties,
                "AstrometricSolution:ControlPoints:Celestial",
            )
            .value;
            assert_eq!(v.as_f64_mat(), Some((300, 2, &matrix()[..])));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn read_header_decodes_compressed_properties() {
            let d = tmp("cmp-hdr");
            let p = d.join("a.xisf");
            compressed_matrix(false).write(&p);
            let h = read_header(&p).unwrap();
            let v = &h
                .property("AstrometricSolution:ControlPoints:Celestial")
                .unwrap()
                .value;
            assert_eq!(v.as_f64_mat(), Some((300, 2, &matrix()[..])));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn property_checksum_mismatch_is_an_error_unless_disabled() {
            let d = tmp("cksum");
            let p = d.join("a.xisf");
            compressed_matrix(true).write(&p);
            let e = XisfImage::open(&p)
                .expect_err("bad checksum must fail")
                .to_string();
            assert!(e.contains("checksum mismatch"), "{e}");
            let e = read_header(&p).unwrap_err().to_string();
            assert!(e.contains("checksum mismatch"), "{e}");
            let opts = XisfOpenOptions {
                verify_checksums: false,
                ..Default::default()
            };
            let img = XisfImage::open_with(&p, &opts).unwrap();
            let v = &prop(
                &img.header().properties,
                "AstrometricSolution:ControlPoints:Celestial",
            )
            .value;
            assert_eq!(v.as_f64_mat(), Some((300, 2, &matrix()[..])));
            assert!(read_header_with(&p, &opts).is_ok());
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn embedded_property_block_decodes() {
            let d = tmp("emb");
            let p = d.join("a.xisf");
            let v = vec![1.5, -2.25, 1e-300, 4.0];
            unit(PropSpec {
                id: "Test:Embedded",
                type_: "F64Vector",
                shape: r#" length="4""#.into(),
                data: f64s(&v),
                attrs: String::new(),
                block: BlockSpec {
                    placement: Placement::Embedded,
                    codec: Some((Codec::Zlib, None, 1 << 20)),
                    checksum: None,
                    bad_checksum: false,
                },
            })
            .write(&p);
            for props in [
                XisfImage::open(&p).unwrap().header().properties.clone(),
                read_header(&p).unwrap().properties,
            ] {
                assert_eq!(
                    prop(&props, "Test:Embedded").value.as_f64_vec(),
                    Some(&v[..])
                );
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn big_endian_property_vector_decodes() {
            let d = tmp("be");
            let p = d.join("a.xisf");
            let v = [3.0f64, -0.125, 7.5];
            let be: Vec<u8> = v.iter().flat_map(|x| x.to_be_bytes()).collect();
            unit(PropSpec {
                id: "Test:BigEndian",
                type_: "F64Vector",
                shape: r#" length="3""#.into(),
                data: be,
                attrs: r#" byteOrder="big""#.into(),
                block: BlockSpec {
                    placement: Placement::Inline,
                    ..Default::default()
                },
            })
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            assert_eq!(
                prop(&img.header().properties, "Test:BigEndian")
                    .value
                    .as_f64_vec(),
                Some(&v[..])
            );
            let i32s: Vec<u8> = [7i32, -9].iter().flat_map(|x| x.to_be_bytes()).collect();
            unit(PropSpec {
                id: "Test:I32",
                type_: "I32Vector",
                shape: r#" length="2""#.into(),
                data: i32s,
                attrs: r#" byteOrder="big""#.into(),
                block: BlockSpec::default(),
            })
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            assert_eq!(
                prop(&img.header().properties, "Test:I32")
                    .value
                    .as_f64_vec(),
                Some(&[7.0, -9.0][..])
            );
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn scalar_text_forms() {
            let d = tmp("text");
            let p = d.join("a.xisf");
            let mut root = String::new();
            for (i, v) in ["true", "false", "1", "0", " true "].iter().enumerate() {
                root.push_str(&format!(
                    r#"<Property id="B{i}" type="Boolean" value="{v}"/>"#
                ));
            }
            for (i, v) in ["NaN", "+Inf", "-Inf", "nan", "-nan", "inf", "-inf", " 2.5 "]
                .iter()
                .enumerate()
            {
                root.push_str(&format!(
                    r#"<Property id="F{i}" type="Float64" value="{v}"/>"#
                ));
            }
            root.push_str(r#"<Property id="I" type="Int32" value=" 42 "/>"#);
            root.push_str(r#"<Property id="S" type="String">  keep inner  </Property>"#);
            XisfBuilder {
                images: vec![tiny_image()],
                properties: vec![],
                root_xml: root,
            }
            .write(&p);
            let props = XisfImage::open(&p).unwrap().header().properties.clone();
            let get = |id: &str| prop(&props, id).value.clone();
            let bools: Vec<_> = (0..5).map(|i| get(&format!("B{i}"))).collect();
            assert_eq!(
                bools,
                [1, 0, 1, 0, 1].map(PropertyValue::I64).to_vec(),
                "Boolean true/false/1/0"
            );
            let f = |i: usize| get(&format!("F{i}")).as_f64().unwrap();
            assert!(f(0).is_nan() && f(3).is_nan() && f(4).is_nan());
            assert_eq!(
                (f(1), f(2), f(5), f(6), f(7)),
                (
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    2.5
                )
            );
            assert_eq!(get("I"), PropertyValue::I64(42));
            assert_eq!(get("S"), PropertyValue::Str("keep inner".into()));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn string_property_blocks_decode() {
            // PixInsight writes String properties as data blocks when block
            // checksums are on: inline base64 with a checksum, or attached
            // and compressed (e.g. PixInsight:ProcessingHistory, zstd).
            let d = tmp("strings");
            let p = d.join("a.xisf");
            let history = "<?xml version=\"1.0\"?><ProcessingHistory>é ".repeat(200);
            XisfBuilder {
                images: vec![tiny_image()],
                properties: vec![
                    PropSpec {
                        id: "PCL:Signature:Preprocessing",
                        type_: "String",
                        shape: String::new(),
                        data: b"process=WeightedBatchPreprocessing,version=2.8.9".to_vec(),
                        attrs: String::new(),
                        block: BlockSpec {
                            placement: Placement::Inline,
                            checksum: Some("sha-256"),
                            ..Default::default()
                        },
                    },
                    PropSpec {
                        id: "PixInsight:ProcessingHistory",
                        type_: "String",
                        shape: String::new(),
                        data: history.as_bytes().to_vec(),
                        attrs: String::new(),
                        block: BlockSpec {
                            placement: Placement::Attached,
                            codec: Some((Codec::Zstd, None, 1 << 20)),
                            checksum: Some("sha-256"),
                            bad_checksum: false,
                        },
                    },
                ],
                root_xml: String::new(),
            }
            .write(&p);
            for props in [
                XisfImage::open(&p).unwrap().header().properties.clone(),
                read_header(&p).unwrap().properties,
            ] {
                assert_eq!(
                    prop(&props, "PCL:Signature:Preprocessing").value,
                    PropertyValue::Str("process=WeightedBatchPreprocessing,version=2.8.9".into())
                );
                assert_eq!(
                    prop(&props, "PixInsight:ProcessingHistory").value,
                    PropertyValue::Str(history.clone())
                );
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn empty_vector_inline_block() {
            let d = tmp("empty");
            let p = d.join("a.xisf");
            unit(PropSpec {
                id: "Test:Empty",
                type_: "F64Vector",
                shape: r#" length="0""#.into(),
                data: vec![],
                attrs: String::new(),
                block: BlockSpec {
                    placement: Placement::Inline,
                    ..Default::default()
                },
            })
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            assert_eq!(
                prop(&img.header().properties, "Test:Empty")
                    .value
                    .as_f64_vec(),
                Some(&[][..])
            );
            std::fs::remove_dir_all(&d).unwrap();
        }
    }

    mod images {
        //! `<Image>` attributes and multi-image units.
        use super::super::*;
        use crate::testkit::{ImageSpec, XisfBuilder, tiny_image};

        fn tmp(tag: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("mpp-xisf-images-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn image(w: u64, h: u64, keyword: &str) -> ImageSpec {
            ImageSpec {
                geometry: (w, h, 1),
                data: vec![0u8; (w * h * 4) as usize],
                children: format!(r#"<FITSKeyword name="OBJECT" value="'{keyword}'" comment=""/>"#),
                ..tiny_image()
            }
        }

        fn header(p: &Path, image_index: usize) -> Result<XisfHeader> {
            parse_header(p, &std::fs::read(p).unwrap(), true, image_index)
        }

        #[test]
        fn second_image_is_selectable() {
            let d = tmp("multi");
            let p = d.join("a.xisf");
            let mut second = image(5, 3, "second");
            second.attrs.push_str(r#" id="beta""#);
            XisfBuilder {
                images: vec![image(4, 2, "first"), second],
                ..Default::default()
            }
            .write(&p);
            let h0 = header(&p, 0).unwrap();
            assert_eq!((h0.width, h0.height, h0.image_count), (4, 2, 2));
            assert_eq!(h0.image_ids, vec![None, Some("beta".to_string())]);
            assert_eq!(h0.fits_keywords[0].value, "'first'");
            let h1 = header(&p, 1).unwrap();
            assert_eq!((h1.width, h1.height), (5, 3));
            assert_eq!(h1.image_id.as_deref(), Some("beta"));
            assert_eq!(
                h1.fits_keywords.len(),
                1,
                "only the selected image's keywords"
            );
            assert_eq!(h1.fits_keywords[0].value, "'second'");
            let opts = XisfOpenOptions {
                image_index: 1,
                ..Default::default()
            };
            let img = XisfImage::open_with(&p, &opts).unwrap();
            assert_eq!((img.width(), img.height()), (5, 3));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn image_index_out_of_range_is_an_error() {
            let d = tmp("range");
            let p = d.join("a.xisf");
            XisfBuilder {
                images: vec![tiny_image()],
                ..Default::default()
            }
            .write(&p);
            let e = header(&p, 1).unwrap_err().to_string();
            assert!(e.contains("image 1") && e.contains("1 image"), "{e}");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn image_properties_belong_to_their_image() {
            let d = tmp("props");
            let p = d.join("a.xisf");
            let mut a = image(2, 2, "a");
            a.children
                .push_str(r#"<Property id="Owner" type="String">a</Property>"#);
            let mut b = image(2, 2, "b");
            b.children
                .push_str(r#"<Property id="Owner" type="String">b</Property>"#);
            XisfBuilder {
                images: vec![a, b],
                root_xml: r#"<Property id="FileLevel" type="Int32" value="1"/>"#.into(),
                ..Default::default()
            }
            .write(&p);
            let h = header(&p, 1).unwrap();
            let ids: Vec<(&str, &PropertyValue)> = h
                .properties
                .iter()
                .map(|p| (p.id.as_str(), &p.value))
                .collect();
            assert_eq!(
                ids,
                vec![
                    ("Owner", &PropertyValue::Str("b".into())),
                    ("FileLevel", &PropertyValue::I64(1))
                ]
            );
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn other_images_bad_properties_do_not_block_this_one() {
            let d = tmp("isolation");
            let p = d.join("a.xisf");
            let mut bad = image(2, 2, "bad");
            // 3 bytes where an F64Vector of length 2 needs 16.
            bad.children.push_str(r#"<Property id="Bad" type="F64Vector" length="2" location="inline:base64">AQID</Property>"#);
            XisfBuilder {
                images: vec![image(2, 2, "good"), bad],
                ..Default::default()
            }
            .write(&p);
            let h = header(&p, 0).unwrap();
            assert!(h.properties.iter().all(|p| p.id != "Bad"));
            assert!(read_header(&p).is_ok());
            assert!(XisfImage::open(&p).is_ok());
            let e = header(&p, 1).unwrap_err().to_string();
            assert!(
                e.contains("Bad"),
                "the selected image still reports it: {e}"
            );
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn thumbnail_is_ignored() {
            let d = tmp("thumb");
            let p = d.join("a.xisf");
            let mut img = image(2, 2, "main");
            img.children.push_str(concat!(
                r#"<Thumbnail geometry="1:1:1" sampleFormat="UInt8" location="attachment:0:1">"#,
                r#"<FITSKeyword name="OBJECT" value="'thumb'" comment=""/>"#,
                r#"<Property id="ThumbProp" type="Int32" value="3"/>"#,
                r#"</Thumbnail>"#
            ));
            XisfBuilder {
                images: vec![img],
                ..Default::default()
            }
            .write(&p);
            let h = header(&p, 0).unwrap();
            assert_eq!(h.image_count, 1);
            assert_eq!(h.fits_keywords.len(), 1);
            assert!(h.properties.iter().all(|p| p.id != "ThumbProp"));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn image_attributes_parse() {
            let d = tmp("attrs");
            let p = d.join("a.xisf");
            XisfBuilder {
                images: vec![ImageSpec {
                    geometry: (2, 2, 3),
                    sample_format: "UInt16",
                    data: vec![0u8; 24],
                    attrs: r#" bounds="100:60000" colorSpace="RGB" pixelStorage="Normal" byteOrder="big" offset="12.5" imageType="Light" orientation="90""#.into(),
                    ..tiny_image()
                }],
                ..Default::default()
            }
            .write(&p);
            let h = header(&p, 0).unwrap();
            assert_eq!(h.sample_format, SampleFormat::UInt16);
            assert_eq!(h.bounds, Some((100.0, 60000.0)));
            assert_eq!(h.effective_bounds(), (100.0, 60000.0));
            assert_eq!(h.color_space, ColorSpace::Rgb);
            assert_eq!(h.pixel_storage, PixelStorage::Normal);
            assert_eq!(h.block.byte_order, ByteOrder::Big);
            assert_eq!(h.offset, 12.5);
            assert_eq!((h.data_offset % 4096, h.data_size), (0, 24));
            for (attr, want) in [
                (r#" colorSpace="HSV""#, "colorSpace"),
                (r#" pixelStorage="Tiled""#, "pixelStorage"),
                (r#" bounds="1""#, "bounds"),
                (r#" offset="x""#, "offset"),
            ] {
                XisfBuilder {
                    images: vec![ImageSpec {
                        attrs: attr.into(),
                        ..tiny_image()
                    }],
                    ..Default::default()
                }
                .write(&p);
                let e = header(&p, 0).unwrap_err().to_string();
                assert!(e.contains(want), "{attr}: {e}");
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn missing_bounds_default_by_sample_format() {
            let d = tmp("bounds");
            let p = d.join("a.xisf");
            for (fmt, bytes, want) in [
                ("Float32", 4, (0.0, 1.0)),
                ("Float64", 8, (0.0, 1.0)),
                ("UInt8", 1, (0.0, 255.0)),
                ("UInt16", 2, (0.0, 65535.0)),
                ("UInt32", 4, (0.0, 4294967295.0)),
            ] {
                XisfBuilder {
                    images: vec![ImageSpec {
                        sample_format: fmt,
                        data: vec![0u8; 4 * bytes],
                        attrs: String::new(),
                        ..tiny_image()
                    }],
                    ..Default::default()
                }
                .write(&p);
                let h = header(&p, 0).unwrap();
                assert_eq!(h.bounds, None, "{fmt}");
                assert_eq!(h.effective_bounds(), want, "{fmt}");
                assert_eq!(h.color_space, ColorSpace::Gray, "default color space");
                assert_eq!(h.pixel_storage, PixelStorage::Planar, "default storage");
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn wide_and_complex_sample_formats_parse() {
            for (s, f, n) in [
                ("UInt64", SampleFormat::UInt64, 8),
                ("Complex32", SampleFormat::Complex32, 8),
                ("Complex64", SampleFormat::Complex64, 16),
            ] {
                assert_eq!(SampleFormat::parse(s), Some(f));
                assert_eq!(f.bytes_per_sample(), n);
            }
        }
    }

    mod pixels {
        //! Pixel decoding: every codec, sample format, byte order and
        //! storage model, against the zero-copy reference.
        use super::super::*;
        use crate::formats::xisf_codec::Codec;
        use crate::testkit::{BlockSpec, ImageSpec, Placement, XisfBuilder, tiny_image};

        const W: u64 = 33;
        const H: u64 = 17;
        const C: u64 = 3;
        const N: usize = (W * H * C) as usize;

        fn tmp(tag: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("mpp-xisf-pixels-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// Planar reference values in [0, 1]: quantized ramps (compressible
        /// by every codec), plus a NaN that must survive bit-exactly.
        fn reference() -> Vec<f32> {
            let mut v: Vec<f32> = (0..N).map(|i| ((i / 5) % 64) as f32 / 64.0).collect();
            v[7] = f32::NAN;
            v
        }

        fn f32_bytes(v: &[f32], big: bool) -> Vec<u8> {
            v.iter()
                .flat_map(|x| {
                    if big {
                        x.to_be_bytes()
                    } else {
                        x.to_le_bytes()
                    }
                })
                .collect()
        }

        fn rgb(data: Vec<u8>, fmt: &'static str, attrs: &str, block: BlockSpec) -> XisfBuilder {
            XisfBuilder {
                images: vec![ImageSpec {
                    geometry: (W, H, C),
                    sample_format: fmt,
                    data,
                    attrs: format!(r#" colorSpace="RGB"{attrs}"#),
                    children: String::new(),
                    block,
                }],
                ..Default::default()
            }
        }

        fn decode(p: &Path) -> Vec<f32> {
            let img = XisfImage::open(p).unwrap();
            let mut out = vec![0f32; N];
            img.decode_into(&mut out).unwrap();
            out
        }

        fn bits(v: &[f32]) -> Vec<u32> {
            v.iter().map(|x| x.to_bits()).collect()
        }

        fn close(a: &[f32], b: &[f32], tol: f32) {
            assert_eq!(a.len(), b.len());
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert!(
                    (x - y).abs() <= tol || (x.is_nan() && y.is_nan()),
                    "sample {i}: {x} vs {y}"
                );
            }
        }

        #[test]
        fn uncompressed_float32_stays_mapped() {
            let d = tmp("mapped");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec::default(),
            )
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            assert!(img.is_mapped());
            assert_eq!(
                bits(img.channel(1)),
                bits(&reference()[(W * H) as usize..2 * (W * H) as usize])
            );
            assert_eq!(bits(&decode(&p)), bits(&reference()));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn every_codec_decodes_bit_identical_to_uncompressed() {
            let d = tmp("codecs");
            let p = d.join("a.xisf");
            let data = f32_bytes(&reference(), false);
            for codec in [Codec::Zlib, Codec::Lz4, Codec::Lz4Hc, Codec::Zstd] {
                for sh in [None, Some(4)] {
                    for sub in [data.len(), data.len() / 3 + 1] {
                        rgb(
                            data.clone(),
                            "Float32",
                            r#" bounds="0:1""#,
                            BlockSpec {
                                codec: Some((codec, sh, sub)),
                                checksum: Some("sha-1"),
                                ..Default::default()
                            },
                        )
                        .write(&p);
                        let img = XisfImage::open(&p).unwrap();
                        assert!(!img.is_mapped(), "{codec:?}");
                        assert_eq!(
                            bits(&decode(&p)),
                            bits(&reference()),
                            "{codec:?} {sh:?} {sub}"
                        );
                    }
                }
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn integer_formats_normalize() {
            let d = tmp("ints");
            let p = d.join("a.xisf");
            let raw: Vec<u64> = (0..N as u64).map(|i| (i * 7) % 200).collect();
            for (fmt, width, max) in [
                ("UInt8", 1usize, 255.0f64),
                ("UInt16", 2, 65535.0),
                ("UInt32", 4, 4294967295.0),
            ] {
                let bytes: Vec<u8> = raw
                    .iter()
                    .flat_map(|&v| v.to_le_bytes()[..width].to_vec())
                    .collect();
                for (attrs, lo, hi) in [("", 0.0, max), (r#" bounds="10:200""#, 10.0, 200.0)] {
                    rgb(
                        bytes.clone(),
                        fmt,
                        attrs,
                        BlockSpec {
                            codec: Some((Codec::Zstd, (width > 1).then_some(width), 1 << 20)),
                            ..Default::default()
                        },
                    )
                    .write(&p);
                    let want: Vec<f32> = raw
                        .iter()
                        .map(|&v| ((v as f64 - lo) / (hi - lo)) as f32)
                        .collect();
                    close(&decode(&p), &want, 1e-6);
                }
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn float64_and_nonunit_bounds_rescale() {
            let d = tmp("floats");
            let p = d.join("a.xisf");
            let r = reference();
            let f64s: Vec<u8> = r.iter().flat_map(|&x| (x as f64).to_le_bytes()).collect();
            rgb(f64s, "Float64", r#" bounds="0:1""#, BlockSpec::default()).write(&p);
            assert_eq!(bits(&decode(&p)), bits(&r), "Float64 0:1 narrows exactly");
            let scaled: Vec<f32> = r.iter().map(|x| x * 65535.0).collect();
            rgb(
                f32_bytes(&scaled, false),
                "Float32",
                r#" bounds="0:65535""#,
                BlockSpec::default(),
            )
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            assert!(!img.is_mapped(), "non-unit bounds cannot be served raw");
            close(&decode(&p), &r, 1e-6);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn big_endian_and_normal_storage_decode() {
            let d = tmp("layout");
            let p = d.join("a.xisf");
            let r = reference();
            let wh = (W * H) as usize;
            let interleaved: Vec<f32> = (0..N)
                .map(|k| r[(k % C as usize) * wh + k / C as usize])
                .collect();
            for (big, normal) in [(true, false), (false, true), (true, true)] {
                let src = if normal { &interleaved } else { &r };
                let mut attrs = String::from(r#" bounds="0:1""#);
                if big {
                    attrs.push_str(r#" byteOrder="big""#);
                }
                if normal {
                    attrs.push_str(r#" pixelStorage="Normal""#);
                }
                rgb(
                    f32_bytes(src, big),
                    "Float32",
                    &attrs,
                    BlockSpec {
                        codec: Some((Codec::Lz4, Some(4), 1 << 20)),
                        ..Default::default()
                    },
                )
                .write(&p);
                assert_eq!(bits(&decode(&p)), bits(&r), "big={big} normal={normal}");
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn embedded_image_block_decodes() {
            let d = tmp("embedded");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    placement: Placement::Embedded,
                    codec: Some((Codec::Zlib, None, 1 << 20)),
                    ..Default::default()
                },
            )
            .write(&p);
            assert_eq!(bits(&decode(&p)), bits(&reference()));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn offset_is_subtracted() {
            let d = tmp("offset");
            let p = d.join("a.xisf");
            let r: Vec<f32> = reference()
                .iter()
                .map(|x| if x.is_nan() { 0.0 } else { *x })
                .collect();
            let shifted: Vec<f32> = r.iter().map(|x| x + 0.25).collect();
            rgb(
                f32_bytes(&shifted, false),
                "Float32",
                r#" bounds="0:1" offset="0.25""#,
                BlockSpec::default(),
            )
            .write(&p);
            close(&decode(&p), &r, 1e-6);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn uint64_complex_and_cielab_are_refused_by_name() {
            let d = tmp("refuse");
            let p = d.join("a.xisf");
            for (fmt, bps, attrs, want) in [
                ("UInt64", 8, "", "UInt64"),
                ("Complex32", 8, "", "Complex32"),
                ("Complex64", 16, "", "Complex64"),
                ("Float32", 4, r#" colorSpace="CIELab""#, "CIELab"),
            ] {
                XisfBuilder {
                    images: vec![ImageSpec {
                        geometry: (2, 2, 1),
                        sample_format: fmt,
                        data: vec![0u8; 4 * bps],
                        attrs: attrs.into(),
                        ..tiny_image()
                    }],
                    ..Default::default()
                }
                .write(&p);
                let e = XisfImage::open(&p).expect_err("must refuse").to_string();
                assert!(e.contains(want), "{fmt}: {e}");
            }
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn mapped_path_still_verifies_checksum() {
            let d = tmp("mapped-cksum");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    checksum: Some("sha-512"),
                    bad_checksum: true,
                    ..Default::default()
                },
            )
            .write(&p);
            let e = XisfImage::open(&p).expect_err("bad checksum").to_string();
            assert!(e.contains("checksum mismatch"), "{e}");
            let img = XisfImage::open_with(
                &p,
                &XisfOpenOptions {
                    verify_checksums: false,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(img.is_mapped());
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn compressed_checksum_mismatch_fails_decode() {
            let d = tmp("cmp-cksum");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    codec: Some((Codec::Zstd, Some(4), 1 << 20)),
                    checksum: Some("sha-256"),
                    bad_checksum: true,
                    ..Default::default()
                },
            )
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            let mut out = vec![0f32; N];
            let e = img.decode_into(&mut out).unwrap_err().to_string();
            assert!(e.contains("checksum mismatch"), "{e}");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn truncated_compressed_image_is_refused() {
            let d = tmp("trunc");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    codec: Some((Codec::Zstd, None, 1 << 20)),
                    ..Default::default()
                },
            )
            .write(&p);
            let len = std::fs::metadata(&p).unwrap().len();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_len(len - 10)
                .unwrap();
            let e = XisfImage::open(&p).expect_err("truncated").to_string();
            assert!(e.contains("past end of file"), "{e}");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn declared_size_must_match_geometry() {
            let d = tmp("size");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    codec: Some((Codec::Zstd, None, 1 << 20)),
                    ..Default::default()
                },
            )
            .write(&p);
            // Rewrite the declared uncompressed size to 2^62 (the header
            // region has room; only the length field changes).
            let mut bytes = std::fs::read(&p).unwrap();
            let hlen = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let xml = String::from_utf8(bytes[16..16 + hlen].to_vec()).unwrap();
            let from = format!(r#"compression="zstd:{}""#, N * 4);
            assert!(xml.contains(&from));
            let xml = xml.replace(&from, r#"compression="zstd:4611686018427387904""#);
            bytes[8..12].copy_from_slice(&(xml.len() as u32).to_le_bytes());
            bytes[16..16 + xml.len()].copy_from_slice(xml.as_bytes());
            std::fs::write(&p, &bytes).unwrap();
            let e = XisfImage::open(&p).expect_err("oversized").to_string();
            assert!(
                e.contains("does not match geometry") || e.contains("expand"),
                "{e}"
            );
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn load_then_planar_rows_matches_decode_into() {
            let d = tmp("load");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    codec: Some((Codec::Lz4Hc, Some(4), 1000)),
                    ..Default::default()
                },
            )
            .write(&p);
            let mut img = XisfImage::open(&p).unwrap();
            img.load().unwrap();
            let mut rows = Vec::new();
            for c in 0..C {
                for y in 0..H {
                    rows.extend_from_slice(crate::formats::PlanarRows::row(&img, c, y));
                }
            }
            assert_eq!(bits(&rows), bits(&decode(&p)));
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        #[should_panic(expected = "XISF pixels not loaded")]
        fn channel_before_load_panics_with_guidance() {
            let d = tmp("pending");
            let p = d.join("a.xisf");
            rgb(
                f32_bytes(&reference(), false),
                "Float32",
                r#" bounds="0:1""#,
                BlockSpec {
                    codec: Some((Codec::Zstd, None, 1 << 20)),
                    ..Default::default()
                },
            )
            .write(&p);
            let img = XisfImage::open(&p).unwrap();
            let _ = img.channel(0);
        }
    }

    mod hostile {
        //! Headers that declare sizes far beyond what the stored bytes can
        //! decompress to must be refused, never allocated (review C1).
        use super::super::*;
        use crate::testkit::tiny_image;

        fn tmp(tag: &str) -> PathBuf {
            let d =
                std::env::temp_dir().join(format!("mpp-xisf-hostile-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// A unit whose root holds `xml` (after one tiny image), with 64
        /// junk bytes at offset 8192 for attachments to point at.
        fn unit(d: &Path, image_xml: Option<&str>, root_xml: &str) -> PathBuf {
            let image = image_xml.map(str::to_string).unwrap_or_else(|| {
                let t = tiny_image();
                format!(
                    r#"<Image geometry="2:2:1" sampleFormat="Float32"{} location="attachment:4096:16"/>"#,
                    t.attrs
                )
            });
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">{image}{root_xml}</xisf>"#
            );
            let mut bytes = b"XISF0100".to_vec();
            bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&[0u8; 4]);
            bytes.extend_from_slice(xml.as_bytes());
            bytes.resize(8192 + 64, 0x5A);
            let p = d.join("h.xisf");
            std::fs::write(&p, bytes).unwrap();
            p
        }

        fn refused(p: &Path) {
            let e = read_header(p).unwrap_err().to_string();
            assert!(
                e.contains("expand") || e.contains("overflow"),
                "read_header: {e}"
            );
            let e = XisfImage::open(p)
                .expect_err("open must refuse")
                .to_string();
            assert!(e.contains("expand") || e.contains("overflow"), "open: {e}");
        }

        #[test]
        fn huge_compressed_image_is_refused() {
            let d = tmp("image");
            let p = unit(
                &d,
                Some(
                    r#"<Image geometry="1073741824:1073741824:1" sampleFormat="Float32" bounds="0:1" compression="zlib:4611686018427387904" location="attachment:8192:12"/>"#,
                ),
                "",
            );
            let e = XisfImage::open(&p)
                .expect_err("open must refuse")
                .to_string();
            assert!(e.contains("expand"), "{e}");
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn huge_numeric_property_is_refused() {
            let d = tmp("vec");
            let p = unit(
                &d,
                None,
                r#"<Property id="V" type="F64Vector" length="576460752303423488" compression="zlib:4611686018427387904" location="attachment:8192:12"/>"#,
            );
            refused(&p);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn huge_attached_string_property_is_refused() {
            let d = tmp("str-att");
            let p = unit(
                &d,
                None,
                r#"<Property id="S" type="String" compression="zstd:4611686018427387904" location="attachment:8192:12"/>"#,
            );
            refused(&p);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn huge_inline_string_property_is_refused() {
            let d = tmp("str-inl");
            let p = unit(
                &d,
                None,
                r#"<Property id="S" type="String" compression="lz4:4611686018427387904" location="inline:base64">AAAAAAAAAAAAAAAA</Property>"#,
            );
            refused(&p);
            std::fs::remove_dir_all(&d).unwrap();
        }

        #[test]
        fn overflowing_vector_length_is_an_error_not_a_panic() {
            let d = tmp("ovf");
            // Non-f64 arrays are lenient: the malformed one reads as Unread.
            let p = unit(
                &d,
                None,
                r#"<Property id="V" type="I32Vector" length="4611686018427387904" location="inline:base64">AAAA</Property>"#,
            );
            let h = read_header(&p).unwrap();
            assert_eq!(h.property("V").unwrap().value, PropertyValue::Unread);
            // F64 arrays are strict: an error, with no overflow panic.
            let p = unit(
                &d,
                None,
                r#"<Property id="V" type="F64Vector" length="4611686018427387904" location="inline:base64">AAAA</Property>"#,
            );
            let e = read_header(&p).unwrap_err().to_string();
            assert!(e.contains("overflow"), "{e}");
            assert!(XisfImage::open(&p).is_err());
            std::fs::remove_dir_all(&d).unwrap();
        }
    }
}
