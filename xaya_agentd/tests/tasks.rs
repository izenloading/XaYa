#![cfg(unix)]
//! Tasks: a goal and testable acceptance criteria attached to sessions. A review of a task's
//! checkpoint carries a deterministic handoff bundle and is judged criterion by criterion.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
        for dir in ["config", "repo"] {
            fs::create_dir(temp.path().join(dir)).unwrap();
        }
        for (name, source) in [
            ("claude", include_str!("fixtures/claude.py")),
            ("reviewer", include_str!("fixtures/reviewer.py")),
        ] {
            fs::write(temp.path().join(name), source).unwrap();
            fs::set_permissions(temp.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
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

    fn restart(&mut self) {
        Client::connect(&self.runtime).unwrap().shutdown().unwrap();
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

fn spawn(runtime: &Runtime) -> Child {
    spawn_at(runtime, None)
}

fn spawn_at(runtime: &Runtime, crash: Option<&str>) -> Child {
    let mut command = Command::new(BINARY);
    if let Some(point) = crash {
        command.env("XAYA_AGENTD_CRASH_AT", point);
    }
    command
        .arg("--runtime-dir")
        .arg(runtime.directory())
        .arg("serve")
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

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// A Claude fixture session in a fresh repository; returns its ID while it is still running.
fn session(service: &Service) -> Value {
    let repo = service.repo();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Task fixture"]);
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
    id
}

/// Stop `id` after changing `src/stats.py` (and adding `extra` new files), then capture it.
fn checkpoint(service: &Service, id: &Value, extra: usize) -> Value {
    let repo = service.repo();
    fs::write(repo.join("src/stats.py"), CHANGED).unwrap();
    for n in 0..extra {
        fs::write(repo.join(format!("src/extra_{n:02}.py")), format!("X = {n}\n")).unwrap();
    }
    let stopped = service.request("session.stop", json!({"session_id":id})).unwrap();
    assert_eq!(stopped["state"], "stopped", "{stopped}");
    let cp = service
        .request("checkpoint.create", json!({"session_id":id,"command_id":"for-review"}))
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

fn create(service: &Service, sessions: &[&Value]) -> xaya_agentd::Result<Value> {
    service.request(
        proto::TASK_CREATE,
        json!({
            "title": "Mean of an empty list",
            "goal": "mean() must return 0 for an empty list instead of raising.",
            "constraints": "Only change src/stats.py.",
            "criteria": ["mean([]) returns 0", "mean([2, 4]) returns 3"],
            "sessions": sessions,
        }),
    )
}

fn review_home(service: &Service, name: &str, behavior: Value) -> PathBuf {
    let home = service.temp.path().join(name);
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("behavior.json"), behavior.to_string()).unwrap();
    home
}

fn clean() -> Value {
    json!({"findings": [], "overall_correctness": "patch is correct",
           "overall_explanation": "Fixture.", "overall_confidence_score": 0.8})
}

fn results(statuses: &[(&str, &str)]) -> Value {
    statuses
        .iter()
        .map(|(id, status)| json!({"id": id, "status": status, "evidence": format!("{id} {status}")}))
        .collect()
}

fn review(
    service: &Service,
    cp: &Value,
    task: Option<&Value>,
    check: Option<&Value>,
    home: &Path,
    command: &str,
) -> xaya_agentd::Result<Value> {
    let mut params = json!({
        "checkpoint_id": cp["id"], "command_id": command, "engine": "codex",
        "executable": service.temp.path().join("reviewer"), "config_dir": home,
        "timeout_secs": 60,
    });
    if let Some(task) = task {
        params["task_id"] = task["id"].clone();
    }
    if let Some(check) = check {
        params["check_id"] = check["id"].clone();
    }
    let started = service.request(proto::REVIEW_START, params)?;
    Ok(service.wait(|| {
        let r = service.request(proto::REVIEW_GET, json!({"review_id":started["id"]})).unwrap();
        (r["status"] != "running").then_some(r)
    }))
}

#[test]
fn tasks_are_validated_and_a_session_belongs_to_one_open_task() {
    let mut service = Service::new();
    let id = session(&service);
    let bad = |params: Value| service.request(proto::TASK_CREATE, params).is_err();
    let base = json!({"title":"T","goal":"G","criteria":["c"],"sessions":[]});
    let with = |key: &str, value: Value| {
        let mut p = base.clone();
        p[key] = value;
        p
    };
    assert!(bad(with("criteria", json!([]))), "at least one criterion");
    assert!(bad(with("criteria", json!(vec!["c"; 21]))), "at most 20 criteria");
    assert!(bad(with("criteria", json!(["x".repeat(301)]))), "criteria up to 300 characters");
    assert!(bad(with("title", json!("t".repeat(121)))), "title up to 120 characters");
    assert!(bad(with("goal", json!("g".repeat(1201)))), "goal up to 1200 characters");
    assert!(bad(with("constraints", json!("c".repeat(801)))), "constraints up to 800 characters");
    assert!(bad(with("title", json!("two\nlines"))), "no control characters in a title");
    assert!(bad(with("sessions", json!(["codex-unknown"]))), "sessions must exist");

    let task = create(&service, &[&id]).unwrap();
    assert_eq!(task["status"], "open", "{task}");
    assert!(task["id"].as_str().unwrap().starts_with("task-"), "{task}");
    let criteria = task["criteria"].as_array().unwrap();
    assert_eq!(criteria[0]["id"], "AC1");
    assert_eq!(criteria[1]["id"], "AC2");
    assert_eq!(criteria[1]["hash"], sha256("mean([2, 4]) returns 3"));
    assert_eq!(task["sessions"], json!([id]));
    // One open task per session, so a review never has to guess.
    assert!(create(&service, &[&id]).is_err());
    let found = service.request(proto::TASK_FOR_SESSION, json!({"session_id":id})).unwrap();
    assert_eq!(found["id"], task["id"]);
    // Criteria are fixed once created: there is no way to edit them.
    let edit = service.request("task.update", json!({"task_id":task["id"],"criteria":["x"]}));
    assert!(edit.is_err());
    let listed = service.request(proto::TASK_LIST, json!({})).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    service.restart();
    let kept = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
    assert_eq!(kept, task, "tasks survive a restart unchanged");

    let closed = service
        .request(proto::TASK_CLOSE, json!({"task_id":task["id"],"status":"accepted"}))
        .unwrap();
    assert_eq!(closed["status"], "accepted");
    assert!(
        service
            .request(proto::TASK_CLOSE, json!({"task_id":task["id"],"status":"abandoned"}))
            .is_err(),
        "a closed task stays closed"
    );
    let none = service.request(proto::TASK_FOR_SESSION, json!({"session_id":id})).unwrap();
    assert!(none.is_null(), "{none}");
    assert!(create(&service, &[&id]).is_ok(), "the session is free for a new task");
}

#[test]
fn a_task_review_carries_a_deterministic_handoff_and_judges_each_criterion() {
    let service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 0);
    let passed = check(&service, &cp, &["true"], "gate");
    let home = review_home(
        &service,
        "met",
        json!({"review": clean(), "criteria": results(&[("AC1","met"),("AC2","met")])}),
    );
    let done = review(&service, &cp, Some(&task), Some(&passed), &home, "first").unwrap();
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["verdict"], "approved", "{done}");
    assert_eq!(done["task_id"], task["id"]);
    assert_eq!(done["criteria"].as_array().unwrap().len(), 2, "{done}");

    let handoff = &done["handoff"];
    assert_eq!(handoff["task_id"], task["id"]);
    assert_eq!(handoff["goal"], task["goal"]);
    assert_eq!(handoff["constraints"], task["constraints"]);
    assert_eq!(handoff["criteria"], task["criteria"]);
    let blob = git(&service.repo(), &[
        "rev-parse",
        &format!("{}:src/stats.py", cp["commit"].as_str().unwrap()),
    ]);
    assert_eq!(handoff["files"], json!([{"path":"src/stats.py","blob":blob}]));
    assert_eq!(handoff["truncated"], false);
    assert_eq!(handoff["hash"].as_str().unwrap().len(), 64, "{handoff}");
    assert!(!handoff.to_string().contains("implement"), "no implementer conversation: {handoff}");

    // The user's goal and criteria are instructions, before the data; the files are data.
    let prompt = fs::read_to_string(home.join("prompt.txt")).unwrap();
    let fence = prompt.find("<<<BEGIN XAYA DATA").expect("data fence");
    for trusted in ["mean() must return 0 for an empty list", "AC2: mean([2, 4]) returns 3"] {
        let at = prompt.find(trusted).unwrap_or_else(|| panic!("{trusted} missing: {prompt}"));
        assert!(at < fence, "{trusted} must come before the data fence");
    }
    assert!(prompt[fence..].contains("Changed files:"), "{prompt}");
    let schema = fs::read_to_string(home.join("schema.json")).unwrap();
    assert!(schema.contains("\"criteria\""), "task reviews require per-criterion results");

    // Same task, same checkpoint: the same bundle.
    let again = review(&service, &cp, Some(&task), Some(&passed), &home, "second").unwrap();
    assert_eq!(again["handoff"]["hash"], handoff["hash"]);
}

#[test]
fn criteria_results_decide_the_verdict_and_cannot_be_talked_round() {
    let service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 0);
    let passed = check(&service, &cp, &["true"], "gate");
    let failed = check(&service, &cp, &["false"], "failing");
    let cases: [(&str, Value, &Value, &str, &str); 6] = [
        ("unmet", results(&[("AC1", "met"), ("AC2", "unmet")]), &passed, "needs-fix", "AC2"),
        ("unknown", results(&[("AC1", "unknown"), ("AC2", "met")]), &passed, "blocked", "AC1"),
        ("missing", results(&[("AC1", "met")]), &passed, "invalid", "AC2"),
        (
            "unexpected",
            results(&[("AC1", "met"), ("AC2", "met"), ("AC9", "met")]),
            &passed,
            "invalid",
            "AC9",
        ),
        (
            "duplicate",
            results(&[("AC1", "met"), ("AC1", "met"), ("AC2", "met")]),
            &passed,
            "invalid",
            "AC1",
        ),
        // Every criterion "met", but the verification failed: still not approved.
        (
            "failing-check",
            results(&[("AC1", "met"), ("AC2", "met")]),
            &failed,
            "blocked",
            "verification",
        ),
    ];
    for (name, criteria, check, verdict, mention) in cases {
        let home = review_home(&service, name, json!({"review": clean(), "criteria": criteria}));
        let done = review(&service, &cp, Some(&task), Some(check), &home, name).unwrap();
        assert_eq!(done["status"], "completed", "{name}: {done}");
        assert_eq!(done["verdict"], verdict, "{name}: {done}");
        assert!(done["reasons"].to_string().contains(mention), "{name}: {done}");
    }
}

#[test]
fn reviews_without_a_task_are_unchanged_and_tasks_must_own_the_checkpoint() {
    let service = Service::new();
    let id = session(&service);
    let cp = checkpoint(&service, &id, 0);
    let passed = check(&service, &cp, &["true"], "gate");
    let home = review_home(&service, "plain", json!({"review": clean()}));
    let plain = review(&service, &cp, None, Some(&passed), &home, "plain").unwrap();
    assert_eq!(plain["verdict"], "approved", "{plain}");
    assert!(plain["task_id"].is_null() && plain["handoff"].is_null(), "{plain}");
    let schema = fs::read_to_string(home.join("schema.json")).unwrap();
    assert!(!schema.contains("\"criteria\""), "no criteria without a task");

    // A task that does not include the checkpoint's session cannot be used for it.
    let other = create(&service, &[]).unwrap();
    assert!(review(&service, &cp, Some(&other), Some(&passed), &home, "foreign").is_err());
    // Nor can a closed one.
    let mine = create(&service, &[&id]).unwrap();
    service.request(proto::TASK_CLOSE, json!({"task_id":mine["id"],"status":"abandoned"})).unwrap();
    assert!(review(&service, &cp, Some(&mine), Some(&passed), &home, "closed").is_err());
}

#[test]
fn a_handoff_lists_at_most_forty_files_and_says_so() {
    let service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 45);
    let home = review_home(
        &service,
        "many",
        json!({"review": clean(), "criteria": results(&[("AC1","met"),("AC2","met")])}),
    );
    let done = review(&service, &cp, Some(&task), None, &home, "many").unwrap();
    assert_eq!(done["handoff"]["files"].as_array().unwrap().len(), 40, "{done}");
    assert_eq!(done["handoff"]["truncated"], true);
}

fn finding(path: &str, line: u32, title: &str, priority: u8, confidence: f64) -> Value {
    json!({
        "title": title, "body": format!("Body of {title}."), "confidence_score": confidence,
        "priority": priority,
        "code_location": {"absolute_file_path": path, "line_range": {"start": line, "end": line}},
    })
}

fn needs_fix(findings: Vec<Value>, correct: bool) -> Value {
    json!({"findings": findings,
           "overall_correctness": if correct { "patch is correct" } else { "patch is incorrect" },
           "overall_explanation": "Empty input raises.", "overall_confidence_score": 0.8})
}

fn resume(service: &Service, id: &Value) {
    service.request("session.resume", json!({"session_id":id})).unwrap();
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "ready").then_some(())
    });
}

