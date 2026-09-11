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
      return "Mega Pixel Processor: file format support for astronomically large "
             "images. Provides the BigTIFF/TIFF format (read+write); further "
             "formats register under this module as they are added.";
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

   return 0;
}
