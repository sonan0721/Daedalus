use std::collections::HashMap;

use daedalus_core::{Revision, SessionId};
use rapier2d::prelude::{
    ColliderBuilder, ColliderHandle, PhysicsWorld as RapierPhysicsWorld, QueryFilter, Ray,
    RigidBodyBuilder, RigidBodyHandle, Vector,
};

use crate::arena::{HandleArena, PhysicsBodyHandle, PhysicsError, next_revision};

const MAX_BODIES: u32 = 1_000_000;
const MAX_FIXED_STEP: f32 = 0.1;
const MAX_POSITION_METERS: f32 = 1_000_000.0;
const MAX_LINEAR_SPEED: f32 = 100_000.0;
const MAX_SHAPE_SIZE_METERS: f32 = 100_000.0;
const MAX_RAY_DISTANCE: f32 = 1_000_000.0;
const MAX_MASS_KILOGRAMS: f32 = 1_000_000_000.0;
const MAX_FRICTION: f32 = 100.0;

/// Configuration for a single-threaded Rapier 2D world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Physics2dConfig {
    /// Gravity in metres per second squared.
    pub gravity: [f32; 2],
    /// The single simulation step used by each call to [`Physics2dWorld::step`].
    pub fixed_timestep_secs: f32,
    /// Maximum number of bodies in this world.
    pub max_bodies: u32,
}

impl Default for Physics2dConfig {
    fn default() -> Self {
        Self {
            gravity: [0.0, -9.81],
            fixed_timestep_secs: 1.0 / 60.0,
            max_bodies: 10_240,
        }
    }
}

impl Physics2dConfig {
    fn validate(self) -> Result<(), PhysicsError> {
        if !self.gravity.into_iter().all(f32::is_finite)
            || length_squared2(self.gravity) > 100.0 * 100.0
        {
            return Err(PhysicsError::InvalidInput(
                "2D gravity must be finite and no stronger than 100 m/s²",
            ));
        }
        if !self.fixed_timestep_secs.is_finite()
            || self.fixed_timestep_secs <= 0.0
            || self.fixed_timestep_secs > MAX_FIXED_STEP
        {
            return Err(PhysicsError::InvalidInput(
                "2D fixed timestep must be finite and in (0, 0.1] seconds",
            ));
        }
        if !(1..=MAX_BODIES).contains(&self.max_bodies) {
            return Err(PhysicsError::InvalidInput(
                "2D max_bodies must be between 1 and 1,000,000",
            ));
        }
        Ok(())
    }
}

/// Supported primitive 2D collider shapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyShape2d {
    /// Circle with radius in metres.
    Ball { radius: f32 },
    /// Axis-aligned box in body-local coordinates, using positive half extents in metres.
    Box { half_extents: [f32; 2] },
}

/// Static, dynamic, or engine-driven kinematic body motion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyMotion2d {
    /// Never moved by simulation.
    Static,
    /// Integrated and solved by Rapier.
    Dynamic,
    /// Moved by the host; not integrated by gravity.
    Kinematic,
}

/// Description used to create a Rapier rigid body with one collider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyDesc2d {
    /// Collider shape.
    pub shape: BodyShape2d,
    /// Motion behavior.
    pub motion: BodyMotion2d,
    /// Initial world position in metres.
    pub position: [f32; 2],
    /// Initial body rotation in radians.
    pub rotation_radians: f32,
    /// Initial linear velocity in metres per second.
    pub linear_velocity: [f32; 2],
    /// Optional total dynamic-body mass in kilograms.
    pub mass: Option<f32>,
    /// Coulomb friction coefficient, finite and non-negative.
    pub friction: f32,
    /// Restitution coefficient in `[0, 1]`.
    pub restitution: f32,
}

impl BodyDesc2d {
    /// Creates an identity-oriented body at the origin with default surface properties.
    pub const fn new(shape: BodyShape2d, motion: BodyMotion2d) -> Self {
        Self {
            shape,
            motion,
            position: [0.0; 2],
            rotation_radians: 0.0,
            linear_velocity: [0.0; 2],
            mass: None,
            friction: 0.5,
            restitution: 0.0,
        }
    }