fn sent_texts(service: &Service) -> Vec<String> {
    fs::read_to_string(service.temp.path().join("config/controls.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|c| c["operation"] == "send")
        .map(|c| c["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn review_fixes_go_back_to_the_implementer_once_fenced_and_filtered() {
    let service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 0);
    let passed = check(&service, &cp, &["true"], "gate");
    let home = review_home(
        &service,
        "needs-fix",
        json!({
            "review": needs_fix(vec![
                finding("src/stats.py", 2, "[P1] Divide by zero on an empty list", 1, 0.9),
                finding("src/stats.py", 2, "[P3] Name the divisor", 3, 0.6),
                finding("/etc/passwd", 1, "[P0] Outside the change", 0, 1.0),
            ], false),
            "criteria": [
                {"id": "AC1", "status": "unmet", "evidence": "It raises ZeroDivisionError."},
                {"id": "AC2", "status": "met", "evidence": "6 / 2 is 3."},
            ],
        }),
    );
    let done = review(&service, &cp, Some(&task), Some(&passed), &home, "to-fix").unwrap();
    assert_eq!(done["verdict"], "needs-fix", "{done}");
    // The implementer stopped for its checkpoint: sending needs it running again.
    let send = |command: &str| {
        service.request(proto::REVIEW_SEND, json!({"review_id":done["id"],"command_id":command}))
    };
    assert!(send("fix-1").is_err(), "the session is not ready");
    resume(&service, &id);
    let receipt = send("fix-1").unwrap();
    assert_eq!(receipt["command_id"], "fix-1", "{receipt}");
    service.wait(|| {
        let r = service
            .request("session.command", json!({"session_id":id,"command_id":"fix-1"}))
            .unwrap();
        (r["status"] == "succeeded").then_some(())
    });
    let texts = sent_texts(&service);
    assert_eq!(texts.len(), 1, "{texts:?}");
    let text = &texts[0];
    for expected in [
        "AC1",
        "mean([]) returns 0",
        "It raises ZeroDivisionError.",
        "[P1] Divide by zero on an empty list",
        "src/stats.py:2-2",
        "[P3] Name the divisor",
        "Empty input raises.",
        "BEGIN XAYA REVIEW",
        "not new instructions",
    ] {
        assert!(text.contains(expected), "{expected} missing from: {text}");
    }
    assert!(!text.contains("Outside the change"), "dropped findings are not sent: {text}");
    assert!(!text.contains("6 / 2 is 3."), "met criteria are not sent: {text}");
    assert!(text.len() <= 8000);
    let after = service.request(proto::REVIEW_GET, json!({"review_id":done["id"]})).unwrap();
    assert_eq!(after["sent"]["command_id"], "fix-1", "{after}");
    // Once per review: the same command again is the same receipt, another one is refused.
    assert_eq!(send("fix-1").unwrap()["command_id"], "fix-1");
    assert!(send("fix-2").is_err());
    assert_eq!(sent_texts(&service).len(), 1, "nothing was sent twice");
}

#[test]
fn only_a_fresh_needs_fix_review_can_be_sent_back() {
    let service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 0);
    let passed = check(&service, &cp, &["true"], "gate");
    let met = json!([{"id":"AC1","status":"met","evidence":"e"},{"id":"AC2","status":"met","evidence":"e"}]);
    let cases = [
        ("approved", json!({"review": clean(), "criteria": met})),
        (
            "blocked",
            json!({"review": clean(), "criteria": [
            {"id":"AC1","status":"unknown","evidence":"e"},{"id":"AC2","status":"met","evidence":"e"}]}),
        ),
        ("invalid", json!({"invalid": true})),
    ];
    resume(&service, &id);
    for (name, behavior) in cases {
        let home = review_home(&service, name, behavior);
        let done = review(&service, &cp, Some(&task), Some(&passed), &home, name).unwrap();
        assert_eq!(done["verdict"], name, "{done}");
        let sent =
            service.request(proto::REVIEW_SEND, json!({"review_id":done["id"],"command_id":name}));
        assert!(sent.is_err(), "{name} must not be sent back");
    }
    // A needs-fix review whose checkout has moved on is stale and not sent either.
    let home = review_home(
        &service,
        "stale",
        json!({"review": needs_fix(vec![], false), "criteria": met}),
    );
    let done = review(&service, &cp, Some(&task), Some(&passed), &home, "stale").unwrap();
    assert_eq!(done["verdict"], "needs-fix", "{done}");
    fs::write(service.repo().join("src/stats.py"), "changed again\n").unwrap();
    let sent =
        service.request(proto::REVIEW_SEND, json!({"review_id":done["id"],"command_id":"stale"}));
    assert!(sent.is_err(), "stale reviews are not sent back");
    assert!(sent_texts(&service).is_empty());
}

#[test]
fn the_task_ledger_tracks_findings_across_reviews() {
    let mut service = Service::new();
    let id = session(&service);
    let task = create(&service, &[&id]).unwrap();
    let cp = checkpoint(&service, &id, 0);
    let met = json!([{"id":"AC1","status":"met","evidence":"e"},{"id":"AC2","status":"met","evidence":"e"}]);
    let alpha = finding("src/stats.py", 2, "[P1] Alpha problem", 1, 0.9);
    let beta = finding("src/stats.py", 2, "[P2] Beta  Problem", 2, 0.9);
    // The same finding with another priority, case and spacing.
    let beta_again = finding("src/stats.py", 1, "[P3] beta problem", 3, 0.9);
    let gamma = finding("src/stats.py", 2, "[P2] Gamma problem", 2, 0.9);
    let rounds = [
        ("r1", json!({"review": needs_fix(vec![alpha.clone(), beta], true), "criteria": met})),
        ("r2", json!({"review": needs_fix(vec![beta_again, gamma], true), "criteria": met})),
        ("r3", json!({"review": needs_fix(vec![alpha], true), "criteria": met})),
        // An invalid review says nothing about the findings.
        ("r4", json!({"invalid": true})),
    ];
    let mut ids = Vec::new();
    for (name, behavior) in rounds {
        let home = review_home(&service, name, behavior);
        ids.push(review(&service, &cp, Some(&task), None, &home, name).unwrap()["id"].clone());
    }
    // Reviews without the task leave its ledger alone.
    let home =
        review_home(&service, "plain", json!({"review": needs_fix(vec![gamma_like()], true)}));
    review(&service, &cp, None, None, &home, "plain").unwrap();

    let check_ledger = |task: &Value| {
        let ledger = task["ledger"].as_array().unwrap();
        assert_eq!(ledger.len(), 3, "{task}");
        let entry = |needle: &str| {
            ledger
                .iter()
                .find(|e| e["title"].as_str().unwrap().to_lowercase().contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {task}"))
                .clone()
        };
        let (a, b, c) = (entry("alpha"), entry("beta"), entry("gamma"));
        assert_eq!((a["status"].as_str(), a["seen"].as_u64()), (Some("reappeared"), Some(2)));
        assert_eq!(
            (a["first_review"].clone(), a["last_review"].clone()),
            (ids[0].clone(), ids[2].clone())
        );
        assert_eq!(a["blocking"], true);
        assert_eq!((b["status"].as_str(), b["seen"].as_u64()), (Some("resolved"), Some(2)));
        assert_eq!(b["last_review"], ids[1]);
        assert_eq!((c["status"].as_str(), c["seen"].as_u64()), (Some("resolved"), Some(1)));
        assert_eq!(a["path"], "src/stats.py");
    };
    check_ledger(&service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap());
    service.restart();
    check_ledger(&service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap());
}

fn gamma_like() -> Value {
    finding("src/stats.py", 2, "[P2] Delta problem", 2, 0.9)
}

#[test]
fn reviews_are_listed_by_session_newest_first() {
    let service = Service::new();
    let id = session(&service);
    let cp = checkpoint(&service, &id, 0);
    let home = review_home(&service, "plain", json!({"review": clean()}));
    let first = review(&service, &cp, None, None, &home, "first").unwrap();
    thread::sleep(Duration::from_millis(5));
    let second = review(&service, &cp, None, None, &home, "second").unwrap();
    let listed = service.request(proto::REVIEW_LIST, json!({"session_id":id})).unwrap();
    let ids: Vec<&Value> = listed.as_array().unwrap().iter().map(|r| &r["id"]).collect();
    assert_eq!(ids, [&second["id"], &first["id"]]);
    let none = service.request(proto::REVIEW_LIST, json!({"session_id":"claude-other"})).unwrap();
    assert!(none.as_array().unwrap().is_empty());
}

fn create_with(service: &Service, sessions: &[&Value], budget: u32) -> Value {
    service
        .request(
            proto::TASK_CREATE,
            json!({"title":"Loop","goal":"Fix mean","criteria":["mean([]) returns 0"],
                   "sessions":sessions,"fix_budget":budget}),
        )
        .unwrap()
}

/// After the implementer's turn: change the code, stop it and capture a new checkpoint.
fn recheckpoint(service: &Service, id: &Value, content: &str, command: &str) -> Value {
    service.wait(|| {
        let s = service.request("session.get", json!({"session_id":id})).unwrap();
        (s["state"] == "ready").then_some(())
    });
    fs::write(service.repo().join("src/stats.py"), content).unwrap();
    service.request("session.stop", json!({"session_id":id})).unwrap();
    let cp = service
        .request("checkpoint.create", json!({"session_id":id,"command_id":command}))
        .unwrap();
    service.wait(|| {
        let c = service.request("checkpoint.get", json!({"checkpoint_id":cp["id"]})).unwrap();
        (c["status"] == "ready").then_some(c)
    })
}

fn met_one() -> Value {
    json!([{"id":"AC1","status":"met","evidence":"e"}])
}

fn blocking(names: &[&str]) -> Value {
    json!({"review": needs_fix(
        names.iter().map(|n| finding("src/stats.py", 2, &format!("[P1] {n}"), 1, 0.9)).collect(),
        true,
    ), "criteria": met_one()})
}

fn task_of(service: &Service, task: &Value) -> Value {
    service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap()
}

fn send_review(service: &Service, review: &Value, command: &str) -> xaya_agentd::Result<Value> {
    service.request(proto::REVIEW_SEND, json!({"review_id":review["id"],"command_id":command}))
}

#[test]
fn the_fix_budget_limits_rounds_until_a_person_continues() {
    let service = Service::new();
    let id = session(&service);
    let task = create_with(&service, &[&id], 1);
    assert_eq!(task["fix_budget"], 1);
    let cp = checkpoint(&service, &id, 0);
    let r1 = review(
        &service,
        &cp,
        Some(&task),
        None,
        &review_home(&service, "r1", blocking(&["Alpha"])),
        "r1",
    )
    .unwrap();
    resume(&service, &id);
    send_review(&service, &r1, "fix-1").unwrap();
    assert_eq!(task_of(&service, &task)["fix_rounds"].as_array().unwrap().len(), 1);

    let cp2 = recheckpoint(&service, &id, "def mean(v):\n    return 0\n", "second");
    let r2 = review(
        &service,
        &cp2,
        Some(&task),
        None,
        &review_home(&service, "r2", blocking(&["Beta"])),
        "r2",
    )
    .unwrap();
    resume(&service, &id);
    assert!(send_review(&service, &r2, "fix-2").is_err(), "the budget is used up");
    let stuck = task_of(&service, &task);
    assert!(stuck["attention"]["reason"].as_str().unwrap().contains("budget"), "{stuck}");
    assert_eq!(stuck["fix_rounds"].as_array().unwrap().len(), 1, "a refused send is no round");

    let continued =
        service.request(proto::TASK_CONTINUE, json!({"task_id":task["id"],"rounds":1})).unwrap();
    assert!(continued["attention"].is_null() && continued["fix_budget"] == 2, "{continued}");
    send_review(&service, &r2, "fix-2").unwrap();
    assert_eq!(task_of(&service, &task)["fix_rounds"].as_array().unwrap().len(), 2);
    // Nothing to continue from, and no rounds to add: refused.
    assert!(
        service.request(proto::TASK_CONTINUE, json!({"task_id":task["id"],"rounds":0})).is_err()
    );
}

#[test]
fn two_invalid_reviews_in_a_row_need_attention() {
    let service = Service::new();
    let id = session(&service);
    let task = create_with(&service, &[&id], 2);
    let cp = checkpoint(&service, &id, 0);
    let invalid = json!({"invalid": true});
    let valid = json!({"review": clean(), "criteria": met_one()});
    for (name, behavior) in [("a", &invalid), ("b", &valid), ("c", &invalid)] {
        review(
            &service,
            &cp,
            Some(&task),
            None,
            &review_home(&service, name, behavior.clone()),
            name,
        )
        .unwrap();
    }
    assert!(task_of(&service, &task)["attention"].is_null(), "not in a row");
    review(&service, &cp, Some(&task), None, &review_home(&service, "d", invalid), "d").unwrap();
    let stuck = task_of(&service, &task);
    assert!(stuck["attention"]["reason"].as_str().unwrap().contains("invalid"), "{stuck}");
}

#[test]
fn a_fix_round_that_changed_nothing_needs_attention() {
    let service = Service::new();
    let id = session(&service);
    let task = create_with(&service, &[&id], 2);
    let cp = checkpoint(&service, &id, 0);
    let r1 = review(
        &service,
        &cp,
        Some(&task),
        None,
        &review_home(&service, "r1", blocking(&["Alpha"])),
        "r1",
    )
    .unwrap();
    resume(&service, &id);
    send_review(&service, &r1, "fix-1").unwrap();
    // Reviewed again on the very same tree: the round changed nothing.
    let r2 = review(
        &service,
        &cp,
        Some(&task),
        None,
        &review_home(&service, "r2", blocking(&["Beta"])),
        "r2",
    )
    .unwrap();
    let stuck = task_of(&service, &task);
    assert!(stuck["attention"]["reason"].as_str().unwrap().contains("changed nothing"), "{stuck}");
    assert_eq!(stuck["attention"]["review_id"], r2["id"]);
    assert!(send_review(&service, &r2, "fix-2").is_err(), "no sends while it needs attention");
}

#[test]
fn a_blocking_finding_that_survives_a_round_needs_attention_unless_disputed() {
    for disputed in [false, true] {
        let service = Service::new();
        let id = session(&service);
        let task = create_with(&service, &[&id], 3);
        let cp = checkpoint(&service, &id, 0);
        let r1 = review(
            &service,
            &cp,
            Some(&task),
            None,
            &review_home(&service, "r1", blocking(&["Alpha", "Beta"])),
            "r1",
        )
        .unwrap();
        if disputed {
            service
                .request(
                    proto::REVIEW_DISPUTE,
                    json!({"review_id":r1["id"],"finding":1,"reason":"Alpha is intended."}),
                )
                .unwrap();
        }
        resume(&service, &id);
        send_review(&service, &r1, "fix-1").unwrap();
        let cp2 = recheckpoint(&service, &id, "def mean(v):\n    return 1\n", "second");
        review(
            &service,
            &cp2,
            Some(&task),
            None,
            &review_home(&service, "r2", blocking(&["Alpha"])),
            "r2",
        )
        .unwrap();
        let after = task_of(&service, &task);
        if disputed {
            assert!(
                after["attention"].is_null(),
                "a disputed finding is not a failed round: {after}"
            );
        } else {
            assert!(after["attention"]["reason"].as_str().unwrap().contains("Alpha"), "{after}");
        }
    }
}

#[test]
fn disputes_are_recorded_left_out_of_fixes_and_never_change_a_verdict() {
    let service = Service::new();
    let id = session(&service);
    let task = create_with(&service, &[&id], 2);
    let cp = checkpoint(&service, &id, 0);
    let r1 = review(
        &service,
        &cp,
        Some(&task),
        None,
        &review_home(&service, "r1", blocking(&["Alpha", "Beta"])),
        "r1",
    )
    .unwrap();
    let dispute = |finding: Value, reason: &str| {
        service.request(
            proto::REVIEW_DISPUTE,
            json!({"review_id":r1["id"],"finding":finding,"reason":reason}),
        )
    };
    assert!(dispute(json!(9), "out of range").is_err());
    assert!(dispute(json!(0), "numbers start at 1").is_err());
    assert!(dispute(json!(1), "").is_err(), "a reason is required");
    dispute(json!(1), "Alpha is intended.").unwrap();
    let ledger = task_of(&service, &task)["ledger"].clone();
    let alpha = ledger
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"].as_str().unwrap().contains("alpha"))
        .unwrap()
        .clone();
    assert_eq!(alpha["status"], "disputed", "{ledger}");
    assert_eq!(alpha["dispute"]["reason"], "Alpha is intended.");
    assert_eq!(alpha["dispute"]["review_id"], r1["id"]);
    let unchanged = service.request(proto::REVIEW_GET, json!({"review_id":r1["id"]})).unwrap();
    assert_eq!(unchanged["verdict"], "needs-fix", "a dispute never changes a verdict");
    resume(&service, &id);
    send_review(&service, &r1, "fix-1").unwrap();
    // The fixture picks the command up on its next poll.
    let texts = service.wait(|| Some(sent_texts(&service)).filter(|texts| !texts.is_empty()));
    assert!(texts[0].contains("Beta") && !texts[0].contains("Alpha"), "{texts:?}");

    // Only disputed findings left: nothing to send; a person decides.
    let service = Service::new();
    let id = session(&service);
    let task = create_with(&service, &[&id], 2);
    let cp = checkpoint(&service, &id, 0);
    let only = review(
        &service,
        &cp,
        Some(&task),
        None,
        &review_home(&service, "only", blocking(&["Alpha"])),
        "only",
    )
    .unwrap();
    service
        .request(
            proto::REVIEW_DISPUTE,
            json!({"review_id":only["id"],"finding":1,"reason":"Intended."}),
        )
        .unwrap();
    resume(&service, &id);
    assert!(send_review(&service, &only, "fix-1").is_err());
    let stuck = task_of(&service, &task);
    assert!(stuck["attention"]["reason"].as_str().unwrap().contains("disputed"), "{stuck}");
    assert!(sent_texts(&service).is_empty());
}

fn cycle_fixture(service: &Service, budget: u32, dual: bool) -> Value {
    let id = session(service);
    let task = service
        .request(
            proto::TASK_CREATE,
            json!({"title":"Cycle fixture","goal":"Fix the change","criteria":["AC1","AC2"],
                "sessions":[id],"fix_budget":budget,"dual_review":dual}),
        )
        .unwrap();
    let root = service.repo();
    fs::create_dir(root.join(".xaya")).unwrap();
    fs::write(
        root.join(".xaya/project.toml"),
        "[verify]\nsteps = [{name = 'fixture', argv = ['/usr/bin/true']}]\n",
    )
    .unwrap();
    let project = service.request(proto::PROJECT_GET, json!({"cwd":root})).unwrap();
    service
        .request(proto::PROJECT_TRUST, json!({"cwd":root,"config_hash":project["config_hash"]}))
        .unwrap();
    fs::write(root.join("src/stats.py"), CHANGED).unwrap();
    task
}

fn run_cycle(service: &Service, task: &Value, first: &str) -> (bool, Value) {
    let home = review_home(
        service,
        "cycle-review",
        json!({"review":clean(),"criteria":results(&[("AC1",first),("AC2","met")])}),
    );
    let second_home = review_home(
        service,
        "cycle-review-claude",
        json!({"review":clean(),"criteria":results(&[("AC1","met"),("AC2","met")])}),
    );
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "cycle", task["id"].as_str().unwrap(), "--codex"])
        .arg(service.temp.path().join("reviewer"))
        .arg("--claude")
        .arg(service.temp.path().join("reviewer"))
        .env("CODEX_HOME", home)
        .env("CLAUDE_CONFIG_DIR", second_home)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    (output.status.success(), report)
}

#[test]
fn task_cycle_waits_for_the_fix_turn_and_stops_on_the_same_tree() {
    let service = Service::new();
    let task = cycle_fixture(&service, 2, false);
    let (success, report) = run_cycle(&service, &task, "unmet");
    assert!(!success, "an unchanged fix cannot approve");
    assert_eq!(report["outcome"], "needs_attention", "{report}");
    assert_eq!(report["rounds"].as_array().unwrap().len(), 2, "{report}");
    assert_eq!(report["rounds"][0]["turn_id"], "send-1", "{report}");
    let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
    assert_eq!(saved["fix_rounds"].as_array().unwrap().len(), 1, "{saved}");
    assert!(saved["attention"]["reason"].as_str().unwrap().contains("same tree"), "{saved}");
}

#[test]
fn task_cycle_can_approve_with_no_fix_budget() {
    let service = Service::new();
    let task = cycle_fixture(&service, 0, false);
    let (success, report) = run_cycle(&service, &task, "met");
    assert!(success, "{report}");
    assert_eq!(report["outcome"], "approved", "{report}");
    assert_eq!(report["rounds"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["rounds"][0]["verdict"], "approved", "{report}");
    assert!(report["rounds"][0]["command_id"].is_null(), "{report}");
}

#[test]
fn task_cycle_does_not_dispatch_after_its_fix_budget_is_exhausted() {
    let service = Service::new();
    let task = cycle_fixture(&service, 0, false);
    let (success, report) = run_cycle(&service, &task, "unmet");
    assert!(!success, "{report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["reason"], "the task's fix budget is used up", "{report}");
    assert_eq!(report["rounds"][0]["verdict"], "needs-fix", "{report}");
    assert!(report["rounds"][0]["command_id"].is_null(), "{report}");
    let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
    assert!(saved["fix_rounds"].as_array().unwrap().is_empty(), "{saved}");
}

#[test]
fn a_dual_review_task_merges_findings_and_records_both_engines() {
    let mut service = Service::new();
    let session = session(&service);
    let task = service
        .request(
            proto::TASK_CREATE,
            json!({
                "title":"Dual review","goal":"Fix mean","criteria":["Mean works"],
                "sessions":[session],"dual_review":true
            }),
        )
        .unwrap();
    let cp = checkpoint(&service, &session, 0);
    let verification = check(&service, &cp, &["/usr/bin/true"], "dual-check");
    let fixture = |title: &str, status: &str, usage: Value| {
        let mut answer = clean();
        answer["findings"] = Value::Array(
            (0..20)
                .map(|index| {
                    json!({
                        "title":format!("{title} {index}"), "body":"Fixture ".repeat(220),
                        "priority":1, "confidence_score":0.95,
                        "code_location":{"absolute_file_path":"src/stats.py",
                        "line_range":{"start":2,"end":2}}
                    })
                })
                .collect(),
        );
        json!({"review":answer,"criteria":results(&[("AC1",status)]),"usage":usage})
    };
    let codex = review_home(
        &service,
        "dual-codex",
        fixture(
            "First finding",
            "met",
            json!({"input_tokens":30,"cached_input_tokens":20,"output_tokens":4}),
        ),
    );
    let claude = review_home(
        &service,
        "dual-claude",
        fixture(
            "Second finding",
            "unmet",
            json!({"input_tokens":3,"cache_read_input_tokens":15,"cache_creation_input_tokens":10,
            "output_tokens":5}),
        ),
    );
    assert!(review(&service, &cp, Some(&task), Some(&verification), &codex, "single").is_err());
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args([
            "review",
            "run",
            cp["id"].as_str().unwrap(),
            "--check",
            verification["id"].as_str().unwrap(),
        ])
        .arg("--codex")
        .arg(service.temp.path().join("reviewer"))
        .arg("--claude")
        .arg(service.temp.path().join("reviewer"))
        .env("CODEX_HOME", codex)
        .env("CLAUDE_CONFIG_DIR", claude)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(report["verdict"], "needs-fix", "{report}");
    assert_eq!(report["findings"].as_array().unwrap().len(), 40, "{report}");
    assert_eq!(report["criteria"][0]["status"], "unmet", "{report}");
    assert_eq!(report["reviewers"][0]["usage"]["input_tokens"], 30, "{report}");
    assert_eq!(report["reviewers"][1]["usage"]["cache_creation_input_tokens"], 10, "{report}");
    service.restart();
    let restored = service.request(proto::REVIEW_GET, json!({"review_id":report["id"]})).unwrap();
    assert_eq!(restored["reviewers"], report["reviewers"]);
    let listed = service.request(proto::REVIEW_LIST, json!({"task_id":task["id"]})).unwrap();
    assert_eq!(listed[0]["findings"], report["findings"]);
    let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
    assert_eq!(saved["ledger"].as_array().unwrap().len(), 40, "{saved}");
}

#[test]
fn task_cycle_uses_both_reviewers_when_the_task_requires_them() {
    let service = Service::new();
    let task = cycle_fixture(&service, 0, true);
    let (success, report) = run_cycle(&service, &task, "met");
    assert!(success, "{report}");
    let review = service
        .request(proto::REVIEW_GET, json!({"review_id":report["rounds"][0]["review_id"]}))
        .unwrap();
    assert_eq!(review["reviewers"][0]["engine"], "codex", "{review}");
    assert_eq!(review["reviewers"][1]["engine"], "claude", "{review}");
    assert_eq!(report["outcome"], "approved", "{report}");
}

#[test]
fn task_prerequisites_are_validated_and_survive_restart() {
    let mut service = Service::new();
    let mut params = json!({"title":"A","goal":"A","criteria":["passes"]});
    let first = service.request(proto::TASK_CREATE, params.clone()).unwrap();
    assert!(first.get("depends_on").is_none());
    params["title"] = json!("B");
    params["depends_on"] = json!(["missing"]);
    assert!(service.request(proto::TASK_CREATE, params.clone()).is_err());
    params["depends_on"] = json!([first["id"], first["id"]]);
    let second = service.request(proto::TASK_CREATE, params).unwrap();
    service.restart();
    let restored = service.request(proto::TASK_GET, json!({"task_id":second["id"]})).unwrap();
    let query = json!({"task_id":second["id"]});
    let snapshot = service.request(proto::TASK_DEPENDENCIES, query.clone()).unwrap();
    assert_eq!(snapshot[0]["state"]["kind"], "awaiting_landing");
    service
        .request(proto::TASK_CLOSE, json!({"task_id":first["id"],"status":"abandoned"}))
        .unwrap();
    let snapshot = service.request(proto::TASK_DEPENDENCIES, query).unwrap();
    assert_eq!(snapshot[0]["state"]["kind"], "abandoned");
    assert_eq!(restored["depends_on"], json!([first["id"]]));
}

#[test]
fn a_reviewed_prerequisite_must_land_and_be_in_the_dependent_checkout() {
    let service = Service::new();
    let session = session(&service);
    let prior = create(&service, &[&session]).unwrap();
    let cp = checkpoint(&service, &session, 0);
    let gate = check(&service, &cp, &["true"], "dependency-gate");
    let home = review_home(
        &service,
        "approved",
        json!({
            "review":clean(), "criteria":results(&[("AC1","met"),("AC2","met")])
        }),
    );
    let reviewed = review(&service, &cp, Some(&prior), Some(&gate), &home, "review").unwrap();
    assert_eq!(reviewed["verdict"], "approved", "{reviewed}");
    service.request(proto::TASK_CLOSE, json!({"task_id":prior["id"],"status":"accepted"})).unwrap();
    let next = service
        .request(
            proto::TASK_CREATE,
            json!({"title":"B","goal":"B","criteria":["passes"],
        "sessions":[session], "depends_on":[prior["id"]]}),
        )
        .unwrap();
    let launch_task = service
        .request(
            proto::TASK_CREATE,
            json!({
                "title":"Launch dependent", "goal":"B", "criteria":["passes"],
                "project":service.repo(), "depends_on":[prior["id"]]
            }),
        )
        .unwrap();
    let launch = json!({"task_id":launch_task["id"], "request_id":"dependent", "claude":{
        "cwd":service.repo(), "executable":service.temp.path().join("claude"),
        "config_dir":service.temp.path().join("config"), "prompt":"implement"
    }});
    assert!(service.request(proto::TASK_START, launch.clone()).is_err());
    let state = || {
        service.request(proto::TASK_DEPENDENCIES, json!({"task_id":next["id"]})).unwrap()[0]
            ["state"]
            .clone()
    };
    assert_eq!(state()["kind"], "awaiting_landing");
    git(&service.repo(), &["branch", "integration"]);
    let preview = service
        .request(
            proto::LAND_PREVIEW,
            json!({"checkpoint_id":cp["id"],
        "check_id":gate["id"], "target_branch":"integration", "command_id":"dependency-land"}),
        )
        .unwrap();
    let params = json!({"preview_id":preview["id"]});
    let wait = |busy: &str| {
        service.wait(|| {
            let p = service.request(proto::LAND_GET, params.clone()).unwrap();
            (p["status"] != busy).then_some(p)
        })
    };
    assert_eq!(wait("creating")["status"], "ready");
    service.request(proto::LAND_VERIFY, params.clone()).unwrap();
    assert_eq!(wait("verifying")["integration_verified"], true);
    let landed = service.request(proto::LAND_RUN, params).unwrap();
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(
        state(),
        json!({"kind":"checkout_behind","session_id":session,"commit":landed["candidate_commit"]})
    );
    assert!(service.request(proto::TASK_START, launch.clone()).is_err());
    let unclaimed = service.request(proto::TASK_GET, json!({"task_id":launch_task["id"]})).unwrap();
    assert!(unclaimed["launch"].is_null());
    service.request(proto::SESSION_STOP, json!({"session_id":session})).unwrap();
    git(&service.repo(), &["switch", "--discard-changes", "integration"]);
    assert_eq!(state(), json!({"kind":"included","commit":landed["candidate_commit"]}));
    let started = service.request(proto::TASK_START, launch.clone()).unwrap();
    assert_eq!(service.request(proto::TASK_START, launch).unwrap()["id"], started["id"]);
}

#[test]
fn project_board_groups_worktrees_and_samples_capacity_without_dispatch() {
    let mut service = Service::new();
    let id = session(&service);
    let manual = create(&service, &[&id]).unwrap();
    let other = service.temp.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-q"]);
    let worktree = service.temp.path().join("linked");
    git(&service.repo(), &["worktree", "add", "--detach", worktree.to_str().unwrap()]);
    let new = |title: &str, cwd: &Path, deps: Value| {
        service.request(proto::TASK_CREATE,
        json!({"title":title,"goal":title,"criteria":["passes"],"project":cwd,"depends_on":deps})).unwrap()
    };
    let a = new("A", &worktree, json!([]));
    let b = new("B", &service.repo(), json!([a["id"]]));
    let _other = new("unrelated", &other, json!([]));
    let params = json!({"cwd":worktree.join("src")});
    let board = service.request(proto::TASK_BOARD, params.clone()).unwrap();
    assert_eq!(board["tasks"].as_array().unwrap().len(), 3);
    assert_eq!(board["available"], 0);
    let state = |board: &Value, task: &Value| {
        board["tasks"].as_array().unwrap().iter().find(|row| row["task_id"] == task["id"]).unwrap()
            ["state"]
            .clone()
    };
    assert_eq!(state(&board, &manual), "manual");
    assert_eq!(state(&board, &a), "waiting_capacity");
    assert_eq!(state(&board, &b), "waiting_dependency");
    service.request("session.stop", json!({"session_id": id})).unwrap();
    service.restart();
    let board = service.request(proto::TASK_BOARD, params.clone()).unwrap();
    assert_eq!(board["available"], 1);
    assert_eq!(state(&board, &a), "eligible");
    assert_eq!(service.request(proto::TASK_BOARD, params).unwrap(), board);
}

#[test]
fn task_launch_is_claimed_once_and_reuses_its_session_after_restart() {
    let mut service = Service::new();
    let previous = session(&service);
    service.request("session.stop", json!({"session_id":previous})).unwrap();
    let task = service
        .request(
            proto::TASK_CREATE,
            json!({
                "title":"Launch","goal":"Launch","criteria":["passes"],"project":service.repo()
            }),
        )
        .unwrap();
    let params = |request_id: &str| {
        json!({
            "task_id":task["id"],"request_id":request_id,"claude":{
                "cwd":service.repo(),"executable":service.temp.path().join("claude"),
                "config_dir":service.temp.path().join("config"),"prompt":"implement"
            }
        })
    };
    let claims = thread::scope(|scope| {
        let first = scope.spawn(|| service.request(proto::TASK_START, params("first")));
        let second = scope.spawn(|| service.request(proto::TASK_START, params("second")));
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(claims.iter().filter(|claim| claim.is_ok()).count(), 1);
    let launch =
        service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap()["launch"].clone();
    let replay = params(launch["request_id"].as_str().unwrap());
    let id = launch["session_id"].clone();
    service.wait(|| {
        let status = service.request(proto::SESSION_GET, json!({"session_id":id})).unwrap();
        (status["state"] == "ready").then_some(())
    });
    service.restart();
    assert_eq!(service.request(proto::TASK_START, replay.clone()).unwrap()["id"], id);
    let mut changed = replay;
    changed["claude"]["prompt"] = json!("different work");
    assert!(service.request(proto::TASK_START, changed).is_err());
}

#[cfg(debug_assertions)]
#[test]
fn task_launch_crashes_never_replay_the_start() {
    for (engine, point) in [
        ("claude", "task.after_claim"),
        ("claude", "launch.after_host"),
        ("codex", "task.after_claim"),
        ("codex", "launch.after_host"),
    ] {
        let mut service = Service::new();
        let previous = session(&service);
        service.request("session.stop", json!({"session_id":previous})).unwrap();
        let task = service
            .request(
                proto::TASK_CREATE,
                json!({
                    "title":"Launch","goal":"Launch","criteria":["passes"],"project":service.repo()
                }),
            )
            .unwrap();
        let mut params = json!({"task_id":task["id"],"request_id":"same"});
        let codex_home = service.temp.path().join("codex-home");
        if engine == "codex" {
            let executable = service.temp.path().join("codex");
            fs::write(
                &executable,
                concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            params["codex"] =
                json!({"cwd":service.repo(),"executable":executable,"codex_home":codex_home});
        } else {
            params["claude"] = json!({
                "cwd":service.repo(),"executable":service.temp.path().join("claude"),
                "config_dir":service.temp.path().join("config"),"prompt":"implement"
            });
        }
        Client::connect(&service.runtime).unwrap().shutdown().unwrap();
        service.process.wait().unwrap();
        service.process = spawn_at(&service.runtime, Some(point));
        service.wait(|| Client::connect(&service.runtime).ok());
        let _ = service.request(proto::TASK_START, params.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        let exit = loop {
            if let Some(exit) = service.process.try_wait().unwrap() {
                break exit;
            }
            assert!(Instant::now() < deadline, "{engine} did not reach crash point {point}");
            thread::sleep(Duration::from_millis(20));
        };
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(exit.signal(), Some(libc::SIGKILL), "{engine}: {point}");
        service.process = spawn(&service.runtime);
        service.wait(|| Client::connect(&service.runtime).ok());
        let launch = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap()
            ["launch"]
            .clone();
        let recovered = service.request(proto::TASK_START, params.clone());
        let reset_params = json!({"task_id":task["id"], "request_id":"same"});
        if point == "task.after_claim" {
            assert!(recovered.unwrap_err().to_string().contains("requires reconciliation"));
            assert_eq!(launch["may_have_started"], false);
            let root = service.runtime.directory().join(launch["session_id"].as_str().unwrap());
            let evidence = root.join("evidence");
            fs::create_dir_all(&root).unwrap();
            fs::write(&evidence, b"unresolved").unwrap();
            assert!(service.request(proto::TASK_RESET_LAUNCH, reset_params.clone()).is_err());
            fs::remove_file(evidence).unwrap();

            let queued =
                capacity_task(&service, "Queued behind unresolved launch", &service.repo());
            let queued_params = capacity_launch(&service, &queued, &service.repo());
            assert_capacity_conflict(&service, queued_params.clone());
            let board = service.request(proto::TASK_BOARD, json!({"cwd":service.repo()})).unwrap();
            assert_eq!(board["occupied"], 1, "an unresolved launch reserves capacity: {board}");
            assert_eq!(board["available"], 0);
            let reset = service.request(proto::TASK_RESET_LAUNCH, reset_params.clone()).unwrap();
            assert!(reset["launch"].is_null());
            assert_eq!(reset["sessions"], json!([]));
            assert_eq!(reset["retired_launch_requests"], json!(["same"]));
            let board = service.request(proto::TASK_BOARD, json!({"cwd":service.repo()})).unwrap();
            assert_eq!(board["available"], 1, "reset must release unresolved reservation: {board}");

            service.restart();
            assert_eq!(service.request(proto::TASK_RESET_LAUNCH, reset_params).unwrap(), reset);
            assert!(
                service
                    .request(proto::TASK_START, params.clone())
                    .unwrap_err()
                    .to_string()
                    .contains("use a new request ID")
            );
            params["request_id"] = json!("retry");
            let retried = service.request(proto::TASK_START, params).unwrap();
            assert_ne!(retried["id"], launch["session_id"]);
        } else {
            assert_eq!(recovered.unwrap()["id"], launch["session_id"]);
            assert_eq!(launch["may_have_started"], true);
            assert!(service.request(proto::TASK_RESET_LAUNCH, reset_params).is_err());
            if engine == "codex" {
                let launches = codex_home.join("backend-launches.txt");
                service.wait(|| launches.exists().then_some(()));
                assert_eq!(fs::read_to_string(launches).unwrap().lines().count(), 1);
            }
        }
    }
}

#[test]
fn task_start_cli_launches_and_replays_codex() {
    let service = Service::new();
    let previous = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":previous})).unwrap();
    let task = service
        .request(
            proto::TASK_CREATE,
            json!({
                "title":"Launch","goal":"Launch","criteria":["passes"],"project":service.repo()
            }),
        )
        .unwrap();
    let executable = service.temp.path().join("codex");
    fs::write(&executable, concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")))
        .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let home = service.temp.path().join("config");
    let start = || {
        let output = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .current_dir(service.repo())
            .args([
                "task",
                "start",
                task["id"].as_str().unwrap(),
                "--request-id",
                "cli",
                "--engine",
                "codex",
                "--codex",
            ])
            .arg(&executable)
            .env("CODEX_HOME", &home)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let recorded = start();
    assert_eq!(recorded["engine"], "codex");
    assert_eq!(recorded["sandbox"], "read-only");
    assert_eq!(start()["id"], recorded["id"]);
}

fn capacity_task(service: &Service, title: &str, cwd: &Path) -> Value {
    service
        .request(
            proto::TASK_CREATE,
            json!({
                "title":title,"goal":title,"criteria":["passes"],"project":cwd
            }),
        )
        .unwrap()
}

fn capacity_launch(service: &Service, task: &Value, cwd: &Path) -> Value {
    json!({"task_id":task["id"],"request_id":"capacity", "claude":{
        "cwd":cwd,"executable":service.temp.path().join("claude"),
        "config_dir":service.temp.path().join("config"),"prompt":"implement"
    }})
}

fn assert_capacity_conflict(service: &Service, params: Value) {
    let task = params["task_id"].clone();
    let error = service.request(proto::TASK_START, params).unwrap_err();
    assert!(matches!(error, xaya_agentd::Error::Rpc { code: proto::CONFLICT, .. }), "{error}");
    let task = service.request(proto::TASK_GET, json!({"task_id":task})).unwrap();
    assert!(task["launch"].is_null(), "refusal must not claim the task: {task}");
}

#[test]
fn task_start_capacity_is_shared_by_concurrent_worktree_launches_and_released_on_stop() {
    let service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let linked = service.temp.path().join("capacity-linked");
    git(&service.repo(), &["worktree", "add", "--detach", linked.to_str().unwrap()]);
    let a = capacity_task(&service, "A", &service.repo());
    let b = capacity_task(&service, "B", &linked);
    let params =
        [capacity_launch(&service, &a, &service.repo()), capacity_launch(&service, &b, &linked)];
    let barrier = std::sync::Barrier::new(2);
    let claims = thread::scope(|scope| {
        let run = |i: usize| {
            barrier.wait();
            service.request(proto::TASK_START, params[i].clone())
        };
        let first = scope.spawn(move || run(0));
        let second = scope.spawn(move || run(1));
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(claims.iter().filter(|claim| claim.is_ok()).count(), 1, "{claims:?}");
    let winner = usize::from(claims[0].is_err());
    let loser = 1 - winner;
    let started = claims[winner].as_ref().unwrap();
    let board = service.request(proto::TASK_BOARD, json!({"cwd":linked})).unwrap();
    assert_eq!(board["occupied"], 1, "a bound launch must not be counted twice: {board}");
    assert_eq!(board["available"], 0);
    assert_capacity_conflict(&service, params[loser].clone());
    assert_eq!(
        service.request(proto::TASK_START, params[winner].clone()).unwrap()["id"],
        started["id"]
    );
    service.request(proto::SESSION_STOP, json!({"session_id":started["id"]})).unwrap();
    let next = service.request(proto::TASK_START, params[loser].clone()).unwrap();
    assert_ne!(next["id"], started["id"]);
}

#[test]
fn task_start_capacity_counts_ready_working_blocked_and_interrupted_manual_sessions() {
    let mut service = Service::new();
    let initial = session(&service);
    let task = capacity_task(&service, "Queued", &service.repo());
    let params = capacity_launch(&service, &task, &service.repo());
    assert_capacity_conflict(&service, params.clone());
    fs::write(service.temp.path().join("config/activate-turn"), b"").unwrap();
    service.wait(|| {
        let state = service.request(proto::SESSION_GET, json!({"session_id":initial})).unwrap();
        (state["state"] == "working").then_some(())
    });
    assert_capacity_conflict(&service, params.clone());
    let info = service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":initial})).unwrap();
    let host = PathBuf::from(info["args"][2].as_str().unwrap());
    let mut attached =
        xaya_agentd::pty_host::attach(&host, xaya_agentd::pty_host::Size { rows: 24, cols: 80 })
            .unwrap();
    attached.send(b"end cancel-turn\r").unwrap();
    attached.read_until("TURN_ENDED cancel-turn", Duration::from_secs(10)).unwrap();
    attached.send(b"permission\r").unwrap();
    attached.read_until("PERMISSION_ASKED", Duration::from_secs(10)).unwrap();
    drop(attached);
    service.wait(|| {
        let state = service.request(proto::SESSION_GET, json!({"session_id":initial})).unwrap();
        (state["state"] == "blocked").then_some(())
    });
    assert_capacity_conflict(&service, params.clone());
    fs::remove_file(host.parent().unwrap().join("bridge.json")).unwrap();
    service.process.kill().unwrap();
    service.process.wait().unwrap();
    service.process = spawn(&service.runtime);
    service.wait(|| Client::connect(&service.runtime).ok());
    assert_eq!(
        service.request(proto::SESSION_GET, json!({"session_id":initial})).unwrap()["state"],
        "interrupted"
    );
    assert_capacity_conflict(&service, params.clone());
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    service.request(proto::TASK_START, params).unwrap();
}

#[test]
fn task_start_capacity_does_not_block_another_project() {
    let service = Service::new();
    let initial = session(&service);
    let other = service.temp.path().join("capacity-other-project");
    git(service.temp.path(), &[
        "clone",
        "--quiet",
        service.repo().to_str().unwrap(),
        other.to_str().unwrap(),
    ]);
    let task = capacity_task(&service, "Independent", &other);
    let started =
        service.request(proto::TASK_START, capacity_launch(&service, &task, &other)).unwrap();
    assert_ne!(started["id"], initial);
}

#[cfg(debug_assertions)]
#[test]
fn task_start_capacity_keeps_a_closed_tasks_unresolved_launch_reserved() {
    let mut service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let claimed = capacity_task(&service, "Unresolved", &service.repo());
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    service.process.wait().unwrap();
    service.process = spawn_at(&service.runtime, Some("task.after_claim"));
    service.wait(|| Client::connect(&service.runtime).ok());
    assert!(
        service
            .request(proto::TASK_START, capacity_launch(&service, &claimed, &service.repo()))
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while service.process.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the service did not reach task.after_claim");
        thread::sleep(Duration::from_millis(20));
    }
    service.process = spawn(&service.runtime);
    service.wait(|| Client::connect(&service.runtime).ok());
    service
        .request(proto::TASK_CLOSE, json!({"task_id":claimed["id"],"status":"abandoned"}))
        .unwrap();
    let queued = capacity_task(&service, "Queued", &service.repo());
    assert_capacity_conflict(&service, capacity_launch(&service, &queued, &service.repo()));
    let board = service.request(proto::TASK_BOARD, json!({"cwd":service.repo()})).unwrap();
    assert_eq!(board["occupied"], 1);
    assert_eq!(board["available"], 0);
    assert!(board["tasks"].as_array().unwrap().iter().all(|task| task["task_id"] != claimed["id"]));
}

#[test]
fn task_start_capacity_releases_retired_session_claims_after_eviction_and_restart() {
    let mut service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let executable = service.temp.path().join("capacity-codex");
    fs::write(&executable, concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")))
        .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let launch = |service: &Service, task: &Value| {
        json!({
            "task_id":task["id"], "request_id":"capacity", "codex":{
                "cwd":service.repo(),"executable":executable,"codex_home":service.temp.path().join("codex-home")
            }
        })
    };
    let mut first = None;
    for n in 0..17 {
        let task = capacity_task(&service, &format!("Completed {n}"), &service.repo());
        let started = service.request(proto::TASK_START, launch(&service, &task)).unwrap();
        first.get_or_insert_with(|| started["id"].clone());
        let stopped =
            service.request(proto::SESSION_STOP, json!({"session_id":started["id"]})).unwrap();
        assert_eq!(stopped["state"], "stopped");
    }
    let first = first.unwrap();
    let error = service.request(proto::SESSION_GET, json!({"session_id":first})).unwrap_err();
    assert!(
        matches!(error, xaya_agentd::Error::Rpc { code: proto::NOT_FOUND, .. }),
        "the session table should have evicted stopped records: {error}"
    );
    service.restart();
    let board = service.request(proto::TASK_BOARD, json!({"cwd":service.repo()})).unwrap();
    assert_eq!(
        board["occupied"], 0,
        "confirmed retired sessions must not become unresolved launches: {board}"
    );
    assert_eq!(board["available"], 1);
    let next = capacity_task(&service, "After eviction", &service.repo());
    service.request(proto::TASK_START, launch(&service, &next)).unwrap();
}

#[test]
fn task_capacity_policy_is_shared_by_worktrees_validated_and_persistent() {
    let mut service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let linked = service.temp.path().join("policy-linked");
    git(&service.repo(), &["worktree", "add", "--detach", linked.to_str().unwrap()]);
    let get = json!({"cwd":service.repo()});
    let default = service.request("task.capacity.get", get.clone()).unwrap();
    assert_eq!(default["capacity"], 1);
    let changed = service.request("task.capacity.set", json!({"cwd":linked,"capacity":2})).unwrap();
    assert_eq!(changed["project"], default["project"]);
    assert_eq!(changed["capacity"], 2);
    assert_eq!(service.request("task.capacity.get", get.clone()).unwrap(), changed);
    for invalid in [json!(0), json!(17), json!(-1), json!(1.5), json!("2")] {
        let error = service
            .request("task.capacity.set", json!({"cwd":linked,"capacity":invalid}))
            .unwrap_err();
        assert!(
            matches!(error, xaya_agentd::Error::Rpc { code: proto::INVALID_PARAMS, .. }),
            "{error}"
        );
        assert_eq!(service.request("task.capacity.get", get.clone()).unwrap(), changed);
    }
    let board = service.request(proto::TASK_BOARD, get.clone()).unwrap();
    assert_eq!(board["capacity"], 2);
    assert_eq!(board["available"], 2);
    let other = service.temp.path().join("policy-other");
    git(service.temp.path(), &[
        "clone",
        "--quiet",
        service.repo().to_str().unwrap(),
        other.to_str().unwrap(),
    ]);
    assert_eq!(service.request("task.capacity.get", json!({"cwd":other})).unwrap()["capacity"], 1);
    service.restart();
    assert_eq!(service.request("task.capacity.get", json!({"cwd":linked})).unwrap(), changed);
    assert_eq!(service.request(proto::TASK_BOARD, get).unwrap()["capacity"], 2);
}

#[test]
fn task_capacity_policy_bounds_parallel_launches_and_reductions_preserve_sessions() {
    let service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let mut directories = vec![service.repo()];
    for n in 1..3 {
        let directory = service.temp.path().join(format!("policy-worktree-{n}"));
        git(&service.repo(), &["worktree", "add", "--detach", directory.to_str().unwrap()]);
        directories.push(directory);
    }
    let params: Vec<_> = directories
        .iter()
        .enumerate()
        .map(|(n, directory)| {
            let task = capacity_task(&service, &format!("Parallel {n}"), directory);
            capacity_launch(&service, &task, directory)
        })
        .collect();
    service.request("task.capacity.set", json!({"cwd":service.repo(),"capacity":2})).unwrap();
    let barrier = std::sync::Barrier::new(3);
    let claims = thread::scope(|scope| {
        let run = |i: usize| {
            barrier.wait();
            service.wait(|| {
                let result = service.request(proto::TASK_START, params[i].clone());
                match &result {
                    Err(xaya_agentd::Error::Rpc { code: proto::CONFLICT, message })
                        if message
                            .contains("another task launch is in progress for this project") =>
                    {
                        None
                    },
                    _ => Some(result),
                }
            })
        };
        let threads: Vec<_> = (0..3).map(|i| scope.spawn(move || run(i))).collect();
        threads.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
    });
    let winners: Vec<_> = claims
        .iter()
        .enumerate()
        .filter_map(|(i, claim)| claim.as_ref().ok().map(|session| (i, session)))
        .collect();
    assert_eq!(winners.len(), 2, "{claims:?}");
    let loser = claims.iter().position(Result::is_err).unwrap();
    assert_capacity_conflict(&service, params[loser].clone());
    let query = json!({"cwd":service.repo()});
    let board = service.request(proto::TASK_BOARD, query.clone()).unwrap();
    assert_eq!(board["capacity"], 2);
    assert_eq!(board["occupied"], 2);
    assert_eq!(board["available"], 0);
    let before: Vec<_> = winners
        .iter()
        .map(|(_, started)| {
            service.wait(|| {
                let session = service
                    .request(proto::SESSION_GET, json!({"session_id":started["id"]}))
                    .unwrap();
                (session["state"] == "ready").then_some(session)
            })
        })
        .collect();
    service.request("task.capacity.set", json!({"cwd":directories[1],"capacity":1})).unwrap();
    let board = service.request(proto::TASK_BOARD, query.clone()).unwrap();
    assert_eq!(board["capacity"], 1);
    assert_eq!(board["occupied"], 2);
    assert_eq!(board["available"], 0, "reduction must use saturating subtraction");
    for ((index, started), before) in winners.iter().zip(before.iter()) {
        let after =
            service.request(proto::SESSION_GET, json!({"session_id":started["id"]})).unwrap();
        assert_eq!(after["state"], "ready");
        assert_eq!(after["backend_pid"], before["backend_pid"]);
        assert_eq!(
            service.request(proto::TASK_START, params[*index].clone()).unwrap()["id"],
            started["id"]
        );
    }
    assert_capacity_conflict(&service, params[loser].clone());
    service.request(proto::SESSION_STOP, json!({"session_id":winners[0].1["id"]})).unwrap();
    assert_capacity_conflict(&service, params[loser].clone());
    assert_eq!(service.request(proto::TASK_BOARD, query.clone()).unwrap()["occupied"], 1);
    service.request(proto::SESSION_STOP, json!({"session_id":winners[1].1["id"]})).unwrap();
    assert_eq!(service.request(proto::TASK_BOARD, query).unwrap()["available"], 1);
    service.request(proto::TASK_START, params[loser].clone()).unwrap();
}

#[test]
fn task_capacity_policy_cli_reads_and_sets_a_project_limit() {
    let service = Service::new();
    session(&service);
    let run = |args: &[&str]| {
        Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .current_dir(service.repo())
            .args(["task", "capacity"])
            .args(args)
            .output()
            .unwrap()
    };
    let read = run(&[]);
    assert!(read.status.success(), "{}", String::from_utf8_lossy(&read.stderr));
    let default: Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(default["capacity"], 1);
    let changed = run(&[".", "--limit", "3"]);
    assert!(changed.status.success(), "{}", String::from_utf8_lossy(&changed.stderr));
    let changed: Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(changed["project"], default["project"]);
    assert_eq!(changed["capacity"], 3);
    assert_eq!(serde_json::from_slice::<Value>(&run(&[]).stdout).unwrap(), changed);
    for invalid in ["0", "17", "-1", "1.5"] {
        assert!(!run(&["--limit", invalid]).status.success());
    }
    assert_eq!(
        service.request("task.capacity.get", json!({"cwd":service.repo()})).unwrap(),
        changed
    );
}

#[test]
fn task_capacity_policy_corruption_blocks_the_board_and_new_launches() {
    let service = Service::new();
    let initial = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":initial})).unwrap();
    let task = capacity_task(&service, "Corrupt policy", &service.repo());
    service.request("task.capacity.set", json!({"cwd":service.repo(),"capacity":2})).unwrap();
    let database =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    database
        .execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE task_capacity SET capacity=0;")
        .unwrap();
    assert!(service.request("task.capacity.get", json!({"cwd":service.repo()})).is_err());
    assert!(service.request(proto::TASK_BOARD, json!({"cwd":service.repo()})).is_err());
    assert!(
        service
            .request(proto::TASK_START, capacity_launch(&service, &task, &service.repo()))
            .is_err()
    );
    let unchanged = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
    assert!(unchanged["launch"].is_null());
    service.request("task.capacity.set", json!({"cwd":service.repo(),"capacity":1})).unwrap();
    service.request(proto::TASK_START, capacity_launch(&service, &task, &service.repo())).unwrap();
}

fn queue_params(service: &Service, task: &Value, request_id: &str) -> Value {
    let mut params = capacity_launch(service, task, &service.repo());
    params["request_id"] = json!(request_id);
    params
}

/// The queue worker may hold a task's launch lock for a moment, even for one that was just
/// cancelled: that conflict is documented as retryable.
fn retry_launch_conflict<E: std::fmt::Debug>(
    mut request: impl FnMut() -> Result<Value, E>,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match request() {
            Ok(value) => return value,
            Err(error)
                if format!("{error:?}").contains("another launch operation is in progress")
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(20));
            },
            Err(error) => panic!("{error:?}"),
        }
    }
}

fn queued_task(service: &Service, task: &Value) -> Value {
    service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap()
}

#[test]
fn task_queue_waits_for_capacity_then_dispatches_once_without_a_client() {
    let service = Service::new();
    let blocker = session(&service);
    let task = capacity_task(&service, "Queued implementation", &service.repo());
    let params = queue_params(&service, &task, "queued-launch");
    let queued = service.request("task.enqueue", params.clone()).unwrap();
    assert_eq!(queued["queue"]["request_id"], "queued-launch");
    service.wait(|| {
        let task = queued_task(&service, &task);
        (task["queue"]["state"] == "waiting").then_some(task)
    });
    assert!(queued_task(&service, &task)["launch"].is_null());
    assert_eq!(
        service.request("task.enqueue", params.clone()).unwrap()["queue"]["request_id"],
        "queued-launch"
    );
    let mut changed = params.clone();
    changed["claude"]["prompt"] = json!("different implementation");
    assert!(service.request("task.enqueue", changed).is_err());
    let mut stolen = params.clone();
    stolen["request_id"] = json!("manual-steal");
    assert!(service.request(proto::TASK_START, stolen).is_err());
    service.request(proto::SESSION_STOP, json!({"session_id":blocker})).unwrap();
    // No task-control request triggers dispatch: wait for the service's background worker to
    // materialize its durable session record before reconnecting to inspect the result.
    let database =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    service.wait(|| {
        let record: String = database
            .query_row(
                "SELECT record FROM tasks WHERE id=?1",
                [task["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let saved: Value = serde_json::from_str(&record).unwrap();
        (saved["queue"]["state"] == "dispatched").then_some(())
    });
    let dispatched = queued_task(&service, &task);
    let session = dispatched["queue"]["session_id"].clone();
    assert!(session.is_string(), "{dispatched}");
    assert_eq!(dispatched["launch"]["session_id"], session);
    assert_eq!(
        service.request("task.enqueue", params.clone()).unwrap()["queue"]["session_id"],
        session
    );
    assert_eq!(service.request(proto::TASK_START, params).unwrap()["id"], session);
    let sessions = service.request(proto::SESSION_LIST, json!({})).unwrap();
    assert_eq!(
        sessions.as_array().unwrap().iter().filter(|item| item["state"] != "stopped").count(),
        1,
        "{sessions}"
    );
}

#[test]
fn task_queue_cancellation_closure_and_dependency_waits_do_not_block_ready_tasks() {
    let service = Service::new();
    let blocker = session(&service);
    let cancelled = capacity_task(&service, "Cancelled queue", &service.repo());
    let closed = capacity_task(&service, "Closed queue", &service.repo());
    let prerequisite = capacity_task(&service, "Unlanded prerequisite", &service.repo());
    let dependent = service.request(proto::TASK_CREATE, json!({
        "title":"Dependent queue", "goal":"Implement after prerequisite", "criteria":["passes"],
        "project":service.repo(), "depends_on":[prerequisite["id"]]
    })).unwrap();
    let ready = capacity_task(&service, "Independent queue", &service.repo());
    for task in [&cancelled, &closed, &dependent, &ready] {
        service.request("task.enqueue", queue_params(&service, task, "queue")).unwrap();
    }
    let cancel = json!({"task_id":cancelled["id"],"request_id":"queue"});
    let cancelled_result =
        retry_launch_conflict(|| service.request("task.dequeue", cancel.clone()));
    assert_eq!(cancelled_result["queue"]["state"], "cancelled");
    assert_eq!(
        retry_launch_conflict(|| service.request("task.dequeue", cancel.clone())),
        cancelled_result
    );
    assert!(
        service
            .request("task.dequeue", json!({"task_id":cancelled["id"],"request_id":"other"}))
            .is_err()
    );
    let replay = retry_launch_conflict(|| {
        service.request("task.enqueue", queue_params(&service, &cancelled, "queue"))
    });
    assert_eq!(
        replay["queue"]["state"], "cancelled",
        "a late enqueue must not revive a cancellation"
    );
    retry_launch_conflict(|| {
        service.request(proto::TASK_CLOSE, json!({"task_id":closed["id"],"status":"abandoned"}))
    });
    service.request(proto::SESSION_STOP, json!({"session_id":blocker})).unwrap();
    let started = service.wait(|| {
        let task = queued_task(&service, &ready);
        (task["queue"]["state"] == "dispatched").then_some(task)
    });
    assert!(started["queue"]["session_id"].is_string());
    for task in [&cancelled, &closed, &dependent] {
        assert!(queued_task(&service, task)["launch"].is_null(), "{task}");
    }
    assert_eq!(queued_task(&service, &cancelled)["queue"]["state"], "cancelled");
    assert_eq!(queued_task(&service, &closed)["status"], "abandoned");
    service
        .request(proto::SESSION_STOP, json!({"session_id":started["queue"]["session_id"]}))
        .unwrap();
    let waiting = service.wait(|| {
        let task = queued_task(&service, &dependent);
        (task["queue"]["state"] == "waiting"
            && task["queue"]["reason"].as_str().is_some_and(|s| s.contains("prerequisite")))
        .then_some(task)
    });
    assert!(waiting["launch"].is_null());
}

#[test]
fn task_queue_survives_restart_and_resumes_its_dispatch() {
    let mut service = Service::new();
    session(&service);
    let task = capacity_task(&service, "Durable queue", &service.repo());
    let params = queue_params(&service, &task, "durable");
    service.request("task.enqueue", params.clone()).unwrap();
    service.wait(|| (queued_task(&service, &task)["queue"]["state"] == "waiting").then_some(()));
    service.restart();
    let dispatched = service.wait(|| {
        let saved = queued_task(&service, &task);
        (saved["queue"]["state"] == "dispatched").then_some(saved)
    });
    let id = dispatched["queue"]["session_id"].clone();
    assert!(id.is_string());
    assert_eq!(dispatched["queue"]["request_id"], "durable");
    assert_eq!(service.request("task.enqueue", params).unwrap()["queue"]["session_id"], id);
    service.restart();
    let restored = queued_task(&service, &task);
    assert_eq!(restored["queue"]["session_id"], id);
    assert_eq!(restored["launch"]["session_id"], id);
    assert_eq!(restored["queue"]["state"], "dispatched");
}

#[cfg(debug_assertions)]
#[test]
fn task_queue_does_not_replay_a_launch_after_claim_crash() {
    let mut service = Service::new();
    let blocker = session(&service);
    service.request(proto::SESSION_STOP, json!({"session_id":blocker})).unwrap();
    let uncertain = capacity_task(&service, "Claim crash target", &service.repo());
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    service.process.wait().unwrap();
    service.process = spawn_at(&service.runtime, Some("task.after_claim"));
    service.wait(|| Client::connect(&service.runtime).ok());
    // A worker can reach SIGKILL before the enqueue response is flushed; persisted state,
    // rather than receipt of this response, is the evidence checked after restart.
    let _ = service.request("task.enqueue", queue_params(&service, &uncertain, "uncertain"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.process.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the queued launcher never reached task.after_claim");
        thread::sleep(Duration::from_millis(20));
    }
    service.process = spawn(&service.runtime);
    service.wait(|| Client::connect(&service.runtime).ok());
    let attention = service.wait(|| {
        let task = queued_task(&service, &uncertain);
        (task["queue"]["state"] == "needs_attention").then_some(task)
    });
    assert_eq!(attention["launch"]["may_have_started"], false);
    let claimed = attention["launch"].clone();
    assert!(
        service.request(proto::SESSION_GET, json!({"session_id":claimed["session_id"]})).is_err()
    );
    let another = capacity_task(&service, "After uncertain launch", &service.repo());
    service.request("task.enqueue", queue_params(&service, &another, "another")).unwrap();
    service.wait(|| (queued_task(&service, &another)["queue"]["state"] == "waiting").then_some(()));
    assert_eq!(queued_task(&service, &uncertain)["launch"], claimed);
    assert_eq!(queued_task(&service, &uncertain)["queue"]["state"], "needs_attention");
}

#[test]
fn task_queue_cli_enqueues_and_cancels_the_same_request() {
    let service = Service::new();
    session(&service);
    let task = capacity_task(&service, "CLI queue", &service.repo());
    let enqueue = || {
        Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .current_dir(service.repo())
            .args([
                "task",
                "enqueue",
                task["id"].as_str().unwrap(),
                "--request-id",
                "cli",
                "--claude",
            ])
            .arg(service.temp.path().join("claude"))
            .env("CLAUDE_CONFIG_DIR", service.temp.path().join("config"))
            .output()
            .unwrap()
    };
    let output = enqueue();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(saved["queue"]["request_id"], "cli");
    assert!(saved["queue"]["claude"]["prompt"].as_str().unwrap().contains("CLI queue"));
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .current_dir(service.repo())
        .args(["task", "dequeue", task["id"].as_str().unwrap(), "--request-id", "cli"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cancelled: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cancelled["queue"]["state"], "cancelled");
    let replay: Value = serde_json::from_slice(&enqueue().stdout).unwrap();
    assert_eq!(replay["queue"]["state"], "cancelled");
}

#[test]
fn task_queue_dispatches_after_its_prerequisite_lands_and_checkout_catches_up() {
    let service = Service::new();
    let implementation = session(&service);
    let prior = create(&service, &[&implementation]).unwrap();
    let checkpoint = checkpoint(&service, &implementation, 0);
    let gate = check(&service, &checkpoint, &["true"], "queue-dependency-check");
    let home = review_home(
        &service,
        "queue-dependency-reviewer",
        json!({"review":clean(), "criteria":results(&[("AC1","met"),("AC2","met")])}),
    );
    let reviewed =
        review(&service, &checkpoint, Some(&prior), Some(&gate), &home, "queue-dependency-review")
            .unwrap();
    assert_eq!(reviewed["verdict"], "approved", "{reviewed}");
    service.request(proto::TASK_CLOSE, json!({"task_id":prior["id"],"status":"accepted"})).unwrap();
    let checkout = service.temp.path().join("queue-dependent-checkout");
    git(&service.repo(), &["worktree", "add", "--detach", checkout.to_str().unwrap()]);
    let dependent = service.request(proto::TASK_CREATE, json!({
        "title":"Queue after landing", "goal":"Implement once dependency is available", "criteria":["passes"],
        "project":checkout, "depends_on":[prior["id"]]
    })).unwrap();
    let mut enqueue = queue_params(&service, &dependent, "after-landing");
    enqueue["claude"]["cwd"] = json!(checkout);
    service.request("task.enqueue", enqueue).unwrap();
    service.wait(|| {
        let task = queued_task(&service, &dependent);
        (task["queue"]["state"] == "waiting"
            && task["queue"]["reason"].as_str().is_some_and(|reason| {
                reason.contains("prerequisite") && reason.contains("awaiting_landing")
            }))
        .then_some(())
    });
    assert!(queued_task(&service, &dependent)["launch"].is_null());
    git(&service.repo(), &["branch", "queue-integration"]);
    let preview = service.request(proto::LAND_PREVIEW, json!({
        "checkpoint_id":checkpoint["id"], "check_id":gate["id"], "target_branch":"queue-integration", "command_id":"queue-dependency-land"
    })).unwrap();
    let params = json!({"preview_id":preview["id"]});
    let wait = |busy: &str| {
        service.wait(|| {
            let receipt = service.request(proto::LAND_GET, params.clone()).unwrap();
            (receipt["status"] != busy).then_some(receipt)
        })
    };
    assert_eq!(wait("creating")["status"], "ready");
    service.request(proto::LAND_VERIFY, params.clone()).unwrap();
    assert_eq!(wait("verifying")["integration_verified"], true);
    let landed = service.request(proto::LAND_RUN, params).unwrap();
    assert_eq!(landed["status"], "landed", "{landed}");
    let candidate = landed["candidate_commit"].as_str().unwrap();
    service.wait(|| {
        let task = queued_task(&service, &dependent);
        (task["queue"]["state"] == "waiting"
            && task["queue"]["reason"].as_str().is_some_and(|reason| reason.contains(candidate)))
        .then_some(())
    });
    assert!(
        queued_task(&service, &dependent)["launch"].is_null(),
        "landing must not bypass a stale checkout"
    );
    git(&checkout, &["switch", "--detach", candidate]);
    let dispatched = service.wait(|| {
        let task = queued_task(&service, &dependent);
        (task["queue"]["state"] == "dispatched").then_some(task)
    });
    assert_eq!(dispatched["queue"]["request_id"], "after-landing");
    assert_eq!(dispatched["queue"]["session_id"], dispatched["launch"]["session_id"]);
    let launched = service
        .request(proto::SESSION_GET, json!({"session_id":dispatched["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(launched["cwd"], checkout.to_str().unwrap());
}

fn drive_params(service: &Service, task: &Value, request: &str, first: &str, sleep: f64) -> Value {
    let home = review_home(
        service,
        &format!("drive-review-{request}"),
        json!({"review":clean(),"criteria":results(&[("AC1",first),("AC2","met")]),"sleep":sleep}),
    );
    json!({"task_id":task["id"],"request_id":request,"timeout_secs":60,
        "reviewer":{"engine":"codex","executable":service.temp.path().join("reviewer"),
            "config_dir":home}})
}

/// Poll, without holding any client open in between, until the drive has an outcome.
fn drive_outcome(service: &Service, task: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
        let state = &saved["drive"]["state"];
        if state != "driving" && state != "waiting" {
            return saved;
        }
        assert!(Instant::now() < deadline, "the drive did not finish: {saved}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn task_drive_approves_in_the_background_and_repeats_only_its_record() {
    let service = Service::new();
    let task = cycle_fixture(&service, 0, false);
    let params = drive_params(&service, &task, "drive-1", "met", 0.0);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "drive", task["id"].as_str().unwrap(), "--request-id", "drive-1"])
        .arg("--codex")
        .arg(service.temp.path().join("reviewer"))
        .env("CODEX_HOME", params["reviewer"]["config_dir"].as_str().unwrap())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved = drive_outcome(&service, &task);
    let drive = &saved["drive"];
    assert_eq!(drive["state"], "approved", "{saved}");
    let rounds = drive["progress"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "{saved}");
    assert_eq!(rounds[0]["verdict"], "approved", "{saved}");
    assert!(rounds[0]["command_id"].is_null(), "nothing is sent back: {saved}");
    // The CLI recorded the same parameters a request would have.
    let mut same = drive["params"].clone();
    let repeated = service.request(proto::TASK_DRIVE, same.clone()).unwrap();
    assert_eq!(repeated["drive"], saved["drive"], "a repeated request returns the record");
    same["timeout_secs"] = json!(61);
    assert!(service.request(proto::TASK_DRIVE, same).is_err(), "parameters are immutable");
    let undriven = service
        .request(proto::TASK_UNDRIVE, json!({"task_id":task["id"],"request_id":"drive-1"}))
        .unwrap();
    assert_eq!(undriven["drive"]["state"], "approved", "an outcome is not cancelled");
}

#[test]
fn task_drive_sends_fixes_back_and_stops_on_the_same_tree() {
    let service = Service::new();
    let task = cycle_fixture(&service, 2, false);
    service
        .request(proto::TASK_DRIVE, drive_params(&service, &task, "drive", "unmet", 0.0))
        .unwrap();
    let saved = drive_outcome(&service, &task);
    let drive = &saved["drive"];
    assert_eq!(drive["state"], "needs_attention", "{saved}");
    let rounds = drive["progress"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 2, "{saved}");
    assert_eq!(rounds[0]["turn_id"], "send-1", "{saved}");
    assert_eq!(saved["fix_rounds"].as_array().unwrap().len(), 1, "{saved}");
    assert!(saved["attention"]["reason"].as_str().unwrap().contains("same tree"), "{saved}");
    assert!(drive["reason"].as_str().unwrap().contains("same tree"), "{saved}");
}

#[test]
fn task_drive_retakes_a_review_a_restart_interrupted_and_never_resends() {
    let mut service = Service::new();
    let task = cycle_fixture(&service, 1, false);
    service.request(proto::TASK_DRIVE, drive_params(&service, &task, "drive", "met", 3.0)).unwrap();
    let interrupted = service.wait(|| {
        let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
        let review = saved["drive"]["progress"]["rounds"][0]["review_id"].as_str()?.to_owned();
        let found = service.request(proto::REVIEW_GET, json!({"review_id":review})).unwrap();
        (found["status"] == "running").then_some(review)
    });
    service.restart();
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    let rounds = saved["drive"]["progress"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "the round goes on with the same checkpoint: {saved}");
    assert_ne!(rounds[0]["review_id"], interrupted.as_str(), "{saved}");
    let old = service.request(proto::REVIEW_GET, json!({"review_id":interrupted})).unwrap();
    assert_eq!(old["status"], "interrupted", "{old}");
    assert!(saved["fix_rounds"].as_array().unwrap().is_empty(), "{saved}");
}

#[test]
fn task_drive_waits_for_a_session_and_its_cancellation_is_final() {
    let service = Service::new();
    let repo = service.repo();
    git(&repo, &["init", "-q"]);
    let task = capacity_task(&service, "Driven later", &repo);
    let params = drive_params(&service, &task, "early", "met", 0.0);
    service.request(proto::TASK_DRIVE, params.clone()).unwrap();
    let waiting = service.wait(|| {
        let saved = service.request(proto::TASK_GET, json!({"task_id":task["id"]})).unwrap();
        (saved["drive"]["state"] == "waiting").then_some(saved)
    });
    assert_eq!(waiting["drive"]["reason"], "the task has no session yet", "{waiting}");
    let mut other = params.clone();
    other["request_id"] = json!("other");
    assert!(service.request(proto::TASK_DRIVE, other.clone()).is_err(), "one drive at a time");
    let cancel = json!({"task_id":task["id"],"request_id":"early"});
    let cancelled = service.request(proto::TASK_UNDRIVE, cancel.clone()).unwrap();
    assert_eq!(cancelled["drive"]["state"], "cancelled", "{cancelled}");
    assert_eq!(
        service.request(proto::TASK_UNDRIVE, cancel).unwrap()["drive"]["state"],
        "cancelled"
    );
    let late = service.request(proto::TASK_DRIVE, params).unwrap();
    assert_eq!(late["drive"]["state"], "cancelled", "a late request does not revive it");
    let replaced = service.request(proto::TASK_DRIVE, other).unwrap();
    assert_eq!(replaced["drive"]["params"]["request_id"], "other", "{replaced}");
}

#[test]
fn a_foreground_cycle_is_refused_while_the_service_drives_the_task() {
    let service = Service::new();
    let task = cycle_fixture(&service, 0, false);
    service.request(proto::TASK_DRIVE, drive_params(&service, &task, "drive", "met", 5.0)).unwrap();
    let (success, _) = run_cycle_refused(&service, &task);
    assert!(!success, "the foreground cycle must not run alongside the drive");
    service
        .request(proto::TASK_UNDRIVE, json!({"task_id":task["id"],"request_id":"drive"}))
        .unwrap();
}

fn run_cycle_refused(service: &Service, task: &Value) -> (bool, String) {
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "cycle", task["id"].as_str().unwrap(), "--codex"])
        .arg(service.temp.path().join("reviewer"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("driving this task in the background"), "{stderr}");
    (output.status.success(), stderr)
}

#[test]
fn a_queued_and_driven_task_is_launched_reviewed_and_approved_without_a_client() {
    let service = Service::new();
    let repo = service.repo();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Task fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/stats.py"), BASE).unwrap();
    fs::create_dir(repo.join(".xaya")).unwrap();
    fs::write(
        repo.join(".xaya/project.toml"),
        "[verify]\nsteps = [{name = 'fixture', argv = ['/usr/bin/true']}]\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"]);
    let project = service.request(proto::PROJECT_GET, json!({"cwd":repo})).unwrap();
    service
        .request(proto::PROJECT_TRUST, json!({"cwd":repo,"config_hash":project["config_hash"]}))
        .unwrap();
    let task = service
        .request(
            proto::TASK_CREATE,
            json!({"title":"Queued and driven","goal":"Fix","criteria":["AC1","AC2"],
                "project":repo,"fix_budget":0}),
        )
        .unwrap();
    // Authorize both before anything runs; the drive waits for the queue's session.
    let mut drive = drive_params(&service, &task, "drive", "met", 0.0);
    drive["timeout_secs"] = json!(60);
    service.request(proto::TASK_DRIVE, drive).unwrap();
    service.request(proto::TASK_ENQUEUE, queue_params(&service, &task, "launch")).unwrap();
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["queue"]["state"], "dispatched", "{saved}");
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    assert_eq!(saved["drive"]["session_id"], saved["queue"]["session_id"], "{saved}");
    let rounds = saved["drive"]["progress"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "{saved}");
    assert_eq!(rounds[0]["verdict"], "approved", "{saved}");
}

#[test]
fn a_queued_codex_task_runs_without_a_tui_and_takes_the_task_as_its_first_message() {
    let service = Service::new();
    let blocker = session(&service); // Creates the repository; stopped to free the slot.
    service.request(proto::SESSION_STOP, json!({"session_id":blocker})).unwrap();
    let repo = service.repo();
    let codex = service.temp.path().join("fake-codex");
    fs::write(&codex, concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")))
        .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let launch = |task: &Value, prompt: Option<&str>| {
        let mut codex = json!({"cwd":repo,"executable":codex,
            "codex_home":service.temp.path().join("codex-home"),"sandbox":"read-only"});
        if let Some(prompt) = prompt {
            codex["prompt"] = json!(prompt);
        }
        json!({"task_id":task["id"],"request_id":"codex","codex":codex})
    };
    let task = capacity_task(&service, "Queued for Codex", &repo);
    // Codex needs its first message to run without a TUI, and one engine is named.
    assert!(service.request(proto::TASK_ENQUEUE, launch(&task, None)).is_err());
    let mut both = launch(&task, Some("Implement it"));
    both["claude"] = capacity_launch(&service, &task, &repo)["claude"].clone();
    assert!(service.request(proto::TASK_ENQUEUE, both).is_err());
    service.request(proto::TASK_ENQUEUE, launch(&task, Some("Implement it"))).unwrap();
    let dispatched = service.wait(|| {
        let saved = queued_task(&service, &task);
        (saved["queue"]["state"] == "dispatched").then_some(saved)
    });
    let session = dispatched["queue"]["session_id"].clone();
    let receipt = service.wait(|| {
        let receipt = service
            .request(
                proto::SESSION_COMMAND,
                json!({"session_id":session,"command_id":"xaya-first-message"}),
            )
            .ok()?;
        (receipt["status"] == "succeeded").then_some(receipt)
    });
    assert!(receipt["turn_id"].is_string(), "{receipt}");
    let current = service.request(proto::SESSION_GET, json!({"session_id":session})).unwrap();
    assert_eq!(current["engine"], "codex", "{current}");
    assert_eq!(current["native_thread_id"], "main-thread", "{current}");
    assert!(!current["attached"].as_bool().unwrap(), "no native TUI: {current}");
    assert_eq!(fs::read_to_string(repo.join("turns-audit.txt")).unwrap(), "Implement it\n");
    service.request(proto::SESSION_STOP, json!({"session_id":session})).unwrap();
}

/// A fresh repository on branch `main` with one commit, and no session.
fn project(service: &Service) -> PathBuf {
    let repo = service.repo();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Task fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(repo.join("README"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "base"]);
    repo
}

/// Where the service puts a task's own checkout: beside the project, never inside it.
fn workspace_of(service: &Service, task: &Value) -> PathBuf {
    service.temp.path().join("repo.xaya").join(task["id"].as_str().unwrap())
}

fn workspace_params(service: &Service, task: &Value, request: &str, base: &str) -> Value {
    let mut params = queue_params(service, task, request);
    params["workspace"] = json!({"base_branch": base});
    params
}

fn wait_queue(service: &Service, task: &Value, state: &str) -> Value {
    service.wait(|| {
        let task = queued_task(service, task);
        (task["queue"]["state"] == state).then_some(task)
    })
}

#[test]
fn a_queued_task_gets_its_own_detached_workspace_beside_the_project() {
    let service = Service::new();
    let repo = project(&service);
    let tip = git(&repo, &["rev-parse", "main"]);
    let task = capacity_task(&service, "Own workspace", &repo);
    let path = workspace_of(&service, &task);
    let params = workspace_params(&service, &task, "workspace-launch", "main");
    let queued = service.request("task.enqueue", params.clone()).unwrap();
    // The service decides the checkout; the caller's directory only names nothing.
    assert_eq!(queued["queue"]["claude"]["cwd"], path.to_str().unwrap(), "{queued}");
    assert_eq!(queued["queue"]["workspace"]["base_branch"], "main");
    assert_eq!(service.request("task.enqueue", params.clone()).unwrap()["queue"], queued["queue"]);
    let mut other_base = params.clone();
    other_base["workspace"]["base_branch"] = json!("elsewhere");
    assert!(service.request("task.enqueue", other_base).is_err());

    let dispatched = wait_queue(&service, &task, "dispatched");
    let session = service
        .request(proto::SESSION_GET, json!({"session_id":dispatched["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(session["cwd"], path.to_str().unwrap());
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), tip);
    let detached = Command::new("git")
        .current_dir(&path)
        .args(["symbolic-ref", "-q", "HEAD"])
        .status()
        .unwrap();
    assert!(!detached.success(), "the workspace must not create or hold a branch");
    assert_eq!(
        git(&path, &["rev-parse", "--git-common-dir"]),
        git(&repo, &["rev-parse", "--absolute-git-dir"])
    );
    assert_eq!(git(&repo, &["branch", "--format=%(refname)"]), "refs/heads/main");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "", "the project checkout is untouched");
    assert_eq!(
        service.request("task.enqueue", params).unwrap()["queue"]["session_id"],
        session["id"]
    );
}

#[test]
fn a_workspace_is_never_created_from_a_missing_base_or_over_something_else() {
    let service = Service::new();
    let repo = project(&service);
    let missing = capacity_task(&service, "Missing base", &repo);
    service
        .request("task.enqueue", workspace_params(&service, &missing, "missing", "nope"))
        .unwrap();
    let attention = wait_queue(&service, &missing, "needs_attention");
    assert!(attention["launch"].is_null(), "{attention}");
    assert!(!workspace_of(&service, &missing).exists());

    // Whatever already holds the path is left alone.
    let occupied = capacity_task(&service, "Occupied path", &repo);
    let path = workspace_of(&service, &occupied);
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("notes"), "mine\n").unwrap();
    service
        .request("task.enqueue", workspace_params(&service, &occupied, "occupied", "main"))
        .unwrap();
    let attention = wait_queue(&service, &occupied, "needs_attention");
    assert!(attention["launch"].is_null(), "{attention}");
    assert_eq!(fs::read_to_string(path.join("notes")).unwrap(), "mine\n");
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);

    for base in ["", "-x", "refs/tags/v1"] {
        let task = capacity_task(&service, "Bad base", &repo);
        let error = service.request("task.enqueue", workspace_params(&service, &task, "bad", base));
        assert!(
            matches!(error, Err(xaya_agentd::Error::Rpc { code: proto::INVALID_PARAMS, .. })),
            "{base}: {error:?}"
        );
    }
}

#[test]
fn a_workspace_left_by_a_crash_is_reused_and_the_launch_happens_once() {
    let mut service = Service::new();
    let repo = project(&service);
    let task = capacity_task(&service, "Crash after workspace", &repo);
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    service.process.wait().unwrap();
    service.process = spawn_at(&service.runtime, Some("task.after_workspace"));
    service.wait(|| Client::connect(&service.runtime).ok());
    let params = workspace_params(&service, &task, "after-crash", "main");
    service.request("task.enqueue", params).unwrap();
    // SIGKILL at the crash point: the worktree exists, nothing was claimed.
    assert!(service.process.wait().unwrap().code().is_none());
    let path = workspace_of(&service, &task);
    assert!(path.join("README").is_file());
    service.process = spawn(&service.runtime);
    service.wait(|| Client::connect(&service.runtime).ok());
    let dispatched = wait_queue(&service, &task, "dispatched");
    let session = service
        .request(proto::SESSION_GET, json!({"session_id":dispatched["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(session["cwd"], path.to_str().unwrap());
    let worktrees = git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.matches("worktree ").count(), 2, "{worktrees}");
    service.restart();
    assert_eq!(queued_task(&service, &task)["launch"]["session_id"], session["id"]);
}

#[test]
fn a_manual_task_start_can_ask_for_its_own_workspace() {
    let service = Service::new();
    let repo = project(&service);
    let task = capacity_task(&service, "Manual workspace", &repo);
    let mut params = capacity_launch(&service, &task, &repo);
    params["workspace"] = json!({"base_branch":"main"});
    let session = service.request(proto::TASK_START, params.clone()).unwrap();
    assert_eq!(session["cwd"], workspace_of(&service, &task).to_str().unwrap());
    assert_eq!(service.request(proto::TASK_START, params).unwrap()["id"], session["id"]);
}

#[test]
fn a_workspace_waits_for_prerequisites_to_land_in_its_base_branch() {
    let service = Service::new();
    let implementation = session(&service);
    git(&service.repo(), &["branch", "-M", "main"]);
    let prior = create(&service, &[&implementation]).unwrap();
    let checkpoint = checkpoint(&service, &implementation, 0);
    let gate = check(&service, &checkpoint, &["true"], "workspace-dependency-check");
    let home = review_home(
        &service,
        "workspace-dependency-reviewer",
        json!({"review":clean(), "criteria":results(&[("AC1","met"),("AC2","met")])}),
    );
    let reviewed = review(
        &service,
        &checkpoint,
        Some(&prior),
        Some(&gate),
        &home,
        "workspace-dependency-review",
    )
    .unwrap();
    assert_eq!(reviewed["verdict"], "approved", "{reviewed}");
    service.request(proto::TASK_CLOSE, json!({"task_id":prior["id"],"status":"accepted"})).unwrap();
    service.request(proto::SESSION_STOP, json!({"session_id":implementation})).unwrap();
    git(&service.repo(), &["branch", "integration"]);
    let dependent = service.request(proto::TASK_CREATE, json!({
        "title":"After landing", "goal":"Start from the landed prerequisite", "criteria":["passes"],
        "project":service.repo(), "depends_on":[prior["id"]]
    })).unwrap();
    let params = workspace_params(&service, &dependent, "after-landing", "integration");
    service.request("task.enqueue", params).unwrap();
    service.wait(|| {
        let task = queued_task(&service, &dependent);
        (task["queue"]["state"] == "waiting"
            && task["queue"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("awaiting_landing")))
        .then_some(())
    });
    assert!(!workspace_of(&service, &dependent).exists(), "no workspace before it can start");
    let preview = service.request(proto::LAND_PREVIEW, json!({
        "checkpoint_id":checkpoint["id"], "check_id":gate["id"], "target_branch":"integration",
        "command_id":"workspace-dependency-land"
    })).unwrap();
    let land = json!({"preview_id":preview["id"]});
    let wait = |busy: &str| {
        service.wait(|| {
            let receipt = service.request(proto::LAND_GET, land.clone()).unwrap();
            (receipt["status"] != busy).then_some(receipt)
        })
    };
    assert_eq!(wait("creating")["status"], "ready");
    service.request(proto::LAND_VERIFY, land.clone()).unwrap();
    assert_eq!(wait("verifying")["integration_verified"], true);
    let landed = service.request(proto::LAND_RUN, land).unwrap();
    assert_eq!(landed["status"], "landed", "{landed}");
    let dispatched = wait_queue(&service, &dependent, "dispatched");
    let path = workspace_of(&service, &dependent);
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), landed["candidate_commit"].as_str().unwrap());
    let session = service
        .request(proto::SESSION_GET, json!({"session_id":dispatched["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(session["cwd"], path.to_str().unwrap());
}

#[test]
fn task_enqueue_cli_asks_for_a_workspace_from_a_base_branch() {
    let service = Service::new();
    let repo = project(&service);
    let task = capacity_task(&service, "CLI workspace", &repo);
    let cli = |args: &[&str]| {
        Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .current_dir(&repo)
            .args(["task", "enqueue", task["id"].as_str().unwrap(), "--request-id", "cli"])
            .args(args)
            .arg("--claude")
            .arg(service.temp.path().join("claude"))
            .env("CLAUDE_CONFIG_DIR", service.temp.path().join("config"))
            .output()
            .unwrap()
    };
    let refused = cli(&["--workspace", "main", "--cwd", "."]);
    assert!(!refused.status.success(), "--workspace names the checkout; --cwd cannot also");
    let output = cli(&["--workspace", "main"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(saved["queue"]["workspace"]["base_branch"], "main");
    let path = workspace_of(&service, &task);
    assert_eq!(saved["queue"]["claude"]["cwd"], path.to_str().unwrap());
    wait_queue(&service, &task, "dispatched");
    assert!(path.join("README").is_file());
}

/// Restart the service with a short retry backoff (debug builds only), so a test can watch it.
fn respawn_with_backoff(service: &mut Service, base_ms: u64) {
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    service.process.wait().unwrap();
    service.process = Command::new(BINARY)
        .env("XAYA_AGENTD_RETRY_BASE_MS", base_ms.to_string())
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    service.wait(|| Client::connect(&service.runtime).ok());
}

fn failing_drive(service: &Service, task: &Value, request: &str, fail_times: u32) -> Value {
    let mut params = drive_params(service, task, request, "met", 0.0);
    let home = PathBuf::from(params["reviewer"]["config_dir"].as_str().unwrap());
    let mut behavior: Value =
        serde_json::from_str(&fs::read_to_string(home.join("behavior.json")).unwrap()).unwrap();
    behavior["fail_times"] = json!(fail_times);
    fs::write(home.join("behavior.json"), behavior.to_string()).unwrap();
    params["reviewer"]["config_dir"] = json!(home);
    params
}

fn reviewer_calls(params: &Value) -> usize {
    let home = PathBuf::from(params["reviewer"]["config_dir"].as_str().unwrap());
    fs::read_to_string(home.join("calls.jsonl")).map_or(0, |calls| calls.lines().count())
}

fn task_reviews(service: &Service, task: &Value) -> Vec<Value> {
    service
        .request(proto::REVIEW_LIST, json!({"task_id":task["id"]}))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn a_driven_review_that_fails_is_retaken_after_a_backoff() {
    let mut service = Service::new();
    respawn_with_backoff(&mut service, 400);
    let task = cycle_fixture(&service, 0, false);
    let params = failing_drive(&service, &task, "flaky", 2);
    service.request(proto::TASK_DRIVE, params.clone()).unwrap();
    // While it backs off the drive waits and says why.
    let waiting = service.wait(|| {
        let saved = queued_task(&service, &task);
        (saved["drive"]["state"] == "waiting"
            && saved["drive"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("retry 1 of 3")))
        .then_some(saved)
    });
    assert!(
        waiting["drive"]["reason"].as_str().unwrap().contains("fixture reviewer failure")
            || waiting["drive"]["reason"].as_str().unwrap().contains("exit"),
        "{waiting}"
    );
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    assert_eq!(reviewer_calls(&params), 3);
    let reviews = task_reviews(&service, &task);
    let failed = reviews.iter().filter(|review| review["status"] == "failed").count();
    assert_eq!((reviews.len(), failed), (3, 2), "{reviews:?}");
    let rounds = saved["drive"]["progress"]["rounds"].as_array().unwrap();
    assert_eq!(rounds.len(), 1, "a retake stays in the same round: {saved}");
    assert!(saved["attention"].is_null(), "{saved}");
}

#[test]
fn a_driven_review_stops_once_its_retries_are_used_up() {
    let mut service = Service::new();
    respawn_with_backoff(&mut service, 50);
    let task = cycle_fixture(&service, 0, false);
    let params = failing_drive(&service, &task, "broken", 100);
    service.request(proto::TASK_DRIVE, params.clone()).unwrap();
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "stopped", "{saved}");
    let reason = saved["drive"]["reason"].as_str().unwrap();
    assert!(reason.contains("after 3 retries"), "{saved}");
    assert_eq!(reviewer_calls(&params), 4, "one attempt and three retries");
}

#[test]
fn a_backoff_survives_a_restart_without_an_extra_attempt() {
    let mut service = Service::new();
    respawn_with_backoff(&mut service, 3000);
    let task = cycle_fixture(&service, 0, false);
    let params = failing_drive(&service, &task, "restart", 1);
    service.request(proto::TASK_DRIVE, params.clone()).unwrap();
    service.wait(|| {
        let saved = queued_task(&service, &task);
        saved["drive"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("retry 1 of 3"))
            .then_some(())
    });
    respawn_with_backoff(&mut service, 3000);
    assert_eq!(reviewer_calls(&params), 1, "the restart does not cut the backoff short");
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    assert_eq!(reviewer_calls(&params), 2);
}

/// A project on `main` whose verification plan is trusted in the project checkout only.
fn trusted_project(service: &Service) -> PathBuf {
    let repo = project(service);
    fs::create_dir(repo.join(".xaya")).unwrap();
    fs::write(
        repo.join(".xaya/project.toml"),
        "[verify]\nsteps = [{name = 'fixture', argv = ['/usr/bin/true']}]\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "plan"]);
    let project = service.request(proto::PROJECT_GET, json!({"cwd":repo})).unwrap();
    service
        .request(proto::PROJECT_TRUST, json!({"cwd":repo,"config_hash":project["config_hash"]}))
        .unwrap();
    repo
}

const GOAL: &str = "  \nMake mean() return 0 for an empty list\nKeep the signature as it is.";

/// Delegate through the CLI, as the panel does: Claude implements, Codex (the fixture) reviews.
fn engine_cli(service: &Service, cwd: &Path, args: &[&str]) -> Value {
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "engine"])
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn a_project_default_engine_implements_delegated_tasks_unless_one_is_named() {
    let service = Service::new();
    let repo = trusted_project(&service);
    let project = repo.join(".git").canonicalize().unwrap();
    // Without a setting, Claude implements.
    let shown = engine_cli(&service, &repo, &[]);
    assert_eq!(shown, json!({"project":project, "engine":"claude", "configured":false}));
    let board = service.request(proto::TASK_BOARD, json!({"cwd":repo})).unwrap();
    assert_eq!(board["engine"], "claude");

    // Set from another worktree, it holds for the whole repository.
    let linked = service.temp.path().join("linked");
    git(&repo, &["worktree", "add", "-q", "--detach", linked.to_str().unwrap()]);
    let set = engine_cli(&service, &linked, &["--set", "codex"]);
    assert_eq!(set, json!({"project":project, "engine":"codex", "configured":true}));
    assert_eq!(engine_cli(&service, &repo, &[])["engine"], "codex");
    let board = service.request(proto::TASK_BOARD, json!({"cwd":repo})).unwrap();
    assert_eq!(board["engine"], "codex");
    let task = delegated(&delegate_cli(&service, &repo, "by-default", "Use the default"));
    assert!(task["queue"]["codex"].is_object() && task["queue"]["claude"].is_null(), "{task}");
    assert_eq!(task["drive"]["params"]["reviewer"]["engine"], "claude", "{task}");
    // A named engine wins over the setting.
    let named = delegate_with(&service, &repo, "named", "Name the engine", &["--engine", "claude"]);
    let named = delegated(&named);
    assert!(named["queue"]["claude"].is_object(), "{named}");
    assert_eq!(named["drive"]["params"]["reviewer"]["engine"], "codex", "{named}");

    // Reset, Claude implements again.
    let reset = engine_cli(&service, &repo, &["--reset"]);
    assert_eq!(reset, json!({"project":project, "engine":"claude", "configured":false}));
    let refused = service.request(proto::TASK_ENGINE_SET, json!({"cwd":repo, "engine":"gpt"}));
    assert!(refused.is_err(), "only Claude or Codex");
}

fn delegate_cli(service: &Service, cwd: &Path, request: &str, goal: &str) -> std::process::Output {
    delegate_with(service, cwd, request, goal, &[])
}

fn delegate_with(
    service: &Service,
    cwd: &Path,
    request: &str,
    goal: &str,
    extra: &[&str],
) -> std::process::Output {
    let home = review_home(
        service,
        "delegate-review",
        json!({"review":clean(),"criteria":results(&[("AC1","met")])}),
    );
    Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "delegate", goal, "--request-id", request, "--cwd"])
        .arg(cwd)
        .arg("--claude")
        .arg(service.temp.path().join("claude"))
        .arg("--codex")
        .arg(service.temp.path().join("reviewer"))
        .args(extra)
        .env("CLAUDE_CONFIG_DIR", service.temp.path().join("config"))
        .env("CODEX_HOME", home)
        .output()
        .unwrap()
}

fn delegated(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn a_delegated_request_is_launched_in_its_own_workspace_and_driven_to_approval() {
    let service = Service::new();
    let repo = trusted_project(&service);
    let task = delegated(&delegate_cli(&service, &repo, "delegate-1", GOAL));
    let path = workspace_of(&service, &task);
    assert_eq!(task["title"], "Make mean() return 0 for an empty list", "{task}");
    assert_eq!(task["goal"], GOAL.trim(), "surrounding blank lines are dropped");
    assert_eq!(task["criteria"].as_array().unwrap().len(), 1, "{task}");
    assert_eq!(task["queue"]["workspace"]["base_branch"], "main", "{task}");
    assert_eq!(task["queue"]["claude"]["cwd"], path.to_str().unwrap(), "{task}");
    let prompt = task["queue"]["claude"]["prompt"].as_str().unwrap();
    assert!(prompt.contains("Keep the signature as it is."), "{prompt}");
    assert!(!prompt.contains("Constraints:"), "no empty constraints line: {prompt}");
    assert_eq!(task["drive"]["params"]["reviewer"]["engine"], "codex", "{task}");

    // One request is one task, however often it is sent.
    let again = delegated(&delegate_cli(&service, &repo, "delegate-1", GOAL));
    assert_eq!(again["id"], task["id"]);
    let other = delegate_cli(&service, &repo, "delegate-1", "Something else");
    assert!(!other.status.success(), "a request ID names one delegation");
    let all = service.request(proto::TASK_LIST, json!({})).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 1, "{all}");

    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["queue"]["state"], "dispatched", "{saved}");
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    let session = service
        .request(proto::SESSION_GET, json!({"session_id":saved["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(session["cwd"], path.to_str().unwrap());
    // The workspace carries the trust given to the same plan in the project checkout.
    let plan = service.request(proto::PROJECT_GET, json!({"cwd":path})).unwrap();
    assert_eq!(plan["trusted"], true, "{plan}");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "", "the project checkout is untouched");
    // The repeat still answers after the checkout has moved on.
    git(&repo, &["checkout", "-q", "--detach"]);
    assert_eq!(delegated(&delegate_cli(&service, &repo, "delegate-1", GOAL))["id"], task["id"]);
}

#[test]
fn a_delegation_needs_a_branch_and_leaves_nothing_behind_when_refused() {
    let service = Service::new();
    let repo = trusted_project(&service);
    git(&repo, &["checkout", "-q", "--detach"]);
    let refused = delegate_cli(&service, &repo, "detached", GOAL);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not on a branch"));
    let missing = delegate_cli(&service, &repo, "blank", " \n ");
    assert!(!missing.status.success(), "a goal is required");
    assert_eq!(service.request(proto::TASK_LIST, json!({})).unwrap(), json!([]));
    assert!(!service.temp.path().join("repo.xaya").exists(), "nothing is created");
    // A plan whose bytes nobody trusted stays untrusted in the workspace.
    git(&repo, &["checkout", "-q", "main"]);
    fs::write(
        repo.join(".xaya/project.toml"),
        "[verify]\nsteps = [{name = 'other', argv = ['/usr/bin/true']}]\n",
    )
    .unwrap();
    git(&repo, &["-c", "core.hooksPath=/dev/null", "commit", "-qam", "new plan"]);
    let task = delegated(&delegate_cli(&service, &repo, "untrusted", GOAL));
    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "stopped", "{saved}");
    assert!(saved["drive"]["reason"].as_str().unwrap().contains("trusted"), "{saved}");
}

fn land_cli(service: &Service, task: &Value, request: &str) -> std::process::Output {
    Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "land", task["id"].as_str().unwrap(), "--request-id", request])
        .output()
        .unwrap()
}

fn acceptance(service: &Service, task: &Value) -> Value {
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "acceptance", task["id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn board_state(service: &Service, repo: &Path, task: &Value) -> Value {
    let board =
        service.request(proto::TASK_BOARD, json!({"cwd":repo.canonicalize().unwrap()})).unwrap();
    let row = board["tasks"].as_array().unwrap().iter().find(|row| row["task_id"] == task["id"]);
    row.map_or(Value::Null, |row| row["state"].clone())
}

fn landing_outcome(service: &Service, task: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let saved = queued_task(service, task);
        if saved["landing"]["state"] != "landing" {
            return saved;
        }
        assert!(Instant::now() < deadline, "the landing did not finish: {saved}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// The landed task once the service has said what became of its workspace.
fn workspace_outcome(service: &Service, task: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let saved = queued_task(service, task);
        let outcome = &saved["landing"]["workspace"]["outcome"];
        if !outcome.is_null() && outcome != "removing" {
            return saved;
        }
        assert!(Instant::now() < deadline, "the workspace was not cleaned up: {saved}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_approved_delegation_lands_on_its_base_branch_and_is_accepted() {
    let service = Service::new();
    let repo = trusted_project(&service);
    fs::write(
        service.temp.path().join("config/first-turn-write.json"),
        json!({"path":"src/feature.txt","content":"done\n"}).to_string(),
    )
    .unwrap();
    let task = delegated(&delegate_cli(&service, &repo, "deliver", GOAL));
    assert_eq!(drive_outcome(&service, &task)["drive"]["state"], "approved");
    let before = git(&repo, &["rev-parse", "main"]);
    // What a person accepts: the reviewed change, its verification and approval.
    assert_eq!(board_state(&service, &repo, &task), "awaiting_acceptance");
    let shown = acceptance(&service, &task);
    assert_eq!(shown["blocked"], Value::Null, "{shown}");
    assert_eq!(shown["target_branch"], "main");
    assert_eq!(shown["files"], json!([{"path":"src/feature.txt","added":1,"removed":0}]));
    assert!(shown["diff"].as_str().unwrap().contains("+done"), "{shown}");
    assert_eq!(shown["diff_truncated"], false);
    assert_eq!(shown["review"]["verdict"], "approved");
    assert_eq!(shown["check"]["status"], "succeeded");

    let output = land_cli(&service, &task, "land-1");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved = landing_outcome(&service, &task);
    let landing = &saved["landing"];
    assert_eq!(landing["state"], "landed", "{saved}");
    assert_eq!(landing["target_branch"], "main", "the workspace's base by default");
    assert_eq!(saved["status"], "accepted", "a landed task is accepted: {saved}");
    // A fast-forward of the checked-out branch, and nothing else.
    assert_eq!(git(&repo, &["rev-parse", "main^"]), before);
    assert_eq!(git(&repo, &["show", "main:src/feature.txt"]), "done");
    assert_eq!(fs::read_to_string(repo.join("src/feature.txt")).unwrap(), "done\n");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "", "the project checkout is clean");
    let preview =
        service.request(proto::LAND_GET, json!({"preview_id":landing["preview_id"]})).unwrap();
    assert_eq!(preview["integration_verified"], true, "{preview}");
    assert_eq!(
        board_state(&service, &repo, &task),
        Value::Null,
        "an accepted task leaves the board"
    );

    // Its workspace goes: the implementing session is stopped and the worktree removed.
    let path = workspace_of(&service, &task);
    let saved = workspace_outcome(&service, &task);
    let cleanup = &saved["landing"]["workspace"];
    assert_eq!(cleanup["outcome"], "removed", "{saved}");
    assert_eq!(cleanup["path"], path.to_str().unwrap());
    assert!(!path.exists() && !service.temp.path().join("repo.xaya").exists());
    let worktrees = git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(path.to_str().unwrap()), "{worktrees}");
    let session = service
        .request(proto::SESSION_GET, json!({"session_id":saved["queue"]["session_id"]}))
        .unwrap();
    assert_eq!(session["state"], "stopped", "{session}");
    // The landed change can still be looked at.
    let shown = acceptance(&service, &task);
    assert_eq!(shown["blocked"], "it has landed");
    assert!(shown["diff"].as_str().unwrap().contains("+done"), "{shown}");

    // The same request answers with the record; another cannot land it again.
    let again = land_cli(&service, &task, "land-1");
    assert!(again.status.success());
    let repeated: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(repeated["landing"], saved["landing"]);
    assert!(!land_cli(&service, &task, "land-2").status.success());
}

#[test]
fn a_task_lands_only_after_a_current_approval() {
    let service = Service::new();
    let repo = project(&service);
    let task = capacity_task(&service, "Not reviewed", &repo);
    let shown = acceptance(&service, &task);
    assert_eq!(shown["blocked"], "the task has no review yet", "{shown}");
    assert_eq!(shown["files"], json!([]));
    // Without a workspace there is no default target.
    let params = json!({"task_id":task["id"],"request_id":"early"});
    assert!(service.request(proto::TASK_LAND, params.clone()).is_err());
    let mut named = params;
    named["target_branch"] = json!("main");
    let landing = service.request(proto::TASK_LAND, named.clone()).unwrap();
    assert_eq!(landing["landing"]["state"], "landing");
    let saved = landing_outcome(&service, &task);
    assert_eq!(saved["landing"]["state"], "failed", "{saved}");
    assert!(saved["landing"]["reason"].as_str().unwrap().contains("no review yet"), "{saved}");
    assert!(saved["landing"]["preview_id"].is_null(), "nothing was previewed");
    assert_eq!(saved["status"], "open");
    assert_eq!(git(&repo, &["rev-parse", "main"]), git(&repo, &["rev-parse", "HEAD"]));
    // A failed landing can be authorized again with a new request.
    named["request_id"] = json!("later");
    assert_eq!(service.request(proto::TASK_LAND, named).unwrap()["landing"]["request_id"], "later");
}

fn revise_cli(
    service: &Service,
    task: &Value,
    request: &str,
    feedback: &str,
) -> std::process::Output {
    Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["task", "revise", task["id"].as_str().unwrap(), "--request-id", request, "--"])
        .arg(feedback)
        .output()
        .unwrap()
}

#[test]
fn requested_changes_go_back_to_the_session_and_are_reviewed_again() {
    let service = Service::new();
    let repo = trusted_project(&service);
    let config = service.temp.path().join("config");
    fs::write(
        config.join("first-turn-write.json"),
        json!({"path":"src/feature.txt","content":"done\n"}).to_string(),
    )
    .unwrap();
    let task = delegated(&delegate_cli(&service, &repo, "deliver", GOAL));
    assert_eq!(drive_outcome(&service, &task)["drive"]["state"], "approved");
    let approved = acceptance(&service, &task)["review"]["id"].clone();
    let revisable = service.request(
        proto::TASK_CREATE,
        json!({
            "title":"Manual","goal":"Manual","criteria":["passes"],"project":repo
        }),
    );
    let manual = revisable.unwrap();
    assert!(!revise_cli(&service, &manual, "change-0", "More").status.success(), "not driven");
    assert!(!revise_cli(&service, &task, "blank", " \n ").status.success());

    // The fix turn takes a moment: long enough to see the change under way.
    fs::write(config.join("send-delay"), "2").unwrap();
    fs::write(
        config.join("send-write.json"),
        json!({"path":"src/notes.txt","content":"noted\n"}).to_string(),
    )
    .unwrap();
    let feedback = "Also write down what changed in src/notes.txt.";
    let output = revise_cli(&service, &task, "change-1", feedback);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let revised: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(revised["drive"]["state"], "driving", "{revised}");
    assert_eq!(revised["revisions"][0]["feedback"], feedback);
    assert_eq!(revised["revisions"][0]["review_id"], approved);
    // While changes are under way: no landing, no second request, nothing to accept.
    let again = revise_cli(&service, &task, "change-1", feedback);
    assert!(again.status.success(), "the same request returns the record");
    let repeated: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(repeated["revisions"].as_array().unwrap().len(), 1);
    assert!(!revise_cli(&service, &task, "change-1", "Other").status.success());
    assert!(!revise_cli(&service, &task, "change-2", "More").status.success());
    assert!(!land_cli(&service, &task, "land-early").status.success());
    assert!(acceptance(&service, &task)["blocked"].as_str().unwrap().contains("under way"));
    assert_ne!(board_state(&service, &repo, &task), "awaiting_acceptance");

    let saved = drive_outcome(&service, &task);
    assert_eq!(saved["drive"]["state"], "approved", "{saved}");
    let sent = fs::read_to_string(config.join("controls.jsonl")).unwrap();
    assert!(sent.contains(feedback), "the person's words reach the session: {sent}");
    // A new checkpoint holds both changes, verified and reviewed again.
    let shown = acceptance(&service, &task);
    assert_ne!(shown["review"]["id"], approved);
    assert_eq!(shown["blocked"], Value::Null, "{shown}");
    let files: Vec<_> =
        shown["files"].as_array().unwrap().iter().map(|file| file["path"].clone()).collect();
    assert_eq!(files, [json!("src/feature.txt"), json!("src/notes.txt")]);
    assert_eq!(saved["fix_rounds"], json!([]), "a person's request uses no fix budget");
}
