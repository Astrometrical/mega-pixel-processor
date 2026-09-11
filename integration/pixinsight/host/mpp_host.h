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

/// Configuration for one `WriteSession`: the worker binary to spawn, the
/// output image's geometry/band shape, and (protocol v3) the full set of
/// TIFF write options the worker's encoder accepts. `band_rows` is the
/// nominal row count per `PushBand`/`PushBandBytes` call the caller commits
/// to -- it sizes the shm slots (`slot_bytes = band_rows * width *
/// WriteSession::SampleBytes(band_format)`), so every push call's `rows`
/// must be `<= band_rows` (the last band of a channel may be shorter;
/// `WriteSession` rejects a larger one as a host bug rather than risk a slot
/// overrun -- see docs/pi-protocol.md §6's "slot-size validation" rule,
/// which this mirrors host-side before the worker ever sees the band).
///
/// **`band_format` vs `bit_depth`** (docs/pi-protocol.md §10): `band_format`
/// is the sample type this host writes into the shm slots; `bit_depth` is
/// the sample format of the *output file*. They are coupled by one rule the
/// constructor enforces (as the worker does): `band_format` must equal
/// `bit_depth` unless `band_format` is `"f32"`, the one sample domain the
/// writer converts from. A native (non-f32) band is written verbatim, so
/// there is no conversion path if the two disagree.
///
/// Every v3 option below is defaulted to its wire default, which is also the
/// v1/v2 behaviour -- a caller that fills in only the v1 fields gets exactly
/// the pre-v3 session it used to.
struct WriteSessionConfig {
  std::string worker_exe;   ///< Absolute path to the `mpp-ipc-worker` binary.
  std::string output_path;  ///< Output file path the worker will encode to.
  uint64_t width = 0;       ///< Image width in pixels.
  uint64_t height = 0;      ///< Image height in pixels.
  uint64_t channels = 0;    ///< Channel count: `1`, `2`, `3` or `4`.
  /// Output file sample format: `"f32"`, `"f64"`, `"u32"`, `"u16"` or `"u8"`.
  std::string bit_depth;
  uint32_t band_rows = 0;   ///< Nominal rows per push call; sizes the shm slots.
  uint32_t slots = 0;       ///< Number of shm slots to create; `>= 2` recommended.

  // ---- protocol v3 write options --------------------------------------
  /// **(v3)** shm sample type: `"f32"` (default; the v1/v2 behaviour and the
  /// only converting path), `"f64"`, `"u32"`, `"u16"` or `"u8"`. See the
  /// coupling rule in this struct's doc comment.
  std::string band_format = "f32";
  std::string container = "auto";    ///< **(v3)** `"auto"`, `"classic"` or `"big"`.
  std::string layout = "strips";     ///< **(v3)** `"strips"` or `"tiles"`.
  uint32_t tile_size = 256;          ///< **(v3)** tile edge, power of two 128..1024.
  std::string compression = "none";  ///< **(v3)** `"none"`, `"lzw"`, `"deflate"`, `"packbits"`.
  /// **(v3)** Deflate level 1-9; ignored unless `compression == "deflate"`.
  /// Defaults to `6`, the wire default -- **not** `0`, which is not a legal
  /// level and would not match what a v2 host's omitted field means.
  uint8_t deflate_level = 6;
  bool predictor = false;            ///< **(v3)** horizontal predictor; integer formats only.
  std::string alpha = "none";        ///< **(v3)** `"none"`, `"unassociated"`, `"associated"`.
  /// **(v3)** Base64-encoded ICC profile to embed; empty means none. Capped
  /// at 16 MiB decoded, checked by the constructor before a process is
  /// spawned (the worker enforces the same bound).
  std::string icc_profile_base64;
  std::string description;           ///< **(v3)** TIFF tag 270 (ImageDescription).
  std::string copyright;             ///< **(v3)** TIFF tag 33432 (Copyright).
  std::string software;              ///< **(v3)** TIFF tag 305 (Software).
  double x_resolution = 0.0;         ///< **(v3)** pixels per unit; `0` = unknown.
  double y_resolution = 0.0;         ///< **(v3)** pixels per unit; `0` = unknown.
  std::string resolution_unit = "inch";  ///< **(v3)** `"inch"` or `"cm"`.
};

