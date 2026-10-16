use glam::{Mat3, Mat4, Vec3};

use crate::RendererError;

/// Camera for a renderer-owned scene. The projection uses a right-handed view
/// with the WebGPU depth range [0, 1].
#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub up: [f32; 3],
    pub vertical_fov_radians: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: [4.2, 3.0, 6.0],
            target: [0.0, 0.5, 0.0],
            up: [0.0, 1.0, 0.0],
            vertical_fov_radians: 55.0_f32.to_radians(),
            near: 0.1,
            far: 100.0,
        }
    }
}

impl Camera {
    pub(crate) fn validate(&self) -> Result<(), RendererError> {
        if !self.position.iter().all(|value| value.is_finite())
            || !self.target.iter().all(|value| value.is_finite())
            || !self.up.iter().all(|value| value.is_finite())
        {
            return Err(RendererError::InvalidScene(
                "camera vectors must contain finite values".into(),
            ));
        }
        let position = Vec3::from_array(self.position);
        let target = Vec3::from_array(self.target);
        let up = Vec3::from_array(self.up);
        if position.distance_squared(target) < 1.0e-8 || up.length_squared() < 1.0e-8 {
            return Err(RendererError::InvalidScene(
                "camera position, target, and up vector must define a view".into(),
            ));
        }
        if (target - position).cross(up).length_squared() < 1.0e-8 {
            return Err(RendererError::InvalidScene(
                "camera up vector cannot be parallel to its view direction".into(),
            ));
        }
        if !self.vertical_fov_radians.is_finite()
            || self.vertical_fov_radians <= 0.01
            || self.vertical_fov_radians >= std::f32::consts::PI - 0.01
            || !self.near.is_finite()
            || !self.far.is_finite()
            || self.near <= 0.0
            || self.far <= self.near
        {
            return Err(RendererError::InvalidScene(
                "camera projection must have a finite field of view and 0 < near < far".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn view_projection(&self, aspect: f32) -> Mat4 {
        let view = Mat4::look_at_rh(
            Vec3::from_array(self.position),
            Vec3::from_array(self.target),
            Vec3::from_array(self.up),
        );
        let projection =
            Mat4::perspective_rh(self.vertical_fov_radians, aspect, self.near, self.far);
        projection * view
    }

    #[cfg(feature = "window")]
    pub(crate) fn orbit(&mut self, yaw_delta: f32, pitch_delta: f32, zoom_delta: f32) {
        let target = Vec3::from_array(self.target);
        let offset = Vec3::from_array(self.position) - target;
        let radius = offset.length().max(0.05);
        let yaw = offset.z.atan2(offset.x) + yaw_delta;
        let mut pitch = (offset.y / radius).clamp(-1.0, 1.0).asin() + pitch_delta;
        pitch = pitch.clamp(-1.48, 1.48);
        let radius = (radius * zoom_delta.exp()).clamp(0.2, 500.0);
        let horizontal = radius * pitch.cos();
        let position = target
            + Vec3::new(
                horizontal * yaw.cos(),
                radius * pitch.sin(),
                horizontal * yaw.sin(),
            );
        if position.is_finite() {
            self.position = position.to_array();
        }
    }
}

/// A normalized directional light. `direction` points from the light toward
/// the scene; the renderer reverses it to obtain the surface-to-light vector.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectionalLight {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub intensity: f32,
}

impl Default for DirectionalLight {
    fn default() -> Self {
        Self {
            direction: [-0.45, -0.82, -0.35],
            color: [1.0, 0.94, 0.82],
            intensity: 4.0,
        }
    }
}

impl DirectionalLight {
    pub(crate) fn validate(&self) -> Result<(), RendererError> {
        if !self.direction.iter().all(|value| value.is_finite())
            || !self.color.iter().all(|value| value.is_finite())
            || self.direction.iter().all(|value| value.abs() < 1.0e-6)
            || self.color.iter().any(|value| *value < 0.0)
            || !self.intensity.is_finite()
            || self.intensity < 0.0
        {
            return Err(RendererError::InvalidScene(
                "directional light needs a nonzero direction and finite nonnegative color/intensity".into(),
            ));
        }
        Ok(())
    }
}

/// Linear base color and scalar metallic/roughness parameters for the initial
/// opaque shading model. Base color values may exceed 1.0 for HDR emission-like
/// inputs; they must remain finite and nonnegative.
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub base_color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
}

impl Default for Material {
    fn default() -> Self {
        Self {
            base_color: [0.65, 0.68, 0.72],
            metallic: 0.0,
            roughness: 0.6,
            emissive: [0.0; 3],
        }
    }
}

impl Material {
    pub(crate) fn validate(&self) -> Result<(), RendererError> {
        if !self
            .base_color
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
            || !self
                .emissive
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
            || !self.metallic.is_finite()
            || !(0.0..=1.0).contains(&self.metallic)
            || !self.roughness.is_finite()
            || !(0.0..=1.0).contains(&self.roughness)
        {
            return Err(RendererError::InvalidScene(
                "material colors must be finite and nonnegative; metallic and roughness must be in [0, 1]".into(),
            ));
        }
        Ok(())
    }
}

/// Position and unit-length normal in mesh-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

impl Vertex {
    pub const fn new(position: [f32; 3], normal: [f32; 3]) -> Self {
        Self { position, normal }
    }
}

/// Renderer-owned indexed triangle mesh. This value contains CPU geometry; the
/// renderer uploads a bounded copy for each submitted frame in this first pass.
#[derive(Clone, Debug, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn new(vertices: Vec<Vertex>, indices: Vec<u32>) -> Self {
        Self { vertices, indices }
    }

