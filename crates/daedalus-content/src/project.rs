//! Host-neutral preparation for external, data-only Daedalus projects.
//!
//! The byte API is shared by native and future non-filesystem consumers. The
//! native loader below is only a bounded filesystem adapter around that same
//! preparation path; this module owns no ECS or GPU resources.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::assets::{
    AssetDependencySummary, AssetImportContext, AssetImportError, AssetImportErrorCode,
    AssetImportReport, AssetKind, CpuCookCache, ImportCancellation, ImportLimits, ImportedAsset,
    NativeProjectResolver, PreparedAssetCatalog, SourceBlob, SourceId, SourceResolver,
};
use crate::scene::{
    AssetDescriptor, SceneDocument, SceneEntity, StableId, ValidationIssue, validate_scene,
};

pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const PROJECT_MANIFEST_FILE: &str = "daedalus.project.json";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SCENE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MESH_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 258;
const MAX_ENTITIES: usize = 16_384;
const MAX_MESH_ASSETS: usize = 256;
const MAX_VERTICES: usize = 1_000_000;
const MAX_INDICES: usize = 3_000_000;

/// The project's versioned JSON entry point. `scene` is project-root relative.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub name: String,
    pub scene: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ProjectPresentation>,
}

impl ProjectManifest {
    /// Creates the empty data-only project's manifest. It does not create a game fixture.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            name: name.into(),
            scene: "scene.json".into(),
            presentation: None,
        }
    }
}

/// Initial renderer settings authored with a project. Every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPresentation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<ProjectCamera>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<ProjectDirectionalLight>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ambient: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear: Option<[f32; 4]>,
}

/// Camera values matching the renderer's initial perspective camera contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCamera {
    pub position: [f32; 3],
    pub target: [f32; 3],
    pub up: [f32; 3],
    pub vertical_fov_radians: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for ProjectCamera {
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

/// Directional-light values matching the renderer's initial opaque scene.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDirectionalLight {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub intensity: f32,
}

impl Default for ProjectDirectionalLight {
    fn default() -> Self {
        Self {
            direction: [-0.45, -0.82, -0.35],
            color: [1.0, 0.94, 0.82],
            intensity: 4.0,
        }
    }
}

/// Authored transform. Quaternion components are stored in x, y, z, w order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTransform {
    #[serde(default = "identity_translation")]
    pub translation: [f32; 3],
    #[serde(default = "identity_rotation")]
    pub rotation: [f32; 4],
    #[serde(default = "identity_scale")]
    pub scale: [f32; 3],
}

impl Default for ProjectTransform {
    fn default() -> Self {
        Self {
            translation: identity_translation(),
            rotation: identity_rotation(),
            scale: identity_scale(),
        }
    }
}

fn identity_translation() -> [f32; 3] {
    [0.0; 3]
}

fn identity_rotation() -> [f32; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

fn identity_scale() -> [f32; 3] {
    [1.0; 3]
}

/// One CPU mesh vertex in mesh-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
}

/// Schema-1 CPU indexed triangle mesh. This is not a general model importer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMesh {
    pub schema_version: u32,
    pub vertices: Vec<ProjectVertex>,
    pub indices: Vec<u32>,
}

/// Scalar opaque material parameters supported by the initial renderer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectMaterial {
    pub base_color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
}

impl Default for ProjectMaterial {
    fn default() -> Self {
        Self {
            base_color: [0.65, 0.68, 0.72],
            metallic: 0.0,
            roughness: 0.6,
            emissive: [0.0; 3],
        }
    }
}

/// An entity's supported component projection, preserving its authored stable ID.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedEntity {
    pub id: StableId,
    pub name: Option<String>,
    pub transform: ProjectTransform,
    pub mesh: Option<StableId>,
    pub material: Option<ProjectMaterial>,
}

/// Identity of one byte source read while preparing a project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectFileIdentity {
    /// Normalized project-relative source name using forward slashes.
    pub source_id: String,
    /// Lowercase 64-digit SHA-256, without an algorithm prefix.
    pub content_sha256: String,
    pub byte_len: u64,
}

/// Validated aggregate resource bounds for manifest, scene, mesh sources and decoded data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectLimits {
    pub max_manifest_bytes: u64,
    pub max_scene_bytes: u64,
    pub max_mesh_bytes: u64,
    pub max_total_bytes: u64,
    pub max_files: usize,
    pub max_entities: usize,
    pub max_mesh_assets: usize,
    pub max_vertices: usize,
    pub max_indices: usize,
}

impl Default for ProjectLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: MAX_MANIFEST_BYTES,
            max_scene_bytes: MAX_SCENE_BYTES,
            max_mesh_bytes: MAX_MESH_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_files: MAX_FILES,
            max_entities: MAX_ENTITIES,
            max_mesh_assets: MAX_MESH_ASSETS,
            max_vertices: MAX_VERTICES,
            max_indices: MAX_INDICES,
        }
    }
}

impl ProjectLimits {
    /// Rejects zero and values above the process-wide hard ceilings.
    pub fn validate(self) -> Result<(), ProjectError> {
        if self.max_manifest_bytes == 0
            || self.max_manifest_bytes > MAX_MANIFEST_BYTES
            || self.max_scene_bytes == 0
            || self.max_scene_bytes > MAX_SCENE_BYTES
            || self.max_mesh_bytes == 0
            || self.max_mesh_bytes > MAX_MESH_BYTES
            || self.max_total_bytes == 0
            || self.max_total_bytes > MAX_TOTAL_BYTES
            || self.max_files == 0
            || self.max_files > MAX_FILES
            || self.max_entities == 0
            || self.max_entities > MAX_ENTITIES
            || self.max_mesh_assets == 0
            || self.max_mesh_assets > MAX_MESH_ASSETS
            || self.max_vertices == 0
            || self.max_vertices > MAX_VERTICES
            || self.max_indices == 0
            || self.max_indices > MAX_INDICES
        {
            return Err(ProjectError::InvalidLimits {
                message: format!(
                    "each project limit must be positive and no larger than its hard ceiling (manifest {MAX_MANIFEST_BYTES}, scene {MAX_SCENE_BYTES}, mesh {MAX_MESH_BYTES}, total {MAX_TOTAL_BYTES}, files {MAX_FILES}, entities {MAX_ENTITIES}, meshes {MAX_MESH_ASSETS}, vertices {MAX_VERTICES}, indices {MAX_INDICES})"
                ),
            });
        }
        Ok(())
    }
}

/// A structured project preparation or loading failure with source and JSON path context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectError {
    InvalidLimits {
        message: String,
    },
    InvalidPath {
        source_id: String,
        message: String,
    },
    Io {
        source_id: String,
        message: String,
    },
    Json {
        source_id: String,
        line: usize,
        column: usize,
        message: String,
    },
    Invalid {
        source_id: String,
        path: String,
        message: String,
    },
    Unsupported {
        source_id: String,
        path: String,
        message: String,
    },
    LimitExceeded {
        source_id: String,
        path: String,
        message: String,
    },
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits { message } => write!(f, "invalid project limits: {message}"),
            Self::InvalidPath { source_id, message } => {
                write!(f, "invalid project path '{source_id}': {message}")
            }
            Self::Io { source_id, message } => {
                write!(f, "cannot read project source '{source_id}': {message}")
            }
            Self::Json {
                source_id,
                line,
                column,
                message,
            } => write!(f, "{source_id}:{line}:{column}: {message}"),
            Self::Invalid {
                source_id,
                path,
                message,
            } => write!(f, "{source_id}:{path}: {message}"),
            Self::Unsupported {
                source_id,
                path,
                message,
            } => write!(f, "{source_id}:{path}: unsupported: {message}"),
            Self::LimitExceeded {
                source_id,
                path,
                message,
            } => write!(f, "{source_id}:{path}: limit exceeded: {message}"),
        }
    }
}

impl std::error::Error for ProjectError {}

/// Fully owned, host-neutral project data ready for a runtime to consume.
#[derive(Clone, Debug)]
pub struct PreparedProject {
    manifest: ProjectManifest,
    scene: SceneDocument,
    entities: Vec<PreparedEntity>,
    meshes: BTreeMap<StableId, ProjectMesh>,
    files: Vec<ProjectFileIdentity>,
    /// SHA-256 project identity over sorted relative source IDs and file hashes.
    digest: String,
    asset_catalog: Option<PreparedAssetCatalog>,
    asset_import_report: Option<AssetImportReport>,
    /// Validated model renderer components keyed by their owning entity ID.
    model_renderers: BTreeMap<StableId, PreparedModelRendererBinding>,
    /// Validated sprite renderer components keyed by their owning entity ID.
    sprite_renderers: BTreeMap<StableId, PreparedSpriteRendererBinding>,
    /// The single validated orthographic camera component, if authored.
    camera_2d: Option<PreparedCamera2dBinding>,
}

impl PartialEq for PreparedProject {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest
            && self.scene == other.scene
            && self.entities == other.entities
            && self.meshes == other.meshes
            && self.files == other.files
            && self.digest == other.digest
            && self.asset_catalog == other.asset_catalog
            && self.model_renderers == other.model_renderers
            && self.sprite_renderers == other.sprite_renderers
            && self.camera_2d == other.camera_2d
    }
}

impl PreparedProject {
    pub fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    pub fn scene(&self) -> &SceneDocument {
        &self.scene
    }

    pub fn entities(&self) -> &[PreparedEntity] {
        &self.entities
    }

    pub fn meshes(&self) -> &BTreeMap<StableId, ProjectMesh> {
        &self.meshes
    }

    pub fn files(&self) -> &[ProjectFileIdentity] {
        &self.files
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn asset_catalog(&self) -> Option<&PreparedAssetCatalog> {
        self.asset_catalog.as_ref()
    }

    pub fn asset_import_report(&self) -> Option<&AssetImportReport> {
        self.asset_import_report.as_ref()
    }

    /// Returns validated model renderer bindings keyed by entity ID.
    pub fn model_renderers(&self) -> &BTreeMap<StableId, PreparedModelRendererBinding> {
        &self.model_renderers
    }

    /// Returns validated sprite renderer bindings keyed by owning entity ID.
    pub fn sprite_renderers(&self) -> &BTreeMap<StableId, PreparedSpriteRendererBinding> {
        &self.sprite_renderers
    }

    /// Returns the project's single validated orthographic camera, if present.
    pub fn camera_2d(&self) -> Option<&PreparedCamera2dBinding> {
        self.camera_2d.as_ref()
    }

    /// Releases only cache eviction pins while retaining all owned cooked asset data.
    ///
    /// Clones of this project retain their own shared pin references until they are released or
    /// dropped. The default loaders do not detach pins automatically.
    pub fn release_asset_cache_pins(mut self) -> Self {
        if let Some(catalog) = self.asset_catalog.as_mut() {
            catalog.release_cache_pins();
        }
        self
    }

    /// Revalidates every public prepared value before runtime construction.
    pub fn validate(&self) -> Result<(), ProjectError> {
        validate_prepared_project(self, ProjectLimits::default())
    }
}

/// A validated reference from an authored entity to an imported static model asset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedModelRendererBinding {
    component_id: StableId,
    model_asset_id: StableId,
}

/// A validated reference from an authored entity to a region in an imported sprite atlas.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedSpriteRendererBinding {
    component_id: StableId,
    atlas_asset_id: StableId,
    region_id: StableId,
    tint: [f32; 4],
    sorting_layer: i32,
    order_in_layer: i32,
}

impl PreparedSpriteRendererBinding {
    pub fn component_id(&self) -> &StableId {
        &self.component_id
    }

    pub fn atlas_asset_id(&self) -> &StableId {
        &self.atlas_asset_id
    }

    pub fn region_id(&self) -> &StableId {
        &self.region_id
    }

    pub fn tint(&self) -> [f32; 4] {
        self.tint
    }

    pub fn sorting_layer(&self) -> i32 {
        self.sorting_layer
    }

    pub fn order_in_layer(&self) -> i32 {
        self.order_in_layer
    }
}

/// A validated project-local orthographic camera component.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedCamera2dBinding {
    entity_id: StableId,
    component_id: StableId,
    vertical_size: f32,
    near: f32,
    far: f32,
}

impl PreparedCamera2dBinding {
    pub fn entity_id(&self) -> &StableId {
        &self.entity_id
    }

    pub fn component_id(&self) -> &StableId {
        &self.component_id
    }

    pub fn vertical_size(&self) -> f32 {
        self.vertical_size
    }

    pub fn near(&self) -> f32 {
        self.near
    }

    pub fn far(&self) -> f32 {
        self.far
    }
}

impl PreparedModelRendererBinding {
    pub fn component_id(&self) -> &StableId {
        &self.component_id
    }

    pub fn model_asset_id(&self) -> &StableId {
        &self.model_asset_id
    }
}

/// Prepares all project files already read by a host, without using a filesystem.
/// The asset map is keyed by each mesh descriptor's project-relative `uri`.
pub fn prepare_project_from_bytes(
    manifest_bytes: &[u8],
    scene_bytes: &[u8],
    asset_bytes: &BTreeMap<String, Vec<u8>>,
    limits: ProjectLimits,
) -> Result<PreparedProject, ProjectError> {
    limits.validate()?;
    enforce_size(
        PROJECT_MANIFEST_FILE,
        "manifest",
        manifest_bytes.len(),
        limits.max_manifest_bytes,
    )?;
    enforce_size(
        PROJECT_MANIFEST_FILE,
        "scene",
        scene_bytes.len(),
        limits.max_scene_bytes,
    )?;
    let initial_total = (manifest_bytes.len() as u64)
        .checked_add(scene_bytes.len() as u64)
        .ok_or_else(|| limit_error(PROJECT_MANIFEST_FILE, "", "total byte count overflow"))?;
    if initial_total > limits.max_total_bytes {
        return Err(limit_error(
            PROJECT_MANIFEST_FILE,
            "",
            format!(
                "manifest and scene use {initial_total} bytes, exceeding total budget {}",
                limits.max_total_bytes
            ),
        ));
    }

    let manifest: ProjectManifest = parse_json(PROJECT_MANIFEST_FILE, manifest_bytes)?;
    validate_manifest(&manifest)?;
    let scene_source = normalize_relative_path(&manifest.scene)
        .map_err(|message| path_error(&manifest.scene, format!("manifest.scene: {message}")))?;
    if scene_source == PROJECT_MANIFEST_FILE {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "scene",
            "scene cannot reference the project manifest itself",
        ));
    }

    let scene: SceneDocument = parse_json(&scene_source, scene_bytes)?;
    validate_scene_document(&scene_source, &scene, limits)?;
    let mesh_assets = supported_mesh_assets(&scene_source, &scene)?;
    if mesh_assets.len() > limits.max_mesh_assets {
        return Err(limit_error(
            &scene_source,
            "assets",
            format!(
                "{} mesh assets exceed the limit of {}",
                mesh_assets.len(),
                limits.max_mesh_assets
            ),
        ));
    }

    let mut normalized_assets = BTreeMap::<String, &Vec<u8>>::new();
    for (source, bytes) in asset_bytes {
        let normalized =
            normalize_relative_path(source).map_err(|message| path_error(source, message))?;
        if normalized_assets
            .insert(normalized.clone(), bytes)
            .is_some()
        {
            return Err(invalid(
                source,
                "assets",
                format!("more than one byte source normalizes to '{normalized}'"),
            ));
        }
    }

    let requested_sources: BTreeSet<&str> = mesh_assets
        .iter()
        .map(|(_, _, source)| source.as_str())
        .collect();
    for source in normalized_assets.keys() {
        if !requested_sources.contains(source.as_str()) {
            return Err(invalid(
                source,
                "assets",
                "byte source is not referenced by a supported mesh asset",
            ));
        }
    }

    let mut total_bytes = initial_total;
    let mut files = vec![
        file_identity(PROJECT_MANIFEST_FILE, manifest_bytes),
        file_identity(&scene_source, scene_bytes),
    ];
    let mut loaded_sources = BTreeSet::<String>::new();
    let mut meshes = BTreeMap::new();
    let mut vertex_total = 0usize;
    let mut index_total = 0usize;

    for (asset_index, asset, source) in mesh_assets {
        let path = format!("assets[{asset_index}]");
        if source == PROJECT_MANIFEST_FILE || source == scene_source {
            return Err(invalid(
                &scene_source,
                format!("{path}.uri"),
                "mesh source must be a distinct file from the manifest and scene",
            ));
        }
        let bytes = normalized_assets.get(&source).ok_or_else(|| {
            invalid(
                &scene_source,
                format!("{path}.uri"),
                format!("no bytes were provided for mesh source '{source}'"),
            )
        })?;
        enforce_size(&source, "", bytes.len(), limits.max_mesh_bytes)?;
        if loaded_sources.insert(source.clone()) {
            total_bytes = total_bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| limit_error(&source, "", "total byte count overflow"))?;
            if total_bytes > limits.max_total_bytes {
                return Err(limit_error(
                    &source,
                    "",
                    format!(
                        "project sources use {total_bytes} bytes, exceeding total budget {}",
                        limits.max_total_bytes
                    ),
                ));
            }
            files.push(file_identity(&source, bytes));
        }
        let source_bytes = *bytes;
        verify_declared_hash(&scene_source, &path, asset, source_bytes)?;
        let mesh: ProjectMesh = parse_json(&source, source_bytes)?;
        validate_mesh(&source, &mesh, limits)?;
        vertex_total = vertex_total
            .checked_add(mesh.vertices.len())
            .ok_or_else(|| limit_error(&source, "vertices", "vertex count overflow"))?;
        index_total = index_total
            .checked_add(mesh.indices.len())
            .ok_or_else(|| limit_error(&source, "indices", "index count overflow"))?;
        if vertex_total > limits.max_vertices {
            return Err(limit_error(
                &scene_source,
                "assets",
                format!(
                    "project has {vertex_total} mesh vertices, exceeding limit {}",
                    limits.max_vertices
                ),
            ));
        }
        if index_total > limits.max_indices {
            return Err(limit_error(
                &scene_source,
                "assets",
                format!(
                    "project has {index_total} mesh indices, exceeding limit {}",
                    limits.max_indices
                ),
            ));
        }
        if meshes.insert(asset.id.clone(), mesh).is_some() {
            return Err(invalid(
                &scene_source,
                format!("{path}.id"),
                format!("duplicate mesh asset ID '{}'", asset.id),
            ));
        }
    }

    if files.len() > limits.max_files {
        return Err(limit_error(
            PROJECT_MANIFEST_FILE,
            "",
            format!(
                "project reads {} unique files, exceeding limit {}",
                files.len(),
                limits.max_files
            ),
        ));
    }

    let decoded = decode_entities(&scene_source, &scene, &meshes)?;
    validate_camera_presentation_compatibility(
        &scene_source,
        &manifest,
        decoded.camera_2d.as_ref(),
    )?;
    let DecodedEntityProjection {
        entities,
        model_renderers,
        sprite_renderers,
        camera_2d,
    } = decoded;
    files.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let digest = project_digest(&files);
    let prepared = PreparedProject {
        manifest,
        scene,
        entities,
        meshes,
        files,
        digest,
        asset_catalog: None,
        asset_import_report: None,
        model_renderers,
        sprite_renderers,
        camera_2d,
    };
    validate_prepared_project(&prepared, limits)?;
    Ok(prepared)
}

