//! Worker-side state machine for the host control channel.
//!
//! [`HostLink`] wraps a generic `Read`/`Write` pair (the worker's real
//! stdin/stdout in `main()`, an in-memory `Cursor`/`Vec<u8>` in tests) and
//! exposes the handshake ([`HostLink::read_init`]), the band/finish/cancel
//! event stream ([`HostLink::next_event`]), and the four outbound frames
//! (`send_band_ack`, `send_progress`, `send_done`, `send_error`) described
//! in `docs/pi-protocol.md` §5–§7. It performs no shm access, encoding, or
//! coverage tracking itself — those live one level up in the worker's main
//! loop, which drives `HostLink` and reacts to each [`HostEvent`].

use std::io::{self, Read, Write};

use crate::protocol::{
    BandHeader, DoneMessage, ErrorMessage, IPC_PROTOCOL_VERSION, InitMessage, ProgressMessage,
    read_frame, tag, write_frame,
};

/// Errors from the worker-side host link.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// A read or write on the underlying transport failed. Note that a
    /// *clean* EOF on `input` (the host closing its end deliberately) is
    /// not represented here — [`HostLink::next_event`] maps it to
    /// [`HostEvent::HostGone`] instead, since a host going away is a
    /// lifecycle event the worker handles by cleanup, not an I/O failure.
    #[error("host link I/O error: {0}")]
    Io(#[from] io::Error),
    /// A frame violated the protocol: an unexpected first frame, a
    /// version mismatch, an unknown/out-of-place tag, or a malformed
    /// payload. The message names the specific field or condition at
    /// fault.
    #[error("host link protocol error: {0}")]
    Protocol(String),
}

/// One event the worker receives from the host after the `Init` handshake.
///
/// Returned by [`HostLink::next_event`]; the worker's main loop matches on
/// this to decide what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// A `Band` frame: one shm slot the host has filled with pixel data,
    /// ready for the worker to copy into its staging file and ack.
    Band(BandHeader),
    /// The host has sent every band for this run; once coverage is
    /// complete the worker should encode and reply with `Done`.
    Finish,
    /// The host asked the worker to abort. The worker should clean up
    /// (delete staging/partial output) and reply with `Error {
    /// message: "cancelled" }`.
    Cancel,
    /// The host's stdin reached clean EOF before a `Finish` frame arrived
    /// — the host process is gone (crashed, or closed the pipe early).
    /// Per `docs/pi-protocol.md` §7.5 this is not an error the worker
    /// reports back (there is nothing to report it *to*): the worker
    /// treats it like [`HostEvent::Cancel`]'s cleanup path and exits
    /// nonzero.
    HostGone,
}

/// Worker's view of the host connection: reads control frames from
/// `input`, writes them to `output`. Generic over `Read`/`Write` so tests
/// can drive it over in-memory pipes (`Cursor<Vec<u8>>` / `Vec<u8>`)
/// without a real subprocess; `main()` passes the worker's locked
/// stdin/stdout.
///
/// Every `send_*` method flushes `output` after writing, since the host
/// blocks waiting for acks/progress/terminal frames on the other end of
/// the pipe.
pub struct HostLink<R: Read, W: Write> {
    input: R,
    output: W,
}

impl<R: Read, W: Write> HostLink<R, W> {
    /// Wraps `input`/`output` as a fresh link. Does not read or write
    /// anything; call [`Self::read_init`] first.
    pub fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    /// Reads and validates the handshake: the very first frame on `input`
    /// must be `Init` (tag [`tag::INIT`]) with `protocol_version ==
    /// `[`IPC_PROTOCOL_VERSION`]` and `worker_version == expected_worker_version`
    /// exactly. Any other first tag, or a version mismatch on either
    /// field, is a [`LinkError::Protocol`] naming the offending tag or
    /// field — the caller (the worker's `main`) is expected to report it
    /// via `send_error` and exit nonzero, per `docs/pi-protocol.md` §7.1.
    pub fn read_init(&mut self, expected_worker_version: &str) -> Result<InitMessage, LinkError> {
        let (t, payload) = read_frame(&mut self.input)?;
        if t != tag::INIT {
            return Err(LinkError::Protocol(format!(
                "expected Init (tag {}) as the first frame, got tag {t}",
                tag::INIT
            )));
        }
        let init: InitMessage = serde_json::from_slice(&payload)
            .map_err(|e| LinkError::Protocol(format!("malformed Init payload: {e}")))?;
        if init.protocol_version != IPC_PROTOCOL_VERSION {
            return Err(LinkError::Protocol(format!(
                "protocol_version mismatch: worker is {IPC_PROTOCOL_VERSION}, host sent {}",
                init.protocol_version
            )));
        }
        if init.worker_version != expected_worker_version {
            return Err(LinkError::Protocol(format!(
                "worker_version mismatch: worker is {expected_worker_version:?}, host sent {:?}",
                init.worker_version
            )));
        }
        Ok(init)
    }

