# Mainstream Formats via Spawned vips Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mpp convert in.xisf out.png` (also .jpg, .webp, .avif, .jxl) — mainstream-format export at any size by spawning the official `vips` binary, per the recorded hybrid decision, with the WCS sidecar carried alongside every output.

**Architecture:** All sample-format math stays in Rust: the converter writes an intermediate BigTIFF at the target bit depth (reusing the existing strip-streaming writer, extended with a U8 mode), then spawns `vips copy tmp.tif "out.png[options]"` as a pure container conversion, then deletes the temp file via an RAII guard. Rationale (verified empirically on vips 8.15.1, 2026-08-21): vips savers silently auto-cast unsupported band formats — float [0,1] data becomes a black 8-bit image with no error — and `pngsave` writes 8-bit unless `bitdepth=16` is passed, so delegating numeric conversion to vips would corrupt data silently. A new `vips.rs` module handles binary discovery (friendly install hint when missing) and process spawning with stderr capture.

**Tech Stack:** Existing workspace; no new Rust dependencies. External: `vips` CLI from libvips-tools (installed on this machine: 8.15.1). Available savers verified: pngsave, jpegsave, webpsave, heifsave (.avif), jxlsave. **No EXR saver exists in vips** — EXR export is out of scope (DESIGN.md's "everything libvips provides" only ever meant vips-supported operations; task 6 records this). `fitssave` exists (cfitsio compiled in) — noted for the future FITS evaluation, not used here.

**Spec:** `docs/DESIGN.md` (hybrid vips decision under "Architecture decisions"; WCS sidecar section). Prior slice: the merged converter (`convert.rs`, `formats/tiff.rs`, `wcs/*`) — this plan extends it.

## Global Constraints

- Every public item in `mpp-core` gets a doc comment; `#![warn(missing_docs)]` stays warning-free.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo doc --no-deps` warning-free at every commit.
- Tests must not depend on `test_data/` or real images — synthesize inputs (`testsupport::synth_xisf`).
- **vips-gated tests:** any test that invokes the real `vips` binary must first check `vips_available()` and return early (with `eprintln!("skipping: vips binary not found on PATH");`) when it is absent, so `cargo test` stays green on machines without libvips. On this machine vips is installed, so they run for real.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure

```
crates/mpp-core/src/
  formats/tiff.rs   # Modify: BitDepth::U8 + to_u8 + Gray8/RGB8 arms   (Task 1)
  vips.rs           # Create: vips_available, vips_copy                 (Task 2)
  convert.rs        # Modify: OutputFormat routing, ConvertOptions      (Task 3)
                    #         { bit_depth: Option<BitDepth>, quality },
                    #         vips delegation + TempTiff guard          (Task 4)
  lib.rs            # Modify: pub mod vips; crate docs line             (Task 2)
crates/mpp/src/main.rs        # Modify: optional --bit-depth u8, --quality (T3 min / T5)
crates/mpp/tests/convert.rs   # Modify: add gated PNG CLI test          (Task 5)
docs/DESIGN.md                # Modify: record delegation details       (Task 6)
README.md                     # Modify: supported formats               (Task 6)
```

Format policy table (implemented in Task 3, documented in Task 6):

| Ext | vips saver | Allowed depths | Default | Saver options |
|---|---|---|---|---|
| tif/tiff | (native Rust) | F32, U16, U8 | F32 | — |
| png | pngsave | U16, U8 | U16 | `[bitdepth=16]` when U16 |
| jpg/jpeg | jpegsave | U8 | U8 | `[Q=n]` when quality set |
| webp | webpsave | U8 | U8 | `[Q=n]` when quality set |
| avif | heifsave | U8 | U8 | `[Q=n]` when quality set |
| jxl | jxlsave | U16, U8 | U16 | `[Q=n]` when quality set |

F32 requested for a non-TIFF format → clear error. Quality set for png/tiff → clear error (lossless paths have no Q).

---

### Task 1: `BitDepth::U8` in the TIFF writer

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff.rs`

**Interfaces:**
- Consumes: existing `write_bigtiff` internals (`stream!` macro, `write_bigtiff_with_target`).
- Produces: `BitDepth::U8` variant; `(1|3 channels, U8)` arms using `colortype::Gray8`/`colortype::RGB8`; `fn to_u8(v: f32) -> u8` clamping [0,1] and scaling by 255 with rounding.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `formats/tiff.rs`:

```rust
    #[test]
    fn u8_output_scales_unit_range() {
        let dir = tmpdir("u8");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 4, 4, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        let opts = TiffWriteOptions { bit_depth: BitDepth::U8 };
        write_bigtiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U8(data) => {
                assert_eq!(data[0], 0);
                // sample 8 = 8/16 = 0.5 -> round(0.5 * 255) = 128.
                assert_eq!(data[8], 128);
                // sample 15 = 15/16 -> round(239.06) = 239.
                assert_eq!(data[15], 239);
            }
            other => panic!("expected U8 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u8_rgb_writes() {
        let dir = tmpdir("u8rgb");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 2, 2, 3, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        let opts = TiffWriteOptions { bit_depth: BitDepth::U8 };
        write_bigtiff(&img, &out, &opts).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U8(data) => assert_eq!(data.len(), 12),
            other => panic!("expected U8 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core tiff`
Expected: compile FAIL — `BitDepth::U8` not defined.

- [ ] **Step 3: Implement**

In `BitDepth`, add below the `U16` variant:

```rust
    /// 8-bit unsigned samples; input clamped to [0, 1] and scaled.
    U8,
```

Beside `to_u16`, add:

```rust
/// Convert a unit-range f32 sample to u8 (clamped, rounded).
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}
```

In the `match (channels, opts.bit_depth)` inside `write_bigtiff_with_target`, add before the error arm:

```rust
        (1, BitDepth::U8) => stream!(colortype::Gray8, u8, to_u8),
        (3, BitDepth::U8) => stream!(colortype::RGB8, u8, to_u8),
```

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): 8-bit BigTIFF output (BitDepth::U8)"
```

---

### Task 2: vips process wrapper (`vips.rs`)

**Files:**
- Create: `crates/mpp-core/src/vips.rs`
- Modify: `crates/mpp-core/src/lib.rs` (add `pub mod vips;` and a crate-docs line)

**Interfaces:**
- Consumes: `crate::{Error, Result}`.
- Produces:

```rust
pub fn vips_available() -> Option<PathBuf>;   // find `vips` on PATH
pub(crate) fn vips_copy(vips_bin: &Path, src: &Path, dest_spec: &str, dest_path: &Path) -> Result<()>;
```

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/vips.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vips_copy_surfaces_stderr_on_failure() {
        let Some(vips) = vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mpp-vips-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.tif");
        let out = dir.join("out.png");
        let err = vips_copy(&vips, &missing, &out.display().to_string(), &out).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("vips"), "error mentions vips: {msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vips_available_finds_binary_or_none() {
        // Must not panic either way; on machines with libvips-tools it is Some.
        let _ = vips_available();
    }
}
```

