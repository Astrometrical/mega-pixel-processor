# Mega Pixel Processor — Design

Status: scaffold (2026-08-20). This document records the decisions made when the
project was founded; keep it current as design evolves.

## Purpose

Big astro images are trapped in astro formats. PixInsight cannot export beyond
classic TIFF's 4 GB limit (no BigTIFF), so large mosaics can't reach Photoshop
and friends; and there is no good self-hosted path from a finished image to an
interactive zoomable web view. `mpp` is a standalone CLI (later also the engine
behind PixInsight file-format modules) that:

1. Converts between astro and mainstream formats at any size — XISF, FITS,
   TIFF/BigTIFF, plus everything libvips provides (PNG, JPEG, WebP, AVIF/HEIF,
   OpenEXR, ...).
2. Carries astrometric solutions everywhere: natively where the container
   supports it (FITS, XISF), via a canonical JSON WCS sidecar where it doesn't
   (TIFF, PNG, DZI, ...).
3. Publishes images as DZI (Deep Zoom) tile pyramids with an
   Astrometrical-branded, OpenSeadragon-based HTML viewer — static files that
   work on any web host.

## Positioning

Tools genuinely useful on their own, leaning toward what Astrometrical
provides. The counter-positioning is against walled gardens (AstroBin):
openness, no vendor lock-in, self-hostable outputs. The DZI + HTML output must
always remain dumb static files that work anywhere — that self-hostability is
the anti-lock-in proof point. A hosted Astrometrical platform then competes on
being better, not on being the only place the files work.

## Branding

- **Mega** is the family constant across Astrometrical tools (Mega Merge
  Mosaic → `mmm`, Mega Pixel Processor → `mpp`). Alliterative pair after the
  prefix; three-letter binary.
- **DeepSky** is reserved for the viewer components (the Deep Zoom ↔ deep-sky
  pun), e.g. an npm package for the OSD wrapper.
