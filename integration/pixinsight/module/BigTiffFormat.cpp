// BigTiffFormat.cpp -- see BigTiffFormat.h.

#include "BigTiffFormat.h"

#include "BigTiffInstance.h"
#include "MppVersion.h"
#include "TiffFormatOptions.h"

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
   // True, and now genuinely so: protocol v3 carries u32 bands natively and
   // the writer emits 32-bit unsigned samples. Before phase 4 this was
   // advertised while the value was narrowed to f32 -- exactly the
   // truth-in-advertising violation this phase existed to fix.
   return true;
}

bool BigTiffFormat::CanStoreFloat() const
{
   return true;   // f32 bands and 32-bit float output, both exact
}

bool BigTiffFormat::CanStoreDouble() const
{
   return true;   // f64 bands and 64-bit float output, both exact
}

bool BigTiffFormat::CanStoreAlphaChannels() const
{
   // Gray+alpha and RGBA write an ExtraSamples tag with the association the
   // options dialog selected.
   return true;
}

bool BigTiffFormat::CanStoreResolution() const
{
   return true;   // XResolution/YResolution/ResolutionUnit from ImageOptions
}

bool BigTiffFormat::CanStoreICCProfiles() const
{
   return true;   // tag 34675, written on save and read back on open
}

// ----------------------------------------------------------------------------

bool BigTiffFormat::UsesFormatSpecificData() const
{
   // True, so the core round-trips our TiffFormatOptions block from
   // BigTiffInstance::QueryOptions() into the SetFormatSpecificData() that
   // precedes Create()/CreateImage() (MetaFileFormat.h:815-828,
   // FileFormatImplementation.h:576-605). While this returns false,
   // SetFormatSpecificData() is never called and the TIFF-specific half of
   // the options dialog would be discarded.
   return true;
}

bool BigTiffFormat::ValidateFormatSpecificData( const void* data ) const
{
   // The core knows nothing about these blocks and passes them among
   // instances of *different* formats, so anything reaching us here may be a
   // foreign pointer that must not be dereferenced as our own type. The
   // magic/version check is the whole of the test, and it lives on the struct
   // that owns the invariant.
   return TiffFormatOptions::IsValid( data );
}

void BigTiffFormat::DisposeFormatSpecificData( void* data ) const
{
   // Ownership arrives here and ends here: this is the only place a block
   // allocated by BigTiffInstance::QueryOptions() or FormatSpecificData() is
   // released. The new and the delete are both inside this module, so the
   // allocator matches.
   //
   // Gated on IsValid for the same reason Validate is: never call delete on a
   // pointer that is not one of ours. A block that fails the test was not
   // allocated by us, so declining to free it cannot leak anything we own.
   if ( !TiffFormatOptions::IsValid( data ) )
      return;
   delete reinterpret_cast<TiffFormatOptions*>( data );
}

// ----------------------------------------------------------------------------

FileFormatImplementation* BigTiffFormat::Create() const
{
   // New instance per file operation (MetaFileFormat.h:835-845).
   return new BigTiffInstance( this );
}

// ----------------------------------------------------------------------------

} // namespace pcl
