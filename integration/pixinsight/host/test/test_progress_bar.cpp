// test_progress_bar.cpp -- the PCL-free text renderer behind the module's
// Console progress bar (module/ProgressBar.h). The Console redraw erases the
// previous line with exactly `visible` backspaces, so the visible-glyph count
// must match the markup's glyphs precisely: markup tags and ANSI sequences
// are zero-width, everything else is one glyph. A miscount either leaves
// stray characters on the line or eats the text before the bar.
//
// Included by relative path from the module directory, like
// test_sectionbar_order reads module sources: the header has no PCL
// dependency by design so this suite can pin it without PixInsight.

#include <cstdio>
#include <cstdlib>
#include <string>

#include "../../module/ProgressBar.h"

namespace {

void Fail( const std::string& what ) {
  std::fprintf( stderr, "test_progress_bar: FAILED: %s\n", what.c_str() );
  std::exit( 1 );
}

// Glyphs a PixInsight console would draw: strip <...> tags and ESC[...m.
int CountGlyphs( const std::string& s ) {
  int n = 0;
  for ( size_t i = 0; i < s.size(); ) {
    if ( s[i] == '<' ) {
      size_t close = s.find( '>', i );
      if ( close == std::string::npos ) Fail( "unterminated tag in " + s );
      i = close + 1;
    } else if ( s[i] == '\x1b' ) {
      size_t close = s.find( 'm', i );
      if ( close == std::string::npos ) Fail( "unterminated ANSI sequence in " + s );
      i = close + 1;
    } else {
      ++n;
      ++i;
    }
  }
  return n;
}

}  // namespace

int main() {
  using mpp::RenderProgressBar;
  using mpp::PercentOf;

  // Percent: integer floor, total 0 is 0, complete is 100, never above.
  if ( PercentOf( 0, 100 ) != 0 ) Fail( "0/100 != 0%" );
  if ( PercentOf( 1, 3 ) != 33 ) Fail( "1/3 != 33%" );
  if ( PercentOf( 7, 7 ) != 100 ) Fail( "7/7 != 100%" );
  if ( PercentOf( 9, 7 ) != 100 ) Fail( "done > total must clamp to 100%" );
  if ( PercentOf( 5, 0 ) != 0 ) Fail( "total 0 must be 0%, not a division by zero" );
  // u64 safe: a 4-gigarow total must not overflow the multiplication.
  if ( PercentOf( 3'000'000'000ull, 4'000'000'000ull ) != 75 ) Fail( "large totals" );

  // Render: the stage name padded to 13, a 24-glyph bar, a 4-glyph percent.
  for ( int pct : { 0, 1, 37, 99, 100 } ) {
    mpp::ProgressBarText t = RenderProgressBar( "Encoding", pct );
    if ( t.visible != CountGlyphs( t.markup ) )
      Fail( "visible count " + std::to_string( t.visible ) + " != glyphs " +
            std::to_string( CountGlyphs( t.markup ) ) + " at " + std::to_string( pct ) + "%" );
    // name(13) + space + '[' + 24 + ']' + space + "nnn%"(4)
    if ( t.visible != 13 + 1 + 1 + 24 + 1 + 1 + 4 )
      Fail( "unexpected visible width " + std::to_string( t.visible ) );
    const int fill = 24*pct/100;
    const std::string bar = std::string( size_t( fill ), '#' ) + std::string( size_t( 24 - fill ), '-' );
    if ( t.markup.find( "[" + bar + "]" ) == std::string::npos )
      Fail( "bar for " + std::to_string( pct ) + "% not found in: " + t.markup );
  }
  if ( RenderProgressBar( "Encoding", 100 ).markup.find( "100%" ) == std::string::npos )
    Fail( "100% not rendered" );
  if ( RenderProgressBar( "Reading", 5 ).markup.find( "  5%" ) == std::string::npos )
    Fail( "percent must be right-aligned in 3 columns" );
  // A name longer than the pad is not truncated, and the count follows it.
  mpp::ProgressBarText longName = RenderProgressBar( "A very long stage name", 50 );
  if ( longName.visible != CountGlyphs( longName.markup ) ) Fail( "long name glyph count" );

  std::puts( "test_progress_bar: OK" );
  return 0;
}
