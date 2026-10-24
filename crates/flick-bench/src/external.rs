//! End-to-end timing of an external viewer (Honeyview, oculante, ...) by
//! watching the screen, since closed-source viewers can't be instrumented.
//!
//! Each run launches the viewer on one file and samples the centre half of its
//! window through the screen DC. Times are milliseconds since the viewer's
//! process was created, the same clock `flick-startup-probe` uses:
//!
//! - `window`: its first visible, unowned top-level window appears.
//! - `image`: the first sample that already looks like the final picture
//!   (a preview counts). Before the viewer paints, the screen DC still shows
//!   whatever was behind the window, so "not flat" is not enough.
//! - `final`: the last change before the region holds still, showing an
//!   image, for `--settle-ms` (e.g. a preview replaced by the full decode).
//!
//! Sampling resolution is the capture cost, reported as `capture ms`.

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{FILETIME, HANDLE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::GetProcessTimes;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::BOOL;

use crate::report::median;

/// Side of the square the sampled region is shrunk to.
const SAMPLE: i32 = 128;
/// Luma standard deviation above which the region counts as an image.
const CONTENT_STDDEV: f64 = 4.0;
/// Mean absolute luma difference between samples that counts as a change.
const CHANGE: f64 = 0.5;
/// Mean absolute luma difference under which a sample shows the final picture.
const SIMILAR: f64 = 16.0;

pub struct Options {
    pub exe: PathBuf,
    /// Whitespace-separated arguments with `{file}` as the image placeholder.
    pub args: Option<String>,
    pub runs: usize,
    pub page: usize,
    pub page_key: VIRTUAL_KEY,
    pub settle: Duration,
    pub timeout: Duration,
    /// Save every distinct sample as a PNG here, to see what each change was.
    pub dump: Option<PathBuf>,
    /// Warm opens: start the viewer once (it stays resident), then time
    /// launches that hand their file to it.
    pub resident: bool,
    /// Wait this long between warm opens (e.g. past the resident's memory trim).
    pub idle: Duration,
}

pub fn parse_key(name: &str) -> Result<VIRTUAL_KEY> {
    Ok(match name {
        "right" => VK_RIGHT,
        "pagedown" => VK_NEXT,
        "space" => VK_SPACE,
        other => bail!("unknown key {other} (right, pagedown, space)"),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Launch {
    pub file: PathBuf,
    pub window_ms: f64,
    pub image_ms: f64,
    pub final_ms: f64,
    /// Times (ms) when, after the picture appeared, the window showed something else.
    #[serde(default)]
    pub glitch_ms: Vec<f64>,
    /// How long the window showed a blank frame before the picture.
    #[serde(default)]
    pub blank_ms: f64,
    pub window_size: (i32, i32),
    /// After the image settled.
    pub private_mb: f64,
    pub working_set_mb: f64,
    /// Peak private commit over the viewer's lifetime so far.
    pub peak_private_mb: f64,
    pub capture_ms: f64,
    /// Page turns after the image settled.
    pub pages: Vec<Page>,
    /// A failed page turn (e.g. past the last file) keeps the open timings.
    pub page_error: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Page {
    /// ms from key press to the next picture, and to its final frame.
    pub image_ms: f64,
    pub final_ms: f64,
    /// Viewer memory once the new picture settled; shows growth while paging.
    pub private_mb: f64,
    pub working_set_mb: f64,
    /// Wall-clock ms when the key was sent (system-wide clock, comparable
    /// with timestamps a viewer logs itself).
    #[serde(default)]
    pub pressed_wall: f64,
}

pub fn run(files: &[PathBuf], opts: &Options) -> Result<Vec<Launch>> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.ok();
    let exe_name = opts.exe.file_name().context("viewer path has no file name")?;
    if let Some(pid) = running_instance(exe_name) {
        bail!("{} is already running (pid {pid}); close it first so launches are cold", exe_name.to_string_lossy());
    }

    let capturer = Capturer::new()?;
    let mut all = Vec::new();
    if opts.resident {
        let first = files.first().context("no files")?;
        let mut resident = spawn(opts, first)?;
        // The start that makes it resident is itself a cold open; then its
        // window is closed and the process keeps running.
        let mut start = Launch { file: first.clone(), ..Default::default() };
        let resident_handle = HANDLE(resident.as_raw_handle());
        let watch = Watch { timing: resident_handle, window_pid: resident.id(), memory: resident_handle };
        if let Err(e) = observe(&watch, Some(&mut resident), opts, 0, &capturer, &mut start) {
            let _ = resident.kill();
            bail!("starting the resident viewer: {e:#}");
        }
        eprintln!("  resident start (cold): {}", describe(&start));
        close_window(resident.id());
        for file in files {
            for run in 0..opts.runs {
                std::thread::sleep(opts.idle.max(Duration::from_millis(500)));
                let page = if run == 0 { opts.page } else { 0 };
                let mut launch = Launch { file: file.clone(), ..Default::default() };
                match spawn(opts, file) {
                    Ok(mut launcher) => {
                        // t = 0 is the launcher's creation; the window and its
                        // memory belong to the resident process.
                        let watch = Watch { timing: HANDLE(launcher.as_raw_handle()), window_pid: resident.id(), memory: resident_handle };
                        if let Err(e) = observe(&watch, None, opts, page, &capturer, &mut launch) {
                            launch.error = Some(format!("{e:#}"));
                        }
                        close_window(resident.id());
                        let _ = launcher.wait();
                    }
                    Err(e) => launch.error = Some(format!("{e:#}")),
                }
                eprintln!("  {}", describe(&launch));
                all.push(launch);
            }
        }
        let _ = resident.kill();
        let _ = resident.wait();
        return Ok(all);
    }
    for file in files {
        for run in 0..opts.runs {
            // Page only once per file; it's the slow part and needs focus.
            let page = if run == 0 { opts.page } else { 0 };
            let launch = launch_once(file, opts, page, &capturer);
            eprintln!("  {}", describe(&launch));
            all.push(launch);
        }
    }
    Ok(all)
}

fn spawn(opts: &Options, file: &Path) -> Result<Child> {
    let mut command = Command::new(&opts.exe);
    match &opts.args {
        Some(template) => {
            for arg in template.split_whitespace() {
                if arg == "{file}" { command.arg(file) } else { command.arg(arg) };
            }
        }
        None => {
            command.arg(file);
        }
    }
    command.spawn().with_context(|| format!("spawning {}", opts.exe.display()))
}

fn launch_once(file: &Path, opts: &Options, page: usize, capturer: &Capturer) -> Launch {
    let mut launch = Launch { file: file.to_path_buf(), ..Default::default() };
    let mut child = match spawn(opts, file) {
        Ok(c) => c,
        Err(e) => {
            launch.error = Some(format!("{e:#}"));
            return launch;
        }
    };
    let handle = HANDLE(child.as_raw_handle());
    let watch = Watch { timing: handle, window_pid: child.id(), memory: handle };
    if let Err(e) = observe(&watch, Some(&mut child), opts, page, capturer, &mut launch) {
        launch.error = Some(format!("{e:#}"));
    }
    close(&mut child);
    launch
}

/// Which process's creation is t = 0, whose window to watch, and whose
/// memory to read (all the same process, except for warm opens).
struct Watch {
    timing: HANDLE,
    window_pid: u32,
    memory: HANDLE,
}

/// Posts WM_CLOSE to `pid`'s window and waits for it to go (not for the process).
fn close_window(pid: u32) {
    if let Some(hwnd) = find_window(pid) {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while find_window(pid).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn observe(watch: &Watch, mut child: Option<&mut Child>, opts: &Options, page: usize, capturer: &Capturer, launch: &mut Launch) -> Result<()> {
    let (process, pid) = (watch.timing, watch.window_pid);
    let deadline = Instant::now() + opts.timeout;

    let hwnd = loop {
        if let Some(hwnd) = find_window(pid) {
            launch.window_ms = ms_since_creation(process);
            break hwnd;
        }
        if let Some(child) = child.as_deref_mut()
            && let Some(status) = child.try_wait()?
        {
            bail!("viewer exited with {status} before showing a window (single-instance hand-off?)");
        }
        if Instant::now() > deadline {
            bail!("no window within {:?}", opts.timeout);
        }
        std::thread::sleep(Duration::from_millis(1));
    };

    let mut captures = Vec::new();
    let stem = launch.file.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let dump = |phase: &str| opts.dump.as_ref().map(|dir| (dir.clone(), format!("{stem}-pid{pid}-{phase}")));
    let settled = wait_until_settled(hwnd, capturer, opts, None, dump("open"), &mut captures, || ms_since_creation(process))?;
    launch.window_size = settled.size;
    launch.image_ms = settled.image_ms;
    launch.final_ms = settled.final_ms;
    launch.glitch_ms = settled.glitch_ms;
    launch.blank_ms = settled.blank_ms;

    (launch.private_mb, launch.working_set_mb, launch.peak_private_mb) = memory(watch.memory)?;

    let mut previous = settled.last;
    for i in 0..page {
        let pressed = Instant::now();
        let pressed_wall = wall_ms();
        press(hwnd, opts.page_key);
        let since_press = || pressed.elapsed().as_secs_f64() * 1000.0;
        match wait_until_settled(hwnd, capturer, opts, Some(&previous), dump(&format!("page{}", i + 1)), &mut captures, since_press) {
            Ok(turn) => {
                let (private_mb, working_set_mb, _) = memory(watch.memory)?;
                launch.pages.push(Page { image_ms: turn.image_ms, final_ms: turn.final_ms, private_mb, working_set_mb, pressed_wall });
                previous = turn.last;
            }
            Err(e) => {
                launch.page_error = Some(format!("page turn {}: {e:#}", i + 1));
                break;
            }
        }
    }
    launch.capture_ms = median(&mut captures);
    Ok(())
}

struct Settled {
    size: (i32, i32),
    image_ms: f64,
    final_ms: f64,
    /// Samples after `image_ms` that did not look like the final picture.
    glitch_ms: Vec<f64>,
    /// How long a flat (blank) frame was on screen before the picture.
    blank_ms: f64,
    last: Vec<u8>,
}

/// Samples the window until its centre holds still for `opts.settle` with an
/// image on it. `baseline`, if given, is the frame before a page turn.
fn wait_until_settled(
    hwnd: HWND,
    capturer: &Capturer,
    opts: &Options,
    baseline: Option<&[u8]>,
    dump: Option<(PathBuf, String)>,
    captures: &mut Vec<f64>,
    clock: impl Fn() -> f64,
) -> Result<Settled> {
    if let Some((dir, _)) = &dump {
        std::fs::create_dir_all(dir)?;
    }
    let deadline = Instant::now() + opts.timeout;
    let settle_ms = opts.settle.as_secs_f64() * 1000.0;
    // Every distinct sample since the start (or since the key press).
    let mut timeline: Vec<(f64, Vec<u8>)> = Vec::new();

    loop {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect) }.context("window went away")?;
        let size = (rect.right - rect.left, rect.bottom - rect.top);

        let t0 = Instant::now();
        let sample = capturer.capture(&rect)?;
        let now = clock();
        captures.push(t0.elapsed().as_secs_f64() * 1000.0);

        let prev = timeline.last().map(|(_, s)| s.as_slice()).or(baseline);
        if prev.is_none_or(|p| mean_abs_diff(p, &sample) > CHANGE) {
            if let Some((dir, prefix)) = &dump {
                let path = dir.join(format!("{prefix}-{now:06.0}ms.png"));
                image::GrayImage::from_raw(SAMPLE as u32, SAMPLE as u32, sample.clone()).context("sample size")?.save(&path)?;
            }
            timeline.push((now, sample));
        }

        // A page turn must change something; the open must end on an image.
        if let Some((last_change, last)) = timeline.last()
            && stddev(last) > CONTENT_STDDEV
            && now - last_change >= settle_ms
        {
            let image_ms = timeline.iter().find(|(_, s)| mean_abs_diff(s, last) < SIMILAR).map_or(*last_change, |(t, _)| *t);
            // Once the picture is up, anything else on screen is a flicker
            // (a black frame, a blank, a stale picture).
            let glitch_ms = timeline.iter().filter(|(t, s)| *t > image_ms && mean_abs_diff(s, last) >= SIMILAR).map(|(t, _)| *t).collect();
            // A window shown before its first draw lands composes as a flat
            // (black or white) frame; the desktop behind it rarely is flat.
            let blank_ms = timeline.iter().find(|(t, s)| *t < image_ms && stddev(s) <= CONTENT_STDDEV).map_or(0.0, |(t, _)| image_ms - t);
            let (final_ms, last) = timeline.pop().unwrap_or_default();
            return Ok(Settled { size, image_ms, final_ms, glitch_ms, blank_ms, last });
        }
        if Instant::now() > deadline {
            bail!("image did not settle within {:?} ({} changes seen)", opts.timeout, timeline.len());
        }
    }
}

fn stddev(luma: &[u8]) -> f64 {
    let n = luma.len() as f64;
    let mean = luma.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    (luma.iter().map(|&v| (f64::from(v) - mean).powi(2)).sum::<f64>() / n).sqrt()
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(&x, &y)| f64::from(x.abs_diff(y))).sum::<f64>() / a.len() as f64
}

/// Reads the centre half of a screen rectangle, shrunk to SAMPLE×SAMPLE luma.
struct Capturer {
    screen: HDC,
    mem: HDC,
    bits: *const u8,
}

impl Capturer {
    fn new() -> Result<Self> {
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: SAMPLE,
                    biHeight: -SAMPLE, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            SelectObject(mem, bitmap.into());
            SetStretchBltMode(mem, COLORONCOLOR);
            Ok(Self { screen, mem, bits: bits as *const u8 })
        }
    }

    fn capture(&self, rect: &RECT) -> Result<Vec<u8>> {
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        unsafe {
            if !StretchBlt(self.mem, 0, 0, SAMPLE, SAMPLE, Some(self.screen), rect.left + w / 4, rect.top + h / 4, w / 2, h / 2, SRCCOPY).as_bool() {
                bail!("StretchBlt from the screen failed");
            }
            let _ = GdiFlush();
            let bgra = std::slice::from_raw_parts(self.bits, (SAMPLE * SAMPLE * 4) as usize);
            Ok(bgra.chunks_exact(4).map(|p| ((u32::from(p[0]) * 29 + u32::from(p[1]) * 150 + u32::from(p[2]) * 77) >> 8) as u8).collect())
        }
    }
}

struct Search {
    pid: u32,
    found: Option<HWND>,
}

unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != search.pid || !unsafe { IsWindowVisible(hwnd) }.as_bool() || unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid()) {
        return true.into();
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() && rect.right - rect.left >= 200 && rect.bottom - rect.top >= 200 {
        search.found = Some(hwnd);
        return false.into();
    }
    true.into()
}

fn find_window(pid: u32) -> Option<HWND> {
    let mut search = Search { pid, found: None };
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize)) };
    search.found
}

