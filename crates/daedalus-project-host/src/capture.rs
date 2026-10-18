//! Bounded live-capture routing, PNG encoding, and capability-backed artifact access.
//!
//! File operations and final file ownership stay on the encoder worker. The host/coordinator
//! use only short `try_lock` operations; a path by itself never extends an artifact's lifetime.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use daedalus_core::SessionId;
use daedalus_renderer::{CpuFrameHandle, ManagedStageKey};
use daedalus_runtime::{
    CaptureArtifactCapability, CaptureArtifactId, CaptureFailure, CaptureFrameMetadata,
    CaptureHandoffState, CaptureInterestWeak, CaptureMetadataError, CapturePixelEncoding,
    CapturePngFormat, CapturePngMetadata, CaptureRequest, CaptureSurfaceFormat, CaptureView,
    CaptureWorkLease, MAX_CAPTURE_PNG_BYTES, MAX_CAPTURE_RAW_BYTES, ProjectPublicationIdentity,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

const ENCODER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PNG_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const MAX_PNG_BYTES: u64 = MAX_CAPTURE_PNG_BYTES;
static SPOOL_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Capacity-one coordinator-to-CPU transfer preserving the actor request and its real local key.
pub(crate) struct RoutedCapture {
    pub request: Box<CaptureRequest>,
    pub stage_key: ManagedStageKey,
}

#[derive(Debug, Error)]
pub(crate) enum CapturePipelineStartError {
    #[error("encoder thread could not start: {0}")]
    EncoderThread(#[source] io::Error),
    #[error("capture frame-slot limit is invalid")]
    InvalidFrameSlotLimit,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CapturePipelineTakeError {
    #[error("CPU capture port was already taken")]
    AlreadyTaken,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CaptureIngressError {
    #[error("capture ingress is busy")]
    Busy,
    #[error("capture transfer fence is closed")]
    Closed,
    #[error("capture ingress is poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CaptureRegistryError {
    #[error("capture registry is busy")]
    Busy,
    #[error("capture registry reached its frame-slot limit")]
    Full,
    #[error("publication binding conflicts with an existing frame")]
    Conflict,
    #[error("selected capture frame is stale or unknown")]
    Stale,
    #[error("capture registry is stopped")]
    Stopped,
    #[error("capture registry is poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CaptureEncoderError {
    #[error("encoder lane is busy")]
    Busy,
    #[error("encoder has stopped")]
    Stopped,
    #[error("encoder lane is poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CaptureStatusReadError {
    #[error("capture status is busy")]
    Busy,
    #[error("capture status is poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CapturePipelineFailure {
    #[error("private spool could not be initialized")]
    SpoolUnavailable,
    #[error("encoder worker panicked")]
    EncoderPanicked,
    #[error("artifact cleanup failed")]
    CleanupFailed,
}

fn failure_code(failure: CapturePipelineFailure) -> u8 {
    match failure {
        CapturePipelineFailure::SpoolUnavailable => 1,
        CapturePipelineFailure::EncoderPanicked => 2,
        CapturePipelineFailure::CleanupFailed => 3,
    }
}

fn failure_from_code(code: u8) -> Option<CapturePipelineFailure> {
    match code {
        1 => Some(CapturePipelineFailure::SpoolUnavailable),
        2 => Some(CapturePipelineFailure::EncoderPanicked),
        3 => Some(CapturePipelineFailure::CleanupFailed),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum CaptureEncodeJobError {
    #[error("captured RGBA dimensions or byte count are invalid")]
    InvalidPixels,
    #[error("capture output pixels must already be sRGB encoded")]
    InvalidEncoding,
    #[error("artifact interest is no longer live")]
    ExpiredInterest,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeArtifactAccessError {
    #[error("capture is busy")]
    Busy,
    #[error("capture is not ready")]
    NotReady,
    #[error("capability belongs to another session")]
    ForeignSession,
    #[error("artifact was not found")]
    UnknownArtifact,
    #[error("artifact interest has expired")]
    Expired,
    #[error("capture has stopped")]
    Stopped,
    #[error("capture registry is poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeArtifactIoError {
    #[error("artifact could not be opened")]
    OpenFailed,
    #[error("artifact read failed")]
    ReadFailed,
    #[error("artifact seek failed")]
    SeekFailed,
    #[error("artifact copy failed")]
    CopyFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureEncoderState {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed(CapturePipelineFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureEncoderShutdown {
    Stopping,
    Stopped,
    Failed(CapturePipelineFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturePipelineStatus {
    session: SessionId,
    renderer_state: CaptureHandoffState,
    encoder: CaptureEncoderState,
    pending_jobs: u8,
    retained_png_bytes: u64,
    cleanup_pending: u8,
    worker_exited: bool,
}

impl CapturePipelineStatus {
    pub(crate) fn session(&self) -> SessionId {
        self.session
    }

    pub(crate) fn renderer_state(&self) -> CaptureHandoffState {
        self.renderer_state
    }

    pub(crate) fn encoder(&self) -> CaptureEncoderState {
        self.encoder
    }

    pub(crate) fn pending_jobs(&self) -> u8 {
        self.pending_jobs
    }

    pub(crate) fn retained_png_bytes(&self) -> u64 {
        self.retained_png_bytes
    }

    pub(crate) fn cleanup_pending(&self) -> u8 {
        self.cleanup_pending
    }

    pub(crate) fn worker_exited(&self) -> bool {
        self.worker_exited
    }
}

struct PipelineShared {
    session: SessionId,
    frame_slots_limit: usize,
    ingress: Mutex<Ingress>,
    transfer_closed: AtomicBool,
    stop_requested: AtomicBool,
    failure_code: AtomicU8,
    publications: Mutex<PublicationRegistry>,
    artifacts: Mutex<ArtifactStore>,
    spool_root: Mutex<Option<PathBuf>>,
    status: Mutex<CapturePipelineStatus>,
    encoder_lane: Mutex<EncoderLane>,
    encoder_wake: Condvar,
}

struct Ingress {
    request: Option<RoutedCapture>,
}

struct PublicationRegistry {
    entries: Vec<PublicationEntry>,
}

struct PublicationEntry {
    publication: ProjectPublicationIdentity,
    stage_key: ManagedStageKey,
    frame: Weak<CpuFrameHandle>,
}

pub(crate) struct CapturePublicationBinding {
    publication: ProjectPublicationIdentity,
    stage_key: ManagedStageKey,
}

impl CapturePublicationBinding {
    pub(crate) fn publication(&self) -> &ProjectPublicationIdentity {
        &self.publication
    }

    pub(crate) fn stage_key(&self) -> ManagedStageKey {
        self.stage_key
    }
}

struct ArtifactStore {
    entries: HashMap<CaptureArtifactId, StoredArtifact>,
}

enum StoredArtifact {
    Partial {
        partial_path: PathBuf,
        final_path: PathBuf,
        _work: CaptureWorkLease,
    },
    Complete {
        weak_interest: CaptureInterestWeak,
        _work: CaptureWorkLease,
        binding: Box<ArtifactBinding>,
    },
}

#[derive(Clone)]
struct ArtifactBinding {
    path: PathBuf,
    metadata: CaptureFrameMetadata,
}

struct EncoderLane {
    job: Option<Box<CaptureEncodeJob>>,
    result: Option<CaptureEncodeResult>,
    active: Option<(CaptureArtifactId, ManagedStageKey)>,
    working: bool,
}

/// One encoder input. The RGBA buffer is moved into this value and is never copied for handoff.
pub(crate) struct CaptureEncodeJob {
    id: CaptureArtifactId,
    stage_key: ManagedStageKey,
    publication: ProjectPublicationIdentity,
    present_sequence: u64,
    view: CaptureView,
    surface_format: CaptureSurfaceFormat,
    surface_encoding: CapturePixelEncoding,
    interest: CaptureInterestWeak,
    work_lease: CaptureWorkLease,
    rgba: Vec<u8>,
}

impl CaptureEncodeJob {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: CaptureArtifactId,
        stage_key: ManagedStageKey,
        publication: ProjectPublicationIdentity,
        present_sequence: u64,
        view: CaptureView,
        surface_format: CaptureSurfaceFormat,
        surface_encoding: CapturePixelEncoding,
        interest: CaptureInterestWeak,
        work_lease: CaptureWorkLease,
        rgba: Vec<u8>,
    ) -> Result<Box<Self>, CaptureEncodeJobError> {
        if !interest.is_live() || interest.id() != id {
            return Err(CaptureEncodeJobError::ExpiredInterest);
        }
        if id.session() != publication.session() || present_sequence == 0 {
            return Err(CaptureEncodeJobError::InvalidPixels);
        }
        if surface_encoding != CapturePixelEncoding::SrgbEncoded {
            return Err(CaptureEncodeJobError::InvalidEncoding);
        }
        let pixels = u64::from(view.physical_width())
            .checked_mul(u64::from(view.physical_height()))
            .ok_or(CaptureEncodeJobError::InvalidPixels)?;
        let expected = pixels
            .checked_mul(4)
            .ok_or(CaptureEncodeJobError::InvalidPixels)?;
        if pixels == 0
            || expected > MAX_CAPTURE_RAW_BYTES
            || usize::try_from(expected).ok() != Some(rgba.len())
        {
            return Err(CaptureEncodeJobError::InvalidPixels);
        }
        Ok(Box::new(Self {
            id,
            stage_key,
            publication,
            present_sequence,
            view,
            surface_format,
            surface_encoding,
            interest,
            work_lease,
            rgba,
        }))
    }
}

pub(crate) struct CaptureEncodeResult {
    id: CaptureArtifactId,
    stage_key: ManagedStageKey,
    result: Result<CaptureFrameMetadata, CaptureFailure>,
}

impl CaptureEncodeResult {
    pub(crate) fn id(&self) -> CaptureArtifactId {
        self.id
    }

    pub(crate) fn stage_key(&self) -> ManagedStageKey {
        self.stage_key
    }

    pub(crate) fn result(&self) -> Result<&CaptureFrameMetadata, CaptureFailure> {
        self.result.as_ref().map_err(|failure| *failure)
    }
}

/// Startup/control guard. Its Drop only signals stop; it never waits for the encoder.
pub(crate) struct CapturePipelineOwner {
    shared: Arc<PipelineShared>,
    cpu_worker: Option<CaptureCpuWorkerPort>,
}

impl CapturePipelineOwner {
    pub(crate) fn coordinator_port(&self) -> CaptureCoordinatorPort {
        CaptureCoordinatorPort {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn control_port(&self) -> CaptureControlPort {
        CaptureControlPort {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn take_cpu_worker_port(
        &mut self,
    ) -> Result<CaptureCpuWorkerPort, CapturePipelineTakeError> {
        self.cpu_worker
            .take()
            .ok_or(CapturePipelineTakeError::AlreadyTaken)
    }

    pub(crate) fn request_stop(&self) {
        self.shared.request_stop();
    }

    pub(crate) fn try_finish_unclaimed(&mut self) -> CaptureEncoderShutdown {
        self.cpu_worker.as_mut().map_or_else(
            || self.shared.shutdown_state(),
            CaptureCpuWorkerPort::try_finish_encoder,
        )
    }
}

impl Drop for CapturePipelineOwner {
    fn drop(&mut self) {
        self.shared.request_stop();
    }
}

/// The one durable coordinator endpoint for request routing and actual frame bindings.
#[derive(Clone)]
pub(crate) struct CaptureCoordinatorPort {
    shared: Arc<PipelineShared>,
}

impl CaptureCoordinatorPort {
    #[cfg(test)]
    pub(crate) fn session(&self) -> SessionId {
        self.shared.session
    }

    /// Move a pending request into ingress only after every effect-free admission check passes.
    /// On Busy/Closed/Poisoned, the caller retains the exact same boxed request and work lease.
    pub(crate) fn try_send_pending(
        &self,
        request: &mut Option<Box<CaptureRequest>>,
        stage_key: ManagedStageKey,
    ) -> Result<(), CaptureIngressError> {
        if self.shared.transfer_closed.load(Ordering::Acquire) {
            return Err(CaptureIngressError::Closed);
        }
        let mut ingress = match self.shared.ingress.try_lock() {
            Ok(ingress) => ingress,
            Err(TryLockError::WouldBlock) => return Err(CaptureIngressError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureIngressError::Poisoned),
        };
        if self.shared.transfer_closed.load(Ordering::Acquire) {
            return Err(CaptureIngressError::Closed);
        }
        if ingress.request.is_some() {
            return Err(CaptureIngressError::Busy);
        }
        let Some(request) = request.take() else {
            return Err(CaptureIngressError::Closed);
        };
        ingress.request = Some(RoutedCapture { request, stage_key });
        Ok(())
    }

    pub(crate) fn try_register_publication(
        &self,
        publication: ProjectPublicationIdentity,
        stage_key: ManagedStageKey,
        frame: &Arc<CpuFrameHandle>,
    ) -> Result<(), CaptureRegistryError> {
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(CaptureRegistryError::Stopped);
        }
        if !publication_matches_frame(self.shared.session, &publication, frame) {
            return Err(CaptureRegistryError::Stale);
        }
        let mut registry = match self.shared.publications.try_lock() {
            Ok(registry) => registry,
            Err(TryLockError::WouldBlock) => return Err(CaptureRegistryError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureRegistryError::Poisoned),
        };
        registry
            .entries
            .retain(|entry| entry.frame.strong_count() != 0);
        for entry in &registry.entries {
            if entry
                .frame
                .upgrade()
                .is_some_and(|registered| Arc::ptr_eq(&registered, frame))
            {
                return if entry.stage_key == stage_key && entry.publication == publication {
                    Ok(())
                } else {
                    Err(CaptureRegistryError::Conflict)
                };
            }
        }
        if registry.entries.len() >= self.shared.frame_slots_limit {
            return Err(CaptureRegistryError::Full);
        }
        registry.entries.push(PublicationEntry {
            publication,
            stage_key,
            frame: Arc::downgrade(frame),
        });
        Ok(())
    }

    pub(crate) fn close_transfer_fence(&self) {
        self.shared.transfer_closed.store(true, Ordering::Release);
    }

    pub(crate) fn is_drained(&self) -> bool {
        if !self.shared.transfer_closed.load(Ordering::Acquire) {
            return false;
        }
        matches!(self.shared.ingress.try_lock(), Ok(ingress) if ingress.request.is_none())
    }

    pub(crate) fn try_status(&self) -> Result<CapturePipelineStatus, CaptureStatusReadError> {
        self.shared.try_status()
    }
}

/// CPU-owner endpoint. It alone owns the encoder JoinHandle after startup handoff.
pub(crate) struct CaptureCpuWorkerPort {
    shared: Arc<PipelineShared>,
    join: Option<JoinHandle<()>>,
    joined: bool,
}

impl CaptureCpuWorkerPort {
    pub(crate) fn try_take_request(&self) -> Result<Option<RoutedCapture>, CaptureIngressError> {
        let mut ingress = match self.shared.ingress.try_lock() {
            Ok(ingress) => ingress,
            Err(TryLockError::WouldBlock) => return Err(CaptureIngressError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureIngressError::Poisoned),
        };
        Ok(ingress.request.take())
    }

    pub(crate) fn resolve_selected_frame(
        &self,
        selected: &Weak<CpuFrameHandle>,
    ) -> Result<Option<CapturePublicationBinding>, CaptureRegistryError> {
        let Some(selected_frame) = selected.upgrade() else {
            return Ok(None);
        };
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err(CaptureRegistryError::Stopped);
        }
        let registry = match self.shared.publications.try_lock() {
            Ok(registry) => registry,
            Err(TryLockError::WouldBlock) => return Err(CaptureRegistryError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureRegistryError::Poisoned),
        };
        Ok(registry.entries.iter().find_map(|entry| {
            let registered = entry.frame.upgrade()?;
            if Arc::ptr_eq(&registered, &selected_frame)
                && publication_matches_frame(self.shared.session, &entry.publication, &registered)
            {
                Some(CapturePublicationBinding {
                    publication: entry.publication.clone(),
                    stage_key: entry.stage_key,
                })
            } else {
                None
            }
        }))
    }

    pub(crate) fn is_ingress_drained(&self) -> bool {
        if !self.shared.transfer_closed.load(Ordering::Acquire) {
            return false;
        }
        matches!(self.shared.ingress.try_lock(), Ok(ingress) if ingress.request.is_none())
    }

    pub(crate) fn try_publish_renderer_state(
        &self,
        state: CaptureHandoffState,
    ) -> Result<(), CaptureStatusReadError> {
        let mut status = match self.shared.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock) => return Err(CaptureStatusReadError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureStatusReadError::Poisoned),
        };
        status.renderer_state = state;
        Ok(())
    }

    pub(crate) fn try_submit_encode(
        &self,
        job: Box<CaptureEncodeJob>,
    ) -> Result<(), (Box<CaptureEncodeJob>, CaptureEncoderError)> {
        if self.shared.stop_requested.load(Ordering::Acquire) {
            return Err((job, CaptureEncoderError::Stopped));
        }
        match self.shared.status.try_lock() {
            Ok(status) if status.encoder == CaptureEncoderState::Ready => {}
            Ok(status)
                if matches!(
                    status.encoder,
                    CaptureEncoderState::Failed(_) | CaptureEncoderState::Stopped
                ) =>
            {
                return Err((job, CaptureEncoderError::Stopped));
            }
            Ok(_) | Err(TryLockError::WouldBlock) => {
                return Err((job, CaptureEncoderError::Busy));
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err((job, CaptureEncoderError::Poisoned));
            }
        }
        let mut lane = match self.shared.encoder_lane.try_lock() {
            Ok(lane) => lane,
            Err(TryLockError::WouldBlock) => return Err((job, CaptureEncoderError::Busy)),
            Err(TryLockError::Poisoned(_)) => return Err((job, CaptureEncoderError::Poisoned)),
        };
        if lane.job.is_some() || lane.result.is_some() || lane.working {
            return Err((job, CaptureEncoderError::Busy));
        }
        lane.job = Some(job);
        drop(lane);
        self.shared.encoder_wake.notify_one();
        self.shared.refresh_metrics_nonblocking();
        Ok(())
    }

    pub(crate) fn try_take_encode_result(
        &self,
    ) -> Result<Option<CaptureEncodeResult>, CaptureEncoderError> {
        let mut lane = match self.shared.encoder_lane.try_lock() {
            Ok(lane) => lane,
            Err(TryLockError::WouldBlock) => return Err(CaptureEncoderError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureEncoderError::Poisoned),
        };
        let result = lane.result.take();
        drop(lane);
        if result.is_some() {
            self.shared.encoder_wake.notify_one();
            self.shared.refresh_metrics_nonblocking();
        }
        Ok(result)
    }

    /// Reconcile a submit call interrupted by unwinding without blocking the CPU owner.
    pub(crate) fn try_encode_job_pending(
        &self,
        id: CaptureArtifactId,
        stage_key: ManagedStageKey,
    ) -> Result<bool, CaptureEncoderError> {
        let lane = match self.shared.encoder_lane.try_lock() {
            Ok(lane) => lane,
            Err(TryLockError::WouldBlock) => return Err(CaptureEncoderError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureEncoderError::Poisoned),
        };
        Ok(lane
            .job
            .as_ref()
            .is_some_and(|job| job.id == id && job.stage_key == stage_key)
            || lane.active == Some((id, stage_key))
            || lane
                .result
                .as_ref()
                .is_some_and(|result| result.id == id && result.stage_key == stage_key))
    }

    #[cfg(test)]
    pub(crate) fn encoder_status(&self) -> CaptureEncoderState {
        if let Some(failure) = self.shared.failure() {
            return CaptureEncoderState::Failed(failure);
        }
        match self.shared.status.try_lock() {
            Ok(status) => status.encoder,
            Err(TryLockError::WouldBlock) => {
                if self.shared.stop_requested.load(Ordering::Acquire) {
                    CaptureEncoderState::Stopping
                } else {
                    CaptureEncoderState::Starting
                }
            }
            Err(TryLockError::Poisoned(_)) => {
                CaptureEncoderState::Failed(CapturePipelineFailure::EncoderPanicked)
            }
        }
    }

    pub(crate) fn request_encoder_stop(&self) {
        self.shared.request_stop();
    }

    pub(crate) fn try_finish_encoder(&mut self) -> CaptureEncoderShutdown {
        self.shared.request_stop();
        if !self.joined {
            let Some(join) = self.join.as_ref() else {
                return CaptureEncoderShutdown::Stopping;
            };
            if !join.is_finished() {
                return CaptureEncoderShutdown::Stopping;
            }
            let join = self.join.take().expect("checked above");
            if join.join().is_err() {
                self.shared
                    .mark_failure_nonblocking(CapturePipelineFailure::EncoderPanicked);
            }
            self.joined = true;
        }
        // The thread is known finished; this status update stays nonwaiting for the CPU owner.
        self.shared.try_mark_worker_exited();
        self.shared.shutdown_state()
    }
}

impl Drop for CaptureCpuWorkerPort {
    fn drop(&mut self) {
        self.shared.request_stop();
        // Dropping a JoinHandle detaches; cleanup remains owned by the encoder thread/shared
        // registry and no actor/UI caller blocks waiting for it.
    }
}

/// Cloneable host-facing metadata and artifact-acquisition port.
#[derive(Clone)]
pub(crate) struct CaptureControlPort {
    shared: Arc<PipelineShared>,
}

impl CaptureControlPort {
    pub(crate) fn try_status(&self) -> Result<CapturePipelineStatus, CaptureStatusReadError> {
        self.shared.try_status()
    }

    pub(crate) fn try_acquire_artifact(
        &self,
        capability: &CaptureArtifactCapability,
    ) -> Result<NativeCaptureArtifactLease, NativeArtifactAccessError> {
        if capability.session() != self.shared.session {
            return Err(NativeArtifactAccessError::ForeignSession);
        }
        let artifacts = match self.shared.artifacts.try_lock() {
            Ok(artifacts) => artifacts,
            Err(TryLockError::WouldBlock) => return Err(NativeArtifactAccessError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(NativeArtifactAccessError::Poisoned),
        };
        let Some(entry) = artifacts.entries.get(&capability.id()) else {
            drop(artifacts);
            return match self.shared.status.try_lock() {
                Ok(status) if status.worker_exited => Err(NativeArtifactAccessError::Stopped),
                Ok(_) => Err(NativeArtifactAccessError::UnknownArtifact),
                Err(TryLockError::WouldBlock) => Err(NativeArtifactAccessError::Busy),
                Err(TryLockError::Poisoned(_)) => Err(NativeArtifactAccessError::Poisoned),
            };
        };
        match entry {
            StoredArtifact::Partial { .. } => Err(NativeArtifactAccessError::NotReady),
            StoredArtifact::Complete {
                weak_interest,
                binding,
                ..
            } => {
                if weak_interest.matches(capability) {
                    Ok(NativeCaptureArtifactLease {
                        capability: capability.clone(),
                        binding: (**binding).clone(),
                    })
                } else if weak_interest.is_live() {
                    Err(NativeArtifactAccessError::UnknownArtifact)
                } else {
                    Err(NativeArtifactAccessError::Expired)
                }
            }
        }
    }
}

/// Capability-bound immutable artifact metadata and explicit local I/O entry points.
pub struct NativeCaptureArtifactLease {
    capability: CaptureArtifactCapability,
    binding: ArtifactBinding,
}

impl NativeCaptureArtifactLease {
    pub fn id(&self) -> CaptureArtifactId {
        self.capability.id()
    }

    pub fn metadata(&self) -> &CaptureFrameMetadata {
        &self.binding.metadata
    }

    pub fn local_spool_path(&self) -> &Path {
        &self.binding.path
    }

    pub fn open(&self) -> Result<NativeCaptureArtifactReader, NativeArtifactIoError> {
        let file = File::open(&self.binding.path).map_err(|_| NativeArtifactIoError::OpenFailed)?;
        Ok(NativeCaptureArtifactReader {
            file,
            _capability: self.capability.clone(),
            _binding: self.binding.clone(),
        })
    }

    pub fn copy_to<W: Write>(&self, writer: &mut W) -> Result<(), NativeArtifactIoError> {
        let mut reader = self.open()?;
        io::copy(&mut reader, writer).map_err(|_| NativeArtifactIoError::CopyFailed)?;
        Ok(())
    }
}

/// Reader whose strong capability prevents worker cleanup while the file is open.
pub struct NativeCaptureArtifactReader {
    file: File,
    _capability: CaptureArtifactCapability,
    _binding: ArtifactBinding,
}

impl Read for NativeCaptureArtifactReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for NativeCaptureArtifactReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl PipelineShared {
    fn new(session: SessionId, frame_slots_limit: usize) -> Self {
        Self {
            session,
            frame_slots_limit,
            ingress: Mutex::new(Ingress { request: None }),
            transfer_closed: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            failure_code: AtomicU8::new(0),
            publications: Mutex::new(PublicationRegistry {
                entries: Vec::new(),
            }),
            artifacts: Mutex::new(ArtifactStore {
                entries: HashMap::new(),
            }),
            spool_root: Mutex::new(None),
            status: Mutex::new(CapturePipelineStatus {
                session,
                renderer_state: CaptureHandoffState::Starting,
                encoder: CaptureEncoderState::Starting,
                pending_jobs: 0,
                retained_png_bytes: 0,
                cleanup_pending: 0,
                worker_exited: false,
            }),
            encoder_lane: Mutex::new(EncoderLane {
                job: None,
                result: None,
                active: None,
                working: false,
            }),
            encoder_wake: Condvar::new(),
        }
    }

    fn try_status(&self) -> Result<CapturePipelineStatus, CaptureStatusReadError> {
        match self.status.try_lock() {
            Ok(status) => {
                let mut snapshot = status.clone();
                if self.stop_requested.load(Ordering::Acquire)
                    && matches!(
                        snapshot.encoder,
                        CaptureEncoderState::Starting | CaptureEncoderState::Ready
                    )
                {
                    snapshot.encoder = CaptureEncoderState::Stopping;
                }
                if let Some(failure) = self.failure() {
                    snapshot.encoder = CaptureEncoderState::Failed(failure);
                }
                Ok(snapshot)
            }
            Err(TryLockError::WouldBlock) => Err(CaptureStatusReadError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(CaptureStatusReadError::Poisoned),
        }
    }

    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        self.transfer_closed.store(true, Ordering::Release);
        if let Ok(mut status) = self.status.try_lock()
            && !matches!(
                status.encoder,
                CaptureEncoderState::Failed(_) | CaptureEncoderState::Stopped
            )
        {
            status.encoder = CaptureEncoderState::Stopping;
        }
        self.encoder_wake.notify_all();
    }

    fn mark_encoder_ready(&self) {
        if let Ok(mut status) = self.status.lock()
            && status.encoder == CaptureEncoderState::Starting
        {
            status.encoder = CaptureEncoderState::Ready;
        }
    }

    fn mark_failure(&self, failure: CapturePipelineFailure) {
        let code = failure_code(failure);
        let _ = self
            .failure_code
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
        let failure = self.failure().unwrap_or(failure);
        if let Ok(mut status) = self.status.lock() {
            status.encoder = CaptureEncoderState::Failed(failure);
        }
    }

    fn mark_failure_nonblocking(&self, failure: CapturePipelineFailure) {
        let code = failure_code(failure);
        let _ = self
            .failure_code
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
        let failure = self.failure().unwrap_or(failure);
        if let Ok(mut status) = self.status.try_lock() {
            status.encoder = CaptureEncoderState::Failed(failure);
        }
    }

    fn failure(&self) -> Option<CapturePipelineFailure> {
        failure_from_code(self.failure_code.load(Ordering::Acquire))
    }

    fn mark_worker_exited(&self) -> bool {
        if !self.cleanup_disposition_ready() {
            return false;
        }
        let mut status = match self.status.lock() {
            Ok(status) => status,
            Err(poisoned) => poisoned.into_inner(),
        };
        status.worker_exited = true;
        status.encoder = self
            .failure()
            .map_or(CaptureEncoderState::Stopped, CaptureEncoderState::Failed);
        status.cleanup_pending = 0;
        status.retained_png_bytes = 0;
        true
    }

    fn try_mark_worker_exited(&self) -> bool {
        if !self.cleanup_disposition_ready() {
            return false;
        }
        let mut status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock) => return false,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        status.worker_exited = true;
        status.encoder = self
            .failure()
            .map_or(CaptureEncoderState::Stopped, CaptureEncoderState::Failed);
        status.cleanup_pending = 0;
        status.retained_png_bytes = 0;
        true
    }

    fn shutdown_state(&self) -> CaptureEncoderShutdown {
        if !self.cleanup_disposition_ready() {
            return CaptureEncoderShutdown::Stopping;
        }
        let status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock) => return CaptureEncoderShutdown::Stopping,
            Err(TryLockError::Poisoned(poisoned)) => {
                let status = poisoned.into_inner();
                return if status.worker_exited {
                    CaptureEncoderShutdown::Failed(
                        self.failure()
                            .unwrap_or(CapturePipelineFailure::EncoderPanicked),
                    )
                } else {
                    CaptureEncoderShutdown::Stopping
                };
            }
        };
        if !status.worker_exited {
            return CaptureEncoderShutdown::Stopping;
        }
        drop(status);
        if !self.cleanup_disposition_ready() {
            return CaptureEncoderShutdown::Stopping;
        }
        let status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock) => return CaptureEncoderShutdown::Stopping,
            Err(TryLockError::Poisoned(poisoned)) => {
                let status = poisoned.into_inner();
                return if status.worker_exited && self.cleanup_disposition_ready() {
                    CaptureEncoderShutdown::Failed(
                        self.failure()
                            .unwrap_or(CapturePipelineFailure::EncoderPanicked),
                    )
                } else {
                    CaptureEncoderShutdown::Stopping
                };
            }
        };
        let encoder = self
            .failure()
            .map_or(status.encoder, CaptureEncoderState::Failed);
        match encoder {
            CaptureEncoderState::Failed(failure) => CaptureEncoderShutdown::Failed(failure),
            CaptureEncoderState::Stopped if status.cleanup_pending == 0 => {
                CaptureEncoderShutdown::Stopped
            }
            CaptureEncoderState::Stopped => {
                CaptureEncoderShutdown::Failed(CapturePipelineFailure::CleanupFailed)
            }
            _ => CaptureEncoderShutdown::Stopping,
        }
    }

    fn refresh_metrics(&self) {
        let (retained, mut cleanup) = match self.artifacts.lock() {
            Ok(artifacts) => {
                let mut retained = 0u64;
                let mut cleanup = 0usize;
                for entry in artifacts.entries.values() {
                    match entry {
                        StoredArtifact::Partial { .. } => cleanup = cleanup.saturating_add(1),
                        StoredArtifact::Complete {
                            weak_interest,
                            binding,
                            ..
                        } => {
                            retained =
                                retained.saturating_add(binding.metadata.png().byte_length());
                            if !weak_interest.is_live() {
                                cleanup = cleanup.saturating_add(1);
                            }
                        }
                    }
                }
                (retained, cleanup)
            }
            Err(poisoned) => {
                let artifacts = poisoned.into_inner();
                (
                    artifacts
                        .entries
                        .values()
                        .fold(0u64, |sum, entry| match entry {
                            StoredArtifact::Complete { binding, .. } => {
                                sum.saturating_add(binding.metadata.png().byte_length())
                            }
                            StoredArtifact::Partial { .. } => sum,
                        }),
                    artifacts.entries.len(),
                )
            }
        };
        if self.stop_requested.load(Ordering::Acquire)
            && self
                .spool_root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        {
            cleanup = cleanup.saturating_add(1);
        }
        let pending = match self.encoder_lane.lock() {
            Ok(lane) => u8::from(
                lane.job.is_some()
                    || lane.result.is_some()
                    || lane.working
                    || lane.active.is_some(),
            ),
            Err(poisoned) => {
                let lane = poisoned.into_inner();
                u8::from(
                    lane.job.is_some()
                        || lane.result.is_some()
                        || lane.working
                        || lane.active.is_some(),
                )
            }
        };
        if let Ok(mut status) = self.status.lock() {
            status.retained_png_bytes = retained;
            status.cleanup_pending = cleanup.min(u8::MAX as usize) as u8;
            status.pending_jobs = pending;
        }
    }

    fn refresh_metrics_nonblocking(&self) {
        let artifacts = match self.artifacts.try_lock() {
            Ok(artifacts) => artifacts,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return,
        };
        let mut retained = 0u64;
        let mut cleanup = 0usize;
        for entry in artifacts.entries.values() {
            match entry {
                StoredArtifact::Partial { .. } => cleanup = cleanup.saturating_add(1),
                StoredArtifact::Complete {
                    weak_interest,
                    binding,
                    ..
                } => {
                    retained = retained.saturating_add(binding.metadata.png().byte_length());
                    if !weak_interest.is_live() {
                        cleanup = cleanup.saturating_add(1);
                    }
                }
            }
        }
        drop(artifacts);
        if self.stop_requested.load(Ordering::Acquire) {
            match self.spool_root.try_lock() {
                Ok(root) if root.is_some() => cleanup = cleanup.saturating_add(1),
                Ok(_) => {}
                Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return,
            }
        }
        let pending = match self.encoder_lane.try_lock() {
            Ok(lane) => u8::from(
                lane.job.is_some()
                    || lane.result.is_some()
                    || lane.working
                    || lane.active.is_some(),
            ),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return,
        };
        let mut status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return,
        };
        status.retained_png_bytes = retained;
        status.cleanup_pending = cleanup.min(u8::MAX as usize) as u8;
        status.pending_jobs = pending;
    }

    fn install_spool_root(&self, root: PathBuf) {
        let mut current = self
            .spool_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Some(root);
    }

    fn spool_root(&self) -> Option<PathBuf> {
        self.spool_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn clear_spool_root(&self) {
        let mut root = self
            .spool_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *root = None;
    }

    fn try_remove_spool_root(&self) -> bool {
        let Some(root) = self.spool_root() else {
            return true;
        };
        match fs::remove_dir(&root) {
            Ok(()) => {
                self.clear_spool_root();
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.clear_spool_root();
                true
            }
            Err(_) => {
                self.mark_failure(CapturePipelineFailure::CleanupFailed);
                false
            }
        }
    }

    fn cleanup_disposition_ready(&self) -> bool {
        let artifacts_empty =
            matches!(self.artifacts.try_lock(), Ok(artifacts) if artifacts.entries.is_empty());
        if !artifacts_empty {
            return false;
        }
        let root_absent = matches!(self.spool_root.try_lock(), Ok(root) if root.is_none());
        if !root_absent {
            return false;
        }
        matches!(self.encoder_lane.try_lock(), Ok(lane) if lane.job.is_none() && lane.active.is_none() && !lane.working)
    }
}

/// Start the encoder worker and create the one-shot CPU port without doing filesystem I/O here.
pub(crate) fn start_capture_pipeline(
    session: SessionId,
    frame_slots_limit: usize,
) -> Result<CapturePipelineOwner, CapturePipelineStartError> {
    if frame_slots_limit == 0 {
        return Err(CapturePipelineStartError::InvalidFrameSlotLimit);
    }
    let shared = Arc::new(PipelineShared::new(session, frame_slots_limit));
    let worker_shared = Arc::clone(&shared);
    let join = thread::Builder::new()
        .name("daedalus-capture-encoder".to_owned())
        .spawn(move || {
            run_with_panic_recovery(&worker_shared, || encoder_main(&worker_shared));
        })
        .map_err(CapturePipelineStartError::EncoderThread)?;
    Ok(CapturePipelineOwner {
        shared: Arc::clone(&shared),
        cpu_worker: Some(CaptureCpuWorkerPort {
            shared,
            join: Some(join),
            joined: false,
        }),
    })
}

fn run_with_panic_recovery(shared: &Arc<PipelineShared>, body: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(body)).is_err() {
        shared.mark_failure(CapturePipelineFailure::EncoderPanicked);
        encoder_panic_recovery(shared);
    }
}

fn publication_matches_frame(
    session: SessionId,
    publication: &ProjectPublicationIdentity,
    frame: &CpuFrameHandle,
) -> bool {
    publication.session() == session
        && frame.session() == session
        && publication.project_generation() == frame.project_generation()
        && publication.source_digest() == frame.source_digest()
        && publication.publication_sequence() == frame.publication_sequence()
        && publication.authority_snapshot_sequence()
            == frame.authority_snapshot_sequence().unwrap_or(0)
        && publication.tick() == frame.tick()
}

fn encoder_main(shared: &Arc<PipelineShared>) {
    match create_private_spool_root(shared) {
        Ok(()) => {}
        Err(_) => {
            shared.mark_failure(CapturePipelineFailure::SpoolUnavailable);
            shared.request_stop();
            encoder_panic_recovery(shared);
            return;
        }
    }
    shared.mark_encoder_ready();
    encoder_loop(shared, false);
}

fn encoder_loop(shared: &Arc<PipelineShared>, recovery_only: bool) {
    loop {
        if !recovery_only {
            let job = take_encoder_job(shared);
            if let Some(job) = job {
                let root = shared
                    .spool_root()
                    .expect("spool root installed before readiness");
                process_encoder_job(shared, &root, job);
                continue;
            }
        }
        cleanup_expired_artifacts(shared);
        shared.refresh_metrics();
        if shared.stop_requested.load(Ordering::Acquire) {
            fail_unstarted_encoder_job(shared);
            cleanup_expired_artifacts(shared);
            if artifact_store_is_empty(shared)
                && shared.try_remove_spool_root()
                && shared.mark_worker_exited()
            {
                return;
            }
        }
        let guard = match shared.encoder_lane.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = shared
            .encoder_wake
            .wait_timeout(guard, ENCODER_POLL_INTERVAL)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn encoder_panic_recovery(shared: &Arc<PipelineShared>) {
    shared.request_stop();
    fail_unstarted_encoder_job(shared);
    encoder_loop(shared, true);
}

fn fail_unstarted_encoder_job(shared: &PipelineShared) {
    let mut lane = match shared.encoder_lane.lock() {
        Ok(lane) => lane,
        Err(poisoned) => poisoned.into_inner(),
    };
    if lane.result.is_none() {
        let failed = lane
            .job
            .as_ref()
            .map(|job| (job.id, job.stage_key))
            .or(lane.active);
        if let Some((id, stage_key)) = failed {
            lane.result = Some(CaptureEncodeResult {
                id,
                stage_key,
                result: Err(CaptureFailure::ArtifactFailed),
            });
        }
    }
    drop(lane.job.take());
    lane.active = None;
    lane.working = false;
    drop(lane);
    shared.encoder_wake.notify_all();
}

fn take_encoder_job(shared: &PipelineShared) -> Option<Box<CaptureEncodeJob>> {
    let mut lane = match shared.encoder_lane.lock() {
        Ok(lane) => lane,
        Err(poisoned) => poisoned.into_inner(),
    };
    if lane.result.is_some() || lane.working {
        return None;
    }
    let pending = lane.job.as_ref()?;
    lane.active = Some((pending.id, pending.stage_key));
    lane.working = true;
    let job = lane.job.take()?;
    drop(lane);
    shared.refresh_metrics();
    Some(job)
}

fn process_encoder_job(shared: &PipelineShared, root: &Path, job: Box<CaptureEncodeJob>) {
    let id = job.id;
    let stage_key = job.stage_key;
    let output = catch_unwind(AssertUnwindSafe(|| encode_job(shared, root, job)));
    let result = match output {
        Ok(Ok(metadata)) => Ok(metadata),
        Ok(Err(failure)) => Err(failure),
        Err(_) => {
            shared.mark_failure(CapturePipelineFailure::EncoderPanicked);
            Err(CaptureFailure::ArtifactFailed)
        }
    };
    let mut lane = match shared.encoder_lane.lock() {
        Ok(lane) => lane,
        Err(poisoned) => poisoned.into_inner(),
    };
    lane.working = false;
    if lane.result.is_none() {
        lane.result = Some(CaptureEncodeResult {
            id,
            stage_key,
            result,
        });
    }
    lane.active = None;
    drop(lane);
    shared.refresh_metrics();
}

fn encode_job(
    shared: &PipelineShared,
    root: &Path,
    job: Box<CaptureEncodeJob>,
) -> Result<CaptureFrameMetadata, CaptureFailure> {
    let nonce = SPOOL_SEQUENCE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| CaptureFailure::ArtifactFailed)?;
    let partial_path = root.join(format!("capture-{nonce:016x}.partial"));
    let final_path = root.join(format!("capture-{nonce:016x}.png"));
    {
        let mut artifacts = shared
            .artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if artifacts.entries.contains_key(&job.id) {
            return Err(CaptureFailure::ArtifactFailed);
        }
        artifacts.entries.insert(
            job.id,
            StoredArtifact::Partial {
                partial_path: partial_path.clone(),
                final_path: final_path.clone(),
                _work: job.work_lease.clone(),
            },
        );
    }
    shared.refresh_metrics();

    let file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial_path)
    {
        Ok(file) => file,
        Err(_) => return Err(CaptureFailure::ArtifactFailed),
    };
    let sync_file = file
        .try_clone()
        .map_err(|_| CaptureFailure::ArtifactFailed)?;
    let (byte_length, digest) = write_png(
        file,
        job.view.physical_width(),
        job.view.physical_height(),
        &job.rgba,
        MAX_PNG_BYTES,
    )?;
    sync_file
        .sync_all()
        .map_err(|_| CaptureFailure::ArtifactFailed)?;
    drop(sync_file);
    fs::rename(&partial_path, &final_path).map_err(|_| CaptureFailure::ArtifactFailed)?;
    let png_metadata = CapturePngMetadata::new(
        job.view.physical_width(),
        job.view.physical_height(),
        byte_length,
        digest,
    )
    .map_err(map_metadata_failure)?;
    debug_assert_eq!(png_metadata.format(), CapturePngFormat::Rgba8Srgb);
    let metadata = CaptureFrameMetadata::new(
        job.publication.clone(),
        job.present_sequence,
        job.view.clone(),
        job.surface_format,
        job.surface_encoding,
        png_metadata,
    )
    .map_err(map_metadata_failure)?;
    let replacement = StoredArtifact::Complete {
        weak_interest: job.interest.clone(),
        _work: job.work_lease.clone(),
        binding: Box::new(ArtifactBinding {
            path: final_path,
            metadata: metadata.clone(),
        }),
    };
    let replaced = {
        let mut artifacts = shared
            .artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            artifacts.entries.get(&job.id),
            Some(StoredArtifact::Partial { .. })
        ) {
            return Err(CaptureFailure::ArtifactFailed);
        }
        artifacts.entries.insert(job.id, replacement)
    };
    drop(replaced);
    shared.refresh_metrics();
    Ok(metadata)
}

fn write_png(
    file: File,
    width: u32,
    height: u32,
    rgba: &[u8],
    max_bytes: u64,
) -> Result<(u64, [u8; 32]), CaptureFailure> {
    let stats = Arc::new(Mutex::new(WriteStats::default()));
    let cap_hit = Arc::new(AtomicBool::new(false));
    let writer = CappedHashWriter {
        file,
        stats: Arc::clone(&stats),
        cap_hit: Arc::clone(&cap_hit),
        max_bytes,
    };
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut png_writer = encoder
        .write_header()
        .map_err(|_| encode_io_failure(&cap_hit))?;
    let mut image_data = png_writer
        .stream_writer_with_size(PNG_STREAM_CHUNK_BYTES)
        .map_err(|_| encode_io_failure(&cap_hit))?;
    image_data
        .write_all(rgba)
        .map_err(|_| encode_io_failure(&cap_hit))?;
    image_data
        .finish()
        .map_err(|_| encode_io_failure(&cap_hit))?;
    png_writer
        .finish()
        .map_err(|_| encode_io_failure(&cap_hit))?;
    let (byte_length, digest) = {
        let stats = stats.lock().map_err(|_| CaptureFailure::ArtifactFailed)?;
        (stats.bytes, stats.hasher.clone().finalize().into())
    };
    if byte_length == 0 || byte_length > max_bytes {
        return Err(CaptureFailure::TooLarge);
    }
    Ok((byte_length, digest))
}

fn map_metadata_failure(error: CaptureMetadataError) -> CaptureFailure {
    match error {
        CaptureMetadataError::TooLarge => CaptureFailure::TooLarge,
        CaptureMetadataError::InvalidView | CaptureMetadataError::InconsistentPng => {
            CaptureFailure::ArtifactFailed
        }
    }
}

fn encode_io_failure(cap_hit: &AtomicBool) -> CaptureFailure {
    if cap_hit.load(Ordering::Acquire) {
        CaptureFailure::TooLarge
    } else {
        CaptureFailure::ArtifactFailed
    }
}

#[derive(Default)]
struct WriteStats {
    bytes: u64,
    hasher: Sha256,
}

struct CappedHashWriter<W: Write> {
    file: W,
    stats: Arc<Mutex<WriteStats>>,
    cap_hit: Arc<AtomicBool>,
    max_bytes: u64,
}

impl<W: Write> Write for CappedHashWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut stats = self
            .stats
            .lock()
            .map_err(|_| io::Error::other("capture hash state unavailable"))?;
        let remaining = self.max_bytes.saturating_sub(stats.bytes);
        if u64::try_from(buffer.len()).unwrap_or(u64::MAX) > remaining {
            self.cap_hit.store(true, Ordering::Release);
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "capture PNG output exceeds its configured byte limit",
            ));
        }
        let written = self.file.write(buffer)?;
        stats.hasher.update(&buffer[..written]);
        stats.bytes = stats.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn create_private_spool_root(shared: &PipelineShared) -> io::Result<()> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let sequence = SPOOL_SEQUENCE
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| io::Error::other("capture spool sequence exhausted"))?;
        let root = base.join(format!(
            "daedalus-capture-{}-{sequence:016x}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => {
                // Publish the cleanup owner immediately after directory creation, before any
                // further filesystem operation can fail or unwind.
                shared.install_spool_root(root.clone());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique private capture directory",
    ))
}

fn cleanup_expired_artifacts(shared: &PipelineShared) {
    cleanup_expired_artifacts_with(shared, remove_all_if_present);
}

fn cleanup_expired_artifacts_with(
    shared: &PipelineShared,
    mut remove_paths: impl FnMut(&[PathBuf]) -> Result<(), ()>,
) {
    let expired = {
        let artifacts = match shared.artifacts.lock() {
            Ok(artifacts) => artifacts,
            Err(poisoned) => poisoned.into_inner(),
        };
        artifacts
            .entries
            .iter()
            .filter_map(|(id, entry)| match entry {
                StoredArtifact::Complete { weak_interest, .. } if !weak_interest.is_live() => {
                    let StoredArtifact::Complete { binding, .. } = entry else {
                        unreachable!()
                    };
                    Some((*id, vec![binding.path.clone()]))
                }
                StoredArtifact::Partial {
                    partial_path,
                    final_path,
                    ..
                } => Some((*id, vec![partial_path.clone(), final_path.clone()])),
                StoredArtifact::Complete { .. } => None,
            })
            .collect::<Vec<_>>()
    };
    for (id, paths) in expired {
        match remove_paths(&paths) {
            Ok(()) => {
                let mut artifacts = match shared.artifacts.lock() {
                    Ok(artifacts) => artifacts,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let removable = match artifacts.entries.get(&id) {
                    Some(StoredArtifact::Partial {
                        partial_path,
                        final_path,
                        ..
                    }) => {
                        paths.len() == 2
                            && paths[0].as_path() == partial_path.as_path()
                            && paths[1].as_path() == final_path.as_path()
                    }
                    Some(StoredArtifact::Complete {
                        weak_interest,
                        binding,
                        ..
                    }) => {
                        !weak_interest.is_live()
                            && paths.len() == 1
                            && paths[0].as_path() == binding.path.as_path()
                    }
                    None => false,
                };
                if removable {
                    artifacts.entries.remove(&id);
                }
            }
            Err(()) => {
                shared.mark_failure(CapturePipelineFailure::CleanupFailed);
            }
        }
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), ()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(()),
    }
}

fn remove_all_if_present(paths: &[PathBuf]) -> Result<(), ()> {
    for path in paths {
        remove_file_if_present(path)?;
    }
    Ok(())
}

fn artifact_store_is_empty(shared: &PipelineShared) -> bool {
    match shared.artifacts.try_lock() {
        Ok(artifacts) => artifacts.entries.is_empty(),
        Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => false,
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use daedalus_runtime::{CaptureCompletion, CaptureProjection, WaitStage};

    use crate::capture::test_support::RuntimeCaptureFixture;

    fn temporary_file(label: &str) -> (PathBuf, File) {
        for _ in 0..32 {
            let sequence = SPOOL_SEQUENCE
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                })
                .expect("test file sequence remains available");
            let path = std::env::temp_dir().join(format!(
                "daedalus-capture-test-{}-{label}-{sequence:016x}",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return (path, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated capture test file: {error}"),
            }
        }
        panic!("could not allocate an isolated capture test file")
    }

    struct TestFile(PathBuf);

    impl TestFile {
        fn create(label: &str) -> (Self, File) {
            let (path, file) = temporary_file(label);
            (Self(path), file)
        }

        fn read(&self) -> Vec<u8> {
            fs::read(&self.0).expect("read isolated capture test file")
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create(label: &str) -> Self {
            let sequence = SPOOL_SEQUENCE
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                })
                .expect("test directory sequence remains available");
            let path = std::env::temp_dir().join(format!(
                "daedalus-capture-test-{}-{label}-{sequence:016x}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create isolated capture test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_view() -> CaptureView {
        CaptureView::new(
            2,
            2,
            [0.0, 0.0, 3.0],
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            1.0,
            0.1,
            100.0,
            CaptureProjection::Perspective,
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
        )
        .expect("small valid capture view")
    }

    fn wait_encoder_ready(worker: &CaptureCpuWorkerPort) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.encoder_status() {
                CaptureEncoderState::Ready => return,
                CaptureEncoderState::Failed(failure) => {
                    panic!("capture encoder failed during startup: {failure:?}");
                }
                CaptureEncoderState::Starting | CaptureEncoderState::Stopping => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "encoder startup timed out"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                CaptureEncoderState::Stopped => panic!("encoder stopped before readiness"),
            }
        }
    }

    fn submit_encode_with_retry(worker: &CaptureCpuWorkerPort, mut job: Box<CaptureEncodeJob>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.try_submit_encode(job) {
                Ok(()) => return,
                Err((returned, CaptureEncoderError::Busy)) => {
                    job = returned;
                    assert!(
                        std::time::Instant::now() < deadline,
                        "encoder lane stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err((_, error)) => panic!("capture encode admission failed: {error:?}"),
            }
        }
    }

    fn take_encode_result(worker: &CaptureCpuWorkerPort) -> CaptureEncodeResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.try_take_encode_result() {
                Ok(Some(result)) => return result,
                Ok(None) | Err(CaptureEncoderError::Busy) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "encoder result timed out"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("capture result lane failed: {error:?}"),
            }
        }
    }

    fn assert_encode_job_pending(
        worker: &CaptureCpuWorkerPort,
        id: CaptureArtifactId,
        stage_key: ManagedStageKey,
        expected: bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.try_encode_job_pending(id, stage_key) {
                Ok(value) => {
                    assert_eq!(value, expected);
                    return;
                }
                Err(CaptureEncoderError::Busy) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "encoder state stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("encoder job state query failed: {error:?}"),
            }
        }
    }

    fn stop_encoder(worker: &mut CaptureCpuWorkerPort) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match worker.try_finish_encoder() {
                CaptureEncoderShutdown::Stopped => return,
                CaptureEncoderShutdown::Failed(failure) => {
                    panic!("capture encoder shutdown failed: {failure:?}");
                }
                CaptureEncoderShutdown::Stopping => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "encoder cleanup timed out"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }

    fn acquire_artifact(
        control: &CaptureControlPort,
        capability: &CaptureArtifactCapability,
    ) -> NativeCaptureArtifactLease {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match control.try_acquire_artifact(capability) {
                Ok(lease) => return lease,
                Err(NativeArtifactAccessError::Busy) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "artifact lookup stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("authentic artifact lookup failed: {error:?}"),
            }
        }
    }

    fn acknowledge_ticket(ticket: &mut daedalus_runtime::RequestTicket) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match ticket.acknowledge() {
                Ok(()) => return,
                Err(daedalus_runtime::RequestAckError::Busy) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "ticket acknowledgement stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("capture ticket acknowledgement failed: {error:?}"),
            }
        }
    }

    fn assert_runtime_capture_capacity(client: &daedalus_runtime::SessionControlClient) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match client.try_submit(daedalus_runtime::SessionOperation::CaptureFrame {
                at_least_tick: None,
                timeout: Some(std::time::Duration::from_secs(5)),
            }) {
                Err(daedalus_runtime::SessionSubmitError::CaptureCapacity) => return,
                Err(
                    daedalus_runtime::SessionSubmitError::Busy
                    | daedalus_runtime::SessionSubmitError::QueueFull,
                ) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "capture admission stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Ok(ticket) => {
                    drop(ticket);
                    panic!("capture image slot was unexpectedly free");
                }
                Err(error) => panic!("capture admission failed unexpectedly: {error}"),
            }
        }
    }

    fn complete_capture(
        request: &mut daedalus_runtime::CaptureRequest,
        completion: &CaptureCompletion,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match request.responder_mut().try_complete(completion) {
                Ok(()) => return,
                Err(daedalus_runtime::CaptureResponderError::Busy) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "capture responder stayed busy"
                    );
                    thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("runtime capture completion failed: {error}"),
            }
        }
    }

    #[test]
    fn png_streaming_writes_rgba8_srgb_and_hashes_the_actual_bounded_output() {
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let (file_guard, file) = TestFile::create("png-success");
        let (byte_length, digest) =
            write_png(file, 2, 2, &rgba, 1024 * 1024).expect("small PNG streams successfully");
        let encoded = file_guard.read();
        assert_eq!(u64::try_from(encoded.len()).unwrap(), byte_length);
        let expected_digest: [u8; 32] = Sha256::digest(&encoded).into();
        assert_eq!(expected_digest, digest);

        let mut reader = png::Decoder::new(Cursor::new(&encoded))
            .read_info()
            .expect("decode streamed PNG header");
        let mut decoded = vec![
            0;
            reader
                .output_buffer_size()
                .expect("bounded fixture PNG output size")
        ];
        let info = reader
            .next_frame(&mut decoded)
            .expect("decode streamed PNG pixels");
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&decoded[..info.buffer_size()], &rgba);
    }

    #[test]
    fn png_streaming_cap_stops_before_the_output_file_can_exceed_its_limit() {
        let rgba = vec![127; 128 * 128 * 4];
        let (file_guard, file) = TestFile::create("png-cap");
        assert_eq!(
            write_png(file, 128, 128, &rgba, 64),
            Err(CaptureFailure::TooLarge)
        );
        let bytes = file_guard.read();
        assert!(
            bytes.len() <= 64,
            "streaming cap allowed {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn capped_writer_hashes_only_bytes_accepted_by_the_sink_and_returns_sink_errors() {
        struct FailAfter {
            remaining: usize,
            bytes: Vec<u8>,
        }

        impl Write for FailAfter {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected sink error",
                    ));
                }
                let written = buffer.len().min(self.remaining);
                self.bytes.extend_from_slice(&buffer[..written]);
                self.remaining -= written;
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let stats = Arc::new(Mutex::new(WriteStats::default()));
        let cap_hit = Arc::new(AtomicBool::new(false));
        let mut writer = CappedHashWriter {
            file: FailAfter {
                remaining: 3,
                bytes: Vec::new(),
            },
            stats: Arc::clone(&stats),
            cap_hit: Arc::clone(&cap_hit),
            max_bytes: 8,
        };
        assert_eq!(writer.write(b"abcdef").expect("sink accepts prefix"), 3);
        assert_eq!(
            writer
                .write(b"def")
                .expect_err("injected sink error is preserved")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(!cap_hit.load(Ordering::Acquire));
        assert_eq!(writer.file.bytes, b"abc");
        let stats = stats.lock().expect("read bounded writer stats");
        assert_eq!(stats.bytes, 3);
        let expected: [u8; 32] = Sha256::digest(b"abc").into();
        assert_eq!(
            stats.hasher.clone().finalize().as_slice(),
            expected.as_slice()
        );
    }

    #[test]
    fn publication_registry_binds_real_cpu_frame_pointer_and_releases_expired_weak_slot() {
        let mut fixture = RuntimeCaptureFixture::new();
        let mut cpu_frame = fixture.take_cpu_frame();
        let publication = cpu_frame.publication.clone();
        let frame = cpu_frame.take_frame();
        let selected = Arc::downgrade(&frame);
        let shared = Arc::new(PipelineShared::new(publication.session(), 1));
        let coordinator = CaptureCoordinatorPort {
            shared: Arc::clone(&shared),
        };
        let cpu_port = CaptureCpuWorkerPort {
            shared: Arc::clone(&shared),
            join: None,
            joined: true,
        };

        coordinator
            .try_register_publication(publication.clone(), ManagedStageKey::INITIAL, &frame)
            .expect("bind actual CPU frame to actual runtime publication");
        coordinator
            .try_register_publication(publication.clone(), ManagedStageKey::INITIAL, &frame)
            .expect("same-pointer registration is idempotent");
        let binding = cpu_port
            .resolve_selected_frame(&selected)
            .expect("resolve selected frame")
            .expect("registered frame resolves");
        assert_eq!(binding.publication(), &publication);
        assert_eq!(binding.stage_key(), ManagedStageKey::INITIAL);

        let next_key = ManagedStageKey::INITIAL
            .checked_next()
            .expect("next managed key is available");
        assert_eq!(
            coordinator.try_register_publication(publication.clone(), next_key, &frame),
            Err(CaptureRegistryError::Conflict),
            "the same frame pointer cannot be rebound to a different stage key"
        );
        let second_frame = cpu_frame.extract_again();
        assert_eq!(
            coordinator.try_register_publication(
                publication.clone(),
                ManagedStageKey::INITIAL,
                &second_frame,
            ),
            Err(CaptureRegistryError::Full),
            "a second frame cannot exceed the configured live-slot count"
        );

        let foreign = RuntimeCaptureFixture::new();
        assert_eq!(
            coordinator.try_register_publication(
                foreign.publication().clone(),
                ManagedStageKey::INITIAL,
                &second_frame,
            ),
            Err(CaptureRegistryError::Stale),
            "a real publication from another session cannot bind this CPU frame"
        );
        drop(frame);
        assert!(
            cpu_port
                .resolve_selected_frame(&selected)
                .expect("expired weak selection is a normal lookup")
                .is_none()
        );
        coordinator
            .try_register_publication(publication, ManagedStageKey::INITIAL, &second_frame)
            .expect("expired weak slot is reclaimed on the next bounded registration");
    }

    #[test]
    fn partial_cleanup_releases_the_capture_image_work_lease_only_after_removal() {
        let mut fixture = RuntimeCaptureFixture::new();
        let request = fixture.request();
        let request_id = request.id();
        let capability = request.artifact().clone();
        let work = request.work_lease().clone();
        let shared = Arc::new(PipelineShared::new(request_id.session(), 1));
        let control = CaptureControlPort {
            shared: Arc::clone(&shared),
        };
        let root = TestDirectory::create("partial-work-lease");
        shared.install_spool_root(root.0.clone());
        let partial = root.0.join("capture.partial");
        let final_path = root.0.join("capture.png");
        fs::write(&partial, b"partial").expect("create tracked partial output");
        shared
            .artifacts
            .lock()
            .expect("lock artifact store")
            .entries
            .insert(
                request_id,
                StoredArtifact::Partial {
                    partial_path: partial.clone(),
                    final_path: final_path.clone(),
                    _work: work,
                },
            );
        assert!(matches!(
            control.try_acquire_artifact(&capability),
            Err(NativeArtifactAccessError::NotReady)
        ));

        let mut request = fixture
            .take_request()
            .expect("fixture owns its pending request");
        let completion = CaptureCompletion::Failed(CaptureFailure::Cancelled);
        complete_capture(&mut request, &completion);
        let mut ticket = fixture.take_ticket();
        let progress = ticket
            .wait_for(WaitStage::Terminal, std::time::Duration::from_secs(5))
            .expect("cancelled actor ticket reaches terminal state");
        assert!(matches!(
            progress.terminal(),
            Some(Err(daedalus_runtime::SessionRequestError::CaptureFailed(
                CaptureFailure::Cancelled
            )))
        ));
        acknowledge_ticket(&mut ticket);
        drop(progress);
        drop(ticket);
        drop(request);
        drop(capability);

        assert_runtime_capture_capacity(fixture.client());
        cleanup_expired_artifacts(&shared);
        assert!(!partial.exists(), "cleanup left the partial file");
        assert!(!final_path.exists(), "cleanup left the replacement path");
        assert!(
            shared
                .artifacts
                .lock()
                .expect("read cleaned artifact store")
                .entries
                .is_empty()
        );
        assert!(shared.try_remove_spool_root());
        assert!(!root.0.exists(), "empty spool root remains after cleanup");

        let next = fixture.submit_next_capture();
        assert_eq!(next.client(), request_id.ticket().client());
        assert_eq!(next.sequence(), request_id.ticket().sequence() + 1);
        let mut request = fixture
            .take_request()
            .expect("next request is delivered once");
        complete_capture(
            &mut request,
            &CaptureCompletion::Failed(CaptureFailure::Cancelled),
        );
        let mut ticket = fixture.take_ticket();
        let progress = ticket
            .wait_for(WaitStage::Terminal, std::time::Duration::from_secs(5))
            .expect("follow-up request reaches terminal");
        acknowledge_ticket(&mut ticket);
        drop(progress);
    }

    #[test]
    fn encoded_artifact_reader_holds_real_capability_until_file_cleanup() {
        let mut fixture = RuntimeCaptureFixture::new();
        let request = fixture.request();
        let session = request.id().session();
        let capability = request.artifact().clone();
        let view = fixture_view();
        let pixels = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let job = CaptureEncodeJob::new(
            request.id(),
            ManagedStageKey::INITIAL,
            fixture.publication().clone(),
            1,
            view,
            CaptureSurfaceFormat::Rgba8UnormSrgb,
            CapturePixelEncoding::SrgbEncoded,
            capability.downgrade(),
            request.work_lease().clone(),
            pixels.clone(),
        )
        .expect("actor-created capability admits bounded sRGB pixels");

        let mut pipeline = start_capture_pipeline(session, 1).expect("start bounded encoder");
        let control = pipeline.control_port();
        let mut worker = pipeline
            .take_cpu_worker_port()
            .expect("take encoder worker port once");
        wait_encoder_ready(&worker);
        assert_encode_job_pending(&worker, request.id(), ManagedStageKey::INITIAL, false);
        submit_encode_with_retry(&worker, job);
        assert_encode_job_pending(&worker, request.id(), ManagedStageKey::INITIAL, true);
        let result = take_encode_result(&worker);
        assert_eq!(result.id(), request.id());
        assert_eq!(result.stage_key(), ManagedStageKey::INITIAL);
        let metadata = result
            .result()
            .expect("valid pixel input produces an artifact")
            .clone();
        assert_eq!(metadata.publication(), fixture.publication());
        assert_eq!(metadata.png().format(), CapturePngFormat::Rgba8Srgb);

        let artifact = acquire_artifact(&control, &capability);
        let path = artifact.local_spool_path().to_path_buf();
        let mut reader = artifact.open().expect("reader opens completed PNG");
        let mut encoded = Vec::new();
        reader
            .read_to_end(&mut encoded)
            .expect("read completed test PNG");
        assert_eq!(encoded.len() as u64, metadata.png().byte_length());
        let expected_digest: [u8; 32] = Sha256::digest(&encoded).into();
        assert_eq!(&expected_digest, metadata.png().sha256());
        let mut decoder = png::Decoder::new(Cursor::new(&encoded))
            .read_info()
            .expect("decode actual encoder output");
        let mut decoded = vec![
            0;
            decoder
                .output_buffer_size()
                .expect("bounded fixture PNG output size")
        ];
        let decoded_info = decoder
            .next_frame(&mut decoded)
            .expect("decode actual encoder pixels");
        assert_eq!(decoded_info.width, 2);
        assert_eq!(decoded_info.height, 2);
        assert_eq!(&decoded[..decoded_info.buffer_size()], &pixels);

        let completion = CaptureCompletion::Captured(Box::new(metadata));
        let mut request = fixture
            .take_request()
            .expect("fixture owns one actor request");
        complete_capture(&mut request, &completion);
        let mut ticket = fixture.take_ticket();
        let progress = ticket
            .wait_for(WaitStage::Terminal, std::time::Duration::from_secs(5))
            .expect("capture ticket reaches terminal result");
        acknowledge_ticket(&mut ticket);
        drop(progress);
        drop(ticket);
        drop(request);
        drop(artifact);
        drop(capability);
        assert_runtime_capture_capacity(fixture.client());
        assert_encode_job_pending(&worker, result.id(), result.stage_key(), false);

        pipeline.request_stop();
        assert_eq!(
            worker.try_finish_encoder(),
            CaptureEncoderShutdown::Stopping,
            "open artifact reader retains the file and worker cleanup owner"
        );
        assert!(path.exists(), "reader-held artifact was removed early");
        drop(reader);
        stop_encoder(&mut worker);
        assert!(
            !path.exists(),
            "artifact file remains after reader release and cleanup"
        );
        let next_id = fixture.submit_next_capture();
        assert_eq!(next_id.client(), result.id().ticket().client());
        assert_eq!(next_id.sequence(), result.id().ticket().sequence() + 1);
        let mut follow_up = fixture
            .take_request()
            .expect("released slot admits follow-up");
        complete_capture(
            &mut follow_up,
            &CaptureCompletion::Failed(CaptureFailure::Cancelled),
        );
        let mut follow_up_ticket = fixture.take_ticket();
        let follow_up_progress = follow_up_ticket
            .wait_for(WaitStage::Terminal, std::time::Duration::from_secs(5))
            .expect("follow-up capture reaches terminal state");
        acknowledge_ticket(&mut follow_up_ticket);
        drop(follow_up_progress);
    }

    #[test]
    fn foreign_session_capability_is_rejected_before_artifact_lookup() {
        let first = RuntimeCaptureFixture::new();
        let second = RuntimeCaptureFixture::new();
        let foreign_capability = first.request().artifact().clone();
        let shared = Arc::new(PipelineShared::new(second.request().id().session(), 1));
        let control = CaptureControlPort { shared };
        assert!(matches!(
            control.try_acquire_artifact(&foreign_capability),
            Err(NativeArtifactAccessError::ForeignSession)
        ));
    }

    #[test]
    fn capture_ingress_returns_same_request_on_busy_and_closed_and_drains_after_fence() {
        let mut first = RuntimeCaptureFixture::new();
        let mut second = RuntimeCaptureFixture::new();
        let shared = Arc::new(PipelineShared::new(first.request().id().session(), 1));
        let coordinator = CaptureCoordinatorPort {
            shared: Arc::clone(&shared),
        };
        let cpu = CaptureCpuWorkerPort {
            shared: Arc::clone(&shared),
            join: None,
            joined: true,
        };
        let mut accepted = Some(first.take_request().expect("first fixture request exists"));
        coordinator
            .try_send_pending(&mut accepted, ManagedStageKey::INITIAL)
            .expect("first real actor request enters the bounded slot");
        assert!(accepted.is_none());
        assert!(
            !coordinator.is_drained(),
            "open transfer fence is not drained"
        );

        let mut pending = Some(
            second
                .take_request()
                .expect("second fixture request exists"),
        );
        let original_pointer =
            pending.as_deref().expect("pending request remains owned") as *const CaptureRequest;
        assert_eq!(
            coordinator.try_send_pending(&mut pending, ManagedStageKey::INITIAL),
            Err(CaptureIngressError::Busy)
        );
        assert_eq!(
            pending.as_deref().expect("busy request remains owned") as *const CaptureRequest,
            original_pointer
        );
        assert_eq!(
            pending.as_deref().unwrap().id().session(),
            second.publication().session()
        );

        coordinator.close_transfer_fence();
        assert_eq!(
            coordinator.try_send_pending(&mut pending, ManagedStageKey::INITIAL),
            Err(CaptureIngressError::Closed)
        );
        assert_eq!(
            pending.as_deref().expect("closed request remains owned") as *const CaptureRequest,
            original_pointer
        );
        assert!(
            !coordinator.is_drained(),
            "accepted ingress request remains outstanding"
        );
        let routed = cpu
            .try_take_request()
            .expect("CPU ingress read succeeds")
            .expect("accepted request was retained");
        assert_eq!(routed.request.id().session(), coordinator.session());
        drop(routed);
        assert!(
            coordinator.is_drained(),
            "closed empty transfer fence is drained"
        );
        drop(pending);
        // Stop also closes the fence, including the owner-abandonment path.
        let open = Arc::new(PipelineShared::new(SessionId::fresh(), 1));
        let open_coordinator = CaptureCoordinatorPort {
            shared: Arc::clone(&open),
        };
        open.request_stop();
        let mut no_request = None;
        assert_eq!(
            open_coordinator.try_send_pending(&mut no_request, ManagedStageKey::INITIAL),
            Err(CaptureIngressError::Closed)
        );
    }

    #[test]
    fn encoder_panic_recovery_removes_registered_spool_before_failed_terminal() {
        let shared = Arc::new(PipelineShared::new(SessionId::fresh(), 1));
        let root_observed = Arc::new(Mutex::new(None));
        let panic_shared = Arc::clone(&shared);
        let panic_root = Arc::clone(&root_observed);
        let worker = thread::spawn(move || {
            run_with_panic_recovery(&panic_shared, || {
                create_private_spool_root(&panic_shared).expect("create private spool");
                *panic_root.lock().unwrap() = panic_shared.spool_root();
                panic!("injected encoder worker failure after durable root registration");
            });
        });
        worker
            .join()
            .expect("outer recovery catches the injected panic");

        let root = root_observed
            .lock()
            .unwrap()
            .clone()
            .expect("recorded spool root");
        assert!(
            !root.exists(),
            "panic recovery left the private spool root behind"
        );
        let status = shared.try_status().expect("read encoder status");
        assert!(
            status.worker_exited(),
            "worker exit must follow completed cleanup"
        );
        assert_eq!(
            status.encoder(),
            CaptureEncoderState::Failed(CapturePipelineFailure::EncoderPanicked)
        );
        assert_eq!(
            shared.shutdown_state(),
            CaptureEncoderShutdown::Failed(CapturePipelineFailure::EncoderPanicked)
        );
    }

    #[test]
    fn cleanup_panic_keeps_partial_file_owner_until_recovery_removes_it() {
        let fixture = RuntimeCaptureFixture::new();
        let request = fixture.request();
        let session = request.id().session();
        let root = TestDirectory::create("cleanup-panic");
        let partial = root.0.join("capture.partial");
        let final_path = root.0.join("capture.png");
        fs::write(&partial, b"partial png bytes").expect("create partial file");
        fs::write(&final_path, b"replacement path after rename").expect("create renamed file");
        let shared = Arc::new(PipelineShared::new(session, 1));
        shared.install_spool_root(root.0.clone());
        shared
            .artifacts
            .lock()
            .expect("lock artifact store")
            .entries
            .insert(
                request.id(),
                StoredArtifact::Partial {
                    partial_path: partial.clone(),
                    final_path: final_path.clone(),
                    _work: request.work_lease().clone(),
                },
            );

        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            run_with_panic_recovery(&worker_shared, || {
                cleanup_expired_artifacts_with(&worker_shared, |_| {
                    panic!("injected unwind during partial artifact cleanup")
                });
            });
        });
        worker
            .join()
            .expect("outer recovery contains cleanup panic");

        assert!(!partial.exists(), "partial file retained after recovery");
        assert!(
            !final_path.exists(),
            "renamed final file retained after recovery"
        );
        assert!(!root.0.exists(), "spool root removed after its entries");
        let status = shared.try_status().expect("read recovered worker status");
        assert!(
            status.worker_exited(),
            "cleanup completes before worker exit"
        );
        assert_eq!(
            status.encoder(),
            CaptureEncoderState::Failed(CapturePipelineFailure::EncoderPanicked)
        );
        assert_eq!(
            shared.shutdown_state(),
            CaptureEncoderShutdown::Failed(CapturePipelineFailure::EncoderPanicked)
        );
    }
}