    /// Reads the next post-`Init` frame and classifies it as a
    /// [`HostEvent`]. A clean EOF on `input` (the host closing its stdin
    /// deliberately, e.g. a crash) is reported as `Ok(HostEvent::HostGone)`
    /// rather than an error — see [`HostEvent::HostGone`]'s docs. Any
    /// other I/O failure (a truncated frame, for instance) stays
    /// `Err(LinkError::Io)`. A frame with a tag other than `Band`,
    /// `Finish`, or `Cancel` — or a `Band` frame with a malformed
    /// payload — is `Err(LinkError::Protocol)`.
    pub fn next_event(&mut self) -> Result<HostEvent, LinkError> {
        let (t, payload) = match read_frame(&mut self.input) {
            Ok(frame) => frame,
            Err(e) if is_clean_pipe_close(&e) => return Ok(HostEvent::HostGone),
            Err(e) => return Err(LinkError::Io(e)),
        };
        match t {
            tag::BAND => {
                let header = BandHeader::from_bytes(&payload).ok_or_else(|| {
                    LinkError::Protocol(format!(
                        "malformed Band payload: expected {} bytes, got {}",
                        BandHeader::WIRE_LEN,
                        payload.len()
                    ))
                })?;
                Ok(HostEvent::Band(header))
            }
            tag::FINISH => Ok(HostEvent::Finish),
            tag::CANCEL => Ok(HostEvent::Cancel),
            other => Err(LinkError::Protocol(format!(
                "unexpected tag {other} from host (expected Band, Finish, or Cancel)"
            ))),
        }
    }

    /// Sends `BandAck { slot }` (tag [`tag::BAND_ACK`]): tells the host
    /// `slot` is free for reuse.
    pub fn send_band_ack(&mut self, slot: u32) -> Result<(), LinkError> {
        self.send(tag::BAND_ACK, &slot.to_le_bytes())
    }

    /// Sends a `Progress` frame (tag [`tag::PROGRESS`]) reporting rows
    /// completed so far out of the total expected.
    pub fn send_progress(&mut self, rows_done: u64, rows_total: u64) -> Result<(), LinkError> {
        let payload = encode_json(&ProgressMessage {
            rows_done,
            rows_total,
        })?;
        self.send(tag::PROGRESS, &payload)
    }

    /// Sends `Done { bytes_written }` (tag [`tag::DONE`]): the run
    /// completed successfully. Per `docs/pi-protocol.md` §4 this must be
    /// the last frame the worker sends.
    pub fn send_done(&mut self, bytes_written: u64) -> Result<(), LinkError> {
        let payload = encode_json(&DoneMessage { bytes_written })?;
        self.send(tag::DONE, &payload)
    }

    /// Sends `Error { message }` (tag [`tag::ERROR`]): the run failed.
    /// Per `docs/pi-protocol.md` §4 this must be the last frame the
    /// worker sends.
    pub fn send_error(&mut self, message: &str) -> Result<(), LinkError> {
        let payload = encode_json(&ErrorMessage {
            message: message.to_string(),
        })?;
        self.send(tag::ERROR, &payload)
    }

    /// Writes one frame to `output` and flushes it so the host observes
    /// it promptly (the host blocks waiting for acks).
    fn send(&mut self, t: u8, payload: &[u8]) -> Result<(), LinkError> {
        write_frame(&mut self.output, t, payload)?;
        self.output.flush()?;
        Ok(())
    }
}

