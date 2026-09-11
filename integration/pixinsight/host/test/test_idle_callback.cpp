// Golden test: WriteSession's idle callback fires while the worker pipe is
// silent. The PixInsight module registers one to pump the GUI event queue
// (so the Console repaints and Windows does not grey the window as "not
// responding") during the long quiet stretches of a big save -- staging
// the last bands, or the whole encode phase between Finish and Done. The
// callback must fire repeatedly while nothing arrives, and the blocking
// read must still complete normally once a frame does.
//
// Drives fake_worker_slow_error (a fixture that goes quiet for ~350 ms
// after Finish, then replies Error), whose path arrives as a compile
// definition, like test_bogus_ack.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdlib>
#include <filesystem>
#include <iostream>
#include <string>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_idle_callback: FAILED: " << what << "\n";
  std::exit(1);
}

}  // namespace

int main() {
  try {
    const std::string fake = FAKE_WORKER_SLOW_ERROR_PATH;
    if (!std::filesystem::exists(fake)) {
      fail("fake worker fixture not found at " + fake);
    }
    const auto output_path =
        std::filesystem::temp_directory_path() / "mpp_test_idle_callback.tf8";

    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = fake;
    cfg.output_path = output_path.string();
    cfg.width = 4;
    cfg.height = 4;
    cfg.channels = 1;
    cfg.bit_depth = "f32";
    cfg.band_rows = 4;
    cfg.slots = 1;

    mpp::WriteSession session(cfg);
    int idle_calls = 0;
    session.SetIdleCallback([&] { ++idle_calls; });

    bool threw = false;
    try {
      session.Finish(nullptr);
    } catch (const mpp::HostError& e) {
      threw = true;
      if (std::string(e.what()).find("slow") == std::string::npos) {
        fail(std::string("expected the fake worker's Error{\"slow\"}, got: ") + e.what());
      }
    }
    if (!threw) {
      fail("Finish returned normally; the fake worker should have replied Error");
    }
    // ~350 ms of silence at WriteSession::kIdleWaitMs (100 ms) is at least
    // three ticks; require two so a slow CI box cannot flake this.
    if (idle_calls < 2) {
      fail("idle callback fired " + std::to_string(idle_calls) +
           " time(s) during a ~350 ms silent stretch; expected >= 2");
    }

    std::cout << "test_idle_callback: OK -- idle callback fired " << idle_calls
              << " times while the worker was silent, then Finish observed its Error\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("unexpected exception: ") + e.what());
  }
}
