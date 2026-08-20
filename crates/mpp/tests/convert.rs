//! End-to-end CLI test: synthesize a tiny XISF, run the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimal monolithic gray Float32 XISF (no WCS) — duplicated from
/// mpp-core's test support because that helper is `cfg(test)`-private.
fn synth_xisf(path: &Path, w: u64, h: u64) {
    let data_offset = 512u64;
    let data_size = w * h * 4;
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="{w}:{h}:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}"/>"#,
            r#"</xisf>"#,
        ),
        w = w,
        h = h,
        off = data_offset,
        size = data_size,
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"XISF0100");
    bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(xml.as_bytes());
    bytes.resize(data_offset as usize, 0);
    for i in 0..(w * h) {
        bytes.extend_from_slice(&(i as f32 / (w * h) as f32).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mpp-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn convert_produces_bigtiff() {
    let dir = tmpdir("ok");
    let input = dir.join("in.xisf");
    synth_xisf(&input, 8, 8);
    let output = dir.join("out.tif");

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let head = std::fs::read(&output).unwrap();
    assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_missing_input_fails_with_message() {
    let dir = tmpdir("missing");
    let out = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(dir.join("nope.xisf"))
        .arg(dir.join("out.tif"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty(), "error goes to stderr");
    std::fs::remove_dir_all(&dir).unwrap();
}