/// Progress snapshot pumped into `WriteSession::Finish`'s callback, mirroring
/// the worker's `ProgressMessage` (docs/pi-protocol.md §4). `stage` says
/// which phase -- and therefore which unit -- `rows_done`/`rows_total` are
/// in: band streaming counts staged rows over every channel, encoding
/// counts output rows written to the file. Both stages can be seen from
/// `Finish`'s callback, since the band phase's final report may only reach
/// the host after `Finish` has gone out.
struct ProgressInfo {
  uint64_t rows_done = 0;
  uint64_t rows_total = 0;
  ProgressStage stage = ProgressStage::Bands;
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
  /// docs/pi-protocol.md §7.7. Once a slot is free, copies
  /// `rows * width * SampleBytes(cfg.band_format)` bytes of `channel` from
  /// `samples` into it (native-endian, row-major, exactly the shm layout
  /// docs/pi-protocol.md §6 specifies) and sends the matching `Band` frame.
  /// `samples` must point at `rows * width` samples of the session's
  /// negotiated `band_format` -- not always `f32`, as it was before protocol
  /// v3. Channels/rows may be pushed in any order (docs/pi-protocol.md
  /// §7.2). Throws `HostError` if `channel` or the
  /// `[start_row, start_row + rows)` range fall outside the configured
  /// geometry, `rows` exceeds the configured `band_rows`, the worker reports
  /// `Error`, or the worker exits/closes its output unexpectedly.
  void PushBandBytes(uint32_t channel, uint64_t start_row, uint32_t rows, const void* samples);

  /// f32 convenience wrapper over `PushBandBytes`, for the (default) session
  /// that negotiated `band_format == "f32"`. Throws `HostError` if it did
  /// not: on, say, a `"u32"` session the buffer's element type would
  /// disagree with the wire format, and silently copying
  /// `rows * width * 4` bytes of `float` into a slot the worker reads back
  /// as `u32` is exactly the class of bug protocol v3 exists to eliminate.
  void PushBand(uint32_t channel, uint64_t start_row, uint32_t rows, const float* samples);

  /// Sample width in bytes of `band_format` -- the unit `PushBandBytes`
  /// measures its buffer in, and the multiplier in this session's
  /// `slot_bytes`. `4` for `"f32"`/`"u32"`, `8` for `"f64"`, `2` for
  /// `"u16"`, `1` for `"u8"`. Throws `HostError` for any other string
  /// (`WriteSession`'s constructor calls this to validate `cfg.band_format`,
  /// so an unknown format never reaches the worker).
  static size_t SampleBytes(const std::string& band_format);

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

  /// Registers `cb` to be invoked whenever this session has been waiting on
  /// a worker frame for `kIdleWaitMs` with nothing arriving -- repeatedly,
  /// about once per `kIdleWaitMs`, for as long as the silence lasts. The
  /// PixInsight module uses it to pump the GUI event queue while the worker
  /// is busy but quiet (staging the last bands, or the whole encode phase
  /// between `Finish` and `Done`), so the Console keeps repainting and the
  /// window is not flagged "not responding". Called on the thread that is
  /// blocked in `PushBandBytes`/`Finish`/`Cancel`; an empty function clears
  /// it. Ported from MergeMosaic's `ProgressCallback::on_idle` rhythm.
  void SetIdleCallback(std::function<void()> cb) { idle_cb_ = std::move(cb); }

  /// How long a frame read waits in silence before the idle callback fires.
  static constexpr int kIdleWaitMs = 100;

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
  // Validates a worker-supplied BandAck slot id (`slot < cfg_.slots` AND
  // `slot_busy_[slot]` -- i.e. currently owned by a Band we sent and have
  // not already freed) before trusting it: an out-of-range id would index
  // outside the shm mapping on the next PushBand's memcpy, and an in-range
  // but not-busy id (stale/duplicate ack) would let a slot be reused while
  // a Band we already sent for it may still be in flight. Throws
  // `HostError` naming the offending slot and kills+reaps the worker
  // (`KillAndReapBestEffort`, not a blocking wait -- a worker that just
  // sent a bogus ack cannot be trusted to exit on its own) on either
  // violation; only marks the slot free on success.
  void FreeSlot(uint32_t slot);

  // ---- frame IO ----
  // Reads one worker->host frame; throws HostError (reaping the worker) on
  // EOF or a malformed frame instead of returning a sentinel, since every
  // caller in this class would otherwise have to duplicate that check.
  // While the pipe is silent, fires idle_cb_ every kIdleWaitMs (see
  // SetIdleCallback) before blocking in the read proper.
  WorkerFrame ReadFrameOrThrow();
  void WriteFramed(const std::vector<uint8_t>& framed);

