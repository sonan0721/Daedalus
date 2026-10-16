//! Bounded glTF 2.0 / GLB static triangle import.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::super::{
    AssetImportContext, AssetImportError, AssetImportErrorCode, AssetKind, BudgetKind, CacheKey,
    CacheNamespace, CacheObjectKind, CacheRead, GeometryResource, ImageRole, ImageSampler,
    ImportLimits, ImportLocation, ImportedAsset, LeafCookKey, MipPolicy, MipmapFilter,
    ModelImageIdentity, ModelMaterial, ModelNode, ModelOwnedImage, ModelPrimitive, ModelTextureRef,
    StaticModel, TextureFilter, TextureWrap,
};
use super::binary::{BinaryError, BinaryReader, BinaryWriter};
use super::image::EffectiveSettings;
use super::{check_schema_version, location, parse_settings};
use crate::assets::{PngColorPolicy, source::SourceBlob};
use crate::scene::{AssetDescriptor, StableId};

const IMPORTER_VERSION: u32 = 1;
const GEOMETRY_CACHE_MAGIC: &[u8; 4] = b"DGEO";
const GEOMETRY_CACHE_HEADER: u64 = 4 + 4 + 4 + 1;
const MAX_GLTF_JSON_CHUNK: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSettings {
    schema_version: u32,
    #[serde(default)]
    scene_index: Option<usize>,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            scene_index: None,
        }
    }
}

#[derive(Clone)]
enum BufferStorage<'a> {
    Glb(&'a [u8]),
    External(Arc<SourceBlob>),
}

impl BufferStorage<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Glb(bytes) => bytes,
            Self::External(source) => source.bytes(),
        }
    }
}

#[derive(Clone)]
struct BufferData<'a> {
    storage: BufferStorage<'a>,
    declared_length: usize,
}

impl BufferData<'_> {
    fn bytes(&self) -> &[u8] {
        self.storage.bytes()
    }
}

/// Imports a glTF/GLB container into immutable CPU model resources.
pub(super) fn import(
    descriptor: &AssetDescriptor,
    source: Arc<SourceBlob>,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedAsset, AssetImportError> {
    let settings: ModelSettings = parse_settings(&descriptor.metadata, &source, "metadata")?;
    check_schema_version(settings.schema_version, &source, "metadata.schema_version")?;
    check_container_extension(&source)?;
    context.check_cancelled(location(&source, "gltf.parse"))?;

    if source.byte_len() > context.limits().max_source_bytes {
        return Err(error(
            &source,
            "gltf",
            AssetImportErrorCode::LimitExceeded,
            "glTF source exceeds configured per-file byte limit",
        ));
    }
    let _parser_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        source.byte_len(),
        location(&source, "gltf.parse"),
    )?;

    let is_glb = validate_glb_if_present(&source)?;
    context.record_container_parse();
    let mut gltf =
        gltf::Gltf::from_slice_without_validation(source.bytes()).map_err(|parse_error| {
            error(
                &source,
                "gltf.json",
                AssetImportErrorCode::Json,
                format!("glTF 2.0 parse failed: {parse_error}"),
            )
        })?;
    context.check_cancelled(location(&source, "gltf.parse"))?;
    if is_glb
        != source
            .source_id()
            .as_str()
            .to_ascii_lowercase()
            .ends_with(".glb")
    {
        return Err(error(
            &source,
            "uri",
            AssetImportErrorCode::Invalid,
            "glTF source extension does not match its container bytes",
        ));
    }
    // Inspect extension declarations before validation: gltf-rs validation itself
    // rejects unsupported declarations, which would lose our stable located
    // Unsupported classification. Reconstructing the document through the public
    // validating constructor below still runs the complete validation for inputs
    // whose declared extension set is supported (currently empty).
    reject_extensions(&gltf, &source)?;
    gltf.document =
        gltf::Document::from_json(gltf.document.into_json()).map_err(|validation_error| {
            error(
                &source,
                "gltf.validation",
                AssetImportErrorCode::Json,
                format!("glTF 2.0 validation failed: {validation_error}"),
            )
        })?;
    reject_unsupported_top_level(&gltf, &source)?;

    let limits = *context.limits();
    let document = &gltf.document;
    let node_count = document.nodes().count();
    let material_count = document.materials().count();
    let scene_count = document.scenes().count();
    let mesh_count = document.meshes().count();
    let image_count = document.images().count();
    if scene_count == 0 {
        return Err(error(
            &source,
            "scenes",
            AssetImportErrorCode::Invalid,
            "glTF model must contain at least one scene",
        ));
    }
    if node_count > limits.max_model_nodes {
        return Err(limit_error(
            &source,
            "nodes",
            "glTF node count exceeds the configured catalog limit",
        ));
    }
    if mesh_count > limits.max_model_primitives {
        return Err(limit_error(
            &source,
            "meshes",
            "glTF mesh definition count exceeds the configured primitive catalog limit",
        ));
    }
    if material_count > limits.max_model_materials {
        return Err(limit_error(
            &source,
            "materials",
            "glTF material count exceeds the configured catalog limit",
        ));
    }
    if image_count > limits.max_decoded_images {
        return Err(limit_error(
            &source,
            "images",
            "glTF image definition count exceeds the configured model image limit",
        ));
    }
    context.check_budget(
        BudgetKind::ModelNodes,
        node_count as u64,
        location(&source, "nodes"),
    )?;
    context.check_budget(
        BudgetKind::ModelMaterials,
        material_count as u64,
        location(&source, "materials"),
    )?;
    context.account(
        BudgetKind::ModelNodes,
        node_count as u64,
        location(&source, "nodes"),
    )?;
    context.account(
        BudgetKind::ModelMaterials,
        material_count as u64,
        location(&source, "materials"),
    )?;

    let selected_scene_index = choose_scene(&gltf, settings.scene_index, scene_count, &source)?;
    let selected_scene = document.scenes().nth(selected_scene_index).ok_or_else(|| {
        error(
            &source,
            "scenes",
            AssetImportErrorCode::Invalid,
            "selected scene index is outside the scene array",
        )
    })?;
    let root_count = selected_scene.nodes().count();
    let graph_scratch = (node_count as u64)
        .checked_mul(64)
        .and_then(|value| {
            value.checked_add((root_count as u64).checked_mul(std::mem::size_of::<usize>() as u64)?)
        })
        .and_then(|value| {
            value
                .checked_add((limits.max_dependency_depth.min(node_count) as u64).checked_mul(128)?)
        })
        .ok_or_else(|| limit_error(&source, "nodes", "node graph scratch size overflow"))?;
    let _graph_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        graph_scratch,
        location(&source, "nodes"),
    )?;
    let mut roots = Vec::new();
    roots
        .try_reserve_exact(root_count)
        .map_err(|_| allocation_error(&source, "scenes"))?;
    roots.extend(selected_scene.nodes().map(|node| node.index()));
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| allocation_error(&source, "nodes"))?;
    nodes.extend(document.nodes());
    let (parents, child_depths) =
        validate_node_graph(&nodes, &source, limits.max_dependency_depth)?;
    validate_scene_roots(document, &parents, &source)?;
    let mut root_indices = Vec::new();
    root_indices
        .try_reserve_exact(roots.len())
        .map_err(|_| allocation_error(&source, "scenes"))?;
    for root in &roots {
        root_indices.push(u32::try_from(*root).map_err(|_| {
            limit_error(&source, "scenes", "scene root index is not representable")
        })?);
    }
    for root in &roots {
        if *root >= nodes.len() || parents.get(*root).is_none_or(Option::is_some) {
            return Err(error(
                &source,
                format!("scenes[{selected_scene_index}].nodes"),
                AssetImportErrorCode::Invalid,
                "scene root is out of range or also appears as another node's child",
            ));
        }
    }
    if roots.is_empty() {
        return Err(error(
            &source,
            format!("scenes[{selected_scene_index}].nodes"),
            AssetImportErrorCode::Invalid,
            "selected glTF scene has no root nodes",
        ));
    }

    let buffer_count = document.buffers().count();
    let buffer_table_scratch = (buffer_count as u64)
        .checked_mul(std::mem::size_of::<BufferData<'_>>() as u64)
        .ok_or_else(|| limit_error(&source, "buffers", "buffer table scratch size overflow"))?;
    let _buffer_table_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        buffer_table_scratch,
        location(&source, "buffers"),
    )?;
    let buffers = load_buffers(&gltf, &source, context)?;
    let mesh_primitive_counts =
        count_mesh_primitives(document, &source, limits.max_model_primitives)?;
    let primitive_count = mesh_primitive_counts
        .iter()
        .try_fold(0usize, |total, count| total.checked_add(*count))
        .ok_or_else(|| limit_error(&source, "meshes.primitives", "primitive count overflow"))?;
    context.check_budget(
        BudgetKind::ModelPrimitives,
        primitive_count as u64,
        location(&source, "meshes.primitives"),
    )?;
    context.account(
        BudgetKind::ModelPrimitives,
        primitive_count as u64,
        location(&source, "meshes.primitives"),
    )?;

    let node_primitive_reference_count =
        count_node_primitive_references(&nodes, &mesh_primitive_counts, &source)?;
    let model_metadata_bytes = estimate_model_metadata_bytes(
        node_count,
        primitive_count,
        material_count,
        node_primitive_reference_count,
        roots.len(),
        document.images().count(),
        &descriptor.id,
    )?;
    context.check_budget(
        BudgetKind::CookedBytes,
        model_metadata_bytes,
        location(&source, "model.metadata"),
    )?;
    context.account(
        BudgetKind::CookedBytes,
        model_metadata_bytes,
        location(&source, "model.metadata"),
    )?;

    let _model_metadata_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        model_metadata_bytes,
        location(&source, "model.metadata"),
    )?;

    let imported_materials = import_materials(&gltf, &buffers, descriptor, &source, context)?;
    let ImportedMaterials {
        materials,
        owned_images,
        material_textures,
    } = imported_materials;

    let mut geometry_by_key = BTreeMap::<[u8; 32], Arc<GeometryResource>>::new();
    let mut primitives = Vec::new();
    primitives
        .try_reserve_exact(primitive_count)
        .map_err(|_| allocation_error(&source, "meshes.primitives"))?;
    let mut mesh_ranges = Vec::<(u32, u32)>::new();
    mesh_ranges
        .try_reserve_exact(mesh_count)
        .map_err(|_| allocation_error(&source, "meshes"))?;

    for mesh in document.meshes() {
        context.check_cancelled(location(&source, format!("meshes[{}]", mesh.index())))?;
        let start = u32::try_from(primitives.len()).map_err(|_| {
            limit_error(
                &source,
                "meshes.primitives",
                "primitive index is not representable",
            )
        })?;
        for primitive in mesh.primitives() {
            let path = format!("meshes[{}].primitives[{}]", mesh.index(), primitive.index());
            context.check_cancelled(location(&source, &path))?;
            reject_unsupported_primitive(&primitive, &source, &path)?;
            let material_index = primitive
                .material()
                .index()
                .map(|index| {
                    u32::try_from(index).map_err(|_| {
                        limit_error(
                            source.as_ref(),
                            format!("{path}.material"),
                            "material index is not representable",
                        )
                    })
                })
                .transpose()?;
            let geometry = import_geometry(
                &primitive,
                &buffers,
                context,
                &source,
                &path,
                &mut geometry_by_key,
            )?;
            if material_index.is_some_and(|index| {
                material_textures
                    .get(index as usize)
                    .is_some_and(Option::is_some)
            }) && geometry.uv0().is_none()
            {
                return Err(error(
                    &source,
                    format!("{path}.attributes.TEXCOORD_0"),
                    AssetImportErrorCode::Unsupported,
                    "a base-color texture requires a TEXCOORD_0 geometry attribute",
                ));
            }
            let ordinal = u32::try_from(primitive.index()).map_err(|_| {
                limit_error(
                    source.as_ref(),
                    format!("{path}.ordinal"),
                    "primitive ordinal is not representable",
                )
            })?;
            primitives.push(ModelPrimitive::new(ordinal, material_index, geometry));
        }
        let end = u32::try_from(primitives.len()).map_err(|_| {
            limit_error(
                &source,
                "meshes.primitives",
                "primitive index is not representable",
            )
        })?;
        mesh_ranges.push((start, end));
    }

    let mut model_nodes = Vec::new();
    model_nodes
        .try_reserve_exact(node_count)
        .map_err(|_| allocation_error(&source, "nodes"))?;
    for (index, node) in nodes.iter().enumerate() {
        context.check_cancelled(location(&source, format!("nodes[{index}]")))?;
        let local_matrix = node.transform().matrix();
        let local_transform = std::array::from_fn(|element| local_matrix[element / 4][element % 4]);
        let primitive_indices = if let Some(mesh) = node.mesh() {
            let (start, end) = *mesh_ranges.get(mesh.index()).ok_or_else(|| {
                error(
                    &source,
                    format!("nodes[{index}].mesh"),
                    AssetImportErrorCode::Invalid,
                    "node references an out-of-range mesh",
                )
            })?;
            let reference_count = (end - start) as usize;
            let mut references = Vec::new();
            references
                .try_reserve_exact(reference_count)
                .map_err(|_| allocation_error(&source, format!("nodes[{index}].mesh")))?;
            references.extend(start..end);
            references
        } else {
            Vec::new()
        };
        let parent = parents[index]
            .map(|parent| {
                u32::try_from(parent).map_err(|_| {
                    limit_error(
                        source.as_ref(),
                        format!("nodes[{index}].children"),
                        "parent index is not representable",
                    )
                })
            })
            .transpose()?;
        model_nodes.push(ModelNode::new(parent, local_transform, primitive_indices));
    }
    drop(child_depths);

    let model_key = make_model_key(
        context,
        ModelKeyParts {
            scene_index: selected_scene_index,
            roots: &root_indices,
            nodes: &model_nodes,
            primitives: &primitives,
            materials: &materials,
            images: &owned_images,
            owner: &descriptor.id,
        },
        &source,
    )?;
    let model = StaticModel::new(
        model_key,
        root_indices,
        model_nodes,
        primitives,
        materials,
        owned_images,
        &limits,
        location(&source, "model"),
    )?;
    Ok(ImportedAsset::Model(Arc::new(model)))
}

