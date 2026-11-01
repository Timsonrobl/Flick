<#
.SYNOPSIS
Registers a Flick build for the current user as an image viewer: "Open with",
and a choice under Settings > Apps > Default apps.

.DESCRIPTION
Runs `flick.exe --register` (crates\flick-app\src\register.rs), which writes
only Flick's own keys and values under HKEY_CURRENT_USER (no admin rights),
for every format that build opens. It never changes which program is the
default; Windows leaves that choice to you (right-click an image > Open with >
Choose another app > Flick > Always, or Settings > Apps > Default apps >
Flick).

The command points at the build output (target\release\flick.exe by
default), so every build is what opens; tools\build.ps1 keeps that path
current while Flick runs. Undo with tools\unregister.ps1. The installer and
its updates register the installed copy the same way.
#>
param([string]$Exe = (Join-Path (Split-Path $PSScriptRoot) 'target\release\flick.exe'))

$ErrorActionPreference = 'Stop'
$Exe = [System.IO.Path]::GetFullPath($Exe)
if (-not (Test-Path $Exe)) { throw "$Exe not found; build first (tools\build.ps1)" }

$run = Start-Process -FilePath $Exe -ArgumentList '--register' -Wait -PassThru
if ($run.ExitCode) { throw "$Exe --register failed (exit $($run.ExitCode))" }
Write-Host "Registered Flick ($Exe)"
