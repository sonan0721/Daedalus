//! CLI surface for the shared CPU asset import/cook pipeline.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use daedalus_content::assets::{
    AssetImportError, AssetImportErrorCode, AssetImportMetrics, AssetKind, CacheLimits,
    CookedResourceKind, FileCookCache, ImportCancellation, ImportLimits,
};
use daedalus_content::project::{PreparedProject, load_project_with_assets};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_FIELD_BYTES: usize = 8 * 1024;
const MAX_ERROR_CHAIN_ENTRIES: usize = 256;
const MAX_ERROR_CHAIN_ITEM_BYTES: usize = 512;

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
    let prepared = load_project_with_assets(
        directory,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .map_err(CookFailure::Import)?;
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
    assets: &'a [daedalus_content::assets::AssetSummary],
    sources: &'a [daedalus_content::assets::AssetSourceSummary],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeConsumers {
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
