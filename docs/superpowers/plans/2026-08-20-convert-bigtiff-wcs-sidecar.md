# XISF → BigTIFF Converter + WCS Sidecar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mpp convert in.xisf out.tif` reads a PixInsight XISF image of any size, writes a BigTIFF (constant memory, strip-streamed), and emits the canonical WCS JSON sidecar when the input carries an astrometric solution — the phase-1 slice whose smoke test is "a >4 GB BigTIFF opens in Photoshop".

**Architecture:** Pure-Rust pipeline in `mpp-core`: a memory-mapped XISF reader (ported from `mmm-core`, per the 2026-08-20 decision reversing the dependency direction), a linear-WCS extractor for PixInsight astrometric-solution properties, a strip-streaming BigTIFF encoder (`tiff` crate, `TiffEncoder::new_big`), and a `convert` orchestrator. The `mpp` CLI adds a thin `convert` subcommand. No libvips anywhere in this plan (vips enters with the mainstream-formats and DZI plans, spawned as a binary).

**Tech Stack:** Rust (edition 2024, workspace already scaffolded), `tiff` 0.11 (BigTIFF encode + decode-for-tests), `quick-xml` 0.37 (pinned to match the mmm-core code being ported), `memmap2` 0.9, `bytemuck` 1, `thiserror` 2, `serde`/`serde_json` 1, `clap` 4 + `anyhow` 1 (CLI only).

**Spec:** `docs/DESIGN.md` (this repo). The mmm-core source being ported lives at `~/dev/MergeMosaic/crates/mmm-core/src/` — port from `formats/mod.rs`, `formats/xisf.rs`, and the linear part of `astrometry.rs`. Porting mmm code is fine (same author, Apache-2.0 both ways); the clean-room rule in DESIGN.md is about PixInsight's PCL modules, which are not touched here.

## Global Constraints

- Every public item in `mpp-core` gets a doc comment; `#![warn(missing_docs)]` is already set in `lib.rs` and must stay warning-free.
- `cargo fmt`, `cargo clippy --all-targets`, and `cargo doc` must be warning-free at every commit.
- Tests must not depend on `test_data/` or any real image — synthesize all inputs. Real-data runs are manual smoke tests.
- License: Apache-2.0 (workspace-level; no per-file headers, matching MMM).
- Rename all MMM-specific naming during the port: `XisfPanel` → `XisfImage`, `mmm-` test-dir prefixes → `mpp-`, doc comments referencing "mosaic panels" reworded for a general converter.
- Rust via rustup: run `source ~/.cargo/env` first if `cargo` is not on PATH.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure

```
crates/mpp-core/src/
  lib.rs            # module decls + crate docs (Modify)
  error.rs          # Error/Result — Io + Format variants          (Task 1)
  formats/mod.rs    # FitsKeyword, XisfProperty, PropertyValue,
                    # SampleFormat — shared metadata types          (Task 2)
  formats/xisf.rs   # XisfImage mmap reader (port of mmm xisf.rs)   (Task 3)
  formats/tiff.rs   # BitDepth, TiffWriteOptions, write_bigtiff     (Task 6)
  wcs/mod.rs        # LinearWcs + TAN math + from_xisf_properties   (Task 4)
  wcs/sidecar.rs    # WcsSidecar schema v1, read/write, path rule   (Task 5)
  convert.rs        # convert() orchestration + ConvertReport       (Task 7)
  testsupport.rs    # cfg(test) synthetic-XISF builder              (Task 6)
crates/mpp/src/main.rs        # clap `convert` subcommand           (Task 8)
crates/mpp/tests/convert.rs   # CLI integration test                (Task 8)
crates/mpp/tests/big_smoke.rs # #[ignore] >4GB end-to-end           (Task 9)
docs/DESIGN.md                # record decisions (Task 1), sidecar schema (Task 5)
```

---

### Task 1: Record decisions, wire dependencies, error module

**Files:**
- Modify: `docs/DESIGN.md` (architecture-decisions bullets, lines ~49–59)
- Modify: `Cargo.toml` (workspace deps)
- Modify: `crates/mpp-core/Cargo.toml`
- Create: `crates/mpp-core/src/error.rs`
- Modify: `crates/mpp-core/src/lib.rs`

**Interfaces:**
- Produces: `mpp_core::error::{Error, Result}` with constructors `Error::io(path, source)` and `Error::format(path, reason)`; `pub use error::{Error, Result};` from `lib.rs`. Every later task returns these.

- [ ] **Step 1: Record the two phase-1 decisions in DESIGN.md**

In `docs/DESIGN.md`, replace the libvips bullet's open question — the text from "Open question for phase 1: Rust bindings vs. spawning" through "blocks the converter leg." — with:

```markdown
  **Decided (2026-08-20):** hybrid. The XISF→BigTIFF leg is pure Rust
  (`tiff` crate, `TiffEncoder::new_big`, strip-streamed, constant memory);
  vips is used by *spawning the official `vips` binaries* for `dzsave` and
  mainstream-format conversion — no FFI bindings, no LGPL linking
  obligations. Revisit bindings only if process spawning proves limiting.
```

Replace the XISF bullet's text from "Initially depend on `mmm-core`" through "layering hurts." with:

```markdown
  **Decided (2026-08-20):** the XISF reader and linear-WCS extraction are
  *ported* from `mmm-core` into `mpp-core` — `mpp-core` owns format code,
  and the long-term dependency direction is the reverse of the original
  idea: `mmm-core` will eventually depend on `mpp-core` for file-format
  support.
```

Keep the FITS sentence ("FITS: vips has cfitsio support…") unchanged.

- [ ] **Step 2: Add workspace dependencies**

In root `Cargo.toml` `[workspace.dependencies]`, add:

```toml
thiserror = "2"
memmap2 = "0.9"
bytemuck = "1"
quick-xml = "0.37"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tiff = "0.11"
```

(`quick-xml` is pinned to 0.37 — the ported reader is written against that API; upgrading is a separate chore.)

In `crates/mpp-core/Cargo.toml` `[dependencies]`:

```toml
thiserror.workspace = true
memmap2.workspace = true
bytemuck.workspace = true
quick-xml.workspace = true
serde.workspace = true
serde_json.workspace = true
tiff.workspace = true
```

- [ ] **Step 3: Write a failing test for the error module**

Create `crates/mpp-core/src/error.rs` with only the test module at first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_carry_path_and_reason() {
        let e = Error::format("/tmp/x.xisf", "bad signature");
        assert_eq!(
            e.to_string(),
            "unsupported or malformed file /tmp/x.xisf: bad signature"
        );
        let e = Error::io(
            "/tmp/y.tif",
            std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        );
        assert!(e.to_string().contains("/tmp/y.tif"));
    }
}
```

Add `pub mod error;` and `pub use error::{Error, Result};` to `lib.rs` (below the crate docs).

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p mpp-core`
Expected: FAIL to compile — `Error` not defined.

