# mpp PixInsight integration: the Mega Pixel Processor module

This directory contains the **Mega Pixel Processor** PixInsight Class Library
(PCL) module, `mpp-pxm.so` (`.dylib` on macOS, `.dll` on Windows). It
registers with the core as `MegaPixelProcessor` and appears in
**Process → Modules** as *"Mega Pixel Processor: file format support for
astronomically large images"*.

Mega Pixel Processor is a **file-format module**: the formats it provides are
registered individually beneath it, each with its own extensions,
capabilities, and description. Today it provides **one** format, `BigTIFF` —
read+write BigTIFF/TIFF, shown in PixInsight's format lists as *"Mega Pixel
Processor: BigTIFF/TIFF (read+write)"*. Further formats are added as siblings
under the same module (see "Module and format naming" below); nothing about
the module's identity, binary name, or install layout is BigTIFF-specific.

The `BigTIFF` format:

- **Writes** TIFF/BigTIFF (`.tif`, `.tiff`, `.btf`, `.tf8`) from inside
  PixInsight's own Save/Save As dialogs — past classic TIFF's 4 GB limit,
  without going through `mpp`'s CLI (phase 2). Every save presents a
  branded options dialog (phase 4): sample format, compression, container,
  strips or tiles, alpha, ICC profile, resolution and the three text tags.
  With the container on its `Automatic` default, `.btf`/`.tf8` name BigTIFF
  and always get it, while `.tif`/`.tiff` get classic TIFF when the pixel
  data fits under 4 GB and BigTIFF when it does not — so an ordinary-sized
  `.tif` opens in the widest possible set of readers. Either container can
  also be demanded outright.
- **Reads** whole-image TIFF and BigTIFF files with the same four
  extensions (phase 3) back into PixInsight, including plain (non-Big)
  stripped/tiled TIFFs the module didn't itself write — the general
  streaming reader in `mpp-core` decodes any TIFF-family file in the
  supported sample/colour subset, not just the module's own output.

`BigTIFF` **claims `.tif`/`.tiff` and replaces PixInsight's stock TIFF
module** — the two cannot coexist, and the stock module must be uninstalled
first. See "Replacing the stock TIFF module" below for the procedure, how to
revert, and what you give up.

As of phase 4 it is at parity with the stock module's write options and
past them in places (tiled output, PackBits, an explicit container choice).
What it still does **not** do: read CMYK or Lab files, pass through FITS
keywords, thumbnails or image properties, or write JPEG/Fax-compressed
output. `docs/tiff-parity.md` records how the gap was closed and what is
left; see also "Out of scope" below and `docs/DESIGN.md`'s phase entries.

## Architecture

```
PixInsight core                              PixInsight core
      |  Save/Save As (write)                      |  Open (read)
      v                                             v
integration/pixinsight/module/   (mpp-pxm.so — PCL-linked shim, C++20)
  BigTiffFormat   -- format identity/capabilities (MetaFileFormat)
  BigTiffInstance -- per-file session (FileFormatImplementation)
      |  write: QueryOptions() -> TiffOptionsDialog     |  read: Open()+ReadImage,
      |  converts each WriteSamples buffer to           |  AllocateData + PixelData
      |  band_format, splits into <= band_rows chunks   |  fills straight from bands
      v                                                  v
integration/pixinsight/host/     (libmpp_host — PCL-free static lib, C++17)
  mpp::WriteSession  -- host pushes Band, worker acks    mpp::ReadSession -- worker pushes
      |  shm segment + child-process lifecycle +         |  ReadBand, host acks — inverted
      |  wire framing (mpp_shm/mpp_os,                   |  slot ownership vs. write
      |  docs/pi-protocol.md frames)                     |  (docs/pi-protocol.md §9)
      v                                                  v
mpp-ipc-worker                   (Rust binary, built by `cargo build -p mpp-ipc-worker`)
      |  write: encodes BigTIFF          |  read: decodes TIFF/BigTIFF (mpp-core's
      |                                  |  streaming reader) into native bands
      v                                  v
output.btf                        pixels back in PixInsight
```

