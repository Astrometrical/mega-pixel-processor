//! Discovery and spawning of the official `vips` binary.
//!
//! Per the hybrid decision in `docs/DESIGN.md`, mpp never links libvips:
//! vips-backed work (mainstream-format conversion, `dzsave` tiling, `msb`
//! bit-depth reduction, and `vipsheader` probing) spawns the `vips`
//! CLI from libvips-tools. All sample-format math happens in Rust
//! beforehand — vips savers silently auto-cast unsupported band formats
//! (verified on 8.15.1: float input becomes a clipped 8-bit image with no
//! error), so vips is only ever handed data already in the target format,
//! making its job a pure container conversion. `vips_dzsave` builds the
//! Deep Zoom tile pyramid, `vips_msb` derives a deterministic 8-bit image
//! from a 16-bit one, and `vipsheader_field` reads back individual header
//! fields (width, height, format, …) for verification, normalizing
//! libvips' verbose enum-value dump (`((VipsBandFormat)
//! VIPS_FORMAT_UCHAR)` on 8.15.1) down to the plain nickname (`"uchar"`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result};

/// Locate the `vips` binary on `PATH`, if installed.
pub fn vips_available() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "vips.exe" } else { "vips" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(exe))
        .find(|candidate| candidate.is_file())
}

/// Spawn a vips-family binary with `args`; nonzero exit becomes an
/// [`Error::Format`] on `context` carrying the captured stderr.
pub(crate) fn run_vips(
    bin: &Path,
    args: &[&OsStr],
    context: &Path,
) -> Result<std::process::Output> {
    let output = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| Error::io(context, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::format(
            context,
            format!(
                "{} {} failed ({}): {}",
                bin.file_name().unwrap_or_default().to_string_lossy(),
                args.first()
                    .map(|a| a.to_string_lossy())
                    .unwrap_or_default(),
                output.status,
                stderr.trim()
            ),
        ));
    }
    Ok(output)
}

/// Run `vips copy <src> <dest_spec>` — a pure container conversion.
///
/// `dest_spec` is the output path plus optional bracketed saver options
/// (e.g. `out.png[bitdepth=16]`); `dest_path` is the plain path, used for
/// error reporting. vips stderr is captured into the error message.
pub(crate) fn vips_copy(
    vips_bin: &Path,
    src: &Path,
    dest_spec: &str,
    dest_path: &Path,
) -> Result<()> {
    run_vips(
        vips_bin,
        &["copy".as_ref(), src.as_os_str(), dest_spec.as_ref()],
        dest_path,
    )
    .map(|_| ())
}

/// Build a DZI (Deep Zoom) pyramid: `<out_base>.dzi` + `<out_base>_files/`.
/// `suffix` is the vips tile suffix, e.g. `.jpg[Q=90]` or `.png`.
// Only exercised by this module's gated tests today; the publish/dzsave
// routing task in this plan wires it into non-test code.
#[allow(dead_code)]
pub(crate) fn vips_dzsave(
    vips_bin: &Path,
    src: &Path,
    out_base: &Path,
    suffix: &str,
) -> Result<()> {
    run_vips(
        vips_bin,
        &[
            "dzsave".as_ref(),
            src.as_os_str(),
            out_base.as_os_str(),
            "--suffix".as_ref(),
            suffix.as_ref(),
        ],
        out_base,
    )
    .map(|_| ())
}

/// Deterministic 16-bit → 8-bit: keep each sample's most significant byte.
// Only exercised by this module's gated tests today; the publish/dzsave
// routing task in this plan wires it into non-test code.
#[allow(dead_code)]
pub(crate) fn vips_msb(vips_bin: &Path, src: &Path, dest: &Path) -> Result<()> {
    run_vips(
        vips_bin,
        &["msb".as_ref(), src.as_os_str(), dest.as_os_str()],
        dest,
    )
    .map(|_| ())
}

