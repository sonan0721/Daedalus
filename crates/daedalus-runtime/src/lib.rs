//! Headless simulation runtime with fixed-rate scheduling and independent presentation snapshots.

mod clock;

use std::time::Duration;

use bevy_ecs::prelude::{IntoScheduleConfigs, Resource, Schedule};
use bevy_ecs::schedule::ExecutorKind;
use bevy_ecs::system::ScheduleSystem;
use daedalus_core::{SessionId, Tick, Transform};
use daedalus_tasks::{ShutdownError, TaskPool, TaskPoolError};
use daedalus_world::{
    CommandResult, CommandSubmission, CommandSubmitError, SimulationWorld, SpawnError, WorldCommand,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use bevy_ecs;
pub use clock::{ClockAdvance, ClockConfigError, FixedClock, FixedClockConfig};
pub use daedalus_core::{RenderEntitySnapshot, RenderSnapshot, SnapshotConsumer};
pub use daedalus_tasks::{
    CancellationToken, SubmitError, TaskCompletion, TaskFailure, TaskHandle, TaskId, TaskOutcome,
    TaskPool as CpuTaskPool,
};
pub use daedalus_world::{RevisionComponent, TransformComponent};

#[derive(Resource, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CurrentTick(pub Tick);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub session: SessionId,
    pub command_capacity: usize,
    pub command_dedup_capacity: usize,
    pub clock: FixedClockConfig,
    pub task_workers: usize,
    pub task_capacity: usize,
    pub snapshot_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            session: SessionId::fresh(),
            command_capacity: 256,
            command_dedup_capacity: 1_024,
            clock: FixedClockConfig::default(),
            // E01 uses one bounded CPU worker by default. A caller can choose a larger pool
            // after measuring its process-wide budget; multiple engine instances do not each
            // silently create a full-core Rayon pool.
            task_workers: 1,
            task_capacity: 64,
            snapshot_capacity: 3,
        }
    }
}

#[derive(Debug, Error)]
pub enum EngineBuildError {
    #[error(transparent)]
    Clock(#[from] ClockConfigError),
    #[error(transparent)]
    TaskPool(#[from] TaskPoolError),
    #[error("engine command, task, and snapshot capacities must be positive")]
    ZeroCapacity,
    #[error("invalid Rust gameplay system schedule: {0}")]
    GameplaySchedule(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SnapshotPublication {
    Published { sequence: u64 },
    SkippedAtCapacity,
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

    /// Register Rust gameplay directly as Bevy ECS systems run inside the single-threaded E01
    /// simulation schedule. Background CPU work uses the bounded Rayon pool separately.
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
        {
            return Err(EngineBuildError::ZeroCapacity);
        }
        let clock = FixedClock::new(self.config.clock)?;
        let tasks = TaskPool::new(self.config.task_workers, self.config.task_capacity)?;
        let mut world = SimulationWorld::new(
            self.config.session,
            self.config.command_capacity,
            self.config.command_dedup_capacity,
        );
        world.ecs_mut().insert_resource(CurrentTick(Tick(0)));
        self.schedule
            .set_executor_kind(ExecutorKind::SingleThreaded);
        self.schedule
            .initialize(world.ecs_mut())
            .map_err(|error| EngineBuildError::GameplaySchedule(format!("{error:?}")))?;
        let (snapshot_publisher, snapshot_consumer) =
            daedalus_core::snapshot_channel(self.config.snapshot_capacity);
        Ok(Engine {
            world,
            schedule: self.schedule,
            clock,
            tasks,
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
        let publication = match self
            .snapshot_publisher
            .try_publish(tick, self.world.render_entities())
        {
            Ok(daedalus_core::PublicationStatus::Published { sequence }) => {
                SnapshotPublication::Published { sequence }
            }
            Ok(daedalus_core::PublicationStatus::SkippedAtCapacity) => {
                SnapshotPublication::SkippedAtCapacity
            }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy_ecs::prelude::{Query, Res};
    use daedalus_core::{CommandId, Revision, Tick, Transform};
    use daedalus_world::{CommandOutcome, WorldCommand, WorldCommandKind};

    use super::{
        CurrentTick, EngineBuilder, EngineConfig, FixedClockConfig, SnapshotPublication,
        TransformComponent,
    };

    fn move_transform(mut query: Query<&mut TransformComponent>, tick: Res<CurrentTick>) {
        for mut transform in &mut query {
            transform.0.translation[0] += 1.0;
            let _ = tick.0;
        }
    }

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
    fn elapsed_clock_keeps_its_bounded_catchup_diagnostics() {
        let mut engine = EngineBuilder::default().build().unwrap();
        let (advance, reports) = engine.advance_elapsed(Duration::from_secs(1)).unwrap();
        assert_eq!(reports.len(), 4);
        assert_eq!(advance.requested_ticks, 60);
        assert_eq!(advance.deferred_ticks, 56);
    }
}
