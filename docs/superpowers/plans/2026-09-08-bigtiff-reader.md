# BigTIFF Reader — PixInsight Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the `BigTIFF-pxm` PixInsight module read TIFF/BigTIFF files (general importer, native sample formats) in addition to writing them, plumbed through a new worker "read mode" over an inverted shm protocol.

**Architecture:** Bottom-up, five layers each independently testable: (1) a streaming TIFF *reader* in `mpp-core` that yields de-interleaved per-channel native-format row bands; (2) a protocol-v2 read extension in the Rust `mpp-ipc` crate; (3) worker read mode driving (1) over (2); (4) a PCL-free C++ `ReadSession` in the host lib mirroring `WriteSession`; (5) the module read lifecycle (`Open`+`ReadImage`), new extensions, and the `.big` save-rename fix. Docs/protocol/closeout last.

**Tech Stack:** Rust (`mpp-core`, `mpp-ipc`, `mpp-ipc-worker`; `tiff` 0.11.3 decoder), C++17 host lib (CMake/CTest) + C++20 PCL module (makefile-x64 / MSVC CMake), POSIX+Windows shm.

**Spec:** `docs/superpowers/specs/2026-09-08-bigtiff-reader-design.md` (read it first; this plan argues from it).

## Global Constraints

- **Clean-room (licensing):** never read/derive from PixInsight PCL *module* implementations. Allowed sources only: public PCL headers under `/opt/PixInsight/include/pcl/`, this repo, MergeMosaic/mega-merge-mosaic (our code), TIFF/BigTIFF public specs + `tiff`-crate/libtiff docs. Every task lists sources consulted.
- **Rust gates green at every commit:** `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc` (no warnings; `missing_docs` is warned — every public item in `mpp-core`/`mpp-ipc` gets a doc comment).
- **C++ gates:** host lib builds warning-free under `-Wall -Wextra -Werror` (GCC/Clang) and `/W4 /WX` (MSVC); the module `.so`/`.dll` builds warning-free and links with no undefined `mpp::` symbols.
- **Wire contract is `docs/pi-protocol.md`:** C++ constants mirror it exactly. Bumping `IPC_PROTOCOL_VERSION`/`kProtocolVersion` is a deliberate, matched change on both ends, guarded by `version_sync`.
- **Tests synthesize inputs** (no dependence on `test_data/`); real-data runs are manual smoke tests.
- **End every commit message with:** `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- Worker binary name: `mpp-ipc-worker` (`.exe` on Windows), found beside the module via `dladdr`/`GetModuleFileNameW`.

## File Structure

```
crates/mpp-core/src/formats/
  tiff_read.rs        # NEW: streaming TIFF/BigTIFF reader (Task 1)
  mod.rs              # MODIFY: pub mod tiff_read; re-exports (Task 1)
crates/mpp-ipc/src/
  protocol.rs         # MODIFY: v2 — mode field, ImageMeta, read tags (Task 2)
  link.rs             # MODIFY: worker-side read frames (send_image_meta/send_read_band; read-band-ack event) (Task 2/3)
crates/mpp-ipc-worker/src/
  main.rs             # MODIFY: dispatch write vs read mode (Task 3)
  read.rs             # NEW: read-mode run loop (Task 3)
crates/mpp-ipc-worker/tests/
  read_e2e.rs         # NEW: in-process host drives worker read mode (Task 3)
  version_sync.rs     # MODIFY: assert kProtocolVersion == IPC_PROTOCOL_VERSION == 2 (Task 5)
integration/pixinsight/host/
  mpp_protocol.h/.cpp # MODIFY: kProtocolVersion=2, mode, ImageMeta, read band/ack (Task 4)
  mpp_host.h/.cpp     # MODIFY: add ReadSession (Task 4)
  test/test_read.cpp  # NEW: host read golden test vs real worker (Task 4)
  CMakeLists.txt      # MODIFY: register test_read (Task 4)
integration/pixinsight/module/
  BigTiffFormat.h/.cpp     # MODIFY: CanRead, CanReadIncrementally, extensions, Description (Task 5)
  BigTiffInstance.h/.cpp   # MODIFY: Open geometry probe, ReadImage overloads, .big rename (Task 5)
docs/pi-protocol.md         # MODIFY: document v2 read extension (Task 6)
integration/pixinsight/README.md, docs/DESIGN.md, README.md  # MODIFY: read+write, phase-3 (Task 6)
```

---

### Task 1: `mpp-core` streaming TIFF reader

**Files:**
- Create: `crates/mpp-core/src/formats/tiff_read.rs`
- Modify: `crates/mpp-core/src/formats/mod.rs` (add `pub mod tiff_read;`)
- Test: inline `#[cfg(test)]` in `tiff_read.rs`

