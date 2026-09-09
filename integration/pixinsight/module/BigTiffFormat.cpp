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
   //
   // This format CLAIMS ".tif"/".tiff" and REPLACES PixInsight's stock TIFF
   // module, which must be uninstalled before this module is installed
   // (design spec finding 1: the core refuses a second format claiming an
   // extension another format already owns). See the module README's
   // "Replacing the stock TIFF module" for the procedure and how to revert.
   //
   // Why we own them rather than coexist -- the full investigation is in
   // docs/tiff-parity.md, "Why we're here". In short: the Save As dialog
   // drops a TIFF-family format whose extensions collide with those of the
   // format currently selected as the default. Our former compound
   // ".big.tiff"/".big.tif" collided with the stock module's *.tif/*.tiff,
   // so this format vanished from the list whenever TIFF was the default --
   // for windows loaded from a .big.tiff, and for non-file-backed windows
   // after any TIFF save. Narrowing to ".btf"/".tf8" fixed the dialog but
   // broke the Photoshop bridge, since Photoshop recognises TIFF only by a
   // .tif/.tiff tail. No extension set satisfies both while the stock module
   // is installed; owning .tif/.tiff satisfies both at once.
   //
   // ".big.tiff"/".big.tif" are deliberately NOT claimed any more: owning the
   // ".tiff" tail routes existing files here regardless, and re-adding a
   // compound extension would reintroduce the collision described above.
   return StringList{ ".tif", ".tiff", ".btf", ".tf8" };
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
   // Product first, container second: this is one of the formats the Mega
   // Pixel Processor module provides, not the module's whole identity.
   return "Mega Pixel Processor: BigTIFF/TIFF (read+write)";
}

String BigTiffFormat::Implementation() const
{
   return "Astrometrical Mega Pixel Processor (mpp): file format support for "
          "astronomically large images. This format is mpp's clean-room "
          "BigTIFF/TIFF reader and BigTIFF encoder. Pixel data are streamed "
          "over shared memory to and from the mpp-ipc-worker process "
          "installed beside this module, which performs the actual "
          "TIFF/BigTIFF decoding (read) and BigTIFF encoding (write). "
          "Implemented from the public TIFF/BigTIFF specifications and the "
          "public PCL API only; see the module's provenance README "
          "(integration/pixinsight/README.md in the mpp source "
          "distribution). Version " MPP_MODULE_VERSION_STRING ".";
}

// ----------------------------------------------------------------------------

bool BigTiffFormat::CanRead() const
{
   return true;   // BigTiffInstance::Open()+ReadImage decode TIFF/BigTIFF files
}

bool BigTiffFormat::CanReadIncrementally() const
{
   // False, explicitly: the core drives interactive opens through whole-image
   // ReadImage, never ReadSamples, even when this returns true (empirically
   // verified, design "Findings" §3). Advertising incremental read would only
   // add an unused ReadSamples code path, so we own the whole-image decode
   // loop and leave ReadSamples unimplemented.
   return false;
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
