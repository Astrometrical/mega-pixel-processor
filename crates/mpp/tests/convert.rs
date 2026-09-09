//! End-to-end CLI test: synthesize a tiny XISF, run the real binary.

mod common;
use common::{synth_xisf, tmpdir, vips_on_path};
use std::process::Command;

#[test]
fn convert_produces_classic_tiff_for_a_small_image() {
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

    // Classic TIFF magic: little-endian "II", version 42 (0x2A). The CLI
    // leaves `Container::Auto`, which picks classic for anything under the
    // 4 GB limit; `big_smoke.rs` covers the BigTIFF side with a real
    // >4 GB payload.
    let head = std::fs::read(&output).unwrap();
    assert_eq!(&head[0..4], &[0x49, 0x49, 0x2A, 0x00], "classic TIFF magic");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_produces_png_via_vips() {
    if !vips_on_path() {
        eprintln!("skipping: vips binary not found on PATH");
        return;
    }
    let dir = tmpdir("png");
    let input = dir.join("in.xisf");
    synth_xisf(&input, 8, 8);
    let output = dir.join("out.png");

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .args(["--bit-depth", "u8"])
        .status()
        .unwrap();
    assert!(status.success());
    let head = std::fs::read(&output).unwrap();
    assert_eq!(&head[0..4], &[0x89, b'P', b'N', b'G']);
    assert_eq!(head[24], 8, "u8 override -> 8-bit PNG");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_writes_a_tiled_deflate_bigtiff() {
    let dir = tmpdir("tiled");
    let src = dir.join("in.xisf");
    synth_xisf(&src, 600, 400);
    let out = dir.join("out.tif");

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&src)
        .arg(&out)
        .args([
            "--tiled",
            "--tile-size",
            "256",
            "--compression",
            "deflate",
            "--container",
            "big",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let head = std::fs::read(&out).unwrap();
    assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");

    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::TileWidth).unwrap(), 256);
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::Compression).unwrap(), 8);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_rejects_a_wide_sample_format_for_png() {
    let dir = tmpdir("reject");
    let src = dir.join("in.xisf");
    synth_xisf(&src, 8, 8);
    let out = dir.join("out.png");

    let output = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&src)
        .arg(&out)
        .args(["--bit-depth", "f64"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("f64") || err.contains("bit depth"),
        "error should name the rejected sample format, got: {err}"
    );
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