**Interfaces:**
- Consumes: `crate::{Error, Result}` (mpp-core error type — check `crates/mpp-core/src/lib.rs`/`error.rs` for the exact constructor names, e.g. `Error::format(path, msg)` used in `wcs/sidecar.rs`), the `tiff` crate (`tiff::decoder::{Decoder, DecodingResult}`, `tiff::ColorType`, `tiff::tags`).
- Produces (later tasks rely on these exact names/types):
  ```rust
  /// Native sample format of a decoded TIFF.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SampleFormat { U8, U16, U32, F32, F64 }
  impl SampleFormat { pub fn bytes(self) -> usize; pub fn wire_name(self) -> &'static str; } // "u8"|"u16"|"u32"|"f32"|"f64"

  /// Nominal colour interpretation (alpha counted separately).
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum ColorClass { Gray, Rgb }

  /// Geometry + format of a TIFF, read from its header.
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct TiffInfo {
      pub width: u64,
      pub height: u64,
      pub color: ColorClass,
      pub color_channels: u32,   // 1 (Gray) or 3 (Rgb)
      pub alpha_channels: u32,   // 0 or 1
      pub sample_format: SampleFormat,
  }
  impl TiffInfo { pub fn total_channels(&self) -> u32 { self.color_channels + self.alpha_channels } }

  pub struct TiffReader { /* owns Decoder<File> + cached TiffInfo */ }
  impl TiffReader {
      /// Open `path`, read+validate the first image's header. Errors on
      /// palette/CMYK/YCbCr/Multiband colour, signed/complex samples, or
      /// unsupported bit depth.
      pub fn open(path: &std::path::Path) -> Result<TiffReader>;
      pub fn info(&self) -> &TiffInfo;
      /// Decode the image chunk-by-chunk (strip or tile) and invoke `sink`
      /// once per (channel, contiguous row band) with the de-interleaved
      /// native-endian sample bytes for that single channel. `bytes.len()
      /// == rows as usize * width as usize * sample_format.bytes()`.
      /// Channels are 0..total_channels(); bands cover every row of every
      /// channel exactly once. Order is chunk order (top-down for strips).
      pub fn read_bands(
          &mut self,
          sink: &mut dyn FnMut(u32 /*channel*/, u64 /*start_row*/, u32 /*rows*/, &[u8]) -> Result<()>,
      ) -> Result<()>;
  }
  ```

- [ ] **Step 1: Add the module.** In `crates/mpp-core/src/formats/mod.rs` add `pub mod tiff_read;` beside the existing `pub mod tiff;`. Read `mod.rs` first to match its doc-comment style.

- [ ] **Step 2: Write failing tests** (inline in `tiff_read.rs`). Use the `tiff` crate's own encoder to synthesize inputs (add `tiff` to `mpp-core`'s `[dev-dependencies]` if not already a normal dep — it is a normal dep, so it's available in tests). Representative cases:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::NamedTempFile;
    use tiff::encoder::{colortype, TiffEncoder};

    // Helper: write a stripped RGB16 image with a deterministic ramp, return its path.
    fn write_rgb16(w: u32, h: u32) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        let mut enc = TiffEncoder::new(std::fs::File::create(f.path()).unwrap()).unwrap();
        let mut data = vec![0u16; (w * h * 3) as usize];
        for y in 0..h { for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            data[i] = (x as u16).wrapping_mul(7);
            data[i + 1] = (y as u16).wrapping_mul(11);
            data[i + 2] = ((x + y) as u16).wrapping_mul(13);
        }}
        enc.write_image::<colortype::RGB16>(w, h, &data).unwrap();
        f
    }

    #[test]
    fn open_reports_rgb16_geometry() {
        let f = write_rgb16(40, 30);
        let r = TiffReader::open(f.path()).unwrap();
        let info = r.info();
        assert_eq!((info.width, info.height), (40, 30));
        assert_eq!(info.color, ColorClass::Rgb);
        assert_eq!(info.color_channels, 3);
        assert_eq!(info.alpha_channels, 0);
        assert_eq!(info.sample_format, SampleFormat::U16);
    }

    #[test]
    fn read_bands_reassembles_rgb16_planes() {
        let (w, h) = (40u32, 30u32);
        let f = write_rgb16(w, h);
        let mut r = TiffReader::open(f.path()).unwrap();
        // Reassemble planar channels from the emitted bands.
        let mut planes = vec![vec![0u16; (w * h) as usize]; 3];
        r.read_bands(&mut |ch, start_row, rows, bytes| {
            assert_eq!(bytes.len(), rows as usize * w as usize * 2);
            for rr in 0..rows as u64 {
                for x in 0..w as u64 {
                    let b = ((rr * w as u64 + x) * 2) as usize;
                    let v = u16::from_ne_bytes([bytes[b], bytes[b + 1]]);
                    let px = ((start_row + rr) * w as u64 + x) as usize;
                    planes[ch as usize][px] = v;
                }
            }
            Ok(())
        }).unwrap();
        // Channel 1 (green) is y*11 across the row.
        assert_eq!(planes[1][5 * w as usize + 0], 5u16.wrapping_mul(11));
        assert_eq!(planes[0][0 * w as usize + 3], 3u16.wrapping_mul(7));
    }

    #[test]
    fn open_rejects_palette() {
        // Build a tiny palette TIFF and assert TiffReader::open errors clearly.
        // (Use tiff::encoder colortype::Gray8 as a stand-in if palette encode
        // is unavailable; otherwise hand-write a 1x1 palette IFD. The assertion
        // is: Err whose message names the unsupported colour type.)
    }
}
```

