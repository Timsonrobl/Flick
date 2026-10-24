# Writes one sample per format into bench/samples/formats (git-ignored) for
# milestone 7: ffmpeg for most formats, flick-bench gen-formats for JPEG XL
# and HEIC. Needs ffmpeg on PATH, the synthetic JPEGs from
# `flick-bench gen-sample bench/samples`, and a release build of flick-bench.
param([string]$Out = "$PSScriptRoot\samples\formats", [switch]$LargeAnimations)

$ErrorActionPreference = 'Stop'
$photo = "$PSScriptRoot\samples\synthetic-4032x3024-q90.jpg"
$bench = "$PSScriptRoot\..\target\release\flick-bench.exe"
New-Item -ItemType Directory -Force $Out | Out-Null

function Run-Ffmpeg([string[]]$Arguments) {
    & ffmpeg -v error -y @Arguments
    if ($LASTEXITCODE) { throw "ffmpeg failed: $Arguments" }
}

# Sources: a 4032x3024 photo-like picture, and a 1600x1200 test card whose
# alpha fades from 0 at the left to opaque at the right.
$alphaCard = "testsrc2=s=1600x1200,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='255*X/W'"
Run-Ffmpeg @('-i', $photo, "$Out\png-photo.png")
Run-Ffmpeg @('-f', 'lavfi', '-i', $alphaCard, '-frames:v', '1', "$Out\png-alpha.png")

Run-Ffmpeg @('-f', 'lavfi', '-i', $alphaCard, '-frames:v', '1', '-pix_fmt', 'rgba64be', "$Out\png16-alpha.png")
Run-Ffmpeg @('-f', 'lavfi', '-i', 'testsrc2=s=20000x1200', '-frames:v', '1', "$Out\png-panorama-20000.png")
Run-Ffmpeg @('-f', 'lavfi', '-i', 'testsrc2=s=640x480:r=12,format=rgba', '-frames:v', '24', '-plays', '0', '-f', 'apng', "$Out\apng-anim.png")
Run-Ffmpeg @('-f', 'lavfi', '-i', 'testsrc2=s=640x480:r=12', '-frames:v', '24', "$Out\gif-anim.gif")

if ($LargeAnimations) {
    # Video-like animations far over the 256 MB a decoded animation may keep
    # (1080p, 300 frames: ~2.4 GB composed), so playback has to stream.
    $video = 'testsrc2=s=1920x1080:r=25'
    Run-Ffmpeg @('-f', 'lavfi', '-i', $video, '-frames:v', '300', "$Out\gif-video-1080p.gif")
    Run-Ffmpeg @('-f', 'lavfi', '-i', $video, '-frames:v', '300', '-c:v', 'libwebp_anim', '-quality', '70', '-loop', '0', "$Out\webp-video-1080p.webp")
}

Run-Ffmpeg @('-i', $photo, '-c:v', 'libwebp', '-quality', '85', "$Out\webp-lossy.webp")
Run-Ffmpeg @('-i', "$Out\png-alpha.png", '-c:v', 'libwebp', '-lossless', '1', "$Out\webp-lossless-alpha.webp")
Run-Ffmpeg @('-f', 'lavfi', '-i', 'testsrc2=s=640x480:r=12', '-frames:v', '24', '-c:v', 'libwebp_anim', '-loop', '0', "$Out\webp-anim.webp")

Run-Ffmpeg @('-i', $photo, '-c:v', 'libaom-av1', '-still-picture', '1', '-crf', '30', '-cpu-used', '8', "$Out\avif-photo.avif")

Run-Ffmpeg @('-i', $photo, '-vf', 'scale=3000:2250', '-pix_fmt', 'rgb48le', "$Out\tiff16.tif")
Run-Ffmpeg @('-i', $photo, '-vf', 'scale=1600:1200', "$Out\bmp.bmp")

# HDR: the test card with values up to 4.0, so tone mapping has work to do.
$bright = "testsrc2=s=1600x1200,format=gbrpf32le,geq=r='r(X,Y)*4':g='g(X,Y)*4':b='b(X,Y)*4'"
Run-Ffmpeg @('-f', 'lavfi', '-i', $bright, '-frames:v', '1', '-compression', 'zip16', "$Out\exr-hdr.exr")
Run-Ffmpeg @('-f', 'lavfi', '-i', $bright, '-frames:v', '1', "$Out\radiance.hdr")

& $bench gen-formats "$Out\png-alpha.png" $Out --stem alpha
& $bench gen-formats "$Out\png-photo.png" $Out --stem photo

@'
<svg xmlns="http://www.w3.org/2000/svg" width="800" height="600" viewBox="0 0 800 600">
  <defs>
    <linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#6aa7ff"/><stop offset="1" stop-color="#f3d1ff"/>
    </linearGradient>
  </defs>
  <rect width="800" height="600" fill="url(#sky)"/>
  <circle cx="600" cy="150" r="80" fill="#ffd34d" stroke="#e0a800" stroke-width="6"/>
  <path d="M0 450 Q200 330 400 450 T800 430 V600 H0 Z" fill="#3c8d5a"/>
  <path d="M0 510 Q250 420 520 520 T800 500 V600 H0 Z" fill="#2c6b44"/>
  <text x="40" y="80" font-family="Segoe UI, sans-serif" font-size="56" fill="#1c2a4a">Flick SVG</text>
</svg>
'@ | Set-Content -Encoding utf8 "$Out\vector.svg"

Get-ChildItem $Out | Select-Object Name, Length
