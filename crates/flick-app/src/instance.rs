//! Single instance (ARCHITECTURE.md §3a): one resident Flick per user
//! session. A later launch hands its request to the resident process over a
//! named pipe and exits, so a warm open costs that process only its own
//! start (~15 ms) plus the resident's decode.
//!
//! Every request carries the launcher's build (its executable's timestamp).
//! A resident instance older than the launcher steps down: it releases the
//! instance mutex, stops listening and tells the launcher to take over, so a
//! freshly built Flick is never served by a stale resident process.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::HSTRING;

use crate::clock::trace;

/// What a launch asks the resident process to open.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub file: PathBuf,
    pub size: Option<(u32, u32)>,
    pub timeline_out: Option<PathBuf>,
    /// The launcher's build, see [`build_id`].
    pub build: u64,
    /// Not a file to open: quit (`flick --quit`, see [`quit`]).
    pub quit: bool,
}

impl Request {
    fn encode(&self) -> String {
        let mut text = format!("file\t{}\nbuild\t{}\n", self.file.display(), self.build);
        if self.quit {
            text += "quit\t1\n";
        }
        if let Some((w, h)) = self.size {
            text += &format!("size\t{w}x{h}\n");
        }
        if let Some(path) = &self.timeline_out {
            text += &format!("timeline\t{}\n", path.display());
        }
        text
    }

    fn decode(text: &str) -> Option<Request> {
        let mut request = Request::default();
        for line in text.lines() {
            match line.split_once('\t')? {
                ("file", v) => request.file = PathBuf::from(v),
                ("build", v) => request.build = v.parse().unwrap_or(0),
                ("size", v) => request.size = v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))),
                ("timeline", v) => request.timeline_out = Some(PathBuf::from(v)),
                ("quit", _) => request.quit = true,
                _ => {}
            }
        }
        (request.quit || !request.file.as_os_str().is_empty()).then_some(request)
    }
}

/// How a hand-off went.
pub enum Handoff {
    /// The resident process opens the file; this launch can exit.
    Delivered,
    /// The resident process was an older build and stepped down: this launch
    /// now holds the instance and must open the file itself.
    BecomeResident,
}

const OK: &[u8] = b"ok";
const OUTDATED: &[u8] = b"outdated";

static BUILD: OnceLock<u64> = OnceLock::new();
/// The instance mutex's handle (0: none), held while this process is the
/// resident one.
static MUTEX: AtomicUsize = AtomicUsize::new(0);

/// This build's identity: the executable's last-write time, read once at
/// startup (before a rebuild could rename the running file).
pub fn build_id() -> u64 {
    *BUILD.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::metadata)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as u64)
    })
}

fn session_name(prefix: &str) -> String {
    let mut session = 0;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric()).collect();
    format!("{prefix}Flick-{session}-{user}")
}

