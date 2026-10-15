use std::collections::HashMap;

use daedalus_core::{Revision, SessionId};
use oxijolt::prelude::{
    BodyId as JoltBodyId, BodySettings, PhysicsWorld, QueryFilter, RayCast, Shape, WorldSettings,
};
use oxijolt::{Quat, RVec3, Vec3};

use crate::arena::{HandleArena, PhysicsBodyHandle, PhysicsError, next_revision};

const MAX_BODIES: u32 = 1_000_000;
const MAX_WORKERS: u32 = 8;
const MAX_FIXED_STEP: f32 = 0.1;
const MAX_POSITION_METERS: f32 = 1_000_000.0;
const MAX_LINEAR_SPEED: f32 = 100_000.0;
const MAX_SHAPE_SIZE_METERS: f32 = 100_000.0;
const MAX_RAY_DISTANCE: f32 = 1_000_000.0;
const MAX_MASS_KILOGRAMS: f32 = 1_000_000_000.0;

/// Configuration for a Jolt 3D world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Physics3dConfig {
    /// Gravity in metres per second squared.
    pub gravity: [f32; 3],
    /// The single simulation step used by each call to [`Physics3dWorld::step`].
    pub fixed_timestep_secs: f32,
    /// Maximum number of bodies in this world.
    pub max_bodies: u32,
    /// Jolt worker threads, in addition to the thread that steps the world.
    pub worker_threads: u32,
}

impl Default for Physics3dConfig {
    fn default() -> Self {
        Self {
            gravity: [0.0, -9.81, 0.0],
            fixed_timestep_secs: 1.0 / 60.0,
            max_bodies: 10_240,
            worker_threads: 1,
        }
    }
}

impl Physics3dConfig {
    fn validate(self) -> Result<(), PhysicsError> {
        if !self.gravity.into_iter().all(f32::is_finite)
            || length_squared3(self.gravity) > 100.0 * 100.0
        {
            return Err(PhysicsError::InvalidInput(
                "3D gravity must be finite and no stronger than 100 m/s²",
            ));
        }
        if !self.fixed_timestep_secs.is_finite()
            || self.fixed_timestep_secs <= 0.0
            || self.fixed_timestep_secs > MAX_FIXED_STEP
        {
            return Err(PhysicsError::InvalidInput(
                "3D fixed timestep must be finite and in (0, 0.1] seconds",
            ));
        }
        if !(1..=MAX_BODIES).contains(&self.max_bodies) {
            return Err(PhysicsError::InvalidInput(
                "3D max_bodies must be between 1 and 1,000,000",
            ));
        }
        if !(1..=MAX_WORKERS).contains(&self.worker_threads) {
            return Err(PhysicsError::InvalidInput(
                "3D worker_threads must be between 1 and 8",
            ));
        }
        Ok(())
    }
}

/// Supported primitive 3D collider shapes. Box and capsule sizes are half extents and half
/// cylinder height respectively.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyShape3d {
    /// Sphere with radius in metres.
    Sphere { radius: f32 },
    /// Axis-aligned box in body-local coordinates, using positive half extents in metres.
    Box { half_extents: [f32; 3] },
    /// Capsule aligned with local Y, with a cylindrical half height and spherical radius.
    Capsule { half_height: f32, radius: f32 },
}

/// Static, dynamic, or engine-driven kinematic body motion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyMotion3d {
    /// Never moved by simulation.
    Static,
    /// Integrated and solved by Jolt.
    Dynamic,
    /// Moved by the host; not integrated by gravity.
    Kinematic,
}

/// Description used to create a Jolt body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyDesc3d {
    /// Collider shape.
    pub shape: BodyShape3d,
    /// Motion behavior.
    pub motion: BodyMotion3d,
    /// Initial world position in metres.
    pub position: [f32; 3],
    /// Initial unit quaternion in `(x, y, z, w)` order.
    pub rotation: [f32; 4],
    /// Initial linear velocity in metres per second.
    pub linear_velocity: [f32; 3],
    /// Optional dynamic-body mass in kilograms. Jolt computes mass from the shape when absent.
    pub mass: Option<f32>,
}

impl BodyDesc3d {
    /// Creates an identity-oriented body at the origin with no initial velocity.
    pub const fn new(shape: BodyShape3d, motion: BodyMotion3d) -> Self {
        Self {
            shape,
            motion,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            linear_velocity: [0.0; 3],
            mass: None,
        }
    }

