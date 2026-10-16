use std::io::Cursor;
use std::sync::Arc;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::super::{
    AssetImportContext, AssetImportError, AssetImportErrorCode, AudioAsset, CacheKey,
    CacheNamespace, CacheObjectKind, CacheRead, ImportedAsset, LeafCookKey,
};
use super::binary::{BinaryError, BinaryReader, BinaryWriter};
use super::{check_schema_version, location, parse_settings};
use crate::assets::source::SourceBlob;
use crate::assets::{BudgetKind, ImportLimits};
use crate::scene::AssetDescriptor;

const AUDIO_CACHE_HEADER_BYTES: u64 = 4 + 4 + 2 + 8 + 8 + 32;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioSettings {
    schema_version: u32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { schema_version: 1 }
    }
}

pub(super) fn import(
    descriptor: &AssetDescriptor,
    source: Arc<SourceBlob>,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedAsset, AssetImportError> {
    let settings: AudioSettings = parse_settings(&descriptor.metadata, &source, "metadata")?;
    check_schema_version(settings.schema_version, &source, "metadata.schema_version")?;
    check_wav_extension(&source)?;
    let memo_key = context.import_key("daedalus.audio.wav-pcm-f32.schema-1", 1, &source, &[]);
    if let Some(asset) = context.memoized_asset(memo_key) {
        return Ok(asset);
    }
    let asset = ImportedAsset::Audio(import_wav(&source, context)?);
    context.memoize_asset(memo_key, asset.clone())?;
    Ok(asset)
}

fn import_wav(
    source: &SourceBlob,
    context: &mut AssetImportContext<'_>,
) -> Result<Arc<AudioAsset>, AssetImportError> {
    check_wav_extension(source)?;
    let parse_scratch = source
        .byte_len()
        .min(context.limits().max_parser_scratch_bytes);
    let _scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        parse_scratch,
        location(source, "wav.header"),
    )?;
    context.check_cancelled(location(source, "wav.header"))?;
    context.record_container_parse();
    let mut reader = hound::WavReader::new(Cursor::new(source.bytes())).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Decode,
            location(source, "wav.header"),
            format!("WAV header is invalid: {error}"),
        )
    })?;
    let spec = reader.spec();
    if !(1..=2).contains(&spec.channels) || !(8_000..=384_000).contains(&spec.sample_rate) {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(source, "wav.fmt"),
            "WAV must contain one or two channels at 8–384 kHz",
        ));
    }
    match spec.sample_format {
        hound::SampleFormat::Int if matches!(spec.bits_per_sample, 8 | 16 | 24 | 32) => {}
        hound::SampleFormat::Float if spec.bits_per_sample == 32 => {}
        hound::SampleFormat::Int => {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location(source, "wav.fmt.bits_per_sample"),
                "integer WAV must use 8, 16, 24, or 32 bits per sample",
            ));
        }
        hound::SampleFormat::Float => {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Unsupported,
                location(source, "wav.fmt.bits_per_sample"),
                "floating WAV must use 32-bit samples",
            ));
        }
    }
    let frames = u64::from(reader.duration());
    if frames == 0 || frames > context.limits().max_audio_frames {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "wav.data"),
            "WAV frame count is zero or exceeds configured audio limit",
        ));
    }
    let sample_count = frames
        .checked_mul(u64::from(spec.channels))
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "wav.data"),
                "WAV sample count overflow",
            )
        })?;
    if sample_count != u64::from(reader.len()) {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Decode,
            location(source, "wav.data"),
            "WAV frame and sample counts are inconsistent",
        ));
    }
    let sample_capacity = usize::try_from(sample_count).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "wav.data"),
            "WAV sample count is not representable",
        )
    })?;
    let sample_bytes = sample_count.checked_mul(4).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "wav.data"),
            "WAV cooked byte count overflow",
        )
    })?;
    let cooked_bytes = sample_bytes
        .checked_add(AUDIO_CACHE_HEADER_BYTES)
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "wav.data"),
                "WAV cooked byte count overflow",
            )
        })?;
    if cooked_bytes > context.limits().max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "wav.data"),
            "WAV cooked samples exceed configured limit",
        ));
    }
    let _sample_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        sample_bytes,
        location(source, "wav.samples"),
    )?;

    let cache_key_digest =
        super::super::types::domain_hash(b"daedalus.wav.samples-cache.v1", &[&source.sha256()]);
    let cache_key = CacheKey::new(CacheNamespace::Cook, cache_key_digest);
    let (samples, key, cache_hit) =
        match context.read_cache(cache_key, CacheObjectKind::Cook, cooked_bytes)? {
            CacheRead::Hit(lease) => match decode_audio_cache(
                lease.payload(),
                spec.sample_rate,
                spec.channels,
                frames,
                sample_capacity,
                context.limits(),
                source,
            ) {
                Ok((samples, key)) => (samples, key, true),
                Err(error) if error.code() == AssetImportErrorCode::Cache => {
                    drop(lease);
                    context.invalidate_cache(
                        cache_key,
                        CacheObjectKind::Cook,
                        location(source, "cache"),
                    )?;
                    let samples = read_samples(
                        &mut reader,
                        spec.sample_format,
                        spec.bits_per_sample,
                        sample_capacity,
                        context,
                        source,
                    )?;
                    let key = audio_key(spec.sample_rate, spec.channels, &samples);
                    (samples, key, false)
                }
                Err(error) => return Err(error),
            },
            CacheRead::Miss => {
                let samples = read_samples(
                    &mut reader,
                    spec.sample_format,
                    spec.bits_per_sample,
                    sample_capacity,
                    context,
                    source,
                )?;
                let key = audio_key(spec.sample_rate, spec.channels, &samples);
                (samples, key, false)
            }
            CacheRead::Corrupt(_) => {
                context.invalidate_cache(
                    cache_key,
                    CacheObjectKind::Cook,
                    location(source, "cache"),
                )?;
                let samples = read_samples(
                    &mut reader,
                    spec.sample_format,
                    spec.bits_per_sample,
                    sample_capacity,
                    context,
                    source,
                )?;
                let key = audio_key(spec.sample_rate, spec.channels, &samples);
                (samples, key, false)
            }
        };

    if let Some(existing) = find_existing_audio(context, key) {
        drop(samples);
        drop(_sample_scratch);
        drop(_scratch);
        if !cache_hit {
            publish_audio_cache(&existing, cache_key, context, source)?;
        }
        return Ok(existing);
    }

    context.check_budget(
        BudgetKind::AudioFrames,
        frames,
        location(source, "wav.data"),
    )?;
    context.check_budget(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, "wav.data"),
    )?;
    context.account(
        BudgetKind::AudioFrames,
        frames,
        location(source, "wav.data"),
    )?;
    context.account(
        BudgetKind::CookedBytes,
        cooked_bytes,
        location(source, "wav.data"),
    )?;
    let audio = Arc::new(AudioAsset::imported(
        spec.sample_rate,
        spec.channels,
        samples,
        key,
        context.limits(),
        location(source, "wav.samples"),
    )?);
    drop(_sample_scratch);
    drop(_scratch);
    if !cache_hit {
        publish_audio_cache(&audio, cache_key, context, source)?;
        context.record_cook(super::super::AssetKind::Audio);
    }
    Ok(audio)
}

