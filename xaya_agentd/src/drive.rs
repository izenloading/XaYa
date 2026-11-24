//! A task's review-and-fix cycle, one durable step at a time.
//!
//! The same steps drive the foreground `xaya task cycle` (over IPC) and the service's background
//! worker (dispatched locally). Every mutating operation is keyed by a command ID chosen when its
//! phase is entered, and the background persists the phase before the operation is issued: a step
//! repeated after a crash queries the original checkpoint, verification, review or fix instead of
//! doing it twice.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xaya_agent_proto as proto;
use xaya_agent_proto::{
    CheckStatus, CheckpointStatus, CommandStatus, ReviewStatus, ReviewVerdict, SessionState,
};

use crate::ownership::new_uuid;
use crate::{Error, Result};

/// Issues one service operation by method name.
pub trait Call {
    fn call(&mut self, method: &str, params: Value) -> Result<Value>;
}

fn get<T: DeserializeOwned>(
    call: &mut impl Call,
    method: &str,
    params: impl Serialize,
) -> Result<T> {
    Ok(serde_json::from_value(call.call(method, serde_json::to_value(params)?)?)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Approved,
    NeedsAttention,
    Stopped,
}

/// What stays fixed for a whole cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub task_id: String,
    pub session_id: String,
    pub reviewer: proto::ReviewerSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_reviewer: Option<proto::ReviewerSpec>,
    pub timeout_secs: u64,
    /// Driven by the service rather than a person at a terminal: wait while the implementing
    /// session is busy, waits for input or has a person attached, instead of stopping; and retake
    /// a checkpoint, verification or review that a service restart interrupted. A fix is never
    /// sent twice.
    #[serde(default)]
    pub background: bool,
}

