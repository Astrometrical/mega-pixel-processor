# TIFF Parity (Phase 4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the Mega Pixel Processor PixInsight module to parity with the stock TIFF module it now replaces — a full options dialog, real `u32`/`f64` output, compression, classic-TIFF output, alpha, embedded metadata — plus tiled output for the Astrometrical large-image load path.

**Architecture:** Replace `mpp-core`'s use of the `tiff` crate's `ImageEncoder` with our own IFD-level writer built on the crate's public `DirectoryEncoder` and compressors, generic over container kind and sample type; widen the IPC protocol to v3 so write bands cross shared memory in their native sample type; and add a PCL `Dialog` subclass, styled after Mega Merge Mosaic's interface, wired through `QueryOptions()` and format-specific data.

**Tech Stack:** Rust 2024 (`mpp-core`, `mpp`, `mpp-ipc`, `mpp-ipc-worker`), `tiff` 0.11.3, C++17 host library, C++20 PCL module, CMake + CTest, clap 4.

**Spec:** `docs/superpowers/specs/2026-09-09-tiff-parity-design.md` — read it before starting. It records the three findings that drove this shape and the non-goals that keep it bounded.

## Global Constraints

- **Clean-room rule.** Never read or derive from PixInsight's PCL module implementations (their TIFF module above all). Design only from the public TIFF 6.0/BigTIFF specs, the `tiff` crate, the public PCL headers, `integration/pixinsight/PCL_FILEFORMAT_API.md`, and MergeMosaic's own module code. See `docs/DESIGN.md` "Licensing constraints".
- **Tests synthesize their inputs.** Nothing under `test_data/` may be a test dependency. Real-data runs are manual smoke tests only.
- **Every public item in `mpp-core` gets a doc comment.** `missing_docs` is warned; treat warnings as failures.
- **Warning-free gates.** `cargo fmt --check`, `cargo clippy --all-targets`, and `cargo doc` must stay clean. The host library builds `-Wall -Wextra -Werror` as C++17; the module builds as C++20.
- **PixInsight floor is 1.9.0.** Do not use PCL API newer than the pinned 2.8.3 / API 0x0182 headers for x64. Pins live in `integration/pixinsight/ci/pcl-pin.env`.
- **PCL header location.** `$HOME/.local/pcl-build-0182/include` and `.../lib`. Never `/opt` headers.
- **Truth-in-advertising.** A `MetaFileFormat` capability may return `true` only if the shim plus worker genuinely do the thing. This plan exists largely because that rule was violated.
- **Byte order.** The `tiff` crate writes the file's byte-order mark from `target_endian`. All sample bytes we emit use `to_ne_bytes`, never `to_le_bytes`.
- **Commit after every task.** Conventional-commit subjects, and end every commit message with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

## File Structure

**Created:**

| Path | Responsibility |
|---|---|
| `crates/mpp-core/src/formats/tiff_write/mod.rs` | Public writer entry point `write_tiff`, container dispatch |
| `crates/mpp-core/src/formats/tiff_write/options.rs` | `TiffWriteOptions` and every option enum, plus validation |
| `crates/mpp-core/src/formats/tiff_write/sample.rs` | The `Sample` trait and its five impls |
| `crates/mpp-core/src/formats/tiff_write/encode.rs` | The generic strip/tile encode loop and tag emission |
| `integration/pixinsight/module/MppIcon.h` | Astrometrical chevron SVG for the dialog header |
| `integration/pixinsight/module/TiffOptionsDialog.h/.cpp` | The branded options dialog |
| `integration/pixinsight/module/TiffFormatOptions.h` | The POD format-specific data block |
| `integration/pixinsight/host/test/test_v3_init.cpp` | Golden test for the v3 `Init` encoding |

**Modified:**

| Path | Change |
|---|---|
| `crates/mpp-core/src/formats/tiff.rs` | Reduced to a re-export shim, then deleted in Task 3 |
| `crates/mpp-core/src/formats/mod.rs` | `PlanarRows` becomes generic; module list updated |
| `crates/mpp-core/src/formats/xisf.rs` | `impl PlanarRows<f32> for XisfImage` |
| `crates/mpp-core/src/formats/tiff_read.rs` | Palette/YCbCr/planar acceptance, ICC readback, error text |
| `crates/mpp-core/src/convert.rs`, `publish.rs` | Call sites for the widened options |
| `crates/mpp/src/main.rs` | New CLI flags and clap mirror enums |
| `crates/mpp-ipc/src/protocol.rs` | Protocol v3 `InitMessage` fields |
| `crates/mpp-ipc-worker/src/staging.rs` | `Staging<T>` generic over sample type |
| `crates/mpp-ipc-worker/src/main.rs` | `band_format` dispatch, widened validation, option plumbing |
| `crates/mpp-ipc-worker/src/read.rs` | ICC bytes in `ImageMeta` |
| `integration/pixinsight/host/mpp_protocol.h/.cpp` | v3 `InitMessage` fields and encoding |
| `integration/pixinsight/host/mpp_host.h/.cpp` | `WriteSessionConfig` fields, `PushBandBytes` |
| `integration/pixinsight/module/BigTiffFormat.h/.cpp` | Capability corrections |
| `integration/pixinsight/module/BigTiffInstance.h/.cpp` | `QueryOptions`, native pushes, ICC, identity mapping |
| `docs/pi-protocol.md`, `docs/DESIGN.md`, `docs/tiff-parity.md`, `integration/pixinsight/README.md` | Documentation |

## Task Groups

- **A (Tasks 1–8)** — the `mpp-core` writer. Fully machine-verifiable; nothing downstream starts until it is green.
- **B (Task 9)** — CLI surface.
- **C (Tasks 10–12)** — protocol v3 across Rust and the C++ host library.
- **D (Tasks 13–14)** — read-side widening.
- **E (Tasks 15–18)** — the PixInsight module, dialog, and documentation.

---

## Group A — the `mpp-core` writer

### Task 1: Widen `BitDepth` and make `PlanarRows` generic

Mechanical groundwork. `BitDepth` gains `U32` and `F64`; `PlanarRows` gains a sample-type parameter so the worker can later stage `u32` or `f64` planes. Nothing changes behaviourally.

**Files:**
- Modify: `crates/mpp-core/src/formats/mod.rs` (the `PlanarRows` trait, lines 11-21)
- Modify: `crates/mpp-core/src/formats/tiff.rs` (the `BitDepth` enum, lines 21-31)
- Modify: `crates/mpp-core/src/formats/xisf.rs:161` (the impl)
- Modify: `crates/mpp-core/src/convert.rs` (`default_bit_depth`, `allows`, `saver_options`)
- Modify: `crates/mpp-ipc-worker/src/staging.rs:233` (the impl)
- Test: `crates/mpp-core/src/convert.rs` (existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `BitDepth::{U8, U16, U32, F32, F64}`; `trait PlanarRows<T>` with `fn row(&self, c: u64, y: u64) -> &[T]`; `impl PlanarRows<f32> for XisfImage`.

- [ ] **Step 1: Write the failing test**

Add to `crates/mpp-core/src/convert.rs`'s test module:

```rust
#[test]
fn vips_formats_reject_wide_sample_formats() {
    use crate::formats::tiff::BitDepth;
    // BigTIFF is the only output that can carry u32 or f64 samples; the
    // vips-backed formats top out at u16, so widening BitDepth must not
    // silently make `--bit-depth f64 --format png` legal.
    assert!(OutputFormat::BigTiff.allows(BitDepth::U32));
    assert!(OutputFormat::BigTiff.allows(BitDepth::F64));
    assert!(!OutputFormat::Png.allows(BitDepth::U32));
    assert!(!OutputFormat::Png.allows(BitDepth::F64));
    assert!(!OutputFormat::Jpeg.allows(BitDepth::U32));
    assert!(!OutputFormat::Jxl.allows(BitDepth::F64));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p mpp-core vips_formats_reject_wide_sample_formats`
Expected: FAIL to compile — `no variant named U32 found for enum BitDepth`.

- [ ] **Step 3: Widen `BitDepth`**

In `crates/mpp-core/src/formats/tiff.rs`, replace the enum with:

```rust
/// Output sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    /// 32-bit IEEE float samples, passthrough from XISF Float32 (default).
    #[default]
    F32,
    /// 64-bit IEEE float samples. Exact for `pcl::DImage` input; from a
    /// unit-range f32 source it simply widens.
    F64,
    /// 32-bit unsigned samples; unit-range input is clamped and scaled.
    U32,
    /// 16-bit unsigned samples; unit-range input is clamped and scaled.
    U16,
    /// 8-bit unsigned samples; unit-range input is clamped and scaled.
    U8,
}
```

- [ ] **Step 4: Update `OutputFormat` for the new variants**

In `crates/mpp-core/src/convert.rs`, `allows` must name the wide formats explicitly rather than relying on a catch-all:

```rust
    /// Whether this format can carry `depth`.
    pub fn allows(self, depth: BitDepth) -> bool {
        match self {
            // BigTIFF carries every sample format the writer supports.
            Self::BigTiff => true,
            // The vips-backed formats top out at 16-bit unsigned.
            Self::Png | Self::Jxl => matches!(depth, BitDepth::U16 | BitDepth::U8),
            Self::Jpeg | Self::Webp | Self::Avif => matches!(depth, BitDepth::U8),
        }
    }
```

Leave `default_bit_depth` and `saver_options` as they are: their existing match arms already cover every variant they can receive, since `allows` gates the wide ones out first.

- [ ] **Step 5: Make `PlanarRows` generic**

In `crates/mpp-core/src/formats/mod.rs`:

```rust
/// A planar image source the streaming writers can drain row by row.
///
/// The type parameter is the source sample type. `XisfImage` and the DZI
/// path use `PlanarRows<f32>`; the PixInsight worker stages whichever type
/// the session negotiated, so it instantiates this at `u8`, `u16`, `u32`,
/// `f32` or `f64`.
pub trait PlanarRows<T> {
    /// Width in pixels.
    fn width(&self) -> u64;
    /// Height in pixels.
    fn height(&self) -> u64;
    /// Channel count.
    fn channels(&self) -> u64;
    /// One row of one channel (len == width). For `f32` sources, values are
    /// nominally in `[0, 1]`.
    fn row(&self, c: u64, y: u64) -> &[T];
}
```

Then in `xisf.rs:161` change `impl super::PlanarRows for XisfImage` to `impl super::PlanarRows<f32> for XisfImage`, and in `staging.rs:233` change `impl PlanarRows for Staging` to `impl PlanarRows<f32> for Staging`. Change `write_bigtiff`'s bound in `tiff.rs` to `img: &impl PlanarRows<f32>`. The three `PlanarRows::width(&s)` calls in `staging.rs`'s tests become `PlanarRows::<f32>::width(&s)` and so on.

- [ ] **Step 6: Run the full test suite**

Run: `cargo test`
Expected: PASS, including the new test. Anything that fails to infer `T` gets an explicit `PlanarRows<f32>` annotation.

- [ ] **Step 7: Run the gates**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add -A crates/
git commit -m "refactor(core): widen BitDepth to u32/f64 and make PlanarRows generic

Groundwork for real 32-bit-integer and 64-bit-float TIFF output. No
behavior change: BigTIFF is the only OutputFormat that accepts the new
variants, and PlanarRows is instantiated only at f32 so far.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Write options and validation

The whole option surface, with validation, before any encoding exists. Validation is where the spec's consistency rules live, so it gets its own test cycle.

**Files:**
- Create: `crates/mpp-core/src/formats/tiff_write/options.rs`
- Create: `crates/mpp-core/src/formats/tiff_write/mod.rs`
- Modify: `crates/mpp-core/src/formats/mod.rs` (add `pub mod tiff_write;`)
- Test: in `options.rs`'s own `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `BitDepth` from Task 1.
- Produces: `TiffWriteOptions` (all fields public), `Compression`, `Container`, `Layout`, `AlphaMode`, `ResolutionUnit`, `Resolution`, and `TiffWriteOptions::validate(&self, width: u64, height: u64, channels: u64) -> Result<Container>` which resolves `Container::Auto` to a concrete `Container::Classic` or `Container::Big`.

- [ ] **Step 1: Write the failing tests**

Create `crates/mpp-core/src/formats/tiff_write/options.rs` with only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> TiffWriteOptions {
        TiffWriteOptions::default()
    }

    #[test]
    fn default_is_uncompressed_stripped_auto_f32() {
        let o = opts();
        assert_eq!(o.bit_depth, BitDepth::F32);
        assert_eq!(o.compression, Compression::None);
        assert_eq!(o.container, Container::Auto);
        assert_eq!(o.layout, Layout::Strips);
        assert_eq!(o.alpha, AlphaMode::None);
    }

    #[test]
    fn auto_picks_classic_for_a_small_image() {
        let o = opts();
        // 1000 x 1000 RGB f32 = 12 MB, comfortably inside classic TIFF.
        assert_eq!(o.validate(1000, 1000, 3).unwrap(), Container::Classic);
    }

    #[test]
    fn auto_picks_big_past_the_four_gigabyte_bound() {
        let o = opts();
        // 40000 x 30000 RGB f32 = 14.4 GB.
        assert_eq!(o.validate(40_000, 30_000, 3).unwrap(), Container::Big);
    }

    #[test]
    fn explicit_classic_rejects_an_oversized_image() {
        let o = TiffWriteOptions {
            container: Container::Classic,
            ..opts()
        };
        let err = o.validate(40_000, 30_000, 3).unwrap_err().to_string();
        assert!(
            err.contains("BigTIFF"),
            "error should point the user at BigTIFF, got: {err}"
        );
    }

    #[test]
    fn predictor_is_rejected_for_float_samples() {
        let o = TiffWriteOptions {
            predictor: true,
            bit_depth: BitDepth::F32,
            ..opts()
        };
        assert!(o.validate(16, 16, 1).is_err());
        let o = TiffWriteOptions {
            predictor: true,
            bit_depth: BitDepth::U16,
            ..opts()
        };
        assert!(o.validate(16, 16, 1).is_ok());
    }

    #[test]
    fn tile_size_must_be_a_power_of_two_in_range() {
        for (size, ok) in [(64u32, false), (128, true), (256, true), (1024, true),
                           (2048, false), (300, false)] {
            let o = TiffWriteOptions {
                layout: Layout::Tiles { size },
                ..opts()
            };
            assert_eq!(o.validate(4096, 4096, 1).is_ok(), ok, "tile size {size}");
        }
    }

    #[test]
    fn alpha_and_channel_count_must_agree_both_ways() {
        let with_alpha = TiffWriteOptions {
            alpha: AlphaMode::Unassociated,
            ..opts()
        };
        assert!(with_alpha.validate(8, 8, 2).is_ok());
        assert!(with_alpha.validate(8, 8, 4).is_ok());
        assert!(with_alpha.validate(8, 8, 1).is_err());
        assert!(with_alpha.validate(8, 8, 3).is_err());
        // 2 and 4 channels without an alpha association would be a multiband
        // image, which this writer does not produce.
        assert!(opts().validate(8, 8, 2).is_err());
        assert!(opts().validate(8, 8, 4).is_err());
        assert!(opts().validate(8, 8, 1).is_ok());
        assert!(opts().validate(8, 8, 3).is_ok());
    }

    #[test]
    fn channel_count_outside_one_to_four_is_rejected() {
        assert!(opts().validate(8, 8, 0).is_err());
        assert!(opts().validate(8, 8, 5).is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-core tiff_write::options`
Expected: FAIL to compile — the module is not declared and none of the types exist.

- [ ] **Step 3: Write the options types and validation**

Prepend to `options.rs` (above the test module):

```rust
//! Write options for [`crate::formats::tiff_write`] and their validation.
//!
//! Validation is deliberately separate from encoding: every rule the spec
//! states about legal option combinations is checked here, before a single
//! byte is written, so an impossible request fails immediately with an
//! actionable message rather than part-way through a multi-gigabyte encode.

use crate::formats::tiff::BitDepth;
use crate::{Error, Result};

/// Largest payload a classic (32-bit) TIFF can address, with room left for
/// the header, the IFD, and its out-of-line values.
const CLASSIC_TIFF_PAYLOAD_LIMIT: u64 = (4u64 << 30) - (16 << 20);

/// Compression applied to each strip or tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression (default). Fastest, largest, and what every reader
    /// supports.
    #[default]
    None,
    /// LZW (TIFF compression 5). Lossless, widely supported.
    Lzw,
    /// Deflate/zlib (TIFF compression 8) at [`TiffWriteOptions::deflate_level`].
    Deflate,
    /// PackBits run-length encoding (TIFF compression 32773).
    Packbits,
}

/// Which TIFF container variant to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Container {
    /// Classic TIFF when the payload fits, BigTIFF otherwise (default).
    #[default]
    Auto,
    /// Classic 32-bit-offset TIFF. Fails validation if the payload cannot
    /// fit; the widest reader compatibility.
    Classic,
    /// BigTIFF (64-bit offsets). Required past 4 GB.
    Big,
}

/// How pixel data is divided into independently addressed blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Full-width horizontal strips, sized to roughly 4 MiB each (default).
    #[default]
    Strips,
    /// Square tiles of `size` pixels per edge. Wanted by the Astrometrical
    /// large-image load path, which reads sub-regions rather than rows.
    Tiles {
        /// Tile edge in pixels; a power of two in `128..=1024`.
        size: u32,
    },
}

/// Whether the last channel is an alpha channel, and how it relates to the
/// colour channels (TIFF's `ExtraSamples` tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlphaMode {
    /// No alpha channel; 1 or 3 channels (default).
    #[default]
    None,
    /// Alpha is not premultiplied into the colour channels
    /// (`ExtraSamples = 2`). The usual choice for a mask.
    Unassociated,
    /// Alpha is premultiplied into the colour channels (`ExtraSamples = 1`).
    Associated,
}

/// Unit the resolution values are expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionUnit {
    /// Pixels per inch (default; TIFF `ResolutionUnit = 2`).
    #[default]
    Inch,
    /// Pixels per centimetre (TIFF `ResolutionUnit = 3`).
    Centimeter,
}

/// Physical resolution written to `XResolution`/`YResolution`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolution {
    /// Horizontal resolution in pixels per [`Resolution::unit`].
    pub x: f64,
    /// Vertical resolution in pixels per [`Resolution::unit`].
    pub y: f64,
    /// Unit the two values are expressed in.
    pub unit: ResolutionUnit,
}

