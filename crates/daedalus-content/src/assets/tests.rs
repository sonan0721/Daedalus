use std::sync::Arc;

use sha2::Digest;

use super::source::ByteMapSourceResolver;
use super::types::domain_hash;
use super::{
    AssetImportContext, CacheKey, CacheNamespace, CacheObjectKind, CacheRead, CpuCookCache,
    ImportCancellation, ImportLimits, ImportLocation, MemoryCookCache, PngColorPolicy, SourceBlob,
    SourceId,
};

fn gray_png(value: u8) -> Vec<u8> {
    gray_png_sized(1, 1, value)
}

fn gray_png_sized(width: u32, height: u32, value: u8) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("PNG header should encode");
        let pixels = vec![value; width as usize * height as usize];
        writer
            .write_image_data(&pixels)
            .expect("PNG pixels should encode");
    }
    bytes
}

fn rgba_png(pixels: &[u8], gamma: Option<u32>) -> Vec<u8> {
    assert_eq!(pixels.len() % 4, 0);
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, (pixels.len() / 4) as u32, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if let Some(gamma) = gamma {
            encoder.set_source_gamma(png::ScaledFloat::from_scaled(gamma));
        }
        let mut writer = encoder.write_header().expect("PNG header should encode");
        writer
            .write_image_data(pixels)
            .expect("PNG pixels should encode");
    }
    bytes
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let crc = png_crc32(kind.iter().chain(data.iter()).copied());
    chunk.extend_from_slice(&crc.to_be_bytes());
    chunk
}

fn png_crc32(bytes: impl Iterator<Item = u8>) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
        }
    }
    !crc
}

fn png_with_chunks_after_ihdr(png: &[u8], chunks: &[u8]) -> Vec<u8> {
    let ihdr_len = u32::from_be_bytes(png[8..12].try_into().unwrap()) as usize;
    let ihdr_end = 8 + 12 + ihdr_len;
    let mut result = Vec::with_capacity(png.len() + chunks.len());
    result.extend_from_slice(&png[..ihdr_end]);
    result.extend_from_slice(chunks);
    result.extend_from_slice(&png[ihdr_end..]);
    result
}

fn context<'a>(
    resolver: &'a mut ByteMapSourceResolver,
    cache: &'a mut dyn CpuCookCache,
    cancellation: &'a ImportCancellation,
) -> AssetImportContext<'a> {
    context_with_limits(resolver, cache, cancellation, ImportLimits::default())
}

fn context_with_limits<'a>(
    resolver: &'a mut ByteMapSourceResolver,
    cache: &'a mut dyn CpuCookCache,
    cancellation: &'a ImportCancellation,
    limits: ImportLimits,
) -> AssetImportContext<'a> {
    let source = SourceBlob::new(
        SourceId::new("daedalus.project.json").unwrap(),
        Arc::<[u8]>::from(&b"{}"[..]),
    );
    AssetImportContext::new(resolver, cache, limits, cancellation, [source]).unwrap()
}

#[test]
fn parser_scratch_peak_tracks_accepted_overlaps_and_released_reservations() {
    let limits = ImportLimits {
        max_source_bytes: 32,
        max_total_source_bytes: 32,
        max_parser_scratch_bytes: 32,
        ..ImportLimits::default()
    };
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let import = context_with_limits(&mut resolver, &mut cache, &cancellation, limits);

    let first = import
        .reserve_temporary(
            super::BudgetKind::ParserScratchBytes,
            10,
            ImportLocation::new("test", "first"),
        )
        .unwrap();
    assert_eq!(
        import.memory_summary().unwrap().parser_scratch_peak_bytes(),
        10
    );

    let rejected = import.reserve_temporary(
        super::BudgetKind::ParserScratchBytes,
        23,
        ImportLocation::new("test", "over-limit"),
    );
    assert_eq!(rejected.unwrap_err().code().as_str(), "limit_exceeded");
    assert_eq!(
        import.memory_summary().unwrap().parser_scratch_peak_bytes(),
        10,
        "a reservation rejected before admission must not advance the peak"
    );

    let second = import
        .reserve_temporary(
            super::BudgetKind::ParserScratchBytes,
            22,
            ImportLocation::new("test", "second"),
        )
        .unwrap();
    assert_eq!(
        import.memory_summary().unwrap().parser_scratch_peak_bytes(),
        32
    );
    drop(first);

    let replacement = import
        .reserve_temporary(
            super::BudgetKind::ParserScratchBytes,
            10,
            ImportLocation::new("test", "replacement"),
        )
        .unwrap();
    assert_eq!(
        import.memory_summary().unwrap().parser_scratch_peak_bytes(),
        32
    );
    drop(second);
    drop(replacement);

    let full_limit = import
        .reserve_temporary(
            super::BudgetKind::ParserScratchBytes,
            32,
            ImportLocation::new("test", "released-all"),
        )
        .unwrap();
    assert_eq!(
        import.memory_summary().unwrap().parser_scratch_peak_bytes(),
        32
    );
    drop(full_limit);
}

