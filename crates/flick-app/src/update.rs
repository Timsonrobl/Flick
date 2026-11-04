//! Updates, Chrome-style (ARCHITECTURE.md §17). An installed Flick looks
//! for a newer release at start and then daily (Preferences ▸ Update
//! Automatically), downloads it in the background and keeps it ready beside
//! itself, in `update\`; the tray icon, its menu, the right-click menu and a
//! notification then offer "Relaunch to Update". Unanswered, the update is
//! put in place when Flick quits, or at the next start; a newer release
//! replaces one waiting.
//!
//! Trust: the manifest (`update.txt`, made by `tools\release.ps1`) must be
//! signed (ECDSA P-256) by the key in `update.key`, the exe must match its
//! size and SHA-256, and only a version newer than this build is taken.
//! Flick's own download carries no Mark of the Web, so Windows never warns
//! about it as it does about a browser-downloaded installer.
//!
//! Only an installed copy updates (the uninstall entry's InstallLocation is
//! the exe's folder): a dev build never replaces itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::{
    BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALG_HANDLE, BCRYPT_ECDSA_PUBLIC_P256_MAGIC, BCRYPT_FLAGS, BCRYPT_KEY_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptDestroyKey,
    BCryptHash, BCryptImportKeyPair, BCryptVerifySignature,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::register;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The newest release's manifest (`$env:FLICK_UPDATE_URL` instead, to test:
/// then it's looked at on every start).
const MANIFEST_URL: &str = "https://github.com/Timsonrobl/Flick/releases/latest/download/update.txt";
/// The release key's public half, X then Y (`tools\update-key.ps1`).
const PUBLIC_KEY: &str = include_str!("update.key");
/// The installer's uninstall entry (installer\flick.iss's AppId).
const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{67D47C8A-C813-4E72-9C48-BA9EDC68B2A1}_is1";
/// A look this long after the last.
const EVERY: Duration = Duration::from_secs(20 * 3600);

/// A release, as its signed manifest describes it.
#[derive(Debug, PartialEq)]
struct Manifest {
    version: String,
    url: String,
    size: u64,
    sha256: [u8; 32],
}

/// This exe's folder, if it is the installed copy.
fn installed() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let location = register::read(UNINSTALL, "InstallLocation")?;
    let norm = |p: &str| p.trim_end_matches('\\').to_lowercase();
    (norm(&dir.to_string_lossy()) == norm(&location)).then(|| dir.to_path_buf())
}

/// Watches for updates on a thread of its own while `enabled`: a look now
/// if one is due, then hourly whether the next is. Each release made ready
/// (or one found ready from before) goes to `ready`, by version.
pub fn start(enabled: Arc<AtomicBool>, ready: impl Fn(String) + Send + 'static) {
    let Some(app) = installed() else { return };
    std::thread::spawn(move || {
        unsafe {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        settle(&app);
        if let Some(waiting) = staged(&app) {
            ready(waiting.version);
        }
        let testing = std::env::var_os("FLICK_UPDATE_URL").is_some();
        loop {
            if enabled.load(Ordering::Relaxed) && (testing || due()) {
                match look(&app) {
                    Some(found) => {
                        checked();
                        if let Some(version) = found {
                            ready(version);
                        }
                    }
                    None => crate::clock::trace(format_args!("update: no answer")),
                }
            }
            if testing {
                return;
            }
            std::thread::sleep(Duration::from_secs(3600));
        }
    });
}

/// After an update, the first start of the new build: its file types and
/// the uninstall entry's version follow it, and the copies renamed aside by
/// updates go (any still running stays till next time).
fn settle(app: &Path) {
    if register::read(UNINSTALL, "DisplayVersion").as_deref() != Some(VERSION) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = register::register(&exe);
        }
        let _ = register::write(UNINSTALL, "DisplayVersion", VERSION);
    }
    for entry in std::fs::read_dir(app).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.starts_with("flick.") && name.ends_with(".old.exe") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn checked_file() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Flick").join("update-checked"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn due() -> bool {
    let last = checked_file().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| t.trim().parse::<u64>().ok());
    last.is_none_or(|last| now().saturating_sub(last) >= EVERY.as_secs())
}

fn checked() {
    if let Some(file) = checked_file() {
        let _ = std::fs::write(file, now().to_string());
    }
}

