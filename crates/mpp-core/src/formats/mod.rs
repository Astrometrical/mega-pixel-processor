//! Image format readers and writers.
//!
//! Readers expose per-channel planar `f32` planes over a memory-mapped file,
//! plus passthrough metadata (FITS keywords, XISF properties). Writers
//! stream pixel data out in constant memory.

use std::path::Path;

use crate::header::Header;
use crate::{Error, Result};

pub mod fits;
pub mod fits_write;
#[cfg(feature = "tiff")]
pub mod tiff;
#[cfg(feature = "tiff")]
pub mod tiff_read;
#[cfg(feature = "tiff")]
pub mod tiff_write;
pub mod xisf;
pub mod xisf_block;
pub mod xisf_codec;
pub(crate) mod xisf_pixels;

/// A planar image source the streaming writers can drain row by row.
///
/// The type parameter is the source sample type. `XisfImage` and the DZI
/// path use `PlanarRows<f32>`; the PixInsight worker stages whichever type
/// the session negotiated, so it instantiates this at `u8`, `u16`, `u32`,
/// `f32` or `f64`.
pub trait PlanarRows<T> {
    /// Width in pixels.
    fn width(&self) -> u64;
    /// Height in pixels.
    fn height(&self) -> u64;
    /// Channel count.
    fn channels(&self) -> u64;
    /// One row of one channel (len == width). For `f32` sources, values are
    /// nominally in `[0, 1]`.
    fn row(&self, c: u64, y: u64) -> &[T];
}

/// File format identified from a file's first bytes by [`sniff_format`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// `XISF0100` signature (a monolithic XISF file).
    Xisf,
    /// A FITS `SIMPLE` primary-header card.
    Fits,
    /// A TIFF or BigTIFF byte-order mark (`II*\0`, `MM\0*`, `II+\0`, `MM\0+`).
    Tiff,
}

/// Identify a file's format from its first 8 bytes — the XISF `XISF0100`
/// signature, the FITS `SIMPLE` card's 8-byte keyword field (`SIMPLE`
/// padded with spaces), or a TIFF/BigTIFF byte-order mark — never from the
/// file extension.
///
/// A short read (file under 8 bytes, including empty) is tolerated: it
/// cannot match a signature, so it falls straight through to the "none of
/// these" error rather than propagating an I/O error. A file matching no
/// signature errors naming exactly that, rather than being handed to a
/// reader that would surface an unrelated format-specific complaint (e.g.
/// "no END card") for what may not be that format at all.
pub fn sniff_format(path: &Path) -> Result<FileFormat> {
    use std::io::Read;
    let mut magic = [0u8; 8];
    let mut f = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut n = 0;
    while n < magic.len() {
        match f.read(&mut magic[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io(path, e)),
        }
    }
    let magic = &magic[..n];
    if magic == b"XISF0100" {
        Ok(FileFormat::Xisf)
    } else if magic == b"SIMPLE  " {
        Ok(FileFormat::Fits)
    } else if magic.len() >= 4 && matches!(&magic[..4], b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+") {
        Ok(FileFormat::Tiff)
    } else {
        Err(Error::format(
            path,
            "not an XISF, FITS or TIFF file (no XISF0100 signature, SIMPLE card or TIFF byte-order mark)",
        ))
    }
}

/// The header of the image file at `path`, without reading its pixels.
///
/// Dispatches on the file's signature ([`sniff_format`]), never its
/// extension: XISF and FITS headers are read by [`xisf::read_header`] and
/// [`fits::read_header`]. TIFF files carry neither FITS keywords nor XISF
/// properties that mpp reads, so they yield an empty [`Header`]. Anything
/// else is an [`Error::Format`].
pub fn read_header(path: &Path) -> Result<Header> {
    match sniff_format(path)? {
        FileFormat::Xisf => xisf::read_header(path),
        FileFormat::Fits => fits::read_header(path),
        FileFormat::Tiff => Ok(Header::default()),
    }
}

/// Stored row order of a FITS image, from its `ROWORDER` card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RowOrder {
    /// `ROWORDER = 'TOP-DOWN'`: the first stored row is the top image row
    /// (the internal convention of the readers); rows and WCS cards are used verbatim.
    TopDown,
    /// Standard FITS (`ROWORDER` absent or `'BOTTOM-UP'`): the first stored
    /// row is the bottom image row; rows are flipped on read and the WCS is
    /// reflected into the top-down frame.
    BottomUp,
}

