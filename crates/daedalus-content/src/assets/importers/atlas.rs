use std::sync::Arc;

use serde::de::{Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::super::{
    AssetImportContext, AssetImportError, AssetImportErrorCode, AtlasRegion, CacheKey,
    CacheNamespace, CacheObjectKind, CacheRead, ImageRole, ImportedAsset, MipPolicy, MipmapFilter,
    SpriteAtlas, TextureFilter,
};
use super::binary::{BinaryError, BinaryReader, BinaryWriter};
use super::{check_schema_version, location, parse_id, parse_settings};
use crate::assets::BudgetKind;
use crate::assets::source::SourceBlob;
use crate::scene::{AssetDescriptor, StableId};

const MAX_ATLAS_REGIONS_HARD: usize = 16_384;
const ATLAS_CACHE_HEADER_BYTES: usize = 4 + 4;
const ATLAS_REGION_RECORD_HEADER_BYTES: usize = 4;
const ATLAS_REGION_FIELDS_BYTES: u64 = 4 * 4 + 2 * 4 + 4;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AtlasSettings {
    schema_version: u32,
}

impl Default for AtlasSettings {
    fn default() -> Self {
        Self { schema_version: 1 }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FilterPolicy {
    NearestNoMipV1,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AtlasDocument {
    schema_version: u32,
    image_asset: String,
    filter_policy: FilterPolicy,
    regions: BoundedRegions,
}

#[derive(Debug)]
struct BoundedRegions(Vec<RegionDocument>);

impl<'de> Deserialize<'de> for BoundedRegions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RegionsVisitor;
        impl<'de> Visitor<'de> for RegionsVisitor {
            type Value = BoundedRegions;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an array of at most 16384 atlas regions")
            }
            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut regions = Vec::new();
                while let Some(region) = sequence.next_element::<RegionDocument>()? {
                    if regions.len() >= MAX_ATLAS_REGIONS_HARD {
                        return Err(A::Error::custom(
                            "atlas region count exceeds hard limit 16384",
                        ));
                    }
                    regions.push(region);
                }
                Ok(BoundedRegions(regions))
            }
        }
        deserializer.deserialize_seq(RegionsVisitor)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegionDocument {
    id: String,
    rect: [u32; 4],
    pivot: [f32; 2],
    pixels_per_unit: f32,
}

pub(super) fn import(
    descriptor: &AssetDescriptor,
    source: Arc<SourceBlob>,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedAsset, AssetImportError> {
    let settings: AtlasSettings = parse_settings(&descriptor.metadata, &source, "metadata")?;
    check_schema_version(settings.schema_version, &source, "metadata.schema_version")?;
    if !source
        .source_id()
        .as_str()
        .to_ascii_lowercase()
        .ends_with(".json")
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(&source, "uri"),
            "sprite-atlas-json-v1 accepts JSON sources only",
        ));
    }
    let scratch_bytes = source.byte_len().checked_mul(2).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(&source, "json"),
            "atlas parser scratch size overflow",
        )
    })?;
    let _scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        scratch_bytes,
        location(&source, "json"),
    )?;
    context.check_cancelled(location(&source, "json"))?;
    context.record_container_parse();
    let document: AtlasDocument = serde_json::from_slice(source.bytes()).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Json,
            location(&source, "atlas"),
            format!("sprite atlas JSON is invalid: {error}"),
        )
    })?;
    if document.schema_version != 1 {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(&source, "schema_version"),
            format!(
                "unsupported sprite atlas schema_version {}; expected 1",
                document.schema_version
            ),
        ));
    }
    if document.filter_policy != FilterPolicy::NearestNoMipV1 {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(&source, "filter_policy"),
            "unsupported atlas filter policy",
        ));
    }
    let image_id = parse_id(&document.image_asset, &source, "image_asset")?;
    context.record_asset_dependency(&image_id)?;
    let image = match context.prepared_asset(&image_id) {
        Some(ImportedAsset::Image(image)) => image.clone(),
        Some(_) => {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location(&source, "image_asset"),
                "atlas image_asset must reference a daedalus.image asset",
            ));
        }
        None => {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location(&source, "image_asset"),
                "atlas image_asset is not available in the prepared project",
            ));
        }
    };
    if image.role() != ImageRole::SpriteAtlasSrgb
        || image.mip_policy() != MipPolicy::None
        || !image.mips().is_empty()
        || image.sampler().min_filter != TextureFilter::Nearest
        || image.sampler().mag_filter != TextureFilter::Nearest
        || image.sampler().mipmap_filter != MipmapFilter::None
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Invalid,
            location(&source, "image_asset"),
            "atlas image must use sprite_atlas_srgb role with nearest filtering and no mipmaps",
        ));
    }
    let dimensions = context.image_dimensions(&image_id).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::Invalid,
            location(&source, "image_asset"),
            "atlas image dimensions are unavailable",
        )
    })?;
    let configured_region_limit = context.limits().max_atlas_regions;
    if document.regions.0.len() > configured_region_limit {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(&source, "regions"),
            format!(
                "atlas has {} regions; configured limit is {configured_region_limit}",
                document.regions.0.len()
            ),
        ));
    }
    let region_count = document.regions.0.len() as u64;
    let mut regions = Vec::with_capacity(document.regions.0.len());
    for (index, region) in document.regions.0.into_iter().enumerate() {
        context.check_cancelled(location(&source, format!("regions[{index}]")))?;
        let id = parse_id(&region.id, &source, &format!("regions[{index}].id"))?;
        regions.push(AtlasRegion::new(
            id,
            region.rect,
            region.pivot,
            region.pixels_per_unit,
        ));
    }
    drop(_scratch);

    let key = atlas_key(&image_id, dimensions, &source);
    if let Some(existing) = find_existing_atlas(context, key) {
        return Ok(ImportedAsset::Atlas(existing));
    }
    context.check_budget(
        BudgetKind::AtlasRegions,
        region_count,
        location(&source, "regions"),
    )?;
    let cooked_bytes = atlas_cooked_bytes(&regions, &image_id, &source)?;
    context.check_budget(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(&source, "regions"),
    )?;
    let cache_key = CacheKey::new(CacheNamespace::Cook, key.digest());
    let cache_max_bytes =
        u64::try_from(atlas_cache_payload_bytes(&regions, &source)?).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(&source, "cache"),
                "atlas cache allocation length is not representable",
            )
        })?;
    let (regions, hit) =
        match context.read_cache(cache_key, CacheObjectKind::Cook, cache_max_bytes)? {
            CacheRead::Hit(lease) => {
                match decode_regions(lease.payload(), &source, context.limits().max_atlas_regions) {
                    Ok(cached) if cached == regions => (cached, true),
                    Ok(_) | Err(_) => {
                        drop(lease);
                        context.invalidate_cache(
                            cache_key,
                            CacheObjectKind::Cook,
                            location(&source, "cache"),
                        )?;
                        (regions, false)
                    }
                }
            }
            CacheRead::Miss => (regions, false),
            CacheRead::Corrupt(_) => {
                context.invalidate_cache(
                    cache_key,
                    CacheObjectKind::Cook,
                    location(&source, "cache"),
                )?;
                (regions, false)
            }
        };
    let atlas = Arc::new(SpriteAtlas::new(
        image_id,
        regions,
        key,
        context.limits(),
        dimensions.0,
        dimensions.1,
        location(&source, "regions"),
    )?);
    context.account(
        BudgetKind::AtlasRegions,
        region_count,
        location(&source, "regions"),
    )?;
    context.account(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(&source, "regions"),
    )?;
    if !hit {
        let payload_bytes = u64::try_from(atlas_cache_payload_bytes(atlas.regions(), &source)?)
            .map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(&source, "cache"),
                    "atlas cache payload length is not representable",
                )
            })?;
        let transient_bytes = payload_bytes.checked_mul(2).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(&source, "cache"),
                "atlas cache transient allocation size overflow",
            )
        })?;
        let _cache_scratch = context.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            transient_bytes,
            location(&source, "cache"),
        )?;
        context.publish_cache(
            cache_key,
            CacheObjectKind::Cook,
            Arc::from(encode_regions(atlas.regions(), &source)?),
            location(&source, "cache"),
        )?;
        context.record_cook(super::super::AssetKind::SpriteAtlas);
    }
    Ok(ImportedAsset::Atlas(atlas))
}

