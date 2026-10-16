//! Headless simulation runtime with fixed-rate scheduling and independent presentation snapshots.

mod clock;
pub mod live_session;
pub mod project;

use std::time::Duration;

use bevy_ecs::prelude::{IntoScheduleConfigs, Resource, Schedule};
use bevy_ecs::schedule::ExecutorKind;
use bevy_ecs::system::ScheduleSystem;
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};
use daedalus_tasks::{ShutdownError, TaskPool, TaskPoolError};
use daedalus_world::{SimulationWorld, SpawnError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use bevy_ecs;
pub use clock::{ClockAdvance, ClockConfigError, FixedClock, FixedClockConfig};
pub use daedalus_core::{
    CommandId, EntityHandle, RenderEntitySnapshot, RenderSnapshot, Revision, SessionId,
    SnapshotConsumer, SnapshotReadError, Tick, Transform,
};
pub use daedalus_tasks::{
    CancellationToken, SubmitError, TaskCompletion, TaskFailure, TaskHandle, TaskId, TaskOutcome,
    TaskPool as CpuTaskPool,
};
pub use daedalus_world::{
    CommandOutcome, CommandResult, CommandSubmission, CommandSubmitError, RevisionComponent,
    TransformComponent, WorldCommand, WorldCommandKind, WorldReadError,
};
pub use live_session::{
    CommandAccepted, LiveCommand, LiveCommandOutcome, LiveCommandResult, LiveCommandSubmitError,
    LiveCpuBudget, LiveFrame, LiveResultReadError, LiveSession, LiveSessionConfig,
    LiveSessionStartError, LiveSessionStats, LiveShutdownReport, LiveStatusReadError,
    LiveTermination, LiveTerrainFrame,
};
pub use project::{
    ProjectEntityState, ProjectSession, ProjectSessionCommandError, ProjectSessionReadError,
    ProjectSessionReport, ProjectSessionStartError,
};

#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CurrentTick(pub Tick);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub session: SessionId,
    pub command_capacity: usize,
    pub command_dedup_capacity: usize,
    pub clock: FixedClockConfig,
    /// Process-global worker count used by Bevy's parallel schedule executor.
    pub compute_workers: usize,
    /// Per-engine bounded background CPU worker count.
    pub task_workers: usize,
    pub task_capacity: usize,
    pub snapshot_capacity: usize,
    /// Maximum transform entities copied into one owned render snapshot.
    /// Each entry has a fixed `RenderEntitySnapshot` size, so steady-state retained
    /// snapshot entity storage is bounded by `snapshot_capacity * snapshot_entity_capacity`.
    /// Temporary extraction allocations and Arc/Vec metadata add transient overhead.
    pub snapshot_entity_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            session: SessionId::fresh(),
            command_capacity: 256,
            command_dedup_capacity: 1_024,
            clock: FixedClockConfig::default(),
            // Bevy owns this process-wide pool. Leave one logical CPU for the separate
            // background worker, one for the simulation caller, and one for presentation.
            // Very small systems still need one ECS worker; the diagnostics reports the
            // resulting estimate so callers can choose a more suitable explicit budget.
            compute_workers: default_compute_workers(),
            // Each Engine gets a small bounded pool; engine instances do not each create a
            // full-core Rayon pool.
            task_workers: 1,
            task_capacity: 64,
            snapshot_capacity: 3,
            snapshot_entity_capacity: 100_000,
        }
    }
}

fn default_compute_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .saturating_sub(3)
        .max(1)
}

/// Per-engine estimate of the worker threads used while this engine runs.
/// The compute pool is process-global and shared; this estimate does not include
/// other engines, OS threads, or unrelated libraries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CpuBudgetDiagnostics {
    pub logical_cpus: usize,
    pub ecs_compute_workers: usize,
    pub background_workers: usize,
    pub simulation_caller_threads: usize,
    pub presentation_reserve_threads: usize,
    pub estimated_total_threads: usize,
    pub exceeds_logical_cpus: bool,
}

