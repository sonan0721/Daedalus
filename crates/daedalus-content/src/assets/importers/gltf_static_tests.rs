use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::assets::{
    CpuCookCache, ImportCancellation, ImportLimits, ImportedAsset, MemoryCookCache, MipmapFilter,
    TextureFilter, TextureWrap,
};
use crate::project::{PreparedProject, prepare_project_with_assets};

const MODEL_URI: &str = "models/triangle.gltf";

fn png_rgba(pixel: [u8; 4]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header should encode");
        writer
            .write_image_data(&pixel)
            .expect("PNG pixel should encode");
    }
    bytes
}

fn png_rgba_with_source_gamma(pixel: [u8; 4], gamma: f32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_gamma(png::ScaledFloat::new(gamma));
        let mut writer = encoder.write_header().expect("PNG header should encode");
        writer
            .write_image_data(&pixel)
            .expect("PNG pixel should encode");
    }
    bytes
}

#[derive(Clone)]
struct Fixture {
    json: Value,
    buffer: Vec<u8>,
    png: Vec<u8>,
}

impl Fixture {
    fn new(indexed: bool) -> Self {
        let mut buffer = Vec::new();
        for vertex in [
            ([0.0_f32, 0.0, 0.0], [0.0_f32, 0.0, 1.0], [0_u8, 0]),
            ([1.0_f32, 0.0, 0.0], [0.0_f32, 0.0, 1.0], [255_u8, 0]),
            ([0.0_f32, 1.0, 0.0], [0.0_f32, 0.0, 1.0], [0_u8, 255]),
        ] {
            for value in vertex.0.into_iter().chain(vertex.1) {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
            buffer.extend_from_slice(&vertex.2);
            buffer.extend_from_slice(&[0, 0]); // Interleaved 28-byte vertex stride padding.
        }
        debug_assert_eq!(buffer.len(), 84);
        let index_offset = buffer.len();
        if indexed {
            for index in [0_u16, 1, 2] {
                buffer.extend_from_slice(&index.to_le_bytes());
            }
        }
        let png = png_rgba([220, 120, 30, 255]);
        let mut image_offset = None;
        if indexed {
            while buffer.len() % 4 != 0 {
                buffer.push(0);
            }
            image_offset = Some(buffer.len());
            buffer.extend_from_slice(&png);
        }
        let buffer_length = buffer.len();

        let mut buffer_views =
            vec![json!({ "buffer": 0, "byteOffset": 0, "byteLength": 84, "byteStride": 28 })];
        let mut accessors = vec![
            json!({ "bufferView": 0, "byteOffset": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] }),
            json!({ "bufferView": 0, "byteOffset": 12, "componentType": 5126, "count": 3, "type": "VEC3" }),
            json!({ "bufferView": 0, "byteOffset": 24, "componentType": 5121, "normalized": true, "count": 3, "type": "VEC2" }),
        ];
        if indexed {
            buffer_views.push(json!({ "buffer": 0, "byteOffset": index_offset, "byteLength": 6 }));
            accessors.push(
                json!({ "bufferView": 1, "componentType": 5123, "count": 3, "type": "SCALAR" }),
            );
        }
        let json = json!({
            "asset": { "version": "2.0" },
            "scene": 0,
            "scenes": [{ "nodes": [0] }, { "nodes": [2] }],
            "nodes": [
                { "translation": [1.0, 2.0, 3.0], "children": [1] },
                { "mesh": 0 },
                { "mesh": 0 }
            ],
            "meshes": [{ "primitives": [{
                "attributes": { "POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2 },
                "indices": if indexed { Some(3) } else { None },
                "material": 0
            }] }],
            "materials": [{ "pbrMetallicRoughness": {
                "baseColorFactor": [0.4, 0.5, 0.6, 1.0],
                "metallicFactor": 0.2,
                "roughnessFactor": 0.7,
                "baseColorTexture": { "index": 0 }
            } }],
            "samplers": [{ "minFilter": 9987, "magFilter": 9728, "wrapS": 33648, "wrapT": 33071 }],
            "textures": [{ "sampler": 0, "source": 0 }],
            "images": if indexed {
                json!([{ "bufferView": 2, "mimeType": "image/png" }])
            } else {
                json!([{ "uri": "albedo.png", "mimeType": "image/png" }])
            },
            "buffers": [{ "uri": "mesh.bin", "byteLength": buffer_length }],
            "bufferViews": if indexed {
                buffer_views.push(json!({ "buffer": 0, "byteOffset": image_offset.unwrap(), "byteLength": png.len() }));
                buffer_views
            } else {
                buffer_views
            },
            "accessors": accessors
        });
        Self { json, buffer, png }
    }

    fn external_json(&self) -> Value {
        let mut document = self.json.clone();
        document["images"] = json!([{ "uri": "albedo.png", "mimeType": "image/png" }]);
        document
    }

    fn external_sources(&self) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                MODEL_URI.to_owned(),
                serde_json::to_vec(&self.external_json()).unwrap(),
            ),
            ("models/mesh.bin".to_owned(), self.buffer.clone()),
            ("models/albedo.png".to_owned(), self.png.clone()),
        ])
    }

    fn glb(&self) -> Vec<u8> {
        let mut document = self.json.clone();
        document["buffers"][0]
            .as_object_mut()
            .unwrap()
            .remove("uri");
        document["buffers"][0]["byteLength"] = json!(self.buffer.len());
        let mut json_chunk = serde_json::to_vec(&document).unwrap();
        while !json_chunk.len().is_multiple_of(4) {
            json_chunk.push(b' ');
        }
        let mut bin_chunk = self.buffer.clone();
        while !bin_chunk.len().is_multiple_of(4) {
            bin_chunk.push(0);
        }
        let total_length = 12 + 8 + json_chunk.len() + 8 + bin_chunk.len();
        let mut glb = Vec::with_capacity(total_length);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(&(total_length as u32).to_le_bytes());
        glb.extend_from_slice(&(json_chunk.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x4e4f534au32.to_le_bytes());
        glb.extend_from_slice(&json_chunk);
        glb.extend_from_slice(&(bin_chunk.len() as u32).to_le_bytes());
        glb.extend_from_slice(&0x004e4942u32.to_le_bytes());
        glb.extend_from_slice(&bin_chunk);
        glb
    }
}

