# TIFF parity: displacing PixInsight's stock TIFF module

Status: analysis only, 2026-09-09. No code written. Feeds a future
implementation plan under `docs/superpowers/plans/`.

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
stops being optional. This document is the gap.

## Write side: the stock TIFF Options dialog

The stock module presents Sample Format, Compression, Embedded Data,
Miscellaneous Options, and three free-text tag fields. Mapped against what
we do today:

| Stock option | Our state | What blocks it |
|---|---|---|
| **Sample format** 8/16/32-bit uint, 32/64-bit float | We *advertise* all of these (`CanStore32Bit`/`Float`/`Double` true) but silently narrow: u32 → f32 and f64 → f32, reported only via `WasLossyWrite()` | `BitDepth` enum is `F32\|U16\|U8` (`tiff.rs:23`); wire `bit_depth` is `"f32"\|"u16"\|"u8"` (`pi-protocol.md:183`). The encoder already has `Gray32`/`Gray64Float`/`RGB32`/`RGB64Float` |
| **Compression** None / Deflate / LZW | Uncompressed only; `SupportsCompression()` false | `TiffWriteOptions` (`tiff.rs:35`) has no field. tiff 0.11.3 ships `Uncompressed`, `Lzw`, `Deflate`, `Packbits` — exact coverage plus one |
| **Embedded data** ICC profile | `CanStoreICCProfiles()` false | Tag write path + a protocol field to carry the blob |
| **Planar organization** | Reader *rejects* planar; writer never offers it | Encoder is chunky-only; needs checking whether it's worth supporting at all |
| **Associated / premultiplied alpha** | `CanStoreAlphaChannels()` false | Protocol accepts `channels == 1 \| 3` (`pi-protocol.md:205`), enforced at `BigTiffInstance.cpp:414`. Encoder has `RGBA8/16/32/32Float/64/64Float` |
| **Image Description / Copyright / Software** | `CanStoreKeywords()` false | TIFF tags 270 / 33432 / 305; needs a tag write path and a protocol field |
| *(not in stock)* **Tiled output** | Reader handles tiled; writer is strip-only | **tiff 0.11.3's encoder has no tiled write at all** — see below |
| *(not in stock's dialog)* Resolution | `CanStoreResolution()` false | TIFF carries XResolution/YResolution; PI shows 72 px/inch. Round-trip loss today |

### The one piece of genuinely new work

Everything above except tiling is plumbing — the `tiff` crate's encoder
already supports it and our own layers are what say no. **Tiled write is the
exception: `tiff-0.11.3/src/encoder/` contains no tile support whatsoever**
(the *decoder* does). Since tiled output is specifically wanted for the
Astrometrical large-image load path, this is the item to scope first:
upstream a tiled encoder, vendor/fork the crate, or hand-roll tiled IFD
writing against our own strip-streaming code. Everything else is a smaller,
better-understood change.

## Read side

Our reader rejects palette, CMYK, YCbCr, Lab, other multiband colour types,
planar storage, signed-integer samples, complex/void samples, and any bit
depth outside u8/u16/u32/f32/f64.

Good news: **these are our policy choices, not crate limits.** tiff 0.11.3's
decoder `ColorType` covers `Gray`, `RGB`, `Palette`, `GrayA`, `RGBA`,
`CMYK`, `CMYKA`, `YCbCr`, `Lab` and multiband, and handles tiled input. So
read parity is conversion decisions and plumbing (what does a CMYK or Lab
TIFF *become* in PixInsight?), not new decoding work.

## PCL mechanism gaps

We currently present no options dialog at all. To show one:

- **`QueryOptions()`** (`FileFormatImplementation.h:506`) — the hook that
  presents the dialog. Not implemented.
- **`UsesFormatSpecificData()`** false → `SetFormatSpecificData` is never
  called, so per-save options can't round-trip.
- **`CanEditPreferences()`** false → no global format preferences.
- A PCL `Dialog` subclass for the UI itself. Per the product call, styled
  distinctly from the stock dialog with Astrometrical branding, to avoid
  confusion about which module is loaded.

## Protocol changes implied

`docs/pi-protocol.md` §5 `InitMessage` needs: wider `bit_depth` (add `u32`,
`f64` — the *read* path already carries all five, `pi-protocol.md:633`),
`channels` widened past `1|3` for alpha, and new fields for compression,
tiling, ICC blob, and the three text tags.

Note the standing asymmetry this would close: the read path is already
native-fidelity across five sample formats and handles alpha, while the
write path normalises everything to f32 and refuses alpha. We can currently
read files we cannot write.

## Not yet investigated

- Whether the stock module writes anything else we haven't seen (its dialog
  is the only evidence used here).
- What PixInsight itself does with a format that declines `CanStoreKeywords`
  when it *is* the only TIFF module — i.e. whether any core behaviour
  silently regresses beyond the dialog.
