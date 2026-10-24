//! Renders the app icon (ARCHITECTURE.md §17) from its SVG sources into a
//! multi-size `assets/icon/flick.ico`, which `flick-app` embeds. A dev tool:
//! the .ico is committed, so building Flick needs neither this crate nor its
//! SVG renderer. Run after changing the artwork:
//!
//!     cargo run --release -p flick-icon
//!
//! 16-24 px come from `flick-small.svg` (heavier lines for the tray and small
//! taskbars), larger sizes from `flick.svg`. Sizes below 256 are stored as
//! BMP entries, 256 as PNG, the classic layout every Windows API reads.

use std::path::{Path, PathBuf};

const SMALL: &[u32] = &[16, 20, 24];
// 30 and 36 are the taskbar's 24 px at 125 % and 150 %, so Windows draws
// them 1:1 instead of shrinking the next larger size.
const LARGE: &[u32] = &[30, 32, 36, 40, 48, 60, 64, 72, 96, 256];

fn main() {
    if let Err(e) = run() {
        eprintln!("flick-icon: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon");
    let small = load(&dir.join("flick-small.svg"))?;
    let large = load(&dir.join("flick.svg"))?;
    let mut icon = ico::IconDir::new(ico::ResourceType::Icon);
    for (tree, sizes) in [(&small, SMALL), (&large, LARGE)] {
        for &size in sizes {
            let image = ico::IconImage::from_rgba_data(size, size, render(tree, size)?);
            let entry = if size >= 256 { ico::IconDirEntry::encode_as_png(&image) } else { ico::IconDirEntry::encode_as_bmp(&image) };
            icon.add_entry(entry.map_err(|e| e.to_string())?);
        }
    }
    let out: PathBuf = dir.join("flick.ico");
    let file = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    icon.write(file).map_err(|e| e.to_string())?;
    println!("wrote {} ({} sizes)", out.display(), SMALL.len() + LARGE.len());
    Ok(())
}

fn load(path: &Path) -> Result<resvg::usvg::Tree, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Straight (not premultiplied) RGBA, as ICO entries store it.
fn render(tree: &resvg::usvg::Tree, size: u32) -> Result<Vec<u8>, String> {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).ok_or("bad size")?;
    let scale = size as f32 / tree.size().width();
    resvg::render(tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Ok(pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect())
}