fn running_instance(exe_name: &std::ffi::OsStr) -> Option<u32> {
    let name = exe_name.to_string_lossy().to_lowercase();
    let output = Command::new("tasklist").args(["/FO", "CSV", "/NH"]).output().ok()?;
    String::from_utf8_lossy(&output.stdout).lines().find_map(|line| {
        let mut cols = line.split("\",\"").map(|c| c.trim_matches('"'));
        let image = cols.next()?.to_lowercase();
        let pid = cols.next()?.parse().ok()?;
        (image == name).then_some(pid)
    })
}

fn press(hwnd: HWND, key: VIRTUAL_KEY) {
    let input = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: key, dwFlags: flags, ..Default::default() } },
    };
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        SendInput(&[input(KEYBD_EVENT_FLAGS(0)), input(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
    }
}

fn close(child: &mut Child) {
    if let Some(hwnd) = find_window(child.id()) {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn wall_ms() -> f64 {
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as f64 / 10_000.0
}

fn ms_since_creation(process: HANDLE) -> f64 {
    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe {
        if GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user).is_err() {
            return f64::NAN;
        }
        ticks(GetSystemTimePreciseAsFileTime()).saturating_sub(ticks(created)) as f64 / 10_000.0
    }
}

fn memory(process: HANDLE) -> Result<(f64, f64, f64)> {
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe { GetProcessMemoryInfo(process, &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) }?;
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    Ok((mb(c.PrivateUsage), mb(c.WorkingSetSize), mb(c.PeakPagefileUsage)))
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

pub fn describe(l: &Launch) -> String {
    if let Some(e) = &l.error {
        return format!("{}: ERROR {e}", file_name(&l.file));
    }
    let pages: Vec<String> = l.pages.iter().map(|p| format!("{:.0}/{:.0}", p.image_ms, p.final_ms)).collect();
    let glitches: Vec<String> = l.glitch_ms.iter().map(|t| format!("{t:.0}")).collect();
    format!(
        "{}: window {:.0} ms, blank {:.0} ms, image {:.0} ms, final {:.0} ms, glitches {}, {}x{}, private {:.0} MB (peak {:.0}), ws {:.0} MB, capture {:.1} ms{}",
        file_name(&l.file),
        l.window_ms,
        l.blank_ms,
        l.image_ms,
        l.final_ms,
        if glitches.is_empty() { "none".to_string() } else { format!("at {} ms", glitches.join(",")) },
        l.window_size.0,
        l.window_size.1,
        l.private_mb,
        l.peak_private_mb,
        l.working_set_mb,
        l.capture_ms,
        if pages.is_empty() { String::new() } else { format!(", page turns image/final ms: {}", pages.join(" ")) },
    ) + &l.page_error.as_ref().map_or_else(String::new, |e| format!(" ({e})"))
}

pub fn print(all: &[Launch]) {
    let mut files: Vec<&Path> = Vec::new();
    for l in all {
        if !files.contains(&l.file.as_path()) {
            files.push(&l.file);
        }
    }
    println!();
    println!("Per file (ms since viewer process creation; medians over successful runs, 1st run in brackets)");
    println!("{:<34} {:>13} {:>13} {:>13} {:>10} {:>10} {:>8}", "file", "window", "image", "final", "private MB", "peak MB", "ws MB");
    for f in files {
        let ok: Vec<&Launch> = all.iter().filter(|l| l.file == f && l.error.is_none()).collect();
        let Some(first) = ok.first() else {
            println!("{:<34} all runs failed", file_name(f));
            continue;
        };
        let col = |get: fn(&Launch) -> f64| {
            let mut v: Vec<f64> = ok.iter().map(|l| get(l)).collect();
            format!("{:.0} [{:.0}]", median(&mut v), get(first))
        };
        let med = |get: fn(&Launch) -> f64| {
            let mut v: Vec<f64> = ok.iter().map(|l| get(l)).collect();
            median(&mut v)
        };
        println!(
            "{:<34} {:>13} {:>13} {:>13} {:>10.0} {:>10.0} {:>8.0}",
            file_name(f),
            col(|l| l.window_ms),
            col(|l| l.image_ms),
            col(|l| l.final_ms),
            med(|l| l.private_mb),
            med(|l| l.peak_private_mb),
            med(|l| l.working_set_mb),
        );
        for (i, p) in first.pages.iter().enumerate() {
            println!(
                "  page turn {:>2}: image {:>4.0} ms, final {:>4.0} ms after key press; private {:.0} MB, ws {:.0} MB",
                i + 1,
                p.image_ms,
                p.final_ms,
                p.private_mb,
                p.working_set_mb
            );
        }
    }
    let ok: Vec<&Launch> = all.iter().filter(|l| l.error.is_none()).collect();
    let flickered = ok.iter().filter(|l| !l.glitch_ms.is_empty()).count();
    println!("flicker after the picture appeared: {flickered} of {} launches", ok.len());
    let blank: Vec<f64> = ok.iter().map(|l| l.blank_ms).filter(|&b| b > 0.0).collect();
    let mut blank_sorted = blank.clone();
    println!("blank window before the picture: {} of {} launches (median {:.0} ms when it happened)", blank.len(), ok.len(), median(&mut blank_sorted));
    let mut capture: Vec<f64> = ok.iter().map(|l| l.capture_ms).collect();
    println!("screen sampling cost: {:.1} ms per sample (timing resolution)", median(&mut capture));
    let failed = all.iter().filter(|l| l.error.is_some()).count();
    if failed > 0 {
        println!("{failed} runs failed; see the progress lines above");
    }
}
