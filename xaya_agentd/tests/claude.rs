#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use serde_json::{Value, json};
use xaya_agentd::client::Client;
use xaya_agentd::pty_host;
use xaya_agentd::runtime::Runtime;

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");

struct Service {
    temp: tempfile::TempDir,
    runtime: Runtime,
    process: Child,
}

impl Service {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        let executable = temp.path().join("claude");
        fs::write(&executable, include_str!("fixtures/claude.py")).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let process = Command::new(BINARY)
            .args(["--runtime-dir", runtime.directory().to_str().unwrap(), "serve"])
            .envs(inherited(temp.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let service = Self { temp, runtime, process };
        service.wait(|| Client::connect(&service.runtime).ok());
        service
    }

    fn request(&self, method: &str, params: Value) -> xaya_agentd::Result<Value> {
        Client::connect(&self.runtime)?.request(method, params)
    }

    fn wait<T>(&self, mut condition: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(value) = condition() {
                return value;
            }
            assert!(Instant::now() < deadline, "Claude fixture timed out");
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn start(&self) -> Value {
        self.request(
            "session.start_claude",
            json!({
                "cwd":self.temp.path(), "executable":self.temp.path().join("claude"),
                "config_dir":self.temp.path().join("config"), "prompt":"native first turn"
            }),
        )
        .unwrap()
    }

    fn ready(&self, id: &Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let s = self.request("session.get", json!({"session_id":id})).unwrap();
            assert_ne!(s["state"], "failed", "{s}");
            if s["state"] == "ready" {
                return s;
            }
            assert!(Instant::now() < deadline, "Claude never became ready; last state: {s}");
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn crash_and_restart(&mut self) {
        self.crash();
        self.restart();
    }

    fn crash(&mut self) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
    }

    /// Replace the service with one that kills itself (SIGKILL) at the named crash point.
    fn restart_crashing_at(&mut self, point: &str) {
        self.crash();
        self.process = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .arg("serve")
            .envs(inherited(self.temp.path()))
            .env("XAYA_AGENTD_CRASH_AT", point)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        // It may reach its crash point (e.g. while rejoining a session) before anyone connects.
        let deadline = Instant::now() + Duration::from_secs(15);
        while Client::connect(&self.runtime).is_err() && self.process.try_wait().unwrap().is_none()
        {
            assert!(Instant::now() < deadline, "the crashing service never started");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the service killed itself at its crash point, then start a normal one.
    fn crashed_then_restart(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the service did not reach its crash point");
            thread::sleep(Duration::from_millis(20));
        };
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL), "crashed, not exited: {status:?}");
        self.restart();
    }

    fn restart(&mut self) {
        self.process = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .arg("serve")
            .envs(inherited(self.temp.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        self.wait(|| Client::connect(&self.runtime).ok());
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.request("server.shutdown", Value::Null);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.process.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
        // The fake worker exits on its own when the private config directory disappears.
    }
}

#[test]
fn claude_cancellation_is_turn_bound_deduplicated_and_persistent() {
    let mut service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    fs::write(service.temp.path().join("config/activate-turn"), "").unwrap();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["turn_id"] == "cancel-turn").then_some(())
    });
    let params = json!({"session_id":id,"command_id":"cancel-once","turn_id":"cancel-turn"});
    let receipt = service.request("session.cancel", params.clone()).unwrap();
    assert!(["pending", "succeeded"].contains(&receipt["status"].as_str().unwrap()));
    let receipt = service.wait(|| {
        let r = service
            .request("session.command", json!({"session_id":id,"command_id":"cancel-once"}))
            .unwrap();
        (r["status"] != "pending").then_some(r)
    });
    assert_eq!(receipt["status"], "succeeded", "{receipt}");
    assert_eq!(receipt["turn_id"], "cancel-turn");
    service.ready(id);
    assert_eq!(service.request("session.cancel", params.clone()).unwrap(), receipt);
    assert!(
        service
            .request(
                "session.cancel",
                json!({"session_id":id,"command_id":"cancel-once","turn_id":"another"})
            )
            .is_err()
    );
    assert!(
        service
            .request(
                "session.cancel",
                json!({"session_id":id,"command_id":"stale","turn_id":"cancel-turn"})
            )
            .is_err()
    );
    let calls = fs::read_to_string(service.temp.path().join("config/controls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(), 1, "duplicate command must not reach native worker");
    service.crash_and_restart();
    assert_eq!(service.request("session.cancel", params).unwrap(), receipt);
    assert_eq!(
        fs::read_to_string(service.temp.path().join("config/controls.jsonl")).unwrap(),
        calls
    );
}

#[test]
fn claude_cancellation_crash_preserves_unknown_receipt_without_replay() {
    let mut service = Service::new();
    fs::write(service.temp.path().join("config/drop-control-ack"), "").unwrap();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    fs::write(service.temp.path().join("config/activate-turn"), "").unwrap();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["turn_id"] == "cancel-turn").then_some(())
    });
    let params = json!({"session_id":id,"command_id":"ack-lost","turn_id":"cancel-turn"});
    service.request("session.cancel", params.clone()).unwrap();
    service.ready(id);
    let path = service.temp.path().join("config/controls.jsonl");
    let calls = fs::read_to_string(&path).unwrap();
    service.crash_and_restart();
    let receipt = service.request("session.cancel", params).unwrap();
    assert_eq!(receipt["status"], "unknown", "{receipt}");
    assert_eq!(receipt["turn_id"], "cancel-turn");
    assert_eq!(fs::read_to_string(path).unwrap(), calls);
}

#[test]
fn claude_cancellation_storage_failure_never_reaches_worker() {
    let service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    fs::write(service.temp.path().join("config/activate-turn"), "").unwrap();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["turn_id"] == "cancel-turn").then_some(())
    });
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_commands BEFORE INSERT ON sessions BEGIN SELECT \
         RAISE(FAIL,'fixture'); END;",
    )
    .unwrap();
    assert!(
        service
            .request(
                "session.cancel",
                json!({"session_id":id,"command_id":"must-not-dispatch","turn_id":"cancel-turn"})
            )
            .is_err()
    );
    thread::sleep(Duration::from_millis(150));
    assert!(!service.temp.path().join("config/controls.jsonl").exists());
    db.execute_batch("DROP TRIGGER refuse_commands").unwrap();
}

fn settled(service: &Service, id: &Value, command: &str) -> Value {
    service.wait(|| {
        let r = service
            .request("session.command", json!({"session_id":id,"command_id":command}))
            .unwrap();
        (r["status"] != "pending").then_some(r)
    })
}