/// Prepares a complete project and its typed CPU asset catalog through a caller-owned source
/// resolver and cache. Project paths remain root-relative; parser dependency URIs are resolved by
/// the importer relative to their owning source.
pub fn prepare_project_with_assets(
    manifest_bytes: &[u8],
    scene_bytes: &[u8],
    resolver: &mut dyn SourceResolver,
    cache: &mut dyn CpuCookCache,
    limits: ImportLimits,
    cancellation: &ImportCancellation,
) -> Result<PreparedProject, AssetImportError> {
    limits.validate()?;
    cancellation_point(cancellation, PROJECT_MANIFEST_FILE, "manifest")?;
    if manifest_bytes.is_empty()
        || manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES
        || manifest_bytes.len() as u64 > limits.max_source_bytes
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new(PROJECT_MANIFEST_FILE, "manifest"),
            "manifest is empty or exceeds 64 KiB",
        ));
    }
    if scene_bytes.is_empty()
        || scene_bytes.len() as u64 > MAX_SCENE_BYTES
        || scene_bytes.len() as u64 > limits.max_source_bytes
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new("scene", "scene"),
            "scene is empty or exceeds 16 MiB",
        ));
    }

    let manifest_id = SourceId::new(PROJECT_MANIFEST_FILE)?;
    let manifest = parse_json(PROJECT_MANIFEST_FILE, manifest_bytes)?;
    validate_manifest(&manifest)?;
    let scene_id = SourceId::new(&manifest.scene)?;
    if scene_id == manifest_id {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "scene",
            "scene cannot reference the project manifest itself",
        )
        .into());
    }
    prepare_project_with_asset_sources(
        manifest,
        SourceBlob::new(manifest_id, Arc::<[u8]>::from(manifest_bytes)),
        SourceBlob::new(scene_id, Arc::<[u8]>::from(scene_bytes)),
        resolver,
        cache,
        limits,
        cancellation,
    )
}

/// Shared preparation core for native and caller-provided source data. The native adapter passes
/// its already-hashed blobs directly so identities are not rehashed and file bytes are not copied.
fn prepare_project_with_asset_sources(
    manifest: ProjectManifest,
    manifest_source_blob: SourceBlob,
    scene_source_blob: SourceBlob,
    resolver: &mut dyn SourceResolver,
    cache: &mut dyn CpuCookCache,
    limits: ImportLimits,
    cancellation: &ImportCancellation,
) -> Result<PreparedProject, AssetImportError> {
    let manifest_id = SourceId::new(PROJECT_MANIFEST_FILE)?;
    let scene_id = SourceId::new(&manifest.scene)?;
    if manifest_source_blob.source_id() != &manifest_id
        || scene_source_blob.source_id() != &scene_id
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Invalid,
            crate::assets::ImportLocation::new(PROJECT_MANIFEST_FILE, "sources"),
            "provided source identities do not match the project manifest",
        ));
    }
    if manifest_source_blob.byte_len() == 0
        || manifest_source_blob.byte_len() > MAX_MANIFEST_BYTES.min(limits.max_source_bytes)
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new(PROJECT_MANIFEST_FILE, "manifest"),
            "manifest is empty or exceeds 64 KiB",
        ));
    }
    if scene_source_blob.byte_len() == 0
        || scene_source_blob.byte_len() > MAX_SCENE_BYTES.min(limits.max_source_bytes)
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new(scene_id.to_string(), "scene"),
            "scene is empty or exceeds 16 MiB",
        ));
    }
    cancellation_point(cancellation, PROJECT_MANIFEST_FILE, "manifest")?;
    let scene_source = scene_id.to_string();
    let scene: SceneDocument = parse_json(&scene_source, scene_source_blob.bytes())?;
    validate_scene_document(&scene_source, &scene, ProjectLimits::default())?;
    if scene.assets.len() > limits.max_assets {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new(&scene_source, "assets"),
            format!(
                "{} asset descriptors exceed configured limit {}",
                scene.assets.len(),
                limits.max_assets
            ),
        ));
    }

    let initial_sources = [manifest_source_blob, scene_source_blob];
    let mut context =
        AssetImportContext::new(resolver, cache, limits, cancellation, initial_sources)?;
    let mut asset_order = (0..scene.assets.len()).collect::<Vec<_>>();
    asset_order.sort_by_key(|index| match scene.assets[*index].kind.as_str() {
        "daedalus.image" => (0, *index),
        "daedalus.mesh" | "daedalus.model" | "daedalus.audio" => (1, *index),
        "daedalus.sprite_atlas" => (2, *index),
        _ => (3, *index),
    });

    let mut meshes = BTreeMap::<StableId, ProjectMesh>::new();
    let mut imported_assets = BTreeMap::<StableId, ImportedAsset>::new();
    let mut legacy_meshes = 0usize;
    let mut legacy_vertices = 0usize;
    let mut legacy_indices = 0usize;
    for asset_index in asset_order {
        cancellation_point(
            cancellation,
            &scene_source,
            &format!("assets[{asset_index}]"),
        )?;
        let descriptor = &scene.assets[asset_index];
        let path = format!("assets[{asset_index}]");
        let source_id = SourceId::new(&descriptor.uri).map_err(|error| {
            AssetImportError::new(
                error.code(),
                crate::assets::ImportLocation::new(&scene_source, format!("{path}.uri")),
                error.message(),
            )
        })?;
        if source_id == manifest_id || source_id == scene_id {
            return Err(invalid(
                &scene_source,
                format!("{path}.uri"),
                "asset source must be distinct from manifest and scene",
            )
            .into());
        }
        if let Some(hash) = &descriptor.content_hash {
            parse_declared_hash(hash).map_err(|message| {
                invalid(&scene_source, format!("{path}.content_hash"), message)
            })?;
        }
        context.begin_asset(descriptor.id.clone(), source_id.clone());
        let per_asset_limit = if descriptor.kind.as_str() == "daedalus.mesh" {
            MAX_MESH_BYTES.min(limits.max_source_bytes)
        } else {
            limits.max_source_bytes
        };
        let source = context.resolve_uri(
            &manifest_id,
            &descriptor.uri,
            per_asset_limit,
            crate::assets::ImportLocation::new(&scene_source, format!("{path}.uri")),
        )?;
        if let Some(hash) = &descriptor.content_hash {
            verify_declared_hash_digest(&scene_source, &path, descriptor, &source.sha256())
                .map_err(AssetImportError::from)?;
            debug_assert!(!hash.is_empty());
        }
        if descriptor.kind.as_str() == "daedalus.mesh" {
            if !descriptor.metadata.is_empty() {
                return Err(unsupported(
                    &scene_source,
                    format!("{path}.metadata"),
                    "legacy mesh metadata is not supported",
                )
                .into());
            }
            legacy_meshes += 1;
            if legacy_meshes > limits.max_assets {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    crate::assets::ImportLocation::new(&scene_source, "assets"),
                    "legacy mesh count exceeds configured asset limit",
                ));
            }
            let mesh: ProjectMesh = parse_json(source.source_id().as_str(), source.bytes())?;
            validate_mesh(source.source_id().as_str(), &mesh, ProjectLimits::default())?;
            context.account(
                crate::assets::BudgetKind::GeometryVertices,
                mesh.vertices.len() as u64,
                crate::assets::ImportLocation::new(source.source_id().to_string(), "vertices"),
            )?;
            context.account(
                crate::assets::BudgetKind::GeometryIndices,
                mesh.indices.len() as u64,
                crate::assets::ImportLocation::new(source.source_id().to_string(), "indices"),
            )?;
            let cooked_bytes = (mesh.vertices.len() as u64)
                .checked_mul(std::mem::size_of::<ProjectVertex>() as u64)
                .and_then(|bytes| {
                    bytes.checked_add(
                        (mesh.indices.len() as u64)
                            .checked_mul(std::mem::size_of::<u32>() as u64)?,
                    )
                })
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(
                            source.source_id().to_string(),
                            "geometry",
                        ),
                        "legacy cooked byte count overflow",
                    )
                })?;
            context.account(
                crate::assets::BudgetKind::CookedBytes,
                cooked_bytes,
                crate::assets::ImportLocation::new(source.source_id().to_string(), "geometry"),
            )?;
            legacy_vertices = legacy_vertices
                .checked_add(mesh.vertices.len())
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(
                            source.source_id().to_string(),
                            "vertices",
                        ),
                        "legacy geometry vertex count overflow",
                    )
                })?;
            legacy_indices = legacy_indices
                .checked_add(mesh.indices.len())
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(
                            source.source_id().to_string(),
                            "indices",
                        ),
                        "legacy geometry index count overflow",
                    )
                })?;
            if legacy_vertices > limits.max_geometry_vertices
                || legacy_indices > limits.max_geometry_indices
            {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    crate::assets::ImportLocation::new(source.source_id().to_string(), "geometry"),
                    "legacy mesh totals exceed configured geometry limits",
                ));
            }
            if meshes.insert(descriptor.id.clone(), mesh).is_some() {
                return Err(invalid(
                    &scene_source,
                    format!("{path}.id"),
                    "duplicate legacy mesh ID",
                )
                .into());
            }
            continue;
        }

        let imported = crate::assets::import_asset(descriptor, source, &mut context)?;
        if !imported_kind_matches(descriptor.kind.as_str(), imported.kind()) {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Internal,
                crate::assets::ImportLocation::new(&scene_source, format!("{path}.kind")),
                "asset importer returned a kind different from the descriptor",
            ));
        }
        context.register_imported_asset(descriptor.id.clone(), imported.clone());
        imported_assets.insert(descriptor.id.clone(), imported);
    }

    let mut sources = context.source_blobs();
    sources.sort_by(|left, right| left.source_id().cmp(right.source_id()));
    if sources.len() > limits.max_unique_sources {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            crate::assets::ImportLocation::new(&scene_source, "sources"),
            "unique source count exceeds configured limit",
        ));
    }
    let source_by_id = sources
        .iter()
        .map(|source| (source.source_id().as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut files = sources
        .iter()
        .map(project_file_identity_from_blob)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let binding_metadata_bytes = estimate_project_binding_metadata_bytes(&scene, &scene_source)?;
    let catalog_metadata_bytes = estimate_catalog_metadata_bytes(
        &imported_assets,
        binding_metadata_bytes,
        &files,
        &context,
        context.cache_pin_count(),
    )?;
    context.account_catalog_metadata(
        catalog_metadata_bytes,
        crate::assets::ImportLocation::new(&scene_source, "catalog_metadata"),
    )?;
    let decoded = decode_entities_with_assets(&scene_source, &scene, &meshes, &imported_assets)?;
    validate_camera_presentation_compatibility(
        &scene_source,
        &manifest,
        decoded.camera_2d.as_ref(),
    )?;
    let DecodedEntityProjection {
        entities,
        model_renderers,
        sprite_renderers,
        camera_2d,
    } = decoded;
    let mut dependencies = BTreeMap::<StableId, Vec<AssetDependencySummary>>::new();
    for id in imported_assets.keys() {
        let mut entries = Vec::new();
        for dependency_id in context.asset_dependency_ids(id) {
            let dependency = source_by_id.get(dependency_id).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::Internal,
                    crate::assets::ImportLocation::new(&scene_source, "dependencies"),
                    format!("dependency source '{dependency_id}' was not retained"),
                )
            })?;
            entries.push(AssetDependencySummary::new(
                dependency_id.to_owned(),
                encode_hex(&dependency.sha256()),
                dependency.byte_len(),
            ));
        }
        dependencies.insert(id.clone(), entries);
    }
    let digest = project_digest(&files);
    let catalog = PreparedAssetCatalog::new(
        imported_assets,
        files.clone(),
        dependencies,
        context.take_cache_leases(),
    );
    let memory = context.memory_summary()?;
    let report = AssetImportReport::new(&catalog, sources, context.metrics(), memory);
    let prepared = PreparedProject {
        manifest,
        scene,
        entities,
        meshes,
        files,
        digest,
        asset_catalog: Some(catalog),
        asset_import_report: Some(report),
        model_renderers,
        sprite_renderers,
        camera_2d,
    };
    validate_prepared_project(&prepared, ProjectLimits::default())?;
    Ok(prepared)
}

/// Bounded native filesystem adapter for the asset-aware project preparation path.
pub fn load_project_with_assets(
    root: impl AsRef<Path>,
    cache: &mut dyn CpuCookCache,
    limits: ImportLimits,
    cancellation: &ImportCancellation,
) -> Result<PreparedProject, AssetImportError> {
    limits.validate()?;
    cancellation_point(cancellation, PROJECT_MANIFEST_FILE, "manifest")?;
    let mut resolver = NativeProjectResolver::open(root)?;
    let manifest_id = SourceId::new(PROJECT_MANIFEST_FILE)?;
    let manifest = resolver.read_source(&manifest_id, (64 * 1024).min(limits.max_source_bytes))?;
    let manifest_doc: ProjectManifest = parse_json(PROJECT_MANIFEST_FILE, manifest.bytes())?;
    validate_manifest(&manifest_doc)?;
    let scene_id = SourceId::new(&manifest_doc.scene)?;
    let scene_max = (16 * 1024 * 1024).min(limits.max_source_bytes);
    let scene = resolver.read_source(&scene_id, scene_max)?;
    prepare_project_with_asset_sources(
        manifest_doc,
        manifest,
        scene,
        &mut resolver,
        cache,
        limits,
        cancellation,
    )
}

