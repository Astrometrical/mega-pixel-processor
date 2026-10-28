//! XISF data blocks (XISF 1.0 spec §10, document rev 1.02): where a block's
//! bytes live, their byte order, compression and checksum.

use super::xisf_codec::{Compression, decompress_into};

/// Byte order of a block's uncompressed data (`byteOrder`, §10.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteOrder {
    /// Little-endian (the default).
    #[default]
    Little,
    /// Big-endian.
    Big,
}

/// Where a block's stored bytes live.
#[derive(Debug, Clone, PartialEq)]
pub enum BlockLocation {
    /// `attachment:<offset>:<size>`: `size` stored bytes at `offset` from
    /// the start of the monolithic file.
    Attachment {
        /// Byte offset from the start of the file.
        offset: u64,
        /// Stored (possibly compressed) length in bytes.
        size: u64,
    },
    /// The decoded bytes of an `inline:` or `embedded` block (still
    /// compressed, if the block is).
    Inline(Vec<u8>),
}

/// One XISF data block: location, byte order, compression and checksum.
#[derive(Debug, Clone, PartialEq)]
pub struct DataBlock {
    /// Where the stored bytes are.
    pub location: BlockLocation,
    /// Byte order of the uncompressed data.
    pub byte_order: ByteOrder,
    /// Compression, if the block is compressed.
    pub compression: Option<Compression>,
    /// Checksum of the stored bytes (for inline/embedded blocks, of the
    /// decoded binary), if declared.
    pub checksum: Option<Checksum>,
}

/// The raw attribute values that describe a block, gathered by the XML
/// walk. For `location="embedded"`, `text`, `text_encoding`, `compression`
/// and `subblocks` come from the child `<Data>` element.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BlockAttrs<'a> {
    pub location: &'a str,
    pub byte_order: Option<&'a str>,
    pub compression: Option<&'a str>,
    pub subblocks: Option<&'a str>,
    pub checksum: Option<&'a str>,
    pub text: Option<&'a str>,
    pub text_encoding: Option<&'a str>,
}

impl DataBlock {
    /// Build a block from its attributes, validating every one.
    pub(crate) fn parse(a: &BlockAttrs) -> std::result::Result<DataBlock, String> {
        use super::xisf::{base64_decode, hex_decode};
        let loc = a.location.trim();
        let decode_text = |encoding: &str, text: &str| match encoding {
            "base64" => base64_decode(text),
            "hex" => hex_decode(text),
            _ => None,
        };
        let location = if let Some(rest) = loc.strip_prefix("attachment:") {
            let mut it = rest.split(':');
            match (
                it.next().and_then(|v| v.parse().ok()),
                it.next().and_then(|v| v.parse().ok()),
                it.next(),
            ) {
                (Some(offset), Some(size), None) => BlockLocation::Attachment { offset, size },
                _ => return Err(format!("bad attachment location '{loc}'")),
            }
        } else if let Some(encoding) = loc.strip_prefix("inline:") {
            let bytes = decode_text(encoding, a.text.unwrap_or(""))
                .ok_or_else(|| format!("bad {loc} block data"))?;
            BlockLocation::Inline(bytes)
        } else if loc == "embedded" {
            let (Some(text), Some(encoding)) = (a.text, a.text_encoding) else {
                return Err("embedded block without a <Data> element and its encoding".into());
            };
            let bytes = decode_text(encoding, text)
                .ok_or_else(|| format!("bad embedded block data (encoding '{encoding}')"))?;
            BlockLocation::Inline(bytes)
        } else if loc.starts_with("url(") || loc.starts_with("path(") {
            return Err(format!(
                "distributed XISF units (url()/path() block locations) are not supported: '{loc}'"
            ));
        } else {
            return Err(format!("unsupported block location '{loc}'"));
        };
        let stored_len = match &location {
            BlockLocation::Attachment { size, .. } => *size,
            BlockLocation::Inline(b) => b.len() as u64,
        };
        let byte_order = match a.byte_order.map(str::trim) {
            None | Some("little") => ByteOrder::Little,
            Some("big") => ByteOrder::Big,
            Some(other) => return Err(format!("bad byteOrder '{other}'")),
        };
        let compression = a
            .compression
            .map(|c| Compression::parse(c, a.subblocks, stored_len))
            .transpose()?;
        let checksum = a.checksum.map(Checksum::parse).transpose()?;
        Ok(DataBlock {
            location,
            byte_order,
            compression,
            checksum,
        })
    }