/// Options for [`crate::formats::tiff_write::write_tiff`].
#[derive(Debug, Clone, Default)]
pub struct TiffWriteOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
    /// Container variant.
    pub container: Container,
    /// Strip or tile layout.
    pub layout: Layout,
    /// Per-block compression.
    pub compression: Compression,
    /// Deflate level, 1-9; ignored unless [`Compression::Deflate`]. `None`
    /// selects the default level 6.
    pub deflate_level: Option<u8>,
    /// Apply the horizontal differencing predictor (TIFF tag 317). Integer
    /// sample formats only.
    pub predictor: bool,
    /// Whether the final channel is alpha, and its association.
    pub alpha: AlphaMode,
    /// ICC profile bytes to embed in tag 34675.
    pub icc_profile: Option<Vec<u8>>,
    /// `ImageDescription` (tag 270).
    pub description: Option<String>,
    /// `Copyright` (tag 33432).
    pub copyright: Option<String>,
    /// `Software` (tag 305).
    pub software: Option<String>,
    /// Physical resolution; omitted from the file when `None`.
    pub resolution: Option<Resolution>,
}

impl TiffWriteOptions {
    /// Check every option against the image geometry and resolve
    /// [`Container::Auto`] to a concrete container.
    ///
    /// Returns the container the encoder should write. Errors name the
    /// offending option and what would make it legal.
    pub fn validate(&self, width: u64, height: u64, channels: u64) -> Result<Container> {
        if width == 0 || height == 0 {
            return Err(Error::invalid(format!(
                "image dimensions must both be nonzero (got {width}x{height})"
            )));
        }
        if !(1..=4).contains(&channels) {
            return Err(Error::invalid(format!(
                "channel count {channels} is not supported (1-4: gray, gray+alpha, RGB, RGBA)"
            )));
        }

        let has_alpha = self.alpha != AlphaMode::None;
        let alpha_channels = if has_alpha { 1 } else { 0 };
        let colour = channels - alpha_channels;
        if has_alpha && channels < 2 {
            return Err(Error::invalid(format!(
                "an alpha association needs at least 2 channels (got {channels})"
            )));
        }
        if colour != 1 && colour != 3 {
            return Err(Error::invalid(format!(
                "{channels} channels with alpha={:?} leaves {colour} colour channel(s); \
                 expected 1 (gray) or 3 (RGB)",
                self.alpha
            )));
        }

        if self.predictor && matches!(self.bit_depth, BitDepth::F32 | BitDepth::F64) {
            return Err(Error::invalid(
                "the horizontal predictor is defined for integer samples only; \
                 turn it off or choose an 8/16/32-bit unsigned sample format"
                    .to_string(),
            ));
        }

        if let Layout::Tiles { size } = self.layout {
            if !(128..=1024).contains(&size) || !size.is_power_of_two() {
                return Err(Error::invalid(format!(
                    "tile size {size} is not supported (a power of two in 128..=1024)"
                )));
            }
        }

        if let Some(level) = self.deflate_level {
            if !(1..=9).contains(&level) {
                return Err(Error::invalid(format!(
                    "deflate level {level} is out of range (1-9)"
                )));
            }
        }

        // Uncompressed payload; compression only ever shrinks it, so a
        // classic container that fits this bound always fits the real file.
        let payload = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(channels))
            .and_then(|v| v.checked_mul(self.bit_depth.bytes() as u64))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "image {width}x{height}x{channels} overflows the addressable payload size"
                ))
            })?;
        let fits_classic = payload <= CLASSIC_TIFF_PAYLOAD_LIMIT;

        match self.container {
            Container::Auto => Ok(if fits_classic {
                Container::Classic
            } else {
                Container::Big
            }),
            Container::Classic if !fits_classic => Err(Error::invalid(format!(
                "a classic TIFF cannot address this image ({payload} bytes of pixel data \
                 exceeds the 4 GB limit); choose BigTIFF"
            ))),
            other => Ok(other),
        }
    }

    /// Deflate level to use, defaulting to 6.
    pub fn effective_deflate_level(&self) -> u8 {
        self.deflate_level.unwrap_or(6)
    }
}
```

- [ ] **Step 4: Add `BitDepth::bytes` and `Error::invalid` if missing**

`validate` needs a byte width per sample format and an invalid-argument error constructor. Add to `crates/mpp-core/src/formats/tiff.rs`:

```rust
impl BitDepth {
    /// Bytes per sample in the output file.
    pub fn bytes(self) -> usize {
        match self {
            BitDepth::U8 => 1,
            BitDepth::U16 => 2,
            BitDepth::U32 | BitDepth::F32 => 4,
            BitDepth::F64 => 8,
        }
    }
}
```

`crates/mpp-core/src/error.rs` currently has only `Io` and `Format`, both of which carry a path. Option validation happens before a path is meaningful, so add a third variant that does not:

```rust
    /// An option or argument combination that cannot produce a valid file.
    ///
    /// Unlike [`Error::Io`] and [`Error::Format`] this carries no path: it
    /// reports a caller mistake caught before any file is touched.
    #[error("invalid option: {reason}")]
    Invalid {
        /// Human-readable explanation of what is wrong and what would be legal.
        reason: String,
    },
```

and, alongside the existing constructors:

```rust
    /// Build an [`Error::Invalid`] from a reason.
    pub fn invalid(reason: impl Into<String>) -> Self {
        Error::Invalid {
            reason: reason.into(),
        }
    }
```

Update the doc comment on the `Error` enum, which currently says "Deliberately small: an I/O failure on a file, or a malformed/unsupported file", to mention the third case.

- [ ] **Step 5: Declare the module**

Create `crates/mpp-core/src/formats/tiff_write/mod.rs`:

```rust
//! IFD-level TIFF/BigTIFF writer.
//!
//! Built directly on the `tiff` crate's `DirectoryEncoder` and its public
//! compressors rather than its `ImageEncoder`, because `ImageEncoder` has no
//! tile support and fixes the colour type at compile time through a marker
//! trait. Writing the IFD ourselves buys tiled output, classic-or-BigTIFF
//! from one code path, all five sample formats, and alpha, at the cost of
//! owning the tag set — see the tests, which assert on raw tag values as
//! well as decoded pixels.

pub mod options;

pub use options::{
    AlphaMode, Compression, Container, Layout, Resolution, ResolutionUnit, TiffWriteOptions,
};
```

And add `pub mod tiff_write;` to `crates/mpp-core/src/formats/mod.rs`'s module list, keeping it alphabetical with the existing `tiff`, `tiff_read`, `xisf`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p mpp-core tiff_write::options`
Expected: PASS, all eight tests.

- [ ] **Step 7: Run the gates**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: clean. Every new public item has a doc comment.

- [ ] **Step 8: Commit**

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): TIFF write options and validation

Every option the parity dialog will offer, with the spec's consistency
rules checked up front: alpha and channel count agree in both directions,
the predictor is integer-only, tile sizes are powers of two in range, and
Container::Auto resolves against the classic-TIFF 4 GB bound before any
bytes are written.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: The `Sample` trait

Five sample types, each knowing its TIFF tag values and how to convert to and from the unit-range `f32` the XISF path produces.

**Files:**
- Create: `crates/mpp-core/src/formats/tiff_write/sample.rs`
- Modify: `crates/mpp-core/src/formats/tiff_write/mod.rs` (declare and re-export)
- Test: in `sample.rs`'s own `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `BitDepth` from Task 1.
- Produces: `trait Sample: Copy + 'static` with associated consts `BITS_PER_SAMPLE: u16`, `TIFF_SAMPLE_FORMAT: u16`, `DEPTH: BitDepth`, and methods `fn from_unit_f32(v: f32) -> Self`, `fn push_ne(self, out: &mut Vec<u8>)`. Implemented for `u8`, `u16`, `u32`, `f32`, `f64`. Task 5 adds `fn wrapping_sub_sample(self, other: Self) -> Self` for the predictor, and Task 11 adds `bytemuck::Pod` as a supertrait.

- [ ] **Step 1: Write the failing test**

Create `crates/mpp-core/src/formats/tiff_write/sample.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_values_match_the_tiff_spec() {
        // SampleFormat: 1 = unsigned integer, 3 = IEEE floating point.
        assert_eq!((u8::BITS_PER_SAMPLE, u8::TIFF_SAMPLE_FORMAT), (8, 1));
        assert_eq!((u16::BITS_PER_SAMPLE, u16::TIFF_SAMPLE_FORMAT), (16, 1));
        assert_eq!((u32::BITS_PER_SAMPLE, u32::TIFF_SAMPLE_FORMAT), (32, 1));
        assert_eq!((f32::BITS_PER_SAMPLE, f32::TIFF_SAMPLE_FORMAT), (32, 3));
        assert_eq!((f64::BITS_PER_SAMPLE, f64::TIFF_SAMPLE_FORMAT), (64, 3));
    }

    #[test]
    fn unit_range_conversion_saturates_and_rounds() {
        assert_eq!(u8::from_unit_f32(0.0), 0);
        assert_eq!(u8::from_unit_f32(1.0), 255);
        assert_eq!(u8::from_unit_f32(0.5), 128);
        assert_eq!(u8::from_unit_f32(-3.0), 0);
        assert_eq!(u8::from_unit_f32(7.0), 255);
        assert_eq!(u16::from_unit_f32(1.0), 65535);
        assert_eq!(u32::from_unit_f32(1.0), u32::MAX);
        assert_eq!(u32::from_unit_f32(0.0), 0);
        assert_eq!(f32::from_unit_f32(0.25), 0.25);
        assert_eq!(f64::from_unit_f32(0.25), 0.25);
    }

    #[test]
    fn u32_conversion_is_monotonic_near_the_top() {
        // f32 has a 24-bit mantissa, so the naive `v * u32::MAX as f32`
        // rounds several distinct inputs to the same output and can
        // overflow on the way to `as u32`. What we require is that the
        // conversion never wraps and never exceeds the maximum.
        assert_eq!(u32::from_unit_f32(1.0), u32::MAX);
        assert!(u32::from_unit_f32(0.999_999) < u32::MAX);
        assert!(u32::from_unit_f32(0.999_999) > u32::MAX / 2);
    }

    #[test]
    fn push_ne_emits_native_order_bytes() {
        let mut v = Vec::new();
        0x1234u16.push_ne(&mut v);
        assert_eq!(v, 0x1234u16.to_ne_bytes().to_vec());

        let mut v = Vec::new();
        1.5f32.push_ne(&mut v);
        assert_eq!(v, 1.5f32.to_ne_bytes().to_vec());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-core tiff_write::sample`
Expected: FAIL to compile — no `Sample` trait.

- [ ] **Step 3: Implement the trait**

Prepend to `sample.rs`:

```rust
//! The sample types the writer can emit, and their TIFF tag values.
//!
//! Sample bytes are always emitted in **native** byte order: the `tiff`
//! crate writes the file's byte-order mark from `target_endian` (`II` on a
//! little-endian host, `MM` on a big-endian one), so native-order samples
//! are by construction the file's order.

use crate::formats::tiff::BitDepth;

/// A sample type the writer can emit into a TIFF file.
pub trait Sample: Copy + 'static {
    /// Value of the `BitsPerSample` tag for this type.
    const BITS_PER_SAMPLE: u16;
    /// Value of the `SampleFormat` tag: `1` unsigned integer, `3` IEEE float.
    const TIFF_SAMPLE_FORMAT: u16;
    /// The [`BitDepth`] this type corresponds to.
    const DEPTH: BitDepth;

    /// Convert a unit-range `f32` (the XISF path's sample domain) to this
    /// type, clamping out-of-range input.
    fn from_unit_f32(v: f32) -> Self;

    /// Append this sample's native-order bytes to `out`.
    fn push_ne(self, out: &mut Vec<u8>);
}

macro_rules! uint_sample {
    ($t:ty, $bits:expr, $depth:expr) => {
        impl Sample for $t {
            const BITS_PER_SAMPLE: u16 = $bits;
            const TIFF_SAMPLE_FORMAT: u16 = 1;
            const DEPTH: BitDepth = $depth;

            fn from_unit_f32(v: f32) -> Self {
                // Scale in f64: for u32 the target range exceeds f32's
                // 24-bit mantissa, so scaling in f32 would both lose
                // resolution and risk rounding above the maximum on the
                // way to the integer cast. `clamp` before the cast makes
                // the saturation explicit rather than relying on Rust's
                // saturating float-to-int semantics.
                let scaled = f64::from(v.clamp(0.0, 1.0)) * <$t>::MAX as f64;
                scaled.round().clamp(0.0, <$t>::MAX as f64) as $t
            }

            fn push_ne(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_ne_bytes());
            }
        }
    };
}

uint_sample!(u8, 8, BitDepth::U8);
uint_sample!(u16, 16, BitDepth::U16);
uint_sample!(u32, 32, BitDepth::U32);

impl Sample for f32 {
    const BITS_PER_SAMPLE: u16 = 32;
    const TIFF_SAMPLE_FORMAT: u16 = 3;
    const DEPTH: BitDepth = BitDepth::F32;

    fn from_unit_f32(v: f32) -> Self {
        v
    }

    fn push_ne(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_ne_bytes());
    }
}

impl Sample for f64 {
    const BITS_PER_SAMPLE: u16 = 64;
    const TIFF_SAMPLE_FORMAT: u16 = 3;
    const DEPTH: BitDepth = BitDepth::F64;

    fn from_unit_f32(v: f32) -> Self {
        f64::from(v)
    }

    fn push_ne(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_ne_bytes());
    }
}
```

Add to `tiff_write/mod.rs`:

```rust
pub mod sample;

pub use sample::Sample;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_write::sample`
Expected: PASS, all four tests.

- [ ] **Step 5: Run the gates and commit**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): Sample trait for the five TIFF sample formats

Each type carries its BitsPerSample/SampleFormat tag values and converts
from the unit-range f32 the XISF path produces. u32 scales through f64
because f32's 24-bit mantissa cannot address the full u32 range.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Strip encoding, uncompressed

The first working writer. It must reproduce today's output for the gray/RGB f32/u16/u8 cases so the existing consumers can switch over unchanged.

**Files:**
- Create: `crates/mpp-core/src/formats/tiff_write/encode.rs`
- Modify: `crates/mpp-core/src/formats/tiff_write/mod.rs` (the `write_tiff` entry point)
- Test: in `encode.rs`'s own `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `TiffWriteOptions`, `Container`, `Layout` (Task 2); `Sample` (Task 3); `PlanarRows<T>` (Task 1).
- Produces:
  ```rust
  pub fn write_tiff<T: Sample>(
      img: &impl PlanarRows<T>,
      path: &Path,
      opts: &TiffWriteOptions,
  ) -> Result<()>
  ```
  and, for tests that need to force many blocks from a small image,
  ```rust
  pub(crate) fn write_tiff_with_target<T: Sample>(
      img: &impl PlanarRows<T>,
      path: &Path,
      opts: &TiffWriteOptions,
      target_block_bytes: u64,
  ) -> Result<()>
  ```

- [ ] **Step 1: Write the failing tests**

