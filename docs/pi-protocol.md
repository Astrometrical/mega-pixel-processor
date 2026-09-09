# mpp IPC wire protocol (v3)

This is the contract between a host process (the PixInsight PCL module,
Plan B) and `mpp-ipc-worker`, the Rust process that links `mpp-core` and
encodes the actual output file. It is the **source of truth for the Plan-B
C++ host** — `mpp_protocol.h` mirrors it byte-for-byte — and must be kept in
exact sync with the Rust constants in `mpp-ipc::protocol` whenever the
protocol changes; both are updated in the same commit. The canonical Rust
types live in:

- `crates/mpp-ipc/src/protocol.rs` — frame codec, tags, binary band frame
  layout, JSON message shapes (added in Task 3 of the implementing plan).
- `crates/mpp-ipc/src/shm.rs` — shared-memory slot layout (Task 2).
- `crates/mpp-ipc/src/link.rs` — the worker-side state machine (`HostLink`)
  that this document describes from the host's point of view (Task 4).
- `crates/mpp-ipc/src/testhost.rs` — `run_write_session` and its variants
  (`run_write_session_shuffled`, `run_cancel_after_first_band`,
  `run_write_session_incomplete`, `run_oversize_band_session`), a reference
  implementation of the host side (test-only, feature `testkit`, but
  protocol-faithful — a useful cross-check while implementing the C++ host)
  (Task 6).

This protocol is a direct descendant of `mmm-ipc`'s own wire protocol
(`~/dev/MergeMosaic/integration/pixinsight/PROTOCOL.md`) — same frame
format, same shm-ownership rules, same versioning discipline — slimmed down
because this direction only needs pixels pushed one way.

**Version 2** adds a second, opposite-direction flow alongside the original
v1 write path described in §1-§8 below: a **read** mode in which the worker
opens an existing TIFF/BigTIFF file and *pulls* bands from the host instead
of the host pushing them. §9 documents everything read-mode adds
(`InitMessage.mode`, the `ImageMeta` frame, the `ReadBand`/`ReadBandAck`
exchange, and the inverted slot-ownership rule that comes with it).

**Version 3** (this revision) widens `InitMessage` and nothing else — no new
tag, no changed binary layout, no changed lifecycle rule. It adds
`band_format`, naming the sample type the host places in shm slots (before
v3, every band crossed shm as unit-range `f32`), and fourteen fields
carrying the TIFF writer's full option set: container, block layout and tile
size, compression, deflate level, predictor, alpha association, an ICC
profile, three text tags, and resolution. **`band_format` describes the shm
payload; `bit_depth` describes the output file** — separating those two is
the entire point of the version. Every v3 field has a wire default, so a v2
host's `Init` payload still deserializes into exactly the v2 behaviour. §10
is the complete v3 contract; §5's field table, §6's alignment rule and §8's
version history are updated in place.

## 1. Overview & topology

PixInsight *has* the pixels: a script or process invokes the module with an
already-open image and asks it to write a BigTIFF (or other supported)
file. That makes this integration a **host-push** model, the mirror image
of `mmm-ipc`'s pull model (where the worker asked the host for panel rows on
demand). Here the host simply delivers every row of every channel, in
whatever order is convenient for it, and tells the worker when it's done.

The host spawns `mpp-ipc-worker` as a child process and talks to it over
two channels:

- **Control channel** — the worker's stdin (host → worker) and stdout
  (worker → host), carrying small length-prefixed frames: job setup, band
  delivery acks, progress, completion/error.
- **Bulk pixel channel** — a single named shared-memory (shm) segment,
  sized and created by the host before the worker starts, carved into
  fixed-size slots. Pixel data never travels over the control-channel
  pipes; only slot ids do.

The worker's job, end to end: read `Init`, attach to the shm segment,
receive a stream of `Band` frames (each naming a slot already filled by the
host with one contiguous run of rows of one channel), copy each slot's
contents into an mmap'd staging file, ack the slot so the host can reuse it,
and — once `Finish` arrives and every row of every channel has been
covered — stream the fully-covered staging planes through the BigTIFF
encoder and report `Done`.

## 2. Transport

- **Control channel** = the worker process's stdin/stdout. The host writes
  host→worker frames to the child's stdin and reads worker→host frames from
  the child's stdout. The worker's stderr carries no protocol frames and is
  free for human-readable diagnostics/panics.
- **Bulk channel** = one named POSIX shared-memory object (a named memory
  section on Windows). The **host creates** it and hands its name to the
  worker in the `Init` message; the **worker attaches** to the existing
  segment by that name — it never creates or resizes it. Only the creator
  unlinks the underlying named object (on the host's shutdown/cleanup, and
  defensively at the start of the next `create` under the same name, to
  clean up after a crashed prior run). See §6 for the full ownership rule.
- The length prefix in every control-channel frame is **trusted, not
  attacker-bounded**: both ends of the pipe are the host and its own
  spawned worker child, not an adversarial peer. A C++ host does not need
  to defend against a malicious length value beyond ordinary OOM handling.
- EOF handling: a clean EOF read before any byte of a new frame means the
  peer closed its output deliberately. An EOF encountered mid-length-prefix
  or mid-payload is a truncated frame and must be treated as an error, not
  a clean shutdown, on either implementation. A worker-side EOF from the
  host's stdin *before* `Finish` has its own lifecycle meaning — see §7.

## 3. Frame format

Every message, on either direction of the control channel, is one frame:

```
┌─────────┬───────────────────────┬──────────────────────┐
│ tag: u8 │ payload_len: u32 (LE) │ payload (len bytes)   │
└─────────┴───────────────────────┴──────────────────────┘
```

- `tag` selects the message variant (table in §4).
- `payload_len` is little-endian `u32`, the exact byte length of `payload`.
- `payload` is either a small fixed binary layout (the `Band`/`BandAck`
  frames, the hot path) or a JSON document (everything else) — the tag
  alone disambiguates which, so no separate "is this JSON" flag is needed.
- Any tag byte not in the table for the direction it was read on is a
  protocol error on read (unknown-tag). Tag values are disjoint by
  direction (worker tags are small integers 1–6; host tags start at 128)
  purely as a debugging convenience for spotting a mixed-up direction —
  nothing in the codec relies on the ranges not overlapping, since each
  side only ever decodes frames it expects to receive.

## 4. Tag table

Constants live in `mpp-ipc::protocol`:

```rust
pub const IPC_PROTOCOL_VERSION: u32 = 2;
```

