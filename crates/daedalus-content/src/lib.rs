//! Host-neutral, serializable content data and CPU world algorithms.
//!
//! This crate deliberately owns no ECS, renderer, I/O runtime, or game-specific rules.

pub mod navigation;
pub mod project;
pub mod scene;
pub mod tile;
pub mod voxel;

pub use project::{
    PreparedEntity, PreparedProject, ProjectCamera, ProjectDirectionalLight, ProjectError,
    ProjectFileIdentity, ProjectLimits, ProjectManifest, ProjectMaterial, ProjectMesh,
    ProjectPresentation, ProjectTransform, ProjectVertex, load_project, load_project_with_limits,
    prepare_project_from_bytes,
};

/// A non-negative monotonically increasing revision used by content snapshots.
pub type Revision = u64;