fn sent(service: &Service) -> Vec<Value> {
    fs::read_to_string(service.temp.path().join("config/controls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn claude_program_send_is_delivered_only_when_its_turn_starts_and_never_replayed() {
    let mut service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    let params = json!({"session_id":id,"command_id":"send-once","text":"program message"});
    let receipt = service.request("session.send", params.clone()).unwrap();
    assert_eq!(receipt["status"], "pending", "{receipt}");
    let receipt = settled(&service, id, "send-once");
    assert_eq!(receipt["status"], "succeeded", "{receipt}");
    // Delivery is the native turn that carries exactly this text, not the submit return.
    assert_eq!(receipt["turn_id"], "send-1");
    service.ready(id);
    assert_eq!(sent(&service), vec![
        json!({"command_id":"send-once","operation":"send","text":"program message"})
    ]);
    assert_eq!(service.request("session.send", params.clone()).unwrap(), receipt);
    assert!(
        service
            .request(
                "session.send",
                json!({"session_id":id,"command_id":"send-once","text":"another message"})
            )
            .is_err(),
        "a command ID is bound to its text"
    );
    for text in ["", "   ", &"x".repeat(8193)] {
        assert!(
            service
                .request("session.send", json!({"session_id":id,"command_id":"bad","text":text}))
                .is_err()
        );
    }
    service.crash_and_restart();
    assert_eq!(service.request("session.send", params).unwrap(), receipt);
    assert_eq!(sent(&service).len(), 1, "a receipt is never replayed");
}

#[test]
fn claude_program_send_is_refused_while_a_turn_is_active() {
    let service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    fs::write(service.temp.path().join("config/activate-turn"), "").unwrap();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["turn_id"] == "cancel-turn").then_some(())
    });
    assert!(
        service
            .request("session.send", json!({"session_id":id,"command_id":"busy","text":"later"}))
            .is_err()
    );
    thread::sleep(Duration::from_millis(300));
    assert!(sent(&service).is_empty());
}

#[test]
fn claude_dropped_send_fails_explicitly_and_points_to_the_native_tui() {
    let service = Service::new();
    fs::write(service.temp.path().join("config/drop-send"), "").unwrap();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    service
        .request("session.send", json!({"session_id":id,"command_id":"dropped","text":"hello"}))
        .unwrap();
    let receipt = settled(&service, id, "dropped");
    assert_eq!(receipt["status"], "failed", "{receipt}");
    let error = receipt["error"].as_str().unwrap();
    assert!(error.contains("not submitted") && error.contains("native"), "{error}");
    assert_eq!(receipt["turn_id"], Value::Null);
    // The session is still usable; the drop is never retried behind the user's back.
    service.ready(id);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(sent(&service).len(), 1);
}

#[test]
fn claude_send_without_its_own_turn_is_uncertain() {
    for (flag, reason) in [("silent-send", "did not start"), ("send-other-turn", "another turn")] {
        let service = Service::new();
        fs::write(service.temp.path().join("config").join(flag), "").unwrap();
        let started = service.start();
        let id = &started["id"];
        service.ready(id);
        service
            .request("session.send", json!({"session_id":id,"command_id":"maybe","text":"hello"}))
            .unwrap();
        let receipt = settled(&service, id, "maybe");
        assert_eq!(receipt["status"], "unknown", "{flag}: {receipt}");
        let error = receipt["error"].as_str().unwrap();
        assert!(error.contains(reason) && error.contains("do not replay"), "{flag}: {error}");
        assert_eq!(sent(&service).len(), 1, "{flag}");
    }
}

fn pending_approval(service: &Service, id: &Value) -> Value {
    service.wait(|| {
        let snapshot = service.request("approval.list", json!({"session_id":id})).unwrap();
        snapshot["approvals"].as_array().and_then(|a| a.first().cloned())
    })
}

fn hook_responses(service: &Service) -> Vec<Value> {
    fs::read_to_string(service.temp.path().join("config/hook-responses.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn respond(
    service: &Service,
    id: &Value,
    command: &str,
    approval: &Value,
    decision: &str,
) -> Value {
    service
        .request(
            "approval.respond",
            json!({"session_id":id,"command_id":command,"approval_id":approval,"decision":decision}),
        )
        .unwrap()
}

#[test]
fn claude_panel_answer_reaches_the_waiting_permission_hook_once() {
    for (decision, behavior) in [("accept", "allow"), ("decline", "deny")] {
        let mut service = Service::new();
        let started = service.start();
        let id = &started["id"];
        service.ready(id);
        type_line(&service, id, "permission", "PERMISSION_ASKED");
        let approval = pending_approval(&service, id);
        assert_eq!(approval["controllable"], true, "{approval}");
        assert_eq!(approval["turn_id"], "perm-turn");
        assert_eq!(approval["method"], "PermissionRequest");
        assert_eq!(approval["details"]["tool_name"], "Write", "{approval}");
        assert_eq!(approval["details"]["tool_input"]["file_path"], "allowed.txt");
        assert_eq!(approval["decisions"], json!(["accept", "decline"]));
        let session = service.request("session.get", json!({"session_id":id})).unwrap();
        assert_eq!(session["state"], "blocked");
        assert!(hook_responses(&service).is_empty(), "the hook waits for an answer");

        let receipt = respond(&service, id, "answer", &approval["id"], decision);
        assert!(["pending", "succeeded"].contains(&receipt["status"].as_str().unwrap()));
        let receipt = settled(&service, id, "answer");
        assert_eq!(receipt["status"], "succeeded", "{decision}: {receipt}");
        let responses = service.wait(|| Some(hook_responses(&service)).filter(|r| !r.is_empty()));
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]["hookSpecificOutput"]["decision"]["behavior"], behavior,
            "{responses:?}"
        );
        assert_eq!(responses[0]["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        service.ready(id);
        let snapshot = service.request("approval.list", json!({"session_id":id})).unwrap();
        assert_eq!(snapshot["approvals"], json!([]));
        // Same command: same receipt; a new command for the answered request is stale.
        assert_eq!(respond(&service, id, "answer", &approval["id"], decision), receipt);
        assert!(
            service
                .request(
                    "approval.respond",
                    json!({"session_id":id,"command_id":"again","approval_id":approval["id"],
                        "decision":decision})
                )
                .is_err()
        );
        service.crash_and_restart();
        assert_eq!(respond(&service, id, "answer", &approval["id"], decision), receipt);
        assert_eq!(hook_responses(&service).len(), 1, "an answer is never replayed");
    }
}

#[test]
fn claude_native_answer_expires_the_panel_request_and_rejects_late_answers() {
    let service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    type_line(&service, id, "permission", "PERMISSION_ASKED");
    let approval = pending_approval(&service, id);
    type_line(&service, id, "native-answer", "NATIVE_ANSWERED");
    // The hook is released with no decision; the native answer stands.
    let responses = service.wait(|| Some(hook_responses(&service)).filter(|r| !r.is_empty()));
    assert_eq!(responses, vec![json!({})]);
    service.ready(id);
    let snapshot = service.request("approval.list", json!({"session_id":id})).unwrap();
    assert_eq!(snapshot["approvals"], json!([]));
    assert!(
        service
            .request(
                "approval.respond",
                json!({"session_id":id,"command_id":"late","approval_id":approval["id"],
                    "decision":"accept"})
            )
            .is_err(),
        "a late answer is refused"
    );
    assert_eq!(hook_responses(&service).len(), 1);
}

#[test]
fn claude_oversized_permission_stays_native_only() {
    let service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    type_line(&service, id, "large-permission", "PERMISSION_ASKED");
    let snapshot = service.wait(|| {
        let s = service.request("approval.list", json!({"session_id":id})).unwrap();
        (s["native_only_approvals"] == 1).then_some(s)
    });
    assert_eq!(snapshot["approvals"], json!([]));
    // Without complete details nothing can be decided outside the TUI; the hook is released.
    let responses = service.wait(|| Some(hook_responses(&service)).filter(|r| !r.is_empty()));
    assert_eq!(responses, vec![json!({})]);
}

#[test]
fn claude_attach_lease_excludes_second_client_and_releases_on_disconnect() {
    let service = Service::new();
    let s = service.start();
    let s = service.ready(&s["id"]);
    let mut lease = Client::connect(&service.runtime).unwrap();
    let _: Value = lease.request("session.attach_claim", json!({"session_id":s["id"]})).unwrap();
    assert_eq!(
        service.request("session.get", json!({"session_id":s["id"]})).unwrap()["attached"],
        true
    );
    assert!(service.request("session.attach_claim", json!({"session_id":s["id"]})).is_err());
    drop(lease);
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":s["id"]})).unwrap();
        (s["attached"] == false).then_some(s)
    });
    let mut lease = Client::connect(&service.runtime).unwrap();
    let _: Value = lease.request("session.attach_claim", json!({"session_id":s["id"]})).unwrap();
}

