# Mega Pixel Processor (mpp)

Standalone converter/publisher for big astro images: XISF/FITS/TIFF/BigTIFF
(+ libvips formats) conversion, astrometric WCS carried natively or via JSON
sidecar, and DZI tile-pyramid export with an Astrometrical-branded
OpenSeadragon HTML viewer. Read `docs/DESIGN.md` first — it records the
founding decisions, licensing constraints, and phase plan. Sibling project of
Mega Merge Mosaic (`~/dev/MergeMosaic`); "Mega" is the Astrometrical family
branding, `mpp` the binary.

## Layout

- `crates/mpp-core` — engine library (UI-agnostic; PixInsight worker and any
  GUI will reuse it)
- `crates/mpp` — CLI frontend (thin)
- `docs/DESIGN.md` — architecture spec; keep it current when design changes
- `docs/superpowers/plans/` — implementation plans

## Commands

```sh
cargo test                 # unit + integration tests (no big files needed)
cargo build --release      # required for real-data runs (multi-GB images)
```

Rust via rustup (`source ~/.cargo/env` if PATH lacks cargo).

## Conventions

- Mirror MMM's engineering conventions: every public item in `mpp-core` gets a
  doc comment (`missing_docs` is warned); keep `cargo fmt`,
  `cargo clippy --all-targets`, and `cargo doc` warning-free; `lib.rs`
  documents the intended public API surface.
- Tests must not depend on `test_data/` (synthesize inputs); real-data runs
  are manual smoke tests.
- Linear data discipline where applicable; DZI/viewer paths take stretched,
  display-ready input by design.
- DZI + HTML outputs must remain dumb static files that work on any web host
  (self-hostability is a product commitment, not an accident).
- **Clean-room rule (licensing):** never read or derive from PixInsight's
  PCL module implementations (e.g. their TIFF module) in this project. The
  future PI format module is designed only from public specs, libtiff/vips
  docs, and the public PCL API. See "Licensing constraints" in
  `docs/DESIGN.md`.
