//! Renderer-neutral ownership and handoff types for one live frame capture.
//!
//! This module does not own GPU buffers, PNG bytes, or paths. The runtime owns request
//! identity and the single image reservation; a native consumer owns actual readback and
//! artifact work through the small leases defined here.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use daedalus_core::{SessionId, Tick};
use thiserror::Error;

use crate::project_generation_consumer::ProjectPublicationIdentity;
use crate::session_control::RequestTicketId;
use crate::session_primitives::{CreditLease, CreditLimits, CreditPool};

pub const DEFAULT_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_CAPTURE_DIMENSION: u32 = 8_192;
pub const MAX_CAPTURE_PIXELS: u64 = 8_388_608;
pub const MAX_CAPTURE_RAW_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_CAPTURE_PNG_BYTES: u64 = 40 * 1024 * 1024;

const HANDOFF_STARTING: u8 = 0;
const HANDOFF_READY: u8 = 1;
const HANDOFF_UNSUPPORTED: u8 = 2;
const HANDOFF_DETACHED: u8 = 3;
const COMPLETION_OPEN: u8 = 0;
const COMPLETION_WRITTEN: u8 = 1;
const COMPLETION_RECEIVER_CLOSED: u8 = 2;

/// Supported native surface storage. The renderer reports the actual surface format; it does
/// not infer this from an adapter name or from the PNG output format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureSurfaceFormat {
    Rgba8Unorm,
    Rgba8UnormSrgb,
    Bgra8Unorm,
    Bgra8UnormSrgb,
}

/// The bounded information a capture consumer reports when its real readback path is ready.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureReadyInfo {
    surface_format: CaptureSurfaceFormat,
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
}

impl CaptureReadyInfo {
    pub fn new(
        surface_format: CaptureSurfaceFormat,
        max_width: u32,
        max_height: u32,
        max_pixels: u64,
    ) -> Result<Self, CaptureHandoffError> {
        let pixels = u64::from(max_width)
            .checked_mul(u64::from(max_height))
            .ok_or(CaptureHandoffError::InvalidReadyInfo)?;
        if max_width == 0
            || max_height == 0
            || max_width > MAX_CAPTURE_DIMENSION
            || max_height > MAX_CAPTURE_DIMENSION
            || max_pixels == 0
            || max_pixels > MAX_CAPTURE_PIXELS
            || pixels < max_pixels
        {
            return Err(CaptureHandoffError::InvalidReadyInfo);
        }
        Ok(Self {
            surface_format,
            max_width,
            max_height,
            max_pixels,
        })
    }

    #[must_use]
    pub const fn surface_format(self) -> CaptureSurfaceFormat {
        self.surface_format
    }

    #[must_use]
    pub const fn max_width(self) -> u32 {
        self.max_width
    }

    #[must_use]
    pub const fn max_height(self) -> u32 {
        self.max_height
    }

    #[must_use]
    pub const fn max_pixels(self) -> u64 {
        self.max_pixels
    }
}

/// Fixed reason a native consumer cannot provide live capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureUnsupportedReason {
    NativeBackendUnavailable,
    SurfaceCopyUnavailable,
    UnsupportedSurfaceFormat,
    CaptureDisabled,
}

/// Current state of the one renderer-neutral capture handoff.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureHandoffState {
    Starting,
    Ready(CaptureReadyInfo),
    Unsupported(CaptureUnsupportedReason),
    Detached,
}

/// Nonwaiting failures from the bounded native capture handoff.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CaptureHandoffError {
    #[error("capture handoff is busy; retry without waiting")]
    Busy,
    #[error("capture handoff is already resolved")]
    AlreadyResolved,
    #[error("capture handoff is detached")]
    Detached,
    #[error("capture ready information is invalid or exceeds the capture limits")]
    InvalidReadyInfo,
    #[error("capture request slot is occupied")]
    RequestOccupied,
    #[error("capture handoff state was poisoned")]
    Poisoned,
}

