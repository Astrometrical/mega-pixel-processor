// BigTiffInstance.cpp -- see BigTiffInstance.h.
//
// Worker resolution (ModuleDirectory/ResolveWorkerPath) follows MergeMosaic's
// proven pattern (MmmExecution.cpp, our code): the module's own on-disk
// location via dladdr (POSIX) / GetModuleFileNameW (Windows), the worker
// binary expected beside it, and a pcl::Error with install advice when it is
// missing.

#include "BigTiffInstance.h"

#include "BigTiffFormat.h"
#include "TiffOptionsDialog.h"

#include <pcl/Console.h>
#include <pcl/Exception.h>
#include <pcl/File.h>

#ifdef _WIN32
#  include <windows.h>
#else
#  include <dlfcn.h>
#endif

#include <algorithm>
#include <cstring>
#include <limits>
#include <type_traits>
#include <vector>

namespace pcl
{

// ----------------------------------------------------------------------------

namespace
{

// Anchor whose address dladdr/GetModuleHandleExW maps back to this module's
// binary (PCL exposes no module-path API).
void ModuleAnchor()
{
}

// Directory containing this module's binary (trailing separator included on
// Windows drive paths; POSIX result ends with the last '/'-terminated
// directory component).
String ModuleDirectory()
{
#ifdef _WIN32
   HMODULE hmod = nullptr;
   if ( ::GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            reinterpret_cast<LPCWSTR>( &ModuleAnchor ), &hmod ) )
   {
      wchar_t buf[ 32768 ];
      DWORD n = ::GetModuleFileNameW( hmod, buf, DWORD( sizeof buf / sizeof buf[0] ) );
      if ( n > 0 && n < DWORD( sizeof buf / sizeof buf[0] ) )
      {
         // wchar_t is 16-bit on Windows == PCL String's UTF-16 code unit.
         String path( reinterpret_cast<const char16_t*>( buf ) );
         return File::ExtractDrive( path ) + File::ExtractDirectory( path );
      }
   }
   throw Error( "BigTIFF: could not determine the module path via GetModuleFileNameW()." );
#else
   Dl_info info;
   if ( dladdr( reinterpret_cast<void*>( &ModuleAnchor ), &info ) != 0 && info.dli_fname != nullptr )
   {
      String path( info.dli_fname );
      return File::ExtractDrive( path ) + File::ExtractDirectory( path );
   }
   throw Error( "BigTIFF: could not determine the module path via dladdr()." );
#endif
}

// Absolute path (UTF-8) to the sibling mpp-ipc-worker binary; throws a
// pcl::Error with install advice naming the expected path if it is absent.
std::string ResolveWorkerPath()
{
   String dir = ModuleDirectory();
   if ( !dir.EndsWith( '/' ) )
      dir += '/';
#ifdef _WIN32
   String worker = dir + "mpp-ipc-worker.exe";
   if ( !File::Exists( worker ) )
      throw Error( "BigTIFF: worker binary not found next to the module: " + worker +
                   "\nBuild it with `cargo build --release -p mpp-ipc-worker` and copy "
                   "`mpp-ipc-worker.exe` beside mpp-pxm.dll." );
#else
   String worker = dir + "mpp-ipc-worker";
   if ( !File::Exists( worker ) )
      throw Error( "BigTIFF: worker binary not found next to the module: " + worker +
                   "\nBuild it with `cargo build --release -p mpp-ipc-worker` and copy "
                   "`mpp-ipc-worker` beside mpp-pxm.so." );
#endif
   return std::string( worker.ToUTF8().c_str() );
}

// Protocol band-format name of a PixInsight sample type: the string
// CreateImage() must have negotiated for a WriteSamples buffer of this type
// to reach a shm slot untouched.
template <typename T>
constexpr const char* NativeBandFormat()
{
   if constexpr ( std::is_same_v<T, float> )
      return "f32";
   else if constexpr ( std::is_same_v<T, double> )
      return "f64";
   else
   {
      static_assert( std::is_unsigned_v<T>, "integer samples are unsigned in PCL" );
      if constexpr ( sizeof( T ) == 1 )
         return "u8";
      else if constexpr ( sizeof( T ) == 2 )
         return "u16";
      else
      {
         static_assert( sizeof( T ) == 4, "unexpected integer sample width" );
         return "u32";
      }
   }
}

// Number of sample values a format can tell apart, as a bit count: the
// integer formats' own width, and a float format's significand (24 for f32,
// 53 for f64). Used only to decide whether a conversion narrows -- it is a
// resolution comparison, not a storage size.
int FormatValueBits( const std::string& fmt )
{
   if ( fmt == "u8" )
      return 8;
   if ( fmt == "u16" )
      return 16;
   if ( fmt == "u32" )
      return 32;
   if ( fmt == "f32" )
      return 24;
   if ( fmt == "f64" )
      return 53;
   return 0;   // unreachable: CreateImage() only ever negotiates the five above
}

// True if converting a T sample to \a fmt can lose information, which is
// exactly what WasLossyWrite() reports (Unknowns §6).
//
// Two ways that happens: the destination resolves fewer distinct values than
// the source (f64 -> f32, u32 -> f32, u16 -> u8, ...), or a floating-point
// sample is quantized onto an integer grid at all -- a DImage saved as u8 is
// the brief's own example, and even f32 -> u32 collapses the dense small
// magnitudes f32 can represent onto a uniform grid. Every same-type
// conversion, and every widening one (u8 -> u16, u16 -> f32, u32 -> f64,
// f32 -> f64), is exact and reports false: since protocol v3 the u32 and f64
// paths are among them, which is the whole point of this phase.
template <typename T>
bool ConversionIsLossy( const std::string& fmt )
{
   const int srcBits = FormatValueBits( NativeBandFormat<T>() );
   const int dstBits = FormatValueBits( fmt );
   const bool floatToInteger =
      std::is_floating_point_v<T> && ( fmt == "u8" || fmt == "u16" || fmt == "u32" );
   return srcBits > dstBits || floatToInteger;
}

// One sample from PixInsight's sample type \a Src to the wire's \a Dst, via
// the unit range PixInsight itself defines: floating-point samples are
// already in [0,1], integer samples span [0, max]. Integer destinations are
// rounded to nearest and clamped, so an out-of-range float sample (PCL
// permits them) cannot wrap around; NaN maps to 0 because the `> 0` test
// below is false for it.
template <typename Dst, typename Src>
Dst ConvertSample( Src v )
{
   double unit;
   if constexpr ( std::is_floating_point_v<Src> )
      unit = double( v );
   else
   {
      static_assert( std::is_unsigned_v<Src>, "integer samples are unsigned in PCL" );
      unit = double( v )/double( std::numeric_limits<Src>::max() );
   }

   if constexpr ( std::is_floating_point_v<Dst> )
      return Dst( unit );
   else
   {
      constexpr double maxv = double( std::numeric_limits<Dst>::max() );
      const double scaled = unit*maxv;
      if ( !( scaled > 0 ) )
         return Dst( 0 );                              // <= 0, or NaN
      if ( scaled >= maxv )
         return std::numeric_limits<Dst>::max();
      return Dst( scaled + 0.5 );
   }
}

// Converts \a n samples from \a src into the raw \a dst buffer, which the
// caller has sized for \a n samples of \a fmt.
template <typename T>
void ConvertBuffer( const T* src, void* dst, uint64_t n, const std::string& fmt )
{
   auto run = [src, n]( auto* out )
   {
      using Dst = std::remove_pointer_t<decltype( out )>;
      for ( uint64_t i = 0; i < n; ++i )
         out[i] = ConvertSample<Dst>( src[i] );
   };
   if ( fmt == "f32" )
      run( reinterpret_cast<float*>( dst ) );
   else if ( fmt == "f64" )
      run( reinterpret_cast<double*>( dst ) );
   else if ( fmt == "u8" )
      run( reinterpret_cast<uint8_t*>( dst ) );
   else if ( fmt == "u16" )
      run( reinterpret_cast<uint16_t*>( dst ) );
   else if ( fmt == "u32" )
      run( reinterpret_cast<uint32_t*>( dst ) );
   else
      throw Error( String( "BigTIFF: internal error: unknown band format \"" ) +
                   String( fmt.c_str() ) + "\"." );
}

// Base64 of a byte range, for the ICC profile the v3 Init carries as text.
// PCL's own RFC 1421 encoder/decoder (IsoString::ToBase64 / FromBase64,
// String.h:7663 and 7765) is used rather than a hand-rolled pair: it is
// public PCL API this module already links, it is the standard alphabet with
// padding the worker's decoder expects
// (base64::engine::general_purpose::STANDARD), and a bespoke codec in the
// one code path that carries opaque user colour data would be all risk and
// no gain.
std::string EncodeBase64( const std::vector<uint8_t>& bytes )
{
   if ( bytes.empty() )
      return std::string();
   return std::string( IsoString::ToBase64( bytes.data(), bytes.size() ).c_str() );
}

// NUL-terminated text out of one of TiffFormatOptions' fixed-size character
// arrays, never reading past the array even if the block arrived unterminated
// (it crosses the module boundary, so its contents are not this module's to
// trust -- the same reason TiffFormatOptions::IsValid() exists).
template <size_t N>
std::string TagText( const char ( &field )[N] )
{
   size_t len = 0;
   while ( len < N && field[len] != '\0' )
      ++len;
   return std::string( field, len );
}

} // anonymous namespace

