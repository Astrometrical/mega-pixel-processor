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

First two phase-1 slices implemented: XISF → BigTIFF plus PNG/JPEG/WebP/AVIF/JXL via the vips CLI (`libvips-tools` required for the non-TIFF formats), each with the WCS JSON
sidecar. DZI/HTML export is upcoming phase-1 work; see the roadmap in [docs/DESIGN.md](docs/DESIGN.md).

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

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
