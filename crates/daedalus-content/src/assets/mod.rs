//! Bounded, host-neutral import/cook types and shared source/cache context.

mod cache;
mod importers;
mod source;
mod types;

#[cfg(test)]
mod tests;

pub(crate) use importers::import_asset;

pub use cache::{
    CacheCorruption, CacheLease, CacheLimits, CacheObjectKind, CachePin, CachePublish,
    CachePublishResult, CacheRead, CacheStats, CpuCookCache, FileCookCache, MemoryCookCache,
};
pub use source::{
    ByteMapSourceResolver, NativeProjectResolver, SourceBlob, SourceId, SourceResolver,
};
pub use types::{
    AssetDependencySummary, AssetImportError, AssetImportErrorCode, AssetImportMetrics, AssetKind,
    AssetSummary, AtlasRegion, AudioAsset, CacheKey, CacheNamespace, CookedResourceKeySummary,
    CookedResourceKind, DecodedImage, GeometryResource, ImageAsset, ImageColorPolicy, ImageMip,
    ImageRole, ImageSampler, ImportCancellation, ImportLimits, ImportLocation, ImportedAsset,
    LeafCookKey, MipPolicy, MipmapFilter, ModelImageIdentity, ModelMaterial, ModelNode,
    ModelOwnedImage, ModelPrimitive, ModelTextureRef, SpriteAtlas, StaticModel, TextureFilter,
    TextureWrap,
};

/// Selects how PNG color metadata affects the decoded base pixels.
///
/// Standalone images normalize supported source gamma to encoded sRGB. glTF images retain their
/// encoded samples because glTF material properties define the texture transfer semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PngColorPolicy {
    SourceProfileToSrgb,
    GltfMaterialEncoded,
}

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::project::ProjectFileIdentity;
use crate::scene::StableId;

use self::types::domain_hash;

/// Immutable typed project asset map, sharing cooked resources through `Arc` identities.
#[derive(Clone, Debug)]
pub struct PreparedAssetCatalog {
    assets: BTreeMap<StableId, ImportedAsset>,
    sources: Vec<ProjectFileIdentity>,
    dependencies: BTreeMap<StableId, Vec<types::AssetDependencySummary>>,
    summaries: Vec<AssetSummary>,
    digest: String,
    // The pins keep on-disk cache entries resident for this catalog's lifetime.
    _cache_leases: Vec<cache::CachePin>,
}

fn update_catalog_hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

impl PartialEq for PreparedAssetCatalog {
    fn eq(&self, other: &Self) -> bool {
        self.assets == other.assets
            && self.sources == other.sources
            && self.dependencies == other.dependencies
            && self.summaries == other.summaries
            && self.digest == other.digest
    }
}

impl PreparedAssetCatalog {
    pub(crate) fn new(
        assets: BTreeMap<StableId, ImportedAsset>,
        sources: Vec<ProjectFileIdentity>,
        dependencies: BTreeMap<StableId, Vec<types::AssetDependencySummary>>,
        cache_leases: Vec<cache::CachePin>,
    ) -> Self {
        let summaries = assets
            .iter()
            .map(|(id, asset)| {
                AssetSummary::from_asset(
                    id.clone(),
                    asset,
                    dependencies.get(id).cloned().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        let mut hasher = Sha256::new();
        let domain = b"daedalus.asset-catalog.v1";
        hasher.update((domain.len() as u64).to_le_bytes());
        hasher.update(domain);
        for (id, asset) in &assets {
            update_catalog_hash_part(&mut hasher, id.as_str().as_bytes());
            update_catalog_hash_part(&mut hasher, &asset.primary_key().digest());
        }
        for source in &sources {
            update_catalog_hash_part(&mut hasher, source.source_id.as_bytes());
            update_catalog_hash_part(&mut hasher, source.content_sha256.as_bytes());
        }
        let digest = types::encode_hex(&hasher.finalize());
        Self {
            assets,
            sources,
            dependencies,
            summaries,
            digest,
            _cache_leases: cache_leases,
        }
    }

    pub fn asset(&self, id: &StableId) -> Option<&ImportedAsset> {
        self.assets.get(id)
    }
    pub(crate) fn assets_map(&self) -> &BTreeMap<StableId, ImportedAsset> {
        &self.assets
    }
    pub fn assets(&self) -> impl Iterator<Item = (&StableId, &ImportedAsset)> {
        self.assets.iter()
    }
    pub fn sources(&self) -> &[ProjectFileIdentity] {
        &self.sources
    }
    pub fn dependencies(&self, id: &StableId) -> &[types::AssetDependencySummary] {
        self.dependencies.get(id).map_or(&[], Vec::as_slice)
    }
    pub fn summaries(&self) -> &[AssetSummary] {
        &self.summaries
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn len(&self) -> usize {
        self.assets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    pub(crate) fn release_cache_pins(&mut self) {
        self._cache_leases.clear();
    }
}

/// Hash-only source summary safe for bounded CLI JSON output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetSourceSummary {
    source_id: String,
    sha256: String,
    byte_len: u64,
}

impl AssetSourceSummary {
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

/// Per-call accounting and bounded asset/source summaries; never cumulative across calls.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetImportReport {
    catalog_digest: String,
    assets: Vec<AssetSummary>,
    sources: Vec<AssetSourceSummary>,
    metrics: AssetImportMetrics,
}

impl AssetImportReport {
    pub(crate) fn new(
        catalog: &PreparedAssetCatalog,
        sources: Vec<SourceBlob>,
        metrics: AssetImportMetrics,
    ) -> Self {
        let sources = sources
            .into_iter()
            .map(|source| AssetSourceSummary {
                source_id: source.source_id().to_string(),
                sha256: types::encode_hex(&source.sha256()),
                byte_len: source.byte_len(),
            })
            .collect();
        Self {
            catalog_digest: catalog.digest().to_owned(),
            assets: catalog.summaries.clone(),
            sources,
            metrics,
        }
    }
    pub fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }
    pub fn assets(&self) -> &[AssetSummary] {
        &self.assets
    }
    pub fn sources(&self) -> &[AssetSourceSummary] {
        &self.sources
    }
    pub fn metrics(&self) -> &AssetImportMetrics {
        &self.metrics
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BudgetKind {
    SourceBytes,
    DecodedImageBytes,
    CookedBytes,
    ParserScratchBytes,
    GeometryVertices,
    GeometryIndices,
    ModelNodes,
    ModelPrimitives,
    ModelMaterials,
    DecodedImages,
    AtlasRegions,
    AudioFrames,
}

#[derive(Clone, Debug, Default)]
struct BudgetState(BTreeMap<BudgetKind, u64>);

/// Temporary checked allocation allowance. Drop returns the reserved bytes.
#[derive(Debug)]
pub struct TemporaryBudget {
    state: Arc<Mutex<BudgetState>>,
    kind: BudgetKind,
    amount: u64,
}

impl Drop for TemporaryBudget {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock()
            && let Some(value) = state.0.get_mut(&self.kind)
        {
            *value = value.saturating_sub(self.amount);
            if *value == 0 {
                state.0.remove(&self.kind);
            }
        }
    }
}

fn reserve_shared_budget(
    state: Arc<Mutex<BudgetState>>,
    kind: BudgetKind,
    limit: u64,
    amount: u64,
    location: ImportLocation,
) -> Result<TemporaryBudget, AssetImportError> {
    let mut ledger = state.lock().map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::Internal,
            location.clone(),
            "budget ledger is unavailable",
        )
    })?;
    let current = ledger.0.get(&kind).copied().unwrap_or_default();
    let next = current.checked_add(amount).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location.clone(),
            "budget counter overflow",
        )
    })?;
    if next > limit {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location,
            format!("{kind:?} budget would exceed configured limit {limit}"),
        ));
    }
    ledger.0.insert(kind, next);
    drop(ledger);
    Ok(TemporaryBudget {
        state,
        kind,
        amount,
    })
}

