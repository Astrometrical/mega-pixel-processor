# PCL File-Format API Reference — for the mpp BigTIFF module (clean-room)

## License scoping and provenance (read this first)

This document **quotes and closely paraphrases declarations and documentation
comments from the public headers of the PixInsight Class Library (PCL),
version 2.9.4** (header banner: "PCL 2.9.4", files released
2025-03-31T15:40:28Z — the SDK bundled with the installed PixInsight 1.9.4).
Those headers are:

**Copyright (c) 2003-2026 Pleiades Astrophoto S.L. All Rights Reserved.**

They are licensed under the **PixInsight Class Library License, Version 2.0.1
(29 December 2025)** — text at `~/dev/pixinsight-test/PCL/LICENSE` (which
carries the 2.0.1 version banner) and
`/opt/PixInsight/etc/legal/licenses/PCL-License-2.0`, also published at
https://pixinsight.com/license/PCL-License-2.0.html. All PCL header excerpts
below remain under that license and that copyright; they are reproduced here
solely to document the public API surface this module links against.

Exact header files quoted/paraphrased (all under `/opt/PixInsight/include/pcl/`):

- `MetaFileFormat.h`, `FileFormatBase.h`, `FileFormatImplementation.h`,
  `FileFormatInstance.h`, `MetaModule.h`, `MetaObject.h`,
  `ImageDescription.h`, `ImageInfo.h`, `ImageOptions.h`, `ColorSpace.h`,
  `Exception.h`, `Defs.h`, `Image.h`, `PixelTraits.h`, `String.h`,
  `StringList.h`

**Everything else in this document** (commentary, tables, mappings to the mpp
behavioral contract, unknowns analysis) is original work of the mpp project and
is licensed **Apache-2.0** like the rest of this repository (see `/LICENSE`).

**Clean-room statement:** this document was produced exclusively from the
public PCL headers listed above, the PCL license files, MergeMosaic's own
integration code (`~/dev/MergeMosaic/integration/pixinsight/module/mmm.cpp`,
`makefile-x64`, and its `PCL_API_REFERENCE.md` — our code), and the mpp repo
itself. **No PCL module implementation source (e.g. the PCL TIFF module) was
read, opened, quoted, or disassembled.** Where MMM's older reference cited
`/opt/PixInsight/src/pcl/*` (PCL library sources), this document deliberately
did NOT re-read those files; facts below marked "(MMM ref)" are carried from
MMM's already-written reference document, not re-derived from PCL sources.

All `file:line` citations below refer to `/opt/PixInsight/include/pcl/<file>`
at PCL 2.9.4 unless another path is given.

> **Build-pin caveat:** CI builds the module against PCL **2.8.3**
> (PixInsight 1.9.0), pinned in `ci/pcl-pin.env`, so the module's declared
> `PCL_API_Version` (0x0182) admits every 1.9.x core — see the policy
> comment in the pin file. (Exception: the macos-arm64 leg builds against
> 2.10.4, the first arm64-capable PCL; every arm64 core is ≥ 1.9.4.) This
> document's file+line citations refer to the PCL 2.9.4 headers installed
> at `/opt/PixInsight/include`; the entry-point/ABI facts below are stable
> across that span — the module compiles and links warning-free against the
> pinned 2.8.3 headers using exactly this surface — but any *new* API must
> be verified against the pinned 2.8.3 headers before use in the module,
> since 2.8.3 is what most targets compile against.

---

## 1. Module entry-point contract

### 1.1 The three entry points (`MetaModule.h:895-1027`, doxygen group `module_entry_points`)

Entry points are **free functions with C linkage**, "nonstatic, publicly
visible global symbols" (`MetaModule.h:899-902`) — not members of `MetaModule`:

```cpp
// PMIDN — module identification (MetaModule.h:920). PCL implements this
// internally; you never write it (MetaModule.h:947-949: "Module developers
// using the standard PixInsight Class Library (PCL) distribution don't have
// to care about PMIDN, since it is already implemented internally by PCL.")
PCL_MODULE_EXPORT uint32 IdentifyPixInsightModule( api_module_description** description, int32 phase );

// PMINI — initialization / API handshake (MetaModule.h:967). Also implemented
// internally by PCL (MetaModule.h:986-989). The apiVersion parameter of the
// handshake lives here — a hand-written module using libPCL-pxi.a does no
// API-version "dance" of its own.
PCL_MODULE_EXPORT uint32 InitializePixInsightModule( api_handle hModule, function_resolver R, uint32 apiVersion, void* reserved );

// PMINS — installation (MetaModule.h:1007). THE ONE FUNCTION YOU WRITE.
PCL_MODULE_EXPORT int32 InstallPixInsightModule( int32 mode );
```

- PMIDN and PMINI are mandatory but supplied by PCL's static library
  (`libPCL-pxi.a`); PMINS is "optional, although usually necessary"
  (`MetaModule.h:905-907`) and "normally has to be defined by non-trivial
  modules in order to create and initialize the different objects and
  meta-objects required to implement their functionality"
  (`MetaModule.h:1017-1021`).
