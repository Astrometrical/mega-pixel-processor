# mpp IPC wire protocol (v1)

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
- `crates/mpp-ipc/src/testhost.rs` — `run_host`, a reference implementation
  of the host side (test-only, feature `testkit`, but protocol-faithful — a
  useful cross-check while implementing the C++ host) (Task 6).

This protocol is a direct descendant of `mmm-ipc`'s own wire protocol
(`~/dev/MergeMosaic/integration/pixinsight/PROTOCOL.md`) — same frame
format, same shm-ownership rules, same versioning discipline — slimmed down
because this direction only needs pixels pushed one way.

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
  direction (worker tags are small integers 1–4; host tags start at 128)
  purely as a debugging convenience for spotting a mixed-up direction —
  nothing in the codec relies on the ranges not overlapping, since each
  side only ever decodes frames it expects to receive.

## 4. Tag table

Constants live in `mpp-ipc::protocol`:

```rust
pub const IPC_PROTOCOL_VERSION: u32 = 1;
```

### Host → worker (frames the host writes to the worker's stdin)

| tag | name     | payload encoding | payload |
|----:|----------|-------------------|---------|
| 128 | `Init`   | JSON              | `InitMessage` — must be the very first frame |
| 129 | `Band`   | binary (20 bytes) | `channel: u32, start_row: u64, rows: u32, slot: u32` |
| 130 | `Finish` | —                 | empty payload (`payload_len` = 0) |
| 131 | `Cancel` | —                 | empty payload (`payload_len` = 0) |

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
|   3 | `Done`     | JSON              | `DoneMessage { bytes_written: u64 }`, then the worker exits 0 |
|   4 | `Error`    | JSON              | `ErrorMessage { message: String }`, then the worker exits nonzero |

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
    pub channels: u64,                // 1 or 3
    pub bit_depth: String,            // "f32" | "u16" | "u8"
    pub shm_name: String,
    pub slot_bytes: u64,              // uniform slot size; >= max band bytes
    pub slots: u32,                   // number of input slots, >= 1
}
```

All field names are exactly as written above — no `camelCase` conversion,
these are serde's default snake_case-as-written names. `InitMessage` is
sent bare (not wrapped in an externally-tagged enum variant) — tag 128
alone identifies it, so the JSON payload is the struct's fields directly,
not `{"Init": {...}}`.

| field | type | notes |
|---|---|---|
| `protocol_version` | u32 | must equal `IPC_PROTOCOL_VERSION` (currently `1`) |
| `worker_version` | string | must equal the worker binary's `CARGO_PKG_VERSION` exactly (see §8) |
| `output_path` | string | filesystem path the worker will write the encoded output file to |
| `width` | u64 | image width in pixels |
| `height` | u64 | image height in pixels |
| `channels` | u64 | `1` (mono) or `3` (RGB) |
| `bit_depth` | string | `"f32"`, `"u16"`, or `"u8"` — sample format of the *output* file; input bands over shm are always native-endian `f32` regardless of this value (see §6) |
| `shm_name` | string | name of the shm segment the host already created |
| `slot_bytes` | u64 | size in bytes of **one** slot — must be large enough for the largest band the host will ever send; see §6's validation rule |
| `slots` | u32 | number of slots in the segment, `>= 1` |

Example:

```jsonc
{
  "protocol_version": 1,
  "worker_version": "0.1.0",
  "output_path": "/tmp/M31-mosaic.tf8",
  "width": 12000,
  "height": 9000,
  "channels": 3,
  "bit_depth": "f32",
  "shm_name": "/mpp-ipc-3f2a91",
  "slot_bytes": 4915200,
  "slots": 4
}
```

(`slot_bytes` above is sized for a 400-row band of a 12000-px-wide image:
`400 * 12000 * 4 = 19_200_000`... in practice the host picks a band height
that fits its own memory budget and computes `slot_bytes` from it — the
value here is illustrative, not a fixed formula the worker enforces beyond
the `>=` bound in §6.)

## 6. Shared memory: layout and ownership

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
start_row, rows, slot }` frame holds `rows * width` `f32` samples of that
one channel, **native-endian** (the segment is memory-mapped raw floats,
not a serialized wire format — both processes are expected to run on the
same machine/architecture), row-major, top-down:

```
index(r, x) = r * width + x        for r in [0, rows), x in [0, width)
```

where `r` is the row's offset **within the band** (`r = row - start_row`,
not an absolute image row) and `width` is `InitMessage.width`. A slot never
holds more than one band's worth of data at a time, and never mixes
channels within one band — each `Band` frame names exactly one `(channel,
start_row, rows)` run.

**Access discipline.** Offsets are always addressed as byte offsets; reads
and writes are done as `f32` element slices, so every offset used must be a
multiple of 4 bytes. Because `slot_offset(i)` is always a multiple of
`slot_bytes`, choosing `slot_bytes` as a multiple of 4 (in practice it will
always be far larger — one band's worth of pixels) keeps every slot 4-byte
aligned automatically, and the shm mapping's base address is itself
page-aligned, so a 4-byte-aligned offset yields a properly aligned `f32*`
in the worker's mmap.

**Slot-size validation (the MMM trap, made loud).** `slot_bytes` must
satisfy `slot_bytes >= rows_max * width * 4`, where `rows_max` is the
largest `rows` value the host will ever send in a `Band` frame. Concretely,
for every `Band` frame the worker receives, it validates:

```
rows * width * 4 <= slot_bytes
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
 |                                             |   (may emit Progress (tag 2) frames while doing so)
 |<-------------- Done (tag 3) -----------------|
 |                                             |-- exits 0
```

If coverage is incomplete when `Finish` arrives, the worker sends `Error`
naming the **first** uncovered `(channel, row)` pair it finds (scanning
channel-major, then row-major, so the error is deterministic and
reproducible across runs) and exits nonzero — it does not attempt to encode
a partially-covered image.

### 7.4 Cancel

The host may send `Cancel` (tag 131) at any point after `Init`. The worker
responds with best-effort cleanup — deleting its staging file and any
partial output file already written — then sends `Error { message:
"cancelled" }` and exits nonzero. A host should treat that specific
message as an intentional stop, not a crash.

### 7.5 Host-gone (stdin EOF before Finish)

If the worker's stdin reaches EOF before a `Finish` frame has been
received, that means the host is gone (crashed, or closed the pipe without
finishing the handshake). The worker treats this identically to `Cancel`'s
cleanup path — staging file and partial output deleted — then exits
nonzero. (There is no frame to send in this case, since the pipe the
worker would send it on is the *other* direction — worker stdout — which
may still be open; the worker still writes `Error { message: "..." }` to
its stdout on a best-effort basis before exiting, for a host that is
reading stdout from a *different* process/log even though its own stdin
write end is gone.)

### 7.6 Post-terminal-frame discipline

The worker never touches the shm segment after sending `Done` or `Error` —
neither frame is followed by any further slot read or write, so the host is
free to unlink the segment as soon as it observes either frame (or the
worker's process exit, whichever it detects first) without any risk of a
racing access from the worker side.

## 8. Versioning

- **`IPC_PROTOCOL_VERSION`** (currently `1`) bumps on **any** wire-format
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

### Version history

| `IPC_PROTOCOL_VERSION` | change |
|---:|---|
| 1 | Initial write-only host-push protocol: `Init`/`Band`/`Finish`/`Cancel` in, `BandAck`/`Progress`/`Done`/`Error` out. |
