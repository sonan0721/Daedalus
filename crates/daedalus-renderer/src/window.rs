use std::{
    collections::{HashSet, VecDeque},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    time::{Duration, Instant},
};

use glam::Vec3;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowAttributes, WindowId},
};

/// Key code type exposed by the window input observer.
pub use winit::keyboard::KeyCode as WindowKeyCode;

use crate::packet::valid_source_digest;
use crate::{
    Camera, CpuFrameHandle, PacketAssetLimits, PacketAssetResourceStats, PacketRenderError,
    PacketRenderer, PacketRendererLimits, PacketResourceStats, PacketStageError, PacketStageId,
    PacketStageInput, PacketStageLimits, PacketStageState, PacketStageStatus, RenderPacket,
    RenderScene, RenderView, Renderer, RendererError, RendererInfo, WindowGpuOwner,
    WindowGpuOwnerPhase,
};
use daedalus_core::{SessionId, Tick};

/// Selects whether native controls may operate the window-local camera.
///
/// `Native` preserves the interactive behavior of existing window APIs.
/// `ProtocolOnly` leaves window lifecycle events active while preventing
/// physical keyboard, pointer, and wheel controls from changing the view or
/// reaching the input observer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowInputPolicy {
    /// Native controls operate the local camera and are forwarded to observers.
    #[default]
    Native,
    /// The protocol is the only source of view changes and gameplay input.
    ProtocolOnly,
}

impl WindowInputPolicy {
    const fn allows_local_camera(self) -> bool {
        matches!(self, Self::Native)
    }

    const fn forwards_native_controls(self) -> bool {
        self.allows_local_camera()
    }
}

mod capture;
mod managed;
pub use capture::*;
pub(crate) use capture::{
    CellLockError as ManagedCaptureCellLockError, ManagedCaptureCell, ManagedCaptureCellGuard,
    checked_capture_buffer_layout,
};
use managed::ManagedPresentedReservationError;
pub use managed::*;

/// Maximum CPU-side scene geometry accepted by the native window path.
/// These limits bound the validation-scene upload; they do not describe mesh
/// residency or a general asset streaming policy.
pub const MAX_WINDOW_SCENE_OBJECTS: usize = 4_096;
pub const MAX_WINDOW_SCENE_VERTICES: usize = 1_000_000;
pub const MAX_WINDOW_SCENE_INDICES: usize = 3_000_000;
pub const MAX_WINDOW_SCENE_GEOMETRY_BYTES: usize = 64 * 1024 * 1024;

/// Maximum number of recent CPU frame intervals retained in a window report.
pub const FRAME_INTERVAL_SAMPLE_CAPACITY: usize = 512;
const MAX_MESSAGES_PER_EVENT: usize = 64;
const MANAGED_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(8);

/// Messages from runtime/presentation producers to the window-owned renderer.
/// The receiver is drained with `try_recv`; a delayed simulation or producer
/// never blocks OS events or local presentation state.
#[derive(Clone, Debug)]
pub enum WindowMessage {
    /// Atomically publishes one owned scene with the loading state and the
    /// runtime's presentation identity. Arrow/R keys still flow through the
    /// input observer; this message never reads runtime-owned state.
    Present {
        scene: RenderScene,
        loading: bool,
        sequence: u64,
        tick: u64,
        generation: u64,
    },
    /// Compatibility message for callers predating atomic presentation.
    ReplaceScene(RenderScene),
    /// Compatibility message for callers predating atomic presentation.
    SetLoading(bool),
    /// Stops the window with a producer-side bridge failure. This lets a CLI
    /// propagate conversion/runtime errors from its presentation thread.
    Abort(String),
}

/// Additive packet presentation channel. It leaves the legacy message enum
/// exhaustive for existing callers.
#[derive(Clone, Debug)]
pub enum PacketWindowMessage {
    Present {
        packet: Arc<RenderPacket>,
        loading: bool,
    },
    SetLoading(bool),
    Close,
    Abort(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PacketWindowError {
    #[error("invalid packet window options: {0}")]
    InvalidOptions(String),
    #[error("packet window event loop failed: {0}")]
    EventLoop(String),
    #[error("packet presentation failed: {0}")]
    Packet(#[from] PacketRenderError),
    #[error("packet window renderer failed: {0}")]
    Renderer(#[from] RendererError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PacketTransition {
    Idempotent,
    New,
}

fn validate_packet_transition(
    previous: &Arc<RenderPacket>,
    next: &Arc<RenderPacket>,
    limits: PacketRendererLimits,
) -> Result<PacketTransition, PacketRenderError> {
    if Arc::ptr_eq(previous, next) || previous.as_ref() == next.as_ref() {
        return Ok(PacketTransition::Idempotent);
    }
    if next.session() != previous.session() {
        return Err(PacketRenderError::InvalidPacket(
            "packet session changed".into(),
        ));
    }
    if next.sequence() <= previous.sequence() {
        return Err(PacketRenderError::InvalidPacket(
            "packet sequence moved backwards or was reused with conflicting contents".into(),
        ));
    }
    if next.project_generation() < previous.project_generation() {
        return Err(PacketRenderError::InvalidPacket(
            "project generation moved backwards".into(),
        ));
    }
    if next.project_generation() == previous.project_generation()
        && next.source_digest() != previous.source_digest()
    {
        return Err(PacketRenderError::InvalidPacket(
            "source digest changed without a project generation change".into(),
        ));
    }
    if next.tick() < previous.tick() {
        return Err(PacketRenderError::InvalidPacket(
            "packet tick moved backwards".into(),
        ));
    }
    next.validate_reusing(limits, Some(previous))?;
    Ok(PacketTransition::New)
}

type FrameProgressMetadata<'a> = (SessionId, u64, &'a str, u64, Tick, Option<u64>);

fn packet_progress_metadata(packet: &RenderPacket) -> FrameProgressMetadata<'_> {
    (
        packet.session(),
        packet.project_generation(),
        packet.source_digest(),
        packet.sequence(),
        packet.tick(),
        packet.authority_snapshot_sequence(),
    )
}

fn managed_frame_progress_error(
    previous: FrameProgressMetadata<'_>,
    next: &RenderPacket,
) -> Option<&'static str> {
    let (session, generation, digest, sequence, tick, snapshot_sequence) = previous;
    if next.session() != session || next.project_generation() < generation {
        return Some("managed frame session or generation moved backwards");
    }
    if next.project_generation() == generation && next.source_digest() != digest {
        return Some("managed frame source digest changed within a generation");
    }
    if next.sequence() <= sequence {
        return Some("managed frame publication sequence did not advance");
    }
    if next.tick() < tick {
        return Some("managed frame tick moved backwards");
    }
    match (snapshot_sequence, next.authority_snapshot_sequence()) {
        (Some(previous), Some(next)) if next < previous => {
            Some("managed frame authority snapshot sequence moved backwards")
        }
        (Some(_), None) => Some("managed frame dropped its authority snapshot sequence"),
        _ => None,
    }
}

fn managed_frame_is_cancelled(
    key: ManagedStageKey,
    cancel_through: u64,
    is_active_key: bool,
) -> bool {
    key.get() <= cancel_through && !is_active_key
}

fn managed_view_input_enabled(mode: WindowMode, has_presented_active_frame: bool) -> bool {
    mode != WindowMode::Managed || has_presented_active_frame
}

#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Requests redraws near the requested interval. The operating system and
    /// display presentation mode may impose a different cadence.
    pub frame_interval: Duration,
    /// Optional bounded observer queue for OS input. Native controls are
    /// forwarded only under `WindowInputPolicy::Native`; focus and resize
    /// lifecycle events remain observable under either policy.
    pub input_events: Option<WindowInputSink>,
    /// Loading state to display from the first native frame.
    pub initial_loading: bool,
    /// Optional smoke-test limit. `None` keeps an interactive window open
    /// until the user closes it.
    pub max_run_time: Option<Duration>,
    /// Optional smoke-test frame limit, counting successfully presented frames.
    pub max_frames: Option<u64>,
    /// Treat loss of the presentation producer as a fatal window error. This
    /// is opt-in so standalone and legacy interactive callers remain open.
    pub fail_on_presentation_disconnect: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "Daedalus".into(),
            width: 1280,
            height: 720,
            frame_interval: Duration::from_nanos(6_944_444),
            input_events: None,
            initial_loading: false,
            max_run_time: None,
            max_frames: None,
            fail_on_presentation_disconnect: false,
        }
    }
}

/// A bounded, nonblocking observer for native window input. A full or
/// disconnected queue drops a forwarded observer event and increments
/// `dropped_event_count()`.
#[derive(Clone, Debug)]
pub struct WindowInputSink {
    sender: SyncSender<WindowInput>,
    dropped: Arc<AtomicU64>,
}

impl WindowInputSink {
    pub fn dropped_event_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn try_send(&self, event: WindowInput) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Creates a bounded input observer queue. Capacity must be positive.
pub fn window_input_channel(
    capacity: usize,
) -> Result<(WindowInputSink, Receiver<WindowInput>), RendererError> {
    if capacity == 0 {
        return Err(RendererError::Window(
            "input observer queue capacity must be positive".into(),
        ));
    }
    let (sender, receiver) = mpsc::sync_channel(capacity);
    Ok((
        WindowInputSink {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        receiver,
    ))
}

/// Native input received by the window loop. Under `WindowInputPolicy::Native`,
/// keyboard, pointer, and wheel controls are forwarded even when they also
/// operate the camera locally. `ProtocolOnly` records those physical events in
/// window counters without forwarding them to this observer.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowInput {
    Key { code: KeyCode, pressed: bool },
    MouseButton { button: MouseButton, pressed: bool },
    CursorMoved { x: f64, y: f64 },
    MouseWheel { lines: f32 },
    Resized { width: u32, height: u32 },
    Focused(bool),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationMetadata {
    pub sequence: u64,
    pub tick: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowExitReason {
    CloseRequested,
    EscapePressed,
    TimeLimit,
    FrameLimit,
    EventLoopReturned,
}

/// Bounded sample of CPU time between successful surface presents. This is
/// neither a GPU completion time nor a frame-rate guarantee.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameIntervalStats {
    pub sample_capacity: usize,
    /// Oldest-to-newest retained intervals in milliseconds.
    pub samples_ms: Vec<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowReport {
    pub rendered_frames: u64,
    pub loading_frames: u64,
    /// All observed input events, including key, pointer, focus, and resize.
    pub input_events: u64,
    pub loading_input_events: u64,
    pub key_events: u64,
    pub loading_key_events: u64,
    pub mouse_button_events: u64,
    pub loading_mouse_button_events: u64,
    pub cursor_events: u64,
    pub loading_cursor_events: u64,
    pub wheel_events: u64,
    pub loading_wheel_events: u64,
    pub resize_events: u64,
    pub loading_resize_events: u64,
    pub focus_events: u64,
    pub loading_focus_events: u64,
    pub local_camera_changes: u64,
    pub final_camera: Camera,
    pub resize_count: u64,
    pub final_width: u32,
    pub final_height: u32,
    pub frame_intervals: FrameIntervalStats,
    pub exit_reason: WindowExitReason,
    pub renderer: Option<RendererInfo>,
    /// True if the presentation sender was dropped. The window remains open
    /// and interactive so legacy and standalone callers retain their behavior.
    pub message_source_disconnected: bool,
    pub final_loading: bool,
    pub latest_presentation: Option<PresentationMetadata>,
    /// Current title plus the latest tick/loading state and native controls.
    pub window_title: String,
}

/// Report plus the final owned presentation scene. A CLI may use this scene
/// for offscreen capture after the native event loop has returned.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOutcome {
    pub report: WindowReport,
    pub final_scene: RenderScene,
}

/// Identity and view parameters for a packet whose surface frame was submitted
/// and passed to winit's `present` method.
#[derive(Clone, Debug, PartialEq)]
pub struct PresentedPacket {
    pub session: SessionId,
    pub tick: Tick,
    pub sequence: u64,
    pub project_generation: u64,
    pub source_digest: String,
    pub camera: Camera,
    pub physical_width: u32,
    pub physical_height: u32,
}

/// Packet-window final metadata. It does not claim GPU completion or scanout.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketWindowOutcome {
    pub report: WindowReport,
    pub final_packet: Arc<RenderPacket>,
    pub last_presented: Option<PresentedPacket>,
    pub resource_stats: PacketResourceStats,
}

/// Packet presentation outcome with imported-model resource counters.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketAssetWindowOutcome {
    pub packet: PacketWindowOutcome,
    pub asset_resource_stats: crate::PacketAssetResourceStats,
}

/// Metadata for the exact packet and camera/projection shown by one successful surface present.
#[derive(Clone, Debug, PartialEq)]
pub struct PresentedView {
    packet: PresentedPacket,
    view: RenderView,
}

impl PresentedView {
    pub fn packet(&self) -> &PresentedPacket {
        &self.packet
    }

    pub fn view(&self) -> &RenderView {
        &self.view
    }
}

/// Native packet outcome with the exact last-presented and final viewer state.
#[derive(Clone, Debug, PartialEq)]
pub struct PacketPresentationWindowOutcome {
    asset_outcome: PacketAssetWindowOutcome,
    last_presented: Option<PresentedView>,
    final_view: RenderView,
}

impl PacketPresentationWindowOutcome {
    pub fn packet_outcome(&self) -> &PacketWindowOutcome {
        &self.asset_outcome.packet
    }

    pub fn asset_resource_stats(&self) -> &crate::PacketAssetResourceStats {
        &self.asset_outcome.asset_resource_stats
    }

    pub fn last_presented(&self) -> Option<&PresentedView> {
        self.last_presented.as_ref()
    }

    pub fn final_view(&self) -> &RenderView {
        &self.final_view
    }

    pub fn into_asset_outcome(self) -> PacketAssetWindowOutcome {
        self.asset_outcome
    }
}

/// Opens a native winit window and returns its report and final owned scene.
/// The event loop stays on the calling/main thread. Messages are drained with
/// a bounded `try_recv` loop; callbacks do not wait for a producer or perform
/// readback/file I/O/thread joins.
pub fn run_windowed_reported(
    initial_scene: RenderScene,
    messages: Receiver<WindowMessage>,
    options: WindowOptions,
) -> Result<WindowOutcome, RendererError> {
    validate_options(&options)?;
    validate_window_scene(&initial_scene)?;
    let event_loop = EventLoop::new().map_err(|error| RendererError::Window(error.to_string()))?;
    let mut app = WindowApp::new(initial_scene, messages, options);
    event_loop
        .run_app(&mut app)
        .map_err(|error| RendererError::Window(error.to_string()))?;
    if let Some(error) = app.failure.take() {
        return Err(match error {
            WindowFailure::Legacy(error) => error,
            WindowFailure::Packet(error) => RendererError::Window(error.to_string()),
        });
    }
    Ok(app.into_outcome())
}

/// Compatibility wrapper for callers that only need the interactive window.
pub fn run_windowed(
    initial_scene: RenderScene,
    messages: Receiver<WindowMessage>,
    options: WindowOptions,
) -> Result<(), RendererError> {
    run_windowed_reported(initial_scene, messages, options).map(|_| ())
}

/// Opens the same native presentation loop with persistent packet resources.
pub fn run_packet_windowed_reported(
    initial_packet: Arc<RenderPacket>,
    messages: Receiver<PacketWindowMessage>,
    options: WindowOptions,
    limits: PacketRendererLimits,
) -> Result<PacketWindowOutcome, PacketWindowError> {
    run_packet_windowed_reported_with_asset_limits(
        initial_packet,
        messages,
        options,
        limits,
        PacketAssetLimits::default(),
    )
    .map(|outcome| outcome.packet)
}

/// Opens the persistent packet path with explicit model texture/residency limits.
pub fn run_packet_windowed_reported_with_asset_limits(
    initial_packet: Arc<RenderPacket>,
    messages: Receiver<PacketWindowMessage>,
    options: WindowOptions,
    limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
) -> Result<PacketAssetWindowOutcome, PacketWindowError> {
    run_packet_windowed_presented(initial_packet, messages, options, limits, asset_limits)
        .map(PacketPresentationWindowOutcome::into_asset_outcome)
}

pub fn run_packet_windowed_presented(
    initial_packet: Arc<RenderPacket>,
    messages: Receiver<PacketWindowMessage>,
    options: WindowOptions,
    limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
) -> Result<PacketPresentationWindowOutcome, PacketWindowError> {
    run_packet_windowed_presented_with_input_policy(
        initial_packet,
        messages,
        options,
        limits,
        asset_limits,
        WindowInputPolicy::Native,
    )
}

