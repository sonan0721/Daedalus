//! Narrow renderer fixtures for validating depth, camera transforms, directional
//! lighting, metallic/roughness response, tone mapping, and PNG capture. These
//! fixtures are engine tests; they are not playable sample content.

use glam::Mat4;

use crate::{Camera, DirectionalLight, Material, Mesh, RenderObject, RenderScene, Vertex};

/// Returns a fixed material-comparison scene containing a ground plane and
/// three smooth spheres with different metalness/roughness values.
pub fn metal_roughness_scene() -> RenderScene {
    let mut scene = RenderScene {
        camera: Camera {
            position: [4.1, 3.0, 7.8],
            target: [0.0, 0.72, 0.0],
            up: [0.0, 1.0, 0.0],
            vertical_fov_radians: 50.0_f32.to_radians(),
            near: 0.1,
            far: 100.0,
        },
        directional_light: DirectionalLight {
            direction: [-0.55, -0.9, -0.35],
            color: [1.0, 0.93, 0.82],
            intensity: 4.5,
        },
        ambient_color: [0.14, 0.17, 0.22],
        clear_color: [0.026, 0.035, 0.052, 1.0],
        objects: Vec::new(),
    };
    scene.objects.push(RenderObject::new(
        ground_plane(7.0),
        Mat4::IDENTITY,
        Material {
            base_color: [0.27, 0.31, 0.38],
            metallic: 0.0,
            roughness: 0.82,
            emissive: [0.0; 3],
        },
    ));
    let sphere = uv_sphere(1.0, 24, 32);
    let materials = [
        Material {
            base_color: [0.72, 0.12, 0.055],
            metallic: 0.0,
            roughness: 0.72,
            emissive: [0.0; 3],
        },
        Material {
            base_color: [0.78, 0.52, 0.16],
            metallic: 1.0,
            roughness: 0.16,
            emissive: [0.0; 3],
        },
        Material {
            base_color: [0.08, 0.28, 0.68],
            metallic: 0.72,
            roughness: 0.56,
            emissive: [0.0; 3],
        },
    ];
    for (index, material) in materials.into_iter().enumerate() {
        let x = (index as f32 - 1.0) * 2.15;
        scene.objects.push(RenderObject::new(
            sphere.clone(),
            Mat4::from_translation(glam::Vec3::new(x, 1.0, 0.0)),
            material,
        ));
    }
    scene
}

fn ground_plane(size: f32) -> Mesh {
    let extent = size * 0.5;
    Mesh::new(
        vec![
            Vertex::new([-extent, 0.0, -extent], [0.0, 1.0, 0.0]),
            Vertex::new([extent, 0.0, -extent], [0.0, 1.0, 0.0]),
            Vertex::new([extent, 0.0, extent], [0.0, 1.0, 0.0]),
            Vertex::new([-extent, 0.0, extent], [0.0, 1.0, 0.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn uv_sphere(radius: f32, rings: u32, segments: u32) -> Mesh {
    let mut vertices = Vec::with_capacity(((rings + 1) * (segments + 1)) as usize);
    for ring in 0..=rings {
        let latitude = std::f32::consts::PI * ring as f32 / rings as f32;
        let y = latitude.cos();
        let radial = latitude.sin();
        for segment in 0..=segments {
            let longitude = 2.0 * std::f32::consts::PI * segment as f32 / segments as f32;
            let normal = [radial * longitude.cos(), y, radial * longitude.sin()];
            vertices.push(Vertex::new(
                [normal[0] * radius, normal[1] * radius, normal[2] * radius],
                normal,
            ));
        }
    }
    let mut indices = Vec::with_capacity((rings * segments * 6) as usize);
    let row = segments + 1;
    for ring in 0..rings {
        for segment in 0..segments {
            let top_left = ring * row + segment;
            let top_right = top_left + 1;
            let bottom_left = top_left + row;
            let bottom_right = bottom_left + 1;
            indices.extend_from_slice(&[
                top_left,
                bottom_left,
                top_right,
                top_right,
                bottom_left,
                bottom_right,
            ]);
        }
    }
    Mesh::new(vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::metal_roughness_scene;

    #[test]
    fn metal_roughness_fixture_is_valid_and_has_multiple_materials() {
        let scene = metal_roughness_scene();
        scene.validate().expect("fixture scene should validate");
        assert_eq!(scene.objects.len(), 4);
        assert_eq!(scene.objects[1].material.metallic, 0.0);
        assert_eq!(scene.objects[2].material.metallic, 1.0);
        assert_ne!(
            scene.objects[1].material.roughness,
            scene.objects[2].material.roughness
        );
    }
}
