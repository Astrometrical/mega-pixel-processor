//! FITS header reading (primary HDU).
//!
//! Only the header is read here: mpp-core has no FITS pixel reader yet. The
//! card parsing follows FITS 4.0 §4.1 for fixed-format value cards: the value
//! indicator `= ` sits in columns 9–10 and the value runs to the `/` comment
//! separator, except inside a `'…'` string. Long-string `CONTINUE` cards are
//! kept as cards in their own right, not joined.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::{FitsKeyword, KeywordValue};
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
/// included; blank cards are dropped. FITS has no XISF properties, so the
/// header's property list is empty. The image size is `NAXIS1` × `NAXIS2`
/// when both are positive integers.
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
            let kw = parse_card(card);
            if kw.name == "END" {
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
            if !(kw.name.is_empty() && kw.value.is_empty() && kw.comment.is_empty()) {
                keywords.push(kw);
            }
        }
    }
    Err(Error::format(
        path,
        format!("FITS header longer than {MAX_HEADER_BLOCKS} blocks"),
    ))
}

/// Split one 80-byte card into name, raw value text and comment.
///
/// Value cards have `= ` at columns 9–10. The value runs to the `/` comment
/// separator, except inside a `'…'` string (where `''` escapes a quote), and
/// keeps its quotes, as [`FitsKeyword::value`] specifies. Commentary cards
/// (no value indicator) return their text as the comment.
fn parse_card(card: &[u8]) -> FitsKeyword {
    // FITS headers are ASCII; anything else becomes '?' so byte slicing below
    // always lands on character boundaries.
    let text: String = card
        .iter()
        .map(|&b| if b.is_ascii() { b as char } else { '?' })
        .collect();
    let name = text[..8].trim_end().to_string();
    if &text[8..10] != "= " {
        return FitsKeyword {
            name,
            value: String::new(),
            comment: text[8..].trim().to_string(),
        };
    }
    let body = &text[10..];
    let mut in_str = false;
    let mut split = body.len();
    for (i, b) in body.bytes().enumerate() {
        match b {
            b'\'' => in_str = !in_str, // a '' escape toggles twice: harmless
            b'/' if !in_str => {
                split = i;
                break;
            }
            _ => {}
        }
    }
    FitsKeyword {
        name,
        value: body[..split].trim().to_string(),
        comment: body[split..].trim_start_matches('/').trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(s: &str) -> Vec<u8> {
        assert!(s.len() <= CARD);
        let mut c = s.as_bytes().to_vec();
        c.resize(CARD, b' ');
        c
    }

    /// A FITS file whose header spans two blocks (40 cards before END), with
    /// a small data block after it.
    fn synth_fits(path: &Path) {
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
        synth_fits(&path);
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
}
