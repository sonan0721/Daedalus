//! Bounded, validated CPU cook cache with exclusive native ownership.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::types::encode_hex;
use super::types::{
    AssetImportError, AssetImportErrorCode, CacheKey, CacheNamespace, ImportLocation,
};

const CACHE_VERSION: u16 = 1;
const ENVELOPE_MAGIC: &[u8; 8] = b"DAEDC001";
const ENVELOPE_HEADER_BYTES: u64 = 8 + 2 + 1 + 1 + 32 + 8 + 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Hard disk limits for this engine-owned cache instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheLimits {
    pub max_bytes: u64,
    pub max_entries: u64,
    pub max_entry_bytes: u64,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024 * 1024,
            max_entries: 1_024,
            max_entry_bytes: 128 * 1024 * 1024,
        }
    }
}

impl CacheLimits {
    pub fn validate(self) -> Result<(), AssetImportError> {
        let hard = Self::default();
        if self.max_bytes == 0
            || self.max_bytes > hard.max_bytes
            || self.max_entries == 0
            || self.max_entries > hard.max_entries
            || self.max_entry_bytes == 0
            || self.max_entry_bytes > hard.max_entry_bytes
            || self.max_entry_bytes > self.max_bytes
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::InvalidLimits,
                ImportLocation::default(),
                "cache limits must be positive and at or below hard ceilings",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheObjectKind {
    Import = 1,
    Cook = 2,
    PngBase = 3,
}

impl CacheObjectKind {
    fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Import),
            2 => Some(Self::Cook),
            3 => Some(Self::PngBase),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub current_bytes: u64,
    pub current_entries: u64,
    pub pinned_entries: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheCorruption {
    reason: String,
}

impl CacheCorruption {
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Debug)]
struct CacheEntryData {
    kind: CacheObjectKind,
    backing: Arc<[u8]>,
    payload_start: usize,
    payload_len: usize,
    sha256: [u8; 32],
}

impl CacheEntryData {
    fn new(kind: CacheObjectKind, payload: Arc<[u8]>, sha256: [u8; 32]) -> Self {
        let payload_len = payload.len();
        Self {
            kind,
            backing: payload,
            payload_start: 0,
            payload_len,
            sha256,
        }
    }
    fn payload(&self) -> &[u8] {
        &self.backing[self.payload_start..self.payload_start + self.payload_len]
    }
}

#[derive(Debug)]
struct PinTable {
    entries: Mutex<BTreeMap<(CacheNamespaceKey, [u8; 32]), usize>>,
    // Retaining this Arc keeps the single OS cache lock alive while any catalog pin exists.
    _owner: Option<Arc<CacheOwner>>,
}

impl Default for PinTable {
    fn default() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            _owner: None,
        }
    }
}

impl PinTable {
    fn with_owner(owner: Arc<CacheOwner>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            _owner: Some(owner),
        }
    }
}

#[derive(Debug)]
struct CacheOwner {
    _lock: File,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CacheNamespaceKey(u8);

impl From<CacheNamespace> for CacheNamespaceKey {
    fn from(value: CacheNamespace) -> Self {
        Self(match value {
            CacheNamespace::Import => 1,
            CacheNamespace::Cook => 2,
        })
    }
}

#[derive(Debug)]
struct CacheLeaseInner {
    entry: CacheEntryData,
    key: CacheKey,
    pins: Option<Arc<PinTable>>,
    // For file-backed hits this covers the bounded Vec -> Arc transition and remains live while
    // callers inspect the returned payload. Memory-cache hits do not allocate a backing copy.
    _read_scratch: Option<super::TemporaryBudget>,
}

impl Drop for CacheLeaseInner {
    fn drop(&mut self) {
        let Some(pins) = &self.pins else { return };
        if let Ok(mut entries) = pins.entries.lock() {
            let key = (
                CacheNamespaceKey::from(self.key.namespace()),
                self.key.digest(),
            );
            if let Some(count) = entries.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    entries.remove(&key);
                }
            }
        }
    }
}

/// A validated cache payload pinned against eviction for its lifetime.
#[derive(Clone, Debug)]
pub struct CacheLease(Arc<CacheLeaseInner>);

impl CacheLease {
    fn new(
        key: CacheKey,
        entry: CacheEntryData,
        pins: Option<Arc<PinTable>>,
        read_scratch: Option<super::TemporaryBudget>,
    ) -> Self {
        if let Some(pins) = &pins
            && let Ok(mut entries) = pins.entries.lock()
        {
            *entries
                .entry((CacheNamespaceKey::from(key.namespace()), key.digest()))
                .or_default() += 1;
        }
        Self(Arc::new(CacheLeaseInner {
            entry,
            key,
            pins,
            _read_scratch: read_scratch,
        }))
    }

    pub(crate) fn pin(&self) -> CachePin {
        CachePin::new(self.0.key, self.0.pins.clone())
    }

    pub fn key(&self) -> CacheKey {
        self.0.key
    }
    pub fn kind(&self) -> CacheObjectKind {
        self.0.entry.kind
    }
    pub fn payload(&self) -> &[u8] {
        self.0.entry.payload()
    }
    pub(crate) fn payload_storage(&self) -> (Arc<[u8]>, usize, usize) {
        (
            self.0.entry.backing.clone(),
            self.0.entry.payload_start,
            self.0.entry.payload_len,
        )
    }
    pub fn payload_sha256(&self) -> [u8; 32] {
        self.0.entry.sha256
    }
}

