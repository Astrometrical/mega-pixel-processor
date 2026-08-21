#pragma once
// Named shared-memory segment, mirroring crates/mpp-ipc/src/shm.rs's
// `ShmSegment`. No PCL headers here -- only the C++ stdlib + POSIX/Win32 --
// so this compiles and tests without PixInsight installed.
//
// POSIX backs the segment with `shm_open`+`mmap`; Windows backs it with a
// pagefile-backed `CreateFileMappingW`+`MapViewOfFile`. The public interface
// is identical on both; only the private handle member differs.
//
// Divergence from MergeMosaic's mmm_shm.h (see task-2 report): this port
// drops MMM's `SlotLayout` type (input/output-slot arithmetic) and the
// `slot_floats()` accessor. mpp's wire contract (docs/pi-protocol.md §6) is
// host-push, one direction only -- the shm segment is `slots` uniform
// `slot_bytes`-sized slots the host alone fills, with
// `slot_offset(i) = slot_bytes * i` computed by the caller (mirroring how
// `crates/mpp-ipc/src/shm.rs` deliberately omits the same arithmetic from
// its own `ShmSegment` and leaves it one level up) -- so `ShmSegment` here
// only needs to create a segment and expose its raw bytes.
//
// NO PCL HEADER MAY EVER BE INCLUDED HERE.

#include <cstdint>
#include <string>

#include "mpp_os.h"

namespace mpp {

/// A named shared-memory segment mapped into this process.
///
/// The C++ host is always the **creator** in this protocol (§6): it creates
/// and sizes the segment before spawning the worker, which only attaches by
/// name (on the Rust side, via `ShmSegment::open`) and never creates,
/// resizes, or unlinks it. POSIX: `create` ftruncates + mmaps the segment
/// and unlinks the named object on destruction. Windows: `create` maps a
/// pagefile-backed `CreateFileMappingW` object; there is NO `shm_unlink`
/// analog -- the object is handle-refcounted by the kernel and vanishes
/// when the last handle (host + worker) closes, so the destructor only
/// unmaps the view and closes the mapping handle.
class ShmSegment {
 public:
  /// Create a new shared-memory segment named `name`, sized
  /// `total_bytes`, and map it into this process. Throws
  /// `std::runtime_error` on failure. `name` must satisfy the portable
  /// grammar validated by `validate_shm_name` in mpp_shm.cpp (leading `/`,
  /// no other `/`, at most 31 characters -- docs/pi-protocol.md §6);
  /// enforced on every platform, not just where it is load-bearing (macOS).
  /// Any stale segment left behind under `name` (e.g. a crashed prior run)
  /// is unlinked first so `create` never fails with "already exists".
  static ShmSegment create(const std::string& name, uint64_t total_bytes);

  ~ShmSegment();

  ShmSegment(ShmSegment&& other) noexcept;
  ShmSegment& operator=(ShmSegment&& other) noexcept;

  ShmSegment(const ShmSegment&) = delete;
  ShmSegment& operator=(const ShmSegment&) = delete;

  /// Mapped base pointer.
  uint8_t* base() const { return base_; }
  /// Size in bytes of the mapping.
  uint64_t size() const { return size_; }
  /// The name this segment was created under.
  const std::string& name() const { return name_; }

 private:
  ShmSegment() = default;

  std::string name_;
  uint8_t* base_ = nullptr;
  uint64_t size_ = 0;
  bool is_creator_ = false;
#ifdef _WIN32
  // The file-mapping object handle. On Windows there is no shm_unlink; the
  // mapping lives as long as any handle to it is open (see class docs).
  os_handle hMap_ = nullptr;
#endif
};

}  // namespace mpp
