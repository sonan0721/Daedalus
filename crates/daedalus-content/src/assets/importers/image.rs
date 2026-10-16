use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::super::{
    AssetImportContext, AssetImportError, AssetImportErrorCode, CacheKey, CacheNamespace,
    CacheObjectKind, CacheRead, DecodedImage, ImageAsset, ImageColorPolicy, ImageMip, ImageRole,
    ImageSampler, ImportLimits, ImportedAsset, MipPolicy, MipmapFilter, PngColorPolicy,
    TextureFilter, TextureWrap,
};
use super::{check_schema_version, location, parse_settings};
use crate::assets::BudgetKind;
use crate::assets::source::SourceBlob;
use crate::scene::AssetDescriptor;

use super::binary::{BinaryError, BinaryReader, BinaryWriter};

const MIP_CACHE_HEADER_BYTES: u64 = 4 + 4;
const MIP_CACHE_ENTRY_HEADER_BYTES: u64 = 4 + 4 + 4;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RoleSetting {
    BaseColorSrgb,
    SpriteAtlasSrgb,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MipSetting {
    None,
    LinearLightBoxV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FilterSetting {
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MipmapSetting {
    None,
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WrapSetting {
    ClampToEdge,
    MirroredRepeat,
    Repeat,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SamplerSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_filter: Option<FilterSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mag_filter: Option<FilterSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mipmap_filter: Option<MipmapSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrap_u: Option<WrapSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrap_v: Option<WrapSetting>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageSettings {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<RoleSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mip_policy: Option<MipSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sampler: Option<SamplerSettings>,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            role: None,
            mip_policy: None,
            sampler: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct EffectiveSettings {
    pub(super) role: ImageRole,
    pub(super) mip_policy: MipPolicy,
    pub(super) sampler: ImageSampler,
}

impl ImageSettings {
    fn effective(&self, source: &SourceBlob) -> Result<EffectiveSettings, AssetImportError> {
        check_schema_version(self.schema_version, source, "metadata.schema_version")?;
        let role = match self.role.unwrap_or(RoleSetting::BaseColorSrgb) {
            RoleSetting::BaseColorSrgb => ImageRole::BaseColorSrgb,
            RoleSetting::SpriteAtlasSrgb => ImageRole::SpriteAtlasSrgb,
        };
        let mip_policy = match self.mip_policy.unwrap_or(MipSetting::None) {
            MipSetting::None => MipPolicy::None,
            MipSetting::LinearLightBoxV1 => MipPolicy::LinearLightBoxV1,
        };
        let authored_sampler = self.sampler.as_ref();
        let sampler = ImageSampler {
            min_filter: match authored_sampler
                .and_then(|sampler| sampler.min_filter)
                .unwrap_or(FilterSetting::Linear)
            {
                FilterSetting::Nearest => TextureFilter::Nearest,
                FilterSetting::Linear => TextureFilter::Linear,
            },
            mag_filter: match authored_sampler
                .and_then(|sampler| sampler.mag_filter)
                .unwrap_or(FilterSetting::Linear)
            {
                FilterSetting::Nearest => TextureFilter::Nearest,
                FilterSetting::Linear => TextureFilter::Linear,
            },
            mipmap_filter: match authored_sampler
                .and_then(|sampler| sampler.mipmap_filter)
                .unwrap_or(MipmapSetting::None)
            {
                MipmapSetting::None => MipmapFilter::None,
                MipmapSetting::Nearest => MipmapFilter::Nearest,
                MipmapSetting::Linear => MipmapFilter::Linear,
            },
            wrap_u: match authored_sampler
                .and_then(|sampler| sampler.wrap_u)
                .unwrap_or(WrapSetting::Repeat)
            {
                WrapSetting::ClampToEdge => TextureWrap::ClampToEdge,
                WrapSetting::MirroredRepeat => TextureWrap::MirroredRepeat,
                WrapSetting::Repeat => TextureWrap::Repeat,
            },
            wrap_v: match authored_sampler
                .and_then(|sampler| sampler.wrap_v)
                .unwrap_or(WrapSetting::Repeat)
            {
                WrapSetting::ClampToEdge => TextureWrap::ClampToEdge,
                WrapSetting::MirroredRepeat => TextureWrap::MirroredRepeat,
                WrapSetting::Repeat => TextureWrap::Repeat,
            },
        };

        if (mip_policy == MipPolicy::None) != (sampler.mipmap_filter == MipmapFilter::None) {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location(source, "metadata.mip_policy"),
                "mip_policy must be none exactly when sampler.mipmap_filter is none",
            ));
        }
        if role == ImageRole::SpriteAtlasSrgb
            && (authored_sampler.is_none()
                || authored_sampler.and_then(|value| value.min_filter)
                    != Some(FilterSetting::Nearest)
                || authored_sampler.and_then(|value| value.mag_filter)
                    != Some(FilterSetting::Nearest)
                || authored_sampler.and_then(|value| value.mipmap_filter)
                    != Some(MipmapSetting::None)
                || self.mip_policy != Some(MipSetting::None))
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Invalid,
                location(source, "metadata"),
                "sprite_atlas_srgb requires explicitly authored nearest min/mag filters and none mip_policy/mipmap_filter",
            ));
        }
        Ok(EffectiveSettings {
            role,
            mip_policy,
            sampler,
        })
    }
}

pub(super) fn import(
    descriptor: &AssetDescriptor,
    source: Arc<SourceBlob>,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedAsset, AssetImportError> {
    let settings: ImageSettings = parse_settings(&descriptor.metadata, &source, "metadata")?;
    let effective = settings.effective(&source)?;
    import_png(&source, effective, context).map(ImportedAsset::Image)
}

pub(super) fn import_png(
    source: &SourceBlob,
    settings: EffectiveSettings,
    context: &mut AssetImportContext<'_>,
) -> Result<Arc<ImageAsset>, AssetImportError> {
    if !source
        .source_id()
        .as_str()
        .to_ascii_lowercase()
        .ends_with(".png")
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(source, "uri"),
            "png-rgba8-v1 accepts PNG sources only",
        ));
    }
    cook_png(source, settings, context)
}

