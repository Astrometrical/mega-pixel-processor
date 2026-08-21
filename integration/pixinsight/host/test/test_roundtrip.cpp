// Golden test 1 (Task 2): drives the REAL mpp-ipc-worker binary end to end
// over the actual host<->worker wire protocol (docs/pi-protocol.md) --
// no mocks, no PCL. Creates a shm segment, spawns the worker, sends Init
// for a 64x48x1 f32 image, pushes every band in order with a deterministic
// pixel formula, sends Finish, and expects Done. Then re-opens the written
// file and checks the BigTIFF magic bytes + a nonzero size (full pixel
// verification against the Rust decoder is Task 3's fuller golden suite;
// here magic+Done+ack-count is the bar).
//
// The worker binary is located via the MPP_WORKER_EXE environment variable
// (see CMakeLists.txt for how CTest sets it). Build + run manually with:
//   cargo build -p mpp-ipc-worker
//   MPP_WORKER_EXE=$PWD/target/debug/mpp-ipc-worker ctest --test-dir <build>
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdio>
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

// Fails the test with a message on stderr and a nonzero exit -- CTest reads
// the process exit code, so every failure path in this file ends up here
// (directly, or via an uncaught exception from main's try/catch).
[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_roundtrip: FAILED: " << what << "\n";
  std::exit(1);
}

// Writes a fully-framed byte buffer (as returned by mpp::encode_init /
// encode_band / encode_finish -- tag+len+payload already assembled) to `h`
// in one shot, handling short writes. Used instead of re-deriving the
// tag/payload split and calling mpp::write_frame a second time.
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

// Deterministic pixel value for (channel, row, col) of the test image --
// arbitrary but reproducible; Task 2 only checks the worker completed and
// produced a well-formed file, not exact pixel values (that's Task 3).
float pixel_value(uint32_t channel, uint64_t row, uint64_t width, uint64_t col) {
  uint64_t n = (channel + 1) * 1009 + row * width + col;
  return static_cast<float>(n % 1009) / 1009.0f;
}

// Waits for the next worker frame, tolerating any number of interleaved
// Progress frames (docs/pi-protocol.md §7.7: "Progress may arrive at any
// point before the terminal frame"). Throws on Error (worker-reported
// failure) or a closed pipe (EOF) before the frame we're waiting for.
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
      continue;  // tolerate, keep waiting
    }
    if (frame.tag != expected) {
      fail("expected worker frame tag " + std::to_string(static_cast<int>(expected)) +
           ", got " + std::to_string(static_cast<int>(frame.tag)));
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

    // ---- geometry ---------------------------------------------------
    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint64_t kChannels = 1;
    constexpr uint32_t kBandRows = 8;  // 48 / 8 = 6 bands, evenly, per channel
    static_assert(kHeight % kBandRows == 0, "kBandRows must evenly divide kHeight for this test");
    constexpr uint32_t kSlots = 2;
    const uint64_t slot_bytes = static_cast<uint64_t>(kBandRows) * kWidth * sizeof(float);

    const std::filesystem::path output_path =
        std::filesystem::temp_directory_path() / "mpp_test_roundtrip_output.tf8";
    std::error_code ec;
    std::filesystem::remove(output_path, ec);  // best-effort: clean any stale file

#ifdef _WIN32
    std::string shm_name = "/mpp-rt-" + std::to_string(GetCurrentProcessId());
#else
    std::string shm_name = "/mpp-rt-" + std::to_string(static_cast<long>(getpid()));
#endif

    // ---- create the shm segment (host owns creation/sizing/unlink) --
    mpp::ShmSegment shm = mpp::ShmSegment::create(shm_name, slot_bytes * kSlots);

    // ---- spawn the real worker ---------------------------------------
    mpp::Pipe in_pipe;   // host writes -> worker stdin
    mpp::Pipe out_pipe;  // worker stdout -> host reads
    // in_pipe.write_end and out_pipe.read_end are OUR ends -- the child must
    // never inherit them (see os_spawn's doc comment: an inherited copy of
    // in_pipe.write_end in the child would mean the child's own stdin pipe
    // never reaches EOF, even after we close our copy).
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

    uint64_t bytes_written = 0;
    try {
      // ---- Init -------------------------------------------------------
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

      // ---- push every band, in order, one slot in flight at a time ---
      uint32_t bands_sent = 0;
      for (uint64_t start_row = 0; start_row < kHeight; start_row += kBandRows) {
        uint32_t rows = kBandRows;
        uint32_t slot = bands_sent % kSlots;

        // Fill the slot with this band's deterministic pixel data (channel 0
        // -- kChannels == 1 -- native-endian f32, row-major, docs/pi-protocol.md §6).
        float* dst = reinterpret_cast<float*>(shm.base() + slot_bytes * slot);
        for (uint32_t r = 0; r < rows; ++r) {
          for (uint64_t c = 0; c < kWidth; ++c) {
            dst[r * kWidth + c] = pixel_value(0, start_row + r, kWidth, c);
          }
        }

        mpp::BandHeader hdr;
        hdr.channel = 0;
        hdr.start_row = start_row;
        hdr.rows = rows;
        hdr.slot = slot;
        write_all_bytes(in_pipe.write_end, mpp::encode_band(hdr));
        ++bands_sent;

        mpp::WorkerFrame ack = wait_for(out_pipe.read_end, mpp::WorkerTag::BandAck);
        if (ack.band_ack_slot != slot) {
          fail("BandAck named slot " + std::to_string(ack.band_ack_slot) + ", expected " +
               std::to_string(slot));
        }
      }
      if (bands_sent != kHeight / kBandRows) {
        fail("internal test error: sent " + std::to_string(bands_sent) + " bands, expected " +
             std::to_string(kHeight / kBandRows));
      }

      // ---- Finish -> Done ----------------------------------------------
      write_all_bytes(in_pipe.write_end, mpp::encode_finish());

      mpp::WorkerFrame done = wait_for(out_pipe.read_end, mpp::WorkerTag::Done);
      bytes_written = done.done.bytes_written;
      if (bytes_written == 0) {
        fail("Done reported bytes_written == 0");
      }

      in_pipe.close_write();
      out_pipe.close_read();
      int exit_code = reap();
      if (exit_code != 0) {
        fail("worker exited with code " + std::to_string(exit_code) + " after Done");
      }
    } catch (...) {
      if (!reaped) {
        mpp::os_kill_process(pid);
        reap();
      }
      throw;
    }

    // ---- verify the output file ----------------------------------------
    if (!std::filesystem::exists(output_path)) {
      fail("output file " + output_path.string() + " does not exist after Done");
    }
    auto file_size = std::filesystem::file_size(output_path);
    if (file_size == 0) {
      fail("output file " + output_path.string() + " is empty");
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

    std::cout << "test_roundtrip: OK -- " << (kHeight / kBandRows) << " bands acked, Done("
              << bytes_written << " bytes reported), output file " << file_size
              << " bytes, BigTIFF magic verified\n";
    std::filesystem::remove(output_path, ec);  // best-effort cleanup
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
