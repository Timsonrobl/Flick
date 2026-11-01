//! Flick's benchmarks (bench/README.md): which JPEG decoders to build around,
//! and how much of the cold-start budget an empty winit + wgpu window already
//! spends.

mod decoders;
#[cfg(windows)]
mod diskcache;
#[cfg(windows)]
mod external;
mod formats;
mod jpeg;
mod report;
mod sample;
mod sys;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use decoders::{Mode, Request};
use report::Measurement;

/// Fallback when the display can't be queried.
const DEFAULT_SCREEN: (u32, u32) = (2560, 1440);

#[derive(Parser)]
#[command(about = "Decoder and startup benchmarks for Flick")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Time decoders on JPEG files. Each measurement runs in its own child
    /// process, so peak memory is exact and decoders can't warm each other up.
    Decode(DecodeArgs),
    /// Time process start to first frame for an empty frameless winit + wgpu window.
    Startup(StartupArgs),
    /// Write synthetic JPEGs (with EXIF thumbnails) for smoke tests.
    GenSample(GenSampleArgs),
    /// Write JPEG XL and HEIC samples from a PNG (the formats ffmpeg can't
    /// write; `bench/gen-formats.ps1` makes the others).
    GenFormats {
        source: PathBuf,
        dir: PathBuf,
        #[arg(long, default_value = "sample")]
        stem: String,
    },
    /// List the decoders compiled into this build.
    Decoders,
    /// Time an external viewer (e.g. Honeyview) end to end by watching the screen.
    #[cfg(windows)]
    ExtViewer(ExtViewerArgs),
    /// Time a disk cache of decoded screen levels (raw and LZ4, warm and cold)
    /// against decoding the JPEG with WIC.
    #[cfg(windows)]
    DiskCache {
        /// JPEG files or folders.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        screen: Option<Size>,
        #[arg(long, default_value = "target/disk-cache-bench")]
        dir: PathBuf,
        /// Keep the cache files instead of deleting them afterwards.
        #[arg(long)]
        keep: bool,
    },
    /// Keep the display on (and the PC awake) for unattended screen-based runs.
    /// A power request held by this process only; no settings change.
    #[cfg(windows)]
    KeepDisplay {
        #[arg(long, default_value_t = 180)]
        minutes: u64,
    },
    /// One measurement; used by `decode`.
    #[command(hide = true)]
    Worker(WorkerArgs),
}

#[derive(Clone, Copy, Debug)]
struct Size(u32, u32);

impl FromStr for Size {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| format!("expected WxH, got {s}"))?;
        let parse = |v: &str| v.trim().parse::<u32>().map_err(|e| format!("{v}: {e}"));
        Ok(Size(parse(w)?, parse(h)?))
    }
}

#[derive(Args)]
struct DecodeArgs {
    /// JPEG files or folders.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Decoders to run (default: all in this build).
    #[arg(long, value_delimiter = ',')]
    decoders: Vec<String>,
    #[arg(long, value_delimiter = ',', default_value = "full,screen,preview,thumb")]
    modes: Vec<Mode>,
    /// Decodes per measurement; the first is reported separately.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// Target screen for `screen` mode (default: primary display).
    #[arg(long)]
    screen: Option<Size>,
    /// Descend into subfolders.
    #[arg(long)]
    recursive: bool,
    /// Stop after this many files.
    #[arg(long)]
    limit: Option<usize>,
    /// Also write every measurement as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Run in this process (faster; peak memory and first-run numbers are then meaningless).
    #[arg(long)]
    in_process: bool,
}

#[derive(Args)]
struct WorkerArgs {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    decoder: String,
    #[arg(long)]
    mode: Mode,
    #[arg(long)]
    runs: usize,
    #[arg(long)]
    screen: Size,
}

#[derive(Args)]
struct StartupArgs {
    #[arg(long, default_value_t = 10)]
    runs: usize,
    #[arg(long, value_delimiter = ',', default_value = "wgpu-dx12,wgpu-vulkan,wgpu-gl,gdi,d2d,d2d-dxgi")]
    backends: Vec<String>,
    /// Decode this file in parallel and draw it as the first (and only) frame.
    #[arg(long)]
    image: Option<PathBuf>,
    /// Probe window size (default: 1280x800).
    #[arg(long)]
    size: Option<Size>,
    #[arg(long)]
    json: Option<PathBuf>,
}

