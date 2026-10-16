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

use crate::MAX_PREPARATION_STATUS_RESULT_BYTES;
use crate::project::MAX_PROJECT_SESSION_ENTITIES;
use crate::project_activation::{
    ActivationId, AssetActivationCommandOutcome, AssetActivationCommandResult,
    AssetActivationOperation, AssetActivationRequestResult, AssetActivationStatus,
};
use crate::project_preparation::{
    AssetPreparationCommandOutcome, AssetPreparationCommandResult, AssetPreparationOperation,
    AssetPreparationResult, PreparationId, ProjectPreparationPort, ProjectPreparationPortError,
    StageResult,
};
use crate::session_primitives::{
    CreditError, CreditLease, CreditLimits, CreditPool, LatestValueMailbox, MailboxError,
    StopSignal,
};
use crate::{
    EngineConfig, ProjectSession, ProjectSessionStartError, SnapshotPublication, StepReport,
};

#[cfg(feature = "audio")]
use crate::ProjectAudioRuntime;
#[cfg(feature = "audio")]
use crate::SessionAudioOptions;
#[cfg(feature = "audio")]
use crate::project_audio::{
    SessionAudioCommandResult, SessionAudioError, SessionAudioOperation, SessionAudioResult,
};

#[cfg(feature = "audio")]
type ActorAudioOptions = Option<SessionAudioOptions>;
#[cfg(not(feature = "audio"))]
type ActorAudioOptions = ();

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
    AssetPreparation(AssetPreparationOperation),
    AssetActivation(AssetActivationOperation),
    #[cfg(feature = "audio")]
    Audio(SessionAudioOperation),
}

impl SessionOperation {
    fn needs_inspect(&self) -> bool {
        let world = matches!(
            self,
            Self::ProjectInfo { .. }
                | Self::GetTransform { .. }
                | Self::ListTransforms { .. }
                | Self::Status
        );
        let preparation = matches!(
            self,
            Self::AssetPreparation(AssetPreparationOperation::Status)
        );
        let activation = matches!(
            self,
            Self::AssetActivation(AssetActivationOperation::Status)
        );
        #[cfg(feature = "audio")]
        {
            world
                || preparation
                || activation
                || matches!(
                    self,
                    Self::Audio(
                        SessionAudioOperation::Status
                            | SessionAudioOperation::Clips { .. }
                            | SessionAudioOperation::Voice { .. }
                            | SessionAudioOperation::Voices { .. }
                    )
                )
        }
        #[cfg(not(feature = "audio"))]
        {
            world || preparation || activation
        }
    }

