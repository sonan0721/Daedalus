//! Small, renderer-independent contracts shared by Daedalus engine crates.

mod snapshot;
mod types;

pub use snapshot::{
    PublicationStatus, RenderEntitySnapshot, RenderSnapshot, SnapshotConsumer,
    SnapshotPublishError, SnapshotPublisher, SnapshotReadError, snapshot_channel,
};
pub use types::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};