/// Opens the persistent packet path with an explicit policy for native input.
pub fn run_packet_windowed_presented_with_input_policy(
    initial_packet: Arc<RenderPacket>,
    messages: Receiver<PacketWindowMessage>,
    options: WindowOptions,
    limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    input_policy: WindowInputPolicy,
) -> Result<PacketPresentationWindowOutcome, PacketWindowError> {
    validate_options(&options)
        .map_err(|error| PacketWindowError::InvalidOptions(error.to_string()))?;
    limits.validate().map_err(PacketWindowError::Packet)?;
    asset_limits.validate().map_err(PacketWindowError::Packet)?;
    initial_packet
        .validate(limits)
        .map_err(PacketWindowError::Packet)?;
    let event_loop = build_event_loop(input_policy)
        .map_err(|error| PacketWindowError::EventLoop(error.to_string()))?;
    let mut app = WindowApp::new_packet(
        initial_packet,
        messages,
        options,
        limits,
        asset_limits,
        input_policy,
    );
    event_loop
        .run_app(&mut app)
        .map_err(|error| PacketWindowError::EventLoop(error.to_string()))?;
    if let Some(error) = app.failure.take() {
        return Err(match error {
            WindowFailure::Packet(error) => error,
            WindowFailure::Legacy(error) => PacketWindowError::Renderer(error),
        });
    }
    let asset_resource_stats = app.packet_renderer.as_ref().map_or_else(
        crate::PacketAssetResourceStats::default,
        PacketRenderer::asset_resource_stats,
    );
    let last_presented = app.last_presented_view.clone();
    let final_view = app.current_view();
    Ok(PacketPresentationWindowOutcome {
        asset_outcome: PacketAssetWindowOutcome {
            packet: app.into_packet_outcome(),
            asset_resource_stats,
        },
        last_presented,
        final_view,
    })
}

/// Opens the staged packet window and always returns its GPU owner, including
/// startup errors and unwinds. The event loop runs on the calling thread.
pub fn run_packet_windowed_managed(
    initial: PacketStageInput,
    server: ManagedPacketWindowServer,
    options: ManagedPacketWindowOptions,
) -> ManagedWindowRun {
    run_packet_windowed_managed_with_input_policy(
        initial,
        server,
        options,
        WindowInputPolicy::Native,
    )
}

/// Opens the staged packet window with an explicit native-input policy.
pub fn run_packet_windowed_managed_with_input_policy(
    initial: PacketStageInput,
    server: ManagedPacketWindowServer,
    options: ManagedPacketWindowOptions,
    input_policy: WindowInputPolicy,
) -> ManagedWindowRun {
    if let Some(capture) = server.capture.as_ref() {
        capture.mark_window_started();
    }
    let mut app = WindowApp::new_managed(initial, server, options, input_policy);
    let run_result = catch_unwind(AssertUnwindSafe(|| {
        app.validate_managed_start()?;
        let event_loop = build_event_loop(input_policy).map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::EventLoop, error.to_string())
        })?;
        event_loop.run_app(&mut app).map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::EventLoop, error.to_string())
        })
    }));
    let outcome = match run_result {
        Ok(result) => result,
        Err(_) => Err(ManagedWindowError::internal_panic()),
    };
    app.finish_managed(outcome)
}

fn build_event_loop(
    input_policy: WindowInputPolicy,
) -> Result<EventLoop<()>, winit::error::EventLoopError> {
    #[cfg(target_os = "macos")]
    if input_policy == WindowInputPolicy::ProtocolOnly {
        use winit::platform::macos::EventLoopBuilderExtMacOS;

        let mut builder = EventLoop::builder();
        builder.with_activate_ignoring_other_apps(false);
        return builder.build();
    }

    let _ = input_policy;
    EventLoop::new()
}

fn validate_options(options: &WindowOptions) -> Result<(), RendererError> {
    if options.width == 0 || options.height == 0 || options.frame_interval.is_zero() {
        return Err(RendererError::Window(
            "window size and redraw interval must be nonzero".into(),
        ));
    }
    if options
        .max_run_time
        .is_some_and(|duration| duration.is_zero())
        || options.max_frames.is_some_and(|frames| frames == 0)
    {
        return Err(RendererError::Window(
            "optional window run limits must be nonzero".into(),
        ));
    }
    Ok(())
}

fn max_run_time_reached(started_at: Instant, now: Instant, limit: Option<Duration>) -> bool {
    limit.is_some_and(|limit| now.saturating_duration_since(started_at) >= limit)
}

fn managed_capture_wait_until(
    now: Instant,
    scheduled: Instant,
    request_deadline: Option<Instant>,
) -> Instant {
    let retry = now
        .checked_add(MANAGED_CAPTURE_POLL_INTERVAL)
        .unwrap_or(scheduled);
    let mut wake = scheduled.min(retry);
    if let Some(deadline) = request_deadline.filter(|deadline| *deadline > now) {
        wake = wake.min(deadline);
    }
    wake
}

fn max_frames_reached(rendered_frames: u64, limit: Option<u64>) -> bool {
    limit.is_some_and(|limit| rendered_frames >= limit)
}

fn validate_window_scene(scene: &RenderScene) -> Result<(), RendererError> {
    if scene.objects.len() > MAX_WINDOW_SCENE_OBJECTS {
        return Err(RendererError::InvalidScene(format!(
            "window scene exceeds bounded validation-scene budget (objects: {}/{MAX_WINDOW_SCENE_OBJECTS})",
            scene.objects.len(),
        )));
    }
    let mut vertices = 0usize;
    let mut indices = 0usize;
    let mut geometry_bytes = 0usize;
    for object in &scene.objects {
        vertices = vertices
            .checked_add(object.mesh.vertices.len())
            .ok_or_else(|| RendererError::InvalidScene("window vertex count overflowed".into()))?;
        indices = indices
            .checked_add(object.mesh.indices.len())
            .ok_or_else(|| RendererError::InvalidScene("window index count overflowed".into()))?;
        let vertex_bytes = object
            .mesh
            .vertices
            .len()
            .checked_mul(std::mem::size_of::<crate::Vertex>())
            .ok_or_else(|| RendererError::InvalidScene("window vertex bytes overflowed".into()))?;
        let index_bytes = object
            .mesh
            .indices
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| RendererError::InvalidScene("window index bytes overflowed".into()))?;
        geometry_bytes = geometry_bytes
            .checked_add(vertex_bytes)
            .and_then(|size| size.checked_add(index_bytes))
            .ok_or_else(|| {
                RendererError::InvalidScene("window geometry bytes overflowed".into())
            })?;
    }
    if vertices > MAX_WINDOW_SCENE_VERTICES
        || indices > MAX_WINDOW_SCENE_INDICES
        || geometry_bytes > MAX_WINDOW_SCENE_GEOMETRY_BYTES
    {
        return Err(RendererError::InvalidScene(format!(
            "window scene exceeds bounded validation-scene budget (objects: {}/{MAX_WINDOW_SCENE_OBJECTS}, vertices: {vertices}/{MAX_WINDOW_SCENE_VERTICES}, indices: {indices}/{MAX_WINDOW_SCENE_INDICES}, geometry: {geometry_bytes}/{MAX_WINDOW_SCENE_GEOMETRY_BYTES} bytes)",
            scene.objects.len(),
        )));
    }
    // Counts and byte budgets are cheap to inspect from vector lengths. Only
    // after admission do we walk vertices, indices, transforms, and materials.
    scene.validate()
}

fn presentation_disconnect_error(fail_on_disconnect: bool) -> Option<RendererError> {
    fail_on_disconnect.then(|| {
        RendererError::Window(
            "presentation message source disconnected before the window closed".into(),
        )
    })
}

fn apply_presentation(
    current_scene: &mut RenderScene,
    current_loading: &mut bool,
    latest_presentation: &mut Option<PresentationMetadata>,
    incoming_scene: RenderScene,
    loading: bool,
    metadata: PresentationMetadata,
    local_camera: Option<&Camera>,
) -> Result<(), RendererError> {
    // Validate the complete producer-owned scene before changing any visible
    // state. A failed update leaves scene, loading, and metadata untouched.
    validate_window_scene(&incoming_scene)?;
    let mut next_scene = incoming_scene;
    if let Some(camera) = local_camera {
        next_scene.camera = camera.clone();
    }
    *current_scene = next_scene;
    *current_loading = loading;
    *latest_presentation = Some(metadata);
    Ok(())
}

struct SurfaceState {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

#[derive(Default)]
struct WindowCounters {
    rendered_frames: u64,
    loading_frames: u64,
    input_events: u64,
    loading_input_events: u64,
    key_events: u64,
    loading_key_events: u64,
    mouse_button_events: u64,
    loading_mouse_button_events: u64,
    cursor_events: u64,
    loading_cursor_events: u64,
    wheel_events: u64,
    loading_wheel_events: u64,
    resize_events: u64,
    loading_resize_events: u64,
    focus_events: u64,
    loading_focus_events: u64,
    local_camera_changes: u64,
    resize_count: u64,
}

struct FrameIntervals {
    last_present: Option<Instant>,
    samples_ms: VecDeque<f64>,
}

impl FrameIntervals {
    fn new() -> Self {
        Self {
            last_present: None,
            samples_ms: VecDeque::with_capacity(FRAME_INTERVAL_SAMPLE_CAPACITY),
        }
    }

    fn record_present(&mut self, now: Instant) {
        if let Some(previous) = self.last_present.replace(now) {
            let sample = now.saturating_duration_since(previous).as_secs_f64() * 1_000.0;
            if self.samples_ms.len() == FRAME_INTERVAL_SAMPLE_CAPACITY {
                self.samples_ms.pop_front();
            }
            self.samples_ms.push_back(sample);
        }
    }

