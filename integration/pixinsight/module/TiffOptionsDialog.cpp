// TiffOptionsDialog.cpp -- implementation of the branded TIFF options dialog.
//
// The header notice (chevron bitmap, bold title, tagline, copyright and the
// hyperlink-styled labels built by the makeLink lambda), e_LogoPaint and
// e_LinkMouseRelease are reused from MergeMosaic's MmmInterface.cpp -- our
// own code -- so the two Astrometrical modules present the same face. Only
// the title, tagline and link set differ.
//
// Clean-room note: no PixInsight module implementation source was consulted.
// The control set is derived from the worker protocol's InitMessage fields
// (crates/mpp-ipc/src/protocol.rs) plus pcl::ImageOptions; the stock TIFF
// dialog is referenced only as a list of options a user sees on screen.

#include "TiffOptionsDialog.h"

#include "MppIcon.h"
#include "MppVersion.h"

#include <pcl/Cursor.h>
#include <pcl/ExternalProcess.h>
#include <pcl/Graphics.h>
#include <pcl/StringList.h>

namespace pcl
{

// ----------------------------------------------------------------------------
// Helpers.
// ----------------------------------------------------------------------------

namespace
{

/*
 * Copies a UI string into a fixed-size TIFF text-tag buffer.
 *
 * The Rust writer's `impl TiffValue for str` refuses non-ASCII bytes and
 * embedded NULs (Task 8), and TiffWriteOptions::validate now reports that as
 * a named error, so both are filtered out here rather than failing the save.
 * Truncation is to capacity-1 characters so the result is always
 * NUL-terminated. Doing it at harvest time means the user sees the accepted
 * text the next time the dialog is opened.
 */
void CopyAsciiTag( char* dst, size_t capacity, const String& src )
{
   size_t n = 0;
   for ( size_type i = 0; i < src.Length() && n < capacity-1; ++i )
   {
      const String::char_type c = src[i];
      // Printable 7-bit ASCII only: this drops embedded NULs, control
      // characters and everything above U+007E.
      if ( c >= 0x20 && c <= 0x7E )
         dst[n++] = char( c );
   }
   dst[n] = '\0';
   for ( size_t i = n+1; i < capacity; ++i )
      dst[i] = '\0';
}

/*
 * The tile-edge choices offered by the tile-size combo, in item order.
 * Every entry is a power of two in 128..=1024, the range the worker accepts.
 */
const uint32 s_tileSizes[] = { 128, 256, 512, 1024 };

/*
 * Combo item index for a tile edge, defaulting to 256 (item 1) for a value
 * that is not one of the offered sizes.
 */
int TileSizeItemIndex( uint32 tileSize )
{
   for ( size_t i = 0; i < ItemsInArray( s_tileSizes ); ++i )
      if ( s_tileSizes[i] == tileSize )
         return int( i );
   return 1;
}

} // namespace

// ----------------------------------------------------------------------------

TiffOptionsDialog::TiffOptionsDialog( const ImageOptions& imageOptions,
                                      const TiffFormatOptions& formatOptions,
                                      bool imageHasAlpha )
   : m_imageOptions( imageOptions )
   , m_formatOptions( formatOptions )
   , m_imageHasAlpha( imageHasAlpha )
{
   const int labelWidth1 = Font().Width( String( "Compression method:" ) + 'M' );
   const int editWidth1  = Font().Width( String( '0', 10 ) );

   //
   // Header notice: chevron logo + title / tagline / copyright / links.
   // Reused from MmmInterface::GUIData (our code) so both Astrometrical
   // modules wear the same mark.
   //
   Logo_Bitmap = Bitmap( MPP_CHEVRON_SVG, sizeof( MPP_CHEVRON_SVG ) - 1, "SVG" );

   Logo_Control.SetScaledFixedSize( 40, 40 );
   Logo_Control.OnPaint( (Control::paint_event_handler)&TiffOptionsDialog::e_LogoPaint, *this );

   // The em dash is written as a UTF-16 literal: a narrow string literal cannot
   // carry a code point above 0xFF, and pcl::String is UTF-16 throughout.
   Title_Label.SetText( String( u"Mega Pixel Processor " ) + String( MPP_MODULE_VERSION_STRING )
                        + String( u" \x2014 TIFF Options" ) );
   Title_Label.SetStyleSheet( ScaledStyleSheet( "QLabel { font-weight: bold; }" ) );

   Tagline_Label.SetText( "Astronomically large images." );

   Copyright_Label.SetText( "Copyright (c) 2026 Astrometrical" );

   // Hyperlink-styled labels: colored + underlined so they read as links,
   // pointing-hand cursor, and a shared mouse-release handler that dispatches
   // on the sender. The color works on both the dark (default) and light core
   // themes.
   auto makeLink = [this]( Label& label, const String& text, const String& toolTip )
   {
      label.SetText( text );
      label.SetToolTip( toolTip );
      label.SetCursor( StdCursor::PointingHand );
      label.SetStyleSheet( "QLabel { color: #4FA6FF; text-decoration: underline; }" );
      label.OnMouseRelease( (Control::mouse_button_event_handler)&TiffOptionsDialog::e_LinkMouseRelease, *this );
   };
   makeLink( ToolsLink_Label, "Astrometrical Tools",
             "<p>Visit https://tools.astrometrical.com/</p>" );
   makeLink( KofiLink_Label, "Buy me a coffee",
             "<p>Support development: https://ko-fi.com/astrometrical</p>" );

   LinkSep_Label.SetText( u"\x00B7" ); // middle dot separator

   Links_Sizer.SetSpacing( 6 );
   Links_Sizer.Add( ToolsLink_Label );
   Links_Sizer.Add( LinkSep_Label );
   Links_Sizer.Add( KofiLink_Label );
   Links_Sizer.AddStretch();

   NoticeText_Sizer.SetSpacing( 2 );
   NoticeText_Sizer.Add( Title_Label );
   NoticeText_Sizer.Add( Tagline_Label );
   NoticeText_Sizer.Add( Copyright_Label );
   NoticeText_Sizer.Add( Links_Sizer );

   Notice_Sizer.SetMargin( 6 );
   Notice_Sizer.SetSpacing( 8 );
   Notice_Sizer.Add( Logo_Control );
   Notice_Sizer.Add( NoticeText_Sizer, 100 );
   Notice_Control.SetSizer( Notice_Sizer );

   //
   // Sample Format section.
   //
   UInt8_RadioButton.SetText( "8-bit unsigned integer" );
   UInt8_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_SampleFormatClick, *this );
   UInt8_RadioButton.SetToolTip( "<p>Write 8 bits per sample. The smallest files, and enough only for "
      "an already stretched, display-ready image.</p>" );

