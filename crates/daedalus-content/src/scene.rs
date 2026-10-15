//! Versioned, AI-readable scene and asset descriptors with atomic patching.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Revision;

pub const SCENE_SCHEMA_VERSION: u32 = 1;
const MAX_STABLE_ID_BYTES: usize = 128;

/// A stable, human-readable identifier. IDs are ASCII so their serialized form is portable.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct StableId(String);

impl StableId {
    pub fn new(value: impl Into<String>) -> Result<Self, StableIdError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_STABLE_ID_BYTES {
            return Err(StableIdError);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        }) {
            return Err(StableIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StableIdError;

impl fmt::Display for StableIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stable ID must be 1..=128 ASCII letters, digits, '.', '_', ':', '/', or '-'")
    }
}

impl std::error::Error for StableIdError {}

impl<'de> Deserialize<'de> for StableId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneDocument {
    pub schema_version: u32,
    pub revision: Revision,
    #[serde(default)]
    pub entities: Vec<SceneEntity>,
    #[serde(default)]
    pub assets: Vec<AssetDescriptor>,
}

impl Default for SceneDocument {
    fn default() -> Self {
        Self {
            schema_version: SCENE_SCHEMA_VERSION,
            revision: 0,
            entities: Vec::new(),
            assets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneEntity {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<StableId>,
    #[serde(default)]
    pub components: Vec<ComponentDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentDescriptor {
    pub id: StableId,
    pub type_id: StableId,
    #[serde(default)]
    pub asset_refs: Vec<StableId>,
    /// Versioned component-specific parameters remain readable JSON data.
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetDescriptor {
    pub id: StableId,
    pub kind: StableId,
    /// URI/path of the source or cooked representation; the importing host owns resolution.
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneValidationError {
    pub issues: Vec<ValidationIssue>,
}

impl fmt::Display for SceneValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scene has {} validation issue(s)", self.issues.len())
    }
}

impl std::error::Error for SceneValidationError {}

/// Checks schema version, stable-ID uniqueness, asset references, parent existence, and cycles.
pub fn validate_scene(document: &SceneDocument) -> Result<(), SceneValidationError> {
    let mut issues = Vec::new();
    if document.schema_version != SCENE_SCHEMA_VERSION {
        issues.push(ValidationIssue {
            path: "schema_version".into(),
            message: format!("unsupported schema version {}", document.schema_version),
        });
    }

    let mut all_ids = BTreeSet::new();
    let mut entity_ids = BTreeSet::new();
    let mut asset_ids = BTreeSet::new();
    for (index, asset) in document.assets.iter().enumerate() {
        let path = format!("assets[{index}]");
        register_id(&mut all_ids, &asset.id, &format!("{path}.id"), &mut issues);
        asset_ids.insert(asset.id.clone());
        validate_nonempty_id(&asset.kind, &format!("{path}.kind"), &mut issues);
        if asset.uri.trim().is_empty() {
            issues.push(ValidationIssue {
                path: format!("{path}.uri"),
                message: "asset URI must not be empty".into(),
            });
        }
        if asset
            .content_hash
            .as_ref()
            .is_some_and(|hash| hash.trim().is_empty())
        {
            issues.push(ValidationIssue {
                path: format!("{path}.content_hash"),
                message: "content hash must not be empty when present".into(),
            });
        }
    }

    for (entity_index, entity) in document.entities.iter().enumerate() {
        let entity_path = format!("entities[{entity_index}]");
        register_id(
            &mut all_ids,
            &entity.id,
            &format!("{entity_path}.id"),
            &mut issues,
        );
        entity_ids.insert(entity.id.clone());
        if entity
            .name
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            issues.push(ValidationIssue {
                path: format!("{entity_path}.name"),
                message: "entity name must not be empty when present".into(),
            });
        }
        for (component_index, component) in entity.components.iter().enumerate() {
            let path = format!("{entity_path}.components[{component_index}]");
            register_id(
                &mut all_ids,
                &component.id,
                &format!("{path}.id"),
                &mut issues,
            );
            validate_nonempty_id(&component.type_id, &format!("{path}.type_id"), &mut issues);
            for reference in &component.asset_refs {
                if !asset_ids.contains(reference)
                    && !document.assets.iter().any(|asset| asset.id == *reference)
                {
                    issues.push(ValidationIssue {
                        path: format!("{path}.asset_refs"),
                        message: format!("unknown asset reference '{reference}'"),
                    });
                }
            }
        }
    }

    for (index, entity) in document.entities.iter().enumerate() {
        if let Some(parent) = &entity.parent
            && !entity_ids.contains(parent)
        {
            issues.push(ValidationIssue {
                path: format!("entities[{index}].parent"),
                message: format!("unknown parent entity '{parent}'"),
            });
        }
    }
    validate_parent_cycles(document, &mut issues);

    if issues.is_empty() {
        Ok(())
    } else {
        Err(SceneValidationError { issues })
    }
}

fn validate_nonempty_id(id: &StableId, path: &str, issues: &mut Vec<ValidationIssue>) {
    if id.0.is_empty() || id.0.len() > MAX_STABLE_ID_BYTES || !id.0.is_ascii() {
        issues.push(ValidationIssue {
            path: path.into(),
            message: "invalid stable ID".into(),
        });
    } else if !id.0.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
    }) {
        issues.push(ValidationIssue {
            path: path.into(),
            message: "invalid stable ID characters".into(),
        });
    }
}

fn register_id(
    seen: &mut BTreeSet<StableId>,
    id: &StableId,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    validate_nonempty_id(id, path, issues);
    if !seen.insert(id.clone()) {
        issues.push(ValidationIssue {
            path: path.into(),
            message: format!("duplicate stable ID '{id}'"),
        });
    }
}

fn validate_parent_cycles(document: &SceneDocument, issues: &mut Vec<ValidationIssue>) {
    let parents: BTreeMap<&StableId, Option<&StableId>> = document
        .entities
        .iter()
        .map(|entity| (&entity.id, entity.parent.as_ref()))
        .collect();
    let mut finished = BTreeSet::new();
    for entity in &document.entities {
        let mut path = BTreeSet::new();
        let mut current = Some(&entity.id);
        while let Some(id) = current {
            if finished.contains(id) {
                break;
            }
            if !path.insert(id) {
                issues.push(ValidationIssue {
                    path: format!("entities[id={id}].parent"),
                    message: "parent relation contains a cycle".into(),
                });
                break;
            }
            current = parents.get(id).copied().flatten();
        }
        finished.extend(path);
    }
}

/// A compact patch language; a patch is validated and committed as one revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SceneOperation {
    AddEntity {
        entity: SceneEntity,
    },
    RemoveEntity {
        entity_id: StableId,
    },
    SetEntityParent {
        entity_id: StableId,
        parent: Option<StableId>,
    },
    UpsertComponent {
        entity_id: StableId,
        component: ComponentDescriptor,
    },
    RemoveComponent {
        entity_id: StableId,
        component_id: StableId,
    },
    UpsertAsset {
        asset: AssetDescriptor,
    },
    RemoveAsset {
        asset_id: StableId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenePatch {
    pub expected_revision: Revision,
    pub operations: Vec<SceneOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchError {
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    Invalid(SceneValidationError),
    MissingEntity(StableId),
    MissingComponent(StableId),
    MissingAsset(StableId),
    DuplicateEntity(StableId),
    RevisionOverflow,
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleRevision { expected, actual } => {
                write!(
                    f,
                    "stale scene revision: expected {expected}, actual {actual}"
                )
            }
            Self::Invalid(error) => error.fmt(f),
            Self::MissingEntity(id) => write!(f, "entity '{id}' does not exist"),
            Self::MissingComponent(id) => write!(f, "component '{id}' does not exist"),
            Self::MissingAsset(id) => write!(f, "asset '{id}' does not exist"),
            Self::DuplicateEntity(id) => write!(f, "entity '{id}' already exists"),
            Self::RevisionOverflow => f.write_str("scene revision overflow"),
        }
    }
}

impl std::error::Error for PatchError {}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneSnapshot {
    pub document: SceneDocument,
}

/// Owns one validated scene and applies edits transactionally.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneStore {
    document: SceneDocument,
}