fn check_container_extension(source: &SourceBlob) -> Result<(), AssetImportError> {
    let id = source.source_id().as_str().to_ascii_lowercase();
    if !(id.ends_with(".gltf") || id.ends_with(".glb")) {
        return Err(error(
            source,
            "uri",
            AssetImportErrorCode::Unsupported,
            "daedalus.model accepts .gltf and .glb sources",
        ));
    }
    Ok(())
}

fn validate_glb_if_present(source: &SourceBlob) -> Result<bool, AssetImportError> {
    let bytes = source.bytes();
    if !bytes.starts_with(b"glTF") {
        if source
            .source_id()
            .as_str()
            .to_ascii_lowercase()
            .ends_with(".glb")
        {
            return Err(error(
                source,
                "glb.header.magic",
                AssetImportErrorCode::Invalid,
                "GLB source does not start with the glTF binary magic",
            ));
        }
        return Ok(false);
    }
    if bytes.len() < 12 {
        return Err(error(
            source,
            "glb.header",
            AssetImportErrorCode::Invalid,
            "GLB header is truncated",
        ));
    }
    let version = le_u32(bytes, 4).ok_or_else(|| {
        error(
            source,
            "glb.header.version",
            AssetImportErrorCode::Invalid,
            "GLB version field is truncated",
        )
    })?;
    let declared_length = le_u32(bytes, 8).ok_or_else(|| {
        error(
            source,
            "glb.header.length",
            AssetImportErrorCode::Invalid,
            "GLB length field is truncated",
        )
    })? as usize;
    if version != 2 || declared_length != bytes.len() {
        return Err(error(
            source,
            "glb.header",
            AssetImportErrorCode::Invalid,
            "GLB must use version 2 and its declared length must match the complete source",
        ));
    }
    let mut cursor = 12usize;
    let mut chunk_index = 0usize;
    while cursor < bytes.len() {
        let header_end = cursor
            .checked_add(8)
            .ok_or_else(|| limit_error(source, "glb.chunks", "GLB chunk header offset overflow"))?;
        let header = bytes.get(cursor..header_end).ok_or_else(|| {
            error(
                source,
                format!("glb.chunks[{chunk_index}]"),
                AssetImportErrorCode::Invalid,
                "GLB chunk header is truncated",
            )
        })?;
        let chunk_length = u32::from_le_bytes(header[..4].try_into().map_err(|_| {
            error(
                source,
                format!("glb.chunks[{chunk_index}].length"),
                AssetImportErrorCode::Invalid,
                "GLB chunk length is truncated",
            )
        })?) as usize;
        let chunk_type = u32::from_le_bytes(header[4..8].try_into().map_err(|_| {
            error(
                source,
                format!("glb.chunks[{chunk_index}].type"),
                AssetImportErrorCode::Invalid,
                "GLB chunk type is truncated",
            )
        })?);
        if !chunk_length.is_multiple_of(4) {
            return Err(error(
                source,
                format!("glb.chunks[{chunk_index}].length"),
                AssetImportErrorCode::Invalid,
                "GLB chunk length must be four-byte aligned",
            ));
        }
        let chunk_end = header_end
            .checked_add(chunk_length)
            .ok_or_else(|| limit_error(source, "glb.chunks", "GLB chunk end offset overflow"))?;
        if chunk_end > bytes.len() {
            return Err(error(
                source,
                format!("glb.chunks[{chunk_index}]"),
                AssetImportErrorCode::Invalid,
                "GLB chunk data is truncated",
            ));
        }
        if chunk_index == 0 {
            if chunk_type != 0x4e4f534a {
                return Err(error(
                    source,
                    "glb.chunks[0].type",
                    AssetImportErrorCode::Invalid,
                    "the first GLB chunk must contain JSON",
                ));
            }
            if chunk_length > MAX_GLTF_JSON_CHUNK {
                return Err(limit_error(
                    source,
                    "glb.chunks[0]",
                    "GLB JSON chunk exceeds the configured parser input bound",
                ));
            }
        } else if chunk_index == 1 {
            if chunk_type != 0x004e4942 {
                return Err(error(
                    source,
                    "glb.chunks[1].type",
                    AssetImportErrorCode::Unsupported,
                    "only a GLB BIN chunk may follow the JSON chunk",
                ));
            }
        } else {
            return Err(error(
                source,
                format!("glb.chunks[{chunk_index}]"),
                AssetImportErrorCode::Unsupported,
                "unknown or additional GLB chunks are not supported by gltf2-static-v1",
            ));
        }
        cursor = chunk_end;
        chunk_index += 1;
    }
    if chunk_index == 0 {
        return Err(error(
            source,
            "glb.chunks",
            AssetImportErrorCode::Invalid,
            "GLB is missing its mandatory JSON chunk",
        ));
    }
    Ok(true)
}

fn reject_extensions(gltf: &gltf::Gltf, source: &SourceBlob) -> Result<(), AssetImportError> {
    if let Some(extension) = gltf.extensions_required().next() {
        return Err(error(
            source,
            "extensionsRequired",
            AssetImportErrorCode::Unsupported,
            format!("required glTF extension '{extension}' is unsupported by gltf2-static-v1"),
        ));
    }
    if let Some(extension) = gltf.extensions_used().next() {
        return Err(error(
            source,
            "extensionsUsed",
            AssetImportErrorCode::Unsupported,
            format!("used glTF extension '{extension}' is unsupported by gltf2-static-v1"),
        ));
    }
    Ok(())
}

fn reject_unsupported_top_level(
    gltf: &gltf::Gltf,
    source: &SourceBlob,
) -> Result<(), AssetImportError> {
    if gltf.document.animations().next().is_some() {
        return Err(error(
            source,
            "animations",
            AssetImportErrorCode::Unsupported,
            "glTF animations are outside the static model importer",
        ));
    }
    if gltf.document.skins().next().is_some() {
        return Err(error(
            source,
            "skins",
            AssetImportErrorCode::Unsupported,
            "glTF skins are outside the static model importer",
        ));
    }
    if gltf.document.cameras().next().is_some() {
        return Err(error(
            source,
            "cameras",
            AssetImportErrorCode::Unsupported,
            "glTF cameras are not represented by static model assets",
        ));
    }
    Ok(())
}

fn choose_scene(
    gltf: &gltf::Gltf,
    requested: Option<usize>,
    scene_count: usize,
    source: &SourceBlob,
) -> Result<usize, AssetImportError> {
    if let Some(index) = requested {
        if index >= scene_count {
            return Err(error(
                source,
                "metadata.scene_index",
                AssetImportErrorCode::Invalid,
                "scene_index is outside the glTF scene array",
            ));
        }
        return Ok(index);
    }
    if let Some(scene) = gltf.default_scene() {
        return Ok(scene.index());
    }
    if scene_count == 1 {
        return Ok(0);
    }
    Err(error(
        source,
        "scenes",
        AssetImportErrorCode::Invalid,
        "glTF has multiple scenes and no default; set metadata.scene_index",
    ))
}

