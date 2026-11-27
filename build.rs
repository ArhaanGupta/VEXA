fn main() {
    // Embed the Windows manifest (modern controls, per-monitor DPI) when building with MSVC.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") && target.contains("msvc") {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rerun-if-changed=app.manifest");
    }
}
