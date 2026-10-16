//! Contained project-relative source identities and native/byte resolver adapters.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::types::{AssetImportError, AssetImportErrorCode, ImportLocation};

/// A normalized UTF-8 project-relative path using forward slashes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: &str) -> Result<Self, AssetImportError> {
        normalize_source_id(value, None)
    }

    pub fn resolve_relative(parent: &Self, uri: &str) -> Result<Self, AssetImportError> {
        normalize_source_id(uri, Some(parent))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn normalize_source_id(
    value: &str,
    parent: Option<&SourceId>,
) -> Result<SourceId, AssetImportError> {
    let fail = |message: &str| {
        AssetImportError::new(
            AssetImportErrorCode::InvalidPath,
            ImportLocation::new(value, "uri"),
            message,
        )
    };
    if value.is_empty() || value.len() > 4096 || value.contains('\0') || value.contains('\\') {
        return Err(fail(
            "source ID is empty, too long, or contains a forbidden byte",
        ));
    }
    if value.starts_with('/') || Path::new(value).is_absolute() || value.contains('%') {
        return Err(fail(
            "absolute paths and percent-encoded paths are not accepted",
        ));
    }
    if value
        .split('/')
        .next()
        .is_some_and(|first| first.contains(':'))
    {
        return Err(fail(
            "URI schemes and drive-qualified paths are not accepted",
        ));
    }

    let mut parts = Vec::<&str>::new();
    if let Some(parent) = parent {
        let parent_parts = parent.0.split('/').collect::<Vec<_>>();
        parts.extend(
            parent_parts.into_iter().take(
                parent
                    .0
                    .rfind('/')
                    .map_or(0, |index| parent.0[..index].split('/').count()),
            ),
        );
    }
    for part in value.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(fail("relative path escapes the project root"));
                }
            }
            _ if part.contains(':') => {
                return Err(fail("colon is not allowed in a path component"));
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(fail("source ID resolves to the project root"));
    }
    let normalized = parts.join("/");
    if normalized.len() > 4096 {
        return Err(fail("normalized source ID exceeds 4096 UTF-8 bytes"));
    }
    Ok(SourceId(normalized))
}

/// Immutable bytes and their computed SHA-256 identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceBlob {
    source_id: SourceId,
    bytes: Arc<[u8]>,
    sha256: [u8; 32],
}

impl SourceBlob {
    pub fn new(source_id: SourceId, bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        let sha256 = Sha256::digest(&bytes).into();
        Self {
            source_id,
            bytes,
            sha256,
        }
    }

    pub fn source_id(&self) -> &SourceId {
        &self.source_id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn bytes_arc(&self) -> &Arc<[u8]> {
        &self.bytes
    }
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub fn byte_len(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Resolves a single normalized project-relative source under caller-owned I/O policy.
pub trait SourceResolver {
    fn read_source(
        &mut self,
        source_id: &SourceId,
        max_bytes: u64,
    ) -> Result<SourceBlob, AssetImportError>;
}

/// In-memory adapter used by byte APIs and parity tests.
#[derive(Clone, Debug, Default)]
pub struct ByteMapSourceResolver {
    sources: BTreeMap<SourceId, Arc<[u8]>>,
}

impl ByteMapSourceResolver {
    pub fn from_bytes(sources: BTreeMap<String, Vec<u8>>) -> Result<Self, AssetImportError> {
        let mut normalized = BTreeMap::new();
        for (id, bytes) in sources {
            let source_id = SourceId::new(&id)?;
            if normalized
                .insert(source_id.clone(), Arc::from(bytes))
                .is_some()
            {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::InvalidPath,
                    ImportLocation::new(source_id.to_string(), "uri"),
                    "byte source map contains duplicate normalized source IDs",
                ));
            }
        }
        Ok(Self {
            sources: normalized,
        })
    }

    pub fn insert(
        &mut self,
        source_id: SourceId,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Option<Arc<[u8]>> {
        self.sources.insert(source_id, bytes.into())
    }
}

impl SourceResolver for ByteMapSourceResolver {
    fn read_source(
        &mut self,
        source_id: &SourceId,
        max_bytes: u64,
    ) -> Result<SourceBlob, AssetImportError> {
        let bytes = self.sources.get(source_id).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::Io,
                ImportLocation::new(source_id.to_string(), "source"),
                "source is not present in the byte source map",
            )
        })?;
        if bytes.len() as u64 > max_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::new(source_id.to_string(), "source"),
                format!(
                    "source has {} bytes, exceeding the {max_bytes}-byte source limit",
                    bytes.len()
                ),
            ));
        }
        Ok(SourceBlob::new(source_id.clone(), bytes.clone()))
    }
}

/// Native project-root resolver that rejects symlink escapes and bounded-read overflow.
#[derive(Debug)]
pub struct NativeProjectResolver {
    canonical_root: PathBuf,
    bytes_read: u64,
}

impl NativeProjectResolver {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, AssetImportError> {
        let root = root.as_ref();
        let canonical_root = fs::canonicalize(root).map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Io,
                ImportLocation::new(root.display().to_string(), "project_root"),
                error.to_string(),
            )
        })?;
        if !canonical_root.is_dir() {
            return Err(AssetImportError::new(
                AssetImportErrorCode::InvalidPath,
                ImportLocation::new(canonical_root.display().to_string(), "project_root"),
                "project root must be a directory",
            ));
        }
        Ok(Self {
            canonical_root,
            bytes_read: 0,
        })
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
}

impl SourceResolver for NativeProjectResolver {
    fn read_source(
        &mut self,
        source_id: &SourceId,
        max_bytes: u64,
    ) -> Result<SourceBlob, AssetImportError> {
        let relative = source_id
            .as_str()
            .split('/')
            .fold(PathBuf::new(), |mut path, part| {
                path.push(part);
                path
            });
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::InvalidPath,
                ImportLocation::new(source_id.to_string(), "uri"),
                "source ID contains a non-normal path component",
            ));
        }
        let candidate = self.canonical_root.join(relative);
        let canonical = candidate.canonicalize().map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Io,
                ImportLocation::new(source_id.to_string(), "source"),
                error.to_string(),
            )
        })?;
        if !canonical.starts_with(&self.canonical_root) || !canonical.is_file() {
            return Err(AssetImportError::new(
                AssetImportErrorCode::InvalidPath,
                ImportLocation::new(source_id.to_string(), "uri"),
                "resolved source is outside the project or is not a regular file",
            ));
        }
        let file = File::open(&canonical).map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Io,
                ImportLocation::new(source_id.to_string(), "source"),
                error.to_string(),
            )
        })?;
        let mut limited = file.take(max_bytes.saturating_add(1));
        let mut bytes = Vec::new();
        limited.read_to_end(&mut bytes).map_err(|error| {
            AssetImportError::new(
                AssetImportErrorCode::Io,
                ImportLocation::new(source_id.to_string(), "source"),
                error.to_string(),
            )
        })?;
        if bytes.len() as u64 > max_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::new(source_id.to_string(), "source"),
                format!("source exceeds the {max_bytes}-byte bounded read"),
            ));
        }
        self.bytes_read = self.bytes_read.saturating_add(bytes.len() as u64);
        Ok(SourceBlob::new(source_id.clone(), Arc::<[u8]>::from(bytes)))
    }
}
