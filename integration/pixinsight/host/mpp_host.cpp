#include "mpp_host.h"

#include <atomic>
#include <cerrno>
#include <cstring>
#include <filesystem>

namespace mpp {

namespace {

// Validates `cfg` and returns it unchanged (throwing HostError, not any
// other exception type, so WriteSession's constructor never lets a raw
// std::runtime_error escape). Runs as the very first step of construction
// (see the member-initializer-list ordering in WriteSession's ctor below),
// before any shm segment or pipe is created, so an invalid config -- most
// importantly a nonexistent `worker_exe` -- fails fast with zero OS
// resources touched and no process ever spawned (the `test_worker_missing`
// golden test pins exactly this: no hang, no leaked resources).
WriteSessionConfig ValidateConfig(const WriteSessionConfig& cfg) {
  if (cfg.worker_exe.empty()) {
    throw HostError("WriteSessionConfig.worker_exe is empty");
  }
  if (!std::filesystem::exists(cfg.worker_exe)) {
    throw HostError("mpp-ipc-worker executable not found at \"" + cfg.worker_exe +
                     "\" -- check the worker path / installation next to the module");
  }
  if (cfg.width == 0 || cfg.height == 0) {
    throw HostError("WriteSessionConfig width/height must be nonzero");
  }
  if (cfg.channels == 0) {
    throw HostError("WriteSessionConfig.channels must be nonzero");
  }
  if (cfg.bit_depth != "f32" && cfg.bit_depth != "u16" && cfg.bit_depth != "u8") {
    throw HostError("WriteSessionConfig.bit_depth must be \"f32\", \"u16\", or \"u8\", got \"" +
                     cfg.bit_depth + "\"");
  }
  if (cfg.band_rows == 0) {
    throw HostError("WriteSessionConfig.band_rows must be nonzero");
  }
  if (cfg.slots == 0) {
    throw HostError("WriteSessionConfig.slots must be nonzero (>= 2 recommended)");
  }
  return cfg;
}

uint64_t ComputeSlotBytes(const WriteSessionConfig& cfg) {
  return static_cast<uint64_t>(cfg.band_rows) * cfg.width * sizeof(float);
}

// A short shm segment name satisfying docs/pi-protocol.md §6's portable
// grammar (leading '/', no other '/', <= 31 chars total, enforced on every
// platform including this one): "/mpp-<pid mod 100000>-<counter mod
// 100000>" is always <= 16 characters. The counter (not just the pid) keeps
// concurrent WriteSessions in the same process from colliding, mirroring
// crates/mpp-ipc/src/testhost.rs's `unique_id()`.
std::string GenerateShmName() {
  static std::atomic<uint64_t> counter{0};
  uint64_t id = counter.fetch_add(1, std::memory_order_relaxed) % 100000;
#ifdef _WIN32
  unsigned long pid = static_cast<unsigned long>(GetCurrentProcessId()) % 100000;
#else
  unsigned long pid = static_cast<unsigned long>(::getpid()) % 100000;
#endif
  return "/mpp-" + std::to_string(pid) + "-" + std::to_string(id);
}

// ReadSessionConfig's analog of ValidateConfig above: same "fail fast, zero
// resources touched, no process spawned" discipline, before any shm segment
// or pipe is created (see the member-initializer-list ordering in
// ReadSession's ctor below).
ReadSessionConfig ValidateReadConfig(const ReadSessionConfig& cfg) {
  if (cfg.worker_exe.empty()) {
    throw HostError("ReadSessionConfig.worker_exe is empty");
  }
  if (!std::filesystem::exists(cfg.worker_exe)) {
    throw HostError("mpp-ipc-worker executable not found at \"" + cfg.worker_exe +
                     "\" -- check the worker path / installation next to the module");
  }
  if (cfg.input_path.empty()) {
    throw HostError("ReadSessionConfig.input_path is empty");
  }
  if (cfg.width == 0) {
    throw HostError(
        "ReadSessionConfig.width must be nonzero (from a prior ReadSession::Probe call) -- it "
        "sizes the shm slots before ImageMeta arrives");
  }
  if (cfg.sample_bytes == 0) {
    throw HostError("ReadSessionConfig.sample_bytes must be nonzero");
  }
  if (cfg.band_rows == 0) {
    throw HostError("ReadSessionConfig.band_rows must be nonzero");
  }
  if (cfg.slots == 0) {
    throw HostError("ReadSessionConfig.slots must be nonzero (>= 2 recommended)");
  }
  return cfg;
}

uint64_t ComputeReadSlotBytes(const ReadSessionConfig& cfg) {
  return static_cast<uint64_t>(cfg.band_rows) * cfg.width * cfg.sample_bytes;
}

}  // namespace

WriteSession::WriteSession(const WriteSessionConfig& cfg)
    : cfg_(ValidateConfig(cfg)),
      slot_bytes_(ComputeSlotBytes(cfg_)),
      shm_(ShmSegment::create(GenerateShmName(), slot_bytes_ * cfg_.slots)) {
  for (uint32_t i = 0; i < cfg_.slots; ++i) {
    free_slots_.push_back(i);
  }
  slot_busy_.assign(cfg_.slots, false);

  // Spawn the worker. `in_pipe_`/`out_pipe_` were default-constructed as
  // real OS pipes by the time this body runs (declaration order after
  // `shm_`); `os_spawn`'s `parent_ends` closes our own retained ends in the
  // child so stdin EOF propagates correctly (see mpp_os.h's doc comment --
  // this is the exact mechanism test_host_gone.cpp guards).
  try {
    pid_ = os_spawn(cfg_.worker_exe, in_pipe_.read_end, out_pipe_.write_end,
                     {in_pipe_.write_end, out_pipe_.read_end});
  } catch (const std::exception& e) {
    throw HostError("failed to spawn mpp-ipc-worker (\"" + cfg_.worker_exe + "\"): " + e.what());
  }
  spawned_ = true;
  in_pipe_.close_read();
  out_pipe_.close_write();

  try {
    InitMessage init;
    init.protocol_version = kProtocolVersion;
    init.worker_version = kExpectedWorkerVersion;
    init.output_path = cfg_.output_path;
    init.width = cfg_.width;
    init.height = cfg_.height;
    init.channels = cfg_.channels;
    init.bit_depth = cfg_.bit_depth;
    init.shm_name = shm_.name();
    init.slot_bytes = slot_bytes_;
    init.slots = cfg_.slots;
    WriteFramed(encode_init(init));
  } catch (...) {
    // The worker is alive but has not necessarily consumed/validated Init
    // yet; killing it here (rather than leaving it to the destructor, which
    // never runs for a constructor that exits via exception) is what keeps
    // a failed WriteSession construction from leaking a process.
    KillAndReapBestEffort();
    throw;
  }
}

WriteSession::~WriteSession() {
  if (!finished_) {
    KillAndReapBestEffort();
  }
  // in_pipe_/out_pipe_ (Pipe) and shm_ (ShmSegment) release themselves via
  // their own destructors, which run automatically after this body returns.
}

void WriteSession::PushBand(uint32_t channel, uint64_t start_row, uint32_t rows,
                             const float* samples) {
  if (finished_) {
    throw HostError(
        "PushBand called after the session already reached a terminal state (Finish/Cancel/error)");
  }
  if (channel >= cfg_.channels) {
    throw HostError("PushBand channel " + std::to_string(channel) +
                     " is out of range (channels=" + std::to_string(cfg_.channels) + ")");
  }
  if (rows > cfg_.band_rows) {
    throw HostError("PushBand rows (" + std::to_string(rows) +
                     ") exceeds the configured band_rows (" + std::to_string(cfg_.band_rows) +
                     "); shm slots are sized for band_rows and a larger band would overrun the "
                     "next slot");
  }
  if (start_row > cfg_.height || rows > cfg_.height - start_row) {
    throw HostError("PushBand rows [" + std::to_string(start_row) + ", " +
                     std::to_string(start_row) + "+" + std::to_string(rows) +
                     ") exceed the configured height (" + std::to_string(cfg_.height) + ")");
  }

  WaitForFreeSlot();
  uint32_t slot = free_slots_.front();
  free_slots_.pop_front();
  slot_busy_[slot] = true;

  const size_t byte_len = static_cast<size_t>(rows) * cfg_.width * sizeof(float);
  std::memcpy(shm_.base() + static_cast<uint64_t>(slot) * slot_bytes_, samples, byte_len);

  BandHeader hdr;
  hdr.channel = channel;
  hdr.start_row = start_row;
  hdr.rows = rows;
  hdr.slot = slot;
  WriteFramed(encode_band(hdr));
  ++outstanding_;
}

uint64_t WriteSession::Finish(const std::function<void(ProgressInfo)>& cb) {
  if (finished_) {
    throw HostError("Finish called after the session already reached a terminal state");
  }

  // Drain every outstanding (unacked) band before sending Finish. This is
  // not merely tidiness: it keeps the post-Finish pump loop below simple --
  // it only ever has to expect Progress/Done/Error, never a BandAck --
  // because every Band frame we sent has already been acked by the time
  // Finish goes out (docs/pi-protocol.md §7.2/§7.7).
  while (outstanding_ > 0) {
    WorkerFrame f = ReadFrameOrThrow();
    switch (f.tag) {
      case WorkerTag::BandAck:
        FreeSlot(f.band_ack_slot);
        break;
      case WorkerTag::Progress:
        if (cb) cb(ProgressInfo{f.progress.rows_done, f.progress.rows_total});
        break;
      case WorkerTag::Error:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker reported error: " + f.error.message);
      case WorkerTag::Done:
      default:
        finished_ = true;
        ReapBlocking();
        throw HostError(
            "worker sent an unexpected frame while draining outstanding band acks before Finish");
    }
  }

  WriteFramed(encode_finish());

  for (;;) {
    WorkerFrame f = ReadFrameOrThrow();
    switch (f.tag) {
      case WorkerTag::Progress:
        if (cb) cb(ProgressInfo{f.progress.rows_done, f.progress.rows_total});
        break;
      case WorkerTag::Done: {
        finished_ = true;
        int code = ReapBlocking();
        if (code != 0) {
          throw HostError("worker exited with code " + std::to_string(code) +
                           " after sending Done (docs/pi-protocol.md §4: the exit code is the "
                           "authoritative completion signal)");
        }
        return f.done.bytes_written;
      }
      case WorkerTag::Error:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker reported error: " + f.error.message);
      case WorkerTag::BandAck:
      default:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker sent an unexpected frame after Finish");
    }
  }
}

void WriteSession::Cancel() {
  if (finished_) {
    throw HostError("Cancel called after the session already reached a terminal state");
  }
  // Cancel always ends the session one way or another from here on (every
  // path below either throws directly or falls into ReadFrameOrThrow's own
  // throwing paths), so mark it terminal up front rather than after each
  // branch below.
  finished_ = true;

  WriteFramed(encode_cancel());

  for (;;) {
    WorkerFrame f = ReadFrameOrThrow();
    switch (f.tag) {
      case WorkerTag::BandAck:
      case WorkerTag::Progress:
        continue;  // tolerate, keep waiting for the terminal reply
      case WorkerTag::Error:
        ReapBlocking();
        throw HostError("worker reported error: " + f.error.message);
      case WorkerTag::Done:
      default:
        ReapBlocking();
        throw HostError("worker sent Done instead of Error after Cancel");
    }
  }
}

void WriteSession::WaitForFreeSlot() {
  while (free_slots_.empty()) {
    WorkerFrame f = ReadFrameOrThrow();
    switch (f.tag) {
      case WorkerTag::BandAck:
        FreeSlot(f.band_ack_slot);
        break;
      case WorkerTag::Progress:
        break;  // no callback is registered during band streaming; tolerate and drop
      case WorkerTag::Error:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker reported error while awaiting a free band slot: " +
                         f.error.message);
      case WorkerTag::Done:
      default:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker sent an unexpected frame while awaiting a free band slot");
    }
  }
}

