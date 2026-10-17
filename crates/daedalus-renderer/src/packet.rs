//! Immutable renderer-owned geometry and compact per-snapshot draw packets.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, OnceLock, atomic::AtomicBool},
};

use bytemuck::{Pod, Zeroable};
use daedalus_content::{
    assets::{
        GeometryResource, ImageAsset, ImageSampler, ImportedAsset, LeafCookKey, MipmapFilter,
        StaticModel, TextureFilter, TextureWrap,
    },
    project::PreparedProject,
    scene::StableId,
};
use daedalus_core::{EntityHandle, RenderSnapshot, SessionId, Tick};
use glam::{Mat3, Mat4, Quat, Vec3};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[path = "packet_cpu.rs"]
mod packet_cpu;
pub(crate) use packet_cpu::ManagedResourceSetToken;
pub use packet_cpu::{
    CpuFrameHandle, CpuGenerationId, CpuReclaimStatus, PacketCpuLimits, PacketCpuOwner,
    PacketCpuStatus, PacketStageInput, SourcePin,
};
use packet_cpu::{GenerationBacking, GenerationRoots, ResourceRequests, StageRequestBacking};

use crate::{Camera, DirectionalLight, Material, Mesh, RendererError, Vertex};

const MESH_LAYOUT_VERSION: u32 = 1;
const MESH_HASH_DOMAIN: &[u8] = b"Daedalus renderer mesh payload\0v1\0";
const MODEL_TEXTURE_HASH_DOMAIN: &[u8] = b"Daedalus renderer model texture payload\0v1\0";
pub const PREPARED_RENDER_METADATA_LIMIT_BYTES: usize = 8 * 1024 * 1024;

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

/// Independent device and CPU upload budgets for immutable imported assets.
///
/// These limits are additive to the legacy packet geometry limits. Geometry's
/// existing preferred/hard byte ceilings remain sub-limits of the resident
/// budgets below; they are not added on top of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketAssetLimits {
    pub resident_working_bytes: usize,
    pub resident_hard_bytes: usize,
    pub max_active_textures: usize,
    pub max_total_textures: usize,
    pub max_active_samplers: usize,
    pub max_total_samplers: usize,
    pub max_active_bind_groups: usize,
    pub max_total_bind_groups: usize,
    pub upload_staging_bytes: usize,
    /// Maximum outstanding packet submissions retained until their completion callbacks run.
    pub max_in_flight_submissions: usize,
    pub cpu_upload_scratch_bytes: usize,
    pub bookkeeping_bytes: usize,
}

impl Default for PacketAssetLimits {
    fn default() -> Self {
        Self {
            resident_working_bytes: 128 * 1024 * 1024,
            resident_hard_bytes: 256 * 1024 * 1024,
            max_active_textures: 256,
            max_total_textures: 512,
            max_active_samplers: 256,
            max_total_samplers: 512,
            max_active_bind_groups: 1024,
            max_total_bind_groups: 2048,
            upload_staging_bytes: 128 * 1024 * 1024,
            max_in_flight_submissions: 3,
            cpu_upload_scratch_bytes: 128 * 1024 * 1024,
            bookkeeping_bytes: 8 * 1024 * 1024,
        }
    }
}

impl PacketAssetLimits {
    pub(crate) fn validate(self) -> Result<(), PacketRenderError> {
        let hard = Self::default();
        let doubled_resident = self.resident_working_bytes.checked_mul(2);
        let doubled_textures = self.max_active_textures.checked_mul(2);
        let doubled_samplers = self.max_active_samplers.checked_mul(2);
        let doubled_bind_groups = self.max_active_bind_groups.checked_mul(2);
        if self.resident_working_bytes == 0
            || self.resident_hard_bytes == 0
            || self.max_active_textures == 0
            || self.max_total_textures == 0
            || self.max_active_samplers == 0
            || self.max_total_samplers == 0
            || self.max_active_bind_groups == 0
            || self.max_total_bind_groups == 0
            || self.upload_staging_bytes == 0
            || self.max_in_flight_submissions == 0
            || self.cpu_upload_scratch_bytes == 0
            || self.bookkeeping_bytes == 0
            || self.resident_working_bytes > hard.resident_working_bytes
            || self.resident_hard_bytes > hard.resident_hard_bytes
            || self.max_active_textures > hard.max_active_textures
            || self.max_total_textures > hard.max_total_textures
            || self.max_active_samplers > hard.max_active_samplers
            || self.max_total_samplers > hard.max_total_samplers
            || self.max_active_bind_groups > hard.max_active_bind_groups
            || self.max_total_bind_groups > hard.max_total_bind_groups
            || self.upload_staging_bytes > hard.upload_staging_bytes
            || self.max_in_flight_submissions > hard.max_in_flight_submissions
            || self.cpu_upload_scratch_bytes > hard.cpu_upload_scratch_bytes
            || self.bookkeeping_bytes > hard.bookkeeping_bytes
            || self.resident_working_bytes > self.resident_hard_bytes
            || doubled_resident.is_none_or(|required| self.resident_hard_bytes < required)
            || doubled_textures.is_none_or(|required| self.max_total_textures < required)
            || doubled_samplers.is_none_or(|required| self.max_total_samplers < required)
            || doubled_bind_groups.is_none_or(|required| self.max_total_bind_groups < required)
        {
            return Err(PacketRenderError::InvalidAssetLimits);
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
    #[error(
        "packet asset limits must be positive, within hard ceilings, and leave a 2x transition reserve"
    )]
    InvalidAssetLimits,
    #[error("packet CPU limits must be positive, within hard ceilings, and internally consistent")]
    InvalidCpuLimits,
    #[error("renderer CPU {resource} budget exceeded ({bytes} bytes > {limit} bytes)")]
    CpuBudgetExceeded {
        resource: &'static str,
        bytes: usize,
        limit: usize,
    },
    #[error("renderer CPU {resource} capacity exceeded ({count} entries > {limit} entries)")]
    CpuLimitExceeded {
        resource: &'static str,
        count: usize,
        limit: usize,
    },
    #[error("renderer CPU generation was cancelled")]
    CpuCancelled,
    #[error("renderer CPU generation identifier is foreign, stale, or exhausted")]
    StaleCpuGeneration,
    #[error("renderer CPU owner is temporarily pinned by retained packet aliases")]
    CpuResidencyBusy,
    #[error("renderer CPU allocation reservation failed")]
    CpuAllocationFailed,
    #[error("invalid immutable mesh: {0}")]
    InvalidMesh(String),
    #[error("invalid render packet: {0}")]
    InvalidPacket(String),
    #[error(
        "packet candidate exceeds the preferred geometry working set ({bytes} bytes > {limit} bytes)"
    )]
    WorkingSetExceeded { bytes: usize, limit: usize },
    #[error(
        "packet asset candidate exceeds the resident working set ({bytes} bytes > {limit} bytes)"
    )]
    AssetWorkingSetExceeded { bytes: usize, limit: usize },
    #[error("packet asset {resource} budget exceeded ({bytes} bytes > {limit} bytes)")]
    AssetByteBudgetExceeded {
        resource: &'static str,
        bytes: usize,
        limit: usize,
    },
    #[error(
        "packet asset candidate exceeds {resource} capacity ({count} entries > {limit} entries)"
    )]
    AssetLimitExceeded {
        resource: &'static str,
        count: usize,
        limit: usize,
    },
    #[error("packet cache is temporarily pinned by in-flight work and cannot admit this candidate")]
    ResidencyBusy,
    #[error("the adapter limits do not support the packet vertex layout: {0}")]
    DeviceLimit(String),
    #[error("sprite rendering does not support the presentation format: {0}")]
    UnsupportedPresentationFormat(String),
    #[error("renderer operation failed: {0}")]
    Renderer(#[from] RendererError),
}

/// Projection applied to the packet view. Perspective remains the default for
/// packets created through the original constructor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RenderProjection {
    Perspective,
    Orthographic { vertical_size: f32 },
}

/// Immutable camera pose and projection used to produce one packet view.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderView {
    camera: Camera,
    projection: RenderProjection,
}

impl RenderView {
    pub fn new(camera: Camera, projection: RenderProjection) -> Self {
        Self { camera, projection }
    }

    pub fn perspective(camera: Camera) -> Self {
        Self::new(camera, RenderProjection::Perspective)
    }

    pub fn camera(&self) -> &Camera {
        &self.camera
    }

    pub fn projection(&self) -> RenderProjection {
        self.projection
    }

    pub fn with_camera(&self, camera: Camera) -> Self {
        Self {
            camera,
            projection: self.projection,
        }
    }

    pub fn with_projection(&self, projection: RenderProjection) -> Self {
        Self {
            camera: self.camera.clone(),
            projection,
        }
    }

    pub fn validate(&self) -> Result<(), PacketRenderError> {
        self.camera
            .validate()
            .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        if let RenderProjection::Orthographic { vertical_size } = self.projection
            && (!vertical_size.is_finite() || !(0.001..=1_000_000.0).contains(&vertical_size))
        {
            return Err(PacketRenderError::InvalidPacket(
                "orthographic vertical size must be finite and within [0.001, 1000000]".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn view_projection(&self, aspect: f32) -> Result<Mat4, PacketRenderError> {
        self.validate()?;
        if !aspect.is_finite() || aspect <= 0.0 {
            return Err(PacketRenderError::InvalidPacket(
                "view aspect ratio must be finite and positive".into(),
            ));
        }
        let matrix = match self.projection {
            RenderProjection::Perspective => self.camera.view_projection(aspect),
            RenderProjection::Orthographic { vertical_size } => {
                let half_height = vertical_size * 0.5;
                let half_width = half_height * aspect;
                let view = Mat4::look_at_rh(
                    glam::Vec3::from_array(self.camera.position),
                    glam::Vec3::from_array(self.camera.target),
                    glam::Vec3::from_array(self.camera.up),
                );
                Mat4::orthographic_rh(
                    -half_width,
                    half_width,
                    -half_height,
                    half_height,
                    self.camera.near,
                    self.camera.far,
                ) * view
            }
        };
        if !matrix.is_finite() {
            return Err(PacketRenderError::InvalidPacket(
                "view projection matrix is non-finite".into(),
            ));
        }
        Ok(matrix)
    }
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
    backing: MeshBacking,
    gpu_bytes: usize,
    source_pin: Option<SourcePin>,
}

#[derive(Debug)]
enum MeshBacking {
    Legacy(Mesh),
    Imported(Arc<GeometryResource>),
    SpriteQuad(SpriteQuadGeometry),
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub(crate) struct SpriteQuadVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
}

#[derive(Clone, Debug)]
pub(crate) struct SpriteQuadGeometry {
    pub vertices: [SpriteQuadVertex; 4],
    pub indices: [u32; 6],
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
            backing: MeshBacking::Legacy(mesh),
            gpu_bytes,
            source_pin: None,
        })))
    }

    fn imported(
        asset_id: StableId,
        generation: u64,
        geometry: Arc<GeometryResource>,
    ) -> Result<Self, PacketRenderError> {
        validate_imported_geometry(&geometry)?;
        let gpu_bytes = geometry
            .vertex_count()
            .checked_mul(32)
            .and_then(|bytes| {
                geometry
                    .index_count()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|indices| bytes.checked_add(indices))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidMesh("imported GPU geometry byte size overflowed".into())
            })?;
        Ok(Self(Arc::new(MeshResource {
            asset_id,
            generation,
            payload_hash: geometry.key().digest(),
            layout_version: MODEL_MESH_LAYOUT_VERSION,
            backing: MeshBacking::Imported(geometry),
            gpu_bytes,
            source_pin: None,
        })))
    }

    fn sprite_quad(asset_id: StableId, generation: u64) -> Self {
        let vertices = [
            SpriteQuadVertex {
                position: [0.0, 0.0],
                uv: [0.0, 1.0],
            },
            SpriteQuadVertex {
                position: [1.0, 0.0],
                uv: [1.0, 1.0],
            },
            SpriteQuadVertex {
                position: [1.0, 1.0],
                uv: [1.0, 0.0],
            },
            SpriteQuadVertex {
                position: [0.0, 1.0],
                uv: [0.0, 0.0],
            },
        ];
        let indices = [0, 1, 2, 0, 2, 3];
        let payload_hash = canonical_sprite_quad_hash(&vertices, &indices);
        Self(Arc::new(MeshResource {
            asset_id,
            generation,
            payload_hash,
            layout_version: SPRITE_MESH_LAYOUT_VERSION,
            backing: MeshBacking::SpriteQuad(SpriteQuadGeometry { vertices, indices }),
            gpu_bytes: 88,
            source_pin: None,
        }))
    }

    fn pin_source(&mut self, source_pin: Option<&SourcePin>) {
        Arc::get_mut(&mut self.0)
            .expect("fresh mesh handles are uniquely owned while pinned")
            .source_pin = source_pin.cloned();
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
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => mesh.vertices.len(),
            MeshBacking::Imported(geometry) => geometry.vertex_count(),
            MeshBacking::SpriteQuad(geometry) => geometry.vertices.len(),
        }
    }
    pub fn index_count(&self) -> usize {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => mesh.indices.len(),
            MeshBacking::Imported(geometry) => geometry.index_count(),
            MeshBacking::SpriteQuad(geometry) => geometry.indices.len(),
        }
    }
    pub(crate) fn legacy_mesh(&self) -> Option<&Mesh> {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => Some(mesh),
            MeshBacking::Imported(_) | MeshBacking::SpriteQuad(_) => None,
        }
    }
    pub(crate) fn imported_geometry(&self) -> Option<&Arc<GeometryResource>> {
        match &self.0.backing {
            MeshBacking::Legacy(_) => None,
            MeshBacking::Imported(geometry) => Some(geometry),
            MeshBacking::SpriteQuad(_) => None,
        }
    }
    pub(crate) fn sprite_quad_geometry(&self) -> Option<&SpriteQuadGeometry> {
        match &self.0.backing {
            MeshBacking::SpriteQuad(geometry) => Some(geometry),
            MeshBacking::Legacy(_) | MeshBacking::Imported(_) => None,
        }
    }
    pub(crate) fn is_sprite_quad(&self) -> bool {
        matches!(self.0.backing, MeshBacking::SpriteQuad(_))
    }
    pub(crate) fn indices(&self) -> &[u32] {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => &mesh.indices,
            MeshBacking::Imported(geometry) => geometry.indices(),
            MeshBacking::SpriteQuad(geometry) => &geometry.indices,
        }
    }
    pub fn gpu_bytes(&self) -> usize {
        self.0.gpu_bytes
    }

    fn legacy_copy_bytes(&self) -> Result<usize, PacketRenderError> {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => {
                let vertices = mesh
                    .vertices
                    .capacity()
                    .checked_mul(std::mem::size_of::<Vertex>())
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(
                            "legacy vertex copy size overflowed".into(),
                        )
                    })?;
                let indices = mesh
                    .indices
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket("legacy index copy size overflowed".into())
                    })?;
                vertices.checked_add(indices).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("legacy mesh copy size overflowed".into())
                })
            }
            MeshBacking::Imported(_) | MeshBacking::SpriteQuad(_) => Ok(0),
        }
    }
    pub(crate) fn key(&self) -> GeometryKey {
        GeometryKey {
            hash: self.0.payload_hash,
            layout: self.0.layout_version,
        }
    }
}

const MODEL_MESH_LAYOUT_VERSION: u32 = 2;
const SPRITE_MESH_LAYOUT_VERSION: u32 = 3;
const SPRITE_QUAD_HASH_DOMAIN: &[u8] = b"Daedalus renderer sprite quad\0v1\0";

fn canonical_sprite_quad_hash(vertices: &[SpriteQuadVertex; 4], indices: &[u32; 6]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SPRITE_QUAD_HASH_DOMAIN);
    hasher.update(SPRITE_MESH_LAYOUT_VERSION.to_le_bytes());
    for vertex in vertices {
        for value in vertex.position.into_iter().chain(vertex.uv) {
            hasher.update(value.to_bits().to_le_bytes());
        }
    }
    for index in indices {
        hasher.update(index.to_le_bytes());
    }
    hasher.finalize().into()
}