fn validate_node_graph<'a>(
    nodes: &[gltf::Node<'a>],
    source: &SourceBlob,
    max_depth: usize,
) -> Result<(Vec<Option<usize>>, Vec<usize>), AssetImportError> {
    let mut parents = Vec::new();
    let mut depths = Vec::new();
    let mut state = Vec::new();
    parents
        .try_reserve_exact(nodes.len())
        .map_err(|_| allocation_error(source, "nodes"))?;
    depths
        .try_reserve_exact(nodes.len())
        .map_err(|_| allocation_error(source, "nodes"))?;
    state
        .try_reserve_exact(nodes.len())
        .map_err(|_| allocation_error(source, "nodes"))?;
    parents.resize(nodes.len(), None);
    depths.resize(nodes.len(), 0usize);
    state.resize(nodes.len(), 0u8);
    for node in nodes {
        let parent_index = node.index();
        for child in node.children() {
            let child_index = child.index();
            if child_index >= nodes.len() {
                return Err(error(
                    source,
                    format!("nodes[{parent_index}].children"),
                    AssetImportErrorCode::Invalid,
                    "node child index is out of range",
                ));
            }
            if parents[child_index].replace(parent_index).is_some() {
                return Err(error(
                    source,
                    format!("nodes[{parent_index}].children"),
                    AssetImportErrorCode::Invalid,
                    "glTF node has duplicate or multiple parents",
                ));
            }
        }
    }

    fn visit(
        index: usize,
        current_depth: usize,
        nodes: &[gltf::Node<'_>],
        states: &mut [u8],
        heights: &mut [usize],
        max_depth: usize,
        source: &SourceBlob,
    ) -> Result<usize, AssetImportError> {
        if current_depth > max_depth {
            return Err(limit_error(
                source,
                format!("nodes[{index}]"),
                "node hierarchy exceeds configured dependency depth",
            ));
        }
        if states[index] == 1 {
            return Err(error(
                source,
                format!("nodes[{index}].children"),
                AssetImportErrorCode::Invalid,
                "glTF node hierarchy contains a cycle",
            ));
        }
        if states[index] == 2 {
            if current_depth
                .checked_add(heights[index].saturating_sub(1))
                .is_none_or(|depth| depth > max_depth)
            {
                return Err(limit_error(
                    source,
                    format!("nodes[{index}]"),
                    "node hierarchy exceeds configured dependency depth",
                ));
            }
            return Ok(heights[index]);
        }
        states[index] = 1;
        let mut depth = 1usize;
        for child in nodes[index].children() {
            let child_depth = visit(
                child.index(),
                current_depth
                    .checked_add(1)
                    .ok_or_else(|| limit_error(source, "nodes", "node hierarchy depth overflow"))?,
                nodes,
                states,
                heights,
                max_depth,
                source,
            )?;
            depth =
                depth.max(child_depth.checked_add(1).ok_or_else(|| {
                    limit_error(source, "nodes", "node hierarchy depth overflow")
                })?);
        }
        if current_depth
            .checked_add(depth.saturating_sub(1))
            .is_none_or(|value| value > max_depth)
        {
            return Err(limit_error(
                source,
                format!("nodes[{index}]"),
                "node hierarchy exceeds configured dependency depth",
            ));
        }
        states[index] = 2;
        heights[index] = depth;
        Ok(depth)
    }
    for (index, parent) in parents.iter().enumerate() {
        if parent.is_none() {
            visit(index, 1, nodes, &mut state, &mut depths, max_depth, source)?;
        }
    }
    // A cycle has no root; visit every remaining component so it is reported instead of omitted.
    let mut index = 0;
    while index < nodes.len() {
        if state[index] == 0 {
            visit(index, 1, nodes, &mut state, &mut depths, max_depth, source)?;
        }
        index += 1;
    }
    Ok((parents, depths))
}

fn validate_scene_roots(
    document: &gltf::Document,
    parents: &[Option<usize>],
    source: &SourceBlob,
) -> Result<(), AssetImportError> {
    let mut seen_in_scene = Vec::new();
    seen_in_scene
        .try_reserve_exact(parents.len())
        .map_err(|_| allocation_error(source, "scenes"))?;
    seen_in_scene.resize(parents.len(), usize::MAX);
    for scene in document.scenes() {
        let scene_index = scene.index();
        for node in scene.nodes() {
            let node_index = node.index();
            if node_index >= parents.len() {
                return Err(error(
                    source,
                    format!("scenes[{scene_index}].nodes"),
                    AssetImportErrorCode::Invalid,
                    "scene root node index is out of range",
                ));
            }
            if parents[node_index].is_some() {
                return Err(error(
                    source,
                    format!("scenes[{scene_index}].nodes"),
                    AssetImportErrorCode::Invalid,
                    "scene root is also another node's child",
                ));
            }
            if seen_in_scene[node_index] == scene_index {
                return Err(error(
                    source,
                    format!("scenes[{scene_index}].nodes"),
                    AssetImportErrorCode::Invalid,
                    "scene contains a duplicate root node",
                ));
            }
            seen_in_scene[node_index] = scene_index;
        }
    }
    Ok(())
}

fn count_mesh_primitives(
    document: &gltf::Document,
    source: &SourceBlob,
    max_primitives: usize,
) -> Result<Vec<usize>, AssetImportError> {
    let mut counts = Vec::new();
    counts
        .try_reserve_exact(document.meshes().count())
        .map_err(|_| allocation_error(source, "meshes"))?;
    for mesh in document.meshes() {
        let count = mesh.primitives().count();
        if count == 0 {
            return Err(error(
                source,
                format!("meshes[{}].primitives", mesh.index()),
                AssetImportErrorCode::Invalid,
                "glTF mesh must contain at least one primitive",
            ));
        }
        counts.push(count);
        if counts
            .iter()
            .try_fold(0usize, |total, value| total.checked_add(*value))
            .is_none_or(|total| total > max_primitives)
        {
            return Err(limit_error(
                source,
                "meshes.primitives",
                "glTF primitive count exceeds the configured catalog limit",
            ));
        }
    }
    Ok(counts)
}

fn count_node_primitive_references(
    nodes: &[gltf::Node<'_>],
    mesh_counts: &[usize],
    source: &SourceBlob,
) -> Result<usize, AssetImportError> {
    let mut total = 0usize;
    for node in nodes {
        if let Some(mesh) = node.mesh() {
            let count = mesh_counts.get(mesh.index()).ok_or_else(|| {
                error(
                    source,
                    format!("nodes[{}].mesh", node.index()),
                    AssetImportErrorCode::Invalid,
                    "node references an out-of-range mesh",
                )
            })?;
            total = total.checked_add(*count).ok_or_else(|| {
                limit_error(
                    source,
                    "nodes.mesh_references",
                    "node primitive reference count overflow",
                )
            })?;
        }
    }
    Ok(total)
}

fn estimate_model_metadata_bytes(
    nodes: usize,
    primitives: usize,
    materials: usize,
    node_primitive_refs: usize,
    roots: usize,
    image_defs: usize,
    owner: &StableId,
) -> Result<u64, AssetImportError> {
    let owner_len = u64::try_from(owner.as_str().len()).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            ImportLocation::default(),
            "owner stable ID length is not representable",
        )
    })?;
    let node_bytes = (nodes as u64)
        .checked_mul(68)
        .and_then(|value| value.checked_add((node_primitive_refs as u64).checked_mul(4)?))
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "model node metadata byte count overflow",
            )
        })?;
    let primitive_bytes = (primitives as u64).checked_mul(40).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            ImportLocation::default(),
            "model primitive metadata byte count overflow",
        )
    })?;
    let material_bytes = (materials as u64)
        .checked_mul(96 + owner_len)
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "model material metadata byte count overflow",
            )
        })?;
    let image_bytes = (image_defs as u64)
        .checked_mul(72 + owner_len)
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "model image metadata byte count overflow",
            )
        })?;
    node_bytes
        .checked_add(primitive_bytes)
        .and_then(|value| value.checked_add(material_bytes))
        .and_then(|value| value.checked_add(image_bytes))
        .and_then(|value| value.checked_add((roots as u64).checked_mul(4)?))
        .and_then(|value| value.checked_add(128))
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "model metadata byte count overflow",
            )
        })
}

fn load_buffers<'a>(
    gltf: &'a gltf::Gltf,
    owner: &SourceBlob,
    context: &mut AssetImportContext<'_>,
) -> Result<Vec<BufferData<'a>>, AssetImportError> {
    let buffer_count = gltf.document.buffers().count();
    let mut buffers = Vec::new();
    buffers
        .try_reserve_exact(buffer_count)
        .map_err(|_| allocation_error(owner, "buffers"))?;
    for buffer in gltf.document.buffers() {
        let index = buffer.index();
        let path = format!("buffers[{index}]");
        context.check_cancelled(location(owner, &path))?;
        let declared_length = buffer.length();
        let storage = match buffer.source() {
            gltf::buffer::Source::Bin => {
                if index != 0 {
                    return Err(error(
                        owner,
                        &path,
                        AssetImportErrorCode::Invalid,
                        "only buffer 0 may use the GLB BIN chunk",
                    ));
                }
                let blob = gltf.blob.as_deref().ok_or_else(|| {
                    error(
                        owner,
                        &path,
                        AssetImportErrorCode::Invalid,
                        "glTF buffer has no URI and the container has no BIN chunk",
                    )
                })?;
                if blob.len() < declared_length || blob.len().saturating_sub(declared_length) > 3 {
                    return Err(error(
                        owner,
                        &path,
                        AssetImportErrorCode::Invalid,
                        "GLB BIN chunk length does not match buffer.byteLength plus at most three padding bytes",
                    ));
                }
                BufferStorage::Glb(blob)
            }
            gltf::buffer::Source::Uri(uri) => {
                if uri.starts_with("data:") {
                    return Err(error(
                        owner,
                        format!("{path}.uri"),
                        AssetImportErrorCode::Unsupported,
                        "data URI buffers are not supported by gltf2-static-v1",
                    ));
                }
                let dependency = context.resolve_uri(
                    owner.source_id(),
                    uri,
                    context.limits().max_source_bytes,
                    location(owner, format!("{path}.uri")),
                )?;
                if dependency.byte_len() < declared_length as u64 {
                    return Err(error(
                        owner,
                        &path,
                        AssetImportErrorCode::Invalid,
                        "external buffer is shorter than its declared buffer.byteLength",
                    ));
                }
                BufferStorage::External(dependency)
            }
        };
        if storage.bytes().len() < declared_length {
            return Err(error(
                owner,
                &path,
                AssetImportErrorCode::Invalid,
                "buffer data is shorter than its declared buffer.byteLength",
            ));
        }
        buffers.push(BufferData {
            storage,
            declared_length,
        });
    }
    Ok(buffers)
}

