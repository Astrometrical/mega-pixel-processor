//! Reference host: a real `mpp-ipc-worker` child process driven exactly the
//! way `docs/pi-protocol.md` describes a host doing it.
//!
//! [`crate::testhost::run_write_session`] (and its variants) create the shm
//! segment, spawn the worker binary named by
//! [`crate::testhost::TestHostConfig::worker_exe`], run it
//! through `Init` → `Band`… → `Finish` → `Done`, and check the child's exit
//! code agrees with the terminal frame it sent — the same cross-check a
//! real host's process supervisor should make (`docs/pi-protocol.md` §4).
//! This is what Task 7's e2e suite drives the worker through, and — being
//! protocol-faithful rather than a mock — it is also a useful reference
//! while implementing Plan B's C++ host.
//!
//! Slot reuse here is simple and synchronous (fill a slot, send its
//! `Band`, block for the matching `BandAck` before reusing that slot id):
//! this module's job is correctness as a reference, not throughput.
//!
//! Protocol v3: [`TestHostConfig`] carries the widened `Init` option set,
//! and [`run_write_session_typed`] pushes bands in the session's own
//! sample type (`band_format`) rather than always in `f32`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::{
    BandHeader, DoneMessage, ErrorMessage, IPC_PROTOCOL_VERSION, InitMessage, SessionMode,
    read_frame, tag, write_frame,
};
use crate::shm::ShmSegment;

/// Configuration for one write session: the worker binary to spawn, the
/// image geometry/band shape to drive it with, and the protocol-v3 write
/// options to advertise in `Init`.
///
/// Every option field mirrors the like-named [`InitMessage`] field and
/// starts at that field's wire default, so a test spells out only what it
/// is actually exercising:
///
/// ```ignore
/// TestHostConfig {
///     bit_depth: "u32".to_string(),
///     band_format: "u32".to_string(),
///     ..TestHostConfig::write(worker_exe, out, 6, 1, 1)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TestHostConfig {
    /// Path to the `mpp-ipc-worker` binary to spawn.
    pub worker_exe: PathBuf,
    /// Output file path the worker will encode to.
    pub output_path: PathBuf,
    /// Image width in pixels.
    pub width: u64,
    /// Image height in pixels.
    pub height: u64,
    /// Channel count: `1`-`4` (2 and 4 need an [`alpha`](Self::alpha) mode).
    pub channels: u64,
    /// Output sample format: `"f32"`, `"f64"`, `"u32"`, `"u16"` or `"u8"`.
    pub bit_depth: String,
    /// Rows per `Band` frame (the last band of a channel may be shorter).
    pub band_rows: u32,
    /// Number of shm slots to create; bands are round-robined across them.
    pub slots: u32,
    /// **(v3)** Sample type of the band data placed in shm slots; also
    /// fixes the slot stride this host computes. Must match the element
    /// type of the `pixel` closure the session is driven with.
    pub band_format: String,
    /// **(v3)** `"auto"`, `"classic"` or `"big"`.
    pub container: String,
    /// **(v3)** `"strips"` or `"tiles"`.
    pub layout: String,
    /// **(v3)** Tile edge in pixels when `layout == "tiles"`.
    pub tile_size: u32,
    /// **(v3)** `"none"`, `"lzw"`, `"deflate"` or `"packbits"`.
    pub compression: String,
    /// **(v3)** Deflate level 1-9.
    pub deflate_level: u8,
    /// **(v3)** Horizontal differencing predictor.
    pub predictor: bool,
    /// **(v3)** `"none"`, `"unassociated"` or `"associated"`.
    pub alpha: String,
    /// **(v3)** Base64-encoded ICC profile, empty for none.
    pub icc_profile: String,
    /// **(v3)** `ImageDescription`, empty for none.
    pub description: String,
    /// **(v3)** `Copyright`, empty for none.
    pub copyright: String,
    /// **(v3)** `Software`, empty for none.
    pub software: String,
    /// **(v3)** Horizontal resolution; `0` writes no resolution tags.
    pub x_resolution: f64,
    /// **(v3)** Vertical resolution; `0` writes no resolution tags.
    pub y_resolution: f64,
    /// **(v3)** `"inch"` or `"cm"`.
    pub resolution_unit: String,
}