/// One accepted request's opaque artifact key. The full ticket ID is retained because ticket
/// sequences are per client, not per session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CaptureArtifactId(RequestTicketId);

impl CaptureArtifactId {
    pub(crate) const fn new(ticket: RequestTicketId) -> Self {
        Self(ticket)
    }

    #[must_use]
    pub const fn ticket(self) -> RequestTicketId {
        self.0
    }

    #[must_use]
    pub const fn session(self) -> SessionId {
        self.0.session()
    }
}

/// The source binding frozen by the actor when it dispatches the capture request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSourceIdentity {
    session: SessionId,
    project_generation: u64,
    source_digest: String,
}

impl CaptureSourceIdentity {
    pub(crate) fn new(session: SessionId, project_generation: u64, source_digest: &str) -> Self {
        Self {
            session,
            project_generation,
            source_digest: source_digest.to_owned(),
        }
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn project_generation(&self) -> u64 {
        self.project_generation
    }

    #[must_use]
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub(crate) fn matches_publication(&self, publication: &ProjectPublicationIdentity) -> bool {
        self.session == publication.session()
            && self.project_generation == publication.project_generation()
            && self.source_digest == publication.source_digest()
    }
}

/// A shared reservation for the single image slot in one session.
#[derive(Clone)]
pub struct CaptureImageLease {
    _credit: CreditLease,
}

impl fmt::Debug for CaptureImageLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureImageLease(<reserved>)")
    }
}

/// An independent hold on the same image slot while native readback/artifact work remains live.
#[derive(Clone, Debug)]
pub struct CaptureWorkLease {
    image: CaptureImageLease,
}

impl CaptureWorkLease {
    pub(crate) fn from_image(image: &CaptureImageLease) -> Self {
        Self {
            image: image.clone(),
        }
    }

    #[must_use]
    pub fn image_lease(&self) -> &CaptureImageLease {
        &self.image
    }
}

struct ArtifactInterest {
    id: CaptureArtifactId,
    _image: CaptureImageLease,
}

/// Strong artifact interest retained by terminal ticket results and native readers.
#[derive(Clone)]
pub struct CaptureArtifactCapability {
    interest: Arc<ArtifactInterest>,
}

impl CaptureArtifactCapability {
    pub(crate) fn new(id: CaptureArtifactId, image: &CaptureImageLease) -> Self {
        Self {
            interest: Arc::new(ArtifactInterest {
                id,
                _image: image.clone(),
            }),
        }
    }

    #[must_use]
    pub fn id(&self) -> CaptureArtifactId {
        self.interest.id
    }

    #[must_use]
    pub fn session(&self) -> SessionId {
        self.interest.id.session()
    }

    #[must_use]
    pub fn downgrade(&self) -> CaptureInterestWeak {
        CaptureInterestWeak {
            id: self.interest.id,
            interest: Arc::downgrade(&self.interest),
        }
    }
}

impl fmt::Debug for CaptureArtifactCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureArtifactCapability")
            .field("id", &self.interest.id)
            .field("session", &self.session())
            .finish_non_exhaustive()
    }
}

impl PartialEq for CaptureArtifactCapability {
    fn eq(&self, other: &Self) -> bool {
        self.interest.id == other.interest.id && Arc::ptr_eq(&self.interest, &other.interest)
    }
}

impl Eq for CaptureArtifactCapability {}

/// Weak observation of a capability. It cannot upgrade itself into a reader or new owner.
#[derive(Clone)]
pub struct CaptureInterestWeak {
    id: CaptureArtifactId,
    interest: Weak<ArtifactInterest>,
}

impl CaptureInterestWeak {
    #[must_use]
    pub fn id(&self) -> CaptureArtifactId {
        self.id
    }

    #[must_use]
    pub fn is_live(&self) -> bool {
        self.interest.strong_count() != 0
    }