fn reject_unsupported_primitive(
    primitive: &gltf::mesh::Primitive<'_>,
    source: &SourceBlob,
    path: &str,
) -> Result<(), AssetImportError> {
    if primitive.mode() != gltf::mesh::Mode::Triangles {
        return Err(error(
            source,
            format!("{path}.mode"),
            AssetImportErrorCode::Unsupported,
            "gltf2-static-v1 supports triangle primitives only",
        ));
    }
    if primitive.morph_targets().next().is_some() {
        return Err(error(
            source,
            format!("{path}.targets"),
            AssetImportErrorCode::Unsupported,
            "glTF morph targets are outside the static model importer",
        ));
    }
    let mut has_positions = false;
    let mut has_normals = false;
    for (semantic, _) in primitive.attributes() {
        match semantic {
            gltf::mesh::Semantic::Positions => has_positions = true,
            gltf::mesh::Semantic::Normals => has_normals = true,
            gltf::mesh::Semantic::Tangents | gltf::mesh::Semantic::TexCoords(0) => {}
            gltf::mesh::Semantic::TexCoords(_) => {
                return Err(error(
                    source,
                    format!("{path}.attributes.TEXCOORD_n"),
                    AssetImportErrorCode::Unsupported,
                    "only TEXCOORD_0 is supported",
                ));
            }
            _ => {
                return Err(error(
                    source,
                    format!("{path}.attributes"),
                    AssetImportErrorCode::Unsupported,
                    format!("vertex attribute {semantic:?} is not supported by gltf2-static-v1"),
                ));
            }
        }
    }
    if !has_positions {
        return Err(error(
            source,
            format!("{path}.attributes.POSITION"),
            AssetImportErrorCode::Unsupported,
            "POSITION is required for a static triangle primitive",
        ));
    }
    if !has_normals {
        return Err(error(
            source,
            format!("{path}.attributes.NORMAL"),
            AssetImportErrorCode::Unsupported,
            "NORMAL is required for a static triangle primitive",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct AccessorView<'a> {
    buffer: &'a [u8],
    start: usize,
    count: usize,
    stride: usize,
    element_bytes: usize,
    component_bytes: usize,
    component_type: gltf::accessor::DataType,
    dimensions: gltf::accessor::Dimensions,
    normalized: bool,
}

impl<'a> AccessorView<'a> {
    fn new(
        accessor: gltf::Accessor<'_>,
        buffers: &'a [BufferData<'_>],
        source: &SourceBlob,
        path: &str,
    ) -> Result<Self, AssetImportError> {
        if accessor.sparse().is_some() {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Unsupported,
                "sparse accessors are not supported by gltf2-static-v1",
            ));
        }
        let view = accessor.view().ok_or_else(|| {
            error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor has no buffer view",
            )
        })?;
        let buffer_index = view.buffer().index();
        let buffer = buffers.get(buffer_index).ok_or_else(|| {
            error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor references an out-of-range buffer",
            )
        })?;
        let component_bytes: usize = match accessor.data_type() {
            gltf::accessor::DataType::I8 | gltf::accessor::DataType::U8 => 1,
            gltf::accessor::DataType::I16 | gltf::accessor::DataType::U16 => 2,
            gltf::accessor::DataType::U32 | gltf::accessor::DataType::F32 => 4,
        };
        let component_count = dimensions_components(accessor.dimensions()).ok_or_else(|| {
            error(
                source,
                path,
                AssetImportErrorCode::Unsupported,
                "matrix accessors are not supported for static vertex attributes",
            )
        })?;
        let element_bytes = component_bytes
            .checked_mul(component_count)
            .ok_or_else(|| limit_error(source, path, "accessor element byte size overflow"))?;
        let stride = view.stride().unwrap_or(element_bytes);
        if stride < element_bytes
            || !stride.is_multiple_of(component_bytes)
            || !accessor.offset().is_multiple_of(component_bytes)
        {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor byte offset or stride is invalid for its component type",
            ));
        }
        let last_element_offset = if accessor.count() == 0 {
            accessor.offset()
        } else {
            accessor
                .count()
                .checked_sub(1)
                .and_then(|last| last.checked_mul(stride))
                .and_then(|last| accessor.offset().checked_add(last))
                .and_then(|last| last.checked_add(element_bytes))
                .ok_or_else(|| limit_error(source, path, "accessor byte range overflow"))?
        };
        if last_element_offset > view.length() {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor byte range exceeds its bufferView",
            ));
        }
        let view_end = view
            .offset()
            .checked_add(view.length())
            .ok_or_else(|| limit_error(source, path, "bufferView end offset overflow"))?;
        if view_end > buffer.declared_length {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "bufferView exceeds its declared buffer length",
            ));
        }
        let start = view
            .offset()
            .checked_add(accessor.offset())
            .ok_or_else(|| limit_error(source, path, "accessor start offset overflow"))?;
        if !start.is_multiple_of(component_bytes) {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor absolute byte offset is misaligned",
            ));
        }
        let end = start
            .checked_add(
                last_element_offset
                    .checked_sub(accessor.offset())
                    .ok_or_else(|| limit_error(source, path, "accessor range underflow"))?,
            )
            .ok_or_else(|| limit_error(source, path, "accessor end offset overflow"))?;
        let bytes = buffer.bytes();
        if end > bytes.len() {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "accessor byte range exceeds its source buffer",
            ));
        }
        Ok(Self {
            buffer: bytes,
            start,
            count: accessor.count(),
            stride,
            element_bytes,
            component_bytes,
            component_type: accessor.data_type(),
            dimensions: accessor.dimensions(),
            normalized: accessor.normalized(),
        })
    }

    fn element(self, index: usize) -> &'a [u8] {
        let start = self.start + index * self.stride;
        &self.buffer[start..start + self.element_bytes]
    }

    fn raw_range_bytes(self) -> Result<u64, AssetImportError> {
        u64::try_from(self.count.checked_mul(self.element_bytes).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "accessor consumed byte count overflow",
            )
        })?)
        .map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "accessor consumed byte count is not representable",
            )
        })
    }
}

fn dimensions_components(value: gltf::accessor::Dimensions) -> Option<usize> {
    match value {
        gltf::accessor::Dimensions::Scalar => Some(1),
        gltf::accessor::Dimensions::Vec2 => Some(2),
        gltf::accessor::Dimensions::Vec3 => Some(3),
        gltf::accessor::Dimensions::Vec4 => Some(4),
        _ => None,
    }
}

fn import_geometry(
    primitive: &gltf::mesh::Primitive<'_>,
    buffers: &[BufferData<'_>],
    context: &mut AssetImportContext<'_>,
    source: &SourceBlob,
    path: &str,
    geometry_by_key: &mut BTreeMap<[u8; 32], Arc<GeometryResource>>,
) -> Result<Arc<GeometryResource>, AssetImportError> {
    let position_accessor = primitive
        .get(&gltf::mesh::Semantic::Positions)
        .ok_or_else(|| {
            error(
                source,
                format!("{path}.attributes.POSITION"),
                AssetImportErrorCode::Unsupported,
                "POSITION is required",
            )
        })?;
    let normal_accessor = primitive
        .get(&gltf::mesh::Semantic::Normals)
        .ok_or_else(|| {
            error(
                source,
                format!("{path}.attributes.NORMAL"),
                AssetImportErrorCode::Unsupported,
                "NORMAL is required",
            )
        })?;
    let uv_accessor = primitive.get(&gltf::mesh::Semantic::TexCoords(0));
    let positions = AccessorView::new(
        position_accessor,
        buffers,
        source,
        &format!("{path}.attributes.POSITION"),
    )?;
    let normals = AccessorView::new(
        normal_accessor,
        buffers,
        source,
        &format!("{path}.attributes.NORMAL"),
    )?;
    let uvs = uv_accessor
        .map(|accessor| {
            AccessorView::new(
                accessor,
                buffers,
                source,
                &format!("{path}.attributes.TEXCOORD_0"),
            )
        })
        .transpose()?;
    validate_vec3_accessor(
        positions,
        true,
        source,
        &format!("{path}.attributes.POSITION"),
    )?;
    validate_vec3_accessor(normals, false, source, &format!("{path}.attributes.NORMAL"))?;
    if positions.count == 0
        || positions.count != normals.count
        || uvs.is_some_and(|uv| uv.count != positions.count)
    {
        return Err(error(
            source,
            format!("{path}.attributes"),
            AssetImportErrorCode::Invalid,
            "POSITION, NORMAL, and optional TEXCOORD_0 accessor counts must match and be nonzero",
        ));
    }
    if positions.count > context.limits().max_geometry_vertices {
        return Err(limit_error(
            source,
            format!("{path}.attributes.POSITION"),
            "primitive vertex count exceeds the configured unique geometry limit",
        ));
    }
    let index_accessor = primitive.indices();
    let index_view = index_accessor
        .map(|accessor| AccessorView::new(accessor, buffers, source, &format!("{path}.indices")))
        .transpose()?;
    if let Some(index_view) = index_view {
        validate_index_accessor(index_view, source, &format!("{path}.indices"))?;
    }
    let index_count = index_view.map_or(positions.count, |indices| indices.count);
    if index_count == 0 || index_count % 3 != 0 {
        return Err(error(
            source,
            format!("{path}.indices"),
            AssetImportErrorCode::Invalid,
            "triangle primitive index/vertex count must be a nonzero multiple of three",
        ));
    }
    if index_count > context.limits().max_geometry_indices {
        return Err(limit_error(
            source,
            format!("{path}.indices"),
            "primitive index count exceeds the configured unique geometry limit",
        ));
    }

    let accessors = GeometryAccessors {
        positions,
        normals,
        uvs,
        indices: index_view,
        index_count,
    };
    let key = hash_geometry_payload(accessors, source, path, context)?;
    let digest = key.digest();
    if let Some(existing) = geometry_by_key.get(&digest) {
        return Ok(existing.clone());
    }
    if let Some(existing) = find_existing_geometry(context, digest) {
        geometry_by_key.insert(digest, existing.clone());
        return Ok(existing);
    }

    let vertex_count = positions.count as u64;
    let index_count_u64 = index_count as u64;
    let cooked_bytes = vertex_count
        .checked_mul(if uvs.is_some() { 32 } else { 24 })
        .and_then(|bytes| bytes.checked_add(index_count_u64.checked_mul(4)?))
        .ok_or_else(|| limit_error(source, path, "geometry cooked byte count overflow"))?;
    context.check_budget(
        BudgetKind::GeometryVertices,
        vertex_count,
        location(source, path),
    )?;
    context.check_budget(
        BudgetKind::GeometryIndices,
        index_count_u64,
        location(source, path),
    )?;
    context.check_budget(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, path),
    )?;
    context.account(
        BudgetKind::GeometryVertices,
        vertex_count,
        location(source, path),
    )?;
    context.account(
        BudgetKind::GeometryIndices,
        index_count_u64,
        location(source, path),
    )?;
    context.account(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, path),
    )?;

    let cache_key = CacheKey::new(CacheNamespace::Cook, digest);
    let cache_payload_bytes =
        geometry_cache_bytes(vertex_count, index_count_u64, uvs.is_some(), source, path)?;
    let mut payload_from_cache = None;
    let cache_read = context.read_cache(cache_key, CacheObjectKind::Cook, cache_payload_bytes)?;
    match cache_read {
        CacheRead::Hit(lease) => {
            let import_limits = *context.limits();
            let conversion_scratch = context.reserve_temporary(
                BudgetKind::ParserScratchBytes,
                cooked_bytes,
                location(source, format!("{path}.cache")),
            )?;
            match decode_geometry_cache(
                lease.payload(),
                GeometryCacheLayout {
                    key,
                    vertices: positions.count,
                    indices: index_count,
                    has_uv: uvs.is_some(),
                },
                &import_limits,
                source,
                path,
                context,
            ) {
                Ok(geometry) => {
                    payload_from_cache = Some(geometry);
                    drop(lease);
                }
                Err(cache_error) if cache_error.code() == AssetImportErrorCode::Cache => {
                    drop(lease);
                    context.invalidate_cache(
                        cache_key,
                        CacheObjectKind::Cook,
                        location(source, format!("{path}.cache")),
                    )?;
                }
                Err(error) => return Err(error),
            }
            drop(conversion_scratch);
        }
        CacheRead::Miss => {}
        CacheRead::Corrupt(_) => {
            context.invalidate_cache(
                cache_key,
                CacheObjectKind::Cook,
                location(source, format!("{path}.cache")),
            )?;
        }
    }

    let geometry = if let Some(geometry) = payload_from_cache {
        geometry
    } else {
        let conversion_scratch = context.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            cooked_bytes,
            location(source, format!("{path}.geometry")),
        )?;
        let values = decode_geometry_values(accessors, source, path, context)?;
        let geometry = Arc::new(GeometryResource::imported(
            values.positions,
            values.normals,
            values.uv0,
            values.indices,
            key,
            context.limits(),
            location(source, path),
        )?);
        drop(conversion_scratch);
        let cache_scratch_bytes = cache_payload_bytes
            .checked_mul(2)
            .ok_or_else(|| limit_error(source, path, "geometry cache scratch size overflow"))?;
        let _cache_scratch = context.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            cache_scratch_bytes,
            location(source, format!("{path}.cache")),
        )?;
        let cache_payload =
            encode_geometry_cache(&geometry, context, source, path, cache_payload_bytes)?;
        context.check_cancelled(location(source, format!("{path}.cache")))?;
        let cache_payload: Arc<[u8]> = Arc::from(cache_payload);
        context.publish_cache(
            cache_key,
            CacheObjectKind::Cook,
            cache_payload,
            location(source, format!("{path}.cache")),
        )?;
        context.record_cook(AssetKind::Model);
        geometry
    };

    geometry_by_key.insert(digest, geometry.clone());
    Ok(geometry)
}

