#[cfg(feature = "window")]
use std::sync::mpsc::TryRecvError;
#[cfg(feature = "window")]
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Instant;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc::TrySendError,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use daedalus_content::scene::StableId;
use daedalus_core::{EntityHandle, SessionId};
#[cfg(feature = "capture")]
use daedalus_renderer::ManagedPacketWindowServer;
#[cfg(feature = "capture")]
use daedalus_renderer::managed_packet_window_capture_channel;
#[cfg(feature = "window")]
use daedalus_renderer::{
    ManagedPacketWindowClient, ManagedPacketWindowControl, ManagedPacketWindowInput,
    ManagedPacketWindowOptions, ManagedPresentedSummary, ManagedPresentedView, ManagedStageEvent,
    ManagedStageKey, ManagedWindowOutcome, ManagedWindowRead, PacketStageInput, PacketStageLimits,
    RenderView, WindowGpuOwner, WindowInputPolicy, managed_packet_window_channel,
    run_packet_windowed_managed_with_input_policy,
};
use daedalus_renderer::{
    PacketAssetLimits, PacketCpuLimits, PacketCpuOwner, PacketCpuStatus, PacketRenderError,
    PacketRendererLimits,
};
#[cfg(feature = "capture")]
use daedalus_runtime::{
    CaptureArtifactCapability, CaptureCompletion, CaptureFailure, CaptureHandoffError,
    CaptureHandoffPort, CaptureHandoffState, CaptureReadyInfo, CaptureRequest,
    CaptureResponderError, CaptureUnsupportedReason,
};
use daedalus_runtime::{
    NativeProjectActivationService, NativeProjectActivationShutdown,
    NativeProjectActivationStartError, NativeProjectPreparationOptions, ProjectConsumerError,
    ProjectConsumerStageProgress, ProjectGenerationConsumerPort, ProjectGenerationConsumerStatus,
    ProjectPublicationIdentity, ProjectSourceIdentity, ProjectSourceKind, RequestPhase,
    SessionAttachError, SessionControlClient, SessionControlConfig, SessionOperation,
    SessionRequestError, SessionRequestResult, SessionShutdownReport, SessionSubmitError,
};
#[cfg(feature = "window")]
use daedalus_runtime::{
    ProjectConsumerReportProgress, ProjectConsumerStageFailure, ProjectConsumerStageFailurePhase,
    ProjectConsumerStageOutcome, ProjectPresentedFrame, ProjectPresentedView,
};
use thiserror::Error;

#[cfg(feature = "capture")]
use crate::capture::{
    CaptureControlPort, CaptureCoordinatorPort, CaptureEncoderShutdown, CaptureEncoderState,
    CapturePipelineFailure, CapturePipelineOwner, CapturePipelineStatus, CaptureStatusReadError,
};
#[cfg(feature = "capture")]
use crate::cpu_worker::CpuCapturePorts;
use crate::cpu_worker::{
    self, CpuFailure, CpuWorker, CpuWorkerClient, PrepareRequest, PrepareResult, RetireRequest,
};
#[cfg(feature = "window")]
use crate::cpu_worker::{
    LatestFrameRequest, LatestFrameResult, ResultDelivery, StageInputRequest, StageInputResult,
};

const BINDING_PAGE_SIZE: usize = 1_024;
const MAX_AUTHORED_ENTITIES: usize = 16_384;
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const COORDINATOR_POLL: Duration = Duration::from_millis(2);
#[cfg(feature = "window")]
const FRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(16);
const MAX_FAILURE_MESSAGE_BYTES: usize = 2_048;
#[cfg(feature = "window")]
const MAX_PRESENTATION_IDENTITIES: usize = 3;

/// Trusted options for one native project host. The host creates one session using the caller's
/// configured clock mode and one render-consumer service; callers attach clients to that authority.
#[derive(Clone, Debug)]
pub struct NativeProjectHostOptions {
    preparation: NativeProjectPreparationOptions,
    session: SessionControlConfig,
    cpu_limits: PacketCpuLimits,
    packet_limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    #[cfg(feature = "capture")]
    live_capture: bool,
}

impl NativeProjectHostOptions {
    pub fn new(preparation: NativeProjectPreparationOptions) -> Self {
        Self {
            preparation,
            session: SessionControlConfig::default(),
            cpu_limits: PacketCpuLimits::default(),
            packet_limits: PacketRendererLimits::default(),
            asset_limits: PacketAssetLimits::default(),
            #[cfg(feature = "capture")]
            live_capture: false,
        }
    }

    pub fn for_project(root: impl AsRef<Path>) -> Self {
        Self::new(NativeProjectPreparationOptions::new(root))
    }

    #[must_use]
    pub fn with_session_config(mut self, session: SessionControlConfig) -> Self {
        self.session = session;
        self
    }

    #[must_use]
    pub fn with_cpu_limits(mut self, limits: PacketCpuLimits) -> Self {
        self.cpu_limits = limits;
        self
    }

    #[must_use]
    pub fn with_packet_limits(mut self, limits: PacketRendererLimits) -> Self {
        self.packet_limits = limits;
        self
    }

    #[must_use]
    pub fn with_asset_limits(mut self, limits: PacketAssetLimits) -> Self {
        self.asset_limits = limits;
        self
    }

    #[cfg(feature = "capture")]
    #[must_use]
    pub fn with_live_capture(mut self) -> Self {
        self.live_capture = true;
        self
    }
}

/// View-specific options. The caller still runs the OS event loop on the calling thread.
#[cfg(feature = "window")]
#[derive(Clone, Debug, Default)]
pub struct NativeProjectViewOptions {
    pub window: ManagedPacketWindowOptions,
    /// Explicit trusted SDK override. By default CPU extraction uses the authored project camera;
    /// the managed window may still apply its local interactive camera without changing authority.
    pub view_override: Option<RenderView>,
}

/// Coarse host lifecycle, distinct from actor, CPU preparation, device, and view progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeProjectHostLifecycle {
    Initializing,
    InitialPreparing,
    CpuPrepared,
    Ready,
    ViewRunning,
    ViewClosed,
    Stopping,
    Stopped,
    Failed,
}

/// Bounded host-level failure metadata. It never carries project or renderer payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProjectHostFailure {
    code: String,
    message: String,
}

/// Bounded capture-pipeline failure copied from the private encoder status.
#[cfg(feature = "capture")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeCaptureFailure {
    code: String,
    message: String,
}

#[cfg(feature = "capture")]
impl NativeCaptureFailure {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code: bounded(code.to_owned(), 64),
            message: bounded(message.to_owned(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Capture readiness lifecycle projected from the actual renderer and encoder owners.
#[cfg(feature = "capture")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeProjectCaptureLifecycle {
    Starting,
    Ready(CaptureReadyInfo),
    Unsupported(CaptureUnsupportedReason),
    Detached,
    Stopping,
    Stopped,
    Failed(NativeCaptureFailure),
}

/// Public encoder state. Private pipeline error variants are mapped to bounded stable metadata.
#[cfg(feature = "capture")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCaptureEncoderState {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed(NativeCaptureFailure),
}

/// Nonwaiting, bounded capture status for the existing host/session.
#[cfg(feature = "capture")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProjectCaptureStatus {
    session: SessionId,
    lifecycle: NativeProjectCaptureLifecycle,
    encoder_state: NativeCaptureEncoderState,
    pending_jobs: u8,
    retained_png_bytes: u64,
    cleanup_pending: bool,
    failure: Option<NativeCaptureFailure>,
}

#[cfg(feature = "capture")]
impl NativeProjectCaptureStatus {
    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn lifecycle(&self) -> &NativeProjectCaptureLifecycle {
        &self.lifecycle
    }

    pub fn encoder_state(&self) -> &NativeCaptureEncoderState {
        &self.encoder_state
    }

    pub fn pending_jobs(&self) -> u8 {
        self.pending_jobs
    }

    pub fn retained_png_bytes(&self) -> u64 {
        self.retained_png_bytes
    }

    pub fn cleanup_pending(&self) -> bool {
        self.cleanup_pending
    }

    pub fn failure(&self) -> Option<&NativeCaptureFailure> {
        self.failure.as_ref()
    }
}

/// Errors from nonwaiting capture status reads on the host facade.
#[cfg(feature = "capture")]
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeCaptureStatusError {
    #[error("live capture is not enabled for this host")]
    Unavailable,
    #[error("capture status is busy; retry without waiting")]
    Busy,
    #[error("capture status is poisoned")]
    Poisoned,
}