// ----------------------------------------------------------------------------

BigTiffInstance::BigTiffInstance( const BigTiffFormat* format )
   : FileFormatImplementation( format )
{
}

// The std::unique_ptr<mpp::WriteSession> member is the whole teardown:
// ~WriteSession kills and reaps the worker if it is still running, and shm/
// pipes are released by their own RAII members -- so an instance destroyed
// without Close() (Unknowns §3: nothing guarantees Close() is called on
// error/abort paths) still never leaks a worker process.
BigTiffInstance::~BigTiffInstance() = default;

// ----------------------------------------------------------------------------

ImageDescriptionArray BigTiffInstance::Open( const String& filePath, const IsoString& hints )
{
   (void)hints;   // no hint vocabulary in v1 (Unknowns §10)

   if ( open_ )
      throw Error( "BigTIFF: internal error: Open() called on an instance that is already open." );

   // Resolve the worker now so a missing installation fails the open
   // immediately; the same resolved path is reused by the ReadImage session.
   worker_path_ = ResolveWorkerPath();

   // Geometry probe (design "Two-phase worker interaction" phase 1): a
   // lightweight header-only read that reports {width,height,channels,alpha,
   // color_space,bit_depth} JSON and exits. This does not create any shm or
   // spawn a full read session -- opening the file twice (probe here, decode
   // in ReadImage) is cheap and keeps Open() side-effect-light.
   mpp::ImageMeta meta;
   try
   {
      meta = mpp::ReadSession::Probe( worker_path_, std::string( filePath.ToUTF8().c_str() ) );
   }
   catch ( const mpp::HostError& e )
   {
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }

   // Map the worker's authoritative geometry -> one PCL ImageDescription
   // (design "Sample-format & colour mapping (read)"). colorSpace is set by
   // the colour channels (Gray vs RGB); alpha, if any, is already counted in
   // meta.channels and imported as an extra channel.
   ImageInfo info;
   info.width = int( meta.width );
   info.height = int( meta.height );
   info.numberOfChannels = int( meta.channels );
   info.colorSpace = ( meta.color_space == "rgb" ) ? ColorSpace::RGB : ColorSpace::Gray;
   info.supported = true;

   // Sample format from bit_depth: unsigned int -> u8/u16/u32, IEEE float ->
   // f32/f64. Signed-integer and complex inputs are rejected upstream (in the
   // worker/core reader); the module declares neither here.
   ImageOptions options;
   const bool ieeefp = ( meta.bit_depth == "f32" || meta.bit_depth == "f64" );
   int bits;
   if ( meta.bit_depth == "u8" )
      bits = 8;
   else if ( meta.bit_depth == "u16" )
      bits = 16;
   else if ( meta.bit_depth == "u32" || meta.bit_depth == "f32" )
      bits = 32;
   else if ( meta.bit_depth == "f64" )
      bits = 64;
   else
      throw Error( String( "BigTIFF: unsupported sample format reported for '" ) + filePath +
                   "': " + String( meta.bit_depth.c_str() ) );
   options.bitsPerSample = uint8( bits );
   options.ieeefpSampleFormat = ieeefp;
   options.complexSample = false;
   options.signedIntegers = false;

   // Persist the declared geometry for the ReadImage overloads.
   read_width_ = meta.width;
   read_height_ = meta.height;
   read_channels_ = meta.channels;
   read_bit_depth_ = meta.bit_depth;
   read_color_space_ = info.colorSpace;
   // Base64 ICC blob, empty when the file embeds none; decoded on demand by
   // ReadICCProfile(). The probe already parsed it, so keeping it costs one
   // string copy and saves reopening the file to answer the core's query.
   read_icc_base64_ = meta.icc_profile;
   path_ = filePath;
   open_ = true;
   read_mode_ = true;

   ImageDescriptionArray descriptions;
   descriptions.Add( ImageDescription( info, options ) );
   return descriptions;
}

