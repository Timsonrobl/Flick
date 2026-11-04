//! Pictures larger than one GPU bitmap holds (over 16 384 px on a side) at
//! full resolution: a grid of tiles, decoded top to bottom in bands of rows
//! on a worker thread and copied into the tiles as they come, so the whole
//! picture is never in memory at once. Each tile covers its cell plus BORDER
//! px of its neighbours'; drawn clipped to its cell, so smoothing and
//! sharpening see across the seams and none shows.

use std::path::Path;

use windows::Win32::Graphics::Direct2D::ID2D1Bitmap1;

use crate::decode;
use crate::gpu::Uploader;

/// A tile's cell, at most (one GPU bitmap holds 16 384).
const CELL: u32 = 4096;
/// Pixels each tile takes from its neighbours on every side.
const BORDER: u32 = 8;
/// Rows decoded at a time: a 30 000 px wide picture's band is ~30 MB.
const BAND: u32 = 256;
/// Full resolution as tiles, up to this much GPU memory (~268 MP); larger
/// pictures get the finest fraction one bitmap holds instead.
pub const MAX_BYTES: u64 = 1 << 30;

pub struct Tile {
    pub bitmap: ID2D1Bitmap1,
    /// Where the bitmap's top left lies in the picture (stored pixels).
    pub origin: (u32, u32),
    /// The part of the picture it draws: left, top, right, bottom.
    pub cell: (u32, u32, u32, u32),
}

/// A picture's tiles, made on a worker thread.
pub struct Tiles(pub Vec<Tile>);

// Bitmaps of the shared device, whose factory is multi-threaded (gpu::Uploader).
unsafe impl Send for Tiles {}

/// The grid for a `size` picture: each tile's bitmap rectangle (cell plus
/// border, inside the picture) and cell, as left, top, right, bottom.
fn layout((w, h): (u32, u32)) -> Vec<((u32, u32, u32, u32), (u32, u32, u32, u32))> {
    let mut tiles = Vec::new();
    for top in (0..h).step_by(CELL as usize) {
        for left in (0..w).step_by(CELL as usize) {
            let cell = (left, top, (left + CELL).min(w), (top + CELL).min(h));
            let rect = (cell.0.saturating_sub(BORDER), cell.1.saturating_sub(BORDER), (cell.2 + BORDER).min(w), (cell.3 + BORDER).min(h));
            tiles.push((rect, cell));
        }
    }
    tiles
}

/// Decodes `path` at full resolution into tiles made with `uploader`.
pub fn build(path: &Path, uploader: &Uploader) -> Result<(Tiles, (u32, u32)), String> {
    let mut tiles: Vec<(Tile, (u32, u32, u32, u32))> = Vec::new();
    let mut size = (0, 0);
    decode::bands(path, BAND, MAX_BYTES, |picture, y, rows, pixels| {
        if tiles.is_empty() {
            size = picture;
            for (rect, cell) in layout(picture) {
                let bitmap = uploader.blank((rect.2 - rect.0, rect.3 - rect.1))?;
                tiles.push((Tile { bitmap, origin: (rect.0, rect.1), cell }, rect));
            }
        }
        let pitch = picture.0 * 4;
        // Every tile whose rows this band reaches gets its part of them.
        for (tile, rect) in &tiles {
            let (top, bottom) = (y.max(rect.1), (y + rows).min(rect.3));
            if top >= bottom {
                continue;
            }
            let at = (top - y) as usize * pitch as usize + rect.0 as usize * 4;
            let into = (0, top - rect.1, rect.2 - rect.0, bottom - rect.1);
            Uploader::copy(&tile.bitmap, into, &pixels[at..], pitch)?;
        }
        Ok(())
    })?;
    Ok((Tiles(tiles.into_iter().map(|(tile, _)| tile).collect()), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Times one file's stages as a window meets them: the screen level
    /// (opening), each zoom level, the bands alone, and the tiles (bands plus
    /// copying into GPU tiles). `$env:FLICK_SAMPLE = "<file>"; cargo test
    /// --release -p flick-app stage_timings -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn stage_timings() {
        let file = std::path::PathBuf::from(std::env::var_os("FLICK_SAMPLE").expect("FLICK_SAMPLE: a file"));
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let picture = decode::decode_screen(&file, (3377, 1440)).unwrap();
        println!("size {:?}, screen level {}x{}: {:.0} ms", picture.full_size, picture.screen.width, picture.screen.height, ms(t));
        for denom in [8, 4, 2] {
            let t = std::time::Instant::now();
            match decode::decode_level(&file, denom) {
                Ok((image, got)) => println!("level 1/{denom}: {}x{} (1/{got}) in {:.0} ms", image.width, image.height, ms(t)),
                Err(e) => println!("level 1/{denom}: {e}"),
            }
        }
        let t = std::time::Instant::now();
        decode::bands(&file, BAND, MAX_BYTES, |_, _, _, _| Ok(())).unwrap();
        println!("full, bands only: {:.0} ms", ms(t));
        let gpu = crate::gpu::Gpu::create(&mut |_| {}).unwrap();
        let uploader = gpu.uploader().unwrap();
        let t = std::time::Instant::now();
        let (tiles, size) = build(&file, &uploader).unwrap();
        println!("full, into {} tiles of {size:?}: {:.0} ms", tiles.0.len(), ms(t));
    }

    #[test]
    fn tiles_cover_the_picture_and_overlap_by_the_border() {
        let tiles = layout((10_000, 5_000));
        assert_eq!(tiles.len(), 3 * 2);
        // Cells tile the picture exactly.
        let area: u64 = tiles.iter().map(|(_, c)| u64::from(c.2 - c.0) * u64::from(c.3 - c.1)).sum();
        assert_eq!(area, 10_000 * 5_000);
        // A middle tile reaches BORDER into its neighbours; edge ones stop at the picture.
        let (rect, cell) = tiles[1];
        assert_eq!(cell, (4096, 0, 8192, 4096));
        assert_eq!(rect, (4096 - BORDER, 0, 8192 + BORDER, 4096 + BORDER));
        assert!(tiles.iter().all(|(r, _)| r.2 - r.0 <= 16_384 && r.3 - r.1 <= 16_384));
    }
}
