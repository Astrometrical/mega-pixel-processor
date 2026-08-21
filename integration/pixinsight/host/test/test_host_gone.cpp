// Golden test 2 (Task 2 fix): the C++ mirror of
// crates/mpp-ipc-worker/tests/end_to_end.rs's `host_gone_cleans_up` --
// pins docs/pi-protocol.md §7.5 (worker stdin EOF before Finish == the host
// is gone) against the REAL mpp-ipc-worker binary, driven from this C++
// host lib exactly like test_roundtrip.cpp.
//
// This test exists because a review found that mpp_os.h's POSIX os_spawn
// originally left the host's own parent-retained pipe ends (in_pipe's write
// end, out_pipe's read end) inherited into the child: the child then held a
// live duplicate handle to its own stdin's write side, so closing the
// host's copy never delivered EOF to the worker -- the worker would hang
// forever waiting for Finish instead of observing EOF and cleaning up. This
// test spawns the worker, completes the handshake through one acked band,
// then closes the host's pipe ends WITHOUT ever sending Finish, and asserts
// the worker exits (nonzero) within a generous timeout and leaves no
// output/staging files behind. Before the os_spawn fix, this test would
// have hung until the timeout and then failed loudly instead of passing.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

#ifndef _WIN32
#include <cerrno>
#include <sys/wait.h>
#endif

#include "mpp_os.h"
#include "mpp_protocol.h"
#include "mpp_shm.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_host_gone: FAILED: " << what << "\n";
  std::exit(1);
}

void write_all_bytes(mpp::os_handle h, const std::vector<uint8_t>& bytes) {
  size_t total = 0;
  while (total < bytes.size()) {
    long w = mpp::os_write(h, bytes.data() + total, bytes.size() - total);
    if (w < 0) {
      fail("write to worker stdin failed");
    }
    total += static_cast<size_t>(w);
  }
}

float pixel_value(uint32_t channel, uint64_t row, uint64_t width, uint64_t col) {
  uint64_t n = (channel + 1) * 1009 + row * width + col;
  return static_cast<float>(n % 1009) / 1009.0f;
}

// Waits for the next worker frame, tolerating interleaved Progress frames
// (docs/pi-protocol.md §7.7). Throws (via fail()) on Error or EOF before the
// expected frame.
mpp::WorkerFrame wait_for(mpp::os_handle h, mpp::WorkerTag expected) {
  for (;;) {
    mpp::WorkerFrame frame;
    if (!mpp::read_worker_frame(h, frame)) {
      fail("worker closed its stdout before sending the expected frame (tag " +
           std::to_string(static_cast<int>(expected)) + ")");
    }
    if (frame.tag == mpp::WorkerTag::Error) {
      fail("worker reported Error: " + frame.error.message);
    }
    if (frame.tag == mpp::WorkerTag::Progress) {
      continue;
    }
    if (frame.tag != expected) {
      fail("expected worker frame tag " + std::to_string(static_cast<int>(expected)) +
           ", got " + std::to_string(static_cast<int>(frame.tag)));
    }
    return frame;
  }
}

// Polls for `pid` to exit within `timeout_ms`, without blocking indefinitely
// -- this is what makes the test fail loudly (instead of hanging forever)
// if the worker never observes stdin EOF (the bug this test guards
// against). Returns true and fills `exit_code` if the process exited in
// time; returns false (leaving `exit_code` unset) on timeout, in which case
// the process is still running and the caller is responsible for killing
// and reaping it.
bool wait_for_exit_with_timeout(mpp::os_pid pid, int timeout_ms, int& exit_code) {
#ifdef _WIN32
  DWORD r = WaitForSingleObject(pid, static_cast<DWORD>(timeout_ms));
  if (r != WAIT_OBJECT_0) return false;
  DWORD code = 1;
  GetExitCodeProcess(pid, &code);
  CloseHandle(pid);
  exit_code = static_cast<int>(code);
  return true;
#else
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
  for (;;) {
    int status = 0;
    pid_t r = ::waitpid(pid, &status, WNOHANG);
    if (r == pid) {
      if (WIFEXITED(status)) {
        exit_code = WEXITSTATUS(status);
      } else if (WIFSIGNALED(status)) {
        exit_code = 128 + WTERMSIG(status);
      } else {
        exit_code = 1;
      }
      return true;
    }
    if (r < 0 && errno != EINTR) {
      fail("waitpid failed while polling for worker exit");
    }
    if (std::chrono::steady_clock::now() >= deadline) {
      return false;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
  }
#endif
}

}  // namespace

