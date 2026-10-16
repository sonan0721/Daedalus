//! Bounded projection of prepared external projects and one authoritative engine snapshot.

use std::collections::{BTreeMap, HashMap};

use daedalus_content::project::PreparedProject;
use daedalus_content::scene::StableId;
use daedalus_core::{EntityHandle, RenderSnapshot, SessionId, Tick};
use glam::{Mat4, Quat, Vec3};
use thiserror::Error;

use crate::{Camera, DirectionalLight, Material, Mesh, RenderObject, RenderScene, Vertex};

/// Hard bounds for copying project CPU geometry into an owned render scene.
/// Expanded budgets count each instance separately, including repeated mesh assets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectProjectionLimits {
    pub max_entities: usize,
    pub max_mesh_assets: usize,
    pub max_mesh_vertices: usize,
    pub max_mesh_indices: usize,
    pub max_expanded_vertices: usize,
    pub max_expanded_indices: usize,
    pub max_expanded_geometry_bytes: usize,
}

impl Default for ProjectProjectionLimits {
    fn default() -> Self {
        Self {
            max_entities: 16_384,
            max_mesh_assets: 256,
            max_mesh_vertices: 1_000_000,
            max_mesh_indices: 3_000_000,
            max_expanded_vertices: 1_000_000,
            max_expanded_indices: 3_000_000,
            max_expanded_geometry_bytes: 64 * 1024 * 1024,
        }
    }
}

impl ProjectProjectionLimits {
    fn validate(self) -> Result<(), ProjectProjectionError> {
        let hard = Self::default();
        if self.max_entities == 0
            || self.max_mesh_assets == 0
            || self.max_mesh_vertices == 0
            || self.max_mesh_indices == 0
            || self.max_expanded_vertices == 0
            || self.max_expanded_indices == 0
            || self.max_expanded_geometry_bytes == 0
            || self.max_entities > hard.max_entities
            || self.max_mesh_assets > hard.max_mesh_assets
            || self.max_mesh_vertices > hard.max_mesh_vertices
            || self.max_mesh_indices > hard.max_mesh_indices
            || self.max_expanded_vertices > hard.max_expanded_vertices
            || self.max_expanded_indices > hard.max_expanded_indices
            || self.max_expanded_geometry_bytes > hard.max_expanded_geometry_bytes
        {
            return Err(ProjectProjectionError::InvalidLimits);
        }
        Ok(())
    }
}

/// Project scene copied from validated CPU assets and one immutable render snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectProjection {
    pub scene: RenderScene,
    pub session: SessionId,
    pub snapshot_tick: Tick,
    pub object_count: usize,
    pub expanded_vertices: usize,
    pub expanded_indices: usize,
    pub expanded_geometry_bytes: usize,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectProjectionError {
    #[error("project projection limits must all be positive")]
    InvalidLimits,
    #[error("project has {found} entities, exceeding projection limit {limit}")]
    EntityLimit { found: usize, limit: usize },
    #[error("project has {found} mesh assets, exceeding projection limit {limit}")]
    MeshAssetLimit { found: usize, limit: usize },
    #[error("snapshot has {found} entities, exceeding projection limit {limit}")]
    SnapshotEntityLimit { found: usize, limit: usize },
    #[error("authored map has {bindings} stable IDs for {entities} prepared entities")]
    BindingCount { entities: usize, bindings: usize },
    #[error("authored entities '{first}' and '{second}' share runtime handle {handle:?}")]
    DuplicateBinding {
        first: StableId,
        second: StableId,
        handle: EntityHandle,
    },
    #[error("authored entity '{stable_id}' has no runtime handle binding")]
    MissingBinding { stable_id: StableId },
    #[error(
        "authored entity '{stable_id}' is bound to session {actual}, expected session {expected}"
    )]
    BindingWrongSession {
        stable_id: StableId,
        expected: u64,
        actual: u64,
    },
    #[error("snapshot entity belongs to session {actual}, expected session {expected}")]
    SnapshotWrongSession { expected: u64, actual: u64 },
    #[error("snapshot contains duplicate entity handle {0:?}")]
    DuplicateSnapshotEntity(EntityHandle),
    #[error("authored entity '{stable_id}' has no matching entity in snapshot")]
    MissingSnapshotEntity { stable_id: StableId },
    #[error(
        "authored entity '{stable_id}' is stale: expected generation {expected}, snapshot has {actual}"
    )]
    StaleEntity {
        stable_id: StableId,
        expected: u32,
        actual: u32,
    },
    #[error("authored entity '{stable_id}' references missing mesh '{mesh_id}'")]
    MissingMesh {
        stable_id: StableId,
        mesh_id: StableId,
    },
    #[error("mesh '{mesh_id}' has {found} vertices, exceeding projection limit {limit}")]
    MeshVertexLimit {
        mesh_id: StableId,
        found: usize,
        limit: usize,
    },
    #[error("mesh '{mesh_id}' has {found} indices, exceeding projection limit {limit}")]
    MeshIndexLimit {
        mesh_id: StableId,
        found: usize,
        limit: usize,
    },
    #[error("expanded scene exceeds the {budget} projection budget")]
    GeometryBudget { budget: &'static str },
    #[error("projected scene is invalid: {0}")]
    InvalidScene(String),
}