bool BigTiffInstance::IsOpen() const
{
   return open_;   // Unknowns §2: state tracked here, not in the base class
}

String BigTiffInstance::FilePath() const
{
   return path_;   // Unknowns §2: state tracked here, not in the base class
}

void BigTiffInstance::Close()
{
   // Idempotent full teardown, callable at any lifecycle point (Unknowns
   // §3). A live session here means Finish was never attempted (CloseImage
   // clears session_ before calling Finish), so Cancel is legal per the
   // WriteSession API and lets the worker delete its partial output file
   // (docs/pi-protocol.md §7.4). WriteSession::Cancel() always throws by
   // design -- HostError containing "cancelled" on the expected path -- and
   // Close() must not propagate a deliberate teardown as a failure.
   if ( session_ != nullptr )
   {
      try
      {
         session_->Cancel();
      }
      catch ( const std::exception& )
      {
         // Expected (see above). If the worker misbehaved instead,
         // resetting the session below still kills+reaps it.
      }
      session_.reset();
   }
   open_ = false;
   read_mode_ = false;   // a read instance carries no live session; just clear state

   // Per-file state that must not leak into a reused instance: an ICC
   // profile stashed for a save that never happened would otherwise be
   // embedded in the next one, and a stale read blob would be reported for
   // a file that has none.
   icc_profile_.clear();
   read_icc_base64_.clear();
   band_format_.clear();
   discarded_channel_ = -1;
}

// ----------------------------------------------------------------------------

