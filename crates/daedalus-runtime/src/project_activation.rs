//! Bounded metadata for native headless prepared-project activation.

use daedalus_content::AssetReplacementCompatibilityError;
use daedalus_core::{CommandId, SessionId, Tick};

use crate::project_preparation::PreparationId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ActivationId {
    session: SessionId,
    sequence: u64,
}

impl ActivationId {
    pub const fn from_parts(session: SessionId, sequence: u64) -> Self {
        Self { session, sequence }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssetActivationOperation {
    Status,
    Stage {
        command_id: Option<CommandId>,
        candidate: PreparationId,
        expected_active_generation: u64,
    },
    Commit {
        command_id: Option<CommandId>,
        activation: ActivationId,
        expected_active_generation: u64,
    },
    Cancel {
        command_id: Option<CommandId>,
        activation: ActivationId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetActivationState {
    Idle,
    StagePending,
    Staging,
    Staged,
    CancelRequested,
    Cancelled,
    Superseded,
    Committed,
    Failed,
    Stopping,
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetActivationSummary {
    id: ActivationId,
    candidate: PreparationId,
    expected_active_generation: u64,
    source_digest: String,
    committed_generation: Option<u64>,
}

impl AssetActivationSummary {
    pub(crate) fn new(
        id: ActivationId,
        candidate: PreparationId,
        expected_active_generation: u64,
        source_digest: String,
        committed_generation: Option<u64>,
    ) -> Self {
        Self {
            id,
            candidate,
            expected_active_generation,
            source_digest,
            committed_generation,
        }
    }

    pub fn id(&self) -> ActivationId {
        self.id
    }
    pub fn candidate(&self) -> PreparationId {
        self.candidate
    }
    pub fn expected_active_generation(&self) -> u64 {
        self.expected_active_generation
    }
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }
    pub fn committed_generation(&self) -> Option<u64> {
        self.committed_generation
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetActivationFailure {
    id: ActivationId,
    code: String,
    message: String,
    compatibility: Option<AssetReplacementCompatibilityError>,
}

impl AssetActivationFailure {
    pub(crate) fn new(
        id: ActivationId,
        code: String,
        message: String,
        compatibility: Option<AssetReplacementCompatibilityError>,
    ) -> Self {
        Self {
            id,
            code,
            message,
            compatibility,
        }
    }

    pub fn id(&self) -> ActivationId {
        self.id
    }
    pub fn code(&self) -> &str {
        &self.code
    }
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn compatibility(&self) -> Option<&AssetReplacementCompatibilityError> {
        self.compatibility.as_ref()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssetActivationMemoryStatus {
    active_cooked_accounted_bytes: u64,
    candidate_cooked_accounted_bytes: u64,
    retired_cooked_accounted_bytes: u64,
    max_cooked_accounted_bytes: u64,
    retired_generation_count: usize,
}

impl AssetActivationMemoryStatus {
    pub(crate) fn new(
        active_cooked_accounted_bytes: u64,
        candidate_cooked_accounted_bytes: u64,
        retired_cooked_accounted_bytes: u64,
        max_cooked_accounted_bytes: u64,
        retired_generation_count: usize,
    ) -> Self {
        Self {
            active_cooked_accounted_bytes,
            candidate_cooked_accounted_bytes,
            retired_cooked_accounted_bytes,
            max_cooked_accounted_bytes,
            retired_generation_count,
        }
    }

    pub fn active_cooked_accounted_bytes(&self) -> u64 {
        self.active_cooked_accounted_bytes
    }
    pub fn candidate_cooked_accounted_bytes(&self) -> u64 {
        self.candidate_cooked_accounted_bytes
    }
    pub fn retired_cooked_accounted_bytes(&self) -> u64 {
        self.retired_cooked_accounted_bytes
    }
    pub fn total_cooked_accounted_bytes(&self) -> u64 {
        self.active_cooked_accounted_bytes
            .saturating_add(self.candidate_cooked_accounted_bytes)
            .saturating_add(self.retired_cooked_accounted_bytes)
    }
    pub fn max_cooked_accounted_bytes(&self) -> u64 {
        self.max_cooked_accounted_bytes
    }
    pub fn retired_generation_count(&self) -> usize {
        self.retired_generation_count
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetActivationStatus {
    session: SessionId,
    state: AssetActivationState,
    active_generation: u64,
    active_source_digest: String,
    current_activation: Option<AssetActivationSummary>,
    latest_failure: Option<AssetActivationFailure>,
    memory: AssetActivationMemoryStatus,
}

impl AssetActivationStatus {
    pub(crate) fn new(
        session: SessionId,
        state: AssetActivationState,
        active_generation: u64,
        active_source_digest: String,
        current_activation: Option<AssetActivationSummary>,
        latest_failure: Option<AssetActivationFailure>,
        memory: AssetActivationMemoryStatus,
    ) -> Self {
        Self {
            session,
            state,
            active_generation,
            active_source_digest,
            current_activation,
            latest_failure,
            memory,
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }
    pub fn state(&self) -> AssetActivationState {
        self.state
    }
    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }
    pub fn active_source_digest(&self) -> &str {
        &self.active_source_digest
    }
    pub fn current_activation(&self) -> Option<&AssetActivationSummary> {
        self.current_activation.as_ref()
    }
    pub fn latest_failure(&self) -> Option<&AssetActivationFailure> {
        self.latest_failure.as_ref()
    }
    pub fn memory(&self) -> &AssetActivationMemoryStatus {
        &self.memory
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssetActivationCommandOutcome {
    StageAccepted {
        activation: ActivationId,
        candidate: PreparationId,
    },
    CommitApplied {
        activation: ActivationId,
        generation: u64,
    },
    CancelAccepted {
        activation: ActivationId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssetActivationCommandResult {
    command_id: CommandId,
    observed_tick: Tick,
    outcome: AssetActivationCommandOutcome,
}

impl AssetActivationCommandResult {
    pub(crate) fn new(
        command_id: CommandId,
        observed_tick: Tick,
        outcome: AssetActivationCommandOutcome,
    ) -> Self {
        Self {
            command_id,
            observed_tick,
            outcome,
        }
    }

    pub fn command_id(&self) -> CommandId {
        self.command_id
    }
    pub fn observed_tick(&self) -> Tick {
        self.observed_tick
    }
    pub fn outcome(&self) -> &AssetActivationCommandOutcome {
        &self.outcome
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssetActivationRequestResult {
    Status(Box<AssetActivationStatus>),
    Command(AssetActivationCommandResult),
}