void WriteSession::FreeSlot(uint32_t slot) {
  // Host memory safety first (ported from MergeMosaic's mmm_host.cpp
  // Host::run, the "Host memory safety first" guard on BandRequest/
  // OutputBand slot ids -- mmm_host.cpp:534-563; that guard's spirit was
  // not carried over on this file's first pass, a code-review finding this
  // fixes): `slot` is worker-supplied and about to gate the NEXT
  // PushBand's memcpy destination. An out-of-range id would write outside
  // the shm mapping; an in-range id that is not currently busy (never
  // handed out, or already freed by an earlier ack) would let a slot be
  // reused while a Band we already sent for it may still be logically in
  // flight. Neither is a value this function may act on unchecked -- both
  // are worker protocol violations, so this kills (not just waits for) the
  // worker: a worker that just sent a bogus ack cannot be trusted to exit
  // on its own the way a legitimate terminal frame promises.
  if (slot >= cfg_.slots || !slot_busy_[slot]) {
    const bool out_of_range = slot >= cfg_.slots;
    finished_ = true;
    KillAndReapBestEffort();
    throw HostError("worker sent BandAck for slot " + std::to_string(slot) + ", which is " +
                     (out_of_range
                          ? "out of range (slots=" + std::to_string(cfg_.slots) + ")"
                          : "not currently busy (stale or duplicate ack)"));
  }
  slot_busy_[slot] = false;
  free_slots_.push_back(slot);
  if (outstanding_ > 0) {
    --outstanding_;
  }
}

