#pragma once
// Wire codec for the host<->worker control channel, mirroring
// crates/mpp-ipc/src/protocol.rs. Frame format:
//   [u8 tag][u32 LE payload_len][payload bytes]
// The `Band` frame is a fixed 20-byte little-endian binary payload;
// everything else is JSON (nlohmann). This is a NEW file for mpp (not a
// port): mpp's wire contract (host-push, docs/pi-protocol.md §3-§8) is a
// different, simpler shape than MergeMosaic's own protocol -- mmm_protocol.h
// was read only as a PATTERN for the framing/JSON style (see task-2 report),
// never copied.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE -- only the C++ stdlib, POSIX/
// Win32 (via mpp_os.h), and vendored json.hpp.

#include <cstdint>
#include <string>
#include <vector>

#include "mpp_os.h"
#include "third_party/json.hpp"

namespace mpp {

/// Wire-protocol version this host speaks, mirroring
/// `mpp_ipc::protocol::IPC_PROTOCOL_VERSION` (docs/pi-protocol.md §8).
/// `InitMessage::protocol_version` is stamped with this; the worker aborts
/// on a mismatch.
///
/// Version 2 adds a **read** direction alongside the original write-only
/// host-push protocol (`crates/mpp-ipc/src/protocol.rs`'s module docs):
/// `InitMessage::mode` selects which way a session runs, and the worker can
/// report `ImageMeta` (tag `kTagImageMeta`) and request bands via
/// `kTagReadBand`, acked by the host via `kTagReadBandAck`. The full read-mode
/// contract now lives in `docs/pi-protocol.md` §9 (the wire-shape source of
/// truth); this header mirrors the Rust definitions in
/// `crates/mpp-ipc/src/protocol.rs` field-for-field.
///
/// Version 3 widens `InitMessage` only -- no new tag, no changed binary
/// layout (docs/pi-protocol.md §10). `InitMessage::band_format` names the
/// sample type the host places in shm slots, separating it from
/// `InitMessage::bit_depth` (the *output file's* sample format); the rest of
/// the v3 fields carry the TIFF writer's option set (container, layout,
/// compression, alpha, ICC profile, text tags, resolution). Every v3 field is
/// `#[serde(default)]` on the Rust side, so a v2 host's `Init` payload still
/// deserializes into exactly the v2 behaviour.
inline constexpr uint32_t kProtocolVersion = 3;

/// Exact release version of the `mpp-ipc-worker` binary this host library
/// was built to drive (the worker's `CARGO_PKG_VERSION`). Stamped into
/// every `Init`; the worker refuses the job on any mismatch. Kept in exact
/// sync with the workspace `Cargo.toml` version (currently "0.1.0") --
/// hand-written here for now, guarded by Task 4's
/// `crates/mpp-ipc-worker/tests/version_sync.rs`. See docs/pi-protocol.md
/// §8's "Keeping the C++ host's expected worker_version in sync" note.
inline constexpr const char* kExpectedWorkerVersion = "0.1.0";

/// Tags for frames the host writes to the worker's stdin (host -> worker).
/// docs/pi-protocol.md §4.
enum class HostTag : uint8_t {
  Init = 128,
  Band = 129,
  Finish = 130,
  Cancel = 131,
  /// Acks a worker `READ_BAND` request (read-direction sessions, protocol
  /// v2+): 4-byte `slot: u32` payload naming the slot the host has finished
  /// consuming, now free for the worker to reuse.
  ReadBandAck = 132,
};

/// Tags for frames the worker writes to its stdout (worker -> host).
/// `Invalid` is never sent on the wire (real worker tags are 1-6); it
/// exists only as a safe default for a default-constructed `WorkerFrame` so
/// an unfilled frame can never be mistaken for a real message.
/// docs/pi-protocol.md §4.
enum class WorkerTag : uint8_t {
  Invalid = 0,
  BandAck = 1,
  Progress = 2,
  Done = 3,
  Error = 4,
  /// The worker's authoritative geometry for a read session (protocol v2+),
  /// sent once immediately after a `Read`-mode `Init` handshake, before any
  /// `ReadBand` request. JSON payload, `ImageMeta`.
  ImageMeta = 5,
  /// The worker requests one band of pixel data (read-direction sessions,
  /// protocol v2+): 20-byte binary payload, `BandHeader` (the same layout
  /// as the host->worker `Band` frame, direction reversed).
  ReadBand = 6,
};

/// Convenience byte-valued aliases for the protocol v2 tag numbers, mirroring
/// `mpp_ipc::protocol::tag::{IMAGE_META,READ_BAND,READ_BAND_ACK}` exactly.
/// Equivalent to `static_cast<uint8_t>(WorkerTag::ImageMeta)` etc. -- provided
/// so callers (and this library's own golden tests) can compare a raw wire
/// tag byte without spelling out the cast every time.
inline constexpr uint8_t kTagImageMeta = static_cast<uint8_t>(WorkerTag::ImageMeta);
inline constexpr uint8_t kTagReadBand = static_cast<uint8_t>(WorkerTag::ReadBand);
inline constexpr uint8_t kTagReadBandAck = static_cast<uint8_t>(HostTag::ReadBandAck);

/// Host -> worker `Init` message (tag `HostTag::Init`), sent bare (its JSON
/// fields directly, not wrapped as `{"Init": {...}}`) as the very first
/// frame of a run. Field names are exactly as written -- no camelCase
/// conversion -- per docs/pi-protocol.md §5.
struct InitMessage {
  uint32_t protocol_version = kProtocolVersion;
  std::string worker_version = kExpectedWorkerVersion;
  std::string output_path;
  uint64_t width = 0;
  uint64_t height = 0;
  uint64_t channels = 0;
  std::string bit_depth;  // output file format: "f32"|"f64"|"u32"|"u16"|"u8"
  std::string shm_name;
  uint64_t slot_bytes = 0;
  uint32_t slots = 0;
  /// Which direction this session runs: `"write"` (the v1 host-push flow,
  /// the default -- matches `mpp_ipc::protocol::SessionMode`'s
  /// `#[derive(Default)]`) or `"read"` (the v2 worker-pull flow). Always
  /// emitted on the wire by `encode_init` (never omitted), even for a write
  /// session, matching the Rust struct's own serialization (only
  /// *deserializing* a v1 payload that omits the field defaults it to
  /// `Write` -- `encode_init` never needs to produce that shape itself).
  std::string mode = "write";