    fn validate(self) -> Result<f32, PhysicsError> {
        if !self
            .position
            .into_iter()
            .all(|value| bounded(value, MAX_POSITION_METERS))
            || !self
                .linear_velocity
                .into_iter()
                .all(|value| bounded(value, MAX_LINEAR_SPEED))
            || !self.rotation_radians.is_finite()
            || self.rotation_radians.abs() > 1_000_000.0
        {
            return Err(PhysicsError::InvalidInput(
                "2D body position, rotation, or velocity is outside supported bounds",
            ));
        }
        if !self.friction.is_finite() || !(0.0..=MAX_FRICTION).contains(&self.friction) {
            return Err(PhysicsError::InvalidInput(
                "2D body friction must be in [0, 100]",
            ));
        }
        if !self.restitution.is_finite() || !(0.0..=1.0).contains(&self.restitution) {
            return Err(PhysicsError::InvalidInput(
                "2D body restitution must be finite and in [0, 1]",
            ));
        }
        if self.motion == BodyMotion2d::Static
            && self
                .linear_velocity
                .into_iter()
                .any(|component| component != 0.0)
        {
            return Err(PhysicsError::InvalidInput(
                "static 2D body cannot have initial velocity",
            ));
        }
        if self.motion != BodyMotion2d::Dynamic && self.mass.is_some() {
            return Err(PhysicsError::InvalidInput(
                "only dynamic 2D bodies can override mass",
            ));
        }
        if let Some(mass) = self.mass
            && (!mass.is_finite() || mass <= 0.0 || mass > MAX_MASS_KILOGRAMS)
        {
            return Err(PhysicsError::InvalidInput(
                "2D body mass must be finite and positive",
            ));
        }
        let area = match self.shape {
            BodyShape2d::Ball { radius } if shape_size(radius) => {
                std::f32::consts::PI * radius * radius
            }
            BodyShape2d::Box { half_extents } if half_extents.into_iter().all(shape_size) => {
                4.0 * half_extents[0] * half_extents[1]
            }
            _ => {
                return Err(PhysicsError::InvalidInput(
                    "2D collider dimensions must be finite and positive",
                ));
            }
        };
        if !area.is_finite() || area <= 0.0 {
            return Err(PhysicsError::InvalidInput(
                "2D collider area is outside supported bounds",
            ));
        }
        if let Some(mass) = self.mass {
            let density = mass / area;
            if !density.is_finite() || density <= 0.0 {
                return Err(PhysicsError::InvalidInput(
                    "2D mass and collider area produce unsupported density",
                ));
            }
        }
        Ok(area)
    }
}

/// Position, orientation, and linear velocity sampled from a Rapier body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyPose2d {
    /// Position in metres.
    pub position: [f32; 2],
    /// Rotation in radians.
    pub rotation_radians: f32,
    /// Linear velocity in metres per second.
    pub linear_velocity: [f32; 2],
}

/// A ray query result tagged with the world revision at which it was read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit2d {
    /// Daedalus body handle for the hit.
    pub body: PhysicsBodyHandle,
    /// Hit point in world space, in metres.
    pub point: [f32; 2],
    /// Outward surface normal of the hit body.
    pub normal: [f32; 2],
    /// Distance from the ray origin to the hit, in metres.
    pub distance: f32,
    /// World revision observed by the query.
    pub revision: Revision,
}

/// One owned Rapier 2D world. The selected dependency omits Rapier's parallel feature.
pub struct Physics2dWorld {
    world: RapierPhysicsWorld,
    handles: HandleArena<(RigidBodyHandle, ColliderHandle)>,
    by_collider: HashMap<ColliderHandle, PhysicsBodyHandle>,
    max_bodies: u32,
    tick: u64,
    revision: Revision,
}

