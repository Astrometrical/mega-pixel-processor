# DZI Publish + Astrometrical Viewer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mpp publish in.xisf outdir/` emits a self-contained, self-hostable web view: a DZI tile pyramid (via spawned `vips dzsave`) plus an Astrometrical-branded OpenSeadragon HTML viewer — or, below a size threshold / on request, a single-image viewer (OSD `type:'image'`, no pyramid). The WCS sidecar rides along and its solution is baked into the page's telemetry line. This completes Phase 1.

**Architecture:** `publish.rs` orchestrates: input routing (XISF → temp 8-bit BigTIFF via the existing writer; 8-bit vips-readable inputs direct; 16-bit vips-readable inputs through `vips msb`, the deterministic high-byte extraction — **vips savers CLIP, not rescale, over-range values** (verified: ushort 256→255, 65535→255), so tiles must be genuinely 8-bit before dzsave); `vips.rs` grows a shared spawn runner plus `dzsave`/`msb`/header-probe wrappers; `viewer.rs` renders a static HTML template with the tile source inlined as a JS object (no XML fetch — friendlier to odd hosts) and a vendored `openseadragon.min.js` copied beside it. Output is dumb static files, per the DESIGN.md anti-lock-in commitment.

**Tech Stack:** Existing workspace; no new Rust dependencies. Vendored asset: OpenSeadragon **v6.1.0** (BSD-3-Clause; release zip `https://github.com/openseadragon/openseadragon/releases/download/v6.1.0/openseadragon-bin-6.1.0.zip`, verified current 2026-08-21). vips 8.15.1 CLI (`vips`, `vipsheader` from libvips-tools).

**Spec:** `docs/DESIGN.md` — "Publishes images as DZI…", "Universal viewer mode", "DZI + HTML outputs must remain dumb static files", branding section ("DeepSky is reserved for the viewer components"; the emitted page is Astrometrical-branded). Proven OSD settings adopted from the sibling astrometrical repo's production viewer (`DziStage.tsx`): `showNavigationControl:false` (custom controls), navigator bottom-left at 0.15, `minZoomImageRatio:1`, `maxZoomPixelRatio:4`, `visibilityRatio:1`, `constrainDuringPan:true`.

## Global Constraints

- Every public item in `mpp-core` gets a doc comment; `#![warn(missing_docs)]` stays warning-free.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc --no-deps` warning-free at every commit.
- Tests synthesize inputs; vips-gated tests skip with `eprintln!("skipping: vips binary not found on PATH");` when vips is absent.
- The emitted output directory must work served by any dumb static host — no CDN references, no absolute URLs, only relative paths.
- End every commit message with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## File Structure

```
crates/mpp-core/
  assets/openseadragon.min.js   # vendored OSD v6.1.0                (Task 2)
  assets/viewer.html            # branded template                    (Task 3)
  src/vips.rs                   # Modify: run_vips core, vips_dzsave,
                                #   vips_msb, vipsheader_field        (Task 1)
  src/viewer.rs                 # Create: template rendering + emit   (Task 3)
  src/publish.rs                # Create: routing, modes, orchestration (Task 4, 5)
  src/lib.rs                    # Modify: pub mod viewer; pub mod publish;
NOTICE                          # Modify: OpenSeadragon BSD-3 entry   (Task 2)
crates/mpp/src/main.rs          # Modify: publish subcommand          (Task 6)
crates/mpp/tests/publish.rs     # Create: gated CLI e2e               (Task 6)
docs/DESIGN.md, README.md       # Modify: record decisions/usage      (Task 7)
```

Output directory layout (all names derive from the input file stem, e.g. `veil`):

```
outdir/
  index.html            # the viewer page
  openseadragon.min.js  # vendored, copied verbatim
  veil.dzi + veil_files/   (dzi mode)      — vips dzsave products
  veil.jpg                 (single mode)   — 8-bit display image
  veil.wcs.json            (when solved and sidecar enabled)
```

Mode policy: `--mode auto|dzi|single`, default `auto` = `single` when `max(w,h) <= 4096`, else `dzi`. Explicit `single` above the threshold proceeds with a stderr warning suggesting DZI (per DESIGN.md "the CLI suggests DZI mode"). Tiles: `--tile-format jpg|png` (default jpg, `Q=90` unless `--tile-quality` given; png ignores quality with an error, mirroring convert's rule).

---

### Task 1: vips runner + dzsave/msb/header wrappers

**Files:**
- Modify: `crates/mpp-core/src/vips.rs`

**Interfaces:**
- Consumes: existing `vips_available`, `Error`, `Result`.
- Produces:

```rust
pub(crate) fn vips_dzsave(vips_bin: &Path, src: &Path, out_base: &Path, suffix: &str) -> Result<()>;
    // spawns: vips dzsave <src> <out_base> --suffix <suffix>   (e.g. ".jpg[Q=90]")
    // producing <out_base>.dzi + <out_base>_files/
