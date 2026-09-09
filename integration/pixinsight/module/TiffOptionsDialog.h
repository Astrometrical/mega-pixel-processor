// TiffOptionsDialog.h -- the Astrometrical-branded TIFF options dialog the
// module presents from BigTiffInstance::QueryOptions().
//
// Deliberately styled unlike PixInsight's stock TIFF options dialog: this
// module REPLACES the stock TIFF module, and a user needs to be able to see
// at a glance which one is loaded. The header block (chevron mark, product
// title, tagline, links) is the same one Mega Merge Mosaic's process
// interface uses -- our own code, reused for family consistency.
//
// Clean-room note: written from the public PCL headers, the project's
// PCL_FILEFORMAT_API.md, and MergeMosaic's own module code. No PCL module
// implementation source was consulted; the stock dialog is referenced only
// as a list of the options it offers a user on screen.
//
// Unlike MmmInterface, whose control tree lives in a heap-allocated GUIData
// block built lazily on first Launch(), a modal dialog is constructed only
// when it is about to be shown, so the controls are direct members here.
// The rest of the idiom is MmmInterface's: one control block in declaration
// order, e_-prefixed handlers, UpdateControls()/HarvestControls() as the
// only bridge between the controls and the two option structs.

#ifndef __TiffOptionsDialog_h
#define __TiffOptionsDialog_h

#include <pcl/Bitmap.h>
#include <pcl/CheckBox.h>
#include <pcl/ComboBox.h>
#include <pcl/Dialog.h>
#include <pcl/Edit.h>
#include <pcl/ImageOptions.h>
#include <pcl/Label.h>
#include <pcl/NumericControl.h>
#include <pcl/PushButton.h>
#include <pcl/RadioButton.h>
#include <pcl/SectionBar.h>
#include <pcl/Sizer.h>
#include <pcl/SpinBox.h>

#include "TiffFormatOptions.h"

namespace pcl
{

// ----------------------------------------------------------------------------

/*!
 * \class TiffOptionsDialog
 * \brief Modal per-save options dialog for the BigTIFF/TIFF format.
 *
 * Constructed with the options the caller wants shown as the initial state,
 * executed with Dialog::Execute(); on StdDialogCode::Ok the two getters
 * return the user's choices. On cancel they return the values passed in,
 * and the caller aborts the save.
 */
class TiffOptionsDialog : public Dialog
{
public:

   /*!
    * \brief Constructs the dialog with an initial state.
    *
    * \param imageOptions   Initial sample format and resolution.
    * \param formatOptions  Initial TIFF-specific options.
    * \param imageHasAlpha  Whether the image being saved carries an alpha
    *                       channel; the alpha-association control is
    *                       disabled when it does not.
    */
   TiffOptionsDialog( const ImageOptions& imageOptions,
                      const TiffFormatOptions& formatOptions,
                      bool imageHasAlpha );

   /*!
    * \brief The sample format and resolution the user chose.
    */
   const ImageOptions& GetImageOptions() const
   {
      return m_imageOptions;
   }

   /*!
    * \brief The TIFF-specific options the user chose.
    */
   const TiffFormatOptions& GetFormatOptions() const
   {
      return m_formatOptions;
   }

private:

   ImageOptions      m_imageOptions;
   TiffFormatOptions m_formatOptions;
   bool              m_imageHasAlpha = false;

   VerticalSizer   Global_Sizer;

   // --- Header notice: chevron + title/tagline/copyright/links ------------
   Control         Notice_Control;
   HorizontalSizer Notice_Sizer;
   Control         Logo_Control;          // paints the chevron bitmap
   VerticalSizer   NoticeText_Sizer;
   Label           Title_Label;
   Label           Tagline_Label;
   Label           Copyright_Label;
   HorizontalSizer Links_Sizer;
   Label           ToolsLink_Label;       // -> https://tools.astrometrical.com/
   Label           LinkSep_Label;
   Label           KofiLink_Label;        // -> https://ko-fi.com/astrometrical
   Bitmap          Logo_Bitmap;           // rendered from MPP_CHEVRON_SVG

