//! Runtime-neutral handoff between the project authority and a trusted render consumer.
//!
//! This module intentionally knows nothing about renderer/device types. The consumer receives
//! borrowed project access and a standard erased Arc pin; it reports only bounded metadata back
//! to the existing authority actor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use daedalus_content::PreparedProject;
use daedalus_core::{SessionId, Tick};
use thiserror::Error;

use crate::project_activation::ActivationId;
use crate::project_preparation::PreparationId;
use crate::session_control::SessionFrame;

const MAX_SOURCE_DIGEST_BYTES: usize = 71;
const MAX_STAGE_FAILURE_CODE_BYTES: usize = 64;
const MAX_STAGE_FAILURE_MESSAGE_BYTES: usize = 2_048;

/// Maximum logical metadata retained in one consumer status response.
pub(crate) const MAX_PROJECT_CONSUMER_STATUS_BYTES: usize = 32 * 1024;

/// Source kind represented by a project source lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectSourceKind {
    Initial,
    Candidate,
}

/// Opaque source identity. Candidate identities carry their accepted activation/preparation IDs
/// and the exact generation transition they are allowed to report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSourceIdentity {
    kind: ProjectSourceKind,
    session: SessionId,
    generation: u64,
    source_digest: String,
    preparation: Option<PreparationId>,
    activation: Option<ActivationId>,
    expected_generation: Option<u64>,
    proposed_generation: Option<u64>,
}

impl ProjectSourceIdentity {
    pub(crate) fn initial(session: SessionId, source_digest: &str) -> Self {
        Self {
            kind: ProjectSourceKind::Initial,
            session,
            generation: 1,
            source_digest: source_digest.to_owned(),
            preparation: None,
            activation: None,
            expected_generation: None,
            proposed_generation: None,
        }
    }

    pub(crate) fn candidate(
        session: SessionId,
        preparation: PreparationId,
        activation: ActivationId,
        expected_generation: u64,
        proposed_generation: u64,
        source_digest: &str,
    ) -> Result<Self, ProjectConsumerError> {
        if source_digest.is_empty()
            || source_digest.len() > MAX_SOURCE_DIGEST_BYTES
            || expected_generation.checked_add(1) != Some(proposed_generation)
            || preparation.session() != session
            || activation.session() != session
        {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(Self {
            kind: ProjectSourceKind::Candidate,
            session,
            generation: proposed_generation,
            source_digest: source_digest.to_owned(),
            preparation: Some(preparation),
            activation: Some(activation),
            expected_generation: Some(expected_generation),
            proposed_generation: Some(proposed_generation),
        })
    }

    pub fn kind(&self) -> ProjectSourceKind {
        self.kind
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Generation represented by this source. For a candidate this is its proposed generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn preparation(&self) -> Option<PreparationId> {
        self.preparation
    }

    pub fn activation(&self) -> Option<ActivationId> {
        self.activation
    }

    pub fn expected_generation(&self) -> Option<u64> {
        self.expected_generation
    }

    pub fn proposed_generation(&self) -> Option<u64> {
        self.proposed_generation
    }
}

/// Non-cloneable view of one prepared source. `is_current` is a checkpoint observation, not a
/// lock held for the duration of `with_source`.
pub struct ProjectSourceLease {
    identity: ProjectSourceIdentity,
    project: Arc<PreparedProject>,
    currentness: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl std::fmt::Debug for ProjectSourceLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectSourceLease")
            .field("identity", &self.identity)
            .field("current", &self.is_current())
            .finish_non_exhaustive()
    }
}

impl ProjectSourceLease {
    pub(crate) fn new(
        identity: ProjectSourceIdentity,
        project: Arc<PreparedProject>,
        currentness: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    ) -> Self {
        Self {
            identity,
            project,
            currentness,
            closed,
            stopped,
        }
    }

    pub fn identity(&self) -> &ProjectSourceIdentity {
        &self.identity
    }

    pub fn with_source<R>(&self, read: impl for<'a> FnOnce(&'a PreparedProject) -> R) -> R {
        read(&self.project)
    }

    pub fn is_current(&self) -> bool {
        self.currentness.load(Ordering::Acquire)
            && !self.closed.load(Ordering::Acquire)
            && !self.stopped.load(Ordering::Acquire)
    }

    /// Returns a standard opaque Arc pin to the immutable project source. It exposes no runtime
    /// type and does not own renderer resources.
    pub fn source_pin(&self) -> Arc<dyn Send + Sync> {
        Arc::new(ProjectSourcePin {
            _project: Arc::clone(&self.project),
        })
    }
}

struct ProjectSourcePin {
    _project: Arc<PreparedProject>,
}

/// Failure phase reported by a trusted render consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectConsumerStageFailurePhase {
    CpuPrepare,
    DevicePrepare,
}

/// A bounded failure diagnostic. Bounds are checked before the value can be copied into actor
/// state or its status response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConsumerStageFailure {
    phase: ProjectConsumerStageFailurePhase,
    code: String,
    message: String,
}

