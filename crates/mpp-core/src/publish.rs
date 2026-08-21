//! One-call publish orchestration: route any supported input to a web
//! viewer output (a DZI tile pyramid or a single full-resolution image, by
//! size or explicit request) plus the branded Astrometrical viewer page and
//! WCS sidecar.
//!
//! [`publish`] always needs the `vips` binary (both modes route through it —
//! DZI tiling itself, and non-XISF input probing/reduction), unlike
//! [`crate::convert::convert`] whose native `BigTiff` leg needs no vips at
//! all.

use std::path::{Path, PathBuf};

use crate::convert::{TempFileGuard, unique_temp};
use crate::formats::tiff::{BitDepth, TiffWriteOptions, write_bigtiff};
use crate::formats::xisf::XisfImage;
use crate::viewer::{ViewerParams, emit_viewer};
use crate::vips::{vips_available, vips_dzsave, vips_msb, vipsheader_field};
use crate::wcs::LinearWcs;
use crate::wcs::sidecar::WcsSidecar;
use crate::{Error, Result};

/// Auto-mode threshold: images at most this many px per side use single
/// mode; larger images use a DZI tile pyramid.
pub const SINGLE_MODE_MAX_DIM: u64 = 4096;

/// How [`publish`] renders the image for web viewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PublishMode {
    /// Pick [`PublishMode::Single`] or [`PublishMode::Dzi`] automatically,
    /// from image size against [`SINGLE_MODE_MAX_DIM`].
    #[default]
    Auto,
    /// A Deep Zoom Image tile pyramid, for large images (the OpenSeadragon
    /// viewer streams only the tiles the current view needs).
    Dzi,
    /// A single full-resolution image file, for small images — an inline
    /// `{ type: 'image', ... }` tile source rather than a DZI pyramid.
    Single,
}

/// Tile container for [`PublishMode::Dzi`] output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TileFormat {
    /// JPEG tiles (lossy; default). Takes an optional
    /// [`PublishOptions::tile_quality`].
    #[default]
    Jpg,
    /// PNG tiles (lossless). [`PublishOptions::tile_quality`] does not
    /// apply and is rejected if set.
    Png,
}

/// Options for [`publish`].
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// Output mode; `Auto` (the default) picks by image size.
    pub mode: PublishMode,
    /// Tile container for DZI output.
    pub tile_format: TileFormat,
    /// JPEG tile quality 1–100 (vips `Q`); only valid with
    /// [`TileFormat::Jpg`] — an error with [`TileFormat::Png`]. `None`
    /// defaults to vips' own default (90) for `Jpg`.
    pub tile_quality: Option<u8>,
    /// Display title for the viewer page; `None` uses the input file's stem.
    pub title: Option<String>,
    /// Write the WCS JSON sidecar when the input carries a solution.
    pub sidecar: bool,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            mode: PublishMode::Auto,
            tile_format: TileFormat::Jpg,
            tile_quality: None,
            title: None,
            sidecar: true,
        }
    }
}

/// What a publish run did, for CLI reporting.
#[derive(Debug, Clone)]
pub struct PublishReport {
    /// The resolved mode actually used (never [`PublishMode::Auto`]).
    pub mode_used: PublishMode,
    /// Input image width in pixels.
    pub width: u64,
    /// Input image height in pixels.
    pub height: u64,
    /// Whether the input carried an extractable astrometric solution.
    pub wcs_found: bool,
    /// Path of the WCS sidecar written, if any.
    pub sidecar: Option<PathBuf>,
    /// Path of the emitted `index.html`.
    pub index: PathBuf,
}

/// A short, human-readable label for a resolved [`PublishMode`], used in the
/// viewer page's meta line.
fn mode_label(mode: PublishMode) -> &'static str {
    match mode {
        PublishMode::Dzi => "DZI tiles",
        PublishMode::Single => "single image",
        PublishMode::Auto => "auto",
    }
}