fn validate_vec3_accessor(
    accessor: AccessorView<'_>,
    position: bool,
    source: &SourceBlob,
    path: &str,
) -> Result<(), AssetImportError> {
    if accessor.dimensions != gltf::accessor::Dimensions::Vec3 {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Invalid,
            "POSITION and NORMAL must use VEC3 accessors",
        ));
    }
    let valid = accessor.component_type == gltf::accessor::DataType::F32 && !accessor.normalized;
    if !valid {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Unsupported,
            if position {
                "POSITION must be unnormalized FLOAT VEC3"
            } else {
                "NORMAL must be unnormalized FLOAT VEC3"
            },
        ));
    }
    Ok(())
}

fn validate_uv_accessor(
    accessor: AccessorView<'_>,
    source: &SourceBlob,
    path: &str,
) -> Result<(), AssetImportError> {
    let valid = accessor.dimensions == gltf::accessor::Dimensions::Vec2
        && ((accessor.component_type == gltf::accessor::DataType::F32 && !accessor.normalized)
            || (matches!(
                accessor.component_type,
                gltf::accessor::DataType::U8 | gltf::accessor::DataType::U16
            ) && accessor.normalized));
    if !valid {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Unsupported,
            "TEXCOORD_0 must be FLOAT VEC2 or normalized UNSIGNED_BYTE/UNSIGNED_SHORT VEC2",
        ));
    }
    Ok(())
}

fn validate_index_accessor(
    accessor: AccessorView<'_>,
    source: &SourceBlob,
    path: &str,
) -> Result<(), AssetImportError> {
    if accessor.dimensions != gltf::accessor::Dimensions::Scalar
        || accessor.normalized
        || !matches!(
            accessor.component_type,
            gltf::accessor::DataType::U8
                | gltf::accessor::DataType::U16
                | gltf::accessor::DataType::U32
        )
    {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Unsupported,
            "indices must use an unnormalized UNSIGNED_BYTE/SHORT/INT SCALAR accessor",
        ));
    }
    if accessor.stride != accessor.element_bytes {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Invalid,
            "index accessors cannot use an interleaved byte stride",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct GeometryAccessors<'a> {
    positions: AccessorView<'a>,
    normals: AccessorView<'a>,
    uvs: Option<AccessorView<'a>>,
    indices: Option<AccessorView<'a>>,
    index_count: usize,
}

struct GeometryValues {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uv0: Option<Vec<[f32; 2]>>,
    indices: Vec<u32>,
}

#[derive(Clone, Copy)]
struct GeometryCacheLayout {
    key: LeafCookKey,
    vertices: usize,
    indices: usize,
    has_uv: bool,
}

fn hash_geometry_payload(
    accessors: GeometryAccessors<'_>,
    source: &SourceBlob,
    path: &str,
    context: &mut AssetImportContext<'_>,
) -> Result<LeafCookKey, AssetImportError> {
    let GeometryAccessors {
        positions,
        normals,
        uvs,
        indices,
        index_count,
    } = accessors;
    if let Some(uv) = uvs {
        validate_uv_accessor(uv, source, &format!("{path}.attributes.TEXCOORD_0"))?;
    }
    let output_bytes = (positions.count as u64)
        .checked_mul(if uvs.is_some() { 32 } else { 24 })
        .and_then(|value| value.checked_add((index_count as u64).checked_mul(4)?))
        .ok_or_else(|| limit_error(source, path, "canonical geometry payload size overflow"))?;
    let mut settings = [0u8; 18];
    settings[0] = 1; // Canonical local-space layout version.
    settings[1] = u8::from(uvs.is_some());
    settings[2..10].copy_from_slice(&(positions.count as u64).to_le_bytes());
    settings[10..18].copy_from_slice(&(index_count as u64).to_le_bytes());
    let mut hasher = leaf_hasher(
        "daedalus.geometry.gltf2-static-v1",
        IMPORTER_VERSION,
        &settings,
        output_bytes,
    );
    for index in 0..positions.count {
        for value in read_vec3(
            positions,
            index,
            source,
            &format!("{path}.attributes.POSITION"),
        )? {
            hash_f32(&mut hasher, value);
        }
        for value in read_vec3(normals, index, source, &format!("{path}.attributes.NORMAL"))? {
            hash_f32(&mut hasher, value);
        }
        if let Some(uvs) = uvs {
            for value in read_vec2(uvs, index, source, &format!("{path}.attributes.TEXCOORD_0"))? {
                hash_f32(&mut hasher, value);
            }
        }
        if index % 256 == 0 {
            context.check_cancelled(location(source, format!("{path}.attributes[{index}]")))?;
        }
    }
    match indices {
        Some(indices) => {
            for index in 0..indices.count {
                let value = read_index(indices, index, source, &format!("{path}.indices"))?;
                if value as usize >= positions.count {
                    return Err(error(
                        source,
                        format!("{path}.indices[{index}]"),
                        AssetImportErrorCode::Invalid,
                        "index references a vertex outside POSITION",
                    ));
                }
                hasher.update(value.to_le_bytes());
                if index % 1024 == 0 {
                    context
                        .check_cancelled(location(source, format!("{path}.indices[{index}]")))?;
                }
            }
        }
        None => {
            for index in 0..index_count {
                let value = u32::try_from(index).map_err(|_| {
                    limit_error(
                        source,
                        path,
                        "generated nonindexed index is not representable",
                    )
                })?;
                hasher.update(value.to_le_bytes());
                if index % 1024 == 0 {
                    context
                        .check_cancelled(location(source, format!("{path}.indices[{index}]")))?;
                }
            }
        }
    }
    let mut source_bytes = positions
        .raw_range_bytes()?
        .checked_add(normals.raw_range_bytes()?)
        .ok_or_else(|| limit_error(source, path, "attribute range byte count overflow"))?;
    if let Some(uvs) = uvs {
        source_bytes = source_bytes
            .checked_add(uvs.raw_range_bytes()?)
            .ok_or_else(|| limit_error(source, path, "attribute range byte count overflow"))?;
    }
    if let Some(indices) = indices {
        source_bytes = source_bytes
            .checked_add(indices.raw_range_bytes()?)
            .ok_or_else(|| limit_error(source, path, "index range byte count overflow"))?;
    }
    context.record_range_hash_bytes(source_bytes);
    Ok(LeafCookKey::from_digest(hasher.finalize().into()))
}

fn leaf_hasher(domain: &str, version: u32, settings: &[u8], payload_length: u64) -> Sha256 {
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain.as_bytes());
    let version_bytes = version.to_le_bytes();
    hasher.update((version_bytes.len() as u64).to_le_bytes());
    hasher.update(version_bytes);
    hasher.update((settings.len() as u64).to_le_bytes());
    hasher.update(settings);
    hasher.update(payload_length.to_le_bytes());
    // The final part's length is followed by its canonical bytes, so the caller appends them.
    hasher
}

fn hash_f32(hasher: &mut Sha256, value: f32) {
    hasher.update(value.to_bits().to_le_bytes());
}

fn read_vec3(
    accessor: AccessorView<'_>,
    index: usize,
    source: &SourceBlob,
    path: &str,
) -> Result<[f32; 3], AssetImportError> {
    let element = accessor.element(index);
    let mut result = [0.0; 3];
    for (component, output) in result.iter_mut().enumerate() {
        *output = float_component(
            &element
                [component * accessor.component_bytes..(component + 1) * accessor.component_bytes],
            accessor.component_type,
            accessor.normalized,
            source,
            path,
        )?;
        if !output.is_finite() {
            return Err(error(
                source,
                format!("{path}[{index}]"),
                AssetImportErrorCode::Invalid,
                "vertex attribute contains a non-finite value",
            ));
        }
    }
    Ok(result)
}

fn read_vec2(
    accessor: AccessorView<'_>,
    index: usize,
    source: &SourceBlob,
    path: &str,
) -> Result<[f32; 2], AssetImportError> {
    let element = accessor.element(index);
    let mut result = [0.0; 2];
    for (component, output) in result.iter_mut().enumerate() {
        *output = float_component(
            &element
                [component * accessor.component_bytes..(component + 1) * accessor.component_bytes],
            accessor.component_type,
            accessor.normalized,
            source,
            path,
        )?;
        if !output.is_finite() {
            return Err(error(
                source,
                format!("{path}[{index}]"),
                AssetImportErrorCode::Invalid,
                "vertex attribute contains a non-finite value",
            ));
        }
    }
    Ok(result)
}