WorkerFrame WriteSession::ReadFrameOrThrow() {
  WorkerFrame f;
  bool got;
  try {
    got = read_worker_frame(out_pipe_.read_end, f);
  } catch (const std::exception& e) {
    finished_ = true;
    int code = ReapBlocking();
    throw HostError(std::string("protocol error reading worker frame: ") + e.what() +
                     " (worker exit code " + std::to_string(code) + ")");
  }
  if (!got) {
    finished_ = true;
    int code = ReapBlocking();
    throw HostError(
        "worker closed its output unexpectedly before completing the run (exit code " +
        std::to_string(code) + ")");
  }
  return f;
}

void WriteSession::WriteFramed(const std::vector<uint8_t>& framed) {
  size_t total = 0;
  while (total < framed.size()) {
    long w = os_write(in_pipe_.write_end, framed.data() + total, framed.size() - total);
    if (w < 0) {
#ifndef _WIN32
      if (errno == EINTR) continue;
#endif
      throw HostError("write to worker stdin failed (the worker may have exited)");
    }
    total += static_cast<size_t>(w);
  }
}

int WriteSession::ReapBlocking() {
  if (reaped_) return 0;
  reaped_ = true;
  return os_wait_process(pid_);
}

void WriteSession::KillAndReapBestEffort() noexcept {
  if (!spawned_ || reaped_) return;
  os_kill_process(pid_);
  reaped_ = true;
  os_wait_process(pid_);  // discard exit code; best-effort reap only
}

