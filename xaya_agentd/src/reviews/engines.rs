//! Headless, read-only reviewer runs. Each engine runs in its own process group, carries the
//! review's ownership marker, and is ended at the deadline or when the review is aborted.
//!
//! - Codex: `exec` in the read-only sandbox with approvals `never`; the final message is written to
//!   a file and must match the output schema.
//! - Claude: `-p` over stream-json with the stdio permission channel; only reading tools are
//!   allowed, every other request is denied. Its answer is the result's `structured_output`.
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use xaya_agent_proto::{ReviewEngine, ReviewUsage};

use super::verdict::MAX_OUTPUT;
use crate::Error;
use crate::ownership::OWNER_ENV;

/// Longest stdout line read from either engine; longer lines end the run.
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Tools a Claude reviewer may use: reading the checkout and answering.
const CLAUDE_ALLOWED: &[&str] = &["Read", "Grep", "Glob", "StructuredOutput"];

pub(super) struct Run<'a> {
    pub engine: ReviewEngine,
    pub executable: &'a str,
    /// Set as the engine's configuration variable; `None` removes it (the engine's default).
    pub config_dir: Option<&'a str>,
    pub checkout: &'a Path,
    /// Private files of this review (schema, final message, stderr), outside the checkout.
    pub scratch: &'a Path,
    pub prompt: &'a str,
    /// The output schema the engine must answer with.
    pub schema: &'a serde_json::Value,
    pub marker: &'a str,
    pub deadline: Instant,
    pub aborted: &'a dyn Fn() -> bool,
}

#[derive(Debug, Default)]
pub(super) struct Outcome {
    /// The structured answer as JSON text; `None` when the engine gave none.
    pub output: Option<String>,
    pub native_session_id: Option<String>,
    pub usage: Option<ReviewUsage>,
}

pub(super) enum Failure {
    TimedOut,
    Aborted,
    Error(String),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Error(error.to_string())
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Error(error.to_string())
    }
}

pub(super) fn run(run: &Run<'_>) -> std::result::Result<Outcome, Failure> {
    match run.engine {
        ReviewEngine::Codex => codex(run),
        ReviewEngine::Claude => claude(run),
    }
}

/// A started reviewer process group; ended (TERM, then KILL) when dropped unless reaped.
struct Group {
    child: Child,
    reaped: bool,
}

impl Group {
    fn spawn(run: &Run<'_>, config_env: &str, args: &[&str]) -> std::result::Result<Self, Failure> {
        let stderr = std::fs::File::create(run.scratch.join("stderr.log"))?;
        let mut command = Command::new(run.executable);
        match run.config_dir {
            Some(dir) => command.env(config_env, dir),
            None => command.env_remove(config_env),
        };
        command
            .args(args)
            .current_dir(run.checkout)
            .env(OWNER_ENV, run.marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .process_group(0);
        Ok(Self { child: command.spawn()?, reaped: false })
    }

    fn signal(&self, signal: libc::c_int) {
        if self.reaped {
            return;
        }
        if let Ok(pid) = i32::try_from(self.child.id()) {
            // SAFETY: the group leader is our unreaped child, so its group ID is still ours.
            unsafe { libc::killpg(pid, signal) };
        }
    }

    /// Wait for the leader until the deadline or an abort; ends the group on either.
    fn wait(&mut self, run: &Run<'_>) -> std::result::Result<std::process::ExitStatus, Failure> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                // Anything the leader left in its group goes with it.
                if let Ok(pid) = i32::try_from(self.child.id()) {
                    // SAFETY: signalling a group we created; a vanished group is ESRCH.
                    unsafe { libc::killpg(pid, libc::SIGKILL) };
                }
                return Ok(status);
            }
            if (run.aborted)() {
                return Err(Failure::Aborted);
            }
            if Instant::now() >= run.deadline {
                return Err(Failure::TimedOut);
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr_tail(run: &Run<'_>) -> String {
        let text = std::fs::read(run.scratch.join("stderr.log")).unwrap_or_default();
        let start = text.len().saturating_sub(800);
        String::from_utf8_lossy(&text[start..]).trim().to_owned()
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        self.signal(libc::SIGTERM);
        let grace = Instant::now() + Duration::from_secs(2);
        while Instant::now() < grace {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.signal(libc::SIGKILL);
        let _ = self.child.wait();
        self.reaped = true;
    }
}

/// Lines of `stream` on a channel, each bounded; a closed channel means EOF or an overlong line.
fn lines(stream: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = Vec::new();
            match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if line.len() > MAX_LINE => return,
                Ok(_) => {
                    if sender.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                        return;
                    }
                },
            }
        }
    });
    receiver
}

