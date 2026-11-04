<#
.SYNOPSIS
Installs the working tree's Flick over this PC's installed copy, to test a
new build as users will run it: one command, no dialogs.

.DESCRIPTION
Builds the installer (tools\installer.ps1: flick.exe, notices, setup.exe),
runs it silently over %LOCALAPPDATA%\Programs\Flick (it quits a running
Flick first; the windows it had go to the close history), checks that the
installed flick.exe is the one just built, and starts it again as an
update would: with --reopen (its windows back) if Flick had windows open,
else resident in the tray (--tray), so the next open is warm.

The version must be a pre-release ("1.1.0-dev", crates\flick-app\Cargo.toml)
so a build from the working tree never calls itself a release; that
version's release, once published, replaces it as an update (update.rs
sorts 1.1.0-dev below 1.1.0). -AllowRelease installs a release version
anyway (the release build itself, before publishing it).

-NoBuild installs what is built already; -NoStart leaves Flick closed.
#>
param([switch]$NoBuild, [switch]$NoStart, [switch]$AllowRelease)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot

$manifest = Get-Content (Join-Path $root 'crates\flick-app\Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'no version in crates\flick-app\Cargo.toml' }
$version = $Matches[1]
if ($version -notmatch '-' -and -not $AllowRelease) {
    throw "$version is a release version: a local build should be the next version's pre-release (e.g. 1.2.0-dev). -AllowRelease to install it anyway."
}

& (Join-Path $PSScriptRoot 'installer.ps1') -NoBuild:$NoBuild
if ($LASTEXITCODE) { throw 'installer build failed' }
$setup = Join-Path $root "target\installer\Flick-$version-setup.exe"

# Windows open before (the installer's --quit sends them to the close history)?
$hadWindows = [bool](Get-Process flick -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 })

$run = Start-Process $setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -PassThru -Wait
if ($run.ExitCode) { throw "setup failed (exit $($run.ExitCode))" }

$installed = Join-Path $env:LOCALAPPDATA 'Programs\Flick\flick.exe'
$built = Join-Path $root 'target\release\flick.exe'
if ((Get-FileHash $installed).Hash -ne (Get-FileHash $built).Hash) { throw "the installed flick.exe isn't the one just built ($installed)" }
$shown = (Get-Item $installed).VersionInfo.ProductVersion
if ($shown -ne $version) { throw "the installed flick.exe says $shown, not $version" }

if (-not $NoStart) {
    Start-Process $installed -ArgumentList $(if ($hadWindows) { '--reopen' } else { '--tray' })
}
Write-Host "Installed Flick $version ($installed)$(if ($NoStart) { '' } elseif ($hadWindows) { ', its windows reopened' } else { ', resident in the tray' })."