Add `tempfile` to `mpp-core` `[dev-dependencies]` if absent (check `Cargo.toml` — the workspace already uses `tempfile` in other crates' tests; mirror the version).

- [ ] **Step 3: Run tests, verify they fail.** `cargo test -p mpp-core tiff_read` → FAIL (module/functions absent).

- [ ] **Step 4: Implement `tiff_read.rs`.** Open the file with `Decoder::new(File::open(path)?)`. Map `decoder.colortype()?`:
  - `ColorType::Gray(b)` → `Gray`, 1 colour ch; `RGB(b)` → `Rgb`, 3; `GrayA(b)` → `Gray`+1 alpha; `RGBA(b)` → `Rgb`+1 alpha.
  - `Palette | CMYK | YCbCr | Multiband { .. }` → `Err(Error::format(path, "unsupported TIFF colour type: <variant>"))`.
  - Bit depth `b`: read the `SampleFormat` tag (`tiff::tags::Tag::SampleFormat`; values 1=uint,2=int,3=ieeefp) via `decoder.get_tag_*` if exposed, else infer from `DecodingResult` variant returned by `read_chunk`. Map `(b, fmt)` → `SampleFormat` (8→U8, 16→U16, 32+uint→U32, 32+ieee→F32, 64+ieee→F64); reject signed/other with a clear error.
  - `read_bands`: iterate `0..decoder.strip_count()?` (or `tile_count()` when tiled — detect via `chunk_dimensions()` vs image width, or the presence of tile tags). For each chunk index, `decoder.read_chunk(i)?` → `DecodingResult` (a typed Vec). `chunk_data_dimensions(i)` gives the chunk's valid (w_c, h_c). The chunk holds **interleaved** samples for `total_channels`. De-interleave into one contiguous native-byte buffer **per channel** for the chunk's row span and call `sink(ch, start_row, rows, &bytes)`.
    - For a **stripped** image a strip spans full width; `start_row = i * rows_per_strip`, `rows = chunk_data_dimensions(i).1`.
    - For a **tiled** image, decode a full **grid row of tiles** before emitting row bands (a single tile is not full width): accumulate the tiles across one tile-row into per-channel full-width row bands, then emit. Keep only one tile-row of tiles in memory (bounded).
  - Convert each `DecodingResult` arm (`U8/U16/U32/F32/F64` vecs) to native-endian bytes for the matching `SampleFormat`; a mismatch between declared format and the returned arm is an internal error.

- [ ] **Step 5: Run tests, verify pass.** `cargo test -p mpp-core tiff_read` → PASS. Then full gates: `cargo fmt --check`, `cargo clippy -p mpp-core --all-targets -- -D warnings`, `cargo doc -p mpp-core --no-deps`.

- [ ] **Step 6: Add a tiled + gray8 + f32 case.** Extend tests: a `Gray8` stripped round-trip; an `f32` (`colortype::Gray32Float`) round-trip; and a tiled input (if the `tiff` encoder supports `write_image` with tiles use it, else construct a minimal 2×2-tile classic TIFF by hand in the test and assert reassembly). Run, verify pass.

- [ ] **Step 7: Commit.**
```bash
git add crates/mpp-core/src/formats/tiff_read.rs crates/mpp-core/src/formats/mod.rs crates/mpp-core/Cargo.toml
git commit -m "feat(core): streaming TIFF/BigTIFF reader (per-channel native bands)"
```

---

### Task 2: Protocol v2 — read frames (`mpp-ipc`)

**Files:**
- Modify: `crates/mpp-ipc/src/protocol.rs`
- Test: inline `#[cfg(test)]` in `protocol.rs`

**Interfaces:**
- Consumes: existing `protocol.rs` items (`IPC_PROTOCOL_VERSION`, `tags::*`, `InitMessage`, `BandHeader`, `write_frame`/`read_frame`).
- Produces:
  ```rust
  pub const IPC_PROTOCOL_VERSION: u32 = 2;   // was 1

  // InitMessage gains:
  //   #[serde(default)] pub mode: SessionMode,   // default = Write (wire-compat with v1)
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
  #[serde(rename_all = "snake_case")]
  pub enum SessionMode { #[default] Write, Read }

  // New tags (namespaced in `tags`), preserving disjoint direction ranges
  // (worker→host low, host→worker >=128):
  pub mod tags {
      // ... existing ...
      pub const IMAGE_META: u8 = 5;    // worker -> host (JSON ImageMeta)
      pub const READ_BAND: u8 = 6;     // worker -> host (binary BandHeader, 20 bytes)
      pub const READ_BAND_ACK: u8 = 132; // host -> worker (binary slot u32, 4 bytes)
  }

  /// Worker's authoritative geometry for a read session.
  #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  pub struct ImageMeta {
      pub width: u64,
      pub height: u64,
      pub channels: u64,       // total incl. alpha
      pub alpha: u64,          // 0 or 1
      pub color_space: String, // "gray" | "rgb"
      pub bit_depth: String,   // "u8"|"u16"|"u32"|"f32"|"f64"
  }
  ```

- [ ] **Step 1: Write failing tests.** Add to `protocol.rs` tests: (a) `ImageMeta` JSON round-trip with exact field names; (b) `IPC_PROTOCOL_VERSION == 2`; (c) `InitMessage` with `mode` omitted deserializes to `SessionMode::Write` (wire-compat), and `mode:"read"` → `SessionMode::Read`; (d) the new tag constants have the expected values and don't collide with existing tags.

```rust
#[test]
fn image_meta_json_roundtrip() {
    let m = ImageMeta { width: 9888, height: 6568, channels: 3, alpha: 0,
                        color_space: "rgb".into(), bit_depth: "f32".into() };
    let j = serde_json::to_string(&m).unwrap();
    assert!(j.contains("\"width\":9888") && j.contains("\"color_space\":\"rgb\""));
    assert_eq!(serde_json::from_str::<ImageMeta>(&j).unwrap(), m);
}

#[test]
fn init_mode_defaults_to_write() {
    // A v1 InitMessage JSON (no "mode") must still parse, as Write.
    let v1 = serde_json::json!({
        "protocol_version": 2, "worker_version": "0.1.0", "output_path": "/x",
        "width": 1, "height": 1, "channels": 1, "bit_depth": "f32",
        "shm_name": "/mpp-x", "slot_bytes": 4, "slots": 1
    }).to_string();
    let init: InitMessage = serde_json::from_str(&v1).unwrap();
    assert_eq!(init.mode, SessionMode::Write);
}
```

- [ ] **Step 2: Run, verify fail.** `cargo test -p mpp-ipc protocol` → FAIL.

- [ ] **Step 3: Implement.** Bump the const to `2`; add `SessionMode` (with `Default`); add `#[serde(default)] pub mode: SessionMode` to `InitMessage` (place it last so existing positional constructions/tests still compile — check `InitMessage`'s constructor/`Default` impl and update `crates/mpp-ipc`'s own `InitMessage::new`/defaults if present); add the three tag consts and `ImageMeta`. Keep `BandHeader` reused for `READ_BAND` (same 20-byte layout).

- [ ] **Step 4: Run, verify pass**, then `cargo fmt --check`, `cargo clippy -p mpp-ipc --all-targets -- -D warnings`, `cargo doc -p mpp-ipc --no-deps`. Also run the whole suite `cargo test` — the InitMessage change must not break write-path tests (fix any positional `InitMessage { .. }` literals across the workspace to include `mode` or use `..Default::default()`).

- [ ] **Step 5: Commit.**
```bash
git add crates/mpp-ipc/src/protocol.rs
git commit -m "feat(ipc): protocol v2 — read mode, ImageMeta, read band/ack tags"
```

---

### Task 3: Worker read mode

**Files:**
- Create: `crates/mpp-ipc-worker/src/read.rs`
- Modify: `crates/mpp-ipc-worker/src/main.rs`
- Modify: `crates/mpp-ipc/src/link.rs` (add worker-side read frames)
- Create: `crates/mpp-ipc-worker/tests/read_e2e.rs`

**Interfaces:**
- Consumes: Task 1 `mpp_core::formats::tiff_read::{TiffReader, TiffInfo, SampleFormat, ColorClass}`; Task 2 protocol items; existing `HostLink`, `ShmSegment`.
- Produces on `HostLink` (worker side):
  ```rust
  impl<R: Read, W: Write> HostLink<R, W> {
      pub fn send_image_meta(&mut self, meta: &ImageMeta) -> Result<(), LinkError>;
      pub fn send_read_band(&mut self, hdr: &BandHeader) -> Result<(), LinkError>;
      // next_event() gains a HostEvent::ReadBandAck { slot: u32 } arm for tag READ_BAND_ACK.
  }
  ```
  The read run loop:
  ```rust
  // crates/mpp-ipc-worker/src/read.rs
  pub fn run_read<R: Read, W: Write>(link: &mut HostLink<R, W>, init: &InitMessage) -> Result<(), WorkerError>;
  ```

- [ ] **Step 1: Extend `link.rs` (worker side) — failing test first.** Add a unit test in `link.rs` that a `send_image_meta` writes tag `IMAGE_META` + JSON, and that `next_event` decodes a `READ_BAND_ACK` frame into `HostEvent::ReadBandAck { slot }`. Run → FAIL.

- [ ] **Step 2: Implement the link additions.** `send_image_meta`/`send_read_band` mirror `send_progress`/the write `Band` writer; add the `READ_BAND_ACK` arm to `next_event`'s tag match and a `HostEvent::ReadBandAck { slot: u32 }` variant (4-byte LE payload, same decode as `BandAck`). Run test → PASS.

- [ ] **Step 3: Write the read-mode e2e test** (`tests/read_e2e.rs`), modeled on the existing write e2e (`tests/` — read it for the in-process host harness + shm setup + `Command`/`std::process` spawn pattern). The test: write a small RGB16 TIFF (via the `tiff` encoder), spawn the worker binary with an `Init{mode:Read, output_path:<the tiff>, ...}` (or the probe first), collect `ImageMeta` + all `READ_BAND` frames (acking each slot), reassemble planes, and assert they equal a direct `TiffReader` decode of the same file. Run → FAIL (read mode unimplemented).

- [ ] **Step 4: Implement `read.rs` + wire `main.rs`.**
  - `main.rs`: after `read_init`, branch on `init.mode`: `Write` → existing path (extract current body into `run_write` or keep inline); `Read` → `read::run_read(link, &init)`.
  - `run_read`: `TiffReader::open(Path::new(&init.output_path))?` (map errors → `fail(link, ..)`); build `ImageMeta` from `reader.info()` (`color_space` = "gray"/"rgb", `bit_depth` = `sample_format.wire_name()`, `channels` = total, `alpha`); `link.send_image_meta(&meta)`; open the shm the host created (`ShmSegment::open(&init.shm_name, total_bytes)` where `total_bytes = slot_bytes * slots`); then drive a slot pool: for each band from `reader.read_bands(...)`, wait for a free slot (initially all free; freed by `ReadBandAck`), copy the band bytes into that slot, `send_read_band(BandHeader{channel,start_row,rows,slot})`; pump `next_event` for `ReadBandAck` to reclaim slots (and `HostGone`). After all bands, `send_done(0)` (bytes_written is irrelevant on read; document as 0) and return.
  - Reuse the write path's slot-accounting discipline (fill slot → send → await ack). Keep worker memory bounded to `slots` bands.
  - **Geometry probe entry:** support `mpp-ipc-worker --probe <path>`: open with `TiffReader`, print the `ImageMeta` JSON to stdout, exit 0; on error print the message to stderr, exit 1. (Add a tiny `env::args` check at the top of `main` before the stdin handshake.)

- [ ] **Step 5: Run tests, verify pass.** `cargo test -p mpp-ipc -p mpp-ipc-worker` → PASS. Manually exercise the probe: `cargo run -p mpp-ipc-worker -- --probe <a-test-tiff>` prints geometry JSON.

- [ ] **Step 6: Full Rust gates.** `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc --no-deps` — all green.

- [ ] **Step 7: Commit.**
```bash
git add crates/mpp-ipc/src/link.rs crates/mpp-ipc-worker/src/read.rs crates/mpp-ipc-worker/src/main.rs crates/mpp-ipc-worker/tests/read_e2e.rs
git commit -m "feat(worker): read mode — decode TIFF and stream native bands + --probe"
```

---

### Task 4: Host lib `ReadSession` (C++, PCL-free)

**Files:**
- Modify: `integration/pixinsight/host/mpp_protocol.h`, `mpp_protocol.cpp`
- Modify: `integration/pixinsight/host/mpp_host.h`, `mpp_host.cpp`
- Create: `integration/pixinsight/host/test/test_read.cpp`
- Modify: `integration/pixinsight/host/CMakeLists.txt`

**Interfaces:**
- Consumes: existing `mpp_protocol` (frame codec, `BandHeader`, Init JSON build) and `mpp_os`/`mpp_shm`; the worker binary via `MPP_WORKER_EXE`.
- Produces (`mpp_host.h`, no PCL includes):
  ```cpp
  namespace mpp {
  struct ImageMeta {
    uint64_t width = 0, height = 0, channels = 0, alpha = 0;
    std::string color_space;  // "gray" | "rgb"
    std::string bit_depth;    // "u8"|"u16"|"u32"|"f32"|"f64"
  };
  struct ReadSessionConfig {
    std::string worker_exe;
    std::string input_path;
    uint32_t band_rows = 0;   // rows per band the host will receive; sizes slots
    uint32_t slots = 0;       // >= 2 recommended
    uint64_t sample_bytes = 0;// bytes per sample of the file's native format
    uint64_t width = 0;       // from a prior probe, to size slots before ImageMeta
    uint64_t channels = 0;
  };
  class ReadSession {          // RAII: dtor kills+reaps worker if still running
   public:
    explicit ReadSession(const ReadSessionConfig&);  // creates shm, spawns (mode=read), reads ImageMeta
    const ImageMeta& Meta() const;
    // Pumps READ_BAND frames until Done; for each band invokes sink with a
    // pointer to `rows*width` native samples for one channel; acks the slot
    // after sink returns. Throws HostError on Error frame / unexpected exit.
    void ReadAll(const std::function<void(uint32_t channel, uint64_t startRow,
                                          uint32_t rows, const void* samples)>& sink);
    // Standalone geometry probe (spawns `worker_exe --probe input_path`).
    static ImageMeta Probe(const std::string& worker_exe, const std::string& input_path);
  };
  }
  ```

- [ ] **Step 1: Extend `mpp_protocol` — failing test first.** In `test_read.cpp` (registered in Step 5) assert `kProtocolVersion == 2` and that an `ImageMeta` JSON parser reads the worker's fields. Add `mpp_protocol` support: `kProtocolVersion = 2`; `mode` field in the Init JSON builder (write it as `"mode":"read"` / `"write"`); an `ImageMeta ParseImageMeta(const std::string& json)`; tag constants `kTagImageMeta=5`, `kTagReadBand=6`, `kTagReadBandAck=132` mirroring `docs/pi-protocol.md`. Build → compile; the assertion drives it.

- [ ] **Step 2: Implement `ReadSession`** in `mpp_host.cpp`/`.h`, mirroring `WriteSession` inverted (read `WriteSession` first): ctor computes `slot_bytes = band_rows * width * sample_bytes`, creates the shm segment, spawns the worker with an `Init{mode:read, output_path:input_path, width, height(0 ok — worker authoritative), channels, bit_depth(""), shm_name, slot_bytes, slots}` (the worker overrides geometry via `ImageMeta`), reads the first frame expecting `IMAGE_META` → stores `meta_`. `ReadAll` loops `ReadFrame`: on `READ_BAND` copy the `BandHeader` + the slot's bytes, call `sink`, then write a `READ_BAND_ACK` for that slot; on `Done` return; on `Error` throw `HostError(message)`; unexpected exit → `HostError`. Dtor kills+reaps like `WriteSession`. `Probe` runs `worker_exe --probe input_path`, reads stdout JSON, `ParseImageMeta`.

- [ ] **Step 3: Write the golden test** (`test_read.cpp`): it needs a real TIFF to read. Generate one at test time by invoking the worker? No — simplest: the test writes a small TIFF with a **known deterministic pattern** using a committed tiny helper. Since the host lib can't encode TIFF, and tests must not use `test_data/`, generate the fixture by driving a **write** session first (the worker's write path emits BigTIFF): (a) `WriteSession` to write an f32 RGB `.big.tiff` with a known ramp; (b) `ReadSession` on that file; (c) assert `Meta()` matches (width/height/3/f32) and the reassembled planes equal the ramp. This exercises the full write→read round-trip against the real worker with no PixInsight.

