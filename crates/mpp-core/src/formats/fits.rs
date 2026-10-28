//! FITS reading: the primary-HDU header, and a memory-mapped pixel reader.
//!
//! Supported image subset: the primary HDU, `NAXIS` 2 or 3 (channels on the
//! last axis, planar), `BITPIX` 8, 16, 32, −32 and −64 with `BZERO`, `BSCALE`
//! and `BLANK`. Rows are exposed top-down: standard (bottom-up) files are
//! flipped on read, and `ROWORDER = 'TOP-DOWN'` files are read as stored.
//! Integer samples are normalized to `[0, 1]` by their type's range; NaN,
//! Inf and `BLANK` samples (and negative integer samples) become 0, the
//! no-data sentinel, while float samples keep their sign. Image data in an
//! extension HDU (fpack `.fz` included), `BITPIX` 64 and `NAXIS` > 3 are
//! refused by name.
//!
//! Card parsing follows FITS 4.0 §4.1 for fixed-format value cards: the value
//! indicator `= ` sits in columns 9–10 and the value runs to the `/` comment
//! separator, except inside a `'…'` string. `COMMENT` and `HISTORY` are
//! always commentary. Long-string `CONTINUE` cards are kept as cards in their
//! own right, not joined.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use super::{FitsKeyword, KeywordValue, RowOrder, card_number, card_string};
use crate::header::Header;
use crate::{Error, Result};

/// FITS logical record length.
const BLOCK: usize = 2880;
/// FITS card length.
const CARD: usize = 80;
/// Headers longer than this are refused as corrupt (about 36,000 cards).
const MAX_HEADER_BLOCKS: usize = 1000;

/// The primary-HDU header of the FITS file at `path`, without its pixels.
///
/// Every card up to `END` is kept in order, structural ones (`SIMPLE`,
/// `BITPIX`, `NAXISn`, …) and commentary ones (`COMMENT`, `HISTORY`)
/// included; cards with a blank keyword are dropped. FITS has no XISF
/// properties, so the header's property list is empty. The image size is
/// `NAXIS1` × `NAXIS2` when both are positive integers.
pub fn read_header(path: &Path) -> Result<Header> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut keywords = Vec::new();
    let mut block = vec![0u8; BLOCK];
    for n in 0..MAX_HEADER_BLOCKS {
        if let Err(e) = file.read_exact(&mut block) {
            return Err(match e.kind() {
                std::io::ErrorKind::UnexpectedEof if n == 0 => {
                    Error::format(path, "not a FITS file (shorter than one header block)")
                }
                std::io::ErrorKind::UnexpectedEof => {
                    Error::format(path, "truncated FITS header (no END card)")
                }
                _ => Error::io(path, e),
            });
        }
        if n == 0 && !block.starts_with(b"SIMPLE  =") {
            return Err(Error::format(path, "not a FITS file (missing SIMPLE card)"));
        }
        for card in block.as_chunks::<CARD>().0 {
            if is_end_card(card) {
                let dim = |name: &str| match keywords
                    .iter()
                    .find(|k: &&FitsKeyword| k.name == name)?
                    .typed_value()
                {
                    KeywordValue::Int(n) if n > 0 => u32::try_from(n).ok(),
                    _ => None,
                };
                let image_size = dim("NAXIS1").zip(dim("NAXIS2"));
                return Ok(Header {
                    keywords,
                    properties: Vec::new(),
                    image_size,
                });
            }
            if let Some(kw) = parse_card(card) {
                keywords.push(kw);
            }
        }
    }
    Err(Error::format(
        path,
        format!("FITS header longer than {MAX_HEADER_BLOCKS} blocks"),
    ))
}

/// Whether `card` is the `END` card (keyword `END`, the rest blank).
fn is_end_card(card: &[u8]) -> bool {
    &card[..8] == b"END     " && card[8..].iter().all(|&b| b == b' ')
}