int main() {
  try {
    const char* worker_exe = std::getenv("MPP_WORKER_EXE");
    if (worker_exe == nullptr || worker_exe[0] == '\0') {
      fail(
          "MPP_WORKER_EXE is not set. Build the worker and point this test at it, e.g.:\n"
          "  cargo build -p mpp-ipc-worker\n"
          "  MPP_WORKER_EXE=$PWD/target/debug/mpp-ipc-worker ctest --test-dir <build>");
    }
    if (!std::filesystem::exists(worker_exe)) {
      fail(std::string("MPP_WORKER_EXE (") + worker_exe + ") does not exist");
    }

    // ---- geometry (only the first band is ever sent -- Finish never is) --
    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint64_t kChannels = 1;
    constexpr uint32_t kBandRows = 8;
    constexpr uint32_t kSlots = 2;
    const uint64_t slot_bytes = static_cast<uint64_t>(kBandRows) * kWidth * sizeof(float);

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_host_gone_output.tf8";
    const std::filesystem::path dir = output_path.parent_path();
    const std::string output_stem = output_path.filename().string();

    // Best-effort cleanup of any file left behind by a previous crashed run
    // of this same test (the output file itself, or a stray
    // "<name>.mpp-stage-<pid>" staging file next to it).
    std::error_code ec;
    for (auto& entry : std::filesystem::directory_iterator(dir, ec)) {
      if (entry.path().filename().string().rfind(output_stem, 0) == 0) {
        std::filesystem::remove(entry.path(), ec);
      }
    }

#ifdef _WIN32
    std::string shm_name = "/mpp-hg-" + std::to_string(GetCurrentProcessId());
#else
    std::string shm_name = "/mpp-hg-" + std::to_string(static_cast<long>(getpid()));
#endif

    mpp::ShmSegment shm = mpp::ShmSegment::create(shm_name, slot_bytes * kSlots);

    mpp::Pipe in_pipe;   // host writes -> worker stdin
    mpp::Pipe out_pipe;  // worker stdout -> host reads
    mpp::os_pid pid = mpp::os_spawn(worker_exe, in_pipe.read_end, out_pipe.write_end,
                                     {in_pipe.write_end, out_pipe.read_end});
    in_pipe.close_read();
    out_pipe.close_write();

    bool reaped = false;
    auto kill_and_reap = [&]() {
      if (reaped) return;
      mpp::os_kill_process(pid);
      mpp::os_wait_process(pid);
      reaped = true;
    };

    try {
      // ---- Init ---------------------------------------------------------
      mpp::InitMessage init;
      init.protocol_version = mpp::kProtocolVersion;
      init.worker_version = mpp::kExpectedWorkerVersion;
      init.output_path = output_path.string();
      init.width = kWidth;
      init.height = kHeight;
      init.channels = kChannels;
      init.bit_depth = "f32";
      init.shm_name = shm_name;
      init.slot_bytes = slot_bytes;
      init.slots = kSlots;
      write_all_bytes(in_pipe.write_end, mpp::encode_init(init));

      // ---- exactly one band, acked -- then we go silent ------------------
      float* dst = reinterpret_cast<float*>(shm.base());
      for (uint32_t r = 0; r < kBandRows; ++r) {
        for (uint64_t c = 0; c < kWidth; ++c) {
          dst[r * kWidth + c] = pixel_value(0, r, kWidth, c);
        }
      }
      mpp::BandHeader hdr;
      hdr.channel = 0;
      hdr.start_row = 0;
      hdr.rows = kBandRows;
      hdr.slot = 0;
      write_all_bytes(in_pipe.write_end, mpp::encode_band(hdr));

      mpp::WorkerFrame ack = wait_for(out_pipe.read_end, mpp::WorkerTag::BandAck);
      if (ack.band_ack_slot != 0) {
        fail("BandAck named slot " + std::to_string(ack.band_ack_slot) + ", expected 0");
      }

      // ---- simulate "host is gone": close our stdin write end WITHOUT
      // ---- ever sending Finish (docs/pi-protocol.md §7.5). If mpp_os.h's
      // ---- os_spawn leaked in_pipe.write_end into the child (the bug this
      // ---- test guards against), the worker's stdin never reaches EOF and
      // ---- it hangs here instead of cleaning up and exiting.
      in_pipe.close_write();
      out_pipe.close_read();

      int exit_code = 0;
      bool exited = wait_for_exit_with_timeout(pid, 10000, exit_code);
      if (!exited) {
        kill_and_reap();  // still running; kill it and reap the zombie
        fail(
            "worker did not exit within 10s of stdin EOF before Finish "
            "(docs/pi-protocol.md §7.5 host-gone) -- likely a leaked pipe "
            "fd keeping the worker's stdin open in the child (see os_spawn's "
            "parent_ends parameter in mpp_os.h)");
      }
      reaped = true;  // wait_for_exit_with_timeout's own waitpid already reaped it
      if (exit_code == 0) {
        fail("worker exited 0 after stdin EOF before Finish; expected nonzero per "
             "docs/pi-protocol.md §7.5");
      }
    } catch (...) {
      kill_and_reap();
      throw;
    }

    // ---- no frame is written on this path (§7.5), and no output/staging
    // ---- file should remain: the worker's cleanup() removes both the
    // ---- partial output file and the ".mpp-stage-<pid>" staging file.
    for (auto& entry : std::filesystem::directory_iterator(dir, ec)) {
      const std::string name = entry.path().filename().string();
      if (name.rfind(output_stem, 0) == 0) {
        fail("leftover file after host-gone cleanup: " + entry.path().string());
      }
    }

    std::cout << "test_host_gone: OK -- worker exited nonzero after stdin EOF before "
                 "Finish, no output/staging files left behind\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
