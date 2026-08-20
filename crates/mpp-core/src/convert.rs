//! One-call conversion: open input, write output, emit the WCS sidecar.

use std::path::{Path, PathBuf};

use crate::formats::tiff::{BitDepth, TiffWriteOptions, write_bigtiff};
use crate::formats::xisf::XisfImage;
use crate::wcs::sidecar::{WcsSidecar, sidecar_path};
use crate::{Error, Result};

/// Options for [`convert`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Output sample format.
    pub bit_depth: BitDepth,
    /// Write the WCS JSON sidecar when the input carries a solution.
    pub sidecar: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            bit_depth: BitDepth::F32,
            sidecar: true,
        }
    }
}

/// What a conversion did, for CLI reporting.
#[derive(Debug, Clone)]
pub struct ConvertReport {
    /// Input/output width in pixels.
    pub width: u64,
    /// Input/output height in pixels.
    pub height: u64,
    /// Channel count.
    pub channels: u64,
    /// Whether the input carried an extractable astrometric solution.
    pub wcs_found: bool,
    /// Path of the sidecar written, if any.
    pub sidecar: Option<PathBuf>,
}

/// Case-insensitive extension of `path`, lowercased ("" when absent).
fn ext(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Convert `input` (XISF) to `output` (BigTIFF, chosen by `.tif`/`.tiff`
/// extension), writing the WCS sidecar next to the output when the input
/// carries an astrometric solution and `opts.sidecar` is on.
pub fn convert(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<ConvertReport> {
    if ext(input) != "xisf" {
        return Err(Error::format(
            input,
            "unsupported input format (XISF only in this phase)",
        ));
    }
    if !matches!(ext(output).as_str(), "tif" | "tiff") {
        return Err(Error::format(
            output,
            "unsupported output format (BigTIFF .tif/.tiff only in this phase)",
        ));
    }

    let img = XisfImage::open(input)?;
    img.advise_sequential();
    write_bigtiff(
        &img,
        output,
        &TiffWriteOptions {
            bit_depth: opts.bit_depth,
        },
    )?;

    let wcs = crate::wcs::from_xisf_properties(&img.header().properties);
    let wcs_found = wcs.is_some();
    let mut sidecar = None;
    if opts.sidecar
        && let Some(wcs) = wcs
    {
        let sc = WcsSidecar::new(img.width(), img.height(), wcs);
        let path = sidecar_path(output);
        sc.write(&path)?;
        sidecar = Some(path);
    }

    Ok(ConvertReport {
        width: img.width(),
        height: img.height(),
        channels: img.channels(),
        wcs_found,
        sidecar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::synth_xisf;
    use crate::wcs::sidecar::WcsSidecar;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-convert-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn converts_and_writes_sidecar_when_solved() {
        let dir = tmpdir("solved");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 6, 4, 1, true);
        let output = dir.join("out.tif");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();

        assert_eq!((report.width, report.height, report.channels), (6, 4, 1));
        assert!(report.wcs_found);
        let sc_path = report.sidecar.expect("sidecar written");
        assert_eq!(sc_path, dir.join("out.wcs.json"));
        let sc = WcsSidecar::read(&sc_path).unwrap();
        assert_eq!((sc.width, sc.height), (6, 4));
        // refimg [3, 2] + 0.5 pixel-convention shift.
        assert_eq!(sc.wcs.crpix, [3.5, 2.5]);
        assert!(output.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_solution_means_no_sidecar_but_conversion_succeeds() {
        let dir = tmpdir("unsolved");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 4, 4, 1, false);
        let output = dir.join("out.tif");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();
        assert!(!report.wcs_found);
        assert!(report.sidecar.is_none());
        assert!(!dir.join("out.wcs.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sidecar_can_be_disabled() {
        let dir = tmpdir("nosidecar");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 4, 4, 1, true);
        let output = dir.join("out.tif");
        let opts = ConvertOptions {
            sidecar: false,
            ..Default::default()
        };
        let report = convert(&input, &output, &opts).unwrap();
        assert!(report.wcs_found, "solution still detected");
        assert!(report.sidecar.is_none());
        assert!(!dir.join("out.wcs.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_unknown_extensions() {
        let dir = tmpdir("ext");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        assert!(convert(&input, &dir.join("out.png"), &ConvertOptions::default()).is_err());
        assert!(
            convert(
                &dir.join("in.fits"),
                &dir.join("out.tif"),
                &ConvertOptions::default()
            )
            .is_err()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