/// One look at the newest release. None: no answer (offline, say); else the
/// version now ready, if it is newer than this build.
fn look(app: &Path) -> Option<Option<String>> {
    let url = std::env::var("FLICK_UPDATE_URL").unwrap_or_else(|_| MANIFEST_URL.into());
    let text = String::from_utf8(get(&url, 64 * 1024)?).ok()?;
    let Some(release) = manifest(&text).filter(|m| newer(&m.version)) else { return Some(None) };
    if staged(app).is_some_and(|waiting| waiting.version == release.version) {
        return Some(Some(release.version));
    }
    let exe = get(&release.url, release.size)?;
    if exe.len() as u64 != release.size || sha256(&exe) != Some(release.sha256) {
        crate::clock::trace(format_args!("update: {} doesn't match its manifest", release.url));
        return Some(None);
    }
    // The exe first: a manifest without its exe is never taken (`staged`).
    let dir = app.join("update");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join("flick.exe.part"), &exe).ok()?;
    std::fs::rename(dir.join("flick.exe.part"), dir.join("flick.exe")).ok()?;
    std::fs::write(dir.join("update.txt"), &text).ok()?;
    crate::clock::trace(format_args!("update: {} ready", release.version));
    Some(Some(release.version))
}

/// The update waiting in `app\update`, checked again (signature, newer
/// than this build, the exe's size and hash).
fn staged(app: &Path) -> Option<Manifest> {
    let dir = app.join("update");
    let release = manifest(&std::fs::read_to_string(dir.join("update.txt")).ok()?).filter(|m| newer(&m.version))?;
    let exe = std::fs::read(dir.join("flick.exe")).ok()?;
    (exe.len() as u64 == release.size && sha256(&exe) == Some(release.sha256)).then_some(release)
}

/// Puts the waiting update in place of this exe, which is renamed aside
/// (it may be running; `settle` deletes it later). True: `flick.exe` is now
/// the new version, at the returned path.
pub fn apply() -> Option<PathBuf> {
    let app = installed()?;
    staged(&app)?;
    let exe = app.join("flick.exe");
    let aside = app.join(format!("flick.{}.old.exe", std::process::id()));
    std::fs::rename(&exe, &aside).ok()?;
    if std::fs::rename(app.join("update").join("flick.exe"), &exe).is_err() {
        let _ = std::fs::rename(&aside, &exe);
        return None;
    }
    let _ = std::fs::remove_file(app.join("update").join("update.txt"));
    Some(exe)
}

/// `text` as a manifest, if its last line signs the rest:
/// `flick-update 1`, then `version`, `url`, `size`, `sha256` lines, then
/// `signature` (hex r‖s over SHA-256 of every byte before that line).
fn manifest(text: &str) -> Option<Manifest> {
    let at = text.rfind("\nsignature ")? + 1;
    let (body, last) = text.split_at(at);
    let signature = hex(last.strip_prefix("signature ")?.trim())?;
    if !verify(body.as_bytes(), &signature) {
        crate::clock::trace(format_args!("update: manifest signature doesn't verify"));
        return None;
    }
    let mut lines = body.lines();
    if lines.next()? != "flick-update 1" {
        return None;
    }
    let (mut version, mut url, mut size, mut sha) = (None, None, None, None);
    for line in lines {
        match line.split_once(' ')? {
            ("version", v) => version = Some(v.to_string()),
            ("url", v) => url = Some(v.to_string()),
            ("size", v) => size = v.parse().ok(),
            ("sha256", v) => sha = hex(v)?.try_into().ok(),
            _ => {}
        }
    }
    Some(Manifest { version: version?, url: url?, size: size?, sha256: sha? })
}

/// Whether `version` is newer than this build.
fn newer(version: &str) -> bool {
    is_newer(version, VERSION)
}

/// Whether `theirs` is newer than `ours`, as semver orders them:
/// major.minor.patch, then a pre-release ("1.1.0-dev", a local build of
/// what will be 1.1.0) before its release, pre-releases of one version by
/// their labels. Anything unparsable is never newer.
fn is_newer(theirs: &str, ours: &str) -> bool {
    // (major, minor, patch, released, pre-release label): false < true, so 1.1.0-dev < 1.1.0.
    let parse = |v: &str| -> Option<(u32, u32, u32, bool, String)> {
        let (numbers, pre) = match v.split_once('-') {
            Some((numbers, pre)) if !pre.is_empty() => (numbers, Some(pre)),
            Some(_) => return None,
            None => (v, None),
        };
        let mut parts = numbers.split('.').map(|p| p.parse().ok());
        let parsed = (parts.next()??, parts.next()??, parts.next()??);
        if parts.next().is_some() {
            return None;
        }
        Some((parsed.0, parsed.1, parsed.2, pre.is_none(), pre.unwrap_or("").to_string()))
    };
    matches!((parse(theirs), parse(ours)), (Some(theirs), Some(ours)) if theirs > ours)
}

fn hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    let mut digest = [0u8; 32];
    unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut digest) }.ok().ok()?;
    Some(digest)
}

