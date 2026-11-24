#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use xaya_agent_proto::{self as proto, AttachInfo, Session, SessionParams, StartSessionParams};
use xaya_agentd::client::Client;
use xaya_agentd::runtime::Runtime;

const BINARY: &str = env!("CARGO_BIN_EXE_xaya-agentd");

#[tokio::test]
async fn checkpoints_are_durable_deduplicated_and_available_through_the_cli() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    drop(ws);
    let root = service.temp.path();
    let git = |args: &[&str]| {
        let output = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Checkpoint fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    fs::write(root.join(".gitignore"), "run/\ncodex-home/\n").unwrap();
    fs::write(root.join("source"), "original\n").unwrap();
    git(&["add", "source", ".gitignore"]);
    git(&["commit", "-qm", "initial"]);
    fs::write(root.join("source"), "captured\n").unwrap();
    let head = git(&["rev-parse", "HEAD"]);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["checkpoint", "create", &session.id, "--command-id", "durable-cp", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cp: proto::Checkpoint = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    assert_eq!(cp.base_commit.as_deref(), Some(head.as_str()));
    assert_eq!(git(&["rev-parse", "HEAD"]), head);
    assert_eq!(git(&["show", &format!("{}:source", cp.commit.as_ref().unwrap())]), "captured");
    service.restart(false);
    let restored: proto::Checkpoint =
        service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
    assert_eq!(restored.commit, cp.commit);
    let duplicate: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"durable-cp"}),
    );
    assert_eq!(duplicate.id, cp.id);
    assert_eq!(duplicate.status, proto::CheckpointStatus::Ready);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["checkpoint", "show", &cp.id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let shown: proto::Checkpoint = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown.tree, cp.tree);
}

#[tokio::test]
async fn checks_bind_checkpoint_preserve_index_and_expire_without_replaying_commands() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    drop(ws);
    let root = service.temp.path();
    let git = |args: &[&str]| {
        let out = Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Check fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    fs::write(root.join(".gitignore"), "run/\ncodex-home/\ngenerated\n").unwrap();
    fs::write(root.join("source"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let cp: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"check-base"}),
    );
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    let index = fs::read(root.join(".git/index")).unwrap();
    let argv = ["/usr/bin/printf", "%s", "literal; $(touch UNEXPECTED)"];
    let params =
        json!({"checkpoint_id":cp.id,"command_id":"verify-once","argv":argv,"timeout_secs":10});
    let check: proto::Check = service.request(proto::CHECK_RUN, params.clone());
    let done = service.wait(|| {
        let c: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":check.id}));
        (c.status != proto::CheckStatus::Running).then_some(c)
    });
    assert_eq!(done.status, proto::CheckStatus::Succeeded, "{:?}", done);
    assert!(!done.stale);
    assert_eq!(done.tree, cp.tree.unwrap());
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert!(!root.join("UNEXPECTED").exists());
    let log: proto::CheckLog =
        service.request(proto::CHECK_LOG, json!({"check_id":check.id,"offset":0}));
    assert!(log.text.contains("literal; $(touch UNEXPECTED)"));
    service.restart(false);
    let duplicate: proto::Check = service.request(proto::CHECK_RUN, params.clone());
    assert_eq!(duplicate.id, check.id);
    assert_eq!(duplicate.status, proto::CheckStatus::Succeeded);
    let mut conflict = params.clone();
    conflict["argv"] = json!(["/usr/bin/true"]);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::Check>(proto::CHECK_RUN, conflict)
            .is_err()
    );
    fs::write(service.temp.path().join("source"), "changed\n").unwrap();
    let stale: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":check.id}));
    assert!(stale.stale);
    let duplicate: proto::Check = service.request(proto::CHECK_RUN, params);
    assert!(duplicate.stale);
    fs::write(service.temp.path().join("source"), "base\n").unwrap();
    let still_stale: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":check.id}));
    assert!(still_stale.stale, "invalidation must be sticky");
    fs::write(service.temp.path().join("source"), "changed\n").unwrap();
    let rejected: proto::Check = service.request(proto::CHECK_RUN,
        json!({"checkpoint_id":cp.id,"command_id":"stale-check","argv":["/usr/bin/touch","UNEXPECTED"],"timeout_secs":10}));
    let rejected = service.wait(|| {
        let c: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":rejected.id}));
        (c.status != proto::CheckStatus::Running).then_some(c)
    });
    assert_eq!(rejected.status, proto::CheckStatus::Failed);
    assert!(rejected.stale);
    assert!(!service.temp.path().join("UNEXPECTED").exists());
}

#[tokio::test]
async fn project_verification_requires_matching_trust_and_runs_steps_in_order() {
    let mut service = Service::new();
    let (session, _) = checkpoint_fixture(&service).await;
    let root = service.temp.path();
    fs::create_dir(root.join(".xaya")).unwrap();
    fs::create_dir(root.join("generated")).unwrap();
    let config = r#"[verify]
timeout = "10s"
steps = [
  { name = "prepare", argv = ["python3", "-c", "from pathlib import Path; Path('generated/first').write_text('ready')"] },
  { name = "test", cwd = "generated", argv = ["python3", "-c", "from pathlib import Path; assert Path('first').read_text() == 'ready'; print('ordered')"] },
]
"#;
    fs::write(root.join(".xaya/project.toml"), config).unwrap();
    let cp: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"project-base"}),
    );
    let cp = service.wait(|| {
        let c: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (c.status != proto::CheckpointStatus::Creating).then_some(c)
    });
    let info: Value = service.request("project.get", json!({"cwd":root}));
    assert_eq!(info["trusted"], false);
    assert!(Client::connect(&service.runtime).unwrap().request::<Value>("check.project",
        json!({"checkpoint_id":cp.id,"command_id":"project-check","config_hash":info["config_hash"]})).is_err());
    assert!(!root.join("generated/first").exists());
    let _: Value =
        service.request("project.trust", json!({"cwd":root,"config_hash":info["config_hash"]}));
    let check: proto::Check = service.request("check.project",
        json!({"checkpoint_id":cp.id,"command_id":"project-check","config_hash":info["config_hash"]}));
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Succeeded, "{done:?}");
    let done = serde_json::to_value(done).unwrap();
    assert_eq!(done["steps"][0]["status"], "succeeded");
    assert_eq!(done["steps"][1]["status"], "succeeded");
    assert_eq!(done["project_hash"], info["config_hash"]);
    service.restart(false);
    let root = service.temp.path();
    let trusted: Value = service.request("project.get", json!({"cwd":root}));
    assert_eq!(trusted["trusted"], true);
    fs::write(root.join(".xaya/project.toml"), config.replace("ordered", "changed")).unwrap();
    let untrusted: Value = service.request("project.get", json!({"cwd":root}));
    assert_eq!(untrusted["trusted"], false);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>(
                "project.trust",
                json!({"cwd":root,"config_hash":info["config_hash"]})
            )
            .is_err()
    );
    assert!(Client::connect(&service.runtime).unwrap().request::<Value>("check.project",
        json!({"checkpoint_id":cp.id,"command_id":"new-check","config_hash":info["config_hash"]})).is_err());
    let duplicate: proto::Check = service.request("check.project",
        json!({"checkpoint_id":cp.id,"command_id":"project-check","config_hash":info["config_hash"]}));
    assert_eq!(duplicate.id, check.id);
    assert!(duplicate.stale);
}

fn git_at(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

async fn landing_fixture(service: &Service) -> (Session, proto::Checkpoint, proto::Check) {
    let (session, _) = checkpoint_fixture(service).await;
    let root = service.temp.path();
    git_at(root, &["branch", "-m", "task"]);
    git_at(root, &["branch", "landing-target"]);
    fs::write(root.join("source"), "candidate\n").unwrap();
    let cp: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"landing-base"}),
    );
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"landing-check","argv":["/usr/bin/true"],"timeout_secs":10}));
    let check = check_done(service, &check.id);
    assert_eq!(check.status, proto::CheckStatus::Succeeded);
    (session, cp, check)
}

fn preview_done(service: &Service, id: &str) -> Value {
    service.wait(|| {
        let preview: Value = service.request("land.get", json!({"preview_id":id}));
        (preview["status"] != "creating").then_some(preview)
    })
}

#[tokio::test]
async fn integrated_candidate_is_verified_in_private_checkout_then_landed_once() {
    let mut service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let head = git_at(service.temp.path(), &["rev-parse", "HEAD"]);
    let index = fs::read(service.temp.path().join(".git/index")).unwrap();
    let preview: Value = service.request(
        "land.preview",
        json!({"checkpoint_id":cp.id,
        "check_id":check.id,"target_branch":"landing-target","command_id":"integrated"}),
    );
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.run", json!({"preview_id":done["id"]}))
            .is_err()
    );
    let _: Value = service.request("land.verify", json!({"preview_id":done["id"]}));
    let verified = service.wait(|| {
        let p: Value = service.request("land.get", json!({"preview_id":done["id"]}));
        (p["status"] != "verifying").then_some(p)
    });
    assert_eq!(verified["integration_verified"], true, "{verified}");
    let check: proto::Check =
        service.request("check.get", json!({"check_id":verified["integration_check_id"]}));
    assert_eq!(check.status, proto::CheckStatus::Succeeded);
    assert_ne!(check.cwd, service.temp.path().to_str().unwrap());
    assert!(!std::path::Path::new(&check.cwd).exists());
    let landed: Value = service.request("land.run", json!({"preview_id":done["id"]}));
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(
        git_at(service.temp.path(), &["rev-parse", "landing-target"]),
        done["candidate_commit"]
    );
    assert_eq!(git_at(service.temp.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(service.temp.path().join(".git/index")).unwrap(), index);
    service.restart(false);
    let duplicate: Value = service.request("land.run", json!({"preview_id":done["id"]}));
    assert_eq!(duplicate["status"], "landed");
}

fn integration_done(service: &Service, id: &Value) -> Value {
    service.wait(|| {
        let p: Value = service.request("land.get", json!({"preview_id":id}));
        (p["status"] != "verifying").then_some(p)
    })
}

async fn integration_fixture(service: &Service, script: &str, timeout: u64) -> Value {
    let (_, cp, _) = landing_fixture(service).await;
    let root = service.temp.path();
    // Target-only content ensures commands really see the merged candidate.
    let target = tempfile::tempdir().unwrap();
    git_at(root, &[
        "worktree",
        "add",
        "--quiet",
        target.path().to_str().unwrap(),
        "landing-target",
    ]);
    fs::write(target.path().join("target-only"), "integrated\n").unwrap();
    git_at(target.path(), &["add", "target-only"]);
    git_at(target.path(), &["commit", "-qm", "target-only change"]);
    git_at(root, &["worktree", "remove", target.path().to_str().unwrap()]);
    let check: proto::Check = service.request(
        "check.run",
        json!({"checkpoint_id":cp.id,
        "command_id":"integration-plan","argv":["/bin/sh","-c",script],"timeout_secs":timeout}),
    );
    assert_eq!(check_done(service, &check.id).status, proto::CheckStatus::Succeeded);
    let p: Value = service.request(
        "land.preview",
        json!({"checkpoint_id":cp.id,
        "check_id":check.id,"target_branch":"landing-target","command_id":"integration-preview"}),
    );
    let p = preview_done(service, p["id"].as_str().unwrap());
    assert_eq!(p["status"], "ready", "{p}");
    p
}

#[tokio::test]
async fn integrated_verification_failure_and_source_mutation_never_advance_target() {
    for script in ["test ! -e target-only", "if [ -e target-only ]; then echo changed > source; fi"]
    {
        let service = Service::new();
        let p = integration_fixture(&service, script, 10).await;
        let target = git_at(service.temp.path(), &["rev-parse", "landing-target"]);
        let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
        let done = integration_done(&service, &p["id"]);
        assert_eq!(done["status"], "failed", "{done}");
        assert_eq!(done["integration_verified"], false);
        let check: proto::Check =
            service.request("check.get", json!({"check_id":done["integration_check_id"]}));
        assert_eq!(check.status, proto::CheckStatus::Failed);
        assert!(
            Client::connect(&service.runtime)
                .unwrap()
                .request::<Value>("land.run", json!({"preview_id":p["id"]}))
                .is_err()
        );
        assert_eq!(git_at(service.temp.path(), &["rev-parse", "landing-target"]), target);
        assert_eq!(fs::read_to_string(service.temp.path().join("source")).unwrap(), "candidate\n");
    }
}

#[tokio::test]
async fn integrated_verification_supports_cancel_timeout_and_durable_logs() {
    for cancel in [false, true] {
        let service = Service::new();
        let p = integration_fixture(
            &service,
            "if [ -e target-only ]; then echo integration-started; sleep 30; fi",
            if cancel { 10 } else { 1 },
        )
        .await;
        let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
        let running = service.wait(|| {
            let p: Value = service.request("land.get", json!({"preview_id":p["id"]}));
            (!p["integration_check_id"].is_null()).then_some(p)
        });
        if cancel {
            let _: proto::Check = service
                .request("check.cancel", json!({"check_id":running["integration_check_id"]}));
        }
        let done = integration_done(&service, &p["id"]);
        let check: proto::Check =
            service.request("check.get", json!({"check_id":done["integration_check_id"]}));
        assert_eq!(
            check.status,
            if cancel { proto::CheckStatus::Cancelled } else { proto::CheckStatus::TimedOut },
            "{check:?}"
        );
        assert_eq!(done["integration_verified"], false);
        assert!(!std::path::Path::new(&check.root).exists());
        if !cancel {
            let log: proto::CheckLog = service.request("check.log", json!({"check_id":check.id}));
            assert!(log.text.contains("integration-started"));
        }
    }
}

#[tokio::test]
async fn verified_landing_rejects_target_movement_and_advances_clean_checkout() {
    for checked_out in [false, true] {
        let service = Service::new();
        let mut p = integration_fixture(&service, "true", 10).await;
        let target = tempfile::tempdir().unwrap();
        if checked_out {
            git_at(service.temp.path(), &[
                "worktree",
                "add",
                "--quiet",
                target.path().to_str().unwrap(),
                "landing-target",
            ]);
            let next: Value = service.request("land.preview", json!({"checkpoint_id":p["checkpoint_id"],
                "check_id":p["check_id"],"target_branch":"landing-target","command_id":"owned-preview"}));
            p = preview_done(&service, next["id"].as_str().unwrap());
            let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
            assert_eq!(integration_done(&service, &p["id"])["integration_verified"], true);
        } else {
            let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
            assert_eq!(integration_done(&service, &p["id"])["integration_verified"], true);
            let old = p["target_head"].as_str().unwrap();
            let commit = git_at(service.temp.path(), &[
                "commit-tree",
                p["candidate_tree"].as_str().unwrap(),
                "-p",
                old,
                "-m",
                "moved target",
            ]);
            git_at(service.temp.path(), &["update-ref", "refs/heads/landing-target", &commit, old]);
        }
        let before = git_at(service.temp.path(), &["rev-parse", "landing-target"]);
        let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
        if checked_out {
            assert_eq!(result["status"], "landed", "{result}");
            assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["candidate_commit"]);
            assert_eq!(git_at(target.path(), &["write-tree"]), p["candidate_tree"]);
            assert_eq!(fs::read_to_string(target.path().join("source")).unwrap(), "candidate\n");
            assert!(git_at(target.path(), &["status", "--porcelain"]).is_empty());
        } else {
            assert_eq!(result["status"], "needs_attention", "{result}");
            assert_eq!(git_at(service.temp.path(), &["rev-parse", "landing-target"]), before);
        }
    }
}