fn validate_imported_geometry(geometry: &GeometryResource) -> Result<(), PacketRenderError> {
    let positions = geometry.positions();
    let normals = geometry.normals();
    let indices = geometry.indices();
    if positions.is_empty()
        || positions.len() != normals.len()
        || geometry.uv0().is_some_and(|uv| uv.len() != positions.len())
        || indices.is_empty()
        || !indices.len().is_multiple_of(3)
        || positions
            .iter()
            .flatten()
            .chain(normals.iter().flatten())
            .chain(geometry.uv0().into_iter().flatten().flatten())
            .any(|value| !value.is_finite())
        || normals
            .iter()
            .any(|normal| normal.iter().all(|component| component.abs() < 1.0e-6))
        || indices
            .iter()
            .any(|index| *index as usize >= positions.len())
    {
        return Err(PacketRenderError::InvalidMesh(
            "imported geometry has invalid attributes, indices, or triangle topology".into(),
        ));
    }
    Ok(())
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TextureKey {
    pub hash: [u8; 32],
    pub format: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelTextureUpload {
    pub key: TextureKey,
    pub image: Arc<ImageAsset>,
    pub byte_len: usize,
    pub _source_pin: Option<SourcePin>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelDrawBinding {
    pub texture: Option<Arc<ModelTextureUpload>>,
    pub sampler: ImageSampler,
}

#[derive(Clone, Debug)]
pub(crate) struct SpriteDrawBinding {
    pub texture: Arc<ModelTextureUpload>,
    pub sampler: ImageSampler,
    pub uv_rect: [f32; 4],
    pub stable_id: StableId,
    pub sorting_layer: i32,
    pub order_in_layer: i32,
    pub tint: [f32; 4],
}

impl PartialEq for SpriteDrawBinding {
    fn eq(&self, other: &Self) -> bool {
        self.texture.key == other.texture.key
            && self.sampler == other.sampler
            && self.uv_rect == other.uv_rect
            && self.stable_id == other.stable_id
            && self.sorting_layer == other.sorting_layer
            && self.order_in_layer == other.order_in_layer
            && self.tint == other.tint
    }
}

impl Eq for SpriteDrawBinding {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SamplerKey(pub [u8; 5]);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct TextureSamplerKey {
    pub texture: Option<TextureKey>,
    pub sampler: SamplerKey,
}

pub(crate) fn sampler_key(sampler: ImageSampler) -> SamplerKey {
    SamplerKey([
        match sampler.min_filter {
            TextureFilter::Nearest => 0,
            TextureFilter::Linear => 1,
        },
        match sampler.mag_filter {
            TextureFilter::Nearest => 0,
            TextureFilter::Linear => 1,
        },
        match sampler.mipmap_filter {
            MipmapFilter::None => 0,
            MipmapFilter::Nearest => 1,
            MipmapFilter::Linear => 2,
        },
        match sampler.wrap_u {
            TextureWrap::ClampToEdge => 0,
            TextureWrap::MirroredRepeat => 1,
            TextureWrap::Repeat => 2,
        },
        match sampler.wrap_v {
            TextureWrap::ClampToEdge => 0,
            TextureWrap::MirroredRepeat => 1,
            TextureWrap::Repeat => 2,
        },
    ])
}

impl PartialEq for ModelDrawBinding {
    fn eq(&self, other: &Self) -> bool {
        self.texture.as_ref().map(|texture| texture.key)
            == other.texture.as_ref().map(|texture| texture.key)
            && self.sampler == other.sampler
    }
}

impl Eq for ModelDrawBinding {}

fn texture_upload_with_pin(
    image: Arc<ImageAsset>,
    source_pin: Option<&SourcePin>,
) -> Result<Arc<ModelTextureUpload>, PacketRenderError> {
    let mut hasher = Sha256::new();
    hasher.update(MODEL_TEXTURE_HASH_DOMAIN);
    hasher.update(1_u32.to_le_bytes()); // Rgba8UnormSrgb payload format.
    let base = image.decoded();
    hasher.update(base.width().to_le_bytes());
    hasher.update(base.height().to_le_bytes());
    let mut byte_len = base.rgba8().len();
    hasher.update((base.rgba8().len() as u64).to_le_bytes());
    hasher.update(base.rgba8());
    let expected_mip_count = full_mip_count(base.width(), base.height());
    if image.mips().len() + 1 != expected_mip_count && !image.mips().is_empty() {
        return Err(PacketRenderError::InvalidPacket(
            "texture mip chain is incomplete".into(),
        ));
    }
    let mut previous_width = base.width();
    let mut previous_height = base.height();
    for mip in image.mips() {
        let width = (previous_width / 2).max(1);
        let height = (previous_height / 2).max(1);
        let expected_bytes =
            usize::try_from(u64::from(width) * u64::from(height) * 4).map_err(|_| {
                PacketRenderError::InvalidPacket("model texture mip size overflowed".into())
            })?;
        if mip.width() != width || mip.height() != height || mip.rgba8().len() != expected_bytes {
            return Err(PacketRenderError::InvalidPacket(
                "model texture mip dimensions or byte length are invalid".into(),
            ));
        }
        byte_len = byte_len.checked_add(expected_bytes).ok_or_else(|| {
            PacketRenderError::InvalidPacket("model texture byte size overflowed".into())
        })?;
        hasher.update(width.to_le_bytes());
        hasher.update(height.to_le_bytes());
        hasher.update((expected_bytes as u64).to_le_bytes());
        hasher.update(mip.rgba8());
        previous_width = width;
        previous_height = height;
    }
    let hash = hasher.finalize().into();
    Ok(Arc::new(ModelTextureUpload {
        key: TextureKey { hash, format: 1 },
        image,
        byte_len,
        _source_pin: source_pin.cloned(),
    }))
}

fn full_mip_count(width: u32, height: u32) -> usize {
    let mut width = width;
    let mut height = height;
    let mut count = 1;
    while width > 1 || height > 1 {
        width = (width / 2).max(1);
        height = (height / 2).max(1);
        count += 1;
    }
    count
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

/// Dense per-generation draw slots shared by CPU frames and staged GPU input.
pub(crate) type ManagedSlotAssignments = Arc<HashMap<RenderInstanceId, usize>>;

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
    authority_snapshot_sequence: Option<u64>,
    project_generation: u64,
    source_digest: String,
    view: RenderView,
    pub(crate) directional_light: DirectionalLight,
    pub(crate) ambient_color: [f32; 3],
    pub(crate) clear_color: [f32; 4],
    instances: Arc<[RenderInstance]>,
    model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
    sprite_bindings: Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>,
    pub(crate) sprite_order: Arc<[RenderInstanceId]>,
    pub(crate) sprite_geometry: Option<GeometryKey>,
    validated_normals: Arc<OnceLock<HashMap<RenderInstanceId, CachedNormal>>>,
    source_pin: Option<SourcePin>,
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
            && self.authority_snapshot_sequence == other.authority_snapshot_sequence
            && self.project_generation == other.project_generation
            && self.source_digest == other.source_digest
            && self.view == other.view
            && self.directional_light == other.directional_light
            && self.ambient_color == other.ambient_color
            && self.clear_color == other.clear_color
            && self.instances == other.instances
            && self.model_bindings == other.model_bindings
            && self.sprite_bindings == other.sprite_bindings
            && self.sprite_order == other.sprite_order
            && self.sprite_geometry == other.sprite_geometry
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
            authority_snapshot_sequence: None,
            project_generation,
            source_digest: source_digest.into(),
            view: RenderView::perspective(camera),
            directional_light,
            ambient_color,
            clear_color,
            instances: instances.into(),
            model_bindings: Arc::new(HashMap::new()),
            sprite_bindings: Arc::new(HashMap::new()),
            sprite_order: Arc::from([]),
            sprite_geometry: None,
            validated_normals: Arc::new(OnceLock::new()),
            source_pin: None,
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
    pub fn authority_snapshot_sequence(&self) -> Option<u64> {
        self.authority_snapshot_sequence
    }
    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }
    pub fn camera(&self) -> &Camera {
        self.view.camera()
    }
    pub fn view(&self) -> &RenderView {
        &self.view
    }
    pub fn projection(&self) -> RenderProjection {
        self.view.projection()
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
    pub(crate) fn model_binding(&self, id: &RenderInstanceId) -> Option<&ModelDrawBinding> {
        self.model_bindings.get(id)
    }
    pub(crate) fn sprite_binding(&self, id: &RenderInstanceId) -> Option<&SpriteDrawBinding> {
        self.sprite_bindings.get(id)
    }
    pub fn with_camera(&self, camera: Camera) -> Self {
        let mut packet = self.clone();
        packet.view = packet.view.with_camera(camera);
        packet
    }
    pub fn with_view(&self, view: RenderView) -> Self {
        let mut packet = self.clone();
        packet.view = view;
        packet
    }
    pub fn with_projection(&self, projection: RenderProjection) -> Self {
        let mut packet = self.clone();
        packet.view = packet.view.with_projection(projection);
        packet
    }
    pub fn with_instances(&self, instances: Vec<RenderInstance>) -> Self {
        let mut packet = self.clone();
        packet.instances = instances.into();
        packet.model_bindings = Arc::new(HashMap::new());
        packet.sprite_bindings = Arc::new(HashMap::new());
        packet.sprite_order = Arc::from([]);
        packet.sprite_geometry = None;
        packet.validated_normals = Arc::new(OnceLock::new());
        packet
    }

    fn with_shared_model_bindings(
        mut self,
        model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
    ) -> Self {
        self.model_bindings = model_bindings;
        self
    }

    fn with_shared_sprite_bindings(
        mut self,
        sprite_bindings: Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>,
        sprite_order: Arc<[RenderInstanceId]>,
        sprite_geometry: Option<GeometryKey>,
    ) -> Self {
        self.sprite_bindings = sprite_bindings;
        self.sprite_order = sprite_order;
        self.sprite_geometry = sprite_geometry;
        self
    }

    #[cfg(any(feature = "window", test))]
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
        self.validate_reusing_with_cancel(limits, previous, &AtomicBool::new(false))
    }

    pub(crate) fn validate_reusing_with_cancel(
        &self,
        limits: PacketRendererLimits,
        previous: Option<&RenderPacket>,
        cancelled: &AtomicBool,
    ) -> Result<PacketValidation, PacketRenderError> {
        self.validate_reusing_with_slots(limits, previous, None, cancelled)
    }

    pub(crate) fn validate_managed_reusing_with_cancel(
        &self,
        limits: PacketRendererLimits,
        previous: Option<&RenderPacket>,
        slot_assignments: &ManagedSlotAssignments,
        cancelled: &AtomicBool,
    ) -> Result<PacketValidation, PacketRenderError> {
        self.validate_reusing_with_slots(limits, previous, Some(slot_assignments), cancelled)
    }

    fn validate_reusing_with_slots(
        &self,
        limits: PacketRendererLimits,
        previous: Option<&RenderPacket>,
        slot_assignments: Option<&ManagedSlotAssignments>,
        cancelled: &AtomicBool,
    ) -> Result<PacketValidation, PacketRenderError> {
        limits.validate()?;
        self.view.validate()?;
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
        if slot_assignments.is_some_and(|assignments| assignments.len() != self.instances.len()) {
            return Err(PacketRenderError::InvalidPacket(
                "managed slot assignment count does not match packet draws".into(),
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
        for (slot, instance) in self.instances.iter().enumerate() {
            packet_cpu::check_cancelled(cancelled)?;
            if instance.id.entity.session != self.session || !ids.insert(instance.id) {
                return Err(PacketRenderError::InvalidPacket(
                    "instance session mismatch or duplicate instance ID".into(),
                ));
            }
            if slot_assignments
                .is_some_and(|assignments| assignments.get(&instance.id) != Some(&slot))
            {
                return Err(PacketRenderError::InvalidPacket(
                    "managed slot assignments do not match packet draw order".into(),
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
            let model_binding = self.model_bindings.get(&instance.id);
            let sprite_binding = self.sprite_bindings.get(&instance.id);
            if instance.mesh.imported_geometry().is_some() != model_binding.is_some()
                || instance.mesh.is_sprite_quad() != sprite_binding.is_some()
                || (model_binding.is_some() && sprite_binding.is_some())
            {
                return Err(PacketRenderError::InvalidPacket(
                    "model draw binding does not match its imported geometry".into(),
                ));
            }
            if let Some(binding) = model_binding {
                let geometry = instance.mesh.imported_geometry().ok_or_else(|| {
                    PacketRenderError::InvalidPacket(
                        "model texture binding refers to legacy mesh geometry".into(),
                    )
                })?;
                if binding.texture.is_some() && geometry.uv0().is_none() {
                    return Err(PacketRenderError::InvalidPacket(
                        "textured model geometry has no UV0 coordinates".into(),
                    ));
                }
            }
            if let Some(binding) = sprite_binding {
                let [u, v, width, height] = binding.uv_rect;
                if !instance.mesh.is_sprite_quad()
                    || !binding.texture.image.mips().is_empty()
                    || [u, v, width, height]
                        .iter()
                        .any(|value| !value.is_finite() || *value < 0.0 || *value > 1.0)
                    || width <= 0.0
                    || height <= 0.0
                    || u + width > 1.0
                    || v + height > 1.0
                    || binding
                        .tint
                        .iter()
                        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                {
                    return Err(PacketRenderError::InvalidPacket(
                        "sprite draw binding has an invalid atlas UV rectangle, tint, or texture"
                            .into(),
                    ));
                }
            }
        }
        if self.model_bindings.keys().any(|id| !ids.contains(id)) {
            return Err(PacketRenderError::InvalidPacket(
                "model draw binding refers to a missing packet instance".into(),
            ));
        }
        if self.sprite_bindings.keys().any(|id| !ids.contains(id)) {
            return Err(PacketRenderError::InvalidPacket(
                "sprite draw binding refers to a missing packet instance".into(),
            ));
        }
        if self.sprite_order.len() != self.sprite_bindings.len()
            || self.sprite_geometry.is_some() == self.sprite_bindings.is_empty()
            || self
                .sprite_order
                .iter()
                .any(|id| !self.sprite_bindings.contains_key(id))
        {
            return Err(PacketRenderError::InvalidPacket(
                "sprite painter order does not match sprite bindings".into(),
            ));
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
            model_bindings: Arc::clone(&self.model_bindings),
            sprite_bindings: Arc::clone(&self.sprite_bindings),
            slot_assignments: slot_assignments.cloned(),
        })
    }
}

fn inverse_transpose(transform: Mat4) -> Result<Mat3, PacketRenderError> {
    let values = transform.to_cols_array();
    if values[3].abs() > 1.0e-6
        || values[7].abs() > 1.0e-6
        || values[11].abs() > 1.0e-6
        || (values[15] - 1.0).abs() > 1.0e-6
    {
        return Err(PacketRenderError::InvalidPacket(
            "instance transform must be affine within the 1e-6 tolerance".into(),
        ));
    }
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
    pub model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
    pub sprite_bindings: Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>,
    slot_assignments: Option<ManagedSlotAssignments>,
}

impl PacketValidation {
    pub(crate) fn resource_requests(&self) -> ResourceRequests<'_> {
        ResourceRequests {
            geometries: &self.geometries,
            model_bindings: &self.model_bindings,
            sprite_bindings: &self.sprite_bindings,
        }
    }

    pub(crate) fn slot_assignments(&self) -> Option<&ManagedSlotAssignments> {
        self.slot_assignments.as_ref()
    }
}

/// Source project resources prepared once, then paired with each authoritative snapshot.
pub struct ProjectRenderResources {
    expected_session: SessionId,
    project_generation: u64,
    source_digest: String,
    view: RenderView,
    camera_2d: Option<PreparedCamera2dState>,
    model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
    sprite_bindings: Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>,
    sprite_order: Arc<[RenderInstanceId]>,
    managed_slot_assignments: Option<ManagedSlotAssignments>,
    sprite_geometry: Option<GeometryKey>,
    directional_light: DirectionalLight,
    ambient_color: [f32; 3],
    clear_color: [f32; 4],
    bindings: Vec<(EntityHandle, ProjectBinding)>,
    geometry_bytes: usize,
    prepared_metadata_bytes: usize,
    legacy_mesh_copy_bytes: usize,
    expanded_instance_count: usize,
    limits: PacketRendererLimits,
    source_pin: Option<SourcePin>,
    stage_requests: Option<Arc<StageRequestBacking>>,
    managed_resource_set_token: Option<Arc<ManagedResourceSetToken>>,
    cpu_root_parent_capacity: usize,
    cpu_root_mesh_capacity: usize,
    cpu_root_texture_capacity: usize,
    cpu_root_leaf_capacity: usize,
}

struct ProjectBinding {
    stable_id: StableId,
    mesh: Option<MeshHandle>,
    material: Material,
    model_draws: Option<Arc<[PreparedModelDraw]>>,
    sprite_draw: Option<(MeshHandle, Material, PreparedSpriteDraw)>,
}

#[derive(Clone)]
struct PreparedModelDraw {
    ordinal: u32,
    mesh: MeshHandle,
    node_global: Mat4,
    material: Material,
    texture: Option<Arc<ModelTextureUpload>>,
    sampler: ImageSampler,
}

struct PreparedSpriteDraw {
    component_id: StableId,
    texture: Arc<ModelTextureUpload>,
    sampler: ImageSampler,
    uv_rect: [f32; 4],
    local: Mat4,
    tint: [f32; 4],
    sorting_layer: i32,
    order_in_layer: i32,
}

fn build_managed_slot_assignments(
    bindings: &[(EntityHandle, ProjectBinding)],
    expected_draw_count: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<ManagedSlotAssignments, PacketRenderError> {
    let mut assignments = HashMap::with_capacity(expected_draw_count);
    for (entity, binding) in bindings {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if binding.mesh.is_some() {
            insert_managed_slot(
                &mut assignments,
                RenderInstanceId {
                    entity: *entity,
                    primitive: 0,
                },
            )?;
        }
        if let Some(draws) = &binding.model_draws {
            for draw in draws.iter() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                insert_managed_slot(
                    &mut assignments,
                    RenderInstanceId {
                        entity: *entity,
                        primitive: draw.ordinal,
                    },
                )?;
            }
        }
        if binding.sprite_draw.is_some() {
            insert_managed_slot(
                &mut assignments,
                RenderInstanceId {
                    entity: *entity,
                    primitive: 0,
                },
            )?;
        }
    }
    if assignments.len() != expected_draw_count {
        return Err(PacketRenderError::InvalidPacket(
            "managed slot assignment count disagrees with prepared draws".into(),
        ));
    }
    Ok(Arc::new(assignments))
}

fn insert_managed_slot(
    assignments: &mut HashMap<RenderInstanceId, usize>,
    id: RenderInstanceId,
) -> Result<(), PacketRenderError> {
    if assignments.contains_key(&id) {
        return Err(PacketRenderError::InvalidPacket(
            "prepared draws contain a duplicate render instance ID".into(),
        ));
    }
    let slot = assignments.len();
    assignments.insert(id, slot);
    Ok(())
}

#[derive(Clone)]
struct PreparedCamera2dState {
    entity: StableId,
    handle: EntityHandle,
    vertical_size: f32,
    near: f32,
    far: f32,
}

fn push_mesh_root(
    mesh: &MeshHandle,
    seen: &mut HashSet<usize>,
    roots: &mut Vec<Option<GenerationBacking>>,
) {
    if seen.insert(Arc::as_ptr(&mesh.0) as usize) {
        roots.push(Some(GenerationBacking::Mesh(Arc::clone(&mesh.0))));
    }
}

fn push_texture_root(
    texture: &Arc<ModelTextureUpload>,
    seen: &mut HashSet<usize>,
    roots: &mut Vec<Option<GenerationBacking>>,
) {
    if seen.insert(Arc::as_ptr(texture) as usize) {
        roots.push(Some(GenerationBacking::Texture(Arc::clone(texture))));
    }
}

impl ProjectRenderResources {
    pub fn prepare(
        project: &PreparedProject,
        project_generation: u64,
        expected_session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        limits: PacketRendererLimits,
    ) -> Result<Self, PacketRenderError> {
        Self::prepare_with_metadata_limit(
            project,
            project_generation,
            expected_session,
            authored_handles,
            limits,
            PREPARED_RENDER_METADATA_LIMIT_BYTES,
        )
    }

    fn prepare_with_metadata_limit(
        project: &PreparedProject,
        project_generation: u64,
        expected_session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        limits: PacketRendererLimits,
        metadata_limit: usize,
    ) -> Result<Self, PacketRenderError> {
        Self::prepare_with_options(
            project,
            project_generation,
            expected_session,
            authored_handles,
            limits,
            metadata_limit,
            usize::MAX,
            None,
            None,
            None,
            None,
        )
    }

    // Keep generation identity, validation limits, source retention, and byte
    // budgets explicit; they are separately checked ownership inputs.
    #[allow(clippy::too_many_arguments)]
    fn prepare_managed(
        project: &PreparedProject,
        project_generation: u64,
        expected_session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
        source_pin: &SourcePin,
        resource_set_token: Arc<ManagedResourceSetToken>,
        cancelled: &AtomicBool,
        metadata_limit: usize,
        legacy_copy_limit: usize,
    ) -> Result<Self, PacketRenderError> {
        asset_limits.validate()?;
        Self::prepare_with_options(
            project,
            project_generation,
            expected_session,
            authored_handles,
            limits,
            metadata_limit,
            legacy_copy_limit,
            Some(source_pin),
            Some(asset_limits),
            Some(cancelled),
            Some(resource_set_token),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_with_options(
        project: &PreparedProject,
        project_generation: u64,
        expected_session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        limits: PacketRendererLimits,
        metadata_limit: usize,
        legacy_copy_limit: usize,
        source_pin: Option<&SourcePin>,
        asset_limits: Option<PacketAssetLimits>,
        cancelled: Option<&AtomicBool>,
        managed_resource_set_token: Option<Arc<ManagedResourceSetToken>>,
    ) -> Result<Self, PacketRenderError> {
        limits.validate()?;
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if authored_handles.len() != project.entities().len() {
            return Err(PacketRenderError::InvalidPacket(
                "authored binding count does not match prepared entities".into(),
            ));
        }
        let mut used_handles = HashSet::with_capacity(authored_handles.len());
        for entity in project.entities() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
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
        let asset_catalog =
            if project.model_renderers().is_empty() && project.sprite_renderers().is_empty() {
                None
            } else {
                Some(project.asset_catalog().ok_or_else(|| {
                    PacketRenderError::InvalidPacket(
                        "asset renderer bindings require a prepared asset catalog".into(),
                    )
                })?)
            };
        let mut selected_models =
            HashMap::<&StableId, (&Arc<StaticModel>, ModelExpansionStats)>::with_capacity(
                project.model_renderers().len(),
            );
        if let Some(catalog) = asset_catalog {
            for binding in project.model_renderers().values() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                let model_id = binding.model_asset_id();
                if selected_models.contains_key(model_id) {
                    continue;
                }
                let Some(ImportedAsset::Model(model)) = catalog.asset(model_id) else {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "model renderer references missing or non-model asset {model_id}"
                    )));
                };
                let stats = model_expansion_stats(model, limits, cancelled)?;
                selected_models.insert(model_id, (model, stats));
            }
        }
        let mut expanded_instance_count = 0usize;
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        let mut model_binding_count = 0usize;
        let mut legacy_geometry_reference_count = 0usize;
        for entity in project.entities() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            if let Some(mesh_id) = &entity.mesh {
                legacy_geometry_reference_count = legacy_geometry_reference_count
                    .checked_add(1)
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(
                            "legacy geometry reference count overflowed".into(),
                        )
                    })?;
                let mesh = project.meshes().get(mesh_id).ok_or_else(|| {
                    PacketRenderError::InvalidPacket(format!(
                        "entity {} references a missing mesh",
                        entity.id
                    ))
                })?;
                expanded_instance_count = checked_add_limit(
                    expanded_instance_count,
                    1,
                    limits.max_instances,
                    "prepared draw count",
                )?;
                expanded_vertices = checked_add_limit(
                    expanded_vertices,
                    mesh.vertices.len(),
                    limits.max_expanded_vertices,
                    "expanded vertex count",
                )?;
                expanded_indices = checked_add_limit(
                    expanded_indices,
                    mesh.indices.len(),
                    limits.max_expanded_indices,
                    "expanded index count",
                )?;
            }
            if let Some(binding) = project.model_renderers().get(&entity.id) {
                let (_, stats) =
                    selected_models
                        .get(binding.model_asset_id())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(format!(
                                "model renderer {} has no preflight stats",
                                binding.component_id()
                            ))
                        })?;
                expanded_instance_count = checked_add_limit(
                    expanded_instance_count,
                    stats.draws,
                    limits.max_instances,
                    "prepared model draw count",
                )?;
                model_binding_count =
                    model_binding_count
                        .checked_add(stats.draws)
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "model binding count overflowed".into(),
                            )
                        })?;
                expanded_vertices = checked_add_limit(
                    expanded_vertices,
                    stats.vertices,
                    limits.max_expanded_vertices,
                    "expanded model vertex count",
                )?;
                expanded_indices = checked_add_limit(
                    expanded_indices,
                    stats.indices,
                    limits.max_expanded_indices,
                    "expanded model index count",
                )?;
            }
            if project.sprite_renderers().contains_key(&entity.id) {
                expanded_instance_count = checked_add_limit(
                    expanded_instance_count,
                    1,
                    limits.max_instances,
                    "prepared sprite draw count",
                )?;
                expanded_vertices = checked_add_limit(
                    expanded_vertices,
                    4,
                    limits.max_expanded_vertices,
                    "expanded sprite vertex count",
                )?;
                expanded_indices = checked_add_limit(
                    expanded_indices,
                    6,
                    limits.max_expanded_indices,
                    "expanded sprite index count",
                )?;
            }
        }
        let sprite_binding_count = project.sprite_renderers().len();
        let legacy_mesh_copy_bytes = packet_cpu::legacy_copy_bytes(project, cancelled)?;
        if legacy_mesh_copy_bytes > legacy_copy_limit {
            return Err(PacketRenderError::CpuBudgetExceeded {
                resource: "legacy mesh copy",
                bytes: legacy_mesh_copy_bytes,
                limit: legacy_copy_limit,
            });
        }
        let mut selected_geometry_count = 0usize;
        for (_, stats) in selected_models.values() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            selected_geometry_count = selected_geometry_count
                .checked_add(stats.geometry_keys.len())
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("geometry count estimate overflowed".into())
                })?;
        }
        let estimated_geometry_count = legacy_geometry_reference_count
            .checked_add(selected_geometry_count)
            .and_then(|count| count.checked_add(usize::from(sprite_binding_count > 0)))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("geometry count estimate overflowed".into())
            })?;
        let prepared_metadata_bytes = prepared_render_metadata_bytes(
            project,
            &selected_models,
            model_binding_count,
            sprite_binding_count,
            estimated_geometry_count,
            if managed_resource_set_token.is_some() {
                expanded_instance_count
            } else {
                0
            },
            source_pin.is_some(),
            cancelled,
        )?;
        if prepared_metadata_bytes > metadata_limit {
            return Err(PacketRenderError::AssetByteBudgetExceeded {
                resource: "prepared CPU metadata",
                bytes: prepared_metadata_bytes,
                limit: metadata_limit,
            });
        }

        let mut mesh_handles = HashMap::with_capacity(project.meshes().len());
        let mesh_limits = MeshResourceLimits::default();
        for (asset_id, source) in project.meshes() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            let mesh = Mesh::new(
                source
                    .vertices
                    .iter()
                    .map(|vertex| Vertex::new(vertex.position, vertex.normal))
                    .collect(),
                source.indices.clone(),
            );
            let mut handle =
                MeshHandle::new(asset_id.clone(), project_generation, mesh, mesh_limits)?;
            handle.pin_source(source_pin);
            mesh_handles.insert(asset_id.clone(), handle);
        }

        let mut sprite_quad = if project.sprite_renderers().is_empty() {
            None
        } else {
            Some(MeshHandle::sprite_quad(
                StableId::new("daedalus.internal.sprite-quad")
                    .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?,
                project_generation,
            ))
        };
        if let Some(mesh) = &mut sprite_quad {
            mesh.pin_source(source_pin);
        }

        // Count every selected-scene occurrence for every owning entity before
        // constructing draw templates or hashing any texture payload.
        let mut imported_geometries = HashMap::<LeafCookKey, MeshHandle>::new();
        let mut texture_uploads = HashMap::<LeafCookKey, Arc<ModelTextureUpload>>::new();
        let mut model_templates = HashMap::<StableId, Arc<[PreparedModelDraw]>>::new();
        for (model_id, (model, stats)) in &selected_models {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            let draws = expand_model_draws(
                model_id,
                model,
                project_generation,
                &mut imported_geometries,
                &mut texture_uploads,
                stats.draws,
                source_pin,
                cancelled,
            )?;
            model_templates.insert((*model_id).clone(), draws.into());
        }
        let mut sprite_templates = HashMap::<StableId, PreparedSpriteDraw>::new();
        if !project.sprite_renderers().is_empty() {
            let catalog = project.asset_catalog().ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "sprite renderer bindings require a prepared asset catalog".into(),
                )
            })?;
            for (entity_id, sprite_binding) in project.sprite_renderers() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                let Some(ImportedAsset::Atlas(atlas)) =
                    catalog.asset(sprite_binding.atlas_asset_id())
                else {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "sprite renderer references missing or non-atlas asset {}",
                        sprite_binding.atlas_asset_id()
                    )));
                };
                let region = find_sprite_region(atlas, sprite_binding.region_id(), cancelled)?
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(format!(
                            "sprite renderer {} references missing atlas region {}",
                            sprite_binding.component_id(),
                            sprite_binding.region_id()
                        ))
                    })?;
                let Some(ImportedAsset::Image(image)) = catalog.asset(atlas.image_asset()) else {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "sprite atlas {} references a missing or non-image asset {}",
                        sprite_binding.atlas_asset_id(),
                        atlas.image_asset()
                    )));
                };
                let image_key = image.key();
                let texture = if let Some(upload) = texture_uploads.get(&image_key) {
                    Arc::clone(upload)
                } else {
                    let upload = texture_upload_with_pin(Arc::clone(image), source_pin)?;
                    texture_uploads.insert(image_key, Arc::clone(&upload));
                    upload
                };
                let [x, y, width, height] = region.rect();
                let image_width = image.decoded().width();
                let image_height = image.decoded().height();
                let ppu = region.pixels_per_unit();
                let pivot = region.pivot();
                let scale = Vec3::new(width as f32 / ppu, height as f32 / ppu, 1.0);
                let translation = Vec3::new(-pivot[0] * scale.x, -pivot[1] * scale.y, 0.0);
                let local = Mat4::from_translation(translation) * Mat4::from_scale(scale);
                let tint = sprite_binding.tint();
                if tint
                    .iter()
                    .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
                    || image_width == 0
                    || image_height == 0
                {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "sprite renderer {} has invalid tint or atlas dimensions",
                        sprite_binding.component_id()
                    )));
                }
                sprite_templates.insert(
                    entity_id.clone(),
                    PreparedSpriteDraw {
                        component_id: sprite_binding.component_id().clone(),
                        texture,
                        sampler: image.sampler(),
                        uv_rect: [
                            x as f32 / image_width as f32,
                            y as f32 / image_height as f32,
                            width as f32 / image_width as f32,
                            height as f32 / image_height as f32,
                        ],
                        local,
                        tint,
                        sorting_layer: sprite_binding.sorting_layer(),
                        order_in_layer: sprite_binding.order_in_layer(),
                    },
                );
            }
        }
        let mut all_geometry_keys = HashSet::new();
        let mut geometry_bytes = 0usize;
        for entity in project.entities() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            if let Some(mesh_id) = &entity.mesh {
                let handle = mesh_handles.get(mesh_id).ok_or_else(|| {
                    PacketRenderError::InvalidPacket(format!(
                        "entity {} references a missing mesh",
                        entity.id
                    ))
                })?;
                if all_geometry_keys.insert(handle.key()) {
                    geometry_bytes =
                        geometry_bytes
                            .checked_add(handle.gpu_bytes())
                            .ok_or_else(|| {
                                PacketRenderError::InvalidPacket(
                                    "project geometry bytes overflowed".into(),
                                )
                            })?;
                }
            }
        }
        for handle in imported_geometries.values().chain(sprite_quad.iter()) {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            if all_geometry_keys.insert(handle.key()) {
                geometry_bytes =
                    geometry_bytes
                        .checked_add(handle.gpu_bytes())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "project geometry bytes overflowed".into(),
                            )
                        })?;
            }
        }
        if all_geometry_keys.len() > limits.max_active_meshes
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
        let camera_2d = project
            .camera_2d()
            .map(
                |binding| -> Result<(RenderView, PreparedCamera2dState), PacketRenderError> {
                    let mut camera_entity = None;
                    for entity in project.entities() {
                        if let Some(cancelled) = cancelled {
                            packet_cpu::check_cancelled(cancelled)?;
                        }
                        if entity.id == *binding.entity_id() {
                            camera_entity = Some(entity);
                            break;
                        }
                    }
                    let entity = camera_entity.ok_or_else(|| {
                        PacketRenderError::InvalidPacket(format!(
                            "2D camera entity {} is missing",
                            binding.entity_id()
                        ))
                    })?;
                    let handle = authored_handles[binding.entity_id()];
                    let camera = camera_from_transform(
                        entity.transform.translation,
                        entity.transform.rotation,
                        entity.transform.scale,
                        binding.near(),
                        binding.far(),
                    )?;
                    let view = RenderView::new(
                        camera,
                        RenderProjection::Orthographic {
                            vertical_size: binding.vertical_size(),
                        },
                    );
                    view.validate()?;
                    Ok((
                        view,
                        PreparedCamera2dState {
                            entity: binding.entity_id().clone(),
                            handle,
                            vertical_size: binding.vertical_size(),
                            near: binding.near(),
                            far: binding.far(),
                        },
                    ))
                },
            )
            .transpose()?;
        let (view, camera_2d) = camera_2d.map_or_else(
            || (RenderView::perspective(camera.clone()), None),
            |(view, state)| (view, Some(state)),
        );
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
        let mut prepared_draws = 0usize;
        let mut prepared_vertices = 0usize;
        let mut prepared_indices = 0usize;
        let mut prepared_model_bindings = HashMap::with_capacity(model_binding_count);
        let mut prepared_sprite_bindings = HashMap::with_capacity(sprite_binding_count);
        for entity in project.entities() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
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
            let model_draws = project
                .model_renderers()
                .get(&entity.id)
                .map(|model_binding| {
                    let draws = model_templates
                        .get(model_binding.model_asset_id())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(format!(
                                "model renderer {} was not prepared",
                                model_binding.component_id()
                            ))
                        })?;
                    if mesh.is_some() {
                        return Err(PacketRenderError::InvalidPacket(format!(
                            "entity {} contains both a legacy mesh and model renderer",
                            entity.id
                        )));
                    }
                    Ok(Arc::clone(draws))
                })
                .transpose()?;
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
            if let Some(mesh) = &mesh {
                prepared_draws = prepared_draws.checked_add(1).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("prepared instance count overflowed".into())
                })?;
                prepared_vertices = prepared_vertices
                    .checked_add(mesh.vertex_count())
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket("expanded vertex count overflowed".into())
                    })?;
                prepared_indices = prepared_indices
                    .checked_add(mesh.index_count())
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket("expanded index count overflowed".into())
                    })?;
            }
            if let Some(draws) = &model_draws {
                prepared_draws = prepared_draws.checked_add(draws.len()).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("prepared instance count overflowed".into())
                })?;
                for draw in draws.iter() {
                    let id = RenderInstanceId {
                        entity: handle,
                        primitive: draw.ordinal,
                    };
                    prepared_model_bindings.insert(
                        id,
                        ModelDrawBinding {
                            texture: draw.texture.clone(),
                            sampler: draw.sampler,
                        },
                    );
                    prepared_vertices = prepared_vertices
                        .checked_add(draw.mesh.vertex_count())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "expanded vertex count overflowed".into(),
                            )
                        })?;
                    prepared_indices = prepared_indices
                        .checked_add(draw.mesh.index_count())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "expanded index count overflowed".into(),
                            )
                        })?;
                }
            }
            let sprite_draw = if let Some(sprite) = sprite_templates.get(&entity.id) {
                let material = Material {
                    base_color: [sprite.tint[0], sprite.tint[1], sprite.tint[2]],
                    metallic: 0.0,
                    roughness: 1.0,
                    emissive: [0.0; 3],
                };
                material
                    .validate()
                    .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
                prepared_draws = prepared_draws.checked_add(1).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("prepared instance count overflowed".into())
                })?;
                prepared_vertices = prepared_vertices.checked_add(4).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded vertex count overflowed".into())
                })?;
                prepared_indices = prepared_indices.checked_add(6).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded index count overflowed".into())
                })?;
                let id = RenderInstanceId {
                    entity: handle,
                    primitive: 0,
                };
                prepared_sprite_bindings.insert(
                    id,
                    SpriteDrawBinding {
                        texture: Arc::clone(&sprite.texture),
                        sampler: sprite.sampler,
                        uv_rect: sprite.uv_rect,
                        stable_id: entity.id.clone(),
                        sorting_layer: sprite.sorting_layer,
                        order_in_layer: sprite.order_in_layer,
                        tint: sprite.tint,
                    },
                );
                Some((
                    sprite_quad
                        .as_ref()
                        .expect("sprite quad exists when sprite templates exist")
                        .clone(),
                    material,
                    PreparedSpriteDraw {
                        component_id: sprite.component_id.clone(),
                        texture: Arc::clone(&sprite.texture),
                        sampler: sprite.sampler,
                        uv_rect: sprite.uv_rect,
                        local: sprite.local,
                        tint: sprite.tint,
                        sorting_layer: sprite.sorting_layer,
                        order_in_layer: sprite.order_in_layer,
                    },
                ))
            } else {
                None
            };
            bindings.push((
                handle,
                ProjectBinding {
                    stable_id: entity.id.clone(),
                    mesh,
                    material,
                    model_draws,
                    sprite_draw,
                },
            ));
        }
        if prepared_draws != expanded_instance_count
            || prepared_vertices != expanded_vertices
            || prepared_indices != expanded_indices
        {
            return Err(PacketRenderError::InvalidPacket(
                "prepared model draw templates disagree with preflight counts".into(),
            ));
        }
        let mut sprite_order = prepared_sprite_bindings.keys().copied().collect::<Vec<_>>();
        sprite_order.sort_by(|left, right| {
            let left_binding = &prepared_sprite_bindings[left];
            let right_binding = &prepared_sprite_bindings[right];
            left_binding
                .sorting_layer
                .cmp(&right_binding.sorting_layer)
                .then_with(|| {
                    left_binding
                        .order_in_layer
                        .cmp(&right_binding.order_in_layer)
                })
                .then_with(|| left_binding.stable_id.cmp(&right_binding.stable_id))
        });
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        let sprite_geometry = sprite_quad.as_ref().map(MeshHandle::key);
        let model_bindings = Arc::new(prepared_model_bindings);
        let sprite_bindings = Arc::new(prepared_sprite_bindings);
        let sprite_order: Arc<[RenderInstanceId]> = sprite_order.into();
        let managed_slot_assignments = managed_resource_set_token
            .as_ref()
            .map(|_| build_managed_slot_assignments(&bindings, expanded_instance_count, cancelled))
            .transpose()?;
        let stage_requests =
            if let (Some(source_pin), Some(asset_limits), Some(resource_set_token)) = (
                source_pin,
                asset_limits,
                managed_resource_set_token.as_ref(),
            ) {
                let mut geometries = HashMap::with_capacity(estimated_geometry_count);
                for (_, binding) in &bindings {
                    if let Some(cancelled) = cancelled {
                        packet_cpu::check_cancelled(cancelled)?;
                    }
                    if let Some(mesh) = &binding.mesh {
                        geometries.entry(mesh.key()).or_insert_with(|| mesh.clone());
                    }
                    if let Some(draws) = &binding.model_draws {
                        for draw in draws.iter() {
                            if let Some(cancelled) = cancelled {
                                packet_cpu::check_cancelled(cancelled)?;
                            }
                            geometries
                                .entry(draw.mesh.key())
                                .or_insert_with(|| draw.mesh.clone());
                        }
                    }
                    if let Some((mesh, _, _)) = &binding.sprite_draw {
                        geometries.entry(mesh.key()).or_insert_with(|| mesh.clone());
                    }
                }
                let mut texture_keys = HashSet::with_capacity(texture_uploads.len());
                for texture in texture_uploads.values() {
                    if let Some(cancelled) = cancelled {
                        packet_cpu::check_cancelled(cancelled)?;
                    }
                    texture_keys.insert(texture.key);
                }
                Some(Arc::new(StageRequestBacking {
                    session: expected_session,
                    project_generation,
                    source_digest: project.digest().to_owned(),
                    geometry_count: geometries.len(),
                    texture_count: texture_keys.len(),
                    draw_count: expanded_instance_count,
                    model_draw_count: model_binding_count,
                    sprite_draw_count: sprite_binding_count,
                    expanded_vertex_count: expanded_vertices,
                    expanded_index_count: expanded_indices,
                    packet_limits: limits,
                    asset_limits,
                    source_pin: source_pin.clone(),
                    geometries: Arc::new(geometries),
                    model_bindings: Arc::clone(&model_bindings),
                    sprite_bindings: Arc::clone(&sprite_bindings),
                    slot_assignments: Arc::clone(managed_slot_assignments.as_ref().ok_or_else(
                        || {
                            PacketRenderError::InvalidPacket(
                                "managed stage request is missing slot assignments".into(),
                            )
                        },
                    )?),
                    resource_set_token: Arc::clone(resource_set_token),
                }))
            } else {
                None
            };
        let actual_metadata_bytes = prepared_render_metadata_bytes(
            project,
            &selected_models,
            model_bindings.capacity(),
            sprite_bindings.capacity(),
            stage_requests
                .as_ref()
                .map_or(0, |requests| requests.geometries.capacity()),
            managed_slot_assignments
                .as_ref()
                .map_or(0, |assignments| assignments.capacity()),
            stage_requests.is_some(),
            cancelled,
        )?;
        if actual_metadata_bytes > metadata_limit {
            return Err(PacketRenderError::AssetByteBudgetExceeded {
                resource: "prepared CPU metadata",
                bytes: actual_metadata_bytes,
                limit: metadata_limit,
            });
        }
        let actual_legacy_mesh_copy_bytes =
            mesh_handles.values().try_fold(0usize, |sum, mesh| {
                sum.checked_add(mesh.legacy_copy_bytes()?).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("legacy mesh copy size overflowed".into())
                })
            })?;
        if actual_legacy_mesh_copy_bytes > legacy_copy_limit {
            return Err(PacketRenderError::CpuBudgetExceeded {
                resource: "legacy mesh copy",
                bytes: actual_legacy_mesh_copy_bytes,
                limit: legacy_copy_limit,
            });
        }
        let cpu_root_parent_capacity =
            5usize.checked_add(selected_models.len()).ok_or_else(|| {
                PacketRenderError::InvalidPacket("prepared root count overflowed".into())
            })?;
        let cpu_root_mesh_capacity = legacy_geometry_reference_count
            .checked_add(selected_geometry_count)
            .and_then(|count| count.checked_add(usize::from(sprite_quad.is_some())))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("prepared root count overflowed".into())
            })?;
        let cpu_root_texture_capacity = texture_uploads.len();
        let cpu_root_leaf_capacity = cpu_root_mesh_capacity
            .checked_add(cpu_root_texture_capacity)
            .and_then(|count| {
                count.checked_add(usize::from(managed_resource_set_token.is_some()) * 2)
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("prepared root count overflowed".into())
            })?;
        Ok(Self {
            expected_session,
            project_generation,
            source_digest: project.digest().to_owned(),
            view,
            camera_2d,
            model_bindings,
            sprite_bindings,
            sprite_order,
            managed_slot_assignments,
            sprite_geometry,
            directional_light,
            ambient_color,
            clear_color,
            bindings,
            geometry_bytes,
            prepared_metadata_bytes: actual_metadata_bytes,
            legacy_mesh_copy_bytes: actual_legacy_mesh_copy_bytes,
            expanded_instance_count,
            limits,
            source_pin: source_pin.cloned(),
            stage_requests,
            managed_resource_set_token,
            cpu_root_parent_capacity,
            cpu_root_mesh_capacity,
            cpu_root_texture_capacity,
            cpu_root_leaf_capacity,
        })
    }

    pub fn prepared_geometry_bytes(&self) -> usize {
        self.geometry_bytes
    }

    pub fn prepared_metadata_bytes(&self) -> usize {
        self.prepared_metadata_bytes
    }

    fn legacy_mesh_copy_bytes(&self) -> usize {
        self.legacy_mesh_copy_bytes
    }

    fn stage_requests(&self) -> Result<Arc<StageRequestBacking>, PacketRenderError> {
        self.stage_requests.as_ref().map(Arc::clone).ok_or_else(|| {
            PacketRenderError::InvalidPacket(
                "CPU generation has no managed stage request backing".into(),
            )
        })
    }

    fn cpu_roots(&self, cancelled: &AtomicBool) -> Result<GenerationRoots, PacketRenderError> {
        let stage = self
            .stage_requests
            .as_ref()
            .expect("managed CPU resources always have a stage projection");
        let mut parents = Vec::with_capacity(self.cpu_root_parent_capacity);
        parents.extend([
            Some(GenerationBacking::Stage(Arc::clone(stage))),
            Some(GenerationBacking::Geometries(Arc::clone(&stage.geometries))),
            Some(GenerationBacking::ModelBindings(Arc::clone(
                &self.model_bindings,
            ))),
            Some(GenerationBacking::SpriteBindings(Arc::clone(
                &self.sprite_bindings,
            ))),
            Some(GenerationBacking::SpriteOrder(Arc::clone(
                &self.sprite_order,
            ))),
        ]);
        let mut parent_ids = HashSet::with_capacity(self.cpu_root_parent_capacity);
        parent_ids.insert(Arc::as_ptr(stage) as usize);
        parent_ids.insert(Arc::as_ptr(&stage.geometries) as usize);
        parent_ids.insert(Arc::as_ptr(&self.model_bindings) as usize);
        parent_ids.insert(Arc::as_ptr(&self.sprite_bindings) as usize);
        parent_ids.insert(Arc::as_ptr(&self.sprite_order) as *const () as usize);
        let mut leaves = Vec::with_capacity(self.cpu_root_leaf_capacity);
        let mut mesh_ids = HashSet::with_capacity(self.cpu_root_mesh_capacity);
        let mut texture_ids = HashSet::with_capacity(self.cpu_root_texture_capacity);
        for mesh in stage.geometries.values() {
            packet_cpu::check_cancelled(cancelled)?;
            push_mesh_root(mesh, &mut mesh_ids, &mut leaves);
        }
        for binding in self.model_bindings.values() {
            packet_cpu::check_cancelled(cancelled)?;
            if let Some(texture) = &binding.texture {
                push_texture_root(texture, &mut texture_ids, &mut leaves);
            }
        }
        for binding in self.sprite_bindings.values() {
            packet_cpu::check_cancelled(cancelled)?;
            push_texture_root(&binding.texture, &mut texture_ids, &mut leaves);
        }
        for (_, binding) in &self.bindings {
            packet_cpu::check_cancelled(cancelled)?;
            if let Some(mesh) = &binding.mesh {
                push_mesh_root(mesh, &mut mesh_ids, &mut leaves);
            }
            if let Some(draws) = &binding.model_draws {
                let id = Arc::as_ptr(draws) as *const () as usize;
                if parent_ids.insert(id) {
                    parents.push(Some(GenerationBacking::ModelTemplate(Arc::clone(draws))));
                }
                for draw in draws.iter() {
                    push_mesh_root(&draw.mesh, &mut mesh_ids, &mut leaves);
                    if let Some(texture) = &draw.texture {
                        push_texture_root(texture, &mut texture_ids, &mut leaves);
                    }
                }
            }
            if let Some((mesh, _, sprite)) = &binding.sprite_draw {
                push_mesh_root(mesh, &mut mesh_ids, &mut leaves);
                push_texture_root(&sprite.texture, &mut texture_ids, &mut leaves);
            }
        }
        if let Some(token) = &self.managed_resource_set_token {
            packet_cpu::check_cancelled(cancelled)?;
            leaves.push(Some(GenerationBacking::ResourceSetToken(Arc::clone(token))));
            let assignments = self.managed_slot_assignments.as_ref().ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "managed CPU resources are missing slot assignments".into(),
                )
            })?;
            leaves.push(Some(GenerationBacking::SlotAssignments(Arc::clone(
                assignments,
            ))));
        }
        Ok(GenerationRoots::new(parents, leaves))
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn extract(
        &self,
        snapshot: &RenderSnapshot,
        camera_override: Option<&Camera>,
    ) -> Result<RenderPacket, PacketRenderError> {
        let view_override = camera_override.map(|camera| self.view.with_camera(camera.clone()));
        self.extract_with_view(snapshot, view_override.as_ref())
    }

    pub fn extract_with_view(
        &self,
        snapshot: &RenderSnapshot,
        view_override: Option<&RenderView>,
    ) -> Result<RenderPacket, PacketRenderError> {
        self.extract_with_identity(
            snapshot,
            snapshot.sequence,
            None,
            view_override,
            self.source_pin.as_ref(),
            None,
        )
    }

    fn extract_managed(
        &self,
        snapshot: &RenderSnapshot,
        publication_sequence: u64,
        view_override: Option<&RenderView>,
        source_pin: &SourcePin,
        cancelled: &AtomicBool,
    ) -> Result<RenderPacket, PacketRenderError> {
        self.extract_with_identity(
            snapshot,
            publication_sequence,
            Some(snapshot.sequence),
            view_override,
            Some(source_pin),
            Some(cancelled),
        )
    }

    fn extract_with_identity(
        &self,
        snapshot: &RenderSnapshot,
        publication_sequence: u64,
        authority_snapshot_sequence: Option<u64>,
        view_override: Option<&RenderView>,
        source_pin: Option<&SourcePin>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<RenderPacket, PacketRenderError> {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if snapshot.entities.len() > self.limits.max_instances {
            return Err(PacketRenderError::InvalidPacket(
                "snapshot entity count exceeds packet limit".into(),
            ));
        }
        let mut snapshot_by_handle = HashMap::with_capacity(snapshot.entities.len());
        for entry in snapshot.entities.iter() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
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
        let mut instances = Vec::with_capacity(self.expanded_instance_count);
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        for (handle, binding) in &self.bindings {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            let transform_data = snapshot_by_handle.get(handle).ok_or_else(|| {
                PacketRenderError::InvalidPacket(format!(
                    "snapshot is missing authored entity {}",
                    binding.stable_id
                ))
            })?;
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
            if let Some(mesh) = &binding.mesh {
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
            if let Some(draws) = &binding.model_draws {
                for draw in draws.iter() {
                    if let Some(cancelled) = cancelled {
                        packet_cpu::check_cancelled(cancelled)?;
                    }
                    let draw_transform = transform * draw.node_global;
                    if !is_affine_matrix(draw_transform) {
                        return Err(PacketRenderError::InvalidPacket(format!(
                            "model transform for {} is non-finite or non-affine",
                            binding.stable_id
                        )));
                    }
                    expanded_vertices = expanded_vertices
                        .checked_add(draw.mesh.vertex_count())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "expanded vertex count overflowed".into(),
                            )
                        })?;
                    expanded_indices = expanded_indices
                        .checked_add(draw.mesh.index_count())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "expanded index count overflowed".into(),
                            )
                        })?;
                    let id = RenderInstanceId {
                        entity: *handle,
                        primitive: draw.ordinal,
                    };
                    instances.push(RenderInstance {
                        id,
                        mesh: draw.mesh.clone(),
                        transform: draw_transform,
                        material: draw.material.clone(),
                    });
                }
            }
            if let Some((mesh, material, sprite)) = &binding.sprite_draw {
                if binding.mesh.is_some() || binding.model_draws.is_some() {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "entity {} has conflicting sprite and mesh renderer bindings",
                        binding.stable_id
                    )));
                }
                let draw_transform = transform * sprite.local;
                if !is_affine_matrix(draw_transform) {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "sprite transform for {} is non-finite or non-affine",
                        binding.stable_id
                    )));
                }
                expanded_vertices = expanded_vertices.checked_add(4).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded vertex count overflowed".into())
                })?;
                expanded_indices = expanded_indices.checked_add(6).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("expanded index count overflowed".into())
                })?;
                let id = RenderInstanceId {
                    entity: *handle,
                    primitive: 0,
                };
                instances.push(RenderInstance {
                    id,
                    mesh: mesh.clone(),
                    transform: draw_transform,
                    material: material.clone(),
                });
            }
        }
        if expanded_vertices > self.limits.max_expanded_vertices
            || expanded_indices > self.limits.max_expanded_indices
        {
            return Err(PacketRenderError::InvalidPacket(
                "expanded snapshot geometry limit exceeded".into(),
            ));
        }
        let view = if let Some(view) = view_override {
            view.clone()
        } else if let Some(camera_state) = &self.camera_2d {
            let transform = snapshot_by_handle
                .get(&camera_state.handle)
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket(format!(
                        "snapshot is missing authoritative 2D camera entity {}",
                        camera_state.entity
                    ))
                })?;
            let camera = camera_from_transform(
                transform.translation,
                transform.rotation,
                transform.scale,
                camera_state.near,
                camera_state.far,
            )?;
            RenderView::new(
                camera,
                RenderProjection::Orthographic {
                    vertical_size: camera_state.vertical_size,
                },
            )
        } else {
            self.view.clone()
        };
        view.validate()?;
        let mut packet = RenderPacket::new(
            self.expected_session,
            snapshot.tick,
            publication_sequence,
            self.project_generation,
            self.source_digest.clone(),
            view.camera().clone(),
            self.directional_light.clone(),
            self.ambient_color,
            self.clear_color,
            instances,
        );
        packet.authority_snapshot_sequence = authority_snapshot_sequence;
        packet.source_pin = source_pin.cloned();
        Ok(packet
            .with_shared_model_bindings(Arc::clone(&self.model_bindings))
            .with_shared_sprite_bindings(
                Arc::clone(&self.sprite_bindings),
                Arc::clone(&self.sprite_order),
                self.sprite_geometry,
            )
            .with_view(view))
    }
}

