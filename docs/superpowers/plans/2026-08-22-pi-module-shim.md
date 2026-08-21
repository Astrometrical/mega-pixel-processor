# PixInsight BigTIFF Module (Phase 2, Plan B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The C++ half of the phase-2 PixInsight integration: a **clean-room, write-only** PCL file-format module `BigTIFF-pxm.so` (extensions `.btf`, `.tf8`) that streams pixel bands over the plan-A shm channel to `mpp-ipc-worker`, plus a PCL-free C++ host static library with a CTest golden suite that proves interop against the real worker with **no PixInsight involved**, the version-sync guard, the PCL license compliance set, the clean-room provenance record, and a CI workflow. In-PixInsight loading is the human's manual smoke (documented, not automated).

**Architecture:** Mirrors MMM's proven layering exactly (`~/dev/MergeMosaic/integration/pixinsight/` — our code, read freely): `integration/pixinsight/host/` is a PCL-free static lib (`mpp_os` / `mpp_shm` / `mpp_protocol` ported near-verbatim from MMM's `mmm_*`, plus a new `mpp_host` WriteSession driver for our simpler host-push protocol from `docs/pi-protocol.md`); `integration/pixinsight/module/` is the only PCL-touching code — a `MetaModule` + `MetaFileFormat` + `FileFormatImplementation` shim that converts PixInsight `WriteSamples` calls to f32 bands and pushes them through the host lib; the worker is found beside the module binary via `dladdr` with an install-advice error when missing (MMM's `ResolveWorkerPath` pattern).

## CLEAN-ROOM RULES (binding on every task; violations are Critical findings)

- **NEVER read, open, quote, or disassemble:** `~/dev/pixinsight-test/PCL/src/modules/**` (the TIFF module lives there — the single most important prohibition), `~/dev/pixinsight-test/src/**` (Sandbox-derived scaffold, treated as tainted), `/opt/PixInsight/bin/*-pxm.so` or any other compiled module, and any PCL module implementation source anywhere else on disk or online (including the PixInsight GitLab `src/modules` tree).
- **Allowed sources, exhaustively:** `/opt/PixInsight/include/pcl/*.h` (public API headers — the primary reference), PCL license/legal files, official PixInsight developer documentation *pages* that document the public API (not module source listings), the public TIFF 6.0 + BigTIFF specifications and libtiff documentation, the entire MergeMosaic repo, and the mpp repo itself.
- Every implementer/report must list the sources actually consulted. The provenance README (Task 5) records the discipline permanently.
- Rationale (docs/DESIGN.md "Licensing constraints"): PCL License 2.0.1 added AI-use restrictions; the founding decision is that this module is designed only from public specs and the public PCL API — the same link-against-the-API posture as MMM's mmm-pxm.

## Global Constraints

- C++: C++17, `-Wall -Wextra` **zero warnings** (MMM's gate); host lib includes **no PCL header** (`mpp_host.h` rule, same as MMM's `mmm_host.h:9-11`); module `.so` must show **no undefined `mpp::` symbols** (`nm -D -u` gate) and link against `$(PCLLIBDIR)/libPCL-pxi.a` with `PCLINCDIR ?= /opt/PixInsight/include`, `PCLLIBDIR ?= $(HOME)/.local/pcl-build/lib` (both exist on this machine; PCL 2.10.4 / PI 1.9.4).
- Rust gates unchanged: `cargo test`, fmt, clippy(-D warnings), doc — all green at every commit (the version-sync test joins the suite).
- The wire contract is `docs/pi-protocol.md` — C++ constants mirror it exactly; any needed contract change is a controller escalation, not a local edit.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure

```
integration/pixinsight/
  PCL_FILEFORMAT_API.md            # clean-room API reference w/ license scoping  (Task 1)
  README.md                        # provenance + build + install + manual smoke  (Task 5)
  host/
    CMakeLists.txt                 # static lib + CTest golden suite              (Task 2, 3)
    mpp_os.h                       # ported from mmm_os.h                         (Task 2)
    mpp_shm.h / mpp_shm.cpp        # ported from mmm_shm.*                        (Task 2)
    mpp_protocol.h / .cpp          # NEW: our v1 frames (mirrors pi-protocol.md)  (Task 2)
    mpp_host.h / mpp_host.cpp      # NEW: WriteSession driver                     (Task 3)
    third_party/json.hpp           # vendored nlohmann (copy from MMM)            (Task 2)
    test/*.cpp                     # golden tests driving the real worker         (Task 2, 3)
  module/
    makefile-x64, Makefile         # modeled on MMM's                             (Task 4)
    Mpp.cpp                        # MetaModule + InstallPixInsightModule          (Task 4)
    BigTiffFormat.h / .cpp         # MetaFileFormat subclass                      (Task 4)
    BigTiffInstance.h / .cpp       # FileFormatImplementation subclass            (Task 4)
    MppVersion.h                   # version string (synced)                      (Task 4)
  ci/
    pcl-pin.env                    # PCL SHA pin (copy MMM's values)              (Task 6)
    build-module-linux.sh          # adapted from MMM                             (Task 6)
    build-module-macos.sh, build-module-windows.ps1  # adapted, unverified here   (Task 6)
.github/workflows/module.yml       # adapted from MMM                             (Task 6)
crates/mpp-ipc-worker/tests/version_sync.rs   # parses C++ headers               (Task 4)
NOTICE, docs/DESIGN.md, README.md  # compliance + docs                            (Task 5, 7)
```

## Behavioral contract for the module (normative; Task 4 implements)

- Format identity: name `BigTIFF`, extensions `.btf` + `.tf8`, write-only (`CanReadImages() == false`, `CanWriteImages() == true`, incremental writes supported to stream `WriteSamples` bands). Description strings identify Astrometrical/mpp and point at the provenance README. No format-specific options dialog in v1; no ICC/thumbnail/property storage claims — advertise only what is true (a FITS-keyword/property sidecar story is phase-3; the WCS sidecar remains mpp's own mechanism).
- Sample-format mapping (PI image → `InitMessage.bit_depth`): PI f32 → `"f32"`; PI f64 → `"f32"` (converted, documented); PI u16 → `"u16"`; PI u8 → `"u8"`; PI u32 → `"f32"` (u32/2^32-1 mapped into [0,1] float — documented precision note). The shim normalizes every `WriteSamples` buffer to f32 in [0,1] before pushing bands (integer types divide by their max; f64 narrows).
- `WriteSamples(buffer, startRow, rowCount, channel)` calls of any size are split into ≤ band_rows chunks (band_rows chosen so one band ≈ 4 MiB, min 1 row) and pushed as `Band` frames; the host lib enforces the slot protocol (fill slot → Band → await ack for slot reuse).
- Worker resolution: `dladdr` on the module's own address → directory → `mpp-ipc-worker[.exe]` beside it; missing worker → PCL `Error` exception with install advice naming the expected path (MMM `ResolveWorkerPath` pattern, `MmmExecution.cpp:313-329`).
- Any worker `Error{message}` or nonzero exit surfaces as a PCL `Error` exception carrying the message; `Done` completes the save. Abort mid-write (PI's abort mechanism, if wired in v1) maps to `Cancel` before `Finish`, else process kill per contract §7.4 — v1 may defer abort wiring; note it in the README if so.

---

### Task 1: Clean-room PCL file-format API reference

**Files:**
- Create: `integration/pixinsight/PCL_FILEFORMAT_API.md`

An implementer-facing reference for exactly the API surface Task 4 needs, produced ONLY from `/opt/PixInsight/include/pcl/` headers (+ MMM's mmm-pxm for the MetaModule/entry-point pattern, which MMM already exercises). MMM's own `PCL_API_REFERENCE.md` explicitly excluded `MetaFileFormat` — this document fills that gap for mpp.

- [ ] **Step 1:** Read (allowed): `/opt/PixInsight/include/pcl/MetaFileFormat.h`, `FileFormatImplementation.h`, `FileFormatBase.h`, `ImageDescription.h`, `ImageInfo.h` + `ImageOptions.h` (if present), `MetaModule.h`, `Error.h`/`Exception.h`, `String.h` (signatures used), and whatever those pull in that Task 4's surface needs. Also read MMM's `integration/pixinsight/module/mmm.cpp` for the module entry-point/registration pattern and `PCL_API_REFERENCE.md` for citation style AND its known compliance gap (verbatim PCL excerpts with no license scoping — do better, see Step 2).
- [ ] **Step 2:** Write the reference: (a) a header block stating the document quotes/paraphrases declarations from the PixInsight Class Library public headers, © Pleiades Astrophoto S.L., under the PixInsight Class Library License 2.0.1, with the exact header paths + PCL version (2.10.4), and that the rest of the document is Apache-2.0; (b) the module entry-point contract (install/uninstall entry points, `MetaModule` virtuals actually needed, the API-version macro dance) with file+line citations; (c) `MetaFileFormat`: every pure-virtual that MUST be overridden and every capability query relevant to a write-only incremental format (extensions, CanRead/CanWrite/CanStore*, SupportsIncrementalWrite or equivalent — cite the real names, do not guess), with signatures and file:line; (d) `FileFormatImplementation`: the write-path lifecycle as the headers document it — construction, `Create(path, ...)`, options/image-description handoff, `CreateImage(ImageInfo/ImageOptions)`, the `WriteSamples` overload set (exact parameter lists per sample type), `Close`, error conventions; (e) an honest "unknowns" section for anything the headers leave ambiguous (call order guarantees, threading), so Task 4 codes defensively instead of assuming.
- [ ] **Step 3:** Report must list every file consulted (the clean-room provenance seed). Commit: `docs(pi): clean-room PCL file-format API reference`

---

### Task 2: Host static lib — os/shm/protocol port + first golden test

**Files:**
- Create: `integration/pixinsight/host/{mpp_os.h, mpp_shm.h, mpp_shm.cpp, mpp_protocol.h, mpp_protocol.cpp, CMakeLists.txt}`
- Create: `integration/pixinsight/host/third_party/json.hpp` (copied byte-identical from `~/dev/MergeMosaic/integration/pixinsight/host/third_party/json.hpp`)
- Create: `integration/pixinsight/host/test/test_roundtrip.cpp`

**Interfaces (namespace `mpp`):**
- `mpp_os.h`: port of `mmm_os.h` (handle/read/write/spawn seam, POSIX fd vs Win32 HANDLE) — near-verbatim rename.
- `mpp_shm.h/.cpp`: port of `mmm_shm.*` — create/open/map/unmap/unlink with the same name rules as the Rust side (leading `/`, ≤31 chars, Windows `Local\` normalization per pi-protocol.md §6).
- `mpp_protocol.h/.cpp`: NEW, mirrors `docs/pi-protocol.md` §4-§5 — `kProtocolVersion = 1`, `kExpectedWorkerVersion` (= workspace crate version string, hand-written here, guarded by Task 4's sync test), tag constants, `WriteFrame(handle, tag, payload)`, `ReadFrame(handle) -> {tag, payload}` (EOF distinguishable), `BandHeader{channel,start_row,rows,slot}` pack/unpack (20-byte LE), Init JSON construction via nlohmann (field names exactly per contract), Done/Error/Progress/BandAck parse.
- CMake: static lib `libmpp_host` + CTest. **No PCL anywhere.** The golden tests locate the worker via env `MPP_WORKER_EXE` (CI/CTest sets it to the cargo-built binary) — document in CMakeLists comments; test runner script may `cargo build -p mpp-ipc-worker` first.
- `test_roundtrip.cpp` (golden test 1): create shm, spawn the real worker, Init (64×48×1 f32), push bands in-order with a deterministic pixel formula, Finish, expect Done; then re-open the written file and verify the BigTIFF magic bytes (II 0x2B) + file size > 0 (full pixel verification is Task 3's suite via the Rust decoder comparison — here magic+Done+ack-count suffices).

- [ ] Steps: port (list divergences in report) → write protocol per contract → CMake + test → `cmake -S ... -B build && cmake --build build && ctest --test-dir build` green with the real worker → gates (`cargo test` suite must remain green; C++ builds warning-free) → commit `feat(pi-host): PCL-free host lib (os/shm/protocol) + first golden interop test`.

---

### Task 3: WriteSession driver + full golden suite

**Files:**
- Create: `integration/pixinsight/host/{mpp_host.h, mpp_host.cpp}`
- Create: `integration/pixinsight/host/test/{test_split_bands.cpp, test_u16.cpp, test_cancel.cpp, test_version_mismatch.cpp, test_worker_missing.cpp}`
- Modify: `integration/pixinsight/host/CMakeLists.txt`

**Interfaces:**

```cpp
// mpp_host.h — no PCL includes (enforced by comment + review)
namespace mpp {
struct WriteSessionConfig {
  std::string worker_exe;      // absolute path
  std::string output_path;
  uint64_t width, height, channels;
  std::string bit_depth;       // "f32" | "u16" | "u8"
  uint32_t band_rows;          // rows per band the caller will push
  uint32_t slots;              // >= 2 recommended
};
struct ProgressInfo { uint64_t rows_done, rows_total; };
class WriteSession {           // RAII: dtor kills+reaps worker if still running
 public:
  explicit WriteSession(const WriteSessionConfig&);   // creates shm, spawns, sends Init
  // Blocks until a slot is free, copies samples (rows*width f32) into it,
  // sends Band. Any-order channels/rows per the contract.
  void PushBand(uint32_t channel, uint64_t start_row, uint32_t rows, const float* samples);
  // Sends Finish, pumps Progress into cb (may be null), returns bytes_written
  // on Done; throws mpp::HostError on Error frame / unexpected exit.
  uint64_t Finish(const std::function<void(ProgressInfo)>& cb);
  void Cancel();               // pre-Finish only, per contract §7.4
};
struct HostError : std::runtime_error { using std::runtime_error::runtime_error; };
}
```

Golden tests (each vs the real worker; compare outputs where applicable against expectations computed in-test — the Rust e2e already pinned worker correctness, so these pin the C++ HOST's correctness): split_bands (PushBand called with a big logical write split into 4 MiB bands, shuffled channel order, output magic + size sanity + Done bytes_written == file size); u16 (bit_depth "u16" runs clean); cancel (Cancel pre-Finish → HostError "cancelled", no output file); version_mismatch (config hack sending wrong kProtocolVersion via a test-only knob or a raw-frame test — worker rejects, HostError names version); worker_missing (nonexistent exe path → HostError with actionable message, no hang).

- [ ] Steps: TDD-ish (tests + header first, driver second), ctest all green, C++ warning-free, Rust suite untouched-green, commit `feat(pi-host): WriteSession driver + golden interop suite`.

---

### Task 4: PCL module shim + build + version sync

**Files:**
- Create: `integration/pixinsight/module/{Mpp.cpp, BigTiffFormat.h, BigTiffFormat.cpp, BigTiffInstance.h, BigTiffInstance.cpp, MppVersion.h, makefile-x64, Makefile}`
- Create: `crates/mpp-ipc-worker/tests/version_sync.rs`

Implements the "Behavioral contract for the module" section above, using Task 1's API reference (+ the headers themselves) and MMM's `mmm.cpp`/`makefile-x64` as the pattern. The instance owns a `mpp::WriteSession`; `WriteSamples` overloads normalize to f32 and split-push; worker path via `dladdr`; all failures → PCL `Error` with the worker's message. `MppVersion.h` defines `MPP_MODULE_VERSION_STRING`; `mpp_protocol.h`'s `kExpectedWorkerVersion` and the workspace `Cargo.toml` version are the other two legs; `version_sync.rs` (modeled on MMM's `crates/mmm-ipc-worker/tests/version_sync.rs`) textually parses both C++ files and asserts all three equal — a red test is the skew alarm.

Build gates (all must pass on this machine): `make -C integration/pixinsight/module -f makefile-x64` produces `BigTIFF-pxm.so` with zero `-Wall -Wextra` warnings; `nm -D -u -C BigTIFF-pxm.so | grep -c "mpp::"` == 0 (demangled — no undefined mpp symbols means the host lib linked fully; check how MMM's `ci/build-module-linux.sh` phrases its equivalent gate and match it); the .so exports the PCL module entry points (verify with `nm -D --defined-only -C BigTIFF-pxm.so | grep -i` for the install entry named in Task 1's reference). Loading into PixInsight is NOT automated — the README (Task 5) documents the manual smoke.

- [ ] Steps: version files + sync test first (red then green), module sources, makefile, build gates, full Rust gates, commit `feat(pi-module): clean-room write-only BigTIFF format module + version sync`.

---

### Task 5: Compliance, provenance, docs

**Files:**
- Create: `integration/pixinsight/README.md`
- Modify: `NOTICE`, `docs/DESIGN.md`

- README: what the module is (write-only BigTIFF `.btf`/`.tf8`), the architecture sketch (shim → shm → worker), build instructions (deps: PCL headers at /opt/PixInsight/include, libPCL-pxi.a, cargo), install layout (module + worker side by side in PI's bin/), **manual smoke procedure** (load module in PI → save any image as `.btf` → verify in Photoshop/mpp), the **clean-room provenance statement** (designed exclusively from: public PCL headers [list], docs/pi-protocol.md, MMM's own integration code, TIFF/BigTIFF public specs; PCL module implementations never read — cite DESIGN.md's founding decision and PCL License 2.0.1's terms), and the PCL/Apache licensing scoping (module/ build products link PCL → PCL License applies to distribution of the module binary; host/, worker, and everything else Apache-2.0; nlohmann json.hpp MIT).
- NOTICE: PCL entry — read `/opt/PixInsight/etc/legal/licenses/PCL-License-2.0` (allowed) and include the license's own **required acknowledgment sentence verbatim** (the license text specifies required wording for redistribution acknowledgment — quote exactly what it requires, where it requires it); plus the nlohmann/json MIT entry; plus no-endorsement language (the module is not endorsed by or affiliated with Pleiades Astrophoto).
- DESIGN.md: phase-2 progress note + record the decisions this plan made (host-push module architecture, sample-format mapping table, no-options v1, manual-smoke verification boundary).
- [ ] Steps: write, verify NOTICE quote against the actual license file, full gates, commit `docs(pi): provenance, PCL compliance set, module README`.

---

### Task 6: CI workflow + build scripts

**Files:**
- Create: `integration/pixinsight/ci/{pcl-pin.env, build-module-linux.sh, build-module-macos.sh, build-module-windows.ps1}`
- Create: `.github/workflows/module.yml`

Adapted from MMM's (`integration/pixinsight/ci/*`, `.github/workflows/module.yml`): pcl-pin.env copies MMM's pinned `PCL_REPO_URL` + `PCL_SHA` (2.10.4/Lockhart) verbatim; three-platform matrix building PCL from the pin (cached by SHA), then the module, then `cargo test -p mpp-ipc -p mpp-ipc-worker`, then the host CTest suite (Linux/macOS; Windows per MMM's pattern), artifact staging `stage/bin/{BigTIFF-pxm.*, mpp-ipc-worker*}` with the two-upload-root trick MMM documents. **No signing, no repo publishing** (MMM's own signing is dormant pending the CPD cert; explicitly out of scope — note in the workflow header comment).

Verification on this machine: `bash integration/pixinsight/ci/build-module-linux.sh` runs end-to-end locally (PCL already built at ~/.local/pcl-build — the script must accept a prebuilt PCLLIBDIR env override exactly like MMM's does); shellcheck-level review for macOS/Windows scripts (cannot run here — say so in the report); `module.yml` is schema-checked by careful review (runs on next push).

- [ ] Steps: adapt, run the Linux script locally green, full gates, commit `ci(pi): module build workflow + PCL pin (no signing yet)`.

---

### Task 7: Closeout

- README.md (repo root): phase-2 status paragraph + pointer to integration/pixinsight/README.md.
- Final full verification: `cargo test` suite green; C++ host `ctest` green; module `.so` builds clean; version-sync green.
- Commit `docs: phase-2 plan B closeout`.

## Out of scope

- Actually loading the module inside PixInsight (manual smoke — documented in the README for the human).
- Signing (`.xsgn`) and the Astrometrical update repo (dormant in MMM too, pending CPD certification).
- Read/import support, format options dialog, abort-during-encode UI wiring (contract supports Cancel pre-Finish; if v1 defers wiring PI's abort, the README says so).
- FITS keywords / ICC / XISF-properties passthrough into the BigTIFF (phase-3 candidates).
