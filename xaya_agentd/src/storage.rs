//! Durable session identity and command deduplication; no automatic replay or backend adoption.
use std::collections::BTreeMap;
use std::fs::{DirBuilder, File};
use std::io::{self, Read};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use xaya_agent_proto::{Session, StartClaudeParams, StartSessionParams};

use crate::events::CommandRecord;
use crate::runtime::{FileLock, Runtime};
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Launch {
    Prepared,
    Running(u32),
    Ended,
    /// A native Claude daemon owns this worker and possibly background tools. Never signal its
    /// PID.
    External,
    /// XaYa's own PTY host owns the program; every owned process carries `marker`.
    Owned(OwnedLaunch),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwnedLaunch {
    pub host_dir: String,
    pub marker: String,
    pub host_pid: Option<i32>,
    pub child_pid: Option<i32>,
    /// `XAYA_TOOL` value given to this launch's tool processes only. Launches recorded by an
    /// older service have none, and their tools can only be ended by stopping the session.
    #[serde(default)]
    pub tool_marker: Option<String>,
    /// A stop was asked for. Durable before any signal, so a service that crashes in the middle
    /// of stopping finishes the stop on restart instead of rejoining the program.
    #[serde(default)]
    pub stop_requested: bool,
}

impl OwnedLaunch {
    /// Ownership ends only when no process of the launch remains: the host, the program and
    /// every descendant, including ones that left the process tree. The program created its own
    /// session, so a member left there also keeps the launch owned, even one whose environment
    /// the platform hides and that has already left its owned parent.
    pub(crate) fn ended(&self) -> Result<bool> {
        if !crate::ownership::scan(&self.marker)?.is_empty() {
            return Ok(false);
        }
        match self.child_pid {
            Some(sid) => Ok(!crate::ownership::session_in_use(sid, &self.marker)?),
            None => Ok(true),
        }
    }
}

/// Untagged Codex records retain their original JSON representation for backward compatibility.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum EngineParams {
    Codex(StartSessionParams),
    Claude(StartClaudeParams),
}

impl EngineParams {
    pub fn executable(&self) -> &str {
        match self {
            Self::Codex(p) => &p.executable,
            Self::Claude(p) => &p.executable,
        }
    }