/// The service's environment, as if its first caller had its own CLAUDE_CONFIG_DIR; and where
/// the fixture keeps its files when it runs without one.
fn inherited(temp: &std::path::Path) -> [(&'static str, std::path::PathBuf); 2] {
    [
        ("CLAUDE_CONFIG_DIR", "/nonexistent/inherited-by-the-service".into()),
        ("XAYA_CLAUDE_FIXTURE_HOME", temp.join("config")),
    ]
}

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(Command::new("git").current_dir(root).args(args).status().unwrap().success());
}

fn calls(service: &Service) -> Vec<Value> {
    fs::read_to_string(service.temp.path().join("config/calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn host_dir(service: &Service, id: &Value) -> std::path::PathBuf {
    let info = service.request("session.attach_info", json!({"session_id":id})).unwrap();
    assert_eq!(info["args"][0], "pty-attach", "{info}");
    assert_eq!(info["args"][1], "--dir");
    std::path::PathBuf::from(info["args"][2].as_str().unwrap())
}

/// Type into the owned PTY exactly as a terminal would, then detach.
fn type_line(service: &Service, id: &Value, line: &str, expect: &str) {
    let size = pty_host::Size { rows: 24, cols: 80 };
    let mut attachment = pty_host::attach(&host_dir(service, id), size).unwrap();
    attachment.send(format!("{line}\r").as_bytes()).unwrap();
    attachment.read_until(expect, Duration::from_secs(10)).unwrap();
}

fn checkpoint(service: &Service, id: &Value, command: &str) -> Value {
    let cp = service
        .request("checkpoint.create", json!({"session_id":id,"command_id":command}))
        .unwrap();
    service.wait(|| {
        let c = service.request("checkpoint.get", json!({"checkpoint_id":cp["id"]})).unwrap();
        (c["status"] != "creating").then_some(c)
    })
}

fn repository(service: &Service) {
    let root = service.temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Claude fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(root.join(".gitignore"), "*\n").unwrap();
    fs::write(root.join("source"), "unchanged\n").unwrap();
    git(root, &["add", "-f", "source"]);
    git(root, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"]);
}

#[test]
fn claude_runs_in_an_owned_pty_with_the_identity_xaya_chose() {
    let service = Service::new();
    let started = service.start();
    let session = service.ready(&started["id"]);
    assert_eq!(session["engine"], "claude");
    assert_eq!(session["sandbox"], "manual");
    // XaYa chooses the native UUID; there is no vendor background job to look up.
    let native = session["native_thread_id"].as_str().unwrap();
    assert_eq!(native.len(), 36, "{native}");
    assert_eq!(&native[14..15], "4", "UUID v4");
    assert!(session["native_job_id"].is_null());
    assert_ne!(session["id"], session["native_thread_id"]);
    let launches = calls(&service);
    assert_eq!(launches.len(), 1, "one launch, no listing/attach/stop CLI calls: {launches:?}");
    let argv = &launches[0];
    let position = argv.as_array().unwrap().iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(argv[position + 1], native);
    assert!(argv.as_array().unwrap().iter().all(|a| a != "--bg"));
    assert_eq!(argv.as_array().unwrap().last().unwrap(), "native first turn");
    // Flag settings pin manual mode, keeping the auto-mode default dialog out of the TUI.
    let settings = argv.as_array().unwrap().iter().position(|a| a == "--settings").unwrap();
    let settings: Value =
        serde_json::from_slice(&fs::read(argv[settings + 1].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["defaultMode"], "manual", "{settings}");
    let mode = argv.as_array().unwrap().iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(argv[mode + 1], "manual");
    let owner = fs::read_to_string(service.temp.path().join("config/owner")).unwrap();
    assert!(owner.len() == 32 && owner.bytes().all(|b| b.is_ascii_hexdigit()), "{owner}");
    let info = service.request("session.attach_info", json!({"session_id":session["id"]})).unwrap();
    assert_eq!(
        std::path::Path::new(info["executable"].as_str().unwrap()).canonicalize().unwrap(),
        std::path::Path::new(BINARY).canonicalize().unwrap()
    );
    assert!(info["environment"].as_object().is_none_or(|e| e.is_empty()), "{info}");
    // The terminal is real: typed input reaches the program through XaYa's PTY.
    type_line(&service, &session["id"], "hello from the pane", "TYPED hello from the pane");
    assert_eq!(
        fs::read_to_string(service.temp.path().join("config/typed.txt")).unwrap(),
        "hello from the pane"
    );
    let control = service.request(
        "approval.respond",
        json!({"session_id":session["id"],"command_id":"no-controls",
            "approval_id":"none","decision":"accept"}),
    );
    assert!(control.is_err(), "an unknown approval cannot be answered");
}

#[test]
fn claude_stop_ends_every_owned_process_then_releases_the_checkout() {
    let service = Service::new();
    repository(&service);
    let first = service.start();
    let session = service.ready(&first["id"]);
    let id = &session["id"];
    let claude = session["backend_pid"].as_i64().unwrap();
    assert!(alive(claude));
    // A live Claude is never frozen.
    let cp = checkpoint(&service, id, "must-not-freeze");
    assert_ne!(cp["status"], "ready", "{cp}");
    type_line(&service, id, "platform-helper", "PLATFORM_HELPER_STARTED");
    let platform_helper: i64 =
        fs::read_to_string(service.temp.path().join("config/platform-helper.pid"))
            .unwrap()
            .parse()
            .unwrap();
    type_line(&service, id, "background", "BACKGROUND_STARTED");
    let background: i64 = service.wait(|| {
        fs::read_to_string(service.temp.path().join("config/background.pid")).ok()?.parse().ok()
    });
    // A tool made of platform programs: the shell and its child hide their environment on
    // macOS 27, yet both belong to the tool.
    type_line(&service, id, "shell-tool", "SHELL_TOOL_STARTED");
    let shell_tool: Vec<i64> = service.wait(|| {
        let text = fs::read_to_string(service.temp.path().join("config/shell-tool.pids")).ok()?;
        let pids: Vec<i64> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
        (pids.len() == 2).then_some(pids)
    });
    assert!(alive(background));
    fs::write(service.temp.path().join("source"), "Claude candidate\n").unwrap();
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    // The reparented background child would have outlived a TaskStop-style stop.
    assert!(!alive(claude) && !alive(background));
    // Hidden platform processes (macOS 27) of the launch end with it as well.
    for pid in shell_tool.iter().chain([&platform_helper]) {
        service.wait(|| (!alive(*pid)).then_some(()));
    }
    thread::sleep(Duration::from_millis(200));
    assert!(!service.temp.path().join("config/background-late").exists());
    let cp = checkpoint(&service, id, "after-confirmed-exit");
    assert_eq!(cp["status"], "ready", "confirmed exit must release the checkout: {cp}");

    let check = service.request("check.run", json!({"checkpoint_id":cp["id"],
        "command_id":"claude-check", "argv":["/bin/sh", "-c", "touch config/check-running; sleep 1"], "timeout_secs":10})).unwrap();
    service.wait(|| service.temp.path().join("config/check-running").exists().then_some(()));
    thread::scope(|scope| {
        let attempts = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    service.request(
            "session.start_claude", json!({"cwd":service.temp.path(),
                "executable":service.temp.path().join("claude"),
                "config_dir":service.temp.path().join("config"), "prompt":"must not bypass freeze"})
        )
                })
            })
            .collect::<Vec<_>>();
        for attempt in attempts {
            assert!(
                attempt.join().unwrap().is_err(),
                "concurrent starts must not bypass verification"
            );
        }
    });
    let check = service.wait(|| {
        let c = service.request("check.get", json!({"check_id":check["id"]})).unwrap();
        (!["queued", "running"].contains(&c["status"].as_str().unwrap())).then_some(c)
    });
    assert_eq!(check["status"], "succeeded", "{check}");
    git(service.temp.path(), &["branch", "claude-target"]);
    let preview = service
        .request(
            "land.preview",
            json!({"checkpoint_id":cp["id"],
        "check_id":check["id"], "target_branch":"claude-target", "command_id":"claude-land"}),
        )
        .unwrap();
    let preview = service.wait(|| {
        let p = service.request("land.get", json!({"preview_id":preview["id"]})).unwrap();
        (p["status"] != "creating").then_some(p)
    });
    assert_eq!(preview["status"], "ready", "{preview}");
    service.request("land.verify", json!({"preview_id":preview["id"]})).unwrap();
    let preview = service.wait(|| {
        let p = service.request("land.get", json!({"preview_id":preview["id"]})).unwrap();
        (p["status"] != "verifying").then_some(p)
    });
    assert_eq!(preview["integration_verified"], true, "{preview}");
    let landed = service.request("land.run", json!({"preview_id":preview["id"]})).unwrap();
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(calls(&service).iter().filter(|a| a.as_array().unwrap().len() > 1).count(), 1);
}