```cpp
// sketch
int main() {
  const char* worker = std::getenv("MPP_WORKER_EXE");            // set by CTest
  // 1) write a 64x48x3 f32 .big.tiff ramp via WriteSession (reuse test_roundtrip's helper)
  // 2) ReadSession rs({worker, path, band_rows, slots, /*sample_bytes*/4, 64, 3});
  // 3) assert rs.Meta().width==64 && height==48 && channels==3 && bit_depth=="f32"
  // 4) rs.ReadAll([&](ch,startRow,rows,samples){ /* scatter into planes */ });
  // 5) assert planes == the ramp; return 0 on success, nonzero on mismatch
}
```

- [ ] **Step 4: Run gate build+test locally** (Linux; worker built via cargo):
```bash
cargo build --release -p mpp-ipc-worker
cmake -S integration/pixinsight/host -B integration/pixinsight/host/ci-build
cmake --build integration/pixinsight/host/ci-build
MPP_WORKER_EXE="$PWD/target/release/mpp-ipc-worker" ctest --test-dir integration/pixinsight/host/ci-build --output-on-failure
```
Expected: all tests (write suite + `test_read`) PASS, C++ warning-free.

- [ ] **Step 5: Register the test** in `CMakeLists.txt` (mirror an existing `add_executable(test_* ...) + target_link_libraries(test_* mpp_host) + add_test(...)` block; the golden write helper may need to be shared — either duplicate the tiny ramp helper into `test_read.cpp` or a small header). Re-run Step 4.