  // ---- process lifecycle ----
  int ReapBlocking();               ///< Blocking wait for the child to exit; idempotent.
  // Kills the child (does not wait for a voluntary exit) then reaps it;
  // idempotent. Used by the destructor, construction failures, and any
  // point where the worker has just been caught violating the protocol
  // (e.g. a bogus BandAck slot id -- see FreeSlot) and so cannot be trusted
  // to exit on its own the way a legitimate Done/Error terminal frame
  // promises (docs/pi-protocol.md §4) -- ReapBlocking's plain wait would be
  // unbounded in that case.
  void KillAndReapBestEffort() noexcept;

  WriteSessionConfig cfg_;
  uint64_t slot_bytes_ = 0;

  ShmSegment shm_;
  Pipe in_pipe_;   // host -> worker stdin
  Pipe out_pipe_;  // worker -> host, reads worker stdout
  os_pid pid_ = os_invalid_pid;
  bool spawned_ = false;  ///< True once os_spawn succeeded (dtor only acts then).
  bool reaped_ = false;
  bool finished_ = false;  ///< True once Finish/Cancel/a fault reached a terminal state.

  std::function<void()> idle_cb_;  ///< See SetIdleCallback; empty = none.

  std::deque<uint32_t> free_slots_;
  // Busy-set: slot_busy_[i] is true iff slot i was handed out in a Band
  // frame we sent and has not yet been freed by a validated BandAck. Every
  // worker-supplied BandAck slot id is checked against this (range AND
  // membership) before it is trusted to free a slot or index the shm
  // mapping -- ported from MergeMosaic's mmm_host.cpp Host::run
  // ("Host memory safety first" guard on BandRequest/OutputBand slot ids,
  // mmm_host.cpp:534-563), which this file's first pass omitted. See
  // FreeSlot's doc comment.
  std::vector<bool> slot_busy_;
  uint32_t outstanding_ = 0;  ///< Bands sent, not yet acked.
};

/// Configuration for one `ReadSession`: the worker binary to spawn, the
/// input file to open, and enough of its geometry (typically from a prior
/// `ReadSession::Probe` call) to size the shm slots before `ImageMeta`
/// arrives. `band_rows` is the row count per band the *host* commits shm
/// space for -- it must be `>=` the largest band the worker will ever send
/// for this file (the worker, not the host, decides actual band shape from
/// the file's own chunk layout; sizing this too small surfaces as a
/// `HostError` from `ReadAll`, mirroring `WriteSessionConfig::band_rows`'s
/// slot-size discipline inverted). `sample_bytes` is the native on-disk
/// sample size of the file's `bit_depth` (`ImageMeta::bit_depth`): `1` for
/// `"u8"`, `2` for `"u16"`, `4` for `"u32"`/`"f32"`, `8` for `"f64"`.
struct ReadSessionConfig {
  std::string worker_exe;   ///< Absolute path to the `mpp-ipc-worker` binary.
  std::string input_path;   ///< Existing image file the worker will open and stream.
  uint32_t band_rows = 0;   ///< Rows per band the host provisions shm space for.
  uint32_t slots = 0;       ///< Number of shm slots to create; `>= 2` recommended.
  uint64_t sample_bytes = 0;  ///< Native sample size in bytes of the file's format.
  uint64_t width = 0;       ///< Image width in pixels (from a prior probe), to size slots.
  uint64_t channels = 0;    ///< Channel count (informational; the worker is authoritative).
};

/// Drives one input file's read session to completion over the
/// worker-pull direction of the protocol (protocol v2, `docs/pi-protocol.md`
/// pending its read-mode addendum -- see `mpp_protocol.h`'s `kProtocolVersion`
/// doc comment): the *worker* fills shm slots and requests them via
/// `ReadBand`, and this class frees them by replying `ReadBandAck`, the
/// exact inversion of `WriteSession`'s `PushBand`/`BandAck` rhythm. Not
/// copyable or movable (owns a live child process and shm mapping via raw
/// handles); construct one per file read.
///
/// RAII: the destructor kills and reaps the worker process if it is still
/// running when the session is destroyed without having reached a terminal
/// state via `ReadAll` (e.g. an exception unwinding mid-read) -- a
/// `ReadSession` going out of scope never leaves an orphaned worker process
/// behind, mirroring `WriteSession`'s own destructor contract exactly.
class ReadSession {
 public:
  /// Validates `cfg`, creates the shm segment, spawns `cfg.worker_exe` with
  /// `mode: "read"`, sends `Init`, and reads the worker's first reply frame,
  /// which must be `ImageMeta` -- stored and returned by `Meta()`. Throws
  /// `HostError` if `cfg` is invalid, `cfg.worker_exe` does not exist, the
  /// shm segment cannot be created, the process cannot be spawned, sending
  /// `Init` fails, the worker replies `Error` instead of `ImageMeta` (e.g.
  /// the file does not exist or is not a supported format), or the worker
  /// sends any other unexpected frame/exits early -- in every failure case
  /// any already-spawned worker process is killed and reaped before the
  /// exception propagates.
  explicit ReadSession(const ReadSessionConfig& cfg);