The tables below are the **complete, current** tag set (v1 write tags plus
the three v2 read tags); rows added in v2 are marked as such. §9 describes
the read-mode message flow these tags participate in; §5-§7 (`InitMessage`,
shm, handshake/lifecycle) describe the v1 write flow only except where noted.

### Host → worker (frames the host writes to the worker's stdin)

| tag | name           | payload encoding | payload |
|----:|----------------|-------------------|---------|
| 128 | `Init`         | JSON              | `InitMessage` — must be the very first frame |
| 129 | `Band`         | binary (20 bytes) | `channel: u32, start_row: u64, rows: u32, slot: u32` |
| 130 | `Finish`       | —                 | empty payload (`payload_len` = 0) |
| 131 | `Cancel`       | —                 | empty payload (`payload_len` = 0) |
| 132 | `ReadBandAck` **(v2)** | binary (4 bytes) | `slot: u32` — acks a worker `ReadBand` request: the host has finished consuming the pixel data the worker filled into this slot, which is now free for the worker to reuse; see §9 |

`Band` binary layout, all fields little-endian, field order exactly as
written (this order matters — it is not alphabetized or regrouped):

```
byte:   0   1   2   3   4   5   6   7   8   9  10  11  12  13  14  15  16  17  18  19
      ┌───────────────┬───────────────────────────────┬───────────────┬───────────────┐
      │    channel    │            start_row           │     rows      │     slot      │
      │    u32 LE     │             u64 LE              │    u32 LE     │    u32 LE     │
      └───────────────┴───────────────────────────────┴───────────────┴───────────────┘
```

| offset | size | field       | type   | meaning |
|-------:|-----:|-------------|--------|---------|
|      0 |    4 | `channel`   | u32 LE | which channel this band belongs to (`0..channels`) |
|      4 |    8 | `start_row` | u64 LE | first row of this band, inclusive, `0..height` |
|     12 |    4 | `rows`      | u32 LE | row count in this band (may be less than a "nominal" band height for the last band of a channel) |
|     16 |    4 | `slot`      | u32 LE | shm slot already filled by the host with this band's pixel data |

### Worker → host (frames the worker writes to its stdout)

| tag | name       | payload encoding | payload |
|----:|------------|-------------------|---------|
|   1 | `BandAck`  | binary (4 bytes)  | `slot: u32` — slot is free for reuse |
|   2 | `Progress` | JSON              | `ProgressMessage { rows_done: u64, rows_total: u64 }` |
|   3 | `Done`     | JSON              | `DoneMessage { bytes_written: u64 }`, then the worker exits 0 (on a read session, `bytes_written` is always `0` and carries no meaning — see §9) |
|   4 | `Error`    | JSON              | `ErrorMessage { message: String }`, then the worker exits nonzero |
|   5 | `ImageMeta` **(v2)** | JSON     | `ImageMeta` — the worker's authoritative geometry for a read session, sent once right after `Init`; see §9 |
|   6 | `ReadBand` **(v2)**  | binary (20 bytes) | `channel: u32, start_row: u64, rows: u32, slot: u32` — same `BandHeader` layout as `Band` above, direction reversed: the *worker* has already filled the named slot with this band's decoded pixel data and announces it; the *host* consumes it and acks; see §9 |

`BandAck` binary layout:

| offset | size | field  | type   | meaning |
|-------:|-----:|--------|--------|---------|
|      0 |    4 | `slot` | u32 LE | the slot id the host may now refill/reuse |

`Done` and `Error` are always the **last** frame the worker sends; no
further frames follow either one, and the worker's exit code is the
authoritative signal a host-side process supervisor should key off if a
`Done`/`Error` frame is somehow lost (e.g. the pipe closed mid-write).

## 5. `InitMessage` (tag 128)

```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InitMessage {
    pub protocol_version: u32,        // must equal IPC_PROTOCOL_VERSION
    pub worker_version: String,       // must equal env!("CARGO_PKG_VERSION") of the worker
    pub output_path: String,
    pub width: u64,
    pub height: u64,
    pub channels: u64,                // 1..=4 (see `alpha` below)
    pub bit_depth: String,            // OUTPUT FILE format: "f32"|"f64"|"u32"|"u16"|"u8"
    pub shm_name: String,
    pub slot_bytes: u64,              // uniform slot size; >= max band bytes
    pub slots: u32,                   // number of input slots, >= 1
    #[serde(default)]
    pub mode: SessionMode,            // "write" (default) | "read" — v2

    // ---- v3: every field below has a wire default (§10) ----
    #[serde(default = "default_band_format")]
    pub band_format: String,          // SHM PAYLOAD type: "f32"|"f64"|"u32"|"u16"|"u8"
    #[serde(default = "default_container")]
    pub container: String,            // "auto" | "classic" | "big"
    #[serde(default = "default_layout")]
    pub layout: String,               // "strips" | "tiles"
    #[serde(default = "default_tile_size")]
    pub tile_size: u32,               // power of two, 128..=1024
    #[serde(default = "default_compression")]
    pub compression: String,          // "none" | "lzw" | "deflate" | "packbits"
    #[serde(default = "default_deflate_level")]
    pub deflate_level: u8,            // 1..=9 — default 6, NOT 0
    #[serde(default)]
    pub predictor: bool,
    #[serde(default = "default_alpha")]
    pub alpha: String,                // "none" | "unassociated" | "associated"
    #[serde(default)]
    pub icc_profile: String,          // base64; "" = absent
    #[serde(default)]
    pub description: String,          // TIFF tag 270; "" = absent
    #[serde(default)]
    pub copyright: String,            // TIFF tag 33432; "" = absent
    #[serde(default)]
    pub software: String,             // TIFF tag 305; "" = absent
    #[serde(default)]
    pub x_resolution: f64,            // pixels per unit; 0 = unknown
    #[serde(default)]
    pub y_resolution: f64,
    #[serde(default = "default_resolution_unit")]
    pub resolution_unit: String,      // "inch" | "cm"
}
```

All field names are exactly as written above — no `camelCase` conversion,
these are serde's default snake_case-as-written names. `InitMessage` is
sent bare (not wrapped in an externally-tagged enum variant) — tag 128
alone identifies it, so the JSON payload is the struct's fields directly,
not `{"Init": {...}}`.

