//! Immutable, renderer-independent imported asset data and diagnostics.

use std::fmt;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::project::ProjectError;
use crate::scene::StableId;

/// Independent hard limits for transitive source import and CPU cooked resources.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    pub max_source_bytes: u64,
    pub max_total_source_bytes: u64,
    pub max_assets: usize,
    pub max_unique_sources: usize,
    pub max_source_id_bytes: usize,
    pub max_dependency_depth: usize,
    pub max_geometry_vertices: usize,
    pub max_geometry_indices: usize,
    pub max_model_nodes: usize,
    pub max_model_primitives: usize,
    pub max_model_materials: usize,
    pub max_decoded_images: usize,
    pub max_image_axis: u32,
    pub max_decoded_image_bytes: u64,
    pub max_atlas_regions: usize,
    pub max_audio_frames: u64,
    pub max_cooked_bytes: u64,
    pub max_parser_scratch_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 32 * 1024 * 1024,
            max_total_source_bytes: 64 * 1024 * 1024,
            max_assets: 256,
            max_unique_sources: 1_026,
            max_source_id_bytes: 4_096,
            max_dependency_depth: 256,
            max_geometry_vertices: 1_000_000,
            max_geometry_indices: 3_000_000,
            max_model_nodes: 16_384,
            max_model_primitives: 4_096,
            max_model_materials: 1_024,
            max_decoded_images: 256,
            max_image_axis: 4_096,
            max_decoded_image_bytes: 64 * 1024 * 1024,
            max_atlas_regions: 16_384,
            max_audio_frames: 10_000_000,
            max_cooked_bytes: 128 * 1024 * 1024,
            max_parser_scratch_bytes: 128 * 1024 * 1024 + 64 * 1024,
        }
    }
}

impl ImportLimits {
    pub fn validate(self) -> Result<(), AssetImportError> {
        let hard = Self::default();
        let positive = [
            self.max_source_bytes > 0,
            self.max_total_source_bytes > 0,
            self.max_assets > 0,
            self.max_unique_sources > 0,
            self.max_source_id_bytes > 0,
            self.max_dependency_depth > 0,
            self.max_geometry_vertices > 0,
            self.max_geometry_indices > 0,
            self.max_model_nodes > 0,
            self.max_model_primitives > 0,
            self.max_model_materials > 0,
            self.max_decoded_images > 0,
            self.max_image_axis > 0,
            self.max_decoded_image_bytes > 0,
            self.max_atlas_regions > 0,
            self.max_audio_frames > 0,
            self.max_cooked_bytes > 0,
            self.max_parser_scratch_bytes > 0,
        ];
        let under_hard = [
            self.max_source_bytes <= hard.max_source_bytes,
            self.max_total_source_bytes <= hard.max_total_source_bytes,
            self.max_assets <= hard.max_assets,
            self.max_unique_sources <= hard.max_unique_sources,
            self.max_source_id_bytes <= hard.max_source_id_bytes,
            self.max_dependency_depth <= hard.max_dependency_depth,
            self.max_geometry_vertices <= hard.max_geometry_vertices,
            self.max_geometry_indices <= hard.max_geometry_indices,
            self.max_model_nodes <= hard.max_model_nodes,
            self.max_model_primitives <= hard.max_model_primitives,
            self.max_model_materials <= hard.max_model_materials,
            self.max_decoded_images <= hard.max_decoded_images,
            self.max_image_axis <= hard.max_image_axis,
            self.max_decoded_image_bytes <= hard.max_decoded_image_bytes,
            self.max_atlas_regions <= hard.max_atlas_regions,
            self.max_audio_frames <= hard.max_audio_frames,
            self.max_cooked_bytes <= hard.max_cooked_bytes,
            self.max_parser_scratch_bytes <= hard.max_parser_scratch_bytes,
        ];
        if positive.into_iter().any(|valid| !valid)
            || under_hard.into_iter().any(|valid| !valid)
            || self.max_source_bytes > self.max_total_source_bytes
            || self.max_parser_scratch_bytes < self.max_source_bytes
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::InvalidLimits,
                ImportLocation::default(),
                "asset import limits must be positive, internally consistent, and at or below hard ceilings",
            ));
        }
        Ok(())
    }
}

/// Caller-driven cancellation that can adapt task systems without owning one.
#[derive(Clone)]
pub struct ImportCancellation(Arc<dyn Fn() -> bool + Send + Sync>);

impl fmt::Debug for ImportCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportCancellation").finish_non_exhaustive()
    }
}

impl Default for ImportCancellation {
    fn default() -> Self {
        Self::never()
    }
}

impl ImportCancellation {
    pub fn never() -> Self {
        Self(Arc::new(|| false))
    }

