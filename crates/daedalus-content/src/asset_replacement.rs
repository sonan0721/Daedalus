//! Bounded authored-topology compatibility checks for prepared asset generations.
//!
//! This compares already-loaded projects only. It performs no source reads, parsing, cooking,
//! hashing, or runtime mutation.

use std::fmt;
use std::mem::size_of;

use crate::project::PreparedProject;
use crate::scene::{AssetDescriptor, ComponentDescriptor, SceneEntity, StableId};

const MAX_COMPARISON_INDEX_BYTES: usize = 4 * 1024 * 1024;

/// The first authored difference that prevents an asset-only replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetReplacementMismatch {
    Manifest,
    SceneSchema,
    AssetSet,
    AssetKind,
    AssetMetadata,
    EntitySet,
    EntityName,
    EntityParent,
    ComponentSet,
    ComponentType,
    ComponentAssetRefs,
    ComponentData,
    ComparisonCapacity,
}

/// A bounded diagnostic for a rejected authored-topology comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetReplacementCompatibilityError {
    kind: AssetReplacementMismatch,
    entity_id: Option<StableId>,
    component_id: Option<StableId>,
    asset_id: Option<StableId>,
}

impl AssetReplacementCompatibilityError {
    fn new(
        kind: AssetReplacementMismatch,
        entity_id: Option<&StableId>,
        component_id: Option<&StableId>,
        asset_id: Option<&StableId>,
    ) -> Self {
        Self {
            kind,
            entity_id: entity_id.cloned(),
            component_id: component_id.cloned(),
            asset_id: asset_id.cloned(),
        }
    }

    /// Returns the mismatch category, in the comparator's deterministic first-error order.
    pub fn kind(&self) -> AssetReplacementMismatch {
        self.kind
    }

    /// Returns the owning entity for entity- or component-level mismatches.
    pub fn entity_id(&self) -> Option<&StableId> {
        self.entity_id.as_ref()
    }

    /// Returns the component associated with a component-level mismatch.
    pub fn component_id(&self) -> Option<&StableId> {
        self.component_id.as_ref()
    }

    /// Returns the asset associated with an asset-level mismatch.
    pub fn asset_id(&self) -> Option<&StableId> {
        self.asset_id.as_ref()
    }
}

impl fmt::Display for AssetReplacementCompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let description = match self.kind {
            AssetReplacementMismatch::Manifest => "project manifest changed",
            AssetReplacementMismatch::SceneSchema => "scene schema changed",
            AssetReplacementMismatch::AssetSet => "project asset IDs changed",
            AssetReplacementMismatch::AssetKind => "asset kind changed",
            AssetReplacementMismatch::AssetMetadata => "asset metadata changed",
            AssetReplacementMismatch::EntitySet => "scene entity IDs changed",
            AssetReplacementMismatch::EntityName => "entity name changed",
            AssetReplacementMismatch::EntityParent => "entity parent changed",
            AssetReplacementMismatch::ComponentSet => "entity component IDs changed",
            AssetReplacementMismatch::ComponentType => "component type changed",
            AssetReplacementMismatch::ComponentAssetRefs => "component asset references changed",
            AssetReplacementMismatch::ComponentData => "component data changed",
            AssetReplacementMismatch::ComparisonCapacity => {
                "compatibility comparison exceeded its index budget"
            }
        };
        f.write_str(description)?;
        if let Some(id) = &self.asset_id {
            write!(f, " (asset '{id}')")?;
        }
        if let Some(id) = &self.entity_id {
            write!(f, " (entity '{id}')")?;
        }
        if let Some(id) = &self.component_id {
            write!(f, " (component '{id}')")?;
        }
        Ok(())
    }
}

impl std::error::Error for AssetReplacementCompatibilityError {}

