<#
.SYNOPSIS
Keeps the built installer and flick.exe as the build of a commit, so a
regression can be traced by installing or benchmarking earlier commits
without rebuilding them.

.DESCRIPTION
Copies target\installer\Flick-<version>-setup.exe and target\release\flick.exe
to builds\<commit>\ (git-ignored), with the commit's subject, and keeps the
newest -Keep commits (3 by default).

The build must be the commit's code: no uncommitted changes under crates\,
installer\ or the Cargo files, and the installer built after every file
there last changed. A build tested from the working tree and then committed
as it was passes both, so run this after the commit. -Force skips the check
(for a build known to be the commit's, made from a stash, say).

tools\install-local.ps1 calls this itself after building a clean tree, and
installs a kept build with -Commit <hash>.
#>
param([string]$Commit = 'HEAD', [int]$Keep = 3, [switch]$Force)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$code = 'crates', 'installer', 'Cargo.toml', 'Cargo.lock'

$hash = (git -C $root rev-parse --short $Commit).Trim()
if ($LASTEXITCODE) { throw "no commit $Commit" }
$manifest = Get-Content (Join-Path $root 'crates\flick-app\Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw 'no version in crates\flick-app\Cargo.toml' }
$version = $Matches[1]
$setup = Join-Path $root "target\installer\Flick-$version-setup.exe"
$exe = Join-Path $root 'target\release\flick.exe'
if (-not (Test-Path $setup)) { throw "no installer built ($setup)" }

if (-not $Force) {
    if ($hash -ne (git -C $root rev-parse --short HEAD).Trim()) { throw "$hash isn't HEAD: only HEAD's build can be checked (-Force if you know it is $hash's)" }
    $dirty = git -C $root status --porcelain -- @code
    if ($dirty) { throw "uncommitted changes in the code: the build isn't $hash's" }
    $built = (Get-Item $setup).LastWriteTime
    $newer = git -C $root ls-files -- @code | ForEach-Object { Get-Item (Join-Path $root $_) } | Where-Object { $_.LastWriteTime -gt $built } | Select-Object -First 1
    if ($newer) { throw "the code changed after the installer was built: rebuild first" }
}

$dir = Join-Path $root "builds\$hash"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $setup, $exe $dir -Force
git -C $root log -1 --format='%h %ad %s' --date=short $hash | Set-Content (Join-Path $dir 'commit.txt')
Write-Host "Kept the build of $hash ($version) in builds\$hash."

# The newest $Keep commits stay.
Get-ChildItem (Join-Path $root 'builds') -Directory | Sort-Object LastWriteTime -Descending | Select-Object -Skip $Keep | ForEach-Object {
    Remove-Item $_.FullName -Recurse -Force
    Write-Host "Dropped the kept build of $($_.Name)."
}
