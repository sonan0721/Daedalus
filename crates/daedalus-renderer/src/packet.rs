//! Immutable renderer-owned geometry and compact per-snapshot draw packets.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, OnceLock},
};

use daedalus_content::{project::PreparedProject, scene::StableId};
use daedalus_core::{EntityHandle, RenderSnapshot, SessionId, Tick};
use glam::{Mat3, Mat4, Quat, Vec3};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{Camera, DirectionalLight, Material, Mesh, RendererError, Vertex};

const MESH_LAYOUT_VERSION: u32 = 1;
const MESH_HASH_DOMAIN: &[u8] = b"Daedalus renderer mesh payload\0v1\0";

/// CPU limits for a single immutable renderer mesh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeshResourceLimits {
    pub max_vertices: usize,
    pub max_indices: usize,
    pub max_geometry_bytes: usize,
}

impl Default for MeshResourceLimits {
    fn default() -> Self {
        Self {
            max_vertices: 1_000_000,
            max_indices: 3_000_000,
            max_geometry_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Limits for a candidate packet and the device-local immutable mesh cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketRendererLimits {
    pub preferred_geometry_bytes: usize,
    pub hard_geometry_bytes: usize,
    pub max_active_meshes: usize,
    pub max_total_entries: usize,
    pub max_instances: usize,
    pub max_expanded_vertices: usize,
    pub max_expanded_indices: usize,
}

impl Default for PacketRendererLimits {
    fn default() -> Self {
        Self {
            preferred_geometry_bytes: 64 * 1024 * 1024,
            hard_geometry_bytes: 128 * 1024 * 1024,
            max_active_meshes: 256,
            max_total_entries: 512,
            max_instances: 16_384,
            max_expanded_vertices: 1_000_000,
            max_expanded_indices: 3_000_000,
        }
    }
}

impl PacketRendererLimits {
    pub(crate) fn validate(self) -> Result<(), PacketRenderError> {
        let hard = Self::default();
        let doubled_bytes = self.preferred_geometry_bytes.checked_mul(2);
        let doubled_entries = self.max_active_meshes.checked_mul(2);
        if self.preferred_geometry_bytes == 0
            || self.hard_geometry_bytes == 0
            || self.max_active_meshes == 0
            || self.max_total_entries == 0
            || self.max_instances == 0
            || self.max_expanded_vertices == 0
            || self.max_expanded_indices == 0
            || self.preferred_geometry_bytes > hard.preferred_geometry_bytes
            || self.hard_geometry_bytes > hard.hard_geometry_bytes
            || self.max_active_meshes > hard.max_active_meshes
            || self.max_total_entries > hard.max_total_entries
            || self.max_instances > hard.max_instances
            || self.max_expanded_vertices > hard.max_expanded_vertices
            || self.max_expanded_indices > hard.max_expanded_indices
            || doubled_bytes.is_none_or(|value| self.hard_geometry_bytes < value)
            || doubled_entries.is_none_or(|value| self.max_total_entries < value)
        {
            return Err(PacketRenderError::InvalidLimits);
        }
        Ok(())
    }
}

/// Structured packet validation, device and bounded-residency failures.
#[derive(Debug, Error)]
pub enum PacketRenderError {
    #[error(
        "packet renderer limits must be positive, within hard ceilings, and leave a 2x transition reserve"
    )]
    InvalidLimits,
    #[error("invalid immutable mesh: {0}")]
    InvalidMesh(String),
    #[error("invalid render packet: {0}")]
    InvalidPacket(String),
    #[error(
        "packet candidate exceeds the preferred geometry working set ({bytes} bytes > {limit} bytes)"
    )]
    WorkingSetExceeded { bytes: usize, limit: usize },
    #[error("packet cache is temporarily pinned by in-flight work and cannot admit this candidate")]
    ResidencyBusy,
    #[error("the adapter limits do not support the packet vertex layout: {0}")]
    DeviceLimit(String),
    #[error("renderer operation failed: {0}")]
    Renderer(#[from] RendererError),
}

/// A validated, immutable local-space indexed triangle mesh.
#[derive(Clone, Debug)]
pub struct MeshHandle(Arc<MeshResource>);

#[derive(Debug)]
struct MeshResource {
    asset_id: StableId,
    generation: u64,
    payload_hash: [u8; 32],
    layout_version: u32,
    mesh: Mesh,
    gpu_bytes: usize,
}