fn check_wav_extension(source: &SourceBlob) -> Result<(), AssetImportError> {
    if source
        .source_id()
        .as_str()
        .to_ascii_lowercase()
        .ends_with(".wav")
    {
        Ok(())
    } else {
        Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(source, "uri"),
            "wav-pcm-f32-v1 accepts WAV sources only",
        ))
    }
}

fn read_samples(
    reader: &mut hound::WavReader<Cursor<&[u8]>>,
    format: hound::SampleFormat,
    bits_per_sample: u16,
    sample_capacity: usize,
    context: &AssetImportContext<'_>,
    source: &SourceBlob,
) -> Result<Vec<f32>, AssetImportError> {
    let mut samples = Vec::new();
    samples.try_reserve_exact(sample_capacity).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "wav.samples"),
            "WAV sample allocation failed",
        )
    })?;
    match format {
        hound::SampleFormat::Float => {
            for (index, result) in reader.samples::<f32>().enumerate() {
                if index.is_multiple_of(16_384) {
                    context.check_cancelled(location(source, format!("wav.samples[{index}]")))?;
                }
                let sample = result.map_err(|error| {
                    AssetImportError::new(
                        AssetImportErrorCode::Decode,
                        location(source, format!("wav.samples[{index}]")),
                        format!("WAV float sample is invalid: {error}"),
                    )
                })?;
                if !sample.is_finite() {
                    return Err(AssetImportError::new(
                        AssetImportErrorCode::Invalid,
                        location(source, format!("wav.samples[{index}]")),
                        "WAV contains a non-finite float sample",
                    ));
                }
                samples.push(sample);
            }
        }
        hound::SampleFormat::Int => {
            let denominator = 2.0f64.powi(i32::from(bits_per_sample) - 1);
            for (index, result) in reader.samples::<i32>().enumerate() {
                if index.is_multiple_of(16_384) {
                    context.check_cancelled(location(source, format!("wav.samples[{index}]")))?;
                }
                let sample = result.map_err(|error| {
                    AssetImportError::new(
                        AssetImportErrorCode::Decode,
                        location(source, format!("wav.samples[{index}]")),
                        format!("WAV integer sample is invalid: {error}"),
                    )
                })?;
                samples.push((f64::from(sample) / denominator) as f32);
            }
        }
    }
    if samples.len() != sample_capacity {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Decode,
            location(source, "wav.samples"),
            "WAV sample payload is truncated",
        ));
    }
    Ok(samples)
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
    LeafCookKey::from_digest(hasher.finalize().into())
}

