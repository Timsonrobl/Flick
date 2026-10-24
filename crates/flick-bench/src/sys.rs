//! Process memory counters and display size. Windows only; other platforms
//! report `None` and the bench falls back to defaults.

#[derive(Clone, Copy, Debug)]
pub struct Memory {
    /// Private commit (Task Manager "Commit size").
    pub private: u64,
    /// Peak private commit over the process lifetime.
    pub peak_private: u64,
}

#[cfg(windows)]
pub fn memory() -> Option<Memory> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) }.ok()?;
    Some(Memory { private: c.PrivateUsage as u64, peak_private: c.PeakPagefileUsage as u64 })
}

#[cfg(not(windows))]
pub fn memory() -> Option<Memory> {
    None
}

/// Physical resolution of the primary display.
#[cfg(windows)]
pub fn primary_display() -> Option<(u32, u32)> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};

    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool().then_some((mode.dmPelsWidth, mode.dmPelsHeight))
}

#[cfg(not(windows))]
pub fn primary_display() -> Option<(u32, u32)> {
    None
}