/// Read one header field of a vips-readable image via `vipsheader -f`
/// (e.g. "width", "height", "format"). `vipsheader` ships in the same
/// package as `vips`, so its presence is implied by [`vips_available`].
///
/// Plain scalar fields (`width`, `height`, …) print bare (e.g. `"16384"`).
/// Enum-typed fields (`format`, `interpretation`, `coding`) print as
/// GLib's verbose value dump — `((VipsBandFormat) VIPS_FORMAT_UCHAR)` on
/// libvips 8.15.1 — rather than the plain nickname `vipsheader -a` shows
/// for the same field; [`normalize_enum_field`] reduces that shape to the
/// bare lowercase nickname (`"uchar"`) so callers see one consistent form.
// Only exercised by this module's gated tests today; the publish/dzsave
// routing task in this plan wires it into non-test code.
#[allow(dead_code)]
pub(crate) fn vipsheader_field(src: &Path, field: &str) -> Result<String> {
    let exe = if cfg!(windows) {
        "vipsheader.exe"
    } else {
        "vipsheader"
    };
    let output = Command::new(exe)
        .arg("-f")
        .arg(field)
        .arg(src)
        .output()
        .map_err(|e| Error::io(src, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::format(
            src,
            format!("vipsheader -f {field} failed: {}", stderr.trim()),
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(normalize_enum_field(&raw))
}

/// Reduce libvips' `((TypeName) ENUM_VALUE)` value-contents dump (as
/// printed by `vipsheader -f` for enum-typed fields) to the bare lowercase
/// nickname, e.g. `((VipsBandFormat) VIPS_FORMAT_UCHAR)` -> `"uchar"`.
/// Strings not in that shape (plain scalar fields) pass through unchanged.
// Reachable only through `vipsheader_field` above, which is itself
// unreached from non-test code until a later task wires it in.
#[allow(dead_code)]
fn normalize_enum_field(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix("((").and_then(|s| s.strip_suffix(')')) else {
        return raw.to_string();
    };
    let Some((_type_name, value)) = inner.split_once(") ") else {
        return raw.to_string();
    };
    match value.rsplit('_').next() {
        Some(nick) if !nick.is_empty() => nick.to_lowercase(),
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vips_copy_surfaces_stderr_on_failure() {
        let Some(vips) = vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mpp-vips-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("nope.tif");
        let out = dir.join("out.png");
        let err = vips_copy(&vips, &missing, &out.display().to_string(), &out).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("vips"), "error mentions vips: {msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vips_available_finds_binary_or_none() {
        // Must not panic either way; on machines with libvips-tools it is Some.
        let _ = vips_available();
    }

    #[test]
    fn normalize_enum_field_strips_glib_value_dump() {
        assert_eq!(
            normalize_enum_field("((VipsBandFormat) VIPS_FORMAT_UCHAR)"),
            "uchar"
        );
        assert_eq!(
            normalize_enum_field("((VipsInterpretation) VIPS_INTERPRETATION_MULTIBAND)"),
            "multiband"
        );
        // Plain scalar fields pass through untouched.
        assert_eq!(normalize_enum_field("31"), "31");
    }

    #[test]
    fn dzsave_produces_pyramid() {
        let Some(vips) = vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mpp-dzsave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A tiny uchar source image made with vips itself (no Rust writer needed).
        let src = dir.join("src.v");
        run_vips(
            &vips,
            &[
                "black".as_ref(),
                src.as_os_str(),
                "64".as_ref(),
                "48".as_ref(),
            ],
            &src,
        )
        .unwrap();
        let base = dir.join("pyr");
        vips_dzsave(&vips, &src, &base, ".jpg[Q=90]").unwrap();
        assert!(dir.join("pyr.dzi").is_file());
        assert!(dir.join("pyr_files").is_dir());
        assert!(
            dir.join("pyr_files").join("0").join("0_0.jpg").is_file(),
            "level-0 tile exists"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn msb_takes_high_byte() {
        let Some(vips) = vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mpp-msb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (dir.join("a.v"), dir.join("b.v"), dir.join("c.v"));
        run_vips(
            &vips,
            &["black".as_ref(), a.as_os_str(), "4".as_ref(), "4".as_ref()],
            &a,
        )
        .unwrap();
        // 0 * x + 256 = 256 everywhere, cast to ushort.
        run_vips(
            &vips,
            &[
                "linear".as_ref(),
                a.as_os_str(),
                b.as_os_str(),
                "0".as_ref(),
                "256".as_ref(),
            ],
            &b,
        )
        .unwrap();
        run_vips(
            &vips,
            &[
                "cast".as_ref(),
                b.as_os_str(),
                c.as_os_str(),
                "ushort".as_ref(),
            ],
            &c,
        )
        .unwrap();
        let m = dir.join("m.v");
        vips_msb(&vips, &c, &m).unwrap();
        assert_eq!(vipsheader_field(&m, "format").unwrap(), "uchar");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vipsheader_reads_fields() {
        let Some(vips) = vips_available() else {
            eprintln!("skipping: vips binary not found on PATH");
            return;
        };
        let dir = std::env::temp_dir().join(format!("mpp-vh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("s.v");
        run_vips(
            &vips,
            &[
                "black".as_ref(),
                src.as_os_str(),
                "31".as_ref(),
                "17".as_ref(),
            ],
            &src,
        )
        .unwrap();
        assert_eq!(vipsheader_field(&src, "width").unwrap(), "31");
        assert_eq!(vipsheader_field(&src, "height").unwrap(), "17");
        assert_eq!(vipsheader_field(&src, "format").unwrap(), "uchar");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
