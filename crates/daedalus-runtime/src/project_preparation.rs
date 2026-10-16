//! Native, bounded preparation of immutable project candidates.
//!
//! The worker owns one filesystem cook cache and never publishes a prepared project through the
//! session control port. The actor sees compact status only; candidate payloads remain private to
//! this module until they are discarded by the worker.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use daedalus_content::assets::{
    AssetImportError, AssetImportErrorCode, AssetImportMemorySummary, AssetImportMetrics,
    CacheLimits, FileCookCache, ImportCancellation, ImportLimits, NativeProjectResolver,
};
use daedalus_content::project::{PreparedProject, load_project_with_assets};
use daedalus_core::{CommandId, SessionId, Tick};
use thiserror::Error;

const DEFAULT_MAX_CATALOG_ACCOUNTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FAILURE_CODE_BYTES: usize = 64;
const MAX_FAILURE_MESSAGE_BYTES: usize = 2_048;
const MAX_FAILURE_SOURCE_ID_BYTES: usize = 4_096;
const MAX_FAILURE_PATH_BYTES: usize = 4_096;

/// Conservative logical status-result size checked before cloning strings or metrics.
///
/// The value covers the fixed DTO layouts, up to two digests, fixed memory/import metrics, the
/// bounded diagnostic fields, and bounded optional status metadata. The protocol writer enforces
/// its separate encoded-result cap. This is not an estimate of candidate payload bytes.
pub(crate) const MAX_PREPARATION_STATUS_RESULT_BYTES: usize = 32 * 1024;

/// Trusted, process-local configuration for the native preparation worker.
#[derive(Clone, Debug)]
pub struct NativeProjectPreparationOptions {
    root: PathBuf,
    cache_root: PathBuf,
    import_limits: ImportLimits,
    cache_limits: CacheLimits,
    max_catalog_accounted_bytes: u64,
}

impl NativeProjectPreparationOptions {
    /// Creates native preparation options using `<root>/.daedalus/cache` and engine defaults.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        Self {
            cache_root: root.join(".daedalus").join("cache"),
            root,
            import_limits: ImportLimits::default(),
            cache_limits: CacheLimits::default(),
            max_catalog_accounted_bytes: DEFAULT_MAX_CATALOG_ACCOUNTED_BYTES,
        }
    }

    /// Uses a trusted cache root. The content cache adds its own versioned subdirectory.
    #[must_use]
    pub fn with_cache_root(mut self, cache_root: impl Into<PathBuf>) -> Self {
        self.cache_root = cache_root.into();
        self
    }

    /// Uses existing bounded asset-import limits.
    #[must_use]
    pub fn with_import_limits(mut self, import_limits: ImportLimits) -> Self {
        self.import_limits = import_limits;
        self
    }

    /// Uses existing bounded cook-cache limits.
    #[must_use]
    pub fn with_cache_limits(mut self, cache_limits: CacheLimits) -> Self {
        self.cache_limits = cache_limits;
        self
    }

    /// Sets the total logical cooked/catalog ledger allowed for active plus candidate projects.
    ///
    /// This is not a process-memory limit. The value must be positive and no greater than the
    /// engine's 256 MiB hard ceiling.
    #[must_use]
    pub fn with_max_catalog_accounted_bytes(mut self, max_bytes: u64) -> Self {
        self.max_catalog_accounted_bytes = max_bytes;
        self
    }

    fn validate(&self) -> Result<(), NativeProjectPreparationStartError> {
        self.import_limits
            .validate()
            .map_err(NativeProjectPreparationStartError::Import)?;
        self.cache_limits
            .validate()
            .map_err(NativeProjectPreparationStartError::Import)?;
        if self.max_catalog_accounted_bytes == 0
            || self.max_catalog_accounted_bytes > DEFAULT_MAX_CATALOG_ACCOUNTED_BYTES
        {
            return Err(NativeProjectPreparationStartError::InvalidOptions(
                "catalog accounted-byte limit must be positive and at most 256 MiB",
            ));
        }
        Ok(())
    }
}

/// Failure while opening the native worker and preparing its initial active project.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum NativeProjectPreparationStartError {
    #[error("invalid native preparation options: {0}")]
    InvalidOptions(&'static str),
    #[error("native project import failed: {0}")]
    Import(AssetImportError),
    #[error("initial project's cooked/catalog ledger exceeds the configured capacity")]
    Capacity,
    #[error("the asset-aware initial project did not provide an accounting summary")]
    MissingMemorySummary,
    #[error("native filesystem preparation is unavailable on this target")]
    NativeUnavailable,
    #[error("could not start the native project preparation worker: {0}")]
    ThreadSpawn(String),
    #[error("preparation worker panicked before its initial project was ready")]
    WorkerPanicked,
    #[error("preparation worker stopped before publishing its initial project")]
    StartupDisconnected,
}

/// A native owner for the initial project and its long-lived cache/preparation worker.
pub struct NativeProjectPreparationService {
    shared: Arc<Shared>,
    initial_project: Arc<PreparedProject>,
    control_port_taken: bool,
    join: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for NativeProjectPreparationService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeProjectPreparationService")
            .field("control_port_taken", &self.control_port_taken)
            .field("worker_joinable", &self.join.is_some())
            .finish_non_exhaustive()
    }
}