/// Where the cycle is. Each phase carries the command ID of the operation it issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    /// Before a round: the implementing session must be quiet; Claude is stopped so that its
    /// checkout can be captured.
    Settle,
    Checkpoint {
        command_id: String,
    },
    Check {
        checkpoint_id: String,
        command_id: String,
    },
    Review {
        checkpoint_id: String,
        check_id: String,
        command_id: String,
    },
    /// The fix goes back to the session. The event cursor is taken (and persisted) before the
    /// send, so a turn that ends before its receipt arrives is still seen.
    Fix {
        review_id: String,
        command_id: String,
        /// A person's requested changes (the task revision with this request ID) are sent instead
        /// of the review's findings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<proto::EventCursor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    pub checkpoint_id: Option<String>,
    pub check_id: Option<String>,
    pub review_id: Option<String>,
    pub verdict: Option<ReviewVerdict>,
    pub command_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub phase: Phase,
    /// When the current phase (or its current wait) began; bounds every wait.
    pub since_ms: u64,
    pub rounds: Vec<Round>,
    /// Times the current phase was retaken after its operation failed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub retries: u32,
    /// A retake waits until then; set with `retries`, cleared by the next phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<u64>,
    /// Why the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_reason: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl State {
    pub fn new(now_ms: u64) -> Self {
        Self {
            phase: Phase::Settle,
            since_ms: now_ms,
            rounds: Vec::new(),
            retries: 0,
            retry_at_ms: None,
            retry_reason: None,
        }
    }

    fn enter(&mut self, phase: Phase, now_ms: u64) -> Step {
        self.phase = phase;
        self.since_ms = now_ms;
        self.retries = 0;
        self.retry_at_ms = None;
        self.retry_reason = None;
        Step::Advanced
    }

    /// Retake `phase` after a backoff, or give up once its retries are used up. Its time limit
    /// starts when the backoff ends.
    fn retry(&mut self, phase: Phase, what: &str, error: String, now_ms: u64) -> Result<Step> {
        if self.retries >= RETRIES {
            return Err(stopped(format!("{what} failed after {RETRIES} retries: {error}")));
        }
        let delay = crate::faults::retry_base_ms(RETRY_BASE_MS)
            .saturating_mul(RETRY_FACTOR.saturating_pow(self.retries));
        let at = now_ms.saturating_add(delay);
        self.phase = phase;
        self.since_ms = at;
        self.retries += 1;
        self.retry_at_ms = Some(at);
        self.retry_reason = Some(error);
        Ok(Step::Wait(self.backoff_reason(what, now_ms)))
    }

    fn backoff_reason(&self, what: &str, now_ms: u64) -> String {
        let left = self.retry_at_ms.unwrap_or(now_ms).saturating_sub(now_ms).div_ceil(1000);
        format!(
            "{what} failed; retry {} of {RETRIES} in {left}s: {}",
            self.retries,
            self.retry_reason.as_deref().unwrap_or("unknown error")
        )
    }

    fn round(&mut self) -> &mut Round {
        if self.rounds.is_empty() {
            self.rounds.push(Round::default());
        }
        self.rounds.last_mut().expect("a round exists")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The state changed; take the next step now (after persisting it).
    Advanced,
    /// Nothing to do until something else happens.
    Wait(String),
    Finished(Outcome, Option<String>),
}

const CHECKPOINT_SECS: u64 = 120;
const RECEIPT_SECS: u64 = 60;
const RESUME_SECS: u64 = 60;
/// A background review that failed outright (its reviewer could not start, exited or timed out)
/// has ended and changed nothing; it is retaken after 30s, 2min and 8min, then the drive stops.
const RETRIES: u32 = 3;
const RETRY_BASE_MS: u64 = 30_000;
const RETRY_FACTOR: u64 = 4;

/// Take one step. An error ends the cycle as stopped, with the error as its reason.
pub fn step(call: &mut impl Call, plan: &Plan, state: &mut State, now_ms: u64) -> Result<Step> {
    let task: proto::Task =
        get(call, proto::TASK_GET, proto::TaskParams { task_id: plan.task_id.clone() })?;
    if let Some(attention) = task.attention {
        return Ok(Step::Finished(Outcome::NeedsAttention, Some(attention.reason)));
    }
    if task.status != proto::TaskStatus::Open {
        return Err(Error::Protocol("the task was closed"));
    }
    if let Some(at) = state.retry_at_ms
        && now_ms < at
    {
        return Ok(Step::Wait(state.backoff_reason("the review", now_ms)));
    }
    let session_params = proto::SessionParams { session_id: plan.session_id.clone() };
    let since_ms = state.since_ms;
    let waited = |limit: u64, what: &str| -> Result<Step> {
        if now_ms.saturating_sub(since_ms) >= limit.saturating_mul(1000) {
            Err(stopped(format!("timed out waiting for {what}")))
        } else {
            Ok(Step::Wait(what.into()))
        }
    };
    match state.phase.clone() {
        Phase::Settle => {
            let session: proto::Session = get(call, proto::SESSION_GET, &session_params)?;
            // A freshly launched session may look ready before its first prompt's turn begins;
            // the background never takes a checkpoint before the session has finished a turn.
            if plan.background
                && state.rounds.is_empty()
                && !finished_a_turn(call, &plan.session_id)?
            {
                return Ok(Step::Wait("the implementing session has not finished a turn".into()));
            }
            match session.state {
                SessionState::Ready | SessionState::Stopped
                    if plan.background && session.attached =>
                {
                    Ok(Step::Wait("a person is attached to the implementing session".into()))
                },
                SessionState::Ready if session.engine == "claude" => {
                    let _: proto::Session = get(call, proto::SESSION_STOP_IDLE, &session_params)?;
                    Ok(Step::Advanced)
                },
                SessionState::Ready | SessionState::Stopped => {
                    state.rounds.push(Round::default());
                    Ok(state.enter(Phase::Checkpoint { command_id: new_uuid()? }, now_ms))
                },
                _ if plan.background => Ok(Step::Wait("the implementing session is busy".into())),
                _ => Err(Error::Protocol("the implementing session must be ready or stopped")),
            }
        },
        Phase::Checkpoint { command_id } => {
            let checkpoint: proto::Checkpoint =
                get(call, proto::CHECKPOINT_CREATE, proto::CheckpointCreateParams {
                    session_id: plan.session_id.clone(),
                    command_id,
                })?;
            state.round().checkpoint_id = Some(checkpoint.id.clone());
            match checkpoint.status {
                CheckpointStatus::Creating => waited(CHECKPOINT_SECS, "the checkpoint"),
                CheckpointStatus::Interrupted if plan.background => {
                    Ok(state.enter(Phase::Checkpoint { command_id: new_uuid()? }, now_ms))
                },
                CheckpointStatus::Ready => Ok(state.enter(
                    Phase::Check { checkpoint_id: checkpoint.id, command_id: new_uuid()? },
                    now_ms,
                )),
                _ => Err(stopped(checkpoint.error.unwrap_or_else(|| "checkpoint failed".into()))),
            }
        },
        Phase::Check { checkpoint_id, command_id } => {
            let session: proto::Session = get(call, proto::SESSION_GET, &session_params)?;
            let project: proto::ProjectInfo =
                get(call, proto::PROJECT_GET, proto::ProjectParams { cwd: session.cwd })?;
            if !project.trusted {
                return Err(Error::Protocol(
                    "the cycle requires a trusted project verification plan",
                ));
            }
            let check: proto::Check = get(call, proto::CHECK_PROJECT, proto::ProjectCheckParams {
                checkpoint_id: checkpoint_id.clone(),
                command_id,
                config_hash: project.config_hash,
            })?;
            state.round().check_id = Some(check.id.clone());
            if check.status == CheckStatus::Running {
                let limit = check.timeout_secs + 30 * (check.steps.len() as u64 + 2) + 10;
                return waited(limit, "verification");
            }
            if check.status == CheckStatus::Interrupted && plan.background {
                return Ok(
                    state.enter(Phase::Check { checkpoint_id, command_id: new_uuid()? }, now_ms)
                );
            }
            if check.status != CheckStatus::Succeeded || check.stale {
                return Err(stopped(check.error.unwrap_or_else(|| {
                    format!("verification ended {:?} (stale: {})", check.status, check.stale)
                })));
            }
            Ok(state.enter(
                Phase::Review { checkpoint_id, check_id: check.id, command_id: new_uuid()? },
                now_ms,
            ))
        },
        Phase::Review { checkpoint_id, check_id, command_id } => {
            let review: proto::Review = get(call, proto::REVIEW_START, proto::ReviewStartParams {
                checkpoint_id: checkpoint_id.clone(),
                command_id,
                engine: plan.reviewer.engine,
                executable: plan.reviewer.executable.clone(),
                config_dir: plan.reviewer.config_dir.clone(),
                second_reviewer: plan.second_reviewer.clone(),
                check_id: Some(check_id.clone()),
                base: None,
                timeout_secs: plan.timeout_secs,
                task_id: Some(plan.task_id.clone()),
            })?;
            state.round().review_id = Some(review.id.clone());
            if review.status == ReviewStatus::Running {
                return waited(plan.timeout_secs + 60, "the review");
            }
            if review.status == ReviewStatus::Interrupted && plan.background {
                let retake = Phase::Review { checkpoint_id, check_id, command_id: new_uuid()? };
                return Ok(state.enter(retake, now_ms));
            }
            if review.status == ReviewStatus::Failed && plan.background && !review.stale {
                let error = review.error.unwrap_or_else(|| "the reviewer failed".into());
                let retake = Phase::Review { checkpoint_id, check_id, command_id: new_uuid()? };
                return state.retry(retake, "the review", error, now_ms);
            }
            state.round().verdict = review.verdict;
            // The review's own bookkeeping may have asked for attention.
            let task: proto::Task =
                get(call, proto::TASK_GET, proto::TaskParams { task_id: plan.task_id.clone() })?;
            if let Some(attention) = task.attention {
                return Ok(Step::Finished(Outcome::NeedsAttention, Some(attention.reason)));
            }
            if review.stale || review.status != ReviewStatus::Completed || review.error.is_some() {
                return Err(stopped(
                    review.error.unwrap_or_else(|| "the review failed or became stale".into()),
                ));
            }
            match review.verdict {
                Some(ReviewVerdict::Approved) => {
                    return Ok(Step::Finished(Outcome::Approved, None));
                },
                Some(ReviewVerdict::NeedsFix) => (),
                _ => return Err(stopped(review.reasons.join("; "))),
            }
            if task.status != proto::TaskStatus::Open {
                return Err(Error::Protocol("the task was closed"));
            }
            if task.fix_rounds.len() >= task.fix_budget as usize {
                return Ok(Step::Finished(
                    Outcome::Stopped,
                    Some("the task's fix budget is used up".into()),
                ));
            }
            let fix = Phase::Fix {
                review_id: review.id,
                command_id: new_uuid()?,
                revision: None,
                cursor: None,
                turn_id: None,
            };
            Ok(state.enter(fix, now_ms))
        },
        Phase::Fix { review_id, command_id, revision, cursor: None, turn_id: None } => {
            let session: proto::Session = get(call, proto::SESSION_GET, &session_params)?;
            match session.state {
                SessionState::Stopped => {
                    let _: proto::Session = get(call, proto::SESSION_RESUME, &session_params)?;
                    return waited(RESUME_SECS, "the implementing session to resume");
                },
                SessionState::Starting => {
                    return waited(RESUME_SECS, "the implementing session to resume");
                },
                SessionState::Ready if !(session.engine == "claude" && session.attached) => (),
                _ if plan.background => {
                    return Ok(Step::Wait(
                        "the implementing session is not available for a fix".into(),
                    ));
                },
                SessionState::Ready => {
                    return Err(Error::Protocol(
                        "the implementing session is no longer available for a fix",
                    ));
                },
                _ => return Err(Error::Protocol("the resumed session did not become ready")),
            }
            let history: proto::EventHistory =
                get(call, proto::EVENTS_HISTORY, proto::HistoryParams {
                    session_id: plan.session_id.clone(),
                    // Past every event (stored sequence numbers are SQLite integers): only the
                    // cursor comes back.
                    after_seq: i64::MAX as u64,
                })?;
            let cursor = proto::EventCursor { epoch: history.epoch, seq: history.last_seq };
            let fix =
                Phase::Fix { review_id, command_id, revision, cursor: Some(cursor), turn_id: None };
            Ok(state.enter(fix, now_ms))
        },
        Phase::Fix { review_id, command_id, revision, cursor: Some(cursor), turn_id: None } => {
            state.round().command_id = Some(command_id.clone());
            let sent = match &revision {
                Some(request) => {
                    let Some(revision) =
                        task.revisions.iter().find(|revision| &revision.request_id == request)
                    else {
                        return Err(Error::Protocol("the requested changes are not recorded"));
                    };
                    get::<proto::CommandReceipt>(call, proto::SESSION_SEND, proto::SendParams {
                        session_id: plan.session_id.clone(),
                        command_id: command_id.clone(),
                        text: revision_message(revision),
                    })
                },
                None => get::<proto::CommandReceipt>(
                    call,
                    proto::REVIEW_SEND,
                    proto::ReviewSendParams {
                        review_id: review_id.clone(),
                        command_id: command_id.clone(),
                    },
                ),
            };
            let receipt = match sent {
                Ok(receipt) => receipt,
                Err(error) => {
                    let task: proto::Task = get(call, proto::TASK_GET, proto::TaskParams {
                        task_id: plan.task_id.clone(),
                    })?;
                    if let Some(attention) = task.attention {
                        return Ok(Step::Finished(Outcome::NeedsAttention, Some(attention.reason)));
                    }
                    return Err(error);
                },
            };
            match (receipt.status, receipt.turn_id) {
                (CommandStatus::Pending, _) => waited(RECEIPT_SECS, "the fix to be delivered"),
                (CommandStatus::Succeeded, Some(turn_id)) => {
                    state.round().turn_id = Some(turn_id.clone());
                    let fix = Phase::Fix {
                        review_id,
                        command_id,
                        revision,
                        cursor: Some(cursor),
                        turn_id: Some(turn_id),
                    };
                    Ok(state.enter(fix, now_ms))
                },
                _ => Err(stopped(
                    receipt.error.unwrap_or_else(|| "the fix turn was not confirmed".into()),
                )),
            }
        },
        Phase::Fix {
            review_id,
            command_id,
            revision,
            cursor: Some(mut cursor),
            turn_id: Some(turn_id),
        } => {
            loop {
                let history: proto::EventHistory =
                    get(call, proto::EVENTS_HISTORY, proto::HistoryParams {
                        session_id: plan.session_id.clone(),
                        after_seq: cursor.seq,
                    })?;
                if history.epoch != cursor.epoch
                    || history.last_seq < cursor.seq
                    || history.first_seq > cursor.seq.saturating_add(1)
                {
                    return Err(Error::Protocol(
                        "the fix turn's event history is no longer available",
                    ));
                }
                for event in history.events {
                    if event.epoch != cursor.epoch || event.seq != cursor.seq + 1 {
                        return Err(Error::Protocol("the fix turn's event history changed"));
                    }
                    cursor.seq = event.seq;
                    if let proto::EventPayload::TurnCompleted { turn_id: id, status } =
                        event.payload
                        && id == turn_id
                    {
                        return if status == "completed" {
                            Ok(state.enter(Phase::Settle, now_ms))
                        } else {
                            Err(stopped(format!("the fix turn ended {status}")))
                        };
                    }
                }
                // Keep what was read, without restarting the turn's time limit.
                state.phase = Phase::Fix {
                    review_id: review_id.clone(),
                    command_id: command_id.clone(),
                    revision: revision.clone(),
                    cursor: Some(cursor.clone()),
                    turn_id: Some(turn_id.clone()),
                };
                if cursor.seq >= history.last_seq {
                    break;
                }
            }
            let session: proto::Session = get(call, proto::SESSION_GET, &session_params)?;
            if session.state == SessionState::Blocked && plan.background {
                return Ok(Step::Wait("the fix turn waits for input".into()));
            }
            if !session.state.is_active() || session.state == SessionState::Blocked {
                return Err(Error::Protocol("the fix turn needs input or the session has stopped"));
            }
            waited(plan.timeout_secs, "the fix turn")
        },
        Phase::Fix { cursor: None, turn_id: Some(_), .. } => {
            Err(Error::Protocol("a fix turn was recorded without its event cursor"))
        },
    }
}

/// The message that carries a person's requested changes to the implementing session. They come
/// from the person the session works for, so they are instructions, unlike a reviewer's report.
fn revision_message(revision: &proto::TaskRevision) -> String {
    format!(
        "[XaYa · checkpoint {}]\nThe person you are working for looked at your change and asks \
         for the following. Make these changes, then stop; XaYa will verify and review \
         again.\n\n{}\n",
        revision.checkpoint_id, revision.feedback
    )
}

/// Restart a finished drive at a fix turn that carries the person's requested changes in
/// `request_id`, keeping the rounds taken so far.
pub fn revise(
    progress: Option<&serde_json::Value>,
    review_id: &str,
    command_id: String,
    request_id: &str,
    now_ms: u64,
) -> Result<serde_json::Value> {
    let mut state = match progress {
        Some(progress) => serde_json::from_value(progress.clone())
            .map_err(|_| Error::Protocol("the drive's recorded progress is unreadable"))?,
        None => State::new(now_ms),
    };
    state.enter(
        Phase::Fix {
            review_id: review_id.into(),
            command_id,
            revision: Some(request_id.into()),
            cursor: None,
            turn_id: None,
        },
        now_ms,
    );
    Ok(serde_json::to_value(state)?)
}

/// Whether the session's stored history shows a completed turn. Reads at most a bounded number
/// of pages; a longer history without one keeps the cycle waiting.
fn finished_a_turn(call: &mut impl Call, session_id: &str) -> Result<bool> {
    let mut after_seq = 0;
    for _ in 0..64 {
        let history: proto::EventHistory =
            get(call, proto::EVENTS_HISTORY, proto::HistoryParams {
                session_id: session_id.into(),
                after_seq,
            })?;
        if history
            .events
            .iter()
            .any(|event| matches!(event.payload, proto::EventPayload::TurnCompleted { .. }))
        {
            return Ok(true);
        }
        match history.events.last() {
            Some(last) if last.seq < history.last_seq => after_seq = last.seq,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn stopped(message: String) -> Error {
    Error::Rpc { code: proto::NOT_READY, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> Phase {
        Phase::Review { checkpoint_id: "cp".into(), check_id: "ck".into(), command_id: "c".into() }
    }

    #[test]
    fn retries_back_off_thirty_seconds_times_four_then_stop() {
        let mut state = State::new(0);
        let mut now = 1_000;
        for (retry, delay) in [(1, 30_000), (2, 120_000), (3, 480_000)] {
            let step = state.retry(review(), "the review", "exit 3".into(), now).unwrap();
            assert_eq!(state.retries, retry);
            assert_eq!(state.retry_at_ms, Some(now + delay));
            assert_eq!(state.since_ms, now + delay, "the time limit starts after the backoff");
            let Step::Wait(reason) = step else { panic!("{step:?}") };
            assert!(reason.contains(&format!("retry {retry} of 3 in {}s: exit 3", delay / 1000)));
            now += delay;
        }
        let error = state.retry(review(), "the review", "exit 3".into(), now).unwrap_err();
        assert!(error.to_string().contains("the review failed after 3 retries: exit 3"));
        assert_eq!(state.enter(Phase::Settle, now), Step::Advanced);
        assert_eq!((state.retries, state.retry_at_ms, state.retry_reason), (0, None, None));
    }

    #[test]
    fn progress_recorded_before_retries_loads_without_a_backoff() {
        let old = serde_json::json!({"phase":{"phase":"settle"},"since_ms":5,"rounds":[]});
        let state: State = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(state, State::new(5));
        assert_eq!(serde_json::to_value(&state).unwrap(), old, "no new fields until a retry");
    }
}
