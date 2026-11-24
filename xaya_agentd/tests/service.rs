#![cfg(unix)]

use std::process::{Command, Stdio};

use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");

#[test]
fn status_starts_once_and_stop_removes_only_the_socket() {
    let temp = TempDir::new().unwrap();
    let runtime = temp.path().join("runtime");
    let call = |args: &[&str]| {
        let output =
            Command::new(BINARY).arg("--runtime-dir").arg(&runtime).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        output.stdout
    };
    let first: serde_json::Value = serde_json::from_slice(&call(&["status", "--json"])).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&call(&["start", "--json"])).unwrap();
    assert_eq!(first["pid"], second["pid"]);
    assert_eq!(first["active_sessions"], 0);
    call(&["stop", "--json"]);
    assert!(!runtime.join("agentd.sock").exists());
    assert!(runtime.join("agentd.lock").exists());
    let status = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(&runtime)
        .args(["status", "--no-start"])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}
