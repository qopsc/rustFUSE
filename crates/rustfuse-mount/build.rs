fn main() {
    // `fuse_mount`: this build can serve FUSE mounts (Linux, or macOS with
    // the `macfuse` feature).
    println!("cargo::rustc-check-cfg=cfg(fuse_mount)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "linux" || (os == "macos" && std::env::var_os("CARGO_FEATURE_MACFUSE").is_some()) {
        println!("cargo::rustc-cfg=fuse_mount");
    }
}
