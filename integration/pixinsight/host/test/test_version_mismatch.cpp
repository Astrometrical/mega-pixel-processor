// Golden test (Task 3): deliberately bypasses mpp::WriteSession and speaks
// the raw wire protocol (mpp_os.h/mpp_protocol.h/mpp_shm.h directly, same
// pattern as Task 2's test_roundtrip.cpp) to send an Init with the wrong
// protocol_version to the REAL mpp-ipc-worker. WriteSession's own
// InitMessage is hardcoded to the correct mpp::kProtocolVersion (there is
// no "send a bad version" knob on the production API, deliberately -- see
// mpp_host.h's WriteSession ctor: it always stamps kProtocolVersion/
// kExpectedWorkerVersion), so this is exactly the "raw-frame test" option
// the plan calls out, rather than adding a test-only knob to the driver.
//
// Per docs/pi-protocol.md §7.1: "A version mismatch on either field
// (protocol_version or worker_version) is fatal the same way: the worker
// sends Error and exits nonzero." This test asserts the worker's Error
// message names "version", and that it exits nonzero, and that no output
// file is created.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

#include "mpp_os.h"
#include "mpp_protocol.h"
#include "mpp_shm.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_version_mismatch: FAILED: " << what << "\n";
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

std::string to_lower(std::string s) {
  std::transform(s.begin(), s.end(), s.begin(),
                  [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
  return s;
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

    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint64_t kChannels = 1;
    constexpr uint32_t kBandRows = 8;
    constexpr uint32_t kSlots = 2;
    const uint64_t slot_bytes = static_cast<uint64_t>(kBandRows) * kWidth * sizeof(float);

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_version_mismatch_output.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);

#ifdef _WIN32
    std::string shm_name = "/mpp-vm-" + std::to_string(GetCurrentProcessId());
#else
    std::string shm_name = "/mpp-vm-" + std::to_string(static_cast<long>(getpid()));
#endif

    mpp::ShmSegment shm = mpp::ShmSegment::create(shm_name, slot_bytes * kSlots);

    mpp::Pipe in_pipe;
    mpp::Pipe out_pipe;
    mpp::os_pid pid = mpp::os_spawn(worker_exe, in_pipe.read_end, out_pipe.write_end,
                                     {in_pipe.write_end, out_pipe.read_end});
    in_pipe.close_read();
    out_pipe.close_write();

    bool reaped = false;
    auto reap = [&]() -> int {
      if (reaped) return 0;
      reaped = true;
      return mpp::os_wait_process(pid);
    };

    try {
      // ---- Init, deliberately wrong protocol_version -------------------
      mpp::InitMessage init;
      init.protocol_version = mpp::kProtocolVersion + 1;  // wrong on purpose
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

      // ---- expect Error (tolerating any interleaved Progress, though none
      // ---- is expected before Init is even validated) -------------------
      mpp::WorkerFrame frame;
      std::string error_message;
      bool got_error = false;
      for (;;) {
        if (!mpp::read_worker_frame(out_pipe.read_end, frame)) {
          fail("worker closed its stdout without sending an Error frame for the version mismatch");
        }
        if (frame.tag == mpp::WorkerTag::Progress) continue;
        if (frame.tag == mpp::WorkerTag::Error) {
          got_error = true;
          error_message = frame.error.message;
          break;
        }
        fail("expected an Error frame for the version mismatch, got tag " +
             std::to_string(static_cast<int>(frame.tag)));
      }
      if (!got_error) {
        fail("internal test error: loop exited without an Error frame");
      }
      if (to_lower(error_message).find("version") == std::string::npos) {
        fail("worker's Error message did not name \"version\": " + error_message);
      }

      in_pipe.close_write();
      out_pipe.close_read();
      int exit_code = reap();
      if (exit_code == 0) {
        fail("worker exited 0 after rejecting a protocol_version mismatch; expected nonzero");
      }
    } catch (...) {
      if (!reaped) {
        mpp::os_kill_process(pid);
        reap();
      }
      throw;
    }

    if (std::filesystem::exists(output_path)) {
      fail("output file " + output_path.string() +
           " exists after a rejected Init, expected it to be absent");
    }

    std::cout << "test_version_mismatch: OK -- worker rejected protocol_version mismatch with "
                 "an Error naming \"version\" and exited nonzero\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
