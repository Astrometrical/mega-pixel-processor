// Golden test (Task 4): mpp::ReadSession -- the read direction's inversion
// of WriteSession -- driving the REAL mpp-ipc-worker binary end to end over
// the actual host<->worker wire protocol (protocol v2: Init.mode="read",
// ImageMeta, ReadBand/ReadBandAck). No mocks, no PCL.
//
// The host library cannot encode TIFF itself, and tests must not depend on
// test_data/, so this generates its own fixture by driving a WRITE session
// first (mpp::WriteSession, the same driver test_split_bands.cpp exercises,
// including its deterministic pixel_value ramp): write a small f32 RGB
// BigTIFF with a known ramp, then open it with mpp::ReadSession and assert
// both Meta() (the worker's authoritative geometry) and the reassembled
// per-channel planes equal the ramp exactly. This is a full write->read
// round trip against the real worker, with no PixInsight involved anywhere.
//
// Also covers, standalone (no process spawn needed):
//   - mpp::kProtocolVersion == 3 (protocol v3, static_assert)
//   - the new tag constants (kTagImageMeta/kTagReadBand/kTagReadBandAck)
//   - mpp::ParseImageMeta on a literal JSON payload
// and, using the real worker: mpp::ReadSession::Probe's standalone
// `--probe <path>` geometry query.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <algorithm>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_read: FAILED: " << what << "\n";
  std::exit(1);
}

// Same deterministic pixel-value ramp as test_split_bands.cpp (Task 3's
// golden suite) -- arbitrary but reproducible, and distinct enough per
// (channel, row, col) that a reassembly bug (wrong channel, wrong row,
// transposed axes) shows up as a mismatch rather than accidentally passing.
float pixel_value(uint32_t channel, uint64_t row, uint64_t width, uint64_t col) {
  uint64_t n = (channel + 1) * 1009 + row * width + col;
  return static_cast<float>(n % 1009) / 1009.0f;
}

// Protocol-layer checks that need no process at all: the wire constants and
// the ImageMeta JSON parser, exercised directly against a literal payload
// shaped like the worker's own IMAGE_META frame (docs: crates/mpp-ipc/src/
// protocol.rs's ImageMeta, mirrored field-for-field by mpp::ImageMeta).
void check_protocol_constants_and_parse() {
  static_assert(mpp::kProtocolVersion == 3,
                "kProtocolVersion must be 3 (native band sample types)");
  static_assert(mpp::kTagImageMeta == 5, "kTagImageMeta must mirror mpp_ipc::protocol::tag::IMAGE_META");
  static_assert(mpp::kTagReadBand == 6, "kTagReadBand must mirror mpp_ipc::protocol::tag::READ_BAND");
  static_assert(mpp::kTagReadBandAck == 132,
                "kTagReadBandAck must mirror mpp_ipc::protocol::tag::READ_BAND_ACK");

  const std::string json =
      R"({"width":9888,"height":6568,"channels":3,"alpha":0,"color_space":"rgb","bit_depth":"f32"})";
  mpp::ImageMeta meta = mpp::ParseImageMeta(json);
  if (meta.width != 9888 || meta.height != 6568 || meta.channels != 3 || meta.alpha != 0 ||
      meta.color_space != "rgb" || meta.bit_depth != "f32") {
    fail("ParseImageMeta produced unexpected fields from a literal ImageMeta JSON payload");
  }
}

}  // namespace