    pub(crate) fn validate(&self) -> Result<(), RendererError> {
        if self.vertices.is_empty()
            || self.indices.is_empty()
            || !self.indices.len().is_multiple_of(3)
        {
            return Err(RendererError::InvalidScene(
                "each mesh must contain vertices and a nonempty triangle index list".into(),
            ));
        }
        if self.vertices.len() > u32::MAX as usize || self.indices.len() > u32::MAX as usize {
            return Err(RendererError::InvalidScene(
                "mesh is too large for the renderer's 32-bit draw ranges".into(),
            ));
        }
        for vertex in &self.vertices {
            if !vertex.position.iter().all(|value| value.is_finite())
                || !vertex.normal.iter().all(|value| value.is_finite())
                || vertex.normal.iter().all(|value| value.abs() < 1.0e-6)
            {
                return Err(RendererError::InvalidScene(
                    "mesh positions and nonzero normals must be finite".into(),
                ));
            }
        }
        if self
            .indices
            .iter()
            .any(|index| *index as usize >= self.vertices.len())
        {
            return Err(RendererError::InvalidScene(
                "mesh index references a vertex outside the mesh".into(),
            ));
        }
        Ok(())
    }
}

/// One mesh instance and its material. `transform` is a column-major model
/// matrix; non-uniform scale is supported through an inverse-transpose normal
/// transform.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderObject {
    pub mesh: Mesh,
    pub transform: Mat4,
    pub material: Material,
}

impl RenderObject {
    pub fn new(mesh: Mesh, transform: Mat4, material: Material) -> Self {
        Self {
            mesh,
            transform,
            material,
        }
    }
}

/// A small, self-contained scene description passed from runtime/presentation
/// code to the renderer. Geometry and material data are owned independently of
/// the live ECS; callers can build it from snapshots without sharing world
/// access or locking simulation data in the draw path.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderScene {
    pub camera: Camera,
    pub directional_light: DirectionalLight,
    pub ambient_color: [f32; 3],
    pub clear_color: [f32; 4],
    pub objects: Vec<RenderObject>,
}

impl Default for RenderScene {
    fn default() -> Self {
        Self {
            camera: Camera::default(),
            directional_light: DirectionalLight::default(),
            ambient_color: [0.16, 0.18, 0.22],
            clear_color: [0.035, 0.045, 0.065, 1.0],
            objects: Vec::new(),
        }
    }
}

