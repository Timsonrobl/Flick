//! Watching a folder for new, removed, renamed and rewritten files, so
//! paging sees them (ARCHITECTURE.md §12). One watcher per folder, shared by
//! every tab and window paging through it: a thread waiting on Windows'
//! change notifications (`ReadDirectoryChangesW`), which costs nothing while
//! nothing changes. Dropping the watcher stops it.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::HSTRING;

use crate::folder::Change;

/// A handle the watching thread and its owner both use.
#[derive(Clone, Copy)]
struct Shared(HANDLE);
unsafe impl Send for Shared {}

pub struct Watcher {
    stop: Shared,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching `dir`; `report` gets each batch of changes (on the
    /// watcher's thread). None if the folder can't be watched.
    pub fn start(dir: &Path, report: impl Fn(Vec<Change>) + Send + 'static) -> Option<Watcher> {
        unsafe {
            let handle = CreateFileW(
                &HSTRING::from(dir.as_os_str()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
            .ok()?;
            let Ok(stop) = CreateEventW(None, true, false, None) else {
                let _ = CloseHandle(handle);
                return None;
            };
            let (dir, handles, stop) = (dir.to_path_buf(), (Shared(handle), Shared(stop)), Shared(stop));
            let (ready, live) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let (handle, stop) = handles; // whole, so the closure moves the Send wrappers
                watch(&dir, handle.0, stop.0, &report, ready);
                let _ = CloseHandle(handle.0);
            });
            // Changes count from the thread's first read: return once it's
            // made, so nothing done right after this call goes unseen.
            let _ = live.recv();
            Some(Watcher { stop, thread: Some(thread) })
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.stop.0);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = CloseHandle(self.stop.0);
        }
    }
}

/// Files appearing, going, renamed, rewritten (size or date).
const FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(FILE_NOTIFY_CHANGE_FILE_NAME.0 | FILE_NOTIFY_CHANGE_SIZE.0 | FILE_NOTIFY_CHANGE_LAST_WRITE.0);

unsafe fn watch(dir: &Path, handle: HANDLE, stop: HANDLE, report: &dyn Fn(Vec<Change>), ready: std::sync::mpsc::Sender<()>) {
    let Ok(done) = (unsafe { CreateEventW(None, true, false, None) }) else { return };
    // DWORD-aligned, as the notifications need.
    let mut buffer = vec![0u32; 16 * 1024];
    loop {
        let mut overlapped = OVERLAPPED { hEvent: done, ..Default::default() };
        let bytes = (buffer.len() * 4) as u32;
        let started = unsafe { ReadDirectoryChangesW(handle, buffer.as_mut_ptr().cast(), bytes, false, FILTER, None, Some(&mut overlapped), None) };
        let _ = ready.send(()); // the first time: the watch is live (or failed)
        if started.is_err() {
            report(vec![Change::Rescan]); // the folder went away or can't be watched
            break;
        }
        if unsafe { WaitForMultipleObjects(&[done, stop], false, INFINITE) } != WAIT_OBJECT_0 {
            // Stopping: cancel, and wait until Windows is done with the buffer.
            let mut ignored = 0;
            unsafe {
                let _ = CancelIoEx(handle, Some(&overlapped));
                let _ = GetOverlappedResult(handle, &overlapped, &mut ignored, true);
            }
            break;
        }
        let mut written = 0;
        if unsafe { GetOverlappedResult(handle, &overlapped, &mut written, false) }.is_err() {
            report(vec![Change::Rescan]);
            break;
        }
        // Nothing written: more changed than the buffer holds.
        let changes = if written == 0 { vec![Change::Rescan] } else { parse(dir, &buffer, written as usize) };
        if !changes.is_empty() {
            report(changes);
        }
    }
    let _ = unsafe { CloseHandle(done) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn reports_added_renamed_rewritten_and_removed_files() {
        let dir = std::env::temp_dir().join(format!("flick-watcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let watcher = Watcher::start(&dir, move |changes| {
            let _ = send.send(changes);
        })
        .unwrap();
        let file = |n: &str| dir.join(n);
        std::fs::write(file("new.jpg"), b"1").unwrap();
        std::fs::rename(file("new.jpg"), file("renamed.jpg")).unwrap();
        std::fs::write(file("renamed.jpg"), b"22").unwrap();
        std::fs::remove_file(file("renamed.jpg")).unwrap();

        let mut seen = Vec::new();
        while let Ok(changes) = receive.recv_timeout(Duration::from_millis(500)) {
            seen.extend(changes);
        }
        drop(watcher); // stops, without hanging
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.contains(&Change::Added(file("new.jpg"))), "{seen:?}");
        assert!(seen.contains(&Change::Renamed(file("new.jpg"), file("renamed.jpg"))), "{seen:?}");
        assert!(seen.contains(&Change::Modified(file("renamed.jpg"))), "{seen:?}");
        assert!(seen.contains(&Change::Removed(file("renamed.jpg"))), "{seen:?}");
    }
}

/// The FILE_NOTIFY_INFORMATION records in `buffer`, as changes.
fn parse(dir: &Path, buffer: &[u32], written: usize) -> Vec<Change> {
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), written) };
    let mut changes = Vec::new();
    let mut renamed_from: Option<PathBuf> = None;
    let mut at = 0;
    while at + 12 <= bytes.len() {
        let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap_or_default());
        let (next, action, length) = (u32_at(at) as usize, u32_at(at + 4), u32_at(at + 8) as usize);
        let name_bytes = &bytes[(at + 12).min(bytes.len())..(at + 12 + length).min(bytes.len())];
        let name: Vec<u16> = name_bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let name = String::from_utf16_lossy(&name);
        // One name in this folder (a network share's server writes these):
        // never a path that would put another folder's file in the list.
        let plain = !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', ':']);
        let path = dir.join(&name);
        match FILE_ACTION(action) {
            _ if !plain => {}
            FILE_ACTION_ADDED => changes.push(Change::Added(path)),
            FILE_ACTION_REMOVED => changes.push(Change::Removed(path)),
            FILE_ACTION_MODIFIED => changes.push(Change::Modified(path)),
            FILE_ACTION_RENAMED_OLD_NAME => renamed_from = Some(path),
            FILE_ACTION_RENAMED_NEW_NAME => changes.push(match renamed_from.take() {
                Some(old) => Change::Renamed(old, path),
                None => Change::Added(path),
            }),
            _ => {}
        }
        if next == 0 {
            break;
        }
        at += next;
    }
    if let Some(old) = renamed_from {
        changes.push(Change::Removed(old)); // renamed out of this folder
    }
    changes
}
