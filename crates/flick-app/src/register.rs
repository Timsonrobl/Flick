//! Flick's file associations for the current user (`flick --register`,
//! `flick --unregister`): "Open with", and a choice under Settings ▸ Apps ▸
//! Default apps. Only under HKEY_CURRENT_USER (no admin rights), and only
//! Flick's own keys and values:
//!   Software\Classes\Flick.Image                          the "Flick image" file type
//!   Software\Classes\Applications\flick.exe               the application entry
//!   Software\Classes\.<ext>\OpenWithProgids : Flick.Image   one value per extension
//!   Software\Flick\Capabilities, Software\RegisteredApplications : Flick
//! It never changes which program is the default; Windows leaves that choice
//! to the user (Open with ▸ Always, or Settings ▸ Default apps ▸ Flick).
//!
//! The extensions are the ones Flick opens (`folder.rs`, `raw.rs`), so a
//! build that learns a format registers it: the installer registers, and so
//! does every update. `tools\register.ps1` runs this for a dev build.

use std::iter::once;
use std::path::Path;

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW,
    RegGetValueW, RegSetValueExW,
};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use windows::core::{HSTRING, PCWSTR, Result};

const CLASSES: &str = r"Software\Classes";
const PROG_ID: &str = "Flick.Image";
const APP: &str = r"Software\Classes\Applications\flick.exe";
const CAPABILITIES: &str = r"Software\Flick\Capabilities";
const REGISTERED: &str = r"Software\RegisteredApplications";

/// Every extension Flick opens, with its dot.
fn extensions() -> impl Iterator<Item = String> {
    crate::folder::IMAGE_EXTENSIONS.iter().chain(crate::raw::EXTENSIONS).map(|e| format!(".{e}"))
}

/// Registers `exe` as the program for Flick's file types.
pub fn register(exe: &Path) -> Result<()> {
    let exe = exe.display();
    let command = format!("\"{exe}\" \"%1\"");
    set(&format!(r"{CLASSES}\{PROG_ID}"), None, "Flick image")?;
    set(&format!(r"{CLASSES}\{PROG_ID}\DefaultIcon"), None, &format!("\"{exe}\",0"))?;
    set(&format!(r"{CLASSES}\{PROG_ID}\shell\open\command"), None, &command)?;
    set(APP, Some("FriendlyAppName"), "Flick")?;
    set(&format!(r"{APP}\shell\open\command"), None, &command)?;
    set(CAPABILITIES, Some("ApplicationName"), "Flick")?;
    set(CAPABILITIES, Some("ApplicationDescription"), "Fast frameless image viewer")?;
    for ext in extensions() {
        set(&format!(r"{CLASSES}\{ext}\OpenWithProgids"), Some(PROG_ID), "")?;
        set(&format!(r"{APP}\SupportedTypes"), Some(&ext), "")?;
        set(&format!(r"{CAPABILITIES}\FileAssociations"), Some(&ext), PROG_ID)?;
    }
    set(REGISTERED, Some("Flick"), CAPABILITIES)?;
    changed();
    Ok(())
}

/// Removes what `register` added, and nothing else: other programs' entries
/// under each extension stay. Where Flick was the default, Windows asks again.
pub fn unregister() -> Result<()> {
    for key in [format!(r"{CLASSES}\{PROG_ID}"), APP.into(), r"Software\Flick".into()] {
        gone(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key)) }.ok())?;
    }
    for ext in extensions() {
        delete_value(&format!(r"{CLASSES}\{ext}\OpenWithProgids"), PROG_ID)?;
    }
    delete_value(REGISTERED, "Flick")?;
    changed();
    Ok(())
}

/// A string value under HKEY_CURRENT_USER, if there is one.
pub fn read(key: &str, name: &str) -> Option<String> {
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name), RRF_RT_REG_SZ, None, Some(buffer.as_mut_ptr().cast()), Some(&mut size)) }.ok().ok()?;
    let chars = (size as usize / 2).saturating_sub(1); // without the terminating zero
    Some(String::from_utf16_lossy(&buffer[..chars]))
}

/// A string value under HKEY_CURRENT_USER (the key made if missing).
pub fn write(key: &str, name: &str, value: &str) -> Result<()> {
    set(key, Some(name), value)
}

/// A string value (`None`: the key's default), creating the key if missing
/// (and only then: other values under it stay).
fn set(key: &str, name: Option<&str>, value: &str) -> Result<()> {
    let mut handle = HKEY::default();
    unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(key), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut handle, None) }.ok()?;
    let data: Vec<u16> = value.encode_utf16().chain(once(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2) };
    let name = name.map(HSTRING::from);
    let result = unsafe { RegSetValueExW(handle, name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())), None, REG_SZ, Some(bytes)) }.ok();
    let _ = unsafe { RegCloseKey(handle) };
    result
}

fn delete_value(key: &str, name: &str) -> Result<()> {
    gone(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name)) }.ok())
}

/// Something already absent counts as removed.
fn gone(result: Result<()>) -> Result<()> {
    match result {
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
        other => other,
    }
}

/// Tells Explorer that associations changed.
fn changed() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