- [ ] **Step 5: Implement the error module**

Above the test module in `error.rs`:

```rust
//! Crate-wide error type: every fallible operation reports the file it was
//! working on plus either the underlying I/O error or a human-readable reason.

use std::path::PathBuf;

/// Crate-wide result alias over [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The error type for all mpp-core operations.
///
/// Deliberately small: an I/O failure on a file, or a malformed/unsupported
/// file. New variants are added only when a caller needs to distinguish them.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system I/O failure on `path`.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The file or directory the operation was working on.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// `path` exists but its contents are unsupported, malformed, or
    /// inconsistent with what the operation requires; `reason` says how.
    #[error("unsupported or malformed file {path}: {reason}")]
    Format {
        /// The offending file.
        path: PathBuf,
        /// Human-readable explanation of what is wrong.
        reason: String,
    },
}

impl Error {
    /// Wrap an [`std::io::Error`] with the path it occurred on.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }

    /// Build an [`Error::Format`] from a path and a reason.
    pub fn format(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Format { path: path.into(), reason: reason.into() }
    }
}
```

(The `{path}` interpolation of a `PathBuf` works because thiserror 2 formats path fields via `Path::display` natively — this is exactly the code mmm-core compiles with thiserror 2 today.)

- [ ] **Step 6: Run tests, fmt, clippy**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add docs/DESIGN.md Cargo.toml Cargo.lock crates/mpp-core
git commit -m "feat(core): error module; record phase-1 vips + XISF-port decisions"
```

---

### Task 2: Format metadata types (`formats/mod.rs`)

**Files:**
- Create: `crates/mpp-core/src/formats/mod.rs`
- Modify: `crates/mpp-core/src/lib.rs`

**Interfaces:**
- Produces: `mpp_core::formats::{FitsKeyword, XisfProperty, PropertyValue, SampleFormat}` — exactly the shapes below; Tasks 3–7 consume them.

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/formats/mod.rs` containing (for now) just the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_value_accessors() {
        assert_eq!(PropertyValue::F64(1.5).as_f64(), Some(1.5));
        assert_eq!(PropertyValue::I64(-7).as_f64(), Some(-7.0));
        assert_eq!(PropertyValue::Str("x".into()).as_str(), Some("x"));
        assert_eq!(
            PropertyValue::F64Vec(vec![1.0, 2.0]).as_f64_vec(),
            Some(&[1.0, 2.0][..])
        );
        let m = PropertyValue::F64Mat { rows: 2, cols: 2, data: vec![1.0, 0.0, 0.0, 1.0] };
        assert_eq!(m.as_f64_mat(), Some((2, 2, &[1.0, 0.0, 0.0, 1.0][..])));
        assert!(PropertyValue::F64Vec(Vec::new()).needs_attachment_data());
        assert!(!PropertyValue::F64(0.0).needs_attachment_data());
    }

    #[test]
    fn sample_format_parse_and_size() {
        assert_eq!(SampleFormat::parse("Float32"), Some(SampleFormat::Float32));
        assert_eq!(SampleFormat::parse("nope"), None);
        assert_eq!(SampleFormat::Float32.bytes_per_sample(), 4);
        assert_eq!(SampleFormat::UInt16.bytes_per_sample(), 2);
        assert_eq!(SampleFormat::Float64.bytes_per_sample(), 8);
    }
}
```

Add `pub mod formats;` to `lib.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mpp-core`
Expected: compile FAIL — types not defined.

- [ ] **Step 3: Port the types**

Port the entire contents of `~/dev/MergeMosaic/crates/mmm-core/src/formats/mod.rs` above its `pub mod xisf;` line — the types `FitsKeyword`, `XisfProperty`, `PropertyValue` (with `as_f64`, `as_str`, `as_f64_vec`, `as_f64_mat`, `needs_attachment_data`), and `SampleFormat` (with `parse`, `bytes_per_sample`) — verbatim including doc comments, with these changes:

- Module doc becomes:

```rust
//! Image format readers and writers.
//!
//! Readers expose per-channel planar `f32` planes over a memory-mapped file,
//! plus passthrough metadata (FITS keywords, XISF properties). Writers
//! stream pixel data out in constant memory.
```

- Do **not** add `pub mod xisf;` yet (Task 3 does).
- Keep the `serde::Serialize, serde::Deserialize` derives on `FitsKeyword`, `XisfProperty`, `PropertyValue` exactly as in mmm-core.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): format metadata types (FITS keywords, XISF properties)"
```

---

### Task 3: XISF reader port (`formats/xisf.rs`)

**Files:**
- Create: `crates/mpp-core/src/formats/xisf.rs`
- Modify: `crates/mpp-core/src/formats/mod.rs` (add `pub mod xisf;`)

**Interfaces:**
- Consumes: `formats::{FitsKeyword, PropertyValue, SampleFormat, XisfProperty}`, `crate::{Error, Result}`.
- Produces: `formats::xisf::{XisfHeader, XisfImage}` where `XisfImage` has exactly: `open(path: &Path) -> Result<Self>`, `path() -> &Path`, `header() -> &XisfHeader`, `width() -> u64`, `height() -> u64`, `channels() -> u64`, `channel(c: u64) -> &[f32]`, `row(c: u64, y: u64) -> &[f32]`, `advise_sequential()`. `XisfHeader` has public fields `width, height, channels, sample_format, data_offset, data_size, fits_keywords: Vec<FitsKeyword>, properties: Vec<XisfProperty>`.

- [ ] **Step 1: Port the file with its tests**