| field | type | notes |
|---|---|---|
| `protocol_version` | u32 | must equal `IPC_PROTOCOL_VERSION` (currently `3`) |
| `worker_version` | string | must equal the worker binary's `CARGO_PKG_VERSION` exactly (see §8) |
| `output_path` | string | filesystem path the worker will write the encoded output file to |
| `width` | u64 | image width in pixels |
| `height` | u64 | image height in pixels |
| `channels` | u64 | `1` (gray), `2` (gray+alpha), `3` (RGB) or `4` (RGBA); `2` and `4` require an `alpha` association |
| `bit_depth` | string | `"f32"`, `"f64"`, `"u32"`, `"u16"` or `"u8"` — sample format of the **output file**. The shm payload's own sample type is `band_format`, a separate field (see §10); before v3 there was no such field and bands were always unit-range `f32`. |
| `shm_name` | string | name of the shm segment the host already created |
| `slot_bytes` | u64 | size in bytes of **one** slot — must be large enough for the largest band the host will ever send; see §6's validation rule |
| `slots` | u32 | number of slots in the segment, `>= 1` |
| `mode` | string | **(v2)** `"write"` or `"read"` — see §9. Absent in a v1 payload, which deserializes as `"write"` for wire-compat; a v2 host always emits the field explicitly (§9.1). |
| `band_format` | string | **(v3)** default `"f32"`. Sample type of the pixel data the host places in **shm slots**: `"f32"`, `"f64"`, `"u32"`, `"u16"` or `"u8"`. Coupled to `bit_depth` by the rule in §10.1. |
| `container` | string | **(v3)** default `"auto"`. `"auto"` (classic TIFF until the payload no longer fits, unless a `.btf`/`.tf8` output extension declares BigTIFF), `"classic"` or `"big"`. |
| `layout` | string | **(v3)** default `"strips"`. `"strips"` (full-width, ~4 MiB each) or `"tiles"`. |
| `tile_size` | u32 | **(v3)** default `256`. Tile edge in pixels, a power of two in `128..=1024`. Ignored — not validated — unless `layout` is `"tiles"`. |
| `compression` | string | **(v3)** default `"none"`. `"none"`, `"lzw"`, `"deflate"` or `"packbits"`. |
| `deflate_level` | u8 | **(v3)** default **`6`**, not `0`. Level `1..=9`. Ignored — not validated — unless `compression` is `"deflate"`. |
| `predictor` | bool | **(v3)** default `false`. Horizontal differencing predictor; rejected for a floating-point `bit_depth`. |
| `alpha` | string | **(v3)** default `"none"`. `"none"`, `"unassociated"` or `"associated"`; must agree with `channels` in both directions. |
| `icc_profile` | string | **(v3)** default `""` (absent). Base64 (standard alphabet) ICC profile to embed; capped at 16 MiB decoded. |
| `description` | string | **(v3)** default `""` (absent). `ImageDescription`, TIFF tag 270. |
| `copyright` | string | **(v3)** default `""` (absent). `Copyright`, TIFF tag 33432. |
| `software` | string | **(v3)** default `""` (absent). `Software`, TIFF tag 305. |
| `x_resolution` | f64 | **(v3)** default `0.0`. Horizontal resolution in pixels per `resolution_unit`; `0` means unknown and writes no resolution tags. |
| `y_resolution` | f64 | **(v3)** default `0.0`. Vertical resolution; see `x_resolution`. |
| `resolution_unit` | string | **(v3)** default `"inch"`. `"inch"` (TIFF `ResolutionUnit = 2`) or `"cm"` — those two spellings only; any other value is **refused by name**, like every other v3 string enum, rather than falling back to inch. |

Example 1 — the minimal payload a **v2** host sends. Every v3 field is
absent, and each one takes the default in the table above, which is exactly
the v1/v2 behaviour (f32 bands, strips, no compression, no alpha, no
metadata):

```jsonc
{
  "protocol_version": 3,
  "worker_version": "0.1.0",
  "output_path": "/tmp/M31-mosaic.tf8",
  "width": 12000,
  "height": 9000,
  "channels": 3,
  "bit_depth": "f32",
  "shm_name": "/mpp-ipc-3f2a91",
  "slot_bytes": 19200000,
  "slots": 4
}
```

(`slot_bytes` above is sized for a 400-row band of a 12000-px-wide image
with `band_format` at its `"f32"` default: `400 * 12000 * 4 = 19_200_000` —
in practice the host picks a band height that fits its own memory budget and
computes `slot_bytes` from it — the value here is illustrative, not a fixed
formula the worker enforces beyond the `>=` bound in §6.)

Example 2 — a **v3** write session: native `u32` samples crossing shm
untouched, 512-pixel tiles, Deflate with a predictor, and metadata. Note
`band_format` equal to `bit_depth`, as §10.1 requires for a non-`"f32"`
band format, and `slot_bytes` sized in `u32` samples
(`400 * 12000 * 4 = 19_200_000`, four bytes per sample here as well):

```jsonc
{
  "protocol_version": 3,
  "worker_version": "0.1.0",
  "output_path": "/tmp/M31-mosaic.tf8",
  "width": 12000,
  "height": 9000,
  "channels": 4,
  "bit_depth": "u32",
  "shm_name": "/mpp-ipc-3f2a91",
  "slot_bytes": 19200000,
  "slots": 4,
  "mode": "write",
  "band_format": "u32",
  "container": "big",
  "layout": "tiles",
  "tile_size": 512,
  "compression": "deflate",
  "deflate_level": 9,
  "predictor": true,
  "alpha": "unassociated",
  "icc_profile": "",
  "description": "M31 — 4x3 mosaic",
  "copyright": "(c) 2026",
  "software": "mpp",
  "x_resolution": 300.0,
  "y_resolution": 300.0,
  "resolution_unit": "cm"
}
```

A v2-or-later host emits `mode` explicitly either way — see §9.1 for the
read-session form of this example. The C++ host
(`integration/pixinsight/host/mpp_protocol.cpp`'s `encode_init`) emits every
v3 field on every `Init`, at its default when the caller did not set it: an
omitted field and a field carrying its default value mean exactly the same
thing to the worker.

**Integer width.** Every numeric field above (`width`, `height`, `channels`,
`slot_bytes` as `u64`; `protocol_version`, `slots` as `u32`) is transmitted
as a full-width JSON integer, not a string. A C++ host must decode these
into a 64-bit (or 32-bit, matching field width) integer type directly —
never round-trip a `u64` field through `double`/`float` on the way in or
out, since JSON numbers wider than 2^53 silently lose precision through an
IEEE-754 double and this protocol's `u64` fields (`width`, `height`,
`slot_bytes`) are exactly the ones a large mosaic can push past that bound.

**`output_path` encoding.** `output_path` is a JSON string and therefore
UTF-8 on the wire, regardless of platform. On Windows the worker converts
this UTF-8 path to UTF-16 before calling into the filesystem APIs; a path
that is not representable in UTF-8 (or not round-trippable to a valid
UTF-16 Windows path) is not supported by this protocol version — the host
is responsible for only ever sending a path the worker can encode.