impl RenderScene {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn validate(&self) -> Result<(), RendererError> {
        self.camera.validate()?;
        self.directional_light.validate()?;
        if !self
            .ambient_color
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
            || !self
                .clear_color
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
        {
            return Err(RendererError::InvalidScene(
                "ambient light must be finite/nonnegative and clear color must be in [0, 1]".into(),
            ));
        }
        if self.objects.len() > u32::MAX as usize {
            return Err(RendererError::InvalidScene(
                "scene contains too many draw objects".into(),
            ));
        }
        let mut vertex_total = 0usize;
        let mut index_total = 0usize;
        for object in &self.objects {
            object.mesh.validate()?;
            object.material.validate()?;
            if !object
                .transform
                .to_cols_array()
                .iter()
                .all(|value| value.is_finite())
            {
                return Err(RendererError::InvalidScene(
                    "object transform must contain finite values".into(),
                ));
            }
            let linear = Mat3::from_mat4(object.transform);
            let determinant = linear.determinant();
            let inverse = linear.inverse();
            if !determinant.is_finite()
                || determinant == 0.0
                || !inverse
                    .to_cols_array()
                    .iter()
                    .all(|value| value.is_finite())
            {
                return Err(RendererError::InvalidScene(
                    "object transform must have a finite inverse to transform mesh normals".into(),
                ));
            }
            let columns = object.transform.to_cols_array();
            if columns[3].abs() > 1.0e-6
                || columns[7].abs() > 1.0e-6
                || columns[11].abs() > 1.0e-6
                || (columns[15] - 1.0).abs() > 1.0e-6
            {
                return Err(RendererError::InvalidScene(
                    "object transform must be affine".into(),
                ));
            }
            vertex_total = vertex_total
                .checked_add(object.mesh.vertices.len())
                .ok_or_else(|| {
                    RendererError::InvalidScene("scene vertex count overflowed".into())
                })?;
            index_total = index_total
                .checked_add(object.mesh.indices.len())
                .ok_or_else(|| {
                    RendererError::InvalidScene("scene index count overflowed".into())
                })?;
        }
        if vertex_total > u32::MAX as usize || index_total > u32::MAX as usize {
            return Err(RendererError::InvalidScene(
                "scene exceeds 32-bit draw limits".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use glam::Mat4;

    use super::{Camera, Material, Mesh, RenderObject, RenderScene, Vertex};

    fn triangle() -> Mesh {
        Mesh::new(
            vec![
                Vertex::new([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ],
            vec![0, 1, 2],
        )
    }

    #[test]
    fn rejects_indices_outside_mesh() {
        let mut scene = RenderScene::default();
        scene.objects.push(RenderObject::new(
            Mesh::new(triangle().vertices, vec![0, 1, 9]),
            Mat4::IDENTITY,
            Material::default(),
        ));
        assert!(scene.validate().is_err());
    }

    #[test]
    fn rejects_singular_object_transform() {
        let mut scene = RenderScene::default();
        scene.objects.push(RenderObject::new(
            triangle(),
            Mat4::from_scale(glam::Vec3::new(1.0, 0.0, 1.0)),
            Material::default(),
        ));
        assert!(scene.validate().is_err());
    }

    #[test]
    fn accepts_tiny_but_invertible_object_transform() {
        let mut scene = RenderScene::default();
        scene.objects.push(RenderObject::new(
            triangle(),
            Mat4::from_scale(glam::Vec3::splat(0.001)),
            Material::default(),
        ));
        assert!(scene.validate().is_ok());
    }

    #[test]
    fn rejects_object_transform_with_non_finite_inverse() {
        let mut scene = RenderScene::default();
        scene.objects.push(RenderObject::new(
            triangle(),
            Mat4::from_scale(glam::Vec3::new(1.0e-39, 1.0e20, 1.0e20)),
            Material::default(),
        ));
        assert!(scene.validate().is_err());
    }

    #[test]
    fn rejects_degenerate_camera_and_nonphysical_material_values() {
        let mut scene = RenderScene::default();
        scene.camera = Camera {
            target: scene.camera.position,
            ..scene.camera
        };
        assert!(scene.validate().is_err());

        let mut scene = RenderScene::default();
        scene.objects.push(RenderObject::new(
            triangle(),
            Mat4::IDENTITY,
            Material {
                metallic: 1.2,
                ..Material::default()
            },
        ));
        assert!(scene.validate().is_err());
    }
}