Add to `lib.rs`: `pub mod vips;` and extend the crate-docs list with:

```rust
//! - [`vips`] — discovery and spawning of the official `vips` binary
//!   (mainstream-format conversion and, later, DZI export).
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core vips`
Expected: compile FAIL.

- [ ] **Step 3: Implement**

Above the tests:

```rust
//! Discovery and spawning of the official `vips` binary.
//!
//! Per the hybrid decision in `docs/DESIGN.md`, mpp never links libvips:
//! vips-backed work (mainstream-format conversion, later `dzsave`) spawns
//! the `vips` CLI from libvips-tools. All sample-format math happens in
//! Rust beforehand — vips savers silently auto-cast unsupported band
//! formats (verified on 8.15.1: float input becomes a clipped 8-bit image
//! with no error), so vips is only ever handed data already in the target
//! format, making its job a pure container conversion.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// Locate the `vips` binary on `PATH`, if installed.
pub fn vips_available() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "vips.exe" } else { "vips" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// Run `vips copy <src> <dest_spec>` — a pure container conversion.
///
/// `dest_spec` is the output path plus optional bracketed saver options
/// (e.g. `out.png[bitdepth=16]`); `dest_path` is the plain path, used for
/// error reporting. vips stderr is captured into the error message.
pub(crate) fn vips_copy(
    vips_bin: &Path,
    src: &Path,
    dest_spec: &str,
    dest_path: &Path,
) -> Result<()> {
    let output = Command::new(vips_bin)
        .arg("copy")
        .arg(src)
        .arg(dest_spec)
        .output()
        .map_err(|e| Error::io(dest_path, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::format(
            dest_path,
            format!("vips copy failed ({}): {}", output.status, stderr.trim()),
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS (the failure-path test runs for real on this machine), no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): vips binary discovery + copy wrapper"
```

