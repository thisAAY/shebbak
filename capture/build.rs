// `screencapturekit`'s own build script bakes the Swift runtime rpaths (for
// `libswift_Concurrency.dylib` et al.) into *its* crate's binaries/tests via
// `cargo:rustc-link-arg`, but that instruction does not propagate to a
// downstream package's binaries — cargo only applies it within the package
// that owns the build script. Without repeating it here, `list_windows` and
// `capture_probe` fail at launch with `dyld: Library not loaded:
// @rpath/libswift_Concurrency.dylib`.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    if let Ok(output) = std::process::Command::new("xcode-select").arg("-p").output() {
        if output.status.success() {
            let xcode_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,{xcode_path}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift-5.5/macosx"
            );
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,{xcode_path}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
            );
        }
    }
}
