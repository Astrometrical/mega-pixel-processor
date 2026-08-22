# mpp PixInsight integration: the BigTIFF file-format module

This directory contains a **write-only** PixInsight Class Library (PCL) file
format module, `BigTIFF-pxm.so` (`.dylib` on macOS, `.dll` on Windows), that
lets PixInsight save images directly as BigTIFF (`.btf`, `.tf8`) — past
classic TIFF's 4 GB limit — without going through `mpp`'s CLI. It exists so
PixInsight users get the same BigTIFF export `mpp convert` already provides,
from inside PixInsight's own Save/Save As dialogs.

It is **not** a general-purpose TIFF reader/writer: no import support, no
format-options dialog, no ICC/thumbnail/property passthrough in v1. See
"Out of scope" below and `docs/DESIGN.md`'s phase-2 entry for what's
deferred to a later phase.

## Architecture

```
PixInsight core
      |  MetaFileFormat / FileFormatImplementation calls
      v
integration/pixinsight/module/   (BigTIFF-pxm.so — PCL-linked shim, C++20)
  BigTiffFormat   -- format identity/capabilities (MetaFileFormat)
  BigTiffInstance -- per-file write session (FileFormatImplementation)
      |  normalizes every WriteSamples buffer to f32 in [0,1],
      |  splits it into <= band_rows chunks
      v
integration/pixinsight/host/     (libmpp_host — PCL-free static lib, C++17)
  mpp::WriteSession  -- shm segment + child-process lifecycle + wire framing
      |  POSIX shm + pipes (mpp_shm/mpp_os) carrying docs/pi-protocol.md frames
      v
mpp-ipc-worker                   (Rust binary, built by `cargo build -p mpp-ipc-worker`)
      |  decodes bands, encodes BigTIFF
      v
output.btf
```

