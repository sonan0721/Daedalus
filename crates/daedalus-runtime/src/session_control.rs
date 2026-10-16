//! One authoritative project Engine with bounded, per-request control results.

use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use daedalus_content::project::{PreparedProject, ProjectFileIdentity};
use daedalus_content::scene::StableId;
use daedalus_core::{
    CommandId, EntityHandle, RenderSnapshot, Revision, SessionId, Tick, Transform,
};
use daedalus_world::{
    CommandResult, CommandSubmission, WorldCommand, WorldCommandKind, WorldReadError,
    WorldTransformState,
};
use thiserror::Error;

use crate::project::MAX_PROJECT_SESSION_ENTITIES;
use crate::session_primitives::{
    CreditError, CreditLease, CreditLimits, CreditPool, LatestValueMailbox, MailboxError,
    StopSignal,
};
use crate::{EngineConfig, ProjectSession, ProjectSessionStartError, StepReport};

const MAX_CONTROL_QUEUE: usize = 4_096;
const MAX_CONTROL_CLIENTS: usize = 64;
const MAX_PENDING_PER_CLIENT: usize = 256;
const MAX_PENDING_TOTAL: usize = 4_096;
const MAX_RESULT_BYTES_PER_TICKET: usize = 1024 * 1024;
const MAX_RESULT_BYTES_TOTAL: usize = 32 * 1024 * 1024;
const MAX_CONTROL_PAGE_ENTITIES: usize = 1_024;
const MAX_MANUAL_STEPS: u32 = 120;
const MAX_RETAINED_FRAMES: usize = 64;
const MAX_REPLAY_LEDGER_ENTRIES: usize = 16_384;
const MAX_ERROR_TEXT_BYTES: usize = 512;
const MAX_SOURCE_FILES: usize = 258;
const MAX_CALLER_COMMAND_ID: u128 = 1_u128 << 127;
const SERVER_COMMAND_ID_PREFIX: u128 = 1_u128 << 127;
const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionClockMode {
    Manual,
    Realtime,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionControlConfig {
    pub engine: EngineConfig,
    pub clock_mode: SessionClockMode,
    pub limits: SessionControlLimits,
}

impl Default for SessionControlConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            clock_mode: SessionClockMode::Manual,
            limits: SessionControlLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionControlLimits {
    pub command_capacity: usize,
    pub max_clients: usize,
    pub max_pending_per_client: usize,
    pub max_pending_total: usize,
    pub max_result_bytes_total: usize,
    pub max_page_entities: usize,
    pub max_steps_per_request: u32,
    pub max_retained_frames: usize,
    pub replay_ledger_capacity: usize,
}

impl Default for SessionControlLimits {
    fn default() -> Self {
        Self {
            command_capacity: 128,
            max_clients: 16,
            max_pending_per_client: 8,
            max_pending_total: 128,
            max_result_bytes_total: MAX_RESULT_BYTES_TOTAL,
            max_page_entities: MAX_CONTROL_PAGE_ENTITIES,
            max_steps_per_request: MAX_MANUAL_STEPS,
            max_retained_frames: 3,
            replay_ledger_capacity: 1_024,
        }
    }
}

impl SessionControlLimits {
    fn validate(self, engine: &EngineConfig) -> Result<(), SessionControlStartError> {
        if !(2..=MAX_CONTROL_QUEUE).contains(&self.command_capacity) {
            return Err(SessionControlStartError::InvalidConfig(
                "command_capacity must be in 2..=4096 (one actor queue slot is reserved for Manual Step)"
                    .to_owned(),
            ));
        }
        if !(1..=MAX_CONTROL_CLIENTS).contains(&self.max_clients) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_clients must be in 1..=64".to_owned(),
            ));
        }
        if !(2..=MAX_PENDING_PER_CLIENT).contains(&self.max_pending_per_client) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_pending_per_client must be in 2..=256 (one client slot is reserved for Manual Step)"
                    .to_owned(),
            ));
        }
        if !(2..=MAX_PENDING_TOTAL).contains(&self.max_pending_total) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_pending_total must be in 2..=4096 (one session slot is reserved for Manual Step)"
                    .to_owned(),
            ));
        }
        if !(2 * MAX_RESULT_BYTES_PER_TICKET..=MAX_RESULT_BYTES_TOTAL)
            .contains(&self.max_result_bytes_total)
        {
            return Err(SessionControlStartError::InvalidConfig(
                "max_result_bytes_total must be in 2..=32 MiB; one 1 MiB ticket is reserved for Manual Step"
                    .to_owned(),
            ));
        }
        if !(1..=MAX_CONTROL_PAGE_ENTITIES).contains(&self.max_page_entities) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_page_entities must be in 1..=1024".to_owned(),
            ));
        }
        if !(1..=MAX_MANUAL_STEPS).contains(&self.max_steps_per_request) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_steps_per_request must be in 1..=120".to_owned(),
            ));
        }
        if !(1..=MAX_RETAINED_FRAMES).contains(&self.max_retained_frames) {
            return Err(SessionControlStartError::InvalidConfig(
                "max_retained_frames must be in 1..=64".to_owned(),
            ));
        }
        if !(1..=MAX_REPLAY_LEDGER_ENTRIES).contains(&self.replay_ledger_capacity) {
            return Err(SessionControlStartError::InvalidConfig(
                "replay_ledger_capacity must be in 1..=16384".to_owned(),
            ));
        }
        if engine.command_capacity == 0 || engine.command_capacity > MAX_CONTROL_QUEUE {
            return Err(SessionControlStartError::InvalidConfig(
                "EngineConfig.command_capacity must be in 1..=4096".to_owned(),
            ));
        }
        if !(1..=16_384).contains(&engine.command_dedup_capacity) {
            return Err(SessionControlStartError::InvalidConfig(
                "EngineConfig.command_dedup_capacity must be in 1..=16384".to_owned(),
            ));
        }
        if !(2..=MAX_RETAINED_FRAMES).contains(&engine.snapshot_capacity) {
            return Err(SessionControlStartError::InvalidConfig(
                "EngineConfig.snapshot_capacity must be in 2..=64 for last-good service frames"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionCapabilities {
    pub inspect: bool,
    pub mutate: bool,
    pub manual_step: bool,
}

impl SessionCapabilities {
    #[must_use]
    pub const fn trusted_local() -> Self {
        Self {
            inspect: true,
            mutate: true,
            manual_step: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionClientId(u64);

impl SessionClientId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestTicketId {
    session: SessionId,
    client: SessionClientId,
    sequence: u64,
}

impl RequestTicketId {
    #[must_use]
    pub const fn session(self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn client(self) -> SessionClientId {
        self.client
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionEntityTarget {
    StableId(StableId),
    Handle(EntityHandle),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionOperation {
    ProjectInfo {
        offset: usize,
        limit: Option<usize>,
    },
    SpawnTransform {
        transform: Transform,
    },
    GetTransform {
        target: SessionEntityTarget,
    },
    ListTransforms {
        offset: usize,
        limit: Option<usize>,
    },
    SetTransform {
        target: SessionEntityTarget,
        command_id: Option<CommandId>,
        expected_revision: Revision,
        apply_at: Option<Tick>,
        transform: Transform,
    },
    Step {
        count: u32,
    },
    Status,
}

impl SessionOperation {
    fn needs_inspect(&self) -> bool {
        matches!(
            self,
            Self::ProjectInfo { .. }
                | Self::GetTransform { .. }
                | Self::ListTransforms { .. }
                | Self::Status
        )
    }

    fn needs_mutate(&self) -> bool {
        matches!(
            self,
            Self::SpawnTransform { .. } | Self::SetTransform { .. }
        )
    }

    fn is_step(&self) -> bool {
        matches!(self, Self::Step { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestPhase {
    Queued,
    AuthorityAccepted,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitStage {
    AuthorityAccepted,
    Terminal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthorityAdmission {
    pub command_id: CommandId,
    pub submission: CommandSubmission,
    /// Known target tick when this service owns the pending command; cached World-only replay may
    /// leave it unknown.
    pub apply_tick: Option<Tick>,
}

pub struct RequestProgress {
    id: RequestTicketId,
    phase: RequestPhase,
    authority: Option<AuthorityAdmission>,
    terminal: Option<Result<SessionRequestResult, SessionRequestError>>,
    _credit: Arc<CreditBundle>,
}

impl RequestProgress {
    #[must_use]
    pub const fn id(&self) -> RequestTicketId {
        self.id
    }

    #[must_use]
    pub const fn phase(&self) -> RequestPhase {
        self.phase
    }

    #[must_use]
    pub fn authority(&self) -> Option<&AuthorityAdmission> {
        self.authority.as_ref()
    }

    #[must_use]
    pub fn terminal(&self) -> Option<&Result<SessionRequestResult, SessionRequestError>> {
        self.terminal.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectEntityBinding {
    pub id: StableId,
    pub handle: EntityHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectInfoPage {
    pub session: SessionId,
    pub tick: Tick,
    pub name: String,
    pub schema_version: u32,
    pub source_scene_revision: u64,
    pub source_digest: String,
    pub source_files: Vec<ProjectFileIdentity>,
    pub entity_count: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub entities: Vec<ProjectEntityBinding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionEntityState {
    pub stable_id: Option<StableId>,
    pub handle: EntityHandle,
    pub transform: Transform,
    pub revision: Revision,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransformState {
    pub observed_tick: Tick,
    pub entity: SessionEntityState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransformPage {
    pub observed_tick: Tick,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub entities: Vec<SessionEntityState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStatus {
    pub session: SessionId,
    pub clock: SessionClockMode,
    pub tick: Tick,
    pub entity_count: usize,
    pub pending_commands: usize,
    pub active_clients: usize,
    pub stopping: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandTerminalResult {
    pub result: CommandResult,
    /// `None` means the bounded replay ledger no longer knows the original resolution tick.
    pub resolve_tick: Option<Tick>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionRequestResult {
    ProjectInfo(ProjectInfoPage),
    Spawned {
        handle: EntityHandle,
        entity_count: usize,
    },
    Transform(TransformState),
    Transforms(TransformPage),
    Command(CommandTerminalResult),
    Step(Vec<StepReport>),
    Status(SessionStatus),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionRequestError {
    #[error("project stable ID '{0}' is not present in this session")]
    UnknownStableId(StableId),
    #[error("project stable ID '{0}' has no runtime entity binding")]
    MissingBinding(StableId),
    #[error(transparent)]
    WorldRead(#[from] WorldReadError),
    #[error("world command rejected: {0}")]
    CommandRejected(String),
    #[error("runtime entity limit (16384) reached")]
    EntityLimit,
    #[error("runtime entity spawn failed: {0}")]
    SpawnFailed(String),
    #[error("request result exceeds the 1 MiB ticket result ceiling")]
    ResultTooLarge,
    #[error("Manual Step could not advance after {completed_ticks} tick(s): {message}")]
    StepFailed {
        completed_ticks: usize,
        message: String,
    },
    #[error("request stopped before it reached Engine authority")]
    StoppedBeforeAuthority,
    #[error("Engine accepted command {command_id:?}, but shutdown occurred before its apply tick")]
    StoppedNotApplied { command_id: CommandId },
    #[error("Engine may have applied command {command_id:?}, but its final outcome is unknown")]
    OutcomeUnknown { command_id: CommandId },
    #[error("session control actor failed: {0}")]
    ActorFailed(String),
    #[error(
        "caller command ID {command_id} is at or below session high-water {high_water} but outside the replay ledger"
    )]
    ReplayWindowExpired { command_id: u128, high_water: u128 },
    #[error("pending command {0:?} has no service-owned request record")]
    UnknownPendingCommand(CommandId),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SessionSubmitError {
    #[error("the bounded actor queue is full")]
    QueueFull,
    #[error("bounded result ticket or byte credits are exhausted")]
    ResultCapacity,
    #[error("request admission is busy; retry without waiting")]
    Busy,
    #[error("this client attachment has detached")]
    Detached,
    #[error("the session is stopping or stopped")]
    Stopped,
    #[error("the client does not have the required session capability")]
    CapabilityDenied,
    #[error("the operation is not valid for this session clock mode")]
    ClockModeMismatch,
    #[error("request parameters exceed the configured limits")]
    InvalidRequest,
    #[error("caller command IDs in the upper half of u128 are reserved for the session")]
    ReservedCommandId,
    #[error("request queue sequence is exhausted")]
    QueueSequenceExhausted,
    #[error("request ticket sequence is exhausted")]
    TicketIdExhausted,
}

#[derive(Debug, Error)]
pub enum SessionControlStartError {
    #[error("invalid session-control configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Project(#[from] ProjectSessionStartError),
    #[error("could not start project session actor: {0}")]
    ThreadSpawn(String),
    #[error("project actor stopped before startup completed")]
    StartupDisconnected,
    #[error("project actor startup handshake failed: {0}")]
    StartupFailed(String),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestReadError {
    #[error("request progress state is busy")]
    Busy,
    #[error("request result was acknowledged")]
    Acknowledged,
    #[error("request result was explicitly abandoned")]
    Abandoned,
    #[error("request progress state was poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestWaitError {
    #[error(transparent)]
    Read(#[from] RequestReadError),
    #[error("request wait timed out; the ticket remains live")]
    TimedOut,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RequestAckError {
    #[error("request progress state is busy")]
    Busy,
    #[error(transparent)]
    Read(#[from] RequestReadError),
    #[error("a non-terminal ticket cannot be acknowledged")]
    NotTerminal,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum FrameReadError {
    #[error("latest frame mailbox is busy")]
    Busy,
    #[error("latest frame mailbox is poisoned")]
    Poisoned,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTermination {
    StopRequested,
    EngineError(String),
    Panicked(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionShutdownReport {
    pub session: Option<SessionId>,
    pub final_tick: Tick,
    pub termination: SessionTermination,
    pub queued_requests_stopped: usize,
    pub commands_stopped_not_applied: usize,
    pub terminal_unacknowledged: usize,
    pub abandoned_tickets: usize,
    pub result_credit_tickets: usize,
    pub result_credit_bytes: usize,
}

/// One latest project presentation frame. Fields are read-only so consumers retain the bounded
/// frame Arc itself rather than extracting an untracked snapshot Arc from it.
#[derive(Debug, PartialEq)]
pub struct SessionFrame {
    session: SessionId,
    tick: Tick,
    sequence: u64,
    project_generation: u64,
    source_digest: String,
    source_scene_revision: u64,
    snapshot: Arc<RenderSnapshot>,
}

impl SessionFrame {
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn tick(&self) -> Tick {
        self.tick
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    #[must_use]
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    #[must_use]
    pub const fn source_scene_revision(&self) -> u64 {
        self.source_scene_revision
    }

    #[must_use]
    pub fn snapshot(&self) -> &RenderSnapshot {
        &self.snapshot
    }
}

struct CreditBundle {
    _session: CreditLease,
    _client: CreditLease,
}

struct RequestCellState {
    progress: Option<Arc<RequestProgress>>,
    acknowledged: bool,
    abandoned: bool,
}

struct ControlCounters {
    terminal_unacked: AtomicUsize,
    abandoned: AtomicUsize,
    queued_stopped: AtomicUsize,
    commands_stopped_not_applied: AtomicUsize,
}

struct ActiveRequest {
    cell: Arc<RequestCell>,
    possible_command_id: Option<CommandId>,
}

struct RequestCell {
    id: RequestTicketId,
    state: Mutex<RequestCellState>,
    changed: Condvar,
    counters: Arc<ControlCounters>,
    abandon_requested: AtomicBool,
    terminal_unacked_counted: AtomicBool,
}

impl RequestCell {
    fn new(id: RequestTicketId, credit: Arc<CreditBundle>, counters: Arc<ControlCounters>) -> Self {
        let progress = Arc::new(RequestProgress {
            id,
            phase: RequestPhase::Queued,
            authority: None,
            terminal: None,
            _credit: credit,
        });
        Self {
            id,
            state: Mutex::new(RequestCellState {
                progress: Some(progress),
                acknowledged: false,
                abandoned: false,
            }),
            changed: Condvar::new(),
            counters,
            abandon_requested: AtomicBool::new(false),
            terminal_unacked_counted: AtomicBool::new(false),
        }
    }

    fn progress(&self) -> Result<Arc<RequestProgress>, RequestReadError> {
        if self.abandon_requested.load(Ordering::Acquire) {
            return Err(RequestReadError::Abandoned);
        }
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(RequestReadError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(RequestReadError::Poisoned),
        };
        if state.acknowledged {
            return Err(RequestReadError::Acknowledged);
        }
        if state.abandoned {
            return Err(RequestReadError::Abandoned);
        }
        state.progress.clone().ok_or(RequestReadError::Abandoned)
    }

    fn set_authority(&self, admission: AuthorityAdmission) {
        if let Ok(mut state) = self.state.lock() {
            if state.acknowledged
                || state.abandoned
                || self.abandon_requested.load(Ordering::Acquire)
            {
                state.abandoned |= self.abandon_requested.load(Ordering::Acquire);
                return;
            }
            let Some(previous) = state.progress.as_ref() else {
                return;
            };
            let progress = Arc::new(RequestProgress {
                id: self.id,
                phase: RequestPhase::AuthorityAccepted,
                authority: Some(admission),
                terminal: None,
                _credit: Arc::clone(&previous._credit),
            });
            state.progress = Some(progress);
            if self.abandon_requested.load(Ordering::Acquire) {
                state.abandoned = true;
                state.progress = None;
            }
        }
        self.changed.notify_all();
    }

    fn set_terminal(&self, terminal: Result<SessionRequestResult, SessionRequestError>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            if self.abandon_requested.load(Ordering::Acquire) {
                state.abandoned = true;
                state.progress = None;
            }
            let credit = state
                .progress
                .as_ref()
                .map(|progress| Arc::clone(&progress._credit));
            if credit.is_none() {
                return;
            }
            if state.acknowledged || state.abandoned {
                state.progress = None;
                return;
            }
            let previous = state.progress.as_ref().expect("checked progress");
            if previous.terminal.is_some() {
                return;
            }
            let progress = Arc::new(RequestProgress {
                id: self.id,
                phase: RequestPhase::Terminal,
                authority: previous.authority.clone(),
                terminal: Some(terminal),
                _credit: credit.expect("checked credit"),
            });
            state.progress = Some(progress);
            if !self.terminal_unacked_counted.swap(true, Ordering::AcqRel) {
                self.counters
                    .terminal_unacked
                    .fetch_add(1, Ordering::AcqRel);
            }
            if self.abandon_requested.load(Ordering::Acquire) {
                state.abandoned = true;
                state.progress = None;
                if self.terminal_unacked_counted.swap(false, Ordering::AcqRel) {
                    self.counters
                        .terminal_unacked
                        .fetch_sub(1, Ordering::AcqRel);
                }
            }
        }
        self.changed.notify_all();
    }

    fn acknowledge(&self) -> Result<(), RequestAckError> {
        if self.abandon_requested.load(Ordering::Acquire) {
            return Err(RequestReadError::Abandoned.into());
        }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(RequestAckError::Busy),
            Err(TryLockError::Poisoned(_)) => {
                return Err(RequestReadError::Poisoned.into());
            }
        };
        if state.acknowledged {
            return Err(RequestReadError::Acknowledged.into());
        }
        if state.abandoned {
            return Err(RequestReadError::Abandoned.into());
        }
        let progress = state.progress.as_ref().ok_or(RequestReadError::Abandoned)?;
        if progress.terminal.is_none() {
            return Err(RequestAckError::NotTerminal);
        }
        state.acknowledged = true;
        state.progress = None;
        if self.terminal_unacked_counted.swap(false, Ordering::AcqRel) {
            self.counters
                .terminal_unacked
                .fetch_sub(1, Ordering::AcqRel);
        }
        self.changed.notify_all();
        Ok(())
    }

    fn abandon(&self) {
        if self.abandon_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        self.counters.abandoned.fetch_add(1, Ordering::AcqRel);
        if self.terminal_unacked_counted.swap(false, Ordering::AcqRel) {
            self.counters
                .terminal_unacked
                .fetch_sub(1, Ordering::AcqRel);
        }
        if let Ok(mut state) = self.state.try_lock()
            && !state.acknowledged
            && !state.abandoned
        {
            state.abandoned = true;
            state.progress = None;
        }
        self.changed.notify_all();
    }
}

/// An owned capability to exactly one request's progress and terminal result.
pub struct RequestTicket {
    cell: Arc<RequestCell>,
    active: bool,
}

impl RequestTicket {
    #[must_use]
    pub fn id(&self) -> RequestTicketId {
        self.cell.id
    }

    pub fn try_state(&self) -> Result<Arc<RequestProgress>, RequestReadError> {
        self.cell.progress()
    }

    pub fn wait_for(
        &self,
        stage: WaitStage,
        timeout: Duration,
    ) -> Result<Arc<RequestProgress>, RequestWaitError> {
        let Some(deadline) = Instant::now().checked_add(timeout) else {
            return Err(RequestWaitError::TimedOut);
        };
        let mut state = self
            .cell
            .state
            .lock()
            .map_err(|_| RequestReadError::Poisoned)?;
        loop {
            if state.acknowledged {
                return Err(RequestReadError::Acknowledged.into());
            }
            if state.abandoned {
                return Err(RequestReadError::Abandoned.into());
            }
            let progress = state.progress.as_ref().ok_or(RequestReadError::Abandoned)?;
            let ready = match stage {
                WaitStage::AuthorityAccepted => {
                    progress.authority.is_some() || progress.terminal.is_some()
                }
                WaitStage::Terminal => progress.terminal.is_some(),
            };
            if ready {
                return Ok(Arc::clone(progress));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RequestWaitError::TimedOut);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, timeout_result) = self
                .cell
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| RequestReadError::Poisoned)?;
            state = next;
            if timeout_result.timed_out() {
                return Err(RequestWaitError::TimedOut);
            }
        }
    }

    pub fn acknowledge(&mut self) -> Result<(), RequestAckError> {
        self.cell.acknowledge()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RequestTicket {
    fn drop(&mut self) {
        if self.active {
            self.cell.abandon();
        }
    }
}

struct ClientAttachment {
    id: SessionClientId,
    active: AtomicBool,
    capabilities: SessionCapabilities,
    next_ticket: AtomicU64,
    normal_credits: CreditPool,
    step_credits: CreditPool,
}

struct QueuedRequest {
    enqueue_sequence: u64,
    operation: SessionOperation,
    cell: Arc<RequestCell>,
    credit: Arc<CreditBundle>,
}

struct QueueState {
    next_enqueue_sequence: u64,
    normal: VecDeque<QueuedRequest>,
    manual_step: Option<QueuedRequest>,
}

struct RequestQueue {
    state: Mutex<QueueState>,
    available: Condvar,
    capacity: usize,
}

impl RequestQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                next_enqueue_sequence: 1,
                normal: VecDeque::new(),
                manual_step: None,
            }),
            available: Condvar::new(),
            capacity,
        }
    }

    fn pop(&self, timeout: Duration) -> Option<QueuedRequest> {
        let state = self.state.lock().ok()?;
        let (mut state, _) = self
            .available
            .wait_timeout_while(state, timeout, |state| {
                state.normal.is_empty() && state.manual_step.is_none()
            })
            .ok()?;
        match (state.normal.front(), state.manual_step.as_ref()) {
            (Some(normal), Some(step)) if step.enqueue_sequence < normal.enqueue_sequence => {
                state.manual_step.take()
            }
            (Some(_), _) => state.normal.pop_front(),
            (None, Some(_)) => state.manual_step.take(),
            (None, None) => None,
        }
    }

    fn drain(&self) -> Vec<QueuedRequest> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut pending = state.normal.drain(..).collect::<Vec<_>>();
        if let Some(step) = state.manual_step.take() {
            pending.push(step);
        }
        pending.sort_by_key(|request| request.enqueue_sequence);
        pending
    }

    fn wake(&self) {
        self.available.notify_all();
    }
}

struct ControlShared {
    session: SessionId,
    clock_mode: SessionClockMode,
    limits: SessionControlLimits,
    accepting: AtomicBool,
    stop: StopSignal,
    queue: RequestQueue,
    frames: Arc<LatestValueMailbox<SessionFrame>>,
    normal_credits: CreditPool,
    step_credits: CreditPool,
    active_clients: AtomicUsize,
    next_client_id: AtomicU64,
    counters: Arc<ControlCounters>,
    #[cfg(test)]
    panic_before_next_request: AtomicBool,
    #[cfg(test)]
    panic_after_next_engine_step: AtomicBool,
    #[cfg(test)]
    pause_after_next_request_before_cleanup: AtomicBool,
    #[cfg(test)]
    request_cleanup_paused: AtomicBool,
    #[cfg(test)]
    release_paused_request_cleanup: AtomicBool,
}

impl ControlShared {
    fn request_stop(&self) {
        self.accepting.store(false, Ordering::Release);
        self.stop.request();
        self.queue.wake();
    }
}

/// Owns one project actor. Dropping the owner requests stop without joining; use `shutdown` to
/// obtain the bounded terminal report and join outside UI/event callbacks.
pub struct SessionControlService {
    shared: Arc<ControlShared>,
    join: Option<JoinHandle<SessionShutdownReport>>,
}

impl SessionControlService {
    pub fn start(
        prepared: PreparedProject,
        mut config: SessionControlConfig,
    ) -> Result<Self, SessionControlStartError> {
        config.limits.validate(&config.engine)?;
        if prepared.entities().len() > MAX_PROJECT_SESSION_ENTITIES {
            return Err(SessionControlStartError::InvalidConfig(
                "prepared project exceeds the 16384 runtime entity cap".to_owned(),
            ));
        }
        if config.engine.snapshot_entity_capacity < prepared.entities().len() {
            return Err(SessionControlStartError::InvalidConfig(
                "snapshot_entity_capacity must cover all authored project entities".to_owned(),
            ));
        }
        config.engine.snapshot_entity_capacity = config
            .engine
            .snapshot_entity_capacity
            .min(MAX_PROJECT_SESSION_ENTITIES);
        let session = config.engine.session;
        let normal_result_bytes = config
            .limits
            .max_result_bytes_total
            .saturating_sub(MAX_RESULT_BYTES_PER_TICKET);
        let shared = Arc::new(ControlShared {
            session,
            clock_mode: config.clock_mode,
            limits: config.limits,
            accepting: AtomicBool::new(true),
            stop: StopSignal::default(),
            queue: RequestQueue::new(config.limits.command_capacity),
            frames: Arc::new(LatestValueMailbox::new(config.limits.max_retained_frames)),
            normal_credits: CreditPool::new(CreditLimits {
                tickets: config.limits.max_pending_total - 1,
                bytes: normal_result_bytes,
            }),
            step_credits: CreditPool::new(CreditLimits {
                tickets: 1,
                bytes: MAX_RESULT_BYTES_PER_TICKET,
            }),
            active_clients: AtomicUsize::new(0),
            next_client_id: AtomicU64::new(1),
            counters: Arc::new(ControlCounters {
                terminal_unacked: AtomicUsize::new(0),
                abandoned: AtomicUsize::new(0),
                queued_stopped: AtomicUsize::new(0),
                commands_stopped_not_applied: AtomicUsize::new(0),
            }),
            #[cfg(test)]
            panic_before_next_request: AtomicBool::new(false),
            #[cfg(test)]
            panic_after_next_engine_step: AtomicBool::new(false),
            #[cfg(test)]
            pause_after_next_request_before_cleanup: AtomicBool::new(false),
            #[cfg(test)]
            request_cleanup_paused: AtomicBool::new(false),
            #[cfg(test)]
            release_paused_request_cleanup: AtomicBool::new(false),
        });
        let (startup_sender, startup_receiver) = std::sync::mpsc::sync_channel(1);
        let actor_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("daedalus-project-control".to_owned())
            .spawn(move || run_control_actor(prepared, config, actor_shared, startup_sender))
            .map_err(|error| SessionControlStartError::ThreadSpawn(error.to_string()))?;
        match startup_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error.into());
            }
            Err(_) => {
                let _ = join.join();
                return Err(SessionControlStartError::StartupDisconnected);
            }
        }
        Ok(Self {
            shared,
            join: Some(join),
        })
    }

    pub fn attach(&self) -> Result<SessionControlClient, SessionAttachError> {
        self.attach_with_capabilities(SessionCapabilities::trusted_local())
    }

    pub fn attach_with_capabilities(
        &self,
        capabilities: SessionCapabilities,
    ) -> Result<SessionControlClient, SessionAttachError> {
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(SessionAttachError::Stopped);
        }
        loop {
            let active = self.shared.active_clients.load(Ordering::Acquire);
            if active >= self.shared.limits.max_clients {
                return Err(SessionAttachError::ClientLimit);
            }
            if self
                .shared
                .active_clients
                .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        let client_id = self
            .shared
            .next_client_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| {
                self.shared.active_clients.fetch_sub(1, Ordering::AcqRel);
                SessionAttachError::IdExhausted
            })?;
        if !self.shared.accepting.load(Ordering::Acquire) {
            self.shared.active_clients.fetch_sub(1, Ordering::AcqRel);
            return Err(SessionAttachError::Stopped);
        }
        let per_client_count = self.shared.limits.max_pending_per_client;
        let normal_count = per_client_count - 1;
        let client_bytes = normal_count.saturating_mul(MAX_RESULT_BYTES_PER_TICKET);
        Ok(SessionControlClient {
            shared: Arc::clone(&self.shared),
            attachment: Arc::new(ClientAttachment {
                id: SessionClientId(client_id),
                active: AtomicBool::new(true),
                capabilities,
                next_ticket: AtomicU64::new(1),
                normal_credits: CreditPool::new(CreditLimits {
                    tickets: normal_count,
                    bytes: client_bytes,
                }),
                step_credits: CreditPool::new(CreditLimits {
                    tickets: 1,
                    bytes: MAX_RESULT_BYTES_PER_TICKET,
                }),
            }),
        })
    }

    pub fn request_stop(&self) {
        self.shared.request_stop();
    }

    pub fn shutdown(mut self) -> SessionShutdownReport {
        self.shared.request_stop();
        self.join
            .take()
            .expect("session-control actor join handle")
            .join()
            .unwrap_or_else(|payload| {
                shutdown_report(
                    &self.shared,
                    SessionTermination::Panicked(panic_message(payload)),
                    Tick(0),
                )
            })
    }
}

impl Drop for SessionControlService {
    fn drop(&mut self) {
        self.shared.request_stop();
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SessionAttachError {
    #[error("maximum session client attachments reached")]
    ClientLimit,
    #[error("session is stopping or stopped")]
    Stopped,
    #[error("client identifier space is exhausted")]
    IdExhausted,
}

/// One independent admission endpoint. Dropping it detaches new submission but owned tickets
/// remain readable until ack/drop; there is deliberately no reconnect/resume API in this phase.
pub struct SessionControlClient {
    shared: Arc<ControlShared>,
    attachment: Arc<ClientAttachment>,
}

impl SessionControlClient {
    #[must_use]
    pub fn id(&self) -> SessionClientId {
        self.attachment.id
    }

    pub fn try_submit(
        &self,
        operation: SessionOperation,
    ) -> Result<RequestTicket, SessionSubmitError> {
        if !self.attachment.active.load(Ordering::Acquire) {
            return Err(SessionSubmitError::Detached);
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            return Err(SessionSubmitError::Stopped);
        }
        if operation.needs_inspect() && !self.attachment.capabilities.inspect
            || operation.needs_mutate() && !self.attachment.capabilities.mutate
            || operation.is_step() && !self.attachment.capabilities.manual_step
        {
            return Err(SessionSubmitError::CapabilityDenied);
        }
        validate_operation(&operation, self.shared.clock_mode, self.shared.limits)?;
        if let SessionOperation::SetTransform {
            command_id: Some(id),
            ..
        } = &operation
            && id.get() >= MAX_CALLER_COMMAND_ID
        {
            return Err(SessionSubmitError::ReservedCommandId);
        }
        let mut queue = self
            .shared
            .queue
            .state
            .try_lock()
            .map_err(|error| match error {
                TryLockError::WouldBlock => SessionSubmitError::Busy,
                TryLockError::Poisoned(_) => SessionSubmitError::Stopped,
            })?;
        if !self.attachment.active.load(Ordering::Acquire)
            || !self.shared.accepting.load(Ordering::Acquire)
        {
            return Err(SessionSubmitError::Stopped);
        }
        let is_step = operation.is_step();
        let queue_used = queue.normal.len() + usize::from(queue.manual_step.is_some());
        if queue_used >= self.shared.queue.capacity
            || is_step && queue.manual_step.is_some()
            || !is_step && queue.normal.len() >= self.shared.queue.capacity - 1
        {
            return Err(SessionSubmitError::QueueFull);
        }
        let global_credit = if is_step {
            self.shared
                .step_credits
                .try_reserve(MAX_RESULT_BYTES_PER_TICKET)
        } else {
            self.shared
                .normal_credits
                .try_reserve(MAX_RESULT_BYTES_PER_TICKET)
        }
        .map_err(map_credit_error)?;
        let client_credit = if is_step {
            self.attachment
                .step_credits
                .try_reserve(MAX_RESULT_BYTES_PER_TICKET)
        } else {
            self.attachment
                .normal_credits
                .try_reserve(MAX_RESULT_BYTES_PER_TICKET)
        }
        .map_err(map_credit_error)?;
        let sequence = self
            .attachment
            .next_ticket
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| SessionSubmitError::TicketIdExhausted)?;
        let Some(next_enqueue_sequence) = queue.next_enqueue_sequence.checked_add(1) else {
            return Err(SessionSubmitError::Busy);
        };
        let id = RequestTicketId {
            session: self.shared.session,
            client: self.attachment.id,
            sequence,
        };
        let credit = Arc::new(CreditBundle {
            _session: global_credit,
            _client: client_credit,
        });
        let cell = Arc::new(RequestCell::new(
            id,
            Arc::clone(&credit),
            Arc::clone(&self.shared.counters),
        ));
        let enqueue_sequence = queue.next_enqueue_sequence;
        queue.next_enqueue_sequence = next_enqueue_sequence;
        let request = QueuedRequest {
            enqueue_sequence,
            operation,
            cell: Arc::clone(&cell),
            credit,
        };
        if is_step {
            queue.manual_step = Some(request);
        } else {
            queue.normal.push_back(request);
        }
        drop(queue);
        self.shared.queue.available.notify_one();
        Ok(RequestTicket { cell, active: true })
    }

    pub fn try_latest_frame(&self) -> Result<Option<Arc<SessionFrame>>, FrameReadError> {
        self.shared
            .frames
            .try_latest()
            .map_err(|error| match error {
                MailboxError::Busy => FrameReadError::Busy,
                MailboxError::Poisoned | MailboxError::AtCapacity => FrameReadError::Poisoned,
            })
    }
}

impl Drop for SessionControlClient {
    fn drop(&mut self) {
        if self.attachment.active.swap(false, Ordering::AcqRel) {
            self.shared.active_clients.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn map_credit_error(_error: CreditError) -> SessionSubmitError {
    SessionSubmitError::ResultCapacity
}

fn validate_operation(
    operation: &SessionOperation,
    mode: SessionClockMode,
    limits: SessionControlLimits,
) -> Result<(), SessionSubmitError> {
    match operation {
        SessionOperation::ProjectInfo { limit, .. }
        | SessionOperation::ListTransforms { limit, .. } => {
            if limit.is_some_and(|limit| limit == 0 || limit > limits.max_page_entities) {
                return Err(SessionSubmitError::InvalidRequest);
            }
        }
        SessionOperation::SpawnTransform { transform }
        | SessionOperation::SetTransform { transform, .. } => {
            if !transform.is_finite() {
                return Err(SessionSubmitError::InvalidRequest);
            }
        }
        SessionOperation::Step { count } => {
            if mode != SessionClockMode::Manual {
                return Err(SessionSubmitError::ClockModeMismatch);
            }
            if *count == 0 || *count > limits.max_steps_per_request {
                return Err(SessionSubmitError::InvalidRequest);
            }
        }
        SessionOperation::GetTransform { .. } | SessionOperation::Status => {}
    }
    Ok(())
}

struct PendingTicket {
    cell: Arc<RequestCell>,
    credit: Arc<CreditBundle>,
}

struct PendingCommand {
    apply_tick: Tick,
    tickets: Vec<PendingTicket>,
}

#[derive(Clone)]
struct ReplayEntry {
    apply_tick: Option<Tick>,
    resolve_tick: Option<Tick>,
    result: CommandResult,
}

struct ControlActor {
    session: ProjectSession,
    config: SessionControlConfig,
    shared: Arc<ControlShared>,
    last_advance: Instant,
    last_published_sequence: u64,
    next_server_command_id: u128,
    caller_command_high_water: Option<u128>,
    pending_commands: HashMap<CommandId, PendingCommand>,
    completed_replay: HashMap<CommandId, ReplayEntry>,
    replay_order: VecDeque<CommandId>,
    active_request: Option<ActiveRequest>,
}

fn run_control_actor(
    prepared: PreparedProject,
    config: SessionControlConfig,
    shared: Arc<ControlShared>,
    startup_sender: std::sync::mpsc::SyncSender<Result<(), ProjectSessionStartError>>,
) -> SessionShutdownReport {
    let session = match ProjectSession::new(prepared, config.engine.clone()) {
        Ok(session) => session,
        Err(error) => {
            shared.request_stop();
            let _ = startup_sender.send(Err(error));
            return shutdown_report(
                &shared,
                SessionTermination::EngineError("project session startup failed".to_owned()),
                Tick(0),
            );
        }
    };
    let _ = startup_sender.send(Ok(()));
    let mut actor = ControlActor {
        session,
        config,
        shared: Arc::clone(&shared),
        last_advance: Instant::now(),
        last_published_sequence: 0,
        next_server_command_id: 1,
        caller_command_high_water: None,
        pending_commands: HashMap::new(),
        completed_replay: HashMap::new(),
        replay_order: VecDeque::new(),
        active_request: None,
    };

    let termination = match catch_unwind(AssertUnwindSafe(|| actor.run_loop())) {
        Ok(Ok(())) => SessionTermination::StopRequested,
        Ok(Err(error)) => SessionTermination::EngineError(error),
        Err(payload) => SessionTermination::Panicked(panic_message(payload)),
    };
    shared.request_stop();

    for request in shared.queue.drain() {
        shared
            .counters
            .queued_stopped
            .fetch_add(1, Ordering::AcqRel);
        drop(request.credit);
        request
            .cell
            .set_terminal(Err(SessionRequestError::StoppedBeforeAuthority));
    }

    let abnormal = termination != SessionTermination::StopRequested;
    if let Some(active) = actor.active_request.take() {
        let error = active.possible_command_id.map_or_else(
            || SessionRequestError::ActorFailed(termination_message(&termination)),
            |command_id| SessionRequestError::OutcomeUnknown { command_id },
        );
        active.cell.set_terminal(Err(error));
    }

    let pending = std::mem::take(&mut actor.pending_commands);
    for (command_id, pending) in pending {
        if !abnormal {
            shared
                .counters
                .commands_stopped_not_applied
                .fetch_add(1, Ordering::AcqRel);
        }
        for ticket in pending.tickets {
            let error = if abnormal {
                SessionRequestError::OutcomeUnknown { command_id }
            } else {
                SessionRequestError::StoppedNotApplied { command_id }
            };
            drop(ticket.credit);
            ticket.cell.set_terminal(Err(error));
        }
    }

    if let Err(error) = actor.session.engine_mut().shutdown()
        && termination == SessionTermination::StopRequested
    {
        return shutdown_report(
            &shared,
            SessionTermination::EngineError(error.to_string()),
            actor.session.engine().clock().current_tick(),
        );
    }
    shutdown_report(
        &shared,
        termination,
        actor.session.engine().clock().current_tick(),
    )
}

impl ControlActor {
    fn run_loop(&mut self) -> Result<(), String> {
        loop {
            if self.shared.stop.is_requested() {
                return Ok(());
            }
            if let Some(request) = self.shared.queue.pop(ACTOR_POLL_INTERVAL) {
                self.active_request = Some(ActiveRequest {
                    cell: Arc::clone(&request.cell),
                    possible_command_id: None,
                });
                #[cfg(test)]
                if self
                    .shared
                    .panic_before_next_request
                    .swap(false, Ordering::AcqRel)
                {
                    panic!("injected actor request failure");
                }
                self.process_request(request);
                #[cfg(test)]
                self.pause_after_request_before_cleanup_for_test();
                self.active_request = None;
            }
            if self.shared.stop.is_requested() {
                return Ok(());
            }
            if self.config.clock_mode == SessionClockMode::Realtime {
                let now = Instant::now();
                let elapsed = now.saturating_duration_since(self.last_advance);
                self.last_advance = now;
                let (_, reports) = self
                    .session
                    .engine_mut()
                    .advance_elapsed(elapsed)
                    .map_err(|error| error.to_string())?;
                for report in reports {
                    self.process_step_report(&report);
                    self.publish_latest_frame();
                }
            }
        }
    }

    fn process_request(&mut self, request: QueuedRequest) {
        let QueuedRequest {
            operation,
            cell,
            credit,
            ..
        } = request;
        match operation {
            SessionOperation::ProjectInfo { offset, limit } => {
                let result = self.project_info_page(offset, limit);
                self.finish_cell(cell, credit, result.map(SessionRequestResult::ProjectInfo));
            }
            SessionOperation::SpawnTransform { transform } => {
                let world = self.session.engine().world();
                if world.entity_count() >= MAX_PROJECT_SESSION_ENTITIES {
                    self.finish_cell(cell, credit, Err(SessionRequestError::EntityLimit));
                    return;
                }
                let result = self
                    .session
                    .engine_mut()
                    .spawn_transform(transform)
                    .map(|handle| SessionRequestResult::Spawned {
                        handle,
                        entity_count: self.session.engine().world().entity_count(),
                    })
                    .map_err(|error| SessionRequestError::SpawnFailed(error.to_string()));
                self.finish_cell(cell, credit, result);
            }
            SessionOperation::GetTransform { target } => {
                let result = self
                    .get_transform(target)
                    .map(SessionRequestResult::Transform);
                self.finish_cell(cell, credit, result);
            }
            SessionOperation::ListTransforms { offset, limit } => {
                let result = self
                    .list_transforms(offset, limit)
                    .map(SessionRequestResult::Transforms);
                self.finish_cell(cell, credit, result);
            }
            SessionOperation::SetTransform {
                target,
                command_id,
                expected_revision,
                apply_at,
                transform,
            } => self.submit_transform(
                cell,
                credit,
                target,
                command_id,
                expected_revision,
                apply_at,
                transform,
            ),
            SessionOperation::Step { count } => self.step(count, cell, credit),
            SessionOperation::Status => {
                let status = SessionStatus {
                    session: self.session.engine().world().session(),
                    clock: self.config.clock_mode,
                    tick: self.session.engine().clock().current_tick(),
                    entity_count: self.session.engine().world().entity_count(),
                    pending_commands: self.session.engine().world().pending_command_count(),
                    active_clients: self.shared.active_clients.load(Ordering::Acquire),
                    stopping: self.shared.stop.is_requested(),
                };
                self.finish_cell(cell, credit, Ok(SessionRequestResult::Status(status)));
            }
        }
    }

    fn project_info_page(
        &self,
        offset: usize,
        requested_limit: Option<usize>,
    ) -> Result<ProjectInfoPage, SessionRequestError> {
        let project = self.session.project();
        let total = project.entities().len();
        let start = offset.min(total);
        let limit = requested_limit.unwrap_or(self.config.limits.max_page_entities);
        let end = start.saturating_add(limit).min(total);

        // Preflight the complete page plus the bounded source-file manifest before cloning any
        // source strings or entity identifiers into the response.
        let mut bytes = std::mem::size_of::<ProjectInfoPage>()
            .saturating_add(project.manifest().name.len())
            .saturating_add(project.digest().len());
        for file in project.files() {
            bytes = bytes
                .checked_add(std::mem::size_of::<ProjectFileIdentity>())
                .and_then(|bytes| bytes.checked_add(file.source_id.len()))
                .and_then(|bytes| bytes.checked_add(file.content_sha256.len()))
                .ok_or(SessionRequestError::ResultTooLarge)?;
        }
        for entity in &project.entities()[start..end] {
            bytes = bytes
                .checked_add(std::mem::size_of::<ProjectEntityBinding>())
                .and_then(|bytes| bytes.checked_add(entity.id.as_str().len()))
                .ok_or(SessionRequestError::ResultTooLarge)?;
        }
        if project.files().len() > MAX_SOURCE_FILES || bytes > MAX_RESULT_BYTES_PER_TICKET {
            return Err(SessionRequestError::ResultTooLarge);
        }

        let entities = project.entities()[start..end]
            .iter()
            .map(|entity| {
                self.session
                    .entity_handle(&entity.id)
                    .map(|handle| ProjectEntityBinding {
                        id: entity.id.clone(),
                        handle,
                    })
                    .ok_or_else(|| SessionRequestError::MissingBinding(entity.id.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_offset = (end < total).then_some(end);
        Ok(ProjectInfoPage {
            session: self.session.engine().world().session(),
            tick: self.session.engine().clock().current_tick(),
            name: project.manifest().name.clone(),
            schema_version: project.scene().schema_version,
            source_scene_revision: project.scene().revision,
            source_digest: project.digest().to_owned(),
            source_files: project.files().to_vec(),
            entity_count: total,
            offset: start,
            limit,
            next_offset,
            entities,
        })
    }

    fn resolve_handle(
        &self,
        target: SessionEntityTarget,
    ) -> Result<EntityHandle, SessionRequestError> {
        match target {
            SessionEntityTarget::StableId(id) => self
                .session
                .entity_handle(&id)
                .ok_or(SessionRequestError::UnknownStableId(id)),
            SessionEntityTarget::Handle(handle) => Ok(handle),
        }
    }

    fn get_transform(
        &self,
        target: SessionEntityTarget,
    ) -> Result<TransformState, SessionRequestError> {
        let handle = self.resolve_handle(target)?;
        let (transform, revision) = self.session.engine().world().transform(handle)?;
        let state = SessionEntityState {
            stable_id: self.session.stable_id_for_handle(handle).cloned(),
            handle,
            transform,
            revision,
        };
        Ok(TransformState {
            observed_tick: self.session.engine().clock().current_tick(),
            entity: state,
        })
    }

    fn list_transforms(
        &mut self,
        offset: usize,
        requested_limit: Option<usize>,
    ) -> Result<TransformPage, SessionRequestError> {
        let limit = requested_limit.unwrap_or(self.config.limits.max_page_entities);
        let page = self
            .session
            .engine_mut()
            .world_mut()
            .transform_page(offset, limit)
            .map_err(|_| SessionRequestError::ResultTooLarge)?;
        let entities = page
            .entities
            .into_iter()
            .map(
                |WorldTransformState {
                     handle,
                     transform,
                     revision,
                 }| SessionEntityState {
                    stable_id: self.session.stable_id_for_handle(handle).cloned(),
                    handle,
                    transform,
                    revision,
                },
            )
            .collect::<Vec<_>>();
        let end = page.offset.saturating_add(entities.len());
        Ok(TransformPage {
            observed_tick: self.session.engine().clock().current_tick(),
            total: page.total,
            offset: page.offset,
            limit,
            next_offset: (end < page.total).then_some(end),
            entities,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_transform(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        target: SessionEntityTarget,
        requested_id: Option<CommandId>,
        expected_revision: Revision,
        requested_apply_tick: Option<Tick>,
        transform: Transform,
    ) {
        let (command_id, caller_id) = match requested_id {
            Some(id) => (id, true),
            None => match self.next_server_command_id() {
                Some(id) => (id, false),
                None => {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ActorFailed(
                            "server command identifier space is exhausted".to_owned(),
                        )),
                    );
                    return;
                }
            },
        };

        if caller_id {
            if let Some(pending) = self.pending_commands.get_mut(&command_id) {
                cell.set_authority(AuthorityAdmission {
                    command_id,
                    submission: CommandSubmission::DuplicatePending,
                    apply_tick: Some(pending.apply_tick),
                });
                pending.tickets.push(PendingTicket { cell, credit });
                return;
            }
            if let Some(replay) = self.completed_replay.get(&command_id).cloned() {
                cell.set_authority(AuthorityAdmission {
                    command_id,
                    submission: CommandSubmission::DuplicateCompleted(replay.result.clone()),
                    apply_tick: replay.apply_tick,
                });
                self.finish_cell(
                    cell,
                    credit,
                    Ok(SessionRequestResult::Command(CommandTerminalResult {
                        result: replay.result,
                        resolve_tick: replay.resolve_tick,
                    })),
                );
                return;
            }
            if let Some(high_water) = self.caller_command_high_water
                && command_id.get() <= high_water
            {
                self.finish_cell(
                    cell,
                    credit,
                    Err(SessionRequestError::ReplayWindowExpired {
                        command_id: command_id.get(),
                        high_water,
                    }),
                );
                return;
            }
        }

        let target = match self.resolve_handle(target) {
            Ok(target) => target,
            Err(error) => {
                self.finish_cell(cell, credit, Err(error));
                return;
            }
        };
        let current_tick = self.session.engine().clock().current_tick();
        let apply_tick = match requested_apply_tick {
            Some(tick) => tick,
            None => match current_tick.0.checked_add(1) {
                Some(tick) => Tick(tick),
                None => {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::CommandRejected(
                            "no future simulation tick exists".to_owned(),
                        )),
                    );
                    return;
                }
            },
        };
        let command = WorldCommand {
            command_id,
            target,
            expected_revision,
            apply_at: apply_tick,
            kind: WorldCommandKind::SetTransform(transform),
        };
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = Some(command_id);
        }
        let submission = match self.session.engine_mut().try_command(command) {
            Ok(submission) => submission,
            Err(error) => {
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = None;
                }
                self.finish_cell(
                    cell,
                    credit,
                    Err(SessionRequestError::CommandRejected(error.to_string())),
                );
                return;
            }
        };
        if caller_id {
            self.caller_command_high_water = Some(
                self.caller_command_high_water
                    .map_or(command_id.get(), |current| current.max(command_id.get())),
            );
        }

        match submission {
            CommandSubmission::Enqueued => {
                cell.set_authority(AuthorityAdmission {
                    command_id,
                    submission: CommandSubmission::Enqueued,
                    apply_tick: Some(apply_tick),
                });
                self.pending_commands.insert(
                    command_id,
                    PendingCommand {
                        apply_tick,
                        tickets: vec![PendingTicket { cell, credit }],
                    },
                );
            }
            CommandSubmission::DuplicatePending => {
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = None;
                }
                // All commands entered through this actor have a request record. If this
                // invariant breaks, do not claim a terminal result that cannot be recovered.
                self.finish_cell(
                    cell,
                    credit,
                    Err(SessionRequestError::UnknownPendingCommand(command_id)),
                );
            }
            CommandSubmission::DuplicateCompleted(result) => {
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = None;
                }
                cell.set_authority(AuthorityAdmission {
                    command_id,
                    submission: CommandSubmission::DuplicateCompleted(result.clone()),
                    apply_tick: None,
                });
                self.finish_cell(
                    cell,
                    credit,
                    Ok(SessionRequestResult::Command(CommandTerminalResult {
                        result,
                        resolve_tick: None,
                    })),
                );
            }
        }
    }

    fn next_server_command_id(&mut self) -> Option<CommandId> {
        if self.next_server_command_id >= MAX_CALLER_COMMAND_ID {
            return None;
        }
        let raw = SERVER_COMMAND_ID_PREFIX | self.next_server_command_id;
        self.next_server_command_id = self.next_server_command_id.checked_add(1)?;
        Some(CommandId::from_raw(raw))
    }

    fn step(&mut self, count: u32, cell: Arc<RequestCell>, credit: Arc<CreditBundle>) {
        let estimate = usize::try_from(count)
            .ok()
            .and_then(|count| count.checked_mul(std::mem::size_of::<StepReport>()))
            .and_then(|bytes| {
                self.config
                    .engine
                    .command_capacity
                    .checked_mul(std::mem::size_of::<CommandResult>())
                    .and_then(|commands| bytes.checked_add(commands))
            })
            .and_then(|bytes| bytes.checked_add(512));
        if estimate.is_none_or(|bytes| bytes > MAX_RESULT_BYTES_PER_TICKET) {
            self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
            return;
        }
        let mut reports = Vec::with_capacity(count as usize);
        for _ in 0..count {
            match self.session.step_tick() {
                Ok(report) => {
                    #[cfg(test)]
                    if self
                        .shared
                        .panic_after_next_engine_step
                        .swap(false, Ordering::AcqRel)
                    {
                        panic!("injected actor failure after Engine step");
                    }
                    self.process_step_report(&report);
                    reports.push(report);
                    self.publish_latest_frame();
                }
                Err(error) => {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::StepFailed {
                            completed_ticks: reports.len(),
                            message: error.to_string(),
                        }),
                    );
                    return;
                }
            }
        }
        self.finish_cell(cell, credit, Ok(SessionRequestResult::Step(reports)));
    }

    fn process_step_report(&mut self, report: &StepReport) {
        for result in &report.commands {
            let Some(pending) = self.pending_commands.remove(&result.command_id) else {
                continue;
            };
            let terminal = SessionRequestResult::Command(CommandTerminalResult {
                result: result.clone(),
                resolve_tick: Some(report.tick),
            });
            self.remember_completed(
                result.command_id,
                ReplayEntry {
                    apply_tick: Some(pending.apply_tick),
                    resolve_tick: Some(report.tick),
                    result: result.clone(),
                },
            );
            for ticket in pending.tickets {
                self.finish_cell(ticket.cell, ticket.credit, Ok(terminal.clone()));
            }
        }
    }

    fn remember_completed(&mut self, id: CommandId, entry: ReplayEntry) {
        if self.completed_replay.contains_key(&id) {
            self.replay_order.retain(|candidate| *candidate != id);
        }
        self.completed_replay.insert(id, entry);
        self.replay_order.push_back(id);
        while self.completed_replay.len() > self.config.limits.replay_ledger_capacity {
            if let Some(oldest) = self.replay_order.pop_front() {
                self.completed_replay.remove(&oldest);
            } else {
                break;
            }
        }
    }

    fn finish_cell(
        &self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        terminal: Result<SessionRequestResult, SessionRequestError>,
    ) {
        let terminal = if terminal
            .as_ref()
            .ok()
            .is_some_and(|result| estimate_result_bytes(result) > MAX_RESULT_BYTES_PER_TICKET)
        {
            Err(SessionRequestError::ResultTooLarge)
        } else {
            terminal
        };
        // The request progress owns the result lease through terminal acknowledgement and
        // any reader Arc. Drop this bookkeeping reference before publishing terminal state,
        // so a woken caller can immediately reuse its credit after releasing those owners.
        drop(credit);
        cell.set_terminal(terminal);
    }

    #[cfg(test)]
    fn pause_after_request_before_cleanup_for_test(&self) {
        if !self
            .shared
            .pause_after_next_request_before_cleanup
            .swap(false, Ordering::AcqRel)
        {
            return;
        }
        self.shared
            .request_cleanup_paused
            .store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self
            .shared
            .release_paused_request_cleanup
            .load(Ordering::Acquire)
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        self.shared
            .request_cleanup_paused
            .store(false, Ordering::Release);
    }

    fn publish_latest_frame(&mut self) {
        let snapshot = match self.session.latest_snapshot() {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(_) => return,
        };
        if snapshot.sequence <= self.last_published_sequence {
            return;
        }
        let prepared = self.session.project();
        let frame = SessionFrame {
            session: self.session.engine().world().session(),
            tick: snapshot.tick,
            sequence: snapshot.sequence,
            project_generation: 1,
            source_digest: prepared.digest().to_owned(),
            source_scene_revision: prepared.scene().revision,
            snapshot,
        };
        let snapshot = Arc::downgrade(&frame.snapshot);
        let entities = Arc::downgrade(&frame.snapshot.entities);
        match self
            .shared
            .frames
            .try_publish_with_backing_probe(frame, move || {
                let snapshot = snapshot.upgrade();
                entities.upgrade().is_some_and(|entities| {
                    // A live RenderSnapshot owns one backing Arc; the probe upgrade owns another.
                    let internal_and_probe = 1 + usize::from(snapshot.is_some());
                    Arc::strong_count(&entities) > internal_and_probe
                })
            }) {
            Ok(()) => {
                self.last_published_sequence = self
                    .session
                    .latest_snapshot()
                    .ok()
                    .flatten()
                    .map_or(self.last_published_sequence, |snapshot| snapshot.sequence)
            }
            Err(MailboxError::AtCapacity | MailboxError::Busy) => {}
            Err(MailboxError::Poisoned) => {}
        }
    }
}

fn estimate_result_bytes(result: &SessionRequestResult) -> usize {
    match result {
        SessionRequestResult::ProjectInfo(page) => {
            let files = page.source_files.iter().fold(0_usize, |bytes, file| {
                bytes
                    .saturating_add(std::mem::size_of::<ProjectFileIdentity>())
                    .saturating_add(file.source_id.len())
                    .saturating_add(file.content_sha256.len())
            });
            page.entities.iter().fold(
                std::mem::size_of::<ProjectInfoPage>()
                    .saturating_add(page.name.len())
                    .saturating_add(page.source_digest.len())
                    .saturating_add(files),
                |bytes, entity| {
                    bytes
                        .saturating_add(std::mem::size_of::<ProjectEntityBinding>())
                        .saturating_add(entity.id.as_str().len())
                },
            )
        }
        SessionRequestResult::Spawned { .. } => std::mem::size_of::<EntityHandle>() + 128,
        SessionRequestResult::Transform(state) => {
            std::mem::size_of::<TransformState>()
                + state
                    .entity
                    .stable_id
                    .as_ref()
                    .map_or(0, |id| id.as_str().len())
        }
        SessionRequestResult::Transforms(page) => {
            page.entities
                .iter()
                .fold(std::mem::size_of::<TransformPage>(), |bytes, entity| {
                    bytes
                        .saturating_add(std::mem::size_of::<SessionEntityState>())
                        .saturating_add(entity.stable_id.as_ref().map_or(0, |id| id.as_str().len()))
                })
        }
        SessionRequestResult::Command(_) => std::mem::size_of::<CommandTerminalResult>() + 256,
        SessionRequestResult::Step(reports) => {
            reports
                .iter()
                .fold(std::mem::size_of::<Vec<StepReport>>(), |bytes, report| {
                    bytes
                        .saturating_add(std::mem::size_of::<StepReport>())
                        .saturating_add(
                            report
                                .commands
                                .len()
                                .saturating_mul(std::mem::size_of::<CommandResult>()),
                        )
                })
        }
        SessionRequestResult::Status(_) => std::mem::size_of::<SessionStatus>(),
    }
}

fn shutdown_report(
    shared: &ControlShared,
    termination: SessionTermination,
    final_tick: Tick,
) -> SessionShutdownReport {
    let normal = shared.normal_credits.usage();
    let step = shared.step_credits.usage();
    SessionShutdownReport {
        session: Some(shared.session),
        final_tick,
        termination,
        queued_requests_stopped: shared.counters.queued_stopped.load(Ordering::Acquire),
        commands_stopped_not_applied: shared
            .counters
            .commands_stopped_not_applied
            .load(Ordering::Acquire),
        terminal_unacknowledged: shared.counters.terminal_unacked.load(Ordering::Acquire),
        abandoned_tickets: shared.counters.abandoned.load(Ordering::Acquire),
        result_credit_tickets: normal.tickets.saturating_add(step.tickets),
        result_credit_bytes: normal.bytes.saturating_add(step.bytes),
    }
}

fn termination_message(termination: &SessionTermination) -> String {
    match termination {
        SessionTermination::StopRequested => "session stopped".to_owned(),
        SessionTermination::EngineError(message) => format!("Engine failed: {message}"),
        SessionTermination::Panicked(message) => format!("actor panicked: {message}"),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "session-control actor panicked with a non-string payload".to_owned()
    };
    message.chars().take(MAX_ERROR_TEXT_BYTES).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    use daedalus_content::project::{PreparedProject, ProjectLimits, prepare_project_from_bytes};
    use daedalus_content::scene::StableId;
    use daedalus_core::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};
    use daedalus_world::{CommandOutcome, CommandSubmission};

    use super::{
        MAX_PROJECT_SESSION_ENTITIES, ProjectInfoPage, RequestPhase, SessionCapabilities,
        SessionClockMode, SessionControlConfig, SessionControlLimits, SessionControlService,
        SessionEntityTarget, SessionOperation, SessionRequestError, SessionRequestResult,
        SessionSubmitError, SessionTermination, WaitStage,
    };
    use crate::EngineConfig;

    const ONE_ENTITY_SCENE: &str = r#"{
        "schema_version": 1,
        "revision": 37,
        "entities": [{
            "id": "entity:hero",
            "components": [{
                "id": "component:transform",
                "type_id": "daedalus.transform",
                "data": {
                    "translation": [2.0, 3.0, 4.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                }
            }]
        }],
        "assets": []
    }"#;

    fn scene_with_entities(count: usize) -> String {
        let entities = (0..count)
            .map(|index| {
                format!(
                    r#"{{"id":"entity:item-{index}","components":[{{"id":"component:transform-{index}","type_id":"daedalus.transform","data":{{"translation":[{index}.0,0.0,0.0],"rotation":[0.0,0.0,0.0,1.0],"scale":[1.0,1.0,1.0]}}}}]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"schema_version":1,"revision":9,"entities":[{entities}],"assets":[]}}"#)
    }

    fn prepared(scene: &str) -> PreparedProject {
        let manifest = br#"{"schema_version":1,"name":"control fixture","scene":"scene.json"}"#;
        prepare_project_from_bytes(
            manifest,
            scene.as_bytes(),
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .expect("prepare test project")
    }

    fn start(config: SessionControlConfig) -> SessionControlService {
        SessionControlService::start(prepared(ONE_ENTITY_SCENE), config)
            .expect("start project control service")
    }

    fn default_config() -> SessionControlConfig {
        SessionControlConfig {
            engine: EngineConfig::default(),
            ..SessionControlConfig::default()
        }
    }

    fn moved(x: f32) -> Transform {
        Transform {
            translation: [x, 5.0, 6.0],
            ..Transform::IDENTITY
        }
    }

    fn stable_id(value: &str) -> StableId {
        StableId::new(value).expect("valid fixture stable ID")
    }

    fn wait_terminal(ticket: &super::RequestTicket) -> std::sync::Arc<super::RequestProgress> {
        ticket
            .wait_for(WaitStage::Terminal, Duration::from_secs(3))
            .expect("request should reach terminal")
    }

    fn submit_after_busy(
        client: &super::SessionControlClient,
        operation: SessionOperation,
    ) -> super::RequestTicket {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match client.try_submit(operation.clone()) {
                Ok(ticket) => return ticket,
                Err(SessionSubmitError::Busy) if Instant::now() < deadline => {
                    thread::yield_now();
                }
                Err(error) => panic!("request admission failed: {error:?}"),
            }
        }
    }

    fn wait_for_frame(client: &super::SessionControlClient) -> std::sync::Arc<super::SessionFrame> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match client.try_latest_frame() {
                Ok(Some(frame)) => return frame,
                Ok(None) | Err(super::FrameReadError::Busy) => {}
                Err(error) => panic!("frame mailbox failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for project frame"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn manual_set_acceptance_precedes_step_and_reads_snapshot_from_the_same_engine() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let target = SessionEntityTarget::StableId(stable_id("entity:hero"));
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: target.clone(),
                command_id: None,
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(8.0),
            },
        );
        let accepted = set
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(accepted.phase(), RequestPhase::AuthorityAccepted);
        assert_eq!(accepted.authority().unwrap().apply_tick, Some(Tick(1)));

        let mut before = submit_after_busy(
            &client,
            SessionOperation::GetTransform {
                target: target.clone(),
            },
        );
        let before_state = wait_terminal(&before);
        let SessionRequestResult::Transform(before_transform) =
            before_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("get transform returned a different result")
        };
        assert_eq!(before_transform.observed_tick, Tick(0));
        assert_eq!(
            before_transform.entity.transform.translation,
            [2.0, 3.0, 4.0]
        );

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let step_state = wait_terminal(&step);
        let SessionRequestResult::Step(reports) = step_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("manual step returned a different result")
        };
        assert_eq!(reports[0].tick, Tick(1));
        assert_eq!(reports[0].commands.len(), 1);
        assert_eq!(
            reports[0].commands[0].outcome,
            CommandOutcome::Applied {
                revision: Revision(1)
            }
        );
        let set_state = wait_terminal(&set);
        let Ok(SessionRequestResult::Command(set_command)) = set_state.terminal().unwrap() else {
            panic!("set ticket should return a command result")
        };
        assert_eq!(set_command.result, reports[0].commands[0]);
        assert_eq!(set_command.resolve_tick, Some(Tick(1)));
        let frame = wait_for_frame(&client);
        assert_eq!(frame.tick(), Tick(1));
        assert_eq!(frame.snapshot().entities[0].transform, moved(8.0));
        let mut after = submit_after_busy(&client, SessionOperation::GetTransform { target });
        let after_state = wait_terminal(&after);
        let SessionRequestResult::Transform(after_transform) =
            after_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("get transform returned a different result")
        };
        assert_eq!(after_transform.observed_tick, Tick(1));
        assert_eq!(after_transform.entity.transform, moved(8.0));

        after.acknowledge().unwrap();
        before.acknowledge().unwrap();
        step.acknowledge().unwrap();
        set.acknowledge().unwrap();
        assert_eq!(service.shutdown().final_tick, Tick(1));
    }

    #[test]
    fn reserved_manual_step_credit_breaks_full_pending_write_backpressure() {
        let mut config = default_config();
        config.limits.max_pending_per_client = 2;
        config.limits.max_pending_total = 2;
        config.limits.max_result_bytes_total = 2 * 1024 * 1024;
        let service = start(config);
        let client = service.attach().unwrap();
        let mut write = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: None,
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(10.0),
            },
        );
        write
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match client.try_submit(SessionOperation::Status) {
                Err(SessionSubmitError::Busy) if Instant::now() < deadline => {
                    thread::yield_now();
                }
                Err(SessionSubmitError::ResultCapacity) => break,
                Err(error) => panic!("expected bounded result-credit exhaustion, got {error:?}"),
                Ok(_) => panic!("result-credit exhaustion unexpectedly admitted a ticket"),
            }
        }

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let step_state = wait_terminal(&step);
        let SessionRequestResult::Step(reports) = step_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("step should complete")
        };
        assert_eq!(reports[0].commands.len(), 1);
        assert!(matches!(
            reports[0].commands[0].outcome,
            CommandOutcome::Applied { .. }
        ));
        wait_terminal(&write);
        step.acknowledge().unwrap();
        write.acknowledge().unwrap();
        let mut status = submit_after_busy(&client, SessionOperation::Status);
        wait_terminal(&status);
        status.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn wait_timeout_does_not_cancel_the_authority_accepted_write() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: None,
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(14.0),
            },
        );
        set.wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            set.wait_for(WaitStage::Terminal, Duration::from_millis(1)),
            Err(super::RequestWaitError::TimedOut)
        ));
        assert_eq!(
            set.try_state().unwrap().phase(),
            RequestPhase::AuthorityAccepted
        );

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        let terminal = wait_terminal(&set);
        assert!(matches!(
            terminal.terminal().unwrap(),
            Ok(SessionRequestResult::Command(_))
        ));
        set.acknowledge().unwrap();
        step.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn a_reader_arc_keeps_result_bytes_reserved_after_ticket_ack() {
        let mut config = default_config();
        config.limits.max_pending_per_client = 2;
        config.limits.max_pending_total = 2;
        config.limits.max_result_bytes_total = 2 * 1024 * 1024;
        let service = start(config);
        let client = service.attach().unwrap();
        let mut first = submit_after_busy(&client, SessionOperation::Status);
        let retained_progress = wait_terminal(&first);
        first.acknowledge().unwrap();
        assert_eq!(
            client.try_submit(SessionOperation::Status).err().unwrap(),
            SessionSubmitError::ResultCapacity
        );
        drop(retained_progress);
        let mut second = submit_after_busy(&client, SessionOperation::Status);
        wait_terminal(&second);
        second.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn terminal_ack_releases_step_credit_before_actor_request_cleanup() {
        let mut config = default_config();
        config.limits.max_result_bytes_total = 2 * 1024 * 1024;
        let service = start(config);
        let client = service.attach().unwrap();
        service
            .shared
            .pause_after_next_request_before_cleanup
            .store(true, Ordering::Release);

        let mut first = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let first_state = wait_terminal(&first);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !service
            .shared
            .request_cleanup_paused
            .load(Ordering::Acquire)
        {
            assert!(
                Instant::now() < deadline,
                "actor did not pause after publishing the Step terminal"
            );
            thread::yield_now();
        }
        assert!(first_state.terminal().unwrap().is_ok());
        first.acknowledge().unwrap();
        drop(first_state);

        // The actor is deliberately paused with its ActiveRequest still installed. The Step
        // reserve must already be free once terminal readers release their progress Arc.
        let second_admission = client.try_submit(SessionOperation::Step { count: 1 });
        service
            .shared
            .release_paused_request_cleanup
            .store(true, Ordering::Release);
        let mut second = second_admission
            .expect("acknowledged terminal Step credit is available before actor cleanup");
        let second_state = wait_terminal(&second);
        assert!(second_state.terminal().unwrap().is_ok());
        second.acknowledge().unwrap();
        drop(second_state);
        assert_eq!(service.shutdown().final_tick, Tick(2));
    }

    #[test]
    fn try_state_and_ticket_drop_do_not_wait_on_the_progress_mutex() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let ticket = submit_after_busy(&client, SessionOperation::Status);
        let cell = std::sync::Arc::clone(&ticket.cell);
        let state_guard = cell.state.lock().unwrap();
        assert!(matches!(
            ticket.try_state(),
            Err(super::RequestReadError::Busy)
        ));

        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(ticket);
            finished_sender.send(()).unwrap();
        });
        finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("ticket Drop must not wait for progress readers");
        drop(state_guard);
        dropper.join().unwrap();
        let report = service.shutdown();
        assert!(report.abandoned_tickets >= 1);
    }

    #[test]
    fn client_frame_arc_retention_skips_publication_without_stalling_manual_ticks() {
        let mut config = default_config();
        config.engine.snapshot_capacity = 8;
        config.limits.max_retained_frames = 2;
        let service = start(config);
        let client = service.attach().unwrap();

        let mut first_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&first_step);
        first_step.acknowledge().unwrap();
        let first_frame = wait_for_frame(&client);

        let mut second_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&second_step);
        second_step.acknowledge().unwrap();
        let second_frame = wait_for_frame(&client);
        assert_eq!(first_frame.tick(), Tick(1));
        assert_eq!(second_frame.tick(), Tick(2));

        let mut third_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let third_state = wait_terminal(&third_step);
        let Ok(SessionRequestResult::Step(reports)) = third_state.terminal().unwrap() else {
            panic!("third manual step should complete")
        };
        assert_eq!(reports[0].tick, Tick(3));
        let latest = wait_for_frame(&client);
        assert_eq!(latest.sequence(), second_frame.sequence());
        assert_eq!(latest.tick(), Tick(2));
        third_step.acknowledge().unwrap();
        drop(third_state);

        drop(first_frame);
        let mut fourth_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&fourth_step);
        fourth_step.acknowledge().unwrap();
        let recovered = wait_for_frame(&client);
        assert_eq!(recovered.tick(), Tick(4));
        assert!(recovered.sequence() > second_frame.sequence());
        let _ = service.shutdown();
    }

    #[test]
    fn frame_capacity_one_counts_backing_only_retention_and_recovers() {
        let mut config = default_config();
        config.engine.snapshot_capacity = 64;
        config.limits.max_retained_frames = 1;
        let service = start(config);
        let client = service.attach().unwrap();

        let mut first_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let first_state = wait_terminal(&first_step);
        let SessionRequestResult::Step(first_reports) =
            first_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("first manual step should report Engine tick")
        };
        assert_eq!(first_reports[0].tick, Tick(1));
        assert!(matches!(
            &first_reports[0].snapshot,
            crate::SnapshotPublication::Published { .. }
        ));
        first_step.acknowledge().unwrap();
        drop(first_state);

        let first_frame = wait_for_frame(&client);
        assert_eq!(first_frame.tick(), Tick(1));
        let retained_entities = std::sync::Arc::clone(&first_frame.snapshot().entities);
        drop(first_frame);

        let mut second_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let second_state = wait_terminal(&second_step);
        let SessionRequestResult::Step(second_reports) =
            second_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("second manual step should report Engine tick")
        };
        assert_eq!(second_reports[0].tick, Tick(2));
        assert!(matches!(
            &second_reports[0].snapshot,
            crate::SnapshotPublication::Published { .. }
        ));
        second_step.acknowledge().unwrap();
        drop(second_state);
        assert_eq!(wait_for_frame(&client).tick(), Tick(1));

        drop(retained_entities);
        let mut third_step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let third_state = wait_terminal(&third_step);
        let SessionRequestResult::Step(third_reports) =
            third_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("third manual step should report Engine tick")
        };
        assert_eq!(third_reports[0].tick, Tick(3));
        third_step.acknowledge().unwrap();
        drop(third_state);
        assert_eq!(wait_for_frame(&client).tick(), Tick(3));
        let _ = service.shutdown();
    }

    #[test]
    fn frame_capacity_one_publishes_repeatedly_with_minimum_producer_capacity() {
        let mut config = default_config();
        config.engine.snapshot_capacity = 2;
        config.limits.max_retained_frames = 1;
        let service = start(config);
        let client = service.attach().unwrap();

        for tick in 1..=4 {
            let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
            let state = wait_terminal(&step);
            let SessionRequestResult::Step(reports) = state.terminal().unwrap().as_ref().unwrap()
            else {
                panic!("manual step should report Engine tick")
            };
            assert_eq!(reports[0].tick, Tick(tick));
            assert!(matches!(
                &reports[0].snapshot,
                crate::SnapshotPublication::Published { .. }
            ));
            step.acknowledge().unwrap();
            drop(state);
            let frame = wait_for_frame(&client);
            assert_eq!(frame.tick(), Tick(tick));
            drop(frame);
        }
        let _ = service.shutdown();
    }

    #[test]
    fn submit_returns_busy_when_the_bounded_queue_lock_is_contended() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let queue_guard = service.shared.queue.state.lock().unwrap();
        assert_eq!(
            client.try_submit(SessionOperation::Status).err().unwrap(),
            SessionSubmitError::Busy
        );
        drop(queue_guard);
        let _ = service.shutdown();
    }

    #[test]
    fn duplicate_caller_ids_replay_real_outcomes_and_eviction_obeys_high_water() {
        let mut config = default_config();
        config.limits.replay_ledger_capacity = 1;
        let service = start(config);
        let first_client = service.attach().unwrap();
        let second_client = service.attach().unwrap();
        let target = SessionEntityTarget::StableId(stable_id("entity:hero"));
        let mut first = submit_after_busy(
            &first_client,
            SessionOperation::SetTransform {
                target: target.clone(),
                command_id: Some(CommandId::from_raw(7)),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(7.0),
            },
        );
        let first_admission = first
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            first_admission.authority().unwrap().apply_tick,
            Some(Tick(1))
        );
        let mut duplicate_pending = submit_after_busy(
            &second_client,
            SessionOperation::SetTransform {
                target: target.clone(),
                command_id: Some(CommandId::from_raw(7)),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(99.0),
            },
        );
        let duplicate_admission = duplicate_pending
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            duplicate_admission.authority().unwrap().submission,
            CommandSubmission::DuplicatePending
        );
        assert_eq!(
            duplicate_admission.authority().unwrap().apply_tick,
            Some(Tick(1))
        );

        let mut step = submit_after_busy(&first_client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        let first_result = wait_terminal(&first).terminal().unwrap().clone();
        let pending_duplicate_result = wait_terminal(&duplicate_pending)
            .terminal()
            .unwrap()
            .clone();
        assert_eq!(first_result, pending_duplicate_result);
        first.acknowledge().unwrap();
        duplicate_pending.acknowledge().unwrap();
        step.acknowledge().unwrap();

        let mut duplicate_completed = submit_after_busy(
            &second_client,
            SessionOperation::SetTransform {
                target: target.clone(),
                command_id: Some(CommandId::from_raw(7)),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(-99.0),
            },
        );
        let completed = duplicate_completed
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(completed.authority().unwrap().apply_tick, Some(Tick(1)));
        assert!(matches!(
            completed.authority().unwrap().submission,
            CommandSubmission::DuplicateCompleted(_)
        ));
        assert_eq!(
            wait_terminal(&duplicate_completed).terminal().unwrap(),
            &first_result
        );
        duplicate_completed.acknowledge().unwrap();

        let mut next = submit_after_busy(
            &first_client,
            SessionOperation::SetTransform {
                target,
                command_id: Some(CommandId::from_raw(8)),
                expected_revision: Revision(1),
                apply_at: None,
                transform: moved(8.0),
            },
        );
        let mut second_step = submit_after_busy(&first_client, SessionOperation::Step { count: 1 });
        wait_terminal(&second_step);
        wait_terminal(&next);
        next.acknowledge().unwrap();
        second_step.acknowledge().unwrap();

        let mut expired = submit_after_busy(
            &second_client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: Some(CommandId::from_raw(7)),
                expected_revision: Revision(2),
                apply_at: None,
                transform: moved(70.0),
            },
        );
        let expired_state = wait_terminal(&expired);
        assert_eq!(
            expired_state.terminal().unwrap(),
            &Err(SessionRequestError::ReplayWindowExpired {
                command_id: 7,
                high_water: 8,
            })
        );
        expired.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn pages_are_bounded_and_reach_the_last_source_binding() {
        let service = SessionControlService::start(
            prepared(&scene_with_entities(5)),
            SessionControlConfig {
                limits: SessionControlLimits {
                    max_page_entities: 2,
                    ..SessionControlLimits::default()
                },
                ..default_config()
            },
        )
        .unwrap();
        let client = service.attach().unwrap();
        let mut page_one = submit_after_busy(
            &client,
            SessionOperation::ProjectInfo {
                offset: 0,
                limit: Some(2),
            },
        );
        let first_state = wait_terminal(&page_one);
        let Ok(SessionRequestResult::ProjectInfo(first)) = first_state.terminal().unwrap() else {
            panic!("project info should return a source page")
        };
        assert_eq!(first.entity_count, 5);
        assert_eq!(first.entities.len(), 2);
        assert_eq!(first.next_offset, Some(2));
        let mut page_three = submit_after_busy(
            &client,
            SessionOperation::ProjectInfo {
                offset: 4,
                limit: Some(1),
            },
        );
        let last_state = wait_terminal(&page_three);
        let Ok(SessionRequestResult::ProjectInfo(last)) = last_state.terminal().unwrap() else {
            panic!("project info should return a source page")
        };
        assert_eq!(last.entities.len(), 1);
        assert_eq!(last.entities[0].id.as_str(), "entity:item-4");
        assert_eq!(last.next_offset, None);
        page_one.acknowledge().unwrap();
        page_three.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn limits_reject_configs_that_cannot_reserve_manual_step_capacity() {
        let mut config = default_config();
        config.limits.max_pending_per_client = 1;
        let error = SessionControlService::start(prepared(ONE_ENTITY_SCENE), config)
            .err()
            .expect("one client ticket cannot be split into normal and Step reserves");
        assert!(matches!(
            error,
            super::SessionControlStartError::InvalidConfig(_)
        ));

        let mut config = default_config();
        config.limits.max_result_bytes_total = 1024 * 1024;
        let error = SessionControlService::start(prepared(ONE_ENTITY_SCENE), config)
            .err()
            .expect("global result bytes must include both normal and Step reserve");
        assert!(matches!(
            error,
            super::SessionControlStartError::InvalidConfig(_)
        ));
    }

    #[test]
    fn service_engine_cache_boundaries_are_explicit() {
        for capacity in [1, 16_384] {
            let engine = EngineConfig {
                command_dedup_capacity: capacity,
                snapshot_capacity: 2,
                ..EngineConfig::default()
            };
            assert!(SessionControlLimits::default().validate(&engine).is_ok());
        }
        for capacity in [0, 16_385, usize::MAX] {
            let engine = EngineConfig {
                command_dedup_capacity: capacity,
                snapshot_capacity: 2,
                ..EngineConfig::default()
            };
            let error = SessionControlLimits::default()
                .validate(&engine)
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid session-control configuration: EngineConfig.command_dedup_capacity must be in 1..=16384"
            );
        }

        for capacity in [2, 64] {
            let engine = EngineConfig {
                snapshot_capacity: capacity,
                ..EngineConfig::default()
            };
            assert!(SessionControlLimits::default().validate(&engine).is_ok());
        }
        let error = SessionControlLimits::default()
            .validate(&EngineConfig {
                snapshot_capacity: 1,
                ..EngineConfig::default()
            })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid session-control configuration: EngineConfig.snapshot_capacity must be in 2..=64 for last-good service frames"
        );
    }

    #[test]
    fn realtime_requests_reach_terminal_and_manual_step_is_rejected() {
        let service = start(SessionControlConfig {
            clock_mode: SessionClockMode::Realtime,
            ..default_config()
        });
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: None,
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(12.0),
            },
        );
        let terminal_first = set
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(3))
            .unwrap();
        assert!(terminal_first.authority().is_some());
        let terminal = set
            .wait_for(WaitStage::Terminal, Duration::from_secs(3))
            .unwrap();
        assert!(matches!(
            terminal.terminal().unwrap(),
            Ok(SessionRequestResult::Command(
                super::CommandTerminalResult {
                    result: daedalus_world::CommandResult {
                        outcome: CommandOutcome::Applied { .. },
                        ..
                    },
                    resolve_tick: Some(_),
                }
            ))
        ));
        assert_eq!(
            client
                .try_submit(SessionOperation::Step { count: 1 })
                .err()
                .unwrap(),
            SessionSubmitError::ClockModeMismatch
        );
        set.acknowledge().unwrap();
        assert!(service.shutdown().final_tick >= Tick(1));
    }

    #[test]
    fn terminal_query_satisfies_authority_wait_and_ticket_results_are_isolated() {
        let service = start(default_config());
        let first_client = service.attach().unwrap();
        let second_client = service.attach().unwrap();
        let mut status = submit_after_busy(&first_client, SessionOperation::Status);
        let mut page = submit_after_busy(
            &second_client,
            SessionOperation::ProjectInfo {
                offset: 0,
                limit: Some(1),
            },
        );
        let status_state = wait_terminal(&status);
        let _ = wait_terminal(&page);
        let page_state = page
            .wait_for(WaitStage::AuthorityAccepted, Duration::ZERO)
            .unwrap();
        assert!(matches!(
            status_state.terminal().unwrap(),
            Ok(SessionRequestResult::Status(_))
        ));
        assert!(matches!(
            page_state.terminal().unwrap(),
            Ok(SessionRequestResult::ProjectInfo(ProjectInfoPage { .. }))
        ));
        status.acknowledge().unwrap();
        page.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn handle_validation_capabilities_reserved_ids_and_clock_mode_are_explicit() {
        let service = start(SessionControlConfig {
            clock_mode: SessionClockMode::Realtime,
            ..default_config()
        });
        let readonly = service
            .attach_with_capabilities(SessionCapabilities {
                inspect: true,
                mutate: false,
                manual_step: false,
            })
            .unwrap();
        assert_eq!(
            readonly
                .try_submit(SessionOperation::SetTransform {
                    target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                    command_id: None,
                    expected_revision: Revision(0),
                    apply_at: None,
                    transform: moved(2.0),
                })
                .err()
                .unwrap(),
            SessionSubmitError::CapabilityDenied
        );
        let trusted = service.attach().unwrap();
        assert_eq!(
            trusted
                .try_submit(SessionOperation::SetTransform {
                    target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                    command_id: Some(CommandId::from_raw(1_u128 << 127)),
                    expected_revision: Revision(0),
                    apply_at: None,
                    transform: moved(2.0),
                })
                .err()
                .unwrap(),
            SessionSubmitError::ReservedCommandId
        );
        assert_eq!(
            trusted
                .try_submit(SessionOperation::Step { count: 1 })
                .err()
                .unwrap(),
            SessionSubmitError::ClockModeMismatch
        );
        let wrong_session = SessionEntityTarget::Handle(EntityHandle {
            session: SessionId::from_raw(999_999),
            index: 0,
            generation: 0,
        });
        let mut read = submit_after_busy(
            &trusted,
            SessionOperation::GetTransform {
                target: wrong_session,
            },
        );
        assert_eq!(
            wait_terminal(&read).terminal().unwrap(),
            &Err(SessionRequestError::WorldRead(
                daedalus_world::WorldReadError::WrongSession
            ))
        );
        read.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn shutdown_reports_unapplied_authority_and_keeps_the_ticket_terminal() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: None,
                expected_revision: Revision(0),
                apply_at: Some(Tick(9)),
                transform: moved(18.0),
            },
        );
        set.wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let report = service.shutdown();
        assert_eq!(report.final_tick, Tick(0));
        assert_eq!(report.commands_stopped_not_applied, 1);
        assert_eq!(
            wait_terminal(&set).terminal().unwrap(),
            &Err(SessionRequestError::StoppedNotApplied {
                command_id: set.try_state().unwrap().authority().unwrap().command_id,
            })
        );
        set.acknowledge().unwrap();
    }

    #[test]
    fn shutdown_counts_duplicate_pending_tickets_as_one_unapplied_command() {
        let service = start(default_config());
        let first_client = service.attach().unwrap();
        let second_client = service.attach().unwrap();
        let target = SessionEntityTarget::StableId(stable_id("entity:hero"));
        let command_id = CommandId::from_raw(23);
        let mut original = submit_after_busy(
            &first_client,
            SessionOperation::SetTransform {
                target: target.clone(),
                command_id: Some(command_id),
                expected_revision: Revision(0),
                apply_at: Some(Tick(9)),
                transform: moved(23.0),
            },
        );
        original
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let mut duplicate = submit_after_busy(
            &second_client,
            SessionOperation::SetTransform {
                target,
                command_id: Some(command_id),
                expected_revision: Revision(0),
                apply_at: Some(Tick(9)),
                transform: moved(99.0),
            },
        );
        assert_eq!(
            duplicate
                .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
                .unwrap()
                .authority()
                .unwrap()
                .submission,
            CommandSubmission::DuplicatePending
        );

        let report = service.shutdown();
        assert_eq!(report.commands_stopped_not_applied, 1);
        let expected = Err(SessionRequestError::StoppedNotApplied { command_id });
        assert_eq!(wait_terminal(&original).terminal().unwrap(), &expected);
        assert_eq!(wait_terminal(&duplicate).terminal().unwrap(), &expected);
        original.acknowledge().unwrap();
        duplicate.acknowledge().unwrap();
    }

    #[test]
    fn actor_panic_terminalizes_in_flight_query_and_ambiguous_write_as_unknown() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        service
            .shared
            .panic_before_next_request
            .store(true, std::sync::atomic::Ordering::Release);
        let mut query = submit_after_busy(&client, SessionOperation::Status);
        assert!(matches!(
            wait_terminal(&query).terminal().unwrap(),
            Err(SessionRequestError::ActorFailed(_))
        ));
        let report = service.shutdown();
        assert!(matches!(
            report.termination,
            SessionTermination::Panicked(_)
        ));
        query.acknowledge().unwrap();
    }

    #[test]
    fn actor_panic_after_engine_step_terminalizes_step_and_does_not_claim_write_unapplied() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: Some(CommandId::from_raw(11)),
                expected_revision: Revision(0),
                apply_at: Some(Tick(1)),
                transform: moved(11.0),
            },
        );
        let admission = set
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(admission.authority().unwrap().apply_tick, Some(Tick(1)));

        service
            .shared
            .panic_after_next_engine_step
            .store(true, std::sync::atomic::Ordering::Release);
        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        assert!(matches!(
            wait_terminal(&step).terminal().unwrap(),
            Err(SessionRequestError::ActorFailed(_))
        ));
        assert_eq!(
            wait_terminal(&set).terminal().unwrap(),
            &Err(SessionRequestError::OutcomeUnknown {
                command_id: CommandId::from_raw(11),
            })
        );

        let report = service.shutdown();
        assert_eq!(report.final_tick, Tick(1));
        assert!(matches!(
            report.termination,
            SessionTermination::Panicked(_)
        ));
        assert_eq!(report.commands_stopped_not_applied, 0);
        set.acknowledge().unwrap();
        step.acknowledge().unwrap();
    }

    #[test]
    fn wrong_session_handle_reaches_real_world_command_result() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::Handle(EntityHandle {
                    session: SessionId::fresh(),
                    index: 0,
                    generation: 0,
                }),
                command_id: None,
                expected_revision: Revision(9),
                apply_at: Some(Tick(1)),
                transform: moved(22.0),
            },
        );
        set.wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        let terminal = wait_terminal(&set);
        let SessionRequestResult::Command(command) = terminal.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("set should have a command result")
        };
        assert_eq!(
            command.result.outcome,
            CommandOutcome::Rejected(daedalus_world::CommandError::WrongSession)
        );
        set.acknowledge().unwrap();
        step.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn stale_revision_is_rejected_by_the_real_world_after_step() {
        let service = start(default_config());
        let client = service.attach().unwrap();
        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: None,
                expected_revision: Revision(9),
                apply_at: Some(Tick(1)),
                transform: moved(22.0),
            },
        );
        set.wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        let terminal = wait_terminal(&set);
        let Ok(SessionRequestResult::Command(command)) = terminal.terminal().unwrap() else {
            panic!("set should have a command result")
        };
        assert_eq!(
            command.result.outcome,
            CommandOutcome::Rejected(daedalus_world::CommandError::RevisionConflict {
                expected: Revision(9),
                actual: Revision(0),
            })
        );
        assert_eq!(command.resolve_tick, Some(Tick(1)));
        set.acknowledge().unwrap();
        step.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[test]
    fn runtime_spawn_rejects_entity_16385() {
        let service = SessionControlService::start(
            prepared(&scene_with_entities(MAX_PROJECT_SESSION_ENTITIES)),
            default_config(),
        )
        .expect("the authored entity ceiling is inclusive");
        let client = service.attach().unwrap();
        let mut spawn = submit_after_busy(
            &client,
            SessionOperation::SpawnTransform {
                transform: Transform::IDENTITY,
            },
        );
        assert_eq!(
            wait_terminal(&spawn).terminal().unwrap(),
            &Err(SessionRequestError::EntityLimit)
        );
        spawn.acknowledge().unwrap();
        let _ = service.shutdown();
    }
}