    #[must_use]
    pub fn matches(&self, capability: &CaptureArtifactCapability) -> bool {
        self.id == capability.id()
            && self
                .interest
                .upgrade()
                .is_some_and(|interest| Arc::ptr_eq(&interest, &capability.interest))
    }
}

impl fmt::Debug for CaptureInterestWeak {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureInterestWeak")
            .field("id", &self.id)
            .field("live", &self.is_live())
            .finish()
    }
}

/// Actual projection data copied from the selected rendered view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CaptureProjection {
    Perspective,
    Orthographic { vertical_size: f32 },
}

/// Bounded camera and presentation data for one actual frame.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureView {
    physical_width: u32,
    physical_height: u32,
    camera_position: [f32; 3],
    camera_target: [f32; 3],
    camera_up: [f32; 3],
    vertical_fov_radians: f32,
    near: f32,
    far: f32,
    projection: CaptureProjection,
    view_projection: [f32; 16],
}

impl CaptureView {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        physical_width: u32,
        physical_height: u32,
        camera_position: [f32; 3],
        camera_target: [f32; 3],
        camera_up: [f32; 3],
        vertical_fov_radians: f32,
        near: f32,
        far: f32,
        projection: CaptureProjection,
        view_projection: [f32; 16],
    ) -> Result<Self, CaptureMetadataError> {
        let pixels = u64::from(physical_width)
            .checked_mul(u64::from(physical_height))
            .ok_or(CaptureMetadataError::TooLarge)?;
        let valid_projection = match projection {
            CaptureProjection::Perspective => true,
            CaptureProjection::Orthographic { vertical_size } => {
                vertical_size.is_finite() && (0.001..=1_000_000.0).contains(&vertical_size)
            }
        };
        if physical_width == 0
            || physical_height == 0
            || physical_width > MAX_CAPTURE_DIMENSION
            || physical_height > MAX_CAPTURE_DIMENSION
            || pixels > MAX_CAPTURE_PIXELS
        {
            return Err(CaptureMetadataError::TooLarge);
        }
        if camera_position
            .iter()
            .chain(camera_target.iter())
            .chain(camera_up.iter())
            .chain(view_projection.iter())
            .any(|value| !value.is_finite())
            || !vertical_fov_radians.is_finite()
            || vertical_fov_radians <= 0.01
            || vertical_fov_radians >= std::f32::consts::PI - 0.01
            || !near.is_finite()
            || !far.is_finite()
            || near <= 0.0
            || far <= near
            || !valid_projection
            || !camera_pose_is_valid(camera_position, camera_target, camera_up)
        {
            return Err(CaptureMetadataError::InvalidView);
        }
        Ok(Self {
            physical_width,
            physical_height,
            camera_position,
            camera_target,
            camera_up,
            vertical_fov_radians,
            near,
            far,
            projection,
            view_projection,
        })
    }

    #[must_use]
    pub const fn physical_width(&self) -> u32 {
        self.physical_width
    }

    #[must_use]
    pub const fn physical_height(&self) -> u32 {
        self.physical_height
    }

    #[must_use]
    pub const fn camera_position(&self) -> &[f32; 3] {
        &self.camera_position
    }

    #[must_use]
    pub const fn camera_target(&self) -> &[f32; 3] {
        &self.camera_target
    }

    #[must_use]
    pub const fn camera_up(&self) -> &[f32; 3] {
        &self.camera_up
    }

    #[must_use]
    pub const fn vertical_fov_radians(&self) -> f32 {
        self.vertical_fov_radians
    }

    #[must_use]
    pub const fn near(&self) -> f32 {
        self.near
    }

    #[must_use]
    pub const fn far(&self) -> f32 {
        self.far
    }

    #[must_use]
    pub const fn projection(&self) -> CaptureProjection {
        self.projection
    }

    #[must_use]
    pub const fn view_projection(&self) -> &[f32; 16] {
        &self.view_projection
    }
}