    pub fn home(&self) -> &str {
        match self {
            Self::Codex(p) => &p.codex_home,
            Self::Claude(p) => p.config_dir.as_deref().unwrap_or_default(),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Record {
    pub session: Session,
    pub params: EngineParams,
    pub launch: Launch,
    pub commands: BTreeMap<String, CommandRecord>,
    /// The native client's `initialize` parameters, replayed when a restarted service adopts a
    /// still-running Codex backend on a new connection.
    #[serde(default)]
    pub handshake: Option<serde_json::Value>,
}

/// The newest stored stream of a session: a restarted service continues it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventHead {
    pub epoch: String,
    pub seq: u64,
    pub bytes: u64,
}

pub(crate) struct Store {
    connection: Mutex<Connection>,
    /// Event history only. `synchronous=NORMAL` in WAL mode: a committed event survives a crash
    /// of this process (the one failure history must cover) without an fsync per text delta;
    /// a power loss may drop the newest events, together with every client that saw them.
    events: Mutex<Connection>,
    _lock: FileLock,
    directory: PathBuf,
}

impl Store {
    pub fn open(runtime: &Runtime, override_dir: Option<PathBuf>) -> Result<Arc<Self>> {
        let directory = match override_dir {
            Some(directory) => directory,
            None if !runtime.is_default() => runtime.directory().join("state"),
            None => {
                let home = std::env::var_os("HOME").ok_or(Error::Protocol("HOME is not set"))?;
                #[cfg(target_os = "macos")]
                let base = PathBuf::from(home).join("Library/Application Support");
                #[cfg(not(target_os = "macos"))]
                let base = std::env::var_os("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(home).join(".local/state"));
                base.join("xaya/agentd")
            },
        };
        if !directory.is_absolute() {
            return Err(Error::Protocol("state directory must be absolute"));
        }
        let parent = directory.parent().ok_or(Error::Protocol("invalid state directory"))?;
        DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let private = Runtime::prepare(Some(directory))?;
        let lock = FileLock::exclusive(private.private_file("store.lock", false)?)?;
        let file = private.private_file("agentd.db", false)?;
        let connection = Connection::open(private.directory().join("agentd.db"))?;
        connection.busy_timeout(std::time::Duration::from_secs(1))?;
        drop(file);
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE sessions (id TEXT PRIMARY KEY, record TEXT NOT \
                 NULL); PRAGMA user_version=1; COMMIT;",
            )?,
            1..=11 => (),
            _ => return Err(Error::Protocol("unsupported agent storage version")),
        }
        if version < 2 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE checkpoints (id TEXT PRIMARY KEY, record TEXT NOT \
                 NULL); PRAGMA user_version=2; COMMIT;",
            )?;
        }
        if version < 3 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE checks (id TEXT PRIMARY KEY, record TEXT NOT \
                 NULL); PRAGMA user_version=3; COMMIT;",
            )?;
        }
        if version < 4 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE project_trust (root TEXT PRIMARY KEY, hash TEXT \
                 NOT NULL); PRAGMA user_version=4; COMMIT;",
            )?;
        }
        if version < 5 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE land_previews (id TEXT PRIMARY KEY, record TEXT \
                 NOT NULL); PRAGMA user_version=5; COMMIT;",
            )?;
        }
        if version < 6 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE events (session_id TEXT NOT NULL, epoch TEXT NOT \
                 NULL, seq INTEGER NOT NULL, event TEXT NOT NULL, PRIMARY KEY(session_id, epoch, \
                 seq)); PRAGMA user_version=6; COMMIT;",
            )?;
        }
        if version < 7 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE reviews (id TEXT PRIMARY KEY, record TEXT NOT \
                 NULL); PRAGMA user_version=7; COMMIT;",
            )?;
        }
        if version < 8 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL); \
                 PRAGMA user_version=8; COMMIT;",
            )?;
        }
        if version < 9 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS retired_sessions (id TEXT PRIMARY \
                 KEY); PRAGMA user_version=9; COMMIT;",
            )?;
        }
        if version < 10 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS task_capacity (project TEXT PRIMARY \
                 KEY, capacity INTEGER NOT NULL CHECK(capacity BETWEEN 1 AND 16)); PRAGMA \
                 user_version=10; COMMIT;",
            )?;
        }
        if version < 11 {
            connection.execute_batch(
                "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS task_engine (project TEXT PRIMARY \
                 KEY, engine TEXT NOT NULL CHECK(engine IN ('claude','codex'))); PRAGMA \
                 user_version=11; COMMIT;",
            )?;
        }
        let events = Connection::open(private.directory().join("agentd.db"))?;
        events.busy_timeout(std::time::Duration::from_secs(1))?;
        events.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Arc::new(Self {
            connection: Mutex::new(connection),
            events: Mutex::new(events),
            _lock: lock,
            directory: private.directory().to_owned(),
        }))
    }

    pub(crate) fn integration_root(&self) -> Result<PathBuf> {
        Ok(Runtime::prepare(Some(self.directory.join("integrations")))?.directory().to_owned())
    }

    pub fn load(&self) -> Result<Vec<Record>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM sessions ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub fn has_session(&self, id: &str) -> Result<bool> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn load_checkpoints(&self) -> Result<Vec<xaya_agent_proto::Checkpoint>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM checkpoints ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub fn save_checkpoint(&self, cp: &xaya_agent_proto::Checkpoint) -> Result<()> {
        let value = serde_json::to_string(cp)?;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO checkpoints(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![cp.id, value],
        )?;
        Ok(())
    }

    pub(crate) fn load_checks(&self) -> Result<Vec<crate::checks::Record>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM checks ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub(crate) fn save_check(&self, record: &crate::checks::Record) -> Result<()> {
        let value = serde_json::to_string(record)?;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO checks(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![record.check.id, value],
        )?;
        Ok(())
    }

    pub(crate) fn load_previews(&self) -> Result<Vec<xaya_agent_proto::LandPreview>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM land_previews ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub(crate) fn save_preview(&self, preview: &xaya_agent_proto::LandPreview) -> Result<()> {
        let value = serde_json::to_string(preview)?;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO land_previews(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![preview.id, value],
        )?;
        Ok(())
    }

    pub(crate) fn load_tasks(&self) -> Result<Vec<xaya_agent_proto::Task>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM tasks ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub(crate) fn save_task(&self, task: &xaya_agent_proto::Task) -> Result<()> {
        let value = serde_json::to_string(task)?;
        let mut connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO tasks(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![task.id, value],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn load_reviews(&self) -> Result<Vec<crate::reviews::Record>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement = connection.prepare("SELECT record FROM reviews ORDER BY id")?;
        let values = statement.query_map([], |row| row.get::<_, String>(0))?;
        values.map(|value| Ok(serde_json::from_str(&value?)?)).collect()
    }

    pub(crate) fn save_review(&self, record: &crate::reviews::Record) -> Result<()> {
        let value = serde_json::to_string(record)?;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO reviews(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![record.review.id, value],
        )?;
        Ok(())
    }

    pub(crate) fn project_trusted(&self, root: &str, hash: &str) -> Result<bool> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM project_trust WHERE root=?1 AND hash=?2",
            params![root, hash],
            |row| row.get::<_, u64>(0),
        )? != 0)
    }

    /// Checkout roots that trust exactly these configuration bytes.
    pub(crate) fn roots_trusting(&self, hash: &str) -> Result<Vec<String>> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let mut statement =
            connection.prepare("SELECT root FROM project_trust WHERE hash=?1 ORDER BY root")?;
        let roots = statement
            .query_map(params![hash], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(roots)
    }

    pub(crate) fn trust_project(&self, root: &str, hash: &str) -> Result<()> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO project_trust(root,hash) VALUES(?1,?2) ON CONFLICT(root) DO UPDATE SET \
             hash=excluded.hash",
            params![root, hash],
        )?;
        Ok(())
    }

    pub fn save(&self, record: &Record) -> Result<()> {
        let value = serde_json::to_string(record)?;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        connection.execute(
            "INSERT INTO sessions(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET \
             record=excluded.record",
            params![record.session.id, value],
        )?;
        Ok(())
    }

    pub fn remove(&self, ids: &[String]) -> Result<()> {
        let mut connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let transaction = connection.transaction()?;
        for id in ids {
            transaction.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
            transaction.execute("DELETE FROM events WHERE session_id=?1", params![id])?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Eviction preserves proof that a session ended, so its task's durable launch does not
    /// become indistinguishable from a crash between claiming and creating the session.
    pub(crate) fn retire(&self, ids: &[String]) -> Result<()> {
        let mut connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let transaction = connection.transaction()?;
        for id in ids {
            let value: String =
                transaction
                    .query_row("SELECT record FROM sessions WHERE id=?1", [id], |row| row.get(0))?;
            let record: Record = serde_json::from_str(&value)?;
            if !matches!(
                record.session.state,
                xaya_agent_proto::SessionState::Stopped | xaya_agent_proto::SessionState::Failed
            ) {
                return Err(Error::Protocol("only confirmed ended sessions may be retired"));
            }
            can_resume(&record.launch)?;
            transaction.execute("INSERT OR IGNORE INTO retired_sessions(id) VALUES(?1)", [id])?;
            transaction.execute("DELETE FROM sessions WHERE id=?1", [id])?;
            transaction.execute("DELETE FROM events WHERE session_id=?1", [id])?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// A missing local task policy keeps the original single-slot behavior. Stored invalid
    /// values are errors, never an excuse to dispatch using a more permissive default.
    pub(crate) fn task_capacity(&self, project: &str) -> Result<usize> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let capacity: Option<i64> = connection
            .query_row("SELECT capacity FROM task_capacity WHERE project=?1", [project], |row| {
                row.get(0)
            })
            .optional()?;
        match capacity {
            None => Ok(xaya_agent_proto::TASK_DEFAULT_CAPACITY),
            Some(value) if (1..=xaya_agent_proto::TASK_MAX_CAPACITY as i64).contains(&value) => {
                Ok(value as usize)
            },
            Some(_) => Err(Error::Protocol("invalid stored project task capacity")),
        }
    }

    pub(crate) fn set_task_capacity(&self, project: &str, capacity: usize) -> Result<()> {
        if !(1..=xaya_agent_proto::TASK_MAX_CAPACITY).contains(&capacity) {
            return Err(Error::Rpc {
                code: xaya_agent_proto::INVALID_PARAMS,
                message: "project task capacity must be between 1 and 16".into(),
            });
        }
        let mut connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        // SQLite FAIL triggers can report an error after changing the row. Roll the entire
        // policy change back unless both execution and commit succeed.
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO task_capacity(project,capacity) VALUES(?1,?2) ON CONFLICT(project) DO \
             UPDATE SET capacity=excluded.capacity",
            params![project, capacity as i64],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The project's default implementing engine, if it sets one.
    pub(crate) fn task_engine(
        &self,
        project: &str,
    ) -> Result<Option<xaya_agent_proto::ReviewEngine>> {
        use xaya_agent_proto::ReviewEngine;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        let engine: Option<String> = connection
            .query_row("SELECT engine FROM task_engine WHERE project=?1", [project], |row| {
                row.get(0)
            })
            .optional()?;
        match engine.as_deref() {
            None => Ok(None),
            Some("claude") => Ok(Some(ReviewEngine::Claude)),
            Some("codex") => Ok(Some(ReviewEngine::Codex)),
            Some(_) => Err(Error::Protocol("invalid stored project task engine")),
        }
    }

    /// Set the project's default implementing engine, or with `None` go back to the default.
    pub(crate) fn set_task_engine(
        &self,
        project: &str,
        engine: Option<xaya_agent_proto::ReviewEngine>,
    ) -> Result<()> {
        use xaya_agent_proto::ReviewEngine;
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        match engine {
            None => connection.execute("DELETE FROM task_engine WHERE project=?1", [project])?,
            Some(engine) => {
                let name = match engine {
                    ReviewEngine::Claude => "claude",
                    ReviewEngine::Codex => "codex",
                };
                connection.execute(
                    "INSERT INTO task_engine(project,engine) VALUES(?1,?2) ON CONFLICT(project) \
                     DO UPDATE SET engine=excluded.engine",
                    params![project, name],
                )?
            },
        };
        Ok(())
    }

    pub(crate) fn retired(&self, id: &str) -> Result<bool> {
        let connection =
            self.connection.lock().map_err(|_| Error::Protocol("storage lock poisoned"))?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_sessions WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    fn event_connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.events.lock().map_err(|_| Error::Protocol("storage lock poisoned"))
    }

    /// Stores one event; returns its stored size. An existing (session, epoch, seq) is an error:
    /// a sequence number is never reused for another event.
    pub(crate) fn append_event(&self, event: &xaya_agent_proto::SessionEvent) -> Result<u64> {
        let value = serde_json::to_string(event)?;
        self.event_connection()?.execute(
            "INSERT INTO events(session_id,epoch,seq,event) VALUES(?1,?2,?3,?4)",
            params![event.session_id, event.epoch, event.seq, value],
        )?;
        Ok(value.len() as u64)
    }

    pub(crate) fn event_head(&self, session: &str) -> Result<Option<EventHead>> {
        let connection = self.event_connection()?;
        let epoch: Option<String> = connection
            .query_row(
                "SELECT epoch FROM events WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1",
                params![session],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(error),
            })?;
        let Some(epoch) = epoch else { return Ok(None) };
        let (seq, bytes): (u64, u64) = connection.query_row(
            "SELECT MAX(seq), SUM(length(event)) FROM events WHERE session_id=?1 AND epoch=?2",
            params![session, epoch],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Some(EventHead { epoch, seq, bytes }))
    }

    /// Events of one stream after `after`, in order, until `max_bytes` (at least one event).
    pub(crate) fn load_events(
        &self,
        session: &str,
        epoch: &str,
        after: u64,
        max_bytes: u64,
    ) -> Result<Vec<xaya_agent_proto::SessionEvent>> {
        let connection = self.event_connection()?;
        let mut statement = connection.prepare(
            "SELECT event FROM events WHERE session_id=?1 AND epoch=?2 AND seq>?3 ORDER BY seq",
        )?;
        let mut rows = statement.query(params![session, epoch, after])?;
        let (mut events, mut bytes) = (Vec::new(), 0);
        while let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            bytes += value.len() as u64;
            if bytes > max_bytes && !events.is_empty() {
                break;
            }
            events.push(serde_json::from_str(&value)?);
        }
        Ok(events)
    }

    /// The oldest retained sequence of a stream, 0 when it has none.
    pub(crate) fn first_event(&self, session: &str, epoch: &str) -> Result<u64> {
        Ok(self.event_connection()?.query_row(
            "SELECT IFNULL(MIN(seq), 0) FROM events WHERE session_id=?1 AND epoch=?2",
            params![session, epoch],
            |row| row.get(0),
        )?)
    }

    /// Keeps the newest events of the current stream within `keep` bytes (always the newest
    /// one) and drops every older stream. Returns the bytes retained.
    pub(crate) fn prune_events(&self, session: &str, epoch: &str, keep: u64) -> Result<u64> {
        let mut connection = self.event_connection()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM events WHERE session_id=?1 AND epoch<>?2", params![
            session, epoch
        ])?;
        let (mut retained, mut cutoff) = (0, None);
        {
            let mut statement = transaction.prepare(
                "SELECT seq, length(event) FROM events WHERE session_id=?1 AND epoch=?2 ORDER BY \
                 seq DESC",
            )?;
            let mut rows = statement.query(params![session, epoch])?;
            while let Some(row) = rows.next()? {
                let (seq, size): (u64, u64) = (row.get(0)?, row.get(1)?);
                if retained > 0 && retained + size > keep {
                    cutoff = Some(seq);
                    break;
                }
                retained += size;
            }
        }
        if let Some(cutoff) = cutoff {
            transaction.execute(
                "DELETE FROM events WHERE session_id=?1 AND epoch=?2 AND seq<=?3",
                params![session, epoch, cutoff],
            )?;
        }
        transaction.commit()?;
        Ok(retained)
    }

    #[cfg(test)]
    pub(crate) fn refuse_events_for_test(&self) {
        self.connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_events BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, \
                 'refused for test'); END;",
            )
            .unwrap();
    }
}