Copy `~/dev/MergeMosaic/crates/mmm-core/src/formats/xisf.rs` to `crates/mpp-core/src/formats/xisf.rs` in full — implementation *and* the `#[cfg(test)] mod tests` (the tests are the failing-test half of this task's TDD cycle; they were written against this exact behavior). Apply these mechanical renames:

- `XisfPanel` → `XisfImage` everywhere (type name, impl block, doc comments).
- Test temp-dir prefixes `mmm-xisf-…` → `mpp-xisf-…`.
- Module doc header: replace the first paragraph with:

```rust
//! Minimal XISF reader (PixInsight's native format).
//!
//! Supports the monolithic XISF subset PixInsight emits for processed
//! images: one `<Image>` element, planar pixel storage, little-endian
//! Float32 samples, uncompressed attachment data block. Anything else
//! errors clearly; compression and other sample formats come later.
//!
//! Spec: <https://pixinsight.com/doc/docs/XISF-1.0-spec/XISF-1.0-spec.html>
```

- Doc comment on `XisfImage` (was "A memory-mapped, read-only XISF panel…"): "A memory-mapped, read-only XISF image exposing planar f32 channel data."
- Imports change from `use super::{FitsKeyword, PropertyValue, SampleFormat, XisfProperty};` + `use crate::{Error, Result};` — identical module paths exist in mpp-core, so these lines stay as-is.

Add `pub mod xisf;` at the top of `formats/mod.rs` (after the module docs).

- [ ] **Step 2: Run the ported tests**

Run: `cargo test -p mpp-core xisf`
Expected: PASS — `reads_synthetic_xisf`, `parses_properties_in_all_shapes`, `decodes_real_pixinsight_inline_base64`, `rejects_bad_signature` all green. If a rename was missed the compiler says so.

- [ ] **Step 3: Full check**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): XISF reader ported from mmm-core (XisfImage)"
```

---

### Task 4: Linear WCS model (`wcs/mod.rs`)

**Files:**
- Create: `crates/mpp-core/src/wcs/mod.rs`
- Modify: `crates/mpp-core/src/lib.rs` (add `pub mod wcs;`)

**Interfaces:**
- Consumes: `formats::XisfProperty`, `formats::PropertyValue`.
- Produces:

```rust
pub struct LinearWcs {
    pub crval: [f64; 2],
    pub crpix: [f64; 2],
    pub cd: [[f64; 2]; 2],
    pub ctype: [String; 2],
    pub radesys: String,
}   // + Serialize, Deserialize, Debug, Clone, PartialEq
impl LinearWcs {
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64);
    pub fn sky_to_pixel(&self, ra: f64, dec: f64) -> (f64, f64);
}
pub fn from_xisf_properties(props: &[XisfProperty]) -> Option<LinearWcs>;
```

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/wcs/mod.rs` with just:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::{PropertyValue, XisfProperty};

    fn prop(id: &str, type_: &str, value: PropertyValue) -> XisfProperty {
        XisfProperty { id: id.into(), type_: type_.into(), value, location: None }
    }

    fn solution_props() -> Vec<XisfProperty> {
        vec![
            prop(
                "PCL:AstrometricSolution:ReferenceCelestialCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![84.199, -3.240]),
            ),
            prop(
                "PCL:AstrometricSolution:ReferenceImageCoordinates",
                "F64Vector",
                PropertyValue::F64Vec(vec![100.0, 200.0]),
            ),
            prop(
                "PCL:AstrometricSolution:LinearTransformationMatrix",
                "F64Matrix",
                PropertyValue::F64Mat {
                    rows: 2,
                    cols: 2,
                    data: vec![-4.4368e-4, 0.0, 0.0, 4.4368e-4],
                },
            ),
            prop(
                "PCL:AstrometricSolution:ProjectionSystem",
                "String",
                PropertyValue::Str("Gnomonic".into()),
            ),
            prop(
                "Observation:CelestialReferenceSystem",
                "String",
                PropertyValue::Str("ICRS".into()),
            ),
        ]
    }

    #[test]
    fn extracts_linear_wcs_from_pixinsight_properties() {
        let w = from_xisf_properties(&solution_props()).unwrap();
        assert_eq!(w.crval, [84.199, -3.240]);
        // PixInsight image coords (centers at k + 0.5, 0-based) → FITS
        // pixel coords (centers at integers, 1-based): +0.5 on both axes.
        assert_eq!(w.crpix, [100.5, 200.5]);
        assert_eq!(w.cd, [[-4.4368e-4, 0.0], [0.0, 4.4368e-4]]);
        assert_eq!(w.ctype, ["RA---TAN".to_string(), "DEC--TAN".to_string()]);
        assert_eq!(w.radesys, "ICRS");
    }

    #[test]
    fn missing_projection_defaults_to_gnomonic_unknown_refuses() {
        let mut props = solution_props();
        props.retain(|p| p.id != "PCL:AstrometricSolution:ProjectionSystem");
        assert_eq!(from_xisf_properties(&props).unwrap().ctype[0], "RA---TAN");

        let mut props = solution_props();
        props
            .iter_mut()
            .find(|p| p.id == "PCL:AstrometricSolution:ProjectionSystem")
            .unwrap()
            .value = PropertyValue::Str("Bonne".into());
        assert!(from_xisf_properties(&props).is_none());
    }

    #[test]
    fn incomplete_solution_yields_none() {
        let mut props = solution_props();
        props.retain(|p| p.id != "PCL:AstrometricSolution:LinearTransformationMatrix");
        assert!(from_xisf_properties(&props).is_none());
    }

    #[test]
    fn pixel_sky_roundtrip() {
        let w = from_xisf_properties(&solution_props()).unwrap();
        let (ra, dec) = w.pixel_to_sky(3000.0, 4000.0);
        let (x, y) = w.sky_to_pixel(ra, dec);
        assert!((x - 3000.0).abs() < 1e-6, "x {x}");
        assert!((y - 4000.0).abs() < 1e-6, "y {y}");
        // Reference pixel maps exactly to the reference coordinates.
        let (ra0, dec0) = w.pixel_to_sky(w.crpix[0], w.crpix[1]);
        assert!((ra0 - 84.199).abs() < 1e-9);
        assert!((dec0 - -3.240).abs() < 1e-9);
    }
}
```

Add `pub mod wcs;` to `lib.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mpp-core wcs`
Expected: compile FAIL.

- [ ] **Step 3: Port the linear-WCS code**

From `~/dev/MergeMosaic/crates/mmm-core/src/astrometry.rs`, port **only** these items into `wcs/mod.rs`, above the tests:

- `LinearWcs` (struct + `pixel_to_sky` + `sky_to_pixel`) — add `serde::Serialize, serde::Deserialize` to its derives (the sidecar in Task 5 serializes it directly).
- `tan_deproject`, `tan_project` (private helpers, verbatim).
- `wcs_from_properties` — rename to `from_xisf_properties` (it lives in the `wcs` module now; `wcs::from_xisf_properties` reads well), body verbatim.
- `projection_code` (private, verbatim).

Do **not** port `Grid2D`, `WcsModel`, `wcs_cards`, `wcs_cards_flipped`, or `tan_project_checked` — spline/distortion solutions and FITS card emission are out of scope for this plan (the sidecar schema is versioned so distortion can be added later).

Module docs:

```rust
//! Astrometric solutions: the linear WCS model and its extraction from
//! PixInsight XISF astrometric-solution properties.
//!
//! PixInsight (ImageSolver / MosaicByCoordinates) stores plate solutions as
//! XISF `<Property>` elements, not FITS keywords. Extraction to
//! [`LinearWcs`] keeps the stored top-down y axis:
//!
//! - `CRVAL1/2` = `PCL:AstrometricSolution:ReferenceCelestialCoordinates`,
//! - `CRPIX1/2` = `ReferenceImageCoordinates` + 0.5 on both axes
//!   (PixInsight pixel centers sit at k + 0.5 in 0-based coords; FITS pixel
//!   centers sit at integer 1-based coords),
//! - `CD1_1..CD2_2` = `LinearTransformationMatrix` exactly as stored,
//! - `CTYPE1/2` = `RA---`/`DEC--` + projection code (Gnomonic → `TAN`).
//!
//! These conventions were verified empirically against real PixInsight
//! output in the sibling MMM project (see mmm-core `astrometry.rs` for the
//! full verification record).
//!
//! **Limitation:** if a solution also carries a distortion model
//! (`PCL:AstrometricSolution:SplineWorldTransformation`), only the linear
//! part is extracted — `LinearTransformationMatrix` is PixInsight's own
//! linear approximation around the reference point, so residuals grow away
//! from it. Distortion support is a planned sidecar schema extension.
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): linear WCS model + extraction from XISF properties"
```

---

### Task 5: WCS sidecar schema v1 (`wcs/sidecar.rs`)

**Files:**
- Create: `crates/mpp-core/src/wcs/sidecar.rs`
- Modify: `crates/mpp-core/src/wcs/mod.rs` (add `pub mod sidecar;`)
- Modify: `docs/DESIGN.md` (add schema section)

**Interfaces:**
- Consumes: `wcs::LinearWcs` (serde-derived, Task 4), `crate::{Error, Result}`.
- Produces:

```rust
pub const SCHEMA: &str = "mpp-wcs/1";
pub struct WcsSidecar {
    pub schema: String,
    pub width: u64,
    pub height: u64,
    pub wcs: LinearWcs,
}   // + Serialize, Deserialize, Debug, Clone, PartialEq
impl WcsSidecar {
    pub fn new(width: u64, height: u64, wcs: LinearWcs) -> Self;
    pub fn write(&self, path: &Path) -> Result<()>;
    pub fn read(path: &Path) -> Result<Self>;
}
pub fn sidecar_path(output: &Path) -> PathBuf;
```

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/wcs/sidecar.rs` with just:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wcs::LinearWcs;
    use std::path::Path;

    fn wcs() -> LinearWcs {
        LinearWcs {
            crval: [84.199, -3.240],
            crpix: [100.5, 200.5],
            cd: [[-4.4e-4, 0.0], [0.0, 4.4e-4]],
            ctype: ["RA---TAN".into(), "DEC--TAN".into()],
            radesys: "ICRS".into(),
        }
    }

    #[test]
    fn sidecar_path_replaces_last_extension() {
        assert_eq!(
            sidecar_path(Path::new("/out/mosaic.tif")),
            Path::new("/out/mosaic.wcs.json")
        );
        assert_eq!(
            sidecar_path(Path::new("m31.v2.tiff")),
            Path::new("m31.v2.wcs.json")
        );
        assert_eq!(sidecar_path(Path::new("bare")), Path::new("bare.wcs.json"));
    }

    #[test]
    fn json_shape_is_the_canonical_schema() {
        let s = WcsSidecar::new(16384, 12288, wcs());
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(v["schema"], "mpp-wcs/1");
        assert_eq!(v["width"], 16384);
        assert_eq!(v["height"], 12288);
        assert_eq!(v["wcs"]["crval"][0], 84.199);
        assert_eq!(v["wcs"]["ctype"][0], "RA---TAN");
        assert_eq!(v["wcs"]["cd"][1][1], 4.4e-4);
        assert_eq!(v["wcs"]["radesys"], "ICRS");
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mpp-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.wcs.json");
        let s = WcsSidecar::new(100, 50, wcs());
        s.write(&path).unwrap();
        assert_eq!(WcsSidecar::read(&path).unwrap(), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_rejects_unknown_schema() {
        let dir = std::env::temp_dir().join(format!("mpp-sidecar-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.wcs.json");
        let mut s = WcsSidecar::new(1, 1, wcs());
        s.schema = "mpp-wcs/99".into();
        s.write(&path).unwrap();
        assert!(WcsSidecar::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

Add `pub mod sidecar;` to `wcs/mod.rs`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mpp-core sidecar`
Expected: compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! The canonical WCS JSON sidecar (schema `mpp-wcs/1`).
//!
//! One schema used by every container that cannot carry an astrometric
//! solution natively (TIFF, PNG, DZI, …), and consumed by the DZI viewer.
//! Written next to the image as `<stem>.wcs.json`.
//!
//! # Schema v1
//!
//! ```json
//! {
//!   "schema": "mpp-wcs/1",
//!   "width": 16384,
//!   "height": 12288,
//!   "wcs": {
//!     "crval": [84.199, -3.240],
//!     "crpix": [8192.5, 6144.5],
//!     "cd": [[-4.4368e-4, 0.0], [0.0, 4.4368e-4]],
//!     "ctype": ["RA---TAN", "DEC--TAN"],
//!     "radesys": "ICRS"
//!   }
//! }
//! ```
//!
//! Conventions (fixed for schema v1):
//! - `crval` in degrees `[RA, Dec]`; `cd` in degrees/pixel, row 0 → axis 1.
//! - `crpix` is FITS-convention: 1-based, pixel centers at integers, y
//!   counted in *stored row order* (top-down) of the image the sidecar
//!   accompanies.
//! - `width`/`height` are the pixel geometry of that image, letting
//!   consumers detect a stale sidecar after a resize.
//! - Angular units are always degrees; no `cunit` field.
//! - The `schema` string is a major version: readers must reject majors
//!   they do not know. Additive fields (e.g. a future distortion grid) do
//!   not bump the major.

use std::path::{Path, PathBuf};

use crate::wcs::LinearWcs;
use crate::{Error, Result};

/// Schema identifier written to and required from every v1 sidecar.
pub const SCHEMA: &str = "mpp-wcs/1";

/// A parsed (or to-be-written) WCS sidecar file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WcsSidecar {
    /// Schema identifier; [`SCHEMA`] for files this version writes.
    pub schema: String,
    /// Width in pixels of the image this sidecar describes.
    pub width: u64,
    /// Height in pixels of the image this sidecar describes.
    pub height: u64,
    /// The astrometric solution.
    pub wcs: LinearWcs,
}

impl WcsSidecar {
    /// Build a v1 sidecar for an image of the given pixel geometry.
    pub fn new(width: u64, height: u64, wcs: LinearWcs) -> Self {
        Self { schema: SCHEMA.to_string(), width, height, wcs }
    }

    /// Serialize to pretty JSON at `path` (overwrites).
    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::format(path, format!("sidecar serialize: {e}")))?;
        std::fs::write(path, json).map_err(|e| Error::io(path, e))
    }

    /// Read and validate a sidecar; errors on unknown schema versions.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let s: Self = serde_json::from_str(&text)
            .map_err(|e| Error::format(path, format!("sidecar parse: {e}")))?;
        if s.schema != SCHEMA {
            return Err(Error::format(
                path,
                format!("unknown sidecar schema '{}' (expected '{SCHEMA}')", s.schema),
            ));
        }
        Ok(s)
    }
}

/// The sidecar path for an output image: the last extension is replaced
/// with `wcs.json` (`mosaic.tif` → `mosaic.wcs.json`).
pub fn sidecar_path(output: &Path) -> PathBuf {
    output.with_extension("wcs.json")
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Record the schema in DESIGN.md**

Add a new section to `docs/DESIGN.md` after "## Architecture decisions":

```markdown
## WCS sidecar schema (v1, decided 2026-08-20)

Canonical JSON, written as `<output-stem>.wcs.json` beside any container
that cannot carry the solution natively; consumed by the DZI viewer.

```json
{
  "schema": "mpp-wcs/1",
  "width": 16384,
  "height": 12288,
  "wcs": {
    "crval": [84.199, -3.240],
    "crpix": [8192.5, 6144.5],
    "cd": [[-4.4368e-4, 0.0], [0.0, 4.4368e-4]],
    "ctype": ["RA---TAN", "DEC--TAN"],
    "radesys": "ICRS"
  }
}
```

Degrees everywhere; `crpix` is FITS-convention (1-based, pixel centers at
integers) against the stored top-down rows; `width`/`height` pin the
geometry so consumers can detect stale sidecars. `schema` is a major
version — readers reject unknown majors; additive fields (future
distortion grids) do not bump it. Source of truth:
`mpp-core::wcs::sidecar` module docs.
```

- [ ] **Step 6: Commit**

```bash
git add crates/mpp-core docs/DESIGN.md
git commit -m "feat(core): canonical WCS JSON sidecar (schema mpp-wcs/1)"
```

---

### Task 6: Streaming BigTIFF writer (`formats/tiff.rs`)

**Files:**
- Create: `crates/mpp-core/src/formats/tiff.rs`
- Create: `crates/mpp-core/src/testsupport.rs`
- Modify: `crates/mpp-core/src/formats/mod.rs` (add `pub mod tiff;`)
- Modify: `crates/mpp-core/src/lib.rs` (add `#[cfg(test)] mod testsupport;`)

**Interfaces:**
- Consumes: `formats::xisf::XisfImage`, `crate::{Error, Result}`.
- Produces:

```rust
pub enum BitDepth { F32, U16 }          // + Debug, Clone, Copy, PartialEq, Eq, Default (F32)
pub struct TiffWriteOptions { pub bit_depth: BitDepth }  // + Debug, Clone, Default
pub fn write_bigtiff(img: &XisfImage, path: &Path, opts: &TiffWriteOptions) -> Result<()>;
```

- Produces (test-only): `testsupport::synth_xisf(path: &Path, w: u64, h: u64, c: u64, with_wcs: bool)` — writes a synthetic monolithic XISF; sample value at plane `k`, index `i` is `(k * w * h + i) as f32 / (w * h * c) as f32` (all values in `[0, 1)`); when `with_wcs`, embeds a full PixInsight astrometric solution property set.

- [ ] **Step 1: Write the test-support synthesizer**

Create `crates/mpp-core/src/testsupport.rs`:

```rust
//! Test-only builders for synthetic input files (tests never depend on
//! real image data — see CLAUDE.md).

use std::path::Path;

/// Base64-encode f64s little-endian, mirroring PixInsight's
/// `location="inline:base64"` property payloads.
fn b64_f64s(data: &[f64]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
    let mut s = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(ALPHA[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Write a minimal monolithic XISF file: `w`×`h`, `c` planar Float32
/// channels; sample at plane `k`, index `i` is `(k*w*h + i) / (w*h*c)`.
/// With `with_wcs`, a full PixInsight astrometric-solution property set is
/// embedded (Gnomonic, ICRS, crval [84.199, -3.240], refimg [w/2, h/2],
/// diagonal matrix ±4.4368e-4 deg/px).
pub(crate) fn synth_xisf(path: &Path, w: u64, h: u64, c: u64, with_wcs: bool) {
    let data_offset = 4096u64;
    let data_size = w * h * c * 4;
    let props = if with_wcs {
        let refimg = [w as f64 / 2.0, h as f64 / 2.0];
        format!(
            concat!(
                r#"<Property id="PCL:AstrometricSolution:ReferenceCelestialCoordinates" type="F64Vector" length="2" location="inline:base64">{crval}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:ReferenceImageCoordinates" type="F64Vector" length="2" location="inline:base64">{refimg}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:LinearTransformationMatrix" type="F64Matrix" rows="2" columns="2" location="inline:base64">{mat}</Property>"#,
                r#"<Property id="PCL:AstrometricSolution:ProjectionSystem" type="String" value="Gnomonic"/>"#,
                r#"<Property id="Observation:CelestialReferenceSystem" type="String" value="ICRS"/>"#,
            ),
            crval = b64_f64s(&[84.199, -3.240]),
            refimg = b64_f64s(&refimg),
            mat = b64_f64s(&[-4.4368e-4, 0.0, 0.0, 4.4368e-4]),
        )
    } else {
        String::new()
    };
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="{w}:{h}:{c}" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}">"#,
            "{props}",
            r#"</Image></xisf>"#,
        ),
        w = w, h = h, c = c, off = data_offset, size = data_size, props = props,
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"XISF0100");
    bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(xml.as_bytes());
    assert!(bytes.len() <= data_offset as usize, "header must fit below data");
    bytes.resize(data_offset as usize, 0);
    let total = (w * h * c) as f32;
    for i in 0..(w * h * c) {
        bytes.extend_from_slice(&(i as f32 / total).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}
```

Add to `lib.rs`: `#[cfg(test)] mod testsupport;`

- [ ] **Step 2: Write failing tests for the writer**

Create `crates/mpp-core/src/formats/tiff.rs` with just:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::xisf::XisfImage;
    use crate::testsupport::synth_xisf;
    use std::path::PathBuf;
    use tiff::decoder::{Decoder, DecodingResult};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-tiff-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_bigtiff_gray_f32() {
        let dir = tmpdir("gray");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 5, 4, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        // BigTIFF magic: little-endian "II", version 43.
        let head = std::fs::read(&out).unwrap();
        assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00]);

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (5, 4));
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 20);
                assert_eq!(data[0], 0.0);
                assert!((data[19] - 19.0 / 20.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_bigtiff_rgb_interleaved() {
        let dir = tmpdir("rgb");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 3, 2, 3, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                // Planar XISF (plane size 6) interleaves to RGBRGB…:
                // pixel 0 = planes' sample 0 = [0, 6, 12] / 18.
                assert_eq!(data.len(), 18);
                assert!((data[0] - 0.0 / 18.0).abs() < 1e-7);
                assert!((data[1] - 6.0 / 18.0).abs() < 1e-7);
                assert!((data[2] - 12.0 / 18.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u16_output_scales_unit_range() {
        let dir = tmpdir("u16");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 4, 4, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        let opts = TiffWriteOptions { bit_depth: BitDepth::U16 };
        write_bigtiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(data) => {
                assert_eq!(data[0], 0);
                // sample 15 = 15/16 → round(15/16 * 65535) = 61439.
                assert_eq!(data[15], 61439);
            }
            other => panic!("expected U16 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn strip_streaming_covers_multi_strip_images() {
        // Small strip target is not configurable; instead verify a taller
        // image (which will span multiple strips at any rows_per_strip)
        // roundtrips exactly.
        let dir = tmpdir("strips");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 3, 64, 1, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        write_bigtiff(&img, &out, &TiffWriteOptions::default()).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                let total = (3 * 64) as f32;
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / total).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unsupported_channel_count() {
        let dir = tmpdir("chans");
        let src = dir.join("in.xisf");
        synth_xisf(&src, 2, 2, 2, false);
        let img = XisfImage::open(&src).unwrap();
        let out = dir.join("out.tif");
        assert!(write_bigtiff(&img, &out, &TiffWriteOptions::default()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

Add `pub mod tiff;` to `formats/mod.rs`.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p mpp-core tiff`
Expected: compile FAIL — writer not defined.

- [ ] **Step 4: Implement the writer**

Above the tests in `formats/tiff.rs`:

```rust
//! Strip-streaming BigTIFF writer.
//!
//! Always writes BigTIFF (the 64-bit TIFF variant): the whole point of the
//! converter is escaping classic TIFF's 4 GB limit, and every consumer we
//! target (Photoshop, libvips, libtiff ≥ 4.x) reads BigTIFF. Pixel data is
//! interleaved from the reader's planar planes strip by strip — memory use
//! is one strip (~[`TARGET_STRIP_BYTES`]), independent of image size.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use tiff::encoder::{colortype, TiffEncoder};

use crate::formats::xisf::XisfImage;
use crate::{Error, Result};

/// Target uncompressed strip size; rows per strip is derived from this.
const TARGET_STRIP_BYTES: u64 = 4 << 20;

/// Output sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    /// 32-bit IEEE float samples, passthrough from XISF Float32 (default).
    #[default]
    F32,
    /// 16-bit unsigned samples; input clamped to [0, 1] and scaled.
    U16,
}

/// Options for [`write_bigtiff`].
#[derive(Debug, Clone, Default)]
pub struct TiffWriteOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
}

/// Convert a unit-range f32 sample to u16 (clamped, rounded).
fn to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// Write `img` to `path` as an uncompressed BigTIFF, streaming strip by
/// strip in constant memory. Grayscale (1 channel) and RGB (3 channels)
/// are supported; XISF planar planes are interleaved to TIFF chunky order.
pub fn write_bigtiff(img: &XisfImage, path: &Path, opts: &TiffWriteOptions) -> Result<()> {
    let w = u32::try_from(img.width())
        .map_err(|_| Error::format(path, "image width exceeds TIFF limits"))?;
    let h = u32::try_from(img.height())
        .map_err(|_| Error::format(path, "image height exceeds TIFF limits"))?;
    let channels = img.channels();
    let terr = |e: tiff::TiffError| Error::format(path, format!("TIFF encode: {e}"));

    let file = File::create(path).map_err(|e| Error::io(path, e))?;
    let mut buf = BufWriter::new(file);
    let mut enc = TiffEncoder::new_big(&mut buf).map_err(terr)?;

    // One code path per (channels, depth) combination; the macro exists
    // because each combination needs a different tiff colortype marker
    // type, which cannot be chosen at runtime.
    macro_rules! stream {
        ($ct:ty, $sample:ty, $map:expr) => {{
            let mut image = enc.new_image::<$ct>(w, h).map_err(terr)?;
            let row_bytes = w as u64 * channels * std::mem::size_of::<$sample>() as u64;
            let rows = (TARGET_STRIP_BYTES / row_bytes.max(1)).clamp(1, h.max(1) as u64) as u32;
            image.rows_per_strip(rows).map_err(terr)?;
            let mut strip: Vec<$sample> = Vec::new();
            let mut y: u64 = 0;
            while image.next_strip_sample_count() > 0 {
                let n = image.next_strip_sample_count() as usize;
                let rows_here = n / (w as usize * channels as usize);
                strip.clear();
                for yy in y..y + rows_here as u64 {
                    let planes: Vec<&[f32]> = (0..channels).map(|c| img.row(c, yy)).collect();
                    for x in 0..w as usize {
                        for p in &planes {
                            strip.push($map(p[x]));
                        }
                    }
                }
                image.write_strip(&strip).map_err(terr)?;
                y += rows_here as u64;
            }
            image.finish().map_err(terr)?;
        }};
    }

    match (channels, opts.bit_depth) {
        (1, BitDepth::F32) => stream!(colortype::Gray32Float, f32, |v: f32| v),
        (3, BitDepth::F32) => stream!(colortype::RGB32Float, f32, |v: f32| v),
        (1, BitDepth::U16) => stream!(colortype::Gray16, u16, to_u16),
        (3, BitDepth::U16) => stream!(colortype::RGB16, u16, to_u16),
        (c, _) => {
            return Err(Error::format(
                path,
                format!("unsupported channel count {c} (grayscale or RGB only)"),
            ));
        }
    }
    Ok(())
}
```

If the compiler objects to details (e.g. `TiffError` path, `new_image` turbofish, decoder variant names), fix against the real `tiff` 0.11 API — the shapes above were checked against its docs but bounds may need small adjustments.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): strip-streaming BigTIFF writer (f32/u16, gray/RGB)"
```

---

### Task 7: Convert orchestration (`convert.rs`)

**Files:**
- Create: `crates/mpp-core/src/convert.rs`
- Modify: `crates/mpp-core/src/lib.rs` (add `pub mod convert;`, refresh crate docs)

**Interfaces:**
- Consumes: `formats::xisf::XisfImage`, `formats::tiff::{write_bigtiff, TiffWriteOptions, BitDepth}`, `wcs::from_xisf_properties`, `wcs::sidecar::{WcsSidecar, sidecar_path}`, `crate::{Error, Result}`.
- Produces:

```rust
pub struct ConvertOptions { pub bit_depth: BitDepth, pub sidecar: bool }  // Default: F32, true
pub struct ConvertReport {
    pub width: u64,
    pub height: u64,
    pub channels: u64,
    pub wcs_found: bool,
    pub sidecar: Option<PathBuf>,
}
pub fn convert(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<ConvertReport>;
```

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/convert.rs` with just:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::synth_xisf;
    use crate::wcs::sidecar::WcsSidecar;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-convert-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn converts_and_writes_sidecar_when_solved() {
        let dir = tmpdir("solved");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 6, 4, 1, true);
        let output = dir.join("out.tif");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();

        assert_eq!((report.width, report.height, report.channels), (6, 4, 1));
        assert!(report.wcs_found);
        let sc_path = report.sidecar.expect("sidecar written");
        assert_eq!(sc_path, dir.join("out.wcs.json"));
        let sc = WcsSidecar::read(&sc_path).unwrap();
        assert_eq!((sc.width, sc.height), (6, 4));
        // refimg [3, 2] + 0.5 pixel-convention shift.
        assert_eq!(sc.wcs.crpix, [3.5, 2.5]);
        assert!(output.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_solution_means_no_sidecar_but_conversion_succeeds() {
        let dir = tmpdir("unsolved");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 4, 4, 1, false);
        let output = dir.join("out.tif");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();
        assert!(!report.wcs_found);
        assert!(report.sidecar.is_none());
        assert!(!dir.join("out.wcs.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sidecar_can_be_disabled() {
        let dir = tmpdir("nosidecar");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 4, 4, 1, true);
        let output = dir.join("out.tif");
        let opts = ConvertOptions { sidecar: false, ..Default::default() };
        let report = convert(&input, &output, &opts).unwrap();
        assert!(report.wcs_found, "solution still detected");
        assert!(report.sidecar.is_none());
        assert!(!dir.join("out.wcs.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unknown_extensions() {
        let dir = tmpdir("ext");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        assert!(convert(&input, &dir.join("out.png"), &ConvertOptions::default()).is_err());
        assert!(convert(&dir.join("in.fits"), &dir.join("out.tif"), &ConvertOptions::default())
            .is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

Add `pub mod convert;` to `lib.rs`, and replace the "Intended public API surface (nothing implemented yet…)" crate-docs block with:

```rust
//! Public API surface:
//!
//! - [`formats`] — image readers/writers: native XISF reading and
//!   strip-streaming BigTIFF writing today; libvips-backed formats and the
//!   DZI pipeline arrive with later phase-1 plans.
//! - [`wcs`] — astrometric solutions: the linear WCS model, extraction
//!   from PixInsight XISF properties, and the canonical JSON sidecar.
//! - [`convert`] — one-call conversion orchestration used by the CLI.
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mpp-core convert`
Expected: compile FAIL.

- [ ] **Step 3: Implement**

```rust
//! One-call conversion: open input, write output, emit the WCS sidecar.

use std::path::{Path, PathBuf};

use crate::formats::tiff::{write_bigtiff, BitDepth, TiffWriteOptions};
use crate::formats::xisf::XisfImage;
use crate::wcs::sidecar::{sidecar_path, WcsSidecar};
use crate::{Error, Result};

/// Options for [`convert`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
    /// Write the WCS JSON sidecar when the input carries a solution.
    pub sidecar: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { bit_depth: BitDepth::F32, sidecar: true }
    }
}

/// What a conversion did, for CLI reporting.
#[derive(Debug, Clone)]
pub struct ConvertReport {
    /// Input/output width in pixels.
    pub width: u64,
    /// Input/output height in pixels.
    pub height: u64,
    /// Channel count.
    pub channels: u64,
    /// Whether the input carried an extractable astrometric solution.
    pub wcs_found: bool,
    /// Path of the sidecar written, if any.
    pub sidecar: Option<PathBuf>,
}

/// Case-insensitive extension of `path`, lowercased ("" when absent).
fn ext(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Convert `input` (XISF) to `output` (BigTIFF, chosen by `.tif`/`.tiff`
/// extension), writing the WCS sidecar next to the output when the input
/// carries an astrometric solution and `opts.sidecar` is on.
pub fn convert(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<ConvertReport> {
    if ext(input) != "xisf" {
        return Err(Error::format(
            input,
            "unsupported input format (XISF only in this phase)",
        ));
    }
    if !matches!(ext(output).as_str(), "tif" | "tiff") {
        return Err(Error::format(
            output,
            "unsupported output format (BigTIFF .tif/.tiff only in this phase)",
        ));
    }

    let img = XisfImage::open(input)?;
    img.advise_sequential();
    write_bigtiff(&img, output, &TiffWriteOptions { bit_depth: opts.bit_depth })?;

    let wcs = crate::wcs::from_xisf_properties(&img.header().properties);
    let wcs_found = wcs.is_some();
    let mut sidecar = None;
    if opts.sidecar && let Some(wcs) = wcs {
        let sc = WcsSidecar::new(img.width(), img.height(), wcs);
        let path = sidecar_path(output);
        sc.write(&path)?;
        sidecar = Some(path);
    }

    Ok(ConvertReport {
        width: img.width(),
        height: img.height(),
        channels: img.channels(),
        wcs_found,
        sidecar,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): convert orchestration (XISF -> BigTIFF + sidecar)"
```

---

### Task 8: CLI `convert` subcommand

**Files:**
- Modify: `crates/mpp/src/main.rs`
- Modify: `crates/mpp/Cargo.toml` (add `anyhow.workspace = true`)
- Create: `crates/mpp/tests/convert.rs`

**Interfaces:**
- Consumes: `mpp_core::convert::{convert, ConvertOptions}`, `mpp_core::formats::tiff::BitDepth`.
- Produces: `mpp convert <INPUT> <OUTPUT> [--bit-depth f32|u16] [--no-sidecar]`; exit 0 on success with a human summary on stdout; nonzero with the error on stderr on failure.

- [ ] **Step 1: Write failing CLI integration test**

Create `crates/mpp/tests/convert.rs`:

```rust
//! End-to-end CLI test: synthesize a tiny XISF, run the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimal monolithic gray Float32 XISF (no WCS) — duplicated from
/// mpp-core's test support because that helper is `cfg(test)`-private.
fn synth_xisf(path: &Path, w: u64, h: u64) {
    let data_offset = 512u64;
    let data_size = w * h * 4;
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="{w}:{h}:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}"/>"#,
            r#"</xisf>"#,
        ),
        w = w, h = h, off = data_offset, size = data_size,
    );
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"XISF0100");
    bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(xml.as_bytes());
    bytes.resize(data_offset as usize, 0);
    for i in 0..(w * h) {
        bytes.extend_from_slice(&(i as f32 / (w * h) as f32).to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mpp-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn convert_produces_bigtiff() {
    let dir = tmpdir("ok");
    let input = dir.join("in.xisf");
    synth_xisf(&input, 8, 8);
    let output = dir.join("out.tif");

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let head = std::fs::read(&output).unwrap();
    assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_missing_input_fails_with_message() {
    let dir = tmpdir("missing");
    let out = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(dir.join("nope.xisf"))
        .arg(dir.join("out.tif"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty(), "error goes to stderr");
    std::fs::remove_dir_all(&dir).unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p mpp`
Expected: FAIL — the binary has no `convert` subcommand yet (clap errors, nonzero exit in the first test).

- [ ] **Step 3: Implement the subcommand**

Add `anyhow.workspace = true` to `crates/mpp/Cargo.toml` dependencies. Replace `crates/mpp/src/main.rs` with:

```rust
//! Mega Pixel Processor CLI — thin frontend over `mpp-core`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use mpp_core::convert::{convert, ConvertOptions};
use mpp_core::formats::tiff::BitDepth;

/// Mega Pixel Processor: big astro image conversion and DZI publishing.
#[derive(Parser)]
#[command(name = "mpp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert an image between formats (XISF -> BigTIFF), carrying the
    /// astrometric solution to a WCS JSON sidecar.
    Convert {
        /// Input image (.xisf).
        input: PathBuf,
        /// Output image (.tif/.tiff, written as BigTIFF).
        output: PathBuf,
        /// Output sample format.
        #[arg(long, value_enum, default_value_t = BitDepthArg::F32)]
        bit_depth: BitDepthArg,
        /// Do not write the WCS sidecar even when the input is solved.
        #[arg(long)]
        no_sidecar: bool,
    },
}

/// CLI mirror of [`BitDepth`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, ValueEnum)]
enum BitDepthArg {
    /// 32-bit float samples (lossless from XISF Float32).
    F32,
    /// 16-bit unsigned samples (clamped to [0, 1], scaled).
    U16,
}

impl From<BitDepthArg> for BitDepth {
    fn from(v: BitDepthArg) -> Self {
        match v {
            BitDepthArg::F32 => BitDepth::F32,
            BitDepthArg::U16 => BitDepth::U16,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Convert { input, output, bit_depth, no_sidecar } => {
            let opts = ConvertOptions {
                bit_depth: bit_depth.into(),
                sidecar: !no_sidecar,
            };
            let report = convert(&input, &output, &opts)?;
            println!(
                "converted {}x{}x{} {} -> {}",
                report.width,
                report.height,
                report.channels,
                input.display(),
                output.display()
            );
            match (&report.sidecar, report.wcs_found) {
                (Some(p), _) => println!("wrote WCS sidecar {}", p.display()),
                (None, true) => println!("WCS sidecar disabled (--no-sidecar)"),
                (None, false) => println!("no astrometric solution found; no sidecar"),
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p mpp && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp
git commit -m "feat(cli): convert subcommand (XISF -> BigTIFF + WCS sidecar)"
```

---

### Task 9: >4 GB smoke test + docs

**Files:**
- Create: `crates/mpp/tests/big_smoke.rs`
- Modify: `README.md` (add smoke-test instructions)

**Interfaces:**
- Consumes: the `mpp` binary (`CARGO_BIN_EXE_mpp`).
- Produces: an `#[ignore]`d test proving the >4 GB path end-to-end; run manually with `cargo test -p mpp --release -- --ignored`.

- [ ] **Step 1: Write the ignored big test**

Create `crates/mpp/tests/big_smoke.rs`:

```rust
//! Manual smoke test for the phase-1 acceptance criterion: a >4 GB
//! BigTIFF. Ignored by default — it writes ~9 GB of temporary data.
//! Run: `cargo test -p mpp --release -- --ignored`
//! The remaining manual step is opening the produced file in Photoshop.

use std::io::{BufWriter, Write};
use std::process::Command;

/// Stream a gray Float32 XISF of `w`×`h` to disk without holding it in
/// memory (the pixel block alone is w*h*4 bytes).
fn synth_big_xisf(path: &std::path::Path, w: u64, h: u64) {
    let data_offset = 4096u64;
    let data_size = w * h * 4;
    let xml = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<xisf version="1.0" xmlns="http://www.pixinsight.com/xisf">"#,
            r#"<Image geometry="{w}:{h}:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:{off}:{size}"/>"#,
            r#"</xisf>"#,
        ),
        w = w, h = h, off = data_offset, size = data_size,
    );
    let file = std::fs::File::create(path).unwrap();
    let mut out = BufWriter::with_capacity(1 << 20, file);
    out.write_all(b"XISF0100").unwrap();
    out.write_all(&(xml.len() as u32).to_le_bytes()).unwrap();
    out.write_all(&0u32.to_le_bytes()).unwrap();
    out.write_all(xml.as_bytes()).unwrap();
    let written = 16 + xml.len() as u64;
    for _ in written..data_offset {
        out.write_all(&[0]).unwrap();
    }
    // A cheap deterministic pattern; one row buffer reused.
    let mut row = vec![0u8; (w * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let v = ((x ^ y) & 0xFFFF) as f32 / 65535.0;
            row[(x * 4) as usize..(x * 4 + 4) as usize].copy_from_slice(&v.to_le_bytes());
        }
        out.write_all(&row).unwrap();
    }
    out.flush().unwrap();
}

#[test]
#[ignore = "writes ~9 GB of temp data; run manually with --ignored"]
fn bigtiff_over_4gb_end_to_end() {
    // 33000^2 * 4 bytes = 4.356e9 > 4 GiB (4.295e9).
    let (w, h) = (33_000u64, 33_000u64);
    let dir = std::env::temp_dir().join(format!("mpp-big-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("big.xisf");
    let output = dir.join("big.tif");
    synth_big_xisf(&input, w, h);

    let status = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["convert"])
        .arg(&input)
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());

    let meta = std::fs::metadata(&output).unwrap();
    assert!(
        meta.len() > 4 * 1024 * 1024 * 1024,
        "output is {} bytes, expected > 4 GiB",
        meta.len()
    );
    // 4-byte read — never read the whole >4 GB file for the magic check.
    use std::io::Read;
    let mut head = [0u8; 4];
    std::fs::File::open(&output).unwrap().read_exact(&mut head).unwrap();
    assert_eq!(head, [0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");
    std::fs::remove_dir_all(&dir).unwrap();
}
```

- [ ] **Step 2: Verify it is ignored by default, then run it for real**

Run: `cargo test -p mpp` — expected: the new test is listed as `ignored`, everything else passes.
Run: `cargo test -p mpp --release -- --ignored` — expected: PASS in a few minutes (needs ~9 GB free in the temp dir; if the machine lacks that, note it and rely on the real-data manual run).

- [ ] **Step 3: Document the smoke test in README.md**

Add to `README.md` (create a "## Development" section if absent):

```markdown
## Smoke tests

The phase-1 acceptance criterion is a >4 GB BigTIFF that opens in
Photoshop (PS ≥ 26.2 also saves BigTIFF; opening support is older).

- Synthetic end-to-end: `cargo test -p mpp --release -- --ignored`
  (writes ~9 GB of temporary data).
- Real data (manual): `cargo build --release`, then
  `target/release/mpp convert <big>.xisf <big>.tif` on a real mosaic and
  open the result in Photoshop. A solved input also writes
  `<big>.wcs.json` next to the output.
```

- [ ] **Step 4: Full workspace check**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: all green, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp README.md
git commit -m "test: >4GB BigTIFF smoke test; document phase-1 smoke procedure"
```

---

## Out of scope for this plan (later phase-1 plans)

- Mainstream formats via spawned `vips` (PNG/JPEG/WebP/AVIF/EXR, classic TIFF) — needs `libvips-tools` installed.
- DZI export + Astrometrical-branded OpenSeadragon HTML viewer (+ universal viewer mode).
- FITS read/write evaluation (vips/cfitsio vs. native).
- Distortion (spline) solutions in the sidecar schema (additive extension).
- XISF sample formats beyond Float32, compressed blocks, TIFF compression options.