    fn needs_mutate(&self) -> bool {
        let world = matches!(
            self,
            Self::SpawnTransform { .. } | Self::SetTransform { .. }
        ) || matches!(
            self,
            Self::AssetPreparation(
                AssetPreparationOperation::Prepare { .. }
                    | AssetPreparationOperation::Cancel { .. }
            )
        );
        let activation = matches!(
            self,
            Self::AssetActivation(
                AssetActivationOperation::Stage { .. }
                    | AssetActivationOperation::Commit { .. }
                    | AssetActivationOperation::Cancel { .. }
            )
        );
        #[cfg(feature = "audio")]
        {
            world
                || activation
                || matches!(self, Self::Audio(SessionAudioOperation::Command { .. }))
        }
        #[cfg(not(feature = "audio"))]
        {
            world || activation
        }
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
    AssetPreparation(AssetPreparationResult),
    AssetActivation(AssetActivationRequestResult),
    #[cfg(feature = "audio")]
    Audio(SessionAudioResult),
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
    #[error("could not reserve space in the bounded command replay ledger")]
    ReplayStorageUnavailable,
    #[cfg(feature = "audio")]
    #[error("audio operations require a service started with audio enabled")]
    AudioUnavailable,
    #[error("asset preparation operations require a service started with preparation enabled")]
    AssetPreparationUnavailable,
    #[error("asset activation operations require the native headless activation service")]
    AssetActivationUnavailable,
    #[error("asset preparation state is busy")]
    PreparationBusy,
    #[error("asset preparation capacity is exhausted")]
    PreparationCapacity,
    #[error("asset preparation worker is stopping or stopped")]
    PreparationStopped,
    #[error("candidate ID is invalid for this session")]
    InvalidCandidate,
    #[error("candidate has expired from the current preparation generation")]
    CandidateExpired,
    #[error("candidate is not active and cannot be cancelled")]
    CandidateNotActive,
    #[error("asset preparation epoch space is exhausted")]
    PreparationEpochExhausted,
    #[error("asset activation state is busy")]
    ActivationBusy,
    #[error("activation ID is invalid for this session")]
    InvalidActivation,
    #[error("activation has expired")]
    ActivationExpired,
    #[error("activation is not staged")]
    ActivationNotStaged,
    #[error("active project generation does not match the request")]
    ActiveGenerationMismatch,
    #[error("activation sequence space is exhausted")]
    ActivationSequenceExhausted,
    #[error("project generation space is exhausted")]
    ProjectGenerationExhausted,
    #[error("retained project activation capacity is exhausted")]
    ActivationCapacity,
    #[cfg(feature = "audio")]
    #[error(transparent)]
    Audio(#[from] SessionAudioError),
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
    #[cfg(feature = "audio")]
    #[error("this session was started without audio enabled")]
    AudioUnavailable,
    #[error("this session was started without asset preparation enabled")]
    AssetPreparationUnavailable,
    #[error("this session was started without native asset activation enabled")]
    AssetActivationUnavailable,
}

#[derive(Debug, Error)]
pub enum SessionControlStartError {
    #[error("invalid session-control configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Project(#[from] ProjectSessionStartError),
    #[error("initial tick-zero snapshot could not be published: {0:?}")]
    InitialSnapshot(SnapshotPublication),
    #[error("initial tick-zero session frame could not be published: {0}")]
    InitialFramePublication(String),
    #[error("could not start project session actor: {0}")]
    ThreadSpawn(String),
    #[error("project actor stopped before startup completed")]
    StartupDisconnected,
    #[error("project actor startup handshake failed: {0}")]
    StartupFailed(String),
    #[cfg(feature = "audio")]
    #[error("project audio startup failed: {0}")]
    Audio(#[from] crate::project_audio::SessionAudioStartError),
    #[error("project preparation port could not bind: {0}")]
    PreparationPort(#[from] ProjectPreparationPortError),
    #[error("preparation port does not own the supplied prepared project")]
    PreparationProjectMismatch,
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
    authority_snapshot_sequence: u64,
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

    /// Engine snapshot sequence represented by this publication. It can stay fixed when the
    /// same authority state is republished under a newly committed project generation.
    #[must_use]
    pub const fn authority_snapshot_sequence(&self) -> u64 {
        self.authority_snapshot_sequence
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
    #[cfg(feature = "audio")]
    audio_enabled: bool,
    asset_preparation_enabled: bool,
    asset_activation_enabled: bool,
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
    #[cfg(all(test, feature = "audio"))]
    panic_after_audio_command_effect: AtomicBool,
    #[cfg(test)]
    panic_after_preparation_command_effect: AtomicBool,
    #[cfg(test)]
    panic_after_activation_command_effect: AtomicBool,
    #[cfg(all(test, feature = "audio"))]
    audio_command_effects: AtomicUsize,
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
        config: SessionControlConfig,
    ) -> Result<Self, SessionControlStartError> {
        Self::start_shared(Arc::new(prepared), config)
    }

    /// Start the actor with the same immutable prepared-project allocation held by the caller.
    pub fn start_shared(
        prepared: Arc<PreparedProject>,
        config: SessionControlConfig,
    ) -> Result<Self, SessionControlStartError> {
        #[cfg(feature = "audio")]
        let audio_options = None;
        #[cfg(not(feature = "audio"))]
        let audio_options = ();
        Self::start_shared_inner(prepared, config, audio_options, None, false)
    }

    /// Starts the existing project actor with an optional native preparation control capability.
    pub fn start_shared_with_preparation(
        prepared: Arc<PreparedProject>,
        config: SessionControlConfig,
        preparation_port: ProjectPreparationPort,
    ) -> Result<Self, SessionControlStartError> {
        if !preparation_port.matches_initial_project(&prepared) {
            return Err(SessionControlStartError::PreparationProjectMismatch);
        }
        #[cfg(feature = "audio")]
        let audio_options = None;
        #[cfg(not(feature = "audio"))]
        let audio_options = ();
        Self::start_shared_inner(
            prepared,
            config,
            audio_options,
            Some(preparation_port),
            false,
        )
    }

    pub(crate) fn start_shared_with_activation(
        prepared: Arc<PreparedProject>,
        config: SessionControlConfig,
        preparation_port: ProjectPreparationPort,
    ) -> Result<Self, SessionControlStartError> {
        if !preparation_port.matches_initial_project(&prepared) {
            return Err(SessionControlStartError::PreparationProjectMismatch);
        }
        #[cfg(feature = "audio")]
        let audio_options = None;
        #[cfg(not(feature = "audio"))]
        let audio_options = ();
        Self::start_shared_inner(
            prepared,
            config,
            audio_options,
            Some(preparation_port),
            true,
        )
    }

    /// Starts one project actor with both optional native preparation and audio services.
    #[cfg(feature = "audio")]
    pub fn start_shared_with_preparation_and_audio(
        prepared: Arc<PreparedProject>,
        config: SessionControlConfig,
        preparation_port: ProjectPreparationPort,
        audio_options: SessionAudioOptions,
    ) -> Result<Self, SessionControlStartError> {
        if !preparation_port.matches_initial_project(&prepared) {
            return Err(SessionControlStartError::PreparationProjectMismatch);
        }
        Self::start_shared_inner(
            prepared,
            config,
            Some(audio_options),
            Some(preparation_port),
            false,
        )
    }

    #[cfg(feature = "audio")]
    pub fn start_shared_with_audio(
        prepared: Arc<PreparedProject>,
        config: SessionControlConfig,
        audio_options: SessionAudioOptions,
    ) -> Result<Self, SessionControlStartError> {
        Self::start_shared_inner(prepared, config, Some(audio_options), None, false)
    }

    fn start_shared_inner(
        prepared: Arc<PreparedProject>,
        mut config: SessionControlConfig,
        audio_options: ActorAudioOptions,
        preparation_port: Option<ProjectPreparationPort>,
        asset_activation_enabled: bool,
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
            #[cfg(feature = "audio")]
            audio_enabled: audio_options.is_some(),
            asset_preparation_enabled: preparation_port.is_some(),
            asset_activation_enabled,
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
            #[cfg(all(test, feature = "audio"))]
            panic_after_audio_command_effect: AtomicBool::new(false),
            #[cfg(test)]
            panic_after_preparation_command_effect: AtomicBool::new(false),
            #[cfg(test)]
            panic_after_activation_command_effect: AtomicBool::new(false),
            #[cfg(all(test, feature = "audio"))]
            audio_command_effects: AtomicUsize::new(0),
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
            .spawn(move || {
                run_control_actor(
                    prepared,
                    config,
                    audio_options,
                    preparation_port,
                    actor_shared,
                    startup_sender,
                )
            })
            .map_err(|error| SessionControlStartError::ThreadSpawn(error.to_string()))?;
        match startup_receiver.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
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

    /// Whether this actor owns the optional native preparation worker capability.
    #[must_use]
    pub fn asset_preparation_enabled(&self) -> bool {
        self.shared.asset_preparation_enabled
    }

    /// Whether this actor is the native headless asset activation service.
    #[must_use]
    pub fn asset_activation_enabled(&self) -> bool {
        self.shared.asset_activation_enabled
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

    pub(crate) fn try_join_finished(&mut self) -> Option<SessionShutdownReport> {
        self.shared.request_stop();
        let join = self.join.as_ref()?;
        if !join.is_finished() {
            return None;
        }
        let join = self.join.take().expect("actor handle checked above");
        Some(join.join().unwrap_or_else(|payload| {
            shutdown_report(
                &self.shared,
                SessionTermination::Panicked(panic_message(payload)),
                Tick(0),
            )
        }))
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

    /// Whether this attachment is connected to a service with native asset preparation enabled.
    #[must_use]
    pub fn asset_preparation_enabled(&self) -> bool {
        self.shared.asset_preparation_enabled
    }

    /// Whether this attachment is connected to native headless asset activation.
    #[must_use]
    pub fn asset_activation_enabled(&self) -> bool {
        self.shared.asset_activation_enabled
    }

    /// Whether the session has closed request admission or stopped. A retained latest frame may
    /// still be readable after this returns true; frame availability is not a liveness signal.
    #[must_use]
    pub fn is_stopping(&self) -> bool {
        !self.shared.accepting.load(Ordering::Acquire)
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
        #[cfg(feature = "audio")]
        if matches!(&operation, SessionOperation::Audio(_)) && !self.shared.audio_enabled {
            return Err(SessionSubmitError::AudioUnavailable);
        }
        if matches!(&operation, SessionOperation::AssetPreparation(_))
            && !self.shared.asset_preparation_enabled
        {
            return Err(SessionSubmitError::AssetPreparationUnavailable);
        }
        if matches!(&operation, SessionOperation::AssetActivation(_))
            && !self.shared.asset_activation_enabled
        {
            return Err(SessionSubmitError::AssetActivationUnavailable);
        }
        if operation.needs_inspect() && !self.attachment.capabilities.inspect
            || operation.needs_mutate() && !self.attachment.capabilities.mutate
            || operation.is_step() && !self.attachment.capabilities.manual_step
        {
            return Err(SessionSubmitError::CapabilityDenied);
        }
        validate_operation(&operation, self.shared.clock_mode, self.shared.limits)?;
        let caller_command_id = match &operation {
            SessionOperation::SetTransform {
                command_id: Some(id),
                ..
            } => Some(*id),
            #[cfg(feature = "audio")]
            SessionOperation::Audio(SessionAudioOperation::Command {
                command_id: Some(id),
                ..
            }) => Some(*id),
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(id),
            })
            | SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(id),
                ..
            }) => Some(*id),
            SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                command_id: Some(id),
                ..
            })
            | SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(id),
                ..
            })
            | SessionOperation::AssetActivation(AssetActivationOperation::Cancel {
                command_id: Some(id),
                ..
            }) => Some(*id),
            _ => None,
        };
        if caller_command_id.is_some_and(|id| id.get() >= MAX_CALLER_COMMAND_ID) {
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

fn preparation_port_error(error: ProjectPreparationPortError) -> SessionRequestError {
    match error {
        ProjectPreparationPortError::Busy => SessionRequestError::PreparationBusy,
        ProjectPreparationPortError::Stopped => SessionRequestError::PreparationStopped,
        ProjectPreparationPortError::InvalidEpoch => SessionRequestError::InvalidCandidate,
        ProjectPreparationPortError::NoActiveCandidate => SessionRequestError::CandidateNotActive,
        ProjectPreparationPortError::Capacity => SessionRequestError::PreparationCapacity,
        ProjectPreparationPortError::GenerationMismatch => {
            SessionRequestError::ActiveGenerationMismatch
        }
        ProjectPreparationPortError::GenerationExhausted => {
            SessionRequestError::ProjectGenerationExhausted
        }
        ProjectPreparationPortError::SessionMismatch
        | ProjectPreparationPortError::AlreadyTaken
        | ProjectPreparationPortError::AlreadyBound => SessionRequestError::ActorFailed(
            "preparation port violated its session binding".to_owned(),
        ),
    }
}

fn activation_port_error(error: ProjectPreparationPortError) -> SessionRequestError {
    match error {
        ProjectPreparationPortError::Busy => SessionRequestError::ActivationBusy,
        ProjectPreparationPortError::Stopped => SessionRequestError::PreparationStopped,
        ProjectPreparationPortError::InvalidEpoch => SessionRequestError::InvalidActivation,
        ProjectPreparationPortError::NoActiveCandidate => SessionRequestError::ActivationNotStaged,
        ProjectPreparationPortError::Capacity => SessionRequestError::ActivationCapacity,
        ProjectPreparationPortError::GenerationMismatch => {
            SessionRequestError::ActiveGenerationMismatch
        }
        ProjectPreparationPortError::GenerationExhausted => {
            SessionRequestError::ProjectGenerationExhausted
        }
        ProjectPreparationPortError::SessionMismatch
        | ProjectPreparationPortError::AlreadyTaken
        | ProjectPreparationPortError::AlreadyBound => SessionRequestError::ActorFailed(
            "activation port violated its session binding".to_owned(),
        ),
    }
}

fn replay_storage_error() -> SessionRequestError {
    SessionRequestError::ReplayStorageUnavailable
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
        SessionOperation::GetTransform { .. }
        | SessionOperation::Status
        | SessionOperation::AssetPreparation(AssetPreparationOperation::Status)
        | SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare { .. })
        | SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel { .. })
        | SessionOperation::AssetActivation(_) => {}
        #[cfg(feature = "audio")]
        SessionOperation::Audio(operation) => match operation {
            SessionAudioOperation::Status | SessionAudioOperation::Voice { .. } => {}
            SessionAudioOperation::Clips { limit, .. }
            | SessionAudioOperation::Voices { limit, .. } => {
                if limit.is_some_and(|limit| limit == 0 || limit > limits.max_page_entities) {
                    return Err(SessionSubmitError::InvalidRequest);
                }
            }
            SessionAudioOperation::Command { mutation, .. } => match mutation {
                crate::SessionAudioMutation::Play { volume, pan, .. }
                    if !volume.is_finite()
                        || !(0.0..=1.0).contains(volume)
                        || !pan.is_finite()
                        || !(-1.0..=1.0).contains(pan) =>
                {
                    return Err(SessionSubmitError::InvalidRequest);
                }
                crate::SessionAudioMutation::SetVolume { volume, .. }
                    if !volume.is_finite() || !(0.0..=1.0).contains(volume) =>
                {
                    return Err(SessionSubmitError::InvalidRequest);
                }
                crate::SessionAudioMutation::SetPan { pan, .. }
                    if !pan.is_finite() || !(-1.0..=1.0).contains(pan) =>
                {
                    return Err(SessionSubmitError::InvalidRequest);
                }
                crate::SessionAudioMutation::RenderOffline { frames }
                    if !(1..=4_096).contains(frames) =>
                {
                    return Err(SessionSubmitError::InvalidRequest);
                }
                _ => {}
            },
        },
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
enum CachedCommandResult {
    World(CommandResult),
    #[cfg(feature = "audio")]
    Audio(SessionAudioCommandResult),
    Preparation(AssetPreparationCommandResult),
    Activation(AssetActivationCommandResult),
}

#[derive(Clone)]
struct ReplayEntry {
    apply_tick: Option<Tick>,
    resolve_tick: Option<Tick>,
    result: CachedCommandResult,
}

struct ControlActor {
    session: ProjectSession,
    config: SessionControlConfig,
    shared: Arc<ControlShared>,
    last_advance: Instant,
    last_published_sequence: u64,
    last_authority_snapshot_sequence: u64,
    frame_needs_publish: bool,
    active_project_generation: u64,
    active_project_digest: String,
    next_server_command_id: u128,
    caller_command_high_water: Option<u128>,
    pending_commands: HashMap<CommandId, PendingCommand>,
    completed_replay: HashMap<CommandId, ReplayEntry>,
    replay_order: VecDeque<CommandId>,
    preparation_port: Option<ProjectPreparationPort>,
    last_issued_preparation_epoch: u64,
    cancelled_preparation_epoch: Option<u64>,
    last_issued_activation_sequence: u64,
    current_activation: Option<crate::AssetActivationSummary>,
    latest_activation_failure: Option<crate::AssetActivationFailure>,
    staged_project: Option<StagedProjectCandidate>,
    deferred_stage_result: Option<StageResult>,
    #[cfg(feature = "audio")]
    audio: Option<ProjectAudioRuntime>,
    active_request: Option<ActiveRequest>,
}

struct StagedProjectCandidate {
    activation: ActivationId,
    candidate: PreparationId,
    expected_active_generation: u64,
    project: Arc<PreparedProject>,
}

fn run_control_actor(
    prepared: Arc<PreparedProject>,
    config: SessionControlConfig,
    #[cfg(feature = "audio")] audio_options: ActorAudioOptions,
    #[cfg(not(feature = "audio"))] _audio_options: ActorAudioOptions,
    mut preparation_port: Option<ProjectPreparationPort>,
    shared: Arc<ControlShared>,
    startup_sender: std::sync::mpsc::SyncSender<Result<(), SessionControlStartError>>,
) -> SessionShutdownReport {
    let mut session = match ProjectSession::new_shared(prepared, config.engine.clone()) {
        Ok(session) => session,
        Err(error) => {
            shared.request_stop();
            let _ = startup_sender.send(Err(SessionControlStartError::Project(error)));
            return shutdown_report(
                &shared,
                SessionTermination::EngineError("project session startup failed".to_owned()),
                Tick(0),
            );
        }
    };
    if let Some(port) = preparation_port.as_mut()
        && let Err(error) = port.bind_session(session.engine().world().session())
    {
        shared.request_stop();
        let _ = startup_sender.send(Err(SessionControlStartError::PreparationPort(error)));
        let _ = session.engine().shutdown();
        return shutdown_report(
            &shared,
            SessionTermination::EngineError("preparation port could not bind".to_owned()),
            Tick(0),
        );
    }
    #[cfg(feature = "audio")]
    let audio = match audio_options {
        Some(options) => match ProjectAudioRuntime::start(
            session.project(),
            session.engine().world().session(),
            options,
        ) {
            Ok(audio) => Some(audio),
            Err(error) => {
                shared.request_stop();
                let _ = startup_sender.send(Err(SessionControlStartError::Audio(error)));
                let _ = session.engine().shutdown();
                return shutdown_report(
                    &shared,
                    SessionTermination::EngineError("project audio startup failed".to_owned()),
                    Tick(0),
                );
            }
        },
        None => None,
    };
    let initial_snapshot = session.engine_mut().publish_current_snapshot();
    if !matches!(initial_snapshot, SnapshotPublication::Published { .. }) {
        shared.request_stop();
        let _ = startup_sender.send(Err(SessionControlStartError::InitialSnapshot(
            initial_snapshot,
        )));
        #[cfg(feature = "audio")]
        if let Some(audio) = audio {
            audio.shutdown();
        }
        let _ = session.engine().shutdown();
        return shutdown_report(
            &shared,
            SessionTermination::EngineError(
                "project session could not publish its tick-zero snapshot".to_owned(),
            ),
            Tick(0),
        );
    }
    let mut completed_replay = HashMap::new();
    if completed_replay
        .try_reserve(config.limits.replay_ledger_capacity.saturating_add(1))
        .is_err()
    {
        shared.request_stop();
        #[cfg(feature = "audio")]
        if let Some(audio) = audio {
            audio.shutdown();
        }
        let _ = session.engine().shutdown();
        let _ = startup_sender.send(Err(SessionControlStartError::StartupFailed(
            "could not reserve bounded command replay storage".to_owned(),
        )));
        return shutdown_report(
            &shared,
            SessionTermination::EngineError("could not reserve command replay ledger".to_owned()),
            Tick(0),
        );
    }
    let mut replay_order = VecDeque::new();
    if replay_order
        .try_reserve(config.limits.replay_ledger_capacity.saturating_add(1))
        .is_err()
    {
        shared.request_stop();
        #[cfg(feature = "audio")]
        if let Some(audio) = audio {
            audio.shutdown();
        }
        let _ = session.engine().shutdown();
        let _ = startup_sender.send(Err(SessionControlStartError::StartupFailed(
            "could not reserve bounded command replay storage".to_owned(),
        )));
        return shutdown_report(
            &shared,
            SessionTermination::EngineError("could not reserve command replay ledger".to_owned()),
            Tick(0),
        );
    }
    let active_project_digest = session.project().digest().to_owned();
    let mut actor = ControlActor {
        session,
        config,
        shared: Arc::clone(&shared),
        last_advance: Instant::now(),
        last_published_sequence: 0,
        last_authority_snapshot_sequence: 0,
        frame_needs_publish: true,
        active_project_generation: 1,
        active_project_digest,
        next_server_command_id: 1,
        caller_command_high_water: None,
        pending_commands: HashMap::new(),
        completed_replay,
        replay_order,
        preparation_port,
        last_issued_preparation_epoch: 0,
        cancelled_preparation_epoch: None,
        last_issued_activation_sequence: 0,
        current_activation: None,
        latest_activation_failure: None,
        staged_project: None,
        deferred_stage_result: None,
        #[cfg(feature = "audio")]
        audio,
        active_request: None,
    };
    if let Err(error) = actor.publish_latest_frame() {
        shared.request_stop();
        let _ = startup_sender.send(Err(SessionControlStartError::InitialFramePublication(
            error,
        )));
        #[cfg(feature = "audio")]
        if let Some(audio) = actor.audio.take() {
            audio.shutdown();
        }
        let _ = actor.session.engine().shutdown();
        return shutdown_report(
            &shared,
            SessionTermination::EngineError(
                "project session could not publish its initial tick-zero frame".to_owned(),
            ),
            Tick(0),
        );
    }
    let _ = startup_sender.send(Ok(()));

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

    #[cfg(feature = "audio")]
    if let Some(audio) = actor.audio.take() {
        audio.shutdown();
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
            if self
                .preparation_port
                .as_ref()
                .is_some_and(ProjectPreparationPort::worker_panicked)
            {
                return Err("project preparation worker panicked".to_owned());
            }
            self.poll_activation_stage_results()?;
            #[cfg(feature = "audio")]
            if let Some(audio) = self.audio.as_mut() {
                audio
                    .reconcile()
                    .map_err(|error| format!("audio service reconciliation failed: {error}"))?;
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
                    let _ = self.publish_latest_frame();
                }
            }
            if self.frame_needs_publish {
                self.publish_latest_frame()?;
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
            SessionOperation::AssetPreparation(operation) => {
                self.preparation_operation(cell, credit, operation);
            }
            SessionOperation::AssetActivation(operation) => {
                self.asset_activation_operation(cell, credit, operation);
            }
            #[cfg(feature = "audio")]
            SessionOperation::Audio(operation) => {
                self.audio_operation(cell, credit, operation);
            }
        }
    }

