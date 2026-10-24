<#
.SYNOPSIS
Builds Flick (release), also while flick.exe is running.

.DESCRIPTION
A running executable can't be overwritten, but it can be renamed. Every
in-use copy of flick.exe (target\release\flick.exe and its hard-linked twin
in target\release\deps) is renamed to *.old.exe first, so the build writes a
fresh flick.exe at the path Windows has registered (tools\register.ps1).
The running, resident copy keeps working from the renamed file until the
next launch of the new build makes it step down
(crates\flick-app\src\instance.rs). Renamed copies are deleted on later
builds, once nothing runs them.

Extra arguments go to cargo, e.g. .\tools\build.ps1 -p flick-app
(plain $args, no param block: an advanced script would claim -p itself).
#>
$CargoArgs = $args
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$release = Join-Path $root 'target\release'

function Test-InUse([string]$path) {
    try {
        [System.IO.File]::Open($path, 'Open', 'ReadWrite', 'None').Dispose()
        $false
    } catch {
        $true
    }
}

$candidates = @(Get-Item (Join-Path $release 'flick.exe') -ErrorAction SilentlyContinue) +
    @(Get-ChildItem (Join-Path $release 'deps') -Filter 'flick-*.exe' -ErrorAction SilentlyContinue | Where-Object Name -notlike '*.old.exe')

# Earlier renamed copies that no process runs any more.
foreach ($old in @(Get-ChildItem $release -Recurse -Filter '*.old.exe' -ErrorAction SilentlyContinue)) {
    if (-not (Test-InUse $old.FullName)) {
        Remove-Item $old.FullName
    }
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
foreach ($file in $candidates) {
    if (Test-InUse $file.FullName) {
        $renamed = '{0}.{1}.old.exe' -f $file.BaseName, $stamp
        Rename-Item $file.FullName $renamed
        Write-Host "in use, renamed: $($file.Name) -> $renamed"
    }
}

Push-Location $root
try {
    cargo build --release @CargoArgs
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