/// Shared, importer-facing services for dependency reads, checked budgets, keys, and cache.
///
/// Constructed by the project preparation pipeline. It has no public constructor because the
/// core owns aggregate accounting and current-source identity.
pub struct AssetImportContext<'a> {
    resolver: &'a mut dyn SourceResolver,
    cache: &'a mut dyn CpuCookCache,
    limits: ImportLimits,
    cancellation: &'a ImportCancellation,
    current_source: Option<SourceId>,
    sources: BTreeMap<SourceId, Arc<SourceBlob>>,
    source_parents: BTreeMap<SourceId, SourceId>,
    current_asset: Option<StableId>,
    asset_dependencies: BTreeMap<StableId, BTreeSet<String>>,
    asset_reference_dependencies: BTreeMap<StableId, BTreeSet<StableId>>,
    prepared_assets: BTreeMap<StableId, ImportedAsset>,
    usage: Arc<Mutex<BudgetState>>,
    metrics: AssetImportMetrics,
    cache_start: crate::assets::CacheStats,
    decoded_png: BTreeMap<[u8; 32], Arc<DecodedImage>>,
    cache_leases: Vec<cache::CachePin>,
    memoized_assets: BTreeMap<[u8; 32], ImportedAsset>,
}

const PNG_DECODE_CACHE_DOMAIN_V2: &[u8] = b"daedalus.png.decode.rgba8-srgb-normalize-v2";
const PNG_BASE_CACHE_DOMAIN_V2: &[u8] = b"daedalus.png.base-cache.rgba8-srgb-normalize-v2";

fn png_gamma_to_normalize(
    info: &png::Info<'_>,
    policy: PngColorPolicy,
    location: ImportLocation,
) -> Result<Option<u32>, AssetImportError> {
    if policy == PngColorPolicy::GltfMaterialEncoded {
        return Ok(None);
    }

    if info.icc_profile.is_some()
        || info.coding_independent_code_points.is_some()
        || info.mastering_display_color_volume.is_some()
        || info.content_light_level.is_some()
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location,
            "PNG has an ICC, HDR, or unsupported color profile",
        ));
    }

    // PNG specifies sRGB as authoritative when present, even if gAMA/cHRM chunks disagree.
    if info.srgb.is_some() {
        return Ok(None);
    }

    if let Some(chrm) = info.chrm_chunk {
        let coordinates = [
            chrm.white.0.into_scaled(),
            chrm.white.1.into_scaled(),
            chrm.red.0.into_scaled(),
            chrm.red.1.into_scaled(),
            chrm.green.0.into_scaled(),
            chrm.green.1.into_scaled(),
            chrm.blue.0.into_scaled(),
            chrm.blue.1.into_scaled(),
        ];
        if coordinates
            != [
                31_270, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
            ]
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location,
                "PNG chromaticities are outside the supported sRGB subset",
            ));
        }
    }

    match info.gama_chunk.map(png::ScaledFloat::into_scaled) {
        Some(0) => Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location,
            "PNG source gamma must be positive",
        )),
        gamma => Ok(gamma),
    }
}

