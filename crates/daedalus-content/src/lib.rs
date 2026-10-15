//! Host-neutral, serializable content data and CPU world algorithms.
//!
//! This crate deliberately owns no ECS, renderer, I/O runtime, or game-specific rules.

pub mod navigation;
pub mod scene;
pub mod tile;
pub mod voxel;

/// A non-negative monotonically increasing revision used by content snapshots.
pub type Revision = u64;
