//! Managed-session panel state and layout. No engine protocol or service runtime is linked here.
use crate::agent_panel::{LineKind, PanelLine, Segment, Tone, fit};
use xaya_agent_proto::{
    ApprovalDecision, EventSnapshot, ManagedRow, Session, SessionState, TaskLandingState,
};

#[cfg(unix)]
mod bridge;
#[cfg(unix)]
pub use bridge::Bridge;

#[derive(Debug, Clone)]
pub enum ManagedAction {
    Inspect(String),
    InspectChecks(String),
    CancelCheck(String),
    Respond {
        session_id: String,
        approval_id: String,
        decision: ApprovalDecision,
    },
    Cancel {
        session_id: String,
        turn_id: String,
    },
    /// End the session's tool processes; the session keeps running.
    EndTools {
        session_id: String,
    },
    /// Load the session's latest review.
    InspectReview(String),
    ProjectBoard(std::path::PathBuf),
    /// Send a needs-fix review back to its session (resumed first when stopped).
    SendReview {
        session_id: String,
        review_id: String,
    },
    /// Hand a request over in the project checkout `cwd`: one task, launched in a workspace of
    /// its own and driven to review. The same request ID returns the same task.
    Delegate {
        cwd: std::path::PathBuf,
        goal: String,
        request_id: String,
        implementer: Implementer,
    },
    /// Ask for changes to the task's reviewed change; the service sends them to its session and
    /// reviews again. The same request ID returns the same request.
    Revise {
        task_id: String,
        request_id: String,
        feedback: String,
    },
    /// Load what accepting the task takes: its change, verification, review and landing.
    InspectTask(String),
    /// Land the task's approved change in the background and accept it. The same request ID
    /// returns the same landing.
    Land {
        task_id: String,
        request_id: String,
    },
}

/// The longest goal a task takes.
const GOAL_LIMIT: usize = xaya_agent_proto::TASK_MAX_GOAL;

/// The engine a delegated task is implemented with; the other one reviews it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Implementer {
    #[default]
    Claude,
    Codex,
}

impl Implementer {
    /// The name `task delegate --engine` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude implements · Codex reviews",
            Self::Codex => "Codex implements · Claude reviews",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Claude => Self::Codex,
            Self::Codex => Self::Claude,
        }
    }
}

/// What a draft is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftFor {
    /// A new task, written in the project view.
    NewTask,
    /// Changes to the task the acceptance view shows.
    Changes(String),
}

/// A request being written in the panel.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub text: String,
    pub purpose: DraftFor,
    /// Kept while the text is unchanged, so submitting again after an uncertain result returns
    /// the task the first attempt may have created instead of making another.
    request_id: String,
    submitted: bool,
    /// For a new task: the project's engine unless switched.
    implementer: Implementer,
    /// The person chose `implementer`; the project's setting no longer applies.
    switched: bool,
}

fn project_engine(board: Option<&xaya_agent_proto::TaskBoard>) -> Implementer {
    board.map_or_else(Implementer::default, |board| match board.engine {
        xaya_agent_proto::ReviewEngine::Claude => Implementer::Claude,
        xaya_agent_proto::ReviewEngine::Codex => Implementer::Codex,
    })
}

fn request_id(kind: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static IDS: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{kind}-{}-{stamp}-{}", std::process::id(), IDS.fetch_add(1, Ordering::Relaxed))
}

/// Which detail the panel shows for the selected session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DetailView {
    #[default]
    Approvals,
    Project,
    Checks,
    Review,
    /// The task chosen on the project board, to accept and land.
    Acceptance,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedUpdate {
    pub rows: Option<Vec<ManagedRow>>,
    pub board: Option<xaya_agent_proto::TaskBoard>,
    pub detail: Option<EventSnapshot>,
    pub verification: Option<(String, Option<xaya_agent_proto::Check>)>,
    /// The latest review of a session, or none.
    pub review: Option<(String, Option<xaya_agent_proto::Review>)>,
    pub dependencies: Option<(String, Vec<xaya_agent_proto::DependencySnapshot>)>,
    /// Why that review's task needs attention, if it does.
    pub task_attention: Option<(String, Option<String>)>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub action_done: bool,
    /// A submitted draft's request ID and what to tell the person, or why it failed.
    pub submitted: Option<(String, Result<String, String>)>,
    pub acceptance: Option<(String, xaya_agent_proto::TaskAcceptance)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManagedPanel {
    rows: Vec<ManagedRow>,
    board: Option<xaya_agent_proto::TaskBoard>,
    selected: Option<String>,
    detail: Option<EventSnapshot>,
    pub preview: bool,
    pub view: DetailView,
    check: Option<xaya_agent_proto::Check>,
    review: Option<xaya_agent_proto::Review>,
    dependencies: Vec<xaya_agent_proto::DependencySnapshot>,
    task_attention: Option<String>,
    approval_index: usize,
    scroll: usize,
    pub pending: bool,
    pub notice: Option<String>,
    error: Option<String>,
    /// The checkout the project view was opened in.
    board_cwd: Option<std::path::PathBuf>,
    pub draft: Option<Draft>,
    /// The draft was submitted; it waits for the answer.
    submitting: bool,
    /// The task chosen on the project board; followed by identity as the board changes.
    board_task: Option<String>,
    /// The task the acceptance view shows, and what was loaded for it.
    task: Option<String>,
    acceptance: Option<xaya_agent_proto::TaskAcceptance>,
    /// The last landing asked for (task, request ID): asked again while its answer is unknown.
    land_request: Option<(String, String)>,
}

fn dependency_reason(state: &xaya_agent_proto::DependencyState) -> &'static str {
    use xaya_agent_proto::DependencyState::{
        Abandoned, AwaitingCheckout, AwaitingLanding, CheckoutBehind, CheckoutUnknown, Included,
        MultipleLandingTargets,
    };
    match state {
        AwaitingLanding => "waiting for reviewed changes to land",
        Abandoned => "prerequisite abandoned",
        MultipleLandingTargets => "multiple landed commits; target unclear",
        AwaitingCheckout { .. } => "landed; awaiting checkout",
        CheckoutBehind { .. } => "checkout does not contain landed commit",
        CheckoutUnknown { .. } => "could not verify checkout",
        Included { .. } => "landed commit included",
    }
}

/// Board order: what needs a person first, then what can be accepted.
fn board_rank(state: xaya_agent_proto::BoardState) -> u8 {
    use xaya_agent_proto::BoardState::{
        ActiveSession, AwaitingAcceptance, Eligible, Landing, Manual, NeedsAttention,
        WaitingCapacity, WaitingDependency,
    };
    match state {
        NeedsAttention => 0,
        AwaitingAcceptance => 1,
        Landing => 2,
        ActiveSession => 3,
        WaitingDependency => 4,
        WaitingCapacity => 5,
        Eligible => 6,
        Manual => 7,
    }
}

fn rank(row: &ManagedRow) -> u8 {
    match row.session.state {
        SessionState::Blocked => 0,
        SessionState::Interrupted | SessionState::Failed => 1,
        SessionState::Working | SessionState::Starting => 2,
        SessionState::Ready => 3,
        SessionState::Stopped => 4,
    }
}

fn line(text: &str, tone: Tone, columns: usize, selected: bool) -> PanelLine {
    PanelLine {
        kind: LineKind::Window { selected },
        segments: vec![Segment { column: 0, text: fit(text, columns), tone }],
    }
}

impl ManagedPanel {
    pub fn selected(&self) -> Option<&Session> {
        self.rows
            .iter()
            .find(|row| Some(&row.session.id) == self.selected.as_ref())
            .map(|row| &row.session)
    }

    /// What the open detail view shows and keeps current: a session, or the accepted task.
    pub fn inspect_id(&self) -> Option<String> {
        match self.view {
            _ if !self.preview => None,
            DetailView::Project => None,
            // Each load runs Git; poll only while the page can change.
            DetailView::Acceptance => self.task.clone().filter(|_| {
                self.acceptance.as_ref().is_none_or(|acceptance| {
                    let task = &acceptance.task;
                    task.landing.as_ref().is_some_and(|l| match l.state {
                        TaskLandingState::Landing => true,
                        // Until the service says what became of the workspace.
                        TaskLandingState::Landed => l.workspace.as_ref().is_none_or(|w| {
                            w.outcome == xaya_agent_proto::TaskWorkspaceOutcome::Removing
                        }),
                        TaskLandingState::Failed => false,
                    }) || task.drive.as_ref().is_some_and(|drive| drive.state.is_active())
                })
            }),
            DetailView::Approvals | DetailView::Checks | DetailView::Review => {
                self.selected.clone()
            },
        }
    }

    fn board_rows(&self) -> &[xaya_agent_proto::TaskBoardRow] {
        self.board.as_ref().map_or(&[], |board| &board.tasks)
    }

    fn board_index(&self) -> usize {
        let rows = self.board_rows();
        rows.iter().position(|row| Some(&row.task_id) == self.board_task.as_ref()).unwrap_or(0)
    }

    /// Open the task chosen on the project board.
    pub fn open_task(&mut self) -> Option<ManagedAction> {
        if !(self.preview && self.view == DetailView::Project) || self.draft.is_some() {
            return None;
        }
        let id = self.board_rows().get(self.board_index())?.task_id.clone();
        self.board_task = Some(id.clone());
        self.view = DetailView::Acceptance;
        self.task = Some(id.clone());
        self.acceptance = None;
        self.scroll = 0;
        self.pending = true;
        self.error = None;
        self.notice = None;
        Some(ManagedAction::InspectTask(id))
    }