/// Small catalog-retained eviction pin; it does not keep the serialized cache payload resident.
#[derive(Clone, Debug)]
pub struct CachePin(Arc<CachePinInner>);

#[derive(Debug)]
struct CachePinInner {
    key: CacheKey,
    pins: Option<Arc<PinTable>>,
}

impl CachePin {
    fn new(key: CacheKey, pins: Option<Arc<PinTable>>) -> Self {
        if let Some(pins) = &pins
            && let Ok(mut entries) = pins.entries.lock()
        {
            *entries
                .entry((CacheNamespaceKey::from(key.namespace()), key.digest()))
                .or_default() += 1;
        }
        Self(Arc::new(CachePinInner { key, pins }))
    }

    pub(crate) fn key(&self) -> CacheKey {
        self.0.key
    }
}

impl Drop for CachePinInner {
    fn drop(&mut self) {
        let Some(pins) = &self.pins else { return };
        if let Ok(mut entries) = pins.entries.lock() {
            let key = (
                CacheNamespaceKey::from(self.key.namespace()),
                self.key.digest(),
            );
            if let Some(count) = entries.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    entries.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum CacheRead {
    Miss,
    Hit(CacheLease),
    Corrupt(CacheCorruption),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CachePublish {
    Written,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExistingPayload {
    AbsentOrCorrupt,
    Same,
    Different,
}

/// Outcome and small eviction pin from a successful cache publication.
#[derive(Clone, Debug)]
pub struct CachePublishResult {
    outcome: CachePublish,
    pin: CachePin,
}

impl CachePublishResult {
    pub fn outcome(&self) -> CachePublish {
        self.outcome
    }

    pub(crate) fn into_pin(self) -> CachePin {
        self.pin
    }
}

/// The content pipeline requires cache implementations to distinguish miss, hit, and corruption.
pub trait CpuCookCache {
    fn read(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
    ) -> Result<CacheRead, AssetImportError>;
    /// Reads with a caller-owned reservation made after the backend has checked the on-disk
    /// envelope length and before allocating its payload buffer. Custom caches should override
    /// this when reads allocate; the default is appropriate for an in-memory Arc clone.
    fn read_with_reservation(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
        _reserve: &mut dyn FnMut(u64) -> Result<super::TemporaryBudget, AssetImportError>,
    ) -> Result<CacheRead, AssetImportError> {
        self.read(key, expected_kind, max_payload_bytes)
    }
    fn publish(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
    ) -> Result<CachePublish, AssetImportError>;
    /// Publishes and returns a lightweight pin without retaining a payload lease. The fallback
    /// is for custom cache adapters; built-in adapters override it to avoid rereading the entry.
    fn publish_pinned(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
        reserve: &mut dyn FnMut(u64) -> Result<super::TemporaryBudget, AssetImportError>,
    ) -> Result<CachePublishResult, AssetImportError> {
        let outcome = self.publish(key, kind, payload.clone())?;
        match self.read_with_reservation(key, kind, payload.len() as u64, reserve)? {
            CacheRead::Hit(lease) => Ok(CachePublishResult {
                outcome,
                pin: lease.pin(),
            }),
            CacheRead::Miss => Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                "published cache entry was not readable",
            )),
            CacheRead::Corrupt(corruption) => Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                format!(
                    "published cache entry failed validation: {}",
                    corruption.reason()
                ),
            )),
        }
    }
    fn invalidate(&mut self, key: CacheKey, kind: CacheObjectKind) -> Result<(), AssetImportError>;
    fn stats(&self) -> CacheStats;
}

/// Simple in-memory cache for byte-source preparation and deterministic tests.
#[derive(Debug)]
pub struct MemoryCookCache {
    entries: BTreeMap<(CacheNamespaceKey, [u8; 32]), CacheEntryData>,
    pinned: Arc<PinTable>,
    max_bytes: u64,
    max_entries: u64,
    max_entry_bytes: u64,
}

impl MemoryCookCache {
    pub fn new(max_bytes: u64) -> Result<Self, AssetImportError> {
        let defaults = CacheLimits::default();
        Self::with_limits(CacheLimits {
            max_bytes,
            max_entries: defaults.max_entries,
            max_entry_bytes: defaults.max_entry_bytes.min(max_bytes),
        })
    }

    pub fn with_limits(limits: CacheLimits) -> Result<Self, AssetImportError> {
        limits.validate()?;
        Ok(Self {
            entries: BTreeMap::new(),
            pinned: Arc::new(PinTable::default()),
            max_bytes: limits.max_bytes,
            max_entries: limits.max_entries,
            max_entry_bytes: limits.max_entry_bytes,
        })
    }
}

impl Default for MemoryCookCache {
    fn default() -> Self {
        Self::with_limits(CacheLimits::default()).expect("default cache limits are valid")
    }
}

impl CpuCookCache for MemoryCookCache {
    fn read(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
    ) -> Result<CacheRead, AssetImportError> {
        let Some(entry) = self.entries.get(&(key.namespace().into(), key.digest())) else {
            return Ok(CacheRead::Miss);
        };
        let actual_hash: [u8; 32] = Sha256::digest(entry.payload()).into();
        if entry.kind != expected_kind
            || entry.payload_len as u64 > max_payload_bytes
            || actual_hash != entry.sha256
        {
            return Ok(CacheRead::Corrupt(CacheCorruption {
                reason: "in-memory entry failed kind, length, or digest validation".into(),
            }));
        }
        Ok(CacheRead::Hit(CacheLease::new(
            key,
            entry.clone(),
            Some(self.pinned.clone()),
            None,
        )))
    }