- [ ] **Step 6: Commit.**
```bash
git add integration/pixinsight/host/mpp_protocol.h integration/pixinsight/host/mpp_protocol.cpp integration/pixinsight/host/mpp_host.h integration/pixinsight/host/mpp_host.cpp integration/pixinsight/host/test/test_read.cpp integration/pixinsight/host/CMakeLists.txt
git commit -m "feat(pi-host): ReadSession + read golden test (write->read round-trip)"
```

---

### Task 5: Module read lifecycle + extensions + version sync + `.big` fix

**Files:**
- Modify: `integration/pixinsight/module/BigTiffFormat.h`, `BigTiffFormat.cpp`
- Modify: `integration/pixinsight/module/BigTiffInstance.h`, `BigTiffInstance.cpp`
- Modify: `crates/mpp-ipc-worker/tests/version_sync.rs`

**Interfaces:**
- Consumes: Task 4 `mpp::ReadSession`/`ImageMeta`; PCL read API (`ImageDescription`, `ImageInfo`, `ImageOptions`, `ColorSpace`, the five `ReadImage` overloads — see `docs/pi-protocol.md` and `PCL_FILEFORMAT_API.md`; extend the API reference if a cited symbol is new).
- Produces: a read-capable `BigTiffFormat`/`BigTiffInstance`.