    /// Land the loaded task when nothing stands in the way.
    pub fn land(&mut self) -> Option<ManagedAction> {
        if !self.preview || self.view != DetailView::Acceptance || self.pending {
            return None;
        }
        let acceptance =
            self.acceptance.as_ref().filter(|acceptance| acceptance.blocked.is_none())?;
        let task_id = acceptance.task.id.clone();
        // Once the service has the request (and it failed), landing again is a new request.
        let recorded = acceptance.task.landing.as_ref().map(|landing| &landing.request_id);
        let request_id = match &self.land_request {
            Some((task, id)) if *task == task_id && Some(id) != recorded => id.clone(),
            _ => request_id("land"),
        };
        self.land_request = Some((task_id.clone(), request_id.clone()));
        self.pending = true;
        self.error = None;
        self.notice = None;
        Some(ManagedAction::Land { task_id, request_id })
    }

    /// Start writing a request for the project the view shows.
    pub fn start_draft(&mut self) -> bool {
        if !(self.preview && self.view == DetailView::Project) || self.board_cwd.is_none() {
            return false;
        }
        if self.draft.is_none() {
            self.draft = Some(Draft {
                text: String::new(),
                purpose: DraftFor::NewTask,
                request_id: request_id("ask"),
                submitted: false,
                implementer: project_engine(self.board.as_ref()),
                switched: false,
            });
        }
        self.error = None;
        true
    }

    /// Whether the loaded task takes requested changes: the service drove it to a completed
    /// review, is not driving it now, and it is not landing or landed.
    fn revisable(&self) -> Option<&str> {
        let acceptance = self.acceptance.as_ref()?;
        let task = &acceptance.task;
        let driven = task
            .drive
            .as_ref()
            .is_some_and(|drive| !drive.state.is_active() && drive.session_id.is_some());
        let landing = task.landing.as_ref().is_some_and(|l| l.state != TaskLandingState::Failed);
        let reviewed = acceptance
            .review
            .as_ref()
            .is_some_and(|review| review.status == xaya_agent_proto::ReviewStatus::Completed);
        (task.status == xaya_agent_proto::TaskStatus::Open
            && task.attention.is_none()
            && driven
            && !landing
            && reviewed)
            .then_some(task.id.as_str())
    }

    /// Start writing the changes to ask of the task the acceptance view shows.
    pub fn start_changes(&mut self) -> bool {
        if !self.preview || self.view != DetailView::Acceptance || self.pending {
            return false;
        }
        let Some(task) = self.revisable().map(str::to_owned) else { return false };
        if self.draft.as_ref().is_none_or(|draft| draft.purpose != DraftFor::Changes(task.clone()))
        {
            self.draft = Some(Draft {
                text: String::new(),
                purpose: DraftFor::Changes(task),
                request_id: request_id("change"),
                submitted: false,
                implementer: Implementer::default(),
                switched: false,
            });
        }
        self.error = None;
        self.notice = None;
        true
    }

    /// Whether the draft belongs to the view on show, which then takes every key.
    pub fn drafting(&self) -> bool {
        self.preview
            && match self.draft.as_ref().map(|draft| &draft.purpose) {
                Some(DraftFor::NewTask) => self.view == DetailView::Project,
                Some(DraftFor::Changes(task)) => {
                    self.view == DetailView::Acceptance && self.task.as_ref() == Some(task)
                },
                None => false,
            }
    }

    /// Edit the draft; a request edited after it was submitted is a new request.
    pub fn edit_draft(&mut self, edit: impl FnOnce(&mut String)) {
        if self.submitting {
            return;
        }
        let Some(draft) = self.draft.as_mut() else { return };
        let before = draft.text.clone();
        edit(&mut draft.text);
        draft.text.retain(|c| !c.is_control());
        match draft.purpose {
            DraftFor::NewTask if draft.text.chars().count() > GOAL_LIMIT => {
                draft.text = draft.text.chars().take(GOAL_LIMIT).collect();
            },
            DraftFor::Changes(_) => {
                while draft.text.len() > xaya_agent_proto::TASK_MAX_FEEDBACK_BYTES {
                    draft.text.pop();
                }
            },
            DraftFor::NewTask => (),
        }
        if draft.submitted && draft.text != before {
            let kind = if draft.purpose == DraftFor::NewTask { "ask" } else { "change" };
            draft.request_id = request_id(kind);
            draft.submitted = false;
        }
    }

    /// Hand this new task to the other engine. A request already sent with the first one is a
    /// new request with this one.
    pub fn switch_implementer(&mut self) -> bool {
        let new_task = self.draft.as_ref().is_some_and(|draft| draft.purpose == DraftFor::NewTask);
        if self.submitting || !self.drafting() || !new_task {
            return false;
        }
        let Some(draft) = self.draft.as_mut() else { return false };
        draft.implementer = draft.implementer.other();
        draft.switched = true;
        if draft.submitted {
            draft.request_id = request_id("ask");
            draft.submitted = false;
        }
        true
    }

    pub fn cancel_draft(&mut self) {
        if !self.submitting {
            self.draft = None;
        }
    }

    pub fn submit_draft(&mut self) -> Option<ManagedAction> {
        if self.submitting {
            return None;
        }
        if !self.drafting() {
            return None;
        }
        let cwd = self.board_cwd.clone();
        let draft = self.draft.as_mut().filter(|draft| !draft.text.trim().is_empty())?;
        let (text, request_id) = (draft.text.trim().to_owned(), draft.request_id.clone());
        let action = match &draft.purpose {
            DraftFor::NewTask => ManagedAction::Delegate {
                cwd: cwd?,
                goal: text,
                request_id,
                implementer: draft.implementer,
            },
            DraftFor::Changes(task) => {
                ManagedAction::Revise { task_id: task.clone(), request_id, feedback: text }
            },
        };
        draft.submitted = true;
        self.submitting = true;
        self.error = None;
        self.notice = None;
        Some(action)
    }

    pub fn open_board(&mut self, cwd: Option<std::path::PathBuf>) {
        let changes = self.draft.as_ref().is_some_and(|draft| draft.purpose != DraftFor::NewTask);
        if (cwd != self.board_cwd || changes) && !self.submitting {
            self.draft = None;
        }
        self.board_cwd = cwd;
        self.view = DetailView::Project;
        self.preview = true;
        self.pending = true;
        self.scroll = 0;
        self.notice = None;
        self.error = None;
        self.board = None;
    }

    pub fn apply(&mut self, update: ManagedUpdate) {
        // A delegation that could not even be submitted answers with an error alone.
        let refused = update.action_done && update.error.is_some();
        if let Some(mut board) = update.board.filter(|_| self.view == DetailView::Project) {
            board.tasks.sort_by_key(|row| board_rank(row.state));
            if !board.tasks.iter().any(|row| Some(&row.task_id) == self.board_task.as_ref()) {
                self.board_task = board.tasks.first().map(|row| row.task_id.clone());
            }
            self.board = Some(board);
            // A draft begun before the board arrived takes the project's engine, until sent.
            if let Some(draft) = self.draft.as_mut().filter(|draft| {
                draft.purpose == DraftFor::NewTask && !draft.switched && !draft.submitted
            }) {
                draft.implementer = project_engine(self.board.as_ref());
            }
        }
        if let Some((id, acceptance)) = update.acceptance
            && self.view == DetailView::Acceptance
            && self.task.as_ref() == Some(&id)
            && acceptance.task.id == id
        {
            self.acceptance = Some(acceptance);
        }
        if let Some(mut rows) = update.rows {
            rows.sort_by(|a, b| {
                rank(a).cmp(&rank(b)).then_with(|| a.session.id.cmp(&b.session.id))
            });
            if !rows.iter().any(|row| Some(&row.session.id) == self.selected.as_ref()) {
                self.selected = rows.first().map(|row| row.session.id.clone());
                if self.view != DetailView::Project {
                    self.preview = false;
                }
                self.detail = None;
            }
            self.rows = rows;
            self.error = None;
        }
        if let Some(error) = update.error {
            match self.view {
                DetailView::Project => self.board = None,
                DetailView::Checks => self.check = None,
                DetailView::Review => self.review = None,
                DetailView::Approvals | DetailView::Acceptance => (),
            }
            self.error = Some(error);
        }
        if let Some(notice) = update.notice {
            self.notice = Some(notice);
        }
        if update.action_done {
            self.pending = false;
        }
        if update.submitted.is_some() || refused {
            self.submitting = false;
        }
        if let Some((request, result)) = update.submitted
            && let Some(draft) = self.draft.as_ref().filter(|draft| draft.request_id == request)
        {
            match result {
                Ok(notice) => {
                    self.draft = None;
                    self.notice = Some(notice);
                },
                Err(error) if draft.purpose == DraftFor::NewTask => {
                    self.error = Some(format!(" Not handed over: {error}"))
                },
                Err(error) => self.error = Some(format!(" Not sent: {error}")),
            }
        }
        if let Some((session_id, check)) = update.verification
            && Some(&session_id) == self.selected.as_ref()
            && check.as_ref().is_none_or(|check| check.session_id == session_id)
        {
            self.check = check;
        }
        if let Some((session_id, review)) = update.review
            && Some(&session_id) == self.selected.as_ref()
            && review.as_ref().is_none_or(|review| review.session_id == session_id)
        {
            self.review = review;
        }
        if let Some((id, dependencies)) = update.dependencies
            && Some(&id) == self.selected.as_ref()
        {
            self.dependencies = dependencies;
        }
        if let Some((session_id, attention)) = update.task_attention
            && Some(&session_id) == self.selected.as_ref()
        {
            self.task_attention = attention;
        }
        if let Some(detail) = update.detail
            && Some(&detail.session.id) == self.selected.as_ref()
        {
            let old_id = self
                .detail
                .as_ref()
                .and_then(|detail| detail.approvals.get(self.approval_index))
                .map(|approval| approval.id.clone());
            self.approval_index = old_id
                .as_ref()
                .and_then(|id| detail.approvals.iter().position(|approval| &approval.id == id))
                .unwrap_or(0);
            self.detail = Some(detail);
        }
    }

