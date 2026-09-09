# TIFF Parity — Options Dialog, Tiled Write, Honest Sample Formats (Phase 4) Design

**Status:** design, approved shape (2026-09-09). Successor to the stock-TIFF
replacement decision (`docs/DESIGN.md` phase 4, `docs/tiff-parity.md`). Closes
the parity gap that opened when the module took ownership of `.tif`/`.tiff`
and displaced PixInsight's bundled TIFF module.

## Goal

Reach and exceed parity with the stock TIFF module's write options, behind an
Astrometrical-branded options dialog that is visibly not the stock one. In
scope because the module is now the *only* TIFF support in PixInsight: every
gap below is a regression for anyone who swaps, not a deferred feature.

Concretely, after this work a PixInsight user saving a TIFF can choose sample
format (8/16/32-bit unsigned integer, 32/64-bit float — all written for
real), compression (None / LZW / Deflate / PackBits, with a horizontal
predictor for integer data), container (classic TIFF or BigTIFF, defaulting
to whichever fits), layout (strips or tiles — our addition, for the
Astrometrical large-image load path), alpha association, an embedded ICC
profile, resolution, and the three TIFF text tags.

## Scope

**In scope:**

- A new IFD-level TIFF/BigTIFF writer in `mpp-core`, replacing the current
  use of the `tiff` crate's `ImageEncoder`: strips **and tiles**, classic
  **and** BigTIFF, five sample formats, 1–4 channels, four compressors, the
  horizontal predictor, and the metadata tags listed above.
- Protocol v3: write-direction bands carry their **native** sample type, so
  `u32` and `f64` output stop being a lie; plus the option fields the dialog
  produces.
- `TiffOptionsDialog`, a PCL `Dialog` subclass, wired through
  `QueryOptions()` / `UsesFormatSpecificData()` / `SetFormatSpecificData()`.
- Capability corrections on `BigTiffFormat` (truth-in-advertising rule).
- Read-side widening to palette, YCbCr and planar/separate storage; ICC
  profile readback.
- `mpp convert` CLI flags for every new write option.

**Non-goals (explicitly deferred):**

- **Global format preferences.** `CanEditPreferences()` stays `false`; there
  is one per-save dialog and defaults live in code. Sticky defaults are a
  later change if users ask.
- **CMYK and Lab input.** The decoder handles them; faithful conversion needs
  colour management this project does not have, and a naive formula would
  quietly misrepresent pixel values. They keep an explicit rejection, with
  error text that says why rather than just "unsupported".
- **Complex and void sample formats**, signed integers — unchanged
  rejections.
- **FITS keywords, thumbnails, image properties.** `CanStoreKeywords()` stays
  `false`: TIFF has no FITS-keyword equivalent, and the three text tags are
  format-specific data, not keywords.
- **WCS sidecar readback** — still its own future phase.
- **Incremental read (`ReadSamples`)** — unchanged; the core doesn't use it.
- **Non-blocking abort wiring** — the known phase-2 limitation stands.
- **JPEG-in-TIFF and Fax compression on write.** Read support comes free from
  the decoder; writing them is not parity with the stock dialog and lossy
  compression is wrong for linear astro data.

## Findings that shaped this design

1. **Tiled write is tractable without a fork.** `tiff` 0.11.3's
   `ImageEncoder` has no tile support, but the layer beneath it is entirely
   public: `TiffEncoder::image_directory()` yields a `DirectoryEncoder` with
   `write_data()` (writes bytes, returns the file offset) and `write_tag()`;
   `tiff::encoder::compression` publicly exports `Compressor`,
   `CompressionAlgorithm`, `Lzw`, `Deflate`, `DeflateLevel`, `Packbits` and
   `Uncompressed`; and `TiffKind` / `TiffKindStandard` / `TiffKindBig` are
   public, so classic-vs-Big is a type parameter. Every primitive a tiled
   encoder needs is therefore already reachable — no vendoring, no fork, no
   waiting on upstream. This falsifies the analysis document's framing of
   tiled write as the item that drives the schedule; it is still the largest
   single piece of work, but it is ordinary work.
