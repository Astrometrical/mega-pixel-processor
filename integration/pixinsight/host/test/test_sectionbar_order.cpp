// test_sectionbar_order.cpp -- guards a member-declaration-order invariant in
// TiffOptionsDialog that, when violated, crashes PixInsight on every TIFF save.
//
// pcl::SectionBar keeps a raw `Control* m_section` (SectionBar.h:358), set by
// SetSection(). ~SectionBar dereferences that pointer and calls through it.
// C++ destroys members in REVERSE declaration order, so a SectionBar declared
// BEFORE the Control it manages is destroyed AFTER it -- and dereferences a
// destroyed control. PCL rejects the dead handle with pcl::APIFunctionError,
// which escapes the implicitly-noexcept ~TiffOptionsDialog and calls
// std::terminate.
//
// That shipped once and aborted PixInsight on every File > Save As. The rule:
//
//     each section Control must be declared BEFORE the SectionBar that
//     SetSection()s it, so the SectionBar is destroyed first, while the
//     control it points at is still alive.
//
// This test is PCL-free by design -- it reads the two module sources as text,
// which is the only way to check a module invariant in a project whose module
// has no runtime test harness. It pairs each `X_SectionBar.SetSection( Y )`
// in the .cpp with the declaration order of X_SectionBar and Y in the .h.

#include <cstdio>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

namespace {

std::string Slurp( const char* path ) {
  std::ifstream in( path );
  if ( !in ) {
    std::fprintf( stderr, "cannot open %s\n", path );
    std::exit( 1 );
  }
  std::ostringstream ss;
  ss << in.rdbuf();
  return ss.str();
}

// Offset of the declaration of member `name` in the header, or npos.
// Matches the declaration line (type then name), not a use.
size_t DeclOffset( const std::string& hdr, const std::string& name ) {
  for ( size_t at = 0;; ) {
    const size_t hit = hdr.find( name, at );
    if ( hit == std::string::npos )
      return std::string::npos;
    const size_t eol = hdr.find( '\n', hit );
    const size_t bol = hdr.rfind( '\n', hit );
    const std::string line =
        hdr.substr( bol + 1, ( eol == std::string::npos ? hdr.size() : eol ) - bol - 1 );
    // A declaration line ends in ';' and has no '(' (which would be a call or
    // a function parameter).
    if ( line.find( ';' ) != std::string::npos && line.find( '(' ) == std::string::npos )
      return hit;
    at = hit + name.size();
  }
}

}  // namespace

int main( int argc, char** argv ) {
  if ( argc != 3 ) {
    std::fprintf( stderr, "usage: %s <TiffOptionsDialog.h> <TiffOptionsDialog.cpp>\n", argv[0] );
    return 2;
  }
  const std::string hdr = Slurp( argv[1] );
  const std::string src = Slurp( argv[2] );

  int pairs = 0, failures = 0;
  const std::string call = ".SetSection(";
  for ( size_t at = 0;; ) {
    const size_t hit = src.find( call, at );
    if ( hit == std::string::npos )
      break;
    at = hit + call.size();

    // Left of ".SetSection(" is the bar's name; inside the parens, the section.
    size_t barEnd = hit;
    size_t barBeg = barEnd;
    while ( barBeg > 0 && ( isalnum( (unsigned char)src[barBeg - 1] ) || src[barBeg - 1] == '_' ) )
      --barBeg;
    const std::string bar = src.substr( barBeg, barEnd - barBeg );

    const size_t close = src.find( ')', at );
    std::string section = src.substr( at, close - at );
    section.erase( 0, section.find_first_not_of( " \t" ) );
    section.erase( section.find_last_not_of( " \t" ) + 1 );

    const size_t barDecl = DeclOffset( hdr, bar );
    const size_t secDecl = DeclOffset( hdr, section );
    if ( barDecl == std::string::npos || secDecl == std::string::npos ) {
      std::fprintf( stderr, "FAILED: cannot locate declarations for %s / %s\n",
                    bar.c_str(), section.c_str() );
      ++failures;
      continue;
    }

    ++pairs;
    if ( secDecl > barDecl ) {
      std::fprintf( stderr,
                    "FAILED: %s is declared before its section %s.\n"
                    "        Members are destroyed in reverse declaration order, so %s\n"
                    "        would be destroyed AFTER %s and dereference a destroyed\n"
                    "        control in ~SectionBar -- pcl::APIFunctionError escaping a\n"
                    "        noexcept destructor, i.e. terminate on every save.\n"
                    "        Declare %s BEFORE %s.\n",
                    bar.c_str(), section.c_str(), bar.c_str(), section.c_str(),
                    section.c_str(), bar.c_str() );
      ++failures;
    }
  }

  if ( pairs == 0 ) {
    std::fprintf( stderr, "FAILED: no SetSection() pairs found -- the test parsed nothing\n" );
    return 1;
  }
  std::printf( "checked %d SectionBar/section pair(s), %d failure(s)\n", pairs, failures );
  return failures == 0 ? 0 : 1;
}