- [ ] **Step 1: version-sync first (red→green).** Update `version_sync.rs` to also assert the protocol versions agree: parse `IPC_PROTOCOL_VERSION` from `crates/mpp-ipc/src/protocol.rs` and `kProtocolVersion` from `integration/pixinsight/host/mpp_protocol.h`, assert both `== 2`. Run → it should PASS only once Task 4 set `kProtocolVersion=2` and Task 2 set the Rust const; if either lags, fix it. (Module/worker *version strings* remain guarded as before.)

- [ ] **Step 2: Format capabilities.** `BigTiffFormat.cpp`/`.h`: `CanRead()` → `true`; add `CanReadIncrementally()` override → `false` (explicit, with a comment: the core uses whole-image `ReadImage` for interactive opens — empirically verified); `FileExtensions()` → `{ ".big.tiff", ".big.tif", ".btf", ".tf8" }`; update `Description()` to say read+write. Build the module (`make -C integration/pixinsight/module -f makefile-x64`, PCL prefix `~/.local/pcl-build-0182`) → still links, warning-free.

- [ ] **Step 3: `Open()` — geometry probe.** Replace the write-only throw in `BigTiffInstance::Open(path, hints)`: run `mpp::ReadSession::Probe(ResolveWorkerPath(), path.ToUTF8())` to get `ImageMeta`; build one `ImageDescription`:
  - `info.width/height` from meta; `info.numberOfChannels = meta.channels`; `info.colorSpace = (meta.color_space=="rgb") ? ColorSpace::RGB : ColorSpace::Gray`; `info.supported = true`.
  - `options.bitsPerSample` = 8/16/32/64 from `bit_depth`; `options.ieeefpSampleFormat = (bit_depth=="f32"||"f64")`; `options.complexSample=false`; `options.signedIntegers=false`.
  - Set `path_`, `open_=true`; return a one-element `ImageDescriptionArray`. Map any `mpp::HostError` → `pcl::Error`.