/// Decodes and cooks already-identified PNG bytes. Container importers use this path for
/// embedded or buffer-view PNGs, while the direct image importer additionally checks the URI.
pub(super) fn cook_png(
    source: &SourceBlob,
    settings: EffectiveSettings,
    context: &mut AssetImportContext<'_>,
) -> Result<Arc<ImageAsset>, AssetImportError> {
    cook_png_at(
        source,
        settings,
        context,
        "png",
        PngColorPolicy::SourceProfileToSrgb,
    )
}

pub(super) fn cook_png_at(
    source: &SourceBlob,
    settings: EffectiveSettings,
    context: &mut AssetImportContext<'_>,
    path: &str,
    color_policy: PngColorPolicy,
) -> Result<Arc<ImageAsset>, AssetImportError> {
    context.check_cancelled(location(source, path))?;
    context.record_container_parse();
    let decoded = context.decode_png_base_with_policy(
        source.bytes(),
        location(source, path),
        color_policy,
    )?;
    let key_settings = image_key_settings(settings, decoded.width(), decoded.height());
    let key = context.leaf_key(
        "daedalus.image.png-rgba8-v1",
        1,
        &key_settings,
        decoded.rgba8(),
    );
    if let Some(existing) = find_existing_image(context, key) {
        return Ok(existing);
    }

    let mip_bytes = mip_chain_bytes(
        decoded.width(),
        decoded.height(),
        settings.mip_policy,
        source,
    )?;
    let cooked_bytes = (decoded.rgba8().len() as u64)
        .checked_add(mip_bytes)
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "mips"),
                "image cooked byte count overflow",
            )
        })?;
    context.check_budget(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, "mips"),
    )?;

    let cache_key = CacheKey::new(CacheNamespace::Cook, key.digest());
    let cache_max_bytes = mip_cache_expected_payload_bytes(
        decoded.width(),
        decoded.height(),
        settings.mip_policy,
        source,
    )?;
    let mut cache_hit = false;
    let mips = match context.read_cache(cache_key, CacheObjectKind::Cook, cache_max_bytes)? {
        CacheRead::Hit(lease) => match decode_mip_cache(
            lease.payload(),
            decoded.width(),
            decoded.height(),
            settings.mip_policy,
            context.limits(),
            source,
        ) {
            Ok(mips) => {
                cache_hit = true;
                mips
            }
            Err(error) if error.code() == AssetImportErrorCode::Cache => {
                drop(lease);
                context.invalidate_cache(
                    cache_key,
                    CacheObjectKind::Cook,
                    location(source, "cache"),
                )?;
                generate_mips(&decoded, settings.mip_policy, context, source)?
            }
            Err(error) => return Err(error),
        },
        CacheRead::Miss => generate_mips(&decoded, settings.mip_policy, context, source)?,
        CacheRead::Corrupt(_) => {
            context.invalidate_cache(
                cache_key,
                CacheObjectKind::Cook,
                location(source, "cache"),
            )?;
            generate_mips(&decoded, settings.mip_policy, context, source)?
        }
    };
    let image = Arc::new(ImageAsset::new(
        decoded,
        settings.role,
        ImageColorPolicy::SrgbOrUnprofiled,
        settings.sampler,
        settings.mip_policy,
        mips,
        key,
    ));
    context.account(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, "mips"),
    )?;
    if !cache_hit {
        let payload_bytes = mip_cache_payload_bytes(image.mips(), source)?;
        let scratch_bytes = payload_bytes.checked_mul(2).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "mip cache transient allocation size overflow",
            )
        })?;
        let _cache_scratch = context.reserve_temporary(
            BudgetKind::ParserScratchBytes,
            scratch_bytes,
            location(source, "cache"),
        )?;
        let payload = encode_mip_cache(image.mips(), context.limits(), source)?;
        context.publish_cache(
            cache_key,
            CacheObjectKind::Cook,
            Arc::from(payload),
            location(source, "cache"),
        )?;
        context.record_cook(super::super::AssetKind::Image);
    }
    Ok(image)
}