## 6. Shared memory: layout and ownership

**`shm_name` grammar.** The name the host chooses and sends as
`InitMessage.shm_name` must match this exact grammar, enforced by the
implementation on **every** platform (not just where it is load-bearing):
starts with a single `/`, contains no other `/`, and is at most 31
characters total including the leading `/` — the portable limit is macOS's
`PSHMNAMLEN` (`shm_open`/`shm_unlink` fail there with a bare `EINVAL`
beyond it), but the worker rejects a name violating this grammar on every
platform so a name that happens to work on Linux/Windows doesn't silently
break the first time it runs on macOS. On Windows, neither side transmits
or expects a Windows object name on the wire: both the host and the worker
independently normalize the POSIX-style `/name` from `Init` to
`Local\name` (encoded UTF-16) before calling into the Win32 named-mapping
APIs. Concretely, this means:

- The host must always put the POSIX-style form (`/name`) in `Init.shm_name`
  — on every platform, including Windows — and let each side's own
  normalization step produce the platform-appropriate object name.
- Passing an already-Windows-normalized name (e.g. `Local\name`, or
  `/Local\name`) is a bug: the worker's normalizer strips at most one
  leading `/` and prepends `Local\` unconditionally, so a pre-normalized
  input becomes a double-prefixed name (`Local\Local\name`) that the host's
  own differently-prefixed mapping never matches, and `ShmSegment::open`
  fails.

**Ownership.** The shm segment is created, sized, and unlinked by the
**host only**. The worker never creates, resizes, or unlinks it — it only
attaches (by `shm_name`) after receiving `Init`, and detaches (without
unlinking) when it exits. As a defensive measure against a crashed prior
run leaving a stale segment behind, the **creator** (host) unlinks any
existing segment of the same name before creating a fresh one; the worker
performs no such cleanup, by design, since it does not own the name.

**Layout.** The segment is one contiguous byte buffer of `slots` uniform
slots, each `slot_bytes` long:

```
total_bytes = slot_bytes * slots
slot_offset(i) = slot_bytes * i        for i in [0, slots)
```

```
┌───────────┬───────────┬─────┬───────────────┐
│  slot 0   │  slot 1   │ ... │  slot N-1     │
└───────────┴───────────┴─────┴───────────────┘
 offset 0    slot_bytes         slot_bytes*(N-1)
```

(`N` = `slots`.)

**Pixel layout inside a slot.** A slot filled for a `Band { channel,
start_row, rows, slot }` frame holds `rows * width` samples of that one
channel in the session's `InitMessage.band_format` type (v3; `"f32"` before
v3 and by default, hence `sample_bytes` = 4, 8, 4, 2 or 1 for `"f32"`,
`"f64"`, `"u32"`, `"u16"`, `"u8"`), **native-endian** (the segment is
memory-mapped raw samples, not a serialized wire format — both processes are
expected to run on the same machine/architecture), row-major, top-down:

```
index(r, x) = r * width + x        for r in [0, rows), x in [0, width)
```

where `r` is the row's offset **within the band** (`r = row - start_row`,
not an absolute image row) and `width` is `InitMessage.width`. A slot never
holds more than one band's worth of data at a time, and never mixes
channels within one band — each `Band` frame names exactly one `(channel,
start_row, rows)` run.

**Access discipline.** Offsets are always addressed as byte offsets; reads
and writes are done as element slices of the `band_format` sample type, so
every offset used must be a multiple of **the band sample width**
(`sample_bytes` above) — not a fixed 4 bytes, as it was before v3, since a
`"f64"` session slices 8-byte elements and a `"u16"` one slices 2-byte
elements. Because `slot_offset(i)` is always a multiple of `slot_bytes`,
choosing `slot_bytes` as a multiple of `sample_bytes` (in practice it will
always be far larger — one band's worth of pixels) keeps every slot
correctly aligned automatically, and the shm mapping's base address is
itself page-aligned, so an aligned offset yields a properly aligned element
pointer in the worker's mmap. The worker enforces this: a `slot_bytes` that
is zero, or not a multiple of `sample_bytes`, is rejected at `Init`.

**Slot-size validation (the MMM trap, made loud).** `slot_bytes` must
satisfy `slot_bytes >= rows_max * width * sample_bytes`, where `rows_max` is
the largest `rows` value the host will ever send in a `Band` frame and
`sample_bytes` is the width of the session's `band_format` (4 for the
`"f32"` default, so this is the pre-v3 rule unchanged for a pre-v3 host).
Concretely, for every `Band` frame the worker receives, it validates:

```
rows * width * sample_bytes <= slot_bytes
```

before touching the slot, and if that check fails it sends `Error` (naming
the offending band) and exits nonzero rather than reading or writing past
the slot boundary. This check exists because MMM's own protocol left this
exact case as a silent-corruption bug: an undersized `slot_bytes` lets a
copy overrun into the next slot, and neither shm nor the OS notices — the
corruption shows up later, far from the actual mistake, as garbled pixels
or a crash in an unrelated part of the pipeline. `mpp-ipc` refuses to repeat
that: undersizing `slot_bytes` relative to the host's own chosen band
height is a host bug, and the worker now catches it at the point of first
use instead of letting it corrupt silently.

**Concurrency.** Distinct slot ids address disjoint byte ranges by
construction, so the host may have multiple slots in flight (filled but not
yet acked) simultaneously without any risk of aliasing. A slot is "owned"
by the host from the moment it is filled and named in a `Band` frame until
the matching `BandAck` arrives; the host must not refill or reuse a slot id
before its `BandAck` is received, and the worker must not read a slot after
it has sent that slot's `BandAck`.

## 7. Handshake, lifecycle, and error rules

### 7.1 Init handshake (once per run, before anything else)

```
host                                          worker (mpp-ipc-worker)
 |-- creates shm segment "shm_name",          |
 |   total_bytes = slot_bytes * slots         |
 |-- spawns worker child, stdin/stdout piped  |
 |------------- Init (tag 128) --------------> |
 |                                             |-- checks protocol_version == IPC_PROTOCOL_VERSION
 |                                             |-- checks worker_version == env!("CARGO_PKG_VERSION")
 |                                             |-- attaches to shm "shm_name" (must match total_bytes)
 |                                             |-- opens/mmaps its staging file
 |                                     (band streaming — see 7.2)