   UInt16_RadioButton.SetText( "16-bit unsigned integer" );
   UInt16_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_SampleFormatClick, *this );
   UInt16_RadioButton.SetToolTip( "<p>Write 16 bits per sample. The most widely accepted format for "
      "stretched astronomical images, and what most editors expect.</p>" );

   UInt32_RadioButton.SetText( "32-bit unsigned integer" );
   UInt32_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_SampleFormatClick, *this );
   UInt32_RadioButton.SetToolTip( "<p>Write 32 bits per sample as unsigned integers. Twice the size of "
      "16-bit, with headroom few readers actually use.</p>" );

   Float32_RadioButton.SetText( "32-bit IEEE 754 floating point" );
   Float32_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_SampleFormatClick, *this );
   Float32_RadioButton.SetToolTip( "<p>Write 32-bit floating point samples. The native path for linear "
      "data: no quantization, and the format the engine works in internally.</p>"
      "<p>The horizontal predictor is not available for floating-point samples.</p>" );

   Float64_RadioButton.SetText( "64-bit IEEE 754 floating point" );
   Float64_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_SampleFormatClick, *this );
   Float64_RadioButton.SetToolTip( "<p>Write 64-bit floating point samples. Double the size of 32-bit "
      "float for precision no astronomical sensor produces; read by very few applications.</p>" );

   SampleFormat_Sizer.SetSpacing( 4 );
   SampleFormat_Sizer.Add( UInt8_RadioButton );
   SampleFormat_Sizer.Add( UInt16_RadioButton );
   SampleFormat_Sizer.Add( UInt32_RadioButton );
   SampleFormat_Sizer.Add( Float32_RadioButton );
   SampleFormat_Sizer.Add( Float64_RadioButton );
   SampleFormat_Control.SetSizer( SampleFormat_Sizer );

   SampleFormat_SectionBar.SetTitle( "Sample Format" );
   SampleFormat_SectionBar.SetSection( SampleFormat_Control );
   SampleFormat_SectionBar.OnToggleSection( (SectionBar::section_event_handler)&TiffOptionsDialog::e_ToggleSection, *this );

   //
   // Compression section. Combo item order MUST match TiffCompression.
   //
   CompressionMethod_Label.SetText( "Compression method:" );
   CompressionMethod_Label.SetFixedWidth( labelWidth1 );
   CompressionMethod_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   CompressionMethod_ComboBox.AddItem( "None" );
   CompressionMethod_ComboBox.AddItem( "LZW" );
   CompressionMethod_ComboBox.AddItem( "Deflate (zlib)" );
   CompressionMethod_ComboBox.AddItem( "PackBits (RLE)" );
   CompressionMethod_ComboBox.OnItemSelected( (ComboBox::item_event_handler)&TiffOptionsDialog::e_CompressionItemSelected, *this );
   CompressionMethod_ComboBox.SetToolTip( "<p>How each strip or tile is compressed.</p>"
      "<p><b>None</b> writes raw samples: the largest files, the fastest writes, and readable "
      "by everything.</p>"
      "<p><b>LZW</b> is lossless and universally supported, but gains little on noisy "
      "astronomical data.</p>"
      "<p><b>Deflate</b> is lossless and usually the best ratio here, at the cost of write "
      "time.</p>"
      "<p><b>PackBits</b> is a cheap run-length scheme that only helps on flat regions.</p>"
      "<p>Every method here is lossless; none of them alters a single sample value.</p>" );

   CompressionMethod_Sizer.SetSpacing( 4 );
   CompressionMethod_Sizer.Add( CompressionMethod_Label );
   CompressionMethod_Sizer.Add( CompressionMethod_ComboBox );
   CompressionMethod_Sizer.AddStretch();

   DeflateLevel_Label.SetText( "Deflate level:" );
   DeflateLevel_Label.SetFixedWidth( labelWidth1 );
   DeflateLevel_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   DeflateLevel_SpinBox.SetRange( 1, 9 );
   DeflateLevel_SpinBox.SetFixedWidth( editWidth1 );
   DeflateLevel_SpinBox.SetToolTip( "<p>Effort spent by the deflate encoder, from 1 (fastest) to 9 "
      "(smallest).</p>"
      "<p>The encoder groups these into three settings -- fast, balanced and best -- so "
      "neighbouring numbers often produce byte-identical files. Only used when the "
      "compression method is Deflate.</p>" );

   DeflateLevel_Sizer.SetSpacing( 4 );
   DeflateLevel_Sizer.Add( DeflateLevel_Label );
   DeflateLevel_Sizer.Add( DeflateLevel_SpinBox );
   DeflateLevel_Sizer.AddStretch();

   Predictor_CheckBox.SetText( "Horizontal differencing predictor" );
   Predictor_CheckBox.SetToolTip( "<p>Store the difference between horizontally adjacent samples "
      "(TIFF tag 317) before compressing. On smooth gradients this often shrinks an LZW or "
      "Deflate file noticeably; on noise it can make it slightly larger.</p>"
      "<p>TIFF defines the predictor for integer samples only, so it is unavailable for the "
      "floating-point sample formats.</p>" );

   Predictor_Sizer.SetSpacing( 4 );
   Predictor_Sizer.AddUnscaledSpacing( labelWidth1 + LogicalPixelsToPhysical( 4 ) );
   Predictor_Sizer.Add( Predictor_CheckBox );
   Predictor_Sizer.AddStretch();

   Compression_Sizer.SetSpacing( 4 );
   Compression_Sizer.Add( CompressionMethod_Sizer );
   Compression_Sizer.Add( DeflateLevel_Sizer );
   Compression_Sizer.Add( Predictor_Sizer );
   Compression_Control.SetSizer( Compression_Sizer );

   Compression_SectionBar.SetTitle( "Compression" );
   Compression_SectionBar.SetSection( Compression_Control );
   Compression_SectionBar.OnToggleSection( (SectionBar::section_event_handler)&TiffOptionsDialog::e_ToggleSection, *this );

   //
   // Layout section. Combo item order MUST match TiffContainer.
   //
   Container_Label.SetText( "Container:" );
   Container_Label.SetFixedWidth( labelWidth1 );
   Container_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Container_ComboBox.AddItem( "Automatic (classic TIFF when it fits)" );
   Container_ComboBox.AddItem( "Classic TIFF" );
   Container_ComboBox.AddItem( "BigTIFF" );
   Container_ComboBox.OnItemSelected( (ComboBox::item_event_handler)&TiffOptionsDialog::e_ContainerItemSelected, *this );
   Container_ComboBox.SetToolTip( "<p>Which TIFF container to write.</p>"
      "<p><b>Classic TIFF</b> uses 32-bit file offsets and therefore cannot exceed 4 GB -- but it "
      "is what every reader accepts.</p>"
      "<p><b>BigTIFF</b> uses 64-bit offsets and has no practical size limit, at the cost of "
      "readers that do not understand it.</p>"
      "<p><b>Automatic</b> writes classic TIFF while the file fits and switches to BigTIFF only "
      "when it does not, which is the safe default.</p>" );

   Container_Sizer.SetSpacing( 4 );
   Container_Sizer.Add( Container_Label );
   Container_Sizer.Add( Container_ComboBox );
   Container_Sizer.AddStretch();

   BlockLayout_Label.SetText( "Block layout:" );
   BlockLayout_Label.SetFixedWidth( labelWidth1 );
   BlockLayout_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Strips_RadioButton.SetText( "Strips" );
   Strips_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_LayoutClick, *this );
   Strips_RadioButton.SetToolTip( "<p>Store the image as horizontal strips of rows. This is what the "
      "great majority of software reads, and the right choice unless you know otherwise.</p>" );

   Tiles_RadioButton.SetText( "Tiles" );
   Tiles_RadioButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_LayoutClick, *this );
   Tiles_RadioButton.SetToolTip( "<p>Store the image as square tiles. Intended for large-image viewers "
      "that read sub-regions, which can then fetch a small window without decoding whole rows.</p>"
      "<p>Striped TIFFs are read far more widely, so choose tiles only for a reader you know "
      "wants them.</p>" );

   BlockLayout_Sizer.SetSpacing( 8 );
   BlockLayout_Sizer.Add( BlockLayout_Label );
   BlockLayout_Sizer.Add( Strips_RadioButton );
   BlockLayout_Sizer.Add( Tiles_RadioButton );
   BlockLayout_Sizer.AddStretch();

   TileSize_Label.SetText( "Tile size:" );
   TileSize_Label.SetFixedWidth( labelWidth1 );
   TileSize_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   for ( size_t i = 0; i < ItemsInArray( s_tileSizes ); ++i )
      TileSize_ComboBox.AddItem( String().Format( "%u x %u", s_tileSizes[i], s_tileSizes[i] ) );
   TileSize_ComboBox.SetToolTip( "<p>Edge length of each tile, in pixels. TIFF requires a multiple of "
      "16; these are the powers of two from 128 to 1024 that the engine accepts.</p>"
      "<p>Smaller tiles let a viewer fetch a tighter region; larger tiles compress a little "
      "better and cost fewer offsets. Only used with the Tiles layout.</p>" );

   TileSize_Sizer.SetSpacing( 4 );
   TileSize_Sizer.Add( TileSize_Label );
   TileSize_Sizer.Add( TileSize_ComboBox );
   TileSize_Sizer.AddStretch();

   Layout_Sizer.SetSpacing( 4 );
   Layout_Sizer.Add( Container_Sizer );
   Layout_Sizer.Add( BlockLayout_Sizer );
   Layout_Sizer.Add( TileSize_Sizer );
   Layout_Control.SetSizer( Layout_Sizer );

   Layout_SectionBar.SetTitle( "Layout" );
   Layout_SectionBar.SetSection( Layout_Control );
   Layout_SectionBar.OnToggleSection( (SectionBar::section_event_handler)&TiffOptionsDialog::e_ToggleSection, *this );

   //
   // Embedded Data section. Combo item order MUST match TiffAlpha.
   //
   EmbedICC_CheckBox.SetText( "Embed ICC profile" );
   EmbedICC_CheckBox.SetToolTip( "<p>Write the image's ICC colour profile into the file (TIFF tag "
      "34675), so a colour-managed application reproduces the same colours.</p>"
      "<p>Nothing is embedded when the image has no profile.</p>" );

   EmbedICC_Sizer.SetSpacing( 4 );
   EmbedICC_Sizer.AddUnscaledSpacing( labelWidth1 + LogicalPixelsToPhysical( 4 ) );
   EmbedICC_Sizer.Add( EmbedICC_CheckBox );
   EmbedICC_Sizer.AddStretch();

   Alpha_Label.SetText( "Alpha channel:" );
   Alpha_Label.SetFixedWidth( labelWidth1 );
   Alpha_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Alpha_ComboBox.AddItem( "Not written" );
   Alpha_ComboBox.AddItem( "Unassociated (straight)" );
   Alpha_ComboBox.AddItem( "Associated (premultiplied)" );
   Alpha_ComboBox.SetToolTip( "<p>How an alpha channel is declared in the file (TIFF tag 338, "
      "ExtraSamples).</p>"
      "<p><b>Unassociated</b> means the colour samples are independent of alpha -- the usual "
      "choice, and what most editors expect.</p>"
      "<p><b>Associated</b> means the colour samples are already multiplied by alpha.</p>"
      "<p><b>Not written</b> discards the alpha channel. This control is disabled when the "
      "image being saved has no alpha channel.</p>" );

   Alpha_Sizer.SetSpacing( 4 );
   Alpha_Sizer.Add( Alpha_Label );
   Alpha_Sizer.Add( Alpha_ComboBox );
   Alpha_Sizer.AddStretch();

   EmbeddedData_Sizer.SetSpacing( 4 );
   EmbeddedData_Sizer.Add( EmbedICC_Sizer );
   EmbeddedData_Sizer.Add( Alpha_Sizer );
   EmbeddedData_Control.SetSizer( EmbeddedData_Sizer );

   EmbeddedData_SectionBar.SetTitle( "Embedded Data" );
   EmbeddedData_SectionBar.SetSection( EmbeddedData_Control );
   EmbeddedData_SectionBar.OnToggleSection( (SectionBar::section_event_handler)&TiffOptionsDialog::e_ToggleSection, *this );

   //
   // Metadata section.
   //
   const String asciiNote = "<p>Written as an ASCII TIFF tag: characters outside printable 7-bit "
      "ASCII are removed and the text is truncated to 255 characters when you accept the "
      "dialog.</p>";

   Description_Label.SetText( "Description:" );
   Description_Label.SetFixedWidth( labelWidth1 );
   Description_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Description_Edit.SetMaxLength( int( TiffTextTagCapacity ) - 1 );
   Description_Edit.SetToolTip( String( "<p>Free-text description of the image (TIFF tag 270, "
      "ImageDescription).</p>" ) + asciiNote );

   Description_Sizer.SetSpacing( 4 );
   Description_Sizer.Add( Description_Label );
   Description_Sizer.Add( Description_Edit, 100 );

   CopyrightTag_Label.SetText( "Copyright:" );
   CopyrightTag_Label.SetFixedWidth( labelWidth1 );
   CopyrightTag_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Copyright_Edit.SetMaxLength( int( TiffTextTagCapacity ) - 1 );
   Copyright_Edit.SetToolTip( String( "<p>Copyright notice for the image (TIFF tag 33432).</p>" ) + asciiNote );

   Copyright_Sizer.SetSpacing( 4 );
   Copyright_Sizer.Add( CopyrightTag_Label );
   Copyright_Sizer.Add( Copyright_Edit, 100 );

   Software_Label.SetText( "Software:" );
   Software_Label.SetFixedWidth( labelWidth1 );
   Software_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   Software_Edit.SetMaxLength( int( TiffTextTagCapacity ) - 1 );
   Software_Edit.SetToolTip( String( "<p>Name and version of the software that produced the image (TIFF tag "
      "305). Leave empty to let the module identify itself.</p>" ) + asciiNote );

   Software_Sizer.SetSpacing( 4 );
   Software_Sizer.Add( Software_Label );
   Software_Sizer.Add( Software_Edit, 100 );

   Resolution_Label.SetText( "Resolution:" );
   Resolution_Label.SetFixedWidth( labelWidth1 );
   Resolution_Label.SetTextAlignment( TextAlign::Right | TextAlign::VertCenter );

   XResolution_NumericEdit.label.SetText( "Horizontal:" );
   XResolution_NumericEdit.SetReal();
   XResolution_NumericEdit.SetRange( 0, 10000 );
   XResolution_NumericEdit.SetPrecision( 3 );
   XResolution_NumericEdit.edit.SetFixedWidth( editWidth1 );
   XResolution_NumericEdit.SetToolTip( "<p>Horizontal print resolution (TIFF tag 282), in pixels per "
      "resolution unit. It has no effect on the pixel data; it only tells a printing or layout "
      "application how large to render the image.</p>" );

   YResolution_NumericEdit.label.SetText( "Vertical:" );
   YResolution_NumericEdit.SetReal();
   YResolution_NumericEdit.SetRange( 0, 10000 );
   YResolution_NumericEdit.SetPrecision( 3 );
   YResolution_NumericEdit.edit.SetFixedWidth( editWidth1 );
   YResolution_NumericEdit.SetToolTip( "<p>Vertical print resolution (TIFF tag 283), in pixels per "
      "resolution unit. It has no effect on the pixel data.</p>" );

   // Item order MUST match ImageOptions::metricResolution (false, then true).
   ResolutionUnit_ComboBox.AddItem( "Pixels per inch" );
   ResolutionUnit_ComboBox.AddItem( "Pixels per centimeter" );
   ResolutionUnit_ComboBox.SetToolTip( "<p>Unit the two resolution values are expressed in (TIFF tag "
      "296, ResolutionUnit).</p>" );

   Resolution_Sizer.SetSpacing( 4 );
   Resolution_Sizer.Add( Resolution_Label );
   Resolution_Sizer.Add( XResolution_NumericEdit );
   Resolution_Sizer.Add( YResolution_NumericEdit );
   Resolution_Sizer.Add( ResolutionUnit_ComboBox );
   Resolution_Sizer.AddStretch();

   Metadata_Sizer.SetSpacing( 4 );
   Metadata_Sizer.Add( Description_Sizer );
   Metadata_Sizer.Add( Copyright_Sizer );
   Metadata_Sizer.Add( Software_Sizer );
   Metadata_Sizer.Add( Resolution_Sizer );
   Metadata_Control.SetSizer( Metadata_Sizer );

   Metadata_SectionBar.SetTitle( "Metadata" );
   Metadata_SectionBar.SetSection( Metadata_Control );
   Metadata_SectionBar.OnToggleSection( (SectionBar::section_event_handler)&TiffOptionsDialog::e_ToggleSection, *this );

   //
   // Buttons.
   //
   OK_PushButton.SetText( "OK" );
   OK_PushButton.SetDefault();
   OK_PushButton.SetCursor( StdCursor::Checkmark );
   OK_PushButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_ButtonClick, *this );
   OK_PushButton.SetToolTip( "<p>Accept these options and continue writing the file.</p>" );

   Cancel_PushButton.SetText( "Cancel" );
   Cancel_PushButton.SetCursor( StdCursor::Crossmark );
   Cancel_PushButton.OnClick( (Button::click_event_handler)&TiffOptionsDialog::e_ButtonClick, *this );
   Cancel_PushButton.SetToolTip( "<p>Abort the save operation.</p>" );

   Buttons_Sizer.SetSpacing( 8 );
   Buttons_Sizer.AddStretch();
   Buttons_Sizer.Add( OK_PushButton );
   Buttons_Sizer.Add( Cancel_PushButton );

   //
   // Top-level layout.
   //
   Global_Sizer.SetMargin( 8 );
   Global_Sizer.SetSpacing( 6 );
   Global_Sizer.Add( Notice_Control );
   Global_Sizer.Add( SampleFormat_SectionBar );
   Global_Sizer.Add( SampleFormat_Control );
   Global_Sizer.Add( Compression_SectionBar );
   Global_Sizer.Add( Compression_Control );
   Global_Sizer.Add( Layout_SectionBar );
   Global_Sizer.Add( Layout_Control );
   Global_Sizer.Add( EmbeddedData_SectionBar );
   Global_Sizer.Add( EmbeddedData_Control );
   Global_Sizer.Add( Metadata_SectionBar );
   Global_Sizer.Add( Metadata_Control );
   Global_Sizer.Add( Buttons_Sizer );

   SetSizer( Global_Sizer );

   UpdateControls();

   EnsureLayoutUpdated();
   AdjustToContents();
   SetFixedHeight();

   SetWindowTitle( String( u"TIFF Options \x2014 Mega Pixel Processor" ) );
}