fn replace_embedded_png(glb: &[u8], original: &[u8], replacement: &[u8]) -> Vec<u8> {
    assert_eq!(&glb[..4], b"glTF");
    let json_len = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let json_start = 20;
    let json_end = json_start + json_len;
    let mut document: Value = serde_json::from_slice(&glb[json_start..json_end]).unwrap();
    let bin_chunk_header = json_end;
    let bin_len = u32::from_le_bytes(
        glb[bin_chunk_header..bin_chunk_header + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let bin_start = bin_chunk_header + 8;
    let bin_end = bin_start + bin_len;
    let bin = &glb[bin_start..bin_end];
    let image_start = bin
        .windows(original.len())
        .position(|window| window == original)
        .expect("embedded image should exist in the GLB BIN chunk");
    let mut edited_bin = Vec::with_capacity(bin.len() - original.len() + replacement.len());
    edited_bin.extend_from_slice(&bin[..image_start]);
    edited_bin.extend_from_slice(replacement);
    edited_bin.extend_from_slice(&bin[image_start + original.len()..]);
    document["buffers"][0]["byteLength"] = json!(edited_bin.len());
    document["bufferViews"][2]["byteLength"] = json!(replacement.len());

    let mut json_chunk = serde_json::to_vec(&document).unwrap();
    while !json_chunk.len().is_multiple_of(4) {
        json_chunk.push(b' ');
    }
    while !edited_bin.len().is_multiple_of(4) {
        edited_bin.push(0);
    }
    let total_length = 12 + 8 + json_chunk.len() + 8 + edited_bin.len();
    let mut edited = Vec::with_capacity(total_length);
    edited.extend_from_slice(b"glTF");
    edited.extend_from_slice(&2u32.to_le_bytes());
    edited.extend_from_slice(&(total_length as u32).to_le_bytes());
    edited.extend_from_slice(&(json_chunk.len() as u32).to_le_bytes());
    edited.extend_from_slice(&0x4e4f534au32.to_le_bytes());
    edited.extend_from_slice(&json_chunk);
    edited.extend_from_slice(&(edited_bin.len() as u32).to_le_bytes());
    edited.extend_from_slice(&0x004e4942u32.to_le_bytes());
    edited.extend_from_slice(&edited_bin);
    edited
}

fn project_scene(asset_ids: &[&str], uri: &str, metadata: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1,
        "revision": 1,
        "entities": [],
        "assets": asset_ids.iter().map(|id| json!({
            "id": id,
            "kind": "daedalus.model",
            "uri": uri,
            "metadata": metadata,
        })).collect::<Vec<_>>()
    }))
    .unwrap()
}

