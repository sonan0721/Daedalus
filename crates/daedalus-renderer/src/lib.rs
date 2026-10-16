//! Daedalus' renderer-owned scene API and initial wgpu implementation.
//!
//! The renderer accepts owned presentation data and immutable render packets. It
//! does not query an ECS or retain simulation snapshots. The packet path supports
//! imported static triangle models with opaque base-color textures, per-texture
//! sampler state, and bounded device-local geometry and texture caches. Its
//! metallic/roughness Cook-Torrance shader uses a simple ambient term and ACES-style
//! tone mapping; shadows, transparency, and temporal effects remain future work.

mod packet;
mod packet_renderer;
mod project;
mod renderer;
mod scene;
#[cfg(feature = "window")]
mod window;

#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;

pub use packet::{
    MeshHandle, MeshResourceLimits, PacketAssetLimits, PacketRenderError, PacketRendererLimits,
    ProjectRenderResources, RenderInstance, RenderInstanceId, RenderPacket,
};
pub use packet_renderer::{PacketAssetResourceStats, PacketRenderer, PacketResourceStats};
pub use project::{
    ProjectProjection, ProjectProjectionError, ProjectProjectionLimits, project_snapshot,
    project_snapshot_with_limits,
};
pub use renderer::{CaptureArtifact, CaptureOptions, Renderer, RendererError, RendererInfo};
pub use scene::{Camera, DirectionalLight, Material, Mesh, RenderObject, RenderScene, Vertex};
#[cfg(feature = "window")]
pub use window::{
    FRAME_INTERVAL_SAMPLE_CAPACITY, FrameIntervalStats, MAX_WINDOW_SCENE_GEOMETRY_BYTES,
    MAX_WINDOW_SCENE_INDICES, MAX_WINDOW_SCENE_OBJECTS, MAX_WINDOW_SCENE_VERTICES,
    PacketAssetWindowOutcome, PacketWindowError, PacketWindowMessage, PacketWindowOutcome,
    PresentationMetadata, PresentedPacket, WindowExitReason, WindowInput, WindowInputSink,
    WindowKeyCode, WindowMessage, WindowOptions, WindowOutcome, WindowReport,
    run_packet_windowed_reported, run_packet_windowed_reported_with_asset_limits, run_windowed,
    run_windowed_reported, window_input_channel,
};
