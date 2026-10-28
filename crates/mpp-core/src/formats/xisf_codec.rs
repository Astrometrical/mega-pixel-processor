//! XISF data-block compression (XISF 1.0 spec §10.6, document rev 1.02).
//!
//! A compressed block is `compression="<codec>:<uncompressed-size>[:<item-size>]"`
//! plus, optionally, `subblocks="c1,u1:c2,u2:…"`; with no `subblocks`
//! attribute the block is one subblock. Codec stream formats: `zlib` is the
//! RFC 1950 zlib stream, `lz4`/`lz4hc` the LZ4 *block* format (LZ4HC output
//! is plain LZ4 block format, so one decoder serves both), `zstd` one
//! Zstandard frame per subblock. The `+sh` variants byte-shuffle the whole
//! block before it is split, so decoding unshuffles once after every
//! subblock is decompressed and concatenated.
//!
//! Revision 2 rules: a subblock whose compressed and uncompressed lengths
//! are equal is stored raw (a legacy Revision 1 encoder may still have
//! compressed it, so decompression is tried first and used only if it
//! yields exactly the uncompressed length); every compressed subblock must
//! decompress to exactly its declared length, or the block is invalid.

/// A compression codec named in an XISF `compression` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// zlib (RFC 1950) stream.
    Zlib,
    /// LZ4 block format.
    Lz4,
    /// LZ4 high-compression; decodes as the LZ4 block format.
    Lz4Hc,
    /// Zstandard, one frame per subblock.
    Zstd,
}

impl Codec {
    fn id(self) -> &'static str {
        match self {
            Codec::Zlib => "zlib",
            Codec::Lz4 => "lz4",
            Codec::Lz4Hc => "lz4hc",
            Codec::Zstd => "zstd",
        }
    }
}

/// The parsed `compression` / `subblocks` attributes of one data block.
#[derive(Debug, Clone, PartialEq)]
pub struct Compression {
    /// The codec.
    pub codec: Codec,
    /// Byte-shuffling item size: `Some` exactly for the `+sh` codecs.
    pub item_size: Option<usize>,
    /// Length of the whole block after decompression (and unshuffling).
    pub uncompressed_size: u64,
    /// `(compressed, uncompressed)` length of each subblock, in storage
    /// order; a single entry when the block has no `subblocks` attribute.
    pub subblocks: Vec<(u64, u64)>,
}

impl Compression {
    /// Parse a `compression` attribute and an optional `subblocks` attribute
    /// for a block whose stored (compressed) length is `stored_len`.
    ///
    /// Validates the codec, the item size of `+sh` codecs (≥ 1), and that
    /// the subblock lengths sum to `stored_len` and `uncompressed_size`.
    pub fn parse(
        compression: &str,
        subblocks: Option<&str>,
        stored_len: u64,
    ) -> std::result::Result<Compression, String> {
        let bad = || format!("bad compression attribute '{compression}'");
        let (codec_s, rest) = compression.split_once(':').ok_or_else(bad)?;
        let (base, shuffled) = match codec_s.strip_suffix("+sh") {
            Some(b) => (b, true),
            None => (codec_s, false),
        };
        let codec = match base {
            "zlib" => Codec::Zlib,
            "lz4" => Codec::Lz4,
            "lz4hc" => Codec::Lz4Hc,
            "zstd" => Codec::Zstd,
            _ => return Err(format!("unsupported compression codec '{codec_s}'")),
        };
        let mut params = rest.split(':');
        let uncompressed_size: u64 = params
            .next()
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(bad)?;
        let item_size = if shuffled {
            match params.next().and_then(|v| v.trim().parse::<usize>().ok()) {
                Some(n) if n >= 1 => Some(n),
                _ => {
                    return Err(format!(
                        "compression '{compression}': byte-shuffled codecs need an item size >= 1"
                    ));
                }
            }
        } else {
            None
        };
        let subblocks = match subblocks {
            None => vec![(stored_len, uncompressed_size)],
            Some(list) => list
                .split(':')
                .map(|pair| {
                    let (c, u) = pair.split_once(',')?;
                    Some((c.trim().parse().ok()?, u.trim().parse().ok()?))
                })
                .collect::<Option<Vec<(u64, u64)>>>()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("bad subblocks attribute '{list}'"))?,
        };
        let c_sum = subblocks
            .iter()
            .try_fold(0u64, |a, s| a.checked_add(s.0))
            .ok_or("subblock compressed lengths overflow")?;
        let u_sum = subblocks
            .iter()
            .try_fold(0u64, |a, s| a.checked_add(s.1))
            .ok_or("subblock uncompressed lengths overflow")?;
        if c_sum != stored_len {
            return Err(format!(
                "subblock compressed lengths sum to {c_sum}, but the block stores {stored_len} bytes"
            ));
        }
        if u_sum != uncompressed_size {
            return Err(format!(
                "subblock uncompressed lengths sum to {u_sum}, but compression declares {uncompressed_size}"
            ));
        }
        Ok(Compression {
            codec,
            item_size,
            uncompressed_size,
            subblocks,
        })
    }

    /// The codec identifier as written in the attribute, e.g. `"zstd+sh"`.
    pub fn codec_id(&self) -> String {
        match self.item_size {
            Some(_) => format!("{}+sh", self.codec.id()),
            None => self.codec.id().to_string(),
        }
    }
}

