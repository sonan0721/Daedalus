//! Bounded same-device resource staging for managed CPU generations.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use thiserror::Error;

use crate::{
    CpuFrameHandle, PacketRenderError, PacketStageInput, RenderPacket, RenderView, RendererError,
    SourcePin,
    packet::SpriteQuadVertex,
    packet::{ManagedResourceSetToken, ManagedSlotAssignments},
    renderer::{GpuModelVertex, GpuVertex},
};

use super::{
    GeometryKey, GeometryResource, ModelTextureUpload, PacketRenderer, PacketViewSubmission,
    ResourceAdmissionPlan, SamplerKey, SamplerResource, TextureBindGroupResource, TextureKey,
    TextureResource, TextureSamplerKey, check_submission_admission,
};

const DEFAULT_STAGE_UPLOAD_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_STAGE_CREATE_CALLS: usize = 8;
const MAX_STAGE_DIAGNOSTIC_CODE_BYTES: usize = 64;
const MAX_STAGE_DIAGNOSTIC_MESSAGE_BYTES: usize = 2048;

static NEXT_RENDERER_NAMESPACE: AtomicU64 = AtomicU64::new(1);

/// Per-combined-submit work limits for a GPU resource stage.
///
/// Builders only lower the defaults. The upload limit is measured in raw
/// resource bytes; staging-buffer accounting separately includes conservative
/// padded texture rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketStageLimits {
    upload_bytes_per_submission: usize,
    max_resource_creates_per_submission: usize,
}

impl Default for PacketStageLimits {
    fn default() -> Self {
        Self {
            upload_bytes_per_submission: DEFAULT_STAGE_UPLOAD_BYTES,
            max_resource_creates_per_submission: DEFAULT_STAGE_CREATE_CALLS,
        }
    }
}

impl PacketStageLimits {
    pub fn with_upload_bytes_per_submission(mut self, bytes: usize) -> Self {
        self.upload_bytes_per_submission = bytes;
        self
    }

    pub fn with_max_resource_creates_per_submission(mut self, calls: usize) -> Self {
        self.max_resource_creates_per_submission = calls;
        self
    }

    pub fn upload_bytes_per_submission(self) -> usize {
        self.upload_bytes_per_submission
    }

    pub fn max_resource_creates_per_submission(self) -> usize {
        self.max_resource_creates_per_submission
    }

    fn validate(self) -> Result<(), PacketStageError> {
        if self.upload_bytes_per_submission == 0
            || self.upload_bytes_per_submission > DEFAULT_STAGE_UPLOAD_BYTES
            || self.max_resource_creates_per_submission == 0
            || self.max_resource_creates_per_submission > DEFAULT_STAGE_CREATE_CALLS
        {
            return Err(PacketStageError::InvalidLimits);
        }
        Ok(())
    }
}

fn validate_candidate_create_group(
    limits: PacketStageLimits,
    create_calls: usize,
) -> Result<(), PacketStageError> {
    if create_calls > limits.max_resource_creates_per_submission {
        Err(PacketStageError::InvalidLimits)
    } else {
        Ok(())
    }
}

/// Opaque renderer-local identity for one stage. A stage id from another
/// PacketRenderer is never treated as a valid resource generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PacketStageId {
    renderer_namespace: u64,
    sequence: u64,
}

impl PacketStageId {
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Observable bounded lifecycle for an admitted GPU stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketStageState {
    Admitted,
    Uploading,
    AwaitingCompletion,
    Ready,
    Cancelling,
    Retiring,
    Cancelled,
    Failed,
}

/// Bounded terminal diagnostic for a rejected or failed stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketStageFailure {
    code: Arc<str>,
    message: Arc<str>,
}

impl PacketStageFailure {
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Small status only; resource keys, source payloads and draw arrays are never
/// copied into a status response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketStageStatus {
    id: PacketStageId,
    state: PacketStageState,
    session: daedalus_core::SessionId,
    project_generation: u64,
    source_digest: Arc<str>,
    total_upload_bytes: usize,
    uploaded_bytes: usize,
    staging_bytes_submitted: usize,
    create_calls_completed: usize,
    initialization_serial: Option<u64>,
    initialization_complete: bool,
    failure: Option<PacketStageFailure>,
}

impl PacketStageStatus {
    pub fn id(&self) -> PacketStageId {
        self.id
    }

    pub fn state(&self) -> PacketStageState {
        self.state
    }

    pub fn session(&self) -> daedalus_core::SessionId {
        self.session
    }

    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn total_upload_bytes(&self) -> usize {
        self.total_upload_bytes
    }

    pub fn uploaded_bytes(&self) -> usize {
        self.uploaded_bytes
    }

    pub fn staging_bytes_submitted(&self) -> usize {
        self.staging_bytes_submitted
    }

    pub fn create_calls_completed(&self) -> usize {
        self.create_calls_completed
    }

    pub fn initialization_serial(&self) -> Option<u64> {
        self.initialization_serial
    }

    pub fn initialization_complete(&self) -> bool {
        self.initialization_complete
    }

    pub fn failure(&self) -> Option<&PacketStageFailure> {
        self.failure.as_ref()
    }
}

/// Error from stage admission, lookup or cancellation.
#[derive(Debug, Error)]
pub enum PacketStageError {
    #[error(
        "packet stage limits must be positive, within hard ceilings, and allow the minimum atomic operation"
    )]
    InvalidLimits,
    #[error("packet stage identifier is foreign, stale, or exhausted")]
    InvalidStageId,
    #[error("another GPU resource stage is still active or retiring")]
    StageBusy,
    #[error("CPU frame limits do not match this PacketRenderer")]
    FrameLimitsMismatch,
    #[error("CPU frame identity is not the active generation or admitted stage")]
    FrameIdentityMismatch,
    #[error("CPU frame references resources that are not fully ready on this PacketRenderer")]
    FrameResourcesUnavailable,
    #[error("packet stage bookkeeping reservation failed")]
    AllocationFailed,
    #[error("renderer bookkeeping budget exceeded ({bytes} bytes > {limit} bytes)")]
    BookkeepingBudgetExceeded { bytes: usize, limit: usize },
    #[error(
        "one indivisible geometry record or padded texture row exceeds the available stage upload budget ({bytes} > {limit})"
    )]
    UploadUnitTooLarge { bytes: usize, limit: usize },
    #[error("packet stage generation or upload accounting overflowed")]
    CounterExhausted,
    #[error("packet rendering failed: {0}")]
    Render(#[from] PacketRenderError),
}

/// A combined device submit, with an optional presented packet for bootstrap
/// frames and compact stage progress for the caller.
#[derive(Clone, Debug)]
pub struct PacketStageFrameSubmission {
    frame: Option<PacketViewSubmission>,
    progress: Option<PacketStageStatus>,
}

impl PacketStageFrameSubmission {
    pub fn frame(&self) -> Option<&PacketViewSubmission> {
        self.frame.as_ref()
    }

    pub fn progress(&self) -> Option<&PacketStageStatus> {
        self.progress.as_ref()
    }

