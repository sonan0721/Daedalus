//! Managed CPU-side render generations, frames, and source-retention roots.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use daedalus_content::{project::PreparedProject, scene::StableId};
use daedalus_core::{EntityHandle, RenderSnapshot, SessionId};

use super::{
    CachedNormal, GeometryKey, ManagedSlotAssignments, MeshHandle, MeshResource, ModelDrawBinding,
    ModelTextureUpload, PacketAssetLimits, PacketRenderError, PacketRendererLimits,
    PreparedModelDraw, ProjectRenderResources, RenderInstance, RenderInstanceId, RenderPacket,
    RenderView, SpriteDrawBinding, TextureKey, hash_map_storage_bytes, hash_set_storage_bytes,
};

const GENERATION_SLOT_COUNT: usize = 2;
const FRAME_SLOT_COUNT: usize = 3;

static NEXT_CPU_OWNER_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque identity shared by the resource-only stage request and frames
/// validated from one managed CPU generation.
pub(crate) struct ManagedResourceSetToken {
    _private: (),
}

impl ManagedResourceSetToken {
    fn new() -> Self {
        Self { _private: () }
    }
}

/// Logical CPU-side residency limits for immutable renderer-owned backings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketCpuLimits {
    pub prepared_metadata_bytes_per_generation: usize,
    pub prepared_metadata_bytes_total: usize,
    pub legacy_mesh_copy_bytes_per_generation: usize,
    pub legacy_mesh_copy_bytes_total: usize,
    pub frame_bytes_per_frame: usize,
    pub frame_bytes_total: usize,
    pub scratch_bytes: usize,
}

impl Default for PacketCpuLimits {
    fn default() -> Self {
        Self {
            prepared_metadata_bytes_per_generation: 8 * 1024 * 1024,
            prepared_metadata_bytes_total: 16 * 1024 * 1024,
            legacy_mesh_copy_bytes_per_generation: 64 * 1024 * 1024,
            legacy_mesh_copy_bytes_total: 128 * 1024 * 1024,
            frame_bytes_per_frame: 16 * 1024 * 1024,
            frame_bytes_total: 48 * 1024 * 1024,
            scratch_bytes: 16 * 1024 * 1024,
        }
    }
}

impl PacketCpuLimits {
    pub fn validate(self) -> Result<(), PacketRenderError> {
        let hard = Self::default();
        if self.prepared_metadata_bytes_per_generation == 0
            || self.prepared_metadata_bytes_total == 0
            || self.legacy_mesh_copy_bytes_per_generation == 0
            || self.legacy_mesh_copy_bytes_total == 0
            || self.frame_bytes_per_frame == 0
            || self.frame_bytes_total == 0
            || self.scratch_bytes == 0
            || self.prepared_metadata_bytes_per_generation
                > hard.prepared_metadata_bytes_per_generation
            || self.prepared_metadata_bytes_total > hard.prepared_metadata_bytes_total
            || self.legacy_mesh_copy_bytes_per_generation
                > hard.legacy_mesh_copy_bytes_per_generation
            || self.legacy_mesh_copy_bytes_total > hard.legacy_mesh_copy_bytes_total
            || self.frame_bytes_per_frame > hard.frame_bytes_per_frame
            || self.frame_bytes_total > hard.frame_bytes_total
            || self.scratch_bytes > hard.scratch_bytes
            || self
                .prepared_metadata_bytes_per_generation
                .checked_mul(2)
                .is_none_or(|needed| self.prepared_metadata_bytes_total < needed)
            || self
                .legacy_mesh_copy_bytes_per_generation
                .checked_mul(2)
                .is_none_or(|needed| self.legacy_mesh_copy_bytes_total < needed)
            || self
                .frame_bytes_per_frame
                .checked_mul(2)
                .is_none_or(|needed| self.frame_bytes_total < needed)
        {
            return Err(PacketRenderError::InvalidCpuLimits);
        }
        Ok(())
    }
}

/// Opaque retention marker for a prepared source generation.
///
/// The payload is never formatted or exposed by renderer APIs.
#[derive(Clone)]
pub struct SourcePin {
    _source: Arc<dyn Send + Sync>,
}

impl SourcePin {
    pub fn new(source: Arc<dyn Send + Sync>) -> Self {
        Self { _source: source }
    }
}

impl fmt::Debug for SourcePin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SourcePin(<opaque>)")
    }
}

/// Renderer-local generation identity. Its private owner component prevents cross-owner reuse.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CpuGenerationId {
    owner: u64,
    sequence: u64,
}

/// Immutable packet and private validation result tracked by a `PacketCpuOwner`.
pub struct CpuFrameHandle {
    backing: Arc<CpuFrameBacking>,
}

pub(super) struct CpuFrameBacking {
    packet: Arc<RenderPacket>,
    validation: super::PacketValidation,
    packet_limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    resource_set_token: Arc<ManagedResourceSetToken>,
}

impl CpuFrameHandle {
    pub fn packet(&self) -> &RenderPacket {
        &self.backing.packet
    }

    pub fn session(&self) -> SessionId {
        self.backing.packet.session()
    }

    pub fn project_generation(&self) -> u64 {
        self.backing.packet.project_generation()
    }

    pub fn source_digest(&self) -> &str {
        self.backing.packet.source_digest()
    }

    pub fn publication_sequence(&self) -> u64 {
        self.backing.packet.sequence()
    }

    pub fn authority_snapshot_sequence(&self) -> Option<u64> {
        self.backing.packet.authority_snapshot_sequence()
    }

    pub fn tick(&self) -> daedalus_core::Tick {
        self.backing.packet.tick()
    }

    pub fn view(&self) -> &RenderView {
        self.backing.packet.view()
    }

    pub(crate) fn packet_arc(&self) -> Arc<RenderPacket> {
        Arc::clone(&self.backing.packet)
    }

    pub(crate) fn validation(&self) -> &super::PacketValidation {
        &self.backing.validation
    }

    pub(crate) fn packet_limits(&self) -> PacketRendererLimits {
        self.backing.packet_limits
    }

    pub(crate) fn asset_limits(&self) -> PacketAssetLimits {
        self.backing.asset_limits
    }

    pub(crate) fn resource_set_token(&self) -> &Arc<ManagedResourceSetToken> {
        &self.backing.resource_set_token
    }

    pub(crate) fn slot_assignments(&self) -> Option<&ManagedSlotAssignments> {
        self.backing.validation.slot_assignments()
    }
}