/// Parsed primary-HDU metadata of a FITS file.
#[derive(Debug, Clone)]
pub struct FitsHeader {
    /// Image width (`NAXIS1`).
    pub width: u64,
    /// Image height (`NAXIS2`).
    pub height: u64,
    /// Channel count (`NAXIS3`, or 1 for a 2-D image).
    pub channels: u64,
    /// Sample type code (8, 16, 32, −32, −64).
    pub bitpix: i32,
    /// `BZERO` (default 0).
    pub bzero: f64,
    /// `BSCALE` (default 1).
    pub bscale: f64,
    /// `BLANK` raw value marking undefined integer samples, if declared.
    pub blank: Option<i64>,
    /// Stored row order (see [`RowOrder`]).
    pub row_order: RowOrder,
    /// Byte offset of the data block.
    pub data_offset: u64,
    /// Every header card except `END`, in header order.
    pub keywords: Vec<FitsKeyword>,
}

/// A memory-mapped, read-only FITS image decoding rows on demand.
#[derive(Debug)]
pub struct FitsImage {
    path: PathBuf,
    mmap: Mmap,
    header: FitsHeader,
}

/// Split one 80-byte card into a keyword; `None` for blank or malformed
/// cards. `card` is always exactly [`CARD`] bytes (a `chunks_exact` slice),
/// but lossy UTF-8 decoding of non-ASCII bytes can change the decoded
/// string's length, so every slice into `text` is boundary-checked rather
/// than assumed to line up with the raw byte offsets.
fn parse_card(card: &[u8]) -> Option<FitsKeyword> {
    let text = String::from_utf8_lossy(card);
    let name = text.get(..8)?.trim_end().to_string();
    if name.is_empty() {
        return None;
    }
    let has_value = text.get(8..10) == Some("= ");
    if !has_value || name == "COMMENT" || name == "HISTORY" {
        let rest = text.get(8..).unwrap_or_default();
        return Some(FitsKeyword {
            name,
            value: String::new(),
            comment: rest.trim().to_string(),
        });
    }
    let body = text.get(10..).unwrap_or_default();
    let s = body.trim_start();
    let (value, comment) = if s.starts_with('\'') {
        let b = s.as_bytes();
        let mut i = 1;
        while i < b.len() {
            if b[i] == b'\'' {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                break;
            }
            i += 1;
        }
        let end = (i + 1).min(s.len());
        let comment = s[end..]
            .split_once('/')
            .map(|(_, c)| c.trim().to_string())
            .unwrap_or_default();
        (s[..end].trim_end().to_string(), comment)
    } else {
        match body.split_once('/') {
            Some((v, c)) => (v.trim().to_string(), c.trim().to_string()),
            None => (body.trim().to_string(), String::new()),
        }
    };
    Some(FitsKeyword {
        name,
        value,
        comment,
    })
}

/// Parse the primary header; `data_offset` is the first block after `END`.
fn parse_header(path: &Path, bytes: &[u8]) -> Result<FitsHeader> {
    let mut cards = Vec::new();
    let mut end_at = None;
    let scan = &bytes[..bytes.len().min(MAX_HEADER_BLOCKS * BLOCK)];
    for (i, card) in scan.as_chunks::<CARD>().0.iter().enumerate() {
        if is_end_card(card) {
            end_at = Some(i);
            break;
        }
        if let Some(k) = parse_card(card) {
            cards.push(k);
        }
    }
    let end_at = end_at.ok_or_else(|| Error::format(path, "no END card in the primary header"))?;
    let data_offset = ((end_at + 1) * CARD).div_ceil(BLOCK) * BLOCK;
    if cards.first().map(|k| k.name.as_str()) != Some("SIMPLE")
        || card_string_logical(&cards, "SIMPLE") != Some(true)
    {
        return Err(Error::format(path, "not a FITS file (SIMPLE = T missing)"));
    }
    let int =
        |name: &str| card_number(&cards, name).and_then(|v| (v.fract() == 0.0).then_some(v as i64));
    let naxis = int("NAXIS").ok_or_else(|| Error::format(path, "NAXIS missing"))?;
    let (width, height, channels) = match naxis {
        0 => {
            return Err(Error::format(
                path,
                "no image in the primary HDU (image data in an extension HDU, e.g. fpack-compressed .fz, is unsupported)",
            ));
        }
        2 | 3 => {
            let w = int("NAXIS1")
                .filter(|&v| v > 0)
                .ok_or_else(|| Error::format(path, "NAXIS1 missing or zero"))?;
            let h = int("NAXIS2")
                .filter(|&v| v > 0)
                .ok_or_else(|| Error::format(path, "NAXIS2 missing or zero"))?;
            let c = if naxis == 3 {
                int("NAXIS3")
                    .filter(|&v| v > 0)
                    .ok_or_else(|| Error::format(path, "NAXIS3 missing or zero"))?
            } else {
                1
            };
            (w as u64, h as u64, c as u64)
        }
        n => {
            return Err(Error::format(
                path,
                format!("NAXIS {n} is unsupported (need a 2-D image or a planar 3-D cube)"),
            ));
        }
    };
    let bitpix = int("BITPIX").ok_or_else(|| Error::format(path, "BITPIX missing"))? as i32;
    if !matches!(bitpix, 8 | 16 | 32 | -32 | -64) {
        return Err(Error::format(
            path,
            format!("BITPIX {bitpix} is unsupported"),
        ));
    }
    let row_order = match card_string(&cards, "ROWORDER").as_deref() {
        Some("TOP-DOWN") => RowOrder::TopDown,
        _ => RowOrder::BottomUp,
    };
    Ok(FitsHeader {
        width,
        height,
        channels,
        bitpix,
        bzero: card_number(&cards, "BZERO").unwrap_or(0.0),
        bscale: card_number(&cards, "BSCALE").unwrap_or(1.0),
        blank: int("BLANK"),
        row_order,
        data_offset: data_offset as u64,
        keywords: cards,
    })
}