    fn report(&self) -> FrameIntervalStats {
        let samples_ms: Vec<_> = self.samples_ms.iter().copied().collect();
        let mut sorted = samples_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let percentile = |fraction: f64| {
            if sorted.is_empty() {
                None
            } else {
                let index = ((fraction * sorted.len() as f64).ceil() as usize)
                    .saturating_sub(1)
                    .min(sorted.len() - 1);
                Some(sorted[index])
            }
        };
        FrameIntervalStats {
            sample_capacity: FRAME_INTERVAL_SAMPLE_CAPACITY,
            p50_ms: percentile(0.50),
            p95_ms: percentile(0.95),
            p99_ms: percentile(0.99),
            max_ms: sorted.last().copied(),
            samples_ms,
        }
    }
}

enum WindowFailure {
    Legacy(RendererError),
    Packet(PacketWindowError),
}

impl From<RendererError> for WindowFailure {
    fn from(error: RendererError) -> Self {
        Self::Legacy(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowMode {
    Legacy,
    Packet,
    Managed,
}

struct ManagedFrame {
    key: ManagedStageKey,
    identity: ManagedGenerationIdentity,
    frame: Arc<CpuFrameHandle>,
}

struct ManagedStage {
    key: ManagedStageKey,
    identity: ManagedGenerationIdentity,
    limits: PacketStageLimits,
    input: Option<PacketStageInput>,
    id: Option<PacketStageId>,
    frame: Option<Arc<CpuFrameHandle>>,
    event_published: bool,
    cancellation_requested: bool,
    last_status: Option<PacketStageStatus>,
}

struct ManagedWindowState {
    server: ManagedPacketWindowServer,
    initial: ManagedGenerationIdentity,
    active: Option<ManagedFrame>,
    stage: Option<ManagedStage>,
    highest_key: ManagedStageKey,
    deferred_key: Option<ManagedStageKey>,
    pending_event: Option<ManagedStageEvent>,
    present_sequence: u64,
    last_presented: Option<ManagedPresentedView>,
    bootstrap_frames: u64,
}

struct WindowApp {
    messages: Option<Receiver<WindowMessage>>,
    packet_messages: Option<Receiver<PacketWindowMessage>>,
    options: WindowOptions,
    input_policy: WindowInputPolicy,
    mode: WindowMode,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    packet_renderer: Option<PacketRenderer>,
    gpu_owner: Option<WindowGpuOwner>,
    managed: Option<ManagedWindowState>,
    capture_request: Option<Box<ManagedCaptureRequest>>,
    managed_failure: Option<ManagedWindowError>,
    managed_reason: Option<ManagedWindowExitReason>,
    packet_limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    packet: Option<Arc<RenderPacket>>,
    last_presented: Option<PresentedPacket>,
    last_presented_view: Option<PresentedView>,
    renderer_info: Option<RendererInfo>,
    surface: Option<SurfaceState>,
    scene: RenderScene,
    local_camera: Option<Camera>,
    local_view: Option<RenderView>,
    latest_presentation: Option<PresentationMetadata>,
    loading_since: Option<Instant>,
    message_source_disconnected: bool,
    keys_down: HashSet<KeyCode>,
    left_mouse_down: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    last_update: Instant,
    next_redraw: Instant,
    started_at: Instant,
    exit_reason: Option<WindowExitReason>,
    failure: Option<WindowFailure>,
    counters: WindowCounters,
    intervals: FrameIntervals,
    final_width: u32,
    final_height: u32,
    window_title: String,
}

fn update_managed_frame(
    managed: &mut ManagedWindowState,
    key: ManagedStageKey,
    frame: Arc<CpuFrameHandle>,
    cancel_through: u64,
) -> Result<(), &'static str> {
    let is_current_active_key = managed
        .active
        .as_ref()
        .is_some_and(|active| active.key == key);
    if managed_frame_is_cancelled(key, cancel_through, is_current_active_key) {
        return Ok(());
    }

    let frame_matches_identity = |identity: &ManagedGenerationIdentity| {
        frame.session() == identity.session()
            && frame.project_generation() == identity.project_generation()
            && frame.source_digest() == identity.source_digest()
    };
    let mut update_active = false;
    let mut update_stage = false;
    let mut invalid_message = None;

    if let Some(active) = managed.active.as_ref()
        && active.key == key
    {
        if !frame_matches_identity(&active.identity) {
            invalid_message = Some("active frame identity does not match its stage key");
        } else {
            let current_sequence = active.frame.packet().sequence();
            let next_sequence = frame.packet().sequence();
            if next_sequence < current_sequence {
                return Ok(());
            }
            if next_sequence == current_sequence {
                if Arc::ptr_eq(&active.frame, &frame) {
                    return Ok(());
                }
                invalid_message =
                    Some("active frame reused a publication sequence with a different frame");
            } else if let Some(message) = managed_frame_progress_error(
                packet_progress_metadata(active.frame.packet()),
                frame.packet(),
            ) {
                invalid_message = Some(message);
            } else {
                update_active = true;
            }
        }
    } else if let Some(stage) = managed.stage.as_ref()
        && stage.key == key
    {
        let ready = stage
            .last_status
            .as_ref()
            .is_some_and(|status| status.state() == PacketStageState::Ready)
            && stage.event_published;
        if !ready || !frame_matches_identity(&stage.identity) {
            invalid_message = Some("candidate frame arrived before matching stage readiness");
        } else if let Some(current) = stage.frame.as_ref() {
            let current_sequence = current.packet().sequence();
            let next_sequence = frame.packet().sequence();
            if next_sequence < current_sequence {
                return Ok(());
            }
            if next_sequence == current_sequence {
                if Arc::ptr_eq(current, &frame) {
                    return Ok(());
                }
                invalid_message =
                    Some("candidate frame reused a publication sequence with a different frame");
            } else if let Some(message) = managed_frame_progress_error(
                packet_progress_metadata(current.packet()),
                frame.packet(),
            ) {
                invalid_message = Some(message);
            } else {
                update_stage = true;
            }
        } else {
            let previous = managed
                .active
                .as_ref()
                .map(|active| packet_progress_metadata(active.frame.packet()))
                .or_else(|| {
                    managed.last_presented.as_ref().map(|presented| {
                        (
                            presented.session(),
                            presented.project_generation(),
                            presented.source_digest(),
                            presented.publication_sequence(),
                            presented.tick(),
                            presented.authority_snapshot_sequence(),
                        )
                    })
                });
            if let Some(message) =
                previous.and_then(|previous| managed_frame_progress_error(previous, frame.packet()))
            {
                invalid_message = Some(message);
            } else {
                update_stage = true;
            }
        }
    } else if managed
        .active
        .as_ref()
        .is_some_and(|active| key < active.key)
    {
        // A late frame from an older already-promoted generation is harmlessly stale.
        return Ok(());
    } else {
        invalid_message = Some("frame does not match the active or staged generation");
    }

    if let Some(message) = invalid_message {
        return Err(message);
    }
    if update_active && let Some(active) = managed.active.as_mut() {
        active.frame = frame;
    } else if update_stage && let Some(stage) = managed.stage.as_mut() {
        stage.frame = Some(frame);
    }
    Ok(())
}

impl WindowApp {
    fn new(
        initial_scene: RenderScene,
        messages: Receiver<WindowMessage>,
        options: WindowOptions,
    ) -> Self {
        let now = Instant::now();
        let initial_loading = options.initial_loading;
        let width = options.width;
        let height = options.height;
        Self::with_source(
            Some(messages),
            None,
            initial_scene,
            None,
            options,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
            now,
            initial_loading,
            width,
            height,
        )
    }

    fn new_packet(
        initial_packet: Arc<RenderPacket>,
        messages: Receiver<PacketWindowMessage>,
        options: WindowOptions,
        limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
        input_policy: WindowInputPolicy,
    ) -> Self {
        let now = Instant::now();
        let initial_loading = options.initial_loading;
        let width = options.width;
        let height = options.height;
        let mut app = Self::with_source(
            None,
            Some(messages),
            RenderScene::new(),
            Some(initial_packet),
            options,
            limits,
            asset_limits,
            now,
            initial_loading,
            width,
            height,
        );
        app.input_policy = input_policy;
        app
    }

    fn new_managed(
        initial: PacketStageInput,
        server: ManagedPacketWindowServer,
        options: ManagedPacketWindowOptions,
        input_policy: WindowInputPolicy,
    ) -> Self {
        let now = Instant::now();
        let initial_identity = ManagedGenerationIdentity::new(
            ManagedStageKey::INITIAL,
            initial.session(),
            initial.project_generation(),
            initial.source_digest(),
        );
        let initial_stage = ManagedStage {
            key: ManagedStageKey::INITIAL,
            identity: initial_identity.clone(),
            limits: options.initial_stage_limits,
            input: Some(initial),
            id: None,
            frame: None,
            event_published: false,
            cancellation_requested: false,
            last_status: None,
        };
        let initial_loading = options.window.initial_loading;
        let width = options.window.width;
        let height = options.window.height;
        let mut app = Self::with_source(
            None,
            None,
            RenderScene::new(),
            None,
            options.window,
            options.packet_limits,
            options.asset_limits,
            now,
            initial_loading,
            width,
            height,
        );
        app.mode = WindowMode::Managed;
        app.input_policy = input_policy;
        app.gpu_owner = Some(WindowGpuOwner::new());
        app.managed = Some(ManagedWindowState {
            server,
            initial: initial_identity,
            active: None,
            stage: Some(initial_stage),
            highest_key: ManagedStageKey::INITIAL,
            deferred_key: None,
            pending_event: None,
            present_sequence: 0,
            last_presented: None,
            bootstrap_frames: 0,
        });
        app
    }

    #[allow(clippy::too_many_arguments)]
    fn with_source(
        messages: Option<Receiver<WindowMessage>>,
        packet_messages: Option<Receiver<PacketWindowMessage>>,
        scene: RenderScene,
        packet: Option<Arc<RenderPacket>>,
        options: WindowOptions,
        packet_limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
        now: Instant,
        initial_loading: bool,
        width: u32,
        height: u32,
    ) -> Self {
        let latest_presentation = packet.as_ref().map(|packet| PresentationMetadata {
            sequence: packet.sequence(),
            tick: packet.tick().get(),
            generation: packet.project_generation(),
        });
        Self {
            messages,
            packet_messages,
            options,
            input_policy: WindowInputPolicy::Native,
            mode: if packet.is_some() {
                WindowMode::Packet
            } else {
                WindowMode::Legacy
            },
            window: None,
            renderer: None,
            packet_renderer: None,
            gpu_owner: None,
            managed: None,
            capture_request: None,
            managed_failure: None,
            managed_reason: None,
            packet_limits,
            asset_limits,
            packet,
            last_presented: None,
            last_presented_view: None,
            renderer_info: None,
            surface: None,
            scene,
            local_camera: None,
            local_view: None,
            latest_presentation,
            loading_since: initial_loading.then_some(now),
            message_source_disconnected: false,
            keys_down: HashSet::new(),
            left_mouse_down: false,
            last_cursor: None,
            last_update: now,
            next_redraw: now,
            started_at: now,
            exit_reason: None,
            failure: None,
            counters: WindowCounters::default(),
            intervals: FrameIntervals::new(),
            final_width: width,
            final_height: height,
            window_title: String::new(),
        }
    }

    fn validate_managed_start(&self) -> Result<(), ManagedWindowError> {
        let state = self.managed.as_ref().ok_or_else(|| {
            ManagedWindowError::new(
                ManagedWindowErrorCode::InvalidOptions,
                "managed window state is missing",
            )
        })?;
        validate_options(&self.options).map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::InvalidOptions, error.to_string())
        })?;
        if self.options.title.len() > 512 {
            return Err(ManagedWindowError::new(
                ManagedWindowErrorCode::InvalidOptions,
                "managed window title exceeds 512 UTF-8 bytes",
            ));
        }
        self.packet_limits.validate().map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::InvalidOptions, error.to_string())
        })?;
        self.asset_limits.validate().map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::InvalidOptions, error.to_string())
        })?;
        let initial = state.stage.as_ref().ok_or_else(|| {
            ManagedWindowError::new(
                ManagedWindowErrorCode::InvalidOptions,
                "managed initial stage is missing",
            )
        })?;
        let Some(input) = initial.input.as_ref() else {
            return Err(ManagedWindowError::new(
                ManagedWindowErrorCode::InvalidOptions,
                "managed initial stage input is missing",
            ));
        };
        if initial.key != ManagedStageKey::INITIAL
            || input.project_generation() != 1
            || input.session() != state.initial.session()
            || !valid_source_digest(input.source_digest())
            || input.packet_limits() != self.packet_limits
            || input.asset_limits() != self.asset_limits
            || initial.limits.upload_bytes_per_submission() == 0
            || initial.limits.upload_bytes_per_submission()
                > PacketStageLimits::default().upload_bytes_per_submission()
            || initial.limits.max_resource_creates_per_submission() == 0
            || initial.limits.max_resource_creates_per_submission()
                > PacketStageLimits::default().max_resource_creates_per_submission()
        {
            return Err(ManagedWindowError::new(
                ManagedWindowErrorCode::InvalidOptions,
                "initial stage identity or limits are invalid",
            ));
        }
        Ok(())
    }

    fn finish_managed(mut self, run_result: Result<(), ManagedWindowError>) -> ManagedWindowRun {
        let outcome = self.managed_failure.clone().map_or(run_result, Err);
        if outcome.is_err() || self.exit_reason.is_none() {
            self.exit_reason = Some(WindowExitReason::EventLoopReturned);
        }

        let resource_stats = self
            .gpu_owner
            .as_mut()
            .and_then(WindowGpuOwner::packet_renderer_mut)
            .map_or_else(PacketResourceStats::default, |renderer| {
                renderer.resource_stats()
            });
        let asset_resource_stats = self
            .gpu_owner
            .as_mut()
            .and_then(WindowGpuOwner::packet_renderer_mut)
            .map_or_else(PacketAssetResourceStats::default, |renderer| {
                renderer.asset_resource_stats()
            });
        let final_view = self.current_view();
        let report = self.make_report();
        let last_presented = self
            .managed
            .as_ref()
            .and_then(|state| state.last_presented.clone());
        let mut status = self.managed_status(ManagedWindowLifecycle::Closed);
        status.exit_reason = if outcome.is_err() {
            Some(ManagedWindowExitReason::Failed)
        } else {
            self.managed_exit_reason()
        };
        let window_outcome = outcome.map(|()| ManagedWindowOutcome {
            report,
            last_presented,
            final_view,
            resource_stats,
            asset_resource_stats,
        });

        let shared = self
            .managed
            .as_ref()
            .map(|state| Arc::clone(&state.server.shared));
        if let Some(capture) = self
            .managed
            .as_ref()
            .and_then(|state| state.server.capture.as_ref())
        {
            capture.close_window();
        }
        self.capture_request = None;
        if let Some(shared) = &shared {
            shared.mark_closed();
            let _ = shared.try_publish_status(status.clone());
        }
        if let Some(state) = self.managed.as_mut() {
            state.active = None;
            state.stage = None;
            state.pending_event = None;
        }
        self.packet = None;
        self.last_presented = None;
        self.last_presented_view = None;
        self.renderer = None;
        self.packet_renderer = None;
        drop(self.managed.take());
        drop(self.surface.take());
        drop(self.window.take());
        let gpu_owner = self.gpu_owner.take().unwrap_or_default();
        ManagedWindowRun {
            outcome: window_outcome,
            status,
            gpu_owner,
        }
    }

    fn managed_status(&self, lifecycle: ManagedWindowLifecycle) -> ManagedWindowStatus {
        let (active, staging, last_presented, bootstrap_frames, shared) =
            if let Some(state) = &self.managed {
                let staging = state.stage.as_ref().and_then(|stage| {
                    stage
                        .last_status
                        .as_ref()
                        .map(|status| PacketStageStatusSummary::from_status(stage.key, status))
                });
                (
                    state.active.as_ref().map(|active| active.identity.clone()),
                    staging,
                    state
                        .last_presented
                        .as_ref()
                        .map(ManagedPresentedSummary::from_view),
                    state.bootstrap_frames,
                    Some(&state.server.shared),
                )
            } else {
                (None, None, None, 0, None)
            };
        let owner_phase = self
            .gpu_owner
            .as_ref()
            .map_or(WindowGpuOwnerPhase::NoDevice, WindowGpuOwner::phase);
        let lifecycle = if lifecycle == ManagedWindowLifecycle::Closed {
            lifecycle
        } else if shared
            .is_some_and(|shared| shared.host_stop_requested() || shared.view_close_requested())
        {
            ManagedWindowLifecycle::Stopping
        } else if self.window.is_some() {
            ManagedWindowLifecycle::Running
        } else {
            ManagedWindowLifecycle::Starting
        };
        ManagedWindowStatus {
            lifecycle,
            exit_reason: self.managed_exit_reason(),
            owner_phase,
            active,
            staging,
            last_presented,
            rendered_frames: self.counters.rendered_frames,
            bootstrap_frames,
            physical_width: self.final_width,
            physical_height: self.final_height,
        }
    }

    fn managed_exit_reason(&self) -> Option<ManagedWindowExitReason> {
        self.managed_reason
            .or_else(|| {
                self.managed_failure
                    .as_ref()
                    .map(|_| ManagedWindowExitReason::Failed)
            })
            .or_else(|| self.exit_reason.map(managed::map_exit_reason))
    }

    fn current_camera(&self) -> &Camera {
        if let Some(camera) = self
            .local_view
            .as_ref()
            .map(RenderView::camera)
            .or(self.local_camera.as_ref())
        {
            return camera;
        }
        if let Some(packet) = &self.packet {
            return packet.camera();
        }
        if let Some(frame) = self.managed.as_ref().and_then(|managed| {
            managed
                .active
                .as_ref()
                .map(|active| &active.frame)
                .or_else(|| {
                    managed
                        .stage
                        .as_ref()
                        .and_then(|stage| stage.frame.as_ref())
                })
        }) {
            return frame.packet().camera();
        }
        &self.scene.camera
    }

    fn current_view(&self) -> RenderView {
        self.local_view.clone().unwrap_or_else(|| {
            if let Some(packet) = &self.packet {
                packet.view().clone()
            } else if let Some(frame) = self.managed.as_ref().and_then(|managed| {
                managed
                    .active
                    .as_ref()
                    .map(|active| active.frame.as_ref())
                    .or_else(|| {
                        managed
                            .stage
                            .as_ref()
                            .and_then(|stage| stage.frame.as_deref())
                    })
            }) {
                frame.packet().view().clone()
            } else {
                RenderView::perspective(self.scene.camera.clone())
            }
        })
    }

    fn sync_legacy_camera(&mut self) {
        if self.mode == WindowMode::Legacy
            && let Some(camera) = &self.local_camera
        {
            self.scene.camera = camera.clone();
        }
    }

    fn is_packet_path(&self) -> bool {
        self.mode != WindowMode::Legacy
    }

    fn validate_current_dimensions(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(), RendererError> {
        if let Some(renderer) = &self.renderer {
            renderer.validate_dimensions(width, height)
        } else if let Some(renderer) = &self.packet_renderer {
            renderer.validate_dimensions(width, height)
        } else if let Some(owner) = &mut self.gpu_owner {
            if let Some(renderer) = owner.renderer() {
                renderer.validate_dimensions(width, height)
            } else if let Some(renderer) = owner.packet_renderer_mut() {
                renderer.validate_dimensions(width, height)
            } else {
                Err(RendererError::Window("renderer is not initialized".into()))
            }
        } else {
            Err(RendererError::Window("renderer is not initialized".into()))
        }
    }

    fn is_loading(&self) -> bool {
        self.loading_since.is_some()
    }

    fn set_loading(&mut self, loading: bool) {
        if self.is_loading() != loading {
            self.loading_since = loading.then(Instant::now);
        }
        self.update_window_title();
    }

    fn update_window_title(&mut self) {
        let tick_and_generation = self.latest_presentation.as_ref().map_or_else(
            || "tick — · gen —".to_owned(),
            |metadata| format!("tick {} · gen {}", metadata.tick, metadata.generation),
        );
        let loading = if self.is_loading() {
            "Loading"
        } else {
            "Ready"
        };
        let controls = window_controls_hint(self.is_packet_path(), self.input_policy);
        self.window_title = format!(
            "{} | {tick_and_generation} · {loading} | {controls}",
            self.options.title,
        );
        if let Some(window) = &self.window {
            window.set_title(&self.window_title);
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RendererError) {
        if self.mode == WindowMode::Managed {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string()),
            );
            return;
        }
        self.failure = Some(if self.packet.is_some() {
            WindowFailure::Packet(PacketWindowError::Renderer(error))
        } else {
            WindowFailure::Legacy(error)
        });
        event_loop.exit();
    }

    fn fail_packet(&mut self, event_loop: &ActiveEventLoop, error: PacketWindowError) {
        if self.mode == WindowMode::Managed {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(ManagedWindowErrorCode::Packet, error.to_string()),
            );
            return;
        }
        self.failure = Some(WindowFailure::Packet(error));
        event_loop.exit();
    }

    fn fail_managed(&mut self, event_loop: &ActiveEventLoop, error: ManagedWindowError) {
        self.managed_failure = Some(error);
        self.exit_reason = Some(WindowExitReason::EventLoopReturned);
        let status = self.managed_status(ManagedWindowLifecycle::Stopping);
        if let Some(state) = &self.managed {
            let _ = state.server.shared.try_publish_status(status);
        }
        event_loop.exit();
    }

    fn exit(&mut self, event_loop: &ActiveEventLoop, reason: WindowExitReason) {
        self.exit_reason = Some(reason);
        if self.mode == WindowMode::Managed
            && let Some(state) = &self.managed
        {
            let _ = state
                .server
                .shared
                .try_publish_status(self.managed_status(ManagedWindowLifecycle::Stopping));
        }
        event_loop.exit();
    }

    fn exit_managed(&mut self, event_loop: &ActiveEventLoop, reason: ManagedWindowExitReason) {
        self.managed_reason = Some(reason);
        self.exit_reason = Some(managed::legacy_exit_reason(Some(reason)));
        let status = self.managed_status(ManagedWindowLifecycle::Stopping);
        if let Some(state) = &self.managed {
            let _ = state.server.shared.try_publish_status(status);
        }
        event_loop.exit();
    }

    fn initialize_managed(
        &mut self,
        event_loop: &ActiveEventLoop,
    ) -> Result<(), ManagedWindowError> {
        let attributes = WindowAttributes::default()
            .with_title(self.options.title.clone())
            .with_inner_size(LogicalSize::new(self.options.width, self.options.height))
            .with_active(self.input_policy == WindowInputPolicy::Native);
        let window = Arc::new(event_loop.create_window(attributes).map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::EventLoop, error.to_string())
        })?);
        let size = window.inner_size();
        self.final_width = size.width;
        self.final_height = size.height;
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window.clone()).map_err(|error| {
            ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string())
        })?;
        let (adapter, capabilities) = pollster::block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: Some(&surface),
                })
                .await
                .map_err(|error| {
                    ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string())
                })?;
            let capabilities = surface.get_capabilities(&adapter);
            Ok::<_, ManagedWindowError>((adapter, capabilities))
        })?;
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| {
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Renderer,
                    "surface exposes no color format",
                )
            })?;
        let capture_shared = self
            .managed
            .as_ref()
            .and_then(|managed| managed.server.capture.as_ref())
            .cloned();
        let capture_format = capture::capture_format_for_wgpu(format);
        let capture_can_copy = capture_shared
            .as_ref()
            .is_some_and(|shared| matches!(shared.state(), ManagedCaptureState::Starting))
            && capabilities.usages.contains(wgpu::TextureUsages::COPY_SRC)
            && capture_format.is_some();
        if let Some(shared) = &capture_shared
            && matches!(shared.state(), ManagedCaptureState::Starting)
        {
            let unsupported = if !capabilities.usages.contains(wgpu::TextureUsages::COPY_SRC) {
                Some(ManagedCaptureUnsupportedReason::SurfaceCopyUnavailable)
            } else if capture_format.is_none() {
                Some(ManagedCaptureUnsupportedReason::UnsupportedSurfaceFormat)
            } else {
                None
            };
            if let Some(reason) = unsupported {
                let _ = shared.try_set_unsupported(reason);
            }
        }
        let present_mode = if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::AutoVsync)
        {
            wgpu::PresentMode::AutoVsync
        } else if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Fifo)
        {
            wgpu::PresentMode::Fifo
        } else {
            *capabilities.present_modes.first().ok_or_else(|| {
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Renderer,
                    "surface exposes no presentation mode",
                )
            })?
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | if capture_can_copy {
                    wgpu::TextureUsages::COPY_SRC
                } else {
                    wgpu::TextureUsages::empty()
                },
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: preferred_surface_alpha_mode(&capabilities.alpha_modes),
            view_formats: vec![],
        };
        let owner = self.gpu_owner.as_mut().ok_or_else(|| {
            ManagedWindowError::new(
                ManagedWindowErrorCode::InternalPanic,
                "managed GPU owner is missing",
            )
        })?;
        pollster::block_on(owner.initialize_renderer(instance, adapter, format)).map_err(
            |error| ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string()),
        )?;
        let renderer = owner.renderer().ok_or_else(|| {
            ManagedWindowError::new(
                ManagedWindowErrorCode::Renderer,
                "renderer initialization did not produce a ready renderer",
            )
        })?;
        renderer
            .validate_dimensions(config.width, config.height)
            .map_err(|error| {
                ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string())
            })?;
        surface.configure(&renderer.device, &config);
        self.renderer_info = Some(managed::bounded_renderer_info(renderer.info()));
        owner
            .initialize_packet_renderer(self.packet_limits, self.asset_limits)
            .map_err(|error| {
                ManagedWindowError::new(ManagedWindowErrorCode::Packet, error.to_string())
            })?;
        if capture_can_copy
            && let (Some(shared), Some(surface_format)) = (capture_shared.as_ref(), capture_format)
            && let Some(packet_renderer) = owner.packet_renderer_mut()
        {
            packet_renderer.attach_managed_capture_cell(shared.cell());
            let max_dimension = packet_renderer
                .device()
                .limits()
                .max_texture_dimension_2d
                .min(MANAGED_CAPTURE_MAX_WIDTH)
                .min(MANAGED_CAPTURE_MAX_HEIGHT);
            let max_pixels = u64::from(max_dimension)
                .saturating_mul(u64::from(max_dimension))
                .min(MANAGED_CAPTURE_MAX_PIXELS);
            if let Ok(ready) = ManagedCaptureReadyInfo::new(
                surface_format,
                max_dimension,
                max_dimension,
                max_pixels,
            ) {
                let _ = shared.try_set_ready(ready);
            } else {
                let _ =
                    shared.try_set_unsupported(ManagedCaptureUnsupportedReason::CaptureDisabled);
            }
        }
        self.window = Some(window);
        self.surface = Some(SurfaceState { surface, config });
        self.update_window_title();
        self.admit_managed_initial_stage(event_loop);
        Ok(())
    }

    fn admit_managed_initial_stage(&mut self, event_loop: &ActiveEventLoop) {
        let (input, limits, shared) = {
            let Some(managed) = &mut self.managed else {
                return;
            };
            if managed.server.shared.cancel_watermark() >= ManagedStageKey::INITIAL.get() {
                managed.stage = None;
                managed.pending_event = Some(ManagedStageEvent::Cancelled {
                    key: ManagedStageKey::INITIAL,
                });
                (
                    None,
                    PacketStageLimits::default(),
                    Arc::clone(&managed.server.shared),
                )
            } else if let Some(stage) = managed.stage.as_mut() {
                (
                    stage.input.take(),
                    stage.limits,
                    Arc::clone(&managed.server.shared),
                )
            } else {
                return;
            }
        };
        let Some(input) = input else {
            self.publish_managed_event();
            let status = self.managed_status(ManagedWindowLifecycle::Running);
            let _ = shared.try_publish_status(status);
            return;
        };
        if shared.cancel_watermark() >= ManagedStageKey::INITIAL.get() {
            if let Some(managed) = &mut self.managed {
                managed.stage = None;
                managed.pending_event = Some(ManagedStageEvent::Cancelled {
                    key: ManagedStageKey::INITIAL,
                });
            }
            self.publish_managed_event();
            return;
        }
        let Some(renderer) = self
            .gpu_owner
            .as_mut()
            .and_then(WindowGpuOwner::packet_renderer_mut)
        else {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Packet,
                    "packet renderer is not ready for the initial stage",
                ),
            );
            return;
        };
        let result = renderer.try_begin_stage(input, limits);
        match result {
            Ok(id) => {
                if let Some(stage) = self
                    .managed
                    .as_mut()
                    .and_then(|managed| managed.stage.as_mut())
                {
                    stage.id = Some(id);
                }
            }
            Err(PacketStageError::StageBusy) => {
                if let Some(managed) = &mut self.managed {
                    managed.deferred_key = Some(ManagedStageKey::INITIAL);
                    managed.stage = None;
                    managed.pending_event = Some(ManagedStageEvent::Deferred {
                        key: ManagedStageKey::INITIAL,
                        reason: ManagedStageDeferredReason::RendererBusy,
                    });
                }
            }
            Err(error) => {
                if let Some(managed) = &mut self.managed {
                    managed.stage = None;
                    managed.pending_event = Some(ManagedStageEvent::Failed {
                        key: ManagedStageKey::INITIAL,
                        error: ManagedWindowError::new(
                            ManagedWindowErrorCode::Stage,
                            error.to_string(),
                        ),
                    });
                }
            }
        }
        self.publish_managed_event();
        let status = self.managed_status(ManagedWindowLifecycle::Running);
        let _ = shared.try_publish_status(status);
    }

    fn publish_managed_event(&mut self) {
        let Some((shared, event)) = self.managed.as_ref().map(|managed| {
            (
                Arc::clone(&managed.server.shared),
                managed.pending_event.clone(),
            )
        }) else {
            return;
        };
        if let Some(event) = event
            && shared.try_publish_stage_event(event.clone())
            && let Some(managed) = &mut self.managed
            && managed.pending_event.as_ref() == Some(&event)
        {
            managed.pending_event = None;
            if let ManagedStageEvent::Ready { key, .. } = event
                && let Some(stage) = managed.stage.as_mut()
                && stage.key == key
            {
                stage.event_published = true;
            }
        }
        let status = self.managed_status(ManagedWindowLifecycle::Running);
        let _ = shared.try_publish_status(status);
    }

    fn record_managed_stage_event(&mut self, event: ManagedStageEvent) {
        if let Some(managed) = &mut self.managed
            && managed.pending_event.is_none()
        {
            managed.pending_event = Some(event);
        }
        self.publish_managed_event();
    }

    fn poll_managed_signals(&mut self, event_loop: &ActiveEventLoop) {
        let Some(shared) = self
            .managed
            .as_ref()
            .map(|managed| Arc::clone(&managed.server.shared))
        else {
            return;
        };
        if shared.host_stop_requested() {
            self.exit_managed(event_loop, ManagedWindowExitReason::HostStop);
            return;
        }
        if shared.view_close_requested() {
            self.exit_managed(event_loop, ManagedWindowExitReason::ViewClosed);
            return;
        }
        let watermark = shared.cancel_watermark();
        let Some((key, stage_id, cancellation_requested)) = self
            .managed
            .as_ref()
            .and_then(|managed| managed.stage.as_ref())
            .map(|stage| (stage.key, stage.id, stage.cancellation_requested))
        else {
            self.publish_managed_event();
            return;
        };
        if watermark < key.get() {
            self.publish_managed_event();
            return;
        }
        if cancellation_requested {
            let Some(stage_id) = stage_id else {
                self.finish_managed_cancellation(key);
                self.publish_managed_event();
                return;
            };
            let status = self
                .gpu_owner
                .as_mut()
                .and_then(WindowGpuOwner::packet_renderer_mut)
                .ok_or(PacketStageError::InvalidStageId)
                .and_then(|renderer| renderer.try_stage_status(stage_id));
            match status {
                Ok(status)
                    if matches!(
                        status.state(),
                        PacketStageState::Cancelled | PacketStageState::Failed
                    ) =>
                {
                    self.finish_managed_cancellation(key)
                }
                Ok(status) => {
                    if let Some(stage) = self
                        .managed
                        .as_mut()
                        .and_then(|managed| managed.stage.as_mut())
                    {
                        stage.last_status = Some(status);
                    }
                }
                Err(error) => {
                    self.fail_managed(
                        event_loop,
                        ManagedWindowError::new(ManagedWindowErrorCode::Stage, error.to_string()),
                    );
                    return;
                }
            }
            self.publish_managed_event();
            return;
        }
        if let Some(stage_id) = stage_id {
            let status = self
                .gpu_owner
                .as_mut()
                .and_then(WindowGpuOwner::packet_renderer_mut)
                .ok_or(PacketStageError::InvalidStageId)
                .and_then(|renderer| renderer.try_cancel_stage(stage_id));
            match status {
                Ok(status) => {
                    if let Some(stage) = self
                        .managed
                        .as_mut()
                        .and_then(|managed| managed.stage.as_mut())
                    {
                        stage.cancellation_requested = true;
                        stage.last_status = Some(status.clone());
                    }
                    if matches!(
                        status.state(),
                        PacketStageState::Cancelled | PacketStageState::Failed
                    ) {
                        self.finish_managed_cancellation(key);
                    }
                }
                Err(error) => {
                    self.fail_managed(
                        event_loop,
                        ManagedWindowError::new(ManagedWindowErrorCode::Stage, error.to_string()),
                    );
                    return;
                }
            }
        } else {
            self.finish_managed_cancellation(key);
        }
        self.publish_managed_event();
    }

    fn finish_managed_cancellation(&mut self, key: ManagedStageKey) {
        let mut publish_cancelled = false;
        if let Some(managed) = &mut self.managed
            && managed.stage.as_ref().is_some_and(|stage| stage.key == key)
        {
            publish_cancelled = !managed
                .stage
                .as_ref()
                .is_some_and(|stage| stage.event_published);
            managed.stage = None;
        }
        if publish_cancelled && let Some(managed) = &mut self.managed {
            let replaces_unpublished_ready = matches!(
                managed.pending_event.as_ref(),
                Some(ManagedStageEvent::Ready { key: pending_key, .. }) if *pending_key == key
            );
            if managed.pending_event.is_none() || replaces_unpublished_ready {
                managed.pending_event = Some(ManagedStageEvent::Cancelled { key });
            }
        }
    }

    fn drain_managed_inputs(&mut self, event_loop: &ActiveEventLoop) {
        if self.mode != WindowMode::Managed {
            return;
        }
        self.poll_managed_capture_request();
        self.poll_managed_signals(event_loop);
        if self.managed_failure.is_some() || self.exit_reason.is_some() {
            return;
        }
        self.publish_managed_event();
        for _ in 0..MAX_MESSAGES_PER_EVENT {
            let Some(managed) = &self.managed else {
                return;
            };
            if managed.pending_event.is_some()
                || managed.server.shared.stage_event_is_occupied_or_busy()
            {
                break;
            }
            let received = managed.server.receiver.try_recv();
            match received {
                Ok(ManagedPacketWindowInput::BeginStage { key, input, limits }) => {
                    self.accept_managed_stage(key, input, limits, event_loop);
                }
                Ok(ManagedPacketWindowInput::Frame { key, frame }) => {
                    self.accept_managed_frame(key, frame, event_loop);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.exit_managed(event_loop, ManagedWindowExitReason::HostStop);
                    return;
                }
            }
            self.publish_managed_event();
            if self.managed_failure.is_some() || self.exit_reason.is_some() {
                return;
            }
        }
    }

    fn poll_managed_capture_request(&mut self) {
        let Some(shared) = self
            .managed
            .as_ref()
            .and_then(|managed| managed.server.capture.as_ref())
            .cloned()
        else {
            self.capture_request = None;
            return;
        };

        if let Some(request) = self.capture_request.as_ref() {
            let key = request.key();
            let cell = shared.cell();
            if cell.current_key() != Some(key) || cell.is_released() {
                // The worker may have consumed a terminal result and reused the
                // single cell before this window receives another eligible draw.
                self.capture_request = None;
            } else {
                let terminal_failure = if cell.is_cancelled(key) {
                    Some(ManagedCaptureFailure::Cancelled)
                } else if Instant::now() >= request.deadline() {
                    Some(ManagedCaptureFailure::DeadlineElapsed)
                } else {
                    None
                };
                if let Some(failure) = terminal_failure {
                    match cell.mark_failed(key, failure) {
                        Ok(()) | Err(ManagedCaptureCellLockError::WrongKey) => {
                            self.capture_request = None;
                        }
                        Err(ManagedCaptureCellLockError::Busy) => return,
                        Err(ManagedCaptureCellLockError::InvalidState) => {
                            self.capture_request = None;
                        }
                    }
                }
            }
        }
        if self.capture_request.is_some() {
            return;
        }
        match shared.try_take_request() {
            Ok(Some(request)) => self.capture_request = Some(request),
            Ok(None)
            | Err(ManagedCaptureEndpointError::Busy)
            | Err(ManagedCaptureEndpointError::Detached)
            | Err(ManagedCaptureEndpointError::Poisoned)
            | Err(ManagedCaptureEndpointError::AlreadyRegistered) => {}
        }
    }

    fn accept_managed_stage(
        &mut self,
        key: ManagedStageKey,
        input: PacketStageInput,
        limits: PacketStageLimits,
        event_loop: &ActiveEventLoop,
    ) {
        let Some(managed) = self.managed.as_ref() else {
            return;
        };
        let is_deferred_retry = managed.deferred_key == Some(key);
        let initial_retry_valid = key == ManagedStageKey::INITIAL
            && is_deferred_retry
            && managed.active.is_none()
            && input.session() == managed.initial.session()
            && input.project_generation() == managed.initial.project_generation()
            && input.source_digest() == managed.initial.source_digest();
        let candidate_identity_valid = if key == ManagedStageKey::INITIAL {
            false
        } else if let Some(active) = managed.active.as_ref() {
            active.identity.project_generation().checked_add(1) == Some(input.project_generation())
                && input.session() == managed.initial.session()
                && valid_source_digest(input.source_digest())
        } else {
            false
        };
        let key_valid = if is_deferred_retry {
            managed.deferred_key == Some(key)
        } else {
            key > managed.highest_key
        };
        let identity_valid = (initial_retry_valid || candidate_identity_valid) && key_valid;
        if !identity_valid {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Identity,
                    "candidate stage key or project identity is invalid",
                ),
            );
            return;
        }
        if managed.server.shared.cancel_watermark() >= key.get() {
            if let Some(managed) = &mut self.managed {
                managed.deferred_key = None;
                if key > managed.highest_key {
                    managed.highest_key = key;
                }
            }
            self.record_managed_stage_event(ManagedStageEvent::Cancelled { key });
            return;
        }
        if let Some(stage) = self.managed.as_ref().and_then(|state| state.stage.as_ref()) {
            let _ = stage;
            if let Some(managed) = &mut self.managed {
                managed.deferred_key = Some(key);
            }
            self.record_managed_stage_event(ManagedStageEvent::Deferred {
                key,
                reason: ManagedStageDeferredReason::RendererBusy,
            });
            return;
        }
        let identity = ManagedGenerationIdentity::new(
            key,
            input.session(),
            input.project_generation(),
            input.source_digest(),
        );
        let result = self
            .gpu_owner
            .as_mut()
            .and_then(WindowGpuOwner::packet_renderer_mut)
            .ok_or(PacketStageError::InvalidLimits)
            .and_then(|renderer| renderer.try_begin_stage(input, limits));
        match result {
            Ok(id) => {
                let managed = self.managed.as_mut().expect("managed mode has state");
                managed.highest_key = key;
                managed.deferred_key = None;
                managed.stage = Some(ManagedStage {
                    key,
                    identity,
                    limits,
                    input: None,
                    id: Some(id),
                    frame: None,
                    event_published: false,
                    cancellation_requested: false,
                    last_status: None,
                });
            }
            Err(PacketStageError::StageBusy) => {
                if let Some(managed) = &mut self.managed {
                    managed.deferred_key = Some(key);
                }
                self.record_managed_stage_event(ManagedStageEvent::Deferred {
                    key,
                    reason: ManagedStageDeferredReason::RendererBusy,
                });
            }
            Err(error) => {
                if let Some(managed) = &mut self.managed {
                    managed.highest_key = key;
                    managed.deferred_key = None;
                }
                self.record_managed_stage_event(ManagedStageEvent::Failed {
                    key,
                    error: ManagedWindowError::new(
                        if matches!(error, PacketStageError::CounterExhausted) {
                            ManagedWindowErrorCode::CounterExhausted
                        } else {
                            ManagedWindowErrorCode::Stage
                        },
                        error.to_string(),
                    ),
                });
            }
        }
    }

    fn accept_managed_frame(
        &mut self,
        key: ManagedStageKey,
        frame: Arc<CpuFrameHandle>,
        event_loop: &ActiveEventLoop,
    ) {
        let outcome = self.managed.as_mut().map(|managed| {
            let watermark = managed.server.shared.cancel_watermark();
            update_managed_frame(managed, key, frame, watermark)
        });
        if let Some(Err(message)) = outcome {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(ManagedWindowErrorCode::Identity, message),
            );
        }
    }

    fn record_managed_stage_progress(&mut self, status: PacketStageStatus) {
        let Some(managed) = self.managed.as_mut() else {
            return;
        };
        let Some(stage) = managed.stage.as_ref() else {
            return;
        };
        if stage.id != Some(status.id()) {
            return;
        }
        let key = stage.key;
        let event_published = stage.event_published;
        if let Some(stage) = managed.stage.as_mut() {
            stage.last_status = Some(status.clone());
        }
        match status.state() {
            PacketStageState::Ready => {
                let cancelled = managed.server.shared.cancel_watermark() >= key.get();
                if !cancelled && !event_published && managed.pending_event.is_none() {
                    managed.pending_event = Some(ManagedStageEvent::Ready {
                        key,
                        stage_id: status.id(),
                        status,
                    });
                }
            }
            PacketStageState::Cancelled => {
                managed.stage = None;
                if !event_published {
                    let replaces_unpublished_ready = matches!(
                        managed.pending_event.as_ref(),
                        Some(ManagedStageEvent::Ready { key: pending_key, .. })
                            if *pending_key == key
                    );
                    if managed.pending_event.is_none() || replaces_unpublished_ready {
                        managed.pending_event = Some(ManagedStageEvent::Cancelled { key });
                    }
                }
            }
            PacketStageState::Failed => {
                let error = status.failure().map_or_else(
                    || ManagedWindowError::new(ManagedWindowErrorCode::Stage, "GPU stage failed"),
                    managed::map_packet_stage_failure,
                );
                managed.stage = None;
                if !event_published {
                    let replaces_unpublished_ready = matches!(
                        managed.pending_event.as_ref(),
                        Some(ManagedStageEvent::Ready { key: pending_key, .. })
                            if *pending_key == key
                    );
                    if managed.pending_event.is_none() || replaces_unpublished_ready {
                        managed.pending_event = Some(ManagedStageEvent::Failed { key, error });
                    }
                }
            }
            _ => {}
        }
    }

    fn redraw_managed(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_managed_inputs(event_loop);
        if self.managed_failure.is_some() || self.exit_reason.is_some() {
            return;
        }
        let now = Instant::now();
        self.update_local_camera(now);
        let Some(window) = self.window.as_ref().map(Arc::clone) else {
            return;
        };
        let Some(surface_state) = self.surface.as_ref() else {
            return;
        };
        let width = surface_state.config.width;
        let height = surface_state.config.height;
        if width == 0 || height == 0 {
            return;
        }

        let (active_frame, active_key, candidate_frame, candidate_identity, stage_id) = {
            let Some(managed) = self.managed.as_ref() else {
                return;
            };
            let candidate = managed.stage.as_ref().and_then(|stage| {
                (!stage.cancellation_requested
                    && stage.event_published
                    && stage
                        .last_status
                        .as_ref()
                        .is_some_and(|status| status.state() == PacketStageState::Ready))
                .then(|| stage.frame.clone())
                .flatten()
            });
            (
                managed.active.as_ref().map(|active| active.frame.clone()),
                managed.active.as_ref().map(|active| active.key),
                candidate,
                managed
                    .stage
                    .as_ref()
                    .and_then(|stage| stage.frame.as_ref().map(|_| stage.identity.clone())),
                managed.stage.as_ref().and_then(|stage| {
                    (!stage.cancellation_requested)
                        .then_some(stage.id)
                        .flatten()
                }),
            )
        };

        let mut chosen_is_candidate = candidate_frame.is_some();
        let mut chosen_frame = candidate_frame.or(active_frame.clone());
        let mut chosen_key = if chosen_is_candidate {
            candidate_identity
                .as_ref()
                .map(ManagedGenerationIdentity::key)
        } else {
            active_key
        };
        let shared = self
            .managed
            .as_ref()
            .map(|managed| Arc::clone(&managed.server.shared));
        let mut present_guard = None;
        if let Some(frame) = chosen_frame.as_ref() {
            let Some(shared) = shared.as_ref() else {
                return;
            };
            match shared.try_reserve_presented(frame.project_generation()) {
                Ok(guard) => present_guard = Some(guard),
                Err(ManagedPresentedReservationError::Busy) => return,
                Err(ManagedPresentedReservationError::PreviousGenerationUnapplied)
                    if chosen_is_candidate =>
                {
                    chosen_frame = active_frame.clone();
                    chosen_is_candidate = false;
                    chosen_key = active_key;
                    let Some(active) = chosen_frame.as_ref() else {
                        return;
                    };
                    match shared.try_reserve_presented(active.project_generation()) {
                        Ok(guard) => present_guard = Some(guard),
                        Err(_) => return,
                    }
                }
                Err(ManagedPresentedReservationError::PreviousGenerationUnapplied) => return,
            }
        }

        let chosen_view = chosen_frame.as_ref().map(|frame| {
            self.local_view
                .clone()
                .unwrap_or_else(|| frame.view().clone())
        });
        let mut intended_present = None;
        if let (Some(frame), Some(view), Some(key)) =
            (chosen_frame.as_ref(), chosen_view.as_ref(), chosen_key)
        {
            let next_present_sequence = self
                .managed
                .as_ref()
                .and_then(|managed| managed.present_sequence.checked_add(1));
            let Some(next_present_sequence) = next_present_sequence else {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(
                        ManagedWindowErrorCode::CounterExhausted,
                        "present sequence exhausted",
                    ),
                );
                return;
            };
            match ManagedPresentedView::new(
                key,
                frame.packet(),
                next_present_sequence,
                width,
                height,
                view,
            ) {
                Ok(presented) => intended_present = Some(presented),
                Err(error) => {
                    self.fail_managed(event_loop, error);
                    return;
                }
            }
        } else if chosen_frame.is_some() {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Identity,
                    "selected frame has no managed generation key",
                ),
            );
            return;
        }

        if chosen_is_candidate
            && intended_present.as_ref().is_some_and(|presented| {
                shared
                    .as_ref()
                    .is_some_and(|shared| shared.cancel_watermark() >= presented.key().get())
            })
        {
            return;
        }

        let loading_elapsed = self
            .loading_since
            .map(|start| now.saturating_duration_since(start).as_secs_f32());
        let surface_result = self
            .surface
            .as_mut()
            .map(|surface| surface.surface.get_current_texture());
        let surface_frame = match surface_result {
            Some(Ok(frame)) => frame,
            Some(Err(wgpu::SurfaceError::Timeout)) => return,
            Some(Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost)) => {
                let device = self.gpu_owner.as_mut().and_then(|owner| {
                    if owner.phase() == WindowGpuOwnerPhase::PacketRendererReady {
                        owner
                            .packet_renderer_mut()
                            .map(|renderer| renderer.device())
                    } else {
                        owner.renderer().map(|renderer| &renderer.device)
                    }
                });
                if let (Some(device), Some(surface)) = (device, self.surface.as_mut()) {
                    surface.surface.configure(device, &surface.config);
                }
                return;
            }
            Some(Err(wgpu::SurfaceError::OutOfMemory)) => {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(
                        ManagedWindowErrorCode::Renderer,
                        "surface ran out of memory",
                    ),
                );
                return;
            }
            Some(Err(error)) => {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string()),
                );
                return;
            }
            None => return,
        };
        let target = surface_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let render_view = chosen_view.unwrap_or_else(|| self.current_view());
        let render_frame = chosen_frame.clone();
        let renderer_ready = self
            .gpu_owner
            .as_ref()
            .is_some_and(|owner| owner.phase() == WindowGpuOwnerPhase::PacketRendererReady);
        if !renderer_ready {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Renderer,
                    "packet renderer is unavailable during redraw",
                ),
            );
            return;
        }
        let submission = {
            let renderer = self
                .gpu_owner
                .as_mut()
                .and_then(WindowGpuOwner::packet_renderer_mut)
                .expect("readiness was checked before renderer use");
            renderer.render_cpu_frame_with_stage(
                &target,
                width,
                height,
                render_frame,
                &render_view,
                stage_id,
                loading_elapsed,
            )
        };
        let submission = match submission {
            Ok(submission) => submission,
            Err(PacketStageError::Render(PacketRenderError::ResidencyBusy)) => return,
            Err(error @ PacketStageError::Render(PacketRenderError::Renderer(_))) => {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string()),
                );
                return;
            }
            Err(error) => {
                if let Some(stage_id) = stage_id {
                    if let Some(status) = self
                        .gpu_owner
                        .as_mut()
                        .and_then(WindowGpuOwner::packet_renderer_mut)
                        .and_then(|renderer| renderer.try_cancel_stage(stage_id).ok())
                        && let Some(stage) = self
                            .managed
                            .as_mut()
                            .and_then(|managed| managed.stage.as_mut())
                    {
                        stage.last_status = Some(status);
                    }
                    let key = self
                        .managed
                        .as_ref()
                        .and_then(|managed| managed.stage.as_ref())
                        .map(|stage| stage.key)
                        .unwrap_or(ManagedStageKey::INITIAL);
                    self.record_managed_stage_event(ManagedStageEvent::Failed {
                        key,
                        error: ManagedWindowError::new(
                            if matches!(error, PacketStageError::CounterExhausted) {
                                ManagedWindowErrorCode::CounterExhausted
                            } else {
                                ManagedWindowErrorCode::Stage
                            },
                            error.to_string(),
                        ),
                    });
                    return;
                }
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(
                        if matches!(
                            error,
                            PacketStageError::FrameIdentityMismatch
                                | PacketStageError::FrameResourcesUnavailable
                        ) {
                            ManagedWindowErrorCode::Identity
                        } else {
                            ManagedWindowErrorCode::Packet
                        },
                        error.to_string(),
                    ),
                );
                return;
            }
        };

        if let Some(progress) = submission.progress().cloned() {
            self.record_managed_stage_progress(progress);
            self.publish_managed_event();
        }
        let presented_submission = submission.frame();
        if intended_present.is_some() && presented_submission.is_none() {
            self.fail_managed(
                event_loop,
                ManagedWindowError::new(
                    ManagedWindowErrorCode::Identity,
                    "game frame submission produced no packet view",
                ),
            );
            return;
        }
        if let (Some(presented), Some(submitted)) =
            (intended_present.as_ref(), presented_submission)
        {
            let packet = submitted.packet();
            if submitted.view() != presented.view()
                || packet.session() != presented.session()
                || packet.sequence() != presented.publication_sequence()
                || packet.tick() != presented.tick()
                || packet.authority_snapshot_sequence() != presented.authority_snapshot_sequence()
                || packet.project_generation() != presented.project_generation()
                || packet.source_digest() != presented.source_digest()
            {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(
                        ManagedWindowErrorCode::Identity,
                        "submitted frame identity changed after managed preflight",
                    ),
                );
                return;
            }
        }
        let selected_frame = chosen_frame.as_ref().filter(|frame| {
            presented_submission
                .is_some_and(|submitted| std::ptr::eq(frame.packet(), submitted.packet().as_ref()))
        });

        let capture_shared = self
            .managed
            .as_ref()
            .and_then(|managed| managed.server.capture.as_ref())
            .cloned();
        let capture_cell = capture_shared.as_ref().map(|shared| shared.cell());
        let mut capture_guard = None;
        let mut capture_request_consumed = false;
        if let (Some(request), Some(presented)) =
            (self.capture_request.as_ref(), intended_present.as_ref())
            && let (Some(capture_shared), Some(cell)) =
                (capture_shared.as_ref(), capture_cell.as_ref())
        {
            let failure = if now >= request.deadline() {
                Some(ManagedCaptureFailure::DeadlineElapsed)
            } else if cell.is_cancelled(request.key()) {
                Some(ManagedCaptureFailure::Cancelled)
            } else if request.source().session() != presented.session() {
                Some(ManagedCaptureFailure::StaleSource)
            } else if request.source().project_generation() > presented.project_generation() {
                None
            } else if request.source().project_generation() < presented.project_generation()
                || !request.source().matches_view(presented)
            {
                Some(ManagedCaptureFailure::StaleSource)
            } else if presented.tick() < request.at_least_tick() {
                None
            } else if selected_frame.is_none() {
                Some(ManagedCaptureFailure::StaleSource)
            } else {
                match capture_shared.state() {
                    ManagedCaptureState::Ready(info) => {
                        match cell.try_lock_for_ui(request.key()) {
                            Ok(mut guard) => {
                                let result = self
                                    .gpu_owner
                                    .as_mut()
                                    .and_then(WindowGpuOwner::packet_renderer_mut)
                                    .ok_or(ManagedCaptureFailure::CopyFailed)
                                    .and_then(|renderer| {
                                        renderer.try_submit_managed_capture_copy(
                                            &surface_frame.texture,
                                            width,
                                            height,
                                            info,
                                            info.surface_format(),
                                            selected_frame.expect("eligible selected frame"),
                                            cell,
                                            &mut guard,
                                        )
                                    });
                                match result {
                                    Ok(true) => capture_guard = Some(guard),
                                    Ok(false) => guard.disarm_cleanup_on_drop(),
                                    Err(failure) if guard.copy_possible() => {
                                        let _ = failure;
                                        capture_request_consumed = true;
                                    }
                                    Err(failure) => {
                                        guard.mark_failed(failure);
                                        guard.disarm_cleanup_on_drop();
                                        capture_request_consumed = true;
                                    }
                                }
                            }
                            Err(ManagedCaptureCellLockError::Busy) => {}
                            Err(ManagedCaptureCellLockError::WrongKey)
                            | Err(ManagedCaptureCellLockError::InvalidState) => {
                                capture_request_consumed = true;
                            }
                        }
                        None
                    }
                    ManagedCaptureState::Unsupported(_) => Some(ManagedCaptureFailure::Unsupported),
                    ManagedCaptureState::Starting | ManagedCaptureState::Detached => None,
                }
            };
            if let Some(failure) = failure
                && cell.mark_failed(request.key(), failure).is_ok()
            {
                capture_request_consumed = true;
            }
        }

        window.pre_present_notify();
        surface_frame.present();
        if let Some(mut guard) = capture_guard {
            if let Some(presented) = intended_present.as_ref() {
                if guard.mark_submitted(guard.key(), presented.clone()).is_ok() {
                    guard.disarm_cleanup_on_drop();
                } else {
                    guard.mark_cleanup(Some(ManagedCaptureFailure::CopyFailed));
                    capture_request_consumed = true;
                }
            } else {
                guard.mark_cleanup(Some(ManagedCaptureFailure::CopyFailed));
                capture_request_consumed = true;
            }
            if !capture_request_consumed {
                capture_request_consumed = true;
            }
        }
        if capture_request_consumed {
            self.capture_request = None;
        }
        if let Some(presented) = intended_present {
            if let Some(guard) = &mut present_guard {
                guard.publish(presented.clone());
            }
            if let Some(managed) = &mut self.managed {
                managed.present_sequence = presented.present_sequence();
                managed.last_presented = Some(presented.clone());
                if chosen_is_candidate {
                    if let (Some(frame), Some(identity)) = (chosen_frame.take(), candidate_identity)
                    {
                        managed.active = Some(ManagedFrame {
                            key: identity.key(),
                            identity,
                            frame,
                        });
                        managed.stage = None;
                    }
                } else if let (Some(frame), Some(active)) =
                    (chosen_frame.take(), managed.active.as_mut())
                {
                    active.frame = frame;
                }
            }
            self.latest_presentation = Some(PresentationMetadata {
                sequence: presented.publication_sequence(),
                tick: presented.tick().get(),
                generation: presented.project_generation(),
            });
            self.set_loading(false);
        } else if let Some(managed) = &mut self.managed {
            managed.bootstrap_frames = managed.bootstrap_frames.saturating_add(1);
        }
        self.counters.rendered_frames = self.counters.rendered_frames.saturating_add(1);
        if self.is_loading() {
            self.counters.loading_frames = self.counters.loading_frames.saturating_add(1);
        }
        self.intervals.record_present(Instant::now());
        let status = self.managed_status(ManagedWindowLifecycle::Running);
        if let Some(managed) = &self.managed {
            let _ = managed.server.shared.try_publish_status(status);
        }
        if max_frames_reached(self.counters.rendered_frames, self.options.max_frames) {
            self.exit_managed(event_loop, ManagedWindowExitReason::FrameLimit);
        }
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), WindowFailure> {
        let attributes = WindowAttributes::default()
            .with_title(self.options.title.clone())
            .with_inner_size(LogicalSize::new(self.options.width, self.options.height))
            .with_active(self.input_policy == WindowInputPolicy::Native);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| RendererError::Window(error.to_string()))?,
        );
        let size = window.inner_size();
        self.final_width = size.width;
        self.final_height = size.height;
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| RendererError::Window(error.to_string()))?;
        let capabilities = pollster::block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: Some(&surface),
                })
                .await
                .map_err(|error| RendererError::NoAdapter(error.to_string()))?;
            let capabilities = surface.get_capabilities(&adapter);
            Ok::<_, RendererError>((adapter, capabilities))
        })?;
        let (adapter, capabilities) = capabilities;
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| RendererError::Window("surface exposes no color format".into()))?;
        let present_mode = if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::AutoVsync)
        {
            wgpu::PresentMode::AutoVsync
        } else if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Fifo)
        {
            wgpu::PresentMode::Fifo
        } else {
            *capabilities.present_modes.first().ok_or_else(|| {
                RendererError::Window("surface exposes no presentation mode".into())
            })?
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: preferred_surface_alpha_mode(&capabilities.alpha_modes),
            view_formats: vec![],
        };
        let renderer = pollster::block_on(Renderer::from_adapter(instance, adapter, format))?;
        renderer.validate_dimensions(config.width, config.height)?;
        surface.configure(&renderer.device, &config);
        self.renderer_info = Some(renderer.info().clone());
        self.window = Some(window);
        if self.packet.is_some() {
            self.packet_renderer = Some(
                renderer
                    .into_packet_renderer_with_asset_limits(self.packet_limits, self.asset_limits)
                    .map_err(|error| WindowFailure::Packet(PacketWindowError::Packet(error)))?,
            );
        } else {
            self.renderer = Some(renderer);
        }
        self.surface = Some(SurfaceState { surface, config });
        self.update_window_title();
        self.drain_messages(event_loop);
        Ok(())
    }

    fn drain_messages(&mut self, event_loop: &ActiveEventLoop) {
        for _ in 0..MAX_MESSAGES_PER_EVENT {
            let legacy_message = self.messages.as_ref().map(Receiver::try_recv);
            let packet_message = self.packet_messages.as_ref().map(Receiver::try_recv);
            if legacy_message.is_none() {
                match packet_message.expect("one message receiver is configured") {
                    Ok(PacketWindowMessage::Present { packet, loading }) => {
                        let previous = self
                            .packet
                            .as_ref()
                            .expect("packet app has an initial packet");
                        match validate_packet_transition(previous, &packet, self.packet_limits) {
                            Ok(PacketTransition::Idempotent) => self.set_loading(loading),
                            Ok(PacketTransition::New) => {
                                self.latest_presentation = Some(PresentationMetadata {
                                    sequence: packet.sequence(),
                                    tick: packet.tick().get(),
                                    generation: packet.project_generation(),
                                });
                                self.packet = Some(packet);
                                self.set_loading(loading);
                            }
                            Err(error) => {
                                self.fail_packet(event_loop, PacketWindowError::Packet(error));
                                return;
                            }
                        }
                    }
                    Ok(PacketWindowMessage::SetLoading(loading)) => self.set_loading(loading),
                    Ok(PacketWindowMessage::Close) => {
                        self.exit(event_loop, WindowExitReason::CloseRequested);
                        return;
                    }
                    Ok(PacketWindowMessage::Abort(message)) => {
                        self.fail_packet(
                            event_loop,
                            PacketWindowError::EventLoop(bounded_message(message)),
                        );
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.message_source_disconnected = true;
                        if let Some(error) = presentation_disconnect_error(
                            self.options.fail_on_presentation_disconnect,
                        ) {
                            self.fail(event_loop, error);
                            return;
                        }
                        break;
                    }
                }
                continue;
            }
            match legacy_message.expect("legacy receiver is configured") {
                Ok(WindowMessage::Present {
                    scene,
                    loading,
                    sequence,
                    tick,
                    generation,
                }) => {
                    let mut current_loading = self.is_loading();
                    if let Err(error) = apply_presentation(
                        &mut self.scene,
                        &mut current_loading,
                        &mut self.latest_presentation,
                        scene,
                        loading,
                        PresentationMetadata {
                            sequence,
                            tick,
                            generation,
                        },
                        self.local_camera.as_ref(),
                    ) {
                        self.fail(event_loop, error);
                        return;
                    }
                    self.set_loading(current_loading);
                }
                Ok(WindowMessage::ReplaceScene(mut scene)) => {
                    if let Err(error) = validate_window_scene(&scene) {
                        self.fail(event_loop, error);
                        return;
                    }
                    if let Some(camera) = &self.local_camera {
                        scene.camera = camera.clone();
                    }
                    self.scene = scene;
                }
                Ok(WindowMessage::SetLoading(loading)) => self.set_loading(loading),
                Ok(WindowMessage::Abort(message)) => {
                    self.fail(event_loop, RendererError::Window(bounded_message(message)));
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.message_source_disconnected = true;
                    if let Some(error) =
                        presentation_disconnect_error(self.options.fail_on_presentation_disconnect)
                    {
                        self.fail(event_loop, error);
                        return;
                    }
                    break;
                }
            }
        }
    }

    fn update_local_camera(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_update)
            .as_secs_f32()
            .min(0.05);
        self.last_update = now;
        if elapsed <= 0.0 {
            return;
        }
        self.apply_camera_movement(elapsed);
    }

    fn apply_camera_movement(&mut self, elapsed: f32) {
        if elapsed <= 0.0 || !self.input_policy.allows_local_camera() {
            return;
        }
        if !managed_view_input_enabled(
            self.mode,
            self.managed
                .as_ref()
                .is_some_and(|managed| managed.active.is_some()),
        ) {
            // Keep observing input during bootstrap, but do not replace the
            // first authored view before its first frame is actually presented.
            return;
        }
        let current_view = self.current_view();
        let mut camera = current_view.camera().clone();
        let forward = (Vec3::from_array(camera.target) - Vec3::from_array(camera.position))
            .normalize_or_zero();
        let camera_up = Vec3::from_array(camera.up).normalize_or_zero();
        let right = forward.cross(camera_up).normalize_or_zero();
        let mut movement = Vec3::ZERO;
        if matches!(
            current_view.projection(),
            crate::RenderProjection::Orthographic { .. }
        ) {
            if self.keys_down.contains(&KeyCode::KeyW) {
                movement += camera_up;
            }
            if self.keys_down.contains(&KeyCode::KeyS) {
                movement -= camera_up;
            }
            if self.keys_down.contains(&KeyCode::KeyD) {
                movement += right;
            }
            if self.keys_down.contains(&KeyCode::KeyA) {
                movement -= right;
            }
        } else {
            let forward_flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
            let legacy_right = forward_flat.cross(Vec3::Y).normalize_or_zero();
            if self.keys_down.contains(&KeyCode::KeyW) {
                movement += forward_flat;
            }
            if self.keys_down.contains(&KeyCode::KeyS) {
                movement -= forward_flat;
            }
            if self.keys_down.contains(&KeyCode::KeyD) {
                movement += legacy_right;
            }
            if self.keys_down.contains(&KeyCode::KeyA) {
                movement -= legacy_right;
            }
            if self.keys_down.contains(&KeyCode::KeyE) {
                movement += Vec3::Y;
            }
            if self.keys_down.contains(&KeyCode::KeyQ) {
                movement -= Vec3::Y;
            }
        }
        if movement.length_squared() == 0.0 {
            return;
        }
        let previous = camera.clone();
        move_camera(&mut camera, movement, elapsed);
        if camera != previous {
            let updated = current_view.with_camera(camera.clone());
            if self.is_packet_path() {
                self.local_view = Some(updated);
            } else {
                self.local_camera = Some(camera);
            }
            self.sync_legacy_camera();
            self.counters.local_camera_changes =
                self.counters.local_camera_changes.saturating_add(1);
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.mode == WindowMode::Managed {
            self.redraw_managed(event_loop);
            return;
        }
        self.drain_messages(event_loop);
        if self.failure.is_some() {
            return;
        }
        let now = Instant::now();
        self.update_local_camera(now);
        let Some(window) = &self.window else {
            return;
        };
        let Some(surface_state) = &mut self.surface else {
            return;
        };
        if surface_state.config.width == 0 || surface_state.config.height == 0 {
            return;
        }
        let width = surface_state.config.width;
        let height = surface_state.config.height;
        let frame = match surface_state.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                let device = self
                    .renderer
                    .as_ref()
                    .map(|renderer| &renderer.device)
                    .or_else(|| self.packet_renderer.as_ref().map(PacketRenderer::device));
                if let Some(device) = device {
                    surface_state
                        .surface
                        .configure(device, &surface_state.config);
                }
                return;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                self.fail(event_loop, RendererError::SurfaceOutOfMemory);
                return;
            }
            Err(error) => {
                self.fail(event_loop, RendererError::Window(error.to_string()));
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let loading_elapsed = self
            .loading_since
            .map(|start| now.saturating_duration_since(start).as_secs_f32());
        let rendered_submission = if let Some(packet) = self.packet.as_ref() {
            let Some(renderer) = self.packet_renderer.as_mut() else {
                return;
            };
            match renderer.render_packet_view_with_view(
                &view,
                width,
                height,
                Arc::clone(packet),
                self.local_view.as_ref(),
                loading_elapsed,
            ) {
                Ok(submission) => Some(submission),
                Err(PacketRenderError::ResidencyBusy) => {
                    // The acquired surface texture is deliberately dropped without
                    // presenting or advancing presented-packet metadata.
                    return;
                }
                Err(error) => {
                    self.fail_packet(event_loop, PacketWindowError::Packet(error));
                    return;
                }
            }
        } else {
            let Some(renderer) = self.renderer.as_ref() else {
                return;
            };
            if let Err(error) =
                renderer.render_to_view(&view, width, height, &self.scene, loading_elapsed)
            {
                self.fail(event_loop, error);
                return;
            }
            None
        };
        window.pre_present_notify();
        frame.present();
        if let Some(submission) = rendered_submission {
            let packet = submission.packet();
            let camera = submission.view().camera().clone();
            let presented = PresentedPacket {
                session: packet.session(),
                tick: packet.tick(),
                sequence: packet.sequence(),
                project_generation: packet.project_generation(),
                source_digest: packet.source_digest().to_owned(),
                camera,
                physical_width: width,
                physical_height: height,
            };
            self.last_presented = Some(presented.clone());
            self.last_presented_view = Some(PresentedView {
                packet: presented,
                view: submission.view().clone(),
            });
        }
        self.counters.rendered_frames = self.counters.rendered_frames.saturating_add(1);
        if self.is_loading() {
            self.counters.loading_frames = self.counters.loading_frames.saturating_add(1);
        }
        self.intervals.record_present(Instant::now());
        if max_frames_reached(self.counters.rendered_frames, self.options.max_frames) {
            self.exit(event_loop, WindowExitReason::FrameLimit);
        }
    }

    fn emit_input(&mut self, event: WindowInput) {
        self.record_input(event, true);
    }

    fn emit_native_control(&mut self, event: WindowInput) {
        self.record_input(event, self.input_policy.forwards_native_controls());
    }

    fn process_native_key(&mut self, code: KeyCode, pressed: bool) -> bool {
        if !self.input_policy.forwards_native_controls() {
            self.record_input(WindowInput::Key { code, pressed }, false);
            return false;
        }
        if pressed {
            self.keys_down.insert(code);
        } else {
            self.keys_down.remove(&code);
        }
        self.emit_input(WindowInput::Key { code, pressed });
        true
    }

    fn record_input(&mut self, event: WindowInput, forward_to_observer: bool) {
        self.counters.input_events = self.counters.input_events.saturating_add(1);
        let while_loading = self.is_loading();
        if while_loading {
            self.counters.loading_input_events =
                self.counters.loading_input_events.saturating_add(1);
        }
        let (events, loading_events) = match &event {
            WindowInput::Key { .. } => (
                &mut self.counters.key_events,
                &mut self.counters.loading_key_events,
            ),
            WindowInput::MouseButton { .. } => (
                &mut self.counters.mouse_button_events,
                &mut self.counters.loading_mouse_button_events,
            ),
            WindowInput::CursorMoved { .. } => (
                &mut self.counters.cursor_events,
                &mut self.counters.loading_cursor_events,
            ),
            WindowInput::MouseWheel { .. } => (
                &mut self.counters.wheel_events,
                &mut self.counters.loading_wheel_events,
            ),
            WindowInput::Resized { .. } => (
                &mut self.counters.resize_events,
                &mut self.counters.loading_resize_events,
            ),
            WindowInput::Focused(_) => (
                &mut self.counters.focus_events,
                &mut self.counters.loading_focus_events,
            ),
        };
        *events = events.saturating_add(1);
        if while_loading {
            *loading_events = loading_events.saturating_add(1);
        }
        if forward_to_observer && let Some(sender) = &self.options.input_events {
            sender.try_send(event);
        }
    }

    fn make_report(&self) -> WindowReport {
        let final_loading = self.is_loading();
        WindowReport {
            rendered_frames: self.counters.rendered_frames,
            loading_frames: self.counters.loading_frames,
            input_events: self.counters.input_events,
            loading_input_events: self.counters.loading_input_events,
            key_events: self.counters.key_events,
            loading_key_events: self.counters.loading_key_events,
            mouse_button_events: self.counters.mouse_button_events,
            loading_mouse_button_events: self.counters.loading_mouse_button_events,
            cursor_events: self.counters.cursor_events,
            loading_cursor_events: self.counters.loading_cursor_events,
            wheel_events: self.counters.wheel_events,
            loading_wheel_events: self.counters.loading_wheel_events,
            resize_events: self.counters.resize_events,
            loading_resize_events: self.counters.loading_resize_events,
            focus_events: self.counters.focus_events,
            loading_focus_events: self.counters.loading_focus_events,
            local_camera_changes: self.counters.local_camera_changes,
            final_camera: self.current_camera().clone(),
            resize_count: self.counters.resize_count,
            final_width: self.final_width,
            final_height: self.final_height,
            frame_intervals: self.intervals.report(),
            exit_reason: self
                .exit_reason
                .unwrap_or(WindowExitReason::EventLoopReturned),
            renderer: self.renderer_info.clone(),
            message_source_disconnected: self.message_source_disconnected,
            final_loading,
            latest_presentation: self.latest_presentation.clone(),
            window_title: self.window_title.clone(),
        }
    }

    fn into_outcome(self) -> WindowOutcome {
        let report = self.make_report();
        WindowOutcome {
            report,
            final_scene: self.scene,
        }
    }

    fn into_packet_outcome(self) -> PacketWindowOutcome {
        let report = self.make_report();
        let resource_stats = self
            .packet_renderer
            .as_ref()
            .map_or_else(PacketResourceStats::default, PacketRenderer::resource_stats);
        PacketWindowOutcome {
            report,
            final_packet: self
                .packet
                .expect("packet app retains its latest accepted packet"),
            last_presented: self.last_presented,
            resource_stats,
        }
    }
}

