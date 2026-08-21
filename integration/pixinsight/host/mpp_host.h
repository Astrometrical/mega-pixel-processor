#pragma once
// mpp_host.h -- WriteSession: the host-side RAII driver that streams pixel
// bands to mpp-ipc-worker over the wire protocol (docs/pi-protocol.md) and
// produces one encoded output file per session. This is what Task 4's PCL
// module instance will hold one of per file-write; today it is exercised
// only by this directory's golden CTest suite (no PixInsight installed).
//
// Divergence from MergeMosaic's mmm_host.h/.cpp (design PATTERN only, read
// under the clean-room allowance -- see the task-3 report for the full
// comparison): mmm's Host is a bidirectional, request/reply, callback-driven
// server loop (the worker PULLS input panel rows and PUSHES output bands, so
// Host implements PanelSource/OutputCollector abstract interfaces and
// Host::run() blocks for the whole job). mpp's protocol is host-push, one
// direction only (docs/pi-protocol.md §1): the host already has every pixel
// and simply streams Band frames as PushBand() is called, so WriteSession
// has no callback interfaces to implement -- it is closer in shape to mmm's
// own probe_frame/probe_panels helpers (call, block, get a result or throw)
// than to Host::run()'s long-lived serve loop. The synchronous
// fill-a-slot/send/await-ack rhythm generalizes
// crates/mpp-ipc/src/testhost.rs's Session::send_band_and_await_ack (same
// project, protocol-faithful reference, allowed as design cross-check) to a
// free-slot pool so multiple bands can be in flight before PushBand ever
// needs to block -- mmm's Host has no equivalent, since its slots are filled
// by the WORKER's BandRequest, never pushed by the host.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE (clean-room rule): this file and
// everything else in integration/pixinsight/host/ must build and test
// without PixInsight installed.

#include <cstdint>
#include <deque>
#include <functional>
#include <stdexcept>
#include <string>
#include <vector>

#include "mpp_os.h"
#include "mpp_protocol.h"
#include "mpp_shm.h"

namespace mpp {

/// Configuration for one `WriteSession`: the worker binary to spawn and the
/// output image's geometry/band shape. `band_rows` is the nominal row count
/// per `PushBand` call the caller commits to -- it sizes the shm slots
/// (`slot_bytes = band_rows * width * sizeof(float)`), so every `PushBand`
/// call's `rows` must be `<= band_rows` (the last band of a channel may be
/// shorter; `WriteSession` rejects a larger one as a host bug rather than
/// risk a slot overrun -- see docs/pi-protocol.md §6's "slot-size
/// validation" rule, which this mirrors host-side before the worker ever
/// sees the band).
struct WriteSessionConfig {
  std::string worker_exe;   ///< Absolute path to the `mpp-ipc-worker` binary.
  std::string output_path;  ///< Output file path the worker will encode to.
  uint64_t width = 0;       ///< Image width in pixels.
  uint64_t height = 0;      ///< Image height in pixels.
  uint64_t channels = 0;    ///< Channel count: `1` or `3`.
  std::string bit_depth;    ///< Output sample format: `"f32"`, `"u16"`, or `"u8"`.
  uint32_t band_rows = 0;   ///< Nominal rows per `PushBand` call; sizes the shm slots.
  uint32_t slots = 0;       ///< Number of shm slots to create; `>= 2` recommended.
};

/// Progress snapshot pumped into `WriteSession::Finish`'s callback, mirroring
/// the worker's `ProgressMessage` (docs/pi-protocol.md §4).
struct ProgressInfo {
  uint64_t rows_done = 0;
  uint64_t rows_total = 0;
};

/// Thrown by `WriteSession` on any fault: a worker `Error` frame, an
/// unexpected/early worker exit, a truncated/malformed frame, or a host-side
/// usage error (e.g. a `PushBand` call larger than the configured
/// `band_rows`, or a nonexistent `worker_exe`). Mirrors MergeMosaic's
/// `mmm::HostError` (same role: the one exception type this driver ever lets
/// escape a public method).
struct HostError : std::runtime_error {
  using std::runtime_error::runtime_error;
};

/// Drives one output file's write session to completion over the host-push
/// protocol (docs/pi-protocol.md). Not copyable or movable (owns a live
/// child process and shm mapping via raw handles); construct one per file
/// write.
///
/// RAII: the destructor kills and reaps the worker process if it is still
/// running when the session is destroyed without having reached a terminal
/// state via `Finish` or `Cancel` (e.g. an exception unwinding mid-write) --
/// a `WriteSession` going out of scope never leaves an orphaned worker
/// process behind. The shm segment and the stdin/stdout pipes are released
/// by their own RAII members (`ShmSegment`, `Pipe`) regardless of how this
/// class's constructor or methods exit.
class WriteSession {
 public:
  /// Validates `cfg`, creates the shm segment, spawns `cfg.worker_exe`, and
  /// sends `Init`. Throws `HostError` if `cfg` is invalid, `cfg.worker_exe`
  /// does not exist, the shm segment cannot be created, the process cannot
  /// be spawned, or writing the `Init` frame fails -- in every failure case
  /// any already-spawned worker process is killed and reaped before the
  /// exception propagates, so a failed construction never leaks a process.
  explicit WriteSession(const WriteSessionConfig& cfg);