// ----------------------------------------------------------------------------

void TiffOptionsDialog::UpdateControls()
{
   // --- Sample format. Anything the module cannot express falls back to the
   // 16-bit integer default rather than leaving every button unchecked.
   const bool isFloat = m_imageOptions.ieeefpSampleFormat;
   UInt8_RadioButton.SetChecked( !isFloat && m_imageOptions.bitsPerSample == 8 );
   UInt16_RadioButton.SetChecked( !isFloat && m_imageOptions.bitsPerSample != 8
                                           && m_imageOptions.bitsPerSample != 32 );
   UInt32_RadioButton.SetChecked( !isFloat && m_imageOptions.bitsPerSample == 32 );
   Float32_RadioButton.SetChecked( isFloat && m_imageOptions.bitsPerSample != 64 );
   Float64_RadioButton.SetChecked( isFloat && m_imageOptions.bitsPerSample == 64 );

   // --- Compression.
   CompressionMethod_ComboBox.SetCurrentItem( int( m_formatOptions.compression ) );
   DeflateLevel_SpinBox.SetValue( int( m_formatOptions.deflateLevel ) );

   // TIFF tag 317 is defined for integer samples only, and the engine's
   // TiffWriteOptions::validate rejects predictor + a float sample format, so
   // the check box goes away rather than producing an error at save time.
   Predictor_CheckBox.SetChecked( m_formatOptions.predictor && !isFloat );
   Predictor_CheckBox.Enable( !isFloat );

   // Only Deflate consults a level.
   DeflateLevel_SpinBox.Enable( m_formatOptions.compression == TiffCompression::Deflate );

   // --- Layout.
   Container_ComboBox.SetCurrentItem( int( m_formatOptions.container ) );

   const bool tiled = m_formatOptions.layout == TiffLayout::Tiles;
   Strips_RadioButton.SetChecked( !tiled );
   Tiles_RadioButton.SetChecked( tiled );
   TileSize_ComboBox.SetCurrentItem( TileSizeItemIndex( m_formatOptions.tileSize ) );
   TileSize_ComboBox.Enable( tiled );

   // --- Embedded data.
   EmbedICC_CheckBox.SetChecked( m_imageOptions.embedICCProfile );
   Alpha_ComboBox.SetCurrentItem( m_imageHasAlpha ? int( m_formatOptions.alpha ) : 0 );
   Alpha_ComboBox.Enable( m_imageHasAlpha );

   // --- Metadata.
   Description_Edit.SetText( String( m_formatOptions.description ) );
   Copyright_Edit.SetText( String( m_formatOptions.copyright ) );
   Software_Edit.SetText( String( m_formatOptions.software ) );

   XResolution_NumericEdit.SetValue( m_imageOptions.xResolution );
   YResolution_NumericEdit.SetValue( m_imageOptions.yResolution );
   ResolutionUnit_ComboBox.SetCurrentItem( m_imageOptions.metricResolution ? 1 : 0 );
}

