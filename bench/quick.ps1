<#
.SYNOPSIS
  The quick benchmark: cold opens, warm opens and page turns in about a
  minute, compared with a saved baseline (pass/fail), and logged for trends.

.DESCRIPTION
  Builds Flick (unless -NoBuild), closes any running Flick (cold opens need
  none resident), then runs flick-bench ext-viewer three ways on one photo:
    cold   -Runs launches with --standalone   (time to final picture, memory)
    warm   -Runs launches handed to a resident (time to final picture, memory)
    paging -Pages key presses in one window    (key -> picture, memory after;
           a folder with Pages or fewer pictures is replaced by Pages + 1
           flipped and rotated JPEG copies of the photo, made once)
  It watches the screen: don't touch the keyboard or mouse while it runs.

  Results:
    - a table: baseline, previous run, now, change, and pass/fail;
    - bench/results/quick-history.csv: one line per run (commit, note, every
      figure), for what each iteration cost in time and memory;
    - raw ext-viewer JSON in bench/quick-runs/<timestamp>/ (git-ignored).
  Exit code 1 if any check fails.

.EXAMPLE
  .\bench\quick.ps1 -Note "stars"            # run, compare, log
  .\bench\quick.ps1 -SetBaseline              # run and make this the baseline
#>
param(
    [string]$File,
    [int]$Runs = 5,
    [int]$Pages = 20,
    [string]$Note = '',
    [switch]$SetBaseline,
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$exe = Join-Path $root 'target\release\flick.exe'
$bench = Join-Path $root 'target\release\flick-bench.exe'
$baselinePath = Join-Path $PSScriptRoot 'results\quick-baseline.json'
$historyPath = Join-Path $PSScriptRoot 'results\quick-history.csv'
$stamp = Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'
$runDir = Join-Path $PSScriptRoot "quick-runs\$stamp"

if (-not $File) {
    # Committed (CC0, bench\images\CREDITS.md), so every checkout measures the same photo.
    $File = Join-Path $PSScriptRoot 'images\pixel-7-2.jpg'
}
if (-not (Test-Path $File)) { throw "no benchmark photo: pass -File (default: bench\images\pixel-7-2.jpg)" }

if (-not $NoBuild) {
    & (Join-Path $root 'tools\build.ps1') -p flick-app | Out-Null
    & cargo build --release -p flick-bench 2>&1 | Out-Null
    if ($LASTEXITCODE) { throw 'flick-bench failed to build' }
}

$running = @(Get-Process flick -ErrorAction SilentlyContinue)
if ($running.Count) {
    Write-Host "Closing $($running.Count) running Flick process(es): cold opens need none resident."
    $running | Stop-Process -Force
    Start-Sleep -Milliseconds 500
}

New-Item -ItemType Directory -Force $runDir | Out-Null

# Paging needs a folder with more pictures than page turns, each visibly
# different (the bench watches the screen for the change). The committed
# default photo has one sibling, so a small folder is replaced by Pages + 1
# copies of the photo, flipped and rotated in turn, made once in
# bench\quick-runs\paging-<photo> and reused.
$pagingFile = $File
$siblings = @(Get-ChildItem (Split-Path $File) -File | Where-Object { $_.Extension -match '^\.(jpe?g|png|gif|webp|avif|heic|jxl|tiff?|bmp)$' })
if ($siblings.Count -le $Pages) {
    $pagingDir = Join-Path $PSScriptRoot "quick-runs\paging-$([IO.Path]::GetFileNameWithoutExtension($File))"
    $ext = [IO.Path]::GetExtension($File)
    if (@(Get-ChildItem $pagingDir -File -ErrorAction SilentlyContinue).Count -lt $Pages + 1) {
        Write-Host "Making $($Pages + 1) flipped and rotated copies of $(Split-Path $File -Leaf) in $pagingDir (once)."
        New-Item -ItemType Directory -Force $pagingDir | Out-Null
        Add-Type -AssemblyName System.Drawing
        $flips = @([System.Drawing.RotateFlipType]::RotateNoneFlipNone, [System.Drawing.RotateFlipType]::RotateNoneFlipX,
                   [System.Drawing.RotateFlipType]::Rotate180FlipNone, [System.Drawing.RotateFlipType]::RotateNoneFlipY)
        $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object MimeType -eq 'image/jpeg'
        $quality = New-Object System.Drawing.Imaging.EncoderParameters(1)
        $quality.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter([System.Drawing.Imaging.Encoder]::Quality, [long]92)
        foreach ($i in 1..($Pages + 1)) {
            $img = [System.Drawing.Image]::FromFile($File)
            $img.RotateFlip($flips[($i - 1) % $flips.Count])
            $img.Save((Join-Path $pagingDir ('{0:D3}.jpg' -f $i)), $codec, $quality)
            $img.Dispose()
        }
    }
    $pagingFile = Join-Path $pagingDir '001.jpg'
    Write-Host "Paging through $($Pages + 1) copies of $(Split-Path $File -Leaf): its folder has only $($siblings.Count) picture(s)."
}
# The screen is what's measured: keep the display on meanwhile.
$keepAwake = Start-Process $bench -ArgumentList 'keep-display', '--minutes', '10' -WindowStyle Hidden -PassThru
$started = Get-Date
function Invoke-Bench([string]$Name, [string[]]$Arguments, [string]$Target = $File) {
    $json = Join-Path $runDir "$Name.json"
    Write-Host "  $Name..."
    & $bench ext-viewer --exe $exe @Arguments --json $json $Target 2>&1 | Out-File (Join-Path $runDir "$Name.log")
    Get-Content $json -Raw | ConvertFrom-Json
}
Write-Host "Benchmarking $(Split-Path $File -Leaf) (hands off the keyboard and mouse)"
try {
    $cold = Invoke-Bench 'cold' @('--args', '--standalone --size 3377x1440 {file}', '--runs', $Runs)
    $warm = Invoke-Bench 'warm' @('--args', '--size 3377x1440 {file}', '--resident', '--runs', $Runs)
    $paging = Invoke-Bench 'paging' @('--args', '--standalone --size 3377x1440 {file}', '--runs', 1, '--page', $Pages) $pagingFile
} finally {
    Stop-Process -Id $keepAwake.Id -ErrorAction SilentlyContinue
}
$seconds = ((Get-Date) - $started).TotalSeconds

function Median([double[]]$Values) {
    if (-not $Values.Count) { return [double]::NaN }
    $sorted = $Values | Sort-Object
    $n = $sorted.Count
    if ($n % 2) { $sorted[($n - 1) / 2] } else { ($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2 }
}
function Percentile([double[]]$Values, [double]$P) {
    if (-not $Values.Count) { return [double]::NaN }
    $sorted = $Values | Sort-Object
    $sorted[[math]::Min($sorted.Count - 1, [math]::Floor($P * $sorted.Count))]
}
function Max([double[]]$Values) { if ($Values.Count) { ($Values | Measure-Object -Maximum).Maximum } else { [double]::NaN } }

$coldOk = @($cold | Where-Object { -not $_.error })
$warmOk = @($warm | Where-Object { -not $_.error })
$turns = @($paging[0].pages | ForEach-Object { [double]$_.final_ms })
$all = @($cold) + @($warm) + @($paging)
$failedRuns = @($all | Where-Object { $_.error }).Count + $(if ($paging[0].page_error) { 1 } else { 0 })

# Every figure, in the order of the table and the history file. Lower is
# better for all; `Limit` is the allowed rise over the baseline (absolute,
# or a fraction of it, whichever is larger), null for information only.
$metrics = [ordered]@{
    'cold open, median (ms)'      = @{ Value = Median ($coldOk | ForEach-Object { $_.final_ms }); Abs = 15; Rel = 0.08 }
    'cold open, worst (ms)'       = @{ Value = Max ($coldOk | ForEach-Object { $_.final_ms }); Abs = 40; Rel = 0.20 }
    'warm open, median (ms)'      = @{ Value = Median ($warmOk | ForEach-Object { $_.final_ms }); Abs = 15; Rel = 0.10 }
    'warm open, worst (ms)'       = @{ Value = Max ($warmOk | ForEach-Object { $_.final_ms }); Abs = 40; Rel = 0.25 }
    'page turn, median (ms)'      = @{ Value = Median $turns; Abs = 8; Rel = 0.25 }
    'page turn, p90 (ms)'         = @{ Value = Percentile $turns 0.9; Abs = 15; Rel = 0.30 }
    'blank or flicker frames'     = @{ Value = @($all | Where-Object { $_.blank_ms -gt 0 -or $_.glitch_ms.Count }).Count; Abs = 0; Rel = 0 }
    'failed runs'                 = @{ Value = $failedRuns; Abs = 0; Rel = 0 }
    'cold, private (MB)'          = @{ Value = Median ($coldOk | ForEach-Object { $_.private_mb }); Abs = 10; Rel = 0.08 }
    'cold, working set (MB)'      = @{ Value = Median ($coldOk | ForEach-Object { $_.working_set_mb }); Abs = 8; Rel = 0.10 }
    'resident after warm, private (MB)' = @{ Value = [double]$warm[-1].private_mb; Abs = 15; Rel = 0.10 }
    'resident after warm, working set (MB)' = @{ Value = [double]$warm[-1].working_set_mb; Abs = 10; Rel = 0.12 }
    'after paging, private (MB)'  = @{ Value = [double]$paging[0].pages[-1].private_mb; Abs = 15; Rel = 0.10 }
    'after paging, working set (MB)' = @{ Value = [double]$paging[0].pages[-1].working_set_mb; Abs = 10; Rel = 0.12 }
    'flick.exe (MB)'              = @{ Value = [math]::Round((Get-Item $exe).Length / 1MB, 2); Abs = $null; Rel = $null }
}

$commit = (& git -C $root rev-parse --short HEAD 2>$null)
if (& git -C $root status --porcelain 2>$null) { $commit += '+changes' }

$baseline = if (Test-Path $baselinePath) { Get-Content $baselinePath -Raw | ConvertFrom-Json } else { $null }
$previous = if (Test-Path $historyPath) { Import-Csv $historyPath | Select-Object -Last 1 } else { $null }

# Numbers as "212.5" whatever the Windows locale (the history is read back).
$invariant = [cultureinfo]::InvariantCulture
function Num($Value, [string]$Format = '0.#') {
    if ($null -eq $Value -or "$Value" -eq '' -or [double]::IsNaN([double]$Value)) { return '-' }
    ([double]$Value).ToString($Format, $invariant)
}
$rows = foreach ($name in $metrics.Keys) {
    $m = $metrics[$name]
    $now = [double]$m.Value
    $base = if ($baseline) { $baseline.metrics.$name } else { $null }
    $prev = if ($previous -and $previous.$name -ne '' -and $previous.$name -ne '-') { [double]::Parse($previous.$name, $invariant) } else { $null }
    $status = 'info'
    if ($null -ne $m.Abs -and $m.Abs -eq 0) {
        $status = if ($now -le 0) { 'ok' } else { 'FAIL' } # zero means zero, baseline or not
    } elseif ($null -ne $m.Abs -and $null -ne $base) {
        $allowed = [math]::Max([double]$m.Abs, [double]$m.Rel * [double]$base)
        $status = if ($now -le [double]$base + $allowed) { 'ok' } else { 'FAIL' }
    }
    [pscustomobject]@{
        Metric        = $name
        Baseline      = Num $base
        Previous      = Num $prev
        Now           = Num $now
        'vs previous' = if ($null -ne $prev) { Num ($now - $prev) '+0.#;-0.#;0' } else { '-' }
        'vs baseline' = if ($null -ne $base) { Num ($now - [double]$base) '+0.#;-0.#;0' } else { '-' }
        Check         = $status
    }
}

Write-Host ''
Write-Host ([string]::Format($invariant, "Commit {0}{1}, {2:0} s; baseline: {3}", $commit, $(if ($Note) { " ($Note)" } else { '' }), $seconds, $(if ($baseline) { "$($baseline.commit) ($($baseline.date))" } else { 'none yet (-SetBaseline)' })))
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

# History: one line per run, for trends across iterations.
$line = [ordered]@{ date = $stamp; commit = $commit; note = $Note; photo = (Split-Path $File -Leaf) }
foreach ($name in $metrics.Keys) { $line[$name] = Num $metrics[$name].Value '0.##' }
# -Force: a history from before a metric was added still takes the new line.
[pscustomobject]$line | Export-Csv $historyPath -Append -NoTypeInformation -Encoding utf8 -Force

if ($SetBaseline) {
    $values = [ordered]@{}
    foreach ($name in $metrics.Keys) { $values[$name] = [double]$metrics[$name].Value }
    [ordered]@{ date = $stamp; commit = $commit; note = $Note; photo = (Split-Path $File -Leaf); metrics = $values } |
        ConvertTo-Json -Depth 3 | Set-Content $baselinePath -Encoding utf8
    Write-Host "Saved as the baseline: $baselinePath"
}
Write-Host "Logged to $historyPath; raw runs in $runDir"

$failures = @($rows | Where-Object Check -eq 'FAIL')
if ($failures.Count) {
    Write-Host "$($failures.Count) check(s) failed: $(($failures | ForEach-Object Metric) -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'All checks passed.' -ForegroundColor Green