  ~WriteSession();

  WriteSession(const WriteSession&) = delete;
  WriteSession& operator=(const WriteSession&) = delete;
  WriteSession(WriteSession&&) = delete;
  WriteSession& operator=(WriteSession&&) = delete;

  /// Blocks until a shm slot is free -- reading and handling worker frames
  /// while none is: a `BandAck` frees the slot it names, a `Progress` frame
  /// is tolerated and dropped (no callback is registered during band
  /// streaming; see `Finish` for progress reporting) -- per
  /// docs/pi-protocol.md §7.7. Once a slot is free, copies `rows * width`
  /// `f32` samples of `channel` into it (native-endian, row-major, exactly
  /// the shm layout docs/pi-protocol.md §6 specifies) and sends the matching
  /// `Band` frame. Channels/rows may be pushed in any order
  /// (docs/pi-protocol.md §7.2). Throws `HostError` if `channel` or the
  /// `[start_row, start_row + rows)` range fall outside the configured
  /// geometry, `rows` exceeds the configured `band_rows`, the worker reports
  /// `Error`, or the worker exits/closes its output unexpectedly.
  void PushBand(uint32_t channel, uint64_t start_row, uint32_t rows, const float* samples);

  /// Drains any outstanding (unacked) bands -- pumping `Progress` frames
  /// seen along the way into `cb` -- then sends `Finish` and reads frames
  /// until the terminal one: further `Progress` frames are pumped into `cb`
  /// (which may be an empty `std::function`, i.e. no callback), and `Done`
  /// returns `bytes_written`. Throws `HostError` on an `Error` frame, an
  /// unexpected worker exit, or a worker exit code that disagrees with its
  /// own `Done` frame (docs/pi-protocol.md §4: the exit code is the
  /// authoritative completion signal). Must be called at most once, and not
  /// after `Cancel`.
  uint64_t Finish(const std::function<void(ProgressInfo)>& cb);

  /// Sends `Cancel` -- only meaningful before `Finish` is sent, per
  /// docs/pi-protocol.md §7.4 -- and reads frames (tolerating `BandAck`/
  /// `Progress`) until the worker's terminal reply. Always throws
  /// `HostError`: a cancelled run is a deliberate stop the caller asked for,
  /// never a value that could be mistaken for a successful `Finish`. On the
  /// expected path the worker completes its best-effort cleanup and replies
  /// `Error{"cancelled"}`, so the thrown message contains `"cancelled"`; any
  /// other worker reply (e.g. `Done`, or the connection closing) still
  /// throws `HostError`, naming the deviation. Must be called at most once,
  /// and not after `Finish`.
  void Cancel();

 private:
  // ---- band accounting ----
  void WaitForFreeSlot();
  void FreeSlot(uint32_t slot);

  // ---- frame IO ----
  // Reads one worker->host frame; throws HostError (reaping the worker) on
  // EOF or a malformed frame instead of returning a sentinel, since every
  // caller in this class would otherwise have to duplicate that check.
  WorkerFrame ReadFrameOrThrow();
  void WriteFramed(const std::vector<uint8_t>& framed);

  // ---- process lifecycle ----
  int ReapBlocking();               ///< Blocking wait for the child to exit; idempotent.
  void KillAndReapBestEffort() noexcept;  ///< Used by the destructor and construction failures.

  WriteSessionConfig cfg_;
  uint64_t slot_bytes_ = 0;

  ShmSegment shm_;
  Pipe in_pipe_;   // host -> worker stdin
  Pipe out_pipe_;  // worker -> host, reads worker stdout
  os_pid pid_ = os_invalid_pid;
  bool spawned_ = false;  ///< True once os_spawn succeeded (dtor only acts then).
  bool reaped_ = false;
  bool finished_ = false;  ///< True once Finish/Cancel/a fault reached a terminal state.

  std::deque<uint32_t> free_slots_;
  uint32_t outstanding_ = 0;  ///< Bands sent, not yet acked.
};

}  // namespace mpp