// ===========================================================================
// ReadSession -- the read-direction inversion of WriteSession above (see
// mpp_host.h's class doc comment): the worker fills shm slots and requests
// them via ReadBand, this class fills nothing and only acks. Every
// process-lifecycle/frame-IO helper below is a deliberate near-duplicate of
// WriteSession's own (ReadFrameOrThrow, WriteFramed, ReapBlocking,
// KillAndReapBestEffort) rather than a shared base class -- the two classes'
// constructors and terminal-state bookkeeping differ enough (ReadSession has
// no PushBand/Finish/Cancel surface at all) that sharing would cost more
// clarity than the few duplicated lines save, matching the plan's choice to
// keep WriteSession and ReadSession as separate top-level classes.

ReadSession::ReadSession(const ReadSessionConfig& cfg)
    : cfg_(ValidateReadConfig(cfg)),
      slot_bytes_(ComputeReadSlotBytes(cfg_)),
      shm_(ShmSegment::create(GenerateShmName(), slot_bytes_ * cfg_.slots)) {
  try {
    pid_ = os_spawn(cfg_.worker_exe, in_pipe_.read_end, out_pipe_.write_end,
                     {in_pipe_.write_end, out_pipe_.read_end});
  } catch (const std::exception& e) {
    throw HostError("failed to spawn mpp-ipc-worker (\"" + cfg_.worker_exe + "\"): " + e.what());
  }
  spawned_ = true;
  in_pipe_.close_read();
  out_pipe_.close_write();

  try {
    // Geometry/bit_depth fields the worker will override via ImageMeta once
    // it opens the file (see ReadSessionConfig's doc comment) -- only
    // output_path/shm_name/slot_bytes/slots/mode are load-bearing for a read
    // session's Init.
    InitMessage init;
    init.protocol_version = kProtocolVersion;
    init.worker_version = kExpectedWorkerVersion;
    init.output_path = cfg_.input_path;
    init.width = cfg_.width;
    init.height = 0;
    init.channels = cfg_.channels;
    init.bit_depth = "";
    init.shm_name = shm_.name();
    init.slot_bytes = slot_bytes_;
    init.slots = cfg_.slots;
    init.mode = "read";
    WriteFramed(encode_init(init));

    WorkerFrame f = ReadFrameOrThrow();
    if (f.tag == WorkerTag::Error) {
      finished_ = true;
      ReapBlocking();
      throw HostError("worker reported error: " + f.error.message);
    }
    if (f.tag != WorkerTag::ImageMeta) {
      finished_ = true;
      KillAndReapBestEffort();
      throw HostError("expected ImageMeta (tag " +
                       std::to_string(static_cast<int>(WorkerTag::ImageMeta)) +
                       ") as the first reply frame in a read session, got tag " +
                       std::to_string(static_cast<int>(f.tag)));
    }
    meta_ = f.image_meta;
  } catch (...) {
    // The worker is alive but not necessarily past this point cleanly;
    // killing it here (rather than leaving it to the destructor, which
    // never runs for a constructor that exits via exception) is what keeps
    // a failed ReadSession construction from leaking a process -- mirrors
    // WriteSession's constructor exactly.
    if (!finished_) {
      KillAndReapBestEffort();
    }
    throw;
  }
}