fn camera_pose_is_valid(position: [f32; 3], target: [f32; 3], up: [f32; 3]) -> bool {
    let view = [
        target[0] - position[0],
        target[1] - position[1],
        target[2] - position[2],
    ];
    if view.iter().any(|value| !value.is_finite()) {
        return false;
    }
    let view_squared = view.iter().map(|value| value * value).sum::<f32>();
    let up_squared = up.iter().map(|value| value * value).sum::<f32>();
    if !view_squared.is_finite()
        || !up_squared.is_finite()
        || view_squared < 1.0e-8
        || up_squared < 1.0e-8
    {
        return false;
    }
    let cross = [
        view[1] * up[2] - view[2] * up[1],
        view[2] * up[0] - view[0] * up[2],
        view[0] * up[1] - view[1] * up[0],
    ];
    let cross_squared = cross.iter().map(|value| value * value).sum::<f32>();
    cross_squared.is_finite() && cross_squared >= 1.0e-8
}

/// Color encoding of the actual surface and the bounded PNG output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapturePixelEncoding {
    Linear,
    SrgbEncoded,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CaptureMetadataError {
    #[error("capture metadata contains an invalid view or color encoding")]
    InvalidView,
    #[error("capture image dimensions or encoded size exceed the configured ceiling")]
    TooLarge,
    #[error("PNG metadata is inconsistent with the selected frame dimensions")]
    InconsistentPng,
}

/// Fixed-size PNG artifact metadata. Output pixels are always RGBA8/sRGB; input surface
/// format/encoding are reported separately by [`CaptureFrameMetadata`]. Pixel bytes and paths
/// are deliberately absent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturePngMetadata {
    width: u32,
    height: u32,
    byte_length: u64,
    sha256: [u8; 32],
    format: CapturePngFormat,
}

/// The only PNG format supported by this capture contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapturePngFormat {
    Rgba8Srgb,
}

impl CapturePngMetadata {
    pub fn new(
        width: u32,
        height: u32,
        byte_length: u64,
        sha256: [u8; 32],
    ) -> Result<Self, CaptureMetadataError> {
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(CaptureMetadataError::TooLarge)?;
        if width == 0
            || height == 0
            || width > MAX_CAPTURE_DIMENSION
            || height > MAX_CAPTURE_DIMENSION
            || pixels > MAX_CAPTURE_PIXELS
            || byte_length == 0
            || byte_length > MAX_CAPTURE_PNG_BYTES
        {
            return Err(CaptureMetadataError::TooLarge);
        }
        Ok(Self {
            width,
            height,
            byte_length,
            sha256,
            format: CapturePngFormat::Rgba8Srgb,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    #[must_use]
    pub const fn format(&self) -> CapturePngFormat {
        self.format
    }
}

/// Selected-frame data supplied by a trusted capture consumer.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureFrameMetadata {
    publication: ProjectPublicationIdentity,
    present_sequence: u64,
    view: CaptureView,
    surface_format: CaptureSurfaceFormat,
    surface_encoding: CapturePixelEncoding,
    png: CapturePngMetadata,
}

impl CaptureFrameMetadata {
    pub fn new(
        publication: ProjectPublicationIdentity,
        present_sequence: u64,
        view: CaptureView,
        surface_format: CaptureSurfaceFormat,
        surface_encoding: CapturePixelEncoding,
        png: CapturePngMetadata,
    ) -> Result<Self, CaptureMetadataError> {
        if present_sequence == 0 {
            return Err(CaptureMetadataError::InvalidView);
        }
        let surface_is_srgb = matches!(
            surface_format,
            CaptureSurfaceFormat::Rgba8UnormSrgb | CaptureSurfaceFormat::Bgra8UnormSrgb
        );
        if surface_is_srgb && surface_encoding == CapturePixelEncoding::Linear {
            return Err(CaptureMetadataError::InvalidView);
        }
        if png.width != view.physical_width || png.height != view.physical_height {
            return Err(CaptureMetadataError::InconsistentPng);
        }
        Ok(Self {
            publication,
            present_sequence,
            view,
            surface_format,
            surface_encoding,
            png,
        })
    }

    #[must_use]
    pub fn publication(&self) -> &ProjectPublicationIdentity {
        &self.publication
    }

    #[must_use]
    pub const fn present_sequence(&self) -> u64 {
        self.present_sequence
    }

    #[must_use]
    pub fn view(&self) -> &CaptureView {
        &self.view
    }

    #[must_use]
    pub const fn surface_format(&self) -> CaptureSurfaceFormat {
        self.surface_format
    }

    #[must_use]
    pub const fn surface_encoding(&self) -> CapturePixelEncoding {
        self.surface_encoding
    }

    #[must_use]
    pub fn png(&self) -> &CapturePngMetadata {
        &self.png
    }
}

/// Bounded metadata returned by an accepted frame-capture request.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureFrameResult {
    artifact: CaptureArtifactCapability,
    metadata: CaptureFrameMetadata,
}

impl CaptureFrameResult {
    pub(crate) fn new(artifact: CaptureArtifactCapability, metadata: CaptureFrameMetadata) -> Self {
        Self { artifact, metadata }
    }