---

### Task 3: Output-format routing and options plumbing

**Files:**
- Modify: `crates/mpp-core/src/convert.rs`
- Modify: `crates/mpp/src/main.rs` (minimal adaptation so the workspace stays green; full CLI polish is Task 5)

**Interfaces:**
- Produces (consumed by Task 4/5):

```rust
pub enum OutputFormat { BigTiff, Png, Jpeg, Webp, Avif, Jxl }  // Debug, Clone, Copy, PartialEq, Eq
impl OutputFormat {
    pub fn from_ext(ext: &str) -> Option<Self>;          // lowercase ext, "tif"|"tiff"|"png"|"jpg"|"jpeg"|"webp"|"avif"|"jxl"
    pub fn default_bit_depth(self) -> BitDepth;          // per the policy table
    pub fn allows(self, depth: BitDepth) -> bool;
    pub fn lossy_quality(self) -> bool;                  // Jpeg | Webp | Avif | Jxl
    pub fn saver_options(self, depth: BitDepth, quality: Option<u8>) -> String;
}
pub struct ConvertOptions {
    pub bit_depth: Option<BitDepth>,   // None = format default  (CHANGED from BitDepth)
    pub sidecar: bool,
    pub quality: Option<u8>,           // NEW: 1-100, lossy formats only
}
```

`ConvertReport` unchanged.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `convert.rs`:

```rust
    #[test]
    fn output_format_routing_and_defaults() {
        use crate::formats::tiff::BitDepth;
        assert_eq!(OutputFormat::from_ext("tiff"), Some(OutputFormat::BigTiff));
        assert_eq!(OutputFormat::from_ext("png"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::from_ext("jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::from_ext("exr"), None);

        assert_eq!(OutputFormat::BigTiff.default_bit_depth(), BitDepth::F32);
        assert_eq!(OutputFormat::Png.default_bit_depth(), BitDepth::U16);
        assert_eq!(OutputFormat::Jpeg.default_bit_depth(), BitDepth::U8);
        assert_eq!(OutputFormat::Jxl.default_bit_depth(), BitDepth::U16);

        assert!(OutputFormat::BigTiff.allows(BitDepth::U8));
        assert!(OutputFormat::Png.allows(BitDepth::U8));
        assert!(!OutputFormat::Png.allows(BitDepth::F32));
        assert!(!OutputFormat::Jpeg.allows(BitDepth::U16));
    }

    #[test]
    fn saver_options_strings() {
        use crate::formats::tiff::BitDepth;
        assert_eq!(OutputFormat::Png.saver_options(BitDepth::U16, None), "[bitdepth=16]");
        assert_eq!(OutputFormat::Png.saver_options(BitDepth::U8, None), "");
        assert_eq!(OutputFormat::Jpeg.saver_options(BitDepth::U8, Some(85)), "[Q=85]");
        assert_eq!(OutputFormat::Webp.saver_options(BitDepth::U8, None), "");
        assert_eq!(OutputFormat::Avif.saver_options(BitDepth::U8, Some(50)), "[Q=50]");
    }

    #[test]
    fn incompatible_depth_and_quality_rejected() {
        let dir = tmpdir("opts");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        // F32 PNG is impossible.
        let opts = ConvertOptions {
            bit_depth: Some(crate::formats::tiff::BitDepth::F32),
            ..Default::default()
        };
        assert!(convert(&input, &dir.join("o.png"), &opts).is_err());
        // Quality on a lossless format is a user error.
        let opts = ConvertOptions { quality: Some(80), ..Default::default() };
        assert!(convert(&input, &dir.join("o.tif"), &opts).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

The three existing `convert` tests keep passing unchanged (they use `ConvertOptions::default()` / struct-update syntax, which still compiles with the new fields — verify; `sidecar_can_be_disabled` uses `ConvertOptions { sidecar: false, ..Default::default() }` which is fine).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core convert`
Expected: compile FAIL — `OutputFormat` not defined.

- [ ] **Step 3: Implement routing in `convert.rs`**

