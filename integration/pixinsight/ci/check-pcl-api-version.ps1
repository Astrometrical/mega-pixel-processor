# Verify the PCL_API_Version declared by whatever -PclInc is about to be
# linked against, against the pin's PCL_API_VERSION_HEX in pcl-pin.env.
#   check-pcl-api-version.ps1 -PclInc <dir>
#
# Strict equality (see build-module-linux.sh's matching gate for the full
# rationale, mirrored here): PCL_API_Version becomes the module's declared
# API-version floor. HIGHER than the pin silently locks out cores between
# the two versions; LOWER means linking headers older than the pin actually
# tested. Either direction is a misconfigured -PclInc -- point it at a
# prefix built from the pin. Failing to read a PCL_API_Version at all is
# likewise a hard failure, never a silent skip.
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

if ($actualDec -ne $pinnedDec) {
  throw "PCL_API_Version at $PclInc is 0x$actualHex, but the pin (pcl-pin.env) requires exactly 0x$apiWant. The module's declared core-API floor comes from these headers; point -PclInc at a prefix built from the pin. Update PCL_API_VERSION_HEX only deliberately -- raising it drops older cores."
}
Write-Host "PCL_API_Version confirmed 0x$apiWant at $apiHdr"