impl SceneStore {
    pub fn new(document: SceneDocument) -> Result<Self, SceneValidationError> {
        validate_scene(&document)?;
        Ok(Self { document })
    }

    pub fn document(&self) -> &SceneDocument {
        &self.document
    }

    pub fn apply_patch(&mut self, patch: ScenePatch) -> Result<SceneSnapshot, PatchError> {
        if patch.expected_revision != self.document.revision {
            return Err(PatchError::StaleRevision {
                expected: patch.expected_revision,
                actual: self.document.revision,
            });
        }
        let next_revision = self
            .document
            .revision
            .checked_add(1)
            .ok_or(PatchError::RevisionOverflow)?;
        let mut staged = self.document.clone();
        for operation in patch.operations {
            apply_operation(&mut staged, operation)?;
        }
        staged.revision = next_revision;
        validate_scene(&staged).map_err(PatchError::Invalid)?;
        self.document = staged;
        Ok(SceneSnapshot {
            document: self.document.clone(),
        })
    }
}

fn apply_operation(
    document: &mut SceneDocument,
    operation: SceneOperation,
) -> Result<(), PatchError> {
    match operation {
        SceneOperation::AddEntity { entity } => {
            if document
                .entities
                .iter()
                .any(|existing| existing.id == entity.id)
            {
                return Err(PatchError::DuplicateEntity(entity.id));
            }
            document.entities.push(entity);
        }
        SceneOperation::RemoveEntity { entity_id } => {
            let index = document
                .entities
                .iter()
                .position(|entity| entity.id == entity_id)
                .ok_or_else(|| PatchError::MissingEntity(entity_id.clone()))?;
            document.entities.remove(index);
        }
        SceneOperation::SetEntityParent { entity_id, parent } => {
            let entity = document
                .entities
                .iter_mut()
                .find(|entity| entity.id == entity_id)
                .ok_or_else(|| PatchError::MissingEntity(entity_id.clone()))?;
            entity.parent = parent;
        }
        SceneOperation::UpsertComponent {
            entity_id,
            component,
        } => {
            let entity = document
                .entities
                .iter_mut()
                .find(|entity| entity.id == entity_id)
                .ok_or_else(|| PatchError::MissingEntity(entity_id.clone()))?;
            if let Some(existing) = entity
                .components
                .iter_mut()
                .find(|existing| existing.id == component.id)
            {
                *existing = component;
            } else {
                entity.components.push(component);
            }
        }
        SceneOperation::RemoveComponent {
            entity_id,
            component_id,
        } => {
            let entity = document
                .entities
                .iter_mut()
                .find(|entity| entity.id == entity_id)
                .ok_or_else(|| PatchError::MissingEntity(entity_id.clone()))?;
            let index = entity
                .components
                .iter()
                .position(|component| component.id == component_id)
                .ok_or_else(|| PatchError::MissingComponent(component_id.clone()))?;
            entity.components.remove(index);
        }
        SceneOperation::UpsertAsset { asset } => {
            if let Some(existing) = document
                .assets
                .iter_mut()
                .find(|existing| existing.id == asset.id)
            {
                *existing = asset;
            } else {
                document.assets.push(asset);
            }
        }
        SceneOperation::RemoveAsset { asset_id } => {
            let index = document
                .assets
                .iter()
                .position(|asset| asset.id == asset_id)
                .ok_or_else(|| PatchError::MissingAsset(asset_id.clone()))?;
            document.assets.remove(index);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> StableId {
        StableId::new(value).unwrap()
    }

    fn entity(entity_id: &str) -> SceneEntity {
        SceneEntity {
            id: id(entity_id),
            name: Some(entity_id.into()),
            parent: None,
            components: vec![],
        }
    }

    #[test]
    fn scene_round_trips_as_readable_versioned_json() {
        let document = SceneDocument {
            assets: vec![AssetDescriptor {
                id: id("asset:hero"),
                kind: id("texture"),
                uri: "asset://hero.png".into(),
                content_hash: Some("sha256:abc".into()),
                metadata: BTreeMap::new(),
            }],
            entities: vec![SceneEntity {
                id: id("entity:hero"),
                name: Some("Hero".into()),
                parent: None,
                components: vec![ComponentDescriptor {
                    id: id("component:sprite"),
                    type_id: id("sprite"),
                    asset_refs: vec![id("asset:hero")],
                    data: json!({ "layer": 2 }),
                }],
            }],
            ..SceneDocument::default()
        };
        validate_scene(&document).unwrap();
        let encoded = serde_json::to_string_pretty(&document).unwrap();
        assert!(encoded.contains("schema_version"));
        let decoded: SceneDocument = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, document);
    }

    #[test]
    fn patch_is_atomic_on_validation_failure_and_stale_revision() {
        let mut document = SceneDocument {
            entities: vec![entity("root")],
            ..SceneDocument::default()
        };
        let mut store = SceneStore::new(document.clone()).unwrap();

        let failed = store.apply_patch(ScenePatch {
            expected_revision: 0,
            operations: vec![
                SceneOperation::AddEntity {
                    entity: entity("child"),
                },
                SceneOperation::SetEntityParent {
                    entity_id: id("child"),
                    parent: Some(id("missing")),
                },
            ],
        });
        assert!(matches!(failed, Err(PatchError::Invalid(_))));
        assert_eq!(store.document(), &document);

        let committed = store
            .apply_patch(ScenePatch {
                expected_revision: 0,
                operations: vec![SceneOperation::AddEntity {
                    entity: entity("child"),
                }],
            })
            .unwrap();
        assert_eq!(committed.document.revision, 1);
        assert!(matches!(
            store.apply_patch(ScenePatch {
                expected_revision: 0,
                operations: vec![],
            }),
            Err(PatchError::StaleRevision {
                expected: 0,
                actual: 1
            })
        ));

        document.revision = 1;
        assert_eq!(store.document(), &document_with_two_entities(document));
    }

    fn document_with_two_entities(mut document: SceneDocument) -> SceneDocument {
        document.entities.push(entity("child"));
        document
    }

    #[test]
    fn parent_cycles_and_missing_asset_references_are_rejected() {
        let mut left = entity("left");
        let mut right = entity("right");
        left.parent = Some(id("right"));
        right.parent = Some(id("left"));
        let error = validate_scene(&SceneDocument {
            entities: vec![left, right],
            ..SceneDocument::default()
        })
        .unwrap_err();
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.message.contains("cycle"))
        );

        let mut invalid_reference = entity("owner");
        invalid_reference.components.push(ComponentDescriptor {
            id: id("component:missing-asset"),
            type_id: id("sprite"),
            asset_refs: vec![id("asset:missing")],
            data: Value::Null,
        });
        let error = validate_scene(&SceneDocument {
            entities: vec![invalid_reference],
            ..SceneDocument::default()
        })
        .unwrap_err();
        assert!(
            error
                .issues
                .iter()
                .any(|issue| issue.message.contains("unknown asset reference"))
        );

        assert!(serde_json::from_str::<StableId>("\"bad id\"").is_err());
    }
}
