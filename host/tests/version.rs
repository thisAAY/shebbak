use std::process::Command;

fn assert_version_flag(flag: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_srw-host"))
        .arg(flag)
        .output()
        .expect("failed to spawn srw-host");
    assert!(
        out.status.success(),
        "{flag}: nonzero exit: {:?}",
        out.status
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        env!("CARGO_PKG_VERSION"),
        "{flag}: unexpected stdout"
    );
}

#[test]
fn version_long_flag_prints_version_and_exits_zero() {
    assert_version_flag("--version");
}

#[test]
fn version_short_flag_prints_version_and_exits_zero() {
    assert_version_flag("-V");
}