fn preferred_surface_alpha_mode(
    supported: &[wgpu::CompositeAlphaMode],
) -> wgpu::CompositeAlphaMode {
    if supported.contains(&wgpu::CompositeAlphaMode::Opaque) {
        wgpu::CompositeAlphaMode::Opaque
    } else {
        supported
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto)
    }
}

impl ApplicationHandler for WindowApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if self.mode == WindowMode::Managed {
            if let Err(error) = self.initialize_managed(event_loop) {
                self.fail_managed(event_loop, error);
            }
            return;
        }
        if let Err(error) = self.initialize(event_loop) {
            self.failure = Some(error);
            event_loop.exit();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.id() != window_id)
        {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if self.mode == WindowMode::Managed {
                    self.exit_managed(event_loop, ManagedWindowExitReason::ViewClosed);
                } else {
                    self.exit(event_loop, WindowExitReason::CloseRequested);
                }
            }
            WindowEvent::Resized(size) => {
                self.counters.resize_count = self.counters.resize_count.saturating_add(1);
                self.final_width = size.width;
                self.final_height = size.height;
                if size.width > 0 && size.height > 0 {
                    if let Err(error) = self.validate_current_dimensions(size.width, size.height) {
                        self.fail(event_loop, error);
                        return;
                    }
                    let device = self
                        .renderer
                        .as_ref()
                        .map(|renderer| &renderer.device)
                        .or_else(|| self.packet_renderer.as_ref().map(PacketRenderer::device))
                        .or_else(|| {
                            self.gpu_owner.as_mut().and_then(|owner| {
                                if owner.phase() == WindowGpuOwnerPhase::PacketRendererReady {
                                    owner
                                        .packet_renderer_mut()
                                        .map(|renderer| renderer.device())
                                } else {
                                    owner.renderer().map(|renderer| &renderer.device)
                                }
                            })
                        });
                    if let (Some(device), Some(surface_state)) = (device, &mut self.surface) {
                        surface_state.config.width = size.width;
                        surface_state.config.height = size.height;
                        surface_state
                            .surface
                            .configure(device, &surface_state.config);
                    }
                }
                self.emit_input(WindowInput::Resized {
                    width: size.width,
                    height: size.height,
                });
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    self.left_mouse_down = false;
                    self.keys_down.clear();
                    self.last_cursor = None;
                }
                self.emit_input(WindowInput::Focused(focused));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                if !self.process_native_key(code, pressed) {
                    return;
                }
                if pressed && is_camera_key(code) {
                    // Apply a small immediate step so a short press/release
                    // between redraws is visible in the final camera report.
                    let now = Instant::now();
                    self.apply_camera_movement(
                        self.options
                            .frame_interval
                            .as_secs_f32()
                            .clamp(1.0 / 120.0, 0.05),
                    );
                    self.last_update = now;
                }
                if code == KeyCode::Escape && pressed {
                    if self.mode == WindowMode::Managed {
                        self.exit_managed(event_loop, ManagedWindowExitReason::EscapePressed);
                    } else {
                        self.exit(event_loop, WindowExitReason::EscapePressed);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if button == MouseButton::Left {
                    self.left_mouse_down = pressed;
                }
                self.emit_native_control(WindowInput::MouseButton { button, pressed });
            }
            WindowEvent::CursorMoved { position, .. } => {
                if !self.input_policy.allows_local_camera()
                    || !managed_view_input_enabled(
                        self.mode,
                        self.managed
                            .as_ref()
                            .is_some_and(|managed| managed.active.is_some()),
                    )
                {
                    self.last_cursor = Some(position);
                    self.emit_native_control(WindowInput::CursorMoved {
                        x: position.x,
                        y: position.y,
                    });
                    return;
                }
                if self.left_mouse_down
                    && let Some(previous) = self.last_cursor
                {
                    let dx = (position.x - previous.x) as f32;
                    let dy = (position.y - previous.y) as f32;
                    let view = self.current_view();
                    let mut camera = view.camera().clone();
                    if let crate::RenderProjection::Orthographic { vertical_size } =
                        view.projection()
                    {
                        let height = self
                            .surface
                            .as_ref()
                            .map_or(self.final_height.max(1), |surface| {
                                surface.config.height.max(1)
                            });
                        let forward = (Vec3::from_array(camera.target)
                            - Vec3::from_array(camera.position))
                        .normalize_or_zero();
                        let up = Vec3::from_array(camera.up).normalize_or_zero();
                        let right = forward.cross(up).normalize_or_zero();
                        let delta = (-right * dx + up * dy) * (vertical_size / height as f32);
                        camera.position = (Vec3::from_array(camera.position) + delta).to_array();
                        camera.target = (Vec3::from_array(camera.target) + delta).to_array();
                        if delta.length_squared() > 0.0 {
                            self.local_view = Some(view.with_camera(camera));
                            self.counters.local_camera_changes =
                                self.counters.local_camera_changes.saturating_add(1);
                        }
                    } else {
                        let previous_camera = camera.clone();
                        camera.orbit(-dx * 0.006, -dy * 0.006, 0.0);
                        if camera != previous_camera {
                            if self.is_packet_path() {
                                self.local_view = Some(view.with_camera(camera));
                            } else {
                                self.local_camera = Some(camera);
                            }
                            self.sync_legacy_camera();
                            self.counters.local_camera_changes =
                                self.counters.local_camera_changes.saturating_add(1);
                        }
                    }
                }
                self.last_cursor = Some(position);
                self.emit_native_control(WindowInput::CursorMoved {
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(position) => (position.y as f32) * 0.01,
                };
                if !self.input_policy.allows_local_camera()
                    || !managed_view_input_enabled(
                        self.mode,
                        self.managed
                            .as_ref()
                            .is_some_and(|managed| managed.active.is_some()),
                    )
                {
                    self.emit_native_control(WindowInput::MouseWheel { lines });
                    return;
                }
                let view = self.current_view();
                if let crate::RenderProjection::Orthographic { vertical_size } = view.projection() {
                    let zoom = (vertical_size * (-lines * 0.12).exp()).clamp(0.001, 1_000_000.0);
                    if zoom.is_finite() && zoom != vertical_size {
                        self.local_view =
                            Some(view.with_projection(crate::RenderProjection::Orthographic {
                                vertical_size: zoom,
                            }));
                        self.counters.local_camera_changes =
                            self.counters.local_camera_changes.saturating_add(1);
                    }
                } else {
                    let mut camera = view.camera().clone();
                    let previous_camera = camera.clone();
                    camera.orbit(0.0, 0.0, -lines * 0.12);
                    if camera != previous_camera {
                        if self.is_packet_path() {
                            self.local_view = Some(view.with_camera(camera));
                        } else {
                            self.local_camera = Some(camera);
                        }
                        self.sync_legacy_camera();
                        self.counters.local_camera_changes =
                            self.counters.local_camera_changes.saturating_add(1);
                    }
                }
                self.emit_native_control(WindowInput::MouseWheel { lines });
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.mode == WindowMode::Managed {
            self.drain_managed_inputs(event_loop);
            if self.managed_failure.is_some() || self.exit_reason.is_some() {
                return;
            }
            let capture_enabled = self
                .managed
                .as_ref()
                .is_some_and(|managed| managed.server.capture.is_some());
            if capture_enabled
                && let Some(renderer) = self
                    .gpu_owner
                    .as_mut()
                    .and_then(WindowGpuOwner::packet_renderer_mut)
                && let Err(error) = renderer.poll_managed_capture()
            {
                self.fail_managed(
                    event_loop,
                    ManagedWindowError::new(ManagedWindowErrorCode::Renderer, error.to_string()),
                );
                return;
            }
            let now = Instant::now();
            if max_run_time_reached(self.started_at, now, self.options.max_run_time) {
                self.exit_managed(event_loop, ManagedWindowExitReason::TimeLimit);
                return;
            }
            if now >= self.next_redraw {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                self.next_redraw = now + self.options.frame_interval;
            }
            let mut wait_until = self
                .options
                .max_run_time
                .map(|limit| self.started_at + limit)
                .map_or(self.next_redraw, |deadline| self.next_redraw.min(deadline));
            if capture_enabled {
                wait_until = managed_capture_wait_until(
                    now,
                    wait_until,
                    self.capture_request
                        .as_ref()
                        .map(|request| request.deadline()),
                );
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(wait_until));
            return;
        }
        self.drain_messages(event_loop);
        if self.failure.is_some() {
            return;
        }
        let now = Instant::now();
        if max_run_time_reached(self.started_at, now, self.options.max_run_time) {
            self.exit(event_loop, WindowExitReason::TimeLimit);
            return;
        }
        if now >= self.next_redraw {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            self.next_redraw = now + self.options.frame_interval;
        }
        let wait_until = self
            .options
            .max_run_time
            .map(|limit| self.started_at + limit)
            .map_or(self.next_redraw, |deadline| self.next_redraw.min(deadline));
        event_loop.set_control_flow(ControlFlow::WaitUntil(wait_until));
    }
}

fn bounded_message(mut message: String) -> String {
    const MAX_MESSAGE_BYTES: usize = 4 * 1024;
    if message.len() > MAX_MESSAGE_BYTES {
        let mut boundary = MAX_MESSAGE_BYTES;
        while !message.is_char_boundary(boundary) {
            boundary -= 1;
        }
        message.truncate(boundary);
        message.push_str("… [message truncated]");
    }
    message
}

fn window_controls_hint(packet_mode: bool, input_policy: WindowInputPolicy) -> &'static str {
    if input_policy == WindowInputPolicy::ProtocolOnly {
        return "Protocol controls · OS close";
    }
    if packet_mode {
        "WASD/QE + drag/wheel · Esc close"
    } else {
        "WASD/QE + drag/wheel · Arrows move · R regenerate · Esc close"
    }
}

