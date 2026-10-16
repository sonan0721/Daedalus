//! Persistent device-local geometry and instance resources for render packets.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Vec3};
use wgpu::util::DeviceExt;

use crate::{
    Camera, CaptureArtifact, CaptureOptions, Material, MeshHandle, PacketAssetLimits,
    PacketRenderError, PacketRendererLimits, RenderInstance, RenderInstanceId, RenderPacket,
    Renderer, RendererError,
    packet::{
        GeometryKey, ModelTextureUpload, PacketValidation, SamplerKey, TextureKey,
        TextureSamplerKey, sampler_key,
    },
    renderer::{GlobalsUniform, GpuModelVertex, MaterialUniform, OverlayUniform},
};

/// Cumulative packet path writes and current cache residency.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PacketResourceStats {
    pub geometry_upload_count: u64,
    pub geometry_upload_bytes: u64,
    pub cache_hit_count: u64,
    pub cache_miss_count: u64,
    pub instance_write_count: u64,
    pub instance_write_bytes: u64,
    pub material_write_count: u64,
    pub material_write_bytes: u64,
    pub global_write_count: u64,
    pub global_write_bytes: u64,
    pub depth_target_recreation_count: u64,
    pub cache_eviction_count: u64,
    pub pending_candidate_count: u64,
    pub active_entries: usize,
    pub active_bytes: usize,
    pub retired_entries: usize,
    pub retired_bytes: usize,
    pub pending_candidate: bool,
}

/// Imported-model residency, device cache reuse, upload staging and scratch accounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PacketAssetResourceStats {
    pub texture_upload_count: u64,
    pub texture_upload_bytes: u64,
    pub texture_cache_hit_count: u64,
    pub texture_cache_miss_count: u64,
    pub sampler_create_count: u64,
    pub sampler_cache_hit_count: u64,
    pub sampler_cache_miss_count: u64,
    pub bind_group_create_count: u64,
    pub bind_group_cache_hit_count: u64,
    pub bind_group_cache_miss_count: u64,
    pub active_texture_entries: usize,
    pub retired_texture_entries: usize,
    pub active_sampler_entries: usize,
    pub retired_sampler_entries: usize,
    pub active_bind_group_entries: usize,
    pub retired_bind_group_entries: usize,
    /// Geometry + textures + the shared packet instance/material/global buffers.
    pub resident_active_bytes: usize,
    /// Inactive resources still held until their final submission completes.
    pub resident_retired_bytes: usize,
    pub resident_total_bytes: usize,
    pub upload_staging_in_flight_bytes: usize,
    pub upload_staging_peak_bytes: usize,
    pub in_flight_submissions: usize,
    pub upload_staging_write_count: u64,
    pub upload_staging_write_bytes: u64,
    pub cpu_upload_scratch_current_bytes: usize,
    pub cpu_upload_scratch_peak_bytes: usize,
    /// Conservative capacity reserved for renderer-owned cache and draw records.
    pub bookkeeping_reserved_bytes: usize,
    pub overlay_write_count: u64,
    pub overlay_write_bytes: u64,
    pub pending_candidate_count: u64,
    pub busy_count: u64,
    pub rejected_count: u64,
    pub pending_candidate: bool,
}

struct GeometryResource {
    vertex: wgpu::Buffer,
    index: wgpu::Buffer,
    index_count: u32,
    bytes: usize,
    last_submission: u64,
    last_touch: u64,
}

struct TextureResource {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    bytes: usize,
    last_submission: u64,
    last_touch: u64,
}

struct SamplerResource {
    sampler: wgpu::Sampler,
    last_submission: u64,
    last_touch: u64,
}

struct TextureBindGroupResource {
    bind_group: wgpu::BindGroup,
    last_submission: u64,
    last_touch: u64,
}

struct SlotState {
    id: RenderInstanceId,
    instance: GpuInstance,
    material: MaterialUniform,
}

struct PreparedSlotUpdate {
    planned: HashMap<RenderInstanceId, usize>,
}

struct PacketFrameWrites<'a> {
    width: u32,
    height: u32,
    packet: &'a RenderPacket,
    loading_elapsed: Option<f32>,
    globals: &'a GlobalsUniform,
    globals_changed: bool,
    slot_write_counts: &'a (usize, usize),
    validation: Option<&'a PacketValidation>,
    slot_update: Option<&'a PreparedSlotUpdate>,
    resource_plan: Option<&'a ResourceAdmissionPlan>,
}

struct ResourceAdmissionPlan {
    requested_geometries: HashMap<GeometryKey, MeshHandle>,
    missing_geometries: Vec<(GeometryKey, MeshHandle)>,
    geometry_victims: Vec<GeometryKey>,
    requested_textures: HashMap<TextureKey, Arc<ModelTextureUpload>>,
    missing_textures: Vec<(TextureKey, Arc<ModelTextureUpload>)>,
    texture_victims: Vec<TextureKey>,
    requested_samplers: HashSet<SamplerKey>,
    missing_samplers: Vec<SamplerKey>,
    sampler_victims: Vec<SamplerKey>,
    requested_bind_groups: HashSet<TextureSamplerKey>,
    missing_bind_groups: Vec<TextureSamplerKey>,
    bind_group_victims: Vec<TextureSamplerKey>,
    active_textures: HashSet<TextureKey>,
    active_samplers: HashSet<SamplerKey>,
    active_bind_groups: HashSet<TextureSamplerKey>,
    default_texture_requested: bool,
    default_texture_missing: bool,
    staging_new_bytes: usize,
    scratch_bytes: usize,
}

#[derive(Clone, Copy)]
struct CacheEntryPlan {
    bytes: usize,
    last_submission: u64,
    last_touch: u64,
}