    #[must_use]
    pub fn artifact(&self) -> &CaptureArtifactCapability {
        &self.artifact
    }

    #[must_use]
    pub fn metadata(&self) -> &CaptureFrameMetadata {
        &self.metadata
    }
}

/// Fixed, bounded reason for an accepted capture to fail.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CaptureFailure {
    #[error("capture deadline elapsed before an eligible frame was completed")]
    TimedOut,
    #[error("capture consumer detached before completion")]
    Detached,
    #[error("capture responder was dropped before completion")]
    ResponderDropped,
    #[error("the active project generation changed before capture completion")]
    StaleGeneration,
    #[error("captured frame did not match the request's session or source identity")]
    StaleIdentity,
    #[error("the actual surface cannot be captured")]
    Unsupported,
    #[error("the selected frame exceeds capture limits")]
    TooLarge,
    #[error("GPU readback failed")]
    ReadbackFailed,
    #[error("PNG encoding or artifact finalization failed")]
    ArtifactFailed,
    #[error("capture was cancelled")]
    Cancelled,
}

/// One completion in the actor-reserved result slot.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureCompletion {
    Captured(Box<CaptureFrameMetadata>),
    Failed(CaptureFailure),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CaptureResponderError {
    #[error("capture completion slot is busy; retry the same completion")]
    Busy,
    #[error("capture responder has already completed")]
    AlreadyCompleted,
    #[error("capture actor has stopped")]
    Stopped,
}

pub(crate) struct CaptureCompletionSlot {
    value: Mutex<Option<CaptureCompletion>>,
    state: AtomicU8,
}

pub(crate) struct CaptureResponderInterest;

/// One-shot sender for the actor's pre-reserved capacity-one completion slot.
pub struct CaptureResponder {
    slot: Arc<CaptureCompletionSlot>,
    _interest: Arc<CaptureResponderInterest>,
    completed: bool,
}

impl fmt::Debug for CaptureResponder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureResponder")
            .field("completed", &self.completed)
            .finish_non_exhaustive()
    }
}

