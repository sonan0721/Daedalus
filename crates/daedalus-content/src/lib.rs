//! Host-neutral, serializable content data and CPU world algorithms.
//!
//! This crate deliberately owns no ECS, renderer, I/O runtime, or game-specific rules.

pub mod asset_replacement;
pub mod assets;
pub mod navigation;
pub mod project;
pub mod scene;
pub mod tile;
pub mod voxel;

pub use assets::{
    AssetDependencySummary, AssetImportContext, AssetImportError, AssetImportErrorCode,
    AssetImportMetrics, AssetImportReport, AssetKind, AssetSourceSummary, AssetSummary,
    AtlasRegion, AudioAsset, BudgetKind, ByteMapSourceResolver, CacheCorruption, CacheKey,
    CacheLimits, CacheNamespace, CacheObjectKind, CachePublish, CacheRead, CacheStats,
    CookedResourceKeySummary, CookedResourceKind, CpuCookCache, DecodedImage, FileCookCache,
    GeometryResource, ImageAsset, ImageColorPolicy, ImageMip, ImageRole, ImageSampler,
    ImportCancellation, ImportLimits, ImportLocation, ImportedAsset, LeafCookKey, MemoryCookCache,
    MipPolicy, MipmapFilter, ModelImageIdentity, ModelMaterial, ModelNode, ModelOwnedImage,
    ModelPrimitive, ModelTextureRef, NativeProjectResolver, PreparedAssetCatalog, SourceBlob,
    SourceId, SourceResolver, SpriteAtlas, StaticModel, TemporaryBudget, TextureFilter,
    TextureWrap,
};

pub use project::{
    PreparedCamera2dBinding, PreparedEntity, PreparedModelRendererBinding, PreparedProject,
    PreparedSpriteRendererBinding, ProjectCamera, ProjectDirectionalLight, ProjectError,
    ProjectFileIdentity, ProjectLimits, ProjectManifest, ProjectMaterial, ProjectMesh,
    ProjectPresentation, ProjectTransform, ProjectVertex, load_project, load_project_with_assets,
    load_project_with_limits, prepare_project_from_bytes, prepare_project_with_assets,
};

pub use asset_replacement::{
    AssetReplacementCompatibilityError, AssetReplacementMismatch,
    check_asset_replacement_compatibility,
};

/// A non-negative monotonically increasing revision used by content snapshots.
pub type Revision = u64;