impl MeshHandle {
    /// Validates and owns geometry, then hashes canonical little-endian content once.
    pub fn new(
        asset_id: StableId,
        generation: u64,
        mesh: Mesh,
        limits: MeshResourceLimits,
    ) -> Result<Self, PacketRenderError> {
        if limits.max_vertices == 0
            || limits.max_indices == 0
            || limits.max_geometry_bytes == 0
            || limits.max_vertices > MeshResourceLimits::default().max_vertices
            || limits.max_indices > MeshResourceLimits::default().max_indices
            || limits.max_geometry_bytes > MeshResourceLimits::default().max_geometry_bytes
        {
            return Err(PacketRenderError::InvalidLimits);
        }
        mesh.validate()
            .map_err(|error| PacketRenderError::InvalidMesh(error.to_string()))?;
        if mesh.vertices.len() > limits.max_vertices || mesh.indices.len() > limits.max_indices {
            return Err(PacketRenderError::InvalidMesh(
                "mesh exceeds its vertex or index limit".into(),
            ));
        }
        let source_bytes = mesh
            .vertices
            .len()
            .checked_mul(std::mem::size_of::<Vertex>())
            .and_then(|size| {
                mesh.indices
                    .len()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|index_bytes| size.checked_add(index_bytes))
            })
            .ok_or_else(|| PacketRenderError::InvalidMesh("mesh byte size overflowed".into()))?;
        if source_bytes > limits.max_geometry_bytes {
            return Err(PacketRenderError::InvalidMesh(
                "mesh exceeds its byte limit".into(),
            ));
        }
        let gpu_bytes = mesh
            .vertices
            .len()
            .checked_mul(24)
            .and_then(|size| {
                mesh.indices
                    .len()
                    .checked_mul(4)
                    .and_then(|index_bytes| size.checked_add(index_bytes))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidMesh("GPU mesh byte size overflowed".into())
            })?;
        let payload_hash = canonical_mesh_hash(&mesh);
        Ok(Self(Arc::new(MeshResource {
            asset_id,
            generation,
            payload_hash,
            layout_version: MESH_LAYOUT_VERSION,
            mesh,
            gpu_bytes,
        })))
    }

    pub fn asset_id(&self) -> &StableId {
        &self.0.asset_id
    }
    pub fn generation(&self) -> u64 {
        self.0.generation
    }
    pub fn payload_hash(&self) -> [u8; 32] {
        self.0.payload_hash
    }
    pub fn layout_version(&self) -> u32 {
        self.0.layout_version
    }
    pub fn vertex_count(&self) -> usize {
        self.0.mesh.vertices.len()
    }
    pub fn index_count(&self) -> usize {
        self.0.mesh.indices.len()
    }
    pub(crate) fn mesh(&self) -> &Mesh {
        &self.0.mesh
    }
    pub(crate) fn gpu_bytes(&self) -> usize {
        self.0.gpu_bytes
    }
    pub(crate) fn key(&self) -> GeometryKey {
        GeometryKey {
            hash: self.0.payload_hash,
            layout: self.0.layout_version,
        }
    }
}

impl PartialEq for MeshHandle {
    fn eq(&self, other: &Self) -> bool {
        self.0.asset_id == other.0.asset_id
            && self.0.generation == other.0.generation
            && self.0.payload_hash == other.0.payload_hash
            && self.0.layout_version == other.0.layout_version
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct GeometryKey {
    pub hash: [u8; 32],
    pub layout: u32,
}

fn canonical_mesh_hash(mesh: &Mesh) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(MESH_HASH_DOMAIN);
    hasher.update(MESH_LAYOUT_VERSION.to_le_bytes());
    hasher.update((mesh.vertices.len() as u64).to_le_bytes());
    hasher.update((mesh.indices.len() as u64).to_le_bytes());
    for vertex in &mesh.vertices {
        for value in vertex.position.into_iter().chain(vertex.normal) {
            hasher.update(value.to_bits().to_le_bytes());
        }
    }
    for index in &mesh.indices {
        hasher.update(index.to_le_bytes());
    }
    hasher.finalize().into()
}

/// Stable instance slot identity across packets in one runtime session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderInstanceId {
    pub entity: EntityHandle,
    pub primitive: u32,
}

