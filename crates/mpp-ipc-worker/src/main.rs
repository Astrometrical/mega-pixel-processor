//! `mpp-ipc-worker`: the PixInsight-side worker process.
//!
//! Two directions, selected by `Init.mode` (`docs/pi-protocol.md`'s v2
//! addition):
//!
//! - [`SessionMode::Write`] (the original v1 flow, [`run_write`]): the
//!   worker attaches to the host's shm segment, stages every `Band` the
//!   host sends into a memory-mapped scratch file ([`staging::Staging`]),
//!   and — once `Finish` confirms every row of every channel arrived —
//!   streams the staged planes through
//!   [`mpp_core::formats::tiff_write::write_tiff`] and reports `Done`. `Cancel`
//!   (or the host disappearing before `Finish`) tears down cleanly instead.
//!   Since protocol v3 the staging file holds the session's *own* sample
//!   type (`Init.band_format`), not unit-range `f32`: [`run_write`] parses
//!   that field and dispatches to [`run_write_typed`], so `u32` and `f64`
//!   pixels reach the encoder with every bit intact.
//! - [`SessionMode::Read`] ([`read::run_read`]): the worker opens an
//!   existing image via [`TiffReader`], reports its geometry as
//!   `ImageMeta`, and pulls bands out to the host over the same shm
//!   segment, direction reversed (see `read.rs`'s module docs).
//!
//! See `docs/pi-protocol.md` for the full wire contract this implements;
//! [`run_write`] follows its §7 lifecycle section arm by arm.
//!
//! A `--probe <path>` CLI entry (checked before the stdin handshake, in
//! [`main`]) opens `path` and prints its [`ImageMeta`](mpp_ipc::protocol::ImageMeta)
//! as standalone JSON — a debugging aid for inspecting a file's geometry
//! without spinning up a full host session.

mod read;
mod staging;

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use mpp_core::formats::tiff_read::TiffReader;
use mpp_core::formats::tiff_write::{
    AlphaMode, BitDepth, Compression, Container, Layout, Resolution, ResolutionUnit, Sample,
    TiffWriteOptions, write_tiff, write_tiff_native,
};
use mpp_ipc::link::{HostEvent, HostLink};
use mpp_ipc::protocol::{InitMessage, SessionMode};
use mpp_ipc::shm::ShmSegment;

use staging::Staging;