fn cancellation_point(
    cancellation: &ImportCancellation,
    source_id: &str,
    path: &str,
) -> Result<(), AssetImportError> {
    if cancellation.is_cancelled() {
        Err(AssetImportError::new(
            AssetImportErrorCode::Cancelled,
            crate::assets::ImportLocation::new(source_id, path),
            "asset import was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn imported_kind_matches(kind: &str, imported: AssetKind) -> bool {
    matches!(
        (kind, imported),
        ("daedalus.model", AssetKind::Model)
            | ("daedalus.image", AssetKind::Image)
            | ("daedalus.sprite_atlas", AssetKind::SpriteAtlas)
            | ("daedalus.audio", AssetKind::Audio)
    )
}

/// Loads a project from a native directory using the default limits.
pub fn load_project(root: impl AsRef<Path>) -> Result<PreparedProject, ProjectError> {
    load_project_with_limits(root, ProjectLimits::default())
}

/// Bounded native filesystem adapter for [`prepare_project_from_bytes`].
pub fn load_project_with_limits(
    root: impl AsRef<Path>,
    limits: ProjectLimits,
) -> Result<PreparedProject, ProjectError> {
    limits.validate()?;
    let root = root.as_ref();
    let canonical_root = fs::canonicalize(root).map_err(|error| ProjectError::Io {
        source_id: root.display().to_string(),
        message: error.to_string(),
    })?;
    if !canonical_root.is_dir() {
        return Err(ProjectError::InvalidPath {
            source_id: canonical_root.display().to_string(),
            message: "project root must be a directory".into(),
        });
    }

    let manifest_bytes = read_project_file(
        &canonical_root,
        PROJECT_MANIFEST_FILE,
        limits.max_manifest_bytes,
        limits.max_total_bytes,
    )?;
    let manifest: ProjectManifest = parse_json(PROJECT_MANIFEST_FILE, &manifest_bytes)?;
    validate_manifest(&manifest)?;
    let scene_source = normalize_relative_path(&manifest.scene)
        .map_err(|message| path_error(&manifest.scene, format!("manifest.scene: {message}")))?;
    if scene_source == PROJECT_MANIFEST_FILE {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "scene",
            "scene cannot reference the project manifest itself",
        ));
    }
    let remaining = limits
        .max_total_bytes
        .saturating_sub(manifest_bytes.len() as u64);
    let scene_bytes = read_project_file(
        &canonical_root,
        &scene_source,
        limits.max_scene_bytes,
        remaining,
    )?;
    let current_total = (manifest_bytes.len() as u64)
        .checked_add(scene_bytes.len() as u64)
        .ok_or_else(|| limit_error(&scene_source, "", "total byte count overflow"))?;
    if current_total > limits.max_total_bytes {
        return Err(limit_error(
            &scene_source,
            "",
            "manifest and scene exceed total byte budget",
        ));
    }
    let scene: SceneDocument = parse_json(&scene_source, &scene_bytes)?;
    validate_scene_document(&scene_source, &scene, limits)?;
    let mesh_assets = supported_mesh_assets(&scene_source, &scene)?;
    if mesh_assets.len() > limits.max_mesh_assets {
        return Err(limit_error(
            &scene_source,
            "assets",
            format!(
                "{} mesh assets exceed the configured limit",
                mesh_assets.len()
            ),
        ));
    }
    let expected_files = mesh_assets
        .iter()
        .map(|(_, _, source)| source.as_str())
        .collect::<BTreeSet<_>>()
        .len()
        .saturating_add(2);
    if expected_files > limits.max_files {
        return Err(limit_error(
            &scene_source,
            "assets",
            format!(
                "project requires {expected_files} unique source files, exceeding configured limit {}",
                limits.max_files
            ),
        ));
    }
    let mut asset_bytes = BTreeMap::<String, Vec<u8>>::new();
    let mut loaded_sources = BTreeSet::new();
    let mut running_total = current_total;
    for (_, _, source) in mesh_assets {
        if !loaded_sources.insert(source.clone()) {
            continue;
        }
        let max_by_total = limits.max_total_bytes.saturating_sub(running_total);
        let bytes = read_project_file(
            &canonical_root,
            &source,
            limits.max_mesh_bytes,
            max_by_total,
        )?;
        running_total = running_total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| limit_error(&source, "", "total byte count overflow"))?;
        if running_total > limits.max_total_bytes {
            return Err(limit_error(
                &source,
                "",
                "project sources exceed total byte budget",
            ));
        }
        asset_bytes.insert(source, bytes);
    }
    prepare_project_from_bytes(&manifest_bytes, &scene_bytes, &asset_bytes, limits)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeshRendererData {
    mesh: StableId,
    #[serde(default)]
    material: Option<ProjectMaterial>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelRendererData {
    model: StableId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpriteRendererData {
    atlas: StableId,
    region: StableId,
    #[serde(default = "default_sprite_tint")]
    tint: [f32; 4],
    #[serde(default)]
    sorting_layer: i32,
    #[serde(default)]
    order_in_layer: i32,
}

fn default_sprite_tint() -> [f32; 4] {
    [1.0; 4]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Camera2dData {
    vertical_size: f32,
    near: f32,
    far: f32,
}

#[derive(Debug)]
struct DecodedEntityProjection {
    entities: Vec<PreparedEntity>,
    model_renderers: BTreeMap<StableId, PreparedModelRendererBinding>,
    sprite_renderers: BTreeMap<StableId, PreparedSpriteRendererBinding>,
    camera_2d: Option<PreparedCamera2dBinding>,
}

fn validate_manifest(manifest: &ProjectManifest) -> Result<(), ProjectError> {
    if manifest.schema_version != PROJECT_SCHEMA_VERSION {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "schema_version",
            format!(
                "unsupported project schema version {} (supported: {PROJECT_SCHEMA_VERSION})",
                manifest.schema_version
            ),
        ));
    }
    if manifest.name.trim().is_empty() {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "name",
            "project name must not be empty",
        ));
    }
    if manifest.name.len() > 256 {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "name",
            "project name must be at most 256 UTF-8 bytes",
        ));
    }
    normalize_relative_path(&manifest.scene)
        .map_err(|message| invalid(PROJECT_MANIFEST_FILE, "scene", message))?;
    if let Some(presentation) = &manifest.presentation {
        validate_presentation(presentation)?;
    }
    Ok(())
}

fn validate_presentation(presentation: &ProjectPresentation) -> Result<(), ProjectError> {
    if let Some(camera) = &presentation.camera {
        let all_finite = camera
            .position
            .iter()
            .chain(camera.target.iter())
            .chain(camera.up.iter())
            .all(|value| value.is_finite());
        let direction = subtract(camera.target, camera.position);
        let direction_length = length_squared(direction);
        let up_length = length_squared(camera.up);
        let view_up_length = length_squared(cross(direction, camera.up));
        if !all_finite
            || !direction.iter().all(|value| value.is_finite())
            || !direction_length.is_finite()
            || !up_length.is_finite()
            || !view_up_length.is_finite()
            || direction_length < 1.0e-8
            || up_length < 1.0e-8
            || view_up_length < 1.0e-8
            || !camera.vertical_fov_radians.is_finite()
            || camera.vertical_fov_radians <= 0.01
            || camera.vertical_fov_radians >= std::f32::consts::PI - 0.01
            || !camera.near.is_finite()
            || !camera.far.is_finite()
            || camera.near <= 0.0
            || camera.far <= camera.near
        {
            return Err(invalid(
                PROJECT_MANIFEST_FILE,
                "presentation.camera",
                "camera vectors and projection must define a finite view with 0 < near < far",
            ));
        }
    }
    if let Some(light) = &presentation.light
        && (!light.direction.iter().all(|value| value.is_finite())
            || !length_squared(light.direction).is_finite()
            || length_squared(light.direction) < 1.0e-12
            || !light
                .color
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
            || !light.intensity.is_finite()
            || light.intensity < 0.0)
    {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "presentation.light",
            "direction must be finite and nonzero; color and intensity must be finite and nonnegative",
        ));
    }
    if presentation
        .ambient
        .is_some_and(|color| !color.iter().all(|value| value.is_finite() && *value >= 0.0))
    {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "presentation.ambient",
            "ambient color must be finite and nonnegative",
        ));
    }
    if presentation.clear.is_some_and(|color| {
        !color
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
    }) {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "presentation.clear",
            "clear color must be finite and within [0, 1]",
        ));
    }
    Ok(())
}

fn validate_scene_document(
    source_id: &str,
    scene: &SceneDocument,
    limits: ProjectLimits,
) -> Result<(), ProjectError> {
    if scene.entities.len() > limits.max_entities {
        return Err(limit_error(
            source_id,
            "entities",
            format!(
                "{} entities exceed the configured limit {}",
                scene.entities.len(),
                limits.max_entities
            ),
        ));
    }
    validate_scene(scene).map_err(|error| validation_issue_error(source_id, error.issues))
}

fn supported_mesh_assets<'a>(
    source_id: &str,
    scene: &'a SceneDocument,
) -> Result<Vec<(usize, &'a AssetDescriptor, String)>, ProjectError> {
    let mut mesh_assets = Vec::new();
    for (index, asset) in scene.assets.iter().enumerate() {
        let path = format!("assets[{index}]");
        if asset.kind.as_str() != "daedalus.mesh" {
            return Err(unsupported(
                source_id,
                format!("{path}.kind"),
                format!("asset kind '{}' is not supported", asset.kind),
            ));
        }
        if !asset.metadata.is_empty() {
            return Err(unsupported(
                source_id,
                format!("{path}.metadata"),
                "mesh asset metadata is not supported by this schema",
            ));
        }
        let source = normalize_relative_path(&asset.uri)
            .map_err(|message| invalid(source_id, format!("{path}.uri"), message))?;
        if let Some(hash) = &asset.content_hash {
            parse_declared_hash(hash)
                .map_err(|message| invalid(source_id, format!("{path}.content_hash"), message))?;
        }
        mesh_assets.push((index, asset, source));
    }
    Ok(mesh_assets)
}

fn legacy_mesh_assets<'a>(
    source_id: &str,
    scene: &'a SceneDocument,
) -> Result<Vec<(usize, &'a AssetDescriptor, String)>, ProjectError> {
    let mut mesh_assets = Vec::new();
    for (index, asset) in scene.assets.iter().enumerate() {
        if asset.kind.as_str() != "daedalus.mesh" {
            continue;
        }
        let path = format!("assets[{index}]");
        if !asset.metadata.is_empty() {
            return Err(unsupported(
                source_id,
                format!("{path}.metadata"),
                "legacy mesh metadata is not supported",
            ));
        }
        let source = normalize_relative_path(&asset.uri)
            .map_err(|message| invalid(source_id, format!("{path}.uri"), message))?;
        if let Some(hash) = &asset.content_hash {
            parse_declared_hash(hash)
                .map_err(|message| invalid(source_id, format!("{path}.content_hash"), message))?;
        }
        mesh_assets.push((index, asset, source));
    }
    Ok(mesh_assets)
}

fn decode_entities(
    source_id: &str,
    scene: &SceneDocument,
    meshes: &BTreeMap<StableId, ProjectMesh>,
) -> Result<DecodedEntityProjection, ProjectError> {
    decode_entities_inner(source_id, scene, meshes, None)
}

fn decode_entities_with_assets(
    source_id: &str,
    scene: &SceneDocument,
    meshes: &BTreeMap<StableId, ProjectMesh>,
    assets: &BTreeMap<StableId, ImportedAsset>,
) -> Result<DecodedEntityProjection, ProjectError> {
    decode_entities_inner(source_id, scene, meshes, Some(assets))
}

fn decode_entities_inner(
    source_id: &str,
    scene: &SceneDocument,
    meshes: &BTreeMap<StableId, ProjectMesh>,
    assets: Option<&BTreeMap<StableId, ImportedAsset>>,
) -> Result<DecodedEntityProjection, ProjectError> {
    let mut prepared = Vec::with_capacity(scene.entities.len());
    let mut model_renderers = BTreeMap::new();
    let mut sprite_renderers = BTreeMap::new();
    let mut camera_2d = None;
    let mut has_sprites = false;
    for (entity_index, entity) in scene.entities.iter().enumerate() {
        let entity_path = format!("entities[{entity_index}]");
        if entity.parent.is_some() {
            return Err(unsupported(
                source_id,
                format!("{entity_path}.parent"),
                "parented entities and hierarchy are not supported by this project loader",
            ));
        }
        let decoded = decode_entity_inner(source_id, entity_index, entity, meshes, assets)?;
        if let Some(binding) = decoded.model_renderer {
            model_renderers.insert(entity.id.clone(), binding);
        }
        if let Some(binding) = decoded.sprite_renderer {
            has_sprites = true;
            sprite_renderers.insert(entity.id.clone(), binding);
        }
        if let Some(binding) = decoded.camera_2d
            && camera_2d.replace(binding).is_some()
        {
            return Err(invalid(
                source_id,
                format!("{entity_path}.components"),
                "a project may declare at most one daedalus.camera_2d component",
            ));
        }
        prepared.push(decoded.entity);
    }
    if has_sprites && camera_2d.is_none() {
        return Err(invalid(
            source_id,
            "entities",
            "a project with daedalus.sprite_renderer components requires exactly one daedalus.camera_2d",
        ));
    }
    Ok(DecodedEntityProjection {
        entities: prepared,
        model_renderers,
        sprite_renderers,
        camera_2d,
    })
}

struct DecodedEntity {
    entity: PreparedEntity,
    model_renderer: Option<PreparedModelRendererBinding>,
    sprite_renderer: Option<PreparedSpriteRendererBinding>,
    camera_2d: Option<PreparedCamera2dBinding>,
}