struct DepthTarget {
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// Owns the existing renderer/device and adds bounded, persistent packet resources.
pub struct PacketRenderer {
    renderer: Renderer,
    limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    geometry: HashMap<GeometryKey, GeometryResource>,
    active_keys: HashSet<GeometryKey>,
    textures: HashMap<TextureKey, TextureResource>,
    active_textures: HashSet<TextureKey>,
    samplers: HashMap<SamplerKey, SamplerResource>,
    active_samplers: HashSet<SamplerKey>,
    texture_bind_groups: HashMap<TextureSamplerKey, TextureBindGroupResource>,
    active_texture_bind_groups: HashSet<TextureSamplerKey>,
    default_model_texture: Option<TextureResource>,
    active_default_texture: bool,
    slots: HashMap<RenderInstanceId, usize>,
    slot_state: Vec<Option<SlotState>>,
    instance_buffer: wgpu::Buffer,
    material_buffer: wgpu::Buffer,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    materials_bind_group: wgpu::BindGroup,
    depth: Option<DepthTarget>,
    last_globals: Option<GlobalsUniform>,
    ready_packet: Option<Arc<RenderPacket>>,
    pending_packet: Option<Arc<RenderPacket>>,
    stats: PacketResourceStats,
    asset_stats: PacketAssetResourceStats,
    submission_serial: u64,
    completion_watermark: Arc<AtomicU64>,
    staging_submissions: VecDeque<(u64, usize)>,
    staging_in_flight_bytes: usize,
    cpu_scratch_current_bytes: usize,
    cpu_scratch_peak_bytes: usize,
    fixed_resident_bytes: usize,
    touch_serial: u64,
}

impl PacketRenderer {
    pub(crate) fn new(
        renderer: Renderer,
        limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
    ) -> Result<Self, PacketRenderError> {
        limits.validate()?;
        asset_limits.validate()?;
        let device_limits = renderer.device.limits();
        let instance_size = (limits.max_instances as u64)
            .checked_mul(std::mem::size_of::<GpuInstance>() as u64)
            .ok_or_else(|| {
                PacketRenderError::DeviceLimit("instance buffer size overflowed".into())
            })?;
        let material_size = (limits.max_instances as u64)
            .checked_mul(std::mem::size_of::<MaterialUniform>() as u64)
            .ok_or_else(|| {
                PacketRenderError::DeviceLimit("material buffer size overflowed".into())
            })?;
        if device_limits.max_vertex_attributes < 10
            || device_limits.max_vertex_buffer_array_stride
                < std::mem::size_of::<GpuInstance>() as u32
            || instance_size > device_limits.max_buffer_size
            || material_size > device_limits.max_buffer_size
            || material_size > u64::from(device_limits.max_storage_buffer_binding_size)
        {
            return Err(PacketRenderError::DeviceLimit(format!(
                "requires 10 vertex attributes, a {} byte instance stride, {instance_size} instance bytes and {material_size} material bytes; device exposes {} attributes, {} stride, {} buffer bytes, {} storage binding bytes",
                std::mem::size_of::<GpuInstance>(),
                device_limits.max_vertex_attributes,
                device_limits.max_vertex_buffer_array_stride,
                device_limits.max_buffer_size,
                device_limits.max_storage_buffer_binding_size,
            )));
        }
        let fixed_resident_bytes = usize::try_from(instance_size)
            .ok()
            .and_then(|size| size.checked_add(usize::try_from(material_size).ok()?))
            .and_then(|size| size.checked_add(std::mem::size_of::<GlobalsUniform>()))
            .and_then(|size| size.checked_add(std::mem::size_of::<OverlayUniform>()))
            .ok_or_else(|| {
                PacketRenderError::DeviceLimit("fixed packet resource bytes overflowed".into())
            })?;
        if fixed_resident_bytes > asset_limits.resident_working_bytes {
            return Err(PacketRenderError::AssetWorkingSetExceeded {
                bytes: fixed_resident_bytes,
                limit: asset_limits.resident_working_bytes,
            });
        }
        let bookkeeping_bytes = packet_bookkeeping_bytes(limits, asset_limits)?;
        if bookkeeping_bytes > asset_limits.bookkeeping_bytes {
            return Err(PacketRenderError::InvalidAssetLimits);
        }
        let instance_buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Daedalus persistent packet instances"),
            size: instance_size.max(std::mem::size_of::<GpuInstance>() as u64),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let material_buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Daedalus persistent packet materials"),
            size: material_size.max(std::mem::size_of::<MaterialUniform>() as u64),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_buffer = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Daedalus persistent packet camera and light"),
            size: std::mem::size_of::<GlobalsUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = renderer
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Daedalus persistent packet globals"),
                layout: &renderer.globals_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals_buffer.as_entire_binding(),
                }],
            });
        let materials_bind_group = renderer
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Daedalus persistent packet materials"),
                layout: &renderer.materials_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: material_buffer.as_entire_binding(),
                }],
            });
        let mut slot_state = Vec::new();
        slot_state.resize_with(limits.max_instances, || None);
        Ok(Self {
            renderer,
            limits,
            asset_limits,
            geometry: HashMap::new(),
            active_keys: HashSet::new(),
            textures: HashMap::new(),
            active_textures: HashSet::new(),
            samplers: HashMap::new(),
            active_samplers: HashSet::new(),
            texture_bind_groups: HashMap::new(),
            active_texture_bind_groups: HashSet::new(),
            default_model_texture: None,
            active_default_texture: false,
            slots: HashMap::new(),
            slot_state,
            instance_buffer,
            material_buffer,
            globals_buffer,
            globals_bind_group,
            materials_bind_group,
            depth: None,
            last_globals: None,
            ready_packet: None,
            pending_packet: None,
            stats: PacketResourceStats::default(),
            asset_stats: PacketAssetResourceStats {
                bookkeeping_reserved_bytes: bookkeeping_bytes,
                ..PacketAssetResourceStats::default()
            },
            submission_serial: 0,
            completion_watermark: Arc::new(AtomicU64::new(0)),
            staging_submissions: VecDeque::new(),
            staging_in_flight_bytes: 0,
            cpu_scratch_current_bytes: 0,
            cpu_scratch_peak_bytes: 0,
            fixed_resident_bytes,
            touch_serial: 0,
        })
    }

    #[cfg(any(feature = "window", test))]
    pub(crate) fn device(&self) -> &wgpu::Device {
        &self.renderer.device
    }

    #[cfg(feature = "window")]
    pub(crate) fn validate_dimensions(&self, width: u32, height: u32) -> Result<(), RendererError> {
        self.renderer.validate_dimensions(width, height)
    }

    pub fn resource_stats(&self) -> PacketResourceStats {
        let mut stats = self.stats.clone();
        stats.active_entries = self.active_keys.len();
        stats.active_bytes = self
            .active_keys
            .iter()
            .filter_map(|key| self.geometry.get(key))
            .map(|entry| entry.bytes)
            .sum();
        for (key, entry) in &self.geometry {
            if !self.active_keys.contains(key) {
                stats.retired_entries += 1;
                stats.retired_bytes = stats.retired_bytes.saturating_add(entry.bytes);
            }
        }
        stats.pending_candidate = self.pending_packet.is_some();
        stats
    }

    pub fn asset_resource_stats(&self) -> PacketAssetResourceStats {
        let mut stats = self.asset_stats.clone();
        stats.active_texture_entries = self.active_textures.len();
        stats.retired_texture_entries = self
            .textures
            .len()
            .saturating_sub(self.active_textures.len());
        stats.active_sampler_entries = self.active_samplers.len();
        stats.retired_sampler_entries = self
            .samplers
            .len()
            .saturating_sub(self.active_samplers.len());
        stats.active_bind_group_entries = self.active_texture_bind_groups.len();
        stats.retired_bind_group_entries = self
            .texture_bind_groups
            .len()
            .saturating_sub(self.active_texture_bind_groups.len());
        let active_geometry_bytes = self
            .active_keys
            .iter()
            .filter_map(|key| self.geometry.get(key))
            .map(|entry| entry.bytes)
            .sum::<usize>();
        let retired_geometry_bytes = self
            .geometry
            .iter()
            .filter(|(key, _)| !self.active_keys.contains(key))
            .map(|(_, entry)| entry.bytes)
            .sum::<usize>();
        let active_texture_bytes = self
            .active_textures
            .iter()
            .filter_map(|key| self.textures.get(key))
            .map(|entry| entry.bytes)
            .sum::<usize>();
        let retired_texture_bytes = self
            .textures
            .iter()
            .filter(|(key, _)| !self.active_textures.contains(key))
            .map(|(_, entry)| entry.bytes)
            .sum::<usize>();
        let default_texture_bytes = usize::from(self.default_model_texture.is_some()) * 4;
        stats.resident_active_bytes = self
            .fixed_resident_bytes
            .saturating_add(active_geometry_bytes)
            .saturating_add(active_texture_bytes)
            .saturating_add(usize::from(self.active_default_texture) * default_texture_bytes);
        stats.resident_retired_bytes = retired_geometry_bytes
            .saturating_add(retired_texture_bytes)
            .saturating_add(
                usize::from(self.default_model_texture.is_some() && !self.active_default_texture)
                    * 4,
            );
        stats.resident_total_bytes = stats
            .resident_active_bytes
            .saturating_add(stats.resident_retired_bytes);
        stats.upload_staging_in_flight_bytes = self.staging_in_flight_bytes;
        stats.in_flight_submissions = self.staging_submissions.len();
        stats.cpu_upload_scratch_current_bytes = self.cpu_scratch_current_bytes;
        stats.cpu_upload_scratch_peak_bytes = self.cpu_scratch_peak_bytes;
        stats.pending_candidate = self.pending_packet.is_some();
        stats
    }

    /// Submit a packet to a window view. Camera overrides are borrowed; the packet
    /// and its instance array remain shared and immutable.
    pub fn render_packet_view(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        packet: Arc<RenderPacket>,
        camera_override: Option<&Camera>,
        loading_elapsed: Option<f32>,
    ) -> Result<Arc<RenderPacket>, PacketRenderError> {
        self.renderer.validate_dimensions(width, height)?;
        if let Some(camera) = camera_override {
            camera
                .validate()
                .map_err(|error| PacketRenderError::InvalidPacket(error.to_string()))?;
        }
        self.poll_retirement()?;
        let unchanged = self
            .ready_packet
            .as_ref()
            .is_some_and(|ready| Arc::ptr_eq(ready, &packet));
        if !unchanged {
            match self.submit_packet_frame(
                Arc::clone(&packet),
                true,
                view,
                width,
                height,
                camera_override,
                loading_elapsed,
            ) {
                Ok(submitted) => return Ok(submitted),
                Err(PacketRenderError::ResidencyBusy) => {
                    self.pending_packet = Some(packet);
                    self.stats.pending_candidate_count =
                        self.stats.pending_candidate_count.saturating_add(1);
                    self.asset_stats.pending_candidate_count =
                        self.asset_stats.pending_candidate_count.saturating_add(1);
                    self.asset_stats.busy_count = self.asset_stats.busy_count.saturating_add(1);
                    let Some(active) = self.ready_packet.as_ref().cloned() else {
                        return Err(PacketRenderError::ResidencyBusy);
                    };
                    return self.submit_packet_frame(
                        active,
                        false,
                        view,
                        width,
                        height,
                        camera_override,
                        loading_elapsed,
                    );
                }
                Err(error) => {
                    self.stats.pending_candidate = false;
                    self.asset_stats.rejected_count =
                        self.asset_stats.rejected_count.saturating_add(1);
                    return Err(error);
                }
            }
        }
        let Some(active) = self.ready_packet.as_ref().cloned() else {
            return Err(PacketRenderError::InvalidPacket(
                "no ready packet is available".into(),
            ));
        };
        self.submit_packet_frame(
            active,
            false,
            view,
            width,
            height,
            camera_override,
            loading_elapsed,
        )
    }

    /// Synchronous explicit capture using the same device-local cache as windows.
    pub fn render_packet_png(
        &mut self,
        packet: &RenderPacket,
        options: &CaptureOptions,
    ) -> Result<CaptureArtifact, PacketRenderError> {
        self.renderer
            .validate_dimensions(options.width, options.height)?;
        self.poll_retirement()?;
        let packet = Arc::new(packet.clone());
        let texture = self
            .renderer
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("Daedalus packet capture target"),
                size: wgpu::Extent3d {
                    width: options.width,
                    height: options.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.renderer.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        if let Err(error) = self.submit_packet_frame(
            packet,
            true,
            &view,
            options.width,
            options.height,
            None,
            None,
        ) {
            if matches!(error, PacketRenderError::ResidencyBusy) {
                self.asset_stats.busy_count = self.asset_stats.busy_count.saturating_add(1);
            } else {
                self.asset_stats.rejected_count = self.asset_stats.rejected_count.saturating_add(1);
            }
            return Err(error);
        }
        let copy_encoder =
            self.renderer
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Daedalus packet capture copy"),
                });
        let artifact = self.renderer.capture_texture_png(
            &texture,
            copy_encoder,
            options,
            "Daedalus packet capture readback",
        )?;
        self.poll_retirement()?;
        Ok(artifact)
    }

    #[allow(clippy::too_many_arguments)] // Owns the validation, admission and submission transaction.
    fn submit_packet_frame(
        &mut self,
        packet: Arc<RenderPacket>,
        activate: bool,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        camera_override: Option<&Camera>,
        loading_elapsed: Option<f32>,
    ) -> Result<Arc<RenderPacket>, PacketRenderError> {
        let camera = camera_override.unwrap_or(packet.camera());
        let globals = make_globals(&packet, camera, width, height, self.renderer.manual_srgb);
        let validation = if activate {
            let previous = self
                .pending_packet
                .as_deref()
                .or(self.ready_packet.as_deref());
            Some(packet.validate_reusing(self.limits, previous)?)
        } else {
            None
        };
        let slot_update = validation
            .as_ref()
            .map(|validation| self.prepare_slot_update(&packet, validation))
            .transpose()?;
        let slot_write_counts =
            if let (Some(validation), Some(update)) = (validation.as_ref(), slot_update.as_ref()) {
                self.slot_write_counts(&packet, validation, update)?
            } else {
                (0, 0)
            };
        let resource_plan = validation
            .as_ref()
            .map(|validation| self.plan_resources(validation))
            .transpose()?;
        let globals_changed = self.last_globals != Some(globals);
        let global_bytes = usize::from(globals_changed) * std::mem::size_of::<GlobalsUniform>();
        let overlay_bytes =
            usize::from(loading_elapsed.is_some()) * std::mem::size_of::<OverlayUniform>();
        let frame_write_bytes = slot_write_counts
            .0
            .checked_mul(std::mem::size_of::<GpuInstance>())
            .and_then(|bytes| {
                slot_write_counts
                    .1
                    .checked_mul(std::mem::size_of::<MaterialUniform>())
                    .and_then(|material| bytes.checked_add(material))
            })
            .and_then(|bytes| bytes.checked_add(global_bytes))
            .and_then(|bytes| bytes.checked_add(overlay_bytes))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("frame upload bytes overflowed".into())
            })?;
        let candidate_staging = resource_plan
            .as_ref()
            .map_or(0, |plan| plan.staging_new_bytes);
        let candidate_scratch = resource_plan.as_ref().map_or(0, |plan| plan.scratch_bytes);
        if candidate_scratch > self.asset_limits.cpu_upload_scratch_bytes {
            return Err(PacketRenderError::AssetByteBudgetExceeded {
                resource: "CPU upload scratch",
                bytes: candidate_scratch,
                limit: self.asset_limits.cpu_upload_scratch_bytes,
            });
        }
        let total_staging = candidate_staging
            .checked_add(frame_write_bytes)
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("upload staging bytes overflowed".into())
            })?;
        if total_staging > self.asset_limits.upload_staging_bytes {
            return Err(PacketRenderError::AssetByteBudgetExceeded {
                resource: "upload staging",
                bytes: total_staging,
                limit: self.asset_limits.upload_staging_bytes,
            });
        }
        check_submission_admission(
            self.staging_in_flight_bytes,
            total_staging,
            self.asset_limits.upload_staging_bytes,
            self.staging_submissions.len(),
            self.asset_limits.max_in_flight_submissions,
        )?;
        if let Some(validation) = &validation
            && !validation.model_bindings.is_empty()
        {
            self.renderer.ensure_model_packet_pipeline()?;
        }
        if activate {
            self.validate_candidate_draws(
                &packet,
                slot_update
                    .as_ref()
                    .map_or(&self.slots, |update| &update.planned),
                validation.as_ref().expect("activation validates packet"),
                resource_plan.as_ref().expect("activation plans resources"),
            )?;
        } else {
            self.validate_draw_resources(&packet, &self.slots)?;
        }
        self.ensure_depth(width, height);
        if let (Some(plan), Some(validation)) = (&resource_plan, &validation) {
            self.commit_resources(plan, validation);
        }
        self.write_packet_frame(PacketFrameWrites {
            width,
            height,
            packet: &packet,
            loading_elapsed,
            globals: &globals,
            globals_changed,
            slot_write_counts: &slot_write_counts,
            validation: validation.as_ref(),
            slot_update: slot_update.as_ref(),
            resource_plan: resource_plan.as_ref(),
        });
        let draw_slots = slot_update
            .as_ref()
            .map_or(&self.slots, |update| &update.planned);
        let submission = self.encode_packet(view, &packet, loading_elapsed, draw_slots);
        self.note_submission(submission, total_staging, &packet);
        if let (Some(validation), Some(update)) = (&validation, slot_update) {
            self.commit_slot_update_state(&packet, validation, update);
            self.active_keys = validation.geometries.keys().copied().collect();
            self.active_textures = resource_plan
                .as_ref()
                .map_or_else(HashSet::new, |plan| plan.active_textures.clone());
            self.active_samplers = resource_plan
                .as_ref()
                .map_or_else(HashSet::new, |plan| plan.active_samplers.clone());
            self.active_texture_bind_groups = resource_plan
                .as_ref()
                .map_or_else(HashSet::new, |plan| plan.active_bind_groups.clone());
            self.active_default_texture = resource_plan
                .as_ref()
                .is_some_and(|plan| plan.default_texture_requested);
            self.ready_packet = Some(Arc::clone(&packet));
            if self
                .pending_packet
                .as_ref()
                .is_some_and(|pending| pending.sequence() <= packet.sequence())
            {
                self.pending_packet = None;
            }
        }
        self.asset_stats.pending_candidate = self.pending_packet.is_some();
        Ok(packet)
    }

    fn prepare_slot_update(
        &self,
        packet: &RenderPacket,
        validation: &PacketValidation,
    ) -> Result<PreparedSlotUpdate, PacketRenderError> {
        let planned = plan_instance_slots(
            &self.slots,
            packet.instances().iter().map(|instance| instance.id),
            self.limits.max_instances,
        )?;
        for instance in packet.instances() {
            let slot = planned[&instance.id];
            let normal = validation
                .normal_matrices
                .get(&instance.id)
                .copied()
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket(
                        "validated instance normal matrix is missing".into(),
                    )
                })?;
            let _ = GpuInstance::new(instance, slot, normal)?;
        }
        Ok(PreparedSlotUpdate { planned })
    }

    fn slot_write_counts(
        &self,
        packet: &RenderPacket,
        validation: &PacketValidation,
        update: &PreparedSlotUpdate,
    ) -> Result<(usize, usize), PacketRenderError> {
        let mut instances = 0usize;
        let mut materials = 0usize;
        for instance in packet.instances() {
            let slot = update.planned[&instance.id];
            let previous = self.slot_state[slot].as_ref();
            let normal = validation
                .normal_matrices
                .get(&instance.id)
                .copied()
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("instance normal matrix is missing".into())
                })?;
            let next_instance = if previous.is_some_and(|old| {
                old.id == instance.id && old.instance.model == instance.transform.to_cols_array_2d()
            }) {
                previous.expect("checked Some above").instance
            } else {
                GpuInstance::new(instance, slot, normal)?
            };
            let next = SlotState {
                id: instance.id,
                instance: next_instance,
                material: MaterialUniform::from_material(&instance.material),
            };
            instances = instances
                .checked_add(usize::from(needs_instance_write(previous, &next)))
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("instance write count overflowed".into())
                })?;
            materials = materials
                .checked_add(usize::from(needs_material_write(previous, &next)))
                .ok_or_else(|| {
                    PacketRenderError::InvalidPacket("material write count overflowed".into())
                })?;
        }
        Ok((instances, materials))
    }

    fn commit_slot_update_state(
        &mut self,
        packet: &RenderPacket,
        validation: &PacketValidation,
        update: PreparedSlotUpdate,
    ) {
        for (id, old_slot) in &self.slots {
            if !update.planned.contains_key(id) {
                self.slot_state[*old_slot] = None;
            }
        }
        for instance in packet.instances() {
            let slot = update.planned[&instance.id];
            let normal = validation.normal_matrices[&instance.id];
            let next_instance = if let Some(old) = self.slot_state[slot].as_ref().filter(|old| {
                old.id == instance.id && old.instance.model == instance.transform.to_cols_array_2d()
            }) {
                old.instance
            } else {
                GpuInstance::new(instance, slot, normal)
                    .expect("slot was range-checked before GPU writes")
            };
            self.slot_state[slot] = Some(SlotState {
                id: instance.id,
                instance: next_instance,
                material: MaterialUniform::from_material(&instance.material),
            });
        }
        self.slots = update.planned;
    }

    fn validate_draw_resources(
        &self,
        packet: &RenderPacket,
        slots: &HashMap<RenderInstanceId, usize>,
    ) -> Result<(), PacketRenderError> {
        for instance in packet.instances() {
            if !slots.contains_key(&instance.id)
                || !self.geometry.contains_key(&instance.mesh.key())
            {
                return Err(PacketRenderError::InvalidPacket(
                    "active packet draw resources are incomplete".into(),
                ));
            }
            if instance.mesh.imported_geometry().is_some() {
                let binding = packet.model_binding(&instance.id).ok_or_else(|| {
                    PacketRenderError::InvalidPacket("model draw binding is missing".into())
                })?;
                let key = TextureSamplerKey {
                    texture: binding.texture.as_ref().map(|texture| texture.key),
                    sampler: sampler_key(binding.sampler),
                };
                if !self.texture_bind_groups.contains_key(&key) {
                    return Err(PacketRenderError::InvalidPacket(
                        "model texture bind group is missing".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_candidate_draws(
        &self,
        packet: &RenderPacket,
        slots: &HashMap<RenderInstanceId, usize>,
        validation: &PacketValidation,
        plan: &ResourceAdmissionPlan,
    ) -> Result<(), PacketRenderError> {
        for instance in packet.instances() {
            if !slots.contains_key(&instance.id)
                || !validation.geometries.contains_key(&instance.mesh.key())
            {
                return Err(PacketRenderError::InvalidPacket(
                    "candidate draw is missing its validated geometry or slot".into(),
                ));
            }
            if instance.mesh.imported_geometry().is_some() {
                let binding = validation.model_bindings.get(&instance.id).ok_or_else(|| {
                    PacketRenderError::InvalidPacket(
                        "candidate model draw binding is missing".into(),
                    )
                })?;
                let key = TextureSamplerKey {
                    texture: binding.texture.as_ref().map(|texture| texture.key),
                    sampler: sampler_key(binding.sampler),
                };
                if !plan.requested_bind_groups.contains(&key) {
                    return Err(PacketRenderError::InvalidPacket(
                        "candidate model bind group was not preflighted".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn plan_resources(
        &self,
        validation: &PacketValidation,
    ) -> Result<ResourceAdmissionPlan, PacketRenderError> {
        let requested_geometries = validation.geometries.clone();
        let active_geometries: HashSet<_> = requested_geometries.keys().copied().collect();
        let mut requested_textures = HashMap::<TextureKey, Arc<ModelTextureUpload>>::new();
        let mut requested_samplers = HashSet::new();
        let mut requested_bind_groups = HashSet::new();
        let mut default_texture_requested = false;
        for binding in validation.model_bindings.values() {
            let texture = binding.texture.as_ref().map(|upload| upload.key);
            if let Some(upload) = &binding.texture {
                requested_textures
                    .entry(upload.key)
                    .or_insert_with(|| Arc::clone(upload));
            } else {
                default_texture_requested = true;
            }
            let sampler = sampler_key(binding.sampler);
            requested_samplers.insert(sampler);
            requested_bind_groups.insert(TextureSamplerKey { texture, sampler });
        }
        let active_textures: HashSet<_> = requested_textures.keys().copied().collect();
        let active_samplers = requested_samplers.clone();
        let active_bind_groups = requested_bind_groups.clone();
        check_asset_active_count(
            "textures",
            active_textures.len(),
            self.asset_limits.max_active_textures,
        )?;
        check_asset_active_count(
            "samplers",
            active_samplers.len(),
            self.asset_limits.max_active_samplers,
        )?;
        check_asset_active_count(
            "texture bind groups",
            active_bind_groups.len(),
            self.asset_limits.max_active_bind_groups,
        )?;

        let mut active_bytes = self.fixed_resident_bytes;
        for mesh in requested_geometries.values() {
            active_bytes = active_bytes.checked_add(mesh.gpu_bytes()).ok_or_else(|| {
                PacketRenderError::InvalidPacket("candidate resident size overflowed".into())
            })?;
        }
        for upload in requested_textures.values() {
            if upload.image.decoded().width()
                > self.renderer.device.limits().max_texture_dimension_2d
                || upload.image.decoded().height()
                    > self.renderer.device.limits().max_texture_dimension_2d
            {
                return Err(PacketRenderError::DeviceLimit(
                    "model texture exceeds the adapter's 2D texture dimension limit".into(),
                ));
            }
            active_bytes = active_bytes.checked_add(upload.byte_len).ok_or_else(|| {
                PacketRenderError::InvalidPacket(
                    "candidate texture resident size overflowed".into(),
                )
            })?;
        }
        if default_texture_requested {
            active_bytes = active_bytes.checked_add(4).ok_or_else(|| {
                PacketRenderError::InvalidPacket("default texture resident size overflowed".into())
            })?;
        }
        if active_bytes > self.asset_limits.resident_working_bytes {
            return Err(PacketRenderError::AssetWorkingSetExceeded {
                bytes: active_bytes,
                limit: self.asset_limits.resident_working_bytes,
            });
        }

        let completed = self.completion_watermark.load(Ordering::Acquire);
        let requested_geometry_bytes: HashMap<_, _> = requested_geometries
            .iter()
            .map(|(key, mesh)| (*key, mesh.gpu_bytes()))
            .collect();
        let geometry_existing: HashMap<_, _> = self
            .geometry
            .iter()
            .map(|(key, entry)| {
                (
                    *key,
                    CacheEntryPlan {
                        bytes: entry.bytes,
                        last_submission: entry.last_submission,
                        last_touch: entry.last_touch,
                    },
                )
            })
            .collect();
        let no_active = HashSet::new();
        let mut geometry_victims = plan_cache_admission(
            &geometry_existing,
            &no_active,
            &requested_geometry_bytes,
            completed,
            self.limits,
        )?;
        let mut texture_victims = plan_entry_victims(
            self.textures
                .iter()
                .map(|(key, entry)| (*key, entry.last_submission, entry.last_touch)),
            &active_textures,
            self.asset_limits.max_total_textures,
            completed,
        )?;
        let sampler_victims = plan_entry_victims(
            self.samplers
                .iter()
                .map(|(key, entry)| (*key, entry.last_submission, entry.last_touch)),
            &active_samplers,
            self.asset_limits.max_total_samplers,
            completed,
        )?;
        let mut bind_group_victims = plan_entry_victims(
            self.texture_bind_groups
                .iter()
                .map(|(key, entry)| (*key, entry.last_submission, entry.last_touch)),
            &active_bind_groups,
            self.asset_limits.max_total_bind_groups,
            completed,
        )?;

        let missing_geometries: Vec<_> = requested_geometries
            .iter()
            .filter(|(key, _)| !self.geometry.contains_key(key))
            .map(|(key, mesh)| (*key, mesh.clone()))
            .collect();
        let missing_textures: Vec<_> = requested_textures
            .iter()
            .filter(|(key, _)| !self.textures.contains_key(key))
            .map(|(key, image)| (*key, Arc::clone(image)))
            .collect();
        let missing_samplers: Vec<_> = requested_samplers
            .iter()
            .filter(|key| !self.samplers.contains_key(key))
            .copied()
            .collect();
        let missing_bind_groups: Vec<_> = requested_bind_groups
            .iter()
            .filter(|key| !self.texture_bind_groups.contains_key(key))
            .copied()
            .collect();
        let default_texture_missing =
            default_texture_requested && self.default_model_texture.is_none();
        let mut new_resident = missing_geometries
            .iter()
            .try_fold(0usize, |sum, (_, mesh)| sum.checked_add(mesh.gpu_bytes()))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("new geometry resident size overflowed".into())
            })?;
        new_resident = missing_textures
            .iter()
            .try_fold(new_resident, |sum, (_, texture)| {
                sum.checked_add(texture.byte_len)
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("new texture resident size overflowed".into())
            })?;
        if default_texture_missing {
            new_resident = new_resident.checked_add(4).ok_or_else(|| {
                PacketRenderError::InvalidPacket("default texture resident size overflowed".into())
            })?;
        }
        let geometry_staging = missing_geometries
            .iter()
            .try_fold(0usize, |sum, (_, mesh)| sum.checked_add(mesh.gpu_bytes()))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("geometry staging bytes overflowed".into())
            })?;
        let texture_staging = missing_textures
            .iter()
            .try_fold(0usize, |sum, (_, upload)| {
                texture_upload_staging_bytes(upload).and_then(|bytes| sum.checked_add(bytes))
            })
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("texture staging bytes overflowed".into())
            })?;
        let staging_new_bytes = geometry_staging
            .checked_add(texture_staging)
            .and_then(|bytes| bytes.checked_add(if default_texture_missing { 256 } else { 0 }))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("asset staging bytes overflowed".into())
            })?;
        let scratch_bytes = missing_geometries
            .iter()
            .map(|(_, mesh)| mesh.gpu_bytes())
            .max()
            .unwrap_or(0);

        // Every retained GPU resource is counted once. Completed, no-longer-requested
        // entries can be retired as one atomic candidate plan to satisfy the combined cap.
        let mut resident_after_victims = self.fixed_resident_bytes;
        resident_after_victims = resident_after_victims
            .checked_add(
                self.geometry
                    .values()
                    .map(|entry| entry.bytes)
                    .sum::<usize>(),
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    self.textures
                        .values()
                        .map(|entry| entry.bytes)
                        .sum::<usize>(),
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(usize::from(self.default_model_texture.is_some()) * 4)
            })
            .and_then(|bytes| bytes.checked_add(new_resident))
            .ok_or_else(|| {
                PacketRenderError::InvalidPacket("resident transition size overflowed".into())
            })?;
        let geometry_victim_set: HashSet<_> = geometry_victims.iter().copied().collect();
        let texture_victim_set: HashSet<_> = texture_victims.iter().copied().collect();
        for key in &geometry_victim_set {
            if let Some(entry) = self.geometry.get(key) {
                resident_after_victims = resident_after_victims.saturating_sub(entry.bytes);
            }
        }
        for key in &texture_victim_set {
            if let Some(entry) = self.textures.get(key) {
                resident_after_victims = resident_after_victims.saturating_sub(entry.bytes);
            }
        }
        if resident_after_victims > self.asset_limits.resident_hard_bytes {
            let mut extra_geometry: Vec<_> = self
                .geometry
                .iter()
                .filter(|(key, entry)| {
                    !active_geometries.contains(key)
                        && !geometry_victim_set.contains(key)
                        && entry.last_submission <= completed
                })
                .map(|(key, entry)| (*key, entry.last_touch, entry.bytes))
                .collect();
            let mut extra_textures: Vec<_> = self
                .textures
                .iter()
                .filter(|(key, entry)| {
                    !active_textures.contains(key)
                        && !texture_victim_set.contains(key)
                        && entry.last_submission <= completed
                })
                .map(|(key, entry)| (*key, entry.last_touch, entry.bytes))
                .collect();
            extra_geometry.sort_by_key(|(_, touch, _)| *touch);
            extra_textures.sort_by_key(|(_, touch, _)| *touch);
            let mut candidates: Vec<(u64, bool, GeometryKey, TextureKey, usize)> = extra_geometry
                .into_iter()
                .map(|(key, touch, bytes)| {
                    (
                        touch,
                        true,
                        key,
                        TextureKey {
                            hash: [0; 32],
                            format: 0,
                        },
                        bytes,
                    )
                })
                .collect();
            candidates.extend(extra_textures.into_iter().map(|(key, touch, bytes)| {
                (
                    touch,
                    false,
                    GeometryKey {
                        hash: [0; 32],
                        layout: 0,
                    },
                    key,
                    bytes,
                )
            }));
            candidates.sort_by_key(|(touch, _, _, _, _)| *touch);
            for (_, is_geometry, geometry_key, texture_key, bytes) in candidates {
                if resident_after_victims <= self.asset_limits.resident_hard_bytes {
                    break;
                }
                if is_geometry {
                    if !geometry_victims.contains(&geometry_key) {
                        geometry_victims.push(geometry_key);
                    }
                } else if !texture_victims.contains(&texture_key) {
                    texture_victims.push(texture_key);
                }
                resident_after_victims = resident_after_victims.saturating_sub(bytes);
            }
        }
        if resident_after_victims > self.asset_limits.resident_hard_bytes {
            return Err(PacketRenderError::ResidencyBusy);
        }

        // A texture or sampler victim cannot remain retained through a cached
        // bind group. Retire its completed, no-longer-requested pairs as well.
        let texture_victim_set: HashSet<_> = texture_victims.iter().copied().collect();
        let sampler_victim_set: HashSet<_> = sampler_victims.iter().copied().collect();
        for (key, entry) in &self.texture_bind_groups {
            if !active_bind_groups.contains(key)
                && entry.last_submission <= completed
                && (key
                    .texture
                    .is_some_and(|texture| texture_victim_set.contains(&texture))
                    || sampler_victim_set.contains(&key.sampler))
                && !bind_group_victims.contains(key)
            {
                bind_group_victims.push(*key);
            }
        }
        Ok(ResourceAdmissionPlan {
            requested_geometries,
            missing_geometries,
            geometry_victims,
            requested_textures,
            missing_textures,
            texture_victims,
            requested_samplers,
            missing_samplers,
            sampler_victims,
            requested_bind_groups,
            missing_bind_groups,
            bind_group_victims,
            active_textures,
            active_samplers,
            active_bind_groups,
            default_texture_requested,
            default_texture_missing,
            staging_new_bytes,
            scratch_bytes,
        })
    }

    fn commit_resources(&mut self, plan: &ResourceAdmissionPlan, validation: &PacketValidation) {
        for key in &plan.bind_group_victims {
            self.texture_bind_groups.remove(key);
        }
        for key in &plan.sampler_victims {
            self.samplers.remove(key);
        }
        for key in &plan.texture_victims {
            self.textures.remove(key);
        }
        for key in &plan.geometry_victims {
            if self.geometry.remove(key).is_some() {
                self.stats.cache_eviction_count = self.stats.cache_eviction_count.saturating_add(1);
            }
        }
        for (key, mesh) in &plan.missing_geometries {
            self.cpu_scratch_current_bytes = mesh.gpu_bytes();
            self.cpu_scratch_peak_bytes = self
                .cpu_scratch_peak_bytes
                .max(self.cpu_scratch_current_bytes);
            self.asset_stats.cpu_upload_scratch_current_bytes = self.cpu_scratch_current_bytes;
            self.asset_stats.cpu_upload_scratch_peak_bytes = self.cpu_scratch_peak_bytes;
            let vertex = if let Some(geometry) = mesh.imported_geometry() {
                let uv = geometry.uv0();
                let packed: Vec<_> = geometry
                    .positions()
                    .iter()
                    .enumerate()
                    .map(|(index, position)| GpuModelVertex {
                        position: *position,
                        normal: geometry.normals()[index],
                        uv0: uv.map_or([0.0; 2], |coords| coords[index]),
                    })
                    .collect();
                self.renderer
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Daedalus cached imported model vertices"),
                        contents: bytemuck::cast_slice(&packed),
                        usage: wgpu::BufferUsages::VERTEX,
                    })
            } else {
                let packed: Vec<_> = mesh
                    .legacy_mesh()
                    .expect("legacy handle has mesh backing")
                    .vertices
                    .iter()
                    .map(|vertex| GpuVertex {
                        position: vertex.position,
                        normal: vertex.normal,
                    })
                    .collect();
                self.renderer
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Daedalus cached immutable packet vertices"),
                        contents: bytemuck::cast_slice(&packed),
                        usage: wgpu::BufferUsages::VERTEX,
                    })
            };
            let index =
                self.renderer
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Daedalus cached immutable packet indices"),
                        contents: bytemuck::cast_slice(mesh.indices()),
                        usage: wgpu::BufferUsages::INDEX,
                    });
            self.cpu_scratch_current_bytes = 0;
            self.asset_stats.cpu_upload_scratch_current_bytes = 0;
            let index_count =
                u32::try_from(mesh.index_count()).expect("validated index count fits draw range");
            self.geometry.insert(
                *key,
                GeometryResource {
                    vertex,
                    index,
                    index_count,
                    bytes: mesh.gpu_bytes(),
                    last_submission: 0,
                    last_touch: self.touch_serial.saturating_add(1),
                },
            );
            self.stats.geometry_upload_count = self.stats.geometry_upload_count.saturating_add(1);
            self.stats.geometry_upload_bytes = self
                .stats
                .geometry_upload_bytes
                .saturating_add(mesh.gpu_bytes() as u64);
        }
        for (key, upload) in &plan.missing_textures {
            let image = upload.image.as_ref();
            let texture = self
                .renderer
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("Daedalus imported model base-color texture"),
                    size: wgpu::Extent3d {
                        width: image.decoded().width(),
                        height: image.decoded().height(),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: u32::try_from(image.mips().len() + 1)
                        .expect("bounded mip count"),
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.textures.insert(
                *key,
                TextureResource {
                    _texture: texture,
                    view,
                    bytes: upload.byte_len,
                    last_submission: 0,
                    last_touch: self.touch_serial.saturating_add(1),
                },
            );
        }
        if plan.default_texture_missing {
            let texture = self
                .renderer
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("Daedalus shared white model texture"),
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
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.default_model_texture = Some(TextureResource {
                _texture: texture,
                view,
                bytes: 4,
                last_submission: 0,
                last_touch: self.touch_serial.saturating_add(1),
            });
        }
        for key in &plan.missing_samplers {
            let sampler = self
                .renderer
                .device
                .create_sampler(&sampler_descriptor(*key));
            self.samplers.insert(
                *key,
                SamplerResource {
                    sampler,
                    last_submission: 0,
                    last_touch: self.touch_serial.saturating_add(1),
                },
            );
        }
        if !plan.missing_bind_groups.is_empty() {
            let texture_layout = self
                .renderer
                .model_textures_layout()
                .expect("model texture layout was prepared");
            for key in &plan.missing_bind_groups {
                let texture = key.texture.map_or_else(
                    || {
                        self.default_model_texture
                            .as_ref()
                            .expect("default texture was prepared")
                    },
                    |texture| {
                        self.textures
                            .get(&texture)
                            .expect("requested texture was prepared")
                    },
                );
                let sampler = self
                    .samplers
                    .get(&key.sampler)
                    .expect("requested sampler was prepared");
                let bind_group =
                    self.renderer
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("Daedalus cached model texture sampler pair"),
                            layout: texture_layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&texture.view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&sampler.sampler),
                                },
                            ],
                        });
                self.texture_bind_groups.insert(
                    *key,
                    TextureBindGroupResource {
                        bind_group,
                        last_submission: 0,
                        last_touch: self.touch_serial.saturating_add(1),
                    },
                );
            }
        }
        self.stats.cache_miss_count = self
            .stats
            .cache_miss_count
            .saturating_add(plan.missing_geometries.len() as u64);
        self.stats.cache_hit_count = self.stats.cache_hit_count.saturating_add(
            plan.requested_geometries
                .len()
                .saturating_sub(plan.missing_geometries.len()) as u64,
        );
        self.asset_stats.texture_cache_miss_count = self
            .asset_stats
            .texture_cache_miss_count
            .saturating_add(plan.missing_textures.len() as u64);
        self.asset_stats.texture_cache_hit_count =
            self.asset_stats.texture_cache_hit_count.saturating_add(
                plan.requested_textures
                    .len()
                    .saturating_sub(plan.missing_textures.len()) as u64,
            );
        self.asset_stats.sampler_cache_miss_count = self
            .asset_stats
            .sampler_cache_miss_count
            .saturating_add(plan.missing_samplers.len() as u64);
        self.asset_stats.sampler_cache_hit_count =
            self.asset_stats.sampler_cache_hit_count.saturating_add(
                plan.requested_samplers
                    .len()
                    .saturating_sub(plan.missing_samplers.len()) as u64,
            );
        self.asset_stats.bind_group_cache_miss_count = self
            .asset_stats
            .bind_group_cache_miss_count
            .saturating_add(plan.missing_bind_groups.len() as u64);
        self.asset_stats.bind_group_cache_hit_count =
            self.asset_stats.bind_group_cache_hit_count.saturating_add(
                plan.requested_bind_groups
                    .len()
                    .saturating_sub(plan.missing_bind_groups.len()) as u64,
            );
        self.asset_stats.texture_upload_count = self
            .asset_stats
            .texture_upload_count
            .saturating_add(plan.missing_textures.len() as u64);
        self.asset_stats.texture_upload_bytes =
            self.asset_stats.texture_upload_bytes.saturating_add(
                plan.missing_textures
                    .iter()
                    .map(|(_, upload)| upload.byte_len as u64)
                    .sum::<u64>(),
            );
        self.asset_stats.sampler_create_count = self
            .asset_stats
            .sampler_create_count
            .saturating_add(plan.missing_samplers.len() as u64);
        self.asset_stats.bind_group_create_count = self
            .asset_stats
            .bind_group_create_count
            .saturating_add(plan.missing_bind_groups.len() as u64);
        self.touch_serial = self.touch_serial.saturating_add(1);
        for key in validation.geometries.keys() {
            if let Some(entry) = self.geometry.get_mut(key) {
                entry.last_touch = self.touch_serial;
            }
        }
        for key in &plan.active_textures {
            if let Some(entry) = self.textures.get_mut(key) {
                entry.last_touch = self.touch_serial;
            }
        }
        for key in &plan.active_samplers {
            if let Some(entry) = self.samplers.get_mut(key) {
                entry.last_touch = self.touch_serial;
            }
        }
        for key in &plan.active_bind_groups {
            if let Some(entry) = self.texture_bind_groups.get_mut(key) {
                entry.last_touch = self.touch_serial;
            }
        }
    }

    fn write_texture_uploads(&mut self, plan: &ResourceAdmissionPlan) {
        for (key, upload) in &plan.missing_textures {
            let texture = &self.textures[key]._texture;
            let image = upload.image.as_ref();
            Self::write_texture_mip(
                &self.renderer.queue,
                &mut self.asset_stats,
                texture,
                0,
                image.decoded().width(),
                image.decoded().height(),
                image.decoded().rgba8(),
            );
            for (index, mip) in image.mips().iter().enumerate() {
                Self::write_texture_mip(
                    &self.renderer.queue,
                    &mut self.asset_stats,
                    texture,
                    (index + 1) as u32,
                    mip.width(),
                    mip.height(),
                    mip.rgba8(),
                );
            }
        }
        if plan.default_texture_missing {
            let texture = &self
                .default_model_texture
                .as_ref()
                .expect("default texture was created")
                ._texture;
            Self::write_texture_mip(
                &self.renderer.queue,
                &mut self.asset_stats,
                texture,
                0,
                1,
                1,
                &[255, 255, 255, 255],
            );
        }
    }

    fn write_texture_mip(
        queue: &wgpu::Queue,
        stats: &mut PacketAssetResourceStats,
        texture: &wgpu::Texture,
        mip_level: u32,
        width: u32,
        height: u32,
        rgba8: &[u8],
    ) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba8,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        stats.upload_staging_write_count = stats.upload_staging_write_count.saturating_add(1);
        stats.upload_staging_write_bytes = stats
            .upload_staging_write_bytes
            .saturating_add(rgba8.len() as u64);
    }

    fn write_slot_updates(
        &mut self,
        packet: &RenderPacket,
        validation: &PacketValidation,
        update: &PreparedSlotUpdate,
    ) {
        for instance in packet.instances() {
            let slot = update.planned[&instance.id];
            let normal = validation.normal_matrices[&instance.id];
            let old = self.slot_state[slot].as_ref();
            let gpu = if old.is_some_and(|old| {
                old.id == instance.id && old.instance.model == instance.transform.to_cols_array_2d()
            }) {
                old.expect("checked Some above").instance
            } else {
                GpuInstance::new(instance, slot, normal).expect("slot conversion was preflighted")
            };
            let material = MaterialUniform::from_material(&instance.material);
            let next = SlotState {
                id: instance.id,
                instance: gpu,
                material,
            };
            if needs_instance_write(old, &next) {
                self.renderer.queue.write_buffer(
                    &self.instance_buffer,
                    (slot * std::mem::size_of::<GpuInstance>()) as u64,
                    bytemuck::bytes_of(&next.instance),
                );
                self.asset_stats.upload_staging_write_count = self
                    .asset_stats
                    .upload_staging_write_count
                    .saturating_add(1);
                self.asset_stats.upload_staging_write_bytes = self
                    .asset_stats
                    .upload_staging_write_bytes
                    .saturating_add(std::mem::size_of::<GpuInstance>() as u64);
            }
            if needs_material_write(old, &next) {
                self.renderer.queue.write_buffer(
                    &self.material_buffer,
                    (slot * std::mem::size_of::<MaterialUniform>()) as u64,
                    bytemuck::bytes_of(&next.material),
                );
                self.asset_stats.upload_staging_write_count = self
                    .asset_stats
                    .upload_staging_write_count
                    .saturating_add(1);
                self.asset_stats.upload_staging_write_bytes = self
                    .asset_stats
                    .upload_staging_write_bytes
                    .saturating_add(std::mem::size_of::<MaterialUniform>() as u64);
            }
        }
    }

    fn write_packet_frame(&mut self, frame: PacketFrameWrites<'_>) {
        if let Some(validation) = frame.validation {
            if let Some(plan) = frame.resource_plan {
                self.write_texture_uploads(plan);
            }
            if let Some(update) = frame.slot_update {
                self.write_slot_updates(frame.packet, validation, update);
                self.stats.instance_write_count = self
                    .stats
                    .instance_write_count
                    .saturating_add(frame.slot_write_counts.0 as u64);
                self.stats.instance_write_bytes = self.stats.instance_write_bytes.saturating_add(
                    (frame.slot_write_counts.0 * std::mem::size_of::<GpuInstance>()) as u64,
                );
                self.stats.material_write_count = self
                    .stats
                    .material_write_count
                    .saturating_add(frame.slot_write_counts.1 as u64);
                self.stats.material_write_bytes = self.stats.material_write_bytes.saturating_add(
                    (frame.slot_write_counts.1 * std::mem::size_of::<MaterialUniform>()) as u64,
                );
            }
        }
        if frame.globals_changed {
            self.renderer.queue.write_buffer(
                &self.globals_buffer,
                0,
                bytemuck::bytes_of(frame.globals),
            );
            self.stats.global_write_count = self.stats.global_write_count.saturating_add(1);
            self.stats.global_write_bytes = self
                .stats
                .global_write_bytes
                .saturating_add(std::mem::size_of::<GlobalsUniform>() as u64);
            self.asset_stats.upload_staging_write_count = self
                .asset_stats
                .upload_staging_write_count
                .saturating_add(1);
            self.asset_stats.upload_staging_write_bytes = self
                .asset_stats
                .upload_staging_write_bytes
                .saturating_add(std::mem::size_of::<GlobalsUniform>() as u64);
            self.last_globals = Some(*frame.globals);
        }
        if let Some(elapsed) = frame.loading_elapsed {
            self.renderer.queue.write_buffer(
                &self.renderer.overlay_globals,
                0,
                bytemuck::bytes_of(&OverlayUniform {
                    time: elapsed,
                    width: frame.width as f32,
                    height: frame.height as f32,
                    manual_srgb: if self.renderer.manual_srgb { 1.0 } else { 0.0 },
                }),
            );
            self.asset_stats.overlay_write_count =
                self.asset_stats.overlay_write_count.saturating_add(1);
            self.asset_stats.overlay_write_bytes = self
                .asset_stats
                .overlay_write_bytes
                .saturating_add(std::mem::size_of::<OverlayUniform>() as u64);
            self.asset_stats.upload_staging_write_count = self
                .asset_stats
                .upload_staging_write_count
                .saturating_add(1);
            self.asset_stats.upload_staging_write_bytes = self
                .asset_stats
                .upload_staging_write_bytes
                .saturating_add(std::mem::size_of::<OverlayUniform>() as u64);
        }
    }

    fn encode_packet(
        &self,
        view: &wgpu::TextureView,
        packet: &RenderPacket,
        loading_elapsed: Option<f32>,
        slots: &HashMap<RenderInstanceId, usize>,
    ) -> wgpu::SubmissionIndex {
        let depth_view = &self.depth.as_ref().expect("depth target initialized").view;
        let mut encoder =
            self.renderer
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Daedalus packet render encoder"),
                });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus shared PBR packet pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(packet.clear_color[0]),
                            g: f64::from(packet.clear_color[1]),
                            b: f64::from(packet.clear_color[2]),
                            a: f64::from(packet.clear_color[3]),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
            for instance in packet.instances() {
                let slot = slots[&instance.id];
                let resource = &self.geometry[&instance.mesh.key()];
                if instance.mesh.imported_geometry().is_some() {
                    pass.set_pipeline(
                        self.renderer
                            .model_packet_mesh_pipeline()
                            .expect("model packet pipeline was prepared"),
                    );
                    pass.set_bind_group(
                        2,
                        &self.texture_bind_groups[&TextureSamplerKey {
                            texture: packet
                                .model_binding(&instance.id)
                                .expect("model binding was validated before GPU writes")
                                .texture
                                .as_ref()
                                .map(|texture| texture.key),
                            sampler: sampler_key(
                                packet
                                    .model_binding(&instance.id)
                                    .expect("model binding was validated before GPU writes")
                                    .sampler,
                            ),
                        }]
                            .bind_group,
                        &[],
                    );
                } else {
                    pass.set_pipeline(&self.renderer.packet_mesh_pipeline);
                }
                pass.set_bind_group(0, &self.globals_bind_group, &[]);
                pass.set_bind_group(1, &self.materials_bind_group, &[]);
                pass.set_vertex_buffer(0, resource.vertex.slice(..));
                pass.set_index_buffer(resource.index.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..resource.index_count, 0, slot as u32..slot as u32 + 1);
            }
        }
        if loading_elapsed.is_some() {
            let mut overlay = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus packet loading overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
        self.renderer.queue.submit([encoder.finish()])
    }

    fn ensure_depth(&mut self, width: u32, height: u32) {
        if self
            .depth
            .as_ref()
            .is_none_or(|depth| depth.width != width || depth.height != height)
        {
            let texture = self
                .renderer
                .device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("Daedalus persistent packet depth target"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Depth32Float,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.depth = Some(DepthTarget {
                _texture: texture,
                view,
                width,
                height,
            });
            self.stats.depth_target_recreation_count =
                self.stats.depth_target_recreation_count.saturating_add(1);
        }
    }

    fn note_submission(
        &mut self,
        _submission: wgpu::SubmissionIndex,
        staging_bytes: usize,
        packet: &RenderPacket,
    ) {
        self.submission_serial = self.submission_serial.saturating_add(1);
        let serial = self.submission_serial;
        self.staging_in_flight_bytes = self.staging_in_flight_bytes.saturating_add(staging_bytes);
        self.asset_stats.upload_staging_peak_bytes = self
            .asset_stats
            .upload_staging_peak_bytes
            .max(self.staging_in_flight_bytes);
        self.staging_submissions.push_back((serial, staging_bytes));
        for instance in packet.instances() {
            if let Some(resource) = self.geometry.get_mut(&instance.mesh.key()) {
                resource.last_submission = serial;
            }
            if let Some(binding) = packet.model_binding(&instance.id) {
                let texture_key = binding.texture.as_ref().map(|texture| texture.key);
                let sampler = sampler_key(binding.sampler);
                let bind_group = TextureSamplerKey {
                    texture: texture_key,
                    sampler,
                };
                if let Some(texture_key) = texture_key
                    && let Some(texture) = self.textures.get_mut(&texture_key)
                {
                    texture.last_submission = serial;
                }
                if let Some(sampler) = self.samplers.get_mut(&sampler) {
                    sampler.last_submission = serial;
                }
                if let Some(group) = self.texture_bind_groups.get_mut(&bind_group) {
                    group.last_submission = serial;
                }
                if texture_key.is_none()
                    && let Some(texture) = self.default_model_texture.as_mut()
                {
                    texture.last_submission = serial;
                }
            }
        }
        let watermark = Arc::clone(&self.completion_watermark);
        self.renderer.queue.on_submitted_work_done(move || {
            watermark.fetch_max(serial, Ordering::AcqRel);
        });
    }

    fn poll_retirement(&mut self) -> Result<(), PacketRenderError> {
        self.renderer
            .device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| PacketRenderError::Renderer(RendererError::Poll(error.to_string())))?;
        let completed = self.completion_watermark.load(Ordering::Acquire);
        while self
            .staging_submissions
            .front()
            .is_some_and(|(serial, _)| *serial <= completed)
        {
            if let Some((_, bytes)) = self.staging_submissions.pop_front() {
                self.staging_in_flight_bytes = self.staging_in_flight_bytes.saturating_sub(bytes);
            }
        }
        self.asset_stats.upload_staging_in_flight_bytes = self.staging_in_flight_bytes;
        Ok(())
    }
}

