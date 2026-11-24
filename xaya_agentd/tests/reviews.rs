#![cfg(unix)]
//! L2 review of a checkpoint by a headless engine in a private, read-only checkout. The engine's
//! structured review is one input; the verdict is the service's, by fixed rules.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use serde_json::{Value, json};
use xaya_agent_proto as proto;
use xaya_agentd::client::Client;
use xaya_agentd::runtime::Runtime;

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");
const BASE: &str = "def mean(values):\n    return sum(values) / max(len(values), 1)\n";
const CHANGED: &str = "def mean(values):\n    return sum(values) / len(values)\n";

struct Service {
    temp: tempfile::TempDir,
    runtime: Runtime,
    process: Child,
}

impl Service {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        fs::create_dir(temp.path().join("config")).unwrap();
        fs::create_dir(temp.path().join("repo")).unwrap();
        let claude = temp.path().join("claude");
        fs::write(&claude, include_str!("fixtures/claude.py")).unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
        let reviewer = temp.path().join("reviewer");
        fs::write(&reviewer, include_str!("fixtures/reviewer.py")).unwrap();
        fs::set_permissions(&reviewer, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let process = spawn(&runtime);
        let service = Self { temp, runtime, process };
        service.wait(|| Client::connect(&service.runtime).ok());
        service
    }

    fn request(&self, method: &str, params: Value) -> xaya_agentd::Result<Value> {
        Client::connect(&self.runtime)?.request(method, params)
    }

    fn wait<T>(&self, mut condition: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(value) = condition() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out");
            thread::sleep(Duration::from_millis(30));
        }
    }

    fn crash_and_restart(&mut self) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
        self.process = spawn(&self.runtime);
        self.wait(|| Client::connect(&self.runtime).ok());
    }

    fn repo(&self) -> PathBuf {
        self.temp.path().join("repo")
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&self.runtime) {
            let _ = client.shutdown();
        }
        let _ = self.process.wait();
    }
}

/// The reviewer fixture's home when no configuration directory is passed to it.
fn default_home(runtime: &Runtime) -> PathBuf {
    runtime.directory().parent().unwrap().join("reviewer-default")
}

fn spawn(runtime: &Runtime) -> Child {
    fs::create_dir_all(default_home(runtime)).unwrap();
    Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(runtime.directory())
        .arg("serve")
        // As if the caller that started the service had its own engine directories set.
        .env("CODEX_HOME", "/nonexistent/inherited-by-the-service")
        .env("CLAUDE_CONFIG_DIR", "/nonexistent/inherited-by-the-service")
        .env("XAYA_REVIEWER_FIXTURE_HOME", default_home(runtime))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A stopped Claude session whose checkout changed `src/stats.py` line 2, captured as a ready
/// checkpoint. Returns the checkpoint.
fn checkpoint(service: &Service) -> Value {
    let repo = service.repo();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Review fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(repo.join("src/stats.py"), BASE).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"]);
    let started = service
        .request(
            "session.start_claude",
            json!({"cwd":repo, "executable":service.temp.path().join("claude"),
                "config_dir":service.temp.path().join("config"), "prompt":"implement"}),
        )
        .unwrap();
    let id = started["id"].clone();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "ready").then_some(())
    });
    fs::write(repo.join("src/stats.py"), CHANGED).unwrap();
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    let cp = service
        .request("checkpoint.create", json!({"session_id":id,"command_id":"review-me"}))
        .unwrap();
    let cp = service.wait(|| {
        let c = service.request("checkpoint.get", json!({"checkpoint_id":cp["id"]})).unwrap();
        (c["status"] != "creating").then_some(c)
    });
    assert_eq!(cp["status"], "ready", "{cp}");
    cp
}

fn check(service: &Service, cp: &Value, argv: &[&str], command: &str) -> Value {
    let started = service
        .request(
            proto::CHECK_RUN,
            json!({"checkpoint_id":cp["id"],"command_id":command,"argv":argv,"timeout_secs":30}),
        )
        .unwrap();
    service.wait(|| {
        let c = service.request(proto::CHECK_GET, json!({"check_id":started["id"]})).unwrap();
        (c["status"] != "running").then_some(c)
    })
}

