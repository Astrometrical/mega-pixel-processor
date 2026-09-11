// Test-only fake worker for test_idle_callback.cpp. This is NOT part of the
// production worker -- it exists so a real mpp::WriteSession can be driven
// against a worker that goes quiet for a while, which the real
// mpp-ipc-worker does too (while it stages or encodes a multi-gigabyte
// image) but not on a timescale a unit test can afford.
//
// Behavior: consumes the host's Init frame, then reads one more frame (the
// host's Finish), sleeps for ~350 ms with the pipe silent, and finally
// replies Error{"slow"} and exits nonzero -- a legal terminal frame per
// docs/pi-protocol.md §4, so WriteSession::Finish throws HostError. The
// silent stretch is what the test measures: the session's idle callback
// must have been invoked while nothing was arriving.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <chrono>
#include <cstdint>
#include <string>
#include <thread>
#include <vector>

#include "mpp_os.h"
#include "mpp_protocol.h"

int main() {
#ifdef _WIN32
  mpp::os_handle in_h = GetStdHandle(STD_INPUT_HANDLE);
  mpp::os_handle out_h = GetStdHandle(STD_OUTPUT_HANDLE);
#else
  mpp::os_handle in_h = 0;
  mpp::os_handle out_h = 1;
#endif

  try {
    uint8_t tag = 0;
    std::vector<uint8_t> payload;
    if (!mpp::read_frame(in_h, tag, payload)) return 1;  // Init
    if (!mpp::read_frame(in_h, tag, payload)) return 1;  // Finish

    std::this_thread::sleep_for(std::chrono::milliseconds(350));

    const std::string json = "{\"message\":\"slow\"}";
    mpp::write_frame(out_h, static_cast<uint8_t>(mpp::WorkerTag::Error),
                      reinterpret_cast<const uint8_t*>(json.data()),
                      static_cast<uint32_t>(json.size()));
  } catch (...) {
    return 1;
  }
  return 2;
}