/// Project a prepared project using only handle bindings from its owning runtime session.
/// Render transforms come from `snapshot`, never from authored or report-side state.
pub fn project_snapshot(
    prepared: &PreparedProject,
    expected_session: SessionId,
    authored_handles: &BTreeMap<StableId, EntityHandle>,
    snapshot: &RenderSnapshot,
) -> Result<ProjectProjection, ProjectProjectionError> {
    project_snapshot_with_limits(
        prepared,
        expected_session,
        authored_handles,
        snapshot,
        ProjectProjectionLimits::default(),
    )
}

/// Limit-configurable form used by hosts with tighter presentation memory budgets.
pub fn project_snapshot_with_limits(
    prepared: &PreparedProject,
    expected_session: SessionId,
    authored_handles: &BTreeMap<StableId, EntityHandle>,
    snapshot: &RenderSnapshot,
    limits: ProjectProjectionLimits,
) -> Result<ProjectProjection, ProjectProjectionError> {
    limits.validate()?;

    let entities = prepared.entities();
    let meshes = prepared.meshes();
    if entities.len() > limits.max_entities {
        return Err(ProjectProjectionError::EntityLimit {
            found: entities.len(),
            limit: limits.max_entities,
        });
    }
    if meshes.len() > limits.max_mesh_assets {
        return Err(ProjectProjectionError::MeshAssetLimit {
            found: meshes.len(),
            limit: limits.max_mesh_assets,
        });
    }
    if snapshot.entities.len() > limits.max_entities {
        return Err(ProjectProjectionError::SnapshotEntityLimit {
            found: snapshot.entities.len(),
            limit: limits.max_entities,
        });
    }
    if authored_handles.len() != entities.len() {
        return Err(ProjectProjectionError::BindingCount {
            entities: entities.len(),
            bindings: authored_handles.len(),
        });
    }
    let mut bound_handles = HashMap::with_capacity(authored_handles.len());
    for entity in entities {
        let handle = authored_handles.get(&entity.id).copied().ok_or_else(|| {
            ProjectProjectionError::MissingBinding {
                stable_id: entity.id.clone(),
            }
        })?;
        if let Some(first) = bound_handles.insert(handle, entity.id.clone()) {
            return Err(ProjectProjectionError::DuplicateBinding {
                first,
                second: entity.id.clone(),
                handle,
            });
        }
    }

    // Bound the source mesh records before reading entity references or reserving maps.
    for (mesh_id, mesh) in meshes {
        if mesh.vertices.len() > limits.max_mesh_vertices {
            return Err(ProjectProjectionError::MeshVertexLimit {
                mesh_id: mesh_id.clone(),
                found: mesh.vertices.len(),
                limit: limits.max_mesh_vertices,
            });
        }
        if mesh.indices.len() > limits.max_mesh_indices {
            return Err(ProjectProjectionError::MeshIndexLimit {
                mesh_id: mesh_id.clone(),
                found: mesh.indices.len(),
                limit: limits.max_mesh_indices,
            });
        }
    }

    let mut snapshot_by_handle = HashMap::with_capacity(snapshot.entities.len());
    let mut snapshot_generation_by_slot = HashMap::with_capacity(snapshot.entities.len());
    for entry in snapshot.entities.iter() {
        if entry.entity.session != expected_session {
            return Err(ProjectProjectionError::SnapshotWrongSession {
                expected: expected_session.get(),
                actual: entry.entity.session.get(),
            });
        }
        if snapshot_by_handle
            .insert(entry.entity, entry.transform)
            .is_some()
        {
            return Err(ProjectProjectionError::DuplicateSnapshotEntity(
                entry.entity,
            ));
        }
        snapshot_generation_by_slot.insert(
            (entry.entity.session, entry.entity.index),
            entry.entity.generation,
        );
    }

    // Validate every binding and preflight the expanded geometry before cloning any mesh.
    let mut object_count = 0usize;
    let mut expanded_vertices = 0usize;
    let mut expanded_indices = 0usize;
    let mut expanded_geometry_bytes = 0usize;
    for entity in entities {
        let handle = authored_handles.get(&entity.id).copied().ok_or_else(|| {
            ProjectProjectionError::MissingBinding {
                stable_id: entity.id.clone(),
            }
        })?;
        if handle.session != expected_session {
            return Err(ProjectProjectionError::BindingWrongSession {
                stable_id: entity.id.clone(),
                expected: expected_session.get(),
                actual: handle.session.get(),
            });
        }
        if !snapshot_by_handle.contains_key(&handle) {
            return match snapshot_generation_by_slot.get(&(handle.session, handle.index)) {
                Some(actual) => Err(ProjectProjectionError::StaleEntity {
                    stable_id: entity.id.clone(),
                    expected: handle.generation,
                    actual: *actual,
                }),
                None => Err(ProjectProjectionError::MissingSnapshotEntity {
                    stable_id: entity.id.clone(),
                }),
            };
        }

        if let Some(mesh_id) = &entity.mesh {
            let mesh = meshes
                .get(mesh_id)
                .ok_or_else(|| ProjectProjectionError::MissingMesh {
                    stable_id: entity.id.clone(),
                    mesh_id: mesh_id.clone(),
                })?;
            object_count =
                object_count
                    .checked_add(1)
                    .ok_or(ProjectProjectionError::GeometryBudget {
                        budget: "object count",
                    })?;
            expanded_vertices = expanded_vertices.checked_add(mesh.vertices.len()).ok_or(
                ProjectProjectionError::GeometryBudget {
                    budget: "expanded vertices",
                },
            )?;
            expanded_indices = expanded_indices.checked_add(mesh.indices.len()).ok_or(
                ProjectProjectionError::GeometryBudget {
                    budget: "expanded indices",
                },
            )?;
            let vertex_bytes = mesh
                .vertices
                .len()
                .checked_mul(std::mem::size_of::<Vertex>())
                .ok_or(ProjectProjectionError::GeometryBudget {
                    budget: "expanded bytes",
                })?;
            let index_bytes = mesh
                .indices
                .len()
                .checked_mul(std::mem::size_of::<u32>())
                .ok_or(ProjectProjectionError::GeometryBudget {
                    budget: "expanded bytes",
                })?;
            expanded_geometry_bytes = expanded_geometry_bytes
                .checked_add(vertex_bytes)
                .and_then(|bytes| bytes.checked_add(index_bytes))
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<RenderObject>()))
                .ok_or(ProjectProjectionError::GeometryBudget {
                    budget: "expanded bytes",
                })?;
            if expanded_vertices > limits.max_expanded_vertices {
                return Err(ProjectProjectionError::GeometryBudget {
                    budget: "expanded vertices",
                });
            }
            if expanded_indices > limits.max_expanded_indices {
                return Err(ProjectProjectionError::GeometryBudget {
                    budget: "expanded indices",
                });
            }
            if expanded_geometry_bytes > limits.max_expanded_geometry_bytes {
                return Err(ProjectProjectionError::GeometryBudget {
                    budget: "expanded bytes",
                });
            }
        }
    }

    let presentation = prepared.manifest().presentation.as_ref();
    let mut scene = RenderScene {
        camera: presentation
            .and_then(|value| value.camera.as_ref())
            .map_or_else(Camera::default, |camera| Camera {
                position: camera.position,
                target: camera.target,
                up: camera.up,
                vertical_fov_radians: camera.vertical_fov_radians,
                near: camera.near,
                far: camera.far,
            }),
        directional_light: presentation
            .and_then(|value| value.light.as_ref())
            .map_or_else(DirectionalLight::default, |light| DirectionalLight {
                direction: light.direction,
                color: light.color,
                intensity: light.intensity,
            }),
        ambient_color: presentation
            .and_then(|value| value.ambient)
            .unwrap_or([0.16, 0.18, 0.22]),
        clear_color: presentation
            .and_then(|value| value.clear)
            .unwrap_or([0.035, 0.045, 0.065, 1.0]),
        objects: Vec::with_capacity(object_count),
    };

    for entity in entities {
        let Some(mesh_id) = &entity.mesh else {
            continue;
        };
        let handle = authored_handles[&entity.id];
        let transform = snapshot_by_handle[&handle];
        if !transform.is_finite() {
            return Err(ProjectProjectionError::InvalidScene(format!(
                "snapshot transform for '{}' contains non-finite values",
                entity.id
            )));
        }
        let mesh = &meshes[mesh_id];
        let render_mesh = Mesh::new(
            mesh.vertices
                .iter()
                .map(|vertex| Vertex::new(vertex.position, vertex.normal))
                .collect(),
            mesh.indices.clone(),
        );
        let rotation = Quat::from_xyzw(
            transform.rotation[0],
            transform.rotation[1],
            transform.rotation[2],
            transform.rotation[3],
        );
        let rotation_length_squared = rotation.length_squared();
        if !rotation_length_squared.is_finite() || rotation_length_squared < 1.0e-8 {
            return Err(ProjectProjectionError::InvalidScene(format!(
                "snapshot transform for '{}' has a zero rotation quaternion",
                entity.id
            )));
        }
        let model = Mat4::from_scale_rotation_translation(
            Vec3::from_array(transform.scale),
            rotation.normalize(),
            Vec3::from_array(transform.translation),
        );
        let material = entity
            .material
            .as_ref()
            .map_or_else(Material::default, |value| Material {
                base_color: value.base_color,
                metallic: value.metallic,
                roughness: value.roughness,
                emissive: value.emissive,
            });
        scene
            .objects
            .push(RenderObject::new(render_mesh, model, material));
    }
    scene
        .validate()
        .map_err(|error| ProjectProjectionError::InvalidScene(error.to_string()))?;

    Ok(ProjectProjection {
        scene,
        session: expected_session,
        snapshot_tick: snapshot.tick,
        object_count,
        expanded_vertices,
        expanded_indices,
        expanded_geometry_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use daedalus_content::project::{PreparedProject, ProjectLimits, prepare_project_from_bytes};
    use daedalus_content::scene::StableId;
    use daedalus_core::{
        EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
    };
    use glam::Vec3;

    use super::{
        ProjectProjectionError, ProjectProjectionLimits, project_snapshot,
        project_snapshot_with_limits,
    };

    fn prepared(entity_ids: &[&str], authored_translation: [f32; 3]) -> PreparedProject {
        prepared_with_scale(entity_ids, authored_translation, [1.0, 1.0, 1.0])
    }

    fn prepared_with_scale(
        entity_ids: &[&str],
        authored_translation: [f32; 3],
        authored_scale: [f32; 3],
    ) -> PreparedProject {
        let entities = entity_ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                format!(
                    r#"{{"id":"{id}","components":[
                        {{"id":"component:transform:{index}","type_id":"daedalus.transform","data":{{"translation":{:?},"rotation":[0.0,0.0,0.0,1.0],"scale":{:?}}}}},
                        {{"id":"component:mesh:{index}","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{{"mesh":"asset:triangle","material":{{"base_color":[0.2,0.4,0.8],"metallic":0.25,"roughness":0.7,"emissive":[0.0,0.0,0.0]}}}}}}
                    ]}}"#,
                    authored_translation,
                    authored_scale
                )
            })
            .collect::<Vec<_>>();
        let scene = format!(
            r#"{{"schema_version":1,"revision":9,"entities":[{}],"assets":[{{"id":"asset:triangle","kind":"daedalus.mesh","uri":"triangle.json","metadata":{{}}}}]}}"#,
            entities.join(",")
        );
        let manifest = br#"{"schema_version":1,"name":"projection test","scene":"scene.json","presentation":{"ambient":[0.1,0.2,0.3],"clear":[0.01,0.02,0.03,1.0]}}"#;
        let mesh = br#"{"schema_version":1,"vertices":[{"position":[0.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[1.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[0.0,1.0,0.0],"normal":[0.0,0.0,1.0]}],"indices":[0,1,2]}"#;
        let assets = BTreeMap::from([("triangle.json".to_owned(), mesh.to_vec())]);
        prepare_project_from_bytes(
            manifest,
            scene.as_bytes(),
            &assets,
            ProjectLimits::default(),
        )
        .unwrap()
    }

    fn bindings(project: &PreparedProject, session: SessionId) -> BTreeMap<StableId, EntityHandle> {
        project
            .entities()
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                (
                    entity.id.clone(),
                    EntityHandle {
                        session,
                        index: index as u32,
                        generation: 0,
                    },
                )
            })
            .collect()
    }

    fn snapshot(
        bindings: &BTreeMap<StableId, EntityHandle>,
        transforms: &[Transform],
    ) -> RenderSnapshot {
        RenderSnapshot {
            sequence: 4,
            tick: Tick(11),
            entities: bindings
                .values()
                .zip(transforms)
                .map(|(entity, transform)| RenderEntitySnapshot {
                    entity: *entity,
                    transform: *transform,
                })
                .collect::<Vec<_>>()
                .into(),
        }
    }

    #[test]
    fn projects_actual_snapshot_transform_and_authored_material() {
        let project = prepared(&["entity:hero"], [1.0, 2.0, 3.0]);
        let session = SessionId::from_raw(42);
        let bindings = bindings(&project, session);
        let current_transform = Transform {
            translation: [5.0, 6.0, 7.0],
            scale: [2.0, 3.0, 4.0],
            ..Transform::IDENTITY
        };
        let snapshot = snapshot(&bindings, &[current_transform]);

        let projection = project_snapshot(&project, session, &bindings, &snapshot).unwrap();
        let actual_translation = projection.scene.objects[0]
            .transform
            .transform_point3(Vec3::ZERO);
        assert_eq!(actual_translation.to_array(), current_transform.translation);
        assert_eq!(
            [
                projection.scene.objects[0]
                    .transform
                    .transform_vector3(Vec3::X)
                    .length(),
                projection.scene.objects[0]
                    .transform
                    .transform_vector3(Vec3::Y)
                    .length(),
                projection.scene.objects[0]
                    .transform
                    .transform_vector3(Vec3::Z)
                    .length(),
            ],
            current_transform.scale
        );
        assert_eq!(
            projection.scene.objects[0].material.base_color,
            [0.2, 0.4, 0.8]
        );
        assert_eq!(projection.scene.objects[0].material.metallic, 0.25);
        assert_eq!(projection.snapshot_tick, Tick(11));
        assert_eq!(projection.scene.ambient_color, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn accepts_content_valid_small_scale_through_project_projection() {
        let project = prepared_with_scale(&["entity:tiny"], [0.0, 0.0, 0.0], [0.001; 3]);
        let session = SessionId::from_raw(42);
        let handles = bindings(&project, session);
        let snapshot = snapshot(
            &handles,
            &[Transform {
                scale: [0.001; 3],
                ..Transform::IDENTITY
            }],
        );

        let projection = project_snapshot(&project, session, &handles, &snapshot).unwrap();
        let projected_scale = projection.scene.objects[0]
            .transform
            .transform_vector3(Vec3::X)
            .length();
        assert!((projected_scale - 0.001).abs() < 1.0e-7);
    }

    #[test]
    fn rejects_wrong_session_missing_and_stale_snapshot_entities() {
        let project = prepared(&["entity:hero"], [0.0, 0.0, 0.0]);
        let session = SessionId::from_raw(42);
        let handles = bindings(&project, session);
        let authored = handles.values().next().copied().unwrap();
        let other = SessionId::from_raw(43);

        let wrong_session = RenderSnapshot {
            sequence: 1,
            tick: Tick(1),
            entities: vec![RenderEntitySnapshot {
                entity: EntityHandle {
                    session: other,
                    ..authored
                },
                transform: Transform::IDENTITY,
            }]
            .into(),
        };
        assert!(matches!(
            project_snapshot(&project, session, &handles, &wrong_session),
            Err(ProjectProjectionError::SnapshotWrongSession { .. })
        ));

        let empty = RenderSnapshot {
            sequence: 2,
            tick: Tick(2),
            entities: Vec::new().into(),
        };
        assert!(matches!(
            project_snapshot(&project, session, &handles, &empty),
            Err(ProjectProjectionError::MissingSnapshotEntity { .. })
        ));

        let stale = RenderSnapshot {
            sequence: 3,
            tick: Tick(3),
            entities: vec![RenderEntitySnapshot {
                entity: EntityHandle {
                    generation: authored.generation + 1,
                    ..authored
                },
                transform: Transform::IDENTITY,
            }]
            .into(),
        };
        assert!(matches!(
            project_snapshot(&project, session, &handles, &stale),
            Err(ProjectProjectionError::StaleEntity { .. })
        ));
    }

    #[test]
    fn repeated_mesh_instances_are_budgeted_before_scene_geometry_clones() {
        let project = prepared(&["entity:one", "entity:two"], [0.0, 0.0, 0.0]);
        let session = SessionId::from_raw(42);
        let handles = bindings(&project, session);
        let snapshot = snapshot(&handles, &[Transform::IDENTITY; 2]);
        let limits = ProjectProjectionLimits {
            max_expanded_vertices: 3,
            max_expanded_indices: 6,
            max_expanded_geometry_bytes: 2
                * (3 * std::mem::size_of::<super::Vertex>()
                    + 3 * std::mem::size_of::<u32>()
                    + std::mem::size_of::<super::RenderObject>()),
            ..ProjectProjectionLimits::default()
        };

        assert!(matches!(
            project_snapshot_with_limits(&project, session, &handles, &snapshot, limits),
            Err(ProjectProjectionError::GeometryBudget {
                budget: "expanded vertices"
            })
        ));
    }

    #[test]
    fn rejects_duplicate_runtime_handles_for_distinct_authored_ids() {
        let project = prepared(&["entity:one", "entity:two"], [0.0, 0.0, 0.0]);
        let session = SessionId::from_raw(42);
        let mut handles = bindings(&project, session);
        let first = project.entities()[0].id.clone();
        let second = project.entities()[1].id.clone();
        handles.insert(second, handles[&first]);
        let snapshot = snapshot(&handles, &[Transform::IDENTITY; 2]);

        assert!(matches!(
            project_snapshot(&project, session, &handles, &snapshot),
            Err(ProjectProjectionError::DuplicateBinding { .. })
        ));
    }

    #[test]
    fn projection_limits_cannot_raise_the_hard_ceiling() {
        let project = prepared(&["entity:hero"], [0.0, 0.0, 0.0]);
        let session = SessionId::from_raw(42);
        let handles = bindings(&project, session);
        let snapshot = snapshot(&handles, &[Transform::IDENTITY]);
        let limits = ProjectProjectionLimits {
            max_expanded_geometry_bytes: usize::MAX,
            ..ProjectProjectionLimits::default()
        };

        assert!(matches!(
            project_snapshot_with_limits(&project, session, &handles, &snapshot, limits),
            Err(ProjectProjectionError::InvalidLimits)
        ));
    }
}