impl ProjectConsumerStageFailure {
    pub fn new(
        phase: ProjectConsumerStageFailurePhase,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Self, ProjectConsumerError> {
        let code = code.into();
        let message = message.into();
        if code.len() > MAX_STAGE_FAILURE_CODE_BYTES
            || message.len() > MAX_STAGE_FAILURE_MESSAGE_BYTES
        {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(Self {
            phase,
            code,
            message,
        })
    }

    pub fn phase(&self) -> ProjectConsumerStageFailurePhase {
        self.phase
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Consumer result for a compatible CPU candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectConsumerStageOutcome {
    Ready,
    Failed(ProjectConsumerStageFailure),
}

/// Whether a stage report has only entered the bounded handoff slot or the actor applied it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectConsumerReportProgress {
    Queued,
    Applied,
}

/// Nonblocking consumer-port errors. Every error is effect-free and permits retry only where the
/// caller retains the same identity and operation.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProjectConsumerError {
    #[error("project generation consumer slot is busy")]
    Busy,
    #[error("project generation consumer is closed")]
    Closed,
    #[error("project generation consumer actor has stopped")]
    Stopped,
    #[error("project consumer identity belongs to another session")]
    ForeignSession,
    #[error("project consumer identity is no longer current")]
    StaleIdentity,
    #[error("project consumer report conflicts with the accepted report")]
    ConflictingReport,
    #[error("project consumer report or presentation is invalid")]
    InvalidReport,
    #[error("project generation consumer state is poisoned")]
    Poisoned,
}

/// Renderer-neutral view data associated with an actual presented project publication.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectPresentedView {
    physical_width: u32,
    physical_height: u32,
    view_projection: [f32; 16],
}

impl ProjectPresentedView {
    pub fn new(
        physical_width: u32,
        physical_height: u32,
        view_projection: [f32; 16],
    ) -> Result<Self, ProjectConsumerError> {
        if physical_width == 0
            || physical_height == 0
            || view_projection.iter().any(|value| !value.is_finite())
        {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(Self {
            physical_width,
            physical_height,
            view_projection,
        })
    }

    pub fn physical_width(&self) -> u32 {
        self.physical_width
    }

    pub fn physical_height(&self) -> u32 {
        self.physical_height
    }

    pub fn view_projection(&self) -> &[f32; 16] {
        &self.view_projection
    }
}

/// Opaque identity copied from a real actor-published `SessionFrame`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectPublicationIdentity {
    session: SessionId,
    tick: Tick,
    publication_sequence: u64,
    authority_snapshot_sequence: u64,
    project_generation: u64,
    source_digest: String,
    scene_revision: u64,
}

impl ProjectPublicationIdentity {
    pub fn from_frame(frame: &SessionFrame) -> Result<Self, ProjectConsumerError> {
        if frame.sequence() == 0
            || frame.project_generation() == 0
            || frame.source_digest().is_empty()
            || frame.source_digest().len() > MAX_SOURCE_DIGEST_BYTES
        {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(Self {
            session: frame.session(),
            tick: frame.tick(),
            publication_sequence: frame.sequence(),
            authority_snapshot_sequence: frame.authority_snapshot_sequence(),
            project_generation: frame.project_generation(),
            source_digest: frame.source_digest().to_owned(),
            scene_revision: frame.source_scene_revision(),
        })
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn tick(&self) -> Tick {
        self.tick
    }

    pub fn publication_sequence(&self) -> u64 {
        self.publication_sequence
    }

    pub fn authority_snapshot_sequence(&self) -> u64 {
        self.authority_snapshot_sequence
    }

    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn scene_revision(&self) -> u64 {
        self.scene_revision
    }
}

/// Actual host presentation report for one real publication and the view used to draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectPresentedFrame {
    publication: ProjectPublicationIdentity,
    present_sequence: u64,
    view: ProjectPresentedView,
}

impl ProjectPresentedFrame {
    pub fn new(
        publication: ProjectPublicationIdentity,
        present_sequence: u64,
        view: ProjectPresentedView,
    ) -> Result<Self, ProjectConsumerError> {
        if present_sequence == 0 {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(Self {
            publication,
            present_sequence,
            view,
        })
    }

    pub fn publication(&self) -> &ProjectPublicationIdentity {
        &self.publication
    }

    pub fn present_sequence(&self) -> u64 {
        self.present_sequence
    }

    pub fn view(&self) -> &ProjectPresentedView {
        &self.view
    }
}

/// Consumer-side progress for the current candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectConsumerStageProgress {
    CpuPreparing,
    WaitingDevice,
    ReadyQueued,
    ReadyApplied,
    FailedQueued,
    FailedApplied,
    Cancelled,
    Superseded,
    Closed,
}

/// Small stage summary visible through the runtime-neutral consumer status API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConsumerStageStatus {
    identity: ProjectSourceIdentity,
    progress: ProjectConsumerStageProgress,
    failure: Option<ProjectConsumerStageFailure>,
}

impl ProjectConsumerStageStatus {
    pub fn identity(&self) -> &ProjectSourceIdentity {
        &self.identity
    }

    pub fn progress(&self) -> ProjectConsumerStageProgress {
        self.progress
    }