template <class Img>
void BigTiffInstance::ReadInto( Img& image, const char* wantDepth )
{
   using sample = typename Img::sample;

   if ( !open_ || !read_mode_ )
      throw Error( "BigTIFF: internal error: ReadImage() called before a successful Open()." );

   // The core dispatches to the overload whose sample type matches the format
   // declared by Open() (native fidelity); a mismatched overload should never
   // fire. Fail loudly rather than silently reinterpret samples of the wrong
   // width (Unknowns: the headers do not promise which overload is called).
   if ( read_bit_depth_ != wantDepth )
      throw Error( String().Format(
         "BigTIFF: internal error: ReadImage() dispatched to the \"%s\" overload but the "
         "file declares \"%s\" samples (only the matching overload should be called).",
         wantDepth, read_bit_depth_.c_str() ) );

   const int width = int( read_width_ );
   const int height = int( read_height_ );
   const int channels = int( read_channels_ );

   // Allocate the destination in the geometry/colour space declared by Open()
   // (design phase 2). Row order is top-down in both TIFF and PixInsight -- no
   // flip.
   image.AllocateData( width, height, channels, read_color_space_ );

   // Size the shm slots the host provisions: ~4 MiB of native samples per
   // band, at least one row, at most the whole image (mirrors the write path's
   // band sizing, inverted).
   const uint64_t sampleBytes = uint64_t( sizeof( sample ) );
   const uint64_t kTargetBandBytes = uint64_t( 4 )*1024*1024;
   uint64_t bandRows = kTargetBandBytes/( read_width_*sampleBytes );
   bandRows = std::max<uint64_t>( 1, std::min<uint64_t>( bandRows, read_height_ ) );

   mpp::ReadSessionConfig cfg;
   cfg.worker_exe = worker_path_;
   cfg.input_path = std::string( path_.ToUTF8().c_str() );
   cfg.band_rows = uint32_t( bandRows );
   cfg.slots = 4;   // >= 2 recommended (mpp_host.h); 4 keeps bands in flight
   cfg.sample_bytes = sampleBytes;
   cfg.width = read_width_;
   cfg.channels = read_channels_;

   try
   {
      // Constructing the session spawns the worker (read mode) and reads its
      // authoritative ImageMeta; on any failure the ctor kills+reaps before
      // throwing.
      mpp::ReadSession rs( cfg );

      // Cross-check the session's authoritative geometry against Open()'s
      // probe: if the file changed on disk between the two opens, the
      // destination we just allocated would not match the incoming bands.
      const mpp::ImageMeta& m = rs.Meta();
      if ( m.width != read_width_ || m.height != read_height_ || m.channels != read_channels_ ||
           m.bit_depth != read_bit_depth_ )
         throw Error( "BigTIFF: the file's geometry changed between opening and reading it." );

      rs.ReadAll(
         [&]( uint32_t ch, uint64_t startRow, uint32_t rows, const void* samples )
         {
            // Guard against a misbehaving worker before the memcpy: an
            // out-of-range channel or a band running past the last row would
            // otherwise overrun the allocated image (host-side memory-safety
            // first, mirroring WriteSession's slot-id discipline).
            if ( ch >= uint32_t( channels ) )
               throw mpp::HostError( "worker sent a band for a nonexistent channel" );
            if ( startRow + rows > read_height_ )
               throw mpp::HostError( "worker sent a band exceeding the image height" );

            // Native copy: the incoming samples are already `sample`-typed,
            // row-major, native-endian -- exactly the destination plane's
            // layout. PixelData(ch) is the channel's first sample; offset by
            // startRow full rows.
            sample* dst = image.PixelData( int( ch ) ) + size_t( startRow )*size_t( width );
            std::memcpy( dst, samples, size_t( rows )*size_t( width )*sizeof( sample ) );
         } );
   }
   catch ( const mpp::HostError& e )
   {
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }
}

void BigTiffInstance::ReadImage( pcl::Image& image )
{
   ReadInto( image, "f32" );
}

void BigTiffInstance::ReadImage( pcl::DImage& image )
{
   ReadInto( image, "f64" );
}

void BigTiffInstance::ReadImage( UInt8Image& image )
{
   ReadInto( image, "u8" );
}

void BigTiffInstance::ReadImage( UInt16Image& image )
{
   ReadInto( image, "u16" );
}

void BigTiffInstance::ReadImage( UInt32Image& image )
{
   ReadInto( image, "u32" );
}

// ----------------------------------------------------------------------------

bool BigTiffInstance::QueryOptions( Array<ImageOptions>& options, Array<void*>& formatOptions )
{
   // Called once, just before Create(), typically for File > Save As
   // (FileFormatImplementation.h:461-506). Returning false aborts the save,
   // which is exactly what cancelling the dialog must do.

   // The core passes one ImageOptions per image and says formats that do not
   // support multiple images may ignore all but the first
   // (FileFormatImplementation.h:465-475). This format writes a single image
   // per file, so only the first is meaningful; an empty array means the
   // caller offered no starting point, so start from the PCL defaults.
   const ImageOptions imageOptions = options.IsEmpty() ? ImageOptions() : options[0];

   // Same for the format-specific block, with one extra step: the pointer may
   // be null, or a foreign block from another format entirely, so it is only
   // read after TiffFormatOptions::IsValid() confirms it is ours. Otherwise
   // the struct's own defaults stand -- note the default container is Auto,
   // not Big: this format owns .tif/.tiff now, so a save that fits in a
   // classic TIFF should be one.
   TiffFormatOptions tiffOptions;
   if ( !formatOptions.IsEmpty() )
   {
      const void* existing = formatOptions[0];
      if ( TiffFormatOptions::IsValid( existing ) )
         tiffOptions = *reinterpret_cast<const TiffFormatOptions*>( existing );
   }

   // imageHasAlpha is unconditionally true here, and cannot be anything else:
   // QueryOptions() runs BEFORE Create(), so no geometry has been handed to
   // this instance yet and there is no way to ask whether this particular
   // image carries an alpha channel. The alpha control is therefore always
   // offered, and reconciling the user's choice with the image that actually
   // arrives is CreateImage()'s job -- an image with no alpha channel forces
   // the association to "none" on the wire regardless of what was chosen
   // here, because the image, not the dialog, is the authority.
   TiffOptionsDialog dialog( imageOptions, tiffOptions, /*imageHasAlpha=*/true );
   if ( dialog.Execute() != StdDialogCode::Ok )
      return false;   // aborts the save

   options.Clear();
   options.Add( dialog.GetImageOptions() );

   // Ownership: this block is allocated here and released by
   // BigTiffFormat::DisposeFormatSpecificData(), both inside this module. The
   // unique_ptr covers the window in which Add() could throw; on success the
   // core owns the raw pointer and we must not touch it again.
   //
   // Any pointer already in the array is deliberately NOT deleted: nothing in
   // the API transfers ownership of an incoming block to us (only
   // DisposeFormatSpecificData does that), so freeing one here risks a
   // double-free in the core, which is far worse than the at-most-one small
   // block a core that does not dispose it would strand.
   std::unique_ptr<TiffFormatOptions> block(
      new TiffFormatOptions( dialog.GetFormatOptions() ) );
   formatOptions.Clear();
   formatOptions.Add( block.get() );
   (void)block.release();

   return true;
}