   // --- Sample Format -----------------------------------------------------
   SectionBar      SampleFormat_SectionBar;
   Control         SampleFormat_Control;
   VerticalSizer   SampleFormat_Sizer;
   RadioButton     UInt8_RadioButton;
   RadioButton     UInt16_RadioButton;
   RadioButton     UInt32_RadioButton;
   RadioButton     Float32_RadioButton;
   RadioButton     Float64_RadioButton;

   // --- Compression -------------------------------------------------------
   SectionBar      Compression_SectionBar;
   Control         Compression_Control;
   VerticalSizer   Compression_Sizer;
   HorizontalSizer CompressionMethod_Sizer;
   Label           CompressionMethod_Label;
   ComboBox        CompressionMethod_ComboBox;
   HorizontalSizer DeflateLevel_Sizer;
   Label           DeflateLevel_Label;
   SpinBox         DeflateLevel_SpinBox;
   HorizontalSizer Predictor_Sizer;
   CheckBox        Predictor_CheckBox;

   // --- Layout (our addition; not in the stock dialog) ---------------------
   SectionBar      Layout_SectionBar;
   Control         Layout_Control;
   VerticalSizer   Layout_Sizer;
   HorizontalSizer Container_Sizer;
   Label           Container_Label;
   ComboBox        Container_ComboBox;
   HorizontalSizer BlockLayout_Sizer;
   Label           BlockLayout_Label;
   RadioButton     Strips_RadioButton;
   RadioButton     Tiles_RadioButton;
   HorizontalSizer TileSize_Sizer;
   Label           TileSize_Label;
   ComboBox        TileSize_ComboBox;

   // --- Embedded Data -----------------------------------------------------
   SectionBar      EmbeddedData_SectionBar;
   Control         EmbeddedData_Control;
   VerticalSizer   EmbeddedData_Sizer;
   HorizontalSizer EmbedICC_Sizer;
   CheckBox        EmbedICC_CheckBox;
   HorizontalSizer Alpha_Sizer;
   Label           Alpha_Label;
   ComboBox        Alpha_ComboBox;

   // --- Metadata ----------------------------------------------------------
   SectionBar      Metadata_SectionBar;
   Control         Metadata_Control;
   VerticalSizer   Metadata_Sizer;
   HorizontalSizer Description_Sizer;
   Label           Description_Label;
   Edit            Description_Edit;
   HorizontalSizer Copyright_Sizer;
   Label           CopyrightTag_Label;
   Edit            Copyright_Edit;
   HorizontalSizer Software_Sizer;
   Label           Software_Label;
   Edit            Software_Edit;
   HorizontalSizer Resolution_Sizer;
   Label           Resolution_Label;
   NumericEdit     XResolution_NumericEdit;
   NumericEdit     YResolution_NumericEdit;
   ComboBox        ResolutionUnit_ComboBox;

   // --- Buttons -----------------------------------------------------------
   HorizontalSizer Buttons_Sizer;
   PushButton      OK_PushButton;
   PushButton      Cancel_PushButton;

   /*!
    * \brief Loads every control from m_imageOptions/m_formatOptions and
    * applies the enable rules.
    */
   void UpdateControls();

   /*!
    * \brief Re-applies the enable rules from the controls' own state.
    *
    * Called by UpdateControls() and by every handler whose control governs
    * another control's availability. It does not harvest, so the text edits
    * are never rewritten while the user is still typing into them.
    */
   void UpdateEnableStates();

   /*!
    * \brief Reads every control back into m_imageOptions/m_formatOptions.
    *
    * Called only when the user accepts the dialog. The three TIFF text tags
    * are truncated and reduced to printable 7-bit ASCII on the way in.
    */
   void HarvestControls();

   void e_SampleFormatClick( Button& sender, bool checked );
   void e_CompressionItemSelected( ComboBox& sender, int itemIndex );
   void e_LayoutClick( Button& sender, bool checked );
   void e_ContainerItemSelected( ComboBox& sender, int itemIndex );
   void e_ButtonClick( Button& sender, bool checked );
   void e_ToggleSection( SectionBar& sender, Control& section, bool start );
   void e_LinkMouseRelease( Control& sender, const pcl::Point& pos,
                            int button, unsigned buttons, unsigned modifiers );
   void e_LogoPaint( Control& sender, const pcl::Rect& updateRect );
};

// ----------------------------------------------------------------------------

} // namespace pcl

#endif   // __TiffOptionsDialog_h
