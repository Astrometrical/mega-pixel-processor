//! Emission of the static, self-hostable Astrometrical viewer page.
//!
//! The page is dumb static files by design (see DESIGN.md positioning):
//! one HTML file rendered from an embedded template, plus the vendored
//! OpenSeadragon bundle copied beside it. All references are relative;
//! nothing is fetched from a CDN. The OSD viewer settings mirror the
//! production Astrometrical platform viewer.

use std::path::Path;

use crate::{Error, Result};

/// The vendored OpenSeadragon bundle (v6.1.0, BSD-3-Clause — see NOTICE).
const OPENSEADRAGON_JS: &str = include_str!("../assets/openseadragon.min.js");
/// The OpenSeadragon license text, emitted verbatim into every published
/// output directory per NOTICE.
const OPENSEADRAGON_LICENSE: &str = include_str!("../assets/OPENSEADRAGON-LICENSE.txt");
/// The viewer page template; `{{…}}` placeholders are filled by [`emit_viewer`].
const TEMPLATE: &str = include_str!("../assets/viewer.html");

/// Content of the emitted viewer page.
#[derive(Debug, Clone)]
pub struct ViewerParams {
    /// Display title (image name); HTML-escaped on render.
    pub title: String,
    /// JavaScript expression for OpenSeadragon `tileSources` — built by
    /// the publisher, inserted verbatim (never derived from user text).
    pub tile_source: String,
    /// Telemetry footer text (dimensions, sky position); may be empty.
    pub meta_line: String,
    /// Relative href of the WCS sidecar to advertise via `<link>`, if any.
    pub sidecar_href: Option<String>,
}

/// Minimal HTML escaping for text interpolated into the template.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Write `index.html`, `openseadragon.min.js`, and
/// `OPENSEADRAGON-LICENSE.txt` into `outdir` (which must already exist).
pub fn emit_viewer(outdir: &Path, params: &ViewerParams) -> Result<()> {
    let sidecar_link = match &params.sidecar_href {
        Some(href) => format!(
            r#"<link rel="alternate" type="application/json" title="WCS solution" href="{}">"#,
            escape_html(href)
        ),
        None => String::new(),
    };
    let html = TEMPLATE
        .replace("{{TITLE}}", &escape_html(&params.title))
        .replace("{{TILE_SOURCE}}", &params.tile_source)
        .replace("{{META_LINE}}", &escape_html(&params.meta_line))
        .replace("{{SIDECAR_LINK}}", &sidecar_link);

    let index = outdir.join("index.html");
    std::fs::write(&index, html).map_err(|e| Error::io(&index, e))?;
    let osd = outdir.join("openseadragon.min.js");
    std::fs::write(&osd, OPENSEADRAGON_JS).map_err(|e| Error::io(&osd, e))?;
    let license = outdir.join("OPENSEADRAGON-LICENSE.txt");
    std::fs::write(&license, OPENSEADRAGON_LICENSE).map_err(|e| Error::io(&license, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpp-viewer-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn emits_complete_static_page() {
        let dir = tmpdir("emit");
        let params = ViewerParams {
            title: "Veil <West>".to_string(),
            tile_source: "{ type: 'image', url: 'veil.jpg' }".to_string(),
            meta_line: "21384 x 14208 px".to_string(),
            sidecar_href: Some("veil.wcs.json".to_string()),
        };
        emit_viewer(&dir, &params).unwrap();

        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(html.contains("Veil &lt;West&gt;"), "title HTML-escaped");
        assert!(
            html.contains("{ type: 'image', url: 'veil.jpg' }"),
            "tile source verbatim"
        );
        assert!(html.contains("21384 x 14208 px"));
        assert!(
            html.contains(r#"href="veil.wcs.json""#),
            "sidecar link present"
        );
        assert!(
            html.contains(r#"src="openseadragon.min.js""#),
            "relative script ref"
        );
        assert!(!html.contains("{{"), "no unexpanded placeholders");
        assert!(!html.contains("src=\"http"), "no external script fetch");
        assert!(!html.contains("href=\"http"), "no external link fetch");
        assert!(!html.contains("@import"), "no external stylesheet import");

        let js = std::fs::read(dir.join("openseadragon.min.js")).unwrap();
        assert!(
            js.len() > 100_000,
            "real OSD bundle copied ({} bytes)",
            js.len()
        );

        let license = std::fs::read_to_string(dir.join("OPENSEADRAGON-LICENSE.txt")).unwrap();
        assert!(
            !license.is_empty(),
            "OpenSeadragon license emitted verbatim"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sidecar_link_omitted_when_absent() {
        let dir = tmpdir("nosc");
        let params = ViewerParams {
            title: "T".into(),
            tile_source: "{}".into(),
            meta_line: String::new(),
            sidecar_href: None,
        };
        emit_viewer(&dir, &params).unwrap();
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(!html.contains("wcs.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