impl TestHostConfig {
    /// A write-session config for `worker_exe` encoding to `output_path` at
    /// the given geometry, with every other field at its protocol default
    /// (f32 bands into an f32 file, uncompressed strips, 8 rows per band,
    /// 2 slots). Struct-update syntax overrides what a test cares about.
    pub fn write(
        worker_exe: PathBuf,
        output_path: PathBuf,
        width: u64,
        height: u64,
        channels: u64,
    ) -> Self {
        let defaults = InitMessage::default();
        Self {
            worker_exe,
            output_path,
            width,
            height,
            channels,
            bit_depth: defaults.bit_depth,
            band_rows: 8,
            slots: 2,
            band_format: defaults.band_format,
            container: defaults.container,
            layout: defaults.layout,
            tile_size: defaults.tile_size,
            compression: defaults.compression,
            deflate_level: defaults.deflate_level,
            predictor: defaults.predictor,
            alpha: defaults.alpha,
            icc_profile: defaults.icc_profile,
            description: defaults.description,
            copyright: defaults.copyright,
            software: defaults.software,
            x_resolution: defaults.x_resolution,
            y_resolution: defaults.y_resolution,
            resolution_unit: defaults.resolution_unit,
        }
    }

    /// Bytes per sample of [`band_format`](Self::band_format) — the stride
    /// this host lays slots out at, and what a band's byte length is
    /// computed from. Panics on an unknown name, since a config naming a
    /// sample format the protocol has never had is a test bug, not a
    /// protocol case worth exercising through the worker.
    fn band_sample_bytes(&self) -> usize {
        match self.band_format.as_str() {
            "u8" => 1,
            "u16" => 2,
            "u32" | "f32" => 4,
            "f64" => 8,
            other => panic!("TestHostConfig::band_format {other:?} is not a sample format"),
        }
    }
}

/// Spawns the worker, streams `pixel(c, y, x) -> f32` through shm bands in
/// channel-major, row-ascending order, collects `Progress`, and expects
/// `Done`. Returns `bytes_written`.
///
/// Any protocol violation — an unexpected frame, an `Error` reply, or the
/// worker's exit code disagreeing with its own terminal frame — surfaces
/// loudly as `Err` rather than being silently tolerated, since this is the
/// reference implementation other hosts (including the eventual C++ one)
/// are checked against.
pub fn run_write_session(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
) -> Result<u64, String> {
    run_write_session_typed(cfg, pixel)
}

/// [`run_write_session`] for a session whose `band_format` is not `"f32"`:
/// `pixel` yields the session's own sample type, which this host copies
/// into shm bit-for-bit (protocol v3's whole point — nothing is routed
/// through the `f32` domain on the way in).
///
/// `S` must match [`TestHostConfig::band_format`]; a mismatch is reported
/// as `Err` rather than silently laying bands out at the wrong stride.
pub fn run_write_session_typed<S: bytemuck::Pod>(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> S,
) -> Result<u64, String> {
    run_bands_in_order(
        cfg,
        band_plan(cfg.height, cfg.channels, cfg.band_rows),
        pixel,
    )
}

/// As [`run_write_session`], but sends the same bands in reverse order — a
/// deterministic non-monotonic delivery order (no RNG dependency) that
/// exercises the worker's out-of-order coverage tracking, per
/// `docs/pi-protocol.md` §7.2 ("bands may arrive in any order").
pub fn run_write_session_shuffled(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
) -> Result<u64, String> {
    let mut bands = band_plan(cfg.height, cfg.channels, cfg.band_rows);
    bands.reverse();
    run_bands_in_order(cfg, bands, pixel)
}

/// Sends one filler band (channel 0, rows `0..min(band_rows, height)`, all
/// zero), waits for its `BandAck`, then sends `Cancel` and expects `Error {
/// message: "cancelled" }`, a nonzero exit, and no output file left behind
/// (`docs/pi-protocol.md` §7.4).
pub fn run_cancel_after_first_band(cfg: &TestHostConfig) -> Result<(), String> {
    let mut session = Session::start(cfg)?;
    let rows = cfg.height.min(cfg.band_rows as u64) as u32;
    let filler = vec![0f32; rows as usize * cfg.width as usize];
    session.send_band_and_await_ack(0, 0, rows, 0, &filler)?;
    session.cancel_and_await_error()?;
    if cfg.output_path.exists() {
        return Err(format!(
            "output file {:?} exists after a cancelled run, expected it to be absent",
            cfg.output_path
        ));
    }
    Ok(())
}

