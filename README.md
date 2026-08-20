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

Early scaffold — the workspace builds, the roadmap lives in
[docs/DESIGN.md](docs/DESIGN.md), and no conversion paths are implemented yet.

## Smoke tests

The phase-1 acceptance criterion is a >4 GB BigTIFF that opens in
Photoshop (PS ≥ 26.2 also saves BigTIFF; opening support is older).

- Synthetic end-to-end: `cargo test -p mpp --release -- --ignored`
  (writes ~9 GB of temporary data).
- Real data (manual): `cargo build --release`, then
  `target/release/mpp convert <big>.xisf <big>.tif` on a real mosaic and
  open the result in Photoshop. A solved input also writes
  `<big>.wcs.json` next to the output.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