fn codex(run: &Run<'_>) -> std::result::Result<Outcome, Failure> {
    let schema = run.scratch.join("schema.json");
    std::fs::write(&schema, run.schema.to_string())?;
    let last = run.scratch.join("final.json");
    let schema_arg = schema.to_str().ok_or(Failure::Error("non-UTF-8 review path".into()))?;
    let last_arg = last.to_str().ok_or(Failure::Error("non-UTF-8 review path".into()))?;
    let mut group = Group::spawn(run, "CODEX_HOME", &[
        "--no-daemon",
        "-a",
        "never",
        "exec",
        "--sandbox",
        "read-only",
        "--json",
        "--output-schema",
        schema_arg,
        "-o",
        last_arg,
        "-",
    ])?;
    if let Some(mut stdin) = group.child.stdin.take() {
        stdin.write_all(run.prompt.as_bytes())?;
    }
    let events = lines(group.child.stdout.take().ok_or(Failure::Error("no stdout".into()))?);
    let status = group.wait(run)?;
    let mut outcome = Outcome::default();
    // The stdout reader may still be draining the final event after the child exits.
    let drain_until = Instant::now() + Duration::from_millis(250);
    while let Some(remaining) = drain_until.checked_duration_since(Instant::now())
        && let Ok(line) = events.recv_timeout(remaining)
    {
        let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
        match event["type"].as_str() {
            Some("thread.started") => {
                outcome.native_session_id = event["thread_id"].as_str().map(str::to_owned);
            },
            Some("turn.completed") => {
                if let Some(counters) = usage(&event["usage"], "cached_input_tokens") {
                    outcome.usage = Some(counters);
                }
            },
            _ => (),
        }
    }
    if !status.success() {
        return Err(Failure::Error(format!(
            "Codex reviewer exited with {status}: {}",
            Group::stderr_tail(run)
        )));
    }
    outcome.output = std::fs::read(&last)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_OUTPUT)
        .and_then(|bytes| String::from_utf8(bytes).ok());
    Ok(outcome)
}

fn send(stdin: &mut ChildStdin, value: &Value) -> std::io::Result<()> {
    stdin.write_all(format!("{value}\n").as_bytes())?;
    stdin.flush()
}

fn claude(run: &Run<'_>) -> std::result::Result<Outcome, Failure> {
    let schema = run.schema.to_string();
    let mut group = Group::spawn(run, "CLAUDE_CONFIG_DIR", &[
        "-p",
        "--tools",
        "Read,Grep,Glob",
        "--permission-mode",
        "manual",
        "--permission-prompt-tool",
        "stdio",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--json-schema",
        &schema,
    ])?;
    let mut stdin = group.child.stdin.take().ok_or(Failure::Error("no stdin".into()))?;
    let frames = lines(group.child.stdout.take().ok_or(Failure::Error("no stdout".into()))?);
    send(
        &mut stdin,
        &json!({"type": "control_request", "request_id": "xaya-review-init",
                             "request": {"subtype": "initialize"}}),
    )?;
    let mut outcome = Outcome::default();
    loop {
        if (run.aborted)() {
            return Err(Failure::Aborted);
        }
        let now = Instant::now();
        if now >= run.deadline {
            return Err(Failure::TimedOut);
        }
        let wait = (run.deadline - now).min(Duration::from_millis(200));
        let line = match frames.recv_timeout(wait) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let status = group.wait(run)?;
                return Err(Failure::Error(format!(
                    "Claude reviewer ended without a result ({status}): {}",
                    Group::stderr_tail(run)
                )));
            },
        };
        let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
        match frame["type"].as_str() {
            Some("control_response") if frame["response"]["request_id"] == "xaya-review-init" => {
                if frame["response"]["subtype"] != "success" {
                    return Err(Failure::Error("Claude reviewer initialization failed".into()));
                }
                send(
                    &mut stdin,
                    &json!({"type": "user",
                                         "message": {"role": "user", "content": run.prompt}}),
                )?;
            },
            Some("control_request") => {
                let request = &frame["request"];
                let allowed = request["subtype"] == "can_use_tool"
                    && request["tool_name"].as_str().is_some_and(|t| CLAUDE_ALLOWED.contains(&t));
                let response = if allowed {
                    json!({"behavior": "allow", "updatedInput": request["input"]})
                } else {
                    json!({"behavior": "deny", "message": "Denied by XaYa: reviews are read-only"})
                };
                send(
                    &mut stdin,
                    &json!({"type": "control_response", "response": {
                    "subtype": "success", "request_id": frame["request_id"],
                    "response": response}}),
                )?;
            },
            Some("result") => {
                outcome.native_session_id = frame["session_id"].as_str().map(str::to_owned);
                outcome.usage = usage(&frame["usage"], "cache_read_input_tokens");
                if frame["is_error"] != true {
                    outcome.output = frame
                        .get("structured_output")
                        .filter(|value| !value.is_null())
                        .map(Value::to_string);
                }
                break;
            },
            _ => (),
        }
    }
    // The answer is in; closing input lets Claude exit. Its exit status no longer matters.
    drop(stdin);
    let grace = Instant::now() + Duration::from_secs(5);
    while Instant::now() < grace {
        if matches!(group.child.try_wait(), Ok(Some(_))) {
            group.reaped = true;
            if let Ok(pid) = i32::try_from(group.child.id()) {
                // SAFETY: as in `Group::wait`: what the leader left in its group goes with it.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Dropping a group whose leader still runs ends it.
    Ok(outcome)
}

fn usage(value: &Value, cache_read_key: &str) -> Option<ReviewUsage> {
    let counters = ReviewUsage {
        input_tokens: value["input_tokens"].as_u64(),
        output_tokens: value["output_tokens"].as_u64(),
        cache_read_input_tokens: value[cache_read_key].as_u64(),
        cache_creation_input_tokens: value["cache_creation_input_tokens"].as_u64(),
    };
    (counters != ReviewUsage::default()).then_some(counters)
}