/// Byte-shuffle `src` into `dst` with item size `item` (spec §10.6.3):
/// bytes of equal significance are grouped into contiguous runs, in
/// ascending order of significance; the trailing `len % item` bytes that do
/// not form a whole item are copied unshuffled to the end.
pub fn shuffle(src: &[u8], item: usize, dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len(), "shuffle: length mismatch");
    let n = src.len() / item.max(1);
    if item <= 1 || n == 0 {
        dst.copy_from_slice(src);
        return;
    }
    for (i, it) in src.chunks_exact(item).enumerate() {
        for (b, &byte) in it.iter().enumerate() {
            dst[b * n + i] = byte;
        }
    }
    let whole = n * item;
    dst[whole..].copy_from_slice(&src[whole..]);
}

/// Invert [`shuffle`]: rebuild items from `src`'s significance runs.
pub fn unshuffle(src: &[u8], item: usize, dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len(), "unshuffle: length mismatch");
    let n = src.len() / item.max(1);
    if item <= 1 || n == 0 {
        dst.copy_from_slice(src);
        return;
    }
    for (i, it) in dst.chunks_exact_mut(item).enumerate() {
        for (b, byte) in it.iter_mut().enumerate() {
            *byte = src[b * n + i];
        }
    }
    let whole = n * item;
    dst[whole..].copy_from_slice(&src[whole..]);
}

/// Decompress the stored bytes of a block into `out`, which must be exactly
/// `c.uncompressed_size` long.
///
/// For a byte-shuffled codec, `scratch` must be `Some` buffer of the same
/// length: subblocks decompress into it and are unshuffled into `out`.
/// Subblocks are independent, so they are decoded in parallel. Errors name
/// the codec and the failing subblock index.
pub fn decompress_into(
    c: &Compression,
    stored: &[u8],
    out: &mut [u8],
    scratch: Option<&mut [u8]>,
) -> std::result::Result<(), String> {
    if out.len() as u64 != c.uncompressed_size {
        return Err(format!(
            "output buffer is {} bytes, block decompresses to {}",
            out.len(),
            c.uncompressed_size
        ));
    }
    let stored_total: u64 = c.subblocks.iter().map(|s| s.0).sum();
    if stored.len() as u64 != stored_total {
        return Err(format!(
            "{} block stores {} bytes, its subblocks declare {stored_total}",
            c.codec_id(),
            stored.len()
        ));
    }
    match c.item_size {
        Some(item) => {
            let scratch = scratch.ok_or("byte-shuffled block needs a scratch buffer")?;
            if scratch.len() != out.len() {
                return Err("scratch buffer length differs from the output".into());
            }
            decode_subblocks(c, stored, scratch)?;
            unshuffle(scratch, item, out);
        }
        None => decode_subblocks(c, stored, out)?,
    }
    Ok(())
}

/// One subblock to decode: its index, stored bytes and output range.
type Job<'a> = (usize, &'a [u8], &'a mut [u8]);

