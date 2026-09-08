# BigTIFF Reader — PixInsight Import (Phase 3, v1) Design

**Status:** design, approved shape (2026-09-08). Successor to the write-only
module (phase 2, plan B). Turns `BigTIFF-pxm` into a read+write PixInsight
file format so PixInsight can *open* BigTIFF/TIFF files, and establishes the
`.big.tiff` compound extension as a bidirectional Photoshop bridge.

## Goal

A **general TIFF/BigTIFF importer** in the PixInsight module: opening a
`.big.tiff` / `.big.tif` / `.btf` / `.tf8` file in PixInsight decodes it —
classic TIFF or BigTIFF, stripped or tiled, common compressions — and hands
PixInsight a full image in its **native sample format**. Pixels + geometry
only in v1; metadata deferred (see Non-goals).

## Scope

**In scope (v1):**
- Read: gray + RGB (and their +alpha forms), sample formats u8 / u16 / u32 /
  f32 / f64, stripped and tiled layouts, uncompressed + LZW + Deflate +
  PackBits + Fax4 + JPEG (whatever the `tiff` 0.11.3 decoder supports),
  classic TIFF and BigTIFF.
- Native-fidelity import: a u16 file becomes a `UInt16Image`, f32 a
  `pcl::Image`, etc. — no forced normalisation on the read path (unlike the
  write path, which normalises to f32).
- Whole-image read via `ReadImage` (see "Why whole-image").
- New extensions `.big.tiff` + `.big.tif` on the format; `CanRead` flipped
  on; the write-path `.big`→`.big.tiff` save-rename fix.
- Worker "read mode": open + decode + stream bands back to the module.

**Non-goals (deferred to later phases):**
- WCS sidecar readback (translate `<stem>.wcs.json` → WCS FITS keywords via
  `ReadFITSKeywords`). Its own phase, end-to-end. See
  `crates/mpp-core/src/wcs/sidecar.rs` and the astrometrical WCS reference.
- Incremental read (`ReadSamples`) — the core doesn't use it for interactive
  opens (see findings); `CanReadIncrementally()` stays `false`.
- General TIFF-tag → FITS-keyword mapping, ICC profile, thumbnail, image
  properties.
- Palette / CMYK / YCbCr / arbitrary Multiband inputs — rejected in v1 with
  an actionable `pcl::Error`.
- Claiming `.tif` / `.tiff` (the core rejects duplicate-extension
  registration while the stock TIFF module is installed — see findings). The
  full stock-TIFF-replacement story (user uninstalls stock TIFF, we claim
  `.tif`/`.tiff`) is a separate later phase.
- Classic (non-Big) TIFF *writing*.

## Findings that shaped this design (empirical, PixInsight 1.9.3)

Probes (throwaway module `.so` builds, reverted; never committed):

1. **Extension ownership is unique at registration.** Two installed formats
   claiming the same extension → the second's `InstallPixInsightModule`
   fails: "An installed file format already supports file extension: .tiff".
   So we cannot coexist with stock TIFF on `.tif`/`.tiff`. `.big.tiff` (and
   `.big.tif`) collide with nothing and register cleanly. MIME overlap
   (`image/tiff`) is allowed.
2. **`.big.tiff` is a working bidirectional Photoshop bridge.** Write: our
   worker emits real BigTIFF (`II 0x2B`), and Photoshop opens the
   `.big.tiff`. Read: the core routes `File > Open` of a `.big.tiff` to our
   `Open()`. `.btf` also routes to us.
3. **The core uses `ReadImage` (whole-image), not `ReadSamples`, for an
   interactive open** — even with `CanReadIncrementally()` returning true.
   This *simplifies* the reader: we own the decode loop, so there is no
   external access order to satisfy and **no chunk cache** — each strip/tile
   is decoded exactly once and scattered into the destination image.
4. **`tiff` 0.11.3 has a full chunk API** (`read_chunk`, `chunk_data_
   dimensions`, `strip_count`/`tile_count`, `dimensions`, `colortype`,
   `next_image`) and decodes None/LZW/PackBits/Deflate/JPEG/Fax4 — enough
   for a general importer, and enough to decode chunk-by-chunk without ever
   holding a whole multi-GB image in the worker.
5. **Save-dialog quirk:** choosing "BigTIFF" appends `.big` (not
   `.big.tiff`); a `.big` file is unusable (Photoshop won't take it, PI's
   chooser won't show it). Fixed on the write path (see Module changes).

## Why whole-image (not streaming into the core)

PixInsight's in-memory image *is* the whole image (backed by PI's own
swap-file system for large ones); the core allocates it and calls
`ReadImage(Img&)` for us to fill. Streaming never reduces the core's peak
memory — it only avoids the **worker** holding a second full copy. That
benefit is preserved: the worker decodes chunk-by-chunk and streams ~4 MiB
bands into the destination image, so worker peak ≈ one strip/tile + one
band, not the whole image.

## Architecture

Mirrors the write path inverted. Three components, same layering as phase 2:

- **`mpp-core`** gains a streaming BigTIFF/TIFF **reader** API (today
  `formats/tiff.rs` is a writer; it only decodes inside `#[cfg(test)]`).
  The reader exposes geometry (dimensions, colortype, sample format,
  alpha) and a chunk-ordered iterator that yields decoded, **de-interleaved
  per-channel row bands**.
- **`mpp-ipc-worker`** gains a **read mode**: open the file, report
  geometry, then decode chunk-by-chunk and stream per-channel `Band` frames
  to the module over shm, native sample bytes.
- **module (`BigTiffInstance`)** implements the read lifecycle: `Open()`
  gets geometry (native format) and builds the `ImageDescription`;
  `ReadImage(Img&)` allocates the destination, runs a read session, and
  scatters each incoming band into the destination image's channel planes.

### Two-phase worker interaction

1. **Geometry probe — at `Open(path)`.** The module runs the worker in a
   lightweight probe that reads only the TIFF header and prints geometry
   JSON (`{width, height, channels, alpha, color_space, bit_depth}`), then
   exits. The module builds `ImageDescription{ info{width,height,
   numberOfChannels,colorSpace,supported=true}, options{bitsPerSample,
   ieeefpSampleFormat,signedIntegers} }` from it. PixInsight then dispatches
   to the `ReadImage` overload matching that declared format → native
   fidelity. (Header read is cheap; opening the file twice is fine.)
2. **Decode + stream — at `ReadImage(Img& image)`.** The module
   `image.AllocateData(width,height,numberOfChannels,colorSpace)`, creates
   the shm segment, spawns the worker in read mode (naming the input file +
   shm), and consumes `Band` frames: for each `Band{channel,start_row,rows,
   slot}` it copies `rows × width` native samples from the slot into
   `image`'s `channel` plane at `start_row`, then acks the slot. On `Done`
   the read is complete; any `Error` becomes a `pcl::Error` carrying the
   worker's message. The worker holds one chunk; the module adds one band.

Each of the five `ReadImage` overloads (`pcl::Image`/`DImage`/`UInt8Image`/
`UInt16Image`/`UInt32Image`) drives the same session; the sample type is
fixed by the geometry declared in `Open()`, so only the matching overload is
called in practice, but all five are implemented (the non-matching ones
error clearly — the headers don't *promise* which fires; defensive per the
existing "Unknowns" discipline).

### Protocol extension (`docs/pi-protocol.md`)

Read is an **additive, direction-symmetric** extension of the v1 write
protocol. The slot state machine is reused inverted: in write the host fills
a slot → sends `Band` → worker acks; in read the **worker fills** a slot →
sends `Band` → **host acks**.

- Bump `IPC_PROTOCOL_VERSION` to `2`. The worker handles both write (phase-2
  clients) and read; module and worker are versioned together and guarded by
  the existing `version_sync` test, so a v2 module never talks to a v1
  worker.
- **`Init`** gains `mode: "write" | "read"` (serde default `"write"` keeps
  the write JSON wire-compatible). For read, `Init` names the **input**
  path; `bit_depth` is echoed by the worker from the file (not chosen by the
  host) — the authoritative geometry comes from the probe / the `ImageMeta`
  frame below.
- New frames (exact tag numbers finalised in the plan, preserving the
  disjoint-direction convention — worker tags low, host tags ≥128):
  - `ImageMeta` (worker→host, JSON): `{width,height,channels,alpha,
    color_space,bit_depth}` — the worker's authoritative geometry, sent once
    after it opens the file, before any band. (Redundant with the probe but
    lets the read session self-describe and cross-check the probe.)
  - `Band` (worker→host): identical 20-byte binary layout as the write
    `Band` (`channel:u32, start_row:u64, rows:u32, slot:u32`), opposite
    direction. Native sample bytes in the slot (u8/u16/u32/f32/f64), not
    forced f32 (the write path forces f32; read preserves native).
  - `BandAck` (host→worker): 4-byte `slot:u32`, opposite direction to
    write's `BandAck`.
  - `Done` / `Error` / `Progress` reused unchanged (worker→host).
- `slot_bytes` sized by the host for the largest band it will receive
  (band_rows × width × native_sample_size), same `>=` rule as write.

### Sample-format & colour mapping (read)

`tiff::ColorType` → PixInsight (`ImageInfo.colorSpace` / channel count /
image class), by the inner bit depth:

| tiff ColorType | bits / fmt | PixInsight image | colorSpace | channels |
|---|---|---|---|---|
| `Gray(8)` | 8 int | `UInt8Image` | Gray | 1 |
| `Gray(16)` | 16 int | `UInt16Image` | Gray | 1 |
| `Gray(32)` int | 32 int | `UInt32Image` | Gray | 1 |
| `Gray(32)` float | 32 fp | `pcl::Image` | Gray | 1 |
| `Gray(64)` float | 64 fp | `DImage` | Gray | 1 |
| `RGB(n)` | as above | matching class | RGB | 3 |
| `GrayA(n)` | as above | matching class | Gray | 2 (1 alpha) |
| `RGBA(n)` | as above | matching class | RGB | 4 (1 alpha) |
| `Palette` / `CMYK` / `YCbCr` / `Multiband` | — | — | reject with `pcl::Error` | — |

