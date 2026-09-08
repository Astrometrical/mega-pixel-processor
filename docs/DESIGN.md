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
- Rejected names, for the record: Peta Pixel * (petapixel.com), Giga Pixel *
  (Topaz Gigapixel), Prism (Syqon Prism Deep), Parser/Press (weak/ambiguous).

## Architecture decisions

- **Rust workspace**, mirroring MMM: `mpp-core` (UI-agnostic engine),
  `mpp` (thin CLI). A PixInsight-facing worker crate comes with phase 2.
- **libvips** is the image-processing backbone (LGPL-2.1+, dynamically
  linked): demand-driven/streaming, constant memory on multi-GB images,
  `tiffsave` with `bigtiff` option, `dzsave` for DZI pyramids.
  **Decided (2026-08-20):** hybrid. The XISF→BigTIFF leg is pure Rust
  (`tiff` crate, `TiffEncoder::new_big`, strip-streamed, constant memory);
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
  live WCS readout is phase-3 DeepSky work).
- Universal viewer mode: `--mode single` / auto below 4096 px per side
  uses OSD's single-image source, no pyramid; oversize explicit-single
  warns and suggests DZI.

## Phases

1. **CLI** (phase 1, completed 2026-08-21): convert + sidecar + DZI/HTML.
   Remaining loose ends: FITS read/write evaluation, real-data Photoshop
   smoke test (manual). Smoke test that matters: a >4 GB BigTIFF opens in
   Photoshop (PS ≥ 26.2 saves BigTIFF; opening support is older).
2. **PixInsight file-format module, write path (phase 2, complete):**
   plan A's Rust worker + IPC, plan B's C++ PCL host module and the
   `BigTIFF-pxm.so` shim itself — a write-only "BigTIFF" format (`.btf`,
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
   - **Sample-format mapping** (PI image type → wire `bit_depth`; every
     `WriteSamples` buffer is normalized to f32 in `[0,1]` regardless of the
     negotiated output format): `pcl::Image` (f32) → `"f32"` native;
     `pcl::DImage` (f64) → `"f32"` narrowed (lossy, reported);
     `UInt8Image`/`UInt16Image` → `"u8"`/`"u16"` exact;
     `UInt32Image` → `"f32"` via `v/(2^32-1)` (lossy, reported). See
     `integration/pixinsight/README.md`'s mapping table for the full detail.
   - **No-options v1**: no format-specific options dialog, no ICC/thumbnail/
     property storage — the module advertises only what is true; a
     FITS-keyword/property sidecar into BigTIFF is a phase-3 candidate, and
     the WCS sidecar stays `mpp`'s own JSON mechanism regardless.
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
   extensions `.big.tiff`/`.big.tif` register alongside the existing
   `.btf`/`.tf8`; the wire protocol bumped to v2 (`docs/pi-protocol.md` §9)
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
   - **Native fidelity on read, unlike the normalized write path.** Every
     `WriteSamples` buffer is normalized to f32 in `[0,1]` (phase 2); reads
     go the other way — a u16 file decodes into a `UInt16Image`, an f32 file
     into a `pcl::Image`, with no forced conversion. `ImageMeta.bit_depth`
     (`docs/pi-protocol.md` §9.2) reports the file's own native sample
     format, which is why the module's `Open()` uses a geometry-probe worker
     run (`mpp-ipc-worker --probe`) to select the matching `ReadImage`
     overload before any pixels move.
   - **Extension additions, not replacements.** `.big.tiff`/`.big.tif` were
     added to close the "choosing BigTIFF in the save dialog appends `.big`,
     which nothing can reopen" gap (design spec finding 5) — fixed on the
     write path via a `.big`→`.big.tiff` rename (`PCL_FILEFORMAT_API.md`
     §8.4) — and to give Photoshop and PixInsight a shared bidirectional
     extension. `.tif`/`.tiff` remain deliberately unclaimed: PixInsight
     refuses a second format registering an extension the stock TIFF module
     already owns (design spec finding 1), so claiming them is scoped to a
     future stock-TIFF-replacement phase, not this one.
4. **DeepSky viewer components / platform hooks**: richer viewer (WCS
   readout, annotations, catalog overlays), possible hosted platform.

## Licensing constraints (important — read before phase 2)

- Our code: Apache-2.0.
- **PCL License 2.0.1 (Dec 2025) added AI-use restrictions** ("expressly
  includes use via APIs, wrappers, or third-party tools"). Founding decision:
  **do not fork or even read PixInsight's TIFF module source** (or any PCL
  module implementation) in this project — AI-assisted or otherwise. The
  phase-2 module is clean-room: designed from the BigTIFF spec, libtiff/vips
  documentation, and the public PCL API only — the same
  link-against-the-API posture as MMM's existing mmm-pxm module. Record this
  provenance in the module's README when phase 2 starts.
- PCL-linked module code (phase 2) carries the PCL license compliance set:
  retain notices, ship the license text, the verbatim acknowledgment sentence
  in end-user docs/NOTICE, no PixInsight/Pleiades endorsement implication,
  and clear scoping so PCL-licensed parts are separate from Apache-2.0 code.
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
