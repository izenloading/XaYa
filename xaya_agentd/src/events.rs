//! Session snapshots and subscriber registration share the same lock as event sequencing.
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use xaya_agent_proto::{
    self as proto, Approval, CommandReceipt, ControlOperation, EventPayload, EventSnapshot,
    Session, SessionEvent,
};

use crate::storage::{EventHead, Persistence, Record};

/// Stored history per session: past this, the oldest events are dropped down to the floor.
const HISTORY_LIMIT: u64 = 4 << 20;
const HISTORY_FLOOR: u64 = 3 << 20;
use crate::{Error, Result};

pub(crate) type SharedSession = Arc<Mutex<SessionData>>;

/// Bumped after every event that can change the board (anything but text deltas), so board
/// watchers wake instead of polling. One service per process: a process-wide signal suffices.
pub(crate) struct BoardSignal {
    version: Mutex<u64>,
    changed: Condvar,
}

pub(crate) static BOARD: BoardSignal =
    BoardSignal { version: Mutex::new(0), changed: Condvar::new() };

impl BoardSignal {
    fn bump(&self) {
        let mut version = self.version.lock().unwrap_or_else(|poison| poison.into_inner());
        *version += 1;
        self.changed.notify_all();
    }

    pub(crate) fn version(&self) -> u64 {
        *self.version.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// The current version once it differs from `seen`, or `seen` after `timeout`.
    pub(crate) fn wait(&self, seen: u64, timeout: Duration) -> u64 {
        let version = self.version.lock().unwrap_or_else(|poison| poison.into_inner());
        let (version, _) = self
            .changed
            .wait_timeout_while(version, timeout, |version| *version == seen)
            .unwrap_or_else(|poison| poison.into_inner());
        *version
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommandRecord {
    pub operation: ControlOperation,
    pub receipt: CommandReceipt,
}

struct Subscriber {
    sender: SyncSender<SessionEvent>,
    overflow: Arc<AtomicBool>,
}

pub(crate) struct SessionData {
    pub session: Session,
    pub approvals: BTreeMap<String, Approval>,
    pub native_only_approvals: u32,
    pub commands: BTreeMap<String, CommandRecord>,
    epoch: String,
    sequence: u64,
    /// Stored size of the current stream's history.
    history_bytes: u64,
    subscribers: Vec<Subscriber>,
    pub persistence: Option<Persistence>,
    storage_error: Option<String>,
    pub pending_requests: usize,
    pub claude_cancel_ready: bool,
    /// Claude permission hooks held open for a panel answer, by approval ID. Taking a sender
    /// is the single delivery attempt; dropping it releases the hook with no decision.
    pub claude_answers: BTreeMap<String, ClaudeAnswer>,
}

pub(crate) type ClaudeAnswer = tokio::sync::oneshot::Sender<(String, proto::ApprovalDecision)>;

impl Deref for SessionData {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.session
    }
}

impl DerefMut for SessionData {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}

impl SessionData {
    pub fn new(session: Session) -> Self {
        // Internal IDs are random per daemon run and never reused across its sessions.
        Self {
            epoch: session.id.clone(),
            session,
            approvals: BTreeMap::new(),
            commands: BTreeMap::new(),
            native_only_approvals: 0,
            sequence: 0,
            history_bytes: 0,
            subscribers: Vec::new(),
            persistence: None,
            storage_error: None,
            pending_requests: 0,
            claude_cancel_ready: false,
            claude_answers: BTreeMap::new(),
        }
    }

    pub fn snapshot(&self) -> EventSnapshot {
        EventSnapshot {
            session: self.session.clone(),
            approvals: self.approvals.values().cloned().collect(),
            native_only_approvals: self.native_only_approvals,
            epoch: self.epoch.clone(),
            last_seq: self.sequence,
        }
    }

    /// Stored before it is delivered: no client ever sees an event a restarted service could
    /// number differently. An event that cannot be stored is not delivered at all; every
    /// subscriber is told to resynchronise and the session refuses further operations.
    pub fn publish(&mut self, payload: EventPayload) {
        if !matches!(&payload, EventPayload::TextDelta { .. }) {
            self.persist();
        }
        let event = SessionEvent {
            session_id: self.id.clone(),
            epoch: self.epoch.clone(),
            seq: self.sequence + 1,
            payload,
        };
        if let Some(persistence) = &self.persistence {
            let store = &persistence.store;
            match store.append_event(&event) {
                Ok(size) => {
                    self.history_bytes += size;
                    if self.history_bytes > HISTORY_LIMIT {
                        match store.prune_events(&event.session_id, &self.epoch, HISTORY_FLOOR) {
                            Ok(retained) => self.history_bytes = retained,
                            Err(error) => self.storage_error = Some(error.to_string()),
                        }
                    }
                },
                Err(error) => {
                    self.storage_error = Some(format!("event history: {error}"));
                    for subscriber in self.subscribers.drain(..) {
                        subscriber.overflow.store(true, Ordering::Release);
                    }
                    return;
                },
            }
        }
        self.sequence = event.seq;
        if !matches!(&event.payload, EventPayload::TextDelta { .. }) {
            BOARD.bump();
        }
        self.subscribers.retain(|subscriber| match subscriber.sender.try_send(event.clone()) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                subscriber.overflow.store(true, Ordering::Release);
                false
            },
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }

    pub fn persist(&mut self) {
        if let Some(persistence) = &self.persistence {
            let record = Record {
                session: self.session.clone(),
                params: persistence.params.clone(),
                launch: persistence.launch.clone(),
                commands: self.commands.clone(),
                handshake: persistence.handshake.clone(),
            };
            if let Err(error) = persistence.store.save(&record) {
                self.storage_error = Some(error.to_string());
            }
        }
    }

    pub fn storage_ready(&self) -> Result<()> {
        match &self.storage_error {
            Some(error) => Err(Error::Rpc {
                code: proto::ENGINE_ERROR,
                message: format!(
                    "persistent state failed: {error}; operation will not be replayed"
                ),
            }),
            None => Ok(()),
        }
    }

    pub fn new_epoch(&mut self) -> Result<()> {
        self.epoch = crate::storage::epoch()?;
        self.sequence = 0;
        self.history_bytes = 0;
        Ok(())
    }

    /// Continue the stored stream after a service restart: same epoch, next sequence.
    pub fn restore_stream(&mut self, head: EventHead) {
        self.epoch = head.epoch;
        self.sequence = head.seq;
        self.history_bytes = head.bytes;
    }

    pub fn store(&self) -> Option<Arc<crate::storage::Store>> {
        self.persistence.as_ref().map(|persistence| Arc::clone(&persistence.store))
    }

    pub fn changed(&mut self) {
        self.publish(EventPayload::SessionChanged(self.session.clone()));
    }

    pub fn resolve_approval(&mut self, id: &str) {
        if self.approvals.remove(id).is_some() {
            self.publish(EventPayload::ApprovalResolved { approval_id: id.into() });
        }
    }

    pub fn command_changed(&mut self, receipt: CommandReceipt) {
        if let Some(command) = self.commands.get_mut(&receipt.command_id) {
            command.receipt = receipt.clone();
        }
        self.publish(EventPayload::CommandChanged(receipt));
    }

    pub fn subscribe(&mut self, count: Arc<AtomicUsize>) -> Result<Subscription> {
        let mut current = count.load(Ordering::Acquire);
        loop {
            if current >= 8 {
                return Err(Error::Rpc {
                    code: proto::CONFLICT,
                    message: "event subscriber limit reached (8)".into(),
                });
            }
            match count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        let (sender, receiver) = mpsc::sync_channel(64);
        let overflow = Arc::new(AtomicBool::new(false));
        self.subscribers.retain(|subscriber| !subscriber.overflow.load(Ordering::Acquire));
        self.subscribers.push(Subscriber { sender, overflow: Arc::clone(&overflow) });
        Ok(Subscription { snapshot: self.snapshot(), receiver, overflow, count })
    }
}

pub(crate) struct Subscription {
    pub snapshot: EventSnapshot,
    pub receiver: Receiver<SessionEvent>,
    pub overflow: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.overflow.store(true, Ordering::Release);
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SessionData {
        SessionData::new(Session {
            id: "session-epoch".into(),
            engine: "codex".into(),
            cwd: "/tmp".into(),
            sandbox: proto::Sandbox::ReadOnly,
            state: proto::SessionState::Ready,
            native_thread_id: None,
            native_job_id: None,
            turn_id: None,
            attached: false,
            backend_pid: None,
            last_error: None,
            verification: None,
        })
    }

    #[test]
    fn snapshot_and_registered_stream_have_no_sequence_gap() {
        let mut state = state();
        state.changed();
        let subscription = state.subscribe(Arc::default()).unwrap();
        assert_eq!(subscription.snapshot.last_seq, 1);
        state.changed();
        assert_eq!(subscription.receiver.recv().unwrap().seq, 2);
    }

    #[test]
    fn slow_subscriber_requires_resync_without_blocking_other_subscribers() {
        let mut state = state();
        let slow = state.subscribe(Arc::default()).unwrap();
        let fast = state.subscribe(Arc::default()).unwrap();
        for _ in 0..65 {
            state.changed();
            fast.receiver.recv().unwrap();
        }
        assert!(slow.overflow.load(Ordering::Acquire));
        assert!(!fast.overflow.load(Ordering::Acquire));
        let fresh = state.subscribe(Arc::default()).unwrap();
        assert_eq!(fresh.snapshot.last_seq, 65);
    }

    fn stored(temp: &tempfile::TempDir) -> (SessionData, Arc<crate::storage::Store>) {
        let runtime = crate::runtime::Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = crate::storage::Store::open(&runtime, None).unwrap();
        let mut data = state();
        data.persistence = Some(Persistence {
            store: Arc::clone(&store),
            params: crate::storage::EngineParams::Claude(proto::StartClaudeParams {
                cwd: "/tmp".into(),
                executable: "/bin/true".into(),
                config_dir: Some("/tmp".into()),
                prompt: "p".into(),
            }),
            launch: crate::storage::Launch::Ended,
            handshake: None,
        });
        (data, store)
    }

    #[test]
    fn events_are_stored_before_delivery_and_a_restored_stream_continues_its_numbering() {
        let temp = tempfile::tempdir().unwrap();
        let (mut data, store) = stored(&temp);
        let subscription = data.subscribe(Arc::default()).unwrap();
        data.changed();
        let delivered = subscription.receiver.recv().unwrap();
        let saved = store.load_events(&data.id, &delivered.epoch, 0, 1 << 20).unwrap();
        assert_eq!(saved, vec![delivered.clone()]);
        // A restarted service keeps the identity and the numbering of the stored stream.
        let head = store.event_head(&data.id).unwrap().unwrap();
        let mut restored = state();
        restored.persistence = data.persistence.take();
        restored.restore_stream(head);
        assert_eq!(restored.snapshot().epoch, delivered.epoch);
        restored.changed();
        assert_eq!(store.event_head(&restored.id).unwrap().unwrap().seq, 2);
    }

    #[test]
    fn an_event_that_cannot_be_stored_is_never_delivered() {
        let temp = tempfile::tempdir().unwrap();
        let (mut data, store) = stored(&temp);
        data.changed();
        let subscription = data.subscribe(Arc::default()).unwrap();
        store.refuse_events_for_test();
        data.changed();
        assert!(subscription.receiver.try_recv().is_err(), "unstored events are not sent");
        assert!(subscription.overflow.load(Ordering::Acquire), "the subscriber must resync");
        assert!(data.storage_ready().is_err());
        assert_eq!(data.snapshot().last_seq, 1, "a sequence number is never spent unstored");
    }

    #[test]
    fn subscribers_leave_capacity_for_control_connections() {
        let mut state = state();
        let count = Arc::default();
        let subscriptions: Vec<_> =
            (0..8).map(|_| state.subscribe(Arc::clone(&count)).unwrap()).collect();
        assert!(state.subscribe(Arc::clone(&count)).is_err());
        drop(subscriptions);
        assert!(state.subscribe(count).is_ok());
    }
}
