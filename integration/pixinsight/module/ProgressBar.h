// ProgressBar.h -- the text behind the module's Console progress bar, with
// no PCL dependency so integration/pixinsight/host/test/test_progress_bar.cpp
// can pin it without PixInsight installed.
//
// The bar is redrawn in place: PixInsight's console has no carriage return
// that overwrites (<bol> + write inserts), but <bsp>/'\b' deletes a glyph, so
// ConsoleProgress (BigTiffInstance.cpp) erases the previous line with exactly
// `visible` backspaces before writing the next one. Markup tags (<b>...</b>)
// and ANSI colour sequences are zero-width on the console, which is why the
// glyph count is assembled here alongside the markup rather than measured
// from it. Same look as MergeMosaic's bar: bold stage name padded to a fixed
// column, teal 24-glyph [####----] bar, right-aligned percent.

#ifndef __mpp_ProgressBar_h
#define __mpp_ProgressBar_h

#include <cstdint>
#include <cstdio>
#include <string>

namespace mpp {

/// One rendered progress line: console markup plus its visible glyph count.
struct ProgressBarText {
  std::string markup;  ///< Text to hand to Console::Write.
  int visible = 0;     ///< Glyphs the console will draw for it.
};

/// Integer percent of `done` over `total`, floored, clamped to 0..100, and
/// `0` for a zero total. `done * 100` cannot overflow: both operands are
/// halved together until it fits, which keeps the ratio to within a percent
/// for row counts no real image reaches.
inline int PercentOf( uint64_t done, uint64_t total ) {
  if ( total == 0 ) return 0;
  if ( done >= total ) return 100;
  while ( done > UINT64_MAX/100 ) {
    done >>= 1;
    total >>= 1;
  }
  return int( done*100/total );
}

/// Render `name` and `percent` (0..100) as a console progress line.
inline ProgressBarText RenderProgressBar( const std::string& name, int percent ) {
  constexpr int kNameColumns = 13;
  constexpr int kBarGlyphs = 24;
  if ( percent < 0 ) percent = 0;
  if ( percent > 100 ) percent = 100;

  std::string padded = name;
  if ( int( padded.size() ) < kNameColumns )
    padded.append( size_t( kNameColumns - int( padded.size() ) ), ' ' );

  const int fill = kBarGlyphs*percent/100;
  std::string bar( size_t( fill ), '#' );
  bar.append( size_t( kBarGlyphs - fill ), '-' );

  char pct[8];
  std::snprintf( pct, sizeof pct, "%3d%%", percent );

  ProgressBarText out;
  out.markup = "<b>" + padded + "</b> "
               "\x1b[38;2;42;149;171m[" + bar + "]\x1b[39m " + pct;
  out.visible = int( padded.size() ) + 1 + 1 + kBarGlyphs + 1 + 1 + int( std::string( pct ).size() );
  return out;
}

}  // namespace mpp

#endif  // __mpp_ProgressBar_h
