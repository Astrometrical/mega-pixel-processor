//! XISF data blocks (XISF 1.0 spec §10, document rev 1.02): where a block's
//! bytes live, their byte order, compression and checksum.

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

#[cfg(test)]
mod tests {
    use super::*;

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