/// Numeric value of the card `name` (case-insensitive), accepting Fortran
/// `D` exponents; `None` when absent or not a number.
pub fn card_number(cards: &[FitsKeyword], name: &str) -> Option<f64> {
    let v = cards.iter().find(|k| k.name.eq_ignore_ascii_case(name))?;
    v.value.trim().replace(['D', 'd'], "E").parse().ok()
}

/// String value of the card `name`: quotes stripped, `''` unescaped,
/// trailing spaces trimmed; `None` when absent or not a quoted string.
pub fn card_string(cards: &[FitsKeyword], name: &str) -> Option<String> {
    let v = cards.iter().find(|k| k.name.eq_ignore_ascii_case(name))?;
    let t = v.value.trim();
    let inner = t.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'").trim_end().to_string())
}

/// A FITS header keyword carried through from input to output.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FitsKeyword {
    /// Keyword name (e.g. `OBJECT`, `CRVAL1`), as written in the header.
    pub name: String,
    /// Raw value text, quoting included for strings (e.g. `'M42'`).
    pub value: String,
    /// Free-text comment, empty when the card has none.
    pub comment: String,
}

impl FitsKeyword {
    /// The value parsed by FITS rules: a quoted string (with `''` unescaped
    /// and trailing blanks dropped), `T`/`F`, an integer, a real (a `D`
    /// exponent is accepted), or none for an empty value (commentary cards).
    /// Text that fits none of these, such as a complex value, is returned as
    /// a string, unchanged.
    pub fn typed_value(&self) -> KeywordValue {
        let v = self.value.trim();
        if v.is_empty() {
            return KeywordValue::None;
        }
        if let Some(inner) = v.strip_prefix('\'') {
            let inner = inner.strip_suffix('\'').unwrap_or(inner);
            return KeywordValue::Str(inner.replace("''", "'").trim_end().to_string());
        }
        match v {
            "T" => return KeywordValue::Bool(true),
            "F" => return KeywordValue::Bool(false),
            _ => {}
        }
        if let Ok(i) = v.parse::<i64>() {
            return KeywordValue::Int(i);
        }
        match v.replace(['D', 'd'], "E").parse::<f64>() {
            Ok(r) => KeywordValue::Real(r),
            Err(_) => KeywordValue::Str(v.to_string()),
        }
    }
}

/// A FITS keyword value parsed by [`FitsKeyword::typed_value`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum KeywordValue {
    /// A string (quotes removed), or unparseable value text.
    Str(String),
    /// An integer.
    Int(i64),
    /// A real number.
    Real(f64),
    /// A logical value (`T` or `F`).
    Bool(bool),
    /// No value (commentary cards such as `COMMENT` and `HISTORY`).
    None,
}

impl KeywordValue {
    /// The value as a float: reals directly, integers converted; else `None`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Real(v) => Some(*v),
            Self::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// The value as a string slice, for strings only.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// One XISF `<Property>` element carried through from the input header.
///
/// PixInsight stores plate solutions (and much other metadata) as properties,
/// not FITS keywords. Values arrive in three shapes, all parsed by the XISF
/// reader:
/// - scalar `value="…"` attributes (Float64, Int*, TimePoint, …),
/// - element text (String properties, and `location="inline:base64"` /
///   `inline:hex` encoded vector/matrix data),
/// - attachment data blocks (`location="attachment:offset:size"`), exposed via
///   [`XisfProperty::location`] and resolved on open for f64 vectors/matrices.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct XisfProperty {
    /// Property identifier (e.g. `PCL:AstrometricSolution:ProjectionSystem`).
    pub id: String,
    /// XISF type name as written in the header (e.g. `Float64`, `F64Vector`).
    pub type_: String,
    /// Decoded value (see [`PropertyValue`] for the shapes).
    pub value: PropertyValue,
    /// Byte offset and size of an attachment-located data block, if any.
    pub location: Option<(u64, u64)>,
}

