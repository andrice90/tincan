fn main() {
    // tauri-build embeds icons/icon.ico in the exe, but cargo does not know the build depends
    // on it: without these lines a changed icon is not picked up until something else in the
    // crate changes, and the exe (and every shortcut to it) keeps the old one.
    for f in ["icons/icon.ico", "icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png", "tauri.conf.json"] {
        println!("cargo:rerun-if-changed={f}");
    }
    tauri_build::build();
}