pub(crate) fn vips_msb(vips_bin: &Path, src: &Path, dest: &Path) -> Result<()>;
    // spawns: vips msb <src> <dest> — 16-bit -> high byte (deterministic 8-bit)
pub(crate) fn vipsheader_field(src: &Path, field: &str) -> Result<String>;
    // spawns: vipsheader -f <field> <src>; trimmed stdout ("uchar", "16384", …)
```

Internally, refactor `vips_copy` and the new functions over one shared runner — `pub(crate)`, because publish.rs's tests (Task 5) also use it to synthesize source images:

```rust
/// Spawn a vips-family binary with `args`, mapping failure to a
/// caller-attributed error on `context` (stderr captured).
pub(crate) fn run_vips(bin: &Path, args: &[&std::ffi::OsStr], context: &Path) -> Result<std::process::Output>
```

- [ ] **Step 1: Write failing (gated) tests**

Add to `vips.rs` tests:

```rust
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
        run_vips(&vips, &["black".as_ref(), a.as_os_str(), "4".as_ref(), "4".as_ref()], &a).unwrap();
        // 0 * x + 256 = 256 everywhere, cast to ushort.
        run_vips(&vips, &["linear".as_ref(), a.as_os_str(), b.as_os_str(), "0".as_ref(), "256".as_ref()], &b).unwrap();
        run_vips(&vips, &["cast".as_ref(), b.as_os_str(), c.as_os_str(), "ushort".as_ref()], &c).unwrap();
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
        run_vips(&vips, &["black".as_ref(), src.as_os_str(), "31".as_ref(), "17".as_ref()], &src).unwrap();
        assert_eq!(vipsheader_field(&src, "width").unwrap(), "31");
        assert_eq!(vipsheader_field(&src, "height").unwrap(), "17");
        assert_eq!(vipsheader_field(&src, "format").unwrap(), "uchar");
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

