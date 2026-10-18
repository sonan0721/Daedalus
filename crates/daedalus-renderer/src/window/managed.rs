//! Managed packet-window transport and bounded metadata.

use std::sync::{
    Arc, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};

use daedalus_core::{SessionId, Tick};

use crate::packet::valid_source_digest;
use crate::{
    CpuFrameHandle, PacketAssetResourceStats, PacketResourceStats, PacketStageFailure,
    PacketStageId, PacketStageInput, PacketStageLimits, PacketStageState, PacketStageStatus,
    RenderView, WindowGpuOwner, WindowGpuOwnerPhase,
};

use super::capture::ManagedCaptureShared;
use super::{WindowExitReason, WindowReport};

const MANAGED_INPUT_CAPACITY: usize = 1;
const MAX_MANAGED_FAILURE_MESSAGE_BYTES: usize = 2 * 1024;

/// A renderer-window-local identity. It is not a runtime generation or GPU stage ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManagedStageKey(u64);

impl ManagedStageKey {
    /// Identity assigned to the initial resource-only stage.
    pub const INITIAL: Self = Self(1);

    /// Returns the next local identity without wrapping.
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).filter(|value| *value != 0).map(Self)
    }

    /// Returns the opaque integer value for host-side identity mapping.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// One owned message sent to a managed window. Cancellation uses the OOB control instead.
pub enum ManagedPacketWindowInput {
    BeginStage {
        key: ManagedStageKey,
        input: PacketStageInput,
        limits: PacketStageLimits,
    },
    Frame {
        key: ManagedStageKey,
        frame: Arc<CpuFrameHandle>,
    },
}

impl std::fmt::Debug for ManagedPacketWindowInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginStage { key, input, limits } => formatter
                .debug_struct("BeginStage")
                .field("key", key)
                .field("session", &input.session())
                .field("project_generation", &input.project_generation())
                .field("source_digest", &input.source_digest())
                .field("limits", limits)
                .finish(),
            Self::Frame { key, frame } => formatter
                .debug_struct("Frame")
                .field("key", key)
                .field("session", &frame.session())
                .field("project_generation", &frame.project_generation())
                .field("publication_sequence", &frame.publication_sequence())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWindowExitReason {
    ViewClosed,
    EscapePressed,
    HostStop,
    TimeLimit,
    FrameLimit,
    EventLoopReturned,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWindowLifecycle {
    Starting,
    Running,
    Stopping,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedWindowErrorCode {
    InvalidOptions,
    EventLoop,
    Renderer,
    Packet,
    Stage,
    Identity,
    CounterExhausted,
    InternalPanic,
}

/// A finite diagnostic. Messages are truncated at a UTF-8 boundary before publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWindowError {
    code: ManagedWindowErrorCode,
    message: String,
}

impl ManagedWindowError {
    pub fn code(&self) -> ManagedWindowErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub(super) fn new(code: ManagedWindowErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: bounded_message(message.into(), MAX_MANAGED_FAILURE_MESSAGE_BYTES),
        }
    }

    pub(super) fn internal_panic() -> Self {
        Self::new(
            ManagedWindowErrorCode::InternalPanic,
            "managed packet window panicked; GPU owner was preserved",
        )
    }
}