- PMINS returns 0 on success; anything else is a module-specific error code
  (`MetaModule.h:1014-1015`).
- `mode` is one of `pcl::InstallMode::{ FullInstall, QueryModuleInfo,
  VerifyModule }` (`MetaModule.h:720-728`). Create the full object tree only
  for `FullInstall`; the `MetaModule` singleton itself must be created in
  every mode (proven pattern: MMM's `mmm.cpp:81-115`, which loads and runs in
  PixInsight today).
- `PCL_MODULE_EXPORT` is defined at `Defs.h:380` (MSVC:
  `extern "C" PCL_EXPORT`), `Defs.h:391` (clang:
  `extern "C" __attribute__((visibility ("default")))`) and `Defs.h:393`
  (gcc: same + `externally_visible`). It forces the C symbol out of a
  `-fvisibility=hidden` build.

**Verification gate for Task 4:** `nm -D --defined-only BigTIFF-pxm.so` must
show `InstallPixInsightModule`, `IdentifyPixInsightModule`, and
`InitializePixInsightModule` as exported symbols (the latter two come from
linking `libPCL-pxi.a`).

### 1.2 `MetaModule` — what to override (`MetaModule.h:69-686`)

Pure virtual (MUST implement):

```cpp
virtual const char* Version() const = 0;   // MetaModule.h:163
virtual IsoString Name() const = 0;        // MetaModule.h:224 — unique valid-C-identifier module id
```

`Version()` must return `"PIXINSIGHT_MODULE_VERSION_" + "MM.mm.rr.bbbb.LLL[.<status>]"`
(`MetaModule.h:111-156`). Build it with `PCL_MODULE_VERSION( MM, mm, rr, bbbb, lan )`
(`MetaModule.h:807-813`) or `PCL_MODULE_VERSION_S(..., status)`
(`MetaModule.h:880-887`). Version numbers 0-99, build 1-9999, zero-padded;
language is a 3-letter ISO 639.2 code (`eng`).