async fn owned_landing_fixture(service: &Service, incoming: bool) -> (Value, TempDir) {
    let (session, ..) = landing_fixture(service).await;
    let root = service.temp.path();
    if incoming {
        fs::write(root.join("run/incoming"), "candidate-added\n").unwrap();
        git_at(root, &["add", "-f", "run/incoming"]);
    }
    let cp: proto::Checkpoint = service
        .request("checkpoint.create", json!({"session_id":session.id,"command_id":"owned-cp"}));
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request("checkpoint.get", json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    let check: proto::Check = service.request("check.run", json!({"checkpoint_id":cp.id,"command_id":"owned-check","argv":["/usr/bin/true"],"timeout_secs":10}));
    assert_eq!(check_done(service, &check.id).status, proto::CheckStatus::Succeeded);
    let target = tempfile::tempdir().unwrap();
    git_at(root, &[
        "worktree",
        "add",
        "--quiet",
        target.path().to_str().unwrap(),
        "landing-target",
    ]);
    let p: Value = service.request("land.preview", json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"owned-preview"}));
    let p = preview_done(service, p["id"].as_str().unwrap());
    assert_eq!(p["status"], "ready", "{p}");
    let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    let p = integration_done(service, &p["id"]);
    assert_eq!(p["integration_verified"], true, "{p}");
    (p, target)
}

#[tokio::test]
async fn checked_out_landing_preserves_ignored_files_and_rejects_collisions() {
    for collision in ["file", "directory", "unrelated"] {
        let service = Service::new();
        let (p, target) = owned_landing_fixture(&service, true).await;
        fs::create_dir(target.path().join("run")).unwrap();
        let sentinel = match collision {
            "file" => target.path().join("run/incoming"),
            "directory" => {
                fs::create_dir(target.path().join("run/incoming")).unwrap();
                target.path().join("run/incoming/keep")
            },
            _ => target.path().join("run/unrelated"),
        };
        fs::write(&sentinel, "local-ignored\n").unwrap();
        let index_path =
            git_at(target.path(), &["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        let index = fs::read(&index_path).unwrap();
        let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "local-ignored\n");
        let head_lock = format!(
            "{}.lock",
            git_at(target.path(), &["rev-parse", "--path-format=absolute", "--git-path", "HEAD"])
        );
        assert!(!std::path::Path::new(&head_lock).exists(), "transaction leaked HEAD.lock");
        assert!(
            !service.temp.path().join(".git/refs/heads/landing-target.lock").exists(),
            "transaction leaked branch.lock"
        );
        if collision == "unrelated" {
            assert_eq!(result["status"], "landed", "{result}");
            assert_eq!(
                fs::read_to_string(target.path().join("run/incoming")).unwrap(),
                "candidate-added\n"
            );
        } else {
            assert_eq!(result["landing_outcome"], "not_applied", "{result}");
            assert_eq!(result["status"], "blocked");
            assert!(result["error"].as_str().unwrap().contains("ignored"), "{result}");
            assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["target_head"]);
            assert_eq!(fs::read(index_path).unwrap(), index);
        }
    }
}

#[tokio::test]
async fn checked_out_landing_rejects_existing_git_locks_without_removing_them() {
    for lock_index in [false, true] {
        let service = Service::new();
        let (p, target) = owned_landing_fixture(&service, false).await;
        let path = if lock_index {
            format!(
                "{}.lock",
                git_at(target.path(), &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "index"
                ])
            )
        } else {
            service
                .temp
                .path()
                .join(".git/refs/heads/landing-target.lock")
                .to_str()
                .unwrap()
                .to_owned()
        };
        fs::write(&path, "external-lock").unwrap();
        let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
        assert_eq!(result["landing_outcome"], "unknown", "{result}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "external-lock");
        assert_eq!(fs::read_to_string(target.path().join("source")).unwrap(), "base\n");
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["target_head"]);
    }
}

#[tokio::test]
async fn interrupted_landing_reconciliation_samples_without_replay_or_rollback() {
    for state in ["old", "applied", "partial", "moved"] {
        let mut service = Service::new();
        let (p, target) = owned_landing_fixture(&service, false).await;
        let candidate = p["candidate_commit"].as_str().unwrap();
        match state {
            "applied" => {
                git_at(target.path(), &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "merge",
                    "--ff-only",
                    candidate,
                ]);
            },
            "partial" => {
                git_at(target.path(), &[
                    "read-tree",
                    "-m",
                    "-u",
                    p["target_head"].as_str().unwrap(),
                    candidate,
                ]);
            },
            "moved" => {
                git_at(target.path(), &["commit", "--allow-empty", "-qm", "external target"]);
            },
            _ => (),
        }
        let before = git_at(target.path(), &["rev-parse", "HEAD"]);
        let index_path =
            git_at(target.path(), &["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        let index = fs::read(index_path.clone()).unwrap();
        let db = rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db"))
            .unwrap();
        let mut receipt = p.clone();
        receipt["status"] = json!("landing");
        receipt["landing_outcome"] = json!("unknown");
        db.execute("UPDATE land_previews SET record=?1 WHERE id=?2", rusqlite::params![
            receipt.to_string(),
            p["id"].as_str().unwrap()
        ])
        .unwrap();
        service.restart(false);
        let restored: Value = service.request("land.get", json!({"preview_id":p["id"]}));
        assert_eq!(restored["status"], "interrupted");
        let result: Value = service.request("land.reconcile", json!({"preview_id":p["id"]}));
        assert_eq!(
            result["landing_outcome"],
            match state {
                "old" => "not_applied",
                "applied" => "applied",
                _ => "needs_attention",
            },
            "{result}"
        );
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), before);
        assert_eq!(fs::read(index_path).unwrap(), index);
        let retry: Value = service.request("land.run", json!({"preview_id":p["id"]}));
        assert_eq!(retry["landing_outcome"], result["landing_outcome"]);
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), before);
    }
}

#[tokio::test]
async fn checked_out_landing_ignores_large_unrelated_build_output() {
    let service = Service::new();
    let (p, target) = owned_landing_fixture(&service, true).await;
    // Real targets carry build trees far larger than any bounded Git listing.
    let build = target.path().join("run/build-output/deeply/nested/artifact-directory");
    fs::create_dir_all(&build).unwrap();
    for i in 0..6000 {
        fs::write(build.join(format!("object-file-with-a-realistic-long-name-{i:05}.o")), "x")
            .unwrap();
    }
    let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
    assert_eq!(result["status"], "landed", "{result}");
    assert_eq!(
        fs::read_to_string(build.join("object-file-with-a-realistic-long-name-00000.o")).unwrap(),
        "x"
    );
    assert!(git_at(target.path(), &["status", "--porcelain"]).is_empty());
}

fn git_path(root: &std::path::Path, name: &str) -> String {
    git_at(root, &["rev-parse", "--path-format=absolute", "--git-path", name])
}

/// Persist an uncertain landing receipt as a crash would leave it, then restart.
fn interrupt_landing(service: &mut Service, p: &Value, outcome: &str) {
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    let mut receipt = p.clone();
    receipt["status"] = json!(if outcome == "not_applied" { "blocked" } else { "landing" });
    receipt["landing_outcome"] = json!(outcome);
    receipt["stale"] = json!(outcome == "not_applied");
    db.execute("UPDATE land_previews SET record=?1 WHERE id=?2", rusqlite::params![
        receipt.to_string(),
        p["id"].as_str().unwrap()
    ])
    .unwrap();
    service.restart(false);
}

fn try_land(service: &Service, method: &str, p: &Value) -> Result<Value, String> {
    Client::connect(&service.runtime)
        .unwrap()
        .request::<Value>(method, json!({"preview_id":p["id"]}))
        .map_err(|error| error.to_string())
}

fn landing_leftovers(target: &std::path::Path, id: &str) -> Vec<String> {
    let index = git_path(target, "index");
    let private = git_path(target, &format!("index.xaya-{id}"));
    [format!("{index}.lock"), private.clone(), format!("{private}.lock")]
        .into_iter()
        .filter(|path| std::path::Path::new(path).exists())
        .collect()
}

#[tokio::test]
async fn interrupted_checked_out_landing_resumes_from_every_provable_state() {
    for state in
        ["lock-only", "half", "files-applied", "index-applied", "ref-applied", "not-applied"]
    {
        let mut service = Service::new();
        let (p, target) = owned_landing_fixture(&service, true).await;
        let id = p["id"].as_str().unwrap();
        let old = p["target_head"].as_str().unwrap();
        let candidate = p["candidate_commit"].as_str().unwrap();
        let token = format!("xaya-landing {id}\n");
        let index = git_path(target.path(), "index");
        match state {
            "lock-only" => fs::write(format!("{index}.lock"), &token).unwrap(),
            "half" => {
                fs::write(format!("{index}.lock"), &token).unwrap();
                fs::write(target.path().join("source"), "candidate\n").unwrap();
            },
            "files-applied" => {
                let private = target.path().join("private-index");
                fs::copy(&index, &private).unwrap();
                let status = Command::new("git")
                    .current_dir(target.path())
                    .env("GIT_INDEX_FILE", &private)
                    .args(["read-tree", "-m", "-u", old, candidate])
                    .status()
                    .unwrap();
                assert!(status.success());
                fs::remove_file(private).unwrap();
                fs::write(format!("{index}.lock"), &token).unwrap();
            },
            "index-applied" => {
                git_at(target.path(), &["read-tree", "-m", "-u", old, candidate]);
            },
            // The reference moved but the final receipt never became durable.
            "ref-applied" => {
                git_at(target.path(), &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "merge",
                    "--ff-only",
                    candidate,
                ]);
            },
            _ => (),
        }
        interrupt_landing(
            &mut service,
            &p,
            if state == "not-applied" { "not_applied" } else { "unknown" },
        );
        let result = try_land(&service, "land.resume", &p).unwrap();
        assert_eq!(result["status"], "landed", "{state}: {result}");
        assert_eq!(result["landing_outcome"], "applied", "{state}");
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), candidate, "{state}");
        assert!(git_at(target.path(), &["status", "--porcelain"]).is_empty(), "{state}");
        assert_eq!(
            fs::read_to_string(target.path().join("run/incoming")).unwrap(),
            "candidate-added\n"
        );
        assert!(landing_leftovers(target.path(), id).is_empty(), "{state}");
        // A confirmed result is history: resuming again never repeats an update.
        let again = try_land(&service, "land.resume", &p).unwrap();
        assert_eq!(again["status"], "landed");
    }
}

#[tokio::test]
async fn resume_refuses_unproven_target_states_without_writing() {
    for state in
        ["third-content", "outside-edit", "foreign-lock", "private-lock", "moved", "ignored-dir"]
    {
        let mut service = Service::new();
        let (p, target) = owned_landing_fixture(&service, true).await;
        let id = p["id"].as_str().unwrap();
        let index = git_path(target.path(), "index");
        match state {
            "third-content" => fs::write(target.path().join("source"), "user\n").unwrap(),
            "outside-edit" => fs::write(target.path().join(".gitignore"), "user\n").unwrap(),
            "foreign-lock" => fs::write(format!("{index}.lock"), "another git\n").unwrap(),
            "private-lock" => {
                fs::write(git_path(target.path(), &format!("index.xaya-{id}.lock")), "").unwrap()
            },
            "moved" => {
                git_at(target.path(), &["commit", "--allow-empty", "-qm", "external target"]);
            },
            _ => {
                fs::create_dir_all(target.path().join("run/incoming")).unwrap();
                fs::write(target.path().join("run/incoming/keep"), "local-ignored\n").unwrap();
            },
        }
        let head = git_at(target.path(), &["rev-parse", "HEAD"]);
        let index_bytes = fs::read(&index).unwrap();
        let files: Vec<_> = ["source", ".gitignore"]
            .iter()
            .map(|name| fs::read(target.path().join(name)).unwrap())
            .collect();
        interrupt_landing(&mut service, &p, "unknown");
        let result = try_land(&service, "land.resume", &p);
        let result = result.unwrap();
        assert_ne!(result["status"], "landed", "{state}: {result}");
        let reason = match state {
            "third-content" => "neither the old version nor the candidate: source",
            "outside-edit" => "dirty",
            "foreign-lock" => "locked by another Git operation",
            "private-lock" => "may still be running",
            "moved" => "ownership changed",
            _ => "collide with a local ignored file or directory: run/incoming",
        };
        assert!(result["error"].as_str().unwrap().contains(reason), "{state}: {result}");
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), head, "{state}");
        assert_eq!(fs::read(&index).unwrap(), index_bytes, "{state}");
        for (name, bytes) in ["source", ".gitignore"].iter().zip(files) {
            assert_eq!(fs::read(target.path().join(name)).unwrap(), bytes, "{state}");
        }
        assert!(!target.path().join("run/incoming").is_file(), "{state}");
        if state == "foreign-lock" {
            assert_eq!(fs::read_to_string(format!("{index}.lock")).unwrap(), "another git\n");
        }
    }
}

#[tokio::test]
async fn landing_recovery_does_not_require_the_origin_session() {
    let mut service = Service::new();
    let (p, target) = owned_landing_fixture(&service, true).await;
    let index = git_path(target.path(), "index");
    fs::write(format!("{index}.lock"), format!("xaya-landing {}\n", p["id"].as_str().unwrap()))
        .unwrap();
    interrupt_landing(&mut service, &p, "unknown");
    // Capacity eviction removes stopped sessions from memory and storage.
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute("DELETE FROM sessions WHERE id=?1", rusqlite::params![
        p["session_id"].as_str().unwrap()
    ])
    .unwrap();
    service.restart(false);
    let sampled = try_land(&service, "land.reconcile", &p).unwrap();
    assert_eq!(sampled["landing_outcome"], "unknown", "{sampled}");
    let result = try_land(&service, "land.resume", &p).unwrap();
    assert_eq!(result["status"], "landed", "{result}");
}

#[tokio::test]
async fn integration_cleanup_rejects_live_workers_and_reclaims_only_owned_directory_after_exit() {
    let mut service = Service::new();
    let p = integration_fixture(&service, "if [ -e target-only ]; then echo $$; sleep 30; fi", 60)
        .await;
    let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    let (check, pid) = service.wait(|| {
        let p: Value = service.request("land.get", json!({"preview_id":p["id"]}));
        let id = p["integration_check_id"].as_str()?;
        let check: proto::Check = service.request("check.get", json!({"check_id":id}));
        let log: proto::CheckLog = service.request("check.log", json!({"check_id":id}));
        let pid = log.text.trim().parse::<i32>().ok()?;
        Some((check, pid))
    });
    assert!(std::path::Path::new(&check.root).exists());
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("check.cleanup", json!({"check_id":check.id}))
            .is_err()
    );
    service.restart(true);
    let interrupted: proto::Check = service.request("check.get", json!({"check_id":check.id}));
    assert_eq!(interrupted.status, proto::CheckStatus::Interrupted);
    assert!(interrupted.integration_cleanup_pending);
    assert!(std::path::Path::new(&check.root).exists());
    // SAFETY: the test-owned still-live verifier reported its process-group leader above.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    service.wait(|| {
        let c: proto::Check = service.request("check.cleanup", json!({"check_id":check.id}));
        (!c.integration_cleanup_pending).then_some(c)
    });
    assert!(!std::path::Path::new(&check.root).exists());
    let unrecorded = service.runtime.directory().join("state/integrations/unrecorded");
    fs::create_dir(&unrecorded).unwrap();
    fs::write(unrecorded.join("keep"), "not-owned").unwrap();
    service.restart(false);
    assert_eq!(fs::read_to_string(unrecorded.join("keep")).unwrap(), "not-owned");
}