ReadSession::~ReadSession() {
  if (!finished_) {
    KillAndReapBestEffort();
  }
  // in_pipe_/out_pipe_ (Pipe) and shm_ (ShmSegment) release themselves via
  // their own destructors, which run automatically after this body returns.
}

void ReadSession::ReadAll(
    const std::function<void(uint32_t channel, uint64_t start_row, uint32_t rows,
                              const void* samples)>& sink) {
  if (finished_) {
    throw HostError("ReadAll called after the session already reached a terminal state");
  }

  for (;;) {
    WorkerFrame f = ReadFrameOrThrow();
    switch (f.tag) {
      case WorkerTag::ReadBand: {
        const BandHeader& hdr = f.read_band;
        if (hdr.slot >= cfg_.slots) {
          finished_ = true;
          KillAndReapBestEffort();
          throw HostError("worker sent ReadBand for slot " + std::to_string(hdr.slot) +
                           ", out of range (slots=" + std::to_string(cfg_.slots) + ")");
        }
        const uint64_t byte_len =
            static_cast<uint64_t>(hdr.rows) * meta_.width * cfg_.sample_bytes;
        if (byte_len > slot_bytes_) {
          finished_ = true;
          KillAndReapBestEffort();
          throw HostError("worker's ReadBand channel " + std::to_string(hdr.channel) +
                           " start_row " + std::to_string(hdr.start_row) + " rows " +
                           std::to_string(hdr.rows) + " needs " + std::to_string(byte_len) +
                           " bytes, exceeds slot_bytes " + std::to_string(slot_bytes_));
        }

        const uint8_t* src = shm_.base() + static_cast<uint64_t>(hdr.slot) * slot_bytes_;
        if (sink) {
          sink(hdr.channel, hdr.start_row, hdr.rows, src);
        }

        WriteFramed(encode_read_band_ack(hdr.slot));
        break;
      }
      case WorkerTag::Progress:
        break;  // no read-mode Progress is emitted today, but tolerate/drop per §7.7's spirit
      case WorkerTag::Done: {
        finished_ = true;
        int code = ReapBlocking();
        if (code != 0) {
          throw HostError("worker exited with code " + std::to_string(code) +
                           " after sending Done (the exit code is the authoritative completion "
                           "signal, docs/pi-protocol.md §4)");
        }
        return;
      }
      case WorkerTag::Error:
        finished_ = true;
        ReapBlocking();
        throw HostError("worker reported error: " + f.error.message);
      case WorkerTag::BandAck:
      case WorkerTag::Invalid:
      default:
        finished_ = true;
        KillAndReapBestEffort();
        throw HostError("worker sent an unexpected frame (tag " +
                         std::to_string(static_cast<int>(f.tag)) +
                         ") during ReadAll of a read session");
    }
  }
}