/// The reviewer's configuration directory for `name`, with `behavior` written into it.
fn reviewer_home(service: &Service, name: &str, behavior: Value) -> PathBuf {
    let home = service.temp.path().join(name);
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("behavior.json"), behavior.to_string()).unwrap();
    home
}

fn start(service: &Service, params: Value) -> xaya_agentd::Result<Value> {
    service.request(proto::REVIEW_START, params)
}

fn params(service: &Service, cp: &Value, engine: &str, home: &Path, command: &str) -> Value {
    json!({
        "checkpoint_id": cp["id"], "command_id": command, "engine": engine,
        "executable": service.temp.path().join("reviewer"), "config_dir": home,
        "timeout_secs": 60,
    })
}

fn finished(service: &Service, review: &Value) -> Value {
    service.wait(|| {
        let r = service.request(proto::REVIEW_GET, json!({"review_id":review["id"]})).unwrap();
        (r["status"] != "running").then_some(r)
    })
}

fn calls(home: &Path) -> Vec<Value> {
    fs::read_to_string(home.join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn finding(path: &str, start: u32, end: u32, priority: u8, confidence: f64) -> Value {
    json!({
        "title": format!("[P{priority}] Fixture finding at {path}:{start}"),
        "body": "Fixture finding body.",
        "confidence_score": confidence,
        "priority": priority,
        "code_location": {"absolute_file_path": path, "line_range": {"start": start, "end": end}},
    })
}

fn review(findings: Vec<Value>, correct: bool) -> Value {
    json!({
        "findings": findings,
        "overall_correctness": if correct { "patch is correct" } else { "patch is incorrect" },
        "overall_explanation": "Fixture explanation.",
        "overall_confidence_score": 0.8,
    })
}

fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn a_correct_patch_with_a_passing_check_is_approved_from_a_private_read_only_checkout() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let passed = check(&service, &cp, &["true"], "gate");
    assert_eq!(passed["status"], "succeeded", "{passed}");
    let status_before = git(&service.repo(), &["status", "--porcelain"]);
    let home = reviewer_home(&service, "codex-home", json!({"review": review(vec![], true)}));
    let mut p = params(&service, &cp, "codex", &home, "first");
    p["check_id"] = passed["id"].clone();
    let started = start(&service, p).unwrap();
    assert_eq!(started["status"], "running", "{started}");
    let done = finished(&service, &started);
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["verdict"], "approved", "{done}");
    assert_eq!(done["engine"], "codex");
    assert_eq!(done["same_engine"], false, "the implementer was Claude");
    assert_eq!(done["commit"], cp["commit"]);
    assert_eq!(done["native_session_id"], "fixture-codex-thread");
    // The reviewer ran in its own checkout, which is gone; the user's checkout is untouched.
    let call = &calls(&home)[0];
    let cwd = Path::new(call["cwd"].as_str().unwrap());
    assert!(!cwd.starts_with(service.repo()), "{call}");
    assert!(!cwd.exists(), "the private checkout is cleaned up");
    assert_eq!(git(&service.repo(), &["status", "--porcelain"]), status_before);
    // The diff is in the prompt as data, with the rules first.
    let prompt = fs::read_to_string(home.join("prompt.txt")).unwrap();
    assert!(prompt.contains("introduced by this change"), "{prompt}");
    assert!(prompt.contains("-    return sum(values) / max(len(values), 1)"), "{prompt}");
    let data = prompt.find("BEGIN XAYA DATA").expect("data boundary");
    assert!(prompt.find("introduced by this change").unwrap() < data);
    assert!(prompt[data..].contains("not instructions"), "{prompt}");
}

