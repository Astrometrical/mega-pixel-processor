//! Test-only builders for synthetic input files (tests never depend on
//! real image data — see CLAUDE.md).

// Some builders serve only the feature-gated convert/publish tests.
#![cfg_attr(not(feature = "convert"), allow(dead_code))]

use std::path::Path;

use crate::formats::FitsKeyword;
use crate::{Error, Result};

/// Base64-encode f64s little-endian, mirroring PixInsight's
/// `location="inline:base64"` property payloads.
fn b64_f64s(data: &[f64]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
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

/// Write a minimal monolithic XISF file: `w`×`h`, `c` planar Float32
/// channels; sample at plane `k`, index `i` is `(k*w*h + i) / (w*h*c)`.
/// With `with_wcs`, a full PixInsight astrometric-solution property set is
/// embedded (Gnomonic, ICRS, crval [84.199, -3.240], refimg [w/2, h/2],
/// diagonal matrix ±4.4368e-4 deg/px).
pub(crate) fn synth_xisf(path: &Path, w: u64, h: u64, c: u64, with_wcs: bool) {
    let data_offset = 4096u64;
    let data_size = w * h * c * 4;
    let props = if with_wcs {
        let refimg = [w as f64 / 2.0, h as f64 / 2.0];
        format!(
            concat!(
                r#"<Property id="PCL:AstrometricSolution:ReferenceCelestialCoordinates" type="F64Vector" length="2" location="inline:base64">{crval}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:ReferenceImageCoordinates" type="F64Vector" length="2" location="inline:base64">{refimg}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:LinearTransformationMatrix" type="F64Matrix" rows="2" columns="2" location="inline:base64">{mat}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:ProjectionSystem" type="String" value="Gnomonic"/>"#,
                r#"<Property id="Observation:CelestialReferenceSystem" type="String" value="ICRS"/>"#,
            ),
            crval = b64_f64s(&[84.199, -3.240]),
            refimg = b64_f64s(&refimg),
            mat = b64_f64s(&[-4.4368e-4, 0.0, 0.0, 4.4368e-4]),
        )
    } else {
        String::new()
    };
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="{w}:{h}:{c}" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}">"#,
            "{props}",
            r#"</Image></xisf>"#,
        ),
        w = w,
        h = h,
        c = c,
        off = data_offset,
        size = data_size,
        props = props,
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"XISF0100");
    bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(xml.as_bytes());
    assert!(
        bytes.len() <= data_offset as usize,
        "header must fit below data"
    );
    bytes.resize(data_offset as usize, 0);
    let total = (w * h * c) as f32;
    for i in 0..(w * h * c) {
        bytes.extend_from_slice(&(i as f32 / total).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

/// Minimal FITS writer for tests: primary HDU, `NAXIS` 2 (one channel) or 3
/// (planar), big-endian samples, rows stored bottom-up unless `extra_cards`
/// carries `ROWORDER = 'TOP-DOWN'`. `planes` is top-down planar canvas data
/// in `[0, 1]`; integer `bitpix` (8/16/32) quantizes to the full unsigned
/// range with the conventional `BZERO` (0 / 32768 / 2147483648); −32/−64
/// store the floats. Round-trips through [`crate::formats::fits::FitsImage`].
pub(crate) fn synth_fits(
    path: &Path,
    w: u64,
    h: u64,
    ch: u64,
    planes: &[f32],
    bitpix: i32,
    extra_cards: &[FitsKeyword],
) -> Result<()> {
    let n = (w * h * ch) as usize;
    if planes.len() != n {
        return Err(Error::format(
            path,
            format!(
                "planes length {} does not match geometry {w}x{h}x{ch} ({n})",
                planes.len()
            ),
        ));
    }
    let top_down = extra_cards
        .iter()
        .any(|k| k.name == "ROWORDER" && k.value.contains("TOP-DOWN"));
    let card = |name: &str, value: &str| -> String {
        let s = if value.starts_with('\'') {
            format!("{name:<8}= {value}")
        } else {
            format!("{name:<8}= {value:>20}")
        };
        format!("{s:<80}")
    };
    let mut hdr = String::new();
    hdr.push_str(&card("SIMPLE", "T"));
    hdr.push_str(&card("BITPIX", &bitpix.to_string()));
    hdr.push_str(&card("NAXIS", if ch == 1 { "2" } else { "3" }));
    hdr.push_str(&card("NAXIS1", &w.to_string()));
    hdr.push_str(&card("NAXIS2", &h.to_string()));
    if ch != 1 {
        hdr.push_str(&card("NAXIS3", &ch.to_string()));
    }
    let bzero: f64 = match bitpix {
        16 => 32768.0,
        32 => 2147483648.0,
        _ => 0.0,
    };
    if bitpix > 0 {
        hdr.push_str(&card("BZERO", &format!("{bzero}")));
        hdr.push_str(&card("BSCALE", "1"));
    }
    for k in extra_cards {
        if k.name == "COMMENT" || k.name == "HISTORY" {
            hdr.push_str(&format!("{:<80}", format!("{} {}", k.name, k.comment)));
        } else {
            hdr.push_str(&card(&k.name, &k.value));
        }
    }
    hdr.push_str(&format!("{:<80}", "END"));
    while !hdr.len().is_multiple_of(2880) {
        hdr.push(' ');
    }
    let mut bytes = hdr.into_bytes();
    let (wu, hu) = (w as usize, h as usize);
    let norm: f64 = match bitpix {
        8 => 255.0,
        16 => 65535.0,
        32 => 4294967295.0,
        _ => 1.0,
    };
    for c in 0..ch as usize {
        for r_file in 0..hu {
            let r_img = if top_down { r_file } else { hu - 1 - r_file };
            let row = &planes[(c * hu + r_img) * wu..(c * hu + r_img + 1) * wu];
            for &v in row {
                match bitpix {
                    8 => bytes.push((v as f64 * norm).round().clamp(0.0, 255.0) as u8),
                    16 => bytes.extend(
                        (((v as f64 * norm).round().clamp(0.0, 65535.0) - bzero) as i16)
                            .to_be_bytes(),
                    ),
                    32 => bytes.extend(
                        (((v as f64 * norm).round().clamp(0.0, 4294967295.0) - bzero) as i32)
                            .to_be_bytes(),
                    ),
                    -32 => bytes.extend(v.to_be_bytes()),
                    -64 => bytes.extend((v as f64).to_be_bytes()),
                    other => {
                        return Err(Error::format(
                            path,
                            format!("write_fits: unsupported BITPIX {other}"),
                        ));
                    }
                }
            }
        }
    }
    while !bytes.len().is_multiple_of(2880) {
        bytes.push(0);
    }
    std::fs::write(path, bytes).map_err(|e| Error::io(path, e))
}

/// How a synthesized XISF block is serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// `location="attachment:…"` after the header.
    Attached,
    /// `location="inline:base64"`.
    Inline,
    /// `location="embedded"` with a child `<Data encoding="base64">`.
    Embedded,
}

/// Encoding options for one synthesized block.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockSpec {
    /// Where the block goes.
    pub placement: Placement,
    /// `(codec, shuffle item size, subblock size)` to compress with.
    pub codec: Option<(crate::formats::xisf_codec::Codec, Option<usize>, usize)>,
    /// Checksum algorithm name (`"sha-1"`, `"sha-256"`, …) to declare.
    pub checksum: Option<&'static str>,
    /// Corrupt the declared digest (flip its first byte).
    pub bad_checksum: bool,
}

