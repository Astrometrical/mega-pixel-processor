//! Image format readers and writers.
//!
//! Readers expose per-channel planar `f32` planes over a memory-mapped file,
//! plus passthrough metadata (FITS keywords, XISF properties). Writers
//! stream pixel data out in constant memory.

pub mod tiff;
pub mod tiff_read;
pub mod tiff_write;
pub mod xisf;

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

/// A FITS header keyword carried through from input to output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FitsKeyword {
    /// Keyword name (e.g. `OBJECT`, `CRVAL1`), as written in the header.
    pub name: String,
    /// Raw value text, quoting included for strings (e.g. `'M42'`).
    pub value: String,
    /// Free-text comment, empty when the card has none.
    pub comment: String,
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
/// Attachment-located `F64Vector`/`F64Matrix` properties parse with empty
/// `data` (dimensions from the header attributes); `XisfPanel::open` resolves
/// them from the file. `Unread` marks types we do not decode.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PropertyValue {
    /// String-like value (`String`, `TimePoint`, or any `value` attribute of
    /// an undecoded type).
    Str(String),
    /// Floating-point scalar (`Float32`/`Float64`).
    F64(f64),
    /// Integer scalar (all `Int*`/`UInt*` widths, and `Boolean` as 0/1).
    I64(i64),
    /// `F64Vector` data, in header order.
    F64Vec(Vec<f64>),
    /// `F64Matrix` data, row-major.
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

/// Sample formats we recognize in headers. Only `Float32` is readable for now —
/// MosaicByCoordinates output is always Float32/Float64, and Float32 is the
/// overwhelmingly common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // variants are the XISF sampleFormat names verbatim
pub enum SampleFormat {
    UInt8,
    UInt16,
    UInt32,
    Float32,
    Float64,
}

impl SampleFormat {
    /// Parse an XISF `sampleFormat` attribute value; `None` if unrecognized.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "UInt8" => Self::UInt8,
            "UInt16" => Self::UInt16,
            "UInt32" => Self::UInt32,
            "Float32" => Self::Float32,
            "Float64" => Self::Float64,
            _ => return None,
        })
    }

    /// Size of one sample of this format, in bytes.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::UInt8 => 1,
            Self::UInt16 => 2,
            Self::UInt32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn sample_format_parse_and_size() {
        assert_eq!(SampleFormat::parse("Float32"), Some(SampleFormat::Float32));
        assert_eq!(SampleFormat::parse("nope"), None);
        assert_eq!(SampleFormat::Float32.bytes_per_sample(), 4);
        assert_eq!(SampleFormat::UInt16.bytes_per_sample(), 2);
        assert_eq!(SampleFormat::Float64.bytes_per_sample(), 8);
    }
}