#[test]
fn claude_exiting_by_itself_ends_ownership_and_refuses_attach() {
    let service = Service::new();
    let started = service.start();
    let session = service.ready(&started["id"]);
    let id = &session["id"];
    let dir = host_dir(&service, id);
    type_line(&service, id, "platform-helper", "PLATFORM_HELPER_STARTED");
    let platform_helper: i64 =
        fs::read_to_string(service.temp.path().join("config/platform-helper.pid"))
            .unwrap()
            .parse()
            .unwrap();
    type_line(&service, id, "background", "BACKGROUND_STARTED");
    let background: i64 = service.wait(|| {
        fs::read_to_string(service.temp.path().join("config/background.pid")).ok()?.parse().ok()
    });
    // A tool made of platform programs: the shell and its child hide their environment on
    // macOS 27, yet both belong to the tool.
    type_line(&service, id, "shell-tool", "SHELL_TOOL_STARTED");
    let shell_tool: Vec<i64> = service.wait(|| {
        let text = fs::read_to_string(service.temp.path().join("config/shell-tool.pids")).ok()?;
        let pids: Vec<i64> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
        (pids.len() == 2).then_some(pids)
    });
    let mut attachment = pty_host::attach(&dir, pty_host::Size { rows: 24, cols: 80 }).unwrap();
    attachment.send(b"exit\r").unwrap();
    drop(attachment);
    let ended = service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "stopped").then_some(s)
    });
    assert!(ended["backend_pid"].is_null(), "{ended}");
    assert!(!alive(background), "a survivor of a finished session is not left writing");
    // Hidden platform processes (macOS 27) of the launch end with it as well.
    for pid in shell_tool.iter().chain([&platform_helper]) {
        service.wait(|| (!alive(*pid)).then_some(()));
    }
    assert!(service.request("session.attach_claim", json!({"session_id":id})).is_err());
    assert!(service.request("session.attach_info", json!({"session_id":id})).is_err());
}

#[test]
fn claude_unadoptable_launch_stays_interrupted_and_explicit_stop_recovers_ownership() {
    let mut service = Service::new();
    repository(&service);
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    let claude = first["backend_pid"].as_i64().unwrap();
    // A launch without its bridge record (e.g. from an older service) cannot be rejoined.
    let mut session = first.clone();
    session["host_dir"] = json!(host_dir(&service, id));
    let bridge = host_dir(&service, id).parent().unwrap().join("bridge.json");
    fs::remove_file(&bridge).unwrap();
    service.crash_and_restart();
    let restored = service.request("session.get", json!({"session_id":id})).unwrap();
    assert_eq!(restored["state"], "interrupted");
    assert_eq!(restored["native_thread_id"], first["native_thread_id"]);
    assert_eq!(restored["attached"], false);
    // The owned program survives a service crash, so its checkout stays owned.
    assert!(alive(claude), "the PTY host is independent of the service");
    assert_ne!(checkpoint(&service, id, "while-orphaned")["status"], "ready");
    assert!(service.request("session.resume", json!({"session_id":id})).is_err());
    assert!(service.request("session.attach_claim", json!({"session_id":id})).is_err());
    // Its Mods reach no service: native input fails closed instead of reaching the model.
    type_line_at(&service, &session, "tool orphaned", "TOOL_DENIED orphaned");
    // XaYa owns the tree, so an explicit stop can end it after a crash.
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    assert!(!alive(claude));
    assert_eq!(checkpoint(&service, id, "after-recovered-stop")["status"], "ready");
    assert_eq!(calls(&service).len(), 1, "never relaunched or replayed");
    // Once every owned process is confirmed gone, an explicit resume continues the session.
    let resumed = service.request("session.resume", json!({"session_id":id})).unwrap();
    assert_eq!(resumed["native_thread_id"], first["native_thread_id"]);
    service.ready(id);
    let launches = calls(&service);
    assert_eq!(launches.len(), 2, "{launches:?}");
    assert!(launches[1].as_array().unwrap().iter().all(|a| a != "native first turn"));
}

