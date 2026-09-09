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

/// Like [`write_rgb16`] but forces the whole image into a SINGLE strip
/// (`RowsPerStrip == height`), the geometry real single-strip astro TIFFs
/// use. `TiffReader::read_bands` then yields one band per channel covering
/// every row — a band whose byte length (`height * width * sample_bytes`)
/// far exceeds a fixed ~4 MiB slot, so the worker must sub-divide it.
fn write_rgb16_single_strip(path: &Path, w: u32, h: u32) {
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
    let mut image = enc.new_image::<colortype::RGB16>(w, h).unwrap();
    // One strip spanning the full height: RowsPerStrip == height.
    image.rows_per_strip(h).unwrap();
    image.write_data(&data).unwrap();
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
            ..Default::default()
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
    // (this small image encodes as a single strip spanning the full
    // height, well under this bound).
    let slot_bytes = ref_info.width * 32 * sample_bytes as u64;
    // `total_channels` is 3 (RGB, no alpha) and, because the `tiff` crate
    // encodes an image this small as a single strip, `TiffReader::read_bands`
    // calls `sink` exactly 3 times — one full-height band per channel, back
    // to back, with no other bands in between. `slots` is deliberately set
    // *below* that count (2 < 3) so the third `sink` call cannot find a free
    // slot and must block in `run_read`'s free-slot-pool reclaim loop
    // (`while free_slots.is_empty() { link.next_event() ... }`), waiting for
    // this test's `ReadSession` (playing the host) to ack an earlier slot
    // before the worker reuses it — this is the exact block/reclaim/reuse
    // path a 3-slots-for-3-bands run can never exercise, since every band
    // would get its own never-reused slot.
    let slots = 2u32;
    assert!(
        (slots as usize) < total_channels,
        "test setup: slots ({slots}) must be fewer than the {total_channels} bands this image \
         produces, so the read exercises slot reuse"
    );

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

/// Regression for the "oversize band" failure: a single-strip file whose
/// per-channel band is far larger than one shm slot. `TiffReader::read_bands`
/// yields ONE band per channel spanning all rows, so the worker MUST split
/// each band into slot-sized sub-bands (its own slot + ack apiece) or it
/// cannot open a large fraction of the files this feature advertises. Asserts
/// byte-for-byte reassembly against a direct `TiffReader` decode.
#[test]
fn oversize_single_strip_band_reassembles_via_subbands() {
    let dir = tmpdir("subband");
    let path = dir.join("in.tif");
    let (w, h) = (8u32, 64u32);
    write_rgb16_single_strip(&path, w, h);

    // Reference decode straight through TiffReader for the byte comparison.
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

    // Arithmetic proving band > slot (the exact regression condition):
    //   sample_bytes = 2 (u16)
    //   row_bytes    = width * sample_bytes      = 8 * 2  = 16 bytes
    //   band_bytes   = height * row_bytes        = 64 * 16 = 1024 bytes  (one
    //                  per-channel band, since RowsPerStrip == height == 64)
    //   slot_bytes   = row_bytes * 8             = 16 * 8 = 128 bytes
    //   max_rows     = slot_bytes / row_bytes    = 128 / 16 = 8 rows
    //   sub-bands    = ceil(64 / 8)              = 8 per channel
    // So each whole-strip channel band (1024 B) is 8x the slot (128 B) and
    // must be split into 8 sub-bands; before the fix the worker rejected it
    // outright as an "oversize band".
    let row_bytes = w as u64 * sample_bytes as u64;
    let max_rows = 8u64;
    let slot_bytes = row_bytes * max_rows; // 128
    let band_bytes = h as u64 * row_bytes; // 1024
    assert!(
        band_bytes > slot_bytes,
        "regression precondition: a whole-strip channel band ({band_bytes} B) must exceed \
         slot_bytes ({slot_bytes} B) so the worker is forced to sub-divide it"
    );
    // Fewer slots than the sub-bands one channel produces, so the run also
    // exercises the block/reclaim/reuse path across sub-bands.
    let slots = 3u32;

    let mut session = ReadSession::start(&path, slot_bytes, slots);
    let meta = session.expect_image_meta();
    assert_eq!(meta.width, ref_info.width);
    assert_eq!(meta.height, ref_info.height);
    assert_eq!(meta.channels, ref_info.total_channels() as u64);
    assert_eq!(ref_info.sample_format, SampleFormat::U16);

    let planes = session.drain_bands(ref_info.width, sample_bytes, plane_len_bytes);
    session.expect_exit_success();

    assert_eq!(planes.len(), total_channels);
    for (ch, (got, want)) in planes.iter().zip(ref_planes.iter()).enumerate() {
        assert_eq!(
            got, want,
            "channel {ch} plane mismatch after sub-band reassembly"
        );
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

/// A minimal `PlanarRows<f32>` source: `w * h` all-zero pixels on `c`
/// channels. Only used to get a file on disk through the real
/// `mpp_core::formats::tiff_write` writer — this test cares about the
/// embedded ICC profile carried in `ImageMeta`, not pixel content.
struct ZeroPlanar {
    w: u64,
    h: u64,
    c: u64,
    zero_row: Vec<f32>,
}

impl mpp_core::formats::PlanarRows<f32> for ZeroPlanar {
    fn width(&self) -> u64 {
        self.w
    }
    fn height(&self) -> u64 {
        self.h
    }
    fn channels(&self) -> u64 {
        self.c
    }
    fn row(&self, _c: u64, _y: u64) -> &[f32] {
        &self.zero_row
    }
}

/// End-to-end: a file written (by our own writer) with an embedded ICC
/// profile must have that exact profile come back out through a real
/// worker's `ImageMeta`, base64-decoded. Proves the read path's ICC
/// support all the way across the IPC boundary, not just inside
/// `TiffReader` (already covered by `mpp-core`'s own unit tests).
#[test]
fn image_meta_carries_the_embedded_icc_profile_to_the_host() {
    use base64::Engine as _;
    use mpp_core::formats::tiff_write::{TiffWriteOptions, write_tiff};

    let dir = tmpdir("icc");
    let path = dir.join("icc.tif");
    let icc: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
    let img = ZeroPlanar {
        w: 4,
        h: 4,
        c: 1,
        zero_row: vec![0.0; 4],
    };
    write_tiff(
        &img,
        &path,
        &TiffWriteOptions {
            icc_profile: Some(icc.clone()),
            ..Default::default()
        },
    )
    .unwrap();

    let mut session = ReadSession::start(&path, 4096, 4);
    let meta = session.expect_image_meta();
    assert!(
        !meta.icc_profile.is_empty(),
        "expected a non-empty base64 icc_profile field"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&meta.icc_profile)
        .expect("icc_profile is valid base64");
    assert_eq!(decoded, icc);

    // Drain to a clean exit rather than leaving the child dangling.
    let _ = session.drain_bands(4, 4, 4 * 4 * 4);
    session.expect_exit_success();

    std::fs::remove_dir_all(&dir).unwrap();
}
