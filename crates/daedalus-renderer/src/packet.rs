//! Immutable renderer-owned geometry and compact per-snapshot draw packets.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, OnceLock},
};

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

use crate::{Camera, DirectionalLight, Material, Mesh, RendererError, Vertex};

const MESH_LAYOUT_VERSION: u32 = 1;
const MESH_HASH_DOMAIN: &[u8] = b"Daedalus renderer mesh payload\0v1\0";
const MODEL_TEXTURE_HASH_DOMAIN: &[u8] = b"Daedalus renderer model texture payload\0v1\0";

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
    backing: MeshBacking,
    gpu_bytes: usize,
}

#[derive(Debug)]
enum MeshBacking {
    Legacy(Mesh),
    Imported(Arc<GeometryResource>),
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
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => mesh.vertices.len(),
            MeshBacking::Imported(geometry) => geometry.vertex_count(),
        }
    }
    pub fn index_count(&self) -> usize {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => mesh.indices.len(),
            MeshBacking::Imported(geometry) => geometry.index_count(),
        }
    }
    pub(crate) fn legacy_mesh(&self) -> Option<&Mesh> {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => Some(mesh),
            MeshBacking::Imported(_) => None,
        }
    }
    pub(crate) fn imported_geometry(&self) -> Option<&Arc<GeometryResource>> {
        match &self.0.backing {
            MeshBacking::Legacy(_) => None,
            MeshBacking::Imported(geometry) => Some(geometry),
        }
    }
    pub(crate) fn indices(&self) -> &[u32] {
        match &self.0.backing {
            MeshBacking::Legacy(mesh) => &mesh.indices,
            MeshBacking::Imported(geometry) => geometry.indices(),
        }
    }
    pub fn gpu_bytes(&self) -> usize {
        self.0.gpu_bytes
    }
    pub(crate) fn key(&self) -> GeometryKey {
        GeometryKey {
            hash: self.0.payload_hash,
            layout: self.0.layout_version,
        }
    }
}

const MODEL_MESH_LAYOUT_VERSION: u32 = 2;

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
}

#[derive(Clone, Debug)]
pub(crate) struct ModelDrawBinding {
    pub texture: Option<Arc<ModelTextureUpload>>,
    pub sampler: ImageSampler,
}

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