impl NativeProjectHostFailure {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: bounded(code.into(), 64),
            message: bounded(message.into(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Immutable status snapshot containing only bounded identities and accounting metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProjectHostStatus {
    lifecycle: NativeProjectHostLifecycle,
    session: SessionId,
    active_generation: u64,
    active_source_digest: String,
    cpu: PacketCpuStatus,
    view_started: bool,
    view_closed: bool,
    last_queued_publication: Option<ProjectPublicationIdentity>,
    last_presented: Option<NativePresentedSummary>,
    failure: Option<NativeProjectHostFailure>,
}

impl NativeProjectHostStatus {
    pub fn lifecycle(&self) -> NativeProjectHostLifecycle {
        self.lifecycle
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }

    pub fn active_source_digest(&self) -> &str {
        &self.active_source_digest
    }

    pub fn cpu(&self) -> PacketCpuStatus {
        self.cpu
    }

    pub fn view_started(&self) -> bool {
        self.view_started
    }

    pub fn view_closed(&self) -> bool {
        self.view_closed
    }

    /// Latest real actor publication accepted into the managed-window input lane. This records
    /// enqueue only; it does not claim that the window accepted or presented the frame.
    pub fn last_queued_publication(&self) -> Option<&ProjectPublicationIdentity> {
        self.last_queued_publication.as_ref()
    }

    pub fn last_presented(&self) -> Option<&NativePresentedSummary> {
        self.last_presented.as_ref()
    }

    pub fn failure(&self) -> Option<&NativeProjectHostFailure> {
        self.failure.as_ref()
    }
}

/// Metadata copied from a real managed-window present. No frame or packet is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativePresentedSummary {
    session: SessionId,
    tick: u64,
    publication_sequence: u64,
    authority_snapshot_sequence: Option<u64>,
    project_generation: u64,
    source_digest: String,
    present_sequence: u64,
    physical_width: u32,
    physical_height: u32,
}

impl NativePresentedSummary {
    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn tick(&self) -> u64 {
        self.tick
    }

    pub fn publication_sequence(&self) -> u64 {
        self.publication_sequence
    }

    pub fn authority_snapshot_sequence(&self) -> Option<u64> {
        self.authority_snapshot_sequence
    }

    pub fn project_generation(&self) -> u64 {
        self.project_generation
    }

    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    pub fn present_sequence(&self) -> u64 {
        self.present_sequence
    }

    pub fn physical_size(&self) -> (u32, u32) {
        (self.physical_width, self.physical_height)
    }
}

/// Nonblocking read result for the host's small status slot.
#[derive(Clone, Debug, Eq, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "status reads return an owned metadata snapshot; boxing would allocate and change the additive SDK API"
)]
pub enum NativeProjectHostStatusRead {
    Value(NativeProjectHostStatus),
    Busy,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeProjectHostStatusReadError {
    #[error("native project host status is poisoned")]
    Poisoned,
}

/// Failure before a live host exists. Runtime factory errors include synchronous initial loading.
#[derive(Debug, Error)]
pub enum NativeProjectHostStartError {
    #[error("invalid renderer CPU limits: {0}")]
    CpuLimits(#[from] PacketRenderError),
    #[error("native project activation startup failed: {0}")]
    Activation(#[from] NativeProjectActivationStartError),
}

/// Typed nonblocking host-operation error.
#[derive(Debug, Error)]
pub enum NativeProjectHostError {
    #[error("the native project host is stopping or stopped")]
    Stopped,
    #[error("the requested native project view is a one-shot operation")]
    ViewAlreadyUsed,
    #[error("the requested host operation is temporarily busy")]
    Busy,
    #[error("the native project host could not attach a session client: {0}")]
    Attach(#[from] SessionAttachError),
    #[error("native project host failed: {0}")]
    Failed(String),
    #[error("native project host shutdown failed: {0}")]
    Shutdown(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeProjectHostShutdown {
    Stopping,
    Stopped,
}

/// Cloneable out-of-band lifecycle control. It retains no runtime service, source, or render data.
#[derive(Clone)]
pub struct NativeProjectHostControl {
    signal: Arc<HostLifecycleSignal>,
    status: Arc<Mutex<NativeProjectHostStatus>>,
    #[cfg(feature = "window")]
    window: Arc<Mutex<Option<ManagedPacketWindowControl>>>,
    #[cfg(feature = "capture")]
    capture_control: Option<CaptureControlPort>,
}

impl std::fmt::Debug for NativeProjectHostControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeProjectHostControl")
            .field(
                "stop_requested",
                &self.signal.stop_requested.load(Ordering::Acquire),
            )
            .field(
                "view_close_requested",
                &self.signal.view_close_requested.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl NativeProjectHostControl {
    pub fn request_stop(&self) {
        self.signal.stop_requested.store(true, Ordering::Release);
        #[cfg(feature = "capture")]
        self.signal.capture_stopping.store(true, Ordering::Release);
        #[cfg(feature = "window")]
        if let Ok(window) = self.window.try_lock()
            && let Some(window) = window.as_ref()
        {
            window.request_host_stop();
        }
    }

    #[cfg(feature = "window")]
    pub fn request_view_close(&self) {
        self.signal
            .view_close_requested
            .store(true, Ordering::Release);
        #[cfg(feature = "window")]
        if let Ok(window) = self.window.try_lock()
            && let Some(window) = window.as_ref()
        {
            window.request_view_close();
        }
    }

    pub fn try_status(
        &self,
    ) -> Result<NativeProjectHostStatusRead, NativeProjectHostStatusReadError> {
        match self.status.try_lock() {
            Ok(status) => Ok(NativeProjectHostStatusRead::Value(status.clone())),
            Err(TryLockError::WouldBlock) => Ok(NativeProjectHostStatusRead::Busy),
            Err(TryLockError::Poisoned(_)) => Err(NativeProjectHostStatusReadError::Poisoned),
        }
    }

    #[cfg(feature = "capture")]
    pub fn try_capture_status(
        &self,
    ) -> Result<NativeProjectCaptureStatus, NativeCaptureStatusError> {
        let control = self
            .capture_control
            .as_ref()
            .ok_or(NativeCaptureStatusError::Unavailable)?;
        let status = control.try_status().map_err(map_capture_status_error)?;
        Ok(project_capture_status(
            &status,
            self.signal.capture_stopping.load(Ordering::Acquire),
            self.signal.capture_runtime_ready.load(Ordering::Acquire),
            self.signal.capture_drained.load(Ordering::Acquire),
        ))
    }

    #[cfg(feature = "capture")]
    pub fn try_acquire_capture_artifact(
        &self,
        capability: &CaptureArtifactCapability,
    ) -> Result<crate::NativeCaptureArtifactLease, crate::NativeArtifactAccessError> {
        let control = self
            .capture_control
            .as_ref()
            .ok_or(crate::NativeArtifactAccessError::Stopped)?;
        control.try_acquire_artifact(capability)
    }
}

pub(super) struct HostLifecycleSignal {
    pub stop_requested: AtomicBool,
    pub view_close_requested: AtomicBool,
    pub prepare_handoff_pending: AtomicBool,
    #[cfg(feature = "capture")]
    pub capture_stopping: AtomicBool,
    #[cfg(feature = "capture")]
    pub capture_runtime_ready: AtomicBool,
    #[cfg(feature = "capture")]
    pub capture_drained: AtomicBool,
    #[cfg(feature = "capture")]
    pub capture_cpu_started: AtomicBool,
    #[cfg(feature = "window")]
    pub window_owner_expected: AtomicBool,
    #[cfg(feature = "window")]
    pub window_owner_observed: AtomicBool,
}

impl HostLifecycleSignal {
    pub(super) fn new() -> Self {
        Self {
            stop_requested: AtomicBool::new(false),
            view_close_requested: AtomicBool::new(false),
            prepare_handoff_pending: AtomicBool::new(false),
            #[cfg(feature = "capture")]
            capture_stopping: AtomicBool::new(false),
            #[cfg(feature = "capture")]
            capture_runtime_ready: AtomicBool::new(false),
            #[cfg(feature = "capture")]
            capture_drained: AtomicBool::new(false),
            #[cfg(feature = "capture")]
            capture_cpu_started: AtomicBool::new(false),
            #[cfg(feature = "window")]
            window_owner_expected: AtomicBool::new(false),
            #[cfg(feature = "window")]
            window_owner_observed: AtomicBool::new(false),
        }
    }
}

#[cfg(feature = "window")]
enum CoordinatorCommand {
    StartView {
        client: ManagedPacketWindowClient,
        view_override: Option<RenderView>,
        stage_limits: PacketStageLimits,
        reply: SyncSender<Result<PacketStageInput, NativeProjectHostError>>,
    },
}

/// Compact result from the one native managed view. Packet/source/GPU owners are not returned.
#[cfg(feature = "window")]
pub struct NativeViewOutcome {
    window: ManagedWindowOutcome,
    status: NativeProjectHostStatus,
}

#[cfg(feature = "window")]
impl NativeViewOutcome {
    pub fn window(&self) -> &ManagedWindowOutcome {
        &self.window
    }

    pub fn report(&self) -> &daedalus_renderer::WindowReport {
        self.window.report()
    }

    pub fn status(&self) -> &NativeProjectHostStatus {
        &self.status
    }
}

#[cfg(feature = "window")]
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("managed project view failed ({code}): {message}")]
pub struct NativeViewError {
    code: String,
    message: String,
}

#[cfg(feature = "window")]
impl NativeViewError {
    fn from_window(error: &daedalus_renderer::ManagedWindowError) -> Self {
        Self {
            code: format!("{:?}", error.code()).to_ascii_lowercase(),
            message: bounded(error.message().to_owned(), MAX_FAILURE_MESSAGE_BYTES),
        }
    }
}

pub struct NativeProjectHost {
    service: Arc<NativeProjectActivationService>,
    signal: Arc<HostLifecycleSignal>,
    status: Arc<Mutex<NativeProjectHostStatus>>,
    cpu: Option<CpuWorker>,
    #[cfg(feature = "window")]
    coordinator_commands: SyncSender<CoordinatorCommand>,
    coordinator: Option<JoinHandle<()>>,
    #[cfg(feature = "window")]
    window_control: Arc<Mutex<Option<ManagedPacketWindowControl>>>,
    #[cfg(feature = "window")]
    unpublished_gpu_owner: Option<WindowGpuOwner>,
    pending_shutdown_failure: Option<NativeProjectHostFailure>,
    pending_shutdown_lifecycle: Option<NativeProjectHostLifecycle>,
    #[cfg(feature = "capture")]
    capture_pipeline: Option<CapturePipelineOwner>,
    #[cfg(feature = "capture")]
    capture_window: Option<(ManagedPacketWindowClient, ManagedPacketWindowServer)>,
}

impl std::fmt::Debug for NativeProjectHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeProjectHost")
            .field("status", &self.status.try_lock().ok().as_deref())
            .field("coordinator_running", &self.coordinator.is_some())
            .field("cpu_owner_running", &self.cpu.is_some())
            .finish_non_exhaustive()
    }
}

impl NativeProjectHost {
    /// Starts one native project authority and its render-consumer capability.
    ///
    /// Source loading in the existing runtime factory is synchronous. After that factory returns,
    /// thread-start failures are retained in a failed host so callers can still stop the service.
    pub fn start(options: NativeProjectHostOptions) -> Result<Self, NativeProjectHostStartError> {
        let session = options.session.engine.session;
        let cpu_owner = PacketCpuOwner::new(
            options.cpu_limits,
            options.packet_limits,
            options.asset_limits,
        )?;
        #[cfg(feature = "capture")]
        let live_capture = options.live_capture;
        #[cfg(feature = "capture")]
        let frame_slots_limit = cpu_owner.status().frame_slots_limit;
        #[cfg(feature = "capture")]
        let (service, consumer, capture_handoff) = if live_capture {
            let (service, consumer, handoff) =
                NativeProjectActivationService::start_with_render_consumer_and_capture(
                    options.preparation,
                    options.session,
                )?;
            (service, consumer, Some(handoff))
        } else {
            let (service, consumer) = NativeProjectActivationService::start_with_render_consumer(
                options.preparation,
                options.session,
            )?;
            (service, consumer, None)
        };
        #[cfg(not(feature = "capture"))]
        let (service, consumer) = NativeProjectActivationService::start_with_render_consumer(
            options.preparation,
            options.session,
        )?;
        let service = Arc::new(service);
        let signal = Arc::new(HostLifecycleSignal::new());
        let initial_status = NativeProjectHostStatus {
            lifecycle: NativeProjectHostLifecycle::Initializing,
            session,
            active_generation: 1,
            active_source_digest: String::new(),
            cpu: PacketCpuStatus::default(),
            view_started: false,
            view_closed: false,
            last_queued_publication: None,
            last_presented: None,
            failure: None,
        };
        let status = Arc::new(Mutex::new(initial_status));
        #[cfg(feature = "window")]
        let (command_tx, command_rx) = mpsc::sync_channel(1);

        #[cfg(feature = "capture")]
        let mut capture_pipeline = if live_capture {
            match crate::capture::start_capture_pipeline(session, frame_slots_limit) {
                Ok(capture) => Some(capture),
                Err(error) => {
                    consumer.request_close();
                    service.request_stop();
                    set_failure(&status, "capture_pipeline_start_failed", error.to_string());
                    return Ok(Self::failed(service, signal, status, None, None, None));
                }
            }
        } else {
            None
        };
        #[cfg(feature = "capture")]
        let mut capture_endpoints = if live_capture {
            let (window_client, capture_client, server) = managed_packet_window_capture_channel();
            let worker = match capture_client.register_worker() {
                Ok(worker) => worker,
                Err(error) => {
                    consumer.request_close();
                    service.request_stop();
                    if let Some(capture) = capture_pipeline.as_ref() {
                        capture.request_stop();
                    }
                    set_failure(&status, "capture_endpoint_start_failed", error.to_string());
                    return Ok(Self::failed(
                        service,
                        signal,
                        status,
                        None,
                        capture_pipeline,
                        None,
                    ));
                }
            };
            Some((window_client, capture_client, server, worker))
        } else {
            None
        };

        let cpu_client = match service.session().attach() {
            Ok(client) => client,
            Err(error) => {
                consumer.request_close();
                service.request_stop();
                #[cfg(feature = "capture")]
                {
                    if let Some(handoff) = capture_handoff.as_ref() {
                        handoff.request_detach();
                    }
                    if let Some(capture) = capture_pipeline.as_ref() {
                        capture.request_stop();
                        capture.coordinator_port().close_transfer_fence();
                    }
                }
                set_failure(&status, "cpu_attach_failed", error.to_string());
                return Ok(Self::failed(
                    service,
                    signal,
                    status,
                    None,
                    #[cfg(feature = "capture")]
                    capture_pipeline,
                    #[cfg(feature = "capture")]
                    None,
                ));
            }
        };

        #[cfg(feature = "capture")]
        let (capture_window, capture_ports) =
            if let Some((window, client, server, worker)) = capture_endpoints.take() {
                let Some(pipeline) = capture_pipeline.as_mut() else {
                    consumer.request_close();
                    service.request_stop();
                    if let Some(handoff) = capture_handoff.as_ref() {
                        handoff.request_detach();
                    }
                    set_failure(
                        &status,
                        "capture_pipeline_missing",
                        "capture endpoints were started without their pipeline owner",
                    );
                    return Ok(Self::failed(service, signal, status, None, None, None));
                };
                let pipeline_port = match pipeline.take_cpu_worker_port() {
                    Ok(port) => port,
                    Err(error) => {
                        consumer.request_close();
                        service.request_stop();
                        handoff_detach(&capture_handoff);
                        pipeline.request_stop();
                        capture_fail(&status, "capture_pipeline_take_failed", error.to_string());
                        return Ok(Self::failed(
                            service,
                            signal,
                            status,
                            None,
                            capture_pipeline,
                            None,
                        ));
                    }
                };
                (
                    Some((window, server)),
                    Some(CpuCapturePorts {
                        pipeline: pipeline_port,
                        renderer_client: client,
                        renderer_worker: worker,
                    }),
                )
            } else {
                (None, None)
            };

        #[cfg(feature = "capture")]
        let cpu_result =
            cpu_worker::spawn(cpu_owner, cpu_client, Arc::clone(&signal), capture_ports);
        #[cfg(not(feature = "capture"))]
        let cpu_result = cpu_worker::spawn(cpu_owner, cpu_client, Arc::clone(&signal));
        let cpu = match cpu_result {
            Ok(cpu) => cpu,
            Err(error) => {
                consumer.request_close();
                service.request_stop();
                #[cfg(feature = "capture")]
                {
                    handoff_detach(&capture_handoff);
                    if let Some(capture) = capture_pipeline.as_ref() {
                        capture.request_stop();
                        capture.coordinator_port().close_transfer_fence();
                    }
                }
                set_failure(&status, "cpu_worker_start_failed", error.to_string());
                return Ok(Self::failed(
                    service,
                    signal,
                    status,
                    None,
                    #[cfg(feature = "capture")]
                    capture_pipeline,
                    #[cfg(feature = "capture")]
                    capture_window,
                ));
            }
        };
        #[cfg(feature = "capture")]
        if live_capture {
            signal.capture_cpu_started.store(true, Ordering::Release);
        }
        let coordinator_client = match service.session().attach() {
            Ok(client) => client,
            Err(error) => {
                signal.stop_requested.store(true, Ordering::Release);
                consumer.request_close();
                service.request_stop();
                #[cfg(feature = "capture")]
                {
                    handoff_detach(&capture_handoff);
                    if let Some(capture) = capture_pipeline.as_ref() {
                        capture.request_stop();
                        capture.coordinator_port().close_transfer_fence();
                    }
                }
                set_failure(&status, "coordinator_attach_failed", error.to_string());
                return Ok(Self::failed(
                    service,
                    signal,
                    status,
                    Some(cpu),
                    #[cfg(feature = "capture")]
                    capture_pipeline,
                    #[cfg(feature = "capture")]
                    None,
                ));
            }
        };

        let coordinator_service = Arc::clone(&service);
        let coordinator_signal = Arc::clone(&signal);
        let coordinator_status = Arc::clone(&status);
        let coordinator_cpu = cpu.client();
        #[cfg(feature = "capture")]
        let capture_coordinator = capture_pipeline
            .as_ref()
            .map(CapturePipelineOwner::coordinator_port);
        #[cfg(feature = "capture")]
        let capture_coordinator_cleanup = capture_coordinator.clone();
        let coordinator_spawn = thread::Builder::new()
            .name("daedalus-native-project-host".to_owned())
            .spawn(move || {
                run_coordinator(CoordinatorThreadInputs {
                    service: coordinator_service,
                    consumer,
                    client: coordinator_client,
                    cpu: coordinator_cpu,
                    #[cfg(feature = "window")]
                    commands: command_rx,
                    #[cfg(feature = "capture")]
                    capture_handoff,
                    #[cfg(feature = "capture")]
                    capture_coordinator,
                    signal: coordinator_signal,
                    status: coordinator_status,
                })
            });
        let coordinator = match coordinator_spawn {
            Ok(join) => Some(join),
            Err(error) => {
                signal.stop_requested.store(true, Ordering::Release);
                service.request_stop();
                #[cfg(feature = "capture")]
                {
                    if let Some(capture) = capture_pipeline.as_ref() {
                        capture.request_stop();
                    }
                    if let Some(capture) = capture_coordinator_cleanup.as_ref() {
                        capture.close_transfer_fence();
                    }
                }
                set_failure(&status, "coordinator_start_failed", error.to_string());
                None
            }
        };

        Ok(Self {
            service,
            signal,
            status,
            cpu: Some(cpu),
            #[cfg(feature = "window")]
            coordinator_commands: command_tx,
            coordinator,
            #[cfg(feature = "window")]
            window_control: Arc::new(Mutex::new(None)),
            #[cfg(feature = "window")]
            unpublished_gpu_owner: None,
            pending_shutdown_failure: None,
            pending_shutdown_lifecycle: None,
            #[cfg(feature = "capture")]
            capture_pipeline,
            #[cfg(feature = "capture")]
            capture_window,
        })
    }

    fn failed(
        service: Arc<NativeProjectActivationService>,
        signal: Arc<HostLifecycleSignal>,
        status: Arc<Mutex<NativeProjectHostStatus>>,
        cpu: Option<CpuWorker>,
        #[cfg(feature = "capture")] capture_pipeline: Option<CapturePipelineOwner>,
        #[cfg(feature = "capture")] capture_window: Option<(
            ManagedPacketWindowClient,
            ManagedPacketWindowServer,
        )>,
    ) -> Self {
        signal.stop_requested.store(true, Ordering::Release);
        #[cfg(feature = "capture")]
        signal.capture_stopping.store(true, Ordering::Release);
        #[cfg(feature = "capture")]
        if let Some(capture) = capture_pipeline.as_ref() {
            capture.request_stop();
        }
        Self {
            service,
            signal,
            status,
            cpu,
            coordinator: None,
            #[cfg(feature = "window")]
            coordinator_commands: mpsc::sync_channel(1).0,
            #[cfg(feature = "window")]
            window_control: Arc::new(Mutex::new(None)),
            #[cfg(feature = "window")]
            unpublished_gpu_owner: None,
            pending_shutdown_failure: None,
            pending_shutdown_lifecycle: None,
            #[cfg(feature = "capture")]
            capture_pipeline,
            #[cfg(feature = "capture")]
            capture_window,
        }
    }

    /// Attaches a new client to the same actor. It never creates another session.
    pub fn attach_control(&self) -> Result<SessionControlClient, NativeProjectHostError> {
        if self.signal.stop_requested.load(Ordering::Acquire) {
            return Err(NativeProjectHostError::Stopped);
        }
        self.service.session().attach().map_err(Into::into)
    }

    pub fn control(&self) -> NativeProjectHostControl {
        NativeProjectHostControl {
            signal: Arc::clone(&self.signal),
            status: Arc::clone(&self.status),
            #[cfg(feature = "window")]
            window: self.window_control_arc(),
            #[cfg(feature = "capture")]
            capture_control: self
                .capture_pipeline
                .as_ref()
                .map(CapturePipelineOwner::control_port),
        }
    }

    #[cfg(feature = "window")]
    fn window_control_arc(&self) -> Arc<Mutex<Option<ManagedPacketWindowControl>>> {
        Arc::clone(&self.window_control)
    }

    pub fn try_status(
        &self,
    ) -> Result<NativeProjectHostStatusRead, NativeProjectHostStatusReadError> {
        self.control().try_status()
    }

    /// Returns the runtime's actual shutdown report once the actor has finished.
    pub fn shutdown_report(&self) -> Option<&SessionShutdownReport> {
        self.service.shutdown_report()
    }

    /// Requests whole-host shutdown without joining unfinished workers.
    pub fn request_stop(&self) {
        self.signal.stop_requested.store(true, Ordering::Release);
        #[cfg(feature = "capture")]
        {
            self.signal.capture_stopping.store(true, Ordering::Release);
            if let Some(capture) = self.capture_pipeline.as_ref() {
                capture.request_stop();
            }
        }
        self.service.request_stop();
        #[cfg(feature = "window")]
        if let Ok(window) = self.window_control_arc().try_lock()
            && let Some(window) = window.as_ref()
        {
            window.request_host_stop();
        }
    }

    /// Runs the one native view on the calling thread and returns the owner to the CPU worker.
    #[cfg(feature = "window")]
    pub fn run_view(
        &mut self,
        options: NativeProjectViewOptions,
    ) -> Result<NativeViewOutcome, NativeViewError> {
        self.run_view_with_input_policy(options, WindowInputPolicy::Native)
    }

    /// Runs the one native view with an explicit policy for physical window input.
    #[cfg(feature = "window")]
    pub fn run_view_with_input_policy(
        &mut self,
        options: NativeProjectViewOptions,
        input_policy: WindowInputPolicy,
    ) -> Result<NativeViewOutcome, NativeViewError> {
        if self.signal.stop_requested.load(Ordering::Acquire) {
            return Err(NativeViewError {
                code: "stopped".to_owned(),
                message: "native project host is stopping".to_owned(),
            });
        }
        {
            let mut status = self.status.lock().map_err(|_| NativeViewError {
                code: "status_poisoned".to_owned(),
                message: "native host status is unavailable".to_owned(),
            })?;
            if status.view_started {
                return Err(NativeViewError {
                    code: "view_already_used".to_owned(),
                    message: "the native project view can only be run once".to_owned(),
                });
            }
            status.view_started = true;
            status.lifecycle = NativeProjectHostLifecycle::ViewRunning;
        }
        if self.cpu.is_none() || self.coordinator.is_none() {
            return Err(NativeViewError {
                code: "host_failed".to_owned(),
                message: "native project host workers did not start".to_owned(),
            });
        }

        #[cfg(feature = "capture")]
        let (client, server) = if self.capture_pipeline.is_some() {
            self.capture_window.take().ok_or_else(|| NativeViewError {
                code: "capture_window_unavailable".to_owned(),
                message: "the capture-enabled managed window endpoints are unavailable".to_owned(),
            })?
        } else {
            managed_packet_window_channel()
        };
        #[cfg(not(feature = "capture"))]
        let (client, server) = managed_packet_window_channel();
        let control = client.control();
        if let Ok(mut shared) = self.window_control_arc().try_lock() {
            *shared = Some(control);
        }
        self.signal
            .window_owner_expected
            .store(true, Ordering::Release);
        self.signal
            .window_owner_observed
            .store(false, Ordering::Release);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let command = CoordinatorCommand::StartView {
            client,
            view_override: options.view_override.clone(),
            stage_limits: options.window.initial_stage_limits,
            reply: reply_tx,
        };
        let mut pending_command = Some(command);
        let initial = loop {
            if self.signal.stop_requested.load(Ordering::Acquire) {
                self.signal
                    .window_owner_expected
                    .store(false, Ordering::Release);
                return Err(NativeViewError {
                    code: "stopped".to_owned(),
                    message: "native project host stopped before initial device preparation"
                        .to_owned(),
                });
            }
            if let Some(command) = pending_command.take() {
                match self.coordinator_commands.try_send(command) {
                    Ok(()) => {}
                    Err(TrySendError::Full(command)) => pending_command = Some(command),
                    Err(TrySendError::Disconnected(_)) => {
                        self.signal
                            .window_owner_expected
                            .store(false, Ordering::Release);
                        set_failure(
                            &self.status,
                            "coordinator_stopped",
                            "host coordinator stopped before view startup",
                        );
                        return Err(NativeViewError {
                            code: "coordinator_stopped".to_owned(),
                            message: "host coordinator stopped before view startup".to_owned(),
                        });
                    }
                }
            }
            match reply_rx.try_recv() {
                Ok(Ok(input)) => break input,
                Ok(Err(error)) => {
                    self.signal
                        .window_owner_expected
                        .store(false, Ordering::Release);
                    return Err(NativeViewError {
                        code: "initial_cpu_prepare".to_owned(),
                        message: error.to_string(),
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    self.signal
                        .window_owner_expected
                        .store(false, Ordering::Release);
                    return Err(NativeViewError {
                        code: "initial_stage_disconnected".to_owned(),
                        message: "host coordinator did not return the initial stage input"
                            .to_owned(),
                    });
                }
                Err(TryRecvError::Empty) => thread::sleep(COORDINATOR_POLL),
            }
        };

        let run = run_packet_windowed_managed_with_input_policy(
            initial,
            server,
            options.window,
            input_policy,
        );
        let daedalus_renderer::ManagedWindowRun {
            outcome,
            status: window_status,
            gpu_owner,
        } = run;
        let transfer = if let Some(cpu) = self.cpu.as_ref() {
            cpu.gpu_owner_return.publish(gpu_owner)
        } else {
            Err(gpu_owner)
        };
        match transfer {
            Ok(()) => {}
            Err(owner) => {
                self.unpublished_gpu_owner = Some(owner);
                self.pending_shutdown_failure = Some(NativeProjectHostFailure::new(
                    "gpu_owner_return_failed",
                    "the one-shot managed window owner disposition was already used",
                ));
                self.pending_shutdown_lifecycle = Some(NativeProjectHostLifecycle::Stopping);
            }
        }
        self.flush_shutdown_metadata();
        if let Ok(mut status) = self.status.lock() {
            status.view_closed = true;
            if status.lifecycle != NativeProjectHostLifecycle::Stopping
                && status.lifecycle != NativeProjectHostLifecycle::Failed
            {
                status.lifecycle = NativeProjectHostLifecycle::ViewClosed;
            }
            if let Some(presented) = window_status.last_presented() {
                update_last_presented(&mut status, summary_from_managed(presented));
            }
        }
        if let Ok(mut control) = self.window_control_arc().try_lock() {
            *control = None;
        }
        match outcome {
            Ok(window) => {
                let status = self
                    .status
                    .lock()
                    .map(|status| status.clone())
                    .map_err(|_| NativeViewError {
                        code: "status_poisoned".to_owned(),
                        message: "host status unavailable after managed window completion"
                            .to_owned(),
                    })?;
                Ok(NativeViewOutcome { window, status })
            }
            Err(error) => {
                set_failure(&self.status, "managed_window_failed", error.message());
                Err(NativeViewError::from_window(&error))
            }
        }
    }

    /// Advances shutdown only through finished coordinator, CPU, actor, and preparation owners.
    pub fn try_shutdown(&mut self) -> Result<NativeProjectHostShutdown, NativeProjectHostError> {
        self.request_stop();
        self.drain_failure_result();
        self.pending_shutdown_lifecycle
            .get_or_insert(NativeProjectHostLifecycle::Stopping);
        self.flush_shutdown_metadata();

        if let Some(join) = self.coordinator.as_ref() {
            if !join.is_finished() {
                self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                self.flush_shutdown_metadata();
                return Ok(NativeProjectHostShutdown::Stopping);
            }
            if let Some(join) = self.coordinator.take()
                && join.join().is_err()
            {
                self.queue_shutdown_failure(
                    "coordinator_panicked",
                    "native host coordinator panicked",
                );
            }
        }

        self.drain_failure_result();
        self.flush_shutdown_metadata();
        if let Some(cpu) = self.cpu.as_ref() {
            if !cpu.join.is_finished() {
                self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                self.flush_shutdown_metadata();
                return Ok(NativeProjectHostShutdown::Stopping);
            }
            if let Some(cpu) = self.cpu.take()
                && cpu.join.join().is_err()
            {
                self.queue_shutdown_failure(
                    "cpu_owner_panicked",
                    "native host CPU owner worker panicked",
                );
            }
        }

        #[cfg(feature = "capture")]
        if !self.signal.capture_cpu_started.load(Ordering::Acquire)
            && let Some(capture) = self.capture_pipeline.as_mut()
        {
            match capture.try_finish_unclaimed() {
                CaptureEncoderShutdown::Stopping => {
                    self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                    self.flush_shutdown_metadata();
                    return Ok(NativeProjectHostShutdown::Stopping);
                }
                CaptureEncoderShutdown::Stopped | CaptureEncoderShutdown::Failed(_) => {
                    let drained = capture.control_port().try_status().is_ok_and(|status| {
                        status.worker_exited()
                            && status.pending_jobs() == 0
                            && status.cleanup_pending() == 0
                            && status.retained_png_bytes() == 0
                    });
                    if !drained {
                        self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                        self.flush_shutdown_metadata();
                        return Ok(NativeProjectHostShutdown::Stopping);
                    }
                    self.signal.capture_drained.store(true, Ordering::Release);
                }
            }
        }

        #[cfg(feature = "window")]
        if self.unpublished_gpu_owner.is_some()
            || self.signal.window_owner_expected.load(Ordering::Acquire)
                && !self.signal.window_owner_observed.load(Ordering::Acquire)
        {
            self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
            self.flush_shutdown_metadata();
            return Ok(NativeProjectHostShutdown::Stopping);
        }
        let service_result = match Arc::get_mut(&mut self.service) {
            Some(service) => service.try_shutdown(),
            None => {
                self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                self.flush_shutdown_metadata();
                return Ok(NativeProjectHostShutdown::Stopping);
            }
        };
        match service_result {
            Ok(NativeProjectActivationShutdown::Stopped) => {
                let (published_failure, status_busy, status_poisoned) = {
                    match self.status.try_lock() {
                        Ok(status) => (Some(status.failure.is_some()), false, false),
                        Err(TryLockError::WouldBlock) => (None, true, false),
                        Err(TryLockError::Poisoned(_)) => (None, false, true),
                    }
                };
                if status_busy {
                    self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                    self.flush_shutdown_metadata();
                    return Ok(NativeProjectHostShutdown::Stopping);
                }
                if status_poisoned {
                    self.queue_shutdown_failure(
                        "host_status_poisoned",
                        "host status could not be read during shutdown",
                    );
                    self.flush_shutdown_metadata();
                    return Err(NativeProjectHostError::Failed(
                        "host status is poisoned".to_owned(),
                    ));
                }
                let published_failure = published_failure
                    .expect("non-busy status probe returns a value or poison result");
                let has_failure = self.pending_shutdown_failure.is_some() || published_failure;
                self.queue_shutdown_lifecycle(if has_failure {
                    NativeProjectHostLifecycle::Failed
                } else {
                    NativeProjectHostLifecycle::Stopped
                });
                if self.flush_shutdown_metadata() {
                    Ok(NativeProjectHostShutdown::Stopped)
                } else {
                    Ok(NativeProjectHostShutdown::Stopping)
                }
            }
            Ok(NativeProjectActivationShutdown::Stopping) => {
                self.queue_shutdown_lifecycle(NativeProjectHostLifecycle::Stopping);
                self.flush_shutdown_metadata();
                Ok(NativeProjectHostShutdown::Stopping)
            }
            Err(error) => {
                self.queue_shutdown_failure("runtime_shutdown_failed", error.to_string());
                self.flush_shutdown_metadata();
                Err(NativeProjectHostError::Shutdown(error.to_string()))
            }
        }
    }

    fn drain_failure_result(&mut self) {
        let failure = self.cpu.as_ref().and_then(|cpu| {
            let mut delivery = cpu.failure.try_take()?;
            delivery.take_payload()
        });
        if let Some(failure) = failure {
            self.queue_shutdown_failure(failure.code, failure.message);
        }
    }

    fn queue_shutdown_failure(&mut self, code: &str, message: impl Into<String>) {
        if self.pending_shutdown_failure.is_none() {
            self.pending_shutdown_failure = Some(NativeProjectHostFailure::new(code, message));
        }
        self.pending_shutdown_lifecycle
            .get_or_insert(NativeProjectHostLifecycle::Stopping);
    }

    fn queue_shutdown_lifecycle(&mut self, lifecycle: NativeProjectHostLifecycle) {
        self.pending_shutdown_lifecycle = Some(lifecycle);
    }

    /// Applies pending shutdown metadata without waiting behind a status reader.
    fn flush_shutdown_metadata(&mut self) -> bool {
        let mut status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return false,
        };
        if let Some(failure) = self.pending_shutdown_failure.take() {
            if status.failure.is_none() {
                status.failure = Some(failure);
            }
            if status.lifecycle != NativeProjectHostLifecycle::Stopping {
                status.lifecycle = NativeProjectHostLifecycle::Failed;
            }
        }
        if let Some(lifecycle) = self.pending_shutdown_lifecycle.take() {
            status.lifecycle =
                if lifecycle == NativeProjectHostLifecycle::Stopped && status.failure.is_some() {
                    NativeProjectHostLifecycle::Failed
                } else {
                    lifecycle
                };
        }
        true
    }
}

impl Drop for NativeProjectHost {
    fn drop(&mut self) {
        self.request_stop();
        #[cfg(feature = "window")]
        if let Some(owner) = self.unpublished_gpu_owner.take() {
            // Only the impossible second publication path reaches this branch: the public host
            // permits one run_view call and the preallocated one-shot slot cannot be Busy.
            // Keep that invariant-failure owner alive rather than dropping it on this thread.
            std::mem::forget(owner);
        }
    }
}

struct CoordinatorGeneration {
    identity: ProjectSourceIdentity,
    cpu_generation: daedalus_renderer::CpuGenerationId,
    #[cfg(feature = "window")]
    key: ManagedStageKey,
    #[cfg(feature = "window")]
    cancellation: Arc<AtomicBool>,
    initial: bool,
    cpu_prepared: bool,
    #[cfg(feature = "window")]
    stage_requested: bool,
    #[cfg(feature = "window")]
    stage_submitted: bool,
    #[cfg(feature = "window")]
    device_ready: bool,
    #[cfg(feature = "window")]
    report_pending: bool,
    #[cfg(feature = "window")]
    ready_applied: bool,
}

#[cfg(feature = "window")]
struct PendingStageInput {
    identity: ProjectSourceIdentity,
    generation: daedalus_renderer::CpuGenerationId,
    key: ManagedStageKey,
    input: Option<PacketStageInput>,
    initial: bool,
    delivery: ResultDelivery<StageInputResult>,
}

#[cfg(feature = "window")]
struct PendingFrameInput {
    key: ManagedStageKey,
    publication: ProjectPublicationIdentity,
    frame: Arc<daedalus_renderer::CpuFrameHandle>,
    delivery: ResultDelivery<LatestFrameResult>,
}

#[cfg(feature = "window")]
struct PendingStageReport {
    identity: ProjectSourceIdentity,
    outcome: ProjectConsumerStageOutcome,
}

#[cfg(feature = "window")]
struct PendingPresentedAck {
    presented: ProjectPresentedFrame,
    publication_sequence: u64,
    generation: u64,
    queued: bool,
}

#[cfg(feature = "window")]
#[derive(Clone)]
struct PendingPublication {
    identity: ProjectPublicationIdentity,
    key: ManagedStageKey,
}

#[cfg(feature = "window")]
struct AcknowledgedPublication {
    identity: ProjectPublicationIdentity,
    key: ManagedStageKey,
    present_sequence: u64,
}

#[cfg(feature = "capture")]
struct PendingCapture {
    request: Option<Box<CaptureRequest>>,
    stage_key: Option<ManagedStageKey>,
    completion: Option<CaptureCompletion>,
}

#[cfg(all(test, feature = "capture"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureCoordinatorPanicPoint {
    AfterTake,
    AfterTransfer,
}

struct CoordinatorState {
    bindings: Option<Arc<BTreeMap<StableId, EntityHandle>>>,
    pending_prepare: Option<PrepareRequest>,
    preparing: Option<(ProjectSourceIdentity, Arc<AtomicBool>)>,
    generations: Vec<CoordinatorGeneration>,
    #[cfg(feature = "window")]
    next_stage_key: ManagedStageKey,
    initial_lease_taken: bool,
    #[cfg(feature = "window")]
    window_client: Option<ManagedPacketWindowClient>,
    #[cfg(feature = "window")]
    window_control: Option<ManagedPacketWindowControl>,
    #[cfg(feature = "window")]
    window_started: bool,
    #[cfg(feature = "window")]
    initial_stage_limits: PacketStageLimits,
    #[cfg(feature = "window")]
    initial_device_ready: bool,
    #[cfg(feature = "window")]
    active_view_generation: Option<u64>,
    #[cfg(feature = "window")]
    start_reply: Option<SyncSender<Result<PacketStageInput, NativeProjectHostError>>>,
    #[cfg(feature = "window")]
    pending_stage_request: Option<StageInputRequest>,
    #[cfg(feature = "window")]
    pending_stage_input: Option<PendingStageInput>,
    #[cfg(feature = "window")]
    pending_stage_report: Option<PendingStageReport>,
    #[cfg(feature = "window")]
    pending_frame: Option<PendingFrameInput>,
    #[cfg(feature = "window")]
    frame_request_in_flight: bool,
    #[cfg(feature = "window")]
    last_frame_request: Instant,
    #[cfg(feature = "window")]
    pending_publications: VecDeque<PendingPublication>,
    #[cfg(feature = "window")]
    last_acked_publication: Option<AcknowledgedPublication>,
    #[cfg(feature = "window")]
    pending_ack: Option<PendingPresentedAck>,
    pending_retire: VecDeque<daedalus_renderer::CpuGenerationId>,
    #[cfg(feature = "window")]
    view_override: Option<RenderView>,
    #[cfg(feature = "window")]
    closing_view: bool,
    #[cfg(feature = "capture")]
    capture_handoff: Option<CaptureHandoffPort>,
    #[cfg(feature = "capture")]
    capture_coordinator: Option<CaptureCoordinatorPort>,
    #[cfg(feature = "capture")]
    pending_capture: Option<PendingCapture>,
    #[cfg(all(test, feature = "capture"))]
    capture_test_panic: Option<CaptureCoordinatorPanicPoint>,
    worker_failed: bool,
}

impl CoordinatorState {
    fn new() -> Self {
        Self {
            bindings: None,
            pending_prepare: None,
            preparing: None,
            generations: Vec::with_capacity(2),
            #[cfg(feature = "window")]
            next_stage_key: ManagedStageKey::INITIAL,
            initial_lease_taken: false,
            #[cfg(feature = "window")]
            window_client: None,
            #[cfg(feature = "window")]
            window_control: None,
            #[cfg(feature = "window")]
            window_started: false,
            #[cfg(feature = "window")]
            initial_stage_limits: PacketStageLimits::default(),
            #[cfg(feature = "window")]
            initial_device_ready: false,
            #[cfg(feature = "window")]
            active_view_generation: None,
            #[cfg(feature = "window")]
            start_reply: None,
            #[cfg(feature = "window")]
            pending_stage_request: None,
            #[cfg(feature = "window")]
            pending_stage_input: None,
            #[cfg(feature = "window")]
            pending_stage_report: None,
            #[cfg(feature = "window")]
            pending_frame: None,
            #[cfg(feature = "window")]
            frame_request_in_flight: false,
            #[cfg(feature = "window")]
            last_frame_request: Instant::now(),
            #[cfg(feature = "window")]
            pending_publications: VecDeque::with_capacity(MAX_PRESENTATION_IDENTITIES),
            #[cfg(feature = "window")]
            last_acked_publication: None,
            #[cfg(feature = "window")]
            pending_ack: None,
            pending_retire: VecDeque::with_capacity(2),
            #[cfg(feature = "window")]
            view_override: None,
            #[cfg(feature = "window")]
            closing_view: false,
            #[cfg(feature = "capture")]
            capture_handoff: None,
            #[cfg(feature = "capture")]
            capture_coordinator: None,
            #[cfg(feature = "capture")]
            pending_capture: None,
            #[cfg(all(test, feature = "capture"))]
            capture_test_panic: None,
            worker_failed: false,
        }
    }

    #[cfg(feature = "window")]
    fn generation(&self, identity: &ProjectSourceIdentity) -> Option<&CoordinatorGeneration> {
        self.generations
            .iter()
            .find(|record| &record.identity == identity)
    }

    #[cfg(feature = "window")]
    fn generation_mut(
        &mut self,
        identity: &ProjectSourceIdentity,
    ) -> Option<&mut CoordinatorGeneration> {
        self.generations
            .iter_mut()
            .find(|record| &record.identity == identity)
    }

    fn queue_retire(&mut self, generation: daedalus_renderer::CpuGenerationId) {
        if !self.pending_retire.contains(&generation) && self.pending_retire.len() < 2 {
            self.pending_retire.push_back(generation);
        }
    }

    #[cfg(feature = "window")]
    fn drop_window_payloads(&mut self) {
        self.pending_stage_input.take();
        self.pending_frame.take();
        self.pending_ack.take();
        self.pending_publications.clear();
        self.start_reply.take();
    }
}

struct CoordinatorThreadInputs {
    service: Arc<NativeProjectActivationService>,
    consumer: ProjectGenerationConsumerPort,
    client: SessionControlClient,
    cpu: CpuWorkerClient,
    #[cfg(feature = "window")]
    commands: Receiver<CoordinatorCommand>,
    #[cfg(feature = "capture")]
    capture_handoff: Option<CaptureHandoffPort>,
    #[cfg(feature = "capture")]
    capture_coordinator: Option<CaptureCoordinatorPort>,
    signal: Arc<HostLifecycleSignal>,
    status: Arc<Mutex<NativeProjectHostStatus>>,
}

fn run_coordinator(inputs: CoordinatorThreadInputs) {
    let CoordinatorThreadInputs {
        service,
        consumer,
        client,
        cpu,
        #[cfg(feature = "window")]
        commands,
        #[cfg(feature = "capture")]
        capture_handoff,
        #[cfg(feature = "capture")]
        capture_coordinator,
        signal,
        status,
    } = inputs;
    let session = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .session;
    let mut state = CoordinatorState::new();
    #[cfg(feature = "capture")]
    {
        state.capture_handoff = capture_handoff;
        state.capture_coordinator = capture_coordinator;
    }
    let mut fatal = false;
    loop {
        #[cfg(feature = "capture")]
        take_capture_request(&mut state, &signal, &status);
        let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coordinator_step(
                &mut state,
                &service,
                &consumer,
                &client,
                &cpu,
                #[cfg(feature = "window")]
                &commands,
                &signal,
                &status,
                session,
            );
        }));
        if step.is_err() {
            fatal = true;
            signal.stop_requested.store(true, Ordering::Release);
            set_failure(
                &status,
                "coordinator_panicked",
                "native project coordinator job panicked",
            );
        }

