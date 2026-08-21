//! `mpp-ipc-worker`: the PixInsight-side worker process.
//!
//! Reads one `Init` handshake from stdin, attaches to the shm segment the
//! host already created, stages every `Band` the host sends into a
//! memory-mapped scratch file ([`staging::Staging`]), and — once `Finish`
//! confirms every row of every channel arrived — streams the staged planes
//! through [`mpp_core::formats::tiff::write_bigtiff`] and reports `Done`.
//! `Cancel` (or the host disappearing before `Finish`) tears down cleanly
//! instead. See `docs/pi-protocol.md` for the full wire contract this
//! implements; `run` below follows its §7 lifecycle section arm by arm.

mod staging;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use mpp_core::formats::tiff::{BitDepth, TiffWriteOptions, write_bigtiff};
use mpp_ipc::link::{HostEvent, HostLink};
use mpp_ipc::protocol::InitMessage;
use mpp_ipc::shm::ShmSegment;

use staging::Staging;

/// This binary's own release version — what a host's
/// `InitMessage::worker_version` must equal exactly
/// (`docs/pi-protocol.md` §5/§8).
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut link = HostLink::new(stdin.lock(), stdout.lock());
    match run(&mut link) {
        Ok(()) => {}
        Err(WorkerError::Reported(message)) => {
            eprintln!("mpp-ipc-worker: {message}");
            std::process::exit(1);
        }
        Err(WorkerError::HostGone) => {
            eprintln!("mpp-ipc-worker: host stdin closed before Finish; exiting");
            std::process::exit(1);
        }
    }
}

/// What [`main`] maps to an exit code. Every variant but
/// [`HostGone`](Self::HostGone) means an `Error` frame was already sent
/// (best effort) at the point of failure — see [`fail`] — so `main` only
/// needs to log and exit nonzero.
#[derive(Debug)]
enum WorkerError {
    /// An `Error` frame carrying this message was sent (or attempted, if
    /// the pipe was already broken); `main` also prints it to stderr for
    /// local diagnostics.
    Reported(String),
    /// The host's stdin reached clean EOF before `Finish` arrived. Per
    /// `docs/pi-protocol.md` §7.5 and [`HostEvent::HostGone`]'s own docs,
    /// this is not reported back over the control channel — the host is
    /// the reason that channel exists, and it is the one that is gone.
    HostGone,
}