fn camera_from_transform(
    translation: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    near: f32,
    far: f32,
) -> Result<Camera, PacketRenderError> {
    if !translation.iter().all(|value| value.is_finite())
        || !scale
            .iter()
            .all(|value| value.is_finite() && (*value - 1.0).abs() <= 1.0e-6)
    {
        return Err(PacketRenderError::InvalidPacket(
            "2D camera transform must be finite and have unit scale".into(),
        ));
    }
    let rotation = Quat::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3]);
    if !rotation.is_finite() || rotation.length_squared() < 1.0e-8 {
        return Err(PacketRenderError::InvalidPacket(
            "2D camera transform has a zero or non-finite rotation".into(),
        ));
    }
    let rotation = rotation.normalize();
    let position = Vec3::from_array(translation);
    let forward = rotation * Vec3::NEG_Z;
    let up = rotation * Vec3::Y;
    let camera = Camera {
        position: position.to_array(),
        target: (position + forward).to_array(),
        up: up.to_array(),
        vertical_fov_radians: Camera::default().vertical_fov_radians,
        near,
        far,
    };
    camera
        .validate()
        .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
    Ok(camera)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ModelExpansionStats {
    draws: usize,
    vertices: usize,
    indices: usize,
    geometry_keys: Vec<LeafCookKey>,
    image_keys: Vec<LeafCookKey>,
}

