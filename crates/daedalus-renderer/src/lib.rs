//! Daedalus' renderer-owned scene API and initial wgpu implementation.
//!
//! The renderer accepts copied, owned presentation data. It does not query an ECS
//! or retain simulation snapshots. The initial material shader implements a
//! metallic/roughness Cook-Torrance directional-light model with a simple ambient
//! term and ACES-style tone mapping; shadows, transparency, texture sampling,
//! temporal effects, and asset residency are future work.

mod project;
mod renderer;
mod scene;
#[cfg(feature = "window")]
mod window;

#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;

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
    PresentationMetadata, WindowExitReason, WindowInput, WindowInputSink, WindowKeyCode,
    WindowMessage, WindowOptions, WindowOutcome, WindowReport, run_windowed, run_windowed_reported,
    window_input_channel,
};
