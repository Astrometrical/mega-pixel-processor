// BigTiffInstance.cpp -- see BigTiffInstance.h.
//
// Worker resolution (ModuleDirectory/ResolveWorkerPath) follows MergeMosaic's
// proven pattern (MmmExecution.cpp, our code): the module's own on-disk
// location via dladdr (POSIX) / GetModuleFileNameW (Windows), the worker
// binary expected beside it, and a pcl::Error with install advice when it is
// missing.

#include "BigTiffInstance.h"

#include "BigTiffFormat.h"

#include <pcl/Exception.h>
#include <pcl/File.h>

#ifdef _WIN32
#  include <windows.h>
#else
#  include <dlfcn.h>
#endif

#include <algorithm>
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
                   "`mpp-ipc-worker.exe` beside BigTIFF-pxm.dll." );
#else
   String worker = dir + "mpp-ipc-worker";
   if ( !File::Exists( worker ) )
      throw Error( "BigTIFF: worker binary not found next to the module: " + worker +
                   "\nBuild it with `cargo build --release -p mpp-ipc-worker` and copy "
                   "`mpp-ipc-worker` beside BigTIFF-pxm.so." );
#endif
   return std::string( worker.ToUTF8().c_str() );
}

// Behavioral-contract sample mapping, applied per sample: f32 passes
// through, f64 narrows, unsigned integers divide by their type maximum.
// u8/u16 are exact in f32 (their maxima fit a 24-bit significand); u32 loses
// precision by design (documented precision note).
template <typename T>
float NormalizeSample( T v )
{
   if constexpr ( std::is_floating_point_v<T> )
      return float( v );
   else
   {
      static_assert( std::is_unsigned_v<T>, "integer samples are unsigned in PCL" );
      constexpr double maxv = double( std::numeric_limits<T>::max() );
      return float( double( v )/maxv );
   }
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
   (void)filePath;
   (void)hints;
   // Unknowns §2: the non-overridden base behavior is undocumented -- make
   // write-only explicit rather than inherit an unknown default.
   throw Error( "BigTIFF: this implementation is write-only; reading .btf/.tf8 files is not supported." );
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

   // Resolve the worker now so a missing installation fails the save
   // immediately, before any options/image handoff.
   worker_path_ = ResolveWorkerPath();
   path_ = filePath;   // may be relative (FileFormatImplementation.h:486-489); passed through as-is
   open_ = true;
}

void BigTiffInstance::SetOptions( const ImageOptions& options )
{
   // Advisory (FileFormatInstance.h:761-762): stored here, validated and
   // mapped when the authoritative geometry arrives in CreateImage().
   options_ = options;
   have_options_ = true;
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
   const int nominalChannels = ( info.colorSpace == ColorSpace::Gray ) ? 1 : 3;
   if ( info.numberOfChannels != nominalChannels )
      throw Error( String().Format( "BigTIFF: %d channels are not supported for this color "
                                    "space (expected %d; alpha channels cannot be stored).",
                                    info.numberOfChannels, nominalChannels ) );

   // Sample-format mapping (behavioral contract). If SetOptions() never ran
   // (Unknowns §3), the default-constructed ImageOptions (16-bit integer)
   // select "u16". The options are advisory (Unknowns §5): they choose the
   // OUTPUT sample format only -- every WriteSamples overload normalizes its
   // own buffer to f32 regardless, so a disagreeing overload still writes
   // correct data in the format negotiated here.
   if ( options_.complexSample )
      throw Error( "BigTIFF: complex sample formats are not supported." );
   if ( !options_.ieeefpSampleFormat && options_.signedIntegers )
      throw Error( "BigTIFF: signed integer sample formats are not supported." );
   std::string bitDepth;
   if ( options_.ieeefpSampleFormat )
      switch ( options_.bitsPerSample )
      {
      case 32:   // f32 -> "f32", the native path
      case 64:   // f64 -> "f32", narrowed (documented; lossiness is tracked
                 // where the narrowing actually happens, in WriteSamples)
         bitDepth = "f32";
         break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit floating-point samples are not "
                                       "supported.", int( options_.bitsPerSample ) ) );
      }
   else
      switch ( options_.bitsPerSample )
      {
      case 8:
         bitDepth = "u8";
         break;
      case 16:
         bitDepth = "u16";
         break;
      case 32:   // u32 -> f32 in [0,1] (u32/(2^32-1); documented precision note)
         bitDepth = "f32";
         break;
      default:
         throw Error( String().Format( "BigTIFF: %d-bit integer samples are not supported.",
                                       int( options_.bitsPerSample ) ) );
      }

   width_ = uint64_t( info.width );
   height_ = uint64_t( info.height );
   channels_ = uint64_t( info.numberOfChannels );

   // ~4 MiB of f32 per band (plan contract), at least 1 row, at most the
   // whole image.
   const uint64_t kTargetBandBytes = uint64_t( 4 )*1024*1024;
   uint64_t bandRows = kTargetBandBytes/( width_*sizeof( float ) );
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
void BigTiffInstance::PushNormalized( const T* buffer, int startRow, int rowCount, int channel )
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

   try
   {
      // Split into <= band_rows_ chunks; channel/row-range validity is
      // enforced by WriteSession::PushBand against the Init geometry.
      uint64_t row = uint64_t( startRow );
      uint64_t remaining = uint64_t( rowCount );
      const T* p = buffer;
      std::vector<float> scratch;
      while ( remaining > 0 )
      {
         const uint32_t rows = uint32_t( std::min<uint64_t>( remaining, band_rows_ ) );
         const uint64_t n = uint64_t( rows )*width_;
         const float* samples;
         if constexpr ( std::is_same_v<T, float> )
            samples = p;   // native path: no conversion, no copy
         else
         {
            scratch.resize( size_t( n ) );
            for ( uint64_t i = 0; i < n; ++i )
               scratch[size_t( i )] = NormalizeSample( p[i] );
            samples = scratch.data();
         }
         session_->PushBand( uint32_t( channel ), row, rows, samples );
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

void BigTiffInstance::WriteSamples( const pcl::Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushNormalized( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const pcl::DImage::sample* buffer, int startRow, int rowCount, int channel )
{
   lossy_ = true;   // f64 -> f32 narrowing (behavioral contract; Unknowns §6)
   PushNormalized( buffer, startRow, rowCount, channel );
}

void BigTiffInstance::WriteSamples( const UInt8Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushNormalized( buffer, startRow, rowCount, channel );   // exact in f32
}

void BigTiffInstance::WriteSamples( const UInt16Image::sample* buffer, int startRow, int rowCount, int channel )
{
   PushNormalized( buffer, startRow, rowCount, channel );   // exact in f32
}

void BigTiffInstance::WriteSamples( const UInt32Image::sample* buffer, int startRow, int rowCount, int channel )
{
   lossy_ = true;   // u32 precision exceeds f32's 24-bit significand (Unknowns §6)
   PushNormalized( buffer, startRow, rowCount, channel );
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
   // True iff a narrowing conversion happened on this instance's write path
   // (f64 or u32 sources; Unknowns §6 -- the flag is kept accurate at all
   // times regardless of when the core queries it). A float image saved
   // into a u16/u8 output file is a user-selected quantization, not a shim
   // conversion, and is not reported here.
   return lossy_;
}

// ----------------------------------------------------------------------------

} // namespace pcl