fn find_existing_image(
    context: &AssetImportContext<'_>,
    key: super::super::LeafCookKey,
) -> Option<Arc<ImageAsset>> {
    context
        .catalog_assets()
        .values()
        .find_map(|asset| match asset {
            ImportedAsset::Image(image) if image.key() == key => Some(image.clone()),
            ImportedAsset::Model(model) => model
                .owned_images()
                .iter()
                .find_map(|owned| (owned.asset().key() == key).then(|| owned.asset().clone())),
            _ => None,
        })
}

fn image_key_settings(settings: EffectiveSettings, width: u32, height: u32) -> [u8; 16] {
    let role = match settings.role {
        ImageRole::BaseColorSrgb => 0,
        ImageRole::SpriteAtlasSrgb => 1,
    };
    let mip = match settings.mip_policy {
        MipPolicy::None => 0,
        MipPolicy::LinearLightBoxV1 => 1,
    };
    let min = match settings.sampler.min_filter {
        TextureFilter::Nearest => 0,
        TextureFilter::Linear => 1,
    };
    let mag = match settings.sampler.mag_filter {
        TextureFilter::Nearest => 0,
        TextureFilter::Linear => 1,
    };
    let mipmap = match settings.sampler.mipmap_filter {
        MipmapFilter::None => 0,
        MipmapFilter::Nearest => 1,
        MipmapFilter::Linear => 2,
    };
    let wrap_u = match settings.sampler.wrap_u {
        TextureWrap::ClampToEdge => 0,
        TextureWrap::MirroredRepeat => 1,
        TextureWrap::Repeat => 2,
    };
    let wrap_v = match settings.sampler.wrap_v {
        TextureWrap::ClampToEdge => 0,
        TextureWrap::MirroredRepeat => 1,
        TextureWrap::Repeat => 2,
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&[1, role, mip, min, mag, mipmap, wrap_u, wrap_v]);
    bytes[8..12].copy_from_slice(&width.to_le_bytes());
    bytes[12..16].copy_from_slice(&height.to_le_bytes());
    bytes
}

fn mip_chain_bytes(
    width: u32,
    height: u32,
    policy: MipPolicy,
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    if policy == MipPolicy::None {
        return Ok(0);
    }
    let mut width = width;
    let mut height = height;
    let mut total = 0u64;
    while width > 1 || height > 1 {
        width = (width / 2).max(1);
        height = (height / 2).max(1);
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "mips"),
                    "mip dimensions overflow",
                )
            })?;
        total = total.checked_add(bytes).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "mips"),
                "mip byte count overflow",
            )
        })?;
    }
    Ok(total)
}

fn mip_cache_expected_payload_bytes(
    base_width: u32,
    base_height: u32,
    policy: MipPolicy,
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    let mut total = MIP_CACHE_HEADER_BYTES;
    let (mut width, mut height) = (base_width, base_height);
    if policy != MipPolicy::None {
        while width > 1 || height > 1 {
            width = (width / 2).max(1);
            height = (height / 2).max(1);
            let level_bytes = u64::from(width)
                .checked_mul(u64::from(height))
                .and_then(|pixels| pixels.checked_mul(4))
                .and_then(|pixels| pixels.checked_add(MIP_CACHE_ENTRY_HEADER_BYTES))
                .ok_or_else(|| {
                    AssetImportError::new(
                        AssetImportErrorCode::LimitExceeded,
                        location(source, "cache"),
                        "mip cache payload size overflow",
                    )
                })?;
            total = total.checked_add(level_bytes).ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "cache"),
                    "mip cache payload size overflow",
                )
            })?;
        }
    }
    Ok(total)
}

