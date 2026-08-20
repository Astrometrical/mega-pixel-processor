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

## Phases

1. **CLI** (this repo, now): convert + sidecar + DZI/HTML. Smoke test that
   matters: a >4 GB BigTIFF opens in Photoshop (PS ≥ 26.2 saves BigTIFF;
   opening support is older).
2. **PixInsight file-format module**: write-only "BigTIFF" format (`.btf`,
   `.tf8`) as a thin PCL C++ shim streaming pixel bands over the shm
   host/worker channel (pattern proven in MMM's integration/) to a Rust
   worker that encodes. Write-only is legitimate (PI's RAW format is
   read-only); import can come later. Reuses MMM's CI build/sign/publish
   scripts; the Astrometrical update repo serves both packages.
3. **DeepSky viewer components / platform hooks**: richer viewer (WCS
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