ImageMeta ReadSession::Probe(const std::string& worker_exe, const std::string& input_path) {
  if (worker_exe.empty()) {
    throw HostError("ReadSession::Probe: worker_exe is empty");
  }
  if (!std::filesystem::exists(worker_exe)) {
    throw HostError("mpp-ipc-worker executable not found at \"" + worker_exe +
                     "\" -- check the worker path / installation next to the module");
  }

  Pipe in_pipe;
  Pipe out_pipe;
  os_pid pid = os_invalid_pid;
  try {
    pid = os_spawn(worker_exe, in_pipe.read_end, out_pipe.write_end,
                   {in_pipe.write_end, out_pipe.read_end}, {"--probe", input_path});
  } catch (const std::exception& e) {
    throw HostError("failed to spawn mpp-ipc-worker --probe (\"" + worker_exe + "\"): " +
                     e.what());
  }
  in_pipe.close_read();
  out_pipe.close_write();
  // The `--probe` CLI entry never touches stdin (main.rs checks argv before
  // the stdin handshake); close our write end immediately so it doesn't
  // matter either way.
  in_pipe.close_write();

  std::string output;
  bool reaped = false;
  int exit_code = 0;
  try {
    uint8_t buf[4096];
    for (;;) {
      long n = os_read(out_pipe.read_end, buf, sizeof(buf));
      if (n < 0) {
        throw HostError("mpp-ipc-worker --probe: reading stdout failed");
      }
      if (n == 0) break;  // EOF
      output.append(reinterpret_cast<char*>(buf), static_cast<size_t>(n));
    }
    out_pipe.close_read();
    reaped = true;
    exit_code = os_wait_process(pid);
  } catch (...) {
    if (!reaped) {
      os_kill_process(pid);
      os_wait_process(pid);
    }
    throw;
  }

  if (exit_code != 0) {
    throw HostError("mpp-ipc-worker --probe \"" + input_path + "\" exited with code " +
                     std::to_string(exit_code));
  }
  try {
    return ParseImageMeta(output);
  } catch (const std::exception& e) {
    throw HostError(std::string("mpp-ipc-worker --probe: failed to parse ImageMeta JSON output (") +
                     e.what() + "): " + output);
  }
}

WorkerFrame ReadSession::ReadFrameOrThrow() {
  WorkerFrame f;
  bool got;
  try {
    got = read_worker_frame(out_pipe_.read_end, f);
  } catch (const std::exception& e) {
    finished_ = true;
    int code = ReapBlocking();
    throw HostError(std::string("protocol error reading worker frame: ") + e.what() +
                     " (worker exit code " + std::to_string(code) + ")");
  }
  if (!got) {
    finished_ = true;
    int code = ReapBlocking();
    throw HostError(
        "worker closed its output unexpectedly before completing the run (exit code " +
        std::to_string(code) + ")");
  }
  return f;
}

void ReadSession::WriteFramed(const std::vector<uint8_t>& framed) {
  size_t total = 0;
  while (total < framed.size()) {
    long w = os_write(in_pipe_.write_end, framed.data() + total, framed.size() - total);
    if (w < 0) {
#ifndef _WIN32
      if (errno == EINTR) continue;
#endif
      throw HostError("write to worker stdin failed (the worker may have exited)");
    }
    total += static_cast<size_t>(w);
  }
}

int ReadSession::ReapBlocking() {
  if (reaped_) return 0;
  reaped_ = true;
  return os_wait_process(pid_);
}

void ReadSession::KillAndReapBestEffort() noexcept {
  if (!spawned_ || reaped_) return;
  os_kill_process(pid_);
  reaped_ = true;
  os_wait_process(pid_);  // discard exit code; best-effort reap only
}

}  // namespace mpp
