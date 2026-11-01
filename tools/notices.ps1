<#
.SYNOPSIS
Writes the licence notices of every crate built into flick.exe, for the
installer (tools\installer.ps1).

.DESCRIPTION
The crates are flick-app's normal dependencies on Windows (cargo tree; build
and dev dependencies don't ship). Each crate's own licence files are copied
from cargo's registry: a text several crates share (the Apache licence,
mostly) is written once, under the list of crates it covers. A crate that
ships no licence file takes those of a crate from the same repository (its
siblings: zune-inflate's are zune-jpeg's); one with neither is named on the
console, so it can be looked into.
#>
param([string]$Out = (Join-Path (Split-Path $PSScriptRoot) 'target\installer\THIRD-PARTY-NOTICES.txt'))

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot

Push-Location $root
try {
    $tree = cargo tree -p flick-app -e normal --prefix none -f '{p}' --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE) { throw 'cargo tree failed' }
    $metadata = cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json
    if ($LASTEXITCODE) { throw 'cargo metadata failed' }
} finally {
    Pop-Location
}

# "name v1.2.3", then " (path)" for local crates and " (*)" for repeats.
$shipped = @{}
foreach ($line in $tree) {
    if ($line -match '^(\S+) v(\S+)') { $shipped["$($Matches[1]) $($Matches[2])"] = $true }
}

function Get-LicenceFiles($crate) {
    $dir = Split-Path $crate.manifest_path
    $files = @(Get-ChildItem $dir -File | Where-Object Name -match '^(licen[cs]e|copying|notice|unlicense|copyright)')
    if ($crate.license_file) {
        $files += Get-Item (Join-Path $dir $crate.license_file)
    }
    $files | Sort-Object FullName -Unique
}

# Crates that name no repository, and the one their source lives in.
$repositoryOf = @{ 'zune-inflate' = 'https://github.com/etemesi254/zune-image' }
function Get-Repository($crate) {
    $url = if ($crate.repository) { $crate.repository } else { $repositoryOf[$crate.name] }
    if ($url) { ($url -replace '/tree/.*$', '').TrimEnd('/') }
}

$crates = $metadata.packages | Where-Object { $shipped["$($_.name) $($_.version)"] -and $_.source } | Sort-Object name, version
$files = @{}
$byRepository = @{}
foreach ($crate in $crates) {
    $files[$crate.id] = @(Get-LicenceFiles $crate)
    if ($files[$crate.id] -and ($repository = Get-Repository $crate)) { $byRepository[$repository] = $crate }
}

$texts = [ordered]@{}   # licence text -> crates it covers
$missing = @()
foreach ($crate in $crates) {
    $name = "$($crate.name) $($crate.version) ($($crate.license))"
    $own = $files[$crate.id]
    if (-not $own -and ($repository = Get-Repository $crate) -and ($sibling = $byRepository[$repository])) {
        $own = $files[$sibling.id]
        $name += ", by $($sibling.name)'s licence files (same repository)"
    }
    if (-not $own) {
        $missing += $name
        continue
    }
    foreach ($file in $own) {
        $text = (Get-Content $file.FullName -Raw).Trim()
        if (-not $texts.Contains($text)) { $texts[$text] = [System.Collections.Generic.List[string]]::new() }
        $texts[$text].Add($name)
    }
}

$rule = '=' * 78
$parts = @(
    'Flick is free software under the GNU General Public License, version 3 or later (LICENSE).',
    'It is built with the crates below, each under its own licence, reproduced here as each crate ships it.',
    "$($crates.Count) crates."
)
foreach ($entry in $texts.GetEnumerator()) {
    $parts += $rule
    $parts += $entry.Value | Sort-Object -Unique
    $parts += $rule
    $parts += $entry.Key
}
New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
Set-Content -Path $Out -Value ($parts -join "`r`n`r`n") -Encoding utf8

Write-Host "Notices for $($crates.Count) crates ($($texts.Count) distinct texts): $Out"
if ($missing) {
    Write-Warning "No licence file shipped with: $($missing -join ', ')"
}
