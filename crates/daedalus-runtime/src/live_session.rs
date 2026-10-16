//! A bounded, headless live simulation session for native presentation adapters.
//!
//! The session owns a real [`Engine`] on its simulation thread. Presentation reads immutable
//! latest-value frames and submits bounded commands without waiting for ticks or terrain work.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use daedalus_content::Revision;
use daedalus_content::voxel::{
    ChunkCoord, ChunkDimensions, GreedyMesh, MaterialSurface, TerrainGenerator, VoxelMaterial,
    generate_chunk, mesh_chunk_with_budget,
};
use daedalus_core::{CommandId, EntityHandle, RenderSnapshot, Tick, Transform};
use daedalus_tasks::{
    CancellationToken, ReceiveError as TaskReceiveError, SubmitError as TaskSubmitError,
    TaskCompletion, TaskFailure, TaskHandle, TaskOutcome,
};
use daedalus_world::{CommandOutcome, WorldCommand, WorldCommandKind};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CpuBudgetDiagnostics, Engine, EngineBuildError, EngineBuilder, EngineConfig, EngineError,
    SnapshotPublication,
    session_primitives::{LatestValueMailbox, MailboxError, StopSignal},
};

const MAX_COMMAND_CAPACITY: usize = 4_096;
const MAX_RESULT_CAPACITY: usize = 4_096;
const MAX_FRAME_RETENTION: usize = 64;
const MAX_GENERATION_ITERATIONS: u32 = 10_000;
const MAX_MESH_QUADS: usize = 65_536;
const MAX_MESH_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTITY_COORDINATE: f32 = 1_000_000.0;
const MAX_MOVE_DELTA: f32 = 100.0;
const STOP_SLEEP: Duration = Duration::from_millis(1);

/// Configuration for one native live session. The ECS and background task budgets are the
/// same explicit budgets used by [`EngineConfig`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSessionConfig {
    pub engine: EngineConfig,
    pub command_capacity: usize,
    pub result_capacity: usize,
    pub chunk_dimensions: ChunkDimensions,
    pub max_mesh_quads: usize,
    /// Maximum returned mesh payload bytes, including quad and revision-tag payloads.
    pub max_mesh_bytes: usize,
    /// Number of actual generate+mesh iterations in each accepted terrain job.
    pub generation_iterations: u32,
    /// Number of current or cooperatively-cancelling terrain jobs admitted at once.
    pub active_job_capacity: usize,
    pub initial_seed: u64,
    pub initial_entity_translation: [f32; 3],
}

impl Default for LiveSessionConfig {
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            command_capacity: 128,
            result_capacity: 128,
            chunk_dimensions: ChunkDimensions {
                x: 16,
                y: 16,
                z: 16,
            },
            max_mesh_quads: 8_192,
            max_mesh_bytes: 1024 * 1024,
            generation_iterations: 128,
            active_job_capacity: 1,
            initial_seed: 42,
            initial_entity_translation: [0.0, 14.0, 0.0],
        }
    }
}

impl LiveSessionConfig {
    fn validate(&self) -> Result<(), LiveSessionStartError> {
        let clock = self.engine.clock;
        if self.command_capacity == 0 || self.command_capacity > MAX_COMMAND_CAPACITY {
            return Err(LiveSessionStartError::InvalidConfig(
                "command_capacity must be in 1..=4096".to_owned(),
            ));
        }
        if self.result_capacity == 0 || self.result_capacity > MAX_RESULT_CAPACITY {
            return Err(LiveSessionStartError::InvalidConfig(
                "result_capacity must be in 1..=4096".to_owned(),
            ));
        }
        if !(1..=2).contains(&self.active_job_capacity) {
            return Err(LiveSessionStartError::InvalidConfig(
                "active_job_capacity must be 1 or 2".to_owned(),
            ));
        }
        if self.engine.task_capacity < self.active_job_capacity {
            return Err(LiveSessionStartError::InvalidConfig(
                "EngineConfig.task_capacity must cover active_job_capacity".to_owned(),
            ));
        }
        if self.generation_iterations == 0 || self.generation_iterations > MAX_GENERATION_ITERATIONS
        {
            return Err(LiveSessionStartError::InvalidConfig(
                "generation_iterations must be in 1..=10000".to_owned(),
            ));
        }
        let dims = self.chunk_dimensions;
        if dims.cell_count().is_err() || dims.x > 16 || dims.y > 16 || dims.z > 16 {
            return Err(LiveSessionStartError::InvalidConfig(
                "chunk dimensions must each be in 1..=16".to_owned(),
            ));
        }
        if self.max_mesh_quads == 0 || self.max_mesh_quads > MAX_MESH_QUADS {
            return Err(LiveSessionStartError::InvalidConfig(
                "max_mesh_quads must be in 1..=65536".to_owned(),
            ));
        }
        if self.max_mesh_bytes < std::mem::size_of::<daedalus_content::voxel::MeshQuad>()
            || self.max_mesh_bytes > MAX_MESH_BYTES
        {
            return Err(LiveSessionStartError::InvalidConfig(
                "max_mesh_bytes must fit one quad and be at most 16 MiB".to_owned(),
            ));
        }
        if self.engine.command_capacity == 0
            || self.engine.task_workers == 0
            || self.engine.task_capacity == 0
            || self.engine.snapshot_capacity == 0
            || self.engine.snapshot_capacity > MAX_FRAME_RETENTION
            || self.engine.snapshot_entity_capacity == 0
            || self.engine.compute_workers == 0
            || clock.tick_rate_hz == 0
            || clock.max_catch_up_ticks == 0
        {
            return Err(LiveSessionStartError::InvalidConfig(
                "EngineConfig capacities, worker counts, and clock limits must be positive; snapshot_capacity must be at most 64".to_owned(),
            ));
        }
        if self
            .initial_entity_translation
            .iter()
            .any(|value| !value.is_finite() || value.abs() > MAX_ENTITY_COORDINATE)
        {
            return Err(LiveSessionStartError::InvalidConfig(
                "initial entity translation must be finite and bounded".to_owned(),
            ));
        }
        Ok(())
    }
}

/// User-facing commands accepted by the live session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LiveCommand {
    MoveEntity {
        delta: [f32; 3],
    },
    RegenerateTerrain {
        seed: u64,
    },
    /// Sets the independent stop flag and receives a `Stopped` result when the actor exits.
    Stop,
}

/// Correlation ID returned when a command is admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub id: u64,
}

