// MppIcon.h -- SVG artwork for the Mega Pixel Processor module.
//
// MPP_CHEVRON_SVG is the plain Astrometrical chevron mark used in the TIFF
// options dialog's header. It is what makes the dialog visibly ours rather
// than the stock TIFF module's -- see the product decision recorded in
// docs/superpowers/specs/2026-09-09-tiff-parity-design.md.
//
// EDIT THIS FILE to change the mark. To preview outside PixInsight, copy the
// literal between the svg( )svg delimiters into a .svg file and open it in a
// browser.
//
// The chevron path is the Astrometrical "A" mark (the Angora font glyph):
// polygon vertices traced from apps/web/src/assets/Logo-Black.png in the
// astrometrical repo -- pointed apex, THIN left stroke, THICK right stroke,
// notch rising to about 60% height. Do not "fix" the asymmetry. The path
// data is copied verbatim from MergeMosaic's MmmIcon.h (MMM_CHEVRON_SVG,
// our own code) so the two products' marks stay identical.

#ifndef __MppIcon_h
#define __MppIcon_h

#define MPP_CHEVRON_SVG R"svg(<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" width="48" height="48">
  <path d="M 23.78 7.06 L 46 40.94 L 34.89 40.94 L 20.49 18.97 L 6.34 40.94 L 2 40.94 Z" fill="#2a95ab"/>
</svg>
)svg"

// Format icon for the TIFF file format, returned by
// BigTiffFormat::IconImageSVG(). The core uses it in the Format Explorer, on
// image-window icons of this format, and everywhere else the format appears
// graphically -- so it is the thing a user sees when deciding which TIFF
// module is loaded. Without it the core assigns its generic blue cube.
//
// Motif: a document page with a folded corner (the universal "file" glyph, so
// it reads as a FORMAT rather than a process -- deliberately not the tiled
// panels MMM's process icon uses) in the Astrometrical teal ramp, carrying
// the chevron mark and a TIFF label. The chevron path is MPP_CHEVRON_SVG's,
// verbatim, scaled into place with a transform so the mark stays identical
// across products.
//
// The label is SVG text in a generic sans-serif face. If the core's SVG
// renderer lacks text support on some platform the page and chevron still
// draw and only the label is lost -- graceful, but worth confirming in the
// manual smoke procedure. Note PCL's contract: an INVALID SVG silently falls
// back to the default cube with no error, so a malformed edit here regresses
// invisibly; keep it well-formed.
#define MPP_TIFF_FORMAT_ICON_SVG R"svg(<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48" width="48" height="48">
  <!-- page body -->
  <path d="M 8 2.5 H 29 L 40.5 14 V 45.5 H 8 Z" fill="#2a95ab" stroke="#0b3541" stroke-opacity="0.45" stroke-width="1" stroke-linejoin="round"/>
  <!-- folded corner -->
  <path d="M 29 2.5 V 14 H 40.5 Z" fill="#4ebcd4" stroke="#0b3541" stroke-opacity="0.45" stroke-width="1" stroke-linejoin="round"/>
  <!-- Astrometrical chevron: MPP_CHEVRON_SVG's path, scaled into the page -->
  <g transform="translate(11.5 11) scale(0.52)">
    <path d="M 23.78 7.06 L 46 40.94 L 34.89 40.94 L 20.49 18.97 L 6.34 40.94 L 2 40.94 Z" fill="#ffffff" stroke="#0b3541" stroke-width="1.2" stroke-linejoin="round"/>
  </g>
  <text x="24" y="42" text-anchor="middle" font-family="sans-serif" font-weight="bold" font-size="8.5" fill="#ffffff">TIFF</text>
</svg>
)svg"

#endif   // __MppIcon_h