```rust
/// Supported output containers, chosen by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// BigTIFF, written natively by [`crate::formats::tiff`].
    BigTiff,
    /// PNG via vips `pngsave` (8/16-bit).
    Png,
    /// JPEG via vips `jpegsave` (8-bit, lossy).
    Jpeg,
    /// WebP via vips `webpsave` (8-bit, lossy).
    Webp,
    /// AVIF via vips `heifsave` (8-bit, lossy).
    Avif,
    /// JPEG XL via vips `jxlsave` (8/16-bit).
    Jxl,
}

impl OutputFormat {
    /// Map a lowercase file extension to a format; `None` if unsupported.
    pub fn from_ext(ext: &str) -> Option<Self> {
        Some(match ext {
            "tif" | "tiff" => Self::BigTiff,
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "webp" => Self::Webp,
            "avif" => Self::Avif,
            "jxl" => Self::Jxl,
            _ => return None,
        })
    }

    /// The bit depth used when the caller does not choose one.
    pub fn default_bit_depth(self) -> BitDepth {
        match self {
            Self::BigTiff => BitDepth::F32,
            Self::Png | Self::Jxl => BitDepth::U16,
            Self::Jpeg | Self::Webp | Self::Avif => BitDepth::U8,
        }
    }

    /// Whether this container can hold samples of `depth`.
    pub fn allows(self, depth: BitDepth) -> bool {
        match self {
            Self::BigTiff => true,
            Self::Png | Self::Jxl => matches!(depth, BitDepth::U16 | BitDepth::U8),
            Self::Jpeg | Self::Webp | Self::Avif => matches!(depth, BitDepth::U8),
        }
    }

    /// Whether the format's vips saver takes a `Q` (quality) option.
    pub fn lossy_quality(self) -> bool {
        matches!(self, Self::Jpeg | Self::Webp | Self::Avif | Self::Jxl)
    }

    /// Bracketed vips saver options for this format/depth/quality, or "".
    pub fn saver_options(self, depth: BitDepth, quality: Option<u8>) -> String {
        let mut opts: Vec<String> = Vec::new();
        if self == Self::Png && depth == BitDepth::U16 {
            opts.push("bitdepth=16".to_string());
        }
        if self.lossy_quality()
            && let Some(q) = quality
        {
            opts.push(format!("Q={q}"));
        }
        if opts.is_empty() {
            String::new()
        } else {
            format!("[{}]", opts.join(","))
        }
    }
}
```

Update `ConvertOptions`:

```rust
/// Options for [`convert`].
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// Output sample format; `None` uses the output format's default
    /// ([`OutputFormat::default_bit_depth`]).
    pub bit_depth: Option<BitDepth>,
    /// Write the WCS JSON sidecar when the input carries a solution.
    pub sidecar: bool,
    /// Lossy-format quality 1–100 (vips `Q`); error on lossless formats.
    pub quality: Option<u8>,
}
```

Remove the `Default` derive shown above — keep (and update) the existing manual `impl Default for ConvertOptions` with `bit_depth: None, sidecar: true, quality: None`. A derived Default would silently flip `sidecar` to `false`.

In `convert()`, replace the output-extension check and depth resolution with:

```rust
    let format = OutputFormat::from_ext(&ext(output)).ok_or_else(|| {
        Error::format(output, "unsupported output format (tif/tiff, png, jpg, webp, avif, jxl)")
    })?;
    let depth = match opts.bit_depth {
        Some(d) => {
            if !format.allows(d) {
                return Err(Error::format(
                    output,
                    format!("{d:?} samples are not supported by {format:?} output"),
                ));
            }
            d
        }
        None => format.default_bit_depth(),
    };
    if opts.quality.is_some() && !format.lossy_quality() {
        return Err(Error::format(
            output,
            format!("--quality does not apply to {format:?} output"),
        ));
    }
```

This task implements routing and validation only. In `convert()`'s write step, `BigTiff` writes as before with the resolved `depth`; every other format returns `Error::format(output, "vips-backed output is not wired yet (next commit)")`. Task 4 replaces exactly that arm. The three new tests above do not exercise the vips arm (routing/validation errors fire first), so they pass.

**Also update one existing test:** `rejects_unknown_extensions` currently asserts that `out.png` fails — from Task 4 onward PNG is a *valid* output, so change that assertion's path from `dir.join("out.png")` to `dir.join("out.exr")` (genuinely unsupported: vips has no EXR saver). The `in.fits` input-rejection assertion stays as-is.

Minimal CLI adaptation in `crates/mpp/src/main.rs` (workspace must stay green): change the `--bit-depth` arg to `Option<BitDepthArg>` (drop `default_value_t`), and build `ConvertOptions { bit_depth: bit_depth.map(Into::into), sidecar: !no_sidecar, quality: None }`. Task 5 finishes the CLI.