pub(crate) fn texture_upload(
    image: Arc<ImageAsset>,
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
    if image.mips().len() + 1 != expected_mip_count {
        return Err(PacketRenderError::InvalidPacket(
            "model texture mip chain is incomplete".into(),
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
    model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
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
            && self.model_bindings == other.model_bindings
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
            model_bindings: Arc::new(HashMap::new()),
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
    pub(crate) fn model_binding(&self, id: &RenderInstanceId) -> Option<&ModelDrawBinding> {
        self.model_bindings.get(id)
    }
    pub fn with_camera(&self, camera: Camera) -> Self {
        let mut packet = self.clone();
        packet.camera = camera;
        packet
    }
    pub fn with_instances(&self, instances: Vec<RenderInstance>) -> Self {
        let mut packet = self.clone();
        packet.instances = instances.into();
        packet.model_bindings = Arc::new(HashMap::new());
        packet.validated_normals = Arc::new(OnceLock::new());
        packet
    }

    pub(crate) fn with_model_bindings(
        mut self,
        model_bindings: HashMap<RenderInstanceId, ModelDrawBinding>,
    ) -> Self {
        self.model_bindings = Arc::new(model_bindings);
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
            let binding = self.model_bindings.get(&instance.id);
            if instance.mesh.imported_geometry().is_some() != binding.is_some() {
                return Err(PacketRenderError::InvalidPacket(
                    "model draw binding does not match its imported geometry".into(),
                ));
            }
            if let Some(binding) = binding {
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
        }
        if self.model_bindings.keys().any(|id| !ids.contains(id)) {
            return Err(PacketRenderError::InvalidPacket(
                "model draw binding refers to a missing packet instance".into(),
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
    expanded_instance_count: usize,
    limits: PacketRendererLimits,
}

struct ProjectBinding {
    stable_id: StableId,
    mesh: Option<MeshHandle>,
    material: Material,
    model_draws: Option<Arc<[PreparedModelDraw]>>,
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
            mesh_handles.insert(asset_id.clone(), handle);
        }
        let mut models = HashMap::<StableId, Arc<StaticModel>>::new();
        let mut model_stats = HashMap::<StableId, ModelExpansionStats>::new();
        if !project.model_renderers().is_empty() {
            let catalog = project.asset_catalog().ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "model renderer bindings require a prepared asset catalog".into(),
                )
            })?;
            for binding in project.model_renderers().values() {
                let model_id = binding.model_asset_id();
                if models.contains_key(model_id) {
                    continue;
                }
                let Some(ImportedAsset::Model(model)) = catalog.asset(model_id) else {
                    return Err(PacketRenderError::InvalidPacket(format!(
                        "model renderer references missing or non-model asset {model_id}"
                    )));
                };
                let stats = model_expansion_stats(model, limits)?;
                models.insert(model_id.clone(), Arc::clone(model));
                model_stats.insert(model_id.clone(), stats);
            }
        }
        let mut expanded_instance_count = 0usize;
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        for entity in project.entities() {
            if let Some(mesh_id) = &entity.mesh {
                let mesh = mesh_handles.get(mesh_id).ok_or_else(|| {
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
                    mesh.vertex_count(),
                    limits.max_expanded_vertices,
                    "expanded vertex count",
                )?;
                expanded_indices = checked_add_limit(
                    expanded_indices,
                    mesh.index_count(),
                    limits.max_expanded_indices,
                    "expanded index count",
                )?;
            }
            if let Some(binding) = project.model_renderers().get(&entity.id) {
                let stats = model_stats.get(binding.model_asset_id()).ok_or_else(|| {
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
        }

        // Count every selected-scene occurrence for every owning entity before
        // constructing draw templates or hashing any texture payload.
        let mut imported_geometries = HashMap::<LeafCookKey, MeshHandle>::new();
        let mut texture_uploads = HashMap::<LeafCookKey, Arc<ModelTextureUpload>>::new();
        let mut model_templates = HashMap::<StableId, Arc<[PreparedModelDraw]>>::new();
        for (model_id, model) in &models {
            let draws = expand_model_draws(
                model_id,
                model,
                project_generation,
                &mut imported_geometries,
                &mut texture_uploads,
                model_stats[model_id].draws,
            )?;
            model_templates.insert(model_id.clone(), draws.into());
        }
        let mut all_geometry_keys = HashSet::new();
        let mut geometry_bytes = 0usize;
        for handle in mesh_handles.values().chain(imported_geometries.values()) {
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
            bindings.push((
                handle,
                ProjectBinding {
                    stable_id: entity.id.clone(),
                    mesh,
                    material,
                    model_draws,
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
            expanded_instance_count,
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
        let mut instances = Vec::with_capacity(self.expanded_instance_count);
        let mut model_bindings = HashMap::with_capacity(self.expanded_instance_count);
        let mut expanded_vertices = 0usize;
        let mut expanded_indices = 0usize;
        for (handle, binding) in &self.bindings {
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
                    model_bindings.insert(
                        id,
                        ModelDrawBinding {
                            texture: draw.texture.clone(),
                            sampler: draw.sampler,
                        },
                    );
                }
            }
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
        )
        .with_model_bindings(model_bindings))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ModelExpansionStats {
    draws: usize,
    vertices: usize,
    indices: usize,
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

fn model_expansion_stats(
    model: &StaticModel,
    limits: PacketRendererLimits,
) -> Result<ModelExpansionStats, PacketRenderError> {
    let mut children = vec![Vec::<u32>::new(); model.nodes().len()];
    for (child_index, node) in model.nodes().iter().enumerate() {
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
    let mut visited = HashSet::new();
    let mut stack = Vec::<(u32, Option<Mat4>)>::new();
    for root in model.selected_scene_roots() {
        stack.push((*root, None));
        while let Some((index, parent_global)) = stack.pop() {
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
                let primitive = model
                    .primitives()
                    .get(*primitive_index as usize)
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(
                            "selected model node primitive is out of range".into(),
                        )
                    })?;
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
            }
            for child in children[index as usize].iter().rev() {
                stack.push((*child, Some(global)));
            }
        }
    }
    Ok(stats)
}

fn expand_model_draws(
    model_id: &StableId,
    model: &StaticModel,
    project_generation: u64,
    geometry_handles: &mut HashMap<LeafCookKey, MeshHandle>,
    texture_uploads: &mut HashMap<LeafCookKey, Arc<ModelTextureUpload>>,
    expected_draw_count: usize,
) -> Result<Vec<PreparedModelDraw>, PacketRenderError> {
    let mut children = vec![Vec::<u32>::new(); model.nodes().len()];
    for (child_index, node) in model.nodes().iter().enumerate() {
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
                stack.push((*child, Some(global)));
            }
        }
    }

    let mut draws = Vec::with_capacity(expected_draw_count);
    for (node_index, node_global) in occurrences {
        let node = &model.nodes()[node_index as usize];
        for primitive_index in node.primitive_indices() {
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
                let mesh = MeshHandle::imported(model_id.clone(), project_generation, geometry)?;
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
                    let upload = texture_upload(Arc::clone(owned.asset()))?;
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
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use daedalus_content::project::{PreparedProject, ProjectLimits, prepare_project_from_bytes};
    use daedalus_content::scene::StableId;
    use daedalus_core::{
        EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
    };
    use glam::{Mat4, Vec3};

    use crate::{Mesh, PacketAssetLimits, Vertex};

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