    fn publish(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
    ) -> Result<CachePublish, AssetImportError> {
        if payload.len() as u64 > self.max_entry_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "memory cache payload exceeds maximum entry size",
            ));
        }
        let digest: [u8; 32] = Sha256::digest(&payload).into();
        let map_key = (key.namespace().into(), key.digest());
        if let Some(previous) = self.entries.get(&map_key) {
            if previous.kind == kind
                && previous.sha256 == digest
                && previous.payload() == payload.as_ref()
            {
                return Ok(CachePublish::AlreadyPresent);
            }
            return Err(AssetImportError::new(
                AssetImportErrorCode::CacheConflict,
                ImportLocation::default(),
                "same cache key was published with conflicting content",
            ));
        }
        let current_bytes = self
            .entries
            .values()
            .map(|entry| entry.payload_len as u64)
            .sum::<u64>();
        if self.entries.len() as u64 >= self.max_entries
            || payload.len() as u64 > self.max_bytes
            || current_bytes.saturating_add(payload.len() as u64) > self.max_bytes
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                "memory cook cache capacity is full",
            ));
        }
        self.entries
            .insert(map_key, CacheEntryData::new(kind, payload, digest));
        Ok(CachePublish::Written)
    }

    fn invalidate(&mut self, key: CacheKey, kind: CacheObjectKind) -> Result<(), AssetImportError> {
        let map_key = (key.namespace().into(), key.digest());
        if let Some(entry) = self.entries.get(&map_key)
            && entry.kind != kind
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::CacheConflict,
                ImportLocation::default(),
                "cannot invalidate a cache entry with a different object kind",
            ));
        }
        if self
            .pinned
            .entries
            .lock()
            .map_err(|_| cache_error(Path::new("<memory>"), "cache pin table poisoned"))?
            .contains_key(&map_key)
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                "cannot invalidate a cache entry while a cache lease is active",
            ));
        }
        self.entries.remove(&map_key);
        Ok(())
    }

    fn stats(&self) -> CacheStats {
        let current_bytes = self
            .entries
            .values()
            .map(|entry| entry.payload_len as u64)
            .sum();
        let pinned_entries = self
            .pinned
            .entries
            .lock()
            .map_or(0, |entries| entries.len() as u64);
        CacheStats {
            current_bytes,
            current_entries: self.entries.len() as u64,
            pinned_entries,
        }
    }
}

/// Native disk cache. An exclusive nonblocking lock is held for the object's lifetime.
pub struct FileCookCache {
    root: PathBuf,
    limits: CacheLimits,
    pins: Arc<PinTable>,
}