    pub fn failure(&self) -> Option<&ProjectConsumerStageFailure> {
        self.failure.as_ref()
    }
}

/// Initial source lease state. Initial source is already authoritative and has no candidate IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectConsumerInitialState {
    Available,
    Taken,
}

/// Bounded, renderer-neutral status for the initial source, current stage, and actual presents.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectGenerationConsumerStatus {
    session: SessionId,
    initial: ProjectConsumerInitialState,
    active_generation: u64,
    active_source_digest: String,
    stage: Option<ProjectConsumerStageStatus>,
    last_applied_present: Option<ProjectPresentedFrame>,
    pending_present_sequence: Option<u64>,
    closed: bool,
    stopped: bool,
}

impl ProjectGenerationConsumerStatus {
    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn initial(&self) -> ProjectConsumerInitialState {
        self.initial
    }

    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }

    pub fn active_source_digest(&self) -> &str {
        &self.active_source_digest
    }

    pub fn stage(&self) -> Option<&ProjectConsumerStageStatus> {
        self.stage.as_ref()
    }

    pub fn last_applied_present(&self) -> Option<&ProjectPresentedFrame> {
        self.last_applied_present.as_ref()
    }

    pub fn pending_present_sequence(&self) -> Option<u64> {
        self.pending_present_sequence
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
}

/// Public single-consumer port returned only by the render-consumer startup factory.
pub struct ProjectGenerationConsumerPort {
    shared: Arc<ProjectGenerationConsumerShared>,
}

impl std::fmt::Debug for ProjectGenerationConsumerPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectGenerationConsumerPort")
            .finish_non_exhaustive()
    }
}

impl ProjectGenerationConsumerPort {
    pub(crate) fn new(shared: Arc<ProjectGenerationConsumerShared>) -> Self {
        Self { shared }
    }

    pub fn try_take_initial(&self) -> Result<Option<ProjectSourceLease>, ProjectConsumerError> {
        self.shared.try_take_initial()
    }

    pub fn try_take_candidate(&self) -> Result<Option<ProjectSourceLease>, ProjectConsumerError> {
        self.shared.try_take_candidate()
    }

    pub fn try_report_stage(
        &self,
        identity: &ProjectSourceIdentity,
        outcome: ProjectConsumerStageOutcome,
    ) -> Result<ProjectConsumerReportProgress, ProjectConsumerError> {
        self.shared.try_report_stage(identity, outcome)
    }

    pub fn try_ack_presented(
        &self,
        presented: ProjectPresentedFrame,
    ) -> Result<ProjectConsumerReportProgress, ProjectConsumerError> {
        self.shared.try_ack_presented(presented)
    }

    pub fn try_status(&self) -> Result<ProjectGenerationConsumerStatus, ProjectConsumerError> {
        self.shared.try_status()
    }

    /// Closes the consumer capability without waiting for the actor or worker.
    pub fn request_close(&self) {
        self.shared.request_close();
    }

    #[cfg(test)]
    pub(crate) fn hold_state_for_test(&self) -> impl Drop + '_ {
        self.shared.state.lock().unwrap()
    }
}

impl Drop for ProjectGenerationConsumerPort {
    fn drop(&mut self) {
        self.request_close();
    }
}