fn check_asset_active_count(
    resource: &'static str,
    count: usize,
    limit: usize,
) -> Result<(), PacketRenderError> {
    if count > limit {
        return Err(PacketRenderError::AssetLimitExceeded {
            resource,
            count,
            limit,
        });
    }
    Ok(())
}

fn check_submission_admission(
    staging_in_flight: usize,
    next_staging: usize,
    staging_limit: usize,
    in_flight_submissions: usize,
    submission_limit: usize,
) -> Result<(), PacketRenderError> {
    if in_flight_submissions >= submission_limit
        || staging_in_flight
            .checked_add(next_staging)
            .is_none_or(|bytes| bytes > staging_limit)
    {
        return Err(PacketRenderError::ResidencyBusy);
    }
    Ok(())
}

fn texture_upload_staging_bytes(upload: &ModelTextureUpload) -> Option<usize> {
    const COPY_ROW_ALIGNMENT: usize = 256;
    let image = upload.image.as_ref();
    std::iter::once((image.decoded().width(), image.decoded().height()))
        .chain(image.mips().iter().map(|mip| (mip.width(), mip.height())))
        .try_fold(0usize, |sum, (width, height)| {
            let row = usize::try_from(width).ok()?.checked_mul(4)?;
            let aligned = row
                .checked_add(COPY_ROW_ALIGNMENT - 1)?
                .checked_div(COPY_ROW_ALIGNMENT)?
                .checked_mul(COPY_ROW_ALIGNMENT)?;
            let level = aligned.checked_mul(usize::try_from(height).ok()?)?;
            sum.checked_add(level)
        })
}