#[tokio::test]
async fn integration_cleanup_preserves_checkout_when_ownership_marker_is_replaced() {
    let service = Service::new();
    let p = integration_fixture(&service, "if [ -e target-only ]; then echo $$; sleep 30; fi", 60)
        .await;
    let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    let check: proto::Check = service.wait(|| {
        let p: Value = service.request("land.get", json!({"preview_id":p["id"]}));
        let id = p["integration_check_id"].as_str()?;
        let log: proto::CheckLog = service.request("check.log", json!({"check_id":id}));
        if log.text.trim().parse::<i32>().is_err() {
            return None;
        }
        Some(service.request("check.get", json!({"check_id":id})))
    });
    let owner = std::path::Path::new(&check.root).parent().unwrap().join("owner");
    let token = fs::read(&owner).unwrap();
    fs::write(&owner, "changed-owner").unwrap();
    let _: proto::Check = service.request("check.cancel", json!({"check_id":check.id}));
    let done = integration_done(&service, &p["id"]);
    assert_eq!(done["status"], "failed");
    let pending: proto::Check = service.request("check.cleanup", json!({"check_id":check.id}));
    assert!(pending.integration_cleanup_pending);
    assert!(pending.integration_cleanup_error.unwrap().contains("token changed"));
    assert!(std::path::Path::new(&check.root).exists());
    fs::write(&owner, token).unwrap();
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "cleanup", &check.id])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!std::path::Path::new(&check.root).exists());
}

#[tokio::test]
async fn checked_out_landing_reconciles_a_failed_final_receipt_without_reapplying() {
    let mut service = Service::new();
    let (p, target) = owned_landing_fixture(&service, false).await;
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_landed BEFORE INSERT ON land_previews WHEN \
         json_extract(NEW.record,'$.status')='landed' BEGIN SELECT RAISE(FAIL,'final receipt'); \
         END;",
    )
    .unwrap();
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.run", json!({"preview_id":p["id"]}))
            .is_err()
    );
    assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["candidate_commit"]);
    // A failed final write must not publish an in-memory success to subsequent callers.
    let uncertain: Value = service.request("land.get", json!({"preview_id":p["id"]}));
    assert_eq!(uncertain["status"], "landing", "{uncertain}");
    assert_eq!(uncertain["landing_outcome"], "unknown", "{uncertain}");
    let retry: Value = service.request("land.run", json!({"preview_id":p["id"]}));
    assert_eq!(retry["status"], "landing", "{retry}");
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.reconcile", json!({"preview_id":p["id"]}))
            .is_err(),
        "reconcile must persist its sampled success before publishing it"
    );
    let uncertain: Value = service.request("land.get", json!({"preview_id":p["id"]}));
    assert_eq!(uncertain["landing_outcome"], "unknown", "{uncertain}");
    db.execute_batch("DROP TRIGGER reject_landed").unwrap();
    // Reconcile in the same process, then prove the recovered receipt survives restart.
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["land", "reconcile", p["id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let reconciled: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reconciled["status"], "landed");
    assert_eq!(reconciled["landing_outcome"], "applied");
    assert!(git_at(target.path(), &["status", "--porcelain"]).is_empty());
    service.restart(false);
    let again: Value = service.request("land.run", json!({"preview_id":p["id"]}));
    assert_eq!(again["status"], "landed");
}

#[tokio::test]
async fn checked_out_landing_preserves_new_local_edits_and_untracked_files() {
    for tracked in [false, true] {
        let service = Service::new();
        let (p, target) = owned_landing_fixture(&service, false).await;
        let path = target.path().join(if tracked { "source" } else { "local-file" });
        fs::write(&path, "local-edit\n").unwrap();
        let index_path =
            git_at(target.path(), &["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        let index = fs::read(&index_path).unwrap();
        let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
        assert_eq!(result["landing_outcome"], "needs_attention", "{result}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "local-edit\n");
        assert_eq!(fs::read(index_path).unwrap(), index);
        assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["target_head"]);
    }
}

#[tokio::test]
async fn checked_out_landing_does_not_execute_hooks_or_smudge_filters() {
    let service = Service::new();
    let (p, target) = owned_landing_fixture(&service, false).await;
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("UNEXPECTED");
    let attrs = temp.path().join("attributes");
    fs::write(&attrs, "source filter=sentinel\n").unwrap();
    git_at(service.temp.path(), &["config", "core.attributesFile", attrs.to_str().unwrap()]);
    git_at(service.temp.path(), &[
        "config",
        "filter.sentinel.smudge",
        &format!("touch '{}'; cat", marker.display()),
    ]);
    let hooks = temp.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    let hook = hooks.join("reference-transaction");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    git_at(service.temp.path(), &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
    assert_eq!(result["status"], "landed", "{result}");
    assert!(!marker.exists(), "landing executed external hook or filter");
    assert_eq!(fs::read_to_string(target.path().join("source")).unwrap(), "candidate\n");
}

#[tokio::test]
async fn checked_out_landing_rejects_replaced_git_directory_even_with_matching_commits() {
    let service = Service::new();
    let (p, target) = owned_landing_fixture(&service, false).await;
    let foreign = tempfile::tempdir().unwrap();
    git_at(foreign.path(), &["init", "-q"]);
    fs::write(
        foreign.path().join(".git/objects/info/alternates"),
        format!("{}/.git/objects\n", service.temp.path().display()),
    )
    .unwrap();
    git_at(foreign.path(), &[
        "update-ref",
        "refs/heads/landing-target",
        p["target_head"].as_str().unwrap(),
    ]);
    git_at(foreign.path(), &["symbolic-ref", "HEAD", "refs/heads/landing-target"]);
    git_at(foreign.path(), &["read-tree", p["target_head"].as_str().unwrap()]);
    git_at(foreign.path(), &["config", "core.worktree", target.path().to_str().unwrap()]);
    fs::write(target.path().join(".git"), format!("gitdir: {}/.git\n", foreign.path().display()))
        .unwrap();
    let result: Value = service.request("land.run", json!({"preview_id":p["id"]}));
    assert_eq!(result["landing_outcome"], "unknown", "{result}");
    assert!(result["error"].as_str().unwrap().contains("another repository"));
    assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), p["target_head"]);
    assert_eq!(git_at(service.temp.path(), &["rev-parse", "landing-target"]), p["target_head"]);
    assert_eq!(fs::read_to_string(target.path().join("source")).unwrap(), "base\n");
}

#[tokio::test]
async fn integrated_project_plan_reuses_trust_steps_and_relative_cwd() {
    let service = Service::new();
    let config = r#"[verify]
timeout="10s"
steps=[{name="source",argv=["/bin/sh","-c","test -f source"]},
       {name="nested",argv=["/bin/sh","-c","test -f project.toml"],cwd=".xaya"}]
"#;
    let (session, _, info) = project_fixture(&service, config, false).await;
    let root = service.temp.path();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/marker"), "tracked\n").unwrap();
    git_at(root, &["branch", "-m", "task"]);
    git_at(root, &["branch", "landing-target"]);
    let cp: proto::Checkpoint = service
        .request("checkpoint.create", json!({"session_id":session.id,"command_id":"nested-cp"}));
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request("checkpoint.get", json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    let check: proto::Check = service.request(
        "check.project",
        json!({"checkpoint_id":cp.id,"command_id":"nested-plan","config_hash":info["config_hash"]}),
    );
    assert_eq!(check_done(&service, &check.id).status, proto::CheckStatus::Succeeded);
    let p: Value = service.request("land.preview", json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"nested-preview"}));
    let p = preview_done(&service, p["id"].as_str().unwrap());
    let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    let done = integration_done(&service, &p["id"]);
    assert_eq!(done["integration_verified"], true, "{done}");
    let check: proto::Check =
        service.request("check.get", json!({"check_id":done["integration_check_id"]}));
    assert_eq!(check.steps.len(), 2);
    assert!(check.steps.iter().all(|step| step.status == Some(proto::CheckStatus::Succeeded)));
}

#[tokio::test]
async fn landing_storage_failures_never_dispatch_verification_or_advance_branch() {
    let service = Service::new();
    let p = integration_fixture(&service, "true", 10).await;
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_integration BEFORE INSERT ON checks BEGIN SELECT \
         RAISE(FAIL,'fixture'); END;",
    )
    .unwrap();
    let _: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    let failed = integration_done(&service, &p["id"]);
    assert_eq!(failed["status"], "failed");
    assert!(failed["integration_check_id"].is_null());
    db.execute_batch("DROP TRIGGER reject_integration").unwrap();
    let next: Value = service.request("land.preview", json!({"checkpoint_id":p["checkpoint_id"],
        "check_id":p["check_id"],"target_branch":"landing-target","command_id":"retry-new-preview"}));
    let next = preview_done(&service, next["id"].as_str().unwrap());
    let _: Value = service.request("land.verify", json!({"preview_id":next["id"]}));
    assert_eq!(integration_done(&service, &next["id"])["integration_verified"], true);
    let before = git_at(service.temp.path(), &["rev-parse", "landing-target"]);
    db.execute_batch(
        "CREATE TRIGGER reject_landing BEFORE INSERT ON land_previews BEGIN SELECT \
         RAISE(FAIL,'fixture'); END;",
    )
    .unwrap();
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.run", json!({"preview_id":next["id"]}))
            .is_err()
    );
    assert_eq!(git_at(service.temp.path(), &["rev-parse", "landing-target"]), before);
}

#[tokio::test]
async fn integrated_cli_verifies_then_lands_and_repeated_verify_keeps_receipt() {
    let service = Service::new();
    let p = integration_fixture(&service, "test -f source", 10).await;
    for action in ["verify", "run"] {
        let output = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .args(["land", action, p["id"].as_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["integration_verified"], true);
        assert_eq!(result["status"], if action == "run" { "landed" } else { "ready" });
    }
    let duplicate: Value = service.request("land.verify", json!({"preview_id":p["id"]}));
    assert_eq!(duplicate["status"], "landed");
}

#[tokio::test]
async fn landing_preview_is_private_durable_and_invalidated_when_target_moves() {
    let mut service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let root = service.temp.path();
    let head = git_at(root, &["rev-parse", "HEAD"]);
    let target = git_at(root, &["rev-parse", "landing-target"]);
    let index = fs::read(root.join(".git/index")).unwrap();
    let params = json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"preview-once"});
    let preview: Value = service.request("land.preview", params.clone());
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "ready", "{done}");
    assert_eq!(done["integration_verified"], false);
    assert_eq!(done["target_head"], target);
    let commit = done["candidate_commit"].as_str().unwrap();
    assert_eq!(git_at(root, &["show", &format!("{commit}:source")]), "candidate");
    assert_eq!(git_at(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_at(root, &["rev-parse", "landing-target"]), target);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(git_at(root, &["rev-parse", done["candidate_ref"].as_str().unwrap()]), commit);
    git_at(root, &["gc", "--prune=now"]);
    git_at(root, &["cat-file", "-e", &format!("{commit}^{{commit}}")]);
    let recheck: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(recheck["valid"], true, "{recheck}");
    service.restart(false);
    let duplicate: Value = service.request("land.preview", params);
    assert_eq!(duplicate["id"], done["id"]);
    let root = service.temp.path();
    let advanced = git_at(root, &[
        "commit-tree",
        cp.tree.as_deref().unwrap(),
        "-p",
        &target,
        "-m",
        "target advanced",
    ]);
    git_at(root, &["update-ref", "refs/heads/landing-target", &advanced, &target]);
    let recheck: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(recheck["valid"], false);
    assert_eq!(recheck["preview"]["stale"], true);
    git_at(root, &["update-ref", "refs/heads/landing-target", &target, &advanced]);
    let still_stale: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(still_stale["valid"], false);
}

#[tokio::test]
async fn landing_preview_blocks_dirty_and_active_target_worktrees_and_reports_conflicts() {
    let service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let target = tempfile::tempdir().unwrap();
    let source = service.temp.path();
    git_at(source, &["worktree", "add", "-q", target.path().to_str().unwrap(), "landing-target"]);
    let index_path =
        git_at(target.path(), &["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let target_index = fs::read(&index_path).unwrap();
    let target_head = git_at(target.path(), &["rev-parse", "HEAD"]);
    fs::write(target.path().join("source"), "dirty\n").unwrap();
    let request = |command: &str| -> Value {
        service.request("land.preview",json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":command}))
    };
    let preview = request("dirty");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    assert!(done["error"].as_str().unwrap().contains("dirty"));
    assert_eq!(fs::read(&index_path).unwrap(), target_index);
    assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), target_head);
    fs::write(target.path().join("source"), "base\n").unwrap();
    git_at(target.path(), &["update-index", "--assume-unchanged", "source"]);
    fs::write(target.path().join("source"), "hidden dirty\n").unwrap();
    let preview = request("hidden-dirty");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    assert!(done["error"].as_str().unwrap().contains("assume-unchanged"));
    git_at(target.path(), &["update-index", "--no-assume-unchanged", "source"]);
    fs::write(target.path().join("source"), "base\n").unwrap();
    git_at(source, &["worktree", "lock", target.path().to_str().unwrap()]);
    let preview = request("locked");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    git_at(source, &["worktree", "unlock", target.path().to_str().unwrap()]);
    let metadata = git_at(target.path(), &["rev-parse", "--absolute-git-dir"]);
    let merge_head = std::path::Path::new(&metadata).join("MERGE_HEAD");
    fs::write(&merge_head, format!("{target_head}\n")).unwrap();
    let preview = request("unfinished-merge");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    fs::remove_file(merge_head).unwrap();

    let target_session: Session = service.request(proto::SESSION_START,json!({
        "cwd":target.path(),"executable":source.join("fake-codex"),"codex_home":source.join("codex-home"),"sandbox":"read-only"}));
    let preview = request("writer");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":target_session.id}));
    let preview = request("clean");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "ready", "{done}");
    fs::write(target.path().join("untracked"), "do not overwrite").unwrap();
    let invalid: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(invalid["valid"], false);
    fs::remove_file(target.path().join("untracked")).unwrap();
    fs::write(target.path().join("source"), "target change\n").unwrap();
    git_at(target.path(), &["add", "source"]);
    git_at(target.path(), &["commit", "-qm", "target change"]);
    let before = git_at(target.path(), &["rev-parse", "HEAD"]);
    fs::create_dir_all(source.join("generated")).unwrap();
    let attrs = source.join("generated/attributes");
    fs::write(&attrs, "source merge=fixture filter=fixture\n").unwrap();
    git_at(source, &["config", "core.attributesFile", attrs.to_str().unwrap()]);
    git_at(source, &["config", "merge.fixture.driver", "touch DRIVER_RAN; cp %B %A"]);
    git_at(source, &["config", "filter.fixture.clean", "touch FILTER_RAN; cat"]);
    let preview = request("conflict");
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "conflict", "{done}");
    for root in [source, target.path()] {
        assert!(!root.join("DRIVER_RAN").exists());
        assert!(!root.join("FILTER_RAN").exists());
    }

    assert!(done["candidate_commit"].is_null());
    assert_eq!(git_at(target.path(), &["rev-parse", "HEAD"]), before);
    assert_eq!(
        git_at(target.path(), &[
            "-c",
            "filter.fixture.clean=",
            "-c",
            "filter.fixture.process=",
            "-c",
            "filter.fixture.required=false",
            "status",
            "--porcelain"
        ]),
        ""
    );
}

