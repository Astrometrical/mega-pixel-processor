//! Cross-process proof that the real `mpp-ipc-worker` binary, spawned in
//! `SessionMode::Read`, drives the worker-pull direction of the wire
//! protocol correctly: it opens an existing TIFF via
//! `mpp_core::formats::tiff_read::TiffReader`, reports `ImageMeta` (its own
//! authoritative geometry, not anything the host asserted), then streams
//! every channel's rows as `READ_BAND` requests over shm, acked by this
//! test acting as the host.
//!
//! Modeled on the write-direction e2e (`tests/end_to_end.rs`): same
//! `Command`/`Stdio::piped()` spawn pattern, same raw `read_frame`/
//! `write_frame` control-channel plumbing, same real POSIX shm segment via
//! `mpp_ipc::shm::ShmSegment` — just driven in the opposite direction (the
//! worker requests bands; this test, playing the host, fills nothing and
//! only acks).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use mpp_core::formats::tiff_read::{ColorClass, SampleFormat, TiffReader};
use mpp_ipc::protocol::{
    BandHeader, IPC_PROTOCOL_VERSION, ImageMeta, InitMessage, SessionMode, read_frame, tag,
    write_frame,
};
use mpp_ipc::shm::ShmSegment;
use tiff::encoder::{TiffEncoder, colortype};

/// A fresh, empty scratch directory for one test, isolated by `tag` and
/// this process's pid so parallel `cargo test` runs never collide.
fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mpp-ipcworker-read-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a small RGB16 TIFF at `path`: `pixel(c, y, x)` for `c` in
/// `0..3`, deterministic but distinct per channel/row/col so a
/// reassembly bug (wrong channel, wrong row, transposed axes) shows up as
/// a mismatch rather than accidentally passing.
fn write_rgb16(path: &Path, w: u32, h: u32) {
    let mut enc = TiffEncoder::new(std::fs::File::create(path).unwrap()).unwrap();
    let mut data = vec![0u16; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            data[i] = (x as u16).wrapping_mul(7).wrapping_add(1);
            data[i + 1] = (y as u16).wrapping_mul(11).wrapping_add(2);
            data[i + 2] = ((x + y) as u16).wrapping_mul(13).wrapping_add(3);
        }
    }
    enc.write_image::<colortype::RGB16>(w, h, &data).unwrap();
}

/// A read-mode session driven exactly the way a real host would: creates
/// the shm segment, spawns the worker with `Init { mode: Read, .. }`,
/// reads `ImageMeta`, then acks every `READ_BAND` until `Done`/`Error`.
struct ReadSession {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    shm: ShmSegment,
    slot_bytes: u64,
}

