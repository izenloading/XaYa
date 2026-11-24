//! Foreground task orchestration using the same durable operations as the individual commands.

use std::thread;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use xaya_agent_proto as proto;

use crate::client::Client;
pub use crate::drive::Outcome;
use crate::drive::{self, Plan, Round, State, Step};
use crate::runtime::Runtime;
use crate::{Error, Result};

#[derive(Serialize)]
pub struct Report {
    task_id: String,
    session_id: String,
    pub outcome: Outcome,
    reason: Option<String>,
    rounds: Vec<Round>,
}

struct Rpc(Client);

impl drive::Call for Rpc {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.0.request(method, params)
    }
}

pub fn run(
    runtime: &Runtime,
    task_id: &str,
    session_id: Option<String>,
    timeout_secs: u64,
    reviewer: impl FnOnce(
        &proto::Session,
        bool,
    ) -> Result<(proto::ReviewerSpec, Option<proto::ReviewerSpec>)>,
) -> Result<Report> {
    let mut client = Client::connect(runtime)?;
    let task: proto::Task =
        client.request(proto::TASK_GET, proto::TaskParams { task_id: task_id.into() })?;
    // Before the lock: the service's drive holds it while it takes a step, and a person should
    // learn that the service drives the task, not that some cycle is running.
    if task.drive.as_ref().is_some_and(|drive| drive.state.is_active()) {
        return Err(Error::Protocol(
            "the service is driving this task in the background; undrive it first",
        ));
    }
    let _lock = runtime.lock_task_cycle(&task.id)?;
    let session_id = match session_id {
        Some(id) if task.sessions.contains(&id) => id,
        Some(_) => return Err(Error::Protocol("the selected session does not belong to the task")),
        None => match task.sessions.as_slice() {
            [id] => id.clone(),
            _ => return Err(Error::Protocol("select a task session with --session")),
        },
    };
    let mut report = Report {
        task_id: task.id,
        session_id: session_id.clone(),
        outcome: Outcome::Stopped,
        reason: None,
        rounds: vec![],
    };
    if let Some(attention) = task.attention {
        report.outcome = Outcome::NeedsAttention;
        report.reason = Some(attention.reason);
        return Ok(report);
    }
    if task.status != proto::TaskStatus::Open {
        return Err(Error::Protocol("the task is closed"));
    }
    let session: proto::Session = client
        .request(proto::SESSION_GET, proto::SessionParams { session_id: session_id.clone() })?;
    let project: proto::ProjectInfo =
        client.request(proto::PROJECT_GET, proto::ProjectParams { cwd: session.cwd.clone() })?;
    if !project.trusted {
        return Err(Error::Protocol("the cycle requires a trusted project verification plan"));
    }
    let (reviewer, second_reviewer) = reviewer(&session, task.dual_review)?;
    let plan = Plan {
        task_id: task_id.into(),
        session_id,
        reviewer,
        second_reviewer,
        timeout_secs,
        background: false,
    };
    let mut rpc = Rpc(client);
    let mut state = State::new(drive_now());
    let mut announced = Round::default();
    loop {
        let result = drive::step(&mut rpc, &plan, &mut state, drive_now());
        announce(&mut announced, &state);
        match result {
            Ok(Step::Advanced) => (),
            Ok(Step::Wait(_)) => thread::sleep(Duration::from_millis(150)),
            Ok(Step::Finished(outcome, reason)) => {
                report.outcome = outcome;
                report.reason = reason;
                break;
            },
            Err(error) => {
                report.reason = Some(error.to_string());
                break;
            },
        }
    }
    report.rounds = state.rounds;
    Ok(report)
}

fn drive_now() -> u64 {
    drive::now_ms()
}

/// Print each ID of the current round once, as soon as it is known.
fn announce(announced: &mut Round, state: &State) {
    let Some(round) = state.rounds.last() else { return };
    if state.rounds.len() > 1 && round.checkpoint_id.is_none() {
        *announced = Round::default();
    }
    for (name, now, before) in [
        ("checkpoint_id", &round.checkpoint_id, &mut announced.checkpoint_id),
        ("check_id", &round.check_id, &mut announced.check_id),
        ("review_id", &round.review_id, &mut announced.review_id),
        ("command_id", &round.command_id, &mut announced.command_id),
    ] {
        if now.is_some() && now != before {
            eprintln!("{name}={}", now.as_deref().unwrap_or_default());
            before.clone_from(now);
        }
    }
}