    /// Length of the block's data after decompression.
    pub fn uncompressed_len(&self) -> u64 {
        match &self.compression {
            Some(c) => c.uncompressed_size,
            None => self.stored_len(),
        }
    }

    /// Length of the block as stored.
    pub fn stored_len(&self) -> u64 {
        match &self.location {
            BlockLocation::Attachment { size, .. } => *size,
            BlockLocation::Inline(b) => b.len() as u64,
        }
    }

    /// The stored bytes: a bounds-checked slice of `file` for attachments,
    /// the decoded bytes for inline/embedded blocks.
    pub fn stored<'a>(&'a self, file: &'a [u8]) -> std::result::Result<&'a [u8], String> {
        match &self.location {
            BlockLocation::Attachment { offset, size } => offset
                .checked_add(*size)
                .filter(|&end| end <= file.len() as u64)
                .map(|end| &file[*offset as usize..end as usize])
                .ok_or_else(|| {
                    format!(
                        "block at {offset} ({size} bytes) extends past end of file ({} bytes)",
                        file.len()
                    )
                }),
            BlockLocation::Inline(b) => Ok(b),
        }
    }

    /// Verify the checksum (when `verify` and one is declared) over the
    /// stored bytes, then decompress or copy into `out` (exactly
    /// [`Self::uncompressed_len`] bytes). Byte order is left to the caller.
    /// A byte-shuffled block needs `scratch` of the same length.
    pub fn decode_into(
        &self,
        file: &[u8],
        verify: bool,
        out: &mut [u8],
        scratch: Option<&mut [u8]>,
    ) -> std::result::Result<(), String> {
        self.decode_stored_into(self.stored(file)?, verify, out, scratch)
    }

    /// [`Self::decode_into`] given the block's stored bytes directly (as
    /// read by a streaming reader rather than sliced from a mapped file).
    pub fn decode_stored_into(
        &self,
        stored: &[u8],
        verify: bool,
        out: &mut [u8],
        scratch: Option<&mut [u8]>,
    ) -> std::result::Result<(), String> {
        if stored.len() as u64 != self.stored_len() {
            return Err(format!(
                "stored block is {} bytes, {} declared",
                stored.len(),
                self.stored_len()
            ));
        }
        if verify && let Some(c) = &self.checksum {
            c.verify(stored)?;
        }
        match &self.compression {
            Some(c) => decompress_into(c, stored, out, scratch),
            None if out.len() == stored.len() => {
                out.copy_from_slice(stored);
                Ok(())
            }
            None => Err(format!(
                "output buffer is {} bytes, block holds {}",
                out.len(),
                stored.len()
            )),
        }
    }

    /// [`Self::decode_into`] into a new buffer, for small (property)
    /// blocks.
    pub fn decode(&self, file: &[u8], verify: bool) -> std::result::Result<Vec<u8>, String> {
        self.decode_stored(self.stored(file)?, verify)
    }

    /// [`Self::decode`] given the block's stored bytes directly.
    pub fn decode_stored(
        &self,
        stored: &[u8],
        verify: bool,
    ) -> std::result::Result<Vec<u8>, String> {
        let n = usize::try_from(self.uncompressed_len())
            .map_err(|_| "block too large for this platform".to_string())?;
        let mut out = try_zeroed::<u8>(n)?;
        let shuffled = self
            .compression
            .as_ref()
            .is_some_and(|c| c.item_size.is_some());
        let mut scratch = if shuffled {
            Some(try_zeroed::<u8>(n)?)
        } else {
            None
        };
        self.decode_stored_into(stored, verify, &mut out, scratch.as_deref_mut())?;
        Ok(out)
    }
}

