use std::process::Command;

#[test]
fn version_flag_prints_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_loom-wallpaper"))
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "expected version in {text:?}"
    );
}
