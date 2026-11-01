//! Links the app icon and the version details into flick.exe, through a
//! resource script written here (the .ico comes from `cargo run -p
//! flick-icon`; the version from Cargo.toml). The first icon group is what
//! Explorer, the taskbar and "DefaultIcon = flick.exe,0" show; icon.rs
//! loads it by this ordinal for windows and the tray. The version details
//! name the program "Flick" in Task Manager, Settings and Windows'
//! notifications (the tray's "update ready").

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon/flick.ico");
    let var = |name: &str| std::env::var(name).expect(name);
    let icon = PathBuf::from(var("CARGO_MANIFEST_DIR")).join("../../assets/icon/flick.ico");
    let icon = icon.to_string_lossy().replace('\\', "/");
    let version = var("CARGO_PKG_VERSION");
    let numbers = format!("{},{},{},0", var("CARGO_PKG_VERSION_MAJOR"), var("CARGO_PKG_VERSION_MINOR"), var("CARGO_PKG_VERSION_PATCH"));
    let script = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Flick"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "flick"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "flick.exe"
      VALUE "ProductName", "Flick"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = PathBuf::from(var("OUT_DIR")).join("flick.rc");
    std::fs::write(&path, script).expect("writing flick.rc");
    embed_resource::compile(&path, embed_resource::NONE).manifest_optional().expect("compiling flick.rc");
}
