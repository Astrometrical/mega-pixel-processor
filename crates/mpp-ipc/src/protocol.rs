//! Wire protocol v1 between the PixInsight host and `mpp-ipc-worker`.
//!
//! One control-channel frame is `[u8 tag][u32 LE payload_len][payload
//! bytes]` ([`write_frame`] / [`read_frame`]). `Band` frames (the hot path)
//! carry a fixed 20-byte binary payload ([`BandHeader`]); every other
//! message is a JSON document via `serde_json`. This is the Rust source of
//! truth for `docs/pi-protocol.md` — keep both in sync, in the same commit,
//! on any wire-format change.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Wire-protocol version. Bumped on any wire-format change (new tag,
/// reordered/resized binary field, renamed/retyped JSON field, changed
/// lifecycle rule) — see `docs/pi-protocol.md` §8.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// Frame tag constants. Host→worker tags start at 128; worker→host tags are
/// small integers 1–4 (a debugging convenience for spotting a mixed-up
/// direction — nothing in the codec relies on the ranges not overlapping).
pub mod tag {
    /// Host→worker: `InitMessage`, JSON payload. Must be the first frame.
    pub const INIT: u8 = 128;
    /// Host→worker: `BandHeader`, 20-byte binary payload.
    pub const BAND: u8 = 129;
    /// Host→worker: empty payload; all bands have been sent.
    pub const FINISH: u8 = 130;
    /// Host→worker: empty payload; abort the run.
    pub const CANCEL: u8 = 131;
    /// Worker→host: 4-byte `slot: u32` payload; the slot is free for reuse.
    pub const BAND_ACK: u8 = 1;
    /// Worker→host: `ProgressMessage`, JSON payload.
    pub const PROGRESS: u8 = 2;
    /// Worker→host: `DoneMessage`, JSON payload; the last frame on success.
    pub const DONE: u8 = 3;
    /// Worker→host: `ErrorMessage`, JSON payload; the last frame on failure.
    pub const ERROR: u8 = 4;
}

/// Host→worker `Init` message (tag [`tag::INIT`]), sent bare (not wrapped in
/// an externally-tagged enum) as the very first frame of a run.
///
/// Field names are exactly as written — serde's default snake_case-as-written
/// names, no renaming — per `docs/pi-protocol.md` §5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitMessage {
    /// Must equal [`IPC_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Must equal the worker binary's exact `CARGO_PKG_VERSION`.
    pub worker_version: String,
    /// Filesystem path the worker will write the encoded output file to.
    pub output_path: String,
    /// Image width in pixels.
    pub width: u64,
    /// Image height in pixels.
    pub height: u64,
    /// Channel count: `1` (mono) or `3` (RGB).
    pub channels: u64,
    /// Output sample format: `"f32"`, `"u16"`, or `"u8"`.
    pub bit_depth: String,
    /// Name of the shm segment the host already created.
    pub shm_name: String,
    /// Size in bytes of one shm slot; must be `>=` the largest band the host
    /// will ever send.
    pub slot_bytes: u64,
    /// Number of slots in the shm segment, `>= 1`.
    pub slots: u32,
}

/// Worker→host `Progress` message (tag [`tag::PROGRESS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressMessage {
    /// Rows completed so far, summed across channels.
    pub rows_done: u64,
    /// Total rows expected, summed across channels.
    pub rows_total: u64,
}

/// Worker→host `Done` message (tag [`tag::DONE`]); the last frame the
/// worker sends on a successful run, followed by exit code 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneMessage {
    /// Total bytes written to the output file.
    pub bytes_written: u64,
}

/// Worker→host `Error` message (tag [`tag::ERROR`]); the last frame the
/// worker sends on a failed run, followed by a nonzero exit code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    /// Human-readable error description.
    pub message: String,
}

/// Host→worker `Band` frame payload (tag [`tag::BAND`]): names one
/// contiguous run of rows of one channel, already written by the host into
/// shm `slot`.
///
/// Binary layout, little-endian, exactly this field order (not alphabetized
/// or regrouped), 20 bytes total — see `docs/pi-protocol.md` §4:
///
/// | offset | size | field       |
/// |-------:|-----:|-------------|
/// |      0 |    4 | `channel`   |
/// |      4 |    8 | `start_row` |
/// |     12 |    4 | `rows`      |
/// |     16 |    4 | `slot`      |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandHeader {
    /// Which channel this band belongs to, `0..channels`.
    pub channel: u32,
    /// First row of this band, inclusive, `0..height`.
    pub start_row: u64,
    /// Row count in this band.
    pub rows: u32,
    /// shm slot already filled by the host with this band's pixel data.
    pub slot: u32,
}

