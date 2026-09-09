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

#endif   // __MppIcon_h