fn plan_entry_victims<K: Copy + Eq + std::hash::Hash>(
    existing: impl Iterator<Item = (K, u64, u64)>,
    requested: &HashSet<K>,
    max_total: usize,
    completed_submission: u64,
) -> Result<Vec<K>, PacketRenderError> {
    let existing: Vec<_> = existing.collect();
    let existing_keys: HashSet<_> = existing.iter().map(|(key, _, _)| *key).collect();
    let mut total = existing.len();
    total = total
        .checked_add(
            requested
                .iter()
                .filter(|key| !existing_keys.contains(key))
                .count(),
        )
        .ok_or_else(|| {
            PacketRenderError::InvalidPacket("asset cache entry count overflowed".into())
        })?;
    if requested.len() > max_total {
        return Err(PacketRenderError::AssetLimitExceeded {
            resource: "cache entries",
            count: requested.len(),
            limit: max_total,
        });
    }
    let mut eligible: Vec<_> = existing
        .iter()
        .filter(|(key, last_submission, _)| {
            !requested.contains(key) && *last_submission <= completed_submission
        })
        .map(|(key, _, last_touch)| (*key, *last_touch))
        .collect();
    eligible.sort_by_key(|(_, last_touch)| *last_touch);
    let needed = total.saturating_sub(max_total);
    if eligible.len() < needed {
        return Err(PacketRenderError::ResidencyBusy);
    }
    Ok(eligible
        .into_iter()
        .take(needed)
        .map(|(key, _)| key)
        .collect())
}