fn pipe_name() -> HSTRING {
    HSTRING::from(session_name(r"\\.\pipe\"))
}

/// True if this process became the resident instance; false if one already
/// runs in this session.
pub fn claim() -> bool {
    match unsafe { CreateMutexW(None, false, &HSTRING::from(session_name(r"Local\"))) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                let _ = unsafe { CloseHandle(handle) };
                return false;
            }
            MUTEX.store(handle.0 as usize, Ordering::SeqCst);
            true
        }
        // An elevated resident (an older build) owns it; `send` explains.
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => false,
        Err(_) => true,
    }
}

pub fn release() {
    let handle = MUTEX.swap(0, Ordering::SeqCst);
    if handle != 0 {
        let _ = unsafe { CloseHandle(HANDLE(handle as *mut _)) };
    }
}

/// Hands `request` to the resident process. Lets it take the foreground,
/// since this process (just launched by the user) is allowed to.
pub fn send(request: &Request) -> Result<Handoff, String> {
    let name = pipe_name();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(3);
    trace(format_args!("send: build {}", request.build));
    let mut last_error = None;
    let pipe = loop {
        let opened = unsafe { CreateFileW(&name, GENERIC_READ.0 | GENERIC_WRITE.0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None) };
        if let Err(e) = &opened {
            if last_error != Some(e.code()) {
                trace(format_args!("send: +{:.0} ms open failed: {e}", started.elapsed().as_secs_f64() * 1000.0));
                last_error = Some(e.code());
            }
        }
        match opened {
            Ok(pipe) => break pipe,
            // Only a resident running as administrator refuses us (older
            // builds became resident elevated; see elevation.rs). Waiting
            // won't change that.
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                return Err("A Flick started as administrator is running; quit it from its tray icon.".into());
            }
            Err(e) if Instant::now() > deadline => return Err(format!("resident Flick did not answer: {e}")),
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                let _ = unsafe { WaitNamedPipeW(&name, 500) };
            }
            // The resident instance is still starting its listener.
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let reply = unsafe {
        let mut server = 0;
        if GetNamedPipeServerProcessId(pipe, &mut server).is_ok() {
            let _ = AllowSetForegroundWindow(server);
        }
        let written = WriteFile(pipe, Some(request.encode().as_bytes()), None, None);
        let _ = FlushFileBuffers(pipe);
        let reply = written.ok().and_then(|()| read_message(pipe));
        let _ = CloseHandle(pipe);
        trace(format_args!("send: +{:.0} ms connected to {server}, reply {reply:?}", started.elapsed().as_secs_f64() * 1000.0));
        reply
    };
    match reply.as_deref().map(str::as_bytes) {
        Some(OK) => Ok(Handoff::Delivered),
        Some(OUTDATED) => {
            // It has released the instance mutex already; take it over.
            while Instant::now() < deadline {
                if claim() {
                    return Ok(Handoff::BecomeResident);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err("the older resident Flick did not step down".into())
        }
        _ => Err("resident Flick did not confirm the request".into()),
    }
}

/// Asks the resident Flick to quit (`flick --quit`: the installer, before it
/// replaces or removes flick.exe) and waits up to `wait` for its process to
/// end. True once no resident Flick runs (or none did).
pub fn quit(wait: Duration) -> bool {
    let gone = || {
        let free = claim();
        release();
        free
    };
    if gone() {
        return true;
    }
    if let Err(e) = send(&Request { quit: true, build: build_id(), ..Request::default() }) {
        trace(format_args!("quit: {e}"));
        return false;
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if gone() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Listens for requests on a background thread. Stops, after calling
/// `on_superseded`, when a newer build asks: that launch takes over.
pub fn serve(on_request: impl Fn(Request) + Send + 'static, on_superseded: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        let name = pipe_name();
        loop {
            let pipe = unsafe {
                CreateNamedPipeW(&name, PIPE_ACCESS_DUPLEX, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT, PIPE_UNLIMITED_INSTANCES, 4096, 64 * 1024, 0, None)
            };
            if pipe.is_invalid() {
                let error = windows::core::Error::from_thread();
                trace(format_args!("serve: create failed: {error}"));
                if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
                    return; // the name belongs to an elevated process: never ours
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            let connected = unsafe { ConnectNamedPipe(pipe, None) }.is_ok() || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            let request = if connected { read_message(pipe).as_deref().and_then(Request::decode) } else { None };
            // A quit is obeyed by any build: the installer's may be newer.
            let newer = request.as_ref().is_some_and(|r| !r.quit && r.build > build_id());
            trace(format_args!("serve: connected {connected}, request build {:?} (mine {}), newer {newer}", request.as_ref().map(|r| r.build), build_id()));
            if newer {
                // Release first, so the launcher can claim as soon as it reads the reply.
                release();
            }
            unsafe {
                let _ = WriteFile(pipe, Some(if newer { OUTDATED } else { OK }), None, None);
                let _ = FlushFileBuffers(pipe);
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
            }
            match request {
                Some(_) if newer => {
                    on_superseded();
                    return;
                }
                Some(request) => on_request(request),
                None => {}
            }
        }
    });
}

fn read_message(pipe: HANDLE) -> Option<String> {
    let mut message = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut read = 0;
        let result = unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut read), None) };
        message.extend_from_slice(&chunk[..read as usize]);
        match result {
            Ok(()) => break,
            // The message continues; anything else ends it.
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => continue,
            Err(_) if !message.is_empty() => break,
            Err(_) => return None,
        }
    }
    String::from_utf8(message).ok()
}