/// This binary's own release version — what a host's
/// `InitMessage::worker_version` must equal exactly
/// (`docs/pi-protocol.md` §5/§8).
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    if let Some(path) = probe_path_arg() {
        match TiffReader::open(Path::new(&path)) {
            Ok(reader) => {
                let meta = read::image_meta(reader.info());
                // `serde_json::to_string` on a plain-data struct like
                // `ImageMeta` cannot fail, so unwrap rather than threading a
                // Result through this standalone diagnostic path.
                println!(
                    "{}",
                    serde_json::to_string(&meta).expect("ImageMeta serializes to JSON")
                );
            }
            Err(e) => {
                eprintln!("mpp-ipc-worker --probe: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

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

/// Returns `Some(path)` if this process was invoked as `mpp-ipc-worker
/// --probe <path>`, checked before anything touches stdin so the probe
/// entry never blocks waiting for an `Init` handshake that will never come.
fn probe_path_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    if args.next()?.as_str() != "--probe" {
        return None;
    }
    args.next()
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

/// Reads the `Init` handshake and dispatches on [`InitMessage::mode`]:
/// [`SessionMode::Write`] to [`run_write`] (the v1 flow), [`SessionMode::Read`]
/// to [`read::run_read`] (the v2 worker-pull flow). Both report `Done`/`Error`
/// over `link` themselves — this function only routes.
fn run<R: Read, W: Write>(link: &mut HostLink<R, W>) -> Result<(), WorkerError> {
    let init = match link.read_init(WORKER_VERSION) {
        Ok(init) => init,
        Err(e) => return Err(fail(link, &e.to_string())),
    };
    match init.mode {
        SessionMode::Write => run_write(link, &init),
        SessionMode::Read => read::run_read(link, &init),
    }
}

/// Everything [`run_write_typed`] needs from a validated `Init`.
#[derive(Debug)]
struct WriteSetup {
    /// Sample type of the bands arriving over shm
    /// (`InitMessage::band_format`), which is also the type the staging
    /// file holds.
    band_format: BitDepth,
    /// Fully populated writer options, including the output file's own
    /// sample format in [`TiffWriteOptions::bit_depth`].
    options: TiffWriteOptions,
}

/// How one band sample type reaches the encoder.
///
/// Two shapes, and only two, because `mpp-core` offers exactly two entry
/// points: `f32` bands are the unit-range domain
/// [`write_tiff`] converts from,
/// so an `f32` session can target any output `bit_depth` (this is the v1/v2
/// behaviour, unchanged); every other band type is written verbatim by
/// [`write_tiff_native`],
/// which is the whole point of protocol v3 and requires the output format
/// to match — [`validate_init`] rejects a mismatch up front rather than
/// letting it surface after a multi-gigabyte staging run.
trait BandSample: Sample {
    /// Encode the (fully covered) `staging` image to `path` per `opts`.
    fn encode_staging(
        staging: &Staging<Self>,
        path: &Path,
        opts: &TiffWriteOptions,
    ) -> mpp_core::Result<()>;
}

impl BandSample for f32 {
    fn encode_staging(
        staging: &Staging<Self>,
        path: &Path,
        opts: &TiffWriteOptions,
    ) -> mpp_core::Result<()> {
        // The converting path: unit-range f32 in, any `opts.bit_depth` out.
        write_tiff(staging, path, opts)
    }
}

/// `impl BandSample` for a type written verbatim through `write_tiff_native`.
macro_rules! native_band_sample {
    ($t:ty) => {
        impl BandSample for $t {
            fn encode_staging(
                staging: &Staging<Self>,
                path: &Path,
                opts: &TiffWriteOptions,
            ) -> mpp_core::Result<()> {
                write_tiff_native(staging, path, opts)
            }
        }
    };
}

native_band_sample!(f64);
native_band_sample!(u32);
native_band_sample!(u16);
native_band_sample!(u8);

/// Drives one write-mode run to completion over `link`: validate the
/// handshake, then hand off to [`run_write_typed`] instantiated at the
/// session's `Init.band_format` sample type.
///
/// The dispatch lives here and nowhere else, so the staging file, the shm
/// slot cast and the encoder all agree on one sample type by construction.
fn run_write<R: Read, W: Write>(
    link: &mut HostLink<R, W>,
    init: &InitMessage,
) -> Result<(), WorkerError> {
    let setup = match validate_init(init) {
        Ok(setup) => setup,
        Err(e) => return Err(fail(link, &e)),
    };
    match setup.band_format {
        BitDepth::F32 => run_write_typed::<f32, R, W>(link, init, &setup),
        BitDepth::F64 => run_write_typed::<f64, R, W>(link, init, &setup),
        BitDepth::U32 => run_write_typed::<u32, R, W>(link, init, &setup),
        BitDepth::U16 => run_write_typed::<u16, R, W>(link, init, &setup),
        BitDepth::U8 => run_write_typed::<u8, R, W>(link, init, &setup),
    }
}

/// One write-mode run with `T` as the band (and staging) sample type:
/// attach shm, stage every band, encode, and report `Done`/`Error`.
///
/// Structured as its own function (rather than inline in `run`) so the
/// exit-code mapping in `main` stays trivial and this logic — every branch
/// of which must send `Error` before returning, except
/// [`WorkerError::HostGone`] — is reviewable as one linear story. Every
/// fallible step below is written as an explicit `match`/`if let` (not
/// `.map_err(|e| fail(link, ...))`) so it's visually obvious `link` is
/// never captured or moved away from later steps.
fn run_write_typed<T: BandSample, R: Read, W: Write>(
    link: &mut HostLink<R, W>,
    init: &InitMessage,
    setup: &WriteSetup,
) -> Result<(), WorkerError> {
    let sample_bytes = std::mem::size_of::<T>() as u64;

    let total_bytes = match shm_total_bytes(init) {
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
    let mut staging =
        match Staging::<T>::create(&staging_path, init.width, init.height, init.channels) {
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
                let need_bytes = match band_byte_len(hdr.rows, init.width, sample_bytes) {
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
                // `validate_init` required `slot_bytes` to be a multiple of
                // the band sample width, so `offset` — and therefore this
                // slice — is aligned for `T`. `try_cast_slice` reports
                // rather than panics if that reasoning is ever wrong: a
                // panic here would abort the worker without an `Error`
                // frame (the release profile is `panic = "abort"`).
                let samples: &[T] = match bytemuck::try_cast_slice(slot_data) {
                    Ok(s) => s,
                    Err(e) => {
                        return Err(fail(
                            link,
                            &format!(
                                "shm slot {} at offset {offset} is not usable as {} samples: {e}",
                                hdr.slot, init.band_format
                            ),
                        ));
                    }
                };

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
            Ok(HostEvent::ReadBandAck { slot }) => {
                // `ReadBandAck` only makes sense on a `SessionMode::Read`
                // session (`read::run_read` handles it there); a host
                // sending it on a write session is a protocol violation.
                return Err(fail(
                    link,
                    &format!("unexpected ReadBandAck {{ slot: {slot} }} on a write-mode session"),
                ));
            }
            Err(e) => {
                return Err(fail(link, &e.to_string()));
            }
        }
    }

    // `staging` is fully covered here: the `Finish` arm above already
    // returned early if `first_uncovered()` found a gap, so every `row()`
    // call the encoder makes below reads real data, never the zero-filled
    // uncovered case documented on `Staging`'s `PlanarRows` impl (see
    // `staging.rs`).
    if let Err(e) = T::encode_staging(&staging, &output_path, &setup.options) {
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

/// Largest ICC profile the worker will accept, decoded. Real profiles are
/// kilobytes; the cap exists so a malformed `Init` cannot make the worker
/// allocate without bound before anything else has validated.
const MAX_ICC_BYTES: usize = 16 << 20;

/// `None` for an empty string, `Some(owned)` otherwise — the shape
/// `TiffWriteOptions`'s optional text tags want, from the wire's
/// "empty means absent" convention.
fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Decodes `InitMessage::icc_profile`: `None` for the empty string,
/// otherwise the standard-alphabet base64 bytes. Invalid base64 and a
/// profile over [`MAX_ICC_BYTES`] decoded are both rejected, each naming
/// which of the two it was.
fn decode_icc(b64: &str) -> Result<Option<Vec<u8>>, String> {
    if b64.is_empty() {
        return Ok(None);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("icc_profile is not valid base64: {e}"))?;
    if bytes.len() > MAX_ICC_BYTES {
        return Err(format!(
            "icc_profile is {} bytes decoded, over the {MAX_ICC_BYTES}-byte limit",
            bytes.len()
        ));
    }
    Ok(Some(bytes))
}

/// Parses one of the protocol's sample-format strings into a [`BitDepth`],
/// naming `field` (`"bit_depth"` or `"band_format"`) in the error.
fn parse_depth(s: &str, field: &str) -> Result<BitDepth, String> {
    match s {
        "f32" => Ok(BitDepth::F32),
        "f64" => Ok(BitDepth::F64),
        "u32" => Ok(BitDepth::U32),
        "u16" => Ok(BitDepth::U16),
        "u8" => Ok(BitDepth::U8),
        other => Err(format!(
            "unknown {field} {other:?} (expected \"f32\", \"f64\", \"u32\", \"u16\" or \"u8\")"
        )),
    }
}

/// Validates an `Init` payload beyond what [`HostLink::read_init`] already
/// checked (protocol/worker version) and turns it into the [`WriteSetup`]
/// the write path runs on.
///
/// Everything about *legal option combinations* — channel count, alpha
/// agreement, predictor-vs-float, tile size, deflate level, ASCII-only text
/// tags, classic-container size bound — is delegated to
/// [`TiffWriteOptions::validate`], the same validator the CLI path goes
/// through, so those rules live in exactly one place. What is checked here
/// is what only the IPC layer knows about: geometry that would break the
/// mmap, the shm slot geometry, and the `band_format`/`bit_depth`
/// relationship the two `mpp-core` encode entry points can actually honour.
///
/// Per Task 5's carried review finding, `width == 0 || height == 0` is
/// rejected here rather than left to fail obscurely inside
/// `Staging::create`'s zero-length mmap.
fn validate_init(init: &InitMessage) -> Result<WriteSetup, String> {
    if init.width == 0 || init.height == 0 {
        return Err(format!(
            "width and height must both be nonzero (got {}x{})",
            init.width, init.height
        ));
    }
    if init.slots == 0 {
        return Err("slots must be >= 1".to_string());
    }

    let bit_depth = parse_depth(&init.bit_depth, "bit_depth")?;
    let band_format = parse_depth(&init.band_format, "band_format")?;
    if band_format != BitDepth::F32 && band_format != bit_depth {
        return Err(format!(
            "band_format {:?} must equal bit_depth {:?}: native bands are written to the \
             file verbatim, and only the unit-range \"f32\" band format is converted",
            init.band_format, init.bit_depth
        ));
    }

    // Validated unconditionally, like every other v3 string enum: an
    // unrecognised name is refused rather than silently defaulting to inch,
    // which would denominate the resolution tags in a unit the host did not
    // ask for. (The numeric fields §10.2 calls "genuinely ignored" --
    // `tile_size`, `deflate_level` -- are the deliberate exception; the
    // enums are not among them.)
    let resolution_unit = match init.resolution_unit.as_str() {
        "inch" => ResolutionUnit::Inch,
        "cm" => ResolutionUnit::Centimeter,
        other => {
            return Err(format!(
                "unknown resolution unit {other:?} (expected \"inch\" or \"cm\")"
            ));
        }
    };

    let options = TiffWriteOptions {
        bit_depth,
        container: match init.container.as_str() {
            "auto" => Container::Auto,
            "classic" => Container::Classic,
            "big" => Container::Big,
            other => {
                return Err(format!(
                    "unknown container {other:?} (expected \"auto\", \"classic\" or \"big\")"
                ));
            }
        },
        layout: match init.layout.as_str() {
            "strips" => Layout::Strips,
            "tiles" => Layout::Tiles {
                size: init.tile_size,
            },
            other => {
                return Err(format!(
                    "unknown layout {other:?} (expected \"strips\" or \"tiles\")"
                ));
            }
        },
        compression: match init.compression.as_str() {
            "none" => Compression::None,
            "lzw" => Compression::Lzw,
            "deflate" => Compression::Deflate,
            "packbits" => Compression::Packbits,
            other => {
                return Err(format!(
                    "unknown compression {other:?} (expected \"none\", \"lzw\", \"deflate\" \
                     or \"packbits\")"
                ));
            }
        },
        deflate_level: Some(init.deflate_level),
        predictor: init.predictor,
        alpha: match init.alpha.as_str() {
            "none" => AlphaMode::None,
            "unassociated" => AlphaMode::Unassociated,
            "associated" => AlphaMode::Associated,
            other => {
                return Err(format!(
                    "unknown alpha mode {other:?} (expected \"none\", \"unassociated\" or \
                     \"associated\")"
                ));
            }
        },
        icc_profile: decode_icc(&init.icc_profile)?,
        description: non_empty(&init.description),
        copyright: non_empty(&init.copyright),
        software: non_empty(&init.software),
        resolution: (init.x_resolution > 0.0 && init.y_resolution > 0.0).then_some(Resolution {
            x: init.x_resolution,
            y: init.y_resolution,
            unit: resolution_unit,
        }),
    };
    // One place owns the consistency rules: mpp-core's validator, the same
    // one the CLI path goes through.
    options
        .validate(init.width, init.height, init.channels)
        .map_err(|e| e.to_string())?;

    // Slot geometry. The alignment rule is the band sample width, not a
    // hard-coded 4: it is what makes the `bytemuck` cast of a slot to
    // `&[T]` in `run_write_typed` well-aligned.
    let sample_bytes = band_format.bytes() as u64;
    if init.slot_bytes == 0 || !init.slot_bytes.is_multiple_of(sample_bytes) {
        return Err(format!(
            "slot_bytes {} must be nonzero and a multiple of {sample_bytes} ({} alignment)",
            init.slot_bytes, init.band_format
        ));
    }
    let min_slot_bytes = match init.width.checked_mul(sample_bytes) {
        Some(n) => n,
        None => {
            return Err(format!(
                "width {} is too large: width*{sample_bytes} overflows",
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

    Ok(WriteSetup {
        band_format,
        options,
    })
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

/// `rows * width * sample_bytes`, checked against overflow — the byte
/// length one `Band` frame's slot data must have, at the session's
/// negotiated band sample width.
fn band_byte_len(rows: u32, width: u64, sample_bytes: u64) -> Option<u64> {
    (rows as u64).checked_mul(width)?.checked_mul(sample_bytes)
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
fn cleanup<T: bytemuck::Pod>(staging: Staging<T>, output_path: &Path) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use mpp_ipc::protocol::IPC_PROTOCOL_VERSION;

    /// A minimal, valid write-mode `Init` at the protocol's own defaults:
    /// 64x32 grayscale f32 bands into an f32 file, with slots big enough
    /// for four rows of any of the five sample widths. Tests mutate one
    /// field at a time from here.
    fn test_init() -> InitMessage {
        InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: WORKER_VERSION.to_string(),
            output_path: "/tmp/mpp-validate-init.tif".to_string(),
            width: 64,
            height: 32,
            channels: 1,
            bit_depth: "f32".to_string(),
            shm_name: "/mpp-validate-init".to_string(),
            slot_bytes: 4096,
            slots: 2,
            mode: SessionMode::Write,
            band_format: "f32".to_string(),
            container: "auto".to_string(),
            layout: "strips".to_string(),
            tile_size: 256,
            compression: "none".to_string(),
            deflate_level: 6,
            predictor: false,
            alpha: "none".to_string(),
            icc_profile: String::new(),
            description: String::new(),
            copyright: String::new(),
            software: String::new(),
            x_resolution: 0.0,
            y_resolution: 0.0,
            resolution_unit: "inch".to_string(),
        }
    }

    #[test]
    fn a_defaulted_v2_init_still_validates_to_the_v2_behaviour() {
        let setup = validate_init(&test_init()).unwrap();
        assert_eq!(setup.band_format, BitDepth::F32);
        assert_eq!(setup.options.bit_depth, BitDepth::F32);
        assert_eq!(setup.options.container, Container::Auto);
        assert_eq!(setup.options.layout, Layout::Strips);
        assert_eq!(setup.options.compression, Compression::None);
        assert_eq!(setup.options.alpha, AlphaMode::None);
        assert!(!setup.options.predictor);
        assert!(setup.options.icc_profile.is_none());
        assert!(setup.options.description.is_none());
        assert!(setup.options.copyright.is_none());
        assert!(setup.options.software.is_none());
        assert!(setup.options.resolution.is_none());
    }

    #[test]
    fn validate_init_accepts_the_widened_options() {
        let mut init = test_init();
        init.channels = 4;
        init.alpha = "associated".to_string();
        init.bit_depth = "f64".to_string();
        init.band_format = "f64".to_string();
        init.layout = "tiles".to_string();
        init.tile_size = 256;
        init.compression = "lzw".to_string();
        let setup = validate_init(&init).unwrap();
        assert_eq!(setup.band_format, BitDepth::F64);
        assert_eq!(setup.options.bit_depth, BitDepth::F64);
        assert_eq!(setup.options.layout, Layout::Tiles { size: 256 });
        assert_eq!(setup.options.compression, Compression::Lzw);
        assert_eq!(setup.options.alpha, AlphaMode::Associated);
    }

    #[test]
    fn validate_init_rejects_inconsistent_options() {
        /// One named mutation of a valid `Init` that must be rejected.
        type Case = (&'static str, Box<dyn Fn(&mut InitMessage)>);
        let cases: Vec<Case> = vec![
            ("channels", Box::new(|i: &mut InitMessage| i.channels = 5)),
            (
                "alpha",
                Box::new(|i: &mut InitMessage| {
                    i.channels = 3;
                    i.alpha = "associated".to_string();
                }),
            ),
            (
                "band_format",
                Box::new(|i: &mut InitMessage| i.band_format = "u4".to_string()),
            ),
            (
                "tile_size",
                Box::new(|i: &mut InitMessage| {
                    i.layout = "tiles".to_string();
                    i.tile_size = 300;
                }),
            ),
            (
                "predictor",
                Box::new(|i: &mut InitMessage| {
                    i.predictor = true;
                    i.bit_depth = "f32".to_string();
                }),
            ),
            (
                "container",
                Box::new(|i: &mut InitMessage| i.container = "huge".to_string()),
            ),
            (
                "layout",
                Box::new(|i: &mut InitMessage| i.layout = "rows".to_string()),
            ),
            (
                "compression",
                Box::new(|i: &mut InitMessage| i.compression = "jpeg".to_string()),
            ),
            (
                "alpha_name",
                Box::new(|i: &mut InitMessage| i.alpha = "premultiplied".to_string()),
            ),
            (
                "deflate_level",
                Box::new(|i: &mut InitMessage| {
                    i.compression = "deflate".to_string();
                    i.deflate_level = 12;
                }),
            ),
            (
                "description",
                Box::new(|i: &mut InitMessage| i.description = "caf\u{e9}".to_string()),
            ),
            (
                "icc_profile",
                Box::new(|i: &mut InitMessage| i.icc_profile = "not base64!!".to_string()),
            ),
            (
                "resolution_unit",
                Box::new(|i: &mut InitMessage| {
                    i.x_resolution = 300.0;
                    i.y_resolution = 300.0;
                    i.resolution_unit = "centimeter".to_string();
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut init = test_init();
            mutate(&mut init);
            let err = validate_init(&init).expect_err(&format!("{name} should have been rejected"));
            assert!(!err.is_empty(), "{name}: error message must not be empty");
        }
    }

    #[test]
    fn an_ignored_deflate_level_cannot_reject_an_init() {
        // `deflate_level` is documented as ignored unless the compression
        // is deflate, so a host that never set it — 0 being the natural
        // default for an unset `uint8_t` on the C++ side — must not have
        // every run rejected over it.
        for compression in ["none", "lzw", "packbits"] {
            let mut init = test_init();
            init.compression = compression.to_string();
            init.deflate_level = 0;
            assert!(
                validate_init(&init).is_ok(),
                "deflate_level 0 must be ignored for compression {compression:?}"
            );
        }
        // Under deflate it is load-bearing, and still checked.
        let mut init = test_init();
        init.compression = "deflate".to_string();
        init.deflate_level = 0;
        assert!(validate_init(&init).is_err());
        init.deflate_level = 9;
        assert!(validate_init(&init).is_ok());
    }

    #[test]
    fn a_native_band_format_must_match_the_output_bit_depth() {
        // u16 bands into a u8 file has no encode path: `write_tiff_native`
        // writes samples verbatim and `write_tiff` only converts from f32.
        let mut init = test_init();
        init.band_format = "u16".to_string();
        init.bit_depth = "u8".to_string();
        let err = validate_init(&init).unwrap_err();
        assert!(
            err.contains("band_format"),
            "error should name band_format: {err}"
        );

        // The same pair with matching formats is fine...
        init.bit_depth = "u16".to_string();
        assert_eq!(validate_init(&init).unwrap().band_format, BitDepth::U16);

        // ...and f32 bands may still target any output format, which is
        // exactly what a v2 host (no band_format at all) does.
        let mut init = test_init();
        init.bit_depth = "u8".to_string();
        let setup = validate_init(&init).unwrap();
        assert_eq!(setup.band_format, BitDepth::F32);
        assert_eq!(setup.options.bit_depth, BitDepth::U8);
    }

    #[test]
    fn slot_bytes_alignment_follows_the_band_sample_width() {
        // Every case clears one row of the band's own width (width 64, so
        // 512 bytes covers even f64), leaving alignment as the only thing
        // under test.
        for (band_format, slot_bytes, ok) in [
            ("u8", 513u64, true), // 1-byte alignment accepts anything
            ("u16", 513, false),  // odd
            ("u16", 514, true),
            ("f32", 514, false), // not a multiple of 4
            ("f32", 516, true),
            ("f64", 516, false), // multiple of 4, not of 8
            ("f64", 520, true),
        ] {
            let mut init = test_init();
            init.width = 64;
            init.band_format = band_format.to_string();
            init.bit_depth = band_format.to_string();
            init.slot_bytes = slot_bytes;
            assert_eq!(
                validate_init(&init).is_ok(),
                ok,
                "{band_format} slot_bytes {slot_bytes}"
            );
        }
    }

    #[test]
    fn slot_bytes_must_hold_at_least_one_row_at_the_band_width() {
        // 64 f64 samples need 512 bytes; 256 is one row's worth of f32 but
        // only half a row of f64.
        let mut init = test_init();
        init.band_format = "f64".to_string();
        init.bit_depth = "f64".to_string();
        init.slot_bytes = 256;
        let err = validate_init(&init).unwrap_err();
        assert!(err.contains("slot_bytes"), "{err}");
        init.slot_bytes = 512;
        assert!(validate_init(&init).is_ok());
    }

    #[test]
    fn zero_geometry_and_zero_slots_are_rejected() {
        for mutate in [
            (|i: &mut InitMessage| i.width = 0) as fn(&mut InitMessage),
            |i: &mut InitMessage| i.height = 0,
            |i: &mut InitMessage| i.slots = 0,
            |i: &mut InitMessage| i.slot_bytes = 0,
        ] {
            let mut init = test_init();
            mutate(&mut init);
            assert!(validate_init(&init).is_err());
        }
    }

    #[test]
    fn text_and_resolution_options_reach_the_writer_options() {
        let mut init = test_init();
        init.description = "M31".to_string();
        init.copyright = "(c) 2026".to_string();
        init.software = "mpp".to_string();
        init.x_resolution = 300.0;
        init.y_resolution = 150.0;
        init.resolution_unit = "cm".to_string();
        init.icc_profile = "AAEC".to_string();
        let setup = validate_init(&init).unwrap();
        assert_eq!(setup.options.description.as_deref(), Some("M31"));
        assert_eq!(setup.options.copyright.as_deref(), Some("(c) 2026"));
        assert_eq!(setup.options.software.as_deref(), Some("mpp"));
        assert_eq!(setup.options.icc_profile.as_deref(), Some(&[0u8, 1, 2][..]));
        let res = setup.options.resolution.expect("resolution is set");
        assert_eq!((res.x, res.y), (300.0, 150.0));
        assert_eq!(res.unit, ResolutionUnit::Centimeter);

        // A zero on either axis means "unknown" and writes no tags.
        init.y_resolution = 0.0;
        assert!(validate_init(&init).unwrap().options.resolution.is_none());
    }

    #[test]
    fn unknown_resolution_unit_is_refused_rather_than_defaulting_to_inch() {
        // The whole point: "centimeter" is a plausible spelling of "cm", and
        // silently writing inch-denominated tags for it is exactly the kind
        // of quiet substitution this phase exists to remove. Refused whether
        // or not a resolution is actually set, matching the other four v3
        // string enums.
        for (x, y) in [(300.0, 300.0), (0.0, 0.0)] {
            let mut init = test_init();
            init.x_resolution = x;
            init.y_resolution = y;
            init.resolution_unit = "centimeter".to_string();
            let err = validate_init(&init)
                .expect_err("an unknown resolution unit must be refused by name");
            assert!(err.contains("resolution unit"), "{err}");
            assert!(err.contains("centimeter"), "{err}");
        }

        // Both spellings the protocol does define still map through.
        let mut init = test_init();
        init.x_resolution = 300.0;
        init.y_resolution = 300.0;
        for (name, want) in [
            ("inch", ResolutionUnit::Inch),
            ("cm", ResolutionUnit::Centimeter),
        ] {
            init.resolution_unit = name.to_string();
            let res = validate_init(&init).unwrap().options.resolution.unwrap();
            assert_eq!(res.unit, want, "{name}");
        }
    }

    #[test]
    fn decode_icc_handles_absent_valid_and_oversized_profiles() {
        assert_eq!(decode_icc("").unwrap(), None);
        assert_eq!(decode_icc("AAEC").unwrap(), Some(vec![0u8, 1, 2]));
        assert!(decode_icc("*not base64*").unwrap_err().contains("base64"));

        // 16 MiB + 3 bytes decoded: one byte over the cap, and a size a
        // real profile never reaches.
        let oversized =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_ICC_BYTES + 3]);
        let err = decode_icc(&oversized).unwrap_err();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn band_byte_len_scales_with_the_sample_width() {
        assert_eq!(band_byte_len(4, 100, 1), Some(400));
        assert_eq!(band_byte_len(4, 100, 4), Some(1600));
        assert_eq!(band_byte_len(4, 100, 8), Some(3200));
        assert_eq!(band_byte_len(u32::MAX, u64::MAX, 8), None);
    }

    #[test]
    fn non_empty_maps_the_wires_empty_string_to_none() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("x"), Some("x".to_string()));
    }
}
