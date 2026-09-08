cmake_minimum_required(VERSION 3.16)
project(mpp_host CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Global Constraints (docs/superpowers/plans/2026-08-22-pi-module-shim.md):
# C++17, -Wall -Wextra zero warnings. -Werror turns any warning into a build
# failure so the gate is enforced here, not just eyeballed in review.
if(MSVC)
  # /W4 warning level, /WX = warnings-as-errors, /EHsc standard C++ exception
  # model. This static lib never links PCL, so /EHsc is correct (the future
  # module DLL matches PCL's /EHa separately, in Task 4). UTF-8 source/exec
  # charset keeps narrow string literals well-defined for the
  # FormatMessageA/widen paths in mpp_os.h/mpp_shm.cpp.
  add_compile_options(/W4 /WX /EHsc /utf-8)
  # The golden tests read MPP_WORKER_EXE via std::getenv (standard C++);
  # MSVC's C4996 CRT-deprecation ("consider _dupenv_s") would turn that
  # into a /WX build failure. The reads are start-of-main, read-only, so
  # silence the CRT deprecation rather than forking the code per platform.
  add_compile_definitions(_CRT_SECURE_NO_WARNINGS)
else()
  add_compile_options(-Wall -Wextra -Werror -fPIC)
endif()

add_library(mpp_host STATIC mpp_shm.cpp mpp_protocol.cpp mpp_host.cpp)
target_include_directories(mpp_host PUBLIC ${CMAKE_CURRENT_SOURCE_DIR} ${CMAKE_CURRENT_SOURCE_DIR}/third_party)
if(NOT WIN32 AND NOT APPLE)
  # glibc < 2.34 keeps shm_open/shm_unlink in librt rather than libc; link it
  # unconditionally off Linux/non-Apple so this still works on an older
  # glibc, matching MergeMosaic's own mmm_host CMakeLists.txt.
  target_link_libraries(mpp_host PUBLIC rt)
endif()

# ---------------------------------------------------------------------------
# CTest golden suite. No PCL anywhere in this directory -- these tests link
# only mpp_host (mpp_shm.cpp + mpp_protocol.cpp) and drive the REAL
# mpp-ipc-worker binary as a child process, so they build and run without
# PixInsight installed.
#
# The worker binary is located via the MPP_WORKER_EXE environment variable,
# which CTest/CI sets to the cargo-built debug binary. To run locally:
#
#   cargo build -p mpp-ipc-worker
#   MPP_WORKER_EXE=$PWD/target/debug/mpp-ipc-worker ctest --test-dir <build-dir> --output-on-failure
#
# (Full gate, from the repo root:)
#   cmake -S integration/pixinsight/host -B /tmp/mpp-host-build
#   cmake --build /tmp/mpp-host-build
#   cargo build -p mpp-ipc-worker
#   MPP_WORKER_EXE=$PWD/target/debug/mpp-ipc-worker \
#     ctest --test-dir /tmp/mpp-host-build --output-on-failure
#
# `build_worker` below is a convenience CMake target (NOT a test dependency,
# since CMake cannot invoke cargo automatically during `ctest`); running it
# before `ctest` keeps the default MPP_WORKER_EXE path current, but the
# `MPP_WORKER_EXE` env var is what test_roundtrip actually reads at runtime.
enable_testing()

set(MPP_CARGO_MANIFEST "${CMAKE_CURRENT_SOURCE_DIR}/../../../Cargo.toml")
add_custom_target(build_worker
  COMMAND cargo build --manifest-path ${MPP_CARGO_MANIFEST} -p mpp-ipc-worker
  COMMENT "Building mpp-ipc-worker (debug)")

add_executable(test_roundtrip test/test_roundtrip.cpp)
target_link_libraries(test_roundtrip PRIVATE mpp_host)
add_test(NAME test_roundtrip COMMAND test_roundtrip)

# Host-gone golden test: worker stdin EOF before Finish (docs/pi-protocol.md
# §7.5) must be observed and cleaned up, not hung on forever -- see
# test_host_gone.cpp's header comment for the fd-leak bug this pins.
add_executable(test_host_gone test/test_host_gone.cpp)
target_link_libraries(test_host_gone PRIVATE mpp_host)
add_test(NAME test_host_gone COMMAND test_host_gone)

# Task 3 golden suite: mpp::WriteSession (mpp_host.h/.cpp) driving the real
# worker through PushBand/Finish/Cancel, plus two raw-frame/no-worker edge
# cases (test_version_mismatch bypasses WriteSession deliberately -- see its
# header comment; test_worker_missing never spawns a process at all).
add_executable(test_split_bands test/test_split_bands.cpp)
target_link_libraries(test_split_bands PRIVATE mpp_host)
add_test(NAME test_split_bands COMMAND test_split_bands)

add_executable(test_u16 test/test_u16.cpp)
target_link_libraries(test_u16 PRIVATE mpp_host)
add_test(NAME test_u16 COMMAND test_u16)

add_executable(test_cancel test/test_cancel.cpp)
target_link_libraries(test_cancel PRIVATE mpp_host)
add_test(NAME test_cancel COMMAND test_cancel)

add_executable(test_version_mismatch test/test_version_mismatch.cpp)
target_link_libraries(test_version_mismatch PRIVATE mpp_host)
add_test(NAME test_version_mismatch COMMAND test_version_mismatch)

add_executable(test_worker_missing test/test_worker_missing.cpp)
target_link_libraries(test_worker_missing PRIVATE mpp_host)
add_test(NAME test_worker_missing COMMAND test_worker_missing)

# Task 4 golden suite: mpp::ReadSession (mpp_host.h/.cpp) -- the read
# direction's inversion of WriteSession -- driving the real worker in
# Init.mode=read. Writes its own fixture (a small f32 RGB BigTIFF ramp) via
# WriteSession first, since the host can't encode TIFF and tests must not
# depend on test_data/, then reads it back and checks Meta()/pixel equality.
add_executable(test_read test/test_read.cpp)
target_link_libraries(test_read PRIVATE mpp_host)
add_test(NAME test_read COMMAND test_read)

# Task 3 fix (code-review finding): FreeSlot's busy-set guard against a
# bogus/out-of-range worker-supplied BandAck slot id. fake_worker_bogus_ack
# is a test-only fixture (never the production worker) that speaks just
# enough of the protocol to ack a bogus slot and then refuses to exit on
# its own -- see its header comment. Its path is passed to test_bogus_ack
# as a compile definition (via the $<TARGET_FILE:...> generator expression,
# which resolves to the actual built binary path/extension per platform)
# rather than an environment variable, since -- unlike the real worker --
# this fixture is always built alongside the test that needs it.
add_executable(fake_worker_bogus_ack test/fake_worker_bogus_ack.cpp)
target_link_libraries(fake_worker_bogus_ack PRIVATE mpp_host)

add_executable(test_bogus_ack test/test_bogus_ack.cpp)
target_link_libraries(test_bogus_ack PRIVATE mpp_host)
target_compile_definitions(test_bogus_ack PRIVATE
  FAKE_WORKER_BOGUS_ACK_PATH="$<TARGET_FILE:fake_worker_bogus_ack>")
add_test(NAME test_bogus_ack COMMAND test_bogus_ack)