```

`Init` must be the very first frame the worker reads from its stdin; the
worker's main loop reads and decodes exactly one frame before doing
anything else and treats anything other than `Init` there as a protocol
violation (`Error` + nonzero exit). A version mismatch on **either** field
(`protocol_version` or `worker_version`) is fatal the same way: the worker
sends `Error` and exits nonzero rather than risk misinterpreting later
frames under an incompatible layout or running with mismatched semantics.

### 7.2 Band streaming (repeats per band, any order)

```
host                                          worker
 |-- fills shm[slot_offset(slot) ..] with     |
 |   rows*width native-endian f32 samples     |
 |------------- Band (tag 129) --------------> |
 |                                             |-- validates rows*width*4 <= slot_bytes
 |                                             |-- copies shm slot into staging file at
 |                                             |   (channel, start_row..start_row+rows)
 |                                             |-- marks that range covered
 |<------------ BandAck (tag 1) ---------------|
 |-- slot is now free to refill/reuse         |
 |                                       (worker may also emit Progress (tag 2) here)
```

Bands may arrive in **any order** and **any per-frame row count** — PI's
own call order across channels and rows is not contractual, and the worker
must not assume sequential coverage. The worker tracks which `(channel,
row)` cells have been covered by some band; a `Band` frame whose range
overlaps rows already covered (by an earlier `Band` for the same channel)
is a protocol error: the worker sends `Error` (naming the overlapping
range) and exits nonzero, rather than silently accepting a second write to
already-written pixels (which would either mask a host bug or double-charge
a coverage count).

### 7.3 Finish (end of a successful run)

```
host                                          worker
 |------------- Finish (tag 130) ------------> |
 |                                             |-- checks every (channel, row) is covered
 |                                             |-- streams staging planes through the encoder
 |                                             |   (as of this version, no further Progress is
 |                                             |    emitted during encoding itself — the last
 |                                             |    Progress a host sees is from band streaming)
 |<-------------- Done (tag 3) -----------------|
 |                                             |-- exits 0
```

If coverage is incomplete when `Finish` arrives, the worker sends `Error`
naming the **first** uncovered `(channel, row)` pair it finds (scanning
channel-major, then row-major, so the error is deterministic and
reproducible across runs) and exits nonzero — it does not attempt to encode
a partially-covered image.

### 7.4 Cancel

`Cancel` (tag 131) is only guaranteed to be observed if the host sends it
**before** it sends `Finish`. In that window, the worker responds with
best-effort cleanup — deleting its staging file and any partial output
file already written — then sends `Error { message: "cancelled" }` and
exits nonzero. A host should treat that specific message as an intentional
stop, not a crash.

**After `Finish`, `Cancel` has no effect and must not be relied on.** Once
the worker reads a `Finish` frame it stops reading stdin entirely — it
breaks out of its `Band`/`Finish`/`Cancel` read loop and moves straight to
encoding (§7.3) — so a `Cancel` written to stdin after `Finish` is never
read by the worker; it sits unread in the pipe (or, if the worker has
already exited, is simply discarded) while the run proceeds to completion
and `Done`. The run is considered **committed** as soon as `Finish` is
sent: there is no cooperative abort path past that point.

If a host genuinely needs to abort an in-progress encode after sending
`Finish`, the only mechanism is killing the worker process outright. This
is safe in the sense that matters for host-side bookkeeping — the worker's
exit code is still the authoritative completion signal (§4), and a killed
process's nonzero (or signal-terminated) exit status is unambiguous — but
it bypasses the worker's own in-process cleanup: `Staging`'s cleanup runs
via that type's `Drop` impl (or the explicit `cleanup()` path on the
`Cancel`/host-gone arms), and neither runs if the process is killed rather
than allowed to unwind. Concretely, a `kill()`-based abort after `Finish`
can leave the staging scratch file behind on disk — it is named
`<output-filename>.mpp-stage-<worker-pid>` and lives next to the output
file (same directory as `output_path`). A host that kills a worker for
this reason should, on a best-effort basis, delete any
`<output-filename>.mpp-stage-*` file it finds next to the output path
afterward.

### 7.5 Host-gone (stdin EOF before Finish)

If the worker's stdin reaches EOF before a `Finish` frame has been
received, that means the host is gone (crashed, or closed the pipe without
finishing the handshake). The worker treats this identically to `Cancel`'s
cleanup path — staging file and partial output deleted — then exits
nonzero. **No frame is written** in this case: unlike `Cancel`, there is no
host left to read one, so the worker skips straight to cleanup and exit. A
host-side process supervisor detects this run's failure via the worker's
nonzero exit code alone, not via any terminal frame.

### 7.6 Post-terminal-frame discipline

The worker never touches the shm segment after sending `Done` or `Error` —
neither frame is followed by any further slot read or write, so the host is
free to unlink the segment as soon as it observes either frame (or the
worker's process exit, whichever it detects first) without any risk of a
racing access from the worker side.

### 7.7 Host receiving obligations

A conforming host must handle the following implemented behaviors on the
worker→host direction; a host that assumes otherwise (e.g. that `Progress`
only ever appears in one specific spot, or that a zero-row `Band` is
malformed) will misbehave against the real worker:

- **`Progress` may arrive at any point before the terminal frame.** The
  worker emits `Progress` during band streaming (§7.2, after a `BandAck`);
  as of this protocol version it does not emit further `Progress` frames
  during encoding itself (§7.3) — the last `Progress` a host sees is from
  band streaming, and then `Done`/`Error` follows once encoding finishes.
  A host's read loop must still tolerate an interleaved `Progress` frame
  between any two frames it is otherwise waiting on (a `BandAck`, or the
  final `Done`/`Error`) rather than treating an unexpected `Progress` as a
  protocol violation at a specific point in the exchange — a future
  protocol version may start emitting encode-time `Progress` without that
  being a breaking wire-format change, precisely because hosts are already
  required to tolerate it anywhere.
- **`BandAck`s arrive in band-processing order.** The worker processes
  `Band` frames strictly one at a time, synchronously, off a single read
  loop — it acks band *N* before it reads band *N+1* — so the sequence of
  `BandAck`s on stdout is always in the same order as the `Band` frames the
  host sent on stdin. A host may rely on this to match acks to sends
  positionally without needing to correlate on `slot` (though `slot` is
  still the field that identifies which slot is free, per §4/§6).
- **A `rows: 0` `Band` is accepted as a no-op.** A `Band` frame with
  `rows: 0` passes slot-size validation trivially (`0 <= slot_bytes`),
  contributes no coverage for its `(channel, start_row)`, and is still
  acked normally with a `BandAck`. This is not a documented *host*
  strategy to rely on for anything (a real band should always carry
  `rows >= 1`), but a host that happens to construct one (e.g. from an
  empty tile at a mosaic edge) will not get an `Error` for it.
- **No fixed wall-clock timeout waiting for `Done`.** A host must not
  impose a fixed timeout on the interval between sending `Finish` and
  receiving `Done` — encoding a large image (streaming the staged planes
  through the BigTIFF encoder) can legitimately take far longer than any
  single band-transfer step, and that duration scales with image size in a
  way no fixed constant can safely bound. Since (as just noted) this
  version sends no `Progress` during that specific interval, the health
  signal available to a host here is the worker process's own liveness
  (still running, not defunct/zombie) rather than any frame; a host should
  treat the encode phase as healthy for as long as the process is alive
  and reserve any hang detection for process-level signals, not elapsed
  time since `Finish`.

## 8. Versioning

- **`IPC_PROTOCOL_VERSION`** (currently `3`) bumps on **any** wire-format
  change — a new tag, a reordered/resized binary field, a renamed or
  retyped JSON field, a changed lifecycle rule that a C++ host would need
  to branch on. Bump the constant in `mpp-ipc::protocol` and this document
  together, in the same commit, and describe what changed at the top of
  this section.
- **`worker_version`** is not a protocol version — it's the worker binary's
  own release version, `env!("CARGO_PKG_VERSION")` of the `mpp-ipc-worker`
  crate. The module and worker ship as one package, so the host stamps its
  own compiled-in expectation of this value into every `Init` it sends, and
  the worker checks it **exactly** (not semver-compatibly): two builds can
  agree on `IPC_PROTOCOL_VERSION` yet still differ in some semantic detail
  neither side can detect once bands start flowing (e.g. a stale worker
  binary left behind by a partial install). An exact-string mismatch is
  refused the same way a protocol-version mismatch is — `Error` + nonzero
  exit — even though the wire format itself would have worked.
- **Keeping the C++ host's expected `worker_version` in sync.** The Plan-B
  C++ host stamps its own compiled-in expectation of `worker_version` into
  every `Init` it sends (previous bullet). The intent is that this
  expected-version constant is **generated from the workspace version at
  build time**, not hand-maintained as a separately-edited literal in the
  C++ source: the single source of truth is the Cargo workspace version
  (`version.workspace = true`, shared by every crate including
  `mpp-ipc-worker`, whose `WORKER_VERSION` is `env!("CARGO_PKG_VERSION")`
  of that same value), and the C++ build should read/derive its constant
  from there rather than a human copying the version string into two
  places by hand and letting them drift. This generation step is not yet
  implemented (there is no C++ host yet) — this paragraph records the
  intent for whoever wires up the Plan-B build so the mechanism is chosen
  before hand-maintained drift becomes the default.

### Version history

| `IPC_PROTOCOL_VERSION` | change |
|---:|---|
| 1 | Initial write-only host-push protocol: `Init`/`Band`/`Finish`/`Cancel` in, `BandAck`/`Progress`/`Done`/`Error` out. |
| 2 | Adds a worker-pull **read** direction alongside v1's write path (§9): `InitMessage.mode` (`"write"` default / `"read"`); worker→host `ImageMeta` (tag 5) and `ReadBand` (tag 6, reuses the `Band`/`BandHeader` binary layout, direction reversed); host→worker `ReadBandAck` (tag 132, 4-byte `slot: u32`). A v1 `Init` payload (no `mode` field) still deserializes correctly, as `"write"`. |
| 3 | Widens `InitMessage` (§10) — additive over v2 exactly as v2 was over v1. Fifteen new `Init` fields, every one with a wire default: `band_format` (the shm payload's sample type, separate from `bit_depth`, the output file's), `container`, `layout`, `tile_size`, `compression`, `deflate_level`, `predictor`, `alpha`, `icc_profile`, `description`, `copyright`, `software`, `x_resolution`, `y_resolution`, `resolution_unit`. **No new frame tags, no change to the `Band`/`BandHeader` binary layout, no lifecycle change.** A v2 `Init` payload (none of the fifteen present) still deserializes correctly, into exactly the v2 behaviour. The one rule §6 restates: slot alignment/size is measured in the `band_format` sample width rather than a fixed 4 bytes. A later, still-v3 change widens `ImageMeta` (§9.2) the same way: one new field, `icc_profile`, `#[serde(default)]`, no version bump — the read-side mirror of `InitMessage.icc_profile` above. |