/// Shared borrowed projection consumed by packet validation and staged GPU planning.
#[derive(Clone, Copy)]
pub(crate) struct ResourceRequests<'a> {
    pub geometries: &'a HashMap<GeometryKey, MeshHandle>,
    pub model_bindings: &'a HashMap<RenderInstanceId, ModelDrawBinding>,
    pub sprite_bindings: &'a HashMap<RenderInstanceId, SpriteDrawBinding>,
}

/// Private immutable projection owned and accounted by one prepared generation.
pub(super) struct StageRequestBacking {
    pub session: SessionId,
    pub project_generation: u64,
    pub source_digest: String,
    pub geometry_count: usize,
    pub texture_count: usize,
    pub draw_count: usize,
    pub model_draw_count: usize,
    pub sprite_draw_count: usize,
    pub expanded_vertex_count: usize,
    pub expanded_index_count: usize,
    pub packet_limits: PacketRendererLimits,
    pub asset_limits: PacketAssetLimits,
    pub source_pin: SourcePin,
    pub geometries: Arc<HashMap<GeometryKey, MeshHandle>>,
    pub model_bindings: Arc<HashMap<RenderInstanceId, ModelDrawBinding>>,
    pub sprite_bindings: Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>,
    pub slot_assignments: ManagedSlotAssignments,
    pub resource_set_token: Arc<ManagedResourceSetToken>,
}

/// Opaque resource-only request retaining the already-accounted prepared projection.
pub struct PacketStageInput {
    backing: Arc<StageRequestBacking>,
}

impl PacketStageInput {
    pub fn session(&self) -> SessionId {
        self.backing.session
    }

    pub fn project_generation(&self) -> u64 {
        self.backing.project_generation
    }

    pub fn source_digest(&self) -> &str {
        &self.backing.source_digest
    }

    pub fn geometry_count(&self) -> usize {
        self.backing.geometry_count
    }

    pub fn draw_count(&self) -> usize {
        self.backing.draw_count
    }

    pub fn model_draw_count(&self) -> usize {
        self.backing.model_draw_count
    }

    pub fn sprite_draw_count(&self) -> usize {
        self.backing.sprite_draw_count
    }

    pub fn expanded_vertex_count(&self) -> usize {
        self.backing.expanded_vertex_count
    }

    pub fn expanded_index_count(&self) -> usize {
        self.backing.expanded_index_count
    }

    pub fn texture_count(&self) -> usize {
        self.backing.texture_count
    }

    pub(crate) fn packet_limits(&self) -> PacketRendererLimits {
        self.backing.packet_limits
    }

    pub(crate) fn asset_limits(&self) -> PacketAssetLimits {
        self.backing.asset_limits
    }

    pub(crate) fn resource_requests(&self) -> ResourceRequests<'_> {
        ResourceRequests {
            geometries: &self.backing.geometries,
            model_bindings: &self.backing.model_bindings,
            sprite_bindings: &self.backing.sprite_bindings,
        }
    }

    pub(crate) fn source_pin(&self) -> &SourcePin {
        &self.backing.source_pin
    }

    pub(crate) fn resource_set_token(&self) -> &Arc<ManagedResourceSetToken> {
        &self.backing.resource_set_token
    }

    pub(crate) fn slot_assignments(&self) -> &ManagedSlotAssignments {
        &self.backing.slot_assignments
    }
}

impl fmt::Debug for PacketStageInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PacketStageInput")
            .field("session", &self.backing.session)
            .field("project_generation", &self.backing.project_generation)
            .field("source_digest", &self.backing.source_digest)
            .field("geometry_count", &self.backing.geometry_count)
            .field("texture_count", &self.backing.texture_count)
            .finish_non_exhaustive()
    }
}

/// Current logical accounting in the managed CPU owner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketCpuStatus {
    pub generation_slots_used: usize,
    pub generation_slots_limit: usize,
    pub frame_slots_used: usize,
    pub frame_slots_limit: usize,
    pub prepared_metadata_bytes: usize,
    pub prepared_metadata_reserved_bytes: usize,
    pub legacy_mesh_copy_bytes: usize,
    pub legacy_mesh_copy_reserved_bytes: usize,
    pub frame_bytes: usize,
    pub frame_reserved_bytes: usize,
    pub scratch_current_bytes: usize,
    pub scratch_peak_bytes: usize,
}

/// Result of one bounded owner-thread reclamation pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CpuReclaimStatus {
    pub reclaimed_generations: usize,
    pub reclaimed_frames: usize,
    pub pending_generations: usize,
    pub pending_frames: usize,
}

pub(super) enum GenerationBacking {
    Stage(Arc<StageRequestBacking>),
    Geometries(Arc<HashMap<GeometryKey, MeshHandle>>),
    ModelBindings(Arc<HashMap<RenderInstanceId, ModelDrawBinding>>),
    SpriteBindings(Arc<HashMap<RenderInstanceId, SpriteDrawBinding>>),
    SpriteOrder(Arc<[RenderInstanceId]>),
    ModelTemplate(Arc<[PreparedModelDraw]>),
    ResourceSetToken(Arc<ManagedResourceSetToken>),
    SlotAssignments(ManagedSlotAssignments),
    Texture(Arc<ModelTextureUpload>),
    Mesh(Arc<MeshResource>),
}

impl GenerationBacking {
    fn is_unique(&mut self) -> bool {
        match self {
            Self::Stage(value) => Arc::get_mut(value).is_some(),
            Self::Geometries(value) => Arc::get_mut(value).is_some(),
            Self::ModelBindings(value) => Arc::get_mut(value).is_some(),
            Self::SpriteBindings(value) => Arc::get_mut(value).is_some(),
            Self::SpriteOrder(value) => Arc::get_mut(value).is_some(),
            Self::ModelTemplate(value) => Arc::get_mut(value).is_some(),
            Self::ResourceSetToken(value) => Arc::get_mut(value).is_some(),
            Self::SlotAssignments(value) => Arc::get_mut(value).is_some(),
            Self::Texture(value) => Arc::get_mut(value).is_some(),
            Self::Mesh(value) => Arc::get_mut(value).is_some(),
        }
    }
}

pub(super) struct GenerationRoots {
    parents: Vec<Option<GenerationBacking>>,
    leaves: Vec<Option<GenerationBacking>>,
}

impl GenerationRoots {
    pub(super) fn new(
        parents: Vec<Option<GenerationBacking>>,
        leaves: Vec<Option<GenerationBacking>>,
    ) -> Self {
        Self { parents, leaves }
    }

