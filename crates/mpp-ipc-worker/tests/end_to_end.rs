//! Cross-process proof that the real `mpp-ipc-worker` binary — spawned as a
//! child process and driven purely over stdin/stdout plus a real POSIX
//! shared-memory segment, exactly the way `docs/pi-protocol.md` describes a
//! host doing it — implements every lifecycle arm of the wire protocol:
//! successful writes (grayscale f32 and RGB u16, in-order and shuffled band
//! delivery), incomplete-coverage rejection, version-mismatch rejection,
//! cancellation cleanup, oversize-band rejection, host-gone cleanup,
//! (Task 3) rejecting a read-direction `ReadBandAck` reply on a write
//! session, and (protocol v3) native-sample-type bands plus the widened
//! write options reaching the encoder.
//!
//! The reference host role (creating the shm segment, driving `Init` →
//! `Band`… → `Finish`/`Cancel`, checking the child's exit code against its
//! own terminal frame) is played by `mpp_ipc::testhost`, reached here via
//! the `testkit` feature (see this crate's `Cargo.toml`). Test 5
//! (`version_mismatch_errors`), test 8 (`host_gone_cleans_up`), and test 9
//! (`read_band_ack_on_write_session_errors`) drive the worker directly
//! instead — no testhost helper covers a bare `Init`, a stdin drop without
//! `Finish`, or an out-of-direction reply frame.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mpp_ipc::protocol::{
    BandHeader, ErrorMessage, IPC_PROTOCOL_VERSION, InitMessage, SessionMode, read_frame, tag,
    write_frame,
};
use mpp_ipc::shm::ShmSegment;
use mpp_ipc::testhost::{
    TestHostConfig, run_cancel_after_first_band, run_oversize_band_session, run_write_session,
    run_write_session_incomplete, run_write_session_shuffled, run_write_session_typed,
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

/// Classic TIFF magic: little-endian "II", version 42 (0x2A).
///
/// The worker leaves `Container::Auto`, so these small test images come out
/// as classic TIFF; the container is not what these tests are about, they
/// just need the output to be a TIFF the decoder below can open. BigTIFF is
/// covered by `mpp`'s `big_smoke` test, which writes a real >4 GB payload.
fn assert_tiff_magic(path: &Path) {
    let head = std::fs::read(path).unwrap();
    assert_eq!(
        &head[0..4],
        &[0x49, 0x49, 0x2A, 0x00],
        "expected classic TIFF magic bytes at the start of {}",
        path.display()
    );
}

// ---------------------------------------------------------------------------
// 1. gray_f32_roundtrip
// ---------------------------------------------------------------------------

/// 64x48x1, f32, `band_rows: 7` — doesn't divide 48 (48 = 6*7 + 6), so the
/// last band of the run is a genuine partial band. Every decoded sample
/// must equal `pixel(y, x)` exactly: f32 is passthrough end to end (shm →
/// staging → TIFF), so there is no rounding to tolerate.
#[test]
fn gray_f32_roundtrip() {
    let dir = tmpdir("gray-f32");
    let out = dir.join("out.tif");
    let (width, height) = (64u64, 48u64);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg = TestHostConfig {
        bit_depth: "f32".to_string(),
        band_rows: 7,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            1,
        )
    };
    run_write_session(&cfg, pixel).expect("write session should succeed");

    assert_tiff_magic(&out);
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
        bit_depth: "u16".to_string(),
        band_rows: 8,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            channels,
        )
    };
    run_write_session(&cfg, pixel).expect("write session should succeed");

    assert_tiff_magic(&out);
    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (width as u32, height as u32));
    match dec.read_image().unwrap() {
        DecodingResult::U16(data) => {
            assert_eq!(data.len(), (width * height * channels) as usize);
            for y in 0..height {
                for x in 0..width {
                    for c in 0..channels {
                        let idx = ((y * width + x) * channels + c) as usize;
                        // Mirror `Sample::from_unit_f32`, which scales in
                        // f64. The old f32 product rounded a value a hair
                        // below .5 (54612.4987 here) up to exactly 54612.5
                        // and then away from zero, giving a 1-LSB-high
                        // answer; scaling in f64 does not.
                        let expected =
                            (f64::from(pixel(c, y, x).clamp(0.0, 1.0)) * 65535.0).round() as u16;
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
/// planar layout regardless of arrival order, so the resulting TIFF
/// must be byte-for-byte identical to the in-order run.
#[test]
fn shuffled_band_order() {
    let dir = tmpdir("shuffled");
    let out_in_order = dir.join("in_order.tif");
    let out_shuffled = dir.join("shuffled.tif");
    let (width, height) = (64u64, 48u64);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg_in_order = TestHostConfig {
        bit_depth: "f32".to_string(),
        band_rows: 7,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out_in_order.clone(),
            width,
            height,
            1,
        )
    };
    let cfg_shuffled = TestHostConfig {
        output_path: out_shuffled.clone(),
        ..cfg_in_order.clone()
    };

    run_write_session(&cfg_in_order, pixel).expect("in-order write session should succeed");
    run_write_session_shuffled(&cfg_shuffled, pixel)
        .expect("shuffled write session should succeed");

    assert_tiff_magic(&out_shuffled);
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
        bit_depth: "f32".to_string(),
        band_rows,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            1,
        )
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
        mode: SessionMode::Write,
        ..Default::default()
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
        bit_depth: "f32".to_string(),
        band_rows: 6,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            32,
            24,
            1,
        )
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
        bit_depth: "f32".to_string(),
        band_rows,
        slots: 1,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            1,
        )
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

// ---------------------------------------------------------------------------
// 8. host_gone_cleans_up
// ---------------------------------------------------------------------------

/// Drives the worker directly (no `testhost` helper — this is exactly the
/// "host disappears" case `run_write_session` can't express): a valid
/// `Init` and one valid `Band` are sent and acked, then `stdin` is dropped
/// (closing the pipe) *without* ever sending `Finish`.
///
/// `docs/pi-protocol.md` §7.5 says this is the "host-gone" path: the worker
/// treats stdin EOF-before-`Finish` like `Cancel`'s cleanup (staging file
/// and any partial output removed) but — unlike `Cancel` — sends **no**
/// frame at all, since there is no host left to read one; the nonzero exit
/// code alone is the signal. This test pins exactly that: the worker exits
/// nonzero, stdout carries nothing beyond the `BandAck` (and any interleaved
/// `Progress`) already read — no `Done`, no `Error` — and neither the
/// output file nor a `.mpp-stage-*` staging file is left behind.
#[test]
fn host_gone_cleans_up() {
    let dir = tmpdir("hostgone");
    let out = dir.join("out.tif");
    let (width, height, channels, band_rows, slots) = (8u64, 8u64, 1u64, 4u32, 1u32);
    let slot_bytes = band_rows as u64 * width * 4;

    let shm_name = format!("/mpp-e2e-hg-{}", std::process::id() % 100_000);
    let mut shm = ShmSegment::create(&shm_name, slot_bytes as usize * slots as usize)
        .expect("create shm segment");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mpp-ipc-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn mpp-ipc-worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    let init = InitMessage {
        protocol_version: IPC_PROTOCOL_VERSION,
        worker_version: env!("CARGO_PKG_VERSION").to_string(),
        output_path: out.to_string_lossy().into_owned(),
        width,
        height,
        channels,
        bit_depth: "f32".to_string(),
        shm_name: shm_name.clone(),
        slot_bytes,
        slots,
        mode: SessionMode::Write,
        ..Default::default()
    };
    let init_payload = serde_json::to_vec(&init).unwrap();
    write_frame(&mut stdin, tag::INIT, &init_payload).expect("write Init frame");
    stdin.flush().expect("flush Init frame");

    // One valid, in-bounds band: rows=4 of channel 0 starting at row 0,
    // filled with zeros — its contents don't matter, only that the worker
    // accepts and acks it.
    let rows = band_rows;
    let samples = vec![0f32; rows as usize * width as usize];
    shm.as_mut_slice()[0..samples.len() * 4].copy_from_slice(bytemuck::cast_slice(&samples));
    let header = BandHeader {
        channel: 0,
        start_row: 0,
        rows,
        slot: 0,
    };
    write_frame(&mut stdin, tag::BAND, &header.to_bytes()).expect("write Band frame");
    stdin.flush().expect("flush Band frame");

    // Read frames until the BandAck arrives, tolerating an interleaved
    // Progress (as `docs/pi-protocol.md` §7.2 allows) and failing loudly on
    // anything else.
    loop {
        let (t, payload) = read_frame(&mut stdout).expect("read reply to Band");
        match t {
            tag::BAND_ACK => break,
            tag::PROGRESS => continue,
            tag::ERROR => {
                let msg: ErrorMessage = serde_json::from_slice(&payload).unwrap();
                panic!("worker rejected the valid Band with Error: {}", msg.message);
            }
            other => panic!("unexpected tag {other} while waiting for BandAck"),
        }
    }

    // The host disappears: close its end of stdin without ever sending
    // Finish. No more input arrives on the worker's stdin from here on.
    drop(stdin);

    // Read every remaining frame to EOF. Per §7.5 there must be none beyond
    // an optional trailing Progress — specifically no Done and no Error.
    let mut saw_terminal_frame: Option<u8> = None;
    while let Ok((t, payload)) = read_frame(&mut stdout) {
        match t {
            tag::PROGRESS => continue,
            tag::DONE => panic!("worker sent Done after stdin closed without Finish"),
            tag::ERROR => {
                let msg: ErrorMessage = serde_json::from_slice(&payload).unwrap();
                panic!(
                    "worker sent Error after stdin closed without Finish (expected no frame \
                     at all per §7.5): {}",
                    msg.message
                );
            }
            other => saw_terminal_frame = Some(other),
        }
    }
    // loop exits on the clean EOF `read_frame` returns once the worker
    // closes its stdout and exits.
    assert!(
        saw_terminal_frame.is_none(),
        "unexpected frame tag {saw_terminal_frame:?} on stdout after stdin closed"
    );

    let status = child.wait().expect("wait on worker");
    assert!(
        !status.success(),
        "worker must exit nonzero when the host disappears before Finish"
    );

    assert!(
        !out.exists(),
        "output file must be absent after a host-gone run"
    );
    let leftovers = staging_leftovers(&dir);
    assert!(
        leftovers.is_empty(),
        "staging file(s) left behind after host-gone cleanup: {leftovers:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 9. read_band_ack_on_write_session_errors
// ---------------------------------------------------------------------------

/// A host that sends `ReadBandAck` (tag [`tag::READ_BAND_ACK`]) — a
/// read-direction-only reply — on a `SessionMode::Write` session must be
/// refused: `main.rs`'s `run_write` gained a dedicated match arm for this
/// exact frame (added alongside Task 3's read mode, to keep `HostEvent`'s
/// match exhaustive), and this test is the positive proof that arm actually
/// rejects it rather than silently accepting or panicking. Driven directly
/// (no `testhost` helper, matching `version_mismatch_errors`'s style above)
/// since this is a single malformed frame, not a full write session: a
/// valid `Init` handshake, then `ReadBandAck { slot: 0 }` in place of the
/// `Band` frame a write session actually expects.
#[test]
fn read_band_ack_on_write_session_errors() {
    let dir = tmpdir("write-rejects-readbandack");
    let out = dir.join("out.tif");
    let (width, height, channels, slots) = (8u64, 8u64, 1u64, 1u32);
    let slot_bytes = width * 4;

    let shm_name = format!("/mpp-e2e-wrba-{}", std::process::id() % 100_000);
    let _shm = ShmSegment::create(&shm_name, slot_bytes as usize * slots as usize)
        .expect("create shm segment");

    let mut child = Command::new(env!("CARGO_BIN_EXE_mpp-ipc-worker"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn mpp-ipc-worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");

    let init = InitMessage {
        protocol_version: IPC_PROTOCOL_VERSION,
        worker_version: env!("CARGO_PKG_VERSION").to_string(),
        output_path: out.to_string_lossy().into_owned(),
        width,
        height,
        channels,
        bit_depth: "f32".to_string(),
        shm_name: shm_name.clone(),
        slot_bytes,
        slots,
        mode: SessionMode::Write,
        ..Default::default()
    };
    let payload = serde_json::to_vec(&init).unwrap();
    write_frame(&mut stdin, tag::INIT, &payload).expect("write Init frame");
    stdin.flush().expect("flush Init frame");

    // A read-direction reply on a write session: never valid per
    // `docs/pi-protocol.md`'s direction split, and never sent by any real
    // host in write mode — this is exactly what `run_write`'s new match arm
    // exists to reject.
    write_frame(&mut stdin, tag::READ_BAND_ACK, &0u32.to_le_bytes())
        .expect("write ReadBandAck frame");
    stdin.flush().expect("flush ReadBandAck frame");

    let (t, payload) = read_frame(&mut stdout).expect("read worker's reply frame");
    assert_eq!(t, tag::ERROR, "expected an Error frame");
    let msg: ErrorMessage = serde_json::from_slice(&payload).expect("Error payload is valid JSON");
    assert!(
        msg.message.contains("ReadBandAck"),
        "error must name ReadBandAck: {}",
        msg.message
    );

    let status = child.wait().expect("wait on worker");
    assert!(
        !status.success(),
        "a ReadBandAck on a write session must exit nonzero"
    );

    assert!(
        !out.exists(),
        "output file must be absent after a rejected run"
    );
    // `run_write`'s `ReadBandAck` arm returns straight through `fail`
    // without an explicit `cleanup()` call (unlike the `Cancel`/`HostGone`
    // arms), but `Staging`'s `Drop` impl best-effort removes its scratch
    // file regardless of how the owning `Staging` value goes out of scope
    // (see `staging.rs`) — the same guarantee every other error return in
    // the `Band` arm above already relies on.
    let leftovers = staging_leftovers(&dir);
    assert!(
        leftovers.is_empty(),
        "staging file(s) left behind: {leftovers:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 10. u32_bands_reach_the_file_bit_exact
// ---------------------------------------------------------------------------

/// Protocol v3's reason for existing: `u32` samples must survive the trip
/// through shm unchanged, including values `f32` cannot represent.
///
/// 16_777_217 is the first integer above `f32`'s 24-bit mantissa and
/// 4_000_000_001 is past `f32`'s integer resolution entirely, so this fails
/// loudly the moment a band is routed through the unit-range `f32` domain —
/// which is exactly what the worker used to do unconditionally.
#[test]
fn u32_bands_reach_the_file_bit_exact() {
    let dir = tmpdir("e2e-u32");
    let out = dir.join("out.tif");
    let values: Vec<u32> = vec![0, 1, 16_777_217, 4_000_000_001, u32::MAX, 42];

    let cfg = TestHostConfig {
        bit_depth: "u32".to_string(),
        band_format: "u32".to_string(),
        band_rows: 1,
        slots: 1,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            values.len() as u64,
            1,
            1,
        )
    };
    let bytes_written = run_write_session_typed(&cfg, |_c, _y, x: u64| values[x as usize])
        .expect("u32 write session should succeed");
    assert!(bytes_written > 0);

    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    match dec.read_image().unwrap() {
        DecodingResult::U32(data) => assert_eq!(data, values),
        other => panic!("expected U32 samples, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 11. f64_bands_reach_the_file_bit_exact
// ---------------------------------------------------------------------------

/// The other half of the v3 promise: `f64` samples reach the file with
/// their full mantissa. `f64::consts::PI` and `1.0 + f64::EPSILON` both
/// collapse to a different value if they pass through `f32`, so a
/// regression to the old behaviour is a hard failure, not a rounding
/// tolerance question.
#[test]
fn f64_bands_reach_the_file_bit_exact() {
    let dir = tmpdir("e2e-f64");
    let out = dir.join("out.tif");
    let values: Vec<f64> = vec![
        std::f64::consts::PI,
        1.0 + f64::EPSILON,
        -1.0e300,
        1.0 / 3.0,
    ];

    let cfg = TestHostConfig {
        bit_depth: "f64".to_string(),
        band_format: "f64".to_string(),
        band_rows: 1,
        slots: 1,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            values.len() as u64,
            1,
            1,
        )
    };
    run_write_session_typed(&cfg, |_c, _y, x: u64| values[x as usize])
        .expect("f64 write session should succeed");

    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    match dec.read_image().unwrap() {
        DecodingResult::F64(data) => assert_eq!(data, values),
        other => panic!("expected F64 samples, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 12. a_tiled_compressed_write_session_produces_a_tiled_file
// ---------------------------------------------------------------------------

/// The v3 `Init` write options are not decoration: `layout: "tiles"`,
/// `tile_size: 128` and `compression: "lzw"` must reach
/// `TiffWriteOptions` and show up as real tags in the output file. 300x200
/// is deliberately not a multiple of 128, so the encoder also has to handle
/// partial edge tiles.
#[test]
fn a_tiled_compressed_write_session_produces_a_tiled_file() {
    let dir = tmpdir("e2e-tiled");
    let out = dir.join("out.tif");
    let (width, height) = (300u64, 200u64);
    let pixel = |_c: u64, y: u64, x: u64| ((y * width + x) as f32) / (width as f32 * height as f32);

    let cfg = TestHostConfig {
        layout: "tiles".to_string(),
        tile_size: 128,
        compression: "lzw".to_string(),
        band_rows: 32,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            1,
        )
    };
    let bytes_written = run_write_session(&cfg, pixel).expect("tiled write session should succeed");
    assert!(bytes_written > 0);

    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::TileWidth).unwrap(), 128);
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::TileLength).unwrap(), 128);
    // TIFF compression 5 = LZW.
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::Compression).unwrap(), 5);
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
// 13. text_and_resolution_tags_reach_the_file
// ---------------------------------------------------------------------------

/// The remaining v3 metadata options make it from `Init` into the IFD:
/// `ImageDescription`, `Copyright`, `Software` and the resolution pair.
#[test]
fn text_and_resolution_tags_reach_the_file() {
    let dir = tmpdir("e2e-tags");
    let out = dir.join("out.tif");
    let cfg = TestHostConfig {
        description: "M31 mosaic".to_string(),
        copyright: "Copyright (c) 2026 Astrometrical".to_string(),
        software: "Mega Pixel Processor".to_string(),
        x_resolution: 300.0,
        y_resolution: 300.0,
        band_rows: 8,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            16,
            8,
            1,
        )
    };
    run_write_session(&cfg, |_c, _y, _x| 0.5).expect("write session should succeed");

    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(
        dec.get_tag_ascii_string(tiff::tags::Tag::ImageDescription)
            .unwrap(),
        "M31 mosaic"
    );
    assert_eq!(
        dec.get_tag_ascii_string(tiff::tags::Tag::Copyright)
            .unwrap(),
        "Copyright (c) 2026 Astrometrical"
    );
    assert_eq!(
        dec.get_tag_ascii_string(tiff::tags::Tag::Software).unwrap(),
        "Mega Pixel Processor"
    );
    // ResolutionUnit 2 = inch.
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::ResolutionUnit).unwrap(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------
// 14. u8_bands_reach_the_file_bit_exact
// ---------------------------------------------------------------------------

/// The last of the five `band_format`s to get an end-to-end exercise, and
/// the narrowest: `u8` bands must reach the file as 8-bit samples, byte for
/// byte, with `BitsPerSample = 8`.
///
/// Narrow rather than wide is the risk here. A `u8` band is one byte per
/// sample, so every stride, slot-size and staging calculation the worker
/// makes is at its smallest — an off-by-one that a 4-byte `f32` band
/// absorbs (its rows are 4x longer than the slot's alignment unit) shows up
/// immediately at width 1. Multiple rows and a 3-row image over a 2-row
/// band deliberately force a partial final band.
#[test]
fn u8_bands_reach_the_file_bit_exact() {
    let dir = tmpdir("e2e-u8");
    let out = dir.join("out.tif");
    let (width, height) = (5u64, 3u64);
    // Both endpoints of the range plus the two values a signed/unsigned mix-up
    // would corrupt (0x80 and 0xFF).
    let value = |y: u64, x: u64| -> u8 {
        const PATTERN: [u8; 15] = [
            0, 1, 127, 128, 255, 2, 254, 64, 192, 3, 253, 65, 191, 4, 252,
        ];
        PATTERN[(y * width + x) as usize]
    };

    let cfg = TestHostConfig {
        bit_depth: "u8".to_string(),
        band_format: "u8".to_string(),
        band_rows: 2,
        slots: 2,
        ..TestHostConfig::write(
            env!("CARGO_BIN_EXE_mpp-ipc-worker").into(),
            out.clone(),
            width,
            height,
            1,
        )
    };
    let bytes_written = run_write_session_typed(&cfg, |_c, y: u64, x: u64| value(y, x))
        .expect("u8 write session should succeed");
    assert!(bytes_written > 0);

    let expected: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| value(y, x)))
        .collect();

    let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.dimensions().unwrap(), (width as u32, height as u32));
    assert_eq!(
        dec.get_tag_u64_vec(tiff::tags::Tag::BitsPerSample).unwrap(),
        vec![8]
    );
    match dec.read_image().unwrap() {
        DecodingResult::U8(data) => assert_eq!(data, expected),
        other => panic!("expected U8 samples, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).unwrap();
}