#[cfg(windows)]
#[derive(Args)]
struct ExtViewerArgs {
    /// The viewer executable; it is launched as `<exe> <file>`, or `<exe> <args>`.
    #[arg(long)]
    exe: PathBuf,
    /// Arguments instead of just the file; `{file}` is replaced by the image
    /// path, e.g. "--backend gdi --stay --image {file}".
    #[arg(long, allow_hyphen_values = true)]
    args: Option<String>,
    /// Image files or folders.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Cold launches per file.
    #[arg(long, default_value_t = 5)]
    runs: usize,
    /// On each file's first run, press `--page-key` this many times and time each page turn.
    #[arg(long, default_value_t = 0)]
    page: usize,
    #[arg(long, default_value = "right")]
    page_key: String,
    /// After the key presses: this many fast wheel flicks (down: onward),
    /// each `--wheel-notches` notches `--wheel-interval-ms` apart; timed from
    /// the last notch to the screen holding still (the lag a flick leaves).
    #[arg(long, default_value_t = 0)]
    wheel_bursts: usize,
    #[arg(long, default_value_t = 6)]
    wheel_notches: usize,
    #[arg(long, default_value_t = 40)]
    wheel_interval_ms: u64,
    /// How long the window must hold still to count as final.
    #[arg(long, default_value_t = 700)]
    settle_ms: u64,
    #[arg(long, default_value_t = 15000)]
    timeout_ms: u64,
    /// Save every distinct screen sample as a PNG in this folder.
    #[arg(long)]
    dump: Option<PathBuf>,
    /// Warm opens: start the viewer once (it stays resident) and time
    /// launches that hand their file to it.
    #[arg(long)]
    resident: bool,
    /// With --resident: wait this long before each warm open.
    #[arg(long, default_value_t = 0)]
    idle_ms: u64,
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Args)]
struct GenSampleArgs {
    dir: PathBuf,
    #[arg(long, value_delimiter = ',', default_value = "4032x3024,6000x4000,8256x5504")]
    sizes: Vec<Size>,
    #[arg(long, default_value_t = 90)]
    quality: u8,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Decode(args) => decode(args),
        Cmd::Startup(args) => startup(args),
        Cmd::GenSample(args) => {
            for Size(w, h) in args.sizes {
                let t = Instant::now();
                let path = sample::generate(&args.dir, w, h, args.quality)?;
                eprintln!("wrote {} in {:.1} s", path.display(), t.elapsed().as_secs_f64());
            }
            Ok(())
        }
        Cmd::GenFormats { source, dir, stem } => formats::generate(&source, &dir, &stem),
        Cmd::Decoders => {
            for d in decoders::ALL {
                let modes: Vec<String> = d.modes.iter().map(Mode::to_string).collect();
                println!("{:<12} {:<18} {}", d.name, modes.join(","), d.note);
            }
            Ok(())
        }
        #[cfg(windows)]
        Cmd::ExtViewer(args) => {
            let mut files = Vec::new();
            collect_files(&args.paths, false, &mut files)?;
            let opts = external::Options {
                exe: args.exe,
                args: args.args,
                runs: args.runs,
                page: args.page,
                page_key: external::parse_key(&args.page_key)?,
                wheel: (args.wheel_bursts, args.wheel_notches, std::time::Duration::from_millis(args.wheel_interval_ms)),
                settle: std::time::Duration::from_millis(args.settle_ms),
                timeout: std::time::Duration::from_millis(args.timeout_ms),
                dump: args.dump,
                resident: args.resident,
                idle: std::time::Duration::from_millis(args.idle_ms),
            };
            let kind = if opts.resident { "warm" } else { "cold" };
            eprintln!("{} files, {} {kind} launches each, settle {} ms", files.len(), opts.runs, args.settle_ms);
            let launches = external::run(&files, &opts)?;
            external::print(&launches);
            if let Some(path) = args.json {
                write_json(&path, &launches)?;
            }
            Ok(())
        }
        #[cfg(windows)]
        #[cfg(windows)]
        Cmd::DiskCache { paths, screen, dir, keep } => {
            let mut files = Vec::new();
            collect_files(&paths, false, &mut files)?;
            let screen = screen.map(|Size(w, h)| (w, h)).or_else(sys::primary_display).unwrap_or(DEFAULT_SCREEN);
            diskcache::run(&files, screen, &dir, keep)
        }
        Cmd::KeepDisplay { minutes } => {
            use windows::Win32::System::Power::{ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState};
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED) };
            eprintln!("holding the display on for {minutes} minutes");
            std::thread::sleep(std::time::Duration::from_secs(minutes * 60));
            Ok(())
        }
        Cmd::Worker(args) => {
            let m = measure(&args.file, &args.decoder, args.mode, args.runs, (args.screen.0, args.screen.1));
            println!("{}", serde_json::to_string(&m)?);
            Ok(())
        }
    }
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn measure(file: &Path, decoder: &str, mode: Mode, runs: usize, screen: (u32, u32)) -> Measurement {
    let mut m = Measurement::new(file, decoder, mode);
    if let Err(e) = measure_into(&mut m, runs, screen) {
        m.error = Some(format!("{e:#}"));
    }
    m
}