#[tokio::test]
async fn landing_recheck_rejects_changed_candidates_source_versions_and_wrong_receipts() {
    let service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let request = |command: &str, check_id: &str| -> Value {
        service.request("land.preview",json!({"checkpoint_id":cp.id,"check_id":check_id,"target_branch":"landing-target","command_id":command}))
    };
    let other: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":cp.session_id,"command_id":"another-checkpoint"}),
    );
    let other = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":other.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    let other_check: proto::Check = service.request(proto::CHECK_RUN,json!({
        "checkpoint_id":other.id,"command_id":"another-receipt","argv":["/usr/bin/true"],"timeout_secs":10}));
    let other_check = check_done(&service, &other_check.id);
    assert_eq!(other_check.status, proto::CheckStatus::Succeeded);
    let preview = request("wrong-checkpoint", &other_check.id);
    assert_eq!(preview_done(&service, preview["id"].as_str().unwrap())["status"], "blocked");
    let failed: proto::Check = service.request(proto::CHECK_RUN,json!({
        "checkpoint_id":cp.id,"command_id":"failed-check","argv":["/usr/bin/false"],"timeout_secs":10}));
    let failed = check_done(&service, &failed.id);
    let preview = request("failed-verification", &failed.id);
    assert_eq!(preview_done(&service, preview["id"].as_str().unwrap())["status"], "blocked");
    let preview = request("tamper", &check.id);
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "ready", "{done}");
    git_at(service.temp.path(), &["update-ref", "-d", done["candidate_ref"].as_str().unwrap()]);
    let invalid: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(invalid["valid"], false);
    let preview = request("ownership", &check.id);
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "ready", "{done}");
    let target = tempfile::tempdir().unwrap();
    git_at(service.temp.path(), &[
        "worktree",
        "add",
        "-q",
        target.path().to_str().unwrap(),
        "landing-target",
    ]);
    let invalid: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(invalid["valid"], false);
    assert!(invalid["reason"].as_str().unwrap().contains("ownership"));
    let preview = request("source-change", &check.id);
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "ready", "{done}");
    fs::write(service.temp.path().join("source"), "changed after verification\n").unwrap();
    let invalid: Value = service.request("land.recheck", json!({"preview_id":done["id"]}));
    assert_eq!(invalid["valid"], false);
    fs::write(service.temp.path().join("source"), "candidate\n").unwrap();
    let preview = request("old-check", &check.id);
    assert_eq!(preview_done(&service, preview["id"].as_str().unwrap())["status"], "blocked");
}

#[tokio::test]
async fn landing_preview_rejects_unrelated_and_empty_candidates_without_updating_branches() {
    let service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let root = service.temp.path();
    let original = git_at(root, &["rev-parse", "landing-target"]);
    let orphan = git_at(root, &["commit-tree", cp.tree.as_deref().unwrap(), "-m", "unrelated"]);
    git_at(root, &["update-ref", "refs/heads/landing-target", &orphan, &original]);
    let params = json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"unrelated"});
    let preview: Value = service.request("land.preview", params);
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    assert_eq!(git_at(root, &["rev-parse", "landing-target"]), orphan);
    git_at(root, &[
        "update-ref",
        "refs/heads/landing-target",
        cp.commit.as_deref().unwrap(),
        &orphan,
    ]);
    let preview: Value = service.request("land.preview",json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"empty"}));
    let done = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(done["status"], "blocked", "{done}");
    assert!(done["error"].as_str().unwrap().contains("no changes"));
    assert!(done["candidate_ref"].is_null());
}

#[tokio::test]
async fn landing_preview_storage_failure_never_dispatches_and_cli_queries_exact_receipts() {
    let service = Service::new();
    let (_, cp, check) = landing_fixture(&service).await;
    let root = service.temp.path();
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_previews BEFORE INSERT ON land_previews BEGIN SELECT \
         RAISE(FAIL,'fixture'); END;",
    )
    .unwrap();
    let params = json!({"checkpoint_id":cp.id,"check_id":check.id,"target_branch":"landing-target","command_id":"cli"});
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.preview", params.clone())
            .is_err()
    );
    assert_eq!(git_at(root, &["for-each-ref", "refs/xaya/landing-previews"]), "");
    db.execute_batch("DROP TRIGGER refuse_previews").unwrap();
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args([
            "land",
            "preview",
            &cp.id,
            "--check-id",
            &check.id,
            "--target",
            "landing-target",
            "--command-id",
            "cli",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    let duplicate: Value = service.request("land.preview", params.clone());
    assert_eq!(duplicate["id"], preview["id"]);
    let mut conflict = params;
    conflict["target_branch"] = json!("other");
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Value>("land.preview", conflict)
            .is_err()
    );
    let id = preview["id"].as_str().unwrap();
    for action in ["show", "recheck"] {
        let output = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(service.runtime.directory())
            .args(["land", action, id])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}

async fn project_fixture(
    service: &Service,
    config: &str,
    ignored: bool,
) -> (Session, proto::Checkpoint, Value) {
    let (session, _) = checkpoint_fixture(service).await;
    let root = service.temp.path();
    fs::create_dir(root.join(".xaya")).unwrap();
    fs::create_dir(root.join("generated")).unwrap();
    if ignored {
        fs::write(root.join(".git/info/exclude"), ".xaya/\n").unwrap();
    }
    fs::write(root.join(".xaya/project.toml"), config).unwrap();
    let info: Value = service.request(proto::PROJECT_GET, json!({"cwd":root}));
    let _: Value = service
        .request(proto::PROJECT_TRUST, json!({"cwd":root,"config_hash":info["config_hash"]}));
    let cp: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"project-base"}),
    );
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    (session, cp, info)
}

#[tokio::test]
async fn project_steps_stop_after_failure_and_cli_uses_the_exact_trusted_plan() {
    let service = Service::new();
    let config = r#"[verify]
timeout = "10s"
steps = [
  {name="prepare", argv=["python3", "-c", "print('prepared')"]},
  {name="fail", argv=["python3", "-c", "raise SystemExit(9)"]},
  {name="never", argv=["/usr/bin/touch", "generated/UNEXPECTED"]},
]
"#;
    let (session, cp, info) = project_fixture(&service, config, false).await;
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["project", "show", "--cwd"])
        .arg(service.temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["config_hash"], info["config_hash"]);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["project", "trust", "--cwd"])
        .arg(service.temp.path())
        .args(["--hash", info["config_hash"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "project", &cp.id, "--command-id", "ordered-failure"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let done: proto::Check = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(done.status, proto::CheckStatus::Failed);
    assert_eq!(done.steps[0].status, Some(proto::CheckStatus::Succeeded));
    assert_eq!(done.steps[1].status, Some(proto::CheckStatus::Failed));
    assert_eq!(done.steps[1].exit_code, Some(9));
    assert_eq!(done.steps[2].status, None);
    assert!(!service.temp.path().join("generated/UNEXPECTED").exists());
    let latest: Option<proto::Check> =
        service.request(proto::CHECK_LATEST, json!({"session_id":session.id}));
    assert_eq!(latest.unwrap().id, done.id);
}

#[tokio::test]
async fn project_total_budget_stops_later_steps_and_keeps_step_receipts() {
    let service = Service::new();
    let config = r#"[verify]
timeout = "2s"
steps = [
  {name="first", argv=["python3", "-c", "import time; time.sleep(1.1)"]},
  {name="second", argv=["python3", "-c", "import time; time.sleep(1.1)"]},
  {name="never", argv=["/usr/bin/touch", "generated/UNEXPECTED"]},
]
"#;
    let (_, cp, info) = project_fixture(&service, config, false).await;
    let check: proto::Check = service.request(
        proto::CHECK_PROJECT,
        json!({"checkpoint_id":cp.id,"command_id":"budget","config_hash":info["config_hash"]}),
    );
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::TimedOut);
    assert_eq!(done.steps[0].status, Some(proto::CheckStatus::Succeeded));
    assert_eq!(done.steps[1].status, Some(proto::CheckStatus::TimedOut));
    assert_eq!(done.steps[2].status, None);
    assert!(!service.temp.path().join("generated/UNEXPECTED").exists());
}

#[tokio::test]
async fn project_configuration_change_invalidates_even_an_ignored_config() {
    let service = Service::new();
    let config = r#"[verify]
timeout = "10s"
steps = [
  {name="mutate", argv=["python3", "-c", "from pathlib import Path; p=Path('.xaya/project.toml'); p.write_text(p.read_text()+'#changed')"]},
  {name="never", argv=["/usr/bin/touch", "generated/UNEXPECTED"]},
]
"#;
    let (_, cp, info) = project_fixture(&service, config, true).await;
    let check: proto::Check = service.request(
        proto::CHECK_PROJECT,
        json!({"checkpoint_id":cp.id,"command_id":"changed","config_hash":info["config_hash"]}),
    );
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Failed);
    assert!(done.stale);
    assert_eq!(done.steps[1].status, None);
    assert!(!service.temp.path().join("generated/UNEXPECTED").exists());
}

#[tokio::test]
async fn project_cancel_skips_pending_steps_and_restart_interrupts_a_running_step() {
    let mut service = Service::new();
    let config = r#"[verify]
timeout = "60s"
steps = [
  {name="wait", argv=["python3", "-c", "import os,time; from pathlib import Path; Path('generated/pid').write_text(str(os.getpid())); print('started',flush=True); time.sleep(60)"]},
  {name="never", argv=["/usr/bin/touch", "generated/UNEXPECTED"]},
]
"#;
    let (_, cp, info) = project_fixture(&service, config, false).await;
    let start =
        |command: &str| -> proto::Check {
            service.request(proto::CHECK_PROJECT,
        json!({"checkpoint_id":cp.id,"command_id":command,"config_hash":info["config_hash"]}))
        };
    let check = start("cancel");
    service.wait(|| {
        let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
        log.text.contains("started").then_some(())
    });
    let _: proto::Check = service.request(proto::CHECK_CANCEL, json!({"check_id":check.id}));
    let done = check_done(&service, &check.id);
    assert_eq!(done.steps[0].status, Some(proto::CheckStatus::Cancelled));
    assert_eq!(done.steps[1].status, None);
    let check = start("restart");
    service.wait(|| {
        let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
        log.text.contains("started").then_some(())
    });
    let pid: i32 =
        fs::read_to_string(service.temp.path().join("generated/pid")).unwrap().parse().unwrap();
    service.restart(true);
    let done: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":check.id}));
    assert_eq!(done.status, proto::CheckStatus::Interrupted);
    assert_eq!(done.steps[0].status, Some(proto::CheckStatus::Interrupted));
    assert_eq!(done.steps[1].status, None);
    assert!(!service.temp.path().join("generated/UNEXPECTED").exists());
    // SAFETY: the test-owned still-live verification process wrote its dedicated group PID above.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

async fn checkpoint_fixture(service: &Service) -> (Session, proto::Checkpoint) {
    let (session, ws) = ready(service).await;
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    drop(ws);
    let root = service.temp.path();
    for args in [vec!["init", "-q"], vec!["config", "user.name", "Fixture"], vec![
        "config",
        "user.email",
        "fixture@example.invalid",
    ]] {
        assert!(Command::new("git").current_dir(root).args(args).status().unwrap().success());
    }
    fs::write(root.join(".gitignore"), "run/\ncodex-home/\ngenerated\n").unwrap();
    fs::write(root.join("source"), "base\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-qm", "base"]] {
        assert!(Command::new("git").current_dir(root).args(args).status().unwrap().success());
    }
    let cp: proto::Checkpoint = service
        .request(proto::CHECKPOINT_CREATE, json!({"session_id":session.id,"command_id":"base"}));
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    (session, cp)
}

/// A succeeded single-command check over a checkpoint holding two tracked source files.
async fn binding_fixture(
    service: &Service,
    argv: Value,
) -> (Session, proto::Checkpoint, proto::Check) {
    let (session, _) = checkpoint_fixture(service).await;
    let root = service.temp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    // An already committed entry point, then the task's own uncommitted change.
    fs::write(root.join("src/lib.rs"), "pub mod value;\n").unwrap();
    git_at(root, &["add", "src/lib.rs"]);
    git_at(root, &["-c", "core.hooksPath=/dev/null", "commit", "-qm", "entry point"]);
    fs::write(root.join("src/value.rs"), "pub const VALUE: u8 = 1;\n").unwrap();
    fs::write(root.join("generated"), "ignored, source checkout only\n").unwrap();
    let cp: proto::Checkpoint = service.request(
        proto::CHECKPOINT_CREATE,
        json!({"session_id":session.id,"command_id":"binding-base"}),
    );
    let cp = service.wait(|| {
        let cp: proto::Checkpoint =
            service.request(proto::CHECKPOINT_GET, json!({"checkpoint_id":cp.id}));
        (cp.status != proto::CheckpointStatus::Creating).then_some(cp)
    });
    assert_eq!(cp.status, proto::CheckpointStatus::Ready);
    let check: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"binding-check","argv":argv,"timeout_secs":10}),
    );
    let check = check_done(service, &check.id);
    assert_eq!(check.status, proto::CheckStatus::Succeeded, "{check:?}");
    (session, cp, check)
}

fn binding_done(service: &Service, id: &str) -> proto::Check {
    service.wait(|| {
        let c: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":id}));
        (c.binding.status != proto::BindingStatus::Running).then_some(c)
    })
}

// Fails exactly when the relative source file in its own working tree is corrupted.
const BOUND_GATE: &str = "! grep -q '(((' src/value.rs && ! grep -q '(((' src/lib.rs";

