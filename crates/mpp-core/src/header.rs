//! Image headers: the ordered FITS keywords and XISF properties of a file.
//!
//! [`Header`] is the format-neutral metadata model the readers return beside
//! the pixels ([`formats::read_header`](crate::formats::read_header),
//! [`formats::xisf::read_header`](crate::formats::xisf::read_header),
//! [`formats::fits::read_header`](crate::formats::fits::read_header), or
//! `Header::from(image.header())` for an open [`XisfImage`]). It reuses the
//! existing value models: [`FitsKeyword`] (raw value text, typed on demand
//! by [`FitsKeyword::typed_value`]) and [`XisfProperty`] (scalars, strings,
//! vectors, matrices and times as [`PropertyValue`]s). Decoders such as
//! [`AstrometricSolution::from_header`](crate::astrometry::AstrometricSolution::from_header)
//! read from it.
//!
//! [`XisfImage`]: crate::formats::xisf::XisfImage
//! [`PropertyValue`]: crate::formats::PropertyValue

use crate::formats::xisf::XisfHeader;
use crate::formats::{FitsKeyword, XisfProperty};

/// An XISF property: an id and a typed value. FITS files have none.
pub type Property = XisfProperty;

/// The metadata of an image file: FITS keywords and XISF properties, each in
/// file order.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Header {
    /// FITS keyword cards, in header order. Names may repeat (`COMMENT`,
    /// `HISTORY`).
    pub keywords: Vec<FitsKeyword>,
    /// XISF properties, in header order. Empty for FITS files.
    pub properties: Vec<Property>,
    /// Image `(width, height)` in pixels, when the reader knows it: the
    /// `<Image geometry>` of an XISF file (authoritative, whatever its
    /// `NAXIS` keywords say), or `NAXIS1`/`NAXIS2` of a FITS file.
    #[serde(default)]
    pub image_size: Option<(u32, u32)>,
}

impl Header {
    /// The first keyword named `name`, if any.
    pub fn keyword(&self, name: &str) -> Option<&FitsKeyword> {
        self.keywords.iter().find(|k| k.name == name)
    }

    /// The property with id `id`, if any.
    pub fn property(&self, id: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.id == id)
    }
}

impl From<&XisfHeader> for Header {
    fn from(h: &XisfHeader) -> Self {
        Header {
            keywords: h.fits_keywords.clone(),
            properties: h.properties.clone(),
            image_size: u32::try_from(h.width)
                .ok()
                .zip(u32::try_from(h.height).ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::PropertyValue;

    fn sample() -> Header {
        let kw = |name: &str, value: &str| FitsKeyword {
            name: name.into(),
            value: value.into(),
            comment: "c".into(),
        };
        Header {
            keywords: vec![
                kw("HISTORY", ""),
                kw("FOCALLEN", "1645.0"),
                kw("HISTORY", ""),
            ],
            properties: vec![
                Property {
                    id: "Observation:Time:Start".into(),
                    type_: "TimePoint".into(),
                    value: PropertyValue::Str("2025-05-12T13:15:32.719Z".into()),
                    location: None,
                },
                Property {
                    id: "M".into(),
                    type_: "F64Matrix".into(),
                    value: PropertyValue::F64Mat {
                        rows: 1,
                        cols: 2,
                        data: vec![0.1, -2.5e-7],
                    },
                    location: Some((4096, 16)),
                },
            ],
            image_size: Some((4944, 3284)),
        }
    }

    #[test]
    fn lookups_find_the_first_match() {
        let h = sample();
        assert_eq!(h.keyword("FOCALLEN").unwrap().value, "1645.0");
        assert!(std::ptr::eq(h.keyword("HISTORY").unwrap(), &h.keywords[0]));
        assert!(h.keyword("NOPE").is_none());
        assert_eq!(h.property("M").unwrap().type_, "F64Matrix");
        assert!(h.property("nope").is_none());
    }

    #[test]
    fn image_size_defaults_to_none_when_absent_from_the_encoding() {
        let h: Header = serde_json::from_str(r#"{"keywords":[],"properties":[]}"#).unwrap();
        assert_eq!(h.image_size, None);
    }

    #[test]
    fn round_trips_through_serde() {
        let h = sample();
        let back: Header = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
    }
}