    pub fn select_index(&mut self, index: usize) -> bool {
        let Some(row) = self.rows.get(index) else { return false };
        self.selected = Some(row.session.id.clone());
        self.preview = false;
        self.view = DetailView::Approvals;
        self.check = None;
        self.review = None;
        self.dependencies.clear();
        self.task_attention = None;
        self.detail = None;
        self.scroll = 0;
        true
    }

    pub fn select_relative(&mut self, delta: isize) {
        if self.preview && self.view == DetailView::Project {
            let rows = self.board_rows();
            let last = rows.len().saturating_sub(1);
            let index = self.board_index().saturating_add_signed(delta).min(last);
            self.board_task = rows.get(index).map(|row| row.task_id.clone());
        } else if self.preview {
            self.scroll = self.scroll.saturating_add_signed(delta);
        } else if !self.rows.is_empty() {
            let current = self
                .rows
                .iter()
                .position(|row| Some(&row.session.id) == self.selected.as_ref())
                .unwrap_or(0);
            self.select_index(
                (current as isize + delta).rem_euclid(self.rows.len() as isize) as usize
            );
        }
    }

    pub fn cycle_approval(&mut self, delta: isize) {
        if let Some(detail) = &self.detail
            && !detail.approvals.is_empty()
        {
            self.approval_index = (self.approval_index as isize + delta)
                .rem_euclid(detail.approvals.len() as isize)
                as usize;
            self.scroll = 0;
        }
    }

    pub fn inspect(&mut self) -> Option<ManagedAction> {
        let id = self.selected()?.id.clone();
        if self.pending {
            return None;
        }
        self.preview = true;
        self.view = DetailView::Approvals;
        self.scroll = 0;
        self.pending = true;
        Some(ManagedAction::Inspect(id))
    }

    pub fn inspect_checks(&mut self) -> Option<ManagedAction> {
        let id = self.selected()?.id.clone();
        if self.pending {
            return None;
        }
        self.preview = true;
        self.view = DetailView::Checks;
        self.check = None;
        self.scroll = 0;
        self.pending = true;
        Some(ManagedAction::InspectChecks(id))
    }

    pub fn inspect_review(&mut self) -> Option<ManagedAction> {
        let id = self.selected()?.id.clone();
        if self.pending {
            return None;
        }
        self.preview = true;
        self.view = DetailView::Review;
        self.review = None;
        self.dependencies.clear();
        self.task_attention = None;
        self.scroll = 0;
        self.pending = true;
        Some(ManagedAction::InspectReview(id))
    }

    /// The loaded review, if it can be sent back: completed, needs-fix, fresh, not yet sent, and
    /// of the selected session.
    fn sendable(&self) -> Option<&xaya_agent_proto::Review> {
        if !self.preview || self.view != DetailView::Review || self.task_attention.is_some() {
            return None;
        }
        let review = self.review.as_ref()?;
        (Some(&review.session_id) == self.selected.as_ref()
            && review.status == xaya_agent_proto::ReviewStatus::Completed
            && review.verdict == Some(xaya_agent_proto::ReviewVerdict::NeedsFix)
            && !review.stale
            && review.sent.is_none())
        .then_some(review)
    }

    pub fn send_review(&mut self) -> Option<ManagedAction> {
        if self.pending {
            return None;
        }
        let review = self.sendable()?;
        let action = ManagedAction::SendReview {
            session_id: review.session_id.clone(),
            review_id: review.id.clone(),
        };
        self.pending = true;
        Some(action)
    }

    pub fn cancel_check(&mut self) -> Option<ManagedAction> {
        if !self.preview || self.view != DetailView::Checks || self.pending {
            return None;
        }
        let check = self.check.as_ref()?;
        if check.status != xaya_agent_proto::CheckStatus::Running {
            return None;
        }
        self.pending = true;
        Some(ManagedAction::CancelCheck(check.id.clone()))
    }

    pub fn decide(&mut self, decision: ApprovalDecision) -> Option<ManagedAction> {
        if !self.preview || self.view != DetailView::Approvals || self.pending {
            return None;
        }
        let detail = self.detail.as_ref()?;
        let approval = detail.approvals.get(self.approval_index)?;
        if !approval.controllable {
            return None;
        }
        if !approval.decisions.contains(&decision) {
            return None;
        }
        self.pending = true;
        Some(ManagedAction::Respond {
            session_id: detail.session.id.clone(),
            approval_id: approval.id.clone(),
            decision,
        })
    }

    pub fn cancel(&mut self) -> Option<ManagedAction> {
        if self.pending {
            return None;
        }
        let session = self.selected()?;
        let action = ManagedAction::Cancel {
            session_id: session.id.clone(),
            turn_id: session.turn_id.clone()?,
        };
        self.pending = true;
        Some(action)
    }

    /// Only for a live session between turns: the service refuses while a turn runs.
    pub fn end_tools(&mut self) -> Option<ManagedAction> {
        if self.pending || self.preview {
            return None;
        }
        let session = self.selected()?;
        if !session.state.is_active() || session.turn_id.is_some() {
            return None;
        }
        let action = ManagedAction::EndTools { session_id: session.id.clone() };
        self.pending = true;
        Some(action)
    }

