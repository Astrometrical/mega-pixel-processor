// BigTiffInstance.h -- FileFormatImplementation subclass: one instance per
// TIFF/BigTIFF file being read or written. On the write path it owns a
// mpp::WriteSession (the PCL-free host driver,
// integration/pixinsight/host/mpp_host.h) and converts PixInsight's
// incremental write calls into bands streamed to the mpp-ipc-worker process
// found beside the module binary. Since protocol v3 those bands cross shared
// memory in the OUTPUT file's own sample type (band_format_), not in unit-
// range f32: u32 and f64 saves are exact, which is what makes
// BigTiffFormat::CanStore32Bit()/CanStoreDouble() honest. On the read path Open() runs a
// lightweight geometry probe to describe the file, then each ReadImage
// overload runs a mpp::ReadSession that decodes the file and scatters native
// per-channel row bands into the destination image (design "Two-phase worker
// interaction").
//
// Defensive-coding provenance: every "Unknowns §N" comment below refers to
// PCL_FILEFORMAT_API.md §7 (the headers' documented behavior is not always a
// normative guarantee; this class codes for the gaps).
//
// Clean-room note: written from the public PCL headers, the project's
// PCL_FILEFORMAT_API.md reference, and MergeMosaic's own module code (the
// dladdr worker-resolution pattern). No PCL module implementation source was
// consulted.

#ifndef __BigTiffInstance_h
#define __BigTiffInstance_h

#include <pcl/FileFormatImplementation.h>
#include <pcl/ICCProfile.h>

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

#include "TiffFormatOptions.h"
#include "mpp_host.h"

namespace pcl
{

class BigTiffFormat;

// ----------------------------------------------------------------------------

/*!
 * \class BigTiffInstance
 * \brief Read+write TIFF/BigTIFF file instance: streams pixel bands to and
 *        from mpp-ipc-worker over shared memory.
 */
class BigTiffInstance : public FileFormatImplementation
{
public:

   explicit BigTiffInstance( const BigTiffFormat* format );

   // Destructor tears down any live session: ~WriteSession kills and reaps
   // the worker if it is still running (Unknowns §3: Close() might never be
   // called on error/abort paths -- the destructor must not rely on it).
   ~BigTiffInstance() override;

   // -- Instance state (Unknowns §2: base-class behavior of these virtuals
   //    is undocumented -- override all of them and track state here).

   ImageDescriptionArray Open( const String& filePath, const IsoString& hints ) override;
   bool IsOpen() const override;
   String FilePath() const override;
   void Close() override;

   // -- Read-path lifecycle (PCL_FILEFORMAT_API.md §R). The core drives an
   //    interactive open through whole-image ReadImage (never ReadSamples --
   //    CanReadIncrementally() is false); the overload matching the geometry
   //    declared by Open() fires, so only that one allocates + decodes, and
   //    the others throw (defensive per the "Unknowns" discipline: the
   //    headers do not promise which overload the core will call).

   void ReadImage( pcl::Image& image ) override;
   void ReadImage( pcl::DImage& image ) override;
   void ReadImage( UInt8Image& image ) override;
   void ReadImage( UInt16Image& image ) override;
   void ReadImage( UInt32Image& image ) override;

   // -- Write-path lifecycle (PCL_FILEFORMAT_API.md §3.3). QueryOptions()
   //    runs first and once, before Create(): it presents TiffOptionsDialog
   //    and returns the user's choices as the ImageOptions the core will
   //    hand back through SetOptions() and the TiffFormatOptions block it
   //    will hand back through SetFormatSpecificData().

   bool QueryOptions( Array<ImageOptions>& options, Array<void*>& formatOptions ) override;
   void Create( const String& filePath, int numberOfImages, const IsoString& hints ) override;
   void SetOptions( const ImageOptions& options ) override;
   void SetFormatSpecificData( const void* data ) override;
   void* FormatSpecificData() const override;
   void CreateImage( const ImageInfo& info ) override;
   void CloseImage() override;

   // -- ICC profiles (FileFormatImplementation.h:242-248, 608-611). Only
   //    called while BigTiffFormat::CanStoreICCProfiles() is true. On the
   //    write path the core calls WriteICCProfile() BEFORE CreateImage(),
   //    so the profile is stashed and base64'd into the session's Init;
   //    on the read path ReadICCProfile() decodes the base64 blob the
   //    worker reported in its ImageMeta.

   ICCProfile ReadICCProfile() override;
   void WriteICCProfile( const ICCProfile& icc ) override;

   // -- Incremental writes: all five overloads
   //    (FileFormatImplementation.h:725-757; Unknowns §5: the overload that
   //    fires need not agree with the SetOptions values, so each one
   //    converts its own buffer to the negotiated band type).

   void WriteSamples( const pcl::Image::sample* buffer, int startRow, int rowCount, int channel ) override;
   void WriteSamples( const pcl::DImage::sample* buffer, int startRow, int rowCount, int channel ) override;
   void WriteSamples( const UInt8Image::sample* buffer, int startRow, int rowCount, int channel ) override;
   void WriteSamples( const UInt16Image::sample* buffer, int startRow, int rowCount, int channel ) override;
   void WriteSamples( const UInt32Image::sample* buffer, int startRow, int rowCount, int channel ) override;

