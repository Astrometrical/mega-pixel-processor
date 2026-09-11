// BigTiffFormat.h -- MetaFileFormat subclass describing the read+write
// BigTIFF/TIFF (.tif/.tiff/.btf/.tf8) format. Pure description/
// capability queries; all file I/O lives in BigTiffInstance (the
// FileFormatImplementation this format's Create() manufactures).
//
// This is ONE of the formats the Mega Pixel Processor module (MppModule,
// Mpp.cpp) provides -- the first, not the module's identity. Each further
// format is a sibling <Format>Format/<Format>Instance pair registered the
// same way; see Mpp.cpp's naming-convention note.
//
// Capability truth-in-advertising rule (plan "Behavioral contract for the
// module"): advertise only what the shim + worker actually do. See
// PCL_FILEFORMAT_API.md §2.1 for the full capability table with the PCL
// defaults each override below diverges from.
//
// Clean-room note: written from the public PCL headers and the project's
// PCL_FILEFORMAT_API.md reference only.

#ifndef __BigTiffFormat_h
#define __BigTiffFormat_h

#include <pcl/MetaFileFormat.h>

namespace pcl
{

// ----------------------------------------------------------------------------

/*!
 * \class BigTiffFormat
 * \brief Read+write BigTIFF/TIFF format description (extensions .tif,
 *        .tiff, .btf, .tf8).
 *
 * This format claims .tif/.tiff and replaces PixInsight's stock TIFF
 * module, which must be uninstalled first; see FileExtensions() in the .cpp
 * for why, and the module README for the swap/revert procedure.
 */
class BigTiffFormat : public MetaFileFormat
{
public:

   BigTiffFormat() = default;

   // -- Identity (the three MetaFileFormat pure virtuals + descriptive
   //    members; PCL_FILEFORMAT_API.md §2, §2.2).

   IsoString Name() const override;
   StringList FileExtensions() const override;
   IsoStringList MimeTypes() const override;
   uint32 Version() const override;
   String Description() const override;
   String Implementation() const override;
   IsoString IconImageSVG() const override;   // MPP_TIFF_FORMAT_ICON_SVG; default is the core's generic cube

   // -- Capabilities (PCL_FILEFORMAT_API.md §2.1). Only members whose value
   //    differs from the MetaFileFormat.h default -- or whose value is
   //    load-bearing enough to deserve an explicit statement (CanRead/
   //    CanWrite) -- are overridden; everything else keeps its default.

   bool CanRead() const override;                // default true  -> true (Open()+ReadImage read path)
   bool CanReadIncrementally() const override;   // default false -> false (explicit; whole-image ReadImage only)
   bool CanWrite() const override;               // default true  -> true (explicit for clarity)
   bool CanWriteIncrementally() const override;  // default false -> true (WriteSamples path)
   bool CanStore32Bit() const override;          // default false -> true (native u32 samples, exact)
   bool CanStoreFloat() const override;          // default false -> true (native f32 samples, exact)
   bool CanStoreDouble() const override;         // default false -> true (native f64 samples, exact)
   bool CanStoreAlphaChannels() const override;  // default true  -> true (gray+alpha and RGBA, ExtraSamples)
   bool CanStoreResolution() const override;     // default false -> true (X/YResolution + ResolutionUnit)
   bool CanStoreICCProfiles() const override;    // default false -> true (tag 34675, both directions)
   bool SupportsCompression() const override;    // default false -> true (LZW, Deflate, PackBits)

   // NOT overridden, deliberately: CanStoreKeywords() keeps its false
   // default. TIFF has no FITS-keyword equivalent -- the three text tags this
   // format writes (ImageDescription, Copyright, Software) are fixed,
   // single-valued tags carried as format-specific data (TiffFormatOptions),
   // not a keyword namespace a caller can add names to. Advertising keyword
   // storage would promise a FITSKeywordArray round-trip nothing here can
   // deliver, so this is not a gap to "fix".

   // -- Format-specific data (MetaFileFormat.h:815-852). The core preserves
   //    an opaque per-instance block for us -- our TiffFormatOptions -- and
   //    round-trips it between BigTiffInstance::QueryOptions() and the
   //    Create()/CreateImage() that follow. The core allocates nothing here:
   //    every block originates in this module (QueryOptions or
   //    FileFormatImplementation::FormatSpecificData) and comes back to this
   //    module to be released.

   bool UsesFormatSpecificData() const override;                     // default false -> true
   bool ValidateFormatSpecificData( const void* data ) const override;
   void DisposeFormatSpecificData( void* data ) const override;

   // -- Factory (MetaFileFormat.h:845).

   FileFormatImplementation* Create() const override;
};

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __BigTiffFormat_h