  ~ReadSession();

  ReadSession(const ReadSession&) = delete;
  ReadSession& operator=(const ReadSession&) = delete;
  ReadSession(ReadSession&&) = delete;
  ReadSession& operator=(ReadSession&&) = delete;

  /// The worker's authoritative geometry for the opened file, as reported by
  /// its `ImageMeta` reply to `Init`. Valid for the lifetime of the session
  /// (set once, during construction).
  const ImageMeta& Meta() const { return meta_; }

  /// Pumps `ReadBand` frames from the worker until `Done`: for each band,
  /// copies nothing itself but hands `sink` a pointer directly into the shm
  /// slot the worker filled (`rows * Meta().width` native samples of one
  /// channel, `cfg.sample_bytes` bytes each, native-endian, row-major --
  /// the same in-slot layout `docs/pi-protocol.md` §6 specifies for the
  /// write direction, reversed), then acks the slot once `sink` returns so
  /// the worker may reuse it. Bands may arrive in any order and interleaved
  /// across channels, mirroring the write direction's own `PushBand`
  /// ordering freedom (`docs/pi-protocol.md` §7.2) -- `sink` must not assume
  /// sequential coverage. Must be called at most once. Throws `HostError` on
  /// a worker `Error` frame, an out-of-range or oversize `ReadBand` request,
  /// or an unexpected frame/early worker exit.
  void ReadAll(const std::function<void(uint32_t channel, uint64_t start_row, uint32_t rows,
                                         const void* samples)>& sink);

  /// As `WriteSession::SetIdleCallback`: `cb` fires about every
  /// `WriteSession::kIdleWaitMs` while `ReadAll` is waiting on a worker frame
  /// that has not arrived -- the read direction's silent stretches are the
  /// worker decoding a large chunk before its first band of it can be sent.
  void SetIdleCallback(std::function<void()> cb) { idle_cb_ = std::move(cb); }

  /// Standalone geometry probe: spawns `worker_exe --probe input_path`
  /// (no `Init` handshake -- the `--probe` CLI entry opens the file and
  /// prints its `ImageMeta` as JSON, then exits, per `mpp-ipc-worker`'s
  /// `main.rs`), reads its stdout, and parses it. Throws `HostError` if
  /// `worker_exe` does not exist, spawning fails, the process exits
  /// nonzero, or its stdout is not valid `ImageMeta` JSON. Does not create
  /// any shm segment or otherwise touch the wire protocol proper -- this is
  /// a lighter-weight query than constructing a full `ReadSession`, useful
  /// for sizing a subsequent `ReadSessionConfig` (its `width`/`sample_bytes`)
  /// before committing to shm.
  static ImageMeta Probe(const std::string& worker_exe, const std::string& input_path);

 private:
  // ---- frame IO ----
  // Reads one worker->host frame; throws HostError (reaping the worker) on
  // EOF or a malformed frame -- mirrors WriteSession::ReadFrameOrThrow
  // exactly (see its doc comment).
  WorkerFrame ReadFrameOrThrow();
  void WriteFramed(const std::vector<uint8_t>& framed);

  // ---- process lifecycle ----
  int ReapBlocking();               ///< Blocking wait for the child to exit; idempotent.
  void KillAndReapBestEffort() noexcept;  ///< Kills then reaps; idempotent. See WriteSession's.

  ReadSessionConfig cfg_;
  uint64_t slot_bytes_ = 0;
  ImageMeta meta_{};
  std::function<void()> idle_cb_;  ///< See SetIdleCallback; empty = none.

  ShmSegment shm_;
  Pipe in_pipe_;   // host -> worker stdin
  Pipe out_pipe_;  // worker -> host, reads worker stdout
  os_pid pid_ = os_invalid_pid;
  bool spawned_ = false;  ///< True once os_spawn succeeded (dtor only acts then).
  bool reaped_ = false;
  bool finished_ = false;  ///< True once ReadAll/construction reached a terminal state.
};

}  // namespace mpp