fn float_component(
    bytes: &[u8],
    component: gltf::accessor::DataType,
    normalized: bool,
    source: &SourceBlob,
    path: &str,
) -> Result<f32, AssetImportError> {
    let invalid = || {
        error(
            source,
            path,
            AssetImportErrorCode::Invalid,
            "accessor component byte length is invalid",
        )
    };
    let value = match component {
        gltf::accessor::DataType::F32 => {
            f32::from_le_bytes(bytes.try_into().map_err(|_| invalid())?)
        }
        gltf::accessor::DataType::I8 => {
            let value = *bytes.first().ok_or_else(invalid)? as i8;
            if normalized {
                (value as f32 / 127.0).max(-1.0)
            } else {
                value as f32
            }
        }
        gltf::accessor::DataType::U8 => {
            let value = *bytes.first().ok_or_else(invalid)?;
            if normalized {
                value as f32 / 255.0
            } else {
                value as f32
            }
        }
        gltf::accessor::DataType::I16 => {
            let value = i16::from_le_bytes(bytes.try_into().map_err(|_| invalid())?);
            if normalized {
                (value as f32 / 32767.0).max(-1.0)
            } else {
                value as f32
            }
        }
        gltf::accessor::DataType::U16 => {
            let value = u16::from_le_bytes(bytes.try_into().map_err(|_| invalid())?);
            if normalized {
                value as f32 / 65535.0
            } else {
                value as f32
            }
        }
        gltf::accessor::DataType::U32 => {
            return Err(error(
                source,
                path,
                AssetImportErrorCode::Unsupported,
                "UNSIGNED_INT is not supported for floating vertex attributes",
            ));
        }
    };
    Ok(value)
}

fn read_index(
    accessor: AccessorView<'_>,
    index: usize,
    source: &SourceBlob,
    path: &str,
) -> Result<u32, AssetImportError> {
    let bytes = accessor.element(index);
    match accessor.component_type {
        gltf::accessor::DataType::U8 => bytes.first().copied().map(u32::from).ok_or_else(|| {
            error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "index accessor is truncated",
            )
        }),
        gltf::accessor::DataType::U16 => Ok(u16::from_le_bytes(bytes.try_into().map_err(|_| {
            error(
                source,
                path,
                AssetImportErrorCode::Invalid,
                "index accessor is truncated",
            )
        })?) as u32),
        gltf::accessor::DataType::U32 => {
            Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Invalid,
                    "index accessor is truncated",
                )
            })?))
        }
        _ => Err(error(
            source,
            path,
            AssetImportErrorCode::Unsupported,
            "index component type must be unsigned integer",
        )),
    }
}

fn decode_geometry_values(
    accessors: GeometryAccessors<'_>,
    source: &SourceBlob,
    path: &str,
    context: &AssetImportContext<'_>,
) -> Result<GeometryValues, AssetImportError> {
    let GeometryAccessors {
        positions,
        normals,
        uvs,
        indices,
        index_count,
    } = accessors;
    let count = positions.count;
    let mut position_values = Vec::new();
    let mut normal_values = Vec::new();
    let mut uv_values = uvs.map(|_| Vec::new());
    let mut index_values = Vec::new();
    position_values
        .try_reserve_exact(count)
        .map_err(|_| allocation_error(source, path))?;
    normal_values
        .try_reserve_exact(count)
        .map_err(|_| allocation_error(source, path))?;
    if let Some(values) = &mut uv_values {
        values
            .try_reserve_exact(count)
            .map_err(|_| allocation_error(source, path))?;
    }
    index_values
        .try_reserve_exact(index_count)
        .map_err(|_| allocation_error(source, path))?;
    for index in 0..count {
        context.check_cancelled(location(source, format!("{path}.attributes[{index}]")))?;
        position_values.push(read_vec3(
            positions,
            index,
            source,
            &format!("{path}.attributes.POSITION"),
        )?);
        normal_values.push(read_vec3(
            normals,
            index,
            source,
            &format!("{path}.attributes.NORMAL"),
        )?);
        if let (Some(accessor), Some(values)) = (uvs, &mut uv_values) {
            values.push(read_vec2(
                accessor,
                index,
                source,
                &format!("{path}.attributes.TEXCOORD_0"),
            )?);
        }
    }
    for index in 0..index_count {
        if index % 1024 == 0 {
            context.check_cancelled(location(source, format!("{path}.indices[{index}]")))?;
        }
        let value = match indices {
            Some(accessor) => read_index(accessor, index, source, &format!("{path}.indices"))?,
            None => u32::try_from(index).map_err(|_| {
                limit_error(
                    source,
                    path,
                    "generated nonindexed index is not representable",
                )
            })?,
        };
        if value as usize >= count {
            return Err(error(
                source,
                format!("{path}.indices[{index}]"),
                AssetImportErrorCode::Invalid,
                "index references a vertex outside POSITION",
            ));
        }
        index_values.push(value);
    }
    Ok(GeometryValues {
        positions: position_values,
        normals: normal_values,
        uv0: uv_values,
        indices: index_values,
    })
}

fn find_existing_geometry(
    context: &AssetImportContext<'_>,
    digest: [u8; 32],
) -> Option<Arc<GeometryResource>> {
    context
        .catalog_assets()
        .values()
        .find_map(|asset| match asset {
            ImportedAsset::Model(model) => model.primitives().iter().find_map(|primitive| {
                (primitive.geometry().key().digest() == digest)
                    .then(|| primitive.geometry().clone())
            }),
            _ => None,
        })
}

fn geometry_cache_bytes(
    vertices: u64,
    indices: u64,
    has_uv: bool,
    source: &SourceBlob,
    path: &str,
) -> Result<u64, AssetImportError> {
    vertices
        .checked_mul(if has_uv { 32 } else { 24 })
        .and_then(|value| value.checked_add(indices.checked_mul(4)?))
        .and_then(|value| value.checked_add(GEOMETRY_CACHE_HEADER))
        .ok_or_else(|| limit_error(source, path, "geometry cache payload size overflow"))
}

fn encode_geometry_cache(
    geometry: &GeometryResource,
    context: &AssetImportContext<'_>,
    source: &SourceBlob,
    path: &str,
    expected_bytes: u64,
) -> Result<Vec<u8>, AssetImportError> {
    let vertex_count = geometry.vertex_count();
    let index_count = geometry.index_count();
    let has_uv = geometry.uv0().is_some();
    let bytes = geometry_cache_bytes(
        vertex_count as u64,
        index_count as u64,
        has_uv,
        source,
        path,
    )?;
    if bytes != expected_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Internal,
            location(source, format!("{path}.cache")),
            "geometry cache size changed after its budget check",
        ));
    }
    let capacity = usize::try_from(bytes)
        .map_err(|_| limit_error(source, path, "geometry cache payload is not representable"))?;
    let mut writer =
        BinaryWriter::with_capacity(capacity).map_err(|_| allocation_error(source, path))?;
    writer
        .write(GEOMETRY_CACHE_MAGIC)
        .map_err(|_| allocation_error(source, path))?;
    writer
        .u32(
            u32::try_from(vertex_count).map_err(|_| {
                limit_error(source, path, "geometry vertex count is not representable")
            })?,
        )
        .map_err(|_| allocation_error(source, path))?;
    writer
        .u32(
            u32::try_from(index_count).map_err(|_| {
                limit_error(source, path, "geometry index count is not representable")
            })?,
        )
        .map_err(|_| allocation_error(source, path))?;
    writer
        .u8(u8::from(has_uv))
        .map_err(|_| allocation_error(source, path))?;
    for (index, value) in geometry.positions().iter().flatten().enumerate() {
        if index % 4096 == 0 {
            context
                .check_cancelled(location(source, format!("{path}.cache.positions[{index}]")))?;
        }
        writer
            .f32_bits(*value)
            .map_err(|_| allocation_error(source, path))?;
    }
    for (index, value) in geometry.normals().iter().flatten().enumerate() {
        if index % 4096 == 0 {
            context.check_cancelled(location(source, format!("{path}.cache.normals[{index}]")))?;
        }
        writer
            .f32_bits(*value)
            .map_err(|_| allocation_error(source, path))?;
    }
    if let Some(uvs) = geometry.uv0() {
        for (index, value) in uvs.iter().flatten().enumerate() {
            if index % 4096 == 0 {
                context.check_cancelled(location(source, format!("{path}.cache.uvs[{index}]")))?;
            }
            writer
                .f32_bits(*value)
                .map_err(|_| allocation_error(source, path))?;
        }
    }
    for (index, value) in geometry.indices().iter().enumerate() {
        if index % 4096 == 0 {
            context.check_cancelled(location(source, format!("{path}.cache.indices[{index}]")))?;
        }
        writer
            .u32(*value)
            .map_err(|_| allocation_error(source, path))?;
    }
    Ok(writer.into_bytes())
}

fn decode_geometry_cache(
    payload: &[u8],
    layout: GeometryCacheLayout,
    limits: &ImportLimits,
    source: &SourceBlob,
    path: &str,
    context: &mut AssetImportContext<'_>,
) -> Result<Arc<GeometryResource>, AssetImportError> {
    let GeometryCacheLayout {
        key: expected_key,
        vertices: expected_vertices,
        indices: expected_indices,
        has_uv,
    } = layout;
    let mut reader = BinaryReader::new(payload, GEOMETRY_CACHE_MAGIC).map_err(|_| {
        error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache header is invalid",
        )
    })?;
    let vertex_count = reader.u32().map_err(|_| {
        error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache vertex count is truncated",
        )
    })? as usize;
    let index_count = reader.u32().map_err(|_| {
        error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache index count is truncated",
        )
    })? as usize;
    let cached_uv = reader.u8().map_err(|_| {
        error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache layout is truncated",
        )
    })?;
    if vertex_count != expected_vertices
        || index_count != expected_indices
        || cached_uv != u8::from(has_uv)
    {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache metadata does not match the computed leaf key",
        ));
    }
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uv0 = has_uv.then(Vec::new);
    let mut indices = Vec::new();
    positions
        .try_reserve_exact(vertex_count)
        .map_err(|_| allocation_error(source, path))?;
    normals
        .try_reserve_exact(vertex_count)
        .map_err(|_| allocation_error(source, path))?;
    if let Some(uvs) = &mut uv0 {
        uvs.try_reserve_exact(vertex_count)
            .map_err(|_| allocation_error(source, path))?;
    }
    indices
        .try_reserve_exact(index_count)
        .map_err(|_| allocation_error(source, path))?;
    for index in 0..vertex_count {
        if index % 256 == 0 {
            context.check_cancelled(location(source, format!("{path}.cache.vertices[{index}]")))?;
        }
        positions.push([
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache positions are truncated",
                )
            })?,
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache positions are truncated",
                )
            })?,
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache positions are truncated",
                )
            })?,
        ]);
    }
    for index in 0..vertex_count {
        if index % 256 == 0 {
            context.check_cancelled(location(source, format!("{path}.cache.vertices[{index}]")))?;
        }
        normals.push([
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache normals are truncated",
                )
            })?,
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache normals are truncated",
                )
            })?,
            reader.f32_bits().map_err(|_| {
                error(
                    source,
                    path,
                    AssetImportErrorCode::Cache,
                    "geometry cache normals are truncated",
                )
            })?,
        ]);
    }
    if let Some(uvs) = &mut uv0 {
        for index in 0..vertex_count {
            if index % 256 == 0 {
                context.check_cancelled(location(source, format!("{path}.cache.uvs[{index}]")))?;
            }
            uvs.push([
                reader.f32_bits().map_err(|_| {
                    error(
                        source,
                        path,
                        AssetImportErrorCode::Cache,
                        "geometry cache UVs are truncated",
                    )
                })?,
                reader.f32_bits().map_err(|_| {
                    error(
                        source,
                        path,
                        AssetImportErrorCode::Cache,
                        "geometry cache UVs are truncated",
                    )
                })?,
            ]);
        }
    }
    for index in 0..index_count {
        if index % 1024 == 0 {
            context.check_cancelled(location(source, format!("{path}.cache.indices[{index}]")))?;
        }
        indices.push(reader.u32().map_err(|_| {
            error(
                source,
                path,
                AssetImportErrorCode::Cache,
                "geometry cache indices are truncated",
            )
        })?);
    }
    reader.finish().map_err(|_| {
        error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache has trailing bytes",
        )
    })?;
    let computed_key =
        compute_key_from_values(&positions, &normals, uv0.as_deref(), &indices, source, path)?;
    if computed_key != expected_key {
        return Err(error(
            source,
            path,
            AssetImportErrorCode::Cache,
            "geometry cache payload does not match its expected semantic key",
        ));
    }
    let geometry = GeometryResource::imported(
        positions,
        normals,
        uv0,
        indices,
        expected_key,
        limits,
        location(source, path),
    )
    .map_err(|import_error| {
        if import_error.code() == AssetImportErrorCode::Invalid {
            error(
                source,
                format!("{path}.cache"),
                AssetImportErrorCode::Cache,
                "geometry cache payload violates geometry invariants",
            )
        } else {
            import_error
        }
    })?;
    Ok(Arc::new(geometry))
}

