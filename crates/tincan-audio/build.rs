//! Embeds the DPDFNet model when `models/dpdfnet2_48khz_hr.onnx` exists (scripts/fetch-dpdfnet.ps1
//! puts it there; it is not in git). Without it the crate builds with WebRTC noise suppression only.

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("models")
        .join("dpdfnet2_48khz_hr.onnx");
    println!("cargo::rustc-check-cfg=cfg(dpdfnet_model)");
    println!("cargo::rerun-if-changed=models");
    println!("cargo::rerun-if-changed={}", path.display());
    if path.is_file() {
        println!("cargo::rustc-cfg=dpdfnet_model");
    }
}