#[derive(Clone)]
pub(crate) struct ProjectGenerationConsumerShared {
    state: Arc<Mutex<ConsumerState>>,
    closed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

struct ConsumerState {
    session: SessionId,
    active_generation: u64,
    active_source_digest: Option<String>,
    active_scene_revision: u64,
    initial_lease: Option<ProjectSourceLease>,
    initial_taken: bool,
    initial_currentness: Arc<AtomicBool>,
    active_currentness: Arc<AtomicBool>,
    initialized: bool,
    stage: Option<ConsumerStageSlot>,
    latest_publication: Option<ProjectPublicationIdentity>,
    latest_applied_present: Option<ProjectPresentedFrame>,
    latest_queued_present: Option<ProjectPresentedFrame>,
    last_present_sequence: u64,
    ack_wait_source: Option<Arc<PreparedProject>>,
}

struct ConsumerStageSlot {
    status: ProjectConsumerStageStatus,
    lease: Option<ProjectSourceLease>,
    taken: bool,
    currentness: Arc<AtomicBool>,
    queued_report: Option<ProjectConsumerStageOutcome>,
    applied_report: Option<ProjectConsumerStageOutcome>,
}

impl ProjectGenerationConsumerShared {
    pub(crate) fn new_pending(session: SessionId) -> Self {
        let initial_currentness = Arc::new(AtomicBool::new(false));
        Self {
            state: Arc::new(Mutex::new(ConsumerState {
                session,
                active_generation: 1,
                active_source_digest: None,
                active_scene_revision: 0,
                initial_lease: None,
                initial_taken: false,
                active_currentness: Arc::clone(&initial_currentness),
                initial_currentness,
                initialized: false,
                stage: None,
                latest_publication: None,
                latest_applied_present: None,
                latest_queued_present: None,
                last_present_sequence: 0,
                ack_wait_source: None,
            })),
            closed: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Called once by the preparation worker after its initial import succeeds. The worker still
    /// owns the canonical project Arc, so releasing an untaken lease here can never run the final
    /// PreparedProject destructor on the actor or consumer thread.
    pub(crate) fn initialize_initial(&self, project: Arc<PreparedProject>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(!state.initialized);
        debug_assert!(!self.closed.load(Ordering::Acquire));
        debug_assert!(!self.stopped.load(Ordering::Acquire));
        let identity = ProjectSourceIdentity::initial(state.session, project.digest());
        state.active_source_digest = Some(project.digest().to_owned());
        state.active_scene_revision = project.scene().revision;
        state.initial_currentness.store(true, Ordering::Release);
        state.active_currentness = Arc::clone(&state.initial_currentness);
        state.initial_lease = Some(ProjectSourceLease::new(
            identity,
            project,
            Arc::clone(&state.initial_currentness),
            Arc::clone(&self.closed),
            Arc::clone(&self.stopped),
        ));
        state.initialized = true;
    }

    pub(crate) fn matches_initial_project(&self, project: &PreparedProject) -> bool {
        let Ok(state) = self.state.try_lock() else {
            return false;
        };
        state.initialized
            && state.active_generation == 1
            && state.active_source_digest.as_deref() == Some(project.digest())
            && state.active_scene_revision == project.scene().revision
    }

    fn lock_read(&self) -> Result<std::sync::MutexGuard<'_, ConsumerState>, ProjectConsumerError> {
        match self.state.try_lock() {
            Ok(state) => Ok(state),
            Err(TryLockError::WouldBlock) => Err(ProjectConsumerError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(ProjectConsumerError::Poisoned),
        }
    }

    pub(crate) fn session(&self) -> Result<SessionId, ProjectConsumerError> {
        Ok(self.lock_read()?.session)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(crate) fn try_stage_guard(
        &self,
        identity: ProjectSourceIdentity,
    ) -> Result<ProjectGenerationStageAdmissionGuard<'_>, ProjectConsumerError> {
        let state = self.lock_read()?;
        self.check_open()?;
        if !state.initialized || state.active_source_digest.is_none() {
            return Err(ProjectConsumerError::Busy);
        }
        let session = state.session;
        let active_generation = state.active_generation;
        if identity.kind != ProjectSourceKind::Candidate
            || identity.session != session
            || identity.expected_generation != Some(active_generation)
            || identity.proposed_generation != active_generation.checked_add(1)
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        Ok(ProjectGenerationStageAdmissionGuard {
            state,
            identity: Some(identity),
        })
    }

    pub(crate) fn try_publish_candidate(
        &self,
        identity: &ProjectSourceIdentity,
        project: Arc<PreparedProject>,
    ) -> Result<(), ProjectConsumerError> {
        let mut state = self.lock_read()?;
        self.check_open()?;
        let session = state.session;
        let active_generation = state.active_generation;
        if identity.kind != ProjectSourceKind::Candidate
            || identity.session != session
            || identity.expected_generation != Some(active_generation)
            || identity.proposed_generation != active_generation.checked_add(1)
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        let stage = state
            .stage
            .as_mut()
            .filter(|stage| stage.status.identity == *identity)
            .ok_or(ProjectConsumerError::StaleIdentity)?;
        if stage.status.progress != ProjectConsumerStageProgress::CpuPreparing {
            return Err(ProjectConsumerError::ConflictingReport);
        }
        if !stage.currentness.load(Ordering::Acquire) {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if project.digest() != identity.source_digest {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        stage.lease = Some(ProjectSourceLease::new(
            identity.clone(),
            project,
            Arc::clone(&stage.currentness),
            Arc::clone(&self.closed),
            Arc::clone(&self.stopped),
        ));
        stage.status.progress = ProjectConsumerStageProgress::WaitingDevice;
        Ok(())
    }

    pub(crate) fn try_mark_internal_stage_failure(
        &self,
        identity: &ProjectSourceIdentity,
        failure: ProjectConsumerStageFailure,
    ) -> Result<(), ProjectConsumerError> {
        let mut state = self.lock_read()?;
        self.check_open()?;
        let active_generation = state.active_generation;
        if identity.kind != ProjectSourceKind::Candidate
            || identity.session != state.session
            || identity.expected_generation != Some(active_generation)
            || identity.proposed_generation != active_generation.checked_add(1)
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        let stage = state
            .stage
            .as_mut()
            .filter(|stage| stage.status.identity == *identity)
            .ok_or(ProjectConsumerError::StaleIdentity)?;
        if stage.status.progress != ProjectConsumerStageProgress::CpuPreparing {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if !stage.currentness.load(Ordering::Acquire) {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        stage.currentness.store(false, Ordering::Release);
        stage.lease = None;
        stage.status.progress = ProjectConsumerStageProgress::FailedApplied;
        stage.status.failure = Some(failure);
        Ok(())
    }

    pub(crate) fn try_invalidate_stage(
        &self,
        progress: ProjectConsumerStageProgress,
    ) -> Result<(), ProjectConsumerError> {
        let mut state = self.lock_read()?;
        if let Some(stage) = state.stage.as_mut() {
            stage.currentness.store(false, Ordering::Release);
            stage.lease = None;
            stage.status.progress = progress;
            stage.queued_report = None;
        }
        Ok(())
    }

    pub(crate) fn try_pending_stage_report(
        &self,
    ) -> Result<Option<(ProjectSourceIdentity, ProjectConsumerStageOutcome)>, ProjectConsumerError>
    {
        let state = self.lock_read()?;
        Ok(state.stage.as_ref().and_then(|stage| {
            stage
                .queued_report
                .as_ref()
                .map(|outcome| (stage.status.identity.clone(), outcome.clone()))
        }))
    }

    pub(crate) fn try_stage_report_guard(
        &self,
        identity: &ProjectSourceIdentity,
        outcome: &ProjectConsumerStageOutcome,
    ) -> Result<ProjectGenerationStageReportGuard<'_>, ProjectConsumerError> {
        let mut state = self.lock_read()?;
        self.check_open()?;
        let active_generation = state.active_generation;
        if identity.kind != ProjectSourceKind::Candidate
            || identity.session != state.session
            || identity.expected_generation != Some(active_generation)
            || identity.proposed_generation != active_generation.checked_add(1)
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        let stage = state
            .stage
            .as_mut()
            .filter(|stage| stage.status.identity == *identity)
            .ok_or(ProjectConsumerError::StaleIdentity)?;
        if stage.queued_report.as_ref() != Some(outcome) {
            return Err(ProjectConsumerError::ConflictingReport);
        }
        if !stage.currentness.load(Ordering::Acquire) {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        Ok(ProjectGenerationStageReportGuard {
            state,
            identity: identity.clone(),
            outcome: outcome.clone(),
        })
    }

    pub(crate) fn try_drop_stale_stage_report(
        &self,
        identity: &ProjectSourceIdentity,
        outcome: &ProjectConsumerStageOutcome,
    ) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if let Some(stage) = state.stage.as_mut()
            && stage.status.identity == *identity
            && stage.queued_report.as_ref() == Some(outcome)
        {
            stage.queued_report = None;
        }
    }

    pub(crate) fn try_publication_guard(
        &self,
        publication: ProjectPublicationIdentity,
    ) -> Result<ProjectGenerationPublicationGuard<'_>, ProjectConsumerError> {
        let state = self.lock_read()?;
        self.check_open()?;
        let active_source_digest = state
            .active_source_digest
            .as_deref()
            .ok_or(ProjectConsumerError::Busy)?;
        if publication.session != state.session
            || publication.project_generation != state.active_generation
            || publication.source_digest != active_source_digest
            || publication.scene_revision != state.active_scene_revision
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if let Some(previous) = state.latest_publication.as_ref() {
            if publication.publication_sequence == previous.publication_sequence {
                return if &publication == previous {
                    Ok(ProjectGenerationPublicationGuard { state, publication })
                } else {
                    Err(ProjectConsumerError::ConflictingReport)
                };
            }
            if publication.publication_sequence < previous.publication_sequence
                || publication.authority_snapshot_sequence < previous.authority_snapshot_sequence
                || publication.tick < previous.tick
            {
                return Err(ProjectConsumerError::StaleIdentity);
            }
        }
        Ok(ProjectGenerationPublicationGuard { state, publication })
    }

    pub(crate) fn try_pending_present(
        &self,
    ) -> Result<Option<ProjectPresentedFrame>, ProjectConsumerError> {
        Ok(self.lock_read()?.latest_queued_present.clone())
    }

    pub(crate) fn try_drop_stale_present(&self, presented: &ProjectPresentedFrame) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.latest_queued_present.as_ref() == Some(presented) {
            state.latest_queued_present = None;
        }
    }

    pub(crate) fn try_apply_present(
        &self,
        presented: &ProjectPresentedFrame,
    ) -> Result<(), ProjectConsumerError> {
        let mut state = self.lock_read()?;
        self.check_open()?;
        let active_source_digest = state
            .active_source_digest
            .as_deref()
            .ok_or(ProjectConsumerError::Busy)?;
        if presented.publication.session != state.session
            || presented.publication.project_generation != state.active_generation
            || presented.publication.source_digest != active_source_digest
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if state
            .latest_applied_present
            .as_ref()
            .is_some_and(|last| last.present_sequence() >= presented.present_sequence())
        {
            return Ok(());
        }
        if let Some(queued) = state.latest_queued_present.as_ref()
            && (queued.present_sequence() > presented.present_sequence()
                || queued.publication.publication_sequence
                    > presented.publication.publication_sequence)
        {
            // A newer report arrived after this one was copied by the actor. Applying the older
            // ACK must not consume the newer bounded slot or regress its view watermark.
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if state.latest_queued_present.as_ref() != Some(presented) {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        state.latest_queued_present = None;
        state.last_present_sequence = state
            .last_present_sequence
            .max(presented.present_sequence());
        state.latest_applied_present = Some(presented.clone());
        if presented.publication.project_generation == state.active_generation {
            state.ack_wait_source = None;
        }
        Ok(())
    }

    pub(crate) fn try_commit_guard(
        &self,
    ) -> Result<ProjectGenerationCommitGuard<'_>, ProjectConsumerError> {
        let state = self.lock_read()?;
        self.check_open()?;
        let Some(active_source_digest) = state.active_source_digest.as_deref() else {
            return Err(ProjectConsumerError::Busy);
        };
        let current_ack = state
            .latest_applied_present
            .as_ref()
            .is_some_and(|present| {
                present.publication.session == state.session
                    && present.publication.project_generation == state.active_generation
                    && present.publication.source_digest == active_source_digest
            });
        if !current_ack {
            return Err(ProjectConsumerError::Busy);
        }
        let stage_ready = state.stage.as_ref().is_some_and(|stage| {
            stage.status.progress == ProjectConsumerStageProgress::ReadyApplied
                && stage.status.identity.expected_generation() == Some(state.active_generation)
                && stage.status.identity.proposed_generation()
                    == state.active_generation.checked_add(1)
                && stage.currentness.load(Ordering::Acquire)
        });
        if !stage_ready {
            return Err(ProjectConsumerError::Busy);
        }
        Ok(ProjectGenerationCommitGuard { state })
    }

    pub(crate) fn try_status(
        &self,
    ) -> Result<ProjectGenerationConsumerStatus, ProjectConsumerError> {
        let state = self.lock_read()?;
        let status = ProjectGenerationConsumerStatus {
            session: state.session,
            initial: if state.initial_taken {
                ProjectConsumerInitialState::Taken
            } else {
                ProjectConsumerInitialState::Available
            },
            active_generation: state.active_generation,
            active_source_digest: state.active_source_digest.clone().unwrap_or_default(),
            stage: state.stage.as_ref().map(|stage| stage.status.clone()),
            last_applied_present: state.latest_applied_present.clone(),
            pending_present_sequence: state
                .latest_queued_present
                .as_ref()
                .map(ProjectPresentedFrame::present_sequence),
            closed: self.closed.load(Ordering::Acquire),
            stopped: self.stopped.load(Ordering::Acquire),
        };
        let logical_bytes = std::mem::size_of::<ProjectGenerationConsumerStatus>()
            .saturating_add(status.active_source_digest.len())
            .saturating_add(status.stage.as_ref().map_or(0, |stage| {
                stage.identity.source_digest.len() + stage.status_failure_bytes()
            }))
            .saturating_add(status.last_applied_present.as_ref().map_or(0, |present| {
                present.publication.source_digest.len()
                    + std::mem::size_of::<ProjectPresentedView>()
            }));
        if logical_bytes > MAX_PROJECT_CONSUMER_STATUS_BYTES {
            return Err(ProjectConsumerError::InvalidReport);
        }
        Ok(status)
    }

    fn check_open(&self) -> Result<(), ProjectConsumerError> {
        if self.stopped.load(Ordering::Acquire) {
            Err(ProjectConsumerError::Stopped)
        } else if self.closed.load(Ordering::Acquire) {
            Err(ProjectConsumerError::Closed)
        } else {
            Ok(())
        }
    }

    fn try_take_initial(&self) -> Result<Option<ProjectSourceLease>, ProjectConsumerError> {
        self.check_open()?;
        let mut state = self.lock_read()?;
        self.check_open()?;
        if !state.initialized {
            return Ok(None);
        }
        if state.initial_taken {
            return Ok(None);
        }
        let Some(lease) = state.initial_lease.take() else {
            return Ok(None);
        };
        state.initial_taken = true;
        Ok(Some(lease))
    }

    fn try_take_candidate(&self) -> Result<Option<ProjectSourceLease>, ProjectConsumerError> {
        self.check_open()?;
        let mut state = self.lock_read()?;
        self.check_open()?;
        let active_generation = state.active_generation;
        let Some(stage) = state.stage.as_mut() else {
            return Ok(None);
        };
        if stage.status.progress != ProjectConsumerStageProgress::WaitingDevice || stage.taken {
            return Ok(None);
        }
        if stage.status.identity.expected_generation() != Some(active_generation)
            || stage.status.identity.proposed_generation() != active_generation.checked_add(1)
            || !stage.currentness.load(Ordering::Acquire)
        {
            return Ok(None);
        }
        let Some(lease) = stage.lease.take() else {
            return Ok(None);
        };
        stage.taken = true;
        Ok(Some(lease))
    }

    fn try_report_stage(
        &self,
        identity: &ProjectSourceIdentity,
        outcome: ProjectConsumerStageOutcome,
    ) -> Result<ProjectConsumerReportProgress, ProjectConsumerError> {
        self.check_open()?;
        let mut state = self.lock_read()?;
        self.check_open()?;
        if identity.session != state.session {
            return Err(ProjectConsumerError::ForeignSession);
        }
        let active_generation = state.active_generation;
        if identity.kind != ProjectSourceKind::Candidate
            || identity.expected_generation != Some(active_generation)
            || identity.proposed_generation != active_generation.checked_add(1)
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        let stage = state
            .stage
            .as_mut()
            .filter(|stage| stage.status.identity == *identity)
            .ok_or(ProjectConsumerError::StaleIdentity)?;
        if !stage.currentness.load(Ordering::Acquire)
            && !matches!(
                stage.status.progress,
                ProjectConsumerStageProgress::FailedApplied
                    | ProjectConsumerStageProgress::ReadyApplied
            )
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if let Some(applied) = stage.applied_report.as_ref() {
            return if applied == &outcome {
                Ok(ProjectConsumerReportProgress::Applied)
            } else {
                Err(ProjectConsumerError::ConflictingReport)
            };
        }
        if let Some(queued) = stage.queued_report.as_ref() {
            return if queued == &outcome {
                Ok(ProjectConsumerReportProgress::Queued)
            } else {
                Err(ProjectConsumerError::ConflictingReport)
            };
        }
        if !stage.taken
            || !stage.currentness.load(Ordering::Acquire)
            || stage.status.progress != ProjectConsumerStageProgress::WaitingDevice
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        stage.status.progress = match outcome {
            ProjectConsumerStageOutcome::Ready => ProjectConsumerStageProgress::ReadyQueued,
            ProjectConsumerStageOutcome::Failed(_) => ProjectConsumerStageProgress::FailedQueued,
        };
        if let ProjectConsumerStageOutcome::Failed(failure) = &outcome {
            stage.status.failure = Some(failure.clone());
        }
        stage.queued_report = Some(outcome);
        Ok(ProjectConsumerReportProgress::Queued)
    }

    fn try_ack_presented(
        &self,
        presented: ProjectPresentedFrame,
    ) -> Result<ProjectConsumerReportProgress, ProjectConsumerError> {
        self.check_open()?;
        let mut state = self.lock_read()?;
        self.check_open()?;
        let publication = &presented.publication;
        if publication.session != state.session {
            return Err(ProjectConsumerError::ForeignSession);
        }
        if !state.initialized {
            return Err(ProjectConsumerError::Busy);
        }
        if publication.project_generation != state.active_generation
            || Some(publication.source_digest.as_str()) != state.active_source_digest.as_deref()
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        let Some(latest) = state.latest_publication.as_ref() else {
            return Err(ProjectConsumerError::Busy);
        };
        if publication.publication_sequence > latest.publication_sequence
            || publication.authority_snapshot_sequence > latest.authority_snapshot_sequence
            || publication.tick > latest.tick
            || publication.scene_revision != latest.scene_revision
        {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if !state.initial_taken && state.active_generation == 1 {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        if let Some(applied) = state.latest_applied_present.as_ref()
            && applied.publication.project_generation == publication.project_generation
        {
            if applied.present_sequence() == presented.present_sequence() {
                return if applied == &presented {
                    Ok(ProjectConsumerReportProgress::Applied)
                } else {
                    Err(ProjectConsumerError::ConflictingReport)
                };
            }
            if applied.present_sequence() > presented.present_sequence()
                && publication.publication_sequence <= applied.publication.publication_sequence
            {
                return Ok(ProjectConsumerReportProgress::Applied);
            }
            if publication.publication_sequence < applied.publication.publication_sequence
                || publication.authority_snapshot_sequence
                    < applied.publication.authority_snapshot_sequence
                || publication.tick < applied.publication.tick
            {
                return Err(ProjectConsumerError::StaleIdentity);
            }
        }
        if let Some(queued) = state.latest_queued_present.as_ref() {
            if queued.present_sequence() == presented.present_sequence() {
                return if queued == &presented {
                    Ok(ProjectConsumerReportProgress::Queued)
                } else {
                    Err(ProjectConsumerError::ConflictingReport)
                };
            }
            if presented.present_sequence() < queued.present_sequence()
                || publication.publication_sequence < queued.publication.publication_sequence
            {
                return Err(ProjectConsumerError::StaleIdentity);
            }
        } else if presented.present_sequence() <= state.last_present_sequence {
            return Err(ProjectConsumerError::StaleIdentity);
        }
        state.last_present_sequence = presented.present_sequence();
        state.latest_queued_present = Some(presented);
        Ok(ProjectConsumerReportProgress::Queued)
    }

    pub(crate) fn request_close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.clear_unclaimed();
    }

    pub(crate) fn request_stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.clear_unclaimed();
    }

    pub(crate) fn actor_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
        self.clear_unclaimed();
    }

    pub(crate) fn try_finish_close(&self) -> Result<(), ProjectConsumerError> {
        if !self.is_closed() {
            return Ok(());
        }
        let mut state = self.lock_read()?;
        self.clear_state_locked(&mut state);
        Ok(())
    }

    fn clear_unclaimed(&self) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        self.clear_state_locked(&mut state);
    }

    /// Worker-only cleanup may wait briefly for the bounded state lock. Unlike request_close,
    /// this runs on the canonical source owner and is required before it releases registry Arcs.
    pub(crate) fn worker_stopping(&self) {
        self.stopped.store(true, Ordering::Release);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.clear_state_locked(&mut state);
    }

    fn clear_state_locked(&self, state: &mut ConsumerState) {
        state.initial_currentness.store(false, Ordering::Release);
        state.active_currentness.store(false, Ordering::Release);
        state.initial_lease = None;
        state.initial_taken = true;
        if let Some(stage) = state.stage.as_mut() {
            stage.currentness.store(false, Ordering::Release);
            stage.lease = None;
            stage.queued_report = None;
            stage.status.progress = ProjectConsumerStageProgress::Closed;
        }
        state.latest_queued_present = None;
        state.ack_wait_source = None;
    }
}

/// Holds the consumer state lock across the existing worker's nonblocking Stage admission. It
/// mutates/retires the prior consumer slot only after that worker admission succeeds.
pub(crate) struct ProjectGenerationStageAdmissionGuard<'a> {
    state: std::sync::MutexGuard<'a, ConsumerState>,
    identity: Option<ProjectSourceIdentity>,
}

impl ProjectGenerationStageAdmissionGuard<'_> {
    pub(crate) fn apply(mut self) -> Arc<AtomicBool> {
        let identity = self.identity.take().expect("stage guard identity");
        let active_generation = self.state.active_generation;
        if let Some(old) = self.state.stage.as_mut() {
            if old.status.identity.generation != active_generation {
                old.currentness.store(false, Ordering::Release);
            }
            old.lease = None;
            old.status.progress = ProjectConsumerStageProgress::Superseded;
            old.queued_report = None;
        }
        // The actor has accepted the corresponding preparation Stage before applying this guard.
        // Starting current means cancellation can invalidate it through the actor-owned atomic
        // even while the consumer metadata mutex is contended.
        let currentness = Arc::new(AtomicBool::new(true));
        self.state.stage = Some(ConsumerStageSlot {
            status: ProjectConsumerStageStatus {
                identity,
                progress: ProjectConsumerStageProgress::CpuPreparing,
                failure: None,
            },
            lease: None,
            taken: false,
            currentness: Arc::clone(&currentness),
            queued_report: None,
            applied_report: None,
        });
        currentness
    }
}

pub(crate) struct ProjectGenerationCommitGuard<'a> {
    state: std::sync::MutexGuard<'a, ConsumerState>,
}