/// Checks whether a loaded candidate changes only asset sources/content while preserving the
/// active project's authored entity and component topology.
///
/// Asset URI, content hash, imported payload, and source identity may change. Manifest and asset
/// metadata may not. Scene revision and the order of entity, component, and asset arrays are
/// ignored; every other authored field in the frozen compatibility contract is compared.
pub fn check_asset_replacement_compatibility(
    active: &PreparedProject,
    candidate: &PreparedProject,
) -> Result<(), AssetReplacementCompatibilityError> {
    check_with_index_limit(active, candidate, MAX_COMPARISON_INDEX_BYTES)
}

fn check_with_index_limit(
    active: &PreparedProject,
    candidate: &PreparedProject,
    max_index_bytes: usize,
) -> Result<(), AssetReplacementCompatibilityError> {
    let active_scene = active.scene();
    let candidate_scene = candidate.scene();
    if active.manifest() != candidate.manifest() {
        return Err(mismatch(AssetReplacementMismatch::Manifest));
    }
    if active_scene.schema_version != candidate_scene.schema_version {
        return Err(mismatch(AssetReplacementMismatch::SceneSchema));
    }

    let mut budget = IndexBudget::new(max_index_bytes);
    let active_assets = sorted_index(active_scene.assets.as_slice(), &mut budget, asset_id)?;
    let candidate_assets = sorted_index(candidate_scene.assets.as_slice(), &mut budget, asset_id)?;
    if let Some(id) = first_set_difference(&active_assets, &candidate_assets, asset_id) {
        return Err(mismatch_with_asset(AssetReplacementMismatch::AssetSet, id));
    }
    for (active_asset, candidate_asset) in active_assets.iter().zip(&candidate_assets) {
        if active_asset.kind != candidate_asset.kind {
            return Err(mismatch_with_asset(
                AssetReplacementMismatch::AssetKind,
                &active_asset.id,
            ));
        }
        if active_asset.metadata != candidate_asset.metadata {
            return Err(mismatch_with_asset(
                AssetReplacementMismatch::AssetMetadata,
                &active_asset.id,
            ));
        }
    }

    let active_entities = sorted_index(active_scene.entities.as_slice(), &mut budget, entity_id)?;
    let candidate_entities =
        sorted_index(candidate_scene.entities.as_slice(), &mut budget, entity_id)?;
    if let Some(id) = first_set_difference(&active_entities, &candidate_entities, entity_id) {
        return Err(mismatch_with_entity(
            AssetReplacementMismatch::EntitySet,
            id,
        ));
    }

    for (active_entity, candidate_entity) in active_entities.iter().zip(&candidate_entities) {
        if active_entity.name != candidate_entity.name {
            return Err(mismatch_with_entity(
                AssetReplacementMismatch::EntityName,
                &active_entity.id,
            ));
        }
        if active_entity.parent != candidate_entity.parent {
            return Err(mismatch_with_entity(
                AssetReplacementMismatch::EntityParent,
                &active_entity.id,
            ));
        }

        let component_bytes_before = budget.used();
        let active_components = sorted_index(
            active_entity.components.as_slice(),
            &mut budget,
            component_id,
        )?;
        let candidate_components = sorted_index(
            candidate_entity.components.as_slice(),
            &mut budget,
            component_id,
        )?;
        if let Some(id) =
            first_set_difference(&active_components, &candidate_components, component_id)
        {
            return Err(mismatch_with_component(
                AssetReplacementMismatch::ComponentSet,
                &active_entity.id,
                id,
            ));
        }
        for (active_component, candidate_component) in
            active_components.iter().zip(&candidate_components)
        {
            if active_component.type_id != candidate_component.type_id {
                return Err(mismatch_with_component(
                    AssetReplacementMismatch::ComponentType,
                    &active_entity.id,
                    &active_component.id,
                ));
            }
            if active_component.asset_refs != candidate_component.asset_refs {
                return Err(mismatch_with_component(
                    AssetReplacementMismatch::ComponentAssetRefs,
                    &active_entity.id,
                    &active_component.id,
                ));
            }
            if active_component.data != candidate_component.data {
                return Err(mismatch_with_component(
                    AssetReplacementMismatch::ComponentData,
                    &active_entity.id,
                    &active_component.id,
                ));
            }
        }
        let component_bytes = budget.used() - component_bytes_before;
        drop(candidate_components);
        drop(active_components);
        budget.release(component_bytes);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct IndexBudget {
    max_bytes: usize,
    used_bytes: usize,
}

impl IndexBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            used_bytes: 0,
        }
    }

    fn claim<T>(&mut self, count: usize) -> Result<usize, AssetReplacementCompatibilityError> {
        let bytes = count
            .checked_mul(size_of::<&T>())
            .ok_or_else(capacity_error)?;
        let next = self
            .used_bytes
            .checked_add(bytes)
            .ok_or_else(capacity_error)?;
        if next > self.max_bytes {
            return Err(capacity_error());
        }
        self.used_bytes = next;
        Ok(bytes)
    }

    fn release(&mut self, bytes: usize) {
        self.used_bytes = self
            .used_bytes
            .checked_sub(bytes)
            .expect("released comparison index bytes must be reserved");
    }

    fn used(&self) -> usize {
        self.used_bytes
    }
}