   // -- Whole-image writes (Unknowns §4: the headers nowhere promise an
   //    incremental format is only driven through WriteSamples -- implement
   //    these by synthesizing the incremental path).

   void WriteImage( const pcl::Image& image ) override;
   void WriteImage( const pcl::DImage& image ) override;
   void WriteImage( const UInt8Image& image ) override;
   void WriteImage( const UInt16Image& image ) override;
   void WriteImage( const UInt32Image& image ) override;

   bool WasLossyWrite() const override;

private:

   // Converts one WriteSamples buffer from PixInsight's sample type T to the
   // session's negotiated band type (band_format_) and pushes it as
   // <= band_rows_-row bands. Any mpp::HostError becomes a pcl::Error
   // carrying the worker's message.
   //
   // The conversion is an identity whenever the two agree, which is the
   // normal case: CreateImage() negotiates band_format_ from the same
   // ImageOptions the core used to pick the WriteSamples overload. When they
   // disagree (Unknowns §5: the overload that fires need not match
   // SetOptions), samples go through the unit range, and lossy_ records any
   // narrowing. A push for the alpha plane the user chose not to write
   // (see discarded_channel_) is dropped here.
   template <typename T>
   void PushTyped( const T* buffer, int startRow, int rowCount, int channel );

   // Synthesized whole-image write: CreateImage + per-channel WriteSamples +
   // CloseImage (Unknowns §4).
   template <class Img>
   void WriteWholeImage( const Img& image );

   // Whole-image read for the overload whose sample type matches the file's
   // declared bit_depth (\a wantDepth, e.g. "u16"): verifies the dispatch,
   // allocates \a image, runs a mpp::ReadSession, and copies each native
   // per-channel row band into the matching channel plane at its start row.
   // A non-matching overload throws (only the matching one should ever fire).
   // Any mpp::HostError becomes a pcl::Error carrying the worker's message.
   template <class Img>
   void ReadInto( Img& image, const char* wantDepth );

   String path_;                  // stored at Create()/Open(); may be relative (FileFormatImplementation.h:486-489)
   std::string worker_path_;      // resolved at Create()/Open() via dladdr (UTF-8)
   bool open_ = false;            // Create()/Open() ran, Close() has not
   bool read_mode_ = false;       // Open() ran (read path) rather than Create()

   // Geometry declared by Open()'s probe, consumed by the ReadImage overloads.
   uint64_t read_width_ = 0, read_height_ = 0, read_channels_ = 0;
   std::string read_bit_depth_;   // "u8" | "u16" | "u32" | "f32" | "f64"
   ColorSpace::value_type read_color_space_ = ColorSpace::Gray;
   // Base64 ICC blob the probe found embedded in the opened file, empty when
   // the file carries none; decoded on demand by ReadICCProfile(). Kept
   // encoded rather than decoded because an unqueried profile then costs
   // nothing but the bytes the probe already produced.
   std::string read_icc_base64_;

   bool have_options_ = false;    // SetOptions() ran (Unknowns §3: it might not)
   ImageOptions options_;         // advisory sample format for the next image

   // ICC profile bytes handed to WriteICCProfile() and awaiting base64
   // encoding into the next session's Init. In the same advisory idiom as
   // options_/format_options_: the core calls WriteICCProfile() before
   // CreateImage() only when it has a profile to embed, so "empty" is the
   // ordinary "no profile" state rather than an error. Cleared by Close().
   std::vector<uint8_t> icc_profile_;

   // The TIFF-specific half of the same handshake, in the same idiom and for
   // the same reason: SetFormatSpecificData() is advisory and the core is not
   // guaranteed to call it (Unknowns §3 -- it is skipped entirely for a save
   // driven by another module, and never called at all while
   // BigTiffFormat::UsesFormatSpecificData() is false). The flag says whether
   // a block actually arrived, so the write path can tell the user's choices
   // from the struct's own defaults instead of assuming a value.
   bool have_format_options_ = false;    // SetFormatSpecificData() ran with a valid block
   TiffFormatOptions format_options_;    // advisory TIFF options for the next image
   bool lossy_ = false;           // a narrowing conversion happened (Unknowns §6)
   uint64_t width_ = 0, height_ = 0, channels_ = 0;   // from CreateImage()

   // Wire sample type for this image's bands: "u8" | "u16" | "u32" | "f32" |
   // "f64", negotiated by CreateImage() from options_ and equal to the
   // output file's bit_depth (protocol v3's coupling rule -- see
   // WriteSessionConfig::band_format). Every WriteSamples buffer is
   // converted to this type before it reaches a shm slot, and it sizes the
   // slots, so band_rows_ is derived from its width and never sizeof(float).
   std::string band_format_;

   // Index of the channel CreateImage() decided not to write, or -1 when
   // every channel is written. Only ever the alpha plane of an image whose
   // save options say "not written": the channel count sent to the worker
   // then covers the colour channels alone, and a WriteSamples call for this
   // index is dropped rather than pushed (it would be out of range).
   int discarded_channel_ = -1;

   uint32_t band_rows_ = 0;       // rows per shm band (~4 MiB per band, min 1)
   std::unique_ptr<mpp::WriteSession> session_;       // live between CreateImage() and CloseImage()
};

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __BigTiffInstance_h
