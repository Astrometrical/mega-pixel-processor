// test_v2_init_compat.cpp -- a protocol *v2* Init payload (none of the
// fifteen v3 fields present at all) still drives a complete write session
// against today's v3 worker, and produces the same file a v2 host would
// have got: strips, no compression, f32 bands.
//
// Why this test exists: before protocol v3, `mpp::encode_init` emitted only
// the v1/v2 fields, so every existing golden test *incidentally* proved the
// worker's serde defaults worked. Now that encode_init emits all fifteen v3
// fields unconditionally (which it must -- the host has real options to
// express), nothing in the suite would exercise the omitted-field path any
// more. So this test hand-builds the v2 JSON, bypassing encode_init
// entirely, and speaks the raw wire the way test_roundtrip.cpp/
// test_version_mismatch.cpp do. If a v3 field ever loses its
// `#[serde(default)]`, or a default changes meaning, this goes red.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

#include "mpp_os.h"
#include "mpp_protocol.h"
#include "mpp_shm.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_v2_init_compat: FAILED: " << what << "\n";
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

// Frames a raw payload the way mpp::encode_init does (tag + u32 LE length +
// payload) without going through encode_init, which is the whole point here.
std::vector<uint8_t> frame_of(uint8_t tag, const std::string& payload) {
  std::vector<uint8_t> out;
  out.reserve(5 + payload.size());
  out.push_back(tag);
  const uint32_t len = static_cast<uint32_t>(payload.size());
  for (int i = 0; i < 4; ++i) {
    out.push_back(static_cast<uint8_t>((len >> (8 * i)) & 0xff));
  }
  out.insert(out.end(), payload.begin(), payload.end());
  return out;
}

float pixel_value(uint64_t row, uint64_t width, uint64_t col) {
  uint64_t n = row * width + col;
  return static_cast<float>(n % 1009) / 1009.0f;
}

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
    if (frame.tag == mpp::WorkerTag::Progress) continue;
    if (frame.tag != expected) {
      fail("expected worker frame tag " + std::to_string(static_cast<int>(expected)) + ", got " +
           std::to_string(static_cast<int>(frame.tag)));
    }
    return frame;
  }
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
    constexpr uint32_t kBandRows = 8;
    constexpr uint32_t kSlots = 2;
    const uint64_t slot_bytes = static_cast<uint64_t>(kBandRows) * kWidth * sizeof(float);

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_v2_init_compat.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);

#ifdef _WIN32
    std::string shm_name = "/mpp-v2c-" + std::to_string(GetCurrentProcessId());
#else
    std::string shm_name = "/mpp-v2c-" + std::to_string(static_cast<long>(getpid()));
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
      // ---- a v2-shaped Init: the ten v1 fields plus `mode`, and NOT one
      // ---- of the fifteen v3 fields. protocol_version is 3 because the
      // ---- worker demands exact equality (docs/pi-protocol.md §7.1) --
      // ---- what is under test is the *field set*, not the version number.
      const std::string v2_json = std::string("{") +
          "\"protocol_version\":" + std::to_string(mpp::kProtocolVersion) + "," +
          "\"worker_version\":\"" + mpp::kExpectedWorkerVersion + "\"," +
          "\"output_path\":\"" + output_path.generic_string() + "\"," +
          "\"width\":" + std::to_string(kWidth) + "," +
          "\"height\":" + std::to_string(kHeight) + "," +
          "\"channels\":1," +
          "\"bit_depth\":\"f32\"," +
          "\"shm_name\":\"" + shm_name + "\"," +
          "\"slot_bytes\":" + std::to_string(slot_bytes) + "," +
          "\"slots\":" + std::to_string(kSlots) + "," +
          "\"mode\":\"write\"}";
      write_all_bytes(in_pipe.write_end, frame_of(static_cast<uint8_t>(mpp::HostTag::Init),
                                                   v2_json));

      // ---- push every band as unit-range f32 (the v1/v2 shm sample type,
      // ---- which is what an omitted band_format must still mean) --------
      uint32_t bands_sent = 0;
      for (uint64_t start_row = 0; start_row < kHeight; start_row += kBandRows) {
        uint32_t slot = bands_sent % kSlots;
        float* dst = reinterpret_cast<float*>(shm.base() + slot_bytes * slot);
        for (uint32_t r = 0; r < kBandRows; ++r) {
          for (uint64_t c = 0; c < kWidth; ++c) {
            dst[r * kWidth + c] = pixel_value(start_row + r, kWidth, c);
          }
        }
        mpp::BandHeader hdr;
        hdr.channel = 0;
        hdr.start_row = start_row;
        hdr.rows = kBandRows;
        hdr.slot = slot;
        write_all_bytes(in_pipe.write_end, mpp::encode_band(hdr));
        ++bands_sent;

        mpp::WorkerFrame ack = wait_for(out_pipe.read_end, mpp::WorkerTag::BandAck);
        if (ack.band_ack_slot != slot) {
          fail("BandAck named slot " + std::to_string(ack.band_ack_slot) + ", expected " +
               std::to_string(slot));
        }
      }

      write_all_bytes(in_pipe.write_end, mpp::encode_finish());
      mpp::WorkerFrame done = wait_for(out_pipe.read_end, mpp::WorkerTag::Done);
      if (done.done.bytes_written == 0) {
        fail("Done reported bytes_written == 0 for a v2-shaped Init");
      }

      in_pipe.close_write();
      out_pipe.close_read();
      int exit_code = reap();
      if (exit_code != 0) {
        fail("worker exited with code " + std::to_string(exit_code) +
             " after Done on a v2-shaped Init");
      }
    } catch (...) {
      if (!reaped) {
        mpp::os_kill_process(pid);
        reap();
      }
      throw;
    }

    if (!std::filesystem::exists(output_path) || std::filesystem::file_size(output_path) == 0) {
      fail("no (or empty) output file after a v2-shaped Init produced Done");
    }

    // The defaults a v2 payload leans on are not just "accepted" -- they
    // must still mean the v1/v2 behaviour. `container` defaulting to "auto"
    // with a .tf8 path is what makes this a BigTIFF; a default of "classic"
    // would have produced 0x2A here instead.
    std::ifstream in(output_path, std::ios::binary);
    if (!in) {
      fail("could not reopen output file " + output_path.string());
    }
    uint8_t magic[4] = {0, 0, 0, 0};
    in.read(reinterpret_cast<char*>(magic), sizeof(magic));
    if (in.gcount() != static_cast<std::streamsize>(sizeof(magic))) {
      fail("output file is shorter than the BigTIFF magic");
    }
    if (magic[0] != 'I' || magic[1] != 'I' || magic[2] != 0x2B || magic[3] != 0x00) {
      fail("output file does not start with little-endian BigTIFF magic (II, 0x2B 0x00)");
    }

    std::filesystem::remove(output_path, ec);
    std::cout << "test_v2_init_compat: OK -- an Init with none of the fifteen v3 fields still "
                 "completes a write session on the v3 worker\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