fn packet_bookkeeping_bytes(
    limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
) -> Result<usize, PacketRenderError> {
    // Reserve one bounded draw record plus map/load-factor allowance for the
    // active slot table and candidate planning table, then bounded cache records.
    let draw_record = std::mem::size_of::<SlotState>()
        .checked_add(std::mem::size_of::<RenderInstanceId>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<usize>()))
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or(PacketRenderError::InvalidAssetLimits)?;
    let draws = draw_record
        .checked_mul(limits.max_instances)
        .ok_or(PacketRenderError::InvalidAssetLimits)?;
    let cache_bytes = limits
        .max_total_entries
        .checked_mul(
            std::mem::size_of::<GeometryKey>() + std::mem::size_of::<GeometryResource>() + 96,
        )
        .and_then(|bytes| {
            asset_limits
                .max_total_textures
                .checked_mul(
                    std::mem::size_of::<TextureKey>() + std::mem::size_of::<TextureResource>() + 96,
                )
                .and_then(|textures| bytes.checked_add(textures))
        })
        .and_then(|bytes| {
            asset_limits
                .max_total_samplers
                .checked_mul(
                    std::mem::size_of::<SamplerKey>() + std::mem::size_of::<SamplerResource>() + 64,
                )
                .and_then(|samplers| bytes.checked_add(samplers))
        })
        .and_then(|bytes| {
            asset_limits
                .max_total_bind_groups
                .checked_mul(
                    std::mem::size_of::<TextureSamplerKey>()
                        + std::mem::size_of::<TextureBindGroupResource>()
                        + 64,
                )
                .and_then(|groups| bytes.checked_add(groups))
        })
        .ok_or(PacketRenderError::InvalidAssetLimits)?;
    draws
        .checked_add(cache_bytes)
        .and_then(|bytes| bytes.checked_add(32 * 1024))
        .ok_or(PacketRenderError::InvalidAssetLimits)
}