    pub fn from_predicate(predicate: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(predicate))
    }

    pub fn is_cancelled(&self) -> bool {
        (self.0)()
    }
}

/// A location carried through resolver, parser, decoder, and cache diagnostics.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ImportLocation {
    source_id: Option<String>,
    path: Option<String>,
    dependency_chain: Vec<String>,
}

impl ImportLocation {
    pub fn new(source_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            source_id: Some(source_id.into()),
            path: Some(path.into()),
            dependency_chain: Vec::new(),
        }
    }

    pub fn with_dependency_chain(mut self, chain: impl IntoIterator<Item = String>) -> Self {
        self.dependency_chain = chain.into_iter().collect();
        self
    }

    pub fn source_id(&self) -> Option<&str> {
        self.source_id.as_deref()
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn dependency_chain(&self) -> &[String] {
        &self.dependency_chain
    }
}

/// Stable machine-readable import failure class.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetImportErrorCode {
    InvalidLimits,
    InvalidPath,
    Io,
    Json,
    Invalid,
    Unsupported,
    LimitExceeded,
    Cancelled,
    Cache,
    CacheConflict,
    Decode,
    Internal,
}

impl AssetImportErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidLimits => "invalid_limits",
            Self::InvalidPath => "invalid_path",
            Self::Io => "io",
            Self::Json => "json",
            Self::Invalid => "invalid",
            Self::Unsupported => "unsupported",
            Self::LimitExceeded => "limit_exceeded",
            Self::Cancelled => "cancelled",
            Self::Cache => "cache",
            Self::CacheConflict => "cache_conflict",
            Self::Decode => "decode",
            Self::Internal => "internal",
        }
    }
}

/// Structured diagnostic. Legacy project errors remain inspectable and are not stringified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetImportError {
    code: AssetImportErrorCode,
    location: ImportLocation,
    message: String,
    project_error: Option<Box<ProjectError>>,
}

impl AssetImportError {
    pub fn new(
        code: AssetImportErrorCode,
        location: ImportLocation,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            location,
            message: message.into(),
            project_error: None,
        }
    }

    pub fn code(&self) -> AssetImportErrorCode {
        self.code
    }

    pub fn source_id(&self) -> Option<&str> {
        self.location.source_id()
    }

    pub fn path(&self) -> Option<&str> {
        self.location.path()
    }

    pub fn dependency_chain(&self) -> &[String] {
        self.location.dependency_chain()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn project_error(&self) -> Option<&ProjectError> {
        self.project_error.as_deref()
    }

    pub(crate) fn cancelled(location: ImportLocation) -> Self {
        Self::new(
            AssetImportErrorCode::Cancelled,
            location,
            "asset import was cancelled",
        )
    }
}

impl From<ProjectError> for AssetImportError {
    fn from(error: ProjectError) -> Self {
        let (code, source, path) = match &error {
            ProjectError::InvalidLimits { .. } => (AssetImportErrorCode::InvalidLimits, None, None),
            ProjectError::InvalidPath { source_id, .. } => (
                AssetImportErrorCode::InvalidPath,
                Some(source_id.clone()),
                None,
            ),
            ProjectError::Io { source_id, .. } => {
                (AssetImportErrorCode::Io, Some(source_id.clone()), None)
            }
            ProjectError::Json { source_id, .. } => {
                (AssetImportErrorCode::Json, Some(source_id.clone()), None)
            }
            ProjectError::Invalid {
                source_id, path, ..
            } => (
                AssetImportErrorCode::Invalid,
                Some(source_id.clone()),
                Some(path.clone()),
            ),
            ProjectError::Unsupported {
                source_id, path, ..
            } => (
                AssetImportErrorCode::Unsupported,
                Some(source_id.clone()),
                Some(path.clone()),
            ),
            ProjectError::LimitExceeded {
                source_id, path, ..
            } => (
                AssetImportErrorCode::LimitExceeded,
                Some(source_id.clone()),
                Some(path.clone()),
            ),
        };
        Self {
            code,
            location: ImportLocation {
                source_id: source,
                path,
                dependency_chain: Vec::new(),
            },
            message: error.to_string(),
            project_error: Some(Box::new(error)),
        }
    }
}

impl fmt::Display for AssetImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source_id) = self.source_id() {
            write!(f, "{source_id}")?;
            if let Some(path) = self.path() {
                write!(f, ":{path}")?;
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AssetImportError {}

/// Namespace is part of a cache key and maps only to engine-owned subdirectories.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CacheNamespace {
    Import,
    Cook,
}

/// Safe cache identity; callers cannot provide an arbitrary path or filename.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CacheKey {
    namespace: CacheNamespace,
    digest: [u8; 32],
}

