// Mpp.cpp -- Mega Pixel Processor PixInsight module (MppModule) + installation
// entry point.
//
// Per integration/pixinsight/PCL_FILEFORMAT_API.md §1: the three module entry
// points have C linkage and are free functions, not members of MetaModule.
// PCL's static library (libPCL-pxi.a) supplies IdentifyPixInsightModule
// (PMIDN) and InitializePixInsightModule (PMINI); a hand-written module only
// defines InstallPixInsightModule (PMINS).
//
// Constructing MppModule sets the pcl::Module global; constructing the
// BigTiffFormat singleton afterwards self-registers it under that module
// (MetaObject tree -- see PCL_FILEFORMAT_API.md §1.3 and Unknowns §7.1: the
// MetaModule is created strictly before the MetaFileFormat).
//
// Naming convention: module-level identity is Mpp* (this file, MppModule,
// MppVersion.h); each *file format* the module provides is its own
// MetaFileFormat/FileFormatImplementation pair named for that format
// (BigTiffFormat/BigTiffInstance today). Additional formats are added as
// sibling pairs, each constructed below alongside BigTiffFormat.
//
// Clean-room note: written from the public PCL headers, the project's
// PCL_FILEFORMAT_API.md reference, and MergeMosaic's own mmm.cpp (our code).
// No PCL module implementation source was consulted.

#include <pcl/MetaModule.h>

#include "BigTiffFormat.h"
#include "MppVersion.h"

#define MPP_MODULE_VERSION_LANGUAGE  eng

namespace pcl
{

// ----------------------------------------------------------------------------

/*!
 * \class MppModule
 * \brief The Mega Pixel Processor module meta-object (tree root).
 */
class MppModule : public MetaModule
{
public:

   MppModule() = default;

   const char* Version() const override
   {
      return PCL_MODULE_VERSION( MPP_VERSION_MAJOR,
                                 MPP_VERSION_MINOR,
                                 MPP_VERSION_REVISION,
                                 MPP_VERSION_BUILD,
                                 MPP_MODULE_VERSION_LANGUAGE );
   }

   IsoString Name() const override
   {
      return "MegaPixelProcessor";
   }

   String Description() const override
   {
      // Shown in the Install Modules dialog when the user selects this
      // module -- PCL's own doc says Description() exists "to provide
      // meaningful information to the users during module installation
      // procedures" (MetaModule.h). That is BEFORE installation, so it is
      // the one place a prerequisite can be stated before it bites. Most
      // users install without clicking through to it; the console message
      // below covers them at the moment of collision.
      return "Mega Pixel Processor: file format support for astronomically large "
             "images. Provides the TIFF format (read+write, classic TIFF and "
             "BigTIFF). REPLACES PixInsight's stock TIFF module, which must be "
             "uninstalled first (Process > Modules > Manage Modules) -- otherwise "
             "this module's installation fails with \"Duplicate file format "
             "identifier\". Further formats register under this module as they "
             "are added.";
   }

   String Company() const override
   {
      return "Astrometrical";
   }

   String Author() const override
   {
      return "Daniel Paull";
   }

   String Copyright() const override
   {
      return "Copyright (c) 2026 Astrometrical";
   }
};

// ----------------------------------------------------------------------------

} // namespace pcl

// ----------------------------------------------------------------------------

/*
 * Module installation routine (PMINS). Called by the PixInsight core to
 * create and register the module's meta-objects. Returns 0 on success
 * (MetaModule.h:1014-1015). The MetaModule singleton is created in every
 * install mode; the full object tree only for FullInstall
 * (PCL_FILEFORMAT_API.md §1.1).
 */
PCL_MODULE_EXPORT pcl::int32 InstallPixInsightModule( pcl::int32 mode )
{
   new pcl::MppModule;   // sets pcl::Module

   if ( mode == pcl::InstallMode::FullInstall )
      new pcl::BigTiffFormat;   // self-registers under pcl::Module
                                // (further formats: one `new` each, here)
      //
      // If registration is refused (the stock TIFF module is still installed
      // and owns the "TIFF" identifier), the exception escapes to the core,
      // which reports it and uninstalls this module. Nothing can be done
      // about that from here, and it has been tried twice against a real
      // core: querying the format registry first segfaults, and writing the
      // fix to the Console from inside this function segfaults too. See the
      // README, "Replacing the stock TIFF module", before trying a third.

   return 0;
}