pub(crate) struct ProjectGenerationStageReportGuard<'a> {
    state: std::sync::MutexGuard<'a, ConsumerState>,
    identity: ProjectSourceIdentity,
    outcome: ProjectConsumerStageOutcome,
}

impl ProjectGenerationStageReportGuard<'_> {
    /// Applies a previously validated queued report while retaining the lock that kept the
    /// worker's Staged transition and the consumer's ReadyApplied transition in one actor step.
    pub(crate) fn apply(mut self) {
        let stage = self
            .state
            .stage
            .as_mut()
            .expect("validated stage report slot remains present while locked");
        debug_assert_eq!(stage.status.identity, self.identity);
        debug_assert_eq!(stage.queued_report.as_ref(), Some(&self.outcome));
        stage.queued_report = None;
        stage.applied_report = Some(self.outcome.clone());
        match self.outcome {
            ProjectConsumerStageOutcome::Ready => {
                stage.status.progress = ProjectConsumerStageProgress::ReadyApplied;
                stage.status.failure = None;
            }
            ProjectConsumerStageOutcome::Failed(failure) => {
                stage.currentness.store(false, Ordering::Release);
                stage.lease = None;
                stage.status.progress = ProjectConsumerStageProgress::FailedApplied;
                stage.status.failure = Some(failure);
            }
        }
    }
}

