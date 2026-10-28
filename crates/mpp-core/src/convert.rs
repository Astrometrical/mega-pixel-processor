//! One-call conversion: open input, write output, emit the WCS sidecar.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::formats::tiff_write::{BitDepth, Compression, Container, TiffWriteOptions, write_tiff};
use crate::formats::xisf::XisfImage;
use crate::wcs::sidecar::{WcsSidecar, sidecar_path};
use crate::{Error, Result};

/// Supported output containers, chosen by file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// TIFF/BigTIFF, written natively by [`crate::formats::tiff_write`].
    BigTiff,
    /// PNG via vips `pngsave` (8/16-bit).
    Png,
    /// JPEG via vips `jpegsave` (8-bit, lossy).
    Jpeg,
    /// WebP via vips `webpsave` (8-bit, lossy).
    Webp,
    /// AVIF via vips `heifsave` (8-bit, lossy).
    Avif,
    /// JPEG XL via vips `jxlsave` (8/16-bit).
    Jxl,
}

impl OutputFormat {
    /// Map a lowercase file extension to a format; `None` if unsupported.
    pub fn from_ext(ext: &str) -> Option<Self> {
        Some(match ext {
            "tif" | "tiff" => Self::BigTiff,
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "webp" => Self::Webp,
            "avif" => Self::Avif,
            "jxl" => Self::Jxl,
            _ => return None,
        })
    }

    /// The bit depth used when the caller does not choose one.
    pub fn default_bit_depth(self) -> BitDepth {
        match self {
            Self::BigTiff => BitDepth::F32,
            Self::Png | Self::Jxl => BitDepth::U16,
            Self::Jpeg | Self::Webp | Self::Avif => BitDepth::U8,
        }
    }

    /// Whether this format can carry `depth`.
    pub fn allows(self, depth: BitDepth) -> bool {
        match self {
            // BigTIFF carries every sample format the writer supports.
            Self::BigTiff => true,
            // The vips-backed formats top out at 16-bit unsigned.
            Self::Png | Self::Jxl => matches!(depth, BitDepth::U16 | BitDepth::U8),
            Self::Jpeg | Self::Webp | Self::Avif => matches!(depth, BitDepth::U8),
        }
    }

    /// Whether the format's vips saver takes a `Q` (quality) option.
    pub fn lossy_quality(self) -> bool {
        matches!(self, Self::Jpeg | Self::Webp | Self::Avif | Self::Jxl)
    }

    /// Bracketed vips saver options for this format/depth/quality, or "".
    ///
    /// JXL is the one format whose vips saver (`jxlsave`) defaults to lossy
    /// (distance 1.0) with no quality given, so when `quality` is `None` we
    /// pass `lossless=true` explicitly — an explicit `--quality` is treated
    /// as a deliberate choice to go lossy instead.
    pub fn saver_options(self, depth: BitDepth, quality: Option<u8>) -> String {
        let mut opts: Vec<String> = Vec::new();
        if self == Self::Png && depth == BitDepth::U16 {
            opts.push("bitdepth=16".to_string());
        }
        if self.lossy_quality() {
            match quality {
                Some(q) => opts.push(format!("Q={q}")),
                None if self == Self::Jxl => opts.push("lossless=true".to_string()),
                None => {}
            }
        }
        if opts.is_empty() {
            String::new()
        } else {
            format!("[{}]", opts.join(","))
        }
    }

    /// Hard per-side pixel limit of the format's encoder, where one is
    /// known (verified on vips 8.15.1): WebP 16383, JPEG 65500. `None`
    /// means no known hard limit (AVIF's is encoder-dependent).
    pub fn max_dimension(self) -> Option<u64> {
        match self {
            Self::Webp => Some(16_383),
            Self::Jpeg => Some(65_500),
            _ => None,
        }
    }
}

/// Options for [`convert`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Output sample format; `None` uses the output format's default
    /// ([`OutputFormat::default_bit_depth`]).
    pub bit_depth: Option<BitDepth>,
    /// Write the WCS JSON sidecar when the input carries a solution.
    pub sidecar: bool,
    /// Lossy-format quality 1–100 (vips `Q`); error on lossless formats.
    pub quality: Option<u8>,
    /// TIFF/BigTIFF writer options. Ignored for the vips-backed formats,
    /// which have their own saver options; `bit_depth` above is the field
    /// both paths share.
    pub tiff: TiffWriteOptions,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            bit_depth: None,
            sidecar: true,
            quality: None,
            tiff: TiffWriteOptions::default(),
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