#[tokio::test]
async fn binding_proves_a_gate_reads_its_own_tree_in_a_private_checkout() {
    let service = Service::new();
    let (session, cp, check) =
        binding_fixture(&service, json!(["/bin/sh", "-c", BOUND_GATE])).await;
    assert_eq!(check.binding.status, proto::BindingStatus::NotChecked);
    let root = service.temp.path();
    let head = git_at(root, &["rev-parse", "HEAD"]);
    let index = fs::read(root.join(".git/index")).unwrap();
    let started: proto::Check =
        service.request("check.bind", json!({"check_id":check.id,"command_id":"bind-1"}));
    assert_eq!(started.binding.command_id.as_deref(), Some("bind-1"));
    let bound = binding_done(&service, &check.id);
    assert_eq!(bound.binding.status, proto::BindingStatus::Proved, "{:?}", bound.binding);
    // The task's own change is corrupted first: the gate is most likely to read it.
    assert_eq!(bound.binding.mutated_paths, vec!["src/value.rs".to_string()]);
    assert_eq!(bound.binding.runs.len(), 2);
    let baseline: proto::Check =
        service.request(proto::CHECK_GET, json!({"check_id":bound.binding.runs[0]}));
    let mutated: proto::Check =
        service.request(proto::CHECK_GET, json!({"check_id":bound.binding.runs[1]}));
    let canary = baseline.canary.as_ref().unwrap();
    assert_eq!(canary.parent_check_id, check.id);
    assert_eq!(canary.role, proto::CanaryRole::Baseline);
    assert_eq!(baseline.status, proto::CheckStatus::Succeeded);
    assert_eq!(mutated.canary.as_ref().unwrap().role, proto::CanaryRole::Mutated);
    assert_eq!(mutated.canary.as_ref().unwrap().mutated_path.as_deref(), Some("src/value.rs"));
    assert_eq!(mutated.status, proto::CheckStatus::Failed);
    for run in [&baseline, &mutated] {
        assert_eq!(
            run.tree == check.tree,
            run.id == baseline.id,
            "only the baseline is the same tree"
        );
        assert!(!run.integration_cleanup_pending, "{run:?}");
        assert!(!std::path::Path::new(&run.root).exists(), "private checkout must be removed");
        assert!(!run.root.starts_with(root.to_str().unwrap()));
    }
    // The user's checkout is never corrupted, staged or moved.
    assert_eq!(
        fs::read_to_string(root.join("src/value.rs")).unwrap(),
        "pub const VALUE: u8 = 1;\n"
    );
    assert_eq!(git_at(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "bind", &check.id, "--command-id", "bind-1"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let shown: proto::Check = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown.binding.status, proto::BindingStatus::Proved);
    // Same command ID is a query; a settled result is not re-run under a new one.
    let duplicate: proto::Check =
        service.request("check.bind", json!({"check_id":check.id,"command_id":"bind-1"}));
    assert_eq!(duplicate.binding, bound.binding);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::Check>(
                "check.bind",
                json!({"check_id":check.id,"command_id":"bind-2"})
            )
            .is_err()
    );
    // Canary runs are evidence, never verification of record.
    let latest: Option<proto::Check> =
        service.request(proto::CHECK_LATEST, json!({"session_id":session.id}));
    assert_eq!(latest.unwrap().id, check.id);
    git_at(root, &["branch", "binding-target"]);
    let preview: Value = service.request(
        "land.preview",
        json!({"checkpoint_id":cp.id,
        "check_id":baseline.id,"target_branch":"binding-target","command_id":"canary-land"}),
    );
    let preview = preview_done(&service, preview["id"].as_str().unwrap());
    assert_eq!(preview["status"], "blocked", "a canary receipt cannot gate a landing: {preview}");
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::Check>("check.bind", json!({"check_id":baseline.id,"command_id":"x"}))
            .is_err(),
        "a canary run cannot itself be bound"
    );
}

#[tokio::test]
async fn binding_reports_unbound_for_gates_reading_another_tree_or_nothing() {
    for (label, gate) in [
        (
            "absolute",
            format!("! grep -q '(((' {0}/src/value.rs && ! grep -q '(((' {0}/src/lib.rs", "ROOT"),
        ),
        ("vacuous", "true".to_string()),
    ] {
        let service = Service::new();
        let gate = gate.replace("ROOT", service.temp.path().to_str().unwrap());
        let (_, _, check) = binding_fixture(&service, json!(["/bin/sh", "-c", gate])).await;
        let _: proto::Check =
            service.request("check.bind", json!({"check_id":check.id,"command_id":"unbound"}));
        let done = binding_done(&service, &check.id);
        assert_eq!(
            done.binding.status,
            proto::BindingStatus::Unbound,
            "{label}: {:?}",
            done.binding
        );
        // Two structurally different candidates are tried before calling a gate unbound.
        assert_eq!(done.binding.mutated_paths, vec![
            "src/value.rs".to_string(),
            "src/lib.rs".into()
        ]);
        assert_eq!(done.binding.runs.len(), 3);
        assert!(done.binding.reason.as_deref().unwrap_or("").contains("still passed"));
        // UNBOUND is information about the evidence; the original receipt is unchanged.
        assert_eq!(done.status, proto::CheckStatus::Succeeded);
        assert!(!done.stale);
    }
}

const REQUIRE_BINDING: &str =
    "[verify]\nrequire_binding = true\nsteps = [{name='gate', argv=['/usr/bin/true']}]\n";

/// Writes an excluded project configuration into the checkout that owns the checkpoint.
fn write_policy(root: &std::path::Path, config: &str) {
    fs::create_dir_all(root.join(".xaya")).unwrap();
    let exclude = root.join(".git/info/exclude");
    if !fs::read_to_string(&exclude).unwrap_or_default().contains(".xaya/") {
        fs::write(&exclude, ".xaya/\n").unwrap();
    }
    fs::write(root.join(".xaya/project.toml"), config).unwrap();
}

fn land_preview(service: &Service, cp: &proto::Checkpoint, check: &str, command: &str) -> Value {
    let preview: Value = service.request(
        "land.preview",
        json!({"checkpoint_id":cp.id,
        "check_id":check,"target_branch":"policy-target","command_id":command}),
    );
    preview_done(service, preview["id"].as_str().unwrap())
}

#[tokio::test]
async fn required_binding_blocks_landing_until_the_source_check_is_proved() {
    let service = Service::new();
    let (_, cp, check) = binding_fixture(&service, json!(["/bin/sh", "-c", BOUND_GATE])).await;
    let root = service.temp.path();
    git_at(root, &["branch", "policy-target"]);
    let target = git_at(root, &["rev-parse", "policy-target"]);
    write_policy(root, REQUIRE_BINDING);
    // Stricter policy is honoured without trust: it can only refuse more.
    let info: Value = service.request(proto::PROJECT_GET, json!({"cwd":root}));
    assert_eq!(info["require_binding"], true, "{info}");
    assert_eq!(info["trusted"], false);

    let blocked = land_preview(&service, &cp, &check.id, "unproved");
    assert_eq!(blocked["status"], "blocked", "{blocked}");
    assert_eq!(blocked["binding_required"], true, "{blocked}");
    assert!(blocked["error"].as_str().unwrap().contains("binding"), "{blocked}");

    let _: proto::Check =
        service.request("check.bind", json!({"check_id":check.id,"command_id":"policy-bind"}));
    assert_eq!(binding_done(&service, &check.id).binding.status, proto::BindingStatus::Proved);
    let ready = land_preview(&service, &cp, &check.id, "proved");
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["binding_required"], true);

    // The requirement recorded by a preview is sticky: removing it does not relax the receipt,
    // and an unreadable configuration fails closed.
    write_policy(root, "[verify]\nsteps = [{name='gate', argv=['/usr/bin/true']}]\n");
    let recheck: Value = service.request("land.recheck", json!({"preview_id":ready["id"]}));
    assert_eq!(recheck["valid"], true, "{recheck}");
    assert_eq!(recheck["preview"]["binding_required"], true);
    write_policy(root, "[verify]\nrequire_binding = 'yes'\nsteps = []\n");
    let invalid = land_preview(&service, &cp, &check.id, "invalid-policy");
    assert_eq!(invalid["status"], "blocked", "{invalid}");
    let recheck: Value = service.request("land.recheck", json!({"preview_id":ready["id"]}));
    assert_eq!(recheck["valid"], false, "{recheck}");
    assert_eq!(git_at(root, &["rev-parse", "policy-target"]), target);
}

#[tokio::test]
async fn required_binding_rejects_unbound_checks_and_absent_policy_keeps_landing_open() {
    let service = Service::new();
    let (_, cp, check) = binding_fixture(&service, json!(["/usr/bin/true"])).await;
    let root = service.temp.path();
    git_at(root, &["branch", "policy-target"]);
    // Without a project configuration nothing is required.
    let open = land_preview(&service, &cp, &check.id, "no-policy");
    assert_eq!(open["status"], "ready", "{open}");
    assert_eq!(open["binding_required"], false);

    let _: proto::Check =
        service.request("check.bind", json!({"check_id":check.id,"command_id":"vacuous"}));
    assert_eq!(binding_done(&service, &check.id).binding.status, proto::BindingStatus::Unbound);
    write_policy(root, REQUIRE_BINDING);
    let blocked = land_preview(&service, &cp, &check.id, "unbound");
    assert_eq!(blocked["status"], "blocked", "{blocked}");
    assert!(blocked["error"].as_str().unwrap().contains("binding"), "{blocked}");
    // A preview created before the policy appeared is held to it at recheck and verify.
    let recheck: Value = service.request("land.recheck", json!({"preview_id":open["id"]}));
    assert_eq!(recheck["valid"], false, "{recheck}");
}

#[tokio::test]
async fn binding_cannot_check_when_the_clean_private_checkout_fails() {
    let service = Service::new();
    // Depends on an ignored file that only exists in the user's checkout.
    let (_, _, check) =
        binding_fixture(&service, json!(["/bin/sh", "-c", "test -f generated"])).await;
    let _: proto::Check =
        service.request("check.bind", json!({"check_id":check.id,"command_id":"baseline"}));
    let done = binding_done(&service, &check.id);
    assert_eq!(done.binding.status, proto::BindingStatus::CannotCheck, "{:?}", done.binding);
    assert_eq!(done.binding.runs.len(), 1, "no corruption without a passing baseline");
    assert!(done.binding.mutated_paths.is_empty());
    assert!(done.binding.reason.as_deref().unwrap_or("").contains("clean private checkout"));
    // CANNOT-CHECK is not settled: a new command ID may try again. The CLI waits and reports
    // anything other than a proof as a failure.
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "bind", &check.id, "--command-id", "baseline-2"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("CannotCheck"));
    let retried: proto::Check = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(retried.binding.command_id.as_deref(), Some("baseline-2"));
    assert_eq!(retried.binding.status, proto::BindingStatus::CannotCheck);
}

#[tokio::test]
async fn binding_rejects_unsafe_paths_unfinished_checks_and_storage_failure() {
    let mut service = Service::new();
    let (_, cp, check) = binding_fixture(&service, json!(["/bin/sh", "-c", BOUND_GATE])).await;
    let root = service.temp.path();
    let bind = |params: Value| {
        Client::connect(&service.runtime).unwrap().request::<proto::Check>("check.bind", params)
    };
    for path in
        ["../escape.rs", "/etc/passwd", "src/missing.rs", ".xaya/project.toml", "generated", ""]
    {
        assert!(
            bind(json!({"check_id":check.id,"command_id":"path","path":path})).is_err(),
            "{path} must be refused"
        );
    }
    assert!(bind(json!({"check_id":"check-unknown","command_id":"none"})).is_err());
    assert!(bind(json!({"check_id":check.id,"command_id":""})).is_err());
    // An explicit tracked path is honoured.
    let _: proto::Check = service.request(
        "check.bind",
        json!({"check_id":check.id,"command_id":"explicit","path":"src/lib.rs"}),
    );
    let done = binding_done(&service, &check.id);
    assert_eq!(done.binding.status, proto::BindingStatus::Proved);
    assert_eq!(done.binding.mutated_paths, vec!["src/lib.rs".to_string()]);
    // Only a successful, current receipt can be bound.
    let failed: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"failing","argv":["/usr/bin/false"],"timeout_secs":10}),
    );
    let failed = check_done(&service, &failed.id);
    assert!(bind(json!({"check_id":failed.id,"command_id":"failed"})).is_err());
    fs::write(root.join("src/value.rs"), "pub const VALUE: u8 = 3;\n").unwrap();
    let stale: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"stale","argv":["/usr/bin/true"],"timeout_secs":10}),
    );
    let stale_done = check_done(&service, &stale.id);
    assert!(stale_done.stale || stale_done.status != proto::CheckStatus::Succeeded);
    assert!(bind(json!({"check_id":stale.id,"command_id":"stale"})).is_err());
    fs::write(root.join("src/value.rs"), "pub const VALUE: u8 = 1;\n").unwrap();

    // A durable running receipt precedes any canary dispatch.
    // A slow gate keeps the canary baseline running long enough to restart during it.
    let slow: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({"checkpoint_id":cp.id,
        "command_id":"slow","argv":["/bin/sh","-c",format!("sleep 2; {BOUND_GATE}")],
        "timeout_secs":20}),
    );
    let second = check_done(&service, &slow.id);
    assert_eq!(second.status, proto::CheckStatus::Succeeded);
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_checks BEFORE INSERT ON checks BEGIN SELECT RAISE(FAIL,'fixture'); \
         END;",
    )
    .unwrap();
    assert!(bind(json!({"check_id":second.id,"command_id":"no-store"})).is_err());
    let unchanged: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":second.id}));
    assert_eq!(unchanged.binding.status, proto::BindingStatus::NotChecked);
    assert!(unchanged.binding.runs.is_empty());
    db.execute_batch("DROP TRIGGER refuse_checks").unwrap();

    // A binding in flight at restart is reported, never replayed.
    let _: proto::Check =
        service.request("check.bind", json!({"check_id":second.id,"command_id":"restart"}));
    service.wait(|| {
        let c: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":second.id}));
        (!c.binding.runs.is_empty()).then_some(())
    });
    service.restart(true);
    let restored: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":second.id}));
    assert_eq!(
        restored.binding.status,
        proto::BindingStatus::CannotCheck,
        "{:?}",
        restored.binding
    );
    assert!(restored.binding.reason.as_deref().unwrap_or("").contains("not replayed"));
    let duplicate: proto::Check =
        service.request("check.bind", json!({"check_id":second.id,"command_id":"restart"}));
    assert_eq!(duplicate.binding.status, proto::BindingStatus::CannotCheck);
}

fn check_done(service: &Service, id: &str) -> proto::Check {
    service.wait(|| {
        let c: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":id}));
        (c.status != proto::CheckStatus::Running).then_some(c)
    })
}

#[tokio::test]
async fn verification_reports_exit_failure_source_mutation_and_bounded_durable_logs() {
    let mut service = Service::new();
    let (_, cp) = checkpoint_fixture(&service).await;
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"failure","argv":["python3","-c",
            "import sys; print('out'); print('err',file=sys.stderr); sys.exit(7)"],"timeout_secs":10}));
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Failed);
    assert_eq!(done.exit_code, Some(7));
    let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
    assert!(log.text.contains("out") && log.text.contains("err"));
    let check: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"large-log","argv":["python3","-c",
            "import sys; sys.stdout.write('\\x01' * 300000)"],"timeout_secs":10}),
    );
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Succeeded);
    assert!(done.log_truncated);
    service.restart(false);
    let mut offset = 0;
    loop {
        let log: proto::CheckLog =
            service.request(proto::CHECK_LOG, json!({"check_id":check.id,"offset":offset}));
        assert!(log.truncated);
        assert!(log.text.len() <= 8192);
        assert!(log.next_offset > offset || log.complete);
        offset = log.next_offset;
        if log.complete {
            break;
        }
    }
    assert_eq!(offset, 128 * 1024);
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "run", &cp.id, "--command-id", "cli-check", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let cli_check: proto::Check = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli_check.status, proto::CheckStatus::Succeeded);
    let check: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"unicode","argv":["python3","-c",
            "print('中🙂' * 3000,end='')"],"timeout_secs":10}),
    );
    assert_eq!(check_done(&service, &check.id).status, proto::CheckStatus::Succeeded);
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let log: proto::CheckLog =
            service.request(proto::CHECK_LOG, json!({"check_id":check.id,"offset":offset}));
        text.push_str(&log.text);
        offset = log.next_offset;
        if log.complete {
            break;
        }
    }
    assert_eq!(text, "中🙂".repeat(3000));
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::CheckLog>(proto::CHECK_LOG, json!({"check_id":check.id,"offset":1}))
            .is_err()
    );
    let check: proto::Check = service.request(
        proto::CHECK_RUN,
        json!({
        "checkpoint_id":cp.id,"command_id":"mutation","argv":["python3","-c",
            "from pathlib import Path; Path('source').write_text('mutation')"],"timeout_secs":10}),
    );
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Failed);
    assert!(done.stale);
    assert_eq!(done.exit_code, Some(0));
}