// ----------------------------------------------------------------------------

void TiffOptionsDialog::HarvestControls()
{
   // --- Sample format.
   if ( UInt8_RadioButton.IsChecked() )
   {
      m_imageOptions.bitsPerSample = 8;
      m_imageOptions.ieeefpSampleFormat = false;
   }
   else if ( UInt32_RadioButton.IsChecked() )
   {
      m_imageOptions.bitsPerSample = 32;
      m_imageOptions.ieeefpSampleFormat = false;
   }
   else if ( Float32_RadioButton.IsChecked() )
   {
      m_imageOptions.bitsPerSample = 32;
      m_imageOptions.ieeefpSampleFormat = true;
   }
   else if ( Float64_RadioButton.IsChecked() )
   {
      m_imageOptions.bitsPerSample = 64;
      m_imageOptions.ieeefpSampleFormat = true;
   }
   else
   {
      m_imageOptions.bitsPerSample = 16;
      m_imageOptions.ieeefpSampleFormat = false;
   }
   m_imageOptions.complexSample = false;
   m_imageOptions.signedIntegers = false;

   // --- Compression.
   m_formatOptions.compression = TiffCompression( uint8_t( Max( 0, CompressionMethod_ComboBox.CurrentItem() ) ) );
   m_formatOptions.deflateLevel = uint8_t( DeflateLevel_SpinBox.Value() );
   // Never harvest a predictor the engine would reject.
   m_formatOptions.predictor = Predictor_CheckBox.IsChecked() && !m_imageOptions.ieeefpSampleFormat;

   // --- Layout.
   m_formatOptions.container = TiffContainer( uint8_t( Max( 0, Container_ComboBox.CurrentItem() ) ) );
   m_formatOptions.layout = Tiles_RadioButton.IsChecked() ? TiffLayout::Tiles : TiffLayout::Strips;
   // CurrentItem() returns -1 for an empty selection, which cannot happen here
   // (UpdateControls always selects an item) but must not index the array.
   const int tileItem = TileSize_ComboBox.CurrentItem();
   if ( tileItem >= 0 && size_t( tileItem ) < ItemsInArray( s_tileSizes ) )
      m_formatOptions.tileSize = s_tileSizes[tileItem];

   // --- Embedded data.
   m_imageOptions.embedICCProfile = EmbedICC_CheckBox.IsChecked();
   m_formatOptions.alpha = m_imageHasAlpha ? TiffAlpha( uint8_t( Max( 0, Alpha_ComboBox.CurrentItem() ) ) )
                                           : TiffAlpha::None;

   // --- Metadata. Truncated and reduced to printable 7-bit ASCII here, where
   // reopening the dialog shows the user exactly what will be written.
   CopyAsciiTag( m_formatOptions.description, TiffTextTagCapacity, Description_Edit.Text().Trimmed() );
   CopyAsciiTag( m_formatOptions.copyright, TiffTextTagCapacity, Copyright_Edit.Text().Trimmed() );
   CopyAsciiTag( m_formatOptions.software, TiffTextTagCapacity, Software_Edit.Text().Trimmed() );

   m_imageOptions.xResolution = XResolution_NumericEdit.Value();
   m_imageOptions.yResolution = YResolution_NumericEdit.Value();
   m_imageOptions.metricResolution = ResolutionUnit_ComboBox.CurrentItem() == 1;
}