// ----------------------------------------------------------------------------

void BigTiffInstance::Create( const String& filePath, int numberOfImages, const IsoString& hints )
{
   (void)hints;   // no hint vocabulary in v1 (Unknowns §10)

   if ( open_ )
      throw Error( "BigTIFF: internal error: Create() called on an instance that is already open." );

   // A single-image format should reject zero and multiple images; a
   // negative count means "unknown" and is treated as one image
   // (FileFormatImplementation.h:491-504; Unknowns §9).
   if ( numberOfImages == 0 || numberOfImages > 1 )
      throw Error( String().Format( "BigTIFF: only single-image files are supported "
                                    "(%d images requested).", numberOfImages ) );

   // Safety net: never leave a save at a path nothing downstream can reopen.
   // Rewrite any path lacking a TIFF-family tail to "<path>.tiff" and note the
   // rename on the Console.
   //
   // Historically this fired constantly: while ".big.tiff" was
   // FileExtensions()[0], choosing this format in Save As appended a bare
   // ".big" (design spec finding 5 -- the dialog splits a compound extension
   // at the first dot), and a ".big" file is unusable downstream (Photoshop
   // won't take it, PI's chooser won't show it). Now that ".tif" leads and the
   // compound extensions are gone (see BigTiffFormat::FileExtensions()), the
   // dialog appends ".tif" and this branch should not be reached from a normal
   // save -- it is kept for paths typed by hand or supplied by a script.
   String path = filePath;
   const bool hasTiffTail = path.EndsWithIC( ".tiff" ) || path.EndsWithIC( ".tif" ) ||
                            path.EndsWithIC( ".btf" ) || path.EndsWithIC( ".tf8" );
   if ( !hasTiffTail )
   {
      const String rewritten = path + ".tiff";
      Console().WarningLn( "** BigTIFF: output path '" + path + "' has no TIFF-family "
                           "extension; writing to '" + rewritten + "' instead." );
      path = rewritten;
   }

   // Resolve the worker now so a missing installation fails the save
   // immediately, before any options/image handoff.
   worker_path_ = ResolveWorkerPath();
   path_ = path;   // may be relative (FileFormatImplementation.h:486-489); passed through as-is
   open_ = true;
}

void BigTiffInstance::SetOptions( const ImageOptions& options )
{
   // Advisory (FileFormatInstance.h:761-762): stored here, validated and
   // mapped when the authoritative geometry arrives in CreateImage().
   options_ = options;
   have_options_ = true;
}

void BigTiffInstance::SetFormatSpecificData( const void* data )
{
   // Advisory, exactly like SetOptions(): stored here, applied when the
   // authoritative geometry arrives in CreateImage().
   //
   // The core promises to call BigTiffFormat::ValidateFormatSpecificData()
   // first and to skip this call if that returns false
   // (FileFormatImplementation.h:600-604), but the same block also reaches us
   // through intermodule communication, so the identity test is repeated here
   // rather than assumed. A block that fails it leaves have_format_options_
   // false and the defaults in force.
   if ( TiffFormatOptions::IsValid( data ) )
   {
      format_options_ = *reinterpret_cast<const TiffFormatOptions*>( data );
      have_format_options_ = true;
   }
}

void* BigTiffInstance::FormatSpecificData() const
{
   // A fresh copy for the caller, who becomes its owner and releases it
   // through BigTiffFormat::DisposeFormatSpecificData(); this instance keeps
   // format_options_. Returning the address of the member instead would hand
   // out a pointer into an object the core does not own and would outlive
   // nothing -- the core would eventually delete a member of this instance.
   //
   // nullptr when nothing was set is the documented "no such data have been
   // retrieved" answer (FileFormatImplementation.h:214-221). The read path
   // never sets these options, so an opened file reports none.
   if ( !have_format_options_ )
      return nullptr;
   return new TiffFormatOptions( format_options_ );
}

