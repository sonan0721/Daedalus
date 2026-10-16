//! CLI surface for the shared CPU asset import/cook pipeline.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use daedalus_content::assets::{
    AssetImportError, AssetImportErrorCode, AssetImportMetrics, AssetKind, CacheLimits,
    CookedResourceKind, CpuCookCache, FileCookCache, ImportCancellation, ImportLimits,
    MemoryCookCache,
};
use daedalus_content::project::{PreparedProject, load_project_with_assets};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_FIELD_BYTES: usize = 8 * 1024;
const MAX_ERROR_CHAIN_ENTRIES: usize = 256;
const MAX_ERROR_CHAIN_ITEM_BYTES: usize = 512;

/// Maps imported-asset renderer counters into one CLI JSON shape shared by capture and view.
#[cfg(any(feature = "capture", feature = "window"))]
pub(super) fn asset_resource_stats_json(
    stats: &daedalus_renderer::PacketAssetResourceStats,
) -> Value {
    json!({
        "texture_upload_count":stats.texture_upload_count,
        "texture_upload_bytes":stats.texture_upload_bytes,
        "texture_cache_hit_count":stats.texture_cache_hit_count,
        "texture_cache_miss_count":stats.texture_cache_miss_count,
        "sampler_create_count":stats.sampler_create_count,
        "sampler_cache_hit_count":stats.sampler_cache_hit_count,
        "sampler_cache_miss_count":stats.sampler_cache_miss_count,
        "bind_group_create_count":stats.bind_group_create_count,
        "bind_group_cache_hit_count":stats.bind_group_cache_hit_count,
        "bind_group_cache_miss_count":stats.bind_group_cache_miss_count,
        "active_texture_entries":stats.active_texture_entries,
        "retired_texture_entries":stats.retired_texture_entries,
        "active_sampler_entries":stats.active_sampler_entries,
        "retired_sampler_entries":stats.retired_sampler_entries,
        "active_bind_group_entries":stats.active_bind_group_entries,
        "retired_bind_group_entries":stats.retired_bind_group_entries,
        "resident_active_bytes":stats.resident_active_bytes,
        "resident_retired_bytes":stats.resident_retired_bytes,
        "resident_total_bytes":stats.resident_total_bytes,
        "upload_staging_in_flight_bytes":stats.upload_staging_in_flight_bytes,
        "upload_staging_peak_bytes":stats.upload_staging_peak_bytes,
        "in_flight_submissions":stats.in_flight_submissions,
        "upload_staging_write_count":stats.upload_staging_write_count,
        "upload_staging_write_bytes":stats.upload_staging_write_bytes,
        "cpu_upload_scratch_current_bytes":stats.cpu_upload_scratch_current_bytes,
        "cpu_upload_scratch_peak_bytes":stats.cpu_upload_scratch_peak_bytes,
        "bookkeeping_reserved_bytes":stats.bookkeeping_reserved_bytes,
        "overlay_write_count":stats.overlay_write_count,
        "overlay_write_bytes":stats.overlay_write_bytes,
        "pending_candidate_count":stats.pending_candidate_count,
        "busy_count":stats.busy_count,
        "rejected_count":stats.rejected_count,
        "pending_candidate":stats.pending_candidate,
    })
}

/// Maps packet geometry/instance counters into the legacy CLI `resources` JSON shape.
#[cfg(any(feature = "capture", feature = "window"))]
pub(super) fn packet_resource_stats_json(stats: &daedalus_renderer::PacketResourceStats) -> Value {
    json!({
        "active_bytes":stats.active_bytes,
        "active_entries":stats.active_entries,
        "retired_bytes":stats.retired_bytes,
        "retired_entries":stats.retired_entries,
        "geometry_upload_count":stats.geometry_upload_count,
        "geometry_upload_bytes":stats.geometry_upload_bytes,
        "cache_hit_count":stats.cache_hit_count,
        "cache_miss_count":stats.cache_miss_count,
        "instance_write_count":stats.instance_write_count,
        "instance_write_bytes":stats.instance_write_bytes,
        "material_write_count":stats.material_write_count,
        "material_write_bytes":stats.material_write_bytes,
        "global_write_count":stats.global_write_count,
        "global_write_bytes":stats.global_write_bytes,
        "depth_target_recreation_count":stats.depth_target_recreation_count,
        "cache_eviction_count":stats.cache_eviction_count,
        "pending_candidate_count":stats.pending_candidate_count,
        "pending_candidate":stats.pending_candidate,
    })
}