/// Type into a session's PTY without asking the service (it may be down).
fn type_line_at(service: &Service, session: &Value, line: &str, expect: &str) {
    let _ = service;
    let dir = session["host_dir"].as_str().map(std::path::PathBuf::from).unwrap();
    let mut attachment = pty_host::attach(&dir, pty_host::Size { rows: 24, cols: 80 }).unwrap();
    attachment.send(format!("{line}\r").as_bytes()).unwrap();
    attachment.read_until(expect, Duration::from_secs(10)).unwrap();
}

fn state(service: &Service, id: &Value, wanted: &str) -> Value {
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == wanted).then_some(s)
    })
}

/// Snapshots the fixture saw accepted. It records one only after the service has replied,
/// which is after the session already shows the rejoined state, so wait for the record.
fn syncs(service: &Service) -> Vec<Value> {
    let read = || -> Vec<Value> {
        fs::read_to_string(service.temp.path().join("config/syncs.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    };
    let snapshots = service.wait(|| Some(read()).filter(|snapshots| !snapshots.is_empty()));
    thread::sleep(Duration::from_millis(100));
    let settled = read();
    assert_eq!(settled.len(), snapshots.len(), "no further snapshot arrives");
    settled
}

#[test]
fn claude_service_crash_readopts_the_running_tui_and_its_turn_without_relaunch() {
    let mut service = Service::new();
    repository(&service);
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    let claude = first["backend_pid"].as_i64().unwrap();
    type_line(&service, id, "tool crossing-tool", "TOOL_ADMITTED crossing-tool");
    type_line(&service, id, "turn crossing", "TURN_STARTED crossing");
    state(&service, id, "working");
    service.crash_and_restart();
    // The same TUI and turn are joined again from the Mods snapshot: nothing is relaunched.
    let adopted = service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        assert!(
            !["interrupted", "failed", "stopped"].contains(&s["state"].as_str().unwrap()),
            "{s}"
        );
        (s["state"] == "working").then_some(s)
    });
    assert_eq!(adopted["turn_id"], "crossing", "{adopted}");
    assert_eq!(adopted["native_thread_id"], first["native_thread_id"]);
    assert_eq!(adopted["backend_pid"], claude);
    assert!(adopted["last_error"].is_null(), "{adopted}");
    let snapshots = syncs(&service);
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0]["activeTurn"], "crossing");
    assert_eq!(snapshots[0]["tools"][0]["tool_use_id"], "crossing-tool");
    // A tool admitted by the previous service may finish under the new one.
    type_line(&service, id, "finish crossing-tool", "TOOL_FINISHED crossing-tool");
    type_line(&service, id, "end crossing", "TURN_ENDED crossing");
    let ready = service.ready(id);
    assert!(ready["turn_id"].is_null() && ready["last_error"].is_null(), "{ready}");
    // The adopted bridge carries program sends and admissions like a fresh one.
    service
        .request(
            "session.send",
            json!({"session_id":id,"command_id":"after-adoption","text":"adopted send"}),
        )
        .unwrap();
    assert_eq!(settled(&service, id, "after-adoption")["status"], "succeeded");
    service.ready(id);
    type_line(&service, id, "tool after-adoption", "TOOL_ADMITTED after-adoption");
    type_line(&service, id, "finish after-adoption", "TOOL_FINISHED after-adoption");
    // Still a live writer: the checkout stays owned until stop confirms every process gone.
    assert_ne!(checkpoint(&service, id, "adopted-live")["status"], "ready");
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    assert!(!alive(claude));
    assert_eq!(checkpoint(&service, id, "adopted-stopped")["status"], "ready");
    assert_eq!(calls(&service).len(), 1, "never relaunched or replayed");
}

#[test]
fn claude_outage_fails_admission_closed_and_the_snapshot_reconciles_native_progress() {
    let mut service = Service::new();
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    let mut session = first.clone();
    session["host_dir"] = json!(host_dir(&service, id));
    type_line(&service, id, "turn outage", "TURN_STARTED outage");
    state(&service, id, "working");
    service.crash();
    // Nobody can admit input or tools while the service is down; the turn ends natively.
    type_line_at(&service, &session, "tool while-down", "TOOL_DENIED while-down");
    type_line_at(&service, &session, "end outage", "TURN_ENDED outage");
    service.restart();
    let ready = service.ready(id);
    assert!(ready["turn_id"].is_null(), "{ready}");
    let snapshots = syncs(&service);
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0]["activeTurn"], Value::Null);
    assert_eq!(snapshots[0]["tools"], json!([]), "a denied tool is not adopted");
    // Admission opens again once the snapshot is accepted.
    type_line(&service, id, "tool reopened", "TOOL_ADMITTED reopened");
    type_line(&service, id, "finish reopened", "TOOL_FINISHED reopened");
    assert_eq!(service.request("session.get", json!({"session_id":id})).unwrap()["state"], "ready");
    assert_eq!(calls(&service).len(), 1);
}

#[test]
fn claude_exit_while_the_service_is_down_is_settled_on_restart() {
    let mut service = Service::new();
    repository(&service);
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    let dir = host_dir(&service, id);
    type_line(&service, id, "platform-helper", "PLATFORM_HELPER_STARTED");
    let platform_helper: i64 =
        fs::read_to_string(service.temp.path().join("config/platform-helper.pid"))
            .unwrap()
            .parse()
            .unwrap();
    type_line(&service, id, "background", "BACKGROUND_STARTED");
    let background: i64 = service.wait(|| {
        fs::read_to_string(service.temp.path().join("config/background.pid")).ok()?.parse().ok()
    });
    // A tool made of platform programs: the shell and its child hide their environment on
    // macOS 27, yet both belong to the tool.
    type_line(&service, id, "shell-tool", "SHELL_TOOL_STARTED");
    let shell_tool: Vec<i64> = service.wait(|| {
        let text = fs::read_to_string(service.temp.path().join("config/shell-tool.pids")).ok()?;
        let pids: Vec<i64> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
        (pids.len() == 2).then_some(pids)
    });
    service.crash();
    let mut attachment = pty_host::attach(&dir, pty_host::Size { rows: 24, cols: 80 }).unwrap();
    attachment.send(b"exit\r").unwrap();
    drop(attachment);
    service.wait(|| pty_host::exit_status(&dir).unwrap());
    service.restart();
    let ended = state(&service, id, "stopped");
    assert!(ended["backend_pid"].is_null(), "{ended}");
    // The setsid() survivor is found by marker and ended before the checkout is released.
    assert!(!alive(background));
    // Hidden platform processes (macOS 27) of the launch end with it as well.
    for pid in shell_tool.iter().chain([&platform_helper]) {
        service.wait(|| (!alive(*pid)).then_some(()));
    }
    assert_eq!(checkpoint(&service, id, "after-offline-exit")["status"], "ready");
    assert!(service.request("session.attach_claim", json!({"session_id":id})).is_err());
    assert_eq!(calls(&service).len(), 1);
}