/// Decompress every subblock of `c` from `stored` into its range of `out`,
/// spreading subblocks over the available cores.
fn decode_subblocks(
    c: &Compression,
    stored: &[u8],
    out: &mut [u8],
) -> std::result::Result<(), String> {
    let mut jobs: Vec<Job> = Vec::with_capacity(c.subblocks.len());
    let (mut src, mut dst) = (stored, out);
    for (i, &(cl, ul)) in c.subblocks.iter().enumerate() {
        let (s, s_rest) = src.split_at(cl as usize);
        let (d, d_rest) = std::mem::take(&mut dst).split_at_mut(ul as usize);
        jobs.push((i, s, d));
        src = s_rest;
        dst = d_rest;
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(jobs.len())
        .max(1);
    if threads == 1 {
        let mut dec = Decoder::new()?;
        return jobs
            .into_iter()
            .try_for_each(|(i, s, d)| dec.subblock(c.codec, i, s, d));
    }
    let mut groups: Vec<Vec<Job>> = (0..threads).map(|_| Vec::new()).collect();
    for (k, job) in jobs.into_iter().enumerate() {
        groups[k % threads].push(job);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = groups
            .into_iter()
            .map(|group| {
                scope.spawn(move || {
                    let mut dec = Decoder::new()?;
                    group
                        .into_iter()
                        .try_for_each(|(i, s, d)| dec.subblock(c.codec, i, s, d))
                })
            })
            .collect();
        let mut first_err = Ok(());
        for h in handles {
            let r = h
                .join()
                .unwrap_or_else(|_| Err("subblock decoder panicked".into()));
            if first_err.is_ok() {
                first_err = r;
            }
        }
        first_err
    })
}

/// Per-thread decoder state (a reusable zstd context).
struct Decoder {
    zstd: zstd::bulk::Decompressor<'static>,
}

/// The zstd frame magic number, little-endian.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

impl Decoder {
    fn new() -> std::result::Result<Decoder, String> {
        Ok(Decoder {
            zstd: zstd::bulk::Decompressor::new().map_err(|e| format!("zstd: {e}"))?,
        })
    }

    /// Decode subblock `i` (`src` → exactly `dst.len()` bytes).
    fn subblock(
        &mut self,
        codec: Codec,
        i: usize,
        src: &[u8],
        dst: &mut [u8],
    ) -> std::result::Result<(), String> {
        if src.len() == dst.len() {
            // Revision 2: stored raw. A Revision 1 encoder may have compressed
            // it anyway; only self-validating streams (zstd frame magic, zlib
            // header + Adler-32) are worth trying. LZ4 blocks carry no check,
            // so raw data could decode as LZ4: always copy those.
            let looks_compressed = match codec {
                Codec::Zstd => src.starts_with(&ZSTD_MAGIC),
                Codec::Zlib => {
                    src.len() >= 2
                        && src[0] & 0x0F == 8
                        && (u16::from(src[0]) << 8 | u16::from(src[1])) % 31 == 0
                }
                Codec::Lz4 | Codec::Lz4Hc => false,
            };
            if !(looks_compressed && self.decompress(codec, src, dst) == Ok(dst.len())) {
                dst.copy_from_slice(src);
            }
            return Ok(());
        }
        match self.decompress(codec, src, dst) {
            Ok(n) if n == dst.len() => Ok(()),
            Ok(n) => Err(format!(
                "{} subblock {i}: decompressed to {n} bytes, {} declared",
                codec.id(),
                dst.len()
            )),
            Err(e) => Err(format!("{} subblock {i}: {e}", codec.id())),
        }
    }

    /// Decompress `src` into `dst`, returning the decompressed length; data
    /// that would overflow `dst` is an error.
    fn decompress(
        &mut self,
        codec: Codec,
        src: &[u8],
        dst: &mut [u8],
    ) -> std::result::Result<usize, String> {
        match codec {
            Codec::Zlib => {
                let mut z = flate2::Decompress::new(true);
                match z.decompress(src, dst, flate2::FlushDecompress::Finish) {
                    Ok(flate2::Status::StreamEnd) => Ok(z.total_out() as usize),
                    Ok(_) => Err("zlib stream does not end within the declared length".into()),
                    Err(e) => Err(e.to_string()),
                }
            }
            Codec::Lz4 | Codec::Lz4Hc => {
                lz4_flex::block::decompress_into(src, dst).map_err(|e| e.to_string())
            }
            Codec::Zstd => self
                .zstd
                .decompress_to_buffer(src, dst)
                .map_err(|e| e.to_string()),
        }
    }
}

