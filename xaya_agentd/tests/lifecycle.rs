#![cfg(unix)]

use std::io::{BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use tempfile::TempDir;
use xaya_agent_proto::{self as proto, InitializeParams, Request, Response};
use xaya_agentd::client::Client;
use xaya_agentd::runtime::Runtime;
use xaya_agentd::wire::{IO_TIMEOUT, read_frame, write_frame};

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");

struct Service {
    _temp: TempDir,
    runtime: Runtime,
    child: Option<Child>,
}

impl Service {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("runtime"))).unwrap();
        Self { _temp: temp, runtime, child: None }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .args(args)
            .output()
            .unwrap()
    }

    fn foreground(&mut self, idle: &str) {
        self.child = Some(
            Command::new(BINARY)
                .arg("--runtime-dir")
                .arg(self.runtime.directory())
                .args(["serve", "--idle-timeout-secs", idle])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while Client::connect(&self.runtime).is_err() {
            assert!(Instant::now() < deadline, "service did not start");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.cli(&["stop"]);
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn concurrent_starters_share_one_daemon() {
    let service = Service::new();
    let children = (0..8)
        .map(|_| {
            Command::new(BINARY)
                .arg("--runtime-dir")
                .arg(service.runtime.directory())
                .args(["start", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut pids = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        pids.push(status["pid"].as_u64().unwrap());
    }
    assert!(pids.iter().all(|pid| *pid == pids[0]));
    assert_eq!(fs::metadata(service.runtime.directory()).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(service.runtime.socket()).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn crash_leaves_stale_socket_that_can_be_recovered() {
    let mut service = Service::new();
    service.foreground("600");
    let child = service.child.as_mut().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    service.child = None;
    assert!(service.runtime.socket().exists());
    let output = service.cli(&["start", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(Client::connect(&service.runtime).unwrap().status().is_ok());
}

#[test]
fn idle_service_exits_and_cleans_its_socket() {
    let mut service = Service::new();
    service.foreground("1");
    let deadline = Instant::now() + Duration::from_secs(4);
    while service.child.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "idle service stayed alive");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!service.runtime.socket().exists());
}

#[test]
fn uninitialized_slow_and_malformed_clients_do_not_block_status() {
    let mut service = Service::new();
    service.foreground("600");
    let _slow = UnixStream::connect(service.runtime.socket()).unwrap();
    let mut raw = BufReader::new(UnixStream::connect(service.runtime.socket()).unwrap());
    write_frame(raw.get_mut(), &Request::new(1, proto::SHUTDOWN, serde_json::Value::Null).unwrap())
        .unwrap();
    let response: Response =
        serde_json::from_slice(&read_frame(&mut raw, IO_TIMEOUT).unwrap()).unwrap();
    let Response::Failure(failure) = response else {
        panic!("shutdown was accepted without initialization")
    };
    assert_eq!(failure.error.code, proto::NOT_INITIALIZED);
    let mut malformed = UnixStream::connect(service.runtime.socket()).unwrap();
    malformed.write_all(b"not-json\n").unwrap();
    let start = Instant::now();
    assert!(Client::connect(&service.runtime).unwrap().status().is_ok());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn startup_waits_for_private_socket_permissions() {
    let service = Service::new();
    let socket = service.runtime.socket();
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o700)).unwrap();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(60));
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600)).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let request: Request =
            serde_json::from_slice(&read_frame(&mut reader, IO_TIMEOUT).unwrap()).unwrap();
        let params: InitializeParams = serde_json::from_value(request.params).unwrap();
        assert_eq!(params.protocol_version, proto::PROTOCOL_VERSION);
        write_frame(
            reader.get_mut(),
            &Response::success(request.id, proto::InitializeResult {
                protocol_version: proto::PROTOCOL_VERSION,
                server_version: "test".into(),
                capabilities: vec![proto::STATUS.into()],
            })
            .unwrap(),
        )
        .unwrap();
    });
    assert!(
        Client::ensure_running(&service.runtime, std::path::Path::new("/not-a-binary")).is_ok()
    );
    worker.join().unwrap();
    fs::remove_file(service.runtime.socket()).unwrap();
}

#[test]
fn mismatched_response_id_is_rejected_without_starting_another_service() {
    let service = Service::new();
    let listener = UnixListener::bind(service.runtime.socket()).unwrap();
    fs::set_permissions(service.runtime.socket(), fs::Permissions::from_mode(0o600)).unwrap();
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let _ = read_frame(&mut reader, IO_TIMEOUT).unwrap();
        write_frame(
            reader.get_mut(),
            &Response::success(proto::RequestId::Number(999), serde_json::json!({})).unwrap(),
        )
        .unwrap();
    });
    assert!(matches!(
        Client::ensure_running(&service.runtime, std::path::Path::new("/not-a-binary")),
        Err(xaya_agentd::Error::Protocol(_))
    ));
    worker.join().unwrap();
    fs::remove_file(service.runtime.socket()).unwrap();
}