/// Whether `signature` (r‖s) signs `message` with the release key.
fn verify(message: &[u8], signature: &[u8]) -> bool {
    let (Some(key), Some(digest)) = (hex(PUBLIC_KEY.trim()).filter(|k| k.len() == 64), sha256(message)) else { return false };
    let mut blob = Vec::with_capacity(8 + key.len());
    blob.extend(BCRYPT_ECDSA_PUBLIC_P256_MAGIC.to_le_bytes());
    blob.extend(32u32.to_le_bytes());
    blob.extend(&key);
    unsafe {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        if BCryptImportKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, None, BCRYPT_ECCPUBLIC_BLOB, &mut handle, &blob, 0).is_err() {
            return false;
        }
        let good = BCryptVerifySignature(handle, None, &digest, signature, BCRYPT_FLAGS(0)).is_ok();
        let _ = BCryptDestroyKey(handle);
        good
    }
}

/// A WinHTTP handle, closed when dropped.
struct Handle(*mut std::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn handle(raw: *mut std::ffi::c_void) -> Option<Handle> {
    (!raw.is_null()).then_some(Handle(raw))
}

/// GETs `url` (http or https, redirects followed, the system's proxy), at
/// most `limit` bytes. None on any failure or a status other than 200.
fn get(url: &str, limit: u64) -> Option<Vec<u8>> {
    let (secure, rest) = match url.split_once("://")? {
        ("https", rest) => (true, rest),
        ("http", rest) => (false, rest),
        _ => return None,
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, if secure { INTERNET_DEFAULT_HTTPS_PORT } else { INTERNET_DEFAULT_HTTP_PORT }),
    };
    unsafe {
        let session = handle(WinHttpOpen(&HSTRING::from(format!("Flick/{VERSION}")), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0))?;
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000).ok()?;
        let connection = handle(WinHttpConnect(session.0, &HSTRING::from(host), port, 0))?;
        let flags = if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) };
        let request = handle(WinHttpOpenRequest(connection.0, w!("GET"), &HSTRING::from(path), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), flags))?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0).ok()?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).ok()?;
        let (mut status, mut length) = (0u32, size_of::<u32>() as u32);
        WinHttpQueryHeaders(request.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some((&raw mut status).cast()), &mut length, std::ptr::null_mut()).ok()?;
        if status != 200 {
            crate::clock::trace(format_args!("update: {url}: HTTP {status}"));
            return None;
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read).ok()?;
            if read == 0 {
                return Some(body);
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() as u64 > limit {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest signed by `tools\release.ps1`'s method with the release
    /// key verifies; a changed byte or a wrong signature doesn't.
    #[test]
    fn verifies_signed_manifests_only() {
        let Some(text) = std::env::var_os("FLICK_TEST_MANIFEST").and_then(|p| std::fs::read_to_string(p).ok()) else { return };
        let release = manifest(&text).expect("signed by the release key");
        assert!(release.size > 0);
        assert!(manifest(&text.replacen("size ", "size 1", 1)).is_none(), "an edited manifest");
        let at = text.rfind("signature ").unwrap() + "signature ".len();
        let mut forged = text.clone();
        forged.replace_range(at..at + 2, if &text[at..at + 2] == "00" { "01" } else { "00" });
        assert!(manifest(&forged).is_none(), "a wrong signature");
    }

    #[test]
    fn hashes_and_compares_versions() {
        // SHA-256 of "abc" (FIPS 180-2's example).
        assert_eq!(sha256(b"abc").map(|d| d.iter().map(|b| format!("{b:02x}")).collect::<String>()).as_deref(), Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
        assert!(newer("99.0.0") && !newer(VERSION) && !newer("0.0.1") && !newer("x.y.z"));
    }

    /// A local build of the next version (1.1.0-dev) takes that version's
    /// release as an update, but not the release it was made after.
    #[test]
    fn a_pre_release_sorts_below_its_release() {
        assert!(is_newer("1.1.0", "1.1.0-dev"), "the release replaces its dev build");
        assert!(!is_newer("1.0.0", "1.1.0-dev"), "the release before doesn't");
        assert!(!is_newer("1.1.0-dev", "1.1.0"));
        assert!(is_newer("1.1.0-dev", "1.0.0"), "1.1.0-dev is past 1.0.0");
        assert!(is_newer("1.1.1", "1.1.0-dev") && is_newer("1.1.0-rc2", "1.1.0-rc1"));
        assert!(!is_newer("1.1.0-dev", "1.1.0-dev") && !is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.1.0-", "1.0.0") && !is_newer("1.1.0.4", "1.0.0") && !is_newer("1.x.0", "1.0.0"));
    }
}
