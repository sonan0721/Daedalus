use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Vec3};
use wgpu::util::DeviceExt;

use crate::{PacketRenderError, PacketRenderer, PacketRendererLimits, RenderScene, scene::Vertex};

const PNG_ROW_ALIGNMENT: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
const MAP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClearAlphaPolicy {
    Preserve,
    Opaque,
    Premultiplied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PngAlphaRepresentation {
    Straight,
    SrgbPremultiplied,
}

pub(crate) fn target_clear_color(
    source: [f32; 4],
    manual_srgb: bool,
    alpha_policy: ClearAlphaPolicy,
) -> wgpu::Color {
    let mut target = source;
    match alpha_policy {
        ClearAlphaPolicy::Preserve => {}
        ClearAlphaPolicy::Opaque => target[3] = 1.0,
        ClearAlphaPolicy::Premultiplied => {
            target[0] *= target[3];
            target[1] *= target[3];
            target[2] *= target[3];
        }
    }
    if manual_srgb {
        for channel in &mut target[..3] {
            *channel = linear_to_srgb(*channel);
        }
    }
    wgpu::Color {
        r: f64::from(target[0]),
        g: f64::from(target[1]),
        b: f64::from(target[2]),
        a: f64::from(target[3]),
    }
}

fn linear_to_srgb(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_to_linear(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn unpremultiply_srgb_rgba_in_place(rgba: &mut [u8]) {
    debug_assert!(rgba.len().is_multiple_of(4));
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha != u8::MAX {
            let alpha = f32::from(alpha) / f32::from(u8::MAX);
            for channel in &mut pixel[..3] {
                let premultiplied = f32::from(*channel) / f32::from(u8::MAX);
                let straight = (srgb_to_linear(premultiplied) / alpha).clamp(0.0, 1.0);
                *channel = (linear_to_srgb(straight) * f32::from(u8::MAX)).round() as u8;
            }
        }
    }
}

fn apply_png_alpha_representation(rgba: &mut [u8], representation: PngAlphaRepresentation) {
    if representation == PngAlphaRepresentation::SrgbPremultiplied {
        unpremultiply_srgb_rgba_in_place(rgba);
    }
}

#[derive(Debug)]
pub enum RendererError {
    NoAdapter(String),
    RequestDevice(String),
    InitializationIncomplete,
    InvalidScene(String),
    InvalidCaptureSize {
        width: u32,
        height: u32,
        maximum: u32,
    },
    CaptureBufferTooLarge,
    Poll(String),
    Map(String),
    Png(png::EncodingError),
    Io(io::Error),
    #[cfg(feature = "window")]
    Window(String),
    #[cfg(feature = "window")]
    SurfaceOutOfMemory,
}

impl std::fmt::Display for RendererError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter(message) => write!(f, "no compatible wgpu adapter: {message}"),
            Self::RequestDevice(message) => write!(f, "could not create wgpu device: {message}"),
            Self::InitializationIncomplete => {
                write!(f, "renderer initialization is incomplete")
            }
            Self::InvalidScene(message) => write!(f, "invalid render scene: {message}"),
            Self::InvalidCaptureSize {
                width,
                height,
                maximum,
            } => write!(
                f,
                "capture size {width}x{height} is zero or exceeds the device limit {maximum}"
            ),
            Self::CaptureBufferTooLarge => write!(
                f,
                "capture readback buffer size overflowed the device limits"
            ),
            Self::Poll(message) => write!(f, "GPU submission did not complete: {message}"),
            Self::Map(message) => write!(f, "GPU readback mapping failed: {message}"),
            Self::Png(error) => write!(f, "PNG encoding failed: {error}"),
            Self::Io(error) => write!(f, "capture file I/O failed: {error}"),
            #[cfg(feature = "window")]
            Self::Window(message) => write!(f, "window renderer failed: {message}"),
            #[cfg(feature = "window")]
            Self::SurfaceOutOfMemory => write!(f, "window surface ran out of memory"),
        }
    }
}