#[test]
fn png_base_decode_is_shared_in_call_and_reused_from_persistent_cpu_cache() {
    let png = gray_png(37);
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();

    let first_image = {
        let mut import = context(&mut resolver, &mut cache, &cancellation);
        let first = import
            .decode_png_base(&png, ImportLocation::new("image.png", "png"))
            .unwrap();
        let second = import
            .decode_png_base(&png, ImportLocation::new("image.png", "png"))
            .unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "one source payload should share one decoded base image within a preparation"
        );
        assert_eq!(first.rgba8(), &[37, 37, 37, 255]);
        assert_eq!(import.metrics().png_base_decodes, 1);
        assert_eq!(import.metrics().cache_misses, 1);
        assert_eq!(import.metrics().cache_writes, 1);
        first
    };

    let mut second_resolver = ByteMapSourceResolver::default();
    let second = {
        let mut import = context(&mut second_resolver, &mut cache, &cancellation);
        let second = import
            .decode_png_base(&png, ImportLocation::new("image.png", "png"))
            .unwrap();
        assert_eq!(import.metrics().cache_hits, 1);
        assert_eq!(
            import.metrics().png_base_decodes,
            0,
            "a valid persistent payload should bypass full image decoding"
        );
        second
    };
    assert_eq!(first_image, second);
}

#[test]
fn standalone_gamma_is_normalized_and_cache_roundtrip_preserves_pixels() {
    let png = rgba_png(&[128, 64, 32, 73], Some(100_000));
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let mut resolver = ByteMapSourceResolver::default();
    let first = {
        let mut import = context(&mut resolver, &mut cache, &cancellation);
        let image = import
            .decode_png_base(&png, ImportLocation::new("gamma.png", "image"))
            .unwrap();
        assert_eq!(image.rgba8(), &[188, 137, 99, 73]);
        assert_eq!(import.metrics().png_base_decodes, 1);
        image
    };

    let mut second_resolver = ByteMapSourceResolver::default();
    let second = {
        let mut import = context(&mut second_resolver, &mut cache, &cancellation);
        let image = import
            .decode_png_base(&png, ImportLocation::new("gamma.png", "image"))
            .unwrap();
        assert_eq!(import.metrics().cache_hits, 1);
        assert_eq!(import.metrics().png_base_decodes, 0);
        image
    };
    assert_eq!(first.rgba8(), second.rgba8());
}

#[test]
fn srgb_chunk_overrides_conflicting_gamma_and_alpha_is_preserved() {
    let mut color_chunks = png_chunk(b"gAMA", &100_000u32.to_be_bytes());
    color_chunks.extend_from_slice(&png_chunk(b"sRGB", &[0]));
    let png = png_with_chunks_after_ihdr(&gray_png(128), &color_chunks);
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let mut import = context(&mut resolver, &mut cache, &cancellation);
    let image = import
        .decode_png_base(&png, ImportLocation::new("tagged.png", "image"))
        .unwrap();
    assert_eq!(image.rgba8(), &[128, 128, 128, 255]);

    let alpha_png = rgba_png(&[128, 64, 32, 73], Some(100_000));
    let alpha = import
        .decode_png_base(&alpha_png, ImportLocation::new("alpha.png", "image"))
        .unwrap();
    assert_eq!(alpha.rgba8(), &[188, 137, 99, 73]);
}