fn atlas_key(
    image_id: &StableId,
    dimensions: (u32, u32),
    source: &SourceBlob,
) -> super::super::LeafCookKey {
    let mut settings = Vec::with_capacity(1 + image_id.as_str().len() + 16);
    settings.push(1);
    settings.extend_from_slice(&(image_id.as_str().len() as u64).to_le_bytes());
    settings.extend_from_slice(image_id.as_str().as_bytes());
    settings.extend_from_slice(&dimensions.0.to_le_bytes());
    settings.extend_from_slice(&dimensions.1.to_le_bytes());
    super::super::LeafCookKey::from_digest(super::super::types::domain_hash(
        b"daedalus.sprite-atlas-json-v1",
        &[&settings, source.bytes()],
    ))
}

fn find_existing_atlas(
    context: &AssetImportContext<'_>,
    key: super::super::LeafCookKey,
) -> Option<Arc<SpriteAtlas>> {
    context
        .catalog_assets()
        .values()
        .find_map(|asset| match asset {
            ImportedAsset::Atlas(atlas) if atlas.key() == key => Some(atlas.clone()),
            _ => None,
        })
}

fn atlas_cooked_bytes(
    regions: &[AtlasRegion],
    image_id: &StableId,
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    u64::try_from(atlas_cache_payload_bytes(regions, source)?)
        .map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "regions"),
                "atlas cooked byte count is not representable",
            )
        })?
        .checked_add(image_id.as_str().len() as u64)
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "image_asset"),
                "atlas cooked byte count overflow",
            )
        })
}

