//! A validated authored project loaded into one authoritative runtime engine session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use daedalus_content::project::{PreparedProject, ProjectError};
use daedalus_content::scene::StableId;
use daedalus_core::{
    CommandId, EntityHandle, RenderSnapshot, Revision, SessionId, SnapshotReadError, Tick,
    Transform,
};
use daedalus_world::{
    CommandSubmission, CommandSubmitError, SpawnError, WorldCommand, WorldCommandKind,
    WorldReadError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_PROJECT_SESSION_ENTITIES: usize = 16_384;

use crate::{Engine, EngineBuildError, EngineBuilder, EngineConfig, EngineError, StepReport};

/// The authoritative runtime state associated with one authored stable entity ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectEntityState {
    pub id: StableId,
    pub handle: EntityHandle,
    pub transform: Transform,
    pub revision: Revision,
}

/// A serializable view of the project source identity and current ECS state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectSessionReport {
    pub project_name: String,
    pub session: SessionId,
    pub source_schema_version: u32,
    pub source_scene_revision: u64,
    pub source_digest: String,
    pub tick: Tick,
    pub entities: Vec<ProjectEntityState>,
}

/// Errors found before an Engine is created and the process-global ECS pool is initialized.
#[derive(Debug, Error)]
pub enum ProjectSessionStartError {
    #[error(transparent)]
    InvalidProject(#[from] ProjectError),
    #[error(
        "prepared project has {prepared_count} runtime entities for {source_count} source entities"
    )]
    EntityProjectionCount {
        source_count: usize,
        prepared_count: usize,
    },
    #[error("prepared project runtime entity IDs do not match its source scene IDs")]
    EntityProjectionIds,
    #[error("prepared project has {0} entities; runtime session cap is 16384")]
    EntityLimit(usize),
    #[error("prepared project entity '{stable_id}' has a non-finite runtime transform")]
    InvalidRuntimeTransform { stable_id: StableId },
    #[error(transparent)]
    Engine(#[from] EngineBuildError),
    #[error("could not create runtime entity '{stable_id}': {source}")]
    Spawn {
        stable_id: StableId,
        #[source]
        source: SpawnError,
    },
}

/// Errors while reading current project entity state from its authoritative ECS world.
#[derive(Debug, Error)]
pub enum ProjectSessionReadError {
    #[error("authored entity '{stable_id}' no longer has readable runtime state: {source}")]
    Entity {
        stable_id: StableId,
        #[source]
        source: WorldReadError,
    },
}

/// Errors while resolving an authored stable ID into the ordinary world command path.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProjectSessionCommandError {
    #[error("authored entity '{0}' is not part of this project session")]
    UnknownEntity(StableId),
    #[error(transparent)]
    Rejected(#[from] CommandSubmitError),
}

/// Owns one prepared project, one real Engine and the authored-ID mapping for that session.
pub struct ProjectSession {
    project: Arc<PreparedProject>,
    engine: Engine,
    entity_handles: BTreeMap<StableId, EntityHandle>,
    stable_ids_by_handle: HashMap<EntityHandle, StableId>,
}

impl ProjectSession {
    /// Validate the complete prepared projection before building an Engine or spawning entities.
    pub fn new(
        prepared: PreparedProject,
        config: EngineConfig,
    ) -> Result<Self, ProjectSessionStartError> {
        Self::new_shared(Arc::new(prepared), config)
    }

