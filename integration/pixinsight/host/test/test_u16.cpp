// Golden test (Task 3): drives mpp::WriteSession with bit_depth "u16"
// against the REAL mpp-ipc-worker to pin that the C++ host's WriteSession
// works cleanly for a non-f32 output sample format (input bands over shm
// are always native-endian f32 regardless of the output bit_depth, per
// docs/pi-protocol.md §5 -- WriteSession itself never branches on
// bit_depth, but this exercises the worker's u16 conversion path end to end
// through the real driver, not raw frames).
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>
#include <vector>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_u16: FAILED: " << what << "\n";
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
    static_assert(kHeight % kBandRows == 0, "kBandRows must evenly divide kHeight for this test");
    constexpr uint32_t kSlots = 2;

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_u16_output.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);

    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = worker_exe;
    cfg.output_path = output_path.string();
    cfg.width = kWidth;
    cfg.height = kHeight;
    cfg.channels = kChannels;
    cfg.bit_depth = "u16";
    cfg.band_rows = kBandRows;
    cfg.slots = kSlots;

    mpp::WriteSession session(cfg);

    std::vector<float> buf(static_cast<size_t>(kBandRows) * kWidth);
    for (uint64_t start_row = 0; start_row < kHeight; start_row += kBandRows) {
      for (uint32_t r = 0; r < kBandRows; ++r) {
        for (uint64_t x = 0; x < kWidth; ++x) {
          buf[r * kWidth + x] = pixel_value(0, start_row + r, kWidth, x);
        }
      }
      session.PushBand(0, start_row, kBandRows, buf.data());
    }

    uint64_t bytes_written = session.Finish(nullptr);
    if (bytes_written == 0) {
      fail("Finish reported bytes_written == 0");
    }

    if (!std::filesystem::exists(output_path)) {
      fail("output file " + output_path.string() + " does not exist after Finish");
    }
    auto file_size = std::filesystem::file_size(output_path);
    if (file_size != bytes_written) {
      fail("Finish's bytes_written (" + std::to_string(bytes_written) +
           ") does not match the actual output file size (" + std::to_string(file_size) + ")");
    }

    std::ifstream in(output_path, std::ios::binary);
    if (!in) {
      fail("could not reopen output file " + output_path.string());
    }
    uint8_t magic[4] = {0, 0, 0, 0};
    in.read(reinterpret_cast<char*>(magic), sizeof(magic));
    if (in.gcount() != static_cast<std::streamsize>(sizeof(magic))) {
      fail("output file is shorter than the BigTIFF magic");
    }
    const uint8_t kExpectedMagic[4] = {0x49, 0x49, 0x2B, 0x00};
    for (int i = 0; i < 4; ++i) {
      if (magic[i] != kExpectedMagic[i]) {
        fail("output file magic bytes mismatch at offset " + std::to_string(i));
      }
    }

    std::cout << "test_u16: OK -- bit_depth=\"u16\" run completed, Done(" << bytes_written
              << " bytes), output file " << file_size << " bytes, BigTIFF magic verified\n";
    std::filesystem::remove(output_path, ec);
    return 0;
  } catch (const mpp::HostError& e) {
    fail(std::string("HostError: ") + e.what());
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