fn checked_add_limit(
    current: usize,
    amount: usize,
    limit: usize,
    label: &str,
) -> Result<usize, PacketRenderError> {
    let value = current
        .checked_add(amount)
        .ok_or_else(|| PacketRenderError::InvalidPacket(format!("{label} overflowed")))?;
    if value > limit {
        return Err(PacketRenderError::InvalidPacket(format!(
            "{label} exceeds its configured packet limit"
        )));
    }
    Ok(value)
}

// Each capacity is a distinct retained allocation used by the shared preflight
// and reconciliation estimator; keep the accounting dimensions explicit.
#[allow(clippy::too_many_arguments)]
fn prepared_render_metadata_bytes(
    project: &PreparedProject,
    selected_models: &HashMap<&StableId, (&Arc<StaticModel>, ModelExpansionStats)>,
    model_table_capacity: usize,
    sprite_table_capacity: usize,
    geometry_table_capacity: usize,
    slot_assignment_capacity: usize,
    managed: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<usize, PacketRenderError> {
    let mut bytes = std::mem::size_of::<ProjectRenderResources>();
    let mut add = |amount: usize| -> Result<(), PacketRenderError> {
        bytes = bytes.checked_add(amount).ok_or_else(|| {
            PacketRenderError::InvalidPacket("prepared CPU metadata estimate overflowed".into())
        })?;
        Ok(())
    };
    let aligned_string = |value: &str| -> Result<usize, PacketRenderError> {
        if value.is_empty() {
            return Ok(0);
        }
        value
            .len()
            .checked_add(15)
            .map(|length| length & !15)
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "prepared CPU metadata string size overflowed".into(),
                )
            })
    };
    let aligned_string_length = |length: usize| -> Result<usize, PacketRenderError> {
        if length == 0 {
            return Ok(0);
        }
        length
            .checked_add(15)
            .map(|value| value & !15)
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "prepared CPU metadata string size overflowed".into(),
                )
            })
    };
    let scaled = |count: usize, size: usize| -> Result<usize, PacketRenderError> {
        count.checked_mul(size).ok_or_else(|| {
            PacketRenderError::InvalidPacket("prepared CPU metadata estimate overflowed".into())
        })
    };

    add(scaled(
        project.entities().len(),
        std::mem::size_of::<(EntityHandle, ProjectBinding)>(),
    )?)?;
    add(aligned_string(project.digest())?)?;
    for entity in project.entities() {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        add(aligned_string(entity.id.as_str())?)?;
    }

    for mesh_id in project.meshes().keys() {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        add(aligned_string(mesh_id.as_str())?)?;
    }

    let mut imported_geometry_ids = HashMap::<LeafCookKey, usize>::new();
    let mut image_keys = HashSet::<LeafCookKey>::new();
    let mut template_draws = 0usize;
    for (model_id, (_, stats)) in selected_models {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        add(aligned_string(model_id.as_str())?)?;
        template_draws = template_draws.checked_add(stats.draws).ok_or_else(|| {
            PacketRenderError::InvalidPacket("prepared model template count overflowed".into())
        })?;
        for key in &stats.geometry_keys {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            imported_geometry_ids
                .entry(*key)
                .and_modify(|length| *length = (*length).max(model_id.as_str().len()))
                .or_insert(model_id.as_str().len());
        }
        for key in &stats.image_keys {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            image_keys.insert(*key);
        }
    }
    let unique_models = selected_models.len();
    if unique_models > 0 {
        add(scaled(
            template_draws,
            std::mem::size_of::<PreparedModelDraw>(),
        )?)?;
        add(scaled(unique_models, 2 * std::mem::size_of::<usize>())?)?;
    }

    let mut sprite_count = 0usize;
    if !project.sprite_renderers().is_empty() {
        let catalog = project.asset_catalog().ok_or_else(|| {
            PacketRenderError::InvalidPacket(
                "sprite renderer bindings require a prepared asset catalog".into(),
            )
        })?;
        for (entity_id, binding) in project.sprite_renderers() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            let Some(ImportedAsset::Atlas(atlas)) = catalog.asset(binding.atlas_asset_id()) else {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "sprite renderer references missing or non-atlas asset {}",
                    binding.atlas_asset_id()
                )));
            };
            if find_sprite_region(atlas, binding.region_id(), cancelled)?.is_none() {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "sprite renderer {} references missing atlas region {}",
                    binding.component_id(),
                    binding.region_id()
                )));
            }
            let Some(ImportedAsset::Image(image)) = catalog.asset(atlas.image_asset()) else {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "sprite atlas {} references a missing or non-image asset {}",
                    binding.atlas_asset_id(),
                    atlas.image_asset()
                )));
            };
            image_keys.insert(image.key());
            add(aligned_string(entity_id.as_str())?)?;
            add(aligned_string(binding.component_id().as_str())?)?;
            sprite_count = sprite_count.checked_add(1).ok_or_else(|| {
                PacketRenderError::InvalidPacket("prepared sprite count overflowed".into())
            })?;
        }
    }
    // The prepared camera state owns a copy of its entity StableId.
    if let Some(camera) = project.camera_2d() {
        add(aligned_string(camera.entity_id().as_str())?)?;
    }

    let arc_header_bytes = 2 * std::mem::size_of::<usize>();
    add(
        std::mem::size_of::<HashMap<RenderInstanceId, ModelDrawBinding>>()
            .checked_add(arc_header_bytes)
            .and_then(|header| {
                hash_map_storage_bytes::<RenderInstanceId, ModelDrawBinding>(model_table_capacity)
                    .and_then(|buckets| header.checked_add(buckets))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "prepared model sidecar estimate overflowed".into(),
                )
            })?,
    )?;
    add(
        std::mem::size_of::<HashMap<RenderInstanceId, SpriteDrawBinding>>()
            .checked_add(arc_header_bytes)
            .and_then(|header| {
                hash_map_storage_bytes::<RenderInstanceId, SpriteDrawBinding>(sprite_table_capacity)
                    .and_then(|buckets| header.checked_add(buckets))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "prepared sprite sidecar estimate overflowed".into(),
                )
            })?,
    )?;
    add(scaled(
        sprite_count,
        std::mem::size_of::<RenderInstanceId>(),
    )?)?;
    add(arc_header_bytes)?;
    if sprite_count > 0 {
        add(aligned_string("daedalus.internal.sprite-quad")?)?;
    }

    let texture_wrapper_bytes = std::mem::size_of::<ModelTextureUpload>() + arc_header_bytes;
    add(scaled(image_keys.len(), texture_wrapper_bytes)?)?;
    let geometry_wrapper_count = project
        .meshes()
        .len()
        .checked_add(imported_geometry_ids.len())
        .and_then(|count| count.checked_add(usize::from(sprite_count > 0)))
        .ok_or_else(|| {
            PacketRenderError::InvalidPacket("prepared geometry wrapper count overflowed".into())
        })?;
    add(scaled(
        geometry_wrapper_count,
        std::mem::size_of::<MeshResource>() + arc_header_bytes,
    )?)?;
    for id_length in imported_geometry_ids.values() {
        add(aligned_string_length(*id_length)?)?;
    }
    if managed {
        let parent_roots = 5usize.checked_add(unique_models).ok_or_else(|| {
            PacketRenderError::InvalidPacket("prepared root count overflowed".into())
        })?;
        let leaf_roots = geometry_wrapper_count
            .checked_add(image_keys.len())
            .and_then(|count| count.checked_add(2))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("prepared root count overflowed".into())
            })?;
        add(std::mem::size_of::<packet_cpu::GenerationRecord>())?;
        add(std::mem::size_of::<GenerationRoots>())?;
        add(scaled(
            parent_roots,
            std::mem::size_of::<Option<GenerationBacking>>(),
        )?)?;
        add(scaled(
            leaf_roots,
            std::mem::size_of::<Option<GenerationBacking>>(),
        )?)?;
        add(std::mem::size_of::<StageRequestBacking>()
            .checked_add(arc_header_bytes)
            .and_then(|value| value.checked_add(aligned_string(project.digest()).ok()?))
            .and_then(|value| {
                value.checked_add(std::mem::size_of::<HashMap<GeometryKey, MeshHandle>>())
            })
            .and_then(|value| value.checked_add(arc_header_bytes))
            .and_then(|value| {
                hash_map_storage_bytes::<GeometryKey, MeshHandle>(geometry_table_capacity)
                    .and_then(|map_bytes| value.checked_add(map_bytes))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "stage request metadata estimate overflowed".into(),
                )
            })?)?;
        add(std::mem::size_of::<ManagedResourceSetToken>()
            .checked_add(arc_header_bytes)
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("resource-set token estimate overflowed".into())
            })?)?;
        add(std::mem::size_of::<HashMap<RenderInstanceId, usize>>()
            .checked_add(arc_header_bytes)
            .and_then(|header| {
                hash_map_storage_bytes::<RenderInstanceId, usize>(slot_assignment_capacity)
                    .and_then(|buckets| header.checked_add(buckets))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "managed slot assignment metadata estimate overflowed".into(),
                )
            })?)?;
    }
    Ok(bytes)
}

