//! Shared test helpers for CLI tests.

use std::path::{Path, PathBuf};

/// Minimal monolithic gray Float32 XISF (no WCS) — duplicated from
/// mpp-core's test support because that helper is `cfg(test)`-private.
pub fn synth_xisf(path: &Path, w: u64, h: u64) {
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

/// Create a temporary directory for test output.
pub fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mpp-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Check if vips binary is available on PATH.
pub fn vips_on_path() -> bool {
    std::process::Command::new("vips")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