fn sampler_descriptor(key: SamplerKey) -> wgpu::SamplerDescriptor<'static> {
    let filter = |value| match value {
        0 => wgpu::FilterMode::Nearest,
        _ => wgpu::FilterMode::Linear,
    };
    let address = |value| match value {
        0 => wgpu::AddressMode::ClampToEdge,
        1 => wgpu::AddressMode::MirrorRepeat,
        _ => wgpu::AddressMode::Repeat,
    };
    let mip_filter = match key.0[2] {
        1 => wgpu::FilterMode::Nearest,
        2 => wgpu::FilterMode::Linear,
        _ => wgpu::FilterMode::Nearest,
    };
    wgpu::SamplerDescriptor {
        label: Some("Daedalus imported model sampler"),
        address_mode_u: address(key.0[3]),
        address_mode_v: address(key.0[4]),
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter(key.0[1]),
        min_filter: filter(key.0[0]),
        mipmap_filter: mip_filter,
        lod_min_clamp: 0.0,
        lod_max_clamp: if key.0[2] == 0 { 0.0 } else { 32.0 },
        compare: None,
        anisotropy_clamp: 1,
        border_color: None,
    }
}

fn plan_cache_admission(
    existing: &HashMap<GeometryKey, CacheEntryPlan>,
    active: &HashSet<GeometryKey>,
    requested: &HashMap<GeometryKey, usize>,
    completed_submission: u64,
    limits: PacketRendererLimits,
) -> Result<Vec<GeometryKey>, PacketRenderError> {
    let mut remaining_bytes = existing
        .values()
        .try_fold(0usize, |sum, entry| sum.checked_add(entry.bytes))
        .ok_or_else(|| PacketRenderError::InvalidPacket("resident cache size overflowed".into()))?;
    let mut remaining_entries = existing.len();
    for (key, bytes) in requested {
        if !existing.contains_key(key) {
            remaining_bytes = remaining_bytes.checked_add(*bytes).ok_or_else(|| {
                PacketRenderError::InvalidPacket("candidate cache size overflowed".into())
            })?;
            remaining_entries = remaining_entries.saturating_add(1);
        }
    }
    let mut eligible: Vec<_> = existing
        .iter()
        .filter(|(key, entry)| {
            !active.contains(key)
                && !requested.contains_key(key)
                && entry.last_submission <= completed_submission
        })
        .map(|(key, entry)| (*key, entry.last_touch, entry.bytes))
        .collect();
    eligible.sort_by_key(|(_, last_touch, _)| *last_touch);
    let mut victims = Vec::new();
    let mut index = 0;
    while remaining_entries > limits.max_total_entries
        || remaining_bytes > limits.hard_geometry_bytes
    {
        let Some((key, _, bytes)) = eligible.get(index).copied() else {
            return Err(PacketRenderError::ResidencyBusy);
        };
        index += 1;
        victims.push(key);
        remaining_entries = remaining_entries.saturating_sub(1);
        remaining_bytes = remaining_bytes.saturating_sub(bytes);
    }
    Ok(victims)
}

fn plan_instance_slots(
    current: &HashMap<RenderInstanceId, usize>,
    ids: impl IntoIterator<Item = RenderInstanceId>,
    capacity: usize,
) -> Result<HashMap<RenderInstanceId, usize>, PacketRenderError> {
    let ids: Vec<_> = ids.into_iter().collect();
    if ids.len() > capacity {
        return Err(PacketRenderError::InvalidPacket(
            "instance slot limit exceeded".into(),
        ));
    }
    let mut planned = HashMap::with_capacity(ids.len());
    let mut reserved = HashSet::with_capacity(ids.len());
    for id in &ids {
        if let Some(slot) = current.get(id).copied() {
            if slot >= capacity || !reserved.insert(slot) {
                return Err(PacketRenderError::InvalidPacket(
                    "current instance slot map is invalid".into(),
                ));
            }
            planned.insert(*id, slot);
        }
    }
    let mut available = (0..capacity).filter(|slot| !reserved.contains(slot));
    for id in ids {
        if let std::collections::hash_map::Entry::Vacant(entry) = planned.entry(id) {
            let slot = available.next().ok_or_else(|| {
                PacketRenderError::InvalidPacket("instance slot limit exceeded".into())
            })?;
            entry.insert(slot);
        }
    }
    Ok(planned)
}

fn needs_instance_write(previous: Option<&SlotState>, next: &SlotState) -> bool {
    previous.is_none_or(|old| old.id != next.id || old.instance != next.instance)
}