(`run_vips` is used directly by the tests to synthesize sources; that is why it should be `pub(crate)`-visible to the test module — as a private fn in the same file it already is.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core vips`
Expected: compile FAIL — new functions not defined.

- [ ] **Step 3: Implement**

```rust
use std::ffi::OsStr;

/// Spawn a vips-family binary with `args`; nonzero exit becomes an
/// [`Error::Format`] on `context` carrying the captured stderr.
fn run_vips(bin: &Path, args: &[&OsStr], context: &Path) -> Result<std::process::Output> {
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
                args.first().map(|a| a.to_string_lossy()).unwrap_or_default(),
                output.status,
                stderr.trim()
            ),
        ));
    }
    Ok(output)
}
```

Rewrite `vips_copy`'s body over `run_vips` (behavior identical: `["copy", src, dest_spec]`, error attributed to `dest_path`). Add:

```rust
/// Build a DZI (Deep Zoom) pyramid: `<out_base>.dzi` + `<out_base>_files/`.
/// `suffix` is the vips tile suffix, e.g. `.jpg[Q=90]` or `.png`.
pub(crate) fn vips_dzsave(vips_bin: &Path, src: &Path, out_base: &Path, suffix: &str) -> Result<()> {
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
pub(crate) fn vipsheader_field(src: &Path, field: &str) -> Result<String> {
    let exe = if cfg!(windows) { "vipsheader.exe" } else { "vipsheader" };
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
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
```

Update the module docs to mention dzsave/msb/header probing.

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS (gated tests run for real here), no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): vips dzsave/msb/header wrappers over a shared runner"
```

---

### Task 2: Vendor OpenSeadragon v6.1.0

**Files:**
- Create: `crates/mpp-core/assets/openseadragon.min.js`
- Modify: `NOTICE`
- Modify: `crates/mpp-core/Cargo.toml` only if `include`/packaging fields exist (they don't today — nothing to do)

**Interfaces:**
- Produces: the asset file that Task 3 embeds via `include_str!("../assets/openseadragon.min.js")`.

- [ ] **Step 1: Download and vendor**

```bash
cd /tmp && curl -fsSL -o osd.zip https://github.com/openseadragon/openseadragon/releases/download/v6.1.0/openseadragon-bin-6.1.0.zip
unzip -o osd.zip 'openseadragon-bin-6.1.0/openseadragon.min.js' 'openseadragon-bin-6.1.0/LICENSE.txt'
mkdir -p <worktree>/crates/mpp-core/assets
cp openseadragon-bin-6.1.0/openseadragon.min.js <worktree>/crates/mpp-core/assets/
```

(If the zip's internal layout differs, `unzip -l osd.zip | grep min.js` and adjust; the file to vendor is the plain `openseadragon.min.js`, not the ES module build.) Verify the vendored file mentions `OpenSeadragon` and `6.1.0` in its header comment, and note its byte size in your report. If the sandbox blocks the download, report BLOCKED — do not substitute another version or source.

- [ ] **Step 2: Record the license**

Append to `NOTICE`:

```
This product bundles OpenSeadragon (https://openseadragon.github.io/),
version 6.1.0, licensed under the BSD 3-Clause License (New BSD).
Copyright (c) CodePlex Foundation and OpenSeadragon contributors.
The full license text is preserved in the header of the bundled
crates/mpp-core/assets/openseadragon.min.js and is emitted verbatim
into every published output directory.
```

(Check the downloaded LICENSE.txt: if the min.js header does NOT carry the license text, instead copy LICENSE.txt to `crates/mpp-core/assets/OPENSEADRAGON-LICENSE.txt`, reference that path in NOTICE, and have Task 3 emit it alongside the JS.)

- [ ] **Step 3: Sanity-compile a reference to the asset**

Add a temporary test nowhere — instead just verify the path from Rust in Task 3; for this task run `ls -la crates/mpp-core/assets/` and `head -c 600 crates/mpp-core/assets/openseadragon.min.js` to confirm content, plus the full gates:
`cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: unchanged green (asset not yet referenced).

- [ ] **Step 4: Commit**

```bash
git add crates/mpp-core/assets NOTICE
git commit -m "chore: vendor OpenSeadragon 6.1.0 (BSD-3-Clause) for the emitted viewer"
```

---

### Task 3: Viewer template + emission (`viewer.rs`)

**Files:**
- Create: `crates/mpp-core/assets/viewer.html`
- Create: `crates/mpp-core/src/viewer.rs`
- Modify: `crates/mpp-core/src/lib.rs` (add `pub mod viewer;` + crate-docs line)

**Interfaces:**
- Consumes: the vendored asset (Task 2).
- Produces (Task 4/5 consume):

```rust
pub struct ViewerParams {
    pub title: String,          // page + display title (HTML-escaped on render)
    pub tile_source: String,    // JS expression for OSD tileSources (trusted, built by publish.rs)
    pub meta_line: String,      // telemetry footer text, may be ""
    pub sidecar_href: Option<String>, // relative href of the wcs sidecar, if any
}
pub fn emit_viewer(outdir: &Path, params: &ViewerParams) -> Result<()>;
    // writes outdir/index.html and outdir/openseadragon.min.js
```

- [ ] **Step 1: Write failing tests**

Create `crates/mpp-core/src/viewer.rs` with only:

```rust
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
        assert!(html.contains("{ type: 'image', url: 'veil.jpg' }"), "tile source verbatim");
        assert!(html.contains("21384 x 14208 px"));
        assert!(html.contains(r#"href="veil.wcs.json""#), "sidecar link present");
        assert!(html.contains(r#"src="openseadragon.min.js""#), "relative script ref");
        assert!(!html.contains("{{"), "no unexpanded placeholders");
        assert!(!html.contains("http://") && !html.contains("https://cdn"),
            "no external fetches (xmlns attrs excluded by checking http:// only... see note)");

        let js = std::fs::read(dir.join("openseadragon.min.js")).unwrap();
        assert!(js.len() > 100_000, "real OSD bundle copied ({} bytes)", js.len());
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
```

Note on the no-external-fetch assertion: the deepzoom `xmlns` string constant contains `http://schemas.microsoft.com` but that is data, not a fetch. Write the assertion as: the html must not contain `src="http`, `href="http`, or `@import` — implement exactly that (three `!html.contains(...)` checks) instead of the loose sketch above.

Add `pub mod viewer;` to `lib.rs` and a crate-docs bullet: `//! - [`viewer`] — emission of the static, self-hostable Astrometrical viewer page.` (merge into the existing list style; also mention publish once Task 4 lands).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core viewer`
Expected: compile FAIL.

- [ ] **Step 3: Create the template**

Create `crates/mpp-core/assets/viewer.html`. Design intent: observatory minimalism — the image is the interface; true-black surround; chrome that fades when idle; monospace telemetry; letterspaced Astrometrical wordmark. Self-containment rules: system font stacks only (no webfonts), no external URLs, all refs relative.

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="generator" content="Mega Pixel Processor (mpp)">
<title>{{TITLE}} — Astrometrical</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Cpath fill='%23e8e4d8' d='M8 0l1.2 6.8L16 8l-6.8 1.2L8 16 6.8 9.2 0 8l6.8-1.2z'/%3E%3C/svg%3E">
{{SIDECAR_LINK}}
<style>
  :root {
    --ink: #e8e4d8;        /* warm starlight, not clinical white */
    --ink-dim: #8a8678;
    --line: rgba(232, 228, 216, 0.16);
    --scrim: rgba(3, 4, 6, 0.55);
    --serif: 'Iowan Old Style', 'Palatino Linotype', Palatino, Georgia, serif;
    --mono: ui-monospace, 'Cascadia Code', 'SF Mono', Menlo, Consolas, monospace;
    --sans: 'Avenir Next', 'Segoe UI', 'Helvetica Neue', sans-serif;
  }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  html, body { height: 100%; background: #030406; overflow: hidden; }
  #viewer { position: fixed; inset: 0; background: #030406; outline: none; }

  /* Chrome fades away while you look at the sky. */
  .chrome { transition: opacity 0.6s ease; }
  body.idle .chrome { opacity: 0; }

  header {
    position: fixed; top: 0; left: 0; right: 0; z-index: 10;
    padding: 1.1rem 1.4rem 2.6rem;
    background: linear-gradient(to bottom, var(--scrim), transparent);
    pointer-events: none;
  }
  .wordmark {
    font: 600 0.6rem/1 var(--sans);
    letter-spacing: 0.42em; text-transform: uppercase;
    color: var(--ink-dim);
  }
  .wordmark b { color: var(--ink); font-weight: 600; }
  h1 {
    margin-top: 0.45rem;
    font: italic 400 clamp(1.15rem, 2.6vw, 1.7rem)/1.2 var(--serif);
    color: var(--ink); letter-spacing: 0.01em;
  }

  footer {
    position: fixed; left: 0; right: 0; bottom: 0; z-index: 10;
    display: flex; align-items: flex-end; justify-content: space-between;
    padding: 2.4rem 1.4rem 1rem;
    background: linear-gradient(to top, var(--scrim), transparent);
    pointer-events: none;
  }
  .meta {
    font: 400 0.68rem/1.6 var(--mono);
    color: var(--ink-dim); letter-spacing: 0.04em;
    white-space: pre-line;
  }
  .controls { display: flex; gap: 0.4rem; pointer-events: auto; }
  .controls button {
    width: 2.1rem; height: 2.1rem;
    font: 400 1rem/1 var(--mono); color: var(--ink);
    background: rgba(3, 4, 6, 0.5);
    border: 1px solid var(--line); border-radius: 2px;
    cursor: pointer; backdrop-filter: blur(6px);
    transition: border-color 0.2s ease, color 0.2s ease;
  }
  .controls button:hover { border-color: var(--ink-dim); }
  .controls button:active { color: #fff; }

  /* OSD navigator: quiet frame, bottom-left above the scrim padding. */
  .navigator { border: 1px solid var(--line) !important; }
</style>
</head>
<body>
<div id="viewer"></div>

<header class="chrome">
  <div class="wordmark"><b>Astrometrical</b>&ensp;·&ensp;Deep&nbsp;Zoom</div>
  <h1>{{TITLE}}</h1>
</header>

<footer class="chrome">
  <div class="meta">{{META_LINE}}</div>
  <div class="controls" role="group" aria-label="Viewer controls">
    <button id="zi" title="Zoom in" aria-label="Zoom in">+</button>
    <button id="zo" title="Zoom out" aria-label="Zoom out">−</button>
    <button id="home" title="Fit to screen" aria-label="Fit to screen">⌂</button>
    <button id="fs" title="Fullscreen" aria-label="Fullscreen">⤢</button>
  </div>
</footer>

<script src="openseadragon.min.js"></script>
<script>
  var viewer = OpenSeadragon({
    id: 'viewer',
    tileSources: {{TILE_SOURCE}},
    showNavigationControl: false,
    showNavigator: true,
    navigatorPosition: 'BOTTOM_LEFT',
    navigatorSizeRatio: 0.15,
    minZoomImageRatio: 1,
    maxZoomPixelRatio: 4,
    visibilityRatio: 1,
    constrainDuringPan: true
  });
  document.getElementById('zi').addEventListener('click', function () {
    viewer.viewport.zoomBy(1.4); viewer.viewport.applyConstraints();
  });
  document.getElementById('zo').addEventListener('click', function () {
    viewer.viewport.zoomBy(1 / 1.4); viewer.viewport.applyConstraints();
  });
  document.getElementById('home').addEventListener('click', function () {
    viewer.viewport.goHome();
  });
  document.getElementById('fs').addEventListener('click', function () {
    viewer.setFullScreen(!viewer.isFullScreen());
  });

  // Chrome fades after 2.5 s of stillness; any pointer activity restores it.
  var idleTimer = null;
  function wake() {
    document.body.classList.remove('idle');
    clearTimeout(idleTimer);
    idleTimer = setTimeout(function () {
      document.body.classList.add('idle');
    }, 2500);
  }
  ['pointermove', 'pointerdown', 'keydown', 'touchstart'].forEach(function (ev) {
    window.addEventListener(ev, wake, { passive: true });
  });
  wake();
</script>
</body>
</html>
```

- [ ] **Step 4: Implement `viewer.rs`**

```rust
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

/// Write `index.html` and `openseadragon.min.js` into `outdir`
/// (which must already exist).
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
    Ok(())
}
```

Adjust the emitted-page test's external-fetch assertion to the three-checks form described in Step 1. If Task 2 ended up vendoring a separate `OPENSEADRAGON-LICENSE.txt`, also emit it here and extend the test.

- [ ] **Step 5: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): Astrometrical viewer template + static emission"
```

---

### Task 4: Publish orchestration, DZI mode (`publish.rs`)

**Files:**
- Create: `crates/mpp-core/src/publish.rs`
- Modify: `crates/mpp-core/src/lib.rs` (add `pub mod publish;`, crate-docs line)

**Interfaces:**
- Consumes: `formats::xisf::XisfImage`, `formats::tiff::{write_bigtiff, TiffWriteOptions, BitDepth}`, `vips::{vips_available, vips_dzsave, vips_msb, vipsheader_field}`, `viewer::{emit_viewer, ViewerParams}`, `wcs::from_xisf_properties`, `wcs::sidecar::WcsSidecar`. Reuse `convert.rs`'s temp machinery: make its `TempFileGuard` and `TEMP_SEQ` counter `pub(crate)` (keep `convert.rs`'s call sites working) rather than duplicating them here.
- Produces:

```rust
pub enum PublishMode { Auto, Dzi, Single }         // + Debug, Clone, Copy, PartialEq, Eq, Default(Auto)
pub enum TileFormat { Jpg, Png }                    // + Debug, Clone, Copy, PartialEq, Eq, Default(Jpg)
pub struct PublishOptions {
    pub mode: PublishMode,
    pub tile_format: TileFormat,
    pub tile_quality: Option<u8>,   // jpg tiles only; error with Png
    pub title: Option<String>,      // default: input file stem
    pub sidecar: bool,              // default true
}                                    // manual Default: Auto, Jpg, None, None, true
pub struct PublishReport {
    pub mode_used: PublishMode,     // resolved (never Auto)
    pub width: u64,
    pub height: u64,
    pub wcs_found: bool,
    pub sidecar: Option<PathBuf>,
    pub index: PathBuf,             // path of the emitted index.html
}
pub fn publish(input: &Path, outdir: &Path, opts: &PublishOptions) -> Result<PublishReport>;
/// Auto-mode threshold: images at most this many px per side use single mode.
pub const SINGLE_MODE_MAX_DIM: u64 = 4096;
```

This task implements everything EXCEPT the single-mode leg: when the resolved mode is `Single`, return `Error::format(outdir, "single mode is not wired yet (next commit)")`. Task 5 replaces that arm.

Publish flow (dzi mode):
1. `vips_available()` or friendly libvips-tools error (publish always needs vips, both modes).
2. Create `outdir` (`create_dir_all`).
3. Route input to an 8-bit vips-readable source:
   - ext `xisf` → open `XisfImage`, dims from header, extract WCS; write temp u8 BigTIFF **inside `outdir`** named `<stem>.mpp-tmp-<pid>-<seq>.tif` (reuse `convert.rs`'s now-`pub(crate)` `TempFileGuard`; add a local `fn temp_in(outdir, stem) -> PathBuf` with its own `AtomicU64` or reuse convert's counter by making it `pub(crate)` too — prefer reusing one shared counter, moved to a small `pub(crate) mod tempfile` section in `convert.rs`).
   - other extensions: require vips-readable; probe `vipsheader_field(format)`: `"uchar"` → use input directly (no temp); `"ushort"` → `vips_msb` into a guarded temp `.v` file in `outdir`; anything else → `Error::format(input, "publish needs 8- or 16-bit display-ready input (got sample format '<fmt>'); stretch and export first")`. Dims via `vipsheader_field("width"/"height")`. WCS: none for non-XISF input in this plan (`wcs_found = false`).
4. Resolve mode: `Auto` → `Single` if `max(w, h) <= SINGLE_MODE_MAX_DIM` else `Dzi`; explicit values pass through. Explicit `Single` with `max(w, h) > SINGLE_MODE_MAX_DIM`: `eprintln!` a warning suggesting DZI mode, proceed (Task 5 arm).
5. Validate: `tile_quality.is_some() && tile_format == Png` → error.
6. Dzi: suffix = `.jpg[Q=<q or 90>]` or `.png`; `vips_dzsave(src8, outdir/<stem>)`.
7. Sidecar (XISF + solution + opts.sidecar): `WcsSidecar::new(w, h, wcs).write(outdir/<stem>.wcs.json)`.
8. Meta line: `"<W> × <H> px · <mode>"` first row; when WCS known append a second row `"center RA <ra>° Dec <dec>° · <scale>″/px"` where `(ra, dec) = wcs.pixel_to_sky(w as f64 / 2.0 + 0.5, h as f64 / 2.0 + 0.5)` (float math — the FITS-convention image center) formatted to 4 decimal places, and scale in arcsec/px = `wcs.cd[0][0].hypot(wcs.cd[1][0]) * 3600.0` to 3 decimals. Join rows with `'\n'` (the template's `.meta` uses `white-space: pre-line`).
9. `emit_viewer` with inline DZI tile source:

```rust
let tile_source = format!(
    "{{ Image: {{ xmlns: 'http://schemas.microsoft.com/deepzoom/2008', \
     Url: '{stem}_files/', Format: '{fmt}', Overlap: '1', TileSize: '254', \
     Size: {{ Width: '{w}', Height: '{h}' }} }} }}"
);
// fmt: "jpeg" for Jpg tiles, "png" for Png — matching what dzsave wrote.
```

(The `.dzi` XML also exists on disk for other consumers; the page inlines the source so it renders even where XML fetches are awkward. `stem` must be sanitized for the JS string: it derives from a filename — reject stems containing `'` or `\` with a clear error rather than escaping.)

- [ ] **Step 1: Write failing (gated) tests**

```rust
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
        let opts = PublishOptions { mode: PublishMode::Dzi, ..Default::default() };
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
        assert!(html.contains("veil_files/"), "inline tile source points at tiles");
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
            &crate::formats::tiff::TiffWriteOptions { bit_depth: crate::formats::tiff::BitDepth::U16 },
        )
        .unwrap();

        let out = dir.join("site");
        let opts = PublishOptions { mode: PublishMode::Dzi, ..Default::default() };
        let report = publish(&t16, &out, &opts).unwrap();
        assert_eq!((report.width, report.height), (32, 16));
        assert!(!report.wcs_found, "no WCS path for non-XISF input");
        assert!(out.join("plain.dzi").is_file());
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
        assert!(publish(&input, &dir.join("s"), &opts).is_err(), "quality+png rejected");
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

(Float/other-format non-XISF rejection is exercised implicitly by the routing code and covered at the unit level by the `vipsheader_field` probe tests; a dedicated float-input test is not required in this plan.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core publish`
Expected: compile FAIL.

- [ ] **Step 3: Implement per the flow above**

Make `convert.rs`'s `TempFileGuard` and its `TEMP_SEQ` counter `pub(crate)` (move both plus a
`pub(crate) fn unique_temp(dir_neighbor: &Path, ext: &str) -> PathBuf` helper if that reads cleaner — keep `convert.rs`'s own call sites working). All new public items documented. `PublishMode`/`TileFormat` get `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]` with `#[default]` on `Auto`/`Jpg`.

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): publish orchestration with DZI pyramid + branded viewer"
```

---

### Task 5: Single-image mode + auto threshold

**Files:**
- Modify: `crates/mpp-core/src/publish.rs`

**Interfaces:**
- Consumes: Task 4's structure; `vips::vips_copy`.
- Produces: the `Single` arm — emit `<stem>.jpg` (always jpg for the single display image: `vips_copy(src8, "<stem>.jpg[Q=<quality or 90>]")`; when the input itself is already a `.jpg`/`.jpeg` file, plain `std::fs::copy` instead of re-encoding) and the `{ type: 'image', url: '<stem>.jpg' }` tile source. Auto resolution and the oversize-single warning live here too.

- [ ] **Step 1: Write failing (gated) tests**

```rust
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
        assert!(out.join("m27.wcs.json").is_file(), "sidecar in single mode too");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn auto_mode_threshold_boundary() {
        // Pure resolution logic — no vips needed if factored as a function.
        assert_eq!(resolve_mode(PublishMode::Auto, 4096, 100), PublishMode::Single);
        assert_eq!(resolve_mode(PublishMode::Auto, 4097, 100), PublishMode::Dzi);
        assert_eq!(resolve_mode(PublishMode::Dzi, 10, 10), PublishMode::Dzi);
        assert_eq!(resolve_mode(PublishMode::Single, 100_000, 10), PublishMode::Single);
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
            &["black".as_ref(), v.as_os_str(), "40".as_ref(), "30".as_ref()],
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp-core publish`
Expected: single-mode tests FAIL (arm still returns the Task-4 error).

- [ ] **Step 3: Implement**

Extract `fn resolve_mode(requested: PublishMode, w: u64, h: u64) -> PublishMode` (pub(crate) for the unit test, or `#[cfg(test)]-visible`; simplest: private fn + `#[cfg(test)] use` in the test module — same file, so plain private works). Implement the Single arm per the interface block; oversize explicit-single prints `eprintln!("note: {w}x{h} exceeds {SINGLE_MODE_MAX_DIM} px; single-image mode loads the whole image in the browser — consider --mode dzi")`.

- [ ] **Step 4: Verify green**

Run: `cargo test -p mpp-core && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc -p mpp-core --no-deps`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp-core
git commit -m "feat(core): single-image viewer mode + auto threshold"
```

---

### Task 6: CLI `publish` subcommand

**Files:**
- Modify: `crates/mpp/src/main.rs`
- Create: `crates/mpp/tests/publish.rs`

**Interfaces:**
- Consumes: `mpp_core::publish::{publish, PublishOptions, PublishMode, TileFormat}`.
- Produces: `mpp publish <INPUT> <OUTDIR> [--mode auto|dzi|single] [--tile-format jpg|png] [--tile-quality 1-100] [--title T] [--no-sidecar]`; success summary on stdout including the resolved mode and index.html path.

- [ ] **Step 1: Write failing (gated) CLI test**

Create `crates/mpp/tests/publish.rs` (reuse the `synth_xisf`/`tmpdir`/`vips_on_path` helper shapes from `tests/convert.rs` — duplication across test files of the same crate: extract the shared helpers into `crates/mpp/tests/common/mod.rs` and use `mod common;` from both test files, updating `convert.rs`'s tests to match):

```rust
mod common;
use common::{synth_xisf, tmpdir, vips_on_path};
use std::process::Command;

#[test]
fn publish_emits_viewer_site() {
    if !vips_on_path() {
        eprintln!("skipping: vips binary not found on PATH");
        return;
    }
    let dir = tmpdir("publish");
    let input = dir.join("ngc7000.xisf");
    synth_xisf(&input, 300, 200);
    let out = dir.join("site");

    let cmd = Command::new(env!("CARGO_BIN_EXE_mpp"))
        .args(["publish"])
        .arg(&input)
        .arg(&out)
        .args(["--mode", "dzi", "--title", "North America"])
        .output()
        .unwrap();
    assert!(cmd.status.success(), "stderr: {}", String::from_utf8_lossy(&cmd.stderr));

    assert!(out.join("index.html").is_file());
    assert!(out.join("openseadragon.min.js").is_file());
    assert!(out.join("ngc7000.dzi").is_file());
    let html = std::fs::read_to_string(out.join("index.html")).unwrap();
    assert!(html.contains("North America"), "--title lands in the page");
    std::fs::remove_dir_all(&dir).unwrap();
}
```

`common/mod.rs` holds the exact helper bodies currently in `tests/convert.rs` (`synth_xisf`, `tmpdir`, `vips_on_path`), made `pub`; `tests/convert.rs` switches to `mod common; use common::*;` and deletes its local copies. (Cargo treats `tests/common/mod.rs` as a non-test module directory — standard pattern.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p mpp`
Expected: FAIL — no `publish` subcommand.

- [ ] **Step 3: Implement the subcommand**

Add to the `Command` enum:

```rust
    /// Publish an image as a self-hosted web viewer: DZI tile pyramid (or a
    /// single image below 4096 px) + Astrometrical-branded OpenSeadragon
    /// page. Requires the vips CLI (libvips-tools).
    Publish {
        /// Input image (.xisf, or any 8/16-bit vips-readable format).
        input: PathBuf,
        /// Output directory for the static site (created if missing).
        outdir: PathBuf,
        /// Pyramid vs single-image viewer; auto picks by size (<= 4096 px -> single).
        #[arg(long, value_enum, default_value_t = ModeArg::Auto)]
        mode: ModeArg,
        /// Tile image format for DZI mode.
        #[arg(long, value_enum, default_value_t = TileFormatArg::Jpg)]
        tile_format: TileFormatArg,
        /// JPEG tile quality, 1-100 (default 90); error with --tile-format png.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        tile_quality: Option<u8>,
        /// Page title (default: input file stem).
        #[arg(long)]
        title: Option<String>,
        /// Do not write the WCS sidecar even when the input is solved.
        #[arg(long)]
        no_sidecar: bool,
    },
```

With `ModeArg { Auto, Dzi, Single }` / `TileFormatArg { Jpg, Png }` ValueEnums + `From` impls (each variant doc-commented), and the handler:

```rust
        Command::Publish { input, outdir, mode, tile_format, tile_quality, title, no_sidecar } => {
            let opts = PublishOptions {
                mode: mode.into(),
                tile_format: tile_format.into(),
                tile_quality,
                title,
                sidecar: !no_sidecar,
            };
            let report = publish(&input, &outdir, &opts)?;
            println!(
                "published {}x{} as {:?} -> {}",
                report.width, report.height, report.mode_used, report.index.display()
            );
            match (&report.sidecar, report.wcs_found) {
                (Some(p), _) => println!("wrote WCS sidecar {}", p.display()),
                (None, true) => println!("WCS sidecar disabled (--no-sidecar)"),
                (None, false) => println!("no astrometric solution found; no sidecar"),
            }
            println!("serve the directory with any static host, e.g.: python3 -m http.server -d {}", outdir.display());
        }
```

- [ ] **Step 4: Verify green**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/mpp
git commit -m "feat(cli): publish subcommand (DZI/single viewer site)"
```

---

### Task 7: Docs + phase-1 closeout

**Files:**
- Modify: `docs/DESIGN.md`
- Modify: `README.md`

- [ ] **Step 1: Record decisions in DESIGN.md**

Add a new subsection after the WCS-sidecar section:

```markdown
## DZI publishing decisions (2026-08-21)

- Tiles must be genuinely 8-bit before `dzsave`: vips savers CLIP (not
  rescale) over-range values — a 16-bit input would blow out to white.
  Routing: XISF → Rust 8-bit temp BigTIFF; 8-bit vips inputs direct;
  16-bit vips inputs via `vips msb` (deterministic high byte); other
  sample formats are rejected (publish takes stretched, display-ready
  input by design).
- The viewer page inlines the tile source as a JS object (the `.dzi` XML
  is still emitted for other consumers) and bundles OpenSeadragon v6.1.0
  (BSD-3-Clause, see NOTICE) with all-relative references — the output
  works on any dumb static host, per the positioning commitment.
- Viewer OSD settings mirror the production Astrometrical platform viewer
  (custom controls, navigator bottom-left, maxZoomPixelRatio 4,
  visibilityRatio 1). When a solution exists, center RA/Dec and pixel
  scale are baked into the page's telemetry footer at publish time (a
  live WCS readout is phase-3 DeepSky work).
- Universal viewer mode: `--mode single` / auto below 4096 px per side
  uses OSD's single-image source, no pyramid; oversize explicit-single
  warns and suggests DZI.
```

Also update the "Phases" section: mark phase 1's scope (convert + sidecar + DZI/HTML) as implemented as of 2026-08-21, remaining phase-1 loose ends: FITS evaluation, real-data Photoshop smoke test.

- [ ] **Step 2: Update README**

Extend Status to cover `mpp publish`; add a usage example:

```markdown
## Publishing a zoomable web view

```sh
mpp publish mosaic.xisf site/ --title "Veil Nebula"
# then serve `site/` from any static host:
python3 -m http.server -d site/
```

DZI tile pyramid + a self-contained OpenSeadragon viewer (no CDN, works
self-hosted anywhere). Images ≤ 4096 px publish as a single-image page;
`--mode dzi|single` overrides. A solved input also writes the
`*.wcs.json` sidecar next to the tiles and bakes center coordinates into
the page footer.
```

- [ ] **Step 3: Full workspace check + manual browser smoke note**

Run: `cargo test && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
Expected: all green. In your report, note the manual smoke step for the human: `cargo run -p mpp -- publish <real>.xisf /tmp/site && python3 -m http.server -d /tmp/site` and open in a browser.

- [ ] **Step 4: Commit**

```bash
git add docs/DESIGN.md README.md
git commit -m "docs: record DZI publishing decisions; phase-1 scope complete"
```

---

## Out of scope for this plan

- Live WCS readout, annotations, catalog overlays (phase 3 / DeepSky components).
- A `deepsky` npm package for the viewer (phase 3).
- WCS extraction from non-XISF inputs (FITS evaluation pending).
- dzsave tuning (tile size/overlap stay at DeepZoom defaults 254/1), IIIF/zoomify layouts.
- Any hosted-platform hooks (Astronomic).