/// Drives one worker run to completion over `link`: handshake, validate,
/// attach shm, stage every band, encode, and report `Done`/`Error`.
///
/// Structured as its own function (rather than inline in `main`) so the
/// exit-code mapping in `main` stays trivial and this logic — every branch
/// of which must send `Error` before returning, except
/// [`WorkerError::HostGone`] — is reviewable as one linear story. Every
/// fallible step below is written as an explicit `match`/`if let` (not
/// `.map_err(|e| fail(link, ...))`) so it's visually obvious `link` is
/// never captured or moved away from later steps.
fn run<R: Read, W: Write>(link: &mut HostLink<R, W>) -> Result<(), WorkerError> {
    let init = match link.read_init(WORKER_VERSION) {
        Ok(init) => init,
        Err(e) => return Err(fail(link, &e.to_string())),
    };
    let bit_depth = match validate_init(&init) {
        Ok(bd) => bd,
        Err(e) => return Err(fail(link, &e)),
    };

    let total_bytes = match shm_total_bytes(&init) {
        Ok(n) => n,
        Err(e) => return Err(fail(link, &e)),
    };
    let shm = match ShmSegment::open(&init.shm_name, total_bytes) {
        Ok(shm) => shm,
        Err(e) => {
            return Err(fail(
                link,
                &format!("opening shm segment {:?}: {e}", init.shm_name),
            ));
        }
    };

    let output_path = PathBuf::from(&init.output_path);
    let staging_path = staging_path_for(&output_path);
    let mut staging = match Staging::create(&staging_path, init.width, init.height, init.channels) {
        Ok(s) => s,
        Err(e) => {
            return Err(fail(
                link,
                &format!("creating staging file {}: {e}", staging_path.display()),
            ));
        }
    };

    let rows_total = init.height.saturating_mul(init.channels);
    let progress_every = (init.height / 100).max(1);
    let mut last_progress: u64 = 0;

    loop {
        match link.next_event() {
            Ok(HostEvent::Band(hdr)) => {
                let need_bytes = match band_byte_len(hdr.rows, init.width) {
                    Some(n) => n,
                    None => {
                        return Err(fail(
                            link,
                            &format!(
                                "band channel {} start_row {} rows {} overflows computing \
                                 byte length",
                                hdr.channel, hdr.start_row, hdr.rows
                            ),
                        ));
                    }
                };
                if need_bytes > init.slot_bytes {
                    return Err(fail(
                        link,
                        &format!(
                            "band channel {} start_row {} rows {} needs {need_bytes} bytes, \
                             exceeds slot_bytes {}",
                            hdr.channel, hdr.start_row, hdr.rows, init.slot_bytes
                        ),
                    ));
                }
                if hdr.slot >= init.slots {
                    return Err(fail(
                        link,
                        &format!(
                            "band references slot {} but the shm segment only has {} slots",
                            hdr.slot, init.slots
                        ),
                    ));
                }

                // Safe to index: `hdr.slot < init.slots` (just checked) and
                // `need_bytes <= init.slot_bytes` (just checked), so
                // `offset + need_bytes <= init.slot_bytes * init.slots ==
                // total_bytes`, which `shm_total_bytes` already proved fits
                // `usize`.
                let offset = init.slot_bytes as usize * hdr.slot as usize;
                let slot_data = &shm.as_slice()[offset..offset + need_bytes as usize];
                let samples: &[f32] = bytemuck::cast_slice(slot_data);

                if let Err(e) = staging.write_band(hdr.channel as u64, hdr.start_row, samples) {
                    return Err(fail(link, &e.to_string()));
                }

                if let Err(e) = link.send_band_ack(hdr.slot) {
                    return Err(fail(link, &e.to_string()));
                }

                // `rows_received` (summed across channels, matching
                // `ProgressMessage`'s field docs) is the source of truth
                // for progress, rather than a hand-rolled counter — it's
                // also what makes a duplicate/overlapping band (rejected
                // above by `write_band`, so unreachable here) impossible
                // to double-count.
                let rows_done = staging.rows_received();
                if rows_done.saturating_sub(last_progress) >= progress_every
                    || rows_done >= rows_total
                {
                    if let Err(e) = link.send_progress(rows_done, rows_total) {
                        return Err(fail(link, &e.to_string()));
                    }
                    last_progress = rows_done;
                }
            }
            Ok(HostEvent::Finish) => {
                if !staging.is_complete() {
                    let (c, y) = staging
                        .first_uncovered()
                        .expect("is_complete() is false, so first_uncovered() must return Some");
                    return Err(fail(
                        link,
                        &format!(
                            "incomplete coverage at Finish: channel {c} row {y} is uncovered \
                             (was never received)"
                        ),
                    ));
                }
                break;
            }
            Ok(HostEvent::Cancel) => {
                cleanup(staging, &output_path);
                let _ = link.send_error("cancelled");
                return Err(WorkerError::Reported("cancelled".to_string()));
            }
            Ok(HostEvent::HostGone) => {
                cleanup(staging, &output_path);
                return Err(WorkerError::HostGone);
            }
            Err(e) => {
                return Err(fail(link, &e.to_string()));
            }
        }
    }

    // `staging` is fully covered here: the `Finish` arm above already
    // returned early if `first_uncovered()` found a gap, so every `row()`
    // call `write_bigtiff` makes below reads real data, never the
    // zero-filled uncovered case documented on `Staging`'s `PlanarRows`
    // impl (see `staging.rs`).
    let opts = TiffWriteOptions { bit_depth };
    if let Err(e) = write_bigtiff(&staging, &output_path, &opts) {
        // The encoder may have left a partial file behind; best-effort
        // clean it up rather than leaving a corrupt-looking output file
        // for the host to trip over after we've already reported failure.
        let _ = std::fs::remove_file(&output_path);
        return Err(fail(
            link,
            &format!("encoding {}: {e}", output_path.display()),
        ));
    }

    if let Err(e) = staging.remove() {
        // Cosmetic cleanup failure only: the output file is already
        // written and correct, so this does not fail the run — just note
        // it on stderr. (`Staging::drop` won't retry: `remove` marks
        // itself done regardless of whether the filesystem call
        // succeeded.)
        eprintln!(
            "mpp-ipc-worker: warning: failed to remove staging file {}: {e}",
            staging_path.display()
        );
    }

    let bytes_written = match std::fs::metadata(&output_path) {
        Ok(m) => m.len(),
        Err(e) => {
            return Err(fail(
                link,
                &format!("stat output file {}: {e}", output_path.display()),
            ));
        }
    };

    if let Err(e) = link.send_done(bytes_written) {
        return Err(fail(link, &e.to_string()));
    }
    Ok(())
}

