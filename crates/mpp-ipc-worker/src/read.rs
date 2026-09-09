//! Worker read-mode run loop: decode an existing TIFF via
//! [`TiffReader`] and stream its native per-channel row bands to the host
//! over shared memory.
//!
//! Inverts the write path's slot-accounting discipline (`crate::run_write`'s
//! `Band`/`BandAck` handling, in `main.rs`): here the *worker* fills shm
//! slots (from [`TiffReader::read_bands`]) and sends `READ_BAND` requests,
//! and the *host* frees them by replying `ReadBandAck`. A small free-slot
//! pool (initially every slot in `0..init.slots`) means the worker only
//! blocks on [`HostLink::next_event`] once every slot is in-flight — so
//! peak worker memory for pixel data stays bounded to `init.slots` bands'
//! worth of shm, never the whole image.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;

use base64::Engine as _;
use mpp_core::formats::tiff_read::{ColorClass, TiffInfo, TiffReader};
use mpp_ipc::link::{HostEvent, HostLink};
use mpp_ipc::protocol::{BandHeader, ImageMeta, InitMessage};
use mpp_ipc::shm::ShmSegment;

use crate::{WorkerError, fail, shm_total_bytes};

/// Builds the [`ImageMeta`] the worker reports for a read session from the
/// geometry [`TiffReader::open`] read off the file's header.
///
/// Shared by [`run_read`] (sent over the control channel as the
/// [`tag::IMAGE_META`](mpp_ipc::protocol::tag::IMAGE_META) frame) and the
/// `--probe` CLI entry in `main.rs` (printed standalone as JSON), so both
/// paths report exactly the same geometry for the same file.
pub fn image_meta(info: &TiffInfo) -> ImageMeta {
    ImageMeta {
        width: info.width,
        height: info.height,
        channels: u64::from(info.total_channels()),
        alpha: u64::from(info.alpha_channels),
        color_space: match info.color {
            ColorClass::Gray => "gray",
            ColorClass::Rgb => "rgb",
        }
        .to_string(),
        bit_depth: info.sample_format.wire_name().to_string(),
        icc_profile: info
            .icc_profile
            .as_deref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .unwrap_or_default(),
    }
}

