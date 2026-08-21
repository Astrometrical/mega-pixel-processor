//! Cross-process proof that the real `mpp-ipc-worker` binary — spawned as a
//! child process and driven purely over stdin/stdout plus a real POSIX
//! shared-memory segment, exactly the way `docs/pi-protocol.md` describes a
//! host doing it — implements every lifecycle arm of the wire protocol:
//! successful writes (grayscale f32 and RGB u16, in-order and shuffled band
//! delivery), incomplete-coverage rejection, version-mismatch rejection,
//! cancellation cleanup, and oversize-band rejection.
//!
//! The reference host role (creating the shm segment, driving `Init` →
//! `Band`… → `Finish`/`Cancel`, checking the child's exit code against its
//! own terminal frame) is played by `mpp_ipc::testhost`, reached here via
//! the `testkit` feature (see this crate's `Cargo.toml`). Test 5
//! (`version_mismatch_errors`) drives the worker directly instead — no
//! testhost helper is needed for a single malformed `Init`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mpp_ipc::protocol::{
    ErrorMessage, IPC_PROTOCOL_VERSION, InitMessage, read_frame, tag, write_frame,
};
use mpp_ipc::testhost::{
    TestHostConfig, run_cancel_after_first_band, run_oversize_band_session, run_write_session,
    run_write_session_incomplete, run_write_session_shuffled,
};
use tiff::decoder::{Decoder, DecodingResult};

/// A fresh, empty scratch directory for one test, isolated by `tag` and
/// this process's pid so parallel `cargo test` runs never collide.
fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mpp-ipcworker-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Lists any files in `dir` whose name contains `.mpp-stage-` — the
/// worker's staging-scratch-file naming convention
/// (`crates/mpp-ipc-worker/src/main.rs`'s `staging_path_for`,
/// `<output-filename>.mpp-stage-<pid>`). Used to assert "no staging file
/// leftover" without needing to know the worker's own pid.
fn staging_leftovers(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".mpp-stage-"))
        })
        .collect()
}