int main() {
  try {
    check_protocol_constants_and_parse();

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

    // ---- Step A: write a small f32 RGB BigTIFF ramp via WriteSession -----
    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint64_t kChannels = 3;
    constexpr uint32_t kWriteBandRows = 8;  // 48 / 8 = 6 bands per channel
    static_assert(kHeight % kWriteBandRows == 0,
                  "kWriteBandRows must evenly divide kHeight for this test");
    constexpr uint32_t kWriteSlots = 2;

    const std::filesystem::path input_path =
        std::filesystem::temp_directory_path() / "mpp_test_read_input.big.tiff";
    std::error_code ec;
    std::filesystem::remove(input_path, ec);  // best-effort: clean any stale file

    {
      mpp::WriteSessionConfig wcfg;
      wcfg.worker_exe = worker_exe;
      wcfg.output_path = input_path.string();
      wcfg.width = kWidth;
      wcfg.height = kHeight;
      wcfg.channels = kChannels;
      wcfg.bit_depth = "f32";
      wcfg.band_rows = kWriteBandRows;
      wcfg.slots = kWriteSlots;

      mpp::WriteSession session(wcfg);
      std::vector<float> buf(static_cast<size_t>(kWriteBandRows) * kWidth);
      for (uint32_t c = 0; c < kChannels; ++c) {
        for (uint64_t start_row = 0; start_row < kHeight; start_row += kWriteBandRows) {
          for (uint32_t r = 0; r < kWriteBandRows; ++r) {
            for (uint64_t x = 0; x < kWidth; ++x) {
              buf[r * kWidth + x] = pixel_value(c, start_row + r, kWidth, x);
            }
          }
          session.PushBand(c, start_row, kWriteBandRows, buf.data());
        }
      }
      uint64_t bytes_written = session.Finish(nullptr);
      if (bytes_written == 0) {
        fail("fixture write: Finish reported bytes_written == 0");
      }
    }
    if (!std::filesystem::exists(input_path)) {
      fail("fixture write: output file " + input_path.string() + " does not exist after Finish");
    }

    // ---- Step B: Probe() the geometry standalone --------------------------
    mpp::ImageMeta probed = mpp::ReadSession::Probe(worker_exe, input_path.string());
    if (probed.width != kWidth || probed.height != kHeight || probed.channels != kChannels ||
        probed.alpha != 0 || probed.color_space != "rgb" || probed.bit_depth != "f32") {
      fail("Probe() geometry mismatch: width=" + std::to_string(probed.width) +
           " height=" + std::to_string(probed.height) +
           " channels=" + std::to_string(probed.channels) +
           " alpha=" + std::to_string(probed.alpha) + " color_space=" + probed.color_space +
           " bit_depth=" + probed.bit_depth);
    }

    // ---- Step C: ReadSession the fixture back and reassemble -------------
    constexpr uint32_t kReadBandRows = 48;  // >= kHeight: generous, avoids oversize-band errors
    constexpr uint32_t kReadSlots = 2;
    constexpr uint64_t kSampleBytes = 4;  // f32

    mpp::ReadSessionConfig rcfg;
    rcfg.worker_exe = worker_exe;
    rcfg.input_path = input_path.string();
    rcfg.band_rows = kReadBandRows;
    rcfg.slots = kReadSlots;
    rcfg.sample_bytes = kSampleBytes;
    rcfg.width = probed.width;
    rcfg.channels = probed.channels;

    mpp::ReadSession rs(rcfg);
    const mpp::ImageMeta& meta = rs.Meta();
    if (meta.width != kWidth || meta.height != kHeight || meta.channels != kChannels ||
        meta.alpha != 0 || meta.color_space != "rgb" || meta.bit_depth != "f32") {
      fail("ReadSession::Meta() geometry mismatch: width=" + std::to_string(meta.width) +
           " height=" + std::to_string(meta.height) +
           " channels=" + std::to_string(meta.channels) +
           " alpha=" + std::to_string(meta.alpha) + " color_space=" + meta.color_space +
           " bit_depth=" + meta.bit_depth);
    }

    std::vector<std::vector<float>> planes(
        kChannels, std::vector<float>(static_cast<size_t>(kWidth) * kHeight, -1.0f));
    uint64_t bands_seen = 0;
    rs.ReadAll([&](uint32_t channel, uint64_t start_row, uint32_t rows, const void* samples) {
      if (channel >= kChannels) {
        fail("ReadAll sink: channel " + std::to_string(channel) + " out of range");
      }
      if (start_row + rows > kHeight) {
        fail("ReadAll sink: band [" + std::to_string(start_row) + ", " +
             std::to_string(start_row + rows) + ") exceeds height " + std::to_string(kHeight));
      }
      const float* src = reinterpret_cast<const float*>(samples);
      float* dst = planes[channel].data() + start_row * kWidth;
      std::copy(src, src + static_cast<size_t>(rows) * kWidth, dst);
      ++bands_seen;
    });
    if (bands_seen == 0) {
      fail("ReadAll: sink was never called");
    }

    // ---- Step D: verify every pixel against the write-time ramp ----------
    for (uint32_t c = 0; c < kChannels; ++c) {
      for (uint64_t y = 0; y < kHeight; ++y) {
        for (uint64_t x = 0; x < kWidth; ++x) {
          float want = pixel_value(c, y, kWidth, x);
          float got = planes[c][y * kWidth + x];
          if (got != want) {
            fail("pixel mismatch at channel " + std::to_string(c) + " row " + std::to_string(y) +
                 " col " + std::to_string(x) + ": got " + std::to_string(got) + ", want " +
                 std::to_string(want));
          }
        }
      }
    }

    std::cout << "test_read: OK -- " << bands_seen << " READ_BAND frames consumed, "
              << (kWidth * kHeight * kChannels) << " pixels verified against the write-time ramp\n";
    std::filesystem::remove(input_path, ec);  // best-effort cleanup
    return 0;
  } catch (const mpp::HostError& e) {
    fail(std::string("HostError: ") + e.what());
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