    fn try_release(&mut self) -> bool {
        for parent in &mut self.parents {
            if parent.as_mut().is_some_and(GenerationBacking::is_unique) {
                *parent = None;
            }
        }
        if self.parents.iter().any(Option::is_some) {
            return false;
        }
        for leaf in &mut self.leaves {
            if leaf.as_mut().is_some_and(GenerationBacking::is_unique) {
                *leaf = None;
            }
        }
        self.leaves.iter().all(Option::is_none)
    }
}

struct FrameRoots {
    packet: Option<Arc<RenderPacket>>,
    instances: Option<Arc<[RenderInstance]>>,
    normals: Option<Arc<std::sync::OnceLock<HashMap<RenderInstanceId, CachedNormal>>>>,
}

impl FrameRoots {
    fn new(packet: &Arc<RenderPacket>) -> Self {
        Self {
            packet: Some(Arc::clone(packet)),
            instances: Some(Arc::clone(&packet.instances)),
            normals: Some(Arc::clone(&packet.validated_normals)),
        }
    }

    fn try_release(&mut self) -> bool {
        if self
            .packet
            .as_mut()
            .is_some_and(|packet| Arc::get_mut(packet).is_some())
        {
            self.packet = None;
        }
        if self
            .instances
            .as_mut()
            .is_some_and(|instances| Arc::get_mut(instances).is_some())
        {
            self.instances = None;
        }
        if self
            .normals
            .as_mut()
            .is_some_and(|normals| Arc::get_mut(normals).is_some())
        {
            self.normals = None;
        }
        self.packet.is_none() && self.instances.is_none() && self.normals.is_none()
    }
}

pub(super) struct GenerationRecord {
    id: CpuGenerationId,
    resources: Option<ProjectRenderResources>,
    roots: Option<GenerationRoots>,
    source_pin: Option<SourcePin>,
    metadata_bytes: usize,
    metadata_reserved_bytes: usize,
    legacy_mesh_copy_bytes: usize,
    legacy_mesh_copy_reserved_bytes: usize,
    frame_reservation: bool,
    retiring: bool,
}

struct FrameRecord {
    sequence: u64,
    generation_id: CpuGenerationId,
    backing: Option<Arc<CpuFrameBacking>>,
    roots: FrameRoots,
    bytes: usize,
    retiring: bool,
}

/// Sole-owner API for prepared CPU render generations, frames, and source pins.
pub struct PacketCpuOwner {
    owner_id: u64,
    next_generation_sequence: u64,
    next_frame_sequence: u64,
    cpu_limits: PacketCpuLimits,
    packet_limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    generations: Vec<GenerationRecord>,
    frames: Vec<FrameRecord>,
    latest_frame_sequence: Option<u64>,
    metadata_reserved_bytes: usize,
    legacy_reserved_bytes: usize,
    frame_reserved_bytes: usize,
    frame_bytes: usize,
    scratch_current_bytes: usize,
    scratch_peak_bytes: usize,
}

impl PacketCpuOwner {
    pub fn new(
        cpu_limits: PacketCpuLimits,
        packet_limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
    ) -> Result<Self, PacketRenderError> {
        cpu_limits.validate()?;
        packet_limits.validate()?;
        asset_limits.validate()?;
        let owner_id = allocate_owner_id().ok_or(PacketRenderError::StaleCpuGeneration)?;
        Ok(Self {
            owner_id,
            next_generation_sequence: 1,
            next_frame_sequence: 1,
            cpu_limits,
            packet_limits,
            asset_limits,
            generations: Vec::with_capacity(GENERATION_SLOT_COUNT),
            frames: Vec::with_capacity(FRAME_SLOT_COUNT),
            latest_frame_sequence: None,
            metadata_reserved_bytes: 0,
            legacy_reserved_bytes: 0,
            frame_reserved_bytes: 0,
            frame_bytes: 0,
            scratch_current_bytes: 0,
            scratch_peak_bytes: 0,
        })
    }

    pub fn status(&self) -> PacketCpuStatus {
        PacketCpuStatus {
            generation_slots_used: self.generations.len(),
            generation_slots_limit: GENERATION_SLOT_COUNT,
            frame_slots_used: self.frames.len()
                + self
                    .generations
                    .iter()
                    .filter(|record| record.frame_reservation)
                    .count(),
            frame_slots_limit: FRAME_SLOT_COUNT,
            prepared_metadata_bytes: self
                .generations
                .iter()
                .map(|record| record.metadata_bytes)
                .sum(),
            prepared_metadata_reserved_bytes: self.metadata_reserved_bytes,
            legacy_mesh_copy_bytes: self
                .generations
                .iter()
                .map(|record| record.legacy_mesh_copy_bytes)
                .sum(),
            legacy_mesh_copy_reserved_bytes: self.legacy_reserved_bytes,
            frame_bytes: self.frame_bytes,
            frame_reserved_bytes: self.frame_reserved_bytes,
            scratch_current_bytes: self.scratch_current_bytes,
            scratch_peak_bytes: self.scratch_peak_bytes,
        }
    }

    pub fn prepare_generation(
        &mut self,
        project: &PreparedProject,
        project_generation: u64,
        session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        source_pin: SourcePin,
        cancelled: &AtomicBool,
    ) -> Result<CpuGenerationId, PacketRenderError> {
        self.prepare_generation_with_hook(
            project,
            project_generation,
            session,
            authored_handles,
            source_pin,
            cancelled,
            || {},
        )
    }

    #[cfg(test)]
    pub(super) fn prepare_generation_cancel_after_resources(
        &mut self,
        project: &PreparedProject,
        project_generation: u64,
        session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        source_pin: SourcePin,
        cancelled: &AtomicBool,
    ) -> Result<CpuGenerationId, PacketRenderError> {
        self.prepare_generation_with_hook(
            project,
            project_generation,
            session,
            authored_handles,
            source_pin,
            cancelled,
            || cancelled.store(true, Ordering::Relaxed),
        )
    }