- Sample format from the TIFF `SampleFormat` tag: unsigned int → u8/u16/u32;
  IEEE float → f32/f64. Signed integer and complex inputs are rejected in v1
  (`pcl::Error`, actionable).
- Alpha channels are imported as PixInsight "extra" channels (numberOfChannels
  includes them); colorSpace is set by the colour channels (Gray vs RGB).
- Row order: TIFF is top-down; PixInsight rows are top-down — no flip. (The
  WCS sidecar's row convention is documented top-down/stored-order; relevant
  only when the sidecar phase lands.)

## Module changes (`integration/pixinsight/module`)

- `BigTiffFormat`: `CanRead()` → `true`; `CanReadIncrementally()` stays
  `false` (add the override returning false, explicit); `FileExtensions()`
  → `{ ".big.tiff", ".big.tif", ".btf", ".tf8" }`. Update `Description()`
  and the README/provenance to say read+write.
- `BigTiffInstance`: implement `Open()` (geometry probe → `ImageDescription`),
  `IsOpen()`/`FilePath()`/`Close()` for the read state, and the five
  `ReadImage` overloads (allocate + run read session + de-interleave-place).
  Reuse `mpp_host` for the shm/spawn/frame plumbing, extended for the read
  direction (a `ReadSession` mirroring `WriteSession`).
- Worker resolution unchanged (`dladdr`/`GetModuleFileNameW`, worker beside
  the module).
- **`.big` save-rename fix (write path):** in `Create()` (the module knows
  the path before writing), if `output_path` ends in `.big` or otherwise
  lacks a TIFF tail expected by downstream tools, rewrite to `<stem>.big.tiff`
  and surface a `pcl::Console`/message noting the rename. Keeps the Save As
  dialog quirk from producing unusable `.big` files.

## Host lib changes (`integration/pixinsight/host`)

- `mpp_protocol`: add the read-mode frames (`ImageMeta`, read `Band`/`BandAck`
  direction), `kProtocolVersion` → 2, `mode` in Init construction.
- `mpp_host`: add a `ReadSession` (RAII, mirrors `WriteSession`): ctor
  creates shm + spawns worker (read mode) + reads `ImageMeta`; a pump that
  yields bands (or invokes a callback) until `Done`; dtor kills+reaps.
- All host code stays PCL-free (the `mpp_host.h` no-PCL rule).

## Worker changes (`crates/mpp-ipc-worker`)

- Read mode: parse `Init.mode == "read"`; open the input via the new
  `mpp-core` reader; send `ImageMeta`; loop chunks, de-interleave into
  per-channel row bands, fill slots, send `Band`, await `BandAck`; send
  `Done`. Errors → `Error` frame + nonzero exit.
- Geometry-probe entry (a `--probe <path>` mode or an `Init`-less
  header-only path — finalised in the plan): print geometry JSON, exit 0.

## `mpp-core` changes

- `formats/tiff.rs` (or a new `formats/tiff_read.rs`): a reader that opens a
  path, exposes geometry + colortype + sample format + alpha, and yields
  decoded per-channel row bands in chunk order (strip or tile), decoding
  each chunk once. Unsupported colortypes → typed error. Public API
  documented (`missing_docs`).

## Testing

- **`mpp-core`:** unit tests decoding synthesized TIFFs (stripped + tiled;
  uncompressed + LZW + Deflate + PackBits; gray + RGB + alpha; u8/u16/u32/
  f32/f64), including write→read round-trips against the existing writer.
  No dependence on `test_data/` (synthesize inputs).
- **worker:** read-mode golden tests via the testkit host — spawn the
  worker on a synthesized file, collect bands, reassemble, compare to a
  direct `tiff`-crate decode.
- **host C++ CTest:** a read golden test (spawn the real worker in read
  mode, drive `ReadSession`, verify reassembled pixels + geometry) with no
  PixInsight involved — mirrors the write golden suite.
- **version-sync:** unchanged mechanism; the three version legs must still
  agree (now protocol v2).
- **native-format dispatch:** an early implementation test/probe confirming
  the core calls `ReadImage(UInt16Image&)` for a declared-u16 file (assumed,
  verify cheaply before building all five overloads out).
- **manual PI smoke (human):** open the `~/test.big.tiff` written by the
  write path and confirm pixels match the original; documented in the
  README.

## Rollout / extensions & registration

`.big.tiff`/`.big.tif` added alongside `.btf`/`.tf8`. `.tif`/`.tiff` NOT
claimed (registration conflict; deferred to the stock-TIFF-replacement
phase). CI (`module.yml`) already builds all four platforms; the new read
golden test joins the host CTest suite on each.

## Open questions (resolve during planning, not blocking)

- Exact new tag numbers + whether geometry rides a `--probe` invocation or
  an in-session `ImageMeta` only (leaning: both — probe for `Open()`,
  `ImageMeta` for the read session's self-description).
- Whether to keep de-interleaving in the worker (chosen: yes — lets the host
  reuse the per-channel `Band` layout unchanged and keeps the module simple).
- Tiled-input band granularity (a tile row-band spans a grid row of tiles;
  the worker emits row bands after de-tiling — confirm memory bound).
