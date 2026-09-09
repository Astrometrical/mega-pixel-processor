// TiffFormatOptions.h -- the format-specific data block BigTiffFormat
// passes between QueryOptions(), SetFormatSpecificData() and CreateImage().
//
// PCL owns this block's lifetime across a module boundary
// (MetaFileFormat::ValidateFormatSpecificData/DisposeFormatSpecificData),
// so it is STANDARD-LAYOUT and TRIVIALLY COPYABLE: fixed-width fields and
// fixed-size character arrays, no pointers, no std::string, nothing whose
// layout could differ between the allocating and freeing side, and nothing
// that needs a destructor run on the side that frees it.
//
// Those are the two properties, asserted below; note that the struct is
// deliberately NOT trivial and NOT a POD in the pre-C++20 sense, because the
// default member initializers give it a non-trivial default constructor.
// That is wanted -- the defaults are the module's contract -- and it is
// irrelevant to crossing the boundary, where all that matters is that the
// bytes may be memcpy'd and that destruction is a no-op. Do not "fix" a
// failing std::is_pod_v by deleting the initializers: that would silently
// drop every default in the struct.
//
// Everything the core already models -- sample format, resolution, ICC
// embedding -- lives in pcl::ImageOptions instead; this block carries only
// what TIFF adds on top.
//
// The enumerator vocabularies mirror the worker protocol's string values
// one for one (crates/mpp-ipc/src/protocol.rs, InitMessage): container
// "auto"|"classic"|"big", layout "strips"|"tiles", compression
// "none"|"lzw"|"deflate"|"packbits", alpha "none"|"unassociated"|
// "associated". Task 16 does the string mapping; this header only fixes the
// correspondence.

#ifndef __TiffFormatOptions_h
#define __TiffFormatOptions_h

#include <cstddef>
#include <cstdint>
#include <type_traits>

namespace pcl
{

// ----------------------------------------------------------------------------

/*!
 * \enum TiffCompression
 * \brief Block compression choices, matching the protocol's `compression`
 * values ("none", "lzw", "deflate", "packbits").
 */
enum class TiffCompression : uint8_t
{
   None     = 0,  //!< No compression; largest files, fastest writes.
   Lzw      = 1,  //!< LZW, universally readable.
   Deflate  = 2,  //!< zlib deflate; see TiffFormatOptions::deflateLevel.
   Packbits = 3   //!< PackBits RLE; cheap, weak on noisy astro data.
};

/*!
 * \enum TiffContainer
 * \brief Container choices, matching the protocol's `container` values
 * ("auto", "classic", "big").
 */
enum class TiffContainer : uint8_t
{
   Auto    = 0,  //!< Classic TIFF while the file fits, BigTIFF otherwise.
   Classic = 1,  //!< Classic TIFF; 4 GB ceiling, read by everything.
   Big     = 2   //!< BigTIFF; 64-bit offsets, no practical size limit.
};

/*!
 * \enum TiffLayout
 * \brief Block layout, matching the protocol's `layout` values
 * ("strips", "tiles").
 */
enum class TiffLayout : uint8_t
{
   Strips = 0,  //!< Row strips; the widest-compatibility layout.
   Tiles  = 1   //!< Square tiles; see TiffFormatOptions::tileSize.
};

/*!
 * \enum TiffAlpha
 * \brief Alpha association, matching the protocol's `alpha` values
 * ("none", "unassociated", "associated").
 */
enum class TiffAlpha : uint8_t
{
   None         = 0,  //!< No alpha channel is written.
   Unassociated = 1,  //!< ExtraSamples = 2; colour is not premultiplied.
   Associated   = 2   //!< ExtraSamples = 1; colour is premultiplied.
};

/*!
 * \brief Longest text a TIFF text tag may carry through this block,
 * including the terminating NUL.
 *
 * Longer input is truncated at the dialog, where the user can see it happen.
 */
constexpr size_t TiffTextTagCapacity = 256;

// ----------------------------------------------------------------------------

/*!
 * \struct TiffFormatOptions
 * \brief Per-save TIFF options that pcl::ImageOptions does not model.
 *
 * Standard-layout and trivially copyable by design: PCL, not this module,
 * decides when the block is copied and destroyed, so every field is
 * fixed-width and self-contained.
 */
struct TiffFormatOptions
{
   /*!
    * \brief Magic number identifying this block ("MPPT" in ASCII).
    */
   static constexpr uint32_t Magic = 0x4D505054u;

   /*!
    * \brief Current layout version.
    */
   static constexpr uint32_t Version = 1u;

   /*!
    * \brief Identifies a block as ours; checked by ValidateFormatSpecificData().
    */
   uint32_t magic = Magic;

   /*!
    * \brief Layout version of this struct; bumped whenever a field is added.
    */
   uint32_t version = Version;

   /*!
    * \brief Block compression method.
    */
   TiffCompression compression = TiffCompression::None;

   /*!
    * \brief TIFF container variant.
    */
   TiffContainer container = TiffContainer::Auto;

   /*!
    * \brief Strip or tile block layout.
    */
   TiffLayout layout = TiffLayout::Strips;

   /*!
    * \brief Alpha association for an image that carries an alpha channel.
    */
   TiffAlpha alpha = TiffAlpha::Unassociated;

   /*!
    * \brief Tile edge in pixels; a power of two in 128..=1024.
    *
    * Ignored unless #layout is TiffLayout::Tiles.
    */
   uint32_t tileSize = 256;

   /*!
    * \brief Deflate level 1-9. The encoder buckets this into fast/balanced/best.
    *
    * Ignored unless #compression is TiffCompression::Deflate.
    */
   uint8_t deflateLevel = 6;

   /*!
    * \brief Horizontal differencing predictor; integer sample formats only.
    *
    * TIFF tag 317 is defined for integer samples, and the engine rejects the
    * combination with a floating-point sample format.
    */
   bool predictor = false;

   /*!
    * \brief TIFF tag 270 (ImageDescription), NUL-terminated ASCII; empty when unset.
    */
   char description[ TiffTextTagCapacity ] = {};

   /*!
    * \brief TIFF tag 33432 (Copyright), NUL-terminated ASCII; empty when unset.
    */
   char copyright[ TiffTextTagCapacity ] = {};

   /*!
    * \brief TIFF tag 305 (Software), NUL-terminated ASCII; empty when unset.
    */
   char software[ TiffTextTagCapacity ] = {};
};

/*
 * The two properties the module boundary actually depends on, checked at
 * compile time because this header has no other automated gate:
 *
 *   - standard layout, so both sides of the boundary agree on the offset of
 *     every field regardless of which compiler saw the definition;
 *   - trivially copyable, so PCL may duplicate the block with a plain memcpy
 *     and release it without running a destructor.
 *
 * Deliberately NOT std::is_pod_v / std::is_trivial_v: the default member
 * initializers above make the default constructor non-trivial, which those
 * traits reject and which the boundary does not care about. See the note at
 * the top of this file.
 */
static_assert( std::is_standard_layout_v<TiffFormatOptions>,
               "TiffFormatOptions must be standard-layout: PCL and the module must agree on field offsets." );
static_assert( std::is_trivially_copyable_v<TiffFormatOptions>,
               "TiffFormatOptions must be trivially copyable: PCL copies and frees the block without running our code." );

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __TiffFormatOptions_h
