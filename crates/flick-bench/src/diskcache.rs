//! Would a disk cache of decoded `Screen` levels beat decoding the JPEG again?
//! (ARCHITECTURE.md §16, "Disk cache: shelved".)
//!
//! For each file: decode the screen level with WIC (the baseline), write it
//! as raw BGRA and as LZ4, then read each back warm (still in the OS file
//! cache) and cold (unbuffered, as after a reboot), including decompression.

use std::io::Read;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};

use crate::decoders::{self, Mode, Request};
use crate::report::median;

const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const SECTOR: usize = 4096;

struct Row {
    name: String,
    decode_ms: f64,
    raw_mb: f64,
    lz4_mb: f64,
    compress_ms: f64,
    write_raw_ms: f64,
    write_lz4_ms: f64,
    warm_raw_ms: f64,
    warm_lz4_ms: f64,
    cold_raw_ms: f64,
    cold_lz4_ms: f64,
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// Reads bypassing the OS file cache; returns the buffer and the data range.
fn read_unbuffered(path: &Path) -> Result<(Vec<u8>, std::ops::Range<usize>)> {
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_NO_BUFFERING).open(path)?;
    let len = file.metadata()?.len() as usize;
    let padded = len.div_ceil(SECTOR) * SECTOR;
    let mut storage = vec![0u8; padded + SECTOR];
    let offset = storage.as_ptr().align_offset(SECTOR);
    let buf = &mut storage[offset..offset + padded];
    // Unbuffered reads must start sector-aligned; the last one comes back
    // short (end of file), so stop once the file's length is in.
    let mut done = 0;
    while done < len {
        let n = file.read(&mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok((storage, offset..offset + len))
}

pub fn run(files: &[PathBuf], screen: (u32, u32), dir: &Path, keep: bool) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut wic = decoders::create("wic")?;
    let mut rows = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let data = std::fs::read(file)?;
        let t = Instant::now();
        let out = wic.decode(&data, &Request { mode: Mode::Screen, screen })?;
        let decode_ms = ms(t);
        let pixels = out.pixels;

        let (raw_path, lz4_path) = (dir.join(format!("{i}.bgra")), dir.join(format!("{i}.lz4")));
        let t = Instant::now();
        std::fs::write(&raw_path, &pixels)?;
        let write_raw_ms = ms(t);
        let t = Instant::now();
        let packed = lz4_flex::compress_prepend_size(&pixels);
        let compress_ms = ms(t);
        let t = Instant::now();
        std::fs::write(&lz4_path, &packed)?;
        let write_lz4_ms = ms(t);

        let t = Instant::now();
        let warm = std::fs::read(&raw_path)?;
        let warm_raw_ms = ms(t);
        assert_eq!(warm.len(), pixels.len());
        let t = Instant::now();
        let unpacked = lz4_flex::decompress_size_prepended(&std::fs::read(&lz4_path)?)?;
        let warm_lz4_ms = ms(t);
        assert_eq!(unpacked.len(), pixels.len());

        let row = Row {
            name: file.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            decode_ms,
            raw_mb: pixels.len() as f64 / 1e6,
            lz4_mb: packed.len() as f64 / 1e6,
            compress_ms,
            write_raw_ms,
            write_lz4_ms,
            warm_raw_ms,
            warm_lz4_ms,
            cold_raw_ms: f64::NAN,
            cold_lz4_ms: f64::NAN,
        };
        rows.push(row);
    }

    // Cold reads only after everything written has reached the SSD; reading
    // while Windows still flushes the writes measures the flush, not the cache.
    for i in 0..rows.len() {
        for ext in ["bgra", "lz4"] {
            std::fs::OpenOptions::new().write(true).open(dir.join(format!("{i}.{ext}")))?.sync_all()?;
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    for (i, row) in rows.iter_mut().enumerate() {
        let t = Instant::now();
        let (buf, range) = read_unbuffered(&dir.join(format!("{i}.bgra")))?;
        row.cold_raw_ms = ms(t);
        assert_eq!(range.len() as f64 / 1e6, row.raw_mb);
        drop(buf);
        let t = Instant::now();
        let (buf, range) = read_unbuffered(&dir.join(format!("{i}.lz4")))?;
        let unpacked = lz4_flex::decompress_size_prepended(&buf[range])?;
        row.cold_lz4_ms = ms(t);
        assert_eq!(unpacked.len() as f64 / 1e6, row.raw_mb);
        eprintln!(
            "  {}: decode {:.1} ms | raw {:.1} MB: cold {:.1} ms, warm {:.1} ms | lz4 {:.1} MB: cold {:.1} ms, warm {:.1} ms (compress {:.1} ms)",
            row.name, row.decode_ms, row.raw_mb, row.cold_raw_ms, row.warm_raw_ms, row.lz4_mb, row.cold_lz4_ms, row.warm_lz4_ms, row.compress_ms
        );
    }

    let med = |get: fn(&Row) -> f64| {
        let mut v: Vec<f64> = rows.iter().map(get).collect();
        median(&mut v)
    };
    println!();
    println!("Medians over {} files (screen {}x{}); times include decompression, not GPU upload", rows.len(), screen.0, screen.1);
    println!("  WIC decode of the JPEG          {:>7.1} ms", med(|r| r.decode_ms));
    println!("  raw BGRA  {:>5.1} MB  cold read   {:>7.1} ms   warm read {:>6.1} ms   write {:>6.1} ms", med(|r| r.raw_mb), med(|r| r.cold_raw_ms), med(|r| r.warm_raw_ms), med(|r| r.write_raw_ms));
    println!(
        "  LZ4       {:>5.1} MB  cold read   {:>7.1} ms   warm read {:>6.1} ms   write {:>6.1} ms (+ compress {:.1} ms)",
        med(|r| r.lz4_mb),
        med(|r| r.cold_lz4_ms),
        med(|r| r.warm_lz4_ms),
        med(|r| r.write_lz4_ms),
        med(|r| r.compress_ms)
    );
    if !keep {
        std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}