fn is_camera_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::KeyW
            | KeyCode::KeyA
            | KeyCode::KeyS
            | KeyCode::KeyD
            | KeyCode::KeyQ
            | KeyCode::KeyE
    )
}

fn move_camera(camera: &mut Camera, movement: Vec3, elapsed: f32) {
    if movement.length_squared() > 0.0 && elapsed > 0.0 {
        let delta = movement.normalize() * (elapsed * 3.5);
        camera.position = (Vec3::from_array(camera.position) + delta).to_array();
        camera.target = (Vec3::from_array(camera.target) + delta).to_array();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, atomic::AtomicBool},
    };

    use daedalus_content::{
        ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache, ProjectManifest,
        prepare_project_with_assets,
    };
    use daedalus_core::{RenderSnapshot, SessionId, Tick};
    use glam::Mat4;
    use winit::keyboard::KeyCode;

    use crate::{
        Camera, CpuFrameHandle, DirectionalLight, Material, Mesh, PacketAssetLimits,
        PacketCpuLimits, PacketCpuOwner, PacketRendererLimits, PacketStageInput, RenderObject,
        RenderPacket, RenderProjection, RenderScene, RenderView, SourcePin, Vertex,
    };

    use super::{
        FRAME_INTERVAL_SAMPLE_CAPACITY, FrameIntervals, MANAGED_CAPTURE_POLL_INTERVAL,
        ManagedFrame, ManagedWindowState, PacketTransition, PresentationMetadata, WindowApp,
        WindowInput, WindowInputPolicy, WindowMode, WindowOptions, apply_presentation,
        managed_capture_wait_until, managed_frame_is_cancelled, managed_view_input_enabled,
        max_frames_reached, max_run_time_reached, preferred_surface_alpha_mode,
        presentation_disconnect_error, validate_options, validate_packet_transition,
        validate_window_scene, window_controls_hint, window_input_channel,
    };

    #[test]
    fn control_hint_describes_only_controls_available_in_each_source_mode() {
        assert_eq!(
            window_controls_hint(true, WindowInputPolicy::Native),
            "WASD/QE + drag/wheel · Esc close"
        );
        assert_eq!(
            window_controls_hint(false, WindowInputPolicy::Native),
            "WASD/QE + drag/wheel · Arrows move · R regenerate · Esc close"
        );
        assert_eq!(
            window_controls_hint(true, WindowInputPolicy::ProtocolOnly),
            "Protocol controls · OS close"
        );
    }

    #[test]
    fn protocol_only_records_native_controls_but_forwards_only_lifecycle_events() {
        let (sink, observed_events) = window_input_channel(8).expect("positive observer capacity");
        let (_messages, message_receiver) = std::sync::mpsc::channel();
        let options = WindowOptions {
            input_events: Some(sink),
            ..WindowOptions::default()
        };
        let mut app = WindowApp::new(RenderScene::new(), message_receiver, options);
        app.input_policy = WindowInputPolicy::ProtocolOnly;

        assert!(!app.process_native_key(KeyCode::ArrowUp, true));
        assert!(!app.process_native_key(KeyCode::Escape, true));
        assert!(app.keys_down.is_empty());
        app.emit_native_control(WindowInput::MouseButton {
            button: winit::event::MouseButton::Left,
            pressed: true,
        });
        app.emit_native_control(WindowInput::CursorMoved { x: 12.0, y: 24.0 });
        app.emit_native_control(WindowInput::MouseWheel { lines: 1.0 });
        app.emit_input(WindowInput::Focused(false));
        app.emit_input(WindowInput::Resized {
            width: 640,
            height: 480,
        });

        assert_eq!(app.counters.input_events, 7);
        assert_eq!(app.counters.key_events, 2);
        assert_eq!(app.counters.mouse_button_events, 1);
        assert_eq!(app.counters.cursor_events, 1);
        assert_eq!(app.counters.wheel_events, 1);
        assert_eq!(app.counters.focus_events, 1);
        assert_eq!(app.counters.resize_events, 1);
        assert_eq!(
            app.options
                .input_events
                .as_ref()
                .expect("observer configured")
                .dropped_event_count(),
            0
        );
        assert_eq!(
            observed_events.try_iter().collect::<Vec<_>>(),
            vec![
                WindowInput::Focused(false),
                WindowInput::Resized {
                    width: 640,
                    height: 480,
                },
            ]
        );
    }

    #[test]
    fn protocol_only_cannot_apply_held_native_camera_keys() {
        let (_messages, receiver) = std::sync::mpsc::channel();
        let mut app = WindowApp::new(RenderScene::new(), receiver, WindowOptions::default());
        app.input_policy = WindowInputPolicy::ProtocolOnly;
        assert!(!app.input_policy.allows_local_camera());
        let initial_camera = app.current_camera().clone();
        app.keys_down.insert(KeyCode::KeyW);
        app.apply_camera_movement(1.0 / 60.0);

        assert_eq!(app.current_camera(), &initial_camera);
        assert_eq!(app.counters.local_camera_changes, 0);
    }

    #[test]
    fn native_policy_remains_the_default_and_forwards_key_observation() {
        let (sink, observed_events) = window_input_channel(1).expect("positive observer capacity");
        let (_messages, message_receiver) = std::sync::mpsc::channel();
        let options = WindowOptions {
            input_events: Some(sink),
            ..WindowOptions::default()
        };
        let mut app = WindowApp::new(RenderScene::new(), message_receiver, options);

        assert_eq!(app.input_policy, WindowInputPolicy::default());
        assert_eq!(app.input_policy, WindowInputPolicy::Native);
        assert!(app.process_native_key(KeyCode::ArrowUp, true));
        assert_eq!(
            observed_events.try_recv().expect("native key is forwarded"),
            WindowInput::Key {
                code: KeyCode::ArrowUp,
                pressed: true,
            }
        );
    }

    #[test]
    fn surface_alpha_mode_prefers_opaque_and_has_bounded_fallback() {
        assert_eq!(
            preferred_surface_alpha_mode(&[
                wgpu::CompositeAlphaMode::PreMultiplied,
                wgpu::CompositeAlphaMode::Opaque,
            ]),
            wgpu::CompositeAlphaMode::Opaque
        );
        assert_eq!(
            preferred_surface_alpha_mode(&[wgpu::CompositeAlphaMode::PostMultiplied]),
            wgpu::CompositeAlphaMode::PostMultiplied
        );
        assert_eq!(
            preferred_surface_alpha_mode(&[]),
            wgpu::CompositeAlphaMode::Auto
        );
    }

    fn packet(tick: u64, sequence: u64, generation: u64, digest: &str) -> Arc<RenderPacket> {
        Arc::new(RenderPacket::new(
            SessionId::from_raw(44),
            Tick(tick),
            sequence,
            generation,
            digest,
            Camera::default(),
            DirectionalLight::default(),
            [0.1; 3],
            [0.02, 0.03, 0.04, 1.0],
            Vec::new(),
        ))
    }

    fn managed_cpu_frames() -> (
        PacketCpuOwner,
        crate::CpuGenerationId,
        PacketStageInput,
        Arc<CpuFrameHandle>,
        Arc<CpuFrameHandle>,
    ) {
        let manifest = serde_json::to_vec(&ProjectManifest::new("managed-window-test"))
            .expect("manifest serializes");
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
        .expect("empty project prepares through the public asset-aware path");

        let session = SessionId::from_raw(44);
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .expect("default CPU renderer limits are valid");
        let generation = owner
            .prepare_generation(
                &project,
                1,
                session,
                &BTreeMap::new(),
                SourcePin::new(Arc::new(())),
                &AtomicBool::new(false),
            )
            .expect("empty imported project prepares a managed generation");
        let input = owner
            .stage_input(generation)
            .expect("prepared generation exposes its real stage input");
        let view = RenderView::new(
            Camera::default(),
            RenderProjection::Orthographic {
                vertical_size: 12.0,
            },
        );
        let mut make_frame = |sequence| {
            let snapshot = RenderSnapshot {
                sequence,
                tick: Tick(sequence),
                entities: Arc::from([]),
            };
            owner
                .extract_validated(
                    generation,
                    &snapshot,
                    sequence,
                    Some(&view),
                    &AtomicBool::new(false),
                )
                .expect("snapshot extracts through the real CPU renderer owner")
        };
        let first = make_frame(1);
        let second = make_frame(2);
        (owner, generation, input, first, second)
    }

    #[test]
    fn packet_window_initial_metadata_is_accepted_but_not_presented() {
        let initial = packet(9, 12, 5, &"ab".repeat(32));
        let (sender, receiver) = std::sync::mpsc::channel::<super::PacketWindowMessage>();
        let app = WindowApp::new_packet(
            initial,
            receiver,
            WindowOptions::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
            WindowInputPolicy::Native,
        );

        assert_eq!(
            app.latest_presentation,
            Some(PresentationMetadata {
                sequence: 12,
                tick: 9,
                generation: 5,
            })
        );
        assert!(app.last_presented.is_none());
        drop(sender);
    }

    #[test]
    fn packet_window_accepts_only_monotone_authority_and_exact_idempotency() {
        let digest_a = "ab".repeat(32);
        let digest_b = "cd".repeat(32);
        let previous = packet(8, 12, 3, &digest_a);
        assert_eq!(
            validate_packet_transition(
                &previous,
                &Arc::clone(&previous),
                PacketRendererLimits::default()
            )
            .unwrap(),
            PacketTransition::Idempotent
        );
        assert_eq!(
            validate_packet_transition(
                &previous,
                &packet(8, 12, 3, &digest_a),
                PacketRendererLimits::default()
            )
            .unwrap(),
            PacketTransition::Idempotent
        );
        assert_eq!(
            validate_packet_transition(
                &previous,
                &packet(8, 13, 3, &digest_a),
                PacketRendererLimits::default()
            )
            .unwrap(),
            PacketTransition::New
        );
        assert_eq!(
            validate_packet_transition(
                &previous,
                &packet(9, 13, 4, &digest_b),
                PacketRendererLimits::default()
            )
            .unwrap(),
            PacketTransition::New
        );

        assert!(
            validate_packet_transition(
                &previous,
                &packet(8, 11, 3, &digest_a),
                PacketRendererLimits::default()
            )
            .is_err()
        );
        assert!(
            validate_packet_transition(
                &previous,
                &packet(7, 13, 3, &digest_a),
                PacketRendererLimits::default()
            )
            .is_err()
        );
        assert!(
            validate_packet_transition(
                &previous,
                &packet(8, 13, 2, &digest_a),
                PacketRendererLimits::default()
            )
            .is_err()
        );
        assert!(
            validate_packet_transition(
                &previous,
                &packet(8, 13, 3, &digest_b),
                PacketRendererLimits::default()
            )
            .is_err()
        );
        assert!(
            validate_packet_transition(
                &previous,
                &packet(8, 12, 3, &digest_b),
                PacketRendererLimits::default()
            )
            .is_err()
        );
    }

    fn triangle_scene() -> RenderScene {
        let mesh = Mesh::new(
            vec![
                Vertex::new([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
                Vertex::new([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ],
            vec![0, 1, 2],
        );
        let mut scene = RenderScene::new();
        scene
            .objects
            .push(RenderObject::new(mesh, Mat4::IDENTITY, Material::default()));
        scene
    }

    #[test]
    fn input_observer_queue_is_bounded_and_reports_drops() {
        let (sink, receiver) = window_input_channel(1).expect("positive queue capacity");
        sink.try_send(WindowInput::Key {
            code: KeyCode::KeyW,
            pressed: true,
        });
        sink.try_send(WindowInput::Key {
            code: KeyCode::KeyW,
            pressed: false,
        });
        assert_eq!(sink.dropped_event_count(), 1);
        assert_eq!(
            receiver.try_recv(),
            Ok(WindowInput::Key {
                code: KeyCode::KeyW,
                pressed: true
            })
        );
    }

    #[test]
    fn window_scene_validation_accepts_empty_loading_scene_and_rejects_bad_geometry() {
        assert!(validate_window_scene(&RenderScene::new()).is_ok());

        let mut malformed = triangle_scene();
        malformed.objects[0].mesh.indices[2] = 99;
        assert!(validate_window_scene(&malformed).is_err());
    }

    #[test]
    fn window_geometry_budget_rejects_oversized_scene_before_upload() {
        let mut oversized = triangle_scene();
        // Keep the individual mesh structurally valid while exceeding a
        // window-path count limit with a deterministic CPU-only fixture.
        oversized.objects[0].mesh.vertices.resize(
            super::MAX_WINDOW_SCENE_VERTICES + 1,
            Vertex::new([0.0; 3], [0.0, 0.0, 1.0]),
        );
        oversized.objects[0].mesh.vertices[0].position[0] = f32::NAN;
        let error = validate_window_scene(&oversized)
            .expect_err("count budget must reject before reading malformed vertex data");
        assert!(
            error
                .to_string()
                .contains("bounded validation-scene budget")
        );
    }

    #[test]
    fn atomic_presentation_applies_scene_loading_and_metadata_together() {
        let mut scene = RenderScene::new();
        let mut loading = false;
        let mut metadata = None;
        let next = triangle_scene();
        apply_presentation(
            &mut scene,
            &mut loading,
            &mut metadata,
            next.clone(),
            true,
            PresentationMetadata {
                sequence: 10,
                tick: 7,
                generation: 3,
            },
            None,
        )
        .expect("valid presentation applies");
        assert_eq!(scene, next);
        assert!(loading);
        assert_eq!(
            metadata,
            Some(PresentationMetadata {
                sequence: 10,
                tick: 7,
                generation: 3,
            })
        );

        let previous_scene = scene.clone();
        let previous_metadata = metadata.clone();
        let mut invalid = RenderScene::new();
        invalid.camera.position = [f32::NAN, 0.0, 0.0];
        assert!(
            apply_presentation(
                &mut scene,
                &mut loading,
                &mut metadata,
                invalid,
                false,
                PresentationMetadata {
                    sequence: 11,
                    tick: 8,
                    generation: 3,
                },
                None,
            )
            .is_err()
        );
        assert_eq!(scene, previous_scene);
        assert!(loading);
        assert_eq!(metadata, previous_metadata);
    }

    #[test]
    fn frame_interval_samples_stay_bounded_and_compute_percentiles() {
        let mut intervals = FrameIntervals::new();
        let start = std::time::Instant::now();
        intervals.record_present(start);
        for frame in 1..=(FRAME_INTERVAL_SAMPLE_CAPACITY + 4) {
            intervals.record_present(start + std::time::Duration::from_millis(frame as u64));
        }
        let report = intervals.report();
        assert_eq!(report.samples_ms.len(), FRAME_INTERVAL_SAMPLE_CAPACITY);
        assert_eq!(report.sample_capacity, FRAME_INTERVAL_SAMPLE_CAPACITY);
        assert_eq!(report.samples_ms.first().copied(), Some(1.0));
        assert_eq!(report.samples_ms.last().copied(), Some(1.0));
        assert_eq!(report.p50_ms, Some(1.0));
        assert_eq!(report.p95_ms, Some(1.0));
        assert_eq!(report.p99_ms, Some(1.0));
        assert_eq!(report.max_ms, Some(1.0));
    }

    #[test]
    fn smoke_time_and_frame_limits_must_be_nonzero_when_set() {
        let mut options = WindowOptions::default();
        assert!(validate_options(&options).is_ok());
        options.max_run_time = Some(std::time::Duration::ZERO);
        assert!(validate_options(&options).is_err());
        options.max_run_time = None;
        options.max_frames = Some(0);
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn presentation_disconnect_is_nonfatal_by_default_and_can_be_fatal() {
        assert!(!WindowOptions::default().fail_on_presentation_disconnect);
        assert!(presentation_disconnect_error(false).is_none());
        let error = presentation_disconnect_error(true)
            .expect("opt-in disconnect handling returns a fatal error");
        assert!(
            error
                .to_string()
                .contains("presentation message source disconnected")
        );
    }

    #[test]
    fn smoke_time_and_frame_limits_trigger_at_the_requested_boundary() {
        let start = std::time::Instant::now();
        assert!(!max_run_time_reached(
            start,
            start + std::time::Duration::from_millis(49),
            Some(std::time::Duration::from_millis(50)),
        ));
        assert!(max_run_time_reached(
            start,
            start + std::time::Duration::from_millis(50),
            Some(std::time::Duration::from_millis(50)),
        ));
        assert!(!max_frames_reached(2, Some(3)));
        assert!(max_frames_reached(3, Some(3)));
    }

    #[test]
    fn managed_capture_poll_wake_is_bounded_and_never_spins_on_expired_deadline() {
        let now = std::time::Instant::now();
        let scheduled = now + std::time::Duration::from_secs(60);
        assert_eq!(
            managed_capture_wait_until(now, scheduled, None),
            now + MANAGED_CAPTURE_POLL_INTERVAL
        );
        assert_eq!(
            managed_capture_wait_until(
                now,
                scheduled,
                Some(now + std::time::Duration::from_millis(3)),
            ),
            now + std::time::Duration::from_millis(3)
        );
        assert_eq!(
            managed_capture_wait_until(
                now,
                scheduled,
                Some(now - std::time::Duration::from_millis(1)),
            ),
            now + MANAGED_CAPTURE_POLL_INTERVAL
        );
    }

    #[test]
    fn managed_capture_pump_retires_reused_local_key_and_takes_next_request() {
        let (_window_client, capture_client, server) =
            super::managed_packet_window_capture_channel();
        let worker = capture_client
            .register_worker()
            .expect("durable capture worker registers before window start");
        let shared = server
            .capture
            .as_ref()
            .expect("capture server lane is installed")
            .clone();
        shared
            .try_set_ready(
                super::ManagedCaptureReadyInfo::new(
                    super::ManagedCaptureSurfaceFormat::Rgba8Unorm,
                    8,
                    8,
                    64,
                )
                .expect("small capture readiness limits are valid"),
            )
            .expect("capture endpoint becomes ready once");

        let (_cpu_owner, _generation, input, _first, _second) = managed_cpu_frames();
        let source = super::ManagedCaptureSource::new(
            super::ManagedStageKey::INITIAL,
            input.session(),
            input.project_generation(),
            input.source_digest(),
        )
        .expect("source uses the actual prepared generation identity");
        let mut app = WindowApp::new_managed(
            input,
            server,
            super::ManagedPacketWindowOptions::default(),
            WindowInputPolicy::Native,
        );

        let key1 = super::ManagedCaptureKey::new(1).expect("nonzero capture key");
        capture_client
            .try_submit(
                super::ManagedCaptureRequest::new(
                    key1,
                    source.clone(),
                    Tick(0),
                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .expect("first request deadline is in the future"),
            )
            .expect("first request enters the capture slot");
        app.poll_managed_capture_request();
        assert_eq!(
            app.capture_request.as_ref().map(|request| request.key()),
            Some(key1)
        );

        capture_client
            .request_cancel(key1)
            .expect("cancel marks the first request");
        let Some(super::ManagedCaptureReturn::Failed { mut cleanup, .. }) = worker
            .try_take_return()
            .expect("cancelled request returns its no-copy cleanup")
        else {
            panic!("cancelled no-copy request must return Failed cleanup");
        };
        assert!(cleanup.try_finish().expect("no-copy cleanup releases cell"));

        let key2 = super::ManagedCaptureKey::new(2).expect("second nonzero capture key");
        capture_client
            .try_submit(
                super::ManagedCaptureRequest::new(
                    key2,
                    source,
                    Tick(0),
                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .expect("second request deadline is in the future"),
            )
            .expect("released cell accepts the next request");
        app.poll_managed_capture_request();
        assert_eq!(
            app.capture_request.as_ref().map(|request| request.key()),
            Some(key2)
        );
        assert_eq!(shared.cell().current_key(), Some(key2));
    }

    #[test]
    fn input_report_separates_keys_from_pointer_and_focus_during_loading() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let options = WindowOptions {
            initial_loading: true,
            ..WindowOptions::default()
        };
        let mut app = WindowApp::new(RenderScene::new(), receiver, options);
        app.emit_input(WindowInput::Key {
            code: KeyCode::ArrowUp,
            pressed: true,
        });
        app.emit_input(WindowInput::CursorMoved { x: 10.0, y: 20.0 });
        app.emit_input(WindowInput::Focused(false));

        let report = app.into_outcome().report;
        assert_eq!(report.input_events, 3);
        assert_eq!(report.loading_input_events, 3);
        assert_eq!(report.key_events, 1);
        assert_eq!(report.loading_key_events, 1);
        assert_eq!(report.cursor_events, 1);
        assert_eq!(report.loading_cursor_events, 1);
        assert_eq!(report.focus_events, 1);
        assert_eq!(report.loading_focus_events, 1);
    }

    #[test]
    fn short_camera_key_step_changes_the_owned_final_camera_immediately() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut app = WindowApp::new(RenderScene::new(), receiver, WindowOptions::default());
        let starting_camera = app.scene.camera.clone();
        app.keys_down.insert(KeyCode::KeyW);
        app.apply_camera_movement(1.0 / 120.0);

        assert_ne!(app.scene.camera, starting_camera);
        assert_eq!(app.counters.local_camera_changes, 1);
    }

    #[test]
    fn packet_camera_without_input_keeps_the_authority_view_unmodified() {
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let authoritative = packet(0, 1, 1, &"ab".repeat(32));
        let expected_view = authoritative.view().clone();
        let mut app = WindowApp::new_packet(
            authoritative,
            receiver,
            WindowOptions::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
            WindowInputPolicy::Native,
        );

        app.apply_camera_movement(1.0 / 60.0);

        assert!(app.local_view.is_none());
        assert_eq!(app.current_view(), expected_view);
        assert_eq!(app.counters.local_camera_changes, 0);
    }

    #[test]
    fn cancel_through_drops_only_pending_stage_frames_not_active_pose_updates() {
        let (_owner, _generation, _input, first, second) = managed_cpu_frames();
        let (_client, server) = super::managed::managed_packet_window_channel();
        let active_key = super::ManagedStageKey::INITIAL;
        let candidate_key = active_key.checked_next().expect("second managed key");
        let identity = super::ManagedGenerationIdentity::new(
            active_key,
            first.session(),
            first.project_generation(),
            first.source_digest(),
        );
        let mut state = ManagedWindowState {
            server,
            initial: identity.clone(),
            active: Some(ManagedFrame {
                key: active_key,
                identity,
                frame: Arc::clone(&first),
            }),
            stage: None,
            highest_key: candidate_key,
            deferred_key: None,
            pending_event: None,
            present_sequence: 0,
            last_presented: None,
            bootstrap_frames: 0,
        };

        assert!(managed_frame_is_cancelled(
            candidate_key,
            candidate_key.get(),
            false
        ));
        super::update_managed_frame(
            &mut state,
            active_key,
            Arc::clone(&second),
            candidate_key.get(),
        )
        .expect("candidate cancellation does not block active pose updates");
        let active = state.active.as_ref().expect("active frame remains present");
        assert_eq!(active.frame.publication_sequence(), 2);
        assert!(Arc::ptr_eq(&active.frame, &second));
    }

    #[test]
    fn managed_bootstrap_camera_input_preserves_the_first_authored_view() {
        let (_owner, _generation, _input, first, _second) = managed_cpu_frames();
        let (_sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut app = WindowApp::new(RenderScene::new(), receiver, WindowOptions::default());
        let (_client, server) = super::managed::managed_packet_window_channel();
        let identity = super::ManagedGenerationIdentity::new(
            super::ManagedStageKey::INITIAL,
            first.session(),
            first.project_generation(),
            first.source_digest(),
        );
        app.mode = WindowMode::Managed;
        app.managed = Some(super::ManagedWindowState {
            server,
            initial: identity.clone(),
            active: None,
            stage: None,
            highest_key: super::managed::ManagedStageKey::INITIAL,
            deferred_key: None,
            pending_event: None,
            present_sequence: 0,
            last_presented: None,
            bootstrap_frames: 0,
        });

        assert_eq!(
            first.view().projection(),
            RenderProjection::Orthographic {
                vertical_size: 12.0
            }
        );
        assert!(!managed_view_input_enabled(WindowMode::Managed, false));
        app.keys_down.insert(KeyCode::KeyW);
        app.apply_camera_movement(1.0 / 60.0);
        assert!(app.local_view.is_none());
        assert_eq!(app.counters.local_camera_changes, 0);

        app.managed.as_mut().unwrap().active = Some(ManagedFrame {
            key: super::ManagedStageKey::INITIAL,
            identity,
            frame: Arc::clone(&first),
        });
        assert_eq!(
            app.current_view().projection(),
            RenderProjection::Orthographic {
                vertical_size: 12.0
            }
        );
        app.apply_camera_movement(1.0 / 60.0);
        assert_eq!(
            app.local_view.as_ref().unwrap().projection(),
            RenderProjection::Orthographic {
                vertical_size: 12.0
            }
        );
        assert_eq!(app.counters.local_camera_changes, 1);
        assert!(managed_view_input_enabled(WindowMode::Managed, true));
        assert!(managed_view_input_enabled(WindowMode::Packet, false));
    }

    #[test]
    fn managed_payload_channel_returns_exact_owned_cpu_inputs_and_frames() {
        let (owner, generation, initial_input, first, second) = managed_cpu_frames();
        let full_input = owner.stage_input(generation).unwrap();
        let disconnected_input = owner.stage_input(generation).unwrap();
        let expected_session = full_input.session();
        let expected_generation = full_input.project_generation();
        let expected_digest = full_input.source_digest().to_owned();
        let key = super::ManagedStageKey::INITIAL;
        let candidate_key = key.checked_next().expect("candidate key follows initial");
        let after_candidate = candidate_key
            .checked_next()
            .expect("third key is representable");
        let (client, server) = super::managed::managed_packet_window_channel();

        client
            .try_send(super::ManagedPacketWindowInput::BeginStage {
                key,
                input: initial_input,
                limits: crate::PacketStageLimits::default(),
            })
            .expect("the one-slot channel accepts its first owned request");
        let returned_input = match client.try_send(super::ManagedPacketWindowInput::BeginStage {
            key: candidate_key,
            input: full_input,
            limits: crate::PacketStageLimits::default(),
        }) {
            Err(std::sync::mpsc::TrySendError::Full(
                super::ManagedPacketWindowInput::BeginStage { key, input, .. },
            )) => {
                assert_eq!(key, candidate_key);
                input
            }
            other => panic!("full channel returns the exact stage input, got {other:?}"),
        };
        assert_eq!(returned_input.session(), expected_session);
        assert_eq!(returned_input.project_generation(), expected_generation);
        assert_eq!(returned_input.source_digest(), expected_digest);

        let returned_full_frame = match client.try_send(super::ManagedPacketWindowInput::Frame {
            key,
            frame: Arc::clone(&first),
        }) {
            Err(std::sync::mpsc::TrySendError::Full(super::ManagedPacketWindowInput::Frame {
                key: returned_key,
                frame,
            })) => {
                assert_eq!(returned_key, key);
                frame
            }
            other => panic!("full channel returns the exact frame, got {other:?}"),
        };
        assert!(Arc::ptr_eq(&returned_full_frame, &first));

        drop(server);
        let returned_disconnected_frame =
            match client.try_send(super::ManagedPacketWindowInput::Frame {
                key: candidate_key,
                frame: Arc::clone(&second),
            }) {
                Err(std::sync::mpsc::TrySendError::Disconnected(
                    super::ManagedPacketWindowInput::Frame {
                        key: returned_key,
                        frame,
                    },
                )) => {
                    assert_eq!(returned_key, candidate_key);
                    frame
                }
                other => panic!("disconnected channel returns the exact frame, got {other:?}"),
            };
        assert!(Arc::ptr_eq(&returned_disconnected_frame, &second));

        let returned_disconnected_input =
            match client.try_send(super::ManagedPacketWindowInput::BeginStage {
                key: after_candidate,
                input: disconnected_input,
                limits: crate::PacketStageLimits::default(),
            }) {
                Err(std::sync::mpsc::TrySendError::Disconnected(
                    super::ManagedPacketWindowInput::BeginStage { key, input, .. },
                )) => {
                    assert_eq!(key, after_candidate);
                    input
                }
                other => {
                    panic!("disconnected channel returns the exact stage input, got {other:?}")
                }
            };
        assert_eq!(returned_disconnected_input.session(), expected_session);
        assert_eq!(
            returned_disconnected_input.project_generation(),
            expected_generation
        );
        assert_eq!(returned_disconnected_input.source_digest(), expected_digest);
    }

    #[test]
    fn managed_identity_and_present_metadata_preserve_canonical_project_digest() {
        let (_owner, _generation, input, first, _second) = managed_cpu_frames();
        let digest = input.source_digest();

        assert_eq!(digest.len(), 71);
        assert!(digest.starts_with("sha256:"));
        assert_eq!(first.source_digest(), digest);

        let identity = super::ManagedGenerationIdentity::new(
            super::ManagedStageKey::INITIAL,
            first.session(),
            first.project_generation(),
            first.source_digest(),
        );
        assert_eq!(identity.source_digest(), digest);

        let presented = super::ManagedPresentedView::new(
            super::ManagedStageKey::INITIAL,
            first.packet(),
            1,
            640,
            360,
            first.view(),
        )
        .expect("canonical project digest is retained in presented metadata");
        assert_eq!(presented.source_digest(), digest);
        assert_eq!(
            super::ManagedPresentedSummary::from_view(&presented).source_digest(),
            digest
        );
    }

    #[test]
    fn invalid_managed_start_returns_no_device_without_acknowledging_a_frame() {
        let (_owner, _generation, initial_input, _first, _second) = managed_cpu_frames();
        let (_client, server) = super::managed::managed_packet_window_channel();
        let mut options = super::ManagedPacketWindowOptions::default();
        options.window.width = 0;

        let run = super::run_packet_windowed_managed(initial_input, server, options);

        assert_eq!(
            run.outcome
                .as_ref()
                .expect_err("invalid options fail before native event-loop startup")
                .code(),
            super::ManagedWindowErrorCode::InvalidOptions
        );
        assert_eq!(run.gpu_owner.phase(), crate::WindowGpuOwnerPhase::NoDevice);
        assert_eq!(
            run.status.lifecycle(),
            super::ManagedWindowLifecycle::Closed
        );
        assert_eq!(run.status.last_presented(), None);
        assert_eq!(run.status.rendered_frames(), 0);
        assert_eq!(run.status.bootstrap_frames(), 0);
    }
}
