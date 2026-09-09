// test_v3_init.cpp -- the v3 Init frame carries every write option, with the
// exact field names and JSON types docs/pi-protocol.md §5/§10 specify, and a
// default-constructed InitMessage encodes exactly the wire defaults a v2
// host's omitted fields deserialize to.
//
// This is a pure encoder test: it never spawns a worker, so it needs no
// MPP_WORKER_EXE. The companion live test (test_v2_init_compat.cpp) is what
// proves the *worker* still accepts a v2-shaped payload.
//
// Assertions are explicit if/throw rather than <cassert>, so the test keeps
// its teeth in a build where NDEBUG is defined (CMAKE_BUILD_TYPE=Release):
// an assert()-only test silently passes there, which is exactly the kind of
// tautology this suite must not ship.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <string>
#include <vector>

#include "mpp_protocol.h"

namespace {

int failures = 0;

// Records a failure (rather than aborting) so one run reports every field
// that drifted, not just the first -- a diff against the Rust side is far
// more useful whole.
void check(bool ok, const std::string& what) {
  if (!ok) {
    std::cerr << "test_v3_init: FAILED: " << what << "\n";
    ++failures;
  }
}

// The framed Init payload as a string: skip the 5-byte frame header (tag +
// u32 LE length) to reach the JSON.
std::string payload_of(const mpp::InitMessage& init) {
  const std::vector<uint8_t> frame = mpp::encode_init(init);
  if (frame.size() < 5) {
    std::cerr << "test_v3_init: FAILED: encode_init produced a " << frame.size()
              << "-byte frame, shorter than the 5-byte header\n";
    std::exit(1);
  }
  return std::string(frame.begin() + 5, frame.end());
}

void expect_contains(const std::string& json, const std::string& needle) {
  check(json.find(needle) != std::string::npos, "Init JSON is missing " + needle + "\n  JSON: " + json);
}

}  // namespace

int main() {
  // ---- 1. Every v3 field is emitted, with the value it was given --------
  {
    mpp::InitMessage init;
    init.output_path = "/tmp/o.tif";
    init.width = 40000;
    init.height = 30000;
    init.channels = 4;
    init.bit_depth = "u32";
    init.band_format = "u32";
    init.shm_name = "/mpp-test";
    init.slot_bytes = 480000;
    init.slots = 4;
    init.container = "big";
    init.layout = "tiles";
    init.tile_size = 512;
    init.compression = "deflate";
    init.deflate_level = 9;
    init.predictor = true;
    init.alpha = "unassociated";
    init.icc_profile = "AAEC";
    init.description = "M31";
    init.copyright = "(c) 2026";
    init.software = "mpp";
    init.x_resolution = 300.0;
    init.y_resolution = 300.0;
    init.resolution_unit = "cm";

    const std::string json = payload_of(init);

    // Version bump.
    expect_contains(json, "\"protocol_version\":3");
    // Wide integers stay integers, never doubles (docs/pi-protocol.md §5).
    expect_contains(json, "\"width\":40000");
    expect_contains(json, "\"height\":30000");
    expect_contains(json, "\"slot_bytes\":480000");
    // v1/v2 fields still present and unmoved.
    expect_contains(json, "\"output_path\":\"/tmp/o.tif\"");
    expect_contains(json, "\"channels\":4");
    expect_contains(json, "\"bit_depth\":\"u32\"");
    expect_contains(json, "\"shm_name\":\"/mpp-test\"");
    expect_contains(json, "\"slots\":4");
    expect_contains(json, "\"mode\":\"write\"");
    // Every v3 field is present with its value.
    expect_contains(json, "\"band_format\":\"u32\"");
    expect_contains(json, "\"container\":\"big\"");
    expect_contains(json, "\"layout\":\"tiles\"");
    expect_contains(json, "\"tile_size\":512");
    expect_contains(json, "\"compression\":\"deflate\"");
    expect_contains(json, "\"deflate_level\":9");
    expect_contains(json, "\"predictor\":true");
    expect_contains(json, "\"alpha\":\"unassociated\"");
    expect_contains(json, "\"icc_profile\":\"AAEC\"");
    expect_contains(json, "\"description\":\"M31\"");
    expect_contains(json, "\"copyright\":\"(c) 2026\"");
    expect_contains(json, "\"software\":\"mpp\"");
    expect_contains(json, "\"resolution_unit\":\"cm\"");
    // A deflate level must never be encoded as a JSON string: uint8_t is a
    // character type, and `j["deflate_level"] = init.deflate_level` without
    // the widening cast in encode_init is how that happens.
    check(json.find("\"deflate_level\":\"") == std::string::npos,
          "deflate_level was encoded as a JSON string, not a number\n  JSON: " + json);
  }

  // ---- 2. The defaults match the Rust serde defaults exactly ------------
  // A default-constructed InitMessage must encode the same values a v2
  // host's *omitted* v3 fields deserialize to on the worker side
  // (crates/mpp-ipc/src/protocol.rs's default_* fns). Any drift here means
  // this host silently asks for something a v2 host did not -- most
  // sharply for deflate_level, whose natural C++ "unset" value of 0 is NOT
  // the wire default of 6.
  {
    mpp::InitMessage init;
    init.output_path = "/tmp/o.tif";
    init.width = 16;
    init.height = 16;
    init.channels = 1;
    init.bit_depth = "f32";
    init.shm_name = "/mpp-test";
    init.slot_bytes = 64;
    init.slots = 2;

    const std::string json = payload_of(init);
    expect_contains(json, "\"band_format\":\"f32\"");
    expect_contains(json, "\"container\":\"auto\"");
    expect_contains(json, "\"layout\":\"strips\"");
    expect_contains(json, "\"tile_size\":256");
    expect_contains(json, "\"compression\":\"none\"");
    expect_contains(json, "\"deflate_level\":6");
    expect_contains(json, "\"predictor\":false");
    expect_contains(json, "\"alpha\":\"none\"");
    expect_contains(json, "\"icc_profile\":\"\"");
    expect_contains(json, "\"description\":\"\"");
    expect_contains(json, "\"copyright\":\"\"");
    expect_contains(json, "\"software\":\"\"");
    expect_contains(json, "\"resolution_unit\":\"inch\"");
    // 0.0 is the "unknown resolution" sentinel; nlohmann renders a double
    // zero as "0.0", and either spelling would satisfy serde's f64, so
    // accept both rather than pinning a formatting detail.
    check(json.find("\"x_resolution\":0.0") != std::string::npos ||
              json.find("\"x_resolution\":0") != std::string::npos,
          "x_resolution did not default to 0\n  JSON: " + json);
    check(json.find("\"y_resolution\":0.0") != std::string::npos ||
              json.find("\"y_resolution\":0") != std::string::npos,
          "y_resolution did not default to 0\n  JSON: " + json);
  }

  if (failures != 0) {
    std::cerr << "test_v3_init: " << failures << " check(s) failed\n";
    return 1;
  }
  std::cout << "test_v3_init: OK -- every v3 Init field encodes with the documented name, "
               "type and default\n";
  return 0;
}