fn generate_mips(
    decoded: &DecodedImage,
    policy: MipPolicy,
    context: &mut AssetImportContext<'_>,
    source: &SourceBlob,
) -> Result<Vec<ImageMip>, AssetImportError> {
    if policy == MipPolicy::None {
        return Ok(Vec::new());
    }
    for (index, pixel) in decoded.rgba8().chunks_exact(4).enumerate() {
        if index.is_multiple_of(16_384) {
            context.check_cancelled(location(source, format!("png.alpha.pixel[{index}]")))?;
        }
        if pixel[3] != 255 {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location(source, "png.alpha"),
                "linear_light_box_v1 mip generation currently requires fully opaque color pixels",
            ));
        }
    }
    let mut previous_width = decoded.width();
    let mut previous_height = decoded.height();
    let scratch_bytes =
        mip_intermediate_scratch(decoded.width(), decoded.height(), policy, source)?;
    let _scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        scratch_bytes,
        location(source, "mips"),
    )?;
    let mut mips: Vec<ImageMip> = Vec::new();
    while previous_width > 1 || previous_height > 1 {
        let width = (previous_width / 2).max(1);
        let height = (previous_height / 2).max(1);
        let previous_pixels = mips.last().map_or(decoded.rgba8(), |mip| mip.rgba8());
        let byte_count =
            usize::try_from(u64::from(width) * u64::from(height) * 4).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "mips"),
                    "mip allocation length is not representable",
                )
            })?;
        let mut output = Vec::new();
        output.try_reserve_exact(byte_count).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "mips"),
                "mip output allocation failed",
            )
        })?;
        for y in 0..height {
            context.check_cancelled(location(source, format!("mips.row[{y}]")))?;
            for x in 0..width {
                let start_x = f64::from(x) * f64::from(previous_width) / f64::from(width);
                let end_x = f64::from(x + 1) * f64::from(previous_width) / f64::from(width);
                let start_y = f64::from(y) * f64::from(previous_height) / f64::from(height);
                let end_y = f64::from(y + 1) * f64::from(previous_height) / f64::from(height);
                let first_x = start_x.floor() as u32;
                let last_x = end_x.ceil() as u32;
                let first_y = start_y.floor() as u32;
                let last_y = end_y.ceil() as u32;
                let mut linear = [0.0f64; 3];
                let mut total_weight = 0.0f64;
                for source_y in first_y..last_y {
                    let source_top = f64::from(source_y);
                    let weight_y = (end_y.min(source_top + 1.0) - start_y.max(source_top)).max(0.0);
                    for source_x in first_x..last_x {
                        let source_left = f64::from(source_x);
                        let weight_x =
                            (end_x.min(source_left + 1.0) - start_x.max(source_left)).max(0.0);
                        let weight = weight_x * weight_y;
                        let offset = ((source_y * previous_width + source_x) * 4) as usize;
                        for channel in 0..3 {
                            linear[channel] +=
                                srgb_to_linear(previous_pixels[offset + channel]) * weight;
                        }
                        total_weight += weight;
                    }
                }
                for component in linear {
                    output.push(linear_to_srgb((component / total_weight) as f32));
                }
                output.push(255);
            }
        }
        previous_width = width;
        previous_height = height;
        mips.push(ImageMip::new(width, height, output));
    }
    Ok(mips)
}

fn mip_intermediate_scratch(
    width: u32,
    height: u32,
    policy: MipPolicy,
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    if policy == MipPolicy::None {
        return Ok(0);
    }
    let mut max_scratch = 0u64;
    let mut width = width;
    let mut height = height;
    while width > 1 || height > 1 {
        width = (width / 2).max(1);
        height = (height / 2).max(1);
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "mips"),
                    "mip intermediate size overflow",
                )
            })?;
        let conversion_peak = bytes.checked_mul(2).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "mips"),
                "mip scratch size overflow",
            )
        })?;
        max_scratch = max_scratch.max(conversion_peak);
    }
    Ok(max_scratch)
}