fn arg_after<'a>(argv: &'a Value, flag: &str) -> Option<&'a Value> {
    let argv = argv.as_array().unwrap();
    argv.iter().position(|a| a == flag).map(|i| &argv[i + 1])
}

#[test]
fn claude_resume_continues_the_same_native_session_in_a_new_owned_host() {
    let service = Service::new();
    repository(&service);
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    let native = first["native_thread_id"].clone();
    // A running session is not relaunched; resume reports it unchanged.
    let same = service.request("session.resume", json!({"session_id":id})).unwrap();
    assert_eq!(same["backend_pid"], first["backend_pid"]);
    assert_eq!(calls(&service).len(), 1);
    let old_dir = host_dir(&service, id);
    let old_owner = fs::read_to_string(service.temp.path().join("config/owner")).unwrap();
    assert_eq!(
        service.request("session.stop", json!({"session_id":id})).unwrap()["state"],
        "stopped"
    );

    let resumed = service.request("session.resume", json!({"session_id":id})).unwrap();
    assert_eq!(resumed["id"], *id);
    assert_eq!(resumed["native_thread_id"], native);
    let live = service.ready(id);
    assert_eq!(live["native_thread_id"], native, "{live}");
    assert!(live["native_job_id"].is_null());
    let pid = live["backend_pid"].as_i64().unwrap();
    assert!(alive(pid) && Some(pid) != first["backend_pid"].as_i64());
    let launches = calls(&service);
    assert_eq!(launches.len(), 2, "{launches:?}");
    let argv = &launches[1];
    assert_eq!(arg_after(argv, "--resume"), Some(&native), "{argv}");
    assert!(arg_after(argv, "--session-id").is_none(), "{argv}");
    assert!(argv.as_array().unwrap().iter().all(|a| a != "--" && a != "native first turn"));
    assert_eq!(arg_after(argv, "--permission-mode").unwrap(), "manual");
    let settings = arg_after(argv, "--settings").unwrap().as_str().unwrap();
    let settings: Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert_eq!(settings["permissions"]["defaultMode"], "manual");
    // Every launch is owned on its own: a new marker and a new private host.
    let owner = fs::read_to_string(service.temp.path().join("config/owner")).unwrap();
    assert!(owner.len() == 32 && owner != old_owner, "{owner} {old_owner}");
    let new_dir = host_dir(&service, id);
    assert_ne!(new_dir, old_dir);
    type_line(&service, id, "after resume", "TYPED after resume");
    // A live resumed Claude owns the checkout again.
    assert_ne!(checkpoint(&service, id, "resumed-is-live")["status"], "ready");
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    assert!(!alive(pid));
    assert_eq!(checkpoint(&service, id, "after-resumed-stop")["status"], "ready");
}

#[test]
fn claude_resume_refuses_a_live_or_mismatched_launch() {
    let mut service = Service::new();
    let first = service.start();
    let first = service.ready(&first["id"]);
    let id = &first["id"];
    // Exit by itself, then resume: a different identity from Mods must not be adopted.
    let mut attachment =
        pty_host::attach(&host_dir(&service, id), pty_host::Size { rows: 24, cols: 80 }).unwrap();
    attachment.send(b"exit\r").unwrap();
    drop(attachment);
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "stopped").then_some(())
    });
    fs::write(service.temp.path().join("config/resume-wrong-id"), "").unwrap();
    service.request("session.resume", json!({"session_id":id})).unwrap();
    let broken = service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "interrupted").then_some(s)
    });
    assert_eq!(broken["native_thread_id"], first["native_thread_id"], "{broken}");
    // Its processes are still owned: resume is refused until an explicit stop ends them.
    let pid = broken["backend_pid"].as_i64().or(first["backend_pid"].as_i64()).unwrap();
    assert!(service.request("session.resume", json!({"session_id":id})).is_err());
    assert_eq!(calls(&service).len(), 2);
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    fs::remove_file(service.temp.path().join("config/resume-wrong-id")).unwrap();
    // A crash while resumed keeps ownership; the running launch is adopted, not relaunched.
    service.request("session.resume", json!({"session_id":id})).unwrap();
    let live = service.ready(id);
    let resumed_pid = live["backend_pid"].as_i64().unwrap();
    service.crash_and_restart();
    assert!(alive(resumed_pid));
    // The live launch is joined again; resume returns it instead of a second writer.
    let again = service.request("session.resume", json!({"session_id":id})).unwrap();
    assert_eq!(again["backend_pid"], resumed_pid, "{again}");
    service.ready(id);
    assert_eq!(calls(&service).len(), 3, "never relaunched while owned");
    service.request("session.stop", json!({"session_id":id})).unwrap();
    assert!(!alive(resumed_pid) && !alive(pid));
}

#[test]
fn claude_does_not_accept_a_codex_sandbox_override() {
    let service = Service::new();
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["run", "--engine", "claude", "--claude"])
        .arg(service.temp.path().join("claude"))
        .args(["--sandbox", "read-only", "--detach", "--json", "explicit prompt"])
        .env("CLAUDE_CONFIG_DIR", service.temp.path().join("config"))
        .current_dir(service.temp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "Claude manual permissions must not imply a read-only sandbox"
    );
    assert!(!service.temp.path().join("config/calls.jsonl").exists());
}

#[test]
fn claude_storage_failure_does_not_launch_a_native_job() {
    let service = Service::new();
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_session BEFORE INSERT ON sessions BEGIN SELECT \
         RAISE(FAIL,'fixture'); END;",
    )
    .unwrap();
    assert!(
        service
            .request(
                "session.start_claude",
                json!({
                    "cwd":service.temp.path(), "executable":service.temp.path().join("claude"),
                    "config_dir":service.temp.path().join("config"), "prompt":"must not start"
                })
            )
            .is_err()
    );
    assert!(!service.temp.path().join("config/calls.jsonl").exists());
}

#[test]
fn codex_start_rejects_claude_manual_policy_before_spawning() {
    let service = Service::new();
    assert!(
        service
            .request(
                "session.start",
                json!({
                    "cwd":service.temp.path(), "executable":service.temp.path().join("claude"),
                    "codex_home":service.temp.path().join("config"), "sandbox":"manual"
                })
            )
            .is_err()
    );
    assert!(!service.temp.path().join("config/calls.jsonl").exists());
}