/// Drives one read-mode run to completion over `link`: open
/// `init.output_path` via [`TiffReader`], report [`ImageMeta`], attach the
/// host's shm segment, stream every band [`TiffReader::read_bands`]
/// produces — split into slot-sized sub-bands, since one file chunk (a whole
/// strip or tile-row) routinely exceeds `init.slot_bytes` — through a bounded
/// pool of `init.slots` shm slots (acked by the host via `ReadBandAck`),
/// drain the last in-flight acks, and report `Done`.
///
/// Every fallible step sends an `Error` frame before returning, mirroring
/// `crate::run_write`'s "report, then fail" discipline (see `crate::fail`).
/// `bytes_written` in the final `Done` frame is always `0` — meaningless on
/// a read session, since the worker writes nothing to disk.
pub fn run_read<R: Read, W: Write>(
    link: &mut HostLink<R, W>,
    init: &InitMessage,
) -> Result<(), WorkerError> {
    let path = Path::new(&init.output_path);
    let mut reader = match TiffReader::open(path) {
        Ok(r) => r,
        Err(e) => return Err(fail(link, &format!("opening {}: {e}", path.display()))),
    };

    let meta = image_meta(reader.info());
    if let Err(e) = link.send_image_meta(&meta) {
        return Err(fail(link, &e.to_string()));
    }

    let total_bytes = match shm_total_bytes(init) {
        Ok(n) => n,
        Err(e) => return Err(fail(link, &e)),
    };
    let mut shm = match ShmSegment::open(&init.shm_name, total_bytes) {
        Ok(shm) => shm,
        Err(e) => {
            return Err(fail(
                link,
                &format!("opening shm segment {:?}: {e}", init.shm_name),
            ));
        }
    };

    // A band is always whole rows, so the smallest unit the worker can place
    // in a slot is one full row of one channel: `width * sample_bytes`. Both
    // come straight off the reader's header (authoritative, the same source
    // `ImageMeta` reports from).
    let sample_bytes = reader.info().sample_format.bytes() as u64;
    let width = reader.info().width;
    let row_bytes = match width.checked_mul(sample_bytes) {
        Some(n) => n,
        None => {
            return Err(fail(
                link,
                &format!("width {width} * sample_bytes {sample_bytes} overflows u64"),
            ));
        }
    };
    // `read_bands` yields a whole file chunk (a full strip, or a full
    // de-tiled tile-row) as one per-channel band, whose byte length
    // (`rows * row_bytes`) routinely exceeds `init.slot_bytes` for real
    // files — a single-strip astro TIFF has `RowsPerStrip == height`, so its
    // per-channel band is the *entire* channel; a tiled Photoshop BigTIFF's
    // de-tiled tile-row band is `tile_h * row_bytes`. The host, meanwhile,
    // sizes each slot to a fixed ~4 MiB independent of the file's chunk
    // geometry. So the worker splits each incoming band into sub-bands of at
    // most `max_rows` whole rows, each sent as its own `READ_BAND` (its own
    // slot, its own ack). Peak in-flight pixel memory stays bounded to
    // `slots` sub-bands regardless of how large the file's chunks are.
    //
    // `slot_bytes` smaller than a single row is a genuine host misconfig (a
    // band is whole rows — it can never be split below one), so error clearly
    // rather than emit a partial row.
    if row_bytes > init.slot_bytes {
        return Err(fail(
            link,
            &format!(
                "slot_bytes {} cannot hold even one row of width {width} ({row_bytes} bytes); \
                 a band is always whole rows",
                init.slot_bytes
            ),
        ));
    }
    // `row_bytes >= 1` (both factors are >= 1: `SampleFormat::bytes()` is
    // never 0, and `TiffReader::open` rejects a zero width) and `row_bytes <=
    // slot_bytes` (just checked), so `max_rows >= 1`.
    let max_rows = init.slot_bytes / row_bytes;

    // Every slot starts free; a `HostEvent::ReadBandAck` (handled inside
    // `sink` below) returns a slot to this pool once the host has consumed
    // it.
    let mut free_slots: VecDeque<u32> = (0..init.slots).collect();
    // Set from inside `sink` on any host-link/shm failure. The closure must
    // return an `mpp_core::Error` to unwind out of `read_bands` (that's the
    // sink signature's error type), not the `WorkerError` this function
    // actually needs to propagate, so `run_read` prefers this stashed value
    // once `read_bands` returns `Err`. The fallback below — wrapping
    // `read_bands`'s own error directly — is the live path for a genuine
    // decode failure (a corrupt chunk, an I/O error reading the file, …)
    // that `read_bands` hits on its own, never through `sink` at all, so
    // `worker_err` stays `None` and the decode error's own message is what
    // gets reported.
    let mut worker_err: Option<WorkerError> = None;

    let mut sink =
        |channel: u32, start_row: u64, rows: u32, bytes: &[u8]| -> mpp_core::Result<()> {
            // Split this whole-chunk band into slot-sized sub-bands of at
            // most `max_rows` whole rows each. Each sub-band gets its own
            // free slot, its own `READ_BAND` frame, and its own ack, so the
            // worker never holds more than `slots` sub-bands in flight even
            // when a single file chunk dwarfs one slot.
            let mut row_off: u32 = 0;
            while row_off < rows {
                let sub_rows = u64::from(rows - row_off).min(max_rows) as u32;
                let byte_start = row_off as usize * row_bytes as usize;
                let byte_end = byte_start + sub_rows as usize * row_bytes as usize;
                let sub_bytes = &bytes[byte_start..byte_end];
                let sub_start_row = start_row + u64::from(row_off);

                while free_slots.is_empty() {
                    match link.next_event() {
                        Ok(HostEvent::ReadBandAck { slot }) => free_slots.push_back(slot),
                        Ok(HostEvent::HostGone) => {
                            worker_err = Some(WorkerError::HostGone);
                            return Err(mpp_core::Error::format(path, "aborted: host gone"));
                        }
                        Ok(other) => {
                            worker_err = Some(fail(
                                link,
                                &format!(
                                    "unexpected event from host while waiting for a free read \
                                     slot: {other:?}"
                                ),
                            ));
                            return Err(mpp_core::Error::format(path, "aborted: protocol error"));
                        }
                        Err(e) => {
                            worker_err = Some(fail(link, &e.to_string()));
                            return Err(mpp_core::Error::format(path, "aborted: link error"));
                        }
                    }
                }
                // The loop above only exits once `free_slots` is non-empty.
                let slot = free_slots.pop_front().expect("free_slots is non-empty");

                let offset = init.slot_bytes as usize * slot as usize;
                shm.as_mut_slice()[offset..offset + sub_bytes.len()].copy_from_slice(sub_bytes);

                let hdr = BandHeader {
                    channel,
                    start_row: sub_start_row,
                    rows: sub_rows,
                    slot,
                };
                if let Err(e) = link.send_read_band(&hdr) {
                    worker_err = Some(fail(link, &e.to_string()));
                    return Err(mpp_core::Error::format(path, "aborted: link error"));
                }

                row_off += sub_rows;
            }
            Ok(())
        };

    if let Err(e) = reader.read_bands(&mut sink) {
        return Err(worker_err.unwrap_or_else(|| fail(link, &e.to_string())));
    }

    // Drain the acks for every still-in-flight sub-band before `Done`. The
    // host writes each `ReadBandAck` only *after* it has read that band's
    // slot, so the final up-to-`slots` acks can still be in flight when
    // `read_bands` returns. Exiting now (closing our stdin) would race those
    // writes and turn a fully-successful decode into a host-side link error,
    // so wait until every slot is back in the free pool. `HostGone` here is
    // handled exactly as in the send loop above.
    while (free_slots.len() as u32) < init.slots {
        match link.next_event() {
            Ok(HostEvent::ReadBandAck { slot }) => free_slots.push_back(slot),
            Ok(HostEvent::HostGone) => return Err(WorkerError::HostGone),
            Ok(other) => {
                return Err(fail(
                    link,
                    &format!("unexpected event from host while draining read acks: {other:?}"),
                ));
            }
            Err(e) => return Err(fail(link, &e.to_string())),
        }
    }

    if let Err(e) = link.send_done(0) {
        return Err(fail(link, &e.to_string()));
    }
    Ok(())
}