/// BigTIFF magic: little-endian "II", version 43 (0x2B) — `tiff.rs`'s own
/// unit tests check the same bytes.
fn assert_bigtiff_magic(path: &Path) {
    let head = std::fs::read(path).unwrap();
    assert_eq!(
        &head[0..4],
        &[0x49, 0x49, 0x2B, 0x00],
        "expected BigTIFF magic bytes at the start of {}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// 1. gray_f32_roundtrip
// ---------------------------------------------------------------------------

/// 64x48x1, f32, `band_rows: 7` — doesn't divide 48 (48 = 6*7 + 6), so the
/// last band of the run is a genuine partial band. Every decoded sample
/// must equal `pixel(y, x)` exactly: f32 is passthrough end to end (shm →
/// staging → BigTIFF), so there is no rounding to tolerate.
#[test]
fn gray_f32_roundtrip() {
    let dir = tmpdir("gray-f32");
    let out = dir.join("out.tif");
    let (width, height) = (64u64, 48u64);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out.clone(),
        width,
        height,
        channels: 1,
        bit_depth: "f32".to_string(),
        band_rows: 7,
        slots: 2,
    };
    run_write_session(&cfg, pixel).expect("write session should succeed");

    assert_bigtiff_magic(&out);
    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (width as u32, height as u32));
    match dec.read_image().unwrap() {
        DecodingResult::F32(data) => {
            assert_eq!(data.len(), (width * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let idx = (y * width + x) as usize;
                    assert_eq!(data[idx], pixel(0, y, x), "mismatch at row {y} col {x}");
                }
            }
        }
        other => panic!("expected F32 samples, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 2. rgb_u16_roundtrip
// ---------------------------------------------------------------------------

/// 16x8x3, u16 output: decoded samples must equal
/// `(pixel.clamp(0,1) * 65535).round()`, interleaved RGB (chunky, per
/// `docs/pi-protocol.md`'s shm layout being planar-in but the encoder
/// interleaving to TIFF chunky order on the way out).
#[test]
fn rgb_u16_roundtrip() {
    let dir = tmpdir("rgb-u16");
    let out = dir.join("out.tif");
    let (width, height, channels) = (16u64, 8u64, 3u64);
    let pixel = |c: u64, y: u64, x: u64| {
        ((c * height * width + y * width + x) as f32)
            / (channels as f32 * height as f32 * width as f32)
    };

    let cfg = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out.clone(),
        width,
        height,
        channels,
        bit_depth: "u16".to_string(),
        band_rows: 8,
        slots: 2,
    };
    run_write_session(&cfg, pixel).expect("write session should succeed");

    assert_bigtiff_magic(&out);
    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (width as u32, height as u32));
    match dec.read_image().unwrap() {
        DecodingResult::U16(data) => {
            assert_eq!(data.len(), (width * height * channels) as usize);
            for y in 0..height {
                for x in 0..width {
                    for c in 0..channels {
                        let idx = ((y * width + x) * channels + c) as usize;
                        let expected = (pixel(c, y, x).clamp(0.0, 1.0) * 65535.0).round() as u16;
                        assert_eq!(
                            data[idx], expected,
                            "mismatch at row {y} col {x} channel {c}"
                        );
                    }
                }
            }
        }
        other => panic!("expected U16 samples, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 3. shuffled_band_order
// ---------------------------------------------------------------------------

/// Same geometry/pixel formula as `gray_f32_roundtrip`, but driven through
/// `run_write_session_shuffled` (bands sent in reverse order). The
/// worker's staging file reassembles arbitrary-order bands into the same
/// planar layout regardless of arrival order, so the resulting BigTIFF
/// must be byte-for-byte identical to the in-order run.
#[test]
fn shuffled_band_order() {
    let dir = tmpdir("shuffled");
    let out_in_order = dir.join("in_order.tif");
    let out_shuffled = dir.join("shuffled.tif");
    let (width, height) = (64u64, 48u64);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg_in_order = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out_in_order.clone(),
        width,
        height,
        channels: 1,
        bit_depth: "f32".to_string(),
        band_rows: 7,
        slots: 2,
    };
    let cfg_shuffled = TestHostConfig {
        output_path: out_shuffled.clone(),
        ..cfg_in_order.clone()
    };

    run_write_session(&cfg_in_order, pixel).expect("in-order write session should succeed");
    run_write_session_shuffled(&cfg_shuffled, pixel)
        .expect("shuffled write session should succeed");

    assert_bigtiff_magic(&out_shuffled);
    let in_order_bytes = std::fs::read(&out_in_order).unwrap();
    let shuffled_bytes = std::fs::read(&out_shuffled).unwrap();
    assert_eq!(
        shuffled_bytes, in_order_bytes,
        "shuffled-band-order output must be byte-identical to in-order output"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 4. incomplete_coverage_errors
// ---------------------------------------------------------------------------

/// Sends only the first half of the bands, then `Finish`es: the worker
/// must reject with an `Error` naming the uncovered rows, exit nonzero,
/// and leave neither an output file nor a staging file behind
/// (`docs/pi-protocol.md` §7.3).
#[test]
fn incomplete_coverage_errors() {
    let dir = tmpdir("incomplete");
    let out = dir.join("out.tif");
    let (width, height) = (64u64, 48u64);
    let band_rows = 7u32;
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out.clone(),
        width,
        height,
        channels: 1,
        bit_depth: "f32".to_string(),
        band_rows,
        slots: 2,
    };

    // 7 bands total (48 = 6*7 + 6); send half, leaving rows 21..48 uncovered.
    let total_bands = height.div_ceil(band_rows as u64) as usize;
    let bands_to_send = total_bands / 2;
    assert!(
        bands_to_send > 0 && bands_to_send < total_bands,
        "test setup: expected a genuine partial send, got {bands_to_send}/{total_bands}"
    );

    let err = run_write_session_incomplete(&cfg, pixel, bands_to_send)
        .expect("worker should report an Error, not succeed");
    assert!(
        err.contains("uncovered"),
        "error message should mention the uncovered rows: {err}"
    );

    assert!(
        !out.exists(),
        "output file must be absent after a rejected run"
    );
    let leftovers = staging_leftovers(&dir);
    assert!(
        leftovers.is_empty(),
        "staging file(s) left behind: {leftovers:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 5. version_mismatch_errors
// ---------------------------------------------------------------------------

/// A host that sends `protocol_version: 99` in `Init` must be refused
/// before anything else happens: the worker's very first (and only)
/// reply is an `Error` naming the version mismatch, followed by a
/// nonzero exit (`docs/pi-protocol.md` §7.1). Driven directly (no
/// `testhost` helper) since it's a single malformed handshake, not a
/// full write session.
#[test]
fn version_mismatch_errors() {
    let dir = tmpdir("vermismatch");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mpp-ipc-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn mpp-ipc-worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    let init = InitMessage {
        protocol_version: 99,
        worker_version: env!("CARGO_PKG_VERSION").to_string(),
        output_path: dir.join("out.tif").to_string_lossy().into_owned(),
        width: 8,
        height: 8,
        channels: 1,
        bit_depth: "f32".to_string(),
        shm_name: "/mpp-e2e-vermismatch-never-opened".to_string(),
        slot_bytes: 128,
        slots: 1,
    };
    assert_ne!(init.protocol_version, IPC_PROTOCOL_VERSION);
    let payload = serde_json::to_vec(&init).unwrap();
    write_frame(&mut stdin, tag::INIT, &payload).expect("write Init frame");
    stdin.flush().expect("flush Init frame");
    drop(stdin);

    let (t, payload) = read_frame(&mut stdout).expect("read worker's reply frame");
    assert_eq!(t, tag::ERROR, "expected an Error frame");
    let msg: ErrorMessage = serde_json::from_slice(&payload).expect("Error payload is valid JSON");
    assert!(
        msg.message.contains("protocol_version"),
        "error must name protocol_version: {}",
        msg.message
    );

    let status = child.wait().expect("wait on worker");
    assert!(!status.success(), "a refused Init must exit nonzero");

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 6. cancel_cleans_up
// ---------------------------------------------------------------------------

/// `Cancel` after the first band leaves neither an output file nor a
/// staging file behind (`docs/pi-protocol.md` §7.4).
#[test]
fn cancel_cleans_up() {
    let dir = tmpdir("cancel");
    let out = dir.join("out.tif");
    let cfg = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out.clone(),
        width: 32,
        height: 24,
        channels: 1,
        bit_depth: "f32".to_string(),
        band_rows: 6,
        slots: 2,
    };

    run_cancel_after_first_band(&cfg).expect("cancel session should report cancellation cleanly");

    assert!(
        !out.exists(),
        "output file must be absent after a cancelled run"
    );
    let leftovers = staging_leftovers(&dir);
    assert!(
        leftovers.is_empty(),
        "staging file(s) left behind: {leftovers:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 7. oversize_band_rejected
// ---------------------------------------------------------------------------

/// An `Init.slot_bytes` too small to hold the very first band must be
/// rejected before the worker touches the slot at all: `Error` naming
/// the slot-size violation, nonzero exit (`docs/pi-protocol.md` §6's
/// slot-size validation rule).
#[test]
fn oversize_band_rejected() {
    let dir = tmpdir("oversize");
    let out = dir.join("out.tif");
    let (width, height, band_rows) = (32u64, 16u64, 8u32);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg = TestHostConfig {
        worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
        output_path: out.clone(),
        width,
        height,
        channels: 1,
        bit_depth: "f32".to_string(),
        band_rows,
        slots: 1,
    };

    // A real band needs band_rows * width * 4 = 8 * 32 * 4 = 1024 bytes;
    // advertise half that.
    let real_band_bytes = band_rows as u64 * width * 4;
    let advertised_slot_bytes = real_band_bytes / 2;

    let err = run_oversize_band_session(&cfg, pixel, advertised_slot_bytes)
        .expect("worker should report an Error, not accept the oversize band");
    assert!(
        err.contains("slot_bytes"),
        "error message should mention slot_bytes: {err}"
    );
    assert!(
        !out.exists(),
        "output file must be absent after a rejected run"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
