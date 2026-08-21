# PixInsight Worker IPC (Phase 2, Plan A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Rust half of the PixInsight write-only BigTIFF module: a new `mpp-ipc` crate (shared-memory transport + wire protocol + worker-side link), a streaming-input generalization of the BigTIFF writer, and the `mpp-ipc-worker` binary that receives pixel bands over shm from a host and encodes a BigTIFF — fully testable on this machine with **no PixInsight and no C++** via a Rust test host driving the real worker binary. Plan B builds the C++ host/module against the contract this plan freezes.

**Architecture:** Host-push model (the reverse of MMM's pull): PixInsight *delivers* pixels to a file writer, so the host fills a shm slot with one planar f32 band and notifies the worker; the worker copies it into an mmap'd staging file (bands may arrive in any row/channel order — PI's call order is not contractual), acks the slot, and on `Finish` streams the fully-covered staging planes through the existing strip-streaming BigTIFF encoder. Framing, handshake, versioning, slot-pool, and shm-lifecycle rules are ported from MMM's proven protocol (`~/dev/MergeMosaic/crates/mmm-core/src/ipc/`, `integration/pixinsight/PROTOCOL.md`) — our own code, explicitly reusable. The protocol itself is new and slimmer (write-only needs 4+4 tags, not 10).

**Tech Stack:** New workspace crate `crates/mpp-ipc` (deps: `nix` 0.29 [mman, fs] + `libc` on unix; `windows-sys` 0.59 [Win32_Foundation, Win32_Security, Win32_System_Memory] on windows — `Win32_Security` is REQUIRED, `CreateFileMappingW`'s signature references `SECURITY_ATTRIBUTES`; `serde`/`serde_json` for JSON frames). New binary crate `crates/mpp-ipc-worker` (deps: mpp-ipc, mpp-core, memmap2). No changes to mpp/mpp-core dependency lists beyond what's stated per task.

**Spec:** `docs/DESIGN.md` phase 2 ("write-only 'BigTIFF' format (.btf, .tf8) as a thin PCL C++ shim streaming pixel bands over the shm host/worker channel … to a Rust worker that encodes"). Reference implementation (ours, read freely): `~/dev/MergeMosaic/crates/mmm-core/src/ipc/{shm.rs,protocol.rs,client.rs,testhost.rs,mod.rs}`, `~/dev/MergeMosaic/crates/mmm-ipc-worker/`, `~/dev/MergeMosaic/integration/pixinsight/PROTOCOL.md`.

**Clean-room note:** This plan touches no PCL code at all. Nothing in it may read `~/dev/pixinsight-test/PCL/src/modules/**`, `~/dev/pixinsight-test/src/**`, or disassemble `/opt/PixInsight/bin/*-pxm.so`. (Plan B carries the PCL-facing rules.)

## Global Constraints

- Every public item in `mpp-ipc`/`mpp-core` gets a doc comment; both crates carry `#![warn(missing_docs)]` (add the attribute to `mpp-ipc`'s lib.rs; the worker is a binary — doc comments per project style but no missing_docs gate).
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc --no-deps` warning-free at every commit. Linux is the dev platform; `#[cfg(windows)]` arms are ported verbatim from MMM but compile-verified only if a cross toolchain exists — do NOT install one; carry MMM's provenance comment instead.
- Tests synthesize everything; no real images, no PixInsight.
- The protocol doc is the contract Plan B's C++ mirrors — keep `docs/pi-protocol.md` exactly in sync with the Rust constants in the same commit that changes either.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure

```
docs/pi-protocol.md                    # wire contract v1                  (Task 1)
crates/mpp-ipc/
  Cargo.toml, src/lib.rs               # crate scaffold                    (Task 1)
  src/shm.rs                           # shm segment port (unix+windows)   (Task 2)
  src/protocol.rs                      # tags, framing, messages           (Task 3)
  src/link.rs                          # worker-side HostLink (stdin/out)  (Task 4)
  src/testhost.rs                      # feature "testkit": reference host (Task 6)
crates/mpp-core/src/formats/tiff.rs    # PlanarRows trait generalization   (Task 5)
crates/mpp-core/src/formats/xisf.rs    # impl PlanarRows for XisfImage     (Task 5)
crates/mpp-ipc-worker/
  Cargo.toml
  src/staging.rs                       # mmap staging + coverage tracking  (Task 5)
  src/main.rs                          # worker loop                       (Task 6)
  tests/end_to_end.rs                  # testhost drives real binary       (Task 7)
Cargo.toml                             # workspace members + deps          (Task 1)
```

## The wire contract (v1) — normative summary

Frames on the worker's stdio pipes: `tag: u8 | payload_len: u32 LE | payload`. Length prefixes are trusted (host and worker are the same trust domain; same rule as MMM). Constants live in `mpp-ipc::protocol`:

```rust
pub const IPC_PROTOCOL_VERSION: u32 = 1;
```

Host → worker (worker stdin):
| tag | name | payload |
|---|---|---|
| 128 | Init | JSON `InitMessage` — must be the very first frame |
| 129 | Band | flat LE: `channel: u32, start_row: u64, rows: u32, slot: u32` (20 bytes) |
| 130 | Finish | empty |
| 131 | Cancel | empty |

Worker → host (worker stdout):
| tag | name | payload |
|---|---|---|
| 1 | BandAck | flat LE: `slot: u32` (4 bytes) — slot is free for reuse |
| 2 | Progress | JSON `ProgressMessage { rows_done: u64, rows_total: u64 }` |
| 3 | Done | JSON `DoneMessage { bytes_written: u64 }`, then exit 0 |
| 4 | Error | JSON `ErrorMessage { message: String }`, then nonzero exit |

```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InitMessage {
    pub protocol_version: u32,        // must equal IPC_PROTOCOL_VERSION
    pub worker_version: String,       // must equal env!("CARGO_PKG_VERSION") of the worker
    pub output_path: String,
    pub width: u64,
    pub height: u64,
    pub channels: u64,                // 1 or 3
    pub bit_depth: String,            // "f32" | "u16" | "u8"
    pub shm_name: String,
    pub slot_bytes: u64,              // uniform slot size; >= max band bytes
    pub slots: u32,                   // number of input slots, >= 1
}
```

Shm: one segment, created/sized/unlinked by the HOST only (worker never creates/resizes/unlinks; creator defensively unlinks any stale segment of the same name before creating). Layout: `slots` uniform slots, `slot_offset(i) = slot_bytes * i`. A slot filled for `Band { channel, start_row, rows, slot }` contains `rows * width` native-endian f32 samples of that channel, row-major, top-down. `slot_bytes` must be `>= rows_max * width * 4` where `rows_max` is the host's chosen band height; undersizing is a host bug (worker validates `rows * width * 4 <= slot_bytes` and errors — MMM's silent-corruption trap, made loud).

Handshake/lifecycle: Init first or `Error`; version mismatch (either field) → `Error` + nonzero exit. Bands may arrive in any order and any per-frame row count; overlapping coverage is an error. `Finish` with incomplete coverage → `Error` naming the first uncovered `(channel, row)`. `Cancel` → best-effort cleanup (staging deleted, partial output deleted), `Error { message: "cancelled" }`, nonzero exit. Worker stdin EOF before Finish = host gone: same cleanup, nonzero exit. Worker never touches the segment after sending `Done`/`Error`.

The full normative text (with the trust-domain note, byte-exact frame tables, and the staging/coverage rules) is written to `docs/pi-protocol.md` in Task 1 — everything above verbatim plus prose; Plan B's C++ `mpp_protocol.h` mirrors it.

---

### Task 1: Crate scaffold + protocol contract doc

**Files:**
- Create: `crates/mpp-ipc/Cargo.toml`, `crates/mpp-ipc/src/lib.rs`
- Create: `docs/pi-protocol.md`
- Modify: root `Cargo.toml` (workspace members + `[workspace.dependencies] mpp-ipc = { path = "crates/mpp-ipc" }`, `memmap2` is already present; add `nix`, `libc`, `windows-sys` per the arch section)

**Interfaces:**
- Produces: an empty-but-documented `mpp-ipc` crate compiling in the workspace with `#![warn(missing_docs)]`; the normative `docs/pi-protocol.md`.

- [ ] **Step 1:** Write `docs/pi-protocol.md` containing the full "wire contract (v1)" section above expanded to normative prose: overview (host-push write-only model, why: PI delivers pixels to writers), frame table (byte-exact), InitMessage JSON schema with an example, shm layout + ownership rules, handshake + lifecycle + error rules, the slot-size validation rule with the MMM-trap rationale, and a "Versioning" section (IPC_PROTOCOL_VERSION bumps on any wire change; worker_version is the crate version and is checked exactly). Cite MMM's `integration/pixinsight/PROTOCOL.md` as the pattern ancestor in one line.
- [ ] **Step 2:** Root `Cargo.toml`: add `"crates/mpp-ipc"`, `"crates/mpp-ipc-worker"` to members (the worker dir arrives in Task 5-6 — cargo errors on missing members, so add ONLY `mpp-ipc` now; the worker member is added in Task 5). Add workspace deps:

```toml
mpp-ipc = { path = "crates/mpp-ipc" }
memmap2 = "0.9"        # (already present from phase 1 — skip if so)
nix = { version = "0.29", features = ["mman", "fs"] }
libc = "0.2"
windows-sys = { version = "0.59", features = [
    "Win32_Foundation",
    # Required: CreateFileMappingW's signature references SECURITY_ATTRIBUTES
    # (Win32_Security); without it shm compilation fails (E0432). Provenance:
    # verified empirically in MMM (mmm-core/Cargo.toml comment).
    "Win32_Security",
    "Win32_System_Memory",
] }
```

- [ ] **Step 3:** `crates/mpp-ipc/Cargo.toml`: package boilerplate matching other crates (workspace version/edition/license/authors/repository), description "Mega Pixel Processor host/worker IPC — shared-memory pixel transport and wire protocol for the PixInsight module", deps: `serde.workspace`, `serde_json.workspace`, `thiserror.workspace`; `[target.'cfg(unix)'.dependencies] nix.workspace, libc.workspace`; `[target.'cfg(windows)'.dependencies] windows-sys.workspace`; `[features] testkit = []`.
- [ ] **Step 4:** `src/lib.rs`:

```rust
//! Host/worker IPC for the PixInsight integration.
//!
//! The PixInsight module (C++, plan B) is the HOST: it creates one shared
//! memory segment, spawns `mpp-ipc-worker`, and streams planar f32 pixel
//! bands through shm slots while control frames flow over the worker's
//! stdio. The wire contract is `docs/pi-protocol.md` — the Rust constants
//! here and the C++ mirror must both match it exactly.

#![warn(missing_docs)]
```

(modules are added by later tasks). Run the gates: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps` — all green.
- [ ] **Step 5:** Commit: `feat(ipc): mpp-ipc crate scaffold + wire protocol contract v1`

---

### Task 2: Shared-memory segment (`shm.rs`)

**Files:**
- Create: `crates/mpp-ipc/src/shm.rs`
- Modify: `crates/mpp-ipc/src/lib.rs` (`pub mod shm;`)

**Interfaces:**
- Produces (Tasks 6/7 and Plan B's Rust tests consume):

```rust
pub struct ShmSegment { /* opaque */ }
impl ShmSegment {
    pub fn create(name: &str, size: usize) -> Result<Self, ShmError>;  // host side; unlinks stale first
    pub fn open(name: &str, size: usize) -> Result<Self, ShmError>;    // worker side; never creates
    pub fn as_slice(&self) -> &[u8];
    pub fn as_mut_slice(&mut self) -> &mut [u8];
    pub fn name(&self) -> &str;
}
// created segments unlink on Drop; opened segments only unmap.
pub enum ShmError { /* thiserror; Create/Open/Map variants with OS error + name */ }
```

- [ ] **Step 1 (port):** Port `~/dev/MergeMosaic/crates/mmm-core/src/ipc/shm.rs` (732 lines: unix shm_open/mmap arms, `#[cfg(windows)]` CreateFileMappingW arms at ~343-557, stub for other targets, and its unit tests). Mechanical renames `mmm`→`mpp` (shm name prefixes, test temp names), error type adapted to a self-contained `ShmError` (thiserror) instead of mmm's crate Error — this crate must not depend on mpp-core. Keep the create-unlinks-stale-first behavior and the Drop semantics exactly. Keep MMM's provenance comments on the Windows arm. If mmm's file exposes extras beyond the interface block above (slot helpers etc.), port only what the interface + tests need; note omissions in the module docs.
- [ ] **Step 2:** Run the ported tests: `cargo test -p mpp-ipc shm` — green on Linux (unix arm). Full gates green.
- [ ] **Step 3:** Commit: `feat(ipc): shared-memory segment (ported from mmm-core)`

---

### Task 3: Wire protocol (`protocol.rs`)

**Files:**
- Create: `crates/mpp-ipc/src/protocol.rs`
- Modify: `crates/mpp-ipc/src/lib.rs`

**Interfaces:**
- Produces:

```rust
pub const IPC_PROTOCOL_VERSION: u32 = 1;
pub mod tag { pub const INIT: u8 = 128; pub const BAND: u8 = 129; pub const FINISH: u8 = 130;
              pub const CANCEL: u8 = 131; pub const BAND_ACK: u8 = 1; pub const PROGRESS: u8 = 2;
              pub const DONE: u8 = 3; pub const ERROR: u8 = 4; }
pub struct InitMessage { /* exactly the fields in the contract section */ }
pub struct ProgressMessage { pub rows_done: u64, pub rows_total: u64 }
pub struct DoneMessage { pub bytes_written: u64 }
pub struct ErrorMessage { pub message: String }
pub struct BandHeader { pub channel: u32, pub start_row: u64, pub rows: u32, pub slot: u32 }
impl BandHeader { pub fn to_bytes(&self) -> [u8; 20]; pub fn from_bytes(b: &[u8]) -> Option<Self>; }
pub fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()>;
pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)>;   // EOF at a frame boundary -> ErrorKind::UnexpectedEof? No: return Ok? — see below
```

`read_frame` semantics (pin in doc + tests): clean EOF before any byte of a frame returns `io::Error` of kind `UnexpectedEof` with message "pipe closed" (callers treat as host-gone); partial frame is also `UnexpectedEof`. JSON payloads via serde_json; `BandHeader` layout LE, field order `channel, start_row, rows, slot`, exactly 20 bytes.

- [ ] **Step 1 (TDD):** Tests first: frame roundtrip (write then read over a `Vec<u8>` cursor, all tags), BandHeader to/from bytes exactness (`[u8;20]`, LE golden bytes for a known header — write the expected byte array literally in the test), InitMessage JSON roundtrip + a golden JSON string assert (field names locked: `protocol_version`, `worker_version`, `output_path`, `width`, `height`, `channels`, `bit_depth`, `shm_name`, `slot_bytes`, `slots`), read_frame EOF behavior. Model on `mmm-core/src/ipc/protocol.rs` freely.
- [ ] **Step 2:** Implement; gates green.
- [ ] **Step 3:** Commit: `feat(ipc): wire protocol v1 (frames, messages, band headers)`

---

### Task 4: Worker-side link (`link.rs`)

**Files:**
- Create: `crates/mpp-ipc/src/link.rs`
- Modify: `crates/mpp-ipc/src/lib.rs`

**Interfaces:**
- Produces:

```rust
/// Worker's view of the host connection: reads frames from `input`,
/// writes frames to `output` (generic so tests use in-memory pipes;
/// main() passes locked stdin/stdout).
pub struct HostLink<R: Read, W: Write> { /* ... */ }
impl<R: Read, W: Write> HostLink<R, W> {
    pub fn new(input: R, output: W) -> Self;
    pub fn read_init(&mut self) -> Result<InitMessage, LinkError>;   // first frame must be Init; validates versions
    pub fn next_event(&mut self) -> Result<HostEvent, LinkError>;    // Band / Finish / Cancel; HostGone on EOF
    pub fn send_band_ack(&mut self, slot: u32) -> Result<(), LinkError>;
    pub fn send_progress(&mut self, rows_done: u64, rows_total: u64) -> Result<(), LinkError>;
    pub fn send_done(&mut self, bytes_written: u64) -> Result<(), LinkError>;
    pub fn send_error(&mut self, message: &str) -> Result<(), LinkError>;
}
pub enum HostEvent { Band(BandHeader), Finish, Cancel, HostGone }
pub enum LinkError { /* thiserror: Io, Protocol(String) */ }
```

`read_init` checks `protocol_version == IPC_PROTOCOL_VERSION` and `worker_version == expected` — expected passed in: make it `read_init(&mut self, expected_worker_version: &str)`. Frames are flushed after every send (the host waits on acks).

- [ ] **Step 1 (TDD):** Tests over in-memory pipes (`std::io::Cursor` for input built by the protocol writer fns; `Vec<u8>` output inspected with `read_frame`): happy handshake; wrong first tag → Protocol error; version mismatch (both fields separately) → Protocol error mentioning the field; Band/Finish/Cancel event stream; EOF → `HostGone` (not an error — a host crash is an event the worker handles by cleanup); ack/progress/done/error frames appear correctly tagged on the output. Model on `mmm-core/src/ipc/client.rs`.
- [ ] **Step 2:** Implement; gates green.
- [ ] **Step 3:** Commit: `feat(ipc): worker-side host link`

---

### Task 5: PlanarRows generalization + staging file

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff.rs` (writer takes the trait)
- Modify: `crates/mpp-core/src/formats/mod.rs` or `tiff.rs` (trait home: put `PlanarRows` in `formats/mod.rs` — it describes any planar f32 image source)
- Modify: `crates/mpp-core/src/formats/xisf.rs` (impl for XisfImage)
- Create: `crates/mpp-ipc-worker/Cargo.toml`, `crates/mpp-ipc-worker/src/main.rs` (stub main: `fn main() {}` — Task 6 fills it; needed so the member compiles), `crates/mpp-ipc-worker/src/staging.rs`
- Modify: root `Cargo.toml` (add the worker member)

**Interfaces:**
- Produces in mpp-core (`formats/mod.rs`):

```rust
/// A planar f32 image source the streaming writers can drain row by row.
pub trait PlanarRows {
    /// Width in pixels.
    fn width(&self) -> u64;
    /// Height in pixels.
    fn height(&self) -> u64;
    /// Channel count.
    fn channels(&self) -> u64;
    /// One row of one channel (len == width), values nominally [0, 1].
    fn row(&self, c: u64, y: u64) -> &[f32];
}
```

`write_bigtiff` (and the private `write_bigtiff_with_target`) change their first parameter from `&XisfImage` to `img: &impl PlanarRows` (or `&dyn` — use `impl`, call sites are static). `XisfImage` implements the trait by delegating to its inherent methods (keep the inherent methods — everything else uses them). ALL existing tests/call sites (`convert.rs`, `publish.rs`, tiff tests) keep compiling untouched apart from the writer signature.

- Produces in mpp-ipc-worker (`staging.rs`):

```rust
/// Mmap-backed planar f32 staging for out-of-order band arrival.
pub struct Staging { /* file + MmapMut + coverage bitmaps per channel */ }
impl Staging {
    pub fn create(path: &Path, width: u64, height: u64, channels: u64) -> io::Result<Self>;
    /// Copy one band; errors on overlap or out-of-range coordinates.
    pub fn write_band(&mut self, channel: u64, start_row: u64, samples: &[f32]) -> Result<(), StagingError>;
    pub fn rows_received(&self) -> u64;                       // total across channels
    pub fn first_uncovered(&self) -> Option<(u64, u64)>;      // (channel, row) or None when complete
    pub fn is_complete(&self) -> bool;
    /// Delete the staging file (also happens on Drop, best effort).
    pub fn remove(self) -> io::Result<()>;
}
impl mpp_core::formats::PlanarRows for Staging { /* reads from the mmap */ }
```

Coverage: per channel a `Vec<bool>` of rows (heights are ≤ millions; fine). `write_band` computes `samples.len() == rows_in_band * width` from the samples slice length (caller passes exactly the band's samples; rows = len / width, validated divisible).

- [ ] **Step 1 (TDD, mpp-core):** Refactor test: none new needed — the compile + the existing 50-test suite green IS the gate for the trait refactor. Do the refactor first, run `cargo test -p mpp-core`: all green unchanged.
- [ ] **Step 2 (TDD, staging):** Worker crate scaffold (Cargo.toml: `mpp-ipc.workspace`, `mpp-core.workspace`, `memmap2.workspace`; add `mpp-ipc-worker` member + `mpp-core` doesn't need changes). Note: add `mpp-core = { path = "crates/mpp-core" }` to workspace deps if not present. Staging tests first: create 4×3×2, write bands out of order (rows 2..3 of ch1, rows 0..2 of ch0, etc.), overlap rejected, out-of-range rejected, `first_uncovered` walks correctly, complete after full coverage, `PlanarRows::row` returns exactly what was written, file deleted on `remove()`. Then implement; green.
- [ ] **Step 3:** Full workspace gates green. Commit: `feat(core,worker): PlanarRows source trait + mmap band staging`

---

### Task 6: Worker main loop + reference test host

**Files:**
- Modify: `crates/mpp-ipc-worker/src/main.rs`
- Create: `crates/mpp-ipc/src/testhost.rs` (behind `#[cfg(feature = "testkit")]`)
- Modify: `crates/mpp-ipc/src/lib.rs` (`#[cfg(feature = "testkit")] pub mod testhost;`)

**Interfaces:**
- Worker behavior (the protocol section is normative): read Init (expected worker version = `env!("CARGO_PKG_VERSION")`) → validate (`channels` 1|3, `bit_depth` known, `slot_bytes`/`slots` sane) → open shm → `Staging::create(<output_path>.mpp-stage-<pid>)` in the output's directory → loop events: `Band` (validate `rows*width*4 <= slot_bytes`, copy from slot via `bytemuck`-free manual f32 LE? — the slot is native-endian f32: cast with `bytemuck::cast_slice` — add `bytemuck.workspace` to the worker) → `write_band` → `send_band_ack`; every N rows send `Progress` (N = max(1, height/100)); `Finish` → completeness check → `write_bigtiff` from the staging through a `BitDepth` parsed from `bit_depth` → staging removed → `send_done(bytes_written = output file len)` → exit 0. `Cancel`/`HostGone` → remove staging, delete partial output (best effort), `send_error` (Cancel only; HostGone has no listener) → exit nonzero. ALL error paths send `Error` before nonzero exit (except HostGone).
- Testhost (`testkit`) — the reference host used by e2e tests AND by Plan B's golden tests to cross-check the C++ host:

```rust
pub struct TestHostConfig {
    pub worker_exe: PathBuf,
    pub output_path: PathBuf,
    pub width: u64, pub height: u64, pub channels: u64,
    pub bit_depth: String,
    pub band_rows: u32,          // rows per Band frame
    pub slots: u32,
}
/// Spawn the worker, stream `pixel(c, y, x) -> f32` through shm bands
/// (in-order by default), collect Progress, expect Done. Returns
/// bytes_written. Panics/errors mirror protocol violations loudly.
pub fn run_write_session(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
) -> Result<u64, String>;
/// Variants the e2e tests need (same shape as run_write_session):
pub fn run_write_session_shuffled(
    cfg: &TestHostConfig,
    pixel: impl Fn(u64, u64, u64) -> f32,
) -> Result<u64, String>;   // deterministic non-monotonic band order (reverse; no RNG dep)
pub fn run_cancel_after_first_band(cfg: &TestHostConfig) -> Result<(), String>; // sends Cancel, expects Error + nonzero exit + no output file
```

Model on `mmm-core/src/ipc/testhost.rs` (376 lines). The host side here creates the shm (`ShmSegment::create`), spawns via `std::process::Command` with piped stdio, writes frames with `protocol::write_frame`, reads with `read_frame`.

- [ ] **Step 1:** The worker is a binary whose behavior test IS Task 7's e2e suite — this task adds no unit tests of its own. This task's gate: everything compiles including the feature-gated testhost. NOTE the root is a virtual workspace, so `--features mpp-ipc/testkit` is NOT valid at the root — use per-package invocations: `cargo build -p mpp-ipc-worker && cargo test -p mpp-ipc --features testkit && cargo test`, plus `cargo clippy -p mpp-ipc --all-targets --features testkit -- -D warnings` alongside the normal workspace clippy/fmt/doc gates.
- [ ] **Step 2:** Implement worker main (structure it as `fn run(link, ...) -> Result` with `main` doing the exit-code mapping, so logic stays reviewable) and testhost.
- [ ] **Step 3:** Commit: `feat(worker,ipc): BigTIFF write worker + testkit reference host`

---

### Task 7: End-to-end tests

**Files:**
- Create: `crates/mpp-ipc-worker/tests/end_to_end.rs`
- Modify: `crates/mpp-ipc-worker/Cargo.toml` (`[dev-dependencies] mpp-ipc = { workspace = true, features = ["testkit"] }`, `tiff.workspace`)

**Interfaces:** consumes the real worker binary via `env!("CARGO_BIN_EXE_mpp-ipc-worker")`.

- [ ] **Step 1 (TDD — these ARE the worker's behavior tests):**

Tests (each builds a `TestHostConfig` with `worker_exe: env!("CARGO_BIN_EXE_mpp-ipc-worker").into()`):
1. `gray_f32_roundtrip`: 64×48×1, `pixel = |_, y, x| ((y * 64 + x) as f32) / (64.0 * 48.0)`, band_rows 7 (doesn't divide 48 — exercises the partial last band), bit_depth "f32". Decode the output with the `tiff` crate: BigTIFF magic, dims, every sample equals `pixel` exactly (f32 passthrough).
2. `rgb_u16_roundtrip`: 16×8×3, bit_depth "u16", verify decoded U16 samples equal `(pixel.clamp(0,1) * 65535).round()` interleaved RGB.
3. `shuffled_band_order`: same as 1 via `run_write_session_shuffled` — result identical (staging reorders).
4. `incomplete_coverage_errors`: drive manually (open testhost internals or a dedicated `run_write_session_incomplete` added to testhost): send Finish after half the bands → worker sends Error mentioning "uncovered", exits nonzero, output file absent, staging file absent.
5. `version_mismatch_errors`: Init with `protocol_version: 99` → Error naming version, nonzero exit.
6. `cancel_cleans_up`: `run_cancel_after_first_band` → no output, no staging leftovers in the dir.
7. `oversize_band_rejected`: `slot_bytes` deliberately smaller than a band (`run_write_session` variant or manual drive) → Error mentioning slot size.

(Whichever of 4/7 need testhost additions: add them to testhost in this task — testhost is testkit code, its API may grow here.)

- [ ] **Step 2:** Run: `cargo test -p mpp-ipc-worker` — all 7 pass (the dev-dependency enables the testkit feature automatically). Then the full gates, using per-package feature invocations (virtual workspace root — `--features pkg/feat` is invalid there): `cargo test && cargo test -p mpp-ipc --features testkit && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo clippy -p mpp-ipc --all-targets --features testkit -- -D warnings && cargo doc --no-deps` — all green.
- [ ] **Step 3:** Update `docs/DESIGN.md` phases section: phase 2 in progress — Rust worker + IPC landed (plan A), C++ module next (plan B). One sentence.
- [ ] **Step 4:** Commit: `test(worker): end-to-end host/worker BigTIFF write suite`

---

## Out of scope for this plan (Plan B and later)

- Everything C++: the PCL module shim, the PCL-free host static lib (`mpp_shm`/`mpp_protocol`/`mpp_host`), CTest golden suite, makefiles, CI workflow, signing/packaging.
- The clean-room PCL API reference document.
- PCL license compliance additions to NOTICE + provenance README (Plan B, where PCL-adjacent work actually starts).
- In-order streaming fast path that skips staging (follow-up optimization; ledger).
- Read support (.btf import), format-specific options dialogs.