pub(crate) struct ProjectGenerationPublicationGuard<'a> {
    state: std::sync::MutexGuard<'a, ConsumerState>,
    publication: ProjectPublicationIdentity,
}

impl ProjectGenerationPublicationGuard<'_> {
    pub(crate) fn apply(mut self) {
        self.state.latest_publication = Some(self.publication);
    }
}

impl ProjectGenerationCommitGuard<'_> {
    pub(crate) fn apply_commit(
        &mut self,
        generation: u64,
        source_digest: String,
        scene_revision: u64,
        old_source: Arc<PreparedProject>,
    ) {
        self.state
            .active_currentness
            .store(false, Ordering::Release);
        self.state
            .initial_currentness
            .store(false, Ordering::Release);
        let active_currentness = if let Some(stage) = self.state.stage.as_mut() {
            if stage.status.identity.generation == generation {
                stage.currentness.store(true, Ordering::Release);
                Some(Arc::clone(&stage.currentness))
            } else {
                None
            }
        } else {
            None
        };
        if let Some(currentness) = active_currentness {
            self.state.active_currentness = currentness;
        }
        self.state.active_generation = generation;
        self.state.active_source_digest = Some(source_digest);
        self.state.active_scene_revision = scene_revision;
        self.state.ack_wait_source = Some(old_source);
        self.state.latest_queued_present = None;
    }
}

impl ProjectConsumerStageStatus {
    fn status_failure_bytes(&self) -> usize {
        self.failure.as_ref().map_or(0, |failure| {
            failure.code.len().saturating_add(failure.message.len())
        })
    }
}
