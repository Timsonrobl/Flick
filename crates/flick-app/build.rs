//! Links the app icon into flick.exe (flick.rc; the .ico comes from
//! `cargo run -p flick-icon`).

fn main() {
    println!("cargo:rerun-if-changed=flick.rc");
    println!("cargo:rerun-if-changed=../../assets/icon/flick.ico");
    embed_resource::compile("flick.rc", embed_resource::NONE).manifest_optional().expect("compiling flick.rc");
}