impl std::fmt::Debug for FileCookCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileCookCache")
            .field("root", &self.root)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl FileCookCache {
    pub fn open(root: impl AsRef<Path>, limits: CacheLimits) -> Result<Self, AssetImportError> {
        limits.validate()?;
        let root = root.as_ref().join("v1");
        fs::create_dir_all(&root).map_err(|error| cache_error(&root, error.to_string()))?;
        validate_directory(&root)?;
        for name in ["import", "cook"] {
            let path = root.join(name);
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_directory(&path)?
                }
                Err(error) => return Err(cache_error(&path, error.to_string())),
            }
            validate_key_entries(&path)?;
        }
        let lock_path = root.join(".owner.lock");
        if let Ok(metadata) = fs::symlink_metadata(&lock_path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(cache_error(
                &lock_path,
                "cache owner lock must be a regular file",
            ));
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| cache_error(&lock_path, error.to_string()))?;
        lock.try_lock().map_err(|error| {
            cache_error(
                &lock_path,
                format!("cache directory is already owned or cannot be locked: {error}"),
            )
        })?;
        let owner = Arc::new(CacheOwner { _lock: lock });
        let cache = Self {
            root,
            limits,
            pins: Arc::new(PinTable::with_owner(owner)),
        };
        cache.scan_stats_checked()?;
        Ok(cache)
    }

    fn path_for(&self, key: CacheKey) -> PathBuf {
        let namespace = match key.namespace() {
            CacheNamespace::Import => "import",
            CacheNamespace::Cook => "cook",
        };
        self.root
            .join(namespace)
            .join(format!("{}.bin", encode_hex(&key.digest())))
    }

    fn read_file(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
        reserve: Option<&mut dyn FnMut(u64) -> Result<super::TemporaryBudget, AssetImportError>>,
    ) -> Result<CacheRead, AssetImportError> {
        let path = self.path_for(key);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(cache_error(
                &path,
                "cache entry must be a regular non-symlink file",
            ));
        }
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheRead::Miss);
            }
            Err(error) => return Err(cache_error(&path, error.to_string())),
        };
        let metadata_len = file
            .metadata()
            .map_err(|error| cache_error(&path, error.to_string()))?
            .len();
        let max_payload = max_payload_bytes.min(self.limits.max_entry_bytes);
        if metadata_len < ENVELOPE_HEADER_BYTES
            || metadata_len
                > self
                    .limits
                    .max_entry_bytes
                    .saturating_add(ENVELOPE_HEADER_BYTES)
            || metadata_len > max_payload.saturating_add(ENVELOPE_HEADER_BYTES)
        {
            return Ok(CacheRead::Corrupt(CacheCorruption {
                reason: "cache envelope length is outside the configured bound".into(),
            }));
        }

        // Validate schema, identity, and declared payload size using only a fixed stack header
        // before asking the context to reserve or allocating an attacker-controlled payload.
        let mut header = [0u8; ENVELOPE_HEADER_BYTES as usize];
        if let Err(error) = file.read_exact(&mut header) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(CacheRead::Corrupt(CacheCorruption {
                    reason: "cache envelope header was truncated".into(),
                }));
            }
            return Err(cache_error(&path, error.to_string()));
        }
        let (payload_len, payload_sha) =
            match decode_envelope_header(&header, key, expected_kind, max_payload, metadata_len) {
                Ok(decoded) => decoded,
                Err(reason) => return Ok(CacheRead::Corrupt(CacheCorruption { reason })),
            };

        let scratch_bytes = payload_len.checked_mul(2).ok_or_else(|| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::new(path.display().to_string(), "cache"),
                "cache read scratch size overflow",
            )
        })?;
        let read_scratch = reserve.map(|reserve| reserve(scratch_bytes)).transpose()?;
        let payload_capacity = usize::try_from(payload_len).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::new(path.display().to_string(), "cache"),
                "cache payload length is not representable on this target",
            )
        })?;
        let mut payload = Vec::new();
        payload.try_reserve_exact(payload_capacity).map_err(|_| {
            AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::new(path.display().to_string(), "cache"),
                "unable to allocate bounded cache payload",
            )
        })?;
        payload.resize(payload_capacity, 0);
        if let Err(error) = file.read_exact(&mut payload) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(CacheRead::Corrupt(CacheCorruption {
                    reason: "cache entry changed size during bounded read".into(),
                }));
            }
            return Err(cache_error(&path, error.to_string()));
        }
        let mut trailing_byte = [0u8; 1];
        if file
            .read(&mut trailing_byte)
            .map_err(|error| cache_error(&path, error.to_string()))?
            != 0
        {
            return Ok(CacheRead::Corrupt(CacheCorruption {
                reason: "cache entry changed size during bounded read".into(),
            }));
        }
        let actual_sha: [u8; 32] = Sha256::digest(&payload).into();
        if actual_sha != payload_sha {
            return Ok(CacheRead::Corrupt(CacheCorruption {
                reason: "cache payload digest mismatch".into(),
            }));
        }
        let entry = CacheEntryData::new(expected_kind, Arc::from(payload), actual_sha);
        Ok(CacheRead::Hit(CacheLease::new(
            key,
            entry,
            Some(self.pins.clone()),
            read_scratch,
        )))
    }

    fn scan_stats(&self) -> CacheStats {
        self.scan_stats_checked().unwrap_or_default()
    }

    fn scan_stats_checked(&self) -> Result<CacheStats, AssetImportError> {
        let mut bytes = 0u64;
        let mut entries = 0u64;
        for namespace in ["import", "cook"] {
            let directory = self.root.join(namespace);
            for item in fs::read_dir(&directory)
                .map_err(|error| cache_error(&directory, error.to_string()))?
            {
                let item = item.map_err(|error| cache_error(&directory, error.to_string()))?;
                let path = item.path();
                if !is_key_entry_name(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| cache_error(&path, error.to_string()))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(cache_error(
                        &path,
                        "cache entry must be a regular non-symlink file",
                    ));
                }
                entries = entries
                    .checked_add(1)
                    .ok_or_else(|| cache_error(&directory, "cache entry count overflow"))?;
                if entries > CacheLimits::default().max_entries {
                    return Err(cache_error(
                        &directory,
                        "cache contains more recognized entries than the hard entry limit",
                    ));
                }
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| cache_error(&directory, "cache byte count overflow"))?;
            }
        }
        let pinned_entries = self
            .pins
            .entries
            .lock()
            .map_err(|_| cache_error(&self.root, "cache pin table poisoned"))?
            .len() as u64;
        Ok(CacheStats {
            current_bytes: bytes,
            current_entries: entries,
            pinned_entries,
        })
    }

    fn compare_existing_payload(
        &self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: &[u8],
        payload_sha: [u8; 32],
    ) -> Result<ExistingPayload, AssetImportError> {
        let path = self.path_for(key);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(cache_error(
                &path,
                "cache entry must be a regular non-symlink file",
            ));
        }
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ExistingPayload::AbsentOrCorrupt);
            }
            Err(error) => return Err(cache_error(&path, error.to_string())),
        };
        let metadata_len = file
            .metadata()
            .map_err(|error| cache_error(&path, error.to_string()))?
            .len();
        if metadata_len < ENVELOPE_HEADER_BYTES
            || metadata_len
                > self
                    .limits
                    .max_entry_bytes
                    .saturating_add(ENVELOPE_HEADER_BYTES)
        {
            return Ok(ExistingPayload::AbsentOrCorrupt);
        }
        let mut header = [0u8; ENVELOPE_HEADER_BYTES as usize];
        if let Err(error) = file.read_exact(&mut header) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(ExistingPayload::AbsentOrCorrupt);
            }
            return Err(cache_error(&path, error.to_string()));
        }
        let (payload_len, expected_sha) = match decode_envelope_header(
            &header,
            key,
            kind,
            self.limits.max_entry_bytes,
            metadata_len,
        ) {
            Ok(header) => header,
            Err(_) => return Ok(ExistingPayload::AbsentOrCorrupt),
        };

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8 * 1024];
        let mut remaining = payload_len;
        let mut offset = 0usize;
        let mut equal = payload_len == payload.len() as u64;
        while remaining > 0 {
            let count = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("bounded cache chunk fits usize");
            if let Err(error) = file.read_exact(&mut buffer[..count]) {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(ExistingPayload::AbsentOrCorrupt);
                }
                return Err(cache_error(&path, error.to_string()));
            }
            hasher.update(&buffer[..count]);
            if equal && payload[offset..offset + count] != buffer[..count] {
                equal = false;
            }
            offset += count;
            remaining -= count as u64;
        }
        let mut trailing = [0u8; 1];
        if file
            .read(&mut trailing)
            .map_err(|error| cache_error(&path, error.to_string()))?
            != 0
        {
            return Ok(ExistingPayload::AbsentOrCorrupt);
        }
        let actual_sha: [u8; 32] = hasher.finalize().into();
        if actual_sha != expected_sha {
            return Ok(ExistingPayload::AbsentOrCorrupt);
        }
        if actual_sha == payload_sha && equal {
            Ok(ExistingPayload::Same)
        } else {
            Ok(ExistingPayload::Different)
        }
    }

    fn evict_for(
        &self,
        incoming_bytes: u64,
        incoming_entries: u64,
        protected_key: CacheKey,
    ) -> Result<(), AssetImportError> {
        let mut stats = self.scan_stats_checked()?;
        if stats.current_bytes.saturating_add(incoming_bytes) <= self.limits.max_bytes
            && stats.current_entries.saturating_add(incoming_entries) <= self.limits.max_entries
        {
            return Ok(());
        }
        let mut candidates = Vec::<(SystemTime, PathBuf, u64, CacheKey)>::new();
        for (namespace, folder) in [
            (CacheNamespace::Import, "import"),
            (CacheNamespace::Cook, "cook"),
        ] {
            for item in fs::read_dir(self.root.join(folder))
                .map_err(|error| cache_error(&self.root, error.to_string()))?
            {
                let item = item.map_err(|error| cache_error(&self.root, error.to_string()))?;
                let path = item.path();
                if !is_key_entry_name(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|error| cache_error(&path, error.to_string()))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(cache_error(
                        &path,
                        "cache entry must be a regular non-symlink file",
                    ));
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let Some(hex) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let Some(digest) = decode_digest(hex) else {
                    continue;
                };
                candidates.push((
                    modified,
                    path,
                    metadata.len(),
                    CacheKey::new(namespace, digest),
                ));
                if candidates.len() as u64 > CacheLimits::default().max_entries {
                    return Err(cache_error(
                        &self.root,
                        "cache eviction candidate count exceeds the hard entry limit",
                    ));
                }
            }
        }
        candidates.sort_by_key(|(modified, _, _, _)| *modified);
        let pins = self
            .pins
            .entries
            .lock()
            .map_err(|_| cache_error(&self.root, "cache pin table poisoned"))?;
        for (_, path, size, key) in candidates {
            if stats.current_bytes.saturating_add(incoming_bytes) <= self.limits.max_bytes
                && stats.current_entries.saturating_add(incoming_entries) <= self.limits.max_entries
            {
                break;
            }
            if key == protected_key || pins.contains_key(&(key.namespace().into(), key.digest())) {
                continue;
            }
            fs::remove_file(&path).map_err(|error| cache_error(&path, error.to_string()))?;
            stats.current_bytes = stats.current_bytes.saturating_sub(size);
            stats.current_entries = stats.current_entries.saturating_sub(1);
        }
        if stats.current_bytes.saturating_add(incoming_bytes) > self.limits.max_bytes
            || stats.current_entries.saturating_add(incoming_entries) > self.limits.max_entries
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                "cache capacity is pinned or insufficient; no entry was published",
            ));
        }
        Ok(())
    }
}

