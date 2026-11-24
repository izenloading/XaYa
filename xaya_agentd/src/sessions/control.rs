use super::*;
use crate::events::CommandRecord;
use xaya_agent_proto::{ApprovalDecision, CommandReceipt, CommandStatus, ControlOperation};

pub(super) struct ControlRequest {
    pub command_id: String,
    pub operation: ControlOperation,
    pub queued_at: std::time::Instant,
}

pub(super) fn enqueue(
    sessions: &Sessions,
    id: &str,
    command_id: String,
    operation: ControlOperation,
) -> Result<CommandReceipt> {
    if command_id.is_empty() || command_id.len() > 64 || command_id.chars().any(char::is_control) {
        return Err(rpc(
            proto::INVALID_PARAMS,
            "command_id must contain 1..64 bytes without control characters",
        ));
    }
    if let ControlOperation::Send { text } = &operation
        && (text.trim().is_empty() || text.len() > 8192)
    {
        return Err(rpc(proto::INVALID_PARAMS, "message must contain 1..8192 bytes"));
    }
    match &operation {
        ControlOperation::Respond { approval_id, .. } if approval_id.len() > 128 => {
            return Err(rpc(proto::INVALID_PARAMS, "approval ID is too long"));
        },
        ControlOperation::Cancel { turn_id } if turn_id.len() > 256 => {
            return Err(rpc(proto::INVALID_PARAMS, "turn ID is too long"));
        },
        _ => (),
    }
    let entries = lock(&sessions.entries);
    let entry = entries.get(id).ok_or_else(|| rpc(proto::NOT_FOUND, "unknown session"))?;
    let mut state = lock(&entry.state);
    if let Some(existing) = state.commands.get(&command_id) {
        return if existing.operation == operation {
            Ok(existing.receipt.clone())
        } else {
            Err(rpc(proto::CONFLICT, "command_id was already used for another operation"))
        };
    }
    if !state.state.is_active() || entry.stopping.load(Ordering::Acquire) {
        return Err(rpc(proto::NOT_READY, "session is not running"));
    }
    if state.engine == "claude" {
        if !state.claude_cancel_ready {
            return Err(rpc(proto::NOT_READY, "Claude cancellation bridge is not ready"));
        }
        if let ControlOperation::Cancel { turn_id } = &operation
            && state.turn_id.as_ref() != Some(turn_id)
        {
            return Err(rpc(proto::STALE, "the requested turn is no longer active"));
        }
        // A message is only sent into an idle conversation, so the turn it starts is its own.
        if matches!(operation, ControlOperation::Send { .. })
            && (state.state != SessionState::Ready || state.turn_id.is_some())
        {
            return Err(rpc(proto::CONFLICT, "Claude is busy; send when the session is ready"));
        }
    }
    if state.verification.is_some() {
        return Err(rpc(proto::CONFLICT, "workspace checkpoint in progress"));
    }
    if state.commands.len() >= 256 {
        return Err(rpc(
            proto::CONFLICT,
            "command history limit reached (256); start another session",
        ));
    }
    // A Claude permission hook is already waiting on its HTTP request: the answer goes straight
    // to it, not through the Mods poll. Validate everything before recording the receipt.
    let claude_answer = match &operation {
        ControlOperation::Respond { approval_id, decision } if state.engine == "claude" => {
            let approval = state
                .approvals
                .get(approval_id)
                .ok_or_else(|| rpc(proto::STALE, "approval was answered or expired"))?;
            if !approval.controllable {
                return Err(rpc(proto::NOT_READY, "answer this approval in the native TUI"));
            }
            if !approval.decisions.contains(decision) {
                return Err(rpc(proto::INVALID_PARAMS, "decision is not offered by this approval"));
            }
            if !state.claude_answers.contains_key(approval_id) {
                return Err(rpc(proto::STALE, "approval is no longer pending"));
            }
            Some((approval_id.clone(), decision.clone()))
        },
        _ => None,
    };
    let receipt = CommandReceipt {
        command_id: command_id.clone(),
        status: CommandStatus::Pending,
        turn_id: None,
        error: None,
    };
    state.commands.insert(command_id.clone(), CommandRecord {
        operation: operation.clone(),
        receipt: receipt.clone(),
    });
    state.publish(EventPayload::CommandChanged(receipt));
    state.storage_ready()?;
    if let Some((approval_id, decision)) = claude_answer {
        // Taking the sender is the single delivery attempt; the hook settles the receipt.
        let delivered = state
            .claude_answers
            .remove(&approval_id)
            .is_some_and(|answer| answer.send((command_id.clone(), decision)).is_ok());
        state.resolve_approval(&approval_id);
        if !delivered {
            fail(&mut state, &command_id, "approval expired before the answer was delivered");
        }
        return Ok(state.commands[&command_id].receipt.clone());
    }
    if entry
        .controller
        .try_send(ControlRequest {
            command_id: command_id.clone(),
            operation,
            queued_at: std::time::Instant::now(),
        })
        .is_err()
    {
        fail(&mut state, &command_id, "controller queue is full or stopped; command was not sent");
    }
    Ok(state.commands[&command_id].receipt.clone())
}