pub(super) fn hash_map_storage_bytes<K, V>(entries: usize) -> Option<usize> {
    hash_table_storage_bytes::<(K, V)>(entries)
}

pub(super) fn hash_set_storage_bytes<K>(entries: usize) -> Option<usize> {
    hash_table_storage_bytes::<K>(entries)
}

fn hash_table_storage_bytes<T>(entries: usize) -> Option<usize> {
    if entries == 0 {
        return Some(0);
    }
    // Hash tables use power-of-two buckets and reserve at most a 7/8 load
    // factor. `T` is the complete `(K, V)` tuple for maps or `K` for sets, so
    // per-bucket field padding is included before control/alignment slack.
    let desired_buckets = entries.checked_mul(8)?.checked_add(6)?.checked_div(7)?;
    let bucket_count = desired_buckets.max(4).checked_next_power_of_two()?;
    bucket_count
        .checked_mul(std::mem::size_of::<T>().checked_add(1)?)?
        .checked_add(16 + 15)
}

fn model_expansion_stats(
    model: &StaticModel,
    limits: PacketRendererLimits,
    cancelled: Option<&AtomicBool>,
) -> Result<ModelExpansionStats, PacketRenderError> {
    let mut children = vec![Vec::<u32>::new(); model.nodes().len()];
    for (child_index, node) in model.nodes().iter().enumerate() {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if let Some(parent) = node.parent() {
            let siblings = children.get_mut(parent as usize).ok_or_else(|| {
                PacketRenderError::InvalidPacket("model node parent is out of range".into())
            })?;
            siblings.push(u32::try_from(child_index).map_err(|_| {
                PacketRenderError::InvalidPacket("model node index exceeds u32".into())
            })?);
        }
    }
    let mut stats = ModelExpansionStats::default();
    let mut geometry_keys = HashSet::new();
    let mut image_keys = HashSet::new();
    let mut visited = HashSet::new();
    let mut stack = Vec::<(u32, Option<Mat4>)>::new();
    for root in model.selected_scene_roots() {
        stack.push((*root, None));
        while let Some((index, parent_global)) = stack.pop() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            if !visited.insert(index) {
                return Err(PacketRenderError::InvalidPacket(
                    "selected model scene repeats or cycles a node".into(),
                ));
            }
            let node = model.nodes().get(index as usize).ok_or_else(|| {
                PacketRenderError::InvalidPacket("selected model scene node is out of range".into())
            })?;
            let local = Mat4::from_cols_array(node.local_transform());
            if !is_affine_matrix(local) {
                return Err(PacketRenderError::InvalidPacket(
                    "selected model node transform is non-finite or non-affine".into(),
                ));
            }
            let global = parent_global.map_or(local, |parent| parent * local);
            if !is_affine_matrix(global) {
                return Err(PacketRenderError::InvalidPacket(
                    "selected model node global transform is invalid".into(),
                ));
            }
            for primitive_index in node.primitive_indices() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                let primitive = model
                    .primitives()
                    .get(*primitive_index as usize)
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(
                            "selected model node primitive is out of range".into(),
                        )
                    })?;
                let geometry_key = primitive.geometry().key();
                if geometry_keys.insert(geometry_key) {
                    stats.geometry_keys.push(geometry_key);
                }
                stats.draws = checked_add_limit(
                    stats.draws,
                    1,
                    limits.max_instances,
                    "selected model draw count",
                )?;
                stats.vertices = checked_add_limit(
                    stats.vertices,
                    primitive.geometry().vertex_count(),
                    limits.max_expanded_vertices,
                    "selected model expanded vertex count",
                )?;
                stats.indices = checked_add_limit(
                    stats.indices,
                    primitive.geometry().index_count(),
                    limits.max_expanded_indices,
                    "selected model expanded index count",
                )?;
                if let Some(texture_ref) = primitive
                    .material_index()
                    .and_then(|material| model.materials().get(material as usize))
                    .and_then(|material| material.base_color_texture.as_ref())
                {
                    let owned = model
                        .owned_images()
                        .iter()
                        .find(|image| image.identity() == texture_ref.owner_image())
                        .ok_or_else(|| {
                            PacketRenderError::InvalidPacket(
                                "selected model material references an absent owned image".into(),
                            )
                        })?;
                    let image_key = owned.asset().key();
                    if image_keys.insert(image_key) {
                        stats.image_keys.push(image_key);
                    }
                }
            }
            for child in children[index as usize].iter().rev() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                stack.push((*child, Some(global)));
            }
        }
    }
    Ok(stats)
}