fn measure_into(m: &mut Measurement, runs: usize, screen: (u32, u32)) -> Result<()> {
    let file = std::fs::File::open(&m.file)?;
    // mmap rather than read: file pages are not private commit, so the peak
    // below is the decoder's own memory, and the first run pays page faults
    // the way a cold open does.
    let data = unsafe { memmap2::Mmap::map(&file)? };
    m.file_bytes = data.len() as u64;
    m.src = jpeg::dimensions(&data).context("no JPEG start-of-frame found")?;
    m.format = jpeg::format(&data).unwrap_or_default();

    let baseline = sys::memory();
    let t = Instant::now();
    let mut decoder = decoders::create(&m.decoder)?;
    m.init_ms = ms(t);

    let req = Request { mode: m.mode, screen };
    let mut times = Vec::with_capacity(runs.max(1));
    for _ in 0..runs.max(1) {
        let t = Instant::now();
        let out = decoder.decode(&data, &req)?;
        times.push(ms(t));
        m.out = (out.width, out.height);
        m.denom = out.denom;
        drop(out);
    }
    m.first_ms = times[0];
    m.median_ms = report::median(&mut times);
    m.min_ms = times[0];

    if let (Some(before), Some(after)) = (baseline, sys::memory()) {
        m.peak_mb = Some(after.peak_private.saturating_sub(before.private) as f64 / (1024.0 * 1024.0));
    }
    Ok(())
}

