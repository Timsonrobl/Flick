//! Synthetic test JPEGs with an embedded EXIF thumbnail, so the bench can be
//! smoke-tested without real photos. Real camera files are what decisions
//! should be based on: these are 4:4:4 and their noise is not camera noise.

use std::path::Path;

use anyhow::Result;
use image::codecs::jpeg::JpegEncoder;
use image::{RgbImage, imageops};

use crate::jpeg;

pub fn generate(dir: &Path, width: u32, height: u32, quality: u8) -> Result<std::path::PathBuf> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(width) << 32 ^ u64::from(height);
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 59) as i32 - 16
    };
    let (fw, fh) = (width as f32, height as f32);
    // Pattern frequencies differ per size, so paging between samples visibly
    // changes the picture (`ext-viewer` detects page turns by that).
    let (freq_u, freq_v) = (9.0 + (width % 13) as f32 * 2.0, 5.0 + (height % 11) as f32 * 2.0);
    let img = RgbImage::from_fn(width, height, |x, y| {
        let (u, v) = (x as f32 / fw, y as f32 / fh);
        let wave = ((u * freq_u).sin() * (v * freq_v).cos() * 60.0) as i32;
        let channel = |base: f32, n: i32| (base + wave as f32 + n as f32).clamp(0.0, 255.0) as u8;
        image::Rgb([channel(60.0 + 140.0 * u, noise()), channel(80.0 + 120.0 * v, noise()), channel(170.0 - 90.0 * u * v, noise())])
    });

    let mut body = Vec::new();
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut body, quality))?;
    let mut thumb = Vec::new();
    imageops::thumbnail(&img, 160, 160 * height / width).write_with_encoder(JpegEncoder::new_with_quality(&mut thumb, 80))?;

    let mut file = body[..2].to_vec(); // SOI
    file.extend_from_slice(&jpeg::exif_app1_with_thumbnail(&thumb));
    file.extend_from_slice(&body[2..]);

    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("synthetic-{width}x{height}-q{quality}.jpg"));
    std::fs::write(&path, file)?;
    Ok(path)
}
