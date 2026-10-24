//! Process-relative time and memory, for the open timeline (ARCHITECTURE.md §14).

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

/// Milliseconds since this process was created (DLL loading included).
pub fn ms_since_start() -> f64 {
    let ticks = |ft: FILETIME| (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user).is_err() {
            return f64::NAN;
        }
        ticks(GetSystemTimePreciseAsFileTime()).saturating_sub(ticks(created)) as f64 / 10_000.0
    }
}

/// Wall-clock milliseconds (system-wide, so comparable across processes).
pub fn wall_ms() -> f64 {
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as f64 / 10_000.0
}

/// (private commit, working set) in MB.
pub fn memory_mb() -> (f64, f64) {
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) };
    if ok.is_err() {
        return (f64::NAN, f64::NAN);
    }
    let mb = |b: usize| b as f64 / (1024.0 * 1024.0);
    (mb(c.PrivateUsage), mb(c.WorkingSetSize))
}

/// Diagnostics: appends a line to the file named by `FLICK_TRACE`, if set.
pub fn trace(args: std::fmt::Arguments) {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("FLICK_TRACE")?;
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(file) = file {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{:>10.1} {args}", ms_since_start());
        }
    }
}

/// Named points on the open path, in milliseconds since process start.
#[derive(Default)]
pub struct Timeline {
    pub stages: Vec<(&'static str, f64)>,
}

impl Timeline {
    pub fn mark(&mut self, stage: &'static str) {
        self.stages.push((stage, ms_since_start()));
    }

    pub fn push(&mut self, stage: &'static str, at: f64) {
        self.stages.push((stage, at));
        self.stages.sort_by(|a, b| a.1.total_cmp(&b.1));
    }
}