/// Serializes the immutable camera/projection pair that a capture or window actually used.
#[cfg(any(feature = "capture", feature = "window"))]
pub(super) fn render_view_json(view: &daedalus_renderer::RenderView) -> Value {
    let projection = match view.projection() {
        daedalus_renderer::RenderProjection::Perspective => json!({"kind":"perspective"}),
        daedalus_renderer::RenderProjection::Orthographic { vertical_size } => {
            json!({"kind":"orthographic","vertical_size":vertical_size})
        }
    };
    json!({
        "camera":camera_json(view.camera()),
        "projection":projection,
    })
}

#[cfg(any(feature = "capture", feature = "window"))]
pub(super) fn camera_json(camera: &daedalus_renderer::Camera) -> Value {
    json!({
        "position":camera.position,
        "target":camera.target,
        "up":camera.up,
        "vertical_fov_radians":camera.vertical_fov_radians,
        "near":camera.near,
        "far":camera.far,
    })
}

/// The shared asset-aware loader seam used by lifecycle commands and `project cook`.
/// Callers choose the cache lifetime explicitly; all project parsing/importing stays in content.
pub(super) fn load_project_with_cache(
    directory: &Path,
    cache: &mut dyn CpuCookCache,
) -> Result<PreparedProject, AssetImportError> {
    load_project_with_assets(
        directory,
        cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
}

/// Read-only lifecycle commands use a bounded invocation-local cache and never acquire a disk lock.
pub(super) fn load_project_read_only(
    directory: &Path,
) -> Result<PreparedProject, AssetImportError> {
    let mut cache = MemoryCookCache::default();
    load_project_with_cache(directory, &mut cache)
}

/// Runtime commands eagerly cook into the project-local disk cache, then detach its pins and drop
/// the exclusive cache owner before starting a session or renderer. The returned catalog remains
/// fully owned by its Arc-backed prepared data.
pub(super) fn load_project_runtime(directory: &Path) -> Result<PreparedProject, AssetImportError> {
    let cache_root = directory.join(".daedalus/cache");
    load_project_runtime_with_cache_root(directory, &cache_root)
}

fn load_project_runtime_with_cache_root(
    directory: &Path,
    cache_root: &Path,
) -> Result<PreparedProject, AssetImportError> {
    let directory = validate_runtime_project_root(directory)?;
    let mut cache = FileCookCache::open(cache_root, CacheLimits::default())?;
    let prepared = load_project_with_cache(&directory, &mut cache)?;
    let prepared = prepared.release_asset_cache_pins();
    drop(cache);
    Ok(prepared)
}

fn validate_runtime_project_root(directory: &Path) -> Result<PathBuf, AssetImportError> {
    let canonical_root = std::fs::canonicalize(directory).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Io,
            daedalus_content::assets::ImportLocation::new(
                directory.display().to_string(),
                "project_root",
            ),
            error.to_string(),
        )
    })?;
    if !canonical_root.is_dir() {
        return Err(AssetImportError::new(
            AssetImportErrorCode::InvalidPath,
            daedalus_content::assets::ImportLocation::new(
                canonical_root.display().to_string(),
                "project_root",
            ),
            "project root must be a directory",
        ));
    }
    Ok(canonical_root)
}

pub(super) fn run(
    directory: PathBuf,
    requested_cache: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let cache_root = requested_cache.unwrap_or_else(|| directory.join(".daedalus/cache"));
    match execute(&directory, &cache_root) {
        Ok(line) => {
            io::stdout().lock().write_all(&line)?;
            Ok(())
        }
        Err(failure) => {
            let error = failure.into_import_error();
            let value = error_output(&error);
            let line = encode_json_line(&value).unwrap_or_else(|_| {
                b"{\"command\":\"project_cook\",\"ok\":false,\"error\":{\"code\":\"internal\",\"message\":\"could not serialize bounded asset-cook error\"}}\n".to_vec()
            });
            io::stdout().lock().write_all(&line)?;
            Err(Box::new(CookCommandFailed(
                error.code().as_str().to_owned(),
            )))
        }
    }
}