/// A checksum algorithm named in an XISF `checksum` attribute (§10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    /// SHA-1 (`sha-1` / `sha1`).
    Sha1,
    /// SHA-256 (`sha-256` / `sha256`).
    Sha256,
    /// SHA-512 (`sha-512` / `sha512`).
    Sha512,
    /// SHA3-256, FIPS 202 (`sha3-256`).
    Sha3_256,
    /// SHA3-512, FIPS 202 (`sha3-512`).
    Sha3_512,
}

impl ChecksumAlgorithm {
    /// The primary attribute name, e.g. `"sha-256"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha-1",
            Self::Sha256 => "sha-256",
            Self::Sha512 => "sha-512",
            Self::Sha3_256 => "sha3-256",
            Self::Sha3_512 => "sha3-512",
        }
    }

    fn digest_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 | Self::Sha3_256 => 32,
            Self::Sha512 | Self::Sha3_512 => 64,
        }
    }
}

/// A parsed `checksum="<algorithm>:<hex digest>"` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    /// The hashing algorithm.
    pub algorithm: ChecksumAlgorithm,
    /// The expected digest bytes.
    pub digest: Vec<u8>,
}

impl Checksum {
    /// Parse a `checksum` attribute. Both spellings of each algorithm name
    /// are accepted, and hex digits in either case (the spec writes
    /// lowercase; reading is lenient).
    pub fn parse(attr: &str) -> std::result::Result<Checksum, String> {
        let (alg, hex) = attr
            .split_once(':')
            .ok_or_else(|| format!("bad checksum attribute '{attr}'"))?;
        let algorithm = match alg.trim() {
            "sha-1" | "sha1" => ChecksumAlgorithm::Sha1,
            "sha-256" | "sha256" => ChecksumAlgorithm::Sha256,
            "sha-512" | "sha512" => ChecksumAlgorithm::Sha512,
            "sha3-256" => ChecksumAlgorithm::Sha3_256,
            "sha3-512" => ChecksumAlgorithm::Sha3_512,
            other => return Err(format!("unsupported checksum algorithm '{other}'")),
        };
        let digest = super::xisf::hex_decode(hex.trim())
            .filter(|d| d.len() == algorithm.digest_len())
            .ok_or_else(|| format!("bad {} checksum digest '{hex}'", algorithm.name()))?;
        Ok(Checksum { algorithm, digest })
    }

    /// Hash `data` and compare against the expected digest.
    pub fn verify(&self, data: &[u8]) -> std::result::Result<(), String> {
        use sha2::Digest;
        let actual: Vec<u8> = match self.algorithm {
            ChecksumAlgorithm::Sha1 => sha1::Sha1::digest(data).to_vec(),
            ChecksumAlgorithm::Sha256 => sha2::Sha256::digest(data).to_vec(),
            ChecksumAlgorithm::Sha512 => sha2::Sha512::digest(data).to_vec(),
            ChecksumAlgorithm::Sha3_256 => sha3::Sha3_256::digest(data).to_vec(),
            ChecksumAlgorithm::Sha3_512 => sha3::Sha3_512::digest(data).to_vec(),
        };
        if actual == self.digest {
            Ok(())
        } else {
            let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
            Err(format!(
                "{} checksum mismatch: expected {}, data hashes to {}",
                self.algorithm.name(),
                hex(&self.digest),
                hex(&actual)
            ))
        }
    }
}