/// Sends only the first `bands_to_send` bands of the full in-order plan,
/// then `Finish`es without ever sending the rest — exercising the worker's
/// incomplete-coverage rejection (`docs/pi-protocol.md` §7.3, "the worker
/// sends `Error` naming the first uncovered `(channel, row)` pair"). Returns
/// the `Error` message on that expected path; `Err` for any other protocol
/// deviation (e.g. the worker accepting the incomplete run and sending
/// `Done` anyway).
pub fn run_write_session_incomplete(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
    bands_to_send: usize,
) -> Result<String, String> {
    let bands = band_plan(cfg.height, cfg.channels, cfg.band_rows);
    let mut session = Session::start(cfg)?;
    let mut buf: Vec<f32> = Vec::new();
    for (i, (channel, start_row, rows)) in bands.into_iter().take(bands_to_send).enumerate() {
        buf.clear();
        for y in start_row..start_row + rows as u64 {
            for x in 0..cfg.width {
                buf.push(pixel(channel, y, x));
            }
        }
        let slot_id = (i as u32) % session.slots;
        session.send_band_and_await_ack(channel, start_row, rows, slot_id, &buf)?;
    }
    session.finish_and_await_error()
}

/// Drives one band through a worker whose advertised `Init.slot_bytes` is
/// deliberately smaller than the band about to be sent — exercising the
/// slot-size validation `docs/pi-protocol.md` §6 requires (`rows * width *
/// 4 <= slot_bytes`) before the worker ever touches the shm slot. The
/// host's own shm segment is still physically sized to hold a real band (so
/// assembling and copying the band into it here is safe) — only the
/// `Init.slot_bytes` field the worker is told lies. Returns the `Error`
/// message on the expected rejection; `Err` for any other protocol
/// deviation (e.g. the worker accepting the oversize band and acking it).
pub fn run_oversize_band_session(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
    advertised_slot_bytes: u64,
) -> Result<String, String> {
    let mut session = Session::start_with_slot_bytes_override(cfg, advertised_slot_bytes)?;
    let rows = cfg.height.min(cfg.band_rows as u64) as u32;
    let mut buf = Vec::with_capacity(rows as usize * cfg.width as usize);
    for y in 0..rows as u64 {
        for x in 0..cfg.width {
            buf.push(pixel(0, y, x));
        }
    }
    match session.send_band_and_await_ack(0, 0, rows, 0, &buf) {
        // The worker is still alive, blocked waiting for the next frame
        // (we never send Finish/Cancel on this path) — don't call
        // `expect_exit` here, which would block on `child.wait()` forever.
        // `Session::drop` kills and reaps it instead, so this returns
        // promptly.
        Ok(()) => Err("worker acked an oversize band instead of rejecting it".to_string()),
        Err(msg) => {
            session.expect_exit(false)?;
            Ok(msg)
        }
    }
}

/// Fills every slot's worth of pixels via `pixel` and streams `bands` to
/// the worker in the order given, then `Finish`es and returns
/// `bytes_written` from `Done`. Shared by [`run_write_session`] and
/// [`run_write_session_shuffled`] — they differ only in the order of the
/// `bands` list they pass in.
fn run_bands_in_order<S: bytemuck::Pod>(
    cfg: &TestHostConfig,
    bands: Vec<BandSpec>,
    pixel: impl Fn(u64, u64, u64) -> S,
) -> Result<u64, String> {
    let mut session = Session::start(cfg)?;
    let mut buf: Vec<S> = Vec::new();
    for (i, (channel, start_row, rows)) in bands.into_iter().enumerate() {
        buf.clear();
        for y in start_row..start_row + rows as u64 {
            for x in 0..cfg.width {
                buf.push(pixel(channel, y, x));
            }
        }
        let slot_id = (i as u32) % session.slots;
        session.send_band_and_await_ack(channel, start_row, rows, slot_id, &buf)?;
    }
    session.finish_and_await_done()
}

/// One `(channel, start_row, rows)` band to send, in the order
/// [`band_plan`] returns them.
type BandSpec = (u64, u64, u32);

/// Every `(channel, start_row, rows)` band that covers a `height x
/// channels` image at `band_rows` rows per band, in channel-major,
/// row-ascending order (channel 0's bands top to bottom, then channel 1's,
/// …) — the "in order" default [`run_write_session`] sends.
fn band_plan(height: u64, channels: u64, band_rows: u32) -> Vec<BandSpec> {
    let mut bands = Vec::new();
    for c in 0..channels {
        let mut y = 0u64;
        while y < height {
            let rows = (height - y).min(band_rows as u64) as u32;
            bands.push((c, y, rows));
            y += rows as u64;
        }
    }
    bands
}