fn encode_regions(
    regions: &[AtlasRegion],
    source: &SourceBlob,
) -> Result<Vec<u8>, AssetImportError> {
    let capacity = atlas_cache_payload_bytes(regions, source)?;
    let mut bytes =
        BinaryWriter::with_capacity(capacity).map_err(|error| binary_cache_error(source, error))?;
    bytes
        .write(b"DAT1")
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .u32(u32::try_from(regions.len()).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "atlas region count is not representable",
            )
        })?)
        .map_err(|error| binary_cache_error(source, error))?;
    for region in regions {
        let id = region.id().as_str().as_bytes();
        bytes
            .u32(u32::try_from(id.len()).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "cache"),
                    "atlas region ID is not representable",
                )
            })?)
            .map_err(|error| binary_cache_error(source, error))?;
        bytes
            .write(id)
            .map_err(|error| binary_cache_error(source, error))?;
        for value in region.rect() {
            bytes
                .u32(value)
                .map_err(|error| binary_cache_error(source, error))?;
        }
        for value in region.pivot() {
            bytes
                .f32_bits(value)
                .map_err(|error| binary_cache_error(source, error))?;
        }
        bytes
            .f32_bits(region.pixels_per_unit())
            .map_err(|error| binary_cache_error(source, error))?;
    }
    Ok(bytes.into_bytes())
}

fn atlas_cache_payload_bytes(
    regions: &[AtlasRegion],
    source: &SourceBlob,
) -> Result<usize, AssetImportError> {
    regions
        .iter()
        .try_fold(ATLAS_CACHE_HEADER_BYTES, |total, region| {
            total
                .checked_add(ATLAS_REGION_RECORD_HEADER_BYTES)?
                .checked_add(region.id().as_str().len())?
                .checked_add(usize::try_from(ATLAS_REGION_FIELDS_BYTES).ok()?)
        })
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "atlas cache allocation size overflow",
            )
        })
}

fn decode_regions(
    payload: &[u8],
    source: &SourceBlob,
    max_regions: usize,
) -> Result<Vec<AtlasRegion>, AssetImportError> {
    let mut reader =
        BinaryReader::new(payload, b"DAT1").map_err(|error| binary_cache_error(source, error))?;
    let count = usize::try_from(
        reader
            .u32()
            .map_err(|error| binary_cache_error(source, error))?,
    )
    .map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "atlas cache count is not representable",
        )
    })?;
    if count > max_regions || count > MAX_ATLAS_REGIONS_HARD {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "atlas cache region count exceeds configured limit",
        ));
    }
    let mut regions = Vec::with_capacity(count);
    for index in 0..count {
        let id_len = usize::try_from(
            reader
                .u32()
                .map_err(|error| binary_cache_error(source, error))?,
        )
        .map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, "cache"),
                "atlas cache ID length is not representable",
            )
        })?;
        if id_len == 0 || id_len > 4096 {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, format!("cache.regions[{index}]")),
                "atlas cache region has invalid ID length",
            ));
        }
        let id_bytes = reader
            .read_exact(id_len)
            .map_err(|error| binary_cache_error(source, error))?;
        let id_text = std::str::from_utf8(id_bytes).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, format!("cache.regions[{index}].id")),
                "atlas cache ID is not UTF-8",
            )
        })?;
        let id = StableId::new(id_text).map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, format!("cache.regions[{index}].id")),
                format!("atlas cache ID is invalid: {error}"),
            )
        })?;
        let mut rect = [0u32; 4];
        for value in &mut rect {
            *value = reader
                .u32()
                .map_err(|error| binary_cache_error(source, error))?;
        }
        let mut pivot = [0.0f32; 2];
        for value in &mut pivot {
            *value = reader
                .f32_bits()
                .map_err(|error| binary_cache_error(source, error))?;
        }
        let pixels_per_unit = reader
            .f32_bits()
            .map_err(|error| binary_cache_error(source, error))?;
        regions.push(AtlasRegion::new(id, rect, pivot, pixels_per_unit));
    }
    reader
        .finish()
        .map_err(|error| binary_cache_error(source, error))?;
    Ok(regions)
}