## 9. Read mode (protocol v2)

Version 2 adds a second flow, selected by `InitMessage.mode`, in which the
**worker** opens an existing TIFF/BigTIFF file and *pulls* pixel bands from
the host — the mirror image of the write flow in §1-§7, where the host
pushes bands to the worker. This section is the complete contract for that
flow; everything in §1-§3 (transport, frame format) applies unchanged to
both directions.

### 9.1 `InitMessage.mode`

`InitMessage` gains one field, `mode: SessionMode` (§5 field table), which
selects which flow this run is:

- `"write"` — the v1 host-push flow described in §5-§7. This is the
  `#[derive(Default)]` value, so a v1 `Init` JSON payload that omits `mode`
  entirely still deserializes as `"write"` — v1 hosts remain wire-compatible
  with a v2 worker with no changes.
- `"read"` — the worker opens the file at `output_path` (read mode reuses
  this field as the **input** path — there is no separate field) for
  decoding, and pulls bands from the host as described below instead of
  receiving `Band` frames.

Example `Init` for a read session:

```jsonc
{
  "protocol_version": 3,
  "worker_version": "0.1.0",
  "output_path": "/tmp/M31-mosaic.tif",
  "width": 9888,
  "height": 6568,
  "channels": 3,
  "bit_depth": "f32",
  "shm_name": "/mpp-ipc-3f2a91",
  "slot_bytes": 19200000,
  "slots": 4,
  "mode": "read"
}
```