fn compute_key_from_values(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uvs: Option<&[[f32; 2]]>,
    indices: &[u32],
    source: &SourceBlob,
    path: &str,
) -> Result<LeafCookKey, AssetImportError> {
    let bytes = (positions.len() as u64)
        .checked_mul(if uvs.is_some() { 32 } else { 24 })
        .and_then(|value| value.checked_add((indices.len() as u64).checked_mul(4)?))
        .ok_or_else(|| limit_error(source, path, "cached geometry canonical size overflow"))?;
    let mut settings = [0u8; 18];
    settings[0] = 1;
    settings[1] = u8::from(uvs.is_some());
    settings[2..10].copy_from_slice(&(positions.len() as u64).to_le_bytes());
    settings[10..18].copy_from_slice(&(indices.len() as u64).to_le_bytes());
    let mut hasher = leaf_hasher(
        "daedalus.geometry.gltf2-static-v1",
        IMPORTER_VERSION,
        &settings,
        bytes,
    );
    for (index, (position, normal)) in positions.iter().zip(normals).enumerate() {
        for value in position.iter().chain(normal) {
            hash_f32(&mut hasher, *value);
        }
        if let Some(uvs) = uvs {
            for value in &uvs[index] {
                hash_f32(&mut hasher, *value);
            }
        }
    }
    for index in indices {
        hasher.update(index.to_le_bytes());
    }
    Ok(LeafCookKey::from_digest(hasher.finalize().into()))
}

struct ImportedMaterials {
    materials: Vec<ModelMaterial>,
    owned_images: Vec<ModelOwnedImage>,
    material_textures: Vec<Option<ModelTextureRef>>,
}

fn import_materials(
    gltf: &gltf::Gltf,
    buffers: &[BufferData<'_>],
    descriptor: &AssetDescriptor,
    source: &SourceBlob,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedMaterials, AssetImportError> {
    let material_count = gltf.document.materials().count();
    let image_count = gltf.document.images().count();
    let mut used_images = BTreeMap::<usize, ModelImageIdentity>::new();
    let mut materials = Vec::new();
    let mut material_textures = Vec::new();
    materials
        .try_reserve_exact(material_count)
        .map_err(|_| allocation_error(source, "materials"))?;
    material_textures
        .try_reserve_exact(material_count)
        .map_err(|_| allocation_error(source, "materials"))?;
    for material in gltf.document.materials() {
        let index = material.index().unwrap_or(0);
        let path = format!("materials[{index}]");
        context.check_cancelled(location(source, &path))?;
        if material.alpha_mode() != gltf::material::AlphaMode::Opaque {
            return Err(error(
                source,
                format!("{path}.alphaMode"),
                AssetImportErrorCode::Unsupported,
                "MASK and BLEND alpha modes are not supported by gltf2-static-v1",
            ));
        }
        if material.double_sided() {
            return Err(error(
                source,
                format!("{path}.doubleSided"),
                AssetImportErrorCode::Unsupported,
                "double-sided materials are not supported by gltf2-static-v1",
            ));
        }
        if material.normal_texture().is_some()
            || material.occlusion_texture().is_some()
            || material.emissive_texture().is_some()
        {
            return Err(error(
                source,
                &path,
                AssetImportErrorCode::Unsupported,
                "normal, occlusion, and emissive texture roles are not supported by gltf2-static-v1",
            ));
        }
        if material.emissive_factor() != [0.0, 0.0, 0.0] {
            return Err(error(
                source,
                format!("{path}.emissiveFactor"),
                AssetImportErrorCode::Unsupported,
                "nonzero emissive factors are not represented by static PBR materials",
            ));
        }
        let pbr = material.pbr_metallic_roughness();
        if pbr.metallic_roughness_texture().is_some() {
            return Err(error(
                source,
                format!("{path}.pbrMetallicRoughness.metallicRoughnessTexture"),
                AssetImportErrorCode::Unsupported,
                "metallic-roughness textures are not supported by gltf2-static-v1",
            ));
        }
        let base_color_factor = pbr.base_color_factor();
        let metallic_factor = pbr.metallic_factor();
        let roughness_factor = pbr.roughness_factor();
        if base_color_factor
            .iter()
            .chain([&metallic_factor, &roughness_factor])
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(error(
                source,
                &path,
                AssetImportErrorCode::Invalid,
                "PBR material factors must be finite and within [0, 1]",
            ));
        }
        let texture_ref = if let Some(info) = pbr.base_color_texture() {
            if info.tex_coord() != 0 {
                return Err(error(
                    source,
                    format!("{path}.pbrMetallicRoughness.baseColorTexture.texCoord"),
                    AssetImportErrorCode::Unsupported,
                    "base-color texture coordinate sets other than TEXCOORD_0 are not supported",
                ));
            }
            let texture = info.texture();
            let image = texture.source();
            let image_index = image.index();
            if image_index >= image_count {
                return Err(error(
                    source,
                    format!("{path}.pbrMetallicRoughness.baseColorTexture"),
                    AssetImportErrorCode::Invalid,
                    "base-color texture references an out-of-range image",
                ));
            }
            let image_index_u32 = u32::try_from(image_index).map_err(|_| {
                limit_error(
                    source,
                    format!("images[{image_index}]"),
                    "image index is not representable",
                )
            })?;
            let identity = ModelImageIdentity::new(descriptor.id.clone(), image_index_u32);
            used_images
                .entry(image_index)
                .or_insert_with(|| identity.clone());
            Some(ModelTextureRef::new(
                identity,
                gltf_sampler(texture.sampler()),
            ))
        } else {
            None
        };
        material_textures.push(texture_ref.clone());
        materials.push(ModelMaterial {
            base_color_factor,
            metallic_factor,
            roughness_factor,
            base_color_texture: texture_ref,
        });
    }

    let image_settings = EffectiveSettings {
        role: ImageRole::BaseColorSrgb,
        mip_policy: MipPolicy::LinearLightBoxV1,
        // Sampling belongs to ModelTextureRef and may differ for two textures sharing one image.
        sampler: ImageSampler::default(),
    };
    let mut owned_images = Vec::new();
    owned_images
        .try_reserve_exact(used_images.len())
        .map_err(|_| allocation_error(source, "images"))?;
    for (image_index, identity) in used_images {
        context.check_cancelled(location(source, format!("images[{image_index}]")))?;
        let image = gltf.document.images().nth(image_index).ok_or_else(|| {
            error(
                source,
                format!("images[{image_index}]"),
                AssetImportErrorCode::Invalid,
                "image index is out of range",
            )
        })?;
        let mut embedded_copy_scratch = None;
        let image_source = match image.source() {
            gltf::image::Source::Uri { uri, mime_type } => {
                validate_png_mime(mime_type, Some(uri), source, image_index)?;
                if uri.starts_with("data:") {
                    return Err(error(
                        source,
                        format!("images[{image_index}].uri"),
                        AssetImportErrorCode::Unsupported,
                        "data URI images are not supported by gltf2-static-v1",
                    ));
                }
                context.resolve_uri(
                    source.source_id(),
                    uri,
                    context.limits().max_source_bytes,
                    location(source, format!("images[{image_index}]")),
                )?
            }
            gltf::image::Source::View { view, mime_type } => {
                validate_png_mime(Some(mime_type), None, source, image_index)?;
                let buffer_index = view.buffer().index();
                let buffer = buffers.get(buffer_index).ok_or_else(|| {
                    error(
                        source,
                        format!("images[{image_index}].bufferView"),
                        AssetImportErrorCode::Invalid,
                        "image bufferView references an out-of-range buffer",
                    )
                })?;
                let start = view.offset();
                let end = start.checked_add(view.length()).ok_or_else(|| {
                    limit_error(
                        source,
                        format!("images[{image_index}].bufferView"),
                        "embedded image range overflow",
                    )
                })?;
                if end > buffer.declared_length || end > buffer.bytes().len() {
                    return Err(error(
                        source,
                        format!("images[{image_index}].bufferView"),
                        AssetImportErrorCode::Invalid,
                        "embedded image bufferView exceeds its declared buffer",
                    ));
                }
                if view.length() as u64 > context.limits().max_source_bytes {
                    return Err(limit_error(
                        source,
                        format!("images[{image_index}]"),
                        "embedded PNG exceeds configured per-file limit",
                    ));
                }
                embedded_copy_scratch = Some(context.reserve_temporary(
                    BudgetKind::ParserScratchBytes,
                    view.length() as u64,
                    location(source, format!("images[{image_index}].bufferView")),
                )?);
                let encoded: Arc<[u8]> = Arc::from(&buffer.bytes()[start..end]);
                Arc::new(SourceBlob::new(source.source_id().clone(), encoded))
            }
        };
        let cooked = super::image::cook_png_at(
            &image_source,
            image_settings,
            context,
            &format!("images[{image_index}]"),
            PngColorPolicy::GltfMaterialEncoded,
        )?;
        drop(embedded_copy_scratch);
        owned_images.push(ModelOwnedImage::new(identity, cooked));
    }
    Ok(ImportedMaterials {
        materials,
        owned_images,
        material_textures,
    })
}

fn validate_png_mime(
    mime: Option<&str>,
    uri: Option<&str>,
    owner: &SourceBlob,
    image_index: usize,
) -> Result<(), AssetImportError> {
    if let Some(mime) = mime {
        if mime.eq_ignore_ascii_case("image/png") {
            return Ok(());
        }
        return Err(error(
            owner,
            format!("images[{image_index}].mimeType"),
            AssetImportErrorCode::Unsupported,
            format!(
                "image MIME type '{mime}' is unsupported; only opaque PNG base-color images are accepted"
            ),
        ));
    }
    if let Some(uri) = uri {
        let extension = uri.rsplit('/').next().unwrap_or(uri).to_ascii_lowercase();
        if extension.ends_with(".png") {
            return Ok(());
        }
        if extension.ends_with(".jpg") || extension.ends_with(".jpeg") {
            return Err(error(
                owner,
                format!("images[{image_index}].uri"),
                AssetImportErrorCode::Unsupported,
                "JPEG images are not supported by gltf2-static-v1",
            ));
        }
    }
    Err(error(
        owner,
        format!("images[{image_index}].mimeType"),
        AssetImportErrorCode::Unsupported,
        "base-color image MIME type must identify PNG",
    ))
}

fn gltf_sampler(sampler: gltf::texture::Sampler<'_>) -> ImageSampler {
    let (min_filter, mipmap_filter) = match sampler.min_filter() {
        None | Some(gltf::texture::MinFilter::LinearMipmapLinear) => {
            (TextureFilter::Linear, MipmapFilter::Linear)
        }
        Some(gltf::texture::MinFilter::Nearest) => (TextureFilter::Nearest, MipmapFilter::None),
        Some(gltf::texture::MinFilter::Linear) => (TextureFilter::Linear, MipmapFilter::None),
        Some(gltf::texture::MinFilter::NearestMipmapNearest) => {
            (TextureFilter::Nearest, MipmapFilter::Nearest)
        }
        Some(gltf::texture::MinFilter::LinearMipmapNearest) => {
            (TextureFilter::Linear, MipmapFilter::Nearest)
        }
        Some(gltf::texture::MinFilter::NearestMipmapLinear) => {
            (TextureFilter::Nearest, MipmapFilter::Linear)
        }
    };
    let mag_filter = match sampler.mag_filter() {
        None | Some(gltf::texture::MagFilter::Linear) => TextureFilter::Linear,
        Some(gltf::texture::MagFilter::Nearest) => TextureFilter::Nearest,
    };
    let wrap = |mode| match mode {
        gltf::texture::WrappingMode::ClampToEdge => TextureWrap::ClampToEdge,
        gltf::texture::WrappingMode::MirroredRepeat => TextureWrap::MirroredRepeat,
        gltf::texture::WrappingMode::Repeat => TextureWrap::Repeat,
    };
    ImageSampler {
        min_filter,
        mag_filter,
        mipmap_filter,
        wrap_u: wrap(sampler.wrap_s()),
        wrap_v: wrap(sampler.wrap_t()),
    }
}

struct ModelKeyParts<'a> {
    scene_index: usize,
    roots: &'a [u32],
    nodes: &'a [ModelNode],
    primitives: &'a [ModelPrimitive],
    materials: &'a [ModelMaterial],
    images: &'a [ModelOwnedImage],
    owner: &'a StableId,
}