Optional, defaults return empty/no-op — reimplement the identity ones:
`Description()` (`MetaModule.h:244`), `Company()` (259), `Author()` (271),
`Copyright()` (284), `TradeMarks()` (296), `OriginalFileName()` (306),
`GetReleaseDate(int&,int&,int&)` (329 — must be a *valid* date or the module
won't install, per 322-323), `OnLoad()` (468), `OnUnload()` (482).

Do not use: `UniqueId()` (deprecated, `MetaModule.h:99-102`).

### 1.3 Registration idiom

- `extern MetaModule* Module;` global in namespace `pcl`
  (`MetaModule.h:690`, documented at 53-61). Constructing your `MetaModule`
  subclass sets it (MMM ref: `src/pcl/MetaModule.cpp:58-68` throws
  `Error("MetaModule: Module redefinition not allowed")` on a second one).
  Construct it exactly once, first thing in PMINS (working example:
  `mmm.cpp:83`).
- `MetaObject( MetaObject* parent )` adds itself to the parent's children
  list (`MetaObject.h:66-71`); the whole module is a tree rooted at `Module`
  (`MetaModule.h:63-65`). Metaobjects are non-copyable, non-movable
  (`MetaObject.h:86-104`) and the parent destroys its children
  (`MetaObject.h:77-80`) — so `new` the singletons in PMINS and never delete
  them yourself.
- `MetaFileFormat()`'s default constructor (`MetaFileFormat.h:84`) takes no
  parent argument; by parallel with `MetaProcess` (confirmed for MetaProcess
  in MMM ref from `src/pcl/MetaProcess.cpp:47-48`: constructs with `Module`
  as parent), **constructing the `MetaFileFormat` subclass after the module
  object is what registers the format**. The header does not spell this out
  for `MetaFileFormat` — see Unknowns §7.1 — but `MetaModule.h:1017-1021`
  (PMINS creates "the different objects and meta-objects") and the
  MetaObject tree design make any other mechanism implausible.

Skeleton (pattern from `mmm.cpp:81-115`, adapted):

```cpp
PCL_MODULE_EXPORT pcl::int32 InstallPixInsightModule( pcl::int32 mode )
{
   new pcl::MppModule;                       // sets pcl::Module
   if ( mode == pcl::InstallMode::FullInstall )
      new pcl::BigTiffFormat;                // MetaFileFormat subclass, self-registers
   return 0;
}
```

---

## 2. `MetaFileFormat` — the format description class

`class PCL_CLASS MetaFileFormat : public MetaObject, public FileFormatBase`
(`MetaFileFormat.h:77`). Describes the format; does **no** file I/O
(`MetaFileFormat.h:54-57`). Copy construction/assignment deleted
(`MetaFileFormat.h:95,101`).

`FileFormatBase` (`FileFormatBase.h:63`) declares the whole query surface as
pure virtuals; `MetaFileFormat` supplies concrete defaults for everything
except the three below, so **only three members are pure virtual in
`MetaFileFormat`**:

```cpp
virtual IsoString Name() const override = 0;              // MetaFileFormat.h:105
virtual StringList FileExtensions() const override = 0;   // MetaFileFormat.h:109
virtual FileFormatImplementation* Create() const = 0;     // MetaFileFormat.h:845
```

- `Name()`: the format identifier — "unique, valid C identifiers. Examples:
  'FITS', 'TIFF', 'JPEG2000'" (`FileFormatBase.h:81-83`). Ours: `"BigTIFF"`.
- `FileExtensions()`: "a sequence of '.xxx...x' strings in priority order"
  (`FileFormatBase.h:89-91`). Ours: `{ ".btf", ".tf8" }`. Return type
  `StringList = Array<String>` (`StringList.h:47`).
- `Create()`: factory returning a **new** `FileFormatImplementation`
  instance for each file operation (`MetaFileFormat.h:835-845`).

### 2.1 Capability queries relevant to a write-only incremental format

Every row cites the `MetaFileFormat.h` default implementation (all are
`bool ...() const override` returning a literal). "Ours" is what the mpp
behavioral contract implies Task 4 should return.

| Member | Line | Default | Ours (write-only BigTIFF shim) |
|---|---|---|---|
| `CanRead()` | 428 | `true` | **override → `false`** |
| `CanWrite()` | 439 | `true` | `true` (default OK; explicit override recommended for clarity) |
| `CanReadIncrementally()` | 453 | `false` | `false` (default) |
| `CanWriteIncrementally()` | 467 | `false` | **override → `true`** — required, or `WriteSamples` is never called (`FileFormatImplementation.h:718-721`) |
| `CanStore8Bit()` | 478 | `true` | `true` (shim maps u8 → `"u8"`) |
| `CanStore16Bit()` | 489 | `true` | `true` (u16 → `"u16"`) |
| `CanStore32Bit()` | 500 | `false` | **override → `true`** if u32 saves are to be offered (shim converts u32 → f32; document the precision note) — else leave `false` and the u32 `WriteSamples` overload should never fire |
| `CanStore64Bit()` | 511 | `false` | `false` |
| `CanStoreFloat()` | 523 | `false` | **override → `true`** (f32 is our native path) |
| `CanStoreDouble()` | 535 | `false` | **override → `true`** if f64 saves are offered (converted to f32 — a *lossy narrowing*; see `WasLossyWrite`, §3.6 and Unknowns §7.6) — else `false` |
| `CanStoreComplex()` | 547 | `false` | `false` |
| `CanStoreDComplex()` | 559 | `false` | `false` |
| `CanStoreGrayscale()` | 570 | `true` | `true` |
| `CanStoreRGBColor()` | 581 | `true` | `true` |
| `CanStoreAlphaChannels()` | 592 | `true` | per contract: only if the worker/protocol accepts >3 channels; else **override → `false`** |
| `CanStoreResolution()` | 603 | `false` | `false` |
| `CanStoreKeywords()` | 614 | `false` | `false` (FITS-keyword sidecar is phase-3) |
| `CanStoreICCProfiles()` | 625 | `false` | `false` |
| `CanStoreThumbnails()` | 636 | `false` | `false` |
| `CanStoreProperties()` | 655 | `false` | `false` |
| `CanStoreImageProperties()` | 668 | `false` | `false` |
| `CanStoreRGBWS()` | 679 | `false` | `false` |
| `CanStoreDisplayFunctions()` | 690 | `false` | `false` |
| `CanStoreColorFilterArrays()` | 701 | `false` | `false` |
| `SupportsCompression()` | 720 | `false` | `true` **only** if the worker writes compressed pixel data — "The optional ZIP and LZW compressions used in TIFF are the exact kind of compressions that must cause this member function to return true" (`MetaFileFormat.h:714-716`); if the worker writes uncompressed BigTIFF in v1, leave `false` |
| `SupportsMultipleImages()` | 736 | `false` | `false` (then `SelectImage`/`SelectedImageIndex` are never called — `FileFormatImplementation.h:165-169`) |
| `SupportsViewProperties()` | 762 | `false` | `false` |
| `CanEditPreferences()` | 776 | `false` | `false` (no options dialog in v1; then `EditPreferences()` is never called — `MetaFileFormat.h:829-830`) |
| `UsesFormatSpecificData()` | 792 | `false` | `false` (then `SetFormatSpecificData` is never called — `FileFormatImplementation.h:563-566`) |
| `IsDeprecated()` | 807 | `false` | `false` |

Note on capability semantics: the wording throughout is "can **read/write**"
(e.g. `MetaFileFormat.h:472-474`). For a `CanRead() == false` format these
effectively advertise which sample formats the *write* path accepts. The
headers do not state how the core chooses which `WriteSamples` overload to
call (see Unknowns §7.5) — implement **all five** overloads regardless.

### 2.2 Descriptive members (defaults empty; reimplement)

```cpp
IsoStringList MimeTypes() const override;   // MetaFileFormat.h:128 — default empty; "image/tiff" applies to TIFF-family
uint32 Version() const override;            // MetaFileFormat.h:135 — format version as hex (0x100 = 1.0.0, FileFormatBase.h:109-116)
String Description() const override;        // MetaFileFormat.h:151 — one-liner, e.g. "BigTIFF (write-only, Astrometrical mpp)"
String Implementation() const override;     // MetaFileFormat.h:178 — shows in Format Explorer; may use Console text tags (168-171)
String Status() const override;             // MetaFileFormat.h:199 — normally empty; basic HTML, no console tags (192-194)
virtual IsoString IconImageSVG() const;     // MetaFileFormat.h:229 — SVG source, UTF-8; empty → default icon (219-222)
virtual String IconImageSVGFile() const;    // MetaFileFormat.h:296 — "@module_icons_dir/" prefix supported (259-289)
```

Per the plan, `Description()`/`Implementation()` identify Astrometrical/mpp
and point at the provenance README. XPM/file icon members
(`MetaFileFormat.h:327,353,386,409`) are deprecated since 1.8.8-6 — do not
implement.

### 2.3 Members implemented by the PCL library (do not override)

`Icon()`/`SmallIcon()` (`MetaFileFormat.h:416,420`),
`ValidateFormatSpecificData()`/`DisposeFormatSpecificData()` (814, 818),
`EditPreferences()` (832), private `PerformAPIDefinitions()` (849 — PCL
internal registration hook).

---

## 3. `FileFormatImplementation` — the write-path instance

`class PCL_CLASS FileFormatImplementation` (`FileFormatImplementation.h:65`).
One instance ≈ one file being read/written (`FileFormatImplementation.h:53-55`).
Non-copyable (`:90,96`).

### 3.1 Construction / identity / teardown

```cpp
FileFormatImplementation( const MetaFileFormat* format );  // :75 — pass your metaformat singleton
virtual ~FileFormatImplementation();                       // :80
const MetaFileFormat* Meta() const;                        // :103 — returns the protected `meta` member (:781)
virtual void Close();                                      // :111 — "Closes an image file (after Open() or Create())"
virtual bool IsOpen() const;                               // :116
virtual String FilePath() const;                           // :123 — empty until Open()/Create() set a path
```

`Close/IsOpen/FilePath` are virtual with library-side default bodies whose
behavior the headers do not document — override all three and track state
yourself (Unknowns §7.2). Your `Close()`/destructor must be safe to call at
any point in the lifecycle (partially written file, worker still running):
the RAII rule for the `mpp::WriteSession` member is that the destructor
kills+reaps the worker.

### 3.2 Error conventions

- "In the event of error, a reimplementation of this member function should
  throw a PCL exception. Thrown exceptions will be caught and processed by
  internal PCL code." — stated for `Open()` (`FileFormatImplementation.h:156-158`)
  and for `Create()` (`:518-520`). Same convention applies module-wide.
- Throw `pcl::Error( const String& message )` — `Exception.h:223` (class),
  `Exception.h:248` (constructor; optional `bool unformatted = false`).
  Root class `pcl::Exception` at `Exception.h:49`. `FatalError`
  (`Exception.h:297`) terminates — don't use it for worker failures.
- So: worker `Error{message}` frames, nonzero worker exits, and a missing
  worker binary all become `throw pcl::Error( ... )` with an actionable
  message (install advice naming the expected path, per the MMM
  `ResolveWorkerPath` pattern).

### 3.3 Write-path lifecycle (in the order the core drives it)

The core drives a save through `FileFormatInstance` (intermodule proxy); its
header documents the client-side sequence, which is the best public evidence
of call order (see also Unknowns §7.3):

1. **`QueryOptions`** — `virtual bool QueryOptions( Array<ImageOptions>& options,
   Array<void*>& formatOptions )` (`FileFormatImplementation.h:474`).
   Called "just before calling Create(). Typically ... as part of a File >
   Save As operation" (`:464-466`). Return `true` to continue, `false` to
   abort the save (`:442-444`). Default returns `true` with no dialog
   (`:471-472`) — **keep the default for v1 (no options dialog)**.
   Client-side mirror: `FileFormatInstance.h:700` ("A module should call
   this function just before calling Create()", `:694-695`).

2. **`Create`** — `virtual void Create( const String& filePath,
   int numberOfImages, const IsoString& hints )`
   (`FileFormatImplementation.h:522`).
   - `filePath` may be relative or even invalid — "A module must be prepared
     to receive relative file path specifications, and even invalid paths"
     (`:486-489`). Existing files are overwritten (`:482-484`).
   - `numberOfImages` "can be zero if an empty file is being created, and
     also less than zero if the number of images is unknown"; a
     single-image format "should throw an Error exception if this value is
     zero or not equal to one" (`:491-504`) — but the same doc warns other
     modules "might not behave so accurately", so accept `1` **and** negative
     (treat as 1), reject `0` and `>1`.
   - `hints`: space-separated tokens, free to ignore (`:506-516`). Ignore in v1.
   - This is where the shim resolves the worker path (`dladdr`) and stores
     `filePath`; the shm/worker spawn can be deferred to `CreateImage` when
     geometry is known.

3. **`SetId`** *(optional)* — `virtual void SetId( const IsoString& id )`
   (`FileFormatImplementation.h:531`) — default ignores; keep default.

4. **`SetOptions`** — `virtual void SetOptions( const ImageOptions& options )`
   (`FileFormatImplementation.h:542`), "for the next image that will be
   written or created" (`:537-538`). Default is a no-op — **override and
   store**: `options.bitsPerSample` (`ImageOptions.h:194`) +
   `options.ieeefpSampleFormat` (`:195`) select the `InitMessage.bit_depth`
   mapping (f32/u16/u8 per contract). Also relevant: `complexSample` (`:196`,
   reject true), `signedIntegers` (`:197`, reject true). See §5.2.
   Client-side mirror `FileFormatInstance.h:764`; note `FileFormatInstance.h:761-762`
   says a format "may ignore" these options — i.e. they are advisory, and the
   authoritative pixel geometry arrives in `CreateImage`.

5. **`CreateImage`** — `virtual void CreateImage( const ImageInfo& info )`
   (`FileFormatImplementation.h:678`). "Creates a new image with the
   geometry and color space as specified by an ImageInfo structure. The
   newly created image will be written by subsequent incremental write
   operations. The sample data type and other ... parameters have been
   specified by previous calls to SetOptions() and SetFormatSpecificData()"
   (`:667-673`). "**Must be reimplemented** by all derived classes
   supporting incremental write operations" (`:675-676`).
   This is where the shim has (width, height, channels) + stored options →
   create shm, spawn worker, send `Init`.

6. **`WriteSamples`** — the exact overload set
   (`FileFormatImplementation.h:725,733,741,749,757`):

   ```cpp
   virtual void WriteSamples( const pcl::Image::sample*  buffer, int startRow, int rowCount, int channel );  // :725  f32
   virtual void WriteSamples( const pcl::DImage::sample* buffer, int startRow, int rowCount, int channel );  // :733  f64
   virtual void WriteSamples( const UInt8Image::sample*  buffer, int startRow, int rowCount, int channel );  // :741  u8
   virtual void WriteSamples( const UInt16Image::sample* buffer, int startRow, int rowCount, int channel );  // :749  u16
   virtual void WriteSamples( const UInt32Image::sample* buffer, int startRow, int rowCount, int channel );  // :757  u32
   ```

   Sample-type ground truth: `Image = FImage` (`Image.h:18467`),
   `FImage = GenericImage<FloatPixelTraits>` (`Image.h:18398`) whose
   `sample` is `float` (`PixelTraits.h:336` — `GenericPixelTraits<float>` —
   with `using sample = S` at `PixelTraits.h:151`). Likewise
   `DImage::sample = double` (`Image.h:18407`, `PixelTraits.h:930`),
   `UInt8Image::sample = uint8` (`Image.h:18437`, `PixelTraits.h:3037`),
   `UInt16Image::sample = uint16` (`Image.h:18447`, `PixelTraits.h:3901`),
   `UInt32Image::sample = uint32` (`Image.h:18457`, `PixelTraits.h:4765`).

   Parameters (documented at `:707-713`): `buffer` = source samples,
   `startRow` = first pixel row, `rowCount` = number of rows, `channel` =
   channel index. The buffer therefore holds `rowCount * width` samples of
   one channel (width from the `CreateImage` `ImageInfo`).
   Preconditions (`:718-723`): called only when the metaformat's
   `CanWriteIncrementally()` is true **and** the instance-level
   `CanWriteIncrementally()` (`:699`, default `true` — keep) is true.
   Per the mpp contract each call is normalized to f32 in [0,1] (integers
   divide by type max; f64 narrows) and split into ≤ band_rows chunks.

7. **`CloseImage`** — `virtual void CloseImage()`
   (`FileFormatImplementation.h:687`). "Closes the image that has been
   created by a previous call to CreateImage()." Also "**must be
   reimplemented** by all derived classes supporting incremental write
   operations" (`:684-685`). For a single-image format this is where the
   shim sends `Finish`, pumps `Progress`, and waits for `Done`/`Error`
   (worker `Error` → `throw pcl::Error(message)`).

8. **`Close`** (`:111`) — final teardown of the instance. Idempotent; if
   `CloseImage` never ran (aborted save), tear down the worker per contract
   §7.4 (`Cancel` pre-Finish, else kill).

### 3.4 Whole-image writes (defensive)

```cpp
virtual void WriteImage( const pcl::Image& image );    // FileFormatImplementation.h:644
virtual void WriteImage( const pcl::DImage& image );   // :649
virtual void WriteImage( const UInt8Image& image );    // :654
virtual void WriteImage( const UInt16Image& image );   // :659
virtual void WriteImage( const UInt32Image& image );   // :664
```

The headers nowhere promise that a `CanWriteIncrementally() == true` format
is *only* driven through `CreateImage`/`WriteSamples` (Unknowns §7.4).
Implement these five by synthesizing the incremental path
(`CreateImage(ImageInfo(image))` → per-channel `WriteSamples` over the
pixel data → `CloseImage`) so a whole-image save also works.

### 3.5 Members that will never be called (given §2.1 capabilities) — leave defaults

Reader side: `Open` (`:160`) — **override anyway** to
`throw Error( "BigTIFF: this implementation is write-only" )`, because the
base behavior is undocumented (Unknowns §7.2); `SelectImage`/
`SelectedImageIndex` (`:170,180` — gated on `SupportsMultipleImages`,
`:165-169`), `ReadICCProfile` (`:216`), `ReadRGBWorkingSpace` (`:227`),
`ReadDisplayFunction` (`:239`), `ReadColorFilterArray` (`:250`),
`ReadThumbnail` (`:257`), `ReadFITSKeywords` (`:264`),
`PropertyDescriptions`/`ReadProperty` (`:281,297` — gated on
`CanStoreProperties`), `ImagePropertyDescriptions`/`ReadImageProperty`
(`:311,324`), `ReadImage` ×5 (`:329-349`), `ReadSamples` ×5 (`:387-419`),
`CanReadIncrementally` (`:361`), `WasInexactRead` (`:433`).
Writer extras gated off by our capabilities: `SetFormatSpecificData`
(`:573`), `WriteICCProfile` (`:579`), `WriteRGBWorkingSpace` (`:585`),
`WriteDisplayFunction` (`:591`), `WriteColorFilterArray` (`:597`),
`WriteThumbnail` (`:603`), `WriteFITSKeywords` (`:609`), `WriteProperty`
(`:625`), `WriteImageProperty` (`:639`).

### 3.6 Lossy-write reporting

`virtual bool WasLossyWrite() const` (`FileFormatImplementation.h:771`,
default `false`): "invoked ... just after successful completion of a call to
WriteImage() or Write()" (`:762-764`). Note the doc names only the
whole-image entry points, not the incremental path (Unknowns §7.6). Since the
shim narrows f64→f32 and remaps u32, track whether the just-written image
came from a narrowing source and return `true` then.

---

## 4. Supporting types

### 4.1 `ImageInfo` (`ImageInfo.h:38`)

```cpp
int         width;             // ImageInfo.h:48 — image width in pixels
int         height;            // :49
int         numberOfChannels;  // :50 — includes alpha channels
color_space colorSpace;        // :51 — ColorSpace::value_type (:46)
bool        supported;         // :52
```

`IsValid()` (`ImageInfo.h:120-126`): width/height > 0, colorSpace known,
`numberOfChannels >= (Gray ? 1 : 3)`. `ColorSpace` enum
(`ColorSpace.h:54-65`): `Unknown = -1, Gray = 0, RGB, CIEXYZ, CIELab,
CIELch, HSV, HSI`. The shim should accept `Gray` (1 ch + optional alpha) and
`RGB` (3 ch + optional alpha) and reject anything else with `pcl::Error`.

### 4.2 `ImageOptions` (`ImageOptions.h:190`) — fields the shim reads

```cpp
uint8 bitsPerSample      : 8;  // ImageOptions.h:194 — 8, 16, 32 or 64 (default 16, :246)
bool  ieeefpSampleFormat : 1;  // :195 — true = IEEE 754 float, false = integer (default false)
bool  complexSample      : 1;  // :196 — reject true
bool  signedIntegers     : 1;  // :197 — reject true
```

Mapping to `InitMessage.bit_depth` (mpp behavioral contract):
`(ieeefp, 32) → "f32"`, `(ieeefp, 64) → "f32"` (narrowed, lossy),
`(int, 16) → "u16"`, `(int, 8) → "u8"`, `(int, 32) → "f32"` (u32/2^32-1).
If `SetOptions` was never called, default-constructed values apply
(`Reset()`, `ImageOptions.h:244-271`: 16-bit integer) — but prefer selecting
the mapping from which `WriteSamples` overload actually fires (Unknowns §7.5).

### 4.3 `ImageDescription` (`ImageDescription.h:47`)

`{ ImageInfo info; ImageOptions options; IsoString id; }`
(`ImageDescription.h:49-51`); `ImageDescriptionArray = Array<ImageDescription>`
(`:100`). Only used by the read path (`Open()` returns one) — not needed by
the write-only shim beyond the `Open()` throw.

### 4.4 Strings

`IsoString` (8-bit, `String.h:5219`; constructible from `const char*`,
`String.h:5381`), `String` (UTF-16, `String.h:7923`),
`StringList = Array<String>` (`StringList.h:47`),
`IsoStringList = Array<IsoString>` (`StringList.h:65`). PCL `String` has
UTF-8 conversion members; the shim converts `FilePath()`-style `String`
paths to UTF-8 `IsoString` for the host lib (verify the exact member name in
`String.h` when coding — MMM's module code is the working example).

---

## 5. Cross-check: client-side view (`FileFormatInstance.h`)

`FileFormatInstance` is how *other* modules (and, at the same conceptual
level, the core) drive a format instance. Its write-path surface mirrors
§3.3 one-for-one and confirms the intended sequence:

- `QueryOptions` (`FileFormatInstance.h:700`, "just before calling
  Create()" at `:694`)
- `Create( filePath, hints, numberOfImages = 1 )` (`:730`)
- `SetId` (`:747`), `SetOptions` (`:764`), `SetFormatSpecificData` (`:794`)
- `CreateImage( const ImageInfo& )` (`:1039`, "The sample data type and
  other ... parameters have been specified by previous calls to
  SetOptions()..." at `:1029-1031`)
- `WriteSamples` ×5 (`:1080-1116`); "Returns true iff the specified pixel
  row(s) were successfully written; false in the event of error, **or if
  the operation was cancelled**" (`:1063-1064`); "it will throw an exception
  if called for a format that does not support incremental writes"
  (`:1075-1078`)
- `CloseImage` (`:1050`), `WasLossyWrite` (`:1126`)

This is documentation of the *caller's* obligations, not a normative
guarantee about the core's exact behavior — hence §7.3.

---

## 6. Build/link facts Task 4 needs (from MMM, our code — verified working)

- Link `libPCL-pxi.a` (`PCLLIBDIR ?= $(HOME)/.local/pcl-build-0182/lib`), headers
  `PCLINCDIR ?= $(HOME)/.local/pcl-build-0182/include` (a `ci/build-pcl.sh
  --out` prefix at the pin); Linux flags include
  `-D__PCL_LINUX -D__PCL_AVX2 -D__PCL_FMA -mavx2 -mfma -m64
  -fnon-call-exceptions -minline-all-stringops -fPIC -pthread`, link
  `-shared ... -lPCL-pxi -lpthread -lrt` (MMM `makefile-x64:14-77`).
- Do **not** pass `-Wl,--no-undefined`: undefined PixInsight-core symbols in
  the module are expected and resolved at load time
  (MMM `makefile-x64:8-11`).
- The module is built with default visibility rules of the makefile;
  `PCL_MODULE_EXPORT` handles the entry point's visibility.

---

## 7. Unknowns — where the headers are silent (code defensively)

1. **`MetaFileFormat` self-registration parent.** `MetaFileFormat()`
   (`MetaFileFormat.h:84`) is documented only as "Constructs a
   MetaFileFormat object." That it parents itself under `pcl::Module` is an
   inference from the MetaObject tree design (§1.3), proven for
   `MetaProcess` but not re-verified for `MetaFileFormat`. Defense: create
   the `MetaModule` strictly before the `MetaFileFormat` in PMINS; if the
   module fails to install with a format-registration error, this is the
   first place to look.
2. **Base-class behavior of non-overridden virtuals.** `Open`, `Close`,
   `IsOpen`, `FilePath`, `Create`, `CreateImage`, `CloseImage`,
   `WriteSamples`, etc. are declared virtual with out-of-line library
   bodies; the headers never say whether the defaults throw, no-op, or
   crash. Defense: override every member on our write path **and** `Open`
   (explicit "write-only" `Error`), `IsOpen`, `FilePath`, `Close`.
3. **Call-order guarantees.** The documented sequence (§3.3/§5) is what the
   core "typically" does; nothing normatively promises `SetOptions` is
   always called before `CreateImage`, that `CloseImage` precedes `Close`,
   or that `Close` is called at all on error/abort paths. Defense: derive
   geometry only from `CreateImage`; make `CloseImage` and `Close`
   idempotent; make the destructor do a full teardown (kill+reap worker,
   unlink shm) if `Close` never came.
4. **Whole-image vs incremental dispatch.** Headers don't say whether the
   core prefers `WriteSamples` over `WriteImage` when
   `CanWriteIncrementally()` is true, or under what threshold it might use
   `WriteImage`. Defense: implement both paths (§3.4).
5. **Which `WriteSamples` overload fires.** Presumably matches the source
   image's sample type, but the headers don't bind overload choice to
   `SetOptions` values. Defense: make every overload self-contained
   (normalize its own buffer to f32); don't assume the overload agrees with
   `ImageOptions.bitsPerSample`; if the first `WriteSamples` overload
   disagrees with the stored options, trust the overload for conversion and
   keep the Init bit_depth already negotiated.
6. **`WasLossyWrite` after incremental writes.** Documented trigger is
   "after ... WriteImage() or Write()" (`FileFormatImplementation.h:762-764`);
   whether the core also queries it after `CloseImage` is unstated. Defense:
   keep the flag accurate at all times; it costs nothing.
7. **Threading.** Nothing in these headers states which thread calls the
   instance members, whether one instance can be called concurrently, or
   whether multiple instances (parallel saves) can exist simultaneously.
   Defense: no mutable statics in the instance path; one `WriteSession` per
   instance; unique shm names per instance (the pi-protocol already requires
   this); assume per-instance calls are serialized but never assume
   cross-instance serialization, and don't touch GUI/Console from
   `WriteSamples`.
8. **Abort/cancel delivery.** `FileFormatImplementation` exposes no abort
   hook; client-side `WriteSamples` "returns false ... if the operation was
   cancelled" (`FileFormatInstance.h:1063-1064`) suggests cancellation
   surfaces as an exception (or core-side short-circuit) rather than a
   callback into the module. Defense: v1 may defer abort wiring (contract
   allows it); ensure `Close`/destructor handles a half-written image by
   `Cancel`-then-kill per contract §7.4, and never leaves the worker running.
9. **`numberOfImages < 0`.** Allowed per `FileFormatImplementation.h:493-495`
   ("less than zero if the number of images is unknown"). Defense: treat
   negative as 1 (§3.3 step 2) rather than throwing.
10. **Hints vocabulary.** No standard hint list exists in these headers;
    hint strings are format-defined (`:506-516`). v1 ignores hints; if a
    later version adds any, they must be documented in the README.

---

## 8. Read-path API (Phase 3 — the module now reads)

Added when `CanRead()` was flipped to `true` (BigTIFF reader). All citations
below refer to `/opt/PixInsight/include/pcl/<file>` at PCL 2.9.4 (verified
identical line numbers in the pinned 2.8.3 build headers,
`$HOME/.local/pcl-build-0182/include/pcl/`, against which the module actually
compiles). No PCL module implementation source was consulted.

### 8.1 Format-level capability

- `bool MetaFileFormat::CanReadIncrementally() const override`
  (`MetaFileFormat.h:487`, default returns `false`). We override it to
  `false` **explicitly**: the core drives interactive opens through
  whole-image `ReadImage`, not `ReadSamples`, even when this is `true`
  (design "Findings" §3). `CanRead()` (`MetaFileFormat.h:462`, default
  `true`) is left/asserted `true`.
- The instance-level `FileFormatImplementation::CanReadIncrementally() const`
  (`FileFormatImplementation.h:393`, default returns **`true`**) is only
  consulted when the format-level one is `true`; since ours is `false`,
  `ReadSamples` is never called and is left unimplemented.

### 8.2 `Open()` — geometry probe → `ImageDescription`

- `virtual ImageDescriptionArray Open( const String& filePath,
  const IsoString& hints )` (`FileFormatImplementation.h:192`). Returns one
  `ImageDescription` per image in the file; a single-image format returns a
  one-element array. On error, throw `pcl::Error` (same convention as the
  write path, §3.2).
- `ImageDescription( const ImageInfo& i, const ImageOptions& o,
  const IsoString& s = IsoString() )` (`ImageDescription.h:94`) — the value
  we `Add` to the returned `ImageDescriptionArray` (`Array::Add`,
  `Array.h:1025`).
- `ImageInfo` fields set from the worker's probe geometry: `width`, `height`,
  `numberOfChannels` (includes alpha), `colorSpace`
  (`ColorSpace::RGB`/`ColorSpace::Gray`), `supported = true` (§4.1).
- `ImageOptions` fields set from the file's sample format:
  `bitsPerSample` (8/16/32/64), `ieeefpSampleFormat` (true for f32/f64),
  `complexSample = false`, `signedIntegers = false` (§4.2). The core
  dispatches to the `ReadImage` overload matching this declared format.
- `ColorSpace::value_type` (`ColorSpace.h:86`) is the enum stored on
  `ImageInfo::colorSpace` (`ImageInfo.h:79` aliases it as `color_space`) and
  passed to `AllocateData`.

### 8.3 The five `ReadImage` overloads

```cpp
virtual void ReadImage( pcl::Image&   image );  // FileFormatImplementation.h:361  f32
virtual void ReadImage( pcl::DImage&  image );  // :366                            f64
virtual void ReadImage( UInt8Image&   image );  // :371                            u8
virtual void ReadImage( UInt16Image&  image );  // :376                            u16
virtual void ReadImage( UInt32Image&  image );  // :381                            u32
```

The overload matching the `bit_depth` declared in `Open()` allocates the
destination and decodes; the others throw (defensive — the headers do not
promise which fires; mirrors the write-path "implement all five" discipline,
Unknowns §5). Sample-type ground truth is the same table as §3.3 step 6
(`Image::sample = float`, `DImage::sample = double`, `UInt8/16/32Image::sample
= uint8/uint16/uint32`).

- `GenericImage& AllocateData( int width, int height,
  int numberOfChannels = 1, color_space colorSpace = ColorSpace::Gray )`
  (`Image.h:6484`) — allocates the destination pixel buffer. "Newly allocated
  pixel samples are not initialized" (`Image.h:6481-6482`), which is fine
  because the decode covers every row of every channel. Resets all selections
  (`Image.h:6478-6479`).
- `sample* PixelData( int channel = 0 )` (`Image.h:6721`) — "a pointer to the
  first pixel sample in the specified channel" at (x=0,y=0)
  (`Image.h:6713-6716`); the channel plane is contiguous row-major, so a band
  starting at row `startRow` is written at `PixelData(ch) + startRow*width`
  for `rows*width` samples. This is the write-mutating counterpart of the
  `const sample* PixelData( int ) const` accessor the write path reads
  through (`Image.h:6734`); `ScanLine( int y, int channel )` (`Image.h:6820`,
  `= PixelData(channel) + RowOffset(y)`) is the equivalent per-row accessor,
  not needed here since bands are whole rows.

### 8.4 `.big` save-rename (write path)

- `String::EndsWithIC( const_c_string t ) const` (`String.h:2976`,
  case-insensitive) — tests the output path for a TIFF-family tail in
  `Create()`.
- `void Console::WarningLn( const String& s )` (`Console.h:473`) — emits the
  one-line rename notice. `pcl::Console` is `Console.h:360`; a temporary
  `Console()` suffices for a single message.