/// Validates an `Init` payload beyond what [`HostLink::read_init`] already
/// checked (protocol/worker version), and parses `bit_depth` into a
/// [`BitDepth`] so callers don't re-parse it.
///
/// Per Task 5's carried review finding, `width == 0 || height == 0` is
/// rejected here rather than left to fail obscurely inside
/// `Staging::create`'s zero-length mmap.
fn validate_init(init: &InitMessage) -> Result<BitDepth, String> {
    if init.width == 0 || init.height == 0 {
        return Err(format!(
            "width and height must both be nonzero (got {}x{})",
            init.width, init.height
        ));
    }
    if init.channels != 1 && init.channels != 3 {
        return Err(format!("channels must be 1 or 3, got {}", init.channels));
    }
    let bit_depth = match init.bit_depth.as_str() {
        "f32" => BitDepth::F32,
        "u16" => BitDepth::U16,
        "u8" => BitDepth::U8,
        other => {
            return Err(format!(
                "unknown bit_depth {other:?} (expected \"f32\", \"u16\", or \"u8\")"
            ));
        }
    };
    if init.slots == 0 {
        return Err("slots must be >= 1".to_string());
    }
    if init.slot_bytes == 0 || !init.slot_bytes.is_multiple_of(4) {
        return Err(format!(
            "slot_bytes {} must be nonzero and a multiple of 4 (f32 alignment)",
            init.slot_bytes
        ));
    }
    let min_slot_bytes = match init.width.checked_mul(4) {
        Some(n) => n,
        None => {
            return Err(format!(
                "width {} is too large: width*4 overflows",
                init.width
            ));
        }
    };
    if init.slot_bytes < min_slot_bytes {
        return Err(format!(
            "slot_bytes {} cannot hold even one row of width {} (needs >= {min_slot_bytes})",
            init.slot_bytes, init.width
        ));
    }
    Ok(bit_depth)
}

/// `slot_bytes * slots` as a `usize` — the size to open the shm segment
/// at. A plain `String` error (not a full error type) since this only
/// ever feeds straight into [`fail`].
fn shm_total_bytes(init: &InitMessage) -> Result<usize, String> {
    let total = init
        .slot_bytes
        .checked_mul(init.slots as u64)
        .ok_or_else(|| {
            format!(
                "slot_bytes {} * slots {} overflows u64",
                init.slot_bytes, init.slots
            )
        })?;
    usize::try_from(total)
        .map_err(|_| format!("shm segment size {total} exceeds this platform's addressable memory"))
}

/// `rows * width * 4`, checked against overflow — the byte length one
/// `Band` frame's slot data must have.
fn band_byte_len(rows: u32, width: u64) -> Option<u64> {
    (rows as u64).checked_mul(width)?.checked_mul(4)
}

/// The staging scratch file path for `output_path`: `<output>.mpp-stage-<pid>`
/// in the same directory as the output file. Co-locating keeps the staging
/// file on the same filesystem as the final output (relevant if the output
/// directory is a mount with more room than a system temp dir), and the
/// pid suffix keeps concurrent runs targeting the same output path from
/// colliding.
fn staging_path_for(output_path: &Path) -> PathBuf {
    let mut name = output_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("mpp-ipc-worker-output"))
        .to_os_string();
    name.push(format!(".mpp-stage-{}", std::process::id()));
    match output_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

/// Best-effort cleanup for the `Cancel`/`HostGone` paths: removes the
/// staging file (via `Staging::remove`, consuming it) and any partial
/// output file. Errors from either removal are ignored — this runs on an
/// already-failing path where there's no further frame to report a
/// secondary cleanup failure through.
fn cleanup(staging: Staging, output_path: &Path) {
    let _ = staging.remove();
    let _ = std::fs::remove_file(output_path);
}

/// Sends `Error { message }` (best effort — the pipe may already be
/// broken) and returns the [`WorkerError`] the caller should propagate.
///
/// Centralizing this is what makes every fallible step in [`run`] visibly
/// "report, then fail" in one line, rather than each call site separately
/// remembering to call `send_error`.
fn fail<R: Read, W: Write>(link: &mut HostLink<R, W>, message: &str) -> WorkerError {
    let _ = link.send_error(message);
    WorkerError::Reported(message.to_string())
}
