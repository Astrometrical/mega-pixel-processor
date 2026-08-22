// BigTiffFormat.h -- MetaFileFormat subclass describing the write-only
// BigTIFF (.btf/.tf8) format. Pure description/capability queries; all file
// I/O lives in BigTiffInstance (the FileFormatImplementation this format's
// Create() manufactures).
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
 * \brief Write-only BigTIFF format description (extensions .btf, .tf8).
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

   // -- Capabilities (PCL_FILEFORMAT_API.md §2.1). Only members whose value
   //    differs from the MetaFileFormat.h default -- or whose value is
   //    load-bearing enough to deserve an explicit statement (CanRead/
   //    CanWrite) -- are overridden; everything else keeps its default.

   bool CanRead() const override;                // default true  -> false (write-only)
   bool CanWrite() const override;               // default true  -> true (explicit for clarity)
   bool CanWriteIncrementally() const override;  // default false -> true (WriteSamples path)
   bool CanStore32Bit() const override;          // default false -> true (u32 -> f32, precision note)
   bool CanStoreFloat() const override;          // default false -> true (f32 is the native path)
   bool CanStoreDouble() const override;         // default false -> true (f64 -> f32, lossy narrowing)
   bool CanStoreAlphaChannels() const override;  // default true  -> false (worker accepts 1 or 3 channels only)

   // -- Factory (MetaFileFormat.h:845).

   FileFormatImplementation* Create() const override;
};

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __BigTiffFormat_h