impl CacheKey {
    pub fn new(namespace: CacheNamespace, digest: [u8; 32]) -> Self {
        Self { namespace, digest }
    }

    pub fn namespace(self) -> CacheNamespace {
        self.namespace
    }

    pub fn digest(self) -> [u8; 32] {
        self.digest
    }

    pub fn hex(self) -> String {
        encode_hex(&self.digest)
    }
}

/// Leaf cooker identity, distinct from the whole-container import identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LeafCookKey([u8; 32]);

impl LeafCookKey {
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub fn digest(self) -> [u8; 32] {
        self.0
    }

    pub fn hex(self) -> String {
        encode_hex(&self.0)
    }
}

pub(crate) fn domain_hash(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Data retained for one unique local-space primitive geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct GeometryResource {
    positions: Arc<[[f32; 3]]>,
    normals: Arc<[[f32; 3]]>,
    uv0: Option<Arc<[[f32; 2]]>>,
    indices: Arc<[u32]>,
    key: LeafCookKey,
}

impl GeometryResource {
    pub fn new(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        uv0: Option<Vec<[f32; 2]>>,
        indices: Vec<u32>,
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        validate_geometry(
            &positions,
            &normals,
            uv0.as_deref(),
            &indices,
            limits,
            &location,
        )?;
        let key = geometry_key(&positions, &normals, uv0.as_deref(), &indices);
        Ok(Self {
            positions: positions.into(),
            normals: normals.into(),
            uv0: uv0.map(Arc::from),
            indices: indices.into(),
            key,
        })
    }

    pub(crate) fn imported(
        positions: Vec<[f32; 3]>,
        normals: Vec<[f32; 3]>,
        uv0: Option<Vec<[f32; 2]>>,
        indices: Vec<u32>,
        key: LeafCookKey,
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        validate_geometry(
            &positions,
            &normals,
            uv0.as_deref(),
            &indices,
            limits,
            &location,
        )?;
        Ok(Self {
            positions: positions.into(),
            normals: normals.into(),
            uv0: uv0.map(Arc::from),
            indices: indices.into(),
            key,
        })
    }

    pub fn positions(&self) -> &[[f32; 3]] {
        &self.positions
    }
    pub fn normals(&self) -> &[[f32; 3]] {
        &self.normals
    }
    pub fn uv0(&self) -> Option<&[[f32; 2]]> {
        self.uv0.as_deref()
    }
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }
    pub fn key(&self) -> LeafCookKey {
        self.key
    }
    pub fn vertex_count(&self) -> usize {
        self.positions.len()
    }
    pub fn index_count(&self) -> usize {
        self.indices.len()
    }
}

fn validate_geometry(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uv0: Option<&[[f32; 2]]>,
    indices: &[u32],
    limits: &ImportLimits,
    location: &ImportLocation,
) -> Result<(), AssetImportError> {
    if positions.is_empty()
        || positions.len() != normals.len()
        || uv0.is_some_and(|uv| uv.len() != positions.len())
        || indices.is_empty()
        || !indices.len().is_multiple_of(3)
        || positions.len() > limits.max_geometry_vertices
        || indices.len() > limits.max_geometry_indices
        || indices
            .iter()
            .any(|index| *index as usize >= positions.len())
        || positions
            .iter()
            .flatten()
            .chain(normals.iter().flatten())
            .any(|value| !value.is_finite())
        || uv0.is_some_and(|uv| uv.iter().flatten().any(|value| !value.is_finite()))
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Invalid,
            location.clone(),
            "geometry violates finite, triangle, index, or configured count constraints",
        ));
    }
    Ok(())
}