fn sorted_index<'a, T, F>(
    items: &'a [T],
    budget: &mut IndexBudget,
    id: F,
) -> Result<Vec<&'a T>, AssetReplacementCompatibilityError>
where
    F: for<'item> Fn(&'item T) -> &'item StableId,
{
    budget.claim::<T>(items.len())?;
    let mut index = Vec::new();
    if index.try_reserve_exact(items.len()).is_err() {
        budget.release(
            items
                .len()
                .checked_mul(size_of::<&T>())
                .ok_or_else(capacity_error)?,
        );
        return Err(capacity_error());
    }
    index.extend(items.iter());
    index.sort_unstable_by(|left, right| id(left).cmp(id(right)));
    Ok(index)
}

fn first_set_difference<'a, T, F>(left: &[&'a T], right: &[&'a T], id: F) -> Option<&'a StableId>
where
    F: for<'item> Fn(&'item T) -> &'item StableId,
{
    let (mut left_index, mut right_index) = (0, 0);
    while left_index < left.len() && right_index < right.len() {
        match id(left[left_index]).cmp(id(right[right_index])) {
            std::cmp::Ordering::Less => return Some(id(left[left_index])),
            std::cmp::Ordering::Greater => return Some(id(right[right_index])),
            std::cmp::Ordering::Equal => {
                left_index += 1;
                right_index += 1;
            }
        }
    }
    left.get(left_index)
        .map(|item| id(item))
        .or_else(|| right.get(right_index).map(|item| id(item)))
}

fn asset_id(asset: &AssetDescriptor) -> &StableId {
    &asset.id
}

fn entity_id(entity: &SceneEntity) -> &StableId {
    &entity.id
}

fn component_id(component: &ComponentDescriptor) -> &StableId {
    &component.id
}

fn mismatch(kind: AssetReplacementMismatch) -> AssetReplacementCompatibilityError {
    AssetReplacementCompatibilityError::new(kind, None, None, None)
}

fn mismatch_with_asset(
    kind: AssetReplacementMismatch,
    asset_id: &StableId,
) -> AssetReplacementCompatibilityError {
    AssetReplacementCompatibilityError::new(kind, None, None, Some(asset_id))
}

fn mismatch_with_entity(
    kind: AssetReplacementMismatch,
    entity_id: &StableId,
) -> AssetReplacementCompatibilityError {
    AssetReplacementCompatibilityError::new(kind, Some(entity_id), None, None)
}

fn mismatch_with_component(
    kind: AssetReplacementMismatch,
    entity_id: &StableId,
    component_id: &StableId,
) -> AssetReplacementCompatibilityError {
    AssetReplacementCompatibilityError::new(kind, Some(entity_id), Some(component_id), None)
}

