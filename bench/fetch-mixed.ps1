<#
.SYNOPSIS
Fetches the "mixed" benchmark set: real camera and phone JPEGs from Wikimedia
Commons "Taken with <camera>" categories.

.DESCRIPTION
For each camera, picks files at the sensor's native resolution (so they are
camera output or full-size exports, not crops) and takes the ones at the 1/3
and 2/3 points of the file-size distribution: typical files, not extremes.

The picks are recorded in bench/mixed-sources.json (tracked: source page,
author, licence). The images go to bench/photos/mixed (git-ignored). With an
existing manifest only missing files are downloaded; -Discover re-picks.
#>
param(
    [switch]$Discover,
    # Write/print the manifest without downloading.
    [switch]$ListOnly,
    [int]$PerCamera = 2,
    [double]$MaxMB = 40
)

$ErrorActionPreference = 'Stop'
$ua = 'Flick-bench/0.1 (image viewer benchmark sample collection)'
$api = 'https://commons.wikimedia.org/w/api.php'
$manifestPath = Join-Path $PSScriptRoot 'mixed-sources.json'
$outDir = Join-Path $PSScriptRoot 'photos/mixed'

# Native sizes are landscape; portrait files match them rotated.
$cameras = @(
    @{ name = 'sony-a7rv';        category = 'Taken with Sony ILCE-7RM5';                  sizes = @(, @(9504, 6336)) }
    @{ name = 'nikon-d850';       category = 'Taken with Nikon D850';                      sizes = @(, @(8256, 5504)) }
    @{ name = 'canon-r5';         category = 'Taken with Canon EOS R5';                    sizes = @(, @(8192, 5464)) }
    @{ name = 'canon-5d4';        category = 'Taken with Canon EOS 5D Mark IV';            sizes = @(, @(6720, 4480)) }
    @{ name = 'fuji-xt3';         category = 'Taken with Fujifilm X-T3';                   sizes = @(, @(6240, 4160)) }
    @{ name = 'sony-a7iii';       category = 'Taken with Sony ILCE-7M3';                   sizes = @(, @(6000, 4000)) }
    @{ name = 'nikon-d750';       category = 'Taken with Nikon D750';                      sizes = @(, @(6016, 4016)) }
    @{ name = 'olympus-em1ii';    category = 'Taken with Olympus E-M1 Mark II';            sizes = @(, @(5184, 3888)) }
    @{ name = 'iphone-15promax';  category = 'Taken with iPhone 15 Pro Max (main camera)'; sizes = @(@(4032, 3024), @(5712, 4284)) }
    @{ name = 'galaxy-s23ultra';  category = 'Taken with Samsung Galaxy S23 Ultra';        sizes = @(@(4000, 3000), @(8160, 6120)) }
    @{ name = 'pixel-7';          category = 'Taken with Google Pixel 7';                  sizes = @(, @(4080, 3072)) }
)

# Wikimedia rate-limits anonymous clients: pace requests and back off on 429.
function Invoke-Paced([scriptblock]$request, [int]$pauseMs) {
    for ($attempt = 1; ; $attempt++) {
        Start-Sleep -Milliseconds $pauseMs
        try {
            return & $request
        } catch {
            $status = $_.Exception.Response.StatusCode.value__
            if ($status -ne 429 -or $attempt -ge 6) { throw }
            $wait = 30 * $attempt
            Write-Host "rate limited; waiting $wait s"
            Start-Sleep -Seconds $wait
        }
    }
}

function Invoke-Api([hashtable]$query) {
    Invoke-Paced { Invoke-RestMethod -Uri $api -Body $query -UserAgent $ua } 1500
}

function Get-CategoryFiles([string]$category, [int]$limit = 300) {
    $files = @()
    $continue = @{}
    do {
        $query = @{
            action = 'query'; format = 'json'
            generator = 'categorymembers'; gcmtitle = "Category:$category"; gcmtype = 'file'; gcmlimit = '100'
            prop = 'imageinfo'; iiprop = 'url|size|mime|extmetadata'; iiextmetadatafilter = 'LicenseShortName|Artist'
        }
        foreach ($k in $continue.Keys) { $query[$k] = $continue[$k] }
        $r = Invoke-Api $query
        if ($r.query) { $files += $r.query.pages.PSObject.Properties.Value | Where-Object { $_.imageinfo } }
        $continue = @{}
        if ($r.continue) { foreach ($p in $r.continue.PSObject.Properties) { $continue[$p.Name] = $p.Value } }
    } while ($continue.Count -gt 0 -and $files.Count -lt $limit)
    $files
}

function Test-NativeSize($info, $sizes) {
    foreach ($s in $sizes) {
        if (($info.width -eq $s[0] -and $info.height -eq $s[1]) -or ($info.width -eq $s[1] -and $info.height -eq $s[0])) { return $true }
    }
    $false
}

if ($Discover -or -not (Test-Path $manifestPath)) {
    $picks = foreach ($cam in $cameras) {
        $candidates = @(Get-CategoryFiles $cam.category | Where-Object {
            $i = $_.imageinfo[0]
            $i.mime -eq 'image/jpeg' -and $i.size -le $MaxMB * 1MB -and (Test-NativeSize $i $cam.sizes)
        } | Sort-Object { $_.imageinfo[0].size })
        Write-Host ("{0,-16} {1,4} native-size JPEGs" -f $cam.name, $candidates.Count)
        if ($candidates.Count -eq 0) { continue }
        $taken = @{}
        for ($k = 1; $k -le $PerCamera; $k++) {
            $index = [math]::Min($candidates.Count - 1, [math]::Floor($candidates.Count * $k / ($PerCamera + 1)))
            if ($taken[$index]) { continue }
            $taken[$index] = $true
            $file = $candidates[$index]
            $i = $file.imageinfo[0]
            [pscustomobject]@{
                file    = '{0}-{1}.jpg' -f $cam.name, $k
                camera  = $cam.category -replace '^Taken with ', ''
                width   = $i.width
                height  = $i.height
                bytes   = $i.size
                title   = $file.title
                page    = $i.descriptionurl
                url     = $i.url
                license = $i.extmetadata.LicenseShortName.value
                artist  = ($i.extmetadata.Artist.value -replace '<[^>]+>', '').Trim()
            }
        }
    }
    $picks | ConvertTo-Json -Depth 3 | Set-Content -Path $manifestPath -Encoding utf8
    Write-Host "wrote $manifestPath"
}

$manifest = Get-Content $manifestPath -Raw | ConvertFrom-Json
if ($ListOnly) {
    $manifest | Format-Table file, width, height, @{ n = 'MB'; e = { [math]::Round($_.bytes / 1MB, 1) } }, license, artist -AutoSize | Out-String -Width 220
    Write-Host ("{0} files, {1:N0} MB total" -f @($manifest).Count, (($manifest | Measure-Object bytes -Sum).Sum / 1MB))
    return
}
New-Item -ItemType Directory -Force $outDir | Out-Null
$total = 0
foreach ($entry in $manifest) {
    $path = Join-Path $outDir $entry.file
    if ((Test-Path $path) -and (Get-Item $path).Length -eq $entry.bytes) { continue }
    Write-Host ("downloading {0} ({1:N1} MB) from {2}" -f $entry.file, ($entry.bytes / 1MB), $entry.page)
    Invoke-Paced { Invoke-WebRequest -Uri $entry.url -OutFile $path -UserAgent $ua } 5000
    $total += $entry.bytes
}
Write-Host ("{0} files in manifest, {1:N0} MB downloaded this run" -f @($manifest).Count, ($total / 1MB))
