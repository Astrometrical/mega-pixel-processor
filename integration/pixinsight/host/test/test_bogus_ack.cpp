// Golden test (Task 3 fix / code-review finding): pins WriteSession's
// busy-set guard against an out-of-range or stale/duplicate worker-supplied
// BandAck slot id (mpp_host.cpp's FreeSlot). Drives a real mpp::WriteSession
// against `fake_worker_bogus_ack` (built by this same CMakeLists.txt), a
// deliberately misbehaving fixture that acks slot 9999 -- a value no host
// using this test's `slots = 1` config could ever have handed out -- and
// then refuses to exit on its own (see its file header for why that
// matters). Fails against the pre-fix code: before the guard, FreeSlot
// pushed slot 9999 into free_slots_ unchecked and PushBand's `outstanding_`
// bookkeeping would have gone inconsistent (and, more seriously, a
// subsequent PushBand choosing slot 9999 would memcpy outside the shm
// mapping -- the actual OOB the finding named).
//
// Asserts:
//   - the second PushBand() call (the one that must block on
//     WaitForFreeSlot() -- `slots = 1`, so the first PushBand already
//     claimed the only slot) throws mpp::HostError naming the bogus slot
//     id ("9999");
//   - that happens promptly: WriteSession must kill the misbehaving worker
//     rather than wait indefinitely for a voluntary exit that will never
//     come.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

#include "mpp_host.h"

#ifndef FAKE_WORKER_BOGUS_ACK_PATH
#error "FAKE_WORKER_BOGUS_ACK_PATH must be defined by CMakeLists.txt"
#endif

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_bogus_ack: FAILED: " << what << "\n";
  std::exit(1);
}

}  // namespace

int main() {
  try {
    if (!std::filesystem::exists(FAKE_WORKER_BOGUS_ACK_PATH)) {
      fail("fake_worker_bogus_ack binary not found at " FAKE_WORKER_BOGUS_ACK_PATH);
    }

    constexpr uint64_t kWidth = 64;
    constexpr uint64_t kHeight = 48;
    constexpr uint64_t kChannels = 1;
    constexpr uint32_t kBandRows = 8;
    constexpr uint32_t kSlots = 1;  // exactly one slot: the SECOND PushBand
                                     // must block on WaitForFreeSlot(),
                                     // which is what reads the bogus ack.

    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = FAKE_WORKER_BOGUS_ACK_PATH;
    cfg.output_path =
        (std::filesystem::temp_directory_path() / "mpp_test_bogus_ack_output.tf8").string();
    cfg.width = kWidth;
    cfg.height = kHeight;
    cfg.channels = kChannels;
    cfg.bit_depth = "f32";
    cfg.band_rows = kBandRows;
    cfg.slots = kSlots;

    mpp::WriteSession session(cfg);

    std::vector<float> buf(static_cast<size_t>(kBandRows) * kWidth, 0.0f);

    // First PushBand claims the only slot; free_slots_ already has it, so
    // this returns without reading anything from the fake worker.
    session.PushBand(0, 0, kBandRows, buf.data());

    const auto start = std::chrono::steady_clock::now();
    bool threw = false;
    try {
      // Second PushBand has no free slot, so it must read frames until one
      // frees up -- which is where it observes (and must reject) the
      // fake worker's bogus BandAck(slot=9999).
      session.PushBand(0, kBandRows, kBandRows, buf.data());
    } catch (const mpp::HostError& e) {
      threw = true;
      const std::string msg = e.what();
      if (msg.find("9999") == std::string::npos) {
        fail("HostError from the bogus BandAck did not name the offending slot (9999): " + msg);
      }
    }
    const auto elapsed = std::chrono::steady_clock::now() - start;

    if (!threw) {
      fail("second PushBand() did not throw for a bogus/out-of-range BandAck slot id -- the "
           "busy-set guard did not fire");
    }
    // The fake worker never exits voluntarily; WriteSession must kill it
    // rather than block forever waiting for a graceful exit that will never
    // come. A generous bound (10s) catches a regression to a plain blocking
    // wait without being a flaky tight timer.
    if (elapsed > std::chrono::seconds(10)) {
      fail("WriteSession took too long to recover from a bogus BandAck (possible hang waiting "
           "for the misbehaving worker to exit on its own instead of killing it)");
    }

    std::cout << "test_bogus_ack: OK -- bogus BandAck slot 9999 rejected with HostError, "
                 "misbehaving worker killed promptly (no hang, no OOB)\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