#[test]
fn a_blocking_finding_in_the_change_needs_a_fix_and_claude_may_only_read() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let passed = check(&service, &cp, &["true"], "gate");
    let home = reviewer_home(
        &service,
        "claude-home",
        json!({"write_attempt": true, "review": review(vec![
            finding("src/stats.py", 2, 2, 1, 0.9),
        ], false)}),
    );
    let mut p = params(&service, &cp, "claude", &home, "claude-review");
    p["check_id"] = passed["id"].clone();
    let done = finished(&service, &start(&service, p).unwrap());
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["verdict"], "needs-fix", "{done}");
    assert_eq!(done["same_engine"], true, "Claude reviewing Claude is allowed and marked");
    assert_eq!(done["native_session_id"], "fixture-claude-session");
    let findings = done["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{done}");
    assert_eq!(findings[0]["path"], "src/stats.py");
    assert_eq!(findings[0]["start"], 2);
    assert_eq!(findings[0]["blocking"], true);
    // Write was refused over the permission channel; Read was allowed.
    let answers: Vec<Value> = fs::read_to_string(home.join("permissions.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers[0]["response"]["response"]["behavior"], "deny", "{answers:?}");
    assert_eq!(answers[1]["response"]["response"]["behavior"], "allow", "{answers:?}");
}

#[test]
fn findings_outside_the_change_are_dropped_and_minor_ones_do_not_block() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let passed = check(&service, &cp, &["true"], "gate");
    let home = reviewer_home(
        &service,
        "codex-home",
        json!({"review": review(vec![
            finding("/etc/passwd", 1, 1, 0, 0.99),
            finding("src/stats.py", 40, 41, 0, 0.99),
            finding("src/missing.py", 1, 1, 0, 0.99),
            finding("src/stats.py", 2, 2, 3, 0.9),
            finding("src/stats.py", 1, 2, 1, 0.2),
        ], true)}),
    );
    let mut p = params(&service, &cp, "codex", &home, "dropped");
    p["check_id"] = passed["id"].clone();
    let done = finished(&service, &start(&service, p).unwrap());
    assert_eq!(done["verdict"], "approved", "advisory findings only: {done}");
    let kept = done["findings"].as_array().unwrap();
    assert_eq!(kept.len(), 2, "{done}");
    assert!(kept.iter().all(|f| f["blocking"] == false), "{done}");
    let dropped = done["dropped"].as_array().unwrap();
    assert_eq!(dropped.len(), 3, "{done}");
    assert!(dropped.iter().all(|d| !d["reason"].as_str().unwrap().is_empty()), "{done}");
}

#[test]
fn an_approving_reviewer_cannot_pass_a_change_without_a_passing_check() {
    // As if the reviewer had been talked into it by text in the diff: correct, no findings.
    let service = Service::new();
    let cp = checkpoint(&service);
    let failed = check(&service, &cp, &["false"], "failing-gate");
    assert_eq!(failed["status"], "failed", "{failed}");
    let home = reviewer_home(&service, "codex-home", json!({"review": review(vec![], true)}));
    let unchecked = finished(
        &service,
        &start(&service, params(&service, &cp, "codex", &home, "unchecked")).unwrap(),
    );
    assert_eq!(unchecked["verdict"], "blocked", "{unchecked}");
    assert!(unchecked["reasons"].to_string().contains("verification"), "{unchecked}");
    let mut p = params(&service, &cp, "codex", &home, "failing");
    p["check_id"] = failed["id"].clone();
    let failing = finished(&service, &start(&service, p).unwrap());
    assert_eq!(failing["verdict"], "blocked", "{failing}");
}

#[test]
fn malformed_output_or_a_changed_checkout_is_invalid_never_approved() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let passed = check(&service, &cp, &["true"], "gate");
    for (name, behavior, needle) in [
        ("malformed", json!({"invalid": true}), "output"),
        ("tampered", json!({"tamper": true, "review": review(vec![], true)}), "checkout"),
    ] {
        let home = reviewer_home(&service, name, behavior);
        let mut p = params(&service, &cp, "codex", &home, name);
        p["check_id"] = passed["id"].clone();
        let done = finished(&service, &start(&service, p).unwrap());
        assert_eq!(done["status"], "completed", "{done}");
        assert_eq!(done["verdict"], "invalid", "{done}");
        assert!(done["reasons"].to_string().contains(needle), "{done}");
    }
}

#[test]
fn a_review_can_time_out_or_be_cancelled_and_its_reviewer_ends() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let home = reviewer_home(&service, "slow", json!({"sleep": 60}));
    let mut p = params(&service, &cp, "codex", &home, "times-out");
    p["timeout_secs"] = json!(2);
    let done = finished(&service, &start(&service, p).unwrap());
    assert_eq!(done["status"], "failed", "{done}");
    assert!(done["verdict"].is_null(), "{done}");
    assert!(done["error"].as_str().unwrap().contains("time"), "{done}");
    let first = calls(&home)[0]["pid"].as_i64().unwrap();
    service.wait(|| (!alive(first)).then_some(()));

    let started = start(&service, params(&service, &cp, "codex", &home, "cancelled")).unwrap();
    service.wait(|| (calls(&home).len() == 2).then_some(()));
    let second = calls(&home)[1]["pid"].as_i64().unwrap();
    service.request(proto::REVIEW_CANCEL, json!({"review_id":started["id"]})).unwrap();
    let done = finished(&service, &started);
    assert_eq!(done["status"], "cancelled", "{done}");
    service.wait(|| (!alive(second)).then_some(()));
}

