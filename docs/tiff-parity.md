# TIFF parity: displacing PixInsight's stock TIFF module

**Status: RESOLVED (2026-09-10).** Every gap this document opened is closed.
The analysis below is kept for its investigation record — the root cause of
the vanishing-format bug, and the hypotheses falsified chasing it — which is
the part that stays useful. What shipped is specified in
`docs/superpowers/specs/2026-09-09-tiff-parity-design.md` and was built by
`docs/superpowers/plans/2026-09-09-tiff-parity.md`; `docs/DESIGN.md` phase 4
records the decisions and `integration/pixinsight/README.md` documents the
user-facing behaviour. What is still open is in "Remaining gaps" at the
bottom of this file — it is a short list.

**Two of this document's own premises turned out to be wrong.** Both were
believed when it was written, both were falsified during implementation, and
both are recorded here so nobody re-derives them:

1. **The `tiff` crate does not hand back raw palette indices.** "Read parity
   is conversion decisions and plumbing, not new decoding work" (below) was
   true for planar storage and false for palette: the crate refuses a
   `PhotometricInterpretation::RGBPalette` image through every public path,
   so there is no way to get the indices out and apply the `ColorMap`
   ourselves via the decoder's normal API. The palette expansion in
   `crates/mpp-core/src/formats/tiff_read.rs` reads the `ColorMap` tag and
   does the lookup itself.
2. **The `tiff` crate does not invert YCbCr.** The design spec inherited the
   same assumption ("the decoder already inverts the transform") and it is
   not so. The TIFF 6.0 §21 inverse, parameterised by the file's own
   `YCbCrCoefficients` and `ReferenceBlackWhite` tags, is implemented in this
   repo.

The third premise worth flagging as *right but misleading*: tiled write was
framed below as "the one piece of genuinely new work" that would drive the
schedule. It was the largest single piece, but it turned out to be ordinary
work — `tiff` 0.11.3's `DirectoryEncoder`, compressors and `TiffKind` are all
public one layer below `ImageEncoder`, so a hand-rolled IFD writer needed no
fork, no vendoring and no upstream wait.

## Why we're here

The mpp BigTIFF format vanished from PixInsight's Save As dialog for some
windows. Root cause, established empirically over four test builds: **the
dialog drops our format whenever the stock TIFF format is the selected
default**, because our compound `.big.tiff`/`.big.tif` extensions collide
with the stock module's `*.tif`/`*.tiff`. Two symptoms, one cause:

1. A window loaded from `.big.tiff` gets TIFF offered as the save default,
   and BigTIFF is missing from the list.
2. A non-file-backed window gets the *last used* format as its default,
   which is TIFF if either TIFF or BigTIFF was used last — BigTIFF missing
   again.

Falsified along the way, recorded so they aren't re-tested: format
capability declarations (every query in `FileFormatBase.h:225-458` is a
`const` method on the format with no image argument, so answers can't vary
by window, and there is no size-related capability at all); image
characteristics (the same 7.7 GiB RGB f32 mosaic is refused as a new window
and accepted after an XISF round trip); and extension *ordering* (putting
`.btf` first changed nothing — only *dropping* the compound pair did).

`.btf`/`.tf8` alone fixes the dialog but breaks the Photoshop bridge:
Photoshop recognises TIFF only by a `.tif`/`.tiff` tail, and PixInsight
breaks if we claim anything with that tail while the stock module is
installed. **No extension satisfies both.** Hence the decision: claim
`.tif`/`.tiff` outright and displace the stock TIFF module. Verified
working — the module registers and takes over once `TIFF-pxm.so` is
uninstalled, and the swap is reversible through Manage Modules / Install
Modules.

Displacing it makes us the *only* TIFF support in PixInsight, so parity
stopped being optional. This document was the gap; the sections below now
record how each row was closed.

## Write side: the stock TIFF Options dialog

The stock module presents Sample Format, Compression, Embedded Data,
Miscellaneous Options, and three free-text tag fields. Mapped against what
we ship today:

| Stock option | What shipped |
|---|---|
| **Sample format** 8/16/32-bit uint, 32/64-bit float | All five, written for real. `BitDepth` covers `U8\|U16\|U32\|F32\|F64`; protocol v3's `band_format` carries the samples over shm in their native type, so `u32` and `f64` reach the encoder intact instead of being flattened through f32's 24-bit significand. `CanStore32Bit`/`CanStoreDouble` are now honest — that they were not is the violation this whole phase existed to fix |
| **Compression** None / Deflate / LZW | None / LZW / Deflate / **PackBits** (one better than the stock dialog), plus a horizontal predictor for integer formats — refused for float data, as the TIFF spec and the `tiff` crate both require. Deflate takes a 1–9 level. `SupportsCompression()` answers `true` accordingly, which `MetaFileFormat.h` requires of exactly this case ("The optional ZIP and LZW compressions used in TIFF are the exact kind of compressions that must cause this member function to return true") |
| **Embedded data** ICC profile | Written as tag 34675 on save and read back on open. `CanStoreICCProfiles()` is `true`; the blob rides base64 in the v3 `Init`, capped at 16 MiB decoded |
| **Planar organization** | Accepted on **read** (de-interleaved into the reader's chunky band model). Not offered on write: the writer is chunky-only by choice, since planar output buys nothing here and doubles the layout matrix |
| **Associated / premultiplied alpha** | Gray+alpha and RGBA write, with an `ExtraSamples` tag carrying the association the dialog selected. The protocol accepts `channels` in `1..=4`, with alpha and channel count validated against each other in both directions |
| **Image Description / Copyright / Software** | All three written (tags 270 / 33432 / 305), carried as format-specific data. `CanStoreKeywords()` stays `false` on purpose: these are three specific TIFF tags, not a FITS-keyword equivalent, and TIFF has none |
| *(not in stock)* **Tiled output** | Shipped. One generic encode loop serves strips and tiles alike; tile edges are powers of two in 128–1024, default 256, with zero-padded right/bottom edge tiles per the spec. Built for the Astrometrical large-image load path |
| *(not in stock's dialog)* Resolution | `XResolution`/`YResolution`/`ResolutionUnit` written from `ImageOptions`, with an inch/cm unit combo in the dialog. `CanStoreResolution()` is `true` |
| *(not in stock)* **Container choice** | Classic TIFF **or** BigTIFF, selectable, defaulting to `Auto`. This row did not exist in the original analysis and was arguably the sharpest regression on the list: before phase 4 *every* save was a BigTIFF, including small ones named `.tif`, which a good deal of older software will not open |

### The one piece of genuinely new work

*(Original framing, kept for the record; see the premise note at the top of
this file for how it turned out.)* Everything above except tiling is
plumbing — the `tiff` crate's encoder already supports it and our own layers
are what say no. **Tiled write is the exception: `tiff-0.11.3/src/encoder/`
contains no tile support whatsoever** (the *decoder* does). Since tiled
output is specifically wanted for the Astrometrical large-image load path,
this is the item to scope first: upstream a tiled encoder, vendor/fork the
crate, or hand-roll tiled IFD writing against our own strip-streaming code.
Everything else is a smaller, better-understood change.

**How it resolved:** hand-rolled, against the crate's public
`DirectoryEncoder` — no fork, no vendoring, no upstream dependency. See
`crates/mpp-core/src/formats/tiff_write/`.

## Read side

The reader used to reject palette, CMYK, YCbCr, Lab, other multiband colour
types, planar storage, signed-integer samples, complex/void samples, and any
bit depth outside u8/u16/u32/f32/f64.

| Colour / storage form | What shipped |
|---|---|
| **Palette** | Accepted. Expanded to RGB through the `ColorMap` tag, read and applied by this repo (the crate refuses palette outright — see premise 1 above). Reports a `u16` sample format, because `ColorMap` entries are 16-bit |
| **YCbCr** | Accepted, 8-bit, chunky. The TIFF 6.0 §21 inverse is implemented here, parameterised by the file's own `YCbCrCoefficients` and `ReferenceBlackWhite`. A file declaring coefficients we cannot honour, or a degenerate `ReferenceBlackWhite` (a channel whose black and white references are equal, so it has no coding range), is **refused rather than approximated** |
| **Planar / separate storage** | Accepted. A mechanical de-interleave into the reader's chunky band model; the samples themselves are untouched. Planar YCbCr is refused — the inverse needs all three components in one chunk |
| **ICC profile** | Read back from tag 34675 and returned by `BigTiffInstance::ReadICCProfile()`. An unreadable ICC tag is an error, not a silent drop |
| **CMYK, Lab** | Still refused, by decision — see "Remaining gaps" |
| **Signed integers, complex, void samples, arbitrary multiband** | Still refused; unchanged |

The original note stands and was the basis of the policy: **these are our
policy choices, not crate limits** for everything except palette and the
YCbCr inverse. `tiff` 0.11.3's decoder `ColorType` covers `Gray`, `RGB`,
`Palette`, `GrayA`, `RGBA`, `CMYK`, `CMYKA`, `YCbCr`, `Lab` and multiband,
and handles tiled input.

## PCL mechanism gaps

All closed:

- **`QueryOptions()`** — implemented. Builds and executes `TiffOptionsDialog`,
  writes the sample format and resolution into `ImageOptions`, and pushes a
  `TiffFormatOptions` block into `formatOptions`. Cancel returns `false`,
  which aborts the save.
- **`UsesFormatSpecificData()`** — now `true`, with
  `ValidateFormatSpecificData()` (magic + version) and
  `DisposeFormatSpecificData()` alongside it, so per-save options round-trip.
- **A PCL `Dialog` subclass for the UI** — `TiffOptionsDialog`, styled with
  the Astrometrical notice block (chevron, product title, tagline, copyright,
  three links) so it is unmistakably not the stock dialog. That was the
  product requirement, and it is what tells a user which module is loaded.
- **`CanEditPreferences()`** — deliberately still `false`. See "Remaining
  gaps".

## Protocol changes implied

Shipped as protocol v3 (`docs/pi-protocol.md` §10): `band_format` (the
load-bearing addition — the shm payload's sample type, independent of
`bit_depth`, the output file's), `channels` widened to `1..=4`, and fields
for container, layout, tile size, compression, deflate level, predictor,
alpha association, the ICC blob, the three text tags and resolution. Fifteen
new `Init` fields, every one `#[serde(default)]`, so a v2 payload still
parses into exactly the v2 behaviour. No new frame tags and no change to the
`Band`/`BandHeader` layout.

This closed the standing asymmetry the analysis identified: the read path was
already native-fidelity across five sample formats and handled alpha, while
the write path normalised everything to f32 and refused alpha. Both
directions are now native.

## Remaining gaps

Short, and each is a decision rather than an oversight:

- **JPEG-compressed subsampled YCbCr on read is accepted, and untested.**
  The reader checks the YCbCr transform's parameters but does *not* check
  `ChromaSubsampling`, so a subsampled file is accepted rather than refused.
  No JPEG-in-TIFF fixture exists in this repo to test it against, and it is
  the most likely YCbCr TIFF a real user has. Treat any such file as
  unverified: it may decode correctly, mis-decode, or error out of the
  `tiff` crate. Either validating and refusing subsampled YCbCr, or
  supporting it properly with a fixture, is the honest fix.
- **CMYK and Lab on read.** Refused. Converting either to RGB faithfully
  needs ICC colour management this project does not have, and an unmanaged
  conversion would silently misrepresent pixel values. The rejection message
  says so rather than only naming the colour type. This unblocks when the
  project gains colour management, not before.
- **JPEG-in-TIFF and Fax compression on write.** Not offered. Read support
  comes free from the decoder where it works; writing lossy compression is
  wrong for linear astro data and is not parity with the stock dialog
  either.
- **Global format preferences.** `CanEditPreferences()` stays `false`: one
  per-save dialog, defaults in code, no sticky settings. A later change if
  users ask for it.
- **FITS keywords, thumbnails and image properties.** Nothing beyond
  geometry, pixel data, the ICC profile, resolution and the three text tags
  crosses the container. `CanStoreKeywords()` is `false` and stays that way
  — TIFF has no FITS-keyword equivalent.
- **Signed integers, complex and void samples, arbitrary multiband.**
  Unchanged rejections.
- **IFDs can land on an odd file offset.** TIFF 6.0 requires an IFD to start
  on a word boundary. The writer emits the directory immediately after the
  pixel data with no padding, so whenever the total payload is an odd number
  of bytes the IFD offset is odd too: uncompressed `u8` output with an odd
  sample count (a 3x3 `u8` image puts it at offset 17 classic / 25 BigTIFF),
  or any compressed output whose block bytes happen to sum to an odd count
  (a 7x3 `u16` PackBits image lands at 51 / 59). Wider *uncompressed* sample
  formats always keep the payload even and are never affected. This is
  **inherited, not introduced**: it is how the `tiff` crate's own strip
  writer has always behaved, it predates this phase, and libtiff and Pillow
  both read such files without complaint. Left alone deliberately — the fix
  is a padding byte before the directory, which changes every file this
  project has ever written.

## Not yet investigated

- Whether the stock module writes anything else we haven't seen (its dialog
  is the only evidence used here — the clean-room rule forbids reading its
  source).
- What PixInsight itself does with a format that declines `CanStoreKeywords`
  when it *is* the only TIFF module — i.e. whether any core behaviour
  silently regresses beyond the dialog. Still open; the manual smoke
  procedure in `integration/pixinsight/README.md` is where it would surface.