/// A zero-filled buffer of `n` elements whose size came from a file
/// header, allocated fallibly: an absurd size becomes an error, not a
/// process abort.
pub(crate) fn try_zeroed<T: Clone + Default>(n: usize) -> std::result::Result<Vec<T>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| {
        format!(
            "cannot allocate {} bytes for an XISF data block",
            n.saturating_mul(std::mem::size_of::<T>())
        )
    })?;
    v.resize(n, T::default());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::formats::xisf_codec::{Codec, compress};

    fn attrs(location: &str) -> BlockAttrs<'_> {
        BlockAttrs {
            location,
            ..Default::default()
        }
    }

    fn sha256(data: &[u8]) -> String {
        use sha2::Digest;
        let d = sha2::Sha256::digest(data);
        format!(
            "sha-256:{}",
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }

    #[test]
    fn parses_attachment_inline_and_embedded_locations() {
        let b = DataBlock::parse(&attrs("attachment:4096:24")).unwrap();
        assert_eq!(
            b.location,
            BlockLocation::Attachment {
                offset: 4096,
                size: 24
            }
        );
        assert_eq!((b.stored_len(), b.uncompressed_len()), (24, 24));
        let b = DataBlock::parse(&BlockAttrs {
            text: Some(" AQID\n "),
            ..attrs("inline:base64")
        })
        .unwrap();
        assert_eq!(b.location, BlockLocation::Inline(vec![1, 2, 3]));
        let b = DataBlock::parse(&BlockAttrs {
            text: Some("0a0B"),
            ..attrs("inline:hex")
        })
        .unwrap();
        assert_eq!(b.location, BlockLocation::Inline(vec![10, 11]));
        let b = DataBlock::parse(&BlockAttrs {
            text: Some("BAU="),
            text_encoding: Some("base64"),
            ..attrs("embedded")
        })
        .unwrap();
        assert_eq!(b.location, BlockLocation::Inline(vec![4, 5]));
        let b = DataBlock::parse(&BlockAttrs {
            text: Some(""),
            ..attrs("inline:base64")
        })
        .unwrap();
        assert_eq!(b.uncompressed_len(), 0, "empty vector/matrix block");
        for bad in ["attachment:x:1", "attachment:1", "inline:rot13", "nowhere"] {
            assert!(DataBlock::parse(&attrs(bad)).is_err(), "{bad}");
        }
        assert!(
            DataBlock::parse(&attrs("embedded")).is_err(),
            "embedded without <Data>"
        );
    }

    #[test]
    fn distributed_locations_are_refused_by_name() {
        for loc in [
            "url(http://x/y.xisb):0x1f",
            "path(@header_dir/a.xisb):3",
            "url(file:///a)",
        ] {
            let e = DataBlock::parse(&attrs(loc)).unwrap_err();
            assert!(e.contains("distributed XISF units"), "{loc}: {e}");
        }
    }

    #[test]
    fn byte_order_parses_and_defaults_little() {
        let b = DataBlock::parse(&attrs("attachment:0:4")).unwrap();
        assert_eq!(b.byte_order, ByteOrder::Little);
        let b = DataBlock::parse(&BlockAttrs {
            byte_order: Some("big"),
            ..attrs("attachment:0:4")
        })
        .unwrap();
        assert_eq!(b.byte_order, ByteOrder::Big);
        assert!(
            DataBlock::parse(&BlockAttrs {
                byte_order: Some("middle"),
                ..attrs("attachment:0:4")
            })
            .is_err()
        );
    }

    #[test]
    fn inconsistent_compression_attributes_are_refused() {
        for (comp, sb) in [
            ("zlib:10", Some("3,5:3,5")),
            ("zstd+sh:8:0", None),
            ("zstd:8", Some("bogus")),
        ] {
            let r = DataBlock::parse(&BlockAttrs {
                compression: Some(comp),
                subblocks: sb,
                ..attrs("attachment:0:7")
            });
            assert!(r.is_err(), "{comp} {sb:?}");
        }
    }

    #[test]
    fn decode_verifies_checksum_before_decompressing() {
        let data: Vec<u8> = (0..4000u32)
            .flat_map(|i| ((i / 8) as f32).to_le_bytes())
            .collect();
        let (stored, c) = compress(Codec::Zstd, Some(4), &data, 6000);
        let mut file = vec![0u8; 100];
        file.extend_from_slice(&stored);
        let loc = format!("attachment:100:{}", stored.len());
        let comp = format!("zstd+sh:{}:4", data.len());
        let sb: String = c
            .subblocks
            .iter()
            .map(|(a, b)| format!("{a},{b}"))
            .collect::<Vec<_>>()
            .join(":");
        let block = |checksum: &str| {
            DataBlock::parse(&BlockAttrs {
                compression: Some(&comp),
                subblocks: Some(&sb),
                checksum: Some(checksum),
                ..attrs(&loc)
            })
            .unwrap()
        };
        assert_eq!(block(&sha256(&stored)).decode(&file, true).unwrap(), data);
        // A digest of the *uncompressed* data is wrong: the spec hashes the
        // stored bytes.
        let e = block(&sha256(&data)).decode(&file, true).unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
    }

    #[test]
    fn decode_skips_verification_when_disabled() {
        let file = b"0123456789".to_vec();
        let b = DataBlock::parse(&BlockAttrs {
            checksum: Some(&sha256(b"other")),
            ..attrs("attachment:2:5")
        })
        .unwrap();
        assert!(b.decode(&file, true).is_err());
        assert_eq!(b.decode(&file, false).unwrap(), b"23456");
    }

    #[test]
    fn inline_block_checksum_covers_decoded_bytes() {
        let b = DataBlock::parse(&BlockAttrs {
            text: Some("AQID"),
            checksum: Some(&sha256(&[1, 2, 3])),
            ..attrs("inline:base64")
        })
        .unwrap();
        assert_eq!(b.decode(&[], true).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn attachment_out_of_bounds_is_an_error() {
        let b = DataBlock::parse(&attrs("attachment:8:5")).unwrap();
        let e = b.decode(&[0u8; 10], true).unwrap_err();
        assert!(e.contains("past end of file"), "{e}");
    }

    const ABC: [(&str, &str); 5] = [
        ("sha-1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
        (
            "sha-256",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "sha-512",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
        (
            "sha3-256",
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532",
        ),
        (
            "sha3-512",
            "b751850b1a57168a5693cd924b6b096e08f621827444f70d884f5d0240d2712e10e116e9192af3c91a7ec57647e3934057340b4cf408d5a56592f8274eec53f0",
        ),
    ];

    #[test]
    fn checksum_known_vectors() {
        for (alg, hex) in ABC {
            let c = Checksum::parse(&format!("{alg}:{hex}")).unwrap();
            assert_eq!(c.algorithm.name(), alg);
            c.verify(b"abc").unwrap();
            let upper = Checksum::parse(&format!("{alg}:{}", hex.to_uppercase())).unwrap();
            assert_eq!(upper, c, "{alg}: hex case");
        }
        for (alias, alg) in [
            ("sha1", ChecksumAlgorithm::Sha1),
            ("sha256", ChecksumAlgorithm::Sha256),
            ("sha512", ChecksumAlgorithm::Sha512),
        ] {
            let hex = ABC.iter().find(|(n, _)| *n == alg.name()).unwrap().1;
            assert_eq!(
                Checksum::parse(&format!("{alias}:{hex}"))
                    .unwrap()
                    .algorithm,
                alg
            );
        }
    }

    #[test]
    fn checksum_mismatch_names_both_digests() {
        let c = Checksum::parse(&format!("sha-1:{}", ABC[0].1)).unwrap();
        let e = c.verify(b"abd").unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
        assert!(e.contains("sha-1"), "{e}");
        assert!(e.contains(ABC[0].1), "{e}");
        assert!(
            e.contains("cb4cc28df0fdbe0ecf9d9662e294b118092a5735"),
            "{e}"
        );
    }

    #[test]
    fn checksum_rejects_unknown_algorithm_and_bad_digests() {
        for (attr, want) in [
            (
                "md5:900150983cd24fb0d6963f7d28e17f72",
                "unsupported checksum algorithm",
            ),
            ("sha-1:zz", "checksum"),
            ("sha-1:a9993e", "checksum"),
            ("sha-1", "checksum"),
        ] {
            let e = Checksum::parse(attr).unwrap_err();
            assert!(e.contains(want), "{attr}: {e}");
        }
    }
}
