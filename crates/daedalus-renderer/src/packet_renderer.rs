//! Persistent device-local geometry and instance resources for render packets.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Vec3};
use wgpu::util::DeviceExt;

use crate::{
    Camera, CaptureArtifact, CaptureOptions, Material, PacketRenderError, PacketRendererLimits,
    RenderInstance, RenderInstanceId, RenderPacket, Renderer, RendererError,
    packet::{GeometryKey, PacketValidation},
    renderer::{GlobalsUniform, MaterialUniform, OverlayUniform},
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

struct GeometryResource {
    vertex: wgpu::Buffer,
    index: wgpu::Buffer,
    index_count: u32,
    bytes: usize,
    last_submission: u64,
    last_touch: u64,
}

struct SlotState {
    id: RenderInstanceId,
    transform: glam::Mat4,
    instance: GpuInstance,
    material: MaterialUniform,
}

struct PreparedSlotUpdate {
    planned: HashMap<RenderInstanceId, usize>,
    states: Vec<(usize, SlotState)>,
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
    geometry: HashMap<GeometryKey, GeometryResource>,
    active_keys: HashSet<GeometryKey>,
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
    submission_serial: u64,
    completion_watermark: Arc<AtomicU64>,
    touch_serial: u64,
}

impl PacketRenderer {
    pub(crate) fn new(
        renderer: Renderer,
        limits: PacketRendererLimits,
    ) -> Result<Self, PacketRenderError> {
        limits.validate()?;
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
            geometry: HashMap::new(),
            active_keys: HashSet::new(),
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
            submission_serial: 0,
            completion_watermark: Arc::new(AtomicU64::new(0)),
            touch_serial: 0,
        })
    }

    pub(crate) fn device(&self) -> &wgpu::Device {
        &self.renderer.device
    }

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
        if !unchanged && let Err(error) = self.try_activate_packet(Arc::clone(&packet)) {
            if matches!(error, PacketRenderError::ResidencyBusy) {
                self.pending_packet = Some(packet);
                self.stats.pending_candidate_count =
                    self.stats.pending_candidate_count.saturating_add(1);
            } else {
                return Err(error);
            }
        }
        let Some(active) = self.ready_packet.as_ref().cloned() else {
            return Err(PacketRenderError::InvalidPacket(
                "no ready packet is available".into(),
            ));
        };
        let camera = camera_override.unwrap_or(active.camera());
        self.encode_ready(view, width, height, &active, camera, loading_elapsed)?;
        Ok(active)
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
        self.try_activate_packet(Arc::clone(&packet))?;
        let active = self.ready_packet.as_ref().cloned().ok_or_else(|| {
            PacketRenderError::InvalidPacket("no ready packet is available".into())
        })?;
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
        self.encode_ready(
            &view,
            options.width,
            options.height,
            &active,
            active.camera(),
            None,
        )?;
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

    fn try_activate_packet(&mut self, packet: Arc<RenderPacket>) -> Result<(), PacketRenderError> {
        let previous = self
            .pending_packet
            .as_deref()
            .or(self.ready_packet.as_deref());
        let validation = packet.validate_reusing(self.limits, previous)?;
        let slot_update = self.prepare_slot_update(&packet, &validation)?;
        self.ensure_geometry(&validation)?;
        self.commit_slot_update(slot_update, &validation);
        self.active_keys = validation.geometries.keys().copied().collect();
        self.ready_packet = Some(Arc::clone(&packet));
        if self
            .pending_packet
            .as_ref()
            .is_some_and(|pending| pending.sequence() <= packet.sequence())
        {
            self.pending_packet = None;
        }
        Ok(())
    }

    fn ensure_geometry(&mut self, validation: &PacketValidation) -> Result<(), PacketRenderError> {
        let missing: Vec<_> = validation
            .geometries
            .iter()
            .filter(|(key, _)| !self.geometry.contains_key(key))
            .map(|(key, mesh)| (*key, mesh.clone()))
            .collect();
        for key in validation.geometries.keys() {
            if self.geometry.contains_key(key) {
                self.stats.cache_hit_count = self.stats.cache_hit_count.saturating_add(1);
            } else {
                self.stats.cache_miss_count = self.stats.cache_miss_count.saturating_add(1);
            }
        }
        let existing_plan: HashMap<_, _> = self
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
        let requested: HashMap<_, _> = validation
            .geometries
            .iter()
            .map(|(key, mesh)| (*key, mesh.gpu_bytes()))
            .collect();
        let victims = plan_cache_admission(
            &existing_plan,
            &self.active_keys,
            &requested,
            self.completion_watermark.load(Ordering::Acquire),
            self.limits,
        )?;
        for key in victims {
            if self.geometry.remove(&key).is_some() {
                self.stats.cache_eviction_count = self.stats.cache_eviction_count.saturating_add(1);
            }
        }
        for (key, mesh) in missing {
            let vertex_bytes: Vec<GpuVertex> = mesh
                .mesh()
                .vertices
                .iter()
                .map(|v| GpuVertex {
                    position: v.position,
                    normal: v.normal,
                })
                .collect();
            let vertex =
                self.renderer
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Daedalus cached immutable packet vertices"),
                        contents: bytemuck::cast_slice(&vertex_bytes),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
            let index =
                self.renderer
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("Daedalus cached immutable packet indices"),
                        contents: bytemuck::cast_slice(&mesh.mesh().indices),
                        usage: wgpu::BufferUsages::INDEX,
                    });
            let index_count = u32::try_from(mesh.index_count()).map_err(|_| {
                PacketRenderError::InvalidPacket("index count exceeds draw range".into())
            })?;
            self.geometry.insert(
                key,
                GeometryResource {
                    vertex,
                    index,
                    index_count,
                    bytes: mesh.gpu_bytes(),
                    last_submission: 0,
                    last_touch: self.touch_serial,
                },
            );
            self.stats.geometry_upload_count = self.stats.geometry_upload_count.saturating_add(1);
            self.stats.geometry_upload_bytes = self
                .stats
                .geometry_upload_bytes
                .saturating_add(mesh.gpu_bytes() as u64);
        }
        Ok(())
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
        let mut states = Vec::with_capacity(packet.instances().len());
        for instance in packet.instances() {
            let slot = planned[&instance.id];
            let previous = self.slot_state[slot].as_ref();
            let gpu_instance = if let Some(previous) =
                previous.filter(|old| old.transform == instance.transform)
            {
                previous.instance
            } else {
                let normal = validation
                    .normal_matrices
                    .get(&instance.id)
                    .copied()
                    .ok_or_else(|| {
                        PacketRenderError::InvalidPacket(
                            "validated instance normal matrix is missing".into(),
                        )
                    })?;
                GpuInstance::new(instance, slot, normal)?
            };
            let material = MaterialUniform::from_material(&instance.material);
            states.push((
                slot,
                SlotState {
                    id: instance.id,
                    transform: instance.transform,
                    instance: gpu_instance,
                    material,
                },
            ));
        }
        Ok(PreparedSlotUpdate { planned, states })
    }

    fn commit_slot_update(&mut self, update: PreparedSlotUpdate, validation: &PacketValidation) {
        let mut next_slots = HashSet::with_capacity(update.states.len());
        for (slot, next) in update.states {
            let previous = self.slot_state[slot].as_ref();
            if needs_instance_write(previous, &next) {
                self.renderer.queue.write_buffer(
                    &self.instance_buffer,
                    (slot * std::mem::size_of::<GpuInstance>()) as u64,
                    bytemuck::bytes_of(&next.instance),
                );
                self.stats.instance_write_count = self.stats.instance_write_count.saturating_add(1);
                self.stats.instance_write_bytes = self
                    .stats
                    .instance_write_bytes
                    .saturating_add(std::mem::size_of::<GpuInstance>() as u64);
            }
            if needs_material_write(previous, &next) {
                self.renderer.queue.write_buffer(
                    &self.material_buffer,
                    (slot * std::mem::size_of::<MaterialUniform>()) as u64,
                    bytemuck::bytes_of(&next.material),
                );
                self.stats.material_write_count = self.stats.material_write_count.saturating_add(1);
                self.stats.material_write_bytes = self
                    .stats
                    .material_write_bytes
                    .saturating_add(std::mem::size_of::<MaterialUniform>() as u64);
            }
            self.slot_state[slot] = Some(next);
            next_slots.insert(slot);
        }
        for (slot, state) in self.slot_state.iter_mut().enumerate() {
            if !next_slots.contains(&slot) {
                *state = None;
            }
        }
        self.slots = update.planned;
        for key in validation.geometries.keys() {
            if let Some(entry) = self.geometry.get_mut(key) {
                self.touch_serial = self.touch_serial.saturating_add(1);
                entry.last_touch = self.touch_serial;
            }
        }
    }

    fn encode_ready(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        packet: &RenderPacket,
        camera: &Camera,
        loading_elapsed: Option<f32>,
    ) -> Result<(), PacketRenderError> {
        let globals = make_globals(packet, camera, width, height, self.renderer.manual_srgb);
        if self.last_globals != Some(globals) {
            self.renderer
                .queue
                .write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
            self.stats.global_write_count = self.stats.global_write_count.saturating_add(1);
            self.stats.global_write_bytes = self
                .stats
                .global_write_bytes
                .saturating_add(std::mem::size_of::<GlobalsUniform>() as u64);
            self.last_globals = Some(globals);
        }
        self.ensure_depth(width, height);
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
            pass.set_pipeline(&self.renderer.packet_mesh_pipeline);
            pass.set_bind_group(0, &self.globals_bind_group, &[]);
            pass.set_bind_group(1, &self.materials_bind_group, &[]);
            pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
            for instance in packet.instances() {
                let slot = self.slots[&instance.id];
                let Some(resource) = self.geometry.get(&instance.mesh.key()) else {
                    return Err(PacketRenderError::InvalidPacket(
                        "active packet mesh was evicted".into(),
                    ));
                };
                pass.set_vertex_buffer(0, resource.vertex.slice(..));
                pass.set_index_buffer(resource.index.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..resource.index_count, 0, slot as u32..slot as u32 + 1);
            }
        }
        if let Some(elapsed) = loading_elapsed {
            self.renderer.queue.write_buffer(
                &self.renderer.overlay_globals,
                0,
                bytemuck::bytes_of(&OverlayUniform {
                    time: elapsed,
                    width: width as f32,
                    height: height as f32,
                    manual_srgb: if self.renderer.manual_srgb { 1.0 } else { 0.0 },
                }),
            );
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
        let submission = self.renderer.queue.submit([encoder.finish()]);
        self.note_submission(submission);
        for instance in packet.instances() {
            if let Some(entry) = self.geometry.get_mut(&instance.mesh.key()) {
                entry.last_submission = self.submission_serial;
            }
        }
        Ok(())
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

    fn note_submission(&mut self, _submission: wgpu::SubmissionIndex) {
        self.submission_serial = self.submission_serial.saturating_add(1);
        let serial = self.submission_serial;
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
        Ok(())
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
        CacheEntryPlan, GpuInstance, MaterialUniform, SlotState, needs_instance_write,
        needs_material_write, plan_cache_admission, plan_instance_slots,
    };
    use crate::{
        CaptureOptions, Material, Mesh, MeshHandle, MeshResourceLimits, PacketRenderError,
        PacketRendererLimits, ProjectRenderResources, RenderInstanceId, RenderObject, RenderScene,
        Renderer, Vertex, packet::GeometryKey,
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
            transform: glam::Mat4::IDENTITY,
            instance,
            material,
        };
        let same = SlotState {
            id: id(1),
            transform: glam::Mat4::IDENTITY,
            instance,
            material,
        };
        assert!(!needs_instance_write(Some(&old), &same));
        assert!(!needs_material_write(Some(&old), &same));

        let moved = SlotState {
            id: id(1),
            transform: glam::Mat4::from_translation(glam::Vec3::X),
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
            transform: glam::Mat4::IDENTITY,
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
}