impl NativeProjectPreparationService {
    /// Opens one cache owner and synchronously prepares the initial source on the worker thread.
    ///
    /// The initial load is startup work; callers should not invoke this from a UI callback.
    pub fn start(
        options: NativeProjectPreparationOptions,
    ) -> Result<Self, NativeProjectPreparationStartError> {
        options.validate()?;
        #[cfg(target_family = "wasm")]
        {
            let _ = options;
            Err(NativeProjectPreparationStartError::NativeUnavailable)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            Self::start_native(options)
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn start_native(
        options: NativeProjectPreparationOptions,
    ) -> Result<Self, NativeProjectPreparationStartError> {
        let shared = Arc::new(Shared::new());
        let worker_shared = Arc::clone(&shared);
        let panic_shared = Arc::clone(&shared);
        let worker_options = options.clone();
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("daedalus-project-preparation".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_main(worker_shared, worker_options, startup_tx)
                }));
                if let Err(payload) = result {
                    // Record bounded failure status, then preserve the panic for JoinHandle.
                    panic_shared.mark_panicked();
                    std::panic::resume_unwind(payload);
                }
            })
            .map_err(|error| NativeProjectPreparationStartError::ThreadSpawn(error.to_string()))?;

        let initial_project = match startup_rx.recv() {
            Ok(Ok(project)) => project,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(_) => {
                let panic_recorded = shared.worker_panicked.load(Ordering::Acquire);
                let join_panicked = join.join().is_err();
                return if panic_recorded || join_panicked {
                    shared.mark_panic_joined();
                    Err(NativeProjectPreparationStartError::WorkerPanicked)
                } else {
                    Err(NativeProjectPreparationStartError::StartupDisconnected)
                };
            }
        };

        Ok(Self {
            shared,
            initial_project,
            control_port_taken: false,
            join: Some(join),
        })
    }

    /// Returns the immutable tick-zero project prepared by the worker.
    pub fn initial_project(&self) -> &Arc<PreparedProject> {
        &self.initial_project
    }

    /// Transfers the single opaque control capability to one session actor.
    pub fn take_control_port(
        &mut self,
    ) -> Result<ProjectPreparationPort, ProjectPreparationPortError> {
        if self.control_port_taken {
            return Err(ProjectPreparationPortError::AlreadyTaken);
        }
        self.control_port_taken = true;
        Ok(ProjectPreparationPort {
            shared: Arc::clone(&self.shared),
            initial_project: Arc::clone(&self.initial_project),
            bound: false,
        })
    }

    /// Requests worker shutdown without waiting for filesystem/parser work to return.
    pub fn request_stop(&self) {
        self.shared.request_stop();
    }

    /// Installs a deterministic worker pause for sibling actor tests.
    #[cfg(all(test, feature = "audio"))]
    pub(crate) fn block_preparation_for_test(&self, epoch: u64) -> PreparationTestGateRelease {
        let gate = Arc::new(PreparationTestGate::new(epoch));
        self.shared.install_test_gate(Arc::clone(&gate));
        PreparationTestGateRelease(gate)
    }

    /// Requests stop and joins only after the worker has already finished.
    pub fn try_shutdown(
        &mut self,
    ) -> Result<PreparationWorkerShutdown, PreparationWorkerShutdownError> {
        self.request_stop();
        let Some(join) = self.join.as_ref() else {
            return if self.shared.worker_panicked.load(Ordering::Acquire) {
                Err(PreparationWorkerShutdownError::WorkerPanicked)
            } else {
                Ok(PreparationWorkerShutdown::Stopped)
            };
        };
        if !join.is_finished() {
            return Ok(PreparationWorkerShutdown::Stopping);
        }
        let join = self.join.take().expect("worker handle checked above");
        let join_panicked = join.join().is_err();
        if self.shared.worker_panicked.load(Ordering::Acquire) {
            self.shared.mark_panic_joined();
            return Err(PreparationWorkerShutdownError::WorkerPanicked);
        }
        if join_panicked {
            self.shared.mark_panic_joined();
            return Err(PreparationWorkerShutdownError::WorkerPanicked);
        }
        Ok(PreparationWorkerShutdown::Stopped)
    }
}

impl Drop for NativeProjectPreparationService {
    fn drop(&mut self) {
        self.request_stop();
        // Dropping JoinHandle detaches. The worker closure keeps the cache owner alive until its
        // actual return; Drop never waits on the actor/UI thread.
    }
}

/// Nonblocking result of observing native worker shutdown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationWorkerShutdown {
    Stopping,
    Stopped,
}

/// Failure observed while joining a finished preparation worker.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PreparationWorkerShutdownError {
    #[error("project preparation worker panicked")]
    WorkerPanicked,
}

/// Opaque one-shot capability the common session actor uses to control preparation.
///
/// This value cannot be cloned. Its mutation and status methods are crate-private so callers
/// cannot bypass session command replay or capability checks.
pub struct ProjectPreparationPort {
    shared: Arc<Shared>,
    initial_project: Arc<PreparedProject>,
    bound: bool,
}

impl std::fmt::Debug for ProjectPreparationPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectPreparationPort")
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

impl ProjectPreparationPort {
    pub(crate) fn matches_initial_project(&self, project: &Arc<PreparedProject>) -> bool {
        Arc::ptr_eq(&self.initial_project, project)
    }

    pub(crate) fn bind_session(
        &mut self,
        session: SessionId,
    ) -> Result<(), ProjectPreparationPortError> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| ProjectPreparationPortError::Stopped)?;
        if self.bound || state.bound_session.is_some() {
            return Err(ProjectPreparationPortError::AlreadyBound);
        }
        if state.stopped || state.stopping || self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(ProjectPreparationPortError::Stopped);
        }
        state.bound_session = Some(session);
        self.bound = true;
        Ok(())
    }

    pub(crate) fn try_prepare(&self, epoch: u64) -> Result<(), ProjectPreparationPortError> {
        if epoch == 0 {
            return Err(ProjectPreparationPortError::InvalidEpoch);
        }
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ProjectPreparationPortError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(ProjectPreparationPortError::Stopped),
        };
        if state.stopped || state.stopping || self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(ProjectPreparationPortError::Stopped);
        }
        if !self.bound || state.bound_session.is_none() {
            return Err(ProjectPreparationPortError::SessionMismatch);
        }
        if epoch <= state.last_issued_epoch {
            return Err(ProjectPreparationPortError::InvalidEpoch);
        }
        let remaining = state
            .max_catalog_accounted_bytes
            .checked_sub(state.active_cooked_accounted_bytes)
            .ok_or(ProjectPreparationPortError::Capacity)?;
        if remaining == 0 {
            return Err(ProjectPreparationPortError::Capacity);
        }

        state.last_issued_epoch = epoch;
        state.desired_epoch = Some(epoch);
        state.pending_epoch = Some(epoch);
        state.cancel_requested_epoch = None;
        state.state = PreparationState::Pending;
        self.shared.cancelled_epoch.store(0, Ordering::Release);
        self.shared.desired_epoch.store(epoch, Ordering::Release);
        drop(state);
        self.shared.changed.notify_all();
        Ok(())
    }

    pub(crate) fn try_cancel(&self, epoch: u64) -> Result<(), ProjectPreparationPortError> {
        if epoch == 0 {
            return Err(ProjectPreparationPortError::InvalidEpoch);
        }
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ProjectPreparationPortError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(ProjectPreparationPortError::Stopped),
        };
        if state.stopped || state.stopping || self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(ProjectPreparationPortError::Stopped);
        }
        if !self.bound || state.bound_session.is_none() {
            return Err(ProjectPreparationPortError::SessionMismatch);
        }
        if state.desired_epoch != Some(epoch) {
            return Err(ProjectPreparationPortError::NoActiveCandidate);
        }
        if state.running_epoch != Some(epoch)
            && state.pending_epoch != Some(epoch)
            && state.ready_epoch != Some(epoch)
        {
            return Err(ProjectPreparationPortError::NoActiveCandidate);
        }
        state.desired_epoch = None;
        if state.pending_epoch == Some(epoch) {
            state.pending_epoch = None;
        }
        state.cancel_requested_epoch = Some(epoch);
        state.state = PreparationState::CancelRequested;
        self.shared.desired_epoch.store(0, Ordering::Release);
        self.shared.cancelled_epoch.store(epoch, Ordering::Release);
        drop(state);
        self.shared.changed.notify_all();
        Ok(())
    }

    pub(crate) fn try_status(
        &self,
        session: SessionId,
    ) -> Result<AssetPreparationStatus, ProjectPreparationPortError> {
        let state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ProjectPreparationPortError::Busy),
            Err(TryLockError::Poisoned(poisoned)) => {
                self.shared.state.clear_poison();
                poisoned.into_inner()
            }
        };
        if !self.bound || state.bound_session != Some(session) {
            return Err(ProjectPreparationPortError::SessionMismatch);
        }
        self.shared.port_status_locked(&state, session)
    }

    pub(crate) fn request_stop(&self) {
        self.shared.request_stop();
    }
}

