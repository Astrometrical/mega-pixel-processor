// test_native_band.cpp -- protocol v3's native band path through
// mpp::WriteSession: a "u16" band_format session whose shm slots carry
// u16 samples, not the unit-range f32 every band crossed shm as before v3.
//
// What it pins:
//   1. PushBandBytes measures its buffer in the session's negotiated sample
//      width. The geometry below is chosen so a regression to
//      `sizeof(float)` is not merely inaccurate but fatal: slot_bytes *
//      slots is exactly 4096 bytes (one page), so a 2x-oversized memcpy for
//      the second band writes past the end of the shm mapping and the test
//      dies on a signal rather than passing with plausible-looking output.
//   2. The pixels survive verbatim -- u16 values above 2^11 that a round
//      trip through unit-range f32 (v1/v2's only shm sample type) could not
//      have carried exactly, read back through ReadSession and compared
//      sample for sample.
//   3. PushBand (the f32 wrapper) REFUSES a session that negotiated
//      anything but "f32", rather than copying a float buffer into a u16
//      slot.
//   4. The host-side guards that must fail before a process is ever
//      spawned: the band_format/bit_depth coupling rule, an unknown
//      band_format, and an ICC profile over the 16 MiB decoded bound.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_native_band: FAILED: " << what << "\n";
  std::exit(1);
}