`width`/`height`/`channels`/`bit_depth` are **not authoritative** for a read
session — the worker never reads them to decide anything, and derives its
own geometry straight from the file. A host will typically still have a
best-effort value for `width`/`channels` in hand (e.g. from a prior
`mpp-ipc-worker --probe` call, used to size `slot_bytes`, per the reference
host's `ReadSessionConfig`), but the worker's own `ImageMeta` (§9.2) is what
a host must actually trust. `shm_name`/`slot_bytes`/`slots` still describe
the shm segment exactly as in a write session; the segment itself is used
identically (§6), just filled by the worker instead of the host.

### 9.2 `ImageMeta` (tag 5, worker → host)

Immediately after a `Read`-mode `Init` handshake, before requesting any
band, the worker attaches to the shm segment, opens the file, and sends one
`ImageMeta` frame: the file's own geometry, derived from what the worker
actually decoded rather than anything the host may have guessed in `Init`.

```rust
pub struct ImageMeta {
    pub width: u64,
    pub height: u64,
    pub channels: u64,      // total channel count, including alpha
    pub alpha: u64,         // 1 if `channels` includes an alpha channel, 0 otherwise
    pub color_space: String, // "gray" | "rgb"
    pub bit_depth: String,   // "u8" | "u16" | "u32" | "f32" | "f64"
    pub icc_profile: String, // base64; "" = absent (default)
}
```

Example:

```jsonc
{
  "width": 9888,
  "height": 6568,
  "channels": 3,
  "alpha": 0,
  "color_space": "rgb",
  "bit_depth": "f32",
  "icc_profile": ""
}
```

Unlike the write path's `InitMessage.bit_depth` (an *output* format the host
chose), `ImageMeta.bit_depth` is the file's *native* sample format — the
worker never converts samples on read, so a host must be prepared to handle
any of the five listed values, not just the three the write path advertises
(`u8`/`u16`/`f32`).

`icc_profile` is the mirror image of `InitMessage.icc_profile` (§10): the
same standard-alphabet base64 encoding, but here it is the worker reporting
what it found embedded in the file (TIFF tag 34675), not the host asserting
what to write. Most astro TIFFs carry no profile, so `""` is the common
case. `#[serde(default)]`, so a worker built before this field existed still
produces an `ImageMeta` a current host can parse, just always reporting `""`.

`mpp-ipc-worker --probe <path>` prints this same JSON shape standalone, with
no shm/handshake involved, for host-side or manual inspection of a file's
geometry ahead of a real read session.

### 9.3 Band pull loop: `ReadBand` (tag 6) / `ReadBandAck` (tag 132)

Once `ImageMeta` has been sent, the worker drives the exchange — the host
only ever responds to a request, it never sends a band unprompted:

```
host                                          worker (read mode)
                                               |-- attaches to shm, opens file
                                     <---------|-- ImageMeta (tag 5)
                                               |-- decodes one band into an
                                               |   shm slot
                                     <---------|-- ReadBand (tag 6): channel,
                                               |   start_row, rows, slot
 |-- reads shm[slot_offset(slot)..],          |
 |   rows*width native-sample-format          |
 |   samples of that channel                  |
 |------------- ReadBandAck (tag 132) ------->|
                                               |-- slot is now free to refill
                                               |                    (repeats)
                                     <---------|-- Done (tag 3)
```

`ReadBand`'s payload is the same 20-byte `BandHeader` layout as the write
path's `Band` frame (§4's field table applies unchanged: `channel: u32,
start_row: u64, rows: u32, slot: u32`) — only the direction is reversed, and
`slot` now names a slot the *worker* has just filled, not one the host
filled. The samples in the slot are the file's **native** sample format
named by `ImageMeta.bit_depth` (not always `f32`, unlike the write path's
shm convention in §6 — a read session's slot contents match whatever
`ImageMeta` reported), native-endian, row-major, laid out identically to
§6's "Pixel layout inside a slot" otherwise (`rows * width` samples of one
channel, `index(r, x) = r * width + x`).

`ReadBandAck`'s payload is a bare 4-byte little-endian `slot: u32` — the
slot id the host has finished consuming, now free for the worker to reuse.

**Band geometry is the worker's choice, bounded by `slot_bytes`.** A
`ReadBand` frame does **not** correspond one-to-one to a file chunk (a strip
or a tile-row): a single decoded chunk routinely exceeds one slot — a
single-strip image has `RowsPerStrip == height`, so its whole channel is one
chunk — so the worker splits each chunk into as many `ReadBand` sub-bands of
whole rows as needed to fit `slot_bytes` (`rows * width * sample_bytes <=
slot_bytes` for every frame), each with contiguous `start_row`/`rows` and its
own slot and ack. A host must therefore treat every `ReadBand` as an
independent `(channel, start_row, rows)` scatter and must not assume any
particular banding; the reference host and module already do (they scatter by
those fields alone). The only geometry guarantee is that a band is always
whole rows of a single channel, so `slot_bytes` smaller than one full row
(`width * sample_bytes`) is a host misconfiguration the worker rejects up
front with an `Error`.

**Inverted slot ownership.** This is the one rule that differs from §6's
ownership model, and the reason read mode needs its own ack tag rather than
reusing `BandAck`: on write, the *host* fills a slot and the *worker* acks
it free; on read, the *worker* fills a slot and the *host* acks it free. A
slot is "owned" by the worker from the moment it names that slot in a
`ReadBand` frame until the matching `ReadBandAck` arrives; the worker must
not refill that slot before the ack, and the host must not read a slot
after it has sent that slot's `ReadBandAck`. Everything else about slot
addressing, alignment, and concurrency (§6) applies unchanged — distinct
slot ids still address disjoint byte ranges, so multiple `ReadBand` requests
may be outstanding at once, up to `slots` of them.

### 9.4 Completion: `Done`/`Error`, and `bytes_written`

Once the worker has requested every band of every channel, it sends `Done`
(tag 3) and exits 0, exactly as on the write path (§4, §7.3). **On a read
session, `DoneMessage.bytes_written` is always `0` and carries no
meaning** — there is no output file being written, so the field is present
only because `Done` is one shared message shape for both directions; a host
must not interpret it as a byte count, row count, or any other read-session
statistic.

`Error` (tag 4) is the worker-sent failure frame, exactly as in §4/§7:
`ErrorMessage { message }`, then a nonzero exit. On a read session the
worker sends it for a decode failure (a corrupt chunk, an I/O error re-
reading the file), a link I/O failure, or — per §9.3 — a host protocol
violation such as receiving anything other than a `ReadBandAck` while
waiting on a free slot.

Two validations a host must itself perform are **not** the worker's
responsibility to police, and a misbehaving host will not get an `Error`
frame for them: a conforming host validates every incoming `ReadBand`
request's `slot` (must be `< slots`) and `rows * width * sample_bytes`
(must be `<= slot_bytes`) itself before touching the slot, exactly
mirroring the worker's own slot-size validation on the write path (§6's
"the MMM trap, made loud") — a violation here is a **worker** bug (it
should never happen against a correct decoder), and the reference host
implementation treats it as fatal to the whole session (kill the worker,
raise locally) rather than expecting any cooperative `Error` exchange.

### 9.5 What's unchanged — and what isn't — from the write path