// ----------------------------------------------------------------------------
// Event handlers.
// ----------------------------------------------------------------------------

void TiffOptionsDialog::e_SampleFormatClick( Button&, bool )
{
   // The predictor's availability depends on the sample format, so harvest
   // the current state and re-apply every enable rule from one place.
   HarvestControls();
   UpdateControls();
}

void TiffOptionsDialog::e_CompressionItemSelected( ComboBox&, int )
{
   HarvestControls();
   UpdateControls();
}

void TiffOptionsDialog::e_LayoutClick( Button&, bool )
{
   HarvestControls();
   UpdateControls();
}

void TiffOptionsDialog::e_ContainerItemSelected( ComboBox&, int )
{
   HarvestControls();
   UpdateControls();
}

void TiffOptionsDialog::e_ButtonClick( Button& sender, bool )
{
   if ( &sender == &OK_PushButton )
   {
      HarvestControls();
      Ok();
   }
   else if ( &sender == &Cancel_PushButton )
      Cancel();
}

// ----------------------------------------------------------------------------
// SectionBar toggle: let the dialog grow and shrink with its sections. The
// height is pinned again once the animation is over, so the window never
// ends up taller than its contents.
// ----------------------------------------------------------------------------

void TiffOptionsDialog::e_ToggleSection( SectionBar&, Control&, bool start )
{
   if ( start )
      SetVariableHeight();
   else
   {
      EnsureLayoutUpdated();
      AdjustToContents();
      SetFixedHeight();
   }
}

