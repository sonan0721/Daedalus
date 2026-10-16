//! Bounded CPU importers for the schema-1 external project asset kinds.

mod atlas;
mod audio;
mod binary;
mod gltf_static;
mod image;

use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::source::SourceBlob;
use super::{
    AssetImportContext, AssetImportError, AssetImportErrorCode, ImportLocation, ImportedAsset,
};
use crate::scene::{AssetDescriptor, StableId};

pub(crate) fn import_asset(
    descriptor: &AssetDescriptor,
    source: Arc<SourceBlob>,
    context: &mut AssetImportContext<'_>,
) -> Result<ImportedAsset, AssetImportError> {
    match descriptor.kind.as_str() {
        "daedalus.model" => gltf_static::import(descriptor, source, context),
        "daedalus.image" => image::import(descriptor, source, context),
        "daedalus.sprite_atlas" => atlas::import(descriptor, source, context),
        "daedalus.audio" => audio::import(descriptor, source, context),
        _ => Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(&source, "kind"),
            format!("unsupported asset kind '{}'", descriptor.kind),
        )),
    }
}

fn location(source: &SourceBlob, path: impl Into<String>) -> ImportLocation {
    ImportLocation::new(source.source_id().to_string(), path)
        .with_dependency_chain([source.source_id().to_string()])
}

fn parse_settings<T: DeserializeOwned + Default>(
    metadata: &std::collections::BTreeMap<String, serde_json::Value>,
    source: &SourceBlob,
    path: &str,
) -> Result<T, AssetImportError> {
    if metadata.is_empty() {
        return Ok(T::default());
    }
    let bytes = serde_json::to_vec(metadata).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Internal,
            location(source, path),
            format!("asset metadata could not be serialized for strict validation: {error}"),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Json,
            location(source, path),
            format!("asset metadata is invalid: {error}"),
        )
    })
}

fn check_schema_version(
    version: u32,
    source: &SourceBlob,
    path: &str,
) -> Result<(), AssetImportError> {
    if version != 1 {
        return Err(AssetImportError::new(
            AssetImportErrorCode::Unsupported,
            location(source, path),
            format!("unsupported settings schema_version {version}; expected 1"),
        ));
    }
    Ok(())
}

fn parse_id(value: &str, source: &SourceBlob, path: &str) -> Result<StableId, AssetImportError> {
    StableId::new(value).map_err(|error| {
        AssetImportError::new(
            AssetImportErrorCode::Invalid,
            location(source, path),
            format!("invalid stable ID: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::check_schema_version;
    use super::parse_settings;
    use crate::assets::source::{SourceBlob, SourceId};
    use crate::scene::AssetDescriptor;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StrictSettings {
        schema_version: u32,
        #[serde(default)]
        enabled: bool,
    }

    impl Default for StrictSettings {
        fn default() -> Self {
            Self {
                schema_version: 1,
                enabled: false,
            }
        }
    }

    fn source() -> SourceBlob {
        SourceBlob::new(SourceId::new("assets/test.json").unwrap(), Vec::<u8>::new())
    }

    #[test]
    fn metadata_absence_defaults_and_nonempty_metadata_is_strict() {
        let descriptor = AssetDescriptor {
            id: crate::scene::StableId::new("test").unwrap(),
            kind: crate::scene::StableId::new("daedalus.image").unwrap(),
            uri: "test.png".into(),
            content_hash: None,
            metadata: Default::default(),
        };
        let absent: StrictSettings =
            parse_settings(&descriptor.metadata, &source(), "metadata").unwrap();
        assert_eq!(absent.schema_version, 1);
        assert!(!absent.enabled);

        let metadata = serde_json::from_str(r#"{"schema_version":1,"unknown":true}"#).unwrap();
        let settings: std::collections::BTreeMap<String, serde_json::Value> = metadata;
        assert!(parse_settings::<StrictSettings>(&settings, &source(), "metadata").is_err());
    }

    #[test]
    fn schema_version_is_explicit_and_fixed_for_nonempty_metadata() {
        let source = source();
        assert!(check_schema_version(1, &source, "metadata").is_ok());
        assert!(check_schema_version(2, &source, "metadata").is_err());
    }
}