    fn validate(self) -> Result<(), PhysicsError> {
        if !self
            .position
            .into_iter()
            .all(|value| bounded(value, MAX_POSITION_METERS))
            || !self
                .linear_velocity
                .into_iter()
                .all(|value| bounded(value, MAX_LINEAR_SPEED))
        {
            return Err(PhysicsError::InvalidInput(
                "3D body position or velocity is outside supported bounds",
            ));
        }
        if !self.rotation.into_iter().all(f32::is_finite) {
            return Err(PhysicsError::InvalidInput(
                "3D body rotation must be finite",
            ));
        }
        let rotation_length = self.rotation.iter().map(|v| v * v).sum::<f32>();
        if (rotation_length - 1.0).abs() > 1.0e-5 {
            return Err(PhysicsError::InvalidInput(
                "3D body rotation must be a unit quaternion",
            ));
        }
        if let Some(mass) = self.mass
            && (!mass.is_finite() || mass <= 0.0 || mass > MAX_MASS_KILOGRAMS)
        {
            return Err(PhysicsError::InvalidInput(
                "3D body mass must be finite and positive",
            ));
        }
        if self.motion != BodyMotion3d::Dynamic && self.mass.is_some() {
            return Err(PhysicsError::InvalidInput(
                "only dynamic 3D bodies can override mass",
            ));
        }
        match self.shape {
            BodyShape3d::Sphere { radius } if shape_size(radius) => Ok(()),
            BodyShape3d::Box { half_extents } if half_extents.into_iter().all(shape_size) => Ok(()),
            BodyShape3d::Capsule {
                half_height,
                radius,
            } if shape_size(half_height) && shape_size(radius) => Ok(()),
            _ => Err(PhysicsError::InvalidInput(
                "3D collider dimensions must be finite and positive",
            )),
        }
    }
}

/// Position, orientation, and linear velocity sampled from a Jolt body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyPose3d {
    /// Position in metres.
    pub position: [f32; 3],
    /// Unit quaternion in `(x, y, z, w)` order.
    pub rotation: [f32; 4],
    /// Linear velocity in metres per second.
    pub linear_velocity: [f32; 3],
}

/// A ray query result tagged with the world revision at which it was read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit3d {
    /// Daedalus body handle for the hit.
    pub body: PhysicsBodyHandle,
    /// Hit point in world space, in metres.
    pub point: [f32; 3],
    /// Outward surface normal of the hit body.
    pub normal: [f32; 3],
    /// Distance from the ray origin to the hit, in metres.
    pub distance: f32,
    /// World revision observed by the query.
    pub revision: Revision,
}

/// One owned Jolt 5.6 world.
pub struct Physics3dWorld {
    world: PhysicsWorld,
    handles: HandleArena<JoltBodyId>,
    by_backend_id: HashMap<JoltBodyId, PhysicsBodyHandle>,
    fixed_timestep_secs: f32,
    max_bodies: u32,
    tick: u64,
    revision: Revision,
    poisoned: bool,
}

impl Physics3dWorld {
    /// Creates a Jolt world with a bounded native worker pool and checked world settings.
    pub fn new(config: Physics3dConfig) -> Result<Self, PhysicsError> {
        config.validate()?;
        let session = SessionId::fresh();
        let settings = WorldSettings::default()
            .gravity(Vec3::new(
                config.gravity[0],
                config.gravity[1],
                config.gravity[2],
            ))
            .max_bodies(config.max_bodies)
            .worker_threads(config.worker_threads);
        let world = PhysicsWorld::new(settings)
            .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?;
        Ok(Self {
            world,
            handles: HandleArena::new(session),
            by_backend_id: HashMap::new(),
            fixed_timestep_secs: config.fixed_timestep_secs,
            max_bodies: config.max_bodies,
            tick: 0,
            revision: Revision(0),
            poisoned: false,
        })
    }

    /// Revision of this world’s current authoritative physics state.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Number of completed fixed simulation steps.
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Live body count.
    pub fn body_count(&self) -> usize {
        self.handles.len()
    }

    /// Creates a body and returns a world-scoped, generation-checked handle.
    pub fn create_body(&mut self, desc: BodyDesc3d) -> Result<PhysicsBodyHandle, PhysicsError> {
        self.ensure_healthy()?;
        desc.validate()?;
        if self.handles.len() >= self.max_bodies as usize {
            return Err(PhysicsError::BodyCapacityReached);
        }
        let next_revision = next_revision(self.revision)?;
        self.handles.prepare_insert()?;
        self.by_backend_id
            .try_reserve(1)
            .map_err(|_| PhysicsError::AllocationFailed)?;

        let shape = match desc.shape {
            BodyShape3d::Sphere { radius } => Shape::new_sphere(radius),
            BodyShape3d::Box { half_extents } => {
                Shape::new_box(Vec3::new(half_extents[0], half_extents[1], half_extents[2]))
            }
            BodyShape3d::Capsule {
                half_height,
                radius,
            } => Shape::new_capsule(half_height, radius),
        }
        .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?;
        let mut settings = match desc.motion {
            BodyMotion3d::Static => BodySettings::new_static(),
            BodyMotion3d::Dynamic => BodySettings::new_dynamic(),
            BodyMotion3d::Kinematic => BodySettings::new_kinematic(),
        }
        .position(RVec3::new(
            desc.position[0],
            desc.position[1],
            desc.position[2],
        ))
        .rotation(Quat::from_xyzw(
            desc.rotation[0],
            desc.rotation[1],
            desc.rotation[2],
            desc.rotation[3],
        ))
        .linear_velocity(Vec3::new(
            desc.linear_velocity[0],
            desc.linear_velocity[1],
            desc.linear_velocity[2],
        ));
        if let Some(mass) = desc.mass {
            settings = settings.mass(mass);
        }
        let backend_id = self
            .world
            .create_body(&shape, &settings)
            .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?;
        let handle = self.handles.insert_prepared(backend_id);
        self.by_backend_id.insert(backend_id, handle);
        self.revision = next_revision;
        Ok(handle)
    }

