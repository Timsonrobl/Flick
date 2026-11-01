<#
.SYNOPSIS
Removes what tools\register.ps1 added for the current user, and nothing else.

.DESCRIPTION
Runs `flick.exe --unregister` (crates\flick-app\src\register.rs): deletes
Flick's own keys (Flick.Image, Applications\flick.exe, Software\Flick) and
Flick's single value under each extension's OpenWithProgids and under
RegisteredApplications. Other programs' entries are left alone. If Flick was
the default for a type, Windows falls back to asking.
#>
param([string]$Exe = (Join-Path (Split-Path $PSScriptRoot) 'target\release\flick.exe'))

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "$Exe not found; build first (tools\build.ps1)" }

$run = Start-Process -FilePath $Exe -ArgumentList '--unregister' -Wait -PassThru
if ($run.ExitCode) { throw "$Exe --unregister failed (exit $($run.ExitCode))" }
Write-Host 'Flick unregistered.'