§2 (transport), §3 (frame format), §6's shm layout/alignment/ownership-shape
(only the *direction* of ownership inverts, per §9.3), §7.1's version-check
half of the `Init` handshake, §7.5's host-gone rule, §7.6's post-terminal-
frame discipline, and §8's versioning discipline all apply to a read session
exactly as written for a write session, substituting "worker fills, host
consumes and acks" for "host fills, worker consumes and acks" wherever §6/§7
describe slot filling. Concretely for host-gone: if the worker's stdin
reaches EOF while
it is still waiting on a `ReadBandAck`, the worker's host-gone handling
short-circuits the band-pull loop exactly as it does the write path's
`Band` loop — no frame is written, and the worker exits nonzero.

**§7.4 (`Cancel`) does *not* apply to a read session.** There is no
cooperative abort path for read mode in this protocol version: the host
library's `ReadSession` (unlike `WriteSession`) has no `Cancel()` method and
never sends tag 131, and the worker's read-mode loop has no `Cancel`
handling of its own — a stray `Cancel` frame received mid-read is treated as
an unexpected event (a protocol error, `Error` + nonzero exit), not the
graceful "cancelled" cleanup §7.4 describes for a write session. This is a
deliberate scope cut, not an oversight: a read session has no
partially-written output file to abandon the way a write session does, so
the write path's motivating case for `Cancel` doesn't carry over. If a host
needs to abort an in-progress read, killing the worker process is the only
mechanism, exactly as §7.4 already describes for a write session aborted
after `Finish`.

## 10. Native band formats and write options (protocol v3)

Version 3 is **purely additive over v2, exactly as v2 was over v1**: fifteen
new `InitMessage` fields, every one with a wire default. No new frame tags.
No change to the `Band`/`BandHeader` 20-byte binary layout, or to any other
frame's shape. No lifecycle change — the handshake, band streaming,
`Finish`, `Cancel`, host-gone, and post-terminal-frame rules of §7 are
untouched. A v2 host's `Init` payload, carrying none of the fifteen, still
deserializes into precisely the v2 behaviour; §5's Example 1 is that
payload.

Two things motivated the version. First, **`f32` was the only sample type a
band could cross shm as**, and a unit-range `f32`'s 24-bit mantissa cannot
carry a `u32` value or a `f64` one: an image already held natively in either
was silently degraded on its way to a file that could have stored it
exactly. Second, the worker exposed **none** of `mpp-core`'s TIFF write
options over the wire, so a host could not ask for tiles, compression, an
alpha association, an ICC profile or any metadata, however plainly the user
had asked for them.

### 10.1 `band_format` vs `bit_depth`, and the rule coupling them

These are two different fields describing two different things, and the
distinction is the whole point of the version:

- **`band_format` describes the shm payload** — the sample type the *host*
  writes into slots and the worker reads back out of them (§6's "Pixel
  layout inside a slot"). It is what `slot_bytes` is measured in.
- **`bit_depth` describes the output file** — the sample format the worker
  encodes into the TIFF.

Before v3 there was no `band_format`: the shm payload was always unit-range
`f32`, and `bit_depth` alone said what the file would hold, with the worker
converting. That behaviour is still exactly what a session gets when
`band_format` is `"f32"` (its default).

**Coupling rule.** `band_format` must equal `bit_depth`, **unless**
`band_format` is `"f32"`.

The reason is not stylistic. A native (non-`"f32"`) band is written to the
file **verbatim** — that is what makes it lossless, and it means no
conversion is applied, or available: `mpp-core` has one converting encode
path and its input domain is unit-range `f32`. So `"f32"` bands may target
any `bit_depth` (the v1/v2 freedom, preserved), while a `"u16"` band can
only produce a `"u16"` file. The worker rejects a violating `Init` up front,
before it stages a single band — `Error` + nonzero exit, naming
`band_format`. A mirroring host should reproduce the check locally so the
user sees it before a process is spawned and a segment created; the
reference C++ host does, in `WriteSession`'s constructor.

| `band_format` | `bit_depth` | result |
|---|---|---|
| `"f32"` (default) | any of the five | accepted — the converting path, v1/v2 behaviour |
| `"f64"`/`"u32"`/`"u16"`/`"u8"` | the same string | accepted — samples written verbatim |
| `"f64"`/`"u32"`/`"u16"`/`"u8"` | anything else | **rejected at `Init`** |

`band_format` also sets the shm sample width used by §6's alignment and
slot-size rules: `slot_bytes` must be a nonzero multiple of that width, and
at least one row of it (`width * sample_bytes`).

### 10.2 Ignored fields are genuinely ignored

`tile_size` is consulted only when `layout` is `"tiles"`; `deflate_level`
only when `compression` is `"deflate"`. When they do not apply they are
**not validated** — an out-of-range value in an inapplicable field can never
fail an `Init`. This matters for hosts that carry a fixed struct of options:
a host that always sends `tile_size: 256` on a strip session, or a
`deflate_level` it has no UI for, must not be refused for it.

The converse obligation is on the defaults. Because an omitted field and a
field carrying its default value are indistinguishable to the worker, a
host's own compiled-in default **must equal the wire default** in §5's
table, or that host silently asks for something a v2 host would not have.
`deflate_level` is the trap: the natural "unset" value for an integer field
is `0`, and the wire default is `6`.

### 10.3 What a v3 host gains, and what it must still get right

The remaining fourteen fields are a straight pass-through to
`mpp-core`'s `TiffWriteOptions`, and `TiffWriteOptions::validate` — the same
validator the `mpp convert` CLI path goes through — rules on every
combination, so the legality rules live in exactly one place rather than
being restated here. The ones with cross-field consequences a host should
know about:

- `alpha` must agree with `channels` in **both** directions: `2` and `4`
  channels require an association (`"unassociated"` or `"associated"`), and
  `1` and `3` channels require `"none"`.
- `predictor` is rejected for a floating-point `bit_depth` (`"f32"`,
  `"f64"`) — horizontal differencing is defined for integer samples.
- `container: "classic"` fails validation when the payload cannot fit
  32-bit offsets; `"auto"` picks classic only while it fits, and a `.btf`
  or `.tf8` output extension declares BigTIFF regardless.
- `icc_profile` is standard-alphabet base64 and is capped at **16 MiB
  decoded**; the reference C++ host checks that bound itself, before
  spawning a worker, so an oversized profile is refused without an OS
  resource ever being created.
- `x_resolution`/`y_resolution` of `0` mean *unknown* and write no
  resolution tags at all — `0` is a sentinel, not a resolution.