// ----------------------------------------------------------------------------
// Header notice: logo paint + hyperlink labels. Reused from MmmInterface.cpp.
// ----------------------------------------------------------------------------

void TiffOptionsDialog::e_LogoPaint( Control& sender, const pcl::Rect& )
{
   Graphics g( sender );
   if ( !Logo_Bitmap.IsNull() )
      g.DrawScaledBitmap( sender.BoundsRect(), Logo_Bitmap );
}

void TiffOptionsDialog::e_LinkMouseRelease( Control& sender, const pcl::Point&, int button, unsigned, unsigned )
{
   if ( button != MouseButton::Left )
      return;

   String url;
   if ( &sender == &ToolsLink_Label )
      url = "https://tools.astrometrical.com/";
   else if ( &sender == &KofiLink_Label )
      url = "https://ko-fi.com/astrometrical";
   else
      return;

   try
   {
#ifdef _WIN32
      ExternalProcess::StartProgram( "cmd.exe", StringList() << "/c" << "start" << "" << url );
#elif defined( __APPLE__ )
      ExternalProcess::StartProgram( "open", StringList() << url );
#else
      ExternalProcess::StartProgram( "xdg-open", StringList() << url );
#endif
   }
   catch ( ... )
   {
      // Non-fatal: each link's tooltip shows the full URL anyway.
   }
}

// ----------------------------------------------------------------------------

} // namespace pcl