fn prepare(
    sources: BTreeMap<String, Vec<u8>>,
    asset_ids: &[&str],
    uri: &str,
    metadata: Value,
    cache: &mut MemoryCookCache,
) -> Result<PreparedProject, crate::assets::AssetImportError> {
    prepare_with_limits(
        sources,
        asset_ids,
        uri,
        metadata,
        cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
}

fn prepare_with_limits(
    sources: BTreeMap<String, Vec<u8>>,
    asset_ids: &[&str],
    uri: &str,
    metadata: Value,
    cache: &mut MemoryCookCache,
    limits: ImportLimits,
    cancellation: &ImportCancellation,
) -> Result<PreparedProject, crate::assets::AssetImportError> {
    let mut resolver = crate::assets::ByteMapSourceResolver::from_bytes(sources).unwrap();
    let manifest = serde_json::to_vec(&json!({
        "schema_version": 1,
        "name": "gltf static fixture",
        "scene": "scene.json"
    }))
    .unwrap();
    let scene = project_scene(asset_ids, uri, metadata);
    prepare_project_with_assets(
        &manifest,
        &scene,
        &mut resolver,
        cache,
        limits,
        cancellation,
    )
}

fn model<'a>(project: &'a PreparedProject, id: &str) -> &'a Arc<crate::assets::StaticModel> {
    match project
        .asset_catalog()
        .unwrap()
        .asset(&crate::scene::StableId::new(id).unwrap())
        .unwrap()
    {
        ImportedAsset::Model(model) => model,
        other => panic!("expected a model, got {other:?}"),
    }
}

