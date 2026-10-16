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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedProject {
    manifest: ProjectManifest,
    scene: SceneDocument,
    entities: Vec<PreparedEntity>,
    meshes: BTreeMap<StableId, ProjectMesh>,
    files: Vec<ProjectFileIdentity>,
    /// SHA-256 project identity over sorted relative source IDs and file hashes.
    digest: String,
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

    /// Revalidates every public prepared value before runtime construction.
    pub fn validate(&self) -> Result<(), ProjectError> {
        validate_prepared_project(self, ProjectLimits::default())
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

    let entities = decode_entities(&scene_source, &scene, &meshes)?;
    files.sort_by(|left, right| left.source_id.cmp(&right.source_id));
    let digest = project_digest(&files);
    let prepared = PreparedProject {
        manifest,
        scene,
        entities,
        meshes,
        files,
        digest,
    };
    validate_prepared_project(&prepared, limits)?;
    Ok(prepared)
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

fn decode_entities(
    source_id: &str,
    scene: &SceneDocument,
    meshes: &BTreeMap<StableId, ProjectMesh>,
) -> Result<Vec<PreparedEntity>, ProjectError> {
    let mut prepared = Vec::with_capacity(scene.entities.len());
    for (entity_index, entity) in scene.entities.iter().enumerate() {
        let entity_path = format!("entities[{entity_index}]");
        if entity.parent.is_some() {
            return Err(unsupported(
                source_id,
                format!("{entity_path}.parent"),
                "parented entities and hierarchy are not supported by this project loader",
            ));
        }
        prepared.push(decode_entity(source_id, entity_index, entity, meshes)?);
    }
    Ok(prepared)
}

fn decode_entity(
    source_id: &str,
    entity_index: usize,
    entity: &SceneEntity,
    meshes: &BTreeMap<StableId, ProjectMesh>,
) -> Result<PreparedEntity, ProjectError> {
    let entity_path = format!("entities[{entity_index}]");
    let mut transform = ProjectTransform::default();
    let mut mesh = None;
    let mut material = None;
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
            _ => {
                return Err(unsupported(
                    source_id,
                    format!("{path}.type_id"),
                    format!("component type '{type_id}' is not supported"),
                ));
            }
        }
    }
    Ok(PreparedEntity {
        id: entity.id.clone(),
        name: entity.name.clone(),
        transform,
        mesh,
        material,
    })
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
    let expected_entities = decode_entities(&scene_source, &project.scene, &project.meshes)?;
    if expected_entities != project.entities {
        return Err(invalid(
            &scene_source,
            "entities",
            "prepared entity projection does not match the scene's supported components",
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
    let Some(declared) = &asset.content_hash else {
        return Ok(());
    };
    let expected = parse_declared_hash(declared)
        .map_err(|message| invalid(source_id, format!("{path}.content_hash"), message))?;
    let actual = sha256_hex(bytes);
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
    use serde_json::json;
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