/// Cheap process-unique suffix for shm segment names so parallel tests
/// don't collide on the same name.
fn unique_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A spawned worker child, its shm segment, and the slot geometry backing
/// it — live for the duration of one session (one call to
/// [`run_bands_in_order`] or [`run_cancel_after_first_band`]).
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    shm: ShmSegment,
    slot_bytes: u64,
    slots: u32,
    /// Bytes per band sample, from `TestHostConfig::band_format` — the
    /// stride `slot_bytes` was computed at, checked against the element
    /// type of every band actually pushed.
    sample_bytes: usize,
}

impl Session {
    /// Creates the shm segment, spawns `cfg.worker_exe`, and writes the
    /// `Init` frame. The worker doesn't reply to `Init` itself — the first
    /// reply frame is the first `BandAck` (or an `Error`, if `Init` was
    /// rejected).
    fn start(cfg: &TestHostConfig) -> Result<Self, String> {
        let slot_bytes = cfg.band_rows as u64 * cfg.width * cfg.band_sample_bytes() as u64;
        Self::start_inner(cfg, slot_bytes, slot_bytes)
    }

    /// As [`start`](Self::start), but advertises `advertised_slot_bytes` to
    /// the worker in `Init` instead of the real, band-sized `slot_bytes` —
    /// used only by [`run_oversize_band_session`] to exercise the worker's
    /// slot-size validation. The host's own shm segment is still physically
    /// sized for a real band (`physical_slot_bytes`), so filling it stays
    /// safe even though the worker is told a smaller size.
    fn start_with_slot_bytes_override(
        cfg: &TestHostConfig,
        advertised_slot_bytes: u64,
    ) -> Result<Self, String> {
        let physical_slot_bytes = cfg.band_rows as u64 * cfg.width * cfg.band_sample_bytes() as u64;
        Self::start_inner(cfg, physical_slot_bytes, advertised_slot_bytes)
    }

    /// Shared implementation of [`start`](Self::start) and
    /// [`start_with_slot_bytes_override`](Self::start_with_slot_bytes_override):
    /// creates a shm segment physically `physical_slot_bytes * cfg.slots`
    /// bytes (always large enough for real band data), spawns
    /// `cfg.worker_exe`, and writes the `Init` frame advertising
    /// `advertised_slot_bytes` as `InitMessage::slot_bytes` (equal to
    /// `physical_slot_bytes` in the normal case). [`Session::slot_bytes`]
    /// tracks the *physical* stride, since that's what governs where this
    /// host itself writes slot data — the advertised value only affects
    /// what the worker is told to validate against.
    fn start_inner(
        cfg: &TestHostConfig,
        physical_slot_bytes: u64,
        advertised_slot_bytes: u64,
    ) -> Result<Self, String> {
        let shm_name = format!("/mpp-th-{}-{}", std::process::id() % 100_000, unique_id());
        let total_bytes = physical_slot_bytes as usize * cfg.slots as usize;
        let shm = ShmSegment::create(&shm_name, total_bytes)
            .map_err(|e| format!("creating shm segment {shm_name:?}: {e}"))?;

        let mut child = Command::new(&cfg.worker_exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawning worker {:?}: {e}", cfg.worker_exe))?;
        let mut stdin = child.stdin.take().expect("worker spawned with piped stdin");
        let stdout = child
            .stdout
            .take()
            .expect("worker spawned with piped stdout");

        let init = InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: env!("CARGO_PKG_VERSION").to_string(),
            output_path: cfg.output_path.to_string_lossy().into_owned(),
            width: cfg.width,
            height: cfg.height,
            channels: cfg.channels,
            bit_depth: cfg.bit_depth.clone(),
            shm_name: shm_name.clone(),
            slot_bytes: advertised_slot_bytes,
            slots: cfg.slots,
            mode: SessionMode::Write,
            band_format: cfg.band_format.clone(),
            container: cfg.container.clone(),
            layout: cfg.layout.clone(),
            tile_size: cfg.tile_size,
            compression: cfg.compression.clone(),
            deflate_level: cfg.deflate_level,
            predictor: cfg.predictor,
            alpha: cfg.alpha.clone(),
            icc_profile: cfg.icc_profile.clone(),
            description: cfg.description.clone(),
            copyright: cfg.copyright.clone(),
            software: cfg.software.clone(),
            x_resolution: cfg.x_resolution,
            y_resolution: cfg.y_resolution,
            resolution_unit: cfg.resolution_unit.clone(),
        };
        let payload = serde_json::to_vec(&init).map_err(|e| format!("encoding Init: {e}"))?;
        write_frame(&mut stdin, tag::INIT, &payload)
            .map_err(|e| format!("writing Init frame: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flushing Init frame: {e}"))?;

        Ok(Session {
            child,
            stdin,
            stdout,
            shm,
            slot_bytes: physical_slot_bytes,
            slots: cfg.slots,
            sample_bytes: cfg.band_sample_bytes(),
        })
    }

