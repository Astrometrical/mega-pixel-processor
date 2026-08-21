// Test-only fake worker (Task 3 fix, code-review finding: WriteSession's
// FreeSlot did not validate a worker-supplied BandAck slot id). This is NOT
// part of the production worker -- it exists only so test_bogus_ack.cpp can
// drive a real mpp::WriteSession against a worker that misbehaves in a way
// the real mpp-ipc-worker binary (correctly) never does.
//
// Behavior: reads exactly one frame from its stdin (the host's Init --
// contents are irrelevant here, it just needs to be consumed so the host's
// blocking Init write doesn't stall on a full pipe), then emits ONE
// BandAck naming slot id 9999 -- a value no host using this test's `slots =
// 1` config could ever have legitimately handed out -- and then
// deliberately does NOT exit: it blocks reading stdin until EOF (i.e. until
// the host closes its end, or this process is killed). That last part is
// the point: it proves WriteSession's bogus-slot guard cannot rely on the
// misbehaving worker exiting voluntarily (a real worker's Done/Error is
// always followed by an exit, per docs/pi-protocol.md §4 -- a bogus
// mid-stream BandAck carries no such promise), so the guard must kill the
// process itself rather than block on a graceful exit that will never come.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
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
    // Consume the Init frame; its contents don't matter for this test.
    uint8_t tag = 0;
    std::vector<uint8_t> payload;
    if (!mpp::read_frame(in_h, tag, payload)) {
      return 1;  // host closed before even sending Init -- nothing to do
    }

    // Emit a BandAck for slot 9999 -- bogus by construction (the test
    // configures `slots = 1`, so no legitimate slot id is >= 1, let alone
    // 9999).
    const uint32_t kBogusSlot = 9999;
    uint8_t ack_payload[4] = {
        static_cast<uint8_t>(kBogusSlot & 0xff),
        static_cast<uint8_t>((kBogusSlot >> 8) & 0xff),
        static_cast<uint8_t>((kBogusSlot >> 16) & 0xff),
        static_cast<uint8_t>((kBogusSlot >> 24) & 0xff),
    };
    mpp::write_frame(out_h, static_cast<uint8_t>(mpp::WorkerTag::BandAck), ack_payload,
                      sizeof(ack_payload));
  } catch (...) {
    return 1;
  }

  // Deliberately do not exit here -- see the file header comment. Block
  // reading stdin (discarding everything) until EOF or a read error, i.e.
  // until the host closes its end or this process is killed.
  uint8_t discard[256];
  for (;;) {
    long n = mpp::os_read(in_h, discard, sizeof(discard));
    if (n <= 0) break;
  }
  return 0;
}