Create `crates/mpp-core/src/formats/tiff_write/encode.rs` with only a test module. Note the shared `Synth` helper — later tasks reuse it.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::tiff::BitDepth;
    use std::path::PathBuf;
    use tiff::decoder::{Decoder, DecodingResult};
    use tiff::tags::Tag;

    /// An in-memory planar image whose sample `(c, y, x)` is a distinct,
    /// predictable value — so a transposed, mis-strided or mis-interleaved
    /// write cannot accidentally produce the expected pixels.
    struct Synth {
        w: u64,
        h: u64,
        c: u64,
        planes: Vec<Vec<f32>>,
    }

    impl Synth {
        fn new(w: u64, h: u64, c: u64) -> Self {
            let total = (w * h * c) as f32;
            let planes = (0..c)
                .map(|ch| {
                    (0..w * h)
                        .map(|i| (ch * w * h + i) as f32 / total)
                        .collect()
                })
                .collect();
            Self { w, h, c, planes }
        }
    }

    impl crate::formats::PlanarRows<f32> for Synth {
        fn width(&self) -> u64 {
            self.w
        }
        fn height(&self) -> u64 {
            self.h
        }
        fn channels(&self) -> u64 {
            self.c
        }
        fn row(&self, c: u64, y: u64) -> &[f32] {
            let start = (y * self.w) as usize;
            &self.planes[c as usize][start..start + self.w as usize]
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-tw-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_gray_f32_strips_readable_by_the_decoder() {
        let dir = tmpdir("gray-f32");
        let out = dir.join("out.tif");
        let img = Synth::new(5, 4, 1);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.dimensions().unwrap(), (5, 4));
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 20);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 20.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rgb_planes_interleave_to_chunky_order() {
        let dir = tmpdir("rgb");
        let out = dir.join("out.tif");
        let img = Synth::new(3, 2, 3);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 18);
                // Pixel 0 gathers sample 0 of each plane: 0/18, 6/18, 12/18.
                assert!((data[0] - 0.0 / 18.0).abs() < 1e-7);
                assert!((data[1] - 6.0 / 18.0).abs() < 1e-7);
                assert!((data[2] - 12.0 / 18.0).abs() < 1e-7);
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_strip_output_covers_a_partial_final_strip() {
        // 3x7 gray f32: row_bytes = 12; a 64-byte target gives 5 rows per
        // strip, so a 7-row image is one full strip plus a 2-row remainder.
        let dir = tmpdir("strips");
        let out = dir.join("out.tif");
        let img = Synth::new(3, 7, 1);
        write_tiff_with_target(&img, &out, &TiffWriteOptions::default(), 64).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        let offsets = dec.get_tag_u64_vec(Tag::StripOffsets).unwrap();
        let counts = dec.get_tag_u64_vec(Tag::StripByteCounts).unwrap();
        assert_eq!(offsets.len(), 2, "expected two strips");
        assert_eq!(counts, vec![5 * 3 * 4, 2 * 3 * 4]);
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 21);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 21.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32 samples, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u16_and_u8_scale_the_unit_range() {
        let dir = tmpdir("ints");
        let img = Synth::new(4, 4, 1);

        let out16 = dir.join("o16.tif");
        write_tiff(
            &img,
            &out16,
            &TiffWriteOptions {
                bit_depth: BitDepth::U16,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out16).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U16(d) => {
                assert_eq!(d[0], 0);
                assert_eq!(d[15], 61439); // round(15/16 * 65535)
            }
            other => panic!("expected U16, got {other:?}"),
        }

        let out8 = dir.join("o8.tif");
        write_tiff(
            &img,
            &out8,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out8).unwrap()).unwrap();
        match dec.read_image().unwrap() {
            DecodingResult::U8(d) => {
                assert_eq!(d[0], 0);
                assert_eq!(d[8], 128);
                assert_eq!(d[15], 239);
            }
            other => panic!("expected U8, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn core_tags_are_written_with_spec_values() {
        let dir = tmpdir("tags");
        let out = dir.join("out.tif");
        let img = Synth::new(8, 8, 3);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(), vec![32, 32, 32]);
        assert_eq!(dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(), vec![3, 3, 3]);
        assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 3);
        assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 2); // RGB
        assert_eq!(dec.get_tag_u64(Tag::PlanarConfiguration).unwrap(), 1); // chunky
        assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), 1); // none
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_an_invalid_option_combination_before_creating_the_file() {
        let dir = tmpdir("reject");
        let out = dir.join("never.tif");
        let img = Synth::new(4, 4, 2); // 2 channels with no alpha association
        assert!(write_tiff(&img, &out, &TiffWriteOptions::default()).is_err());
        assert!(!out.exists(), "a rejected write must not leave a file behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: FAIL to compile — `write_tiff` does not exist.

- [ ] **Step 3: Implement the encoder**

Prepend to `encode.rs`. This is the core of the whole plan; read the spec's "The writer" section alongside it.

```rust
//! The strip/tile encode loop and the tag set it emits.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use tiff::encoder::{TiffEncoder, TiffKind, TiffKindBig, TiffKindStandard};
use tiff::tags::Tag;

use super::options::{AlphaMode, Container, Layout, TiffWriteOptions};
use super::sample::Sample;
use crate::formats::PlanarRows;
use crate::{Error, Result};

/// Target uncompressed bytes per strip; rows per strip derives from this.
const TARGET_BLOCK_BYTES: u64 = 4 << 20;

/// Write `img` to `path` as a TIFF or BigTIFF per `opts`, streaming block by
/// block in constant memory.
///
/// Options are validated first, so an impossible request fails before the
/// output file is created. Grayscale, gray+alpha, RGB and RGBA are
/// supported; planar source planes are interleaved to TIFF's chunky order.
pub fn write_tiff<T: Sample>(
    img: &impl PlanarRows<T>,
    path: &Path,
    opts: &TiffWriteOptions,
) -> Result<()> {
    write_tiff_with_target(img, path, opts, TARGET_BLOCK_BYTES)
}

/// [`write_tiff`], parameterized on the target block size — split out so
/// tests can force multi-block output without a multi-gigabyte image.
pub(crate) fn write_tiff_with_target<T: Sample>(
    img: &impl PlanarRows<T>,
    path: &Path,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
) -> Result<()> {
    let (w, h, c) = (img.width(), img.height(), img.channels());
    let container = opts.validate(w, h, c)?;

    let file = File::create(path).map_err(|e| Error::io(path, e))?;
    let mut buf = BufWriter::new(file);
    let result = match container {
        Container::Classic => {
            encode::<TiffKindStandard, T, _, _>(img, &mut buf, opts, target_block_bytes, path)
        }
        // `validate` resolves Auto to Classic or Big and never returns Auto.
        _ => encode::<TiffKindBig, T, _, _>(img, &mut buf, opts, target_block_bytes, path),
    };
    if result.is_err() {
        // Do not leave a truncated file where a reader might find it.
        drop(buf);
        let _ = std::fs::remove_file(path);
        return result;
    }
    buf.flush().map_err(|e| Error::io(path, e))?;
    Ok(())
}

/// Photometric interpretation for a colour-channel count: 1 = BlackIsZero,
/// 2 = RGB (TIFF 6.0 §8).
fn photometric(colour_channels: u64) -> u16 {
    if colour_channels == 1 { 1 } else { 2 }
}

fn encode<K, T, W, I>(
    img: &I,
    writer: &mut W,
    opts: &TiffWriteOptions,
    target_block_bytes: u64,
    path: &Path,
) -> Result<()>
where
    K: TiffKind,
    T: Sample,
    W: Write + Seek,
    I: PlanarRows<T> + ?Sized,
{
    let terr = |e: tiff::TiffError| Error::format(path, format!("TIFF encode: {e}"));
    let (w, h, c) = (img.width(), img.height(), img.channels());
    let w32 = u32::try_from(w).map_err(|_| Error::format(path, "image width exceeds TIFF limits"))?;
    let h32 =
        u32::try_from(h).map_err(|_| Error::format(path, "image height exceeds TIFF limits"))?;
    let has_alpha = opts.alpha != AlphaMode::None;
    let colour_channels = if has_alpha { c - 1 } else { c };

    let mut enc: TiffEncoder<&mut W, K> = TiffEncoder::new_generic(writer).map_err(terr)?;
    let mut dir = enc.image_directory().map_err(terr)?;

    let mut offsets: Vec<K::OffsetType> = Vec::new();
    let mut byte_counts: Vec<K::OffsetType> = Vec::new();
    let mut raw = Vec::<u8>::new();

    // -- pixel data ---------------------------------------------------------
    let rows_per_strip = match opts.layout {
        Layout::Strips => {
            let row_bytes = w * c * T::BITS_PER_SAMPLE as u64 / 8;
            let rows = (target_block_bytes / row_bytes.max(1)).clamp(1, h);
            for start in (0..h).step_by(rows as usize) {
                let n = rows.min(h - start);
                gather_strip(img, start, n, &mut raw);
                write_block::<K, _>(&mut dir, &raw, &mut offsets, &mut byte_counts).map_err(terr)?;
            }
            Some(u32::try_from(rows).unwrap_or(u32::MAX))
        }
        Layout::Tiles { size } => {
            let size64 = u64::from(size);
            for ty in (0..h).step_by(size as usize) {
                for tx in (0..w).step_by(size as usize) {
                    gather_tile(img, tx, ty, size64, &mut raw);
                    write_block::<K, _>(&mut dir, &raw, &mut offsets, &mut byte_counts)
                        .map_err(terr)?;
                }
            }
            None
        }
    };

    // -- tags ---------------------------------------------------------------
    dir.write_tag(Tag::ImageWidth, w32).map_err(terr)?;
    dir.write_tag(Tag::ImageLength, h32).map_err(terr)?;
    let bits: Vec<u16> = vec![T::BITS_PER_SAMPLE; c as usize];
    dir.write_tag(Tag::BitsPerSample, &bits[..]).map_err(terr)?;
    let formats: Vec<u16> = vec![T::TIFF_SAMPLE_FORMAT; c as usize];
    dir.write_tag(Tag::SampleFormat, &formats[..]).map_err(terr)?;
    dir.write_tag(Tag::SamplesPerPixel, c as u16).map_err(terr)?;
    dir.write_tag(Tag::PhotometricInterpretation, photometric(colour_channels))
        .map_err(terr)?;
    dir.write_tag(Tag::PlanarConfiguration, 1u16).map_err(terr)?; // chunky
    dir.write_tag(Tag::Compression, 1u16).map_err(terr)?; // none, for now

    match (opts.layout, rows_per_strip) {
        (Layout::Strips, Some(rows)) => {
            dir.write_tag(Tag::RowsPerStrip, rows).map_err(terr)?;
            dir.write_tag(Tag::StripOffsets, K::convert_slice(&offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::StripByteCounts, K::convert_slice(&byte_counts))
                .map_err(terr)?;
        }
        (Layout::Tiles { size }, _) => {
            dir.write_tag(Tag::TileWidth, size).map_err(terr)?;
            dir.write_tag(Tag::TileLength, size).map_err(terr)?;
            dir.write_tag(Tag::TileOffsets, K::convert_slice(&offsets))
                .map_err(terr)?;
            dir.write_tag(Tag::TileByteCounts, K::convert_slice(&byte_counts))
                .map_err(terr)?;
        }
        _ => unreachable!("rows_per_strip is Some exactly for Layout::Strips"),
    }

    dir.finish().map_err(terr)?;
    Ok(())
}

/// Append one compressed block to the file, recording its offset and length.
fn write_block<K, W>(
    dir: &mut tiff::encoder::DirectoryEncoder<'_, W, K>,
    bytes: &[u8],
    offsets: &mut Vec<K::OffsetType>,
    byte_counts: &mut Vec<K::OffsetType>,
) -> tiff::TiffResult<()>
where
    K: TiffKind,
    W: Write + Seek,
{
    let offset = dir.write_data(bytes)?;
    offsets.push(K::convert_offset(offset)?);
    // Byte counts are the same width as offsets in both container kinds;
    // `OffsetType: TryFrom<usize>` is the crate's own idiom for this
    // (see its `ImageEncoder::write_strip`).
    byte_counts.push(K::OffsetType::try_from(bytes.len())?);
    Ok(())
}

/// Interleave rows `[start, start + n)` of every channel into `out`.
fn gather_strip<T: Sample, I: PlanarRows<T> + ?Sized>(
    img: &I,
    start: u64,
    n: u64,
    out: &mut Vec<u8>,
) {
    let (w, c) = (img.width(), img.channels());
    out.clear();
    for y in start..start + n {
        let planes: Vec<&[T]> = (0..c).map(|ch| img.row(ch, y)).collect();
        for x in 0..w as usize {
            for p in &planes {
                p[x].push_ne(out);
            }
        }
    }
}

/// Interleave one `size x size` tile whose top-left pixel is `(tx, ty)` into
/// `out`, zero-padding the parts that fall outside the image — TIFF 6.0 §15
/// requires every tile to be full size, including the edge ones.
fn gather_tile<T: Sample, I: PlanarRows<T> + ?Sized>(
    img: &I,
    tx: u64,
    ty: u64,
    size: u64,
    out: &mut Vec<u8>,
) {
    let (w, h, c) = (img.width(), img.height(), img.channels());
    let zero = T::from_unit_f32(0.0);
    out.clear();
    for y in ty..ty + size {
        if y < h {
            let planes: Vec<&[T]> = (0..c).map(|ch| img.row(ch, y)).collect();
            for x in tx..tx + size {
                for p in &planes {
                    if x < w { p[x as usize] } else { zero }.push_ne(out);
                }
            }
        } else {
            for _ in 0..size * c {
                zero.push_ne(out);
            }
        }
    }
}
```

Then in `tiff_write/mod.rs`:

```rust
pub mod encode;

pub use encode::write_tiff;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: PASS, all six tests. If `TiffEncoder::new_generic` will not infer `K`, give the binding the explicit type shown above.

- [ ] **Step 5: Migrate the existing consumers and delete the old writer**

Replace every `write_bigtiff(img, path, &TiffWriteOptions { bit_depth })` call with `write_tiff`, importing from `crate::formats::tiff_write`:

- `crates/mpp-core/src/convert.rs:6, 236-249`
- `crates/mpp-core/src/publish.rs:14, 228-232, 439-443`
- `crates/mpp-ipc-worker/src/main.rs:32, 287-288`

`BitDepth` stays in `formats::tiff`; re-export it from `tiff_write` so consumers need only one import:

```rust
pub use crate::formats::tiff::BitDepth;
```

Then delete `write_bigtiff`, `write_bigtiff_with_target`, `to_u16`, `to_u8`, `TiffWriteOptions` and the whole test module from `crates/mpp-core/src/formats/tiff.rs`, leaving only `BitDepth` and its `bytes()` method. Update the file's module doc comment to say it now holds only the sample-format enum, and point at `tiff_write` for the writer.

**Note the behaviour change this introduces deliberately:** every existing caller keeps `Container::Auto`, so small outputs that used to be BigTIFF now come out as classic TIFF. That is the intended fix from the spec's finding 3. `crates/mpp-core/src/formats/tiff.rs`'s old test asserting the BigTIFF magic `[0x49, 0x49, 0x2B, 0x00]` is gone with the rest of that module; Task 6 adds container-specific magic assertions in its place.

- [ ] **Step 6: Run the full suite**

Run: `cargo test`
Expected: PASS. Tests elsewhere that assert BigTIFF magic on small outputs must be updated to either assert classic magic or pass `Container::Big` explicitly — check `crates/mpp/tests/` and `crates/mpp-ipc-worker/tests/` for such assertions and fix each one, choosing `Container::Big` where the test's subject is BigTIFF specifically.

- [ ] **Step 7: Run the gates and commit**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`

```bash
git add -A crates/
git commit -m "feat(core): IFD-level strip writer replacing tiff's ImageEncoder

Writes the IFD ourselves over the crate's public DirectoryEncoder, which
removes the ColorType-per-combination macro and opens the way to tiles,
classic TIFF, u32/f64 and alpha. Behaviour is unchanged except that
Container::Auto now emits classic TIFF for payloads under 4 GB, which is
the point: we own .tif/.tiff and were writing BigTIFF into it.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Compression and the horizontal predictor

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_write/encode.rs`
- Test: same file's test module

**Interfaces:**
- Consumes: `Compression` (Task 2), the `write_block` helper (Task 4).
- Produces: `write_block` gains a `&mut Compressor` parameter; a new `fn predict_horizontal<T: Sample>(bytes: &mut [u8], row_bytes: usize)`.

- [ ] **Step 1: Write the failing tests**

Append to `encode.rs`'s test module:

```rust
    #[test]
    fn every_compression_round_trips_the_same_pixels() {
        use crate::formats::tiff_write::options::Compression;
        let dir = tmpdir("compress");
        let img = Synth::new(37, 23, 1); // deliberately not a block multiple
        for (name, compression, tag) in [
            ("none", Compression::None, 1u64),
            ("lzw", Compression::Lzw, 5),
            ("deflate", Compression::Deflate, 8),
            ("packbits", Compression::Packbits, 32773),
        ] {
            let out = dir.join(format!("{name}.tif"));
            let opts = TiffWriteOptions {
                compression,
                ..Default::default()
            };
            // A small target forces several blocks, so this also proves
            // each block is compressed independently.
            write_tiff_with_target(&img, &out, &opts, 512).unwrap();

            let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
            assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), tag, "{name}");
            match dec.read_image().unwrap() {
                DecodingResult::F32(data) => {
                    assert_eq!(data.len(), 37 * 23, "{name}");
                    for (i, v) in data.iter().enumerate() {
                        assert!(
                            (v - i as f32 / (37.0 * 23.0)).abs() < 1e-7,
                            "{name} sample {i}"
                        );
                    }
                }
                other => panic!("{name}: expected F32, got {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compression_actually_shrinks_compressible_data() {
        use crate::formats::tiff_write::options::Compression;
        // A constant image compresses hard; an incompressible one would
        // make this assertion meaningless.
        struct Flat(u64, u64);
        impl crate::formats::PlanarRows<u8> for Flat {
            fn width(&self) -> u64 {
                self.0
            }
            fn height(&self) -> u64 {
                self.1
            }
            fn channels(&self) -> u64 {
                1
            }
            fn row(&self, _c: u64, _y: u64) -> &[u8] {
                const ZEROS: [u8; 512] = [0; 512];
                &ZEROS[..512]
            }
        }
        let dir = tmpdir("shrink");
        let img = Flat(512, 512);
        let plain = dir.join("plain.tif");
        let packed = dir.join("packed.tif");
        write_tiff(
            &img,
            &plain,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                ..Default::default()
            },
        )
        .unwrap();
        write_tiff(
            &img,
            &packed,
            &TiffWriteOptions {
                bit_depth: BitDepth::U8,
                compression: Compression::Deflate,
                ..Default::default()
            },
        )
        .unwrap();
        let plain_len = std::fs::metadata(&plain).unwrap().len();
        let packed_len = std::fs::metadata(&packed).unwrap().len();
        assert!(
            packed_len * 4 < plain_len,
            "deflate should compress a constant image hard: {packed_len} vs {plain_len}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_predictor_is_written_and_round_trips() {
        use crate::formats::tiff_write::options::Compression;
        let dir = tmpdir("predictor");
        let out = dir.join("out.tif");
        let img = Synth::new(16, 16, 3);
        let opts = TiffWriteOptions {
            bit_depth: BitDepth::U16,
            compression: Compression::Lzw,
            predictor: true,
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::Predictor).unwrap(), 2);
        match dec.read_image().unwrap() {
            DecodingResult::U16(data) => {
                assert_eq!(data.len(), 16 * 16 * 3);
                assert_eq!(data[0], 0);
                // Sample 767 of 768 -> round(767/768 * 65535).
                assert_eq!(data[767], 65450);
            }
            other => panic!("expected U16, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: FAIL — the `Compression` tag is 1 for every case and `Predictor` is absent.

- [ ] **Step 3: Compress each block**

In `encode.rs`, import the compressors and add the tag mapping:

```rust
use tiff::encoder::compression::{
    CompressionAlgorithm, Compressor, Deflate, DeflateLevel, Lzw, Packbits, Uncompressed,
};
```

Build the compressor once, from the options:

```rust
/// Build the block compressor and the `Compression` tag value for `opts`.
///
/// Each block gets a fresh encoder inside `Compressor::write_to`, which is
/// what makes strips and tiles independently decodable as TIFF requires.
fn compressor_for(opts: &TiffWriteOptions) -> (Compressor, u16) {
    match opts.compression {
        Compression::None => (Compressor::Uncompressed(Uncompressed), 1),
        Compression::Lzw => (Compressor::Lzw(Lzw), 5),
        Compression::Deflate => {
            let level = match opts.effective_deflate_level() {
                1..=3 => DeflateLevel::Fast,
                7..=9 => DeflateLevel::Best,
                _ => DeflateLevel::Balanced,
            };
            (Compressor::Deflate(Deflate::with_level(level)), 8)
        }
        Compression::Packbits => (Compressor::Packbits(Packbits), 32773),
    }
}
```

`DeflateLevel` is a three-variant enum (`Fast = 1`, `Balanced = 6`, `Best = 9`), verified in `tiff-0.11.3/src/encoder/compression/deflate.rs`, so the 1-9 level the options carry buckets into those three. Document that in `TiffWriteOptions::deflate_level`'s doc comment so the dialog's spin box does not promise finer control than exists.

Change `write_block` to compress:

```rust
fn write_block<K, W>(
    dir: &mut tiff::encoder::DirectoryEncoder<'_, W, K>,
    raw: &[u8],
    compressor: &mut Compressor,
    scratch: &mut Vec<u8>,
    offsets: &mut Vec<K::OffsetType>,
    byte_counts: &mut Vec<K::OffsetType>,
) -> tiff::TiffResult<()>
where
    K: TiffKind,
    W: Write + Seek,
{
    scratch.clear();
    compressor.write_to(scratch, raw)?;
    let offset = dir.write_data(&scratch[..])?;
    offsets.push(K::convert_offset(offset)?);
    byte_counts.push(K::OffsetType::try_from(scratch.len())?);
    Ok(())
}
```

Thread a `scratch: Vec<u8>` and the compressor through `encode`, and replace the hard-coded compression tag:

```rust
    let (mut compressor, compression_tag) = compressor_for(opts);
    // ...
    dir.write_tag(Tag::Compression, compression_tag).map_err(terr)?;
```

- [ ] **Step 4: Apply the predictor**

The predictor differences each row's samples against the previous sample of the same channel, in-place on the raw block bytes before compression:

```rust
/// Apply TIFF's horizontal differencing predictor (tag 317 = 2) in place.
///
/// Each sample becomes the difference from the sample `channels` positions
/// earlier in the same row, so runs of similar values compress far better.
/// Integer sample types only — `TiffWriteOptions::validate` rejects the
/// predictor for float formats, since differencing IEEE bit patterns is not
/// what tag 317 means.
fn predict_horizontal<T: Sample>(block: &mut [u8], samples_per_row: usize, channels: usize) {
    let width = T::BITS_PER_SAMPLE as usize / 8;
    let row_bytes = samples_per_row * width;
    for row in block.chunks_exact_mut(row_bytes) {
        // Walk backwards so each subtraction sees the original predecessor.
        for i in (channels * width..row_bytes).rev() {
            row[i] = row[i].wrapping_sub(row[i - channels * width]);
        }
    }
}
```

That byte-wise formulation is only correct for 8-bit samples. For 16- and 32-bit samples the difference must be taken on whole samples, so write it generically instead: gather the block as `Vec<T>` rather than `Vec<u8>` when the predictor is on, difference the values with `wrapping_sub` on the integer type, then serialise. Add a `wrapping_sub_sample` method to the `Sample` trait for this, defaulting to `unimplemented!()` for `f32`/`f64` (validation guarantees they never reach it):

```rust
    /// Wrapping subtraction, used by the horizontal predictor. Float
    /// formats never reach this: `TiffWriteOptions::validate` rejects the
    /// predictor for them.
    fn wrapping_sub_sample(self, other: Self) -> Self;
```

Then in `encode`, when `opts.predictor` is set, run the difference over the gathered samples before `push_ne` serialisation. Restructure `gather_strip`/`gather_tile` to fill a `Vec<T>` and add a `serialise(samples: &[T], out: &mut Vec<u8>)` step, so the predictor has typed values to work with.

Finally write the tag:

```rust
    if opts.predictor {
        dir.write_tag(Tag::Predictor, 2u16).map_err(terr)?;
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p mpp-core tiff_write`
Expected: PASS, including the three new tests and all of Task 4's.

- [ ] **Step 6: Run the gates and commit**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): LZW, Deflate and PackBits compression plus the predictor

Each block compresses through a fresh encoder, so strips and tiles stay
independently decodable. The horizontal predictor differences whole
samples, not bytes, and validation keeps it away from float formats where
tag 317 has no meaning.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Tiled output and container selection

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_write/encode.rs`
- Test: same file's test module

**Interfaces:**
- Consumes: everything from Tasks 2-5. The tile path was written in Task 4; this task proves it and adds the container assertions.
- Produces: no new signatures.

- [ ] **Step 1: Write the failing tests**

Append to `encode.rs`'s test module:

```rust
    #[test]
    fn tiled_output_writes_tile_tags_and_round_trips() {
        use crate::formats::tiff_write::options::Layout;
        let dir = tmpdir("tiles");
        let out = dir.join("out.tif");
        // 300x200 with 128-pixel tiles: 3 tile columns x 2 tile rows, so
        // the right column and bottom row are both partial, including the
        // bottom-right corner tile which is partial in both axes.
        let img = Synth::new(300, 200, 1);
        let opts = TiffWriteOptions {
            layout: Layout::Tiles { size: 128 },
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::TileWidth).unwrap(), 128);
        assert_eq!(dec.get_tag_u64(Tag::TileLength).unwrap(), 128);
        assert_eq!(dec.get_tag_u64_vec(Tag::TileOffsets).unwrap().len(), 6);
        assert_eq!(dec.get_tag_u64_vec(Tag::TileByteCounts).unwrap().len(), 6);
        // Every tile is full size on disk, edge tiles included.
        for count in dec.get_tag_u64_vec(Tag::TileByteCounts).unwrap() {
            assert_eq!(count, 128 * 128 * 4);
        }
        assert!(dec.get_tag(Tag::StripOffsets).is_err(), "no strip tags in a tiled file");

        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => {
                assert_eq!(data.len(), 300 * 200);
                for (i, v) in data.iter().enumerate() {
                    assert!((v - i as f32 / 60_000.0).abs() < 1e-7, "sample {i}");
                }
            }
            other => panic!("expected F32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tiled_output_compresses_per_tile() {
        use crate::formats::tiff_write::options::{Compression, Layout};
        let dir = tmpdir("tiles-lzw");
        let out = dir.join("out.tif");
        let img = Synth::new(300, 200, 3);
        let opts = TiffWriteOptions {
            layout: Layout::Tiles { size: 256 },
            compression: Compression::Lzw,
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::Compression).unwrap(), 5);
        match dec.read_image().unwrap() {
            DecodingResult::F32(data) => assert_eq!(data.len(), 300 * 200 * 3),
            other => panic!("expected F32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn container_choice_shows_in_the_file_header() {
        use crate::formats::tiff_write::options::Container;
        let dir = tmpdir("container");
        let img = Synth::new(8, 8, 1);

        let classic = dir.join("classic.tif");
        write_tiff(
            &img,
            &classic,
            &TiffWriteOptions {
                container: Container::Classic,
                ..Default::default()
            },
        )
        .unwrap();
        let head = std::fs::read(&classic).unwrap();
        assert_eq!(&head[0..4], &[0x49, 0x49, 0x2A, 0x00], "classic TIFF magic");

        let big = dir.join("big.tif");
        write_tiff(
            &img,
            &big,
            &TiffWriteOptions {
                container: Container::Big,
                ..Default::default()
            },
        )
        .unwrap();
        let head = std::fs::read(&big).unwrap();
        assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");

        // Auto on a small image picks classic (spec finding 3: we own
        // .tif/.tiff now, so a small save must stay universally readable).
        let auto = dir.join("auto.tif");
        write_tiff(&img, &auto, &TiffWriteOptions::default()).unwrap();
        let head = std::fs::read(&auto).unwrap();
        assert_eq!(&head[0..4], &[0x49, 0x49, 0x2A, 0x00], "Auto picks classic when it fits");

        std::fs::remove_dir_all(&dir).unwrap();
    }
```

The magic-byte assertions above assume a little-endian host. Guard them with `#[cfg(target_endian = "little")]` on the test function, and note in a comment that a big-endian host writes `MM` — the crate takes the byte order from `target_endian`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: the tile tests may already pass from Task 4's implementation; the container test fails if `Container::Classic` was not threaded to `TiffKindStandard`. Fix whatever fails — if all three pass immediately, that is a legitimate outcome of Task 4 having implemented both paths, and the tests are still the gate that proves it.

- [ ] **Step 3: Fix whatever the tests catch**

Likely candidates: tile ordering must be row-major over tile rows then tile columns (TIFF 6.0 §15); `TileByteCounts` must reflect the *compressed* size while every tile's *uncompressed* content is full-size; a tiled file must not also carry `RowsPerStrip`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_write`
Expected: PASS.

- [ ] **Step 5: Run the gates and commit**

```bash
git add -A crates/mpp-core/
git commit -m "test(core): tiled output and container selection

Covers the partial edge tiles in both axes and the corner, per-tile
compression, the absence of strip tags in a tiled file, and the magic
bytes each container choice produces.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Alpha channels and the wide sample formats

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_write/encode.rs`
- Test: same file's test module

**Interfaces:**
- Consumes: `AlphaMode` (Task 2), `Sample` (Task 3).
- Produces: no new signatures; `encode` gains the `ExtraSamples` tag.

- [ ] **Step 1: Write the failing tests**

Append to `encode.rs`'s test module:

```rust
    /// A planar source of an arbitrary sample type, so the wide formats can
    /// be exercised without going through the unit-range f32 conversion.
    struct Typed<T> {
        w: u64,
        h: u64,
        c: u64,
        planes: Vec<Vec<T>>,
    }

    impl<T: Copy> crate::formats::PlanarRows<T> for Typed<T> {
        fn width(&self) -> u64 {
            self.w
        }
        fn height(&self) -> u64 {
            self.h
        }
        fn channels(&self) -> u64 {
            self.c
        }
        fn row(&self, c: u64, y: u64) -> &[T] {
            let start = (y * self.w) as usize;
            &self.planes[c as usize][start..start + self.w as usize]
        }
    }

    #[test]
    fn rgba_writes_extra_samples_and_keeps_four_channels() {
        use crate::formats::tiff_write::options::AlphaMode;
        let dir = tmpdir("rgba");
        for (mode, expected) in [
            (AlphaMode::Unassociated, 2u64),
            (AlphaMode::Associated, 1u64),
        ] {
            let out = dir.join(format!("{expected}.tif"));
            let img = Synth::new(6, 5, 4);
            let opts = TiffWriteOptions {
                alpha: mode,
                ..Default::default()
            };
            write_tiff(&img, &out, &opts).unwrap();

            let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
            assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 4);
            assert_eq!(dec.get_tag_u64_vec(Tag::ExtraSamples).unwrap(), vec![expected]);
            assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 2);
            assert_eq!(
                dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(),
                vec![32, 32, 32, 32]
            );
            match dec.read_image().unwrap() {
                DecodingResult::F32(d) => assert_eq!(d.len(), 6 * 5 * 4),
                other => panic!("expected F32, got {other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gray_plus_alpha_stays_black_is_zero() {
        use crate::formats::tiff_write::options::AlphaMode;
        let dir = tmpdir("graya");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 2);
        write_tiff(
            &img,
            &out,
            &TiffWriteOptions {
                alpha: AlphaMode::Unassociated,
                ..Default::default()
            },
        )
        .unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64(Tag::PhotometricInterpretation).unwrap(), 1);
        assert_eq!(dec.get_tag_u64(Tag::SamplesPerPixel).unwrap(), 2);
        assert_eq!(dec.get_tag_u64_vec(Tag::ExtraSamples).unwrap(), vec![2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn u32_samples_round_trip_exactly() {
        // The whole point of protocol v3: a u32 source must survive
        // unchanged, including values f32 cannot represent.
        let dir = tmpdir("u32");
        let out = dir.join("out.tif");
        let values: Vec<u32> = vec![0, 1, 16_777_217, 4_000_000_001, u32::MAX, 12345];
        let img = Typed {
            w: 6,
            h: 1,
            c: 1,
            planes: vec![values.clone()],
        };
        write_tiff(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::U32,
                ..Default::default()
            },
        )
        .unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(), vec![32]);
        assert_eq!(dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(), vec![1]);
        match dec.read_image().unwrap() {
            DecodingResult::U32(d) => assert_eq!(d, values),
            other => panic!("expected U32, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn f64_samples_round_trip_exactly() {
        let dir = tmpdir("f64");
        let out = dir.join("out.tif");
        let values: Vec<f64> = vec![0.0, 1.0, std::f64::consts::PI, 1e-300, -2.5];
        let img = Typed {
            w: 5,
            h: 1,
            c: 1,
            planes: vec![values.clone()],
        };
        write_tiff(
            &img,
            &out,
            &TiffWriteOptions {
                bit_depth: BitDepth::F64,
                ..Default::default()
            },
        )
        .unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u64_vec(Tag::BitsPerSample).unwrap(), vec![64]);
        assert_eq!(dec.get_tag_u64_vec(Tag::SampleFormat).unwrap(), vec![3]);
        match dec.read_image().unwrap() {
            DecodingResult::F64(d) => assert_eq!(d, values),
            other => panic!("expected F64, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: the alpha tests FAIL (`ExtraSamples` is absent); the u32/f64 tests may pass already if Task 4's generics are right — either way they are the gate.

- [ ] **Step 3: Write the `ExtraSamples` tag**

In `encode`, after the `PhotometricInterpretation` tag:

```rust
    // TIFF 6.0 §18: ExtraSamples describes every sample beyond the ones the
    // photometric interpretation accounts for. We write at most one, the
    // alpha channel, and its value says whether the colour channels are
    // premultiplied by it.
    match opts.alpha {
        AlphaMode::None => {}
        AlphaMode::Associated => dir.write_tag(Tag::ExtraSamples, &[1u16][..]).map_err(terr)?,
        AlphaMode::Unassociated => dir.write_tag(Tag::ExtraSamples, &[2u16][..]).map_err(terr)?,
    }
```

`photometric(colour_channels)` from Task 4 already returns `1` for gray+alpha and `2` for RGBA, because `colour_channels` excludes the alpha channel.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_write`
Expected: PASS.

- [ ] **Step 5: Run the gates and commit**

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): alpha channels and exact u32/f64 output

Gray+alpha and RGBA write ExtraSamples with the association the caller
chose; u32 and f64 sources round-trip bit-exactly, including values f32
cannot represent, which is what makes CanStore32Bit/CanStoreDouble
truthful once protocol v3 delivers native bands.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Embedded metadata — ICC, text tags, resolution

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_write/encode.rs`
- Test: same file's test module

**Interfaces:**
- Consumes: `TiffWriteOptions`'s `icc_profile`, `description`, `copyright`, `software`, `resolution` (Task 2).
- Produces: no new signatures.

- [ ] **Step 1: Write the failing test**

Append to `encode.rs`'s test module:

```rust
    #[test]
    fn metadata_tags_are_written_when_requested() {
        use crate::formats::tiff_write::options::{Resolution, ResolutionUnit};
        let dir = tmpdir("meta");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 1);
        let icc: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let opts = TiffWriteOptions {
            icc_profile: Some(icc.clone()),
            description: Some("M31 mosaic".to_string()),
            copyright: Some("Copyright (c) 2026 Astrometrical".to_string()),
            software: Some("Mega Pixel Processor 0.1.0".to_string()),
            resolution: Some(Resolution {
                x: 300.0,
                y: 300.0,
                unit: ResolutionUnit::Inch,
            }),
            ..Default::default()
        };
        write_tiff(&img, &out, &opts).unwrap();

        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(dec.get_tag_u8_vec(Tag::IccProfile).unwrap(), icc);
        assert_eq!(
            dec.get_tag_ascii_string(Tag::ImageDescription).unwrap(),
            "M31 mosaic"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Copyright).unwrap(),
            "Copyright (c) 2026 Astrometrical"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Software).unwrap(),
            "Mega Pixel Processor 0.1.0"
        );
        assert_eq!(dec.get_tag_f64(Tag::XResolution).unwrap(), 300.0);
        assert_eq!(dec.get_tag_f64(Tag::YResolution).unwrap(), 300.0);
        assert_eq!(dec.get_tag_u64(Tag::ResolutionUnit).unwrap(), 2); // inch
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_tags_are_absent_when_not_requested() {
        let dir = tmpdir("nometa");
        let out = dir.join("out.tif");
        let img = Synth::new(4, 4, 1);
        write_tiff(&img, &out, &TiffWriteOptions::default()).unwrap();
        let mut dec = Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
        for tag in [
            Tag::IccProfile,
            Tag::ImageDescription,
            Tag::Copyright,
            Tag::Software,
            Tag::XResolution,
        ] {
            assert!(dec.get_tag(tag).is_err(), "{tag:?} should be absent");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-core tiff_write::encode`
Expected: FAIL — none of these tags are written.

- [ ] **Step 3: Write the tags**

In `encode`, after the layout tags:

```rust
    if let Some(icc) = &opts.icc_profile {
        // Tag 34675 is UNDEFINED-typed, but `impl TiffValue for [u8]`
        // declares Type::BYTE, so `write_tag` would label it wrongly. Build
        // the entry with an explicit type instead and splice it in.
        let entry = dir
            .write_entry_bytes(tiff::tags::Type::UNDEFINED, &icc[..])
            .map_err(terr)?;
        let mut extra = tiff::Directory::empty();
        extra.extend([(Tag::IccProfile, entry)]);
        dir.extend_from(&extra);
    }
    for (tag, value) in [
        (Tag::ImageDescription, &opts.description),
        (Tag::Copyright, &opts.copyright),
        (Tag::Software, &opts.software),
    ] {
        if let Some(text) = value {
            // Pass the bare string: `impl TiffValue for str` appends the
            // terminating NUL itself and *rejects* a value that already
            // contains one, or any non-ASCII byte. Callers must sanitise
            // before they get here (Task 16 does that in the module).
            dir.write_tag(tag, text.as_str()).map_err(terr)?;
        }
    }
    if let Some(res) = &opts.resolution {
        // RATIONAL tags: a denominator of 1000 keeps the common values
        // (72, 300, 1200 dpi) exact.
        let rat = |v: f64| tiff::encoder::Rational {
            n: (v * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32,
            d: 1000,
        };
        dir.write_tag(Tag::XResolution, rat(res.x)).map_err(terr)?;
        dir.write_tag(Tag::YResolution, rat(res.y)).map_err(terr)?;
        let unit: u16 = match res.unit {
            ResolutionUnit::Inch => 2,
            ResolutionUnit::Centimeter => 3,
        };
        dir.write_tag(Tag::ResolutionUnit, unit).map_err(terr)?;
    }
```

Facts verified against `tiff` 0.11.3 so you do not have to re-derive them: `Rational { n: u32, d: u32 }` lives in `encoder::tiff_value` and is re-exported as `tiff::encoder::Rational`; `Type::UNDEFINED` is variant 7 of `tiff::tags::Type`; `DirectoryEncoder::write_entry_bytes`, `Directory::empty`/`extend`, and `DirectoryEncoder::extend_from` are all public; and `impl TiffValue for str` writes `Type::ASCII` with its own NUL terminator.

If the `Directory`/`Entry` splice proves awkward, falling back to `dir.write_tag(Tag::IccProfile, &icc[..])` (which labels the tag `BYTE` rather than `UNDEFINED`) still produces a file every reader we care about accepts — both types are single-byte and the on-disk value bytes are identical. Take the fallback only if the typed path costs more than an hour, and note the deviation in the commit message.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_write`
Expected: PASS.

- [ ] **Step 5: Run the gates and commit**

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): ICC profile, text tags and resolution on write

Closes the stock dialog's Embedded Data and Miscellaneous sections in the
engine. Tags are written only when requested, so a default save produces
the same minimal IFD it always did.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

**Checkpoint — end of Group A.** `cargo test` must be fully green and `cargo fmt --check`, `cargo clippy --all-targets`, `cargo doc` warning-free before Group B starts. The engine is now feature-complete; everything after this is plumbing.

---

## Group B — CLI surface

### Task 9: `mpp convert` flags for the new options

**Files:**
- Modify: `crates/mpp/src/main.rs`
- Modify: `crates/mpp-core/src/convert.rs` (`ConvertOptions` carries the writer options through)
- Test: `crates/mpp/tests/convert.rs`

**Interfaces:**
- Consumes: `TiffWriteOptions` and its enums (Task 2).
- Produces: `ConvertOptions` gains `pub tiff: TiffWriteOptions`; clap mirrors `CompressionArg`, `ContainerArg`, `BitDepthArg` (widened) in `crates/mpp/src/main.rs`.

- [ ] **Step 1: Write the failing test**

Add to `crates/mpp/tests/convert.rs`, following the existing tests' shape for building an input and invoking the binary:

```rust
#[test]
fn convert_writes_a_tiled_deflate_bigtiff() {
    let dir = common::tmpdir("cli-tiled");
    let src = dir.join("in.xisf");
    common::synth_xisf(&src, 600, 400, 1);
    let out = dir.join("out.tif");

    let status = common::mpp()
        .args([
            "convert",
            src.to_str().unwrap(),
            out.to_str().unwrap(),
            "--tiled",
            "--tile-size",
            "256",
            "--compression",
            "deflate",
            "--container",
            "big",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let head = std::fs::read(&out).unwrap();
    assert_eq!(&head[0..4], &[0x49, 0x49, 0x2B, 0x00], "BigTIFF magic");

    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::TileWidth).unwrap(), 256);
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::Compression).unwrap(), 8);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn convert_rejects_a_wide_sample_format_for_png() {
    let dir = common::tmpdir("cli-reject");
    let src = dir.join("in.xisf");
    common::synth_xisf(&src, 8, 8, 1);
    let out = dir.join("out.png");

    let output = common::mpp()
        .args([
            "convert",
            src.to_str().unwrap(),
            out.to_str().unwrap(),
            "--bit-depth",
            "f64",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("f64") || err.contains("bit depth"),
        "error should name the rejected sample format, got: {err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
```

Match the helper names to whatever `crates/mpp/tests/common/mod.rs` actually provides — read it first and use its real API rather than the names above.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp --test convert`
Expected: FAIL — `--tiled` is not a recognised argument.

- [ ] **Step 3: Add the clap mirrors**

In `crates/mpp/src/main.rs`, widen `BitDepthArg` and add the new mirror enums, following the existing `BitDepthArg`/`ModeArg` pattern exactly (a `#[derive(Clone, Copy, ValueEnum)]` enum plus a `From` impl, keeping clap out of `mpp-core`):

```rust
/// CLI mirror of [`BitDepth`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum BitDepthArg {
    /// 32-bit IEEE float (BigTIFF/TIFF only).
    F32,
    /// 64-bit IEEE float (BigTIFF/TIFF only).
    F64,
    /// 32-bit unsigned integer (BigTIFF/TIFF only).
    U32,
    /// 16-bit unsigned integer.
    U16,
    /// 8-bit unsigned integer.
    U8,
}

/// CLI mirror of [`mpp_core::formats::tiff_write::Compression`].
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CompressionArg {
    /// No compression (default).
    None,
    /// LZW, lossless and widely supported.
    Lzw,
    /// Deflate/zlib, lossless, usually the smallest.
    Deflate,
    /// PackBits run-length encoding.
    Packbits,
}

/// CLI mirror of [`mpp_core::formats::tiff_write::Container`].
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ContainerArg {
    /// Classic TIFF when the image fits, BigTIFF otherwise (default).
    Auto,
    /// Classic 32-bit TIFF; fails for images past 4 GB.
    Classic,
    /// BigTIFF, always.
    Big,
}
```

with `From` impls mapping each to its `mpp-core` counterpart, mirroring the existing `impl From<BitDepthArg> for BitDepth`.

- [ ] **Step 4: Add the arguments**

To the `Convert` variant of `enum Command`:

```rust
        /// Block compression for TIFF output.
        #[arg(long, value_enum, default_value_t = CompressionArg::None)]
        compression: CompressionArg,

        /// Deflate level 1-9 (buckets to fast/balanced/best); ignored unless
        /// --compression deflate.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=9))]
        deflate_level: Option<u8>,

        /// Apply the horizontal differencing predictor. Integer sample
        /// formats only.
        #[arg(long)]
        predictor: bool,

        /// Write tiles instead of strips.
        #[arg(long)]
        tiled: bool,

        /// Tile edge in pixels; a power of two in 128..=1024.
        #[arg(long, default_value_t = 256)]
        tile_size: u32,

        /// TIFF container variant.
        #[arg(long, value_enum, default_value_t = ContainerArg::Auto)]
        container: ContainerArg,
```

- [ ] **Step 5: Thread them to the writer**

In `crates/mpp-core/src/convert.rs`, replace `ConvertOptions`'s standalone `bit_depth: Option<BitDepth>` usage at the write site with a full `TiffWriteOptions`. Keep `bit_depth` on `ConvertOptions` as-is — the vips paths read it — and add:

```rust
    /// TIFF/BigTIFF writer options. Ignored for the vips-backed formats,
    /// which have their own saver options; `bit_depth` above is the field
    /// both paths share.
    pub tiff: TiffWriteOptions,
```

At the two `write_tiff` call sites, build the options from `opts.tiff` with `bit_depth` overridden by the resolved depth:

```rust
    let tiff_opts = TiffWriteOptions {
        bit_depth: depth,
        ..opts.tiff.clone()
    };
```

The vips intermediate at `convert.rs:249` must keep writing something vips can read: force `Container::Big` and `Compression::None` there, and add a comment saying the intermediate is a private handoff file, not a user-visible output, so it does not honour the user's container or compression choice.

In `crates/mpp/src/main.rs`, map the new arguments into `ConvertOptions.tiff`, translating `tiled`/`tile_size` into `Layout::Tiles { size }` or `Layout::Strips`.

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS, including both new CLI tests.

- [ ] **Step 7: Run the gates and commit**

```bash
git add -A crates/
git commit -m "feat(cli): expose compression, tiling and container on mpp convert

Every writer option the PixInsight dialog will offer is reachable from
the CLI, which is what makes them testable end to end without a
PixInsight install.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## Group C — protocol v3

### Task 10: `InitMessage` v3 fields and worker validation

**Files:**
- Modify: `crates/mpp-ipc/src/protocol.rs` (version constant, `InitMessage`)
- Modify: `crates/mpp-ipc-worker/src/main.rs` (`validate_init`)
- Test: `crates/mpp-ipc/src/protocol.rs`'s test module and `crates/mpp-ipc-worker/src/main.rs`'s

**Interfaces:**
- Consumes: the option enums (Task 2).
- Produces: `IPC_PROTOCOL_VERSION = 3`; `InitMessage` with the fields in the spec's protocol table; `fn validate_init(init: &InitMessage) -> Result<WriteSetup, String>` where `WriteSetup { band_format: BitDepth, options: TiffWriteOptions }` — the output sample format lives in `options.bit_depth`, so it is not repeated.

- [ ] **Step 1: Write the failing tests**

Add to `crates/mpp-ipc/src/protocol.rs`'s test module:

```rust
    #[test]
    fn a_v2_init_payload_still_deserializes() {
        // Wire compatibility: every v3 field is #[serde(default)], so a
        // payload written by a v2 host parses into today's behaviour.
        let v2 = r#"{
            "protocol_version": 2,
            "worker_version": "0.1.0",
            "output_path": "/tmp/o.tif",
            "width": 10, "height": 10, "channels": 3,
            "bit_depth": "f32",
            "shm_name": "/mpp-x", "slot_bytes": 1200, "slots": 2
        }"#;
        let init: InitMessage = serde_json::from_str(v2).unwrap();
        assert_eq!(init.band_format, "f32");
        assert_eq!(init.container, "auto");
        assert_eq!(init.layout, "strips");
        assert_eq!(init.compression, "none");
        assert_eq!(init.alpha, "none");
        assert!(init.icc_profile.is_empty());
        assert!(init.description.is_empty());
        assert_eq!(init.resolution_unit, "inch");
    }

    #[test]
    fn v3_fields_round_trip_through_json() {
        let init = InitMessage {
            protocol_version: IPC_PROTOCOL_VERSION,
            worker_version: "0.1.0".to_string(),
            output_path: "/tmp/o.tif".to_string(),
            width: 4, height: 4, channels: 4,
            bit_depth: "u32".to_string(),
            shm_name: "/mpp-y".to_string(),
            slot_bytes: 64, slots: 2,
            mode: SessionMode::Write,
            band_format: "u32".to_string(),
            container: "big".to_string(),
            layout: "tiles".to_string(),
            tile_size: 512,
            compression: "deflate".to_string(),
            deflate_level: 9,
            predictor: true,
            alpha: "unassociated".to_string(),
            icc_profile: "AAEC".to_string(),
            description: "M31".to_string(),
            copyright: "(c) 2026".to_string(),
            software: "mpp".to_string(),
            x_resolution: 300.0,
            y_resolution: 300.0,
            resolution_unit: "cm".to_string(),
        };
        let json = serde_json::to_string(&init).unwrap();
        let back: InitMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, init);
        assert_eq!(IPC_PROTOCOL_VERSION, 3);
    }