#[tokio::test]
async fn verification_timeout_cancel_and_leader_exit_reap_the_dedicated_group() {
    let service = Service::new();
    let (session, cp) = checkpoint_fixture(&service).await;
    let code = "import subprocess,time; subprocess.Popen(['python3','-c',\"import time; from \
                pathlib import Path; time.sleep(3); Path('generated').write_text('leaked')\"]); \
                print('started',flush=True); time.sleep(60)";
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"timeout","argv":["python3","-c",code],"timeout_secs":1}));
    service.wait(|| {
        let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
        log.text.contains("started").then_some(())
    });
    assert!(service.get(&session.id).verification.is_some());
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Session>(proto::SESSION_RESUME, json!({"session_id":session.id}))
            .is_err()
    );
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::TimedOut);
    assert!(service.get(&session.id).verification.is_none());
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"cancel","argv":["python3","-c",code],"timeout_secs":30}));
    service.wait(|| {
        let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
        log.text.contains("started").then_some(())
    });
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["check", "cancel", &check.id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let done = check_done(&service, &check.id);
    assert_eq!(done.status, proto::CheckStatus::Cancelled);
    // A successful leader with a pipe-holding descendant must also finish promptly.
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"leader-exit","argv":["python3","-c",
            "import subprocess; subprocess.Popen(['sleep','60']); print('done')"],"timeout_secs":10}));
    assert_eq!(check_done(&service, &check.id).status, proto::CheckStatus::Succeeded);
    let check: proto::Check = service.request(proto::CHECK_RUN, json!({
        "checkpoint_id":cp.id,"command_id":"escaped-pipe","argv":["python3","-c",
            "import subprocess; child=subprocess.Popen(['sleep','60'], start_new_session=True); print(child.pid,flush=True)"],"timeout_secs":10}));
    assert_eq!(check_done(&service, &check.id).status, proto::CheckStatus::Succeeded);
    let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
    let escaped_pid: i32 = log.text.trim().parse().unwrap();
    // SAFETY: the test-owned escaped child reported its still-live process group above.
    unsafe {
        libc::kill(-escaped_pid, libc::SIGKILL);
    }
    let (sender, receiver) = std::sync::mpsc::channel::<()>();
    assert!(receiver.recv_timeout(Duration::from_secs(3)).is_err());
    drop(sender);
    assert!(
        !service.temp.path().join("generated").exists(),
        "cancelled descendant wrote after cleanup"
    );
}

#[tokio::test]
async fn verification_storage_failure_does_not_dispatch_and_restart_never_replays() {
    let mut service = Service::new();
    let (session, cp) = checkpoint_fixture(&service).await;
    let db =
        rusqlite::Connection::open(service.runtime.directory().join("state/agentd.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_checks BEFORE INSERT ON checks BEGIN SELECT RAISE(FAIL,'fixture'); \
         END;",
    )
    .unwrap();
    let params = json!({"checkpoint_id":cp.id,"command_id":"no-dispatch",
        "argv":["/usr/bin/touch","generated"],"timeout_secs":10});
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::Check>(proto::CHECK_RUN, params)
            .is_err()
    );
    assert!(!service.temp.path().join("generated").exists());
    db.execute_batch("DROP TRIGGER refuse_checks").unwrap();
    for (argv, timeout) in [
        (json!([]), 10),
        (json!([""]), 10),
        (json!(["/usr/bin/touch", "generated"]), 0),
        (json!(["/usr/bin/touch", "generated"]), 3601),
    ] {
        assert!(Client::connect(&service.runtime).unwrap().request::<proto::Check>(proto::CHECK_RUN,
            json!({"checkpoint_id":cp.id,"command_id":"invalid","argv":argv,"timeout_secs":timeout})).is_err());
    }
    assert!(!service.temp.path().join("generated").exists());
    let params = json!({"checkpoint_id":cp.id,"command_id":"interrupted",
        "argv":["python3","-c","import os,time; from pathlib import Path; Path('generated').write_text(str(os.getpid())); print('started',flush=True); time.sleep(60)"],"timeout_secs":60});
    let check: proto::Check = service.request(proto::CHECK_RUN, params.clone());
    service.wait(|| {
        let log: proto::CheckLog = service.request(proto::CHECK_LOG, json!({"check_id":check.id}));
        log.text.contains("started").then_some(())
    });
    let pid: i32 =
        fs::read_to_string(service.temp.path().join("generated")).unwrap().parse().unwrap();
    service.restart(true);
    let restored: proto::Check = service.request(proto::CHECK_GET, json!({"check_id":check.id}));
    assert_eq!(restored.status, proto::CheckStatus::Interrupted);
    assert!(restored.stale);
    let duplicate: proto::Check = service.request(proto::CHECK_RUN, params);
    assert_eq!(duplicate.id, check.id);
    assert_eq!(duplicate.status, proto::CheckStatus::Interrupted);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<Session>(proto::SESSION_RESUME, json!({"session_id":session.id}))
            .is_err(),
        "must not overlap an orphan verifier"
    );
    // SAFETY: PID was reported by this test's still-live dedicated verification child.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

// Keep process-heavy fixtures bounded under workspace-wide test concurrency.
static SERVICE_SLOTS: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());
struct ServicePermit;
impl ServicePermit {
    fn acquire() -> Self {
        let (mutex, available) = &SERVICE_SLOTS;
        let mut active = mutex.lock().unwrap();
        while *active >= 4 {
            active = available.wait(active).unwrap();
        }
        *active += 1;
        Self
    }
}
impl Drop for ServicePermit {
    fn drop(&mut self) {
        let (mutex, available) = &SERVICE_SLOTS;
        *mutex.lock().unwrap() -= 1;
        available.notify_one();
    }
}

struct Service {
    _permit: ServicePermit,
    temp: TempDir,
    runtime: Runtime,
    child: Child,
}

impl Service {
    fn restart(&mut self, abrupt: bool) {
        self.restart_after(abrupt, || ());
    }

    /// Stops the service, runs `between` while it is down, then starts it again.
    fn restart_after(&mut self, abrupt: bool, between: impl FnOnce()) {
        if abrupt {
            self.child.kill().unwrap();
        } else {
            Client::connect(&self.runtime).unwrap().shutdown().unwrap();
        }
        self.child.wait().unwrap();
        between();
        self.child = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .args(["serve", "--idle-timeout-secs", "1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        self.wait(|| Client::connect(&self.runtime).ok());
    }

    /// Replace the service with one that kills itself (SIGKILL) at the named crash point.
    fn restart_crashing_at(&mut self, point: &str) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.child = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .args(["serve", "--idle-timeout-secs", "1"])
            .env("XAYA_AGENTD_CRASH_AT", point)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        // It may reach its crash point before anyone connects.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Client::connect(&self.runtime).is_err() && self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the crashing service never started");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the service killed itself at its crash point, then start a normal one.
    fn crashed_then_restart(&mut self) {
        use std::os::unix::process::ExitStatusExt;
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the service did not reach its crash point");
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL), "crashed, not exited: {status:?}");
        self.child = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(self.runtime.directory())
            .args(["serve", "--idle-timeout-secs", "1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        self.wait(|| Client::connect(&self.runtime).ok());
    }

    /// Start a session without requiring a reply: the service may die before answering.
    fn start_unanswered(&self) {
        let executable = self.temp.path().join("fake-codex");
        fs::write(
            &executable,
            concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = Client::connect(&self.runtime).unwrap().request::<Value>(
            proto::SESSION_START,
            serde_json::to_value(StartSessionParams {
                cwd: self.temp.path().to_str().unwrap().into(),
                executable: executable.to_str().unwrap().into(),
                codex_home: self.temp.path().join("codex-home").to_str().unwrap().into(),
                sandbox: Default::default(),
                prompt: None,
            })
            .unwrap(),
        );
    }

    fn only(&self) -> Session {
        let sessions: Vec<Session> = self.request(proto::SESSION_LIST, json!({}));
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        sessions.into_iter().next().unwrap()
    }

    fn new() -> Self {
        let permit = ServicePermit::acquire();
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let child = Command::new(BINARY)
            .arg("--runtime-dir")
            .arg(runtime.directory())
            .args(["serve", "--idle-timeout-secs", "1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let service = Self { _permit: permit, temp, runtime, child };
        service.wait(|| Client::connect(&service.runtime).ok());
        service
    }

    fn wait<T>(&self, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for service state");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn request<T: serde::de::DeserializeOwned>(&self, method: &str, params: Value) -> T {
        Client::connect(&self.runtime).unwrap().request(method, params).unwrap()
    }

    fn start(&self) -> Session {
        let executable = self.temp.path().join("fake-codex");
        fs::write(
            &executable,
            concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        self.request(
            proto::SESSION_START,
            serde_json::to_value(StartSessionParams {
                cwd: self.temp.path().to_str().unwrap().into(),
                executable: executable.to_str().unwrap().into(),
                codex_home: self.temp.path().join("codex-home").to_str().unwrap().into(),
                sandbox: Default::default(),
                prompt: None,
            })
            .unwrap(),
        )
    }

    fn get(&self, id: &str) -> Session {
        self.request(proto::SESSION_GET, json!({"session_id":id}))
    }
}

/// `xaya-agentd attach` for `session`; the fixture plays the native TUI, which keeps
/// reconnecting after a lost connection like Codex 0.169.
fn attach_cli(service: &Service, session: &Session) -> Child {
    Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["attach", &session.id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Connections the fixture TUI completed (initialize and start or resume).
fn tui_connections(service: &Service) -> usize {
    fs::read_to_string(service.temp.path().join("codex-home/tui-connections.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn tui_pid(service: &Service) -> i32 {
    fs::read_to_string(service.temp.path().join("codex-home/tui.pid")).unwrap().parse().unwrap()
}

/// Like `Service::wait`, with room for a service restart and a reconnect backoff.
fn wait_long<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn get_if_running(service: &Service, id: &str) -> Option<Session> {
    Client::connect(&service.runtime)
        .ok()?
        .request(proto::SESSION_GET, json!({"session_id":id}))
        .ok()
}

#[test]
fn attach_restarts_a_crashed_service_and_its_tui_reconnects() {
    let mut service = Service::new();
    let session = service.start();
    let mut attach = attach_cli(&service, &session);
    // The fixture logs its connection after the service already shows it attached.
    let bound = wait_long("the TUI to bind", || {
        let s = service.get(&session.id);
        (s.attached && s.native_thread_id.is_some() && tui_connections(&service) == 1).then_some(s)
    });
    let backend = bound.backend_pid.unwrap();
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    // Nothing else starts the service again: the attach command does, and the TUI reconnects to
    // the same backend on the same thread.
    let rejoined = wait_long("the service restarted by attach and the TUI back", || {
        let s = get_if_running(&service, &session.id)?;
        (s.attached && tui_connections(&service) >= 2).then_some(s)
    });
    assert_eq!(rejoined.backend_pid, Some(backend), "{rejoined:?}");
    assert_eq!(rejoined.native_thread_id, bound.native_thread_id);
    assert_eq!(launches(&service), 1, "no second backend");
    assert!(attach.try_wait().unwrap().is_none(), "attach keeps running with its TUI");
    // Stopping the session ends the attached TUI, and the attach command with it.
    let tui = tui_pid(&service);
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    wait_long("attach to exit", || attach.try_wait().unwrap());
    wait_long("the TUI to end", || (!alive(tui)).then_some(()));
}

#[test]
fn attach_leaves_a_deliberately_stopped_service_stopped() {
    let service = Service::new();
    let session = service.start();
    let mut attach = attach_cli(&service, &session);
    wait_long("the TUI to bind", || service.get(&session.id).attached.then_some(()));
    let tui = tui_pid(&service);
    // An orderly shutdown stops every backend: there is nothing to reconnect to.
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    let status = wait_long("attach to exit", || attach.try_wait().unwrap());
    assert!(!status.success(), "the session ended under it: {status:?}");
    wait_long("the TUI to end", || (!alive(tui)).then_some(()));
    thread::sleep(Duration::from_secs(2));
    assert!(
        Client::connect(&service.runtime).is_err(),
        "attach must not restart a stopped service"
    );
}

/// The PID of the service currently holding `service`'s runtime, if one runs.
fn service_pid(service: &Service) -> Option<i32> {
    let out = Command::new("lsof")
        .arg("-t")
        .arg(service.runtime.directory().join("agentd.lock"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).split_whitespace().next()?.parse().ok()
}

/// Gone entirely: neither running nor left unreaped as a zombie.
fn gone(pid: i32) -> bool {
    let out = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[test]
fn terminating_attach_ends_its_tui_and_frees_the_session() {
    let service = Service::new();
    let session = service.start();
    let mut attach = attach_cli(&service, &session);
    wait_long("the TUI to bind", || {
        let s = service.get(&session.id);
        (s.attached && tui_connections(&service) == 1).then_some(())
    });
    let tui = tui_pid(&service);
    // What a terminal, a script or the panel does to the command it started.
    // SAFETY: signalling our own child process.
    unsafe { libc::kill(attach.id() as i32, libc::SIGTERM) };
    wait_long("attach to exit", || attach.try_wait().unwrap());
    wait_long("the TUI to end with it", || gone(tui).then_some(()));
    wait_long("the session to be free", || (!service.get(&session.id).attached).then_some(()));
    // So it can be attached again.
    let mut again = attach_cli(&service, &session);
    wait_long("a second attach", || {
        (service.get(&session.id).attached && tui_connections(&service) == 2).then_some(())
    });
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    wait_long("the second attach to exit", || again.try_wait().unwrap());
}

#[test]
fn attach_reaps_the_services_it_restarts() {
    let mut service = Service::new();
    let session = service.start();
    let mut attach = attach_cli(&service, &session);
    wait_long("the TUI to bind", || {
        let s = service.get(&session.id);
        (s.attached && tui_connections(&service) == 1).then_some(())
    });
    service.child.kill().unwrap();
    service.child.wait().unwrap();
    let restarted = wait_long("attach to restart the service", || {
        let pid = service_pid(&service)?;
        get_if_running(&service, &session.id)?.attached.then_some(pid)
    });
    // The restarted service is attach's child. When it dies, attach must reap it.
    // SAFETY: the PID was just read from the lock holder.
    unsafe { libc::kill(restarted, libc::SIGKILL) };
    wait_long("the killed service to be reaped", || gone(restarted).then_some(()));
    wait_long("attach to start it again", || service_pid(&service).filter(|pid| *pid != restarted));
    wait_long("the TUI back", || get_if_running(&service, &session.id)?.attached.then_some(()));
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    wait_long("attach to exit", || attach.try_wait().unwrap());
}

fn launches(service: &Service) -> usize {
    fs::read_to_string(service.temp.path().join("codex-home/backend-launches.txt"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes whether the fixture-owned process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn crash_restart_readopts_the_hosted_backend_and_its_running_turn() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let initial: proto::EventSnapshot =
        service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"uncertain-restart","text":"NO_REPLY"}),
    );
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Working).then_some(()));
    let pid = service.get(&session.id).backend_pid.unwrap();
    service.restart(true);
    drop(ws);
    // The turn kept running in the hosted backend; the new service adopts it, not a replacement.
    let restored = service.wait(|| {
        let snapshot: proto::EventSnapshot =
            service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
        (snapshot.session.state == proto::SessionState::Working).then_some(snapshot)
    });
    // The stored stream continues: same epoch, numbering after the last stored event.
    assert_eq!(initial.epoch, restored.epoch);
    assert!(restored.last_seq > initial.last_seq);
    assert_eq!(restored.session.native_thread_id.as_deref(), Some("main-thread"));
    assert_eq!(restored.session.backend_pid, Some(pid));
    assert!(alive(pid as i32));
    let turn = restored.session.turn_id.clone().expect("adopted turn");
    receipt(&service, &session, "uncertain-restart", proto::CommandStatus::Unknown);
    let duplicate: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"uncertain-restart","text":"NO_REPLY"}),
    );
    assert_eq!(duplicate.status, proto::CommandStatus::Unknown);
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_CANCEL,
        json!({"session_id":session.id,"command_id":"cancel-adopted","turn_id":turn}),
    );
    receipt(&service, &session, "cancel-adopted", proto::CommandStatus::Succeeded);
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    // Native attach continues the same backend without initializing it twice.
    let info: AttachInfo =
        service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}));
    assert!(info.args.windows(2).any(|args| args == ["resume", "main-thread"]));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"initialize","params":{}}).to_string()))
        .await
        .unwrap();
    response(&mut ws, 1).await;
    ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.unwrap();
    ws.send(Message::text(
        json!({"id":2,"method":"thread/resume","params":{"threadId":"main-thread"}}).to_string(),
    ))
    .await
    .unwrap();
    response(&mut ws, 2).await;
    ws.send(Message::text(json!({"id":3,"method":"fixture/stats"}).to_string())).await.unwrap();
    assert_eq!(response(&mut ws, 3).await["result"]["sends"], 1);
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"after-restart","text":"HELLO"}),
    );
    receipt(&service, &session, "after-restart", proto::CommandStatus::Succeeded);
    assert_eq!(launches(&service), 1, "no second writer was started");
    // An orderly shutdown still ends every managed backend.
    service.restart(false);
    assert_eq!(service.get(&session.id).state, proto::SessionState::Stopped);
    service.wait(|| (!alive(pid as i32)).then_some(()));
}

