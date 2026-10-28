//! Manual smoke test: decode real PixInsight XISF samples and compare each
//! against its uncompressed reference.
//!
//! The samples are written by mmm's `tools/pixinsight/gen_xisf_samples.js`
//! (every codec × checksum × sample format, plus multi-subblock and
//! real-panel sets); each directory holds `manifest-*.json` files listing
//! `{file, reference, …}` entries. For every entry this checks that the
//! file opens, that its decoded planar f32 pixels are bit-identical to the
//! reference's, and that its properties (minus the `XISF:*` unit metadata,
//! which records the compression settings) and FITS keywords are equal.
//!
//! ```sh
//! cargo run --release -p mpp-core --no-default-features \
//!     --example xisf_samples_check -- <sample-dir>...
//! ```
//!
//! Exits non-zero on any mismatch.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use mpp_core::formats::xisf::{XisfImage, XisfOpenOptions};
use mpp_core::formats::{FitsKeyword, XisfProperty};

/// One decoded unit: pixels plus the metadata that must match.
struct Decoded {
    pixels: Vec<f32>,
    properties: Vec<XisfProperty>,
    keywords: Vec<FitsKeyword>,
    seconds: f64,
}

fn decode(path: &Path, image_index: usize) -> Result<Decoded, String> {
    let t = Instant::now();
    let opts = XisfOpenOptions {
        image_index,
        ..Default::default()
    };
    let img = XisfImage::open_with(path, &opts).map_err(|e| e.to_string())?;
    let n = (img.width() * img.height() * img.channels()) as usize;
    let mut pixels = vec![0f32; n];
    img.decode_into(&mut pixels).map_err(|e| e.to_string())?;
    let h = img.header();
    Ok(Decoded {
        pixels,
        properties: h
            .properties
            .iter()
            .filter(|p| !p.id.starts_with("XISF:"))
            .cloned()
            .collect(),
        keywords: h.fits_keywords.clone(),
        seconds: t.elapsed().as_secs_f64(),
    })
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn describe(path: &Path) -> String {
    match XisfImage::open_with(path, &XisfOpenOptions::default()) {
        Ok(img) => {
            let h = img.header();
            let comp = h
                .block
                .compression
                .as_ref()
                .map(|c| format!("{} ({} subblocks)", c.codec_id(), c.subblocks.len()))
                .unwrap_or_else(|| "none".into());
            let ck = h
                .block
                .checksum
                .as_ref()
                .map(|c| c.algorithm.name())
                .unwrap_or("none");
            format!(
                "{:?} {}x{}x{} codec={comp} checksum={ck} mapped={}",
                h.sample_format,
                h.width,
                h.height,
                h.channels,
                img.is_mapped()
            )
        }
        Err(e) => format!("open failed: {e}"),
    }
}

fn check_dir(dir: &Path, failures: &mut usize, checked: &mut usize) {
    let mut manifests: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("manifest-") && n.ends_with(".json"))
        })
        .collect();
    manifests.sort();
    for manifest in manifests {
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
        println!(
            "== {} (PixInsight {})",
            manifest.display(),
            doc["pixinsight"].as_str().unwrap_or("?")
        );
        let mut cached: Option<(String, Decoded)> = None;
        for entry in doc["files"].as_array().into_iter().flatten() {
            let file = entry["file"].as_str().unwrap();
            let path = dir.join(file);
            if !path.exists() {
                println!("SKIP {file} (missing)");
                continue;
            }
            *checked += 1;
            let info = describe(&path);
            let Some(reference) = entry["reference"].as_str() else {
                // Multi-image (or reference-less) unit: every image decodes.
                let count = XisfImage::open(&path).map(|i| i.header().image_count);
                let ok = match count {
                    Ok(n) => {
                        (0..n).all(|i| decode(&path, i).is_ok())
                            && n == entry["images"].as_array().map_or(n, |a| a.len())
                    }
                    Err(_) => false,
                };
                println!(
                    "{} {file}: {info}, images={count:?}",
                    if ok { "OK  " } else { "FAIL" }
                );
                *failures += usize::from(!ok);
                continue;
            };
            if cached.as_ref().is_none_or(|(name, _)| name != reference) {
                match decode(&dir.join(reference), 0) {
                    Ok(d) => cached = Some((reference.to_string(), d)),
                    Err(e) => {
                        println!("FAIL {file}: reference {reference}: {e}");
                        *failures += 1;
                        cached = None;
                        continue;
                    }
                }
            }
            let want = &cached.as_ref().unwrap().1;
            match decode(&path, 0) {
                Ok(got) => {
                    let mut why = Vec::new();
                    if !same_bits(&got.pixels, &want.pixels) {
                        why.push("pixels differ");
                    }
                    // Same id/type/value; `location` is a file offset and
                    // legitimately differs between encodings.
                    let props =
                        |d: &Decoded| -> Vec<(String, String, mpp_core::formats::PropertyValue)> {
                            d.properties
                                .iter()
                                .map(|p| (p.id.clone(), p.type_.clone(), p.value.clone()))
                                .collect()
                        };
                    if props(&got) != props(want) {
                        why.push("properties differ");
                    }
                    if got.keywords.len() != want.keywords.len()
                        || got.keywords.iter().zip(&want.keywords).any(|(a, b)| {
                            (&a.name, &a.value, &a.comment) != (&b.name, &b.value, &b.comment)
                        })
                    {
                        why.push("keywords differ");
                    }
                    let mb = got.pixels.len() as f64 * 4.0 / 1e6;
                    let rate = format!("{:.0} MB/s", mb / got.seconds.max(1e-9));
                    if why.is_empty() {
                        println!(
                            "OK   {file}: {info}, {} props, {rate}",
                            got.properties.len()
                        );
                    } else {
                        println!("FAIL {file}: {info}: {}", why.join(", "));
                        *failures += 1;
                    }
                }
                Err(e) => {
                    println!("FAIL {file}: {info}: {e}");
                    *failures += 1;
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let dirs: Vec<String> = std::env::args().skip(1).collect();
    if dirs.is_empty() {
        eprintln!("usage: xisf_samples_check <sample-dir>...");
        return ExitCode::from(2);
    }
    let (mut failures, mut checked) = (0, 0);
    for d in &dirs {
        check_dir(Path::new(d), &mut failures, &mut checked);
    }
    println!("checked {checked} files, {failures} failed");
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