// Deterministic u16 ramp with values well above 2^11, so an accidental trip
// through a unit-range f32 (or any 8-bit narrowing) shows up as a mismatch.
uint16_t pixel_value(uint64_t row, uint64_t width, uint64_t col) {
  return static_cast<uint16_t>(((row * width + col) * 977u + 12345u) & 0xffffu);
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

    // ---- 0. SampleBytes is the sample-width table both the slot sizing and
    // ---- PushBandBytes measure in -------------------------------------
    struct { const char* fmt; size_t bytes; } kWidths[] = {
        {"f32", 4}, {"f64", 8}, {"u32", 4}, {"u16", 2}, {"u8", 1}};
    for (const auto& w : kWidths) {
      size_t got = mpp::WriteSession::SampleBytes(w.fmt);
      if (got != w.bytes) {
        fail(std::string("SampleBytes(\"") + w.fmt + "\") == " + std::to_string(got) +
             ", expected " + std::to_string(w.bytes));
      }
    }
    bool threw = false;
    try {
      mpp::WriteSession::SampleBytes("u4");
    } catch (const mpp::HostError&) {
      threw = true;
    }
    if (!threw) {
      fail("SampleBytes(\"u4\") did not throw HostError for an unknown band_format");
    }

    // ---- geometry: slot_bytes * slots is exactly one 4096-byte page ----
    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint32_t kBandRows = 16;  // 16 * 64 * 2 = 2048 bytes per slot
    constexpr uint32_t kSlots = 2;      // 2048 * 2 = 4096 = one page exactly
    static_assert(kHeight % kBandRows == 0, "kBandRows must evenly divide kHeight");

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_native_band.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);

    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = worker_exe;
    cfg.output_path = output_path.string();
    cfg.width = kWidth;
    cfg.height = kHeight;
    cfg.channels = 1;
    cfg.bit_depth = "u16";
    cfg.band_format = "u16";  // native: no conversion anywhere on the path
    cfg.band_rows = kBandRows;
    cfg.slots = kSlots;
    cfg.description = "test_native_band";

    // ---- 1-3. write the image natively --------------------------------
    {
      mpp::WriteSession session(cfg);

      // The f32 wrapper must refuse this session outright.
      std::vector<float> f32_buf(static_cast<size_t>(kBandRows) * kWidth, 0.5f);
      bool refused = false;
      std::string refusal;
      try {
        session.PushBand(0, 0, kBandRows, f32_buf.data());
      } catch (const mpp::HostError& e) {
        refused = true;
        refusal = e.what();
      }
      if (!refused) {
        fail("PushBand accepted an f32 buffer on a band_format=\"u16\" session");
      }
      if (refusal.find("band_format") == std::string::npos) {
        fail("PushBand's refusal did not name band_format: " + refusal);
      }

      std::vector<uint16_t> buf(static_cast<size_t>(kBandRows) * kWidth);
      for (uint64_t start_row = 0; start_row < kHeight; start_row += kBandRows) {
        for (uint32_t r = 0; r < kBandRows; ++r) {
          for (uint64_t x = 0; x < kWidth; ++x) {
            buf[r * kWidth + x] = pixel_value(start_row + r, kWidth, x);
          }
        }
        session.PushBandBytes(0, start_row, kBandRows, buf.data());
      }
      uint64_t bytes_written = session.Finish(nullptr);
      if (bytes_written == 0) {
        fail("Finish reported bytes_written == 0 for a native u16 session");
      }
    }
    if (!std::filesystem::exists(output_path)) {
      fail("output file " + output_path.string() + " does not exist after Finish");
    }

    // ---- 2. read it back and compare every sample ----------------------
    mpp::ImageMeta probed = mpp::ReadSession::Probe(worker_exe, output_path.string());
    if (probed.bit_depth != "u16" || probed.width != kWidth || probed.height != kHeight ||
        probed.channels != 1) {
      fail("Probe() mismatch: width=" + std::to_string(probed.width) + " height=" +
           std::to_string(probed.height) + " channels=" + std::to_string(probed.channels) +
           " bit_depth=" + probed.bit_depth + " (expected 64x48x1 u16)");
    }

    mpp::ReadSessionConfig rcfg;
    rcfg.worker_exe = worker_exe;
    rcfg.input_path = output_path.string();
    rcfg.band_rows = static_cast<uint32_t>(kHeight);  // generous: whole image in one band
    rcfg.slots = 2;
    rcfg.sample_bytes = 2;  // u16
    rcfg.width = probed.width;
    rcfg.channels = probed.channels;

    std::vector<uint16_t> plane(static_cast<size_t>(kWidth) * kHeight, 0);
    std::vector<bool> covered(static_cast<size_t>(kHeight), false);
    {
      mpp::ReadSession rs(rcfg);
      rs.ReadAll([&](uint32_t channel, uint64_t start_row, uint32_t rows, const void* samples) {
        if (channel != 0) {
          fail("ReadAll sink: channel " + std::to_string(channel) + " out of range");
        }
        if (start_row + rows > kHeight) {
          fail("ReadAll sink: band exceeds height");
        }
        const uint16_t* src = static_cast<const uint16_t*>(samples);
        for (uint32_t r = 0; r < rows; ++r) {
          covered[start_row + r] = true;
          for (uint64_t x = 0; x < kWidth; ++x) {
            plane[(start_row + r) * kWidth + x] = src[r * kWidth + x];
          }
        }
      });
    }
    for (uint64_t y = 0; y < kHeight; ++y) {
      if (!covered[y]) {
        fail("row " + std::to_string(y) + " was never delivered by ReadAll");
      }
      for (uint64_t x = 0; x < kWidth; ++x) {
        uint16_t want = pixel_value(y, kWidth, x);
        uint16_t got = plane[y * kWidth + x];
        if (got != want) {
          fail("sample mismatch at row " + std::to_string(y) + " col " + std::to_string(x) +
               ": got " + std::to_string(got) + ", want " + std::to_string(want));
        }
      }
    }
    std::filesystem::remove(output_path, ec);

    // ---- 4. host-side guards, all before any process is spawned --------
    auto expect_rejected = [&](mpp::WriteSessionConfig bad, const std::string& needle,
                                const std::string& what) {
      bool rejected = false;
      std::string message;
      try {
        mpp::WriteSession session(bad);
      } catch (const mpp::HostError& e) {
        rejected = true;
        message = e.what();
      }
      if (!rejected) {
        fail(what + ": WriteSession construction was accepted, expected HostError");
      }
      if (message.find(needle) == std::string::npos) {
        fail(what + ": HostError did not mention \"" + needle + "\": " + message);
      }
    };

    {
      // The coupling rule: a native band format must equal bit_depth.
      mpp::WriteSessionConfig bad = cfg;
      bad.bit_depth = "f32";
      bad.band_format = "u16";
      expect_rejected(bad, "band_format", "coupling rule");
    }
    {
      // ...but "f32" bands may target any output bit_depth, which is what
      // keeps every pre-v3 caller working. (Constructed and immediately
      // destroyed; the destructor kills and reaps the worker.)
      mpp::WriteSessionConfig ok = cfg;
      ok.bit_depth = "u16";
      ok.band_format = "f32";
      ok.output_path = (std::filesystem::temp_directory_path() /
                        "mpp_test_native_band_f32_to_u16.tf8").string();
      mpp::WriteSession session(ok);
      std::filesystem::remove(ok.output_path, ec);
    }
    {
      mpp::WriteSessionConfig bad = cfg;
      bad.band_format = "u4";
      expect_rejected(bad, "band_format", "unknown band_format");
    }
    {
      // 22369624 base64 characters decode to 16 MiB + 2 bytes: one group
      // over the worker's own cap, so the host must refuse it here rather
      // than spawn a worker and create a segment to be told the same thing.
      mpp::WriteSessionConfig bad = cfg;
      bad.icc_profile_base64 = std::string(22369624, 'A');
      expect_rejected(bad, "16 MiB", "oversized ICC profile");
    }

    std::cout << "test_native_band: OK -- native u16 bands cross shm and round-trip verbatim; "
                 "PushBand refuses a non-f32 session; host-side v3 guards reject before spawn\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