impl Drop for ProjectPreparationPort {
    fn drop(&mut self) {
        self.request_stop();
    }
}

/// Immediate result of a port operation. This error never consumes a caller command ID.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProjectPreparationPortError {
    #[error("preparation worker state is busy")]
    Busy,
    #[error("preparation worker is stopping or stopped")]
    Stopped,
    #[error("preparation status belongs to a different or unbound session")]
    SessionMismatch,
    #[error("candidate epoch is zero, expired, future, or reused")]
    InvalidEpoch,
    #[error("candidate has no active pending, running, or ready work")]
    NoActiveCandidate,
    #[error("active plus candidate cooked/catalog capacity is exhausted")]
    Capacity,
    #[error("the one-shot preparation control port was already taken")]
    AlreadyTaken,
    #[error("the preparation control port was already bound to a session")]
    AlreadyBound,
}

/// Session-scoped identity for one preparation epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreparationId {
    session: SessionId,
    epoch: u64,
}

impl PreparationId {
    pub const fn from_parts(session: SessionId, epoch: u64) -> Self {
        Self { session, epoch }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn epoch(self) -> u64 {
        self.epoch
    }
}

/// Bounded lifecycle state visible through the session status operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparationState {
    Idle,
    Pending,
    Preparing,
    CpuPrepared,
    CancelRequested,
    Cancelled,
    Superseded,
    Failed,
    Stopping,
    Stopped,
}

/// Small immutable summary of a privately held, cooked candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparationCandidateSummary {
    id: PreparationId,
    is_current: bool,
    source_digest: String,
    catalog_digest: String,
    source_scene_revision: u64,
    asset_count: usize,
    entity_count: usize,
    memory: AssetImportMemorySummary,
    metrics: AssetImportMetrics,
}

impl PreparationCandidateSummary {
    pub fn id(&self) -> PreparationId {
        self.id
    }

    pub fn is_current(&self) -> bool {
        self.is_current
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    pub fn source_scene_revision(&self) -> u64 {
        self.source_scene_revision
    }

    pub fn asset_count(&self) -> usize {
        self.asset_count
    }

    pub fn entity_count(&self) -> usize {
        self.entity_count
    }

    pub fn memory(&self) -> &AssetImportMemorySummary {
        &self.memory
    }

    pub fn metrics(&self) -> &AssetImportMetrics {
        &self.metrics
    }
}

/// One bounded, UTF-8-safe importer/cache diagnostic retained in status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparationFailure {
    id: PreparationId,
    code: String,
    message: String,
    source_id: Option<String>,
    path: Option<String>,
    diagnostic_truncated: bool,
}

impl PreparationFailure {
    fn from_import_error(epoch: u64, error: &AssetImportError) -> Self {
        let mut truncated = false;
        let code = bounded_text(
            error.code().as_str(),
            MAX_FAILURE_CODE_BYTES,
            &mut truncated,
        );
        let message = bounded_text(error.message(), MAX_FAILURE_MESSAGE_BYTES, &mut truncated);
        let source_id = error
            .source_id()
            .map(|value| bounded_text(value, MAX_FAILURE_SOURCE_ID_BYTES, &mut truncated));
        let path = error
            .path()
            .map(|value| bounded_text(value, MAX_FAILURE_PATH_BYTES, &mut truncated));
        Self {
            id: PreparationId::from_parts(SessionId::from_raw(0), epoch),
            code,
            message,
            source_id,
            path,
            diagnostic_truncated: truncated,
        }
    }

    pub fn id(&self) -> PreparationId {
        self.id
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn source_id(&self) -> Option<&str> {
        self.source_id.as_deref()
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn diagnostic_truncated(&self) -> bool {
        self.diagnostic_truncated
    }
}

/// Snapshot of preparation and active-project metadata; never contains project payloads.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetPreparationStatus {
    session: SessionId,
    active_generation: u64,
    active_source_digest: String,
    active_cooked_accounted_bytes: u64,
    desired: Option<PreparationId>,
    running: Option<PreparationId>,
    pending: Option<PreparationId>,
    state: PreparationState,
    ready_candidate: Option<PreparationCandidateSummary>,
    latest_failure: Option<PreparationFailure>,
}

impl AssetPreparationStatus {
    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }

    pub fn active_source_digest(&self) -> &str {
        &self.active_source_digest
    }

    pub fn active_cooked_accounted_bytes(&self) -> u64 {
        self.active_cooked_accounted_bytes
    }

    pub fn desired(&self) -> Option<PreparationId> {
        self.desired
    }

    pub fn running(&self) -> Option<PreparationId> {
        self.running
    }

    pub fn pending(&self) -> Option<PreparationId> {
        self.pending
    }

    pub fn state(&self) -> PreparationState {
        self.state
    }

    pub fn ready_candidate(&self) -> Option<&PreparationCandidateSummary> {
        self.ready_candidate.as_ref()
    }

    pub fn latest_failure(&self) -> Option<&PreparationFailure> {
        self.latest_failure.as_ref()
    }
}

/// A preparation mutation accepted by the common session actor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AssetPreparationOperation {
    Prepare {
        command_id: Option<CommandId>,
    },
    Cancel {
        command_id: Option<CommandId>,
        candidate: PreparationId,
    },
    Status,
}

/// Accepted command outcome; importer completion is reported later in status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetPreparationCommandOutcome {
    PrepareAccepted { candidate: PreparationId },
    CancelAccepted { candidate: PreparationId },
}

/// Terminal receipt for one accepted preparation command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPreparationCommandResult {
    command_id: CommandId,
    observed_tick: Tick,
    outcome: AssetPreparationCommandOutcome,
}

impl AssetPreparationCommandResult {
    pub(crate) const fn new(
        command_id: CommandId,
        observed_tick: Tick,
        outcome: AssetPreparationCommandOutcome,
    ) -> Self {
        Self {
            command_id,
            observed_tick,
            outcome,
        }
    }

    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub const fn observed_tick(&self) -> Tick {
        self.observed_tick
    }

    pub const fn outcome(&self) -> &AssetPreparationCommandOutcome {
        &self.outcome
    }
}

/// Result of a preparation status or mutation operation.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetPreparationResult {
    Command(AssetPreparationCommandResult),
    Status(Box<AssetPreparationStatus>),
}

