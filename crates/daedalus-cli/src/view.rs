use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::Duration,
};

use clap::Args;
use daedalus_content::voxel::{FaceDirection, GreedyMesh, MaterialSurface, MeshQuad};
use daedalus_core::{RenderEntitySnapshot, Transform};
use daedalus_renderer::{
    Camera, CaptureOptions, DirectionalLight, Material, Mesh, RenderObject, RenderScene, Vertex,
    WindowExitReason, WindowInput, WindowKeyCode, WindowMessage, WindowOptions,
    run_windowed_reported, window_input_channel,
};
use daedalus_runtime::{
    EngineConfig, LiveCommand, LiveCommandOutcome, LiveFrame, LiveSession, LiveSessionConfig,
    LiveShutdownReport, LiveStatusReadError,
};
use glam::{Mat4, Quat, Vec3};
use serde_json::{Value, json};

const VIEW_INPUT_CAPACITY: usize = 1_024;
const VIEW_COMMAND_CAPACITY: usize = 128;
const VIEW_RESULT_CAPACITY: usize = 128;
const VIEW_SCENE_CAPACITY: usize = 1;
const VIEW_MAX_INPUTS_PER_POLL: usize = 128;
const VIEW_MAX_RESULTS_PER_POLL: usize = 128;
const VIEW_MAX_MESH_QUADS: usize = 8_192;
const VIEW_MAX_MESH_BYTES: usize = 16 * 1024 * 1024;
const VIEW_MAX_RECENT_RESULTS: usize = 256;
const VIEW_POLL_INTERVAL: Duration = Duration::from_millis(2);
const _: () = assert!(VIEW_MAX_MESH_BYTES >= VIEW_MAX_MESH_QUADS * 120);

#[derive(Args, Debug)]
pub(crate) struct ViewArgs {
    /// Requested native window width in logical points.
    #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(64..=4096))]
    width: u32,
    /// Requested native window height in logical points.
    #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(64..=4096))]
    height: u32,
    /// Close automatically after this many seconds (1 to 3600); omit for an interactive session.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=3600))]
    seconds: Option<u32>,
    /// Number of real terrain generation and meshing repetitions per job (1 to 10000).
    #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(u32).range(1..=10000))]
    work_iterations: u32,
    /// Deterministic terrain seed.
    #[arg(long, default_value_t = 42)]
    seed: u64,
    /// Write the bounded JSON session report to this path.
    #[arg(long)]
    report: Option<PathBuf>,
    /// After the window exits, render the last owned scene to a PNG at this path.
    #[arg(long)]
    capture_output: Option<PathBuf>,
}

#[derive(Default)]
struct BridgeReport {
    frames_observed: u64,
    presentation_updates_sent: u64,
    presentation_updates_coalesced: u64,
    input_events_observed: u64,
    focus_resets: u64,
    movement_commands_requested: u64,
    movement_commands_accepted: u64,
    movement_command_submit_rejections: u64,
    movement_commands_applied: u64,
    movement_command_rejections: u64,
    terrain_commands_requested: u64,
    terrain_commands_accepted: u64,
    terrain_command_submit_rejections: u64,
    terrain_generations_completed: u64,
    terrain_generations_superseded: u64,
    terrain_command_rejections: u64,
    command_results_received: u64,
    pending_command_results: HashMap<u64, CommandKind>,
    initial_entity: Option<RenderEntitySnapshot>,
    final_entity: Option<RenderEntitySnapshot>,
    last_observed_tick: Option<u64>,
    cpu_budget: Option<Value>,
    latest_terrain: Option<TerrainCommandResult>,
    first_terrain_mesh: Option<Value>,
    recent_command_results: VecDeque<Value>,
}

#[derive(Clone, Copy)]
enum CommandKind {
    Movement,
    Terrain,
}

#[derive(Clone)]
struct TerrainCommandResult {
    generation: u64,
    seed: u64,
    quad_count: usize,
}

#[derive(Default)]
struct InputDebouncer {
    pressed: HashSet<WindowKeyCode>,
    focus_resets: u64,
}