impl CaptureResponder {
    /// Stores the completion without waiting. On Busy, the caller retains its completion and
    /// retries; the responder remains usable until one successful write.
    pub fn try_complete(
        &mut self,
        completion: &CaptureCompletion,
    ) -> Result<(), CaptureResponderError> {
        let state = self.slot.state.load(Ordering::Acquire);
        if self.completed || state == COMPLETION_WRITTEN {
            return Err(CaptureResponderError::AlreadyCompleted);
        }
        if state == COMPLETION_RECEIVER_CLOSED {
            return Err(CaptureResponderError::Stopped);
        }
        let mut slot = match self.slot.value.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return Err(CaptureResponderError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureResponderError::Stopped),
        };
        if slot.is_some() {
            return Err(CaptureResponderError::AlreadyCompleted);
        }
        self.slot
            .state
            .compare_exchange(
                COMPLETION_OPEN,
                COMPLETION_WRITTEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|state| match state {
                COMPLETION_RECEIVER_CLOSED => CaptureResponderError::Stopped,
                _ => CaptureResponderError::AlreadyCompleted,
            })?;
        *slot = Some(completion.clone());
        self.completed = true;
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct CaptureCancellation(Arc<AtomicBool>);

impl CaptureCancellation {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A capture request delivered to the trusted native consumer.
pub struct CaptureRequest {
    id: CaptureArtifactId,
    source: CaptureSourceIdentity,
    at_least_tick: Tick,
    deadline: Instant,
    cancellation: CaptureCancellation,
    artifact: CaptureArtifactCapability,
    work_lease: CaptureWorkLease,
    responder: CaptureResponder,
}

impl fmt::Debug for CaptureRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureRequest")
            .field("id", &self.id)
            .field("source", &self.source)
            .field("at_least_tick", &self.at_least_tick)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl CaptureRequest {
    #[must_use]
    pub const fn id(&self) -> CaptureArtifactId {
        self.id
    }

    #[must_use]
    pub fn source(&self) -> &CaptureSourceIdentity {
        &self.source
    }

    #[must_use]
    pub const fn at_least_tick(&self) -> Tick {
        self.at_least_tick
    }

    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.0.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn artifact(&self) -> &CaptureArtifactCapability {
        &self.artifact
    }

    #[must_use]
    pub fn work_lease(&self) -> &CaptureWorkLease {
        &self.work_lease
    }

    pub fn responder_mut(&mut self) -> &mut CaptureResponder {
        &mut self.responder
    }
}

/// Single-owner public side of the runtime/native capture handoff.
pub struct CaptureHandoffPort {
    shared: Arc<CaptureHandoffShared>,
}

impl fmt::Debug for CaptureHandoffPort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureHandoffPort")
            .field("session", &self.shared.session)
            .field("state", &self.try_status())
            .finish_non_exhaustive()
    }
}

impl CaptureHandoffPort {
    pub(crate) fn new_pending(session: SessionId) -> (Self, Arc<CaptureHandoffShared>) {
        let shared = Arc::new(CaptureHandoffShared::new(session));
        (
            Self {
                shared: Arc::clone(&shared),
            },
            shared,
        )
    }

    #[must_use]
    pub fn session(&self) -> SessionId {
        self.shared.session
    }

    #[must_use]
    pub fn try_status(&self) -> CaptureHandoffState {
        self.shared.state()
    }

    pub fn try_set_ready(&self, info: CaptureReadyInfo) -> Result<(), CaptureHandoffError> {
        self.shared.set_ready(info)
    }

    pub fn try_set_unsupported(
        &self,
        reason: CaptureUnsupportedReason,
    ) -> Result<(), CaptureHandoffError> {
        self.shared.set_unsupported(reason)
    }

    /// OOB, idempotent, nonwaiting detach signal. The actor terminalizes accepted work.
    pub fn request_detach(&self) {
        self.shared.detach();
    }

    pub fn try_take_request(&self) -> Result<Option<CaptureRequest>, CaptureHandoffError> {
        self.shared.take_request()
    }

    #[cfg(test)]
    pub(crate) fn has_unclaimed_request(&self) -> bool {
        match self.shared.request.try_lock() {
            Ok(request) => request.is_some(),
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().is_some(),
        }
    }