2. **True `u32`/`f64` output cannot be fixed in the writer alone.** Every
   write band crosses the shm channel as f32 in `[0,1]`
   (`BigTiffInstance.cpp`'s `PushNormalized`), and f32's 24-bit mantissa
   destroys u32 precision before `mpp-core` ever sees a sample. Honest
   `CanStore32Bit`/`CanStoreDouble` therefore requires a protocol change, not
   an encoder change. The read path already carries native sample formats, so
   this closes a standing asymmetry rather than inventing a mechanism.
3. **Every save we currently produce is BigTIFF, and we now own `.tif`.** The
   stock module wrote classic TIFF. A user saving a 20 MB image gets a
   BigTIFF named `.tif`, which a good deal of older software will not open.
   This is not in `docs/tiff-parity.md`'s table and is arguably a sharper
   regression than the missing tile option.
4. **`ImageOptions` already carries part of the dialog.** `bitsPerSample`,
   `ieeefpSampleFormat`, `signedIntegers`, `xResolution`, `yResolution`,
   `metricResolution` and `embedICCProfile` are core-owned fields
   (`ImageOptions.h`), so the sample format and resolution belong there and
   only the rest needs a format-specific data block.
5. **The reader already handles alpha.** `tiff_read.rs` maps `GrayA` and
   `RGBA` to `alpha_channels`, so alpha is a write-side gap only, and adding
   it makes the two directions symmetric.

## Architecture

```
mpp convert --compression --tiled --container ...   PixInsight Save As
        |                                                  |
        |                                        QueryOptions() -> TiffOptionsDialog
        |                                                  |  ImageOptions + TiffFormatOptions blob
        v                                                  v
mpp-core::formats::tiff_write                    BigTiffInstance (module, C++20)
  TiffWriteOptions  -- the full option set                 |  native-typed bands
  write_tiff<K, T>  -- one loop, strips or tiles           v
        ^                                          mpp::WriteSession (host lib, C++17)
        |  PlanarRows<T>                                   |  protocol v3 Init + Band
        |                                                  v
   Staging<T> (worker, mmap)  <---------------------  mpp-ipc-worker
```

The writer is the only new engine component. Everything else is plumbing that
carries a wider option set through layers that already exist.

### The writer (`crates/mpp-core/src/formats/tiff_write.rs`)

One encode loop serves both layouts:

1. Gather the next chunk of source pixels into an interleaved sample buffer —
   a full-width band of `rows_per_strip` rows for strips, a
   `tile_width × tile_length` block (zero-padded at the right and bottom
   edges, as the TIFF spec requires) for tiles.
2. Apply the horizontal predictor when configured.
3. Compress through a `Compressor`, chosen once from the options.
4. `DirectoryEncoder::write_data()` the compressed bytes; record the returned
   offset and the byte count.
5. After the last chunk, write the tag set and `finish()`.

Strip and tile data are written back-to-back with no inter-block padding,
matching the `tiff` crate's own strip writer: TIFF's word-alignment rule
applies to IFD entries and their out-of-line values, which `write_tag()` and
`DirectoryEncoder`'s own `pad_word_boundary()` already handle, not to pixel
data blocks.

Sample bytes are emitted in **native** byte order (`to_ne_bytes`), because
the crate writes the file's byte-order mark from `target_endian` — `II` on a
little-endian host, `MM` on a big-endian one — so native-order samples are by
construction the file's order.

Each compressed block is independent, as TIFF requires: the crate's `Lzw` and
`Deflate` compressors construct a fresh encoder per `write_to()` call, so a
block never depends on its predecessor's dictionary state.

Strips and tiles differ only in step 1 and in which offset/byte-count tags
step 5 emits. Memory stays bounded by one chunk, as it is today.

Generic parameters: `K: TiffKind` (classic or BigTIFF — same code, different
offset widths and tag types) and `T: Sample` (the source sample type).

**Tag set written.** `ImageWidth`, `ImageLength`, `BitsPerSample`,
`SampleFormat`, `SamplesPerPixel`, `PhotometricInterpretation`,
`Compression`, `PlanarConfiguration` (always chunky on write),
`Predictor` when not `None`, `ExtraSamples` when alpha is present, and either
`RowsPerStrip`/`StripOffsets`/`StripByteCounts` or
`TileWidth`/`TileLength`/`TileOffsets`/`TileByteCounts`. Optionally
`XResolution`/`YResolution`/`ResolutionUnit`, `ICCProfile` (34675),
`ImageDescription` (270), `Copyright` (33432), `Software` (305).

**Container selection.** `Container::{Auto, Classic, Big}`. `Auto` resolves
before the first byte is written: `width × height × channels ×
bytes_per_sample`, plus a fixed IFD allowance, under 4 GiB minus a safety
margin selects classic, otherwise BigTIFF. Compression only ever shrinks the
payload, so `Auto` never under-predicts. `Classic` with an over-large image
fails at option-validation time, before any pixels move, with an error naming
the size and suggesting BigTIFF.

**Sample handling.** A `Sample` trait supplies each type's TIFF
`BitsPerSample` and `SampleFormat` tag values plus unit-range conversions to
and from `f32`. `PlanarRows` becomes `PlanarRows<T>`; `XisfImage` implements
`PlanarRows<f32>`. When the source type equals the output type the write is a
straight copy with no conversion; otherwise samples convert through the unit
range, which is the existing documented contract. The PixInsight path
arranges for the two to match, so its pixels are copied verbatim.

**Colour and channels.** 1 channel → `BlackIsZero`; 2 → `BlackIsZero` +
one extra sample; 3 → `RGB`; 4 → `RGB` + one extra sample. `ExtraSamples`
carries `AssociatedAlpha` or `UnassociatedAlpha` per the option.

**Predictor constraint.** The horizontal predictor is defined for integer
data only; the `tiff` crate refuses it for IEEE floating-point samples and we
mirror that rule in both the option validation and the dialog (the checkbox
disables for float formats).

**Tile size.** Powers of two only, 128–1024, default 256, validated at option
level. Tile dimensions must be multiples of 16 per the TIFF spec; powers of
two from 128 up satisfy that.

### Protocol v3 (`docs/pi-protocol.md`)

`IPC_PROTOCOL_VERSION` 2 → 3. `InitMessage` gains the fields below, each
`#[serde(default)]` so a v2 payload still deserialises to the current
behaviour:

| field | type | meaning |
|---|---|---|
| `band_format` | string | sample type of bands over shm: `"f32"` (default) \| `"f64"` \| `"u8"` \| `"u16"` \| `"u32"` |
| `channels` | u64 | widened from `1\|3` to `1..=4` |
| `container` | string | `"auto"` (default) \| `"classic"` \| `"big"` |
| `layout` | string | `"strips"` (default) \| `"tiles"` |
| `tile_size` | u32 | tile edge in pixels; power of two, 128–1024 |
| `compression` | string | `"none"` (default) \| `"lzw"` \| `"deflate"` \| `"packbits"` |
| `deflate_level` | u8 | 1–9, default 6; ignored unless `compression == "deflate"` |
| `predictor` | bool | horizontal predictor; rejected for float `bit_depth` |
| `alpha` | string | `"none"` (default) \| `"unassociated"` \| `"associated"` |
| `icc_profile` | string | base64 ICC blob, empty when absent; capped at 16 MiB decoded |
| `description` / `copyright` / `software` | string | TIFF tags 270 / 33432 / 305, empty when absent |
| `x_resolution` / `y_resolution` | f64 | pixels per unit, 0 = unknown |
| `resolution_unit` | string | `"inch"` (default) \| `"cm"` |

`band_format` is the load-bearing addition. It describes the sample type of
the pixel data in shm slots, independently of `bit_depth` (the *output* file
format). The `Band` frame layout is unchanged — only the element width
differs, which `slot_bytes` already parameterises, and the host's existing
`slot_bytes >= max band bytes` validation still holds.

The ICC blob rides in the Init JSON rather than a new frame type: it is one
small payload sent once before any pixels, and a new frame tag would buy
nothing. The 16 MiB cap is validated host-side and worker-side.

**Validation rules the worker enforces**, each with an actionable message:
`band_format` and `bit_depth` both known; `channels` in `1..=4`; `alpha` and
`channels` mutually consistent in both directions (an alpha association
requires 2 or 4 channels, and 2 or 4 channels require an alpha association —
a 2-channel image with `alpha == "none"` is a multiband image, which the
writer does not produce); `tile_size` a power of two in range when
`layout == "tiles"`; `predictor` false for float output; `container ==
"classic"` compatible with the computed payload size.

### `Staging` (worker)

`Staging` becomes generic over the sample type, mmapping
`width × height × channels × size_of::<T>()` bytes and implementing
`PlanarRows<T>`. The worker selects the concrete instantiation from
`band_format` at Init time. Band writes and the row-coverage bookkeeping are
otherwise unchanged.

### Module changes (`integration/pixinsight/module`)

**`TiffOptionsDialog`** (new `TiffOptionsDialog.h`/`.cpp`), following
MergeMosaic's `MmmInterface` idiom: a `GUIData` struct holding every control,
built once; `SectionBar`-grouped sections; fixed-width right-aligned labels;
`<p>`-wrapped HTML tooltips; `e_`-prefixed handlers attached with the direct
`OnXxx` idiom.

The branding header is the MMM notice block: a `Control` painting the
Astrometrical chevron from an SVG bitmap, a bold title
("Mega Pixel Processor — TIFF Options"), a tagline, a copyright line, and the
three link labels (astrometrical.com · Astrometrical Tools · Ko-fi) with
pointing-hand cursors. This is what makes the dialog unmistakably ours rather
than the stock module's, which is the stated product requirement. A new
`MppIcon.h` carries the SVG, mirroring MMM's `MmmIcon.h`.

Sections:

- **Sample Format** — radio buttons: 8-bit unsigned, 16-bit unsigned, 32-bit
  unsigned, 32-bit float, 64-bit float.
- **Compression** — combo (None / LZW / Deflate / PackBits), Deflate level
  spin box, horizontal-predictor check box (disabled for float formats).
- **Layout** — container combo (Automatic / Classic TIFF / BigTIFF),
  strips-vs-tiles radio pair, tile-size combo. Our addition; not in the stock
  dialog.
- **Embedded Data** — embed-ICC-profile check box, alpha-association combo
  (enabled only when the image carries alpha).
- **Metadata** — Image Description, Copyright and Software edits; horizontal
  and vertical resolution numeric controls with an inch/cm unit combo.

Standard OK / Cancel buttons; Cancel returns `false` from `QueryOptions()`,
which aborts the save.

**Wiring.** `QueryOptions()` builds and executes the dialog, writes the
sample format (`bitsPerSample`, `ieeefpSampleFormat`, `signedIntegers`) and
resolution fields into the first `ImageOptions`, and pushes a heap-allocated
`TiffFormatOptions` block into `formatOptions`.
`MetaFileFormat::UsesFormatSpecificData()` returns `true`;
`ValidateFormatSpecificData()` checks a magic number and a version word;
`DisposeFormatSpecificData()` frees the block.
`FileFormatImplementation::SetFormatSpecificData()` stores a copy on the
instance for `CreateImage()` to fold into the v3 `Init`, and
`FormatSpecificData()` hands the current block back. When
`SetFormatSpecificData()` never runs — the "Unknowns" discipline says assume
it might not — `CreateImage()` uses in-code defaults, which reproduce today's
behaviour except for the container, which defaults to `Auto`.

`TiffFormatOptions` is a POD struct with a magic, a version, fixed-width
fields and fixed-size character arrays for the three text tags (no pointers,
no `std::string`) — the block crosses a module boundary and PCL owns its
lifetime.

**`WriteICCProfile()`** stores the profile bytes for `CreateImage()` to
base64 into the Init. PCL calls it before `CreateImage()` on the write path;
if it arrives late the instance throws rather than silently dropping it.

**Sample-format mapping** in `CreateImage()` replaces today's narrowing table
with an identity mapping: `bitsPerSample`/`ieeefpSampleFormat` select both
`bit_depth` and `band_format`, and each `WriteSamples` overload converts its
buffer to that native type. `WasLossyWrite()` then reports true only for a
genuine narrowing (for example a `DImage` saved as 16-bit by user choice),
not for the whole `u32`/`f64` category as it does today.

**Capability corrections** on `BigTiffFormat`:

| query | now | after | why |
|---|---|---|---|
| `CanStore32Bit` | true (narrowed) | true (real) | native u32 bands and output |
| `CanStoreDouble` | true (narrowed) | true (real) | native f64 bands and output |
| `CanStoreAlphaChannels` | false | true | 2- and 4-channel write |
| `CanStoreResolution` | false | true | XResolution/YResolution written |
| `CanStoreICCProfiles` | false | true | tag 34675 written and read |
| `CanStoreKeywords` | false | false | TIFF has no FITS-keyword equivalent |
| `UsesFormatSpecificData` | false | true | the options block |
| `CanEditPreferences` | false | false | out of scope by decision |

### Read-side changes (`crates/mpp-core/src/formats/tiff_read.rs`)

Accepted where the conversion is mechanical and exact or spec-defined:

- **Palette** — expanded to RGB through the `ColorMap` tag. The decoder
  performs the expansion; the reader maps the result to `ColorClass::Rgb`.
- **YCbCr** — the decoder already inverts the transform; the reader maps to
  `ColorClass::Rgb`.
- **Planar / separate storage** — de-interleaved into the reader's existing
  chunky band assumption.

Still rejected, with error text that states the reason rather than only the
colour type: **CMYK** and **Lab** (need colour management to convert
faithfully), **signed integers**, **complex** and **void** samples,
**arbitrary multiband**.

**ICC readback** — the profile bytes are read from tag 34675, carried in the
read-direction `ImageMeta`, and returned by
`BigTiffInstance::ReadICCProfile()`.

### CLI changes (`crates/mpp`)

`mpp convert` gains `--compression`, `--deflate-level`, `--predictor`,
`--tiled`, `--tile-size`, `--container`, and a widened `--bit-depth` that
accepts `u32` and `f64` alongside today's three. Clap mirror enums stay in
the `mpp` crate, keeping clap out of `mpp-core`, exactly as `BitDepthArg`
does today.

## Testing

Machine-verifiable, all inputs synthesized (never `test_data/`):

- **Writer unit tests** — for each layout × container × compression ×
  sample format combination that matters: write, decode the result with the
  `tiff` crate's decoder, assert pixel equality; and separately assert on the
  raw tag values, so a structurally plausible but semantically wrong IFD
  cannot pass. Specific cases: partial final strip; partial edge tiles (right
  column and bottom row, including the corner); classic-vs-BigTIFF magic
  bytes; `Auto` selecting each container side of the threshold; u32 and f64
  round-tripping exactly; `ExtraSamples` present and correct for 2- and
  4-channel images; predictor rejected for float; every compressed block
  decoding independently.
- **Reader unit tests** — palette, YCbCr and planar fixtures. These are
  written by the new IFD writer itself (extended in test-only code where it
  must emit a layout the writer does not offer in production), which is what
  keeps the fixtures synthesized rather than checked in.
- **Worker e2e tests** — one per `band_format`, asserting the output file's
  sample format and exact pixel values; plus the new validation rejections.
- **Host-lib golden `ctest`** — the v3 `Init` JSON serialises with the new
  fields, and a v2 payload still parses.
- **CLI integration tests** — each new flag reaches the writer and changes
  the output as expected.
- **Gates** — `cargo fmt`, `cargo clippy --all-targets`, `cargo doc` all
  warning-free (`missing_docs` means every new public item in `mpp-core`
  carries a doc comment); the module's compile/link/symbol gates unchanged.

Manual smoke (documented, not automated, per the standing verification
boundary): the dialog itself in PixInsight — each control reaching the file,
Cancel aborting the save — and a Photoshop round-trip of a tiled,
Deflate-compressed BigTIFF.

## Risks

- **Hand-rolled IFD correctness.** The failure mode is a file that our own
  decoder reads back happily while other readers reject it. Mitigated by
  asserting on raw tags as well as decoded pixels, and by the Photoshop
  round-trip in manual smoke. Sequencing the writer first means a problem
  here surfaces before any protocol or UI work is committed.
- **Scale of a single plan.** Six areas across three languages ending in a
  manual-verification tail. Mitigated by hard checkpoint boundaries between
  task groups, each independently verifiable.
- **`PlanarRows` becoming generic** ripples into `XisfImage`, `Staging`,
  `convert.rs` and the DZI path. Mitigated by keeping `PlanarRows<f32>` the
  only instantiation outside the worker.

## Documentation updates (same change)

- `docs/pi-protocol.md` — protocol v3: the new `InitMessage` fields, the
  `band_format` semantics, the validation rules, and a worked example.
- `docs/DESIGN.md` — phase 4 marked complete with its decisions recorded.
- `integration/pixinsight/README.md` — the parity table, the shrunken "what
  you give up while swapped" list, and the dialog in the manual smoke
  procedure.
- `docs/tiff-parity.md` — marked resolved, pointing at this spec and the
  implementation plan.

## Clean-room provenance

Everything here is designed from the public TIFF 6.0 and BigTIFF
specifications, the `tiff` crate's public API and source, the public PCL
headers, this project's `PCL_FILEFORMAT_API.md`, and MergeMosaic's own module
code (our own, for the UI idiom). The stock TIFF module's dialog is described
only as a user sees it on screen — a list of the options it offers. No
PixInsight module implementation source was read or derived from, per
`docs/DESIGN.md`'s licensing constraints.