        if signal.stop_requested.load(Ordering::Acquire) || fatal {
            #[cfg(feature = "capture")]
            signal.capture_stopping.store(true, Ordering::Release);
            service.request_stop();
            consumer.request_close();
            if let Some((_, cancelled)) = state.preparing.as_ref() {
                cancelled.store(true, Ordering::Release);
            }
            if let Some((_, cancelled)) = state.pending_prepare.as_ref().map(|request| {
                (
                    request.lease.identity().clone(),
                    Arc::clone(&request.cancelled),
                )
            }) {
                cancelled.store(true, Ordering::Release);
            }
            #[cfg(feature = "window")]
            if let Some(control) = state.window_control.as_ref() {
                control.request_host_stop();
            }
            #[cfg(feature = "capture")]
            {
                if let Some(handoff) = state.capture_handoff.as_ref() {
                    handoff.request_detach();
                }
                reject_pending_capture(&mut state);
                advance_pending_capture(&mut state, None, true);
                if state.pending_capture.is_none()
                    && let Some(capture) = state.capture_coordinator.as_ref()
                {
                    capture.close_transfer_fence();
                }
            }
            // A move-only lease that has not yet entered the worker must be handed to its
            // owner thread before the coordinator exits. The CPU stop path drains it there.
            flush_prepare_request(&mut state, &cpu, &signal);
            #[cfg(feature = "window")]
            state.drop_window_payloads();
            while let Some(mut delivery) = cpu.prepared.try_take() {
                delivery.take_payload();
            }
            #[cfg(feature = "window")]
            while let Some(mut delivery) = cpu.stage_input_result.try_take() {
                delivery.take_payload();
            }
            #[cfg(feature = "window")]
            while let Some(mut delivery) = cpu.latest_frame_result.try_take() {
                delivery.take_payload();
            }
            while let Some(mut delivery) = cpu.failure.try_take() {
                if let Some(failure) = delivery.take_payload() {
                    set_failure(&status, failure.code, failure.message);
                }
            }
            #[cfg(feature = "capture")]
            let capture_drained = state
                .capture_coordinator
                .as_ref()
                .is_none_or(CaptureCoordinatorPort::is_drained);
            #[cfg(not(feature = "capture"))]
            let capture_drained = true;
            if state.pending_prepare.is_none()
                && !signal.prepare_handoff_pending.load(Ordering::Acquire)
                && capture_drained
            {
                break;
            }
            thread::sleep(COORDINATOR_POLL);
            continue;
        }
        thread::sleep(COORDINATOR_POLL);
    }
}

