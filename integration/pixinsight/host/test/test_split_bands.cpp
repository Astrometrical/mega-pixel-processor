// Golden test (Task 3): drives mpp::WriteSession -- the real driver, not
// raw protocol frames -- against the REAL mpp-ipc-worker binary for a
// multi-channel, multi-band-per-channel write pushed through PushBand in a
// deliberately shuffled order (docs/pi-protocol.md §7.2: "bands may arrive
// in any order"). Pins the C++ HOST's correctness (band splitting, slot
// reuse, out-of-order delivery); the worker's own pixel-level correctness is
// already pinned by the Rust end-to-end suite (crates/mpp-ipc-worker/tests).
//
// Band size here (width * band_rows * 4 bytes = 1 MiB) is smaller than the
// ~4 MiB production target noted in the implementing plan -- chosen for
// CTest speed while still exercising more than one band per channel plus a
// shuffled multi-channel PushBand order; the band-size math itself
// (`width * band_rows * sizeof(float) <= slot_bytes`) is identical at any
// size.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <algorithm>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <random>
#include <string>
#include <vector>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_split_bands: FAILED: " << what << "\n";
  std::exit(1);
}

// Deterministic pixel value for (channel, row, col) -- arbitrary but
// reproducible; this test only checks structural correctness (file size,
// Done's bytes_written, magic bytes), not pixel-exact round-tripping (that
// would need a BigTIFF decoder in C++, out of scope here -- the Rust e2e
// suite already pins the worker's own encode correctness).
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

    constexpr uint64_t kWidth = 512;
    constexpr uint64_t kHeight = 1536;
    constexpr uint64_t kChannels = 3;
    constexpr uint32_t kBandRows = 512;  // 1536 / 512 = 3 bands per channel, 9 bands total
    static_assert(kHeight % kBandRows == 0, "kBandRows must evenly divide kHeight for this test");
    constexpr uint32_t kSlots = 3;

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_split_bands_output.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);  // best-effort: clean any stale file

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

    // Every (channel, start_row) band that covers the image, then shuffled
    // with a fixed seed (reproducible across runs/platforms -- not a source
    // of test flakiness) so PushBand sees an out-of-order, cross-channel
    // delivery sequence.
    struct BandSpec {
      uint32_t channel;
      uint64_t start_row;
    };
    std::vector<BandSpec> bands;
    for (uint32_t c = 0; c < kChannels; ++c) {
      for (uint64_t y = 0; y < kHeight; y += kBandRows) {
        bands.push_back({c, y});
      }
    }
    std::mt19937 rng(42);
    std::shuffle(bands.begin(), bands.end(), rng);

    std::vector<float> buf(static_cast<size_t>(kBandRows) * kWidth);
    for (const BandSpec& b : bands) {
      for (uint32_t r = 0; r < kBandRows; ++r) {
        for (uint64_t x = 0; x < kWidth; ++x) {
          buf[r * kWidth + x] = pixel_value(b.channel, b.start_row + r, kWidth, x);
        }
      }
      session.PushBand(b.channel, b.start_row, kBandRows, buf.data());
    }

    uint64_t bytes_written = session.Finish(nullptr);
    if (bytes_written == 0) {
      fail("Finish reported bytes_written == 0");
    }

    // ---- verify the output file ----------------------------------------
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
    // BigTIFF magic: "II" (little-endian byte order) + 0x2B 0x00 (BigTIFF
    // version marker), per the TIFF6/BigTIFF public spec.
    const uint8_t kExpectedMagic[4] = {0x49, 0x49, 0x2B, 0x00};
    for (int i = 0; i < 4; ++i) {
      if (magic[i] != kExpectedMagic[i]) {
        fail("output file magic bytes mismatch at offset " + std::to_string(i));
      }
    }

    std::cout << "test_split_bands: OK -- " << bands.size() << " shuffled bands across "
              << kChannels << " channels, Done(" << bytes_written << " bytes), output file "
              << file_size << " bytes, BigTIFF magic verified\n";
    std::filesystem::remove(output_path, ec);  // best-effort cleanup
    return 0;
  } catch (const mpp::HostError& e) {
    fail(std::string("HostError: ") + e.what());
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