/// Resolve `requested` against image dimensions `w`×`h`: [`PublishMode::Auto`]
/// becomes [`PublishMode::Single`] at or below [`SINGLE_MODE_MAX_DIM`] per
/// side, else [`PublishMode::Dzi`]; an explicit mode passes through
/// unchanged (oversize-single warnings are the caller's concern, since they
/// need the original request to know the mode was explicit).
fn resolve_mode(requested: PublishMode, w: u64, h: u64) -> PublishMode {
    match requested {
        PublishMode::Auto => {
            if w.max(h) <= SINGLE_MODE_MAX_DIM {
                PublishMode::Single
            } else {
                PublishMode::Dzi
            }
        }
        explicit => explicit,
    }
}

/// Publish `input` as a self-hostable web viewer under `outdir`: an 8-bit
/// [`PublishMode::Dzi`] tile pyramid (built by vips `dzsave`) or a
/// [`PublishMode::Single`] full-resolution `<stem>.jpg` — plus the branded
/// OpenSeadragon viewer page ([`crate::viewer::emit_viewer`]) and, when the
/// input carries an astrometric solution and `opts.sidecar` is on, the WCS
/// JSON sidecar. [`PublishMode::Auto`] (the default) picks between the two
/// by [`SINGLE_MODE_MAX_DIM`]; an explicit [`PublishMode::Single`] above
/// that threshold proceeds anyway, with a warning to stderr, since the
/// browser will load the whole image.
///
/// `input` may be XISF (astrometric solution extracted natively) or any
/// other vips-readable, display-ready (8- or 16-bit) format; 16-bit input is
/// reduced to 8-bit with vips' `msb` operator before tiling, and float or
/// other sample formats are rejected with a message to stretch and export
/// first. `outdir` is created if missing.
///
/// In single mode, a `.jpg`/`.jpeg` input is copied byte-for-byte to
/// `<stem>.jpg` rather than re-encoded; any other input is re-encoded via
/// vips at `opts.tile_quality` (default 90).
pub fn publish(input: &Path, outdir: &Path, opts: &PublishOptions) -> Result<PublishReport> {
    let vips_bin = vips_available().ok_or_else(|| {
        Error::format(
            outdir,
            "vips binary not found on PATH; install libvips-tools \
             (e.g. `sudo apt install libvips-tools`)",
        )
    })?;

    std::fs::create_dir_all(outdir).map_err(|e| Error::io(outdir, e))?;

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.contains('\'') || stem.contains('\\') {
        return Err(Error::format(
            input,
            "file stem contains a character (' or \\) that cannot be safely \
             embedded in the viewer page; rename the file first",
        ));
    }

    let in_ext = input
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    let (src8, w, h, wcs, _guard): (PathBuf, u64, u64, Option<LinearWcs>, Option<TempFileGuard>) =
        if in_ext == "xisf" {
            let img = XisfImage::open(input)?;
            img.advise_sequential();
            let wcs = crate::wcs::from_xisf_properties(&img.header().properties);
            let tmp = unique_temp(outdir, &stem, "tif");
            let guard = TempFileGuard(tmp.clone());
            write_bigtiff(
                &img,
                &tmp,
                &TiffWriteOptions {
                    bit_depth: BitDepth::U8,
                },
            )?;
            (tmp, img.width(), img.height(), wcs, Some(guard))
        } else {
            let fmt = vipsheader_field(input, "format")?;
            let (src, guard) = match fmt.as_str() {
                "uchar" => (input.to_path_buf(), None),
                "ushort" => {
                    let tmp = unique_temp(outdir, &stem, "v");
                    let guard = TempFileGuard(tmp.clone());
                    vips_msb(&vips_bin, input, &tmp)?;
                    (tmp, Some(guard))
                }
                other => {
                    return Err(Error::format(
                        input,
                        format!(
                            "publish needs 8- or 16-bit display-ready input (got sample \
                             format '{other}'); stretch and export first"
                        ),
                    ));
                }
            };
            let w: u64 = vipsheader_field(input, "width")?
                .parse()
                .map_err(|_| Error::format(input, "vipsheader returned a non-numeric width"))?;
            let h: u64 = vipsheader_field(input, "height")?
                .parse()
                .map_err(|_| Error::format(input, "vipsheader returned a non-numeric height"))?;
            (src, w, h, None, guard)
        };

    let mode_used = resolve_mode(opts.mode, w, h);
    if opts.mode == PublishMode::Single && w.max(h) > SINGLE_MODE_MAX_DIM {
        eprintln!(
            "note: {w}x{h} exceeds {SINGLE_MODE_MAX_DIM} px; single-image mode \
             loads the whole image in the browser — consider --mode dzi"
        );
    }

    if opts.tile_quality.is_some() && opts.tile_format == TileFormat::Png {
        return Err(Error::format(
            outdir,
            "--tile-quality does not apply to PNG tiles",
        ));
    }

    let wcs_found = wcs.is_some();

    let tile_source = match mode_used {
        PublishMode::Single => {
            let dest = outdir.join(format!("{stem}.jpg"));
            if in_ext == "jpg" || in_ext == "jpeg" {
                std::fs::copy(input, &dest).map_err(|e| Error::io(&dest, e))?;
            } else {
                let quality = opts.tile_quality.unwrap_or(90);
                let dest_spec = format!("{}[Q={quality}]", dest.display());
                crate::vips::vips_copy(&vips_bin, &src8, &dest_spec, &dest)?;
            }
            format!("{{ type: 'image', url: '{stem}.jpg' }}")
        }
        PublishMode::Dzi => {
            let (suffix, fmt) = match opts.tile_format {
                // vips' dzsave writes the `.dzi` Format attribute from its
                // saver's own name, not the literal suffix — verified
                // empirically on vips 8.15: a ".jpg[Q=90]" suffix still
                // yields both tiles *and* `Format="jpg"` (three letters,
                // not "jpeg"). The inline tile source below must match that
                // real Format value, or OpenSeadragon requests tile URLs
                // with the wrong extension.
                TileFormat::Jpg => (
                    format!(".jpg[Q={}]", opts.tile_quality.unwrap_or(90)),
                    "jpg",
                ),
                TileFormat::Png => (".png".to_string(), "png"),
            };
            let out_base = outdir.join(&stem);
            vips_dzsave(&vips_bin, &src8, &out_base, &suffix)?;

            format!(
                "{{ Image: {{ xmlns: 'http://schemas.microsoft.com/deepzoom/2008', \
                 Url: '{stem}_files/', Format: '{fmt}', Overlap: '1', TileSize: '254', \
                 Size: {{ Width: '{w}', Height: '{h}' }} }} }}"
            )
        }
        PublishMode::Auto => unreachable!("resolved above to Single or Dzi"),
    };

    let mut sidecar = None;
    if opts.sidecar
        && let Some(w0) = &wcs
    {
        let sc = WcsSidecar::new(w, h, w0.clone());
        let path = outdir.join(format!("{stem}.wcs.json"));
        sc.write(&path)?;
        sidecar = Some(path);
    }

    let mut meta_line = format!("{w} × {h} px · {}", mode_label(mode_used));
    if let Some(w0) = &wcs {
        let (ra, dec) = w0.pixel_to_sky(w as f64 / 2.0 + 0.5, h as f64 / 2.0 + 0.5);
        let scale = w0.cd[0][0].hypot(w0.cd[1][0]) * 3600.0;
        meta_line.push('\n');
        meta_line.push_str(&format!(
            "center RA {ra:.4}° Dec {dec:.4}° · {scale:.3}″/px"
        ));
    }

    let params = ViewerParams {
        title: opts.title.clone().unwrap_or_else(|| stem.clone()),
        tile_source,
        meta_line,
        sidecar_href: sidecar.as_ref().map(|_| format!("{stem}.wcs.json")),
    };
    emit_viewer(outdir, &params)?;

    Ok(PublishReport {
        mode_used,
        width: w,
        height: h,
        wcs_found,
        sidecar,
        index: outdir.join("index.html"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::synth_xisf;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-publish-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn publishes_dzi_from_xisf() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("dzi");
        let input = dir.join("veil.xisf");
        synth_xisf(&input, 600, 400, 1, true);
        let out = dir.join("site");
        let opts = PublishOptions {
            mode: PublishMode::Dzi,
            ..Default::default()
        };
        let report = publish(&input, &out, &opts).unwrap();

        assert_eq!(report.mode_used, PublishMode::Dzi);
        assert_eq!((report.width, report.height), (600, 400));
        assert!(report.wcs_found);
        assert!(out.join("index.html").is_file());
        assert!(out.join("openseadragon.min.js").is_file());
        assert!(out.join("veil.dzi").is_file());
        assert!(out.join("veil_files").is_dir());
        assert!(out.join("veil.wcs.json").is_file());

        let html = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(
            html.contains("veil_files/"),
            "inline tile source points at tiles"
        );
        assert!(html.contains("Width: '600'"));
        assert!(html.contains("center RA"), "WCS telemetry baked in");
        assert!(!html.contains("{{"));

        // Temp interchange files must not survive.
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mpp-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn publishes_dzi_from_plain_ushort_tiff_via_msb() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("ushort");
        // Make a u16 TIFF with our own writer (values span [0,1] -> u16).
        let x = dir.join("in.xisf");
        synth_xisf(&x, 32, 16, 1, false);
        let img = crate::formats::xisf::XisfImage::open(&x).unwrap();
        let t16 = dir.join("plain.tif");
        crate::formats::tiff::write_bigtiff(
            &img,
            &t16,
            &crate::formats::tiff::TiffWriteOptions {
                bit_depth: crate::formats::tiff::BitDepth::U16,
            },
        )
        .unwrap();

        let out = dir.join("site");
        let opts = PublishOptions {
            mode: PublishMode::Dzi,
            ..Default::default()
        };
        let report = publish(&t16, &out, &opts).unwrap();
        assert_eq!((report.width, report.height), (32, 16));
        assert!(!report.wcs_found, "no WCS path for non-XISF input");
        assert!(out.join("plain.dzi").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_mode_small_image_publishes_single() {
        if crate::vips::vips_available().is_none() {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        }
        let dir = tmpdir("single");
        let input = dir.join("m27.xisf");
        synth_xisf(&input, 64, 48, 1, true);
        let out = dir.join("site");
        let report = publish(&input, &out, &PublishOptions::default()).unwrap();

        assert_eq!(report.mode_used, PublishMode::Single);
        assert!(out.join("m27.jpg").is_file());
        assert!(!out.join("m27.dzi").exists(), "no pyramid in single mode");
        let html = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(html.contains("type: 'image'"));
        assert!(html.contains("m27.jpg"));
        assert!(
            out.join("m27.wcs.json").is_file(),
            "sidecar in single mode too"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_mode_threshold_boundary() {
        // Pure resolution logic — no vips needed if factored as a function.
        assert_eq!(
            resolve_mode(PublishMode::Auto, 4096, 100),
            PublishMode::Single
        );
        assert_eq!(resolve_mode(PublishMode::Auto, 4097, 100), PublishMode::Dzi);
        assert_eq!(resolve_mode(PublishMode::Dzi, 10, 10), PublishMode::Dzi);
        assert_eq!(
            resolve_mode(PublishMode::Single, 100_000, 10),
            PublishMode::Single
        );
    }

    #[test]
    fn jpg_input_single_mode_copies_without_reencode() {
        let Some(vips) = crate::vips::vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = tmpdir("jpgcopy");
        // Produce a small uchar jpg with vips itself (`vips black` emits uchar).
        let v = dir.join("s.v");
        crate::vips::run_vips(
            &vips,
            &[
                "black".as_ref(),
                v.as_os_str(),
                "40".as_ref(),
                "30".as_ref(),
            ],
            &v,
        )
        .unwrap();
        let input = dir.join("photo.jpg");
        crate::vips::vips_copy(&vips, &v, &input.display().to_string(), &input).unwrap();

        let out = dir.join("site");
        let report = publish(&input, &out, &PublishOptions::default()).unwrap();
        assert_eq!(report.mode_used, PublishMode::Single);
        let src = std::fs::read(&input).unwrap();
        let dst = std::fs::read(out.join("photo.jpg")).unwrap();
        assert_eq!(src, dst, "byte-identical copy, not re-encoded");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_png_tile_quality() {
        let dir = tmpdir("reject");
        let input = dir.join("in.xisf");
        synth_xisf(&input, 8, 8, 1, false);
        let opts = PublishOptions {
            mode: PublishMode::Dzi,
            tile_format: TileFormat::Png,
            tile_quality: Some(80),
            ..Default::default()
        };
        assert!(
            publish(&input, &dir.join("s"), &opts).is_err(),
            "quality+png rejected"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