    fn preparation_operation(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        operation: AssetPreparationOperation,
    ) {
        match operation {
            AssetPreparationOperation::Status => {
                if MAX_PREPARATION_STATUS_RESULT_BYTES > MAX_RESULT_BYTES_PER_TICKET {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
                    return;
                }
                let session = self.session.engine().world().session();
                let result = self
                    .preparation_port
                    .as_ref()
                    .ok_or(SessionRequestError::AssetPreparationUnavailable)
                    .and_then(|port| {
                        port.try_status(session)
                            .map(Box::new)
                            .map_err(preparation_port_error)
                    })
                    .map(|status| {
                        SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(
                            status,
                        ))
                    });
                self.finish_cell(cell, credit, result);
            }
            mutation @ (AssetPreparationOperation::Prepare { .. }
            | AssetPreparationOperation::Cancel { .. }) => {
                self.submit_preparation_command(cell, credit, mutation);
            }
        }
    }

    fn asset_activation_operation(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        operation: AssetActivationOperation,
    ) {
        match operation {
            AssetActivationOperation::Status => {
                if MAX_PREPARATION_STATUS_RESULT_BYTES > MAX_RESULT_BYTES_PER_TICKET {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
                    return;
                }
                let session = self.session.engine().world().session();
                let result = self
                    .preparation_port
                    .as_ref()
                    .ok_or(SessionRequestError::AssetActivationUnavailable)
                    .and_then(|port| {
                        let (state, memory) =
                            port.activation_snapshot().map_err(activation_port_error)?;
                        Ok(AssetActivationStatus::new(
                            session,
                            state,
                            self.active_project_generation,
                            self.active_project_digest.clone(),
                            self.current_activation.clone(),
                            self.latest_activation_failure.clone(),
                            memory,
                        ))
                    })
                    .map(Box::new)
                    .map(AssetActivationRequestResult::Status)
                    .map(SessionRequestResult::AssetActivation);
                self.finish_cell(cell, credit, result);
            }
            mutation @ (AssetActivationOperation::Stage { .. }
            | AssetActivationOperation::Commit { .. }
            | AssetActivationOperation::Cancel { .. }) => {
                self.submit_asset_activation_command(cell, credit, mutation);
            }
        }
    }

    fn submit_asset_activation_command(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        operation: AssetActivationOperation,
    ) {
        let requested_id = match &operation {
            AssetActivationOperation::Stage { command_id, .. }
            | AssetActivationOperation::Commit { command_id, .. }
            | AssetActivationOperation::Cancel { command_id, .. } => *command_id,
            AssetActivationOperation::Status => unreachable!("status has no command ID"),
        };
        let (command_id, caller_id) = match requested_id {
            Some(command_id) => (command_id, true),
            None => match self.next_server_command_id() {
                Some(command_id) => (command_id, false),
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
        let Some((cell, credit)) =
            self.replay_existing_command(cell, credit, command_id, caller_id)
        else {
            return;
        };
        if self.preparation_port.is_none() {
            self.finish_cell(
                cell,
                credit,
                Err(SessionRequestError::AssetActivationUnavailable),
            );
            return;
        }

        let session = self.session.engine().world().session();
        let outcome = match operation {
            AssetActivationOperation::Stage {
                candidate,
                expected_active_generation,
                ..
            } => {
                if candidate.session() != session || candidate.epoch() == 0 {
                    self.finish_cell(cell, credit, Err(SessionRequestError::InvalidCandidate));
                    return;
                }
                if candidate.epoch() > self.last_issued_preparation_epoch {
                    self.finish_cell(cell, credit, Err(SessionRequestError::InvalidCandidate));
                    return;
                }
                if candidate.epoch() < self.last_issued_preparation_epoch {
                    self.finish_cell(cell, credit, Err(SessionRequestError::CandidateExpired));
                    return;
                }
                if expected_active_generation != self.active_project_generation {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ActiveGenerationMismatch),
                    );
                    return;
                }
                let preparation = match self
                    .preparation_port
                    .as_ref()
                    .expect("activation port checked above")
                    .try_status(session)
                {
                    Ok(status) => status,
                    Err(error) => {
                        self.finish_cell(cell, credit, Err(preparation_port_error(error)));
                        return;
                    }
                };
                if matches!(
                    preparation.state(),
                    crate::PreparationState::Pending | crate::PreparationState::Preparing
                ) {
                    self.finish_cell(cell, credit, Err(SessionRequestError::PreparationBusy));
                    return;
                }
                let Some(ready) = preparation.ready_candidate() else {
                    self.finish_cell(cell, credit, Err(SessionRequestError::CandidateNotActive));
                    return;
                };
                if ready.id() != candidate || !ready.is_current() {
                    self.finish_cell(cell, credit, Err(SessionRequestError::CandidateNotActive));
                    return;
                }
                let Some(sequence) = self.last_issued_activation_sequence.checked_add(1) else {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ActivationSequenceExhausted),
                    );
                    return;
                };
                if let Err(error) = self.prepare_replay_slot() {
                    self.finish_cell(cell, credit, Err(error));
                    return;
                }
                if std::mem::size_of::<AssetActivationCommandResult>() + 128
                    > MAX_RESULT_BYTES_PER_TICKET
                {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
                    return;
                }
                let activation = ActivationId::from_parts(session, sequence);
                let accepted = self
                    .preparation_port
                    .as_ref()
                    .expect("activation port checked above")
                    .try_stage(sequence, candidate.epoch(), expected_active_generation);
                if let Err(error) = accepted {
                    self.finish_cell(cell, credit, Err(activation_port_error(error)));
                    return;
                }
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = Some(command_id);
                }
                self.last_issued_activation_sequence = sequence;
                self.current_activation = Some(crate::AssetActivationSummary::new(
                    activation,
                    candidate,
                    expected_active_generation,
                    ready.source_digest().to_owned(),
                    None,
                ));
                self.latest_activation_failure = None;
                self.staged_project = None;
                self.deferred_stage_result = None;
                self.drain_stage_results();
                AssetActivationCommandOutcome::StageAccepted {
                    activation,
                    candidate,
                }
            }
            AssetActivationOperation::Commit {
                activation,
                expected_active_generation,
                ..
            } => {
                if let Err(error) = self.validate_activation_id(activation, session) {
                    self.finish_cell(cell, credit, Err(error));
                    return;
                }
                if expected_active_generation != self.active_project_generation {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ActiveGenerationMismatch),
                    );
                    return;
                }
                let Some(staged) = self.staged_project.as_ref() else {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ActivationNotStaged));
                    return;
                };
                if staged.activation != activation
                    || staged.expected_active_generation != expected_active_generation
                {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ActivationNotStaged));
                    return;
                }
                let candidate = staged.candidate;
                let new_digest = staged.project.digest().to_owned();
                let project = Arc::clone(&staged.project);
                let Some(generation) = self.active_project_generation.checked_add(1) else {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ProjectGenerationExhausted),
                    );
                    return;
                };
                if self.last_published_sequence.checked_add(1).is_none() {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::ActorFailed(
                            "session presentation frame sequence space is exhausted".to_owned(),
                        )),
                    );
                    return;
                }
                if let Err(error) = self.prepare_replay_slot() {
                    self.finish_cell(cell, credit, Err(error));
                    return;
                }
                if std::mem::size_of::<AssetActivationCommandResult>() + 128
                    > MAX_RESULT_BYTES_PER_TICKET
                {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
                    return;
                }
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = Some(command_id);
                }
                let mut displaced = None;
                let port = self
                    .preparation_port
                    .as_ref()
                    .expect("activation port checked above");
                let session = &mut self.session;
                let accepted = port.try_commit_activation(
                    activation.sequence(),
                    candidate.epoch(),
                    expected_active_generation,
                    generation,
                    new_digest.clone(),
                    || {
                        displaced = Some(session.replace_project_source(project));
                    },
                );
                if let Err(error) = accepted {
                    if let Some(active) = self.active_request.as_mut() {
                        active.possible_command_id = None;
                    }
                    self.finish_cell(cell, credit, Err(activation_port_error(error)));
                    return;
                }
                if caller_id {
                    self.caller_command_high_water = Some(
                        self.caller_command_high_water
                            .map_or(command_id.get(), |current| current.max(command_id.get())),
                    );
                }
                // The worker registry still owns the old source; this local drop cannot be its
                // final large deallocation. Promotion/reclamation occurs on the worker thread.
                drop(displaced.take());
                self.active_project_generation = generation;
                self.active_project_digest = new_digest.clone();
                self.current_activation = Some(crate::AssetActivationSummary::new(
                    activation,
                    candidate,
                    expected_active_generation,
                    new_digest,
                    Some(generation),
                ));
                self.latest_activation_failure = None;
                self.staged_project = None;
                self.frame_needs_publish = true;
                AssetActivationCommandOutcome::CommitApplied {
                    activation,
                    generation,
                }
            }
            AssetActivationOperation::Cancel { activation, .. } => {
                if let Err(error) = self.validate_activation_id(activation, session) {
                    self.finish_cell(cell, credit, Err(error));
                    return;
                }
                if self
                    .current_activation
                    .as_ref()
                    .is_some_and(|summary| summary.committed_generation().is_some())
                {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ActivationNotStaged));
                    return;
                }
                if let Err(error) = self.prepare_replay_slot() {
                    self.finish_cell(cell, credit, Err(error));
                    return;
                }
                if std::mem::size_of::<AssetActivationCommandResult>() + 128
                    > MAX_RESULT_BYTES_PER_TICKET
                {
                    self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
                    return;
                }
                if let Some(active) = self.active_request.as_mut() {
                    active.possible_command_id = Some(command_id);
                }
                if let Err(error) = self
                    .preparation_port
                    .as_ref()
                    .expect("activation port checked above")
                    .try_cancel_activation(activation.sequence())
                {
                    if let Some(active) = self.active_request.as_mut() {
                        active.possible_command_id = None;
                    }
                    self.finish_cell(cell, credit, Err(activation_port_error(error)));
                    return;
                }
                self.staged_project = None;
                self.drain_stage_results();
                AssetActivationCommandOutcome::CancelAccepted { activation }
            }
            AssetActivationOperation::Status => unreachable!("status handled above"),
        };

        #[cfg(test)]
        if self
            .shared
            .panic_after_activation_command_effect
            .swap(false, Ordering::AcqRel)
        {
            panic!("injected actor failure after asset activation command effect");
        }

        if caller_id {
            self.caller_command_high_water = Some(
                self.caller_command_high_water
                    .map_or(command_id.get(), |current| current.max(command_id.get())),
            );
        }
        let observed_tick = self.session.engine().clock().current_tick();
        let command = AssetActivationCommandResult::new(command_id, observed_tick, outcome);
        self.remember_completed(
            command_id,
            ReplayEntry {
                apply_tick: None,
                resolve_tick: None,
                result: CachedCommandResult::Activation(command.clone()),
            },
        );
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = None;
        }
        self.finish_cell(
            cell,
            credit,
            Ok(SessionRequestResult::AssetActivation(
                AssetActivationRequestResult::Command(command),
            )),
        );
    }

    fn validate_activation_id(
        &self,
        activation: ActivationId,
        session: SessionId,
    ) -> Result<(), SessionRequestError> {
        if activation.session() != session
            || activation.sequence() == 0
            || activation.sequence() > self.last_issued_activation_sequence
        {
            return Err(SessionRequestError::InvalidActivation);
        }
        if activation.sequence() < self.last_issued_activation_sequence {
            return Err(SessionRequestError::ActivationExpired);
        }
        if !self
            .current_activation
            .as_ref()
            .is_some_and(|summary| summary.id() == activation)
        {
            return Err(SessionRequestError::ActivationExpired);
        }
        Ok(())
    }

    fn poll_activation_stage_results(&mut self) -> Result<(), String> {
        if let Some(result) = self.deferred_stage_result.take()
            && !self.consume_activation_stage_result(result)
        {
            return Ok(());
        }
        loop {
            let result = self
                .preparation_port
                .as_mut()
                .and_then(ProjectPreparationPort::try_take_stage_result);
            let Some(result) = result else {
                break;
            };
            if !self.consume_activation_stage_result(result) {
                break;
            }
        }
        Ok(())
    }

    fn drain_stage_results(&mut self) {
        self.deferred_stage_result = None;
        while let Some(result) = self
            .preparation_port
            .as_mut()
            .and_then(ProjectPreparationPort::try_take_stage_result)
        {
            drop(result);
        }
    }

    fn consume_activation_stage_result(&mut self, result: StageResult) -> bool {
        match result {
            StageResult::Ready {
                sequence,
                epoch,
                project,
            } => {
                let session = self.session.engine().world().session();
                let activation = ActivationId::from_parts(session, sequence);
                let candidate = PreparationId::from_parts(session, epoch);
                let current = self.current_activation.as_ref().is_some_and(|summary| {
                    summary.id() == activation
                        && summary.candidate() == candidate
                        && summary.expected_active_generation() == self.active_project_generation
                }) && epoch == self.last_issued_preparation_epoch
                    && self.cancelled_preparation_epoch != Some(epoch);
                if !current {
                    drop(project);
                    return true;
                }
                if let Some(port) = self.preparation_port.as_ref() {
                    match port.mark_stage_result(sequence, epoch, true) {
                        Ok(()) => {}
                        Err(ProjectPreparationPortError::Busy) => {
                            self.deferred_stage_result = Some(StageResult::Ready {
                                sequence,
                                epoch,
                                project,
                            });
                            return false;
                        }
                        Err(_) => {
                            drop(project);
                            return true;
                        }
                    }
                } else {
                    drop(project);
                    return true;
                }
                self.latest_activation_failure = None;
                self.staged_project = Some(StagedProjectCandidate {
                    activation,
                    candidate,
                    expected_active_generation: self.active_project_generation,
                    project,
                });
                true
            }
            StageResult::Incompatible {
                sequence,
                epoch,
                error,
            } => {
                let session = self.session.engine().world().session();
                let activation = ActivationId::from_parts(session, sequence);
                if self
                    .current_activation
                    .as_ref()
                    .is_some_and(|summary| summary.id() == activation)
                    && epoch == self.last_issued_preparation_epoch
                    && self.cancelled_preparation_epoch != Some(epoch)
                {
                    if let Some(port) = self.preparation_port.as_ref() {
                        match port.mark_stage_result(sequence, epoch, false) {
                            Ok(()) => {}
                            Err(ProjectPreparationPortError::Busy) => {
                                self.deferred_stage_result = Some(StageResult::Incompatible {
                                    sequence,
                                    epoch,
                                    error,
                                });
                                return false;
                            }
                            Err(_) => return true,
                        }
                    } else {
                        return true;
                    }
                    let mut truncated = false;
                    let message =
                        bounded_activation_text(&error.to_string(), 2_048, &mut truncated);
                    self.latest_activation_failure = Some(crate::AssetActivationFailure::new(
                        activation,
                        "incompatible_topology".to_owned(),
                        message,
                        Some(error),
                    ));
                    let _ = epoch;
                }
                true
            }
            StageResult::Failed { sequence, epoch } => {
                let session = self.session.engine().world().session();
                let activation = ActivationId::from_parts(session, sequence);
                if self
                    .current_activation
                    .as_ref()
                    .is_some_and(|summary| summary.id() == activation)
                    && epoch == self.last_issued_preparation_epoch
                    && self.cancelled_preparation_epoch != Some(epoch)
                {
                    if let Some(port) = self.preparation_port.as_ref() {
                        match port.mark_stage_result(sequence, epoch, false) {
                            Ok(()) => {}
                            Err(ProjectPreparationPortError::Busy) => {
                                self.deferred_stage_result =
                                    Some(StageResult::Failed { sequence, epoch });
                                return false;
                            }
                            Err(_) => return true,
                        }
                    } else {
                        return true;
                    }
                    self.latest_activation_failure = Some(crate::AssetActivationFailure::new(
                        activation,
                        "internal".to_owned(),
                        "the prepared candidate could not be staged".to_owned(),
                        None,
                    ));
                    let _ = epoch;
                }
                true
            }
        }
    }

    fn submit_preparation_command(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        operation: AssetPreparationOperation,
    ) {
        let (requested_id, caller_id) = match &operation {
            AssetPreparationOperation::Prepare { command_id }
            | AssetPreparationOperation::Cancel { command_id, .. } => {
                (*command_id, command_id.is_some())
            }
            AssetPreparationOperation::Status => unreachable!("status has no command ID"),
        };
        let (command_id, caller_id) = match requested_id {
            Some(id) => (id, caller_id),
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
        let Some((cell, credit)) =
            self.replay_existing_command(cell, credit, command_id, caller_id)
        else {
            return;
        };

        let current_session = self.session.engine().world().session();
        let candidate = match &operation {
            AssetPreparationOperation::Prepare { .. } => {
                let Some(epoch) = self.last_issued_preparation_epoch.checked_add(1) else {
                    self.finish_cell(
                        cell,
                        credit,
                        Err(SessionRequestError::PreparationEpochExhausted),
                    );
                    return;
                };
                PreparationId::from_parts(current_session, epoch)
            }
            AssetPreparationOperation::Cancel { candidate, .. } => {
                if candidate.epoch() == 0 || candidate.session() != current_session {
                    self.finish_cell(cell, credit, Err(SessionRequestError::InvalidCandidate));
                    return;
                }
                if candidate.epoch() > self.last_issued_preparation_epoch {
                    self.finish_cell(cell, credit, Err(SessionRequestError::InvalidCandidate));
                    return;
                }
                if candidate.epoch() < self.last_issued_preparation_epoch {
                    self.finish_cell(cell, credit, Err(SessionRequestError::CandidateExpired));
                    return;
                }
                *candidate
            }
            AssetPreparationOperation::Status => unreachable!("status handled above"),
        };

        if let Err(error) = self.prepare_replay_slot() {
            self.finish_cell(cell, credit, Err(error));
            return;
        }
        if std::mem::size_of::<AssetPreparationCommandResult>() + 128 > MAX_RESULT_BYTES_PER_TICKET
        {
            self.finish_cell(cell, credit, Err(SessionRequestError::ResultTooLarge));
            return;
        }
        if self.preparation_port.is_none() {
            self.finish_cell(
                cell,
                credit,
                Err(SessionRequestError::AssetPreparationUnavailable),
            );
            return;
        }
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = Some(command_id);
        }
        let accepted = match (&operation, self.preparation_port.as_ref()) {
            (AssetPreparationOperation::Prepare { .. }, Some(port)) => {
                port.try_prepare(candidate.epoch())
            }
            (AssetPreparationOperation::Cancel { .. }, Some(port)) => {
                port.try_cancel(candidate.epoch())
            }
            _ => Err(ProjectPreparationPortError::Stopped),
        };
        if let Err(error) = accepted {
            if let Some(active) = self.active_request.as_mut() {
                active.possible_command_id = None;
            }
            self.finish_cell(cell, credit, Err(preparation_port_error(error)));
            return;
        }

        if matches!(&operation, AssetPreparationOperation::Prepare { .. }) {
            self.last_issued_preparation_epoch = candidate.epoch();
            self.cancelled_preparation_epoch = None;
            self.drain_stage_results();
        } else {
            self.cancelled_preparation_epoch = Some(candidate.epoch());
            self.drain_stage_results();
        }
        self.staged_project = None;
        if caller_id {
            self.caller_command_high_water = Some(
                self.caller_command_high_water
                    .map_or(command_id.get(), |current| current.max(command_id.get())),
            );
        }

        #[cfg(test)]
        if self
            .shared
            .panic_after_preparation_command_effect
            .swap(false, Ordering::AcqRel)
        {
            panic!("injected actor failure after preparation command effect");
        }
        let observed_tick = self.session.engine().clock().current_tick();
        let outcome = match operation {
            AssetPreparationOperation::Prepare { .. } => {
                AssetPreparationCommandOutcome::PrepareAccepted { candidate }
            }
            AssetPreparationOperation::Cancel { .. } => {
                AssetPreparationCommandOutcome::CancelAccepted { candidate }
            }
            AssetPreparationOperation::Status => unreachable!("status handled above"),
        };
        let command = AssetPreparationCommandResult::new(command_id, observed_tick, outcome);
        let replay = ReplayEntry {
            apply_tick: None,
            resolve_tick: None,
            result: CachedCommandResult::Preparation(command.clone()),
        };
        self.remember_completed(command_id, replay);
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = None;
        }
        self.finish_cell(
            cell,
            credit,
            Ok(SessionRequestResult::AssetPreparation(
                AssetPreparationResult::Command(command),
            )),
        );
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

        let Some((cell, credit)) =
            self.replay_existing_command(cell, credit, command_id, caller_id)
        else {
            return;
        };

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
        if let Err(error) = self.prepare_replay_slot() {
            self.finish_cell(cell, credit, Err(error));
            return;
        }
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

    #[cfg(feature = "audio")]
    fn audio_operation(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        operation: SessionAudioOperation,
    ) {
        match operation {
            SessionAudioOperation::Command {
                command_id,
                mutation,
            } => self.submit_audio_command(cell, credit, command_id, mutation),
            query => {
                let tick = self.session.engine().clock().current_tick();
                let result = match self.audio.as_mut() {
                    Some(audio) => audio
                        .query(
                            query,
                            tick,
                            self.config.limits.max_page_entities,
                            MAX_RESULT_BYTES_PER_TICKET,
                        )
                        .map(SessionRequestResult::Audio)
                        .map_err(SessionRequestError::Audio),
                    None => Err(SessionRequestError::AudioUnavailable),
                };
                self.finish_cell(cell, credit, result);
            }
        }
    }

    #[cfg(feature = "audio")]
    fn submit_audio_command(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        requested_id: Option<CommandId>,
        mutation: crate::SessionAudioMutation,
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

        let Some((cell, credit)) =
            self.replay_existing_command(cell, credit, command_id, caller_id)
        else {
            return;
        };

        if self.audio.is_none() {
            self.finish_cell(cell, credit, Err(SessionRequestError::AudioUnavailable));
            return;
        }
        if let Err(error) = self.prepare_replay_slot() {
            self.finish_cell(cell, credit, Err(error));
            return;
        }

        // Check both bounded ledger containers before the side effect. Advance caller high-water
        // before execution so an abnormal exit is conservative about an accepted command.
        if caller_id {
            self.caller_command_high_water = Some(
                self.caller_command_high_water
                    .map_or(command_id.get(), |current| current.max(command_id.get())),
            );
        }
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = Some(command_id);
        }
        let observed_tick = self.session.engine().clock().current_tick();
        let result = self
            .audio
            .as_mut()
            .expect("audio sidecar checked above")
            .execute(command_id, mutation, observed_tick);
        #[cfg(test)]
        {
            self.shared
                .audio_command_effects
                .fetch_add(1, Ordering::AcqRel);
            if self
                .shared
                .panic_after_audio_command_effect
                .swap(false, Ordering::AcqRel)
            {
                panic!("injected actor failure after audio command effect");
            }
        }
        self.remember_completed(
            command_id,
            ReplayEntry {
                apply_tick: None,
                resolve_tick: None,
                result: CachedCommandResult::Audio(result.clone()),
            },
        );
        if let Some(active) = self.active_request.as_mut() {
            active.possible_command_id = None;
        }
        self.finish_cell(
            cell,
            credit,
            Ok(SessionRequestResult::Audio(SessionAudioResult::Command(
                result,
            ))),
        );
    }

    fn next_server_command_id(&mut self) -> Option<CommandId> {
        if self.next_server_command_id >= MAX_CALLER_COMMAND_ID {
            return None;
        }
        let raw = SERVER_COMMAND_ID_PREFIX | self.next_server_command_id;
        self.next_server_command_id = self.next_server_command_id.checked_add(1)?;
        Some(CommandId::from_raw(raw))
    }

    /// Share one session-wide pending/completed replay path across world and audio mutations.
    /// An original world command keeps its real authority receipt; an original audio command
    /// returns only its typed terminal result.
    fn replay_existing_command(
        &mut self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        command_id: CommandId,
        caller_id: bool,
    ) -> Option<(Arc<RequestCell>, Arc<CreditBundle>)> {
        if !caller_id {
            return Some((cell, credit));
        }
        if let Some(pending) = self.pending_commands.get_mut(&command_id) {
            cell.set_authority(AuthorityAdmission {
                command_id,
                submission: CommandSubmission::DuplicatePending,
                apply_tick: Some(pending.apply_tick),
            });
            pending.tickets.push(PendingTicket { cell, credit });
            return None;
        }
        match self.completed_replay_for(command_id, true) {
            Ok(Some(replay)) => {
                self.finish_replayed_command(cell, credit, command_id, replay);
                None
            }
            Ok(None) => Some((cell, credit)),
            Err(error) => {
                self.finish_cell(cell, credit, Err(error));
                None
            }
        }
    }

    fn finish_replayed_command(
        &self,
        cell: Arc<RequestCell>,
        credit: Arc<CreditBundle>,
        command_id: CommandId,
        replay: ReplayEntry,
    ) {
        let result = match replay.result {
            CachedCommandResult::World(result) => {
                cell.set_authority(AuthorityAdmission {
                    command_id,
                    submission: CommandSubmission::DuplicateCompleted(result.clone()),
                    apply_tick: replay.apply_tick,
                });
                SessionRequestResult::Command(CommandTerminalResult {
                    result,
                    resolve_tick: replay.resolve_tick,
                })
            }
            #[cfg(feature = "audio")]
            CachedCommandResult::Audio(result) => {
                SessionRequestResult::Audio(SessionAudioResult::Command(result))
            }
            CachedCommandResult::Preparation(result) => {
                SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(result))
            }
            CachedCommandResult::Activation(result) => {
                SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(result))
            }
        };
        self.finish_cell(cell, credit, Ok(result));
    }

    fn completed_replay_for(
        &self,
        command_id: CommandId,
        caller_id: bool,
    ) -> Result<Option<ReplayEntry>, SessionRequestError> {
        if !caller_id {
            return Ok(None);
        }
        if let Some(replay) = self.completed_replay.get(&command_id) {
            return Ok(Some(replay.clone()));
        }
        if let Some(high_water) = self.caller_command_high_water
            && command_id.get() <= high_water
        {
            return Err(SessionRequestError::ReplayWindowExpired {
                command_id: command_id.get(),
                high_water,
            });
        }
        Ok(None)
    }

    fn prepare_replay_slot(&mut self) -> Result<(), SessionRequestError> {
        self.completed_replay
            .try_reserve(1)
            .map_err(|_| replay_storage_error())?;
        self.replay_order
            .try_reserve(1)
            .map_err(|_| replay_storage_error())
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
                    let _ = self.publish_latest_frame();
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
                    result: CachedCommandResult::World(result.clone()),
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
        } else if self.completed_replay.len() >= self.config.limits.replay_ledger_capacity
            && let Some(oldest) = self.replay_order.pop_front()
        {
            self.completed_replay.remove(&oldest);
        }
        self.completed_replay.insert(id, entry);
        self.replay_order.push_back(id);
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

    fn publish_latest_frame(&mut self) -> Result<(), String> {
        let snapshot = self
            .session
            .latest_snapshot()
            .map_err(|error| format!("snapshot read failed: {error}"))?
            .ok_or_else(|| "no Engine snapshot has been published".to_owned())?;
        if snapshot.sequence <= self.last_authority_snapshot_sequence && !self.frame_needs_publish {
            return Ok(());
        }
        let authority_snapshot_sequence = snapshot.sequence;
        let sequence = if authority_snapshot_sequence > self.last_published_sequence {
            snapshot.sequence
        } else {
            let Some(sequence) = self.last_published_sequence.checked_add(1) else {
                return Err("session presentation frame sequence space is exhausted".to_owned());
            };
            sequence
        };
        let prepared = self.session.project();
        let frame = SessionFrame {
            session: self.session.engine().world().session(),
            tick: snapshot.tick,
            sequence,
            authority_snapshot_sequence,
            project_generation: self.active_project_generation,
            source_digest: self.active_project_digest.clone(),
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
                self.last_published_sequence = sequence;
                self.last_authority_snapshot_sequence = authority_snapshot_sequence;
                self.frame_needs_publish = false;
            }
            Err(MailboxError::Busy | MailboxError::AtCapacity) => {
                self.frame_needs_publish = true;
            }
            Err(MailboxError::Poisoned) => {
                return Err("session frame mailbox was poisoned".to_owned());
            }
        }
        Ok(())
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
        SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(_)) => {
            MAX_PREPARATION_STATUS_RESULT_BYTES
        }
        SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_)) => {
            std::mem::size_of::<AssetPreparationCommandResult>() + 128
        }
        SessionRequestResult::AssetActivation(AssetActivationRequestResult::Status(_)) => {
            MAX_PREPARATION_STATUS_RESULT_BYTES
        }
        SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(_)) => {
            std::mem::size_of::<AssetActivationCommandResult>() + 128
        }
        #[cfg(feature = "audio")]
        SessionRequestResult::Audio(audio) => estimate_audio_result_bytes(audio),
    }
}