  // ---- protocol v3 -----------------------------------------------------
  // Every field below is `#[serde(default)]` on the Rust side, so a v2 host
  // that omits them all still gets exactly the v2 behaviour. The defaults
  // spelled out here are therefore not free choices: each one must equal the
  // value `mpp_ipc::protocol::InitMessage`'s serde default produces, or this
  // host would silently ask for something a v2 host did not. (`deflate_level`
  // is the trap: the natural C++ "unset" default of 0 is NOT the wire
  // default, which is 6.)
  //
  // `band_format` describes the **shm payload** -- the sample type the host
  // writes into slots -- while `bit_depth` describes the **output file**.
  // Coupling rule (docs/pi-protocol.md §10, enforced by the worker and
  // mirrored by `WriteSession`): `band_format` must equal `bit_depth` unless
  // `band_format` is `"f32"`, the one domain the writer converts from. A
  // native (non-f32) band is written to the file verbatim, so there is no
  // conversion available if the two disagree.
  std::string band_format = "f32";  ///< **(v3)** shm sample type: "f32"|"f64"|"u8"|"u16"|"u32"
  std::string container = "auto";   ///< **(v3)** "auto"|"classic"|"big"
  std::string layout = "strips";    ///< **(v3)** "strips"|"tiles"
  uint32_t tile_size = 256;         ///< **(v3)** tile edge; power of two 128..1024
  std::string compression = "none"; ///< **(v3)** "none"|"lzw"|"deflate"|"packbits"
  uint8_t deflate_level = 6;        ///< **(v3)** 1-9; ignored unless compression == "deflate"
  bool predictor = false;           ///< **(v3)** horizontal predictor; integer formats only
  std::string alpha = "none";       ///< **(v3)** "none"|"unassociated"|"associated"
  std::string icc_profile;          ///< **(v3)** base64 ICC blob, empty when absent
  std::string description;          ///< **(v3)** TIFF tag 270 (ImageDescription)
  std::string copyright;            ///< **(v3)** TIFF tag 33432 (Copyright)
  std::string software;             ///< **(v3)** TIFF tag 305 (Software)
  double x_resolution = 0.0;        ///< **(v3)** pixels per unit; 0 = unknown
  double y_resolution = 0.0;        ///< **(v3)** pixels per unit; 0 = unknown
  std::string resolution_unit = "inch";  ///< **(v3)** "inch"|"cm"
};

/// Host -> worker `Band` frame payload (tag `HostTag::Band`): names one
/// contiguous run of rows of one channel, already written by the host into
/// shm `slot`. Binary layout, little-endian, exactly this field order (not
/// alphabetized or regrouped), 20 bytes total -- docs/pi-protocol.md §4.
struct BandHeader {
  uint32_t channel = 0;
  uint64_t start_row = 0;
  uint32_t rows = 0;
  uint32_t slot = 0;

  /// Encoded size in bytes of a `BandHeader`.
  static constexpr size_t kWireLen = 20;

  /// Encodes to the 20-byte little-endian wire layout.
  std::vector<uint8_t> to_bytes() const;