fn decode_entity_inner(
    source_id: &str,
    entity_index: usize,
    entity: &SceneEntity,
    meshes: &BTreeMap<StableId, ProjectMesh>,
    assets: Option<&BTreeMap<StableId, ImportedAsset>>,
) -> Result<DecodedEntity, ProjectError> {
    let entity_path = format!("entities[{entity_index}]");
    let mut transform = ProjectTransform::default();
    let mut mesh = None;
    let mut material = None;
    let mut model_renderer = None;
    let mut sprite_renderer = None;
    let mut camera_2d = None;
    let mut camera_component_path = None;
    let mut seen = BTreeSet::new();
    for (component_index, component) in entity.components.iter().enumerate() {
        let path = format!("{entity_path}.components[{component_index}]");
        let type_id = component.type_id.as_str();
        if !seen.insert(type_id.to_owned()) {
            return Err(invalid(
                source_id,
                format!("{path}.type_id"),
                format!("duplicate '{type_id}' component on one entity"),
            ));
        }
        match type_id {
            "daedalus.transform" => {
                if !component.asset_refs.is_empty() {
                    return Err(invalid(
                        source_id,
                        format!("{path}.asset_refs"),
                        "transform components cannot reference assets",
                    ));
                }
                transform = decode_component_data(source_id, &path, &component.data)?;
                validate_transform(source_id, &path, &transform)?;
            }
            "daedalus.mesh_renderer" => {
                if model_renderer.is_some() || sprite_renderer.is_some() {
                    return Err(invalid(
                        source_id,
                        format!("{path}.type_id"),
                        "an entity cannot combine mesh, model, or sprite renderer components",
                    ));
                }
                let renderer: MeshRendererData =
                    decode_component_data(source_id, &path, &component.data)?;
                if component.asset_refs.as_slice() != [renderer.mesh.clone()] {
                    return Err(invalid(
                        source_id,
                        format!("{path}.asset_refs"),
                        "asset_refs must contain exactly the same mesh ID as data.mesh",
                    ));
                }
                if !meshes.contains_key(&renderer.mesh) {
                    return Err(invalid(
                        source_id,
                        format!("{path}.data.mesh"),
                        format!(
                            "'{}' does not identify a supported mesh asset",
                            renderer.mesh
                        ),
                    ));
                }
                if let Some(value) = &renderer.material {
                    validate_material(source_id, &format!("{path}.data.material"), value)?;
                }
                mesh = Some(renderer.mesh);
                material = renderer.material;
            }
            "daedalus.model_renderer" => {
                let Some(assets) = assets else {
                    return Err(unsupported(
                        source_id,
                        format!("{path}.type_id"),
                        "component type 'daedalus.model_renderer' requires the asset-aware project loader",
                    ));
                };
                if mesh.is_some() || sprite_renderer.is_some() {
                    return Err(invalid(
                        source_id,
                        format!("{path}.type_id"),
                        "an entity cannot combine mesh, model, or sprite renderer components",
                    ));
                }
                let renderer: ModelRendererData =
                    decode_component_data(source_id, &path, &component.data)?;
                if component.asset_refs.as_slice() != [renderer.model.clone()] {
                    return Err(invalid(
                        source_id,
                        format!("{path}.asset_refs"),
                        "asset_refs must contain exactly the same model ID as data.model",
                    ));
                }
                match assets.get(&renderer.model) {
                    Some(ImportedAsset::Model(_)) => {}
                    Some(asset) => {
                        return Err(invalid(
                            source_id,
                            format!("{path}.data.model"),
                            format!(
                                "'{}' identifies a {:?} asset, not a static model",
                                renderer.model,
                                asset.kind()
                            ),
                        ));
                    }
                    None => {
                        return Err(invalid(
                            source_id,
                            format!("{path}.data.model"),
                            format!(
                                "'{}' does not identify a prepared model asset",
                                renderer.model
                            ),
                        ));
                    }
                }
                model_renderer = Some(PreparedModelRendererBinding {
                    component_id: component.id.clone(),
                    model_asset_id: renderer.model,
                });
            }
            "daedalus.sprite_renderer" => {
                let Some(assets) = assets else {
                    return Err(unsupported(
                        source_id,
                        format!("{path}.type_id"),
                        "component type 'daedalus.sprite_renderer' requires the asset-aware project loader",
                    ));
                };
                if mesh.is_some() || model_renderer.is_some() {
                    return Err(invalid(
                        source_id,
                        format!("{path}.type_id"),
                        "an entity cannot combine mesh, model, or sprite renderer components",
                    ));
                }
                let renderer: SpriteRendererData =
                    decode_component_data(source_id, &path, &component.data)?;
                if component.asset_refs.as_slice() != [renderer.atlas.clone()] {
                    return Err(invalid(
                        source_id,
                        format!("{path}.asset_refs"),
                        "asset_refs must contain exactly the same atlas ID as data.atlas",
                    ));
                }
                if !renderer
                    .tint
                    .iter()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
                {
                    return Err(invalid(
                        source_id,
                        format!("{path}.data.tint"),
                        "sprite tint components must be finite values in [0, 1]",
                    ));
                }
                match assets.get(&renderer.atlas) {
                    Some(ImportedAsset::Atlas(atlas)) => {
                        if !atlas
                            .regions()
                            .iter()
                            .any(|region| region.id() == &renderer.region)
                        {
                            return Err(invalid(
                                source_id,
                                format!("{path}.data.region"),
                                format!(
                                    "'{}' does not identify a region in atlas '{}'",
                                    renderer.region, renderer.atlas
                                ),
                            ));
                        }
                    }
                    Some(asset) => {
                        return Err(invalid(
                            source_id,
                            format!("{path}.data.atlas"),
                            format!(
                                "'{}' identifies a {:?} asset, not a sprite atlas",
                                renderer.atlas,
                                asset.kind()
                            ),
                        ));
                    }
                    None => {
                        return Err(invalid(
                            source_id,
                            format!("{path}.data.atlas"),
                            format!(
                                "'{}' does not identify a prepared sprite atlas",
                                renderer.atlas
                            ),
                        ));
                    }
                }
                sprite_renderer = Some(PreparedSpriteRendererBinding {
                    component_id: component.id.clone(),
                    atlas_asset_id: renderer.atlas,
                    region_id: renderer.region,
                    tint: renderer.tint,
                    sorting_layer: renderer.sorting_layer,
                    order_in_layer: renderer.order_in_layer,
                });
            }
            "daedalus.camera_2d" => {
                if !component.asset_refs.is_empty() {
                    return Err(invalid(
                        source_id,
                        format!("{path}.asset_refs"),
                        "camera2d components cannot reference assets",
                    ));
                }
                let camera: Camera2dData =
                    decode_component_data(source_id, &path, &component.data)?;
                validate_camera_2d_data(source_id, &path, &camera)?;
                camera_component_path = Some(path.clone());
                camera_2d = Some(PreparedCamera2dBinding {
                    entity_id: entity.id.clone(),
                    component_id: component.id.clone(),
                    vertical_size: camera.vertical_size,
                    near: camera.near,
                    far: camera.far,
                });
            }
            _ => {
                return Err(unsupported(
                    source_id,
                    format!("{path}.type_id"),
                    format!("component type '{type_id}' is not supported"),
                ));
            }
        }
    }
    if let Some(path) = camera_component_path.as_deref() {
        if mesh.is_some() || model_renderer.is_some() || sprite_renderer.is_some() {
            return Err(invalid(
                source_id,
                format!("{path}.type_id"),
                "an entity cannot combine daedalus.camera_2d with a renderer component",
            ));
        }
        validate_camera_2d_transform(source_id, path, &transform)?;
    }
    Ok(DecodedEntity {
        entity: PreparedEntity {
            id: entity.id.clone(),
            name: entity.name.clone(),
            transform,
            mesh,
            material,
        },
        model_renderer,
        sprite_renderer,
        camera_2d,
    })
}

fn validate_camera_2d_data(
    source_id: &str,
    component_path: &str,
    camera: &Camera2dData,
) -> Result<(), ProjectError> {
    if !camera.vertical_size.is_finite()
        || !(0.001..=1_000_000.0).contains(&camera.vertical_size)
        || !camera.near.is_finite()
        || !camera.far.is_finite()
        || camera.near <= 0.0
        || camera.far <= camera.near
    {
        return Err(invalid(
            source_id,
            format!("{component_path}.data"),
            "camera2d requires finite 0.001 <= vertical_size <= 1000000 and 0 < near < far",
        ));
    }
    Ok(())
}

fn validate_camera_2d_transform(
    source_id: &str,
    component_path: &str,
    transform: &ProjectTransform,
) -> Result<(), ProjectError> {
    validate_transform(source_id, component_path, transform)?;
    if transform
        .scale
        .iter()
        .any(|scale| (*scale - 1.0).abs() > 1.0e-6)
    {
        return Err(invalid(
            source_id,
            format!("{component_path}.transform.scale"),
            "camera2d transform scale must be unit within 1e-6 on every axis",
        ));
    }
    Ok(())
}

fn validate_camera_presentation_compatibility(
    _scene_source: &str,
    manifest: &ProjectManifest,
    camera_2d: Option<&PreparedCamera2dBinding>,
) -> Result<(), ProjectError> {
    if camera_2d.is_some()
        && manifest
            .presentation
            .as_ref()
            .is_some_and(|presentation| presentation.camera.is_some())
    {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "presentation.camera",
            "presentation.camera cannot be combined with a daedalus.camera_2d component",
        ));
    }
    Ok(())
}

fn decode_component_data<T: for<'de> Deserialize<'de>>(
    source_id: &str,
    component_path: &str,
    data: &Value,
) -> Result<T, ProjectError> {
    serde_json::from_value(data.clone()).map_err(|error| {
        invalid(
            source_id,
            format!("{component_path}.data"),
            error.to_string(),
        )
    })
}

fn validate_transform(
    source_id: &str,
    path: &str,
    transform: &ProjectTransform,
) -> Result<(), ProjectError> {
    let finite = transform
        .translation
        .iter()
        .chain(transform.rotation.iter())
        .chain(transform.scale.iter())
        .all(|value| value.is_finite());
    let quaternion_length = transform
        .rotation
        .iter()
        .map(|value| value * value)
        .sum::<f32>();
    if !finite
        || !quaternion_length.is_finite()
        || quaternion_length < 1.0e-12
        || transform.scale.iter().any(|scale| scale.abs() < 1.0e-8)
    {
        return Err(invalid(
            source_id,
            format!("{path}.data"),
            "transform must be finite with a nonzero quaternion and nondegenerate scale",
        ));
    }
    Ok(())
}

fn validate_material(
    source_id: &str,
    path: &str,
    material: &ProjectMaterial,
) -> Result<(), ProjectError> {
    if !material
        .base_color
        .iter()
        .chain(material.emissive.iter())
        .all(|value| value.is_finite() && *value >= 0.0)
        || !material.metallic.is_finite()
        || !(0.0..=1.0).contains(&material.metallic)
        || !material.roughness.is_finite()
        || !(0.0..=1.0).contains(&material.roughness)
    {
        return Err(invalid(
            source_id,
            path,
            "material colors must be finite and nonnegative; metallic and roughness must be in [0, 1]",
        ));
    }
    Ok(())
}

fn validate_mesh(
    source_id: &str,
    mesh: &ProjectMesh,
    limits: ProjectLimits,
) -> Result<(), ProjectError> {
    if mesh.schema_version != 1 {
        return Err(invalid(
            source_id,
            "schema_version",
            format!(
                "unsupported mesh schema version {} (supported: 1)",
                mesh.schema_version
            ),
        ));
    }
    if mesh.vertices.is_empty() || mesh.indices.is_empty() || !mesh.indices.len().is_multiple_of(3)
    {
        return Err(invalid(
            source_id,
            "geometry",
            "mesh needs vertices and a nonempty triangle index list divisible by three",
        ));
    }
    if mesh.vertices.len() > limits.max_vertices {
        return Err(limit_error(
            source_id,
            "vertices",
            format!(
                "{} vertices exceed limit {}",
                mesh.vertices.len(),
                limits.max_vertices
            ),
        ));
    }
    if mesh.indices.len() > limits.max_indices {
        return Err(limit_error(
            source_id,
            "indices",
            format!(
                "{} indices exceed limit {}",
                mesh.indices.len(),
                limits.max_indices
            ),
        ));
    }
    for (index, vertex) in mesh.vertices.iter().enumerate() {
        if !vertex
            .position
            .iter()
            .chain(vertex.normal.iter())
            .all(|value| value.is_finite())
            || vertex.normal.iter().all(|value| value.abs() < 1.0e-6)
        {
            return Err(invalid(
                source_id,
                format!("vertices[{index}]"),
                "vertex position and nonzero normal must contain finite values",
            ));
        }
    }
    for (index, vertex_index) in mesh.indices.iter().enumerate() {
        if *vertex_index as usize >= mesh.vertices.len() {
            return Err(invalid(
                source_id,
                format!("indices[{index}]"),
                format!("vertex index {vertex_index} is outside the vertex array"),
            ));
        }
    }
    Ok(())
}

fn validate_prepared_project(
    project: &PreparedProject,
    limits: ProjectLimits,
) -> Result<(), ProjectError> {
    limits.validate()?;
    validate_manifest(&project.manifest)?;
    let scene_source = normalize_relative_path(&project.manifest.scene)
        .map_err(|message| invalid(PROJECT_MANIFEST_FILE, "scene", message))?;
    validate_scene_document(&scene_source, &project.scene, limits)?;
    if project.asset_catalog.is_some() {
        return validate_prepared_asset_project(project, &scene_source);
    }
    let mesh_assets = supported_mesh_assets(&scene_source, &project.scene)?;
    if mesh_assets.len() > limits.max_mesh_assets {
        return Err(limit_error(
            &scene_source,
            "assets",
            format!(
                "{} mesh assets exceed the configured limit",
                mesh_assets.len()
            ),
        ));
    }

    let expected_mesh_ids: BTreeSet<StableId> = mesh_assets
        .iter()
        .map(|(_, asset, _)| asset.id.clone())
        .collect();
    let actual_mesh_ids: BTreeSet<StableId> = project.meshes.keys().cloned().collect();
    if expected_mesh_ids != actual_mesh_ids {
        return Err(invalid(
            &scene_source,
            "assets",
            "prepared mesh IDs do not match the scene's mesh asset IDs",
        ));
    }
    let expected = decode_entities(&scene_source, &project.scene, &project.meshes)?;
    validate_camera_presentation_compatibility(
        &scene_source,
        &project.manifest,
        expected.camera_2d.as_ref(),
    )?;
    if expected.entities != project.entities
        || expected.model_renderers != project.model_renderers
        || !expected.sprite_renderers.is_empty()
        || expected.camera_2d != project.camera_2d
    {
        return Err(invalid(
            &scene_source,
            "entities",
            "prepared entity, renderer, or camera bindings do not match the legacy scene components",
        ));
    }

    let mut vertex_total = 0usize;
    let mut index_total = 0usize;
    for mesh in project.meshes.values() {
        validate_mesh("<prepared mesh>", mesh, limits)?;
        vertex_total = vertex_total
            .checked_add(mesh.vertices.len())
            .ok_or_else(|| limit_error(&scene_source, "assets", "vertex count overflow"))?;
        index_total = index_total
            .checked_add(mesh.indices.len())
            .ok_or_else(|| limit_error(&scene_source, "assets", "index count overflow"))?;
    }
    if vertex_total > limits.max_vertices || index_total > limits.max_indices {
        return Err(limit_error(
            &scene_source,
            "assets",
            format!(
                "prepared geometry has {vertex_total} vertices and {index_total} indices, exceeding configured limits"
            ),
        ));
    }

    let expected_source_ids: BTreeSet<String> = std::iter::once(PROJECT_MANIFEST_FILE.to_owned())
        .chain(std::iter::once(scene_source.clone()))
        .chain(mesh_assets.iter().map(|(_, _, source)| source.clone()))
        .collect();
    let mut actual_source_ids = BTreeSet::new();
    let mut total_bytes = 0u64;
    for file in &project.files {
        let normalized = normalize_relative_path(&file.source_id)
            .map_err(|message| invalid(&file.source_id, "files.source_id", message))?;
        if normalized != file.source_id {
            return Err(invalid(
                &file.source_id,
                "files.source_id",
                "source IDs must be normalized project-relative paths",
            ));
        }
        if !is_lowercase_sha256(&file.content_sha256) {
            return Err(invalid(
                &file.source_id,
                "files.content_sha256",
                "file identity must contain a lowercase 64-digit SHA-256",
            ));
        }
        if !actual_source_ids.insert(file.source_id.clone()) {
            return Err(invalid(
                &file.source_id,
                "files.source_id",
                "duplicate file source identity",
            ));
        }
        let per_file_limit = if file.source_id == PROJECT_MANIFEST_FILE {
            limits.max_manifest_bytes
        } else if file.source_id == scene_source {
            limits.max_scene_bytes
        } else {
            limits.max_mesh_bytes
        };
        if file.byte_len == 0 || file.byte_len > per_file_limit {
            return Err(limit_error(
                &file.source_id,
                "files.byte_len",
                format!(
                    "source length {} is outside 1..={per_file_limit}",
                    file.byte_len
                ),
            ));
        }
        total_bytes = total_bytes
            .checked_add(file.byte_len)
            .ok_or_else(|| limit_error(&file.source_id, "files.byte_len", "byte count overflow"))?;
    }
    if actual_source_ids != expected_source_ids {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "files",
            "prepared file identities do not match the manifest, scene, and mesh sources",
        ));
    }
    if project.files.len() > limits.max_files || total_bytes > limits.max_total_bytes {
        return Err(limit_error(
            PROJECT_MANIFEST_FILE,
            "files",
            format!(
                "prepared sources have {} files and {total_bytes} bytes, exceeding configured limits",
                project.files.len()
            ),
        ));
    }

    for (asset_index, asset, source) in mesh_assets {
        if let Some(expected) = &asset.content_hash {
            let expected_hash = parse_declared_hash(expected).map_err(|message| {
                invalid(
                    &scene_source,
                    format!("assets[{asset_index}].content_hash"),
                    message,
                )
            })?;
            let file = project
                .files
                .iter()
                .find(|file| file.source_id == source)
                .ok_or_else(|| {
                    invalid(
                        &scene_source,
                        format!("assets[{asset_index}].uri"),
                        "mesh asset has no recorded source identity",
                    )
                })?;
            if expected_hash != file.content_sha256 {
                return Err(invalid(
                    &scene_source,
                    format!("assets[{asset_index}].content_hash"),
                    format!("declared SHA-256 does not match source '{source}'"),
                ));
            }
        }
    }
    if project.digest != project_digest(&project.files) {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "digest",
            "project digest does not match its recorded source identities",
        ));
    }
    Ok(())
}