impl CpuBudgetDiagnostics {
    fn new(ecs_compute_workers: usize, background_workers: usize) -> Self {
        let logical_cpus =
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let simulation_caller_threads = 1;
        let presentation_reserve_threads = 1;
        let estimated_total_threads = ecs_compute_workers
            .saturating_add(background_workers)
            .saturating_add(simulation_caller_threads)
            .saturating_add(presentation_reserve_threads);
        Self {
            logical_cpus,
            ecs_compute_workers,
            background_workers,
            simulation_caller_threads,
            presentation_reserve_threads,
            estimated_total_threads,
            exceeds_logical_cpus: estimated_total_threads > logical_cpus,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ComputePoolConfigError {
    #[error("Bevy ECS compute worker count must be positive")]
    NoWorkers,
    #[error("Bevy ECS compute pool already has {actual} workers; requested {requested}")]
    ConflictingProcessBudget { requested: usize, actual: usize },
}

#[derive(Debug, Error)]
pub enum EngineBuildError {
    #[error(transparent)]
    Clock(#[from] ClockConfigError),
    #[error(transparent)]
    TaskPool(#[from] TaskPoolError),
    #[error("engine command, task, snapshot, and snapshot entity capacities must be positive")]
    ZeroCapacity,
    #[error(transparent)]
    ComputePool(#[from] ComputePoolConfigError),
    #[error("invalid Rust gameplay system schedule: {0}")]
    GameplaySchedule(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SnapshotPublication {
    Published { sequence: u64 },
    SkippedAtCapacity,
    SkippedAtEntityLimit { entity_count: usize, limit: usize },
    Busy,
    Poisoned,
    SequenceExhausted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    pub tick: Tick,
    pub commands: Vec<CommandResult>,
    pub snapshot: SnapshotPublication,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine cannot advance because the fixed tick counter is exhausted")]
    TickCounterExhausted,
    #[error(transparent)]
    Shutdown(#[from] ShutdownError),
}

pub struct EngineBuilder {
    config: EngineConfig,
    schedule: Schedule,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new(EngineConfig::default())
    }
}

impl EngineBuilder {
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            schedule: Schedule::default(),
        }
    }

    /// Register Rust gameplay directly as Bevy ECS systems run in Bevy's multi-threaded
    /// schedule. Background CPU work uses the separately-budgeted bounded Rayon pool.
    pub fn add_gameplay_systems<M>(
        &mut self,
        systems: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        self.schedule.add_systems(systems);
        self
    }

    pub fn build(mut self) -> Result<Engine, EngineBuildError> {
        if self.config.command_capacity == 0
            || self.config.task_capacity == 0
            || self.config.snapshot_capacity == 0
            || self.config.snapshot_entity_capacity == 0
        {
            return Err(EngineBuildError::ZeroCapacity);
        }
        let clock = FixedClock::new(self.config.clock)?;
        let tasks = TaskPool::new(self.config.task_workers, self.config.task_capacity)?;
        let background_workers = tasks.worker_count();
        let mut world = SimulationWorld::new(
            self.config.session,
            self.config.command_capacity,
            self.config.command_dedup_capacity,
        );
        world.ecs_mut().insert_resource(CurrentTick(Tick(0)));
        self.schedule.set_executor_kind(ExecutorKind::MultiThreaded);
        self.schedule
            .initialize(world.ecs_mut())
            .map_err(|error| EngineBuildError::GameplaySchedule(format!("{error:?}")))?;
        // Initialize the irreversible process-global compute pool only after all other
        // fallible engine setup succeeds. A bad task or gameplay config must not lock the
        // process into a worker count that a later valid Engine cannot use.
        let compute_workers = configure_compute_pool(self.config.compute_workers)?;
        let (snapshot_publisher, snapshot_consumer) =
            daedalus_core::snapshot_channel(self.config.snapshot_capacity);
        Ok(Engine {
            world,
            schedule: self.schedule,
            clock,
            tasks,
            cpu_budget: CpuBudgetDiagnostics::new(compute_workers, background_workers),
            snapshot_capacity: self.config.snapshot_capacity,
            snapshot_entity_capacity: self.config.snapshot_entity_capacity,
            snapshot_publisher,
            snapshot_consumer,
        })
    }
}

pub struct Engine {
    world: SimulationWorld,
    schedule: Schedule,
    clock: FixedClock,
    tasks: TaskPool,
    cpu_budget: CpuBudgetDiagnostics,
    snapshot_capacity: usize,
    snapshot_entity_capacity: usize,
    snapshot_publisher: daedalus_core::SnapshotPublisher,
    snapshot_consumer: SnapshotConsumer,
}

impl Engine {
    #[must_use]
    pub const fn clock(&self) -> &FixedClock {
        &self.clock
    }

    #[must_use]
    pub const fn world(&self) -> &SimulationWorld {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut SimulationWorld {
        &mut self.world
    }

    #[must_use]
    pub fn tasks(&self) -> &TaskPool {
        &self.tasks
    }

    #[must_use]
    pub const fn cpu_budget(&self) -> CpuBudgetDiagnostics {
        self.cpu_budget
    }

    /// Steady-state upper bound on entity payload bytes across retained snapshots.
    /// Excludes temporary extraction allocations and Arc/Vec metadata.
    #[must_use]
    pub const fn snapshot_payload_budget_bytes(&self) -> usize {
        self.snapshot_capacity
            .saturating_mul(self.snapshot_entity_capacity)
            .saturating_mul(std::mem::size_of::<RenderEntitySnapshot>())
    }

    #[must_use]
    pub fn snapshot_consumer(&self) -> SnapshotConsumer {
        self.snapshot_consumer.clone()
    }

    pub fn spawn_transform(
        &mut self,
        transform: Transform,
    ) -> Result<daedalus_core::EntityHandle, SpawnError> {
        self.world.spawn_transform(transform)
    }

    pub fn try_command(
        &mut self,
        command: WorldCommand,
    ) -> Result<CommandSubmission, CommandSubmitError> {
        self.world.try_command(command, self.clock.current_tick())
    }

    /// Advance one explicit fixed tick and publish the resulting owned snapshot.
    pub fn step_tick(&mut self) -> Result<StepReport, EngineError> {
        let advance = self.clock.step_tick();
        let tick = advance
            .first_tick
            .ok_or(EngineError::TickCounterExhausted)?;
        Ok(self.run_tick(tick))
    }

    /// Run an explicit number of fixed simulation steps for deterministic headless control.
    pub fn run_ticks(&mut self, count: u32) -> Result<Vec<StepReport>, EngineError> {
        (0..count).map(|_| self.step_tick()).collect()
    }

    /// Feed elapsed wall time through the bounded fixed clock; deferred tick debt is retained.
    pub fn advance_elapsed(
        &mut self,
        elapsed: Duration,
    ) -> Result<(ClockAdvance, Vec<StepReport>), EngineError> {
        let previous_tick = self.clock.current_tick();
        let advance = self.clock.advance(elapsed);
        if advance.tick_counter_saturated && advance.executed_ticks == 0 {
            return Err(EngineError::TickCounterExhausted);
        }
        let reports = (1..=advance.executed_ticks)
            .map(|offset| self.run_tick(Tick(previous_tick.0 + u64::from(offset))))
            .collect();
        Ok((advance, reports))
    }

    pub fn shutdown(&self) -> Result<(), EngineError> {
        self.tasks.shutdown()?;
        Ok(())
    }

    fn run_tick(&mut self, tick: Tick) -> StepReport {
        self.world.begin_tick();
        let commands = self.world.apply_due_commands(tick);
        self.world.ecs_mut().insert_resource(CurrentTick(tick));
        self.schedule.run(self.world.ecs_mut());
        self.world.refresh_changed_transform_revisions();
        let entity_count = self.world.render_entity_count();
        let entity_limit = self.snapshot_entity_capacity;
        let world = &mut self.world;
        let publication =
            match self
                .snapshot_publisher
                .try_publish_with(tick, entity_count, entity_limit, || {
                    world.render_entities_bounded(entity_limit)
                }) {
                Ok(daedalus_core::PublicationStatus::Published { sequence }) => {
                    SnapshotPublication::Published { sequence }
                }
                Ok(daedalus_core::PublicationStatus::SkippedAtCapacity) => {
                    SnapshotPublication::SkippedAtCapacity
                }
                Ok(daedalus_core::PublicationStatus::SkippedAtEntityLimit {
                    entity_count,
                    limit,
                }) => SnapshotPublication::SkippedAtEntityLimit {
                    entity_count,
                    limit,
                },
                Err(daedalus_core::SnapshotPublishError::Busy) => SnapshotPublication::Busy,
                Err(daedalus_core::SnapshotPublishError::Poisoned) => SnapshotPublication::Poisoned,
                Err(daedalus_core::SnapshotPublishError::SequenceExhausted) => {
                    SnapshotPublication::SequenceExhausted
                }
            };
        StepReport {
            tick,
            commands,
            snapshot: publication,
        }
    }
}

/// Configure the process-global Bevy schedule pool once. Later engines can reuse
/// the same worker budget; a conflicting request returns an explicit error.
fn configure_compute_pool(requested: usize) -> Result<usize, ComputePoolConfigError> {
    if requested == 0 {
        return Err(ComputePoolConfigError::NoWorkers);
    }
    let pool = ComputeTaskPool::get_or_init(|| {
        TaskPoolBuilder::new()
            .num_threads(requested)
            .thread_name("daedalus-ecs".to_owned())
            .build()
    });
    let actual = pool.thread_num();
    if actual != requested {
        return Err(ComputePoolConfigError::ConflictingProcessBudget { requested, actual });
    }
    Ok(actual)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy_ecs::prelude::{Component, Query, Res};
    use daedalus_core::{CommandId, Revision, Tick, Transform};
    use daedalus_world::{CommandOutcome, WorldCommand, WorldCommandKind};

    use super::{
        ComputePoolConfigError, CurrentTick, EngineBuilder, EngineConfig, FixedClockConfig,
        SnapshotPublication, TransformComponent,
    };

    fn move_transform(mut query: Query<&mut TransformComponent>, tick: Res<CurrentTick>) {
        for mut transform in &mut query {
            transform.0.translation[0] += 1.0;
            let _ = tick.0;
        }
    }

    #[derive(Component)]
    struct NonRenderableMarker;

    #[test]
    fn systems_mutate_actual_ecs_and_unchanged_ticks_do_not_advance_revisions() {
        let mut builder = EngineBuilder::new(EngineConfig {
            clock: FixedClockConfig {
                tick_rate_hz: 60,
                max_catch_up_ticks: 2,
            },
            ..EngineConfig::default()
        });
        builder.add_gameplay_systems(move_transform);
        let mut engine = builder.build().unwrap();
        let entity = engine.spawn_transform(Transform::IDENTITY).unwrap();
        engine.step_tick().unwrap();
        let (transform, revision) = engine.world().transform(entity).unwrap();
        assert_eq!(transform.translation[0], 1.0);
        assert_eq!(revision, Revision(1));

        // Run no mutating system after removing it from a fresh schedule to prove the revision
        // tracker does not treat a fresh Changed query as an unbounded change window.
        let mut still_engine = EngineBuilder::default().build().unwrap();
        let unchanged = still_engine.spawn_transform(Transform::IDENTITY).unwrap();
        still_engine.step_tick().unwrap();
        let (_, initial_revision) = still_engine.world().transform(unchanged).unwrap();
        still_engine.step_tick().unwrap();
        let (_, next_revision) = still_engine.world().transform(unchanged).unwrap();
        assert_eq!(initial_revision, Revision(0));
        assert_eq!(next_revision, Revision(0));
    }

    #[test]
    fn command_ack_revision_matches_world_read_after_tick() {
        let mut engine = EngineBuilder::default().build().unwrap();
        let entity = engine.spawn_transform(Transform::IDENTITY).unwrap();
        let command = WorldCommand {
            command_id: CommandId::from_raw(1),
            target: entity,
            expected_revision: Revision(0),
            apply_at: Tick(1),
            kind: WorldCommandKind::SetTransform(Transform {
                translation: [3.0, 0.0, 0.0],
                ..Transform::IDENTITY
            }),
        };
        engine.try_command(command).unwrap();
        let report = engine.step_tick().unwrap();
        assert_eq!(
            report.commands[0].outcome,
            CommandOutcome::Applied {
                revision: Revision(1)
            }
        );
        assert_eq!(engine.world().transform(entity).unwrap().1, Revision(1));
    }

    #[test]
    fn gameplay_system_mutations_increment_once_per_tick_and_snapshots_progress_while_work_waits() {
        let mut builder = EngineBuilder::default();
        builder.add_gameplay_systems(move_transform);
        let mut engine = builder.build().unwrap();
        let entity = engine.spawn_transform(Transform::IDENTITY).unwrap();

        let (started_send, started_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::channel();
        let _slow = engine
            .tasks()
            .try_submit(move |token| {
                started_send.send(()).unwrap();
                while release_receive.try_recv().is_err() && !token.is_cancelled() {
                    std::thread::yield_now();
                }
                Ok::<_, daedalus_tasks::TaskFailure>(())
            })
            .unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        engine.step_tick().unwrap();
        let first_snapshot = engine.snapshot_consumer().try_latest().unwrap().unwrap();
        engine.step_tick().unwrap();
        let second_snapshot = engine.snapshot_consumer().try_latest().unwrap().unwrap();
        assert_eq!(first_snapshot.tick, Tick(1));
        assert_eq!(second_snapshot.tick, Tick(2));
        assert_eq!(second_snapshot.entities[0].transform.translation[0], 2.0);
        assert_eq!(engine.world().transform(entity).unwrap().1, Revision(2));
        release_send.send(()).unwrap();
        assert!(engine.tasks().wait_idle(Duration::from_secs(2)));
        engine.shutdown().unwrap();
    }

    #[test]
    fn held_snapshot_saturates_publish_without_stalling_simulation_tick() {
        let config = EngineConfig {
            snapshot_capacity: 1,
            ..EngineConfig::default()
        };
        let mut engine = EngineBuilder::new(config).build().unwrap();
        let entity = engine.spawn_transform(Transform::IDENTITY).unwrap();
        engine.step_tick().unwrap();
        let retained = engine.snapshot_consumer().try_latest().unwrap().unwrap();
        let report = engine.step_tick().unwrap();
        assert_eq!(report.tick, Tick(2));
        assert_eq!(report.snapshot, SnapshotPublication::SkippedAtCapacity);
        assert_eq!(engine.world().transform(entity).unwrap().1, Revision(0));
        drop(retained);
        assert!(matches!(
            engine.step_tick().unwrap().snapshot,
            SnapshotPublication::Published { .. }
        ));
    }

    #[test]
    fn over_budget_world_retains_last_snapshot_and_reports_the_entity_limit() {
        let config = EngineConfig {
            snapshot_capacity: 2,
            snapshot_entity_capacity: 1,
            ..EngineConfig::default()
        };
        let mut engine = EngineBuilder::new(config).build().unwrap();
        let first = engine.spawn_transform(Transform::IDENTITY).unwrap();
        assert!(matches!(
            engine.step_tick().unwrap().snapshot,
            SnapshotPublication::Published { .. }
        ));
        let last_snapshot = engine.snapshot_consumer().try_latest().unwrap().unwrap();
        engine.spawn_transform(Transform::IDENTITY).unwrap();
        let report = engine.step_tick().unwrap();
        assert_eq!(
            report.snapshot,
            SnapshotPublication::SkippedAtEntityLimit {
                entity_count: 2,
                limit: 1,
            }
        );
        assert_eq!(
            engine
                .snapshot_consumer()
                .try_latest()
                .unwrap()
                .unwrap()
                .tick,
            last_snapshot.tick
        );
        assert_eq!(last_snapshot.tick, Tick(1));
        assert_eq!(engine.world().transform(first).unwrap().1, Revision(0));
        assert_eq!(
            engine.snapshot_payload_budget_bytes(),
            2 * std::mem::size_of::<daedalus_core::RenderEntitySnapshot>()
        );
    }

    #[test]
    fn non_transform_entities_do_not_consume_render_snapshot_budget() {
        let config = EngineConfig {
            snapshot_entity_capacity: 1,
            ..EngineConfig::default()
        };
        let mut engine = EngineBuilder::new(config).build().unwrap();
        let renderable = engine.spawn_transform(Transform::IDENTITY).unwrap();
        engine.world_mut().ecs_mut().spawn(NonRenderableMarker);
        assert_eq!(engine.world().entity_count(), 2);
        assert_eq!(engine.world_mut().render_entity_count(), 1);

        assert!(matches!(
            engine.step_tick().unwrap().snapshot,
            SnapshotPublication::Published { .. }
        ));
        let snapshot = engine.snapshot_consumer().try_latest().unwrap().unwrap();
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].entity, renderable);
    }

    #[test]
    fn elapsed_clock_keeps_its_bounded_catchup_diagnostics() {
        let mut engine = EngineBuilder::default().build().unwrap();
        let (advance, reports) = engine.advance_elapsed(Duration::from_secs(1)).unwrap();
        assert_eq!(reports.len(), 4);
        assert_eq!(advance.requested_ticks, 60);
        assert_eq!(advance.deferred_ticks, 56);
    }

    #[test]
    fn process_global_compute_budget_is_reported_and_conflicts_are_explicit() {
        let engine = EngineBuilder::default().build().unwrap();
        let budget = engine.cpu_budget();
        assert!(budget.ecs_compute_workers > 0);
        assert!(budget.background_workers > 0);
        assert_eq!(
            budget.estimated_total_threads,
            budget.ecs_compute_workers + budget.background_workers + 2
        );
        let requested = budget.ecs_compute_workers + 1;
        let error = EngineBuilder::new(EngineConfig {
            compute_workers: requested,
            ..EngineConfig::default()
        })
        .build()
        .err()
        .expect("a different process-global compute budget must be rejected");
        assert!(matches!(
            error,
            super::EngineBuildError::ComputePool(
                ComputePoolConfigError::ConflictingProcessBudget {
                    requested: actual_requested,
                    actual: configured,
                }
            ) if actual_requested == requested && configured == budget.ecs_compute_workers
        ));
    }
}