    /// Fills slot `slot_id` with `samples` (native-endian, via
    /// `bytemuck::cast_slice`, matching `docs/pi-protocol.md` §6's shm
    /// pixel layout) and sends the matching `Band` frame, then blocks
    /// until the corresponding `BandAck` arrives — collecting/ignoring any
    /// interleaved `Progress` frames along the way, and failing loudly on
    /// an `Error` reply or any other unexpected tag.
    fn send_band_and_await_ack<S: bytemuck::Pod>(
        &mut self,
        channel: u64,
        start_row: u64,
        rows: u32,
        slot_id: u32,
        samples: &[S],
    ) -> Result<(), String> {
        if std::mem::size_of::<S>() != self.sample_bytes {
            return Err(format!(
                "band sample type is {} bytes but the session's band_format is {} bytes; \
                 the pixel closure and TestHostConfig::band_format must agree",
                std::mem::size_of::<S>(),
                self.sample_bytes
            ));
        }
        let offset = self.slot_bytes as usize * slot_id as usize;
        let byte_len = std::mem::size_of_val(samples);
        self.shm.as_mut_slice()[offset..offset + byte_len]
            .copy_from_slice(bytemuck::cast_slice(samples));

        let header = BandHeader {
            channel: channel as u32,
            start_row,
            rows,
            slot: slot_id,
        };
        write_frame(&mut self.stdin, tag::BAND, &header.to_bytes())
            .map_err(|e| format!("writing Band frame (slot={slot_id}): {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flushing Band frame (slot={slot_id}): {e}"))?;

        loop {
            let (t, payload) = read_frame(&mut self.stdout)
                .map_err(|e| format!("reading reply to Band(slot={slot_id}): {e}"))?;
            match t {
                tag::BAND_ACK => {
                    let acked = decode_band_ack(&payload)?;
                    if acked != slot_id {
                        return Err(format!("worker acked slot {acked}, expected {slot_id}"));
                    }
                    return Ok(());
                }
                tag::PROGRESS => continue,
                tag::ERROR => return Err(decode_error(&payload)?),
                other => {
                    return Err(format!(
                        "unexpected tag {other} while waiting for BandAck(slot={slot_id})"
                    ));
                }
            }
        }
    }

    /// Sends `Finish` and reads frames until `Done` (returns
    /// `bytes_written`) or `Error` (returns `Err`), collecting/ignoring any
    /// interleaved `Progress` frames. Checks the worker's exit code agrees
    /// with which terminal frame it sent.
    fn finish_and_await_done(mut self) -> Result<u64, String> {
        write_frame(&mut self.stdin, tag::FINISH, &[])
            .map_err(|e| format!("writing Finish frame: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flushing Finish frame: {e}"))?;

        let outcome: Result<u64, String> = loop {
            let (t, payload) = match read_frame(&mut self.stdout) {
                Ok(f) => f,
                Err(e) => break Err(format!("reading reply to Finish: {e}")),
            };
            match t {
                tag::PROGRESS => continue,
                tag::DONE => break decode_done(&payload),
                tag::ERROR => break decode_error(&payload),
                other => break Err(format!("unexpected tag {other} while waiting for Done")),
            }
        };

        self.expect_exit(outcome.is_ok())?;
        outcome
    }

    /// Sends `Finish` and reads frames until `Error` (returns the error
    /// message) — the expected outcome when the host stopped short of
    /// covering the whole image, per `docs/pi-protocol.md` §7.3. `Err` is
    /// returned instead for any other protocol deviation, including the
    /// worker sending `Done` anyway. Checks the worker's exit code agrees
    /// with the failure, like [`finish_and_await_done`](Self::finish_and_await_done).
    fn finish_and_await_error(mut self) -> Result<String, String> {
        write_frame(&mut self.stdin, tag::FINISH, &[])
            .map_err(|e| format!("writing Finish frame: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flushing Finish frame: {e}"))?;

        let outcome: Result<String, String> = loop {
            let (t, payload) = match read_frame(&mut self.stdout) {
                Ok(f) => f,
                Err(e) => break Err(format!("reading reply to Finish: {e}")),
            };
            match t {
                tag::PROGRESS => continue,
                tag::ERROR => {
                    let msg: ErrorMessage = match serde_json::from_slice(&payload) {
                        Ok(m) => m,
                        Err(e) => break Err(format!("malformed Error payload: {e}")),
                    };
                    break Ok(msg.message);
                }
                tag::DONE => {
                    break Err(
                        "worker sent Done after an incomplete Finish, expected Error".to_string(),
                    );
                }
                other => {
                    break Err(format!(
                        "unexpected tag {other} while waiting for Finish's Error reply"
                    ));
                }
            }
        };

        self.expect_exit(false)?;
        outcome
    }