/// Decoded value of an XISF property.
///
/// Attachment-located vector/matrix properties parse with empty `data`
/// (dimensions from the header attributes); [`xisf::XisfImage::open`] and
/// [`xisf::read_header`] resolve them from the file. `Unread` marks types we
/// do not decode.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PropertyValue {
    /// String-like value (`String`, `TimePoint`, or any `value` attribute of
    /// an undecoded type).
    Str(String),
    /// Floating-point scalar (`Float32`/`Float64`).
    F64(f64),
    /// Integer scalar (all `Int*`/`UInt*` widths, and `Boolean` as 0/1).
    I64(i64),
    /// Vector data of any XISF numeric vector type (`F64Vector`, `I32Vector`,
    /// `F32Vector`, …), converted to f64 in header order; the property's
    /// `type_` records the original type. Integers are exact up to 2^53.
    F64Vec(Vec<f64>),
    /// Matrix data of any XISF numeric matrix type, converted to f64,
    /// row-major; `type_` records the original type.
    F64Mat {
        /// Number of matrix rows.
        rows: u32,
        /// Number of matrix columns.
        cols: u32,
        /// Row-major element data, `rows × cols` long once resolved.
        data: Vec<f64>,
    },
    /// A type this reader does not decode.
    Unread,
}

impl PropertyValue {
    /// The value as a float: `F64` directly, `I64` converted; else `None`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(v) => Some(*v),
            Self::I64(v) => Some(*v as f64),
            _ => None,
        }
    }

    /// The value as a string slice, for `Str` values only.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The value as an f64 slice, for `F64Vec` values only.
    pub fn as_f64_vec(&self) -> Option<&[f64]> {
        match self {
            Self::F64Vec(v) => Some(v),
            _ => None,
        }
    }

    /// The value as `(rows, cols, row-major data)`, for `F64Mat` values only.
    pub fn as_f64_mat(&self) -> Option<(u32, u32, &[f64])> {
        match self {
            Self::F64Mat { rows, cols, data } => Some((*rows, *cols, data)),
            _ => None,
        }
    }

    /// True for f64 vector/matrix values whose data still lives in an
    /// attachment block (they parse with empty `data`; the reader fills them).
    pub fn needs_attachment_data(&self) -> bool {
        match self {
            Self::F64Vec(v) => v.is_empty(),
            Self::F64Mat { data, .. } => data.is_empty(),
            _ => false,
        }
    }
}

/// XISF `sampleFormat` values. Every value parses; the pixel decoders read
/// UInt8/16/32 and Float32/64 and refuse UInt64 and the complex formats by
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // variants are the XISF sampleFormat names verbatim
pub enum SampleFormat {
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float32,
    Float64,
    Complex32,
    Complex64,
}