fn bounded_activation_text(input: &str, max_bytes: usize, truncated: &mut bool) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    *truncated = true;
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_owned()
}

#[cfg(feature = "audio")]
fn estimate_audio_result_bytes(result: &SessionAudioResult) -> usize {
    if let Some(status) = result.as_status() {
        return std::mem::size_of_val(status);
    }
    if let Some(page) = result.as_clips() {
        return page
            .clips()
            .iter()
            .fold(std::mem::size_of_val(page), |bytes, clip| {
                bytes
                    .saturating_add(std::mem::size_of_val(clip))
                    .saturating_add(clip.asset_id().as_str().len())
            });
    }
    if let Some(voice) = result.as_voice() {
        return std::mem::size_of_val(voice)
            .saturating_add(voice.clip_id().map_or(0, |clip_id| clip_id.as_str().len()));
    }
    if let Some(page) = result.as_voices() {
        return page
            .voices()
            .iter()
            .fold(std::mem::size_of_val(page), |bytes, voice| {
                bytes
                    .saturating_add(std::mem::size_of_val(voice))
                    .saturating_add(voice.clip_id().map_or(0, |clip_id| clip_id.as_str().len()))
            });
    }
    result.as_command().map_or(0, |command| {
        std::mem::size_of_val(command).saturating_add(512)
    })
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
    use std::sync::Arc;
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
    use crate::project_preparation::PreparationTestProject;
    use crate::{
        AssetActivationCommandOutcome, AssetActivationOperation, AssetActivationRequestResult,
        AssetActivationState, AssetPreparationCommandOutcome, AssetPreparationOperation,
        AssetPreparationResult, NativeProjectActivationService, NativeProjectActivationShutdown,
        NativeProjectPreparationOptions, NativeProjectPreparationService, PreparationId,
        PreparationState, PreparationWorkerShutdown,
    };

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

    #[cfg(feature = "audio")]
    fn start_audio(config: SessionControlConfig) -> SessionControlService {
        start_audio_project(prepared(ONE_ENTITY_SCENE), config)
    }

    #[cfg(feature = "audio")]
    fn start_audio_project(
        project: PreparedProject,
        config: SessionControlConfig,
    ) -> SessionControlService {
        SessionControlService::start_shared_with_audio(
            Arc::new(project),
            config,
            crate::SessionAudioOptions::offline(8_000),
        )
        .expect("start project control service with offline audio")
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
    fn client_stop_signal_is_distinct_from_retained_latest_frame_availability() {
        let service = start(default_config());
        let client = service.attach().expect("attach session client");
        assert!(!client.is_stopping());

        let initial = wait_for_frame(&client);
        assert_eq!(initial.tick(), Tick(0));
        service.request_stop();
        assert!(client.is_stopping());

        let retained = client
            .try_latest_frame()
            .expect("latest frame mailbox should remain readable")
            .expect("last good frame remains available after stop");
        assert!(Arc::ptr_eq(&initial, &retained));
        assert_eq!(retained.tick(), Tick(0));

        let report = service.shutdown();
        assert_eq!(report.termination, SessionTermination::StopRequested);
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
    fn shared_service_publishes_actual_tick_zero_before_start_returns() {
        let source = Arc::new(prepared(ONE_ENTITY_SCENE));
        let service = SessionControlService::start_shared(Arc::clone(&source), default_config())
            .expect("start shared project service");
        assert_eq!(
            Arc::strong_count(&source),
            2,
            "service shares the source Arc"
        );

        let client = service.attach().unwrap();
        let first_frame = wait_for_frame(&client);
        assert_eq!(first_frame.tick(), Tick(0));
        assert_eq!(first_frame.sequence(), 1);
        assert_eq!(first_frame.project_generation(), 1);
        assert_eq!(first_frame.source_digest(), source.digest());
        assert_eq!(first_frame.source_scene_revision(), source.scene().revision);
        assert_eq!(first_frame.snapshot().tick, Tick(0));
        assert_eq!(first_frame.snapshot().entities.len(), 1);
        assert_eq!(
            first_frame.snapshot().entities[0].transform.translation,
            [2.0, 3.0, 4.0]
        );

        let mut before = submit_after_busy(
            &client,
            SessionOperation::GetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
            },
        );
        let before_state = wait_terminal(&before);
        let SessionRequestResult::Transform(transform) =
            before_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("initial transform query returned a different result")
        };
        assert_eq!(transform.observed_tick, Tick(0));
        assert_eq!(transform.entity.revision, Revision(0));
        assert_eq!(transform.entity.transform.translation, [2.0, 3.0, 4.0]);

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let step_state = wait_terminal(&step);
        let SessionRequestResult::Step(reports) = step_state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("initial manual step returned a different result")
        };
        assert_eq!(reports[0].tick, Tick(1));
        assert!(matches!(
            &reports[0].snapshot,
            crate::SnapshotPublication::Published { sequence: 2 }
        ));
        let next_frame = wait_for_frame(&client);
        assert_eq!(next_frame.tick(), Tick(1));
        assert_eq!(next_frame.sequence(), 2);

        before.acknowledge().unwrap();
        step.acknowledge().unwrap();
        drop(first_frame);
        drop(next_frame);
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

    #[cfg(feature = "audio")]
    #[test]
    fn audio_replay_is_terminal_without_authority_and_eviction_expires_the_id() {
        let mut config = default_config();
        config.limits.replay_ledger_capacity = 1;
        let service = start_audio(config);
        let client = service.attach().unwrap();

        let mut typed_failure = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(30)),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("missing-clip"),
                    volume: 1.0,
                    pan: 0.0,
                    looping: false,
                },
            }),
        );
        let typed_failure_state = wait_terminal(&typed_failure);
        assert!(matches!(
            typed_failure_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(audio))
                if audio.as_command().unwrap().outcome()
                    == &Err(crate::SessionAudioError::UnknownClip)
        ));
        let mut typed_failure_replay = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(30)),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 1 },
            }),
        );
        let typed_failure_replay_state = wait_terminal(&typed_failure_replay);
        assert_eq!(
            typed_failure_replay_state.terminal(),
            typed_failure_state.terminal()
        );
        typed_failure.acknowledge().unwrap();
        typed_failure_replay.acknowledge().unwrap();
        drop(typed_failure_state);
        drop(typed_failure_replay_state);

        let mut first = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(31)),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 32 },
            }),
        );
        // Audio is immediate, but the stage wait still terminates successfully instead of
        // waiting forever for a world-only AuthorityAdmission.
        let first_state = first
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(3))
            .expect("audio terminal satisfies the authority-stage wait");
        assert_eq!(first_state.phase(), RequestPhase::Terminal);
        assert!(first_state.authority().is_none());
        assert!(matches!(
            first_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(audio))
                if matches!(audio.as_command().unwrap().outcome(),
                    Ok(crate::SessionAudioCommandOutcome::Offline(summary))
                        if summary.frames() == 32)
        ));

        // Same command ID with a different operation replays the first actual typed outcome.
        let mut duplicate = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(31)),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("missing-clip"),
                    volume: 1.0,
                    pan: 0.0,
                    looping: false,
                },
            }),
        );
        let duplicate_state = duplicate
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(3))
            .expect("audio replay is immediately terminal");
        assert_eq!(duplicate_state.phase(), RequestPhase::Terminal);
        assert_eq!(duplicate_state.authority(), None);
        assert_eq!(duplicate_state.terminal(), first_state.terminal());

        let mut next = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(32)),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 64 },
            }),
        );
        let next_state = wait_terminal(&next);
        assert!(matches!(
            next_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(audio))
                if matches!(audio.as_command().unwrap().outcome(),
                    Ok(crate::SessionAudioCommandOutcome::Offline(summary))
                        if summary.frames() == 64 && summary.cumulative_frames() == 96)
        ));

        let mut expired = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(31)),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 1 },
            }),
        );
        let expired_state = wait_terminal(&expired);
        assert_eq!(
            expired_state.terminal().unwrap(),
            &Err(SessionRequestError::ReplayWindowExpired {
                command_id: 31,
                high_water: 32,
            })
        );
        first.acknowledge().unwrap();
        duplicate.acknowledge().unwrap();
        next.acknowledge().unwrap();
        expired.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[cfg(feature = "audio")]
    #[test]
    fn audio_cross_kind_duplicate_preserves_world_admission_and_cached_outcome() {
        let service = start_audio(default_config());
        let client = service.attach().unwrap();
        let id = CommandId::from_raw(41);
        let target = SessionEntityTarget::StableId(stable_id("entity:hero"));
        let mut world = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target,
                command_id: Some(id),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(41.0),
            },
        );
        let accepted = world
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(accepted.authority().unwrap().apply_tick, Some(Tick(1)));

        let mut pending_audio_duplicate = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(id),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 1 },
            }),
        );
        let pending_admission = pending_audio_duplicate
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(pending_admission.phase(), RequestPhase::AuthorityAccepted);
        assert_eq!(
            pending_admission.authority().unwrap().submission,
            CommandSubmission::DuplicatePending
        );
        assert_eq!(
            pending_admission.authority().unwrap().apply_tick,
            Some(Tick(1))
        );

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        let world_result = wait_terminal(&world).terminal().unwrap().clone();
        assert!(matches!(world_result, Ok(SessionRequestResult::Command(_))));
        assert_eq!(
            wait_terminal(&pending_audio_duplicate).terminal().unwrap(),
            &world_result
        );
        pending_audio_duplicate.acknowledge().unwrap();
        world.acknowledge().unwrap();
        step.acknowledge().unwrap();

        let mut completed_audio_duplicate = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(id),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("missing-clip"),
                    volume: 1.0,
                    pan: 0.0,
                    looping: false,
                },
            }),
        );
        let completed_admission = completed_audio_duplicate
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let admission = completed_admission.authority().unwrap();
        assert_eq!(admission.apply_tick, Some(Tick(1)));
        assert!(matches!(
            &admission.submission,
            CommandSubmission::DuplicateCompleted(result)
                if matches!(&world_result,
                    Ok(SessionRequestResult::Command(command))
                        if result == &command.result)
        ));
        assert_eq!(completed_admission.terminal(), Some(&world_result));
        completed_audio_duplicate.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[cfg(feature = "audio")]
    #[test]
    fn audio_result_lease_uses_normal_lane_while_manual_step_keeps_its_reserve() {
        let mut config = default_config();
        config.limits.max_pending_per_client = 2;
        config.limits.max_pending_total = 2;
        config.limits.max_result_bytes_total = 2 * 1024 * 1024;
        let service = start_audio(config);
        let client = service.attach().unwrap();

        let mut audio = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: None,
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 16 },
            }),
        );
        let audio_state = wait_terminal(&audio);
        assert!(audio_state.terminal().unwrap().is_ok());
        assert_eq!(
            client
                .try_submit(SessionOperation::Audio(
                    crate::SessionAudioOperation::Status
                ))
                .err()
                .unwrap(),
            SessionSubmitError::ResultCapacity
        );

        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let step_state = wait_terminal(&step);
        assert!(step_state.terminal().unwrap().is_ok());
        audio.acknowledge().unwrap();
        step.acknowledge().unwrap();
        drop(audio_state);
        drop(step_state);

        let mut status = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Status),
        );
        let status_state = wait_terminal(&status);
        assert!(matches!(
            status_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(audio)) if audio.as_status().is_some()
        ));
        status.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[cfg(feature = "audio")]
    #[test]
    fn audio_mutation_obeys_capability_and_replay_is_shared_with_world_ids() {
        let service = start_audio(default_config());
        let read_only = service
            .attach_with_capabilities(SessionCapabilities {
                inspect: true,
                mutate: false,
                manual_step: false,
            })
            .unwrap();
        assert_eq!(
            read_only
                .try_submit(SessionOperation::Audio(
                    crate::SessionAudioOperation::Command {
                        command_id: None,
                        mutation: crate::SessionAudioMutation::RenderOffline { frames: 8 },
                    }
                ))
                .err()
                .unwrap(),
            SessionSubmitError::CapabilityDenied
        );
        let mut status = submit_after_busy(
            &read_only,
            SessionOperation::Audio(crate::SessionAudioOperation::Status),
        );
        wait_terminal(&status);
        status.acknowledge().unwrap();

        let client = service.attach().unwrap();
        assert_eq!(
            client
                .try_submit(SessionOperation::Audio(
                    crate::SessionAudioOperation::Command {
                        command_id: Some(CommandId::from_raw(1_u128 << 127)),
                        mutation: crate::SessionAudioMutation::RenderOffline { frames: 8 },
                    }
                ))
                .err()
                .unwrap(),
            SessionSubmitError::ReservedCommandId
        );
        let id = CommandId::from_raw(53);
        let mut audio = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(id),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 8 },
            }),
        );
        let audio_result = wait_terminal(&audio).terminal().unwrap().clone();
        audio.acknowledge().unwrap();

        // The world API shares the same lower-half replay namespace. Reusing an audio ID returns
        // its original typed result and never creates a world mutation or synthetic authority.
        let mut world_duplicate = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
                command_id: Some(id),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(53.0),
            },
        );
        let replay = world_duplicate
            .wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        assert_eq!(replay.phase(), RequestPhase::Terminal);
        assert!(replay.authority().is_none());
        assert_eq!(replay.terminal(), Some(&audio_result));
        world_duplicate.acknowledge().unwrap();

        let mut read = submit_after_busy(
            &client,
            SessionOperation::GetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
            },
        );
        let state = wait_terminal(&read);
        let SessionRequestResult::Transform(transform) =
            state.terminal().unwrap().as_ref().unwrap()
        else {
            panic!("world read returned a different result")
        };
        assert_eq!(transform.entity.revision, Revision(0));
        read.acknowledge().unwrap();
        let _ = service.shutdown();
    }

    #[cfg(feature = "audio")]
    #[test]
    fn imported_play_replay_after_ack_and_client_detach_does_not_start_a_second_voice() {
        let service =
            start_audio_project(crate::project_audio::test_wav_project(), default_config());
        let first_client = service.attach().unwrap();
        let id = CommandId::from_raw(55);
        let mut first_play = submit_after_busy(
            &first_client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(id),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("clip_a"),
                    volume: 1.0,
                    pan: 0.0,
                    looping: true,
                },
            }),
        );
        let first_state = wait_terminal(&first_play);
        let first_result = first_state.terminal().unwrap().clone();
        let Ok(SessionRequestResult::Audio(audio_result)) = &first_result else {
            panic!("Play returned a different request result")
        };
        let Ok(crate::SessionAudioCommandOutcome::Played { voice }) =
            audio_result.as_command().unwrap().outcome()
        else {
            panic!("imported Play did not start a voice")
        };
        let voice = *voice;
        first_play.acknowledge().unwrap();
        drop(first_state);
        drop(first_play);
        drop(first_client);

        let second_client = service.attach().unwrap();
        let mut duplicate = submit_after_busy(
            &second_client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(id),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("clip_b"),
                    volume: 0.2,
                    pan: 1.0,
                    looping: false,
                },
            }),
        );
        let duplicate_state = wait_terminal(&duplicate);
        assert_eq!(duplicate_state.terminal(), Some(&first_result));
        duplicate.acknowledge().unwrap();

        let mut voices = submit_after_busy(
            &second_client,
            SessionOperation::Audio(crate::SessionAudioOperation::Voices {
                offset: 0,
                limit: None,
            }),
        );
        let voices_state = wait_terminal(&voices);
        let Ok(SessionRequestResult::Audio(audio_result)) =
            voices_state.terminal().unwrap().as_ref()
        else {
            panic!("voice query returned a different request result")
        };
        let page = audio_result.as_voices().unwrap();
        assert_eq!(page.total(), 1);
        assert_eq!(page.voices()[0].voice(), voice);
        assert_eq!(page.voices()[0].clip_id().unwrap().as_str(), "clip_a");
        assert_eq!(page.voices()[0].looping(), Some(true));
        voices.acknowledge().unwrap();
        drop(voices_state);
        let _ = service.shutdown();
    }

    #[cfg(feature = "audio")]
    #[test]
    fn actor_panic_after_audio_effect_reports_unknown_without_claiming_no_effect() {
        let service = start_audio(default_config());
        let client = service.attach().unwrap();
        service
            .shared
            .panic_after_audio_command_effect
            .store(true, Ordering::Release);
        let mut ticket = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(54)),
                mutation: crate::SessionAudioMutation::RenderOffline { frames: 24 },
            }),
        );
        let state = wait_terminal(&ticket);
        assert_eq!(
            state.terminal().unwrap(),
            &Err(SessionRequestError::OutcomeUnknown {
                command_id: CommandId::from_raw(54),
            })
        );
        assert_eq!(
            service.shared.audio_command_effects.load(Ordering::Acquire),
            1
        );
        ticket.acknowledge().unwrap();
        assert!(matches!(
            service.shutdown().termination,
            SessionTermination::Panicked(_)
        ));
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

    #[test]
    fn default_service_keeps_preparation_disabled_without_worker() {
        let service = start(default_config());
        assert!(!service.asset_preparation_enabled());
        let client = service.attach().unwrap();
        assert!(matches!(
            client.try_submit(SessionOperation::AssetPreparation(
                AssetPreparationOperation::Status
            )),
            Err(SessionSubmitError::AssetPreparationUnavailable)
        ));
        let _ = service.shutdown();
    }

    #[test]
    fn preparation_uses_same_actor_replay_and_keeps_pre_effect_ids_available() {
        let fixture = PreparationTestProject::with_audio();
        let mut owner = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(fixture.path()),
        )
        .expect("start native preparation owner");
        let project = Arc::clone(owner.initial_project());
        let port = owner.take_control_port().expect("take one control port");
        let mut config = default_config();
        config.engine.session = SessionId::from_raw(7_701);
        let service = SessionControlService::start_shared_with_preparation(project, config, port)
            .expect("start one actor with preparation");
        assert!(service.asset_preparation_enabled());

        let client = service.attach().unwrap();
        let initial = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
        )
        .expect("initial preparation status should succeed");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(initial)) =
            initial
        else {
            panic!("status should use preparation metadata");
        };
        assert_eq!(initial.session(), SessionId::from_raw(7_701));
        assert_eq!(initial.active_generation(), 1);
        assert_eq!(initial.state(), PreparationState::Idle);

        let first_command_id = CommandId::from_raw(700);
        let first_terminal = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(first_command_id),
            }),
        )
        .expect("first preparation should be accepted");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(first_result)) =
            &first_terminal
        else {
            panic!("prepare should return its accepted epoch");
        };
        let AssetPreparationCommandOutcome::PrepareAccepted { candidate } = *first_result.outcome()
        else {
            panic!("prepare should return PrepareAccepted");
        };
        assert_eq!(
            candidate,
            PreparationId::from_parts(SessionId::from_raw(7_701), 1)
        );
        drop(client);

        // A new attachment and a different operation with the same retained command ID must
        // observe the original preparation receipt before target validation or a second effect.
        let retry_client = service.attach().unwrap();
        let mut replay = submit_after_busy(
            &retry_client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("missing:never-read")),
                command_id: Some(first_command_id),
                expected_revision: Revision(0),
                apply_at: None,
                transform: moved(9.0),
            },
        );
        assert_eq!(
            wait_terminal(&replay).terminal().unwrap(),
            &Ok(first_terminal)
        );
        replay.acknowledge().unwrap();

        wait_for_preparation_state(&retry_client, PreparationState::CpuPrepared, 1);

        // A future candidate is rejected after ticket admission but before consuming command ID
        // 701. Reusing that ID for a valid Prepare therefore starts exactly epoch 2.
        let second_command_id = CommandId::from_raw(701);
        let mut invalid_cancel = submit_after_busy(
            &retry_client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(second_command_id),
                candidate: PreparationId::from_parts(SessionId::from_raw(7_701), 2),
            }),
        );
        assert_eq!(
            wait_terminal(&invalid_cancel).terminal().unwrap(),
            &Err(SessionRequestError::InvalidCandidate)
        );
        invalid_cancel.acknowledge().unwrap();

        let second_terminal = terminal_result_after_preparation_busy(
            &retry_client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(second_command_id),
            }),
        )
        .expect("second preparation should be accepted");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(second_result)) =
            second_terminal
        else {
            panic!("second prepare should return its accepted epoch");
        };
        let AssetPreparationCommandOutcome::PrepareAccepted {
            candidate: second_candidate,
        } = *second_result.outcome()
        else {
            panic!("second prepare should return PrepareAccepted");
        };
        assert_eq!(second_candidate.epoch(), 2);
        wait_for_preparation_state(&retry_client, PreparationState::CpuPrepared, 2);

        // An older issued epoch is expired; the same command ID can then cancel current epoch 2.
        let cancel_command_id = CommandId::from_raw(702);
        let mut expired_cancel = submit_after_busy(
            &retry_client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(cancel_command_id),
                candidate,
            }),
        );
        assert_eq!(
            wait_terminal(&expired_cancel).terminal().unwrap(),
            &Err(SessionRequestError::CandidateExpired)
        );
        expired_cancel.acknowledge().unwrap();

        let cancel_terminal = terminal_result_after_preparation_busy(
            &retry_client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(cancel_command_id),
                candidate: second_candidate,
            }),
        )
        .expect("current candidate cancellation should be accepted");
        assert!(matches!(
            cancel_terminal,
            SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_))
        ));
        wait_for_preparation_state(&retry_client, PreparationState::Cancelled, 2);

        let shutdown = service.shutdown();
        assert_eq!(shutdown.termination, SessionTermination::StopRequested);
        owner.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match owner.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping if Instant::now() >= deadline => {
                    panic!("preparation worker did not stop within bounded test deadline");
                }
                PreparationWorkerShutdown::Stopping => thread::yield_now(),
            }
        }
    }

    #[test]
    fn actor_panic_after_preparation_acceptance_reports_unknown_not_unapplied() {
        let fixture = PreparationTestProject::with_audio();
        let mut owner = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(fixture.path()),
        )
        .expect("start native preparation owner");
        let project = Arc::clone(owner.initial_project());
        let port = owner.take_control_port().expect("take one control port");
        let service =
            SessionControlService::start_shared_with_preparation(project, default_config(), port)
                .expect("start one actor with preparation");
        service
            .shared
            .panic_after_preparation_command_effect
            .store(true, Ordering::Release);
        let client = service.attach().unwrap();
        let command_id = CommandId::from_raw(703);
        let terminal = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(command_id),
            }),
        );
        assert_eq!(
            terminal,
            Err(SessionRequestError::OutcomeUnknown { command_id })
        );
        drop(client);

        let shutdown = service.shutdown();
        assert!(matches!(
            shutdown.termination,
            SessionTermination::Panicked(_)
        ));
        owner.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match owner.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping if Instant::now() >= deadline => {
                    panic!("preparation worker did not stop after actor panic");
                }
                PreparationWorkerShutdown::Stopping => thread::yield_now(),
            }
        }
    }

    #[test]
    fn headless_activation_commits_same_engine_and_republishes_authority_under_new_source() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let gate = service.block_preparation_for_test(1);
        let client = service.session().attach().unwrap();
        let initial_frame = wait_for_frame(&client);
        let session = initial_frame.session();
        let initial_digest = initial_frame.source_digest().to_owned();
        assert_eq!(initial_frame.project_generation(), 1);
        assert_eq!(initial_frame.tick(), Tick(0));

        let initial_transform = terminal_result_after_activation_busy(
            &client,
            SessionOperation::GetTransform {
                target: SessionEntityTarget::StableId(stable_id("entity:hero")),
            },
        )
        .expect("initial authority entity should exist");
        let SessionRequestResult::Transform(initial_transform) = initial_transform else {
            panic!("transform query returned the wrong result")
        };
        let entity_handle = initial_transform.entity.handle;

        let mut set = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::Handle(entity_handle),
                command_id: Some(CommandId::from_raw(88_010)),
                expected_revision: Revision(0),
                apply_at: Some(Tick(1)),
                transform: moved(9.0),
            },
        );
        set.wait_for(WaitStage::AuthorityAccepted, Duration::from_secs(2))
            .unwrap();
        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        wait_terminal(&step);
        assert!(matches!(
            wait_terminal(&set).terminal().unwrap(),
            Ok(SessionRequestResult::Command(_))
        ));
        set.acknowledge().unwrap();
        step.acknowledge().unwrap();
        let frame_before_commit = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let frame = wait_for_frame(&client);
                if frame.tick() == Tick(1) {
                    break frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "tick-one frame was not published"
                );
                thread::yield_now();
            }
        };
        assert_eq!(frame_before_commit.project_generation(), 1);

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let prepare = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(CommandId::from_raw(88_011)),
            }),
        )
        .expect("candidate preparation should be accepted");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(prepare)) =
            prepare
        else {
            panic!("prepare returned the wrong result")
        };
        let AssetPreparationCommandOutcome::PrepareAccepted { candidate } = *prepare.outcome()
        else {
            panic!("prepare did not return its candidate")
        };
        gate.wait_until_entered();

        // The same command ID remains available after the pre-effect Busy result.
        let early_stage_id = CommandId::from_raw(88_012);
        let mut early_stage = submit_after_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                command_id: Some(early_stage_id),
                candidate,
                expected_active_generation: 1,
            }),
        );
        assert_eq!(
            wait_terminal(&early_stage).terminal().unwrap(),
            &Err(SessionRequestError::PreparationBusy)
        );
        early_stage.acknowledge().unwrap();

        gate.release();
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let stage = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                command_id: Some(early_stage_id),
                candidate,
                expected_active_generation: 1,
            }),
        )
        .expect("ready candidate should stage");
        let SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(stage)) =
            stage
        else {
            panic!("stage returned the wrong result")
        };
        let AssetActivationCommandOutcome::StageAccepted { activation, .. } = *stage.outcome()
        else {
            panic!("stage did not return an activation")
        };
        let staged = wait_for_activation_state(&client, AssetActivationState::Staged);
        assert_eq!(staged.session(), session);
        assert_eq!(staged.active_generation(), 1);

        let commit_id = CommandId::from_raw(88_013);
        let commit_operation =
            SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(commit_id),
                activation,
                expected_active_generation: 1,
            });
        let locked_worker_state = service.hold_worker_state_for_test();
        let mut busy_commit = submit_after_busy(&client, commit_operation.clone());
        let busy_commit_result = wait_terminal(&busy_commit)
            .terminal()
            .expect("busy commit must reach a terminal result")
            .clone();
        assert_eq!(
            busy_commit_result,
            Err(SessionRequestError::ActivationBusy),
            "transient commit contention must not consume the caller command ID"
        );
        busy_commit.acknowledge().unwrap();
        drop(locked_worker_state);

        let mut expired_command = submit_after_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::Handle(entity_handle),
                command_id: Some(CommandId::from_raw(88_009)),
                expected_revision: Revision(1),
                apply_at: Some(Tick(2)),
                transform: moved(10.0),
            },
        );
        let expired_result = wait_terminal(&expired_command)
            .terminal()
            .expect("old command ID must reach a terminal result")
            .clone();
        assert!(
            matches!(
                expired_result,
                Err(SessionRequestError::ReplayWindowExpired {
                    command_id: 88_009,
                    high_water: 88_012,
                })
            ),
            "transient commit contention must preserve the earlier caller replay high-water"
        );
        expired_command.acknowledge().unwrap();

        let commit = terminal_result_after_activation_busy(&client, commit_operation)
            .expect("staged candidate should commit");
        let SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(commit)) =
            &commit
        else {
            panic!("commit returned the wrong result")
        };
        assert!(matches!(
            commit.outcome(),
            AssetActivationCommandOutcome::CommitApplied { generation: 2, .. }
        ));

        let committed_frame = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let frame = wait_for_frame(&client);
                if frame.project_generation() == 2 {
                    break frame;
                }
                assert!(
                    Instant::now() < deadline,
                    "generation-two frame was not published"
                );
                thread::yield_now();
            }
        };
        assert_eq!(committed_frame.session(), session);
        assert_eq!(committed_frame.tick(), frame_before_commit.tick());
        assert_eq!(
            committed_frame.authority_snapshot_sequence(),
            frame_before_commit.authority_snapshot_sequence(),
            "commit republishes the same authority snapshot without stepping"
        );
        assert!(committed_frame.sequence() > frame_before_commit.sequence());
        assert_ne!(committed_frame.source_digest(), initial_digest);
        assert_eq!(committed_frame.source_scene_revision(), 38);

        let after_commit = terminal_result_after_activation_busy(
            &client,
            SessionOperation::GetTransform {
                target: SessionEntityTarget::Handle(entity_handle),
            },
        )
        .expect("same entity handle should remain bound after source replacement");
        let SessionRequestResult::Transform(after_commit) = after_commit else {
            panic!("transform query returned the wrong result")
        };
        assert_eq!(after_commit.entity.handle, entity_handle);
        assert_eq!(after_commit.entity.transform, moved(9.0));
        assert_eq!(after_commit.entity.revision, Revision(1));

        // Cross-kind same-ID retry replays the committed activation before target validation.
        let replay = terminal_result_after_activation_busy(
            &client,
            SessionOperation::SetTransform {
                target: SessionEntityTarget::StableId(stable_id("missing:never-read")),
                command_id: Some(commit_id),
                expected_revision: Revision(0),
                apply_at: Some(Tick(2)),
                transform: moved(99.0),
            },
        )
        .expect("cross-kind retry should replay the original activation result");
        assert_eq!(
            replay,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(
                commit.clone()
            ))
        );

        drop(client);
        stop_activation_service(&mut service);
        assert_eq!(
            service.shutdown_report().unwrap().termination,
            SessionTermination::StopRequested
        );
    }

    #[test]
    fn committed_generation_republishes_after_held_frame_reader_releases_without_step() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut config = default_config();
        config.limits.max_retained_frames = 1;
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            config,
        )
        .expect("start headless asset activation service");
        let client = service.session().attach().unwrap();

        let initial_frame = wait_for_frame(&client);
        let session = initial_frame.session();
        let initial_digest = initial_frame.source_digest().to_owned();
        assert_eq!(initial_frame.project_generation(), 1);
        assert_eq!(initial_frame.tick(), Tick(0));
        drop(initial_frame);

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let candidate = prepare_activation_candidate(&client, 89_061);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let activation = stage_activation_candidate(&client, candidate, 89_062, 1);
        wait_for_activation_state(&client, AssetActivationState::Staged);

        let precommit_frame = wait_for_frame(&client);
        assert_eq!(precommit_frame.project_generation(), 1);
        assert_eq!(precommit_frame.tick(), Tick(0));
        let precommit_sequence = precommit_frame.sequence();
        let precommit_snapshot_sequence = precommit_frame.authority_snapshot_sequence();
        let precommit_tick = precommit_frame.tick();

        let commit_result = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(CommandId::from_raw(89_063)),
                activation,
                expected_active_generation: 1,
            }),
        )
        .expect("staged candidate should commit while the old frame is retained");
        assert!(matches!(
            commit_result,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(result))
                if matches!(result.outcome(), AssetActivationCommandOutcome::CommitApplied { generation: 2, .. })
        ));
        let committed = wait_for_activation_state(&client, AssetActivationState::Committed);
        assert_eq!(committed.active_generation(), 2);

        let still_last_good = wait_for_frame(&client);
        assert_eq!(still_last_good.project_generation(), 1);
        assert_eq!(still_last_good.sequence(), precommit_sequence);
        drop(still_last_good);

        // The held old frame fills the one-frame consumer budget. Releasing it lets the actor
        // publish the already-committed generation against the same authority snapshot.
        drop(precommit_frame);
        let committed_frame = {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let frame = wait_for_frame(&client);
                if frame.project_generation() == 2 {
                    break frame;
                }
                drop(frame);
                assert!(
                    Instant::now() < deadline,
                    "committed generation was not republished after frame release"
                );
                thread::yield_now();
            }
        };
        assert_eq!(committed_frame.session(), session);
        assert_eq!(committed_frame.tick(), precommit_tick);
        assert_eq!(
            committed_frame.authority_snapshot_sequence(),
            precommit_snapshot_sequence
        );
        assert!(committed_frame.sequence() > precommit_sequence);
        assert_ne!(committed_frame.source_digest(), initial_digest);

        drop(committed_frame);
        drop(client);
        stop_activation_service(&mut service);
    }

    #[test]
    fn activation_shutdown_stays_stopping_until_retired_source_reader_releases() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let promotion_gate = service.block_promotion_for_test(1);
        let mut source_reader = service.hold_next_retired_project_for_test();
        let client = service.session().attach().unwrap();

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let candidate = prepare_activation_candidate(&client, 89_071);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let activation = stage_activation_candidate(&client, candidate, 89_072, 1);
        wait_for_activation_state(&client, AssetActivationState::Staged);
        let commit = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(CommandId::from_raw(89_073)),
                activation,
                expected_active_generation: 1,
            }),
        )
        .expect("staged candidate should commit");
        assert!(matches!(
            commit,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(result))
                if matches!(result.outcome(), AssetActivationCommandOutcome::CommitApplied { generation: 2, .. })
        ));
        promotion_gate.wait_until_entered();
        promotion_gate.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = wait_for_activation_state(&client, AssetActivationState::Committed);
            if status.memory().retired_generation_count() == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not retain the old project source reader"
            );
            thread::yield_now();
        }

        drop(client);
        service.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Ok(NativeProjectActivationShutdown::Stopping)
                    if service.shutdown_report().is_some() =>
                {
                    break;
                }
                Ok(NativeProjectActivationShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "actor did not stop while retired source remained leased"
                    );
                    thread::yield_now();
                }
                Ok(NativeProjectActivationShutdown::Stopped) => {
                    panic!("worker reported Stopped while a retired source reader was held");
                }
                Err(error) => panic!("activation owner shutdown failed: {error}"),
            }
        }
        assert_eq!(
            service.shutdown_report().unwrap().termination,
            SessionTermination::StopRequested
        );
        assert_eq!(
            service.try_shutdown(),
            Ok(NativeProjectActivationShutdown::Stopping),
            "a held source Arc must keep worker shutdown in Stopping"
        );

        source_reader.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Ok(NativeProjectActivationShutdown::Stopped) => break,
                Ok(NativeProjectActivationShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "worker did not finish after retired source release"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("activation owner shutdown failed after release: {error}"),
            }
        }
        assert_eq!(
            service.shutdown_report().unwrap().termination,
            SessionTermination::StopRequested
        );
    }

    #[test]
    fn commit_promotion_retains_retired_charge_and_prepare_retries_after_reclamation() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut measurer = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(fixture.path()),
        )
        .expect("measure the initial project catalog");
        let active_bytes = measurer
            .initial_project()
            .asset_import_report()
            .expect("asset report")
            .memory()
            .cooked_accounted_bytes();
        stop_preparation_owner(&mut measurer);
        drop(measurer);

        let maximum = active_bytes.checked_mul(2).expect("small test budget");
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path())
                .with_max_catalog_accounted_bytes(maximum),
            default_config(),
        )
        .expect("start activation with exactly one transition reserve");
        let promotion_gate = service.block_promotion_for_test(1);
        let mut retired_project_hold = service.hold_next_retired_project_for_test();
        let client = service.session().attach().unwrap();

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let candidate = prepare_activation_candidate(&client, 89_031);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let preparation = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
        )
        .expect("candidate memory should be inspectable");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(preparation)) =
            preparation
        else {
            panic!("preparation status returned the wrong result");
        };
        assert_eq!(
            preparation
                .ready_candidate()
                .unwrap()
                .memory()
                .cooked_accounted_bytes(),
            active_bytes
        );

        let activation = stage_activation_candidate(&client, candidate, 89_032, 1);
        wait_for_activation_state(&client, AssetActivationState::Staged);
        let commit = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(CommandId::from_raw(89_033)),
                activation,
                expected_active_generation: 1,
            }),
        )
        .expect("candidate should commit");
        assert!(matches!(
            commit,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(result))
                if matches!(result.outcome(), AssetActivationCommandOutcome::CommitApplied { generation: 2, .. })
        ));
        promotion_gate.wait_until_entered();

        let committing = wait_for_activation_state(&client, AssetActivationState::Committed);
        let memory = committing.memory();
        assert_eq!(memory.active_cooked_accounted_bytes(), active_bytes);
        assert_eq!(memory.retired_cooked_accounted_bytes(), active_bytes);
        assert_eq!(memory.retired_generation_count(), 1);
        assert_eq!(memory.total_cooked_accounted_bytes(), maximum);

        let next_prepare = SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
            command_id: Some(CommandId::from_raw(89_034)),
        });
        let mut pending_promotion_retry = submit_after_busy(&client, next_prepare.clone());
        assert!(
            matches!(
                wait_terminal(&pending_promotion_retry).terminal().unwrap(),
                Err(SessionRequestError::PreparationBusy)
            ),
            "a new preparation cannot overtake the worker promotion"
        );
        pending_promotion_retry.acknowledge().unwrap();

        promotion_gate.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = wait_for_activation_state(&client, AssetActivationState::Committed);
            if status.memory().retired_generation_count() == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "retired project hold was not published"
            );
            thread::yield_now();
        }

        let mut capacity_retry = submit_after_busy(&client, next_prepare.clone());
        assert!(
            matches!(
                wait_terminal(&capacity_retry).terminal().unwrap(),
                Err(SessionRequestError::PreparationCapacity)
            ),
            "the held retired project remains charged against the catalog budget"
        );
        capacity_retry.acknowledge().unwrap();

        retired_project_hold.release();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = wait_for_activation_state(&client, AssetActivationState::Committed);
            if status.memory().retired_generation_count() == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not reclaim released retired Arc"
            );
            thread::yield_now();
        }
        let accepted = terminal_result_after_preparation_busy(&client, next_prepare)
            .expect("the same unconsumed Prepare command should succeed after reclamation");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(accepted)) =
            accepted
        else {
            panic!("prepare retry returned the wrong result");
        };
        let AssetPreparationCommandOutcome::PrepareAccepted {
            candidate: next_candidate,
        } = *accepted.outcome()
        else {
            panic!("prepare retry did not accept the next epoch");
        };
        assert_eq!(next_candidate.epoch(), 2);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, 2);

        let cancel = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(CommandId::from_raw(89_035)),
                candidate: next_candidate,
            }),
        )
        .expect("cleanup candidate should be cancellable");
        assert!(matches!(
            cancel,
            SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_))
        ));
        wait_for_preparation_state(&client, PreparationState::Cancelled, 2);

        drop(client);
        stop_activation_service(&mut service);
    }

    #[test]
    fn delayed_ready_stage_result_after_activation_cancel_cannot_restage_candidate() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let gate = service.block_stage_result_for_test(1);
        let client = service.session().attach().unwrap();

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let candidate = prepare_activation_candidate(&client, 89_001);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let activation = stage_activation_candidate(&client, candidate, 89_002, 1);
        gate.wait_until_entered();

        let cancel = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Cancel {
                command_id: Some(CommandId::from_raw(89_003)),
                activation,
            }),
        )
        .expect("staging activation should cancel while its result is held");
        assert!(matches!(
            cancel,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(result))
                if matches!(result.outcome(), AssetActivationCommandOutcome::CancelAccepted { activation: cancelled } if *cancelled == activation)
        ));

        gate.release();
        let cancelled = wait_for_activation_state(&client, AssetActivationState::Cancelled);
        assert!(cancelled.latest_failure().is_none());
        assert_eq!(
            cancelled.current_activation().map(|summary| summary.id()),
            Some(activation)
        );
        let preparation = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
        )
        .expect("preparation status should remain available");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(preparation)) =
            preparation
        else {
            panic!("preparation status returned the wrong result");
        };
        assert_eq!(preparation.state(), PreparationState::Cancelled);
        assert!(preparation.ready_candidate().is_none());

        drop(client);
        stop_activation_service(&mut service);
    }

    #[test]
    fn delayed_incompatible_stage_result_is_ignored_after_new_prepare_supersedes_it() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let gate = service.block_stage_result_for_test(1);
        let client = service.session().attach().unwrap();

        std::fs::write(fixture.path().join("scene.json"), scene_with_entities(2)).unwrap();
        let first_candidate = prepare_activation_candidate(&client, 89_011);
        wait_for_preparation_state(
            &client,
            PreparationState::CpuPrepared,
            first_candidate.epoch(),
        );
        let _old_activation = stage_activation_candidate(&client, first_candidate, 89_012, 1);
        gate.wait_until_entered();

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 39");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let second_candidate = prepare_activation_candidate(&client, 89_013);
        assert_eq!(second_candidate.epoch(), 2);
        gate.release();
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, 2);

        let superseded = wait_for_activation_state(&client, AssetActivationState::Superseded);
        assert!(superseded.latest_failure().is_none());
        let preparation = terminal_result_after_activation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
        )
        .expect("preparation status should expose the new desired candidate");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(preparation)) =
            preparation
        else {
            panic!("preparation status returned the wrong result");
        };
        assert_eq!(
            preparation
                .ready_candidate()
                .map(|candidate| candidate.id()),
            Some(second_candidate)
        );
        assert!(
            preparation
                .ready_candidate()
                .is_some_and(|candidate| candidate.is_current())
        );

        let new_activation = stage_activation_candidate(&client, second_candidate, 89_014, 1);
        let staged = wait_for_activation_state(&client, AssetActivationState::Staged);
        assert_eq!(
            staged.current_activation().map(|summary| summary.id()),
            Some(new_activation)
        );
        assert!(staged.latest_failure().is_none());

        drop(client);
        stop_activation_service(&mut service);
    }

    #[test]
    fn actor_panic_after_activation_commit_effect_reports_unknown_and_cleans_worker() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let client = service.session().attach().unwrap();

        let replacement_scene = ONE_ENTITY_SCENE.replace("\"revision\": 37", "\"revision\": 38");
        std::fs::write(fixture.path().join("scene.json"), replacement_scene).unwrap();
        let candidate = prepare_activation_candidate(&client, 89_021);
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, candidate.epoch());
        let activation = stage_activation_candidate(&client, candidate, 89_022, 1);
        wait_for_activation_state(&client, AssetActivationState::Staged);
        service
            .session()
            .shared
            .panic_after_activation_command_effect
            .store(true, Ordering::Release);

        let command_id = CommandId::from_raw(89_023);
        let mut commit = submit_after_busy(
            &client,
            SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                command_id: Some(command_id),
                activation,
                expected_active_generation: 1,
            }),
        );
        let terminal = wait_terminal(&commit)
            .terminal()
            .expect("post-effect actor panic must resolve the active ticket")
            .clone();
        assert_eq!(
            terminal,
            Err(SessionRequestError::OutcomeUnknown { command_id })
        );
        commit.acknowledge().unwrap();
        drop(client);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Err(crate::NativeProjectActivationShutdownError::ActorFailed) => break,
                Ok(NativeProjectActivationShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "activation owner failed to clean up"
                    );
                    thread::yield_now();
                }
                other => panic!("unexpected shutdown result after actor panic: {other:?}"),
            }
        }
        assert!(matches!(
            service.shutdown_report().map(|report| &report.termination),
            Some(SessionTermination::Panicked(_))
        ));
        assert_eq!(
            service.try_shutdown(),
            Err(crate::NativeProjectActivationShutdownError::ActorFailed),
            "repeated shutdown must preserve the actor failure"
        );
    }

    #[test]
    fn activation_owner_preserves_worker_panic_after_join() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let mut service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let gate = service.panic_preparation_for_test(1);
        let client = service.session().attach().unwrap();

        let accepted = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(CommandId::from_raw(89_041)),
            }),
        )
        .expect("candidate preparation should be accepted before worker panic");
        assert!(matches!(
            accepted,
            SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_))
        ));
        gate.wait_until_entered();
        gate.release();
        drop(client);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Err(crate::NativeProjectActivationShutdownError::WorkerPanicked) => break,
                Ok(NativeProjectActivationShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "panicked activation owner did not stop"
                    );
                    thread::yield_now();
                }
                other => panic!("unexpected shutdown result after worker panic: {other:?}"),
            }
        }
        let termination = service.shutdown_report().map(|report| &report.termination);
        assert!(
            termination == Some(&SessionTermination::StopRequested)
                || matches!(
                    termination,
                    Some(SessionTermination::EngineError(message))
                        if message == "project preparation worker panicked"
                ),
            "worker panic may be observed by the actor before or after owner stop: {termination:?}"
        );
        assert_eq!(
            service.try_shutdown(),
            Err(crate::NativeProjectActivationShutdownError::WorkerPanicked),
            "a later poll cannot report Stopped after a worker panic"
        );
    }

    #[test]
    fn dropping_activation_owner_does_not_wait_for_worker_and_cache_releases_after_cleanup() {
        let fixture = PreparationTestProject::with_audio();
        std::fs::write(fixture.path().join("scene.json"), ONE_ENTITY_SCENE).unwrap();
        let service = NativeProjectActivationService::start_headless(
            NativeProjectPreparationOptions::new(fixture.path()),
            default_config(),
        )
        .expect("start headless asset activation service");
        let gate = service.block_preparation_for_test(1);
        let client = service.session().attach().unwrap();
        let accepted = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(CommandId::from_raw(89_051)),
            }),
        )
        .expect("candidate preparation should be accepted");
        assert!(matches!(
            accepted,
            SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_))
        ));
        gate.wait_until_entered();
        drop(client);

        let (dropped_sender, dropped_receiver) = std::sync::mpsc::sync_channel(1);
        let drop_thread = thread::spawn(move || {
            drop(service);
            let _ = dropped_sender.send(());
        });
        if dropped_receiver
            .recv_timeout(Duration::from_secs(2))
            .is_err()
        {
            gate.release();
            let _ = drop_thread.join();
            panic!("dropping the activation owner waited for blocked import work");
        }
        drop_thread
            .join()
            .expect("owner drop thread should return normally");

        let cache_root = fixture.path().join(".daedalus/cache");
        assert!(
            daedalus_content::FileCookCache::open(
                &cache_root,
                daedalus_content::CacheLimits::default()
            )
            .is_err(),
            "a detached worker retains the cache owner until it has cleaned up"
        );
        gate.release();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match daedalus_content::FileCookCache::open(
                &cache_root,
                daedalus_content::CacheLimits::default(),
            ) {
                Ok(cache) => {
                    drop(cache);
                    break;
                }
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "worker did not release cache ownership"
                    );
                    thread::yield_now();
                }
            }
        }
    }

    #[cfg(feature = "audio")]
    #[test]
    fn held_preparation_worker_does_not_block_reserved_step_or_same_actor_audio() {
        let fixture = PreparationTestProject::with_audio();
        let mut owner = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(fixture.path()),
        )
        .expect("start native preparation owner");
        let gate = owner.block_preparation_for_test(1);
        let project = Arc::clone(owner.initial_project());
        let port = owner.take_control_port().expect("take one control port");
        let service = SessionControlService::start_shared_with_preparation_and_audio(
            project,
            default_config(),
            port,
            crate::SessionAudioOptions::offline(8_000),
        )
        .expect("start one actor with preparation and offline audio");
        let client = service.attach().unwrap();

        let mut play = submit_after_busy(
            &client,
            SessionOperation::Audio(crate::SessionAudioOperation::Command {
                command_id: Some(CommandId::from_raw(9_999)),
                mutation: crate::SessionAudioMutation::Play {
                    clip: stable_id("tone"),
                    volume: 1.0,
                    pan: 0.0,
                    looping: true,
                },
            }),
        );
        let play_state = wait_terminal(&play);
        assert!(matches!(
            play_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(crate::SessionAudioResult::Command(result)))
                if matches!(result.outcome(),
                    Ok(crate::SessionAudioCommandOutcome::Played { .. }))
        ));
        play.acknowledge().unwrap();
        drop(play_state);

        let prepare_id = CommandId::from_raw(10_000);
        let accepted = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(prepare_id),
            }),
        )
        .expect("preparation command should be accepted");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(result)) =
            accepted
        else {
            panic!("preparation should return its command receipt");
        };
        let AssetPreparationCommandOutcome::PrepareAccepted { candidate } = *result.outcome()
        else {
            panic!("preparation should accept the candidate");
        };
        gate.wait_until_entered();

        // Hold every normal ticket credit. The reserved Manual Step lane must still advance the
        // same Engine while the importer is deliberately stopped at its before-import gate.
        let mut normal_tickets = (0..7)
            .map(|_| submit_after_busy(&client, SessionOperation::Status))
            .collect::<Vec<_>>();
        let final_normal = normal_tickets.last().unwrap();
        assert!(wait_terminal(final_normal).terminal().unwrap().is_ok());
        let mut step = submit_after_busy(&client, SessionOperation::Step { count: 1 });
        let step_state = wait_terminal(&step);
        assert!(matches!(
            step_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Step(reports))
                if reports.len() == 1 && reports[0].tick == Tick(1)
        ));

        let audio_operation = SessionOperation::Audio(crate::SessionAudioOperation::Command {
            command_id: Some(CommandId::from_raw(10_001)),
            mutation: crate::SessionAudioMutation::RenderOffline { frames: 16 },
        });
        assert_normal_result_capacity(&client, audio_operation.clone());
        normal_tickets[0].acknowledge().unwrap();
        let mut audio = submit_after_busy(&client, audio_operation);
        let audio_state = wait_terminal(&audio);
        assert!(matches!(
            audio_state.terminal().unwrap().as_ref(),
            Ok(SessionRequestResult::Audio(crate::SessionAudioResult::Command(result)))
                if result.observed_tick() == Tick(1)
                    && matches!(result.outcome(),
                        Ok(crate::SessionAudioCommandOutcome::Offline(summary))
                            if summary.frames() == 16
                                && summary.cumulative_frames() == 16
                                && summary.nonzero_samples() > 0
                                && summary.peak() > 0.0)
        ));
        audio.acknowledge().unwrap();
        step.acknowledge().unwrap();
        for ticket in normal_tickets.iter_mut().skip(1) {
            ticket.acknowledge().unwrap();
        }
        drop(step_state);
        drop(audio_state);

        gate.release();
        wait_for_preparation_state(&client, PreparationState::CpuPrepared, 1);
        let cancel = terminal_result_after_preparation_busy(
            &client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                command_id: Some(CommandId::from_raw(10_002)),
                candidate,
            }),
        )
        .expect("ready candidate should accept cancellation");
        assert!(matches!(
            cancel,
            SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(_))
        ));
        wait_for_preparation_state(&client, PreparationState::Cancelled, 1);
        drop(client);

        let shutdown = service.shutdown();
        assert_eq!(shutdown.termination, SessionTermination::StopRequested);
        owner.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match owner.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping if Instant::now() >= deadline => {
                    panic!("preparation worker did not stop within bounded test deadline");
                }
                PreparationWorkerShutdown::Stopping => thread::yield_now(),
            }
        }
    }

    fn wait_for_preparation_state(
        client: &super::SessionControlClient,
        expected: PreparationState,
        epoch: u64,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = terminal_result_after_preparation_busy(
                client,
                SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
            );
            let status = match result {
                Ok(SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(
                    status,
                ))) => status,
                Ok(other) => panic!("status returned an unexpected result: {other:?}"),
                Err(error) => panic!("status failed while waiting for {expected:?}: {error}"),
            };
            if status.state() == expected
                && status
                    .desired()
                    .is_none_or(|desired| desired.epoch() == epoch)
                && status
                    .running()
                    .is_none_or(|running| running.epoch() == epoch)
                && status
                    .pending()
                    .is_none_or(|pending| pending.epoch() == epoch)
                && (expected != PreparationState::CpuPrepared
                    || status
                        .ready_candidate()
                        .is_some_and(|candidate| candidate.id().epoch() == epoch))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "preparation worker did not reach {expected:?} for epoch {epoch}"
            );
            thread::yield_now();
        }
    }

    fn terminal_result_after_preparation_busy(
        client: &super::SessionControlClient,
        operation: SessionOperation,
    ) -> Result<SessionRequestResult, SessionRequestError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut ticket = submit_after_busy(client, operation.clone());
            let progress = wait_terminal(&ticket);
            let terminal = progress
                .terminal()
                .expect("terminal wait returns a terminal result")
                .clone();
            ticket.acknowledge().unwrap();
            drop(progress);
            match terminal {
                Err(SessionRequestError::PreparationBusy) if Instant::now() < deadline => {
                    thread::yield_now();
                }
                other => return other,
            }
            assert!(
                Instant::now() < deadline,
                "preparation remained busy past bounded retry deadline"
            );
        }
    }

    fn terminal_result_after_activation_busy(
        client: &super::SessionControlClient,
        operation: SessionOperation,
    ) -> Result<SessionRequestResult, SessionRequestError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut ticket = submit_after_busy(client, operation.clone());
            let progress = wait_terminal(&ticket);
            let terminal = progress
                .terminal()
                .expect("terminal wait returns a terminal result")
                .clone();
            ticket.acknowledge().unwrap();
            drop(progress);
            match terminal {
                Err(SessionRequestError::ActivationBusy | SessionRequestError::PreparationBusy)
                    if Instant::now() < deadline =>
                {
                    thread::yield_now();
                }
                other => return other,
            }
            assert!(
                Instant::now() < deadline,
                "activation worker remained busy past bounded retry deadline"
            );
        }
    }

    fn wait_for_activation_state(
        client: &super::SessionControlClient,
        expected: AssetActivationState,
    ) -> crate::AssetActivationStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = terminal_result_after_activation_busy(
                client,
                SessionOperation::AssetActivation(AssetActivationOperation::Status),
            );
            let status = match result {
                Ok(SessionRequestResult::AssetActivation(
                    AssetActivationRequestResult::Status(status),
                )) => status,
                Ok(other) => panic!("activation status returned an unexpected result: {other:?}"),
                Err(error) => panic!("activation status failed while waiting: {error}"),
            };
            if status.state() == expected {
                return *status;
            }
            assert!(
                Instant::now() < deadline,
                "activation did not reach {expected:?} (actual {:?})",
                status.state()
            );
            thread::yield_now();
        }
    }

    fn prepare_activation_candidate(
        client: &super::SessionControlClient,
        raw_command_id: u128,
    ) -> PreparationId {
        let result = terminal_result_after_preparation_busy(
            client,
            SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                command_id: Some(CommandId::from_raw(raw_command_id)),
            }),
        )
        .expect("candidate preparation should be accepted");
        let SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(command)) =
            result
        else {
            panic!("prepare returned the wrong result");
        };
        let AssetPreparationCommandOutcome::PrepareAccepted { candidate } = command.outcome()
        else {
            panic!("prepare did not return a candidate");
        };
        *candidate
    }

    fn stage_activation_candidate(
        client: &super::SessionControlClient,
        candidate: PreparationId,
        raw_command_id: u128,
        expected_active_generation: u64,
    ) -> crate::ActivationId {
        let result = terminal_result_after_activation_busy(
            client,
            SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                command_id: Some(CommandId::from_raw(raw_command_id)),
                candidate,
                expected_active_generation,
            }),
        )
        .expect("candidate should be accepted for staging");
        let SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(command)) =
            result
        else {
            panic!("stage returned the wrong result");
        };
        let AssetActivationCommandOutcome::StageAccepted { activation, .. } = command.outcome()
        else {
            panic!("stage did not return an activation");
        };
        *activation
    }

    fn stop_activation_service(service: &mut NativeProjectActivationService) {
        service.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Ok(NativeProjectActivationShutdown::Stopped) => return,
                Ok(NativeProjectActivationShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "activation service shutdown timed out"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("activation service shutdown failed: {error}"),
            }
        }
    }

    fn stop_preparation_owner(owner: &mut NativeProjectPreparationService) {
        owner.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match owner.try_shutdown() {
                Ok(PreparationWorkerShutdown::Stopped) => return,
                Ok(PreparationWorkerShutdown::Stopping) => {
                    assert!(
                        Instant::now() < deadline,
                        "preparation worker shutdown timed out"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("preparation worker shutdown failed: {error}"),
            }
        }
    }

    #[cfg(feature = "audio")]
    fn assert_normal_result_capacity(
        client: &super::SessionControlClient,
        operation: SessionOperation,
    ) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match client.try_submit(operation.clone()) {
                Err(SessionSubmitError::Busy) if Instant::now() < deadline => {
                    thread::yield_now();
                }
                Err(SessionSubmitError::ResultCapacity) => return,
                Ok(mut ticket) => {
                    let _ = ticket.acknowledge();
                    panic!("normal operation was admitted while all normal credits were held");
                }
                Err(error) => panic!("unexpected admission error: {error:?}"),
            }
        }
    }
}