#[derive(Clone, Debug)]
struct CandidateSummaryData {
    epoch: u64,
    source_digest: String,
    catalog_digest: String,
    source_scene_revision: u64,
    asset_count: usize,
    entity_count: usize,
    memory: AssetImportMemorySummary,
    metrics: AssetImportMetrics,
}

impl CandidateSummaryData {
    fn from_project(epoch: u64, project: &PreparedProject) -> Result<Self, AssetImportError> {
        let report = project.asset_import_report().ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::Internal,
                Default::default(),
                "asset-aware project preparation returned no memory summary",
            )
        })?;
        Ok(Self {
            epoch,
            source_digest: project.digest().to_owned(),
            catalog_digest: report.catalog_digest().to_owned(),
            source_scene_revision: project.scene().revision,
            // Count declared scene assets, including legacy mesh descriptors.
            asset_count: project.scene().assets.len(),
            entity_count: project.entities().len(),
            memory: report.memory().clone(),
            metrics: report.metrics().clone(),
        })
    }

    fn to_public(&self, session: SessionId, is_current: bool) -> PreparationCandidateSummary {
        PreparationCandidateSummary {
            id: PreparationId::from_parts(session, self.epoch),
            is_current,
            source_digest: self.source_digest.clone(),
            catalog_digest: self.catalog_digest.clone(),
            source_scene_revision: self.source_scene_revision,
            asset_count: self.asset_count,
            entity_count: self.entity_count,
            memory: self.memory.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct FailureData {
    epoch: u64,
    code: String,
    message: String,
    source_id: Option<String>,
    path: Option<String>,
    diagnostic_truncated: bool,
}

impl FailureData {
    fn from_import_error(epoch: u64, error: &AssetImportError) -> Self {
        let failure = PreparationFailure::from_import_error(epoch, error);
        Self {
            epoch,
            code: failure.code,
            message: failure.message,
            source_id: failure.source_id,
            path: failure.path,
            diagnostic_truncated: failure.diagnostic_truncated,
        }
    }

    fn to_public(&self, session: SessionId) -> PreparationFailure {
        PreparationFailure {
            id: PreparationId::from_parts(session, self.epoch),
            code: self.code.clone(),
            message: self.message.clone(),
            source_id: self.source_id.clone(),
            path: self.path.clone(),
            diagnostic_truncated: self.diagnostic_truncated,
        }
    }
}

fn bounded_text(value: &str, max_bytes: usize, truncated: &mut bool) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    *truncated = true;
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
static PREPARATION_TEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(test)]
const PREPARATION_TEST_SCENE: &[u8] = br#"{"schema_version":1,"revision":7,"entities":[],"assets":[{"id":"tone","kind":"daedalus.audio","uri":"audio/tone.wav","metadata":{"schema_version":1}}]}"#;

/// Reusable imported-WAV project fixture for sibling runtime actor tests.
#[cfg(test)]
pub(crate) struct PreparationTestProject(PathBuf);

#[cfg(test)]
impl PreparationTestProject {
    pub(crate) fn with_audio() -> Self {
        let id = PREPARATION_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("daedalus-preparation-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("audio")).unwrap();
        std::fs::write(
            root.join("daedalus.project.json"),
            br#"{"schema_version":1,"name":"preparation-test","scene":"scene.json"}"#,
        )
        .unwrap();
        std::fs::write(root.join("scene.json"), PREPARATION_TEST_SCENE).unwrap();
        std::fs::write(root.join("audio/tone.wav"), preparation_test_wav()).unwrap();
        Self(root)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for PreparationTestProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
fn preparation_test_wav() -> Vec<u8> {
    let samples = [0_i16, 8192, -8192, 16384, -16384, 0, 4096, -4096];
    let data_bytes = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_000_u32.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PreparationTestGatePhase {
    BeforeImport,
    AfterImport,
}

#[cfg(test)]
struct PreparationTestGate {
    epoch: u64,
    phase: PreparationTestGatePhase,
    panic_after_release: bool,
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl PreparationTestGate {
    fn new(epoch: u64) -> Self {
        Self {
            epoch,
            phase: PreparationTestGatePhase::BeforeImport,
            panic_after_release: false,
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn after_import(epoch: u64) -> Self {
        Self {
            epoch,
            phase: PreparationTestGatePhase::AfterImport,
            panic_after_release: false,
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn panicking(epoch: u64) -> Self {
        Self {
            epoch,
            phase: PreparationTestGatePhase::BeforeImport,
            panic_after_release: true,
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, (bool, bool)> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.state.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    fn worker_wait(&self) -> bool {
        let mut state = self.lock_state();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = match self.changed.wait(state) {
                Ok(state) => state,
                Err(poisoned) => {
                    self.state.clear_poison();
                    poisoned.into_inner()
                }
            };
        }
        self.panic_after_release
    }

    fn wait_until_entered(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut state = self.lock_state();
        while !state.0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                drop(state);
                panic!("worker did not reach preparation test gate");
            }
            let (next, timeout) = match self.changed.wait_timeout(state, remaining) {
                Ok(result) => result,
                Err(poisoned) => {
                    self.state.clear_poison();
                    poisoned.into_inner()
                }
            };
            state = next;
            if timeout.timed_out() && !state.0 {
                drop(state);
                panic!("worker did not reach preparation test gate");
            }
        }
    }

    fn release(&self) {
        let mut state = self.lock_state();
        state.1 = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
pub(crate) struct PreparationTestGateRelease(Arc<PreparationTestGate>);

#[cfg(all(test, feature = "audio"))]
impl PreparationTestGateRelease {
    pub(crate) fn wait_until_entered(&self) {
        self.0.wait_until_entered();
    }

    pub(crate) fn release(&self) {
        self.0.release();
    }
}

#[cfg(test)]
impl Drop for PreparationTestGateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Shared {
    state: Mutex<WorkerState>,
    changed: Condvar,
    stop_requested: AtomicBool,
    worker_panicked: AtomicBool,
    desired_epoch: AtomicU64,
    cancelled_epoch: AtomicU64,
    #[cfg(test)]
    test_gate: Mutex<Option<Arc<PreparationTestGate>>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: Mutex::new(WorkerState::starting()),
            changed: Condvar::new(),
            stop_requested: AtomicBool::new(false),
            worker_panicked: AtomicBool::new(false),
            desired_epoch: AtomicU64::new(0),
            cancelled_epoch: AtomicU64::new(0),
            #[cfg(test)]
            test_gate: Mutex::new(None),
        }
    }

    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.desired_epoch.store(0, Ordering::Release);
        self.changed.notify_all();
    }

    fn port_status_locked(
        &self,
        state: &WorkerState,
        session: SessionId,
    ) -> Result<AssetPreparationStatus, ProjectPreparationPortError> {
        if state.bound_session != Some(session) {
            return Err(ProjectPreparationPortError::SessionMismatch);
        }
        let stopping = self.stop_requested.load(Ordering::Acquire);
        let desired = (!stopping)
            .then_some(state.desired_epoch)
            .flatten()
            .map(|epoch| PreparationId::from_parts(session, epoch));
        let running = state
            .running_epoch
            .map(|epoch| PreparationId::from_parts(session, epoch));
        let pending = state
            .pending_epoch
            .map(|epoch| PreparationId::from_parts(session, epoch));
        let ready_candidate = state.ready_summary.as_ref().map(|ready| {
            ready.to_public(
                session,
                !stopping
                    && state.desired_epoch == Some(ready.epoch)
                    && state.cancel_requested_epoch != Some(ready.epoch),
            )
        });
        let latest_failure = state
            .latest_failure
            .as_ref()
            .map(|failure| failure.to_public(session));
        Ok(AssetPreparationStatus {
            session,
            active_generation: 1,
            active_source_digest: state.active_source_digest.clone(),
            active_cooked_accounted_bytes: state.active_cooked_accounted_bytes,
            desired,
            running,
            pending,
            state: if state.stopped {
                PreparationState::Stopped
            } else if stopping {
                PreparationState::Stopping
            } else {
                state.state
            },
            ready_candidate,
            latest_failure,
        })
    }

    #[cfg(test)]
    fn install_test_gate(&self, gate: Arc<PreparationTestGate>) {
        *self.test_gate.lock().unwrap() = Some(gate);
    }

    #[cfg(test)]
    fn wait_at_test_gate(&self, epoch: u64, phase: PreparationTestGatePhase) {
        let gate = self.test_gate.lock().unwrap().clone();
        let Some(gate) = gate.filter(|gate| gate.epoch == epoch && gate.phase == phase) else {
            return;
        };
        if gate.worker_wait() {
            panic!("injected preparation worker panic");
        }
    }

    #[cfg(not(test))]
    fn wait_at_test_gate(&self, _epoch: u64, _phase: PreparationTestGatePhase) {}

    fn mark_panicked(&self) {
        self.worker_panicked.store(true, Ordering::Release);
        self.stop_requested.store(true, Ordering::Release);
        self.desired_epoch.store(0, Ordering::Release);
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.state.clear_poison();
                poisoned.into_inner()
            }
        };
        let failure_epoch = state
            .running_epoch
            .or(state.desired_epoch)
            .or(state.ready_epoch);
        state.running_epoch = None;
        state.pending_epoch = None;
        state.ready_epoch = None;
        state.ready_summary = None;
        state.cancel_requested_epoch = None;
        state.stopping = true;
        state.stopped = false;
        state.state = PreparationState::Stopping;
        if let Some(epoch) = failure_epoch {
            state.latest_failure = Some(FailureData {
                epoch,
                code: "worker_panic".to_owned(),
                message: "project preparation worker panicked".to_owned(),
                source_id: None,
                path: None,
                diagnostic_truncated: false,
            });
        }
        drop(state);
        self.changed.notify_all();
    }

    fn mark_panic_joined(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.state.clear_poison();
                poisoned.into_inner()
            }
        };
        state.running_epoch = None;
        state.pending_epoch = None;
        state.ready_epoch = None;
        state.ready_summary = None;
        state.cancel_requested_epoch = None;
        state.stopping = false;
        state.stopped = true;
        state.state = PreparationState::Stopped;
        drop(state);
        self.changed.notify_all();
    }
}

struct WorkerState {
    max_catalog_accounted_bytes: u64,
    active_source_digest: String,
    active_cooked_accounted_bytes: u64,
    bound_session: Option<SessionId>,
    last_issued_epoch: u64,
    desired_epoch: Option<u64>,
    running_epoch: Option<u64>,
    pending_epoch: Option<u64>,
    ready_epoch: Option<u64>,
    ready_summary: Option<CandidateSummaryData>,
    cancel_requested_epoch: Option<u64>,
    latest_failure: Option<FailureData>,
    state: PreparationState,
    stopping: bool,
    stopped: bool,
}

impl WorkerState {
    fn starting() -> Self {
        Self {
            max_catalog_accounted_bytes: 0,
            active_source_digest: String::new(),
            active_cooked_accounted_bytes: 0,
            bound_session: None,
            last_issued_epoch: 0,
            desired_epoch: None,
            running_epoch: None,
            pending_epoch: None,
            ready_epoch: None,
            ready_summary: None,
            cancel_requested_epoch: None,
            latest_failure: None,
            state: PreparationState::Idle,
            stopping: false,
            stopped: false,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn worker_main(
    shared: Arc<Shared>,
    options: NativeProjectPreparationOptions,
    startup: std::sync::mpsc::SyncSender<
        Result<Arc<PreparedProject>, NativeProjectPreparationStartError>,
    >,
) {
    // Validate the project root before FileCookCache can create the default nested cache path.
    if let Err(error) = NativeProjectResolver::open(&options.root) {
        let _ = startup.send(Err(NativeProjectPreparationStartError::Import(error)));
        return;
    }
    let mut cache = match FileCookCache::open(&options.cache_root, options.cache_limits) {
        Ok(cache) => cache,
        Err(error) => {
            let _ = startup.send(Err(NativeProjectPreparationStartError::Import(error)));
            return;
        }
    };
    let mut initial_limits = options.import_limits;
    initial_limits.max_cooked_bytes = initial_limits
        .max_cooked_bytes
        .min(options.max_catalog_accounted_bytes);
    let initial = match load_project_with_assets(
        &options.root,
        &mut cache,
        initial_limits,
        &ImportCancellation::never(),
    ) {
        Ok(project) => project,
        Err(error) => {
            let _ = startup.send(Err(NativeProjectPreparationStartError::Import(error)));
            return;
        }
    };
    let Some(report) = initial.asset_import_report() else {
        let _ = startup.send(Err(
            NativeProjectPreparationStartError::MissingMemorySummary,
        ));
        return;
    };
    let active_bytes = report.memory().cooked_accounted_bytes();
    if active_bytes > options.max_catalog_accounted_bytes {
        let _ = startup.send(Err(NativeProjectPreparationStartError::Capacity));
        return;
    }
    let initial = Arc::new(initial.release_asset_cache_pins());
    if let Ok(mut state) = shared.state.lock() {
        state.max_catalog_accounted_bytes = options.max_catalog_accounted_bytes;
        state.active_source_digest = initial.digest().to_owned();
        state.active_cooked_accounted_bytes = active_bytes;
        state.state = PreparationState::Idle;
    }
    shared.changed.notify_all();
    if startup.send(Ok(initial)).is_err() {
        shared.request_stop();
        worker_loop(shared, options, cache);
        return;
    }
    worker_loop(shared, options, cache);
}

#[cfg(not(target_family = "wasm"))]
enum WorkerAction {
    Prepare(u64),
    DropReady(u64),
    Stop,
}

#[cfg(not(target_family = "wasm"))]
fn worker_loop(
    shared: Arc<Shared>,
    options: NativeProjectPreparationOptions,
    mut cache: FileCookCache,
) {
    // Candidate payloads live only on this thread; shared status holds bounded metadata only.
    let mut ready: Option<(u64, PreparedProject)> = None;
    loop {
        let action = {
            let mut state = match shared.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            loop {
                if state.stopping || shared.stop_requested.load(Ordering::Acquire) {
                    state.stopping = true;
                    state.state = PreparationState::Stopping;
                    shared.changed.notify_all();
                    break WorkerAction::Stop;
                }
                if let Some(epoch) = state.pending_epoch {
                    if let Some((ready_epoch, _)) = ready.as_ref() {
                        break WorkerAction::DropReady(*ready_epoch);
                    }
                    state.pending_epoch = None;
                    state.running_epoch = Some(epoch);
                    state.state = PreparationState::Preparing;
                    shared.changed.notify_all();
                    break WorkerAction::Prepare(epoch);
                }
                if let Some(epoch) = state.cancel_requested_epoch
                    && state.running_epoch.is_none()
                {
                    if ready
                        .as_ref()
                        .is_some_and(|(ready_epoch, _)| *ready_epoch == epoch)
                    {
                        break WorkerAction::DropReady(epoch);
                    }
                    state.cancel_requested_epoch = None;
                    state.state = PreparationState::Cancelled;
                    shared.changed.notify_all();
                }
                // `request_stop` is intentionally lock-free. The timeout closes the small race
                // where its Condvar notification arrives just before this wait begins.
                state = match shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(50))
                {
                    Ok((state, _)) => state,
                    Err(_) => return,
                };
            }
        };

        match action {
            WorkerAction::Stop => {
                // Destruction of candidate buffers and cache ownership precedes Stopped.
                drop(ready.take());
                drop(cache);
                if let Ok(mut state) = shared.state.lock() {
                    state.running_epoch = None;
                    state.pending_epoch = None;
                    state.ready_epoch = None;
                    state.ready_summary = None;
                    state.cancel_requested_epoch = None;
                    state.stopping = false;
                    state.stopped = true;
                    state.state = PreparationState::Stopped;
                }
                shared.changed.notify_all();
                return;
            }
            WorkerAction::DropReady(epoch) => {
                if ready
                    .as_ref()
                    .is_some_and(|(ready_epoch, _)| *ready_epoch == epoch)
                {
                    drop(ready.take());
                }
                if let Ok(mut state) = shared.state.lock() {
                    if state.ready_epoch == Some(epoch) {
                        state.ready_epoch = None;
                        state.ready_summary = None;
                    }
                    if state.cancel_requested_epoch == Some(epoch) {
                        state.cancel_requested_epoch = None;
                        if state.pending_epoch.is_none() && !state.stopping {
                            state.state = PreparationState::Cancelled;
                        }
                    }
                }
                shared.changed.notify_all();
            }
            WorkerAction::Prepare(epoch) => {
                shared.wait_at_test_gate(epoch, PreparationTestGatePhase::BeforeImport);
                let remaining = match shared.state.lock() {
                    Ok(state) => state
                        .max_catalog_accounted_bytes
                        .checked_sub(state.active_cooked_accounted_bytes),
                    Err(_) => return,
                };
                let Some(remaining) = remaining.filter(|remaining| *remaining > 0) else {
                    finish_import_error(
                        &shared,
                        epoch,
                        AssetImportError::new(
                            AssetImportErrorCode::LimitExceeded,
                            Default::default(),
                            "active plus candidate cooked/catalog capacity is exhausted",
                        ),
                    );
                    continue;
                };
                let mut limits = options.import_limits;
                limits.max_cooked_bytes = limits.max_cooked_bytes.min(remaining);
                let cancel_shared = Arc::clone(&shared);
                let cancellation = ImportCancellation::from_predicate(move || {
                    cancel_shared.stop_requested.load(Ordering::Acquire)
                        || cancel_shared.desired_epoch.load(Ordering::Acquire) != epoch
                        || cancel_shared.cancelled_epoch.load(Ordering::Acquire) == epoch
                });
                let result =
                    load_project_with_assets(&options.root, &mut cache, limits, &cancellation);
                shared.wait_at_test_gate(epoch, PreparationTestGatePhase::AfterImport);
                match result {
                    Ok(project) => {
                        let cooked_bytes = project
                            .asset_import_report()
                            .map(|report| report.memory().cooked_accounted_bytes());
                        match cooked_bytes {
                            Some(cooked_bytes) if cooked_bytes <= remaining => {
                                if let Some(project) = finish_candidate(&shared, epoch, project) {
                                    ready = Some((epoch, project));
                                }
                            }
                            Some(_) => {
                                drop(project);
                                finish_import_error(
                                    &shared,
                                    epoch,
                                    AssetImportError::new(
                                        AssetImportErrorCode::LimitExceeded,
                                        Default::default(),
                                        "candidate exceeds its remaining active plus candidate catalog capacity",
                                    ),
                                );
                            }
                            None => {
                                drop(project);
                                finish_import_error(
                                    &shared,
                                    epoch,
                                    AssetImportError::new(
                                        AssetImportErrorCode::Internal,
                                        Default::default(),
                                        "asset-aware project preparation returned no memory summary",
                                    ),
                                );
                            }
                        }
                    }
                    Err(error) => finish_import_error(&shared, epoch, error),
                }
            }
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn finish_candidate(
    shared: &Arc<Shared>,
    epoch: u64,
    project: PreparedProject,
) -> Option<PreparedProject> {
    let summary = match CandidateSummaryData::from_project(epoch, &project) {
        Ok(summary) => summary,
        Err(error) => {
            drop(project);
            finish_import_error(shared, epoch, error);
            return None;
        }
    };
    let mut state = match shared.state.lock() {
        Ok(state) => state,
        Err(_) => {
            drop(project);
            return None;
        }
    };
    let cancelled = shared.stop_requested.load(Ordering::Acquire)
        || shared.desired_epoch.load(Ordering::Acquire) != epoch
        || shared.cancelled_epoch.load(Ordering::Acquire) == epoch;
    if cancelled {
        drop(state);
        drop(project);
        if let Ok(mut state) = shared.state.lock() {
            if state.running_epoch == Some(epoch) {
                state.running_epoch = None;
            }
            let explicit_cancel = shared.cancelled_epoch.load(Ordering::Acquire) == epoch;
            if !shared.stop_requested.load(Ordering::Acquire)
                && state.pending_epoch.is_none()
                && !state.stopping
            {
                state.state = if explicit_cancel {
                    PreparationState::Cancelled
                } else {
                    PreparationState::Superseded
                };
            }
        }
        shared.changed.notify_all();
        return None;
    }
    state.running_epoch = None;
    state.ready_epoch = Some(epoch);
    state.ready_summary = Some(summary);
    state.cancel_requested_epoch = None;
    state.state = PreparationState::CpuPrepared;
    state.latest_failure = None;
    shared.changed.notify_all();
    drop(state);
    Some(project)
}

#[cfg(not(target_family = "wasm"))]
fn finish_import_error(shared: &Arc<Shared>, epoch: u64, error: AssetImportError) {
    let mut state = match shared.state.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    state.running_epoch = None;
    let stop_requested = shared.stop_requested.load(Ordering::Acquire);
    let explicit_cancel = shared.cancelled_epoch.load(Ordering::Acquire) == epoch;
    let superseded = shared.desired_epoch.load(Ordering::Acquire) != epoch;
    if !stop_requested && !explicit_cancel && !superseded {
        state.latest_failure = Some(FailureData::from_import_error(epoch, &error));
        state.state = PreparationState::Failed;
    } else if !state.stopping && state.pending_epoch.is_none() {
        state.state = if explicit_cancel {
            PreparationState::Cancelled
        } else {
            PreparationState::Superseded
        };
    }
    shared.changed.notify_all();
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    type TempProject = PreparationTestProject;

    fn wait_for_state(
        service: &NativeProjectPreparationService,
        session: SessionId,
        expected: impl Fn(&AssetPreparationStatus) -> bool,
    ) -> AssetPreparationStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = service.shared.state.lock().unwrap();
        loop {
            let status = service.shared.port_status_locked(&state, session).unwrap();
            if expected(&status) {
                return status;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "preparation worker did not reach expected state"
            );
            let (next, timeout) = service
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap();
            state = next;
            assert!(
                !timeout.timed_out(),
                "preparation worker did not reach expected state"
            );
        }
    }

    fn stop_worker(service: &mut NativeProjectPreparationService) {
        service.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => return,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }
    }

    #[test]
    fn options_reject_zero_or_over_hard_catalog_budget_before_worker_start() {
        let project = TempProject::with_audio();
        for value in [0, DEFAULT_MAX_CATALOG_ACCOUNTED_BYTES + 1] {
            let options = NativeProjectPreparationOptions::new(project.path())
                .with_max_catalog_accounted_bytes(value);
            assert!(matches!(
                NativeProjectPreparationService::start(options),
                Err(NativeProjectPreparationStartError::InvalidOptions(_))
            ));
            assert!(!project.path().join(".daedalus").exists());
        }
    }

    #[test]
    fn invalid_roots_fail_before_the_default_cache_directory_is_created() {
        let missing = std::env::temp_dir().join(format!(
            "daedalus-preparation-missing-{}-{}",
            std::process::id(),
            PREPARATION_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(matches!(
            NativeProjectPreparationService::start(NativeProjectPreparationOptions::new(&missing)),
            Err(NativeProjectPreparationStartError::Import(_))
        ));
        assert!(!missing.exists());

        let root = TempProject::with_audio();
        let file_root = root.path().join("not-a-project-directory");
        std::fs::write(&file_root, b"regular file").unwrap();
        assert!(matches!(
            NativeProjectPreparationService::start(NativeProjectPreparationOptions::new(
                &file_root
            )),
            Err(NativeProjectPreparationStartError::Import(_))
        ));
        assert!(file_root.is_file());
        assert!(!file_root.join(".daedalus").exists());
    }

    #[test]
    fn stop_request_is_nonblocking_while_worker_metadata_is_locked() {
        let project = TempProject::with_audio();
        let service = NativeProjectPreparationService::start(NativeProjectPreparationOptions::new(
            project.path(),
        ))
        .unwrap();
        let state = service.shared.state.lock().unwrap();
        service.request_stop();
        assert!(service.shared.stop_requested.load(Ordering::Acquire));
        drop(state);
        let mut service = service;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }
    }

    #[test]
    fn worker_panic_keeps_typed_status_and_is_observed_by_join() {
        let project = TempProject::with_audio();
        let mut service = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(project.path()),
        )
        .unwrap();
        let session = SessionId::fresh();
        let mut port = service.take_control_port().unwrap();
        port.bind_session(session).unwrap();
        let gate = Arc::new(PreparationTestGate::panicking(1));
        service.shared.install_test_gate(Arc::clone(&gate));
        let _release_gate = PreparationTestGateRelease(Arc::clone(&gate));
        port.try_prepare(1).unwrap();
        gate.wait_until_entered();
        gate.release();

        let failure = wait_for_state(&service, session, |status| {
            status.state() == PreparationState::Stopping
                && status
                    .latest_failure()
                    .is_some_and(|failure| failure.code() == "worker_panic")
        });
        assert_eq!(failure.running(), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown() {
                Err(PreparationWorkerShutdownError::WorkerPanicked) => break,
                Ok(PreparationWorkerShutdown::Stopping) => {
                    assert!(Instant::now() < deadline, "panicked worker join timed out");
                    thread::yield_now();
                }
                other => panic!("unexpected worker shutdown result: {other:?}"),
            }
        }
        let stopped = port.try_status(session).unwrap();
        assert_eq!(stopped.state(), PreparationState::Stopped);
        assert_eq!(
            stopped.latest_failure().map(PreparationFailure::code),
            Some("worker_panic")
        );
        let reopened = FileCookCache::open(
            project.path().join(".daedalus/cache"),
            CacheLimits::default(),
        )
        .expect("panic unwinding and join release cache owner");
        drop(reopened);
    }

    #[test]
    fn zero_remaining_catalog_capacity_rejects_prepare_without_consuming_epoch() {
        let project = TempProject::with_audio();
        let first = NativeProjectPreparationService::start(NativeProjectPreparationOptions::new(
            project.path(),
        ))
        .unwrap();
        let active_bytes = first
            .initial_project()
            .asset_import_report()
            .unwrap()
            .memory()
            .cooked_accounted_bytes();
        let mut first = first;
        first.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match first.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }

        let mut service = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(project.path())
                .with_max_catalog_accounted_bytes(active_bytes),
        )
        .unwrap();
        let mut port = service.take_control_port().unwrap();
        let session = SessionId::fresh();
        port.bind_session(session).unwrap();
        assert_eq!(
            port.try_prepare(1),
            Err(ProjectPreparationPortError::Capacity)
        );
        let status = port.try_status(session).unwrap();
        assert!(status.desired().is_none());
        assert!(status.running().is_none());
        assert!(status.pending().is_none());
        drop(port);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }
    }

    #[test]
    fn native_worker_reuses_one_cache_coalesces_to_latest_and_releases_owner_on_shutdown() {
        let project = TempProject::with_audio();
        let options = NativeProjectPreparationOptions::new(project.path());
        let mut service = NativeProjectPreparationService::start(options).unwrap();
        let active = Arc::clone(service.initial_project());
        let active_digest = active.digest().to_owned();
        let session = SessionId::fresh();
        let mut port = service.take_control_port().unwrap();
        assert!(port.matches_initial_project(&active));
        port.bind_session(session).unwrap();
        let initial = port.try_status(session).unwrap();
        assert_eq!(initial.state(), PreparationState::Idle);
        assert_eq!(initial.active_generation(), 1);
        assert_eq!(initial.active_source_digest(), active_digest);
        assert!(initial.desired().is_none());
        assert!(initial.ready_candidate().is_none());

        assert!(
            FileCookCache::open(
                project.path().join(".daedalus/cache"),
                CacheLimits::default()
            )
            .is_err()
        );

        port.try_prepare(1).unwrap();
        let first = wait_for_state(&service, session, |status| {
            status
                .ready_candidate()
                .is_some_and(|candidate| candidate.id().epoch() == 1)
        });
        let first_candidate = first.ready_candidate().unwrap();
        assert!(first_candidate.is_current());
        assert_eq!(first_candidate.source_scene_revision(), 7);
        assert_eq!(first_candidate.asset_count(), 1);
        assert!(first_candidate.metrics().cache_hits >= 1);

        let gate = Arc::new(PreparationTestGate::new(2));
        service.shared.install_test_gate(Arc::clone(&gate));
        let _release_gate = PreparationTestGateRelease(Arc::clone(&gate));
        port.try_prepare(2).unwrap();
        gate.wait_until_entered();
        port.try_prepare(3).unwrap();
        port.try_prepare(4).unwrap();
        let coalesced = port.try_status(session).unwrap();
        assert_eq!(coalesced.running().map(PreparationId::epoch), Some(2));
        assert_eq!(coalesced.pending().map(PreparationId::epoch), Some(4));
        gate.release();
        let latest = wait_for_state(&service, session, |status| {
            status
                .ready_candidate()
                .is_some_and(|candidate| candidate.id().epoch() == 4)
        });
        let latest_candidate = latest.ready_candidate().unwrap();
        assert!(latest_candidate.is_current());
        assert_eq!(latest_candidate.source_digest(), active_digest);
        assert!(latest_candidate.metrics().cache_hits >= 1);
        assert_eq!(latest.running(), None);
        assert_eq!(latest.pending(), None);

        port.try_cancel(4).unwrap();
        let cancelled = wait_for_state(&service, session, |status| {
            status.state() == PreparationState::Cancelled && status.ready_candidate().is_none()
        });
        assert!(cancelled.desired().is_none());

        port.request_stop();
        drop(port);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }
        let reopened = FileCookCache::open(
            project.path().join(".daedalus/cache"),
            CacheLimits::default(),
        )
        .expect("worker releases its single cache owner after join");
        drop(reopened);
    }

    #[test]
    fn completed_import_success_is_discarded_when_cancelled_and_superseded_before_publish() {
        let project = TempProject::with_audio();
        let mut service = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(project.path()),
        )
        .unwrap();
        let session = SessionId::fresh();
        let mut port = service.take_control_port().unwrap();
        port.bind_session(session).unwrap();
        let gate = Arc::new(PreparationTestGate::after_import(1));
        service.shared.install_test_gate(Arc::clone(&gate));
        let _release_gate = PreparationTestGateRelease(Arc::clone(&gate));

        port.try_prepare(1).unwrap();
        gate.wait_until_entered();
        let imported_but_unpublished = port.try_status(session).unwrap();
        assert_eq!(
            imported_but_unpublished.running().map(PreparationId::epoch),
            Some(1)
        );
        assert!(imported_but_unpublished.ready_candidate().is_none());

        port.try_cancel(1).unwrap();
        assert_eq!(
            port.try_status(session).unwrap().state(),
            PreparationState::CancelRequested
        );
        port.try_prepare(2).unwrap();
        gate.release();

        let latest = wait_for_state(&service, session, |status| {
            status
                .ready_candidate()
                .is_some_and(|candidate| candidate.id().epoch() == 2)
        });
        let candidate = latest.ready_candidate().unwrap();
        assert!(candidate.is_current());
        assert_eq!(candidate.id().epoch(), 2);
        assert!(latest.latest_failure().is_none());
        assert_eq!(latest.running(), None);
        assert_eq!(latest.pending(), None);

        drop(port);
        stop_worker(&mut service);
    }

    #[test]
    fn completed_import_failure_is_not_published_after_cancel_and_new_desired_epoch() {
        let project = TempProject::with_audio();
        let mut service = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(project.path()),
        )
        .unwrap();
        let session = SessionId::fresh();
        let mut port = service.take_control_port().unwrap();
        port.bind_session(session).unwrap();
        std::fs::write(project.path().join("scene.json"), b"{").unwrap();

        let gate = Arc::new(PreparationTestGate::after_import(1));
        service.shared.install_test_gate(Arc::clone(&gate));
        let _release_gate = PreparationTestGateRelease(Arc::clone(&gate));
        port.try_prepare(1).unwrap();
        gate.wait_until_entered();
        let imported_failure_unpublished = port.try_status(session).unwrap();
        assert_eq!(
            imported_failure_unpublished
                .running()
                .map(PreparationId::epoch),
            Some(1)
        );
        assert!(imported_failure_unpublished.latest_failure().is_none());

        std::fs::write(project.path().join("scene.json"), PREPARATION_TEST_SCENE).unwrap();
        port.try_cancel(1).unwrap();
        port.try_prepare(2).unwrap();
        gate.release();

        let latest = wait_for_state(&service, session, |status| {
            status
                .ready_candidate()
                .is_some_and(|candidate| candidate.id().epoch() == 2)
        });
        let candidate = latest.ready_candidate().unwrap();
        assert!(candidate.is_current());
        assert_eq!(candidate.id().epoch(), 2);
        assert!(latest.latest_failure().is_none());
        assert_eq!(latest.running(), None);
        assert_eq!(latest.pending(), None);

        drop(port);
        stop_worker(&mut service);
    }

    #[test]
    fn port_rejects_invalid_epochs_and_session_mismatch_without_effects() {
        let project = TempProject::with_audio();
        let mut service = NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(project.path()),
        )
        .unwrap();
        let mut port = service.take_control_port().unwrap();
        let session = SessionId::fresh();
        port.bind_session(session).unwrap();
        assert_eq!(
            port.try_prepare(0),
            Err(ProjectPreparationPortError::InvalidEpoch)
        );
        assert_eq!(
            port.try_status(SessionId::fresh()),
            Err(ProjectPreparationPortError::SessionMismatch)
        );
        let initial = port.try_status(session).unwrap();
        assert_eq!(initial.state(), PreparationState::Idle);
        assert!(initial.desired().is_none());
        drop(port);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service.try_shutdown().unwrap() {
                PreparationWorkerShutdown::Stopped => break,
                PreparationWorkerShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "worker shutdown timed out");
                    thread::yield_now();
                }
            }
        }
    }
}