- **Astronomic** is reserved for a possible hosted DZI platform ("Astronomic,
  by Astrometrical").
- **PixInsight module identity is the product, not a format** (decided
  2026-09-09): the module binary is `mpp-pxm.{so,dylib,dll}` and registers
  as `MegaPixelProcessor` ("Mega Pixel Processor: file format support for
  astronomically large images"). The *formats* it provides register beneath
  it individually and are named for their container, per PixInsight's own
  convention (`TIFF`, `XISF`, `FITS`) — today just `TIFF`, described as
  "Mega Pixel Processor: TIFF/BigTIFF (read+write)". The format was named
  `BigTIFF` until phase 4 renamed it: this module displaces the stock TIFF
  module, so users look for it under `TIFF`, and since `Container::Auto`
  writes a classic TIFF below 4 GB, `BigTIFF` had come to misname the common
  case. The C++ classes keep their `BigTiffFormat`/`BigTiffInstance` names —
  the identifier is user-facing, the class names are not. BigTIFF was the
  first
  format, never the module's identity; adding a second format is additive
  (a new `<Format>Format`/`<Format>Instance` pair constructed in
  `InstallPixInsightModule`) and changes no binary name, CI job, or install
  procedure. See `integration/pixinsight/README.md`, "Module and format
  naming".
- Rejected names, for the record: Peta Pixel * (petapixel.com), Giga Pixel *
  (Topaz Gigapixel), Prism (Syqon Prism Deep), Parser/Press (weak/ambiguous).

## Architecture decisions

- **Rust workspace**, mirroring MMM: `mpp-core` (UI-agnostic engine),
  `mpp` (thin CLI). A PixInsight-facing worker crate comes with phase 2.
- **libvips** is the image-processing backbone (LGPL-2.1+, dynamically
  linked): demand-driven/streaming, constant memory on multi-GB images,
  `tiffsave` with `bigtiff` option, `dzsave` for DZI pyramids.
  **Decided (2026-08-20):** hybrid. The XISF→BigTIFF leg is pure Rust
  (`tiff` crate, `TiffEncoder::new_big`, strip-streamed; since 2026-09-11
  blocks are compressed on worker threads and written in order through a
  bounded window, so memory is a few blocks per core, not the image);
  vips is used by *spawning the official `vips` binaries* for `dzsave` and
  mainstream-format conversion — no FFI bindings, no LGPL linking
  obligations. Revisit bindings only if process spawning proves limiting.
  Mainstream-format delegation (2026-08-21): all sample-format math stays
  in Rust — the converter writes a temp BigTIFF at the target bit depth
  next to the output, then `vips copy` re-containers it. This is because
  vips savers silently auto-cast unsupported band formats (float [0,1]
  input becomes a clipped 8-bit image with no error, verified on 8.15.1)
  and `pngsave` needs an explicit `bitdepth=16`. Formats: png (u16/u8),
  jpg/webp/avif (u8, `Q=` quality), jxl (u16/u8). vips has no EXR saver,
  so EXR export is out; `fitssave` exists in the Ubuntu build (cfitsio) —
  relevant to the FITS read/write evaluation still pending. JXL defaults
  to lossless (`lossless=true`) unless `--quality` is given; WebP tops out
  at 16383 px per side and JPEG at 65500 (preflighted before conversion).
- **XISF is our code** — vips has no XISF.
  **Decided (2026-08-20):** the XISF reader and linear-WCS extraction are
  *ported* from `mmm-core` into `mpp-core` — `mpp-core` owns format code,
  and the long-term dependency direction is the reverse of the original
  idea: `mmm-core` will eventually depend on `mpp-core` for file-format
  support. FITS: vips has cfitsio support, but our own reader/writer
  may serve WCS fidelity better — evaluate during phase 1.
  **Decided (2026-10-28), formats home:** mpp-core is the single home of
  XISF and FITS handling for every Astrometrical tool. mmm's FITS pixel
  reader (`formats::fits::FitsImage`) and streaming writer
  (`formats::fits_write::FitsWriter`, standard real/commentary cards) and
  its signature detection (`formats::sniff_format`) moved here; XISF
  revision 2 compliance (compression, checksums, more sample formats)
  lands here next, and mmm consumes mpp-core with
  `default-features = false` (the `tiff`, `convert` and `publish` features
  gate everything mmm does not need). Roadmap: mmm
  `docs/superpowers/plans/2026-10-28-formats-to-mpp-xisf-rev2.md`. This
  settles the FITS read/write evaluation: our own code, not cfitsio.
- **WCS sidecar**: one canonical JSON schema used by every container that
  can't carry the solution natively, and consumed by the DZI viewer. Design
  the schema once, early in phase 1.
- **Universal viewer mode**: `mpp` can emit the branded HTML viewer alongside
  *any* image, not just DZI — OpenSeadragon displays plain single images
  directly (`ImageTileSource`), no pyramid needed. Doesn't scale; above a
  size threshold the CLI suggests DZI mode.
- **Linear data discipline** where applicable, as in MMM; DZI tiles are 8-bit
  display formats, so the DZI path operates on stretched, display-ready
  input by design.

## Headers and full astrometric solutions (2026-10-08)

Added for Photometrical, which reads image metadata and plate solutions
through `mpp-core`:

- **`header::Header`**: the ordered FITS keywords and XISF properties of a
  file, reusing `FitsKeyword` and `XisfProperty`. Readers:
  `formats::read_header` (by file signature), `formats::xisf::read_header`
  (header and property attachments only, any sample format) and
  `formats::fits::read_header` (primary-HDU cards; the pixel reader is
  `formats::fits::FitsImage`). An open `XisfImage` converts with `Header::from`.
  Existing reader signatures are unchanged.
- **`Header::image_size`**: `Option<(u32, u32)>`, the XISF `<Image geometry>`
  (authoritative, whatever the `NAXIS` keywords say) or FITS
  `NAXIS1`/`NAXIS2`. It decodes as `None` when absent from an older encoding.
- **Grid caches dropped**: `xisf::read_header` skips PixInsight's private
  `PCL:AstrometricSolution:Grid:*` properties (about 20 MB). No decoder uses
  them, and a `Header` is meant to travel between nodes. `XisfImage::open`
  keeps them.
- **XISF properties**: every numeric vector/matrix type (`I32Vector`,
  `F32Matrix`, …) now decodes to f64, as in mmm; only complex and other
  non-numeric types stay `Unread`.
- **`astrometry::AstrometricSolution`**: the full solution, a union of
  `PixInsight(PiSolution)` and a reserved, still-uninhabited
  `Wcs(WcsSolution)` for FITS WCS with SIP. The PixInsight decoders (1.9.5
  standard block with its RBF distortion model; legacy ≤ 1.9.4 block with
  its interpolation grids) are copied from `mmm-core` and evaluated
  directly rather than sampled onto grids. This code is duplicated from mmm
  for now: mmm and mii keep their own copies, and consolidation comes with
  their in-process integration.
  `AstrometricSolution` is `#[non_exhaustive]`, so code outside the crate
  keeps a fallback arm for the `Wcs` form when it gains values.
  `AstrometricSolution::from_header(&Header) -> Result<Option<Self>>`
  decodes the standard block, falling back to the legacy one, and gives
  `Ok(None)` when the header holds no solution. A solution with a distortion
  or projective layer needs the image size (`Header::image_size`, else
  `NAXIS1`/`NAXIS2`) for its consistency checks and is an error without
  one; a linear-only solution needs none.
  Persist the `Header`, not an encoded solution: both models derive serde,
  but postcard is positional, so a stored solution breaks when the model
  gains a field, while re-decoding a stored `Header` does not. That holds
  only when the `Header` is stored in a self-describing encoding such as
  JSON. A postcard-encoded `Header` is positional too: adding a field to
  `Header` or `FitsKeyword` breaks `Header` values already stored that way.
  Only the Gnomonic projection and the standard native frame are decoded;
  anything else is an `Error::Astrometry`, never a wrong mapping. The
  linear-only `wcs` module and the sidecar are unchanged.

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

## DZI publishing decisions (2026-08-21)

- Tiles must be genuinely 8-bit before `dzsave`: vips savers CLIP (not
  rescale) over-range values — a 16-bit input would blow out to white.
  Routing: XISF → Rust 8-bit temp BigTIFF; 8-bit vips inputs direct;
  16-bit vips inputs via `vips msb` (deterministic high byte); other
  sample formats are rejected (publish takes stretched, display-ready
  input by design).
- The viewer page inlines the tile source as a JS object (the `.dzi` XML
  is still emitted for other consumers) and bundles OpenSeadragon v6.1.0
  (BSD-3-Clause, see NOTICE) with all-relative references, with the
  `OPENSEADRAGON-LICENSE.txt` file riding alongside — the output works on
  any dumb static host, per the positioning commitment.
- Viewer OSD settings mirror the production Astrometrical platform viewer
  (custom controls, navigator bottom-left, maxZoomPixelRatio 4,
  visibilityRatio 1). When a solution exists, center RA/Dec and pixel
  scale are baked into the page's telemetry footer at publish time (a
  live WCS readout is phase-6 DeepSky work).
- Universal viewer mode: `--mode single` / auto below 4096 px per side
  uses OSD's single-image source, no pyramid; oversize explicit-single
  warns and suggests DZI.

## Phases

1. **CLI** (phase 1, completed 2026-08-21): convert + sidecar + DZI/HTML.
   Remaining loose ends: real-data Photoshop
   smoke test (manual). Smoke test that matters: a >4 GB BigTIFF opens in
   Photoshop (PS ≥ 26.2 saves BigTIFF; opening support is older).
2. **PixInsight file-format module, write path (phase 2, complete):**
   plan A's Rust worker + IPC, plan B's C++ PCL host module and the
   `mpp-pxm.so` shim itself — a write-only "BigTIFF" format (`.btf`,
   `.tf8`) streaming pixel bands over the shm host/worker channel (pattern
   proven in MMM's integration/) to a Rust worker that encodes. Write-only
   was legitimate as a v1 scope (PI's RAW format is also read-only); the
   read path landed as phase 3, below. Machine-verified (cargo gates,
   host-lib golden `ctest`, module link/symbol gates), built by CI for all
   four targets, smoke-tested in PixInsight itself. Reuses MMM's CI
   build/sign/publish scripts; the Astrometrical update repo serves both
   packages.

   Decisions made while landing plan B (implementation plan
   `docs/superpowers/plans/2026-08-22-pi-module-shim.md`):
   - **Host-push module architecture**: unlike MMM's `mmm-pxm` (worker pulls
     input panel rows and pushes output bands, a bidirectional
     request/reply server loop), mpp's protocol is host-push and
     one-directional (`docs/pi-protocol.md` §1) — the PCL shim already has
     every pixel PixInsight hands it via `WriteSamples`, so it simply
     streams `Band` frames as bands arrive; `mpp::WriteSession` (the host
     driver) has no callback interfaces to implement, just a synchronous
     fill-a-slot/send/await-ack rhythm over a free-slot pool.
   - **Sample-format mapping** — *superseded by phase 4*, recorded as the
     v1 shape (PI image type → wire `bit_depth`; every `WriteSamples` buffer
     was normalized to f32 in `[0,1]` regardless of the negotiated output
     format; protocol v3 replaced this with an identity mapping and native
     bands): `pcl::Image` (f32) → `"f32"` native;
     `pcl::DImage` (f64) → `"f32"` narrowed (lossy, reported);
     `UInt8Image`/`UInt16Image` → `"u8"`/`"u16"` exact;
     `UInt32Image` → `"f32"` via `v/(2^32-1)` (lossy, reported). See
     `integration/pixinsight/README.md`'s mapping table for the full detail.
   - **No-options v1** — *the options dialog and ICC storage arrived in
     phase 4*: v1 shipped no format-specific options dialog and no
     ICC/thumbnail/property storage, on the principle that the module
     advertises only what is true. A FITS-keyword/property sidecar into
     BigTIFF is still unbuilt (phase 5 below), and the WCS sidecar stays
     `mpp`'s own JSON mechanism regardless.
   - **Manual-smoke verification boundary**: loading the module inside
     PixInsight is deliberately *not* automated. The machine-verifiable
     boundary is compile/link/symbol gates (zero warnings, no undefined
     `mpp::` symbols, the three PCL entry points exported) plus the host
     library's golden `ctest` suite, which proves the wire protocol and
     process lifecycle against the real `mpp-ipc-worker` binary with no
     PixInsight involved. The human-run manual smoke procedure is
     documented in `integration/pixinsight/README.md`.
   - **C++20 forced for the module** (not the plan's original C++17
     constraint): PCL 2.9.x's `Image.h` does not compile under C++17
     (`pcl::Complex<float>` clip/truncate comparisons only resolve under
     C++20 overload rules, confirmed empirically against the installed
     1.9.4 SDK's 2.9.4 headers; the pinned 2.8.3 headers do compile as
     C++17, but the macOS-arm64 leg builds from the 2.10.x arm64 pin, so
     C++20 is kept uniform per-pin). Scoped narrowly: only
     `integration/pixinsight/module/`'s translation units build as C++20;
     `integration/pixinsight/host/` (the PCL-free static lib) remains
     C++17-clean, gated by its own CMake build.
   - **Compatibility policy — PixInsight >= 1.9.0** (adopted from
     MergeMosaic after its 1.9.3 field report "Unsupported API version 187
     (expected <= 186)"): the PCL static lib bakes `PCL_API_Version` into
     the module as its declared core-API floor, so the module is built
     against the PCL matching the OLDEST supported core — PCL 2.8.3 / API
     0x0182 for all x64 targets, with a separate macOS-arm64 pin (PCL
     2.10.4 / 0x0187; pre-2.10.3 PCL cannot compile for arm64, and every
     arm64 core is >= 1.9.4). Single source of truth + hard build gates:
     `integration/pixinsight/ci/pcl-pin.env`.
   - **v1 defers PI-abort wiring**: PixInsight's abort mechanism is not
     wired to a mid-write `Cancel` in v1. `Close()` on a still-open write
     session sends `Cancel` and *blocks* until the worker replies (a wedged
     worker hangs `Close()`); only the destructor path (no `Close()` ever
     ran) is non-blocking, killing and reaping the worker directly. Noted
     as a known limitation in `integration/pixinsight/README.md`.
3. **PixInsight file-format module, read path (phase 3, complete):**
   `BigTiffFormat`/`BigTiffInstance` gained `Open()`+`ReadImage`, turning the
   module into a general read+write TIFF/BigTIFF importer (implementation
   plan `docs/superpowers/plans/2026-09-08-bigtiff-reader.md`, design spec
   `docs/superpowers/specs/2026-09-08-bigtiff-reader-design.md`). New
   extensions `.big.tiff`/`.big.tif` registered alongside the existing
   `.btf`/`.tf8` (both since withdrawn — see phase 4); the wire protocol
   bumped to v2 (`docs/pi-protocol.md` §9)
   to carry a worker-pull read direction alongside the original host-push
   write direction. See `integration/pixinsight/README.md`'s "Read support"
   section for the user-facing behavior and deferred items.

   Decisions locked while landing this plan:
   - **Whole-image read only.** The core drives an interactive `File > Open`
     through `ReadImage`, never `ReadSamples`, even with
     `CanReadIncrementally()` returning `true` (empirically verified against
     a real PixInsight core — see the design spec's "Findings"). The module
     therefore leaves `CanReadIncrementally()` `false` explicitly and owns
     the whole decode loop itself; `mpp-core`'s reader is internally
     chunked/streaming (one strip/tile at a time, never the whole image in
     worker memory) but that's an implementation detail invisible to PCL.
   - **Worker read-mode is the write protocol inverted, not a new shape.**
     Rather than design a second protocol, v2 reuses the exact `Band`/
     `BandHeader` wire layout and the slot-pool discipline of the write path
     (`docs/pi-protocol.md` §6-§7), just swapping who fills a slot and who
     acks it: on read the *worker* fills and requests (`ReadBand`, tag 6),
     the *host* consumes and acks (`ReadBandAck`, tag 132). This kept the
     protocol change additive (new tags + one `Init` field) instead of a
     breaking redesign, and let the host/worker read-mode code reuse the
     write path's free-slot-pool pattern nearly verbatim.
   - **Native fidelity on read** — the contrast this bullet originally drew,
     against a write path that normalized everything, was *abolished by
     phase 4*: protocol v3 made the write direction native too, so there is
     no longer an asymmetry to contrast against. What stands, unchanged: a
     u16 file decodes into a `UInt16Image`, an f32 file into a `pcl::Image`,
     with no forced conversion. `ImageMeta.bit_depth`
     (`docs/pi-protocol.md` §9.2) reports the file's own native sample
     format, which is why the module's `Open()` uses a geometry-probe worker
     run (`mpp-ipc-worker --probe`) to select the matching `ReadImage`
     overload before any pixels move. As shipped in phase 3, every
     `WriteSamples` buffer was still normalized to f32 in `[0,1]`, which is
     the asymmetry phase 4's `band_format` closed.
   - **Extension additions, since superseded.** `.big.tiff`/`.big.tif` were
     added during phase 3 to close the "choosing BigTIFF in the save dialog
     appends `.big`, which nothing can reopen" gap (design spec finding 5)
     and to give Photoshop and PixInsight a shared bidirectional extension.
     Both were **withdrawn** afterwards — see "Stock TIFF replacement"
     below.
4. **Stock TIFF replacement and TIFF parity (phase 4, complete):** the
   module claims `.tif`/`.tiff` outright and **replaces** PixInsight's stock
   TIFF module, which must be uninstalled first (design spec finding 1: the
   core refuses a second format claiming an owned extension). The swap is
   reversible through Manage Modules / Install Modules; the procedure and its
   costs are in `integration/pixinsight/README.md`, "Replacing the stock TIFF
   module".

   The replacement was forced by a Save As dialog behavior found while
   debugging a "BigTIFF is missing from the format list" report: the dialog
   drops a TIFF-family format whose extensions collide with those of the
   format currently selected as the default. The compound `.big.tiff`/
   `.big.tif` collided with the stock module's `*.tif`/`*.tiff`, so BigTIFF
   vanished whenever TIFF was the default — for windows loaded from a
   `.big.tiff`, and for non-file-backed windows after any TIFF save.
   Narrowing to `.btf`/`.tf8` fixed the dialog but broke the Photoshop
   bridge, since Photoshop recognises TIFF only by a `.tif`/`.tiff` tail.
   **No extension set satisfies both while the stock module is installed**,
   so coexistence was abandoned. Full investigation, including the
   hypotheses falsified along the way: `docs/tiff-parity.md`.

   Displacing the stock module made parity mandatory rather than optional —
   every remaining gap became a regression for anyone who swaps — so the
   same phase closed it (design spec
   `docs/superpowers/specs/2026-09-09-tiff-parity-design.md`, implementation
   plan `docs/superpowers/plans/2026-09-09-tiff-parity.md`). A PixInsight
   user saving a TIFF now chooses sample format, compression, container,
   strips-vs-tiles, alpha association, an embedded ICC profile, resolution
   and the three TIFF text tags, from an Astrometrical-branded options
   dialog.

   Decisions locked while landing this plan:
   - **Hand-rolled IFD writer, not a fork of the `tiff` crate.** The crate's
     `ImageEncoder` has no tile support, but every primitive it withholds is
     public one layer down — `TiffEncoder::image_directory()`'s
     `DirectoryEncoder` (`write_data()`/`write_tag()`), the
     `encoder::compression` compressors, and `TiffKind`/`TiffKindStandard`/
     `TiffKindBig` as a type parameter. So `mpp-core` writes IFDs itself
     (`formats/tiff_write/`) on top of the crate's public API: strips **and**
     tiles, classic **and** BigTIFF, five sample formats, 1–4 channels, four
     compressors and the horizontal predictor all fall out of one generic
     encode loop, with no vendored encoder to re-merge and no wait on
     upstream. This falsified `docs/tiff-parity.md`'s framing of tiled write
     as the schedule-driving item: it is the largest single piece of work,
     but it is ordinary work.
   - **Protocol v3 carries write bands in their native sample type.** Honest
     `CanStore32Bit`/`CanStoreDouble` could not be fixed in the encoder
     alone: every write band crossed shm as unit-range `f32`, and f32's
     24-bit significand destroys u32 precision before `mpp-core` ever sees a
     sample. `InitMessage` therefore gained `band_format`, describing the shm
     payload's own type independently of `bit_depth` (the output file's), so
     `u32` and `f64` reach the encoder intact. Fifteen new `Init` fields in
     all, every one `#[serde(default)]`, so a v2 payload still parses into
     exactly the v2 behaviour (`docs/pi-protocol.md` §10). This also closes a
     standing asymmetry rather than inventing a mechanism — the read path has
     carried native sample formats since phase 3.
   - **`Container::Auto` is the default, and a `.btf`/`.tf8` tail overrides
     it.** Owning `.tif`/`.tiff` meant every save was a BigTIFF wearing a
     classic extension, which a good deal of older software will not open;
     `Auto` writes classic TIFF while the uncompressed payload fits under
     4 GiB (less a 16 MiB header/IFD allowance) and BigTIFF above it.
     Compression only ever shrinks the payload, so `Auto` never
     under-predicts. `.btf` and `.tf8` are container *declarations*, not
     merely TIFF-family extensions, so under `Auto` they force BigTIFF at
     any size — a classic TIFF inside a `.btf` would be a file whose name
     lies about its contents. The override is scoped to `Auto` alone: an
     explicit `Container::Classic` on a `.btf` path stays classic, because
     the caller named the container outright and the writer does not
     second-guess an explicit request. An explicit `Classic` on an
     over-large image fails at option validation, before any pixels move.
   - **Read policy: palette, YCbCr and planar in; CMYK and Lab out.** The
     first three unwind exactly or by spec — `ColorMap` is a lookup table,
     TIFF 6.0 §21 defines the YCbCr inverse in terms of the file's own
     `YCbCrCoefficients`/`ReferenceBlackWhite`, and planar storage is a
     mechanical de-interleave — so accepting them costs nothing in fidelity.
     CMYK and Lab need ICC colour management this project does not have, and
     an unmanaged conversion would silently misrepresent pixel values, which
     is precisely what the linear-data discipline exists to prevent. They
     keep an explicit rejection whose text says *why*, not just
     "unsupported". Two of the analysis document's own premises were wrong
     here and are recorded in `docs/tiff-parity.md` so nobody re-derives
     them: the `tiff` crate refuses palette through every public path, and it
     does not invert YCbCr — both had to be implemented in this repo.
   - **Refuse rather than approximate, on read.** A YCbCr file declaring
     coefficients the reader cannot honour, a degenerate `ReferenceBlackWhite`
     (a channel with no coding range), and an unreadable ICC tag are all hard
     errors now. A plausible-looking wrong image is worse than a refusal,
     for the same reason CMYK is rejected.
   - **The options dialog is branded, and that is the product requirement.**
     `TiffOptionsDialog` follows MergeMosaic's `MmmInterface` idiom (a
     `GUIData` control struct, `SectionBar` sections, HTML tooltips,
     `e_`-prefixed handlers) and opens with the Astrometrical notice block —
     chevron, "Mega Pixel Processor — TIFF Options", tagline, copyright and
     the three links. A user who has swapped modules must be able to tell at
     a glance which module is loaded; a dialog indistinguishable from the
     stock one would make the swap invisible and its failure modes
     unattributable.
   - **No global format preferences.** `CanEditPreferences()` stays `false`:
     one per-save dialog, defaults in code. Sticky defaults are a later
     change if users ask for them, and shipping a preferences surface would
     have added a second source of truth for every option before there was
     any evidence one is wanted.
   - **`CanStoreKeywords()` stays `false`.** TIFF has no FITS-keyword
     equivalent, so answering `true` would be the same truth-in-advertising
     violation this phase existed to fix. `ImageDescription`, `Copyright` and
     `Software` are three specific tags and travel as format-specific data,
     not as keywords.
   - **The dialog has no automated test, by standing decision.** The
     machine-verifiable boundary set in phase 2 is unchanged, and no gate in
     this repo touches a PCL widget. The manual smoke procedure in
     `integration/pixinsight/README.md` is therefore the *only* coverage the
     dialog and the PixInsight integration will ever have; it is written as a
     regression test (several steps would have failed before this phase, the
     32-bit-integer round trip most pointedly) rather than a checklist of the
     obvious, and it must be run before the branch is called done.

   **User-visible behaviour changes**, recorded because they are easy to
   miss and each is a support question waiting to happen:
   - Small `.tif`/`.tiff` saves are now **classic TIFF**, where every save
     used to be BigTIFF. `.btf`/`.tf8` still get BigTIFF at any size under
     `Auto`; an explicit `Classic` on such a path is honoured as written.
   - `u8`/`u16` output can differ from pre-phase-4 builds by 1 LSB: integer
     scaling now goes through `f64`, which is strictly more accurate.
   - Resolution tags are now written whenever both values are positive,
     which in practice means always — PCL's `ImageOptions` defaults to
     72 dpi. Previously none were written and the round trip lost them.
   - `WasLossyWrite()` now reports `true` for an f32 source saved as
     `u16`/`u8`, which it did not before, and no longer reports `true` for
     the whole `u32`/`f64` category, which is no longer narrowed. It also
     reports `true` when the save options discard the alpha channel: the
     user asked for the drop, but a whole channel of the source is absent
     from the file and calling that lossless would understate it.
   - A palette file reports a `u16` sample format, because `ColorMap`
     entries are 16-bit.
   - The refusals listed above (YCbCr coefficients, `ReferenceBlackWhite`,
     unreadable ICC tag) are new errors on files that previously either did
     not open at all or would have been approximated.

5. **Deferred from phase 4, not yet assigned a phase:** WCS sidecar readback
   (reading a `.wcs.json` — or any other WCS source — back into PixInsight
   FITS keywords on Open; the sidecar remains `mpp`'s own JSON mechanism);
   incremental read (`ReadSamples`), which the core never drives; thumbnails,
   image properties and general TIFF-tag ↔ FITS-keyword mapping; global
   format preferences (`CanEditPreferences()`); non-blocking abort wiring
   (the phase-2 limitation, still standing); JPEG-in-TIFF and Fax compression
   on **write** (lossy compression is wrong for linear astro data); and CMYK
   and Lab **read**, which stay refused until this project has colour
   management. Also open: JPEG-compressed subsampled YCbCr on read is
   currently *accepted* and untested — see `docs/tiff-parity.md`'s
   "Remaining gaps".

6. **DeepSky viewer components / platform hooks**: richer viewer (WCS
   readout, annotations, catalog overlays), possible hosted platform.

## Licensing constraints (important — applies to the PixInsight module, phases 2-4)

- Our code: Apache-2.0.
- **PCL License 2.0.1 (Dec 2025) added AI-use restrictions** ("expressly
  includes use via APIs, wrappers, or third-party tools"). Founding decision:
  **do not fork or even read PixInsight's TIFF module source** (or any PCL
  module implementation) in this project — AI-assisted or otherwise. The
  module (its phase-2 write path, its phase-3 read path, and phase 4's
  options dialog and parity work alike) is clean-room: designed from the BigTIFF spec, libtiff/vips documentation,
  and the public PCL API only — the same link-against-the-API posture as
  MMM's existing mmm-pxm module. This provenance is recorded in the
  module's README (`integration/pixinsight/README.md`).
- PCL-linked module code (phases 2-4) carries the PCL license compliance
  set: retain notices, ship the license text, the verbatim acknowledgment
  sentence in end-user docs/NOTICE, no PixInsight/Pleiades endorsement
  implication, and clear scoping so PCL-licensed parts are separate from
  Apache-2.0 code.
- libvips is LGPL-2.1+: dynamic linking + standard LGPL compliance
  (attribution, library sources/relink ability) when distributing binaries.

## Verified facts

- libvips: `dzsave` writes DeepZoom pyramids in constant memory; `tiffsave`
  has a `bigtiff` flag; float band formats native; FITS via cfitsio when
  built with it; official Windows/macOS/Linux binaries exist.
- Rust `tiff` crate: `TiffEncoder::new_big()` writes BigTIFF, pure Rust.
- PixInsight bundles libtiff 4.6.0 (BigTIFF-capable) but exposes no BigTIFF
  write path; format modules are the only way to add file formats (PJSR
  scripts cannot).
- Photoshop: opens BigTIFF; saves BigTIFF since ~26.2.