impl CpuCookCache for FileCookCache {
    fn read(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
    ) -> Result<CacheRead, AssetImportError> {
        self.read_file(key, expected_kind, max_payload_bytes, None)
    }

    fn read_with_reservation(
        &mut self,
        key: CacheKey,
        expected_kind: CacheObjectKind,
        max_payload_bytes: u64,
        reserve: &mut dyn FnMut(u64) -> Result<super::TemporaryBudget, AssetImportError>,
    ) -> Result<CacheRead, AssetImportError> {
        self.read_file(key, expected_kind, max_payload_bytes, Some(reserve))
    }

    fn publish(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
    ) -> Result<CachePublish, AssetImportError> {
        if payload.len() as u64 > self.limits.max_entry_bytes {
            return Err(AssetImportError::new(
                AssetImportErrorCode::LimitExceeded,
                ImportLocation::default(),
                "cache payload exceeds the maximum entry size",
            ));
        }
        let digest: [u8; 32] = Sha256::digest(&payload).into();
        match self.compare_existing_payload(key, kind, &payload, digest)? {
            ExistingPayload::Same => return Ok(CachePublish::AlreadyPresent),
            ExistingPayload::Different => {
                return Err(AssetImportError::new(
                    AssetImportErrorCode::CacheConflict,
                    ImportLocation::default(),
                    "same cache key was published with conflicting content",
                ));
            }
            ExistingPayload::AbsentOrCorrupt => {}
        }
        let envelope_header = encode_envelope_header(key, kind, payload.len() as u64, digest);
        let destination = self.path_for(key);
        let previous_bytes = match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(cache_error(
                    &destination,
                    "cache entry must be a regular non-symlink file",
                ));
            }
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(cache_error(&destination, error.to_string())),
        };
        let envelope_len = (envelope_header.len() as u64).saturating_add(payload.len() as u64);
        let incoming_delta = envelope_len.saturating_sub(previous_bytes);
        self.evict_for(incoming_delta, u64::from(previous_bytes == 0), key)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary =
            destination.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
        let write_result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&envelope_header)?;
            file.write_all(&payload)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &destination)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(cache_error(&destination, error.to_string()));
        }
        Ok(CachePublish::Written)
    }

    fn publish_pinned(
        &mut self,
        key: CacheKey,
        kind: CacheObjectKind,
        payload: Arc<[u8]>,
        _reserve: &mut dyn FnMut(u64) -> Result<super::TemporaryBudget, AssetImportError>,
    ) -> Result<CachePublishResult, AssetImportError> {
        let outcome = self.publish(key, kind, payload)?;
        Ok(CachePublishResult {
            outcome,
            pin: CachePin::new(key, Some(self.pins.clone())),
        })
    }

    fn invalidate(&mut self, key: CacheKey, kind: CacheObjectKind) -> Result<(), AssetImportError> {
        let map_key = (key.namespace().into(), key.digest());
        if self
            .pins
            .entries
            .lock()
            .map_err(|_| cache_error(&self.root, "cache pin table poisoned"))?
            .contains_key(&map_key)
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::Cache,
                ImportLocation::default(),
                "cannot invalidate a cache entry while a cache lease is active",
            ));
        }
        let path = self.path_for(key);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(cache_error(&path, error.to_string())),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(cache_error(
                &path,
                "cache entry must be a regular non-symlink file",
            ));
        }
        let mut file = File::open(&path).map_err(|error| cache_error(&path, error.to_string()))?;
        let envelope_len = file
            .metadata()
            .map_err(|error| cache_error(&path, error.to_string()))?
            .len();
        if envelope_len < ENVELOPE_HEADER_BYTES
            || envelope_len
                > self
                    .limits
                    .max_entry_bytes
                    .saturating_add(ENVELOPE_HEADER_BYTES)
        {
            fs::remove_file(&path).map_err(|error| cache_error(&path, error.to_string()))?;
            return Ok(());
        }
        let mut header = [0u8; ENVELOPE_HEADER_BYTES as usize];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                fs::remove_file(&path).map_err(|error| cache_error(&path, error.to_string()))?;
                return Ok(());
            }
            Err(error) => return Err(cache_error(&path, error.to_string())),
        }
        if let Ok((existing_kind, _, _)) =
            decode_envelope_header_identity(&header, key, self.limits.max_entry_bytes, envelope_len)
            && existing_kind != kind
        {
            return Err(AssetImportError::new(
                AssetImportErrorCode::CacheConflict,
                ImportLocation::new(path.display().to_string(), "cache"),
                "cannot invalidate a cache entry with a different object kind",
            ));
        }
        fs::remove_file(&path).map_err(|error| cache_error(&path, error.to_string()))
    }

    fn stats(&self) -> CacheStats {
        self.scan_stats()
    }
}