/// Every stored event after `after`, paging until the history's last sequence.
fn history(service: &Service, id: &Value, mut after: u64) -> (Value, Vec<Value>) {
    let mut events = Vec::new();
    loop {
        let page =
            service.request("events.history", json!({"session_id":id,"after_seq":after})).unwrap();
        let batch = page["events"].as_array().unwrap().clone();
        if let Some(last) = batch.last() {
            after = last["seq"].as_u64().unwrap();
        }
        events.extend(batch.iter().cloned());
        if batch.is_empty() || after >= page["last_seq"].as_u64().unwrap() {
            return (page["epoch"].clone(), events);
        }
    }
}

#[test]
fn claude_event_history_survives_a_crash_and_a_cursor_replays_it_without_gaps() {
    let mut service = Service::new();
    let started = service.start();
    let id = &started["id"];
    service.ready(id);
    let (epoch, before) = history(&service, id, 0);
    let seqs: Vec<u64> = before.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>(), "stored without gaps");
    assert!(
        before
            .iter()
            .any(|e| e["kind"] == "text_delta" && e["data"]["text"] == "CLAUDE_NATIVE_TEXT"),
        "{before:?}"
    );
    assert!(
        before.iter().any(|e| e["kind"] == "turn_completed" && e["data"]["turn_id"] == "turn-one")
    );
    let cursor = *seqs.last().unwrap();
    service.crash_and_restart();
    service.ready(id);
    let (again, after) = history(&service, id, 0);
    assert_eq!(again, epoch, "the stream identity survives a service restart");
    assert_eq!(after[..before.len()], before[..], "history before the crash is kept as it was");
    assert!(after.len() > before.len(), "the rejoined session continues the same stream");
    // A subscriber that saw everything up to `cursor` continues exactly after it.
    let mut client = Client::connect(&service.runtime).unwrap();
    let snapshot = client
        .subscribe_after(
            id.as_str().unwrap(),
            Some(xaya_agent_proto::EventCursor {
                epoch: epoch.as_str().unwrap().into(),
                seq: cursor,
            }),
        )
        .unwrap();
    assert_eq!(snapshot.epoch, epoch.as_str().unwrap());
    for expected in &after[before.len()..] {
        let event = client.next_event().unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), *expected);
    }
    type_line(&service, id, "turn live", "TURN_STARTED live");
    let live = client.next_event().unwrap();
    assert_eq!(live.seq, after.last().unwrap()["seq"].as_u64().unwrap() + 1, "{live:?}");
    // History pages after a sequence and never repeats it.
    let (_, tail) = history(&service, id, cursor);
    assert_eq!(tail[0]["seq"].as_u64().unwrap(), cursor + 1);
    // The CLI prints the same stored events, one JSON line each.
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["session", "history", id.as_str().unwrap(), "--after", &cursor.to_string()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let printed: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let (_, now) = history(&service, id, cursor);
    assert_eq!(printed, now);
}

#[test]
fn claude_cursor_outside_the_stored_history_is_refused_as_stale() {
    let service = Service::new();
    let started = service.start();
    let id = started["id"].as_str().unwrap();
    service.ready(&started["id"]);
    let (epoch, events) = history(&service, &started["id"], 0);
    let last = events.last().unwrap()["seq"].as_u64().unwrap();
    let epoch = epoch.as_str().unwrap().to_owned();
    let stale = |cursor: xaya_agent_proto::EventCursor| match Client::connect(&service.runtime)
        .unwrap()
        .subscribe_after(id, Some(cursor))
    {
        Err(xaya_agentd::Error::Rpc { code, .. }) => code,
        other => panic!("expected a stale refusal, got {:?}", other.map(|s| s.last_seq)),
    };
    let other = xaya_agent_proto::EventCursor { epoch: "0".repeat(32), seq: 1 };
    assert_eq!(stale(other), xaya_agent_proto::STALE);
    let ahead = xaya_agent_proto::EventCursor { epoch: epoch.clone(), seq: last + 5 };
    assert_eq!(stale(ahead), xaya_agent_proto::STALE);
    // A cursor at the head replays nothing and goes straight to live events.
    let mut client = Client::connect(&service.runtime).unwrap();
    let head = xaya_agent_proto::EventCursor { epoch, seq: last };
    assert!(client.subscribe_after(id, Some(head)).unwrap().last_seq >= last);
    type_line(&service, &started["id"], "turn head", "TURN_STARTED head");
    assert!(client.next_event().unwrap().seq > last);
    assert!(
        service.request("events.history", json!({"session_id":"claude-none"})).is_err(),
        "unknown session"
    );
}

#[test]
fn claude_end_tools_ends_only_tool_processes_and_keeps_the_session() {
    let service = Service::new();
    let started = service.start();
    let session = service.ready(&started["id"]);
    let id = &session["id"];
    let claude = session["backend_pid"].as_i64().unwrap();
    type_line(&service, id, "helper", "HELPER_STARTED");
    let helper: i64 =
        fs::read_to_string(service.temp.path().join("config/helper.pid")).unwrap().parse().unwrap();
    type_line(&service, id, "platform-helper", "PLATFORM_HELPER_STARTED");
    let platform_helper: i64 =
        fs::read_to_string(service.temp.path().join("config/platform-helper.pid"))
            .unwrap()
            .parse()
            .unwrap();
    type_line(&service, id, "background", "BACKGROUND_STARTED");
    let background: i64 = service.wait(|| {
        fs::read_to_string(service.temp.path().join("config/background.pid")).ok()?.parse().ok()
    });
    // A tool made of platform programs: the shell and its child hide their environment on
    // macOS 27, yet both belong to the tool.
    type_line(&service, id, "shell-tool", "SHELL_TOOL_STARTED");
    let shell_tool: Vec<i64> = service.wait(|| {
        let text = fs::read_to_string(service.temp.path().join("config/shell-tool.pids")).ok()?;
        let pids: Vec<i64> = text.split_whitespace().filter_map(|p| p.parse().ok()).collect();
        (pids.len() == 2).then_some(pids)
    });
    // A running turn's tools are not ended underneath it: cancel the turn first.
    type_line(&service, id, "turn busy", "TURN_STARTED busy");
    state(&service, id, "working");
    assert!(service.request("session.end_tools", json!({"session_id":id})).is_err());
    type_line(&service, id, "end busy", "TURN_ENDED busy");
    service.ready(id);
    let ended = service.request("session.end_tools", json!({"session_id":id})).unwrap();
    assert!(ended["ended"].as_u64().unwrap() >= 1, "{ended}");
    // The setsid() survivor of the background tool and the platform shell tool are gone;
    // Claude and its helpers, hidden or not, are not.
    assert!(!alive(background));
    for pid in &shell_tool {
        assert!(!alive(*pid), "platform tool process {pid} was ended");
    }
    assert!(
        alive(claude) && alive(helper) && alive(platform_helper),
        "only tool processes are ended"
    );
    thread::sleep(Duration::from_millis(200));
    assert!(!service.temp.path().join("config/background-late").exists());
    let after = service.request("session.get", json!({"session_id":id})).unwrap();
    assert_eq!(after["state"], "ready", "{after}");
    assert_eq!(service.request("session.end_tools", json!({"session_id":id})).unwrap()["ended"], 0);
    // The session goes on, and stop still ends everything it owns, helpers included.
    type_line(&service, id, "tool still-usable", "TOOL_ADMITTED still-usable");
    type_line(&service, id, "finish still-usable", "TOOL_FINISHED still-usable");
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped");
    assert!(!alive(helper) && !alive(platform_helper) && !alive(claude));
    assert!(service.request("session.end_tools", json!({"session_id":id})).is_err());
}

