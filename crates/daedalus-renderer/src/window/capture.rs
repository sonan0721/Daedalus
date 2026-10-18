//! Bounded renderer-owned live-capture request and readback ownership.

use std::{
    num::NonZeroU64,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::CpuFrameHandle;
use daedalus_core::{SessionId, Tick};
use thiserror::Error;

use super::managed::{
    ManagedGenerationIdentity, ManagedPacketWindowClient, ManagedPacketWindowServer,
    ManagedPresentedView, ManagedStageKey, managed_packet_window_channel,
};

const MAP_NOT_REQUESTED: u8 = 0;
const MAP_PENDING: u8 = 1;
const MAP_SUCCEEDED: u8 = 2;
const MAP_FAILED: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturePhase {
    Vacant,
    Reserved,
    Installing,
    CopyMayBeSubmitted,
    MapPending,
    Submitted,
    CleanupRequired,
    Failed,
    Released,
}

/// Renderer-owned upper bound for the one live capture lane.
pub const MANAGED_CAPTURE_MAX_WIDTH: u32 = 8192;
/// Renderer-owned upper bound for the one live capture lane.
pub const MANAGED_CAPTURE_MAX_HEIGHT: u32 = 8192;
/// Renderer-owned upper bound for the one live capture lane.
pub const MANAGED_CAPTURE_MAX_PIXELS: u64 = 8_388_608;
/// Conservative raw readback ceiling for one live capture.
pub const MANAGED_CAPTURE_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Per-channel identity for one accepted capture. Keys must be nonzero and increase strictly.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ManagedCaptureKey(NonZeroU64);

impl ManagedCaptureKey {
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Exact source generation requested for a capture, available before its first presentation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedCaptureSource {
    stage_key: ManagedStageKey,
    session: SessionId,
    project_generation: u64,
    source_digest: String,
}

impl ManagedCaptureSource {
    pub fn new(
        stage_key: ManagedStageKey,
        session: SessionId,
        project_generation: u64,
        source_digest: &str,
    ) -> Result<Self, ManagedCaptureSourceError> {
        if project_generation == 0 || !crate::packet::valid_source_digest(source_digest) {
            return Err(ManagedCaptureSourceError::InvalidIdentity);
        }
        Ok(Self {
            stage_key,
            session,
            project_generation,
            source_digest: source_digest.to_owned(),
        })
    }

    pub fn from_generation(identity: &ManagedGenerationIdentity) -> Self {
        Self {
            stage_key: identity.key(),
            session: identity.session(),
            project_generation: identity.project_generation(),
            source_digest: identity.source_digest().to_owned(),
        }
    }

    pub fn from_presented(view: &ManagedPresentedView) -> Self {
        Self {
            stage_key: view.key(),
            session: view.session(),
            project_generation: view.project_generation(),
            source_digest: view.source_digest().to_owned(),
        }
    }

    pub fn stage_key(&self) -> ManagedStageKey {
        self.stage_key
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub(super) fn matches_view(&self, view: &ManagedPresentedView) -> bool {
        self.stage_key == view.key()
            && self.session == view.session()
            && self.project_generation == view.project_generation()
            && self.source_digest == view.source_digest()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureSourceError {
    #[error("capture source identity is invalid")]
    InvalidIdentity,
}

/// One request for the first eligible actual frame before its deadline.
pub struct ManagedCaptureRequest {
    key: ManagedCaptureKey,
    source: ManagedCaptureSource,
    at_least_tick: Tick,
    deadline: Instant,
}

impl std::fmt::Debug for ManagedCaptureRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedCaptureRequest")
            .field("key", &self.key)
            .field("source", &self.source)
            .field("at_least_tick", &self.at_least_tick)
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl ManagedCaptureRequest {
    pub fn new(
        key: ManagedCaptureKey,
        source: ManagedCaptureSource,
        at_least_tick: Tick,
        deadline: Instant,
    ) -> Result<Box<Self>, ManagedCaptureRequestError> {
        if deadline <= Instant::now() {
            return Err(ManagedCaptureRequestError::DeadlineElapsed);
        }
        Ok(Box::new(Self {
            key,
            source,
            at_least_tick,
            deadline,
        }))
    }

    pub fn key(&self) -> ManagedCaptureKey {
        self.key
    }

    pub fn source(&self) -> &ManagedCaptureSource {
        &self.source
    }

    pub fn at_least_tick(&self) -> Tick {
        self.at_least_tick
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureRequestError {
    #[error("capture deadline has elapsed")]
    DeadlineElapsed,
}

/// Supported native surface storage formats for the bounded readback path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCaptureSurfaceFormat {
    Rgba8Unorm,
    Rgba8UnormSrgb,
    Bgra8Unorm,
    Bgra8UnormSrgb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManagedCaptureBufferLayout {
    pub(crate) row_bytes: usize,
    pub(crate) bytes_per_row: usize,
    pub(crate) bytes_per_row_u32: u32,
    pub(crate) total_bytes: usize,
}

pub(crate) fn checked_capture_buffer_layout(
    width: u32,
    height: u32,
    ready: ManagedCaptureReadyInfo,
    format: ManagedCaptureSurfaceFormat,
    maximum_buffer_size: u64,
) -> Result<ManagedCaptureBufferLayout, ManagedCaptureFailure> {
    if width == 0
        || height == 0
        || width > ready.max_width()
        || height > ready.max_height()
        || format != ready.surface_format()
    {
        return Err(ManagedCaptureFailure::TooLarge);
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(ManagedCaptureFailure::TooLarge)?;
    if pixels > ready.max_pixels() || pixels > MANAGED_CAPTURE_MAX_PIXELS {
        return Err(ManagedCaptureFailure::TooLarge);
    }
    let width = usize::try_from(width).map_err(|_| ManagedCaptureFailure::TooLarge)?;
    let height = usize::try_from(height).map_err(|_| ManagedCaptureFailure::TooLarge)?;
    let row_bytes = width
        .checked_mul(4)
        .ok_or(ManagedCaptureFailure::TooLarge)?;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let bytes_per_row = row_bytes
        .checked_add(alignment - 1)
        .map(|bytes| bytes & !(alignment - 1))
        .ok_or(ManagedCaptureFailure::TooLarge)?;
    let bytes_per_row_u32 =
        u32::try_from(bytes_per_row).map_err(|_| ManagedCaptureFailure::TooLarge)?;
    let total_bytes = bytes_per_row
        .checked_mul(height)
        .ok_or(ManagedCaptureFailure::TooLarge)?;
    let total_bytes_u64 =
        u64::try_from(total_bytes).map_err(|_| ManagedCaptureFailure::TooLarge)?;
    if total_bytes_u64 > MANAGED_CAPTURE_MAX_BYTES || total_bytes_u64 > maximum_buffer_size {
        return Err(ManagedCaptureFailure::TooLarge);
    }
    Ok(ManagedCaptureBufferLayout {
        row_bytes,
        bytes_per_row,
        bytes_per_row_u32,
        total_bytes,
    })
}

/// One-time readiness limits discovered from the configured surface and device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedCaptureReadyInfo {
    surface_format: ManagedCaptureSurfaceFormat,
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
}

impl ManagedCaptureReadyInfo {
    pub fn new(
        surface_format: ManagedCaptureSurfaceFormat,
        max_width: u32,
        max_height: u32,
        max_pixels: u64,
    ) -> Result<Self, ManagedCaptureReadyError> {
        let pixels = u64::from(max_width)
            .checked_mul(u64::from(max_height))
            .ok_or(ManagedCaptureReadyError::InvalidLimits)?;
        if max_width == 0
            || max_height == 0
            || max_pixels == 0
            || max_width > MANAGED_CAPTURE_MAX_WIDTH
            || max_height > MANAGED_CAPTURE_MAX_HEIGHT
            || max_pixels > MANAGED_CAPTURE_MAX_PIXELS
            || max_pixels > pixels
        {
            return Err(ManagedCaptureReadyError::InvalidLimits);
        }
        Ok(Self {
            surface_format,
            max_width,
            max_height,
            max_pixels,
        })
    }

    pub fn surface_format(self) -> ManagedCaptureSurfaceFormat {
        self.surface_format
    }

    pub fn max_width(self) -> u32 {
        self.max_width
    }

    pub fn max_height(self) -> u32 {
        self.max_height
    }

    pub fn max_pixels(self) -> u64 {
        self.max_pixels
    }
}

pub(super) fn capture_format_for_wgpu(
    format: wgpu::TextureFormat,
) -> Option<ManagedCaptureSurfaceFormat> {
    match format {
        wgpu::TextureFormat::Rgba8Unorm => Some(ManagedCaptureSurfaceFormat::Rgba8Unorm),
        wgpu::TextureFormat::Rgba8UnormSrgb => Some(ManagedCaptureSurfaceFormat::Rgba8UnormSrgb),
        wgpu::TextureFormat::Bgra8Unorm => Some(ManagedCaptureSurfaceFormat::Bgra8Unorm),
        wgpu::TextureFormat::Bgra8UnormSrgb => Some(ManagedCaptureSurfaceFormat::Bgra8UnormSrgb),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureReadyError {
    #[error("capture readiness was already resolved")]
    AlreadyResolved,
    #[error("capture readiness limits are invalid")]
    InvalidLimits,
    #[error("capture endpoint is detached")]
    Detached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCaptureUnsupportedReason {
    SurfaceCopyUnavailable,
    UnsupportedSurfaceFormat,
    CaptureDisabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCaptureState {
    Starting,
    Ready(ManagedCaptureReadyInfo),
    Unsupported(ManagedCaptureUnsupportedReason),
    Detached,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureEndpointError {
    #[error("capture endpoint is busy")]
    Busy,
    #[error("capture endpoint is detached")]
    Detached,
    #[error("capture endpoint state was poisoned")]
    Poisoned,
    #[error("the durable capture worker has already been registered")]
    AlreadyRegistered,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureSubmitError {
    #[error("capture request slot is occupied")]
    Busy,
    #[error("capture surface is not ready")]
    NotReady,
    #[error("capture is unsupported")]
    Unsupported,
    #[error("capture endpoint is detached")]
    Detached,
    #[error("capture request identity or deadline is invalid")]
    InvalidRequest,
    #[error("capture key was already used or is not increasing")]
    KeyReused,
    #[error("capture cell state was poisoned")]
    Poisoned,
    #[error("durable capture worker was not registered")]
    WorkerUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCaptureCancelStatus {
    Accepted,
    AlreadySubmitted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureCancelError {
    #[error("capture cell is busy")]
    Busy,
    #[error("capture endpoint is detached")]
    Detached,
    #[error("capture cell state was poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureFailure {
    #[error("capture was cancelled")]
    Cancelled,
    #[error("capture deadline elapsed")]
    DeadlineElapsed,
    #[error("selected frame did not match the requested source")]
    StaleSource,
    #[error("capture surface is unsupported")]
    Unsupported,
    #[error("capture dimensions exceed the configured limit")]
    TooLarge,
    #[error("surface copy submission failed")]
    CopyFailed,
    #[error("readback mapping failed")]
    MapFailed,
    #[error("device polling failed")]
    DevicePollFailed,
    #[error("capture submission counter was exhausted")]
    CounterExhausted,
    #[error("capture readback allocation failed")]
    AllocationFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedCaptureCellStatus {
    Reserved,
    Installing,
    CopyMayBeSubmitted,
    MapPending,
    Submitted,
    CleanupRequired,
    QueuePending,
    Released,
}

/// A single owner for the one bounded capture request and its durable readback cell.
pub struct ManagedCaptureClient {
    shared: Arc<ManagedCaptureShared>,
}

/// The registered non-cloneable CPU-side observer and final-buffer owner.
pub struct ManagedCaptureWorker {
    shared: Arc<ManagedCaptureShared>,
    detached: bool,
}

impl ManagedCaptureClient {
    fn new() -> (Self, Arc<ManagedCaptureShared>) {
        let shared = Arc::new(ManagedCaptureShared::new());
        (
            Self {
                shared: Arc::clone(&shared),
            },
            shared,
        )
    }

    pub fn register_worker(&self) -> Result<ManagedCaptureWorker, ManagedCaptureEndpointError> {
        if self.shared.window_started.load(Ordering::Acquire)
            || self.shared.detached.load(Ordering::Acquire)
        {
            return Err(ManagedCaptureEndpointError::Detached);
        }
        self.shared
            .worker_registered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ManagedCaptureEndpointError::AlreadyRegistered)?;
        if self.shared.window_started.load(Ordering::Acquire) {
            self.shared
                .worker_registered
                .store(false, Ordering::Release);
            return Err(ManagedCaptureEndpointError::Detached);
        }
        self.shared.worker_alive.store(true, Ordering::Release);
        self.shared
            .cell
            .worker_detached
            .store(false, Ordering::Release);
        Ok(ManagedCaptureWorker {
            shared: Arc::clone(&self.shared),
            detached: false,
        })
    }

    pub fn try_status(&self) -> Result<ManagedCaptureState, ManagedCaptureEndpointError> {
        if self.shared.detached.load(Ordering::Acquire) {
            return Ok(ManagedCaptureState::Detached);
        }
        Ok(self
            .shared
            .readiness
            .get()
            .cloned()
            .unwrap_or(ManagedCaptureState::Starting))
    }

    pub fn try_submit(
        &self,
        request: Box<ManagedCaptureRequest>,
    ) -> Result<(), (Box<ManagedCaptureRequest>, ManagedCaptureSubmitError)> {
        let reject = |request, error| Err((request, error));
        if self.shared.detached.load(Ordering::Acquire) {
            return reject(request, ManagedCaptureSubmitError::Detached);
        }
        if !self.shared.worker_alive.load(Ordering::Acquire) {
            return reject(request, ManagedCaptureSubmitError::WorkerUnavailable);
        }
        match self.try_status() {
            Ok(ManagedCaptureState::Ready(_)) => {}
            Ok(ManagedCaptureState::Starting) => {
                return reject(request, ManagedCaptureSubmitError::NotReady);
            }
            Ok(ManagedCaptureState::Unsupported(_)) => {
                return reject(request, ManagedCaptureSubmitError::Unsupported);
            }
            Ok(ManagedCaptureState::Detached) | Err(ManagedCaptureEndpointError::Detached) => {
                return reject(request, ManagedCaptureSubmitError::Detached);
            }
            Err(ManagedCaptureEndpointError::Busy | ManagedCaptureEndpointError::Poisoned) => {
                return reject(request, ManagedCaptureSubmitError::Busy);
            }
            Err(ManagedCaptureEndpointError::AlreadyRegistered) => {
                return reject(request, ManagedCaptureSubmitError::InvalidRequest);
            }
        }
        if request.deadline <= Instant::now()
            || request.source.project_generation == 0
            || !crate::packet::valid_source_digest(&request.source.source_digest)
        {
            return reject(request, ManagedCaptureSubmitError::InvalidRequest);
        }

        let mut request_slot = match self.shared.request.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => {
                return reject(request, ManagedCaptureSubmitError::Busy);
            }
            Err(TryLockError::Poisoned(error)) => {
                return reject(request, {
                    drop(error.into_inner());
                    ManagedCaptureSubmitError::Poisoned
                });
            }
        };
        if request_slot.is_some() {
            return reject(request, ManagedCaptureSubmitError::Busy);
        }
        if self.shared.detached.load(Ordering::Acquire) {
            return reject(request, ManagedCaptureSubmitError::Detached);
        }
        if !self.shared.worker_alive.load(Ordering::Acquire) {
            return reject(request, ManagedCaptureSubmitError::WorkerUnavailable);
        }
        let last_key = self.shared.last_key.load(Ordering::Acquire);
        if request.key.get() <= last_key {
            return reject(request, ManagedCaptureSubmitError::KeyReused);
        }
        if self.shared.cell.reserve(request.key).is_err() {
            return reject(request, ManagedCaptureSubmitError::Busy);
        }
        *request_slot = Some(request);
        let accepted_key = request_slot.as_ref().map_or(0, |request| request.key.get());
        self.shared.last_key.store(accepted_key, Ordering::Release);
        Ok(())
    }

    pub fn request_cancel(
        &self,
        key: ManagedCaptureKey,
    ) -> Result<ManagedCaptureCancelStatus, ManagedCaptureCancelError> {
        self.shared.cancel(key, true)
    }

    pub fn request_detach(&self) {
        self.shared.detached.store(true, Ordering::Release);
        let _ = self.shared.abandon_unclaimed_request();
        let key = self.shared.cell.key.load(Ordering::Acquire);
        if key != 0 {
            self.shared.cell.cancel_key.fetch_max(key, Ordering::AcqRel);
            self.shared
                .cell
                .abandon_key
                .fetch_max(key, Ordering::AcqRel);
        }
    }
}

/// Creates a managed window channel with a separate, bounded live-capture lane.
/// The returned worker must be registered before the window server starts.
pub fn managed_packet_window_capture_channel() -> (
    ManagedPacketWindowClient,
    ManagedCaptureClient,
    ManagedPacketWindowServer,
) {
    let (window, mut server) = managed_packet_window_channel();
    let (capture, shared) = ManagedCaptureClient::new();
    server.capture = Some(shared);
    (window, capture, server)
}

impl ManagedCaptureWorker {
    /// Cancels a request after ownership has moved to the durable worker.
    pub fn request_cancel(
        &self,
        key: ManagedCaptureKey,
    ) -> Result<ManagedCaptureCancelStatus, ManagedCaptureCancelError> {
        self.shared.cancel(key, false)
    }

    /// Signals that no worker will consume this lane. GPU cleanup still requires a drain owner.
    pub fn request_detach(&self) {
        self.shared.worker_alive.store(false, Ordering::Release);
        self.shared
            .cell
            .worker_detached
            .store(true, Ordering::Release);
        let key = self.shared.cell.key.load(Ordering::Acquire);
        if key != 0 {
            self.shared.cell.cancel_key.fetch_max(key, Ordering::AcqRel);
            self.shared
                .cell
                .abandon_key
                .fetch_max(key, Ordering::AcqRel);
        }
    }

    pub fn try_take_return(
        &self,
    ) -> Result<Option<ManagedCaptureReturn>, ManagedCaptureWorkerError> {
        self.shared
            .abandon_unclaimed_request()
            .map_err(|error| match error {
                ManagedCaptureEndpointError::Busy => ManagedCaptureWorkerError::Busy,
                ManagedCaptureEndpointError::Poisoned => ManagedCaptureWorkerError::Poisoned,
                ManagedCaptureEndpointError::Detached
                | ManagedCaptureEndpointError::AlreadyRegistered => {
                    ManagedCaptureWorkerError::InvalidState
                }
            })?;
        let mut inner = self.shared.cell.try_lock().map_err(|error| match error {
            CellLockError::Busy => ManagedCaptureWorkerError::Busy,
            CellLockError::WrongKey => ManagedCaptureWorkerError::WrongKey,
            CellLockError::InvalidState => ManagedCaptureWorkerError::InvalidState,
        })?;
        if inner.return_taken {
            return Ok(None);
        }
        let Some(key) = inner.key else {
            return Ok(None);
        };
        if self.shared.cell.abandon_key.load(Ordering::Acquire) == key.get()
            && inner.phase != CapturePhase::Released
        {
            inner.phase = if inner.copy_possible {
                CapturePhase::CleanupRequired
            } else {
                inner
                    .failure
                    .get_or_insert(ManagedCaptureFailure::Cancelled);
                CapturePhase::Failed
            };
        }
        if self.shared.window_closed.load(Ordering::Acquire)
            && !matches!(
                inner.phase,
                CapturePhase::Submitted | CapturePhase::Released
            )
        {
            if inner.copy_possible {
                inner
                    .failure
                    .get_or_insert(ManagedCaptureFailure::CopyFailed);
                inner.phase = CapturePhase::CleanupRequired;
                self.shared
                    .cell
                    .abandon_key
                    .fetch_max(key.get(), Ordering::AcqRel);
            } else {
                inner
                    .failure
                    .get_or_insert(ManagedCaptureFailure::Cancelled);
                inner.phase = CapturePhase::Failed;
                self.shared
                    .cell
                    .abandon_key
                    .fetch_max(key.get(), Ordering::AcqRel);
            }
        }
        let result = match inner.phase {
            CapturePhase::Submitted => {
                let (Some(presented), Some(surface_format)) =
                    (inner.presented.clone(), inner.format)
                else {
                    return Err(ManagedCaptureWorkerError::InvalidState);
                };
                let selected_frame = inner
                    .selected_frame
                    .as_ref()
                    .map(Arc::downgrade)
                    .ok_or(ManagedCaptureWorkerError::InvalidState)?;
                ManagedCaptureReturn::Submitted(ManagedCaptureReadback::new(
                    Arc::clone(&self.shared.cell),
                    key,
                    presented,
                    surface_format,
                    selected_frame,
                ))
            }
            CapturePhase::CleanupRequired => ManagedCaptureReturn::Cleanup(
                ManagedCaptureCleanup::new(Arc::clone(&self.shared.cell), key),
            ),
            CapturePhase::Failed => ManagedCaptureReturn::Failed {
                key,
                failure: inner.failure.unwrap_or(ManagedCaptureFailure::CopyFailed),
                cleanup: ManagedCaptureCleanup::new(Arc::clone(&self.shared.cell), key),
            },
            _ => return Ok(None),
        };
        inner.return_taken = true;
        self.shared
            .cell
            .consumer_handle_key
            .store(key.get(), Ordering::Release);
        Ok(Some(result))
    }

    pub fn try_status(
        &self,
        key: ManagedCaptureKey,
    ) -> Result<ManagedCaptureCellStatus, ManagedCaptureWorkerError> {
        let inner = self.shared.cell.try_lock().map_err(|error| match error {
            CellLockError::Busy => ManagedCaptureWorkerError::Busy,
            CellLockError::WrongKey => ManagedCaptureWorkerError::WrongKey,
            CellLockError::InvalidState => ManagedCaptureWorkerError::InvalidState,
        })?;
        if inner.key != Some(key) {
            return Err(ManagedCaptureWorkerError::WrongKey);
        }
        Ok(self.shared.cell.public_status(&inner, key))
    }
}

impl Drop for ManagedCaptureWorker {
    fn drop(&mut self) {
        if !self.detached {
            self.request_detach();
            self.detached = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureWorkerError {
    #[error("capture cell is busy")]
    Busy,
    #[error("capture worker is detached")]
    Detached,
    #[error("capture cell state was poisoned")]
    Poisoned,
    #[error("capture key does not name the active cell")]
    WrongKey,
    #[error("capture cell is in an invalid state")]
    InvalidState,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureReadbackError {
    #[error("capture cell is busy")]
    Busy,
    #[error("capture was cancelled")]
    Cancelled,
    #[error("capture deadline elapsed")]
    DeadlineElapsed,
    #[error("selected frame did not match requested source")]
    StaleSource,
    #[error("capture dimensions exceed configured limits")]
    TooLarge,
    #[error("capture surface format is unsupported")]
    UnsupportedFormat,
    #[error("surface copy submission failed")]
    CopyFailed,
    #[error("readback mapping failed")]
    MapFailed,
    #[error("device polling failed")]
    DevicePollFailed,
    #[error("capture submission counter was exhausted")]
    CounterExhausted,
    #[error("capture readback allocation failed")]
    AllocationFailed,
    #[error("mapped readback range is invalid")]
    InvalidMappedRange,
    #[error("capture readback has already completed")]
    InvalidState,
    #[error("capture cell state was poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ManagedCaptureCleanupError {
    #[error("capture cell is busy")]
    Busy,
    #[error("device polling failed")]
    DevicePollFailed,
    #[error("capture cell state was poisoned")]
    Poisoned,
    #[error("capture cleanup is in an invalid state")]
    InvalidState,
}

pub enum ManagedCaptureReturn {
    Submitted(ManagedCaptureReadback),
    Cleanup(ManagedCaptureCleanup),
    Failed {
        key: ManagedCaptureKey,
        failure: ManagedCaptureFailure,
        cleanup: ManagedCaptureCleanup,
    },
}

impl std::fmt::Debug for ManagedCaptureReturn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submitted(readback) => {
                formatter.debug_tuple("Submitted").field(readback).finish()
            }
            Self::Cleanup(cleanup) => formatter.debug_tuple("Cleanup").field(cleanup).finish(),
            Self::Failed { key, failure, .. } => formatter
                .debug_struct("Failed")
                .field("key", key)
                .field("failure", failure)
                .finish(),
        }
    }
}

pub struct ManagedCaptureReadback {
    cell: Arc<ManagedCaptureCell>,
    key: ManagedCaptureKey,
    presented: Box<ManagedPresentedView>,
    surface_format: ManagedCaptureSurfaceFormat,
    selected_frame: Weak<CpuFrameHandle>,
    finished: bool,
}

impl ManagedCaptureReadback {
    fn new(
        cell: Arc<ManagedCaptureCell>,
        key: ManagedCaptureKey,
        presented: ManagedPresentedView,
        surface_format: ManagedCaptureSurfaceFormat,
        selected_frame: Weak<CpuFrameHandle>,
    ) -> Self {
        Self {
            cell,
            key,
            presented: Box::new(presented),
            surface_format,
            selected_frame,
            finished: false,
        }
    }

    pub fn key(&self) -> ManagedCaptureKey {
        self.key
    }

    pub fn presented(&self) -> &ManagedPresentedView {
        &self.presented
    }

    pub fn surface_format(&self) -> ManagedCaptureSurfaceFormat {
        self.surface_format
    }

    /// Weak handle for the exact CPU frame rendered into this readback.
    /// The capture cell retains the strong pin until worker cleanup completes.
    pub fn selected_frame(&self) -> &Weak<CpuFrameHandle> {
        &self.selected_frame
    }

    pub fn try_read_rgba8(
        &mut self,
    ) -> Result<Option<ManagedCapturePixels>, ManagedCaptureReadbackError> {
        let mut inner = self.cell.try_lock().map_err(|error| match error {
            CellLockError::Busy => ManagedCaptureReadbackError::Busy,
            CellLockError::WrongKey | CellLockError::InvalidState => {
                ManagedCaptureReadbackError::InvalidState
            }
        })?;
        if inner.key != Some(self.key) || self.finished {
            return Err(ManagedCaptureReadbackError::InvalidState);
        }
        if let Some(failure) = inner.failure {
            inner.phase = CapturePhase::CleanupRequired;
            return Err(map_readback_failure(failure));
        }
        if inner.phase != CapturePhase::Submitted {
            return if inner.phase == CapturePhase::CleanupRequired
                || inner.phase == CapturePhase::Failed
            {
                Err(ManagedCaptureReadbackError::InvalidState)
            } else {
                Ok(None)
            };
        }
        if self.cell.map_failure_key.load(Ordering::Acquire) == self.key.get() {
            inner.failure = Some(ManagedCaptureFailure::MapFailed);
            inner.phase = CapturePhase::CleanupRequired;
            return Err(ManagedCaptureReadbackError::MapFailed);
        }
        if self.cell.map_success_key.load(Ordering::Acquire) < self.key.get()
            || self.cell.queue_complete_key.load(Ordering::Acquire) < self.key.get()
        {
            return Ok(None);
        }
        let width = inner.width;
        let height = inner.height;
        let bytes_per_row = inner.bytes_per_row;
        let total_bytes = inner.total_bytes;
        let format = self.surface_format;
        let output_len = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(ManagedCaptureReadbackError::TooLarge)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(output_len)
            .map_err(|_| ManagedCaptureReadbackError::TooLarge)?;
        rgba.resize(output_len, 0);
        let buffer = inner
            .buffer
            .as_ref()
            .ok_or(ManagedCaptureReadbackError::InvalidState)?;
        if bytes_per_row
            .checked_mul(height as usize)
            .is_none_or(|required| total_bytes < required)
        {
            inner.failure = Some(ManagedCaptureFailure::MapFailed);
            inner.phase = CapturePhase::CleanupRequired;
            return Err(ManagedCaptureReadbackError::InvalidMappedRange);
        }
        {
            let view = buffer.slice(..).get_mapped_range();
            if view.len() < total_bytes {
                drop(view);
                inner.failure = Some(ManagedCaptureFailure::MapFailed);
                inner.phase = CapturePhase::CleanupRequired;
                return Err(ManagedCaptureReadbackError::InvalidMappedRange);
            }
            let copy_result = copy_surface_rows_to_rgba8(
                &view[..total_bytes],
                &mut rgba,
                width,
                height,
                bytes_per_row,
                format,
            );
            drop(view);
            if let Err(error) = copy_result {
                inner.failure = Some(ManagedCaptureFailure::MapFailed);
                inner.phase = CapturePhase::CleanupRequired;
                return Err(error);
            }
        }
        let Some(buffer) = inner.buffer.take() else {
            return Err(ManagedCaptureReadbackError::InvalidState);
        };
        buffer.unmap();
        drop(buffer);
        drop(inner.selected_frame.take());
        inner.phase = CapturePhase::Released;
        self.cell.clear_consumer_handle(self.key);
        self.finished = true;
        Ok(Some(ManagedCapturePixels {
            key: self.key,
            presented: (*self.presented).clone(),
            surface_format: format,
            rgba8: rgba,
        }))
    }

    pub fn into_cleanup(mut self) -> ManagedCaptureCleanup {
        if self.finished {
            return ManagedCaptureCleanup::finished(Arc::clone(&self.cell), self.key);
        }
        if let Ok(mut inner) = self.cell.try_lock()
            && inner.key == Some(self.key)
            && inner.phase != CapturePhase::Released
        {
            inner.phase = CapturePhase::CleanupRequired;
        }
        self.cell
            .abandon_key
            .fetch_max(self.key.get(), Ordering::AcqRel);
        self.finished = true;
        ManagedCaptureCleanup::new(Arc::clone(&self.cell), self.key)
    }
}

impl Drop for ManagedCaptureReadback {
    fn drop(&mut self) {
        if !self.finished {
            self.cell
                .abandon_key
                .fetch_max(self.key.get(), Ordering::AcqRel);
            if let Ok(mut inner) = self.cell.try_lock()
                && inner.key == Some(self.key)
                && inner.phase != CapturePhase::Released
            {
                inner.phase = CapturePhase::CleanupRequired;
                inner.return_taken = false;
            }
            self.cell.clear_consumer_handle(self.key);
        }
    }
}

impl std::fmt::Debug for ManagedCaptureReadback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedCaptureReadback")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

pub struct ManagedCaptureCleanup {
    cell: Arc<ManagedCaptureCell>,
    key: ManagedCaptureKey,
    finished: bool,
}

impl ManagedCaptureCleanup {
    fn new(cell: Arc<ManagedCaptureCell>, key: ManagedCaptureKey) -> Self {
        Self {
            cell,
            key,
            finished: false,
        }
    }

    fn finished(cell: Arc<ManagedCaptureCell>, key: ManagedCaptureKey) -> Self {
        Self {
            cell,
            key,
            finished: true,
        }
    }

    pub fn key(&self) -> ManagedCaptureKey {
        self.key
    }

    pub fn try_finish(&mut self) -> Result<bool, ManagedCaptureCleanupError> {
        if self.finished {
            return Ok(true);
        }
        let mut inner = self.cell.try_lock().map_err(|error| match error {
            CellLockError::Busy => ManagedCaptureCleanupError::Busy,
            CellLockError::WrongKey | CellLockError::InvalidState => {
                ManagedCaptureCleanupError::InvalidState
            }
        })?;
        if inner.key != Some(self.key) {
            return Err(ManagedCaptureCleanupError::InvalidState);
        }
        if inner.phase == CapturePhase::Released {
            if inner.buffer.is_some() || inner.selected_frame.is_some() {
                return Err(ManagedCaptureCleanupError::InvalidState);
            }
            self.finished = true;
            self.cell.clear_consumer_handle(self.key);
            return Ok(true);
        }
        if inner.copy_possible
            && self.cell.queue_complete_key.load(Ordering::Acquire) < self.key.get()
        {
            return Ok(false);
        }
        if self.cell.map_state(self.key) == MAP_PENDING {
            return Ok(false);
        }
        let Some(buffer) = inner.buffer.take() else {
            drop(inner.selected_frame.take());
            inner.phase = CapturePhase::Released;
            self.finished = true;
            self.cell.clear_consumer_handle(self.key);
            return Ok(true);
        };
        match self.cell.map_state(self.key) {
            MAP_SUCCEEDED => buffer.unmap(),
            MAP_PENDING => {
                inner.buffer = Some(buffer);
                inner.phase = CapturePhase::MapPending;
                return Ok(false);
            }
            _ => {}
        }
        drop(buffer);
        drop(inner.selected_frame.take());
        inner.phase = CapturePhase::Released;
        self.finished = true;
        self.cell.clear_consumer_handle(self.key);
        Ok(true)
    }
}

impl Drop for ManagedCaptureCleanup {
    fn drop(&mut self) {
        if !self.finished {
            self.cell
                .abandon_key
                .fetch_max(self.key.get(), Ordering::AcqRel);
            if let Ok(mut inner) = self.cell.try_lock()
                && inner.key == Some(self.key)
                && inner.phase != CapturePhase::Released
            {
                inner.return_taken = false;
            }
            self.cell.clear_consumer_handle(self.key);
        }
    }
}

impl std::fmt::Debug for ManagedCaptureCleanup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedCaptureCleanup")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

pub struct ManagedCapturePixels {
    key: ManagedCaptureKey,
    presented: ManagedPresentedView,
    surface_format: ManagedCaptureSurfaceFormat,
    rgba8: Vec<u8>,
}

impl ManagedCapturePixels {
    pub fn key(&self) -> ManagedCaptureKey {
        self.key
    }

    pub fn presented(&self) -> &ManagedPresentedView {
        &self.presented
    }

    pub fn surface_format(&self) -> ManagedCaptureSurfaceFormat {
        self.surface_format
    }

    pub fn rgba8(&self) -> &[u8] {
        &self.rgba8
    }

    pub fn into_parts(
        self,
    ) -> (
        ManagedCaptureKey,
        ManagedPresentedView,
        ManagedCaptureSurfaceFormat,
        Vec<u8>,
    ) {
        (self.key, self.presented, self.surface_format, self.rgba8)
    }
}

impl std::fmt::Debug for ManagedCapturePixels {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedCapturePixels")
            .field("key", &self.key)
            .field("presented", &self.presented)
            .field("surface_format", &self.surface_format)
            .field("rgba8_bytes", &self.rgba8.len())
            .finish()
    }
}

pub(super) struct ManagedCaptureShared {
    readiness: OnceLock<ManagedCaptureState>,
    detached: AtomicBool,
    window_closed: AtomicBool,
    request: Mutex<Option<Box<ManagedCaptureRequest>>>,
    last_key: AtomicU64,
    worker_registered: AtomicBool,
    worker_alive: AtomicBool,
    window_started: AtomicBool,
    cell: Arc<ManagedCaptureCell>,
}

impl ManagedCaptureShared {
    fn new() -> Self {
        Self {
            readiness: OnceLock::new(),
            detached: AtomicBool::new(false),
            window_closed: AtomicBool::new(false),
            request: Mutex::new(None),
            last_key: AtomicU64::new(0),
            worker_registered: AtomicBool::new(false),
            worker_alive: AtomicBool::new(false),
            window_started: AtomicBool::new(false),
            cell: Arc::new(ManagedCaptureCell::new()),
        }
    }

    pub(super) fn try_set_ready(
        &self,
        info: ManagedCaptureReadyInfo,
    ) -> Result<(), ManagedCaptureReadyError> {
        if self.detached.load(Ordering::Acquire) {
            return Err(ManagedCaptureReadyError::Detached);
        }
        self.readiness
            .set(ManagedCaptureState::Ready(info))
            .map_err(|_| ManagedCaptureReadyError::AlreadyResolved)
    }

    pub(super) fn try_set_unsupported(
        &self,
        reason: ManagedCaptureUnsupportedReason,
    ) -> Result<(), ManagedCaptureReadyError> {
        if self.detached.load(Ordering::Acquire) {
            return Err(ManagedCaptureReadyError::Detached);
        }
        self.readiness
            .set(ManagedCaptureState::Unsupported(reason))
            .map_err(|_| ManagedCaptureReadyError::AlreadyResolved)
    }

    pub(super) fn state(&self) -> ManagedCaptureState {
        if self.detached.load(Ordering::Acquire) {
            ManagedCaptureState::Detached
        } else {
            self.readiness
                .get()
                .cloned()
                .unwrap_or(ManagedCaptureState::Starting)
        }
    }

    pub(super) fn try_take_request(
        &self,
    ) -> Result<Option<Box<ManagedCaptureRequest>>, ManagedCaptureEndpointError> {
        let mut slot = self.request.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => ManagedCaptureEndpointError::Busy,
            TryLockError::Poisoned(error) => {
                drop(error.into_inner());
                ManagedCaptureEndpointError::Poisoned
            }
        })?;
        let Some(request) = slot.take() else {
            return Ok(None);
        };
        if self.detached.load(Ordering::Acquire) || !self.worker_alive.load(Ordering::Acquire) {
            self.reject_unclaimed_key(request.key);
            return Ok(None);
        }
        Ok(Some(request))
    }

    fn reject_unclaimed_key(&self, key: ManagedCaptureKey) {
        self.cell.cancel_key.fetch_max(key.get(), Ordering::AcqRel);
        self.cell.abandon_key.fetch_max(key.get(), Ordering::AcqRel);
        let _ = self.cell.mark_failed(key, ManagedCaptureFailure::Cancelled);
    }

    fn abandon_unclaimed_request(&self) -> Result<(), ManagedCaptureEndpointError> {
        if !self.detached.load(Ordering::Acquire) && self.worker_alive.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut slot = self.request.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => ManagedCaptureEndpointError::Busy,
            TryLockError::Poisoned(error) => {
                drop(error.into_inner());
                ManagedCaptureEndpointError::Poisoned
            }
        })?;
        if let Some(request) = slot.take() {
            self.reject_unclaimed_key(request.key);
        }
        Ok(())
    }

    pub(super) fn close_window(&self) {
        self.window_closed.store(true, Ordering::Release);
        self.detached.store(true, Ordering::Release);
        let _ = self.abandon_unclaimed_request();
        let key = self.cell.key.load(Ordering::Acquire);
        if key == 0 {
            return;
        }
        self.cell.cancel_key.fetch_max(key, Ordering::AcqRel);
        if let Some(key) = ManagedCaptureKey::new(key) {
            let _ = self.cell.close_window_key(key);
        }
    }

    pub(super) fn cell(&self) -> Arc<ManagedCaptureCell> {
        Arc::clone(&self.cell)
    }

    pub(super) fn mark_window_started(&self) {
        self.window_started.store(true, Ordering::Release);
        if !self.worker_registered.load(Ordering::Acquire) {
            let _ = self.try_set_unsupported(ManagedCaptureUnsupportedReason::CaptureDisabled);
        }
    }

    fn cancel(
        &self,
        key: ManagedCaptureKey,
        check_detached: bool,
    ) -> Result<ManagedCaptureCancelStatus, ManagedCaptureCancelError> {
        if check_detached && self.detached.load(Ordering::Acquire) {
            return Err(ManagedCaptureCancelError::Detached);
        }
        let mut inner = self.cell.try_lock().map_err(|error| match error {
            CellLockError::Busy => ManagedCaptureCancelError::Busy,
            CellLockError::WrongKey | CellLockError::InvalidState => {
                ManagedCaptureCancelError::Poisoned
            }
        })?;
        if inner.key != Some(key)
            || matches!(inner.phase, CapturePhase::Vacant | CapturePhase::Released)
        {
            return Ok(ManagedCaptureCancelStatus::Unknown);
        }
        self.cell.cancel_key.fetch_max(key.get(), Ordering::AcqRel);
        if inner.copy_possible {
            inner.failure = Some(ManagedCaptureFailure::Cancelled);
            inner.phase = CapturePhase::CleanupRequired;
            self.cell.abandon_key.fetch_max(key.get(), Ordering::AcqRel);
            Ok(ManagedCaptureCancelStatus::AlreadySubmitted)
        } else {
            inner.failure = Some(ManagedCaptureFailure::Cancelled);
            inner.phase = CapturePhase::Failed;
            Ok(ManagedCaptureCancelStatus::Accepted)
        }
    }
}

pub(crate) struct ManagedCaptureCell {
    inner: Mutex<ManagedCaptureCellInner>,
    key: AtomicU64,
    cancel_key: AtomicU64,
    abandon_key: AtomicU64,
    map_pending_key: AtomicU64,
    map_registered_key: AtomicU64,
    map_success_key: AtomicU64,
    map_failure_key: AtomicU64,
    queue_complete_key: AtomicU64,
    consumer_handle_key: AtomicU64,
    worker_detached: AtomicBool,
    fence_registered_key: AtomicU64,
}

struct ManagedCaptureCellInner {
    key: Option<ManagedCaptureKey>,
    phase: CapturePhase,
    buffer: Option<wgpu::Buffer>,
    selected_frame: Option<Arc<CpuFrameHandle>>,
    presented: Option<ManagedPresentedView>,
    format: Option<ManagedCaptureSurfaceFormat>,
    width: u32,
    height: u32,
    bytes_per_row: usize,
    total_bytes: usize,
    failure: Option<ManagedCaptureFailure>,
    return_taken: bool,
    queue_serial: Option<u64>,
    copy_possible: bool,
}

impl ManagedCaptureCell {
    fn new() -> Self {
        Self {
            inner: Mutex::new(ManagedCaptureCellInner {
                key: None,
                phase: CapturePhase::Vacant,
                buffer: None,
                selected_frame: None,
                presented: None,
                format: None,
                width: 0,
                height: 0,
                bytes_per_row: 0,
                total_bytes: 0,
                failure: None,
                return_taken: false,
                queue_serial: None,
                copy_possible: false,
            }),
            key: AtomicU64::new(0),
            cancel_key: AtomicU64::new(0),
            abandon_key: AtomicU64::new(0),
            map_pending_key: AtomicU64::new(0),
            map_registered_key: AtomicU64::new(0),
            map_success_key: AtomicU64::new(0),
            map_failure_key: AtomicU64::new(0),
            queue_complete_key: AtomicU64::new(0),
            consumer_handle_key: AtomicU64::new(0),
            worker_detached: AtomicBool::new(false),
            fence_registered_key: AtomicU64::new(0),
        }
    }

    fn try_lock(&self) -> Result<MutexGuard<'_, ManagedCaptureCellInner>, CellLockError> {
        match self.inner.try_lock() {
            Ok(inner) => Ok(inner),
            Err(TryLockError::WouldBlock) => Err(CellLockError::Busy),
            Err(TryLockError::Poisoned(error)) => Ok(error.into_inner()),
        }
    }

    fn clear_consumer_handle(&self, key: ManagedCaptureKey) {
        let _ = self.consumer_handle_key.compare_exchange(
            key.get(),
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn reserve(&self, key: ManagedCaptureKey) -> Result<(), CellLockError> {
        let mut inner = self.try_lock()?;
        if !matches!(inner.phase, CapturePhase::Vacant | CapturePhase::Released)
            || inner.buffer.is_some()
            || inner.selected_frame.is_some()
            || inner.key.is_some_and(|current| {
                self.consumer_handle_key.load(Ordering::Acquire) == current.get()
            })
        {
            return Err(CellLockError::Busy);
        }
        self.key.store(key.get(), Ordering::Release);
        self.cancel_key.store(0, Ordering::Release);
        self.abandon_key.store(0, Ordering::Release);
        self.map_pending_key.store(0, Ordering::Release);
        self.map_registered_key.store(0, Ordering::Release);
        self.map_success_key.store(0, Ordering::Release);
        self.map_failure_key.store(0, Ordering::Release);
        self.queue_complete_key.store(0, Ordering::Release);
        self.consumer_handle_key.store(0, Ordering::Release);
        self.fence_registered_key.store(0, Ordering::Release);
        inner.key = Some(key);
        inner.phase = CapturePhase::Reserved;
        inner.buffer = None;
        inner.selected_frame = None;
        inner.presented = None;
        inner.format = None;
        inner.width = 0;
        inner.height = 0;
        inner.bytes_per_row = 0;
        inner.total_bytes = 0;
        inner.failure = None;
        inner.return_taken = false;
        inner.queue_serial = None;
        inner.copy_possible = false;
        Ok(())
    }

    fn close_window_key(&self, key: ManagedCaptureKey) -> Result<(), CellLockError> {
        let mut inner = self.try_lock()?;
        if inner.key != Some(key) || inner.phase == CapturePhase::Released {
            return Ok(());
        }
        if self.map_pending_key.load(Ordering::Acquire) == key.get()
            && self.map_registered_key.load(Ordering::Acquire) != key.get()
        {
            self.map_failure_key.fetch_max(key.get(), Ordering::AcqRel);
        }
        match inner.phase {
            CapturePhase::Submitted => {}
            CapturePhase::CopyMayBeSubmitted | CapturePhase::MapPending if inner.copy_possible => {
                inner
                    .failure
                    .get_or_insert(ManagedCaptureFailure::CopyFailed);
                inner.phase = CapturePhase::CleanupRequired;
                self.abandon_key.fetch_max(key.get(), Ordering::AcqRel);
            }
            CapturePhase::CleanupRequired => {
                self.abandon_key.fetch_max(key.get(), Ordering::AcqRel);
            }
            CapturePhase::Failed => {}
            CapturePhase::Vacant
            | CapturePhase::Reserved
            | CapturePhase::Installing
            | CapturePhase::CopyMayBeSubmitted
            | CapturePhase::MapPending
            | CapturePhase::Released => {
                inner
                    .failure
                    .get_or_insert(ManagedCaptureFailure::Cancelled);
                inner.phase = CapturePhase::Failed;
                self.abandon_key.fetch_max(key.get(), Ordering::AcqRel);
            }
        }
        Ok(())
    }

    pub(crate) fn try_lock_for_ui(
        &self,
        key: ManagedCaptureKey,
    ) -> Result<ManagedCaptureCellGuard<'_>, CellLockError> {
        let inner = self.try_lock()?;
        if inner.key != Some(key) {
            return Err(CellLockError::WrongKey);
        }
        Ok(ManagedCaptureCellGuard {
            cell: self,
            key,
            inner,
            cleanup_on_drop: false,
        })
    }

    pub(crate) fn try_abandoned_cleanup(&self) -> Result<bool, CellLockError> {
        if !self.worker_detached.load(Ordering::Acquire)
            || self.consumer_handle_key.load(Ordering::Acquire) == self.key.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let key = self.key.load(Ordering::Acquire);
        if key == 0 || self.abandon_key.load(Ordering::Acquire) != key {
            return Ok(false);
        }
        let mut inner = self.try_lock()?;
        if inner.phase == CapturePhase::Released {
            return Ok(inner.buffer.is_none() && inner.selected_frame.is_none());
        }
        let copy_may_be_submitted = inner.copy_possible;
        if inner.key.map(ManagedCaptureKey::get) != Some(key)
            || (copy_may_be_submitted && self.queue_complete_key.load(Ordering::Acquire) < key)
        {
            return Ok(false);
        }
        if self.map_pending_key.load(Ordering::Acquire) == key
            && self.map_success_key.load(Ordering::Acquire) < key
            && self.map_failure_key.load(Ordering::Acquire) < key
        {
            inner.phase = CapturePhase::MapPending;
            return Ok(false);
        }
        if let Some(buffer) = inner.buffer.take() {
            if self.map_success_key.load(Ordering::Acquire) == key {
                buffer.unmap();
            }
            drop(buffer);
        }
        drop(inner.selected_frame.take());
        inner.phase = CapturePhase::Released;
        Ok(true)
    }

    pub(crate) fn fallback_fence_info(
        &self,
    ) -> Result<Option<(ManagedCaptureKey, u64)>, CellLockError> {
        let inner = self.try_lock()?;
        let Some(key) = inner.key else {
            return Ok(None);
        };
        if !inner.copy_possible
            || self.queue_complete_key.load(Ordering::Acquire) >= key.get()
            || self.fence_registered_key.load(Ordering::Acquire) == key.get()
        {
            return Ok(None);
        }
        let Some(serial) = inner.queue_serial else {
            return Ok(None);
        };
        Ok(Some((key, serial)))
    }

    pub(crate) fn register_queue_fence(&self, key: ManagedCaptureKey) {
        self.fence_registered_key
            .store(key.get(), Ordering::Release);
    }

    pub(crate) fn mark_queue_complete(&self, key: ManagedCaptureKey) {
        self.queue_complete_key
            .fetch_max(key.get(), Ordering::AcqRel);
    }

    pub(crate) fn mark_failed(
        &self,
        key: ManagedCaptureKey,
        failure: ManagedCaptureFailure,
    ) -> Result<(), CellLockError> {
        let mut inner = self.try_lock()?;
        if inner.key != Some(key) {
            return Err(CellLockError::WrongKey);
        }
        if inner.copy_possible {
            inner.failure = Some(failure);
            inner.phase = CapturePhase::CleanupRequired;
        } else {
            inner.failure = Some(failure);
            inner.phase = CapturePhase::Failed;
        }
        Ok(())
    }

    pub(super) fn is_cancelled(&self, key: ManagedCaptureKey) -> bool {
        self.cancel_key.load(Ordering::Acquire) == key.get()
            || self.abandon_key.load(Ordering::Acquire) == key.get()
    }

    pub(super) fn current_key(&self) -> Option<ManagedCaptureKey> {
        ManagedCaptureKey::new(self.key.load(Ordering::Acquire))
    }

    pub(crate) fn is_released(&self) -> bool {
        self.try_lock().is_ok_and(|inner| {
            matches!(inner.phase, CapturePhase::Vacant | CapturePhase::Released)
                && inner.buffer.is_none()
                && inner.selected_frame.is_none()
        })
    }

    fn public_status(
        &self,
        inner: &ManagedCaptureCellInner,
        key: ManagedCaptureKey,
    ) -> ManagedCaptureCellStatus {
        if inner.copy_possible && self.queue_complete_key.load(Ordering::Acquire) < key.get() {
            return ManagedCaptureCellStatus::QueuePending;
        }
        if inner.phase == CapturePhase::Submitted && self.map_state(key) == MAP_PENDING {
            return ManagedCaptureCellStatus::MapPending;
        }
        match inner.phase {
            CapturePhase::Vacant | CapturePhase::Released => ManagedCaptureCellStatus::Released,
            CapturePhase::Reserved => ManagedCaptureCellStatus::Reserved,
            CapturePhase::Installing => ManagedCaptureCellStatus::Installing,
            CapturePhase::CopyMayBeSubmitted => ManagedCaptureCellStatus::CopyMayBeSubmitted,
            CapturePhase::MapPending => ManagedCaptureCellStatus::MapPending,
            CapturePhase::Submitted => ManagedCaptureCellStatus::Submitted,
            CapturePhase::CleanupRequired => ManagedCaptureCellStatus::CleanupRequired,
            CapturePhase::Failed => ManagedCaptureCellStatus::CleanupRequired,
        }
    }

    pub(crate) fn set_map_pending(&self, key: ManagedCaptureKey) {
        self.map_pending_key.store(key.get(), Ordering::Release);
    }

    pub(crate) fn set_map_registered(&self, key: ManagedCaptureKey) {
        self.map_registered_key.store(key.get(), Ordering::Release);
    }

    pub(crate) fn set_map_result(&self, key: ManagedCaptureKey, success: bool) {
        if success {
            self.map_success_key.fetch_max(key.get(), Ordering::AcqRel);
        } else {
            self.map_failure_key.fetch_max(key.get(), Ordering::AcqRel);
        }
    }

    fn map_state(&self, key: ManagedCaptureKey) -> u8 {
        if self.map_success_key.load(Ordering::Acquire) >= key.get() {
            MAP_SUCCEEDED
        } else if self.map_failure_key.load(Ordering::Acquire) >= key.get() {
            MAP_FAILED
        } else if self.map_pending_key.load(Ordering::Acquire) >= key.get() {
            MAP_PENDING
        } else {
            MAP_NOT_REQUESTED
        }
    }
}

pub(crate) struct ManagedCaptureCellGuard<'a> {
    cell: &'a ManagedCaptureCell,
    key: ManagedCaptureKey,
    inner: MutexGuard<'a, ManagedCaptureCellInner>,
    cleanup_on_drop: bool,
}

impl ManagedCaptureCellGuard<'_> {
    pub(crate) fn arm_cleanup_on_drop(&mut self) {
        self.cleanup_on_drop = true;
    }

    pub(crate) fn disarm_cleanup_on_drop(&mut self) {
        self.cleanup_on_drop = false;
    }

    pub(crate) fn copy_possible(&self) -> bool {
        self.inner.copy_possible
    }

    /// Pins the exact selected frame in the durable cell. Call only after all
    /// retryable admission checks and immediately before GPU buffer creation.
    pub(crate) fn install_selected_frame(
        &mut self,
        frame: Arc<CpuFrameHandle>,
    ) -> Result<(), CellLockError> {
        if self.inner.key != Some(self.key)
            || self.inner.phase != CapturePhase::Reserved
            || self.inner.buffer.is_some()
            || self.inner.selected_frame.is_some()
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.selected_frame = Some(frame);
        Ok(())
    }

    pub(crate) fn begin_install(&mut self) -> Result<(), CellLockError> {
        if self.inner.phase != CapturePhase::Reserved
            || self.inner.buffer.is_some()
            || self.inner.selected_frame.is_none()
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.phase = CapturePhase::Installing;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn begin_metadata_install(&mut self) -> Result<(), CellLockError> {
        if self.inner.phase != CapturePhase::Reserved || self.inner.buffer.is_some() {
            return Err(CellLockError::InvalidState);
        }
        self.inner.phase = CapturePhase::Installing;
        Ok(())
    }

    pub(crate) fn create_and_install_buffer(
        &mut self,
        device: &wgpu::Device,
        descriptor: &wgpu::BufferDescriptor<'_>,
    ) -> Result<(), CellLockError> {
        if self.inner.buffer.is_some() {
            return Err(CellLockError::InvalidState);
        }
        self.begin_install()?;
        // The cell guard remains held across creation and immediate installation.
        // No local Buffer survives this method, including an unwind after creation.
        let buffer = device.create_buffer(descriptor);
        self.inner.buffer = Some(buffer);
        self.inner.phase = CapturePhase::Reserved;
        Ok(())
    }

    pub(crate) fn buffer(&self) -> Result<&wgpu::Buffer, CellLockError> {
        self.inner
            .buffer
            .as_ref()
            .ok_or(CellLockError::InvalidState)
    }

    pub(crate) fn set_dimensions(
        &mut self,
        width: u32,
        height: u32,
        bytes_per_row: usize,
        total_bytes: usize,
        format: ManagedCaptureSurfaceFormat,
    ) {
        self.inner.width = width;
        self.inner.height = height;
        self.inner.bytes_per_row = bytes_per_row;
        self.inner.total_bytes = total_bytes;
        self.inner.format = Some(format);
    }

    pub(crate) fn mark_copy_may_be_submitted(&mut self) -> Result<(), CellLockError> {
        if self.inner.phase != CapturePhase::Reserved
            || self.inner.buffer.is_none()
            || self.inner.selected_frame.is_none()
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.copy_possible = true;
        self.inner.phase = CapturePhase::CopyMayBeSubmitted;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn mark_copy_pending_metadata(&mut self, serial: u64) -> Result<(), CellLockError> {
        if self.inner.key != Some(self.key)
            || self.inner.phase != CapturePhase::Reserved
            || self.inner.buffer.is_some()
            || self.inner.selected_frame.is_none()
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.copy_possible = true;
        self.inner.queue_serial = Some(serial);
        self.inner.phase = CapturePhase::MapPending;
        self.cell.set_map_pending(self.key);
        self.cell.set_map_registered(self.key);
        Ok(())
    }

    pub(crate) fn mark_map_pending(&mut self, key: ManagedCaptureKey) {
        self.inner.phase = CapturePhase::MapPending;
        self.inner.key = Some(key);
    }

    pub(crate) fn set_queue_serial(&mut self, serial: u64) -> Result<(), CellLockError> {
        if self.inner.key != Some(self.key) {
            return Err(CellLockError::WrongKey);
        }
        self.inner.queue_serial = Some(serial);
        Ok(())
    }

    pub(crate) fn key(&self) -> ManagedCaptureKey {
        self.key
    }

    pub(crate) fn mark_submitted(
        &mut self,
        key: ManagedCaptureKey,
        view: ManagedPresentedView,
    ) -> Result<(), CellLockError> {
        if self.inner.key != Some(key)
            || self.inner.selected_frame.is_none()
            || !matches!(
                self.inner.phase,
                CapturePhase::MapPending
                    | CapturePhase::CopyMayBeSubmitted
                    | CapturePhase::CleanupRequired
            )
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.presented = Some(view);
        if self.inner.phase != CapturePhase::CleanupRequired {
            self.inner.phase = CapturePhase::Submitted;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn mark_submitted_metadata(
        &mut self,
        key: ManagedCaptureKey,
        view: ManagedPresentedView,
    ) -> Result<(), CellLockError> {
        if self.inner.key != Some(key)
            || !matches!(
                self.inner.phase,
                CapturePhase::Installing
                    | CapturePhase::MapPending
                    | CapturePhase::CopyMayBeSubmitted
            )
        {
            return Err(CellLockError::InvalidState);
        }
        self.inner.presented = Some(view);
        self.inner.phase = CapturePhase::Submitted;
        Ok(())
    }

    pub(crate) fn mark_cleanup(&mut self, failure: Option<ManagedCaptureFailure>) {
        if self.inner.phase != CapturePhase::Released {
            if failure.is_some() {
                self.inner.failure = failure;
            }
            self.inner.phase = CapturePhase::CleanupRequired;
        }
    }

    pub(crate) fn mark_failed(&mut self, failure: ManagedCaptureFailure) {
        self.inner.failure = Some(failure);
        self.inner.phase = CapturePhase::Failed;
    }
}

impl Drop for ManagedCaptureCellGuard<'_> {
    fn drop(&mut self) {
        if !self.cleanup_on_drop || self.inner.key != Some(self.key) {
            return;
        }
        self.cell
            .abandon_key
            .fetch_max(self.key.get(), Ordering::AcqRel);
        if self.cell.map_pending_key.load(Ordering::Acquire) == self.key.get()
            && self.cell.map_registered_key.load(Ordering::Acquire) != self.key.get()
        {
            self.cell.set_map_result(self.key, false);
        }
        if self.inner.phase != CapturePhase::Released {
            self.inner
                .failure
                .get_or_insert(ManagedCaptureFailure::CopyFailed);
            self.inner.phase = if self.inner.copy_possible {
                CapturePhase::CleanupRequired
            } else {
                CapturePhase::Failed
            };
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CellLockError {
    Busy,
    WrongKey,
    InvalidState,
}

fn map_readback_failure(failure: ManagedCaptureFailure) -> ManagedCaptureReadbackError {
    match failure {
        ManagedCaptureFailure::Cancelled => ManagedCaptureReadbackError::Cancelled,
        ManagedCaptureFailure::DeadlineElapsed => ManagedCaptureReadbackError::DeadlineElapsed,
        ManagedCaptureFailure::StaleSource => ManagedCaptureReadbackError::StaleSource,
        ManagedCaptureFailure::Unsupported => ManagedCaptureReadbackError::UnsupportedFormat,
        ManagedCaptureFailure::TooLarge => ManagedCaptureReadbackError::TooLarge,
        ManagedCaptureFailure::CopyFailed => ManagedCaptureReadbackError::CopyFailed,
        ManagedCaptureFailure::MapFailed => ManagedCaptureReadbackError::MapFailed,
        ManagedCaptureFailure::DevicePollFailed => ManagedCaptureReadbackError::DevicePollFailed,
        ManagedCaptureFailure::CounterExhausted => ManagedCaptureReadbackError::CounterExhausted,
        ManagedCaptureFailure::AllocationFailed => ManagedCaptureReadbackError::AllocationFailed,
    }
}

fn copy_surface_rows_to_rgba8(
    source: &[u8],
    target: &mut [u8],
    width: u32,
    height: u32,
    bytes_per_row: usize,
    format: ManagedCaptureSurfaceFormat,
) -> Result<(), ManagedCaptureReadbackError> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(ManagedCaptureReadbackError::TooLarge)?;
    let required_source = bytes_per_row
        .checked_mul(height as usize)
        .ok_or(ManagedCaptureReadbackError::TooLarge)?;
    let required_target = row_bytes
        .checked_mul(height as usize)
        .ok_or(ManagedCaptureReadbackError::TooLarge)?;
    if source.len() < required_source || target.len() < required_target {
        return Err(ManagedCaptureReadbackError::InvalidMappedRange);
    }
    for y in 0..height as usize {
        let source_row = &source[y * bytes_per_row..y * bytes_per_row + row_bytes];
        let target_row = &mut target[y * row_bytes..(y + 1) * row_bytes];
        match format {
            ManagedCaptureSurfaceFormat::Rgba8UnormSrgb
            | ManagedCaptureSurfaceFormat::Rgba8Unorm => target_row.copy_from_slice(source_row),
            ManagedCaptureSurfaceFormat::Bgra8UnormSrgb
            | ManagedCaptureSurfaceFormat::Bgra8Unorm => {
                for (pixel, output) in source_row
                    .chunks_exact(4)
                    .zip(target_row.chunks_exact_mut(4))
                {
                    output.copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    use daedalus_content::{
        ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache, ProjectManifest,
        ProjectPresentation, prepare_project_with_assets,
    };
    use daedalus_core::{RenderSnapshot, SessionId, Tick};

    use super::{
        ManagedCaptureCell, ManagedCaptureCleanup, ManagedCaptureClient, ManagedCaptureFailure,
        ManagedCaptureKey, ManagedCaptureReadyInfo, ManagedCaptureRequest, ManagedCaptureSource,
        ManagedCaptureState, ManagedCaptureSubmitError, ManagedCaptureSurfaceFormat,
        ManagedCaptureUnsupportedReason, checked_capture_buffer_layout, copy_surface_rows_to_rgba8,
    };
    use crate::window::managed::ManagedStageKey;
    use crate::{
        PacketAssetLimits, PacketCpuLimits, PacketCpuOwner, PacketRendererLimits, SourcePin,
    };

    fn source() -> ManagedCaptureSource {
        ManagedCaptureSource::new(
            ManagedStageKey::INITIAL,
            SessionId::from_raw(3),
            1,
            &format!("{:064x}", 17),
        )
        .unwrap()
    }

    pub(super) fn actual_cpu_frame() -> Arc<crate::CpuFrameHandle> {
        let mut manifest = ProjectManifest::new("capture-frame-pin");
        manifest.presentation = Some(ProjectPresentation {
            clear: Some([0.13, 0.34, 0.72, 0.37]),
            ..ProjectPresentation::default()
        });
        let manifest = serde_json::to_vec(&manifest).expect("manifest serializes");
        let scene = br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#;
        let mut resolver = ByteMapSourceResolver::default();
        let mut cache = MemoryCookCache::default();
        let project = prepare_project_with_assets(
            &manifest,
            scene,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .expect("empty imported project prepares through the public asset path");
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .expect("default CPU limits are valid");
        let generation = owner
            .prepare_generation(
                &project,
                1,
                SessionId::from_raw(3),
                &BTreeMap::new(),
                SourcePin::new(Arc::new(())),
                &AtomicBool::new(false),
            )
            .expect("project creates one managed generation");
        owner
            .extract_validated(
                generation,
                &RenderSnapshot {
                    sequence: 1,
                    tick: Tick(1),
                    entities: Arc::from([]),
                },
                1,
                None,
                &AtomicBool::new(false),
            )
            .expect("snapshot extracts through the managed CPU path")
    }

    #[test]
    fn readiness_is_one_shot_and_capture_request_is_owned_on_busy() {
        let (client, shared) = ManagedCaptureClient::new();
        let worker = client.register_worker().unwrap();
        assert!(client.register_worker().is_err());
        assert_eq!(client.try_status().unwrap(), ManagedCaptureState::Starting);
        shared
            .try_set_ready(
                ManagedCaptureReadyInfo::new(
                    ManagedCaptureSurfaceFormat::Bgra8UnormSrgb,
                    8192,
                    8192,
                    8_388_608,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(
            shared
                .try_set_unsupported(ManagedCaptureUnsupportedReason::CaptureDisabled)
                .is_err()
        );
        let request = ManagedCaptureRequest::new(
            ManagedCaptureKey::new(1).unwrap(),
            source(),
            Tick(4),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        client.try_submit(request).unwrap();
        let retry = ManagedCaptureRequest::new(
            ManagedCaptureKey::new(2).unwrap(),
            source(),
            Tick(4),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let (retry, error) = client.try_submit(retry).unwrap_err();
        assert_eq!(error, ManagedCaptureSubmitError::Busy);
        assert_eq!(retry.key().get(), 2);
        drop(worker);
    }

    #[test]
    fn request_source_preserves_prefixed_digest_exactly() {
        let digest = format!("sha256:{:064x}", 91);
        let source =
            ManagedCaptureSource::new(ManagedStageKey::INITIAL, SessionId::from_raw(8), 1, &digest)
                .unwrap();
        assert_eq!(source.source_digest(), digest);
    }

    #[test]
    fn selected_cpu_frame_stays_pinned_until_worker_cleanup_releases_it() {
        let cell = Arc::new(ManagedCaptureCell::new());
        let key = ManagedCaptureKey::new(1).unwrap();
        cell.reserve(key).expect("empty cell reserves key");
        let frame = actual_cpu_frame();
        let weak = Arc::downgrade(&frame);
        {
            let mut guard = cell
                .try_lock_for_ui(key)
                .expect("reserved cell is available");
            guard
                .install_selected_frame(Arc::clone(&frame))
                .expect("actual selected frame is pinned in the cell");
            guard.mark_failed(ManagedCaptureFailure::CopyFailed);
        }
        drop(frame);
        assert!(
            weak.upgrade().is_some(),
            "the cell owns the exact frame pin"
        );
        assert_eq!(
            cell.reserve(ManagedCaptureKey::new(2).unwrap()),
            Err(super::CellLockError::Busy),
            "a live strong frame pin prevents reuse of the capture cell"
        );
        assert!(
            weak.upgrade().is_some(),
            "busy reserve leaves the old pin intact"
        );

        // Metadata-only no-copy cleanup through the worker cleanup owner. This proves CPU
        // backing lifetime, not a queue submit, GPU fence, map, or pixel result.
        let mut cleanup = ManagedCaptureCleanup::new(Arc::clone(&cell), key);
        assert!(cleanup.try_finish().expect("no-copy cleanup finishes"));
        assert!(
            weak.upgrade().is_none(),
            "worker cleanup releases the final frame pin"
        );
        assert!(cell.is_released());
        cell.reserve(ManagedCaptureKey::new(2).unwrap())
            .expect("worker release permits a later key to reserve the cell");
    }

    #[test]
    fn synthetic_copy_unwind_keeps_frame_pin_until_queue_and_map_retire() {
        let cell = Arc::new(ManagedCaptureCell::new());
        let key = ManagedCaptureKey::new(1).unwrap();
        cell.reserve(key).expect("empty cell reserves key");
        let frame = actual_cpu_frame();
        let weak = Arc::downgrade(&frame);
        {
            let mut guard = cell
                .try_lock_for_ui(key)
                .expect("reserved cell is available");
            guard
                .install_selected_frame(Arc::clone(&frame))
                .expect("actual selected frame is pinned");
            guard.arm_cleanup_on_drop();
            guard
                .mark_copy_pending_metadata(19)
                .expect("test injects a post-submit metadata state");
        }
        drop(frame);
        assert!(weak.upgrade().is_some());
        let mut cleanup = ManagedCaptureCleanup::new(Arc::clone(&cell), key);
        assert!(!cleanup.try_finish().expect("queue fence is still pending"));
        cell.mark_queue_complete(key);
        assert!(!cleanup.try_finish().expect("map callback is still pending"));
        cell.set_map_result(key, true);
        assert!(
            cleanup
                .try_finish()
                .expect("synthetic completion flags permit cleanup")
        );
        assert!(weak.upgrade().is_none());
        assert!(cell.is_released());
        // This state-only test injects serial/fence/map metadata. It does not prove a GPU copy,
        // queue callback, map completion, or pixel output.
    }

    #[test]
    fn capture_copy_preserves_rgba_bytes_and_swizzles_bgra_without_gamma_conversion() {
        let width = 3;
        let height = 2u32;
        let row_bytes = width as usize * 4;
        let bytes_per_row = 256;
        let pixels: [[u8; 4]; 6] = [
            [3, 17, 129, 255],
            [41, 83, 197, 61],
            [254, 128, 4, 19],
            [23, 67, 111, 222],
            [9, 155, 201, 33],
            [250, 100, 5, 77],
        ];
        let mut source = vec![0xA5; bytes_per_row * height as usize];
        for (index, pixel) in pixels.iter().enumerate() {
            let y = index / width as usize;
            let x = index % width as usize;
            let start = y * bytes_per_row + x * 4;
            source[start..start + 4].copy_from_slice(pixel);
        }
        let expected_rgba: Vec<u8> = pixels.iter().flatten().copied().collect();
        for format in [
            ManagedCaptureSurfaceFormat::Rgba8Unorm,
            ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
            ManagedCaptureSurfaceFormat::Bgra8Unorm,
            ManagedCaptureSurfaceFormat::Bgra8UnormSrgb,
        ] {
            let mut encoded_source = source.clone();
            if matches!(
                format,
                ManagedCaptureSurfaceFormat::Bgra8Unorm
                    | ManagedCaptureSurfaceFormat::Bgra8UnormSrgb
            ) {
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let start = y * bytes_per_row + x * 4;
                        encoded_source[start..start + 4].copy_from_slice(&[
                            pixels[y * width as usize + x][2],
                            pixels[y * width as usize + x][1],
                            pixels[y * width as usize + x][0],
                            pixels[y * width as usize + x][3],
                        ]);
                    }
                }
            }
            let mut output = vec![0; row_bytes * height as usize];
            copy_surface_rows_to_rgba8(
                &encoded_source,
                &mut output,
                width,
                height,
                bytes_per_row,
                format,
            )
            .expect("padded mapped rows convert");
            assert_eq!(output, expected_rgba, "surface format {format:?}");
        }
    }

    #[test]
    fn capture_layout_preflights_physical_limits_alignment_and_device_buffer_size() {
        let ready =
            ManagedCaptureReadyInfo::new(ManagedCaptureSurfaceFormat::Rgba8UnormSrgb, 3, 2, 6)
                .unwrap();
        let layout = checked_capture_buffer_layout(
            3,
            2,
            ready,
            ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
            512,
        )
        .expect("odd width uses one padded 256-byte pitch per row");
        assert_eq!(layout.row_bytes, 12);
        assert_eq!(layout.bytes_per_row, 256);
        assert_eq!(layout.bytes_per_row_u32, 256);
        assert_eq!(layout.total_bytes, 512);
        assert!(
            checked_capture_buffer_layout(
                3,
                2,
                ready,
                ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
                511,
            )
            .is_err()
        );
        assert!(
            checked_capture_buffer_layout(
                4,
                2,
                ready,
                ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
                512,
            )
            .is_err()
        );
        assert!(
            checked_capture_buffer_layout(
                3,
                3,
                ready,
                ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
                1024,
            )
            .is_err()
        );
        assert!(
            checked_capture_buffer_layout(
                3,
                2,
                ready,
                ManagedCaptureSurfaceFormat::Bgra8UnormSrgb,
                512,
            )
            .is_err()
        );
        assert!(
            checked_capture_buffer_layout(
                0,
                2,
                ready,
                ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
                512,
            )
            .is_err()
        );
    }

    #[test]
    fn capture_row_copy_rejects_short_source_and_destination_ranges() {
        let mut output = vec![0; 24];
        assert!(
            copy_surface_rows_to_rgba8(
                &[0; 511],
                &mut output,
                3,
                2,
                256,
                ManagedCaptureSurfaceFormat::Rgba8Unorm,
            )
            .is_err()
        );
        assert!(
            copy_surface_rows_to_rgba8(
                &[0; 512],
                &mut output[..23],
                3,
                2,
                256,
                ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
            )
            .is_err()
        );
    }
}

#[cfg(test)]
#[path = "capture/ownership_tests.rs"]
mod ownership_tests;

#[cfg(test)]
#[path = "capture/gpu_tests.rs"]
mod gpu_tests;