/// Why a consumed begin-stage input must be sent again from the canonical CPU generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedStageDeferredReason {
    RendererBusy,
    EventSlotOccupied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedStageEvent {
    Ready {
        key: ManagedStageKey,
        stage_id: PacketStageId,
        status: PacketStageStatus,
    },
    Failed {
        key: ManagedStageKey,
        error: ManagedWindowError,
    },
    Cancelled {
        key: ManagedStageKey,
    },
    Deferred {
        key: ManagedStageKey,
        reason: ManagedStageDeferredReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedGenerationIdentity {
    key: ManagedStageKey,
    session: SessionId,
    project_generation: u64,
    source_digest: String,
}

impl ManagedGenerationIdentity {
    pub fn key(&self) -> ManagedStageKey {
        self.key
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

    pub(super) fn new(
        key: ManagedStageKey,
        session: SessionId,
        project_generation: u64,
        source_digest: &str,
    ) -> Self {
        Self {
            key,
            session,
            project_generation,
            source_digest: source_digest.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ManagedPresentedView {
    key: ManagedStageKey,
    session: SessionId,
    tick: Tick,
    publication_sequence: u64,
    authority_snapshot_sequence: Option<u64>,
    project_generation: u64,
    source_digest: String,
    present_sequence: u64,
    physical_width: u32,
    physical_height: u32,
    view: RenderView,
    view_projection: [f32; 16],
}

impl ManagedPresentedView {
    pub fn key(&self) -> ManagedStageKey {
        self.key
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn tick(&self) -> Tick {
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

    pub fn physical_width(&self) -> u32 {
        self.physical_width
    }

    pub fn physical_height(&self) -> u32 {
        self.physical_height
    }

    pub fn view(&self) -> &RenderView {
        &self.view
    }

    /// Column-major view-projection matrix used for the actual presented frame.
    pub fn view_projection(&self) -> &[f32; 16] {
        &self.view_projection
    }

    pub(super) fn new(
        key: ManagedStageKey,
        packet: &crate::RenderPacket,
        present_sequence: u64,
        physical_width: u32,
        physical_height: u32,
        view: &RenderView,
    ) -> Result<Self, ManagedWindowError> {
        if physical_width == 0 || physical_height == 0 {
            return Err(ManagedWindowError::new(
                ManagedWindowErrorCode::Identity,
                "presented view dimensions must be nonzero",
            ));
        }
        if !valid_source_digest(packet.source_digest()) {
            return Err(ManagedWindowError::new(
                ManagedWindowErrorCode::Identity,
                "presented source digest is not canonical",
            ));
        }
        let aspect = physical_width as f32 / physical_height.max(1) as f32;
        let view_projection = view
            .view_projection(aspect)
            .map_err(|error| {
                ManagedWindowError::new(ManagedWindowErrorCode::Identity, error.to_string())
            })?
            .to_cols_array();
        Ok(Self {
            key,
            session: packet.session(),
            tick: packet.tick(),
            publication_sequence: packet.sequence(),
            authority_snapshot_sequence: packet.authority_snapshot_sequence(),
            project_generation: packet.project_generation(),
            source_digest: packet.source_digest().to_owned(),
            present_sequence,
            physical_width,
            physical_height,
            view: view.clone(),
            view_projection,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedPresentedSummary {
    key: ManagedStageKey,
    session: SessionId,
    tick: Tick,
    publication_sequence: u64,
    authority_snapshot_sequence: Option<u64>,
    project_generation: u64,
    source_digest: String,
    present_sequence: u64,
    physical_width: u32,
    physical_height: u32,
}

impl ManagedPresentedSummary {
    pub fn key(&self) -> ManagedStageKey {
        self.key
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn tick(&self) -> Tick {
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

    pub fn physical_width(&self) -> u32 {
        self.physical_width
    }

    pub fn physical_height(&self) -> u32 {
        self.physical_height
    }

    pub(super) fn from_view(view: &ManagedPresentedView) -> Self {
        Self {
            key: view.key,
            session: view.session,
            tick: view.tick,
            publication_sequence: view.publication_sequence,
            authority_snapshot_sequence: view.authority_snapshot_sequence,
            project_generation: view.project_generation,
            source_digest: view.source_digest.clone(),
            present_sequence: view.present_sequence,
            physical_width: view.physical_width,
            physical_height: view.physical_height,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWindowStatus {
    pub(super) lifecycle: ManagedWindowLifecycle,
    pub(super) exit_reason: Option<ManagedWindowExitReason>,
    pub(super) owner_phase: WindowGpuOwnerPhase,
    pub(super) active: Option<ManagedGenerationIdentity>,
    pub(super) staging: Option<PacketStageStatusSummary>,
    pub(super) last_presented: Option<ManagedPresentedSummary>,
    pub(super) rendered_frames: u64,
    pub(super) bootstrap_frames: u64,
    pub(super) physical_width: u32,
    pub(super) physical_height: u32,
}

impl ManagedWindowStatus {
    pub fn lifecycle(&self) -> ManagedWindowLifecycle {
        self.lifecycle
    }

    pub fn exit_reason(&self) -> Option<ManagedWindowExitReason> {
        self.exit_reason
    }

    pub fn owner_phase(&self) -> WindowGpuOwnerPhase {
        self.owner_phase
    }

    pub fn active(&self) -> Option<&ManagedGenerationIdentity> {
        self.active.as_ref()
    }

    pub fn staging(&self) -> Option<&PacketStageStatusSummary> {
        self.staging.as_ref()
    }

    pub fn last_presented(&self) -> Option<&ManagedPresentedSummary> {
        self.last_presented.as_ref()
    }

    pub fn rendered_frames(&self) -> u64 {
        self.rendered_frames
    }

    pub fn bootstrap_frames(&self) -> u64 {
        self.bootstrap_frames
    }

    pub fn physical_size(&self) -> (u32, u32) {
        (self.physical_width, self.physical_height)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PacketStageStatusSummary {
    key: ManagedStageKey,
    stage_id: PacketStageId,
    state: PacketStageState,
    session: SessionId,
    project_generation: u64,
    source_digest: String,
    total_upload_bytes: usize,
    uploaded_bytes: usize,
    create_calls_completed: usize,
    initialization_complete: bool,
}

impl PacketStageStatusSummary {
    pub fn key(&self) -> ManagedStageKey {
        self.key
    }

    pub fn stage_id(&self) -> PacketStageId {
        self.stage_id
    }

    pub fn state(&self) -> PacketStageState {
        self.state
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

    pub fn total_upload_bytes(&self) -> usize {
        self.total_upload_bytes
    }

    pub fn uploaded_bytes(&self) -> usize {
        self.uploaded_bytes
    }

    pub fn create_calls_completed(&self) -> usize {
        self.create_calls_completed
    }

    pub fn initialization_complete(&self) -> bool {
        self.initialization_complete
    }

    pub(super) fn from_status(key: ManagedStageKey, status: &PacketStageStatus) -> Self {
        Self {
            key,
            stage_id: status.id(),
            state: status.state(),
            session: status.session(),
            project_generation: status.project_generation(),
            source_digest: status.source_digest().to_owned(),
            total_upload_bytes: status.total_upload_bytes(),
            uploaded_bytes: status.uploaded_bytes(),
            create_calls_completed: status.create_calls_completed(),
            initialization_complete: status.initialization_complete(),
        }
    }
}

/// Managed window options preserve every legacy [`WindowOptions`](super::WindowOptions) field.
#[derive(Clone, Debug)]
pub struct ManagedPacketWindowOptions {
    pub window: super::WindowOptions,
    pub packet_limits: crate::PacketRendererLimits,
    pub asset_limits: crate::PacketAssetLimits,
    pub initial_stage_limits: PacketStageLimits,
}

impl Default for ManagedPacketWindowOptions {
    fn default() -> Self {
        Self {
            window: super::WindowOptions {
                initial_loading: true,
                ..super::WindowOptions::default()
            },
            packet_limits: crate::PacketRendererLimits::default(),
            asset_limits: crate::PacketAssetLimits::default(),
            initial_stage_limits: PacketStageLimits::default(),
        }
    }
}

/// Successful managed-window metadata. It contains no packet/frame/source roots.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedWindowOutcome {
    pub(super) report: WindowReport,
    pub(super) last_presented: Option<ManagedPresentedView>,
    pub(super) final_view: RenderView,
    pub(super) resource_stats: PacketResourceStats,
    pub(super) asset_resource_stats: PacketAssetResourceStats,
}

impl ManagedWindowOutcome {
    pub fn report(&self) -> &WindowReport {
        &self.report
    }

    pub fn last_presented(&self) -> Option<&ManagedPresentedView> {
        self.last_presented.as_ref()
    }

    pub fn final_view(&self) -> &RenderView {
        &self.final_view
    }

    pub fn resource_stats(&self) -> &PacketResourceStats {
        &self.resource_stats
    }

    pub fn asset_resource_stats(&self) -> &PacketAssetResourceStats {
        &self.asset_resource_stats
    }
}

/// Always-owned return from managed window startup, failure, or unwind.
pub struct ManagedWindowRun {
    pub outcome: Result<ManagedWindowOutcome, ManagedWindowError>,
    pub status: ManagedWindowStatus,
    pub gpu_owner: WindowGpuOwner,
}

impl std::fmt::Debug for ManagedWindowRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedWindowRun")
            .field("outcome", &self.outcome)
            .field("status", &self.status)
            .field("gpu_owner", &self.gpu_owner.phase())
            .finish()
    }
}

/// Nonblocking result of reading a managed window metadata slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedWindowRead<T> {
    Value(T),
    Empty,
    Busy,
    Closed,
}

/// Non-clone payload writer and metadata reader. Full sends return the exact input.
pub struct ManagedPacketWindowClient {
    sender: SyncSender<ManagedPacketWindowInput>,
    shared: Arc<ManagedWindowShared>,
}

impl ManagedPacketWindowClient {
    pub fn try_send(
        &self,
        input: ManagedPacketWindowInput,
    ) -> Result<(), TrySendError<ManagedPacketWindowInput>> {
        if self.shared.is_closed() {
            return Err(TrySendError::Disconnected(input));
        }
        self.sender.try_send(input)
    }

    pub fn control(&self) -> ManagedPacketWindowControl {
        ManagedPacketWindowControl {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn try_stage_event(&self) -> ManagedWindowRead<ManagedStageEvent> {
        self.shared.try_take_stage_event()
    }

    pub fn try_presented(&self) -> ManagedWindowRead<ManagedPresentedView> {
        self.shared.try_take_presented()
    }

    pub fn try_status(&self) -> ManagedWindowRead<ManagedWindowStatus> {
        self.shared.try_read_status()
    }
}

/// Cloneable out-of-band controls and compact status access; it holds no GPU or CPU resource roots.
#[derive(Clone)]
pub struct ManagedPacketWindowControl {
    shared: Arc<ManagedWindowShared>,
}

impl ManagedPacketWindowControl {
    pub fn request_view_close(&self) {
        self.shared.view_close.store(true, Ordering::Release);
    }

    pub fn request_host_stop(&self) {
        self.shared.host_stop.store(true, Ordering::Release);
    }

    pub fn request_cancel_through(&self, key: ManagedStageKey) {
        self.shared
            .cancel_through
            .fetch_max(key.get(), Ordering::AcqRel);
    }

    pub fn try_status(&self) -> ManagedWindowRead<ManagedWindowStatus> {
        self.shared.try_read_status()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }
}

/// Move-only half consumed by the event loop.
pub struct ManagedPacketWindowServer {
    pub(super) receiver: Receiver<ManagedPacketWindowInput>,
    pub(super) shared: Arc<ManagedWindowShared>,
    pub(super) capture: Option<Arc<ManagedCaptureShared>>,
}

/// Creates the fixed-capacity payload channel plus independent finite metadata/control slots.
pub fn managed_packet_window_channel() -> (ManagedPacketWindowClient, ManagedPacketWindowServer) {
    let (sender, receiver) = mpsc::sync_channel(MANAGED_INPUT_CAPACITY);
    let shared = Arc::new(ManagedWindowShared::new());
    (
        ManagedPacketWindowClient {
            sender,
            shared: Arc::clone(&shared),
        },
        ManagedPacketWindowServer {
            receiver,
            shared,
            capture: None,
        },
    )
}

pub(super) struct ManagedWindowShared {
    view_close: AtomicBool,
    host_stop: AtomicBool,
    cancel_through: AtomicU64,
    closed: AtomicBool,
    stage_event: Mutex<Option<ManagedStageEvent>>,
    presented: Mutex<Option<ManagedPresentedView>>,
    status: Mutex<ManagedWindowStatus>,
}

impl ManagedWindowShared {
    fn new() -> Self {
        Self {
            view_close: AtomicBool::new(false),
            host_stop: AtomicBool::new(false),
            cancel_through: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            stage_event: Mutex::new(None),
            presented: Mutex::new(None),
            status: Mutex::new(ManagedWindowStatus {
                lifecycle: ManagedWindowLifecycle::Starting,
                exit_reason: None,
                owner_phase: WindowGpuOwnerPhase::NoDevice,
                active: None,
                staging: None,
                last_presented: None,
                rendered_frames: 0,
                bootstrap_frames: 0,
                physical_width: 0,
                physical_height: 0,
            }),
        }
    }

    pub(super) fn view_close_requested(&self) -> bool {
        self.view_close.load(Ordering::Acquire)
    }

    pub(super) fn host_stop_requested(&self) -> bool {
        self.host_stop.load(Ordering::Acquire)
    }

    pub(super) fn cancel_watermark(&self) -> u64 {
        self.cancel_through.load(Ordering::Acquire)
    }

    pub(super) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(super) fn stage_event_is_occupied_or_busy(&self) -> bool {
        match self.stage_event.try_lock() {
            Ok(slot) => slot.is_some(),
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Poisoned(error)) => error.into_inner().is_some(),
        }
    }

    pub(super) fn try_publish_stage_event(&self, event: ManagedStageEvent) -> bool {
        match self.stage_event.try_lock() {
            Ok(mut slot) => {
                if slot.is_some() {
                    return false;
                }
                *slot = Some(event);
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Poisoned(error)) => {
                let mut slot = error.into_inner();
                if slot.is_some() {
                    return false;
                }
                *slot = Some(event);
                true
            }
        }
    }

    fn try_take_stage_event(&self) -> ManagedWindowRead<ManagedStageEvent> {
        let mut slot = match self.stage_event.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return ManagedWindowRead::Busy,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if let Some(event) = slot.take() {
            ManagedWindowRead::Value(event)
        } else if self.is_closed() {
            ManagedWindowRead::Closed
        } else {
            ManagedWindowRead::Empty
        }
    }

    pub(super) fn try_reserve_presented(
        &self,
        project_generation: u64,
    ) -> Result<ManagedPresentedGuard<'_>, ManagedPresentedReservationError> {
        let slot = match self.presented.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => {
                return Err(ManagedPresentedReservationError::Busy);
            }
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if slot
            .as_ref()
            .is_some_and(|presented| presented.project_generation != project_generation)
        {
            return Err(ManagedPresentedReservationError::PreviousGenerationUnapplied);
        }
        Ok(ManagedPresentedGuard { slot })
    }

    fn try_take_presented(&self) -> ManagedWindowRead<ManagedPresentedView> {
        let mut slot = match self.presented.try_lock() {
            Ok(slot) => slot,
            Err(TryLockError::WouldBlock) => return ManagedWindowRead::Busy,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if let Some(presented) = slot.take() {
            ManagedWindowRead::Value(presented)
        } else if self.is_closed() {
            ManagedWindowRead::Closed
        } else {
            ManagedWindowRead::Empty
        }
    }

    pub(super) fn try_publish_status(&self, next: ManagedWindowStatus) -> bool {
        match self.status.try_lock() {
            Ok(mut status) => {
                *status = next;
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Poisoned(error)) => {
                *error.into_inner() = next;
                true
            }
        }
    }

    fn try_read_status(&self) -> ManagedWindowRead<ManagedWindowStatus> {
        match self.status.try_lock() {
            Ok(status) => ManagedWindowRead::Value(status.clone()),
            Err(TryLockError::WouldBlock) => ManagedWindowRead::Busy,
            Err(TryLockError::Poisoned(error)) => {
                ManagedWindowRead::Value(error.into_inner().clone())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ManagedPresentedReservationError {
    Busy,
    PreviousGenerationUnapplied,
}

pub(super) struct ManagedPresentedGuard<'a> {
    slot: MutexGuard<'a, Option<ManagedPresentedView>>,
}

impl ManagedPresentedGuard<'_> {
    pub(super) fn publish(&mut self, presented: ManagedPresentedView) {
        *self.slot = Some(presented);
    }
}

pub(super) fn map_packet_stage_failure(error: &PacketStageFailure) -> ManagedWindowError {
    ManagedWindowError::new(
        ManagedWindowErrorCode::Stage,
        format!("{}: {}", error.code(), error.message()),
    )
}

pub(super) fn bounded_message(mut message: String, limit: usize) -> String {
    if message.len() > limit {
        const SUFFIX: &str = "… [truncated]";
        let mut boundary = limit.saturating_sub(SUFFIX.len());
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
        message.push_str(SUFFIX);
    }
    message
}

pub(super) fn bounded_string(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    const SUFFIX: &str = "… [truncated]";
    let mut boundary = limit.saturating_sub(SUFFIX.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut result = String::with_capacity(limit);
    result.push_str(&value[..boundary]);
    result.push_str(SUFFIX);
    result
}

pub(super) fn bounded_renderer_info(info: &crate::RendererInfo) -> crate::RendererInfo {
    const MAX_FIELD_BYTES: usize = 256;
    crate::RendererInfo {
        adapter_name: bounded_string(&info.adapter_name, MAX_FIELD_BYTES),
        backend: bounded_string(&info.backend, MAX_FIELD_BYTES),
        device_type: bounded_string(&info.device_type, MAX_FIELD_BYTES),
    }
}

pub(super) fn legacy_exit_reason(reason: Option<ManagedWindowExitReason>) -> WindowExitReason {
    match reason.unwrap_or(ManagedWindowExitReason::EventLoopReturned) {
        ManagedWindowExitReason::ViewClosed => WindowExitReason::CloseRequested,
        ManagedWindowExitReason::EscapePressed => WindowExitReason::EscapePressed,
        ManagedWindowExitReason::HostStop => WindowExitReason::CloseRequested,
        ManagedWindowExitReason::TimeLimit => WindowExitReason::TimeLimit,
        ManagedWindowExitReason::FrameLimit => WindowExitReason::FrameLimit,
        ManagedWindowExitReason::EventLoopReturned => WindowExitReason::EventLoopReturned,
        ManagedWindowExitReason::Failed => WindowExitReason::EventLoopReturned,
    }
}

pub(super) fn map_exit_reason(reason: WindowExitReason) -> ManagedWindowExitReason {
    match reason {
        WindowExitReason::CloseRequested => ManagedWindowExitReason::ViewClosed,
        WindowExitReason::EscapePressed => ManagedWindowExitReason::EscapePressed,
        WindowExitReason::TimeLimit => ManagedWindowExitReason::TimeLimit,
        WindowExitReason::FrameLimit => ManagedWindowExitReason::FrameLimit,
        WindowExitReason::EventLoopReturned => ManagedWindowExitReason::EventLoopReturned,
    }
}

#[cfg(test)]
mod tests {
    use daedalus_core::{SessionId, Tick};

    use crate::{Camera, DirectionalLight, RenderPacket};

    use super::{
        ManagedGenerationIdentity, ManagedPresentedView, ManagedStageEvent, ManagedStageKey,
        ManagedWindowError, ManagedWindowErrorCode, ManagedWindowLifecycle, ManagedWindowRead,
        managed_packet_window_channel,
    };

    fn packet(generation: u64, sequence: u64) -> RenderPacket {
        RenderPacket::new(
            SessionId::from_raw(77),
            Tick(sequence),
            sequence,
            generation,
            format!("{:064x}", generation),
            Camera::default(),
            DirectionalLight::default(),
            [0.1; 3],
            [0.02, 0.03, 0.04, 1.0],
            Vec::new(),
        )
    }

    fn presented(key: ManagedStageKey, generation: u64, sequence: u64) -> ManagedPresentedView {
        let packet = packet(generation, sequence);
        ManagedPresentedView::new(key, &packet, sequence, 640, 360, packet.view())
            .expect("finite packet view and nonzero dimensions")
    }

    #[test]
    fn stage_events_are_a_single_nonoverwriting_terminal_slot() {
        let (client, server) = managed_packet_window_channel();
        let key = ManagedStageKey::INITIAL;
        assert!(
            server
                .shared
                .try_publish_stage_event(ManagedStageEvent::Cancelled { key })
        );
        assert!(
            !server
                .shared
                .try_publish_stage_event(ManagedStageEvent::Failed {
                    key,
                    error: ManagedWindowError::new(ManagedWindowErrorCode::Stage, "later result"),
                })
        );
        assert_eq!(
            client.try_stage_event(),
            ManagedWindowRead::Value(ManagedStageEvent::Cancelled { key })
        );
        assert_eq!(client.try_stage_event(), ManagedWindowRead::Empty);
    }

    #[test]
    fn presented_slot_holds_old_generation_until_host_observes_it() {
        let (client, server) = managed_packet_window_channel();
        let initial = presented(ManagedStageKey::INITIAL, 1, 1);
        let mut guard = server
            .shared
            .try_reserve_presented(1)
            .expect("initial generation can reserve an empty slot");
        guard.publish(initial.clone());
        drop(guard);

        let candidate_key = ManagedStageKey::INITIAL.checked_next().unwrap();
        assert!(matches!(
            server.shared.try_reserve_presented(2),
            Err(super::ManagedPresentedReservationError::PreviousGenerationUnapplied)
        ));
        assert_eq!(
            client.try_presented(),
            ManagedWindowRead::Value(initial.clone())
        );
        let next = presented(candidate_key, 2, 2);
        let mut guard = server
            .shared
            .try_reserve_presented(2)
            .expect("observing the previous generation releases its slot");
        guard.publish(next.clone());
        drop(guard);
        assert_eq!(client.try_presented(), ManagedWindowRead::Value(next));
    }

    #[test]
    fn same_generation_presentation_coalesces_but_status_reads_never_wait() {
        let (client, server) = managed_packet_window_channel();
        let first = presented(ManagedStageKey::INITIAL, 1, 1);
        let second = presented(ManagedStageKey::INITIAL, 1, 2);
        let mut guard = server.shared.try_reserve_presented(1).unwrap();
        guard.publish(first);
        guard.publish(second.clone());
        drop(guard);
        assert_eq!(client.try_presented(), ManagedWindowRead::Value(second));

        let status_guard = server.shared.status.lock().unwrap();
        assert_eq!(client.try_status(), ManagedWindowRead::Busy);
        drop(status_guard);
        assert!(
            matches!(client.try_status(), ManagedWindowRead::Value(status)
            if status.lifecycle() == ManagedWindowLifecycle::Starting)
        );
    }

    #[test]
    fn managed_keys_and_cancel_watermark_are_checked_and_monotone() {
        let (client, server) = managed_packet_window_channel();
        let key2 = ManagedStageKey::INITIAL.checked_next().unwrap();
        let key3 = key2.checked_next().unwrap();
        assert_eq!(ManagedStageKey(u64::MAX).checked_next(), None);

        client.control().request_cancel_through(key3);
        client.control().request_cancel_through(key2);
        assert_eq!(server.shared.cancel_watermark(), key3.get());
        let identity =
            ManagedGenerationIdentity::new(key2, SessionId::from_raw(77), 2, &"ab".repeat(32));
        assert_eq!(identity.key(), key2);
        assert_eq!(identity.project_generation(), 2);
    }

    #[test]
    fn close_and_host_stop_are_independent_out_of_band_signals() {
        let (client, server) = managed_packet_window_channel();
        let control = client.control();
        control.request_view_close();
        assert!(server.shared.view_close_requested());
        assert!(!server.shared.host_stop_requested());
        control.request_host_stop();
        control.request_host_stop();
        assert!(server.shared.host_stop_requested());
        assert!(!control.is_closed());
    }

    #[test]
    fn presented_projection_is_for_the_actual_physical_aspect() {
        let view = presented(ManagedStageKey::INITIAL, 1, 1);
        let expected = view
            .view()
            .view_projection(640.0 / 360.0)
            .unwrap()
            .to_cols_array();
        assert_eq!(view.view_projection(), &expected);
        assert_eq!(view.physical_width(), 640);
        assert_eq!(view.physical_height(), 360);
    }
}