#[test]
fn glb_embedded_png_and_external_interleaved_normalized_uv_preserve_static_ir() {
    let fixture = Fixture::new(true);
    let mut cache = MemoryCookCache::default();
    let glb = fixture.glb();
    let project = prepare(
        BTreeMap::from([("models/triangle.glb".into(), glb)]),
        &["model:embedded"],
        "models/triangle.glb",
        json!({}),
        &mut cache,
    )
    .unwrap();
    let imported_model = model(&project, "model:embedded");
    assert_eq!(imported_model.selected_scene_roots(), &[0]);
    assert_eq!(imported_model.nodes().len(), 3);
    assert_eq!(imported_model.nodes()[1].parent(), Some(0));
    assert_eq!(
        &imported_model.nodes()[0].local_transform()[12..15],
        &[1.0, 2.0, 3.0]
    );
    assert!(imported_model.nodes()[0].primitive_indices().is_empty());
    assert_eq!(imported_model.nodes()[1].primitive_indices(), &[0]);
    assert_eq!(imported_model.nodes()[2].primitive_indices(), &[0]);
    assert_eq!(imported_model.primitives()[0].primitive_ordinal(), 0);
    assert_eq!(imported_model.primitives()[0].material_index(), Some(0));
    assert_eq!(
        imported_model.primitives()[0].geometry().positions().len(),
        3
    );
    assert_eq!(
        imported_model.primitives()[0].geometry().indices(),
        &[0, 1, 2]
    );
    assert_eq!(
        imported_model.primitives()[0].geometry().uv0().unwrap()[1],
        [1.0, 0.0]
    );
    assert_eq!(
        imported_model.materials()[0].base_color_factor,
        [0.4, 0.5, 0.6, 1.0]
    );
    assert_eq!(imported_model.materials()[0].metallic_factor, 0.2);
    assert_eq!(imported_model.materials()[0].roughness_factor, 0.7);
    let sampler = imported_model.materials()[0]
        .base_color_texture
        .as_ref()
        .unwrap()
        .sampler();
    assert_eq!(sampler.min_filter, TextureFilter::Linear);
    assert_eq!(sampler.mag_filter, TextureFilter::Nearest);
    assert_eq!(sampler.mipmap_filter, MipmapFilter::Linear);
    assert_eq!(sampler.wrap_u, TextureWrap::MirroredRepeat);
    assert_eq!(sampler.wrap_v, TextureWrap::ClampToEdge);
    assert_eq!(imported_model.owned_images().len(), 1);
    assert_eq!(
        imported_model.owned_images()[0].asset().decoded().rgba8(),
        &[220, 120, 30, 255]
    );

    let fixture = Fixture::new(false);
    let mut sources = fixture.external_sources();
    let mut unindexed = fixture.json.clone();
    unindexed["meshes"][0]["primitives"][0]
        .as_object_mut()
        .unwrap()
        .remove("indices");
    sources.insert(MODEL_URI.into(), serde_json::to_vec(&unindexed).unwrap());
    let external = prepare(
        sources,
        &["model:external"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    let geometry = model(&external, "model:external").primitives()[0]
        .geometry()
        .clone();
    assert_eq!(geometry.indices(), &[0, 1, 2]);
}

#[test]
fn explicit_scene_selection_and_shared_geometry_work_across_nodes_and_asset_ids() {
    let fixture = Fixture::new(true);
    let mut sources = fixture.external_sources();
    // Both assets use the same source geometry but select different root sets.
    let mut second = fixture.external_json();
    second["scene"] = json!(1);
    let second_uri = "models/second.gltf";
    sources.insert(second_uri.into(), serde_json::to_vec(&second).unwrap());
    sources.insert("models/second.bin".into(), fixture.buffer.clone());
    sources.insert("models/second.png".into(), fixture.png.clone());
    second["buffers"][0]["uri"] = json!("second.bin");
    second["images"][0]["uri"] = json!("second.png");
    sources.insert(second_uri.into(), serde_json::to_vec(&second).unwrap());

    let mut cache = MemoryCookCache::default();
    let scene = serde_json::to_vec(&json!({
        "schema_version": 1,
        "revision": 1,
        "entities": [],
        "assets": [
            {"id":"model:first", "kind":"daedalus.model", "uri":MODEL_URI, "metadata":{}},
            {"id":"model:second", "kind":"daedalus.model", "uri":second_uri, "metadata":{}}
        ]
    }))
    .unwrap();
    let manifest =
        serde_json::to_vec(&json!({"schema_version":1,"name":"shared","scene":"scene.json"}))
            .unwrap();
    let mut resolver = crate::assets::ByteMapSourceResolver::from_bytes(sources).unwrap();
    let project = prepare_project_with_assets(
        &manifest,
        &scene,
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .unwrap();
    let first = model(&project, "model:first");
    let second = model(&project, "model:second");
    assert_eq!(first.selected_scene_roots(), &[0]);
    assert_eq!(second.selected_scene_roots(), &[2]);
    assert!(Arc::ptr_eq(
        first.primitives()[0].geometry(),
        second.primitives()[0].geometry()
    ));
    assert_eq!(
        project
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        1
    );
}

#[test]
fn explicit_scene_index_metadata_selects_scene_without_a_default() {
    let fixture = Fixture::new(true);
    let mut document = fixture.json.clone();
    document.as_object_mut().unwrap().remove("scene");
    let mut sources = fixture.external_sources();
    sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let mut cache = MemoryCookCache::default();
    let project = prepare(
        sources,
        &["model:selected"],
        MODEL_URI,
        json!({"schema_version": 1, "scene_index": 1}),
        &mut cache,
    )
    .unwrap();
    assert_eq!(
        model(&project, "model:selected").selected_scene_roots(),
        &[2]
    );
}

#[test]
fn semantic_geometry_cache_survives_material_node_and_png_edits_but_tracks_geometry() {
    let fixture = Fixture::new(true);
    let mut cache = MemoryCookCache::default();
    let first = prepare(
        fixture.external_sources(),
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    let first_key = first
        .asset_catalog()
        .unwrap()
        .asset(&crate::scene::StableId::new("model:stable").unwrap())
        .unwrap()
        .primary_key();
    let geometry_key = model(&first, "model:stable").primitives()[0]
        .geometry()
        .key();

    let unchanged = prepare(
        fixture.external_sources(),
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_eq!(model(&unchanged, "model:stable").key(), first_key);
    assert_eq!(
        model(&unchanged, "model:stable").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    assert_eq!(
        unchanged
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        0
    );

    let mut material_edit = fixture.external_sources();
    let mut document: Value =
        serde_json::from_slice(material_edit.get(MODEL_URI).unwrap()).unwrap();
    document["materials"][0]["pbrMetallicRoughness"]["metallicFactor"] = json!(0.8);
    material_edit.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let material_changed = prepare(
        material_edit,
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_ne!(model(&material_changed, "model:stable").key(), first_key);
    assert_eq!(
        model(&material_changed, "model:stable").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    assert_eq!(
        material_changed
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        0
    );

    let mut node_edit = fixture.external_sources();
    let mut document: Value = serde_json::from_slice(node_edit.get(MODEL_URI).unwrap()).unwrap();
    document["nodes"][0]["translation"] = json!([8.0, 2.0, 3.0]);
    node_edit.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let node_changed = prepare(
        node_edit,
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_ne!(model(&node_changed, "model:stable").key(), first_key);
    assert_eq!(
        model(&node_changed, "model:stable").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    assert_eq!(
        node_changed
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        0
    );

    let mut png_edit = fixture.external_sources();
    png_edit.insert("models/albedo.png".into(), png_rgba([10, 20, 30, 255]));
    let image_changed = prepare(
        png_edit,
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_ne!(model(&image_changed, "model:stable").key(), first_key);
    assert_eq!(
        model(&image_changed, "model:stable").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    assert_eq!(
        image_changed
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        0
    );
    assert_eq!(
        image_changed
            .asset_import_report()
            .unwrap()
            .metrics()
            .png_base_decodes,
        1
    );

    let mut geometry_edit = fixture.external_sources();
    let mut bytes = geometry_edit.get("models/mesh.bin").unwrap().clone();
    bytes[0..4].copy_from_slice(&0.25_f32.to_le_bytes());
    geometry_edit.insert("models/mesh.bin".into(), bytes);
    let geometry_changed = prepare(
        geometry_edit,
        &["model:stable"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_ne!(
        model(&geometry_changed, "model:stable").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    assert_eq!(
        geometry_changed
            .asset_import_report()
            .unwrap()
            .metrics()
            .model_geometry_cooks,
        1
    );
}

#[test]
fn embedded_png_edit_rebuilds_owner_model_without_recooking_geometry() {
    let fixture = Fixture::new(true);
    let original_glb = fixture.glb();
    let mut cache = MemoryCookCache::default();
    let first = prepare(
        BTreeMap::from([("models/embedded.glb".into(), original_glb.clone())]),
        &["model:embedded-edit"],
        "models/embedded.glb",
        json!({}),
        &mut cache,
    )
    .unwrap();
    let first_key = model(&first, "model:embedded-edit").key();
    let geometry_key = model(&first, "model:embedded-edit").primitives()[0]
        .geometry()
        .key();

    let replacement_png = png_rgba([17, 29, 41, 255]);
    let edited_glb = replace_embedded_png(&original_glb, &fixture.png, &replacement_png);
    let edited = prepare(
        BTreeMap::from([("models/embedded.glb".into(), edited_glb)]),
        &["model:embedded-edit"],
        "models/embedded.glb",
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert_ne!(model(&edited, "model:embedded-edit").key(), first_key);
    assert_eq!(
        model(&edited, "model:embedded-edit").primitives()[0]
            .geometry()
            .key(),
        geometry_key
    );
    let metrics = edited.asset_import_report().unwrap().metrics();
    assert_eq!(metrics.model_geometry_cooks, 0);
    assert_eq!(metrics.png_base_decodes, 1);
}

#[test]
fn embedded_gamma_png_preserves_gltf_material_rgba_and_reuses_warm_decode() {
    let fixture = Fixture::new(true);
    let source_png = png_rgba_with_source_gamma([128, 64, 32, 255], 1.0);
    let glb = replace_embedded_png(&fixture.glb(), &fixture.png, &source_png);
    let mut cache = MemoryCookCache::default();

    let cold = prepare(
        BTreeMap::from([("models/gamma.glb".into(), glb.clone())]),
        &["model:gamma"],
        "models/gamma.glb",
        json!({}),
        &mut cache,
    )
    .unwrap();
    let cold_model = model(&cold, "model:gamma");
    let cold_image = cold_model.owned_images()[0].asset();
    assert_eq!(cold_image.decoded().rgba8(), &[128, 64, 32, 255]);
    assert_eq!(
        cold_model.materials()[0]
            .base_color_texture
            .as_ref()
            .unwrap()
            .owner_image()
            .image_index(),
        0
    );
    assert_eq!(
        cold.asset_import_report()
            .unwrap()
            .metrics()
            .png_base_decodes,
        1
    );

    let warm = prepare(
        BTreeMap::from([("models/gamma.glb".into(), glb)]),
        &["model:gamma"],
        "models/gamma.glb",
        json!({}),
        &mut cache,
    )
    .unwrap();
    let warm_model = model(&warm, "model:gamma");
    let warm_image = warm_model.owned_images()[0].asset();
    assert_eq!(warm_image.decoded().rgba8(), &[128, 64, 32, 255]);
    assert_eq!(warm_image.key(), cold_image.key());
    assert_eq!(
        warm.asset_import_report()
            .unwrap()
            .metrics()
            .png_base_decodes,
        0
    );
}

#[test]
fn malformed_glb_sparse_accessors_uri_escape_and_required_extensions_fail_located() {
    let fixture = Fixture::new(true);
    let mut cache = MemoryCookCache::default();
    let mut malformed = fixture.glb();
    let malformed_length = (malformed.len() - 4) as u32;
    malformed[8..12].copy_from_slice(&malformed_length.to_le_bytes());
    let error = prepare(
        BTreeMap::from([("models/triangle.glb".into(), malformed)]),
        &["model:bad-glb"],
        "models/triangle.glb",
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(error.code().as_str(), "invalid");
    assert_eq!(error.path(), Some("glb.header"));

    let mut sparse_sources = fixture.external_sources();
    let mut document: Value =
        serde_json::from_slice(sparse_sources.get(MODEL_URI).unwrap()).unwrap();
    document["bufferViews"].as_array_mut().unwrap().push(json!({
        "buffer":0,
        "byteOffset":0,
        "byteLength":12
    }));
    document["accessors"][0]["sparse"] = json!({
        "count":1,
        "indices":{"bufferView":1,"componentType":5123},
        "values":{"bufferView":3}
    });
    sparse_sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let sparse_error = prepare(
        sparse_sources,
        &["model:sparse"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(sparse_error.code().as_str(), "unsupported");
    assert!(sparse_error.path().unwrap().contains("POSITION"));

    let mut accessor_sources = fixture.external_sources();
    let mut document: Value =
        serde_json::from_slice(accessor_sources.get(MODEL_URI).unwrap()).unwrap();
    document["accessors"][0]["byteOffset"] = json!(80);
    accessor_sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let accessor_error = prepare(
        accessor_sources,
        &["model:accessor"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert!(matches!(accessor_error.code().as_str(), "invalid" | "json"));

    let mut escaped_sources = fixture.external_sources();
    let mut document: Value =
        serde_json::from_slice(escaped_sources.get(MODEL_URI).unwrap()).unwrap();
    document["buffers"][0]["uri"] = json!("../../outside.bin");
    escaped_sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let escape_error = prepare(
        escaped_sources,
        &["model:escape"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(escape_error.code().as_str(), "invalid_path");

    let mut extension_sources = fixture.external_sources();
    let mut document: Value =
        serde_json::from_slice(extension_sources.get(MODEL_URI).unwrap()).unwrap();
    document["extensionsUsed"] = json!(["KHR_materials_unlit"]);
    extension_sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let extension_error = prepare(
        extension_sources,
        &["model:extension"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(extension_error.code().as_str(), "unsupported");
    assert_eq!(extension_error.path(), Some("extensionsUsed"));

    for (asset_id, extension) in [
        ("model:unknown-required", "VENDOR_unknown_extension"),
        ("model:draco-required", "KHR_draco_mesh_compression"),
    ] {
        let mut required_sources = fixture.external_sources();
        let mut document: Value =
            serde_json::from_slice(required_sources.get(MODEL_URI).unwrap()).unwrap();
        document["extensionsRequired"] = json!([extension]);
        required_sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
        let required_error = prepare(
            required_sources,
            &[asset_id],
            MODEL_URI,
            json!({}),
            &mut cache,
        )
        .unwrap_err();
        assert_eq!(required_error.code().as_str(), "unsupported");
        assert_eq!(required_error.path(), Some("extensionsRequired"));
    }
}

#[test]
fn unique_geometry_budget_is_aggregated_across_model_descriptors() {
    let fixture = Fixture::new(true);
    let mut second_document = fixture.json.clone();
    second_document["buffers"][0]["uri"] = json!("second.bin");
    second_document["images"][0]["uri"] = json!("second.png");
    let mut second_buffer = fixture.buffer.clone();
    second_buffer[0..4].copy_from_slice(&0.5_f32.to_le_bytes());
    let mut sources = fixture.external_sources();
    sources.insert(
        "models/second.gltf".into(),
        serde_json::to_vec(&second_document).unwrap(),
    );
    sources.insert("models/second.bin".into(), second_buffer);
    sources.insert("models/second.png".into(), fixture.png.clone());
    let manifest =
        serde_json::to_vec(&json!({"schema_version":1,"name":"aggregate","scene":"scene.json"}))
            .unwrap();
    let scene = serde_json::to_vec(&json!({
        "schema_version":1,
        "revision":1,
        "entities":[],
        "assets":[
            {"id":"model:first","kind":"daedalus.model","uri":MODEL_URI,"metadata":{}},
            {"id":"model:second","kind":"daedalus.model","uri":"models/second.gltf","metadata":{}}
        ]
    }))
    .unwrap();
    let mut resolver = crate::assets::ByteMapSourceResolver::from_bytes(sources).unwrap();
    let mut cache = MemoryCookCache::default();
    let error = prepare_project_with_assets(
        &manifest,
        &scene,
        &mut resolver,
        &mut cache,
        ImportLimits {
            max_geometry_vertices: 5,
            ..ImportLimits::default()
        },
        &ImportCancellation::never(),
    )
    .unwrap_err();
    assert_eq!(error.code().as_str(), "limit_exceeded");
    assert_eq!(error.path(), Some("meshes[0].primitives[0]"));
}

#[test]
fn node_graph_cycles_multiple_parents_and_excess_depth_are_rejected() {
    let fixture = Fixture::new(true);
    let mut cache = MemoryCookCache::default();

    let mut cycle_sources = fixture.external_sources();
    let mut cycle: Value = serde_json::from_slice(cycle_sources.get(MODEL_URI).unwrap()).unwrap();
    cycle["nodes"][1]["children"] = json!([0]);
    cycle_sources.insert(MODEL_URI.into(), serde_json::to_vec(&cycle).unwrap());
    let cycle_error = prepare(
        cycle_sources,
        &["model:cycle"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(cycle_error.code().as_str(), "invalid");

    let mut parent_sources = fixture.external_sources();
    let mut multiple_parents: Value =
        serde_json::from_slice(parent_sources.get(MODEL_URI).unwrap()).unwrap();
    multiple_parents["nodes"][0]["children"] = json!([2]);
    multiple_parents["nodes"][1]["children"] = json!([2]);
    multiple_parents["nodes"][1]
        .as_object_mut()
        .unwrap()
        .remove("mesh");
    parent_sources.insert(
        MODEL_URI.into(),
        serde_json::to_vec(&multiple_parents).unwrap(),
    );
    let parent_error = prepare(
        parent_sources,
        &["model:multiple-parents"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(parent_error.code().as_str(), "invalid");

    let mut depth_sources = fixture.external_sources();
    let mut depth: Value = serde_json::from_slice(depth_sources.get(MODEL_URI).unwrap()).unwrap();
    let node_count = ImportLimits::default().max_dependency_depth + 1;
    let mut nodes = (0..node_count)
        .map(|index| {
            if index + 1 < node_count {
                json!({"children":[index+1]})
            } else {
                json!({})
            }
        })
        .collect::<Vec<_>>();
    nodes[0]["translation"] = json!([0.0, 0.0, 0.0]);
    depth["nodes"] = json!(nodes);
    depth["scenes"] = json!([{"nodes":[0]}, {"nodes":[0]}]);
    depth_sources.insert(MODEL_URI.into(), serde_json::to_vec(&depth).unwrap());
    let depth_error = prepare(
        depth_sources,
        &["model:depth"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap_err();
    assert_eq!(depth_error.code().as_str(), "limit_exceeded");
}

#[test]
fn texture_without_uv_and_premature_cancellation_fail_without_partial_catalog() {
    let fixture = Fixture::new(false);
    let mut untextured_sources = fixture.external_sources();
    let mut untextured: Value =
        serde_json::from_slice(untextured_sources.get(MODEL_URI).unwrap()).unwrap();
    untextured["meshes"][0]["primitives"][0]["attributes"]
        .as_object_mut()
        .unwrap()
        .remove("TEXCOORD_0");
    untextured["materials"][0]["pbrMetallicRoughness"]
        .as_object_mut()
        .unwrap()
        .remove("baseColorTexture");
    untextured_sources.insert(MODEL_URI.into(), serde_json::to_vec(&untextured).unwrap());
    let mut cache = MemoryCookCache::default();
    let untextured_project = prepare(
        untextured_sources,
        &["model:untextured"],
        MODEL_URI,
        json!({}),
        &mut cache,
    )
    .unwrap();
    assert!(
        model(&untextured_project, "model:untextured").primitives()[0]
            .geometry()
            .uv0()
            .is_none()
    );

    let mut sources = fixture.external_sources();
    let mut document: Value = serde_json::from_slice(sources.get(MODEL_URI).unwrap()).unwrap();
    document["meshes"][0]["primitives"][0]["attributes"]
        .as_object_mut()
        .unwrap()
        .remove("TEXCOORD_0");
    sources.insert(MODEL_URI.into(), serde_json::to_vec(&document).unwrap());
    let missing_uv =
        prepare(sources, &["model:no-uv"], MODEL_URI, json!({}), &mut cache).unwrap_err();
    assert_eq!(missing_uv.code().as_str(), "unsupported");
    assert!(missing_uv.path().unwrap().contains("TEXCOORD_0"));

    let fixture = Fixture::new(true);
    let sources = fixture.external_sources();
    let mut resolver = crate::assets::ByteMapSourceResolver::from_bytes(sources).unwrap();
    let manifest =
        serde_json::to_vec(&json!({"schema_version":1,"name":"cancelled","scene":"scene.json"}))
            .unwrap();
    let scene = project_scene(&["model:cancel"], MODEL_URI, json!({}));
    let cancellation = ImportCancellation::from_predicate(|| true);
    let mut cache = MemoryCookCache::default();
    let cancelled = prepare_project_with_assets(
        &manifest,
        &scene,
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &cancellation,
    )
    .unwrap_err();
    assert_eq!(cancelled.code().as_str(), "cancelled");
    assert_eq!(cache.stats().current_entries, 0);
}