fn png_effective_color_key(gamma_scaled: Option<u32>) -> [u8; 5] {
    let mut key = [0; 5];
    if let Some(gamma) = gamma_scaled {
        key[0] = 1;
        key[1..].copy_from_slice(&gamma.to_le_bytes());
    }
    key
}

fn png_gamma_to_srgb_lut(gamma_scaled: u32) -> [u8; 256] {
    std::array::from_fn(|sample| {
        let encoded = sample as f64 / 255.0;
        let linear = encoded.powf(100_000.0 / f64::from(gamma_scaled));
        let srgb = if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (srgb * 255.0).round().clamp(0.0, 255.0) as u8
    })
}

impl<'a> AssetImportContext<'a> {
    pub(crate) fn new(
        resolver: &'a mut dyn SourceResolver,
        cache: &'a mut dyn CpuCookCache,
        limits: ImportLimits,
        cancellation: &'a ImportCancellation,
        initial_sources: impl IntoIterator<Item = SourceBlob>,
    ) -> Result<Self, AssetImportError> {
        let mut sources = BTreeMap::new();
        let mut source_bytes = 0u64;
        let mut metrics = AssetImportMetrics::default();
        for source in initial_sources {
            if source.byte_len() > limits.max_source_bytes {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    ImportLocation::new(source.source_id().to_string(), "source"),
                    "initial project source exceeds the configured per-source byte limit",
                ));
            }
            source_bytes = source_bytes.checked_add(source.byte_len()).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    ImportLocation::new(source.source_id().to_string(), "source"),
                    "initial project source byte count overflow",
                )
            })?;
            metrics.source_reads = metrics.source_reads.saturating_add(1);
            metrics.source_bytes = metrics.source_bytes.saturating_add(source.byte_len());
            sources.insert(source.source_id().clone(), Arc::new(source));
        }
        if sources.len() > limits.max_unique_sources || source_bytes > limits.max_total_source_bytes
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "manifest and scene exceed the aggregate source budget",
            ));
        }
        let mut usage = BudgetState::default();
        usage.0.insert(BudgetKind::SourceBytes, source_bytes);
        let cache_start = cache.stats();
        Ok(Self {
            resolver,
            cache,
            limits,
            cancellation,
            current_source: None,
            sources,
            source_parents: BTreeMap::new(),
            current_asset: None,
            asset_dependencies: BTreeMap::new(),
            asset_reference_dependencies: BTreeMap::new(),
            prepared_assets: BTreeMap::new(),
            usage: Arc::new(Mutex::new(usage)),
            metrics,
            cache_start,
            decoded_png: BTreeMap::new(),
            cache_leases: Vec::new(),
            memoized_assets: BTreeMap::new(),
        })
    }

    pub fn limits(&self) -> &ImportLimits {
        &self.limits
    }
    pub fn current_source(&self) -> Option<&SourceId> {
        self.current_source.as_ref()
    }
    pub fn begin_asset(&mut self, asset_id: StableId, source_id: SourceId) {
        self.current_asset = Some(asset_id.clone());
        self.current_source = Some(source_id.clone());
        self.asset_dependencies
            .entry(asset_id)
            .or_default()
            .insert(source_id.to_string());
    }
    pub fn set_current_source(&mut self, source_id: SourceId) {
        self.current_source = Some(source_id);
    }
    pub fn check_cancelled(&self, location: ImportLocation) -> Result<(), AssetImportError> {
        self.cancellation_point(location)
    }
    pub fn cancellation_point(&self, location: ImportLocation) -> Result<(), AssetImportError> {
        if self.cancellation.is_cancelled() {
            Err(AssetImportError::cancelled(location))
        } else {
            Ok(())
        }
    }

    /// Resolve a parser URI relative to its source. Repeated normalized IDs are read and hashed once.
    pub fn resolve_uri(
        &mut self,
        parent: &SourceId,
        uri: &str,
        max_bytes: u64,
        location: ImportLocation,
    ) -> Result<Arc<SourceBlob>, AssetImportError> {
        self.cancellation_point(location.clone())?;
        let source_id = SourceId::resolve_relative(parent, uri).map_err(|error| {
            AssetImportError::new(error.code(), location.clone(), error.message())
        })?;
        let mut ancestors = Vec::<SourceId>::new();
        let mut cursor = Some(parent.clone());
        let mut seen = BTreeSet::new();
        while let Some(source) = cursor {
            if !seen.insert(source.clone()) {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::Invalid,
                    location.clone(),
                    "source dependency graph contains a cycle",
                ));
            }
            ancestors.push(source.clone());
            cursor = self.source_parents.get(&source).cloned();
            if ancestors.len() > self.limits.max_dependency_depth {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "source dependency chain exceeds configured depth",
                ));
            }
        }
        if seen.contains(&source_id) {
            ancestors.push(source_id.clone());
            ancestors.reverse();
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location
                    .clone()
                    .with_dependency_chain(ancestors.into_iter().map(|item| item.to_string())),
                "source dependency introduces a cycle",
            ));
        }
        ancestors.reverse();
        ancestors.push(source_id.clone());
        let located = location
            .clone()
            .with_dependency_chain(ancestors.iter().map(ToString::to_string));
        if ancestors.len() > self.limits.max_dependency_depth {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                located,
                "source dependency chain exceeds configured depth",
            ));
        }
        if source_id.as_str().len() > self.limits.max_source_id_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                located,
                "normalized source ID exceeds its configured byte limit",
            ));
        }
        if let Some(source) = self.sources.get(&source_id).cloned() {
            if source.byte_len() > max_bytes.min(self.limits.max_source_bytes) {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    located,
                    "shared source exceeds the strictest requesting per-source byte limit",
                ));
            }
            if let Some(asset_id) = &self.current_asset {
                self.asset_dependencies
                    .entry(asset_id.clone())
                    .or_default()
                    .insert(source_id.to_string());
            }
            return Ok(source);
        }
        if self.sources.len() >= self.limits.max_unique_sources {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                located,
                "unique source count exceeds configured limit",
            ));
        }
        let current_source_bytes = self
            .usage
            .lock()
            .ok()
            .and_then(|usage| usage.0.get(&BudgetKind::SourceBytes).copied())
            .unwrap_or_default();
        let remaining_total = self
            .limits
            .max_total_source_bytes
            .saturating_sub(current_source_bytes);
        let max_bytes = max_bytes
            .min(self.limits.max_source_bytes)
            .min(remaining_total);
        if max_bytes == 0 {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                located,
                "no aggregate source budget remains",
            ));
        }
        let source = self
            .resolver
            .read_source(&source_id, max_bytes)
            .map_err(|error| {
                let error_location =
                    ImportLocation::new(source_id.to_string(), located.path().unwrap_or("source"))
                        .with_dependency_chain(located.dependency_chain().iter().cloned());
                AssetImportError::new(error.code(), error_location, error.message())
            })?;
        if source.source_id() != &source_id || source.byte_len() > max_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                located,
                "source resolver returned an unexpected identity or oversized source",
            ));
        }
        self.account(BudgetKind::SourceBytes, source.byte_len(), location.clone())?;
        self.metrics.source_reads = self.metrics.source_reads.saturating_add(1);
        self.metrics.source_bytes = self.metrics.source_bytes.saturating_add(source.byte_len());
        let source = Arc::new(source);
        if let Some(asset_id) = &self.current_asset {
            self.asset_dependencies
                .entry(asset_id.clone())
                .or_default()
                .insert(source_id.to_string());
        }
        self.source_parents
            .entry(source_id.clone())
            .or_insert_with(|| parent.clone());
        self.sources.insert(source_id, source.clone());
        Ok(source)
    }

    pub fn check_budget(
        &self,
        kind: BudgetKind,
        additional: u64,
        location: ImportLocation,
    ) -> Result<(), AssetImportError> {
        let current = self
            .usage
            .lock()
            .map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::Internal,
                    location.clone(),
                    "budget ledger is unavailable",
                )
            })?
            .0
            .get(&kind)
            .copied()
            .unwrap_or_default();
        let limit = self.budget_limit(kind);
        if current
            .checked_add(additional)
            .is_none_or(|next| next > limit)
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location,
                format!("{kind:?} budget would exceed configured limit {limit}"),
            ));
        }
        Ok(())
    }

    /// Accounts a successfully retained allocation after the caller checked before allocating.
    pub fn account(
        &mut self,
        kind: BudgetKind,
        amount: u64,
        location: ImportLocation,
    ) -> Result<(), AssetImportError> {
        self.check_budget(kind, amount, location.clone())?;
        let mut state = self.usage.lock().map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::Internal,
                location.clone(),
                "budget ledger is unavailable",
            )
        })?;
        let next = state
            .0
            .get(&kind)
            .copied()
            .unwrap_or_default()
            .checked_add(amount)
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "budget counter overflow",
                )
            })?;
        state.0.insert(kind, next);
        Ok(())
    }

    pub fn reserve_temporary(
        &self,
        kind: BudgetKind,
        amount: u64,
        location: ImportLocation,
    ) -> Result<TemporaryBudget, AssetImportError> {
        reserve_shared_budget(
            self.usage.clone(),
            kind,
            self.budget_limit(kind),
            amount,
            location,
        )
    }

    fn budget_limit(&self, kind: BudgetKind) -> u64 {
        match kind {
            BudgetKind::SourceBytes => self.limits.max_total_source_bytes,
            BudgetKind::DecodedImageBytes => self.limits.max_decoded_image_bytes,
            BudgetKind::CookedBytes => self.limits.max_cooked_bytes,
            BudgetKind::ParserScratchBytes => self.limits.max_parser_scratch_bytes,
            BudgetKind::GeometryVertices => self.limits.max_geometry_vertices as u64,
            BudgetKind::GeometryIndices => self.limits.max_geometry_indices as u64,
            BudgetKind::ModelNodes => self.limits.max_model_nodes as u64,
            BudgetKind::ModelPrimitives => self.limits.max_model_primitives as u64,
            BudgetKind::ModelMaterials => self.limits.max_model_materials as u64,
            BudgetKind::DecodedImages => self.limits.max_decoded_images as u64,
            BudgetKind::AtlasRegions => self.limits.max_atlas_regions as u64,
            BudgetKind::AudioFrames => self.limits.max_audio_frames,
        }
    }

    pub fn leaf_key(
        &self,
        domain: &str,
        version: u32,
        settings: &[u8],
        consumed_payload: &[u8],
    ) -> LeafCookKey {
        let version_bytes = version.to_le_bytes();
        LeafCookKey::from_digest(domain_hash(
            domain.as_bytes(),
            &[&version_bytes, settings, consumed_payload],
        ))
    }

    pub fn import_key(
        &self,
        importer: &str,
        version: u32,
        source: &SourceBlob,
        dependencies: &[SourceBlob],
    ) -> CacheKey {
        let version_bytes = version.to_le_bytes();
        let source_hash = source.sha256();
        let dependency_hashes = dependencies
            .iter()
            .map(SourceBlob::sha256)
            .collect::<Vec<_>>();
        let mut parts = vec![
            importer.as_bytes(),
            version_bytes.as_slice(),
            source_hash.as_slice(),
        ];
        for digest in &dependency_hashes {
            parts.push(digest.as_slice());
        }
        CacheKey::new(
            CacheNamespace::Import,
            domain_hash(b"daedalus.import.container.v1", &parts),
        )
    }

    pub fn read_cache(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        max_bytes: u64,
    ) -> Result<CacheRead, AssetImportError> {
        let read = self.read_cache_reserved(key, kind, max_bytes)?;
        match &read {
            CacheRead::Hit(lease) => {
                self.metrics.cache_hits = self.metrics.cache_hits.saturating_add(1);
                self.cache_leases.push(lease.pin());
            }
            CacheRead::Miss => {
                self.metrics.cache_misses = self.metrics.cache_misses.saturating_add(1)
            }
            CacheRead::Corrupt(_) => {
                self.metrics.cache_corrupt = self.metrics.cache_corrupt.saturating_add(1)
            }
        }
        Ok(read)
    }

    fn read_cache_reserved(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        max_bytes: u64,
    ) -> Result<CacheRead, AssetImportError> {
        let usage = self.usage.clone();
        let limit = self.limits.max_parser_scratch_bytes;
        let location = self.current_location();
        let mut reserve = move |amount| {
            reserve_shared_budget(
                usage.clone(),
                BudgetKind::ParserScratchBytes,
                limit,
                amount,
                location.clone(),
            )
        };
        self.cache
            .read_with_reservation(key, kind, max_bytes, &mut reserve)
    }

    pub fn publish_cache(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
        location: ImportLocation,
    ) -> Result<(), AssetImportError> {
        self.cancellation_point(location.clone())?;
        let usage = self.usage.clone();
        let limit = self.limits.max_parser_scratch_bytes;
        let mut reserve = move |amount| {
            reserve_shared_budget(
                usage.clone(),
                BudgetKind::ParserScratchBytes,
                limit,
                amount,
                location.clone(),
            )
        };
        let published = self
            .cache
            .publish_pinned(key, kind, payload, &mut reserve)?;
        if published.outcome() == self::cache::CachePublish::Written {
            self.metrics.cache_writes = self.metrics.cache_writes.saturating_add(1);
        }
        self.cache_leases.push(published.into_pin());
        Ok(())
    }

    /// Drops this preparation's provisional hit pins, then removes a semantically invalid cooked
    /// payload. Callers must drop the returned `CacheLease` before invoking this method.
    pub fn invalidate_cache(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        location: ImportLocation,
    ) -> Result<(), AssetImportError> {
        let matching_pins = self
            .cache_leases
            .iter()
            .filter(|pin| pin.key() == key)
            .count() as u64;
        self.cache_leases.retain(|pin| pin.key() != key);
        self.cache
            .invalidate(key, kind)
            .map_err(|error| AssetImportError::new(error.code(), location, error.message()))?;
        self.metrics.cache_hits = self.metrics.cache_hits.saturating_sub(matching_pins);
        // `read_cache` already counts an outer-envelope Corrupt result. A provisional context pin
        // exists only for a payload returned as Hit whose importer rejected its inner semantics.
        if matching_pins > 0 {
            self.metrics.cache_corrupt = self.metrics.cache_corrupt.saturating_add(1);
        }
        Ok(())
    }

    /// Counts parser work explicitly; this does not imply a CPU cache hit.
    pub fn record_container_parse(&mut self) {
        self.metrics.container_parses = self.metrics.container_parses.saturating_add(1);
    }
    pub fn record_range_hash_bytes(&mut self, amount: u64) {
        self.metrics.range_hash_bytes = self.metrics.range_hash_bytes.saturating_add(amount);
    }
    pub fn record_cook(&mut self, kind: AssetKind) {
        let counter = match kind {
            AssetKind::Model => &mut self.metrics.model_geometry_cooks,
            AssetKind::Image => &mut self.metrics.image_cooks,
            AssetKind::SpriteAtlas => &mut self.metrics.atlas_cooks,
            AssetKind::Audio => &mut self.metrics.audio_cooks,
        };
        *counter = counter.saturating_add(1);
    }

    pub fn source_blobs(&self) -> Vec<SourceBlob> {
        self.sources
            .values()
            .map(|source| source.as_ref().clone())
            .collect()
    }
    pub fn dependency_ids(&self) -> Vec<String> {
        self.current_asset
            .as_ref()
            .and_then(|id| self.asset_dependencies.get(id))
            .map(|items| items.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn asset_dependencies(&self, id: &StableId) -> Vec<String> {
        self.asset_dependencies
            .get(id)
            .map(|items| items.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn asset_dependency_ids<'b>(&'b self, id: &StableId) -> impl Iterator<Item = &'b str> + 'b {
        self.asset_dependencies
            .get(id)
            .into_iter()
            .flat_map(|items| items.iter().map(String::as_str))
    }
    pub fn asset_reference_ids(&self, id: &StableId) -> Vec<StableId> {
        self.asset_reference_dependencies
            .get(id)
            .map(|items| items.iter().cloned().collect())
            .unwrap_or_default()
    }
    /// Records a project-asset edge and carries the referenced asset's source identities into the
    /// current asset summary. This is used by atlases and other non-container references.
    pub fn record_asset_dependency(&mut self, id: &StableId) -> Result<(), AssetImportError> {
        let referenced = self.prepared_assets.get(id).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::Invalid,
                self.current_location(),
                format!("referenced asset '{id}' has not been imported"),
            )
        })?;
        if !matches!(referenced, ImportedAsset::Image(_)) {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                self.current_location(),
                format!("referenced asset '{id}' is not an image"),
            ));
        }
        let Some(current) = self.current_asset.clone() else {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Internal,
                self.current_location(),
                "asset dependency was recorded outside an active import",
            ));
        };
        self.asset_reference_dependencies
            .entry(current.clone())
            .or_default()
            .insert(id.clone());
        if let Some(dependencies) = self.asset_dependencies.get(id).cloned() {
            self.asset_dependencies
                .entry(current)
                .or_default()
                .extend(dependencies);
        }
        Ok(())
    }
    pub fn register_imported_asset(&mut self, id: StableId, asset: ImportedAsset) {
        self.prepared_assets.insert(id, asset);
    }
    pub fn prepared_asset(&self, id: &StableId) -> Option<&ImportedAsset> {
        self.prepared_assets.get(id)
    }
    pub fn image_dimensions(&self, id: &StableId) -> Option<(u32, u32)> {
        match self.prepared_asset(id)? {
            ImportedAsset::Image(image) => {
                Some((image.decoded().width(), image.decoded().height()))
            }
            _ => None,
        }
    }
    pub fn catalog_assets(&self) -> &BTreeMap<StableId, ImportedAsset> {
        &self.prepared_assets
    }
    /// Returns an immutable asset already cooked in this invocation under the same importer key.
    pub fn memoized_asset(&self, key: CacheKey) -> Option<ImportedAsset> {
        self.memoized_assets.get(&key.digest()).cloned()
    }
    /// Registers a successful source-level import for same-invocation reuse. The memo is capped
    /// by the project's asset limit and does not add a second cooked-resource budget charge.
    pub fn memoize_asset(
        &mut self,
        key: CacheKey,
        asset: ImportedAsset,
    ) -> Result<(), AssetImportError> {
        if self.memoized_assets.contains_key(&key.digest()) {
            return Ok(());
        }
        if self.memoized_assets.len() >= self.limits.max_assets {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                self.current_location(),
                "source import memo exceeds the configured asset count",
            ));
        }
        self.memoized_assets.insert(key.digest(), asset);
        Ok(())
    }
    pub(crate) fn take_cache_leases(&mut self) -> Vec<cache::CachePin> {
        std::mem::take(&mut self.cache_leases)
    }
    pub fn metrics(&self) -> AssetImportMetrics {
        let mut metrics = self.metrics.clone();
        let stats = self.cache.stats();
        metrics.cache_bytes_current = stats.current_bytes;
        metrics.cache_entries_current = stats.current_entries;
        metrics
    }
    pub fn cache_stats(&self) -> crate::assets::CacheStats {
        self.cache.stats()
    }

    pub(crate) fn cache_pin_count(&self) -> usize {
        self.cache_leases.len()
    }

    pub(crate) fn account_catalog_metadata(
        &mut self,
        amount: u64,
        location: ImportLocation,
    ) -> Result<(), AssetImportError> {
        self.account(BudgetKind::CookedBytes, amount, location)?;
        self.metrics.catalog_metadata_bytes = amount;
        Ok(())
    }
    pub fn cache_start_stats(&self) -> crate::assets::CacheStats {
        self.cache_start
    }
    pub fn note_png_base_decode(&mut self) {
        self.metrics.png_base_decodes = self.metrics.png_base_decodes.saturating_add(1);
    }
    pub fn cached_png(&self, key: &[u8; 32]) -> Option<Arc<DecodedImage>> {
        self.decoded_png.get(key).cloned()
    }
    pub fn cache_png(&mut self, key: [u8; 32], image: Arc<DecodedImage>) {
        self.decoded_png.insert(key, image);
    }

    fn current_location(&self) -> ImportLocation {
        self.current_source
            .as_ref()
            .map_or_else(ImportLocation::default, |source| {
                ImportLocation::new(source.to_string(), "asset")
            })
    }

    /// Decodes and shares one PNG RGBA8 base image per encoded-payload/settings identity.
    /// Role-specific mip chains and sampler state remain separate image assets/references.
    pub fn decode_png_base(
        &mut self,
        encoded: &[u8],
        location: ImportLocation,
    ) -> Result<Arc<DecodedImage>, AssetImportError> {
        self.decode_png_base_with_policy(encoded, location, PngColorPolicy::SourceProfileToSrgb)
    }

    /// Decodes and shares one PNG RGBA8 base image under the effective color interpretation.
    ///
    /// Header parsing precedes cache lookup so unsupported profiles cannot be smuggled through a
    /// cached payload, and policies with identical effective pixels share the same cache identity.
    pub fn decode_png_base_with_policy(
        &mut self,
        encoded: &[u8],
        location: ImportLocation,
        policy: PngColorPolicy,
    ) -> Result<Arc<DecodedImage>, AssetImportError> {
        self.cancellation_point(location.clone())?;
        let mut decoder = png::Decoder::new(Cursor::new(encoded));
        decoder.set_ignore_text_chunk(true);
        decoder.set_limits(png::Limits {
            bytes: usize::try_from(self.limits.max_decoded_image_bytes).unwrap_or(usize::MAX),
        });
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Decode,
                location.clone(),
                format!("PNG header decode failed: {error}"),
            )
        })?;
        let source_width = reader.info().width;
        let source_height = reader.info().height;
        let animated = reader.info().animation_control.is_some();
        let bit_depth = reader.info().bit_depth;
        let gamma_scaled = png_gamma_to_normalize(reader.info(), policy, location.clone())?;
        if animated {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location,
                "animated PNG is not supported by png-rgba8-v2",
            ));
        }
        if bit_depth != png::BitDepth::Eight {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location,
                format!("only 8-bit PNG sources are supported; found {bit_depth:?}"),
            ));
        }
        if source_width == 0
            || source_height == 0
            || source_width > self.limits.max_image_axis
            || source_height > self.limits.max_image_axis
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location,
                "PNG dimensions exceed configured image axes",
            ));
        }
        let effective_color_key = png_effective_color_key(gamma_scaled);
        let encoded_hash: [u8; 32] = Sha256::digest(encoded).into();
        let decode_key = domain_hash(
            PNG_DECODE_CACHE_DOMAIN_V2,
            &[&encoded_hash, &effective_color_key],
        );
        if let Some(image) = self.decoded_png.get(&decode_key) {
            return Ok(image.clone());
        }

        let cache_key = CacheKey::new(
            CacheNamespace::Cook,
            domain_hash(
                PNG_BASE_CACHE_DOMAIN_V2,
                &[&encoded_hash, &effective_color_key],
            ),
        );
        match self.read_cache(
            cache_key,
            CacheObjectKind::PngBase,
            self.limits.max_decoded_image_bytes.saturating_add(44),
        )? {
            CacheRead::Hit(lease) => {
                let payload = lease.payload();
                let cache_image = if payload.len() >= 44
                    && &payload[..4] == b"DPNG"
                    && payload[4..36] == encoded_hash
                {
                    let width = u32::from_le_bytes(
                        payload[36..40].try_into().expect("four-byte width slice"),
                    );
                    let height = u32::from_le_bytes(
                        payload[40..44].try_into().expect("four-byte height slice"),
                    );
                    let rgba = &payload[44..];
                    let expected = u64::from(width)
                        .checked_mul(u64::from(height))
                        .and_then(|pixels| pixels.checked_mul(4));
                    (width == source_width
                        && height == source_height
                        && width != 0
                        && height != 0
                        && expected == Some(rgba.len() as u64))
                    .then_some((width, height, rgba.len()))
                } else {
                    None
                };
                if let Some((width, height, rgba_len)) = cache_image {
                    if width > self.limits.max_image_axis || height > self.limits.max_image_axis {
                        return Err(AssetImportError::new(
                            AssetImportErrorCode::LimitExceeded,
                            location,
                            "cached PNG dimensions exceed configured image axes",
                        ));
                    }
                    self.check_budget(BudgetKind::DecodedImages, 1, location.clone())?;
                    self.check_budget(
                        BudgetKind::DecodedImageBytes,
                        rgba_len as u64,
                        location.clone(),
                    )?;
                    let (storage, payload_start, _) = lease.payload_storage();
                    self.account(
                        BudgetKind::DecodedImageBytes,
                        rgba_len as u64,
                        location.clone(),
                    )?;
                    self.account(BudgetKind::DecodedImages, 1, location.clone())?;
                    let image = Arc::new(DecodedImage::from_rgba8_storage(
                        width,
                        height,
                        storage,
                        payload_start + 44,
                        rgba_len,
                        encoded_hash,
                        &self.limits,
                        location,
                    )?);
                    self.decoded_png.insert(decode_key, image.clone());
                    return Ok(image);
                }
                // A self-consistent outer envelope with invalid PNG metadata is corrupt. Drop the
                // payload lease before deleting the entry, then regenerate from authored source.
                drop(lease);
                self.invalidate_cache(cache_key, CacheObjectKind::PngBase, location.clone())?;
            }
            CacheRead::Miss => {}
            CacheRead::Corrupt(_) => {
                // The outer envelope is invalid, so remove it before the regenerated payload is
                // published under the same deterministic key.
                self.invalidate_cache(cache_key, CacheObjectKind::PngBase, location.clone())?;
            }
        }

        let width = source_width;
        let height = source_height;
        let pixel_count = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG pixel count overflow",
                )
            })?;
        let rgba_bytes = pixel_count.checked_mul(4).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location.clone(),
                "PNG RGBA byte count overflow",
            )
        })?;
        self.check_budget(BudgetKind::DecodedImages, 1, location.clone())?;
        self.check_budget(BudgetKind::DecodedImageBytes, rgba_bytes, location.clone())?;
        let buffer_size = reader.output_buffer_size().ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location.clone(),
                "PNG output buffer size is not representable",
            )
        })?;
        if buffer_size as u64 > self.limits.max_decoded_image_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location,
                "PNG decoded buffer exceeds image byte limit",
            ));
        }
        let _decoder_scratch = self.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            buffer_size as u64,
            location.clone(),
        )?;
        // The converted RGBA Vec remains alive until it is moved into the immutable image Arc.
        // Hold this reservation through that conversion so peak scratch is charged continuously.
        let conversion_scratch =
            self.reserve_temporary(BudgetKind::ParserScratchBytes, rgba_bytes, location.clone())?;
        let output = {
            let mut decoded = vec![0u8; buffer_size];
            let frame = reader.next_frame(&mut decoded).map_err(|error| {
                AssetImportError::new(
                    AssetImportErrorCode::Decode,
                    location.clone(),
                    format!("PNG pixel decode failed: {error}"),
                )
            })?;
            if frame.bit_depth != png::BitDepth::Eight {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::Unsupported,
                    location,
                    format!(
                        "PNG expansion produced unsupported bit depth {:?}",
                        frame.bit_depth
                    ),
                ));
            }
            let src = &decoded[..frame.buffer_size()];
            let row_pixels = usize::try_from(width).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG row width is not representable",
                )
            })?;
            let rgba_capacity = usize::try_from(rgba_bytes).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG RGBA size is not representable",
                )
            })?;
            let channels: usize = match frame.color_type {
                png::ColorType::Rgba => 4,
                png::ColorType::Rgb => 3,
                png::ColorType::Grayscale => 1,
                png::ColorType::GrayscaleAlpha => 2,
                png::ColorType::Indexed => {
                    return Err(AssetImportError::new(
                        AssetImportErrorCode::Decode,
                        location,
                        "PNG palette was not expanded by the decoder",
                    ));
                }
            };
            let expected_input = pixel_count.checked_mul(channels as u64).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG sample count overflow",
                )
            })?;
            if src.len() as u64 != expected_input {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::Decode,
                    location,
                    "PNG output sample count does not match its dimensions",
                ));
            }
            let mut rgba = Vec::with_capacity(rgba_capacity);
            let input_row_bytes = row_pixels.checked_mul(channels).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG row sample count overflow",
                )
            })?;
            for (row_index, row) in src.chunks_exact(input_row_bytes).enumerate() {
                self.cancellation_point(location.clone())?;
                match frame.color_type {
                    png::ColorType::Rgba => rgba.extend_from_slice(row),
                    png::ColorType::Rgb => {
                        for rgb in row.chunks_exact(3) {
                            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
                        }
                    }
                    png::ColorType::Grayscale => {
                        for gray in row {
                            rgba.extend_from_slice(&[*gray, *gray, *gray, 255]);
                        }
                    }
                    png::ColorType::GrayscaleAlpha => {
                        for pair in row.chunks_exact(2) {
                            rgba.extend_from_slice(&[pair[0], pair[0], pair[0], pair[1]]);
                        }
                    }
                    png::ColorType::Indexed => unreachable!("indexed mode returned above"),
                }
                if row_index + 1 == height as usize {
                    self.cancellation_point(location.clone())?;
                }
            }
            if rgba.len() != rgba_capacity {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::Decode,
                    location,
                    "PNG RGBA conversion produced an unexpected byte count",
                ));
            }
            if let Some(gamma_scaled) = gamma_scaled {
                let lut_scratch =
                    self.reserve_temporary(BudgetKind::ParserScratchBytes, 256, location.clone())?;
                let lut = png_gamma_to_srgb_lut(gamma_scaled);
                let output_row_bytes = row_pixels.checked_mul(4).ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        location.clone(),
                        "PNG RGBA row size overflow",
                    )
                })?;
                for row in rgba.chunks_exact_mut(output_row_bytes) {
                    self.cancellation_point(location.clone())?;
                    for pixel in row.chunks_exact_mut(4) {
                        pixel[0] = lut[usize::from(pixel[0])];
                        pixel[1] = lut[usize::from(pixel[1])];
                        pixel[2] = lut[usize::from(pixel[2])];
                    }
                }
                self.cancellation_point(location.clone())?;
                drop(lut_scratch);
            }
            rgba
        };
        drop(_decoder_scratch);
        self.account(BudgetKind::DecodedImageBytes, rgba_bytes, location.clone())?;
        self.account(BudgetKind::DecodedImages, 1, location.clone())?;
        let image = Arc::new(DecodedImage::new_rgba8(
            width,
            height,
            output,
            encoded_hash,
            &self.limits,
            location.clone(),
        )?);
        self.metrics.png_base_decodes = self.metrics.png_base_decodes.saturating_add(1);
        drop(conversion_scratch);
        let payload_len = image.rgba8().len().checked_add(44).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location.clone(),
                "PNG cache payload length overflow",
            )
        })?;
        // Vec-to-Arc slice conversion creates the reference-counted allocation while the Vec is
        // still alive, so reserve for both bounded buffers before building the payload.
        let cache_scratch = u64::try_from(payload_len)
            .ok()
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location.clone(),
                    "PNG cache scratch byte count overflow",
                )
            })?;
        let _cache_scratch = self.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            cache_scratch,
            location.clone(),
        )?;
        let mut cache_payload = Vec::with_capacity(payload_len);
        cache_payload.extend_from_slice(b"DPNG");
        cache_payload.extend_from_slice(&encoded_hash);
        cache_payload.extend_from_slice(&width.to_le_bytes());
        cache_payload.extend_from_slice(&height.to_le_bytes());
        cache_payload.extend_from_slice(image.rgba8());
        self.publish_cache(
            cache_key,
            CacheObjectKind::PngBase,
            Arc::from(cache_payload),
            location.clone(),
        )?;
        self.decoded_png.insert(decode_key, image.clone());
        Ok(image)
    }
}