fn geometry_key(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uv0: Option<&[[f32; 2]]>,
    indices: &[u32],
) -> LeafCookKey {
    let mut hasher = Sha256::new();
    let domain = b"daedalus.geometry.layout-v1";
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update((positions.len() as u64).to_le_bytes());
    hasher.update((positions.len() as u64 * 3).to_le_bytes());
    hasher.update((normals.len() as u64 * 3).to_le_bytes());
    hasher.update((uv0.map_or(0, |uv| uv.len()) as u64 * 2).to_le_bytes());
    hasher.update((indices.len() as u64).to_le_bytes());
    hasher.update([u8::from(uv0.is_some())]);
    for value in positions.iter().flatten().chain(normals.iter().flatten()) {
        hasher.update(value.to_bits().to_le_bytes());
    }
    if let Some(uvs) = uv0 {
        for value in uvs.iter().flatten() {
            hasher.update(value.to_bits().to_le_bytes());
        }
    }
    for index in indices {
        hasher.update(index.to_le_bytes());
    }
    LeafCookKey(hasher.finalize().into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageRole {
    BaseColorSrgb,
    SpriteAtlasSrgb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MipPolicy {
    None,
    LinearLightBoxV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureFilter {
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextureWrap {
    ClampToEdge,
    MirroredRepeat,
    Repeat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MipmapFilter {
    None,
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageColorPolicy {
    SrgbOrUnprofiled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageSampler {
    pub min_filter: TextureFilter,
    pub mag_filter: TextureFilter,
    pub mipmap_filter: MipmapFilter,
    pub wrap_u: TextureWrap,
    pub wrap_v: TextureWrap,
}

impl Default for ImageSampler {
    fn default() -> Self {
        Self {
            min_filter: TextureFilter::Linear,
            mag_filter: TextureFilter::Linear,
            mipmap_filter: MipmapFilter::None,
            wrap_u: TextureWrap::Repeat,
            wrap_v: TextureWrap::Repeat,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DecodedImage {
    width: u32,
    height: u32,
    storage: Arc<[u8]>,
    rgba_start: usize,
    rgba_len: usize,
    encoded_key: [u8; 32],
}

impl PartialEq for DecodedImage {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.rgba8() == other.rgba8()
            && self.encoded_key == other.encoded_key
    }
}

impl DecodedImage {
    pub fn new_rgba8(
        width: u32,
        height: u32,
        rgba8: Vec<u8>,
        encoded_key: [u8; 32],
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(4));
        if width == 0
            || height == 0
            || width > limits.max_image_axis
            || height > limits.max_image_axis
            || expected != Some(rgba8.len() as u64)
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location,
                "decoded RGBA8 image dimensions or byte length are invalid",
            ));
        }
        let rgba_len = rgba8.len();
        Ok(Self {
            width,
            height,
            storage: rgba8.into(),
            rgba_start: 0,
            rgba_len,
            encoded_key,
        })
    }
    #[allow(clippy::too_many_arguments)] // These inputs form the validated shared-storage view.
    pub(crate) fn from_rgba8_storage(
        width: u32,
        height: u32,
        storage: Arc<[u8]>,
        rgba_start: usize,
        rgba_len: usize,
        encoded_key: [u8; 32],
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(4));
        let end = rgba_start.checked_add(rgba_len);
        if width == 0
            || height == 0
            || width > limits.max_image_axis
            || height > limits.max_image_axis
            || expected != Some(rgba_len as u64)
            || end.is_none_or(|end| end > storage.len())
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location,
                "shared decoded RGBA8 image dimensions or byte range are invalid",
            ));
        }
        Ok(Self {
            width,
            height,
            storage,
            rgba_start,
            rgba_len,
            encoded_key,
        })
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn rgba8(&self) -> &[u8] {
        &self.storage[self.rgba_start..self.rgba_start + self.rgba_len]
    }
    pub fn encoded_key(&self) -> [u8; 32] {
        self.encoded_key
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageMip {
    width: u32,
    height: u32,
    rgba8: Arc<[u8]>,
}

impl ImageMip {
    pub(crate) fn new(width: u32, height: u32, rgba8: Vec<u8>) -> Self {
        Self {
            width,
            height,
            rgba8: rgba8.into(),
        }
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn rgba8(&self) -> &[u8] {
        &self.rgba8
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageAsset {
    decoded: Arc<DecodedImage>,
    role: ImageRole,
    color_policy: ImageColorPolicy,
    sampler: ImageSampler,
    mip_policy: MipPolicy,
    mips: Arc<[ImageMip]>,
    key: LeafCookKey,
}

impl ImageAsset {
    pub(crate) fn new(
        decoded: Arc<DecodedImage>,
        role: ImageRole,
        color_policy: ImageColorPolicy,
        sampler: ImageSampler,
        mip_policy: MipPolicy,
        mips: Vec<ImageMip>,
        key: LeafCookKey,
    ) -> Self {
        Self {
            decoded,
            role,
            color_policy,
            sampler,
            mip_policy,
            mips: mips.into(),
            key,
        }
    }
    pub fn decoded(&self) -> &Arc<DecodedImage> {
        &self.decoded
    }
    pub fn role(&self) -> ImageRole {
        self.role
    }
    pub fn color_policy(&self) -> ImageColorPolicy {
        self.color_policy
    }
    pub fn sampler(&self) -> ImageSampler {
        self.sampler
    }
    pub fn mip_policy(&self) -> MipPolicy {
        self.mip_policy
    }
    pub fn mips(&self) -> &[ImageMip] {
        &self.mips
    }
    pub fn key(&self) -> LeafCookKey {
        self.key
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelImageIdentity {
    owner: StableId,
    image_index: u32,
}

impl ModelImageIdentity {
    pub fn new(owner: StableId, image_index: u32) -> Self {
        Self { owner, image_index }
    }
    pub fn owner(&self) -> &StableId {
        &self.owner
    }
    pub fn image_index(&self) -> u32 {
        self.image_index
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelOwnedImage {
    identity: ModelImageIdentity,
    asset: Arc<ImageAsset>,
}

impl ModelOwnedImage {
    pub(crate) fn new(identity: ModelImageIdentity, asset: Arc<ImageAsset>) -> Self {
        Self { identity, asset }
    }
    pub fn identity(&self) -> &ModelImageIdentity {
        &self.identity
    }
    pub fn asset(&self) -> &Arc<ImageAsset> {
        &self.asset
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelTextureRef {
    owner_image: ModelImageIdentity,
    sampler: ImageSampler,
}

impl ModelTextureRef {
    pub fn new(owner_image: ModelImageIdentity, sampler: ImageSampler) -> Self {
        Self {
            owner_image,
            sampler,
        }
    }
    pub fn owner_image(&self) -> &ModelImageIdentity {
        &self.owner_image
    }
    pub fn sampler(&self) -> ImageSampler {
        self.sampler
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelMaterial {
    pub base_color_factor: [f32; 4],
    pub metallic_factor: f32,
    pub roughness_factor: f32,
    pub base_color_texture: Option<ModelTextureRef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelPrimitive {
    primitive_ordinal: u32,
    material_index: Option<u32>,
    geometry: Arc<GeometryResource>,
}

impl ModelPrimitive {
    pub(crate) fn new(
        primitive_ordinal: u32,
        material_index: Option<u32>,
        geometry: Arc<GeometryResource>,
    ) -> Self {
        Self {
            primitive_ordinal,
            material_index,
            geometry,
        }
    }
    pub fn primitive_ordinal(&self) -> u32 {
        self.primitive_ordinal
    }
    pub fn material_index(&self) -> Option<u32> {
        self.material_index
    }
    pub fn geometry(&self) -> &Arc<GeometryResource> {
        &self.geometry
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelNode {
    parent: Option<u32>,
    local_transform: [f32; 16],
    primitive_indices: Arc<[u32]>,
}

impl ModelNode {
    pub fn new(
        parent: Option<u32>,
        local_transform: [f32; 16],
        primitive_indices: Vec<u32>,
    ) -> Self {
        Self {
            parent,
            local_transform,
            primitive_indices: primitive_indices.into(),
        }
    }
    pub fn parent(&self) -> Option<u32> {
        self.parent
    }
    pub fn local_transform(&self) -> &[f32; 16] {
        &self.local_transform
    }
    pub fn primitive_indices(&self) -> &[u32] {
        &self.primitive_indices
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StaticModel {
    key: LeafCookKey,
    selected_scene_roots: Arc<[u32]>,
    nodes: Arc<[ModelNode]>,
    primitives: Arc<[ModelPrimitive]>,
    materials: Arc<[ModelMaterial]>,
    owned_images: Arc<[ModelOwnedImage]>,
}

impl StaticModel {
    #[allow(clippy::too_many_arguments)] // Keeps model payload validation at its immutable boundary.
    pub(crate) fn new(
        key: LeafCookKey,
        selected_scene_roots: Vec<u32>,
        nodes: Vec<ModelNode>,
        primitives: Vec<ModelPrimitive>,
        materials: Vec<ModelMaterial>,
        owned_images: Vec<ModelOwnedImage>,
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        let roots_are_valid = {
            let mut unique = std::collections::BTreeSet::new();
            selected_scene_roots.iter().all(|root| {
                unique.insert(*root)
                    && nodes
                        .get(*root as usize)
                        .is_some_and(|node| node.parent.is_none())
            })
        };
        let hierarchy_is_valid = nodes.iter().enumerate().all(|(start, _)| {
            let mut seen = std::collections::BTreeSet::new();
            let mut current = Some(start as u32);
            let mut depth = 0usize;
            while let Some(index) = current {
                if !seen.insert(index) || depth > limits.max_dependency_depth {
                    return false;
                }
                current = nodes.get(index as usize).and_then(|node| node.parent);
                depth += 1;
            }
            true
        });
        if nodes.len() > limits.max_model_nodes
            || primitives.len() > limits.max_model_primitives
            || materials.len() > limits.max_model_materials
            || !roots_are_valid
            || !hierarchy_is_valid
            || nodes.iter().any(|node| {
                node.parent
                    .is_some_and(|parent| parent as usize >= nodes.len())
                    || node
                        .primitive_indices
                        .iter()
                        .any(|index| *index as usize >= primitives.len())
            })
            || primitives.iter().any(|primitive| {
                primitive
                    .material_index
                    .is_some_and(|index| index as usize >= materials.len())
            })
            || materials
                .iter()
                .filter_map(|material| material.base_color_texture.as_ref())
                .any(|texture| {
                    !owned_images
                        .iter()
                        .any(|image| image.identity == texture.owner_image)
                })
            || nodes
                .iter()
                .flat_map(|node| node.local_transform)
                .any(|value| !value.is_finite())
            || materials.iter().any(|material| {
                material
                    .base_color_factor
                    .iter()
                    .chain([&material.metallic_factor, &material.roughness_factor])
                    .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            })
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location,
                "static model has invalid scene roots, references, transforms, materials, or configured counts",
            ));
        }
        Ok(Self {
            key,
            selected_scene_roots: selected_scene_roots.into(),
            nodes: nodes.into(),
            primitives: primitives.into(),
            materials: materials.into(),
            owned_images: owned_images.into(),
        })
    }
    pub fn key(&self) -> LeafCookKey {
        self.key
    }
    pub fn selected_scene_roots(&self) -> &[u32] {
        &self.selected_scene_roots
    }
    pub fn nodes(&self) -> &[ModelNode] {
        &self.nodes
    }
    pub fn primitives(&self) -> &[ModelPrimitive] {
        &self.primitives
    }
    pub fn materials(&self) -> &[ModelMaterial] {
        &self.materials
    }
    pub fn owned_images(&self) -> &[ModelOwnedImage] {
        &self.owned_images
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AtlasRegion {
    id: StableId,
    rect: [u32; 4],
    pivot: [f32; 2],
    pixels_per_unit: f32,
}

impl AtlasRegion {
    pub fn new(id: StableId, rect: [u32; 4], pivot: [f32; 2], pixels_per_unit: f32) -> Self {
        Self {
            id,
            rect,
            pivot,
            pixels_per_unit,
        }
    }
    pub fn id(&self) -> &StableId {
        &self.id
    }
    pub fn rect(&self) -> [u32; 4] {
        self.rect
    }
    pub fn pivot(&self) -> [f32; 2] {
        self.pivot
    }
    pub fn pixels_per_unit(&self) -> f32 {
        self.pixels_per_unit
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpriteAtlas {
    image_asset: StableId,
    regions: Arc<[AtlasRegion]>,
    key: LeafCookKey,
}

impl SpriteAtlas {
    pub(crate) fn new(
        image_asset: StableId,
        regions: Vec<AtlasRegion>,
        key: LeafCookKey,
        limits: &ImportLimits,
        image_width: u32,
        image_height: u32,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        let mut ids = std::collections::BTreeSet::new();
        if regions.len() > limits.max_atlas_regions
            || regions.iter().any(|region| {
                let [x, y, width, height] = region.rect;
                !ids.insert(region.id.clone())
                    || width == 0
                    || height == 0
                    || x.checked_add(width).is_none_or(|right| right > image_width)
                    || y.checked_add(height)
                        .is_none_or(|bottom| bottom > image_height)
                    || region.pivot.iter().any(|value| !value.is_finite())
                    || !region.pixels_per_unit.is_finite()
                    || region.pixels_per_unit <= 0.0
            })
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location,
                "sprite atlas region is duplicate, out of bounds, or invalid",
            ));
        }
        Ok(Self {
            image_asset,
            regions: regions.into(),
            key,
        })
    }
    pub fn image_asset(&self) -> &StableId {
        &self.image_asset
    }
    pub fn regions(&self) -> &[AtlasRegion] {
        &self.regions
    }
    pub fn key(&self) -> LeafCookKey {
        self.key
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioAsset {
    sample_rate: u32,
    channels: u16,
    frames: u64,
    samples: Arc<[f32]>,
    key: LeafCookKey,
}

impl AudioAsset {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        validate_audio(sample_rate, channels, &samples, limits, &location)?;
        let key = audio_key(sample_rate, channels, &samples);
        Ok(Self::from_validated(sample_rate, channels, samples, key))
    }

    pub(crate) fn imported(
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
        key: LeafCookKey,
        limits: &ImportLimits,
        location: ImportLocation,
    ) -> Result<Self, AssetImportError> {
        validate_audio(sample_rate, channels, &samples, limits, &location)?;
        Ok(Self::from_validated(sample_rate, channels, samples, key))
    }

    fn from_validated(
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
        key: LeafCookKey,
    ) -> Self {
        let frames = if channels == 0 {
            0
        } else {
            samples.len() as u64 / u64::from(channels)
        };
        Self {
            sample_rate,
            channels,
            frames,
            samples: samples.into(),
            key,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
    pub fn key(&self) -> LeafCookKey {
        self.key
    }
}

fn validate_audio(
    sample_rate: u32,
    channels: u16,
    samples: &[f32],
    limits: &ImportLimits,
    location: &ImportLocation,
) -> Result<(), AssetImportError> {
    let frames = if channels == 0 {
        0
    } else {
        samples.len() as u64 / u64::from(channels)
    };
    if !(8_000..=384_000).contains(&sample_rate)
        || !(1..=2).contains(&channels)
        || !samples.len().is_multiple_of(usize::from(channels.max(1)))
        || frames > limits.max_audio_frames
        || samples.iter().any(|sample| !sample.is_finite())
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Invalid,
            location.clone(),
            "PCM audio violates sample rate, channel, frame, or finite sample constraints",
        ));
    }
    Ok(())
}

fn audio_key(sample_rate: u32, channels: u16, samples: &[f32]) -> LeafCookKey {
    let mut hasher = Sha256::new();
    let domain = b"daedalus.audio.pcm-f32.v1";
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update(sample_rate.to_le_bytes());
    hasher.update(channels.to_le_bytes());
    hasher.update((samples.len() as u64).to_le_bytes());
    for sample in samples {
        hasher.update(sample.to_bits().to_le_bytes());
    }
    LeafCookKey(hasher.finalize().into())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Model,
    Image,
    SpriteAtlas,
    Audio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CookedResourceKind {
    Model,
    Geometry,
    Image,
    Atlas,
    Audio,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CookedResourceKeySummary {
    kind: CookedResourceKind,
    key: String,
}

impl CookedResourceKeySummary {
    pub fn kind(&self) -> CookedResourceKind {
        self.kind
    }
    pub fn key(&self) -> &str {
        &self.key
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetDependencySummary {
    source_id: String,
    sha256: String,
    byte_len: u64,
}

impl AssetDependencySummary {
    pub(crate) fn new(source_id: String, sha256: String, byte_len: u64) -> Self {
        Self {
            source_id,
            sha256,
            byte_len,
        }
    }
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImportedAsset {
    Model(Arc<StaticModel>),
    Image(Arc<ImageAsset>),
    Atlas(Arc<SpriteAtlas>),
    Audio(Arc<AudioAsset>),
}

impl ImportedAsset {
    pub fn kind(&self) -> AssetKind {
        match self {
            Self::Model(_) => AssetKind::Model,
            Self::Image(_) => AssetKind::Image,
            Self::Atlas(_) => AssetKind::SpriteAtlas,
            Self::Audio(_) => AssetKind::Audio,
        }
    }
    pub fn primary_key(&self) -> LeafCookKey {
        match self {
            Self::Model(model) => model.key(),
            Self::Image(image) => image.key(),
            Self::Atlas(atlas) => atlas.key(),
            Self::Audio(audio) => audio.key(),
        }
    }
    pub fn leaf_keys(&self) -> Vec<LeafCookKey> {
        match self {
            Self::Model(model) => {
                let mut keys = vec![model.key()];
                for primitive in model.primitives.iter() {
                    if !keys.contains(&primitive.geometry.key()) {
                        keys.push(primitive.geometry.key());
                    }
                }
                for image in model.owned_images.iter() {
                    if !keys.contains(&image.asset.key()) {
                        keys.push(image.asset.key());
                    }
                }
                keys
            }
            _ => vec![self.primary_key()],
        }
    }
}

/// Bounded metadata suitable for a JSON project-cook summary; contains no payload arrays.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetSummary {
    asset_id: StableId,
    kind: AssetKind,
    primary_key: String,
    resource_keys: Vec<CookedResourceKeySummary>,
    dependencies: Vec<AssetDependencySummary>,
    nodes: usize,
    primitives: usize,
    unique_geometry: usize,
    referenced_geometry: usize,
    vertices: u64,
    indices: u64,
    materials: usize,
    images: usize,
    width: Option<u32>,
    height: Option<u32>,
    image_role: Option<ImageRole>,
    mip_policy: Option<MipPolicy>,
    mip_levels: usize,
    atlas_regions: usize,
    audio_frames: u64,
    audio_channels: u16,
    audio_sample_rate: u32,
}

impl AssetSummary {
    pub(crate) fn from_asset(
        asset_id: StableId,
        asset: &ImportedAsset,
        dependencies: Vec<AssetDependencySummary>,
    ) -> Self {
        let mut summary = Self {
            asset_id,
            kind: asset.kind(),
            primary_key: asset.primary_key().hex(),
            resource_keys: Vec::new(),
            dependencies,
            nodes: 0,
            primitives: 0,
            unique_geometry: 0,
            referenced_geometry: 0,
            vertices: 0,
            indices: 0,
            materials: 0,
            images: 0,
            width: None,
            height: None,
            image_role: None,
            mip_policy: None,
            mip_levels: 0,
            atlas_regions: 0,
            audio_frames: 0,
            audio_channels: 0,
            audio_sample_rate: 0,
        };
        match asset {
            ImportedAsset::Model(model) => {
                summary.nodes = model.nodes.len();
                summary.primitives = model.primitives.len();
                summary.materials = model.materials.len();
                summary.images = model.owned_images.len();
                summary.resource_keys.push(CookedResourceKeySummary {
                    kind: CookedResourceKind::Model,
                    key: model.key.hex(),
                });
                let mut unique_geometry = std::collections::BTreeMap::<String, (u64, u64)>::new();
                for primitive in model.primitives.iter() {
                    let geometry = &primitive.geometry;
                    unique_geometry.entry(geometry.key.hex()).or_insert((
                        geometry.vertex_count() as u64,
                        geometry.index_count() as u64,
                    ));
                }
                summary.referenced_geometry = model.primitives.len();
                summary.unique_geometry = unique_geometry.len();
                for (key, (vertices, indices)) in unique_geometry {
                    summary.vertices += vertices;
                    summary.indices += indices;
                    summary.resource_keys.push(CookedResourceKeySummary {
                        kind: CookedResourceKind::Geometry,
                        key,
                    });
                }
                let mut image_keys = std::collections::BTreeSet::new();
                for image in model.owned_images.iter() {
                    if image_keys.insert(image.asset.key.hex()) {
                        summary.resource_keys.push(CookedResourceKeySummary {
                            kind: CookedResourceKind::Image,
                            key: image.asset.key.hex(),
                        });
                    }
                }
            }
            ImportedAsset::Image(image) => {
                summary.width = Some(image.decoded.width);
                summary.height = Some(image.decoded.height);
                summary.images = 1;
                summary.image_role = Some(image.role);
                summary.mip_policy = Some(image.mip_policy);
                summary.mip_levels = image.mips.len() + 1;
                summary.resource_keys.push(CookedResourceKeySummary {
                    kind: CookedResourceKind::Image,
                    key: image.key.hex(),
                });
            }
            ImportedAsset::Atlas(atlas) => {
                summary.atlas_regions = atlas.regions.len();
                summary.resource_keys.push(CookedResourceKeySummary {
                    kind: CookedResourceKind::Atlas,
                    key: atlas.key.hex(),
                });
            }
            ImportedAsset::Audio(audio) => {
                summary.audio_frames = audio.frames;
                summary.audio_channels = audio.channels;
                summary.audio_sample_rate = audio.sample_rate;
                summary.resource_keys.push(CookedResourceKeySummary {
                    kind: CookedResourceKind::Audio,
                    key: audio.key.hex(),
                });
            }
        }
        summary
    }
    pub fn asset_id(&self) -> &StableId {
        &self.asset_id
    }
    pub fn kind(&self) -> AssetKind {
        self.kind
    }
    pub fn primary_key(&self) -> &str {
        &self.primary_key
    }
    pub fn resource_keys(&self) -> &[CookedResourceKeySummary] {
        &self.resource_keys
    }
    pub fn dependencies(&self) -> &[AssetDependencySummary] {
        &self.dependencies
    }
    pub fn nodes(&self) -> usize {
        self.nodes
    }
    pub fn primitives(&self) -> usize {
        self.primitives
    }
    pub fn unique_geometry(&self) -> usize {
        self.unique_geometry
    }
    pub fn referenced_geometry(&self) -> usize {
        self.referenced_geometry
    }
    pub fn vertices(&self) -> u64 {
        self.vertices
    }
    pub fn indices(&self) -> u64 {
        self.indices
    }
    pub fn materials(&self) -> usize {
        self.materials
    }
    pub fn images(&self) -> usize {
        self.images
    }
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.width.zip(self.height)
    }
    pub fn image_role(&self) -> Option<ImageRole> {
        self.image_role
    }
    pub fn mip_policy(&self) -> Option<MipPolicy> {
        self.mip_policy
    }
    pub fn mip_levels(&self) -> usize {
        self.mip_levels
    }
    pub fn atlas_regions(&self) -> usize {
        self.atlas_regions
    }
    pub fn audio(&self) -> (u64, u16, u32) {
        (
            self.audio_frames,
            self.audio_channels,
            self.audio_sample_rate,
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetImportMetrics {
    pub source_reads: u64,
    pub source_bytes: u64,
    pub container_parses: u64,
    pub range_hash_bytes: u64,
    pub png_base_decodes: u64,
    pub model_geometry_cooks: u64,
    pub image_cooks: u64,
    pub atlas_cooks: u64,
    pub audio_cooks: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_corrupt: u64,
    pub cache_writes: u64,
    pub cache_bytes_current: u64,
    pub cache_entries_current: u64,
    pub catalog_metadata_bytes: u64,
}