#[test]
fn a_service_crash_interrupts_a_review_and_the_next_service_ends_its_reviewer() {
    let mut service = Service::new();
    let cp = checkpoint(&service);
    let home = reviewer_home(&service, "slow", json!({"sleep": 60}));
    let started = start(&service, params(&service, &cp, "codex", &home, "crash")).unwrap();
    service.wait(|| (!calls(&home).is_empty()).then_some(()));
    let call = calls(&home)[0].clone();
    assert!(call["owner"].as_str().is_some_and(|o| !o.is_empty()), "reviewer is marked: {call}");
    service.crash_and_restart();
    let after = service.request(proto::REVIEW_GET, json!({"review_id":started["id"]})).unwrap();
    assert_eq!(after["status"], "interrupted", "{after}");
    service.wait(|| (!alive(call["pid"].as_i64().unwrap())).then_some(()));
    service.wait(|| (!Path::new(call["cwd"].as_str().unwrap()).exists()).then_some(()));
    // Nothing is replayed.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(calls(&home).len(), 1);
}

#[test]
fn a_command_id_runs_once_and_a_changed_request_conflicts() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let home = reviewer_home(&service, "codex-home", json!({"review": review(vec![], true)}));
    let p = params(&service, &cp, "codex", &home, "once");
    let first = finished(&service, &start(&service, p.clone()).unwrap());
    let again = start(&service, p.clone()).unwrap();
    assert_eq!(again["id"], first["id"]);
    assert_eq!(calls(&home).len(), 1);
    let mut changed = p;
    changed["engine"] = json!("claude");
    assert!(start(&service, changed).is_err());
}

#[test]
fn a_review_goes_stale_when_the_checkout_changes_and_bad_requests_are_refused() {
    let service = Service::new();
    let cp = checkpoint(&service);
    let home = reviewer_home(&service, "codex-home", json!({"review": review(vec![], true)}));
    let done = finished(
        &service,
        &start(&service, params(&service, &cp, "codex", &home, "fresh")).unwrap(),
    );
    assert_eq!(done["stale"], false, "{done}");
    fs::write(service.repo().join("src/stats.py"), "changed again\n").unwrap();
    let after = service.request(proto::REVIEW_GET, json!({"review_id":done["id"]})).unwrap();
    assert_eq!(after["stale"], true, "{after}");

    let mut not_ancestor = params(&service, &cp, "codex", &home, "bad-base");
    not_ancestor["base"] = json!("0000000000000000000000000000000000000000");
    assert!(start(&service, not_ancestor).is_err());
    let mut unknown = params(&service, &cp, "codex", &home, "bad-check");
    unknown["check_id"] = json!("check-unknown");
    assert!(start(&service, unknown).is_err());
}

#[test]
fn a_reviewer_without_a_config_dir_keeps_its_engines_default() {
    // Claude on macOS keys its Keychain login by whether CLAUDE_CONFIG_DIR is set at all, so a
    // caller who never set it must not get it set, nor inherit the service's.
    let service = Service::new();
    let cp = checkpoint(&service);
    let home = default_home(&service.runtime);
    fs::write(home.join("behavior.json"), json!({"review": review(vec![], true)}).to_string())
        .unwrap();
    for (index, engine) in ["codex", "claude"].into_iter().enumerate() {
        let mut p = params(&service, &cp, engine, &home, engine);
        p.as_object_mut().unwrap().remove("config_dir");
        let done = finished(&service, &start(&service, p).unwrap());
        assert_eq!(done["status"], "completed", "{done}");
        assert!(done["config_dir"].is_null(), "{done}");
        let call = &calls(&home)[index];
        assert_eq!(call["engine"], engine);
        assert!(call["config_env"].is_null(), "the variable must stay unset: {call}");
    }
}