The module is the *only* PCL-linked code in this tree. `host/` builds and
tests (via its own CTest golden suite) with **no PixInsight installed** —
it never includes a PCL header — because it only knows about shared memory,
pipes, and the wire protocol; PCL awareness lives entirely in `module/`.
The worker is a separate Rust process, found by the module at runtime next
to its own binary (via `dladdr` on POSIX / `GetModuleFileNameW` on Windows,
the same pattern MergeMosaic's `mmm-pxm` module uses), not linked into the
`.so` at all.

Note the C++ standard split: the host library (`host/`) is gated as C++17 by
its own CMake build (`-Wall -Wextra -Werror`, zero warnings). The module
(`module/`) is compiled as **C++20**, not C++17, purely because PCL 2.10.4's
own `Image.h` fails to compile under C++17 (`pcl::Complex<float>`
clip/truncate comparisons only resolve under C++20 overload rules — verified
empirically building this module). The module's makefile documents this;
`host/`'s CMake build is unaffected.

## Sample-format mapping

Every `WriteSamples` buffer is normalized to `f32` in `[0, 1]` before it is
pushed as a band, independently of what output sample format was negotiated
via `SetOptions`/`CreateImage`:

| PixInsight sample type | `InitMessage.bit_depth` sent to the worker | Notes |
|---|---|---|
| `pcl::Image` (`float`, f32) | `"f32"` | native path, zero-copy pass-through |
| `pcl::DImage` (`double`, f64) | `"f32"` | narrowed; `WasLossyWrite()` reports `true` |
| `UInt8Image` (`uint8`) | `"u8"` | exact — `u8` fits f32's significand |
| `UInt16Image` (`uint16`) | `"u16"` | exact — `u16` fits f32's significand |
| `UInt32Image` (`uint32`) | `"f32"` | `v / (2^32 - 1)` into `[0,1]`; lossy — `u32`'s precision exceeds f32's 24-bit significand, reported via `WasLossyWrite()` |

Complex and signed-integer sample formats are rejected with a `pcl::Error`.
Only grayscale (1 channel) and RGB (3 channels) images are supported; alpha
channels cannot be stored (`CanStoreAlphaChannels() == false`) because the
wire protocol (`docs/pi-protocol.md` §5) only accepts `channels == 1` or `3`.

## Build

Dependencies:

- PCL public headers, `/opt/PixInsight/include` (override with
  `PCLINCDIR=...`).
- `libPCL-pxi.a`, built from PCL source. This repo does not build PCL
  itself — build it once following MergeMosaic's `ci/build-pcl.sh` pattern
  (`~/dev/MergeMosaic/integration/pixinsight/ci/build-pcl.sh`; this repo's
  own copy of that script lands in Task 6 of the implementation plan).
  Point `PCLLIBDIR` at the resulting `lib/` directory (default
  `$HOME/.local/pcl-build/lib`, which is where the script above places it).
- A Rust toolchain (`cargo`) to build `mpp-ipc-worker`.

```sh
# 1. Build the worker the module will spawn at runtime.
cargo build --release -p mpp-ipc-worker

# 2. Build the module (Linux; see makefile-x64 for the macOS branch).
#    PCLINCDIR/PCLLIBDIR default to /opt/PixInsight/include and
#    $HOME/.local/pcl-build/lib respectively; override if yours differ.
make -C integration/pixinsight/module -f makefile-x64
```

This produces `integration/pixinsight/module/BigTIFF-pxm.so`. The host
library (`host/`) has its own build (`cmake -S integration/pixinsight/host
-B <build-dir> && cmake --build <build-dir>`) used only for its golden CTest
suite in this repo's regular gates — the module's makefile compiles the
host sources directly into the `.so` link, so a separate host-library
artifact is never installed.

Build gates enforced on this machine and re-checked at every commit that
touches this tree: zero `-Wall -Wextra` warnings; `nm -D -u -C
BigTIFF-pxm.so | grep -c "mpp::"` is `0` (no undefined `mpp::` symbols — the
host lib linked in fully); `nm -D --defined-only -C BigTIFF-pxm.so | grep -i
PixInsightModule` shows the three exported entry points
(`IdentifyPixInsightModule`, `InitializePixInsightModule`,
`InstallPixInsightModule`).

The module is **unsigned** by this build. PixInsight >= 1.9 refuses to load
unsigned modules; `make sign` (see the makefile) produces a local
`BigTIFF-pxm.xsgn` from a locally-generated signing identity, sufficient for
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
  BigTIFF-pxm.so          # or .dylib / .dll
  mpp-ipc-worker           # or mpp-ipc-worker.exe
```

If the worker is missing, `BigTiffInstance::Create()` throws a `pcl::Error`
naming the exact expected path and the `cargo build` command to produce it,
rather than failing silently or hanging.

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
3. In PixInsight: **Process → Modules → Install Modules…**, point it at the
   module directory (or `BigTIFF-pxm.so` directly), restart if prompted.
   Confirm "BigTIFF" appears under File > Save As > format list, with the
   description "BigTIFF (write-only, Astrometrical Mega Pixel Processor)".
4. Open any image (any bit depth, grayscale or RGB), Save As, choose the
   BigTIFF format, extension `.btf` or `.tf8`.
5. Verify the saved file: open it in Photoshop (PS >= 26.2 opens BigTIFF)
   or round-trip it through `mpp`'s own reader (`target/release/mpp convert
   output.btf check.png` and inspect visually), and confirm the file size
   and pixel content match the source image.
6. Try a save that would exceed 4 GB uncompressed (a large mosaic) and
   confirm it completes — this is the whole point of the module over
   PixInsight's built-in TIFF writer.

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

- The public PCL headers under `/opt/PixInsight/include/pcl/` (PCL 2.10.4):
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

- **`integration/pixinsight/module/` (`BigTIFF-pxm.so` and its build
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
  distributed package containing `BigTIFF-pxm.so` should include
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

- Reading `.btf`/`.tf8` files back into PixInsight.
- A format-options dialog (no options exposed in v1).
- FITS-keyword / ICC-profile / XISF-property passthrough into the BigTIFF
  container (phase-3 candidates; the WCS sidecar remains `mpp`'s own JSON
  mechanism, not carried into this module).
- Non-blocking abort wiring (see "Known v1 limitation" above).
- Code signing for distribution to other machines, and the Astrometrical
  update repository (dormant in MergeMosaic too, pending Certified-Developer
  certification).