    // Keep the prepared-generation identity, limits, source pin, cancellation
    // token, and test-only post-resource hook explicit at this boundary.
    #[allow(clippy::too_many_arguments)]
    fn prepare_generation_with_hook<F>(
        &mut self,
        project: &PreparedProject,
        project_generation: u64,
        session: SessionId,
        authored_handles: &BTreeMap<StableId, EntityHandle>,
        source_pin: SourcePin,
        cancelled: &AtomicBool,
        after_resources: F,
    ) -> Result<CpuGenerationId, PacketRenderError>
    where
        F: FnOnce(),
    {
        check_cancelled(cancelled)?;
        if self.generations.len() >= GENERATION_SLOT_COUNT {
            if self.generations.iter().any(|record| record.retiring) {
                return Err(PacketRenderError::CpuResidencyBusy);
            }
            return Err(PacketRenderError::CpuLimitExceeded {
                resource: "generation slots",
                count: self.generations.len().saturating_add(1),
                limit: GENERATION_SLOT_COUNT,
            });
        }
        if self.next_generation_sequence == u64::MAX {
            return Err(PacketRenderError::StaleCpuGeneration);
        }
        let legacy_bytes = legacy_copy_bytes(project, Some(cancelled))?;
        if legacy_bytes > self.cpu_limits.legacy_mesh_copy_bytes_per_generation {
            return Err(cpu_budget_error(
                "legacy mesh copy",
                legacy_bytes,
                self.cpu_limits.legacy_mesh_copy_bytes_per_generation,
            ));
        }
        let model_index_bytes = vec_backing_bytes::<&StableId>(project.model_renderers().len())?;
        self.check_scratch(model_index_bytes)?;
        self.begin_scratch(model_index_bytes);
        let model_stats = collect_model_scratch_stats(project, cancelled);
        self.release_scratch();
        let model_stats = model_stats?;
        let scratch = prepare_scratch_bytes(project, &model_stats, cancelled)?;
        self.check_scratch(scratch)?;

        let metadata_reserve = self.cpu_limits.prepared_metadata_bytes_per_generation;
        let legacy_reserve = self.cpu_limits.legacy_mesh_copy_bytes_per_generation;
        if self
            .metadata_reserved_bytes
            .checked_add(self.metadata_bytes())
            .and_then(|value| value.checked_add(metadata_reserve))
            .is_none_or(|value| value > self.cpu_limits.prepared_metadata_bytes_total)
        {
            self.release_scratch();
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        if self
            .legacy_reserved_bytes
            .checked_add(self.legacy_bytes())
            .and_then(|value| value.checked_add(legacy_reserve))
            .is_none_or(|value| value > self.cpu_limits.legacy_mesh_copy_bytes_total)
        {
            self.release_scratch();
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        if self.frames.len() + self.frame_reservations() >= FRAME_SLOT_COUNT
            || self
                .frame_bytes
                .checked_add(self.frame_reserved_bytes)
                .and_then(|value| value.checked_add(self.cpu_limits.frame_bytes_per_frame))
                .is_none_or(|value| value > self.cpu_limits.frame_bytes_total)
        {
            self.release_scratch();
            return Err(PacketRenderError::CpuResidencyBusy);
        }

        self.metadata_reserved_bytes += metadata_reserve;
        self.legacy_reserved_bytes += legacy_reserve;
        self.frame_reserved_bytes += self.cpu_limits.frame_bytes_per_frame;
        self.begin_scratch(scratch);
        let resource_set_token = Arc::new(ManagedResourceSetToken::new());
        let result = ProjectRenderResources::prepare_managed(
            project,
            project_generation,
            session,
            authored_handles,
            self.packet_limits,
            self.asset_limits,
            &source_pin,
            Arc::clone(&resource_set_token),
            cancelled,
            metadata_reserve,
            legacy_reserve,
        );
        let resources = match result {
            Ok(resources) => resources,
            Err(error) => {
                self.metadata_reserved_bytes -= metadata_reserve;
                self.legacy_reserved_bytes -= legacy_reserve;
                self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
                self.release_scratch();
                return Err(error);
            }
        };
        after_resources();
        if let Err(error) = check_cancelled(cancelled) {
            self.metadata_reserved_bytes -= metadata_reserve;
            self.legacy_reserved_bytes -= legacy_reserve;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
            self.release_scratch();
            return Err(error);
        }
        let metadata_bytes = resources.prepared_metadata_bytes();
        let legacy_mesh_copy_bytes = resources.legacy_mesh_copy_bytes();
        if metadata_bytes > metadata_reserve {
            self.metadata_reserved_bytes -= metadata_reserve;
            self.legacy_reserved_bytes -= legacy_reserve;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
            self.release_scratch();
            return Err(cpu_budget_error(
                "prepared metadata",
                metadata_bytes,
                metadata_reserve,
            ));
        }
        if legacy_mesh_copy_bytes > legacy_reserve {
            self.metadata_reserved_bytes -= metadata_reserve;
            self.legacy_reserved_bytes -= legacy_reserve;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
            self.release_scratch();
            return Err(cpu_budget_error(
                "legacy mesh copy",
                legacy_mesh_copy_bytes,
                legacy_reserve,
            ));
        }
        let first_frame_bytes = match frame_backing_bytes(
            resources.expanded_instance_count,
            resources
                .stage_requests
                .as_ref()
                .map_or(0, |requests| requests.geometries.len()),
            resources.source_digest.len(),
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.metadata_reserved_bytes -= metadata_reserve;
                self.legacy_reserved_bytes -= legacy_reserve;
                self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
                self.release_scratch();
                return Err(error);
            }
        };
        if first_frame_bytes > self.cpu_limits.frame_bytes_per_frame {
            self.metadata_reserved_bytes -= metadata_reserve;
            self.legacy_reserved_bytes -= legacy_reserve;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
            self.release_scratch();
            return Err(cpu_budget_error(
                "frame backing",
                first_frame_bytes,
                self.cpu_limits.frame_bytes_per_frame,
            ));
        }
        self.metadata_reserved_bytes -= metadata_reserve;
        self.legacy_reserved_bytes -= legacy_reserve;
        let id = CpuGenerationId {
            owner: self.owner_id,
            sequence: self.next_generation_sequence,
        };
        let roots = match resources.cpu_roots(cancelled) {
            Ok(roots) => roots,
            Err(error) => {
                self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
                self.release_scratch();
                return Err(error);
            }
        };
        if let Err(error) = check_cancelled(cancelled) {
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
            self.release_scratch();
            return Err(error);
        }
        self.next_generation_sequence += 1;
        self.generations.push(GenerationRecord {
            id,
            resources: Some(resources),
            roots: Some(roots),
            source_pin: Some(source_pin),
            metadata_bytes,
            metadata_reserved_bytes: 0,
            legacy_mesh_copy_bytes,
            legacy_mesh_copy_reserved_bytes: 0,
            frame_reservation: true,
            retiring: false,
        });
        self.release_scratch();
        Ok(id)
    }

    pub fn stage_input(&self, id: CpuGenerationId) -> Result<PacketStageInput, PacketRenderError> {
        let record = self.generation(id)?;
        if record.retiring {
            return Err(PacketRenderError::StaleCpuGeneration);
        }
        let backing = record
            .resources
            .as_ref()
            .ok_or(PacketRenderError::StaleCpuGeneration)?
            .stage_requests();
        Ok(PacketStageInput { backing: backing? })
    }

    pub fn extract_validated(
        &mut self,
        id: CpuGenerationId,
        snapshot: &RenderSnapshot,
        publication_sequence: u64,
        view: Option<&RenderView>,
        cancelled: &AtomicBool,
    ) -> Result<Arc<CpuFrameHandle>, PacketRenderError> {
        let generation_index = self.generation_index(id)?;
        let resources = self.generations[generation_index]
            .resources
            .as_ref()
            .ok_or(PacketRenderError::StaleCpuGeneration)?;
        let scratch = frame_scratch_bytes(
            snapshot.entities.len(),
            resources.expanded_instance_count,
            resources
                .stage_requests
                .as_ref()
                .map_or(0, |stage| stage.geometries.len()),
        )?;
        self.check_scratch(scratch)?;
        let result = self.extract_validated_inner(
            generation_index,
            id,
            snapshot,
            publication_sequence,
            view,
            cancelled,
            scratch,
        );
        self.release_scratch();
        result
    }

    // These are independent packet, publication, cancellation, and preflight
    // values; a wrapper struct here would only hide this private boundary.
    #[allow(clippy::too_many_arguments)]
    fn extract_validated_inner(
        &mut self,
        generation_index: usize,
        id: CpuGenerationId,
        snapshot: &RenderSnapshot,
        publication_sequence: u64,
        view: Option<&RenderView>,
        cancelled: &AtomicBool,
        scratch: usize,
    ) -> Result<Arc<CpuFrameHandle>, PacketRenderError> {
        check_cancelled(cancelled)?;
        if self.generations[generation_index].retiring {
            return Err(PacketRenderError::StaleCpuGeneration);
        }
        if self.frames.len() >= FRAME_SLOT_COUNT {
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        let reserved = self.generations[generation_index].frame_reservation;
        let (instance_count, geometry_count, source_digest_bytes) = {
            let resources = self.generations[generation_index]
                .resources
                .as_ref()
                .ok_or(PacketRenderError::StaleCpuGeneration)?;
            (
                resources.expanded_instance_count,
                resources
                    .stage_requests
                    .as_ref()
                    .map_or(0, |stage| stage.geometries.len()),
                resources.source_digest.len(),
            )
        };
        let estimate = frame_backing_bytes(instance_count, geometry_count, source_digest_bytes)?;
        if estimate > self.cpu_limits.frame_bytes_per_frame {
            return Err(cpu_budget_error(
                "frame backing",
                estimate,
                self.cpu_limits.frame_bytes_per_frame,
            ));
        }
        if !reserved && self.frames.len() + self.frame_reservations() >= FRAME_SLOT_COUNT {
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        let projected_estimate = self
            .frame_bytes
            .checked_add(self.frame_reserved_bytes)
            .and_then(|value| {
                value.checked_sub(if reserved {
                    self.cpu_limits.frame_bytes_per_frame
                } else {
                    0
                })
            })
            .and_then(|value| value.checked_add(estimate));
        if projected_estimate.is_none_or(|value| value > self.cpu_limits.frame_bytes_total) {
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        self.begin_scratch(scratch);
        let resources = self.generations[generation_index]
            .resources
            .as_ref()
            .ok_or(PacketRenderError::StaleCpuGeneration)?;
        let packet = resources.extract_managed(
            snapshot,
            publication_sequence,
            view,
            self.generations[generation_index]
                .source_pin
                .as_ref()
                .ok_or(PacketRenderError::StaleCpuGeneration)?,
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        let packet = Arc::new(packet);
        let slot_assignments = self.generations[generation_index]
            .resources
            .as_ref()
            .and_then(|resources| resources.managed_slot_assignments.as_ref())
            .ok_or(PacketRenderError::StaleCpuGeneration)?;
        let validation = packet.validate_managed_reusing_with_cancel(
            self.packet_limits,
            self.latest_packet(),
            slot_assignments,
            cancelled,
        )?;
        check_cancelled(cancelled)?;
        let actual_bytes = frame_actual_bytes(&packet, &validation)?;
        if actual_bytes > self.cpu_limits.frame_bytes_per_frame {
            return Err(cpu_budget_error(
                "frame backing",
                actual_bytes,
                self.cpu_limits.frame_bytes_per_frame,
            ));
        }
        let projected_actual = self
            .frame_bytes
            .checked_add(self.frame_reserved_bytes)
            .and_then(|value| {
                value.checked_sub(if reserved {
                    self.cpu_limits.frame_bytes_per_frame
                } else {
                    0
                })
            })
            .and_then(|value| value.checked_add(actual_bytes));
        if projected_actual.is_none_or(|value| value > self.cpu_limits.frame_bytes_total) {
            return Err(PacketRenderError::CpuResidencyBusy);
        }
        let sequence = self.next_frame_sequence;
        if sequence == u64::MAX {
            return Err(PacketRenderError::StaleCpuGeneration);
        }
        check_cancelled(cancelled)?;
        let resource_set_token = Arc::clone(
            self.generations[generation_index]
                .resources
                .as_ref()
                .and_then(|resources| resources.managed_resource_set_token.as_ref())
                .ok_or(PacketRenderError::StaleCpuGeneration)?,
        );
        let backing = Arc::new(CpuFrameBacking {
            packet: Arc::clone(&packet),
            validation,
            packet_limits: self.packet_limits,
            asset_limits: self.asset_limits,
            resource_set_token,
        });
        self.next_frame_sequence += 1;
        if reserved {
            self.generations[generation_index].frame_reservation = false;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
        }
        self.frame_bytes += actual_bytes;
        for frame in &mut self.frames {
            if Some(frame.sequence) == self.latest_frame_sequence {
                frame.retiring = true;
            }
        }
        self.frames.push(FrameRecord {
            sequence,
            generation_id: id,
            backing: Some(Arc::clone(&backing)),
            roots: FrameRoots::new(&packet),
            bytes: actual_bytes,
            retiring: false,
        });
        self.latest_frame_sequence = Some(sequence);
        Ok(Arc::new(CpuFrameHandle { backing }))
    }

    pub fn retire_generation(&mut self, id: CpuGenerationId) -> Result<(), PacketRenderError> {
        let index = self.generation_index(id)?;
        if self.generations[index].retiring {
            return Ok(());
        }
        self.generations[index].retiring = true;
        if self.generations[index].frame_reservation {
            self.generations[index].frame_reservation = false;
            self.frame_reserved_bytes -= self.cpu_limits.frame_bytes_per_frame;
        }
        self.generations[index].resources.take();
        for frame in &mut self.frames {
            if frame.generation_id == id {
                frame.retiring = true;
            }
        }
        if self.latest_frame_sequence.is_some_and(|sequence| {
            self.frames
                .iter()
                .any(|frame| frame.sequence == sequence && frame.generation_id == id)
        }) {
            self.latest_frame_sequence = None;
        }
        Ok(())
    }

    pub fn try_reclaim(&mut self) -> CpuReclaimStatus {
        let mut status = CpuReclaimStatus::default();
        let mut frame_index = 0;
        while frame_index < self.frames.len() {
            let frame = &mut self.frames[frame_index];
            if frame.retiring {
                if frame
                    .backing
                    .as_mut()
                    .is_some_and(|backing| Arc::get_mut(backing).is_some())
                {
                    frame.backing.take();
                }
                let released = frame.roots.try_release();
                if released && frame.backing.is_none() {
                    let frame = self.frames.remove(frame_index);
                    self.frame_bytes = self.frame_bytes.saturating_sub(frame.bytes);
                    status.reclaimed_frames += 1;
                    continue;
                }
                status.pending_frames += 1;
            }
            frame_index += 1;
        }
        let mut generation_index = 0;
        while generation_index < self.generations.len() {
            let generation = &mut self.generations[generation_index];
            if generation.retiring {
                let released = generation
                    .roots
                    .as_mut()
                    .is_some_and(GenerationRoots::try_release);
                if released {
                    generation.roots.take();
                    generation.source_pin.take();
                    let generation = self.generations.remove(generation_index);
                    self.metadata_reserved_bytes = self
                        .metadata_reserved_bytes
                        .saturating_sub(generation.metadata_reserved_bytes);
                    self.legacy_reserved_bytes = self
                        .legacy_reserved_bytes
                        .saturating_sub(generation.legacy_mesh_copy_reserved_bytes);
                    status.reclaimed_generations += 1;
                    continue;
                }
                status.pending_generations += 1;
            }
            generation_index += 1;
        }
        status
    }

    fn check_scratch(&self, bytes: usize) -> Result<(), PacketRenderError> {
        if bytes > self.cpu_limits.scratch_bytes {
            return Err(cpu_budget_error(
                "scratch",
                bytes,
                self.cpu_limits.scratch_bytes,
            ));
        }
        Ok(())
    }

    fn begin_scratch(&mut self, bytes: usize) {
        self.scratch_current_bytes = bytes;
        self.scratch_peak_bytes = self.scratch_peak_bytes.max(bytes);
    }

    fn release_scratch(&mut self) {
        self.scratch_current_bytes = 0;
    }

    fn generation_index(&self, id: CpuGenerationId) -> Result<usize, PacketRenderError> {
        if id.owner != self.owner_id {
            return Err(PacketRenderError::StaleCpuGeneration);
        }
        self.generations
            .iter()
            .position(|record| record.id == id)
            .ok_or(PacketRenderError::StaleCpuGeneration)
    }

    fn generation(&self, id: CpuGenerationId) -> Result<&GenerationRecord, PacketRenderError> {
        let index = self.generation_index(id)?;
        Ok(&self.generations[index])
    }

    fn metadata_bytes(&self) -> usize {
        self.generations
            .iter()
            .map(|record| record.metadata_bytes)
            .sum()
    }

    fn legacy_bytes(&self) -> usize {
        self.generations
            .iter()
            .map(|record| record.legacy_mesh_copy_bytes)
            .sum()
    }

    fn frame_reservations(&self) -> usize {
        self.generations
            .iter()
            .filter(|record| record.frame_reservation)
            .count()
    }

    fn latest_packet(&self) -> Option<&RenderPacket> {
        let sequence = self.latest_frame_sequence?;
        self.frames
            .iter()
            .find(|frame| frame.sequence == sequence)
            .and_then(|frame| frame.backing.as_ref())
            .map(|backing| backing.packet.as_ref())
    }
}

fn allocate_owner_id() -> Option<u64> {
    NEXT_CPU_OWNER_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
}

pub(super) fn check_cancelled(cancelled: &AtomicBool) -> Result<(), PacketRenderError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(PacketRenderError::CpuCancelled)
    } else {
        Ok(())
    }
}

fn cpu_budget_error(resource: &'static str, bytes: usize, limit: usize) -> PacketRenderError {
    PacketRenderError::CpuBudgetExceeded {
        resource,
        bytes,
        limit,
    }
}

pub(super) fn legacy_copy_bytes(
    project: &PreparedProject,
    cancelled: Option<&AtomicBool>,
) -> Result<usize, PacketRenderError> {
    project.meshes().values().try_fold(0usize, |total, mesh| {
        if let Some(cancelled) = cancelled {
            check_cancelled(cancelled)?;
        }
        let vertices = mesh
            .vertices
            .capacity()
            .checked_mul(std::mem::size_of::<crate::Vertex>())
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("legacy mesh capacity overflowed".into())
            })?;
        let indices = mesh
            .indices
            .capacity()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("legacy mesh capacity overflowed".into())
            })?;
        total
            .checked_add(vertices)
            .and_then(|value| value.checked_add(indices))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("legacy mesh capacity overflowed".into())
            })
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct ModelScratchStats {
    unique_models: usize,
    total_model_id_bytes: usize,
    total_primitive_refs: usize,
    total_owned_images: usize,
    max_nodes: usize,
    max_edges: usize,
    max_primitive_refs: usize,
    max_owned_images: usize,
}

