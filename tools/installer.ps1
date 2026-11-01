<#
.SYNOPSIS
Builds Flick's installer: target\installer\Flick-<version>-setup.exe.

.DESCRIPTION
Builds flick.exe (tools\build.ps1, release, C runtime linked in), writes the
dependencies' licence notices (tools\notices.ps1), then compiles
installer\flick.iss with Inno Setup 6 (winget install JRSoftware.InnoSetup).
The version is flick-app's, from its Cargo.toml.

-NoBuild packages the flick.exe already built.
#>
param([switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found: winget install JRSoftware.InnoSetup' }

if (-not $NoBuild) {
    & (Join-Path $PSScriptRoot 'build.ps1') -p flick-app
    if ($LASTEXITCODE) { throw 'build failed' }
}
& (Join-Path $PSScriptRoot 'notices.ps1')

$manifest = Get-Content (Join-Path $root 'crates\flick-app\Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'no version in crates\flick-app\Cargo.toml' }
$version = $Matches[1]

& $iscc /Qp "/DVersion=$version" (Join-Path $root 'installer\flick.iss')
if ($LASTEXITCODE) { throw "ISCC failed (exit $LASTEXITCODE)" }
$setup = Get-Item (Join-Path $root "target\installer\Flick-$version-setup.exe")
Write-Host ("{0} ({1:N1} MB)" -f $setup.FullName, ($setup.Length / 1MB))