void BigTiffInstance::CreateImage( const ImageInfo& info )
{
   if ( !open_ )
      throw Error( "BigTIFF: internal error: CreateImage() called before Create()." );
   if ( session_ != nullptr )
      throw Error( "BigTIFF: only single-image files are supported (CreateImage() called twice)." );

   if ( info.width <= 0 || info.height <= 0 )
      throw Error( "BigTIFF: invalid image geometry." );
   if ( info.colorSpace != ColorSpace::Gray && info.colorSpace != ColorSpace::RGB )
      throw Error( "BigTIFF: only grayscale and RGB images are supported." );
   // Colour channels plus an optional alpha plane: the engine accepts 1, 2, 3
   // or 4 channels (gray, gray+alpha, RGB, RGBA -- TiffWriteOptions::validate).
   const int colourChannels = ( info.colorSpace == ColorSpace::Gray ) ? 1 : 3;
   if ( info.numberOfChannels != colourChannels && info.numberOfChannels != colourChannels + 1 )
      throw Error( String().Format(
         "BigTIFF: %d channels are not supported for this color space "
         "(expected %d, or %d with an alpha channel).",
         info.numberOfChannels, colourChannels, colourChannels + 1 ) );
   const bool hasAlpha = info.numberOfChannels == colourChannels + 1;

   // Sample-format mapping. Protocol v3 carries bands in their native type,
   // so this is now an identity map rather than the narrowing table phase 2
   // shipped: what the user asks for is what the file gets. If SetOptions()
   // never ran (Unknowns §3), the default-constructed ImageOptions (16-bit
   // integer) select "u16".
   if ( options_.complexSample )
      throw Error( "BigTIFF: complex sample formats are not supported." );
   if ( !options_.ieeefpSampleFormat && options_.signedIntegers )
      throw Error( "BigTIFF: signed integer sample formats are not supported." );
   std::string bitDepth;
   if ( options_.ieeefpSampleFormat )
      switch ( options_.bitsPerSample )
      {
      case 32: bitDepth = "f32"; break;
      case 64: bitDepth = "f64"; break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit floating-point samples are not "
                                       "supported.", int( options_.bitsPerSample ) ) );
      }
   else
      switch ( options_.bitsPerSample )
      {
      case 8:  bitDepth = "u8"; break;
      case 16: bitDepth = "u16"; break;
      case 32: bitDepth = "u32"; break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit integer samples are not supported.",
                                       int( options_.bitsPerSample ) ) );
      }
   // v3: bands cross shm in the output's own type. Setting the two from one
   // variable is deliberate -- the protocol's coupling rule (band_format must
   // equal bit_depth unless it is the converting "f32", see
   // WriteSessionConfig::band_format) is satisfied by construction, and the
   // worker rejects an Init where they drift apart.
   band_format_ = bitDepth;

   // The alpha contract, both directions. The dialog's alpha item may say
   // "not written", and an image may carry no alpha channel at all; the two
   // must be reconciled here because the IMAGE, not the dialog, is the
   // authority on whether an alpha channel exists (QueryOptions() runs before
   // any geometry is known, so it always offers the control).
   //
   //   image has alpha + "not written" -> drop the plane: send the colour
   //      channels alone and discard the alpha WriteSamples call, rather than
   //      sending a channel count with alpha="none" that
   //      TiffWriteOptions::validate rejects as a multiband image;
   //   image has no alpha -> force alpha="none" whatever was chosen, or the
   //      same validator rejects an alpha association with 1 or 3 channels.
   //
   // Either mismatch would otherwise surface as a save-time error from a
   // dialog that promised the save would work.
   std::string alphaMode = "none";
   discarded_channel_ = -1;
   uint64_t wireChannels = uint64_t( info.numberOfChannels );
   if ( hasAlpha )
      switch ( format_options_.alpha )
      {
      case TiffAlpha::Unassociated: alphaMode = "unassociated"; break;
      case TiffAlpha::Associated:   alphaMode = "associated"; break;
      case TiffAlpha::None:
      default:
         // The user asked for the alpha channel not to be written.
         discarded_channel_ = colourChannels;
         wireChannels = uint64_t( colourChannels );
         break;
      }

   width_ = uint64_t( info.width );
   height_ = uint64_t( info.height );
   channels_ = wireChannels;

   // ~4 MiB per band (plan contract) measured in the NEGOTIATED band sample
   // width, not sizeof( float ): an f64 session would otherwise commit twice
   // the intended shm per slot. At least 1 row, at most the whole image.
   uint64_t bandSampleBytes = 0;
   try
   {
      bandSampleBytes = uint64_t( mpp::WriteSession::SampleBytes( band_format_ ) );
   }
   catch ( const mpp::HostError& e )
   {
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }
   const uint64_t kTargetBandBytes = uint64_t( 4 )*1024*1024;
   uint64_t bandRows = kTargetBandBytes/( width_*bandSampleBytes );
   bandRows = std::max<uint64_t>( 1, std::min<uint64_t>( bandRows, height_ ) );
   band_rows_ = uint32_t( bandRows );

   mpp::WriteSessionConfig cfg;
   cfg.worker_exe = worker_path_;
   cfg.output_path = std::string( path_.ToUTF8().c_str() );
   cfg.width = width_;
   cfg.height = height_;
   cfg.channels = channels_;
   cfg.bit_depth = bitDepth;
   cfg.band_rows = band_rows_;
   cfg.slots = 4;   // >= 2 recommended (mpp_host.h); 4 keeps bands in flight
   cfg.band_format = band_format_;

   // The TIFF-specific half of the options. format_options_ holds the user's
   // choices when SetFormatSpecificData() ran and the struct's own defaults
   // otherwise (Unknowns §3 -- a save driven by another module never calls
   // it), and those defaults are the behaviour that save should get.
   switch ( format_options_.container )
   {
   case TiffContainer::Classic: cfg.container = "classic"; break;
   case TiffContainer::Big:     cfg.container = "big"; break;
   case TiffContainer::Auto:
   default:                     cfg.container = "auto"; break;
   }
   switch ( format_options_.layout )
   {
   case TiffLayout::Tiles:  cfg.layout = "tiles"; break;
   case TiffLayout::Strips:
   default:                 cfg.layout = "strips"; break;
   }
   switch ( format_options_.compression )
   {
   case TiffCompression::Lzw:      cfg.compression = "lzw"; break;
   case TiffCompression::Deflate:  cfg.compression = "deflate"; break;
   case TiffCompression::Packbits: cfg.compression = "packbits"; break;
   case TiffCompression::None:
   default:                        cfg.compression = "none"; break;
   }
   cfg.tile_size = format_options_.tileSize;
   cfg.deflate_level = format_options_.deflateLevel;
   // TIFF tag 317 is defined for integer samples only and the engine rejects
   // the combination, so a predictor never reaches the wire alongside a
   // floating-point output format. The dialog already refuses to harvest one
   // (TiffOptionsDialog::UpdateEnableStates), but the block can also arrive
   // from a script or another module whose ImageOptions disagree with it.
   const bool floatOutput = ( bitDepth == "f32" || bitDepth == "f64" );
   cfg.predictor = format_options_.predictor && !floatOutput;
   cfg.alpha = alphaMode;
   cfg.description = TagText( format_options_.description );
   cfg.copyright = TagText( format_options_.copyright );
   cfg.software = TagText( format_options_.software );

   // Resolution: ImageOptions models it, so it comes from there rather than
   // the TIFF block. The worker writes the tags only when both values are
   // positive; PCL's own "0 = unknown" convention therefore passes straight
   // through.
   cfg.x_resolution = options_.xResolution;
   cfg.y_resolution = options_.yResolution;
   cfg.resolution_unit = options_.metricResolution ? "cm" : "inch";

   // ICC profile stashed by WriteICCProfile(), which the core calls before
   // this function. options_.embedICCProfile is the core's own switch for
   // whether a profile should be embedded at all; honour it rather than
   // embedding a profile a caller explicitly asked us not to.
   if ( !icc_profile_.empty() && options_.embedICCProfile )
      cfg.icc_profile_base64 = EncodeBase64( icc_profile_ );

   try
   {
      // Creates the shm segment, spawns the worker and sends Init; on any
      // failure the WriteSession constructor has already killed+reaped a
      // spawned worker before throwing.
      session_ = std::make_unique<mpp::WriteSession>( cfg );
   }
   catch ( const mpp::HostError& e )
   {
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }
}