fn collect_model_scratch_stats(
    project: &PreparedProject,
    cancelled: &AtomicBool,
) -> Result<ModelScratchStats, PacketRenderError> {
    let mut model_ids = Vec::with_capacity(project.model_renderers().len());
    for binding in project.model_renderers().values() {
        check_cancelled(cancelled)?;
        model_ids.push(binding.model_asset_id());
    }
    model_ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    model_ids.dedup();

    let mut stats = ModelScratchStats::default();
    let Some(catalog) = project.asset_catalog() else {
        return Ok(stats);
    };
    for model_id in model_ids {
        check_cancelled(cancelled)?;
        let Some(daedalus_content::assets::ImportedAsset::Model(model)) = catalog.asset(model_id)
        else {
            continue;
        };
        stats.total_model_id_bytes = checked_sum(
            stats.total_model_id_bytes,
            aligned_string_bytes(model_id.as_str())?,
            "model template IDs",
        )?;
        let nodes = model.nodes().len();
        let mut edges = 0usize;
        let mut primitive_refs = 0usize;
        for node in model.nodes() {
            check_cancelled(cancelled)?;
            edges = checked_sum(edges, usize::from(node.parent().is_some()), "model edges")?;
            primitive_refs = checked_sum(
                primitive_refs,
                node.primitive_indices().len(),
                "model primitive references",
            )?;
        }
        let owned_images = model.owned_images().len();
        stats.unique_models = checked_sum(stats.unique_models, 1, "selected model count")?;
        stats.total_primitive_refs = checked_sum(
            stats.total_primitive_refs,
            primitive_refs,
            "selected model primitive references",
        )?;
        stats.total_owned_images = checked_sum(
            stats.total_owned_images,
            owned_images,
            "selected model owned images",
        )?;
        stats.max_nodes = stats.max_nodes.max(nodes);
        stats.max_edges = stats.max_edges.max(edges);
        stats.max_primitive_refs = stats.max_primitive_refs.max(primitive_refs);
        stats.max_owned_images = stats.max_owned_images.max(owned_images);
    }
    Ok(stats)
}