fn binary_cache_error(source: &SourceBlob, error: BinaryError) -> AssetImportError {
    let (code, description) = match error {
        BinaryError::Allocation => (
            AssetImportErrorCode::LimitExceeded,
            "cache envelope allocation failed",
        ),
        BinaryError::CapacityExceeded => (
            AssetImportErrorCode::Internal,
            "cache encoder exceeded its checked capacity",
        ),
        BinaryError::Truncated => (
            AssetImportErrorCode::Cache,
            "cache envelope is truncated or has an invalid header",
        ),
        BinaryError::LengthOverflow => (
            AssetImportErrorCode::Cache,
            "cache envelope length overflow",
        ),
        BinaryError::TrailingBytes => (
            AssetImportErrorCode::Cache,
            "cache envelope contains trailing bytes",
        ),
    };
    AssetImportError::new(code, location(source, "cache"), description)
}

#[cfg(test)]
mod tests {
    use super::{atlas_cache_payload_bytes, atlas_key, decode_regions, encode_regions};
    use crate::assets::source::{SourceBlob, SourceId};
    use crate::assets::types::AtlasRegion;
    use crate::scene::StableId;

    fn source(bytes: &[u8]) -> SourceBlob {
        SourceBlob::new(SourceId::new("assets/atlas.json").unwrap(), bytes.to_vec())
    }

    #[test]
    fn warm_cache_roundtrip_uses_all_twenty_eight_region_field_bytes() {
        let source = source(b"fixture");
        let region = AtlasRegion::new(
            StableId::new("tile_one").unwrap(),
            [1, 2, 3, 4],
            [0.5, 1.25],
            16.0,
        );
        let encoded = encode_regions(std::slice::from_ref(&region), &source).unwrap();
        let mut expected = b"DAT1".to_vec();
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&8u32.to_le_bytes());
        expected.extend_from_slice(b"tile_one");
        for value in [1u32, 2, 3, 4] {
            expected.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0.5f32, 1.25, 16.0] {
            expected.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        assert_eq!(encoded, expected);
        assert_eq!(encoded.len(), 8 + 4 + 8 + 28);
        assert_eq!(
            atlas_cache_payload_bytes(std::slice::from_ref(&region), &source).unwrap(),
            encoded.len()
        );
        assert_eq!(
            decode_regions(&encoded, &source, 16).unwrap(),
            vec![region.clone()]
        );

        assert!(decode_regions(&encoded[..encoded.len() - 1], &source, 16).is_err());
        let mut with_trailing = encoded;
        with_trailing.push(0);
        assert!(decode_regions(&with_trailing, &source, 16).is_err());
    }

    #[test]
    fn atlas_key_tracks_image_identity_and_dimensions_not_pixels() {
        let source_a = source(b"same atlas json");
        let source_b = source(b"same atlas json");
        let image = StableId::new("sprite_sheet").unwrap();
        assert_eq!(
            atlas_key(&image, (16, 32), &source_a),
            atlas_key(&image, (16, 32), &source_b)
        );
        assert_ne!(
            atlas_key(&image, (16, 32), &source_a),
            atlas_key(&image, (32, 16), &source_a)
        );
        assert_ne!(
            atlas_key(&image, (16, 32), &source_a),
            atlas_key(&StableId::new("other_sheet").unwrap(), (16, 32), &source_a)
        );
    }
}