#[tokio::test]
async fn crash_restart_keeps_a_pending_approval_answerable_from_the_panel() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"approval-turn","text":"APPROVAL"}),
    );
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Blocked).then_some(()));
    service.restart(true);
    drop(ws);
    // Codex re-sends the outstanding request when the adopting connection resumes the thread.
    let snapshot = service.wait(|| {
        let snapshot: proto::EventSnapshot =
            service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
        (snapshot.session.state == proto::SessionState::Blocked && !snapshot.approvals.is_empty())
            .then_some(snapshot)
    });
    let params = json!({"session_id":session.id,"command_id":"panel-answer",
        "approval_id":snapshot.approvals[0].id,"decision":"accept"});
    let _: proto::CommandReceipt = service.request(proto::APPROVAL_RESPOND, params);
    receipt(&service, &session, "panel-answer", proto::CommandStatus::Succeeded);
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    assert_eq!(launches(&service), 1);
}

#[tokio::test]
async fn a_backend_lost_while_the_service_was_down_is_reported_and_resumed_once() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let pid = service.get(&session.id).backend_pid.unwrap() as i32;
    service.restart_after(true, || {
        // SAFETY: the fixture backend is a test-owned process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    });
    drop(ws);
    let failed = service.wait(|| {
        let session = service.get(&session.id);
        (session.state == proto::SessionState::Failed).then_some(session)
    });
    assert!(failed.backend_pid.is_none());
    let _: Session = service.request(proto::SESSION_RESUME, json!({"session_id":session.id}));
    service.wait(|| service.get(&session.id).state.is_active().then_some(()));
    service.wait(|| (launches(&service) == 2).then_some(()));
}

#[tokio::test]
async fn stopping_an_adopted_session_ends_the_backend_and_its_tools() {
    let mut service = Service::new();
    let (session, mut ws) = ready(&service).await;
    ws.send(Message::text(json!({"id":7,"method":"fixture/spawn_child"}).to_string()))
        .await
        .unwrap();
    let tool = response(&mut ws, 7).await["result"]["pid"].as_u64().unwrap() as i32;
    let backend = service.get(&session.id).backend_pid.unwrap() as i32;
    service.restart(true);
    drop(ws);
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    assert!(alive(tool) && alive(backend));
    let stopped: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    assert_eq!(stopped.state, proto::SessionState::Stopped);
    for pid in [backend, tool] {
        service.wait(|| (!alive(pid)).then_some(()));
    }
}

#[tokio::test]
async fn end_tools_ends_exec_tools_but_not_the_backend_or_its_helpers() {
    let service = Service::new();
    let (session, mut ws) = ready(&service).await;
    ws.send(Message::text(json!({"id":7,"method":"fixture/spawn_child"}).to_string()))
        .await
        .unwrap();
    let helper = response(&mut ws, 7).await["result"]["pid"].as_u64().unwrap() as i32;
    ws.send(Message::text(json!({"id":8,"method":"fixture/spawn_tool"}).to_string()))
        .await
        .unwrap();
    let tool = response(&mut ws, 8).await["result"].clone();
    let (tool, grandchild) =
        (tool["pid"].as_u64().unwrap() as i32, tool["grandchild"].as_u64().unwrap() as i32);
    let backend = service.get(&session.id).backend_pid.unwrap() as i32;
    // A running turn's tools are not ended underneath it.
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"hold","text":"HOLD"}),
    );
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Working).then_some(()));
    let mut client = Client::connect(&service.runtime).unwrap();
    let refused = client.request::<Value>("session.end_tools", json!({"session_id":session.id}));
    assert!(refused.is_err(), "{refused:?}");
    let turn = service.get(&session.id).turn_id.unwrap();
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_CANCEL,
        json!({"session_id":session.id,"command_id":"release","turn_id":turn}),
    );
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    let ended: Value = service.request("session.end_tools", json!({"session_id":session.id}));
    assert_eq!(ended["ended"], 2, "{ended}");
    for pid in [tool, grandchild] {
        service.wait(|| (!alive(pid)).then_some(()));
    }
    assert!(alive(backend) && alive(helper), "the backend and its helpers keep running");
    assert_eq!(service.get(&session.id).state, proto::SessionState::Ready);
    let stopped: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    assert_eq!(stopped.state, proto::SessionState::Stopped);
    service.wait(|| (!alive(helper)).then_some(()));
}

#[test]
fn codex_crash_before_its_host_starts_is_settled_as_never_started() {
    let mut service = Service::new();
    service.restart_crashing_at("launch.before_host");
    service.start_unanswered();
    service.crashed_then_restart();
    let session = service.only();
    assert_eq!(session.state, proto::SessionState::Failed, "{session:?}");
    assert!(session.last_error.as_deref().unwrap_or_default().contains("before"), "{session:?}");
    assert_eq!(launches(&service), 0, "no backend was ever started");
    // There is no native thread to resume; a new session starts normally.
    let resumed = Client::connect(&service.runtime)
        .unwrap()
        .request::<Session>(proto::SESSION_RESUME, json!({"session_id":session.id}));
    assert!(resumed.is_err(), "{resumed:?}");
    let next = service.start();
    assert_ne!(next.id, session.id);
    service.wait(|| (launches(&service) == 1).then_some(()));
}

#[test]
fn codex_crash_right_after_its_host_starts_still_rejoins_it() {
    let mut service = Service::new();
    service.restart_crashing_at("launch.after_host");
    service.start_unanswered();
    service.crashed_then_restart();
    let id = service.only().id;
    let ready = service.wait(|| {
        let session = service.get(&id);
        (session.state == proto::SessionState::Ready).then_some(session)
    });
    let pid = ready.backend_pid.expect("adopted PID recorded") as i32;
    assert!(alive(pid));
    assert_eq!(launches(&service), 1, "never launched twice");
    let stopped: Session = service.request(proto::SESSION_STOP, json!({"session_id":id}));
    assert_eq!(stopped.state, proto::SessionState::Stopped);
    service.wait(|| (!alive(pid)).then_some(()));
}

#[tokio::test]
async fn codex_crash_in_the_middle_of_stop_finishes_the_stop_on_restart() {
    let mut service = Service::new();
    let (session, ws) = ready(&service).await;
    let pid = service.get(&session.id).backend_pid.unwrap() as i32;
    drop(ws);
    service.restart_crashing_at("stop.after_intent");
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    let _ = Client::connect(&service.runtime)
        .unwrap()
        .request::<Value>(proto::SESSION_STOP, json!({"session_id":session.id}));
    service.crashed_then_restart();
    let stopped = service.get(&session.id);
    assert_eq!(stopped.state, proto::SessionState::Stopped, "{stopped:?}");
    service.wait(|| (!alive(pid)).then_some(()));
    assert_eq!(launches(&service), 1);
}

#[tokio::test]
async fn failed_durable_write_never_dispatches_the_control_turn() {
    let service = Service::new();
    let (session, _ws) = ready(&service).await;
    let database = service.runtime.directory().join("state/agentd.db");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.execute("DROP TABLE sessions", []).unwrap();
    let mut client = Client::connect(&service.runtime).unwrap();
    assert!(
        client
            .request::<proto::CommandReceipt>(
                proto::SESSION_SEND,
                json!({"session_id":session.id,"command_id":"must-not-run","text":"FORBIDDEN"})
            )
            .is_err()
    );
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Failed).then_some(()));
    assert!(!service.temp.path().join("turns-audit.txt").exists());
}

#[tokio::test]
async fn terminal_auto_restore_does_not_resume_an_inactive_managed_backend() {
    let service = Service::new();
    let (session, _ws) = ready(&service).await;
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .args(["restore", "main-thread"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("explicitly run xaya attach"));
    assert_eq!(service.get(&session.id).state, proto::SessionState::Stopped);
    assert!(service.get(&session.id).backend_pid.is_none());
}

#[test]
fn terminal_auto_restore_preserves_unmanaged_codex_resume() {
    let service = Service::new();
    let bin = service.temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let codex = bin.join("codex");
    fs::write(&codex, "#!/bin/sh\nprintf '%s %s' \"$1\" \"$2\"\n").unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin)
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let output = Command::new(BINARY)
        .arg("--runtime-dir")
        .arg(service.runtime.directory())
        .env("PATH", path)
        .args(["restore", "unmanaged-thread"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "resume unmanaged-thread");
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&self.runtime) {
            let _ = client.shutdown();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn attach(info: &AttachInfo) -> tokio_tungstenite::WebSocketStream<UnixStream> {
    let path = info.args.iter().find(|arg| arg.starts_with("unix://")).unwrap();
    let stream = UnixStream::connect(path.trim_start_matches("unix://")).await.unwrap();
    tokio_tungstenite::client_async("ws://localhost/", stream).await.unwrap().0
}

async fn response(ws: &mut tokio_tungstenite::WebSocketStream<UnixStream>, id: i64) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let raw = ws.next().await.unwrap().unwrap();
            if let Message::Text(text) = raw {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value.get("id") == Some(&json!(id)) && value.get("method").is_none() {
                    return value;
                }
            }
        }
    })
    .await
    .unwrap()
}

async fn ready(service: &Service) -> (Session, tokio_tungstenite::WebSocketStream<UnixStream>) {
    let session = service.start();
    let info: AttachInfo =
        service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"initialize","params":{}}).to_string()))
        .await
        .unwrap();
    response(&mut ws, 1).await;
    ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"id":2,"method":"thread/start","params":{}}).to_string()))
        .await
        .unwrap();
    response(&mut ws, 2).await;
    (session, ws)
}

fn receipt(
    service: &Service,
    session: &Session,
    id: &str,
    expected: proto::CommandStatus,
) -> proto::CommandReceipt {
    service.wait(|| {
        let receipt: proto::CommandReceipt = service
            .request(proto::SESSION_COMMAND, json!({"session_id":session.id,"command_id":id}));
        (receipt.status == expected).then_some(receipt)
    })
}

async fn approval_request(ws: &mut tokio_tungstenite::WebSocketStream<UnixStream>) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Message::Text(raw) = ws.next().await.unwrap().unwrap() {
                let message: Value = serde_json::from_str(&raw).unwrap();
                if message["method"] == "item/commandExecution/requestApproval" {
                    return message;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn control_send_is_deduplicated_and_stream_has_a_contiguous_snapshot_boundary() {
    let service = Service::new();
    let (session, mut ws) = ready(&service).await;
    let mut watcher = Client::connect(&service.runtime).unwrap();
    let snapshot = watcher.subscribe(&session.id).unwrap();
    assert_eq!(snapshot.session.native_thread_id.as_deref(), Some("main-thread"));
    let (sender, receiver) = std::sync::mpsc::channel();
    let stream = thread::spawn(move || {
        let mut events = Vec::new();
        while let Ok(event) = watcher.next_event() {
            let finished = matches!(&event.payload, proto::EventPayload::TurnCompleted { .. });
            events.push(event);
            if finished {
                break;
            }
        }
        sender.send(events).unwrap();
    });
    let params = json!({"session_id":session.id,"command_id":"send-1","text":"HELLO"});
    let _: proto::CommandReceipt = service.request(proto::SESSION_SEND, params.clone());
    let _: proto::CommandReceipt = service.request(proto::SESSION_SEND, params);
    receipt(&service, &session, "send-1", proto::CommandStatus::Succeeded);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<proto::CommandReceipt>(
                proto::SESSION_SEND,
                json!({"session_id":session.id,"command_id":"send-1","text":"DIFFERENT"})
            )
            .is_err()
    );
    let events = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    stream.join().unwrap();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.seq, snapshot.last_seq + index as u64 + 1);
        assert_eq!(event.epoch, snapshot.epoch);
    }
    let text: String = events
        .iter()
        .filter_map(|event| match &event.payload {
            proto::EventPayload::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "fixture:HELLO");
    ws.send(Message::text(json!({"id":9,"method":"fixture/stats"}).to_string())).await.unwrap();
    assert_eq!(response(&mut ws, 9).await["result"]["sends"], 1);
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"send-error","text":"ERROR"}),
    );
    assert_eq!(
        receipt(&service, &session, "send-error", proto::CommandStatus::Failed).error.as_deref(),
        Some("fixture rejection")
    );
}

