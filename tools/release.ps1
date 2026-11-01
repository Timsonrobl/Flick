#Requires -Version 7
<#
.SYNOPSIS
Builds a release: the installer, flick.exe, and the signed update manifest,
in target\release-upload, ready for GitHub Releases.

.DESCRIPTION
Builds with tools\installer.ps1, then writes update.txt (update.rs reads it):

  flick-update 1
  version 0.2.0
  url https://github.com/Timsonrobl/Flick/releases/download/v0.2.0/flick.exe
  size 10456576
  sha256 <hex>
  signature <hex: ECDSA P-256 over SHA-256 of every line above, r then s>

signed with the key from tools\update-key.ps1. Installed copies fetch
.../releases/latest/download/update.txt, so the newest release is what they
see. Upload (creates the tag and release):

  gh release create v<version> target\release-upload\* --title "Flick <version>"

-BaseUrl serves the exe from elsewhere (a local test server); -NoBuild
packages what is built already.
#>
param([string]$BaseUrl, [switch]$NoBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$keyFile = Join-Path $env:USERPROFILE '.flick\update-signing.key'
if (-not (Test-Path $keyFile)) { throw "No signing key ($keyFile): tools\update-key.ps1 makes one" }

& (Join-Path $PSScriptRoot 'installer.ps1') -NoBuild:$NoBuild
$manifest = Get-Content (Join-Path $root 'crates\flick-app\Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'no version in crates\flick-app\Cargo.toml' }
$version = $Matches[1]
if (-not $BaseUrl) { $BaseUrl = "https://github.com/Timsonrobl/Flick/releases/download/v$version" }

$out = Join-Path $root 'target\release-upload'
New-Item -ItemType Directory -Force $out | Out-Null
Get-ChildItem $out -File | Remove-Item
$exe = Join-Path $out 'flick.exe'
Copy-Item (Join-Path $root 'target\release\flick.exe') $exe
Copy-Item (Join-Path $root "target\installer\Flick-$version-setup.exe") $out

$body = @(
    'flick-update 1'
    "version $version"
    "url $BaseUrl/flick.exe"
    "size $((Get-Item $exe).Length)"
    "sha256 $((Get-FileHash $exe -Algorithm SHA256).Hash.ToLower())"
) -join "`n"
$body += "`n"

Add-Type -AssemblyName System.Security
$pkcs8 = [System.Security.Cryptography.ProtectedData]::Unprotect([System.IO.File]::ReadAllBytes($keyFile), $null, 'CurrentUser')
$key = [System.Security.Cryptography.ECDsa]::Create()
$key.ImportPkcs8PrivateKey($pkcs8, [ref]$null)
$signature = $key.SignData([System.Text.Encoding]::UTF8.GetBytes($body), [System.Security.Cryptography.HashAlgorithmName]::SHA256)
[System.IO.File]::WriteAllText((Join-Path $out 'update.txt'), $body + "signature $([Convert]::ToHexString($signature).ToLower())`n")

Write-Host "Flick $version in ${out}:"
Get-ChildItem $out | ForEach-Object { Write-Host ("  {0} ({1:N1} MB)" -f $_.Name, ($_.Length / 1MB)) }
Write-Host "Upload: gh release create v$version $out\* --title `"Flick $version`""