/// One compact draw instance. Geometry remains shared through `MeshHandle`.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderInstance {
    pub id: RenderInstanceId,
    pub mesh: MeshHandle,
    pub transform: Mat4,
    pub material: Material,
}

/// One immutable frame-facing input to the packet renderer.
#[derive(Clone, Debug)]
pub struct RenderPacket {
    session: SessionId,
    tick: Tick,
    sequence: u64,
    project_generation: u64,
    source_digest: String,
    camera: Camera,
    pub(crate) directional_light: DirectionalLight,
    pub(crate) ambient_color: [f32; 3],
    pub(crate) clear_color: [f32; 4],
    instances: Arc<[RenderInstance]>,
    validated_normals: Arc<OnceLock<HashMap<RenderInstanceId, CachedNormal>>>,
}

#[derive(Clone, Copy, Debug)]
struct CachedNormal {
    transform: Mat4,
    normal: Mat3,
}

impl PartialEq for RenderPacket {
    fn eq(&self, other: &Self) -> bool {
        self.session == other.session
            && self.tick == other.tick
            && self.sequence == other.sequence
            && self.project_generation == other.project_generation
            && self.source_digest == other.source_digest
            && self.camera == other.camera
            && self.directional_light == other.directional_light
            && self.ambient_color == other.ambient_color
            && self.clear_color == other.clear_color
            && self.instances == other.instances
    }
}

impl RenderPacket {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: SessionId,
        tick: Tick,
        sequence: u64,
        project_generation: u64,
        source_digest: impl Into<String>,
        camera: Camera,
        directional_light: DirectionalLight,
        ambient_color: [f32; 3],
        clear_color: [f32; 4],
        instances: Vec<RenderInstance>,
    ) -> Self {
        Self {
            session,
            tick,
            sequence,
            project_generation,
            source_digest: source_digest.into(),
            camera,
            directional_light,
            ambient_color,
            clear_color,
            instances: instances.into(),
            validated_normals: Arc::new(OnceLock::new()),
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }
    pub fn tick(&self) -> Tick {
        self.tick
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }
    pub fn camera(&self) -> &Camera {
        &self.camera
    }
    pub fn directional_light(&self) -> &DirectionalLight {
        &self.directional_light
    }
    pub fn ambient_color(&self) -> [f32; 3] {
        self.ambient_color
    }
    pub fn clear_color(&self) -> [f32; 4] {
        self.clear_color
    }
    pub fn instances(&self) -> &[RenderInstance] {
        &self.instances
    }
    pub fn with_camera(&self, camera: Camera) -> Self {
        let mut packet = self.clone();
        packet.camera = camera;
        packet
    }
    pub fn with_instances(&self, instances: Vec<RenderInstance>) -> Self {
        let mut packet = self.clone();
        packet.instances = instances.into();
        packet.validated_normals = Arc::new(OnceLock::new());
        packet
    }

    pub(crate) fn validate(
        &self,
        limits: PacketRendererLimits,
    ) -> Result<PacketValidation, PacketRenderError> {
        self.validate_reusing(limits, None)
    }

    pub(crate) fn validate_reusing(
        &self,
        limits: PacketRendererLimits,
        previous: Option<&RenderPacket>,
    ) -> Result<PacketValidation, PacketRenderError> {
        limits.validate()?;
        self.camera
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        self.directional_light
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        if !valid_source_digest(&self.source_digest)
            || !self
                .ambient_color
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
            || !self.clear_color.iter().all(|value| value.is_finite())
        {
            return Err(PacketRenderError::InvalidPacket(
                "source digest or frame colors are invalid".into(),
            ));
        }
        if self.instances.len() > limits.max_instances {
            return Err(PacketRenderError::InvalidPacket(
                "instance limit exceeded".into(),
            ));
        }
        let mut ids = HashSet::with_capacity(self.instances.len());
        let mut geometries = HashMap::new();
        let mut normal_matrices = HashMap::with_capacity(self.instances.len());
        let mut validated_normals = HashMap::with_capacity(self.instances.len());
        let own_normals = self.validated_normals.get();
        let previous_normals = previous.and_then(|packet| packet.validated_normals.get());
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        for instance in self.instances.iter() {
            if instance.id.entity.session != self.session || !ids.insert(instance.id) {
                return Err(PacketRenderError::InvalidPacket(
                    "instance session mismatch or duplicate instance ID".into(),
                ));
            }
            if !instance.transform.is_finite()
                || !Mat4::from_mat3(glam::Mat3::from_mat4(instance.transform)).is_finite()
            {
                return Err(PacketRenderError::InvalidPacket(
                    "instance transform is non-finite".into(),
                ));
            }
            let cached = own_normals
                .and_then(|normals| normals.get(&instance.id))
                .filter(|cached| cached.transform == instance.transform)
                .or_else(|| {
                    previous_normals
                        .and_then(|normals| normals.get(&instance.id))
                        .filter(|cached| cached.transform == instance.transform)
                });
            let normal = cached
                .map(|cached| cached.normal)
                .map_or_else(|| inverse_transpose(instance.transform), Ok)?;
            normal_matrices.insert(instance.id, normal);
            validated_normals.insert(
                instance.id,
                CachedNormal {
                    transform: instance.transform,
                    normal,
                },
            );
            instance
                .material
                .validate()
                .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
            let key = instance.mesh.key();
            geometries
                .entry(key)
                .or_insert_with(|| instance.mesh.clone());
            expanded_vertices = expanded_vertices
                .checked_add(instance.mesh.vertex_count())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded vertex count overflowed".into())
                })?;
            expanded_indices = expanded_indices
                .checked_add(instance.mesh.index_count())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded index count overflowed".into())
                })?;
        }
        if geometries.len() > limits.max_active_meshes {
            return Err(PacketRenderError::InvalidPacket(
                "active unique mesh limit exceeded".into(),
            ));
        }
        if expanded_vertices > limits.max_expanded_vertices
            || expanded_indices > limits.max_expanded_indices
        {
            return Err(PacketRenderError::InvalidPacket(
                "expanded draw geometry limit exceeded".into(),
            ));
        }
        let unique_bytes = geometries
            .values()
            .try_fold(0usize, |sum, mesh| sum.checked_add(mesh.gpu_bytes()))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("unique geometry byte count overflowed".into())
            })?;
        if unique_bytes > limits.preferred_geometry_bytes {
            return Err(PacketRenderError::WorkingSetExceeded {
                bytes: unique_bytes,
                limit: limits.preferred_geometry_bytes,
            });
        }
        let _ = self.validated_normals.set(validated_normals);
        Ok(PacketValidation {
            geometries,
            normal_matrices,
        })
    }
}