fn encode_audio_cache(
    audio: &AudioAsset,
    limits: &ImportLimits,
    source: &SourceBlob,
) -> Result<Vec<u8>, AssetImportError> {
    let total = audio_cache_payload_bytes(audio.samples().len(), source)?;
    if total > limits.max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache envelope exceeds configured limit",
        ));
    }
    let capacity = usize::try_from(total).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache allocation size is not representable",
        )
    })?;
    let mut bytes =
        BinaryWriter::with_capacity(capacity).map_err(|error| binary_cache_error(source, error))?;
    bytes
        .write(b"DAU1")
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .u32(audio.sample_rate())
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .u16(audio.channels())
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .u64(audio.frames())
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .u64(u64::try_from(audio.samples().len()).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "audio sample count is not representable",
            )
        })?)
        .map_err(|error| binary_cache_error(source, error))?;
    bytes
        .write(&audio.key().digest())
        .map_err(|error| binary_cache_error(source, error))?;
    for sample in audio.samples() {
        bytes
            .f32_bits(*sample)
            .map_err(|error| binary_cache_error(source, error))?;
    }
    Ok(bytes.into_bytes())
}

fn audio_cache_payload_bytes(
    sample_count: usize,
    source: &SourceBlob,
) -> Result<u64, AssetImportError> {
    u64::try_from(sample_count)
        .ok()
        .and_then(|samples| samples.checked_mul(4))
        .and_then(|bytes| bytes.checked_add(AUDIO_CACHE_HEADER_BYTES))
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                location(source, "cache"),
                "audio cache byte count overflow",
            )
        })
}

fn publish_audio_cache(
    audio: &AudioAsset,
    cache_key: CacheKey,
    context: &mut AssetImportContext<'_>,
    source: &SourceBlob,
) -> Result<(), AssetImportError> {
    let payload_bytes = audio_cache_payload_bytes(audio.samples().len(), source)?;
    if payload_bytes > context.limits().max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache envelope exceeds configured limit",
        ));
    }
    let transient_bytes = payload_bytes.checked_mul(2).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache transient allocation size overflow",
        )
    })?;
    let _cache_scratch = context.reserve_temporary(
        BudgetKind::ParserScratchBytes,
        transient_bytes,
        location(source, "cache"),
    )?;
    let payload = encode_audio_cache(audio, context.limits(), source)?;
    context.publish_cache(
        cache_key,
        CacheObjectKind::Cook,
        Arc::from(payload),
        location(source, "cache"),
    )
}