fn encode_envelope_header(
    key: CacheKey,
    kind: CacheObjectKind,
    payload_len: u64,
    payload_sha: [u8; 32],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ENVELOPE_HEADER_BYTES as usize);
    bytes.extend_from_slice(ENVELOPE_MAGIC);
    bytes.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    bytes.push(match key.namespace() {
        CacheNamespace::Import => 1,
        CacheNamespace::Cook => 2,
    });
    bytes.push(kind as u8);
    bytes.extend_from_slice(&key.digest());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&payload_sha);
    bytes
}

fn decode_envelope_header(
    bytes: &[u8; ENVELOPE_HEADER_BYTES as usize],
    key: CacheKey,
    expected_kind: CacheObjectKind,
    max_payload: u64,
    envelope_len: u64,
) -> Result<(u64, [u8; 32]), String> {
    let (kind, payload_len, payload_hash) =
        decode_envelope_header_identity(bytes, key, max_payload, envelope_len)?;
    if kind != expected_kind {
        return Err("cache object kind mismatch".into());
    }
    Ok((payload_len, payload_hash))
}

fn decode_envelope_header_identity(
    bytes: &[u8; ENVELOPE_HEADER_BYTES as usize],
    key: CacheKey,
    max_payload: u64,
    envelope_len: u64,
) -> Result<(CacheObjectKind, u64, [u8; 32]), String> {
    if &bytes[..8] != ENVELOPE_MAGIC {
        return Err("bad cache magic".into());
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != CACHE_VERSION {
        return Err("cache schema version mismatch".into());
    }
    let namespace = bytes[10];
    let kind = CacheObjectKind::from_byte(bytes[11])
        .ok_or_else(|| "unknown cache object kind".to_string())?;
    let expected_namespace = match key.namespace() {
        CacheNamespace::Import => 1,
        CacheNamespace::Cook => 2,
    };
    if namespace != expected_namespace || bytes[12..44] != key.digest() {
        return Err("cache key namespace or digest mismatch".into());
    }
    let payload_len = u64::from_le_bytes(
        bytes[44..52]
            .try_into()
            .map_err(|_| "truncated payload length".to_string())?,
    );
    if payload_len > max_payload
        || payload_len.checked_add(ENVELOPE_HEADER_BYTES) != Some(envelope_len)
    {
        return Err("cache payload length is out of bounds or inconsistent".into());
    }
    let expected_hash = bytes[52..84]
        .try_into()
        .map_err(|_| "truncated payload digest".to_string())?;
    Ok((kind, payload_len, expected_hash))
}

fn cache_error(path: &Path, message: impl Into<String>) -> AssetImportError {
    AssetImportError::new(
        AssetImportErrorCode::Cache,
        ImportLocation::new(path.display().to_string(), "cache"),
        message,
    )
}

fn is_key_entry_name(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "bin")
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| {
                stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

fn validate_directory(path: &Path) -> Result<(), AssetImportError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| cache_error(path, error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(cache_error(
            path,
            "cache namespace must be a regular non-symlink directory",
        ));
    }
    Ok(())
}

fn validate_key_entries(path: &Path) -> Result<(), AssetImportError> {
    let mut recognized = 0u64;
    for item in fs::read_dir(path).map_err(|error| cache_error(path, error.to_string()))? {
        let item = item.map_err(|error| cache_error(path, error.to_string()))?;
        let entry_path = item.path();
        if !is_key_entry_name(&entry_path) {
            continue;
        }
        recognized = recognized
            .checked_add(1)
            .ok_or_else(|| cache_error(path, "cache entry count overflow"))?;
        if recognized > CacheLimits::default().max_entries {
            return Err(cache_error(
                path,
                "cache namespace contains more recognized entries than the hard entry limit",
            ));
        }
        let metadata = fs::symlink_metadata(&entry_path)
            .map_err(|error| cache_error(&entry_path, error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(cache_error(
                &entry_path,
                "cache namespace key entry must be a regular non-symlink file",
            ));
        }
    }
    Ok(())
}

fn decode_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Some(digest)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    fn test_root(label: &str) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "daedalus-cache-{label}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn test_key(value: u8) -> CacheKey {
        let mut digest = [0; 32];
        digest[0] = value;
        CacheKey::new(CacheNamespace::Cook, digest)
    }

    #[test]
    fn memory_cache_invalidation_requires_drop_of_active_lease() {
        let mut cache = MemoryCookCache::default();
        let key = test_key(7);
        cache
            .publish(key, CacheObjectKind::Cook, Arc::from(&b"payload"[..]))
            .unwrap();
        let CacheRead::Hit(lease) = cache.read(key, CacheObjectKind::Cook, 32).unwrap() else {
            panic!("expected cache hit")
        };
        assert!(cache.invalidate(key, CacheObjectKind::Cook).is_err());
        drop(lease);
        cache.invalidate(key, CacheObjectKind::Cook).unwrap();
        assert!(matches!(
            cache.read(key, CacheObjectKind::Cook, 32).unwrap(),
            CacheRead::Miss
        ));
    }

    #[test]
    fn memory_cache_constructor_enforces_the_shared_hard_ceiling() {
        let over_hard = CacheLimits::default().max_bytes + 1;
        assert!(MemoryCookCache::new(over_hard).is_err());
        assert!(MemoryCookCache::new(CacheLimits::default().max_bytes).is_ok());
    }

    #[test]
    fn file_cache_reserves_exact_payload_transition_after_header_validation() {
        let root = test_root("read-scratch");
        let _ = fs::remove_dir_all(&root);
        let mut cache = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        let key = test_key(13);
        let payload: Arc<[u8]> = Arc::from(&b"payload"[..]);
        cache
            .publish(key, CacheObjectKind::Cook, payload.clone())
            .unwrap();

        let state = Arc::new(Mutex::new(super::super::BudgetState::default()));
        let mut reserved = 0;
        let mut reserve = |amount| {
            reserved = amount;
            super::super::reserve_shared_budget(
                state.clone(),
                super::super::BudgetKind::ParserScratchBytes,
                u64::MAX,
                amount,
                ImportLocation::default(),
            )
        };
        let CacheRead::Hit(lease) = cache
            .read_with_reservation(key, CacheObjectKind::Cook, 64, &mut reserve)
            .unwrap()
        else {
            panic!("expected native cache hit")
        };
        assert_eq!(reserved, payload.len() as u64 * 2);
        assert_eq!(lease.payload(), payload.as_ref());
        assert_eq!(
            state
                .lock()
                .unwrap()
                .0
                .get(&super::super::BudgetKind::ParserScratchBytes),
            Some(&reserved)
        );
        drop(lease);
        assert!(state.lock().unwrap().0.is_empty());

        let path = cache.path_for(key);
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&99u16.to_le_bytes()).unwrap();
        file.flush().unwrap();
        let mut reserve_called = false;
        let mut reserve = |_| {
            reserve_called = true;
            Err(AssetImportError::new(
                AssetImportErrorCode::Internal,
                ImportLocation::default(),
                "invalid header must be rejected before reservation",
            ))
        };
        assert!(matches!(
            cache
                .read_with_reservation(key, CacheObjectKind::Cook, 64, &mut reserve)
                .unwrap(),
            CacheRead::Corrupt(_)
        ));
        assert!(!reserve_called);
        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalidation_removes_unrecognized_or_malformed_headers() {
        let root = test_root("invalidate-malformed");
        let _ = fs::remove_dir_all(&root);
        let mut cache = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        let key = test_key(17);
        let payload = b"payload";
        let mut valid = encode_envelope_header(
            key,
            CacheObjectKind::Cook,
            payload.len() as u64,
            Sha256::digest(payload).into(),
        );
        valid.extend_from_slice(payload);

        let mut bad_magic = valid.clone();
        bad_magic[0] ^= 1;
        let mut bad_version = valid.clone();
        bad_version[8..10].copy_from_slice(&(CACHE_VERSION + 1).to_le_bytes());
        let mut bad_key = valid.clone();
        bad_key[12] ^= 1;
        let mut bad_namespace = valid.clone();
        bad_namespace[10] = 1;
        let mut unknown_kind = valid.clone();
        unknown_kind[11] = 99;

        for malformed in [
            b"corrupt offline smoke cache entry".to_vec(),
            valid[..12].to_vec(),
            valid[..ENVELOPE_HEADER_BYTES as usize].to_vec(),
            bad_magic,
            bad_version,
            bad_key,
            bad_namespace,
            unknown_kind,
        ] {
            let path = cache.path_for(key);
            fs::write(&path, malformed).unwrap();
            cache.invalidate(key, CacheObjectKind::Cook).unwrap();
            assert!(!path.exists(), "malformed cache entry should be removed");
        }

        cache
            .publish(key, CacheObjectKind::Cook, Arc::from(&b"rebuilt"[..]))
            .unwrap();
        let CacheRead::Hit(lease) = cache.read(key, CacheObjectKind::Cook, 32).unwrap() else {
            panic!("a malformed entry should be rebuildable after invalidation")
        };
        assert_eq!(lease.payload(), b"rebuilt");
        drop(lease);

        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalidation_preserves_valid_other_kind_and_active_pins() {
        let root = test_root("invalidate-kind-pin");
        let _ = fs::remove_dir_all(&root);
        let mut cache = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        let key = test_key(18);
        cache
            .publish(key, CacheObjectKind::Cook, Arc::from(&b"payload"[..]))
            .unwrap();
        let path = cache.path_for(key);

        let wrong_kind = cache.invalidate(key, CacheObjectKind::PngBase).unwrap_err();
        assert_eq!(wrong_kind.code(), AssetImportErrorCode::CacheConflict);
        assert!(
            path.exists(),
            "valid entry of another kind must be preserved"
        );

        let CacheRead::Hit(lease) = cache.read(key, CacheObjectKind::Cook, 32).unwrap() else {
            panic!("expected valid cache hit")
        };
        assert!(cache.invalidate(key, CacheObjectKind::Cook).is_err());
        drop(lease);
        cache.invalidate(key, CacheObjectKind::Cook).unwrap();
        assert!(!path.exists());

        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_pin_keeps_native_owner_lock_until_last_pin_drops() {
        let root = test_root("owner-pin");
        let _ = fs::remove_dir_all(&root);
        let key = test_key(9);
        let mut cache = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        cache
            .publish(key, CacheObjectKind::PngBase, Arc::from(&b"rgba"[..]))
            .unwrap();
        let CacheRead::Hit(lease) = cache.read(key, CacheObjectKind::PngBase, 32).unwrap() else {
            panic!("expected cache hit")
        };
        let catalog_pin = lease.pin();
        drop(lease);
        drop(cache);
        assert!(
            FileCookCache::open(&root, CacheLimits::default()).is_err(),
            "catalog pin must retain exclusive owner lock"
        );
        drop(catalog_pin);
        let mut reopened = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        assert!(matches!(
            reopened.read(key, CacheObjectKind::PngBase, 32).unwrap(),
            CacheRead::Hit(_)
        ));
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn png_base_objects_share_the_cook_directory_namespace() {
        let root = test_root("png-folder");
        let _ = fs::remove_dir_all(&root);
        let cache = FileCookCache::open(&root, CacheLimits::default()).unwrap();
        assert!(cache.root.join("cook").is_dir());
        assert!(!cache.root.join("png-base").exists());
        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }
}