fn inverse_transpose(transform: Mat4) -> Result<Mat3, PacketRenderError> {
    crate::scene::inverse_transpose_normal_matrix(transform).ok_or_else(|| {
        PacketRenderError::InvalidPacket(
            "instance transform must have a finite inverse-transpose normal matrix".into(),
        )
    })
}

fn valid_source_digest(digest: &str) -> bool {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) struct PacketValidation {
    pub geometries: HashMap<GeometryKey, MeshHandle>,
    pub normal_matrices: HashMap<RenderInstanceId, Mat3>,
}

/// Source project resources prepared once, then paired with each authoritative snapshot.
pub struct ProjectRenderResources {
    expected_session: SessionId,
    project_generation: u64,
    source_digest: String,
    camera: Camera,
    directional_light: DirectionalLight,
    ambient_color: [f32; 3],
    clear_color: [f32; 4],
    bindings: Vec<(EntityHandle, ProjectBinding)>,
    geometry_bytes: usize,
    limits: PacketRendererLimits,
}

struct ProjectBinding {
    stable_id: StableId,
    mesh: Option<MeshHandle>,
    material: Material,
}

impl ProjectRenderResources {
    pub fn prepare(
        project: &PreparedProject,
        project_generation: u64,
        expected_session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        limits: PacketRendererLimits,
    ) -> Result<Self, PacketRenderError> {
        limits.validate()?;
        if authored_handles.len() != project.entities().len() {
            return Err(PacketRenderError::InvalidPacket(
                "authored binding count does not match prepared entities".into(),
            ));
        }
        let mut used_handles = HashSet::with_capacity(authored_handles.len());
        for entity in project.entities() {
            let handle = authored_handles.get(&entity.id).ok_or_else(|| {
                PacketRenderError::InvalidPacket(format!(
                    "missing runtime binding for {}",
                    entity.id
                ))
            })?;
            if handle.session != expected_session || !used_handles.insert(*handle) {
                return Err(PacketRenderError::InvalidPacket(
                    "runtime binding session mismatch or duplicate handle".into(),
                ));
            }
        }
        let mut mesh_handles = HashMap::with_capacity(project.meshes().len());
        let mesh_limits = MeshResourceLimits::default();
        let mut geometry_bytes = 0usize;
        for (asset_id, source) in project.meshes() {
            let mesh = Mesh::new(
                source
                    .vertices
                    .iter()
                    .map(|vertex| Vertex::new(vertex.position, vertex.normal))
                    .collect(),
                source.indices.clone(),
            );
            let handle = MeshHandle::new(asset_id.clone(), project_generation, mesh, mesh_limits)?;
            geometry_bytes = geometry_bytes
                .checked_add(handle.gpu_bytes())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("project geometry bytes overflowed".into())
                })?;
            mesh_handles.insert(asset_id.clone(), handle);
        }
        if mesh_handles.len() > limits.max_active_meshes
            || geometry_bytes > limits.preferred_geometry_bytes
        {
            return Err(PacketRenderError::WorkingSetExceeded {
                bytes: geometry_bytes,
                limit: limits.preferred_geometry_bytes,
            });
        }
        let presentation = project.manifest().presentation.as_ref();
        let camera = presentation
            .and_then(|value| value.camera.as_ref())
            .map_or_else(Camera::default, |value| Camera {
                position: value.position,
                target: value.target,
                up: value.up,
                vertical_fov_radians: value.vertical_fov_radians,
                near: value.near,
                far: value.far,
            });
        let directional_light = presentation
            .and_then(|value| value.light.as_ref())
            .map_or_else(DirectionalLight::default, |value| DirectionalLight {
                direction: value.direction,
                color: value.color,
                intensity: value.intensity,
            });
        let ambient_color = presentation
            .and_then(|value| value.ambient)
            .unwrap_or([0.16, 0.18, 0.22]);
        let clear_color = presentation
            .and_then(|value| value.clear)
            .unwrap_or([0.035, 0.045, 0.065, 1.0]);
        camera
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        directional_light
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        let mut bindings = Vec::with_capacity(project.entities().len());
        for entity in project.entities() {
            let handle = authored_handles[&entity.id];
            let mesh = entity
                .mesh
                .as_ref()
                .and_then(|id| mesh_handles.get(id))
                .cloned();
            if entity.mesh.is_some() && mesh.is_none() {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "entity {} references a missing mesh",
                    entity.id
                )));
            }
            let material = entity
                .material
                .as_ref()
                .map_or_else(Material::default, |value| Material {
                    base_color: value.base_color,
                    metallic: value.metallic,
                    roughness: value.roughness,
                    emissive: value.emissive,
                });
            material
                .validate()
                .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
            bindings.push((
                handle,
                ProjectBinding {
                    stable_id: entity.id.clone(),
                    mesh,
                    material,
                },
            ));
        }
        Ok(Self {
            expected_session,
            project_generation,
            source_digest: project.digest().to_owned(),
            camera,
            directional_light,
            ambient_color,
            clear_color,
            bindings,
            geometry_bytes,
            limits,
        })
    }

    pub fn prepared_geometry_bytes(&self) -> usize {
        self.geometry_bytes
    }
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn extract(
        &self,
        snapshot: &RenderSnapshot,
        camera_override: Option<&Camera>,
    ) -> Result<RenderPacket, PacketRenderError> {
        if snapshot.entities.len() > self.limits.max_instances {
            return Err(PacketRenderError::InvalidPacket(
                "snapshot entity count exceeds packet limit".into(),
            ));
        }
        let mut snapshot_by_handle = HashMap::with_capacity(snapshot.entities.len());
        for entry in snapshot.entities.iter() {
            if entry.entity.session != self.expected_session
                || snapshot_by_handle
                    .insert(entry.entity, entry.transform)
                    .is_some()
            {
                return Err(PacketRenderError::InvalidPacket(
                    "snapshot session mismatch or duplicate handle".into(),
                ));
            }
        }
        let mut instances = Vec::with_capacity(self.bindings.len());
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        for (handle, binding) in &self.bindings {
            let transform_data = snapshot_by_handle.get(handle).ok_or_else(|| {
                PacketRenderError::InvalidPacket(format!(
                    "snapshot is missing authored entity {}",
                    binding.stable_id
                ))
            })?;
            let Some(mesh) = &binding.mesh else {
                continue;
            };
            if !transform_data.is_finite() {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "snapshot transform for {} is non-finite",
                    binding.stable_id
                )));
            }
            let rotation = Quat::from_xyzw(
                transform_data.rotation[0],
                transform_data.rotation[1],
                transform_data.rotation[2],
                transform_data.rotation[3],
            );
            let rotation_length = rotation.length_squared();
            if !rotation_length.is_finite() || rotation_length < 1.0e-8 {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "snapshot transform for {} has a zero rotation",
                    binding.stable_id
                )));
            }
            let transform = Mat4::from_scale_rotation_translation(
                Vec3::from_array(transform_data.scale),
                rotation.normalize(),
                Vec3::from_array(transform_data.translation),
            );
            expanded_vertices = expanded_vertices
                .checked_add(mesh.vertex_count())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded vertex count overflowed".into())
                })?;
            expanded_indices = expanded_indices
                .checked_add(mesh.index_count())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded index count overflowed".into())
                })?;
            instances.push(RenderInstance {
                id: RenderInstanceId {
                    entity: *handle,
                    primitive: 0,
                },
                mesh: mesh.clone(),
                transform,
                material: binding.material.clone(),
            });
        }
        if expanded_vertices > self.limits.max_expanded_vertices
            || expanded_indices > self.limits.max_expanded_indices
        {
            return Err(PacketRenderError::InvalidPacket(
                "expanded snapshot geometry limit exceeded".into(),
            ));
        }
        let camera = camera_override
            .cloned()
            .unwrap_or_else(|| self.camera.clone());
        camera
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        Ok(RenderPacket::new(
            self.expected_session,
            snapshot.tick,
            snapshot.sequence,
            self.project_generation,
            self.source_digest.clone(),
            camera,
            self.directional_light.clone(),
            self.ambient_color,
            self.clear_color,
            instances,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use daedalus_content::project::{PreparedProject, ProjectLimits, prepare_project_from_bytes};
    use daedalus_content::scene::StableId;
    use daedalus_core::{
        EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
    };
    use glam::{Mat4, Vec3};

    use crate::{Mesh, Vertex};

    use super::{
        MeshHandle, MeshResourceLimits, PacketRendererLimits, ProjectRenderResources,
        RenderInstance, RenderInstanceId, RenderPacket,
    };

    fn triangle() -> Mesh {
        Mesh::new(
            vec![
                Vertex::new([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ],
            vec![0, 1, 2],
        )
    }

    fn prepared_two_instances() -> PreparedProject {
        let scene = br#"{"schema_version":1,"revision":1,"entities":[
            {"id":"entity:a","components":[{"id":"component:mesh:a","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle"}}]},
            {"id":"entity:b","components":[{"id":"component:mesh:b","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle"}}]}
        ],"assets":[{"id":"asset:triangle","kind":"daedalus.mesh","uri":"triangle.json","metadata":{}}]}"#;
        let manifest = br#"{"schema_version":1,"name":"packet test","scene":"scene.json"}"#;
        let mesh = br#"{"schema_version":1,"vertices":[{"position":[0.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[1.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[0.0,1.0,0.0],"normal":[0.0,0.0,1.0]}],"indices":[0,1,2]}"#;
        prepare_project_from_bytes(
            manifest,
            scene,
            &BTreeMap::from([("triangle.json".to_owned(), mesh.to_vec())]),
            ProjectLimits::default(),
        )
        .unwrap()
    }

    fn project_bindings(
        project: &PreparedProject,
        session: SessionId,
    ) -> BTreeMap<StableId, EntityHandle> {
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
                        generation: 3,
                    },
                )
            })
            .collect()
    }

    fn project_snapshot(
        bindings: &BTreeMap<StableId, EntityHandle>,
        transforms: &[Transform],
    ) -> RenderSnapshot {
        RenderSnapshot {
            sequence: 22,
            tick: Tick(14),
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
    fn mesh_hash_is_canonical_and_generation_independent() {
        let id = StableId::new("mesh.test").unwrap();
        let a = MeshHandle::new(id.clone(), 1, triangle(), MeshResourceLimits::default()).unwrap();
        let b = MeshHandle::new(id, 2, triangle(), MeshResourceLimits::default()).unwrap();
        assert_eq!(a.payload_hash(), b.payload_hash());
        assert_eq!(a.layout_version(), 1);
        assert_ne!(a.generation(), b.generation());
        let original = triangle();
        let expected = MeshHandle::new(
            StableId::new("mesh.copy").unwrap(),
            1,
            original.clone(),
            MeshResourceLimits::default(),
        )
        .unwrap();
        let mut caller_copy = original;
        caller_copy.vertices[0].position[0] = 10.0;
        assert_eq!(expected.vertex_count(), 3);
        assert_ne!(
            expected.payload_hash(),
            MeshHandle::new(
                StableId::new("mesh.copy").unwrap(),
                1,
                caller_copy,
                MeshResourceLimits::default()
            )
            .unwrap()
            .payload_hash()
        );
    }

    #[test]
    fn packet_limits_require_two_times_transition_reserve() {
        let mut limits = PacketRendererLimits::default();
        assert!(limits.validate().is_ok());
        limits.max_total_entries = limits.max_active_meshes;
        assert!(limits.validate().is_err());
        limits = PacketRendererLimits::default();
        limits.hard_geometry_bytes = limits.preferred_geometry_bytes;
        assert!(limits.validate().is_err());
    }

    #[test]
    fn prepared_project_extracts_shared_mesh_handles_with_authoritative_transform() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(9);
        let bindings = project_bindings(&project, session);
        let resources = ProjectRenderResources::prepare(
            &project,
            7,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        assert_eq!(resources.prepared_geometry_bytes(), 84);
        let snapshot = project_snapshot(
            &bindings,
            &[
                Transform {
                    translation: [1.0, 2.0, 3.0],
                    ..Transform::IDENTITY
                },
                Transform {
                    translation: [-2.0, 0.0, 1.0],
                    scale: [2.0, 3.0, 1.0],
                    ..Transform::IDENTITY
                },
            ],
        );
        let override_camera = crate::Camera {
            position: [2.0, 3.0, 4.0],
            ..crate::Camera::default()
        };
        let packet = resources
            .extract(&snapshot, Some(&override_camera))
            .unwrap();
        assert_eq!(packet.session(), session);
        assert_eq!(packet.tick(), Tick(14));
        assert_eq!(packet.sequence(), 22);
        assert_eq!(packet.project_generation(), 7);
        assert_eq!(packet.source_digest(), project.digest());
        assert_eq!(packet.instances().len(), 2);
        assert!(Arc::ptr_eq(
            &packet.instances()[0].mesh.0,
            &packet.instances()[1].mesh.0
        ));
        assert_eq!(
            packet.instances()[0]
                .transform
                .transform_point3(glam::Vec3::ZERO)
                .to_array(),
            [1.0, 2.0, 3.0]
        );
        assert_eq!(packet.instances()[1].transform.x_axis.x, 2.0);
        assert_eq!(packet.camera(), &override_camera);
    }

    #[test]
    fn prepared_project_packet_accepts_tiny_scale_with_finite_inverse() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(19);
        let bindings = project_bindings(&project, session);
        let resources = ProjectRenderResources::prepare(
            &project,
            1,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        let snapshot = project_snapshot(
            &bindings,
            &[
                Transform {
                    scale: [0.001; 3],
                    ..Transform::IDENTITY
                },
                Transform::IDENTITY,
            ],
        );
        let packet = resources.extract(&snapshot, None).unwrap();
        let validation = packet.validate(PacketRendererLimits::default()).unwrap();
        assert_eq!(validation.normal_matrices.len(), 2);
    }

    #[test]
    fn project_resource_extraction_rejects_wrong_session_duplicates_and_missing_entities() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(9);
        let bindings = project_bindings(&project, session);
        let resources = ProjectRenderResources::prepare(
            &project,
            1,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();

        let wrong_session = RenderSnapshot {
            sequence: 1,
            tick: Tick(1),
            entities: vec![RenderEntitySnapshot {
                entity: EntityHandle {
                    session: SessionId::from_raw(10),
                    ..*bindings.values().next().unwrap()
                },
                transform: Transform::IDENTITY,
            }]
            .into(),
        };
        assert!(resources.extract(&wrong_session, None).is_err());

        let first = *bindings.values().next().unwrap();
        let duplicate = RenderSnapshot {
            sequence: 2,
            tick: Tick(2),
            entities: vec![
                RenderEntitySnapshot {
                    entity: first,
                    transform: Transform::IDENTITY,
                },
                RenderEntitySnapshot {
                    entity: first,
                    transform: Transform::IDENTITY,
                },
            ]
            .into(),
        };
        assert!(resources.extract(&duplicate, None).is_err());

        let missing = RenderSnapshot {
            sequence: 3,
            tick: Tick(3),
            entities: vec![RenderEntitySnapshot {
                entity: first,
                transform: Transform::IDENTITY,
            }]
            .into(),
        };
        assert!(resources.extract(&missing, None).is_err());
    }

    #[test]
    fn packet_validation_rejects_duplicate_identity_singular_transform_and_expanded_budget() {
        let session = SessionId::from_raw(11);
        let instance = packet_instance(session, 1);
        let packet = RenderPacket::new(
            session,
            Tick(0),
            0,
            1,
            "ab".repeat(32),
            crate::Camera::default(),
            crate::DirectionalLight::default(),
            [0.1; 3],
            [0.0, 0.0, 0.0, 1.0],
            vec![instance.clone(), instance.clone()],
        );
        assert!(packet.validate(PacketRendererLimits::default()).is_err());

        let mut singular = instance;
        singular.transform = Mat4::from_scale(glam::Vec3::new(0.0, 1.0, 1.0));
        let packet = RenderPacket::new(
            session,
            Tick(0),
            0,
            1,
            "ab".repeat(32),
            crate::Camera::default(),
            crate::DirectionalLight::default(),
            [0.1; 3],
            [0.0, 0.0, 0.0, 1.0],
            vec![singular],
        );
        assert!(packet.validate(PacketRendererLimits::default()).is_err());

        let mut extreme = packet_instance(session, 1);
        extreme.transform = Mat4::from_scale(Vec3::new(1.0e20, 1.0e20, 1.0e-39));
        let packet = RenderPacket::new(
            session,
            Tick(0),
            0,
            1,
            "ab".repeat(32),
            crate::Camera::default(),
            crate::DirectionalLight::default(),
            [0.1; 3],
            [0.0, 0.0, 0.0, 1.0],
            vec![extreme],
        );
        assert!(packet.validate(PacketRendererLimits::default()).is_err());

        let packet = RenderPacket::new(
            session,
            Tick(0),
            0,
            1,
            "ab".repeat(32),
            crate::Camera::default(),
            crate::DirectionalLight::default(),
            [0.1; 3],
            [0.0, 0.0, 0.0, 1.0],
            vec![packet_instance(session, 1), packet_instance(session, 2)],
        );
        let limits = PacketRendererLimits {
            max_expanded_vertices: 3,
            max_expanded_indices: 3,
            ..PacketRendererLimits::default()
        };
        assert!(packet.validate(limits).is_err());
    }

    fn packet_instance(session: SessionId, index: u32) -> RenderInstance {
        RenderInstance {
            id: RenderInstanceId {
                entity: EntityHandle {
                    session,
                    index,
                    generation: 0,
                },
                primitive: 0,
            },
            mesh: MeshHandle::new(
                StableId::new("mesh.test").unwrap(),
                1,
                triangle(),
                MeshResourceLimits::default(),
            )
            .unwrap(),
            transform: Mat4::IDENTITY,
            material: crate::Material::default(),
        }
    }

    #[test]
    fn packet_validation_distinguishes_candidate_working_set_excess() {
        let session = SessionId::from_raw(12);
        let packet = RenderPacket::new(
            session,
            Tick(0),
            0,
            1,
            "ab".repeat(32),
            crate::Camera::default(),
            crate::DirectionalLight::default(),
            [0.1; 3],
            [0.0, 0.0, 0.0, 1.0],
            vec![packet_instance(session, 1)],
        );
        let limits = PacketRendererLimits {
            preferred_geometry_bytes: 1,
            hard_geometry_bytes: 2,
            max_active_meshes: 1,
            max_total_entries: 2,
            ..PacketRendererLimits::default()
        };
        assert!(matches!(
            packet.validate(limits),
            Err(super::PacketRenderError::WorkingSetExceeded { .. })
        ));
    }
}