#[allow(clippy::too_many_arguments)]
fn coordinator_step(
    state: &mut CoordinatorState,
    service: &NativeProjectActivationService,
    consumer: &ProjectGenerationConsumerPort,
    client: &SessionControlClient,
    cpu: &CpuWorkerClient,
    #[cfg(feature = "window")] commands: &Receiver<CoordinatorCommand>,
    signal: &HostLifecycleSignal,
    status: &Mutex<NativeProjectHostStatus>,
    session: SessionId,
) {
    if state.bindings.is_none() {
        match collect_bindings(client, session, &signal.stop_requested) {
            Ok(bindings) => state.bindings = Some(Arc::new(bindings)),
            Err(failure) => {
                set_failure(status, failure.code, failure.message);
                signal.stop_requested.store(true, Ordering::Release);
                return;
            }
        }
    }

    #[cfg(feature = "window")]
    {
        while let Ok(command) = commands.try_recv() {
            match command {
                CoordinatorCommand::StartView {
                    client,
                    view_override,
                    stage_limits,
                    reply,
                } if !state.window_started => {
                    state.window_started = true;
                    state.window_control = Some(client.control());
                    state.window_client = Some(client);
                    state.view_override = view_override;
                    state.initial_stage_limits = stage_limits;
                    state.start_reply = Some(reply);
                }
                CoordinatorCommand::StartView { .. } => {}
            }
        }
    }

    let runtime_status = match consumer.try_status() {
        Ok(snapshot) => {
            synchronize_runtime_status(status, &snapshot);
            Some(snapshot)
        }
        Err(ProjectConsumerError::Busy) => None,
        Err(error) => {
            set_failure(status, "consumer_status_failed", error.to_string());
            None
        }
    };
    #[cfg(feature = "capture")]
    {
        update_capture_readiness(state, signal, status);
        advance_pending_capture(state, runtime_status.as_ref(), false);
    }
    if let Some(runtime_status) = runtime_status.as_ref()
        && let Some((identity, cancelled)) = state.preparing.as_ref()
        && !is_identity_current(runtime_status, identity)
    {
        cancelled.store(true, Ordering::Release);
        state.preparing = None;
    }
    #[cfg(feature = "window")]
    if let Some(runtime_status) = runtime_status.as_ref() {
        if runtime_status.is_closed() || runtime_status.is_stopped() {
            state.closing_view = true;
            if let Some(control) = state.window_control.as_ref() {
                control.request_cancel_through(ManagedStageKey::INITIAL);
            }
        }
        update_generation_progress(state, runtime_status);
    }

    poll_cpu_results(state, consumer, cpu, status, runtime_status.as_ref());
    flush_prepare_request(state, cpu, signal);
    flush_retire_requests(state, cpu);
    #[cfg(feature = "window")]
    retry_stage_report(state, consumer, status);

    if !state.initial_lease_taken {
        match consumer.try_take_initial() {
            Ok(Some(lease)) => {
                state.initial_lease_taken = true;
                set_lifecycle(status, NativeProjectHostLifecycle::InitialPreparing);
                queue_prepare(state, lease, signal);
            }
            Ok(None) | Err(ProjectConsumerError::Busy) => {}
            Err(error) => {
                set_failure(status, "initial_source_failed", error.to_string());
                signal.stop_requested.store(true, Ordering::Release);
                return;
            }
        }
    }
    #[cfg(feature = "window")]
    if state.pending_prepare.is_none()
        && state.preparing.is_none()
        && !state.closing_view
        && let Some(bindings) = state.bindings.as_ref()
    {
        match consumer.try_take_candidate() {
            Ok(Some(lease)) => {
                queue_prepare_with_bindings(state, lease, Arc::clone(bindings), signal)
            }
            Ok(None) | Err(ProjectConsumerError::Busy) => {}
            Err(ProjectConsumerError::Closed | ProjectConsumerError::Stopped) => {}
            Err(error) => set_failure(status, "candidate_source_failed", error.to_string()),
        }
    }

    #[cfg(feature = "window")]
    {
        poll_window_events(state, consumer, cpu, status);
        request_stage_inputs(state, cpu, runtime_status.as_ref());
        flush_pending_stage(state, status, runtime_status.as_ref());
        request_latest_frame(state, cpu, runtime_status.as_ref());
        flush_pending_frame(state, status);
        poll_presented(state, consumer, status);
        retry_present_ack(state, consumer, status);
    }

    #[cfg(feature = "window")]
    if signal.view_close_requested.load(Ordering::Acquire) && !state.closing_view {
        state.closing_view = true;
        consumer.request_close();
        if let Some(control) = state.window_control.as_ref() {
            control.request_view_close();
        }
    }
    #[cfg(feature = "window")]
    if let Some(runtime_status) = runtime_status.as_ref() {
        retire_obsolete_generations(state, runtime_status);
    }
    if let Some(cpu_status) = cpu.status.try_take()
        && let Ok(mut host_status) = status.lock()
    {
        host_status.cpu = cpu_status;
    }
    if let Some(mut delivery) = cpu.failure.try_take()
        && let Some(failure) = delivery.take_payload()
    {
        set_failure(status, failure.code, failure.message);
    }
    if let Ok(mut status) = status.lock()
        && status.lifecycle == NativeProjectHostLifecycle::InitialPreparing
        && state
            .generations
            .iter()
            .any(|record| record.initial && record.cpu_prepared)
    {
        #[cfg(feature = "window")]
        {
            status.lifecycle = if state.initial_device_ready {
                NativeProjectHostLifecycle::Ready
            } else {
                NativeProjectHostLifecycle::CpuPrepared
            };
        }
        #[cfg(not(feature = "window"))]
        {
            status.lifecycle = NativeProjectHostLifecycle::CpuPrepared;
        }
    }
    let _ = service;
}

#[cfg(feature = "capture")]
fn take_capture_request(
    state: &mut CoordinatorState,
    signal: &HostLifecycleSignal,
    status: &Mutex<NativeProjectHostStatus>,
) {
    // `None` means the coordinator completed the nonblocking move into the capacity-one ingress.
    // If an unwind landed between that move and clearing the local marker, reconcile the marker
    // before polling the actor again; the request itself remains in ingress.
    if state
        .pending_capture
        .as_ref()
        .is_some_and(|pending| pending.request.is_none())
    {
        state.pending_capture = None;
    }
    if state.pending_capture.is_some() {
        return;
    }
    let Some(handoff) = state.capture_handoff.as_ref() else {
        return;
    };
    match handoff.try_take_request() {
        Ok(Some(request)) => {
            state.pending_capture = Some(PendingCapture {
                request: Some(Box::new(request)),
                stage_key: None,
                completion: None,
            });
            #[cfg(test)]
            panic_if_capture_test_requested(
                &mut state.capture_test_panic,
                CaptureCoordinatorPanicPoint::AfterTake,
            );
        }
        Ok(None) | Err(CaptureHandoffError::Busy | CaptureHandoffError::RequestOccupied) => {}
        Err(CaptureHandoffError::Detached | CaptureHandoffError::AlreadyResolved) => {}
        Err(error) => {
            set_failure(status, "capture_handoff_failed", error.to_string());
            signal.stop_requested.store(true, Ordering::Release);
        }
    }
}

#[cfg(feature = "capture")]
fn update_capture_readiness(
    state: &CoordinatorState,
    signal: &HostLifecycleSignal,
    host_status: &Mutex<NativeProjectHostStatus>,
) {
    let Some(handoff) = state.capture_handoff.as_ref() else {
        return;
    };
    if signal.capture_stopping.load(Ordering::Acquire)
        || signal.stop_requested.load(Ordering::Acquire)
    {
        handoff.request_detach();
        return;
    }
    let Some(coordinator) = state.capture_coordinator.as_ref() else {
        return;
    };
    let pipeline = match coordinator.try_status() {
        Ok(status) => status,
        Err(CaptureStatusReadError::Busy) => return,
        Err(CaptureStatusReadError::Poisoned) => {
            set_failure(
                host_status,
                "capture_status_poisoned",
                "capture pipeline status could not be read",
            );
            handoff.request_detach();
            signal.stop_requested.store(true, Ordering::Release);
            return;
        }
    };
    if pipeline.session() != handoff.session() {
        set_failure(
            host_status,
            "capture_session_mismatch",
            "capture pipeline belongs to a different session",
        );
        handoff.request_detach();
        signal.stop_requested.store(true, Ordering::Release);
        return;
    }
    let encoder_state = pipeline.encoder();
    match encoder_state {
        CaptureEncoderState::Failed(_) | CaptureEncoderState::Stopped => {
            handoff.request_detach();
            return;
        }
        CaptureEncoderState::Starting
        | CaptureEncoderState::Stopping
        | CaptureEncoderState::Ready => {}
    }
    match pipeline.renderer_state() {
        CaptureHandoffState::Starting => {}
        CaptureHandoffState::Ready(info) if encoder_state == CaptureEncoderState::Ready => {
            match handoff.try_status() {
                CaptureHandoffState::Ready(_) => {
                    signal.capture_runtime_ready.store(true, Ordering::Release);
                }
                CaptureHandoffState::Starting => match handoff.try_set_ready(info) {
                    Ok(()) => signal.capture_runtime_ready.store(true, Ordering::Release),
                    Err(CaptureHandoffError::Busy) => {}
                    Err(CaptureHandoffError::AlreadyResolved) => {
                        if matches!(handoff.try_status(), CaptureHandoffState::Ready(_)) {
                            signal.capture_runtime_ready.store(true, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        set_failure(host_status, "capture_ready_failed", error.to_string());
                        handoff.request_detach();
                        signal.stop_requested.store(true, Ordering::Release);
                    }
                },
                CaptureHandoffState::Unsupported(_) | CaptureHandoffState::Detached => {}
            }
        }
        CaptureHandoffState::Ready(_) => {}
        CaptureHandoffState::Unsupported(reason) => match handoff.try_status() {
            CaptureHandoffState::Starting => match handoff.try_set_unsupported(reason) {
                Ok(()) | Err(CaptureHandoffError::AlreadyResolved) => {}
                Err(CaptureHandoffError::Busy) => {}
                Err(error) => {
                    set_failure(host_status, "capture_unsupported_failed", error.to_string());
                    handoff.request_detach();
                }
            },
            CaptureHandoffState::Ready(_) => {
                set_failure(
                    host_status,
                    "capture_readiness_changed",
                    "capture renderer became unsupported after readiness",
                );
                handoff.request_detach();
                signal.stop_requested.store(true, Ordering::Release);
            }
            CaptureHandoffState::Unsupported(_) | CaptureHandoffState::Detached => {}
        },
        CaptureHandoffState::Detached => handoff.request_detach(),
    }
}

#[cfg(feature = "capture")]
fn reject_pending_capture(state: &mut CoordinatorState) {
    if state
        .pending_capture
        .as_ref()
        .is_some_and(|pending| pending.request.is_none())
    {
        // The empty request slot proves that the ingress transfer already succeeded. The
        // destination retains the request while stop drains it; the coordinator must not turn
        // that accepted route back into a local terminal completion.
        state.pending_capture = None;
        return;
    }
    let Some(pending) = state.pending_capture.as_mut() else {
        return;
    };
    pending.stage_key = None;
    pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::Detached));
}

#[cfg(feature = "capture")]
fn advance_pending_capture(
    state: &mut CoordinatorState,
    runtime_status: Option<&ProjectGenerationConsumerStatus>,
    stopping: bool,
) {
    let Some(pending) = state.pending_capture.as_mut() else {
        return;
    };
    // This is the durable post-transfer marker. `try_send_pending` clears the request slot only
    // after ingress has accepted the same Box; leave the marker for `take_capture_request` to
    // reconcile if an unwind occurs before this function returns.
    let Some(request) = pending.request.as_ref() else {
        return;
    };
    if pending.completion.is_none() {
        let source = request.source();
        if stopping
            || runtime_status.is_some_and(|snapshot| snapshot.is_closed() || snapshot.is_stopped())
        {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::Detached));
        } else if request.is_cancelled() {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::Cancelled));
        } else if Instant::now() >= request.deadline() {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::TimedOut));
        } else if source.session() != request.id().session() {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::StaleIdentity));
        } else if let Some(record) = state.generations.iter().find(|record| {
            record.identity.session() == source.session()
                && record.identity.generation() == source.project_generation()
                && record.identity.source_digest() == source.source_digest()
        }) {
            pending.stage_key = Some(record.key);
        } else if runtime_status.is_some_and(|snapshot| {
            capture_source_is_stale(
                snapshot,
                source.session(),
                source.project_generation(),
                source.source_digest(),
            )
        }) {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::StaleGeneration));
        }
    }

    if let Some(completion) = pending.completion.as_ref() {
        let result = pending
            .request
            .as_mut()
            .map(|request| request.responder_mut().try_complete(completion));
        match result {
            Some(Ok(()))
            | Some(Err(CaptureResponderError::AlreadyCompleted | CaptureResponderError::Stopped)) =>
            {
                state.pending_capture = None;
            }
            Some(Err(CaptureResponderError::Busy)) => {}
            None => state.pending_capture = None,
        }
        return;
    }

    if let Some(stage_key) = pending.stage_key {
        let Some(coordinator) = state.capture_coordinator.as_ref() else {
            pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::Detached));
            return;
        };
        match coordinator.try_send_pending(&mut pending.request, stage_key) {
            Ok(()) => {
                // Keep the empty slot until the next coordinator boundary. If anything unwinds
                // between ingress acceptance and that boundary, the request is still owned by
                // ingress and `take_capture_request` can safely clear this marker.
                #[cfg(test)]
                panic_if_capture_test_requested(
                    &mut state.capture_test_panic,
                    CaptureCoordinatorPanicPoint::AfterTransfer,
                );
            }
            Err(crate::capture::CaptureIngressError::Busy) => {}
            Err(
                crate::capture::CaptureIngressError::Closed
                | crate::capture::CaptureIngressError::Poisoned,
            ) => {
                pending.completion = Some(CaptureCompletion::Failed(CaptureFailure::Detached));
            }
        }
    }
}

#[cfg(all(test, feature = "capture"))]
fn panic_if_capture_test_requested(
    requested: &mut Option<CaptureCoordinatorPanicPoint>,
    point: CaptureCoordinatorPanicPoint,
) {
    if *requested == Some(point) {
        *requested = None;
        panic!("injected capture coordinator panic at {point:?}");
    }
}

#[cfg(feature = "capture")]
fn capture_source_is_stale(
    status: &ProjectGenerationConsumerStatus,
    session: SessionId,
    generation: u64,
    digest: &str,
) -> bool {
    if session != status.session() || status.active_generation() > generation {
        return true;
    }
    if status.active_generation() == generation && status.active_source_digest() != digest {
        return true;
    }
    status.stage().is_some_and(|stage| {
        let identity = stage.identity();
        identity.session() == session
            && identity.generation() == generation
            && identity.source_digest() == digest
            && matches!(
                stage.progress(),
                ProjectConsumerStageProgress::FailedApplied
                    | ProjectConsumerStageProgress::Cancelled
                    | ProjectConsumerStageProgress::Superseded
                    | ProjectConsumerStageProgress::Closed
            )
    })
}

fn queue_prepare(
    state: &mut CoordinatorState,
    lease: daedalus_runtime::ProjectSourceLease,
    signal: &HostLifecycleSignal,
) {
    let bindings = state
        .bindings
        .as_ref()
        .cloned()
        .unwrap_or_else(|| Arc::new(BTreeMap::new()));
    queue_prepare_with_bindings(state, lease, bindings, signal);
}

fn queue_prepare_with_bindings(
    state: &mut CoordinatorState,
    lease: daedalus_runtime::ProjectSourceLease,
    bindings: Arc<BTreeMap<StableId, EntityHandle>>,
    signal: &HostLifecycleSignal,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    signal
        .prepare_handoff_pending
        .store(true, Ordering::Release);
    state.pending_prepare = Some(PrepareRequest {
        lease,
        bindings,
        cancelled,
    });
}

