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
    prepare_project_with_assets, scene::StableId,
};
use daedalus_core::{
    EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
};
use glam::{Mat4, Quat, Vec3};
use serde_json::{Value, json};

use super::{PacketRendererLimits, ProjectRenderResources, RenderInstance, RenderInstanceId};
use crate::{CaptureOptions, Material, Mesh, RenderObject, RenderScene, Renderer, Vertex};

fn triangle_buffer() -> Vec<u8> {
    let values = [
        0.0_f32, 0.0, 0.0, // positions
        1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // positions
        0.0, 0.0, 1.0, // normals
        0.0, 0.0, 1.0, // normals
        0.0, 0.0, 1.0, // normals
    ];
    values.into_iter().flat_map(f32::to_le_bytes).collect()
}

fn hierarchy_gltf() -> Value {
    let half_sqrt = std::f32::consts::FRAC_1_SQRT_2;
    json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [
            {"nodes": [0]},
            {"nodes": [3]}
        ],
        "nodes": [
            {
                "translation": [1.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, half_sqrt, half_sqrt],
                "children": [2, 1]
            },
            {"translation": [1.0, 0.0, 0.0], "mesh": 0},
            {"translation": [0.0, 2.0, 0.0], "mesh": 0},
            {"translation": [0.0, 0.0, 5.0], "mesh": 0}
        ],
        "meshes": [{"primitives": [
            {"attributes": {"POSITION": 0, "NORMAL": 1}},
            {"attributes": {"POSITION": 0, "NORMAL": 1}}
        ]}],
        "buffers": [{"uri": "shared.bin", "byteLength": 72}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": 0, "byteLength": 36},
            {"buffer": 0, "byteOffset": 36, "byteLength": 36}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
            {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"}
        ]
    })
}

fn nonaffine_hierarchy_gltf() -> Value {
    let mut model = hierarchy_gltf();
    model["nodes"][0] = json!({
        "matrix": [
            1.0, 0.0, 0.0, 0.25,
            0.0, 1.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
            0.0, 0.0, 0.0, 1.0
        ],
        "children": [2, 1]
    });
    model
}

