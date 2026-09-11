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
// Registration yields to a format that got there first. The core allows one
// format per file extension, and this module claims .tif/.tiff -- the stock
// TIFF module's extensions. Before phase 4's guard, colliding with it made
// the core refuse OUR registration and UNINSTALL this module, so the user had
// to remove the stock module and then manually reinstall this one. Now
// InstallPixInsightModule asks the core (FileFormat::IsSupportedFileFormatBySuffix,
// intermodule communication -- live by PMINS, which runs after PMINI's API
// handshake) whether any claimed extension is already taken; if so it skips
// constructing BigTiffFormat, the module installs with no formats, and
// MppModule::OnLoad reports what is in the way, in red, with the fix. On the
// next start after the stock module is gone, the format registers on its own.
// If the query itself throws, the guard falls through to registering as
// before -- the fail-safe is today's behaviour, not something new.
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

#include <pcl/Console.h>
#include <pcl/FileFormat.h>
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

   /*!
    * Records that InstallPixInsightModule() found \a extension already
    * claimed by the installed format \a name (\a description), and so did
    * not register this module's TIFF format. OnLoad() reports it.
    */
   void SetTiffConflict( const String& extension, const IsoString& name, const String& description )
   {
      m_conflictExtension = extension;
      m_conflictName = name;
      m_conflictDescription = description;
   }

   /*!
    * Invoked just after this module has been installed (MetaModule.h:481).
    * The only thing it does is tell the user, in red, when the TIFF format
    * was withheld -- and why, and what to do about it. Console::CriticalLn
    * is the same red-escape the core uses for its own errors, which is what
    * makes the Process Console surface it rather than scroll it past.
    */
   void OnLoad() override
   {
      if ( m_conflictName.IsEmpty() )
         return;

      Console console;
      console.CriticalLn( "*** Mega Pixel Processor: TIFF format NOT registered." );
      console.CriticalLn( "An installed file format already handles " + m_conflictExtension
                          + ": \"" + String( m_conflictName ) + "\""
                          + ( m_conflictDescription.IsEmpty() ? String() : " -- " + m_conflictDescription )
                          + "." );
      console.CriticalLn( "PixInsight allows one format per file extension. To use Mega Pixel "
                          "Processor's TIFF support: Process > Modules > Manage Modules, "
                          "uninstall that TIFF module, and restart PixInsight. This module "
                          "stays installed and its TIFF format registers on the next load." );
   }

private:

   String    m_conflictExtension;    // the first claimed extension found taken, e.g. ".tif"
   IsoString m_conflictName;         // Name() of the format that has it; empty = no conflict
   String    m_conflictDescription;  // its Description(), for the message

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
   pcl::MppModule* module = new pcl::MppModule;   // sets pcl::Module

   if ( mode == pcl::InstallMode::FullInstall )
   {
      // Yield to a format that already owns one of our extensions rather
      // than collide with it (see the file header). Ask before constructing:
      // MetaFileFormat's constructor is the registration.
      bool taken = false;
      try
      {
         for ( const pcl::String& ext : pcl::BigTiffFormat::ClaimedExtensions() )
            if ( pcl::FileFormat::IsSupportedFileFormatBySuffix( "x" + ext ) )
            {
               pcl::FileFormat owner( ext );
               module->SetTiffConflict( ext, owner.Name(), owner.Description() );
               taken = true;
               break;
            }
      }
      catch ( ... )
      {
         // The query is intermodule communication, expected to be live by
         // PMINS but not verified in every core. If it fails, register as we
         // always did and let the core arbitrate -- never worse than before.
         taken = false;
      }

      if ( !taken )
         new pcl::BigTiffFormat;   // self-registers under pcl::Module
                                   // (further formats: one `new` each, here)
   }

   return 0;
}
