//! End-to-end CLI test: publish synthesized XISF as viewer site.

mod common;
use common::{synth_xisf, tmpdir, vips_on_path};
use std::process::Command;

#[test]
fn publish_emits_viewer_site() {
    if !vips_on_path() {
        eprintln!("skipping: vips binary not found on PATH");
        return;
    }
    let dir = tmpdir("publish");
    let input = dir.join("ngc7000.xisf");
    synth_xisf(&input, 300, 200);
    let out = dir.join("site");

    let cmd = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["publish"])
        .arg(&input)
        .arg(&out)
        .args(["--mode", "dzi", "--title", "North America"])
        .output()
        .unwrap();
    assert!(
        cmd.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&cmd.stderr)
    );

    assert!(out.join("index.html").is_file());
    assert!(out.join("openseadragon.min.js").is_file());
    assert!(out.join("ngc7000.dzi").is_file());
    let html = std::fs::read_to_string(out.join("index.html")).unwrap();
    assert!(html.contains("North America"), "--title lands in the page");
    std::fs::remove_dir_all(&dir).unwrap();
}