fn validate_prepared_asset_project(
    project: &PreparedProject,
    scene_source: &str,
) -> Result<(), ProjectError> {
    let catalog = project
        .asset_catalog
        .as_ref()
        .ok_or_else(|| invalid(scene_source, "assets", "asset catalog is missing"))?;
    let mesh_assets = legacy_mesh_assets(scene_source, &project.scene)?;
    let expected_mesh_ids = mesh_assets
        .iter()
        .map(|(_, asset, _)| asset.id.clone())
        .collect::<BTreeSet<_>>();
    let actual_mesh_ids = project.meshes.keys().cloned().collect::<BTreeSet<_>>();
    if expected_mesh_ids != actual_mesh_ids {
        return Err(invalid(
            scene_source,
            "assets",
            "prepared legacy mesh IDs do not match scene mesh descriptors",
        ));
    }
    let expected_typed_ids = project
        .scene
        .assets
        .iter()
        .filter(|asset| asset.kind.as_str() != "daedalus.mesh")
        .map(|asset| asset.id.clone())
        .collect::<BTreeSet<_>>();
    let actual_typed_ids = catalog
        .assets()
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    if expected_typed_ids != actual_typed_ids {
        return Err(invalid(
            scene_source,
            "assets",
            "prepared typed asset IDs do not match scene descriptors",
        ));
    }
    let expected = decode_entities_with_assets(
        scene_source,
        &project.scene,
        &project.meshes,
        catalog.assets_map(),
    )?;
    validate_camera_presentation_compatibility(
        scene_source,
        &project.manifest,
        expected.camera_2d.as_ref(),
    )?;
    if expected.entities != project.entities
        || expected.model_renderers != project.model_renderers
        || expected.sprite_renderers != project.sprite_renderers
        || expected.camera_2d != project.camera_2d
    {
        return Err(invalid(
            scene_source,
            "entities",
            "prepared entity, renderer, or camera projection does not match the full authored scene",
        ));
    }
    let mut total_bytes = 0u64;
    let mut seen_sources = BTreeSet::new();
    let import_limits = ImportLimits::default();
    let mut source_limits = BTreeMap::<String, u64>::from([
        (
            PROJECT_MANIFEST_FILE.to_owned(),
            (64 * 1024).min(import_limits.max_source_bytes),
        ),
        (
            scene_source.to_owned(),
            (16 * 1024 * 1024).min(import_limits.max_source_bytes),
        ),
    ]);
    for descriptor in &project.scene.assets {
        let source = normalize_relative_path(&descriptor.uri)
            .map_err(|message| invalid(scene_source, "assets.uri", message))?;
        let limit = if descriptor.kind.as_str() == "daedalus.mesh" {
            MAX_MESH_BYTES
        } else {
            import_limits.max_source_bytes
        };
        source_limits
            .entry(source)
            .and_modify(|existing| *existing = (*existing).min(limit))
            .or_insert(limit);
    }
    for file in &project.files {
        let normalized = normalize_relative_path(&file.source_id)
            .map_err(|message| invalid(&file.source_id, "files.source_id", message))?;
        let per_file_limit = source_limits
            .get(&file.source_id)
            .copied()
            .unwrap_or(import_limits.max_source_bytes);
        if normalized != file.source_id
            || !is_lowercase_sha256(&file.content_sha256)
            || file.byte_len == 0
            || file.byte_len > per_file_limit
        {
            return Err(invalid(
                &file.source_id,
                "files",
                "source identity is malformed or outside hard limits",
            ));
        }
        if !seen_sources.insert(file.source_id.clone()) {
            return Err(invalid(
                &file.source_id,
                "files",
                "duplicate source identity",
            ));
        }
        total_bytes = total_bytes.checked_add(file.byte_len).ok_or_else(|| {
            limit_error(
                &file.source_id,
                "files.byte_len",
                "source byte count overflow",
            )
        })?;
    }
    if seen_sources.len() > import_limits.max_unique_sources
        || total_bytes > import_limits.max_total_source_bytes
    {
        return Err(limit_error(
            PROJECT_MANIFEST_FILE,
            "files",
            "prepared sources exceed hard asset-import bounds",
        ));
    }
    for required in [PROJECT_MANIFEST_FILE.to_owned(), scene_source.to_owned()] {
        if !seen_sources.contains(&required) {
            return Err(invalid(
                &required,
                "files",
                "manifest or scene source identity is missing",
            ));
        }
    }
    for (_, descriptor, source) in mesh_assets {
        if !seen_sources.contains(&source) {
            return Err(invalid(
                scene_source,
                "files",
                format!("legacy mesh source '{source}' is missing"),
            ));
        }
        if let Some(hash) = &descriptor.content_hash {
            let declared = parse_declared_hash(hash)
                .map_err(|message| invalid(scene_source, "assets.content_hash", message))?;
            let identity = project
                .files
                .iter()
                .find(|file| file.source_id == source)
                .expect("source presence checked");
            if declared != identity.content_sha256 {
                return Err(invalid(
                    scene_source,
                    "assets.content_hash",
                    "prepared source does not match its declared content hash",
                ));
            }
        }
    }
    for descriptor in &project.scene.assets {
        if descriptor.kind.as_str() == "daedalus.mesh" {
            continue;
        }
        let source = normalize_relative_path(&descriptor.uri)
            .map_err(|message| invalid(scene_source, "assets.uri", message))?;
        if !seen_sources.contains(&source) {
            return Err(invalid(
                scene_source,
                "files",
                format!("asset source '{source}' is missing"),
            ));
        }
        if let Some(hash) = &descriptor.content_hash {
            let declared = parse_declared_hash(hash)
                .map_err(|message| invalid(scene_source, "assets.content_hash", message))?;
            let identity = project
                .files
                .iter()
                .find(|file| file.source_id == source)
                .expect("source presence checked");
            if declared != identity.content_sha256 {
                return Err(invalid(
                    scene_source,
                    "assets.content_hash",
                    "prepared source does not match its declared content hash",
                ));
            }
        }
    }
    if catalog.sources() != project.files || project_digest(&project.files) != project.digest {
        return Err(invalid(
            PROJECT_MANIFEST_FILE,
            "files",
            "prepared catalog sources or project digest do not match",
        ));
    }
    for mesh in project.meshes.values() {
        validate_mesh("<prepared mesh>", mesh, ProjectLimits::default())?;
    }
    Ok(())
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    source_id: &str,
    bytes: &[u8],
) -> Result<T, ProjectError> {
    serde_json::from_slice(bytes).map_err(|error| ProjectError::Json {
        source_id: source_id.into(),
        line: error.line(),
        column: error.column(),
        message: error.to_string(),
    })
}

fn verify_declared_hash(
    source_id: &str,
    path: &str,
    asset: &AssetDescriptor,
    bytes: &[u8],
) -> Result<(), ProjectError> {
    let actual: [u8; 32] = Sha256::digest(bytes).into();
    verify_declared_hash_digest(source_id, path, asset, &actual)
}

fn verify_declared_hash_digest(
    source_id: &str,
    path: &str,
    asset: &AssetDescriptor,
    actual_digest: &[u8; 32],
) -> Result<(), ProjectError> {
    let Some(declared) = &asset.content_hash else {
        return Ok(());
    };
    let expected = parse_declared_hash(declared)
        .map_err(|message| invalid(source_id, format!("{path}.content_hash"), message))?;
    let actual = encode_hex(actual_digest);
    if expected != actual {
        return Err(invalid(
            source_id,
            format!("{path}.content_hash"),
            format!("declared SHA-256 does not match source '{}'", asset.uri),
        ));
    }
    Ok(())
}

fn parse_declared_hash(hash: &str) -> Result<&str, String> {
    let Some(value) = hash.strip_prefix("sha256:") else {
        return Err("content_hash must use sha256:<64 lowercase hex digits>".into());
    };
    if !is_lowercase_sha256(value) {
        return Err("content_hash must use sha256:<64 lowercase hex digits>".into());
    }
    Ok(value)
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn file_identity(source_id: &str, bytes: &[u8]) -> ProjectFileIdentity {
    ProjectFileIdentity {
        source_id: source_id.to_owned(),
        content_sha256: sha256_hex(bytes),
        byte_len: bytes.len() as u64,
    }
}

fn project_file_identity_from_blob(source: &SourceBlob) -> ProjectFileIdentity {
    ProjectFileIdentity {
        source_id: source.source_id().to_string(),
        content_sha256: encode_hex(&source.sha256()),
        byte_len: source.byte_len(),
    }
}

fn estimate_project_binding_metadata_bytes(
    scene: &SceneDocument,
    source_id: &str,
) -> Result<u64, AssetImportError> {
    let mut bytes = 0u64;
    for entity in &scene.entities {
        for component in &entity.components {
            let (id_lengths, id_count, record_bytes) = match component.type_id.as_str() {
                "daedalus.model_renderer" => (
                    [
                        entity.id.as_str().len(),
                        component.id.as_str().len(),
                        component
                            .data
                            .get("model")
                            .and_then(Value::as_str)
                            .map_or(0, |value| value.len().min(128)),
                        0,
                    ],
                    3usize,
                    std::mem::size_of::<(StableId, PreparedModelRendererBinding)>() + 96,
                ),
                "daedalus.sprite_renderer" => (
                    [
                        entity.id.as_str().len(),
                        component.id.as_str().len(),
                        component
                            .data
                            .get("atlas")
                            .and_then(Value::as_str)
                            .map_or(0, |value| value.len().min(128)),
                        component
                            .data
                            .get("region")
                            .and_then(Value::as_str)
                            .map_or(0, |value| value.len().min(128)),
                    ],
                    4usize,
                    std::mem::size_of::<(StableId, PreparedSpriteRendererBinding)>() + 128,
                ),
                "daedalus.camera_2d" => (
                    [entity.id.as_str().len(), component.id.as_str().len(), 0, 0],
                    2usize,
                    std::mem::size_of::<PreparedCamera2dBinding>()
                        + std::mem::size_of::<Option<PreparedCamera2dBinding>>()
                        + 96,
                ),
                _ => continue,
            };
            // StableId stores an owned String. Charge the observed bytes plus the full 128-byte
            // ID ceiling for each retained binding string before any binding clone is made.
            let string_bytes = id_lengths[..id_count]
                .iter()
                .try_fold(0usize, |total, length| {
                    total.checked_add(*length)?.checked_add(128)
                })
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(source_id, "entities.components"),
                        "project renderer binding string metadata size overflow",
                    )
                })?;
            bytes = bytes
                .checked_add(u64::try_from(string_bytes).map_err(|_| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(source_id, "entities.components"),
                        "project renderer binding string metadata is not representable",
                    )
                })?)
                .and_then(|value| value.checked_add(record_bytes as u64))
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        crate::assets::ImportLocation::new(source_id, "entities.components"),
                        "project renderer binding metadata total overflow",
                    )
                })?;
        }
    }
    Ok(bytes)
}

fn estimate_catalog_metadata_bytes(
    assets: &BTreeMap<StableId, ImportedAsset>,
    project_binding_metadata_bytes: u64,
    files: &[ProjectFileIdentity],
    context: &AssetImportContext<'_>,
    cache_pin_count: usize,
) -> Result<u64, AssetImportError> {
    let location = crate::assets::ImportLocation::default();
    let mut total = 0u64;
    {
        let mut add = |amount: u64| -> Result<(), AssetImportError> {
            total = total.checked_add(amount).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "retained catalog metadata byte count overflow",
                )
            })?;
            Ok(())
        };
        let repeated = |count: usize, size: usize| -> Result<u64, AssetImportError> {
            let count = u64::try_from(count).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "retained catalog metadata count is not representable",
                )
            })?;
            let size = u64::try_from(size).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "retained catalog metadata element size is not representable",
                )
            })?;
            count.checked_mul(size).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "retained catalog metadata multiplication overflow",
                )
            })
        };

        (|| -> Result<(), AssetImportError> {
            // PreparedProject files, catalog files, report source summaries, catalog digest, and the
            // report's digest copy remain live after preparation.
            add(repeated(
                files.len(),
                2 * std::mem::size_of::<ProjectFileIdentity>(),
            )?)?;
            for file in files {
                add(
                    u64::try_from(file.source_id.len().saturating_mul(3) + 64 * 3).map_err(
                        |_| {
                            AssetImportError::new(
                                AssetImportErrorCode::LimitExceeded,
                                location.clone(),
                                "project source summary metadata is not representable",
                            )
                        },
                    )?,
                )?;
                add(
                    u64::try_from(std::mem::size_of::<crate::assets::AssetSourceSummary>())
                        .unwrap_or(u64::MAX),
                )?;
            }
            add(2 * std::mem::size_of::<PreparedAssetCatalog>() as u64)?;
            add(2 * std::mem::size_of::<AssetImportReport>() as u64)?;
            add(2 * 64)?;

            // Each stable ID is retained as a catalog map key, dependency map key, catalog summary ID,
            // and report summary ID. Include tree-node and vector slack conservatively per entry.
            add(repeated(
                assets.len(),
                std::mem::size_of::<ImportedAsset>() + std::mem::size_of::<StableId>() + 96,
            )?)?;
            for (id, asset) in assets {
                let id_bytes = u64::try_from(id.as_str().len()).map_err(|_| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        location.clone(),
                        "asset identifier metadata is not representable",
                    )
                })?;
                add(id_bytes.checked_mul(4).ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        location.clone(),
                        "asset identifier metadata multiplication overflow",
                    )
                })?)?;
                add(repeated(
                    2,
                    std::mem::size_of::<crate::assets::AssetSummary>(),
                )?)?;
                add(2 * 64)?; // AssetSummary primary-key strings in the catalog and report.

                let resource_key_count = match asset {
                    ImportedAsset::Model(model) => {
                        let unique_geometry = model
                            .primitives()
                            .iter()
                            .map(|primitive| primitive.geometry().key().digest())
                            .collect::<BTreeSet<_>>()
                            .len();
                        let unique_images = model
                            .owned_images()
                            .iter()
                            .map(|image| image.asset().key().digest())
                            .collect::<BTreeSet<_>>()
                            .len();
                        let mut model_metadata =
                            std::mem::size_of::<crate::assets::StaticModel>() as u64;
                        for amount in [
                            repeated(
                                model.selected_scene_roots().len(),
                                std::mem::size_of::<u32>(),
                            )?,
                            repeated(
                                model.nodes().len(),
                                std::mem::size_of::<crate::assets::ModelNode>() + 16,
                            )?,
                            repeated(
                                model.primitives().len(),
                                std::mem::size_of::<crate::assets::ModelPrimitive>(),
                            )?,
                            repeated(
                                model.materials().len(),
                                std::mem::size_of::<crate::assets::ModelMaterial>(),
                            )?,
                            repeated(
                                model.owned_images().len(),
                                std::mem::size_of::<crate::assets::ModelOwnedImage>(),
                            )?,
                        ] {
                            model_metadata =
                                model_metadata.checked_add(amount).ok_or_else(|| {
                                    AssetImportError::new(
                                        AssetImportErrorCode::LimitExceeded,
                                        location.clone(),
                                        "model metadata byte count overflow",
                                    )
                                })?;
                        }
                        for node in model.nodes() {
                            model_metadata = model_metadata
                                .checked_add(repeated(
                                    node.primitive_indices().len(),
                                    std::mem::size_of::<u32>(),
                                )?)
                                .ok_or_else(|| {
                                    AssetImportError::new(
                                        AssetImportErrorCode::LimitExceeded,
                                        location.clone(),
                                        "model node metadata byte count overflow",
                                    )
                                })?;
                        }
                        for image in model.owned_images() {
                            for amount in [
                                image.identity().owner().as_str().len() as u64,
                                std::mem::size_of::<crate::assets::ImageAsset>() as u64,
                                std::mem::size_of::<crate::assets::DecodedImage>() as u64 + 128,
                                repeated(
                                    image.asset().mips().len(),
                                    std::mem::size_of::<crate::assets::ImageMip>(),
                                )?,
                            ] {
                                model_metadata =
                                    model_metadata.checked_add(amount).ok_or_else(|| {
                                        AssetImportError::new(
                                            AssetImportErrorCode::LimitExceeded,
                                            location.clone(),
                                            "model image metadata byte count overflow",
                                        )
                                    })?;
                            }
                        }
                        add(model_metadata)?;
                        1usize + unique_geometry + unique_images
                    }
                    ImportedAsset::Image(image) => {
                        add(std::mem::size_of::<crate::assets::ImageAsset>() as u64)?;
                        add(std::mem::size_of::<crate::assets::DecodedImage>() as u64 + 128)?;
                        add(repeated(
                            image.mips().len(),
                            std::mem::size_of::<crate::assets::ImageMip>(),
                        )?)?;
                        1
                    }
                    ImportedAsset::Atlas(atlas) => {
                        add(std::mem::size_of::<crate::assets::SpriteAtlas>() as u64)?;
                        add(repeated(
                            atlas.regions().len(),
                            std::mem::size_of::<crate::assets::AtlasRegion>() + 16,
                        )?)?;
                        for region in atlas.regions() {
                            add(region.id().as_str().len() as u64)?;
                        }
                        add(atlas.image_asset().as_str().len() as u64)?;
                        1
                    }
                    ImportedAsset::Audio(_) => {
                        add(std::mem::size_of::<crate::assets::AudioAsset>() as u64)?;
                        1
                    }
                };
                add(repeated(
                    resource_key_count.saturating_mul(2),
                    std::mem::size_of::<crate::assets::CookedResourceKeySummary>() + 64,
                )?)?;
            }

            add(repeated(
                assets.len(),
                std::mem::size_of::<StableId>() + 64,
            )?)?;
            let mut dependency_count = 0usize;
            for asset_id in assets.keys() {
                for dependency_id in context.asset_dependency_ids(asset_id) {
                    dependency_count = dependency_count.checked_add(1).ok_or_else(|| {
                        AssetImportError::new(
                            AssetImportErrorCode::LimitExceeded,
                            location.clone(),
                            "asset dependency count overflow",
                        )
                    })?;
                    add((dependency_id.len() as u64).checked_mul(3).ok_or_else(|| {
                        AssetImportError::new(
                            AssetImportErrorCode::LimitExceeded,
                            location.clone(),
                            "dependency source identity metadata overflow",
                        )
                    })?)?;
                }
            }

            // Binding IDs are estimated from borrowed scene strings before the prepared map is
            // allocated, then charged here with the rest of retained catalog metadata.
            add(project_binding_metadata_bytes)?;
            let copied_dependencies = dependency_count.checked_mul(3).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "asset dependency summary count overflow",
                )
            })?;
            add(repeated(
                copied_dependencies,
                std::mem::size_of::<AssetDependencySummary>() + 64,
            )?)?;
            add(repeated(
                cache_pin_count,
                std::mem::size_of::<crate::assets::CachePin>() + 64,
            )?)?;
            Ok(())
        })()?;
    }
    Ok(total)
}

