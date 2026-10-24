<#
.SYNOPSIS
Registers Flick for the current user as an image viewer: "Open with", and a
choice under Settings > Apps > Default apps.

.DESCRIPTION
Writes only under HKEY_CURRENT_USER (no admin rights), and only Flick's own
keys and values:
  Software\Classes\Flick.Image                       the "Flick image" file type
  Software\Classes\Applications\flick.exe            the application entry
  Software\Classes\<ext>\OpenWithProgids : Flick.Image  one value per extension
  Software\Flick\Capabilities, Software\RegisteredApplications : Flick
It never changes which program is the default; Windows leaves that choice to
you (right-click an image > Open with > Choose another app > Flick > Always,
or Settings > Apps > Default apps > Flick).

The command points at the build output (target\release\flick.exe by
default), so every build is what opens; tools\build.ps1 keeps that path
current while Flick runs. Undo with tools\unregister.ps1.
#>
param([string]$Exe = (Join-Path (Split-Path $PSScriptRoot) 'target\release\flick.exe'))

$ErrorActionPreference = 'Stop'
$Exe = [System.IO.Path]::GetFullPath($Exe)
if (-not (Test-Path $Exe)) { throw "$Exe not found; build first (tools\build.ps1)" }

# Every format Flick opens (folder.rs IMAGE_EXTENSIONS). HEIC/HEIF and AVIF
# need Windows' HEIF + HEVC and AV1 extensions; the rest work without.
$extensions = '.jpg', '.jpeg', '.jpe', '.jfif', '.png', '.apng', '.bmp', '.dib', '.gif', '.tif', '.tiff', '.webp',
    '.heic', '.heif', '.avif', '.jxl', '.jxr', '.wdp', '.ico', '.svg', '.svgz', '.exr', '.hdr'
$classes = 'HKCU:\Software\Classes'
$progId = 'Flick.Image'
$command = "`"$Exe`" `"%1`""

# New-Item -Force would wipe an existing key's values (other apps' entries
# under .jpg\OpenWithProgids), so only create keys that are missing.
function Use-Key([string]$path) {
    if (-not (Test-Path $path)) { New-Item -Path $path -Force | Out-Null }
    $path
}
function Set-Value([string]$path, [string]$name, [string]$value) {
    New-ItemProperty -Path (Use-Key $path) -Name $name -Value $value -PropertyType String -Force | Out-Null
}

Set-Value "$classes\$progId" '(default)' 'Flick image'
Set-Value "$classes\$progId\DefaultIcon" '(default)' "`"$Exe`",0"
Set-Value "$classes\$progId\shell\open\command" '(default)' $command

$app = "$classes\Applications\flick.exe"
Set-Value $app 'FriendlyAppName' 'Flick'
Set-Value "$app\shell\open\command" '(default)' $command

$capabilities = 'HKCU:\Software\Flick\Capabilities'
Set-Value $capabilities 'ApplicationName' 'Flick'
Set-Value $capabilities 'ApplicationDescription' 'Fast frameless image viewer'
foreach ($ext in $extensions) {
    Set-Value "$classes\$ext\OpenWithProgids" $progId ''
    Set-Value "$app\SupportedTypes" $ext ''
    Set-Value "$capabilities\FileAssociations" $ext $progId
}
Set-Value 'HKCU:\Software\RegisteredApplications' 'Flick' 'Software\Flick\Capabilities'

# Tell Explorer that associations changed.
Add-Type -Namespace Flick -Name Shell -MemberDefinition '[DllImport("shell32.dll")] public static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);'
[Flick.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)

Write-Host "Registered Flick ($Exe) for: $($extensions -join ' ')"