  /// Decodes from the 20-byte little-endian wire layout. Throws
  /// `std::runtime_error` if `payload` is not exactly `kWireLen` bytes.
  static BandHeader from_bytes(const std::vector<uint8_t>& payload);
};

/// Worker -> host `Progress` message (tag `WorkerTag::Progress`).
struct ProgressMessage {
  uint64_t rows_done = 0;
  uint64_t rows_total = 0;
};

/// Worker -> host `Done` message (tag `WorkerTag::Done`); the last frame the
/// worker sends on a successful run, followed by exit code 0.
struct DoneMessage {
  uint64_t bytes_written = 0;
};

/// Worker -> host `Error` message (tag `WorkerTag::Error`); the last frame
/// the worker sends on a failed run, followed by a nonzero exit code.
struct ErrorMessage {
  std::string message;
};

/// Worker -> host `ImageMeta` message (tag `WorkerTag::ImageMeta`): the
/// worker's authoritative geometry for a read session, derived from the
/// image it opened rather than anything the host asserted in `Init`.
/// Mirrors `mpp_ipc::protocol::ImageMeta` field-for-field.
struct ImageMeta {
  uint64_t width = 0;
  uint64_t height = 0;
  uint64_t channels = 0;  // total channel count, including alpha
  uint64_t alpha = 0;     // 1 if `channels` includes an alpha channel, 0 otherwise
  std::string color_space;  // "gray" | "rgb"
  std::string bit_depth;    // "u8" | "u16" | "u32" | "f32" | "f64"
  std::string icc_profile;  ///< **(v3)** base64 ICC blob the worker found embedded in the
                             ///< file, empty when absent. Optional on the wire: an older
                             ///< worker's `ImageMeta` JSON has no such field at all, and
                             ///< `ParseImageMeta` defaults it to empty rather than throwing.
};

/// Parses an `ImageMeta` JSON payload (as sent worker->host on tag
/// `WorkerTag::ImageMeta`, or printed standalone by `mpp-ipc-worker --probe`).
/// Throws `std::runtime_error` (via nlohmann's own exceptions, or an
/// explicit throw for a missing/mistyped field) if `json` does not parse or
/// is missing a required field.
ImageMeta ParseImageMeta(const std::string& json);

/// A decoded worker->host frame (binary or JSON), tagged-union style: only
/// the field(s) matching `tag` are meaningful.
struct WorkerFrame {
  WorkerTag tag = WorkerTag::Invalid;

  uint32_t band_ack_slot = 0;    // valid iff tag == BandAck
  ProgressMessage progress{};    // valid iff tag == Progress
  DoneMessage done{};            // valid iff tag == Done
  ErrorMessage error{};          // valid iff tag == Error
  ImageMeta image_meta{};        // valid iff tag == ImageMeta
  BandHeader read_band{};        // valid iff tag == ReadBand
};

/// Reads one raw frame from `h` (blocking): the tag byte and payload bytes,
/// with the length prefix already consumed/validated. Returns false on a
/// clean EOF encountered before any byte of a new frame (docs/pi-protocol.md
/// §2 -- the peer closed its output deliberately). Throws
/// `std::runtime_error` if EOF is hit mid-length-prefix or mid-payload (a
/// truncated frame), per §2's "must be treated as an error, not a clean
/// shutdown" rule.
bool read_frame(os_handle h, uint8_t& tag, std::vector<uint8_t>& payload);

/// Writes one frame to `h`: tag byte, 4-byte LE payload length, then `len`
/// bytes of payload.
void write_frame(os_handle h, uint8_t tag, const uint8_t* payload, uint32_t len);

/// Reads and decodes one worker->host frame from `h` (blocking). Returns
/// false on a clean EOF before any byte of a new frame. Throws
/// `std::runtime_error` if EOF is hit mid-frame (truncated), the tag is not
/// one of the six worker tags (`BandAck`, `Progress`, `Done`, `Error`,
/// `ImageMeta`, `ReadBand`), or a JSON/binary payload fails to parse in the
/// shape its tag requires.
bool read_worker_frame(os_handle h, WorkerFrame& out);

/// Builds a framed `Init` frame (tag 128) from an `InitMessage`, ready to
/// write to the worker's stdin with a single `os_write` loop (or via
/// `write_frame`'s wrapper -- this returns the fully framed bytes,
/// tag+len+payload, matching `write_frame`'s output shape for tag
/// `HostTag::Init`).
std::vector<uint8_t> encode_init(const InitMessage& init);

/// Builds a framed `Band` frame (tag 129) from a `BandHeader`.
std::vector<uint8_t> encode_band(const BandHeader& header);

/// Builds a framed, empty-payload `Finish` frame (tag 130).
std::vector<uint8_t> encode_finish();

/// Builds a framed, empty-payload `Cancel` frame (tag 131).
std::vector<uint8_t> encode_cancel();

/// Builds a framed `ReadBandAck` frame (tag 132, `kTagReadBandAck`): a
/// 4-byte little-endian `slot` payload telling the worker that slot is free
/// for reuse (read-direction sessions, protocol v2+).
std::vector<uint8_t> encode_read_band_ack(uint32_t slot);

}  // namespace mpp
