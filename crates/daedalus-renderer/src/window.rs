use std::{
    collections::HashSet,
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

use crate::{Camera, RenderScene, Renderer, RendererError};

/// Messages from runtime/presentation producers to the window-owned renderer.
/// The receiver is drained with `try_recv`; a delayed simulation or producer
/// never blocks OS events or local presentation state.
#[derive(Clone, Debug)]
pub enum WindowMessage {
    ReplaceScene(RenderScene),
    SetLoading(bool),
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
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "Daedalus".into(),
            width: 1280,
            height: 720,
            frame_interval: Duration::from_nanos(6_944_444),
            input_events: None,
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
/// messages are delayed.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowInput {
    Key { code: KeyCode, pressed: bool },
    MouseButton { button: MouseButton, pressed: bool },
    CursorMoved { x: f64, y: f64 },
    MouseWheel { lines: f32 },
    Resized { width: u32, height: u32 },
    Focused(bool),
}

/// Starts a native winit event loop. Send `WindowMessage::ReplaceScene` and
/// `SetLoading` through a bounded `sync_channel` to publish the latest owned scene
/// and independent loading state. Left-drag orbits the local camera, the wheel
/// zooms, WASD moves, Q/E move vertically, and Escape closes the window.
pub fn run_windowed(
    initial_scene: RenderScene,
    messages: Receiver<WindowMessage>,
    options: WindowOptions,
) -> Result<(), RendererError> {
    if options.width == 0 || options.height == 0 || options.frame_interval.is_zero() {
        return Err(RendererError::Window(
            "window size and redraw interval must be nonzero".into(),
        ));
    }
    initial_scene.validate()?;
    let event_loop = EventLoop::new().map_err(|error| RendererError::Window(error.to_string()))?;
    let mut app = WindowApp::new(initial_scene, messages, options);
    event_loop
        .run_app(&mut app)
        .map_err(|error| RendererError::Window(error.to_string()))?;
    if let Some(error) = app.failure.take() {
        Err(error)
    } else {
        Ok(())
    }
}

struct SurfaceState {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

struct WindowApp {
    initial_scene: Option<RenderScene>,
    messages: Receiver<WindowMessage>,
    options: WindowOptions,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    surface: Option<SurfaceState>,
    scene: RenderScene,
    local_camera: Option<Camera>,
    keys_down: HashSet<KeyCode>,
    left_mouse_down: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    last_update: Instant,
    next_redraw: Instant,
    loading_since: Option<Instant>,
    failure: Option<RendererError>,
}

impl WindowApp {
    fn new(
        initial_scene: RenderScene,
        messages: Receiver<WindowMessage>,
        options: WindowOptions,
    ) -> Self {
        let now = Instant::now();
        Self {
            initial_scene: Some(initial_scene.clone()),
            messages,
            options,
            window: None,
            renderer: None,
            surface: None,
            scene: initial_scene,
            local_camera: None,
            keys_down: HashSet::new(),
            left_mouse_down: false,
            last_cursor: None,
            last_update: now,
            next_redraw: now,
            loading_since: None,
            failure: None,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RendererError) {
        self.failure = Some(error);
        event_loop.exit();
    }

    fn initialize(&mut self, event_loop: &ActiveEventLoop) -> Result<(), RendererError> {
        let attributes = WindowAttributes::default()
            .with_title(self.options.title.clone())
            .with_inner_size(LogicalSize::new(self.options.width, self.options.height));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| RendererError::Window(error.to_string()))?,
        );
        let size = window.inner_size();
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
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.surface = Some(SurfaceState { surface, config });
        self.initial_scene = None;
        self.drain_messages();
        Ok(())
    }

    fn drain_messages(&mut self) {
        for _ in 0..64 {
            match self.messages.try_recv() {
                Ok(WindowMessage::ReplaceScene(mut scene)) => {
                    if let Some(camera) = &self.local_camera {
                        scene.camera = camera.clone();
                    }
                    self.scene = scene;
                }
                Ok(WindowMessage::SetLoading(true)) => {
                    if self.loading_since.is_none() {
                        self.loading_since = Some(Instant::now());
                    }
                }
                Ok(WindowMessage::SetLoading(false)) => self.loading_since = None,
                Err(
                    std::sync::mpsc::TryRecvError::Empty
                    | std::sync::mpsc::TryRecvError::Disconnected,
                ) => break,
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
        let camera = self
            .local_camera
            .get_or_insert_with(|| self.scene.camera.clone());
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
        if movement.length_squared() > 0.0 {
            let delta = movement.normalize() * (elapsed * 3.5);
            camera.position = (Vec3::from_array(camera.position) + delta).to_array();
            camera.target = (Vec3::from_array(camera.target) + delta).to_array();
            self.scene.camera = camera.clone();
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_messages();
        let now = Instant::now();
        self.update_local_camera(now);
        let Some(window) = &self.window else {
            return;
        };
        let Some(renderer) = &self.renderer else {
            return;
        };
        let Some(surface_state) = &mut self.surface else {
            return;
        };
        if surface_state.config.width == 0 || surface_state.config.height == 0 {
            return;
        }
        let frame = match surface_state.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                surface_state
                    .surface
                    .configure(&renderer.device, &surface_state.config);
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
        if let Err(error) = renderer.render_to_view(
            &view,
            surface_state.config.width,
            surface_state.config.height,
            &self.scene,
            loading_elapsed,
        ) {
            self.fail(event_loop, error);
            return;
        }
        window.pre_present_notify();
        frame.present();
    }

    fn emit_input(&self, event: WindowInput) {
        // Presentation keeps its own camera controls. This optional queue is a
        // best-effort observation channel for applications; it never blocks an
        // OS event callback while a runtime consumer is busy.
        if let Some(sender) = &self.options.input_events {
            sender.try_send(event);
        }
    }
}

impl ApplicationHandler for WindowApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(error) = self.initialize(event_loop) {
            self.fail(event_loop, error);
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
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let (Some(renderer), Some(surface_state)) = (&self.renderer, &mut self.surface)
                    && size.width > 0
                    && size.height > 0
                {
                    if let Err(error) = renderer.validate_dimensions(size.width, size.height) {
                        self.fail(event_loop, error);
                        return;
                    }
                    surface_state.config.width = size.width;
                    surface_state.config.height = size.height;
                    surface_state
                        .surface
                        .configure(&renderer.device, &surface_state.config);
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
                }
                self.emit_input(WindowInput::Focused(focused));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                if code == KeyCode::Escape && pressed {
                    event_loop.exit();
                    return;
                }
                if pressed {
                    self.keys_down.insert(code);
                } else {
                    self.keys_down.remove(&code);
                }
                self.emit_input(WindowInput::Key { code, pressed });
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
                    let camera = self
                        .local_camera
                        .get_or_insert_with(|| self.scene.camera.clone());
                    camera.orbit(-dx * 0.006, -dy * 0.006, 0.0);
                    self.scene.camera = camera.clone();
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
                let camera = self
                    .local_camera
                    .get_or_insert_with(|| self.scene.camera.clone());
                camera.orbit(0.0, 0.0, -lines * 0.12);
                self.scene.camera = camera.clone();
                self.emit_input(WindowInput::MouseWheel { lines });
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain_messages();
        let now = Instant::now();
        if now >= self.next_redraw {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            self.next_redraw = now + self.options.frame_interval;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_redraw));
    }
}

#[cfg(test)]
mod tests {
    use winit::keyboard::KeyCode;

    use super::{WindowInput, window_input_channel};

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
}
