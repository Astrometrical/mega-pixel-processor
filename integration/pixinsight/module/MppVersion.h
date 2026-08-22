// MppVersion.h -- single source for the module's human-readable version.
// Mpp.cpp derives its PCL_MODULE_VERSION components from the same numbers;
// keep both in sync when bumping. MPP_MODULE_VERSION_STRING must also stay
// in exact sync with the workspace Cargo.toml version and the host
// library's kExpectedWorkerVersion (mpp_protocol.h) -- the module/worker
// version handshake compares these strings for equality (enforced by
// crates/mpp-ipc-worker/tests/version_sync.rs).

#ifndef __MppVersion_h
#define __MppVersion_h

#define MPP_VERSION_MAJOR     0
#define MPP_VERSION_MINOR     1
#define MPP_VERSION_REVISION  0
#define MPP_VERSION_BUILD     1

#define MPP_MODULE_VERSION_STRING    "0.1.0"

#endif   // __MppVersion_h
