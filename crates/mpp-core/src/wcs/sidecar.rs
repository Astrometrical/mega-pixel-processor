//! The canonical WCS JSON sidecar (schema `mpp-wcs/1`).
//!
//! One schema used by every container that cannot carry an astrometric
//! solution natively (TIFF, PNG, DZI, …), and consumed by the DZI viewer.
//! Written next to the image as `<stem>.wcs.json`.
//!
//! # Schema v1
//!
//! ```json
//! {
//!   "schema": "mpp-wcs/1",
//!   "width": 16384,
//!   "height": 12288,
//!   "wcs": {
//!     "crval": [84.199, -3.240],
//!     "crpix": [8192.5, 6144.5],
//!     "cd": [[-4.4368e-4, 0.0], [0.0, 4.4368e-4]],
//!     "ctype": ["RA---TAN", "DEC--TAN"],
//!     "radesys": "ICRS"
//!   }
//! }
//! ```
//!
//! Conventions (fixed for schema v1):
//! - `crval` in degrees `[RA, Dec]`; `cd` in degrees/pixel, row 0 → axis 1.
//! - `crpix` is FITS-convention: 1-based, pixel centers at integers, y
//!   counted in *stored row order* (top-down) of the image the sidecar
//!   accompanies.
//! - `width`/`height` are the pixel geometry of that image, letting
//!   consumers detect a stale sidecar after a resize.
//! - Angular units are always degrees; no `cunit` field.
//! - The `schema` string is a major version: readers must reject majors
//!   they do not know. Additive fields (e.g. a future distortion grid) do
//!   not bump the major.

use std::path::{Path, PathBuf};

use crate::wcs::LinearWcs;
use crate::{Error, Result};

/// Schema identifier written to and required from every v1 sidecar.
pub const SCHEMA: &str = "mpp-wcs/1";

/// A parsed (or to-be-written) WCS sidecar file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WcsSidecar {
    /// Schema identifier; [`SCHEMA`] for files this version writes.
    pub schema: String,
    /// Width in pixels of the image this sidecar describes.
    pub width: u64,
    /// Height in pixels of the image this sidecar describes.
    pub height: u64,
    /// The astrometric solution.
    pub wcs: LinearWcs,
}

impl WcsSidecar {
    /// Build a v1 sidecar for an image of the given pixel geometry.
    pub fn new(width: u64, height: u64, wcs: LinearWcs) -> Self {
        Self {
            schema: SCHEMA.to_string(),
            width,
            height,
            wcs,
        }
    }

    /// Serialize to pretty JSON at `path` (overwrites).
    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| Error::format(path, format!("sidecar serialize: {e}")))?;
        std::fs::write(path, json).map_err(|e| Error::io(path, e))
    }

    /// Read and validate a sidecar; errors on unknown schema versions.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let s: Self = serde_json::from_str(&text)
            .map_err(|e| Error::format(path, format!("sidecar parse: {e}")))?;
        if s.schema != SCHEMA {
            return Err(Error::format(
                path,
                format!(
                    "unknown sidecar schema '{}' (expected '{SCHEMA}')",
                    s.schema
                ),
            ));
        }
        Ok(s)
    }
}

/// The sidecar path for an output image: the last extension is replaced
/// with `wcs.json` (`mosaic.tif` → `mosaic.wcs.json`).
pub fn sidecar_path(output: &Path) -> PathBuf {
    output.with_extension("wcs.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wcs::LinearWcs;
    use std::path::Path;

    fn wcs() -> LinearWcs {
        LinearWcs {
            crval: [84.199, -3.240],
            crpix: [100.5, 200.5],
            cd: [[-4.4e-4, 0.0], [0.0, 4.4e-4]],
            ctype: ["RA---TAN".into(), "DEC--TAN".into()],
            radesys: "ICRS".into(),
        }
    }

    #[test]
    fn sidecar_path_replaces_last_extension() {
        assert_eq!(
            sidecar_path(Path::new("/out/mosaic.tif")),
            Path::new("/out/mosaic.wcs.json")
        );
        assert_eq!(
            sidecar_path(Path::new("m31.v2.tiff")),
            Path::new("m31.v2.wcs.json")
        );
        assert_eq!(sidecar_path(Path::new("bare")), Path::new("bare.wcs.json"));
    }

    #[test]
    fn json_shape_is_the_canonical_schema() {
        let s = WcsSidecar::new(16384, 12288, wcs());
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(v["schema"], "mpp-wcs/1");
        assert_eq!(v["width"], 16384);
        assert_eq!(v["height"], 12288);
        assert_eq!(v["wcs"]["crval"][0], 84.199);
        assert_eq!(v["wcs"]["ctype"][0], "RA---TAN");
        assert_eq!(v["wcs"]["cd"][1][1], 4.4e-4);
        assert_eq!(v["wcs"]["radesys"], "ICRS");
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mpp-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.wcs.json");
        let s = WcsSidecar::new(100, 50, wcs());
        s.write(&path).unwrap();
        assert_eq!(WcsSidecar::read(&path).unwrap(), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_rejects_unknown_schema() {
        let dir = std::env::temp_dir().join(format!("mpp-sidecar-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.wcs.json");
        let mut s = WcsSidecar::new(1, 1, wcs());
        s.schema = "mpp-wcs/99".into();
        s.write(&path).unwrap();
        assert!(WcsSidecar::read(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