/// Encodes `msg` to JSON, mapping a (practically unreachable, for these
/// plain-data message types) serialization failure to
/// [`LinkError::Protocol`] rather than adding a third `LinkError` variant
/// just for it.
fn encode_json(msg: &impl serde::Serialize) -> Result<Vec<u8>, LinkError> {
    serde_json::to_vec(msg)
        .map_err(|e| LinkError::Protocol(format!("failed to encode outgoing payload: {e}")))
}

/// True for exactly the clean-EOF error [`read_frame`] produces when the
/// peer closes its output before any byte of a new frame — see
/// `read_frame`'s docs. A truncated frame (EOF mid-length-prefix or
/// mid-payload) is also `UnexpectedEof` but carries `read_exact`'s own
/// message, not this one, so it is never misclassified as a clean close.
fn is_clean_pipe_close(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof && e.to_string() == "pipe closed"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::SessionMode;
    use std::io::Cursor;

    const WORKER_VERSION: &str = "0.1.0";

    fn sample_init() -> InitMessage {
        InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: WORKER_VERSION.to_string(),
            output_path: "/tmp/out.tif".to_string(),
            width: 100,
            height: 200,
            channels: 3,
            bit_depth: "f32".to_string(),
            shm_name: "/mpp-ipc-test".to_string(),
            slot_bytes: 4096,
            slots: 2,
            mode: SessionMode::Write,
        }
    }

    fn init_frame(init: &InitMessage) -> Vec<u8> {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::INIT, &serde_json::to_vec(init).unwrap()).unwrap();
        buf
    }

    fn link_over(bytes: Vec<u8>) -> HostLink<Cursor<Vec<u8>>, Vec<u8>> {
        HostLink::new(Cursor::new(bytes), Vec::new())
    }

    #[test]
    fn read_init_happy_handshake() {
        let init = sample_init();
        let mut link = link_over(init_frame(&init));
        let got = link.read_init(WORKER_VERSION).unwrap();
        assert_eq!(got, init);
    }

    #[test]
    fn read_init_rejects_wrong_first_tag() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::FINISH, &[]).unwrap();
        let mut link = link_over(buf);
        let err = link.read_init(WORKER_VERSION).unwrap_err();
        match err {
            LinkError::Protocol(msg) => {
                assert!(msg.contains("Init"), "error should name Init: {msg}");
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn read_init_rejects_protocol_version_mismatch() {
        let mut init = sample_init();
        init.protocol_version = IPC_PROTOCOL_VERSION + 1;
        let mut link = link_over(init_frame(&init));
        let err = link.read_init(WORKER_VERSION).unwrap_err();
        match err {
            LinkError::Protocol(msg) => {
                assert!(
                    msg.contains("protocol_version"),
                    "error should name protocol_version: {msg}"
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn read_init_rejects_worker_version_mismatch() {
        let mut init = sample_init();
        init.worker_version = "9.9.9".to_string();
        let mut link = link_over(init_frame(&init));
        let err = link.read_init(WORKER_VERSION).unwrap_err();
        match err {
            LinkError::Protocol(msg) => {
                assert!(
                    msg.contains("worker_version"),
                    "error should name worker_version: {msg}"
                );
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn read_init_rejects_malformed_json_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::INIT, b"not json").unwrap();
        let mut link = link_over(buf);
        assert!(matches!(
            link.read_init(WORKER_VERSION),
            Err(LinkError::Protocol(_))
        ));
    }

    #[test]
    fn next_event_reads_band_finish_cancel_in_sequence() {
        let header = BandHeader {
            channel: 1,
            start_row: 100,
            rows: 50,
            slot: 2,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::BAND, &header.to_bytes()).unwrap();
        write_frame(&mut buf, tag::FINISH, &[]).unwrap();
        write_frame(&mut buf, tag::CANCEL, &[]).unwrap();
        let mut link = link_over(buf);

        assert_eq!(link.next_event().unwrap(), HostEvent::Band(header));
        assert_eq!(link.next_event().unwrap(), HostEvent::Finish);
        assert_eq!(link.next_event().unwrap(), HostEvent::Cancel);
    }

    #[test]
    fn next_event_maps_clean_eof_to_host_gone() {
        let mut link = link_over(Vec::new());
        assert_eq!(link.next_event().unwrap(), HostEvent::HostGone);
    }

    #[test]
    fn next_event_maps_clean_eof_after_some_frames_to_host_gone() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::FINISH, &[]).unwrap();
        let mut link = link_over(buf);
        assert_eq!(link.next_event().unwrap(), HostEvent::Finish);
        // Pipe now cleanly exhausted: the next read is HostGone, not an
        // error, even though a frame was already successfully read.
        assert_eq!(link.next_event().unwrap(), HostEvent::HostGone);
    }

    #[test]
    fn next_event_reports_truncated_frame_as_io_error_not_host_gone() {
        // A length prefix promising 10 payload bytes, but only 2 delivered
        // before the stream ends: this is a truncated frame (a real I/O
        // failure), never a clean pipe close.
        let buf = vec![tag::BAND, 10, 0, 0, 0, 0xAA, 0xBB];
        let mut link = link_over(buf);
        let err = link.next_event().unwrap_err();
        assert!(
            matches!(err, LinkError::Io(_)),
            "expected Io error, got {err:?}"
        );
    }

    #[test]
    fn next_event_rejects_unexpected_tag() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::INIT, b"{}").unwrap();
        let mut link = link_over(buf);
        assert!(matches!(link.next_event(), Err(LinkError::Protocol(_))));
    }

    #[test]
    fn next_event_rejects_malformed_band_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::BAND, &[1, 2, 3]).unwrap();
        let mut link = link_over(buf);
        assert!(matches!(link.next_event(), Err(LinkError::Protocol(_))));
    }

    #[test]
    fn send_band_ack_writes_tagged_frame() {
        let mut link = HostLink::new(Cursor::new(Vec::new()), Vec::new());
        link.send_band_ack(7).unwrap();
        let mut out = Cursor::new(link.output.clone());
        let (t, payload) = read_frame(&mut out).unwrap();
        assert_eq!(t, tag::BAND_ACK);
        assert_eq!(payload, 7u32.to_le_bytes().to_vec());
    }

    #[test]
    fn send_progress_writes_tagged_json_frame() {
        let mut link = HostLink::new(Cursor::new(Vec::new()), Vec::new());
        link.send_progress(10, 100).unwrap();
        let mut out = Cursor::new(link.output.clone());
        let (t, payload) = read_frame(&mut out).unwrap();
        assert_eq!(t, tag::PROGRESS);
        let msg: ProgressMessage = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            msg,
            ProgressMessage {
                rows_done: 10,
                rows_total: 100
            }
        );
    }

    #[test]
    fn send_done_writes_tagged_json_frame() {
        let mut link = HostLink::new(Cursor::new(Vec::new()), Vec::new());
        link.send_done(12345).unwrap();
        let mut out = Cursor::new(link.output.clone());
        let (t, payload) = read_frame(&mut out).unwrap();
        assert_eq!(t, tag::DONE);
        let msg: DoneMessage = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            msg,
            DoneMessage {
                bytes_written: 12345
            }
        );
    }

    #[test]
    fn send_error_writes_tagged_json_frame() {
        let mut link = HostLink::new(Cursor::new(Vec::new()), Vec::new());
        link.send_error("cancelled").unwrap();
        let mut out = Cursor::new(link.output.clone());
        let (t, payload) = read_frame(&mut out).unwrap();
        assert_eq!(t, tag::ERROR);
        let msg: ErrorMessage = serde_json::from_slice(&payload).unwrap();
        assert_eq!(
            msg,
            ErrorMessage {
                message: "cancelled".to_string()
            }
        );
    }

    #[test]
    fn sends_are_flushed_immediately() {
        // `Vec<u8>::flush` is a no-op that always succeeds, so this mainly
        // guards against a future output type (e.g. a real BufWriter) not
        // being flushed: the frame must already be visible in `output`
        // right after the call returns, with no separate flush needed by
        // the caller.
        let mut link = HostLink::new(Cursor::new(Vec::new()), Vec::new());
        link.send_band_ack(1).unwrap();
        assert!(!link.output.is_empty());
    }
}