void BigTiffInstance::CloseImage()
{
   // Idempotent (Unknowns §3). Moving the session out before Finish also
   // guarantees Close() can never Cancel() a session Finish was attempted
   // on (the WriteSession API forbids Cancel-after-Finish).
   if ( session_ == nullptr )
      return;
   std::unique_ptr<mpp::WriteSession> session = std::move( session_ );
   try
   {
      // No progress callback: nothing documents which thread drives this
      // instance (Unknowns §7), so the shim never touches GUI/Console.
      // Done's bytes_written is not needed here -- the worker's exit code
      // and Done frame are validated inside Finish.
      (void)session->Finish( std::function<void( mpp::ProgressInfo )>() );
   }
   catch ( const mpp::HostError& e )
   {
      // Worker Error frames / unexpected exits surface with the worker's
      // own message (behavioral contract).
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }
}

// ----------------------------------------------------------------------------

template <typename T>
void BigTiffInstance::PushTyped( const T* buffer, int startRow, int rowCount, int channel )
{
   if ( session_ == nullptr )
      throw Error( "BigTIFF: internal error: WriteSamples() called with no image created "
                   "(CreateImage() must run first)." );   // Unknowns §3
   if ( buffer == nullptr )
      throw Error( "BigTIFF: internal error: WriteSamples() called with a null buffer." );
   if ( startRow < 0 || rowCount < 0 || channel < 0 )
      throw Error( "BigTIFF: internal error: WriteSamples() called with negative "
                   "coordinates." );
   if ( rowCount == 0 )
      return;
   // The alpha plane of an image whose save options say "not written": the
   // session was opened for the colour channels alone, so this buffer has
   // nowhere to go and dropping it is what the user asked for (see the alpha
   // contract in CreateImage()). Only ever one index, never a general
   // out-of-range escape hatch -- any other bad channel still reaches
   // PushBandBytes, which rejects it against the Init geometry.
   if ( discarded_channel_ >= 0 && channel == discarded_channel_ )
      return;

   // Whether this buffer's own type survives the trip intact. Recorded on
   // every call rather than per overload: the conversion, not the overload,
   // is what can lose information (Unknowns §5 -- the overload that fires
   // need not match the negotiated format).
   if ( ConversionIsLossy<T>( band_format_ ) )
      lossy_ = true;

   const bool nativeBands = ( band_format_ == NativeBandFormat<T>() );

   try
   {
      // Split into <= band_rows_ chunks; channel/row-range validity is
      // enforced by WriteSession::PushBandBytes against the Init geometry.
      uint64_t row = uint64_t( startRow );
      uint64_t remaining = uint64_t( rowCount );
      const T* p = buffer;
      std::vector<uint8_t> scratch;
      const size_t dstSampleBytes = mpp::WriteSession::SampleBytes( band_format_ );
      while ( remaining > 0 )
      {
         const uint32_t rows = uint32_t( std::min<uint64_t>( remaining, band_rows_ ) );
         const uint64_t n = uint64_t( rows )*width_;
         const void* samples;
         if ( nativeBands )
            samples = p;   // negotiated type == this overload's type: no conversion, no copy
         else
         {
            scratch.resize( size_t( n )*dstSampleBytes );
            ConvertBuffer( p, scratch.data(), n, band_format_ );
            samples = scratch.data();
         }
         session_->PushBandBytes( uint32_t( channel ), row, rows, samples );
         p += n;
         row += rows;
         remaining -= rows;
      }
   }
   catch ( const mpp::HostError& e )
   {
      // The session is no longer usable after a fault; dropping it kills and
      // reaps the worker (RAII) so a failed save never leaves one running.
      session_.reset();
      throw Error( String( "BigTIFF: " ) + String( e.what() ) );
   }
}