// Model interning maps, expected preflight count, source pin, and cancellation
// are independent inputs to this one model-expansion pass.
#[allow(clippy::too_many_arguments)]
fn expand_model_draws(
    model_id: &StableId,
    model: &StaticModel,
    project_generation: u64,
    geometry_handles: &mut HashMap<LeafCookKey, MeshHandle>,
    texture_uploads: &mut HashMap<LeafCookKey, Arc<ModelTextureUpload>>,
    expected_draw_count: usize,
    source_pin: Option<&SourcePin>,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<PreparedModelDraw>, PacketRenderError> {
    let mut children = vec![Vec::<u32>::new(); model.nodes().len()];
    for (child_index, node) in model.nodes().iter().enumerate() {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if let Some(parent) = node.parent() {
            let children = children.get_mut(parent as usize).ok_or_else(|| {
                PacketRenderError::InvalidPacket(format!(
                    "model {model_id} has an out-of-range parent node"
                ))
            })?;
            children.push(u32::try_from(child_index).map_err(|_| {
                PacketRenderError::InvalidPacket("model node index exceeds u32".into())
            })?);
        }
    }

    let mut occurrences = Vec::<(u32, Mat4)>::new();
    let mut visited = HashSet::new();
    let mut stack = Vec::<(u32, Option<Mat4>)>::new();
    for root in model.selected_scene_roots() {
        stack.push((*root, None));
        while let Some((index, parent_global)) = stack.pop() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            if !visited.insert(index) {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "model {model_id} selected scene repeats or cycles a node"
                )));
            }
            let node = model.nodes().get(index as usize).ok_or_else(|| {
                PacketRenderError::InvalidPacket(format!(
                    "model {model_id} selected scene references an out-of-range node"
                ))
            })?;
            let local = Mat4::from_cols_array(node.local_transform());
            if !is_affine_matrix(local) {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "model {model_id} node {index} has a non-finite or non-affine transform"
                )));
            }
            let global = parent_global.map_or(local, |parent| parent * local);
            if !is_affine_matrix(global) {
                return Err(PacketRenderError::InvalidPacket(format!(
                    "model {model_id} node {index} global transform is invalid"
                )));
            }
            occurrences.push((index, global));
            let child_nodes = &children[index as usize];
            for child in child_nodes.iter().rev() {
                if let Some(cancelled) = cancelled {
                    packet_cpu::check_cancelled(cancelled)?;
                }
                stack.push((*child, Some(global)));
            }
        }
    }

    let mut draws = Vec::with_capacity(expected_draw_count);
    for (node_index, node_global) in occurrences {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        let node = &model.nodes()[node_index as usize];
        for primitive_index in node.primitive_indices() {
            if let Some(cancelled) = cancelled {
                packet_cpu::check_cancelled(cancelled)?;
            }
            let primitive = model
                .primitives()
                .get(*primitive_index as usize)
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket(format!(
                        "model {model_id} node {node_index} references a missing primitive"
                    ))
                })?;
            let geometry = Arc::clone(primitive.geometry());
            let mesh = if let Some(mesh) = geometry_handles.get(&geometry.key()).cloned() {
                mesh
            } else {
                let mut mesh =
                    MeshHandle::imported(model_id.clone(), project_generation, geometry)?;
                mesh.pin_source(source_pin);
                geometry_handles.insert(primitive.geometry().key(), mesh.clone());
                mesh
            };
            let material = primitive
                .material_index()
                .and_then(|index| model.materials().get(index as usize));
            let render_material = material.map_or(
                Material {
                    base_color: [1.0; 3],
                    metallic: 1.0,
                    roughness: 1.0,
                    emissive: [0.0; 3],
                },
                |material| Material {
                    base_color: [
                        material.base_color_factor[0],
                        material.base_color_factor[1],
                        material.base_color_factor[2],
                    ],
                    metallic: material.metallic_factor,
                    roughness: material.roughness_factor,
                    emissive: [0.0; 3],
                },
            );
            render_material
                .validate()
                .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
            let mut texture = None;
            let mut sampler = ImageSampler::default();
            if let Some(texture_ref) =
                material.and_then(|material| material.base_color_texture.as_ref())
            {
                sampler = texture_ref.sampler();
                let owned = model
                    .owned_images()
                    .iter()
                    .find(|image| image.identity() == texture_ref.owner_image())
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(format!(
                            "model {model_id} material references an absent owned image"
                        ))
                    })?;
                let image_key = owned.asset().key();
                let upload = if let Some(upload) = texture_uploads.get(&image_key) {
                    Arc::clone(upload)
                } else {
                    let upload = texture_upload_with_pin(Arc::clone(owned.asset()), source_pin)?;
                    texture_uploads.insert(image_key, Arc::clone(&upload));
                    upload
                };
                if geometry_has_no_uv(&mesh) {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "model {model_id} textured primitive has no UV0"
                    )));
                }
                texture = Some(upload);
            }
            let ordinal = u32::try_from(draws.len()).map_err(|_| {
                PacketRenderError::InvalidPacket("model draw ordinal exceeds u32".into())
            })?;
            draws.push(PreparedModelDraw {
                ordinal,
                mesh,
                node_global,
                material: render_material,
                texture,
                sampler,
            });
        }
    }
    Ok(draws)
}

fn geometry_has_no_uv(mesh: &MeshHandle) -> bool {
    mesh.imported_geometry()
        .is_none_or(|geometry| geometry.uv0().is_none())
}

fn find_sprite_region<'a>(
    atlas: &'a daedalus_content::assets::SpriteAtlas,
    region_id: &StableId,
    cancelled: Option<&AtomicBool>,
) -> Result<Option<&'a daedalus_content::assets::AtlasRegion>, PacketRenderError> {
    for region in atlas.regions() {
        if let Some(cancelled) = cancelled {
            packet_cpu::check_cancelled(cancelled)?;
        }
        if region.id() == region_id {
            return Ok(Some(region));
        }
    }
    Ok(None)
}

fn is_affine_matrix(matrix: Mat4) -> bool {
    if !matrix.is_finite() {
        return false;
    }
    let values = matrix.to_cols_array();
    values[3].abs() <= 1.0e-6
        && values[7].abs() <= 1.0e-6
        && values[11].abs() <= 1.0e-6
        && (values[15] - 1.0).abs() <= 1.0e-6
}

#[cfg(test)]
#[path = "model_packet_tests.rs"]
mod model_packet_tests;

