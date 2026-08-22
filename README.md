# Mega Pixel Processor (`mpp`)

Get big astro images out of astro software and into the world.

`mpp` is a standalone command-line tool (and future PixInsight file-format
engine) for astronomically large images:

- **Convert** between XISF, FITS, TIFF/**BigTIFF**, and mainstream formats
  (PNG, JPEG, WebP, AVIF, ...) at any size — including past classic TIFF's
  4 GB limit, so gigapixel mosaics reach Photoshop intact.
- **Keep the astrometry**: astrometric solutions travel natively where the
  container supports them (FITS, XISF) and via a JSON sidecar where it
  doesn't (TIFF, PNG, DZI).
- **Publish**: export any image as a DZI (Deep Zoom) tile pyramid with a
  ready-made interactive HTML viewer — plain static files you can host
  anywhere, no platform required.

Part of the Astrometrical "Mega" tool family, alongside
[Mega Merge Mosaic](https://github.com/Astrometrical/mega-merge-mosaic).

## Status

**Phase-2 (Plan B) complete:** A write-only PixInsight file-format module, `BigTIFF-pxm.so`, lets PixInsight save images directly as BigTIFF (`.btf`, `.tf8`) past the classic TIFF 4 GB limit. The module is machine-verified (all cargo gates green; host library CTest golden suite 8/8; module `.so` builds clean, zero `-Wall -Wextra` warnings, links fully with no undefined `mpp::` symbols; version-sync test guards protocol agreement). Manual PixInsight load/save smoke pending on the human side; unsigned — Astrometrical CPD certificate pending. See [integration/pixinsight/README.md](integration/pixinsight/README.md) for build, install, and manual-test instructions.

**Phase-1 complete:** XISF → BigTIFF plus PNG/JPEG/WebP/AVIF/JXL via the vips CLI (`libvips-tools` required for the non-TIFF formats), each with the WCS JSON sidecar, and DZI tile-pyramid export with an Astrometrical-branded OpenSeadragon viewer. See the roadmap in [docs/DESIGN.md](docs/DESIGN.md).

## Smoke tests

The phase-1 acceptance criterion is a >4 GB BigTIFF that opens in
Photoshop (PS ≥ 26.2 also saves BigTIFF; opening support is older).

- Synthetic end-to-end: `cargo test -p mpp --release -- --ignored`
  (writes ~9 GB of temporary data).
- Real data (manual): `cargo build --release`, then
  `target/release/mpp convert <big>.xisf <big>.tif` on a real mosaic and
  open the result in Photoshop. A solved input also writes
  `<big>.wcs.json` next to the output.
- Mainstream formats (requires libvips-tools): `target/release/mpp convert
  <big>.xisf <big>.png` — PNG defaults to 16-bit; JPEG/WebP/AVIF/JXL take
  `--quality` (JXL is lossless by default when `--quality` is omitted).
  Format dimension ceilings are preflighted before conversion: WebP tops
  out at 16383 px per side, JPEG at 65500; AVIF's limit is
  encoder-dependent.

## Publishing a zoomable web view

```sh
mpp publish mosaic.xisf site/ --title "Veil Nebula"
# then serve `site/` from any static host:
python3 -m http.server -d site/
```

DZI tile pyramid + a self-contained OpenSeadragon viewer (no CDN, works
self-hosted anywhere). Images ≤ 4096 px publish as a single-image page;
`--mode dzi|single` overrides. A solved input also writes the
`*.wcs.json` sidecar next to the tiles and bakes center coordinates into
the page footer.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
