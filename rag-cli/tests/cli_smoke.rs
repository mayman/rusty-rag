//! End-to-end smoke test: spawns the built `rag` binary.

use std::process::Command;

#[test]
fn version_flag_prints_name_and_version_and_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_rag"))
        .arg("--version")
        .output()
        .expect("rag binary should start");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rag --version should exit 0, got {:?}",
        output.status
    );
    assert_eq!(
        stdout.trim_end(),
        format!("rag {}", env!("CARGO_PKG_VERSION"))
    );
}
