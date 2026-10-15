//! Daedalus' renderer-owned scene API and initial wgpu implementation.
//!
//! The renderer accepts copied, owned presentation data. It does not query an ECS
//! or retain simulation snapshots. The initial material shader implements a
//! metallic/roughness Cook-Torrance directional-light model with a simple ambient
//! term and ACES-style tone mapping; shadows, transparency, texture sampling,
//! temporal effects, and asset residency are future work.

mod renderer;
mod scene;
#[cfg(feature = "window")]
mod window;

#[cfg(feature = "test-fixtures")]
pub mod test_fixtures;

pub use renderer::{CaptureArtifact, CaptureOptions, Renderer, RendererError, RendererInfo};
pub use scene::{Camera, DirectionalLight, Material, Mesh, RenderObject, RenderScene, Vertex};
#[cfg(feature = "window")]
pub use window::{
    WindowInput, WindowInputSink, WindowMessage, WindowOptions, run_windowed, window_input_channel,
};