impl std::error::Error for RendererError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Png(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RendererError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<png::EncodingError> for RendererError {
    fn from(value: png::EncodingError) -> Self {
        Self::Png(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RendererInfo {
    pub adapter_name: String,
    pub backend: String,
    pub device_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
    pub width: u32,
    pub height: u32,
    pub output: PathBuf,
}

impl CaptureOptions {
    pub fn new(width: u32, height: u32, output: impl Into<PathBuf>) -> Self {
        Self {
            width,
            height,
            output: output.into(),
        }
    }
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self::new(640, 360, "capture.png")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureArtifact {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub rgba8_bytes: usize,
    pub renderer: RendererInfo,
}

/// An initialized GPU renderer with an offscreen RGBA8-sRGB target format.
/// Windowed presentation uses a second renderer created for the surface's
/// actual format. Readback and PNG encoding happen only in `capture_png`.
pub struct Renderer {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) info: RendererInfo,
    pub(crate) format: wgpu::TextureFormat,
    pub(crate) manual_srgb: bool,
    pub(crate) globals_layout: wgpu::BindGroupLayout,
    pub(crate) materials_layout: wgpu::BindGroupLayout,
    pub(crate) mesh_pipeline: wgpu::RenderPipeline,
    pub(crate) packet_mesh_pipeline: wgpu::RenderPipeline,
    pub(crate) model_packet_mesh_pipeline: Option<wgpu::RenderPipeline>,
    pub(crate) sprite_packet_pipeline: Option<wgpu::RenderPipeline>,
    pub(crate) model_textures_layout: Option<wgpu::BindGroupLayout>,
    pub(crate) overlay_pipeline: wgpu::RenderPipeline,
    pub(crate) overlay_globals: wgpu::Buffer,
    pub(crate) overlay_bind_group: wgpu::BindGroup,
    pub(crate) maximum_dimension: u32,
    pub(crate) maximum_buffer_size: u64,
    pub(crate) maximum_storage_binding_size: u64,
    init_handles: RendererInitHandles,
    retain_lazy_init_handles: bool,
    #[cfg(test)]
    lazy_init_panic_point: Option<LazyInitPanicPoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LazyInitPanicPoint {
    ModelAfterLayout,
    ModelAfterShader,
}

#[cfg(test)]
fn panic_lazy_init_if_requested(
    pending: &mut Option<LazyInitPanicPoint>,
    point: LazyInitPanicPoint,
) {
    if *pending == Some(point) {
        *pending = None;
        panic!("injected lazy pipeline initialization panic");
    }
}

/// Fixed temporary-handle retention for managed initialization. These handles
/// are not needed by the pipelines after creation, but managed owners keep them
/// until the designated GPU cleanup worker drops the renderer.
#[derive(Default)]
struct RendererInitHandles {
    _base_shader: Option<wgpu::ShaderModule>,
    _mesh_pipeline_layout: Option<wgpu::PipelineLayout>,
    _overlay_shader: Option<wgpu::ShaderModule>,
    _overlay_layout: Option<wgpu::BindGroupLayout>,
    _overlay_pipeline_layout: Option<wgpu::PipelineLayout>,
    _model_shader: Option<wgpu::ShaderModule>,
    _model_pipeline_layout: Option<wgpu::PipelineLayout>,
    _sprite_shader: Option<wgpu::ShaderModule>,
    _sprite_pipeline_layout: Option<wgpu::PipelineLayout>,
}

/// Renderer initialization state that owns every handle as soon as the GPU
/// returns it. It is intentionally separate from `Renderer` so an error or
/// unwind can leave the partial state in `WindowGpuOwner`.
pub(crate) struct RendererInitOwner {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: RendererInfo,
    format: wgpu::TextureFormat,
    limits: Option<wgpu::Limits>,
    globals_layout: Option<wgpu::BindGroupLayout>,
    materials_layout: Option<wgpu::BindGroupLayout>,
    mesh_pipeline: Option<wgpu::RenderPipeline>,
    packet_mesh_pipeline: Option<wgpu::RenderPipeline>,
    overlay_pipeline: Option<wgpu::RenderPipeline>,
    overlay_globals: Option<wgpu::Buffer>,
    overlay_bind_group: Option<wgpu::BindGroup>,
    init_handles: RendererInitHandles,
    #[cfg(test)]
    panic_after_globals_layout: bool,
}

impl Renderer {
    /// Selects a high-performance adapter and initializes the offscreen renderer.
    /// Adapter absence is an explicit error; the renderer never substitutes a
    /// placeholder image or a no-op backend.
    pub async fn new_offscreen() -> Result<Self, RendererError> {
        Self::new_offscreen_with_format(wgpu::TextureFormat::Rgba8UnormSrgb).await
    }

    async fn new_offscreen_with_format(format: wgpu::TextureFormat) -> Result<Self, RendererError> {
        let (instance, adapter) = Self::request_offscreen_adapter().await?;
        Self::from_adapter(instance, adapter, format).await
    }

    async fn request_offscreen_adapter() -> Result<(wgpu::Instance, wgpu::Adapter), RendererError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .map_err(|error| RendererError::NoAdapter(error.to_string()))?;
        Ok((instance, adapter))
    }

    /// Synchronous convenience constructor for command-line capture tools.
    pub fn new_offscreen_blocking() -> Result<Self, RendererError> {
        pollster::block_on(Self::new_offscreen())
    }

    /// Test-only format selection for checking linear/sRGB and RGBA/BGRA paths
    /// against the same initialized renderer implementation.
    #[cfg(all(test, feature = "window"))]
    pub(crate) fn new_offscreen_blocking_with_format_for_test(
        format: wgpu::TextureFormat,
    ) -> Result<Self, RendererError> {
        pollster::block_on(async move {
            let (instance, adapter) = Self::request_offscreen_adapter().await?;
            let required = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC;
            if !adapter
                .get_texture_format_features(format)
                .allowed_usages
                .contains(required)
            {
                return Err(RendererError::InvalidScene(format!(
                    "test adapter does not support {format:?} with {required:?}"
                )));
            }
            Self::from_adapter(instance, adapter, format).await
        })
    }

    pub fn info(&self) -> &RendererInfo {
        &self.info
    }

    #[cfg(test)]
    pub(crate) fn retained_init_handle_count(&self) -> usize {
        [
            self.init_handles._base_shader.is_some(),
            self.init_handles._mesh_pipeline_layout.is_some(),
            self.init_handles._overlay_shader.is_some(),
            self.init_handles._overlay_layout.is_some(),
            self.init_handles._overlay_pipeline_layout.is_some(),
            self.init_handles._model_shader.is_some(),
            self.init_handles._model_pipeline_layout.is_some(),
            self.init_handles._sprite_shader.is_some(),
            self.init_handles._sprite_pipeline_layout.is_some(),
        ]
        .into_iter()
        .filter(|retained| *retained)
        .count()
    }

    #[cfg(test)]
    fn inject_lazy_init_panic_for_test(&mut self, point: LazyInitPanicPoint) {
        self.lazy_init_panic_point = Some(point);
    }

    /// Converts this initialized renderer into a persistent packet renderer.
    pub fn into_packet_renderer(
        self,
        limits: PacketRendererLimits,
    ) -> Result<PacketRenderer, PacketRenderError> {
        self.into_packet_renderer_with_asset_limits(limits, crate::PacketAssetLimits::default())
    }

    /// Converts this renderer into the same packet path with independent
    /// geometry and imported-asset residency/upload budgets.
    pub fn into_packet_renderer_with_asset_limits(
        self,
        limits: PacketRendererLimits,
        asset_limits: crate::PacketAssetLimits,
    ) -> Result<PacketRenderer, PacketRenderError> {
        PacketRenderer::new(self, limits, asset_limits)
    }

    fn ensure_shared_texture_layout(&mut self) -> Result<(), PacketRenderError> {
        if self.model_textures_layout.is_some() {
            return Ok(());
        }
        let limits = self.device.limits();
        if limits.max_bind_groups < 3
            || limits.max_sampled_textures_per_shader_stage < 1
            || limits.max_samplers_per_shader_stage < 1
        {
            return Err(PacketRenderError::DeviceLimit(format!(
                "textured packet draws require three bind groups, one sampled texture and one sampler; device exposes {} groups, {} textures and {} samplers",
                limits.max_bind_groups,
                limits.max_sampled_textures_per_shader_stage,
                limits.max_samplers_per_shader_stage,
            )));
        }
        self.model_textures_layout = Some(self.device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("Daedalus packet shared texture layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            },
        ));
        Ok(())
    }

    pub(crate) fn ensure_model_packet_pipeline(&mut self) -> Result<(), PacketRenderError> {
        if self.model_packet_mesh_pipeline.is_some() {
            return Ok(());
        }
        self.ensure_shared_texture_layout()?;
        let limits = self.device.limits();
        if limits.max_vertex_attributes < 11
            || limits.max_vertex_buffer_array_stride < std::mem::size_of::<GpuModelVertex>() as u32
        {
            return Err(PacketRenderError::DeviceLimit(format!(
                "textured model packets require 11 vertex attributes and a {} byte vertex stride; device exposes {} attributes and {} stride",
                std::mem::size_of::<GpuModelVertex>(),
                limits.max_vertex_attributes,
                limits.max_vertex_buffer_array_stride,
            )));
        }
        let texture_layout = self
            .model_textures_layout
            .as_ref()
            .expect("shared texture layout created");
        self.init_handles._model_pipeline_layout = Some(self.device.create_pipeline_layout(
            &wgpu::PipelineLayoutDescriptor {
                label: Some("Daedalus packet model pipeline layout"),
                bind_group_layouts: &[&self.globals_layout, &self.materials_layout, texture_layout],
                push_constant_ranges: &[],
            },
        ));
        #[cfg(test)]
        panic_lazy_init_if_requested(
            &mut self.lazy_init_panic_point,
            LazyInitPanicPoint::ModelAfterLayout,
        );
        let pipeline_layout = self
            .init_handles
            ._model_pipeline_layout
            .as_ref()
            .expect("model packet layout created");
        self.init_handles._model_shader = Some(self.device.create_shader_module(
            wgpu::ShaderModuleDescriptor {
                label: Some("Daedalus packet model textured PBR shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
            },
        ));
        #[cfg(test)]
        panic_lazy_init_if_requested(
            &mut self.lazy_init_panic_point,
            LazyInitPanicPoint::ModelAfterShader,
        );
        let shader = self
            .init_handles
            ._model_shader
            .as_ref()
            .expect("model packet shader created");
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Daedalus packet model textured PBR pipeline"),
                layout: Some(pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_packet_model"),
                    compilation_options: Default::default(),
                    buffers: &[
                        wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<GpuModelVertex>() as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x3,
                                    offset: 0,
                                    shader_location: 0,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x3,
                                    offset: 12,
                                    shader_location: 1,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x2,
                                    offset: 24,
                                    shader_location: 10,
                                },
                            ],
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: 128,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: 2,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: 3,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 32,
                                    shader_location: 4,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 48,
                                    shader_location: 5,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 64,
                                    shader_location: 6,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 80,
                                    shader_location: 7,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 96,
                                    shader_location: 8,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Uint32,
                                    offset: 112,
                                    shader_location: 9,
                                },
                            ],
                        },
                    ],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_model"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::LessEqual,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
        self.model_packet_mesh_pipeline = Some(pipeline);
        if !self.retain_lazy_init_handles {
            self.init_handles._model_shader = None;
            self.init_handles._model_pipeline_layout = None;
        }
        Ok(())
    }

    /// Preflight the device constraints and exact synchronous create-call count
    /// for one or both packet pipeline families. Staging uses this before it
    /// starts a group so a capability error cannot leave half a pipeline family
    /// installed.
    pub(crate) fn packet_pipeline_create_calls(
        &self,
        model: bool,
        sprite: bool,
    ) -> Result<usize, PacketRenderError> {
        let need_model = model && self.model_packet_mesh_pipeline.is_none();
        let need_sprite = sprite && self.sprite_packet_pipeline.is_none();
        if !need_model && !need_sprite {
            return Ok(0);
        }
        let limits = self.device.limits();
        if limits.max_bind_groups < 3
            || limits.max_sampled_textures_per_shader_stage < 1
            || limits.max_samplers_per_shader_stage < 1
        {
            return Err(PacketRenderError::DeviceLimit(
                "textured packet draws require three bind groups, one sampled texture and one sampler".into(),
            ));
        }
        if need_model
            && (limits.max_vertex_attributes < 11
                || limits.max_vertex_buffer_array_stride
                    < std::mem::size_of::<GpuModelVertex>() as u32)
        {
            return Err(PacketRenderError::DeviceLimit(
                "textured model packets require 11 vertex attributes and the imported-model vertex stride".into(),
            ));
        }
        if need_sprite
            && (limits.max_vertex_attributes < 11 || limits.max_vertex_buffer_array_stride < 128)
        {
            return Err(PacketRenderError::DeviceLimit(
                "sprite packets require 11 vertex attributes and a 128-byte instance stride".into(),
            ));
        }
        let pipelines = usize::from(need_model)
            .checked_add(usize::from(need_sprite))
            .and_then(|count| count.checked_mul(3))
            .ok_or_else(|| PacketRenderError::InvalidPacket("pipeline count overflowed".into()))?;
        pipelines
            .checked_add(usize::from(self.model_textures_layout.is_none()))
            .ok_or_else(|| PacketRenderError::InvalidPacket("pipeline count overflowed".into()))
    }

    pub(crate) fn model_textures_layout(&self) -> Option<&wgpu::BindGroupLayout> {
        self.model_textures_layout.as_ref()
    }

    pub(crate) fn model_packet_mesh_pipeline(&self) -> Option<&wgpu::RenderPipeline> {
        self.model_packet_mesh_pipeline.as_ref()
    }

    pub(crate) fn ensure_sprite_packet_pipeline(&mut self) -> Result<(), PacketRenderError> {
        if self.sprite_packet_pipeline.is_some() {
            return Ok(());
        }
        self.ensure_shared_texture_layout()?;
        let texture_layout = self
            .model_textures_layout
            .as_ref()
            .expect("shared texture layout was created with model packet pipeline");
        let limits = self.device.limits();
        if limits.max_vertex_attributes < 11
            || limits.max_vertex_buffer_array_stride < 128
            || limits.max_bind_groups < 3
        {
            return Err(PacketRenderError::DeviceLimit(
                "sprite packets require 11 vertex attributes, a 128-byte instance stride and three bind groups".into(),
            ));
        }
        self.init_handles._sprite_pipeline_layout = Some(self.device.create_pipeline_layout(
            &wgpu::PipelineLayoutDescriptor {
                label: Some("Daedalus sprite packet pipeline layout"),
                bind_group_layouts: &[&self.globals_layout, &self.materials_layout, texture_layout],
                push_constant_ranges: &[],
            },
        ));
        let pipeline_layout = self
            .init_handles
            ._sprite_pipeline_layout
            .as_ref()
            .expect("sprite packet layout created");
        self.init_handles._sprite_shader = Some(self.device.create_shader_module(
            wgpu::ShaderModuleDescriptor {
                label: Some("Daedalus sprite packet shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
            },
        ));
        let shader = self
            .init_handles
            ._sprite_shader
            .as_ref()
            .expect("sprite packet shader created");
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Daedalus sprite packet pipeline"),
                layout: Some(pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_sprite"),
                    compilation_options: Default::default(),
                    buffers: &[
                        wgpu::VertexBufferLayout {
                            array_stride: 16,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x2,
                                    offset: 0,
                                    shader_location: 0,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x2,
                                    offset: 8,
                                    shader_location: 1,
                                },
                            ],
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: 128,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: 2,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: 3,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 32,
                                    shader_location: 4,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 48,
                                    shader_location: 5,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 64,
                                    shader_location: 6,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 80,
                                    shader_location: 7,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 96,
                                    shader_location: 8,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Uint32,
                                    offset: 112,
                                    shader_location: 9,
                                },
                            ],
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: 16,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 0,
                                shader_location: 10,
                            }],
                        },
                    ],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_sprite"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: self.format,
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::SrcAlpha,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });
        self.sprite_packet_pipeline = Some(pipeline);
        if !self.retain_lazy_init_handles {
            self.init_handles._sprite_shader = None;
            self.init_handles._sprite_pipeline_layout = None;
        }
        Ok(())
    }

    pub(crate) fn sprite_packet_pipeline(&self) -> Option<&wgpu::RenderPipeline> {
        self.sprite_packet_pipeline.as_ref()
    }

    pub(crate) async fn from_adapter(
        _instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
    ) -> Result<Self, RendererError> {
        let mut owner = RendererInitOwner::request_device(adapter, format).await?;
        owner.initialize_base()?;
        owner.finish(false)
    }

    /// Renders the scene to a real GPU texture, copies it through a row-aligned
    /// staging buffer, and writes a PNG. This synchronous method waits only for
    /// the requested capture submission and is intended for explicit capture
    /// jobs, not the window present loop.
    pub fn render_png(
        &self,
        scene: &RenderScene,
        options: &CaptureOptions,
    ) -> Result<CaptureArtifact, RendererError> {
        scene.validate()?;
        self.validate_dimensions(options.width, options.height)?;
        self.validate_upload_sizes(scene)?;
        let prepared =
            PreparedScene::from_scene(scene, options.width, options.height, self.manual_srgb)?;
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Daedalus offscreen capture texture"),
            size: wgpu::Extent3d {
                width: options.width,
                height: options.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Daedalus offscreen capture encoder"),
            });
        self.record_scene(
            &mut encoder,
            &view,
            RenderPassInput {
                width: options.width,
                height: options.height,
                scene,
                prepared: &prepared,
                loading_elapsed: None,
                clear_alpha_policy: ClearAlphaPolicy::Preserve,
            },
        );

        self.capture_texture_png(
            &texture,
            encoder,
            options,
            "Daedalus capture readback buffer",
            PngAlphaRepresentation::Straight,
        )
    }

    /// Copies a rendered RGBA texture into a padded staging buffer, performs
    /// the explicit synchronous readback, strips row padding, and writes PNG.
    /// Callers supply any already-recorded draw commands so legacy capture
    /// remains one submission while packet capture copies after its draw.
    pub(crate) fn capture_texture_png(
        &self,
        texture: &wgpu::Texture,
        mut encoder: wgpu::CommandEncoder,
        options: &CaptureOptions,
        readback_label: &'static str,
        alpha_representation: PngAlphaRepresentation,
    ) -> Result<CaptureArtifact, RendererError> {
        self.validate_dimensions(options.width, options.height)?;
        let unpadded_bytes_per_row = options
            .width
            .checked_mul(4)
            .ok_or(RendererError::CaptureBufferTooLarge)?;
        let padded_bytes_per_row = align_to(unpadded_bytes_per_row, PNG_ROW_ALIGNMENT)
            .ok_or(RendererError::CaptureBufferTooLarge)?;
        let padded_size = u64::from(padded_bytes_per_row)
            .checked_mul(u64::from(options.height))
            .ok_or(RendererError::CaptureBufferTooLarge)?;
        if padded_size > self.maximum_buffer_size {
            return Err(RendererError::CaptureBufferTooLarge);
        }
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(readback_label),
            size: padded_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(options.height),
                },
            },
            wgpu::Extent3d {
                width: options.width,
                height: options.height,
                depth_or_array_layers: 1,
            },
        );
        let submission = self.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(MAP_TIMEOUT),
            })
            .map_err(|error| RendererError::Poll(error.to_string()))?;
        receiver
            .recv_timeout(MAP_TIMEOUT)
            .map_err(|error| RendererError::Map(error.to_string()))?
            .map_err(|error| RendererError::Map(error.to_string()))?;

        let mapped = slice.get_mapped_range();
        let output_size =
            usize::try_from(u64::from(unpadded_bytes_per_row) * u64::from(options.height))
                .map_err(|_| RendererError::CaptureBufferTooLarge)?;
        let mut rgba = Vec::with_capacity(output_size);
        for row in 0..options.height as usize {
            let source_start = row * padded_bytes_per_row as usize;
            let source_end = source_start + unpadded_bytes_per_row as usize;
            rgba.extend_from_slice(&mapped[source_start..source_end]);
        }
        drop(mapped);
        readback.unmap();
        apply_png_alpha_representation(&mut rgba, alpha_representation);
        write_png(&options.output, options.width, options.height, &rgba)?;
        Ok(CaptureArtifact {
            path: options.output.clone(),
            width: options.width,
            height: options.height,
            rgba8_bytes: rgba.len(),
            renderer: self.info.clone(),
        })
    }

    pub(crate) fn validate_dimensions(&self, width: u32, height: u32) -> Result<(), RendererError> {
        if width == 0
            || height == 0
            || width > self.maximum_dimension
            || height > self.maximum_dimension
        {
            return Err(RendererError::InvalidCaptureSize {
                width,
                height,
                maximum: self.maximum_dimension,
            });
        }
        Ok(())
    }

    #[cfg(feature = "window")]
    pub(crate) fn render_to_view(
        &self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        scene: &RenderScene,
        loading_elapsed: Option<f32>,
    ) -> Result<(), RendererError> {
        self.validate_dimensions(width, height)?;
        scene.validate()?;
        self.validate_upload_sizes(scene)?;
        let prepared = PreparedScene::from_scene(scene, width, height, self.manual_srgb)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Daedalus window frame encoder"),
            });
        self.record_scene(
            &mut encoder,
            view,
            RenderPassInput {
                width,
                height,
                scene,
                prepared: &prepared,
                loading_elapsed,
                clear_alpha_policy: ClearAlphaPolicy::Opaque,
            },
        );
        self.queue.submit([encoder.finish()]);
        Ok(())
    }

    fn validate_upload_sizes(&self, scene: &RenderScene) -> Result<(), RendererError> {
        let mut vertex_count = 0u64;
        let mut index_count = 0u64;
        for object in &scene.objects {
            vertex_count = vertex_count
                .checked_add(object.mesh.vertices.len() as u64)
                .ok_or_else(|| {
                    RendererError::InvalidScene("scene vertex count overflowed".into())
                })?;
            index_count = index_count
                .checked_add(object.mesh.indices.len() as u64)
                .ok_or_else(|| {
                    RendererError::InvalidScene("scene index count overflowed".into())
                })?;
        }
        let vertex_bytes = vertex_count
            .checked_mul(std::mem::size_of::<GpuVertex>() as u64)
            .ok_or_else(|| RendererError::InvalidScene("vertex upload size overflowed".into()))?;
        let index_bytes = index_count
            .checked_mul(std::mem::size_of::<u32>() as u64)
            .ok_or_else(|| RendererError::InvalidScene("index upload size overflowed".into()))?;
        let material_bytes = (scene.objects.len().max(1) as u64)
            .checked_mul(std::mem::size_of::<MaterialUniform>() as u64)
            .ok_or_else(|| RendererError::InvalidScene("material upload size overflowed".into()))?;
        if vertex_bytes > self.maximum_buffer_size
            || index_bytes > self.maximum_buffer_size
            || material_bytes > self.maximum_buffer_size
            || material_bytes > self.maximum_storage_binding_size
        {
            return Err(RendererError::InvalidScene(format!(
                "scene GPU upload exceeds device limits (vertices: {vertex_bytes} bytes, indices: {index_bytes} bytes, materials: {material_bytes} bytes)"
            )));
        }
        Ok(())
    }

    fn record_scene(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        input: RenderPassInput<'_>,
    ) {
        let RenderPassInput {
            width,
            height,
            scene,
            prepared,
            loading_elapsed,
            clear_alpha_policy,
        } = input;
        let clear_color =
            target_clear_color(scene.clear_color, self.manual_srgb, clear_alpha_policy);
        let depth_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Daedalus scene depth buffer"),
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
        let depth_view = depth_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let globals_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Daedalus frame camera and light data"),
                contents: bytemuck::bytes_of(&prepared.globals),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let globals_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Daedalus frame globals bind group"),
            layout: &self.globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });
        let materials = if prepared.materials.is_empty() {
            vec![MaterialUniform::default()]
        } else {
            prepared.materials.clone()
        };
        let materials_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Daedalus frame material data"),
                contents: bytemuck::cast_slice(&materials),
                usage: wgpu::BufferUsages::STORAGE,
            });
        let materials_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Daedalus frame material bind group"),
            layout: &self.materials_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: materials_buffer.as_entire_binding(),
            }],
        });
        let vertex_buffer = (!prepared.vertices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("Daedalus frame vertex data"),
                    contents: bytemuck::cast_slice(&prepared.vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });
        let index_buffer = (!prepared.indices.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("Daedalus frame index data"),
                    contents: bytemuck::cast_slice(&prepared.indices),
                    usage: wgpu::BufferUsages::INDEX,
                })
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus opaque scene pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.mesh_pipeline);
            pass.set_bind_group(0, &globals_bind_group, &[]);
            pass.set_bind_group(1, &materials_bind_group, &[]);
            if let (Some(vertex_buffer), Some(index_buffer)) = (&vertex_buffer, &index_buffer) {
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                for draw in &prepared.draws {
                    pass.draw_indexed(
                        draw.indices.clone(),
                        draw.base_vertex,
                        draw.material_index..draw.material_index + 1,
                    );
                }
            }
        }

        if let Some(elapsed) = loading_elapsed {
            self.queue.write_buffer(
                &self.overlay_globals,
                0,
                bytemuck::bytes_of(&OverlayUniform {
                    time: elapsed,
                    width: width as f32,
                    height: height as f32,
                    manual_srgb: if self.manual_srgb { 1.0 } else { 0.0 },
                }),
            );
            let mut overlay_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Daedalus local loading overlay pass"),
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
            overlay_pass.set_pipeline(&self.overlay_pipeline);
            overlay_pass.set_bind_group(0, &self.overlay_bind_group, &[]);
            overlay_pass.draw(0..3, 0..1);
        }
    }
}