/// Test encoder: compress `data` with `codec` (byte-shuffled with
/// `shuffle_item` when given) in subblocks of `subblock_size` uncompressed
/// bytes, storing raw any subblock that does not shrink (the Revision 2
/// encoder rule). Returns the stored bytes and the matching [`Compression`].
#[cfg(test)]
pub(crate) fn compress(
    codec: Codec,
    shuffle_item: Option<usize>,
    data: &[u8],
    subblock_size: usize,
) -> (Vec<u8>, Compression) {
    let shuffled;
    let src: &[u8] = match shuffle_item {
        Some(item) => {
            let mut buf = vec![0u8; data.len()];
            shuffle(data, item, &mut buf);
            shuffled = buf;
            &shuffled
        }
        None => data,
    };
    let mut stored = Vec::new();
    let mut subblocks = Vec::new();
    for chunk in src.chunks(subblock_size.max(1)) {
        let packed = match codec {
            Codec::Zlib => {
                use std::io::Write;
                let mut e =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(chunk).unwrap();
                e.finish().unwrap()
            }
            Codec::Lz4 | Codec::Lz4Hc => lz4_flex::block::compress(chunk),
            Codec::Zstd => zstd::bulk::compress(chunk, 3).unwrap(),
        };
        let packed = if packed.len() < chunk.len() {
            packed
        } else {
            chunk.to_vec()
        };
        subblocks.push((packed.len() as u64, chunk.len() as u64));
        stored.extend_from_slice(&packed);
    }
    let c = Compression {
        codec,
        item_size: shuffle_item,
        uncompressed_size: data.len() as u64,
        subblocks,
    };
    (stored, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(Codec, Option<usize>); 8] = [
        (Codec::Zlib, None),
        (Codec::Zlib, Some(4)),
        (Codec::Lz4, None),
        (Codec::Lz4, Some(4)),
        (Codec::Lz4Hc, None),
        (Codec::Lz4Hc, Some(4)),
        (Codec::Zstd, None),
        (Codec::Zstd, Some(4)),
    ];

    /// A quantized Float32 ramp (runs of equal samples: compressible by
    /// every codec, shuffled or not), with a 3-byte tail so the block length
    /// is not a multiple of the item size.
    fn smooth(n_bytes: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..n_bytes / 4)
            .flat_map(|i| ((i / 16) as f32 / 4096.0).to_le_bytes())
            .collect();
        v.resize(n_bytes, 0xAB);
        v
    }

    /// Incompressible pseudo-random bytes.
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    fn decode(c: &Compression, stored: &[u8]) -> std::result::Result<Vec<u8>, String> {
        let n = c.uncompressed_size as usize;
        let mut out = vec![0u8; n];
        let mut scratch = c.item_size.map(|_| vec![0u8; n]);
        decompress_into(c, stored, &mut out, scratch.as_deref_mut())?;
        Ok(out)
    }

    #[test]
    fn shuffle_matches_spec_example() {
        let src = [0xa0, 0xa1, 0xa2, 0xa3, 0xb0, 0xb1, 0xb2, 0xb3, 0x77];
        let mut dst = [0u8; 9];
        shuffle(&src, 4, &mut dst);
        assert_eq!(dst, [0xa0, 0xb0, 0xa1, 0xb1, 0xa2, 0xb2, 0xa3, 0xb3, 0x77]);
        let mut back = [0u8; 9];
        unshuffle(&dst, 4, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn shuffle_is_identity_for_item_one() {
        let src = noise(17, 3);
        let mut dst = vec![0u8; 17];
        shuffle(&src, 1, &mut dst);
        assert_eq!(dst, src);
        unshuffle(&src, 1, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn round_trips_every_codec_with_and_without_shuffle() {
        let data = smooth(100_003);
        for (codec, sh) in ALL {
            for subblock in [data.len(), 20_001] {
                let (stored, c) = compress(codec, sh, &data, subblock);
                assert_eq!(c.subblocks.len(), data.len().div_ceil(subblock));
                assert!(stored.len() < data.len(), "{codec:?}{sh:?} must compress");
                assert_eq!(
                    decode(&c, &stored).unwrap(),
                    data,
                    "{codec:?} {sh:?} {subblock}"
                );
            }
        }
    }

    #[test]
    fn raw_subblocks_are_copied() {
        let data = noise(50_000, 7);
        for (codec, sh) in ALL {
            let (stored, c) = compress(codec, sh, &data, 10_000);
            assert!(
                c.subblocks.iter().all(|&(cl, ul)| cl == ul),
                "{codec:?}: all raw"
            );
            assert_eq!(decode(&c, &stored).unwrap(), data, "{codec:?} {sh:?}");
        }
    }

    #[test]
    fn mixed_raw_and_compressed_subblocks() {
        let mut data = smooth(40_000);
        data.extend(noise(40_000, 11));
        for (codec, sh) in ALL.iter().copied().filter(|(_, sh)| sh.is_none()) {
            let (stored, c) = compress(codec, sh, &data, 40_000);
            assert!(
                c.subblocks[0].0 < c.subblocks[0].1,
                "{codec:?}: first compressed"
            );
            assert_eq!(c.subblocks[1].0, c.subblocks[1].1, "{codec:?}: second raw");
            assert_eq!(decode(&c, &stored).unwrap(), data, "{codec:?}");
        }
    }

    #[test]
    fn legacy_equal_length_compressed_subblock_is_decompressed() {
        // A Revision 1 encoder may emit a compressed subblock exactly as long
        // as its uncompressed data. Simulate one: a zstd frame padded with a
        // skippable frame (magic 0x184D2A50, 4-byte length) to length u.
        let data = smooth(4_000);
        let mut frame = zstd::bulk::compress(&data, 3).unwrap();
        let pad = data.len() - frame.len() - 8;
        frame.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        frame.extend_from_slice(&(pad as u32).to_le_bytes());
        frame.resize(data.len(), 0);
        assert_eq!(frame.len(), data.len());
        let c = Compression {
            codec: Codec::Zstd,
            item_size: None,
            uncompressed_size: data.len() as u64,
            subblocks: vec![(data.len() as u64, data.len() as u64)],
        };
        assert_eq!(decode(&c, &frame).unwrap(), data);
    }

    #[test]
    fn corrupt_subblock_is_an_error() {
        let data = smooth(30_000);
        for (codec, sh) in ALL {
            let (mut stored, c) = compress(codec, sh, &data, 10_000);
            let start: u64 = c.subblocks[..2].iter().map(|s| s.0).sum();
            let (len, _) = c.subblocks[2];
            for b in &mut stored[start as usize..(start + len / 2) as usize] {
                *b ^= 0x5A;
            }
            assert!(
                len < c.subblocks[2].1,
                "{codec:?}: subblock 2 must be compressed"
            );
            let e = decode(&c, &stored).map(|_| ()).unwrap_err();
            assert!(e.contains("subblock 2"), "{codec:?}: {e}");
        }
    }

    #[test]
    fn wrong_decompressed_length_is_an_error() {
        let data = smooth(10_000);
        for (codec, sh) in ALL.iter().copied().filter(|(_, sh)| sh.is_none()) {
            let (stored, mut c) = compress(codec, sh, &data, data.len());
            c.subblocks[0].1 += 1;
            c.uncompressed_size += 1;
            let e = decode(&c, &stored).map(|_| ()).unwrap_err();
            assert!(e.contains("subblock 0"), "{codec:?}: {e}");
        }
    }

    #[test]
    fn compression_attribute_parses() {
        let c = Compression::parse("zstd+sh:400:4", None, 123).unwrap();
        assert_eq!(
            c,
            Compression {
                codec: Codec::Zstd,
                item_size: Some(4),
                uncompressed_size: 400,
                subblocks: vec![(123, 400)],
            }
        );
        assert_eq!(c.codec_id(), "zstd+sh");
        let c = Compression::parse("lz4:10", None, 9).unwrap();
        assert_eq!((c.codec, c.item_size), (Codec::Lz4, None));
        assert_eq!(c.codec_id(), "lz4");
        let c = Compression::parse("lz4hc:30", Some("5,10:10,10:4,10"), 19).unwrap();
        assert_eq!(c.subblocks, vec![(5, 10), (10, 10), (4, 10)]);
        assert_eq!(
            Compression::parse("zlib:7", None, 3).unwrap().codec,
            Codec::Zlib
        );

        for (attr, sb, stored, want) in [
            ("bzip2:10", None, 5, "unsupported compression codec"),
            ("zlib+sh:10", None, 5, "item size"),
            ("zstd+sh:10:0", None, 5, "item size"),
            ("zlib:10", Some("3,5:3,5"), 7, "subblock"),
            ("zlib:11", Some("3,5:4,5"), 7, "subblock"),
            ("zlib", None, 5, "compression"),
            ("zlib:x", None, 5, "compression"),
        ] {
            let e = Compression::parse(attr, sb, stored).unwrap_err();
            assert!(e.contains(want), "{attr} {sb:?}: {e}");
        }
    }
}