- [ ] **Step 4: Verify green**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: PASS (all crates), no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core crates/mpp
git commit -m "feat(core): output-format routing, per-format bit depths, quality option"
```

---

### Task 4: vips delegation with temp-TIFF interchange

**Files:**
- Modify: `crates/mpp-core/src/convert.rs`

**Interfaces:**
- Consumes: `vips::{vips_available, vips_copy}` (Task 2), `OutputFormat` (Task 3), `write_bigtiff` (Task 1).
- Produces: `convert()` handles all six formats end-to-end. Temp interchange file `<parent>/<stem>.mpp-tmp-<pid>.tif` next to the output (same filesystem; avoids exhausting a small `/tmp`), removed by RAII guard on every path including errors.

- [ ] **Step 1: Write failing (vips-gated) tests**

Add to `convert.rs` tests:

```rust
    #[test]
    fn png_via_vips_defaults_to_16bit() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("png16");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 6, 4, 1, true);
        let output = dir.join("out.png");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();
        assert_eq!((report.width, report.height), (6, 4));

        // PNG signature.
        let head = std::fs::read(&output).unwrap();
        assert_eq!(&head[0..4], &[0x89, b'P', b'N', b'G']);
        // 16-bit grayscale: PNG IHDR bit depth byte (offset 24) == 16.
        assert_eq!(head[24], 16, "PNG bit depth");

        // Sidecar rides along for vips-backed outputs too.
        assert!(dir.join("out.wcs.json").exists());
        // Temp interchange file cleaned up.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mpp-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jpeg_via_vips_with_quality() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("jpeg");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 8, 8, 3, false);
        let output = dir.join("out.jpg");
        let opts = ConvertOptions { quality: Some(90), ..Default::default() };
        convert(&input, &output, &opts).unwrap();
        let head = std::fs::read(&output).unwrap();
        assert_eq!(&head[0..2], &[0xFF, 0xD8], "JPEG SOI marker");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vips_failure_cleans_up_temp_file() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("vipsfail");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        // A directory where the output file should go makes vips fail.
        let output = dir.join("blocked.png");
        std::fs::create_dir(&output).unwrap();
        assert!(convert(&input, &output, &ConvertOptions::default()).is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mpp-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core convert`
Expected: `png_via_vips_defaults_to_16bit` and friends FAIL (the vips arm still returns the Task-3 error).

- [ ] **Step 3: Implement**

Add to `convert.rs`:

```rust
/// Deletes the wrapped path on drop — the temp interchange TIFF must not
/// survive any exit path, success or error.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The interchange path for a vips-backed output: same directory (same
/// filesystem — multi-GB temp data must not land in a small /tmp), named
/// after the output plus a pid suffix so concurrent runs cannot collide.
fn interchange_path(output: &Path) -> PathBuf {
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    let name = format!("{stem}.mpp-tmp-{}.tif", std::process::id());
    output.with_file_name(name)
}
```

Replace the write step of `convert()` with:

```rust
    let tiff_opts = TiffWriteOptions { bit_depth: depth };
    match format {
        OutputFormat::BigTiff => write_bigtiff(&img, output, &tiff_opts)?,
        _ => {
            let vips_bin = crate::vips::vips_available().ok_or_else(|| {
                Error::format(
                    output,
                    "vips binary not found on PATH; install libvips-tools \
                     (e.g. `sudo apt install libvips-tools`)",
                )
            })?;
            let tmp = interchange_path(output);
            let _guard = TempFileGuard(tmp.clone());
            write_bigtiff(&img, &tmp, &tiff_opts)?;
            let dest_spec = format!(
                "{}{}",
                output.display(),
                format.saver_options(depth, opts.quality)
            );
            crate::vips::vips_copy(&vips_bin, &tmp, &dest_spec, output)?;
        }
    }
```

Also update the `convert` doc comment: it converts XISF to BigTIFF natively or to png/jpg/webp/avif/jxl by spawning vips with a temp BigTIFF interchange.

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS with the three vips tests running for real; no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): mainstream-format output via spawned vips (png/jpg/webp/avif/jxl)"
```

---

### Task 5: CLI polish (`--bit-depth u8`, `--quality`) + gated CLI test

**Files:**
- Modify: `crates/mpp/src/main.rs`
- Modify: `crates/mpp/tests/convert.rs`

**Interfaces:**
- Consumes: `ConvertOptions { bit_depth: Option<BitDepth>, sidecar, quality }`.
- Produces: `mpp convert <IN> <OUT> [--bit-depth f32|u16|u8] [--quality 1-100] [--no-sidecar]`.

- [ ] **Step 1: Write failing (gated) CLI test**

Add to `crates/mpp/tests/convert.rs`:

```rust
fn vips_on_path() -> bool {
    std::process::Command::new("vips")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn convert_produces_png_via_vips() {
    if !vips_on_path() {
        eprintln!("skipping: vips binary not found on PATH");
        return;
    }
    let dir = tmpdir("png");
    let input = dir.join("in.xisf");
    synth_xisf(&input, 8, 8);
    let output = dir.join("out.png");

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .args(["--bit-depth", "u8"])
        .status()
        .unwrap();
    assert!(status.success());
    let head = std::fs::read(&output).unwrap();
    assert_eq!(&head[0..4], &[0x89, b'P', b'N', b'G']);
    assert_eq!(head[24], 8, "u8 override -> 8-bit PNG");
    std::fs::remove_dir_all(&dir).unwrap();
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp`
Expected: FAIL — `u8` is not yet a valid `--bit-depth` value.

- [ ] **Step 3: Implement**

In `main.rs`: add `U8` to `BitDepthArg` (`/// 8-bit unsigned samples (clamped to [0, 1], scaled).`) and its `From` arm (`BitDepthArg::U8 => BitDepth::U8`); add to the `Convert` variant:

```rust
        /// Lossy-format quality, 1-100 (JPEG/WebP/AVIF/JXL only).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: Option<u8>,
```

and pass `quality` through in `ConvertOptions`. Update the `Convert` doc comment to mention mainstream formats: "Convert an image (XISF -> BigTIFF natively; PNG/JPEG/WebP/AVIF/JXL via the vips CLI), carrying the astrometric solution to a WCS JSON sidecar."

- [ ] **Step 4: Verify green**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp
git commit -m "feat(cli): u8 bit depth and --quality for vips-backed formats"
```

---

### Task 6: Record the details in DESIGN.md and README

**Files:**
- Modify: `docs/DESIGN.md`
- Modify: `README.md`

- [ ] **Step 1: Extend the DESIGN.md vips decision bullet**

In the "Architecture decisions" libvips bullet (the "**Decided (2026-08-20):** hybrid." text), append after "Revisit bindings only if process spawning proves limiting.":

```markdown
  Mainstream-format delegation (2026-08-21): all sample-format math stays
  in Rust — the converter writes a temp BigTIFF at the target bit depth
  next to the output, then `vips copy` re-containers it. This is because
  vips savers silently auto-cast unsupported band formats (float [0,1]
  input becomes a clipped 8-bit image with no error, verified on 8.15.1)
  and `pngsave` needs an explicit `bitdepth=16`. Formats: png (u16/u8),
  jpg/webp/avif (u8, `Q=` quality), jxl (u16/u8). vips has no EXR saver,
  so EXR export is out; `fitssave` exists in the Ubuntu build (cfitsio) —
  relevant to the FITS read/write evaluation still pending.
```

- [ ] **Step 2: Update README status + smoke section**

In README.md's Status section, extend the implemented list to "XISF → BigTIFF plus PNG/JPEG/WebP/AVIF/JXL via the vips CLI (`libvips-tools` required for the non-TIFF formats), each with the WCS JSON sidecar". Add one line to the Smoke tests section:

```markdown
- Mainstream formats (requires libvips-tools): `target/release/mpp convert
  <big>.xisf <big>.png` — PNG defaults to 16-bit; JPEG/WebP/AVIF take
  `--quality`.
```

- [ ] **Step 3: Full workspace check**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: all green, no warnings.

- [ ] **Step 4: Commit**

```bash
git add docs/DESIGN.md README.md
git commit -m "docs: record vips mainstream-format delegation details"
```

---

## Out of scope for this plan

- DZI export + HTML viewer (plan 3; `dzsave` reuses `vips.rs`).
- EXR (no vips saver exists), GIF/JP2/PPM (no astro use case), classic non-Big TIFF, stretching/tone-mapping (display formats take stretched input by design — DESIGN.md "Linear data discipline").
- FITS read/write (separate evaluation; note `fitssave` availability).
- Reading mainstream formats back in (conversion is XISF-outward for now).
