# Windows-only CMake build for the mpp PixInsight module (mpp-pxm.dll).
#
# The Linux/macOS module is built by `makefile-x64` (a .so/.dylib whose
# undefined PixInsight-core symbols are resolved by the core at load time).
# Windows requires every symbol resolved at link time, so this CMake mirrors
# exactly how the real PCL modules are built on Windows/vc17 at the pinned
# PCL SHA (adapted from MergeMosaic's module/CMakeLists.txt -- our code,
# whose windows leg proves this recipe against the same 2.8.3 pin):
#
#   * ConfigurationType DynamicLibrary  -> mpp-pxm.dll
#   * link PCL-pxi.lib + the six vendored 3rdparty -pxi.lib + Win32 system libs
#   * NO PixInsight-core import library exists or is needed: the module exports
#     its entry points (via PCL_MODULE_EXPORT/__declspec(dllexport)) and
#     reaches the core only through PCL's runtime API function-pointer table,
#     which PCL-pxi.lib (static) provides. Hence a fully-linked DLL is produced
#     on a runner with no PixInsight installed, exactly like upstream modules.
#   * ABI flags match the pinned PCL.vcxproj / 3rdparty vcxproj so there is no
#     CRT (/MD) or exception-model (/EHa) mismatch: LanguageStandard stdcpp20,
#     /Zc:__cplusplus /permissive-, ExceptionHandling Async (/EHa),
#     RuntimeLibrary MultiThreadedDLL (/MD), AVX2, PlatformToolset v143.
#
# Invoke with the include/lib dirs of a prefix produced by
# build-pcl-windows.ps1 (same PCLINCDIR/PCLLIBDIR variable names as
# makefile-x64, matching this repo's build-module-* convention):
#   cmake -S <thisdir> -B <build> -A x64 -T v143 \
#     -DPCLINCDIR=<prefix>/include -DPCLLIBDIR=<prefix>/lib
# where <prefix>/lib holds PCL-pxi.lib + the six *-pxi.lib. Driven by
# ci/build-module-windows.ps1.
cmake_minimum_required(VERSION 3.20)
project(mpp_pxm CXX)

if(NOT WIN32)
  message(FATAL_ERROR "module/CMakeLists.txt is the Windows-only build; use makefile-x64 on Linux/macOS.")
endif()
if(NOT MSVC)
  message(FATAL_ERROR "module/CMakeLists.txt requires MSVC to match the pinned PCL ABI.")
endif()
if(NOT DEFINED PCLINCDIR OR NOT DEFINED PCLLIBDIR)
  message(FATAL_ERROR "PCLINCDIR and PCLLIBDIR are required: -DPCLINCDIR=<dir with pcl/ headers> -DPCLLIBDIR=<dir with PCL-pxi.lib>")
endif()

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Force /MD (MultiThreadedDLL) for every config so the module's CRT matches the
# PCL-pxi / 3rdparty static libs; CMP0091 (NEW under cmake_minimum 3.20) routes
# this instead of a per-config /MD flag, avoiding a /MD-vs-/MDd D9025 clash.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")

# Strip CMake's default /EHsc and warning-level flags so our explicit /EHa and
# /W3 win cleanly (no "overriding /EHsc with /EHa" D9025 noise).
string(REGEX REPLACE "/EHsc" "" CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS}")
string(REGEX REPLACE "/W[0-4]" "" CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS}")

# Source list mirrors makefile-x64 exactly (MODULE_OBJS + HOST_OBJS).
add_library(mpp-pxm SHARED
  Mpp.cpp
  BigTiffFormat.cpp
  BigTiffInstance.cpp
  ${CMAKE_CURRENT_SOURCE_DIR}/../host/mpp_shm.cpp
  ${CMAKE_CURRENT_SOURCE_DIR}/../host/mpp_protocol.cpp
  ${CMAKE_CURRENT_SOURCE_DIR}/../host/mpp_host.cpp)

# mpp-pxm.dll, no "lib" prefix (matches the .so/.dylib output name).
set_target_properties(mpp-pxm PROPERTIES PREFIX "" OUTPUT_NAME "mpp-pxm" SUFFIX ".dll")

# PCL headers as SYSTEM (MSVC /external:I): the pinned PCL (2.8.x, C++17-era)
# warns from inside its own headers under C++20, which would trip the
# warning-free gate in build-module-windows.ps1; our code stays under /W3.
target_include_directories(mpp-pxm SYSTEM PRIVATE
  ${PCLINCDIR})
target_include_directories(mpp-pxm PRIVATE
  ${CMAKE_CURRENT_SOURCE_DIR}/../host
  ${CMAKE_CURRENT_SOURCE_DIR}/../host/third_party)

# ABI + preprocessor defines: same set the pinned PCL.vcxproj / 3rdparty vcxproj
# compile with (WarningLevel Level3 == /W3, as the real PCL modules use).
# C++20 comes from CMAKE_CXX_STANDARD above (do not also pass /std:c++20 here,
# or MSVC gets the flag twice).
target_compile_options(mpp-pxm PRIVATE
  /permissive- /Zc:__cplusplus /EHa /arch:AVX2 /W3)
target_compile_definitions(mpp-pxm PRIVATE
  WIN32 WIN64 _WINDOWS UNICODE _UNICODE
  __PCL_WINDOWS __PCL_NO_WIN32_MINIMUM_VERSIONS __PCL_AVX2 __PCL_FMA
  _CRT_SECURE_NO_WARNINGS)

target_link_directories(mpp-pxm PRIVATE ${PCLLIBDIR})

# PCL first, then the six vendored 3rdparty archives (order matters: PCL-pxi
# depends on them), then the exact Win32 system import libs the real PCL
# windows modules link. No PixInsight-core .lib -- see header comment.
target_link_libraries(mpp-pxm PRIVATE
  PCL-pxi
  lz4-pxi zstd-pxi zlib-pxi RFC6234-pxi lcms-pxi cminpack-pxi
  kernel32 user32 gdi32 winspool comdlg32 advapi32 shell32 ole32 oleaut32
  uuid odbc32 odbccp32 userenv imm32 shlwapi ws2_32 wldap32 mscms winmm
  crypt32 secur32 normaliz)
