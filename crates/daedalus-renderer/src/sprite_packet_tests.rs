use std::{
    collections::BTreeMap,
    fs,
    io::BufReader,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use daedalus_content::{
    ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache,
    prepare_project_with_assets, project::PreparedProject, scene::StableId,
};
use daedalus_core::{
    EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
};
use glam::{Mat4, Quat, Vec3};
use serde_json::{Value, json};

use super::{
    PacketRendererLimits, ProjectRenderResources, RenderInstance, RenderProjection, RenderView,
};
use crate::{Camera, CaptureOptions, Renderer};

const ATLAS_EXTENT: u32 = 8;
const CAMERA_ID: &str = "entity:camera";
const CLEAR_LINEAR: [f32; 4] = [0.08, 0.12, 0.18, 1.0];

#[derive(Clone, Copy)]
struct SpriteSpec {
    entity: &'static str,
    atlas: &'static str,
    region: &'static str,
    tint: [f32; 4],
    sorting_layer: i32,
    order_in_layer: i32,
}

fn atlas_rgba8() -> Vec<u8> {
    let mut pixels = Vec::with_capacity((ATLAS_EXTENT * ATLAS_EXTENT * 4) as usize);
    for y in 0..ATLAS_EXTENT {
        for x in 0..ATLAS_EXTENT {
            let color = match (x >= 4, y >= 4) {
                (false, false) => [240, 40, 30, 255],
                (true, false) => [20, 250, 60, 0],
                (false, true) => [30, 80, 230, 128],
                (true, true) => [220, 190, 40, 64],
            };
            pixels.extend_from_slice(&color);
        }
    }
    pixels
}

fn encoded_atlas_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, ATLAS_EXTENT, ATLAS_EXTENT);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .expect("atlas PNG header should encode");
        writer
            .write_image_data(&atlas_rgba8())
            .expect("atlas pixels should encode");
    }
    bytes
}

fn transform_component(id: &str, translation: [f32; 3]) -> Value {
    json!({
        "id": id,
        "type_id": "daedalus.transform",
        "asset_refs": [],
        "data": {
            "translation": translation,
            "rotation": [0.0, 0.0, 0.0, 1.0],
            "scale": [1.0, 1.0, 1.0]
        }
    })
}

fn sprite_entity(spec: SpriteSpec) -> Value {
    json!({
        "id": spec.entity,
        "components": [
            transform_component(&format!("component:transform:{}", spec.entity), [0.0, 0.0, 0.0]),
            {
                "id": format!("component:sprite:{}", spec.entity),
                "type_id": "daedalus.sprite_renderer",
                "asset_refs": [spec.atlas],
                "data": {
                    "atlas": spec.atlas,
                    "region": spec.region,
                    "tint": spec.tint,
                    "sorting_layer": spec.sorting_layer,
                    "order_in_layer": spec.order_in_layer
                }
            }
        ]
    })
}

fn atlas_document() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "image_asset": "asset:image",
        "filter_policy": "nearest_no_mip_v1",
        "regions": [
            {"id":"region:asymmetric", "rect":[1,1,5,5], "pivot":[0.25,0.75], "pixels_per_unit":2.0},
            {"id":"region:red", "rect":[0,0,4,4], "pivot":[0.5,0.5], "pixels_per_unit":4.0},
            {"id":"region:transparent", "rect":[4,0,4,4], "pivot":[0.5,0.5], "pixels_per_unit":4.0},
            {"id":"region:blue", "rect":[0,4,4,4], "pivot":[0.5,0.5], "pixels_per_unit":4.0},
            {"id":"region:gold", "rect":[4,4,4,4], "pivot":[0.5,0.5], "pixels_per_unit":4.0},
            {"id":"region:full", "rect":[0,0,8,8], "pivot":[0.5,0.5], "pixels_per_unit":8.0}
        ]
    }))
    .expect("atlas document should encode")
}

fn prepare_sprite_project(specs: &[SpriteSpec]) -> PreparedProject {
    prepare_sprite_project_with_clear(specs, CLEAR_LINEAR)
}