impl Default for BlockSpec {
    fn default() -> Self {
        BlockSpec {
            placement: Placement::Attached,
            codec: None,
            checksum: None,
            bad_checksum: false,
        }
    }
}

/// Hex digest of `data` under the XISF checksum algorithm `alg`.
pub(crate) fn digest_hex(alg: &str, data: &[u8]) -> String {
    use sha2::Digest;
    let d: Vec<u8> = match alg {
        "sha-1" | "sha1" => sha1::Sha1::digest(data).to_vec(),
        "sha-256" | "sha256" => sha2::Sha256::digest(data).to_vec(),
        "sha-512" | "sha512" => sha2::Sha512::digest(data).to_vec(),
        "sha3-256" => sha3::Sha3_256::digest(data).to_vec(),
        "sha3-512" => sha3::Sha3_512::digest(data).to_vec(),
        other => panic!("digest_hex: unknown algorithm {other}"),
    };
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A block's serialized form: the attributes to put on its element, the
/// stored bytes (attached) or the text (inline/embedded).
struct EncodedBlock {
    /// `compression=…` / `subblocks=…` / `checksum=…` attributes (leading
    /// space included); for embedded blocks these go on `<Data>`.
    attrs: String,
    stored: Vec<u8>,
    placement: Placement,
}

fn base64(bytes: &[u8]) -> String {
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
            s.push(if i <= chunk.len() {
                ALPHA[(n >> (18 - 6 * i)) as usize & 63] as char
            } else {
                '='
            });
        }
    }
    s
}