The module is the *only* PCL-linked code in this tree. `host/` builds and
tests (via its own CTest golden suite) with **no PixInsight installed** —
it never includes a PCL header — because it only knows about shared memory,
pipes, and the wire protocol; PCL awareness lives entirely in `module/`.
The worker is a separate Rust process, found by the module at runtime next
to its own binary (via `dladdr` on POSIX / `GetModuleFileNameW` on Windows,
the same pattern MergeMosaic's `mmm-pxm` module uses), not linked into the
`.so` at all. `mpp::WriteSession` and `mpp::ReadSession` (`host/mpp_host.h`)
are the two host-side session types `BigTiffInstance` drives, one per
direction; both speak the same protocol v3 wire format
(`docs/pi-protocol.md`), just with slot ownership inverted for reads (§9 of
that document). v3 widened `Init` only — fifteen new, defaulted fields
(§10) — leaving the frame layout and lifecycle exactly as v2 left them.

Supported PixInsight versions: **1.9.0 and newer**. The module's declared
core-API floor is baked in from the PCL revision it links (see
`ci/pcl-pin.env`'s compatibility policy): all x64 targets build against
PCL 2.8.3 / API 0x0182, which every 1.9.x core accepts; the macOS arm64
build serves 1.9.4+ (the first arm64-capable core) from its own PCL 2.10.4
pin, and 1.9.0–1.9.3 macOS users (x64 cores, Rosetta on Apple Silicon) are
served by the macos-x64 build.

Note the C++ standard split: the host library (`host/`) is gated as C++17 by
its own CMake build (`-Wall -Wextra -Werror`, zero warnings). The module
(`module/`) is compiled as **C++20**, not C++17, because PCL 2.9.x's
`Image.h` fails to compile under C++17 (`pcl::Complex<float>` clip/truncate
comparisons only resolve under C++20 overload rules — verified empirically
against the installed 1.9.4 SDK's 2.9.4 headers). The pinned 2.8.3 headers
do compile as C++17, but the macOS-arm64 leg builds from the 2.10.x arm64
pin, so the module stays C++20 uniformly rather than varying the standard
per pin. The module's makefile documents this; `host/`'s CMake build is
unaffected.

## Module and format naming

The module and the formats it provides are separate identities, and the code
keeps them separate so that adding a format is purely additive:

| Layer | Name | Where |
|---|---|---|
| Binary | `mpp-pxm.{so,dylib,dll}` | `module/makefile-x64` (`TARGET`), `module/CMakeLists.txt` (Windows) |
| Module (PCL `MetaModule`) | `MegaPixelProcessor` | `module/Mpp.cpp` (`MppModule`); version in `module/MppVersion.h` |
| Format (PCL `MetaFileFormat`) | `BigTIFF` | `module/BigTiffFormat.{h,cpp}` + `module/BigTiffInstance.{h,cpp}` |

Module-level code is named `Mpp*`; each **file format** is a sibling
`<Format>Format` / `<Format>Instance` pair named for the format itself, not
for the product. That matches PixInsight's own convention — a format id names
the container (`TIFF`, `XISF`, `FITS`) while the vendor identity lives in the
module and in each format's `Description()`. A second format is added by
writing a new pair and constructing it in `InstallPixInsightModule`
(`module/Mpp.cpp`) alongside `BigTiffFormat`; the binary name, module
identity, CI jobs, staging layout, and install procedure below are unaffected.

## Write: sample-format mapping

Since protocol v3 the mapping is the **identity**: the sample format chosen
in the options dialog (or, absent a dialog, taken from `ImageOptions`)
selects both the output file's `bit_depth` **and** `band_format`, the sample
type the pixel data itself takes across shared memory. Nothing is normalized
to `f32` on the way out any more, so `u32` and `f64` reach the encoder with
every bit intact.

| Chosen sample format | `bit_depth` | `band_format` | Notes |
|---|---|---|---|
| 8-bit unsigned | `"u8"` | `"u8"` | native, verbatim |
| 16-bit unsigned | `"u16"` | `"u16"` | native, verbatim |
| 32-bit unsigned | `"u32"` | `"u32"` | native, verbatim — before phase 4 this was advertised and then narrowed to f32 |
| 32-bit float | `"f32"` | `"f32"` | native, verbatim |
| 64-bit float | `"f64"` | `"f64"` | native, verbatim — likewise previously narrowed |

The `WriteSamples` overload the core happens to call need not match the
negotiated format (PCL does not specify which it picks), so each overload
converts its own buffer to `band_format`. A conversion that genuinely loses
information — an `f32` view saved as 16-bit by user choice, say — sets the
flag `WasLossyWrite()` reports. It is now a *per-conversion* answer rather
than the blanket "any `u32`/`f64` save is lossy" it used to be, and it is
reset per file.

`band_format` must equal `bit_depth` unless it is `"f32"`, which is the
converting path the CLI and older payloads use (`docs/pi-protocol.md`
§10.1). Complex and signed-integer sample formats are still rejected with a
`pcl::Error`. Channel counts 1–4 are accepted: grayscale, gray+alpha, RGB
and RGBA, with the alpha association carried in an `ExtraSamples` tag.

## Write: the TIFF options dialog

`BigTiffFormat::UsesFormatSpecificData()` is `true` and
`BigTiffInstance::QueryOptions()` presents `TiffOptionsDialog` — a PCL
`Dialog` subclass — before every save. It opens with the Astrometrical
notice block (chevron, *"Mega Pixel Processor — TIFF Options"*, tagline,
copyright and the astrometrical.com / Astrometrical Tools / Ko-fi links), so
it is visibly **not** the stock TIFF dialog. That is deliberate: once you
have swapped modules, the dialog is how you tell which one is loaded.

Cancelling returns `false` from `QueryOptions()`, which aborts the save
before any file is created.

Five `SectionBar` sections:

| Section | Controls | What it sets |
|---|---|---|
| **Sample Format** | Five radio buttons: 8-bit unsigned, 16-bit unsigned, 32-bit unsigned, 32-bit float, 64-bit float | `ImageOptions::bitsPerSample`/`ieeefpSampleFormat`, and through them both `bit_depth` and `band_format` |
| **Compression** | Method combo (None / LZW / Deflate (zlib) / PackBits (RLE)), Deflate level spin box (1–9), horizontal-predictor check box | Per-strip/per-tile compression. The predictor is integer-only and its check box disables for the two float formats — the engine rejects the combination too, so it cannot be smuggled past the UI |
| **Layout** | Container combo (*Automatic (classic TIFF when it fits)* / *Classic TIFF* / *BigTIFF*), strips-vs-tiles radio pair, tile-size combo (powers of two, 128–1024, default 256) | Container selection and the strip/tile choice. Not in the stock dialog; tiles exist for the Astrometrical large-image load path |
| **Embedded Data** | Embed-ICC-profile check box, alpha-association combo (*Not written* / *Unassociated (straight)* / *Associated (premultiplied)*) | Whether tag 34675 is written, and the `ExtraSamples` value. The alpha combo is **always** offered — `QueryOptions()` runs before `Create()`, so no geometry has reached the instance yet and there is no way to ask whether *this* image carries alpha. `CreateImage()` reconciles it: an image with no alpha channel forces `"none"` on the wire whatever was chosen, and an image that has one with *Not written* selected has the plane dropped |
| **Metadata** | Image Description, Copyright and Software edits; X and Y resolution numeric edits with a *Pixels per inch* / *Pixels per centimeter* unit combo | TIFF tags 270 / 33432 / 305, and `XResolution`/`YResolution`/`ResolutionUnit` |

The three text tags must be pure ASCII with no embedded NUL — TIFF's ASCII
tag type cannot encode anything else — and the engine rejects a violation
before it creates the output file, naming the field.

The dialog's TIFF-specific answers travel as a `TiffFormatOptions` block (a
POD struct with a magic word, a version, fixed-width fields and fixed-size
character arrays — no pointers, since PCL owns the block's lifetime across a
module boundary). Sample format and resolution ride in `ImageOptions`
instead, which already models them. If `SetFormatSpecificData()` never runs,
`CreateImage()` falls back to in-code defaults; there are **no global
preferences** (`CanEditPreferences()` is `false`), so every save asks.

### Container selection

`Automatic` writes a **classic** TIFF while the uncompressed payload fits
under 4 GiB (less a 16 MiB header/IFD allowance) and a BigTIFF above it.
Compression only ever shrinks the payload, so `Automatic` never
under-predicts.

Two consequences worth knowing:

- **A small `.tif` save is now a classic TIFF.** Before phase 4 every save
  this module made was a BigTIFF, including small ones wearing a `.tif`
  name, which a good deal of older software will not open.
- **`.btf` and `.tf8` still always get BigTIFF**, whatever the size. Those
  extensions *declare* the container rather than merely naming the family,
  so honouring them beats the size heuristic — a classic TIFF inside a
  `.btf` would be a file whose name lies about its contents. Choosing
  `Classic TIFF` explicitly for an image too large for it fails at option
  validation, before any pixels move, with an error naming the size.

## Read support

`BigTiffFormat::CanRead()` is `true`; `CanReadIncrementally()` is
`false` **explicitly** — the core drives an interactive Open through
whole-image `ReadImage`, not `ReadSamples`, so `BigTiffInstance::Open()`
returns one `ImageDescription` (`PCL_FILEFORMAT_API.md` §8.2) and the module
implements only the five `ReadImage` overloads (§8.3), never `ReadSamples`.

The decode itself is `mpp-core`'s streaming TIFF/BigTIFF reader
(`crates/mpp-core/src/formats/tiff_read.rs`), driven by `mpp-ipc-worker` in
**read mode** (`docs/pi-protocol.md` §9, unchanged from v2 to v3: the worker decodes a
band, fills the named shm slot, and announces it via `ReadBand`; the host
consumes it and sends `ReadBandAck` to free the slot — slot ownership
inverted from the write path). It is a general TIFF/BigTIFF importer, not
limited to files this module itself wrote:

- **Extensions**: the same four this module writes — `.tif`, `.tiff`,
  `.btf`, `.tf8`. Claiming `.tif`/`.tiff` means this module **replaces**
  PixInsight's stock TIFF module; see "Replacing the stock TIFF module".
- **Container**: classic TIFF or BigTIFF, stripped or tiled, the common
  compressions the underlying `tiff` crate decodes (e.g. none/LZW/
  Deflate/PackBits).
- **Colour/sample support**: grayscale or RGB, with or without an alpha
  channel (`ImageMeta.alpha`/`ImageMeta.channels`,
  `docs/pi-protocol.md` §9.2); native `u8`/`u16`/`u32`/`f32`/`f64` samples,
  decoded and handed to PixInsight at their **native** bit depth. Both
  directions are native since phase 4.
- **Palette (`ColorMap`) images**: expanded to RGB by looking the indices up
  in the file's own `ColorMap`. The result reports a `u16` sample format,
  because `ColorMap` entries are 16-bit — a palette file therefore opens as
  a `UInt16Image` whatever the width of its indices.
- **`YCbCr` images**: 8-bit, chunky, inverted to RGB by TIFF 6.0 §21's own
  transform, parameterised by the file's `YCbCrCoefficients` and
  `ReferenceBlackWhite` tags (defaulting to the spec's values when absent).
  A file whose coefficients cannot be honoured, or whose
  `ReferenceBlackWhite` gives a channel no coding range, is **refused**
  rather than approximated. Planar `YCbCr` is refused: the inverse needs all
  three components in one chunk.
- **Planar (`PlanarConfiguration = 2`) storage**: de-interleaved into the
  reader's chunky band model. A mechanical unwind; the samples themselves
  are untouched.
- **ICC profiles**: read back from tag 34675 and returned by
  `BigTiffInstance::ReadICCProfile()`, so a colour-managed file keeps its
  profile through an open. An unreadable ICC tag is an error, not a silent
  drop.
- **Rejected with a clear error** (never silently mis-decoded): CMYK and Lab
  colour, which need ICC colour management this project does not have — the
  error says so, rather than only naming the colour type; other multiband
  colour types; signed-integer samples; complex/void samples; any bit depth
  outside the five listed above.
- **Untested, and accepted anyway — JPEG-compressed subsampled `YCbCr`.**
  The reader validates the YCbCr transform's parameters but does **not**
  check `ChromaSubsampling`, so a subsampled file is accepted. No
  JPEG-in-TIFF fixture exists in this repo, and this is the most likely
  YCbCr TIFF a real user has, so treat such a file as unverified: it may
  decode correctly, mis-decode, or error out of the decoder. Tracked in
  `docs/tiff-parity.md`, "Remaining gaps".
- **Save-rename safety net** (write path, documented here since it
  directly affects what a subsequent read sees): `BigTiffInstance::Create()`
  rewrites any output path lacking a TIFF-family tail to `<path>.tiff` and
  emits a one-line `Console::WarningLn` notice (`PCL_FILEFORMAT_API.md`
  §8.4), so the module never leaves a save at a path it couldn't read back.
  With `.tif` leading the extension list the Save As dialog appends `.tif`
  and this no longer fires on a normal save; it remains for hand-typed and
  script-supplied paths.
- **`mpp-ipc-worker --probe <path>`**: a standalone CLI entry that opens a
  file and prints its `ImageMeta` JSON (`docs/pi-protocol.md` §9.2) with no
  shm/handshake involved — useful for checking whether a given file decodes
  (and at what geometry/bit depth) before driving a real session.

## Build

Dependencies:

- PCL public headers, `/opt/PixInsight/include` (override with
  `PCLINCDIR=...`).
- `libPCL-pxi.a`, built from the pinned PCL source — build it once with
  this repo's own script:
  `bash integration/pixinsight/ci/build-pcl.sh --out $HOME/.local/pcl-build-0182`
  (fetches `ci/pcl-pin.env`'s pinned PCL 2.8.3 and hard-gates its
  `PCL_API_Version`). Point `PCLINCDIR`/`PCLLIBDIR` at the resulting
  `include/` and `lib/` directories (defaults:
  `$HOME/.local/pcl-build-0182/{include,lib}`, matching the pin's 0x0182
  API version — see `ci/pcl-pin.env`'s compatibility policy; do **not**
  build against a locally installed PixInsight's bundled headers, whose
  newer `PCL_API_Version` would silently raise the module's core floor).
- A Rust toolchain (`cargo`) to build `mpp-ipc-worker`.

```sh
# 1. Build the worker the module will spawn at runtime.
cargo build --release -p mpp-ipc-worker

# 2. Build the module (Linux; see makefile-x64 for the macOS branch).
#    PCLINCDIR/PCLLIBDIR default to $HOME/.local/pcl-build-0182/include
#    and $HOME/.local/pcl-build-0182/lib; override if yours differ.
make -C integration/pixinsight/module -f makefile-x64
```

On Windows the module is built by `module/CMakeLists.txt` (MSVC, mirroring
how the real PCL modules build on vc17 — fully linked against
`PCL-pxi.lib` + the six vendored 3rdparty libs, no PixInsight install
needed), driven end-to-end by `ci/build-module-windows.ps1`:

```powershell
integration/pixinsight/ci/build-module-windows.ps1 `
  -PclInc <pcl-prefix>/include -PclLib <pcl-prefix>/lib -Stage stage
```

This produces `integration/pixinsight/module/mpp-pxm.so`. The host
library (`host/`) has its own build (`cmake -S integration/pixinsight/host
-B <build-dir> && cmake --build <build-dir>`) used only for its golden CTest
suite in this repo's regular gates — the module's makefile compiles the
host sources directly into the `.so` link, so a separate host-library
artifact is never installed.

Build gates enforced on this machine and re-checked at every commit that
touches this tree: zero `-Wall -Wextra` warnings; `nm -D -u -C
mpp-pxm.so | grep -c "mpp::"` is `0` (no undefined `mpp::` symbols — the
host lib linked in fully); `nm -D --defined-only -C mpp-pxm.so | grep -i
PixInsightModule` shows the three exported entry points
(`IdentifyPixInsightModule`, `InitializePixInsightModule`,
`InstallPixInsightModule`).

The module is **unsigned** by this build. PixInsight >= 1.9 refuses to load
unsigned modules; `make sign` (see the makefile) produces a local
`mpp-pxm.xsgn` from a locally-generated signing identity, sufficient for
loading on the machine that generated it. Distributing to other machines
needs a Pleiades Certified-Developer signing certificate — out of scope
here (see MergeMosaic's own module, which has the identical dormant
constraint).

## Install layout

PixInsight loads modules via **Process → Modules → Install Modules…**,
pointed at either a directory or a specific module file (the same mechanism
MergeMosaic's `mmm-pxm` module installs through — see
`~/dev/MergeMosaic/integration/pixinsight/module/README.md`, our own code).
Whichever directory you point it at, **the worker binary must sit next to
the module binary** — install both side by side:

```
<module-dir>/
  mpp-pxm.so          # or .dylib / .dll
  mpp-ipc-worker           # or mpp-ipc-worker.exe
```

If the worker is missing, `BigTiffInstance::Create()` throws a `pcl::Error`
naming the exact expected path and the `cargo build` command to produce it,
rather than failing silently or hanging.

## Replacing the stock TIFF module

This module's `BigTIFF` format claims `.tif`/`.tiff`, which PixInsight's
bundled TIFF module already owns. **The two cannot coexist**: the core
refuses to register a second format claiming an extension another format
owns, so `BigTIFF` will silently fail to appear unless the stock module is
uninstalled first.

**To swap:**

1. **Process → Modules → Manage Modules…**, select the stock TIFF module
   (`TIFF-pxm.so` in PixInsight's `bin/`), uninstall it, and restart
   PixInsight when prompted. Uninstall through the UI rather than deleting
   the file — the file is your way back.
2. **Process → Modules → Install Modules…**, point it at this module (see
   "Install layout"), restart.
3. Confirm the `BigTIFF` format now appears in File > Save As and File >
   Open with `.tif`/`.tiff` routed to it.

**To revert**, uninstall this module and reinstall `TIFF-pxm.so` from
PixInsight's `bin/` the same way. Both `TIFF-pxm.so` and its `TIFF-pxm.xsgn`
signature file must be present for the stock module to reinstall, so don't
delete either.

**If you install this module without uninstalling the stock one first**,
nothing breaks — the format just won't register. Uninstall the stock TIFF
module and restart, and it appears.

**What you give up while swapped.** Since phase 4 the honest list is short:

- **No CMYK or Lab input.** Both are refused with an error saying why —
  converting either to RGB faithfully needs ICC colour management this
  project does not have, and an unmanaged conversion would silently
  misrepresent your pixel values. Signed-integer, complex and void samples
  and arbitrary multiband colour are refused for their own reasons.
- **No FITS-keyword, thumbnail or image-property passthrough.**
  `CanStoreKeywords()` is `false`: TIFF has no FITS-keyword equivalent, and
  Image Description / Copyright / Software are three specific tags, not
  keywords. Nothing beyond geometry, pixels, the ICC profile, resolution and
  those three tags crosses the container in either direction.
- **No JPEG- or Fax-compressed output.** Lossy compression is wrong for
  linear astro data. (Reading such a file may work — see "Read support"'s
  note on subsampled `YCbCr`, which is accepted but untested.)
- **No global format preferences.** Every save presents the options dialog;
  there are no sticky defaults.

Everything else the stock dialog offered is here, and tiled output, PackBits
and an explicit container choice are additions on top.
`docs/tiff-parity.md` records the full before/after and what remains.

## Manual smoke procedure

Loading the module inside PixInsight and driving it from the GUI is **not**
automated anywhere in this repo (out of scope per the implementation plan —
the machine-verifiable boundary is the build/link/symbol gates above plus
the host library's golden `ctest` suite, which proves the wire protocol and
process lifecycle against the real worker with no PixInsight involved). This
is the human's manual check:

1. Build the module and worker as above and place them side by side per
   "Install layout".
2. Sign the module locally (`make sign XSSK_FILE=... XSSK_PASSWORD=...`,
   see the makefile) — PixInsight >= 1.9 refuses to install an unsigned
   module ("Required module signature not found"); a local signing identity
   (no Pleiades certificate needed) is generated once via PixInsight's
   bundled SigningKeys script, per MergeMosaic's documented procedure
   (`~/dev/MergeMosaic/integration/pixinsight/module/README.md`, "Code
   signing").
3. Uninstall the stock TIFF module first (see "Replacing the stock TIFF
   module") — without this the `BigTIFF` format will not register at all.
   Then in PixInsight: **Process → Modules → Install Modules…**, point it at
   the module directory (or `mpp-pxm.so` directly), restart if prompted.
   Confirm "MegaPixelProcessor" appears under Process > Modules with the
   description "Mega Pixel Processor: file format support for
   astronomically large images", and that its "BigTIFF" format appears
   under File > Save As > format list (and under File > Open's format
   filter) described as "Mega Pixel Processor: BigTIFF/TIFF (read+write)".
4. **Write smoke**: open any image (any bit depth, grayscale or RGB), Save
   As, choose the BigTIFF format — the dialog appends `.tif`, and the TIFF
   options dialog opens before the file is written (exercised properly in
   "TIFF options" below; accept the defaults for now). Check the format also
   appears for a *non-file-backed* window (duplicate a view, or use any
   process that creates one) and for a window loaded from a TIFF: both were
   the cases the old compound extensions broke (see
   `docs/tiff-parity.md`).
5. Verify the saved file: open it in Photoshop (PS >= 26.2 opens BigTIFF)
   or round-trip it through `mpp`'s own reader (`target/release/mpp convert
   output.btf check.png` and inspect visually), and confirm the file size
   and pixel content match the source image.
6. Try a save that would exceed 4 GB uncompressed (a large mosaic) and
   confirm it completes — this is the whole point of the module over
   PixInsight's built-in TIFF writer.
7. **Read smoke**: in PixInsight, File > Open a `.tif` (or
   `.tiff`/`.btf`/`.tf8`) file — either one saved by this module in step
   4, or an independently-produced TIFF/BigTIFF in the supported subset
   (see "Read support"). Confirm it opens with the correct geometry and
   colour space, and visually verify pixel content against a known-good
   render of the same file (e.g. `mpp convert <file> check.png`).
8. Type an output path with no TIFF-family extension and confirm
   PixInsight's Console shows the rename warning and the file lands at
   `<name>.tiff` — then confirm that renamed file re-opens via step 7's
   procedure.

### TIFF options (phase 4)

**Read this before running it.** The options dialog and its wiring into
PixInsight have **no automated test anywhere in this repo**, by the standing
verification boundary above — no gate here touches a PCL widget. This
procedure is the only coverage they will ever get, so it is not a checklist
of the obvious. Steps marked **[REGRESSION]** would have *failed* before
phase 4; steps marked **[ASSUMPTION]** probe something the PCL headers
describe but do not normatively guarantee, where being wrong is expensive.
Run all eleven, in order, and treat a skipped step as an untested claim.

You will need `xxd` (or any hex dump), and `tiffinfo` (libtiff-tools) or
`exiftool` for the tag checks. `target/release/mpp-ipc-worker --probe <file>`
prints the geometry and sample format of any file this module can read, and
is the quickest independent second opinion.

1. **The dialog is ours.** Save any image as TIFF. Confirm the options
   dialog appears before the file is written, and that it is visibly *not*
   the stock TIFF dialog: the Astrometrical chevron, the title *"Mega Pixel
   Processor — TIFF Options"*, the tagline and copyright line, and three
   clickable links (astrometrical.com · Astrometrical Tools · Ko-fi) that
   show a pointing-hand cursor. Expand and collapse each of the five section
   bars and confirm the dialog grows and shrinks with them.
2. **Cancel aborts.** Press Cancel. Confirm **no file is written** (not an
   empty one, not a truncated one — check the target directory) and that
   PixInsight reports the save as cancelled rather than failed.
3. **[REGRESSION] Every sample format survives.** Save the same image five
   times, once per Sample Format radio button: 8-bit unsigned, 16-bit
   unsigned, 32-bit unsigned, 32-bit float, 64-bit float. Reopen each in
   PixInsight and confirm the sample format survived — the 32-bit-integer
   save must reopen as a `UInt32Image` and the 64-bit float save as a
   `DImage`. **These two are the point of the whole phase**: before it, both
   capabilities were advertised and then silently narrowed to `f32`, so a
   `u32` save came back with 8 bits of precision quietly gone. If either
   reopens as a `pcl::Image`, protocol v3's native band path is broken.
   (Expect `u8`/`u16` values to differ from a pre-phase-4 build by at most
   1 LSB: integer scaling now goes through `f64` and is strictly more
   accurate.)
4. **[REGRESSION] Compression works and is independent per block.** Save the
   same image with LZW, with Deflate (try levels 1 and 9), and with
   PackBits. Confirm each reopens with identical pixels, and that each
   compressed file is smaller than the uncompressed one. Then repeat one of
   them on an *integer* image with the horizontal predictor ticked, and
   confirm the predictor check box is **disabled** when a float sample
   format is selected. Before phase 4 output was always uncompressed.
   (Note `BigTiffFormat::SupportsCompression()` still returns the PCL
   default `false` even though the writer compresses — a known
   understatement, tracked as a code fix, not a docs one.)
5. **[REGRESSION] Automatic picks classic TIFF for a small image.** Save a
   small image (a few MB) with the container combo on Automatic to a `.tif`
   path. Confirm the first four bytes are the *classic* magic:
   `xxd -l 4 out.tif` shows `4949 2a00` (`II*\0`), **not** `4949 2b00`
   (`II+\0`, BigTIFF). Then save the same image to a `.btf` path, still on
   Automatic, and confirm it *is* BigTIFF (`4949 2b00`) — the extension
   declares the container and overrides the size heuristic. Before phase 4
   every save was BigTIFF regardless, which is exactly the behaviour change
   most likely to surprise an existing user.
6. **[REGRESSION] Tiles.** Save with Layout set to Tiles (try 256 and one
   other tile size). Confirm the file reopens in PixInsight with correct
   geometry and pixels, and that `tiffinfo out.tif` reports
   `TileWidth`/`TileLength` (and tile offsets/byte counts) rather than
   `RowsPerStrip`. Use an image whose width and height are **not** multiples
   of the tile size, so the padded right-column, bottom-row and corner tiles
   are exercised.
7. **[REGRESSION] Alpha round trip.** Open or construct an image with an
   alpha channel (RGBA or gray+alpha), save it with the alpha combo set to
   Unassociated, and reopen. Confirm the alpha channel is present and
   correct, and that `tiffinfo` shows an `ExtraSamples` entry. Repeat with
   Associated. Alpha was refused outright before phase 4.
8. **[ASSUMPTION] Alpha set to "Not written" — check the colour channels,
   not just the count.** Save an RGBA image with the alpha combo on *Not
   written*, then **reopen it and compare the R, G and B planes against the
   source**. The drop path opens the session for the colour channels alone
   and discards the `WriteSamples` call for the alpha index, which assumes
   PCL hands colour channels before alpha. If that assumption is ever false,
   this does not error — the wire accepts the geometry either way — it
   writes the **alpha plane into the red channel** and silently corrupts the
   image. A file that merely "opens as RGB with no alpha" is not a pass;
   the pixels must match.
9. **[ASSUMPTION] Save from a colour-managed window.** Take a window that
   carries an ICC profile (assign one via PixInsight's colour management if
   you must), save it with *Embed ICC profile* ticked, and confirm the save
   **completes** and that `exiftool out.tif` (or `tiffinfo`) shows an ICC
   profile (tag 34675). Then reopen the file in PixInsight and confirm the
   profile comes back. This is the single highest-consequence unverified
   assumption in the phase: `WriteICCProfile()` **throws** if PCL calls it
   after `CreateImage()`, and while the PCL header's prose supports the
   assumed before-`CreateImage()` order, that prose is descriptive, not a
   normative guarantee. If any core path calls it later, **every**
   ICC-bearing save fails hard rather than degrading to a profile-less
   file — so exercise this from more than one route if you can (Save,
   Save As, and a script-driven `FileFormat` write).
10. **[REGRESSION] Text tags and resolution.** Fill in Image Description,
    Copyright and Software, set the resolution to something distinctive
    (say 300, unit inch), and save. Confirm with `exiftool out.tif` or
    `tiffinfo out.tif` that tags 270, 33432 and 305 carry your text and that
    `XResolution`/`YResolution`/`ResolutionUnit` carry the value. Note that
    resolution tags are now written on *every* save — PCL's `ImageOptions`
    defaults to 72 dpi, so even a save where you touch nothing gets them,
    where before phase 4 none were written at all. Also confirm a non-ASCII
    character in one of the text fields is refused with a message naming the
    field, **before** any output file appears.
11. **Photoshop round trip.** Open a tiled, Deflate-compressed **BigTIFF**
    written by this module in Photoshop (PS >= 26.2) and confirm it renders
    correctly at full size. This is the check that a hand-rolled IFD is
    correct by more than our own decoder's opinion — the failure mode this
    guards is a file `mpp` reads back happily and every other reader
    rejects. Repeat with a tiled *classic* TIFF written to a `.tif` path,
    which is the newly common case.

Also worth a pass while you are here: confirm `WasLossyWrite()` behaves —
saving a 32-bit-float view as 16-bit unsigned should report a lossy write
(it did not before phase 4), while saving that same view as 32-bit float,
or a `UInt32Image` as 32-bit unsigned, should not.

### Known v1 limitation: abort wiring

PixInsight's in-GUI abort mechanism is **not** wired to the module in v1
(the wire protocol supports a pre-`Finish` `Cancel`, per
`docs/pi-protocol.md` §7.4, but nothing in this module's write path calls
it in response to a user-initiated abort). Practical consequence during the
manual smoke: if you close/destroy the image window mid-save (or the module
instance is otherwise torn down) while a `WriteSession` is still open,
`BigTiffInstance::Close()` sends `Cancel` to the worker and **blocks until
the worker replies** — a legitimate teardown finishes quickly, but a wedged
or unresponsive worker process will make `Close()` hang. Only the raw
*destructor* path (an instance destroyed without `Close()` ever running,
e.g. via an unwinding exception) is non-blocking: it kills and reaps the
worker process directly (`~WriteSession`) without waiting for a reply. If a
save appears to hang on cancel during manual testing, this is why — kill
the PixInsight process or the worker process directly to recover. Wiring a
true non-blocking abort is deferred to a later phase.

## Clean-room provenance statement

This module (`module/`) and its supporting host library (`host/`) were
designed and implemented **exclusively** from:

- The public PCL headers under `/opt/PixInsight/include/pcl/` (PCL 2.9.4):
  `MetaFileFormat.h`, `FileFormatBase.h`, `FileFormatImplementation.h`,
  `FileFormatInstance.h`, `MetaModule.h`, `MetaObject.h`,
  `ImageDescription.h`, `ImageInfo.h`, `ImageOptions.h`, `ColorSpace.h`,
  `Exception.h`, `Defs.h`, `Image.h`, `PixelTraits.h`, `String.h`,
  `StringList.h`, `File.h`, `AbstractImage.h`, `ImageGeometry.h`,
  `Rectangle.h`, `Array.h` — the full list, with exact `file:line`
  citations for every quoted/paraphrased declaration, is
  `PCL_FILEFORMAT_API.md` in this directory.
- `docs/pi-protocol.md` (this project's own wire-protocol contract).
- MergeMosaic's own PixInsight integration code
  (`~/dev/MergeMosaic/integration/pixinsight/`) — our code, from a sibling
  Astrometrical project, reused freely for the module entry-point pattern,
  the `dladdr`-based worker-resolution pattern, and the makefile/CMake
  structure.
- The public TIFF 6.0 and BigTIFF specifications, and libvips/libtiff
  documentation (consumed by the Rust worker this module drives, not by the
  C++ code in this directory).

**No PixInsight Class Library module implementation source was ever read,
opened, quoted, or disassembled** — not the PCL TIFF module, not any other
`*-pxm.so`'s source, and not any compiled module binary. This is the
founding decision recorded in `docs/DESIGN.md` under "Licensing
constraints": PCL License 2.0.1 (29 December 2025) added AI-use
restrictions ("expressly includes use via APIs, wrappers, or third-party
tools" — PCL License 2.0.1, the paragraph following restrictions (a)-(c)),
so this
module is clean-room by construction — designed from the BigTIFF spec,
libtiff/vips documentation, and the public PCL API only, the same
link-against-the-API posture MergeMosaic's own `mmm-pxm` module already
uses. Every implementer's report for each task of the implementation plan
records the exact sources consulted; `PCL_FILEFORMAT_API.md`'s own header
carries the same scoping statement for that document specifically.

## Licensing scoping

This repository is Apache-2.0 (see `/LICENSE`, `/NOTICE`). Building and
distributing `module/`'s build products introduces one exception, scoped
narrowly to that directory:

- **`integration/pixinsight/module/` (`mpp-pxm.so` and its build
  products only)** links against PCL (`libPCL-pxi.a`) and PCL's public
  headers. Distributing that binary is subject to the **PixInsight Class
  Library License, Version 2.0.1** — retained notices, the required
  end-user acknowledgment sentence, and the no-endorsement clause are
  reproduced in `/NOTICE` (root of this repository), and the full license
  text is shipped byte-verbatim in this directory as
  [`LICENSE-PCL`](LICENSE-PCL) (copied from
  `/opt/PixInsight/etc/legal/licenses/PCL-License-2.0`; PCL License 2.0.1
  condition 2 requires the conditions/restriction/disclaimer to be
  reproduced in the documentation accompanying a binary redistribution, so
  this repository ships the text rather than only pointing at it). Any
  distributed package containing `mpp-pxm.so` should include
  `LICENSE-PCL` alongside it (the Task 6 CI staging step does this). The
  same text is also published at
  https://pixinsight.com/license/PCL-License-2.0.html and ships with every
  PixInsight/PCL installation at
  `/opt/PixInsight/etc/legal/licenses/PCL-License-2.0`, listed here as
  additional pointers, not as a substitute for the in-repo copy.
- **`integration/pixinsight/host/` and everything else** — the host static
  library, `mpp-ipc-worker`, and the rest of this project — remain
  Apache-2.0. `host/` never includes a PCL header (enforced by the file
  comment in `mpp_host.h` and by its own CTest suite building/passing
  without PixInsight installed at all).
- **`integration/pixinsight/host/third_party/json.hpp`** vendors
  [nlohmann/json](https://github.com/nlohmann/json) 3.11.3, MIT-licensed
  (Copyright (c) 2013-2023 Niels Lohmann); see `/NOTICE` for the entry.

## Out of scope (see the implementation plan for the full list)

Deferred to a later phase (not implemented by this module today):

- **WCS sidecar readback.** Reading a `.wcs.json` sidecar (or any other WCS
  source) back into PixInsight FITS keywords on Open — the WCS sidecar
  remains `mpp`'s own JSON mechanism, not read by this module.
- **Incremental read (`ReadSamples`).** The core always decodes through
  whole-image `ReadImage`; `mpp-core`'s reader is itself streaming/chunked
  internally, but the module never exposes that as PCL incremental read.
- **General TIFF-tag → FITS-keyword mapping, thumbnails, and image
  properties.** ICC profiles are written and read back as of phase 4, but
  nothing else beyond geometry, pixels, resolution and the three text tags
  crosses the container.
- **CMYK / Lab / other multiband colour, and signed-integer or
  complex-sample inputs.** Rejected with a clear `pcl::Error` rather than
  mis-decoded (see "Read support"). Palette, `YCbCr` and planar storage are
  **no longer** on this list — all three are accepted since phase 4.
- **Global format preferences** (`CanEditPreferences()` is `false`). One
  per-save dialog, defaults in code; no sticky settings.
- **JPEG- and Fax-compressed output.** Read support comes free from the
  decoder where it works; writing lossy compression is wrong for linear
  astro data and is not parity with the stock dialog either. Note that
  JPEG-compressed subsampled `YCbCr` on *read* is currently accepted and
  untested — see "Read support".
- Non-blocking abort wiring (see "Known v1 limitation" above) — write path
  only; there is no read-side cancellation concern since a read session has
  no equivalent in-progress-file state to abandon.
- Code signing and update-repository publishing: these live in the
  `astrometrical-mmm-site` sibling project (which builds the Astrometrical
  tools website and hosts the PixInsight update repositories). This repo's
  CI stages the unsigned per-platform artifacts that pipeline consumes;
  the module is distributed through an Astrometrical update repository.