impl RendererInitOwner {
    pub(crate) async fn request_device(
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
    ) -> Result<Self, RendererError> {
        Self::request_device_with_features(adapter, format, wgpu::Features::empty()).await
    }

    pub(crate) async fn request_device_with_features(
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
        required_features: wgpu::Features,
    ) -> Result<Self, RendererError> {
        let adapter_info = adapter.get_info();
        let info = RendererInfo {
            adapter_name: adapter_info.name,
            backend: format!("{:?}", adapter_info.backend),
            device_type: format!("{:?}", adapter_info.device_type),
        };
        let supported_limits = adapter.limits();
        let required_limits = wgpu::Limits::downlevel_defaults().using_resolution(supported_limits);
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Daedalus renderer device"),
                required_features,
                required_limits,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|error| RendererError::RequestDevice(error.to_string()))?;

        // Construct the owner immediately after the device pair is returned.
        // No adapter/device query or heap allocation precedes this assignment.
        let owner = Self {
            device,
            queue,
            info,
            format,
            limits: None,
            globals_layout: None,
            materials_layout: None,
            mesh_pipeline: None,
            packet_mesh_pipeline: None,
            overlay_pipeline: None,
            overlay_globals: None,
            overlay_bind_group: None,
            init_handles: RendererInitHandles::default(),
            #[cfg(test)]
            panic_after_globals_layout: false,
        };
        Ok(owner)
    }

    #[cfg(test)]
    pub(crate) fn panic_after_globals_layout_for_test(&mut self) {
        self.panic_after_globals_layout = true;
    }

    #[cfg(test)]
    pub(crate) fn retains_first_base_layout_for_test(&self) -> bool {
        self.globals_layout.is_some() && self.materials_layout.is_none()
    }

    pub(crate) fn initialize_base(&mut self) -> Result<(), RendererError> {
        if self.limits.is_none() {
            self.limits = Some(self.device.limits());
        }
        let device = &self.device;
        let format = self.format;
        self.globals_layout = Some(device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("Daedalus renderer globals layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<GlobalsUniform>() as u64,
                        ),
                    },
                    count: None,
                }],
            },
        ));
        #[cfg(test)]
        if std::mem::take(&mut self.panic_after_globals_layout) {
            panic!("injected base renderer initialization panic after globals layout");
        }
        let globals_layout = self
            .globals_layout
            .as_ref()
            .expect("globals_layout initialized");
        self.materials_layout = Some(device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("Daedalus material storage layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<MaterialUniform>() as u64,
                        ),
                    },
                    count: None,
                }],
            },
        ));
        let materials_layout = self
            .materials_layout
            .as_ref()
            .expect("materials_layout initialized");

        self.init_handles._base_shader =
            Some(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Daedalus metallic-roughness shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
            }));
        let shader = self
            .init_handles
            ._base_shader
            .as_ref()
            .expect("shader initialized");
        self.init_handles._mesh_pipeline_layout = Some(device.create_pipeline_layout(
            &wgpu::PipelineLayoutDescriptor {
                label: Some("Daedalus mesh pipeline layout"),
                bind_group_layouts: &[globals_layout, materials_layout],
                push_constant_ranges: &[],
            },
        ));
        let mesh_pipeline_layout = self
            .init_handles
            ._mesh_pipeline_layout
            .as_ref()
            .expect("mesh_pipeline_layout initialized");
        self.mesh_pipeline = Some(
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Daedalus opaque metallic-roughness pipeline"),
                layout: Some(mesh_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<GpuVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: 12,
                                shader_location: 1,
                            },
                        ],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::LessEqual,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            }),
        );

        self.packet_mesh_pipeline = Some(device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("Daedalus shared PBR packet pipeline"),
                layout: Some(mesh_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs_packet"),
                    compilation_options: Default::default(),
                    buffers: &[
                        wgpu::VertexBufferLayout {
                            array_stride: std::mem::size_of::<GpuVertex>() as u64,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x3,
                                    offset: 0,
                                    shader_location: 0,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x3,
                                    offset: 12,
                                    shader_location: 1,
                                },
                            ],
                        },
                        wgpu::VertexBufferLayout {
                            array_stride: 128,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: &[
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 0,
                                    shader_location: 2,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 16,
                                    shader_location: 3,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 32,
                                    shader_location: 4,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 48,
                                    shader_location: 5,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 64,
                                    shader_location: 6,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 80,
                                    shader_location: 7,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Float32x4,
                                    offset: 96,
                                    shader_location: 8,
                                },
                                wgpu::VertexAttribute {
                                    format: wgpu::VertexFormat::Uint32,
                                    offset: 112,
                                    shader_location: 9,
                                },
                            ],
                        },
                    ],
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    unclipped_depth: false,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    conservative: false,
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::LessEqual,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            },
        ));

        self.overlay_globals = Some(device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Daedalus local loading overlay state"),
            size: std::mem::size_of::<OverlayUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        let overlay_globals = self
            .overlay_globals
            .as_ref()
            .expect("overlay_globals initialized");
        self.init_handles._overlay_shader =
            Some(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Daedalus local presentation overlay shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
            }));
        let overlay_shader = self
            .init_handles
            ._overlay_shader
            .as_ref()
            .expect("overlay_shader initialized");
        self.init_handles._overlay_layout = Some(device.create_bind_group_layout(
            &wgpu::BindGroupLayoutDescriptor {
                label: Some("Daedalus loading overlay layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<OverlayUniform>() as u64,
                        ),
                    },
                    count: None,
                }],
            },
        ));
        let overlay_layout = self
            .init_handles
            ._overlay_layout
            .as_ref()
            .expect("overlay_layout initialized");
        self.overlay_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Daedalus loading overlay bind group"),
            layout: overlay_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: overlay_globals.as_entire_binding(),
            }],
        }));
        self.init_handles._overlay_pipeline_layout = Some(device.create_pipeline_layout(
            &wgpu::PipelineLayoutDescriptor {
                label: Some("Daedalus loading overlay pipeline layout"),
                bind_group_layouts: &[overlay_layout],
                push_constant_ranges: &[],
            },
        ));
        let overlay_pipeline_layout = self
            .init_handles
            ._overlay_pipeline_layout
            .as_ref()
            .expect("overlay_pipeline_layout initialized");
        self.overlay_pipeline = Some(device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("Daedalus loading overlay pipeline"),
                layout: Some(overlay_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: overlay_shader,
                    entry_point: Some("vs_overlay"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: overlay_shader,
                    entry_point: Some("fs_overlay"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            },
        ));

        Ok(())
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.globals_layout.is_some()
            && self.materials_layout.is_some()
            && self.mesh_pipeline.is_some()
            && self.packet_mesh_pipeline.is_some()
            && self.overlay_pipeline.is_some()
            && self.overlay_globals.is_some()
            && self.overlay_bind_group.is_some()
            && self.init_handles._base_shader.is_some()
            && self.init_handles._mesh_pipeline_layout.is_some()
            && self.init_handles._overlay_shader.is_some()
            && self.init_handles._overlay_layout.is_some()
            && self.init_handles._overlay_pipeline_layout.is_some()
            && self.limits.is_some()
    }

    pub(crate) fn finish(self, retain_init_handles: bool) -> Result<Renderer, RendererError> {
        if !self.is_ready() {
            return Err(RendererError::InitializationIncomplete);
        }
        Ok(self.into_ready(retain_init_handles))
    }

    pub(crate) fn into_ready(mut self, retain_init_handles: bool) -> Renderer {
        debug_assert!(self.is_ready());
        if !retain_init_handles {
            self.init_handles = RendererInitHandles::default();
        }
        let limits = self.limits.take().expect("readiness checked");
        Renderer {
            device: self.device,
            queue: self.queue,
            info: self.info,
            format: self.format,
            manual_srgb: !self.format.is_srgb(),
            globals_layout: self.globals_layout.take().expect("readiness checked"),
            materials_layout: self.materials_layout.take().expect("readiness checked"),
            mesh_pipeline: self.mesh_pipeline.take().expect("readiness checked"),
            packet_mesh_pipeline: self.packet_mesh_pipeline.take().expect("readiness checked"),
            model_packet_mesh_pipeline: None,
            sprite_packet_pipeline: None,
            model_textures_layout: None,
            overlay_pipeline: self.overlay_pipeline.take().expect("readiness checked"),
            overlay_globals: self.overlay_globals.take().expect("readiness checked"),
            overlay_bind_group: self.overlay_bind_group.take().expect("readiness checked"),
            maximum_dimension: limits.max_texture_dimension_2d,
            maximum_buffer_size: limits.max_buffer_size,
            maximum_storage_binding_size: u64::from(limits.max_storage_buffer_binding_size),
            init_handles: self.init_handles,
            retain_lazy_init_handles: retain_init_handles,
            #[cfg(test)]
            lazy_init_panic_point: None,
        }
    }
}