fn encode_block(data: &[u8], spec: &BlockSpec) -> EncodedBlock {
    let (stored, mut attrs) = match spec.codec {
        Some((codec, item, sub)) => {
            let (stored, c) = crate::formats::xisf_codec::compress(codec, item, data, sub);
            let mut a = format!(r#" compression="{}:{}"#, c.codec_id(), data.len());
            if let Some(i) = item {
                a.push_str(&format!(":{i}"));
            }
            a.push('"');
            if c.subblocks.len() > 1 {
                let list: Vec<String> = c
                    .subblocks
                    .iter()
                    .map(|(x, y)| format!("{x},{y}"))
                    .collect();
                a.push_str(&format!(r#" subblocks="{}""#, list.join(":")));
            }
            (stored, a)
        }
        None => (data.to_vec(), String::new()),
    };
    if let Some(alg) = spec.checksum {
        let mut hex = digest_hex(alg, &stored);
        if spec.bad_checksum {
            let flipped = if hex.starts_with('0') { '1' } else { '0' };
            hex.replace_range(0..1, &flipped.to_string());
        }
        attrs.push_str(&format!(r#" checksum="{alg}:{hex}""#));
    }
    EncodedBlock {
        attrs,
        stored,
        placement: spec.placement,
    }
}

/// One `<Image>` of a synthesized unit.
#[derive(Debug, Clone)]
pub(crate) struct ImageSpec {
    /// `geometry` (width, height, channels).
    pub geometry: (u64, u64, u64),
    /// `sampleFormat` attribute value.
    pub sample_format: &'static str,
    /// Raw sample bytes exactly as they should be stored uncompressed
    /// (already in the declared byte order and pixel storage).
    pub data: Vec<u8>,
    /// Extra `<Image>` attributes (leading space included), e.g.
    /// ` bounds="0:1" pixelStorage="Normal" byteOrder="big"`.
    pub attrs: String,
    /// Child elements: `<FITSKeyword>`s, `<Property>`s, `<Thumbnail>`…
    pub children: String,
    /// Block encoding (placement must be `Attached` or `Embedded`).
    pub block: BlockSpec,
}

/// A `<Property>` whose value is a data block (vector/matrix).
#[derive(Debug, Clone)]
pub(crate) struct PropSpec {
    /// `id`.
    pub id: &'static str,
    /// `type`, e.g. `F64Matrix`.
    pub type_: &'static str,
    /// Shape attributes (leading space included), e.g. ` rows="2" columns="2"`.
    pub shape: String,
    /// Raw element bytes as stored uncompressed (declared byte order).
    pub data: Vec<u8>,
    /// Extra attributes, e.g. ` byteOrder="big"`.
    pub attrs: String,
    /// Block encoding.
    pub block: BlockSpec,
}

/// Builder for synthetic monolithic XISF units, laid out as PixInsight does:
/// uncompressed attached image blocks 4096-aligned, compressed blocks and
/// property attachments packed after them unaligned.
#[derive(Debug, Clone, Default)]
pub(crate) struct XisfBuilder {
    /// Images, in order.
    pub images: Vec<ImageSpec>,
    /// File-level block properties.
    pub properties: Vec<PropSpec>,
    /// Raw XML appended inside the root element (scalar properties,
    /// extension elements…).
    pub root_xml: String,
}

impl XisfBuilder {
    /// Serialize the unit to `path`.
    pub fn write(&self, path: &Path) {
        let images: Vec<EncodedBlock> = self
            .images
            .iter()
            .map(|i| encode_block(&i.data, &i.block))
            .collect();
        let props: Vec<EncodedBlock> = self
            .properties
            .iter()
            .map(|p| encode_block(&p.data, &p.block))
            .collect();
        let mut capacity = 4096u64;
        loop {
            // Lay out attachments after a header region of `capacity` bytes.
            let mut cursor = capacity;
            let mut place = |b: &EncodedBlock, align: bool| -> u64 {
                if align {
                    cursor = cursor.div_ceil(4096) * 4096;
                }
                let at = cursor;
                cursor += b.stored.len() as u64;
                at
            };
            let mut offsets = Vec::new();
            for b in &images {
                let aligned = b.attrs.is_empty() || !b.attrs.contains("compression");
                offsets.push(place(b, aligned));
            }
            for b in &props {
                offsets.push(place(b, false));
            }
            let mut xml = String::from(
                r#"<?xml version="1.0" encoding="UTF-8"?><xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            );
            let mut k = 0;
            for (spec, b) in self.images.iter().zip(&images) {
                let (w, h, c) = spec.geometry;
                xml.push_str(&format!(
                    r#"<Image geometry="{w}:{h}:{c}" sampleFormat="{}"{}"#,
                    spec.sample_format, spec.attrs
                ));
                match b.placement {
                    Placement::Attached => xml.push_str(&format!(
                        r#" location="attachment:{}:{}"{}>"#,
                        offsets[k],
                        b.stored.len(),
                        b.attrs
                    )),
                    Placement::Embedded => xml.push_str(&format!(
                        r#" location="embedded"><Data encoding="base64"{}>{}</Data>"#,
                        b.attrs,
                        base64(&b.stored)
                    )),
                    Placement::Inline => panic!("an Image cannot be inline"),
                }
                xml.push_str(&spec.children);
                xml.push_str("</Image>");
                k += 1;
            }
            for (spec, b) in self.properties.iter().zip(&props) {
                let head = format!(
                    r#"<Property id="{}" type="{}"{}{}"#,
                    spec.id, spec.type_, spec.shape, spec.attrs
                );
                match b.placement {
                    Placement::Attached => xml.push_str(&format!(
                        r#"{head} location="attachment:{}:{}"{}/>"#,
                        offsets[k],
                        b.stored.len(),
                        b.attrs
                    )),
                    Placement::Inline => xml.push_str(&format!(
                        r#"{head} location="inline:base64"{}>{}</Property>"#,
                        b.attrs,
                        base64(&b.stored)
                    )),
                    Placement::Embedded => xml.push_str(&format!(
                        r#"{head} location="embedded"><Data encoding="base64"{}>{}</Data></Property>"#,
                        b.attrs,
                        base64(&b.stored)
                    )),
                }
                k += 1;
            }
            xml.push_str(&self.root_xml);
            xml.push_str("</xisf>");
            if 16 + xml.len() as u64 > capacity {
                capacity *= 2;
                continue;
            }
            let mut bytes = Vec::with_capacity(cursor as usize);
            bytes.extend_from_slice(b"XISF0100");
            bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&[0u8; 4]);
            bytes.extend_from_slice(xml.as_bytes());
            for (b, &off) in images.iter().chain(&props).zip(&offsets) {
                if b.placement == Placement::Attached {
                    bytes.resize(off as usize, 0);
                    bytes.extend_from_slice(&b.stored);
                }
            }
            std::fs::write(path, bytes).unwrap();
            return;
        }
    }
}

/// A minimal one-image unit: `w`×`h`×1 Float32 zeros, uncompressed.
pub(crate) fn tiny_image() -> ImageSpec {
    ImageSpec {
        geometry: (2, 2, 1),
        sample_format: "Float32",
        data: vec![0u8; 16],
        attrs: r#" bounds="0:1" colorSpace="Gray""#.into(),
        children: String::new(),
        block: BlockSpec::default(),
    }
}
