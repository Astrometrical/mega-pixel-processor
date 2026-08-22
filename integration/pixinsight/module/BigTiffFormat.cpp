// BigTiffFormat.cpp -- see BigTiffFormat.h.

#include "BigTiffFormat.h"

#include "BigTiffInstance.h"
#include "MppVersion.h"

namespace pcl
{

// ----------------------------------------------------------------------------

IsoString BigTiffFormat::Name() const
{
   // Unique valid-C-identifier format id (FileFormatBase.h:81-83).
   return "BigTIFF";
}

StringList BigTiffFormat::FileExtensions() const
{
   // ".xxx" strings in priority order (FileFormatBase.h:89-91).
   return StringList{ ".btf", ".tf8" };
}

IsoStringList BigTiffFormat::MimeTypes() const
{
   // BigTIFF is TIFF-family; IANA registers image/tiff for it.
   return IsoStringList{ "image/tiff" };
}

uint32 BigTiffFormat::Version() const
{
   // Format version as hex: 0xMmr (FileFormatBase.h:109-116; 0x100 = 1.0.0).
   // Kept in step with MPP_MODULE_VERSION_STRING (0.1.0 -> 0x010).
   return 0x010;
}

String BigTiffFormat::Description() const
{
   return "BigTIFF (write-only, Astrometrical Mega Pixel Processor)";
}

String BigTiffFormat::Implementation() const
{
   return "Clean-room, write-only BigTIFF encoder for very large images. "
          "Pixel data are streamed over shared memory to the mpp-ipc-worker "
          "process installed beside this module, which performs the actual "
          "BigTIFF encoding. Implemented by Astrometrical's Mega Pixel "
          "Processor (mpp) project from the public TIFF/BigTIFF "
          "specifications and the public PCL API only; see the module's "
          "provenance README (integration/pixinsight/README.md in the mpp "
          "source distribution). Version " MPP_MODULE_VERSION_STRING ".";
}

// ----------------------------------------------------------------------------

bool BigTiffFormat::CanRead() const
{
   return false;   // write-only by design; BigTiffInstance::Open() also throws
}

bool BigTiffFormat::CanWrite() const
{
   return true;
}

bool BigTiffFormat::CanWriteIncrementally() const
{
   // Required, or WriteSamples() is never called
   // (FileFormatImplementation.h:718-721).
   return true;
}

bool BigTiffFormat::CanStore32Bit() const
{
   // u32 saves are accepted and mapped u32/(2^32-1) into [0,1] f32 -- a
   // documented precision reduction (behavioral contract mapping table).
   return true;
}

bool BigTiffFormat::CanStoreFloat() const
{
   return true;   // f32 is the native path (shm bands are f32)
}

bool BigTiffFormat::CanStoreDouble() const
{
   // f64 saves are accepted and narrowed to f32 -- lossy, reported via
   // BigTiffInstance::WasLossyWrite().
   return true;
}

bool BigTiffFormat::CanStoreAlphaChannels() const
{
   // The wire protocol (docs/pi-protocol.md §5) accepts channels == 1 or 3
   // only, so alpha channels cannot be stored.
   return false;
}

// ----------------------------------------------------------------------------

FileFormatImplementation* BigTiffFormat::Create() const
{
   // New instance per file operation (MetaFileFormat.h:835-845).
   return new BigTiffInstance( this );
}

// ----------------------------------------------------------------------------

} // namespace pcl