fn flush_prepare_request(
    state: &mut CoordinatorState,
    cpu: &CpuWorkerClient,
    signal: &HostLifecycleSignal,
) {
    let Some(request) = state.pending_prepare.take() else {
        return;
    };
    let identity = request.lease.identity().clone();
    let cancelled = Arc::clone(&request.cancelled);
    match cpu.prepare.try_send(request) {
        Ok(()) => {
            signal
                .prepare_handoff_pending
                .store(false, Ordering::Release);
            state.preparing = Some((identity, cancelled));
        }
        Err(TrySendError::Full(request)) => state.pending_prepare = Some(request),
        Err(TrySendError::Disconnected(request)) => {
            // Keep the non-clone lease alive rather than letting the coordinator become its last
            // free. The CPU owner has failed; the host remains failed/Stopping.
            std::mem::forget(request);
            signal
                .prepare_handoff_pending
                .store(false, Ordering::Release);
            state.worker_failed = true;
        }
    }
}

fn poll_cpu_results(
    state: &mut CoordinatorState,
    consumer: &ProjectGenerationConsumerPort,
    cpu: &CpuWorkerClient,
    status: &Mutex<NativeProjectHostStatus>,
    runtime_status: Option<&ProjectGenerationConsumerStatus>,
) {
    #[cfg(not(feature = "window"))]
    let _ = consumer;
    if let Some(mut delivery) = cpu.prepared.try_take()
        && let Some(result) = delivery.take_payload()
    {
        match result {
            PrepareResult::Prepared {
                identity,
                generation,
            } => {
                state.preparing = None;
                let current =
                    runtime_status.is_some_and(|runtime| is_identity_current(runtime, &identity));
                if !current {
                    state.queue_retire(generation);
                } else if state
                    .generations
                    .iter()
                    .any(|record| record.identity == identity)
                {
                    state.queue_retire(generation);
                    set_failure(
                        status,
                        "generation_identity_collision",
                        "CPU owner returned a duplicate generation identity",
                    );
                } else {
                    let initial = identity.kind() == ProjectSourceKind::Initial;
                    #[cfg(feature = "window")]
                    let key = if initial {
                        ManagedStageKey::INITIAL
                    } else {
                        match state.next_stage_key.checked_next() {
                            Some(next) => {
                                state.next_stage_key = next;
                                next
                            }
                            None => {
                                state.queue_retire(generation);
                                set_failure(
                                    status,
                                    "stage_key_exhausted",
                                    "managed stage key space exhausted",
                                );
                                return;
                            }
                        }
                    };
                    state.generations.push(CoordinatorGeneration {
                        identity,
                        cpu_generation: generation,
                        #[cfg(feature = "window")]
                        key,
                        #[cfg(feature = "window")]
                        cancellation: Arc::new(AtomicBool::new(false)),
                        initial,
                        cpu_prepared: true,
                        #[cfg(feature = "window")]
                        stage_requested: false,
                        #[cfg(feature = "window")]
                        stage_submitted: false,
                        #[cfg(feature = "window")]
                        device_ready: false,
                        #[cfg(feature = "window")]
                        report_pending: false,
                        #[cfg(feature = "window")]
                        ready_applied: false,
                    });
                }
            }
            PrepareResult::Failed {
                identity,
                phase,
                failure,
            } => {
                state.preparing = None;
                if identity.kind() == ProjectSourceKind::Initial {
                    set_failure(status, failure.code, failure.message);
                    state.worker_failed = true;
                } else {
                    #[cfg(feature = "window")]
                    if let Ok(report) =
                        ProjectConsumerStageFailure::new(phase, failure.code, failure.message)
                    {
                        state.pending_stage_report = Some(PendingStageReport {
                            identity,
                            outcome: ProjectConsumerStageOutcome::Failed(report),
                        });
                    }
                    #[cfg(not(feature = "window"))]
                    let _ = phase;
                }
            }
        }
    }

    #[cfg(feature = "window")]
    if state.pending_stage_input.is_none()
        && let Some(mut delivery) = cpu.stage_input_result.try_take()
        && let Some(result) = delivery.take_payload()
    {
        let StageInputResult {
            identity,
            generation,
            result,
        } = result;
        match result {
            Ok(input) => {
                if let Some(record) = state.generations.iter().find(|record| {
                    record.cpu_generation == generation && record.identity == identity
                }) {
                    state.pending_stage_input = Some(PendingStageInput {
                        identity,
                        generation,
                        key: record.key,
                        input: Some(input),
                        initial: record.initial,
                        delivery,
                    });
                } else {
                    drop(input);
                }
            }
            Err(failure) => {
                if identity.kind() == ProjectSourceKind::Initial {
                    set_failure(status, failure.code, failure.message);
                    state.worker_failed = true;
                } else if let Ok(report) = ProjectConsumerStageFailure::new(
                    ProjectConsumerStageFailurePhase::CpuPrepare,
                    failure.code,
                    failure.message,
                ) {
                    state.pending_stage_report = Some(PendingStageReport {
                        identity,
                        outcome: ProjectConsumerStageOutcome::Failed(report),
                    });
                }
            }
        }
    }

    #[cfg(feature = "window")]
    if let Some(mut delivery) = cpu.latest_frame_result.try_take()
        && let Some(result) = delivery.take_payload()
    {
        state.frame_request_in_flight = false;
        match result {
            LatestFrameResult::Frame {
                identity,
                publication,
                frame,
            } => {
                if let Some(record) = state
                    .generations
                    .iter()
                    .find(|record| record.identity == identity)
                {
                    state.pending_frame = Some(PendingFrameInput {
                        key: record.key,
                        publication,
                        frame,
                        delivery,
                    });
                }
            }
            LatestFrameResult::NoNewFrame => {}
            LatestFrameResult::Failed {
                identity, failure, ..
            } => {
                set_failure(
                    status,
                    failure.code,
                    format!("{}: {}", identity.generation(), failure.message),
                );
            }
        }
    }

    if state.worker_failed {
        if let Some((_, cancelled)) = state.preparing.as_ref() {
            cancelled.store(true, Ordering::Release);
        }
        let _ = consumer;
    }
}