fn capacity_error() -> AssetReplacementCompatibilityError {
    mismatch(AssetReplacementMismatch::ComparisonCapacity)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use serde_json::{Value, json};

    use super::*;
    use crate::assets::{ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache};
    use crate::project::{ProjectLimits, prepare_project_from_bytes, prepare_project_with_assets};

    fn manifest_value(name: &str) -> Value {
        json!({
            "schema_version": 1,
            "name": name,
            "scene": "scene.json"
        })
    }

    fn scene_value(entities: Value, assets: Value, revision: u64) -> Value {
        json!({
            "schema_version": 1,
            "revision": revision,
            "entities": entities,
            "assets": assets,
        })
    }

    fn prepare(
        manifest: Value,
        scene: Value,
        sources: BTreeMap<String, Vec<u8>>,
    ) -> PreparedProject {
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let scene_bytes = serde_json::to_vec(&scene).unwrap();
        let mut resolver = ByteMapSourceResolver::from_bytes(sources).unwrap();
        let mut cache = MemoryCookCache::default();
        prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap()
    }

    fn empty_project() -> PreparedProject {
        prepare(
            manifest_value("compatibility"),
            scene_value(json!([]), json!([]), 0),
            BTreeMap::new(),
        )
    }

    fn png(pixel: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixel).unwrap();
        }
        bytes
    }

    fn image_descriptor(id: &str, uri: &str, metadata: Value) -> Value {
        json!({ "id": id, "kind": "daedalus.image", "uri": uri, "metadata": metadata })
    }

    fn png_project(uri: &str, pixel: [u8; 4], metadata: Value) -> PreparedProject {
        prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([]),
                json!([image_descriptor("asset:image", uri, metadata)]),
                0,
            ),
            BTreeMap::from([(uri.to_owned(), png(pixel))]),
        )
    }

    fn sprite_project(image_uri: &str, pixel: [u8; 4]) -> PreparedProject {
        let scene = scene_value(
            json!([
                {
                    "id": "entity:camera",
                    "components": [{
                        "id": "component:camera",
                        "type_id": "daedalus.camera_2d",
                        "asset_refs": [],
                        "data": {"vertical_size": 10.0, "near": 0.1, "far": 100.0}
                    }]
                },
                {
                    "id": "entity:sprite",
                    "components": [{
                        "id": "component:sprite",
                        "type_id": "daedalus.sprite_renderer",
                        "asset_refs": ["asset:atlas"],
                        "data": {"atlas": "asset:atlas", "region": "region:hero"}
                    }]
                }
            ]),
            json!([
                image_descriptor(
                    "asset:image",
                    image_uri,
                    json!({
                        "schema_version": 1,
                        "role": "sprite_atlas_srgb",
                        "mip_policy": "none",
                        "sampler": {
                            "min_filter": "nearest",
                            "mag_filter": "nearest",
                            "mipmap_filter": "none"
                        }
                    })
                ),
                {
                    "id": "asset:atlas",
                    "kind": "daedalus.sprite_atlas",
                    "uri": "sprites/sheet.atlas.json",
                    "metadata": {"schema_version": 1}
                }
            ]),
            0,
        );
        let atlas = serde_json::to_vec(&json!({
            "schema_version": 1,
            "image_asset": "asset:image",
            "filter_policy": "nearest_no_mip_v1",
            "regions": [{
                "id": "region:hero",
                "rect": [0, 0, 1, 1],
                "pivot": [0.5, 0.5],
                "pixels_per_unit": 16.0
            }]
        }))
        .unwrap();
        prepare(
            manifest_value("compatibility"),
            scene,
            BTreeMap::from([
                (image_uri.to_owned(), png(pixel)),
                ("sprites/sheet.atlas.json".to_owned(), atlas),
            ]),
        )
    }

    fn model_sources() -> BTreeMap<String, Vec<u8>> {
        let mut binary = Vec::new();
        for value in [
            0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // POSITION
            0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // NORMAL
        ] {
            binary.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::to_vec(&json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1}}]}],
            "buffers": [{"uri": "triangle.bin", "byteLength": binary.len()}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": 36},
                {"buffer": 0, "byteOffset": 36, "byteLength": 36}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0]},
                {"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3"}
            ]
        }))
        .unwrap();
        BTreeMap::from([
            ("models/triangle.gltf".to_owned(), gltf),
            ("models/triangle.bin".to_owned(), binary),
        ])
    }

    fn two_model_project(model_ref: &str) -> PreparedProject {
        prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([{
                    "id": "entity:hero",
                    "components": [{
                        "id": "component:model",
                        "type_id": "daedalus.model_renderer",
                        "asset_refs": [model_ref],
                        "data": {"model": model_ref}
                    }]
                }]),
                json!([
                    {"id": "asset:model-a", "kind": "daedalus.model", "uri": "models/triangle.gltf", "metadata": {}},
                    {"id": "asset:model-b", "kind": "daedalus.model", "uri": "models/triangle.gltf", "metadata": {}}
                ]),
                0,
            ),
            model_sources(),
        )
    }

    fn transform_component(id: &str, x: f32) -> Value {
        json!({
            "id": id,
            "type_id": "daedalus.transform",
            "asset_refs": [],
            "data": {"translation": [x, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0]}
        })
    }

    fn entity(id: &str, name: Option<&str>, parent: Option<&str>, components: Value) -> Value {
        let mut value = json!({"id": id, "components": components});
        if let Some(name) = name {
            value["name"] = json!(name);
        }
        if let Some(parent) = parent {
            value["parent"] = json!(parent);
        }
        value
    }

    fn simple_project(entities: Value) -> PreparedProject {
        prepare(
            manifest_value("compatibility"),
            scene_value(entities, json!([]), 0),
            BTreeMap::new(),
        )
    }

    fn legacy_mesh_project(first_vertex_x: f32) -> PreparedProject {
        let manifest = serde_json::to_vec(&manifest_value("compatibility")).unwrap();
        let scene = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 4,
            "entities": [{
                "id": "entity:triangle",
                "name": "Triangle",
                "components": [{
                    "id": "component:renderer",
                    "type_id": "daedalus.mesh_renderer",
                    "asset_refs": ["asset:triangle"],
                    "data": {"mesh": "asset:triangle"}
                }]
            }],
            "assets": [{
                "id": "asset:triangle",
                "kind": "daedalus.mesh",
                "uri": "assets/triangle.json",
                "metadata": {}
            }]
        }))
        .unwrap();
        let mesh = serde_json::to_vec(&json!({
            "schema_version": 1,
            "vertices": [
                {"position": [first_vertex_x, 0.0, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [1.0, 0.0, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [0.0, 1.0, 0.0], "normal": [0.0, 0.0, 1.0]}
            ],
            "indices": [0, 1, 2]
        }))
        .unwrap();
        let sources = BTreeMap::from([("assets/triangle.json".to_owned(), mesh)]);
        prepare_project_from_bytes(&manifest, &scene, &sources, ProjectLimits::default()).unwrap()
    }

    fn assert_kind(
        active: &PreparedProject,
        candidate: &PreparedProject,
        expected: AssetReplacementMismatch,
    ) -> AssetReplacementCompatibilityError {
        let error = check_asset_replacement_compatibility(active, candidate).unwrap_err();
        assert_eq!(error.kind(), expected, "{error}");
        error
    }

    #[test]
    fn unchanged_revision_and_array_reordering_preserve_compatibility() {
        let left = prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([
                    entity(
                        "entity:a",
                        Some("A"),
                        None,
                        json!([
                            transform_component("component:a", 0.0),
                            {
                                "id": "component:camera",
                                "type_id": "daedalus.camera_2d",
                                "asset_refs": [],
                                "data": {"vertical_size": 10.0, "near": 0.1, "far": 100.0}
                            }
                        ])
                    ),
                    entity(
                        "entity:b",
                        Some("B"),
                        None,
                        json!([transform_component("component:b", 1.0)])
                    )
                ]),
                json!([
                    image_descriptor("asset:a", "a.png", json!({})),
                    image_descriptor("asset:b", "b.png", json!({}))
                ]),
                2,
            ),
            BTreeMap::from([
                ("a.png".to_owned(), png([1, 2, 3, 255])),
                ("b.png".to_owned(), png([4, 5, 6, 255])),
            ]),
        );
        let right = prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([
                    entity(
                        "entity:b",
                        Some("B"),
                        None,
                        json!([transform_component("component:b", 1.0)])
                    ),
                    entity(
                        "entity:a",
                        Some("A"),
                        None,
                        json!([
                            {
                                "id": "component:camera",
                                "type_id": "daedalus.camera_2d",
                                "asset_refs": [],
                                "data": {"vertical_size": 10.0, "near": 0.1, "far": 100.0}
                            },
                            transform_component("component:a", 0.0)
                        ])
                    )
                ]),
                json!([
                    image_descriptor("asset:b", "b.png", json!({})),
                    image_descriptor("asset:a", "a.png", json!({}))
                ]),
                99,
            ),
            BTreeMap::from([
                ("a.png".to_owned(), png([1, 2, 3, 255])),
                ("b.png".to_owned(), png([4, 5, 6, 255])),
            ]),
        );
        check_asset_replacement_compatibility(&left, &right).unwrap();
    }

    #[test]
    fn changed_png_payload_and_uri_keep_same_sprite_binding_compatible() {
        let active = sprite_project("sprites/old.png", [255, 0, 0, 255]);
        let candidate = sprite_project("sprites/new.png", [0, 255, 0, 255]);
        assert_eq!(active.entities().len(), candidate.entities().len());
        assert_ne!(active.files(), candidate.files());
        assert_ne!(active.digest(), candidate.digest());
        assert_eq!(active.scene().entities, candidate.scene().entities);
        check_asset_replacement_compatibility(&active, &candidate).unwrap();
    }

    #[test]
    fn changed_legacy_mesh_payload_is_compatible_when_authored_binding_is_unchanged() {
        let active = legacy_mesh_project(0.0);
        let candidate = legacy_mesh_project(0.25);
        assert_eq!(active.scene(), candidate.scene());
        assert_ne!(active.files(), candidate.files());
        assert_ne!(active.digest(), candidate.digest());
        check_asset_replacement_compatibility(&active, &candidate).unwrap();
    }

    #[test]
    fn manifest_and_asset_mismatches_return_the_contract_category_and_id() {
        let base = empty_project();
        let changed_manifest = prepare(
            manifest_value("renamed project"),
            scene_value(json!([]), json!([]), 0),
            BTreeMap::new(),
        );
        assert_kind(&base, &changed_manifest, AssetReplacementMismatch::Manifest);
        let changed_presentation = prepare(
            json!({
                "schema_version": 1,
                "name": "compatibility",
                "scene": "scene.json",
                "presentation": {"ambient": [0.2, 0.2, 0.2]}
            }),
            scene_value(json!([]), json!([]), 0),
            BTreeMap::new(),
        );
        assert_kind(
            &base,
            &changed_presentation,
            AssetReplacementMismatch::Manifest,
        );

        let with_asset = png_project("one.png", [1, 2, 3, 255], json!({}));
        let error = assert_kind(&base, &with_asset, AssetReplacementMismatch::AssetSet);
        assert_eq!(error.asset_id().unwrap().as_str(), "asset:image");

        let image = png_project("shared.png", [1, 2, 3, 255], json!({}));
        let mut wav = Vec::new();
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 8_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let cursor = Cursor::new(&mut wav);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            writer.write_sample(123_i16).unwrap();
            writer.finalize().unwrap();
        }
        let audio = prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([]),
                json!([{"id":"asset:image", "kind":"daedalus.audio", "uri":"shared.wav", "metadata":{"schema_version":1}}]),
                0,
            ),
            BTreeMap::from([("shared.wav".to_owned(), wav)]),
        );
        let error = assert_kind(&image, &audio, AssetReplacementMismatch::AssetKind);
        assert_eq!(error.asset_id().unwrap().as_str(), "asset:image");

        let metadata_a = png_project(
            "metadata.png",
            [1, 2, 3, 255],
            json!({"schema_version":1,"role":"base_color_srgb","mip_policy":"none","sampler":{"mipmap_filter":"none","wrap_u":"repeat"}}),
        );
        let metadata_b = png_project(
            "metadata.png",
            [1, 2, 3, 255],
            json!({"schema_version":1,"role":"base_color_srgb","mip_policy":"none","sampler":{"mipmap_filter":"none","wrap_u":"clamp_to_edge"}}),
        );
        let error = assert_kind(
            &metadata_a,
            &metadata_b,
            AssetReplacementMismatch::AssetMetadata,
        );
        assert_eq!(error.asset_id().unwrap().as_str(), "asset:image");
    }

    #[test]
    fn entity_and_component_mismatches_are_reported_in_frozen_order() {
        let no_entities = simple_project(json!([]));
        let one_entity = simple_project(json!([entity(
            "entity:hero",
            Some("Hero"),
            None,
            json!([transform_component("component:transform", 0.0)])
        )]));
        let error = assert_kind(
            &no_entities,
            &one_entity,
            AssetReplacementMismatch::EntitySet,
        );
        assert_eq!(error.entity_id().unwrap().as_str(), "entity:hero");

        let name_a = simple_project(json!([entity(
            "entity:hero",
            Some("Hero"),
            None,
            json!([])
        )]));
        let name_b = simple_project(json!([entity(
            "entity:hero",
            Some("Player"),
            None,
            json!([])
        )]));
        assert_kind(&name_a, &name_b, AssetReplacementMismatch::EntityName);

        let components_a = simple_project(json!([entity("entity:hero", None, None, json!([]))]));
        let components_b = simple_project(json!([entity(
            "entity:hero",
            None,
            None,
            json!([transform_component("component:transform", 0.0)])
        )]));
        let error = assert_kind(
            &components_a,
            &components_b,
            AssetReplacementMismatch::ComponentSet,
        );
        assert_eq!(error.entity_id().unwrap().as_str(), "entity:hero");
        assert_eq!(
            error.component_id().unwrap().as_str(),
            "component:transform"
        );

        let transform = simple_project(json!([entity(
            "entity:hero",
            None,
            None,
            json!([transform_component("component:shared", 0.0)])
        )]));
        let camera = simple_project(json!([entity(
            "entity:hero",
            None,
            None,
            json!([{
                "id":"component:shared",
                "type_id":"daedalus.camera_2d",
                "asset_refs":[],
                "data":{"vertical_size":10.0,"near":0.1,"far":100.0}
            }])
        )]));
        assert_kind(&transform, &camera, AssetReplacementMismatch::ComponentType);

        let refs_a = two_model_project("asset:model-a");
        let refs_b = two_model_project("asset:model-b");
        let error = assert_kind(
            &refs_a,
            &refs_b,
            AssetReplacementMismatch::ComponentAssetRefs,
        );
        assert_eq!(error.entity_id().unwrap().as_str(), "entity:hero");
        assert_eq!(error.component_id().unwrap().as_str(), "component:model");

        let data_a = simple_project(json!([entity(
            "entity:hero",
            None,
            None,
            json!([transform_component("component:transform", 0.0)])
        )]));
        let data_b = simple_project(json!([entity(
            "entity:hero",
            None,
            None,
            json!([transform_component("component:transform", 1.0)])
        )]));
        assert_kind(&data_a, &data_b, AssetReplacementMismatch::ComponentData);
    }

    #[test]
    fn first_mismatch_is_id_sorted_and_independent_of_input_order() {
        let active = simple_project(json!([
            entity("entity:z", Some("old z"), None, json!([])),
            entity("entity:a", Some("old a"), None, json!([]))
        ]));
        let candidate = simple_project(json!([
            entity("entity:a", Some("new a"), None, json!([])),
            entity("entity:z", Some("new z"), None, json!([]))
        ]));
        let error = assert_kind(&active, &candidate, AssetReplacementMismatch::EntityName);
        assert_eq!(error.entity_id().unwrap().as_str(), "entity:a");
    }

    #[test]
    fn rejected_comparison_does_not_change_either_loaded_project() {
        let active = sprite_project("sprites/old.png", [255, 0, 0, 255]);
        let active_digest = active.digest().to_owned();
        let active_files = active.files().to_vec();
        let mut changed_scene = serde_json::to_value(active.scene()).unwrap();
        changed_scene["entities"][1]["name"] = json!("renamed");
        let changed = prepare(
            manifest_value("compatibility"),
            changed_scene,
            BTreeMap::from([
                ("sprites/old.png".to_owned(), png([255, 0, 0, 255])),
                (
                    "sprites/sheet.atlas.json".to_owned(),
                    serde_json::to_vec(&json!({
                        "schema_version": 1,
                        "image_asset": "asset:image",
                        "filter_policy": "nearest_no_mip_v1",
                        "regions": [{
                            "id": "region:hero",
                            "rect": [0, 0, 1, 1],
                            "pivot": [0.5, 0.5],
                            "pixels_per_unit": 16.0
                        }]
                    }))
                    .unwrap(),
                ),
            ]),
        );
        let changed_digest = changed.digest().to_owned();
        let changed_files = changed.files().to_vec();
        let error = check_asset_replacement_compatibility(&active, &changed).unwrap_err();
        assert_eq!(error.kind(), AssetReplacementMismatch::EntityName);
        assert_eq!(active.digest(), active_digest);
        assert_eq!(changed.digest(), changed_digest);
        assert_eq!(active.files(), active_files);
        assert_eq!(changed.files(), changed_files);
    }

    #[test]
    fn lower_index_limit_fails_before_any_reference_index_is_allocated() {
        let project = png_project("one.png", [1, 2, 3, 255], json!({}));
        let error = check_with_index_limit(&project, &project, 0).unwrap_err();
        assert_eq!(error.kind(), AssetReplacementMismatch::ComparisonCapacity);
        check_with_index_limit(&project, &project, MAX_COMPARISON_INDEX_BYTES).unwrap();
    }

    #[test]
    fn aggregate_index_budget_includes_live_component_indexes_and_releases_each_entity() {
        let project = prepare(
            manifest_value("compatibility"),
            scene_value(
                json!([
                    entity(
                        "entity:a",
                        None,
                        None,
                        json!([transform_component("component:a", 0.0)])
                    ),
                    entity(
                        "entity:b",
                        None,
                        None,
                        json!([transform_component("component:b", 0.0)])
                    )
                ]),
                json!([image_descriptor("asset:image", "one.png", json!({}))]),
                0,
            ),
            BTreeMap::from([("one.png".to_owned(), png([1, 2, 3, 255]))]),
        );
        let pointer_bytes = size_of::<&StableId>();
        let maximum_live_references = 8 * pointer_bytes;
        let error =
            check_with_index_limit(&project, &project, maximum_live_references - 1).unwrap_err();
        assert_eq!(error.kind(), AssetReplacementMismatch::ComparisonCapacity);
        check_with_index_limit(&project, &project, maximum_live_references).unwrap();

        let mut budget = IndexBudget::new(usize::MAX);
        let error = budget.claim::<u8>(usize::MAX).unwrap_err();
        assert_eq!(error.kind(), AssetReplacementMismatch::ComparisonCapacity);
    }

    #[test]
    fn error_display_is_bounded_and_error_traits_are_available() {
        let id = StableId::new("asset:image").unwrap();
        let error = mismatch_with_asset(AssetReplacementMismatch::AssetMetadata, &id);
        let displayed = error.to_string();
        assert!(displayed.contains("asset metadata changed"));
        assert!(displayed.contains("asset:image"));
        let cloned: AssetReplacementCompatibilityError = error.clone();
        assert_eq!(cloned, error);
    }
}