pub(super) fn fail(state: &mut SessionData, command_id: &str, error: &str) {
    state.command_changed(CommandReceipt {
        command_id: command_id.into(),
        status: CommandStatus::Failed,
        turn_id: None,
        error: Some(error.chars().take(256).collect()),
    });
}

pub(super) fn uncertain(state: &mut SessionData, command_id: &str, error: &str) {
    state.command_changed(CommandReceipt {
        command_id: command_id.into(),
        status: CommandStatus::Unknown,
        turn_id: None,
        error: Some(error.chars().take(256).collect()),
    });
}

impl Protocol {
    pub(super) fn prepare_control(
        &mut self,
        command: &ControlRequest,
        state: &mut SessionData,
    ) -> Result<(Value, bool)> {
        if state.verification.is_some() {
            return Err(rpc(proto::CONFLICT, "workspace checkpoint in progress"));
        }
        if !self.initialized_sent {
            return Err(rpc(proto::NOT_READY, "attach and initialize the native TUI first"));
        }
        let thread = state
            .native_thread_id
            .as_ref()
            .ok_or_else(|| rpc(proto::NOT_READY, "native thread is not bound"))?
            .clone();
        let (method, params) = match &command.operation {
            ControlOperation::Send { text } => {
                if state.state != SessionState::Ready
                    || state.turn_id.is_some()
                    || self.pending.values().any(|request| request.method == "turn/start")
                {
                    return Err(rpc(
                        proto::CONFLICT,
                        "session already has a turn or a pending send",
                    ));
                }
                (
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":text,"text_elements":[]}]}),
                )
            },
            ControlOperation::Cancel { turn_id } => {
                let turn = state
                    .turn_id
                    .as_ref()
                    .ok_or_else(|| rpc(proto::NOT_READY, "session has no active turn"))?;
                if turn != turn_id {
                    return Err(rpc(proto::STALE, "the requested turn is no longer active"));
                }
                ("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
            },
            ControlOperation::Respond { approval_id, decision } => {
                let approval = state
                    .approvals
                    .get(approval_id)
                    .ok_or_else(|| rpc(proto::STALE, "approval was answered or expired"))?;
                if !approval.controllable {
                    return Err(rpc(proto::NOT_READY, "answer this approval in the native TUI"));
                }
                let value = match decision {
                    ApprovalDecision::Accept => "accept",
                    ApprovalDecision::Decline => "decline",
                    ApprovalDecision::Cancel => "cancel",
                };
                if !approval.decisions.contains(decision) {
                    return Err(rpc(
                        proto::INVALID_PARAMS,
                        "decision is not offered by this approval",
                    ));
                }
                let native_id = self
                    .requests
                    .iter()
                    .find(|(_, request)| request.approval_id.as_deref() == Some(approval_id))
                    .map(|(id, _)| id.clone())
                    .ok_or_else(|| rpc(proto::STALE, "approval is no longer pending"))?;
                if let Some(request) = self.requests.remove(&native_id) {
                    self.finish_request(request, state);
                }
                return Ok((
                    json!({"id":serde_json::from_str::<Value>(&native_id)?,"result":{"decision":value}}),
                    true,
                ));
            },
        };
        if self.pending.len() >= MAX_PENDING {
            return Err(rpc(proto::CONFLICT, "too many outstanding engine requests"));
        }
        self.sequence += 1;
        let id = format!("xaya-control:{}", self.sequence);
        self.pending.insert(id.clone(), Pending {
            original_id: Value::Null,
            method: method.into(),
            generation: self.generation,
            command_id: Some(command.command_id.clone()),
        });
        state.pending_requests = self.pending.len();
        Ok((json!({"id":id,"method":method,"params":params}), false))
    }

    pub(super) fn finish_request(&self, request: ServerRequest, state: &mut SessionData) {
        if let Some(id) = request.approval_id {
            state.resolve_approval(&id);
        }
        if request.native_only {
            state.native_only_approvals = state.native_only_approvals.saturating_sub(1);
            state.publish(EventPayload::NativeOnlyApprovals { count: state.native_only_approvals });
        }
    }
}