fn prepare_sprite_project_with_clear(
    specs: &[SpriteSpec],
    clear_color: [f32; 4],
) -> PreparedProject {
    let camera = json!({
        "id": CAMERA_ID,
        "components": [
            transform_component("component:transform:camera", [0.0, 0.0, 10.0]),
            {
                "id": "component:camera2d",
                "type_id": "daedalus.camera_2d",
                "asset_refs": [],
                "data": {"vertical_size": 8.0, "near": 0.1, "far": 100.0}
            }
        ]
    });
    let mut entities = Vec::with_capacity(specs.len() + 1);
    entities.push(camera);
    entities.extend(specs.iter().copied().map(sprite_entity));
    let scene = serde_json::to_vec(&json!({
        "schema_version": 1,
        "revision": 1,
        "entities": entities,
        "assets": [
            {
                "id": "asset:image",
                "kind": "daedalus.image",
                "uri": "sprites/sheet.png",
                "metadata": {
                    "schema_version": 1,
                    "role": "sprite_atlas_srgb",
                    "mip_policy": "none",
                    "sampler": {
                        "min_filter": "nearest",
                        "mag_filter": "nearest",
                        "mipmap_filter": "none",
                        "wrap_u": "clamp_to_edge",
                        "wrap_v": "clamp_to_edge"
                    }
                }
            },
            {
                "id": "asset:atlas-a",
                "kind": "daedalus.sprite_atlas",
                "uri": "sprites/atlas-a.json",
                "metadata": {"schema_version": 1}
            },
            {
                "id": "asset:atlas-b",
                "kind": "daedalus.sprite_atlas",
                "uri": "sprites/atlas-b.json",
                "metadata": {"schema_version": 1}
            }
        ]
    }))
    .expect("sprite scene should encode");
    let manifest = serde_json::to_vec(&json!({
        "schema_version": 1,
        "name": "sprite packet test",
        "scene": "scene.json",
        "presentation": {"clear": clear_color}
    }))
    .expect("sprite manifest should encode");
    let mut resolver = ByteMapSourceResolver::from_bytes(BTreeMap::from([
        ("sprites/sheet.png".to_owned(), encoded_atlas_png()),
        ("sprites/atlas-a.json".to_owned(), atlas_document()),
        ("sprites/atlas-b.json".to_owned(), atlas_document()),
    ]))
    .expect("fixture source IDs should be valid");
    let mut cache = MemoryCookCache::default();
    prepare_project_with_assets(
        &manifest,
        &scene,
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .expect("offline sprite fixture should prepare")
}

fn handles_for(project: &PreparedProject, session: SessionId) -> BTreeMap<StableId, EntityHandle> {
    project
        .entities()
        .iter()
        .enumerate()
        .map(|(index, entity)| {
            (
                entity.id.clone(),
                EntityHandle {
                    session,
                    index: index as u32,
                    generation: 4,
                },
            )
        })
        .collect()
}

fn transform(translation: [f32; 3], rotation: Quat, scale: [f32; 3]) -> Transform {
    Transform {
        translation,
        rotation: rotation.to_array(),
        scale,
    }
}

fn snapshot_for(
    project: &PreparedProject,
    handles: &BTreeMap<StableId, EntityHandle>,
    transforms: &BTreeMap<StableId, Transform>,
    sequence: u64,
) -> RenderSnapshot {
    RenderSnapshot {
        sequence,
        tick: Tick(sequence + 100),
        entities: project
            .entities()
            .iter()
            .map(|entity| RenderEntitySnapshot {
                entity: handles[&entity.id],
                transform: transforms
                    .get(&entity.id)
                    .copied()
                    .unwrap_or(Transform::IDENTITY),
            })
            .collect::<Vec<_>>()
            .into(),
    }
}

fn instance_for(packet: &super::RenderPacket, handle: EntityHandle) -> &RenderInstance {
    packet
        .instances()
        .iter()
        .find(|instance| instance.id.entity == handle)
        .expect("sprite entity should have a packet instance")
}

fn assert_matrix_near(actual: Mat4, expected: Mat4) {
    for (actual, expected) in actual
        .to_cols_array()
        .into_iter()
        .zip(expected.to_cols_array())
    {
        assert!(
            (actual - expected).abs() <= 1.0e-5,
            "{actual} != {expected}"
        );
    }
}

fn assert_vec3_near(actual: Vec3, expected: Vec3) {
    assert!(
        (actual - expected).length() <= 1.0e-5,
        "{actual:?} != {expected:?}"
    );
}

fn sample_specs() -> Vec<SpriteSpec> {
    vec![
        SpriteSpec {
            entity: "entity:asym-a",
            atlas: "asset:atlas-a",
            region: "region:asymmetric",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 2,
        },
        SpriteSpec {
            entity: "entity:asym-b",
            atlas: "asset:atlas-b",
            region: "region:asymmetric",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 2,
        },
        SpriteSpec {
            entity: "entity:order-1",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 1,
        },
        SpriteSpec {
            entity: "entity:z-order-2",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 2,
        },
        SpriteSpec {
            entity: "entity:layer-negative",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [1.0; 4],
            sorting_layer: -1,
            order_in_layer: 50,
        },
        SpriteSpec {
            entity: "entity:layer-positive",
            atlas: "asset:atlas-b",
            region: "region:red",
            tint: [1.0; 4],
            sorting_layer: 1,
            order_in_layer: -50,
        },
    ]
}

#[test]
fn sprite_packets_share_prepared_quad_and_sidecars_and_use_authority_pose() {
    let project = prepare_sprite_project(&sample_specs());
    let session = SessionId::from_raw(9_301);
    let handles = handles_for(&project, session);
    let resources = ProjectRenderResources::prepare(
        &project,
        7,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .expect("valid sprite project should prepare once");
    let mut transforms = BTreeMap::new();
    let camera_rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    transforms.insert(
        StableId::new(CAMERA_ID).unwrap(),
        transform([4.0, 5.0, 10.0], camera_rotation, [1.0, 1.0, 1.0]),
    );
    let entity_rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    transforms.insert(
        StableId::new("entity:asym-a").unwrap(),
        transform([3.0, 2.0, 0.0], entity_rotation, [-2.0, 3.0, 1.0]),
    );
    let first_snapshot = snapshot_for(&project, &handles, &transforms, 1);
    let first = resources
        .extract(&first_snapshot, None)
        .expect("authority snapshot should extract");
    let second = resources
        .extract(&snapshot_for(&project, &handles, &transforms, 2), None)
        .expect("next authority snapshot should extract");

    let asym_a = handles[&StableId::new("entity:asym-a").unwrap()];
    let asym_b = handles[&StableId::new("entity:asym-b").unwrap()];
    let first_a = instance_for(&first, asym_a);
    let first_b = instance_for(&first, asym_b);
    assert!(Arc::ptr_eq(&first_a.mesh.0, &first_b.mesh.0));
    assert!(
        first
            .instances()
            .iter()
            .all(|instance| Arc::ptr_eq(&first_a.mesh.0, &instance.mesh.0))
    );
    assert_eq!(first_a.mesh.vertex_count(), 4);
    assert_eq!(first_a.mesh.index_count(), 6);
    assert_eq!(first_a.mesh.gpu_bytes(), 88);
    let quad = first_a.mesh.sprite_quad_geometry().unwrap();
    assert_eq!(quad.vertices[0].position, [0.0, 0.0]);
    assert_eq!(quad.vertices[0].uv, [0.0, 1.0]);
    assert_eq!(quad.vertices[2].position, [1.0, 1.0]);
    assert_eq!(quad.vertices[2].uv, [1.0, 0.0]);
    assert_eq!(quad.indices, [0, 1, 2, 0, 2, 3]);

    let asym_binding = first
        .sprite_binding(&first_a.id)
        .expect("sprite draw binding should be retained");
    assert_eq!(asym_binding.uv_rect, [0.125, 0.125, 0.625, 0.625]);
    assert!(Arc::ptr_eq(
        &asym_binding.texture,
        &first
            .sprite_binding(&first_b.id)
            .expect("alias binding should exist")
            .texture
    ));
    let authority = Mat4::from_scale_rotation_translation(
        Vec3::new(-2.0, 3.0, 1.0),
        entity_rotation,
        Vec3::new(3.0, 2.0, 0.0),
    );
    let local_quad = Mat4::from_translation(Vec3::new(-0.625, -1.875, 0.0))
        * Mat4::from_scale(Vec3::new(2.5, 2.5, 1.0));
    assert_matrix_near(first_a.transform, authority * local_quad);

    assert!(Arc::ptr_eq(&first.sprite_bindings, &second.sprite_bindings));
    assert!(Arc::ptr_eq(&first.sprite_order, &second.sprite_order));
    let expected_order = [
        "entity:layer-negative",
        "entity:order-1",
        "entity:asym-a",
        "entity:asym-b",
        "entity:z-order-2",
        "entity:layer-positive",
    ];
    let expected_handles = expected_order
        .iter()
        .map(|id| handles[&StableId::new(*id).unwrap()])
        .collect::<Vec<_>>();
    assert_eq!(
        first
            .sprite_order
            .iter()
            .map(|id| id.entity)
            .collect::<Vec<_>>(),
        expected_handles
    );
    assert!(first.sprite_order.iter().all(|id| id.primitive == 0));

    assert_eq!(
        first.projection(),
        RenderProjection::Orthographic { vertical_size: 8.0 }
    );
    let expected_forward = camera_rotation * Vec3::NEG_Z;
    let expected_up = camera_rotation * Vec3::Y;
    assert_vec3_near(
        Vec3::from_array(first.camera().position),
        Vec3::new(4.0, 5.0, 10.0),
    );
    assert_vec3_near(
        Vec3::from_array(first.camera().target),
        Vec3::new(4.0, 5.0, 10.0) + expected_forward,
    );
    assert_vec3_near(Vec3::from_array(first.camera().up), expected_up);
    assert_eq!(first.camera().near, 0.1);
    assert_eq!(first.camera().far, 100.0);
    assert_eq!(
        first.with_camera(Camera::default()).projection(),
        first.projection(),
        "camera-only replacement must retain orthographic projection"
    );

    let view = RenderView::new(
        Camera {
            position: [0.0, 0.0, 10.0],
            target: [0.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
            vertical_fov_radians: 1.0,
            near: 0.1,
            far: 100.0,
        },
        RenderProjection::Orthographic { vertical_size: 8.0 },
    );
    let square = view.view_projection(1.0).unwrap();
    let wide = view.view_projection(4.0 / 3.0).unwrap();
    assert!((square.x_axis.x - 0.25).abs() < 1.0e-6);
    assert!((wide.x_axis.x - 0.1875).abs() < 1.0e-6);
    assert!((square.y_axis.y - wide.y_axis.y).abs() < 1.0e-6);

    let mut wrong_session_entries = first_snapshot.entities.to_vec();
    wrong_session_entries
        .iter_mut()
        .find(|entry| entry.entity == asym_a)
        .unwrap()
        .entity
        .session = SessionId::from_raw(9_302);
    let wrong_session = RenderSnapshot {
        entities: wrong_session_entries.into(),
        ..first_snapshot.clone()
    };
    assert!(resources.extract(&wrong_session, None).is_err());

    let mut stale_entries = first_snapshot.entities.to_vec();
    stale_entries
        .iter_mut()
        .find(|entry| entry.entity == asym_a)
        .unwrap()
        .entity
        .generation += 1;
    let stale = RenderSnapshot {
        entities: stale_entries.into(),
        ..first_snapshot.clone()
    };
    assert!(resources.extract(&stale, None).is_err());

    let camera_handle = handles[&StableId::new(CAMERA_ID).unwrap()];
    let missing_camera = RenderSnapshot {
        entities: first_snapshot
            .entities
            .iter()
            .copied()
            .filter(|entry| entry.entity != camera_handle)
            .collect::<Vec<_>>()
            .into(),
        ..first_snapshot
    };
    assert!(resources.extract(&missing_camera, None).is_err());
}

#[derive(Debug)]
struct TemporaryCaptureDirectory(PathBuf);

impl TemporaryCaptureDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        for attempt in 0..32_u32 {
            let path = std::env::temp_dir().join(format!(
                "daedalus-sprite-packet-pixels-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create temporary capture directory: {error}"),
            }
        }
        panic!("could not allocate a unique sprite capture directory")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TemporaryCaptureDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn decode_capture(path: &Path) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(BufReader::new(
        fs::File::open(path).expect("captured PNG should exist"),
    ));
    let mut reader = decoder.read_info().expect("capture should be a PNG");
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .expect("capture output length should fit")
    ];
    let frame = reader
        .next_frame(&mut pixels)
        .expect("capture frame should decode");
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    pixels.truncate(frame.buffer_size());
    (frame.width, frame.height, pixels)
}

fn pixel_at_world(
    image: &(u32, u32, Vec<u8>),
    world_x: f64,
    world_y: f64,
    vertical_size: f64,
) -> [u8; 4] {
    let (width, height, pixels) = image;
    let aspect = f64::from(*width) / f64::from(*height);
    let half_width = vertical_size * aspect * 0.5;
    let half_height = vertical_size * 0.5;
    let x = (((world_x / half_width + 1.0) * 0.5) * f64::from(*width)).floor() as usize;
    let y = (((1.0 - world_y / half_height) * 0.5) * f64::from(*height)).floor() as usize;
    let offset = (y * *width as usize + x) * 4;
    pixels[offset..offset + 4]
        .try_into()
        .expect("sampled pixel should contain four channels")
}

fn srgb_to_linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb_byte(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded = if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round() as u8
}

fn expected_sprite_pixel(source: [u8; 4], tint: [f32; 4], clear: [f32; 4]) -> [u8; 4] {
    let alpha = f64::from(source[3]) / 255.0 * f64::from(tint[3]);
    let mut output = [0; 4];
    for channel in 0..3 {
        let source_linear = srgb_to_linear(source[channel]) * f64::from(tint[channel]);
        let clear_linear = f64::from(clear[channel]);
        output[channel] = linear_to_srgb_byte(source_linear * alpha + clear_linear * (1.0 - alpha));
    }
    output[3] = 255;
    output
}

fn expected_straight_alpha_pixel(clear: [f32; 4], layers: &[([u8; 4], [f32; 4])]) -> [u8; 4] {
    let clear_alpha = f64::from(clear[3]);
    let mut premultiplied = [
        f64::from(clear[0]) * clear_alpha,
        f64::from(clear[1]) * clear_alpha,
        f64::from(clear[2]) * clear_alpha,
    ];
    let mut output_alpha = clear_alpha;

    for (source, tint) in layers {
        let source_alpha = f64::from(source[3]) / 255.0 * f64::from(tint[3]);
        for channel in 0..3 {
            let source_linear = srgb_to_linear(source[channel]) * f64::from(tint[channel]);
            premultiplied[channel] =
                source_linear * source_alpha + premultiplied[channel] * (1.0 - source_alpha);
        }
        output_alpha = source_alpha + output_alpha * (1.0 - source_alpha);
    }

    let mut output = [0; 4];
    if output_alpha > 0.0 {
        for channel in 0..3 {
            output[channel] = linear_to_srgb_byte(premultiplied[channel] / output_alpha);
        }
    }
    output[3] = (output_alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    output
}

fn assert_straight_alpha_pixel_near(
    actual: [u8; 4],
    expected: [u8; 4],
    rgb_tolerance: u8,
    alpha_tolerance: u8,
    label: &str,
) {
    for channel in 0..3 {
        let difference = actual[channel].abs_diff(expected[channel]);
        assert!(
            difference <= rgb_tolerance,
            "{label} RGB pixel {actual:?} differs from independent straight-alpha reference {expected:?} at channel {channel} by {difference} (tolerance {rgb_tolerance})"
        );
    }
    let alpha_difference = actual[3].abs_diff(expected[3]);
    assert!(
        alpha_difference <= alpha_tolerance,
        "{label} alpha pixel {actual:?} differs from independent straight-alpha reference {expected:?} by {alpha_difference} (tolerance {alpha_tolerance})"
    );
}

fn composite_source_over(
    destination: [f64; 3],
    source_srgb: [u8; 3],
    tint: [f32; 4],
    source_alpha: u8,
) -> [f64; 3] {
    let alpha = f64::from(source_alpha) / 255.0 * f64::from(tint[3]);
    std::array::from_fn(|channel| {
        let source = srgb_to_linear(source_srgb[channel]) * f64::from(tint[channel]);
        source * alpha + destination[channel] * (1.0 - alpha)
    })
}

fn assert_pixel_near(actual: [u8; 4], expected: [u8; 4], tolerance: u8, label: &str) {
    for channel in 0..4 {
        let difference = actual[channel].abs_diff(expected[channel]);
        assert!(
            difference <= tolerance,
            "{label} pixel {actual:?} differs from independent scalar reference {expected:?} at channel {channel} by {difference} (tolerance {tolerance})"
        );
    }
}

fn gpu_specs() -> Vec<SpriteSpec> {
    vec![
        SpriteSpec {
            entity: "entity:red-probe",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [0.75, 0.8, 0.9, 1.0],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:transparent-probe",
            atlas: "asset:atlas-b",
            region: "region:transparent",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:blue-probe",
            atlas: "asset:atlas-a",
            region: "region:blue",
            tint: [0.6, 0.9, 1.0, 0.5],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:tie-a",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [0.8, 1.0, 0.5, 0.5],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:tie-b",
            atlas: "asset:atlas-b",
            region: "region:blue",
            tint: [1.0, 0.75, 0.8, 0.75],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:order-after-tie",
            atlas: "asset:atlas-a",
            region: "region:gold",
            tint: [0.9, 1.0, 0.7, 0.8],
            sorting_layer: 0,
            order_in_layer: 1,
        },
        SpriteSpec {
            entity: "entity:mirror-probe",
            atlas: "asset:atlas-b",
            region: "region:full",
            tint: [1.0; 4],
            sorting_layer: 0,
            order_in_layer: 0,
        },
    ]
}

#[test]
#[ignore = "requires a native wgpu adapter; verifies real sprite pixels, alpha, ordering, and cache reuse"]
fn sprite_pixels_match_srgb_alpha_sort_mirror_and_residency_reference() {
    let specs = gpu_specs();
    let project = prepare_sprite_project(&specs);
    let session = SessionId::from_raw(9_401);
    let handles = handles_for(&project, session);
    let resources = ProjectRenderResources::prepare(
        &project,
        11,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .expect("sprite GPU project should prepare");
    let mut transforms = BTreeMap::new();
    transforms.insert(
        StableId::new(CAMERA_ID).unwrap(),
        Transform {
            translation: [0.0, 0.0, 10.0],
            ..Transform::IDENTITY
        },
    );
    let positions = [
        ("entity:red-probe", [-3.0, 1.0, 0.0], [1.0, 1.0, 1.0]),
        (
            "entity:transparent-probe",
            [-1.0, 1.0, 0.0],
            [1.0, 1.0, 1.0],
        ),
        ("entity:blue-probe", [1.0, 1.0, 0.0], [1.0, 1.0, 1.0]),
        ("entity:tie-a", [-1.0, -1.3, 0.0], [1.0, 1.0, 1.0]),
        ("entity:tie-b", [-1.0, -1.3, 0.0], [1.0, 1.0, 1.0]),
        ("entity:order-after-tie", [-1.0, -1.3, 0.0], [1.0, 1.0, 1.0]),
        ("entity:mirror-probe", [3.0, -1.3, 0.0], [-1.0, 1.0, 1.0]),
    ];
    for (entity, translation, scale) in positions {
        transforms.insert(
            StableId::new(entity).unwrap(),
            transform(translation, Quat::IDENTITY, scale),
        );
    }
    let initial_snapshot = snapshot_for(&project, &handles, &transforms, 1);
    let initial_packet = resources
        .extract(&initial_snapshot, None)
        .expect("initial authority sprite packet should extract");
    assert_eq!(initial_packet.instances().len(), specs.len());
    assert_eq!(initial_packet.sprite_order.len(), specs.len());

    let captures = TemporaryCaptureDirectory::new();
    let mut packet_renderer = Renderer::new_offscreen_blocking()
        .expect("explicit native GPU test requires an available adapter")
        .into_packet_renderer(PacketRendererLimits::default())
        .expect("packet renderer should initialize");
    let first_path = captures.file("sprite-first.png");
    packet_renderer
        .render_packet_png(&initial_packet, &CaptureOptions::new(384, 288, &first_path))
        .expect("initial sprite packet should capture");
    let first_image = decode_capture(&first_path);
    let first_geometry = packet_renderer.resource_stats();
    let first_assets = packet_renderer.asset_resource_stats();
    assert_eq!(first_geometry.geometry_upload_count, 1);
    assert_eq!(first_assets.texture_upload_count, 1);
    assert_eq!(first_assets.sampler_create_count, 1);
    assert_eq!(first_assets.bind_group_create_count, 1);
    assert_eq!(first_geometry.instance_write_count, 2 * specs.len() as u64);
    assert_eq!(
        first_geometry.instance_write_bytes,
        144 * specs.len() as u64
    );

    let red = expected_sprite_pixel([240, 40, 30, 255], [0.75, 0.8, 0.9, 1.0], CLEAR_LINEAR);
    let transparent = expected_sprite_pixel([20, 250, 60, 0], [1.0; 4], CLEAR_LINEAR);
    let blue = expected_sprite_pixel([30, 80, 230, 128], [0.6, 0.9, 1.0, 0.5], CLEAR_LINEAR);
    assert_pixel_near(
        pixel_at_world(&first_image, -3.0, 1.0, 8.0),
        red,
        3,
        "opaque red",
    );
    assert_pixel_near(
        pixel_at_world(&first_image, -1.0, 1.0, 8.0),
        transparent,
        2,
        "zero-alpha hidden green",
    );
    assert_pixel_near(
        pixel_at_world(&first_image, 1.0, 1.0, 8.0),
        blue,
        3,
        "partial blue",
    );

    let mut overlap_linear = [
        f64::from(CLEAR_LINEAR[0]),
        f64::from(CLEAR_LINEAR[1]),
        f64::from(CLEAR_LINEAR[2]),
    ];
    // StableId tie ordering draws entity:tie-a first, then entity:tie-b; the larger order
    // is composited last regardless of the entity/source order in the project file.
    overlap_linear =
        composite_source_over(overlap_linear, [240, 40, 30], [0.8, 1.0, 0.5, 0.5], 255);
    overlap_linear =
        composite_source_over(overlap_linear, [30, 80, 230], [1.0, 0.75, 0.8, 0.75], 128);
    overlap_linear =
        composite_source_over(overlap_linear, [220, 190, 40], [0.9, 1.0, 0.7, 0.8], 64);
    let overlap_expected = [
        linear_to_srgb_byte(overlap_linear[0]),
        linear_to_srgb_byte(overlap_linear[1]),
        linear_to_srgb_byte(overlap_linear[2]),
        255,
    ];
    assert_pixel_near(
        pixel_at_world(&first_image, -1.0, -1.3, 8.0),
        overlap_expected,
        4,
        "stable tie and later order source-over",
    );
    let mirror_left = pixel_at_world(&first_image, 2.75, -1.05, 8.0);
    let mirror_right = pixel_at_world(&first_image, 3.25, -1.05, 8.0);
    assert_pixel_near(
        mirror_left,
        transparent,
        2,
        "mirrored transparent right atlas half",
    );
    assert_pixel_near(
        mirror_right,
        expected_sprite_pixel([240, 40, 30, 255], [1.0; 4], CLEAR_LINEAR),
        3,
        "mirrored opaque left atlas half",
    );
    assert_pixel_near(
        pixel_at_world(&first_image, 2.75, -1.55, 8.0),
        expected_sprite_pixel([220, 190, 40, 64], [1.0; 4], CLEAR_LINEAR),
        3,
        "mirrored bottom-right gold atlas half",
    );
    assert_pixel_near(
        pixel_at_world(&first_image, 3.25, -1.55, 8.0),
        expected_sprite_pixel([30, 80, 230, 128], [1.0; 4], CLEAR_LINEAR),
        3,
        "mirrored bottom-left blue atlas half",
    );

    let repeated_path = captures.file("sprite-repeat.png");
    packet_renderer
        .render_packet_png(
            &initial_packet,
            &CaptureOptions::new(384, 288, &repeated_path),
        )
        .expect("repeated immutable packet should capture");
    let repeated_geometry = packet_renderer.resource_stats();
    let repeated_assets = packet_renderer.asset_resource_stats();
    assert_eq!(
        repeated_geometry.geometry_upload_count,
        first_geometry.geometry_upload_count
    );
    assert_eq!(
        repeated_geometry.instance_write_count,
        first_geometry.instance_write_count
    );
    assert_eq!(
        repeated_geometry.instance_write_bytes,
        first_geometry.instance_write_bytes
    );
    assert_eq!(
        repeated_geometry.material_write_count,
        first_geometry.material_write_count
    );
    assert_eq!(
        repeated_geometry.material_write_bytes,
        first_geometry.material_write_bytes
    );
    assert_eq!(
        repeated_assets.texture_upload_count,
        first_assets.texture_upload_count
    );
    assert_eq!(
        repeated_assets.sampler_create_count,
        first_assets.sampler_create_count
    );
    assert_eq!(
        repeated_assets.bind_group_create_count,
        first_assets.bind_group_create_count
    );

    let moved_id = StableId::new("entity:red-probe").unwrap();
    transforms.insert(
        moved_id.clone(),
        transform([-2.8, 1.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
    );
    let moved_packet = resources
        .extract(&snapshot_for(&project, &handles, &transforms, 2), None)
        .expect("transform-only authority update should extract");
    assert_eq!(
        initial_packet
            .sprite_binding(&instance_for(&initial_packet, handles[&moved_id]).id)
            .unwrap()
            .uv_rect,
        moved_packet
            .sprite_binding(&instance_for(&moved_packet, handles[&moved_id]).id)
            .unwrap()
            .uv_rect,
        "moving a sprite must preserve its atlas UV binding"
    );
    let moved_path = captures.file("sprite-transform.png");
    packet_renderer
        .render_packet_png(&moved_packet, &CaptureOptions::new(384, 288, &moved_path))
        .expect("transform-only packet should capture");
    let moved_geometry = packet_renderer.resource_stats();
    let moved_assets = packet_renderer.asset_resource_stats();
    assert_eq!(
        moved_geometry.geometry_upload_count,
        repeated_geometry.geometry_upload_count
    );
    assert_eq!(
        moved_geometry.instance_write_count - repeated_geometry.instance_write_count,
        1
    );
    assert_eq!(
        moved_geometry.instance_write_bytes - repeated_geometry.instance_write_bytes,
        128
    );
    assert_eq!(
        moved_geometry.material_write_count,
        repeated_geometry.material_write_count
    );
    assert_eq!(
        moved_geometry.material_write_bytes,
        repeated_geometry.material_write_bytes
    );
    assert_eq!(
        moved_assets.texture_upload_count,
        repeated_assets.texture_upload_count
    );
    assert_eq!(
        moved_assets.sampler_create_count,
        repeated_assets.sampler_create_count
    );
    assert_eq!(
        moved_assets.bind_group_create_count,
        repeated_assets.bind_group_create_count
    );

    let mut camera_transforms = transforms.clone();
    camera_transforms.insert(
        StableId::new(CAMERA_ID).unwrap(),
        transform([0.1, 0.0, 10.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
    );
    let camera_packet = resources
        .extract(
            &snapshot_for(&project, &handles, &camera_transforms, 3),
            None,
        )
        .expect("camera-only authority update should extract");
    let camera_path = captures.file("sprite-camera.png");
    packet_renderer
        .render_packet_png(&camera_packet, &CaptureOptions::new(384, 288, &camera_path))
        .expect("camera-only packet should capture");
    let camera_geometry = packet_renderer.resource_stats();
    let camera_assets = packet_renderer.asset_resource_stats();
    assert_eq!(
        camera_geometry.geometry_upload_count,
        moved_geometry.geometry_upload_count
    );
    assert_eq!(
        camera_geometry.instance_write_count,
        moved_geometry.instance_write_count
    );
    assert_eq!(
        camera_geometry.material_write_count,
        moved_geometry.material_write_count
    );
    assert_eq!(
        camera_geometry.global_write_count - moved_geometry.global_write_count,
        1
    );
    assert_eq!(
        camera_assets.texture_upload_count,
        moved_assets.texture_upload_count
    );
    assert_eq!(
        camera_assets.sampler_create_count,
        moved_assets.sampler_create_count
    );
    assert_eq!(
        camera_assets.bind_group_create_count,
        moved_assets.bind_group_create_count
    );

    // Physical viewport changes alter horizontal extent; the same vertical world size,
    // shared unit quad, atlas and sampler remain resident.
    let square_path = captures.file("sprite-square-resize.png");
    packet_renderer
        .render_packet_png(&camera_packet, &CaptureOptions::new(288, 288, &square_path))
        .expect("square aspect capture should succeed");
    let square_image = decode_capture(&square_path);
    let square_assets = packet_renderer.asset_resource_stats();
    let square_stats = packet_renderer.resource_stats();
    assert_eq!(
        square_assets.texture_upload_count,
        camera_assets.texture_upload_count
    );
    assert_eq!(
        square_assets.sampler_create_count,
        camera_assets.sampler_create_count
    );
    assert_eq!(
        square_assets.bind_group_create_count,
        camera_assets.bind_group_create_count
    );
    assert_eq!(
        square_stats.geometry_upload_count,
        camera_geometry.geometry_upload_count
    );
    assert_pixel_near(
        pixel_at_world(&square_image, -2.9, 1.0, 8.0),
        red,
        3,
        "orthographic resize keeps vertical scale and sprite color",
    );

    // A tint-alpha-only packet candidate exercises the shared material dirty path. This
    // mutates only a private test packet binding; it does not imply runtime hot-edit support.
    let mut alpha_packet = camera_packet.clone();
    alpha_packet.sequence = 4;
    alpha_packet.tick = Tick(104);
    let red_instance = instance_for(&alpha_packet, handles[&moved_id]);
    let red_draw_id = red_instance.id;
    let mut alpha_bindings = (*alpha_packet.sprite_bindings).clone();
    let red_binding = alpha_bindings
        .get_mut(&red_draw_id)
        .expect("red sprite binding should exist");
    red_binding.tint[3] = 0.25;
    alpha_packet.sprite_bindings = Arc::new(alpha_bindings);
    let before_alpha = packet_renderer.resource_stats();
    let before_alpha_assets = packet_renderer.asset_resource_stats();
    let alpha_path = captures.file("sprite-alpha-tint.png");
    packet_renderer
        .render_packet_png(&alpha_packet, &CaptureOptions::new(288, 288, &alpha_path))
        .expect("alpha-only tint packet should capture");
    let alpha_image = decode_capture(&alpha_path);
    let after_alpha = packet_renderer.resource_stats();
    let after_alpha_assets = packet_renderer.asset_resource_stats();
    assert_eq!(
        after_alpha.geometry_upload_count,
        before_alpha.geometry_upload_count
    );
    assert_eq!(
        after_alpha.instance_write_count,
        before_alpha.instance_write_count
    );
    assert_eq!(
        after_alpha.instance_write_bytes,
        before_alpha.instance_write_bytes
    );
    assert_eq!(
        after_alpha.material_write_count - before_alpha.material_write_count,
        1
    );
    assert_eq!(
        after_alpha.material_write_bytes - before_alpha.material_write_bytes,
        48
    );
    assert_eq!(
        after_alpha_assets.texture_upload_count,
        before_alpha_assets.texture_upload_count
    );
    assert_eq!(
        after_alpha_assets.sampler_create_count,
        before_alpha_assets.sampler_create_count
    );
    assert_eq!(
        after_alpha_assets.bind_group_create_count,
        before_alpha_assets.bind_group_create_count
    );
    assert_pixel_near(
        pixel_at_world(&alpha_image, -2.9, 1.0, 8.0),
        expected_sprite_pixel([240, 40, 30, 255], [0.75, 0.8, 0.9, 0.25], CLEAR_LINEAR),
        3,
        "tint-alpha-only material update",
    );
}

#[test]
#[ignore = "requires a native wgpu adapter; verifies straight-alpha PNG capture over translucent clears"]
fn transparent_packet_png_capture_matches_independent_source_over_reference() {
    let specs = [
        SpriteSpec {
            entity: "entity:partial-blue",
            atlas: "asset:atlas-a",
            region: "region:blue",
            tint: [0.6, 0.9, 1.0, 1.0],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:overlap-red",
            atlas: "asset:atlas-a",
            region: "region:red",
            tint: [0.8, 1.0, 0.5, 0.5],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:overlap-blue",
            atlas: "asset:atlas-b",
            region: "region:blue",
            tint: [1.0, 0.75, 0.8, 0.75],
            sorting_layer: 0,
            order_in_layer: 0,
        },
        SpriteSpec {
            entity: "entity:overlap-gold",
            atlas: "asset:atlas-a",
            region: "region:gold",
            tint: [0.9, 1.0, 0.7, 0.8],
            sorting_layer: 0,
            order_in_layer: 1,
        },
    ];
    let captures = TemporaryCaptureDirectory::new();
    let mut packet_renderer = Renderer::new_offscreen_blocking()
        .expect("explicit native GPU test requires an available adapter")
        .into_packet_renderer(PacketRendererLimits::default())
        .expect("packet renderer should initialize");

    for (case_index, clear_alpha) in [0.0_f32, 0.25, 1.0].into_iter().enumerate() {
        let clear = [
            CLEAR_LINEAR[0],
            CLEAR_LINEAR[1],
            CLEAR_LINEAR[2],
            clear_alpha,
        ];
        let project = prepare_sprite_project_with_clear(&specs, clear);
        let session = SessionId::from_raw(9_420 + case_index as u64);
        let handles = handles_for(&project, session);
        let resources = ProjectRenderResources::prepare(
            &project,
            11,
            session,
            &handles,
            PacketRendererLimits::default(),
        )
        .expect("sprite GPU project should prepare");
        let mut transforms = BTreeMap::new();
        transforms.insert(
            StableId::new(CAMERA_ID).unwrap(),
            Transform {
                translation: [0.0, 0.0, 10.0],
                ..Transform::IDENTITY
            },
        );
        transforms.insert(
            StableId::new("entity:partial-blue").unwrap(),
            transform([-2.0, 1.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
        );
        for entity in [
            "entity:overlap-red",
            "entity:overlap-blue",
            "entity:overlap-gold",
        ] {
            transforms.insert(
                StableId::new(entity).unwrap(),
                transform([1.0, -1.0, 0.0], Quat::IDENTITY, [1.0, 1.0, 1.0]),
            );
        }
        let packet = resources
            .extract(&snapshot_for(&project, &handles, &transforms, 1), None)
            .expect("authority sprite packet should extract");
        let path = captures.file(&format!("translucent-clear-{case_index}.png"));
        packet_renderer
            .render_packet_png(&packet, &CaptureOptions::new(384, 288, &path))
            .expect("translucent sprite packet should capture");
        let image = decode_capture(&path);

        // Readback quantizes premultiplied color and alpha to UNORM8 before restoring
        // straight alpha. Four RGB bytes cover those two quantization steps; alpha is
        // checked independently to one byte because it is stored only once.
        assert_straight_alpha_pixel_near(
            pixel_at_world(&image, 4.5, 3.0, 8.0),
            expected_straight_alpha_pixel(clear, &[]),
            4,
            1,
            "untouched nonblack clear",
        );
        assert_straight_alpha_pixel_near(
            pixel_at_world(&image, -2.0, 1.0, 8.0),
            expected_straight_alpha_pixel(clear, &[([30, 80, 230, 128], [0.6, 0.9, 1.0, 1.0])]),
            4,
            1,
            "single partial-alpha blue sprite",
        );
        assert_straight_alpha_pixel_near(
            pixel_at_world(&image, 1.0, -1.0, 8.0),
            expected_straight_alpha_pixel(
                clear,
                &[
                    // Equal layer/order draws sort by StableId: overlap-blue precedes
                    // overlap-red; the higher-order gold sprite is composited last.
                    ([30, 80, 230, 128], [1.0, 0.75, 0.8, 0.75]),
                    ([240, 40, 30, 255], [0.8, 1.0, 0.5, 0.5]),
                    ([220, 190, 40, 64], [0.9, 1.0, 0.7, 0.8]),
                ],
            ),
            4,
            1,
            "overlapping partial-alpha sprites in painter order",
        );
    }
}