/// Deletes the wrapped path on drop — a temp interchange file must not
/// survive any exit path, success or error. Shared by [`crate::publish`],
/// which writes its own temp interchange files (an 8-bit BigTIFF from XISF,
/// or an [`crate::vips::vips_msb`]-reduced `.v` file from 16-bit input).
pub(crate) struct TempFileGuard(pub(crate) PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Process-wide counter distinguishing concurrent temp interchange files
/// within a single process (see [`unique_temp`]).
pub(crate) static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique temp interchange path inside `dir` (same filesystem as the
/// caller's real output — multi-GB temp data must not land in a small
/// `/tmp`): `<prefix>.mpp-tmp-<pid>-<seq>.<ext>`, where the pid separates
/// concurrent processes and the process-wide [`TEMP_SEQ`] counter separates
/// multiple temp files within one process from colliding.
pub(crate) fn unique_temp(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "{prefix}.mpp-tmp-{}-{seq}.{ext}",
        std::process::id()
    ))
}

/// The interchange path for a vips-backed output: same directory as
/// `output`, named after its full file name (see [`unique_temp`]).
fn interchange_path(output: &Path) -> PathBuf {
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    let dir = output.parent().unwrap_or_else(|| Path::new(""));
    unique_temp(dir, &name, "tif")
}

