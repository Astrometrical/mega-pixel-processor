// Golden test (Task 3): constructs mpp::WriteSession with a worker_exe path
// that does not exist. Expects a fast (no spawn attempt, no hang)
// mpp::HostError naming the missing path, per WriteSession's constructor
// contract ("Throws HostError if cfg.worker_exe does not exist ..."). This
// test does not talk to any real process -- it exercises WriteSession's
// own config validation (mpp_host.cpp's ValidateConfig), run before any
// shm segment or pipe is created.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <chrono>
#include <cstdlib>
#include <iostream>
#include <string>

#include "mpp_host.h"

namespace {

[[noreturn]] void fail(const std::string& what) {
  std::cerr << "test_worker_missing: FAILED: " << what << "\n";
  std::exit(1);
}

}  // namespace

int main() {
  try {
    mpp::WriteSessionConfig cfg;
    cfg.worker_exe = "/definitely/does/not/exist/mpp-ipc-worker-nonexistent";
    cfg.output_path = "/tmp/mpp_test_worker_missing_output.tf8";
    cfg.width = 64;
    cfg.height = 48;
    cfg.channels = 1;
    cfg.bit_depth = "f32";
    cfg.band_rows = 8;
    cfg.slots = 2;

    const auto start = std::chrono::steady_clock::now();
    bool threw = false;
    try {
      mpp::WriteSession session(cfg);
      (void)session;
    } catch (const mpp::HostError& e) {
      threw = true;
      const std::string msg = e.what();
      if (msg.find(cfg.worker_exe) == std::string::npos) {
        fail("HostError message did not name the missing worker_exe path (" + cfg.worker_exe +
             "): " + msg);
      }
    }
    const auto elapsed = std::chrono::steady_clock::now() - start;
    if (!threw) {
      fail("WriteSession constructor did not throw for a nonexistent worker_exe");
    }
    // No process is ever spawned for a config that fails validation, so this
    // must return promptly -- a generous bound (5s) catches any accidental
    // hang (e.g. a stray blocking read) without being a flaky tight timer.
    if (elapsed > std::chrono::seconds(5)) {
      fail("WriteSession constructor took too long to fail for a nonexistent worker_exe "
           "(possible hang)");
    }

    std::cout << "test_worker_missing: OK -- nonexistent worker_exe rejected promptly with an "
                 "actionable HostError\n";
    return 0;
  } catch (const std::exception& e) {
    fail(std::string("uncaught exception: ") + e.what());
  }
}