fn srgb_to_linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> u8 {
    let value = f64::from(value);
    let encoded = if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

fn encode_mip_cache(
    mips: &[ImageMip],
    limits: &ImportLimits,
    source: &SourceBlob,
) -> Result<Vec<u8>, AssetImportError> {
    let payload_bytes = mip_cache_payload_bytes(mips, source)?;
    if payload_bytes > limits.max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "mip cache envelope exceeds cooked resource limit",
        ));
    }
    let capacity = usize::try_from(payload_bytes).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "mip cache allocation length is not representable",
        )
    })?;
    let mut payload =
        BinaryWriter::with_capacity(capacity).map_err(|error| binary_cache_error(source, error))?;
    payload
        .write(b"DIM1")
        .map_err(|error| binary_cache_error(source, error))?;
    payload
        .u32(u32::try_from(mips.len()).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "mip count is not representable",
            )
        })?)
        .map_err(|error| binary_cache_error(source, error))?;
    for mip in mips {
        payload
            .u32(mip.width())
            .map_err(|error| binary_cache_error(source, error))?;
        payload
            .u32(mip.height())
            .map_err(|error| binary_cache_error(source, error))?;
        payload
            .u32(u32::try_from(mip.rgba8().len()).map_err(|_| {
                AssetImportError::new(
                    AssetImportErrorCode::LimitExceeded,
                    location(source, "cache"),
                    "mip byte length is not representable",
                )
            })?)
            .map_err(|error| binary_cache_error(source, error))?;
        payload
            .write(mip.rgba8())
            .map_err(|error| binary_cache_error(source, error))?;
    }
    Ok(payload.into_bytes())
}

fn mip_cache_payload_bytes(
    mips: &[ImageMip],
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    mips.iter()
        .try_fold(MIP_CACHE_HEADER_BYTES, |total, mip| {
            total
                .checked_add(MIP_CACHE_ENTRY_HEADER_BYTES)
                .and_then(|total| {
                    u64::try_from(mip.rgba8().len())
                        .ok()
                        .and_then(|bytes| total.checked_add(bytes))
                })
        })
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "mip cache envelope overflow",
            )
        })
}

