//! Simulation-owned ECS state, generation-checked handles, and bounded world commands.

use std::collections::{HashMap, HashSet, VecDeque};

use bevy_ecs::entity::{EntityGeneration, EntityIndex};
use bevy_ecs::prelude::{Changed, Component, Entity, World as EcsWorld};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_TRANSFORM_PAGE_ENTITIES: usize = 1_024;

pub use daedalus_core::{
    CommandId, EntityHandle, RenderEntitySnapshot, RenderSnapshot, Revision, SessionId, Tick,
    Transform,
};

#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct TransformComponent(pub Transform);

#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct RevisionComponent(pub Revision);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WorldCommandKind {
    SetTransform(Transform),
    Despawn,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldCommand {
    pub command_id: CommandId,
    pub target: EntityHandle,
    pub expected_revision: Revision,
    pub apply_at: Tick,
    pub kind: WorldCommandKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandError {
    WrongSession,
    StaleEntity,
    MissingTransform,
    MissingRevision,
    InvalidTransform,
    RevisionConflict {
        expected: Revision,
        actual: Revision,
    },
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSession => formatter.write_str("entity handle belongs to another session"),
            Self::StaleEntity => formatter.write_str("entity handle is stale or does not exist"),
            Self::MissingTransform => formatter.write_str("entity has no transform component"),
            Self::MissingRevision => formatter.write_str("entity has no revision component"),
            Self::InvalidTransform => formatter.write_str("transform contains a non-finite value"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "expected revision {}, found {}",
                    expected.0, actual.0
                )
            }
        }
    }
}