fn only_session(service: &Service) -> Value {
    let sessions = service.request("session.list", json!({})).unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1, "{sessions}");
    sessions[0].clone()
}

#[test]
fn claude_crash_before_its_host_starts_is_settled_as_never_started() {
    let mut service = Service::new();
    repository(&service);
    service.restart_crashing_at("launch.before_host");
    let _ = service.request(
        "session.start_claude",
        json!({"cwd":service.temp.path(), "executable":service.temp.path().join("claude"),
            "config_dir":service.temp.path().join("config"), "prompt":"never launched"}),
    );
    service.crashed_then_restart();
    // Nothing was launched and no owned process exists: say so, and release the checkout.
    let session = only_session(&service);
    assert_eq!(session["state"], "failed", "{session}");
    assert!(session["last_error"].as_str().unwrap().contains("before"), "{session}");
    assert!(calls(&service).is_empty());
    let id = &session["id"];
    assert_eq!(checkpoint(&service, id, "never-started")["status"], "ready");
    // There is no native conversation to resume; a new session starts normally.
    assert!(service.request("session.resume", json!({"session_id":id})).is_err());
    let next = service.start();
    service.ready(&next["id"]);
    assert_eq!(calls(&service).len(), 1);
}

#[test]
fn claude_crash_right_after_its_host_starts_still_rejoins_it() {
    let mut service = Service::new();
    service.restart_crashing_at("launch.after_host");
    let _ = service.request(
        "session.start_claude",
        json!({"cwd":service.temp.path(), "executable":service.temp.path().join("claude"),
            "config_dir":service.temp.path().join("config"), "prompt":"native first turn"}),
    );
    service.crashed_then_restart();
    // Its PID was never recorded, but its private, authenticated host identifies it.
    let id = only_session(&service)["id"].clone();
    let ready = service.ready(&id);
    let pid = ready["backend_pid"].as_i64().expect("adopted PID recorded");
    assert!(alive(pid));
    assert_eq!(calls(&service).len(), 1, "never launched twice");
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped");
    assert!(!alive(pid));
}

#[test]
fn claude_crash_in_the_middle_of_stop_finishes_the_stop_on_restart() {
    let mut service = Service::new();
    repository(&service);
    let started = service.start();
    let session = service.ready(&started["id"]);
    let id = &session["id"];
    let pid = session["backend_pid"].as_i64().unwrap();
    service.restart_crashing_at("stop.after_intent");
    service.ready(id);
    let _ = service.request("session.stop", json!({"session_id":id}));
    service.crashed_then_restart();
    // The stop that was asked for is completed, not undone by rejoining the program.
    let stopped = state(&service, id, "stopped");
    assert!(stopped["backend_pid"].is_null(), "{stopped}");
    assert!(!alive(pid));
    assert_eq!(checkpoint(&service, id, "stopped-after-crash")["status"], "ready");
    assert_eq!(calls(&service).len(), 1);
}

#[test]
fn claude_crash_while_applying_its_snapshot_rejoins_on_the_next_restart() {
    let mut service = Service::new();
    let started = service.start();
    let id = &started["id"];
    let pid = service.ready(id)["backend_pid"].as_i64().unwrap();
    service.restart_crashing_at("claude.sync");
    service.crashed_then_restart();
    let ready = service.ready(id);
    assert_eq!(ready["backend_pid"], pid);
    assert_eq!(syncs(&service).len(), 1, "the interrupted snapshot is simply sent again");
    type_line(&service, id, "tool after-resync", "TOOL_ADMITTED after-resync");
    assert_eq!(calls(&service).len(), 1);
}

#[test]
fn board_watch_pushes_session_changes_without_polling() {
    let service = Service::new();
    let mut client = Client::connect(&service.runtime).unwrap();
    assert!(client.watch_board().unwrap().is_empty());
    let started = service.start();
    let id = started["id"].as_str().unwrap();
    // Each change of any session arrives as a whole board, until the new session is ready.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let rows = client.next_board().unwrap();
        if rows.iter().any(|row| {
            row.session.id == id && row.session.state == xaya_agent_proto::SessionState::Ready
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "the board never showed the ready session");
    }
    // The CLI prints the board once, then once per change.
    let mut watch = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["session", "board", "--watch"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(watch.stdout.take().unwrap()));
    let first: Vec<xaya_agent_proto::ManagedRow> =
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(first.len(), 1);
    type_line(&service, &started["id"], "turn pushed", "TURN_STARTED pushed");
    let pushed = lines
        .map(|line| {
            serde_json::from_str::<Vec<xaya_agent_proto::ManagedRow>>(&line.unwrap()).unwrap()
        })
        .find(|rows| rows[0].session.state == xaya_agent_proto::SessionState::Working)
        .expect("the working state was pushed");
    assert_eq!(pushed[0].session.turn_id.as_deref(), Some("pushed"));
    watch.kill().unwrap();
    watch.wait().unwrap();
    // A watch holds one of the eight long-lived client slots it shares with event
    // subscriptions; a closed watch gives its slot back once the service notices (heartbeat).
    drop(client);
    let mut held = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while held.len() < 8 {
        let mut client = Client::connect(&service.runtime).unwrap();
        if client.watch_board().is_ok() {
            held.push(client);
        } else {
            assert!(Instant::now() < deadline, "closed watches never released their slots");
            thread::sleep(Duration::from_millis(50));
        }
    }
    assert!(Client::connect(&service.runtime).unwrap().watch_board().is_err(), "limit is 8");
    drop(held);
}

#[test]
fn claude_started_without_a_config_dir_keeps_its_own_default() {
    // Setting CLAUDE_CONFIG_DIR at all, even to ~/.claude, makes Claude on macOS look for its
    // login under another Keychain item. A caller who never set it must not get it set.
    let service = Service::new();
    let started = service
        .request(
            "session.start_claude",
            json!({"cwd":service.temp.path(), "executable":service.temp.path().join("claude"),
                "prompt":"default configuration"}),
        )
        .unwrap();
    let session = service.ready(&started["id"]);
    let seen = fs::read_to_string(service.temp.path().join("config/config-env")).unwrap();
    assert_eq!(seen, "<unset>", "neither passed nor inherited");
    let stopped = service.request("session.stop", json!({"session_id":session["id"]})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
}