#[cfg(feature = "window")]
fn request_stage_inputs(
    state: &mut CoordinatorState,
    cpu: &CpuWorkerClient,
    runtime_status: Option<&ProjectGenerationConsumerStatus>,
) {
    if !state.window_started || state.closing_view || state.pending_stage_input.is_some() {
        return;
    }
    if state.pending_stage_request.is_none() {
        let candidate = state.generations.iter().find(|record| {
            record.cpu_prepared
                && !record.stage_requested
                && (record.initial
                    || runtime_status.is_some_and(|runtime| {
                        candidate_stage_is_current(
                            state.active_view_generation,
                            runtime.active_generation(),
                            record.identity.expected_generation(),
                        )
                    }))
        });
        if let Some(record) = candidate {
            state.pending_stage_request = Some(StageInputRequest {
                generation: record.cpu_generation,
                identity: record.identity.clone(),
            });
        }
    }
    if let Some(request) = state.pending_stage_request.take() {
        if request.identity.kind() == ProjectSourceKind::Candidate
            && !runtime_status.is_some_and(|runtime| {
                candidate_stage_is_current(
                    state.active_view_generation,
                    runtime.active_generation(),
                    request.identity.expected_generation(),
                )
            })
        {
            state.pending_stage_request = Some(request);
            return;
        }
        let identity = request.identity.clone();
        let generation = request.generation;
        match cpu.stage_input.try_send(request) {
            Ok(()) => {
                if let Some(record) = state.generations.iter_mut().find(|record| {
                    record.cpu_generation == generation && record.identity == identity
                }) {
                    record.stage_requested = true;
                }
            }
            Err(TrySendError::Full(request)) => state.pending_stage_request = Some(request),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

#[cfg(feature = "window")]
fn flush_pending_stage(
    state: &mut CoordinatorState,
    status: &Mutex<NativeProjectHostStatus>,
    runtime_status: Option<&ProjectGenerationConsumerStatus>,
) {
    let Some(mut pending) = state.pending_stage_input.take() else {
        return;
    };
    if pending.initial {
        if let Some(reply) = state.start_reply.as_ref() {
            let Some(input) = pending.input.take() else {
                drop(pending.delivery);
                state.start_reply = None;
                set_failure(
                    status,
                    "initial_stage_input_missing",
                    "initial CPU stage result did not retain its canonical input",
                );
                return;
            };
            match reply.try_send(Ok(input)) {
                Ok(()) => {
                    state.start_reply = None;
                    drop(pending.delivery);
                }
                Err(TrySendError::Full(Ok(input))) => {
                    pending.input = Some(input);
                    state.pending_stage_input = Some(pending);
                }
                Err(TrySendError::Disconnected(Ok(input))) => {
                    drop(input);
                    drop(pending.delivery);
                    state.start_reply = None;
                    set_failure(
                        status,
                        "initial_stage_reply_disconnected",
                        "view caller stopped before initial stage handoff",
                    );
                }
                Err(TrySendError::Full(Err(error)))
                | Err(TrySendError::Disconnected(Err(error))) => {
                    drop(pending.delivery);
                    state.start_reply = None;
                    set_failure(status, "initial_stage_reply_error", error.to_string());
                }
            }
        } else {
            state.pending_stage_input = Some(pending);
        }
        return;
    }
    if !runtime_status.is_some_and(|runtime| {
        candidate_stage_is_current(
            state.active_view_generation,
            runtime.active_generation(),
            pending.identity.expected_generation(),
        )
    }) {
        state.pending_stage_input = Some(pending);
        return;
    }
    let Some(client) = state.window_client.as_ref() else {
        state.pending_stage_input = Some(pending);
        return;
    };
    let key = pending.key;
    let generation = pending.generation;
    let identity = pending.identity.clone();
    let Some(input) = pending.input.take() else {
        drop(pending.delivery);
        set_failure(
            status,
            "stage_input_missing",
            "CPU stage result did not retain its canonical input",
        );
        return;
    };
    match client.try_send(ManagedPacketWindowInput::BeginStage {
        key,
        input,
        limits: state.initial_stage_limits,
    }) {
        Ok(()) => {
            if let Some(record) = state
                .generations
                .iter_mut()
                .find(|record| record.cpu_generation == generation)
            {
                record.stage_submitted = true;
            }
            drop(pending.delivery);
        }
        Err(TrySendError::Full(ManagedPacketWindowInput::BeginStage { input, .. })) => {
            pending.input = Some(input);
            state.pending_stage_input = Some(pending);
        }
        Err(TrySendError::Disconnected(ManagedPacketWindowInput::BeginStage { input, .. })) => {
            drop(input);
            drop(pending.delivery);
            if let Some(record) = state
                .generations
                .iter_mut()
                .find(|record| record.identity == identity)
            {
                record.cancellation.store(true, Ordering::Release);
            }
        }
        Err(TrySendError::Full(ManagedPacketWindowInput::Frame { .. }))
        | Err(TrySendError::Disconnected(ManagedPacketWindowInput::Frame { .. })) => {}
    }
}

#[cfg(feature = "window")]
fn poll_window_events(
    state: &mut CoordinatorState,
    consumer: &ProjectGenerationConsumerPort,
    cpu: &CpuWorkerClient,
    status: &Mutex<NativeProjectHostStatus>,
) {
    let Some(client) = state.window_client.as_ref() else {
        return;
    };
    let mut cancelled_generations = [None; 2];
    loop {
        match client.try_stage_event() {
            ManagedWindowRead::Value(event) => match event {
                ManagedStageEvent::Ready { key, .. } => {
                    if let Some(record) = state
                        .generations
                        .iter_mut()
                        .find(|record| record.key == key)
                    {
                        record.device_ready = true;
                        if record.initial {
                            state.initial_device_ready = true;
                            if let Ok(mut host_status) = status.lock()
                                && let Some(lifecycle) = initial_ready_transition(&host_status)
                            {
                                host_status.lifecycle = lifecycle;
                            }
                        } else if !record.ready_applied {
                            state.pending_stage_report = Some(PendingStageReport {
                                identity: record.identity.clone(),
                                outcome: ProjectConsumerStageOutcome::Ready,
                            });
                        }
                    }
                }
                ManagedStageEvent::Failed { key, error } => {
                    if let Some(record) = state.generations.iter().find(|record| record.key == key)
                    {
                        if record.initial {
                            set_failure(status, "initial_device_prepare_failed", error.message());
                            state.worker_failed = true;
                        } else if let Ok(failure) = ProjectConsumerStageFailure::new(
                            ProjectConsumerStageFailurePhase::DevicePrepare,
                            "device_prepare_failed",
                            error.message(),
                        ) {
                            state.pending_stage_report = Some(PendingStageReport {
                                identity: record.identity.clone(),
                                outcome: ProjectConsumerStageOutcome::Failed(failure),
                            });
                        }
                    }
                }
                ManagedStageEvent::Cancelled { key } => {
                    if let Some(record) = state.generations.iter().find(|record| record.key == key)
                    {
                        record.cancellation.store(true, Ordering::Release);
                        if let Some(slot) =
                            cancelled_generations.iter_mut().find(|slot| slot.is_none())
                        {
                            *slot = Some(record.cpu_generation);
                        }
                    }
                }
                ManagedStageEvent::Deferred { key, .. } => {
                    if let Some(record) = state
                        .generations
                        .iter_mut()
                        .find(|record| record.key == key)
                    {
                        record.stage_submitted = false;
                        state.pending_stage_request = Some(StageInputRequest {
                            generation: record.cpu_generation,
                            identity: record.identity.clone(),
                        });
                    }
                }
            },
            ManagedWindowRead::Empty | ManagedWindowRead::Busy => break,
            ManagedWindowRead::Closed => {
                state.closing_view = true;
                consumer.request_close();
                if let Ok(mut status) = status.lock() {
                    status.view_closed = true;
                    if status.lifecycle != NativeProjectHostLifecycle::Stopping {
                        status.lifecycle = NativeProjectHostLifecycle::ViewClosed;
                    }
                }
                break;
            }
        }
    }
    for generation in cancelled_generations.into_iter().flatten() {
        state.queue_retire(generation);
    }
    let _ = (consumer, cpu);
}

#[cfg(feature = "window")]
fn retry_stage_report(
    state: &mut CoordinatorState,
    consumer: &ProjectGenerationConsumerPort,
    status: &Mutex<NativeProjectHostStatus>,
) {
    let Some(report) = state.pending_stage_report.take() else {
        return;
    };
    match consumer.try_report_stage(&report.identity, report.outcome.clone()) {
        Ok(ProjectConsumerReportProgress::Applied) => {
            if report.outcome == ProjectConsumerStageOutcome::Ready
                && let Some(record) = state.generation_mut(&report.identity)
            {
                record.ready_applied = true;
                record.report_pending = false;
            }
        }
        Ok(ProjectConsumerReportProgress::Queued) | Err(ProjectConsumerError::Busy) => {
            if let Some(record) = state.generation_mut(&report.identity) {
                record.report_pending = true;
            }
            state.pending_stage_report = Some(report);
        }
        Err(
            ProjectConsumerError::StaleIdentity
            | ProjectConsumerError::Closed
            | ProjectConsumerError::Stopped,
        ) => {
            if let Some(record) = state.generation(&report.identity) {
                state.queue_retire(record.cpu_generation);
            }
        }
        Err(error) => set_failure(status, "stage_report_failed", error.to_string()),
    }
}

#[cfg(feature = "window")]
fn request_latest_frame(
    state: &mut CoordinatorState,
    cpu: &CpuWorkerClient,
    runtime_status: Option<&ProjectGenerationConsumerStatus>,
) {
    if !state.initial_device_ready
        || state.closing_view
        || state.frame_request_in_flight
        || state.pending_frame.is_some()
        || state.pending_publications.len() >= MAX_PRESENTATION_IDENTITIES
        || state.last_frame_request.elapsed() < FRAME_REQUEST_INTERVAL
    {
        return;
    }
    let Some(runtime) = runtime_status else {
        return;
    };
    let Some(record) = state.generations.iter().find(|record| {
        record.identity.generation() == runtime.active_generation()
            && record.identity.source_digest() == runtime.active_source_digest()
            && (record.initial || record.ready_applied)
    }) else {
        return;
    };
    let request = LatestFrameRequest {
        generation: record.cpu_generation,
        identity: record.identity.clone(),
        view: state.view_override.clone(),
        cancelled: Arc::clone(&record.cancellation),
    };
    match cpu.latest_frame.try_send(request) {
        Ok(()) => {
            state.frame_request_in_flight = true;
            state.last_frame_request = Instant::now();
        }
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

#[cfg(feature = "window")]
fn flush_pending_frame(state: &mut CoordinatorState, status: &Mutex<NativeProjectHostStatus>) {
    let Some(pending) = state.pending_frame.take() else {
        return;
    };
    let Some(client) = state.window_client.as_ref() else {
        state.pending_frame = Some(pending);
        return;
    };
    if state.pending_publications.len() >= MAX_PRESENTATION_IDENTITIES {
        state.pending_frame = Some(pending);
        return;
    }
    let frame = Arc::clone(&pending.frame);
    #[cfg(feature = "capture")]
    if let Some(capture) = state.capture_coordinator.as_ref() {
        match capture.try_register_publication(pending.publication.clone(), pending.key, &frame) {
            Ok(()) => {}
            Err(
                crate::capture::CaptureRegistryError::Busy
                | crate::capture::CaptureRegistryError::Full,
            ) => {
                state.pending_frame = Some(pending);
                return;
            }
            Err(error) => {
                set_failure(
                    status,
                    "capture_frame_registration_failed",
                    error.to_string(),
                );
                state.pending_frame = Some(pending);
                return;
            }
        }
    }
    match client.try_send(ManagedPacketWindowInput::Frame {
        key: pending.key,
        frame,
    }) {
        Ok(()) => {
            if let Ok(mut status) = status.lock() {
                status.last_queued_publication = Some(pending.publication.clone());
            }
            state.pending_publications.push_back(PendingPublication {
                identity: pending.publication,
                key: pending.key,
            });
            drop(pending.delivery);
        }
        Err(TrySendError::Full(ManagedPacketWindowInput::Frame { frame, .. })) => {
            drop(frame);
            state.pending_frame = Some(pending);
        }
        Err(TrySendError::Disconnected(ManagedPacketWindowInput::Frame { frame, .. })) => {
            drop(frame);
            drop(pending);
        }
        Err(TrySendError::Full(ManagedPacketWindowInput::BeginStage { .. }))
        | Err(TrySendError::Disconnected(ManagedPacketWindowInput::BeginStage { .. })) => {}
    }
}

#[cfg(feature = "window")]
fn poll_presented(
    state: &mut CoordinatorState,
    consumer: &ProjectGenerationConsumerPort,
    status: &Mutex<NativeProjectHostStatus>,
) {
    if state.pending_ack.is_some() {
        return;
    }
    let Some(client) = state.window_client.as_ref() else {
        return;
    };
    let ManagedWindowRead::Value(presented) = client.try_presented() else {
        return;
    };
    let pending = state.pending_publications.iter().find(|entry| {
        entry.identity.publication_sequence() == presented.publication_sequence()
            && entry.identity.project_generation() == presented.project_generation()
            && entry.identity.source_digest() == presented.source_digest()
            && entry.key == presented.key()
    });
    let repeated = pending.is_none();
    let (publication, key) = if let Some(pending) = pending {
        (pending.identity.clone(), pending.key)
    } else if let Some(acknowledged) = state.last_acked_publication.as_ref() {
        if !same_presented_publication_key(&acknowledged.identity, acknowledged.key, &presented) {
            set_failure(
                status,
                "unknown_presented_publication",
                "managed window presented a publication not retained by the host",
            );
            return;
        }
        if !matches_presented_publication(&acknowledged.identity, acknowledged.key, &presented) {
            set_failure(
                status,
                "presented_identity_mismatch",
                "managed present identity did not match the actual SessionFrame",
            );
            return;
        }
        if !acknowledged_repeat_matches(acknowledged, &presented) {
            set_failure(
                status,
                "presented_sequence_not_increasing",
                "repeated publication did not advance the actual present sequence",
            );
            return;
        }
        (acknowledged.identity.clone(), acknowledged.key)
    } else {
        set_failure(
            status,
            "unknown_presented_publication",
            "managed window presented a publication not retained by the host",
        );
        return;
    };
    if !repeated && !matches_presented_publication(&publication, key, &presented) {
        set_failure(
            status,
            "presented_identity_mismatch",
            "managed present identity did not match the actual SessionFrame",
        );
        return;
    }
    let view = match ProjectPresentedView::new(
        presented.physical_width(),
        presented.physical_height(),
        *presented.view_projection(),
    ) {
        Ok(view) => view,
        Err(error) => {
            set_failure(status, "invalid_presented_view", error.to_string());
            return;
        }
    };
    let present_sequence = presented.present_sequence();
    let generation = publication.project_generation();
    let reported = match ProjectPresentedFrame::new(publication, present_sequence, view) {
        Ok(frame) => frame,
        Err(error) => {
            set_failure(status, "invalid_presented_frame", error.to_string());
            return;
        }
    };
    state.active_view_generation = Some(presented.project_generation());
    if repeated {
        if let Some(acknowledged) = state.last_acked_publication.as_mut() {
            acknowledged.present_sequence = present_sequence;
        }
        if let Ok(mut host_status) = status.lock() {
            update_last_presented(&mut host_status, summary_from_project(&reported));
        }
        return;
    }
    state.pending_ack = Some(PendingPresentedAck {
        presented: reported,
        publication_sequence: presented.publication_sequence(),
        generation,
        queued: false,
    });
    let _ = consumer;
}

#[cfg(feature = "window")]
fn candidate_stage_is_current(
    active_view_generation: Option<u64>,
    active_generation: u64,
    expected_generation: Option<u64>,
) -> bool {
    active_view_generation == Some(active_generation)
        && expected_generation == Some(active_generation)
}

#[cfg(feature = "window")]
fn same_presented_publication_key(
    identity: &ProjectPublicationIdentity,
    key: ManagedStageKey,
    presented: &ManagedPresentedView,
) -> bool {
    same_publication_key_fields(
        identity,
        key,
        presented.key(),
        presented.publication_sequence(),
        presented.project_generation(),
        presented.source_digest(),
    )
}

#[cfg(feature = "window")]
fn same_publication_key_fields(
    identity: &ProjectPublicationIdentity,
    expected_key: ManagedStageKey,
    actual_key: ManagedStageKey,
    publication_sequence: u64,
    generation: u64,
    source_digest: &str,
) -> bool {
    identity.publication_sequence() == publication_sequence
        && identity.project_generation() == generation
        && identity.source_digest() == source_digest
        && expected_key == actual_key
}

#[cfg(feature = "window")]
fn matches_presented_publication(
    identity: &ProjectPublicationIdentity,
    key: ManagedStageKey,
    presented: &ManagedPresentedView,
) -> bool {
    matches_presented_publication_fields(
        identity,
        key,
        presented.key(),
        presented.session(),
        presented.tick().get(),
        presented.publication_sequence(),
        presented.authority_snapshot_sequence(),
        presented.project_generation(),
        presented.source_digest(),
    )
}

#[cfg(feature = "window")]
// Keep the publication fields explicit so each source of identity is compared independently.
#[allow(clippy::too_many_arguments)]
fn matches_presented_publication_fields(
    identity: &ProjectPublicationIdentity,
    expected_key: ManagedStageKey,
    actual_key: ManagedStageKey,
    session: SessionId,
    tick: u64,
    publication_sequence: u64,
    authority_snapshot_sequence: Option<u64>,
    generation: u64,
    source_digest: &str,
) -> bool {
    same_publication_key_fields(
        identity,
        expected_key,
        actual_key,
        publication_sequence,
        generation,
        source_digest,
    ) && identity.session() == session
        && identity.tick().get() == tick
        && Some(identity.authority_snapshot_sequence()) == authority_snapshot_sequence
}

#[cfg(feature = "window")]
fn acknowledged_repeat_matches(
    acknowledged: &AcknowledgedPublication,
    presented: &ManagedPresentedView,
) -> bool {
    acknowledged_repeat_fields_match(
        &acknowledged.identity,
        acknowledged.key,
        presented.key(),
        presented.session(),
        presented.tick().get(),
        presented.publication_sequence(),
        presented.authority_snapshot_sequence(),
        presented.project_generation(),
        presented.source_digest(),
        acknowledged.present_sequence,
        presented.present_sequence(),
    )
}

#[cfg(feature = "window")]
// Repeated presents validate the full identity plus a strictly increasing present sequence.
#[allow(clippy::too_many_arguments)]
fn acknowledged_repeat_fields_match(
    identity: &ProjectPublicationIdentity,
    expected_key: ManagedStageKey,
    actual_key: ManagedStageKey,
    session: SessionId,
    tick: u64,
    publication_sequence: u64,
    authority_snapshot_sequence: Option<u64>,
    generation: u64,
    source_digest: &str,
    previous_present_sequence: u64,
    present_sequence: u64,
) -> bool {
    present_sequence > previous_present_sequence
        && matches_presented_publication_fields(
            identity,
            expected_key,
            actual_key,
            session,
            tick,
            publication_sequence,
            authority_snapshot_sequence,
            generation,
            source_digest,
        )
}

#[cfg(feature = "window")]
fn initial_ready_transition(
    status: &NativeProjectHostStatus,
) -> Option<NativeProjectHostLifecycle> {
    if status.view_closed
        || matches!(
            status.lifecycle,
            NativeProjectHostLifecycle::Stopping | NativeProjectHostLifecycle::Failed
        )
    {
        None
    } else if status.view_started {
        Some(NativeProjectHostLifecycle::ViewRunning)
    } else {
        Some(NativeProjectHostLifecycle::Ready)
    }
}

#[cfg(feature = "window")]
fn retry_present_ack(
    state: &mut CoordinatorState,
    consumer: &ProjectGenerationConsumerPort,
    status: &Mutex<NativeProjectHostStatus>,
) {
    let Some(ack) = state.pending_ack.as_mut() else {
        return;
    };
    match consumer.try_ack_presented(ack.presented.clone()) {
        Ok(ProjectConsumerReportProgress::Applied) => {
            complete_present_ack(state, status);
        }
        Ok(ProjectConsumerReportProgress::Queued) => ack.queued = true,
        Err(ProjectConsumerError::Busy) => {}
        Err(
            ProjectConsumerError::StaleIdentity
            | ProjectConsumerError::Closed
            | ProjectConsumerError::Stopped,
        ) => {
            let sequence = ack.publication_sequence;
            state
                .pending_publications
                .retain(|entry| entry.identity.publication_sequence() != sequence);
            state.pending_ack = None;
        }
        Err(error) => set_failure(status, "present_ack_failed", error.to_string()),
    }
}

#[cfg(feature = "window")]
fn complete_present_ack(state: &mut CoordinatorState, status: &Mutex<NativeProjectHostStatus>) {
    let Some(ack) = state.pending_ack.take() else {
        return;
    };
    if let Some(publication) = state.pending_publications.iter().find(|entry| {
        entry.identity.project_generation() == ack.generation
            && entry.identity.publication_sequence() == ack.publication_sequence
    }) {
        state.last_acked_publication = Some(AcknowledgedPublication {
            identity: publication.identity.clone(),
            key: publication.key,
            present_sequence: ack.presented.present_sequence(),
        });
    }
    if let Ok(mut status) = status.lock() {
        update_last_presented(&mut status, summary_from_project(&ack.presented));
    }
    state.pending_publications.retain(|entry| {
        entry.identity.project_generation() != ack.generation
            || entry.identity.publication_sequence() > ack.publication_sequence
    });
    let mut retire = [None; 2];
    for record in &state.generations {
        if record.identity.generation() < ack.generation
            && let Some(slot) = retire.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(record.cpu_generation);
        }
    }
    for generation in retire.into_iter().flatten() {
        state.queue_retire(generation);
    }
}

#[cfg(feature = "window")]
fn update_generation_progress(
    state: &mut CoordinatorState,
    status: &ProjectGenerationConsumerStatus,
) {
    if let Some(stage) = status.stage()
        && let Some(record) = state.generation_mut(stage.identity())
    {
        match stage.progress() {
            ProjectConsumerStageProgress::ReadyApplied => {
                record.ready_applied = true;
                record.report_pending = false;
            }
            ProjectConsumerStageProgress::FailedApplied
            | ProjectConsumerStageProgress::Cancelled
            | ProjectConsumerStageProgress::Superseded
            | ProjectConsumerStageProgress::Closed => {
                record.cancellation.store(true, Ordering::Release);
            }
            _ => {}
        }
    }
    let mut retire = [None; 2];
    for record in &state.generations {
        if !record.initial
            && record.identity.generation() != status.active_generation()
            && status.stage().is_some_and(|stage| {
                stage.identity() != &record.identity
                    && matches!(
                        stage.progress(),
                        ProjectConsumerStageProgress::Cancelled
                            | ProjectConsumerStageProgress::Superseded
                            | ProjectConsumerStageProgress::FailedApplied
                    )
            })
        {
            record.cancellation.store(true, Ordering::Release);
            if let Some(slot) = retire.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(record.cpu_generation);
            }
        }
    }
    for generation in retire.into_iter().flatten() {
        state.queue_retire(generation);
    }
}

#[cfg(feature = "window")]
fn retire_obsolete_generations(
    state: &mut CoordinatorState,
    status: &ProjectGenerationConsumerStatus,
) {
    let mut retire = [None; 2];
    for record in &state.generations {
        if record.identity.generation() < status.active_generation()
            && status.last_applied_present().is_some_and(|presented| {
                presented.publication().project_generation() == status.active_generation()
            })
            && let Some(slot) = retire.iter_mut().find(|slot| slot.is_none())
        {
            *slot = Some(record.cpu_generation);
        }
    }
    for generation in retire.into_iter().flatten() {
        state.queue_retire(generation);
    }
}

fn flush_retire_requests(state: &mut CoordinatorState, cpu: &CpuWorkerClient) {
    let Some(generation) = state.pending_retire.pop_front() else {
        return;
    };
    match cpu.retire.try_send(RetireRequest { generation }) {
        Ok(()) => {
            state
                .generations
                .retain(|record| record.cpu_generation != generation);
        }
        Err(TrySendError::Full(_)) => state.pending_retire.push_front(generation),
        Err(TrySendError::Disconnected(_)) => state.pending_retire.push_front(generation),
    }
}

fn is_identity_current(
    status: &ProjectGenerationConsumerStatus,
    identity: &ProjectSourceIdentity,
) -> bool {
    if identity.kind() == ProjectSourceKind::Initial {
        status.active_generation() == identity.generation()
            && status.active_source_digest() == identity.source_digest()
    } else {
        status
            .stage()
            .is_some_and(|stage| stage.identity() == identity)
            && !status.stage().is_some_and(|stage| {
                matches!(
                    stage.progress(),
                    ProjectConsumerStageProgress::Cancelled
                        | ProjectConsumerStageProgress::Superseded
                        | ProjectConsumerStageProgress::Closed
                        | ProjectConsumerStageProgress::FailedApplied
                )
            })
    }
}

fn synchronize_runtime_status(
    host: &Mutex<NativeProjectHostStatus>,
    consumer: &ProjectGenerationConsumerStatus,
) {
    if let Ok(mut host) = host.lock() {
        host.active_generation = consumer.active_generation();
        host.active_source_digest = consumer.active_source_digest().to_owned();
    }
}

fn set_lifecycle(status: &Mutex<NativeProjectHostStatus>, lifecycle: NativeProjectHostLifecycle) {
    if let Ok(mut status) = status.lock() {
        status.lifecycle = lifecycle;
    }
}

fn set_failure(status: &Mutex<NativeProjectHostStatus>, code: &str, message: impl Into<String>) {
    if let Ok(mut status) = status.lock() {
        status.failure = Some(NativeProjectHostFailure::new(code, message));
        if status.lifecycle != NativeProjectHostLifecycle::Stopping {
            status.lifecycle = NativeProjectHostLifecycle::Failed;
        }
    }
}

#[cfg(feature = "window")]
fn summary_from_managed(presented: &ManagedPresentedSummary) -> NativePresentedSummary {
    NativePresentedSummary {
        session: presented.session(),
        tick: presented.tick().get(),
        publication_sequence: presented.publication_sequence(),
        authority_snapshot_sequence: presented.authority_snapshot_sequence(),
        project_generation: presented.project_generation(),
        source_digest: presented.source_digest().to_owned(),
        present_sequence: presented.present_sequence(),
        physical_width: presented.physical_width(),
        physical_height: presented.physical_height(),
    }
}

#[cfg(feature = "window")]
fn update_last_presented(status: &mut NativeProjectHostStatus, presented: NativePresentedSummary) {
    let should_update = match status.last_presented.as_ref() {
        Some(current) if current.session() == presented.session() => {
            presented.present_sequence() > current.present_sequence()
        }
        Some(_) => false,
        None => true,
    };
    if should_update {
        status.last_presented = Some(presented);
    }
}

#[cfg(feature = "window")]
fn summary_from_project(presented: &ProjectPresentedFrame) -> NativePresentedSummary {
    let publication = presented.publication();
    NativePresentedSummary {
        session: publication.session(),
        tick: publication.tick().get(),
        publication_sequence: publication.publication_sequence(),
        authority_snapshot_sequence: Some(publication.authority_snapshot_sequence()),
        project_generation: publication.project_generation(),
        source_digest: publication.source_digest().to_owned(),
        present_sequence: presented.present_sequence(),
        physical_width: presented.view().physical_width(),
        physical_height: presented.view().physical_height(),
    }
}

fn collect_bindings(
    client: &SessionControlClient,
    expected_session: SessionId,
    stopped: &AtomicBool,
) -> Result<BTreeMap<StableId, EntityHandle>, CpuFailure> {
    let mut offset = 0usize;
    let mut expected_digest: Option<String> = None;
    let mut expected_count: Option<usize> = None;
    let mut bindings = BTreeMap::new();
    loop {
        if stopped.load(Ordering::Acquire) {
            return Err(CpuFailure {
                code: "stopped",
                message: "host stopped while collecting authored bindings".to_owned(),
            });
        }
        let result = submit_wait(
            client,
            SessionOperation::ProjectInfo {
                offset,
                limit: Some(BINDING_PAGE_SIZE),
            },
            stopped,
        )
        .map_err(|message| CpuFailure {
            code: "project_info_failed",
            message,
        })?;
        let SessionRequestResult::ProjectInfo(page) = result else {
            return Err(CpuFailure {
                code: "project_info_result_mismatch",
                message: "project info request returned another result type".to_owned(),
            });
        };
        if page.session != expected_session
            || expected_digest
                .as_ref()
                .is_some_and(|digest| digest != &page.source_digest)
            || expected_count.is_some_and(|count| count != page.entity_count)
        {
            return Err(CpuFailure {
                code: "project_info_identity_changed",
                message: "project info pages changed session or source identity".to_owned(),
            });
        }
        expected_digest.get_or_insert_with(|| page.source_digest.clone());
        expected_count.get_or_insert(page.entity_count);
        for binding in &page.entities {
            if bindings
                .insert(binding.id.clone(), binding.handle)
                .is_some()
            {
                return Err(CpuFailure {
                    code: "duplicate_authored_id",
                    message: "project info returned a duplicate authored entity ID".to_owned(),
                });
            }
        }
        if bindings.len() > MAX_AUTHORED_ENTITIES {
            return Err(CpuFailure {
                code: "authored_entity_limit",
                message: "project authored entity count exceeds the host limit".to_owned(),
            });
        }
        match page.next_offset {
            Some(next) if next > offset && next <= MAX_AUTHORED_ENTITIES => offset = next,
            Some(_) => {
                return Err(CpuFailure {
                    code: "invalid_project_info_offset",
                    message: "project info returned a nonadvancing or out-of-range page offset"
                        .to_owned(),
                });
            }
            None => {
                if expected_count != Some(bindings.len()) {
                    return Err(CpuFailure {
                        code: "project_info_count_mismatch",
                        message: "project info pages did not cover the declared entity count"
                            .to_owned(),
                    });
                }
                return Ok(bindings);
            }
        }
    }
}

fn submit_wait(
    client: &SessionControlClient,
    operation: SessionOperation,
    stopped: &AtomicBool,
) -> Result<SessionRequestResult, String> {
    let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
    let mut ticket = loop {
        if stopped.load(Ordering::Acquire) {
            return Err("host stop requested during project info admission".to_owned());
        }
        match client.try_submit(operation.clone()) {
            Ok(ticket) => break ticket,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                if Instant::now() < deadline =>
            {
                thread::sleep(COORDINATOR_POLL);
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    loop {
        if stopped.load(Ordering::Acquire) {
            return Err("host stop requested while waiting for project info".to_owned());
        }
        match ticket.try_state() {
            Ok(progress) => {
                if progress.phase() == RequestPhase::Terminal {
                    let result = progress
                        .terminal()
                        .ok_or_else(|| "terminal project info ticket has no result".to_owned())?
                        .clone();
                    ticket.acknowledge().map_err(|error| error.to_string())?;
                    return result.map_err(|error: SessionRequestError| error.to_string());
                }
            }
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for project info".to_owned());
        }
        thread::sleep(COORDINATOR_POLL);
    }
}

fn bounded(mut value: String, maximum: usize) -> String {
    if value.len() > maximum {
        let mut end = maximum;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

#[cfg(feature = "capture")]
fn map_capture_status_error(error: CaptureStatusReadError) -> NativeCaptureStatusError {
    match error {
        CaptureStatusReadError::Busy => NativeCaptureStatusError::Busy,
        CaptureStatusReadError::Poisoned => NativeCaptureStatusError::Poisoned,
    }
}

#[cfg(feature = "capture")]
fn handoff_detach(handoff: &Option<CaptureHandoffPort>) {
    if let Some(handoff) = handoff {
        handoff.request_detach();
    }
}

#[cfg(feature = "capture")]
fn capture_fail(status: &Mutex<NativeProjectHostStatus>, code: &str, message: String) {
    set_failure(status, code, message);
}

#[cfg(feature = "capture")]
fn project_capture_status(
    status: &CapturePipelineStatus,
    stopping_requested: bool,
    runtime_ready: bool,
    capture_drained: bool,
) -> NativeProjectCaptureStatus {
    let encoder_state = match status.encoder() {
        CaptureEncoderState::Starting => NativeCaptureEncoderState::Starting,
        CaptureEncoderState::Ready => NativeCaptureEncoderState::Ready,
        CaptureEncoderState::Stopping => NativeCaptureEncoderState::Stopping,
        CaptureEncoderState::Stopped => NativeCaptureEncoderState::Stopped,
        CaptureEncoderState::Failed(failure) => {
            NativeCaptureEncoderState::Failed(capture_failure(failure))
        }
    };
    let failure = match status.encoder() {
        CaptureEncoderState::Failed(failure) => Some(capture_failure(failure)),
        _ => None,
    };
    let lifecycle = if let Some(failure) = failure.as_ref() {
        NativeProjectCaptureLifecycle::Failed(failure.clone())
    } else if stopping_requested {
        if capture_drained {
            NativeProjectCaptureLifecycle::Stopped
        } else {
            NativeProjectCaptureLifecycle::Stopping
        }
    } else {
        match status.renderer_state() {
            CaptureHandoffState::Starting => NativeProjectCaptureLifecycle::Starting,
            CaptureHandoffState::Ready(info)
                if runtime_ready && matches!(status.encoder(), CaptureEncoderState::Ready) =>
            {
                NativeProjectCaptureLifecycle::Ready(info)
            }
            CaptureHandoffState::Ready(_) => NativeProjectCaptureLifecycle::Starting,
            CaptureHandoffState::Unsupported(reason) => {
                NativeProjectCaptureLifecycle::Unsupported(reason)
            }
            CaptureHandoffState::Detached => NativeProjectCaptureLifecycle::Detached,
        }
    };
    NativeProjectCaptureStatus {
        session: status.session(),
        lifecycle,
        encoder_state,
        pending_jobs: status.pending_jobs(),
        retained_png_bytes: status.retained_png_bytes(),
        cleanup_pending: status.cleanup_pending() != 0,
        failure,
    }
}

#[cfg(feature = "capture")]
fn capture_failure(failure: CapturePipelineFailure) -> NativeCaptureFailure {
    match failure {
        CapturePipelineFailure::SpoolUnavailable => NativeCaptureFailure::new(
            "spool_unavailable",
            "the private capture spool could not be initialized",
        ),
        CapturePipelineFailure::EncoderPanicked => {
            NativeCaptureFailure::new("encoder_panicked", "the capture encoder worker panicked")
        }
        CapturePipelineFailure::CleanupFailed => {
            NativeCaptureFailure::new("cleanup_failed", "capture artifact cleanup failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::atomic::AtomicU64};

    use daedalus_renderer::PacketCpuLimits;

    static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);

    #[test]
    #[cfg(feature = "capture")]
    fn live_capture_is_opt_in_in_host_options() {
        let defaults = NativeProjectHostOptions::for_project("unused-test-path");
        assert!(!defaults.live_capture);
        assert!(defaults.with_live_capture().live_capture);
    }

    #[test]
    #[cfg(feature = "capture")]
    fn coordinator_panics_preserve_capture_before_and_after_transfer() {
        let project = TempProject::new();
        let (service, consumer, handoff) =
            NativeProjectActivationService::start_with_render_consumer_and_capture(
                NativeProjectPreparationOptions::new(project.path()),
                SessionControlConfig::default(),
            )
            .expect("capture test starts one real actor");
        handoff
            .try_set_ready(
                CaptureReadyInfo::new(
                    daedalus_runtime::CaptureSurfaceFormat::Rgba8UnormSrgb,
                    8,
                    8,
                    64,
                )
                .expect("small capture bounds are valid"),
            )
            .expect("test publishes one actual readiness state");
        let client = service
            .session()
            .attach()
            .expect("test attaches to the capture actor");

        let mut before_transfer_ticket = submit_capture_ticket(&client);
        let session = before_transfer_ticket.id().session();
        let mut pipeline = crate::capture::start_capture_pipeline(session, 1)
            .expect("test starts one bounded encoder pipeline");
        let capture_coordinator = pipeline.coordinator_port();
        let mut cpu_capture = pipeline
            .take_cpu_worker_port()
            .expect("one CPU capture owner is installed");
        let mut state = CoordinatorState::new();
        state.capture_handoff = Some(handoff);
        state.capture_coordinator = Some(capture_coordinator.clone());
        let signal = HostLifecycleSignal::new();
        let status = Mutex::new(NativeProjectHostStatus {
            lifecycle: NativeProjectHostLifecycle::Initializing,
            session,
            active_generation: 1,
            active_source_digest: String::new(),
            cpu: PacketCpuStatus::default(),
            view_started: false,
            view_closed: false,
            last_queued_publication: None,
            last_presented: None,
            failure: None,
        });

        state.capture_test_panic = Some(CaptureCoordinatorPanicPoint::AfterTake);
        let before_transfer_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_for_durable_capture_take(&mut state, &signal, &status);
        }));
        assert!(before_transfer_panic.is_err());
        let before_transfer_id = state
            .pending_capture
            .as_ref()
            .and_then(|pending| pending.request.as_ref())
            .expect("accepted request remains in coordinator state after unwind")
            .id();
        let before_transfer_deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        while state.pending_capture.is_some() {
            advance_pending_capture(&mut state, None, true);
            if state.pending_capture.is_some() {
                assert!(
                    Instant::now() < before_transfer_deadline,
                    "pre-transfer capture completion stayed busy"
                );
                thread::sleep(COORDINATOR_POLL);
            }
        }
        assert!(state.pending_capture.is_none());
        assert_capture_ticket_failed(&mut before_transfer_ticket, CaptureFailure::Detached);

        let mut after_transfer_ticket = submit_capture_ticket(&client);
        wait_for_durable_capture_take(&mut state, &signal, &status);
        let after_transfer_id = state
            .pending_capture
            .as_ref()
            .and_then(|pending| pending.request.as_ref())
            .expect("second actor request is durably retained")
            .id();
        state
            .pending_capture
            .as_mut()
            .expect("pending request remains installed")
            .stage_key = Some(ManagedStageKey::INITIAL);
        state.capture_test_panic = Some(CaptureCoordinatorPanicPoint::AfterTransfer);
        let after_transfer_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            advance_pending_capture(&mut state, None, false);
        }));
        assert!(after_transfer_panic.is_err());
        assert!(
            state
                .pending_capture
                .as_ref()
                .is_some_and(|pending| pending.request.is_none())
        );
        reject_pending_capture(&mut state);
        let mut routed = cpu_capture
            .try_take_request()
            .expect("CPU ingress remains readable")
            .expect("transferred request remains in the ingress slot");
        assert_eq!(routed.stage_key, ManagedStageKey::INITIAL);
        assert_eq!(routed.request.id(), after_transfer_id);
        assert_ne!(after_transfer_id, before_transfer_id);
        complete_capture_responder(
            routed.request.responder_mut(),
            &CaptureCompletion::Failed(CaptureFailure::Cancelled),
        );
        drop(routed);
        assert_capture_ticket_failed(&mut after_transfer_ticket, CaptureFailure::Cancelled);

        capture_coordinator.close_transfer_fence();
        assert!(capture_coordinator.is_drained());
        pipeline.request_stop();
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match cpu_capture.try_finish_encoder() {
                CaptureEncoderShutdown::Stopped | CaptureEncoderShutdown::Failed(_) => break,
                CaptureEncoderShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "test encoder did not finish");
                    thread::sleep(COORDINATOR_POLL);
                }
            }
        }
        drop(cpu_capture);
        drop(state);
        drop(client);
        consumer.request_close();
        drop(consumer);
        service.request_stop();
        stop_activation_service(service);
    }

    #[test]
    #[cfg(feature = "capture")]
    fn default_host_keeps_capture_admission_unavailable() {
        let project = TempProject::new();
        let mut host =
            NativeProjectHost::start(NativeProjectHostOptions::for_project(project.path()))
                .expect("default host starts without an encoder pipeline");
        let client = host
            .attach_control()
            .expect("host attaches its actor client");

        assert_eq!(
            submit_capture_until_pre_ticket_error(&client),
            SessionSubmitError::CaptureUnavailable
        );
        assert_eq!(
            capture_status_until_readable(&host.control()).unwrap_err(),
            NativeCaptureStatusError::Unavailable
        );

        drop(client);
        host.request_stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match host
                .try_shutdown()
                .expect("default host shuts down cleanly")
            {
                NativeProjectHostShutdown::Stopped => break,
                NativeProjectHostShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "default host did not stop");
                    thread::sleep(COORDINATOR_POLL);
                }
            }
        }
    }

    #[test]
    #[cfg(feature = "capture")]
    fn capture_enabled_host_without_a_view_stops_without_claiming_readiness() {
        let project = TempProject::new();
        let mut host = NativeProjectHost::start(
            NativeProjectHostOptions::for_project(project.path()).with_live_capture(),
        )
        .expect("capture-enabled host starts its encoder without a device");
        let client = host
            .attach_control()
            .expect("host attaches its actor client");

        assert_eq!(
            submit_capture_until_pre_ticket_error(&client),
            SessionSubmitError::CaptureStarting
        );
        assert!(matches!(
            capture_status_until_readable(&host.control()).expect("capture status is readable"),
            NativeProjectCaptureStatus {
                lifecycle: NativeProjectCaptureLifecycle::Starting,
                ..
            }
        ));

        drop(client);
        host.request_stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match host
                .try_shutdown()
                .expect("capture host shuts down cleanly")
            {
                NativeProjectHostShutdown::Stopped => break,
                NativeProjectHostShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "capture host did not stop");
                    thread::sleep(COORDINATOR_POLL);
                }
            }
        }
        let capture = capture_status_until_readable(&host.control())
            .expect("capture status survives host shutdown");
        assert_eq!(capture.lifecycle(), &NativeProjectCaptureLifecycle::Stopped);
        assert_eq!(capture.pending_jobs(), 0);
        assert_eq!(capture.retained_png_bytes(), 0);
        assert!(!capture.cleanup_pending());
        assert_eq!(capture.encoder_state(), &NativeCaptureEncoderState::Stopped);
    }

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "daedalus-native-host-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("daedalus.project.json"),
                br#"{"schema_version":1,"name":"native-host-test","scene":"scene.json"}"#,
            )
            .unwrap();
            fs::write(
                path.join("scene.json"),
                br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#,
            )
            .unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "window")]
    fn wait_terminal_result(
        client: &SessionControlClient,
        operation: SessionOperation,
    ) -> Result<SessionRequestResult, SessionRequestError> {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        let mut ticket = loop {
            match client.try_submit(operation.clone()) {
                Ok(ticket) => break ticket,
                Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                    if Instant::now() < deadline =>
                {
                    thread::sleep(COORDINATOR_POLL)
                }
                Err(error) => panic!("request admission failed: {error}"),
            }
        };
        let progress = ticket
            .wait_for(
                daedalus_runtime::WaitStage::Terminal,
                CONTROL_REQUEST_TIMEOUT,
            )
            .expect("request reaches a terminal result");
        assert_eq!(progress.phase(), RequestPhase::Terminal);
        let result = progress
            .terminal()
            .expect("terminal request has a result")
            .clone();
        ticket
            .acknowledge()
            .expect("terminal result credit is acknowledged");
        result
    }

    #[cfg(feature = "window")]
    fn stop_activation_service(mut service: NativeProjectActivationService) {
        service.request_stop();
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match service
                .try_shutdown()
                .expect("runtime service remains healthy")
            {
                NativeProjectActivationShutdown::Stopped => return,
                NativeProjectActivationShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "runtime service did not stop");
                    thread::sleep(COORDINATOR_POLL);
                }
            }
        }
    }

    #[cfg(feature = "capture")]
    fn submit_capture_ticket(client: &SessionControlClient) -> daedalus_runtime::RequestTicket {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match client.try_submit(SessionOperation::CaptureFrame {
                at_least_tick: None,
                timeout: Some(CONTROL_REQUEST_TIMEOUT),
            }) {
                Ok(ticket) => return ticket,
                Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                    if Instant::now() < deadline =>
                {
                    thread::sleep(COORDINATOR_POLL);
                }
                Err(error) => panic!("capture request admission failed: {error}"),
            }
        }
    }

    #[cfg(feature = "capture")]
    fn submit_capture_until_pre_ticket_error(client: &SessionControlClient) -> SessionSubmitError {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match client.try_submit(SessionOperation::CaptureFrame {
                at_least_tick: None,
                timeout: None,
            }) {
                Ok(_) => panic!("capture request was unexpectedly admitted"),
                Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                    if Instant::now() < deadline =>
                {
                    thread::sleep(COORDINATOR_POLL);
                }
                Err(error) => return error,
            }
        }
    }

    #[cfg(feature = "capture")]
    fn capture_status_until_readable(
        control: &NativeProjectHostControl,
    ) -> Result<NativeProjectCaptureStatus, NativeCaptureStatusError> {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match control.try_capture_status() {
                Err(NativeCaptureStatusError::Busy) if Instant::now() < deadline => {
                    thread::sleep(COORDINATOR_POLL);
                }
                result => return result,
            }
        }
    }

    #[cfg(feature = "capture")]
    fn complete_capture_responder(
        responder: &mut daedalus_runtime::CaptureResponder,
        completion: &CaptureCompletion,
    ) {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            match responder.try_complete(completion) {
                Ok(()) => return,
                Err(CaptureResponderError::Busy) if Instant::now() < deadline => {
                    thread::sleep(COORDINATOR_POLL);
                }
                Err(error) => panic!("capture responder completion failed: {error}"),
            }
        }
    }

    #[cfg(feature = "capture")]
    fn wait_for_durable_capture_take(
        state: &mut CoordinatorState,
        signal: &HostLifecycleSignal,
        status: &Mutex<NativeProjectHostStatus>,
    ) {
        let deadline = Instant::now() + CONTROL_REQUEST_TIMEOUT;
        loop {
            take_capture_request(state, signal, status);
            if state.pending_capture.is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "actor capture request was not handed off"
            );
            thread::sleep(COORDINATOR_POLL);
        }
    }

    #[cfg(feature = "capture")]
    fn assert_capture_ticket_failed(
        ticket: &mut daedalus_runtime::RequestTicket,
        expected: CaptureFailure,
    ) {
        let progress = ticket
            .wait_for(
                daedalus_runtime::WaitStage::Terminal,
                CONTROL_REQUEST_TIMEOUT,
            )
            .expect("capture ticket reaches a terminal result");
        assert_eq!(progress.phase(), RequestPhase::Terminal);
        assert!(matches!(
            progress.terminal(),
            Some(Err(SessionRequestError::CaptureFailed(failure))) if *failure == expected
        ));
        ticket
            .acknowledge()
            .expect("capture terminal result is acknowledged");
    }

    #[test]
    #[cfg(feature = "window")]
    fn out_of_band_window_controls_signal_even_if_forwarding_lock_is_busy() {
        let signal = Arc::new(HostLifecycleSignal::new());
        let status = Arc::new(Mutex::new(NativeProjectHostStatus {
            lifecycle: NativeProjectHostLifecycle::Ready,
            session: SessionId::from_raw(1),
            active_generation: 1,
            active_source_digest: String::new(),
            cpu: PacketCpuStatus::default(),
            view_started: false,
            view_closed: false,
            last_queued_publication: None,
            last_presented: None,
            failure: None,
        }));
        let window = Arc::new(Mutex::new(None));
        let control = NativeProjectHostControl {
            signal: Arc::clone(&signal),
            status,
            window: Arc::clone(&window),
            #[cfg(feature = "capture")]
            capture_control: None,
        };
        let held = window.lock().expect("test owns forwarding mutex");

        control.request_stop();
        control.request_view_close();

        assert!(signal.stop_requested.load(Ordering::Acquire));
        assert!(signal.view_close_requested.load(Ordering::Acquire));
        drop(held);
    }

    #[test]
    #[cfg(feature = "window")]
    fn older_present_ack_does_not_replace_newer_actual_window_present() {
        let session = SessionId::from_raw(7);
        let mut status = NativeProjectHostStatus {
            lifecycle: NativeProjectHostLifecycle::ViewClosed,
            session,
            active_generation: 1,
            active_source_digest: String::new(),
            cpu: PacketCpuStatus::default(),
            view_started: true,
            view_closed: true,
            last_queued_publication: None,
            last_presented: None,
            failure: None,
        };
        let summary = |present_sequence| NativePresentedSummary {
            session,
            tick: 0,
            publication_sequence: present_sequence,
            authority_snapshot_sequence: Some(present_sequence),
            project_generation: 1,
            source_digest: "sha256:host-status-test".to_owned(),
            present_sequence,
            physical_width: 640,
            physical_height: 480,
        };

        update_last_presented(&mut status, summary(3));
        update_last_presented(&mut status, summary(2));
        assert_eq!(status.last_presented().unwrap().present_sequence(), 3);
        update_last_presented(&mut status, summary(4));
        assert_eq!(status.last_presented().unwrap().present_sequence(), 4);
    }

    #[test]
    #[cfg(feature = "window")]
    fn acknowledged_publication_accepts_redraw_and_rejects_replaced_identity() {
        let project = TempProject::new();
        let (service, consumer) = NativeProjectActivationService::start_with_render_consumer(
            NativeProjectPreparationOptions::new(project.path()),
            SessionControlConfig::default(),
        )
        .expect("fixture starts the real runtime project and actor");
        let client = service
            .session()
            .attach()
            .expect("test attaches to the real project authority");

        let deadline = Instant::now() + Duration::from_secs(5);
        let first_frame = loop {
            if let Ok(Some(frame)) = client.try_latest_frame() {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "initial publication was not available"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let first = ProjectPublicationIdentity::from_frame(&first_frame)
            .expect("the retained identity comes from a real SessionFrame");
        let key = ManagedStageKey::INITIAL;
        let acknowledged = AcknowledgedPublication {
            identity: first.clone(),
            key,
            present_sequence: 10,
        };
        let repeat_matches = |acknowledged: &AcknowledgedPublication,
                              actual_key: ManagedStageKey,
                              session: SessionId,
                              tick: u64,
                              publication_sequence: u64,
                              authority_sequence: Option<u64>,
                              generation: u64,
                              source_digest: &str,
                              present_sequence: u64| {
            acknowledged_repeat_fields_match(
                &acknowledged.identity,
                acknowledged.key,
                actual_key,
                session,
                tick,
                publication_sequence,
                authority_sequence,
                generation,
                source_digest,
                acknowledged.present_sequence,
                present_sequence,
            )
        };
        let first_repeat = |actual_key,
                            session,
                            tick,
                            publication_sequence,
                            authority,
                            generation,
                            digest,
                            present| {
            repeat_matches(
                &acknowledged,
                actual_key,
                session,
                tick,
                publication_sequence,
                authority,
                generation,
                digest,
                present,
            )
        };

        assert!(first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            10,
        ));
        assert!(!first_repeat(
            key.checked_next().expect("test stage key advances"),
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            SessionId::from_raw(first.session().get().wrapping_add(1)),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get().wrapping_add(1),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence().wrapping_add(1)),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence().wrapping_add(1),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation().wrapping_add(1),
            first.source_digest(),
            11,
        ));
        assert!(!first_repeat(
            key,
            first.session(),
            first.tick().get(),
            first.publication_sequence(),
            Some(first.authority_snapshot_sequence()),
            first.project_generation(),
            "sha256:wrong-source",
            11,
        ));

        let step = wait_terminal_result(&client, SessionOperation::Step { count: 1 })
            .expect("manual authority step succeeds");
        assert!(matches!(step, SessionRequestResult::Step(_)));
        let deadline = Instant::now() + Duration::from_secs(5);
        let second_frame = loop {
            if let Ok(Some(frame)) = client.try_latest_frame()
                && frame.sequence() > first.publication_sequence()
            {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "new publication was not available"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let second = ProjectPublicationIdentity::from_frame(&second_frame)
            .expect("the replacement identity comes from a real SessionFrame");
        assert_eq!(second.session(), first.session());
        assert_eq!(second.project_generation(), first.project_generation());
        assert!(second.publication_sequence() > first.publication_sequence());

        let acknowledged_after_step = AcknowledgedPublication {
            identity: second.clone(),
            key,
            present_sequence: 20,
        };
        let second_repeat =
            |session, tick, publication_sequence, authority, generation, digest, present| {
                repeat_matches(
                    &acknowledged_after_step,
                    key,
                    session,
                    tick,
                    publication_sequence,
                    authority,
                    generation,
                    digest,
                    present,
                )
            };
        assert!(second_repeat(
            second.session(),
            second.tick().get(),
            second.publication_sequence(),
            Some(second.authority_snapshot_sequence()),
            second.project_generation(),
            second.source_digest(),
            21,
        ));
        assert!(
            !second_repeat(
                first.session(),
                first.tick().get(),
                first.publication_sequence(),
                Some(first.authority_snapshot_sequence()),
                first.project_generation(),
                first.source_digest(),
                21,
            ),
            "replacing the bounded trusted identity rejects a redraw of the old frame"
        );

        drop(consumer);
        drop(client);
        stop_activation_service(service);
    }

    #[test]
    #[cfg(feature = "window")]
    fn initial_device_ready_does_not_overwrite_view_or_terminal_lifecycle() {
        let status = |lifecycle, view_started, view_closed| NativeProjectHostStatus {
            lifecycle,
            session: SessionId::from_raw(1),
            active_generation: 1,
            active_source_digest: String::new(),
            cpu: PacketCpuStatus::default(),
            view_started,
            view_closed,
            last_queued_publication: None,
            last_presented: None,
            failure: None,
        };

        assert_eq!(
            initial_ready_transition(&status(
                NativeProjectHostLifecycle::InitialPreparing,
                false,
                false,
            )),
            Some(NativeProjectHostLifecycle::Ready)
        );
        assert_eq!(
            initial_ready_transition(&status(
                NativeProjectHostLifecycle::ViewRunning,
                true,
                false,
            )),
            Some(NativeProjectHostLifecycle::ViewRunning)
        );
        assert_eq!(
            initial_ready_transition(&status(
                NativeProjectHostLifecycle::CpuPrepared,
                true,
                false,
            )),
            Some(NativeProjectHostLifecycle::ViewRunning),
            "an initial Ready event after a slow CPU prepare must restore ViewRunning"
        );
        assert_eq!(
            initial_ready_transition(&status(NativeProjectHostLifecycle::ViewClosed, true, true,)),
            None
        );
        assert_eq!(
            initial_ready_transition(&status(NativeProjectHostLifecycle::Stopping, false, false,)),
            None
        );
        assert_eq!(
            initial_ready_transition(&status(NativeProjectHostLifecycle::Failed, false, false,)),
            None
        );
    }

    #[test]
    #[cfg(feature = "window")]
    fn old_acknowledged_frame_stays_valid_until_new_generation_is_presented() {
        let project = TempProject::new();
        let (service, consumer) = NativeProjectActivationService::start_with_render_consumer(
            NativeProjectPreparationOptions::new(project.path()),
            SessionControlConfig::default(),
        )
        .expect("fixture starts the real runtime project and actor");
        let client = service
            .session()
            .attach()
            .expect("test attaches to the real project authority");

        let deadline = Instant::now() + Duration::from_secs(5);
        let first_frame = loop {
            if let Ok(Some(frame)) = client.try_latest_frame() {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "initial publication was not available"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let first = ProjectPublicationIdentity::from_frame(&first_frame)
            .expect("initial identity comes from the real SessionFrame");
        let first_key = ManagedStageKey::INITIAL;
        let initial_lease = loop {
            match consumer.try_take_initial() {
                Ok(Some(lease)) => break lease,
                Ok(None) | Err(ProjectConsumerError::Busy) => {}
                Err(error) => panic!("initial source lease failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "initial source lease was not available"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let initial_view = ProjectPresentedView::new(
            640,
            480,
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
        )
        .expect("test supplies finite presentation metadata");
        let initial_present = ProjectPresentedFrame::new(first.clone(), 1, initial_view)
            .expect("test uses a nonzero present sequence");
        loop {
            match consumer.try_ack_presented(initial_present.clone()) {
                Ok(_) => break,
                Err(ProjectConsumerError::Busy) => {
                    assert!(Instant::now() < deadline, "initial present stayed busy");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("initial present report failed: {error}"),
            }
        }
        loop {
            match consumer.try_status() {
                Ok(status)
                    if status.last_applied_present().is_some_and(|present| {
                        present.publication().publication_sequence() == first.publication_sequence()
                    }) =>
                {
                    break;
                }
                Ok(_) | Err(ProjectConsumerError::Busy) => {}
                Err(error) => panic!("initial present status failed: {error}"),
            }
            assert!(Instant::now() < deadline, "initial present was not applied");
            thread::sleep(Duration::from_millis(2));
        }
        drop(initial_lease);

        fs::write(
            project.path().join("scene.json"),
            br#"{"schema_version":1,"revision":1,"entities":[],"assets":[]}"#,
        )
        .expect("candidate scene edit succeeds");
        let prepared = wait_terminal_result(
            &client,
            SessionOperation::AssetPreparation(
                daedalus_runtime::AssetPreparationOperation::Prepare { command_id: None },
            ),
        )
        .expect("candidate preparation command succeeds");
        let candidate = match prepared {
            SessionRequestResult::AssetPreparation(
                daedalus_runtime::AssetPreparationResult::Command(command),
            ) => match command.outcome() {
                daedalus_runtime::AssetPreparationCommandOutcome::PrepareAccepted { candidate } => {
                    *candidate
                }
                other => panic!("unexpected preparation result: {other:?}"),
            },
            other => panic!("unexpected preparation response: {other:?}"),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = wait_terminal_result(
                &client,
                SessionOperation::AssetPreparation(
                    daedalus_runtime::AssetPreparationOperation::Status,
                ),
            )
            .expect("preparation status remains readable");
            if let SessionRequestResult::AssetPreparation(
                daedalus_runtime::AssetPreparationResult::Status(status),
            ) = status
                && status.state() == daedalus_runtime::PreparationState::CpuPrepared
                && status
                    .ready_candidate()
                    .is_some_and(|ready| ready.id() == candidate)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "candidate did not become CPU prepared"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let staged = wait_terminal_result(
            &client,
            SessionOperation::AssetActivation(daedalus_runtime::AssetActivationOperation::Stage {
                command_id: None,
                candidate,
                expected_active_generation: 1,
            }),
        )
        .expect("candidate stage command succeeds");
        let activation = match staged {
            SessionRequestResult::AssetActivation(
                daedalus_runtime::AssetActivationRequestResult::Command(command),
            ) => match command.outcome() {
                daedalus_runtime::AssetActivationCommandOutcome::StageAccepted {
                    activation,
                    candidate: accepted,
                } if *accepted == candidate => *activation,
                other => panic!("unexpected stage result: {other:?}"),
            },
            other => panic!("unexpected stage response: {other:?}"),
        };
        let candidate_lease = loop {
            match consumer.try_take_candidate() {
                Ok(Some(lease)) => break lease,
                Ok(None) | Err(ProjectConsumerError::Busy) => {}
                Err(error) => panic!("candidate source lease failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "candidate source lease was not available"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let candidate_identity = candidate_lease.identity().clone();
        assert_eq!(candidate_identity.activation(), Some(activation));
        loop {
            match consumer.try_report_stage(&candidate_identity, ProjectConsumerStageOutcome::Ready)
            {
                Ok(_) => break,
                Err(ProjectConsumerError::Busy) => {
                    assert!(
                        Instant::now() < deadline,
                        "candidate Ready report stayed busy"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("candidate Ready report failed: {error}"),
            }
        }
        loop {
            match consumer.try_status() {
                Ok(status)
                    if status.stage().is_some_and(|stage| {
                        stage.progress()
                            == daedalus_runtime::ProjectConsumerStageProgress::ReadyApplied
                    }) =>
                {
                    break;
                }
                Ok(_) | Err(ProjectConsumerError::Busy) => {}
                Err(error) => panic!("candidate stage status failed: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "candidate Ready report was not applied"
            );
            thread::sleep(Duration::from_millis(2));
        }
        let committed = wait_terminal_result(
            &client,
            SessionOperation::AssetActivation(daedalus_runtime::AssetActivationOperation::Commit {
                command_id: None,
                activation,
                expected_active_generation: 1,
            }),
        )
        .expect("candidate commit succeeds");
        assert!(matches!(
            committed,
            SessionRequestResult::AssetActivation(
                daedalus_runtime::AssetActivationRequestResult::Command(command)
            ) if matches!(
                command.outcome(),
                daedalus_runtime::AssetActivationCommandOutcome::CommitApplied {
                    generation: 2,
                    ..
                }
            )
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        let second_frame = loop {
            if let Ok(Some(frame)) = client.try_latest_frame()
                && frame.project_generation() == 2
            {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "committed generation frame was not published"
            );
            thread::sleep(Duration::from_millis(2));
        };
        let second = ProjectPublicationIdentity::from_frame(&second_frame)
            .expect("new generation identity comes from its real SessionFrame");
        assert_eq!(second.session(), first.session());
        assert_eq!(second.project_generation(), 2);
        assert_ne!(second.source_digest(), first.source_digest());

        let old_acknowledged = AcknowledgedPublication {
            identity: first.clone(),
            key: first_key,
            present_sequence: 1,
        };
        assert!(
            acknowledged_repeat_fields_match(
                &old_acknowledged.identity,
                old_acknowledged.key,
                first_key,
                first.session(),
                first.tick().get(),
                first.publication_sequence(),
                Some(first.authority_snapshot_sequence()),
                first.project_generation(),
                first.source_digest(),
                old_acknowledged.present_sequence,
                2,
            ),
            "the acknowledged old view remains valid between Commit and the new first present"
        );
        let new_key = first_key
            .checked_next()
            .expect("candidate stage key advances");
        let new_acknowledged = AcknowledgedPublication {
            identity: second.clone(),
            key: new_key,
            present_sequence: 3,
        };
        assert!(acknowledged_repeat_fields_match(
            &new_acknowledged.identity,
            new_acknowledged.key,
            new_key,
            second.session(),
            second.tick().get(),
            second.publication_sequence(),
            Some(second.authority_snapshot_sequence()),
            second.project_generation(),
            second.source_digest(),
            new_acknowledged.present_sequence,
            4,
        ));
        assert!(
            !acknowledged_repeat_fields_match(
                &new_acknowledged.identity,
                new_acknowledged.key,
                first_key,
                first.session(),
                first.tick().get(),
                first.publication_sequence(),
                Some(first.authority_snapshot_sequence()),
                first.project_generation(),
                first.source_digest(),
                new_acknowledged.present_sequence,
                4,
            ),
            "the first new-generation ACK replaces the trusted old publication"
        );

        drop(candidate_lease);
        drop(consumer);
        drop(client);
        stop_activation_service(service);
    }

    #[test]
    #[cfg(feature = "window")]
    fn candidate_stage_waits_for_actual_presentation_of_current_generation() {
        assert!(!candidate_stage_is_current(None, 1, Some(1)));
        assert!(!candidate_stage_is_current(Some(1), 2, Some(2)));
        assert!(candidate_stage_is_current(Some(1), 1, Some(1)));
        assert!(!candidate_stage_is_current(Some(1), 1, Some(2)));

        // After a commit advances authority, the previously presented generation is no longer
        // sufficient to submit the next candidate's GPU stage. The newly active generation must
        // first produce an actual managed presentation.
        assert!(!candidate_stage_is_current(Some(1), 2, Some(2)));
        assert!(candidate_stage_is_current(Some(2), 2, Some(2)));
    }

    #[test]
    fn dropped_host_after_real_cpu_prepare_failure_exits_its_worker() {
        let project = TempProject::new();
        // This remains a valid lowered limit configuration, but the project's real prepared
        // metadata cannot fit. It drives the normal runtime lease → CPU prepare → coordinator
        // failure path without injecting a fake worker result.
        let cpu_limits = PacketCpuLimits {
            prepared_metadata_bytes_per_generation: 1,
            prepared_metadata_bytes_total: 2,
            ..PacketCpuLimits::default()
        };
        let mut host = NativeProjectHost::start(
            NativeProjectHostOptions::for_project(project.path()).with_cpu_limits(cpu_limits),
        )
        .expect("valid project starts the host before CPU preparation");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match host.try_status().expect("host status remains readable") {
                NativeProjectHostStatusRead::Value(status) if status.failure().is_some() => {
                    assert_eq!(status.lifecycle(), NativeProjectHostLifecycle::Failed);
                    assert_eq!(
                        status.failure().unwrap().code(),
                        "cpu_budget_exceeded",
                        "failure must come from the actual generation admission: {}",
                        status.failure().unwrap().message()
                    );
                    break;
                }
                NativeProjectHostStatusRead::Value(_) | NativeProjectHostStatusRead::Busy => {
                    assert!(
                        Instant::now() < deadline,
                        "CPU prepare failure was not reported"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }

        // Keep the join handle while dropping the public facade. Drop only requests stop; the
        // worker remains responsible for releasing its retained source and result-lane state.
        let service = Arc::clone(&host.service);
        let coordinator = host
            .coordinator
            .take()
            .expect("valid host owns its coordinator");
        let cpu = host.cpu.take().expect("host owns its actual CPU worker");
        let crate::cpu_worker::CpuWorker { join, .. } = cpu;
        drop(host);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !join.is_finished() || !coordinator.is_finished() {
            assert!(
                Instant::now() < deadline,
                "host workers remained alive after host drop and OOB stop"
            );
            thread::sleep(Duration::from_millis(2));
        }
        join.join().expect("CPU worker exits without panic");
        coordinator
            .join()
            .expect("host coordinator exits without panic");

        let mut service = match Arc::try_unwrap(service) {
            Ok(service) => service,
            Err(_) => panic!("host workers released their shared activation-service owner"),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service
                .try_shutdown()
                .expect("runtime services remain healthy after host drop")
            {
                NativeProjectActivationShutdown::Stopped => break,
                NativeProjectActivationShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "runtime service did not stop");
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
}
