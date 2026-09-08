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
    }
}

/// Drives one read-mode run to completion over `link`: open
/// `init.output_path` via [`TiffReader`], report [`ImageMeta`], attach the
/// host's shm segment, stream every band [`TiffReader::read_bands`]
/// produces through a bounded pool of `init.slots` shm slots (acked by the
/// host via `ReadBandAck`), and report `Done`.
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
            if bytes.len() as u64 > init.slot_bytes {
                worker_err = Some(fail(
                    link,
                    &format!(
                        "band channel {channel} start_row {start_row} rows {rows} needs {} bytes, \
                     exceeds slot_bytes {}",
                        bytes.len(),
                        init.slot_bytes
                    ),
                ));
                return Err(mpp_core::Error::format(path, "aborted: oversize band"));
            }

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
                                "unexpected event from host while waiting for a free read slot: \
                             {other:?}"
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
            shm.as_mut_slice()[offset..offset + bytes.len()].copy_from_slice(bytes);

            let hdr = BandHeader {
                channel,
                start_row,
                rows,
                slot,
            };
            if let Err(e) = link.send_read_band(&hdr) {
                worker_err = Some(fail(link, &e.to_string()));
                return Err(mpp_core::Error::format(path, "aborted: link error"));
            }
            Ok(())
        };

    if let Err(e) = reader.read_bands(&mut sink) {
        return Err(worker_err.unwrap_or_else(|| fail(link, &e.to_string())));
    }

    if let Err(e) = link.send_done(0) {
        return Err(fail(link, &e.to_string()));
    }
    Ok(())
}