    /// Sends `Cancel` and reads frames until `Error` (the only allowed
    /// terminal reply per `docs/pi-protocol.md` §7.4), collecting/ignoring
    /// interleaved `Progress`. Errors loudly if the worker instead sends
    /// `Done`.
    fn cancel_and_await_error(mut self) -> Result<(), String> {
        write_frame(&mut self.stdin, tag::CANCEL, &[])
            .map_err(|e| format!("writing Cancel frame: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flushing Cancel frame: {e}"))?;

        let outcome: Result<(), String> = loop {
            let (t, payload) = match read_frame(&mut self.stdout) {
                Ok(f) => f,
                Err(e) => break Err(format!("reading reply to Cancel: {e}")),
            };
            match t {
                tag::PROGRESS => continue,
                tag::ERROR => {
                    let msg: ErrorMessage = match serde_json::from_slice(&payload) {
                        Ok(m) => m,
                        Err(e) => break Err(format!("malformed Error payload: {e}")),
                    };
                    if msg.message != "cancelled" {
                        break Err(format!(
                            "expected Error {{\"cancelled\"}} after Cancel, got {:?}",
                            msg.message
                        ));
                    }
                    break Ok(());
                }
                tag::DONE => break Err("worker sent Done after Cancel, expected Error".to_string()),
                other => {
                    break Err(format!(
                        "unexpected tag {other} while waiting for Cancel's Error reply"
                    ));
                }
            }
        };

        self.expect_exit(false)?;
        outcome
    }

    /// Waits for the child to exit and checks its exit code matches
    /// `expect_success`. Per `docs/pi-protocol.md` §4 the exit code is the
    /// authoritative completion signal, so a `Done`/`Error` frame that
    /// disagrees with it is itself a protocol violation this reference
    /// host surfaces loudly rather than trusting the frame alone.
    fn expect_exit(&mut self, expect_success: bool) -> Result<(), String> {
        let status = self
            .child
            .wait()
            .map_err(|e| format!("waiting for worker to exit: {e}"))?;
        if status.success() != expect_success {
            return Err(format!(
                "worker exit status {status} did not match its own terminal frame \
                 (expected success={expect_success})"
            ));
        }
        Ok(())
    }
}

impl Drop for Session {
    /// Best-effort reap so an early-return error path (one that never
    /// reaches [`Session::expect_exit`]) doesn't leave a zombie or
    /// still-running worker process behind across a test run. Only kills
    /// the child if it's still running — [`Session::expect_exit`] having
    /// already waited on the happy/expected-failure paths makes the
    /// `try_wait` here a no-op then.
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Decodes a `BandAck` payload (4-byte `slot: u32 LE`).
fn decode_band_ack(payload: &[u8]) -> Result<u32, String> {
    let bytes: [u8; 4] = payload.try_into().map_err(|_| {
        format!(
            "malformed BandAck payload: {} bytes, expected 4",
            payload.len()
        )
    })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Decodes a `Done` JSON payload.
fn decode_done(payload: &[u8]) -> Result<u64, String> {
    let msg: DoneMessage =
        serde_json::from_slice(payload).map_err(|e| format!("malformed Done payload: {e}"))?;
    Ok(msg.bytes_written)
}

/// Decodes an `Error` JSON payload into a descriptive `Err`. Always
/// returns `Err` (never `Ok`) — the generic `T` lets it slot into either a
/// `Result<u64, String>` or `Result<(), String>` call site via `?`/`match`
/// without a cast at the call site.
fn decode_error<T>(payload: &[u8]) -> Result<T, String> {
    let msg: ErrorMessage =
        serde_json::from_slice(payload).map_err(|e| format!("malformed Error payload: {e}"))?;
    Err(format!("worker reported error: {}", msg.message))
}