- [ ] **Step 4: `ReadImage` overloads.** Add the five overrides. Each: verify the destination sample type matches the declared `bit_depth` (else `throw Error(...)` — only the matching overload should ever be called); `image.AllocateData(width, height, numberOfChannels, colorSpace)`; construct a `mpp::ReadSession` (config from the stored meta/geometry, `band_rows` chosen so a band ≈ 4 MiB, `slots>=2`, `sample_bytes` from bit_depth); `rs.ReadAll([&](ch,startRow,rows,samples){ copy rows*width samples into image[channel=ch] at startRow })`. Use the PCL pixel access that writes a channel's row band (mirror how the write path reads a channel band; e.g. `image.PixelData(ch)` + row offset, or per-row `image.ScanLine`/sample pointer — confirm the exact accessor from the headers). Native copy: the incoming `samples` are already the destination type. Map `HostError` → `pcl::Error`.
  - Implement the four non-f32 overloads for `UInt8Image`/`UInt16Image`/`UInt32Image`/`DImage` analogously; the sample pointer type differs, the loop is identical.

- [ ] **Step 5: `.big` save-rename fix (write path).** In `BigTiffInstance::Create(filePath, ...)`, before resolving/opening: if the path's suffix is `.big` (compound-extension dialog quirk) or otherwise lacks a `.tiff`/`.tif`/`.btf`/`.tf8` tail, rewrite `path_` to `<path>.tiff` (so `.big` → `.big.tiff`) and emit a one-line notice via `pcl::Console().WarningLn(...)` naming the new path. Keep the write flow otherwise unchanged.