impl BandHeader {
    /// Encoded size in bytes of a `BandHeader`.
    pub const WIRE_LEN: usize = 20;

    /// Encodes to the 20-byte little-endian wire layout (see the struct
    /// docs for field order/offsets).
    pub fn to_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut buf = [0u8; Self::WIRE_LEN];
        buf[0..4].copy_from_slice(&self.channel.to_le_bytes());
        buf[4..12].copy_from_slice(&self.start_row.to_le_bytes());
        buf[12..16].copy_from_slice(&self.rows.to_le_bytes());
        buf[16..20].copy_from_slice(&self.slot.to_le_bytes());
        buf
    }

    /// Decodes from the 20-byte little-endian wire layout. Returns `None`
    /// if `b` is not exactly [`Self::WIRE_LEN`] bytes.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::WIRE_LEN {
            return None;
        }
        Some(Self {
            channel: u32::from_le_bytes(b[0..4].try_into().ok()?),
            start_row: u64::from_le_bytes(b[4..12].try_into().ok()?),
            rows: u32::from_le_bytes(b[12..16].try_into().ok()?),
            slot: u32::from_le_bytes(b[16..20].try_into().ok()?),
        })
    }
}

/// Writes one frame to `w`: `[u8 tag][u32 LE payload_len][payload]`.
///
/// `payload.len()` must fit in a `u32`; a larger payload fails with
/// `io::ErrorKind::InvalidData` rather than silently truncating the length
/// prefix.
pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame payload too large"))?;
    w.write_all(&[tag])?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    Ok(())
}