// All five overloads are now one call: PushTyped() knows this buffer's type
// and the negotiated band type, so it -- not the overload -- decides whether
// a conversion is needed and whether it narrows. In the ordinary save the two
// agree and the buffer reaches shm untouched.

void BigTiffInstance::WriteSamples( const pcl::Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushTyped( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const pcl::DImage::sample* buffer, int startRow, int rowCount, int channel )
{
   PushTyped( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const UInt8Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushTyped( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const UInt16Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushTyped( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const UInt32Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushTyped( buffer, startRow, rowCount, channel );
}

// ----------------------------------------------------------------------------

template <class Img>
void BigTiffInstance::WriteWholeImage( const Img& image )
{
   // Unknowns §4: the headers nowhere promise an incremental format is only
   // driven through CreateImage/WriteSamples -- synthesize the incremental
   // path here. PixelData() addresses whole-channel rows, so a
   // selection-limited source would silently write wrong geometry: reject
   // anything but a full-image selection.
   if ( !( image.SelectedRectangle() == image.Bounds() ) ||
        image.NumberOfSelectedChannels() != image.NumberOfChannels() )
      throw Error( "BigTIFF: partial-image (selection-limited) saves are not supported." );

   ImageInfo info( image );
   CreateImage( info );
   for ( int c = 0; c < info.numberOfChannels; ++c )
      WriteSamples( image.PixelData( c ), 0, info.height, c );
   CloseImage();
}

void BigTiffInstance::WriteImage( const pcl::Image& image )
{
   WriteWholeImage( image );
}

void BigTiffInstance::WriteImage( const pcl::DImage& image )
{
   WriteWholeImage( image );
}

void BigTiffInstance::WriteImage( const UInt8Image& image )
{
   WriteWholeImage( image );
}

void BigTiffInstance::WriteImage( const UInt16Image& image )
{
   WriteWholeImage( image );
}

void BigTiffInstance::WriteImage( const UInt32Image& image )
{
   WriteWholeImage( image );
}

// ----------------------------------------------------------------------------

bool BigTiffInstance::WasLossyWrite() const
{
   // True iff a conversion that actually loses information happened on this
   // instance's write path -- a DImage the user chose to save as 16-bit, say
   // (Unknowns §6: the flag is kept accurate at all times regardless of when
   // the core queries it).
   //
   // Since protocol v3 that no longer includes u32 or f64 OUTPUT: those bands
   // cross shm and reach the file in their own type, exactly. Before phase 4
   // both were narrowed to f32 and both reported lossy here; reporting them
   // now would be as untrue as the capabilities were then. See
   // ConversionIsLossy() for the full rule.
   return lossy_;
}

// ----------------------------------------------------------------------------

ICCProfile BigTiffInstance::ReadICCProfile()
{
   // Called only while BigTiffFormat::CanStoreICCProfiles() is true, and only
   // for an open file. An empty ICCProfile is the documented "no profile"
   // answer (FileFormatImplementation.h:240-248), which is what a file
   // without tag 34675 gets.
   if ( !open_ || !read_mode_ )
      throw Error( "BigTIFF: internal error: ReadICCProfile() called before a successful "
                   "Open()." );
   if ( read_icc_base64_.empty() )
      return ICCProfile();

   // The blob came from our own worker's standard-alphabet base64 encoder, so
   // a failure here is an internal inconsistency or a corrupt profile in the
   // file -- either way loud beats silent, since silently returning "no
   // profile" would let a colour-managed image open as if it were untagged.
   try
   {
      return ICCProfile( IsoString( read_icc_base64_.c_str() ).FromBase64() );
   }
   catch ( const Exception& e )
   {
      throw Error( String( "BigTIFF: the ICC profile embedded in '" ) + path_ +
                   "' could not be decoded: " + e.Message() );
   }
}

void BigTiffInstance::WriteICCProfile( const ICCProfile& icc )
{
   // PCL calls this before CreateImage() on the write path. Store the bytes;
   // CreateImage() base64s them into the v3 Init. Arriving after the session
   // exists would mean the profile could not reach the file, so say so
   // rather than dropping it silently.
   if ( session_ != nullptr )
      throw Error( "BigTIFF: internal error: an ICC profile arrived after the image was "
                   "created; it cannot be embedded." );
   if ( icc.IsProfile() )
      icc_profile_.assign( icc.ProfileData().Begin(), icc.ProfileData().End() );
   else
      icc_profile_.clear();   // an empty profile means "embed none"
}

// ----------------------------------------------------------------------------

} // namespace pcl