impl std::error::Error for CommandError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Applied { revision: Revision },
    Despawned,
    Rejected(CommandError),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    pub command_id: CommandId,
    pub outcome: CommandOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldTransformState {
    pub handle: EntityHandle,
    pub transform: Transform,
    pub revision: Revision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldTransformPage {
    pub total: usize,
    pub offset: usize,
    pub entities: Vec<WorldTransformState>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WorldTransformPageError {
    #[error("transform page limit must be in 1..=1024")]
    InvalidLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandSubmission {
    Enqueued,
    DuplicatePending,
    DuplicateCompleted(CommandResult),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CommandSubmitError {
    #[error("command queue is full")]
    QueueFull,
    #[error("command target tick must be later than the current tick")]
    TargetTickNotFuture,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WorldReadError {
    #[error("entity handle belongs to another session")]
    WrongSession,
    #[error("entity handle is stale or does not exist")]
    StaleEntity,
    #[error("entity has no transform component")]
    MissingTransform,
    #[error("entity has no revision component")]
    MissingRevision,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SpawnError {
    #[error("transform contains a non-finite value")]
    InvalidTransform,
}

/// The authoritative simulation world. Bevy ECS is used as a component, not as a full engine.
pub struct SimulationWorld {
    session: SessionId,
    ecs: EcsWorld,
    command_capacity: usize,
    dedup_capacity: usize,
    pending: VecDeque<WorldCommand>,
    completed: HashMap<CommandId, CommandResult>,
    completed_order: VecDeque<CommandId>,
    revision_bumped_this_tick: HashSet<Entity>,
}

impl SimulationWorld {
    #[must_use]
    pub fn new(session: SessionId, command_capacity: usize, dedup_capacity: usize) -> Self {
        assert!(
            command_capacity > 0,
            "world command capacity must be positive"
        );
        Self {
            session,
            ecs: EcsWorld::new(),
            command_capacity,
            dedup_capacity,
            pending: VecDeque::new(),
            completed: HashMap::new(),
            completed_order: VecDeque::new(),
            revision_bumped_this_tick: HashSet::new(),
        }
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub fn spawn_transform(&mut self, transform: Transform) -> Result<EntityHandle, SpawnError> {
        if !transform.is_finite() {
            return Err(SpawnError::InvalidTransform);
        }
        let entity = self
            .ecs
            .spawn((TransformComponent(transform), RevisionComponent::default()))
            .id();
        Ok(self.handle_for(entity))
    }

    #[must_use]
    pub fn handle_for(&self, entity: Entity) -> EntityHandle {
        EntityHandle {
            session: self.session,
            index: entity.index_u32(),
            generation: entity.generation().to_bits(),
        }
    }

    pub fn transform(&self, handle: EntityHandle) -> Result<(Transform, Revision), WorldReadError> {
        let entity = self.resolve(handle)?;
        let entity = self
            .ecs
            .get_entity(entity)
            .map_err(|_| WorldReadError::StaleEntity)?;
        let transform = entity
            .get::<TransformComponent>()
            .ok_or(WorldReadError::MissingTransform)?
            .0;
        let revision = entity
            .get::<RevisionComponent>()
            .ok_or(WorldReadError::MissingRevision)?
            .0;
        Ok((transform, revision))
    }

    pub fn try_command(
        &mut self,
        command: WorldCommand,
        current_tick: Tick,
    ) -> Result<CommandSubmission, CommandSubmitError> {
        if let Some(result) = self.completed.get(&command.command_id) {
            return Ok(CommandSubmission::DuplicateCompleted(result.clone()));
        }
        if self
            .pending
            .iter()
            .any(|pending| pending.command_id == command.command_id)
        {
            return Ok(CommandSubmission::DuplicatePending);
        }
        if command.apply_at <= current_tick {
            return Err(CommandSubmitError::TargetTickNotFuture);
        }
        if self.pending.len() >= self.command_capacity {
            return Err(CommandSubmitError::QueueFull);
        }
        self.pending.push_back(command);
        Ok(CommandSubmission::Enqueued)
    }

    /// Apply commands whose target tick is due, preserving their accepted queue order.
    pub fn apply_due_commands(&mut self, tick: Tick) -> Vec<CommandResult> {
        let mut due = Vec::new();
        let mut future = VecDeque::with_capacity(self.pending.len());
        while let Some(command) = self.pending.pop_front() {
            if command.apply_at <= tick {
                due.push(command);
            } else {
                future.push_back(command);
            }
        }
        self.pending = future;
        due.into_iter()
            .map(|command| {
                let result = self.apply_command(&command);
                self.cache_result(result.clone());
                result
            })
            .collect()
    }

    /// Start one scheduler-owned simulation tick and clear earlier Bevy change flags.
    pub fn begin_tick(&mut self) {
        self.revision_bumped_this_tick.clear();
        self.ecs.clear_trackers();
    }

    pub fn refresh_changed_transform_revisions(&mut self) {
        let changed_entities: Vec<Entity> = self
            .ecs
            .query_filtered::<Entity, Changed<TransformComponent>>()
            .iter(&self.ecs)
            .collect();
        for entity in changed_entities {
            if self.revision_bumped_this_tick.remove(&entity) {
                continue;
            }
            if let Ok(mut entity) = self.ecs.get_entity_mut(entity)
                && let Some(mut revision) = entity.get_mut::<RevisionComponent>()
            {
                revision.0 = revision.0.next();
            }
        }
        self.revision_bumped_this_tick.clear();
        self.ecs.clear_trackers();
    }

    #[must_use]
    pub fn render_snapshot(&mut self, sequence: u64, tick: Tick) -> RenderSnapshot {
        let entities = self.render_entities();
        RenderSnapshot {
            sequence,
            tick,
            entities: entities.into(),
        }
    }

    #[must_use]
    pub fn render_entities(&mut self) -> Vec<RenderEntitySnapshot> {
        self.render_entities_bounded(usize::MAX)
    }

    /// Extract at most `limit` transforms for bounded inspection responses.
    #[must_use]
    pub fn render_entities_bounded(&mut self, limit: usize) -> Vec<RenderEntitySnapshot> {
        if limit == 0 {
            return Vec::new();
        }
        let session = self.session;
        self.ecs
            .query::<(Entity, &TransformComponent)>()
            .iter(&self.ecs)
            .take(limit)
            .map(|(entity, transform)| RenderEntitySnapshot {
                entity: EntityHandle {
                    session,
                    index: entity.index_u32(),
                    generation: entity.generation().to_bits(),
                },
                transform: transform.0,
            })
            .collect::<Vec<_>>()
    }

    /// Read one bounded transform/revision page without first collecting the full world.
    pub fn transform_page(
        &mut self,
        offset: usize,
        limit: usize,
    ) -> Result<WorldTransformPage, WorldTransformPageError> {
        if limit == 0 || limit > MAX_TRANSFORM_PAGE_ENTITIES {
            return Err(WorldTransformPageError::InvalidLimit);
        }
        let session = self.session;
        let mut query = self
            .ecs
            .query::<(Entity, &TransformComponent, &RevisionComponent)>();
        let total = query.iter(&self.ecs).len();
        let entities = query
            .iter(&self.ecs)
            .skip(offset)
            .take(limit)
            .map(|(entity, transform, revision)| WorldTransformState {
                handle: EntityHandle {
                    session,
                    index: entity.index_u32(),
                    generation: entity.generation().to_bits(),
                },
                transform: transform.0,
                revision: revision.0,
            })
            .collect();
        Ok(WorldTransformPage {
            total,
            offset: offset.min(total),
            entities,
        })
    }

    #[must_use]
    pub fn entity_count(&self) -> usize {
        self.ecs.entities().count_spawned() as usize
    }

    /// Count transform-bearing render entities without allocating a snapshot vector.
    /// Bevy marks this component-only query as archetypal and reports the exact number
    /// of matching rows from its storage ranges, without visiting entity component data.
    #[must_use]
    pub fn render_entity_count(&mut self) -> usize {
        self.ecs
            .query::<&TransformComponent>()
            .iter(&self.ecs)
            .len()
    }

    #[must_use]
    pub fn pending_command_count(&self) -> usize {
        self.pending.len()
    }

    /// Access the simulation ECS for registered Rust gameplay systems.
    pub fn ecs(&self) -> &EcsWorld {
        &self.ecs
    }

    /// Mutable access is intended for a scheduler-owned Rust gameplay system.
    pub fn ecs_mut(&mut self) -> &mut EcsWorld {
        &mut self.ecs
    }

    fn resolve(&self, handle: EntityHandle) -> Result<Entity, WorldReadError> {
        if handle.session != self.session {
            return Err(WorldReadError::WrongSession);
        }
        let index = EntityIndex::from_raw_u32(handle.index).ok_or(WorldReadError::StaleEntity)?;
        let entity = Entity::from_index_and_generation(
            index,
            EntityGeneration::from_bits(handle.generation),
        );
        self.ecs
            .get_entity(entity)
            .map_err(|_| WorldReadError::StaleEntity)?;
        Ok(entity)
    }

    fn apply_command(&mut self, command: &WorldCommand) -> CommandResult {
        let outcome = match self.resolve(command.target) {
            Err(WorldReadError::WrongSession) => {
                CommandOutcome::Rejected(CommandError::WrongSession)
            }
            Err(_) => CommandOutcome::Rejected(CommandError::StaleEntity),
            Ok(entity) => {
                let actual = match self.ecs.get_entity(entity) {
                    Ok(entity_ref) => entity_ref
                        .get::<RevisionComponent>()
                        .map(|revision| revision.0),
                    Err(_) => None,
                };
                match actual {
                    None => CommandOutcome::Rejected(CommandError::MissingRevision),
                    Some(actual) if actual != command.expected_revision => {
                        CommandOutcome::Rejected(CommandError::RevisionConflict {
                            expected: command.expected_revision,
                            actual,
                        })
                    }
                    Some(_) => match &command.kind {
                        WorldCommandKind::SetTransform(transform) if !transform.is_finite() => {
                            CommandOutcome::Rejected(CommandError::InvalidTransform)
                        }
                        WorldCommandKind::SetTransform(transform) => {
                            match self.ecs.get_entity_mut(entity) {
                                Ok(mut entity_mut) => {
                                    if !entity_mut.contains::<TransformComponent>() {
                                        CommandOutcome::Rejected(CommandError::MissingTransform)
                                    } else if !entity_mut.contains::<RevisionComponent>() {
                                        CommandOutcome::Rejected(CommandError::MissingRevision)
                                    } else {
                                        if let Some(mut component) =
                                            entity_mut.get_mut::<TransformComponent>()
                                        {
                                            component.0 = *transform;
                                        }
                                        let mut result_revision = Revision::default();
                                        if let Some(mut revision) =
                                            entity_mut.get_mut::<RevisionComponent>()
                                        {
                                            revision.0 = revision.0.next();
                                            result_revision = revision.0;
                                        }
                                        self.revision_bumped_this_tick.insert(entity);
                                        CommandOutcome::Applied {
                                            revision: result_revision,
                                        }
                                    }
                                }
                                Err(_) => CommandOutcome::Rejected(CommandError::StaleEntity),
                            }
                        }
                        WorldCommandKind::Despawn => {
                            if self.ecs.despawn(entity) {
                                CommandOutcome::Despawned
                            } else {
                                CommandOutcome::Rejected(CommandError::StaleEntity)
                            }
                        }
                    },
                }
            }
        };
        CommandResult {
            command_id: command.command_id,
            outcome,
        }
    }

    fn cache_result(&mut self, result: CommandResult) {
        if self.dedup_capacity == 0 {
            return;
        }
        let command_id = result.command_id;
        self.completed.insert(command_id, result);
        self.completed_order.push_back(command_id);
        while self.completed_order.len() > self.dedup_capacity {
            if let Some(expired) = self.completed_order.pop_front() {
                self.completed.remove(&expired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CommandError, CommandOutcome, CommandSubmission, CommandSubmitError,
        MAX_TRANSFORM_PAGE_ENTITIES, SimulationWorld, TransformComponent, WorldCommand,
        WorldCommandKind, WorldTransformPageError,
    };
    use daedalus_core::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};

    fn command(
        id: u128,
        target: EntityHandle,
        expected_revision: u64,
        tick: u64,
        kind: WorldCommandKind,
    ) -> WorldCommand {
        WorldCommand {
            command_id: CommandId::from_raw(id),
            target,
            expected_revision: Revision(expected_revision),
            apply_at: Tick(tick),
            kind,
        }
    }

    #[test]
    fn transform_pages_bound_collection_and_report_total_offset() {
        let mut world = SimulationWorld::new(SessionId::from_raw(25), 8, 8);
        let handles = (0..3)
            .map(|index| {
                world
                    .spawn_transform(Transform {
                        translation: [index as f32, 0.0, 0.0],
                        ..Transform::IDENTITY
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let page = world.transform_page(2, 1).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.offset, 2);
        assert_eq!(page.entities.len(), 1);
        assert!(handles.contains(&page.entities[0].handle));
        assert_eq!(
            world.transform_page(0, MAX_TRANSFORM_PAGE_ENTITIES + 1),
            Err(WorldTransformPageError::InvalidLimit)
        );
        assert_eq!(
            world.transform_page(0, 0),
            Err(WorldTransformPageError::InvalidLimit)
        );
    }

    #[test]
    fn stale_session_and_reused_entity_generation_are_rejected() {
        let mut world = SimulationWorld::new(SessionId::from_raw(5), 8, 8);
        let stale = world.spawn_transform(Transform::IDENTITY).unwrap();
        world
            .try_command(command(1, stale, 0, 1, WorldCommandKind::Despawn), Tick(0))
            .unwrap();
        assert_eq!(
            world.apply_due_commands(Tick(1))[0].outcome,
            CommandOutcome::Despawned
        );
        let replacement = world.spawn_transform(Transform::IDENTITY).unwrap();
        assert_eq!(stale.index, replacement.index);
        assert_ne!(stale.generation, replacement.generation);
        world
            .try_command(command(2, stale, 0, 2, WorldCommandKind::Despawn), Tick(1))
            .unwrap();
        let stale_result = world.apply_due_commands(Tick(2)).remove(0);
        assert_eq!(
            stale_result.outcome,
            CommandOutcome::Rejected(CommandError::StaleEntity)
        );

        let wrong_session = EntityHandle {
            session: SessionId::from_raw(999),
            ..replacement
        };
        world
            .try_command(
                command(3, wrong_session, 0, 3, WorldCommandKind::Despawn),
                Tick(2),
            )
            .unwrap();
        assert_eq!(
            world.apply_due_commands(Tick(3))[0].outcome,
            CommandOutcome::Rejected(CommandError::WrongSession)
        );
    }

    #[test]
    fn revision_conflicts_are_rejected_and_command_ids_are_deduplicated() {
        let mut world = SimulationWorld::new(SessionId::from_raw(1), 8, 8);
        let entity = world.spawn_transform(Transform::IDENTITY).unwrap();
        let first = command(
            7,
            entity,
            0,
            1,
            WorldCommandKind::SetTransform(Transform {
                translation: [1.0, 0.0, 0.0],
                ..Transform::IDENTITY
            }),
        );
        assert_eq!(
            world.try_command(first.clone(), Tick(0)),
            Ok(CommandSubmission::Enqueued)
        );
        assert_eq!(
            world.try_command(first, Tick(0)),
            Ok(CommandSubmission::DuplicatePending)
        );
        let result = world.apply_due_commands(Tick(1)).remove(0);
        assert_eq!(
            result.outcome,
            CommandOutcome::Applied {
                revision: Revision(1)
            }
        );
        assert_eq!(
            world.try_command(command(7, entity, 0, 2, WorldCommandKind::Despawn), Tick(1)),
            Ok(CommandSubmission::DuplicateCompleted(result))
        );
        world
            .try_command(command(8, entity, 0, 2, WorldCommandKind::Despawn), Tick(1))
            .unwrap();
        assert_eq!(
            world.apply_due_commands(Tick(2))[0].outcome,
            CommandOutcome::Rejected(CommandError::RevisionConflict {
                expected: Revision(0),
                actual: Revision(1),
            })
        );
    }

    #[test]
    fn command_queue_saturation_and_future_tick_validation_are_explicit() {
        let mut world = SimulationWorld::new(SessionId::from_raw(1), 1, 1);
        let entity = world.spawn_transform(Transform::IDENTITY).unwrap();
        let first = command(1, entity, 0, 2, WorldCommandKind::Despawn);
        assert_eq!(
            world.try_command(first, Tick(0)),
            Ok(CommandSubmission::Enqueued)
        );
        assert_eq!(
            world.try_command(command(2, entity, 0, 2, WorldCommandKind::Despawn), Tick(0)),
            Err(CommandSubmitError::QueueFull)
        );
        assert_eq!(
            world.try_command(command(3, entity, 0, 0, WorldCommandKind::Despawn), Tick(0)),
            Err(CommandSubmitError::TargetTickNotFuture)
        );
        assert_eq!(world.pending_command_count(), 1);
    }

    #[test]
    fn non_finite_transform_is_rejected_before_entering_the_world() {
        let mut world = SimulationWorld::new(SessionId::from_raw(9), 4, 4);
        assert!(
            world
                .spawn_transform(Transform {
                    translation: [f32::NAN, 0.0, 0.0],
                    ..Transform::IDENTITY
                })
                .is_err()
        );
        let entity = world.spawn_transform(Transform::IDENTITY).unwrap();
        let invalid = WorldCommandKind::SetTransform(Transform {
            translation: [f32::INFINITY, 0.0, 0.0],
            ..Transform::IDENTITY
        });
        world
            .try_command(command(40, entity, 0, 1, invalid), Tick(0))
            .unwrap();
        assert_eq!(
            world.apply_due_commands(Tick(1))[0].outcome,
            CommandOutcome::Rejected(CommandError::InvalidTransform)
        );
        assert_eq!(
            world.transform(entity).unwrap(),
            (Transform::IDENTITY, Revision(0))
        );
    }

    #[test]
    fn gameplay_mutations_advance_component_revisions_and_publish_transforms() {
        let mut world = SimulationWorld::new(SessionId::from_raw(2), 8, 8);
        let handle = world.spawn_transform(Transform::IDENTITY).unwrap();
        world.begin_tick();
        let entity = bevy_ecs::entity::Entity::from_raw_u32(handle.index).unwrap();
        world
            .ecs_mut()
            .get_mut::<TransformComponent>(entity)
            .unwrap()
            .0
            .translation[1] = 4.0;
        world.refresh_changed_transform_revisions();
        let (transform, revision) = world.transform(handle).unwrap();
        assert_eq!(transform.translation, [0.0, 4.0, 0.0]);
        assert_eq!(revision, Revision(1));
        let snapshot = world.render_snapshot(1, Tick(1));
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].entity, handle);
        assert_eq!(snapshot.entities[0].transform, transform);
    }

    #[test]
    fn hundred_thousand_entities_keep_unique_handles_and_real_components() {
        let mut world = SimulationWorld::new(SessionId::from_raw(44), 4, 4);
        let mut selected = Vec::new();
        for index in 0..100_000_u32 {
            let handle = world
                .spawn_transform(Transform {
                    translation: [index as f32, 0.0, 0.0],
                    ..Transform::IDENTITY
                })
                .unwrap();
            if index % 25_000 == 0 {
                selected.push((index, handle));
            }
        }
        assert_eq!(world.entity_count(), 100_000);
        for (index, handle) in selected {
            let (transform, revision) = world.transform(handle).unwrap();
            assert_eq!(transform.translation[0], index as f32);
            assert_eq!(revision, Revision(0));
        }
        let snapshot = world.render_snapshot(1, Tick(0));
        assert_eq!(snapshot.entities.len(), 100_000);
    }
}