#[tokio::test]
async fn cancellation_checks_the_turn_and_leaves_the_same_backend_available() {
    let service = Service::new();
    let (session, _ws) = ready(&service).await;
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"hold","text":"HOLD"}),
    );
    let sent = receipt(&service, &session, "hold", proto::CommandStatus::Succeeded);
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_CANCEL,
        json!({"session_id":session.id,"command_id":"stale-cancel","turn_id":"old-turn"}),
    );
    receipt(&service, &session, "stale-cancel", proto::CommandStatus::Failed);
    assert_eq!(service.get(&session.id).state, proto::SessionState::Working);
    let pid = service.get(&session.id).backend_pid;
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_CANCEL,
        json!({"session_id":session.id,"command_id":"cancel","turn_id":sent.turn_id}),
    );
    receipt(&service, &session, "cancel", proto::CommandStatus::Succeeded);
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Ready).then_some(()));
    assert_eq!(service.get(&session.id).backend_pid, pid);
}

#[tokio::test]
async fn backend_loss_keeps_unacknowledged_commands_uncertain_and_never_replays_them() {
    let service = Service::new();
    let (session, _ws) = ready(&service).await;
    let params = json!({"session_id":session.id,"command_id":"uncertain","text":"NO_REPLY"});
    let _: proto::CommandReceipt = service.request(proto::SESSION_SEND, params.clone());
    service.wait(|| (service.get(&session.id).state == proto::SessionState::Working).then_some(()));
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    receipt(&service, &session, "uncertain", proto::CommandStatus::Unknown);
    let duplicate: proto::CommandReceipt = service.request(proto::SESSION_SEND, params);
    assert_eq!(duplicate.status, proto::CommandStatus::Unknown);
    assert_eq!(service.get(&session.id).state, proto::SessionState::Stopped);
}

#[tokio::test]
async fn native_and_controller_approvals_have_one_winner_and_reject_late_answers() {
    let service = Service::new();
    let (session, mut ws) = ready(&service).await;
    for (number, native_wins) in [(1, false), (2, true)] {
        let _: proto::CommandReceipt = service.request(proto::SESSION_SEND,
            json!({"session_id":session.id,"command_id":format!("approval-turn-{number}"),"text":"APPROVAL"}));
        let native = approval_request(&mut ws).await;
        let snapshot: proto::EventSnapshot =
            service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
        assert_eq!(snapshot.session.state, proto::SessionState::Blocked);
        let id = &snapshot.approvals[0].id;
        if native_wins {
            ws.send(Message::text(
                json!({"id":native["id"],"result":{"decision":"accept"}}).to_string(),
            ))
            .await
            .unwrap();
            service.wait(|| {
                (service.get(&session.id).state == proto::SessionState::Ready).then_some(())
            });
        }
        let params = json!({"session_id":session.id,"command_id":format!("decision-{number}"),"approval_id":id,"decision":"decline"});
        let _: proto::CommandReceipt = service.request(proto::APPROVAL_RESPOND, params.clone());
        receipt(
            &service,
            &session,
            &format!("decision-{number}"),
            if native_wins {
                proto::CommandStatus::Failed
            } else {
                proto::CommandStatus::Succeeded
            },
        );
        if !native_wins {
            let _: proto::CommandReceipt = service.request(proto::APPROVAL_RESPOND, params);
            ws.send(Message::text(
                json!({"id":native["id"],"result":{"decision":"accept"}}).to_string(),
            ))
            .await
            .unwrap();
            service.wait(|| {
                (service.get(&session.id).state == proto::SessionState::Ready).then_some(())
            });
        }
        ws.send(Message::text(json!({"id":10 + number,"method":"fixture/stats"}).to_string()))
            .await
            .unwrap();
        assert_eq!(response(&mut ws, 10 + number).await["result"]["answers"], number);
        let snapshot: proto::EventSnapshot =
            service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
        assert!(snapshot.approvals.is_empty());
    }
}

#[tokio::test]
async fn restarting_a_backend_never_reuses_a_previous_approval_identity() {
    let service = Service::new();
    let (session, mut ws) = ready(&service).await;
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"before-restart","text":"APPROVAL"}),
    );
    approval_request(&mut ws).await;
    let old: proto::EventSnapshot =
        service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
    let old_id = old.approvals[0].id.clone();
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    let _: Session = service.request(proto::SESSION_RESUME, json!({"session_id":session.id}));
    let info: AttachInfo =
        service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"initialize","params":{}}).to_string()))
        .await
        .unwrap();
    response(&mut ws, 1).await;
    ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.unwrap();
    ws.send(Message::text(
        json!({"id":2,"method":"thread/resume","params":{"threadId":"main-thread"}}).to_string(),
    ))
    .await
    .unwrap();
    response(&mut ws, 2).await;
    let _: proto::CommandReceipt = service.request(
        proto::SESSION_SEND,
        json!({"session_id":session.id,"command_id":"after-restart","text":"APPROVAL"}),
    );
    approval_request(&mut ws).await;
    let fresh: proto::EventSnapshot =
        service.request(proto::APPROVAL_LIST, json!({"session_id":session.id}));
    assert_ne!(old_id, fresh.approvals[0].id);
    let _: proto::CommandReceipt = service.request(proto::APPROVAL_RESPOND,
        json!({"session_id":session.id,"command_id":"late-old-dialog","approval_id":old_id,"decision":"accept"}));
    receipt(&service, &session, "late-old-dialog", proto::CommandStatus::Failed);
    assert_eq!(service.get(&session.id).state, proto::SessionState::Blocked);
}

#[tokio::test]
async fn native_tui_binds_thread_detaches_and_resumes_without_stopping_backend() {
    let service = Service::new();
    let session = service.start();
    let params = json!({"session_id":session.id});
    let info: AttachInfo = service.request(proto::SESSION_ATTACH_INFO, params.clone());
    assert!(!info.args.iter().any(|arg| arg == "resume"));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"initialize","params":{}}).to_string()))
        .await
        .unwrap();
    assert!(response(&mut ws, 1).await.get("result").is_some());
    ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.unwrap();
    ws.send(Message::text(json!({"id":2,"method":"thread/start","params":{}}).to_string()))
        .await
        .unwrap();
    assert_eq!(response(&mut ws, 2).await["result"]["thread"]["id"], "main-thread");
    let bound = service.get(&session.id);
    assert_eq!(bound.native_thread_id.as_deref(), Some("main-thread"));
    assert!(bound.attached);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<AttachInfo>(proto::SESSION_ATTACH_INFO, params.clone())
            .is_err()
    );
    let endpoint = info.args.iter().find_map(|arg| arg.strip_prefix("unix://")).unwrap();
    assert_eq!(fs::metadata(endpoint).unwrap().permissions().mode() & 0o777, 0o600);
    // Vendor frames larger than the control socket limit pass through unchanged.
    ws.send(Message::text(json!({"id":3,"method":"fixture/large"}).to_string())).await.unwrap();
    assert_eq!(response(&mut ws, 3).await["result"].as_str().unwrap().len(), 2 * 1024 * 1024);
    ws.send(Message::text(json!({"id":4,"method":"thread/start","params":{}}).to_string()))
        .await
        .unwrap();
    assert_eq!(response(&mut ws, 4).await["error"]["code"], proto::CONFLICT);
    ws.send(Message::text(
        json!({"id":5,"method":"turn/start","params":{"threadId":"main-thread"}}).to_string(),
    ))
    .await
    .unwrap();
    response(&mut ws, 5).await;
    assert_eq!(service.get(&session.id).state, proto::SessionState::Working);
    // Detachment keeps the backend and prevents the daemon's one-second idle exit.
    ws.close(None).await.unwrap();
    drop(ws);
    service.wait(|| (!service.get(&session.id).attached).then_some(()));
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(service.get(&session.id).backend_pid, bound.backend_pid);
    assert_eq!(service.get(&session.id).state, proto::SessionState::Ready);
    let info: AttachInfo = service.request(proto::SESSION_ATTACH_INFO, params.clone());
    assert!(info.args.windows(2).any(|pair| pair == ["resume", "main-thread"]));
    assert!(!info.args.iter().any(|arg| arg == "--sandbox" || arg == "--ask-for-approval"));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"initialize","params":{}}).to_string()))
        .await
        .unwrap();
    assert!(response(&mut ws, 1).await.get("result").is_some());
    ws.send(Message::text(json!({"method":"initialized"}).to_string())).await.unwrap();
    ws.send(Message::text(
        json!({"id":2,"method":"thread/resume","params":{"threadId":"main-thread"}}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(response(&mut ws, 2).await["result"]["thread"]["id"], "main-thread");
    let stopped: Session = service.request(proto::SESSION_STOP, params);
    assert_eq!(stopped.state, proto::SessionState::Stopped);
    assert!(!std::path::Path::new(endpoint).exists());
    assert_eq!(
        service.request::<Session>(proto::SESSION_STOP, json!({"session_id":session.id})).state,
        proto::SessionState::Stopped
    );
    assert_eq!(Client::connect(&service.runtime).unwrap().status().unwrap().active_sessions, 0);
}

#[tokio::test]
async fn unexpected_engine_exit_reports_failure_and_removes_attach_endpoint() {
    let service = Service::new();
    let session = service.start();
    let info: AttachInfo =
        service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"fixture/exit"}).to_string())).await.unwrap();
    let failed = service.wait(|| {
        let session = service.get(&session.id);
        (session.state == proto::SessionState::Failed).then_some(session)
    });
    assert!(failed.last_error.is_some());
    assert!(!failed.attached);
    assert!(failed.backend_pid.is_none());
    assert_eq!(Client::connect(&service.runtime).unwrap().status().unwrap().active_sessions, 0);
    assert!(
        Client::connect(&service.runtime)
            .unwrap()
            .request::<AttachInfo>(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}))
            .is_err()
    );
}

#[tokio::test]
async fn shutdown_reaps_active_backend_and_its_tool_process_group() {
    let service = Service::new();
    let session = service.start();
    let info: AttachInfo =
        service.request(proto::SESSION_ATTACH_INFO, json!({"session_id":session.id}));
    let mut ws = attach(&info).await;
    ws.send(Message::text(json!({"id":1,"method":"fixture/spawn_child"}).to_string()))
        .await
        .unwrap();
    let tool_pid = response(&mut ws, 1).await["result"]["pid"].as_u64().unwrap() as i32;
    let backend_pid = service.get(&session.id).backend_pid.unwrap() as i32;
    Client::connect(&service.runtime).unwrap().shutdown().unwrap();
    service.wait(|| (!service.runtime.socket().exists()).then_some(()));
    for pid in [backend_pid, tool_pid] {
        service.wait(|| {
            // SAFETY: signal 0 only checks whether the fixture-owned process still exists.
            let result = unsafe { libc::kill(pid, 0) };
            (result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
                .then_some(())
        });
    }
}

#[test]
fn invalid_start_and_unknown_session_return_errors_without_breaking_status() {
    let service = Service::new();
    let mut client = Client::connect(&service.runtime).unwrap();
    let result = client.request::<Session>(proto::SESSION_START, StartSessionParams {
        cwd: service.temp.path().to_str().unwrap().into(),
        executable: "/does-not-exist/codex".into(),
        codex_home: service.temp.path().join("codex-home").to_str().unwrap().into(),
        sandbox: Default::default(),
        prompt: None,
    });
    assert!(result.is_err());
    assert!(
        client
            .request::<AttachInfo>(proto::SESSION_ATTACH_INFO, SessionParams {
                session_id: "unknown".into()
            })
            .is_err()
    );
    assert_eq!(client.status().unwrap().active_sessions, 0);
}

#[test]
fn a_headless_codex_session_starts_its_thread_and_sends_its_first_message_once() {
    let mut service = Service::new();
    let executable = service.temp.path().join("fake-codex");
    fs::write(&executable, concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")))
        .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let session: Session = service.request(
        proto::SESSION_START,
        serde_json::to_value(StartSessionParams {
            cwd: service.temp.path().to_str().unwrap().into(),
            executable: executable.to_str().unwrap().into(),
            codex_home: service.temp.path().join("codex-home").to_str().unwrap().into(),
            sandbox: Default::default(),
            prompt: Some("Implement the task".into()),
        })
        .unwrap(),
    );
    let first = |service: &Service| -> Option<Value> {
        let receipt: Value = Client::connect(&service.runtime)
            .unwrap()
            .request(
                proto::SESSION_COMMAND,
                json!({"session_id":session.id,"command_id":"xaya-first-message"}),
            )
            .ok()?;
        (receipt["status"] == "succeeded").then_some(receipt)
    };
    let receipt = wait_long("the first message", || first(&service));
    assert!(receipt["turn_id"].as_str().is_some(), "{receipt}");
    let ready = wait_long("the first turn to end", || {
        let current = service.get(&session.id);
        (current.state == proto::SessionState::Ready).then_some(current)
    });
    assert_eq!(ready.native_thread_id.as_deref(), Some("main-thread"), "{ready:?}");
    let audit_path = service.temp.path().join("turns-audit.txt");
    let audit = || fs::read_to_string(&audit_path).unwrap();
    assert_eq!(audit(), "Implement the task\n");
    // A service that crashed rejoins the backend and its thread without sending the message again.
    service.restart(true);
    wait_long("the readopted session", || {
        let current = service.get(&session.id);
        (current.state == proto::SessionState::Ready).then_some(())
    });
    assert!(first(&service).is_some(), "the receipt stays");
    assert_eq!(audit(), "Implement the task\n", "the first message is sent once");
    // A native TUI attaches by resuming that thread; the backend is never initialized twice.
    let mut tui = attach_cli(&service, &service.get(&session.id));
    wait_long("the native TUI", || (tui_connections(&service) >= 1).then_some(()));
    assert_eq!(service.get(&session.id).native_thread_id.as_deref(), Some("main-thread"));
    let _ = tui.kill();
    let _ = tui.wait();
    let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
}

#[test]
fn a_headless_first_message_is_sent_once_whenever_the_service_crashes() {
    for point in ["headless.after_thread", "headless.after_send"] {
        let mut service = Service::new();
        service.restart_crashing_at(point);
        let executable = service.temp.path().join("fake-codex");
        fs::write(
            &executable,
            concat!("#!/usr/bin/env python3\n", include_str!("fixtures/codex.py")),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let _ = Client::connect(&service.runtime).unwrap().request::<Value>(
            proto::SESSION_START,
            serde_json::to_value(StartSessionParams {
                cwd: service.temp.path().to_str().unwrap().into(),
                executable: executable.to_str().unwrap().into(),
                codex_home: service.temp.path().join("codex-home").to_str().unwrap().into(),
                sandbox: Default::default(),
                prompt: Some("Implement the task".into()),
            })
            .unwrap(),
        );
        service.crashed_then_restart();
        let session = wait_long("the readopted session", || {
            let current = service.only();
            (current.state == proto::SessionState::Ready && current.turn_id.is_none())
                .then_some(current)
        });
        assert_eq!(session.native_thread_id.as_deref(), Some("main-thread"), "{point}");
        // Let a delayed first turn surface, if one was wrongly sent twice.
        thread::sleep(Duration::from_millis(600));
        let audit = fs::read_to_string(service.temp.path().join("turns-audit.txt")).unwrap();
        assert_eq!(audit, "Implement the task\n", "sent exactly once after a crash at {point}");
        let _: Session = service.request(proto::SESSION_STOP, json!({"session_id":session.id}));
    }
}
