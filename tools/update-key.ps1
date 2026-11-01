#Requires -Version 7
<#
.SYNOPSIS
Makes the key pair that signs Flick's updates (once), or backs it up.

.DESCRIPTION
An ECDSA P-256 key pair. The private key is kept encrypted for this Windows
account (DPAPI) in %USERPROFILE%\.flick\update-signing.key, outside the
repository; tools\release.ps1 signs each release's manifest with it. The
public key goes to crates\flick-app\src\update.key, built into flick.exe
(update.rs), which then accepts only manifests this key signed.

Losing the private key means no installed Flick ever accepts another update
(a new key needs a new installer by hand): keep a backup.
  -Backup <file>  writes the private key encrypted with a password you type
                  (PKCS#8), to keep somewhere safe.
  -Restore <file> takes the key back from such a backup on a new PC.
#>
param([string]$Backup, [string]$Restore, [switch]$Force)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$keyFile = Join-Path $env:USERPROFILE '.flick\update-signing.key'
$publicFile = Join-Path $root 'crates\flick-app\src\update.key'
Add-Type -AssemblyName System.Security
$scope = [System.Security.Cryptography.DataProtectionScope]::CurrentUser

function Save-Private([byte[]]$pkcs8) {
    New-Item -ItemType Directory -Force (Split-Path $keyFile) | Out-Null
    [System.IO.File]::WriteAllBytes($keyFile, [System.Security.Cryptography.ProtectedData]::Protect($pkcs8, $null, $scope))
}

function Save-Public([System.Security.Cryptography.ECDsa]$key) {
    $q = $key.ExportParameters($false).Q
    Set-Content -Path $publicFile -Value ([Convert]::ToHexString($q.X + $q.Y).ToLower()) -NoNewline
}

if ($Backup) {
    $pkcs8 = [System.Security.Cryptography.ProtectedData]::Unprotect([System.IO.File]::ReadAllBytes($keyFile), $null, $scope)
    $key = [System.Security.Cryptography.ECDsa]::Create()
    $key.ImportPkcs8PrivateKey($pkcs8, [ref]$null)
    $password = Read-Host 'Password for the backup' -AsSecureString
    $pbe = [System.Security.Cryptography.PbeParameters]::new('Aes256Cbc', 'SHA256', 600000)
    [System.IO.File]::WriteAllBytes($Backup, $key.ExportEncryptedPkcs8PrivateKey([System.Net.NetworkCredential]::new('', $password).Password, $pbe))
    Write-Host "Backed up to $Backup"
    return
}

if ((Test-Path $keyFile) -and -not $Force) {
    throw "$keyFile exists: a new key would lock every installed Flick out of updates. -Force to replace it anyway."
}
$key = [System.Security.Cryptography.ECDsa]::Create([System.Security.Cryptography.ECCurve+NamedCurves]::nistP256)
if ($Restore) {
    $password = Read-Host 'Password of the backup' -AsSecureString
    $key.ImportEncryptedPkcs8PrivateKey([System.Net.NetworkCredential]::new('', $password).Password, [System.IO.File]::ReadAllBytes($Restore), [ref]$null)
}
Save-Private $key.ExportPkcs8PrivateKey()
Save-Public $key
Write-Host "Private key: $keyFile (this Windows account only; back it up with -Backup)"
Write-Host "Public key:  $publicFile (commit it; flick.exe checks updates against it)"