    pub fn into_parts(self) -> (Option<PacketViewSubmission>, Option<PacketStageStatus>) {
        (self.frame, self.progress)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketRendererDrainStatus {
    Pending,
    Drained,
}

#[derive(Debug, Error)]
pub enum PacketRendererDrainError {
    #[error("renderer completion polling failed: {0}")]
    Poll(#[from] RendererError),
    #[error("renderer staging operation failed: {0}")]
    Stage(#[from] PacketStageError),
}

pub(super) struct StageState {
    renderer_namespace: u64,
    next_sequence: u64,
    active: Option<StageRecord>,
    last_terminal: Option<PacketStageStatus>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StagePanicPoint {
    GeometryAfterVertex,
    ImportedTextureAfterTexture,
    DefaultTextureAfterTexture,
}

pub(super) struct StageRecord {
    id: PacketStageId,
    input: Option<PacketStageInput>,
    resource_set_token: Option<Arc<ManagedResourceSetToken>>,
    slot_assignments: ManagedSlotAssignments,
    _source_pin: SourcePin,
    plan: Option<ResourceAdmissionPlan>,
    pending_victims: Option<StageVictims>,
    status: PacketStageStatus,
    created: StagedResources,
    pending_geometry: Option<PendingGeometry>,
    pending_texture: Option<PendingTexture>,
    pending_default_texture: Option<PendingDefaultTexture>,
    create_tasks: VecDeque<CreateTask>,
    pending_upload_serial: Option<u64>,
    victims_applied: bool,
    limits: PacketStageLimits,
    cursor: UploadCursor,
    total_upload_bytes: usize,
    raw_uploaded_bytes: usize,
    staging_bytes_submitted: usize,
    create_calls_completed: usize,
    promoted: bool,
    expected: Option<ExpectedResources>,
    #[cfg(test)]
    panic_after_pair_handle: Option<StagePanicPoint>,
}

#[derive(Default)]
pub(super) struct StageVictims {
    geometries: Vec<GeometryKey>,
    textures: Vec<TextureKey>,
    samplers: Vec<SamplerKey>,
    bind_groups: Vec<TextureSamplerKey>,
}

impl StageVictims {
    fn is_empty(&self) -> bool {
        self.geometries.is_empty()
            && self.textures.is_empty()
            && self.samplers.is_empty()
            && self.bind_groups.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct StageCreateBatch {
    task_count: usize,
    create_calls: usize,
    apply_victims: bool,
}

impl StageCreateBatch {
    fn is_empty(self) -> bool {
        self.task_count == 0 && !self.apply_victims
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum StageSubmitAction {
    Create {
        id: PacketStageId,
        batch: StageCreateBatch,
    },
    Promote {
        id: PacketStageId,
    },
}

pub(super) struct ExpectedResources {
    geometries: HashSet<GeometryKey>,
    textures: HashSet<TextureKey>,
    samplers: HashSet<SamplerKey>,
    bind_groups: HashSet<TextureSamplerKey>,
    default_texture: bool,
    sprite_uv_buffer: bool,
}

#[derive(Default)]
pub(super) struct StagedResources {
    geometries: HashMap<GeometryKey, StagedGeometry>,
    textures: HashMap<TextureKey, StagedTexture>,
    samplers: HashMap<SamplerKey, SamplerResource>,
    bind_groups: HashMap<TextureSamplerKey, TextureBindGroupResource>,
    default_texture: Option<TextureResource>,
    sprite_uv_buffer: Option<wgpu::Buffer>,
}

pub(super) struct StagedGeometry {
    resource: GeometryResource,
    vertex_bytes: usize,
    index_bytes: usize,
    vertex_uploaded_bytes: usize,
    index_uploaded_bytes: usize,
    initialized_serial: Option<u64>,
}

/// A first geometry buffer remains rooted here if creation of its paired index
/// buffer unwinds. It is never visible to upload or draw planning.
pub(super) struct PendingGeometry {
    key: GeometryKey,
    vertex: wgpu::Buffer,
    index: Option<wgpu::Buffer>,
    vertex_bytes: usize,
    index_bytes: usize,
    index_count: u32,
    mesh_bytes: usize,
    last_touch: u64,
}

/// Imported texture creation is retained until its view has also been made.
pub(super) struct PendingTexture {
    key: TextureKey,
    texture: wgpu::Texture,
    view: Option<wgpu::TextureView>,
    upload: Arc<ModelTextureUpload>,
    bytes: usize,
    last_touch: u64,
}

pub(super) struct PendingDefaultTexture {
    texture: wgpu::Texture,
    view: Option<wgpu::TextureView>,
    last_touch: u64,
}

pub(super) struct StagedTexture {
    resource: TextureResource,
    upload: Option<Arc<ModelTextureUpload>>,
    uploaded_bytes: usize,
    expected_bytes: usize,
    initialized_serial: Option<u64>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct UploadCursor {
    geometry_index: usize,
    geometry_part: u8,
    geometry_offset: usize,
    texture_index: usize,
    mip_index: usize,
    texture_row: u32,
    default_uploaded: bool,
}

pub(super) enum PreparedUpload {
    GeometryVertex {
        key: GeometryKey,
        data: PreparedVertexData,
        byte_offset: usize,
    },
    GeometryIndex {
        key: GeometryKey,
        byte_offset: usize,
        first_index: usize,
        index_count: usize,
    },
    TextureRows {
        key: TextureKey,
        mip_level: u32,
        row_start: u32,
        row_count: u32,
        width: u32,
        upload: Arc<ModelTextureUpload>,
        data_start: usize,
        data_len: usize,
        bytes_per_row: u32,
    },
    DefaultTexture,
}

pub(super) enum PreparedVertexData {
    Imported(Vec<GpuModelVertex>),
    Legacy(Vec<GpuVertex>),
    Sprite(Vec<SpriteQuadVertex>),
}

impl PreparedVertexData {
    pub(super) fn bytes(&self) -> &[u8] {
        match self {
            Self::Imported(values) => bytemuck::cast_slice(values),
            Self::Legacy(values) => bytemuck::cast_slice(values),
            Self::Sprite(values) => bytemuck::cast_slice(values),
        }
    }

    fn scratch_bytes(&self) -> Result<usize, PacketStageError> {
        let (capacity, element_bytes) = match self {
            Self::Imported(values) => (values.capacity(), std::mem::size_of::<GpuModelVertex>()),
            Self::Legacy(values) => (values.capacity(), std::mem::size_of::<GpuVertex>()),
            Self::Sprite(values) => (values.capacity(), std::mem::size_of::<SpriteQuadVertex>()),
        };
        capacity
            .checked_mul(element_bytes)
            .ok_or(PacketStageError::CounterExhausted)
    }
}

pub(super) struct PreparedUploadChunk {
    pub(super) upload: PreparedUpload,
    pub(super) next_cursor: UploadCursor,
    pub(super) raw_bytes: usize,
    pub(super) staging_bytes: usize,
    pub(super) cpu_scratch_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
struct StageUploadBudget {
    raw_bytes: usize,
    permanent_staging_bytes: usize,
    staging_headroom_bytes: usize,
    staging_available_bytes: usize,
    scratch_bytes: usize,
}

impl StageUploadBudget {
    fn full(limits: PacketStageLimits, asset_limits: crate::PacketAssetLimits) -> Self {
        Self {
            raw_bytes: limits.upload_bytes_per_submission,
            permanent_staging_bytes: asset_limits.upload_staging_bytes,
            staging_headroom_bytes: asset_limits.upload_staging_bytes,
            staging_available_bytes: asset_limits.upload_staging_bytes,
            scratch_bytes: asset_limits.cpu_upload_scratch_bytes,
        }
    }

    fn with_submission_headroom(
        limits: PacketStageLimits,
        asset_limits: crate::PacketAssetLimits,
        frame_staging_bytes: usize,
        overlay_staging_bytes: usize,
        in_flight_staging_bytes: usize,
    ) -> Result<Self, PacketStageError> {
        let permanent_staging_bytes = asset_limits
            .upload_staging_bytes
            .checked_sub(overlay_staging_bytes)
            .ok_or(PacketStageError::Render(
                PacketRenderError::AssetByteBudgetExceeded {
                    resource: "upload staging overlay",
                    bytes: overlay_staging_bytes,
                    limit: asset_limits.upload_staging_bytes,
                },
            ))?;
        let staging_headroom_bytes = asset_limits
            .upload_staging_bytes
            .checked_sub(frame_staging_bytes)
            .ok_or(PacketStageError::Render(
                PacketRenderError::AssetByteBudgetExceeded {
                    resource: "upload staging",
                    bytes: frame_staging_bytes,
                    limit: asset_limits.upload_staging_bytes,
                },
            ))?;
        let staging_available_bytes =
            staging_headroom_bytes.saturating_sub(in_flight_staging_bytes);
        Ok(Self {
            raw_bytes: limits.upload_bytes_per_submission,
            permanent_staging_bytes,
            staging_headroom_bytes,
            staging_available_bytes,
            scratch_bytes: asset_limits.cpu_upload_scratch_bytes,
        })
    }

    fn validate_unit(
        self,
        raw_unit_bytes: usize,
        staging_unit_bytes: usize,
        scratch_unit_bytes: usize,
    ) -> Result<(), PacketStageError> {
        self.validate_intrinsic_unit(raw_unit_bytes, staging_unit_bytes, scratch_unit_bytes)?;
        if staging_unit_bytes > self.staging_headroom_bytes {
            return Err(PacketStageError::UploadUnitTooLarge {
                bytes: staging_unit_bytes,
                limit: self.staging_headroom_bytes,
            });
        }
        Ok(())
    }

    fn validate_intrinsic_unit(
        self,
        raw_unit_bytes: usize,
        staging_unit_bytes: usize,
        scratch_unit_bytes: usize,
    ) -> Result<(), PacketStageError> {
        if raw_unit_bytes == 0 || staging_unit_bytes == 0 {
            return Err(PacketStageError::CounterExhausted);
        }
        if raw_unit_bytes > self.raw_bytes {
            return Err(PacketStageError::UploadUnitTooLarge {
                bytes: raw_unit_bytes,
                limit: self.raw_bytes,
            });
        }
        if scratch_unit_bytes > self.scratch_bytes {
            return Err(PacketStageError::Render(
                PacketRenderError::AssetByteBudgetExceeded {
                    resource: "CPU upload scratch",
                    bytes: scratch_unit_bytes,
                    limit: self.scratch_bytes,
                },
            ));
        }
        Ok(())
    }

    fn max_units(
        self,
        raw_unit_bytes: usize,
        staging_unit_bytes: usize,
        scratch_unit_bytes: usize,
    ) -> Result<Option<usize>, PacketStageError> {
        self.validate_intrinsic_unit(raw_unit_bytes, staging_unit_bytes, scratch_unit_bytes)?;
        if staging_unit_bytes > self.permanent_staging_bytes {
            return Err(PacketStageError::UploadUnitTooLarge {
                bytes: staging_unit_bytes,
                limit: self.permanent_staging_bytes,
            });
        }
        if staging_unit_bytes > self.staging_headroom_bytes
            || staging_unit_bytes > self.staging_available_bytes
        {
            return Ok(None);
        }

        let raw_units = self.raw_bytes / raw_unit_bytes;
        let staging_units = self.staging_available_bytes / staging_unit_bytes;
        let scratch_units = if scratch_unit_bytes == 0 {
            usize::MAX
        } else {
            self.scratch_bytes / scratch_unit_bytes
        };
        Ok(Some(raw_units.min(staging_units).min(scratch_units)))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum CreateTask {
    ModelPipeline,
    SpritePipeline,
    Geometry(GeometryKey),
    Texture(TextureKey),
    DefaultTexture,
    SpriteUvBuffer,
    Sampler(SamplerKey),
    BindGroup(TextureSamplerKey),
}

#[derive(Default)]
pub(super) struct ProtectedResources {
    pub(super) geometries: HashSet<GeometryKey>,
    pub(super) textures: HashSet<TextureKey>,
    pub(super) samplers: HashSet<SamplerKey>,
    pub(super) bind_groups: HashSet<TextureSamplerKey>,
}

impl StageState {
    pub(super) fn new() -> Option<Self> {
        let namespace = NEXT_RENDERER_NAMESPACE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .ok()?;
        Some(Self {
            renderer_namespace: namespace,
            next_sequence: 1,
            active: None,
            last_terminal: None,
        })
    }

    #[cfg(test)]
    pub(super) fn inject_pair_panic_for_test(&mut self, point: StagePanicPoint) {
        self.active
            .as_mut()
            .expect("stage admitted before panic injection")
            .panic_after_pair_handle = Some(point);
    }

    #[cfg(test)]
    pub(super) fn has_incomplete_pair_for_test(&self, point: StagePanicPoint) -> bool {
        let Some(record) = self.active.as_ref() else {
            return false;
        };
        match point {
            StagePanicPoint::GeometryAfterVertex => record
                .pending_geometry
                .as_ref()
                .is_some_and(|pending| pending.index.is_none()),
            StagePanicPoint::ImportedTextureAfterTexture => record
                .pending_texture
                .as_ref()
                .is_some_and(|pending| pending.view.is_none()),
            StagePanicPoint::DefaultTextureAfterTexture => record
                .pending_default_texture
                .as_ref()
                .is_some_and(|pending| pending.view.is_none()),
        }
    }

    pub(super) fn blocks_ordinary_rendering(&self) -> bool {
        self.active.as_ref().is_some_and(|record| {
            matches!(
                record.status.state,
                PacketStageState::Admitted
                    | PacketStageState::Uploading
                    | PacketStageState::AwaitingCompletion
                    | PacketStageState::Ready
                    | PacketStageState::Cancelling
                    | PacketStageState::Retiring
            )
        })
    }

    pub(super) fn staged_resident_bytes(&self, sprite_uv_bytes: usize) -> usize {
        self.active.as_ref().map_or(0, |record| {
            record.created.resident_bytes().saturating_add(
                usize::from(record.created.sprite_uv_buffer.is_some()) * sprite_uv_bytes,
            )
        })
    }

    pub(super) fn matches(&self, id: PacketStageId) -> bool {
        id.renderer_namespace == self.renderer_namespace
            && (self.active.as_ref().is_some_and(|record| record.id == id)
                || self
                    .last_terminal
                    .as_ref()
                    .is_some_and(|status| status.id == id))
    }

    pub(super) fn status(&self, id: PacketStageId) -> Result<PacketStageStatus, PacketStageError> {
        if !self.matches(id) {
            return Err(PacketStageError::InvalidStageId);
        }
        if let Some(record) = self.active.as_ref().filter(|record| record.id == id) {
            return Ok(record.status.clone());
        }
        self.last_terminal
            .as_ref()
            .filter(|status| status.id == id)
            .cloned()
            .ok_or(PacketStageError::InvalidStageId)
    }
}

#[cfg(test)]
impl StageRecord {
    fn panic_after_pair_handle_for_test(&mut self, point: StagePanicPoint) {
        if self.panic_after_pair_handle == Some(point) {
            self.panic_after_pair_handle = None;
            panic!("injected staged resource pair panic after first handle");
        }
    }
}

impl StagedResources {
    pub(super) fn try_reserve(&mut self, plan: &ResourceAdmissionPlan) -> bool {
        self.geometries
            .try_reserve(plan.missing_geometries.len())
            .and_then(|_| self.textures.try_reserve(plan.missing_textures.len()))
            .and_then(|_| self.samplers.try_reserve(plan.missing_samplers.len()))
            .and_then(|_| self.bind_groups.try_reserve(plan.missing_bind_groups.len()))
            .is_ok()
    }

    pub(super) fn resident_bytes(&self) -> usize {
        let geometry = self
            .geometries
            .values()
            .map(|entry| entry.resource.bytes)
            .fold(0usize, usize::saturating_add);
        let texture = self
            .textures
            .values()
            .map(|entry| entry.resource.bytes)
            .fold(0usize, usize::saturating_add);
        let default = self.default_texture.as_ref().map_or(0, |entry| entry.bytes);
        geometry.saturating_add(texture).saturating_add(default)
    }
}

fn expected_resources(plan: &ResourceAdmissionPlan) -> Result<ExpectedResources, PacketStageError> {
    let mut expected = ExpectedResources {
        geometries: HashSet::new(),
        textures: HashSet::new(),
        samplers: HashSet::new(),
        bind_groups: HashSet::new(),
        default_texture: plan.default_texture_requested,
        sprite_uv_buffer: plan.sprite_uv_buffer_required,
    };
    expected
        .geometries
        .try_reserve(plan.requested_geometries.len())
        .map_err(|_| PacketStageError::AllocationFailed)?;
    expected
        .textures
        .try_reserve(plan.active_textures.len())
        .map_err(|_| PacketStageError::AllocationFailed)?;
    expected
        .samplers
        .try_reserve(plan.active_samplers.len())
        .map_err(|_| PacketStageError::AllocationFailed)?;
    expected
        .bind_groups
        .try_reserve(plan.active_bind_groups.len())
        .map_err(|_| PacketStageError::AllocationFailed)?;
    expected
        .geometries
        .extend(plan.requested_geometries.keys().copied());
    expected
        .textures
        .extend(plan.active_textures.iter().copied());
    expected
        .samplers
        .extend(plan.active_samplers.iter().copied());
    expected
        .bind_groups
        .extend(plan.active_bind_groups.iter().copied());
    Ok(expected)
}

fn geometry_buffer_bytes(mesh: &crate::MeshHandle) -> Result<(usize, usize), PacketStageError> {
    let vertex_size = geometry_vertex_stride(mesh);
    let vertex_bytes = mesh
        .vertex_count()
        .checked_mul(vertex_size)
        .ok_or(PacketStageError::CounterExhausted)?;
    let index_bytes = mesh
        .index_count()
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or(PacketStageError::CounterExhausted)?;
    Ok((vertex_bytes, index_bytes))
}

fn geometry_vertex_stride(mesh: &crate::MeshHandle) -> usize {
    if mesh.imported_geometry().is_some() {
        std::mem::size_of::<GpuModelVertex>()
    } else if mesh.sprite_quad_geometry().is_some() {
        std::mem::size_of::<SpriteQuadVertex>()
    } else {
        std::mem::size_of::<GpuVertex>()
    }
}

fn pack_vertex_range(
    mesh: &crate::MeshHandle,
    first: usize,
    count: usize,
) -> Result<PreparedVertexData, PacketStageError> {
    let end = first
        .checked_add(count)
        .ok_or(PacketStageError::CounterExhausted)?;
    if end > mesh.vertex_count() {
        return Err(PacketStageError::CounterExhausted);
    }
    if let Some(geometry) = mesh.imported_geometry() {
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(count)
            .map_err(|_| PacketStageError::AllocationFailed)?;
        let uv = geometry.uv0();
        for index in first..end {
            packed.push(GpuModelVertex {
                position: geometry.positions()[index],
                normal: geometry.normals()[index],
                uv0: uv.map_or([0.0; 2], |coords| coords[index]),
            });
        }
        Ok(PreparedVertexData::Imported(packed))
    } else if let Some(geometry) = mesh.sprite_quad_geometry() {
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(count)
            .map_err(|_| PacketStageError::AllocationFailed)?;
        packed.extend_from_slice(&geometry.vertices[first..end]);
        Ok(PreparedVertexData::Sprite(packed))
    } else {
        let mesh = mesh
            .legacy_mesh()
            .expect("non-imported and non-sprite mesh uses legacy backing");
        let mut packed = Vec::new();
        packed
            .try_reserve_exact(count)
            .map_err(|_| PacketStageError::AllocationFailed)?;
        for vertex in &mesh.vertices[first..end] {
            packed.push(GpuVertex {
                position: vertex.position,
                normal: vertex.normal,
            });
        }
        Ok(PreparedVertexData::Legacy(packed))
    }
}

fn preflight_stage_uploads(
    plan: &ResourceAdmissionPlan,
    limits: PacketStageLimits,
    asset_limits: crate::PacketAssetLimits,
) -> Result<usize, PacketStageError> {
    let mut total = 0usize;
    let budget = StageUploadBudget::full(limits, asset_limits);
    for (_, mesh) in &plan.missing_geometries {
        let (vertex, index) = geometry_buffer_bytes(mesh)?;
        let stride = geometry_vertex_stride(mesh);
        if mesh.vertex_count() > 0 {
            budget.validate_unit(stride, stride, stride)?;
        }
        let index_unit = std::mem::size_of::<u32>();
        if mesh.index_count() > 0 {
            budget.validate_unit(index_unit, index_unit, 0)?;
        }
        total = total
            .checked_add(vertex)
            .and_then(|bytes| bytes.checked_add(index))
            .ok_or(PacketStageError::CounterExhausted)?;
    }
    for (_, upload) in &plan.missing_textures {
        let image = upload.image.as_ref();
        let mut check_level = |width: u32, height: u32| -> Result<(), PacketStageError> {
            let row_bytes = usize::try_from(width)
                .ok()
                .and_then(|value| value.checked_mul(4))
                .ok_or(PacketStageError::CounterExhausted)?;
            let staging_row_bytes = padded_texture_row_bytes(row_bytes)?;
            budget.validate_unit(row_bytes, staging_row_bytes, 0)?;
            let level_bytes = row_bytes
                .checked_mul(
                    usize::try_from(height).map_err(|_| PacketStageError::CounterExhausted)?,
                )
                .ok_or(PacketStageError::CounterExhausted)?;
            total = total
                .checked_add(level_bytes)
                .ok_or(PacketStageError::CounterExhausted)?;
            Ok(())
        };
        check_level(image.decoded().width(), image.decoded().height())?;
        for mip in image.mips() {
            check_level(mip.width(), mip.height())?;
        }
    }
    if plan.default_texture_missing {
        budget.validate_unit(4, 256, 0)?;
        total = total
            .checked_add(4)
            .ok_or(PacketStageError::CounterExhausted)?;
    }
    Ok(total)
}

fn padded_texture_row_bytes(raw_row_bytes: usize) -> Result<usize, PacketStageError> {
    raw_row_bytes
        .checked_add(255)
        .and_then(|value| value.checked_div(256))
        .and_then(|value| value.checked_mul(256))
        .ok_or(PacketStageError::CounterExhausted)
}

fn largest_remaining_staging_unit(record: &StageRecord) -> Result<usize, PacketStageError> {
    let Some(plan) = record.plan.as_ref() else {
        return Ok(0);
    };
    let mut largest = 0usize;
    for (index, (_, mesh)) in plan
        .missing_geometries
        .iter()
        .enumerate()
        .skip(record.cursor.geometry_index)
    {
        let current = index == record.cursor.geometry_index;
        if mesh.vertex_count() > 0 && !(current && record.cursor.geometry_part > 0) {
            largest = largest.max(geometry_vertex_stride(mesh));
        }
        if mesh.index_count() > 0 {
            let (_, index_bytes) = geometry_buffer_bytes(mesh)?;
            let index_finished = current
                && record.cursor.geometry_part > 0
                && record.cursor.geometry_offset >= index_bytes;
            if !index_finished {
                largest = largest.max(std::mem::size_of::<u32>());
            }
        }
    }

    for (texture_index, (_, upload)) in plan
        .missing_textures
        .iter()
        .enumerate()
        .skip(record.cursor.texture_index)
    {
        let image = upload.image.as_ref();
        let first_mip = if texture_index == record.cursor.texture_index {
            record.cursor.mip_index
        } else {
            0
        };
        for mip_index in first_mip..=image.mips().len() {
            let (width, height, _) =
                texture_mip(image, mip_index).ok_or(PacketStageError::CounterExhausted)?;
            let first_row = if texture_index == record.cursor.texture_index
                && mip_index == record.cursor.mip_index
            {
                record.cursor.texture_row
            } else {
                0
            };
            if first_row < height {
                let raw_row = usize::try_from(width)
                    .ok()
                    .and_then(|value| value.checked_mul(4))
                    .ok_or(PacketStageError::CounterExhausted)?;
                largest = largest.max(padded_texture_row_bytes(raw_row)?);
            }
        }
    }

    if plan.default_texture_missing && !record.cursor.default_uploaded {
        largest = largest.max(256);
    }
    Ok(largest)
}

fn validate_overlay_upload_budget(
    record: &StageRecord,
    asset_limits: crate::PacketAssetLimits,
    overlay_present: bool,
) -> Result<(), PacketStageError> {
    if !overlay_present {
        return Ok(());
    }

    let overlay_bytes = std::mem::size_of::<crate::renderer::OverlayUniform>();
    let upload_budget = asset_limits.upload_staging_bytes;
    let available_for_uploads =
        upload_budget
            .checked_sub(overlay_bytes)
            .ok_or(PacketStageError::Render(
                PacketRenderError::AssetByteBudgetExceeded {
                    resource: "upload staging overlay",
                    bytes: overlay_bytes,
                    limit: upload_budget,
                },
            ))?;

    // Each admitted raw unit is at most the per-submit quota. Texture rows add
    // at most 255 bytes of padding, so a rounded quota bound keeps the common
    // pump path O(1). Only unusually tight global budgets need an exact scan of
    // the remaining indivisible units.
    let rounded_raw_quota = record
        .limits
        .upload_bytes_per_submission
        .checked_add(255)
        .and_then(|bytes| bytes.checked_div(256))
        .and_then(|units| units.checked_mul(256))
        .ok_or(PacketStageError::CounterExhausted)?;
    if available_for_uploads >= rounded_raw_quota {
        return Ok(());
    }

    let largest_remaining = largest_remaining_staging_unit(record)?;
    if largest_remaining > available_for_uploads {
        return Err(PacketStageError::UploadUnitTooLarge {
            bytes: largest_remaining,
            limit: available_for_uploads,
        });
    }
    Ok(())
}

fn stage_bookkeeping_estimate(
    input: &PacketStageInput,
    old_geometry_count: usize,
    old_texture_count: usize,
    old_sampler_count: usize,
    old_bind_group_count: usize,
) -> Result<usize, PacketStageError> {
    let bindings = input
        .model_draw_count()
        .checked_add(input.sprite_draw_count())
        .ok_or(PacketStageError::CounterExhausted)?;
    let geometry_union = input
        .geometry_count()
        .checked_add(old_geometry_count)
        .ok_or(PacketStageError::CounterExhausted)?;
    let texture_union = input
        .texture_count()
        .checked_add(old_texture_count)
        .ok_or(PacketStageError::CounterExhausted)?;
    let sampler_union = bindings
        .checked_add(old_sampler_count)
        .ok_or(PacketStageError::CounterExhausted)?;
    let bind_group_union = bindings
        .checked_add(old_bind_group_count)
        .ok_or(PacketStageError::CounterExhausted)?;

    // Each union entry conservatively covers the candidate planner map/vector,
    // protected and expected sets, and (for a missing resource) the staged map.
    let geometry_entry = std::mem::size_of::<GeometryKey>()
        .checked_add(std::mem::size_of::<crate::MeshHandle>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<StagedGeometry>()))
        .and_then(|bytes| bytes.checked_add(4 * (std::mem::size_of::<GeometryKey>() + 64)))
        .ok_or(PacketStageError::CounterExhausted)?;
    let texture_entry = std::mem::size_of::<TextureKey>()
        .checked_add(std::mem::size_of::<Arc<ModelTextureUpload>>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<StagedTexture>()))
        .and_then(|bytes| bytes.checked_add(4 * (std::mem::size_of::<TextureKey>() + 64)))
        .ok_or(PacketStageError::CounterExhausted)?;
    let sampler_entry = std::mem::size_of::<SamplerKey>()
        .checked_add(std::mem::size_of::<SamplerResource>())
        .and_then(|bytes| bytes.checked_add(5 * (std::mem::size_of::<SamplerKey>() + 64)))
        .ok_or(PacketStageError::CounterExhausted)?;
    let bind_group_entry = std::mem::size_of::<TextureSamplerKey>()
        .checked_add(std::mem::size_of::<TextureBindGroupResource>())
        .and_then(|bytes| bytes.checked_add(5 * (std::mem::size_of::<TextureSamplerKey>() + 64)))
        .ok_or(PacketStageError::CounterExhausted)?;
    let geometric = geometry_union
        .checked_mul(geometry_entry)
        .ok_or(PacketStageError::CounterExhausted)?;
    let textures = texture_union
        .checked_mul(texture_entry)
        .ok_or(PacketStageError::CounterExhausted)?;
    let samplers = sampler_union
        .checked_mul(sampler_entry)
        .ok_or(PacketStageError::CounterExhausted)?;
    let bind_groups = bind_group_union
        .checked_mul(bind_group_entry)
        .ok_or(PacketStageError::CounterExhausted)?;
    let tasks = input
        .geometry_count()
        .checked_add(input.texture_count())
        .and_then(|count| count.checked_add(bindings.checked_mul(2)?))
        .and_then(|count| count.checked_add(4))
        .and_then(|count| count.checked_mul(std::mem::size_of::<CreateTask>() + 64))
        .ok_or(PacketStageError::CounterExhausted)?;
    let bounded_diagnostics = MAX_STAGE_DIAGNOSTIC_CODE_BYTES
        .checked_add(MAX_STAGE_DIAGNOSTIC_MESSAGE_BYTES)
        .ok_or(PacketStageError::CounterExhausted)?;
    let status = std::mem::size_of::<StageRecord>()
        .checked_add(std::mem::size_of::<PacketStageStatus>())
        .and_then(|bytes| bytes.checked_add(input.source_digest().len()))
        .and_then(|bytes| bytes.checked_add(bounded_diagnostics))
        .and_then(|bytes| bytes.checked_add(16 * 1024))
        .ok_or(PacketStageError::CounterExhausted)?;
    geometric
        .checked_add(textures)
        .and_then(|bytes| bytes.checked_add(samplers))
        .and_then(|bytes| bytes.checked_add(bind_groups))
        .and_then(|bytes| bytes.checked_add(tasks))
        .and_then(|bytes| bytes.checked_add(status))
        .ok_or(PacketStageError::CounterExhausted)
}

fn stage_plan_fits_estimate(input: &PacketStageInput, plan: &ResourceAdmissionPlan) -> bool {
    let Some(binding_count) = input
        .model_draw_count()
        .checked_add(input.sprite_draw_count())
    else {
        return false;
    };
    plan.requested_geometries.len() <= input.geometry_count()
        && plan.active_textures.len() <= input.texture_count()
        && plan.requested_samplers.len() <= binding_count
        && plan.requested_bind_groups.len() <= binding_count
}

impl PacketRenderer {
    /// Admits one resource-only CPU generation without creating GPU objects or
    /// enqueueing writes. The existing ready generation and the candidate are
    /// protected together by the cache planner.
    pub fn try_begin_stage(
        &mut self,
        input: PacketStageInput,
        limits: PacketStageLimits,
    ) -> Result<PacketStageId, PacketStageError> {
        limits.validate()?;
        if self.stage_state.active.is_some() {
            return Err(PacketStageError::StageBusy);
        }
        if input.packet_limits() != self.limits || input.asset_limits() != self.asset_limits {
            return Err(PacketStageError::FrameLimitsMismatch);
        }
        let stage_bookkeeping_bytes = stage_bookkeeping_estimate(
            &input,
            self.active_keys.len(),
            self.active_textures.len(),
            self.active_samplers.len(),
            self.active_texture_bind_groups.len(),
        )?;
        let reserved_bookkeeping = self
            .bookkeeping_base_bytes
            .checked_add(stage_bookkeeping_bytes)
            .ok_or(PacketStageError::CounterExhausted)?;
        if reserved_bookkeeping > self.asset_limits.bookkeeping_bytes {
            return Err(PacketStageError::BookkeepingBudgetExceeded {
                bytes: reserved_bookkeeping,
                limit: self.asset_limits.bookkeeping_bytes,
            });
        }
        self.poll_retirement()?;

        let mut protected = ProtectedResources::default();
        protected
            .geometries
            .try_reserve(self.active_keys.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .textures
            .try_reserve(self.active_textures.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .samplers
            .try_reserve(self.active_samplers.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .bind_groups
            .try_reserve(self.active_texture_bind_groups.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .geometries
            .extend(self.active_keys.iter().copied());
        protected
            .textures
            .extend(self.active_textures.iter().copied());
        protected
            .samplers
            .extend(self.active_samplers.iter().copied());
        protected
            .bind_groups
            .extend(self.active_texture_bind_groups.iter().copied());

        let mut plan = self.plan_stage_resources(&input, &protected)?;
        if !stage_plan_fits_estimate(&input, &plan) {
            return Err(PacketStageError::CounterExhausted);
        }
        let total_upload_bytes = preflight_stage_uploads(&plan, limits, self.asset_limits)?;
        let device_limit = self.renderer.device.limits().max_buffer_size;
        for (_, mesh) in &plan.missing_geometries {
            let (vertex_bytes, index_bytes) = geometry_buffer_bytes(mesh)?;
            if u64::try_from(vertex_bytes).map_or(true, |bytes| bytes > device_limit)
                || u64::try_from(index_bytes).map_or(true, |bytes| bytes > device_limit)
            {
                return Err(PacketStageError::Render(PacketRenderError::DeviceLimit(
                    "staged geometry buffer exceeds the adapter's max_buffer_size".into(),
                )));
            }
        }
        let requests = input.resource_requests();
        self.renderer
            .packet_pipeline_create_calls(!requests.model_bindings.is_empty(), false)?;
        self.renderer
            .packet_pipeline_create_calls(false, !requests.sprite_bindings.is_empty())?;
        for (_, mesh) in &plan.missing_geometries {
            u32::try_from(mesh.index_count()).map_err(|_| PacketStageError::CounterExhausted)?;
        }

        let expected = expected_resources(&plan)?;
        protected
            .geometries
            .try_reserve(plan.requested_geometries.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .textures
            .try_reserve(plan.active_textures.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .samplers
            .try_reserve(plan.active_samplers.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .bind_groups
            .try_reserve(plan.active_bind_groups.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        protected
            .geometries
            .extend(plan.requested_geometries.keys().copied());
        protected
            .textures
            .extend(plan.active_textures.iter().copied());
        protected
            .samplers
            .extend(plan.active_samplers.iter().copied());
        protected
            .bind_groups
            .extend(plan.active_bind_groups.iter().copied());

        let mut create_tasks = VecDeque::new();
        let task_count = plan
            .missing_geometries
            .len()
            .checked_add(plan.missing_textures.len())
            .and_then(|count| count.checked_add(plan.missing_samplers.len()))
            .and_then(|count| count.checked_add(plan.missing_bind_groups.len()))
            .and_then(|count| count.checked_add(usize::from(plan.default_texture_missing)))
            .and_then(|count| count.checked_add(usize::from(plan.sprite_uv_buffer_missing)))
            .and_then(|count| count.checked_add(usize::from(!requests.model_bindings.is_empty())))
            .and_then(|count| count.checked_add(usize::from(!requests.sprite_bindings.is_empty())))
            .ok_or(PacketStageError::CounterExhausted)?;
        create_tasks
            .try_reserve(task_count)
            .map_err(|_| PacketStageError::AllocationFailed)?;
        if !requests.model_bindings.is_empty() {
            create_tasks.push_back(CreateTask::ModelPipeline);
        }
        if !requests.sprite_bindings.is_empty() {
            create_tasks.push_back(CreateTask::SpritePipeline);
        }
        for (key, _) in &plan.missing_geometries {
            create_tasks.push_back(CreateTask::Geometry(*key));
        }
        for (key, _) in &plan.missing_textures {
            create_tasks.push_back(CreateTask::Texture(*key));
        }
        if plan.default_texture_missing {
            create_tasks.push_back(CreateTask::DefaultTexture);
        }
        if plan.sprite_uv_buffer_missing {
            create_tasks.push_back(CreateTask::SpriteUvBuffer);
        }
        for key in &plan.missing_samplers {
            create_tasks.push_back(CreateTask::Sampler(*key));
        }
        for key in &plan.missing_bind_groups {
            create_tasks.push_back(CreateTask::BindGroup(*key));
        }
        for task in &create_tasks {
            validate_candidate_create_group(limits, self.stage_task_create_calls(*task)?)?;
        }

        let victims = StageVictims {
            geometries: std::mem::take(&mut plan.geometry_victims),
            textures: std::mem::take(&mut plan.texture_victims),
            samplers: std::mem::take(&mut plan.sampler_victims),
            bind_groups: std::mem::take(&mut plan.bind_group_victims),
        };
        let victims_applied = victims.is_empty();

        let mut created = StagedResources::default();
        if !created.try_reserve(&plan) {
            return Err(PacketStageError::AllocationFailed);
        }
        self.geometry
            .try_reserve(plan.missing_geometries.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.textures
            .try_reserve(plan.missing_textures.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.samplers
            .try_reserve(plan.missing_samplers.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.texture_bind_groups
            .try_reserve(plan.missing_bind_groups.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.active_keys
            .try_reserve(plan.requested_geometries.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.active_textures
            .try_reserve(plan.active_textures.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.active_samplers
            .try_reserve(plan.active_samplers.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        self.active_texture_bind_groups
            .try_reserve(plan.active_bind_groups.len())
            .map_err(|_| PacketStageError::AllocationFailed)?;

        let sequence = self.stage_state.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(PacketStageError::CounterExhausted)?;
        let id = PacketStageId {
            renderer_namespace: self.stage_state.renderer_namespace,
            sequence,
        };
        let mut source_digest = String::new();
        source_digest
            .try_reserve_exact(input.source_digest().len())
            .map_err(|_| PacketStageError::AllocationFailed)?;
        source_digest.push_str(input.source_digest());
        let source_digest: Arc<str> = Arc::from(source_digest);
        let source_pin = input.source_pin().clone();
        let slot_assignments = Arc::clone(input.slot_assignments());
        let status = PacketStageStatus {
            id,
            state: PacketStageState::Admitted,
            session: input.session(),
            project_generation: input.project_generation(),
            source_digest,
            total_upload_bytes,
            uploaded_bytes: 0,
            staging_bytes_submitted: 0,
            create_calls_completed: 0,
            initialization_serial: None,
            initialization_complete: false,
            failure: None,
        };
        self.stage_state.next_sequence = next_sequence;
        self.stage_state.last_terminal = None;
        self.stage_state.active = Some(StageRecord {
            id,
            input: Some(input),
            resource_set_token: None,
            slot_assignments,
            _source_pin: source_pin,
            plan: Some(plan),
            pending_victims: (!victims_applied).then_some(victims),
            status,
            created,
            pending_geometry: None,
            pending_texture: None,
            pending_default_texture: None,
            create_tasks,
            pending_upload_serial: None,
            victims_applied,
            limits,
            cursor: UploadCursor::default(),
            total_upload_bytes,
            raw_uploaded_bytes: 0,
            staging_bytes_submitted: 0,
            create_calls_completed: 0,
            promoted: false,
            expected: Some(expected),
            #[cfg(test)]
            panic_after_pair_handle: None,
        });
        self.staging_bookkeeping_bytes = stage_bookkeeping_bytes;
        self.asset_stats.bookkeeping_reserved_bytes = reserved_bookkeeping;
        self.refresh_stage_state();
        Ok(id)
    }

    pub fn try_stage_status(
        &mut self,
        id: PacketStageId,
    ) -> Result<PacketStageStatus, PacketStageError> {
        self.poll_retirement()?;
        self.refresh_stage_state();
        self.stage_state.status(id)
    }

    pub fn try_cancel_stage(
        &mut self,
        id: PacketStageId,
    ) -> Result<PacketStageStatus, PacketStageError> {
        self.poll_retirement()?;
        self.refresh_stage_state();
        if id.renderer_namespace != self.stage_state.renderer_namespace {
            return Err(PacketStageError::InvalidStageId);
        }
        let Some(record) = self
            .stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id)
        else {
            let terminal = self.stage_state.status(id)?;
            return Ok(terminal);
        };
        if matches!(
            record.status.state,
            PacketStageState::Cancelling | PacketStageState::Retiring
        ) {
            return Ok(record.status.clone());
        }
        record.input = None;
        record.plan = None;
        record.pending_victims = None;
        record.expected = None;
        record.create_tasks.clear();
        for texture in record.created.textures.values_mut() {
            texture.upload = None;
        }
        record.status.state = if record
            .pending_upload_serial
            .is_some_and(|serial| serial > self.completion_watermark.load(Ordering::Acquire))
        {
            PacketStageState::Retiring
        } else {
            PacketStageState::Cancelled
        };
        if record.status.state == PacketStageState::Cancelled {
            let status = record.status.clone();
            self.stage_state.active = None;
            self.stage_state.last_terminal = Some(status.clone());
            self.release_stage_bookkeeping();
            return Ok(status);
        }
        Ok(record.status.clone())
    }

    fn prepare_stage_create_batch(
        &self,
        id: PacketStageId,
    ) -> Result<StageCreateBatch, PacketStageError> {
        let record = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?;
        if record
            .pending_upload_serial
            .is_some_and(|serial| serial > self.completion_watermark.load(Ordering::Acquire))
        {
            return Ok(StageCreateBatch::default());
        }
        let mut batch = StageCreateBatch {
            task_count: 0,
            create_calls: 0,
            apply_victims: record.pending_victims.is_some(),
        };
        for task in &record.create_tasks {
            let calls = self.stage_task_create_calls(*task)?;
            let next_calls = batch
                .create_calls
                .checked_add(calls)
                .ok_or(PacketStageError::CounterExhausted)?;
            if next_calls > record.limits.max_resource_creates_per_submission {
                break;
            }
            batch.task_count = batch
                .task_count
                .checked_add(1)
                .ok_or(PacketStageError::CounterExhausted)?;
            batch.create_calls = next_calls;
        }
        Ok(batch)
    }

    pub(super) fn preflight_stage_action(
        &self,
        action: StageSubmitAction,
        candidate_frame: Option<&CpuFrameHandle>,
        packet: Option<&RenderPacket>,
    ) -> Result<(), PacketStageError> {
        match action {
            StageSubmitAction::Create { id, batch } => self.preflight_stage_create_batch(id, batch),
            StageSubmitAction::Promote { id } => {
                let record = self
                    .stage_state
                    .active
                    .as_ref()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?;
                let frame = candidate_frame.ok_or(PacketStageError::FrameIdentityMismatch)?;
                let packet = packet.ok_or(PacketStageError::FrameIdentityMismatch)?;
                if record.status.state != PacketStageState::Ready
                    || record.promoted
                    || record.pending_victims.is_some()
                    || record.expected.is_none()
                    || !same_frame_identity(frame, &record.status)
                    || !same_packet_identity(packet, frame.packet())
                    || !record
                        .resource_set_token
                        .as_ref()
                        .is_some_and(|token| Arc::ptr_eq(token, frame.resource_set_token()))
                    || !frame
                        .slot_assignments()
                        .is_some_and(|slots| Arc::ptr_eq(slots, &record.slot_assignments))
                {
                    return Err(PacketStageError::FrameResourcesUnavailable);
                }
                Ok(())
            }
        }
    }

    pub(super) fn execute_stage_action(
        &mut self,
        action: StageSubmitAction,
    ) -> Result<(), PacketStageError> {
        match action {
            StageSubmitAction::Create { id, batch } => {
                self.create_stage_resources(id, batch)?;
            }
            StageSubmitAction::Promote { id } => {
                self.apply_stage_victims(id)?;
                self.promote_stage_resources(id)?;
            }
        }
        Ok(())
    }

    fn preflight_stage_create_batch(
        &self,
        id: PacketStageId,
        batch: StageCreateBatch,
    ) -> Result<(), PacketStageError> {
        let record = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?;
        if record
            .pending_upload_serial
            .is_some_and(|serial| serial > self.completion_watermark.load(Ordering::Acquire))
            || batch.task_count > record.create_tasks.len()
            || batch.apply_victims != record.pending_victims.is_some()
            || batch.create_calls > record.limits.max_resource_creates_per_submission
        {
            return Err(PacketStageError::CounterExhausted);
        }
        record
            .create_calls_completed
            .checked_add(batch.create_calls)
            .ok_or(PacketStageError::CounterExhausted)?;
        let mut checked_calls = 0usize;
        let mut sampler_creates = 0u64;
        let mut bind_group_creates = 0u64;
        for task in record.create_tasks.iter().take(batch.task_count) {
            let calls = self.stage_task_create_calls(*task)?;
            checked_calls = checked_calls
                .checked_add(calls)
                .ok_or(PacketStageError::CounterExhausted)?;
            match task {
                CreateTask::ModelPipeline => {
                    self.renderer.packet_pipeline_create_calls(true, false)?;
                }
                CreateTask::SpritePipeline => {
                    self.renderer.packet_pipeline_create_calls(false, true)?;
                }
                CreateTask::Geometry(key) => {
                    let mesh = record
                        .plan
                        .as_ref()
                        .and_then(|plan| {
                            plan.missing_geometries
                                .iter()
                                .find(|(candidate, _)| candidate == key)
                                .map(|(_, mesh)| mesh)
                        })
                        .ok_or(PacketStageError::InvalidStageId)?;
                    let (vertex_bytes, index_bytes) = geometry_buffer_bytes(mesh)?;
                    if vertex_bytes == 0
                        || index_bytes == 0
                        || u32::try_from(mesh.index_count()).is_err()
                        || record.created.geometries.contains_key(key)
                        || record.pending_geometry.is_some()
                    {
                        return Err(PacketStageError::CounterExhausted);
                    }
                }
                CreateTask::Texture(key) => {
                    let upload = record
                        .plan
                        .as_ref()
                        .and_then(|plan| {
                            plan.missing_textures
                                .iter()
                                .find(|(candidate, _)| candidate == key)
                                .map(|(_, upload)| upload)
                        })
                        .ok_or(PacketStageError::InvalidStageId)?;
                    if upload.image.decoded().width() == 0
                        || upload.image.decoded().height() == 0
                        || record.created.textures.contains_key(key)
                        || record.pending_texture.is_some()
                    {
                        return Err(PacketStageError::CounterExhausted);
                    }
                }
                CreateTask::DefaultTexture => {
                    if record.created.default_texture.is_some()
                        || record.pending_default_texture.is_some()
                    {
                        return Err(PacketStageError::CounterExhausted);
                    }
                }
                CreateTask::SpriteUvBuffer => {
                    if self.sprite_uv_buffer_bytes == 0 || record.created.sprite_uv_buffer.is_some()
                    {
                        return Err(PacketStageError::CounterExhausted);
                    }
                }
                CreateTask::Sampler(key) => {
                    if record.created.samplers.contains_key(key) {
                        return Err(PacketStageError::CounterExhausted);
                    }
                    sampler_creates = sampler_creates
                        .checked_add(1)
                        .ok_or(PacketStageError::CounterExhausted)?;
                }
                CreateTask::BindGroup(key) => {
                    let selected_before = record.create_tasks.iter().take(batch.task_count);
                    if record.created.bind_groups.contains_key(key)
                        || self.renderer.model_textures_layout().is_none()
                            && !selected_before.clone().any(|prior| {
                                matches!(
                                    prior,
                                    CreateTask::ModelPipeline | CreateTask::SpritePipeline
                                )
                            })
                    {
                        return Err(PacketStageError::InvalidStageId);
                    }
                    let texture_ready = if let Some(texture) = key.texture {
                        record.created.textures.contains_key(&texture)
                            || self.textures.contains_key(&texture)
                            || selected_before.clone().any(|prior| {
                                matches!(prior, CreateTask::Texture(prior_key) if *prior_key == texture)
                            })
                    } else {
                        record.created.default_texture.is_some()
                            || self.default_model_texture.is_some()
                            || selected_before
                                .clone()
                                .any(|prior| matches!(prior, CreateTask::DefaultTexture))
                    };
                    let sampler_ready = record.created.samplers.contains_key(&key.sampler)
                        || self.samplers.contains_key(&key.sampler)
                        || selected_before.clone().any(|prior| {
                            matches!(prior, CreateTask::Sampler(prior_key) if *prior_key == key.sampler)
                        });
                    if !texture_ready || !sampler_ready {
                        return Err(PacketStageError::InvalidStageId);
                    }
                    bind_group_creates = bind_group_creates
                        .checked_add(1)
                        .ok_or(PacketStageError::CounterExhausted)?;
                }
            }
        }
        if checked_calls != batch.create_calls {
            return Err(PacketStageError::CounterExhausted);
        }
        self.asset_stats
            .sampler_create_count
            .checked_add(sampler_creates)
            .ok_or(PacketStageError::CounterExhausted)?;
        self.asset_stats
            .bind_group_create_count
            .checked_add(bind_group_creates)
            .ok_or(PacketStageError::CounterExhausted)?;
        if batch.apply_victims {
            let victims = record
                .pending_victims
                .as_ref()
                .ok_or(PacketStageError::InvalidStageId)?;
            self.stats
                .cache_eviction_count
                .checked_add(
                    victims
                        .geometries
                        .iter()
                        .filter(|key| self.geometry.contains_key(key))
                        .count() as u64,
                )
                .ok_or(PacketStageError::CounterExhausted)?;
        }
        Ok(())
    }

    fn create_stage_resources(
        &mut self,
        id: PacketStageId,
        batch: StageCreateBatch,
    ) -> Result<usize, PacketStageError> {
        self.preflight_stage_create_batch(id, batch)?;
        if batch.apply_victims {
            self.apply_stage_victims(id)?;
        }
        let mut calls_this_pump = 0usize;
        for _ in 0..batch.task_count {
            let task = self
                .stage_state
                .active
                .as_ref()
                .filter(|record| record.id == id)
                .and_then(|record| record.create_tasks.front().copied())
                .ok_or(PacketStageError::InvalidStageId)?;
            let calls = self.stage_task_create_calls(task)?;
            let next_create_calls = self
                .stage_state
                .active
                .as_ref()
                .filter(|record| record.id == id)
                .ok_or(PacketStageError::InvalidStageId)?
                .create_calls_completed
                .checked_add(calls)
                .ok_or(PacketStageError::CounterExhausted)?;
            self.create_stage_task(id, task)?;
            let record = self
                .stage_state
                .active
                .as_mut()
                .filter(|record| record.id == id)
                .ok_or(PacketStageError::InvalidStageId)?;
            record.create_tasks.pop_front();
            calls_this_pump = calls_this_pump
                .checked_add(calls)
                .ok_or(PacketStageError::CounterExhausted)?;
            record.create_calls_completed = next_create_calls;
            record.status.create_calls_completed = record.create_calls_completed;
        }
        if calls_this_pump != batch.create_calls {
            return Err(PacketStageError::CounterExhausted);
        }
        Ok(calls_this_pump)
    }

    fn stage_task_create_calls(&self, task: CreateTask) -> Result<usize, PacketStageError> {
        let calls = match task {
            CreateTask::ModelPipeline => self.renderer.packet_pipeline_create_calls(true, false)?,
            CreateTask::SpritePipeline => {
                self.renderer.packet_pipeline_create_calls(false, true)?
            }
            CreateTask::Geometry(_) | CreateTask::Texture(_) | CreateTask::DefaultTexture => 2,
            CreateTask::SpriteUvBuffer | CreateTask::Sampler(_) | CreateTask::BindGroup(_) => 1,
        };
        Ok(calls)
    }

    fn create_stage_task(
        &mut self,
        id: PacketStageId,
        task: CreateTask,
    ) -> Result<(), PacketStageError> {
        match task {
            CreateTask::ModelPipeline => self.renderer.ensure_model_packet_pipeline()?,
            CreateTask::SpritePipeline => self.renderer.ensure_sprite_packet_pipeline()?,
            CreateTask::Geometry(key) => {
                let mesh = self
                    .stage_state
                    .active
                    .as_ref()
                    .and_then(|record| record.plan.as_ref())
                    .and_then(|plan| {
                        plan.missing_geometries
                            .iter()
                            .find(|(candidate, _)| *candidate == key)
                            .map(|(_, mesh)| mesh.clone())
                    })
                    .ok_or(PacketStageError::InvalidStageId)?;
                let (vertex_bytes, index_bytes) = geometry_buffer_bytes(&mesh)?;
                let index_count = u32::try_from(mesh.index_count())
                    .map_err(|_| PacketStageError::CounterExhausted)?;
                let mesh_bytes = mesh.gpu_bytes();
                let last_touch = self.touch_serial.saturating_add(1);
                let record = self
                    .stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?;
                if record.pending_geometry.is_some() || record.created.geometries.contains_key(&key)
                {
                    return Err(PacketStageError::InvalidStageId);
                }
                let vertex = self.renderer.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Daedalus staged packet geometry vertices"),
                    size: vertex_bytes as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                record.pending_geometry = Some(PendingGeometry {
                    key,
                    vertex,
                    index: None,
                    vertex_bytes,
                    index_bytes,
                    index_count,
                    mesh_bytes,
                    last_touch,
                });
                #[cfg(test)]
                record.panic_after_pair_handle_for_test(StagePanicPoint::GeometryAfterVertex);
                let index = self.renderer.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Daedalus staged packet geometry indices"),
                    size: index_bytes as u64,
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let pending = record
                    .pending_geometry
                    .as_mut()
                    .expect("vertex handle retained before index creation");
                pending.index = Some(index);
                let pending = record
                    .pending_geometry
                    .take()
                    .expect("complete geometry pair retained");
                record.created.geometries.insert(
                    pending.key,
                    StagedGeometry {
                        resource: GeometryResource {
                            vertex: pending.vertex,
                            index: pending.index.expect("geometry pair complete"),
                            index_count: pending.index_count,
                            bytes: pending.mesh_bytes,
                            initialization_serial: 0,
                            last_submission: 0,
                            last_touch: pending.last_touch,
                        },
                        vertex_bytes: pending.vertex_bytes,
                        index_bytes: pending.index_bytes,
                        vertex_uploaded_bytes: 0,
                        index_uploaded_bytes: 0,
                        initialized_serial: None,
                    },
                );
            }
            CreateTask::Texture(key) => {
                let upload = self
                    .stage_state
                    .active
                    .as_ref()
                    .and_then(|record| record.plan.as_ref())
                    .and_then(|plan| {
                        plan.missing_textures
                            .iter()
                            .find(|(candidate, _)| *candidate == key)
                            .map(|(_, upload)| Arc::clone(upload))
                    })
                    .ok_or(PacketStageError::InvalidStageId)?;
                let image = upload.image.as_ref();
                let mip_level_count = u32::try_from(image.mips().len())
                    .ok()
                    .and_then(|count| count.checked_add(1))
                    .ok_or(PacketStageError::CounterExhausted)?;
                let last_touch = self.touch_serial.saturating_add(1);
                let record = self
                    .stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?;
                if record.pending_texture.is_some() || record.created.textures.contains_key(&key) {
                    return Err(PacketStageError::InvalidStageId);
                }
                let texture = self
                    .renderer
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("Daedalus staged imported base-color texture"),
                        size: wgpu::Extent3d {
                            width: image.decoded().width(),
                            height: image.decoded().height(),
                            depth_or_array_layers: 1,
                        },
                        mip_level_count,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                record.pending_texture = Some(PendingTexture {
                    key,
                    texture,
                    view: None,
                    bytes: upload.byte_len,
                    upload,
                    last_touch,
                });
                #[cfg(test)]
                record
                    .panic_after_pair_handle_for_test(StagePanicPoint::ImportedTextureAfterTexture);
                let pending = record
                    .pending_texture
                    .as_ref()
                    .expect("texture handle retained before view creation");
                let view = pending
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                record
                    .pending_texture
                    .as_mut()
                    .expect("pending texture retained")
                    .view = Some(view);
                let pending = record
                    .pending_texture
                    .take()
                    .expect("complete texture pair retained");
                record.created.textures.insert(
                    pending.key,
                    StagedTexture {
                        resource: TextureResource {
                            _texture: pending.texture,
                            view: pending.view.expect("texture pair complete"),
                            bytes: pending.bytes,
                            initialization_serial: 0,
                            last_submission: 0,
                            last_touch: pending.last_touch,
                        },
                        upload: Some(pending.upload),
                        uploaded_bytes: 0,
                        expected_bytes: pending.bytes,
                        initialized_serial: None,
                    },
                );
            }
            CreateTask::DefaultTexture => {
                let last_touch = self.touch_serial.saturating_add(1);
                let record = self
                    .stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?;
                if record.pending_default_texture.is_some()
                    || record.created.default_texture.is_some()
                {
                    return Err(PacketStageError::InvalidStageId);
                }
                let texture = self
                    .renderer
                    .device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("Daedalus staged shared white texture"),
                        size: wgpu::Extent3d {
                            width: 1,
                            height: 1,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                record.pending_default_texture = Some(PendingDefaultTexture {
                    texture,
                    view: None,
                    last_touch,
                });
                #[cfg(test)]
                record
                    .panic_after_pair_handle_for_test(StagePanicPoint::DefaultTextureAfterTexture);
                let pending = record
                    .pending_default_texture
                    .as_ref()
                    .expect("default texture retained before view creation");
                let view = pending
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                record
                    .pending_default_texture
                    .as_mut()
                    .expect("pending default texture retained")
                    .view = Some(view);
                let pending = record
                    .pending_default_texture
                    .take()
                    .expect("complete default texture pair retained");
                record.created.default_texture = Some(TextureResource {
                    _texture: pending.texture,
                    view: pending.view.expect("default texture pair complete"),
                    bytes: 4,
                    initialization_serial: 0,
                    last_submission: 0,
                    last_touch: pending.last_touch,
                });
            }
            CreateTask::SpriteUvBuffer => {
                let buffer = self.renderer.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("Daedalus staged persistent sprite UV rectangles"),
                    size: self.sprite_uv_buffer_bytes as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?
                    .created
                    .sprite_uv_buffer = Some(buffer);
            }
            CreateTask::Sampler(key) => {
                let sampler = self
                    .renderer
                    .device
                    .create_sampler(&super::sampler_descriptor(key));
                self.stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?
                    .created
                    .samplers
                    .insert(
                        key,
                        SamplerResource {
                            sampler,
                            last_submission: 0,
                            last_touch: self.touch_serial.saturating_add(1),
                        },
                    );
                self.asset_stats.sampler_create_count =
                    self.asset_stats.sampler_create_count.saturating_add(1);
            }
            CreateTask::BindGroup(key) => {
                let texture_layout = self
                    .renderer
                    .model_textures_layout()
                    .ok_or(PacketStageError::InvalidStageId)?;
                let (texture_view, sampler) = {
                    let record = self
                        .stage_state
                        .active
                        .as_ref()
                        .filter(|record| record.id == id)
                        .ok_or(PacketStageError::InvalidStageId)?;
                    let texture = key.texture.map_or_else(
                        || {
                            record
                                .created
                                .default_texture
                                .as_ref()
                                .or(self.default_model_texture.as_ref())
                        },
                        |texture_key| {
                            record
                                .created
                                .textures
                                .get(&texture_key)
                                .map(|entry| &entry.resource)
                                .or_else(|| self.textures.get(&texture_key))
                        },
                    );
                    let sampler = record
                        .created
                        .samplers
                        .get(&key.sampler)
                        .or_else(|| self.samplers.get(&key.sampler));
                    (
                        texture.ok_or(PacketStageError::InvalidStageId)?,
                        sampler.ok_or(PacketStageError::InvalidStageId)?,
                    )
                };
                let bind_group =
                    self.renderer
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("Daedalus staged cached texture sampler pair"),
                            layout: texture_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(
                                        &texture_view.view,
                                    ),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&sampler.sampler),
                                },
                            ],
                        });
                self.stage_state
                    .active
                    .as_mut()
                    .filter(|record| record.id == id)
                    .ok_or(PacketStageError::InvalidStageId)?
                    .created
                    .bind_groups
                    .insert(
                        key,
                        TextureBindGroupResource {
                            bind_group,
                            last_submission: 0,
                            last_touch: self.touch_serial.saturating_add(1),
                        },
                    );
                self.asset_stats.bind_group_create_count =
                    self.asset_stats.bind_group_create_count.saturating_add(1);
            }
        }
        Ok(())
    }

    fn prepare_stage_upload(
        &self,
        id: PacketStageId,
        budget: StageUploadBudget,
    ) -> Result<Option<PreparedUploadChunk>, PacketStageError> {
        let record = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?;
        if !record.create_tasks.is_empty()
            || record
                .pending_upload_serial
                .is_some_and(|serial| serial > self.completion_watermark.load(Ordering::Acquire))
        {
            return Ok(None);
        }
        let plan = record
            .plan
            .as_ref()
            .ok_or(PacketStageError::InvalidStageId)?;
        let limits = record.limits;
        let mut next_cursor = record.cursor;

        if next_cursor.geometry_index < plan.missing_geometries.len() {
            let (key, mesh) = &plan.missing_geometries[next_cursor.geometry_index];
            let (vertex_bytes, index_bytes) = geometry_buffer_bytes(mesh)?;
            if next_cursor.geometry_part == 0 {
                let stride = geometry_vertex_stride(mesh);
                let first_vertex = next_cursor.geometry_offset / stride;
                let Some(max_records) = budget.max_units(stride, stride, stride)? else {
                    return Ok(None);
                };
                let vertex_count = mesh.vertex_count();
                if first_vertex >= vertex_count {
                    return Err(PacketStageError::CounterExhausted);
                }
                let record_count = (vertex_count - first_vertex).min(max_records);
                if record_count == 0 {
                    return Err(PacketStageError::UploadUnitTooLarge {
                        bytes: stride,
                        limit: limits.upload_bytes_per_submission,
                    });
                }
                let data = pack_vertex_range(mesh, first_vertex, record_count)?;
                let raw_bytes = data.bytes().len();
                let cpu_scratch_bytes = data.scratch_bytes()?;
                if cpu_scratch_bytes > budget.scratch_bytes {
                    return Err(PacketStageError::Render(
                        PacketRenderError::AssetByteBudgetExceeded {
                            resource: "CPU upload scratch",
                            bytes: cpu_scratch_bytes,
                            limit: budget.scratch_bytes,
                        },
                    ));
                }
                let byte_offset = next_cursor.geometry_offset;
                let next_offset = byte_offset
                    .checked_add(raw_bytes)
                    .ok_or(PacketStageError::CounterExhausted)?;
                if next_offset > vertex_bytes || raw_bytes > limits.upload_bytes_per_submission {
                    return Err(PacketStageError::CounterExhausted);
                }
                if next_offset == vertex_bytes {
                    next_cursor.geometry_part = 1;
                    next_cursor.geometry_offset = 0;
                } else {
                    next_cursor.geometry_offset = next_offset;
                }
                Ok(Some(PreparedUploadChunk {
                    upload: PreparedUpload::GeometryVertex {
                        key: *key,
                        data,
                        byte_offset,
                    },
                    next_cursor,
                    raw_bytes,
                    staging_bytes: raw_bytes,
                    cpu_scratch_bytes,
                }))
            } else {
                let first_index = next_cursor.geometry_offset / std::mem::size_of::<u32>();
                let index_unit = std::mem::size_of::<u32>();
                let Some(max_indices) = budget.max_units(index_unit, index_unit, 0)? else {
                    return Ok(None);
                };
                let index_count = (mesh.index_count() - first_index).min(max_indices);
                if index_count == 0 {
                    return Err(PacketStageError::UploadUnitTooLarge {
                        bytes: std::mem::size_of::<u32>(),
                        limit: limits.upload_bytes_per_submission,
                    });
                }
                let byte_offset = next_cursor.geometry_offset;
                let raw_bytes = index_count
                    .checked_mul(std::mem::size_of::<u32>())
                    .ok_or(PacketStageError::CounterExhausted)?;
                let next_offset = byte_offset
                    .checked_add(raw_bytes)
                    .ok_or(PacketStageError::CounterExhausted)?;
                if next_offset > index_bytes {
                    return Err(PacketStageError::CounterExhausted);
                }
                if next_offset == index_bytes {
                    next_cursor.geometry_index += 1;
                    next_cursor.geometry_part = 0;
                    next_cursor.geometry_offset = 0;
                } else {
                    next_cursor.geometry_offset = next_offset;
                }
                Ok(Some(PreparedUploadChunk {
                    upload: PreparedUpload::GeometryIndex {
                        key: *key,
                        byte_offset,
                        first_index,
                        index_count,
                    },
                    next_cursor,
                    raw_bytes,
                    staging_bytes: raw_bytes,
                    cpu_scratch_bytes: 0,
                }))
            }
        } else {
            while next_cursor.texture_index < plan.missing_textures.len() {
                let (_, upload) = &plan.missing_textures[next_cursor.texture_index];
                let image = upload.image.as_ref();
                if next_cursor.mip_index > image.mips().len() {
                    next_cursor.texture_index += 1;
                    next_cursor.mip_index = 0;
                    next_cursor.texture_row = 0;
                    continue;
                }
                let (_, height, _) = texture_mip(image, next_cursor.mip_index)
                    .ok_or(PacketStageError::InvalidStageId)?;
                if next_cursor.texture_row < height {
                    break;
                }
                next_cursor.mip_index += 1;
                next_cursor.texture_row = 0;
            }
            if next_cursor.texture_index < plan.missing_textures.len() {
                let (key, upload) = &plan.missing_textures[next_cursor.texture_index];
                let image = upload.image.as_ref();
                let (width, height, rgba8) = texture_mip(image, next_cursor.mip_index)
                    .ok_or(PacketStageError::InvalidStageId)?;
                let mip_index = next_cursor.mip_index;
                let row_start = next_cursor.texture_row;
                let row_bytes = usize::try_from(width)
                    .ok()
                    .and_then(|value| value.checked_mul(4))
                    .ok_or(PacketStageError::CounterExhausted)?;
                let remaining_rows = height.saturating_sub(next_cursor.texture_row);
                debug_assert!(remaining_rows > 0);
                let padded_row_bytes = padded_texture_row_bytes(row_bytes)?;
                let Some(rows_per_chunk) = budget.max_units(row_bytes, padded_row_bytes, 0)? else {
                    return Ok(None);
                };
                let row_count =
                    remaining_rows.min(u32::try_from(rows_per_chunk).unwrap_or(u32::MAX));
                let raw_bytes = row_bytes
                    .checked_mul(
                        usize::try_from(row_count)
                            .map_err(|_| PacketStageError::CounterExhausted)?,
                    )
                    .ok_or(PacketStageError::CounterExhausted)?;
                let staging_bytes = padded_row_bytes
                    .checked_mul(
                        usize::try_from(row_count)
                            .map_err(|_| PacketStageError::CounterExhausted)?,
                    )
                    .ok_or(PacketStageError::CounterExhausted)?;
                let row_end = next_cursor
                    .texture_row
                    .checked_add(row_count)
                    .ok_or(PacketStageError::CounterExhausted)?;
                if row_end == height {
                    next_cursor.mip_index += 1;
                    next_cursor.texture_row = 0;
                    if next_cursor.mip_index > image.mips().len() {
                        next_cursor.texture_index += 1;
                        next_cursor.mip_index = 0;
                    }
                } else {
                    next_cursor.texture_row = row_end;
                }
                let data_start = usize::try_from(row_start)
                    .ok()
                    .and_then(|row| row.checked_mul(row_bytes))
                    .ok_or(PacketStageError::CounterExhausted)?;
                let data_end = data_start
                    .checked_add(raw_bytes)
                    .ok_or(PacketStageError::CounterExhausted)?;
                if data_end > rgba8.len() {
                    return Err(PacketStageError::CounterExhausted);
                }
                Ok(Some(PreparedUploadChunk {
                    upload: PreparedUpload::TextureRows {
                        key: *key,
                        mip_level: u32::try_from(mip_index)
                            .map_err(|_| PacketStageError::CounterExhausted)?,
                        row_start,
                        row_count,
                        width,
                        upload: Arc::clone(upload),
                        data_start,
                        data_len: raw_bytes,
                        bytes_per_row: u32::try_from(row_bytes)
                            .map_err(|_| PacketStageError::CounterExhausted)?,
                    },
                    next_cursor,
                    raw_bytes,
                    staging_bytes,
                    cpu_scratch_bytes: 0,
                }))
            } else if plan.default_texture_missing && !next_cursor.default_uploaded {
                if budget.max_units(4, 256, 0)?.is_none() {
                    return Ok(None);
                }
                next_cursor.default_uploaded = true;
                Ok(Some(PreparedUploadChunk {
                    upload: PreparedUpload::DefaultTexture,
                    next_cursor,
                    raw_bytes: 4,
                    staging_bytes: 256,
                    cpu_scratch_bytes: 0,
                }))
            } else {
                Ok(None)
            }
        }
    }

    fn stage_upload_due(&self, id: PacketStageId) -> Result<bool, PacketStageError> {
        let record = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?;
        if matches!(
            record.status.state,
            PacketStageState::Ready
                | PacketStageState::Cancelling
                | PacketStageState::Retiring
                | PacketStageState::Cancelled
                | PacketStageState::Failed
        ) || !record.create_tasks.is_empty()
            || record
                .pending_upload_serial
                .is_some_and(|serial| serial > self.completion_watermark.load(Ordering::Acquire))
        {
            return Ok(false);
        }
        Ok(record.raw_uploaded_bytes < record.total_upload_bytes)
    }

    pub(super) fn write_prepared_upload(&mut self, id: PacketStageId, chunk: &PreparedUploadChunk) {
        match &chunk.upload {
            PreparedUpload::GeometryVertex {
                key,
                data,
                byte_offset,
            } => {
                let buffer = &self
                    .stage_state
                    .active
                    .as_ref()
                    .expect("stage preflight preserved record")
                    .created
                    .geometries[key]
                    .resource
                    .vertex;
                self.renderer
                    .queue
                    .write_buffer(buffer, *byte_offset as u64, data.bytes());
            }
            PreparedUpload::GeometryIndex {
                key,
                byte_offset,
                first_index,
                index_count,
            } => {
                let record = self.stage_state.active.as_ref().expect("stage exists");
                let mesh = record
                    .plan
                    .as_ref()
                    .expect("stage plan retained until completion")
                    .missing_geometries
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .map(|(_, mesh)| mesh)
                    .expect("prepared index resource exists");
                let buffer = &record.created.geometries[key].resource.index;
                self.renderer.queue.write_buffer(
                    buffer,
                    *byte_offset as u64,
                    bytemuck::cast_slice(
                        &mesh.indices()[*first_index..*first_index + *index_count],
                    ),
                );
            }
            PreparedUpload::TextureRows {
                key,
                mip_level,
                row_start,
                row_count,
                width,
                upload,
                data_start,
                data_len,
                bytes_per_row,
            } => {
                let image = upload.image.as_ref();
                let (_, _, rgba8) =
                    texture_mip(image, *mip_level as usize).expect("texture mip was preflighted");
                let end = data_start
                    .checked_add(*data_len)
                    .expect("texture upload range was checked during preparation");
                let data = &rgba8[*data_start..end];
                let texture = &self
                    .stage_state
                    .active
                    .as_ref()
                    .expect("stage exists")
                    .created
                    .textures[key]
                    .resource
                    ._texture;
                self.renderer.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: *mip_level,
                        origin: wgpu::Origin3d {
                            x: 0,
                            y: *row_start,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(*bytes_per_row),
                        rows_per_image: Some(*row_count),
                    },
                    wgpu::Extent3d {
                        width: *width,
                        height: *row_count,
                        depth_or_array_layers: 1,
                    },
                );
            }
            PreparedUpload::DefaultTexture => {
                let texture = &self
                    .stage_state
                    .active
                    .as_ref()
                    .expect("stage exists")
                    .created
                    .default_texture
                    .as_ref()
                    .expect("default texture was staged")
                    ._texture;
                self.renderer.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &[255, 255, 255, 255],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(4),
                        rows_per_image: Some(1),
                    },
                    wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        let _ = id;
    }

    pub(super) fn preflight_stage_submission(
        &self,
        id: PacketStageId,
        chunk: &PreparedUploadChunk,
    ) -> Result<(), PacketStageError> {
        let record = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?;
        if record.pending_upload_serial.is_some() || chunk.raw_bytes == 0 {
            return Err(PacketStageError::CounterExhausted);
        }
        let uploaded = record
            .raw_uploaded_bytes
            .checked_add(chunk.raw_bytes)
            .ok_or(PacketStageError::CounterExhausted)?;
        let staging = record
            .staging_bytes_submitted
            .checked_add(chunk.staging_bytes)
            .ok_or(PacketStageError::CounterExhausted)?;
        if uploaded > record.total_upload_bytes || staging < chunk.staging_bytes {
            return Err(PacketStageError::CounterExhausted);
        }
        self.asset_stats
            .upload_staging_write_count
            .checked_add(1)
            .ok_or(PacketStageError::CounterExhausted)?;
        self.asset_stats
            .upload_staging_write_bytes
            .checked_add(
                u64::try_from(chunk.raw_bytes).map_err(|_| PacketStageError::CounterExhausted)?,
            )
            .ok_or(PacketStageError::CounterExhausted)?;
        match &chunk.upload {
            PreparedUpload::GeometryVertex {
                key,
                data,
                byte_offset,
            } => {
                let staged = record
                    .created
                    .geometries
                    .get(key)
                    .ok_or(PacketStageError::InvalidStageId)?;
                if *byte_offset != staged.vertex_uploaded_bytes
                    || data.bytes().is_empty()
                    || data.bytes().len()
                        % geometry_vertex_stride(
                            record
                                .plan
                                .as_ref()
                                .ok_or(PacketStageError::InvalidStageId)?
                                .missing_geometries
                                .iter()
                                .find(|(candidate, _)| candidate == key)
                                .map(|(_, mesh)| mesh)
                                .ok_or(PacketStageError::InvalidStageId)?,
                        )
                        != 0
                    || staged
                        .vertex_uploaded_bytes
                        .checked_add(data.bytes().len())
                        .is_none_or(|end| end > staged.vertex_bytes)
                {
                    return Err(PacketStageError::CounterExhausted);
                }
                let completes_geometry = staged.vertex_uploaded_bytes + data.bytes().len()
                    == staged.vertex_bytes
                    && staged.index_uploaded_bytes == staged.index_bytes
                    && staged.initialized_serial.is_none();
                if completes_geometry {
                    self.stats
                        .geometry_upload_count
                        .checked_add(1)
                        .ok_or(PacketStageError::CounterExhausted)?;
                    self.stats
                        .geometry_upload_bytes
                        .checked_add(
                            u64::try_from(staged.resource.bytes)
                                .map_err(|_| PacketStageError::CounterExhausted)?,
                        )
                        .ok_or(PacketStageError::CounterExhausted)?;
                }
            }
            PreparedUpload::GeometryIndex {
                key,
                byte_offset,
                first_index,
                index_count,
            } => {
                let staged = record
                    .created
                    .geometries
                    .get(key)
                    .ok_or(PacketStageError::InvalidStageId)?;
                let indexed_bytes = index_count
                    .checked_mul(std::mem::size_of::<u32>())
                    .ok_or(PacketStageError::CounterExhausted)?;
                let end_index = first_index
                    .checked_add(*index_count)
                    .ok_or(PacketStageError::CounterExhausted)?;
                let mesh_index_count = record
                    .plan
                    .as_ref()
                    .ok_or(PacketStageError::InvalidStageId)?
                    .missing_geometries
                    .iter()
                    .find(|(candidate, _)| candidate == key)
                    .map(|(_, mesh)| mesh.index_count())
                    .ok_or(PacketStageError::InvalidStageId)?;
                if end_index > mesh_index_count
                    || *byte_offset != staged.index_uploaded_bytes
                    || first_index.checked_mul(std::mem::size_of::<u32>()) != Some(*byte_offset)
                    || chunk.raw_bytes != indexed_bytes
                    || staged
                        .index_uploaded_bytes
                        .checked_add(chunk.raw_bytes)
                        .is_none_or(|end| end > staged.index_bytes)
                {
                    return Err(PacketStageError::CounterExhausted);
                }
                let completes_geometry = staged.index_uploaded_bytes + chunk.raw_bytes
                    == staged.index_bytes
                    && staged.vertex_uploaded_bytes == staged.vertex_bytes
                    && staged.initialized_serial.is_none();
                if completes_geometry {
                    self.stats
                        .geometry_upload_count
                        .checked_add(1)
                        .ok_or(PacketStageError::CounterExhausted)?;
                    self.stats
                        .geometry_upload_bytes
                        .checked_add(
                            u64::try_from(staged.resource.bytes)
                                .map_err(|_| PacketStageError::CounterExhausted)?,
                        )
                        .ok_or(PacketStageError::CounterExhausted)?;
                }
            }
            PreparedUpload::TextureRows { key, .. } => {
                let staged = record
                    .created
                    .textures
                    .get(key)
                    .ok_or(PacketStageError::InvalidStageId)?;
                let next_uploaded = staged
                    .uploaded_bytes
                    .checked_add(chunk.raw_bytes)
                    .ok_or(PacketStageError::CounterExhausted)?;
                if next_uploaded > staged.expected_bytes {
                    return Err(PacketStageError::CounterExhausted);
                }
                self.asset_stats
                    .texture_upload_bytes
                    .checked_add(
                        u64::try_from(chunk.raw_bytes)
                            .map_err(|_| PacketStageError::CounterExhausted)?,
                    )
                    .ok_or(PacketStageError::CounterExhausted)?;
                if next_uploaded == staged.expected_bytes {
                    self.asset_stats
                        .texture_upload_count
                        .checked_add(1)
                        .ok_or(PacketStageError::CounterExhausted)?;
                }
            }
            PreparedUpload::DefaultTexture => {
                if record.created.default_texture.is_none() || chunk.raw_bytes != 4 {
                    return Err(PacketStageError::CounterExhausted);
                }
            }
        }
        Ok(())
    }

    pub(super) fn record_stage_submission(
        &mut self,
        id: PacketStageId,
        chunk: PreparedUploadChunk,
        serial: u64,
    ) {
        let record = self
            .stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id)
            .expect("stage is unchanged after its submission preflight");
        record.cursor = chunk.next_cursor;
        record.raw_uploaded_bytes += chunk.raw_bytes;
        record.staging_bytes_submitted += chunk.staging_bytes;
        record.pending_upload_serial = Some(serial);
        record.status.initialization_serial = Some(serial);
        record.status.uploaded_bytes = record.raw_uploaded_bytes;
        record.status.staging_bytes_submitted = record.staging_bytes_submitted;
        record.status.state = PacketStageState::AwaitingCompletion;
        self.cpu_scratch_current_bytes = 0;
        self.asset_stats.cpu_upload_scratch_current_bytes = 0;
        self.asset_stats.upload_staging_write_count += 1;
        self.asset_stats.upload_staging_write_bytes += chunk.raw_bytes as u64;

        match chunk.upload {
            PreparedUpload::GeometryVertex { key, data, .. } => {
                let staged = record
                    .created
                    .geometries
                    .get_mut(&key)
                    .expect("stage geometry was preflighted");
                staged.vertex_uploaded_bytes += data.bytes().len();
                if staged.vertex_uploaded_bytes == staged.vertex_bytes
                    && staged.index_uploaded_bytes == staged.index_bytes
                    && staged.initialized_serial.is_none()
                {
                    staged.initialized_serial = Some(serial);
                    staged.resource.initialization_serial = serial;
                    self.stats.geometry_upload_count += 1;
                    self.stats.geometry_upload_bytes += staged.resource.bytes as u64;
                }
            }
            PreparedUpload::GeometryIndex { key, .. } => {
                let staged = record
                    .created
                    .geometries
                    .get_mut(&key)
                    .expect("stage geometry was preflighted");
                staged.index_uploaded_bytes += chunk.raw_bytes;
                if staged.index_uploaded_bytes == staged.index_bytes
                    && staged.vertex_uploaded_bytes == staged.vertex_bytes
                    && staged.initialized_serial.is_none()
                {
                    staged.initialized_serial = Some(serial);
                    staged.resource.initialization_serial = serial;
                    self.stats.geometry_upload_count += 1;
                    self.stats.geometry_upload_bytes += staged.resource.bytes as u64;
                }
            }
            PreparedUpload::TextureRows { key, .. } => {
                let staged = record
                    .created
                    .textures
                    .get_mut(&key)
                    .expect("stage texture was preflighted");
                // The checked increment was reserved by
                // preflight_stage_submission before the queue write.
                staged.uploaded_bytes += chunk.raw_bytes;
                self.asset_stats.texture_upload_bytes += chunk.raw_bytes as u64;
                if staged.uploaded_bytes == staged.expected_bytes {
                    staged.initialized_serial = Some(serial);
                    staged.resource.initialization_serial = serial;
                    self.asset_stats.texture_upload_count += 1;
                }
            }
            PreparedUpload::DefaultTexture => {
                if let Some(texture) = record.created.default_texture.as_mut() {
                    texture.initialization_serial = serial;
                }
            }
        }
    }

    fn refresh_stage_state(&mut self) {
        let completed = self.completion_watermark.load(Ordering::Acquire);
        let Some(mut record) = self.stage_state.active.take() else {
            return;
        };
        let uploads_complete = record.pending_victims.is_none()
            && record.create_tasks.is_empty()
            && record
                .plan
                .as_ref()
                .is_some_and(|plan| cursor_complete(&record, plan));
        let readiness_can_change = record.status.state != PacketStageState::Ready
            && uploads_complete
            && record
                .pending_upload_serial
                .is_none_or(|serial| serial <= completed);
        let initialization = if readiness_can_change {
            expected_resources_initialization(self, &record, completed)
        } else {
            ExpectedResourceInitialization::default()
        };
        if refresh_stage_record(&mut record, completed, uploads_complete, initialization) {
            self.stage_state.last_terminal = Some(record.status);
            self.release_stage_bookkeeping();
            return;
        }
        self.stage_state.active = Some(record);
    }

    fn promote_stage_resources(&mut self, id: PacketStageId) -> Result<(), PacketStageError> {
        let Some(record) = self
            .stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id)
        else {
            return Err(PacketStageError::InvalidStageId);
        };
        if record.status.state != PacketStageState::Ready {
            return Err(PacketStageError::FrameResourcesUnavailable);
        }
        if record.promoted {
            return Ok(());
        }
        if record.created.sprite_uv_buffer.is_some() {
            self.fixed_resident_bytes = self
                .fixed_resident_bytes
                .checked_add(self.sprite_uv_buffer_bytes)
                .ok_or(PacketStageError::CounterExhausted)?;
            self.sprite_uv_buffer = record.created.sprite_uv_buffer.take();
        }
        for (key, entry) in record.created.geometries.drain() {
            self.geometry.insert(key, entry.resource);
        }
        for (key, entry) in record.created.textures.drain() {
            self.textures.insert(key, entry.resource);
        }
        for (key, resource) in record.created.samplers.drain() {
            self.samplers.insert(key, resource);
        }
        for (key, resource) in record.created.bind_groups.drain() {
            self.texture_bind_groups.insert(key, resource);
        }
        if let Some(texture) = record.created.default_texture.take() {
            self.default_model_texture = Some(texture);
        }
        record.promoted = true;
        Ok(())
    }

    fn apply_stage_victims(&mut self, id: PacketStageId) -> Result<(), PacketStageError> {
        let Some(victims) = self
            .stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id)
            .and_then(|record| record.pending_victims.take())
        else {
            return Ok(());
        };
        for key in victims.bind_groups {
            self.texture_bind_groups.remove(&key);
        }
        for key in victims.samplers {
            self.samplers.remove(&key);
        }
        for key in victims.textures {
            self.textures.remove(&key);
        }
        for key in victims.geometries {
            if self.geometry.remove(&key).is_some() {
                self.stats.cache_eviction_count = self.stats.cache_eviction_count.saturating_add(1);
            }
        }
        self.stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id)
            .ok_or(PacketStageError::InvalidStageId)?
            .victims_applied = true;
        Ok(())
    }

    fn commit_stage_activation(&mut self, id: PacketStageId) -> Result<(), PacketStageError> {
        let Some(record) = self
            .stage_state
            .active
            .as_mut()
            .filter(|record| record.id == id && record.promoted)
        else {
            return Err(PacketStageError::InvalidStageId);
        };
        let expected = record
            .expected
            .take()
            .ok_or(PacketStageError::InvalidStageId)?;
        self.active_keys = expected.geometries;
        self.active_textures = expected.textures;
        self.active_samplers = expected.samplers;
        self.active_texture_bind_groups = expected.bind_groups;
        self.active_default_texture = expected.default_texture;
        Ok(())
    }

    fn finish_stage(&mut self, id: PacketStageId) -> Result<PacketStageStatus, PacketStageError> {
        let Some(mut record) = self
            .stage_state
            .active
            .take()
            .filter(|record| record.id == id)
        else {
            return Err(PacketStageError::InvalidStageId);
        };
        record.status.state = PacketStageState::Ready;
        record.status.initialization_complete = true;
        let status = record.status.clone();
        self.stage_state.last_terminal = Some(status.clone());
        self.release_stage_bookkeeping();
        Ok(status)
    }

    fn release_stage_bookkeeping(&mut self) {
        self.staging_bookkeeping_bytes = 0;
        self.asset_stats.bookkeeping_reserved_bytes = self.bookkeeping_base_bytes;
    }

    /// Advances at most one upload chunk and combines it with an old-ready draw
    /// or a bootstrap clear. Candidate frames are submitted only after the stage
    /// initialization serial has completed.
    #[allow(clippy::too_many_arguments)] // Keeps target, frame, view and pump controls explicit at the public entrypoint.
    pub fn render_cpu_frame_with_stage(
        &mut self,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        frame: Option<Arc<CpuFrameHandle>>,
        view: &RenderView,
        stage: Option<PacketStageId>,
        loading_elapsed: Option<f32>,
    ) -> Result<PacketStageFrameSubmission, PacketStageError> {
        self.renderer
            .validate_dimensions(width, height)
            .map_err(PacketRenderError::from)?;
        view.validate()?;
        if let Some(frame) = &frame {
            check_frame_limits(self, frame)?;
            self.validate_packet_target_format(frame.packet())?;
        }
        self.poll_retirement()?;
        self.refresh_stage_state();

        let Some(stage_id) = stage else {
            let submission = if let Some(frame) = frame {
                if !frame_matches_active(self, &frame) {
                    return Err(PacketStageError::FrameResourcesUnavailable);
                }
                let packet = frame.packet_arc();
                Some(self.submit_packet_frame_with_stage(
                    packet,
                    false,
                    target,
                    width,
                    height,
                    Some(&super::ViewOverride::Full(view)),
                    loading_elapsed,
                    crate::renderer::ClearAlphaPolicy::Opaque,
                    None,
                    Some(frame.validation()),
                    Some(Arc::clone(&frame)),
                    None,
                )?)
            } else if let Some(ready) = self.ready_packet.as_ref().cloned() {
                Some(self.submit_packet_frame(
                    ready,
                    false,
                    target,
                    width,
                    height,
                    Some(&super::ViewOverride::Full(view)),
                    loading_elapsed,
                    crate::renderer::ClearAlphaPolicy::Opaque,
                )?)
            } else {
                self.submit_clear_only(target, width, height, loading_elapsed, None, None)?;
                None
            };
            return Ok(PacketStageFrameSubmission {
                frame: submission,
                progress: None,
            });
        };

        let is_active_stage = self
            .stage_state
            .active
            .as_ref()
            .is_some_and(|record| record.id == stage_id);
        if !is_active_stage {
            let status = self.stage_state.status(stage_id)?;
            if status.state == PacketStageState::Ready
                && frame
                    .as_ref()
                    .is_some_and(|frame| same_frame_identity(frame, &status))
            {
                let frame = frame.expect("identity condition requires frame");
                if !frame_matches_active(self, &frame) {
                    return Err(PacketStageError::FrameResourcesUnavailable);
                }
                let submission = self.submit_packet_frame_with_stage(
                    frame.packet_arc(),
                    false,
                    target,
                    width,
                    height,
                    Some(&super::ViewOverride::Full(view)),
                    loading_elapsed,
                    crate::renderer::ClearAlphaPolicy::Opaque,
                    None,
                    Some(frame.validation()),
                    Some(Arc::clone(&frame)),
                    None,
                )?;
                return Ok(PacketStageFrameSubmission {
                    frame: Some(submission),
                    progress: Some(status),
                });
            }
            return Err(PacketStageError::InvalidStageId);
        }

        let current_status = self.stage_state.status(stage_id)?;
        if let Some(frame) = &frame {
            let packet = frame.packet();
            let active_identity = self
                .ready_packet
                .as_ref()
                .is_some_and(|ready| same_packet_identity(ready, packet));
            let candidate_identity = same_frame_identity(frame, &current_status);
            if !active_identity && !candidate_identity {
                return Err(PacketStageError::FrameIdentityMismatch);
            }
            if active_identity && !frame_matches_active(self, frame) {
                return Err(PacketStageError::FrameResourcesUnavailable);
            }
            if candidate_identity && !frame_matches_stage(self, stage_id, frame) {
                return Err(PacketStageError::FrameResourcesUnavailable);
            }
        }

        let Some(record) = self
            .stage_state
            .active
            .as_ref()
            .filter(|record| record.id == stage_id)
        else {
            let status = self.stage_state.status(stage_id)?;
            return Ok(PacketStageFrameSubmission {
                frame: None,
                progress: Some(status),
            });
        };
        let ready_now = record.status.state == PacketStageState::Ready;
        if !ready_now {
            validate_overlay_upload_budget(record, self.asset_limits, loading_elapsed.is_some())?;
        }
        if ready_now
            && frame
                .as_ref()
                .is_some_and(|frame| same_frame_identity(frame, &record.status))
        {
            let frame = frame.expect("candidate identity requires frame");
            if !frame_matches_stage(self, stage_id, &frame) {
                return Err(PacketStageError::FrameResourcesUnavailable);
            }
            let retained = Arc::clone(&frame);
            let submission = self.submit_packet_frame_with_stage(
                frame.packet_arc(),
                true,
                target,
                width,
                height,
                Some(&super::ViewOverride::Full(view)),
                loading_elapsed,
                crate::renderer::ClearAlphaPolicy::Opaque,
                None,
                Some(frame.validation()),
                Some(retained),
                Some(StageSubmitAction::Promote { id: stage_id }),
            )?;
            self.commit_stage_activation(stage_id)?;
            let status = self.finish_stage(stage_id)?;
            return Ok(PacketStageFrameSubmission {
                frame: Some(submission),
                progress: Some(status),
            });
        }

        let active_frame = frame.as_ref().filter(|frame| {
            self.ready_packet
                .as_ref()
                .is_some_and(|ready| same_packet_identity(ready, frame.packet()))
        });
        let upload_due = !ready_now && self.stage_upload_due(stage_id)?;
        let chunk = if upload_due {
            if self.staging_submissions.len() >= self.asset_limits.max_in_flight_submissions {
                return Err(PacketStageError::Render(PacketRenderError::ResidencyBusy));
            }
            let overlay_bytes = usize::from(loading_elapsed.is_some())
                * std::mem::size_of::<crate::renderer::OverlayUniform>();
            let frame_staging_bytes = if let Some(frame) = active_frame {
                self.frame_staging_reservation(
                    frame.packet(),
                    view,
                    (width, height),
                    Some(frame.validation()),
                    Some(self.slots.as_ref()),
                    loading_elapsed.is_some(),
                )?
            } else if let Some(ready) = self.ready_packet.as_ref() {
                self.frame_staging_reservation(
                    ready,
                    view,
                    (width, height),
                    None,
                    None,
                    loading_elapsed.is_some(),
                )?
            } else {
                super::frame_upload_staging_bytes(
                    super::SlotWriteCounts::default(),
                    false,
                    loading_elapsed.is_some(),
                )?
            };
            if frame_staging_bytes > self.asset_limits.upload_staging_bytes {
                return Err(PacketStageError::Render(
                    PacketRenderError::AssetByteBudgetExceeded {
                        resource: "upload staging frame reservation",
                        bytes: frame_staging_bytes,
                        limit: self.asset_limits.upload_staging_bytes,
                    },
                ));
            }
            check_submission_admission(
                self.staging_in_flight_bytes,
                frame_staging_bytes,
                self.asset_limits.upload_staging_bytes,
                self.staging_submissions.len(),
                self.asset_limits.max_in_flight_submissions,
            )?;
            let budget = StageUploadBudget::with_submission_headroom(
                record.limits,
                self.asset_limits,
                frame_staging_bytes,
                overlay_bytes,
                self.staging_in_flight_bytes,
            )?;
            self.prepare_stage_upload(stage_id, budget)?
        } else {
            None
        };
        let create_batch = if ready_now {
            StageCreateBatch::default()
        } else {
            self.prepare_stage_create_batch(stage_id)?
        };
        let stage_action = (!create_batch.is_empty()).then_some(StageSubmitAction::Create {
            id: stage_id,
            batch: create_batch,
        });
        if let Some(chunk) = &chunk {
            self.cpu_scratch_peak_bytes = self.cpu_scratch_peak_bytes.max(chunk.cpu_scratch_bytes);
            self.asset_stats.cpu_upload_scratch_peak_bytes = self
                .asset_stats
                .cpu_upload_scratch_peak_bytes
                .max(chunk.cpu_scratch_bytes);
        }
        let output_frame = if let Some(frame) = active_frame {
            let packet = frame.packet_arc();
            let validation = frame.validation();
            let retained = Arc::clone(frame);
            let submission = self.submit_packet_frame_with_stage(
                packet,
                false,
                target,
                width,
                height,
                Some(&super::ViewOverride::Full(view)),
                loading_elapsed,
                crate::renderer::ClearAlphaPolicy::Opaque,
                chunk.map(|chunk| (stage_id, chunk)),
                Some(validation),
                Some(retained),
                stage_action,
            )?;
            Some(submission)
        } else if let Some(ready) = self.ready_packet.as_ref().cloned() {
            let submission = self.submit_packet_frame_with_stage(
                ready,
                false,
                target,
                width,
                height,
                Some(&super::ViewOverride::Full(view)),
                loading_elapsed,
                crate::renderer::ClearAlphaPolicy::Opaque,
                chunk.map(|chunk| (stage_id, chunk)),
                None,
                None,
                stage_action,
            )?;
            Some(submission)
        } else {
            self.submit_clear_only(
                target,
                width,
                height,
                loading_elapsed,
                chunk.map(|chunk| (stage_id, chunk)),
                stage_action,
            )?;
            None
        };
        self.refresh_stage_state();
        let progress = self.stage_state.status(stage_id)?;
        Ok(PacketStageFrameSubmission {
            frame: output_frame,
            progress: Some(progress),
        })
    }

    pub fn try_drain_after_window(
        &mut self,
    ) -> Result<PacketRendererDrainStatus, PacketRendererDrainError> {
        if let Some(id) = self.stage_state.active.as_ref().map(|record| record.id) {
            self.try_cancel_stage(id)
                .map_err(PacketRendererDrainError::Stage)?;
        }
        self.poll_retirement()
            .map_err(|error| PacketRendererDrainError::Stage(PacketStageError::Render(error)))?;
        self.refresh_stage_state();
        if self.stage_state.active.is_none() && self.staging_submissions.is_empty() {
            Ok(PacketRendererDrainStatus::Drained)
        } else {
            Ok(PacketRendererDrainStatus::Pending)
        }
    }

    fn submit_clear_only(
        &mut self,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        loading_elapsed: Option<f32>,
        stage_upload: Option<(PacketStageId, PreparedUploadChunk)>,
        stage_action: Option<StageSubmitAction>,
    ) -> Result<(), PacketStageError> {
        let stage_staging = stage_upload
            .as_ref()
            .map_or(0, |(_, chunk)| chunk.staging_bytes);
        let frame_staging = super::frame_upload_staging_bytes(
            super::SlotWriteCounts::default(),
            false,
            loading_elapsed.is_some(),
        )
        .map_err(PacketStageError::Render)?;
        let staging = stage_staging
            .checked_add(frame_staging)
            .ok_or(PacketStageError::CounterExhausted)?;
        check_submission_admission(
            self.staging_in_flight_bytes,
            staging,
            self.asset_limits.upload_staging_bytes,
            self.staging_submissions.len(),
            self.asset_limits.max_in_flight_submissions,
        )?;
        let _next_serial = self
            .submission_serial
            .checked_add(1)
            .ok_or(PacketStageError::CounterExhausted)?;
        self.staging_submissions
            .try_reserve(1)
            .map_err(|_| PacketStageError::AllocationFailed)?;
        if let Some((stage_id, chunk)) = stage_upload.as_ref() {
            self.preflight_stage_submission(*stage_id, chunk)?;
        }
        if let Some(action) = stage_action {
            self.preflight_stage_action(action, None, None)?;
        }
        if let Some(action) = stage_action {
            self.execute_stage_action(action)?;
        }
        if let Some((stage_id, chunk)) = stage_upload.as_ref() {
            self.write_prepared_upload(*stage_id, chunk);
        }
        if let Some(elapsed) = loading_elapsed {
            self.renderer.queue.write_buffer(
                &self.renderer.overlay_globals,
                0,
                bytemuck::bytes_of(&crate::renderer::OverlayUniform {
                    time: elapsed,
                    width: width as f32,
                    height: height as f32,
                    manual_srgb: if self.renderer.manual_srgb { 1.0 } else { 0.0 },
                }),
            );
            self.asset_stats.overlay_write_count =
                self.asset_stats.overlay_write_count.saturating_add(1);
            self.asset_stats.overlay_write_bytes = self
                .asset_stats
                .overlay_write_bytes
                .saturating_add(frame_staging as u64);
        }
        let mut encoder =
            self.renderer
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Daedalus staged generation bootstrap clear"),
                });
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus staged generation bootstrap clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            drop(pass);
        }
        if loading_elapsed.is_some() {
            let mut overlay = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus staged generation loading overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            overlay.set_pipeline(&self.renderer.overlay_pipeline);
            overlay.set_bind_group(0, &self.renderer.overlay_bind_group, &[]);
            overlay.draw(0..3, 0..1);
        }
        let submission = self.renderer.queue.submit([encoder.finish()]);
        self.note_empty_submission(submission, staging);
        if let Some((stage_id, chunk)) = stage_upload {
            self.record_stage_submission(stage_id, chunk, self.submission_serial);
        }
        Ok(())
    }
}

fn texture_mip(
    image: &daedalus_content::assets::ImageAsset,
    mip_index: usize,
) -> Option<(u32, u32, &[u8])> {
    if mip_index == 0 {
        let base = image.decoded();
        Some((base.width(), base.height(), base.rgba8()))
    } else {
        let mip = image.mips().get(mip_index.checked_sub(1)?)?;
        Some((mip.width(), mip.height(), mip.rgba8()))
    }
}

fn cursor_complete(record: &StageRecord, plan: &ResourceAdmissionPlan) -> bool {
    record.cursor.geometry_index >= plan.missing_geometries.len()
        && record.cursor.geometry_part == 0
        && record.cursor.geometry_offset == 0
        && record.cursor.texture_index >= plan.missing_textures.len()
        && (!plan.default_texture_missing || record.cursor.default_uploaded)
}

/// Updates stage polling state and returns true when a cancelled stage has
/// reached its terminal state. A ready stage remains ready after its heavy
/// plan/input backing has been released.
fn refresh_stage_record(
    record: &mut StageRecord,
    completed: u64,
    uploads_complete: bool,
    initialization: ExpectedResourceInitialization,
) -> bool {
    if record.status.state == PacketStageState::Ready {
        return false;
    }
    if record
        .pending_upload_serial
        .is_some_and(|serial| serial <= completed)
    {
        record.pending_upload_serial = None;
    }
    if matches!(
        record.status.state,
        PacketStageState::Cancelling | PacketStageState::Retiring
    ) {
        if record.pending_upload_serial.is_none() {
            record.status.state = PacketStageState::Cancelled;
            return true;
        }
        record.status.state = PacketStageState::Retiring;
        return false;
    }
    if uploads_complete && record.pending_upload_serial.is_none() && initialization.complete {
        record.status.state = PacketStageState::Ready;
        record.status.initialization_complete = true;
        record.status.initialization_serial = initialization.latest_serial;
        record.resource_set_token = record
            .input
            .as_ref()
            .map(|input| Arc::clone(input.resource_set_token()));
        record.input = None;
        record.plan = None;
        for texture in record.created.textures.values_mut() {
            texture.upload = None;
        }
    } else if record.pending_upload_serial.is_some() {
        record.status.state = PacketStageState::AwaitingCompletion;
    } else if !record.create_tasks.is_empty() || !uploads_complete {
        let has_resource_progress =
            record.create_calls_completed > 0 || record.raw_uploaded_bytes > 0;
        record.status.state = if has_resource_progress {
            PacketStageState::Uploading
        } else {
            PacketStageState::Admitted
        };
    }
    false
}

fn same_frame_identity(frame: &CpuFrameHandle, status: &PacketStageStatus) -> bool {
    frame.session() == status.session
        && frame.project_generation() == status.project_generation
        && frame.source_digest() == status.source_digest.as_ref()
}

fn same_packet_identity(left: &RenderPacket, right: &RenderPacket) -> bool {
    left.session() == right.session()
        && left.project_generation() == right.project_generation()
        && left.source_digest() == right.source_digest()
}

fn check_frame_limits(
    renderer: &PacketRenderer,
    frame: &CpuFrameHandle,
) -> Result<(), PacketStageError> {
    if frame.packet_limits() != renderer.limits || frame.asset_limits() != renderer.asset_limits {
        return Err(PacketStageError::FrameLimitsMismatch);
    }
    Ok(())
}

fn frame_matches_active(renderer: &PacketRenderer, frame: &CpuFrameHandle) -> bool {
    renderer
        .active_resource_set_token
        .as_ref()
        .is_some_and(|token| Arc::ptr_eq(token, frame.resource_set_token()))
        && frame
            .slot_assignments()
            .is_some_and(|assignments| Arc::ptr_eq(assignments, &renderer.slots))
        && renderer
            .ready_packet
            .as_ref()
            .is_some_and(|ready| same_packet_identity(ready, frame.packet()))
}

fn frame_matches_stage(
    renderer: &PacketRenderer,
    id: PacketStageId,
    frame: &CpuFrameHandle,
) -> bool {
    renderer
        .stage_state
        .active
        .as_ref()
        .filter(|record| record.id == id)
        .is_some_and(|record| {
            same_frame_identity(frame, &record.status)
                && record
                    .resource_set_token
                    .as_ref()
                    .or_else(|| {
                        record
                            .input
                            .as_ref()
                            .map(PacketStageInput::resource_set_token)
                    })
                    .is_some_and(|token| Arc::ptr_eq(token, frame.resource_set_token()))
                && frame
                    .slot_assignments()
                    .is_some_and(|slots| Arc::ptr_eq(slots, &record.slot_assignments))
        })
}

fn initialization_complete(serial: u64, completed: u64) -> bool {
    serial != 0 && serial <= completed
}

#[derive(Clone, Copy, Debug, Default)]
struct ExpectedResourceInitialization {
    complete: bool,
    latest_serial: Option<u64>,
}

impl ExpectedResourceInitialization {
    fn include(&mut self, serial: Option<u64>, completed: u64) -> bool {
        let Some(serial) = serial.filter(|serial| initialization_complete(*serial, completed))
        else {
            return false;
        };
        self.latest_serial = Some(
            self.latest_serial
                .map_or(serial, |latest| latest.max(serial)),
        );
        true
    }
}

fn expected_resources_initialization(
    renderer: &PacketRenderer,
    record: &StageRecord,
    completed: u64,
) -> ExpectedResourceInitialization {
    let mut initialization = ExpectedResourceInitialization {
        complete: true,
        latest_serial: None,
    };
    let Some(expected) = record.expected.as_ref() else {
        initialization.complete = false;
        return initialization;
    };
    let Some(input) = record.input.as_ref() else {
        initialization.complete = false;
        return initialization;
    };
    let requests = input.resource_requests();
    for key in &expected.geometries {
        let serial = record
            .created
            .geometries
            .get(key)
            .map(|entry| entry.resource.initialization_serial)
            .or_else(|| {
                renderer
                    .geometry
                    .get(key)
                    .map(|entry| entry.initialization_serial)
            });
        if !initialization.include(serial, completed) {
            initialization.complete = false;
        }
    }
    for key in &expected.textures {
        let serial = record
            .created
            .textures
            .get(key)
            .map(|entry| entry.resource.initialization_serial)
            .or_else(|| {
                renderer
                    .textures
                    .get(key)
                    .map(|entry| entry.initialization_serial)
            });
        if !initialization.include(serial, completed) {
            initialization.complete = false;
        }
    }
    for key in &expected.samplers {
        if !record.created.samplers.contains_key(key) && !renderer.samplers.contains_key(key) {
            initialization.complete = false;
        }
    }
    for key in &expected.bind_groups {
        if !record.created.bind_groups.contains_key(key)
            && !renderer.texture_bind_groups.contains_key(key)
        {
            initialization.complete = false;
        }
    }
    if expected.default_texture {
        let serial = record
            .created
            .default_texture
            .as_ref()
            .or(renderer.default_model_texture.as_ref())
            .map(|resource| resource.initialization_serial);
        if !initialization.include(serial, completed) {
            initialization.complete = false;
        }
    }
    if expected.sprite_uv_buffer
        && record.created.sprite_uv_buffer.is_none()
        && renderer.sprite_uv_buffer.is_none()
    {
        initialization.complete = false;
    }
    let pipelines_ready = (requests.model_bindings.is_empty()
        || renderer.renderer.model_packet_mesh_pipeline().is_some())
        && (requests.sprite_bindings.is_empty()
            || renderer.renderer.sprite_packet_pipeline().is_some());
    initialization.complete &= pipelines_ready;
    initialization
}

// PacketRenderer operation implementations live in packet_renderer.rs so the
// device submission transaction remains adjacent to the existing draw path.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_quota_accepts_positive_limits_and_checks_candidate_atomic_groups() {
        for quota in 1..=3 {
            let limits =
                PacketStageLimits::default().with_max_resource_creates_per_submission(quota);
            assert!(limits.validate().is_ok());
            assert!(validate_candidate_create_group(limits, quota).is_ok());
            assert!(matches!(
                validate_candidate_create_group(limits, quota + 1),
                Err(PacketStageError::InvalidLimits)
            ));
        }

        let single_create =
            PacketStageLimits::default().with_max_resource_creates_per_submission(1);
        assert!(validate_candidate_create_group(single_create, 0).is_ok());
        assert!(validate_candidate_create_group(single_create, 1).is_ok());

        let warm_candidate =
            PacketStageLimits::default().with_max_resource_creates_per_submission(2);
        for group in [0, 1, 2] {
            assert!(validate_candidate_create_group(warm_candidate, group).is_ok());
        }

        let cold_shared_layout_and_pipeline =
            PacketStageLimits::default().with_max_resource_creates_per_submission(3);
        assert!(matches!(
            validate_candidate_create_group(cold_shared_layout_and_pipeline, 4),
            Err(PacketStageError::InvalidLimits)
        ));
        assert!(matches!(
            PacketStageLimits::default()
                .with_max_resource_creates_per_submission(0)
                .validate(),
            Err(PacketStageError::InvalidLimits)
        ));
        assert!(matches!(
            PacketStageLimits::default()
                .with_max_resource_creates_per_submission(DEFAULT_STAGE_CREATE_CALLS + 1)
                .validate(),
            Err(PacketStageError::InvalidLimits)
        ));
    }

    #[test]
    fn upload_budget_streams_under_scratch_and_global_staging_limits() {
        let asset_limits = crate::PacketAssetLimits {
            cpu_upload_scratch_bytes: 1024 * 1024,
            ..crate::PacketAssetLimits::default()
        };
        let limits = PacketStageLimits::default();
        let budget = StageUploadBudget::full(limits, asset_limits);

        budget.validate_unit(32, 32, 32).unwrap();
        assert_eq!(budget.max_units(32, 32, 32).unwrap(), Some(32 * 1024));
        budget.validate_unit(4, 4, 0).unwrap();
        assert_eq!(
            budget.max_units(4, 4, 0).unwrap(),
            Some(4 * 1024 * 1024 / 4)
        );

        let asset_limits = crate::PacketAssetLimits {
            upload_staging_bytes: 1024 * 1024,
            ..crate::PacketAssetLimits::default()
        };
        let budget = StageUploadBudget::full(limits, asset_limits);
        assert_eq!(
            budget.max_units(32, 32, 32).unwrap(),
            Some(1024 * 1024 / 32)
        );
    }

    #[test]
    fn prepared_vertex_scratch_uses_allocated_capacity() {
        let mut values = Vec::<GpuVertex>::with_capacity(8);
        values.push(GpuVertex {
            position: [0.0; 3],
            normal: [0.0, 0.0, 1.0],
        });
        let allocated_bytes = values.capacity() * std::mem::size_of::<GpuVertex>();
        let prepared = PreparedVertexData::Legacy(values);
        assert_eq!(prepared.bytes().len(), std::mem::size_of::<GpuVertex>());
        assert_eq!(prepared.scratch_bytes().unwrap(), allocated_bytes);
        assert!(allocated_bytes > prepared.bytes().len());
    }

    #[test]
    fn padded_texture_minimum_is_rejected_before_stage_admission() {
        let asset_limits = crate::PacketAssetLimits {
            upload_staging_bytes: 400,
            ..crate::PacketAssetLimits::default()
        };
        let budget = StageUploadBudget::full(PacketStageLimits::default(), asset_limits);

        assert!(matches!(
            budget.validate_unit(260, 512, 0),
            Err(PacketStageError::UploadUnitTooLarge {
                bytes: 512,
                limit: 400
            })
        ));

        let asset_limits = crate::PacketAssetLimits {
            upload_staging_bytes: 128,
            ..crate::PacketAssetLimits::default()
        };
        let budget = StageUploadBudget::full(PacketStageLimits::default(), asset_limits);
        assert!(matches!(
            budget.validate_unit(4, 256, 0),
            Err(PacketStageError::UploadUnitTooLarge {
                bytes: 256,
                limit: 128
            })
        ));
    }

    #[test]
    fn variable_frame_headroom_defers_without_marking_a_valid_unit_oversized() {
        let asset_limits = crate::PacketAssetLimits {
            upload_staging_bytes: 400,
            ..crate::PacketAssetLimits::default()
        };
        let limits = PacketStageLimits::default();
        let dirty_frame =
            StageUploadBudget::with_submission_headroom(limits, asset_limits, 256, 0, 0).unwrap();
        assert_eq!(dirty_frame.max_units(4, 256, 0).unwrap(), None);

        let clean_frame =
            StageUploadBudget::with_submission_headroom(limits, asset_limits, 0, 0, 0).unwrap();
        assert_eq!(clean_frame.max_units(4, 256, 0).unwrap(), Some(1));

        let overlay_only = StageUploadBudget::with_submission_headroom(
            limits,
            crate::PacketAssetLimits {
                upload_staging_bytes: 256,
                ..asset_limits
            },
            16,
            16,
            0,
        )
        .unwrap();
        assert!(matches!(
            overlay_only.max_units(4, 256, 0),
            Err(PacketStageError::UploadUnitTooLarge {
                bytes: 256,
                limit: 240
            })
        ));
    }

    #[test]
    fn admitted_stage_waits_for_resource_progress_and_ready_retains_source_pin() {
        let source: Arc<dyn Send + Sync> = Arc::new(());
        let source_pin = SourcePin::new(Arc::clone(&source));
        let id = PacketStageId {
            renderer_namespace: 1,
            sequence: 1,
        };
        let mut record = StageRecord {
            id,
            input: None,
            resource_set_token: None,
            slot_assignments: Arc::new(HashMap::new()),
            _source_pin: source_pin.clone(),
            plan: None,
            pending_victims: None,
            status: PacketStageStatus {
                id,
                state: PacketStageState::Admitted,
                session: daedalus_core::SessionId::from_raw(1),
                project_generation: 2,
                source_digest: Arc::from(
                    "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ),
                total_upload_bytes: 0,
                uploaded_bytes: 0,
                staging_bytes_submitted: 0,
                create_calls_completed: 0,
                initialization_serial: Some(1),
                initialization_complete: true,
                failure: None,
            },
            created: StagedResources::default(),
            pending_geometry: None,
            pending_texture: None,
            pending_default_texture: None,
            create_tasks: VecDeque::new(),
            pending_upload_serial: None,
            victims_applied: true,
            limits: PacketStageLimits::default(),
            cursor: UploadCursor::default(),
            total_upload_bytes: 0,
            raw_uploaded_bytes: 0,
            staging_bytes_submitted: 0,
            create_calls_completed: 0,
            promoted: false,
            expected: None,
            #[cfg(test)]
            panic_after_pair_handle: None,
        };
        drop(source_pin);

        assert!(!refresh_stage_record(
            &mut record,
            0,
            false,
            ExpectedResourceInitialization::default()
        ));
        assert_eq!(record.status.state(), PacketStageState::Admitted);

        record.create_calls_completed = 1;
        record.status.create_calls_completed = 1;
        assert!(!refresh_stage_record(
            &mut record,
            0,
            false,
            ExpectedResourceInitialization::default()
        ));
        assert_eq!(record.status.state(), PacketStageState::Uploading);

        assert!(!refresh_stage_record(
            &mut record,
            1,
            true,
            ExpectedResourceInitialization {
                complete: true,
                latest_serial: Some(1),
            }
        ));
        assert_eq!(record.status.state(), PacketStageState::Ready);
        assert!(record.status.initialization_complete());
        assert_eq!(record.status.initialization_serial(), Some(1));
        assert!(record.input.is_none());
        assert!(record.plan.is_none());
        assert_eq!(Arc::strong_count(&source), 2);
        drop(record);
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn initialization_summary_reports_latest_required_init_serial() {
        let mut summary = ExpectedResourceInitialization {
            complete: true,
            latest_serial: None,
        };

        assert!(summary.include(Some(4), 8));
        assert!(summary.include(Some(8), 8));
        assert!(summary.include(Some(6), 8));
        assert_eq!(summary.latest_serial, Some(8));
        assert!(!summary.include(Some(9), 8));
        assert_eq!(summary.latest_serial, Some(8));
    }
}