#[cfg(test)]
#[path = "sprite_packet_tests.rs"]
mod sprite_packet_tests;

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, ThreadId},
    };

    use daedalus_content::scene::StableId;
    use daedalus_content::{
        ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache,
        prepare_project_with_assets,
        project::{PreparedProject, ProjectLimits, prepare_project_from_bytes},
    };
    use daedalus_core::{
        EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
    };
    use glam::{Mat4, Vec3};
    use serde_json::json;

    use crate::{Mesh, PacketAssetLimits, Vertex};

    use super::{
        GeometryKey, MeshHandle, MeshResourceLimits, PacketCpuLimits, PacketCpuOwner,
        PacketRendererLimits, PreparedModelDraw, ProjectRenderResources, RenderInstance,
        RenderInstanceId, RenderPacket, SourcePin, hash_map_storage_bytes,
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

    fn prepared_two_instances_with_unused_legacy_mesh() -> PreparedProject {
        let scene = br#"{"schema_version":1,"revision":1,"entities":[
            {"id":"entity:a","components":[{"id":"component:mesh:a","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle"}}]},
            {"id":"entity:b","components":[{"id":"component:mesh:b","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle"}}]}
        ],"assets":[
            {"id":"asset:triangle","kind":"daedalus.mesh","uri":"triangle.json","metadata":{}},
            {"id":"asset:unused","kind":"daedalus.mesh","uri":"unused.json","metadata":{}}
        ]}"#;
        let manifest =
            br#"{"schema_version":1,"name":"packet test with unused mesh","scene":"scene.json"}"#;
        let mesh = br#"{"schema_version":1,"vertices":[{"position":[0.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[1.0,0.0,0.0],"normal":[0.0,0.0,1.0]},{"position":[0.0,1.0,0.0],"normal":[0.0,0.0,1.0]}],"indices":[0,1,2]}"#;
        prepare_project_from_bytes(
            manifest,
            scene,
            &BTreeMap::from([
                ("triangle.json".to_owned(), mesh.to_vec()),
                ("unused.json".to_owned(), mesh.to_vec()),
            ]),
            ProjectLimits::default(),
        )
        .unwrap()
    }

    fn prepared_empty_project() -> PreparedProject {
        prepare_project_from_bytes(
            br#"{"schema_version":1,"name":"empty packet test","scene":"scene.json"}"#,
            br#"{"schema_version":1,"revision":1,"entities":[],"assets":[]}"#,
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .unwrap()
    }

    fn prepared_sprite_instances() -> PreparedProject {
        let mut entities = ["entity:sprite-a", "entity:sprite-b"]
            .into_iter()
            .enumerate()
            .map(|(index, entity_id)| {
                json!({
                    "id": entity_id,
                    "components": [
                        {
                            "id": format!("component:transform:{index}"),
                            "type_id": "daedalus.transform",
                            "asset_refs": [],
                            "data": {
                                "translation": [0.0, 0.0, 0.0],
                                "rotation": [0.0, 0.0, 0.0, 1.0],
                                "scale": [1.0, 1.0, 1.0]
                            }
                        },
                        {
                            "id": format!("component:sprite:{index}"),
                            "type_id": "daedalus.sprite_renderer",
                            "asset_refs": ["asset:atlas"],
                            "data": {
                                "atlas": "asset:atlas",
                                "region": "region:unit",
                                "tint": [1.0, 1.0, 1.0, 1.0],
                                "sorting_layer": 0,
                                "order_in_layer": index
                            }
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();
        entities.push(json!({
            "id": "entity:camera",
            "components": [
                {
                    "id": "component:transform:camera",
                    "type_id": "daedalus.transform",
                    "asset_refs": [],
                    "data": {
                        "translation": [0.0, 0.0, 10.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0]
                    }
                },
                {
                    "id": "component:camera2d",
                    "type_id": "daedalus.camera_2d",
                    "asset_refs": [],
                    "data": {"vertical_size": 8.0, "near": 0.1, "far": 100.0}
                }
            ]
        }));
        let scene = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 1,
            "entities": entities,
            "assets": [
                {
                    "id": "asset:image",
                    "kind": "daedalus.image",
                    "uri": "sprites/sheet.png",
                    "metadata": {
                        "schema_version": 1,
                        "role": "sprite_atlas_srgb",
                        "mip_policy": "none",
                        "sampler": {
                            "min_filter": "nearest",
                            "mag_filter": "nearest",
                            "mipmap_filter": "none",
                            "wrap_u": "clamp_to_edge",
                            "wrap_v": "clamp_to_edge"
                        }
                    }
                },
                {
                    "id": "asset:atlas",
                    "kind": "daedalus.sprite_atlas",
                    "uri": "sprites/atlas.json",
                    "metadata": {"schema_version": 1}
                }
            ]
        }))
        .unwrap();
        let atlas = serde_json::to_vec(&json!({
            "schema_version": 1,
            "image_asset": "asset:image",
            "filter_policy": "nearest_no_mip_v1",
            "regions": [{
                "id": "region:unit",
                "rect": [0, 0, 1, 1],
                "pivot": [0.5, 0.5],
                "pixels_per_unit": 1.0
            }]
        }))
        .unwrap();
        let mut image = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut image, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 255, 255, 255]).unwrap();
        }
        let mut resolver = ByteMapSourceResolver::from_bytes(BTreeMap::from([
            ("sprites/sheet.png".to_owned(), image),
            ("sprites/atlas.json".to_owned(), atlas),
        ]))
        .unwrap();
        let mut cache = MemoryCookCache::default();
        prepare_project_with_assets(
            br#"{"schema_version":1,"name":"managed sprite test","scene":"scene.json"}"#,
            &scene,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap()
    }

    fn prepared_model_instances(model_ids: &[&str]) -> PreparedProject {
        let entities = model_ids
            .iter()
            .enumerate()
            .map(|(index, model_id)| {
                let entity_id = format!("entity:model-{index}");
                json!({
                    "id": entity_id,
                    "components": [
                        {
                            "id": format!("component:transform:model-{index}"),
                            "type_id": "daedalus.transform",
                            "asset_refs": [],
                            "data": {
                                "translation": [0.0, 0.0, 0.0],
                                "rotation": [0.0, 0.0, 0.0, 1.0],
                                "scale": [1.0, 1.0, 1.0]
                            }
                        },
                        {
                            "id": format!("component:model-{index}"),
                            "type_id": "daedalus.model_renderer",
                            "asset_refs": [model_id],
                            "data": {"model": model_id}
                        }
                    ]
                })
            })
            .collect::<Vec<_>>();
        let assets = model_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|model_id| {
                json!({
                    "id": model_id,
                    "kind": "daedalus.model",
                    "uri": "models/shared.gltf",
                    "metadata": {"schema_version": 1}
                })
            })
            .collect::<Vec<_>>();
        let scene = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 1,
            "entities": entities,
            "assets": assets
        }))
        .unwrap();
        let gltf = br#"{
            "asset":{"version":"2.0"},
            "scene":0,
            "scenes":[{"nodes":[0]}],
            "nodes":[{"mesh":0}],
            "meshes":[{"primitives":[{"attributes":{"POSITION":0,"NORMAL":1}}]}],
            "buffers":[{"uri":"shared.bin","byteLength":72}],
            "bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":36}],
            "accessors":[
                {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0.0,0.0,0.0],"max":[1.0,1.0,0.0]},
                {"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}
            ]
        }"#;
        let values = [
            0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0,
            0.0, 1.0,
        ];
        let buffer = values
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut resolver = ByteMapSourceResolver::from_bytes(BTreeMap::from([
            ("models/shared.gltf".to_owned(), gltf.to_vec()),
            ("models/shared.bin".to_owned(), buffer),
        ]))
        .unwrap();
        let mut cache = MemoryCookCache::default();
        prepare_project_with_assets(
            br#"{"schema_version":1,"name":"prepared metadata test","scene":"scene.json"}"#,
            &scene,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
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
    fn asset_limits_allow_smaller_working_set_with_two_times_hard_reserve() {
        let mut limits = PacketAssetLimits {
            resident_working_bytes: 1024 * 1024,
            resident_hard_bytes: 2 * 1024 * 1024,
            ..PacketAssetLimits::default()
        };
        assert!(limits.validate().is_ok());

        limits.resident_hard_bytes = 2 * 1024 * 1024 - 1;
        assert!(limits.validate().is_err());
        limits = PacketAssetLimits::default();
        limits.max_in_flight_submissions = 4;
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
    fn prepared_render_metadata_is_preflighted_and_shared_templates_are_reserved_once() {
        let shared_project = prepared_model_instances(&["asset:model-a", "asset:model-a"]);
        let session = SessionId::from_raw(91);
        let bindings = project_bindings(&shared_project, session);
        let resources = ProjectRenderResources::prepare(
            &shared_project,
            12,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        let reserved = resources.prepared_metadata_bytes();
        assert!(reserved > 0);
        assert!(reserved <= super::PREPARED_RENDER_METADATA_LIMIT_BYTES);
        let templates = resources
            .bindings
            .iter()
            .filter_map(|(_, binding)| binding.model_draws.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(templates.len(), 2);
        assert!(Arc::ptr_eq(templates[0], templates[1]));

        let rejected = ProjectRenderResources::prepare_with_metadata_limit(
            &shared_project,
            12,
            session,
            &bindings,
            PacketRendererLimits::default(),
            1,
        );
        assert!(matches!(
            rejected,
            Err(crate::PacketRenderError::AssetByteBudgetExceeded {
                resource: "prepared CPU metadata",
                bytes,
                limit: 1,
            }) if bytes == reserved
        ));

        let distinct_project = prepared_model_instances(&["asset:model-a", "asset:model-b"]);
        let distinct_session = SessionId::from_raw(92);
        let distinct_bindings = project_bindings(&distinct_project, distinct_session);
        let distinct = ProjectRenderResources::prepare(
            &distinct_project,
            12,
            distinct_session,
            &distinct_bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        // The second unique model adds one retained model-ID string in addition
        // to its draw template and template-table entry.
        let additional_model_id_bytes = ("asset:model-b".len() + 15) & !15;
        let unique_template_delta = std::mem::size_of::<PreparedModelDraw>()
            + 2 * std::mem::size_of::<usize>()
            + additional_model_id_bytes;
        assert_eq!(
            distinct.prepared_metadata_bytes() - reserved,
            unique_template_delta
        );
    }

    #[test]
    fn prepared_metadata_accounts_for_hashmap_power_of_two_capacity_jump() {
        type GeometrySlot = (GeometryKey, MeshHandle);
        let slot_bytes = std::mem::size_of::<GeometrySlot>() + 1;
        assert!(
            std::mem::size_of::<GeometrySlot>()
                > std::mem::size_of::<GeometryKey>() + std::mem::size_of::<MeshHandle>(),
            "geometry hash slot must include tuple alignment padding"
        );
        let below = hash_map_storage_bytes::<GeometryKey, MeshHandle>(56).unwrap();
        let above = hash_map_storage_bytes::<GeometryKey, MeshHandle>(57).unwrap();
        assert_eq!(below, 64 * slot_bytes + 31);
        assert_eq!(above, 128 * slot_bytes + 31);
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

    struct SourceDropThread(Arc<Mutex<Option<ThreadId>>>);

    impl Drop for SourceDropThread {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(thread::current().id());
        }
    }

    #[test]
    fn managed_cpu_generation_retains_real_frame_aliases_and_reclaims_on_owner_thread() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(701);
        let bindings = project_bindings(&project, session);
        let drop_thread = Arc::new(Mutex::new(None));
        let source = Arc::new(SourceDropThread(Arc::clone(&drop_thread)));
        let source_weak = Arc::downgrade(&source);
        let source_erased: Arc<dyn Send + Sync> = source.clone();
        drop(source);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                41,
                session,
                &bindings,
                SourcePin::new(source_erased),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        assert_eq!(stage.session(), session);
        assert_eq!(stage.project_generation(), 41);
        assert_eq!(stage.source_digest(), project.digest());
        assert_eq!(stage.geometry_count(), 1);
        assert_eq!(stage.draw_count(), 2);
        assert_eq!(stage.model_draw_count(), 0);
        assert_eq!(stage.sprite_draw_count(), 0);
        assert_eq!(stage.texture_count(), 0);
        assert_eq!(stage.expanded_vertex_count(), 6);
        assert_eq!(stage.expanded_index_count(), 6);
        assert_eq!(owner.status().scratch_current_bytes, 0);
        assert!(owner.status().scratch_peak_bytes > 0);
        assert_eq!(owner.status().frame_reserved_bytes, 16 * 1024 * 1024);

        let snapshot = project_snapshot(&bindings, &[Transform::IDENTITY; 2]);
        let frame = owner
            .extract_validated(generation, &snapshot, 900, None, &cancelled)
            .unwrap();
        let frame_alias = Arc::clone(&frame);
        let packet_alias = frame.packet().clone();
        assert_eq!(frame.publication_sequence(), 900);
        assert_eq!(frame.authority_snapshot_sequence(), Some(snapshot.sequence));
        assert_eq!(frame.tick(), snapshot.tick);
        assert_eq!(frame.packet().instances().len(), 2);
        assert_eq!(frame.packet_limits(), PacketRendererLimits::default());
        assert_eq!(frame.asset_limits(), PacketAssetLimits::default());
        assert!(owner.status().frame_bytes > 0);
        assert_eq!(owner.status().frame_reserved_bytes, 0);

        owner.retire_generation(generation).unwrap();
        let pinned = owner.try_reclaim();
        assert_eq!(pinned.pending_frames, 1);
        assert_eq!(pinned.pending_generations, 1);
        assert!(source_weak.upgrade().is_some());
        drop(stage);
        drop(frame_alias);
        drop(frame);
        let packet_pinned = owner.try_reclaim();
        assert_eq!(packet_pinned.pending_frames, 1);
        assert_eq!(packet_pinned.pending_generations, 1);
        assert!(owner.status().frame_bytes > 0);
        drop(packet_alias);
        let reclaimed = owner.try_reclaim();
        assert_eq!(reclaimed.reclaimed_frames, 1);
        assert_eq!(reclaimed.reclaimed_generations, 1);
        assert_eq!(owner.status().frame_bytes, 0);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
        assert!(source_weak.upgrade().is_none());
        assert_eq!(*drop_thread.lock().unwrap(), Some(thread::current().id()));
    }

    #[test]
    fn managed_cpu_projection_preserves_imported_model_aliases_and_validated_draws() {
        let project = prepared_model_instances(&["asset:model-a", "asset:model-a"]);
        let session = SessionId::from_raw(706);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                8,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        assert_eq!(stage.geometry_count(), 1);
        assert_eq!(stage.texture_count(), 0);
        assert_eq!(stage.draw_count(), 2);
        assert_eq!(stage.model_draw_count(), 2);
        assert_eq!(stage.sprite_draw_count(), 0);
        assert_eq!(stage.resource_requests().model_bindings.len(), 2);
        assert_eq!(stage.slot_assignments().len(), 2);
        assert_eq!(
            stage
                .slot_assignments()
                .values()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1])
        );

        let snapshot = project_snapshot(&bindings, &vec![Transform::IDENTITY; bindings.len()]);
        let frame = owner
            .extract_validated(generation, &snapshot, 801, None, &cancelled)
            .unwrap();
        assert_eq!(frame.packet().instances().len(), 2);
        assert!(Arc::ptr_eq(
            stage.slot_assignments(),
            frame.slot_assignments().unwrap()
        ));
        for (slot, instance) in frame.packet().instances().iter().enumerate() {
            assert_eq!(stage.slot_assignments().get(&instance.id), Some(&slot));
        }
        assert!(Arc::ptr_eq(
            &frame.packet().instances()[0].mesh.0,
            &frame.packet().instances()[1].mesh.0
        ));
        assert_eq!(frame.validation().resource_requests().geometries.len(), 1);

        owner.retire_generation(generation).unwrap();
        drop(stage);
        drop(frame);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
        assert_eq!(owner.status().frame_bytes, 0);
    }

    #[test]
    fn managed_slot_assignments_follow_expanded_topology_and_share_across_pose_frames() {
        let one_draw_project = prepared_model_instances(&["asset:model-a"]);
        let two_draw_project = prepared_model_instances(&["asset:model-a", "asset:model-a"]);
        let session = SessionId::from_raw(714);
        let one_draw_bindings = project_bindings(&one_draw_project, session);
        let two_draw_bindings = project_bindings(&two_draw_project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);

        let first_generation = owner
            .prepare_generation(
                &one_draw_project,
                30,
                session,
                &one_draw_bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let first_stage = owner.stage_input(first_generation).unwrap();
        assert_eq!(first_stage.draw_count(), 1);
        assert_eq!(first_stage.slot_assignments().len(), 1);
        assert_eq!(first_stage.slot_assignments().values().next(), Some(&0));
        let first_frame = owner
            .extract_validated(
                first_generation,
                &project_snapshot(&one_draw_bindings, &[Transform::IDENTITY]),
                910,
                None,
                &cancelled,
            )
            .unwrap();
        assert!(Arc::ptr_eq(
            first_stage.slot_assignments(),
            first_frame.slot_assignments().unwrap()
        ));

        let second_generation = owner
            .prepare_generation(
                &two_draw_project,
                31,
                session,
                &two_draw_bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let second_stage = owner.stage_input(second_generation).unwrap();
        assert_eq!(second_stage.draw_count(), 2);
        assert_eq!(second_stage.slot_assignments().len(), 2);
        assert_eq!(
            second_stage
                .slot_assignments()
                .values()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1])
        );
        assert!(!Arc::ptr_eq(
            first_stage.slot_assignments(),
            second_stage.slot_assignments()
        ));
        let second_snapshot = project_snapshot(&two_draw_bindings, &[Transform::IDENTITY; 2]);
        let second_frame = owner
            .extract_validated(second_generation, &second_snapshot, 911, None, &cancelled)
            .unwrap();
        assert!(Arc::ptr_eq(
            second_stage.slot_assignments(),
            second_frame.slot_assignments().unwrap()
        ));
        for (slot, instance) in second_frame.packet().instances().iter().enumerate() {
            assert_eq!(
                second_stage.slot_assignments().get(&instance.id),
                Some(&slot)
            );
        }

        let moved_snapshot = RenderSnapshot {
            sequence: second_snapshot.sequence + 1,
            tick: Tick(second_snapshot.tick.0 + 1),
            entities: two_draw_bindings
                .values()
                .enumerate()
                .map(|(index, entity)| RenderEntitySnapshot {
                    entity: *entity,
                    transform: Transform {
                        translation: [index as f32 + 0.25, 0.0, 0.0],
                        ..Transform::IDENTITY
                    },
                })
                .collect::<Vec<_>>()
                .into(),
        };
        let moved_frame = owner
            .extract_validated(second_generation, &moved_snapshot, 912, None, &cancelled)
            .unwrap();
        assert!(Arc::ptr_eq(
            second_frame.slot_assignments().unwrap(),
            moved_frame.slot_assignments().unwrap()
        ));

        owner.retire_generation(first_generation).unwrap();
        drop(first_stage);
        drop(first_frame);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        owner.retire_generation(second_generation).unwrap();
        drop(second_stage);
        drop(second_frame);
        drop(moved_frame);
        let reclaimed = owner.try_reclaim();
        assert_eq!(reclaimed.reclaimed_generations, 1);
        assert_eq!(reclaimed.reclaimed_frames, 2);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
        assert_eq!(owner.status().frame_bytes, 0);
    }

    #[test]
    fn managed_cpu_projection_shares_real_sprite_texture_and_quad_across_entities() {
        let project = prepared_sprite_instances();
        let session = SessionId::from_raw(707);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                9,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        let requests = stage.resource_requests();
        assert_eq!(stage.geometry_count(), 1);
        assert_eq!(stage.texture_count(), 1);
        assert_eq!(stage.draw_count(), 2);
        assert_eq!(stage.model_draw_count(), 0);
        assert_eq!(stage.sprite_draw_count(), 2);
        assert_eq!(requests.sprite_bindings.len(), 2);
        let mut sprite_bindings = requests.sprite_bindings.values();
        let first = sprite_bindings.next().unwrap();
        let second = sprite_bindings.next().unwrap();
        assert!(Arc::ptr_eq(&first.texture, &second.texture));
        assert!(requests.geometries.values().all(MeshHandle::is_sprite_quad));

        let snapshot = project_snapshot(&bindings, &vec![Transform::IDENTITY; bindings.len()]);
        let frame = owner
            .extract_validated(generation, &snapshot, 802, None, &cancelled)
            .unwrap();
        assert_eq!(frame.packet().instances().len(), 2);
        assert_eq!(frame.validation().resource_requests().geometries.len(), 1);
        assert_eq!(
            frame.validation().resource_requests().sprite_bindings.len(),
            2
        );

        owner.retire_generation(generation).unwrap();
        drop(stage);
        drop(frame);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
        assert_eq!(owner.status().frame_bytes, 0);
    }

    #[test]
    fn managed_stage_omits_unreferenced_legacy_mesh_but_keeps_conservative_copy_charge() {
        let project = prepared_two_instances_with_unused_legacy_mesh();
        let source_digest = project.digest().to_owned();
        assert_eq!(project.meshes().len(), 2);
        let conservative_legacy_copy =
            super::packet_cpu::legacy_copy_bytes(&project, None).unwrap();
        assert!(conservative_legacy_copy > 0);

        let session = SessionId::from_raw(709);
        let bindings = project_bindings(&project, session);
        let copied_resources = ProjectRenderResources::prepare(
            &project,
            18,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        let copied_legacy_bytes = copied_resources.legacy_mesh_copy_bytes();

        let referenced_project = prepared_two_instances();
        let referenced_bindings = project_bindings(&referenced_project, SessionId::from_raw(719));
        let referenced_resources = ProjectRenderResources::prepare(
            &referenced_project,
            18,
            SessionId::from_raw(719),
            &referenced_bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        assert!(copied_legacy_bytes > referenced_resources.legacy_mesh_copy_bytes());
        assert!(conservative_legacy_copy >= copied_legacy_bytes);

        // The preflight charges source capacities for every declared legacy
        // mesh before conversion, including the unused mesh omitted from the
        // GPU stage projection.
        let tight_legacy_limit = conservative_legacy_copy - 1;
        let tight_limits = PacketCpuLimits {
            legacy_mesh_copy_bytes_per_generation: tight_legacy_limit,
            legacy_mesh_copy_bytes_total: tight_legacy_limit * 2,
            ..PacketCpuLimits::default()
        };
        let mut rejecting_owner = PacketCpuOwner::new(
            tight_limits,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            rejecting_owner.prepare_generation(
                &project,
                18,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            ),
            Err(crate::PacketRenderError::CpuBudgetExceeded {
                resource: "legacy mesh copy",
                bytes,
                limit,
            }) if bytes == conservative_legacy_copy && limit == tight_legacy_limit
        ));

        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let generation = owner
            .prepare_generation(
                &project,
                18,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        assert_eq!(stage.source_digest(), source_digest.as_str());
        assert_eq!(project.digest(), source_digest);
        assert_eq!(stage.geometry_count(), 1);
        assert_eq!(stage.draw_count(), 2);
        assert_eq!(stage.expanded_vertex_count(), 6);
        assert_eq!(stage.expanded_index_count(), 6);
        let requests = stage.resource_requests();
        assert_eq!(requests.geometries.len(), 1);
        assert_eq!(requests.model_bindings.len(), 0);
        assert_eq!(requests.sprite_bindings.len(), 0);
        assert_eq!(
            requests.geometries.values().next().unwrap().asset_id(),
            &StableId::new("asset:triangle").unwrap()
        );
        assert_eq!(owner.status().legacy_mesh_copy_bytes, copied_legacy_bytes);
        assert!(owner.status().legacy_mesh_copy_bytes > 0);

        let snapshot = project_snapshot(&bindings, &[Transform::IDENTITY; 2]);
        let frame = owner
            .extract_validated(generation, &snapshot, 901, None, &cancelled)
            .unwrap();
        assert_eq!(frame.packet().instances().len(), 2);
        assert_eq!(frame.validation().resource_requests().geometries.len(), 1);
        assert_eq!(
            frame.packet().instances()[0].mesh.asset_id(),
            &StableId::new("asset:triangle").unwrap()
        );
        assert_eq!(project.digest(), source_digest);

        owner.retire_generation(generation).unwrap();
        drop(stage);
        drop(frame);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
    }

    #[test]
    fn managed_resource_set_tokens_are_generation_scoped_and_frame_reclamation_is_independent() {
        let project = prepared_two_instances();
        let digest = project.digest().to_owned();
        let session = SessionId::from_raw(710);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let mut foreign_owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                19,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let foreign_generation = foreign_owner
            .prepare_generation(
                &project,
                19,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        let foreign_stage = foreign_owner.stage_input(foreign_generation).unwrap();
        assert_eq!(stage.session(), foreign_stage.session());
        assert_eq!(
            stage.project_generation(),
            foreign_stage.project_generation()
        );
        assert_eq!(stage.source_digest(), foreign_stage.source_digest());
        assert!(!Arc::ptr_eq(
            stage.resource_set_token(),
            foreign_stage.resource_set_token()
        ));

        let first_snapshot = project_snapshot(&bindings, &[Transform::IDENTITY; 2]);
        let first = owner
            .extract_validated(generation, &first_snapshot, 902, None, &cancelled)
            .unwrap();
        assert!(Arc::ptr_eq(
            stage.resource_set_token(),
            first.resource_set_token()
        ));
        assert!(Arc::ptr_eq(
            stage.slot_assignments(),
            first.slot_assignments().unwrap()
        ));
        assert_eq!(
            stage
                .slot_assignments()
                .values()
                .copied()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([0, 1])
        );
        assert_eq!(first.source_digest(), digest.as_str());

        let moved_snapshot = RenderSnapshot {
            sequence: first_snapshot.sequence + 1,
            tick: Tick(15),
            entities: bindings
                .values()
                .zip([
                    Transform::IDENTITY,
                    Transform {
                        translation: [0.25, 0.0, 0.0],
                        ..Transform::IDENTITY
                    },
                ])
                .map(|(entity, transform)| RenderEntitySnapshot {
                    entity: *entity,
                    transform,
                })
                .collect::<Vec<_>>()
                .into(),
        };
        let second = owner
            .extract_validated(generation, &moved_snapshot, 903, None, &cancelled)
            .unwrap();
        assert_eq!(second.tick(), moved_snapshot.tick);
        assert!(Arc::ptr_eq(
            stage.resource_set_token(),
            second.resource_set_token()
        ));
        assert!(Arc::ptr_eq(
            first.resource_set_token(),
            second.resource_set_token()
        ));
        assert!(Arc::ptr_eq(
            first.slot_assignments().unwrap(),
            second.slot_assignments().unwrap()
        ));

        drop(first);
        assert_eq!(
            owner.try_reclaim().reclaimed_frames,
            1,
            "an old frame must reclaim while its generation token stays active"
        );
        let mut previous = second;
        for offset in 0..3 {
            let snapshot_sequence = moved_snapshot.sequence + offset + 1;
            let snapshot = RenderSnapshot {
                sequence: snapshot_sequence,
                tick: Tick(16 + offset),
                entities: bindings
                    .values()
                    .zip([
                        Transform::IDENTITY,
                        Transform {
                            translation: [0.5 + offset as f32 * 0.1, 0.0, 0.0],
                            ..Transform::IDENTITY
                        },
                    ])
                    .map(|(entity, transform)| RenderEntitySnapshot {
                        entity: *entity,
                        transform,
                    })
                    .collect::<Vec<_>>()
                    .into(),
            };
            let next = owner
                .extract_validated(generation, &snapshot, 904 + offset, None, &cancelled)
                .unwrap();
            assert!(Arc::ptr_eq(
                stage.resource_set_token(),
                next.resource_set_token()
            ));
            drop(previous);
            assert_eq!(
                owner.try_reclaim().reclaimed_frames,
                1,
                "repeated pose frames must reclaim without retiring their generation"
            );
            previous = next;
        }

        let retained_weak = Arc::downgrade(stage.resource_set_token());
        owner.retire_generation(generation).unwrap();
        drop(stage);
        drop(previous);
        let waiting_on_weak = owner.try_reclaim();
        assert_eq!(waiting_on_weak.reclaimed_frames, 1);
        assert_eq!(waiting_on_weak.pending_generations, 1);
        assert!(owner.status().prepared_metadata_bytes > 0);
        assert!(retained_weak.upgrade().is_some());
        drop(retained_weak);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);

        foreign_owner.retire_generation(foreign_generation).unwrap();
        drop(foreign_stage);
        assert_eq!(foreign_owner.try_reclaim().reclaimed_generations, 1);
    }

    #[test]
    fn managed_cpu_generation_rejects_foreign_ids_and_reuses_lowered_limits_after_reclaim() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(702);
        let bindings = project_bindings(&project, session);
        let limits = PacketCpuLimits {
            prepared_metadata_bytes_per_generation: 1024 * 1024,
            prepared_metadata_bytes_total: 2 * 1024 * 1024,
            legacy_mesh_copy_bytes_per_generation: 1024 * 1024,
            legacy_mesh_copy_bytes_total: 2 * 1024 * 1024,
            frame_bytes_per_frame: 1024 * 1024,
            frame_bytes_total: 2 * 1024 * 1024,
            ..PacketCpuLimits::default()
        };
        assert!(limits.validate().is_ok());
        let mut owner = PacketCpuOwner::new(
            limits,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let foreign_owner = PacketCpuOwner::new(
            limits,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                3,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        assert!(matches!(
            foreign_owner.stage_input(generation),
            Err(super::PacketRenderError::StaleCpuGeneration)
        ));
        let snapshot = project_snapshot(&bindings, &[Transform::IDENTITY; 2]);
        let frame = owner
            .extract_validated(generation, &snapshot, 1, None, &cancelled)
            .unwrap();
        let next = owner
            .prepare_generation(
                &project,
                4,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        assert_ne!(generation, next);
        assert_eq!(owner.status().generation_slots_used, 2);
        assert_eq!(owner.status().frame_slots_used, 2);
        assert_eq!(
            owner.status().frame_reserved_bytes,
            limits.frame_bytes_per_frame
        );
        let next_frame = owner
            .extract_validated(next, &snapshot, 2, None, &cancelled)
            .unwrap();
        assert_eq!(owner.status().frame_slots_used, 2);
        assert_eq!(owner.status().frame_reserved_bytes, 0);
        assert!(owner.status().frame_bytes <= limits.frame_bytes_total);

        owner.retire_generation(generation).unwrap();
        assert_eq!(
            owner.try_reclaim().pending_frames,
            1,
            "the retained old-ready frame must keep its slot and bytes"
        );
        drop(frame);
        let old_reclaimed = owner.try_reclaim();
        assert_eq!(old_reclaimed.reclaimed_frames, 1);
        assert_eq!(old_reclaimed.reclaimed_generations, 1);
        assert!(matches!(
            owner.stage_input(generation),
            Err(super::PacketRenderError::StaleCpuGeneration)
        ));
        assert_eq!(owner.stage_input(next).unwrap().project_generation(), 4);

        owner.retire_generation(next).unwrap();
        drop(next_frame);
        let new_reclaimed = owner.try_reclaim();
        assert_eq!(new_reclaimed.reclaimed_frames, 1);
        assert_eq!(new_reclaimed.reclaimed_generations, 1);
        assert_eq!(owner.status().generation_slots_used, 0);
        assert_eq!(owner.status().frame_slots_used, 0);
    }

    #[test]
    fn managed_cpu_reclamation_waits_for_a_weak_mesh_leaf_alias() {
        let project = prepared_model_instances(&["asset:model-a"]);
        let session = SessionId::from_raw(708);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                10,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        let mesh = stage
            .resource_requests()
            .geometries
            .values()
            .next()
            .unwrap()
            .clone();
        let weak_mesh = Arc::downgrade(&mesh.0);
        drop(mesh);
        drop(stage);

        owner.retire_generation(generation).unwrap();
        let held = owner.try_reclaim();
        assert_eq!(held.pending_generations, 1);
        assert!(owner.status().prepared_metadata_bytes > 0);
        assert!(weak_mesh.upgrade().is_some());

        drop(weak_mesh);
        let reclaimed = owner.try_reclaim();
        assert_eq!(reclaimed.reclaimed_generations, 1);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
    }

    #[test]
    fn managed_cpu_reclamation_waits_for_a_weak_slot_assignment_alias() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(715);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                32,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        let weak_assignments = Arc::downgrade(stage.slot_assignments());
        let frame = owner
            .extract_validated(
                generation,
                &project_snapshot(&bindings, &[Transform::IDENTITY; 2]),
                913,
                None,
                &cancelled,
            )
            .unwrap();
        assert!(Arc::ptr_eq(
            stage.slot_assignments(),
            frame.slot_assignments().unwrap()
        ));

        owner.retire_generation(generation).unwrap();
        drop(stage);
        drop(frame);
        let held = owner.try_reclaim();
        assert_eq!(held.pending_generations, 1);
        assert!(owner.status().prepared_metadata_bytes > 0);
        assert!(weak_assignments.upgrade().is_some());

        drop(weak_assignments);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        assert_eq!(owner.status().prepared_metadata_bytes, 0);
        assert_eq!(owner.status().frame_bytes, 0);
    }

    #[test]
    fn managed_cpu_frames_obey_three_slot_backpressure_and_reclaim_releases_a_slot() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(705);
        let bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                5,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            )
            .unwrap();
        let mut snapshot = project_snapshot(&bindings, &[Transform::IDENTITY; 2]);
        let frame_one = owner
            .extract_validated(generation, &snapshot, 100, None, &cancelled)
            .unwrap();
        snapshot.sequence += 1;
        snapshot.tick = Tick(snapshot.tick.0 + 1);
        let frame_two = owner
            .extract_validated(generation, &snapshot, 101, None, &cancelled)
            .unwrap();
        snapshot.sequence += 1;
        snapshot.tick = Tick(snapshot.tick.0 + 1);
        let frame_three = owner
            .extract_validated(generation, &snapshot, 102, None, &cancelled)
            .unwrap();
        assert_eq!(owner.status().frame_slots_used, 3);
        assert!(matches!(
            owner.extract_validated(generation, &snapshot, 103, None, &cancelled),
            Err(super::PacketRenderError::CpuResidencyBusy)
        ));

        drop(frame_one);
        assert_eq!(owner.try_reclaim().reclaimed_frames, 1);
        let frame_four = owner
            .extract_validated(generation, &snapshot, 103, None, &cancelled)
            .unwrap();
        assert_eq!(frame_four.publication_sequence(), 103);
        assert_eq!(owner.status().frame_slots_used, 3);
        drop(frame_two);
        drop(frame_three);
        drop(frame_four);
        owner.retire_generation(generation).unwrap();
        let reclaimed = owner.try_reclaim();
        assert_eq!(reclaimed.reclaimed_frames, 3);
        assert_eq!(reclaimed.reclaimed_generations, 1);
        assert_eq!(owner.status().frame_slots_used, 0);
        assert_eq!(owner.status().frame_bytes, 0);
    }

    #[test]
    fn managed_cpu_generation_preflights_first_frame_and_does_not_record_rejected_scratch_peak() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(703);
        let bindings = project_bindings(&project, session);
        let mut limits = PacketCpuLimits {
            frame_bytes_per_frame: 256,
            frame_bytes_total: 512,
            ..PacketCpuLimits::default()
        };
        assert!(limits.validate().is_ok());
        let mut owner = PacketCpuOwner::new(
            limits,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        assert!(matches!(
            owner.prepare_generation(
                &project,
                1,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            ),
            Err(super::PacketRenderError::CpuBudgetExceeded {
                resource: "frame backing",
                limit: 256,
                ..
            })
        ));
        assert_eq!(owner.status().generation_slots_used, 0);
        assert_eq!(owner.status().frame_reserved_bytes, 0);
        assert_eq!(owner.status().scratch_current_bytes, 0);

        limits.scratch_bytes = 1;
        let mut scratch_limited = PacketCpuOwner::new(
            limits,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            scratch_limited.prepare_generation(
                &project,
                1,
                session,
                &bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            ),
            Err(super::PacketRenderError::CpuBudgetExceeded {
                resource: "scratch",
                limit: 1,
                ..
            })
        ));
        assert_eq!(scratch_limited.status().scratch_current_bytes, 0);
        assert_eq!(scratch_limited.status().scratch_peak_bytes, 0);
    }

    #[test]
    fn managed_cpu_prepare_releases_scratch_and_reservations_on_error_or_cancellation() {
        let project = prepared_two_instances();
        let session = SessionId::from_raw(709);
        let valid_bindings = project_bindings(&project, session);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let not_cancelled = AtomicBool::new(false);
        assert!(matches!(
            owner.prepare_generation(
                &project,
                11,
                session,
                &BTreeMap::new(),
                SourcePin::new(Arc::new(())),
                &not_cancelled,
            ),
            Err(super::PacketRenderError::InvalidPacket(_))
        ));
        let after_error = owner.status();
        assert!(after_error.scratch_peak_bytes > 0);
        assert_eq!(after_error.scratch_current_bytes, 0);
        assert_eq!(after_error.prepared_metadata_reserved_bytes, 0);
        assert_eq!(after_error.legacy_mesh_copy_reserved_bytes, 0);
        assert_eq!(after_error.frame_reserved_bytes, 0);
        assert_eq!(after_error.generation_slots_used, 0);

        let cancelled_after_resources = AtomicBool::new(false);
        assert!(matches!(
            owner.prepare_generation_cancel_after_resources(
                &project,
                11,
                session,
                &valid_bindings,
                SourcePin::new(Arc::new(())),
                &cancelled_after_resources,
            ),
            Err(super::PacketRenderError::CpuCancelled)
        ));
        assert!(cancelled_after_resources.load(Ordering::Relaxed));
        let after_work_cancel = owner.status();
        assert_eq!(after_work_cancel.scratch_current_bytes, 0);
        assert_eq!(after_work_cancel.prepared_metadata_reserved_bytes, 0);
        assert_eq!(after_work_cancel.legacy_mesh_copy_reserved_bytes, 0);
        assert_eq!(after_work_cancel.frame_reserved_bytes, 0);
        assert_eq!(after_work_cancel.generation_slots_used, 0);

        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            owner.prepare_generation(
                &project,
                11,
                session,
                &valid_bindings,
                SourcePin::new(Arc::new(())),
                &cancelled,
            ),
            Err(super::PacketRenderError::CpuCancelled)
        ));
        let after_cancel = owner.status();
        assert_eq!(
            after_cancel.scratch_peak_bytes,
            after_work_cancel.scratch_peak_bytes
        );
        assert_eq!(after_cancel.scratch_current_bytes, 0);
        assert_eq!(after_cancel.prepared_metadata_reserved_bytes, 0);
        assert_eq!(after_cancel.legacy_mesh_copy_reserved_bytes, 0);
        assert_eq!(after_cancel.frame_reserved_bytes, 0);
        assert_eq!(after_cancel.generation_slots_used, 0);
    }

    #[test]
    fn empty_managed_generation_keeps_source_pin_until_stage_and_frame_roots_are_released() {
        let project = prepared_empty_project();
        let session = SessionId::from_raw(704);
        let bindings = project_bindings(&project, session);
        let drop_thread = Arc::new(Mutex::new(None));
        let source = Arc::new(SourceDropThread(Arc::clone(&drop_thread)));
        let weak = Arc::downgrade(&source);
        let source_erased: Arc<dyn Send + Sync> = source.clone();
        drop(source);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .unwrap();
        let cancelled = AtomicBool::new(false);
        let generation = owner
            .prepare_generation(
                &project,
                1,
                session,
                &bindings,
                SourcePin::new(source_erased),
                &cancelled,
            )
            .unwrap();
        let stage = owner.stage_input(generation).unwrap();
        assert_eq!(stage.geometry_count(), 0);
        assert_eq!(stage.draw_count(), 0);
        assert_eq!(stage.texture_count(), 0);
        let snapshot = project_snapshot(&bindings, &[]);
        let frame = owner
            .extract_validated(generation, &snapshot, 2, None, &cancelled)
            .unwrap();
        assert_eq!(frame.packet().instances().len(), 0);
        owner.retire_generation(generation).unwrap();
        drop(frame);
        assert_eq!(owner.try_reclaim().pending_generations, 1);
        assert!(weak.upgrade().is_some());
        drop(stage);
        assert_eq!(owner.try_reclaim().reclaimed_generations, 1);
        assert!(weak.upgrade().is_none());
        assert_eq!(*drop_thread.lock().unwrap(), Some(thread::current().id()));
    }

    #[test]
    fn cpu_limits_reject_a_per_generation_reservation_that_cannot_transition() {
        let mut limits = PacketCpuLimits::default();
        limits.prepared_metadata_bytes_total = limits.prepared_metadata_bytes_per_generation;
        assert!(matches!(
            PacketCpuOwner::new(
                limits,
                PacketRendererLimits::default(),
                PacketAssetLimits::default()
            ),
            Err(super::PacketRenderError::InvalidCpuLimits)
        ));

        limits = PacketCpuLimits::default();
        limits.frame_bytes_total = limits.frame_bytes_per_frame * 2 - 1;
        assert!(matches!(
            PacketCpuOwner::new(
                limits,
                PacketRendererLimits::default(),
                PacketAssetLimits::default()
            ),
            Err(super::PacketRenderError::InvalidCpuLimits)
        ));
    }
}