#[test]
fn gltf_policy_ignores_png_profiles_and_noop_policies_share_base_identity() {
    let gamma_png = rgba_png(&[128, 64, 32, 255], Some(100_000));
    let plain_png = gray_png(128);
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let mut import = context(&mut resolver, &mut cache, &cancellation);

    let normalized = import
        .decode_png_base(&gamma_png, ImportLocation::new("gamma.png", "standalone"))
        .unwrap();
    let material_encoded = import
        .decode_png_base_with_policy(
            &gamma_png,
            ImportLocation::new("gamma.png", "gltf"),
            PngColorPolicy::GltfMaterialEncoded,
        )
        .unwrap();
    assert_eq!(normalized.rgba8(), &[188, 137, 99, 255]);
    assert_eq!(material_encoded.rgba8(), &[128, 64, 32, 255]);
    assert!(!Arc::ptr_eq(&normalized, &material_encoded));

    let plain_standalone = import
        .decode_png_base(&plain_png, ImportLocation::new("plain.png", "standalone"))
        .unwrap();
    let plain_gltf = import
        .decode_png_base_with_policy(
            &plain_png,
            ImportLocation::new("plain.png", "gltf"),
            PngColorPolicy::GltfMaterialEncoded,
        )
        .unwrap();
    assert!(Arc::ptr_eq(&plain_standalone, &plain_gltf));
    assert_eq!(import.metrics().png_base_decodes, 3);
}

#[test]
fn standard_chromaticities_and_palette_transparency_are_supported_but_unknown_chromaticities_are_not()
 {
    let standard: [u32; 8] = [
        31_270, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
    ];
    let mut standard_bytes = Vec::new();
    for coordinate in standard {
        standard_bytes.extend_from_slice(&coordinate.to_be_bytes());
    }
    let standard_png =
        png_with_chunks_after_ihdr(&gray_png(128), &png_chunk(b"cHRM", &standard_bytes));
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let mut import = context(&mut resolver, &mut cache, &cancellation);
    let standard_image = import
        .decode_png_base(&standard_png, ImportLocation::new("standard.png", "image"))
        .unwrap();
    assert_eq!(standard_image.rgba8(), &[128, 128, 128, 255]);

    let mut unknown = standard;
    unknown[7] = 7_000;
    let mut unknown_bytes = Vec::new();
    for coordinate in unknown {
        unknown_bytes.extend_from_slice(&coordinate.to_be_bytes());
    }
    let unknown_png =
        png_with_chunks_after_ihdr(&gray_png(128), &png_chunk(b"cHRM", &unknown_bytes));
    let error = import
        .decode_png_base(&unknown_png, ImportLocation::new("unknown.png", "image"))
        .unwrap_err();
    assert_eq!(error.code().as_str(), "unsupported");
    let gltf_unknown = import
        .decode_png_base_with_policy(
            &unknown_png,
            ImportLocation::new("unknown.png", "gltf"),
            PngColorPolicy::GltfMaterialEncoded,
        )
        .unwrap();
    assert_eq!(gltf_unknown.rgba8(), &[128, 128, 128, 255]);

    let mut palette_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut palette_bytes, 1, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![128, 64, 32]);
        encoder.set_trns(vec![73]);
        encoder.set_source_gamma(png::ScaledFloat::from_scaled(45_455));
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0]).unwrap();
    }
    let indexed = import
        .decode_png_base(&palette_bytes, ImportLocation::new("palette.png", "image"))
        .unwrap();
    assert_eq!(indexed.rgba8(), &[129, 62, 26, 73]);
}