fn is_jpeg(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

fn collect_files(paths: &[PathBuf], recursive: bool, out: &mut Vec<PathBuf>) -> Result<()> {
    for path in paths {
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("reading {}", path.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            entries.sort();
            let (dirs, files): (Vec<_>, Vec<_>) = entries.into_iter().partition(|p| p.is_dir());
            out.extend(files.into_iter().filter(|p| is_jpeg(p)));
            if recursive {
                collect_files(&dirs, true, out)?;
            }
        } else if path.is_file() {
            out.push(path.clone());
        } else {
            bail!("{} does not exist", path.display());
        }
    }
    Ok(())
}

fn decode(args: DecodeArgs) -> Result<()> {
    let screen = args.screen.map(|Size(w, h)| (w, h)).or_else(sys::primary_display).unwrap_or(DEFAULT_SCREEN);
    let mut files = Vec::new();
    collect_files(&args.paths, args.recursive, &mut files)?;
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }
    if files.is_empty() {
        bail!("no JPEG files found");
    }
    let selected: Vec<&decoders::Info> = if args.decoders.is_empty() {
        decoders::ALL.iter().collect()
    } else {
        args.decoders
            .iter()
            .map(|n| decoders::info(n).with_context(|| format!("decoder {n} is not in this build (see `flick-bench decoders`)")))
            .collect::<Result<_>>()?
    };

    eprintln!("screen {}x{}, {} files, {} runs per measurement", screen.0, screen.1, files.len(), args.runs);
    let exe = std::env::current_exe()?;
    let mut all = Vec::new();
    for file in &files {
        for d in &selected {
            for &mode in args.modes.iter().filter(|m| d.modes.contains(m)) {
                let m = if args.in_process {
                    measure(file, d.name, mode, args.runs, screen)
                } else {
                    run_worker(&exe, file, d.name, mode, args.runs, screen)?
                };
                eprintln!("  {}", report::line(&m));
                all.push(m);
            }
        }
    }

    report::print(&all, screen);
    if let Some(path) = args.json {
        write_json(&path, &all)?;
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn run_worker(exe: &Path, file: &Path, decoder: &str, mode: Mode, runs: usize, screen: (u32, u32)) -> Result<Measurement> {
    let output = Command::new(exe)
        .arg("worker")
        .arg("--file")
        .arg(file)
        .args(["--decoder", decoder, "--mode", &mode.to_string(), "--runs", &runs.to_string()])
        .args(["--screen", &format!("{}x{}", screen.0, screen.1)])
        .output()
        .context("spawning worker")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(m) = stdout.lines().last().and_then(|l| serde_json::from_str(l).ok()) {
        return Ok(m);
    }
    let mut m = Measurement::new(file, decoder, mode);
    m.error = Some(format!("worker exited with {}: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
    Ok(m)
}

#[derive(Debug, Serialize, Deserialize)]
struct ProbeResult {
    adapter: String,
    stages: Vec<(String, f64)>,
    private_mb: f64,
    working_set_mb: f64,
    error: Option<String>,
}

fn startup(args: StartupArgs) -> Result<()> {
    let exe = std::env::current_exe()?.with_file_name(format!("flick-startup-probe{}", std::env::consts::EXE_SUFFIX));
    if !exe.exists() {
        bail!("{} not found; build it with `cargo build --release -p flick-startup-probe`", exe.display());
    }

    let mut all: Vec<(String, Vec<ProbeResult>)> = Vec::new();
    for backend in &args.backends {
        let mut results = Vec::new();
        for i in 0..args.runs {
            let out = std::env::temp_dir().join(format!("flick-startup-probe-{}-{backend}-{i}.json", std::process::id()));
            let mut probe = Command::new(&exe);
            probe.args(["--backend", backend, "--out"]).arg(&out);
            if let Some(image) = &args.image {
                probe.arg("--image").arg(image);
            }
            if let Some(Size(w, h)) = args.size {
                probe.args(["--size", &format!("{w}x{h}")]);
            }
            let status = probe.status().context("spawning flick-startup-probe")?;
            let text = std::fs::read_to_string(&out).with_context(|| format!("flick-startup-probe exited with {status} and wrote no result"))?;
            std::fs::remove_file(&out).ok();
            results.push(serde_json::from_str::<ProbeResult>(&text)?);
        }
        print_startup(backend, &results);
        all.push((backend.clone(), results));
    }

    if let Some(path) = args.json {
        let json: serde_json::Map<String, serde_json::Value> =
            all.iter().map(|(b, r)| Ok((b.clone(), serde_json::to_value(r)?))).collect::<Result<_>>()?;
        write_json(&path, &json)?;
    }
    Ok(())
}

fn print_startup(backend: &str, results: &[ProbeResult]) {
    println!();
    let failed: Vec<&ProbeResult> = results.iter().filter(|r| r.error.is_some()).collect();
    let ok: Vec<&ProbeResult> = results.iter().filter(|r| r.error.is_none()).collect();
    let Some(first) = ok.first() else {
        println!("{backend}: all {} runs failed: {}", results.len(), failed[0].error.as_deref().unwrap_or("?"));
        return;
    };
    println!("{backend}: {}  ({} runs, ms since process creation)", first.adapter, ok.len());
    println!("  {:<15} {:>9} {:>9} {:>9}", "stage", "1st run", "median", "min");
    for (name, first_ms) in &first.stages {
        let mut values: Vec<f64> =
            ok.iter().filter_map(|r| r.stages.iter().find(|(n, _)| n == name).map(|(_, v)| *v)).collect();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        println!("  {:<15} {:>9.1} {:>9.1} {:>9.1}", name, first_ms, report::median(&mut values), min);
    }
    let mut private: Vec<f64> = ok.iter().map(|r| r.private_mb).collect();
    let mut working: Vec<f64> = ok.iter().map(|r| r.working_set_mb).collect();
    println!("  memory at exit: private {:.1} MB, working set {:.1} MB (medians)", report::median(&mut private), report::median(&mut working));
    if !failed.is_empty() {
        println!("  {} runs failed, first error: {}", failed.len(), failed[0].error.as_deref().unwrap_or("?"));
    }
}