impl InputDebouncer {
    fn observe(&mut self, input: &WindowInput) -> Option<LiveCommand> {
        match input {
            WindowInput::Focused(false) => {
                self.pressed.clear();
                self.focus_resets = self.focus_resets.saturating_add(1);
                None
            }
            WindowInput::Key { code, pressed } => {
                if !pressed {
                    self.pressed.remove(code);
                    return None;
                }
                if !self.pressed.insert(*code) {
                    return None;
                }
                match code {
                    WindowKeyCode::ArrowUp => Some(LiveCommand::MoveEntity {
                        delta: [0.0, 0.0, -1.0],
                    }),
                    WindowKeyCode::ArrowDown => Some(LiveCommand::MoveEntity {
                        delta: [0.0, 0.0, 1.0],
                    }),
                    WindowKeyCode::ArrowLeft => Some(LiveCommand::MoveEntity {
                        delta: [-1.0, 0.0, 0.0],
                    }),
                    WindowKeyCode::ArrowRight => Some(LiveCommand::MoveEntity {
                        delta: [1.0, 0.0, 0.0],
                    }),
                    WindowKeyCode::KeyR => Some(LiveCommand::RegenerateTerrain { seed: 0 }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

pub(crate) fn run(options: ViewArgs) -> Result<(), Box<dyn Error>> {
    let (input_sink, input_receiver) = window_input_channel(VIEW_INPUT_CAPACITY)?;
    let (scene_sender, scene_receiver) = mpsc::sync_channel(VIEW_SCENE_CAPACITY);
    let initial_scene = validation_scene();
    scene_sender
        .try_send(WindowMessage::Present {
            scene: initial_scene.clone(),
            loading: true,
            sequence: 0,
            tick: 0,
            generation: 0,
        })
        .map_err(|_| "could not initialize the bounded presentation queue")?;

    let engine = EngineConfig {
        compute_workers: std::thread::available_parallelism()
            .map_or(1, |count| count.get().saturating_sub(4).max(1)),
        command_capacity: VIEW_COMMAND_CAPACITY,
        ..EngineConfig::default()
    };
    let live_config = LiveSessionConfig {
        engine,
        command_capacity: VIEW_COMMAND_CAPACITY,
        result_capacity: VIEW_RESULT_CAPACITY,
        max_mesh_quads: VIEW_MAX_MESH_QUADS,
        max_mesh_bytes: VIEW_MAX_MESH_BYTES,
        generation_iterations: options.work_iterations,
        active_job_capacity: 1,
        initial_seed: options.seed,
        initial_entity_translation: [8.0, 14.0, 8.0],
        ..LiveSessionConfig::default()
    };
    let session = Arc::new(LiveSession::start(live_config)?);
    let stop_bridge = Arc::new(AtomicBool::new(false));
    let bridge_session = Arc::clone(&session);
    let bridge_stop = Arc::clone(&stop_bridge);
    let abort_sender = scene_sender.clone();
    let bridge = match thread::Builder::new()
        .name("daedalus-view-presentation".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                presentation_bridge(
                    bridge_session,
                    input_receiver,
                    &scene_sender,
                    bridge_stop,
                    options.seed,
                )
            }));
            match result {
                Ok(Ok(report)) => Ok(report),
                Ok(Err(message)) => {
                    send_abort(&abort_sender, message.clone());
                    Err(message)
                }
                Err(payload) => {
                    let message =
                        format!("presentation bridge panicked: {}", panic_message(payload));
                    send_abort(&abort_sender, message.clone());
                    Err(message)
                }
            }
        }) {
        Ok(bridge) => bridge,
        Err(error) => {
            session.request_stop();
            let session = Arc::try_unwrap(session).map_err(|_| {
                std::io::Error::other(
                    "live session still has an unexpected owner during startup failure",
                )
            })?;
            let _shutdown = session.shutdown();
            return Err(Box::new(error));
        }
    };

    let window_options = WindowOptions {
        title: "Daedalus Live Session".into(),
        width: options.width,
        height: options.height,
        input_events: Some(input_sink.clone()),
        initial_loading: true,
        fail_on_presentation_disconnect: true,
        max_run_time: options
            .seconds
            .map(|seconds| Duration::from_secs(u64::from(seconds))),
        max_frames: None,
        ..WindowOptions::default()
    };
    let window_result = run_windowed_reported(initial_scene, scene_receiver, window_options);

    stop_bridge.store(true, Ordering::Release);
    session.request_stop();
    let bridge_result = match bridge.join() {
        Ok(result) => result,
        Err(payload) => Err(format!(
            "presentation bridge panicked: {}",
            panic_message(payload)
        )),
    };
    let session = Arc::try_unwrap(session).map_err(|_| {
        std::io::Error::other(
            "live session still has an unexpected owner after the presentation bridge joined",
        )
    })?;
    let shutdown_report = session.shutdown();

    let mut bridge_report =
        bridge_result.map_err(|message| -> Box<dyn Error> { message.into() })?;
    for result in &shutdown_report.remaining_command_results {
        record_command_result(&mut bridge_report, result.id, &result.outcome);
    }
    let window_outcome = window_result?;
    let capture = match options.capture_output {
        Some(path) => {
            let renderer = daedalus_renderer::Renderer::new_offscreen_blocking()?;
            let artifact = renderer.render_png(
                &window_outcome.final_scene,
                &CaptureOptions::new(options.width, options.height, path),
            )?;
            json!({
                "path": artifact.path,
                "width": artifact.width,
                "height": artifact.height,
                "rgba8_bytes": artifact.rgba8_bytes,
                "renderer": renderer_info_json(&artifact.renderer),
                "scene_object_count": window_outcome.final_scene.objects.len(),
                "capture_kind": "post_window_offscreen",
            })
        }
        None => Value::Null,
    };
    let report = make_report(
        &window_outcome.report,
        &bridge_report,
        &shutdown_report,
        input_sink.dropped_event_count(),
        options.seed,
        options.work_iterations,
        capture,
    );
    if let Some(path) = options.report {
        fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    println!("{}", serde_json::to_string(&report)?);
    match &shutdown_report.termination {
        daedalus_runtime::LiveTermination::RuntimeError(message) => {
            return Err(std::io::Error::other(format!(
                "live runtime shut down with an error: {message}"
            ))
            .into());
        }
        daedalus_runtime::LiveTermination::Panicked(message) => {
            return Err(std::io::Error::other(format!("live runtime panicked: {message}")).into());
        }
        daedalus_runtime::LiveTermination::StopRequested => {}
    }
    Ok(())
}

fn presentation_bridge(
    session: Arc<LiveSession>,
    input_receiver: Receiver<WindowInput>,
    scene_sender: &SyncSender<WindowMessage>,
    stop: Arc<AtomicBool>,
    base_seed: u64,
) -> Result<BridgeReport, String> {
    let mut report = BridgeReport {
        recent_command_results: VecDeque::with_capacity(VIEW_MAX_RECENT_RESULTS),
        ..BridgeReport::default()
    };
    let mut debouncer = InputDebouncer::default();
    let mut next_seed = base_seed.wrapping_add(1);
    let mut last_sequence = None;
    let mut cached_mesh: Option<(Arc<GreedyMesh>, Vec<RenderObject>)> = None;
    let mut pending_scene: Option<WindowMessage> = None;

    while !stop.load(Ordering::Acquire) {
        for _ in 0..VIEW_MAX_INPUTS_PER_POLL {
            let input = match input_receiver.try_recv() {
                Ok(input) => input,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            report.input_events_observed = report.input_events_observed.saturating_add(1);
            if matches!(&input, WindowInput::Focused(false)) {
                let _ = debouncer.observe(&input);
                report.focus_resets = debouncer.focus_resets;
                continue;
            }
            let Some(command) = debouncer.observe(&input) else {
                continue;
            };
            match command {
                LiveCommand::MoveEntity { delta } => {
                    report.movement_commands_requested =
                        report.movement_commands_requested.saturating_add(1);
                    match session.try_command(LiveCommand::MoveEntity { delta }) {
                        Ok(accepted) => {
                            report.movement_commands_accepted =
                                report.movement_commands_accepted.saturating_add(1);
                            report
                                .pending_command_results
                                .insert(accepted.id, CommandKind::Movement);
                        }
                        Err(_) => {
                            report.movement_command_submit_rejections =
                                report.movement_command_submit_rejections.saturating_add(1);
                        }
                    }
                }
                LiveCommand::RegenerateTerrain { .. } => {
                    report.terrain_commands_requested =
                        report.terrain_commands_requested.saturating_add(1);
                    let seed = next_seed;
                    next_seed = next_seed.wrapping_add(1);
                    match session.try_command(LiveCommand::RegenerateTerrain { seed }) {
                        Ok(accepted) => {
                            report.terrain_commands_accepted =
                                report.terrain_commands_accepted.saturating_add(1);
                            report
                                .pending_command_results
                                .insert(accepted.id, CommandKind::Terrain);
                        }
                        Err(_) => {
                            report.terrain_command_submit_rejections =
                                report.terrain_command_submit_rejections.saturating_add(1);
                        }
                    }
                }
                LiveCommand::Stop => {}
            }
        }
        report.focus_resets = debouncer.focus_resets;

        match session.try_termination() {
            Err(LiveStatusReadError::Busy) => {}
            Err(error) => {
                return Err(format!("could not read live-session termination: {error}"));
            }
            Ok(Some(daedalus_runtime::LiveTermination::StopRequested))
                if stop.load(Ordering::Acquire) =>
            {
                break;
            }
            Ok(Some(termination)) => {
                return Err(format!(
                    "live runtime terminated while the window was open: {termination:?}"
                ));
            }
            Ok(None) => {}
        }

        for _ in 0..VIEW_MAX_RESULTS_PER_POLL {
            match session.try_result().map_err(|error| error.to_string())? {
                Some(result) => {
                    record_command_result(&mut report, result.id, &result.outcome);
                }
                None => break,
            }
        }

        if let Some(frame) = session.try_latest_frame()
            && last_sequence != Some(frame.sequence)
        {
            let scene = build_scene(&frame, &mut cached_mesh)?;
            observe_frame(&mut report, &frame);
            if report.first_terrain_mesh.is_none()
                && let Some(mesh) = &frame.terrain.mesh
            {
                report.first_terrain_mesh = Some(json!({
                    "mesh_generation": mesh.tag.job_generation,
                    "desired_generation": frame.terrain.generation,
                    "desired_seed": frame.terrain.seed,
                    "loading": frame.terrain.loading,
                    "quad_count": mesh.quads.len(),
                    "matches_initial_seed": frame.terrain.seed == base_seed
                        && mesh.tag.job_generation == frame.terrain.generation,
                }));
            }
            last_sequence = Some(frame.sequence);
            let message = WindowMessage::Present {
                scene,
                loading: frame.terrain.loading,
                sequence: frame.sequence,
                tick: frame.snapshot.tick.get(),
                generation: frame.terrain.generation,
            };
            if pending_scene.replace(message).is_some() {
                report.presentation_updates_coalesced =
                    report.presentation_updates_coalesced.saturating_add(1);
            }
        }

        if let Some(message) = pending_scene.take() {
            match scene_sender.try_send(message) {
                Ok(()) => {
                    report.presentation_updates_sent =
                        report.presentation_updates_sent.saturating_add(1);
                }
                Err(TrySendError::Full(message)) => pending_scene = Some(message),
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        thread::sleep(VIEW_POLL_INTERVAL);
    }
    Ok(report)
}

fn send_abort(sender: &SyncSender<WindowMessage>, message: String) {
    let mut abort = WindowMessage::Abort(message);
    // The queue holds at most one scene. Retry briefly so the window can drain that
    // last owned scene and receive the fatal bridge/runtime diagnostic.
    for _ in 0..500 {
        match sender.try_send(abort) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => return,
            Err(TrySendError::Full(message)) => {
                abort = message;
                thread::sleep(VIEW_POLL_INTERVAL);
            }
        }
    }
}

fn observe_frame(report: &mut BridgeReport, frame: &LiveFrame) {
    report.frames_observed = report.frames_observed.saturating_add(1);
    let entity = frame.snapshot.entities.first().copied();
    if report.initial_entity.is_none() {
        report.initial_entity = entity;
    }
    report.final_entity = entity;
    report.last_observed_tick = Some(frame.snapshot.tick.get());
    report.cpu_budget = serde_json::to_value(frame.cpu_budget).ok();
}

fn record_command_result(report: &mut BridgeReport, id: u64, outcome: &LiveCommandOutcome) {
    report.command_results_received = report.command_results_received.saturating_add(1);
    let command_kind = report.pending_command_results.remove(&id);
    match (&command_kind, outcome) {
        (Some(CommandKind::Movement), LiveCommandOutcome::Applied { .. }) => {
            report.movement_commands_applied = report.movement_commands_applied.saturating_add(1);
        }
        (Some(CommandKind::Movement), LiveCommandOutcome::Rejected(_)) => {
            report.movement_command_rejections =
                report.movement_command_rejections.saturating_add(1);
        }
        (
            Some(CommandKind::Terrain),
            LiveCommandOutcome::TerrainReady {
                generation,
                seed,
                quad_count,
            },
        ) => {
            report.terrain_generations_completed =
                report.terrain_generations_completed.saturating_add(1);
            report.latest_terrain = Some(TerrainCommandResult {
                generation: *generation,
                seed: *seed,
                quad_count: *quad_count,
            });
        }
        (Some(CommandKind::Terrain), LiveCommandOutcome::Superseded { .. }) => {
            report.terrain_generations_superseded =
                report.terrain_generations_superseded.saturating_add(1);
        }
        (Some(CommandKind::Terrain), LiveCommandOutcome::Rejected(_)) => {
            report.terrain_command_rejections = report.terrain_command_rejections.saturating_add(1);
        }
        _ => {}
    }
    record_recent_result(
        &mut report.recent_command_results,
        json!({
            "id": id,
            "kind": command_kind.map(command_kind_name),
            "outcome": outcome,
        }),
    );
}

fn record_recent_result(history: &mut VecDeque<Value>, value: Value) {
    if history.len() == VIEW_MAX_RECENT_RESULTS {
        history.pop_front();
    }
    history.push_back(value);
}

fn command_kind_name(kind: CommandKind) -> &'static str {
    match kind {
        CommandKind::Movement => "movement",
        CommandKind::Terrain => "terrain",
    }
}

fn build_scene(
    frame: &LiveFrame,
    cached_mesh: &mut Option<(Arc<GreedyMesh>, Vec<RenderObject>)>,
) -> Result<RenderScene, String> {
    let mut scene = validation_scene();
    if let Some(mesh) = &frame.terrain.mesh {
        let needs_rebuild = cached_mesh
            .as_ref()
            .is_none_or(|(previous, _)| !Arc::ptr_eq(previous, mesh));
        if needs_rebuild {
            *cached_mesh = Some((Arc::clone(mesh), terrain_objects(mesh)?));
        }
        if let Some((_, objects)) = cached_mesh {
            scene.objects.extend(objects.iter().cloned());
        }
    }
    if let Some(entity) = frame.snapshot.entities.first() {
        let transform = transform_matrix(entity.transform)?;
        scene.objects.push(RenderObject::new(
            movable_box_mesh(),
            transform,
            Material {
                base_color: [0.055, 0.32, 0.78],
                metallic: 0.14,
                roughness: 0.28,
                emissive: [0.0; 3],
            },
        ));
    }
    validate_scene_budget(&scene)?;
    Ok(scene)
}

fn validation_scene() -> RenderScene {
    let mut scene = RenderScene::new();
    scene.camera = Camera {
        position: [25.0, 24.0, 30.0],
        target: [8.0, 8.0, 8.0],
        up: [0.0, 1.0, 0.0],
        vertical_fov_radians: 55.0_f32.to_radians(),
        near: 0.1,
        far: 250.0,
    };
    scene.directional_light = DirectionalLight {
        direction: [-0.45, -0.82, -0.35],
        color: [1.0, 0.96, 0.82],
        intensity: 3.5,
    };
    scene.ambient_color = [0.22, 0.25, 0.22];
    scene.clear_color = [0.055, 0.075, 0.105, 1.0];
    scene
}

fn terrain_objects(mesh: &GreedyMesh) -> Result<Vec<RenderObject>, String> {
    if mesh.quads.len() > VIEW_MAX_MESH_QUADS {
        return Err(format!(
            "live terrain has {} quads, over the CLI view budget of {VIEW_MAX_MESH_QUADS}",
            mesh.quads.len()
        ));
    }
    let mut builders = std::collections::BTreeMap::<u16, (Vec<Vertex>, Vec<u32>)>::new();
    for quad in &mesh.quads {
        if quad.surface != MaterialSurface::Opaque {
            return Err("live terrain contains a transparent face unsupported by this view".into());
        }
        let (vertices, indices) = builders.entry(quad.material_id).or_default();
        push_mesh_quad(vertices, indices, quad)?;
    }
    let mut objects = Vec::with_capacity(builders.len());
    for (material_id, (vertices, indices)) in builders {
        if vertices.is_empty() {
            continue;
        }
        let material = match material_id {
            1 => Material {
                base_color: [0.16, 0.39, 0.105],
                metallic: 0.0,
                roughness: 0.9,
                emissive: [0.0; 3],
            },
            2 => Material {
                base_color: [0.30, 0.18, 0.085],
                metallic: 0.0,
                roughness: 0.94,
                emissive: [0.0; 3],
            },
            other => {
                return Err(format!(
                    "live terrain uses unconfigured material id {other}"
                ));
            }
        };
        objects.push(RenderObject::new(
            Mesh::new(vertices, indices),
            Mat4::IDENTITY,
            material,
        ));
    }
    Ok(objects)
}

fn push_mesh_quad(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    quad: &MeshQuad,
) -> Result<(), String> {
    let (axis, positive) = match quad.face {
        FaceDirection::NegativeX => (0, false),
        FaceDirection::PositiveX => (0, true),
        FaceDirection::NegativeY => (1, false),
        FaceDirection::PositiveY => (1, true),
        FaceDirection::NegativeZ => (2, false),
        FaceDirection::PositiveZ => (2, true),
    };
    let u_axis = (axis + 1) % 3;
    let v_axis = (axis + 2) % 3;
    let corner0 = quad.origin.map(f32::from);
    let mut corner1 = corner0;
    corner1[u_axis] += f32::from(quad.extent[0]);
    let mut corner2 = corner1;
    corner2[v_axis] += f32::from(quad.extent[1]);
    let mut corner3 = corner0;
    corner3[v_axis] += f32::from(quad.extent[1]);
    let mut normal = [0.0; 3];
    normal[axis] = if positive { 1.0 } else { -1.0 };
    let base = u32::try_from(vertices.len()).map_err(|_| "terrain vertex index overflow")?;
    vertices.extend([
        Vertex::new(corner0, normal),
        Vertex::new(corner1, normal),
        Vertex::new(corner2, normal),
        Vertex::new(corner3, normal),
    ]);
    if positive {
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    } else {
        indices.extend([base, base + 2, base + 1, base, base + 3, base + 2]);
    }
    Ok(())
}

fn movable_box_mesh() -> Mesh {
    let half = 0.7;
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    let faces = [
        (
            [
                [-half, -half, half],
                [half, -half, half],
                [half, half, half],
                [-half, half, half],
            ],
            [0.0, 0.0, 1.0],
        ),
        (
            [
                [-half, -half, -half],
                [-half, half, -half],
                [half, half, -half],
                [half, -half, -half],
            ],
            [0.0, 0.0, -1.0],
        ),
        (
            [
                [half, -half, -half],
                [half, half, -half],
                [half, half, half],
                [half, -half, half],
            ],
            [1.0, 0.0, 0.0],
        ),
        (
            [
                [-half, -half, -half],
                [-half, -half, half],
                [-half, half, half],
                [-half, half, -half],
            ],
            [-1.0, 0.0, 0.0],
        ),
        (
            [
                [-half, half, -half],
                [-half, half, half],
                [half, half, half],
                [half, half, -half],
            ],
            [0.0, 1.0, 0.0],
        ),
        (
            [
                [-half, -half, -half],
                [half, -half, -half],
                [half, -half, half],
                [-half, -half, half],
            ],
            [0.0, -1.0, 0.0],
        ),
    ];
    for (corners, normal) in faces {
        let base = vertices.len() as u32;
        vertices.extend(corners.map(|position| Vertex::new(position, normal)));
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Mesh::new(vertices, indices)
}

fn transform_matrix(transform: Transform) -> Result<Mat4, String> {
    if !transform.is_finite() {
        return Err("live ECS transform contains non-finite values".into());
    }
    let rotation = Quat::from_xyzw(
        transform.rotation[0],
        transform.rotation[1],
        transform.rotation[2],
        transform.rotation[3],
    );
    if rotation.length_squared() < 1.0e-8 {
        return Err("live ECS transform has a zero-length rotation".into());
    }
    let matrix = Mat4::from_scale_rotation_translation(
        Vec3::from_array(transform.scale),
        rotation.normalize(),
        Vec3::from_array(transform.translation),
    );
    if matrix.determinant().abs() < 1.0e-8 {
        return Err("live ECS transform is singular and cannot render a movable box".into());
    }
    Ok(matrix)
}

fn validate_scene_budget(scene: &RenderScene) -> Result<(), String> {
    let mut vertices = 0usize;
    let mut indices = 0usize;
    let mut geometry_bytes = 0usize;
    for object in &scene.objects {
        vertices = vertices
            .checked_add(object.mesh.vertices.len())
            .ok_or("scene vertex count overflow")?;
        indices = indices
            .checked_add(object.mesh.indices.len())
            .ok_or("scene index count overflow")?;
        let object_bytes = object
            .mesh
            .vertices
            .len()
            .checked_mul(std::mem::size_of::<Vertex>())
            .and_then(|bytes| {
                object
                    .mesh
                    .indices
                    .len()
                    .checked_mul(std::mem::size_of::<u32>())
                    .and_then(|index_bytes| bytes.checked_add(index_bytes))
            })
            .ok_or("scene upload byte count overflow")?;
        geometry_bytes = geometry_bytes
            .checked_add(object_bytes)
            .ok_or("scene upload byte count overflow")?;
    }
    if scene.objects.len() > 8
        || vertices > 400_004
        || indices > 600_036
        || geometry_bytes > VIEW_MAX_MESH_BYTES
    {
        return Err(format!(
            "view scene exceeds CLI geometry budget (objects {}, vertices {vertices}, indices {indices}, upload bytes {geometry_bytes})",
            scene.objects.len()
        ));
    }
    Ok(())
}

fn make_report(
    window: &daedalus_renderer::WindowReport,
    bridge: &BridgeReport,
    shutdown: &LiveShutdownReport,
    dropped_input_events: u64,
    seed: u64,
    work_iterations: u32,
    capture: Value,
) -> Value {
    let camera = |camera: &Camera| {
        json!({
            "position": camera.position,
            "target": camera.target,
            "up": camera.up,
            "vertical_fov_radians": camera.vertical_fov_radians,
            "near": camera.near,
            "far": camera.far,
        })
    };
    let frame_stats = &window.frame_intervals;
    json!({
        "report_version": 1,
        "session": {
            "initial_seed": seed,
            "work_iterations": work_iterations,
            "terrain_chunk_dimensions": [16, 16, 16],
            "initial_entity": bridge.initial_entity,
            "last_observed_entity": bridge.final_entity,
            "last_observed_tick": bridge.last_observed_tick,
            "cpu_budget": bridge.cpu_budget,
            "final_entity_translation": shutdown.final_entity_translation,
            "termination": shutdown.termination,
            "shutdown_outcome": if matches!(&shutdown.termination, daedalus_runtime::LiveTermination::StopRequested) { "stop_requested" } else { "fatal" },
            "terrain_job_totals": {
                "started": shutdown.stats.terrain_jobs_started,
                "completed": shutdown.stats.terrain_jobs_completed,
                "cancelled": shutdown.stats.terrain_jobs_cancelled,
                "stale": shutdown.stats.terrain_jobs_stale,
                "failed": shutdown.stats.terrain_jobs_failed,
                "latest_desired_generation": shutdown.stats.desired_generation,
                "latest_completed_generation": shutdown.stats.completed_generation,
            },
            "live_shutdown": shutdown,
            "automatic_initial_terrain_mesh": bridge.first_terrain_mesh,
        },
        "window": {
            "rendered_frames": window.rendered_frames,
            "loading_frames": window.loading_frames,
            "input_events": window.input_events,
            "loading_input_events": window.loading_input_events,
            "key_events": window.key_events,
            "loading_key_events": window.loading_key_events,
            "mouse_button_events": window.mouse_button_events,
            "loading_mouse_button_events": window.loading_mouse_button_events,
            "cursor_events": window.cursor_events,
            "loading_cursor_events": window.loading_cursor_events,
            "wheel_events": window.wheel_events,
            "loading_wheel_events": window.loading_wheel_events,
            "resize_events": window.resize_events,
            "loading_resize_events": window.loading_resize_events,
            "focus_events": window.focus_events,
            "loading_focus_events": window.loading_focus_events,
            "local_camera_changes": window.local_camera_changes,
            "initial_camera": camera(&validation_scene().camera),
            "final_camera": camera(&window.final_camera),
            "resize_count": window.resize_count,
            "final_width": window.final_width,
            "final_height": window.final_height,
            "frame_intervals_ms": {
                "sample_capacity": frame_stats.sample_capacity,
                "samples": frame_stats.samples_ms,
                "p50": frame_stats.p50_ms,
                "p95": frame_stats.p95_ms,
                "p99": frame_stats.p99_ms,
                "max": frame_stats.max_ms,
                "measurement": "CPU interval between successful surface presents; not GPU completion time",
            },
            "exit_reason": window_exit_name(window.exit_reason),
            "final_loading": window.final_loading,
            "message_source_disconnected": window.message_source_disconnected,
            "latest_presentation": window.latest_presentation.as_ref().map(|metadata| json!({
                "sequence": metadata.sequence,
                "tick": metadata.tick,
                "terrain_generation": metadata.generation,
            })),
            "renderer": window.renderer.as_ref().map(renderer_info_json),
            "title": window.window_title,
        },
        "bridge": {
            "frames_observed": bridge.frames_observed,
            "presentation_updates_sent": bridge.presentation_updates_sent,
            "presentation_updates_coalesced": bridge.presentation_updates_coalesced,
            "input_events_observed": bridge.input_events_observed,
            "input_observer_dropped_events": dropped_input_events,
            "focus_resets": bridge.focus_resets,
            "command_capacity": VIEW_COMMAND_CAPACITY,
            "result_capacity": VIEW_RESULT_CAPACITY,
            "scene_queue_capacity": VIEW_SCENE_CAPACITY,
            "max_scene_quads": VIEW_MAX_MESH_QUADS,
            "max_scene_geometry_bytes": VIEW_MAX_MESH_BYTES,
            "movement_commands_requested": bridge.movement_commands_requested,
            "movement_commands_accepted": bridge.movement_commands_accepted,
            "movement_command_submit_rejections": bridge.movement_command_submit_rejections,
            "movement_commands_applied": bridge.movement_commands_applied,
            "movement_command_rejections": bridge.movement_command_rejections,
            "terrain_commands_requested": bridge.terrain_commands_requested,
            "terrain_commands_accepted": bridge.terrain_commands_accepted,
            "terrain_command_submit_rejections": bridge.terrain_command_submit_rejections,
            "terrain_generations_completed": bridge.terrain_generations_completed,
            "terrain_generations_superseded": bridge.terrain_generations_superseded,
            "terrain_command_rejections": bridge.terrain_command_rejections,
            "command_results_received": bridge.command_results_received,
            "pending_command_results": bridge.pending_command_results.len(),
            "initial_entity_translation": bridge.initial_entity.as_ref().map(|entity| entity.transform.translation),
            "final_entity_translation": bridge.final_entity.as_ref().map(|entity| entity.transform.translation),
            "latest_successful_terrain_command": bridge.latest_terrain.as_ref().map(|terrain| json!({
                "generation": terrain.generation,
                "seed": terrain.seed,
                "quad_count": terrain.quad_count,
            })),
            "recent_command_results": bridge.recent_command_results,
        },
        "capture": capture,
    })
}

fn renderer_info_json(info: &daedalus_renderer::RendererInfo) -> Value {
    json!({
        "adapter_name": info.adapter_name,
        "backend": info.backend,
        "device_type": info.device_type,
    })
}

fn window_exit_name(reason: WindowExitReason) -> &'static str {
    match reason {
        WindowExitReason::CloseRequested => "close_requested",
        WindowExitReason::EscapePressed => "escape_pressed",
        WindowExitReason::TimeLimit => "time_limit",
        WindowExitReason::FrameLimit => "frame_limit",
        WindowExitReason::EventLoopReturned => "event_loop_returned",
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".into()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InputDebouncer, VIEW_MAX_MESH_QUADS, VIEW_MAX_RECENT_RESULTS, record_recent_result,
        terrain_objects, transform_matrix, validate_scene_budget,
    };
    use daedalus_content::voxel::GreedyMesh;
    use daedalus_content::voxel::{
        ChunkCoord, ChunkStamp, FaceDirection, MaterialSurface, MeshQuad, MeshRevisionTag,
        NeighborRevision,
    };
    use daedalus_core::Transform;
    use daedalus_renderer::{
        Material, Mesh, RenderObject, RenderScene, WindowInput, WindowKeyCode,
    };
    use serde_json::json;
    use std::collections::VecDeque;

    fn quad(face: FaceDirection) -> MeshQuad {
        MeshQuad {
            origin: [2, 3, 4],
            extent: [2, 5],
            face,
            material_id: 1,
            merge_group: 1,
            surface: MaterialSurface::Opaque,
        }
    }

    #[test]
    fn greedy_mesh_conversion_builds_indexed_triangles_with_correct_normals_and_winding() {
        let faces = [
            FaceDirection::NegativeX,
            FaceDirection::PositiveX,
            FaceDirection::NegativeY,
            FaceDirection::PositiveY,
            FaceDirection::NegativeZ,
            FaceDirection::PositiveZ,
        ];
        for face in faces {
            let mesh = GreedyMesh {
                tag: MeshRevisionTag {
                    coord: ChunkCoord { x: 0, y: 0, z: 0 },
                    source: ChunkStamp {
                        lifetime_generation: 0,
                        data_revision: 0,
                    },
                    neighbors: [
                        ChunkCoord { x: -1, y: 0, z: 0 },
                        ChunkCoord { x: 1, y: 0, z: 0 },
                        ChunkCoord { x: 0, y: -1, z: 0 },
                        ChunkCoord { x: 0, y: 1, z: 0 },
                        ChunkCoord { x: 0, y: 0, z: -1 },
                        ChunkCoord { x: 0, y: 0, z: 1 },
                    ]
                    .into_iter()
                    .map(|coord| NeighborRevision { coord, stamp: None })
                    .collect(),
                    job_generation: 1,
                },
                quads: vec![quad(face)],
            };
            let objects = terrain_objects(&mesh).unwrap();
            let converted = &objects[0].mesh;
            assert_eq!(converted.vertices.len(), 4);
            assert_eq!(converted.indices.len(), 6);
            let normal = converted.vertices[0].normal;
            let expected_normal = match face {
                FaceDirection::NegativeX => [-1.0, 0.0, 0.0],
                FaceDirection::PositiveX => [1.0, 0.0, 0.0],
                FaceDirection::NegativeY => [0.0, -1.0, 0.0],
                FaceDirection::PositiveY => [0.0, 1.0, 0.0],
                FaceDirection::NegativeZ => [0.0, 0.0, -1.0],
                FaceDirection::PositiveZ => [0.0, 0.0, 1.0],
            };
            assert_eq!(normal, expected_normal);
            for triangle in converted.indices.chunks_exact(3) {
                let a = converted.vertices[triangle[0] as usize].position;
                let b = converted.vertices[triangle[1] as usize].position;
                let c = converted.vertices[triangle[2] as usize].position;
                let cross = [
                    (b[1] - a[1]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[1] - a[1]),
                    (b[2] - a[2]) * (c[0] - a[0]) - (b[0] - a[0]) * (c[2] - a[2]),
                    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]),
                ];
                let orientation =
                    cross[0] * normal[0] + cross[1] * normal[1] + cross[2] * normal[2];
                assert!(
                    orientation > 0.0,
                    "triangle winding must agree with its face normal"
                );
            }
            assert!(converted.indices.iter().all(|index| (*index as usize) < 4));
        }
    }

    #[test]
    fn input_debouncer_accepts_edges_and_resets_on_focus_loss() {
        let mut debouncer = InputDebouncer::default();
        let key_down = WindowInput::Key {
            code: WindowKeyCode::KeyR,
            pressed: true,
        };
        assert!(matches!(
            debouncer.observe(&key_down),
            Some(daedalus_runtime::LiveCommand::RegenerateTerrain { .. })
        ));
        assert!(debouncer.observe(&key_down).is_none());
        debouncer.observe(&WindowInput::Focused(false));
        assert_eq!(debouncer.focus_resets, 1);
        assert!(debouncer.observe(&key_down).is_some());
        let Some(daedalus_runtime::LiveCommand::MoveEntity { delta }) =
            debouncer.observe(&WindowInput::Key {
                code: WindowKeyCode::ArrowUp,
                pressed: true,
            })
        else {
            panic!("ArrowUp should submit a real movement command");
        };
        assert_eq!(delta, [0.0, 0.0, -1.0]);
    }

    #[test]
    fn scene_conversion_respects_quad_and_upload_budgets() {
        assert_eq!(VIEW_MAX_MESH_QUADS * 120, 983_040);
        let mut history = VecDeque::with_capacity(VIEW_MAX_RECENT_RESULTS);
        for index in 0..=VIEW_MAX_RECENT_RESULTS {
            record_recent_result(&mut history, json!({"index": index}));
        }
        assert_eq!(history.len(), VIEW_MAX_RECENT_RESULTS);
        assert_eq!(history.front().unwrap()["index"], 1);
        let mut object_overflow = RenderScene::new();
        for _ in 0..9 {
            object_overflow.objects.push(RenderObject::new(
                Mesh::new(Vec::new(), Vec::new()),
                glam::Mat4::IDENTITY,
                Material {
                    base_color: [1.0; 3],
                    metallic: 0.0,
                    roughness: 1.0,
                    emissive: [0.0; 3],
                },
            ));
        }
        assert!(validate_scene_budget(&object_overflow).is_err());
        let empty_mesh = GreedyMesh {
            tag: MeshRevisionTag {
                coord: ChunkCoord { x: 0, y: 0, z: 0 },
                source: ChunkStamp {
                    lifetime_generation: 0,
                    data_revision: 0,
                },
                neighbors: Vec::new(),
                job_generation: 1,
            },
            quads: vec![],
        };
        assert!(terrain_objects(&empty_mesh).unwrap().is_empty());
        let oversized_mesh = GreedyMesh {
            quads: vec![quad(FaceDirection::PositiveY); VIEW_MAX_MESH_QUADS + 1],
            ..empty_mesh
        };
        assert!(terrain_objects(&oversized_mesh).is_err());
    }

    #[test]
    fn noninvertible_live_transform_is_rejected_before_rendering() {
        assert!(
            transform_matrix(Transform {
                scale: [0.0, 1.0, 1.0],
                ..Transform::IDENTITY
            })
            .is_err()
        );
    }
}