/// A result is emitted for every accepted command. `Accepted` only means bounded queue admission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveCommandResult {
    pub id: u64,
    pub outcome: LiveCommandOutcome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveCommandOutcome {
    Applied {
        tick: u64,
        revision: u64,
        translation: [f32; 3],
    },
    TerrainReady {
        generation: u64,
        seed: u64,
        quad_count: usize,
    },
    Superseded {
        generation: u64,
        by_generation: u64,
    },
    Stopped,
    Rejected(String),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LiveCommandSubmitError {
    #[error("the command queue is full")]
    QueueFull,
    #[error("the reliable result backlog is full")]
    ResultBacklogFull,
    #[error("command admission is busy; retry without waiting")]
    Busy,
    #[error("the live session is stopping or stopped")]
    Stopped,
    #[error("move delta must be finite and each component must be within 100 units")]
    InvalidDelta,
    #[error("command identifier space is exhausted")]
    IdExhausted,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LiveResultReadError {
    #[error("another caller is polling command results")]
    Busy,
    #[error("the command result reader was poisoned")]
    Poisoned,
    #[error("the result channel disconnected")]
    Disconnected,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LiveStatusReadError {
    #[error("another caller is reading session status")]
    Busy,
    #[error("the session status state was poisoned")]
    Poisoned,
}

#[derive(Debug, Error)]
pub enum LiveSessionStartError {
    #[error("invalid live session configuration: {0}")]
    InvalidConfig(String),
    #[error("could not build Engine: {0}")]
    EngineBuild(String),
    #[error("could not start simulation thread: {0}")]
    ThreadSpawn(String),
    #[error("simulation thread stopped before startup completed")]
    StartupDisconnected,
}

/// Engine budget plus one separately reported CLI presentation-bridge thread estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiveCpuBudget {
    pub engine: CpuBudgetDiagnostics,
    pub presentation_bridge_threads: usize,
    pub estimated_total_threads_including_bridge: usize,
    pub exceeds_logical_cpus_including_bridge: bool,
}

impl LiveCpuBudget {
    fn new(engine: CpuBudgetDiagnostics) -> Self {
        let presentation_bridge_threads = 1;
        let estimated_total_threads_including_bridge = engine
            .estimated_total_threads
            .saturating_add(presentation_bridge_threads);
        Self {
            engine,
            presentation_bridge_threads,
            estimated_total_threads_including_bridge,
            exceeds_logical_cpus_including_bridge: estimated_total_threads_including_bridge
                > engine.logical_cpus,
        }
    }
}

/// One bounded, immutable presentation value. Keeping only the latest frame is coalescible.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveFrame {
    pub sequence: u64,
    pub tick: u64,
    pub snapshot: Arc<RenderSnapshot>,
    pub terrain: LiveTerrainFrame,
    pub stats: LiveSessionStats,
    pub cpu_budget: LiveCpuBudget,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveTerrainFrame {
    /// Latest desired generation. This advances immediately on a regeneration request.
    pub generation: u64,
    pub seed: u64,
    pub loading: bool,
    /// Generation that produced the retained complete mesh, if one has completed.
    pub mesh_generation: Option<u64>,
    /// Previous complete mesh remains available while the desired generation loads.
    pub mesh: Option<Arc<GreedyMesh>>,
}

/// Bounded counters copied into each live presentation frame and final report.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiveSessionStats {
    pub simulation_ticks: u64,
    pub deferred_ticks: u64,
    pub snapshot_publications_skipped: u64,
    pub frames_published: u64,
    pub frames_skipped_at_capacity: u64,
    pub commands_accepted: u64,
    pub commands_applied: u64,
    pub commands_rejected: u64,
    pub commands_superseded: u64,
    pub commands_stopped: u64,
    pub terrain_jobs_started: u64,
    pub terrain_jobs_completed: u64,
    pub terrain_jobs_cancelled: u64,
    pub terrain_jobs_stale: u64,
    pub terrain_jobs_failed: u64,
    pub desired_generation: u64,
    pub completed_generation: Option<u64>,
    pub active_jobs: usize,
    pub pending_move_commands: usize,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveTermination {
    StopRequested,
    RuntimeError(String),
    Panicked(String),
}

/// Final observable state after cooperative cancellation and simulation-thread join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveShutdownReport {
    pub termination: LiveTermination,
    pub final_tick: u64,
    pub final_entity_translation: Option<[f32; 3]>,
    pub stats: LiveSessionStats,
    pub cpu_budget: Option<LiveCpuBudget>,
    pub task_workers: usize,
    pub active_job_capacity: usize,
    pub command_capacity: usize,
    pub result_capacity: usize,
    pub generation_iterations: u32,
    pub max_mesh_quads: usize,
    pub max_mesh_bytes: usize,
    /// Results accepted before stop but not consumed through `try_result`; bounded by the
    /// configured result capacity and returned with their original command IDs.
    pub unread_command_results: usize,
    pub remaining_command_results: Vec<LiveCommandResult>,
    pub join_duration_micros: u64,
}

/// Owns the consumer endpoints and join handle for one simulation thread.
pub struct LiveSession {
    shared: Arc<SessionShared>,
    command_sender: SyncSender<CommandEnvelope>,
    result_receiver: Mutex<Receiver<LiveCommandResult>>,
    frames: Arc<LatestValueMailbox<LiveFrame>>,
    join: Option<JoinHandle<LiveShutdownReport>>,
    config: LiveSessionConfig,
    initial_entity: EntityHandle,
}

impl LiveSession {
    /// Validate all public limits before constructing worker pools or the simulation thread.
    pub fn start(config: LiveSessionConfig) -> Result<Self, LiveSessionStartError> {
        config.validate()?;

        let (command_sender, command_receiver) = mpsc::sync_channel(config.command_capacity);
        let (result_sender, result_receiver) = mpsc::sync_channel(config.result_capacity);
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let shared = Arc::new(SessionShared {
            stop_requested: StopSignal::default(),
            next_command_id: AtomicU64::new(1),
            outstanding_results: AtomicUsize::new(0),
            result_capacity: config.result_capacity,
            stop_command_id: AtomicU64::new(0),
            submit_gate: Mutex::new(()),
            termination: Mutex::new(None),
        });
        let frames = Arc::new(LatestValueMailbox::new(config.engine.snapshot_capacity));
        let actor_shared = Arc::clone(&shared);
        let actor_frames = Arc::clone(&frames);
        let actor_config = config.clone();
        let join = thread::Builder::new()
            .name("daedalus-live-simulation".to_owned())
            .spawn(move || {
                run_session_thread(
                    actor_config,
                    actor_shared,
                    actor_frames,
                    command_receiver,
                    result_sender,
                    startup_sender,
                )
            })
            .map_err(|error| LiveSessionStartError::ThreadSpawn(error.to_string()))?;

        let initial_entity = match startup_receiver.recv() {
            Ok(Ok(entity)) => entity,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(_) => {
                let _ = join.join();
                return Err(LiveSessionStartError::StartupDisconnected);
            }
        };
        Ok(Self {
            shared,
            command_sender,
            result_receiver: Mutex::new(result_receiver),
            frames,
            join: Some(join),
            config,
            initial_entity,
        })
    }

    /// The stable handle for the one initially spawned renderable entity.
    #[must_use]
    pub const fn initial_entity_handle(&self) -> EntityHandle {
        self.initial_entity
    }

    /// Admit a command without waiting for queue capacity or a simulation tick.
    pub fn try_command(
        &self,
        command: LiveCommand,
    ) -> Result<CommandAccepted, LiveCommandSubmitError> {
        if let LiveCommand::MoveEntity { delta } = &command
            && delta
                .iter()
                .any(|value| !value.is_finite() || value.abs() > MAX_MOVE_DELTA)
        {
            return Err(LiveCommandSubmitError::InvalidDelta);
        }
        let _gate = match self.shared.submit_gate.try_lock() {
            Ok(gate) => gate,
            Err(TryLockError::WouldBlock) => return Err(LiveCommandSubmitError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(LiveCommandSubmitError::Stopped),
        };
        if self.shared.stop_requested.is_requested() {
            return Err(LiveCommandSubmitError::Stopped);
        }
        self.shared.reserve_result_credit()?;
        let id = match self.shared.next_id() {
            Ok(id) => id,
            Err(error) => {
                self.shared.release_result_credit();
                return Err(error);
            }
        };

        if matches!(command, LiveCommand::Stop) {
            if self
                .shared
                .stop_command_id
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                self.shared.release_result_credit();
                return Err(LiveCommandSubmitError::Stopped);
            }
            self.shared.stop_requested.request();
            return Ok(CommandAccepted { id });
        }

        match self
            .command_sender
            .try_send(CommandEnvelope { id, command })
        {
            Ok(()) => Ok(CommandAccepted { id }),
            Err(TrySendError::Full(_)) => {
                self.shared.release_result_credit();
                Err(LiveCommandSubmitError::QueueFull)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.shared.release_result_credit();
                Err(LiveCommandSubmitError::Stopped)
            }
        }
    }

    /// Read the latest complete frame without waiting for the simulation publisher.
    /// `None` includes an empty mailbox, a busy mailbox, or a retained-frame budget hit.
    #[must_use]
    pub fn try_latest_frame(&self) -> Option<Arc<LiveFrame>> {
        self.frames.try_latest().ok().flatten()
    }

    /// Poll one reliable command result without waiting.
    pub fn try_result(&self) -> Result<Option<LiveCommandResult>, LiveResultReadError> {
        let receiver = match self.result_receiver.try_lock() {
            Ok(receiver) => receiver,
            Err(TryLockError::WouldBlock) => return Err(LiveResultReadError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(LiveResultReadError::Poisoned),
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.shared.release_result_credit();
                Ok(Some(result))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(LiveResultReadError::Disconnected),
        }
    }

    /// Read the terminal reason without waiting. `None` means the actor is still active.
    pub fn try_termination(&self) -> Result<Option<LiveTermination>, LiveStatusReadError> {
        match self.shared.termination.try_lock() {
            Ok(termination) => Ok(termination.clone()),
            Err(TryLockError::WouldBlock) => Err(LiveStatusReadError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(LiveStatusReadError::Poisoned),
        }
    }

    /// Request cooperative shutdown independently of command-queue capacity.
    pub fn request_stop(&self) {
        self.shared.stop_requested.request();
    }

    /// Request stop and join the simulation thread. Call this outside event/presentation callbacks.
    pub fn shutdown(mut self) -> LiveShutdownReport {
        self.request_stop();
        let started = Instant::now();
        let report = match self.join.take().expect("live session join handle").join() {
            Ok(report) => report,
            Err(payload) => empty_report(
                &self.config,
                LiveTermination::Panicked(panic_text(payload)),
                None,
            ),
        };
        let mut report = LiveShutdownReport {
            join_duration_micros: started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            ..report
        };
        if let Ok(receiver) = self.result_receiver.get_mut() {
            while let Ok(result) = receiver.try_recv() {
                self.shared.release_result_credit();
                report.remaining_command_results.push(result);
            }
        }
        report.unread_command_results = report.remaining_command_results.len();
        report
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Do not join from Drop: it can run on a UI callback. The actor observes this signal and
        // performs cooperative task cancellation; explicit `shutdown` is the reporting join path.
        self.shared.stop_requested.request();
    }
}

struct SessionShared {
    stop_requested: StopSignal,
    next_command_id: AtomicU64,
    outstanding_results: AtomicUsize,
    result_capacity: usize,
    stop_command_id: AtomicU64,
    submit_gate: Mutex<()>,
    termination: Mutex<Option<LiveTermination>>,
}

impl SessionShared {
    fn set_termination(&self, termination: LiveTermination) {
        *self
            .termination
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(termination);
    }

    fn reserve_result_credit(&self) -> Result<(), LiveCommandSubmitError> {
        loop {
            let outstanding = self.outstanding_results.load(Ordering::Acquire);
            if outstanding >= self.result_capacity {
                return Err(LiveCommandSubmitError::ResultBacklogFull);
            }
            if self
                .outstanding_results
                .compare_exchange_weak(
                    outstanding,
                    outstanding + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    fn release_result_credit(&self) {
        let _ = self.outstanding_results.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |outstanding| Some(outstanding.saturating_sub(1)),
        );
    }

    fn next_id(&self) -> Result<u64, LiveCommandSubmitError> {
        self.next_command_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| LiveCommandSubmitError::IdExhausted)
    }
}

struct CommandEnvelope {
    id: u64,
    command: LiveCommand,
}

#[derive(Clone)]
struct DesiredTerrain {
    generation: u64,
    seed: u64,
    command_id: Option<u64>,
    submitted: bool,
}

struct ActiveTerrainJob {
    generation: u64,
    task: TaskHandle<Result<Arc<GreedyMesh>, String>>,
    command_id: Option<u64>,
}

struct PendingMove {
    command_id: u64,
}

struct ActorState {
    config: LiveSessionConfig,
    engine: Engine,
    entity: EntityHandle,
    cpu_budget: LiveCpuBudget,
    stats: LiveSessionStats,
    frame_sequence: u64,
    frame_dirty: bool,
    last_frame_attempt: Option<Instant>,
    latest_snapshot: Arc<RenderSnapshot>,
    terrain: LiveTerrainFrame,
    desired: DesiredTerrain,
    pending_generation_command: Option<(u64, u64)>,
    jobs: Vec<ActiveTerrainJob>,
    queued_moves: VecDeque<(u64, [f32; 3])>,
    pending_moves: HashMap<CommandId, PendingMove>,
    next_world_command_id: u128,
    result_outbox: VecDeque<LiveCommandResult>,
}

fn run_session_thread(
    config: LiveSessionConfig,
    shared: Arc<SessionShared>,
    frames: Arc<LatestValueMailbox<LiveFrame>>,
    command_receiver: Receiver<CommandEnvelope>,
    result_sender: SyncSender<LiveCommandResult>,
    startup_sender: SyncSender<Result<EntityHandle, LiveSessionStartError>>,
) -> LiveShutdownReport {
    let engine = match EngineBuilder::new(config.engine.clone()).build() {
        Ok(engine) => engine,
        Err(error) => {
            let message = error.to_string();
            let _ = startup_sender.send(Err(LiveSessionStartError::EngineBuild(message.clone())));
            let termination = LiveTermination::RuntimeError(message);
            shared.set_termination(termination.clone());
            return empty_report(&config, termination, None);
        }
    };
    let mut actor = match ActorState::new(config.clone(), engine) {
        Ok(actor) => actor,
        Err(error) => {
            let message = error.to_string();
            let _ = startup_sender.send(Err(LiveSessionStartError::EngineBuild(message.clone())));
            let termination = LiveTermination::RuntimeError(message);
            shared.set_termination(termination.clone());
            return empty_report(&config, termination, None);
        }
    };
    actor.start_initial_terrain();
    actor.publish_frame(&frames);
    let _ = startup_sender.send(Ok(actor.entity));

    let termination = match catch_unwind(AssertUnwindSafe(|| {
        run_actor_loop(
            &mut actor,
            &shared,
            &frames,
            &command_receiver,
            &result_sender,
        )
    })) {
        Ok(Ok(())) => LiveTermination::StopRequested,
        Ok(Err(error)) => LiveTermination::RuntimeError(error),
        Err(payload) => LiveTermination::Panicked(panic_text(payload)),
    };

    shared.stop_requested.request();
    let pending = drain_commands_at_stop(&shared, &command_receiver);
    for command in pending {
        actor.stats.commands_accepted = actor.stats.commands_accepted.saturating_add(1);
        actor.emit(command.id, LiveCommandOutcome::Stopped);
    }
    let stop_command_id = shared.stop_command_id.swap(0, Ordering::AcqRel);
    if stop_command_id != 0 {
        actor.stats.commands_accepted = actor.stats.commands_accepted.saturating_add(1);
        actor.emit(stop_command_id, LiveCommandOutcome::Stopped);
    }
    actor.stop_pending_terrain();
    actor.apply_final_tick();
    if let Err(error) = actor.engine.shutdown() {
        actor.stats.last_error = Some(error.to_string());
        actor.stats.terrain_jobs_failed = actor.stats.terrain_jobs_failed.saturating_add(1);
    }
    actor.collect_completed_tasks(true);
    actor.reject_pending_moves_as_stopped();
    actor.stats.active_jobs = 0;
    actor.publish_frame(&frames);
    actor.flush_results(&result_sender, true);
    shared.set_termination(termination.clone());
    actor.report(termination)
}

impl ActorState {
    fn new(config: LiveSessionConfig, mut engine: Engine) -> Result<Self, EngineBuildError> {
        let mut initial_transform = Transform::IDENTITY;
        initial_transform.translation = config.initial_entity_translation;
        let entity = engine
            .spawn_transform(initial_transform)
            .map_err(|error| EngineBuildError::GameplaySchedule(error.to_string()))?;
        let step = engine.step_tick().map_err(|error| match error {
            EngineError::TickCounterExhausted => {
                EngineBuildError::GameplaySchedule(error.to_string())
            }
            EngineError::Shutdown(_) => EngineBuildError::GameplaySchedule(error.to_string()),
        })?;
        let snapshot = engine
            .snapshot_consumer()
            .try_latest()
            .map_err(|error| EngineBuildError::GameplaySchedule(error.to_string()))?
            .ok_or_else(|| {
                EngineBuildError::GameplaySchedule(
                    "engine did not publish its initial entity snapshot".to_owned(),
                )
            })?;
        let cpu_budget = LiveCpuBudget::new(engine.cpu_budget());
        let mut stats = LiveSessionStats {
            simulation_ticks: 1,
            ..LiveSessionStats::default()
        };
        if !matches!(step.snapshot, SnapshotPublication::Published { .. }) {
            stats.snapshot_publications_skipped = 1;
        }
        Ok(Self {
            config: config.clone(),
            engine,
            entity,
            cpu_budget,
            stats,
            frame_sequence: 0,
            frame_dirty: true,
            last_frame_attempt: None,
            latest_snapshot: snapshot,
            terrain: LiveTerrainFrame {
                generation: 1,
                seed: config.initial_seed,
                loading: true,
                mesh_generation: None,
                mesh: None,
            },
            desired: DesiredTerrain {
                generation: 1,
                seed: config.initial_seed,
                command_id: None,
                submitted: false,
            },
            pending_generation_command: None,
            jobs: Vec::new(),
            queued_moves: VecDeque::new(),
            pending_moves: HashMap::new(),
            next_world_command_id: 1,
            result_outbox: VecDeque::new(),
        })
    }

    fn start_initial_terrain(&mut self) {
        self.try_start_desired_job();
    }

    fn run_command(&mut self, envelope: CommandEnvelope) {
        self.stats.commands_accepted = self.stats.commands_accepted.saturating_add(1);
        match envelope.command {
            LiveCommand::MoveEntity { delta } => {
                self.queued_moves.push_back((envelope.id, delta));
                self.stats.pending_move_commands = self
                    .pending_moves
                    .len()
                    .saturating_add(self.queued_moves.len());
                self.frame_dirty = true;
            }
            LiveCommand::RegenerateTerrain { seed } => self.regenerate(envelope.id, seed),
            LiveCommand::Stop => {
                // Stop commands are handled out-of-band by LiveSession::try_command.
                self.emit(
                    envelope.id,
                    LiveCommandOutcome::Rejected("stop must use the independent stop path".into()),
                );
            }
        }
    }

    fn schedule_move(&mut self, command_id: u64, delta: [f32; 3]) {
        let (mut transform, revision) = match self.engine.world().transform(self.entity) {
            Ok(state) => state,
            Err(error) => {
                self.reject(command_id, error.to_string());
                return;
            }
        };
        for (axis, change) in delta.into_iter().enumerate() {
            transform.translation[axis] += change;
        }
        if !transform.is_finite()
            || transform
                .translation
                .iter()
                .any(|value| value.abs() > MAX_ENTITY_COORDINATE)
        {
            self.reject(
                command_id,
                "resulting transform is outside the finite session bounds".into(),
            );
            return;
        }
        let Some(apply_tick) = self.engine.clock().current_tick().0.checked_add(1) else {
            self.reject(command_id, "simulation tick counter is exhausted".into());
            return;
        };
        let internal_id = self.next_world_command_id;
        let Some(next_id) = self.next_world_command_id.checked_add(1) else {
            self.reject(
                command_id,
                "world command identifier space is exhausted".into(),
            );
            return;
        };
        self.next_world_command_id = next_id;
        let world_command = WorldCommand {
            command_id: CommandId::from_raw(internal_id),
            target: self.entity,
            expected_revision: revision,
            apply_at: Tick(apply_tick),
            kind: WorldCommandKind::SetTransform(transform),
        };
        match self.engine.try_command(world_command) {
            Ok(_) => {
                self.pending_moves
                    .insert(CommandId::from_raw(internal_id), PendingMove { command_id });
                self.stats.pending_move_commands = self
                    .pending_moves
                    .len()
                    .saturating_add(self.queued_moves.len());
            }
            Err(error) => self.reject(command_id, error.to_string()),
        }
    }

    fn schedule_next_move(&mut self) {
        if !self.pending_moves.is_empty() {
            return;
        }
        while let Some((command_id, delta)) = self.queued_moves.pop_front() {
            self.schedule_move(command_id, delta);
            if !self.pending_moves.is_empty() {
                break;
            }
        }
        self.stats.pending_move_commands = self
            .pending_moves
            .len()
            .saturating_add(self.queued_moves.len());
    }

    fn regenerate(&mut self, command_id: u64, seed: u64) {
        let Some(generation) = self.desired.generation.checked_add(1) else {
            self.reject(command_id, "terrain generation counter is exhausted".into());
            return;
        };
        if let Some((old_generation, old_command_id)) = self.pending_generation_command.take() {
            self.stats.commands_superseded = self.stats.commands_superseded.saturating_add(1);
            self.result_outbox.push_back(LiveCommandResult {
                id: old_command_id,
                outcome: LiveCommandOutcome::Superseded {
                    generation: old_generation,
                    by_generation: generation,
                },
            });
        }
        for active in &self.jobs {
            if active.generation < generation {
                let _ = active.task.cancel();
            }
        }
        self.desired = DesiredTerrain {
            generation,
            seed,
            command_id: Some(command_id),
            submitted: false,
        };
        self.pending_generation_command = Some((generation, command_id));
        self.terrain.generation = generation;
        self.terrain.seed = seed;
        self.terrain.loading = true;
        self.stats.desired_generation = generation;
        self.frame_dirty = true;
        self.try_start_desired_job();
    }

    fn try_start_desired_job(&mut self) {
        if self.desired.submitted || self.jobs.len() >= self.config.active_job_capacity {
            return;
        }
        let request = TerrainJobRequest {
            seed: self.desired.seed,
            generation: self.desired.generation,
            dimensions: self.config.chunk_dimensions,
            iterations: self.config.generation_iterations,
            max_quads: self.config.max_mesh_quads,
            max_mesh_bytes: self.config.max_mesh_bytes,
        };
        let generation = self.desired.generation;
        let command_id = self.desired.command_id;
        match self.engine.tasks().try_submit(move |token| {
            let result = generate_and_mesh_repeatedly(request, token.clone());
            Ok::<_, TaskFailure>(result)
        }) {
            Ok(task) => {
                self.desired.submitted = true;
                self.jobs.push(ActiveTerrainJob {
                    generation,
                    task,
                    command_id,
                });
                self.stats.terrain_jobs_started = self.stats.terrain_jobs_started.saturating_add(1);
                self.stats.active_jobs = self.jobs.len();
                self.frame_dirty = true;
            }
            Err(TaskSubmitError::Full | TaskSubmitError::Busy) => {}
            Err(error) => {
                self.desired.submitted = true;
                self.terrain.loading = false;
                self.stats.terrain_jobs_failed = self.stats.terrain_jobs_failed.saturating_add(1);
                self.stats.last_error = Some(error.to_string());
                if let Some(command_id) = command_id {
                    self.pending_generation_command = None;
                    self.reject(command_id, error.to_string());
                }
            }
        }
    }

    fn collect_completed_tasks(&mut self, shutting_down: bool) {
        loop {
            let completion = match self.engine.tasks().try_recv() {
                Ok(Some(completion)) => completion,
                Ok(None) | Err(TaskReceiveError::Busy) => break,
                Err(TaskReceiveError::Poisoned) => {
                    self.stats.last_error =
                        Some("background task result state was poisoned".into());
                    break;
                }
            };
            self.handle_task_completion(completion, shutting_down);
        }
        self.stats.active_jobs = self.jobs.len();
    }

    fn handle_task_completion(&mut self, completion: TaskCompletion, shutting_down: bool) {
        let Some(index) = self
            .jobs
            .iter()
            .position(|job| job.task.id() == completion.id)
        else {
            self.stats.last_error = Some(format!(
                "unexpected terrain task result {:?}",
                completion.id
            ));
            return;
        };
        let job = self.jobs.swap_remove(index);
        self.stats.active_jobs = self.jobs.len();
        self.frame_dirty = true;
        let stale = job.generation != self.desired.generation;
        if stale {
            self.stats.terrain_jobs_stale = self.stats.terrain_jobs_stale.saturating_add(1);
        }
        let outcome = match completion.into_outcome::<Result<Arc<GreedyMesh>, String>>() {
            Ok(TaskOutcome::Completed(Ok(mesh))) => validate_mesh_tag(
                &mesh,
                job.generation,
                u64::from(self.config.generation_iterations),
            )
            .map(|()| mesh),
            Ok(TaskOutcome::Completed(Err(error))) => Err(error),
            Ok(TaskOutcome::Cancelled) => {
                self.stats.terrain_jobs_cancelled =
                    self.stats.terrain_jobs_cancelled.saturating_add(1);
                if stale || shutting_down {
                    return;
                }
                Err("terrain work was cancelled".to_owned())
            }
            Ok(TaskOutcome::Failed(error)) => Err(error.message),
            Ok(TaskOutcome::Panicked(message)) => {
                Err(format!("terrain worker panicked: {message}"))
            }
            Err(error) => Err(error.to_string()),
        };
        if stale || shutting_down {
            return;
        }
        match outcome {
            Ok(mesh) => {
                let quad_count = mesh.quads.len();
                self.terrain.mesh = Some(mesh);
                self.terrain.mesh_generation = Some(job.generation);
                self.terrain.loading = false;
                self.stats.completed_generation = Some(job.generation);
                self.stats.terrain_jobs_completed =
                    self.stats.terrain_jobs_completed.saturating_add(1);
                if let Some(command_id) = job.command_id {
                    self.pending_generation_command = None;
                    self.emit(
                        command_id,
                        LiveCommandOutcome::TerrainReady {
                            generation: job.generation,
                            seed: self.terrain.seed,
                            quad_count,
                        },
                    );
                }
            }
            Err(error) => {
                self.terrain.loading = false;
                self.stats.terrain_jobs_failed = self.stats.terrain_jobs_failed.saturating_add(1);
                self.stats.last_error = Some(error.clone());
                if let Some(command_id) = job.command_id {
                    self.pending_generation_command = None;
                    self.reject(command_id, error);
                }
            }
        }
    }

    fn process_step_reports(&mut self, reports: Vec<crate::StepReport>) {
        let had_steps = !reports.is_empty();
        for report in reports {
            self.stats.simulation_ticks = self.stats.simulation_ticks.saturating_add(1);
            if !matches!(report.snapshot, SnapshotPublication::Published { .. }) {
                self.stats.snapshot_publications_skipped =
                    self.stats.snapshot_publications_skipped.saturating_add(1);
            }
            for result in report.commands {
                let Some(pending) = self.pending_moves.remove(&result.command_id) else {
                    continue;
                };
                match result.outcome {
                    CommandOutcome::Applied { revision } => {
                        match self.engine.world().transform(self.entity) {
                            Ok((transform, _)) => self.emit(
                                pending.command_id,
                                LiveCommandOutcome::Applied {
                                    tick: report.tick.0,
                                    revision: revision.0,
                                    translation: transform.translation,
                                },
                            ),
                            Err(error) => self.reject(pending.command_id, error.to_string()),
                        }
                    }
                    CommandOutcome::Rejected(error) => {
                        self.reject(pending.command_id, error.to_string());
                    }
                    CommandOutcome::Despawned => {
                        self.reject(pending.command_id, "entity was despawned".into());
                    }
                }
            }
        }
        if let Ok(Some(snapshot)) = self.engine.snapshot_consumer().try_latest() {
            self.latest_snapshot = snapshot;
        }
        self.stats.pending_move_commands = self
            .pending_moves
            .len()
            .saturating_add(self.queued_moves.len());
        self.frame_dirty |= had_steps;
    }

    fn apply_final_tick(&mut self) {
        if self.pending_moves.is_empty() {
            return;
        }
        let result = self.engine.step_tick();
        match result {
            Ok(report) => self.process_step_reports(vec![report]),
            Err(error) => {
                self.stats.last_error = Some(error.to_string());
                self.reject_pending_moves_as_stopped();
            }
        }
    }

    fn stop_pending_terrain(&mut self) {
        for job in &self.jobs {
            let _ = job.task.cancel();
        }
        if let Some((_, command_id)) = self.pending_generation_command.take() {
            self.emit(command_id, LiveCommandOutcome::Stopped);
        }
        self.terrain.loading = false;
        self.frame_dirty = true;
    }

    fn reject_pending_moves_as_stopped(&mut self) {
        while let Some((command_id, _)) = self.queued_moves.pop_front() {
            self.emit(command_id, LiveCommandOutcome::Stopped);
        }
        let pending = std::mem::take(&mut self.pending_moves);
        for (_, move_command) in pending {
            self.emit(move_command.command_id, LiveCommandOutcome::Stopped);
        }
        self.stats.pending_move_commands = 0;
    }

    fn reject(&mut self, command_id: u64, message: String) {
        self.emit(command_id, LiveCommandOutcome::Rejected(message));
    }

    fn emit(&mut self, command_id: u64, outcome: LiveCommandOutcome) {
        self.frame_dirty = true;
        match outcome {
            LiveCommandOutcome::Applied { .. } | LiveCommandOutcome::TerrainReady { .. } => {
                self.stats.commands_applied = self.stats.commands_applied.saturating_add(1);
                self.result_outbox.push_back(LiveCommandResult {
                    id: command_id,
                    outcome,
                });
            }
            LiveCommandOutcome::Superseded { .. } => {
                self.stats.commands_superseded = self.stats.commands_superseded.saturating_add(1);
                self.result_outbox.push_back(LiveCommandResult {
                    id: command_id,
                    outcome,
                });
            }
            LiveCommandOutcome::Stopped => {
                self.stats.commands_stopped = self.stats.commands_stopped.saturating_add(1);
                self.result_outbox.push_back(LiveCommandResult {
                    id: command_id,
                    outcome,
                });
            }
            LiveCommandOutcome::Rejected(_) => {
                self.stats.commands_rejected = self.stats.commands_rejected.saturating_add(1);
                self.result_outbox.push_back(LiveCommandResult {
                    id: command_id,
                    outcome,
                });
            }
        }
    }

    fn flush_results(&mut self, sender: &SyncSender<LiveCommandResult>, final_flush: bool) {
        while let Some(result) = self.result_outbox.pop_front() {
            match sender.try_send(result) {
                Ok(()) => {}
                Err(TrySendError::Full(result)) => {
                    self.result_outbox.push_front(result);
                    if !final_flush {
                        break;
                    }
                    // Credits guarantee all accepted results fit in the bounded channel. Keep
                    // the invariant visible if a future edit violates that relationship.
                    self.stats.last_error =
                        Some("reliable result channel capacity invariant was violated".to_owned());
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.result_outbox.clear();
                    break;
                }
            }
        }
    }

    fn publish_frame(&mut self, mailbox: &LatestValueMailbox<LiveFrame>) {
        if !self.frame_dirty {
            return;
        }
        let retry_interval = Duration::from_nanos(
            1_000_000_000_u64 / u64::from(self.config.engine.clock.tick_rate_hz),
        );
        if self
            .last_frame_attempt
            .is_some_and(|last| last.elapsed() < retry_interval)
        {
            return;
        }
        self.last_frame_attempt = Some(Instant::now());
        let Some(next_sequence) = self.frame_sequence.checked_add(1) else {
            self.stats.last_error = Some("live frame sequence is exhausted".to_owned());
            return;
        };
        self.frame_sequence = next_sequence;
        self.stats.desired_generation = self.desired.generation;
        self.stats.active_jobs = self.jobs.len();
        let frame = LiveFrame {
            sequence: next_sequence,
            tick: self.stats.simulation_ticks,
            snapshot: Arc::clone(&self.latest_snapshot),
            terrain: self.terrain.clone(),
            stats: self.stats.clone(),
            cpu_budget: self.cpu_budget,
        };
        match mailbox.try_publish(frame) {
            Ok(()) => {
                self.stats.frames_published = self.stats.frames_published.saturating_add(1);
                self.frame_dirty = false;
            }
            Err(MailboxError::AtCapacity | MailboxError::Busy) => {
                self.stats.frames_skipped_at_capacity =
                    self.stats.frames_skipped_at_capacity.saturating_add(1);
            }
            Err(MailboxError::Poisoned) => {
                self.stats.last_error = Some("live frame mailbox was poisoned".to_owned());
            }
        }
    }

    fn report(&self, termination: LiveTermination) -> LiveShutdownReport {
        let final_entity_translation = self
            .engine
            .world()
            .transform(self.entity)
            .ok()
            .map(|(transform, _)| transform.translation);
        LiveShutdownReport {
            termination,
            final_tick: self.engine.clock().current_tick().0,
            final_entity_translation,
            stats: self.stats.clone(),
            cpu_budget: Some(self.cpu_budget),
            task_workers: self.engine.tasks().worker_count(),
            active_job_capacity: self.config.active_job_capacity,
            command_capacity: self.config.command_capacity,
            result_capacity: self.config.result_capacity,
            generation_iterations: self.config.generation_iterations,
            max_mesh_quads: self.config.max_mesh_quads,
            max_mesh_bytes: self.config.max_mesh_bytes,
            unread_command_results: 0,
            remaining_command_results: Vec::new(),
            join_duration_micros: 0,
        }
    }
}

fn run_actor_loop(
    actor: &mut ActorState,
    shared: &SessionShared,
    frames: &LatestValueMailbox<LiveFrame>,
    command_receiver: &Receiver<CommandEnvelope>,
    result_sender: &SyncSender<LiveCommandResult>,
) -> Result<(), String> {
    let mut last_advance = Instant::now();
    loop {
        let mut received_command = false;
        while let Ok(command) = command_receiver.try_recv() {
            actor.run_command(command);
            received_command = true;
        }
        actor.collect_completed_tasks(false);
        actor.try_start_desired_job();
        actor.schedule_next_move();

        let now = Instant::now();
        let elapsed = now.saturating_duration_since(last_advance);
        last_advance = now;
        match actor.engine.advance_elapsed(elapsed) {
            Ok((clock, reports)) => {
                actor.stats.deferred_ticks = clock.deferred_ticks;
                actor.process_step_reports(reports);
            }
            Err(error) => return Err(error.to_string()),
        }
        actor.collect_completed_tasks(false);
        actor.try_start_desired_job();
        actor.publish_frame(frames);
        actor.flush_results(result_sender, false);

        if shared.stop_requested.is_requested() {
            return Ok(());
        }
        if !received_command {
            thread::sleep(STOP_SLEEP);
        }
    }
}

fn drain_commands_at_stop(
    shared: &SessionShared,
    receiver: &Receiver<CommandEnvelope>,
) -> Vec<CommandEnvelope> {
    // Submission uses try_lock and holds this guard only through one bounded try_send. Taking it
    // here closes the admission race: every accepted command is either in this drain or was
    // already consumed by the actor before it observed stop.
    let _gate = shared
        .submit_gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut pending = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(command) => pending.push(command),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return pending,
        }
    }
}

struct TerrainJobRequest {
    seed: u64,
    generation: u64,
    dimensions: ChunkDimensions,
    iterations: u32,
    max_quads: usize,
    max_mesh_bytes: usize,
}

fn generate_and_mesh_repeatedly(
    request: TerrainJobRequest,
    cancellation: CancellationToken,
) -> Result<Arc<GreedyMesh>, String> {
    let materials = BTreeMap::from([
        (
            1,
            VoxelMaterial {
                id: 1,
                surface: MaterialSurface::Opaque,
                merge_group: 1,
            },
        ),
        (
            2,
            VoxelMaterial {
                id: 2,
                surface: MaterialSurface::Opaque,
                merge_group: 2,
            },
        ),
    ]);
    let neighbors = BTreeMap::new();
    let max_quads_by_bytes =
        request.max_mesh_bytes / std::mem::size_of::<daedalus_content::voxel::MeshQuad>();
    let max_quads = request.max_quads.min(max_quads_by_bytes);
    let mut latest_mesh = None;
    for iteration in 0..request.iterations {
        if cancellation.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let chunk = generate_chunk(
            request.seed,
            ChunkCoord { x: 0, y: 0, z: 0 },
            request.dimensions,
            TerrainGenerator::default(),
            request.generation,
            Revision::from(iteration) + 1,
        )
        .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let mesh = mesh_chunk_with_budget(
            &chunk,
            &neighbors,
            &materials,
            request.generation,
            max_quads,
        )
        .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let payload_bytes = mesh_payload_bytes(&mesh);
        if payload_bytes > request.max_mesh_bytes {
            return Err(format!(
                "mesh payload is {payload_bytes} bytes, above the configured {} byte limit",
                request.max_mesh_bytes
            ));
        }
        latest_mesh = Some(Arc::new(mesh));
    }
    latest_mesh.ok_or_else(|| "terrain job completed without a mesh".to_owned())
}

fn mesh_payload_bytes(mesh: &GreedyMesh) -> usize {
    std::mem::size_of::<GreedyMesh>()
        .saturating_add(
            mesh.quads
                .len()
                .saturating_mul(std::mem::size_of::<daedalus_content::voxel::MeshQuad>()),
        )
        .saturating_add(std::mem::size_of_val(mesh.tag.neighbors.as_slice()))
}

fn validate_mesh_tag(
    mesh: &GreedyMesh,
    expected_generation: u64,
    expected_revision: u64,
) -> Result<(), String> {
    if mesh.tag.coord != (ChunkCoord { x: 0, y: 0, z: 0 })
        || mesh.tag.source.lifetime_generation != expected_generation
        || mesh.tag.source.data_revision != expected_revision
        || mesh.tag.job_generation != expected_generation
        || mesh.tag.neighbors.len() != 6
        || mesh
            .tag
            .neighbors
            .iter()
            .any(|neighbor| neighbor.stamp.is_some())
    {
        return Err("mesh revision tag did not match the requested terrain input".to_owned());
    }
    Ok(())
}

fn empty_report(
    config: &LiveSessionConfig,
    termination: LiveTermination,
    cpu_budget: Option<LiveCpuBudget>,
) -> LiveShutdownReport {
    LiveShutdownReport {
        termination,
        final_tick: 0,
        final_entity_translation: None,
        stats: LiveSessionStats::default(),
        cpu_budget,
        task_workers: config.engine.task_workers,
        active_job_capacity: config.active_job_capacity,
        command_capacity: config.command_capacity,
        result_capacity: config.result_capacity,
        generation_iterations: config.generation_iterations,
        max_mesh_quads: config.max_mesh_quads,
        max_mesh_bytes: config.max_mesh_bytes,
        unread_command_results: 0,
        remaining_command_results: Vec::new(),
        join_duration_micros: 0,
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "simulation thread panicked with a non-string payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use daedalus_content::voxel::ChunkDimensions;

    use super::{
        LiveCommand, LiveCommandOutcome, LiveCommandSubmitError, LiveSession, LiveSessionConfig,
        LiveSessionStartError,
    };

    fn fast_config() -> LiveSessionConfig {
        LiveSessionConfig {
            generation_iterations: 1,
            ..LiveSessionConfig::default()
        }
    }

    fn wait_result(session: &LiveSession, command_id: u64) -> super::LiveCommandResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while let Some(result) = session.try_result().unwrap() {
                if result.id == command_id {
                    return result;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for command result"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_frame(session: &LiveSession) -> std::sync::Arc<super::LiveFrame> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = session.try_latest_frame() {
                return frame;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for live frame"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_mesh(session: &LiveSession, generation: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if session
                .try_latest_frame()
                .is_some_and(|frame| frame.terrain.mesh_generation == Some(generation))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for mesh generation"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn initial_snapshot_is_real_ecs_and_move_ack_follows_world_command() {
        let session = LiveSession::start(fast_config()).unwrap();
        let initial = wait_for_frame(&session);
        assert_eq!(initial.snapshot.entities.len(), 1);
        assert_eq!(
            initial.snapshot.entities[0].entity,
            session.initial_entity_handle()
        );
        assert_eq!(
            initial.snapshot.entities[0].transform.translation,
            [0.0, 14.0, 0.0]
        );

        let accepted = session
            .try_command(LiveCommand::MoveEntity {
                delta: [1.0, -2.0, 3.0],
            })
            .unwrap();
        let second = session
            .try_command(LiveCommand::MoveEntity {
                delta: [2.0, 1.0, -1.0],
            })
            .unwrap();
        let result = wait_result(&session, accepted.id);
        let LiveCommandOutcome::Applied {
            tick: first_tick,
            revision: first_revision,
            translation: first_translation,
        } = result.outcome
        else {
            panic!("first movement should be applied");
        };
        assert!(first_tick > initial.tick);
        assert_eq!(first_revision, 1);
        assert_eq!(first_translation, [1.0, 12.0, 3.0]);

        let LiveCommandOutcome::Applied {
            tick: second_tick,
            revision: second_revision,
            translation: second_translation,
        } = wait_result(&session, second.id).outcome
        else {
            panic!("second movement should be applied");
        };
        assert!(second_tick > first_tick);
        assert_eq!(second_revision, 2);
        assert_eq!(second_translation, [3.0, 13.0, 2.0]);
        let report = session.shutdown();
        assert_eq!(report.final_entity_translation, Some([3.0, 13.0, 2.0]));
        assert!(report.stats.commands_applied >= 2);
    }

    #[test]
    fn initial_real_generation_and_mesh_are_tagged_and_retained() {
        let session = LiveSession::start(fast_config()).unwrap();
        wait_for_mesh(&session, 1);
        let frame = wait_for_frame(&session);
        let mesh = frame.terrain.mesh.as_ref().unwrap();
        assert_eq!(mesh.tag.job_generation, 1);
        assert!(!mesh.quads.is_empty());
        assert_eq!(frame.terrain.seed, 42);
        let report = session.shutdown();
        assert_eq!(report.stats.terrain_jobs_completed, 1);
    }

    #[test]
    fn latest_regeneration_supersedes_earlier_generation_and_discards_stale_mesh() {
        let session = LiveSession::start(LiveSessionConfig {
            generation_iterations: 10_000,
            active_job_capacity: 2,
            ..LiveSessionConfig::default()
        })
        .unwrap();
        let first = session
            .try_command(LiveCommand::RegenerateTerrain { seed: 100 })
            .unwrap();
        let second = session
            .try_command(LiveCommand::RegenerateTerrain { seed: 200 })
            .unwrap();
        assert!(matches!(
            wait_result(&session, first.id).outcome,
            LiveCommandOutcome::Superseded {
                generation: 2,
                by_generation: 3
            }
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if session
                .try_latest_frame()
                .is_some_and(|frame| frame.terrain.generation == 3 && frame.terrain.loading)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "latest generation was not published"
            );
            thread::sleep(Duration::from_millis(2));
        }
        session.request_stop();
        let report = session.shutdown();
        assert!(report.stats.terrain_jobs_stale >= 1 || report.stats.terrain_jobs_cancelled >= 1);
        assert_eq!(report.stats.commands_accepted, 2);
        assert_eq!(report.stats.commands_superseded, 1);
        assert_eq!(report.stats.commands_stopped, 1);
        assert_eq!(report.unread_command_results, 1);
        assert_eq!(report.remaining_command_results[0].id, second.id);
        assert_eq!(
            report.remaining_command_results[0].outcome,
            LiveCommandOutcome::Stopped
        );
    }

    #[test]
    fn command_and_reliable_result_backlogs_are_bounded_without_tick_waiting() {
        let session = LiveSession::start(LiveSessionConfig {
            command_capacity: 1,
            result_capacity: 1,
            generation_iterations: 1,
            ..LiveSessionConfig::default()
        })
        .unwrap();
        let accepted = session
            .try_command(LiveCommand::RegenerateTerrain { seed: 8 })
            .unwrap();
        assert!(matches!(
            session.try_command(LiveCommand::MoveEntity {
                delta: [1.0, 0.0, 0.0]
            }),
            Err(LiveCommandSubmitError::ResultBacklogFull)
        ));
        let frame_before = wait_for_frame(&session);
        thread::sleep(Duration::from_millis(40));
        let frame_after = wait_for_frame(&session);
        assert!(frame_after.tick >= frame_before.tick);
        // Once the one reliable result slot is consumed, bounded admission becomes available.
        let _ = wait_result(&session, accepted.id);
        let second = session
            .try_command(LiveCommand::MoveEntity {
                delta: [1.0, 0.0, 0.0],
            })
            .unwrap();
        let report = session.shutdown();
        assert_eq!(report.unread_command_results, 1);
        assert_eq!(report.remaining_command_results[0].id, second.id);
        assert!(matches!(
            report.remaining_command_results[0].outcome,
            LiveCommandOutcome::Applied { .. } | LiveCommandOutcome::Stopped
        ));
    }

    #[test]
    fn stop_signal_remains_available_when_command_capacity_is_full_and_cancels_work() {
        let session = LiveSession::start(LiveSessionConfig {
            command_capacity: 1,
            result_capacity: 4,
            generation_iterations: 10_000,
            ..LiveSessionConfig::default()
        })
        .unwrap();
        session
            .try_command(LiveCommand::RegenerateTerrain { seed: 9 })
            .unwrap();
        session.request_stop();
        let report = session.shutdown();
        assert!(matches!(
            report.termination,
            super::LiveTermination::StopRequested
        ));
        assert_eq!(report.stats.active_jobs, 0);
        assert!(report.stats.terrain_jobs_cancelled >= 1 || report.stats.terrain_jobs_stale >= 1);
    }

    #[test]
    fn invalid_live_config_is_rejected_before_engine_or_worker_creation() {
        let error = LiveSession::start(LiveSessionConfig {
            chunk_dimensions: ChunkDimensions {
                x: 17,
                y: 16,
                z: 16,
            },
            ..LiveSessionConfig::default()
        })
        .err()
        .unwrap();
        assert!(matches!(error, LiveSessionStartError::InvalidConfig(_)));
        let error = LiveSession::start(LiveSessionConfig {
            generation_iterations: 0,
            ..LiveSessionConfig::default()
        })
        .err()
        .unwrap();
        assert!(matches!(error, LiveSessionStartError::InvalidConfig(_)));
        let error = LiveSession::start(LiveSessionConfig {
            initial_entity_translation: [f32::NAN, 0.0, 0.0],
            ..LiveSessionConfig::default()
        })
        .err()
        .unwrap();
        assert!(matches!(error, LiveSessionStartError::InvalidConfig(_)));
    }

    #[test]
    fn move_deltas_are_bounded_before_admission() {
        let session = LiveSession::start(fast_config()).unwrap();
        assert_eq!(
            session.try_command(LiveCommand::MoveEntity {
                delta: [f32::INFINITY, 0.0, 0.0],
            }),
            Err(LiveCommandSubmitError::InvalidDelta)
        );
        assert_eq!(
            session.try_command(LiveCommand::MoveEntity {
                delta: [100.1, 0.0, 0.0],
            }),
            Err(LiveCommandSubmitError::InvalidDelta)
        );
        let _ = session.shutdown();
    }
}
