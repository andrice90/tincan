//! The voice room join and leave sounds are third-party (Ovani Sound, royalty-free) and are
//! not part of the Apache-2.0 source: `assets/voice-join.wav` and `assets/voice-leave.wav`
//! are git-ignored. When both are present the build embeds them (`cfg(tincan_cues)`);
//! without them the app builds and runs with silent cues.

use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(tincan_cues)");
    for f in ["assets/voice-join.wav", "assets/voice-leave.wav"] {
        println!("cargo::rerun-if-changed={f}");
    }
    if Path::new("assets/voice-join.wav").is_file() && Path::new("assets/voice-leave.wav").is_file() {
        println!("cargo::rustc-cfg=tincan_cues");
    } else {
        println!("cargo::warning=voice cue sounds not found in crates/tincan-app/assets: building without join and leave sounds");
    }
}