fn needs_material_write(previous: Option<&SlotState>, next: &SlotState) -> bool {
    previous.is_none_or(|old| old.id != next.id || old.material != next.material)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
struct GpuInstance {
    model: [[f32; 4]; 4],
    normal_0: [f32; 4],
    normal_1: [f32; 4],
    normal_2: [f32; 4],
    material_index: u32,
    padding: [u32; 3],
}

impl GpuInstance {
    fn new(
        instance: &RenderInstance,
        slot: usize,
        normal: Mat3,
    ) -> Result<Self, PacketRenderError> {
        let model = instance.transform;
        let columns = normal.to_cols_array_2d();
        Ok(Self {
            model: model.to_cols_array_2d(),
            normal_0: [columns[0][0], columns[0][1], columns[0][2], 0.0],
            normal_1: [columns[1][0], columns[1][1], columns[1][2], 0.0],
            normal_2: [columns[2][0], columns[2][1], columns[2][2], 0.0],
            material_index: u32::try_from(slot)
                .map_err(|_| PacketRenderError::InvalidPacket("material slot overflowed".into()))?,
            padding: [0; 3],
        })
    }
}

impl MaterialUniform {
    fn from_material(material: &Material) -> Self {
        Self {
            base_color: [
                material.base_color[0],
                material.base_color[1],
                material.base_color[2],
                1.0,
            ],
            metallic_roughness: [material.metallic, material.roughness, 0.0, 0.0],
            emissive: [
                material.emissive[0],
                material.emissive[1],
                material.emissive[2],
                0.0,
            ],
        }
    }
}

fn make_globals(
    packet: &RenderPacket,
    camera: &Camera,
    width: u32,
    height: u32,
    manual_srgb: bool,
) -> GlobalsUniform {
    let direction = Vec3::from_array(packet.directional_light.direction).normalize();
    GlobalsUniform {
        view_projection: camera
            .view_projection(width as f32 / height as f32)
            .to_cols_array_2d(),
        camera_position: [
            camera.position[0],
            camera.position[1],
            camera.position[2],
            1.0,
        ],
        light_direction_intensity: [
            direction.x,
            direction.y,
            direction.z,
            packet.directional_light.intensity,
        ],
        light_color: [
            packet.directional_light.color[0],
            packet.directional_light.color[1],
            packet.directional_light.color[2],
            0.0,
        ],
        ambient_and_manual_srgb: [
            packet.ambient_color[0],
            packet.ambient_color[1],
            packet.ambient_color[2],
            if manual_srgb { 1.0 } else { 0.0 },
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        fs::File,
        io::BufReader,
        path::PathBuf,
        sync::Arc,
    };

    use daedalus_content::{
        project::{ProjectLimits, prepare_project_from_bytes},
        scene::StableId,
    };
    use daedalus_core::{
        EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
    };
    use glam::{EulerRot, Mat4, Quat};

    use super::{
        CacheEntryPlan, GpuInstance, MaterialUniform, SlotState, check_submission_admission,
        needs_instance_write, needs_material_write, packet_bookkeeping_bytes, plan_cache_admission,
        plan_entry_victims, plan_instance_slots,
    };
    use crate::{
        CaptureOptions, Material, Mesh, MeshHandle, MeshResourceLimits, PacketAssetLimits,
        PacketRenderError, PacketRendererLimits, ProjectRenderResources, RenderInstanceId,
        RenderObject, RenderPacket, RenderScene, Renderer, Vertex, packet::GeometryKey,
    };

    fn key(byte: u8) -> GeometryKey {
        let mut hash = [0; 32];
        hash[0] = byte;
        GeometryKey { hash, layout: 1 }
    }

    fn id(index: u32) -> RenderInstanceId {
        RenderInstanceId {
            entity: EntityHandle {
                session: SessionId::from_raw(5),
                index,
                generation: 1,
            },
            primitive: 0,
        }
    }

    fn small_limits() -> PacketRendererLimits {
        PacketRendererLimits {
            preferred_geometry_bytes: 10,
            hard_geometry_bytes: 20,
            max_active_meshes: 1,
            max_total_entries: 2,
            ..PacketRendererLimits::default()
        }
    }

    #[test]
    fn cache_planner_evicts_only_retired_completed_entries() {
        let a = key(1);
        let b = key(2);
        let c = key(3);
        let existing = HashMap::from([
            (
                a,
                CacheEntryPlan {
                    bytes: 10,
                    last_submission: 5,
                    last_touch: 9,
                },
            ),
            (
                b,
                CacheEntryPlan {
                    bytes: 10,
                    last_submission: 3,
                    last_touch: 2,
                },
            ),
        ]);
        let active = HashSet::from([a]);
        let requested = HashMap::from([(c, 10)]);
        assert_eq!(
            plan_cache_admission(&existing, &active, &requested, 4, small_limits()).unwrap(),
            vec![b]
        );

        let inflight = HashMap::from([
            (
                a,
                CacheEntryPlan {
                    bytes: 10,
                    last_submission: 5,
                    last_touch: 9,
                },
            ),
            (
                b,
                CacheEntryPlan {
                    bytes: 10,
                    last_submission: 5,
                    last_touch: 2,
                },
            ),
        ]);
        assert!(matches!(
            plan_cache_admission(&inflight, &active, &requested, 4, small_limits()),
            Err(PacketRenderError::ResidencyBusy)
        ));

        let want_existing = HashMap::from([(b, 10)]);
        assert!(
            plan_cache_admission(&existing, &active, &want_existing, 4, small_limits())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn asset_entry_planner_evicts_completed_entries_and_reports_pinned_capacity() {
        let requested = HashSet::from([3_u32]);
        assert_eq!(
            plan_entry_victims([(1, 7, 9), (2, 3, 2)].into_iter(), &requested, 2, 4,).unwrap(),
            vec![2]
        );
        assert!(matches!(
            plan_entry_victims([(1, 7, 9), (2, 7, 2)].into_iter(), &requested, 2, 4,),
            Err(PacketRenderError::ResidencyBusy)
        ));
        assert!(matches!(
            plan_entry_victims([(1, 0, 0)].into_iter(), &HashSet::from([2_u32, 3]), 1, 0,),
            Err(PacketRenderError::AssetLimitExceeded { .. })
        ));
    }

    #[test]
    fn submission_budget_counts_zero_write_frames_and_bytes() {
        assert!(check_submission_admission(0, 0, 128, 2, 3).is_ok());
        assert!(matches!(
            check_submission_admission(0, 0, 128, 3, 3),
            Err(PacketRenderError::ResidencyBusy)
        ));
        assert!(check_submission_admission(96, 32, 128, 0, 3).is_ok());
        assert!(matches!(
            check_submission_admission(97, 32, 128, 0, 3),
            Err(PacketRenderError::ResidencyBusy)
        ));
    }

    #[test]
    fn default_asset_bookkeeping_reservation_fits_its_cap() {
        let limits = PacketAssetLimits::default();
        assert!(
            packet_bookkeeping_bytes(PacketRendererLimits::default(), limits).unwrap()
                <= limits.bookkeeping_bytes
        );
    }

    #[test]
    fn slot_planner_preserves_live_ids_and_reuses_only_freed_slots() {
        let current = HashMap::from([(id(1), 0), (id(2), 1)]);
        let next = plan_instance_slots(&current, [id(2), id(3)], 2).unwrap();
        assert_eq!(next[&id(2)], 1);
        assert_eq!(next[&id(3)], 0);
        assert!(plan_instance_slots(&current, [id(2), id(3), id(4)], 2).is_err());
    }

    #[test]
    fn dirty_comparison_separates_instance_and_material_rows() {
        let instance = GpuInstance {
            model: glam::Mat4::IDENTITY.to_cols_array_2d(),
            normal_0: [1.0, 0.0, 0.0, 0.0],
            normal_1: [0.0, 1.0, 0.0, 0.0],
            normal_2: [0.0, 0.0, 1.0, 0.0],
            material_index: 0,
            padding: [0; 3],
        };
        let material = MaterialUniform::from_material(&Material::default());
        let old = SlotState {
            id: id(1),
            instance,
            material,
        };
        let same = SlotState {
            id: id(1),
            instance,
            material,
        };
        assert!(!needs_instance_write(Some(&old), &same));
        assert!(!needs_material_write(Some(&old), &same));

        let moved = SlotState {
            id: id(1),
            instance: GpuInstance {
                model: glam::Mat4::from_translation(glam::Vec3::X).to_cols_array_2d(),
                ..instance
            },
            material,
        };
        assert!(needs_instance_write(Some(&old), &moved));
        assert!(!needs_material_write(Some(&old), &moved));

        let changed_material = SlotState {
            id: id(1),
            instance,
            material: MaterialUniform::from_material(&Material {
                base_color: [0.1, 0.2, 0.3],
                ..Material::default()
            }),
        };
        assert!(!needs_instance_write(Some(&old), &changed_material));
        assert!(needs_material_write(Some(&old), &changed_material));
    }

    fn external_project() -> daedalus_content::project::PreparedProject {
        let scene = br#"{"schema_version":1,"revision":1,"entities":[
            {"id":"entity:a","components":[{"id":"component:transform:a","type_id":"daedalus.transform","data":{"translation":[0.0,0.0,0.0],"rotation":[0.0,0.0,0.0,1.0],"scale":[1.0,1.0,1.0]}},{"id":"component:mesh:a","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle","material":{"base_color":[0.7,0.2,0.1],"metallic":0.0,"roughness":0.6,"emissive":[0.0,0.0,0.0]}}}]},
            {"id":"entity:b","components":[{"id":"component:transform:b","type_id":"daedalus.transform","data":{"translation":[1.5,0.0,0.0],"rotation":[0.0,0.0,0.0,1.0],"scale":[1.0,1.0,1.0]}},{"id":"component:mesh:b","type_id":"daedalus.mesh_renderer","asset_refs":["asset:triangle"],"data":{"mesh":"asset:triangle","material":{"base_color":[0.1,0.3,0.8],"metallic":0.0,"roughness":0.7,"emissive":[0.0,0.0,0.0]}}}]}
        ],"assets":[{"id":"asset:triangle","kind":"daedalus.mesh","uri":"triangle.json","metadata":{}}]}"#;
        let manifest = br#"{"schema_version":1,"name":"packet GPU test","scene":"scene.json","presentation":{"camera":{"position":[0.0,0.0,5.0],"target":[0.0,0.0,0.0],"up":[0.0,1.0,0.0],"vertical_fov_radians":0.9,"near":0.1,"far":100.0}}}"#;
        let mesh = br#"{"schema_version":1,"vertices":[{"position":[-0.5,-0.5,0.0],"normal":[0.0,0.0,1.0]},{"position":[0.5,-0.5,0.0],"normal":[0.0,0.0,1.0]},{"position":[0.0,0.5,0.0],"normal":[0.0,0.0,1.0]}],"indices":[0,1,2]}"#;
        prepare_project_from_bytes(
            manifest,
            scene,
            &BTreeMap::from([("triangle.json".to_owned(), mesh.to_vec())]),
            ProjectLimits::default(),
        )
        .unwrap()
    }

    fn capture_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("daedalus-packet-{name}-{}.png", std::process::id()))
    }

    fn decoded_rgba(path: &PathBuf) -> Vec<u8> {
        let decoder = png::Decoder::new(BufReader::new(File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut data = vec![0; reader.output_buffer_size().expect("bounded capture buffer")];
        let frame = reader.next_frame(&mut data).unwrap();
        data.truncate(frame.buffer_size());
        data
    }

    #[test]
    #[ignore = "requires a native wgpu adapter; asserts real device writes and image output"]
    fn packet_gpu_reuse_matches_legacy_capture_and_dirty_counters() {
        let project = external_project();
        let session = SessionId::from_raw(77);
        let bindings: BTreeMap<_, _> = project
            .entities()
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                (
                    entity.id.clone(),
                    EntityHandle {
                        session,
                        index: index as u32,
                        generation: 4,
                    },
                )
            })
            .collect();
        let resources = ProjectRenderResources::prepare(
            &project,
            6,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        let make_snapshot = |sequence, tick, second_translation| RenderSnapshot {
            sequence,
            tick: Tick(tick),
            entities: vec![
                RenderEntitySnapshot {
                    entity: bindings[&StableId::new("entity:a").unwrap()],
                    transform: Transform::IDENTITY,
                },
                RenderEntitySnapshot {
                    entity: bindings[&StableId::new("entity:b").unwrap()],
                    transform: Transform {
                        translation: [second_translation, 0.0, 0.0],
                        rotation: Quat::from_euler(EulerRot::XYZ, 0.55, -0.38, 0.12).to_array(),
                        scale: [0.75, 1.3, 0.6],
                    },
                },
            ]
            .into(),
        };
        let initial = Arc::new(resources.extract(&make_snapshot(1, 1, 1.5), None).unwrap());
        let mut renderer = Renderer::new_offscreen_blocking()
            .unwrap()
            .into_packet_renderer(PacketRendererLimits::default())
            .unwrap();
        let packet_path = capture_path("initial");
        let legacy_path = capture_path("legacy");
        let options = CaptureOptions::new(96, 64, &packet_path);
        renderer.render_packet_png(&initial, &options).unwrap();
        let first = renderer.resource_stats();
        assert_eq!(first.geometry_upload_count, 1);
        assert_eq!(first.active_entries, 1);
        assert_eq!(first.instance_write_count, 2);
        assert_eq!(first.material_write_count, 2);

        let mut legacy = RenderScene::new();
        legacy.camera = initial.camera().clone();
        legacy.directional_light = initial.directional_light().clone();
        legacy.ambient_color = initial.ambient_color();
        legacy.clear_color = initial.clear_color();
        let legacy_mesh = Mesh::new(
            vec![
                Vertex::new([-0.5, -0.5, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([0.5, -0.5, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([0.0, 0.5, 0.0], [0.0, 0.0, 1.0]),
            ],
            vec![0, 1, 2],
        );
        for instance in initial.instances() {
            legacy.objects.push(RenderObject::new(
                legacy_mesh.clone(),
                instance.transform,
                instance.material.clone(),
            ));
        }
        renderer
            .renderer
            .render_png(&legacy, &CaptureOptions::new(96, 64, &legacy_path))
            .unwrap();
        let packet_pixels = decoded_rgba(&packet_path);
        let legacy_pixels = decoded_rgba(&legacy_path);
        let mut color_counts = HashMap::<[u8; 4], usize>::new();
        for pixel in packet_pixels.chunks_exact(4) {
            *color_counts
                .entry(pixel.try_into().expect("four color channels"))
                .or_default() += 1;
        }
        let total_pixels = packet_pixels.len() / 4;
        let dominant_color_pixels = color_counts.values().copied().max().unwrap_or_default();
        assert!(
            color_counts.len() >= 3 && total_pixels.saturating_sub(dominant_color_pixels) > 20,
            "capture should contain visible material-colored foreground, colors={}, foreground_pixels={}",
            color_counts.len(),
            total_pixels.saturating_sub(dominant_color_pixels)
        );
        assert_eq!(packet_pixels.len(), legacy_pixels.len());
        assert!(
            packet_pixels
                .iter()
                .zip(&legacy_pixels)
                .all(|(a, b)| a.abs_diff(*b) <= 2)
        );
        eprintln!(
            "packet capture={} legacy capture={} initial stats={first:?}",
            packet_path.display(),
            legacy_path.display()
        );

        let before_repeat = renderer.resource_stats();
        renderer
            .render_packet_png(&initial, &CaptureOptions::new(96, 64, &packet_path))
            .unwrap();
        let after_repeat = renderer.resource_stats();
        assert_eq!(
            after_repeat.geometry_upload_count,
            before_repeat.geometry_upload_count
        );
        assert_eq!(
            after_repeat.instance_write_count,
            before_repeat.instance_write_count
        );
        assert_eq!(
            after_repeat.material_write_count,
            before_repeat.material_write_count
        );

        let mut generation_instances = initial.instances().to_vec();
        let first_handle = &generation_instances[0].mesh;
        let same_payload_new_generation = MeshHandle::new(
            first_handle.asset_id().clone(),
            first_handle.generation() + 1,
            legacy_mesh.clone(),
            MeshResourceLimits::default(),
        )
        .unwrap();
        for instance in &mut generation_instances {
            instance.mesh = same_payload_new_generation.clone();
        }
        let generation_only = initial.with_instances(generation_instances);
        let before_generation = renderer.resource_stats();
        renderer
            .render_packet_png(&generation_only, &CaptureOptions::new(96, 64, &packet_path))
            .unwrap();
        let after_generation = renderer.resource_stats();
        assert_eq!(
            after_generation.geometry_upload_count,
            before_generation.geometry_upload_count
        );
        assert_eq!(
            after_generation.instance_write_count,
            before_generation.instance_write_count
        );
        assert_eq!(
            after_generation.material_write_count,
            before_generation.material_write_count
        );

        let mut moved_instances = initial.instances().to_vec();
        moved_instances[0].transform = Mat4::from_translation(glam::Vec3::new(0.0, 0.4, 0.0));
        let moved = initial.with_instances(moved_instances);
        let before_move = renderer.resource_stats();
        renderer
            .render_packet_png(&moved, &CaptureOptions::new(96, 64, &packet_path))
            .unwrap();
        let after_move = renderer.resource_stats();
        assert_eq!(
            after_move.geometry_upload_count,
            before_move.geometry_upload_count
        );
        assert_eq!(
            after_move.instance_write_count - before_move.instance_write_count,
            1
        );
        assert_eq!(
            after_move.material_write_count,
            before_move.material_write_count
        );

        let mut material_instances = moved.instances().to_vec();
        material_instances[1].material.base_color = [0.9, 0.1, 0.2];
        let material = moved.with_instances(material_instances);
        let before_material = renderer.resource_stats();
        renderer
            .render_packet_png(&material, &CaptureOptions::new(96, 64, &packet_path))
            .unwrap();
        let after_material = renderer.resource_stats();
        assert_eq!(
            after_material.geometry_upload_count,
            before_material.geometry_upload_count
        );
        assert_eq!(
            after_material.instance_write_count,
            before_material.instance_write_count
        );
        assert_eq!(
            after_material.material_write_count - before_material.material_write_count,
            1
        );
        assert_eq!(
            after_material.depth_target_recreation_count,
            first.depth_target_recreation_count
        );

        let mut changed_vertices = legacy_mesh.vertices.clone();
        changed_vertices[0].position[0] -= 0.2;
        let changed_mesh = Mesh::new(changed_vertices, legacy_mesh.indices.clone());
        let changed_handle = MeshHandle::new(
            StableId::new("asset:triangle").unwrap(),
            same_payload_new_generation.generation() + 1,
            changed_mesh,
            MeshResourceLimits::default(),
        )
        .unwrap();
        let mut changed_mesh_instances = material.instances().to_vec();
        for instance in &mut changed_mesh_instances {
            instance.mesh = changed_handle.clone();
        }
        let changed_payload = material.with_instances(changed_mesh_instances);
        let before_changed_payload = renderer.resource_stats();
        renderer
            .render_packet_png(&changed_payload, &CaptureOptions::new(96, 64, &packet_path))
            .unwrap();
        let after_changed_payload = renderer.resource_stats();
        assert_eq!(
            after_changed_payload.geometry_upload_count
                - before_changed_payload.geometry_upload_count,
            1
        );
        assert_eq!(after_changed_payload.active_entries, 1);
        assert_eq!(after_changed_payload.retired_entries, 1);

        let mut invalid_instances = changed_payload.instances().to_vec();
        invalid_instances[0].transform = Mat4::from_scale(glam::Vec3::new(0.0, 1.0, 1.0));
        let invalid = changed_payload.with_instances(invalid_instances);
        let before_invalid = renderer.resource_stats();
        assert!(matches!(
            renderer.render_packet_png(&invalid, &CaptureOptions::new(96, 64, &packet_path)),
            Err(PacketRenderError::InvalidPacket(_))
        ));
        let after_invalid = renderer.resource_stats();
        assert_eq!(
            after_invalid.geometry_upload_count,
            before_invalid.geometry_upload_count
        );
        assert_eq!(
            after_invalid.instance_write_count,
            before_invalid.instance_write_count
        );
        assert_eq!(
            after_invalid.material_write_count,
            before_invalid.material_write_count
        );
        assert_eq!(renderer.ready_packet.as_deref(), Some(&changed_payload));

        renderer
            .render_packet_png(&changed_payload, &CaptureOptions::new(80, 80, &packet_path))
            .unwrap();
        assert_eq!(
            renderer.resource_stats().depth_target_recreation_count,
            first.depth_target_recreation_count + 1
        );
        eprintln!(
            "final packet renderer stats={:?}",
            renderer.resource_stats()
        );
    }

    #[test]
    #[ignore = "requires a native wgpu adapter; checks bounded submission backpressure"]
    fn asset_backpressure_preserves_ready_packet_and_does_not_count_busy_candidate_as_drawn() {
        let project = external_project();
        let session = SessionId::from_raw(88);
        let bindings: BTreeMap<_, _> = project
            .entities()
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                (
                    entity.id.clone(),
                    EntityHandle {
                        session,
                        index: index as u32,
                        generation: 5,
                    },
                )
            })
            .collect();
        let resources = ProjectRenderResources::prepare(
            &project,
            7,
            session,
            &bindings,
            PacketRendererLimits::default(),
        )
        .unwrap();
        let snapshot = |sequence, tick, second_x| RenderSnapshot {
            sequence,
            tick: Tick(tick),
            entities: vec![
                RenderEntitySnapshot {
                    entity: bindings[&StableId::new("entity:a").unwrap()],
                    transform: Transform::IDENTITY,
                },
                RenderEntitySnapshot {
                    entity: bindings[&StableId::new("entity:b").unwrap()],
                    transform: Transform {
                        translation: [second_x, 0.0, 0.0],
                        rotation: Quat::IDENTITY.to_array(),
                        scale: [1.0; 3],
                    },
                },
            ]
            .into(),
        };
        let initial = Arc::new(resources.extract(&snapshot(1, 1, 1.5), None).unwrap());
        let unchanged = Arc::new(resources.extract(&snapshot(2, 2, 1.5), None).unwrap());
        let moved = Arc::new(resources.extract(&snapshot(3, 3, 1.75), None).unwrap());
        let asset_limits = PacketAssetLimits {
            upload_staging_bytes: 4096,
            ..PacketAssetLimits::default()
        };
        let mut renderer = Renderer::new_offscreen_blocking()
            .unwrap()
            .into_packet_renderer_with_asset_limits(PacketRendererLimits::default(), asset_limits)
            .unwrap();
        let target = renderer.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("Daedalus bounded packet admission test target"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        renderer
            .render_packet_view(&view, 64, 64, Arc::clone(&initial), None, None)
            .unwrap();

        let initial_stats = renderer.resource_stats();
        let initial_asset_stats = renderer.asset_resource_stats();
        let initial_slots = renderer.slots.clone();
        let invalid = {
            let mut instances = unchanged.instances().to_vec();
            instances[0].transform = Mat4::from_scale(glam::Vec3::ZERO);
            Arc::new(RenderPacket::new(
                unchanged.session(),
                unchanged.tick(),
                unchanged.sequence(),
                unchanged.project_generation(),
                unchanged.source_digest().to_owned(),
                unchanged.camera().clone(),
                unchanged.directional_light().clone(),
                unchanged.ambient_color(),
                unchanged.clear_color(),
                instances,
            ))
        };
        assert!(matches!(
            renderer.render_packet_view(&view, 64, 64, invalid, None, None),
            Err(PacketRenderError::InvalidPacket(_))
        ));
        assert!(Arc::ptr_eq(
            renderer.ready_packet.as_ref().unwrap(),
            &initial
        ));
        assert_eq!(renderer.slots, initial_slots);
        assert_eq!(
            renderer.resource_stats().instance_write_count,
            initial_stats.instance_write_count
        );
        assert_eq!(
            renderer.resource_stats().material_write_count,
            initial_stats.material_write_count
        );
        assert_eq!(
            renderer.asset_resource_stats().upload_staging_write_count,
            initial_asset_stats.upload_staging_write_count
        );

        renderer.staging_submissions.clear();
        renderer.staging_in_flight_bytes = 0;
        renderer.completion_watermark.store(
            renderer.submission_serial,
            std::sync::atomic::Ordering::Release,
        );
        let future = renderer.submission_serial + 100;
        for offset in 0..renderer.asset_limits.max_in_flight_submissions {
            renderer
                .staging_submissions
                .push_back((future + offset as u64, 0));
        }
        assert!(matches!(
            renderer.render_packet_view(&view, 64, 64, Arc::clone(&unchanged), None, None),
            Err(PacketRenderError::ResidencyBusy)
        ));
        assert!(Arc::ptr_eq(
            renderer.ready_packet.as_ref().unwrap(),
            &initial
        ));
        assert_eq!(renderer.slots, initial_slots);
        assert_eq!(renderer.asset_resource_stats().in_flight_submissions, 3);
        assert_eq!(
            renderer.resource_stats().instance_write_count,
            initial_stats.instance_write_count
        );

        renderer.completion_watermark.store(
            future + renderer.asset_limits.max_in_flight_submissions as u64 - 1,
            std::sync::atomic::Ordering::Release,
        );
        renderer
            .render_packet_view(&view, 64, 64, Arc::clone(&unchanged), None, None)
            .unwrap();
        assert!(Arc::ptr_eq(
            renderer.ready_packet.as_ref().unwrap(),
            &unchanged
        ));
        assert_eq!(
            renderer.resource_stats().instance_write_count,
            initial_stats.instance_write_count
        );
        assert_eq!(
            renderer.resource_stats().material_write_count,
            initial_stats.material_write_count
        );

        let staging_serial = future + 100;
        let staging_capacity = renderer.asset_limits.upload_staging_bytes;
        renderer
            .staging_submissions
            .push_back((staging_serial, staging_capacity));
        renderer.staging_in_flight_bytes = staging_capacity;
        let submitted = renderer
            .render_packet_view(&view, 64, 64, Arc::clone(&moved), None, None)
            .unwrap();
        assert!(Arc::ptr_eq(&submitted, &unchanged));
        assert!(Arc::ptr_eq(
            renderer.ready_packet.as_ref().unwrap(),
            &unchanged
        ));
        assert_eq!(
            renderer
                .asset_resource_stats()
                .upload_staging_in_flight_bytes,
            staging_capacity
        );
        assert!(renderer.asset_resource_stats().pending_candidate);
        assert_eq!(
            renderer.resource_stats().instance_write_count,
            initial_stats.instance_write_count
        );

        renderer
            .completion_watermark
            .store(staging_serial, std::sync::atomic::Ordering::Release);
        renderer.poll_retirement().unwrap();
        let before_move = renderer.resource_stats();
        renderer
            .render_packet_view(&view, 64, 64, Arc::clone(&moved), None, None)
            .unwrap();
        let after_move = renderer.resource_stats();
        assert_eq!(
            after_move.geometry_upload_count,
            before_move.geometry_upload_count
        );
        assert_eq!(
            after_move.instance_write_count - before_move.instance_write_count,
            1
        );
        assert_eq!(
            after_move.material_write_count,
            before_move.material_write_count
        );
        assert_eq!(
            renderer
                .asset_resource_stats()
                .upload_staging_in_flight_bytes,
            128
        );
    }
}
