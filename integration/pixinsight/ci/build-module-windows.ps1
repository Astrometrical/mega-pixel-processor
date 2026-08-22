# Build BigTIFF-pxm.dll + mpp-ipc-worker.exe on Windows, stage the unsigned
# payload. Mirrors the gates of build-module-macos.sh: warning-free build,
# the module DLL is produced, the worker builds, and both land in <Stage>/bin.
#   build-module-windows.ps1 -PclInc <dir> -PclLib <dir> -Stage <dir>
#
# Unlike build-module-linux.sh/build-module-macos.sh, this script does NOT
# run the host CTest golden suite itself -- module.yml's windows job runs it
# as its own separate step, *before* calling this script (MMM's own windows
# job orders it that way; the Task 6 brief says "Windows per MMM's pattern"
# for the ctest step specifically). See module.yml for that step.
#
# Adapted (mechanical rename + retarget) from MergeMosaic's
# integration/pixinsight/ci/build-module-windows.ps1; see
# build-module-linux.sh's header comment for the doc-staging and
# LICENSE-PCL double-staging rationale (identical here). MergeMosaic's
# single -Pcl <prefix> parameter is split into -PclInc/-PclLib to match
# module/makefile-x64's PCLINCDIR/PCLLIBDIR variable names, consistent with
# build-module-linux.sh/build-module-macos.sh in this same directory.
#
# KNOWN GAP (flagged, not fixed by this task): unlike MergeMosaic, mpp's
# integration/pixinsight/module/ has NO Windows/MSVC build system yet --
# only module/makefile-x64 + module/Makefile (GNU make + g++/clang++, used
# for Linux and macOS; see makefile-x64's UNAME_S Darwin/else branches).
# There is no module/CMakeLists.txt, so the `cmake -S $moduleDir` step below
# (mechanically adapted from MergeMosaic's, which DOES have that project)
# will fail until a future task adds an MSVC build for the module itself.
# This script (and the "windows" job in .github/workflows/module.yml that
# calls it) is therefore expected to fail at that step on its first real CI
# run -- CANNOT be run/verified on this machine (Linux) either way. Task
# 6's brief explicitly scoped this file as "adapted, minimal/mechanical,
# unverified"; fixing the underlying module Windows build is out of scope
# here (it belongs with Task 4's module build system, not Task 6's CI
# plumbing).
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$PclInc,
  [Parameter(Mandatory=$true)][string]$PclLib,
  [Parameter(Mandatory=$true)][string]$Stage
)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
$moduleDir = Join-Path $repo 'integration/pixinsight/module'
$build = Join-Path $moduleDir 'win-build'
$PclInc = (Resolve-Path $PclInc).Path
$PclLib = (Resolve-Path $PclLib).Path

# PCL API-version gate: build-pcl-windows.ps1 hard-gates a *freshly built*
# PCL, but that never runs on this prebuilt-PCL path. Re-verify against
# whichever -PclInc is actually about to be linked against, so this can't
# silently pass -- a missing header is a hard failure, not a skip. See
# build-module-linux.sh's matching step for the full rationale.
$pin = @{}
Get-Content (Join-Path $PSScriptRoot 'pcl-pin.env') | ForEach-Object {
  if ($_ -match '^\s*([A-Z_]+)="?([^"]*)"?\s*$') { $pin[$Matches[1]] = $Matches[2] }
}
$apiWant = $pin['PCL_API_VERSION_HEX']
if (-not $apiWant) { throw 'PCL_API_VERSION_HEX not found in pcl-pin.env' }
$apiHdr = Join-Path $PclInc 'pcl/api/APIInterface.h'
if (-not (Test-Path $apiHdr)) { throw "cannot verify PCL_API_Version -- $apiHdr not found (PclInc misconfigured?)" }
$apiHdrText = Get-Content $apiHdr -Raw
if ($apiHdrText -notmatch "(?m)^#define\s+PCL_API_Version\s+0x$apiWant\s*$") {
  $got = ($apiHdrText | Select-String 'define PCL_API_Version.*').Matches.Value
  throw "PCL_API_Version at $PclInc != 0x$apiWant (got: $got). This module would declare a wrong API-version floor; update PCL_API_VERSION_HEX in pcl-pin.env only deliberately, after re-verifying against the actual headers."
}
Write-Host "PCL_API_Version confirmed 0x$apiWant at $apiHdr"

# Configure (x64 + toolset v143, pinned so the module's ABI can't drift if
# the runner image advances) and build Release. See the KNOWN GAP note
# above: this cmake project does not exist in mpp's module/ yet.
cmake -S $moduleDir -B $build -A x64 -T v143 -DPCLINCDIR="$PclInc" -DPCLLIBDIR="$PclLib"
if ($LASTEXITCODE -ne 0) { throw "cmake configure failed ($LASTEXITCODE)" }

$buildLog = cmake --build $build --config Release 2>&1 | Tee-Object -Variable _bl
if ($LASTEXITCODE -ne 0) {
  $buildLog | ForEach-Object { $_ }
  throw "module build failed ($LASTEXITCODE)"
}

# Warning-free gate (MSVC warnings are "... : warning Cxxxx: ...").
$warnings = $buildLog | Select-String -Pattern 'warning C'
if ($warnings) {
  $warnings | ForEach-Object { Write-Host $_ }
  throw "ERROR: MSVC warnings in module build"
}

$dll = Get-ChildItem $build -Recurse -Filter 'BigTIFF-pxm.dll' | Select-Object -First 1
if (-not $dll) { throw 'BigTIFF-pxm.dll not built' }

Push-Location $repo
try {
  cargo build --release --target x86_64-pc-windows-msvc -p mpp-ipc-worker
  if ($LASTEXITCODE -ne 0) { throw "cargo build (worker) failed ($LASTEXITCODE)" }
} finally {
  Pop-Location
}
$worker = Join-Path $repo 'target/x86_64-pc-windows-msvc/release/mpp-ipc-worker.exe'
if (-not (Test-Path $worker)) { throw 'mpp-ipc-worker.exe not built' }

$docSrc = Join-Path $repo 'integration/pixinsight'
if (-not (Test-Path (Join-Path $docSrc 'README.md'))) { throw "module documentation missing at $docSrc/README.md" }
if (-not (Test-Path (Join-Path $docSrc 'LICENSE-PCL'))) { throw "PCL license text missing at $docSrc/LICENSE-PCL" }

New-Item -ItemType Directory -Force -Path (Join-Path $Stage 'bin'), (Join-Path $Stage 'doc') | Out-Null
Copy-Item $dll.FullName (Join-Path $Stage 'bin/BigTIFF-pxm.dll') -Force
Copy-Item $worker (Join-Path $Stage 'bin/mpp-ipc-worker.exe') -Force
Copy-Item (Join-Path $docSrc 'LICENSE-PCL') (Join-Path $Stage 'bin/LICENSE-PCL') -Force
Copy-Item (Join-Path $docSrc 'README.md') (Join-Path $Stage 'doc/README.md') -Force
Copy-Item (Join-Path $docSrc 'LICENSE-PCL') (Join-Path $Stage 'doc/LICENSE-PCL') -Force

Write-Host "staged unsigned payload in $Stage/bin: $((Get-ChildItem (Join-Path $Stage 'bin')).Name -join ', ')"
Write-Host "staged documentation in $Stage/doc: $((Get-ChildItem (Join-Path $Stage 'doc')).Name -join ', ')"