fn make_model_key(
    context: &AssetImportContext<'_>,
    parts: ModelKeyParts<'_>,
    source: &SourceBlob,
) -> Result<LeafCookKey, AssetImportError> {
    let ModelKeyParts {
        scene_index,
        roots,
        nodes,
        primitives,
        materials,
        images,
        owner,
    } = parts;
    let node_reference_bytes = nodes
        .iter()
        .try_fold(0usize, |total, node| {
            total.checked_add(node.primitive_indices().len().checked_mul(4)?)
        })
        .ok_or_else(|| {
            limit_error(
                source,
                "model.key",
                "model node reference key size overflow",
            )
        })?;
    let capacity = 128usize
        .checked_add(
            roots
                .len()
                .checked_mul(4)
                .ok_or_else(|| limit_error(source, "model.key", "model key size overflow"))?,
        )
        .and_then(|value| value.checked_add(nodes.len().checked_mul(72)?))
        .and_then(|value| value.checked_add(node_reference_bytes))
        .and_then(|value| value.checked_add(primitives.len().checked_mul(40)?))
        .and_then(|value| {
            value.checked_add(materials.len().checked_mul(64 + owner.as_str().len())?)
        })
        .and_then(|value| value.checked_add(images.len().checked_mul(48 + owner.as_str().len())?))
        .ok_or_else(|| limit_error(source, "model.key", "model key capacity overflow"))?;
    let _scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        u64::try_from(capacity).map_err(|_| {
            limit_error(
                source,
                "model.key",
                "model key capacity is not representable",
            )
        })?,
        location(source, "model.key"),
    )?;
    let mut writer =
        BinaryWriter::with_capacity(capacity).map_err(|_| allocation_error(source, "model.key"))?;
    writer
        .u64(scene_index as u64)
        .map_err(|_| allocation_error(source, "model.key"))?;
    writer
        .u32(u32::try_from(roots.len()).map_err(|_| {
            limit_error(source, "model.key", "scene root count is not representable")
        })?)
        .map_err(|_| allocation_error(source, "model.key"))?;
    for root in roots {
        writer
            .u32(*root)
            .map_err(|_| allocation_error(source, "model.key"))?;
    }
    writer
        .u32(
            u32::try_from(nodes.len())
                .map_err(|_| limit_error(source, "model.key", "node count is not representable"))?,
        )
        .map_err(|_| allocation_error(source, "model.key"))?;
    for node in nodes {
        writer
            .u32(node.parent().unwrap_or(u32::MAX))
            .map_err(|_| allocation_error(source, "model.key"))?;
        for value in node.local_transform() {
            writer
                .f32_bits(*value)
                .map_err(|_| allocation_error(source, "model.key"))?;
        }
        writer
            .u32(u32::try_from(node.primitive_indices().len()).map_err(|_| {
                limit_error(
                    source,
                    "model.key",
                    "node primitive reference count is not representable",
                )
            })?)
            .map_err(|_| allocation_error(source, "model.key"))?;
        for index in node.primitive_indices() {
            writer
                .u32(*index)
                .map_err(|_| allocation_error(source, "model.key"))?;
        }
    }
    writer
        .u32(u32::try_from(primitives.len()).map_err(|_| {
            limit_error(source, "model.key", "primitive count is not representable")
        })?)
        .map_err(|_| allocation_error(source, "model.key"))?;
    for primitive in primitives {
        writer
            .u32(primitive.primitive_ordinal())
            .map_err(|_| allocation_error(source, "model.key"))?;
        writer
            .u32(primitive.material_index().unwrap_or(u32::MAX))
            .map_err(|_| allocation_error(source, "model.key"))?;
        writer
            .write(&primitive.geometry().key().digest())
            .map_err(|_| allocation_error(source, "model.key"))?;
    }
    writer
        .u32(
            u32::try_from(materials.len()).map_err(|_| {
                limit_error(source, "model.key", "material count is not representable")
            })?,
        )
        .map_err(|_| allocation_error(source, "model.key"))?;
    for material in materials {
        for value in material.base_color_factor {
            writer
                .f32_bits(value)
                .map_err(|_| allocation_error(source, "model.key"))?;
        }
        writer
            .f32_bits(material.metallic_factor)
            .map_err(|_| allocation_error(source, "model.key"))?;
        writer
            .f32_bits(material.roughness_factor)
            .map_err(|_| allocation_error(source, "model.key"))?;
        if let Some(texture) = &material.base_color_texture {
            writer
                .u8(1)
                .map_err(|_| allocation_error(source, "model.key"))?;
            write_stable_id(&mut writer, texture.owner_image().owner())
                .map_err(|_| allocation_error(source, "model.key"))?;
            writer
                .u32(texture.owner_image().image_index())
                .map_err(|_| allocation_error(source, "model.key"))?;
            write_sampler(&mut writer, texture.sampler())
                .map_err(|_| allocation_error(source, "model.key"))?;
        } else {
            writer
                .u8(0)
                .map_err(|_| allocation_error(source, "model.key"))?;
        }
    }
    writer
        .u32(u32::try_from(images.len()).map_err(|_| {
            limit_error(
                source,
                "model.key",
                "model image count is not representable",
            )
        })?)
        .map_err(|_| allocation_error(source, "model.key"))?;
    for image in images {
        write_stable_id(&mut writer, image.identity().owner())
            .map_err(|_| allocation_error(source, "model.key"))?;
        writer
            .u32(image.identity().image_index())
            .map_err(|_| allocation_error(source, "model.key"))?;
        writer
            .write(&image.asset().key().digest())
            .map_err(|_| allocation_error(source, "model.key"))?;
    }
    Ok(context.leaf_key(
        "daedalus.model.gltf2-static-v1",
        IMPORTER_VERSION,
        owner.as_str().as_bytes(),
        &writer.into_bytes(),
    ))
}

fn write_stable_id(writer: &mut BinaryWriter, id: &StableId) -> Result<(), BinaryError> {
    let bytes = id.as_str().as_bytes();
    writer.u32(u32::try_from(bytes.len()).map_err(|_| BinaryError::LengthOverflow)?)?;
    writer.write(bytes)
}

fn write_sampler(writer: &mut BinaryWriter, sampler: ImageSampler) -> Result<(), BinaryError> {
    let min = match sampler.min_filter {
        TextureFilter::Nearest => 0,
        TextureFilter::Linear => 1,
    };
    let mag = match sampler.mag_filter {
        TextureFilter::Nearest => 0,
        TextureFilter::Linear => 1,
    };
    let mip = match sampler.mipmap_filter {
        MipmapFilter::None => 0,
        MipmapFilter::Nearest => 1,
        MipmapFilter::Linear => 2,
    };
    let wrap = |value| match value {
        TextureWrap::ClampToEdge => 0,
        TextureWrap::MirroredRepeat => 1,
        TextureWrap::Repeat => 2,
    };
    writer.u8(min)?;
    writer.u8(mag)?;
    writer.u8(mip)?;
    writer.u8(wrap(sampler.wrap_u))?;
    writer.u8(wrap(sampler.wrap_v))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn error(
    source: &SourceBlob,
    path: impl Into<String>,
    code: AssetImportErrorCode,
    message: impl Into<String>,
) -> AssetImportError {
    let source_id = source.source_id().to_string();
    AssetImportError::new(
        code,
        ImportLocation::new(&source_id, path).with_dependency_chain([source_id]),
        message,
    )
}

fn limit_error(
    source: &SourceBlob,
    path: impl Into<String>,
    message: impl Into<String>,
) -> AssetImportError {
    error(source, path, AssetImportErrorCode::LimitExceeded, message)
}

fn allocation_error(source: &SourceBlob, path: impl Into<String>) -> AssetImportError {
    error(
        source,
        path,
        AssetImportErrorCode::LimitExceeded,
        "bounded glTF import allocation could not be reserved",
    )
}

#[cfg(test)]
#[path = "gltf_static_tests.rs"]
mod tests;
