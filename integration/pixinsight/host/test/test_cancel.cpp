// Golden test (Task 3): drives mpp::WriteSession against the REAL
// mpp-ipc-worker, pushes one band, then calls Cancel() before Finish() --
// docs/pi-protocol.md §7.4. Expects WriteSession::Cancel() to throw
// mpp::HostError with a message containing "cancelled" (the worker's
// best-effort-cleanup reply, Error{"cancelled"}), and expects no output
// file (nor any stray "<name>.mpp-stage-*" staging file) to be left behind.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_cancel: FAILED: " << what << "\n";
  std::exit(1);
}

float pixel_value(uint32_t channel, uint64_t row, uint64_t width, uint64_t col) {
  uint64_t n = (channel + 1) * 1009 + row * width + col;
  return static_cast<float>(n % 1009) / 1009.0f;
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

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_cancel_output.tf8";
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

    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = worker_exe;
    cfg.output_path = output_path.string();
    cfg.width = kWidth;
    cfg.height = kHeight;
    cfg.channels = kChannels;
    cfg.bit_depth = "f32";
    cfg.band_rows = kBandRows;
    cfg.slots = kSlots;

    mpp::WriteSession session(cfg);

    std::vector<float> buf(static_cast<size_t>(kBandRows) * kWidth);
    for (uint32_t r = 0; r < kBandRows; ++r) {
      for (uint64_t x = 0; x < kWidth; ++x) {
        buf[r * kWidth + x] = pixel_value(0, r, kWidth, x);
      }
    }
    session.PushBand(0, 0, kBandRows, buf.data());

    bool threw = false;
    try {
      session.Cancel();
    } catch (const mpp::HostError& e) {
      threw = true;
      const std::string msg = e.what();
      if (msg.find("cancelled") == std::string::npos) {
        fail("HostError from Cancel() did not mention \"cancelled\": " + msg);
      }
    }
    if (!threw) {
      fail("WriteSession::Cancel() did not throw mpp::HostError");
    }

    // ---- no output/staging file should remain ---------------------------
    for (auto& entry : std::filesystem::directory_iterator(dir, ec)) {
      const std::string name = entry.path().filename().string();
      if (name.rfind(output_stem, 0) == 0) {
        fail("leftover file after a cancelled run: " + entry.path().string());
      }
    }

    std::cout << "test_cancel: OK -- Cancel() before Finish() threw HostError(\"cancelled\"), "
                 "no output/staging file left behind\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