/// Convert `input` (XISF) to `output`, whose extension selects the
/// [`OutputFormat`]: [`OutputFormat::BigTiff`] is written natively by
/// [`write_tiff`], while png/jpg/webp/avif/jxl are produced by writing a
/// temp BigTIFF interchange file next to the output and spawning `vips` to
/// convert it to the final container, writing the WCS sidecar next to the
/// output when the input carries an astrometric solution and `opts.sidecar`
/// is on.
pub fn convert(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<ConvertReport> {
    if ext(input) != "xisf" {
        return Err(Error::format(
            input,
            "unsupported input format (XISF only in this phase)",
        ));
    }
    let format = OutputFormat::from_ext(&ext(output)).ok_or_else(|| {
        Error::format(
            output,
            "unsupported output format (tif/tiff, png, jpg, webp, avif, jxl)",
        )
    })?;
    let depth = match opts.bit_depth {
        Some(d) => {
            if !format.allows(d) {
                return Err(Error::format(
                    output,
                    format!("{d:?} bit depth is not supported by {format:?} output"),
                ));
            }
            d
        }
        None => format.default_bit_depth(),
    };
    if opts.quality.is_some() && !format.lossy_quality() {
        return Err(Error::format(
            output,
            format!("--quality does not apply to {format:?} output"),
        ));
    }

    let mut img = XisfImage::open(input)?;
    // A compressed (or otherwise non-mappable) XISF is decoded into memory.
    img.load()?;
    if let Some(limit) = format.max_dimension()
        && (img.width() > limit || img.height() > limit)
    {
        return Err(Error::format(
            output,
            format!(
                "{format:?} output supports at most {limit} px per side (image is {}x{})",
                img.width(),
                img.height()
            ),
        ));
    }
    img.advise_sequential();
    match format {
        OutputFormat::BigTiff => {
            let tiff_opts = TiffWriteOptions {
                bit_depth: depth,
                ..opts.tiff.clone()
            };
            write_tiff(&img, output, &tiff_opts)?
        }
        _ => {
            let vips_bin = crate::vips::vips_available().ok_or_else(|| {
                Error::format(
                    output,
                    "vips binary not found on PATH; install libvips-tools \
                     (e.g. `sudo apt install libvips-tools`)",
                )
            })?;
            let tmp = interchange_path(output);
            let _guard = TempFileGuard(tmp.clone());
            // This TIFF is a private handoff to vips, never a user-visible
            // output: it must not honour the user's `--container` or
            // `--compression` choice, only the resolved sample format vips
            // needs to read back. Force BigTIFF (headroom for any image
            // size) and no compression (vips reads every container/
            // compression combination the writer can produce, but there is
            // no reason to spend cycles compressing data that is about to
            // be re-encoded).
            let tiff_opts = TiffWriteOptions {
                bit_depth: depth,
                container: Container::Big,
                compression: Compression::None,
                ..opts.tiff.clone()
            };
            write_tiff(&img, &tmp, &tiff_opts)?;
            let dest_spec = format!(
                "{}{}",
                output.display(),
                format.saver_options(depth, opts.quality)
            );
            crate::vips::vips_copy(&vips_bin, &tmp, &dest_spec, output)?;
        }
    }

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
        assert!(convert(&input, &dir.join("out.exr"), &ConvertOptions::default()).is_err());
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

    #[test]
    fn output_format_routing_and_defaults() {
        use crate::formats::tiff::BitDepth;
        assert_eq!(OutputFormat::from_ext("tiff"), Some(OutputFormat::BigTiff));
        assert_eq!(OutputFormat::from_ext("png"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::from_ext("jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::from_ext("exr"), None);

        assert_eq!(OutputFormat::BigTiff.default_bit_depth(), BitDepth::F32);
        assert_eq!(OutputFormat::Png.default_bit_depth(), BitDepth::U16);
        assert_eq!(OutputFormat::Jpeg.default_bit_depth(), BitDepth::U8);
        assert_eq!(OutputFormat::Jxl.default_bit_depth(), BitDepth::U16);

        assert!(OutputFormat::BigTiff.allows(BitDepth::U8));
        assert!(OutputFormat::Png.allows(BitDepth::U8));
        assert!(!OutputFormat::Png.allows(BitDepth::F32));
        assert!(!OutputFormat::Jpeg.allows(BitDepth::U16));
    }

    #[test]
    fn saver_options_strings() {
        use crate::formats::tiff::BitDepth;
        assert_eq!(
            OutputFormat::Png.saver_options(BitDepth::U16, None),
            "[bitdepth=16]"
        );
        assert_eq!(OutputFormat::Png.saver_options(BitDepth::U8, None), "");
        assert_eq!(
            OutputFormat::Jpeg.saver_options(BitDepth::U8, Some(85)),
            "[Q=85]"
        );
        assert_eq!(OutputFormat::Webp.saver_options(BitDepth::U8, None), "");
        assert_eq!(
            OutputFormat::Avif.saver_options(BitDepth::U8, Some(50)),
            "[Q=50]"
        );
        assert_eq!(
            OutputFormat::Jxl.saver_options(BitDepth::U16, None),
            "[lossless=true]"
        );
        assert_eq!(
            OutputFormat::Jxl.saver_options(BitDepth::U16, Some(90)),
            "[Q=90]"
        );
    }

    #[test]
    fn max_dimension_known_ceilings() {
        assert_eq!(OutputFormat::Webp.max_dimension(), Some(16_383));
        assert_eq!(OutputFormat::Jpeg.max_dimension(), Some(65_500));
        assert_eq!(OutputFormat::Png.max_dimension(), None);
        assert_eq!(OutputFormat::BigTiff.max_dimension(), None);
        assert_eq!(OutputFormat::Avif.max_dimension(), None);
        assert_eq!(OutputFormat::Jxl.max_dimension(), None);
    }

    #[test]
    fn dimension_ceilings_fail_before_writing() {
        let dir = tmpdir("limits");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 16_384, 1, 1, false); // 64 KiB of pixel data — cheap
        let err = convert(&input, &dir.join("o.webp"), &ConvertOptions::default()).unwrap_err();
        assert!(err.to_string().contains("16383"), "{err}");
        // No temp or output files were created.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "in.xisf")
            .collect();
        assert!(files.is_empty(), "unexpected files: {files:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn png_via_vips_defaults_to_16bit() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("png16");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 6, 4, 1, true);
        let output = dir.join("out.png");
        let report = convert(&input, &output, &ConvertOptions::default()).unwrap();
        assert_eq!((report.width, report.height), (6, 4));

        // PNG signature.
        let head = std::fs::read(&output).unwrap();
        assert_eq!(&head[0..4], &[0x89, b'P', b'N', b'G']);
        // 16-bit grayscale: PNG IHDR bit depth byte (offset 24) == 16.
        assert_eq!(head[24], 16, "PNG bit depth");

        // Sidecar rides along for vips-backed outputs too.
        assert!(dir.join("out.wcs.json").exists());
        // Temp interchange file cleaned up.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mpp-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn jpeg_via_vips_with_quality() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("jpeg");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 8, 8, 3, false);
        let output = dir.join("out.jpg");
        let opts = ConvertOptions {
            quality: Some(90),
            ..Default::default()
        };
        convert(&input, &output, &opts).unwrap();
        let head = std::fs::read(&output).unwrap();
        assert_eq!(&head[0..2], &[0xFF, 0xD8], "JPEG SOI marker");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vips_failure_cleans_up_temp_file() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("vipsfail");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        // A directory where the output file should go makes vips fail.
        let output = dir.join("blocked.png");
        std::fs::create_dir(&output).unwrap();
        assert!(convert(&input, &output, &ConvertOptions::default()).is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mpp-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vips_formats_reject_wide_sample_formats() {
        use crate::formats::tiff::BitDepth;
        // BigTIFF is the only output that can carry u32 or f64 samples; the
        // vips-backed formats top out at u16, so widening BitDepth must not
        // silently make `--bit-depth f64 --format png` legal.
        assert!(OutputFormat::BigTiff.allows(BitDepth::U32));
        assert!(OutputFormat::BigTiff.allows(BitDepth::F64));
        assert!(!OutputFormat::Png.allows(BitDepth::U32));
        assert!(!OutputFormat::Png.allows(BitDepth::F64));
        assert!(!OutputFormat::Jpeg.allows(BitDepth::U32));
        assert!(!OutputFormat::Jxl.allows(BitDepth::F64));
    }

    #[test]
    fn incompatible_depth_and_quality_rejected() {
        let dir = tmpdir("opts");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 2, 2, 1, false);
        // F32 PNG is impossible.
        let opts = ConvertOptions {
            bit_depth: Some(crate::formats::tiff::BitDepth::F32),
            ..Default::default()
        };
        assert!(convert(&input, &dir.join("o.png"), &opts).is_err());
        // Quality on a lossless format is a user error.
        let opts = ConvertOptions {
            quality: Some(80),
            ..Default::default()
        };
        assert!(convert(&input, &dir.join("o.tif"), &opts).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
