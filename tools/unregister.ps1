<#
.SYNOPSIS
Removes what tools\register.ps1 added for the current user, and nothing else.

.DESCRIPTION
Deletes Flick's own keys (Flick.Image, Applications\flick.exe,
Software\Flick) and Flick's single value under each extension's
OpenWithProgids and under RegisteredApplications. Other programs' entries are
left alone. If Flick was the default for a type, Windows falls back to asking.
#>
$ErrorActionPreference = 'Stop'
$classes = 'HKCU:\Software\Classes'
$extensions = '.jpg', '.jpeg', '.jpe', '.jfif', '.png', '.apng', '.bmp', '.dib', '.gif', '.tif', '.tiff', '.webp',
    '.heic', '.heif', '.avif', '.jxl', '.jxr', '.wdp', '.ico', '.svg', '.svgz', '.exr', '.hdr'

foreach ($key in "$classes\Flick.Image", "$classes\Applications\flick.exe", 'HKCU:\Software\Flick') {
    if (Test-Path $key) { Remove-Item $key -Recurse }
}
foreach ($ext in $extensions) {
    Remove-ItemProperty -Path "$classes\$ext\OpenWithProgids" -Name 'Flick.Image' -ErrorAction SilentlyContinue
}
Remove-ItemProperty -Path 'HKCU:\Software\RegisteredApplications' -Name 'Flick' -ErrorAction SilentlyContinue

Add-Type -Namespace Flick -Name Shell -MemberDefinition '[DllImport("shell32.dll")] public static extern void SHChangeNotify(int eventId, uint flags, IntPtr item1, IntPtr item2);'
[Flick.Shell]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
Write-Host 'Flick unregistered.'
