//! Measurement records and the text report.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decoders::Mode;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Measurement {
    pub file: PathBuf,
    pub decoder: String,
    pub mode: Mode,
    pub file_bytes: u64,
    /// JPEG encoding summary from `jpeg::format`, e.g. "baseline 4:2:0 thumb".
    #[serde(default)]
    pub format: String,
    pub src: (u32, u32),
    pub out: (u32, u32),
    pub denom: u32,
    /// Decoder construction (COM/WIC init, handles); paid once per process.
    pub init_ms: f64,
    /// First decode: includes page-faulting the mmapped file and lazy init.
    pub first_ms: f64,
    pub median_ms: f64,
    pub min_ms: f64,
    /// Peak private commit above the pre-decode baseline.
    pub peak_mb: Option<f64>,
    pub error: Option<String>,
}

impl Measurement {
    pub fn new(file: &Path, decoder: &str, mode: Mode) -> Self {
        Self {
            file: file.to_path_buf(),
            decoder: decoder.to_string(),
            mode,
            file_bytes: 0,
            format: String::new(),
            src: (0, 0),
            out: (0, 0),
            denom: 1,
            init_ms: 0.0,
            first_ms: 0.0,
            median_ms: 0.0,
            min_ms: 0.0,
            peak_mb: None,
            error: None,
        }
    }

    fn megapixels(&self) -> f64 {
        f64::from(self.src.0) * f64::from(self.src.1) / 1e6
    }
}

pub fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 0 { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn peak(m: Option<f64>) -> String {
    m.map_or_else(|| "-".into(), |mb| format!("{mb:.0}"))
}

/// One progress line per measurement.
pub fn line(m: &Measurement) -> String {
    match &m.error {
        Some(e) => format!("{:<12} {:<6} {}: ERROR {e}", m.decoder, m.mode, file_name(&m.file)),
        None => format!(
            "{:<12} {:<6} {} ({:.1} MP) -> {}x{} 1/{}: first {:.1} ms, median {:.1} ms, peak {} MB",
            m.decoder,
            m.mode,
            file_name(&m.file),
            m.megapixels(),
            m.out.0,
            m.out.1,
            m.denom,
            m.first_ms,
            m.median_ms,
            peak(m.peak_mb),
        ),
    }
}

pub fn print(all: &[Measurement], screen: (u32, u32)) {
    let ok: Vec<&Measurement> = all.iter().filter(|m| m.error.is_none()).collect();

    let mut groups: Vec<(&str, Mode)> = Vec::new();
    for m in &ok {
        if !groups.contains(&(m.decoder.as_str(), m.mode)) {
            groups.push((m.decoder.as_str(), m.mode));
        }
    }

    // One row per file, one median-ms column per decoder/mode.
    let mut files: Vec<&Measurement> = Vec::new();
    for m in &ok {
        if !files.iter().any(|f| f.file == m.file) {
            files.push(m);
        }
    }
    println!();
    println!("Per file (median ms)");
    let headers: Vec<String> = groups.iter().map(|(d, m)| format!("{d}/{m}")).collect();
    print!("{:<28} {:>5} {:>5}  {:<26}", "file", "MP", "MB", "format");
    for h in &headers {
        print!(" {:>w$}", h, w = h.len().max(7));
    }
    println!();
    for f in &files {
        print!("{:<28} {:>5.1} {:>5.1}  {:<26}", file_name(&f.file), f.megapixels(), f.file_bytes as f64 / 1e6, f.format);
        for ((decoder, mode), h) in groups.iter().zip(&headers) {
            let cell = ok
                .iter()
                .find(|m| m.file == f.file && m.decoder == *decoder && m.mode == *mode)
                .map_or_else(|| "-".into(), |m| format!("{:.1}", m.median_ms));
            print!(" {:>w$}", cell, w = h.len().max(7));
        }
        println!();
    }

    println!();
    println!("Per decoder and mode (medians across files; screen {}x{})", screen.0, screen.1);
    println!("{:<12} {:<6} {:>5} {:>10} {:>9} {:>7} {:>8} {:>8}", "decoder", "mode", "files", "median ms", "first ms", "MP/s", "init ms", "peak MB");
    for (decoder, mode) in &groups {
        let rows: Vec<&&Measurement> = ok.iter().filter(|m| m.decoder == *decoder && m.mode == *mode).collect();
        let mut med: Vec<f64> = rows.iter().map(|m| m.median_ms).collect();
        let mut first: Vec<f64> = rows.iter().map(|m| m.first_ms).collect();
        let mut init: Vec<f64> = rows.iter().map(|m| m.init_ms).collect();
        let mut rate: Vec<f64> = rows.iter().map(|m| m.megapixels() / (m.median_ms / 1000.0)).collect();
        let max_peak = rows.iter().filter_map(|m| m.peak_mb).reduce(f64::max);
        println!(
            "{:<12} {:<6} {:>5} {:>10.1} {:>9.1} {:>7.0} {:>8.1} {:>8}",
            decoder,
            mode,
            rows.len(),
            median(&mut med),
            median(&mut first),
            median(&mut rate),
            median(&mut init),
            peak(max_peak),
        );
    }

    // What the viewer actually waits for: the faster of Screen and Full.
    println!();
    println!("Time to a screen-ready image (best of screen/full per file, then median)");
    let mut decoders: Vec<&str> = Vec::new();
    for (decoder, _) in &groups {
        if !decoders.contains(decoder) {
            decoders.push(decoder);
        }
    }
    for decoder in decoders {
        let mut files: Vec<&Path> = Vec::new();
        for m in ok.iter().filter(|m| m.decoder == decoder && matches!(m.mode, Mode::Full | Mode::Screen)) {
            if !files.contains(&m.file.as_path()) {
                files.push(&m.file);
            }
        }
        if files.is_empty() {
            continue;
        }
        let mut best: Vec<f64> = files
            .iter()
            .map(|f| {
                ok.iter()
                    .filter(|m| m.decoder == decoder && matches!(m.mode, Mode::Full | Mode::Screen) && m.file == *f)
                    .map(|m| m.median_ms)
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        println!("  {:<12} {:>8.1} ms  ({} files)", decoder, median(&mut best), files.len());
    }

    let errors: Vec<&Measurement> = all.iter().filter(|m| m.error.is_some()).collect();
    if !errors.is_empty() {
        println!();
        println!("{} measurements failed:", errors.len());
        for m in errors {
            println!("  {}", line(m));
        }
    }
}