    pub fn lines(&self, columns: usize, max_lines: usize) -> Vec<PanelLine> {
        if columns == 0 || max_lines == 0 {
            return Vec::new();
        }
        let drafting = self.drafting();
        let changes = self.draft.as_ref().is_some_and(|draft| draft.purpose != DraftFor::NewTask);
        let header = if drafting && changes {
            String::from(" Request changes · ⏎ send · esc cancel")
        } else if drafting {
            String::from(" New task · ⏎ hand over · tab switch engine · esc cancel")
        } else if self.preview && self.view == DetailView::Project {
            String::from(" Project tasks · ↑↓ select · ⏎ open · n new task · p refresh · esc back")
        } else if self.preview && self.view == DetailView::Acceptance {
            let land = self
                .acceptance
                .as_ref()
                .filter(|acceptance| acceptance.blocked.is_none())
                .and_then(|acceptance| acceptance.target_branch.as_ref())
                .map(|target| format!(" · l land on {target}"))
                .unwrap_or_default();
            let change = if self.revisable().is_some() { " · c request changes" } else { "" };
            format!(" Accept task{land}{change} · ↑↓ scroll · esc back")
        } else if self.preview && self.view == DetailView::Checks {
            String::from(" Verification · ↑↓ scroll · c cancel running check · esc back")
        } else if self.preview && self.view == DetailView::Review {
            let send = if self.sendable().is_some() { " · s send fixes back" } else { "" };
            format!(" Review · ↑↓ scroll{send} · esc back")
        } else if self.preview {
            let mut header = String::from(" Approval · ↑↓ scroll · [ ] request");
            if let Some(approval) = self
                .detail
                .as_ref()
                .and_then(|detail| detail.approvals.get(self.approval_index))
                .filter(|approval| approval.controllable)
            {
                for (decision, label) in [
                    (ApprovalDecision::Accept, " · y allow"),
                    (ApprovalDecision::Decline, " · n decline"),
                    (ApprovalDecision::Cancel, " · x cancel"),
                ] {
                    if approval.decisions.contains(&decision) {
                        header.push_str(label);
                    }
                }
            }
            header.push_str(" · esc back");
            header
        } else if self.selected().is_some_and(|session| session.engine == "claude") {
            String::from(
                " Claude · p project · m windows · ↑↓ select · Enter attach · a approvals · c \
                 interrupt · t end tools · v checks · r review",
            )
        } else {
            String::from(
                " Managed · p project · m windows · ↑↓ select · Enter attach/resume · a approvals \
                 · v checks · r review · c interrupt · t end tools",
            )
        };
        let mut lines = vec![PanelLine {
            kind: LineKind::Header,
            segments: vec![Segment { column: 0, text: fit(&header, columns), tone: Tone::Plain }],
        }];
        if let Some(error) = &self.error {
            lines.push(line(error, Tone::Error, columns, false));
        }
        if let Some(notice) = &self.notice {
            lines.push(line(notice, Tone::Dim, columns, false));
        }
        if self.pending {
            lines.push(line(" Waiting for service…", Tone::Dim, columns, false));
        }
        if !self.preview
            && let Some(error) = self.selected().and_then(|session| session.last_error.as_ref())
        {
            lines.push(line(error, Tone::Error, columns, false));
        }
        if let Some(draft) = self.draft.as_ref().filter(|_| drafting) {
            let prompt = if changes { " What should change?" } else { " What do you want done?" };
            lines.push(line(prompt, Tone::Plain, columns, false));
            let cursor = if self.submitting { "" } else { "▏" };
            let text = format!("{}{cursor}", draft.text);
            // Show the end of a long request, where the typing is.
            let wrapped = wrap(&text, columns.saturating_sub(3).max(1));
            let room = (max_lines.saturating_sub(lines.len()) / 2).max(1);
            for (index, part) in wrapped.iter().enumerate().skip(wrapped.len().saturating_sub(room))
            {
                let prefix = if index == 0 { " › " } else { "   " };
                lines.push(line(&format!("{prefix}{part}"), Tone::Working, columns, true));
            }
            if !changes {
                lines.push(line(
                    &format!(" {}", draft.implementer.label()),
                    Tone::Dim,
                    columns,
                    false,
                ));
            }
            if self.submitting {
                let doing = if changes { " Sending…" } else { " Handing over…" };
                lines.push(line(doing, Tone::Dim, columns, false));
            }
        }
        if self.preview && self.view == DetailView::Project {
            if let Some(board) = &self.board {
                lines.push(line(
                    &format!(
                        " {} free / {} slots · {} occupied · {}",
                        board.available, board.capacity, board.occupied, board.project
                    ),
                    Tone::Plain,
                    columns,
                    false,
                ));
                if board.tasks.is_empty() {
                    lines.push(line(" No open tasks for this project.", Tone::Dim, columns, false));
                }
                let budget = max_lines.saturating_sub(lines.len()).max(1);
                let chosen = self.board_index();
                let start = chosen.saturating_sub(budget - 1);
                for (index, row) in board.tasks.iter().enumerate().skip(start).take(budget) {
                    use xaya_agent_proto::BoardState::{
                        ActiveSession, AwaitingAcceptance, Eligible, Landing, Manual,
                        NeedsAttention, WaitingCapacity, WaitingDependency,
                    };
                    let state = match row.state {
                        Manual => "manual",
                        NeedsAttention => "needs attention",
                        Landing => "landing",
                        AwaitingAcceptance => "ready to accept",
                        ActiveSession => "active session",
                        WaitingDependency => "waiting for dependency",
                        WaitingCapacity => "waiting for capacity",
                        Eligible => "eligible",
                    };
                    let landing = row.landing.as_ref().and_then(|landing| match landing.state {
                        TaskLandingState::Landing => landing.reason.clone(),
                        TaskLandingState::Failed => Some(format!(
                            "landing failed: {}",
                            landing.reason.as_deref().unwrap_or("unknown")
                        )),
                        TaskLandingState::Landed => None,
                    });
                    let reason = row
                        .attention
                        .as_ref()
                        .map(|a| a.reason.clone())
                        .or(landing)
                        .or_else(|| {
                            row.dependencies
                                .iter()
                                .find(|d| {
                                    !matches!(
                                        d.state,
                                        xaya_agent_proto::DependencyState::Included { .. }
                                    )
                                })
                                .map(|d| format!("{}: {}", d.title, dependency_reason(&d.state)))
                        });
                    let suffix = reason.map(|r| format!(" · {r}")).unwrap_or_default();
                    let tone = match row.state {
                        NeedsAttention => Tone::Blocked,
                        AwaitingAcceptance => Tone::Done,
                        Landing => Tone::Working,
                        _ => Tone::Plain,
                    };
                    lines.push(line(
                        &format!(" {} · {state}{suffix}", row.title),
                        tone,
                        columns,
                        index == chosen,
                    ));
                }
            }
            lines.truncate(max_lines);
            return lines;
        }
        if self.preview && self.view == DetailView::Acceptance {
            let text = self.acceptance.as_ref().map_or_else(
                || vec![(" Loading the task…".to_owned(), Tone::Dim)],
                acceptance_text,
            );
            let wrapped: Vec<(String, Tone)> = text
                .iter()
                .flat_map(|(text, tone)| wrap(text, columns).into_iter().map(|part| (part, *tone)))
                .collect();
            let budget = max_lines.saturating_sub(lines.len());
            let first = self.scroll.min(wrapped.len().saturating_sub(budget.max(1)));
            lines.extend(
                wrapped
                    .iter()
                    .skip(first)
                    .take(budget)
                    .map(|(text, tone)| line(text, *tone, columns, true)),
            );
            lines.truncate(max_lines);
            return lines;
        }
        if self.preview && self.view == DetailView::Review {
            let mut text = self.review.as_ref().map_or_else(
                || vec![" No review recorded for this session.".to_owned()],
                review_text,
            );
            for dependency in self.dependencies.iter().rev() {
                let reason = dependency_reason(&dependency.state);
                text.insert(0, format!(" {}: {reason}", dependency.title));
            }
            if let Some(reason) = &self.task_attention {
                text.insert(0, format!(" Task needs attention: {reason} (xaya task continue)"));
            }
            let wrapped = wrap(&text.join("\n"), columns);
            let budget = max_lines.saturating_sub(lines.len());
            let first = self.scroll.min(wrapped.len().saturating_sub(budget.max(1)));
            lines.extend(
                wrapped
                    .iter()
                    .skip(first)
                    .take(budget)
                    .map(|text| line(text, Tone::Plain, columns, true)),
            );
        } else if self.preview && self.view == DetailView::Checks {
            let mut text = Vec::new();
            if let Some(check) = &self.check {
                text.push(format!(
                    " {} · {:?} · {}",
                    check.id,
                    check.status,
                    if check.stale { "stale / not yet verified" } else { "current" }
                ));
                text.push(format!(" checkpoint {}", check.checkpoint_id));
                for step in &check.steps {
                    text.push(format!(
                        " {} · {} · exit {} · {}ms",
                        step.step.name,
                        step.status
                            .map(|status| format!("{status:?}"))
                            .unwrap_or_else(|| "pending".into()),
                        step.exit_code.map(|code| code.to_string()).unwrap_or_else(|| "—".into()),
                        step.duration_ms.map(|ms| ms.to_string()).unwrap_or_else(|| "—".into())
                    ));
                }
                if let Some(error) = &check.error {
                    text.push(error.clone());
                }
                if check.log_truncated {
                    text.push(" Log truncated. Read with xaya check log.".into());
                }
            } else {
                text.push(" No verification recorded for this session.".into());
            }
            let wrapped = wrap(&text.join("\n"), columns);
            let budget = max_lines.saturating_sub(lines.len());
            let first = self.scroll.min(wrapped.len().saturating_sub(budget.max(1)));
            lines.extend(
                wrapped
                    .iter()
                    .skip(first)
                    .take(budget)
                    .map(|text| line(text, Tone::Plain, columns, true)),
            );
        } else if self.preview {
            if let Some(approval) =
                self.detail.as_ref().and_then(|detail| detail.approvals.get(self.approval_index))
            {
                let title = if approval.kind == xaya_agent_proto::ApprovalKind::FileChange {
                    " File change approval"
                } else {
                    " Command approval"
                };
                lines.push(line(title, Tone::Blocked, columns, true));
                let text = serde_json::to_string_pretty(&approval.details).unwrap_or_default();
                let wrapped = wrap(&text, columns);
                let remaining = max_lines.saturating_sub(lines.len());
                let first = self.scroll.min(wrapped.len().saturating_sub(remaining.max(1)));
                lines.extend(
                    wrapped
                        .iter()
                        .skip(first)
                        .take(remaining)
                        .map(|text| line(text, Tone::Plain, columns, true)),
                );
            } else {
                lines.push(line(
                    " No pending approval. Press esc to return.",
                    Tone::Dim,
                    columns,
                    false,
                ));
            }
        } else {
            let selected = self
                .rows
                .iter()
                .position(|row| Some(&row.session.id) == self.selected.as_ref())
                .unwrap_or(0);
            let budget = max_lines.saturating_sub(lines.len());
            let first = selected.saturating_sub(budget.saturating_sub(1));
            for (index, row) in self.rows.iter().enumerate().skip(first).take(budget) {
                let state = match row.session.state {
                    SessionState::Starting => "starting",
                    SessionState::Ready => "ready",
                    SessionState::Working => "working",
                    SessionState::Blocked => "waiting",
                    SessionState::Failed => "failed",
                    SessionState::Stopped => "stopped",
                    SessionState::Interrupted => "interrupted",
                };
                let text = format!(
                    " {} {:12} {} · {} approvals · {} · {}",
                    index + 1,
                    state,
                    row.session.id,
                    row.approval_count,
                    if row.session.attached { "attached" } else { "detached" },
                    row.session.cwd
                );
                lines.push(line(
                    &text,
                    if row.session.state == SessionState::Blocked {
                        Tone::Blocked
                    } else {
                        Tone::Plain
                    },
                    columns,
                    Some(&row.session.id) == self.selected.as_ref(),
                ));
            }
            if self.rows.is_empty() {
                lines.push(line(
                    " No managed sessions. Start one with xaya run.",
                    Tone::Dim,
                    columns,
                    false,
                ));
            }
        }
        lines.truncate(max_lines);
        lines
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut result = Vec::new();
    for source in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for ch in source.chars() {
            let size = ch.width().unwrap_or(0);
            if used + size > width && !line.is_empty() {
                result.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += size;
        }
        result.push(line);
    }
    result
}

/// What became of a landed task's workspace, when there is something to say.
fn workspace_line(landing: &xaya_agent_proto::TaskLandingRecord) -> Option<(String, Tone)> {
    use xaya_agent_proto::TaskWorkspaceOutcome as Outcome;
    let cleanup = landing.workspace.as_ref()?;
    let path = cleanup.path.as_deref().unwrap_or("its workspace");
    match cleanup.outcome {
        Outcome::Removing => Some((format!(" Removing the workspace {path}"), Tone::Working)),
        Outcome::Removed => Some((format!(" Workspace removed: {path}"), Tone::Dim)),
        Outcome::Kept => Some((
            format!(
                " Workspace kept at {path}: {}",
                cleanup.reason.as_deref().unwrap_or("unknown reason")
            ),
            Tone::Blocked,
        )),
        Outcome::Absent => None,
    }
}

/// The acceptance page: where it stands, verification, the files, the review, then the diff.
fn acceptance_text(acceptance: &xaya_agent_proto::TaskAcceptance) -> Vec<(String, Tone)> {
    let task = &acceptance.task;
    let target = acceptance.target_branch.as_deref().unwrap_or("its target");
    let mut text = vec![(format!(" {} · {}", task.title, task.id), Tone::Plain)];
    match task.landing.as_ref().map(|landing| (landing.state, landing.reason.as_deref())) {
        Some((TaskLandingState::Landing, reason)) => text.push((
            format!(" Landing on {target}: {}", reason.unwrap_or("starting")),
            Tone::Working,
        )),
        Some((TaskLandingState::Landed, _)) => {
            text.push((format!(" Landed on {target} · accepted"), Tone::Done));
            if let Some(line) = task.landing.as_ref().and_then(workspace_line) {
                text.push(line);
            }
        },
        Some((TaskLandingState::Failed, reason)) => text.push((
            format!(" The last landing failed: {}", reason.unwrap_or("unknown")),
            Tone::Error,
        )),
        None => (),
    }
    match &acceptance.blocked {
        None => text.push((format!(" Ready to land on {target} · press l"), Tone::Done)),
        Some(_) if task.landing.as_ref().is_some_and(|l| l.state != TaskLandingState::Failed) => {},
        Some(reason) => text.push((format!(" Cannot land yet: {reason}"), Tone::Blocked)),
    }
    text.push((
        task.goal.lines().next().map(|goal| format!(" Goal: {goal}")).unwrap_or_default(),
        Tone::Dim,
    ));
    if let Some(revision) = task.revisions.last() {
        let first = revision.feedback.lines().next().unwrap_or_default();
        let under_way = task.drive.as_ref().is_some_and(|drive| drive.state.is_active());
        let (label, tone) =
            if under_way { ("Changes under way", Tone::Working) } else { ("You asked", Tone::Dim) };
        text.push((format!(" {label}: {first}"), tone));
    }
    if let Some(check) = &acceptance.check {
        text.push((
            format!(
                " Verification {:?} · {} step{}{}",
                check.status,
                check.steps.len(),
                if check.steps.len() == 1 { "" } else { "s" },
                if check.stale { " · stale" } else { "" }
            )
            .to_lowercase(),
            Tone::Plain,
        ));
    }
    if !acceptance.files.is_empty() {
        let count = |pick: fn(&xaya_agent_proto::ChangedFile) -> Option<u64>| {
            acceptance.files.iter().filter_map(pick).sum::<u64>()
        };
        text.push((
            format!(
                " {} file{} changed · +{} −{}",
                acceptance.files.len(),
                if acceptance.files.len() == 1 { "" } else { "s" },
                count(|file| file.added),
                count(|file| file.removed)
            ),
            Tone::Plain,
        ));
        for file in &acceptance.files {
            let lines = match (file.added, file.removed) {
                (Some(added), Some(removed)) => format!("+{added} −{removed}"),
                _ => "binary".into(),
            };
            text.push((format!("   {lines} {}", file.path), Tone::Dim));
        }
    }
    match &acceptance.review {
        Some(review) => {
            text.extend(review_text(review).into_iter().map(|line| (line, Tone::Plain)))
        },
        None => text.push((" Not reviewed yet.".into(), Tone::Dim)),
    }
    if !acceptance.diff.is_empty() {
        text.push((String::new(), Tone::Plain));
        text.extend(acceptance.diff.lines().map(|line| {
            let tone = match line.as_bytes().first() {
                Some(b'+') if !line.starts_with("+++") => Tone::Done,
                Some(b'-') if !line.starts_with("---") => Tone::Error,
                Some(b'@') => Tone::Working,
                _ => Tone::Plain,
            };
            (format!(" {line}"), tone)
        }));
        if acceptance.diff_truncated {
            text.push((" Diff cut at 256 KiB; the file list above is complete.".into(), Tone::Dim));
        }
    }
    text
}

/// The review detail: what was decided and why, criteria, then findings (blocking first).
fn review_text(review: &xaya_agent_proto::Review) -> Vec<String> {
    let verdict = review.verdict.map_or_else(|| "—".to_owned(), |v| format!("{v:?}"));
    let engines = review.second_reviewer.as_ref().map_or_else(
        || review.engine.as_str().into(),
        |second| format!("{} + {}", review.engine.as_str(), second.engine.as_str()),
    );
    let mut text = vec![format!(
        " {} · {:?} · {verdict} · {engines}{}{}",
        review.id,
        review.status,
        if review.same_engine && review.second_reviewer.is_none() { " (same engine)" } else { "" },
        if review.stale { " · stale" } else { "" },
    )];
    text.push(format!(" checkpoint {}", review.checkpoint_id));
    text.extend(review.reasons.iter().map(|reason| format!(" {reason}")));
    text.extend(review.criteria.iter().map(|c| {
        format!(" {} {} — {}", c.id, format!("{:?}", c.status).to_lowercase(), c.evidence)
    }));
    let mut findings: Vec<&xaya_agent_proto::ReviewFinding> = review.findings.iter().collect();
    findings.sort_by_key(|f| !f.blocking);
    text.extend(findings.iter().map(|f| {
        format!(
            " {} — {}:{}-{}{}",
            f.title,
            f.path,
            f.start,
            f.end,
            if f.blocking { " (blocking)" } else { "" }
        )
    }));
    if !review.dropped.is_empty() {
        text.push(format!(" {} dropped as not about this change", review.dropped.len()));
    }
    if let Some(sent) = &review.sent {
        text.push(format!(" sent back as {}", sent.command_id));
    }
    if let Some(error) = &review.error {
        text.push(format!(" {error}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use xaya_agent_proto::{Approval, Sandbox};

    fn row(id: &str, state: SessionState) -> ManagedRow {
        ManagedRow {
            session: Session {
                id: id.into(),
                engine: "codex".into(),
                cwd: "/project".into(),
                sandbox: Sandbox::ReadOnly,
                state,
                native_thread_id: Some("thread".into()),
                native_job_id: None,
                turn_id: Some("turn".into()),
                attached: false,
                backend_pid: None,
                last_error: None,
                verification: None,
            },
            approval_count: 1,
        }
    }

    fn detail() -> EventSnapshot {
        EventSnapshot {
            session: row("session", SessionState::Blocked).session,
            approvals: vec![Approval {
                id: "approval".into(),
                kind: xaya_agent_proto::ApprovalKind::Command,
                decisions: vec![ApprovalDecision::Accept, ApprovalDecision::Cancel],
                method: "item/commandExecution/requestApproval".into(),
                turn_id: Some("turn".into()),
                details: serde_json::json!({"command":"printf fixture","availableDecisions":["accept","cancel"]}),
                controllable: true,
            }],
            native_only_approvals: 0,
            epoch: "epoch".into(),
            last_seq: 1,
        }
    }

    #[test]
    fn managed_selection_follows_identity_through_reordering() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![
                row("ready", SessionState::Ready),
                row("waiting", SessionState::Blocked),
            ]),
            ..Default::default()
        });
        assert_eq!(panel.selected().unwrap().id, "waiting");
        panel.select_index(1);
        panel.apply(ManagedUpdate {
            rows: Some(vec![
                row("ready", SessionState::Blocked),
                row("waiting", SessionState::Ready),
            ]),
            ..Default::default()
        });
        assert_eq!(panel.selected().unwrap().id, "ready");
        let action = panel.cancel().unwrap();
        assert!(matches!(action,ManagedAction::Cancel {turn_id,..} if turn_id=="turn"));
    }