pub(crate) struct Persistence {
    pub store: Arc<Store>,
    pub params: EngineParams,
    pub launch: Launch,
    pub handshake: Option<serde_json::Value>,
}

pub(crate) fn epoch() -> Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn can_resume(launch: &Launch) -> Result<()> {
    match launch {
        Launch::External => Err(Error::Protocol(
            "Claude worker or tool ownership requires explicit recovery; replacement is refused",
        )),
        Launch::Prepared => Err(Error::Protocol(
            "previous launch ownership is uncertain; automatic backend replacement is refused",
        )),
        Launch::Running(pid) => {
            let pid = i32::try_from(*pid)
                .ok()
                .filter(|pid| *pid > 0)
                .ok_or(Error::Protocol("invalid recorded process group"))?;
            // SAFETY: signal zero only probes the recorded dedicated process group, never kills it.
            let result = unsafe { libc::kill(-pid, 0) };
            if result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(Error::Protocol(
                    "previous backend process group may still be alive; cannot start another \
                     writer",
                ));
            }
            Ok(())
        },
        Launch::Ended => Ok(()),
        Launch::Owned(owned) => {
            if owned.ended()? {
                Ok(())
            } else {
                Err(Error::Protocol(
                    "XaYa-owned processes of this session are still running; stop it explicitly",
                ))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_capacity_defaults_isolates_projects_and_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(store.task_capacity("/project").unwrap(), 1);
        store.set_task_capacity("/project", 16).unwrap();
        assert_eq!(store.task_capacity("/project").unwrap(), 16);
        assert_eq!(store.task_capacity("/other").unwrap(), 1);
        store.set_task_capacity("/project", 2).unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(store.task_capacity("/project").unwrap(), 2);
        assert_eq!(store.task_capacity("/other").unwrap(), 1);
    }

    #[test]
    fn a_task_engine_is_set_per_project_cleared_and_survives_restart() {
        use xaya_agent_proto::ReviewEngine;
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(store.task_engine("/project").unwrap(), None);
        store.set_task_engine("/project", Some(ReviewEngine::Codex)).unwrap();
        store.set_task_engine("/other", Some(ReviewEngine::Claude)).unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(store.task_engine("/project").unwrap(), Some(ReviewEngine::Codex));
        assert_eq!(store.task_engine("/other").unwrap(), Some(ReviewEngine::Claude));
        store.set_task_engine("/project", None).unwrap();
        store.set_task_engine("/project", None).unwrap();
        assert_eq!(store.task_engine("/project").unwrap(), None);
        assert_eq!(store.task_engine("/other").unwrap(), Some(ReviewEngine::Claude));
    }

    #[test]
    fn task_capacity_rejects_invalid_values_and_corrupt_stored_policies() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.set_task_capacity("/project", 2).unwrap();
        for capacity in [0, 17, usize::MAX] {
            assert!(store.set_task_capacity("/project", capacity).is_err());
            assert_eq!(store.task_capacity("/project").unwrap(), 2);
        }
        store
            .connection
            .lock()
            .unwrap()
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        for value in ["0", "-1", "17", "1.5", "'invalid'"] {
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    &format!("UPDATE task_capacity SET capacity={value} WHERE project='/project'"),
                    [],
                )
                .unwrap();
            assert!(store.task_capacity("/project").is_err(), "accepted {value}");
            let connection = store.connection.lock().unwrap();
            connection
                .execute("UPDATE task_capacity SET capacity=2 WHERE project='/project'", [])
                .unwrap();
        }
    }

    #[test]
    fn task_capacity_write_failure_keeps_the_previous_policy() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.set_task_capacity("/project", 2).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_capacity BEFORE INSERT ON task_capacity BEGIN SELECT \
                 RAISE(FAIL, 'refused for test'); END;",
            )
            .unwrap();
        assert!(store.set_task_capacity("/project", 3).is_err());
        assert_eq!(store.task_capacity("/project").unwrap(), 2);
        assert!(store.set_task_capacity("/other", 3).is_err());
        assert_eq!(store.task_capacity("/other").unwrap(), 1);
    }

    fn assert_task_capacity_after_trigger_rolls_back(operation: &str) {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.set_task_capacity("/existing", 2).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(&format!(
                "CREATE TRIGGER refuse_capacity AFTER {operation} ON task_capacity BEGIN SELECT \
                 RAISE(FAIL, 'refused after write'); END;"
            ))
            .unwrap();
        let (project, expected) =
            if operation == "INSERT" { ("/new", 1) } else { ("/existing", 2) };
        assert!(store.set_task_capacity(project, 3).is_err());
        assert_eq!(
            store.task_capacity(project).unwrap(),
            expected,
            "{operation} error must roll back the policy change"
        );
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(store.task_capacity(project).unwrap(), expected);
    }

    #[test]
    fn task_capacity_after_insert_failure_rolls_back() {
        assert_task_capacity_after_trigger_rolls_back("INSERT");
    }

    #[test]
    fn task_capacity_after_update_failure_rolls_back() {
        assert_task_capacity_after_trigger_rolls_back("UPDATE");
    }

    #[test]
    fn task_capacity_schema_nine_migration_preserves_session_and_retirement_proof() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.save(&ended_record("preserved")).unwrap();
        store.save(&ended_record("retired")).unwrap();
        store.retire(&["retired".into()]).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE task_capacity; PRAGMA user_version=9;")
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.has_session("preserved").unwrap());
        assert!(store.retired("retired").unwrap());
        assert_eq!(store.task_capacity("/project").unwrap(), 1);
        store.set_task_capacity("/project", 3).unwrap();
        assert_eq!(store.task_capacity("/project").unwrap(), 3);
    }

    fn ended_record(id: &str) -> Record {
        serde_json::from_value(serde_json::json!({
            "session": {"id":id, "engine":"codex", "cwd":"/tmp", "sandbox":"read-only",
                "state":"stopped", "native_thread_id":null, "native_job_id":null,
                "turn_id":null, "attached":false, "backend_pid":null, "last_error":null,
                "verification":null},
            "params":{"cwd":"/tmp", "executable":"/bin/true", "codex_home":"/tmp",
                "sandbox":"read-only"},
            "launch":"Ended", "commands":{}
        }))
        .unwrap()
    }

    #[test]
    fn retired_sessions_keep_durable_end_proof_but_plain_removal_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.save(&ended_record("codex-ended")).unwrap();
        store.save(&ended_record("codex-removed")).unwrap();
        store.retire(&["codex-ended".into()]).unwrap();
        store.remove(&["codex-removed".into()]).unwrap();
        assert!(!store.has_session("codex-ended").unwrap());
        assert!(store.retired("codex-ended").unwrap());
        assert!(!store.retired("codex-removed").unwrap());
        assert!(!store.retired("codex-unknown").unwrap());
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.retired("codex-ended").unwrap());
    }

    #[test]
    fn retirement_rejects_uncertain_launches_and_rolls_back_the_whole_batch() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.save(&ended_record("codex-ended")).unwrap();
        let mut uncertain = ended_record("codex-uncertain");
        uncertain.session.state = xaya_agent_proto::SessionState::Interrupted;
        store.save(&uncertain).unwrap();
        assert!(store.retire(&["codex-ended".into(), "codex-uncertain".into()]).is_err());
        assert!(store.has_session("codex-ended").unwrap());
        assert!(!store.retired("codex-ended").unwrap());
        uncertain.session.state = xaya_agent_proto::SessionState::Failed;
        uncertain.launch = Launch::Prepared;
        store.save(&uncertain).unwrap();
        assert!(store.retire(&["codex-uncertain".into()]).is_err());
        assert!(!store.retired("codex-uncertain").unwrap());
    }

    #[test]
    fn schema_eight_migration_preserves_sessions_and_adds_retirement_proof() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.save(&ended_record("codex-ended")).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE retired_sessions; PRAGMA user_version=8;")
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.has_session("codex-ended").unwrap());
        assert!(!store.retired("codex-ended").unwrap());
        store.retire(&["codex-ended".into()]).unwrap();
        assert!(store.retired("codex-ended").unwrap());
    }

    #[test]
    fn state_is_private_and_cannot_be_owned_by_two_services() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        assert_eq!(
            std::fs::metadata(runtime.directory().join("state/agentd.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(Store::open(&runtime, None).is_err());
        drop(store);
        assert!(Store::open(&runtime, None).is_ok());
    }

    #[test]
    fn uncertain_launches_and_live_groups_never_start_another_writer() {
        assert!(can_resume(&Launch::Prepared).is_err());
        assert!(can_resume(&Launch::External).is_err());
        // SAFETY: getpgrp has no preconditions; no signal is sent to the group.
        let group = unsafe { libc::getpgrp() } as u32;
        assert!(can_resume(&Launch::Running(group)).is_err());
        assert!(can_resume(&Launch::Ended).is_ok());
    }

    #[test]
    fn legacy_codex_parameters_and_claude_parameters_remain_distinct() {
        let codex = serde_json::json!({"cwd":"/work","executable":"/bin/codex","codex_home":"/home/codex","sandbox":"read-only"});
        let loaded: EngineParams = serde_json::from_value(codex.clone()).unwrap();
        assert!(matches!(&loaded, EngineParams::Codex(_)));
        assert_eq!(serde_json::to_value(loaded).unwrap(), codex);
        let claude = serde_json::json!({"cwd":"/work","executable":"/bin/claude","config_dir":"/home/claude","prompt":"explicit prompt"});
        let loaded: EngineParams = serde_json::from_value(claude.clone()).unwrap();
        assert!(matches!(&loaded, EngineParams::Claude(_)));
        assert_eq!(serde_json::to_value(loaded).unwrap(), claude);
        let mut mixed = claude;
        mixed["codex_home"] = serde_json::json!("/home/codex");
        assert!(serde_json::from_value::<EngineParams>(mixed).is_err());
    }

    fn event(seq: u64, text: &str) -> xaya_agent_proto::SessionEvent {
        xaya_agent_proto::SessionEvent {
            session_id: "claude-events".into(),
            epoch: "epoch-one".into(),
            seq,
            payload: xaya_agent_proto::EventPayload::TextDelta {
                turn_id: "turn".into(),
                item_id: "item".into(),
                text: text.into(),
            },
        }
    }

    #[test]
    fn events_are_stored_in_order_paged_by_size_pruned_and_removed_with_their_session() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.event_head("claude-events").unwrap().is_none());
        let mut total = 0;
        for seq in 1..=20 {
            total += store.append_event(&event(seq, &"x".repeat(1000))).unwrap();
        }
        let head = store.event_head("claude-events").unwrap().unwrap();
        assert_eq!((head.epoch.as_str(), head.seq, head.bytes), ("epoch-one", 20, total));
        // Pages are bounded by size, always make progress and continue after a sequence.
        let page = store.load_events("claude-events", "epoch-one", 0, 4096).unwrap();
        assert!(!page.is_empty() && page.len() < 20, "{}", page.len());
        assert_eq!(
            page.iter().map(|e| e.seq).collect::<Vec<_>>(),
            (1..=page.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(store.load_events("claude-events", "epoch-one", 19, 1).unwrap()[0].seq, 20);
        assert!(store.load_events("claude-events", "other", 0, 4096).unwrap().is_empty());
        // Pruning drops the oldest events of the current stream and every older stream.
        let mut older = event(1, "old stream");
        older.epoch = "epoch-zero".into();
        store.append_event(&older).unwrap();
        let mut newest = event(21, "newest");
        newest.epoch = "epoch-one".into();
        store.append_event(&newest).unwrap();
        let remaining = store.prune_events("claude-events", "epoch-one", 5000).unwrap();
        assert!(remaining <= 5000, "{remaining}");
        let kept = store.load_events("claude-events", "epoch-one", 0, 1 << 20).unwrap();
        assert_eq!(kept.last().unwrap().seq, 21);
        assert!(kept.first().unwrap().seq > 1 && kept.len() < 21);
        assert!(store.load_events("claude-events", "epoch-zero", 0, 1 << 20).unwrap().is_empty());
        assert_eq!(store.event_head("claude-events").unwrap().unwrap().bytes, remaining);
        store.remove(&["claude-events".into()]).unwrap();
        assert!(store.event_head("claude-events").unwrap().is_none());
    }

    #[test]
    fn schema_five_migration_adds_event_history() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.trust_project("/project", "trusted-hash").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "DROP TABLE reviews; DROP TABLE tasks; DROP TABLE events; PRAGMA user_version=5;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.project_trusted("/project", "trusted-hash").unwrap());
        store.append_event(&event(1, "after migration")).unwrap();
        assert_eq!(store.event_head("claude-events").unwrap().unwrap().seq, 1);
    }

    #[test]
    fn schema_six_migration_adds_review_storage() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.trust_project("/project", "trusted-hash").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE reviews; DROP TABLE tasks; PRAGMA user_version=6;")
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.project_trusted("/project", "trusted-hash").unwrap());
        assert!(store.load_reviews().unwrap().is_empty());
    }

    #[test]
    fn schema_seven_migration_adds_task_storage() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.trust_project("/project", "trusted-hash").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE tasks; PRAGMA user_version=7;")
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.project_trusted("/project", "trusted-hash").unwrap());
        assert!(store.load_tasks().unwrap().is_empty());
    }

    #[test]
    fn launches_recorded_before_tool_markers_still_load_without_one() {
        let old = r#"{"Owned":{"host_dir":"/h","marker":"m","host_pid":1,"child_pid":2}}"#;
        let Launch::Owned(owned) = serde_json::from_str(old).unwrap() else { panic!() };
        assert_eq!(owned.tool_marker, None);
        assert!(!owned.stop_requested, "an old record never implies a stop was asked for");
    }

    #[test]
    fn unsupported_schema_is_rejected_instead_of_reset() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.connection.lock().unwrap().pragma_update(None, "user_version", 99).unwrap();
        drop(store);
        assert!(Store::open(&runtime, None).is_err());
    }

    #[test]
    fn schema_four_migration_preserves_trust_and_adds_preview_storage() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store.trust_project("/project", "trusted-hash").unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "DROP TABLE land_previews; DROP TABLE events; DROP TABLE reviews; DROP TABLE \
                 tasks; PRAGMA user_version=4;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.project_trusted("/project", "trusted-hash").unwrap());
        assert!(store.load_previews().unwrap().is_empty());
    }

    #[test]
    fn schema_three_migration_preserves_checks_without_inventing_project_trust() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO checks VALUES('preserved','opaque existing check'); DROP TABLE \
                 project_trust; DROP TABLE land_previews; DROP TABLE events; DROP TABLE reviews; \
                 DROP TABLE tasks; PRAGMA user_version=3;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(!store.project_trusted("/project", "hash").unwrap());
        let value: String = store
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT record FROM checks WHERE id='preserved'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "opaque existing check");
    }

    #[test]
    fn schema_two_migration_preserves_existing_checkpoint_rows() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO checkpoints VALUES('preserved','opaque existing checkpoint'); DROP \
                 TABLE checks; DROP TABLE project_trust; DROP TABLE land_previews; DROP TABLE \
                 events; DROP TABLE reviews; DROP TABLE tasks; PRAGMA user_version=2;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.load_checks().unwrap().is_empty());
        let value: String = store
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT record FROM checkpoints WHERE id='preserved'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "opaque existing checkpoint");
    }

    #[test]
    fn schema_one_migration_preserves_existing_session_rows() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let store = Store::open(&runtime, None).unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch(
                "INSERT INTO sessions VALUES('preserved','opaque existing record'); DROP TABLE \
                 checkpoints; DROP TABLE checks; DROP TABLE project_trust; DROP TABLE \
                 land_previews; DROP TABLE events; DROP TABLE reviews; DROP TABLE tasks; PRAGMA \
                 user_version=1;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&runtime, None).unwrap();
        assert!(store.load_checkpoints().unwrap().is_empty());
        let connection = store.connection.lock().unwrap();
        let value: String = connection
            .query_row("SELECT record FROM sessions WHERE id='preserved'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "opaque existing record");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            11
        );
    }
}
