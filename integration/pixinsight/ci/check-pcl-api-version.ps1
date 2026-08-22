# Verify the PCL_API_Version declared by whatever -PclInc is about to be
# linked against, against the pin's PCL_API_VERSION_HEX in pcl-pin.env.
#   check-pcl-api-version.ps1 -PclInc <dir>
#
# Asymmetric, not equality (see build-module-linux.sh's matching gate for
# the full rationale, mirrored here): PCL_API_Version becomes the module's
# declared API-version floor, so the DANGEROUS direction is the local
# header claiming something HIGHER than the pin (the module would silently
# lock out cores between the two versions). A local value LOWER than the
# pin is comparatively safe -- a locally installed PixInsight app's bundled
# headers can legitimately lag the pinned GitLab checkout's tag -- and only
# warrants a loud warning, not a failure. Failing to read a PCL_API_Version
# at all is a hard failure either way, never a silent skip.
#
# Split out into its own script (called as its own module.yml workflow
# step, not inlined inside build-module-windows.ps1) so the windows job can
# run this as a hard, non-continue-on-error gate BEFORE the known-gap
# module-build step, which itself is continue-on-error: true. Embedding
# this check inside that step let a real "exceeds the pin" failure get
# silently swallowed by the known-gap tolerance -- exactly what a prior
# review round caught; this file exists specifically to fix that.
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$PclInc
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

$pin = @{}
Get-Content (Join-Path $here 'pcl-pin.env') | ForEach-Object {
  if ($_ -match '^\s*([A-Z_]+)="?([^"]*)"?\s*$') { $pin[$Matches[1]] = $Matches[2] }
}
$apiWant = $pin['PCL_API_VERSION_HEX']
if (-not $apiWant) { throw 'PCL_API_VERSION_HEX not found in pcl-pin.env' }

$apiHdr = Join-Path $PclInc 'pcl/api/APIInterface.h'
if (-not (Test-Path $apiHdr)) { throw "cannot verify PCL_API_Version -- $apiHdr not found (PclInc misconfigured?)" }
$apiHdrText = Get-Content $apiHdr -Raw
$m = [regex]::Match($apiHdrText, '(?m)^#define\s+PCL_API_Version\s+0x([0-9A-Fa-f]+)\s*$')
if (-not $m.Success) { throw "no PCL_API_Version found in $apiHdr" }
$actualHex = $m.Groups[1].Value
$actualDec = [Convert]::ToInt32($actualHex, 16)
$pinnedDec = [Convert]::ToInt32($apiWant, 16)

if ($actualDec -gt $pinnedDec) {
  throw "PCL_API_Version at $PclInc is 0x$actualHex, higher than the pinned 0x$apiWant. This module would declare a higher API-version floor than intended, silently locking out cores between the two versions. Update PCL_API_VERSION_HEX in pcl-pin.env only deliberately, after re-verifying against the pinned checkout."
} elseif ($actualDec -lt $pinnedDec) {
  Write-Warning "PCL_API_Version at $PclInc is 0x$actualHex, lower than the pinned 0x$apiWant -- the locally installed headers likely lag the pinned GitLab checkout's tag. Continuing (the built module will declare whichever floor these headers actually carry; a manual PixInsight load/save smoke test settles which one)."
} else {
  Write-Host "PCL_API_Version confirmed 0x$apiWant at $apiHdr"
}