impl SampleFormat {
    /// Parse an XISF `sampleFormat` attribute value; `None` if unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "UInt8" => Self::UInt8,
            "UInt16" => Self::UInt16,
            "UInt32" => Self::UInt32,
            "UInt64" => Self::UInt64,
            "Float32" => Self::Float32,
            "Float64" => Self::Float64,
            "Complex32" => Self::Complex32,
            "Complex64" => Self::Complex64,
            _ => return None,
        })
    }

    /// Size of one sample of this format, in bytes (a complex sample is
    /// its real and imaginary parts together).
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::UInt8 => 1,
            Self::UInt16 => 2,
            Self::UInt32 | Self::Float32 => 4,
            Self::UInt64 | Self::Float64 | Self::Complex32 => 8,
            Self::Complex64 => 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-formats-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sniff_detects_each_signature() {
        let dir = tmpdir("sniff");
        let cases: [(&[u8], FileFormat); 6] = [
            (b"XISF0100\x10\0\0\0", FileFormat::Xisf),
            (b"SIMPLE  =                    T", FileFormat::Fits),
            (b"II*\0\x08\0\0\0", FileFormat::Tiff),
            (b"MM\0*\0\0\0\x08", FileFormat::Tiff),
            (b"II+\0\x08\0\0\0", FileFormat::Tiff),
            (b"MM\0+\0\x08\0\0", FileFormat::Tiff),
        ];
        for (i, (bytes, want)) in cases.iter().enumerate() {
            let p = dir.join(format!("f{i}.bin"));
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(sniff_format(&p).unwrap(), *want, "case {i}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_rejects_short_and_junk_files() {
        let dir = tmpdir("sniff-junk");
        for (i, bytes) in [&b""[..], &b"II*"[..], &b"hello, world"[..]]
            .iter()
            .enumerate()
        {
            let p = dir.join(format!("j{i}.xisf"));
            std::fs::write(&p, bytes).unwrap();
            let e = sniff_format(&p).unwrap_err().to_string();
            assert!(
                e.contains("not an XISF, FITS or TIFF file"),
                "case {i}: {e}"
            );
        }
        assert!(matches!(
            sniff_format(&dir.join("missing.fits")),
            Err(Error::Io { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signature_beats_extension() {
        let dir = tmpdir("sig-ext");
        let p = dir.join("really-xisf.fits");
        crate::testkit::synth_xisf(&p, 4, 3, 1, true);
        let h = read_header(&p).unwrap();
        assert!(
            h.property("PCL:AstrometricSolution:ReferenceCelestialCoordinates")
                .is_some(),
            "XISF properties expected from a .fits-named XISF file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn property_value_accessors() {
        assert_eq!(PropertyValue::F64(1.5).as_f64(), Some(1.5));
        assert_eq!(PropertyValue::I64(-7).as_f64(), Some(-7.0));
        assert_eq!(PropertyValue::Str("x".into()).as_str(), Some("x"));
        assert_eq!(
            PropertyValue::F64Vec(vec![1.0, 2.0]).as_f64_vec(),
            Some(&[1.0, 2.0][..])
        );
        let m = PropertyValue::F64Mat {
            rows: 2,
            cols: 2,
            data: vec![1.0, 0.0, 0.0, 1.0],
        };
        assert_eq!(m.as_f64_mat(), Some((2, 2, &[1.0, 0.0, 0.0, 1.0][..])));
        assert!(PropertyValue::F64Vec(Vec::new()).needs_attachment_data());
        assert!(!PropertyValue::F64(0.0).needs_attachment_data());
    }

    fn kw(value: &str) -> FitsKeyword {
        FitsKeyword {
            name: "K".into(),
            value: value.into(),
            comment: String::new(),
        }
    }

    #[test]
    fn keyword_values_parse_by_fits_rules() {
        assert_eq!(kw("'M42 '").typed_value(), KeywordValue::Str("M42".into()));
        assert_eq!(
            kw("'O''Neil'").typed_value(),
            KeywordValue::Str("O'Neil".into())
        );
        assert_eq!(kw("T").typed_value(), KeywordValue::Bool(true));
        assert_eq!(kw("F").typed_value(), KeywordValue::Bool(false));
        assert_eq!(kw("-32").typed_value(), KeywordValue::Int(-32));
        assert_eq!(kw("1645.0").typed_value(), KeywordValue::Real(1645.0));
        assert_eq!(kw("1.5D3").typed_value(), KeywordValue::Real(1500.0));
        assert_eq!(kw("").typed_value(), KeywordValue::None);
        assert_eq!(
            kw("(1, 2)").typed_value(),
            KeywordValue::Str("(1, 2)".into())
        );
        assert_eq!(kw("90").typed_value().as_f64(), Some(90.0));
        assert_eq!(kw("'x'").typed_value().as_str(), Some("x"));
    }

    #[test]
    fn read_header_dispatches_on_extension() {
        let dir = std::env::temp_dir().join(format!("mpp-read-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tif = dir.join("a.TIF");
        std::fs::write(&tif, b"II*\0").unwrap();
        assert_eq!(read_header(&tif).unwrap(), Header::default());
        assert!(read_header(&dir.join("missing.tif")).is_err());
        let png = dir.join("a.png");
        std::fs::write(&png, b"x").unwrap();
        assert!(read_header(&png).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sample_format_parse_and_size() {
        assert_eq!(SampleFormat::parse("Float32"), Some(SampleFormat::Float32));
        assert_eq!(SampleFormat::parse("nope"), None);
        assert_eq!(SampleFormat::Float32.bytes_per_sample(), 4);
        assert_eq!(SampleFormat::UInt16.bytes_per_sample(), 2);
        assert_eq!(SampleFormat::Float64.bytes_per_sample(), 8);
    }
}