    #[cfg(test)]
    pub(crate) fn hold_request_slot_for_test(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<CaptureRequest>> {
        self.shared
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn dispatch_busy_count_for_test(&self) -> usize {
        self.shared.dispatch_busy_count.load(Ordering::Acquire)
    }
}

impl Drop for CaptureHandoffPort {
    fn drop(&mut self) {
        self.shared.detach();
    }
}

pub(crate) struct CaptureHandoffShared {
    session: SessionId,
    status: AtomicU8,
    ready: std::sync::OnceLock<CaptureReadyInfo>,
    unsupported: std::sync::OnceLock<CaptureUnsupportedReason>,
    request: Mutex<Option<CaptureRequest>>,
    images: CreditPool,
    #[cfg(test)]
    dispatch_busy_count: std::sync::atomic::AtomicUsize,
}

impl CaptureHandoffShared {
    fn new(session: SessionId) -> Self {
        Self {
            session,
            status: AtomicU8::new(HANDOFF_STARTING),
            ready: std::sync::OnceLock::new(),
            unsupported: std::sync::OnceLock::new(),
            request: Mutex::new(None),
            images: CreditPool::new(CreditLimits {
                tickets: 1,
                bytes: 1,
            }),
            #[cfg(test)]
            dispatch_busy_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn state(&self) -> CaptureHandoffState {
        match self.status.load(Ordering::Acquire) {
            HANDOFF_READY => self
                .ready
                .get()
                .copied()
                .map_or(CaptureHandoffState::Detached, CaptureHandoffState::Ready),
            HANDOFF_UNSUPPORTED => self.unsupported.get().copied().map_or(
                CaptureHandoffState::Detached,
                CaptureHandoffState::Unsupported,
            ),
            HANDOFF_DETACHED => CaptureHandoffState::Detached,
            _ => CaptureHandoffState::Starting,
        }
    }

    fn set_ready(&self, info: CaptureReadyInfo) -> Result<(), CaptureHandoffError> {
        if self.status.load(Ordering::Acquire) != HANDOFF_STARTING {
            return Err(if self.status.load(Ordering::Acquire) == HANDOFF_DETACHED {
                CaptureHandoffError::Detached
            } else {
                CaptureHandoffError::AlreadyResolved
            });
        }
        self.ready
            .set(info)
            .map_err(|_| CaptureHandoffError::AlreadyResolved)?;
        self.status
            .compare_exchange(
                HANDOFF_STARTING,
                HANDOFF_READY,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| {
                if state == HANDOFF_DETACHED {
                    CaptureHandoffError::Detached
                } else {
                    CaptureHandoffError::AlreadyResolved
                }
            })
    }

    fn set_unsupported(&self, reason: CaptureUnsupportedReason) -> Result<(), CaptureHandoffError> {
        if self.status.load(Ordering::Acquire) != HANDOFF_STARTING {
            return Err(if self.status.load(Ordering::Acquire) == HANDOFF_DETACHED {
                CaptureHandoffError::Detached
            } else {
                CaptureHandoffError::AlreadyResolved
            });
        }
        self.unsupported
            .set(reason)
            .map_err(|_| CaptureHandoffError::AlreadyResolved)?;
        self.status
            .compare_exchange(
                HANDOFF_STARTING,
                HANDOFF_UNSUPPORTED,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|state| {
                if state == HANDOFF_DETACHED {
                    CaptureHandoffError::Detached
                } else {
                    CaptureHandoffError::AlreadyResolved
                }
            })
    }

    pub(crate) fn detach(&self) {
        self.status.store(HANDOFF_DETACHED, Ordering::Release);
    }

    pub(crate) fn session(&self) -> SessionId {
        self.session
    }

    pub(crate) fn reserve_image(&self) -> Result<CaptureImageLease, CaptureHandoffError> {
        self.images
            .try_reserve(1)
            .map(|credit| CaptureImageLease { _credit: credit })
            .map_err(|_| CaptureHandoffError::RequestOccupied)
    }

    pub(crate) fn dispatch(
        &self,
        request: Box<CaptureRequest>,
    ) -> Result<(), (Box<CaptureRequest>, CaptureHandoffError)> {
        if self.status.load(Ordering::Acquire) != HANDOFF_READY {
            return Err((
                request,
                if self.status.load(Ordering::Acquire) == HANDOFF_DETACHED {
                    CaptureHandoffError::Detached
                } else {
                    CaptureHandoffError::AlreadyResolved
                },
            ));
        }
        let mut slot = match self.request.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                self.dispatch_busy_count.fetch_add(1, Ordering::AcqRel);
                return Err((request, CaptureHandoffError::Busy));
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err((request, CaptureHandoffError::Poisoned));
            }
        };
        if slot.is_some() {
            return Err((request, CaptureHandoffError::RequestOccupied));
        }
        if self.status.load(Ordering::Acquire) != HANDOFF_READY {
            return Err((request, CaptureHandoffError::Detached));
        }
        *slot = Some(*request);
        Ok(())
    }

    fn take_request(&self) -> Result<Option<CaptureRequest>, CaptureHandoffError> {
        if self.status.load(Ordering::Acquire) == HANDOFF_DETACHED {
            return Err(CaptureHandoffError::Detached);
        }
        let mut slot = match self.request.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return Err(CaptureHandoffError::Busy),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        Ok(slot.take())
    }

    pub(crate) fn drain_unclaimed(&self) -> Result<(), CaptureHandoffError> {
        let mut slot = match self.request.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return Err(CaptureHandoffError::Busy),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        drop(slot.take());
        Ok(())
    }

    pub(crate) fn drain_unclaimed_after_stop(&self) {
        let mut slot = self
            .request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(slot.take());
    }
}

pub(crate) struct CaptureActorPending {
    pub(crate) slot: Arc<CaptureCompletionSlot>,
    pub(crate) responder_interest: Weak<CaptureResponderInterest>,
}

impl CaptureActorPending {
    pub(crate) fn close_receiver(&self) {
        let _ = self.slot.state.compare_exchange(
            COMPLETION_OPEN,
            COMPLETION_RECEIVER_CLOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn take_completion(
        &self,
    ) -> Result<Option<CaptureCompletion>, CaptureResponderError> {
        let mut value = match self.slot.value.try_lock() {
            Ok(value) => value,
            Err(TryLockError::WouldBlock) => return Err(CaptureResponderError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(CaptureResponderError::Stopped),
        };
        Ok(value.take())
    }

    pub(crate) fn responder_alive(&self) -> bool {
        self.responder_interest.strong_count() != 0
    }
}

impl Drop for CaptureActorPending {
    fn drop(&mut self) {
        self.close_receiver();
    }
}

impl CaptureRequest {
    pub(crate) fn new(
        id: CaptureArtifactId,
        source: CaptureSourceIdentity,
        at_least_tick: Tick,
        deadline: Instant,
        cancellation: CaptureCancellation,
        artifact: CaptureArtifactCapability,
        work_lease: CaptureWorkLease,
    ) -> (Self, CaptureActorPending) {
        let slot = Arc::new(CaptureCompletionSlot {
            value: Mutex::new(None),
            state: AtomicU8::new(COMPLETION_OPEN),
        });
        let interest = Arc::new(CaptureResponderInterest);
        let pending = CaptureActorPending {
            slot: Arc::clone(&slot),
            responder_interest: Arc::downgrade(&interest),
        };
        (
            Self {
                id,
                source,
                at_least_tick,
                deadline,
                cancellation,
                artifact,
                work_lease,
                responder: CaptureResponder {
                    slot,
                    _interest: interest,
                    completed: false,
                },
            },
            pending,
        )
    }
}

pub(crate) fn capture_deadline(timeout: Option<Duration>, now: Instant) -> Option<Instant> {
    let timeout = timeout.unwrap_or(DEFAULT_CAPTURE_TIMEOUT);
    if timeout.is_zero() || timeout > MAX_CAPTURE_TIMEOUT {
        return None;
    }
    now.checked_add(timeout)
}

pub(crate) fn capture_cancellation() -> CaptureCancellation {
    CaptureCancellation::new()
}

pub(crate) fn cancel_capture(cancellation: &CaptureCancellation) {
    cancellation.cancel();
}
