//! Test-only builders for synthetic input files (tests never depend on
//! real image data — see CLAUDE.md).

// Some builders serve only the feature-gated convert/publish tests.
#![cfg_attr(not(feature = "convert"), allow(dead_code))]

use std::path::Path;

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
