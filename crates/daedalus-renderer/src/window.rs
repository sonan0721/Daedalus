use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
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

use crate::{
    Camera, PacketAssetLimits, PacketRenderError, PacketRenderer, PacketRendererLimits,
    PacketResourceStats, RenderPacket, RenderScene, Renderer, RendererError, RendererInfo,
};
use daedalus_core::{SessionId, Tick};

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

#[derive(Clone, Debug)]
pub struct WindowOptions {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Requests redraws near the requested interval. The operating system and
    /// display presentation mode may impose a different cadence.
    pub frame_interval: Duration,
    /// Optional bounded observer queue for OS input. Full-queue drops are
    /// counted on the sink, while local camera controls continue to work.
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
/// disconnected queue drops the observer event and increments
/// `dropped_event_count()`; local camera controls remain active.
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

/// Native input received by the window loop. The loop also handles camera
/// orbit and zoom locally, so those controls remain responsive while runtime
/// messages are delayed. Key input is always observed, including camera keys.
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
    validate_options(&options)
        .map_err(|error| PacketWindowError::InvalidOptions(error.to_string()))?;
    limits.validate().map_err(PacketWindowError::Packet)?;
    asset_limits.validate().map_err(PacketWindowError::Packet)?;
    initial_packet
        .validate(limits)
        .map_err(PacketWindowError::Packet)?;
    let event_loop =
        EventLoop::new().map_err(|error| PacketWindowError::EventLoop(error.to_string()))?;
    let mut app = WindowApp::new_packet(initial_packet, messages, options, limits, asset_limits);
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
    Ok(PacketAssetWindowOutcome {
        packet: app.into_packet_outcome(),
        asset_resource_stats,
    })
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

struct WindowApp {
    messages: Option<Receiver<WindowMessage>>,
    packet_messages: Option<Receiver<PacketWindowMessage>>,
    options: WindowOptions,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    packet_renderer: Option<PacketRenderer>,
    packet_limits: PacketRendererLimits,
    asset_limits: PacketAssetLimits,
    packet: Option<Arc<RenderPacket>>,
    last_presented: Option<PresentedPacket>,
    renderer_info: Option<RendererInfo>,
    surface: Option<SurfaceState>,
    scene: RenderScene,
    local_camera: Option<Camera>,
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
    ) -> Self {
        let now = Instant::now();
        let initial_loading = options.initial_loading;
        let width = options.width;
        let height = options.height;
        Self::with_source(
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
        )
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
            window: None,
            renderer: None,
            packet_renderer: None,
            packet_limits,
            asset_limits,
            packet,
            last_presented: None,
            renderer_info: None,
            surface: None,
            scene,
            local_camera: None,
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

    fn current_camera(&self) -> &Camera {
        self.local_camera.as_ref().unwrap_or_else(|| {
            self.packet
                .as_ref()
                .map_or(&self.scene.camera, |packet| packet.camera())
        })
    }

    fn sync_legacy_camera(&mut self) {
        if self.packet.is_none()
            && let Some(camera) = &self.local_camera
        {
            self.scene.camera = camera.clone();
        }
    }

    fn validate_current_dimensions(&self, width: u32, height: u32) -> Result<(), RendererError> {
        if let Some(renderer) = &self.renderer {
            renderer.validate_dimensions(width, height)
        } else if let Some(renderer) = &self.packet_renderer {
            renderer.validate_dimensions(width, height)
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
        let controls = window_controls_hint(self.packet.is_some());
        self.window_title = format!(
            "{} | {tick_and_generation} · {loading} | {controls}",
            self.options.title,
        );
        if let Some(window) = &self.window {
            window.set_title(&self.window_title);
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RendererError) {
        self.failure = Some(if self.packet.is_some() {
            WindowFailure::Packet(PacketWindowError::Renderer(error))
        } else {
            WindowFailure::Legacy(error)
        });
        event_loop.exit();
    }

    fn fail_packet(&mut self, event_loop: &ActiveEventLoop, error: PacketWindowError) {
        self.failure = Some(WindowFailure::Packet(error));
        event_loop.exit();
    }

    fn exit(&mut self, event_loop: &ActiveEventLoop, reason: WindowExitReason) {
        self.exit_reason = Some(reason);
        event_loop.exit();
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), WindowFailure> {
        let attributes = WindowAttributes::default()
            .with_title(self.options.title.clone())
            .with_inner_size(LogicalSize::new(self.options.width, self.options.height));
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
            alpha_mode: capabilities
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
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
        let initial_camera = self.current_camera().clone();
        let camera = self.local_camera.get_or_insert(initial_camera);
        let previous = camera.clone();
        let forward = (Vec3::from_array(camera.target) - Vec3::from_array(camera.position))
            .normalize_or_zero();
        let forward_flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or_zero();
        let right = forward_flat.cross(Vec3::Y).normalize_or_zero();
        let mut movement = Vec3::ZERO;
        if self.keys_down.contains(&KeyCode::KeyW) {
            movement += forward_flat;
        }
        if self.keys_down.contains(&KeyCode::KeyS) {
            movement -= forward_flat;
        }
        if self.keys_down.contains(&KeyCode::KeyD) {
            movement += right;
        }
        if self.keys_down.contains(&KeyCode::KeyA) {
            movement -= right;
        }
        if self.keys_down.contains(&KeyCode::KeyE) {
            movement += Vec3::Y;
        }
        if self.keys_down.contains(&KeyCode::KeyQ) {
            movement -= Vec3::Y;
        }
        move_camera(camera, movement, elapsed);
        if *camera != previous {
            self.sync_legacy_camera();
            self.counters.local_camera_changes =
                self.counters.local_camera_changes.saturating_add(1);
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
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
        let rendered_packet = if let Some(packet) = self.packet.as_ref() {
            let Some(renderer) = self.packet_renderer.as_mut() else {
                return;
            };
            match renderer.render_packet_view(
                &view,
                width,
                height,
                Arc::clone(packet),
                self.local_camera.as_ref(),
                loading_elapsed,
            ) {
                Ok(packet) => Some(packet),
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
        if let Some(packet) = rendered_packet {
            let camera = self
                .local_camera
                .as_ref()
                .unwrap_or_else(|| packet.camera())
                .clone();
            self.last_presented = Some(PresentedPacket {
                session: packet.session(),
                tick: packet.tick(),
                sequence: packet.sequence(),
                project_generation: packet.project_generation(),
                source_digest: packet.source_digest().to_owned(),
                camera,
                physical_width: width,
                physical_height: height,
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
        if let Some(sender) = &self.options.input_events {
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

impl ApplicationHandler for WindowApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
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
                self.exit(event_loop, WindowExitReason::CloseRequested);
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
                        .or_else(|| self.packet_renderer.as_ref().map(PacketRenderer::device));
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
                if pressed {
                    self.keys_down.insert(code);
                } else {
                    self.keys_down.remove(&code);
                }
                self.emit_input(WindowInput::Key { code, pressed });
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
                    self.exit(event_loop, WindowExitReason::EscapePressed);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if button == MouseButton::Left {
                    self.left_mouse_down = pressed;
                }
                self.emit_input(WindowInput::MouseButton { button, pressed });
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.left_mouse_down
                    && let Some(previous) = self.last_cursor
                {
                    let dx = (position.x - previous.x) as f32;
                    let dy = (position.y - previous.y) as f32;
                    let initial_camera = self.current_camera().clone();
                    let camera = self.local_camera.get_or_insert(initial_camera);
                    let previous_camera = camera.clone();
                    camera.orbit(-dx * 0.006, -dy * 0.006, 0.0);
                    if *camera != previous_camera {
                        self.sync_legacy_camera();
                        self.counters.local_camera_changes =
                            self.counters.local_camera_changes.saturating_add(1);
                    }
                }
                self.last_cursor = Some(position);
                self.emit_input(WindowInput::CursorMoved {
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(position) => (position.y as f32) * 0.01,
                };
                let initial_camera = self.current_camera().clone();
                let camera = self.local_camera.get_or_insert(initial_camera);
                let previous_camera = camera.clone();
                camera.orbit(0.0, 0.0, -lines * 0.12);
                if *camera != previous_camera {
                    self.sync_legacy_camera();
                    self.counters.local_camera_changes =
                        self.counters.local_camera_changes.saturating_add(1);
                }
                self.emit_input(WindowInput::MouseWheel { lines });
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
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

fn window_controls_hint(packet_mode: bool) -> &'static str {
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
    use std::sync::Arc;

    use daedalus_core::{SessionId, Tick};
    use glam::Mat4;
    use winit::keyboard::KeyCode;

    use crate::{
        Camera, DirectionalLight, Material, Mesh, PacketAssetLimits, PacketRendererLimits,
        RenderObject, RenderPacket, RenderScene, Vertex,
    };

    use super::{
        FRAME_INTERVAL_SAMPLE_CAPACITY, FrameIntervals, PacketTransition, PresentationMetadata,
        WindowApp, WindowInput, WindowOptions, apply_presentation, max_frames_reached,
        max_run_time_reached, presentation_disconnect_error, validate_options,
        validate_packet_transition, validate_window_scene, window_controls_hint,
        window_input_channel,
    };

    #[test]
    fn control_hint_describes_only_controls_available_in_each_source_mode() {
        assert_eq!(
            window_controls_hint(true),
            "WASD/QE + drag/wheel · Esc close"
        );
        assert_eq!(
            window_controls_hint(false),
            "WASD/QE + drag/wheel · Arrows move · R regenerate · Esc close"
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
}