fn model_entity(entity_id: &str, model_id: &str) -> Value {
    json!({
        "id": entity_id,
        "components": [
            {
                "id": format!("component:transform:{entity_id}"),
                "type_id": "daedalus.transform",
                "asset_refs": [],
                "data": {
                    "translation": [100.0, 200.0, 300.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                }
            },
            {
                "id": format!("component:model:{entity_id}"),
                "type_id": "daedalus.model_renderer",
                "asset_refs": [model_id],
                "data": {"model": model_id}
            }
        ]
    })
}

fn model_project_scene() -> Value {
    json!({
        "schema_version": 1,
        "revision": 1,
        "entities": [
            model_entity("entity:a", "asset:model-a"),
            model_entity("entity:b", "asset:model-b")
        ],
        "assets": [
            {
                "id": "asset:model-a",
                "kind": "daedalus.model",
                "uri": "models/shared.gltf",
                "metadata": {}
            },
            {
                "id": "asset:model-b",
                "kind": "daedalus.model",
                "uri": "models/shared.gltf",
                "metadata": {"schema_version": 1, "scene_index": 1}
            }
        ]
    })
}

fn prepare_model_project(scene: Value, mut model: Value) -> daedalus_content::PreparedProject {
    let bin = triangle_buffer();
    model["buffers"][0]["byteLength"] = json!(bin.len());
    let mut resolver = ByteMapSourceResolver::from_bytes(BTreeMap::from([
        (
            "models/shared.gltf".to_owned(),
            serde_json::to_vec(&model).expect("glTF fixture should encode"),
        ),
        ("models/shared.bin".to_owned(), bin),
    ]))
    .expect("fixture source IDs should be valid");
    let mut cache = MemoryCookCache::default();
    prepare_project_with_assets(
        br#"{"schema_version":1,"name":"packet model tests","scene":"scene.json"}"#,
        &serde_json::to_vec(&scene).expect("scene fixture should encode"),
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .expect("offline model project should import")
}

fn authored_handles(
    project: &daedalus_content::PreparedProject,
    session: SessionId,
) -> BTreeMap<StableId, EntityHandle> {
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

fn snapshot(
    project: &daedalus_content::PreparedProject,
    handles: &BTreeMap<StableId, EntityHandle>,
    transforms: &BTreeMap<StableId, Transform>,
    sequence: u64,
) -> RenderSnapshot {
    RenderSnapshot {
        sequence,
        tick: Tick(sequence + 20),
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

fn instance_for(
    packet: &super::RenderPacket,
    entity: EntityHandle,
    primitive: u32,
) -> &RenderInstance {
    packet
        .instances()
        .iter()
        .find(|instance| instance.id == (RenderInstanceId { entity, primitive }))
        .expect("model occurrence should be present")
}

#[test]
fn model_expansion_excludes_unselected_scene_assigns_occurrence_ids_and_shares_geometry() {
    let project = prepare_model_project(model_project_scene(), hierarchy_gltf());
    let session = SessionId::from_raw(701);
    let handles = authored_handles(&project, session);
    let resources = ProjectRenderResources::prepare(
        &project,
        9,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .unwrap();
    let packet = resources
        .extract(&snapshot(&project, &handles, &BTreeMap::new(), 1), None)
        .unwrap();

    assert_eq!(packet.instances().len(), 6);
    let entity_a = handles[&StableId::new("entity:a").unwrap()];
    let entity_b = handles[&StableId::new("entity:b").unwrap()];
    let mut ids_a = packet
        .instances()
        .iter()
        .filter(|instance| instance.id.entity == entity_a)
        .map(|instance| instance.id.primitive)
        .collect::<Vec<_>>();
    let mut ids_b = packet
        .instances()
        .iter()
        .filter(|instance| instance.id.entity == entity_b)
        .map(|instance| instance.id.primitive)
        .collect::<Vec<_>>();
    ids_a.sort_unstable();
    ids_b.sort_unstable();
    assert_eq!(ids_a, [0, 1, 2, 3]);
    assert_eq!(ids_b, [0, 1]);

    // The source child list is [2, 1], but the stable expansion contract uses document node
    // order. The first node's rotated child translation therefore lands at (1, 1, 0).
    assert_vec3_near(
        instance_for(&packet, entity_a, 0)
            .transform
            .transform_point3(Vec3::ZERO),
        Vec3::new(1.0, 1.0, 0.0),
    );
    assert_vec3_near(
        instance_for(&packet, entity_a, 2)
            .transform
            .transform_point3(Vec3::ZERO),
        Vec3::new(-1.0, 0.0, 0.0),
    );
    assert_vec3_near(
        instance_for(&packet, entity_b, 0)
            .transform
            .transform_point3(Vec3::ZERO),
        Vec3::new(0.0, 0.0, 5.0),
    );
    assert!(Arc::ptr_eq(
        &instance_for(&packet, entity_a, 0).mesh.0,
        &instance_for(&packet, entity_b, 0).mesh.0
    ));
    assert_eq!(
        instance_for(&packet, entity_a, 0).mesh.layout_version(),
        instance_for(&packet, entity_b, 0).mesh.layout_version()
    );
}

#[test]
fn model_extraction_uses_authoritative_transform_and_rejects_stale_or_wrong_session_handles() {
    let project = prepare_model_project(model_project_scene(), hierarchy_gltf());
    let session = SessionId::from_raw(702);
    let handles = authored_handles(&project, session);
    let resources = ProjectRenderResources::prepare(
        &project,
        3,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .unwrap();
    let mut transforms = BTreeMap::new();
    transforms.insert(
        StableId::new("entity:a").unwrap(),
        Transform {
            translation: [10.0, 2.0, 0.0],
            rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2).to_array(),
            scale: [-2.0, 3.0, 4.0],
        },
    );
    let packet = resources
        .extract(&snapshot(&project, &handles, &transforms, 2), None)
        .unwrap();

    let entity = Mat4::from_scale_rotation_translation(
        Vec3::new(-2.0, 3.0, 4.0),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        Vec3::new(10.0, 2.0, 0.0),
    );
    let parent = Mat4::from_rotation_translation(
        Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
        Vec3::new(1.0, 0.0, 0.0),
    );
    let child = Mat4::from_translation(Vec3::X);
    let expected = entity * parent * child;
    assert_matrix_near(
        instance_for(&packet, handles[&StableId::new("entity:a").unwrap()], 0).transform,
        expected,
    );
    assert_vec3_near(
        instance_for(&packet, handles[&StableId::new("entity:a").unwrap()], 0)
            .transform
            .transform_point3(Vec3::ZERO),
        Vec3::new(10.0, 5.0, 2.0),
    );
    assert!(packet.validate(PacketRendererLimits::default()).is_ok());

    let entity_a = handles[&StableId::new("entity:a").unwrap()];
    let mut wrong_session_entities = snapshot(&project, &handles, &BTreeMap::new(), 3)
        .entities
        .to_vec();
    wrong_session_entities
        .iter_mut()
        .find(|entry| entry.entity == entity_a)
        .expect("entity a should be in the snapshot")
        .entity
        .session = SessionId::from_raw(999);
    let wrong_session = RenderSnapshot {
        sequence: 3,
        tick: Tick(23),
        entities: wrong_session_entities.into(),
    };
    assert!(resources.extract(&wrong_session, None).is_err());

    let mut stale_entities = snapshot(&project, &handles, &BTreeMap::new(), 4)
        .entities
        .to_vec();
    stale_entities
        .iter_mut()
        .find(|entry| entry.entity == entity_a)
        .expect("entity a should be in the snapshot")
        .entity
        .generation += 1;
    let stale_handle = RenderSnapshot {
        sequence: 4,
        tick: Tick(24),
        entities: stale_entities.into(),
    };
    assert!(resources.extract(&stale_handle, None).is_err());

    let missing = RenderSnapshot {
        sequence: 5,
        tick: Tick(25),
        entities: Vec::new().into(),
    };
    assert!(resources.extract(&missing, None).is_err());
}

#[test]
fn model_packets_reject_singular_nonaffine_and_expanded_limit_candidates() {
    let project = prepare_model_project(model_project_scene(), hierarchy_gltf());
    let session = SessionId::from_raw(703);
    let handles = authored_handles(&project, session);
    let too_few_instances = PacketRendererLimits {
        max_instances: 3,
        ..PacketRendererLimits::default()
    };
    assert!(
        ProjectRenderResources::prepare(&project, 1, session, &handles, too_few_instances,)
            .is_err()
    );
    let too_few_vertices = PacketRendererLimits {
        max_expanded_vertices: 11,
        ..PacketRendererLimits::default()
    };
    assert!(
        ProjectRenderResources::prepare(&project, 1, session, &handles, too_few_vertices,).is_err()
    );

    let resources = ProjectRenderResources::prepare(
        &project,
        1,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .unwrap();
    let mut transforms = BTreeMap::new();
    transforms.insert(
        StableId::new("entity:a").unwrap(),
        Transform {
            scale: [0.0, 1.0, 1.0],
            ..Transform::IDENTITY
        },
    );
    let singular = resources
        .extract(&snapshot(&project, &handles, &transforms, 6), None)
        .unwrap();
    assert!(singular.validate(PacketRendererLimits::default()).is_err());

    let valid_packet = resources
        .extract(&snapshot(&project, &handles, &BTreeMap::new(), 7), None)
        .unwrap();
    let mut nonaffine_instances = valid_packet.instances().to_vec();
    let mut columns = nonaffine_instances[0].transform.to_cols_array();
    columns[3] = 0.25;
    nonaffine_instances[0].transform = Mat4::from_cols_array(&columns);
    assert!(
        valid_packet
            .with_instances(nonaffine_instances)
            .validate(PacketRendererLimits::default())
            .is_err()
    );

    let nonaffine_project =
        prepare_model_project(model_project_scene(), nonaffine_hierarchy_gltf());
    assert!(
        ProjectRenderResources::prepare(
            &nonaffine_project,
            1,
            session,
            &authored_handles(&nonaffine_project, session),
            PacketRendererLimits::default(),
        )
        .is_err()
    );
}

#[derive(Clone, Copy)]
enum ExpectedTextureSample {
    Texel(usize),
    LinearBlend {
        left: usize,
        right: usize,
        weight: f64,
    },
    LinearAverage,
}

#[derive(Clone, Copy)]
struct TextureProbe {
    uv: [f32; 2],
    uv_span: Option<f32>,
    wrap_s: u32,
    mag_filter: u32,
    min_filter: u32,
    expected: ExpectedTextureSample,
}

const PROBE_TEXELS: [[u8; 4]; 4] = [
    [220, 30, 50, 255],
    [20, 190, 70, 255],
    [25, 50, 220, 255],
    [128, 128, 128, 255],
];
const PROBE_FACTOR: [f32; 3] = [0.67, 0.81, 0.56];
const PROBE_FOV: f32 = 0.9;
const PROBE_CAPTURE_SIZE: u32 = 384;

const TEXTURE_PROBES: [TextureProbe; 10] = [
    TextureProbe {
        uv: [0.25, 0.25],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(0),
    },
    TextureProbe {
        uv: [0.75, 0.25],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(1),
    },
    TextureProbe {
        uv: [0.25, 0.75],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(2),
    },
    TextureProbe {
        uv: [0.75, 0.75],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(3),
    },
    TextureProbe {
        uv: [-0.25, 0.25],
        uv_span: None,
        wrap_s: 10497,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(1),
    },
    TextureProbe {
        uv: [-0.25, 0.25],
        uv_span: None,
        wrap_s: 33648,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(0),
    },
    TextureProbe {
        uv: [-0.25, 0.25],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(0),
    },
    TextureProbe {
        uv: [0.45, 0.25],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9728,
        min_filter: 9728,
        expected: ExpectedTextureSample::Texel(0),
    },
    TextureProbe {
        uv: [0.45, 0.25],
        uv_span: None,
        wrap_s: 33071,
        mag_filter: 9729,
        min_filter: 9728,
        expected: ExpectedTextureSample::LinearBlend {
            left: 0,
            right: 1,
            weight: 0.4,
        },
    },
    TextureProbe {
        uv: [0.0, 0.0],
        // A 1024-texel span over a 2x2 texture and a ~42px panel forces the final 1x1 mip.
        uv_span: Some(1024.0),
        wrap_s: 33071,
        mag_filter: 9729,
        min_filter: 9987,
        expected: ExpectedTextureSample::LinearAverage,
    },
];

fn panel_positions(index: usize) -> [[f32; 3]; 6] {
    let (center_x, center_y) = if index == 9 {
        (0.0, -2.6)
    } else {
        let column = (index % 3) as f32;
        let row = (index / 3) as f32;
        ((column - 1.0) * 1.3, (1.0 - row) * 1.3)
    };
    let left = center_x - 0.42;
    let right = center_x + 0.42;
    let bottom = center_y - 0.42;
    let top = center_y + 0.42;
    [
        [left, bottom, 0.0],
        [right, bottom, 0.0],
        [right, top, 0.0],
        [left, bottom, 0.0],
        [right, top, 0.0],
        [left, top, 0.0],
    ]
}

fn append_f32_accessor_data<const N: usize>(
    bytes: &mut Vec<u8>,
    values: &[[f32; N]],
) -> (usize, usize) {
    let offset = bytes.len();
    for value in values.iter().flatten() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    (offset, bytes.len() - offset)
}

fn make_probe_gltf_sources() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut binary = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut primitives = Vec::new();
    let mut materials = Vec::new();
    let mut samplers = Vec::new();
    let mut textures = Vec::new();

    for (index, probe) in TEXTURE_PROBES.iter().enumerate() {
        let positions = panel_positions(index);
        let normals = [[0.0, 0.0, 1.0]; 6];
        let uvs = probe.uv_span.map_or([probe.uv; 6], |span| {
            [
                [0.0, 0.0],
                [span, 0.0],
                [span, span],
                [0.0, 0.0],
                [span, span],
                [0.0, span],
            ]
        });
        let (position_offset, position_bytes) = append_f32_accessor_data(&mut binary, &positions);
        let (normal_offset, normal_bytes) = append_f32_accessor_data(&mut binary, &normals);
        let (uv_offset, uv_bytes) = append_f32_accessor_data(&mut binary, &uvs);

        let mut position_min = [f32::INFINITY; 3];
        let mut position_max = [f32::NEG_INFINITY; 3];
        for position in &positions {
            for ((minimum, maximum), value) in position_min
                .iter_mut()
                .zip(position_max.iter_mut())
                .zip(position)
            {
                *minimum = minimum.min(*value);
                *maximum = maximum.max(*value);
            }
        }

        let position_view = views.len();
        views.push(json!({
            "buffer": 0,
            "byteOffset": position_offset,
            "byteLength": position_bytes
        }));
        let normal_view = views.len();
        views.push(json!({
            "buffer": 0,
            "byteOffset": normal_offset,
            "byteLength": normal_bytes
        }));
        let uv_view = views.len();
        views.push(json!({
            "buffer": 0,
            "byteOffset": uv_offset,
            "byteLength": uv_bytes
        }));

        let position_accessor = accessors.len();
        accessors.push(json!({
            "bufferView": position_view,
            "componentType": 5126,
            "count": 6,
            "type": "VEC3",
            "min": position_min,
            "max": position_max
        }));
        let normal_accessor = accessors.len();
        accessors.push(json!({
            "bufferView": normal_view,
            "componentType": 5126,
            "count": 6,
            "type": "VEC3"
        }));
        let uv_accessor = accessors.len();
        accessors.push(json!({
            "bufferView": uv_view,
            "componentType": 5126,
            "count": 6,
            "type": "VEC2"
        }));
        primitives.push(json!({
            "attributes": {
                "POSITION": position_accessor,
                "NORMAL": normal_accessor,
                "TEXCOORD_0": uv_accessor
            },
            "material": index
        }));

        samplers.push(json!({
            "magFilter": probe.mag_filter,
            "minFilter": probe.min_filter,
            "wrapS": probe.wrap_s,
            "wrapT": 33071
        }));
        textures.push(json!({"sampler": index, "source": 0}));
        materials.push(json!({
            "pbrMetallicRoughness": {
                "baseColorFactor": [PROBE_FACTOR[0], PROBE_FACTOR[1], PROBE_FACTOR[2], 1.0],
                "metallicFactor": 0.0,
                "roughnessFactor": 0.72,
                "baseColorTexture": {"index": index}
            }
        }));
    }

    let model = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": primitives}],
        "materials": materials,
        "images": [{"uri": "checker.png"}],
        "samplers": samplers,
        "textures": textures,
        "buffers": [{"uri": "panels.bin", "byteLength": binary.len()}],
        "bufferViews": views,
        "accessors": accessors
    });

    let texels = PROBE_TEXELS.iter().flatten().copied().collect::<Vec<_>>();
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .expect("PNG fixture header should encode");
        writer
            .write_image_data(&texels)
            .expect("PNG fixture pixels should encode");
    }

    (
        serde_json::to_vec(&model).expect("glTF fixture should encode"),
        binary,
        png_bytes,
    )
}

fn prepare_texture_probe_project() -> daedalus_content::PreparedProject {
    let (gltf, binary, png) = make_probe_gltf_sources();
    let scene = json!({
        "schema_version": 1,
        "revision": 1,
        "entities": [model_entity("entity:texture-probes", "asset:texture-probes")],
        "assets": [{
            "id": "asset:texture-probes",
            "kind": "daedalus.model",
            "uri": "models/probes.gltf",
            "metadata": {}
        }]
    });
    let manifest = json!({
        "schema_version": 1,
        "name": "textured packet pixel regression",
        "scene": "scene.json",
        "presentation": {
            "camera": {
                "position": [0.0, 0.0, 8.0],
                "target": [0.0, 0.0, 0.0],
                "up": [0.0, 1.0, 0.0],
                "vertical_fov_radians": PROBE_FOV,
                "near": 0.1,
                "far": 100.0
            },
            "light": {
                "direction": [0.0, 0.0, -1.0],
                "color": [1.0, 1.0, 1.0],
                "intensity": 0.8
            },
            "ambient": [0.12, 0.14, 0.16],
            "clear": [0.015, 0.02, 0.025, 1.0]
        }
    });
    let mut resolver = ByteMapSourceResolver::from_bytes(BTreeMap::from([
        ("models/probes.gltf".to_owned(), gltf),
        ("models/panels.bin".to_owned(), binary),
        ("models/checker.png".to_owned(), png),
    ]))
    .expect("probe source IDs should be valid");
    let mut cache = MemoryCookCache::default();
    prepare_project_with_assets(
        &serde_json::to_vec(&manifest).expect("manifest fixture should encode"),
        &serde_json::to_vec(&scene).expect("scene fixture should encode"),
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .expect("generated glTF and PNG should import")
}

fn srgb_decode_f64(encoded_byte: u8) -> f64 {
    let encoded = f64::from(encoded_byte) / 255.0;
    if encoded <= 0.04045 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn expected_probe_base_color(sample: ExpectedTextureSample) -> [f32; 3] {
    std::array::from_fn(|channel| {
        let channel_value = match sample {
            ExpectedTextureSample::Texel(index) => srgb_decode_f64(PROBE_TEXELS[index][channel]),
            ExpectedTextureSample::LinearBlend {
                left,
                right,
                weight,
            } => {
                let a = srgb_decode_f64(PROBE_TEXELS[left][channel]);
                let b = srgb_decode_f64(PROBE_TEXELS[right][channel]);
                a * (1.0 - weight) + b * weight
            }
            ExpectedTextureSample::LinearAverage => {
                PROBE_TEXELS
                    .iter()
                    .map(|texel| srgb_decode_f64(texel[channel]))
                    .sum::<f64>()
                    / PROBE_TEXELS.len() as f64
            }
        };
        (channel_value * f64::from(PROBE_FACTOR[channel])) as f32
    })
}

fn legacy_probe_scene(packet: &super::RenderPacket) -> RenderScene {
    let mut scene = RenderScene::new();
    scene.camera = packet.camera().clone();
    scene.directional_light = packet.directional_light().clone();
    scene.ambient_color = packet.ambient_color();
    scene.clear_color = packet.clear_color();
    for (index, probe) in TEXTURE_PROBES.iter().enumerate() {
        let vertices = panel_positions(index)
            .into_iter()
            .map(|position| Vertex::new(position, [0.0, 0.0, 1.0]))
            .collect();
        let mesh = Mesh::new(vertices, vec![0, 1, 2, 3, 4, 5]);
        let material = Material {
            base_color: expected_probe_base_color(probe.expected),
            metallic: 0.0,
            roughness: 0.72,
            emissive: [0.0; 3],
        };
        scene
            .objects
            .push(RenderObject::new(mesh, Mat4::IDENTITY, material));
    }
    scene
}

struct TemporaryCaptureDirectory(PathBuf);

impl TemporaryCaptureDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after the Unix epoch")
            .as_nanos();
        let parent = std::env::temp_dir();
        for attempt in 0..32_u32 {
            let path = parent.join(format!(
                "daedalus-model-packet-pixels-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create capture directory: {error}"),
            }
        }
        panic!("could not allocate a unique capture directory")
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

fn decode_capture_rgba(path: &Path) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(BufReader::new(
        fs::File::open(path).expect("capture should exist"),
    ));
    let mut reader = decoder.read_info().expect("capture should be a PNG");
    let mut bytes = vec![
        0;
        reader
            .output_buffer_size()
            .expect("capture dimensions should be bounded")
    ];
    let frame = reader
        .next_frame(&mut bytes)
        .expect("capture frame should decode");
    assert_eq!(frame.color_type, png::ColorType::Rgba);
    bytes.truncate(frame.buffer_size());
    (frame.width, frame.height, bytes)
}

fn probe_pixel(rgba: &[u8], width: u32, height: u32, index: usize) -> [u8; 4] {
    let (world_x, world_y) = if index == 9 {
        (0.0, -2.6)
    } else {
        let column = (index % 3) as f64;
        let row = (index / 3) as f64;
        ((column - 1.0) * 1.3, (1.0 - row) * 1.3)
    };
    let half_height = 8.0 * (f64::from(PROBE_FOV) * 0.5).tan();
    let pixel_x = (((world_x / half_height + 1.0) * 0.5) * f64::from(width)).floor() as usize;
    let pixel_y = (((1.0 - world_y / half_height) * 0.5) * f64::from(height)).floor() as usize;
    let offset = (pixel_y * width as usize + pixel_x) * 4;
    rgba[offset..offset + 4]
        .try_into()
        .expect("RGBA pixel should contain four channels")
}

#[test]
#[ignore = "requires a native wgpu adapter; checks real textured pixels against scalar PBR captures"]
fn model_texture_pixels_match_scalar_pbr_srgb_wrap_and_filter_reference() {
    let project = prepare_texture_probe_project();
    let session = SessionId::from_raw(7701);
    let handles = authored_handles(&project, session);
    let resources = ProjectRenderResources::prepare(
        &project,
        1,
        session,
        &handles,
        PacketRendererLimits::default(),
    )
    .expect("textured imported model should prepare");
    let packet = resources
        .extract(&snapshot(&project, &handles, &BTreeMap::new(), 1), None)
        .expect("textured imported model should extract");

    let gray_base = expected_probe_base_color(ExpectedTextureSample::Texel(3));
    assert!(
        (f64::from(gray_base[0]) - (128.0 / 255.0) * f64::from(PROBE_FACTOR[0])).abs() > 0.1,
        "sRGB mid-gray must be linearized before applying the material factor"
    );

    let captures = TemporaryCaptureDirectory::new();
    let packet_path = captures.file("textured-packet.png");
    let repeated_path = captures.file("textured-packet-repeat.png");
    let moved_path = captures.file("textured-packet-moved.png");
    let legacy_path = captures.file("scalar-reference.png");
    {
        let mut packet_renderer = Renderer::new_offscreen_blocking()
            .expect("explicit GPU run requires an available adapter")
            .into_packet_renderer(PacketRendererLimits::default())
            .expect("device should meet packet budgets");
        packet_renderer
            .render_packet_png(
                &packet,
                &CaptureOptions::new(PROBE_CAPTURE_SIZE, PROBE_CAPTURE_SIZE, &packet_path),
            )
            .expect("real textured packet capture should succeed");

        let after_first_geometry = packet_renderer.resource_stats();
        let after_first_assets = packet_renderer.asset_resource_stats();
        packet_renderer
            .render_packet_png(
                &packet,
                &CaptureOptions::new(PROBE_CAPTURE_SIZE, PROBE_CAPTURE_SIZE, &repeated_path),
            )
            .expect("repeated textured packet capture should succeed");
        let after_repeat_geometry = packet_renderer.resource_stats();
        let after_repeat_assets = packet_renderer.asset_resource_stats();
        assert_eq!(
            after_repeat_geometry.geometry_upload_count,
            after_first_geometry.geometry_upload_count
        );
        assert_eq!(
            after_repeat_geometry.instance_write_count,
            after_first_geometry.instance_write_count
        );
        assert_eq!(
            after_repeat_geometry.material_write_count,
            after_first_geometry.material_write_count
        );
        assert_eq!(
            after_repeat_assets.texture_upload_count,
            after_first_assets.texture_upload_count
        );
        assert_eq!(
            after_repeat_assets.sampler_create_count,
            after_first_assets.sampler_create_count
        );
        assert_eq!(
            after_repeat_assets.bind_group_create_count,
            after_first_assets.bind_group_create_count
        );

        let mut moved_transforms = BTreeMap::new();
        moved_transforms.insert(
            StableId::new("entity:texture-probes").expect("fixture entity id should be valid"),
            Transform {
                translation: [0.08, 0.12, 0.0],
                ..Transform::IDENTITY
            },
        );
        let moved_packet = resources
            .extract(&snapshot(&project, &handles, &moved_transforms, 2), None)
            .expect("new authority snapshot should extract with its updated transform");
        packet_renderer
            .render_packet_png(
                &moved_packet,
                &CaptureOptions::new(PROBE_CAPTURE_SIZE, PROBE_CAPTURE_SIZE, &moved_path),
            )
            .expect("transformed textured packet capture should succeed");
        let after_move_geometry = packet_renderer.resource_stats();
        let after_move_assets = packet_renderer.asset_resource_stats();
        assert_eq!(
            after_move_geometry.geometry_upload_count,
            after_repeat_geometry.geometry_upload_count
        );
        assert_eq!(
            after_move_geometry.material_write_count,
            after_repeat_geometry.material_write_count
        );
        assert_eq!(
            after_move_geometry.instance_write_count - after_repeat_geometry.instance_write_count,
            moved_packet.instances().len() as u64
        );
        assert_eq!(
            after_move_assets.texture_upload_count,
            after_repeat_assets.texture_upload_count
        );
        assert_eq!(
            after_move_assets.sampler_create_count,
            after_repeat_assets.sampler_create_count
        );
        assert_eq!(
            after_move_assets.bind_group_create_count,
            after_repeat_assets.bind_group_create_count
        );
    }
    let (packet_width, packet_height, packet_pixels) = decode_capture_rgba(&packet_path);
    assert_eq!(
        (packet_width, packet_height),
        (PROBE_CAPTURE_SIZE, PROBE_CAPTURE_SIZE)
    );

    let legacy_scene = legacy_probe_scene(&packet);
    Renderer::new_offscreen_blocking()
        .expect("explicit GPU run requires an available adapter")
        .render_png(
            &legacy_scene,
            &CaptureOptions::new(PROBE_CAPTURE_SIZE, PROBE_CAPTURE_SIZE, &legacy_path),
        )
        .expect("legacy scalar PBR reference capture should succeed");
    let (legacy_width, legacy_height, legacy_pixels) = decode_capture_rgba(&legacy_path);
    assert_eq!((legacy_width, legacy_height), (packet_width, packet_height));

    for (index, probe) in TEXTURE_PROBES.iter().enumerate() {
        let actual = probe_pixel(&packet_pixels, packet_width, packet_height, index);
        let expected = probe_pixel(&legacy_pixels, legacy_width, legacy_height, index);
        let tolerance = if matches!(probe.expected, ExpectedTextureSample::LinearAverage) {
            8
        } else {
            3
        };
        for (actual_channel, expected_channel) in actual.iter().zip(expected) {
            assert!(
                actual_channel.abs_diff(expected_channel) <= tolerance,
                "panel {index} (tolerance {tolerance}): packet {actual:?} != scalar PBR {expected:?}"
            );
        }
        assert_eq!(actual[3], 255, "panel {index} should remain opaque");
    }
}
