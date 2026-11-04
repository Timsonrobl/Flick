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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HLOCAL,
    LocalFree,
};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::{HSTRING, PWSTR};

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

/// A path as one field of a request: the characters that separate fields
/// (and `%`) escaped, so a file name with a line break in it (possible on a
/// network share, not on NTFS) can't add lines such as `quit`.
fn escape(path: &std::path::Path) -> String {
    path.display().to_string().replace('%', "%25").replace('\n', "%0A").replace('\r', "%0D").replace('\t', "%09")
}

fn unescape(field: &str) -> PathBuf {
    PathBuf::from(field.replace("%09", "\t").replace("%0D", "\r").replace("%0A", "\n").replace("%25", "%"))
}

impl Request {
    fn encode(&self) -> String {
        let mut text = format!("file\t{}\nbuild\t{}\n", escape(&self.file), self.build);
        if self.quit {
            text += "quit\t1\n";
        }
        if let Some((w, h)) = self.size {
            text += &format!("size\t{w}x{h}\n");
        }
        if let Some(path) = &self.timeline_out {
            text += &format!("timeline\t{}\n", escape(path));
        }
        text
    }

    fn decode(text: &str) -> Option<Request> {
        let mut request = Request::default();
        for line in text.lines() {
            match line.split_once('\t')? {
                ("file", v) => request.file = unescape(v),
                ("build", v) => request.build = v.parse().unwrap_or(0),
                ("size", v) => request.size = v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))),
                ("timeline", v) => request.timeline_out = Some(unescape(v)),
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
/// This resident has begun quitting (`closing`).
static CLOSING: AtomicBool = AtomicBool::new(false);
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
        // An elevated resident owns it; `send` explains.
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
        // Identification only: the server may learn who we are, never act as us.
        let flags = SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
        let opened = unsafe { CreateFileW(&name, GENERIC_READ.0 | GENERIC_WRITE.0, FILE_SHARE_NONE, None, OPEN_EXISTING, flags, None) };
        if let Err(e) = &opened {
            if last_error != Some(e.code()) {
                trace(format_args!("send: +{:.0} ms open failed: {e}", started.elapsed().as_secs_f64() * 1000.0));
                last_error = Some(e.code());
            }
        }
        match opened {
            Ok(pipe) => break pipe,
            // Only a resident running as administrator refuses us (one that
            // couldn't start unelevated; see elevation.rs). Waiting won't
            // change that.
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
        // Flick's pipe name is guessable: another account's program could
        // take it first. Nothing goes to a server that isn't this user's.
        if GetNamedPipeServerProcessId(pipe, &mut server).is_err() || !same_user(server) {
            let _ = CloseHandle(pipe);
            return Err("Flick's pipe is held by another user's program; Flick can't hand files to itself.".into());
        }
        let _ = AllowSetForegroundWindow(server);
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

/// This resident is quitting: a file asked for from now on is opened by
/// its launcher, which takes over (as from an older build), not taken here
/// and lost when the process ends.
pub fn closing() {
    CLOSING.store(true, Ordering::SeqCst);
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
        let security = owner_only();
        loop {
            // Only this user may connect, never from another computer, and
            // the name must be free (not another account's pipe to join).
            let pipe = unsafe {
                CreateNamedPipeW(
                    &name,
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    64 * 1024,
                    0,
                    security.as_ref().map(|s| &raw const s.0),
                )
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
            // Quitting already, any launcher takes over.
            let newer = request.as_ref().is_some_and(|r| !r.quit && (r.build > build_id() || CLOSING.load(Ordering::SeqCst)));
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

/// A request is a few lines; anything longer isn't one.
const MESSAGE_MAX: usize = 64 * 1024;

fn read_message(pipe: HANDLE) -> Option<String> {
    let mut message = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut read = 0;
        let result = unsafe { ReadFile(pipe, Some(&mut chunk), Some(&mut read), None) };
        message.extend_from_slice(&chunk[..read as usize]);
        if message.len() > MESSAGE_MAX {
            return None;
        }
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

/// A security descriptor for the pipe: this user (by SID) only, protected
/// from inheritance. None if it can't be made (the default then applies).
struct Security(SECURITY_ATTRIBUTES);

// The descriptor is made once and never freed (it lives with the listener).
unsafe impl Send for Security {}

fn owner_only() -> Option<Security> {
    unsafe {
        let user = token_user(GetCurrentProcess())?;
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user_sid(&user), &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        let sddl = HSTRING::from(format!("D:P(A;;GA;;;{})", sid?));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut descriptor, None).ok()?;
        Some(Security(SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: descriptor.0, bInheritHandle: false.into() }))
    }
}

/// A process's TOKEN_USER (in u64s, for its alignment).
fn token_user(process: HANDLE) -> Option<Vec<u64>> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut len = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
        let read = GetTokenInformation(token, TokenUser, Some(buffer.as_mut_ptr().cast()), len, &mut len);
        let _ = CloseHandle(token);
        read.ok()?;
        Some(buffer)
    }
}

fn user_sid(token_user: &[u64]) -> PSID {
    // SAFETY: the buffer holds a TOKEN_USER (from `token_user`).
    unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
}

/// Whether process `pid` runs as this process's user.
fn same_user(pid: u32) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let theirs = token_user(process);
        let _ = CloseHandle(process);
        match (theirs, token_user(GetCurrentProcess())) {
            (Some(theirs), Some(ours)) => EqualSid(user_sid(&theirs), user_sid(&ours)).is_ok(),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file name with line breaks (possible on a network share) stays one
    /// field: it can't add a `quit` or `timeline` line.
    #[test]
    fn requests_keep_odd_names_in_their_field() {
        let file = PathBuf::from(r"\\server\share\a".to_string() + "\nquit\t1\ntimeline\t" + r"C:\x 100%.jpg");
        let request = Request { file: file.clone(), build: 7, ..Request::default() };
        let back = Request::decode(&request.encode()).unwrap();
        assert_eq!(back.file, file);
        assert!(!back.quit && back.timeline_out.is_none());
    }
}