    #[test]
    fn claude_panel_interrupts_exact_turn_and_offers_approvals() {
        let mut panel = ManagedPanel::default();
        let mut claude = row("claude", SessionState::Working);
        claude.session.engine = "claude".into();
        panel.apply(ManagedUpdate { rows: Some(vec![claude]), ..Default::default() });
        assert!(
            matches!(panel.cancel(), Some(ManagedAction::Cancel { turn_id, .. }) if turn_id == "turn")
        );
        assert!(panel.pending);
        assert!(panel.cancel().is_none(), "a pending action cannot be sent twice");
        assert!(panel.lines(200, 10)[0].segments[0].text.contains("c interrupt"));
        assert!(panel.lines(200, 10)[0].segments[0].text.contains("a approvals"));
    }

    #[test]
    fn approval_requires_a_loaded_preview_and_an_offered_decision() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("session", SessionState::Blocked)]),
            ..Default::default()
        });
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
        panel.inspect();
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
        panel.apply(ManagedUpdate {
            detail: Some(detail()),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.decide(ApprovalDecision::Decline).is_none());
        assert!(
            matches!(panel.decide(ApprovalDecision::Accept),Some(ManagedAction::Respond {approval_id,..}) if approval_id=="approval")
        );
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
        let mut expired = detail();
        expired.approvals.clear();
        panel.apply(ManagedUpdate {
            detail: Some(expired),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
    }

    #[test]
    fn oversized_and_other_sessions_details_cannot_be_approved() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("session", SessionState::Blocked)]),
            ..Default::default()
        });
        panel.inspect();
        let mut denied = detail();
        denied.approvals[0].controllable = false;
        panel.apply(ManagedUpdate {
            detail: Some(denied),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
        let mut wrong = detail();
        wrong.session.id = "another-session".into();
        panel.apply(ManagedUpdate { detail: Some(wrong), ..Default::default() });
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
    }

    #[test]
    fn verification_preview_cannot_approve_and_cancel_uses_the_loaded_check_identity() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("session", SessionState::Ready)]),
            ..Default::default()
        });
        panel.inspect();
        panel.apply(ManagedUpdate {
            detail: Some(detail()),
            action_done: true,
            ..Default::default()
        });
        assert!(matches!(panel.inspect_checks(), Some(ManagedAction::InspectChecks(_))));
        assert!(panel.decide(ApprovalDecision::Accept).is_none());
        assert!(panel.cancel_check().is_none());
        let mut check: xaya_agent_proto::Check = serde_json::from_value(serde_json::json!({
            "id":"check-current", "session_id":"another", "checkpoint_id":"checkpoint", "command_id":"command",
            "argv":["true"], "timeout_secs":1, "root":"/project", "cwd":"/project", "base_commit":"base",
            "commit":"commit", "tree":"tree", "status":"running", "stale":true,
            "exit_code":null,"duration_ms":null,"error":null,"log_truncated":false
        })).unwrap();
        panel.apply(ManagedUpdate {
            verification: Some(("session".into(), Some(check.clone()))),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.cancel_check().is_none());
        check.session_id = "session".into();
        panel.apply(ManagedUpdate {
            verification: Some(("session".into(), Some(check))),
            ..Default::default()
        });
        assert!(
            matches!(panel.cancel_check(), Some(ManagedAction::CancelCheck(id)) if id == "check-current")
        );
        panel.apply(ManagedUpdate {
            error: Some("service unavailable".into()),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.cancel_check().is_none());
        for width in 0..40 {
            for height in 0..8 {
                assert!(panel.lines(width, height).len() <= height);
            }
        }
    }

    #[test]
    fn layouts_and_scrolling_stay_within_tiny_window_bounds() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("session", SessionState::Blocked)]),
            ..Default::default()
        });
        for preview in [false, true] {
            panel.preview = preview;
            panel.apply(ManagedUpdate { detail: Some(detail()), ..Default::default() });
            panel.scroll = usize::MAX;
            for columns in 0..40 {
                for height in 0..8 {
                    let lines = panel.lines(columns, height);
                    assert!(lines.len() <= height);
                    assert!(
                        lines.iter().flat_map(|line| &line.segments).all(|segment| segment
                            .text
                            .chars()
                            .count()
                            <= columns)
                    );
                }
            }
        }
    }

    fn live(id: &str, turn: Option<&str>) -> ManagedRow {
        let mut row =
            row(id, if turn.is_some() { SessionState::Working } else { SessionState::Ready });
        row.session.turn_id = turn.map(str::to_owned);
        row
    }

    #[test]
    fn end_tools_needs_a_live_session_without_a_running_turn() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![live("busy", Some("turn"))]),
            ..Default::default()
        });
        assert!(panel.end_tools().is_none(), "cancel the turn first");
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("done", SessionState::Stopped)]),
            ..Default::default()
        });
        assert!(panel.end_tools().is_none(), "nothing runs");
        panel.apply(ManagedUpdate { rows: Some(vec![live("idle", None)]), ..Default::default() });
        let header = panel.lines(200, 10)[0].segments[0].text.clone();
        assert!(header.contains("t end tools"), "{header}");
        match panel.end_tools() {
            Some(ManagedAction::EndTools { session_id }) => assert_eq!(session_id, "idle"),
            other => panic!("{other:?}"),
        }
        assert!(panel.pending && panel.end_tools().is_none(), "one request at a time");
    }

    /// A review of `session` as the service reports it.
    fn review(session: &str, verdict: &str) -> xaya_agent_proto::Review {
        serde_json::from_value(serde_json::json!({
            "id": "review-1", "checkpoint_id": "checkpoint", "command_id": "c",
            "session_id": session, "engine": "claude", "same_engine": true,
            "executable": "/bin/claude", "config_dir": null, "root": "/p", "cwd": "/p",
            "base_commit": "b", "commit": "c", "tree": "t", "check_id": "check-1",
            "timeout_secs": 60, "status": "completed", "verdict": verdict,
            "reasons": ["acceptance criteria not met: AC1"],
            "findings": [
                {"title": "[P3] Minor", "body": "b", "priority": 3, "confidence": 0.9,
                 "path": "src/a.py", "start": 1, "end": 1, "blocking": false},
                {"title": "[P1] Divide by zero", "body": "b", "priority": 1, "confidence": 0.9,
                 "path": "src/stats.py", "start": 2, "end": 2, "blocking": true}],
            "dropped": [{"title": "x", "reason": "outside"}],
            "overall_correct": false, "overall_explanation": "Raises.", "diff_truncated": false,
            "native_session_id": null, "stale": false, "created_at_ms": 1, "duration_ms": 5,
            "error": null, "task_id": "task-1", "handoff": null,
            "criteria": [{"id": "AC1", "status": "unmet", "evidence": "raises"}],
        }))
        .unwrap()
    }

    fn reviewing(verdict: &str) -> ManagedPanel {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("session", SessionState::Stopped)]),
            ..Default::default()
        });
        assert!(
            matches!(panel.inspect_review(), Some(ManagedAction::InspectReview(id)) if id == "session")
        );
        assert_eq!(panel.view, DetailView::Review);
        panel.apply(ManagedUpdate {
            review: Some(("session".into(), Some(review("session", verdict)))),
            action_done: true,
            ..Default::default()
        });
        panel
    }

    #[test]
    fn the_review_view_shows_the_verdict_criteria_and_blocking_findings_first() {
        let panel = reviewing("needs-fix");
        let text: Vec<String> =
            panel.lines(120, 30).iter().map(|line| line.segments[0].text.clone()).collect();
        let all = text.join("\n");
        assert!(text[0].contains("s send fixes back"), "{all}");
        for expected in [
            "review-1",
            "NeedsFix",
            "same engine",
            "AC1",
            "unmet",
            "raises",
            "acceptance criteria not met: AC1",
            "1 dropped",
        ] {
            assert!(all.contains(expected), "{expected} missing: {all}");
        }
        let blocking = all.find("[P1] Divide by zero").expect("blocking finding");
        assert!(blocking < all.find("[P3] Minor").expect("minor finding"), "{all}");
        assert!(all.contains("src/stats.py:2-2"), "{all}");
        for width in 0..40 {
            for height in 0..8 {
                assert!(panel.lines(width, height).len() <= height);
            }
        }
    }

    #[test]
    fn only_a_fresh_unsent_needs_fix_review_can_be_sent_from_the_panel() {
        let mut panel = reviewing("needs-fix");
        assert!(matches!(
            panel.send_review(),
            Some(ManagedAction::SendReview { session_id, review_id })
                if session_id == "session" && review_id == "review-1"
        ));
        assert!(panel.pending && panel.send_review().is_none(), "one action at a time");

        let approved = reviewing("approved");
        assert!(!approved.lines(120, 10)[0].segments[0].text.contains("s send"));
        let mut approved = approved;
        assert!(approved.send_review().is_none());
        for change in [
            |r: &mut xaya_agent_proto::Review| r.stale = true,
            |r: &mut xaya_agent_proto::Review| {
                r.sent = Some(xaya_agent_proto::SentFix {
                    command_id: "fix".into(),
                    session_id: "session".into(),
                    at_ms: 1,
                })
            },
            |r: &mut xaya_agent_proto::Review| r.status = xaya_agent_proto::ReviewStatus::Running,
        ] {
            let mut panel = reviewing("needs-fix");
            let mut changed = review("session", "needs-fix");
            change(&mut changed);
            panel.apply(ManagedUpdate {
                review: Some(("session".into(), Some(changed))),
                ..Default::default()
            });
            assert!(panel.send_review().is_none());
        }
        // Not from another view either.
        let mut list = reviewing("needs-fix");
        list.preview = false;
        assert!(list.send_review().is_none());
    }

    #[test]
    fn a_review_belongs_to_the_session_it_was_loaded_for() {
        let mut panel = ManagedPanel::default();
        panel.apply(ManagedUpdate {
            rows: Some(vec![row("one", SessionState::Ready), row("two", SessionState::Ready)]),
            ..Default::default()
        });
        panel.select_index(0);
        panel.inspect_review();
        // Another session's review never shows here.
        panel.apply(ManagedUpdate {
            review: Some(("one".into(), Some(review("two", "needs-fix")))),
            action_done: true,
            ..Default::default()
        });
        assert!(panel.send_review().is_none());
        panel.apply(ManagedUpdate {
            review: Some(("one".into(), Some(review("one", "needs-fix")))),
            ..Default::default()
        });
        assert!(panel.lines(120, 20).iter().any(|l| l.segments[0].text.contains("review-1")));
        // Moving the selection drops it.
        panel.select_index(1);
        assert_eq!(panel.view, DetailView::Approvals);
        panel.inspect_review();
        assert!(panel.lines(120, 20).iter().all(|l| !l.segments[0].text.contains("review-1")));
        assert!(panel.send_review().is_none());
        panel.apply(ManagedUpdate {
            review: Some(("two".into(), None)),
            action_done: true,
            ..Default::default()
        });
        assert!(
            panel.lines(120, 20).iter().any(|l| l.segments[0].text.contains("No review recorded"))
        );
    }

    /// Each line's text without spaces, since wide characters are followed by a spacer column.
    fn texts(panel: &ManagedPanel) -> Vec<String> {
        let lines = panel.lines(60, 20);
        lines.iter().map(|line| line.segments[0].text.replace(' ', "")).collect()
    }

    fn delegated(panel: &mut ManagedPanel, request: &str, result: Result<&str, &str>) {
        panel.apply(ManagedUpdate {
            action_done: true,
            submitted: Some((
                request.into(),
                result.map(|title| format!(" Handed over · {title}")).map_err(str::to_owned),
            )),
            ..Default::default()
        });
    }

    #[test]
    fn a_request_is_written_in_the_project_view_and_handed_over_once() {
        let mut panel = ManagedPanel::default();
        assert!(!panel.start_draft(), "only the project view takes a request");
        panel.open_board(None);
        assert!(!panel.start_draft(), "a request needs the window's checkout");
        panel.open_board(Some("/repo".into()));
        assert!(panel.start_draft());
        assert!(panel.submit_draft().is_none(), "an empty request is not handed over");
        panel.edit_draft(|text| text.push_str("修复登录\n页面"));
        assert!(texts(&panel)[0].contains("Newtask"));
        assert!(
            texts(&panel).iter().any(|line| line.contains("›修复登录页面▏")),
            "{:?}",
            texts(&panel)
        );

        assert!(texts(&panel).iter().any(|line| line == "Claudeimplements·Codexreviews"));
        let Some(ManagedAction::Delegate { cwd, goal, request_id, implementer }) =
            panel.submit_draft()
        else {
            panic!("the request is handed over");
        };
        assert_eq!((cwd.to_str(), goal.as_str()), (Some("/repo"), "修复登录页面"));
        assert_eq!(implementer, Implementer::Claude, "Claude implements by default");
        assert!(!panel.switch_implementer(), "nor switched while it is being handed over");
        assert!(panel.submit_draft().is_none(), "nothing more while it is being handed over");
        panel.edit_draft(|text| text.push('x'));
        assert_eq!(panel.draft.as_ref().unwrap().text, "修复登录页面", "nor edited");

        // An uncertain failure keeps the text; sent again unchanged, it is the same request.
        delegated(&mut panel, &request_id, Err("service request timed out"));
        assert!(texts(&panel).iter().any(|line| line.contains("Nothandedover")));
        let Some(ManagedAction::Delegate { request_id: again, .. }) = panel.submit_draft() else {
            panic!()
        };
        assert_eq!(again, request_id);
        delegated(&mut panel, &again, Err("refused"));
        // An edited request is a new one.
        panel.edit_draft(|text| text.push('!'));
        let Some(ManagedAction::Delegate { request_id: edited, .. }) = panel.submit_draft() else {
            panic!()
        };
        assert_ne!(edited, request_id);
        // A stale answer changes nothing; the matching one closes the draft.
        delegated(&mut panel, &request_id, Ok("old"));
        assert!(panel.draft.is_some());
        delegated(&mut panel, &edited, Ok("修复登录页面! · task-1"));
        assert!(panel.draft.is_none());
        assert!(texts(&panel).iter().any(|line| line.contains("Handedover·修复登录页面!")));
        assert!(texts(&panel)[0].contains("nnewtask"));
    }

    #[test]
    fn a_new_task_can_go_to_the_other_engine_as_a_new_request() {
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/repo".into()));
        assert!(!panel.switch_implementer(), "only while writing a new task");
        panel.start_draft();
        panel.edit_draft(|text| text.push_str("add a feature"));
        let Some(ManagedAction::Delegate { request_id: first, .. }) = panel.submit_draft() else {
            panic!()
        };
        delegated(&mut panel, &first, Err("claude: not found"));
        assert!(panel.switch_implementer());
        assert!(texts(&panel).iter().any(|line| line == "Codeximplements·Claudereviews"));
        let Some(ManagedAction::Delegate { request_id, implementer, .. }) = panel.submit_draft()
        else {
            panic!()
        };
        assert_eq!(implementer, Implementer::Codex);
        assert_ne!(request_id, first, "another engine is another request");
        delegated(&mut panel, &request_id, Ok("add a feature · task-1"));
        // The switch was for that task; the next one starts from the project's engine again.
        panel.start_draft();
        panel.edit_draft(|text| text.push('x'));
        let Some(ManagedAction::Delegate { implementer, request_id, .. }) = panel.submit_draft()
        else {
            panic!()
        };
        assert_eq!(implementer, Implementer::Claude);
        delegated(&mut panel, &request_id, Ok("x · task-2"));

        // A project that sets Codex starts there, also for a draft begun before the board came.
        let codex = |mut update: ManagedUpdate| {
            update.board.as_mut().unwrap().engine = xaya_agent_proto::ReviewEngine::Codex;
            update
        };
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/repo".into()));
        panel.start_draft();
        panel.apply(codex(board(Vec::new())));
        assert!(texts(&panel).iter().any(|line| line == "Codeximplements·Claudereviews"));
        panel.cancel_draft();
        panel.start_draft();
        panel.edit_draft(|text| text.push('y'));
        let Some(ManagedAction::Delegate { implementer, .. }) = panel.submit_draft() else {
            panic!()
        };
        assert_eq!(implementer, Implementer::Codex);
        // A draft switched by hand keeps its engine when the board comes again.
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/repo".into()));
        panel.apply(codex(board(Vec::new())));
        panel.start_draft();
        assert!(panel.switch_implementer());
        panel.apply(codex(board(Vec::new())));
        assert!(texts(&panel).iter().any(|line| line == "Claudeimplements·Codexreviews"));
        // Requested changes go to the task's own session; there is no engine to choose.
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/p".into()));
        panel.apply(board(vec![board_row("task-1", "awaiting_acceptance")]));
        panel.open_task();
        loaded(&mut panel, driven(accepting("task-1", None, None), "approved"));
        assert!(panel.start_changes());
        assert!(!panel.switch_implementer());
        assert!(!texts(&panel).iter().any(|line| line.contains("implements")));
    }

    #[test]
    fn a_draft_is_bounded_cancelled_and_kept_to_its_checkout() {
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/repo".into()));
        panel.start_draft();
        panel.edit_draft(|text| text.push_str(&"字".repeat(GOAL_LIMIT + 10)));
        assert_eq!(panel.draft.as_ref().unwrap().text.chars().count(), GOAL_LIMIT);
        // A long request shows its end within the window.
        let lines = panel.lines(30, 10);
        assert!(lines.len() <= 10);
        assert!(lines.iter().any(|line| line.segments[0].text.contains('▏')));
        panel.open_board(Some("/repo".into()));
        assert!(panel.draft.is_some(), "refreshing the same project keeps the draft");
        panel.open_board(Some("/other".into()));
        assert!(panel.draft.is_none(), "another checkout starts over");
        panel.start_draft();
        panel.cancel_draft();
        assert!(panel.draft.is_none());
    }

    #[test]
    fn project_preview_works_without_a_selected_session() {
        let mut panel = ManagedPanel::default();
        panel.open_board(None);
        panel.apply(ManagedUpdate {
            board: Some(xaya_agent_proto::TaskBoard {
                engine: xaya_agent_proto::TASK_DEFAULT_ENGINE,
                project: "/repo/.git".into(),
                capacity: 1,
                occupied: 0,
                available: 1,
                tasks: vec![xaya_agent_proto::TaskBoardRow {
                    task_id: "task-1".into(),
                    title: "Fix login".into(),
                    state: xaya_agent_proto::BoardState::Eligible,
                    attention: None,
                    dependencies: vec![],
                    landing: None,
                }],
            }),
            rows: Some(vec![]),
            action_done: true,
            ..Default::default()
        });
        let lines = panel.lines(120, 20);
        assert!(lines.iter().any(|line| line.segments[0].text.contains("Fix login · eligible")));
        assert!(lines.iter().any(|line| line.segments[0].text.contains("1 free / 1 slots")));
        assert!(panel.inspect_id().is_none());
    }

    #[test]
    fn dependency_reason_is_visible_before_the_first_review() {
        let mut panel = reviewing("needs-fix");
        panel.apply(ManagedUpdate {
            review: Some(("session".into(), None)),
            dependencies: Some(("session".into(), vec![xaya_agent_proto::DependencySnapshot {
                task_id: "task-a".into(),
                title: "Fix login".into(),
                state: xaya_agent_proto::DependencyState::AwaitingLanding,
            }])),
            ..Default::default()
        });
        assert!(panel.lines(120, 20).iter().any(|line| {
            line.segments[0].text.contains("Fix login: waiting for reviewed changes to land")
        }));
    }

    #[test]
    fn a_task_that_needs_attention_shows_why_and_offers_no_send() {
        let mut panel = reviewing("needs-fix");
        panel.apply(ManagedUpdate {
            task_attention: Some(("session".into(), Some("the fix round changed nothing".into()))),
            ..Default::default()
        });
        assert!(panel.send_review().is_none());
        let text: Vec<String> =
            panel.lines(120, 20).iter().map(|line| line.segments[0].text.clone()).collect();
        assert!(!text[0].contains("s send"), "{text:?}");
        assert!(text.iter().any(|l| l.contains("the fix round changed nothing")), "{text:?}");
        // Cleared again: sendable.
        panel.apply(ManagedUpdate {
            task_attention: Some(("session".into(), None)),
            ..Default::default()
        });
        assert!(panel.send_review().is_some());
    }

    fn board_row(id: &str, state: &str) -> serde_json::Value {
        serde_json::json!({"task_id": id, "title": format!("Title {id}"), "state": state,
                           "attention": null, "dependencies": []})
    }

    fn board(rows: Vec<serde_json::Value>) -> ManagedUpdate {
        let board = serde_json::json!({"project": "/p/.git", "capacity": 1, "occupied": 0,
                                       "available": 1, "tasks": rows});
        ManagedUpdate {
            action_done: true,
            board: Some(serde_json::from_value(board).unwrap()),
            ..Default::default()
        }
    }

    fn accepting(
        id: &str,
        blocked: Option<&str>,
        landing: Option<(&str, &str)>,
    ) -> xaya_agent_proto::TaskAcceptance {
        let landing = landing.map(|(request, state)| {
            serde_json::json!({"request_id": request, "target_branch": "main", "state": state,
                               "reason": "conflict", "preview_id": null,
                               "started_at_ms": 1, "updated_at_ms": 1})
        });
        serde_json::from_value(serde_json::json!({
            "task": {"id": id, "title": "Add a feature", "goal": "Add a feature\nmore",
                     "constraints": null, "criteria": [], "sessions": [], "status": "open",
                     "created_at_ms": 1, "landing": landing},
            "target_branch": "main",
            "review": review("session-1", "approved"),
            "check": null,
            "files": [{"path": "src/feature.txt", "added": 1, "removed": 0},
                      {"path": "logo.png", "added": null, "removed": null}],
            "diff": "--- a/src/feature.txt\n+++ b/src/feature.txt\n@@ -0,0 +1 @@\n+done\n",
            "diff_truncated": false,
            "blocked": blocked,
        }))
        .unwrap()
    }

    fn loaded(panel: &mut ManagedPanel, acceptance: xaya_agent_proto::TaskAcceptance) {
        panel.apply(ManagedUpdate {
            action_done: true,
            acceptance: Some((acceptance.task.id.clone(), acceptance)),
            ..Default::default()
        });
    }

    #[test]
    fn the_board_puts_what_needs_a_person_first_and_opens_the_chosen_task() {
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/p".into()));
        panel.apply(board(vec![
            board_row("task-a", "eligible"),
            board_row("task-b", "awaiting_acceptance"),
            board_row("task-c", "needs_attention"),
        ]));
        let order: Vec<_> = panel.board_rows().iter().map(|row| row.task_id.as_str()).collect();
        assert_eq!(order, ["task-c", "task-b", "task-a"]);
        assert!(texts(&panel).iter().any(|text| text.contains("Titletask-b·readytoaccept")));
        // The choice follows the task as the board changes.
        panel.select_relative(1);
        panel.apply(board(vec![board_row("task-b", "landing"), board_row("task-c", "eligible")]));
        assert_eq!(panel.board_task.as_deref(), Some("task-b"));
        panel.select_relative(5);
        assert_eq!(panel.board_task.as_deref(), Some("task-c"), "the last row, not past it");

        let Some(ManagedAction::InspectTask(id)) = panel.open_task() else { panic!() };
        assert_eq!(id, "task-c");
        assert_eq!(panel.view, DetailView::Acceptance);
        assert_eq!(panel.inspect_id().as_deref(), Some("task-c"), "loading: kept current");
        // Another task's answer is not this page's.
        loaded(&mut panel, accepting("task-b", None, None));
        assert!(panel.acceptance.is_none());
        loaded(&mut panel, accepting("task-c", None, None));
        assert_eq!(panel.inspect_id(), None, "nothing in flight: no polling");
        let text = texts(&panel);
        assert!(text[0].contains("llandonmain"), "{text:?}");
        for expected in ["Readytolandonmain", "2fileschanged·+1−0", "binarylogo.png", "+done"] {
            assert!(text.iter().any(|line| line.contains(expected)), "{expected}: {text:?}");
        }
    }

    #[test]
    fn a_task_lands_once_per_request_and_only_when_nothing_blocks_it() {
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/p".into()));
        panel.apply(board(vec![board_row("task-1", "active_session")]));
        panel.open_task();
        loaded(&mut panel, accepting("task-1", Some("its latest review asks for changes"), None));
        assert!(panel.land().is_none());
        assert!(!texts(&panel)[0].contains("llandon"));
        assert!(texts(&panel).iter().any(|line| line.contains("Cannotlandyet")));

        loaded(&mut panel, accepting("task-1", None, None));
        let Some(ManagedAction::Land { task_id, request_id: first }) = panel.land() else {
            panic!()
        };
        assert_eq!(task_id, "task-1");
        assert!(panel.land().is_none(), "one at a time");
        // No answer from the service: asking again is the same request.
        panel.apply(ManagedUpdate {
            action_done: true,
            error: Some(" Not landed: timed out".into()),
            ..Default::default()
        });
        let Some(ManagedAction::Land { request_id: again, .. }) = panel.land() else { panic!() };
        assert_eq!(again, first);
        // The service took it and it failed: landing again is a new request.
        loaded(&mut panel, accepting("task-1", None, Some((&first, "failed"))));
        assert!(texts(&panel).iter().any(|line| line.contains("Thelastlandingfailed:conflict")));
        let Some(ManagedAction::Land { request_id: next, .. }) = panel.land() else { panic!() };
        assert_ne!(next, first);
        // While it lands, the page is kept current and cannot land again.
        loaded(&mut panel, accepting("task-1", Some("it is landing"), Some((&next, "landing"))));
        assert!(panel.land().is_none());
        assert_eq!(panel.inspect_id().as_deref(), Some("task-1"));
        assert!(texts(&panel).iter().any(|line| line.contains("Landingonmain:conflict")));
        assert!(!texts(&panel).iter().any(|line| line.contains("Cannotlandyet")));
        // Landed: kept current until the service says what became of the workspace.
        let landed = |workspace: serde_json::Value| {
            let mut acceptance =
                accepting("task-1", Some("it has landed"), Some((&next, "landed")));
            let landing = acceptance.task.landing.as_mut().unwrap();
            landing.workspace = serde_json::from_value(workspace).unwrap();
            acceptance
        };
        loaded(&mut panel, landed(serde_json::Value::Null));
        assert_eq!(panel.inspect_id().as_deref(), Some("task-1"));
        let at = |outcome: &str, reason: serde_json::Value| {
            serde_json::json!({"outcome": outcome, "path": "/p.xaya/task-1",
                               "reason": reason, "at_ms": 2})
        };
        loaded(&mut panel, landed(at("removing", serde_json::Value::Null)));
        assert_eq!(panel.inspect_id().as_deref(), Some("task-1"));
        loaded(&mut panel, landed(at("removed", serde_json::Value::Null)));
        assert_eq!(panel.inspect_id(), None);
        assert!(texts(&panel).iter().any(|line| line.contains("Workspaceremoved:/p.xaya/task-1")));
        loaded(&mut panel, landed(at("kept", "it changed".into())));
        assert_eq!(panel.inspect_id(), None);
        assert!(texts(&panel).iter().any(|line| line.contains("/p.xaya/task-1:itchanged")));
    }

    fn driven(
        mut acceptance: xaya_agent_proto::TaskAcceptance,
        state: &str,
    ) -> xaya_agent_proto::TaskAcceptance {
        acceptance.task.drive = Some(
            serde_json::from_value(serde_json::json!({
                "params": {"task_id": acceptance.task.id, "request_id": "drive",
                           "reviewer": {"engine": "codex", "executable": "/bin/codex"},
                           "timeout_secs": 60},
                "state": state, "reason": null, "session_id": "session-1",
                "started_at_ms": 1, "updated_at_ms": 1,
            }))
            .unwrap(),
        );
        acceptance
    }

    #[test]
    fn changes_are_asked_of_a_driven_task_from_its_acceptance_page() {
        let mut panel = ManagedPanel::default();
        panel.open_board(Some("/p".into()));
        panel.apply(board(vec![board_row("task-1", "awaiting_acceptance")]));
        panel.open_task();
        // A task the service does not drive takes no requested changes here.
        loaded(&mut panel, accepting("task-1", None, None));
        assert!(!panel.start_changes());
        assert!(!texts(&panel)[0].contains("crequestchanges"));

        loaded(&mut panel, driven(accepting("task-1", None, None), "approved"));
        assert!(texts(&panel)[0].contains("crequestchanges"), "{:?}", texts(&panel));
        assert!(panel.start_changes());
        assert!(panel.drafting());
        panel.edit_draft(|text| text.push_str("也写一份说明"));
        assert!(texts(&panel)[0].contains("Requestchanges"));
        assert!(texts(&panel).iter().any(|line| line.contains("Whatshouldchange?")));
        let Some(ManagedAction::Revise { task_id, request_id, feedback }) = panel.submit_draft()
        else {
            panic!("the changes are sent");
        };
        assert_eq!((task_id.as_str(), feedback.as_str()), ("task-1", "也写一份说明"));
        assert!(request_id.starts_with("change-"));
        // Unknown outcome: the same words are the same request.
        panel.apply(ManagedUpdate {
            action_done: true,
            submitted: Some((request_id.clone(), Err("timed out".into()))),
            ..Default::default()
        });
        assert!(texts(&panel).iter().any(|line| line.contains("Notsent:timedout")));
        let Some(ManagedAction::Revise { request_id: again, .. }) = panel.submit_draft() else {
            panic!()
        };
        assert_eq!(again, request_id);
        panel.apply(ManagedUpdate {
            action_done: true,
            submitted: Some((request_id, Ok(" Sent".into()))),
            ..Default::default()
        });
        assert!(panel.draft.is_none());

        // While the changes are under way the page offers neither landing nor more changes.
        let mut under_way =
            driven(accepting("task-1", Some("changes are under way"), None), "driving");
        under_way.task.revisions = vec![
            serde_json::from_value(serde_json::json!({
                "request_id": "change-1", "feedback": "也写一份说明\n细节",
                "review_id": "review-1", "checkpoint_id": "checkpoint", "requested_at_ms": 1,
            }))
            .unwrap(),
        ];
        loaded(&mut panel, under_way);
        assert!(!panel.start_changes());
        assert!(panel.land().is_none());
        assert_eq!(panel.inspect_id().as_deref(), Some("task-1"), "kept current");
        assert!(texts(&panel).iter().any(|line| line.contains("Changesunderway:也写一份说明")));

        // A draft of changes stays with its task's page.
        loaded(&mut panel, driven(accepting("task-1", None, None), "approved"));
        assert!(panel.start_changes());
        panel.open_board(Some("/p".into()));
        assert!(panel.draft.is_none(), "leaving the page drops it");
    }
}