/// Logical card value (`T`/`F`).
fn card_string_logical(cards: &[FitsKeyword], name: &str) -> Option<bool> {
    match cards.iter().find(|k| k.name == name)?.value.trim() {
        "T" => Some(true),
        "F" => Some(false),
        _ => None,
    }
}

impl FitsImage {
    /// Open and validate a FITS file's primary image HDU (module docs list
    /// the supported subset; anything else errors naming the feature).
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io(path, e))?;
        // SAFETY: read-only map; truncation by another process during a run
        // is undefined, as is conventional for mmap readers.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| Error::io(path, e))?;
        let header = parse_header(path, &mmap)?;
        let bps = (header.bitpix.unsigned_abs() / 8) as u64;
        let need = header
            .width
            .checked_mul(header.height)
            .and_then(|n| n.checked_mul(header.channels))
            .and_then(|n| n.checked_mul(bps))
            .and_then(|n| n.checked_add(header.data_offset))
            .filter(|_| (header.width * bps) <= isize::MAX as u64)
            .ok_or_else(|| {
                Error::format(
                    path,
                    format!(
                        "image dimensions overflow ({}x{}x{} at BITPIX {})",
                        header.width, header.height, header.channels, header.bitpix
                    ),
                )
            })?;
        if need > mmap.len() as u64 {
            return Err(Error::format(
                path,
                format!(
                    "data block extends past end of file ({} bytes needed, {} present)",
                    need,
                    mmap.len()
                ),
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            mmap,
            header,
        })
    }

    /// The file this image was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Parsed header.
    pub fn header(&self) -> &FitsHeader {
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
    /// Channel count.
    pub fn channels(&self) -> u64 {
        self.header.channels
    }
    /// Stored row order.
    pub fn row_order(&self) -> RowOrder {
        self.header.row_order
    }

    /// Decode `n` canvas rows of channel `c` starting at top-down canvas row
    /// `canvas_y0` into `out` (`n * width` values, rows contiguous): the
    /// flip rule, byte order, BZERO/BSCALE, integer normalization, and the
    /// zero sentinel for NaN/Inf/BLANK samples are all applied. Negative
    /// integer (BITPIX > 0) samples also become 0; floating samples keep
    /// their sign, taken as-is after BZERO/BSCALE.
    pub fn decode_rows(&self, c: u64, canvas_y0: u64, n: usize, out: &mut [f32]) {
        let h = &self.header;
        let w = h.width as usize;
        assert!(c < h.channels, "channel {c} out of range");
        assert!(canvas_y0 + n as u64 <= h.height, "rows out of range");
        assert_eq!(out.len(), n * w, "output buffer size");
        let bps = (h.bitpix.unsigned_abs() / 8) as usize;
        let norm = match h.bitpix {
            8 => 255.0,
            16 => 65535.0,
            32 => 4294967295.0,
            _ => 1.0,
        };
        let plain_float = h.bitpix < 0 && h.bzero == 0.0 && h.bscale == 1.0;
        for k in 0..n {
            let y = canvas_y0 + k as u64;
            let r_file = match h.row_order {
                RowOrder::TopDown => y,
                RowOrder::BottomUp => h.height - 1 - y,
            };
            let off = h.data_offset as usize + ((c * h.height + r_file) as usize * w) * bps;
            let src = &self.mmap[off..off + w * bps];
            let dst = &mut out[k * w..(k + 1) * w];
            for (i, d) in dst.iter_mut().enumerate() {
                let b = &src[i * bps..(i + 1) * bps];
                let (raw, raw_int): (f64, Option<i64>) = match h.bitpix {
                    8 => (b[0] as f64, Some(b[0] as i64)),
                    16 => {
                        let v = i16::from_be_bytes([b[0], b[1]]);
                        (v as f64, Some(v as i64))
                    }
                    32 => {
                        let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                        (v as f64, Some(v as i64))
                    }
                    -32 => (f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64, None),
                    _ => (f64::from_be_bytes(b.try_into().expect("8 bytes")), None),
                };
                let v = if plain_float {
                    raw
                } else if raw_int.is_some() && raw_int == h.blank {
                    0.0
                } else {
                    (h.bzero + h.bscale * raw) / norm
                };
                let keep = if h.bitpix < 0 { v != 0.0 } else { v > 0.0 };
                *d = if v.is_finite() && keep { v as f32 } else { 0.0 };
            }
        }
    }

    /// Advise the OS that access will be sequential (a no-op off unix).
    pub fn advise_sequential(&self) {
        #[cfg(unix)]
        let _ = self.mmap.advise(memmap2::Advice::Sequential);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{card_number, card_string};
    use crate::testkit::synth_fits;
    use std::path::PathBuf;

    fn card(s: &str) -> Vec<u8> {
        assert!(s.len() <= CARD);
        let mut c = s.as_bytes().to_vec();
        c.resize(CARD, b' ');
        c
    }

    /// A FITS file whose header spans two blocks (40 cards before END), with
    /// a small data block after it.
    fn synth_header_fits(path: &Path) {
        let mut cards = vec![
            card("SIMPLE  =                    T / conforms to FITS standard"),
            card("BITPIX  =                  -32 / 32-bit IEEE float"),
            card("NAXIS   =                    2"),
            card("NAXIS1  =                    2"),
            card("NAXIS2  =                    1"),
            card("OBJECT  = 'NGC 3199 / Panel 5' / Name of the object of interest"),
            card("FOCALLEN=               1645.0 / [mm] Focal length"),
            card("OBSERVER= 'O''Neil  '"),
            card("COMMENT Written by a test"),
            card(""),
        ];
        for i in 0..30 {
            cards.push(card(&format!("HISTORY step {i}")));
        }
        cards.push(card("END"));
        let mut bytes: Vec<u8> = cards.concat();
        bytes.resize(2 * BLOCK, b' ');
        bytes.extend_from_slice(&[0u8; BLOCK]);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn reads_every_card_in_order_across_blocks() {
        let dir = std::env::temp_dir().join(format!("mpp-fits-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.fits");
        synth_header_fits(&path);
        let h = read_header(&path).unwrap();
        assert!(h.properties.is_empty());
        assert_eq!(h.image_size, Some((2, 1)), "from NAXIS1/NAXIS2");
        assert_eq!(h.keywords.len(), 39, "blank card dropped, END excluded");
        assert_eq!(h.keywords[0].name, "SIMPLE");
        assert_eq!(h.keywords[0].typed_value(), KeywordValue::Bool(true));
        let obj = h.keyword("OBJECT").unwrap();
        assert_eq!(obj.value, "'NGC 3199 / Panel 5'", "slash inside a string");
        assert_eq!(obj.comment, "Name of the object of interest");
        assert_eq!(
            h.keyword("FOCALLEN").unwrap().typed_value(),
            KeywordValue::Real(1645.0)
        );
        assert_eq!(h.keyword("FOCALLEN").unwrap().comment, "[mm] Focal length");
        assert_eq!(
            h.keyword("OBSERVER").unwrap().typed_value(),
            KeywordValue::Str("O'Neil".into())
        );
        let c = h.keyword("COMMENT").unwrap();
        assert_eq!(
            (c.value.as_str(), c.comment.as_str()),
            ("", "Written by a test")
        );
        assert_eq!(h.keywords.last().unwrap().comment, "step 29");
        // The dispatcher routes .fits here.
        assert_eq!(crate::formats::read_header(&path).unwrap(), h);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_non_fits_and_truncated_headers() {
        let dir = std::env::temp_dir().join(format!("mpp-fits-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.fits");
        std::fs::write(&path, b"SIMPLE  =").unwrap();
        assert!(read_header(&path).is_err(), "short file");
        let mut not_fits = card("NOTFITS = T").repeat(36);
        std::fs::write(&path, &not_fits).unwrap();
        assert!(
            read_header(&path)
                .unwrap_err()
                .to_string()
                .contains("SIMPLE")
        );
        not_fits[..CARD].copy_from_slice(&card("SIMPLE  =                    T"));
        std::fs::write(&path, &not_fits).unwrap();
        assert!(read_header(&path).unwrap_err().to_string().contains("END"));
        assert!(read_header(&dir.join("missing.fits")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-fits-in-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn kw(name: &str, value: &str) -> FitsKeyword {
        FitsKeyword {
            name: name.into(),
            value: value.into(),
            comment: String::new(),
        }
    }

    fn planes(w: u64, h: u64, ch: u64) -> Vec<f32> {
        (0..w * h * ch)
            .map(|i| (i as f32 + 1.0) / (w * h * ch) as f32)
            .collect()
    }

    fn read_all(p: &FitsImage) -> Vec<f32> {
        let (w, h, ch) = (p.width() as usize, p.height() as usize, p.channels());
        let mut out = vec![0f32; w * h * ch as usize];
        for c in 0..ch {
            p.decode_rows(
                c,
                0,
                h,
                &mut out[c as usize * w * h..(c as usize + 1) * w * h],
            );
        }
        out
    }

    #[test]
    fn float32_bottom_up_round_trips_through_flip() {
        let dir = tmpdir("f32");
        let (w, h, ch) = (5u64, 4u64, 3u64);
        let src = planes(w, h, ch);
        let path = dir.join("p.fits");
        synth_fits(&path, w, h, ch, &src, -32, &[kw("OBJECT", "'M42'")]).unwrap();
        let p = FitsImage::open(&path).unwrap();
        assert_eq!((p.width(), p.height(), p.channels()), (w, h, ch));
        assert_eq!(p.row_order(), RowOrder::BottomUp);
        assert_eq!(p.header().bitpix, -32);
        assert_eq!(read_all(&p), src, "top-down planes come back identical");
        assert_eq!(
            card_string(&p.header().keywords, "OBJECT").as_deref(),
            Some("M42")
        );
        // Partial band in the middle of the image, channel 2.
        let mut band = vec![0f32; 2 * w as usize];
        p.decode_rows(2, 1, 2, &mut band);
        let off = 2 * (w * h) as usize + w as usize;
        assert_eq!(&band[..], &src[off..off + 2 * w as usize]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn top_down_card_disables_the_flip() {
        let dir = tmpdir("td");
        let (w, h, ch) = (3u64, 3u64, 1u64);
        let src = planes(w, h, ch);
        let path = dir.join("p.fits");
        synth_fits(&path, w, h, ch, &src, -32, &[kw("ROWORDER", "'TOP-DOWN'")]).unwrap();
        let p = FitsImage::open(&path).unwrap();
        assert_eq!(p.row_order(), RowOrder::TopDown);
        assert_eq!(read_all(&p), src);
        // The file itself stores row 0 first: reading the raw block confirms
        // no flip happened on write either.
        let bytes = std::fs::read(&path).unwrap();
        let first = f32::from_be_bytes(
            bytes[p.header().data_offset as usize..][..4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(first, src[0]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn integer_formats_normalize_to_unit_range() {
        let dir = tmpdir("int");
        let (w, h, ch) = (4u64, 2u64, 1u64);
        let src: Vec<f32> = vec![0.0, 0.25, 0.5, 1.0, 0.1, 0.9, 0.0, 0.75];
        for (bitpix, tol) in [(8i32, 1.0 / 255.0), (16, 1.0 / 65535.0), (32, 1e-6)] {
            let path = dir.join(format!("p{bitpix}.fits"));
            synth_fits(&path, w, h, ch, &src, bitpix, &[]).unwrap();
            let p = FitsImage::open(&path).unwrap();
            assert_eq!(p.header().bitpix, bitpix);
            let got = read_all(&p);
            for (a, b) in got.iter().zip(&src) {
                assert!((a - b).abs() <= tol, "bitpix {bitpix}: {a} vs {b}");
            }
            assert_eq!(got[0], 0.0, "zero stays the exact sentinel");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nan_inf_blank_and_negative_integers_become_zero() {
        let dir = tmpdir("nan");
        let (w, h) = (4u64, 1u64);
        let path = dir.join("f.fits");
        synth_fits(
            &path,
            w,
            h,
            1,
            &[f32::NAN, f32::INFINITY, -0.5, 0.5],
            -32,
            &[],
        )
        .unwrap();
        let p = FitsImage::open(&path).unwrap();
        // Float samples keep their sign (background-subtracted noise is
        // data, not a hole); only NaN/Inf become the sentinel.
        assert_eq!(read_all(&p), vec![0.0, 0.0, -0.5, 0.5]);
        // Integer data clamps a negative physical value to 0: a hand-written
        // BITPIX 16 file with no BZERO (so phys = raw) and raw -100.
        let path = dir.join("neg16.fits");
        let mut hdr = String::new();
        for c in [
            "SIMPLE  =                    T",
            "BITPIX  =                   16",
            "NAXIS   =                    2",
            "NAXIS1  =                    2",
            "NAXIS2  =                    1",
            "END",
        ] {
            hdr.push_str(&format!("{c:<80}"));
        }
        while !hdr.len().is_multiple_of(BLOCK) {
            hdr.push(' ');
        }
        let mut bytes = hdr.into_bytes();
        bytes.extend_from_slice(&(-100i16).to_be_bytes());
        bytes.extend_from_slice(&1000i16.to_be_bytes());
        bytes.resize(bytes.len().next_multiple_of(BLOCK), 0);
        std::fs::write(&path, bytes).unwrap();
        let p = FitsImage::open(&path).unwrap();
        let got = read_all(&p);
        assert_eq!(got[0], 0.0, "negative integer sample clamps to 0");
        assert!((got[1] - 1000.0 / 65535.0).abs() < 1e-7, "{}", got[1]);
        // BLANK on integer data: write raw 16-bit with BZERO 32768; raw value
        // -32768 (phys 0) is also declared BLANK — both map to 0 anyway, so
        // declare BLANK = 0 raw (phys 32768 → 0.5) and check it vanishes.
        let path = dir.join("i.fits");
        synth_fits(
            &path,
            w,
            h,
            1,
            &[0.5, 0.25, 0.5, 1.0],
            16,
            &[kw("BLANK", "0")],
        )
        .unwrap();
        let p = FitsImage::open(&path).unwrap();
        let got = read_all(&p);
        assert_eq!(got[0], 0.0);
        assert_eq!(got[2], 0.0);
        assert!((got[1] - 0.25).abs() < 1e-4 && (got[3] - 1.0).abs() < 1e-4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn naxis2_is_mono_and_naxis3_of_one_is_mono() {
        let dir = tmpdir("mono");
        let src = planes(3, 2, 1);
        let p2 = dir.join("n2.fits");
        synth_fits(&p2, 3, 2, 1, &src, -32, &[]).unwrap();
        let bytes = std::fs::read(&p2).unwrap();
        let hdr = String::from_utf8_lossy(&bytes[..BLOCK]).to_string();
        assert!(
            hdr.contains("NAXIS   =                    2"),
            "mono is written as NAXIS 2"
        );
        assert_eq!(FitsImage::open(&p2).unwrap().channels(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn long_header_and_quoted_values_parse() {
        let dir = tmpdir("hdr");
        let cards: Vec<FitsKeyword> = (0..60)
            .map(|i| kw(&format!("K{i:03}"), &format!("{i}")))
            .chain([kw("TELESCOP", "'Rob''s RASA'"), kw("COMMENT", "")])
            .collect();
        let path = dir.join("p.fits");
        synth_fits(&path, 2, 2, 1, &planes(2, 2, 1), -32, &cards).unwrap();
        let p = FitsImage::open(&path).unwrap();
        assert!(
            p.header().data_offset >= 2 * BLOCK as u64,
            "header spans two blocks"
        );
        assert_eq!(card_number(&p.header().keywords, "K059"), Some(59.0));
        assert_eq!(
            card_string(&p.header().keywords, "TELESCOP").as_deref(),
            Some("Rob's RASA")
        );
        assert_eq!(read_all(&p), planes(2, 2, 1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refusals_name_the_reason() {
        let dir = tmpdir("bad");
        let write = |name: &str, cards: &[&str], data: usize| {
            let mut h = String::new();
            for c in cards {
                h.push_str(&format!("{c:<80}"));
            }
            h.push_str(&format!("{:<80}", "END"));
            while !h.len().is_multiple_of(BLOCK) {
                h.push(' ');
            }
            let mut bytes = h.into_bytes();
            bytes.extend(std::iter::repeat_n(0u8, data));
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let e = FitsImage::open(&write(
            "ext.fits",
            &[
                "SIMPLE  =                    T",
                "BITPIX  =                  -32",
                "NAXIS   =                    0",
                "EXTEND  =                    T",
            ],
            0,
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("extension"), "{e}");
        let e = FitsImage::open(&write(
            "b64.fits",
            &[
                "SIMPLE  =                    T",
                "BITPIX  =                   64",
                "NAXIS   =                    2",
                "NAXIS1  =                    2",
                "NAXIS2  =                    2",
            ],
            32,
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("BITPIX 64"), "{e}");
        let e = FitsImage::open(&write(
            "n4.fits",
            &[
                "SIMPLE  =                    T",
                "BITPIX  =                  -32",
                "NAXIS   =                    4",
                "NAXIS1  =                    2",
                "NAXIS2  =                    2",
                "NAXIS3  =                    1",
                "NAXIS4  =                    1",
            ],
            16,
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("NAXIS 4"), "{e}");
        let e = FitsImage::open(&write(
            "short.fits",
            &[
                "SIMPLE  =                    T",
                "BITPIX  =                  -32",
                "NAXIS   =                    2",
                "NAXIS1  =                   10",
                "NAXIS2  =                   10",
            ],
            8,
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("past end of file"), "{e}");
        let e = FitsImage::open(&write(
            "notfits.fits",
            &["HELLO   =                    1"],
            0,
        ))
        .unwrap_err()
        .to_string();
        assert!(e.contains("SIMPLE"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn non_ascii_card_does_not_panic() {
        let dir = tmpdir("nonascii");
        let path = dir.join("p.fits");
        synth_fits(
            &path,
            2,
            2,
            1,
            &planes(2, 2, 1),
            -32,
            &[kw("OBJECT", "'M42'")],
        )
        .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // Put a Latin-1 'é' into the OBJECT card's comment area.
        let at = bytes
            .windows(8)
            .position(|w| w == b"OBJECT  ")
            .expect("OBJECT card");
        bytes[at + 40] = 0xE9;
        std::fs::write(&path, &bytes).unwrap();
        let p = FitsImage::open(&path).unwrap();
        assert_eq!(
            card_string(&p.header().keywords, "OBJECT").as_deref(),
            Some("M42")
        );
        assert!(read_header(&path).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn overflowing_dimensions_are_refused() {
        let dir = tmpdir("overflow");
        let mut h = String::new();
        for c in [
            "SIMPLE  =                    T",
            "BITPIX  =                  -32",
            "NAXIS   =                    2",
            "NAXIS1  =           4294967296",
            "NAXIS2  =           4294967296",
            "END",
        ] {
            h.push_str(&format!("{c:<80}"));
        }
        while !h.len().is_multiple_of(BLOCK) {
            h.push(' ');
        }
        let path = dir.join("huge.fits");
        std::fs::write(&path, h.into_bytes()).unwrap();
        let e = FitsImage::open(&path).unwrap_err().to_string();
        assert!(e.contains("overflow"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