fn decode_mip_cache(
    payload: &[u8],
    base_width: u32,
    base_height: u32,
    policy: MipPolicy,
    limits: &ImportLimits,
    source: &SourceBlob,
) -> Result<Vec<ImageMip>, AssetImportError> {
    let mut reader =
        BinaryReader::new(payload, b"DIM1").map_err(|error| binary_cache_error(source, error))?;
    let count = usize::try_from(
        reader
            .u32()
            .map_err(|error| binary_cache_error(source, error))?,
    )
    .map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "mip cache count is not representable",
        )
    })?;
    let expected_count = if policy == MipPolicy::None {
        0
    } else {
        let mut width = base_width;
        let mut height = base_height;
        let mut count = 0usize;
        while width > 1 || height > 1 {
            width = (width / 2).max(1);
            height = (height / 2).max(1);
            count += 1;
        }
        count
    };
    if count != expected_count || count > 64 {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "mip cache chain count is invalid",
        ));
    }
    let total = mip_chain_bytes(base_width, base_height, policy, source)?;
    if total > limits.max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "cached mip chain exceeds configured cooked limit",
        ));
    }
    let mut expected_width = base_width;
    let mut expected_height = base_height;
    let mut mips = Vec::with_capacity(count);
    for _ in 0..count {
        expected_width = (expected_width / 2).max(1);
        expected_height = (expected_height / 2).max(1);
        let width = reader
            .u32()
            .map_err(|error| binary_cache_error(source, error))?;
        let height = reader
            .u32()
            .map_err(|error| binary_cache_error(source, error))?;
        let byte_len = usize::try_from(
            reader
                .u32()
                .map_err(|error| binary_cache_error(source, error))?,
        )
        .map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, "cache"),
                "cached mip byte length is not representable",
            )
        })?;
        let expected_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| {
                AssetImportError::new(
                    AssetImportErrorCode::Cache,
                    location(source, "cache"),
                    "cached mip byte count overflow",
                )
            })?;
        if width != expected_width || height != expected_height || byte_len != expected_len {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, "cache"),
                "cached mip dimensions or byte length is invalid",
            ));
        }
        let cached_pixels = reader
            .read_exact(byte_len)
            .map_err(|error| binary_cache_error(source, error))?;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(byte_len).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "cached mip allocation failed",
            )
        })?;
        pixels.extend_from_slice(cached_pixels);
        mips.push(ImageMip::new(width, height, pixels));
    }
    reader
        .finish()
        .map_err(|error| binary_cache_error(source, error))?;
    Ok(mips)
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
    use std::sync::Arc;

    use super::{MipPolicy, decode_mip_cache, encode_mip_cache, mip_cache_expected_payload_bytes};
    use crate::assets::source::{SourceBlob, SourceId};
    use crate::assets::types::{DecodedImage, ImageMip, ImportLimits};
    use crate::assets::{
        AssetImportContext, ByteMapSourceResolver, ImportCancellation, ImportLocation,
        MemoryCookCache,
    };

    fn source() -> SourceBlob {
        SourceBlob::new(SourceId::new("images/test.png").unwrap(), Vec::<u8>::new())
    }

    #[test]
    fn warm_mip_cache_roundtrip_matches_explicit_wire_bytes_and_rejects_bad_lengths() {
        let source = source();
        let limits = ImportLimits::default();
        let mip = ImageMip::new(1, 1, vec![10, 20, 30, 255]);
        let encoded = encode_mip_cache(std::slice::from_ref(&mip), &limits, &source).unwrap();
        let mut expected = b"DIM1".to_vec();
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&4u32.to_le_bytes());
        expected.extend_from_slice(&[10, 20, 30, 255]);
        assert_eq!(encoded, expected);
        assert_eq!(
            mip_cache_expected_payload_bytes(2, 2, MipPolicy::LinearLightBoxV1, &source).unwrap(),
            encoded.len() as u64
        );
        assert_eq!(
            decode_mip_cache(
                &encoded,
                2,
                2,
                MipPolicy::LinearLightBoxV1,
                &limits,
                &source
            )
            .unwrap(),
            vec![mip]
        );
        assert!(
            decode_mip_cache(
                &encoded[..encoded.len() - 1],
                2,
                2,
                MipPolicy::LinearLightBoxV1,
                &limits,
                &source
            )
            .is_err()
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert!(
            decode_mip_cache(
                &trailing,
                2,
                2,
                MipPolicy::LinearLightBoxV1,
                &limits,
                &source
            )
            .is_err()
        );
    }

    fn generate_test_mips(width: u32, height: u32, pixel_values: &[u8]) -> Vec<ImageMip> {
        assert_eq!(pixel_values.len(), width as usize * height as usize);
        let pixels = pixel_values
            .iter()
            .flat_map(|value| [*value, *value, *value, 255])
            .collect::<Vec<_>>();
        let source = SourceBlob::new(
            SourceId::new("images/mip-test.png").unwrap(),
            Vec::<u8>::new(),
        );
        let limits = ImportLimits::default();
        let decoded = Arc::new(
            DecodedImage::new_rgba8(
                width,
                height,
                pixels,
                [0; 32],
                &limits,
                ImportLocation::new("images/mip-test.png", "fixture"),
            )
            .unwrap(),
        );
        let mut resolver = ByteMapSourceResolver::default();
        let mut cache = MemoryCookCache::default();
        let cancellation = ImportCancellation::never();
        let mut context = AssetImportContext::new(
            &mut resolver,
            &mut cache,
            limits,
            &cancellation,
            [source.clone()],
        )
        .unwrap();
        super::generate_mips(&decoded, MipPolicy::LinearLightBoxV1, &mut context, &source).unwrap()
    }

    #[test]
    fn odd_mip_dimensions_include_all_edge_texels_with_area_weighting() {
        let three_by_one = generate_test_mips(3, 1, &[0, 0, 255]);
        assert_eq!((three_by_one[0].width(), three_by_one[0].height()), (1, 1));
        assert_eq!(&three_by_one[0].rgba8()[..3], &[156, 156, 156]);

        let one_by_three = generate_test_mips(1, 3, &[0, 0, 255]);
        assert_eq!((one_by_three[0].width(), one_by_three[0].height()), (1, 1));
        assert_eq!(&one_by_three[0].rgba8()[..3], &[156, 156, 156]);

        let five_by_three =
            generate_test_mips(5, 3, &[0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255]);
        assert_eq!(
            (five_by_three[0].width(), five_by_three[0].height()),
            (2, 1)
        );
        assert_eq!(&five_by_three[0].rgba8()[..4], &[0, 0, 0, 255]);
        assert_eq!(&five_by_three[0].rgba8()[4..], &[170, 170, 170, 255]);
        assert_eq!(
            (five_by_three[1].width(), five_by_three[1].height()),
            (1, 1)
        );
        assert_eq!(&five_by_three[1].rgba8()[..3], &[124, 124, 124]);
    }
}