    /// Construct a session that shares the caller's immutable prepared source without cloning it.
    pub fn new_shared(
        prepared: Arc<PreparedProject>,
        config: EngineConfig,
    ) -> Result<Self, ProjectSessionStartError> {
        prepared.validate()?;
        if prepared.entities().len() > MAX_PROJECT_SESSION_ENTITIES {
            return Err(ProjectSessionStartError::EntityLimit(
                prepared.entities().len(),
            ));
        }
        let source_ids: BTreeSet<_> = prepared
            .scene()
            .entities
            .iter()
            .map(|entity| entity.id.clone())
            .collect();
        let prepared_ids: BTreeSet<_> = prepared
            .entities()
            .iter()
            .map(|entity| entity.id.clone())
            .collect();
        if prepared.entities().len() != prepared.scene().entities.len() {
            return Err(ProjectSessionStartError::EntityProjectionCount {
                source_count: prepared.scene().entities.len(),
                prepared_count: prepared.entities().len(),
            });
        }
        if source_ids != prepared_ids {
            return Err(ProjectSessionStartError::EntityProjectionIds);
        }

        // Convert and validate all spawn data before EngineBuilder can initialize Bevy's
        // irreversible process-global compute pool. Content validation also checks quaternion,
        // scale, component, mesh and source-file semantics.
        let spawn_data = prepared
            .entities()
            .iter()
            .map(|entity| {
                let transform = Transform {
                    translation: entity.transform.translation,
                    rotation: entity.transform.rotation,
                    scale: entity.transform.scale,
                };
                if !transform.is_finite() {
                    return Err(ProjectSessionStartError::InvalidRuntimeTransform {
                        stable_id: entity.id.clone(),
                    });
                }
                Ok((entity.id.clone(), transform))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut engine = EngineBuilder::new(config).build()?;
        let mut entity_handles = BTreeMap::new();
        let mut stable_ids_by_handle = HashMap::new();
        for (stable_id, transform) in spawn_data {
            let handle = engine.spawn_transform(transform).map_err(|source| {
                ProjectSessionStartError::Spawn {
                    stable_id: stable_id.clone(),
                    source,
                }
            })?;
            stable_ids_by_handle.insert(handle, stable_id.clone());
            entity_handles.insert(stable_id, handle);
        }

        Ok(Self {
            project: prepared,
            engine,
            entity_handles,
            stable_ids_by_handle,
        })
    }

    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    #[must_use]
    pub fn project(&self) -> &PreparedProject {
        self.project.as_ref()
    }

    /// Replaces only the immutable source binding while preserving the live Engine and entity map.
    /// The caller must have completed compatibility checks and reserved worker retirement before
    /// invoking this transaction.
    pub(crate) fn replace_project_source(
        &mut self,
        project: Arc<PreparedProject>,
    ) -> Arc<PreparedProject> {
        std::mem::replace(&mut self.project, project)
    }

    #[must_use]
    pub fn entity_handle(&self, stable_id: &StableId) -> Option<EntityHandle> {
        self.entity_handles.get(stable_id).copied()
    }

    #[must_use]
    pub fn stable_id_for_handle(&self, handle: EntityHandle) -> Option<&StableId> {
        self.stable_ids_by_handle.get(&handle)
    }

    /// Read stable IDs, handles, transforms and revisions from the current ECS world.
    pub fn entities(&self) -> Result<Vec<ProjectEntityState>, ProjectSessionReadError> {
        self.entity_handles
            .iter()
            .map(|(stable_id, handle)| {
                let (transform, revision) =
                    self.engine.world().transform(*handle).map_err(|source| {
                        ProjectSessionReadError::Entity {
                            stable_id: stable_id.clone(),
                            source,
                        }
                    })?;
                Ok(ProjectEntityState {
                    id: stable_id.clone(),
                    handle: *handle,
                    transform,
                    revision,
                })
            })
            .collect()
    }

    /// Build a report from the authoritative ECS while keeping source and runtime revisions distinct.
    pub fn report(&self) -> Result<ProjectSessionReport, ProjectSessionReadError> {
        Ok(ProjectSessionReport {
            project_name: self.project.manifest().name.clone(),
            session: self.engine.world().session(),
            source_schema_version: self.project.scene().schema_version,
            source_scene_revision: self.project.scene().revision,
            source_digest: self.project.digest().to_owned(),
            tick: self.engine.clock().current_tick(),
            entities: self.entities()?,
        })
    }

    /// Submit a transform update through the standard expected-revision/apply-tick command path.
    pub fn submit_transform(
        &mut self,
        stable_id: &StableId,
        command_id: CommandId,
        expected_revision: Revision,
        apply_at: Tick,
        transform: Transform,
    ) -> Result<CommandSubmission, ProjectSessionCommandError> {
        let target = self
            .entity_handle(stable_id)
            .ok_or_else(|| ProjectSessionCommandError::UnknownEntity(stable_id.clone()))?;
        self.engine
            .try_command(WorldCommand {
                command_id,
                target,
                expected_revision,
                apply_at,
                kind: WorldCommandKind::SetTransform(transform),
            })
            .map_err(Into::into)
    }

    /// Advance the same owned Engine by one fixed simulation tick.
    pub fn step_tick(&mut self) -> Result<StepReport, EngineError> {
        self.engine.step_tick()
    }

    /// Read the latest immutable snapshot published by this session's Engine.
    pub fn latest_snapshot(&self) -> Result<Option<Arc<RenderSnapshot>>, SnapshotReadError> {
        self.engine.snapshot_consumer().try_latest()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use daedalus_content::project::{ProjectLimits, prepare_project_from_bytes};
    use daedalus_world::{CommandError, CommandOutcome};

    use super::{PreparedProject, ProjectSession};
    use crate::{CommandId, EngineConfig, Revision, Tick, Transform};

    fn prepared_project(scene: &str) -> PreparedProject {
        let manifest = br#"{"schema_version":1,"name":"runtime fixture","scene":"scene.json"}"#;
        prepare_project_from_bytes(
            manifest,
            scene.as_bytes(),
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .expect("prepare test project")
    }

    const ONE_TRANSFORM_SCENE: &str = r#"{
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
                    "scale": [1.0, 2.0, 1.0]
                }
            }]
        }],
        "assets": []
    }"#;

    #[test]
    fn prepared_entities_project_into_the_actual_engine_world() {
        let session = ProjectSession::new(
            prepared_project(ONE_TRANSFORM_SCENE),
            EngineConfig::default(),
        )
        .unwrap();

        let report = session.report().unwrap();
        assert_eq!(report.source_schema_version, 1);
        assert_eq!(report.source_scene_revision, 37);
        assert_eq!(report.tick, Tick(0));
        assert_eq!(report.entities.len(), 1);
        assert_eq!(session.engine().world().entity_count(), 1);
        assert_eq!(
            report.entities[0].transform,
            Transform {
                translation: [2.0, 3.0, 4.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 2.0, 1.0],
            }
        );
        assert_eq!(report.entities[0].revision, Revision(0));
        assert_eq!(
            session.entity_handle(&report.entities[0].id),
            Some(report.entities[0].handle)
        );
    }

    #[test]
    fn shared_constructor_keeps_the_callers_prepared_project_arc() {
        let prepared = Arc::new(prepared_project(ONE_TRANSFORM_SCENE));
        let session =
            ProjectSession::new_shared(Arc::clone(&prepared), EngineConfig::default()).unwrap();

        assert!(std::ptr::eq(session.project(), prepared.as_ref()));
        assert_eq!(Arc::strong_count(&prepared), 2);
        assert!(session.latest_snapshot().unwrap().is_none());
    }

    #[test]
    fn stable_id_commands_follow_tick_snapshot_and_revision_paths() {
        let mut session = ProjectSession::new(
            prepared_project(ONE_TRANSFORM_SCENE),
            EngineConfig::default(),
        )
        .unwrap();
        let stable_id = session.project().entities()[0].id.clone();
        let moved = Transform {
            translation: [9.0, 8.0, 7.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
        };

        assert!(session.latest_snapshot().unwrap().is_none());
        session
            .submit_transform(
                &stable_id,
                CommandId::from_raw(701),
                Revision(0),
                Tick(1),
                moved,
            )
            .unwrap();
        let step = session.step_tick().unwrap();
        assert_eq!(step.tick, Tick(1));
        assert_eq!(
            step.commands[0].outcome,
            CommandOutcome::Applied {
                revision: Revision(1)
            }
        );

        let snapshot = session.latest_snapshot().unwrap().unwrap();
        assert_eq!(snapshot.tick, Tick(1));
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(
            snapshot.entities[0].entity,
            session.entity_handle(&stable_id).unwrap()
        );
        assert_eq!(snapshot.entities[0].transform, moved);
        let report = session.report().unwrap();
        assert_eq!(report.source_scene_revision, 37);
        assert_eq!(report.entities[0].revision, Revision(1));
        assert_eq!(report.entities[0].transform, moved);
    }

    #[test]
    fn stale_revision_is_rejected_by_the_authoritative_world() {
        let mut session = ProjectSession::new(
            prepared_project(ONE_TRANSFORM_SCENE),
            EngineConfig::default(),
        )
        .unwrap();
        let stable_id = session.project().entities()[0].id.clone();
        let original = session.report().unwrap().entities[0].transform;
        session
            .submit_transform(
                &stable_id,
                CommandId::from_raw(702),
                Revision(8),
                Tick(1),
                Transform {
                    translation: [100.0, 0.0, 0.0],
                    ..Transform::IDENTITY
                },
            )
            .unwrap();

        let step = session.step_tick().unwrap();
        assert_eq!(
            step.commands[0].outcome,
            CommandOutcome::Rejected(CommandError::RevisionConflict {
                expected: Revision(8),
                actual: Revision(0),
            })
        );
        let entity = &session.report().unwrap().entities[0];
        assert_eq!(entity.transform, original);
        assert_eq!(entity.revision, Revision(0));
    }
}
