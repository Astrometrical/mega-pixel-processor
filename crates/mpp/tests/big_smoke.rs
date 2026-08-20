//! Manual smoke test for the phase-1 acceptance criterion: a >4 GB
//! BigTIFF. Ignored by default — it writes ~9 GB of temporary data.
//! Run: `cargo test -p mpp --release -- --ignored`
//! The remaining manual step is opening the produced file in Photoshop.

use std::io::{BufWriter, Write};
use std::process::Command;

/// Stream a gray Float32 XISF of `w`×`h` to disk without holding it in
/// memory (the pixel block alone is w*h*4 bytes).
fn synth_big_xisf(path: &std::path::Path, w: u64, h: u64) {
    let data_offset = 4096u64;
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
    let file = std::fs::File::create(path).unwrap();
    let mut out = BufWriter::with_capacity(1 << 20, file);
    out.write_all(b"XISF0100").unwrap();
    out.write_all(&(xml.len() as u32).to_le_bytes()).unwrap();
    out.write_all(&0u32.to_le_bytes()).unwrap();
    out.write_all(xml.as_bytes()).unwrap();
    let written = 16 + xml.len() as u64;
    for _ in written..data_offset {
        out.write_all(&[0]).unwrap();
    }
    // A cheap deterministic pattern; one row buffer reused.
    let mut row = vec![0u8; (w * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let v = ((x ^ y) & 0xFFFF) as f32 / 65535.0;
            row[(x * 4) as usize..(x * 4 + 4) as usize].copy_from_slice(&v.to_le_bytes());
        }
        out.write_all(&row).unwrap();
    }
    out.flush().unwrap();
}

#[test]
#[ignore = "writes ~9 GB of temp data; run manually with --ignored"]
fn bigtiff_over_4gb_end_to_end() {
    // 33000^2 * 4 bytes = 4.356e9 > 4 GiB (4.295e9).
    let (w, h) = (33_000u64, 33_000u64);
    let dir = std::env::temp_dir().join(format!("mpp-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("big.xisf");
    let output = dir.join("big.tif");
    synth_big_xisf(&input, w, h);

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let meta = std::fs::metadata(&output).unwrap();
    assert!(
        meta.len() > 4 * 1024 * 1024 * 1024,
        "output is {} bytes, expected > 4 GiB",
        meta.len()
    );
    // 4-byte read — never read the whole >4 GB file for the magic check.
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(&output)
        .unwrap()
        .read_exact(&mut head)
        .unwrap();
    assert_eq!(head, [0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");
    std::fs::remove_dir_all(&dir).unwrap();
}
