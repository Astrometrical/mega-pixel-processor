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
inline constexpr uint32_t kProtocolVersion = 1;

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
};

/// Tags for frames the worker writes to its stdout (worker -> host).
/// `Invalid` is never sent on the wire (real worker tags are 1-4); it
/// exists only as a safe default for a default-constructed `WorkerFrame` so
/// an unfilled frame can never be mistaken for a real message.
/// docs/pi-protocol.md §4.
enum class WorkerTag : uint8_t {
  Invalid = 0,
  BandAck = 1,
  Progress = 2,
  Done = 3,
  Error = 4,
};

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
  std::string bit_depth;  // "f32" | "u16" | "u8"
  std::string shm_name;
  uint64_t slot_bytes = 0;
  uint32_t slots = 0;
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

/// A decoded worker->host frame (binary or JSON), tagged-union style: only
/// the field(s) matching `tag` are meaningful.
struct WorkerFrame {
  WorkerTag tag = WorkerTag::Invalid;

  uint32_t band_ack_slot = 0;    // valid iff tag == BandAck
  ProgressMessage progress{};    // valid iff tag == Progress
  DoneMessage done{};            // valid iff tag == Done
  ErrorMessage error{};          // valid iff tag == Error
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
/// one of the four worker tags, or a JSON/binary payload fails to parse in
/// the shape its tag requires.
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

}  // namespace mpp