fn decode_audio_cache(
    payload: &[u8],
    sample_rate: u32,
    channels: u16,
    frames: u64,
    sample_capacity: usize,
    limits: &ImportLimits,
    source: &SourceBlob,
) -> Result<(Vec<f32>, LeafCookKey), AssetImportError> {
    let mut reader =
        BinaryReader::new(payload, b"DAU1").map_err(|error| binary_cache_error(source, error))?;
    let cached_rate = reader
        .u32()
        .map_err(|error| binary_cache_error(source, error))?;
    let cached_channels = reader
        .u16()
        .map_err(|error| binary_cache_error(source, error))?;
    let cached_frames = reader
        .u64()
        .map_err(|error| binary_cache_error(source, error))?;
    let cached_samples = reader
        .u64()
        .map_err(|error| binary_cache_error(source, error))?;
    let key: [u8; 32] = reader
        .read_exact(32)
        .map_err(|error| binary_cache_error(source, error))?
        .try_into()
        .map_err(|_| binary_cache_error(source, BinaryError::Truncated))?;
    let expected_bytes = cached_samples
        .checked_mul(4)
        .and_then(|count| count.checked_add(AUDIO_CACHE_HEADER_BYTES))
        .ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, "cache"),
                "audio cache length overflow",
            )
        })?;
    if cached_rate != sample_rate
        || cached_channels != channels
        || cached_frames != frames
        || cached_samples != sample_capacity as u64
        || cached_samples > limits.max_audio_frames.saturating_mul(u64::from(channels))
        || expected_bytes != payload.len() as u64
    {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "audio cache header does not match the verified source WAV",
        ));
    }
    let byte_count = cached_samples.checked_mul(4).ok_or_else(|| {
        AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "audio cache payload size overflow",
        )
    })?;
    if byte_count > limits.max_cooked_bytes {
        return Err(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache samples exceed cooked byte limit",
        ));
    }
    let mut samples = Vec::new();
    samples.try_reserve_exact(sample_capacity).map_err(|_| {
        AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            location(source, "cache"),
            "audio cache sample allocation failed",
        )
    })?;
    for _ in 0..sample_capacity {
        let sample = reader
            .f32_bits()
            .map_err(|error| binary_cache_error(source, error))?;
        if !sample.is_finite() {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                location(source, "cache"),
                "audio cache contains a non-finite sample",
            ));
        }
        samples.push(sample);
    }
    reader
        .finish()
        .map_err(|error| binary_cache_error(source, error))?;
    let computed_key = audio_key(sample_rate, channels, &samples);
    if computed_key.digest() != key {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Cache,
            location(source, "cache"),
            "audio cache canonical key does not match its samples",
        ));
    }
    Ok((samples, computed_key))
}

