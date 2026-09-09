# Build mpp-pxm.dll + mpp-ipc-worker.exe on Windows, stage the unsigned
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
# This script also does NOT run the PCL API-version gate itself -- that
# lives in check-pcl-api-version.ps1, run as its own module.yml step
# *before* this one.
#
# Adapted (mechanical rename + retarget) from MergeMosaic's
# integration/pixinsight/ci/build-module-windows.ps1; see
# build-module-linux.sh's header comment for the doc-staging and
# LICENSE-PCL double-staging rationale (identical here). MergeMosaic's
# single -Pcl <prefix> parameter is split into -PclInc/-PclLib to match
# module/makefile-x64's PCLINCDIR/PCLLIBDIR variable names, consistent with
# build-module-linux.sh/build-module-macos.sh in this same directory. The
# module itself is built by module/CMakeLists.txt (the Windows-only MSVC
# project; Linux/macOS use module/makefile-x64). CANNOT be run/verified on
# this machine (Linux) -- module.yml's windows job is its verification.
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

# Configure (x64 + toolset v143, pinned so the module's ABI can't drift if
# the runner image advances) and build Release.
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

$dll = Get-ChildItem $build -Recurse -Filter 'mpp-pxm.dll' | Select-Object -First 1
if (-not $dll) { throw 'mpp-pxm.dll not built' }

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
Copy-Item $dll.FullName (Join-Path $Stage 'bin/mpp-pxm.dll') -Force
Copy-Item $worker (Join-Path $Stage 'bin/mpp-ipc-worker.exe') -Force
Copy-Item (Join-Path $docSrc 'LICENSE-PCL') (Join-Path $Stage 'bin/LICENSE-PCL') -Force
Copy-Item (Join-Path $docSrc 'README.md') (Join-Path $Stage 'doc/README.md') -Force
Copy-Item (Join-Path $docSrc 'LICENSE-PCL') (Join-Path $Stage 'doc/LICENSE-PCL') -Force

Write-Host "staged unsigned payload in $Stage/bin: $((Get-ChildItem (Join-Path $Stage 'bin')).Name -join ', ')"
Write-Host "staged documentation in $Stage/doc: $((Get-ChildItem (Join-Path $Stage 'doc')).Name -join ', ')"