fn execute(directory: &Path, cache_root: &Path) -> Result<Vec<u8>, CookFailure> {
    let mut cache =
        FileCookCache::open(cache_root, CacheLimits::default()).map_err(CookFailure::Import)?;
    let prepared = load_project_with_cache(directory, &mut cache).map_err(CookFailure::Import)?;
    let report = prepared.asset_import_report().ok_or_else(|| {
        CookFailure::Import(AssetImportError::new(
            AssetImportErrorCode::Internal,
            daedalus_content::assets::ImportLocation::new("project", "asset_import_report"),
            "asset-aware project loading returned no per-invocation import report",
        ))
    })?;
    let counts = CookCounts::from_project(&prepared, report.assets());
    let cache_root_display = cache_root.display().to_string();
    let output = CookOutput {
        command: "project_cook",
        ok: true,
        project_name: &prepared.manifest().name,
        schema_version: prepared.manifest().schema_version,
        source_scene_revision: prepared.scene().revision,
        project_source_digest: prepared.digest(),
        catalog_digest: report.catalog_digest(),
        cache_root: &cache_root_display,
        cache_namespace: "v1",
        runtime_consumers: RuntimeConsumers {
            model_rendering: false,
            image_rendering: false,
            sprite_playback: false,
            audio_playback: false,
        },
        counts,
        metrics: report.metrics(),
        memory: report.memory(),
        assets: report.assets(),
        sources: report.sources(),
    };
    encode_json_line(&output).map_err(|error| match error {
        JsonEncodeError::TooLarge => CookFailure::Import(AssetImportError::new(
            AssetImportErrorCode::LimitExceeded,
            daedalus_content::assets::ImportLocation::new("project", "asset_cook_summary"),
            format!("project-cook JSON line exceeds the {MAX_JSON_LINE_BYTES}-byte output limit"),
        )),
        JsonEncodeError::Serialize(message) => CookFailure::Import(AssetImportError::new(
            AssetImportErrorCode::Internal,
            daedalus_content::assets::ImportLocation::new("project", "asset_cook_summary"),
            format!("could not serialize project-cook summary: {message}"),
        )),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookOutput<'a> {
    command: &'static str,
    ok: bool,
    project_name: &'a str,
    schema_version: u32,
    source_scene_revision: u64,
    project_source_digest: &'a str,
    catalog_digest: &'a str,
    cache_root: &'a str,
    cache_namespace: &'static str,
    runtime_consumers: RuntimeConsumers,
    counts: CookCounts,
    metrics: &'a AssetImportMetrics,
    memory: &'a daedalus_content::assets::AssetImportMemorySummary,
    assets: &'a [daedalus_content::assets::AssetSummary],
    sources: &'a [daedalus_content::assets::AssetSourceSummary],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeConsumers {
    // These describe this CPU-only `project cook` invocation, not engine capability support.
    model_rendering: bool,
    image_rendering: bool,
    sprite_playback: bool,
    audio_playback: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookCounts {
    declared_assets: usize,
    imported_assets: usize,
    legacy_meshes: usize,
    source_files: usize,
    assets_by_kind: AssetKindCounts,
    unique_cooked_resources: CookedResourceCounts,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetKindCounts {
    models: usize,
    images: usize,
    sprite_atlases: usize,
    audio: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookedResourceCounts {
    models: usize,
    geometry: usize,
    images: usize,
    atlases: usize,
    audio: usize,
}

impl CookCounts {
    fn from_project(
        project: &PreparedProject,
        summaries: &[daedalus_content::assets::AssetSummary],
    ) -> Self {
        let mut assets_by_kind = AssetKindCounts {
            models: 0,
            images: 0,
            sprite_atlases: 0,
            audio: 0,
        };
        let mut resources = BTreeSet::<(&'static str, String)>::new();
        for summary in summaries {
            match summary.kind() {
                AssetKind::Model => assets_by_kind.models += 1,
                AssetKind::Image => assets_by_kind.images += 1,
                AssetKind::SpriteAtlas => assets_by_kind.sprite_atlases += 1,
                AssetKind::Audio => assets_by_kind.audio += 1,
            }
            for key in summary.resource_keys() {
                resources.insert((resource_kind_name(key.kind()), key.key().to_owned()));
            }
        }
        let count_resource = |kind| {
            resources
                .iter()
                .filter(|(actual, _)| *actual == kind)
                .count()
        };
        Self {
            declared_assets: project.scene().assets.len(),
            imported_assets: summaries.len(),
            legacy_meshes: project.meshes().len(),
            source_files: project
                .asset_import_report()
                .map_or(0, |report| report.sources().len()),
            assets_by_kind,
            unique_cooked_resources: CookedResourceCounts {
                models: count_resource("model"),
                geometry: count_resource("geometry"),
                images: count_resource("image"),
                atlases: count_resource("atlas"),
                audio: count_resource("audio"),
            },
        }
    }
}

fn resource_kind_name(kind: CookedResourceKind) -> &'static str {
    match kind {
        CookedResourceKind::Model => "model",
        CookedResourceKind::Geometry => "geometry",
        CookedResourceKind::Image => "image",
        CookedResourceKind::Atlas => "atlas",
        CookedResourceKind::Audio => "audio",
    }
}

fn error_output(error: &AssetImportError) -> Value {
    let source_id = error
        .source_id()
        .map(|text| bounded_text(text, MAX_ERROR_FIELD_BYTES));
    let path = error
        .path()
        .map(|text| bounded_text(text, MAX_ERROR_FIELD_BYTES));
    let message = bounded_text(error.message(), MAX_ERROR_FIELD_BYTES);
    let chain = error
        .dependency_chain()
        .iter()
        .take(MAX_ERROR_CHAIN_ENTRIES)
        .map(|entry| bounded_text(entry, MAX_ERROR_CHAIN_ITEM_BYTES))
        .collect::<Vec<_>>();
    let chain_items_truncated = error
        .dependency_chain()
        .iter()
        .take(MAX_ERROR_CHAIN_ENTRIES)
        .any(|entry| entry.len() > MAX_ERROR_CHAIN_ITEM_BYTES);
    let omitted = error
        .dependency_chain()
        .len()
        .saturating_sub(MAX_ERROR_CHAIN_ENTRIES);
    json!({
        "command":"project_cook",
        "ok":false,
        "error":{
            "code":error.code().as_str(),
            "source_id":source_id,
            "path":path,
            "dependency_chain":chain,
            "dependency_chain_omitted":omitted,
            "dependency_chain_items_truncated":chain_items_truncated,
            "message":message,
        }
    })
}

fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let marker = "…";
    let mut end = maximum_bytes.saturating_sub(marker.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = value[..end].to_owned();
    bounded.push_str(marker);
    bounded
}

fn encode_json_line(value: &impl Serialize) -> Result<Vec<u8>, JsonEncodeError> {
    let mut output = CappedBuffer {
        bytes: Vec::with_capacity(4 * 1024),
        limit: MAX_JSON_LINE_BYTES,
        exceeded: false,
    };
    serde_json::to_writer(&mut output, value).map_err(|error| {
        if output.exceeded {
            JsonEncodeError::TooLarge
        } else {
            JsonEncodeError::Serialize(error.to_string())
        }
    })?;
    output
        .write_all(b"\n")
        .map_err(|_| JsonEncodeError::TooLarge)?;
    Ok(output.bytes)
}

struct CappedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
enum JsonEncodeError {
    TooLarge,
    Serialize(String),
}

enum CookFailure {
    Import(AssetImportError),
}

impl CookFailure {
    fn into_import_error(self) -> AssetImportError {
        match self {
            Self::Import(error) => error,
        }
    }
}

#[cfg(test)]
mod loader_tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use daedalus_content::assets::{AssetImportErrorCode, CacheLimits, FileCookCache};
    use daedalus_content::project::ProjectManifest;
    use daedalus_content::scene::{AssetDescriptor, SceneDocument, StableId};

    use super::{
        CookFailure, execute, load_project_read_only, load_project_runtime_with_cache_root,
    };

    static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);

    fn write_audio_project(path: &std::path::Path) {
        fs::create_dir_all(path).unwrap();
        fs::write(
            path.join("daedalus.project.json"),
            serde_json::to_vec(&ProjectManifest::new("loader-test")).unwrap(),
        )
        .unwrap();
        let mut scene = SceneDocument::default();
        scene.assets.push(AssetDescriptor {
            id: StableId::new("asset:tone").unwrap(),
            kind: StableId::new("daedalus.audio").unwrap(),
            uri: "tone.wav".into(),
            content_hash: None,
            metadata: Default::default(),
        });
        fs::write(path.join("scene.json"), serde_json::to_vec(&scene).unwrap()).unwrap();

        // One short mono 16-bit PCM WAV; the importer produces a real cache entry and catalog pin.
        let samples = [0_u8, 0, 0xff, 0x7f];
        let mut wav = Vec::with_capacity(48);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + samples.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&16_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        fs::write(path.join("tone.wav"), wav).unwrap();
    }

    fn temporary_root() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "daedalus-cli-loader-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed),
        ))
    }

    #[test]
    fn lifecycle_loaders_share_asset_aware_digest_and_release_runtime_cache_owner() {
        let root = temporary_root();
        let project_path = root.join("project");
        let cache_root = root.join("cache");
        write_audio_project(&project_path);

        let checked = load_project_read_only(&project_path).unwrap();
        assert!(checked.asset_catalog().is_some());
        assert!(checked.asset_import_report().is_some());
        assert!(!project_path.join(".daedalus").exists());

        let runtime = load_project_runtime_with_cache_root(&project_path, &cache_root).unwrap();
        assert_eq!(runtime.digest(), checked.digest());
        assert!(runtime.asset_catalog().is_some());
        runtime.validate().unwrap();
        let audio_id = StableId::new("asset:tone").unwrap();
        assert!(matches!(
            runtime.asset_catalog().unwrap().asset(&audio_id),
            Some(daedalus_content::assets::ImportedAsset::Audio(_))
        ));

        // The helper must detach catalog pins and drop its FileCookCache before returning.
        let reopened = FileCookCache::open(&cache_root, CacheLimits::default()).unwrap();
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_loader_rejects_missing_or_file_root_before_creating_cache() {
        let root = temporary_root();
        fs::create_dir_all(&root).unwrap();

        let missing = root.join("missing-project");
        let missing_cache = root.join("missing-cache");
        let missing_error = match load_project_runtime_with_cache_root(&missing, &missing_cache) {
            Ok(_) => panic!("missing project root unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(missing_error.code(), AssetImportErrorCode::Io);
        assert_eq!(missing_error.source_id(), Some(missing.to_str().unwrap()));
        assert_eq!(missing_error.path(), Some("project_root"));
        assert!(!missing.exists());
        assert!(!missing_cache.exists());

        let file_root = root.join("not-a-project-directory");
        fs::write(&file_root, b"not a directory").unwrap();
        let file_cache = root.join("file-cache");
        let file_error = match load_project_runtime_with_cache_root(&file_root, &file_cache) {
            Ok(_) => panic!("file project root unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(file_error.code(), AssetImportErrorCode::InvalidPath);
        let canonical_file_root = fs::canonicalize(&file_root).unwrap();
        assert_eq!(
            file_error.source_id(),
            Some(canonical_file_root.to_str().unwrap())
        );
        assert_eq!(file_error.path(), Some("project_root"));
        assert!(!file_cache.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_cook_emits_the_import_report_memory_summary_verbatim() {
        let root = temporary_root();
        let project_path = root.join("project");
        let cache_root = root.join("cache");
        write_audio_project(&project_path);

        let output = match execute(&project_path, &cache_root) {
            Ok(output) => output,
            Err(CookFailure::Import(error)) => {
                panic!(
                    "project cook summary generation failed: {}",
                    error.code().as_str()
                )
            }
        };
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let expected_project = load_project_read_only(&project_path).unwrap();
        let report = expected_project.asset_import_report().unwrap();
        let expected_memory = serde_json::to_value(report.memory()).unwrap();
        let expected_source_count = report.sources().len() as u64;

        assert_eq!(value.get("memory"), Some(&expected_memory));
        assert!(expected_memory["cookedAccountedBytes"].as_u64().unwrap() > 0);
        assert_eq!(
            expected_memory["uniqueSourceCount"].as_u64(),
            Some(expected_source_count)
        );

        fs::remove_dir_all(root).unwrap();
    }
}

struct CookCommandFailed(String);

impl fmt::Display for CookCommandFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "project cook failed ({})", self.0)
    }
}

impl fmt::Debug for CookCommandFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Error for CookCommandFailed {}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use daedalus_content::assets::{AssetImportError, AssetImportErrorCode, ImportLocation};

    use super::{MAX_JSON_LINE_BYTES, encode_json_line, error_output};

    #[derive(Serialize)]
    struct LargeSummary(String);

    #[test]
    fn json_line_encoder_refuses_oversized_summary_before_writing() {
        let value = LargeSummary("x".repeat(MAX_JSON_LINE_BYTES));
        assert!(matches!(
            encode_json_line(&value),
            Err(super::JsonEncodeError::TooLarge)
        ));
    }

    #[test]
    fn error_output_keeps_typed_location_and_bounds_adversarial_diagnostics() {
        let chain = (0..300).map(|_| "\0".repeat(4_096)).collect::<Vec<_>>();
        let error = AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            ImportLocation::new("models/a.gltf", "meshes[0].primitives[0]")
                .with_dependency_chain(chain),
            "unsupported extension",
        );
        let value = error_output(&error);
        let bytes = encode_json_line(&value).expect("bounded error JSON must fit");
        assert!(bytes.len() <= MAX_JSON_LINE_BYTES);
        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded["error"]["code"], "unsupported");
        assert_eq!(decoded["error"]["source_id"], "models/a.gltf");
        assert_eq!(decoded["error"]["path"], "meshes[0].primitives[0]");
        assert_eq!(decoded["error"]["dependency_chain_omitted"], 44);
    }
}