/// Cheap process-unique suffix for shm segment names so tests running in
/// parallel (`cargo test` runs test fns as threads within one process, so
/// `std::process::id()` alone is not enough) never collide on the same
/// name — mirrors `mpp_ipc::testhost`'s own `unique_id`.
fn unique_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl ReadSession {
    fn start(input_path: &Path, slot_bytes: u64, slots: u32) -> Self {
        let shm_name = format!("/mpp-th-rd{}-{}", std::process::id() % 100_000, unique_id());
        let total_bytes = slot_bytes as usize * slots as usize;
        let shm = ShmSegment::create(&shm_name, total_bytes).expect("create shm segment");

        let mut child = Command::new(env!("CARGO_BIN_EXE_mpp-ipc-worker"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn mpp-ipc-worker");
        let mut stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");

        // Geometry/bit_depth here are host-asserted placeholders (the real
        // fields the worker reports authoritatively via ImageMeta) — the
        // worker in Read mode derives its own from the file, per
        // `docs/pi-protocol.md`'s design intent for ImageMeta.
        let init = InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: env!("CARGO_PKG_VERSION").to_string(),
            output_path: input_path.to_string_lossy().into_owned(),
            width: 0,
            height: 0,
            channels: 0,
            bit_depth: String::new(),
            shm_name: shm_name.clone(),
            slot_bytes,
            slots,
            mode: SessionMode::Read,
        };
        let payload = serde_json::to_vec(&init).unwrap();
        write_frame(&mut stdin, tag::INIT, &payload).expect("write Init frame");
        stdin.flush().expect("flush Init frame");

        ReadSession {
            child,
            stdin,
            stdout,
            shm,
            slot_bytes,
        }
    }

    /// Reads the first reply frame and decodes it as `ImageMeta`, panicking
    /// loudly on anything else (in particular an `Error` frame, whose
    /// message is surfaced directly).
    fn expect_image_meta(&mut self) -> ImageMeta {
        let (t, payload) = read_frame(&mut self.stdout).expect("read ImageMeta reply");
        match t {
            tag::IMAGE_META => serde_json::from_slice(&payload).expect("ImageMeta is valid JSON"),
            tag::ERROR => {
                let msg: mpp_ipc::protocol::ErrorMessage =
                    serde_json::from_slice(&payload).expect("Error payload is valid JSON");
                panic!(
                    "worker reported error instead of ImageMeta: {}",
                    msg.message
                );
            }
            other => panic!(
                "expected ImageMeta (tag {}), got tag {other}",
                tag::IMAGE_META
            ),
        }
    }

    /// Reads frames until `Done`/`Error`, acking every `READ_BAND` request
    /// by copying its slot's bytes out of shm into `planes[channel]` at
    /// `start_row`, then sending `ReadBandAck { slot }`. Returns the
    /// assembled per-channel byte planes on success.
    fn drain_bands(
        &mut self,
        width: u64,
        sample_bytes: usize,
        plane_len_bytes: usize,
    ) -> Vec<Vec<u8>> {
        let mut planes: Vec<Vec<u8>> = Vec::new();
        loop {
            let (t, payload) = read_frame(&mut self.stdout).expect("read reply frame");
            match t {
                tag::READ_BAND => {
                    let hdr = BandHeader::from_bytes(&payload).expect("valid BandHeader payload");
                    let ch = hdr.channel as usize;
                    while planes.len() <= ch {
                        planes.push(vec![0u8; plane_len_bytes]);
                    }
                    let offset = self.slot_bytes as usize * hdr.slot as usize;
                    let byte_len = hdr.rows as usize * width as usize * sample_bytes;
                    let src = &self.shm.as_slice()[offset..offset + byte_len];
                    let dst_off = hdr.start_row as usize * width as usize * sample_bytes;
                    planes[ch][dst_off..dst_off + byte_len].copy_from_slice(src);

                    write_frame(&mut self.stdin, tag::READ_BAND_ACK, &hdr.slot.to_le_bytes())
                        .expect("write ReadBandAck frame");
                    self.stdin.flush().expect("flush ReadBandAck frame");
                }
                tag::DONE => break,
                tag::ERROR => {
                    let msg: mpp_ipc::protocol::ErrorMessage =
                        serde_json::from_slice(&payload).expect("Error payload is valid JSON");
                    panic!(
                        "worker reported error while streaming bands: {}",
                        msg.message
                    );
                }
                other => panic!("unexpected tag {other} while draining READ_BAND frames"),
            }
        }
        planes
    }

    fn expect_exit_success(&mut self) {
        let status = self.child.wait().expect("wait on worker");
        assert!(status.success(), "read session worker must exit 0");
    }
}

impl Drop for ReadSession {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// End-to-end: write a small RGB16 TIFF, spawn the worker in Read mode,
/// collect `ImageMeta` + every `READ_BAND`, reassemble per-channel planes,
/// and assert both the geometry and the pixel data equal a direct
/// `TiffReader` decode of the same file.
#[test]
fn rgb16_read_session_matches_direct_tiff_reader_decode() {
    let dir = tmpdir("rgb16");
    let path = dir.join("in.tif");
    let (w, h) = (24u32, 16u32);
    write_rgb16(&path, w, h);

    // Reference decode straight through TiffReader, independent of the IPC
    // machinery, to compare the worker's IPC-streamed output against.
    let mut reference = TiffReader::open(&path).unwrap();
    let ref_info = reference.info().clone();
    let sample_bytes = ref_info.sample_format.bytes();
    let total_channels = ref_info.total_channels() as usize;
    let plane_len_bytes = ref_info.height as usize * ref_info.width as usize * sample_bytes;
    let mut ref_planes = vec![vec![0u8; plane_len_bytes]; total_channels];
    reference
        .read_bands(&mut |ch, start_row, _rows, bytes| {
            let off = start_row as usize * ref_info.width as usize * sample_bytes;
            ref_planes[ch as usize][off..off + bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    // Slot sized generously for a whole band of the largest possible chunk
    // (this small image encodes as one strip per row range well under this
    // bound); several slots so the worker's free-slot pool actually
    // exercises more than one slot across the run.
    let slot_bytes = ref_info.width * 32 * sample_bytes as u64;
    let slots = 3u32;

    let mut session = ReadSession::start(&path, slot_bytes, slots);
    let meta = session.expect_image_meta();
    assert_eq!(meta.width, ref_info.width);
    assert_eq!(meta.height, ref_info.height);
    assert_eq!(meta.channels, ref_info.total_channels() as u64);
    assert_eq!(meta.alpha, ref_info.alpha_channels as u64);
    assert_eq!(
        meta.color_space,
        match ref_info.color {
            ColorClass::Gray => "gray",
            ColorClass::Rgb => "rgb",
        }
    );
    assert_eq!(meta.bit_depth, ref_info.sample_format.wire_name());
    assert_eq!(ref_info.sample_format, SampleFormat::U16);

    let planes = session.drain_bands(ref_info.width, sample_bytes, plane_len_bytes);
    session.expect_exit_success();

    assert_eq!(planes.len(), total_channels);
    for (ch, (got, want)) in planes.iter().zip(ref_planes.iter()).enumerate() {
        assert_eq!(got, want, "channel {ch} plane mismatch");
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A read session whose `Init.output_path` names a file that isn't a valid
/// TIFF must fail fast with an `Error` frame naming the problem and a
/// nonzero exit — mirroring the write path's "report, then fail" contract
/// (`docs/pi-protocol.md` §7.1's spirit, applied to the read direction).
#[test]
fn read_session_rejects_unreadable_file() {
    let dir = tmpdir("bad-file");
    let path = dir.join("not-a-tiff.tif");
    std::fs::write(&path, b"not a tiff file at all").unwrap();

    let mut session = ReadSession::start(&path, 4096, 1);
    let (t, payload) = read_frame(&mut session.stdout).expect("read reply to bad Init");
    assert_eq!(t, tag::ERROR, "expected an Error frame");
    let msg: mpp_ipc::protocol::ErrorMessage =
        serde_json::from_slice(&payload).expect("Error payload is valid JSON");
    assert!(!msg.message.is_empty());

    let status = session.child.wait().expect("wait on worker");
    assert!(!status.success(), "a rejected read Init must exit nonzero");

    std::fs::remove_dir_all(&dir).unwrap();
}