#[test]
fn malformed_inner_png_cache_payload_is_invalidated_and_rebuilt_from_source() {
    let png = gray_png(91);
    let encoded_hash: [u8; 32] = sha2::Sha256::digest(&png).into();
    let key = CacheKey::new(
        CacheNamespace::Cook,
        domain_hash(
            b"daedalus.png.base-cache.rgba8-srgb-normalize-v2",
            &[&encoded_hash, &[0; 5]],
        ),
    );
    let mut malformed = Vec::from(&b"DPNG"[..]);
    malformed.extend_from_slice(&encoded_hash);
    malformed.extend_from_slice(&2u32.to_le_bytes());
    malformed.extend_from_slice(&1u32.to_le_bytes());
    malformed.extend_from_slice(&[1, 2, 3, 4]); // 2x1 requires eight RGBA bytes.

    let mut cache = MemoryCookCache::default();
    cache
        .publish(key, CacheObjectKind::PngBase, Arc::from(malformed))
        .unwrap();
    let mut resolver = ByteMapSourceResolver::default();
    let cancellation = ImportCancellation::never();
    let (image, metrics) = {
        let mut import = context(&mut resolver, &mut cache, &cancellation);
        let image = import
            .decode_png_base(&png, ImportLocation::new("image.png", "png"))
            .unwrap();
        (image, import.metrics())
    };

    assert_eq!(image.rgba8(), &[91, 91, 91, 255]);
    assert_eq!(
        metrics.cache_hits, 0,
        "a semantic cache failure must not be reported as a usable hit"
    );
    assert_eq!(metrics.cache_corrupt, 1);
    assert_eq!(metrics.png_base_decodes, 1);
    assert_eq!(metrics.cache_writes, 1);
    assert!(matches!(
        cache.read(key, CacheObjectKind::PngBase, 1024).unwrap(),
        CacheRead::Hit(_)
    ));
}

#[test]
fn self_consistent_png_cache_with_wrong_same_size_dimensions_is_rebuilt() {
    let png = gray_png_sized(2, 2, 91);
    let encoded_hash: [u8; 32] = sha2::Sha256::digest(&png).into();
    let key = CacheKey::new(
        CacheNamespace::Cook,
        domain_hash(
            b"daedalus.png.base-cache.rgba8-srgb-normalize-v2",
            &[&encoded_hash, &[0; 5]],
        ),
    );
    let mut malformed = Vec::from(&b"DPNG"[..]);
    malformed.extend_from_slice(&encoded_hash);
    malformed.extend_from_slice(&1u32.to_le_bytes());
    malformed.extend_from_slice(&4u32.to_le_bytes());
    malformed.extend_from_slice(&[9; 16]); // Valid 1x4 RGBA payload, wrong source shape.

    let mut cache = MemoryCookCache::default();
    cache
        .publish(key, CacheObjectKind::PngBase, Arc::from(malformed))
        .unwrap();
    let mut resolver = ByteMapSourceResolver::default();
    let cancellation = ImportCancellation::never();
    let mut import = context(&mut resolver, &mut cache, &cancellation);
    let image = import
        .decode_png_base(&png, ImportLocation::new("shape.png", "image"))
        .unwrap();

    assert_eq!((image.width(), image.height()), (2, 2));
    assert_eq!(image.rgba8(), &[91, 91, 91, 255].repeat(4));
    assert_eq!(import.metrics().cache_corrupt, 1);
    assert_eq!(import.metrics().png_base_decodes, 1);
}

#[test]
fn pre_cancelled_png_import_stops_before_cache_or_decode_work() {
    let png = gray_png(5);
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::from_predicate(|| true);
    let mut import = context(&mut resolver, &mut cache, &cancellation);
    let error = import
        .decode_png_base(&png, ImportLocation::new("image.png", "png"))
        .unwrap_err();

    assert_eq!(error.code().as_str(), "cancelled");
    assert_eq!(
        import.metrics().cache_hits + import.metrics().cache_misses,
        0
    );
    assert_eq!(import.metrics().png_base_decodes, 0);
}

#[test]
fn maximum_axis_png_fits_the_bounded_decode_and_cache_serialization_budget() {
    const AXIS: u32 = 4_096;
    let png = gray_png_sized(AXIS, AXIS, 17);
    let mut resolver = ByteMapSourceResolver::default();
    let mut cache = MemoryCookCache::default();
    let cancellation = ImportCancellation::never();
    let mut import = context(&mut resolver, &mut cache, &cancellation);
    let image = import
        .decode_png_base(&png, ImportLocation::new("large.png", "png"))
        .unwrap();

    assert_eq!(image.width(), AXIS);
    assert_eq!(image.height(), AXIS);
    assert_eq!(image.rgba8().len(), AXIS as usize * AXIS as usize * 4);
    assert_eq!(&image.rgba8()[..4], &[17, 17, 17, 255]);
    assert_eq!(import.metrics().png_base_decodes, 1);
}
