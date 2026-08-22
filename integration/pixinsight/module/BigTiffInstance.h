// BigTiffInstance.h -- FileFormatImplementation subclass: one instance per
// .btf/.tf8 file being written. Owns a mpp::WriteSession (the PCL-free host
// driver, integration/pixinsight/host/mpp_host.h) and converts PixInsight's
// incremental write calls into f32 [0,1] bands streamed to the
// mpp-ipc-worker process found beside the module binary.
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

#include <cstdint>
#include <memory>
#include <string>

#include "mpp_host.h"

namespace pcl
{

class BigTiffFormat;

// ----------------------------------------------------------------------------

/*!
 * \class BigTiffInstance
 * \brief Write-only BigTIFF file instance: streams pixel bands to
 *        mpp-ipc-worker over shared memory.
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

   // -- Write-path lifecycle (PCL_FILEFORMAT_API.md §3.3).

   void Create( const String& filePath, int numberOfImages, const IsoString& hints ) override;
   void SetOptions( const ImageOptions& options ) override;
   void CreateImage( const ImageInfo& info ) override;
   void CloseImage() override;

   // -- Incremental writes: all five overloads
   //    (FileFormatImplementation.h:725-757; Unknowns §5: the overload that
   //    fires need not agree with the SetOptions values, so each one
   //    normalizes its own buffer).

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

   // Normalizes one WriteSamples buffer to f32 in [0,1] (behavioral-contract
   // mapping: f32 pass-through, f64 narrow, u8/u16/u32 divide by type max)
   // and pushes it as <= bandRows_-row bands. Any mpp::HostError becomes a
   // pcl::Error carrying the worker's message.
   template <typename T>
   void PushNormalized( const T* buffer, int startRow, int rowCount, int channel );

   // Synthesized whole-image write: CreateImage + per-channel WriteSamples +
   // CloseImage (Unknowns §4).
   template <class Img>
   void WriteWholeImage( const Img& image );

   String path_;                  // stored at Create(); may be relative (FileFormatImplementation.h:486-489)
   std::string worker_path_;      // resolved at Create() via dladdr (UTF-8)
   bool open_ = false;            // Create() ran, Close() has not
   bool have_options_ = false;    // SetOptions() ran (Unknowns §3: it might not)
   ImageOptions options_;         // advisory sample format for the next image
   bool lossy_ = false;           // a narrowing conversion happened (Unknowns §6)
   uint64_t width_ = 0, height_ = 0, channels_ = 0;   // from CreateImage()
   uint32_t band_rows_ = 0;       // rows per shm band (~4 MiB per band, min 1)
   std::unique_ptr<mpp::WriteSession> session_;       // live between CreateImage() and CloseImage()
};

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __BigTiffInstance_h
