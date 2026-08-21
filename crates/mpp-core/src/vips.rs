//! Discovery and spawning of the official `vips` binary.
//!
//! Per the hybrid decision in `docs/DESIGN.md`, mpp never links libvips:
//! vips-backed work (mainstream-format conversion, later `dzsave`) spawns
//! the `vips` CLI from libvips-tools. All sample-format math happens in
//! Rust beforehand — vips savers silently auto-cast unsupported band
//! formats (verified on 8.15.1: float input becomes a clipped 8-bit image
//! with no error), so vips is only ever handed data already in the target
//! format, making its job a pure container conversion.

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
    let output = Command::new(vips_bin)
        .arg("copy")
        .arg(src)
        .arg(dest_spec)
        .output()
        .map_err(|e| Error::io(dest_path, e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::format(
            dest_path,
            format!("vips copy failed ({}): {}", output.status, stderr.trim()),
        ));
    }
    Ok(())
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
}