fn align_to(value: u32, alignment: u32) -> Option<u32> {
    let remainder = value % alignment;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(alignment - remainder)
    }
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), RendererError> {
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| RendererError::CaptureBufferTooLarge)?;
    if rgba.len() != expected {
        return Err(RendererError::Map(format!(
            "readback returned {} bytes; expected {expected}",
            rgba.len()
        )));
    }
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    encoder.set_compression(png::Compression::Fast);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(())
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuVertex {
    pub(crate) position: [f32; 3],
    pub(crate) normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuModelVertex {
    pub(crate) position: [f32; 3],
    pub(crate) normal: [f32; 3],
    pub(crate) uv0: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
pub(crate) struct GlobalsUniform {
    pub(crate) view_projection: [[f32; 4]; 4],
    pub(crate) camera_position: [f32; 4],
    pub(crate) light_direction_intensity: [f32; 4],
    pub(crate) light_color: [f32; 4],
    pub(crate) ambient_and_manual_srgb: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
pub(crate) struct MaterialUniform {
    pub(crate) base_color: [f32; 4],
    pub(crate) metallic_roughness: [f32; 4],
    pub(crate) emissive: [f32; 4],
}

impl Default for MaterialUniform {
    fn default() -> Self {
        Self {
            base_color: [0.0, 0.0, 0.0, 1.0],
            metallic_roughness: [0.0, 1.0, 0.0, 0.0],
            emissive: [0.0; 4],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct OverlayUniform {
    pub(crate) time: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) manual_srgb: f32,
}

struct DrawRange {
    indices: std::ops::Range<u32>,
    base_vertex: i32,
    material_index: u32,
}

struct PreparedScene {
    vertices: Vec<GpuVertex>,
    indices: Vec<u32>,
    draws: Vec<DrawRange>,
    materials: Vec<MaterialUniform>,
    globals: GlobalsUniform,
}

struct RenderPassInput<'a> {
    width: u32,
    height: u32,
    scene: &'a RenderScene,
    prepared: &'a PreparedScene,
    loading_elapsed: Option<f32>,
    clear_alpha_policy: ClearAlphaPolicy,
}

impl PreparedScene {
    fn from_scene(
        scene: &RenderScene,
        width: u32,
        height: u32,
        manual_srgb: bool,
    ) -> Result<Self, RendererError> {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut draws = Vec::with_capacity(scene.objects.len());
        let mut materials = Vec::with_capacity(scene.objects.len());
        for (material_index, object) in scene.objects.iter().enumerate() {
            let base_vertex = i32::try_from(vertices.len()).map_err(|_| {
                RendererError::InvalidScene(
                    "scene vertex offset exceeds the signed draw limit".into(),
                )
            })?;
            let index_start = u32::try_from(indices.len()).map_err(|_| {
                RendererError::InvalidScene("scene index offset exceeds the draw limit".into())
            })?;
            let normal_matrix = Mat3::from_mat4(object.transform).inverse().transpose();
            for vertex in &object.mesh.vertices {
                vertices.push(transform_vertex(vertex, object.transform, normal_matrix));
            }
            indices.extend_from_slice(&object.mesh.indices);
            let index_end = u32::try_from(indices.len()).map_err(|_| {
                RendererError::InvalidScene("scene index range exceeds the draw limit".into())
            })?;
            draws.push(DrawRange {
                indices: index_start..index_end,
                base_vertex,
                material_index: material_index as u32,
            });
            materials.push(MaterialUniform {
                base_color: [
                    object.material.base_color[0],
                    object.material.base_color[1],
                    object.material.base_color[2],
                    1.0,
                ],
                metallic_roughness: [
                    object.material.metallic,
                    object.material.roughness,
                    0.0,
                    0.0,
                ],
                emissive: [
                    object.material.emissive[0],
                    object.material.emissive[1],
                    object.material.emissive[2],
                    0.0,
                ],
            });
        }
        let light_direction = Vec3::from_array(scene.directional_light.direction).normalize();
        let globals = GlobalsUniform {
            view_projection: scene
                .camera
                .view_projection(width as f32 / height as f32)
                .to_cols_array_2d(),
            camera_position: [
                scene.camera.position[0],
                scene.camera.position[1],
                scene.camera.position[2],
                1.0,
            ],
            light_direction_intensity: [
                light_direction.x,
                light_direction.y,
                light_direction.z,
                scene.directional_light.intensity,
            ],
            light_color: [
                scene.directional_light.color[0],
                scene.directional_light.color[1],
                scene.directional_light.color[2],
                0.0,
            ],
            ambient_and_manual_srgb: [
                scene.ambient_color[0],
                scene.ambient_color[1],
                scene.ambient_color[2],
                if manual_srgb { 1.0 } else { 0.0 },
            ],
        };
        Ok(Self {
            vertices,
            indices,
            draws,
            materials,
            globals,
        })
    }
}

fn transform_vertex(vertex: &Vertex, transform: glam::Mat4, normal_matrix: Mat3) -> GpuVertex {
    let position = transform.transform_point3(Vec3::from_array(vertex.position));
    let normal = (normal_matrix * Vec3::from_array(vertex.normal)).normalize();
    GpuVertex {
        position: position.to_array(),
        normal: normal.to_array(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClearAlphaPolicy, PngAlphaRepresentation, align_to, apply_png_alpha_representation,
        linear_to_srgb, target_clear_color,
    };

    fn managed_base_renderer() -> super::Renderer {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("offscreen adapter");
        let mut initialization = pollster::block_on(super::RendererInitOwner::request_device(
            adapter,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ))
        .expect("renderer device");
        initialization
            .initialize_base()
            .expect("base renderer initialization");
        initialization.into_ready(true)
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; checks lazy layout and shader retention after unwind"]
    fn managed_lazy_model_shader_handles_survive_unwind_before_pipeline() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let mut renderer = managed_base_renderer();
        renderer.inject_lazy_init_panic_for_test(super::LazyInitPanicPoint::ModelAfterShader);
        let result = catch_unwind(AssertUnwindSafe(|| renderer.ensure_model_packet_pipeline()));
        assert!(
            result.is_err(),
            "the configured lazy initialization panic must execute"
        );
        assert!(renderer.init_handles._model_pipeline_layout.is_some());
        assert!(renderer.init_handles._model_shader.is_some());
        assert!(renderer.model_packet_mesh_pipeline.is_none());
        assert_eq!(renderer.retained_init_handle_count(), 7);
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; checks lazy layout retention before shader creation"]
    fn managed_lazy_model_layout_survives_unwind_before_shader() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let mut renderer = managed_base_renderer();
        renderer.inject_lazy_init_panic_for_test(super::LazyInitPanicPoint::ModelAfterLayout);
        let result = catch_unwind(AssertUnwindSafe(|| renderer.ensure_model_packet_pipeline()));
        assert!(
            result.is_err(),
            "the configured lazy initialization panic must execute"
        );
        assert!(renderer.init_handles._model_pipeline_layout.is_some());
        assert!(renderer.init_handles._model_shader.is_none());
        assert!(renderer.model_packet_mesh_pipeline.is_none());
        assert_eq!(renderer.retained_init_handle_count(), 6);
    }

    #[cfg(feature = "window")]
    #[test]
    #[ignore = "requires a native wgpu adapter; checks scene-view clear alpha by readback"]
    fn scene_view_forces_clear_alpha_opaque_without_changing_rgb() {
        use std::{fs::File, io::BufReader};

        use crate::scene::RenderScene;

        let renderer = super::Renderer::new_offscreen_blocking().unwrap();
        let scene = RenderScene {
            clear_color: [0.2, 0.4, 0.6, 0.25],
            ..RenderScene::default()
        };
        let texture = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Daedalus scene opaque clear-alpha target"),
            size: wgpu::Extent3d {
                width: 8,
                height: 8,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: renderer.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        renderer.render_to_view(&view, 8, 8, &scene, None).unwrap();

        let output = std::env::temp_dir().join(format!(
            "daedalus-scene-view-opaque-alpha-{}.png",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&output);
        let options = super::CaptureOptions::new(8, 8, &output);
        let encoder = renderer
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Daedalus scene opaque clear-alpha readback"),
            });
        renderer
            .capture_texture_png(
                &texture,
                encoder,
                &options,
                "Daedalus scene opaque clear-alpha readback",
                PngAlphaRepresentation::Straight,
            )
            .unwrap();

        let decoder = png::Decoder::new(BufReader::new(File::open(&output).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().expect("small test image")];
        let frame = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(frame.buffer_size());
        assert_eq!(pixels.len(), 8 * 8 * 4);
        for pixel in pixels.chunks_exact(4) {
            assert_eq!(pixel[3], u8::MAX);
            assert!((pixel[0] as i16 - 124).abs() <= 1);
            assert!((pixel[1] as i16 - 170).abs() <= 1);
            assert!((pixel[2] as i16 - 203).abs() <= 1);
        }
        let _ = std::fs::remove_file(output);
    }

    #[test]
    fn capture_rows_are_aligned_for_gpu_copy() {
        assert_eq!(align_to(256, 256), Some(256));
        assert_eq!(align_to(257, 256), Some(512));
        assert_eq!(align_to(u32::MAX, 256), None);
    }

    #[test]
    fn packet_png_unpremultiplies_srgb_in_place_and_preserves_alpha_boundaries() {
        let mut straight = [17, 31, 63, 0, 128, 64, 32, 255];
        let original = straight;
        apply_png_alpha_representation(&mut straight, PngAlphaRepresentation::Straight);
        assert_eq!(straight, original);

        let mut target = [
            188, 0, 0, 128, 66, 66, 66, 64, 99, 80, 60, 0, 12, 34, 56, 255,
        ];
        apply_png_alpha_representation(&mut target, PngAlphaRepresentation::SrgbPremultiplied);
        assert_eq!(&target[..4], &[255, 0, 0, 128]);
        assert!(
            target[4..7]
                .iter()
                .all(|channel| (126..=130).contains(channel))
        );
        assert_eq!(target[7], 64);
        assert_eq!(&target[8..12], &[0, 0, 0, 0]);
        assert_eq!(&target[12..], &[12, 34, 56, 255]);
    }

    #[test]
    fn render_target_clear_policy_keeps_encoding_and_alpha_semantics_aligned() {
        let source = [0.8, 0.4, 0.2, 0.25];
        let preserved = target_clear_color(source, false, ClearAlphaPolicy::Preserve);
        assert_eq!(
            [
                preserved.r as f32,
                preserved.g as f32,
                preserved.b as f32,
                preserved.a as f32,
            ],
            source
        );

        let premultiplied = target_clear_color(source, false, ClearAlphaPolicy::Premultiplied);
        assert_eq!(
            [
                premultiplied.r as f32,
                premultiplied.g as f32,
                premultiplied.b as f32,
                premultiplied.a as f32,
            ],
            [0.2_f32, 0.1, 0.05, 0.25]
        );

        let opaque = target_clear_color(source, false, ClearAlphaPolicy::Opaque);
        assert_eq!(
            [
                opaque.r as f32,
                opaque.g as f32,
                opaque.b as f32,
                opaque.a as f32,
            ],
            [0.8_f32, 0.4, 0.2, 1.0]
        );

        let fallback = target_clear_color(source, true, ClearAlphaPolicy::Premultiplied);
        assert!((fallback.r as f32 - linear_to_srgb(0.2)).abs() <= f32::EPSILON);
        assert!((fallback.g as f32 - linear_to_srgb(0.1)).abs() <= f32::EPSILON);
        assert!((fallback.b as f32 - linear_to_srgb(0.05)).abs() <= f32::EPSILON);
        assert_eq!(fallback.a, 0.25);
    }
}
