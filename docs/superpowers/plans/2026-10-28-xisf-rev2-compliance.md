# XISF Revision 2 Compliance (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mpp-core reads every XISF unit a baseline decoder must read under
the XISF 1.0 spec, document version 1.02 (Revision 2): all eight
compression codecs, subblocks (including raw `c = u` subblocks), checksums,
both byte orders, both pixel storage models, UInt8/16/32 and Float32/64
samples, multiple images, and compressed property blocks. Uncompressed
Float32 panels keep their zero-copy mmap path.

**Architecture:** Three new focused modules under `formats/` carry the
generic machinery:
- `xisf_codec` — codecs, byte shuffling, subblocks;
- `xisf_block` — the XISF data block: location, byte order, compression,
  checksum, and decoding to bytes;
- `xisf_pixels` — conversion of decoded sample bytes to normalized planar
  `f32`.

`xisf.rs` keeps the XML walk and the public `XisfImage` / `read_header`
API, and routes every image and property block through `xisf_block`.

**Tech Stack:** Rust 2024; new deps `flate2` (zlib), `lz4_flex` (LZ4/LZ4HC
block format), `zstd` (libzstd), `sha1`/`sha2`/`sha3` (RustCrypto). All are
ungated, because the lean core must read compressed XISF.

**Spec:** the published XISF 1.0 specification
(<https://pixinsight.com/doc/docs/XISF-1.0-spec/XISF-1.0-spec.html>, rev 1.02).
The roadmap is mmm `docs/superpowers/plans/2026-10-28-formats-to-mpp-xisf-rev2.md`
§2 (the compliance gap list, which quotes the normative rules used here)
and §4 Phase 2.

## Global Constraints

- **Branch** `feat/formats-consolidation` (mpp). No push, no merge. Each
  commit is green on `cargo fmt --all --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo test -p mpp-core --no-default-features`, and `cargo doc --no-deps`
  (both feature modes). Every public item gets a doc comment.
- **Tests:** no test reads `test_data/`. Inputs are synthesized by a test
  encoder in `testsupport.rs`.
- **Fast path unchanged:** an uncompressed, attached, Planar,
  little-endian Float32 image with bounds `0:1` (or none), no `offset`, and
  a 4-byte-aligned offset is served zero-copy from the mmap, exactly as
  today.
- **Decoded = mapped:** for the fast-path-equivalent case (Float32, bounds
  0:1), decoded values are the stored values bit-for-bit, NaN included.
  A compressed copy of a file must decode identically to the uncompressed
  original.
- **Normative rules** (spec §10.5–10.6, quoted in the roadmap §2):
  - **Compression syntax:** `compression="<codec>:<usize>[:<item>]"`,
    where `+sh` codecs carry an item size.
  - **Subblocks syntax:** `subblocks="c1,u1:c2,u2:…"`. With no `subblocks`
    attribute, the block is one subblock `(stored_len, usize)`.
  - **Raw subblocks:** `cᵢ = uᵢ` means the subblock is stored raw. But
    first try decompressing it; accept the result only if it yields exactly
    `uᵢ` bytes, and otherwise copy it raw (legacy rule).
  - **Exact length:** every compressed subblock must yield exactly `uᵢ`
    bytes, and Σuᵢ must equal `usize`, or the block is invalid.
  - **Unshuffle:** applied once to the concatenated output, with the
    trailing `len % item` bytes left unshuffled at the end.
  - **Codec stream formats:** zlib is the RFC 1950 stream; LZ4/LZ4HC use
    the LZ4 *block* format; zstd uses one *frame* per subblock.
  - **Checksum syntax:** `checksum="<alg>:<lowercase hex>"`, with `sha-1|sha1`,
    `sha-256|sha256`, `sha-512|sha512`, `sha3-256`, `sha3-512`.
  - **Checksum scope:** the digest covers the stored (compressed) bytes, or
    the decoded binary for inline and embedded blocks. Verify before
    decompressing.
  - **`byteOrder`** applies to the uncompressed data.
- **Policy rulings** (roadmap §4):
  - A checksum mismatch is an error unless the open options set
    `verify_checksums: false`.
  - Float bounds other than `0:1` are rescaled to [0,1].
  - Integers are normalized by their type range, or by `bounds` when it is
    given.
  - `offset` is subtracted before normalization.
  - `orientation` is ignored. It is display-only by spec.
  - Refused by name:
    - `UInt64` and `Complex32/64` pixels;
    - the `CIELab` color space;
    - `url(…)`/`path(…)` locations (distributed units);
    - unknown codecs and checksum algorithms.

## Review Focus

1. **Compressed property blocks on a solved panel:** a property whose
   attachment size is the compressed size must decode, and must not trip
   the old `size == n*width` check. PI writes these, e.g.
   `ControlPoints:Celestial` as `zstd+sh:138384:8`. Pinned in Task 2.4,
   `compressed_property_block_decodes`.
2. **A truncated or corrupt compressed stream** must give an error naming
   the block and the subblock index, never a panic or a short image.
   Pinned in Task 2.1, `corrupt_subblock_is_an_error`, and Task 2.6,
   `truncated_compressed_image_is_refused`.
3. **Subblock sizes inconsistent with the attributes** (Σcᵢ ≠ stored length,
   Σuᵢ ≠ declared size, a `+sh` codec without an item size, item size 0)
   must be refused at open, not at decode. Pinned in Task 2.3,
   `inconsistent_compression_attributes_are_refused`.
4. **A huge declared uncompressed size** (e.g. 2⁶³ from a hostile or
   corrupt header) must be refused before any allocation: it must equal
   geometry × sample size for images, and n × width for properties. Pinned
   in Task 2.6, `declared_size_must_match_geometry`.
5. **Checksum present but the mapped fast path is taken:** the checksum is
   still verified at open (when `verify_checksums`). Pinned in Task 2.6,
   `mapped_path_still_verifies_checksum`.

---

### Task 2.1: Codecs, byte shuffling, subblocks (`xisf_codec`)

**Files:**
- Modify: `Cargo.toml` (workspace deps `flate2 = "1"`,
  `lz4_flex = { version = "0.11", default-features = false, features = ["std", "safe-decode", "safe-encode"] }`,
  `zstd = "0.13"`) and `crates/mpp-core/Cargo.toml`.
- Create: `crates/mpp-core/src/formats/xisf_codec.rs`
- Modify: `crates/mpp-core/src/formats/mod.rs` (`pub mod xisf_codec;`)

**Interfaces:**
- Produces:
  - `pub enum Codec { Zlib, Lz4, Lz4Hc, Zstd }`.
  - `pub struct Compression { pub codec: Codec, pub item_size: Option<usize>, pub uncompressed_size: u64, pub subblocks: Vec<(u64, u64)> }`.
    `item_size` is `Some` exactly for `+sh` codecs.
  - `impl Compression`:
    - `pub fn parse(compression: &str, subblocks: Option<&str>, stored_len: u64) -> std::result::Result<Compression, String>`
      fills `subblocks` with `[(stored_len, uncompressed_size)]` when the
      attribute is absent. It validates Σc = `stored_len`,
      Σu = `uncompressed_size`, item size ≥ 1, and a known codec.
    - `pub fn codec_id(&self) -> String` returns e.g. `"zstd+sh"`, for
      messages and `mmm info`.
  - `pub fn unshuffle(src: &[u8], item: usize, dst: &mut [u8])` and
    `pub fn shuffle(src: &[u8], item: usize, dst: &mut [u8])`. These follow
    the spec reference routines, and the `len % item` tail is copied as is.
  - `pub fn decompress_into(c: &Compression, stored: &[u8], out: &mut [u8], scratch: Option<&mut [u8]>) -> std::result::Result<(), String>`:
    - `out.len()` must equal `uncompressed_size`.
    - With `item_size`, `scratch` must be `Some` with the same length.
      Subblocks are decompressed into scratch, then unshuffled into `out`.
      Without it, they are decompressed straight into `out`.
    - Subblocks are decoded in parallel with `std::thread::scope`, each into
      its disjoint output range.
    - Errors name the subblock index and the codec.
  - `#[cfg(test)] pub(crate) fn compress(codec: Codec, shuffle_item: Option<usize>, data: &[u8], subblock_size: usize) -> (Vec<u8>, Compression)`
    is the test encoder.
    - It applies the Rev 2 encoder rule: a subblock that does not shrink is
      stored raw (`c = u`).
    - zlib uses `flate2::write::ZlibEncoder`.
    - LZ4 and LZ4HC both use `lz4_flex::block::compress`. lz4_flex has no
      HC encoder, but HC output is plain LZ4 block format, so an LZ4 stream
      exercises the LZ4HC decode path.
    - zstd uses `zstd::bulk::compress(_, 3)`.
- Decoders:
  - zlib: `flate2::Decompress::new(true)`, i.e. zlib-wrapped. Loop until
    `Status::StreamEnd`, then require `total_out == uᵢ`.
  - LZ4/LZ4HC: `lz4_flex::block::decompress_into(src, dst)` must return
    `uᵢ`.
  - zstd: `zstd::bulk::Decompressor::new()?.decompress_to_buffer(src, dst)`
    must return `uᵢ`.
  - A `cᵢ == uᵢ` subblock: try the codec into `dst`. If that fails or
    yields ≠ `uᵢ`, copy `src`.

- [ ] **Step 1: Failing tests:**
  - `shuffle_matches_spec_example`: item 4, bytes
    `[a0 a1 a2 a3 b0 b1 b2 b3 t]` → `[a0 b0 a1 b1 a2 b2 a3 b3 t]`, and
    `unshuffle` inverts it.
  - `shuffle_is_identity_for_item_one`.
  - `round_trips_every_codec_with_and_without_shuffle`: 8 codecs ×
    {1 subblock, 5 subblocks} over 100 003 bytes of a smooth Float32 ramp.
  - `raw_subblocks_are_copied`: incompressible noise, so every `c = u`.
  - `mixed_raw_and_compressed_subblocks`: a smooth half and a noise half
    with a subblock size of half.
  - `legacy_equal_length_compressed_subblock_is_decompressed`: build a
    subblock whose zstd output is padded with a zstd skippable frame, so
    `c == u` but the subblock is still compressed. The decoder must return
    the decompressed data.
  - `corrupt_subblock_is_an_error`: flip bytes in subblock 2 of 3. The
    error contains `"subblock 2"`.
  - `wrong_decompressed_length_is_an_error`: declare `u + 1`.
  - `compression_attribute_parses`: covers `"zstd+sh:400:4"`, `"lz4:10"`,
    a subblocks list, and refusal of `"bzip2:10"`, `"zlib+sh:10"` (no item
    size) and `"zlib:10"` with Σc mismatch.
- [ ] **Step 2:** Run `cargo test -p mpp-core --no-default-features xisf_codec`.
  Expected: FAIL (module missing).
- [ ] **Step 3:** Implement. Run it again. Expected: PASS. Also run the
  full workspace.
- [ ] **Step 4: Commit:** `feat(core): XISF compression codecs (zlib/lz4/lz4hc/zstd, +sh), subblocks, Rev 2 raw subblocks`.

### Task 2.2: Checksums

**Files:**
- Modify: `Cargo.toml` (workspace `sha1 = "0.10"`, `sha2 = "0.10"`,
  `sha3 = "0.10"`) and the mpp-core manifest.
- Create: `crates/mpp-core/src/formats/xisf_block.rs`. Start it with the
  checksum part.

**Interfaces:**
- Produces:
  - `pub enum ChecksumAlgorithm { Sha1, Sha256, Sha512, Sha3_256, Sha3_512 }`.
  - `pub struct Checksum { pub algorithm: ChecksumAlgorithm, pub digest: Vec<u8> }`.
  - `impl Checksum`:
    - `pub fn parse(attr: &str) -> std::result::Result<Checksum, String>`
      accepts both name spellings and uppercase hex. The spec says
      lowercase; be lenient on read.
    - `pub fn verify(&self, data: &[u8]) -> std::result::Result<(), String>`.
      The mismatch message contains `"checksum mismatch"`, the algorithm,
      and the expected and actual hex.

- [ ] **Step 1: Failing tests.** `checksum_known_vectors` covers
  `"abc"` digests for all five algorithms (FIPS 180-4 / 202 test vectors);
  both spellings parse. `checksum_mismatch_names_both_digests`.
  `checksum_rejects_unknown_algorithm` uses `"md5:…"`.
- [ ] **Step 2:** FAIL. Implement. PASS.
- [ ] **Step 3: Commit:** `feat(core): XISF block checksums (SHA-1/2/3)`.

### Task 2.3: Data blocks (`xisf_block`)

**Files:**
- Modify: `crates/mpp-core/src/formats/xisf_block.rs`.

**Interfaces:**
- Produces:
  - `pub enum ByteOrder { Little, Big }`.
  - `pub enum BlockLocation { Attachment { offset: u64, size: u64 }, Inline(Vec<u8>) }`.
    `Inline` holds the decoded bytes of an `inline:` or `embedded` block.
  - `pub struct DataBlock { pub location: BlockLocation, pub byte_order: ByteOrder, pub compression: Option<Compression>, pub checksum: Option<Checksum> }`.
  - `pub(crate) struct BlockAttrs<'a> { pub location: &'a str, pub byte_order: Option<&'a str>, pub compression: Option<&'a str>, pub subblocks: Option<&'a str>, pub checksum: Option<&'a str>, pub text: Option<&'a str>, pub text_encoding: Option<&'a str> }`.
    The `embedded` case passes the `<Data>` element's `encoding`,
    `compression` and `subblocks` here.
  - `impl DataBlock`:
    - `pub(crate) fn parse(a: &BlockAttrs) -> std::result::Result<DataBlock, String>`.
      It accepts `attachment:<pos>:<size>`, `inline:base64|hex`, and
      `embedded` (with `text`). For `url(`/`path(`, it returns
      `"distributed XISF units (url()/path() block locations) are not supported"`.
      `byteOrder` is `big` or `little`, defaulting to little.
    - `pub fn uncompressed_len(&self) -> u64`.
    - `pub fn stored_len(&self) -> u64`.
    - `pub fn stored<'a>(&'a self, file: &'a [u8]) -> std::result::Result<&'a [u8], String>`
      does a bounds-checked attachment slice, or returns the inline bytes.
    - `pub fn decode_into(&self, file: &[u8], verify: bool, out: &mut [u8], scratch: Option<&mut [u8]>) -> std::result::Result<(), String>`
      verifies the checksum on the stored bytes (when `verify`), then
      decompresses or copies into `out`. It leaves byte order to the caller.
    - `pub fn decode(&self, file: &[u8], verify: bool) -> std::result::Result<Vec<u8>, String>`
      is the convenience for small (property) blocks, allocating its own
      scratch.

- [ ] **Step 1: Failing tests:**
  - `parses_attachment_inline_and_embedded_locations`.
  - `distributed_locations_are_refused_by_name`.
  - `byte_order_parses_and_defaults_little`.
  - `inconsistent_compression_attributes_are_refused`: Σc ≠ stored size,
    and `+sh` with item 0. This is Review Focus 3.
  - `decode_verifies_checksum_before_decompressing`: a corrupt stored byte
    plus a good checksum of the *uncompressed* data still fails, with
    `"checksum mismatch"`. That proves the digest is over stored bytes.
  - `decode_skips_verification_when_disabled`.
  - `inline_block_checksum_covers_decoded_bytes`.
- [ ] **Step 2:** FAIL. Implement. PASS.
- [ ] **Step 3: Commit:** `feat(core): XISF data blocks — locations, byte order, compression, checksum`.

### Task 2.4: Properties through data blocks; text-form robustness

**Files:**
- Modify: `crates/mpp-core/src/formats/xisf.rs`:
  - `PendingProperty` gains `byte_order`, `compression`, `subblocks`,
    `checksum`, and `data: Option<(attrs, text)>` for an embedded `<Data>`
    child.
  - `finish_property_strict` builds a `DataBlock` for vector and matrix
    properties.
  - The attachment resolution in `XisfImage::open` and `read_header` goes
    through `DataBlock::decode`.
- Modify: `crates/mpp-core/src/testsupport.rs` to add the test encoder
  `XisfBuilder`. It is a small builder that writes monolithic XISF:
  - images: geometry, sample format, storage, byte order, bounds, color
    space, block = attached, optionally compressed and checksummed;
  - properties: scalars and numeric vectors/matrices, as inline, embedded
    or attached blocks, optionally compressed and checksummed;
  - FITS keywords;
  - the header-length field and 4096-aligned attachments for uncompressed
    images. This mirrors PI. Compressed blocks follow unaligned, as PI
    writes them.

  It uses `xisf_codec::compress` and `Checksum` digests via the sha
  crates.

**Interfaces:**
- Consumes: `DataBlock`, `Compression`, `Checksum` (Tasks 2.1–2.3).
- Produces:
  - `XisfProperty::location` keeps its meaning (offset, *stored* size) for
    attached blocks. This is wire-compatible: the type is unchanged, and
    only the size semantics for compressed blocks are newly defined.
  - Vector and matrix byte order is honoured: `byteOrder="big"` swaps per
    element.
  - Booleans are `true`/`false`/`1`/`0`.
  - Float scalars accept `NaN`, `+Inf`, `-Inf`, `nan`, `-nan`, `inf`,
    `-inf`. Rust's `f64::from_str` accepts all of these. Add the test
    anyway.
  - Leading and trailing whitespace on scalars is ignored.
  - Empty vectors and matrices are zero-length inline blocks.

- [ ] **Step 1: Failing tests** (synthesized with `XisfBuilder`):
  - `compressed_property_block_decodes` covers zstd+sh item 8, an attached
    F64Matrix, and a sha-256 checksum. This is Review Focus 1.
  - `embedded_property_block_decodes` covers
    `location="embedded"` + `<Data encoding="base64" compression="zlib:…">`.
  - `big_endian_property_vector_decodes`.
  - `read_header_decodes_compressed_properties`.
  - `property_checksum_mismatch_is_an_error_unless_disabled`.
  - `scalar_text_forms`: booleans, non-finite floats, padded whitespace.
  - `empty_vector_inline_block`.
- [ ] **Step 2:** FAIL. Implement. PASS. All existing xisf tests stay
  green.
- [ ] **Step 3: Commit:** `feat(core): XISF property blocks — compression, checksums, embedded, byte order`.

### Task 2.5: Image elements — attributes and multiple images

**Files:**
- Modify: `crates/mpp-core/src/formats/xisf.rs`
  (`parse_image_element`, `parse_header`).

**Interfaces:**
- Produces:
  - `XisfHeader` gains:
    - `pub block: DataBlock`;
    - `pub pixel_storage: PixelStorage` (`Planar` | `Normal`, default
      Planar);
    - `pub color_space: ColorSpace` (`Gray` | `Rgb` | `CieLab`, default
      Gray);
    - `pub bounds: Option<(f64, f64)>`;
    - `pub offset: f64` (default 0);
    - `pub image_id: Option<String>`;
    - `pub image_count: usize`;
    - `pub image_ids: Vec<Option<String>>`.
  - `data_offset` and `data_size` remain, now documented as the *stored*
    block extent. Inline and embedded blocks give `(0, len)`.
  - `SampleFormat` gains `UInt64`, `Complex32` and `Complex64`. They parse
    and report their sizes. Decoding refuses them in Task 2.6.
  - `pub struct XisfOpenOptions { pub image_index: usize, pub verify_checksums: bool }`,
    with `Default` = `{0, true}`.
  - `XisfImage::open_with(path, &XisfOpenOptions)` and
    `xisf::read_header_with(path, &XisfOpenOptions)`. `open` and
    `read_header` keep using the defaults.
  - The selected image's `<FITSKeyword>`s and the file-level properties
    plus the selected image's properties go into the header. `<Thumbnail>`
    and its children are skipped.
- [ ] **Step 1: Failing tests:**
  - `second_image_is_selectable`: a two-image unit; index 1 reads the
    second image's geometry and keywords.
  - `image_index_out_of_range_is_an_error`.
  - `thumbnail_is_ignored`.
  - `image_attributes_parse` covers bounds, offset, colorSpace and
    pixelStorage.
  - `missing_float_bounds_defaults_to_unit`.
- [ ] **Step 2:** FAIL. Implement. PASS.
- [ ] **Step 3: Commit:** `feat(core): XISF image attributes and multi-image units`.

### Task 2.6: Pixel decoding (`xisf_pixels`) and `XisfImage` backing

**Files:**
- Create: `crates/mpp-core/src/formats/xisf_pixels.rs`
- Modify: `crates/mpp-core/src/formats/xisf.rs` and
  `crates/mpp-core/src/formats/mod.rs`.
- Modify: `crates/mpp-core/src/convert.rs` and
  `crates/mpp-core/src/publish.rs`, which call `XisfImage::load()` after
  `open`.

**Interfaces:**
- Produces:
  - `pub(crate) fn to_planar_f32(header: &XisfHeader, raw: &[u8], out: &mut [f32]) -> std::result::Result<(), String>`.
    `raw` is the uncompressed block in its byte order.
    - Byte order is swapped per sample.
    - Normal storage is de-interleaved.
    - UInt *k* is normalized by `(v − offset − lo)/(hi − lo)`, with
      `[lo, hi] = bounds.unwrap_or([0, 2ᵏ−1])`.
    - Float is `(v − offset − lo)/(hi − lo)` with `bounds.unwrap_or([0, 1])`.
      When it is exactly `[0, 1]` and `offset == 0`, the value is copied
      unchanged (the decoded = mapped rule).
    - UInt64, Complex and CIELab are refused, naming the format.
  - `impl XisfImage`:
    - `pub fn is_mapped(&self) -> bool`.
    - `pub fn decode_into(&self, out: &mut [f32]) -> Result<()>` fills
      planar normalized f32 for `w*h*c` samples. Any image works.
    - `pub fn load(&mut self) -> Result<()>` decodes into an owned `Vec`
      when the image is not mapped; it is a no-op when mapped.
    - `channel()`, `row()` and `PlanarRows` work when mapped or loaded.
      Otherwise they panic with
      `"XISF pixels not loaded: call load() or decode_into()"`.
  - `open` never decodes pixels. For the mapped path it verifies the
    checksum when one is present and `verify_checksums` is set.
  - The scratch buffer for shuffled codecs is a `Vec<u8>`, allocated
    inside `decode_into`. Decoding a 2 GB block costs 2× its size in
    memory transiently. That is acceptable for Phase 2. Phase 3's mmm cache
    may pass an mmap-backed scratch through a `decode_into_with_scratch`
    variant if needed. Do not build that now.
- [ ] **Step 1: Failing tests** (`XisfBuilder` permutations; each compares
  against the same pixels written uncompressed, Planar, little-endian,
  Float32):
  - `every_codec_decodes_bit_identical_to_uncompressed` covers 8 codecs ×
    {1, 3 subblocks} on Float32 RGB.
  - `integer_formats_normalize` covers UInt8, UInt16 and UInt32, with and
    without `bounds`.
  - `float64_and_nonunit_bounds_rescale`.
  - `big_endian_and_normal_storage_decode`.
  - `offset_is_subtracted`.
  - `uint64_complex_and_cielab_are_refused_by_name`.
  - `mapped_path_still_verifies_checksum`: an uncompressed Float32 with a
    bad checksum fails at open; `verify_checksums: false` opens it. This is
    Review Focus 5.
  - `truncated_compressed_image_is_refused`: the stored attachment runs
    past EOF. Expected: an open error, no panic. This is Review Focus 2.
  - `declared_size_must_match_geometry`: compression declares 2⁶²; open
    fails without allocating. This is Review Focus 4.
  - `load_then_planar_rows_matches_decode_into`.
- [ ] **Step 2:** FAIL. Implement. PASS, including the existing
  convert/publish tests.
- [ ] **Step 3: Commit:** `feat(core): decode any baseline XISF image to planar f32; zero-copy path unchanged`.

### Task 2.7: Real-sample smoke check and conformance docs

**Files:**
- Create: `crates/mpp-core/examples/xisf_samples_check.rs`. Usage:
  `cargo run --release -p mpp-core --no-default-features --example xisf_samples_check -- <dir>…`.
  For each `manifest-*.json` in each dir, it does this for every entry
  with a `reference`:
  - opens the file and its reference;
  - checks that the decoded planar f32 is bit-identical;
  - checks that the property ids and values are equal;
  - checks that the FITS keywords are equal;
  - prints one line per file;
  - exits non-zero on any mismatch.

  The multi-image entry checks that `image_count == 2` and that both
  images decode. It also prints the MB/s for the largest file.
- Modify: `docs/DESIGN.md` in mpp, adding an "XISF conformance" section:
  supported, refused-by-name, policy rulings, the PI 1.9.5 encoder facts
  from the roadmap, and the memory note for shuffled decode.
- [ ] **Step 1:** Write the example. Run it against
  `/home/dpaull/dev/mega-merge-mosaic/test_data/xisf_samples/pi-1.9.5-pre-update`
  and `…/pi-1.9.5-1706`. Expected: every line `OK`, exit 0. The 2.35 GB
  big set must decode, and the mixed raw/compressed LZ4 file must match.
  Record the throughput in the commit message.
- [ ] **Step 2:** Write the docs. `cargo doc` produces no warnings.
- [ ] **Step 3: Commit:** `test(core): real PixInsight sample check; docs: XISF conformance`.