    /// Removes a body. The removed handle becomes stale and its slot is generation-bumped.
    pub fn remove_body(&mut self, handle: PhysicsBodyHandle) -> Result<(), PhysicsError> {
        self.ensure_healthy()?;
        let backend_id = *self.handles.get(handle)?;
        let next_revision = next_revision(self.revision)?;
        self.handles.prepare_remove(handle)?;
        self.world
            .remove_body(backend_id)
            .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?;
        self.by_backend_id.remove(&backend_id);
        self.handles.remove_prepared(handle)?;
        self.revision = next_revision;
        Ok(())
    }

    /// Reads a body’s current transform and linear velocity.
    pub fn body_pose(&self, handle: PhysicsBodyHandle) -> Result<BodyPose3d, PhysicsError> {
        self.ensure_healthy()?;
        let backend_id = *self.handles.get(handle)?;
        let body = self
            .world
            .body(backend_id)
            .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?;
        let p = body.position();
        let q = body.rotation();
        let v = body.linear_velocity();
        Ok(BodyPose3d {
            position: [p.x, p.y, p.z],
            rotation: [q.x, q.y, q.z, q.w],
            linear_velocity: [v.x, v.y, v.z],
        })
    }

    /// Advances exactly one configured fixed timestep. This adapter does not accumulate wall time.
    pub fn step(&mut self) -> Result<Revision, PhysicsError> {
        self.ensure_healthy()?;
        let next_revision = next_revision(self.revision)?;
        let next_tick = self
            .tick
            .checked_add(1)
            .ok_or(PhysicsError::RevisionOverflow)?;
        // A native failure can occur after Jolt has mutated some bodies. Publish a new revision
        // before crossing that boundary, and poison the world on any uncertain result.
        self.tick = next_tick;
        self.revision = next_revision;
        let report = match self.world.step(self.fixed_timestep_secs) {
            Ok(report) => report,
            Err(error) => {
                self.poisoned = true;
                return Err(PhysicsError::Backend(format!("{error:?}")));
            }
        };
        if !report.is_complete() {
            self.poisoned = true;
            return Err(PhysicsError::Backend(
                "Jolt returned an incomplete fixed step".to_owned(),
            ));
        }
        Ok(self.revision)
    }

    /// Casts a finite ray and reports the nearest hit, if any.
    pub fn ray_cast(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
    ) -> Result<Option<RayHit3d>, PhysicsError> {
        self.ensure_healthy()?;
        if !origin
            .into_iter()
            .all(|value| bounded(value, MAX_POSITION_METERS))
            || !direction
                .into_iter()
                .all(|value| value.is_finite() && value.abs() <= MAX_POSITION_METERS)
            || !max_distance.is_finite()
            || max_distance <= 0.0
            || max_distance > MAX_RAY_DISTANCE
        {
            return Err(PhysicsError::InvalidInput(
                "3D ray origin, direction, or distance is outside supported bounds",
            ));
        }
        let magnitude = length_squared3(direction).sqrt();
        if !magnitude.is_finite() || magnitude <= f32::EPSILON {
            return Err(PhysicsError::InvalidInput(
                "3D ray direction must have non-zero length",
            ));
        }
        let scale = max_distance / magnitude;
        let ray_direction = [
            direction[0] * scale,
            direction[1] * scale,
            direction[2] * scale,
        ];
        if !ray_direction.into_iter().all(f32::is_finite) {
            return Err(PhysicsError::InvalidInput(
                "3D ray length overflows the supported range",
            ));
        }
        let ray = RayCast::new(
            RVec3::new(origin[0], origin[1], origin[2]),
            Vec3::new(ray_direction[0], ray_direction[1], ray_direction[2]),
        );
        let Some(hit) = self
            .world
            .cast_ray(&ray, &QueryFilter::default())
            .map_err(|error| PhysicsError::Backend(format!("{error:?}")))?
        else {
            return Ok(None);
        };
        let handle = self
            .by_backend_id
            .get(&hit.body)
            .copied()
            .ok_or_else(|| PhysicsError::Backend("Jolt hit an unmanaged body".to_owned()))?;
        let point = ray.point_at(hit.fraction);
        Ok(Some(RayHit3d {
            body: handle,
            point: [point.x, point.y, point.z],
            normal: [hit.normal.x, hit.normal.y, hit.normal.z],
            distance: hit.distance,
            revision: self.revision,
        }))
    }