```

Add to `crates/mpp-ipc-worker/src/main.rs`'s test module:

```rust
    #[test]
    fn validate_init_accepts_the_widened_options() {
        let mut init = test_init(); // existing helper; extend it if needed
        init.channels = 4;
        init.alpha = "associated".to_string();
        init.bit_depth = "f64".to_string();
        init.band_format = "f64".to_string();
        init.layout = "tiles".to_string();
        init.tile_size = 256;
        init.compression = "lzw".to_string();
        let setup = validate_init(&init).unwrap();
        assert_eq!(setup.band_format, BitDepth::F64);
        assert_eq!(setup.options.bit_depth, BitDepth::F64);
    }

    #[test]
    fn validate_init_rejects_inconsistent_options() {
        let cases: Vec<(&str, Box<dyn Fn(&mut InitMessage)>)> = vec![
            ("channels", Box::new(|i: &mut InitMessage| i.channels = 5)),
            (
                "alpha",
                Box::new(|i: &mut InitMessage| {
                    i.channels = 3;
                    i.alpha = "associated".to_string();
                }),
            ),
            (
                "band_format",
                Box::new(|i: &mut InitMessage| i.band_format = "u4".to_string()),
            ),
            (
                "tile_size",
                Box::new(|i: &mut InitMessage| {
                    i.layout = "tiles".to_string();
                    i.tile_size = 300;
                }),
            ),
            (
                "predictor",
                Box::new(|i: &mut InitMessage| {
                    i.predictor = true;
                    i.bit_depth = "f32".to_string();
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut init = test_init();
            mutate(&mut init);
            let err = validate_init(&init)
                .expect_err(&format!("{name} should have been rejected"));
            assert!(!err.is_empty(), "{name}: error message must not be empty");
        }
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mpp-ipc && cargo test -p mpp-ipc-worker`
Expected: FAIL to compile — the fields do not exist.

- [ ] **Step 3: Extend `InitMessage`**

In `crates/mpp-ipc/src/protocol.rs`, bump the constant and append the fields *after* `mode`, keeping the existing "placed last so partial constructors keep compiling" rationale intact:

```rust
pub const IPC_PROTOCOL_VERSION: u32 = 3;
```

```rust
    /// **(v3)** Sample type of the pixel data the host places in shm slots:
    /// `"f32"` (the v1/v2 behaviour and the default), `"f64"`, `"u8"`,
    /// `"u16"` or `"u32"`. Independent of [`InitMessage::bit_depth`], which
    /// names the *output file's* sample format. Absent in a v2 payload,
    /// which defaults to `"f32"`.
    #[serde(default = "default_band_format")]
    pub band_format: String,

    /// **(v3)** TIFF container: `"auto"` (default), `"classic"` or `"big"`.
    #[serde(default = "default_container")]
    pub container: String,

    /// **(v3)** Block layout: `"strips"` (default) or `"tiles"`.
    #[serde(default = "default_layout")]
    pub layout: String,

    /// **(v3)** Tile edge in pixels when `layout == "tiles"`; a power of two
    /// in `128..=1024`.
    #[serde(default = "default_tile_size")]
    pub tile_size: u32,

    /// **(v3)** Block compression: `"none"` (default), `"lzw"`, `"deflate"`
    /// or `"packbits"`.
    #[serde(default = "default_compression")]
    pub compression: String,

    /// **(v3)** Deflate level 1-9; ignored unless `compression == "deflate"`.
    #[serde(default = "default_deflate_level")]
    pub deflate_level: u8,

    /// **(v3)** Apply the horizontal differencing predictor. Rejected for a
    /// floating-point `bit_depth`.
    #[serde(default)]
    pub predictor: bool,

    /// **(v3)** Alpha association: `"none"` (default), `"unassociated"` or
    /// `"associated"`. Must agree with `channels` in both directions.
    #[serde(default = "default_alpha")]
    pub alpha: String,

    /// **(v3)** Base64-encoded ICC profile to embed, empty when absent.
    /// Capped at 16 MiB decoded.
    #[serde(default)]
    pub icc_profile: String,

    /// **(v3)** `ImageDescription` (TIFF tag 270), empty when absent.
    #[serde(default)]
    pub description: String,

    /// **(v3)** `Copyright` (TIFF tag 33432), empty when absent.
    #[serde(default)]
    pub copyright: String,

    /// **(v3)** `Software` (TIFF tag 305), empty when absent.
    #[serde(default)]
    pub software: String,

    /// **(v3)** Horizontal resolution in pixels per
    /// [`InitMessage::resolution_unit`]; `0` means unknown and writes no
    /// resolution tags.
    #[serde(default)]
    pub x_resolution: f64,

    /// **(v3)** Vertical resolution; see [`InitMessage::x_resolution`].
    #[serde(default)]
    pub y_resolution: f64,

    /// **(v3)** Resolution unit: `"inch"` (default) or `"cm"`.
    #[serde(default = "default_resolution_unit")]
    pub resolution_unit: String,
```

with the small `fn default_*()` helpers each `serde(default = ...)` names, each documented.

Base64 decoding needs a dependency. `mpp-ipc` has none today; add `base64 = "0.22"` to `[workspace.dependencies]` and to `crates/mpp-ipc/Cargo.toml`, or hand-roll a 40-line decoder in `protocol.rs` if adding a dependency is unwelcome. Prefer the crate: it is small, widely audited, and the hand-rolled alternative is exactly the kind of code that hides a bug.

- [ ] **Step 4: Widen `validate_init`**

In `crates/mpp-ipc-worker/src/main.rs`, change the return type to a struct carrying everything the encode step needs, and add the checks. Parse each string field into its `mpp-core` enum, then delegate the consistency rules to `TiffWriteOptions::validate` so there is exactly one place they live:

```rust
/// Everything `run_write` needs from a validated `Init`.
struct WriteSetup {
    /// Sample type of the bands arriving over shm.
    band_format: BitDepth,
    /// Fully populated writer options, including the output sample format.
    options: TiffWriteOptions,
}

fn validate_init(init: &InitMessage) -> Result<WriteSetup, String> {
    // ... existing width/height/slots/slot_bytes checks ...

    let parse_depth = |s: &str, field: &str| -> Result<BitDepth, String> {
        match s {
            "f32" => Ok(BitDepth::F32),
            "f64" => Ok(BitDepth::F64),
            "u32" => Ok(BitDepth::U32),
            "u16" => Ok(BitDepth::U16),
            "u8" => Ok(BitDepth::U8),
            other => Err(format!(
                "unknown {field} {other:?} (expected \"f32\", \"f64\", \"u32\", \"u16\" or \"u8\")"
            )),
        }
    };
    let bit_depth = parse_depth(&init.bit_depth, "bit_depth")?;
    let band_format = parse_depth(&init.band_format, "band_format")?;

    let options = TiffWriteOptions {
        bit_depth,
        container: match init.container.as_str() {
            "auto" => Container::Auto,
            "classic" => Container::Classic,
            "big" => Container::Big,
            other => return Err(format!("unknown container {other:?}")),
        },
        layout: match init.layout.as_str() {
            "strips" => Layout::Strips,
            "tiles" => Layout::Tiles {
                size: init.tile_size,
            },
            other => return Err(format!("unknown layout {other:?}")),
        },
        compression: match init.compression.as_str() {
            "none" => Compression::None,
            "lzw" => Compression::Lzw,
            "deflate" => Compression::Deflate,
            "packbits" => Compression::Packbits,
            other => return Err(format!("unknown compression {other:?}")),
        },
        deflate_level: Some(init.deflate_level),
        predictor: init.predictor,
        alpha: match init.alpha.as_str() {
            "none" => AlphaMode::None,
            "unassociated" => AlphaMode::Unassociated,
            "associated" => AlphaMode::Associated,
            other => return Err(format!("unknown alpha mode {other:?}")),
        },
        icc_profile: decode_icc(&init.icc_profile)?,
        description: non_empty(&init.description),
        copyright: non_empty(&init.copyright),
        software: non_empty(&init.software),
        resolution: (init.x_resolution > 0.0 && init.y_resolution > 0.0).then(|| Resolution {
            x: init.x_resolution,
            y: init.y_resolution,
            unit: match init.resolution_unit.as_str() {
                "cm" => ResolutionUnit::Centimeter,
                _ => ResolutionUnit::Inch,
            },
        }),
    };
    // One place owns the consistency rules: mpp-core's validator, the same
    // one the CLI path goes through.
    options
        .validate(init.width, init.height, init.channels)
        .map_err(|e| e.to_string())?;

    // slot_bytes alignment now depends on the band sample width, not f32.
    let sample_bytes = band_format.bytes() as u64;
    if init.slot_bytes == 0 || !init.slot_bytes.is_multiple_of(sample_bytes) {
        return Err(format!(
            "slot_bytes {} must be nonzero and a multiple of {sample_bytes} \
             ({} alignment)",
            init.slot_bytes, init.band_format
        ));
    }

    Ok(WriteSetup { band_format, options })
}
```

`non_empty(&str) -> Option<String>` returns `None` for an empty string and `Some(owned)` otherwise; `decode_icc(&str) -> Result<Option<Vec<u8>>, String>` returns `None` for an empty string, decodes base64 otherwise, and rejects anything over 16 MiB decoded or that fails to decode, naming which. Write both as small documented free functions beside `validate_init`.

Note the `slot_bytes` rule changed: it was hard-coded to a multiple of 4 for f32. Keep the same intent — alignment to the band sample width — and update `docs/pi-protocol.md` §6 to match in Task 12.

Also decode `icc_profile` here, rejecting a blob over 16 MiB or invalid base64 with a clear message, so the encode step receives bytes.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p mpp-ipc && cargo test -p mpp-ipc-worker`
Expected: PASS. The worker's `run_write` will not compile until Task 11 — if the borrow is too tangled to split, do Tasks 10 and 11 as one commit.

- [ ] **Step 6: Commit**

```bash
git add -A crates/
git commit -m "feat(ipc): protocol v3 Init fields for native bands and write options

band_format decouples the shm sample type from the output file's, which
is what lets u32 and f64 reach the encoder intact. Every new field is
serde-defaulted, so a v2 payload still parses into today's behaviour.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 11: Generic staging and the worker write path

**Files:**
- Modify: `crates/mpp-ipc-worker/src/staging.rs`
- Modify: `crates/mpp-ipc-worker/src/main.rs`
- Test: `crates/mpp-ipc-worker/src/staging.rs`'s test module; `crates/mpp-ipc-worker/tests/end_to_end.rs`

**Interfaces:**
- Consumes: `WriteSetup` (Task 10), `write_tiff` (Task 4), `Sample` (Task 3).
- Produces: `Staging<T: Sample>` with the same method set as today (`create`, `write_band`, `rows_received`, `first_uncovered`, `is_complete`, `remove`) plus `impl PlanarRows<T> for Staging<T>`; `fn run_write(...)` dispatching on `setup.band_format`.

- [ ] **Step 1: Write the failing test**

Add to `crates/mpp-ipc-worker/tests/end_to_end.rs`, following the existing harness's shape (read it first; it drives a real worker process through `mpp_ipc::testhost`):

```rust
#[test]
fn u32_bands_reach_the_file_bit_exact() {
    // Protocol v3's reason for existing: u32 samples must survive the trip
    // through shm unchanged, including values f32 cannot represent.
    let dir = tmpdir("e2e-u32");
    let out = dir.join("out.tif");
    let values: Vec<u32> = vec![0, 1, 16_777_217, 4_000_000_001, u32::MAX, 42];

    let mut host = TestHost::start(InitBuilder::write(&out, 6, 1, 1)
        .bit_depth("u32")
        .band_format("u32")
        .build());
    host.push_band_u32(0, 0, 1, &values);
    let done = host.finish().unwrap();
    assert!(done.bytes_written > 0);

    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    match dec.read_image().unwrap() {
        tiff::decoder::DecodingResult::U32(d) => assert_eq!(d, values),
        other => panic!("expected U32, got {other:?}"),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_tiled_compressed_write_session_produces_a_tiled_file() {
    let dir = tmpdir("e2e-tiled");
    let out = dir.join("out.tif");
    let mut host = TestHost::start(InitBuilder::write(&out, 300, 200, 1)
        .layout("tiles")
        .tile_size(128)
        .compression("lzw")
        .build());
    // ... push all 200 rows in bands, as the existing round-trip test does
    let done = host.finish().unwrap();
    assert!(done.bytes_written > 0);

    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&out).unwrap()).unwrap();
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::TileWidth).unwrap(), 128);
    assert_eq!(dec.get_tag_u64(tiff::tags::Tag::Compression).unwrap(), 5);
    std::fs::remove_dir_all(&dir).unwrap();
}
```

Adapt the builder/host API names to whatever `crates/mpp-ipc/src/testhost.rs` actually exposes; extend that helper with the new `Init` fields rather than constructing JSON by hand in the test.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-ipc-worker --test end_to_end`
Expected: FAIL — the worker stages f32 regardless of `band_format`.

- [ ] **Step 3: Make `Staging` generic**

In `staging.rs`, parameterise the struct on the sample type:

```rust
/// Mmap-backed planar staging for out-of-order band arrival.
///
/// The type parameter is the sample type the session negotiated
/// (`InitMessage::band_format`). The staging file is
/// `width * height * channels * size_of::<T>()` bytes, mapped read/write,
/// so out-of-order bands land at their final position and the encoder
/// drains the result through `PlanarRows<T>` without a second copy.
pub struct Staging<T> {
    // ... existing fields ...
    _sample: std::marker::PhantomData<T>,
}
```

`create` multiplies by `std::mem::size_of::<T>()`; `write_band` takes `&[T]`; the row-coverage bookkeeping is byte-count-agnostic and does not change. The `PlanarRows` impl becomes `impl<T: Sample> PlanarRows<T> for Staging<T>`, and its `row` slices the mapping with `bytemuck::cast_slice`, which `mpp-core` already depends on — check whether `mpp-ipc-worker` does and add it if not.

Keep the existing doc comment about zero-filled uncovered rows: it still holds, and `run_write` still guarantees full coverage before encoding.

- [ ] **Step 4: Dispatch on `band_format` in `run_write`**

Split the write path so the generic part is written once:

```rust
/// Run a write session with `T` as the band sample type. Called from
/// `run_write`'s dispatch on `InitMessage::band_format`.
fn run_write_typed<T: Sample + bytemuck::Pod>(
    link: &mut HostLink,
    init: &InitMessage,
    setup: &WriteSetup,
    staging_path: &Path,
    output_path: &Path,
) -> Result<(), WorkerError> {
    // The body is today's run_write with Staging<T> in place of Staging
    // and `write_tiff(&staging, output_path, &setup.options)` in place of
    // `write_bigtiff`.
}

fn run_write(/* existing parameters */) -> Result<(), WorkerError> {
    match setup.band_format {
        BitDepth::F32 => run_write_typed::<f32>(/* ... */),
        BitDepth::F64 => run_write_typed::<f64>(/* ... */),
        BitDepth::U32 => run_write_typed::<u32>(/* ... */),
        BitDepth::U16 => run_write_typed::<u16>(/* ... */),
        BitDepth::U8 => run_write_typed::<u8>(/* ... */),
    }
}
```

`Sample` needs a `bytemuck::Pod` bound (or the dispatch needs its own trait) for the cast in `Staging::row`. Adding `Pod` as a supertrait of `Sample` is fine: all five types are `Pod`, and `mpp-core` already depends on bytemuck.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p mpp-ipc-worker`
Expected: PASS, including the existing round-trip, split-band, u16, cancel and read tests.

- [ ] **Step 6: Run the gates and commit**

```bash
git add -A crates/
git commit -m "feat(worker): stage bands in their native sample type

Staging is generic over the session's band_format, so u32 and f64 pixels
reach the encoder unchanged instead of being flattened to unit-range f32
on the way in.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 12: Host library v3 and the protocol document

**Files:**
- Modify: `integration/pixinsight/host/mpp_protocol.h` (the `InitMessage` struct, `kProtocolVersion`)
- Modify: `integration/pixinsight/host/mpp_protocol.cpp` (`encode_init`)
- Modify: `integration/pixinsight/host/mpp_host.h/.cpp` (`WriteSessionConfig`, `PushBandBytes`)
- Create: `integration/pixinsight/host/test/test_v3_init.cpp`
- Modify: `integration/pixinsight/host/CMakeLists.txt` (register the test)
- Modify: `docs/pi-protocol.md`

**Interfaces:**
- Consumes: the v3 field list (Task 10).
- Produces: `mpp::WriteSessionConfig` gains `band_format`, `container`, `layout`, `tile_size`, `compression`, `deflate_level`, `predictor`, `alpha`, `icc_profile_base64`, `description`, `copyright`, `software`, `x_resolution`, `y_resolution`, `resolution_unit`; `void WriteSession::PushBandBytes(uint32_t channel, uint64_t start_row, uint32_t rows, const void* samples)`.

- [ ] **Step 1: Write the failing test**

Create `integration/pixinsight/host/test/test_v3_init.cpp`, following the shape of the existing tests in that directory (read `test_roundtrip.cpp` first for the harness idiom):

```cpp
// test_v3_init.cpp -- the v3 Init frame carries every write option, with
// the exact field names and JSON types docs/pi-protocol.md §5 specifies.

#include "mpp_protocol.h"

#include <cassert>
#include <string>

int main() {
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
  init.description = "M31";
  init.x_resolution = 300.0;
  init.y_resolution = 300.0;
  init.resolution_unit = "cm";

  const std::vector<uint8_t> frame = mpp::encode_init(init);
  // Skip the 5-byte frame header (tag + u32 length) to reach the payload.
  const std::string json(frame.begin() + 5, frame.end());

  // Version bump.
  assert(json.find("\"protocol_version\":3") != std::string::npos);
  // Wide integers stay integers, never doubles (docs/pi-protocol.md §5).
  assert(json.find("\"width\":40000") != std::string::npos);
  assert(json.find("\"slot_bytes\":480000") != std::string::npos);
  // Every v3 field is present with its value.
  assert(json.find("\"band_format\":\"u32\"") != std::string::npos);
  assert(json.find("\"container\":\"big\"") != std::string::npos);
  assert(json.find("\"layout\":\"tiles\"") != std::string::npos);
  assert(json.find("\"tile_size\":512") != std::string::npos);
  assert(json.find("\"compression\":\"deflate\"") != std::string::npos);
  assert(json.find("\"deflate_level\":9") != std::string::npos);
  assert(json.find("\"predictor\":true") != std::string::npos);
  assert(json.find("\"alpha\":\"unassociated\"") != std::string::npos);
  assert(json.find("\"description\":\"M31\"") != std::string::npos);
  assert(json.find("\"resolution_unit\":\"cm\"") != std::string::npos);
  return 0;
}
```

- [ ] **Step 2: Register and run it**

Add the test to `integration/pixinsight/host/CMakeLists.txt` alongside the existing ones, then:

```bash
cmake -S integration/pixinsight/host -B /tmp/mpp-host-build
cmake --build /tmp/mpp-host-build
ctest --test-dir /tmp/mpp-host-build --output-on-failure
```

Expected: FAIL to compile — `InitMessage` has no `band_format`.

- [ ] **Step 3: Extend the C++ `InitMessage` and encoder**

In `mpp_protocol.h`, bump `kProtocolVersion` to `3` and add the fields with defaults matching the Rust side's `serde(default)` values exactly:

```cpp
  std::string band_format = "f32";  ///< **(v3)** shm sample type: "f32"|"f64"|"u8"|"u16"|"u32"
  std::string container = "auto";   ///< **(v3)** "auto"|"classic"|"big"
  std::string layout = "strips";    ///< **(v3)** "strips"|"tiles"
  uint32_t tile_size = 256;         ///< **(v3)** tile edge; power of two 128..1024
  std::string compression = "none"; ///< **(v3)** "none"|"lzw"|"deflate"|"packbits"
  uint8_t deflate_level = 6;        ///< **(v3)** 1-9
  bool predictor = false;           ///< **(v3)** horizontal predictor; integer formats only
  std::string alpha = "none";       ///< **(v3)** "none"|"unassociated"|"associated"
  std::string icc_profile;          ///< **(v3)** base64 ICC blob, empty when absent
  std::string description;          ///< **(v3)** TIFF tag 270
  std::string copyright;            ///< **(v3)** TIFF tag 33432
  std::string software;             ///< **(v3)** TIFF tag 305
  double x_resolution = 0.0;        ///< **(v3)** pixels per unit; 0 = unknown
  double y_resolution = 0.0;        ///< **(v3)** pixels per unit; 0 = unknown
  std::string resolution_unit = "inch";  ///< **(v3)** "inch"|"cm"
```

In `encode_init`, assign each one into the JSON object, keeping the existing comment about never routing integers through `double`. `deflate_level` is a `uint8_t`, which nlohmann would encode as a number — good — but assign it through `static_cast<unsigned>` so it cannot be mistaken for a character.

- [ ] **Step 4: Add `PushBandBytes`**

In `mpp_host.h`, add the `WriteSessionConfig` fields listed under Interfaces above and the byte-oriented push, keeping the existing `PushBand` as a thin f32 wrapper so the existing golden tests and any caller that only ever sends f32 keep working:

```cpp
  /// Sample width in bytes for `cfg.band_format`; the unit `PushBandBytes`
  /// measures its buffer in.
  static size_t SampleBytes(const std::string& band_format);

  /// Like `PushBand`, but for a buffer whose samples are in the session's
  /// negotiated `band_format` rather than always f32. Copies
  /// `rows * width * SampleBytes(cfg.band_format)` bytes into the slot.
  void PushBandBytes(uint32_t channel, uint64_t start_row, uint32_t rows,
                     const void* samples);

  /// f32 convenience wrapper over `PushBandBytes`. Throws `HostError` if
  /// the session did not negotiate `band_format == "f32"`, since the
  /// buffer's element type would then disagree with the wire format.
  void PushBand(uint32_t channel, uint64_t start_row, uint32_t rows, const float* samples);
```

In `mpp_host.cpp`, move the existing `PushBand` body into `PushBandBytes`, replacing `rows * width * sizeof(float)` with `rows * width * SampleBytes(cfg_.band_format)`, and validate `cfg.band_format` in the constructor against the five known values.

Also validate `cfg.icc_profile_base64` in the constructor: reject anything whose decoded size would exceed 16 MiB, with a `HostError` naming the limit. The worker checks the same bound (Task 10), but failing here means the user sees the error before a process is spawned and a shm segment created.

- [ ] **Step 5: Run the host test suite**

```bash
cmake --build /tmp/mpp-host-build && ctest --test-dir /tmp/mpp-host-build --output-on-failure
```

Expected: PASS, including the existing `test_roundtrip`, `test_u16`, `test_split_bands`, `test_cancel`, `test_host_gone`, `test_read`, `test_bogus_ack`, `test_version_mismatch`, `test_worker_missing`.

- [ ] **Step 6: Update `docs/pi-protocol.md`**

Add a §10 documenting protocol v3, and update in place:

- §5's `InitMessage` listing and field table — every v3 field, its type, default, and meaning; keep the existing "Integer width" and "`output_path` encoding" notes.
- §5's worked example — show a v3 write session with tiling and compression.
- §6's `slot_bytes` rule — alignment is now to the band sample width, not a fixed 4 bytes.
- The version history note — v3 is additive over v2 exactly as v2 was over v1: new `Init` fields with defaults, no new frame tags, no layout change to `Band`.

State explicitly that `band_format` describes the shm payload while `bit_depth` describes the output file, since that distinction is the whole point of the version.

- [ ] **Step 7: Commit**

```bash
git add -A integration/pixinsight/host/ docs/pi-protocol.md
git commit -m "feat(host): protocol v3 Init encoding and native-width band pushes

PushBandBytes measures its buffer in the session's negotiated sample
width; PushBand stays as an f32 wrapper that refuses a session which
negotiated anything else. docs/pi-protocol.md gains section 10 and the
v3 field table.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

**Checkpoint — end of Group C.** `cargo test`, the host `ctest` suite, and the Rust gates must all be green. The engine and the wire are done; the module can now be taught to use them.

---

## Group D — read side

### Task 13: Accept palette, YCbCr and planar input

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_read.rs:140-205`
- Test: `crates/mpp-core/src/formats/tiff_read.rs`'s test module

**Interfaces:**
- Consumes: `write_tiff` and its internals (Group A) — test fixtures are written by our own encoder.
- Produces: `TiffInfo` unchanged in shape; the accepted-colour-type set widens.

- [ ] **Step 1: Write the failing tests**

The fixtures must be synthesized. Palette and planar files cannot be produced by `write_tiff`'s production options, so add a test-only fixture writer in the test module that drives `DirectoryEncoder` directly — the same primitives Task 4 uses.

```rust
    /// Write a minimal palette-colour TIFF: 8-bit indices plus a 256-entry
    /// ColorMap. Test-only; the production writer never emits palette.
    fn write_palette_fixture(path: &Path) {
        use tiff::encoder::{TiffEncoder, TiffKindStandard};
        use tiff::tags::Tag;

        let mut file = std::fs::File::create(path).unwrap();
        let mut enc: TiffEncoder<_, TiffKindStandard> =
            TiffEncoder::new_generic(&mut file).unwrap();
        let mut dir = enc.image_directory().unwrap();

        // 4x2 image, indices 0..8.
        let indices: Vec<u8> = (0u8..8).collect();
        let offset = dir.write_data(&indices[..]).unwrap();

        // ColorMap: 3 * 256 u16 entries, R then G then B. Entry i maps to
        // (i * 257, 0, 0), so index 3 decodes to a pure-red-ish pixel and a
        // mis-read palette cannot pass as correct.
        let mut map = vec![0u16; 3 * 256];
        for i in 0..256 {
            map[i] = (i * 257) as u16;
        }

        dir.write_tag(Tag::ImageWidth, 4u32).unwrap();
        dir.write_tag(Tag::ImageLength, 2u32).unwrap();
        dir.write_tag(Tag::BitsPerSample, &[8u16][..]).unwrap();
        dir.write_tag(Tag::SamplesPerPixel, 1u16).unwrap();
        dir.write_tag(Tag::PhotometricInterpretation, 3u16).unwrap(); // palette
        dir.write_tag(Tag::Compression, 1u16).unwrap();
        dir.write_tag(Tag::PlanarConfiguration, 1u16).unwrap();
        dir.write_tag(Tag::RowsPerStrip, 2u32).unwrap();
        dir.write_tag(Tag::StripOffsets, &[offset as u32][..]).unwrap();
        dir.write_tag(Tag::StripByteCounts, &[8u32][..]).unwrap();
        dir.write_tag(Tag::ColorMap, &map[..]).unwrap();
        dir.finish().unwrap();
    }

    #[test]
    fn palette_input_is_expanded_to_rgb() {
        let dir = tmpdir("palette");
        let src = dir.join("pal.tif");
        write_palette_fixture(&src);

        let reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        assert_eq!(reader.info().alpha_channels, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn planar_storage_is_accepted() {
        // Write a separate-planes fixture the same way, with
        // PlanarConfiguration = 2 and one strip per plane, then assert the
        // reader opens it and reports 3 colour channels.
        let dir = tmpdir("planar");
        let src = dir.join("planar.tif");
        write_planar_fixture(&src);
        let reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().color, ColorClass::Rgb);
        assert_eq!(reader.info().color_channels, 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cmyk_and_lab_are_still_rejected_with_a_reason() {
        // Existing `open_rejects_cmyk` proves the rejection; extend it to
        // require the message to explain *why*, not just name the type.
        let dir = tmpdir("cmyk-reason");
        let src = dir.join("cmyk.tif");
        write_cmyk_fixture(&src);
        let msg = TiffReader::open(&src).unwrap_err().to_string();
        assert!(
            msg.contains("colour management") || msg.contains("color management"),
            "rejection should explain why CMYK is refused, got: {msg}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

Write `write_planar_fixture` and `write_cmyk_fixture` in the same style as `write_palette_fixture` — full code, not a comment; the palette fixture above is the template.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p mpp-core tiff_read`
Expected: FAIL — palette and planar are rejected, and the CMYK message says only "unsupported TIFF colour type".

- [ ] **Step 3: Widen the accepted set**

In `tiff_read.rs`'s colour-type match (around line 143), add the palette and YCbCr arms. The decoder expands both to RGB itself, so they map to `ColorClass::Rgb` with 3 colour channels and no alpha:

```rust
            // The decoder expands a ColorMap-indexed image to RGB, so what
            // reaches us is ordinary RGB samples.
            TiffColorType::Palette(_) => (ColorClass::Rgb, 3, 0, 8),
            // Likewise YCbCr: the decoder applies the inverse transform.
            TiffColorType::YCbCr(b) => (ColorClass::Rgb, 3, 0, b),
```

Check the decoder's `ColorType` variant names and payloads in `tiff-0.11.3/src/decoder/mod.rs` and use exactly those; the shapes above are indicative.

Replace the planar rejection (around line 168-177) with de-interleaving support. The reader currently assumes chunky bands; for `PlanarConfiguration = 2` the decoder returns each plane separately, so gather the planes and interleave before handing bands on — or, if the decoder already normalises this, simply drop the rejection and prove it with the test.

Rewrite the CMYK/Lab rejection message:

```rust
            other => {
                return Err(Error::format(
                    path,
                    format!(
                        "unsupported TIFF colour type {other:?}: converting it to RGB \
                         faithfully needs ICC colour management, which this reader does \
                         not do, and an unmanaged conversion would silently misrepresent \
                         the pixel values"
                    ),
                ));
            }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p mpp-core tiff_read`
Expected: PASS, including the pre-existing rejection tests for signed integers and zero dimensions.

- [ ] **Step 5: Run the gates and commit**

```bash
git add -A crates/mpp-core/
git commit -m "feat(core): read palette, YCbCr and planar TIFFs

Three conversions that are exact or spec-defined, so they cost nothing in
fidelity. CMYK and Lab stay rejected, now with a message that says why:
faithful conversion needs colour management we do not have.

Fixtures are written by our own IFD writer in test-only code, so nothing
here depends on test_data/.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 14: ICC profile readback

**Files:**
- Modify: `crates/mpp-core/src/formats/tiff_read.rs` (`TiffInfo`, `TiffReader::open`)
- Modify: `crates/mpp-ipc/src/protocol.rs` (`ImageMeta`)
- Modify: `crates/mpp-ipc-worker/src/read.rs`
- Test: `crates/mpp-core/src/formats/tiff_read.rs`'s test module; `crates/mpp-ipc-worker/tests/read_e2e.rs`

**Interfaces:**
- Consumes: the writer's ICC support (Task 8).
- Produces: `TiffInfo` gains `pub icc_profile: Option<Vec<u8>>`; `ImageMeta` gains `#[serde(default)] pub icc_profile: String` (base64).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn an_embedded_icc_profile_is_read_back() {
        use crate::formats::tiff_write::{TiffWriteOptions, write_tiff};
        let dir = tmpdir("icc");
        let src = dir.join("icc.tif");
        let icc: Vec<u8> = (0u8..=255).cycle().take(3000).collect();
        // Round-trip through our own writer: what we embed, we must read.
        write_tiff(
            &synth_planar(4, 4, 1),
            &src,
            &TiffWriteOptions {
                icc_profile: Some(icc.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        let reader = TiffReader::open(&src).unwrap();
        assert_eq!(reader.info().icc_profile.as_deref(), Some(&icc[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_without_a_profile_reports_none() {
        use crate::formats::tiff_write::{TiffWriteOptions, write_tiff};
        let dir = tmpdir("noicc");
        let src = dir.join("plain.tif");
        write_tiff(&synth_planar(4, 4, 1), &src, &TiffWriteOptions::default()).unwrap();
        assert!(TiffReader::open(&src).unwrap().info().icc_profile.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

`synth_planar` is a small helper returning a `PlanarRows<f32>` value; write it in the test module if `tiff_read.rs`'s tests do not already have an equivalent.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p mpp-core tiff_read`
Expected: FAIL to compile — `TiffInfo` has no `icc_profile`.

- [ ] **Step 3: Read the tag**

In `TiffReader::open`, after the geometry is established:

```rust
        // Tag 34675. Absent on most astro TIFFs; `get_tag_u8_vec` errors
        // rather than returning None when the tag is missing, so treat any
        // failure as "no profile" rather than a file error.
        let icc_profile = dec.get_tag_u8_vec(TiffTag::IccProfile).ok().filter(|v| !v.is_empty());
```

and add the field to `TiffInfo` with a doc comment.

- [ ] **Step 4: Carry it to the module**

Add `icc_profile: String` (base64, empty when absent) to `ImageMeta` in `crates/mpp-ipc/src/protocol.rs` with `#[serde(default)]`, populate it in `crates/mpp-ipc-worker/src/read.rs` where the rest of the meta is built, and document it in `docs/pi-protocol.md` §9.2.

Add a read-path e2e assertion in `crates/mpp-ipc-worker/tests/read_e2e.rs`: write a file with a profile using `write_tiff`, open it through a read session, and assert the `ImageMeta`'s `icc_profile` decodes to the same bytes.

- [ ] **Step 5: Run the tests**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A crates/ docs/pi-protocol.md
git commit -m "feat(core): read embedded ICC profiles and carry them to the host

Round-trips against our own writer, and ImageMeta gains a base64 field so
the module can hand the profile back through ReadICCProfile.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

**Checkpoint — end of Group D.** Full `cargo test` plus the host `ctest` suite green.

---

## Group E — the PixInsight module

Nothing in this group is covered by an automated test that exercises the UI: PixInsight itself is out of the automated boundary by standing decision (`docs/DESIGN.md` phase 2). The machine gates here are compile, link and symbol checks; behaviour is proven by the manual smoke procedure in Task 18. Write the code so as much logic as possible sits below the UI, where the host library's `ctest` suite and the Rust tests can reach it.

### Task 15: `MppIcon.h` and the branded options dialog

**Files:**
- Create: `integration/pixinsight/module/MppIcon.h`
- Create: `integration/pixinsight/module/TiffOptionsDialog.h`
- Create: `integration/pixinsight/module/TiffOptionsDialog.cpp`
- Create: `integration/pixinsight/module/TiffFormatOptions.h`
- Modify: `integration/pixinsight/module/makefile-x64`, `CMakeLists.txt`, `integration/pixinsight/ci/win/*.vcxproj` (add the new translation unit)

**Interfaces:**
- Consumes: nothing from earlier tasks — this is UI only, wired up in Task 16.
- Produces:
  ```cpp
  struct TiffFormatOptions { /* POD; see Step 2 */ };
  class TiffOptionsDialog : public Dialog {
  public:
     TiffOptionsDialog( const ImageOptions& imageOptions,
                        const TiffFormatOptions& formatOptions,
                        bool imageHasAlpha );
     const ImageOptions& GetImageOptions() const;
     const TiffFormatOptions& GetFormatOptions() const;
  };
  ```

- [ ] **Step 1: Write the icon header**

Create `MppIcon.h`, mirroring MergeMosaic's `MmmIcon.h` exactly in structure and provenance comments (it is our own code, so this is reuse, not derivation):

```cpp
// MppIcon.h -- SVG artwork for the Mega Pixel Processor module.
//
// MPP_CHEVRON_SVG is the plain Astrometrical chevron mark used in the TIFF
// options dialog's header. It is what makes the dialog visibly ours rather
// than the stock TIFF module's -- see the product decision recorded in
// docs/superpowers/specs/2026-09-09-tiff-parity-design.md.
//
// The chevron path is the Astrometrical "A" mark (the Angora font glyph):
// pointed apex, THIN left stroke, THICK right stroke, notch rising to about
// 60% height. Do not "fix" the asymmetry.

#ifndef __MppIcon_h
#define __MppIcon_h

#define MPP_CHEVRON_SVG R"svg(<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" width="48" height="48">
  <path d="M 23.78 7.06 L 46 40.94 L 34.89 40.94 L 20.49 18.97 L 6.34 40.94 L 2 40.94 Z" fill="#2a95ab"/>
</svg>
)svg"

#endif   // __MppIcon_h
```

Copy the path data verbatim from `~/dev/mega-merge-mosaic/integration/pixinsight/module/MmmIcon.h`'s `MMM_CHEVRON_SVG` so the two products' marks stay identical.

- [ ] **Step 2: Write the format-specific data block**

Create `TiffFormatOptions.h`:

```cpp
// TiffFormatOptions.h -- the format-specific data block BigTiffFormat
// passes between QueryOptions(), SetFormatSpecificData() and CreateImage().
//
// PCL owns this block's lifetime across a module boundary
// (MetaFileFormat::ValidateFormatSpecificData/DisposeFormatSpecificData),
// so it is a POD with fixed-width fields and fixed-size character arrays:
// no pointers, no std::string, nothing whose layout could differ between
// the allocating and freeing side.
//
// Everything the core already models -- sample format, resolution, ICC
// embedding -- lives in pcl::ImageOptions instead; this block carries only
// what TIFF adds on top.

#ifndef __TiffFormatOptions_h
#define __TiffFormatOptions_h

#include <cstdint>

namespace pcl
{

/// Compression choices, matching docs/pi-protocol.md's `compression` values.
enum class TiffCompression : uint8_t { None = 0, Lzw = 1, Deflate = 2, Packbits = 3 };

/// Container choices, matching docs/pi-protocol.md's `container` values.
enum class TiffContainer : uint8_t { Auto = 0, Classic = 1, Big = 2 };

/// Block layout, matching docs/pi-protocol.md's `layout` values.
enum class TiffLayout : uint8_t { Strips = 0, Tiles = 1 };

/// Alpha association, matching docs/pi-protocol.md's `alpha` values.
enum class TiffAlpha : uint8_t { None = 0, Unassociated = 1, Associated = 2 };

/// Longest text a TIFF text tag may carry through this block, including the
/// terminating NUL. Longer input is truncated at the dialog, where the user
/// can see it happen.
constexpr size_t TiffTextTagCapacity = 256;

/*!
 * \struct TiffFormatOptions
 * \brief Per-save TIFF options that pcl::ImageOptions does not model.
 */
struct TiffFormatOptions
{
   /// Identifies a block as ours; checked by ValidateFormatSpecificData().
   uint32_t magic = Magic;
   /// Layout version of this struct; bumped whenever a field is added.
   uint32_t version = Version;

   TiffCompression compression = TiffCompression::None;
   TiffContainer container = TiffContainer::Auto;
   TiffLayout layout = TiffLayout::Strips;
   TiffAlpha alpha = TiffAlpha::Unassociated;

   /// Tile edge in pixels; a power of two in 128..=1024.
   uint32_t tileSize = 256;
   /// Deflate level 1-9. The encoder buckets this into fast/balanced/best.
   uint8_t deflateLevel = 6;
   /// Horizontal differencing predictor; integer sample formats only.
   bool predictor = false;

   /// TIFF tag 270, NUL-terminated ASCII; empty when unset.
   char description[TiffTextTagCapacity] = {};
   /// TIFF tag 33432, NUL-terminated ASCII; empty when unset.
   char copyright[TiffTextTagCapacity] = {};
   /// TIFF tag 305, NUL-terminated ASCII; empty when unset.
   char software[TiffTextTagCapacity] = {};

   /// Magic number identifying this block ("MPPT" in ASCII).
   static constexpr uint32_t Magic = 0x4D505054u;
   /// Current layout version.
   static constexpr uint32_t Version = 1u;
};

} // namespace pcl

#endif   // __TiffFormatOptions_h
```

- [ ] **Step 3: Write the dialog header**

Create `TiffOptionsDialog.h`, following `MmmInterface.h`'s idiom — a `GUIData`-style control block, `e_`-prefixed handlers, doc comments in the same voice:

```cpp
// TiffOptionsDialog.h -- the Astrometrical-branded TIFF options dialog the
// module presents from BigTiffInstance::QueryOptions().
//
// Deliberately styled unlike PixInsight's stock TIFF options dialog: this
// module REPLACES the stock TIFF module, and a user needs to be able to see
// at a glance which one is loaded. The header block (chevron mark, product
// title, tagline, links) is the same one Mega Merge Mosaic's process
// interface uses -- our own code, reused for family consistency.
//
// Clean-room note: written from the public PCL headers, the project's
// PCL_FILEFORMAT_API.md, and MergeMosaic's own module code. No PCL module
// implementation source was consulted; the stock dialog is referenced only
// as a list of the options it offers a user on screen.

#ifndef __TiffOptionsDialog_h
#define __TiffOptionsDialog_h

#include <pcl/Bitmap.h>
#include <pcl/CheckBox.h>
#include <pcl/ComboBox.h>
#include <pcl/Dialog.h>
#include <pcl/Edit.h>
#include <pcl/ImageOptions.h>
#include <pcl/Label.h>
#include <pcl/NumericControl.h>
#include <pcl/PushButton.h>
#include <pcl/RadioButton.h>
#include <pcl/SectionBar.h>
#include <pcl/Sizer.h>
#include <pcl/SpinBox.h>

#include "TiffFormatOptions.h"

namespace pcl
{

/*!
 * \class TiffOptionsDialog
 * \brief Modal per-save options dialog for the BigTIFF/TIFF format.
 *
 * Constructed with the options the caller wants shown as the initial state,
 * executed with Dialog::Execute(); on StdDialogCode::Ok the two getters
 * return the user's choices. On cancel they return the values passed in,
 * and the caller aborts the save.
 */
class TiffOptionsDialog : public Dialog
{
public:

   /*!
    * \param imageOptions   Initial sample format and resolution.
    * \param formatOptions  Initial TIFF-specific options.
    * \param imageHasAlpha  Whether the image being saved carries an alpha
    *                       channel; the alpha-association control is
    *                       disabled when it does not.
    */
   TiffOptionsDialog( const ImageOptions& imageOptions,
                      const TiffFormatOptions& formatOptions,
                      bool imageHasAlpha );

   /// The sample format and resolution the user chose.
   const ImageOptions& GetImageOptions() const
   {
      return m_imageOptions;
   }

   /// The TIFF-specific options the user chose.
   const TiffFormatOptions& GetFormatOptions() const
   {
      return m_formatOptions;
   }

private:

   ImageOptions      m_imageOptions;
   TiffFormatOptions m_formatOptions;
   bool              m_imageHasAlpha = false;

   VerticalSizer   Global_Sizer;

   // --- Header notice: chevron + title/tagline/copyright/links ------------
   Control         Notice_Control;
   HorizontalSizer Notice_Sizer;
   Control         Logo_Control;
   VerticalSizer   NoticeText_Sizer;
   Label           Title_Label;
   Label           Tagline_Label;
   Label           Copyright_Label;
   HorizontalSizer Links_Sizer;
   Label           ToolsLink_Label;
   Label           LinkSep_Label;
   Label           KofiLink_Label;
   Bitmap          Logo_Bitmap;

   // --- Sample Format -----------------------------------------------------
   SectionBar      SampleFormat_SectionBar;
   Control         SampleFormat_Control;
   VerticalSizer   SampleFormat_Sizer;
   RadioButton     UInt8_RadioButton;
   RadioButton     UInt16_RadioButton;
   RadioButton     UInt32_RadioButton;
   RadioButton     Float32_RadioButton;
   RadioButton     Float64_RadioButton;

   // --- Compression -------------------------------------------------------
   SectionBar      Compression_SectionBar;
   Control         Compression_Control;
   VerticalSizer   Compression_Sizer;
   HorizontalSizer CompressionMethod_Sizer;
   Label           CompressionMethod_Label;
   ComboBox        CompressionMethod_ComboBox;
   HorizontalSizer DeflateLevel_Sizer;
   Label           DeflateLevel_Label;
   SpinBox         DeflateLevel_SpinBox;
   HorizontalSizer Predictor_Sizer;
   CheckBox        Predictor_CheckBox;

   // --- Layout (our addition; not in the stock dialog) ---------------------
   SectionBar      Layout_SectionBar;
   Control         Layout_Control;
   VerticalSizer   Layout_Sizer;
   HorizontalSizer Container_Sizer;
   Label           Container_Label;
   ComboBox        Container_ComboBox;
   HorizontalSizer BlockLayout_Sizer;
   RadioButton     Strips_RadioButton;
   RadioButton     Tiles_RadioButton;
   HorizontalSizer TileSize_Sizer;
   Label           TileSize_Label;
   ComboBox        TileSize_ComboBox;

   // --- Embedded Data -----------------------------------------------------
   SectionBar      EmbeddedData_SectionBar;
   Control         EmbeddedData_Control;
   VerticalSizer   EmbeddedData_Sizer;
   CheckBox        EmbedICC_CheckBox;
   HorizontalSizer Alpha_Sizer;
   Label           Alpha_Label;
   ComboBox        Alpha_ComboBox;

   // --- Metadata ----------------------------------------------------------
   SectionBar      Metadata_SectionBar;
   Control         Metadata_Control;
   VerticalSizer   Metadata_Sizer;
   HorizontalSizer Description_Sizer;
   Label           Description_Label;
   Edit            Description_Edit;
   HorizontalSizer Copyright_Sizer;
   Label           CopyrightTag_Label;
   Edit            Copyright_Edit;
   HorizontalSizer Software_Sizer;
   Label           Software_Label;
   Edit            Software_Edit;
   HorizontalSizer Resolution_Sizer;
   Label           Resolution_Label;
   NumericEdit     XResolution_NumericEdit;
   NumericEdit     YResolution_NumericEdit;
   ComboBox        ResolutionUnit_ComboBox;

   // --- Buttons -----------------------------------------------------------
   HorizontalSizer Buttons_Sizer;
   PushButton      OK_PushButton;
   PushButton      Cancel_PushButton;

   void UpdateControls();
   void HarvestControls();

   void e_SampleFormatClick( Button& sender, bool checked );
   void e_CompressionItemSelected( ComboBox& sender, int itemIndex );
   void e_LayoutClick( Button& sender, bool checked );
   void e_ContainerItemSelected( ComboBox& sender, int itemIndex );
   void e_ButtonClick( Button& sender, bool checked );
   void e_ToggleSection( SectionBar& sender, Control& section, bool start );
   void e_LinkMouseRelease( Control& sender, const pcl::Point& pos,
                            int button, unsigned buttons, unsigned modifiers );
   void e_LogoPaint( Control& sender, const pcl::Rect& updateRect );
};

} // namespace pcl

#endif   // __TiffOptionsDialog_h
```

- [ ] **Step 4: Write the dialog implementation**

Create `TiffOptionsDialog.cpp`. Copy the header-block construction, `makeLink` lambda, `e_LogoPaint` and `e_LinkMouseRelease` bodies from `~/dev/mega-merge-mosaic/integration/pixinsight/module/MmmInterface.cpp` (lines 149-190 and 501-540), changing the title to `"Mega Pixel Processor " MPP_MODULE_VERSION_STRING " — TIFF Options"` and the tagline to something in the family voice (for example `"Astronomically large images."`).

Key behaviours to implement:

- `UpdateControls()` sets every control from `m_imageOptions`/`m_formatOptions`, and applies the two enable rules: the predictor check box disables when a float sample format is selected (TIFF tag 317 is integer-only, and `TiffWriteOptions::validate` rejects the combination); the alpha combo disables when `m_imageHasAlpha` is false. The tile-size combo disables when Strips is selected; the deflate-level spin box disables unless Deflate is the method.
- `HarvestControls()` reads the controls back into the two structs. Text edits are truncated to `TiffTextTagCapacity - 1` and stripped of non-ASCII and embedded NUL characters, because `impl TiffValue for str` in the Rust writer rejects both (Task 8).
- `e_ButtonClick` calls `HarvestControls()` then `Ok()` for OK, and `Cancel()` for Cancel.
- Every control gets a `SetToolTip( "<p>…</p>" )` explaining what it does, matching MMM's density. The tiled option's tooltip should say it is for large-image viewers that read sub-regions, and that most software reads striped TIFFs more widely.
- Container combo items: "Automatic (classic TIFF when it fits)", "Classic TIFF", "BigTIFF", with a tooltip explaining that classic TIFF is limited to 4 GB but is what every reader accepts.

- [ ] **Step 5: Add the translation unit to every build**

`integration/pixinsight/module/makefile-x64` and `CMakeLists.txt` both enumerate sources; add `TiffOptionsDialog.cpp` to each. Check `integration/pixinsight/ci/win/` for a source list too and update it if there is one.

- [ ] **Step 6: Build the module**

```bash
cargo build -p mpp-ipc-worker --release
make -C integration/pixinsight/module -f makefile-x64
```

Expected: a clean build, zero warnings, `mpp-pxm.so` produced. The dialog is not reachable yet — that is Task 16.

- [ ] **Step 7: Commit**

```bash
git add -A integration/pixinsight/module/
git commit -m "feat(pi-module): Astrometrical-branded TIFF options dialog

A PCL Dialog with the Mega Merge Mosaic header treatment -- chevron mark,
product title, tagline, links -- and five sections covering sample format,
compression, layout, embedded data and metadata. Deliberately unlike the
stock TIFF dialog: this module replaces that one, and the user needs to
see which is loaded. Not yet wired to QueryOptions.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 16: Wire the dialog through `QueryOptions` and format-specific data

**Files:**
- Modify: `integration/pixinsight/module/BigTiffFormat.h/.cpp`
- Modify: `integration/pixinsight/module/BigTiffInstance.h/.cpp`

**Interfaces:**
- Consumes: `TiffOptionsDialog`, `TiffFormatOptions` (Task 15).
- Produces: `BigTiffInstance::QueryOptions`, `SetFormatSpecificData`, `FormatSpecificData`; `BigTiffFormat::UsesFormatSpecificData`, `ValidateFormatSpecificData`, `DisposeFormatSpecificData`.

- [ ] **Step 1: Implement the format-level hooks**

In `BigTiffFormat.h/.cpp`:

```cpp
bool BigTiffFormat::UsesFormatSpecificData() const
{
   // True so the core round-trips our TiffFormatOptions block between
   // QueryOptions() and Create()/CreateImage()
   // (FileFormatImplementation.h:592-605).
   return true;
}

bool BigTiffFormat::ValidateFormatSpecificData( const void* data ) const
{
   // The core hands back blocks it received from us; anything else is a
   // foreign pointer we must not dereference as our own type.
   if ( data == nullptr )
      return false;
   const TiffFormatOptions* o = reinterpret_cast<const TiffFormatOptions*>( data );
   return o->magic == TiffFormatOptions::Magic
       && o->version == TiffFormatOptions::Version;
}

void BigTiffFormat::DisposeFormatSpecificData( void* data ) const
{
   delete reinterpret_cast<TiffFormatOptions*>( data );
}
```

- [ ] **Step 2: Implement `QueryOptions`**

In `BigTiffInstance.cpp`:

```cpp
bool BigTiffInstance::QueryOptions( Array<ImageOptions>& options, Array<void*>& formatOptions )
{
   // The core passes one ImageOptions per image; this format writes a single
   // image per file, so only the first is meaningful
   // (FileFormatImplementation.h:479-487).
   ImageOptions imageOptions = options.IsEmpty() ? ImageOptions() : options[0];

   // Start from whatever the core already gave us, or our defaults. Note
   // the default container is Auto, not Big: we own .tif/.tiff now, so a
   // save that fits in a classic TIFF should be one.
   TiffFormatOptions tiffOptions;
   if ( !formatOptions.IsEmpty() )
      if ( const void* existing = formatOptions[0] )
         if ( TheBigTiffFormat->ValidateFormatSpecificData( existing ) )
            tiffOptions = *reinterpret_cast<const TiffFormatOptions*>( existing );

   // The image geometry is not known until CreateImage(), so the dialog
   // cannot tell whether this particular image carries alpha. Enable the
   // control unconditionally and let CreateImage() ignore the association
   // when the image turns out to have no alpha channel.
   TiffOptionsDialog dialog( imageOptions, tiffOptions, /*imageHasAlpha=*/true );
   if ( dialog.Execute() != StdDialogCode::Ok )
      return false;   // aborts the save

   options.Clear();
   options.Add( dialog.GetImageOptions() );

   // The core takes ownership and frees this through
   // DisposeFormatSpecificData().
   formatOptions.Clear();
   formatOptions.Add( new TiffFormatOptions( dialog.GetFormatOptions() ) );
   return true;
}
```

Add the matching declarations to `BigTiffInstance.h`, and a `TheBigTiffFormat` global if the module does not already have one — check `Mpp.cpp` for how the format instance is held and reuse that.

- [ ] **Step 3: Implement `SetFormatSpecificData` / `FormatSpecificData`**

```cpp
void BigTiffInstance::SetFormatSpecificData( const void* data )
{
   // Advisory, like SetOptions(): stored here, applied in CreateImage().
   // Unknowns §3 -- the core need not call this at all, so
   // have_format_options_ gates its use rather than assuming a value.
   if ( data != nullptr && TheBigTiffFormat->ValidateFormatSpecificData( data ) )
   {
      format_options_ = *reinterpret_cast<const TiffFormatOptions*>( data );
      have_format_options_ = true;
   }
}

void* BigTiffInstance::FormatSpecificData() const
{
   if ( !have_format_options_ )
      return nullptr;
   return new TiffFormatOptions( format_options_ );
}
```

Add `TiffFormatOptions format_options_;` and `bool have_format_options_ = false;` to the private section of `BigTiffInstance.h`, documented in the style of the existing `have_options_`/`options_` pair.

- [ ] **Step 4: Build**

```bash
make -C integration/pixinsight/module -f makefile-x64
```

Expected: clean build, zero warnings.

- [ ] **Step 5: Commit**

```bash
git add -A integration/pixinsight/module/
git commit -m "feat(pi-module): present the options dialog from QueryOptions

UsesFormatSpecificData is now true and the TiffFormatOptions block
round-trips between QueryOptions and CreateImage, validated by magic and
version on every crossing. Cancelling the dialog aborts the save.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 17: Native sample formats, ICC, and honest capabilities

The task that makes `CanStore32Bit`/`CanStoreDouble` true rather than advertised.

**Files:**
- Modify: `integration/pixinsight/module/BigTiffInstance.h/.cpp`
- Modify: `integration/pixinsight/module/BigTiffFormat.h/.cpp`

**Interfaces:**
- Consumes: `WriteSessionConfig`'s v3 fields and `PushBandBytes` (Task 12); `TiffFormatOptions` (Task 15).
- Produces: no new public signatures; `PushNormalized` becomes `PushTyped`.

- [ ] **Step 1: Replace the narrowing sample-format map**

In `CreateImage()`, the mapping table becomes an identity: `bitsPerSample` and `ieeefpSampleFormat` choose the output format, and `band_format` matches it so no conversion happens on the wire.

```cpp
   // Sample-format mapping. Protocol v3 carries bands in their native type,
   // so this is now an identity map rather than the narrowing table phase 2
   // shipped: what the user asks for is what the file gets.
   if ( options_.complexSample )
      throw Error( "BigTIFF: complex sample formats are not supported." );
   if ( !options_.ieeefpSampleFormat && options_.signedIntegers )
      throw Error( "BigTIFF: signed integer sample formats are not supported." );
   std::string bitDepth;
   if ( options_.ieeefpSampleFormat )
      switch ( options_.bitsPerSample )
      {
      case 32: bitDepth = "f32"; break;
      case 64: bitDepth = "f64"; break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit floating-point samples are not "
                                       "supported.", int( options_.bitsPerSample ) ) );
      }
   else
      switch ( options_.bitsPerSample )
      {
      case 8:  bitDepth = "u8"; break;
      case 16: bitDepth = "u16"; break;
      case 32: bitDepth = "u32"; break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit integer samples are not supported.",
                                       int( options_.bitsPerSample ) ) );
      }
   band_format_ = bitDepth;   // v3: bands cross shm in the output's own type
```

Relax the channel check to accept alpha:

```cpp
   const int colourChannels = ( info.colorSpace == ColorSpace::Gray ) ? 1 : 3;
   if ( info.numberOfChannels != colourChannels && info.numberOfChannels != colourChannels + 1 )
      throw Error( String().Format(
         "BigTIFF: %d channels are not supported for this color space "
         "(expected %d, or %d with an alpha channel).",
         info.numberOfChannels, colourChannels, colourChannels + 1 ) );
   const bool hasAlpha = info.numberOfChannels == colourChannels + 1;
```

and populate every v3 field of `WriteSessionConfig` from `format_options_` (or its defaults when `have_format_options_` is false), setting `alpha` to `"none"` when `hasAlpha` is false regardless of what the dialog said — the image is the authority on whether an alpha channel exists.

Recompute `band_rows_` from the band sample width rather than `sizeof( float )`, so the ~4 MiB band budget holds for f64 too.

- [ ] **Step 2: Convert each `WriteSamples` buffer to the negotiated type**

Rename `PushNormalized` to `PushTyped` and give it the destination type as a runtime discriminator. Each overload converts from its own `T` to `band_format_`'s type:

```cpp
// Converts one WriteSamples buffer from PixInsight's sample type T to the
// session's negotiated band type and pushes it as <= band_rows_-row bands.
//
// The conversion is an identity whenever the two agree, which is the normal
// case: CreateImage() negotiates band_format from the same ImageOptions the
// core used to pick the WriteSamples overload. When they disagree (Unknowns
// §5: the overload that fires need not match SetOptions), samples go through
// the unit range, and lossy_ records any narrowing.
template <typename T>
void BigTiffInstance::PushTyped( const T* buffer, int startRow, int rowCount, int channel )
```

Keep the existing guard clauses, the `<= band_rows_` chunking loop and the `mpp::HostError` translation exactly as they are; only the scratch buffer's element type and the push call change:

```cpp
         session_->PushBandBytes( uint32_t( channel ), row, rows, samples );
```

Set `lossy_` where a conversion actually loses information — a `DImage` written as `u8`, say — and *not* for `u32` or `f64` output any more, since those are now exact. Update `WasLossyWrite()`'s comment accordingly.

- [ ] **Step 3: Implement `WriteICCProfile`**

```cpp
void BigTiffInstance::WriteICCProfile( const ICCProfile& icc )
{
   // PCL calls this before CreateImage() on the write path. Store the bytes;
   // CreateImage() base64s them into the v3 Init. Arriving after the session
   // exists would mean the profile could not reach the file, so say so
   // rather than dropping it silently.
   if ( session_ != nullptr )
      throw Error( "BigTIFF: internal error: an ICC profile arrived after the image was "
                   "created; it cannot be embedded." );
   if ( icc.IsProfile() )
      icc_profile_.assign( reinterpret_cast<const uint8_t*>( icc.ProfileData().Begin() ),
                           reinterpret_cast<const uint8_t*>( icc.ProfileData().End() ) );
}
```

Check `ICCProfile`'s accessor names in `$HOME/.local/pcl-build-0182/include/pcl/ICCProfile.h` and use the real ones. Add a base64 encoder — a 30-line function in the module, or reuse one if `mpp_host` already has one; it does not, so write it in `BigTiffInstance.cpp` with a doc comment and keep it file-local.

Also implement `ReadICCProfile()` on the read path, returning a profile built from the `ImageMeta`'s new base64 field (Task 14).

- [ ] **Step 4: Correct the capability declarations**

In `BigTiffFormat.cpp`, update the four capabilities and their comments:

```cpp
bool BigTiffFormat::CanStore32Bit() const
{
   // True, and now genuinely so: protocol v3 carries u32 bands natively and
   // the writer emits 32-bit unsigned samples. Before phase 4 this was
   // advertised while the value was narrowed to f32 -- exactly the
   // truth-in-advertising violation this phase existed to fix.
   return true;
}

bool BigTiffFormat::CanStoreDouble() const
{
   return true;   // f64 bands and 64-bit float output, both exact
}

bool BigTiffFormat::CanStoreAlphaChannels() const
{
   // Gray+alpha and RGBA write an ExtraSamples tag with the association the
   // options dialog selected.
   return true;
}

bool BigTiffFormat::CanStoreResolution() const
{
   return true;   // XResolution/YResolution/ResolutionUnit from ImageOptions
}

bool BigTiffFormat::CanStoreICCProfiles() const
{
   return true;   // tag 34675, written on save and read back on open
}
```

`CanStoreKeywords()` stays absent (default `false`): TIFF has no FITS-keyword equivalent, and the three text tags travel as format-specific data, not keywords. Add a comment in the header's capability table saying so, so a future reader does not "fix" it.

- [ ] **Step 5: Build and run the symbol gates**

```bash
cargo build -p mpp-ipc-worker --release
make -C integration/pixinsight/module -f makefile-x64
nm -D --undefined-only integration/pixinsight/module/mpp-pxm.so | grep ' mpp::' && echo "FAIL: undefined mpp:: symbols" || echo "OK: no undefined mpp:: symbols"
nm -D --defined-only integration/pixinsight/module/mpp-pxm.so | grep -E 'IdentifyPixInsightModule|InstallPixInsightModule|GetPixInsightModule'
```

Expected: a clean, warning-free build; no undefined `mpp::` symbols; the three PCL entry points exported.

- [ ] **Step 6: Commit**

```bash
git add -A integration/pixinsight/module/
git commit -m "feat(pi-module): native u32/f64 output, alpha, ICC, and honest capabilities

CreateImage now maps ImageOptions to the output sample format identically
instead of narrowing u32 and f64 to f32, and bands cross shm in that type.
CanStore32Bit and CanStoreDouble finally mean what they said; alpha,
resolution and ICC capabilities flip on as the code behind them lands.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 18: Documentation and the manual smoke procedure

**Files:**
- Modify: `docs/DESIGN.md` (phase 4)
- Modify: `docs/tiff-parity.md`
- Modify: `integration/pixinsight/README.md`
- Modify: `README.md` (if it lists module capabilities)

**Interfaces:**
- Consumes: everything above.
- Produces: no code.

- [ ] **Step 1: Update `docs/DESIGN.md` phase 4**

Rewrite the phase-4 entry from "implementation pending" to complete, recording the decisions the way phases 2 and 3 record theirs. Cover, one bullet each:

- **Hand-rolled IFD writer, no fork.** Why: every primitive the crate's `ImageEncoder` withholds — `DirectoryEncoder`, the compressors, `TiffKind` — is public one layer down, so tiles, classic TIFF, five sample formats and alpha came from one generic code path instead of a vendored encoder.
- **Protocol v3, native write bands.** Why: f32-normalised bands made `CanStore32Bit`/`CanStoreDouble` unfixable in the encoder alone.
- **`Container::Auto` by default.** Why: owning `.tif`/`.tiff` meant every save was a BigTIFF wearing a classic extension.
- **Read policy: palette/YCbCr/planar in, CMYK/Lab out.** Why: the first three convert exactly or by spec; the last two need colour management, and an unmanaged conversion would misrepresent pixel values.
- **No global preferences.** `CanEditPreferences()` stays false; one per-save dialog, defaults in code.
- **`CanStoreKeywords` stays false.** TIFF has no FITS-keyword equivalent; the text tags are format-specific data.

Add a new phase 5 entry for whatever remains deferred (WCS sidecar readback, incremental read, thumbnails, properties) so nothing silently drops off the roadmap, and renumber the existing "DeepSky viewer components" entry accordingly.

- [ ] **Step 2: Update `docs/tiff-parity.md`**

Add a status block at the top marking the gap closed, pointing at the design spec and this plan, and rewrite the two tables so each row says what shipped rather than what blocks it. Keep the "Why we're here" investigation and the falsified-hypotheses record intact — that is the document's lasting value. Move anything still open into a short "Remaining gaps" section (CMYK/Lab read, JPEG-in-TIFF write, global preferences).

- [ ] **Step 3: Update `integration/pixinsight/README.md`**

- The lead paragraph's "It is **not yet** a full replacement…" list shrinks to what is genuinely still missing.
- "Replacing the stock TIFF module" → "What you give up while swapped": rewrite. After this work the honest list is short — no CMYK/Lab input, no FITS-keyword or thumbnail passthrough, no JPEG-compressed output.
- "Write: sample-format mapping": replace the narrowing table with the identity mapping and note that protocol v3 carries native bands.
- "Read support": add palette, YCbCr, planar and ICC readback.
- "Out of scope": remove every item this plan implemented; keep the rest.
- Add an options-dialog subsection listing the five sections and what each controls.

- [ ] **Step 4: Extend the manual smoke procedure**

Add to the README's "Manual smoke procedure" a numbered TIFF-options section:

1. Save any image as TIFF; confirm the Astrometrical-branded dialog appears (chevron, product title, links) and is visibly not the stock dialog.
2. Cancel it; confirm no file is written.
3. Save 8-bit, 16-bit, 32-bit integer, 32-bit float and 64-bit float; reopen each in PixInsight and confirm the sample format survives — in particular that a 32-bit integer save reopens as a `UInt32Image`, which is the capability that was previously advertised but not delivered.
4. Save with LZW, Deflate and PackBits; confirm each reopens and that the compressed files are smaller.
5. Save a small image with the container on Automatic; confirm the file starts `II*\0` (classic) rather than `II+\0`, e.g. with `xxd -l 4`.
6. Save a tiled file; confirm it reopens in PixInsight and that `tiffinfo` (or `python3 -c` with the `tiff` crate's CLI equivalent) reports tile tags.
7. Save an image with an alpha channel; confirm the channel survives a round trip.
8. Fill in Image Description, Copyright and Software; reopen the file with `exiftool` or `tiffinfo` and confirm the tags.
9. Photoshop round trip: open a tiled, Deflate-compressed BigTIFF written by this module and confirm it renders correctly.

Note which of these previously failed, so a future reader can tell the procedure is a regression test rather than a checklist of the obvious.

- [ ] **Step 5: Commit**

```bash
git add -A docs/ integration/pixinsight/README.md README.md
git commit -m "docs(tiff-parity): record phase 4 as complete

DESIGN.md gains the phase-4 decisions, tiff-parity.md is marked closed
with its investigation record intact, and the module README's parity and
'what you give up' sections shrink to what is genuinely still missing.
The manual smoke procedure gains nine TIFF-options steps.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

**Checkpoint — end of Group E, and of the plan.** Before calling this done:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
cargo test
cmake --build /tmp/mpp-host-build && ctest --test-dir /tmp/mpp-host-build --output-on-failure
make -C integration/pixinsight/module -f makefile-x64
```

All six must pass, and the manual smoke procedure must have been run in PixInsight at least once, before the branch is finished. Do not claim parity on the strength of the automated gates alone: the dialog is the deliverable and no automated test touches it.
