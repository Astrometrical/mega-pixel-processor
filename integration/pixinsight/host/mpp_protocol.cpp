#include "mpp_protocol.h"

#include <cerrno>
#include <cstring>
#include <stdexcept>

namespace mpp {

namespace {

// ---- little-endian helpers -------------------------------------------

uint32_t get_u32_le(const uint8_t* buf, size_t at) {
  return static_cast<uint32_t>(buf[at]) | (static_cast<uint32_t>(buf[at + 1]) << 8) |
         (static_cast<uint32_t>(buf[at + 2]) << 16) | (static_cast<uint32_t>(buf[at + 3]) << 24);
}

uint64_t get_u64_le(const uint8_t* buf, size_t at) {
  uint64_t v = 0;
  for (int i = 7; i >= 0; --i) {
    v = (v << 8) | buf[at + static_cast<size_t>(i)];
  }
  return v;
}

void put_u32_le(std::vector<uint8_t>& buf, uint32_t v) {
  for (int i = 0; i < 4; ++i) {
    buf.push_back(static_cast<uint8_t>((v >> (8 * i)) & 0xff));
  }
}

void put_u64_le(std::vector<uint8_t>& buf, uint64_t v) {
  for (int i = 0; i < 8; ++i) {
    buf.push_back(static_cast<uint8_t>((v >> (8 * i)) & 0xff));
  }
}

// ---- full read/write over an os_handle (handle short reads/writes + EINTR) --

// Reads exactly `n` bytes into `buf`, looping over `os_read()`. Returns the
// number of bytes actually read (< n only on EOF). Throws on a hard read
// error. The EINTR retry only exists on POSIX; Windows `ReadFile` never
// returns EINTR, so a single `os_read` per iteration is the loop body there.
size_t full_read(os_handle h, uint8_t* buf, size_t n) {
  size_t total = 0;
  while (total < n) {
    long r = os_read(h, buf + total, n - total);
    if (r < 0) {
#ifndef _WIN32
      if (errno == EINTR) continue;
      throw std::runtime_error(std::string("read: ") + std::strerror(errno));
#else
      throw std::runtime_error("read: os_read failed");
#endif
    }
    if (r == 0) break;  // EOF
    total += static_cast<size_t>(r);
  }
  return total;
}

void full_write(os_handle h, const uint8_t* buf, size_t n) {
  size_t total = 0;
  while (total < n) {
    long w = os_write(h, buf + total, n - total);
    if (w < 0) {
#ifndef _WIN32
      if (errno == EINTR) continue;
      throw std::runtime_error(std::string("write: ") + std::strerror(errno));
#else
      throw std::runtime_error("write: os_write failed");
#endif
    }
    total += static_cast<size_t>(w);
  }
}

std::vector<uint8_t> build_frame(uint8_t tag, const uint8_t* payload, uint32_t len) {
  std::vector<uint8_t> buf;
  buf.reserve(5 + len);
  buf.push_back(tag);
  put_u32_le(buf, len);
  buf.insert(buf.end(), payload, payload + len);
  return buf;
}

std::vector<uint8_t> build_frame(uint8_t tag, const std::string& payload) {
  return build_frame(tag, reinterpret_cast<const uint8_t*>(payload.data()),
                      static_cast<uint32_t>(payload.size()));
}

}  // namespace

bool read_frame(os_handle h, uint8_t& tag, std::vector<uint8_t>& payload) {
  uint8_t tag_byte = 0;
  size_t n = full_read(h, &tag_byte, 1);
  if (n == 0) {
    return false;  // clean EOF before any byte of a new frame
  }

  uint8_t len_buf[4];
  if (full_read(h, len_buf, 4) != 4) {
    throw std::runtime_error("read_frame: truncated frame (EOF mid-length-prefix)");
  }
  uint32_t len = get_u32_le(len_buf, 0);

  payload.assign(len, 0);
  if (len > 0 && full_read(h, payload.data(), len) != len) {
    throw std::runtime_error("read_frame: truncated frame (EOF mid-payload)");
  }

  tag = tag_byte;
  return true;
}

void write_frame(os_handle h, uint8_t tag, const uint8_t* payload, uint32_t len) {
  std::vector<uint8_t> header;
  header.reserve(5);
  header.push_back(tag);
  put_u32_le(header, len);
  full_write(h, header.data(), header.size());
  if (len > 0) {
    full_write(h, payload, len);
  }
}

std::vector<uint8_t> BandHeader::to_bytes() const {
  std::vector<uint8_t> buf;
  buf.reserve(kWireLen);
  put_u32_le(buf, channel);
  put_u64_le(buf, start_row);
  put_u32_le(buf, rows);
  put_u32_le(buf, slot);
  return buf;
}

ImageMeta ParseImageMeta(const std::string& json) {
  auto j = nlohmann::json::parse(json);
  ImageMeta meta;
  // u64 fields decoded straight from JSON integers, never routed through
  // double, matching docs/pi-protocol.md §5's "Integer width" rule (the
  // same discipline `encode_init` follows for the host->worker direction).
  meta.width = j.at("width").get<uint64_t>();
  meta.height = j.at("height").get<uint64_t>();
  meta.channels = j.at("channels").get<uint64_t>();
  meta.alpha = j.at("alpha").get<uint64_t>();
  meta.color_space = j.at("color_space").get<std::string>();
  meta.bit_depth = j.at("bit_depth").get<std::string>();
  // (v3) Additive and optional: an older worker's ImageMeta JSON predates
  // this field entirely, so default rather than `.at()` -- the same
  // wire-compatibility rule InitMessage's own v3 fields follow, applied in
  // the worker->host direction.
  meta.icc_profile = j.value("icc_profile", std::string());
  return meta;
}

BandHeader BandHeader::from_bytes(const std::vector<uint8_t>& payload) {
  if (payload.size() != kWireLen) {
    throw std::runtime_error("BandHeader::from_bytes: expected " + std::to_string(kWireLen) +
                              " bytes, got " + std::to_string(payload.size()));
  }
  BandHeader out;
  out.channel = get_u32_le(payload.data(), 0);
  out.start_row = get_u64_le(payload.data(), 4);
  out.rows = get_u32_le(payload.data(), 12);
  out.slot = get_u32_le(payload.data(), 16);
  return out;
}

bool read_worker_frame(os_handle h, WorkerFrame& out) {
  uint8_t tag_byte = 0;
  std::vector<uint8_t> payload;
  if (!read_frame(h, tag_byte, payload)) {
    return false;  // clean EOF
  }

  switch (tag_byte) {
    case static_cast<uint8_t>(WorkerTag::BandAck): {
      if (payload.size() != 4) {
        throw std::runtime_error("read_worker_frame: BandAck expected 4 bytes, got " +
                                  std::to_string(payload.size()));
      }
      out.tag = WorkerTag::BandAck;
      out.band_ack_slot = get_u32_le(payload.data(), 0);
      break;
    }
    case static_cast<uint8_t>(WorkerTag::Progress): {
      // Bare JSON object -- fields directly, not wrapped as
      // {"Progress": {...}} (docs/pi-protocol.md §5's InitMessage note
      // applies to every worker->host JSON message the same way: plain
      // structs, not externally-tagged enum variants).
      auto j = nlohmann::json::parse(payload.begin(), payload.end());
      out.tag = WorkerTag::Progress;
      // u64 fields are decoded straight from JSON integers, never routed
      // through double, so values above 2^53 stay exact (docs/pi-protocol.md
      // §5's "Integer width" rule -- it binds every u64 field on the wire,
      // not just InitMessage's).
      out.progress.rows_done = j.at("rows_done").get<uint64_t>();
      out.progress.rows_total = j.at("rows_total").get<uint64_t>();
      // `stage` is additive (serde default "bands" on the Rust side), so an
      // older worker's payload without it is a band-phase report. Any other
      // spelling is refused by name, like every string enum on this wire.
      out.progress.stage = ProgressStage::Bands;
      if (j.contains("stage")) {
        const std::string stage = j.at("stage").get<std::string>();
        if (stage == "encode") {
          out.progress.stage = ProgressStage::Encode;
        } else if (stage != "bands") {
          throw std::runtime_error("read_worker_frame: unknown Progress stage \"" + stage + "\"");
        }
      }
      break;
    }
    case static_cast<uint8_t>(WorkerTag::Done): {
      auto j = nlohmann::json::parse(payload.begin(), payload.end());
      out.tag = WorkerTag::Done;
      out.done.bytes_written = j.at("bytes_written").get<uint64_t>();
      break;
    }
    case static_cast<uint8_t>(WorkerTag::Error): {
      auto j = nlohmann::json::parse(payload.begin(), payload.end());
      out.tag = WorkerTag::Error;
      out.error.message = j.at("message").get<std::string>();
      break;
    }
    case static_cast<uint8_t>(WorkerTag::ImageMeta): {
      out.tag = WorkerTag::ImageMeta;
      out.image_meta =
          ParseImageMeta(std::string(payload.begin(), payload.end()));
      break;
    }
    case static_cast<uint8_t>(WorkerTag::ReadBand): {
      out.tag = WorkerTag::ReadBand;
      out.read_band = BandHeader::from_bytes(payload);
      break;
    }
    default:
      throw std::runtime_error("read_worker_frame: unknown tag " + std::to_string(tag_byte));
  }
  return true;
}

std::vector<uint8_t> encode_init(const InitMessage& init) {
  // InitMessage is sent bare (its fields directly), never wrapped as
  // {"Init": {...}} -- docs/pi-protocol.md §5. Field names are exactly as
  // written, matching mpp_ipc::protocol::InitMessage's serde derive
  // (default snake_case-as-written, no renaming).
  //
  // Every u64/u32 field is assigned from its native C++ integer type
  // directly, never round-tripped through double: nlohmann::json's
  // operator= picks its unsigned-integer number type for a uint64_t/uint32_t
  // input, so the JSON encoding is an exact integer literal, honoring
  // docs/pi-protocol.md §5's "Integer width" rule even for width/height/
  // slot_bytes values above 2^53.
  nlohmann::json j;
  j["protocol_version"] = init.protocol_version;
  j["worker_version"] = init.worker_version;
  j["output_path"] = init.output_path;
  j["width"] = init.width;
  j["height"] = init.height;
  j["channels"] = init.channels;
  j["bit_depth"] = init.bit_depth;
  j["shm_name"] = init.shm_name;
  j["slot_bytes"] = init.slot_bytes;
  j["slots"] = init.slots;
  j["mode"] = init.mode;

  // ---- protocol v3 fields (docs/pi-protocol.md §10) --------------------
  // Emitted unconditionally, even at their defaults: every one of them is
  // `#[serde(default)]` on the worker side, so an omitted field and a field
  // carrying its default value mean exactly the same thing, and spelling
  // them all out keeps this encoder's output a total function of the
  // InitMessage rather than of which members happen to differ from a
  // default. `band_format` names the shm sample type; `bit_depth` above
  // names the output file's -- see InitMessage's own doc comment for the
  // coupling rule between them.
  j["band_format"] = init.band_format;
  j["container"] = init.container;
  j["layout"] = init.layout;
  j["tile_size"] = init.tile_size;
  j["compression"] = init.compression;
  // Widened to `unsigned` before assignment: uint8_t is a character type in
  // an overload set, and encoding a deflate level as a JSON *string* (or a
  // one-character string) instead of a number would be rejected by serde as
  // a type error -- but only after a process spawn. The cast keeps it a JSON
  // integer by construction.
  j["deflate_level"] = static_cast<unsigned>(init.deflate_level);
  j["predictor"] = init.predictor;
  j["alpha"] = init.alpha;
  j["icc_profile"] = init.icc_profile;
  j["description"] = init.description;
  j["copyright"] = init.copyright;
  j["software"] = init.software;
  j["x_resolution"] = init.x_resolution;
  j["y_resolution"] = init.y_resolution;
  j["resolution_unit"] = init.resolution_unit;
  return build_frame(static_cast<uint8_t>(HostTag::Init), j.dump());
}

std::vector<uint8_t> encode_band(const BandHeader& header) {
  std::vector<uint8_t> payload = header.to_bytes();
  return build_frame(static_cast<uint8_t>(HostTag::Band), payload.data(),
                      static_cast<uint32_t>(payload.size()));
}

std::vector<uint8_t> encode_finish() {
  return build_frame(static_cast<uint8_t>(HostTag::Finish), std::string());
}

std::vector<uint8_t> encode_cancel() {
  return build_frame(static_cast<uint8_t>(HostTag::Cancel), std::string());
}

std::vector<uint8_t> encode_read_band_ack(uint32_t slot) {
  std::vector<uint8_t> payload;
  put_u32_le(payload, slot);
  return build_frame(static_cast<uint8_t>(HostTag::ReadBandAck), payload.data(),
                      static_cast<uint32_t>(payload.size()));
}

}  // namespace mpp