    fn ensure_healthy(&self) -> Result<(), PhysicsError> {
        if self.poisoned {
            Err(PhysicsError::WorldPoisoned)
        } else {
            Ok(())
        }
    }
}

fn bounded(value: f32, limit: f32) -> bool {
    value.is_finite() && value.abs() <= limit
}

fn shape_size(value: f32) -> bool {
    value.is_finite() && value > 0.0 && value <= MAX_SHAPE_SIZE_METERS
}

fn length_squared3(value: [f32; 3]) -> f32 {
    value[0] * value[0] + value[1] * value[1] + value[2] * value[2]
}

#[cfg(test)]
mod tests {
    use super::{BodyDesc3d, BodyMotion3d, BodyShape3d, Physics3dConfig, Physics3dWorld};
    use crate::arena::PhysicsError;

    #[test]
    fn jolt_falls_on_floor_and_ray_query_tags_revision() {
        let mut world = Physics3dWorld::new(Physics3dConfig::default()).unwrap();
        let floor = world
            .create_body(BodyDesc3d {
                shape: BodyShape3d::Box {
                    half_extents: [10.0, 0.5, 10.0],
                },
                position: [0.0, -0.5, 0.0],
                ..BodyDesc3d::new(
                    BodyShape3d::Box {
                        half_extents: [10.0, 0.5, 10.0],
                    },
                    BodyMotion3d::Static,
                )
            })
            .unwrap();
        let ball = world
            .create_body(BodyDesc3d {
                position: [0.0, 3.0, 0.0],
                ..BodyDesc3d::new(BodyShape3d::Sphere { radius: 0.5 }, BodyMotion3d::Dynamic)
            })
            .unwrap();
        for _ in 0..240 {
            world.step().unwrap();
        }
        let pose = world.body_pose(ball).unwrap();
        assert!(
            pose.position[1] > 0.35,
            "ball tunneled through floor: {pose:?}"
        );
        assert!(
            pose.position[1] < 0.8,
            "ball did not settle on floor: {pose:?}"
        );
        let hit = world
            .ray_cast([0.0, 2.0, 0.0], [0.0, -1.0, 0.0], 4.0)
            .unwrap()
            .unwrap();
        assert_eq!(hit.body, ball);
        assert_eq!(hit.revision, world.revision());
        assert_ne!(hit.body, floor);
    }

    #[test]
    fn invalid_finite_inputs_and_stale_handles_are_rejected() {
        assert!(matches!(
            Physics3dWorld::new(Physics3dConfig {
                worker_threads: 9,
                ..Physics3dConfig::default()
            }),
            Err(PhysicsError::InvalidInput(_))
        ));
        let mut world = Physics3dWorld::new(Physics3dConfig::default()).unwrap();
        let handle = world
            .create_body(BodyDesc3d::new(
                BodyShape3d::Sphere { radius: 1.0 },
                BodyMotion3d::Dynamic,
            ))
            .unwrap();
        world.remove_body(handle).unwrap();
        assert_eq!(world.body_pose(handle), Err(PhysicsError::StaleBodyHandle));
        assert!(matches!(
            world.ray_cast([f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], 1.0),
            Err(PhysicsError::InvalidInput(_))
        ));
        assert!(matches!(
            world.create_body(BodyDesc3d {
                position: [f32::MAX, 0.0, 0.0],
                ..BodyDesc3d::new(BodyShape3d::Sphere { radius: 1.0 }, BodyMotion3d::Dynamic,)
            }),
            Err(PhysicsError::InvalidInput(_))
        ));
    }

    #[test]
    fn multiple_jolt_worlds_can_initialize_step_and_drop_across_threads() {
        let workers = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut world = Physics3dWorld::new(Physics3dConfig::default()).unwrap();
                    let handle = world
                        .create_body(BodyDesc3d::new(
                            BodyShape3d::Sphere { radius: 0.5 },
                            BodyMotion3d::Dynamic,
                        ))
                        .unwrap();
                    for _ in 0..4 {
                        world.step().unwrap();
                    }
                    assert!(world.body_pose(handle).unwrap().position[1] < 0.0);
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