fn find_existing_audio(
    context: &AssetImportContext<'_>,
    key: LeafCookKey,
) -> Option<Arc<AudioAsset>> {
    context
        .catalog_assets()
        .values()
        .find_map(|asset| match asset {
            ImportedAsset::Audio(audio) if audio.key() == key => Some(audio.clone()),
            _ => None,
        })
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

    use super::{
        AUDIO_CACHE_HEADER_BYTES, audio_cache_payload_bytes, audio_key, decode_audio_cache,
        encode_audio_cache, import, import_wav,
    };
    use crate::assets::source::{SourceBlob, SourceId};
    use crate::assets::types::AudioAsset;
    use crate::assets::{
        AssetImportContext, ByteMapSourceResolver, ImportCancellation, ImportLimits,
        ImportLocation, ImportedAsset, MemoryCookCache,
    };
    use crate::scene::{AssetDescriptor, StableId};

    fn source() -> SourceBlob {
        SourceBlob::new(SourceId::new("audio/test.wav").unwrap(), Vec::<u8>::new())
    }

    fn mono_pcm16_wav(sample: i16) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(46);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&38u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&sample.to_le_bytes());
        bytes
    }

    #[test]
    fn warm_cache_roundtrip_has_the_full_fifty_eight_byte_header() {
        let limits = ImportLimits::default();
        let source = source();
        let samples = vec![0.5f32, -0.25];
        let key = audio_key(8_000, 1, &samples);
        let audio = AudioAsset::imported(
            8_000,
            1,
            samples.clone(),
            key,
            &limits,
            ImportLocation::new("audio/test.wav", "wav.samples"),
        )
        .unwrap();
        let encoded = encode_audio_cache(&audio, &limits, &source).unwrap();
        assert_eq!(AUDIO_CACHE_HEADER_BYTES, 58);
        assert_eq!(encoded.len(), 58 + 2 * 4);
        assert_eq!(
            audio_cache_payload_bytes(samples.len(), &source).unwrap(),
            encoded.len() as u64
        );
        assert_eq!(
            &encoded[..26],
            &[
                b'D', b'A', b'U', b'1', 0x40, 0x1f, 0, 0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0,
                0, 0, 0, 0
            ]
        );
        assert_eq!(&encoded[26..58], &key.digest());
        assert_eq!(&encoded[58..62], &0.5f32.to_bits().to_le_bytes());
        assert_eq!(&encoded[62..66], &(-0.25f32).to_bits().to_le_bytes());
        assert_eq!(
            decode_audio_cache(&encoded, 8_000, 1, 2, 2, &limits, &source).unwrap(),
            (samples, key)
        );
        assert!(
            decode_audio_cache(
                &encoded[..encoded.len() - 1],
                8_000,
                1,
                2,
                2,
                &limits,
                &source
            )
            .is_err()
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_audio_cache(&trailing, 8_000, 1, 2, 2, &limits, &source).is_err());
    }

    #[test]
    fn duplicate_pcm_is_interned_before_aggregate_audio_and_cooked_budget_charges() {
        let bytes = mono_pcm16_wav(8_192);
        let first_source =
            SourceBlob::new(SourceId::new("audio/first.wav").unwrap(), bytes.clone());
        let second_source = SourceBlob::new(SourceId::new("audio/second.wav").unwrap(), bytes);
        let limits = ImportLimits {
            max_audio_frames: 1,
            max_cooked_bytes: 62,
            ..ImportLimits::default()
        };
        let mut resolver = ByteMapSourceResolver::default();
        let mut cache = MemoryCookCache::default();
        let cancellation = ImportCancellation::never();
        let mut context = AssetImportContext::new(
            &mut resolver,
            &mut cache,
            limits,
            &cancellation,
            [first_source.clone(), second_source.clone()],
        )
        .unwrap();

        let first_id = StableId::new("audio_first").unwrap();
        context.begin_asset(first_id.clone(), first_source.source_id().clone());
        let first = import_wav(&first_source, &mut context).unwrap();
        context.register_imported_asset(first_id, ImportedAsset::Audio(first.clone()));

        let second_id = StableId::new("audio_second").unwrap();
        context.begin_asset(second_id, second_source.source_id().clone());
        let second = import_wav(&second_source, &mut context).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn same_wav_source_hash_reuses_invocation_asset_before_container_parse() {
        let bytes = mono_pcm16_wav(8_192);
        let first_source =
            SourceBlob::new(SourceId::new("audio/first.wav").unwrap(), bytes.clone());
        let second_source = SourceBlob::new(SourceId::new("audio/second.wav").unwrap(), bytes);
        let mut resolver = ByteMapSourceResolver::default();
        let mut cache = MemoryCookCache::default();
        let cancellation = ImportCancellation::never();
        let mut context = AssetImportContext::new(
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &cancellation,
            [first_source.clone(), second_source.clone()],
        )
        .unwrap();

        let first_id = StableId::new("audio_first").unwrap();
        context.begin_asset(first_id.clone(), first_source.source_id().clone());
        let first = import(
            &descriptor("audio_first"),
            Arc::new(first_source.clone()),
            &mut context,
        )
        .unwrap();
        context.register_imported_asset(first_id, first.clone());
        assert_eq!(context.metrics().container_parses, 1);

        let second_id = StableId::new("audio_second").unwrap();
        context.begin_asset(second_id, second_source.source_id().clone());
        let second = import(
            &descriptor("audio_second"),
            Arc::new(second_source),
            &mut context,
        )
        .unwrap();
        let (ImportedAsset::Audio(first), ImportedAsset::Audio(second)) = (first, second) else {
            panic!("audio importer returned a different asset kind");
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(context.metrics().container_parses, 1);
    }

    fn descriptor(id: &str) -> AssetDescriptor {
        AssetDescriptor {
            id: StableId::new(id).unwrap(),
            kind: StableId::new("daedalus.audio").unwrap(),
            uri: format!("{id}.wav"),
            content_hash: None,
            metadata: Default::default(),
        }
    }
}