fn prepare_scratch_bytes(
    project: &PreparedProject,
    models: &ModelScratchStats,
    cancelled: &AtomicBool,
) -> Result<usize, PacketRenderError> {
    let mut bytes = 0usize;
    let entity_count = project.entities().len();
    let sprite_binding_count = project.sprite_renderers().len();
    add_bytes(
        &mut bytes,
        hash_set_storage_bytes::<EntityHandle>(entity_count)
            .ok_or_else(|| scratch_overflow("entity handle set"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<
            &StableId,
            (
                &Arc<daedalus_content::assets::StaticModel>,
                super::ModelExpansionStats,
            ),
        >(
            // The implementation reserves for every renderer binding before
            // deduplicating the model IDs. Account that actual requested map
            // capacity even when many entities share one model asset.
            project.model_renderers().len(),
        )?,
    )?;
    add_bytes(
        &mut bytes,
        vec_backing_bytes::<(EntityHandle, super::ProjectBinding)>(entity_count)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<StableId, MeshHandle>(project.meshes().len())
            .ok_or_else(|| scratch_overflow("legacy mesh handle table"))?,
    )?;
    let mut temporary_id_bytes = models.total_model_id_bytes;
    for mesh_id in project.meshes().keys() {
        check_cancelled(cancelled)?;
        temporary_id_bytes = checked_sum(
            temporary_id_bytes,
            aligned_string_bytes(mesh_id.as_str())?,
            "temporary mesh IDs",
        )?;
    }
    for (entity_id, binding) in project.sprite_renderers() {
        check_cancelled(cancelled)?;
        temporary_id_bytes = checked_sum(
            temporary_id_bytes,
            aligned_string_bytes(entity_id.as_str())?,
            "temporary sprite IDs",
        )?;
        temporary_id_bytes = checked_sum(
            temporary_id_bytes,
            aligned_string_bytes(binding.component_id().as_str())?,
            "temporary sprite IDs",
        )?;
    }
    add_bytes(&mut bytes, temporary_id_bytes)?;

    // Model traversal keeps the per-model selected-scene child index, visit
    // set, DFS stack and occurrence list only for the largest one model at a
    // time. Stats vectors from all selected models remain live in selected_models.
    add_bytes(&mut bytes, vec_backing_bytes::<Vec<u32>>(models.max_nodes)?)?;
    add_bytes(
        &mut bytes,
        checked_product(
            models.max_edges,
            2 * std::mem::size_of::<u32>(),
            "model child vectors",
        )?,
    )?;
    add_bytes(&mut bytes, hash_set_growth_peak::<u32>(models.max_nodes)?)?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<(u32, Option<glam::Mat4>)>(models.max_nodes)?,
    )?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<(u32, glam::Mat4)>(models.max_nodes)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_growth_peak::<daedalus_content::assets::LeafCookKey>(models.max_primitive_refs)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_growth_peak::<daedalus_content::assets::LeafCookKey>(models.max_owned_images)?,
    )?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<daedalus_content::assets::LeafCookKey>(models.total_primitive_refs)?,
    )?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<daedalus_content::assets::LeafCookKey>(models.total_owned_images)?,
    )?;
    add_bytes(
        &mut bytes,
        vec_backing_bytes::<PreparedModelDraw>(models.max_primitive_refs)?,
    )?;

    let texture_upload_upper_bound = checked_sum(
        models.total_owned_images,
        sprite_binding_count,
        "texture upload count",
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<daedalus_content::assets::LeafCookKey, MeshHandle>(
            models.total_primitive_refs,
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<daedalus_content::assets::LeafCookKey, Arc<ModelTextureUpload>>(
            texture_upload_upper_bound,
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<StableId, Arc<[PreparedModelDraw]>>(models.unique_models)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<StableId, super::PreparedSpriteDraw>(sprite_binding_count)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_growth_peak::<GeometryKey>(
            project
                .meshes()
                .len()
                .checked_add(models.total_primitive_refs)
                .and_then(|count| count.checked_add(usize::from(sprite_binding_count > 0)))
                .ok_or_else(|| scratch_overflow("geometry dedup entries"))?,
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_growth_peak::<TextureKey>(texture_upload_upper_bound)?,
    )?;

    // Prr's metadata preflight creates temporary geometry/image dedup tables.
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<daedalus_content::assets::LeafCookKey, usize>(
            models.total_primitive_refs,
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_growth_peak::<daedalus_content::assets::LeafCookKey>(models.total_owned_images)?,
    )?;

    // Generation root registration holds pointer sets and capacity-bounded
    // parent/leaf vectors while the completed resources are still live.
    let root_mesh_count = project
        .meshes()
        .len()
        .checked_add(models.total_primitive_refs)
        .and_then(|count| count.checked_add(usize::from(sprite_binding_count > 0)))
        .ok_or_else(|| scratch_overflow("generation mesh roots"))?;
    let root_texture_count = texture_upload_upper_bound;
    let root_leaf_count = checked_sum(
        checked_sum(root_mesh_count, root_texture_count, "generation leaves")?,
        2,
        "resource-set token and slot-assignment roots",
    )?;
    let root_parent_count = checked_sum(5, models.unique_models, "generation parents")?;
    let root_set_total = hash_set_growth_peak::<usize>(root_parent_count)?
        .checked_add(hash_set_growth_peak::<usize>(root_mesh_count)?)
        .and_then(|value| {
            value.checked_add(hash_set_growth_peak::<usize>(root_texture_count).ok()?)
        })
        .ok_or_else(|| scratch_overflow("generation root sets"))?;
    add_bytes(&mut bytes, root_set_total)?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<Option<GenerationBacking>>(root_parent_count)?,
    )?;
    add_bytes(
        &mut bytes,
        vec_growth_peak::<Option<GenerationBacking>>(root_leaf_count)?,
    )?;
    Ok(bytes)
}

fn frame_scratch_bytes(
    entities: usize,
    instances: usize,
    geometry_count: usize,
) -> Result<usize, PacketRenderError> {
    let mut bytes = 0usize;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<EntityHandle, daedalus_core::Transform>(entities)
            .ok_or_else(|| scratch_overflow("snapshot entity map"))?,
    )?;
    // Extraction builds a Vec and then an Arc slice; include both backings at
    // their overlap peak, plus the three HashMaps/HashSet built by validation.
    add_bytes(
        &mut bytes,
        checked_product(
            instances,
            2 * std::mem::size_of::<RenderInstance>(),
            "frame instances",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_set_storage_bytes::<RenderInstanceId>(instances)
            .ok_or_else(|| scratch_overflow("frame instance set"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_growth_peak::<GeometryKey, MeshHandle>(geometry_count)?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, glam::Mat3>(instances)
            .ok_or_else(|| scratch_overflow("frame normal matrix map"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, CachedNormal>(instances)
            .ok_or_else(|| scratch_overflow("frame normal cache"))?,
    )?;
    Ok(bytes)
}

fn frame_backing_bytes(
    instances: usize,
    geometry_count: usize,
    source_digest_bytes: usize,
) -> Result<usize, PacketRenderError> {
    let arc_header = checked_product(2, std::mem::size_of::<usize>(), "Arc header")?;
    let mut bytes = std::mem::size_of::<FrameRecord>();
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<CpuFrameHandle>(),
            "frame handle",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<CpuFrameBacking>(),
            "frame backing",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<RenderPacket>(),
            "render packet",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            checked_product(
                instances,
                std::mem::size_of::<RenderInstance>(),
                "frame instance slice",
            )?,
            "frame instance slice",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<std::sync::OnceLock<HashMap<RenderInstanceId, CachedNormal>>>(),
            "normal cache backing",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, CachedNormal>(instances)
            .ok_or_else(|| scratch_overflow("frame normal cache"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, glam::Mat3>(instances)
            .ok_or_else(|| scratch_overflow("frame normal map"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<GeometryKey, MeshHandle>(geometry_count)
            .ok_or_else(|| scratch_overflow("frame geometry map"))?,
    )?;
    let digest_capacity = source_digest_bytes
        .checked_add(15)
        .map(|value| value & !15)
        .ok_or_else(|| scratch_overflow("frame source digest"))?;
    add_bytes(&mut bytes, digest_capacity)?;
    Ok(bytes)
}

fn frame_actual_bytes(
    packet: &Arc<RenderPacket>,
    validation: &super::PacketValidation,
) -> Result<usize, PacketRenderError> {
    let arc_header = checked_product(2, std::mem::size_of::<usize>(), "Arc header")?;
    let mut bytes = std::mem::size_of::<FrameRecord>();
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<CpuFrameHandle>(),
            "frame handle",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<CpuFrameBacking>(),
            "frame backing",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<RenderPacket>(),
            "render packet",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            checked_product(
                packet.instances.len(),
                std::mem::size_of::<RenderInstance>(),
                "frame instance slice",
            )?,
            "frame instance slice",
        )?,
    )?;
    let normal_cache = packet.validated_normals.get().ok_or_else(|| {
        PacketRenderError::InvalidPacket("packet validation did not retain normal cache".into())
    })?;
    add_bytes(
        &mut bytes,
        checked_sum(
            arc_header,
            std::mem::size_of::<std::sync::OnceLock<HashMap<RenderInstanceId, CachedNormal>>>(),
            "normal cache backing",
        )?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, CachedNormal>(normal_cache.capacity())
            .ok_or_else(|| scratch_overflow("frame normal cache"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<RenderInstanceId, glam::Mat3>(
            validation.normal_matrices.capacity(),
        )
        .ok_or_else(|| scratch_overflow("frame normal map"))?,
    )?;
    add_bytes(
        &mut bytes,
        hash_map_storage_bytes::<GeometryKey, MeshHandle>(validation.geometries.capacity())
            .ok_or_else(|| scratch_overflow("frame geometry map"))?,
    )?;
    let digest_capacity = packet
        .source_digest
        .capacity()
        .checked_add(15)
        .map(|value| value & !15)
        .ok_or_else(|| scratch_overflow("frame source digest"))?;
    add_bytes(&mut bytes, digest_capacity)?;
    Ok(bytes)
}

fn hash_map_growth_peak<K, V>(entries: usize) -> Result<usize, PacketRenderError> {
    let storage = hash_map_storage_bytes::<K, V>(entries)
        .ok_or_else(|| scratch_overflow("HashMap growth"))?;
    checked_product(storage, 3, "HashMap growth peak")
}

fn hash_set_growth_peak<K>(entries: usize) -> Result<usize, PacketRenderError> {
    let storage =
        hash_set_storage_bytes::<K>(entries).ok_or_else(|| scratch_overflow("HashSet growth"))?;
    checked_product(storage, 3, "HashSet growth peak")
}

fn vec_backing_bytes<T>(entries: usize) -> Result<usize, PacketRenderError> {
    let header = 3 * std::mem::size_of::<usize>();
    checked_sum(
        header,
        checked_product(entries, std::mem::size_of::<T>(), "Vec backing")?,
        "Vec backing",
    )
}

fn vec_growth_peak<T>(entries: usize) -> Result<usize, PacketRenderError> {
    checked_product(vec_backing_bytes::<T>(entries)?, 3, "Vec growth peak")
}

fn add_bytes(total: &mut usize, bytes: usize) -> Result<(), PacketRenderError> {
    *total = checked_sum(*total, bytes, "CPU byte estimate")?;
    Ok(())
}

fn checked_sum(
    left: usize,
    right: usize,
    resource: &'static str,
) -> Result<usize, PacketRenderError> {
    left.checked_add(right)
        .ok_or_else(|| scratch_overflow(resource))
}

fn checked_product(
    left: usize,
    right: usize,
    resource: &'static str,
) -> Result<usize, PacketRenderError> {
    left.checked_mul(right)
        .ok_or_else(|| scratch_overflow(resource))
}

fn scratch_overflow(resource: &'static str) -> PacketRenderError {
    PacketRenderError::InvalidPacket(format!("{resource} estimate overflowed"))
}

fn aligned_string_bytes(value: &str) -> Result<usize, PacketRenderError> {
    if value.is_empty() {
        return Ok(0);
    }
    value
        .len()
        .checked_add(15)
        .map(|length| length & !15)
        .ok_or_else(|| scratch_overflow("string capacity"))
}