- [ ] **Step 6: Build + gates.** Build the module warning-free; `nm -D -u BigTIFF-pxm.so | grep mpp::` empty; entry points exported. Run the full Rust suite + host CTest (Task 4 commands) — all green. If a probe-vs-read geometry mismatch is possible, assert-and-error in `ReadImage` when `ReadSession::Meta()` disagrees with `Open()`'s declared geometry.

- [ ] **Step 7: Commit.**
```bash
git add integration/pixinsight/module/BigTiffFormat.h integration/pixinsight/module/BigTiffFormat.cpp integration/pixinsight/module/BigTiffInstance.h integration/pixinsight/module/BigTiffInstance.cpp crates/mpp-ipc-worker/tests/version_sync.rs
git commit -m "feat(pi-module): read lifecycle (Open+ReadImage), .big.tiff/.big.tif, .big save-rename"
```

---

### Task 6: Protocol doc, provenance, README/DESIGN, closeout

**Files:**
- Modify: `docs/pi-protocol.md`, `integration/pixinsight/PCL_FILEFORMAT_API.md` (if new PCL read symbols were cited), `integration/pixinsight/README.md`, `docs/DESIGN.md`, `README.md`

- [ ] **Step 1: `pi-protocol.md` v2.** Bump the title/version to v2; add a "Read mode" section documenting: `InitMessage.mode`, the `ImageMeta` frame (tag 5, JSON, fields + example), `READ_BAND` (tag 6, worker→host, 20-byte `BandHeader`, native sample bytes in the slot), `READ_BAND_ACK` (tag 132, host→worker, 4-byte slot), the inverted slot ownership (worker fills → host acks), and that `Done.bytes_written` is 0/ignored on read. Note back-compat: `mode` defaults to write.

- [ ] **Step 2: API reference.** If any PCL read symbol used in Task 5 (`ImageDescription`, `ReadImage`, pixel accessors) is not already documented in `PCL_FILEFORMAT_API.md`, add it with file:line citations from `/opt/PixInsight/include/pcl/` (clean-room provenance; list files consulted).

- [ ] **Step 3: READMEs + DESIGN.** `integration/pixinsight/README.md`: state read+write; document the read manual-smoke (open `test.big.tiff`, verify pixels); note `.big` auto-rename; note deferred items (WCS sidecar, incremental read, palette/CMYK, `.tif`/`.tiff` claiming). `docs/DESIGN.md`: phase-3 progress note + the decisions this plan locked (whole-image read, worker read-mode inverted protocol, native fidelity, extension additions). `README.md` (root): phase-3 status line.

- [ ] **Step 4: Full verification.** `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc --no-deps`; module build clean; host CTest green (write + read); version-sync green.

- [ ] **Step 5: Commit.**
```bash
git add docs/pi-protocol.md integration/pixinsight/PCL_FILEFORMAT_API.md integration/pixinsight/README.md docs/DESIGN.md README.md
git commit -m "docs(pi): protocol v2 read mode, read+write README/DESIGN, phase-3 closeout"
```

---

## Out of scope (deferred — do not implement here)

- WCS sidecar readback (→ WCS FITS keywords) — its own phase.
- Incremental read (`ReadSamples`) — the core uses whole-image `ReadImage`.
- General TIFF-tag→FITS mapping, ICC, thumbnail, image properties.
- Palette / CMYK / YCbCr / Multiband inputs (rejected with a clear error).
- Claiming `.tif`/`.tiff` (registration conflict with stock TIFF; the stock-TIFF-replacement phase).
- Classic (non-Big) TIFF writing.
```