/// Reads one frame from `r`, returning `(tag, payload)`.
///
/// EOF semantics (`docs/pi-protocol.md` §2): a clean EOF before any byte of
/// a new frame means the peer closed its output deliberately, and is
/// reported as `io::ErrorKind::UnexpectedEof` with the message `"pipe
/// closed"` — callers treat this as "the host is gone" rather than a
/// generic I/O failure. An EOF encountered mid-length-prefix or
/// mid-payload is a truncated frame, also `io::ErrorKind::UnexpectedEof`
/// (via the underlying `read_exact` failure), but is never confused with
/// the clean case since it carries `read_exact`'s own message, not "pipe
/// closed".
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut tag_buf = [0u8; 1];
    let n = r.read(&mut tag_buf)?;
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
    }
    let tag = tag_buf[0];

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // `len` is trusted, not attacker-bounded: both ends of this pipe are the
    // host and its own spawned worker child (docs/pi-protocol.md §2).
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;

    Ok((tag, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_init() -> InitMessage {
        InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: "0.1.0".to_string(),
            output_path: "/tmp/out.tif".to_string(),
            width: 12000,
            height: 9000,
            channels: 3,
            bit_depth: "f32".to_string(),
            shm_name: "/mpp-ipc-3f2a91".to_string(),
            slot_bytes: 4_915_200,
            slots: 4,
        }
    }

    #[test]
    fn frame_roundtrip_all_tags() {
        let cases: &[(u8, &[u8])] = &[
            (tag::INIT, b"{\"a\":1}"),
            (tag::BAND, &[1, 2, 3, 4, 5]),
            (tag::FINISH, b""),
            (tag::CANCEL, b""),
            (tag::BAND_ACK, &[9, 0, 0, 0]),
            (tag::PROGRESS, b"{\"rows_done\":1,\"rows_total\":2}"),
            (tag::DONE, b"{\"bytes_written\":42}"),
            (tag::ERROR, b"{\"message\":\"boom\"}"),
        ];
        for (tag_val, payload) in cases {
            let mut buf = Vec::new();
            write_frame(&mut buf, *tag_val, payload).unwrap();
            let mut cur = Cursor::new(buf);
            let (got_tag, got_payload) = read_frame(&mut cur).unwrap();
            assert_eq!(got_tag, *tag_val);
            assert_eq!(got_payload, *payload);
            // A second read on the exhausted cursor is a clean EOF.
            let err = read_frame(&mut cur).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
            assert_eq!(err.to_string(), "pipe closed");
        }
    }

    #[test]
    fn frame_roundtrip_two_frames_back_to_back() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::BAND_ACK, &[1, 0, 0, 0]).unwrap();
        write_frame(
            &mut buf,
            tag::PROGRESS,
            b"{\"rows_done\":1,\"rows_total\":9}",
        )
        .unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cur).unwrap(),
            (tag::BAND_ACK, vec![1, 0, 0, 0])
        );
        assert_eq!(
            read_frame(&mut cur).unwrap(),
            (
                tag::PROGRESS,
                b"{\"rows_done\":1,\"rows_total\":9}".to_vec()
            )
        );
        assert!(read_frame(&mut cur).is_err()); // clean EOF
    }

    #[test]
    fn band_header_golden_bytes() {
        let header = BandHeader {
            channel: 1,
            start_row: 1000,
            rows: 256,
            slot: 3,
        };
        let expected: [u8; 20] = [
            1, 0, 0, 0, // channel = 1 (u32 LE)
            232, 3, 0, 0, 0, 0, 0, 0, // start_row = 1000 (u64 LE)
            0, 1, 0, 0, // rows = 256 (u32 LE)
            3, 0, 0, 0, // slot = 3 (u32 LE)
        ];
        assert_eq!(header.to_bytes(), expected);
        assert_eq!(BandHeader::from_bytes(&expected), Some(header));
    }

    #[test]
    fn band_header_round_trips_through_a_band_frame() {
        let header = BandHeader {
            channel: 2,
            start_row: 65536,
            rows: 400,
            slot: 7,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::BAND, &header.to_bytes()).unwrap();
        let mut cur = Cursor::new(buf);
        let (t, payload) = read_frame(&mut cur).unwrap();
        assert_eq!(t, tag::BAND);
        assert_eq!(BandHeader::from_bytes(&payload), Some(header));
    }

    #[test]
    fn band_header_from_bytes_rejects_wrong_length() {
        assert_eq!(BandHeader::from_bytes(&[0u8; 19]), None);
        assert_eq!(BandHeader::from_bytes(&[0u8; 21]), None);
        assert_eq!(BandHeader::from_bytes(&[]), None);
    }

    #[test]
    fn init_message_json_roundtrip() {
        let init = sample_init();
        let json = serde_json::to_vec(&init).unwrap();
        let back: InitMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, init);
    }

    #[test]
    fn init_message_golden_json_field_names() {
        let init = sample_init();
        let value = serde_json::to_value(&init).unwrap();
        let expected = serde_json::json!({
            "protocol_version": 1,
            "worker_version": "0.1.0",
            "output_path": "/tmp/out.tif",
            "width": 12000,
            "height": 9000,
            "channels": 3,
            "bit_depth": "f32",
            "shm_name": "/mpp-ipc-3f2a91",
            "slot_bytes": 4_915_200,
            "slots": 4
        });
        assert_eq!(value, expected);
    }

    #[test]
    fn init_message_round_trips_through_a_frame() {
        let init = sample_init();
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::INIT, &serde_json::to_vec(&init).unwrap()).unwrap();
        let mut cur = Cursor::new(buf);
        let (t, payload) = read_frame(&mut cur).unwrap();
        assert_eq!(t, tag::INIT);
        let back: InitMessage = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, init);
    }

    #[test]
    fn progress_done_error_json_roundtrip() {
        let progress = ProgressMessage {
            rows_done: 100,
            rows_total: 9000,
        };
        let back: ProgressMessage =
            serde_json::from_slice(&serde_json::to_vec(&progress).unwrap()).unwrap();
        assert_eq!(back, progress);

        let done = DoneMessage {
            bytes_written: 123_456,
        };
        let back: DoneMessage =
            serde_json::from_slice(&serde_json::to_vec(&done).unwrap()).unwrap();
        assert_eq!(back, done);

        let error = ErrorMessage {
            message: "cancelled".to_string(),
        };
        let back: ErrorMessage =
            serde_json::from_slice(&serde_json::to_vec(&error).unwrap()).unwrap();
        assert_eq!(back, error);
    }

    #[test]
    fn read_frame_clean_eof_before_any_byte() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(err.to_string(), "pipe closed");
    }

    #[test]
    fn read_frame_partial_frame_mid_length_prefix_is_unexpected_eof() {
        // Only the tag byte and two of the four length-prefix bytes.
        let buf = vec![tag::FINISH, 0, 0];
        let mut cur = Cursor::new(buf);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_frame_partial_frame_mid_payload_is_unexpected_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::ERROR, b"{\"message\":\"boom\"}").unwrap();
        buf.truncate(buf.len() - 3); // cut the frame mid-payload
        let mut cur = Cursor::new(buf);
        let err = read_frame(&mut cur).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn write_frame_encodes_empty_payload_as_five_byte_frame() {
        let mut buf = Vec::new();
        write_frame(&mut buf, tag::FINISH, &[]).unwrap();
        assert_eq!(buf, vec![tag::FINISH, 0, 0, 0, 0]);
    }
}
