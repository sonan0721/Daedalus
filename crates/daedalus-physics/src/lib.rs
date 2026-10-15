//! Engine-facing physics adapters backed by Jolt 5.6 and Rapier 2D.

mod arena;
mod physics2d;
mod physics3d;

pub use arena::{PhysicsBodyHandle, PhysicsError};
pub use physics2d::{
    BodyDesc2d, BodyMotion2d, BodyPose2d, BodyShape2d, Physics2dConfig, Physics2dWorld, RayHit2d,
};
pub use physics3d::{
    BodyDesc3d, BodyMotion3d, BodyPose3d, BodyShape3d, Physics3dConfig, Physics3dWorld, RayHit3d,
};