fn sha256_hex(bytes: &[u8]) -> String {
    encode_hex(&Sha256::digest(bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn project_digest(files: &[ProjectFileIdentity]) -> String {
    let mut ordered: Vec<&ProjectFileIdentity> = files.iter().collect();
    ordered.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let mut hasher = Sha256::new();
    hasher.update(b"daedalus-project-source-identity-v1\0");
    for file in ordered {
        hasher.update((file.source_id.len() as u64).to_le_bytes());
        hasher.update(file.source_id.as_bytes());
        hasher.update(file.byte_len.to_le_bytes());
        hasher.update(file.content_sha256.as_bytes());
    }
    format!("sha256:{}", encode_hex(&hasher.finalize()))
}

fn normalize_relative_path(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err("path must not be empty".into());
    }
    if value.contains('\\') || value.contains('\0') {
        return Err("path must use portable forward slashes and contain no NUL".into());
    }
    if has_uri_scheme(value) {
        return Err("URI schemes are not allowed for project sources".into());
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err("path must be project-root relative".into());
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| "path must be valid UTF-8".to_owned())?;
                parts.push(part.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err("parent traversal ('..') is not allowed".into()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("path must be project-root relative".into());
            }
        }
    }
    if parts.is_empty() {
        return Err("path must identify a file below the project root".into());
    }
    Ok(parts.join("/"))
}

fn has_uri_scheme(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
}

fn read_project_file(
    root: &Path,
    source_id: &str,
    max_file_bytes: u64,
    remaining_total_bytes: u64,
) -> Result<Vec<u8>, ProjectError> {
    let normalized =
        normalize_relative_path(source_id).map_err(|message| path_error(source_id, message))?;
    let candidate = root.join(&normalized);
    let canonical = candidate.canonicalize().map_err(|error| ProjectError::Io {
        source_id: normalized.clone(),
        message: error.to_string(),
    })?;
    if !canonical.starts_with(root) {
        return Err(path_error(
            &normalized,
            "canonical target resolves outside the project root",
        ));
    }
    if !canonical.is_file() {
        return Err(path_error(&normalized, "source must be a regular file"));
    }
    let read_limit = max_file_bytes.min(remaining_total_bytes);
    let mut file = File::open(&canonical).map_err(|error| ProjectError::Io {
        source_id: normalized.clone(),
        message: error.to_string(),
    })?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(read_limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ProjectError::Io {
            source_id: normalized.clone(),
            message: error.to_string(),
        })?;
    if bytes.len() as u64 > read_limit {
        let ceiling = if remaining_total_bytes < max_file_bytes {
            "remaining total byte budget"
        } else {
            "per-file byte budget"
        };
        return Err(limit_error(
            &normalized,
            "",
            format!("source exceeds the {ceiling} of {read_limit} bytes"),
        ));
    }
    Ok(bytes)
}

fn enforce_size(
    source_id: &str,
    path: &str,
    actual: usize,
    limit: u64,
) -> Result<(), ProjectError> {
    if actual as u64 > limit {
        return Err(limit_error(
            source_id,
            path,
            format!("source uses {actual} bytes, exceeding limit {limit}"),
        ));
    }
    Ok(())
}

fn validation_issue_error(source_id: &str, issues: Vec<ValidationIssue>) -> ProjectError {
    let issue = issues
        .into_iter()
        .next()
        .expect("validation error contains at least one issue");
    invalid(source_id, issue.path, issue.message)
}

fn invalid(
    source_id: impl Into<String>,
    path: impl Into<String>,
    message: impl Into<String>,
) -> ProjectError {
    ProjectError::Invalid {
        source_id: source_id.into(),
        path: path.into(),
        message: message.into(),
    }
}

fn unsupported(
    source_id: impl Into<String>,
    path: impl Into<String>,
    message: impl Into<String>,
) -> ProjectError {
    ProjectError::Unsupported {
        source_id: source_id.into(),
        path: path.into(),
        message: message.into(),
    }
}

fn limit_error(
    source_id: impl Into<String>,
    path: impl Into<String>,
    message: impl Into<String>,
) -> ProjectError {
    ProjectError::LimitExceeded {
        source_id: source_id.into(),
        path: path.into(),
        message: message.into(),
    }
}

fn path_error(source_id: impl Into<String>, message: impl Into<String>) -> ProjectError {
    ProjectError::InvalidPath {
        source_id: source_id.into(),
        message: message.into(),
    }
}

fn subtract(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn length_squared(vector: [f32; 3]) -> f32 {
    vector.iter().map(|value| value * value).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "daedalus-project-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = File::create(path).unwrap();
            file.write_all(bytes).unwrap();
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(scene: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "external-test",
            "scene": scene
        }))
        .unwrap()
    }

    fn empty_scene() -> Vec<u8> {
        serde_json::to_vec(&SceneDocument::default()).unwrap()
    }

    fn minimal_model_sources() -> BTreeMap<String, Vec<u8>> {
        let mut buffer = Vec::new();
        for value in [
            0.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // POSITION
            0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0, // NORMAL
        ] {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        let gltf = serde_json::to_vec(&json!({
            "asset": {"version": "2.0"},
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "NORMAL": 1}}]}],
            "buffers": [{"uri": "triangle.bin", "byteLength": buffer.len()}],
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
            ("models/triangle.bin".to_owned(), buffer),
        ])
    }

    fn png_rgba(pixel: [u8; 4]) -> Vec<u8> {
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

    fn model_renderer_component(id: &str, refs: Value, data: Value) -> Value {
        json!({
            "id": id,
            "type_id": "daedalus.model_renderer",
            "asset_refs": refs,
            "data": data
        })
    }

    fn sprite_renderer_component(id: &str, refs: Value, data: Value) -> Value {
        json!({
            "id": id,
            "type_id": "daedalus.sprite_renderer",
            "asset_refs": refs,
            "data": data
        })
    }

    fn camera_2d_component(id: &str, refs: Value, data: Value) -> Value {
        json!({
            "id": id,
            "type_id": "daedalus.camera_2d",
            "asset_refs": refs,
            "data": data
        })
    }

    fn transform_component(id: &str, scale: [f32; 3]) -> Value {
        json!({
            "id": id,
            "type_id": "daedalus.transform",
            "asset_refs": [],
            "data": {
                "translation": [0.0, 0.0, 0.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": scale
            }
        })
    }

    fn sprite_project_scene_bytes() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 9,
            "entities": [
                {
                    "id": "entity:camera",
                    "components": [
                        transform_component("component:camera-transform", [1.0, 1.0, 1.0]),
                        camera_2d_component(
                            "component:camera2d",
                            json!([]),
                            json!({"vertical_size": 10.0, "near": 0.1, "far": 100.0})
                        )
                    ]
                },
                {
                    "id": "entity:hero",
                    "components": [
                        transform_component("component:sprite-transform", [1.0, 1.0, 1.0]),
                        sprite_renderer_component(
                            "component:sprite",
                            json!(["asset:atlas"]),
                            json!({
                                "atlas": "asset:atlas",
                                "region": "region:hero",
                                "tint": [0.25, 0.5, 1.0, 0.75],
                                "sorting_layer": -2,
                                "order_in_layer": 8
                            })
                        )
                    ]
                }
            ],
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
                            "mipmap_filter": "none"
                        }
                    }
                },
                {
                    "id": "asset:atlas",
                    "kind": "daedalus.sprite_atlas",
                    "uri": "sprites/sheet.atlas.json",
                    "metadata": {"schema_version": 1}
                }
            ]
        }))
        .unwrap()
    }

    fn sprite_project_sources() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("sprites/sheet.png".into(), png_rgba([240, 80, 20, 255])),
            (
                "sprites/sheet.atlas.json".into(),
                serde_json::to_vec(&json!({
                    "schema_version": 1,
                    "image_asset": "asset:image",
                    "filter_policy": "nearest_no_mip_v1",
                    "regions": [{
                        "id": "region:hero",
                        "rect": [0, 0, 1, 1],
                        "pivot": [0.5, 0.0],
                        "pixels_per_unit": 16.0
                    }]
                }))
                .unwrap(),
            ),
        ])
    }

    fn dummy_atlas_asset() -> ImportedAsset {
        let atlas = crate::assets::SpriteAtlas::new(
            StableId::new("asset:image").unwrap(),
            vec![crate::assets::AtlasRegion::new(
                StableId::new("region:hero").unwrap(),
                [0, 0, 1, 1],
                [0.5, 0.0],
                16.0,
            )],
            crate::assets::LeafCookKey::from_digest([8; 32]),
            &ImportLimits::default(),
            1,
            1,
            crate::assets::ImportLocation::default(),
        )
        .unwrap();
        ImportedAsset::Atlas(Arc::new(atlas))
    }

    fn scene_with_components(components: Vec<Value>) -> SceneDocument {
        serde_json::from_value(json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [{
                "id": "entity:hero",
                "components": components
            }],
            "assets": []
        }))
        .unwrap()
    }

    fn scene_with_sprite_and_camera(sprite_components: Vec<Value>) -> SceneDocument {
        serde_json::from_value(json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [
                {
                    "id": "entity:camera",
                    "components": [camera_2d_component(
                        "component:camera2d",
                        json!([]),
                        json!({"vertical_size": 10.0, "near": 0.1, "far": 100.0})
                    )]
                },
                {
                    "id": "entity:hero",
                    "components": sprite_components
                }
            ],
            "assets": []
        }))
        .unwrap()
    }

    fn dummy_model_asset() -> ImportedAsset {
        let model = crate::assets::StaticModel::new(
            crate::assets::LeafCookKey::from_digest([7; 32]),
            vec![0],
            vec![crate::assets::ModelNode::new(
                None,
                [
                    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
                ],
                Vec::new(),
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &ImportLimits::default(),
            crate::assets::ImportLocation::default(),
        )
        .unwrap();
        ImportedAsset::Model(Arc::new(model))
    }

    fn model_project_scene(with_image: bool) -> Vec<u8> {
        let mut assets = vec![json!({
            "id": "asset:model",
            "kind": "daedalus.model",
            "uri": "models/triangle.gltf",
            "metadata": {}
        })];
        if with_image {
            assets.push(json!({
                "id": "asset:image",
                "kind": "daedalus.image",
                "uri": "sprites/pixel.png",
                "metadata": {}
            }));
        }
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 3,
            "entities": [{
                "id": "entity:hero",
                "name": "Hero",
                "components": [
                    {
                        "id": "component:transform",
                        "type_id": "daedalus.transform",
                        "asset_refs": [],
                        "data": {
                            "translation": [1.0, 2.0, 3.0],
                            "rotation": [0.0, 0.0, 0.0, 1.0],
                            "scale": [1.0, 1.0, 1.0]
                        }
                    },
                    {
                        "id": "component:model-renderer",
                        "type_id": "daedalus.model_renderer",
                        "asset_refs": ["asset:model"],
                        "data": {"model": "asset:model"}
                    }
                ]
            }],
            "assets": assets
        }))
        .unwrap()
    }

    fn triangle_mesh() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "vertices": [
                {"position": [0.0, 0.0, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [1.0, 0.0, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [0.0, 1.0, 0.0], "normal": [0.0, 0.0, 1.0]}
            ],
            "indices": [0, 1, 2]
        }))
        .unwrap()
    }

    fn mesh_scene(hash: Option<String>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 7,
            "entities": [{
                "id": "entity:triangle",
                "name": "Triangle",
                "components": [
                    {
                        "id": "component:transform",
                        "type_id": "daedalus.transform",
                        "asset_refs": [],
                        "data": {
                            "translation": [2.0, 3.0, 4.0],
                            "rotation": [0.0, 0.0, 0.0, 1.0],
                            "scale": [1.0, 2.0, 1.0]
                        }
                    },
                    {
                        "id": "component:renderer",
                        "type_id": "daedalus.mesh_renderer",
                        "asset_refs": ["asset:triangle"],
                        "data": {
                            "mesh": "asset:triangle",
                            "material": {
                                "base_color": [0.2, 0.3, 0.4],
                                "metallic": 0.25,
                                "roughness": 0.7
                            }
                        }
                    }
                ]
            }],
            "assets": [{
                "id": "asset:triangle",
                "kind": "daedalus.mesh",
                "uri": "assets/triangle.json",
                "content_hash": hash,
                "metadata": {}
            }]
        }))
        .unwrap()
    }

    fn one_mesh_source(mesh_bytes: Vec<u8>) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([("assets/triangle.json".into(), mesh_bytes)])
    }

    fn error_text(result: Result<PreparedProject, ProjectError>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn empty_project_is_valid_without_fixture_injection() {
        let prepared = prepare_project_from_bytes(
            &manifest("scene.json"),
            &empty_scene(),
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .unwrap();
        assert!(prepared.entities().is_empty());
        assert!(prepared.meshes().is_empty());
        assert!(prepared.scene().entities.is_empty());
        assert!(prepared.digest().starts_with("sha256:"));
        assert_eq!(prepared.files().len(), 2);
        prepared.validate().unwrap();
    }

    #[test]
    fn byte_prepare_decodes_typed_components_mesh_and_declared_hash() {
        let mesh = triangle_mesh();
        let scene = mesh_scene(Some(format!("sha256:{}", sha256_hex(&mesh))));
        let assets = one_mesh_source(mesh);
        let prepared = prepare_project_from_bytes(
            &manifest("scene.json"),
            &scene,
            &assets,
            ProjectLimits::default(),
        )
        .unwrap();
        assert_eq!(prepared.entities().len(), 1);
        assert_eq!(prepared.entities()[0].id.as_str(), "entity:triangle");
        assert_eq!(
            prepared.entities()[0].transform.translation,
            [2.0, 3.0, 4.0]
        );
        assert_eq!(
            prepared.entities()[0].mesh.as_ref().unwrap().as_str(),
            "asset:triangle"
        );
        assert_eq!(
            prepared.entities()[0].material.as_ref().unwrap().roughness,
            0.7
        );
        assert_eq!(
            prepared.meshes().values().next().unwrap().indices,
            [0, 1, 2]
        );
        assert_eq!(prepared.files().len(), 3);
    }

    #[test]
    fn malformed_json_reports_source_and_line_column() {
        let error = prepare_project_from_bytes(
            b"{\n  invalid\n}",
            &empty_scene(),
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, ProjectError::Json { line: 2, .. }));
        assert!(error.to_string().contains(PROJECT_MANIFEST_FILE));
    }

    #[test]
    fn unsupported_parent_component_and_asset_kinds_fail_with_paths() {
        let parented = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [{
                "id": "entity:child",
                "parent": "entity:parent",
                "components": []
            }, {
                "id": "entity:parent",
                "components": []
            }],
            "assets": []
        }))
        .unwrap();
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &parented,
            &BTreeMap::new(),
            ProjectLimits::default(),
        ));
        assert!(error.contains("entities[0].parent"));
        assert!(error.contains("unsupported"));

        let unknown_component = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [{
                "id": "entity:one",
                "components": [{
                    "id": "component:unknown",
                    "type_id": "daedalus.script",
                    "asset_refs": [],
                    "data": {}
                }]
            }],
            "assets": []
        }))
        .unwrap();
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &unknown_component,
            &BTreeMap::new(),
            ProjectLimits::default(),
        ));
        assert!(error.contains("components[0].type_id"));
        assert!(error.contains("daedalus.script"));

        let texture = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [],
            "assets": [{
                "id": "asset:texture",
                "kind": "daedalus.texture",
                "uri": "assets/albedo.png",
                "metadata": {}
            }]
        }))
        .unwrap();
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &texture,
            &BTreeMap::new(),
            ProjectLimits::default(),
        ));
        assert!(error.contains("assets[0].kind"));
        assert!(error.contains("daedalus.texture"));
    }

    #[test]
    fn path_escape_and_uri_scheme_are_rejected_before_byte_lookup() {
        let error = error_text(prepare_project_from_bytes(
            &manifest("../outside.json"),
            &empty_scene(),
            &BTreeMap::new(),
            ProjectLimits::default(),
        ));
        assert!(error.contains("parent traversal"));

        let scene = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [],
            "assets": [{
                "id": "asset:mesh",
                "kind": "daedalus.mesh",
                "uri": "https://example.invalid/triangle.json",
                "metadata": {}
            }]
        }))
        .unwrap();
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &scene,
            &BTreeMap::new(),
            ProjectLimits::default(),
        ));
        assert!(error.contains("URI schemes"));
    }

    #[test]
    fn hash_mismatch_bad_geometry_and_budgets_are_rejected() {
        let mesh = triangle_mesh();
        let scene = mesh_scene(Some(format!("sha256:{}", "0".repeat(64))));
        let assets = one_mesh_source(mesh.clone());
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &scene,
            &assets,
            ProjectLimits::default(),
        ));
        assert!(error.contains("content_hash"));

        let bad_mesh = serde_json::to_vec(&json!({
            "schema_version": 1,
            "vertices": [{"position": [0,0,0], "normal": [0,0,1]}],
            "indices": [0, 1, 4]
        }))
        .unwrap();
        let scene = mesh_scene(None);
        let assets = one_mesh_source(bad_mesh);
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &scene,
            &assets,
            ProjectLimits::default(),
        ));
        assert!(error.contains("outside the vertex array"));

        let limits = ProjectLimits {
            max_manifest_bytes: 8,
            ..ProjectLimits::default()
        };
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &empty_scene(),
            &BTreeMap::new(),
            limits,
        ));
        assert!(error.contains("manifest"));
        assert!(error.contains("limit"));

        let mesh = triangle_mesh();
        let scene = mesh_scene(None);
        let limits = ProjectLimits {
            max_vertices: 2,
            ..ProjectLimits::default()
        };
        let assets = one_mesh_source(mesh);
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &scene,
            &assets,
            limits,
        ));
        assert!(error.contains("vertices"));

        let limits = ProjectLimits {
            max_total_bytes: 8,
            ..ProjectLimits::default()
        };
        let error = error_text(prepare_project_from_bytes(
            &manifest("scene.json"),
            &empty_scene(),
            &BTreeMap::new(),
            limits,
        ));
        assert!(error.contains("total budget"));
    }

    #[test]
    fn native_loader_reads_real_external_files_and_rejects_parent_escape() {
        let project = TempProject::new();
        let mesh = triangle_mesh();
        let scene = mesh_scene(None);
        project.write(PROJECT_MANIFEST_FILE, &manifest("scene.json"));
        project.write("scene.json", &scene);
        project.write("assets/triangle.json", &mesh);
        let prepared = load_project(project.path()).unwrap();
        assert_eq!(prepared.entities().len(), 1);
        assert_eq!(prepared.files().len(), 3);

        let outside = TempProject::new();
        outside.write("outside.json", &empty_scene());
        project.write(PROJECT_MANIFEST_FILE, &manifest("../outside.json"));
        let error = load_project(project.path()).unwrap_err();
        assert!(error.to_string().contains("parent traversal"));
    }

    #[test]
    fn byte_and_native_adapters_share_source_digest_semantics() {
        let project = TempProject::new();
        let mesh = triangle_mesh();
        let scene = mesh_scene(None);
        let manifest_bytes = manifest("scene.json");
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene);
        project.write("assets/triangle.json", &mesh);
        let assets = one_mesh_source(mesh);
        let from_bytes =
            prepare_project_from_bytes(&manifest_bytes, &scene, &assets, ProjectLimits::default())
                .unwrap();
        let from_filesystem = load_project(project.path()).unwrap();
        assert_eq!(from_bytes.digest(), from_filesystem.digest());

        let mut edited_scene = scene;
        edited_scene.push(b' ');
        project.write("scene.json", &edited_scene);
        let changed = load_project(project.path()).unwrap();
        assert_ne!(from_filesystem.digest(), changed.digest());
    }

    #[test]
    fn asset_aware_and_legacy_loaders_keep_legacy_mesh_source_identity_equal() {
        let manifest_bytes = manifest("scene.json");
        let mesh_bytes = triangle_mesh();
        let scene_bytes = mesh_scene(None);
        let mesh_sources = one_mesh_source(mesh_bytes.clone());
        let legacy_bytes = prepare_project_from_bytes(
            &manifest_bytes,
            &scene_bytes,
            &mesh_sources,
            ProjectLimits::default(),
        )
        .unwrap();

        let mut resolver = crate::assets::ByteMapSourceResolver::from_bytes(mesh_sources).unwrap();
        let mut byte_cache = crate::assets::MemoryCookCache::default();
        let aware_bytes = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut resolver,
            &mut byte_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        let project = TempProject::new();
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        project.write("assets/triangle.json", &mesh_bytes);
        let legacy_native = load_project(project.path()).unwrap();
        let mut native_cache = crate::assets::MemoryCookCache::default();
        let aware_native = load_project_with_assets(
            project.path(),
            &mut native_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        assert_eq!(legacy_bytes.digest(), aware_bytes.digest());
        assert_eq!(legacy_bytes.digest(), legacy_native.digest());
        assert_eq!(legacy_bytes.digest(), aware_native.digest());
        assert_eq!(legacy_bytes.files(), aware_bytes.files());
        assert!(aware_bytes.model_renderers().is_empty());
        aware_bytes.validate().unwrap();
        aware_native.validate().unwrap();
    }

    #[test]
    fn asset_aware_byte_and_native_entrypoints_share_catalog_identity_and_accounting() {
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = empty_scene();
        let mut resolver = crate::assets::ByteMapSourceResolver::default();
        let mut byte_cache = crate::assets::MemoryCookCache::default();
        let cancellation = ImportCancellation::never();
        let from_bytes = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut resolver,
            &mut byte_cache,
            ImportLimits::default(),
            &cancellation,
        )
        .unwrap();

        let project = TempProject::new();
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        let mut native_cache = crate::assets::MemoryCookCache::default();
        let from_native = load_project_with_assets(
            project.path(),
            &mut native_cache,
            ImportLimits::default(),
            &cancellation,
        )
        .unwrap();

        assert_eq!(from_bytes.digest(), from_native.digest());
        assert_eq!(
            from_bytes.asset_catalog().unwrap(),
            from_native.asset_catalog().unwrap()
        );
        let byte_metrics = from_bytes.asset_import_report().unwrap().metrics();
        let native_metrics = from_native.asset_import_report().unwrap().metrics();
        assert_eq!(byte_metrics.source_reads, 2);
        assert_eq!(native_metrics.source_reads, 2);
        assert_eq!(byte_metrics.source_bytes, native_metrics.source_bytes);
        let empty_report = from_bytes.asset_import_report().unwrap();
        let empty_memory = empty_report.memory();
        assert!(empty_report.assets().is_empty());
        assert_eq!(empty_memory.unique_source_count(), 2);
        assert_eq!(
            empty_memory.captured_source_bytes(),
            empty_report
                .sources()
                .iter()
                .map(|source| source.byte_len())
                .sum::<u64>()
        );
        assert_eq!(
            empty_memory.cooked_accounted_bytes(),
            empty_report.metrics().catalog_metadata_bytes
        );
        from_bytes.validate().unwrap();
        from_native.validate().unwrap();
    }

    #[test]
    fn asset_import_memory_summary_matches_cold_and_warm_project_sources() {
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = model_project_scene(true);
        let mut sources = minimal_model_sources();
        sources.insert("sprites/pixel.png".to_owned(), png_rgba([23, 47, 89, 255]));
        let cancellation = ImportCancellation::never();
        let mut cache = crate::assets::MemoryCookCache::default();

        let mut cold_resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sources.clone()).unwrap();
        let cold = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut cold_resolver,
            &mut cache,
            ImportLimits::default(),
            &cancellation,
        )
        .unwrap();
        let cold_report = cold.asset_import_report().unwrap();
        let cold_memory = cold_report.memory();
        let source_bytes = cold_report
            .sources()
            .iter()
            .try_fold(0u64, |total, source| total.checked_add(source.byte_len()))
            .unwrap();

        assert_eq!(cold_memory.schema_version(), 1);
        assert!(cold_memory.cooked_accounted_bytes() > 0);
        assert_eq!(cold_memory.decoded_base_image_bytes(), 4);
        assert!(cold_memory.parser_scratch_peak_bytes() > 0);
        assert_eq!(
            cold_memory.unique_source_count(),
            cold_report.sources().len()
        );
        assert_eq!(cold_memory.captured_source_bytes(), source_bytes);
        assert_eq!(cold_report.metrics().png_base_decodes, 1);

        let serialized = serde_json::to_value(cold_report).unwrap();
        assert_eq!(serialized["memory"]["schemaVersion"], 1);
        assert_eq!(
            serialized["memory"]["capturedSourceBytes"],
            cold_memory.captured_source_bytes()
        );
        assert!(serialized["memory"].get("parserScratchPeakBytes").is_some());

        let mut warm_resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sources.clone()).unwrap();
        let warm = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut warm_resolver,
            &mut cache,
            ImportLimits::default(),
            &cancellation,
        )
        .unwrap();
        let warm_report = warm.asset_import_report().unwrap();
        let warm_memory = warm_report.memory();
        let warm_source_bytes = warm_report
            .sources()
            .iter()
            .try_fold(0u64, |total, source| total.checked_add(source.byte_len()))
            .unwrap();

        assert_eq!(warm_report.metrics().png_base_decodes, 0);
        assert_eq!(
            warm_memory.cooked_accounted_bytes(),
            cold_memory.cooked_accounted_bytes()
        );
        assert_eq!(
            warm_memory.decoded_base_image_bytes(),
            cold_memory.decoded_base_image_bytes()
        );
        assert_eq!(
            warm_memory.unique_source_count(),
            warm_report.sources().len()
        );
        assert_eq!(warm_memory.captured_source_bytes(), warm_source_bytes);
        assert_eq!(
            warm_memory.captured_source_bytes(),
            cold_memory.captured_source_bytes()
        );
        assert!(warm_memory.parser_scratch_peak_bytes() > 0);

        let cooked_limit = cold_memory
            .cooked_accounted_bytes()
            .checked_sub(1)
            .expect("the accepted cold preparation must have a nonzero cooked ledger");
        let limits = ImportLimits {
            max_cooked_bytes: cooked_limit,
            ..ImportLimits::default()
        };
        let mut limited_resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sources).unwrap();
        let mut limited_cache = crate::assets::MemoryCookCache::default();
        let error = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut limited_resolver,
            &mut limited_cache,
            limits,
            &cancellation,
        )
        .unwrap_err();
        assert_eq!(error.code(), AssetImportErrorCode::LimitExceeded);
    }

    #[test]
    fn asset_aware_model_renderer_binding_is_validated_and_native_byte_equivalent() {
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = model_project_scene(false);
        let sources = minimal_model_sources();
        let mut resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sources.clone()).unwrap();
        let mut byte_cache = crate::assets::MemoryCookCache::default();
        let from_bytes = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut resolver,
            &mut byte_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        let project = TempProject::new();
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        for (source_id, bytes) in sources {
            project.write(&source_id, &bytes);
        }
        let mut native_cache = crate::assets::MemoryCookCache::default();
        let from_native = load_project_with_assets(
            project.path(),
            &mut native_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        assert_eq!(from_bytes.digest(), from_native.digest());
        assert_eq!(from_bytes.files(), from_native.files());
        assert_eq!(from_bytes.asset_catalog(), from_native.asset_catalog());
        assert_eq!(from_bytes.model_renderers(), from_native.model_renderers());
        assert_eq!(from_bytes.model_renderers().len(), 1);
        let binding = &from_bytes.model_renderers()[&StableId::new("entity:hero").unwrap()];
        assert_eq!(binding.component_id().as_str(), "component:model-renderer");
        assert_eq!(binding.model_asset_id().as_str(), "asset:model");
        assert!(from_bytes.entities()[0].mesh.is_none());
        from_bytes.validate().unwrap();
        from_native.validate().unwrap();

        let mut corrupted_projection = from_bytes.clone();
        corrupted_projection.model_renderers.clear();
        assert!(corrupted_projection.validate().is_err());
    }

    #[test]
    fn sprite_and_camera_bindings_are_prepared_validated_and_native_byte_equivalent() {
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = sprite_project_scene_bytes();
        let sources = sprite_project_sources();
        let mut resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sources.clone()).unwrap();
        let mut byte_cache = crate::assets::MemoryCookCache::default();
        let from_bytes = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut resolver,
            &mut byte_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        let project = TempProject::new();
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        for (source_id, bytes) in sources {
            project.write(&source_id, &bytes);
        }
        let mut native_cache = crate::assets::MemoryCookCache::default();
        let from_native = load_project_with_assets(
            project.path(),
            &mut native_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();

        assert_eq!(from_bytes.digest(), from_native.digest());
        assert_eq!(from_bytes.files(), from_native.files());
        assert_eq!(from_bytes.asset_catalog(), from_native.asset_catalog());
        assert_eq!(
            from_bytes.sprite_renderers(),
            from_native.sprite_renderers()
        );
        assert_eq!(from_bytes.camera_2d(), from_native.camera_2d());
        assert_eq!(from_bytes.sprite_renderers().len(), 1);
        let sprite = &from_bytes.sprite_renderers()[&StableId::new("entity:hero").unwrap()];
        assert_eq!(sprite.component_id().as_str(), "component:sprite");
        assert_eq!(sprite.atlas_asset_id().as_str(), "asset:atlas");
        assert_eq!(sprite.region_id().as_str(), "region:hero");
        assert_eq!(sprite.tint(), [0.25, 0.5, 1.0, 0.75]);
        assert_eq!(sprite.sorting_layer(), -2);
        assert_eq!(sprite.order_in_layer(), 8);
        let camera = from_bytes.camera_2d().unwrap();
        assert_eq!(camera.entity_id().as_str(), "entity:camera");
        assert_eq!(camera.component_id().as_str(), "component:camera2d");
        assert_eq!(camera.vertical_size(), 10.0);
        assert_eq!(camera.near(), 0.1);
        assert_eq!(camera.far(), 100.0);
        assert_eq!(from_bytes.entities().len(), 2);
        from_bytes.validate().unwrap();
        from_native.validate().unwrap();

        let mut conflict_manifest: ProjectManifest =
            serde_json::from_slice(&manifest_bytes).unwrap();
        conflict_manifest.presentation = Some(ProjectPresentation {
            camera: Some(ProjectCamera::default()),
            ..ProjectPresentation::default()
        });
        let conflict_manifest = serde_json::to_vec(&conflict_manifest).unwrap();
        let mut conflict_resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(sprite_project_sources()).unwrap();
        let mut conflict_cache = crate::assets::MemoryCookCache::default();
        let presentation_conflict = prepare_project_with_assets(
            &conflict_manifest,
            &scene_bytes,
            &mut conflict_resolver,
            &mut conflict_cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap_err()
        .to_string();
        assert!(presentation_conflict.contains("presentation.camera"));
        assert!(presentation_conflict.contains("cannot be combined"));

        let mut corrupted_sprite_map = from_bytes.clone();
        corrupted_sprite_map.sprite_renderers.clear();
        assert!(corrupted_sprite_map.validate().is_err());
        let mut corrupted_camera = from_bytes.clone();
        corrupted_camera.camera_2d = None;
        assert!(corrupted_camera.validate().is_err());
        assert_eq!(from_bytes.digest(), from_native.digest());
    }

    #[test]
    fn sprite_camera_components_reject_invalid_schema_references_and_combinations() {
        let assets = BTreeMap::from([(StableId::new("asset:atlas").unwrap(), dummy_atlas_asset())]);
        let valid_sprite = || {
            sprite_renderer_component(
                "component:sprite",
                json!(["asset:atlas"]),
                json!({"atlas": "asset:atlas", "region": "region:hero"}),
            )
        };
        let decode_error =
            |components: Vec<Value>, asset_map: &BTreeMap<StableId, ImportedAsset>| {
                decode_entities_with_assets(
                    "scene.json",
                    &scene_with_sprite_and_camera(components),
                    &BTreeMap::new(),
                    asset_map,
                )
                .unwrap_err()
                .to_string()
            };

        let valid_projection = decode_entities_with_assets(
            "scene.json",
            &scene_with_sprite_and_camera(vec![valid_sprite()]),
            &BTreeMap::new(),
            &assets,
        )
        .unwrap();
        assert_eq!(valid_projection.sprite_renderers.len(), 1);
        let defaulted = &valid_projection.sprite_renderers[&StableId::new("entity:hero").unwrap()];
        assert_eq!(defaulted.tint(), [1.0; 4]);
        assert_eq!(defaulted.sorting_layer(), 0);
        assert_eq!(defaulted.order_in_layer(), 0);

        let missing_camera = decode_entities_with_assets(
            "scene.json",
            &scene_with_components(vec![valid_sprite()]),
            &BTreeMap::new(),
            &assets,
        )
        .unwrap_err()
        .to_string();
        assert!(missing_camera.contains("requires exactly one daedalus.camera_2d"));

        let invalid_cases = [
            (
                sprite_renderer_component(
                    "component:sprite",
                    json!([]),
                    json!({"atlas": "asset:atlas", "region": "region:hero"}),
                ),
                "asset_refs",
            ),
            (
                sprite_renderer_component(
                    "component:sprite",
                    json!(["asset:atlas"]),
                    json!({"atlas": "asset:atlas", "region": "region:missing"}),
                ),
                "data.region",
            ),
            (
                sprite_renderer_component(
                    "component:sprite",
                    json!(["asset:atlas"]),
                    json!({"atlas": "asset:atlas", "region": "region:hero", "tint": [1.0, 0.0, 0.0, 1.5]}),
                ),
                "data.tint",
            ),
            (
                sprite_renderer_component(
                    "component:sprite",
                    json!(["asset:atlas"]),
                    json!({"atlas": "asset:atlas", "region": "region:hero", "unknown": true}),
                ),
                "data",
            ),
        ];
        for (component, expected_path) in invalid_cases {
            assert!(decode_error(vec![component], &assets).contains(expected_path));
        }

        let duplicate_error = decode_error(vec![valid_sprite(), valid_sprite()], &assets);
        assert!(duplicate_error.contains("duplicate 'daedalus.sprite_renderer'"));
        let conflict_error = decode_error(
            vec![
                valid_sprite(),
                json!({
                    "id": "component:mesh",
                    "type_id": "daedalus.mesh_renderer",
                    "asset_refs": ["asset:mesh"],
                    "data": {"mesh": "asset:mesh"}
                }),
            ],
            &assets,
        );
        assert!(conflict_error.contains("cannot combine mesh, model, or sprite"));

        let mut model_and_atlas_assets = assets.clone();
        model_and_atlas_assets.insert(StableId::new("asset:model").unwrap(), dummy_model_asset());
        let model_conflict = decode_entities_with_assets(
            "scene.json",
            &scene_with_sprite_and_camera(vec![
                valid_sprite(),
                model_renderer_component(
                    "component:model",
                    json!(["asset:model"]),
                    json!({"model": "asset:model"}),
                ),
            ]),
            &BTreeMap::new(),
            &model_and_atlas_assets,
        )
        .unwrap_err()
        .to_string();
        assert!(model_conflict.contains("cannot combine mesh, model, or sprite"));

        let wrong_kind = crate::assets::AudioAsset::new(
            48_000,
            1,
            vec![0.0],
            &ImportLimits::default(),
            crate::assets::ImportLocation::default(),
        )
        .unwrap();
        let wrong_kind_assets = BTreeMap::from([(
            StableId::new("asset:atlas").unwrap(),
            ImportedAsset::Audio(Arc::new(wrong_kind)),
        )]);
        assert!(
            decode_error(vec![valid_sprite()], &wrong_kind_assets).contains("not a sprite atlas")
        );

        let legacy_error = decode_entities(
            "scene.json",
            &scene_with_components(vec![valid_sprite()]),
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(legacy_error.contains("requires the asset-aware project loader"));
    }

    #[test]
    fn camera2d_legacy_support_and_project_wide_constraints_are_revalidated() {
        let camera_component = camera_2d_component(
            "component:camera2d",
            json!([]),
            json!({"vertical_size": 10.0, "near": 0.1, "far": 100.0}),
        );
        let camera_only_scene = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [{
                "id": "entity:camera",
                "components": [camera_component.clone()]
            }],
            "assets": []
        }))
        .unwrap();
        let legacy = prepare_project_from_bytes(
            &manifest("scene.json"),
            &camera_only_scene,
            &BTreeMap::new(),
            ProjectLimits::default(),
        )
        .unwrap();
        assert_eq!(
            legacy.camera_2d().unwrap().entity_id().as_str(),
            "entity:camera"
        );
        legacy.validate().unwrap();

        let invalid_scale = decode_entities(
            "scene.json",
            &scene_with_components(vec![
                transform_component("component:transform", [1.0, 1.0, 1.01]),
                camera_component.clone(),
            ]),
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(invalid_scale.contains("scale must be unit"));

        for invalid_data in [
            json!({"vertical_size": 0.0, "near": 0.1, "far": 100.0}),
            json!({"vertical_size": 10.0, "near": 0.1, "far": 0.1}),
            json!({"vertical_size": 10.0, "near": 0.1}),
            json!({"vertical_size": 10.0, "near": 0.1, "far": 100.0, "extra": 1}),
        ] {
            let error = decode_entities(
                "scene.json",
                &scene_with_components(vec![camera_2d_component(
                    "component:camera2d",
                    json!([]),
                    invalid_data,
                )]),
                &BTreeMap::new(),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("components[0].data"));
        }

        let duplicate_cameras: SceneDocument = serde_json::from_value(json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [
                {"id": "entity:camera-a", "components": [camera_component.clone()]},
                {"id": "entity:camera-b", "components": [camera_2d_component(
                    "component:camera2d-b", json!([]),
                    json!({"vertical_size": 10.0, "near": 0.1, "far": 100.0})
                )]}
            ],
            "assets": []
        }))
        .unwrap();
        let duplicate_error = decode_entities("scene.json", &duplicate_cameras, &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(duplicate_error.contains("at most one daedalus.camera_2d"));

        let mesh_component = json!({
            "id": "component:mesh",
            "type_id": "daedalus.mesh_renderer",
            "asset_refs": ["asset:mesh"],
            "data": {"mesh": "asset:mesh"}
        });
        let mesh_assets = BTreeMap::from([(
            StableId::new("asset:mesh").unwrap(),
            serde_json::from_slice::<ProjectMesh>(&triangle_mesh()).unwrap(),
        )]);
        let render_camera_error = decode_entities(
            "scene.json",
            &scene_with_components(vec![camera_component.clone(), mesh_component]),
            &mesh_assets,
        )
        .unwrap_err()
        .to_string();
        assert!(render_camera_error.contains("camera_2d with a renderer component"));

        let presentation: ProjectManifest = serde_json::from_value(json!({
            "schema_version": 1,
            "name": "external-test",
            "scene": "scene.json",
            "presentation": {"camera": ProjectCamera::default()}
        }))
        .unwrap();
        let single_camera_projection = decode_entities(
            "scene.json",
            &serde_json::from_slice(&camera_only_scene).unwrap(),
            &BTreeMap::new(),
        )
        .unwrap();
        let conflict = validate_camera_presentation_compatibility(
            "scene.json",
            &presentation,
            single_camera_projection.camera_2d.as_ref(),
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("cannot be combined"));
    }

    #[test]
    fn model_renderer_component_reports_schema_reference_and_conflict_errors() {
        let model_id = StableId::new("asset:model").unwrap();
        let model_assets = BTreeMap::from([(model_id.clone(), dummy_model_asset())]);
        let valid = || {
            model_renderer_component(
                "component:model-renderer",
                json!(["asset:model"]),
                json!({"model": "asset:model"}),
            )
        };
        let decode_error = |components: Vec<Value>, assets: &BTreeMap<StableId, ImportedAsset>| {
            decode_entities_with_assets(
                "scene.json",
                &scene_with_components(components),
                &BTreeMap::new(),
                assets,
            )
            .unwrap_err()
            .to_string()
        };

        let error = decode_error(
            vec![model_renderer_component(
                "component:model-renderer",
                json!(["asset:model"]),
                json!({"model": "asset:model", "material": {}}),
            )],
            &model_assets,
        );
        assert!(error.contains("components[0].data"));

        let error = decode_error(
            vec![model_renderer_component(
                "component:model-renderer",
                json!([]),
                json!({"model": "asset:model"}),
            )],
            &model_assets,
        );
        assert!(error.contains("components[0].asset_refs"));

        let error = decode_error(
            vec![model_renderer_component(
                "component:model-renderer",
                json!(["asset:other"]),
                json!({"model": "asset:model"}),
            )],
            &model_assets,
        );
        assert!(error.contains("components[0].asset_refs"));

        let error = decode_error(
            vec![model_renderer_component(
                "component:model-renderer",
                json!(["asset:model"]),
                json!({}),
            )],
            &model_assets,
        );
        assert!(error.contains("components[0].data"));

        let error = decode_error(
            vec![model_renderer_component(
                "component:model-renderer",
                json!(["asset:missing"]),
                json!({"model": "asset:missing"}),
            )],
            &model_assets,
        );
        assert!(error.contains("components[0].data.model"));

        let wrong_kind = crate::assets::AudioAsset::new(
            48_000,
            1,
            vec![0.0],
            &ImportLimits::default(),
            crate::assets::ImportLocation::default(),
        )
        .unwrap();
        let wrong_kind_assets =
            BTreeMap::from([(model_id.clone(), ImportedAsset::Audio(Arc::new(wrong_kind)))]);
        let error = decode_error(vec![valid()], &wrong_kind_assets);
        assert!(error.contains("components[0].data.model"));
        assert!(error.contains("Audio"));

        let duplicate = vec![
            valid(),
            model_renderer_component(
                "component:model-renderer-duplicate",
                json!(["asset:model"]),
                json!({"model": "asset:model"}),
            ),
        ];
        let error = decode_error(duplicate, &model_assets);
        assert!(error.contains("components[1].type_id"));
        assert!(error.contains("duplicate 'daedalus.model_renderer'"));

        let mesh_component = json!({
            "id": "component:mesh-renderer",
            "type_id": "daedalus.mesh_renderer",
            "asset_refs": ["asset:mesh"],
            "data": {"mesh": "asset:mesh"}
        });
        let meshes = BTreeMap::from([(
            StableId::new("asset:mesh").unwrap(),
            serde_json::from_slice::<ProjectMesh>(&triangle_mesh()).unwrap(),
        )]);
        let conflict_scene = scene_with_components(vec![valid(), mesh_component]);
        let error =
            decode_entities_with_assets("scene.json", &conflict_scene, &meshes, &model_assets)
                .unwrap_err()
                .to_string();
        assert!(error.contains("components[1].type_id"));
        assert!(error.contains("cannot combine"));

        let legacy_error = decode_entities(
            "scene.json",
            &scene_with_components(vec![valid()]),
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(legacy_error.contains("unsupported"));
        assert!(legacy_error.contains("requires the asset-aware project loader"));
    }

    #[test]
    fn detaching_asset_pins_preserves_owned_resources_and_waits_for_pinned_clones() {
        let project = TempProject::new();
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = model_project_scene(true);
        let mut sources = minimal_model_sources();
        sources.insert("sprites/pixel.png".into(), png_rgba([23, 47, 89, 255]));
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        for (source_id, bytes) in sources {
            project.write(&source_id, &bytes);
        }
        let cache_root = project.path().join("cache");
        let mut cache =
            crate::assets::FileCookCache::open(&cache_root, crate::assets::CacheLimits::default())
                .unwrap();
        let prepared = load_project_with_assets(
            project.path(),
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap();
        let pinned_clone = prepared.clone();
        let digest = prepared.digest().to_owned();
        let catalog_digest = prepared.asset_catalog().unwrap().digest().to_owned();
        drop(cache);
        assert!(
            crate::assets::FileCookCache::open(&cache_root, crate::assets::CacheLimits::default())
                .is_err()
        );

        let detached = prepared.release_asset_cache_pins();
        assert_eq!(detached.digest(), digest);
        assert_eq!(detached.asset_catalog().unwrap().digest(), catalog_digest);
        assert!(
            crate::assets::FileCookCache::open(&cache_root, crate::assets::CacheLimits::default())
                .is_err()
        );
        drop(pinned_clone);

        let reopened =
            crate::assets::FileCookCache::open(&cache_root, crate::assets::CacheLimits::default())
                .expect("detached project should no longer retain the cache owner lock");
        match detached
            .asset_catalog()
            .unwrap()
            .asset(&StableId::new("asset:model").unwrap())
            .unwrap()
        {
            ImportedAsset::Model(model) => {
                assert_eq!(
                    model.primitives()[0].geometry().positions()[1],
                    [1.0, 0.0, 0.0]
                );
            }
            _ => panic!("model asset should remain owned after pin release"),
        }
        match detached
            .asset_catalog()
            .unwrap()
            .asset(&StableId::new("asset:image").unwrap())
            .unwrap()
        {
            ImportedAsset::Image(image) => {
                assert_eq!(image.decoded().rgba8(), &[23, 47, 89, 255]);
            }
            _ => panic!("image asset should remain owned after pin release"),
        }
        assert!(detached.validate().is_ok());
        assert_eq!(reopened.stats().pinned_entries, 0);
    }

    #[test]
    fn asset_aware_byte_and_native_adapters_enforce_the_same_lower_source_cap() {
        let manifest_bytes = manifest("scene.json");
        let scene_bytes = serde_json::to_vec(&json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [],
            "assets": [{
                "id": "asset:triangle",
                "kind": "daedalus.mesh",
                "uri": "assets/triangle.json",
                "metadata": {}
            }]
        }))
        .unwrap();
        let mut oversized_mesh = triangle_mesh();
        oversized_mesh.resize(512, b' ');
        let limits = ImportLimits {
            max_source_bytes: 256,
            ..ImportLimits::default()
        };
        let cancellation = ImportCancellation::never();

        let mut byte_resolver =
            crate::assets::ByteMapSourceResolver::from_bytes(BTreeMap::from([(
                "assets/triangle.json".to_owned(),
                oversized_mesh.clone(),
            )]))
            .unwrap();
        let mut byte_cache = crate::assets::MemoryCookCache::default();
        let byte_error = prepare_project_with_assets(
            &manifest_bytes,
            &scene_bytes,
            &mut byte_resolver,
            &mut byte_cache,
            limits,
            &cancellation,
        )
        .unwrap_err();

        let project = TempProject::new();
        project.write(PROJECT_MANIFEST_FILE, &manifest_bytes);
        project.write("scene.json", &scene_bytes);
        project.write("assets/triangle.json", &oversized_mesh);
        let mut native_cache = crate::assets::MemoryCookCache::default();
        let native_error =
            load_project_with_assets(project.path(), &mut native_cache, limits, &cancellation)
                .unwrap_err();

        assert_eq!(byte_error.code(), native_error.code());
        assert_eq!(byte_error.code().as_str(), "limit_exceeded");
        assert_eq!(byte_error.source_id(), native_error.source_id());
        assert_eq!(byte_error.source_id(), Some("assets/triangle.json"));
    }

    #[cfg(unix)]
    #[test]
    fn native_loader_rejects_symlinks_that_escape_project_root() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        let outside = TempProject::new();
        outside.write("secret.json", &empty_scene());
        symlink(
            outside.path().join("secret.json"),
            project.path().join("scene.json"),
        )
        .unwrap();
        project.write(PROJECT_MANIFEST_FILE, &manifest("scene.json"));
        let error = load_project(project.path()).unwrap_err();
        assert!(error.to_string().contains("outside the project root"));
    }
}