impl Physics2dWorld {
    /// Creates a Rapier world with fixed-step integration and bounded body storage.
    pub fn new(config: Physics2dConfig) -> Result<Self, PhysicsError> {
        config.validate()?;
        let session = SessionId::fresh();
        let mut world = RapierPhysicsWorld::new();
        world.gravity = Vector::new(config.gravity[0], config.gravity[1]);
        world.integration_parameters.dt = config.fixed_timestep_secs;
        Ok(Self {
            world,
            handles: HandleArena::new(session),
            by_collider: HashMap::new(),
            max_bodies: config.max_bodies,
            tick: 0,
            revision: Revision(0),
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
    pub fn create_body(&mut self, desc: BodyDesc2d) -> Result<PhysicsBodyHandle, PhysicsError> {
        let area = desc.validate()?;
        if self.handles.len() >= self.max_bodies as usize {
            return Err(PhysicsError::BodyCapacityReached);
        }
        let next_revision = next_revision(self.revision)?;
        self.handles.prepare_insert()?;
        self.by_collider
            .try_reserve(1)
            .map_err(|_| PhysicsError::AllocationFailed)?;

        let body_builder = match desc.motion {
            BodyMotion2d::Static => RigidBodyBuilder::fixed(),
            BodyMotion2d::Dynamic => RigidBodyBuilder::dynamic(),
            BodyMotion2d::Kinematic => RigidBodyBuilder::kinematic_position_based(),
        }
        .translation(Vector::new(desc.position[0], desc.position[1]))
        .rotation(desc.rotation_radians)
        .linvel(Vector::new(
            desc.linear_velocity[0],
            desc.linear_velocity[1],
        ));
        let mut collider_builder = match desc.shape {
            BodyShape2d::Ball { radius } => ColliderBuilder::ball(radius),
            BodyShape2d::Box { half_extents } => {
                ColliderBuilder::cuboid(half_extents[0], half_extents[1])
            }
        }
        .friction(desc.friction)
        .restitution(desc.restitution);
        if desc.motion == BodyMotion2d::Dynamic {
            collider_builder = collider_builder.density(desc.mass.map_or(1.0, |mass| mass / area));
        }
        let (rigid_body, collider) = self.world.insert(body_builder, collider_builder);
        let handle = self.handles.insert_prepared((rigid_body, collider));
        self.by_collider.insert(collider, handle);
        self.refresh_queries();
        self.revision = next_revision;
        Ok(handle)
    }

    /// Removes a body and its attached collider. The handle becomes stale.
    pub fn remove_body(&mut self, handle: PhysicsBodyHandle) -> Result<(), PhysicsError> {
        let (rigid_body, collider) = *self.handles.get(handle)?;
        let next_revision = next_revision(self.revision)?;
        self.handles.prepare_remove(handle)?;
        let removed = self.world.bodies.remove(
            rigid_body,
            &mut self.world.islands,
            &mut self.world.colliders,
            &mut self.world.impulse_joints,
            &mut self.world.multibody_joints,
            &mut self.world.soft_bodies,
            true,
        );
        if removed.is_none() {
            return Err(PhysicsError::StaleBodyHandle);
        }
        self.by_collider.remove(&collider);
        self.handles.remove_prepared(handle)?;
        self.refresh_queries();
        self.revision = next_revision;
        Ok(())
    }

    /// Reads a body’s current transform and linear velocity.
    pub fn body_pose(&self, handle: PhysicsBodyHandle) -> Result<BodyPose2d, PhysicsError> {
        let (rigid_body, _) = self.handles.get(handle)?;
        let body = self
            .world
            .bodies
            .get(*rigid_body)
            .ok_or(PhysicsError::StaleBodyHandle)?;
        Ok(BodyPose2d {
            position: [body.translation().x, body.translation().y],
            rotation_radians: body.rotation().angle(),
            linear_velocity: [body.linvel().x, body.linvel().y],
        })
    }

    /// Advances exactly one configured fixed timestep. This adapter does not accumulate wall time.
    pub fn step(&mut self) -> Result<Revision, PhysicsError> {
        let next_revision = next_revision(self.revision)?;
        let next_tick = self
            .tick
            .checked_add(1)
            .ok_or(PhysicsError::RevisionOverflow)?;
        self.world.step();
        self.tick = next_tick;
        self.revision = next_revision;
        Ok(self.revision)
    }

    /// Casts a finite ray and reports the nearest hit, if any.
    pub fn ray_cast(
        &self,
        origin: [f32; 2],
        direction: [f32; 2],
        max_distance: f32,
    ) -> Result<Option<RayHit2d>, PhysicsError> {
        if !origin
            .into_iter()
            .all(|value| bounded(value, MAX_POSITION_METERS))
            || !direction
                .into_iter()
                .all(|value| value.is_finite() && value.abs() <= MAX_POSITION_METERS)
            || !max_distance.is_finite()
            || !(0.0..=MAX_RAY_DISTANCE).contains(&max_distance)
            || max_distance == 0.0
        {
            return Err(PhysicsError::InvalidInput(
                "2D ray origin, direction, or distance is outside supported bounds",
            ));
        }
        let magnitude = length_squared2(direction).sqrt();
        if !magnitude.is_finite() || magnitude <= f32::EPSILON {
            return Err(PhysicsError::InvalidInput(
                "2D ray direction must have non-zero length",
            ));
        }
        let ray = Ray::new(
            Vector::new(origin[0], origin[1]),
            Vector::new(direction[0] / magnitude, direction[1] / magnitude),
        );
        let Some((collider, intersection)) =
            self.world
                .cast_ray_and_get_normal(&ray, max_distance, true, QueryFilter::default())
        else {
            return Ok(None);
        };
        let handle =
            self.by_collider.get(&collider).copied().ok_or_else(|| {
                PhysicsError::Backend("Rapier hit an unmanaged collider".to_owned())
            })?;
        let point = ray.point_at(intersection.time_of_impact);
        Ok(Some(RayHit2d {
            body: handle,
            point: [point.x, point.y],
            normal: [intersection.normal.x, intersection.normal.y],
            distance: intersection.time_of_impact,
            revision: self.revision,
        }))
    }

    fn refresh_queries(&mut self) {
        // A zero-time step applies Rapier's queued insert/remove changes and refreshes its
        // broad-phase query tree without advancing simulation time.
        let fixed_timestep = self.world.integration_parameters.dt;
        self.world.integration_parameters.dt = 0.0;
        self.world.step();
        self.world.integration_parameters.dt = fixed_timestep;
    }
}

fn bounded(value: f32, limit: f32) -> bool {
    value.is_finite() && value.abs() <= limit
}

fn shape_size(value: f32) -> bool {
    value.is_finite() && (0.0..=MAX_SHAPE_SIZE_METERS).contains(&value) && value > 0.0
}

fn length_squared2(value: [f32; 2]) -> f32 {
    value[0] * value[0] + value[1] * value[1]
}

#[cfg(test)]
mod tests {
    use super::{BodyDesc2d, BodyMotion2d, BodyShape2d, Physics2dConfig, Physics2dWorld};
    use crate::arena::PhysicsError;

    #[test]
    fn rapier_falls_on_floor_and_ray_query_tags_revision() {
        let mut world = Physics2dWorld::new(Physics2dConfig::default()).unwrap();
        let floor = world
            .create_body(BodyDesc2d {
                position: [0.0, -0.5],
                ..BodyDesc2d::new(
                    BodyShape2d::Box {
                        half_extents: [10.0, 0.5],
                    },
                    BodyMotion2d::Static,
                )
            })
            .unwrap();
        let ball = world
            .create_body(BodyDesc2d {
                position: [0.0, 3.0],
                ..BodyDesc2d::new(BodyShape2d::Ball { radius: 0.5 }, BodyMotion2d::Dynamic)
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
            .ray_cast([0.0, 2.0], [0.0, -1.0], 4.0)
            .unwrap()
            .unwrap();
        assert_eq!(hit.body, ball);
        assert_eq!(hit.revision, world.revision());
        assert_ne!(hit.body, floor);
    }

    #[test]
    fn invalid_finite_inputs_and_stale_handles_are_rejected() {
        assert!(matches!(
            Physics2dWorld::new(Physics2dConfig {
                fixed_timestep_secs: f32::INFINITY,
                ..Physics2dConfig::default()
            }),
            Err(PhysicsError::InvalidInput(_))
        ));
        let mut world = Physics2dWorld::new(Physics2dConfig::default()).unwrap();
        let handle = world
            .create_body(BodyDesc2d::new(
                BodyShape2d::Ball { radius: 1.0 },
                BodyMotion2d::Dynamic,
            ))
            .unwrap();
        assert!(
            world
                .ray_cast([-2.0, 0.0], [1.0, 0.0], 4.0)
                .unwrap()
                .is_some()
        );
        world.remove_body(handle).unwrap();
        assert_eq!(world.body_pose(handle), Err(PhysicsError::StaleBodyHandle));
        assert!(
            world
                .ray_cast([-2.0, 0.0], [1.0, 0.0], 4.0)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            world.ray_cast([f32::NAN, 0.0], [1.0, 0.0], 1.0),
            Err(PhysicsError::InvalidInput(_))
        ));
        assert!(matches!(
            world.create_body(BodyDesc2d {
                position: [f32::MAX, 0.0],
                ..BodyDesc2d::new(BodyShape2d::Ball { radius: 1.0 }, BodyMotion2d::Dynamic,)
            }),
            Err(PhysicsError::InvalidInput(_))
        ));
    }
}
