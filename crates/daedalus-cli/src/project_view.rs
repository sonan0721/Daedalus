//! Native external-project viewer backed by the shared project control service.

mod activation;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use clap::Args;
use daedalus_content::project::PreparedProject;
use daedalus_content::scene::StableId;
use daedalus_core::EntityHandle;
use daedalus_renderer::{
    PacketAssetLimits, PacketPresentationWindowOutcome, PacketRendererLimits, PacketWindowMessage,
    ProjectRenderResources, WindowInput, WindowKeyCode, WindowOptions,
    run_packet_windowed_presented, window_input_channel,
};
use daedalus_runtime::{
    EngineConfig, NativeProjectPreparationOptions, NativeProjectPreparationService,
    PreparationWorkerShutdown, ProjectInfoPage, RequestPhase, RequestReadError, RequestTicket,
    SessionClockMode, SessionControlClient, SessionControlConfig, SessionControlLimits,
    SessionControlService, SessionEntityTarget, SessionOperation, SessionRequestResult,
    SessionSubmitError, SessionTermination, WaitStage,
};
use serde_json::{Value, json};

use super::project::{ProjectAudioArg, ProjectClockArg};
use super::project_view_control::{ProtocolController, StdioTransport};

const MAX_VIEW_WIDTH: u32 = 4_096;
const MAX_VIEW_HEIGHT: u32 = 4_096;
const MAX_VIEW_SECONDS: u32 = 3_600;
const MAX_VIEW_FRAMES: u64 = 1_000_000;
const PROJECT_VIEW_GENERATION: u64 = 1;
const INPUT_QUEUE_CAPACITY: usize = 1_024;
const PACKET_QUEUE_CAPACITY: usize = 1;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const COORDINATOR_SLEEP: Duration = Duration::from_millis(2);
const MAX_INPUT_EVENTS_PER_LOOP: usize = 64;

#[derive(Args)]
pub(super) struct ProjectViewArgs {
    /// External project directory.
    pub directory: PathBuf,
    /// Use fixed ticks only when protocol Step/F10 is explicitly requested.
    #[arg(long, value_enum, default_value = "realtime")]
    pub clock: ProjectClockArg,
    /// Explicitly load imported audio with an offline or native backend; loading never auto-plays.
    #[arg(long, value_enum)]
    pub audio: Option<ProjectAudioArg>,
    /// Attach bounded protocol-2 JSONL control to stdin/stdout.
    #[arg(long)]
    pub stdio_control: bool,
    /// Enable the native bounded live asset-preparation worker (requires --stdio-control).
    #[arg(long)]
    pub asset_preparation: bool,
    /// Use the native project host and managed generation consumer for this view.
    #[arg(long)]
    pub asset_activation: bool,
    /// Stable entity ID controlled by the arrow-key diagnostic action.
    #[arg(long)]
    pub controlled_entity: Option<String>,
    #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(64..=MAX_VIEW_WIDTH as i64))]
    pub width: u32,
    #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(64..=MAX_VIEW_HEIGHT as i64))]
    pub height: u32,
    /// Optional finite runtime limit in seconds.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=MAX_VIEW_SECONDS as i64))]
    pub seconds: Option<u32>,
    /// Optional finite successfully-presented-frame limit.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_VIEW_FRAMES))]
    pub frames: Option<u64>,
    /// Write the final bounded viewer/session report to this path.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

struct BindingMap {
    session: daedalus_core::SessionId,
    values: BTreeMap<StableId, EntityHandle>,
    first_page: ProjectInfoPage,
}

#[derive(Default)]
struct CoordinatorReport {
    observed_frames: u64,
    presented_candidates: u64,
    input_events: u64,
    arrow_actions_submitted: u64,
    arrow_actions_failed: u64,
    manual_steps_requested: u64,
    manual_steps_failed: u64,
    protocol_requests: u64,
    protocol_errors: u64,
    duplicate_submissions: u64,
    retained_protocol_tickets: usize,
    protocol_detach_reason: Option<String>,
    last_frame: Option<FrameIdentity>,
}

struct FrameIdentity {
    session: String,
    tick: String,
    sequence: String,
    project_generation: String,
    source_digest: String,
}

enum InputTicket {
    Get(RequestTicket, StableId, [f32; 3]),
    Set(RequestTicket),
}

pub(super) fn run(args: ProjectViewArgs) -> Result<(), Box<dyn Error>> {
    validate_args(&args)?;
    if args.asset_activation {
        return activation::run(args);
    }
    let clock = args.clock.runtime();
    if args.audio.is_some() && !cfg!(feature = "audio") {
        return Err(
            "audio support is not compiled into this CLI; rebuild with --features audio".into(),
        );
    }
    if args.audio == Some(ProjectAudioArg::Native) && !cfg!(feature = "native-audio") {
        return Err("native audio support is not compiled into this CLI; rebuild with --features native-audio".into());
    }
    let mut preparation_owner = if args.asset_preparation {
        Some(NativeProjectPreparationService::start(
            NativeProjectPreparationOptions::new(&args.directory),
        )?)
    } else {
        None
    };
    let project = if let Some(owner) = preparation_owner.as_ref() {
        Arc::clone(owner.initial_project())
    } else {
        Arc::new(super::project_assets::load_project_runtime(
            &args.directory,
        )?)
    };
    let control_limits = SessionControlLimits::default();
    let mut service_config = SessionControlConfig {
        engine: EngineConfig::default(),
        clock_mode: clock,
        limits: control_limits,
    };
    service_config.engine.snapshot_entity_capacity = service_config
        .engine
        .snapshot_entity_capacity
        .max(project.entities().len());
    let service_result = if let Some(owner) = preparation_owner.as_mut() {
        match owner.take_control_port() {
            Ok(port) => super::project::start_control_service_with_preparation(
                Arc::clone(&project),
                service_config,
                port,
                args.audio,
            ),
            Err(error) => Err(Box::new(error) as Box<dyn Error>),
        }
    } else {
        super::project::start_control_service(Arc::clone(&project), service_config, args.audio)
    };
    let service = match service_result {
        Ok(service) => service,
        Err(error) => {
            let _ = super::project::stop_preparation_worker(&mut preparation_owner);
            return Err(error);
        }
    };

    let run_result = run_window(&args, Arc::clone(&project), &service, clock, control_limits);
    service.request_stop();
    let shutdown = service.shutdown();
    let preparation_shutdown = super::project::stop_preparation_worker(&mut preparation_owner)?;
    let run = run_result?;
    let termination = shutdown.termination.clone();
    let report = build_report(&args, project.as_ref(), run, shutdown, preparation_shutdown);
    emit_report(&args, report)?;
    if let Some(error) = session_termination_error(&termination) {
        return Err(error.into());
    }
    Ok(())
}

fn session_termination_error(termination: &SessionTermination) -> Option<String> {
    match termination {
        SessionTermination::StopRequested => None,
        SessionTermination::EngineError(message) => Some(format!(
            "project session stopped after an engine error: {message}"
        )),
        SessionTermination::Panicked(message) => {
            Some(format!("project session actor panicked: {message}"))
        }
    }
}

fn validate_args(args: &ProjectViewArgs) -> Result<(), Box<dyn Error>> {
    if args.asset_activation && !args.stdio_control {
        return Err("--asset-activation requires --stdio-control for project view".into());
    }
    if args.asset_activation && args.audio.is_some() {
        return Err("--asset-activation cannot be combined with --audio".into());
    }
    if args.asset_preparation && !args.stdio_control {
        return Err("--asset-preparation requires --stdio-control for project view".into());
    }
    if !(64..=MAX_VIEW_WIDTH).contains(&args.width)
        || !(64..=MAX_VIEW_HEIGHT).contains(&args.height)
    {
        return Err("window width and height must be in 64..=4096".into());
    }
    if args
        .seconds
        .is_some_and(|seconds| seconds == 0 || seconds > MAX_VIEW_SECONDS)
    {
        return Err(format!("seconds must be in 1..={MAX_VIEW_SECONDS}").into());
    }
    if args
        .frames
        .is_some_and(|frames| frames == 0 || frames > MAX_VIEW_FRAMES)
    {
        return Err(format!("frames must be in 1..={MAX_VIEW_FRAMES}").into());
    }
    if args.stdio_control && args.report.is_none() {
        // This is allowed; final report is written to stderr to keep stdout protocol-only.
    }
    Ok(())
}

fn run_window(
    args: &ProjectViewArgs,
    project: Arc<PreparedProject>,
    service: &SessionControlService,
    clock: SessionClockMode,
    control_limits: SessionControlLimits,
) -> Result<WindowRun, Box<dyn Error>> {
    let control_client = service.attach()?;
    let binding_map = collect_bindings(&control_client)?;
    let controlled_entity = args
        .controlled_entity
        .as_ref()
        .map(|value| StableId::new(value.clone()))
        .transpose()?;
    if let Some(stable_id) = controlled_entity.as_ref()
        && !binding_map.values.contains_key(stable_id)
    {
        return Err(format!("controlled entity '{stable_id}' is not in the loaded project").into());
    }

    let limits = PacketRendererLimits::default();
    let asset_limits = PacketAssetLimits::default();
    let resources = ProjectRenderResources::prepare(
        project.as_ref(),
        PROJECT_VIEW_GENERATION,
        binding_map.session,
        &binding_map.values,
        limits,
    )?;
    let initial_frame = latest_frame(&control_client)?;
    if initial_frame.session() != binding_map.session {
        return Err("initial service frame belongs to another session".into());
    }
    let initial_packet = Arc::new(resources.extract(initial_frame.snapshot(), None)?);
    drop(initial_frame);

    let (input_sink, input_events) = window_input_channel(INPUT_QUEUE_CAPACITY)?;
    let (packet_tx, packet_rx) = mpsc::sync_channel(PACKET_QUEUE_CAPACITY);
    let protocol = if args.stdio_control {
        let client = service.attach()?;
        let transport = StdioTransport::start()?;
        Some((client, transport))
    } else {
        None
    };

    let worker_stop = Arc::new(AtomicBool::new(false));
    let worker_stop_thread = Arc::clone(&worker_stop);
    let controlled_for_worker = controlled_entity.clone();
    let audio_enabled = args.audio.is_some();
    let worker = thread::Builder::new()
        .name("daedalus-project-view-coordinator".into())
        .spawn(move || {
            coordinate(CoordinatorContext {
                client: control_client,
                protocol,
                clock,
                controlled_entity: controlled_for_worker,
                resources,
                input_events,
                packet_sender: packet_tx,
                stop: worker_stop_thread,
                first_page: binding_map.first_page,
                control_limits,
                audio_enabled,
            })
        })?;

    let options = WindowOptions {
        title: format!("Daedalus — {}", project.manifest().name),
        width: args.width,
        height: args.height,
        input_events: Some(input_sink),
        max_run_time: args
            .seconds
            .map(|seconds| Duration::from_secs(u64::from(seconds))),
        max_frames: args.frames,
        initial_loading: false,
        fail_on_presentation_disconnect: true,
        ..WindowOptions::default()
    };

    let window_result =
        run_packet_windowed_presented(initial_packet, packet_rx, options, limits, asset_limits);
    worker_stop.store(true, Ordering::Release);
    let coordinator = worker.join().map_err(|payload| {
        format!(
            "project view coordinator panicked: {}",
            panic_message(payload)
        )
    })?;
    let outcome = window_result?;
    Ok(WindowRun {
        outcome,
        coordinator,
    })
}

struct WindowRun {
    outcome: PacketPresentationWindowOutcome,
    coordinator: CoordinatorReport,
}

struct CoordinatorContext {
    client: SessionControlClient,
    protocol: Option<(SessionControlClient, StdioTransport)>,
    clock: SessionClockMode,
    controlled_entity: Option<StableId>,
    resources: ProjectRenderResources,
    input_events: std::sync::mpsc::Receiver<WindowInput>,
    packet_sender: SyncSender<PacketWindowMessage>,
    stop: Arc<AtomicBool>,
    first_page: ProjectInfoPage,
    control_limits: SessionControlLimits,
    audio_enabled: bool,
}

fn collect_bindings(client: &SessionControlClient) -> Result<BindingMap, Box<dyn Error>> {
    let mut offset = 0usize;
    let mut session = None;
    let mut first_page = None;
    let mut values = BTreeMap::new();
    loop {
        let result = submit_wait(
            client,
            SessionOperation::ProjectInfo {
                offset,
                limit: Some(1024),
            },
            CONTROL_TIMEOUT,
        )?;
        let SessionRequestResult::ProjectInfo(page) = result else {
            return Err("project_info returned an unexpected result type".into());
        };
        if let Some(expected) = session {
            if expected != page.session
                || first_page.as_ref().is_some_and(|first: &ProjectInfoPage| {
                    first.source_digest != page.source_digest
                        || first.entity_count != page.entity_count
                })
            {
                return Err(
                    "project_info pages changed session or source identity during startup".into(),
                );
            }
        } else {
            session = Some(page.session);
        }
        for binding in &page.entities {
            if values.insert(binding.id.clone(), binding.handle).is_some() {
                return Err(format!(
                    "duplicate authored ID '{}' in project_info pages",
                    binding.id
                )
                .into());
            }
        }
        if first_page.is_none() {
            first_page = Some(page.clone());
        }
        match page.next_offset {
            Some(next) if next > offset && next <= 16_384 => offset = next,
            Some(_) => return Err("project_info returned an invalid next_offset".into()),
            None => {
                if values.len() != page.entity_count {
                    return Err(
                        "project_info page count does not match authored entity count".into(),
                    );
                }
                return Ok(BindingMap {
                    session: session.expect("first page establishes session"),
                    values,
                    first_page: first_page.expect("first page exists"),
                });
            }
        }
    }
}

fn latest_frame(
    client: &SessionControlClient,
) -> Result<Arc<daedalus_runtime::SessionFrame>, Box<dyn Error>> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    loop {
        match client.try_latest_frame() {
            Ok(Some(frame)) => return Ok(frame),
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(daedalus_runtime::FrameReadError::Busy) => thread::sleep(Duration::from_millis(1)),
            Err(error) => return Err(Box::new(error)),
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the startup frame".into());
        }
    }
}

fn submit_wait(
    client: &SessionControlClient,
    operation: SessionOperation,
    timeout: Duration,
) -> Result<SessionRequestResult, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    let mut ticket = loop {
        match client.try_submit(operation.clone()) {
            Ok(ticket) => break ticket,
            Err(SessionSubmitError::Busy) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1))
            }
            Err(error) => return Err(Box::new(error)),
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for project control admission".into());
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let progress = ticket.wait_for(WaitStage::Terminal, remaining)?;
    let result = progress
        .terminal()
        .ok_or("terminal request has no terminal result")?
        .clone();
    ticket.acknowledge()?;
    Ok(result?)
}

fn coordinate(context: CoordinatorContext) -> CoordinatorReport {
    let CoordinatorContext {
        client,
        protocol: protocol_pair,
        clock,
        controlled_entity,
        resources,
        input_events,
        packet_sender,
        stop,
        first_page,
        control_limits,
        audio_enabled,
    } = context;
    let mut report = CoordinatorReport::default();
    let mut protocol = protocol_pair.map(|(client, transport)| {
        ProtocolController::new(transport, client, clock, control_limits, audio_enabled)
    });
    let mut last_observed_sequence = 0;
    let mut pending_packet: Option<Arc<daedalus_renderer::RenderPacket>> = None;
    let mut close_observed = false;
    let mut close_pending = false;
    let mut input_ticket: Option<InputTicket> = None;
    let mut step_ticket: Option<RequestTicket> = None;
    let first_session = first_page.session;
    let first_digest = first_page.source_digest.clone();

    while !stop.load(Ordering::Acquire) {
        if client.is_stopping() {
            let _ = packet_sender.try_send(PacketWindowMessage::Abort(
                "project session stopped unexpectedly".into(),
            ));
            break;
        }
        if let Some(protocol) = protocol.as_mut() {
            protocol.pump();
        }
        let closing = protocol.as_ref().is_some_and(ProtocolController::closing);
        if closing && !close_observed {
            close_observed = true;
            close_pending = true;
        }
        send_pending_close(&packet_sender, &mut close_pending);
        for _ in 0..MAX_INPUT_EVENTS_PER_LOOP {
            let event = match input_events.try_recv() {
                Ok(event) => event,
                Err(
                    std::sync::mpsc::TryRecvError::Empty
                    | std::sync::mpsc::TryRecvError::Disconnected,
                ) => break,
            };
            report.input_events = report.input_events.saturating_add(1);
            if !closing {
                handle_window_input(
                    event,
                    clock,
                    controlled_entity.as_ref(),
                    &client,
                    &mut input_ticket,
                    &mut step_ticket,
                    &mut report,
                );
            }
        }
        progress_input_ticket(&client, &mut input_ticket, &mut report, !closing);
        progress_step_ticket(&mut step_ticket, &mut report);

        if closing {
            pending_packet = None;
        } else if let Some(packet) = pending_packet.take() {
            match packet_sender.try_send(PacketWindowMessage::Present {
                packet: Arc::clone(&packet),
                loading: false,
            }) {
                Ok(()) => {
                    report.presented_candidates = report.presented_candidates.saturating_add(1)
                }
                Err(TrySendError::Full(PacketWindowMessage::Present { packet, .. })) => {
                    pending_packet = Some(packet)
                }
                Err(TrySendError::Full(_)) => pending_packet = Some(packet),
                Err(TrySendError::Disconnected(_)) => break,
            }
        }

        if !closing {
            match client.try_latest_frame() {
                Ok(Some(frame)) if frame.sequence() > last_observed_sequence => {
                    if frame.session() != first_session || frame.source_digest() != first_digest {
                        let _ = packet_sender.try_send(PacketWindowMessage::Abort(
                            "session frame source identity changed".into(),
                        ));
                        report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
                        break;
                    }
                    match resources.extract(frame.snapshot(), None) {
                        Ok(packet) => {
                            let sequence = packet.sequence();
                            report.last_frame = Some(FrameIdentity {
                                session: packet.session().get().to_string(),
                                tick: packet.tick().get().to_string(),
                                sequence: packet.sequence().to_string(),
                                project_generation: packet.project_generation().to_string(),
                                source_digest: packet.source_digest().to_owned(),
                            });
                            report.observed_frames = report.observed_frames.saturating_add(1);
                            last_observed_sequence = sequence;
                            pending_packet = Some(Arc::new(packet));
                        }
                        Err(error) => {
                            let _ = packet_sender
                                .try_send(PacketWindowMessage::Abort(error.to_string()));
                            break;
                        }
                    }
                }
                Ok(Some(_)) | Ok(None) | Err(daedalus_runtime::FrameReadError::Busy) => {}
                Err(error) => {
                    let _ = packet_sender.try_send(PacketWindowMessage::Abort(error.to_string()));
                    break;
                }
            }
        }
        thread::sleep(COORDINATOR_SLEEP);
    }
    if let Some(protocol) = protocol {
        report.protocol_requests = protocol.accepted_requests();
        report.protocol_errors = protocol.errors();
        report.duplicate_submissions = protocol.duplicate_submissions();
        report.retained_protocol_tickets = protocol.retained_tickets();
        report.protocol_detach_reason = protocol.detach_reason().map(str::to_owned);
    }
    report
}

fn handle_window_input(
    event: WindowInput,
    clock: SessionClockMode,
    controlled_entity: Option<&StableId>,
    client: &SessionControlClient,
    active: &mut Option<InputTicket>,
    step: &mut Option<RequestTicket>,
    report: &mut CoordinatorReport,
) {
    let WindowInput::Key {
        code,
        pressed: true,
    } = event
    else {
        return;
    };
    if code == WindowKeyCode::F10 {
        if clock != SessionClockMode::Manual {
            return;
        }
        if step.is_some() {
            report.manual_steps_failed = report.manual_steps_failed.saturating_add(1);
            return;
        }
        match client.try_submit(SessionOperation::Step { count: 1 }) {
            Ok(ticket) => {
                *step = Some(ticket);
                report.manual_steps_requested = report.manual_steps_requested.saturating_add(1);
            }
            Err(_) => report.manual_steps_failed = report.manual_steps_failed.saturating_add(1),
        }
        return;
    }
    let Some(stable_id) = controlled_entity else {
        return;
    };
    let delta = match code {
        WindowKeyCode::ArrowLeft => [-0.25, 0.0, 0.0],
        WindowKeyCode::ArrowRight => [0.25, 0.0, 0.0],
        WindowKeyCode::ArrowUp => [0.0, 0.0, -0.25],
        WindowKeyCode::ArrowDown => [0.0, 0.0, 0.25],
        _ => return,
    };
    if active.is_some() {
        report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
        return;
    }
    match client.try_submit(SessionOperation::GetTransform {
        target: SessionEntityTarget::StableId(stable_id.clone()),
    }) {
        Ok(ticket) => *active = Some(InputTicket::Get(ticket, stable_id.clone(), delta)),
        Err(_) => report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1),
    }
}

fn progress_input_ticket(
    client: &SessionControlClient,
    active: &mut Option<InputTicket>,
    report: &mut CoordinatorReport,
    allow_mutation: bool,
) {
    let Some(mut ticket) = active.take() else {
        return;
    };
    let (waiting, current) = match &mut ticket {
        InputTicket::Get(ticket, _, _) | InputTicket::Set(ticket) => match ticket.try_state() {
            Ok(progress) if progress.phase() == RequestPhase::Terminal => {
                (false, progress.terminal().cloned())
            }
            Ok(_) | Err(RequestReadError::Busy) => (true, None),
            Err(_) => (false, None),
        },
    };
    if waiting {
        *active = Some(ticket);
        return;
    }
    let result = current;
    match ticket {
        InputTicket::Get(mut ticket, stable_id, delta) => {
            if !allow_mutation {
                let _ = ticket.acknowledge();
                report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
            } else if let Some(Ok(SessionRequestResult::Transform(state))) = result {
                let mut transform = state.entity.transform;
                for (index, delta) in delta.into_iter().enumerate() {
                    transform.translation[index] += delta;
                }
                match client.try_submit(SessionOperation::SetTransform {
                    target: SessionEntityTarget::StableId(stable_id),
                    command_id: None,
                    expected_revision: state.entity.revision,
                    apply_at: None,
                    transform,
                }) {
                    Ok(write) => {
                        let _ = ticket.acknowledge();
                        *active = Some(InputTicket::Set(write));
                        report.arrow_actions_submitted =
                            report.arrow_actions_submitted.saturating_add(1);
                    }
                    Err(_) => {
                        let _ = ticket.acknowledge();
                        report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
                    }
                }
            } else {
                let _ = ticket.acknowledge();
                report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
            }
        }
        InputTicket::Set(mut ticket) => {
            let applied = matches!(
                result,
                Some(Ok(SessionRequestResult::Command(command)))
                    if matches!(command.result.outcome, daedalus_world::CommandOutcome::Applied { .. })
            );
            if !applied {
                report.arrow_actions_failed = report.arrow_actions_failed.saturating_add(1);
            }
            let _ = ticket.acknowledge();
        }
    }
}

fn progress_step_ticket(active: &mut Option<RequestTicket>, report: &mut CoordinatorReport) {
    let Some(mut ticket) = active.take() else {
        return;
    };
    match ticket.try_state() {
        Ok(progress) if progress.phase() == RequestPhase::Terminal => {
            if progress.terminal().is_none_or(Result::is_err) {
                report.manual_steps_failed = report.manual_steps_failed.saturating_add(1);
            }
            let _ = ticket.acknowledge();
        }
        Ok(_) | Err(RequestReadError::Busy) => *active = Some(ticket),
        Err(_) => report.manual_steps_failed = report.manual_steps_failed.saturating_add(1),
    }
}

fn build_report(
    args: &ProjectViewArgs,
    project: &PreparedProject,
    run: WindowRun,
    shutdown: daedalus_runtime::SessionShutdownReport,
    preparation_shutdown: Option<PreparationWorkerShutdown>,
) -> Value {
    let outcome = run.outcome;
    let asset_resources =
        super::project_assets::asset_resource_stats_json(outcome.asset_resource_stats());
    let packet_outcome = outcome.packet_outcome();
    let presented = outcome.last_presented().map(|presentation| {
        let frame = presentation.packet();
        json!({
            "session":frame.session.get().to_string(), "tick":frame.tick.get().to_string(),
            "sequence":frame.sequence.to_string(), "project_generation":frame.project_generation.to_string(),
            "source_digest":frame.source_digest, "physical_width":frame.physical_width,
            "physical_height":frame.physical_height,
            "camera":super::project_assets::camera_json(&frame.camera),
            "render_view":super::project_assets::render_view_json(presentation.view()),
        })
    });
    let final_packet = &packet_outcome.final_packet;
    let frame_intervals = json!({
        "sample_capacity":packet_outcome.report.frame_intervals.sample_capacity,
        "samples_ms":packet_outcome.report.frame_intervals.samples_ms,
        "p50_ms":packet_outcome.report.frame_intervals.p50_ms,
        "p95_ms":packet_outcome.report.frame_intervals.p95_ms,
        "p99_ms":packet_outcome.report.frame_intervals.p99_ms,
        "max_ms":packet_outcome.report.frame_intervals.max_ms,
    });
    let latest_presentation = packet_outcome
        .report
        .latest_presentation
        .as_ref()
        .map(|metadata| {
            json!({
                "sequence":metadata.sequence.to_string(),
                "tick":metadata.tick.to_string(),
                "generation":metadata.generation.to_string(),
            })
        });
    let final_camera = super::project_assets::camera_json(&packet_outcome.report.final_camera);
    let window = json!({
        "exit_reason":format!("{:?}", packet_outcome.report.exit_reason),
        "rendered_frames":packet_outcome.report.rendered_frames,
        "loading_frames":packet_outcome.report.loading_frames,
        "input_events":packet_outcome.report.input_events,
        "loading_input_events":packet_outcome.report.loading_input_events,
        "key_events":packet_outcome.report.key_events,
        "loading_key_events":packet_outcome.report.loading_key_events,
        "mouse_button_events":packet_outcome.report.mouse_button_events,
        "loading_mouse_button_events":packet_outcome.report.loading_mouse_button_events,
        "cursor_events":packet_outcome.report.cursor_events,
        "loading_cursor_events":packet_outcome.report.loading_cursor_events,
        "wheel_events":packet_outcome.report.wheel_events,
        "loading_wheel_events":packet_outcome.report.loading_wheel_events,
        "resize_events":packet_outcome.report.resize_events,
        "loading_resize_events":packet_outcome.report.loading_resize_events,
        "focus_events":packet_outcome.report.focus_events,
        "loading_focus_events":packet_outcome.report.loading_focus_events,
        "local_camera_changes":packet_outcome.report.local_camera_changes,
        "resize_count":packet_outcome.report.resize_count,
        "physical_width":packet_outcome.report.final_width,
        "physical_height":packet_outcome.report.final_height,
        "frame_intervals":frame_intervals,
        "message_source_disconnected":packet_outcome.report.message_source_disconnected,
        "final_loading":packet_outcome.report.final_loading,
        "latest_presentation":latest_presentation,
        "window_title":packet_outcome.report.window_title,
        "final_camera":final_camera,
        "final_render_view":super::project_assets::render_view_json(outcome.final_view()),
    });
    let resources =
        super::project_assets::packet_resource_stats_json(&packet_outcome.resource_stats);
    let coordinator = json!({
        "observed_frames":run.coordinator.observed_frames,
        "presented_candidates":run.coordinator.presented_candidates,
        "input_events":run.coordinator.input_events,
        "arrow_actions_submitted":run.coordinator.arrow_actions_submitted,
        "arrow_actions_failed":run.coordinator.arrow_actions_failed,
        "manual_steps_requested":run.coordinator.manual_steps_requested,
        "manual_steps_failed":run.coordinator.manual_steps_failed,
        "protocol_requests":run.coordinator.protocol_requests,
        "protocol_errors":run.coordinator.protocol_errors,
        "duplicate_submissions":run.coordinator.duplicate_submissions,
        "retained_protocol_tickets":run.coordinator.retained_protocol_tickets,
        "protocol_detach_reason":run.coordinator.protocol_detach_reason,
        "last_frame":run.coordinator.last_frame.as_ref().map(|frame| json!({
            "session":frame.session,
            "tick":frame.tick,
            "sequence":frame.sequence,
            "project_generation":frame.project_generation,
            "source_digest":frame.source_digest,
        })),
    });
    let mut report = json!({
        "command":"project_view",
        "project_name":project.manifest().name,
        "source_digest":project.digest(),
        "source_schema_version":project.scene().schema_version,
        "source_scene_revision":project.scene().revision.to_string(),
        "source_entity_count":project.entities().len(),
        "session":shutdown.session.map(|session| session.get().to_string()),
        "authority_final_tick":shutdown.final_tick.get().to_string(),
        "termination":format!("{:?}", shutdown.termination),
        "shutdown":{"queued_requests_stopped":shutdown.queued_requests_stopped,"commands_stopped_not_applied":shutdown.commands_stopped_not_applied,"terminal_unacknowledged":shutdown.terminal_unacknowledged,"abandoned_tickets":shutdown.abandoned_tickets,"result_credit_tickets":shutdown.result_credit_tickets,"result_credit_bytes":shutdown.result_credit_bytes},
        "accepted_packet":{"session":final_packet.session().get().to_string(),"tick":final_packet.tick().get().to_string(),"sequence":final_packet.sequence().to_string(),"project_generation":final_packet.project_generation().to_string(),"source_digest":final_packet.source_digest()},
        "last_presented":presented,
        "window":window,
        "resources":resources,
        "asset_resources":asset_resources,
        "coordinator":coordinator,
        "limits":{"width":args.width,"height":args.height,"seconds":args.seconds,"frames":args.frames},
        "live_capture":false,"mcp":false,
    });
    if let Some(status) = preparation_shutdown {
        report["asset_preparation_worker"] =
            json!(super::project::preparation_shutdown_name(status));
    }
    report
}

fn emit_report(args: &ProjectViewArgs, report: Value) -> Result<(), Box<dyn Error>> {
    let bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > 1024 * 1024 {
        return Err("project view report exceeds 1 MiB".into());
    }
    if let Some(path) = &args.report {
        fs::write(path, &bytes)?;
        if !args.stdio_control {
            println!("{}", String::from_utf8_lossy(&bytes));
        }
    } else if args.stdio_control {
        eprintln!("{}", String::from_utf8_lossy(&bytes));
    } else {
        println!("{}", String::from_utf8_lossy(&bytes));
    }
    Ok(())
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "unknown panic payload".into())
}

fn send_pending_close(sender: &SyncSender<PacketWindowMessage>, pending: &mut bool) {
    if !*pending {
        return;
    }
    match sender.try_send(PacketWindowMessage::Close) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => *pending = false,
        Err(TrySendError::Full(_)) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{TryRecvError, sync_channel};

    use daedalus_renderer::PacketWindowMessage;
    use daedalus_runtime::SessionTermination;

    use super::{ProjectViewArgs, send_pending_close, session_termination_error, validate_args};

    #[test]
    fn normal_session_stop_succeeds_but_engine_failure_and_panic_fail_closed() {
        assert_eq!(
            session_termination_error(&SessionTermination::StopRequested),
            None
        );
        assert!(
            session_termination_error(&SessionTermination::EngineError("lost".into()))
                .is_some_and(|error| error.contains("lost"))
        );
        assert!(
            session_termination_error(&SessionTermination::Panicked("boom".into()))
                .is_some_and(|error| error.contains("boom"))
        );
    }

    #[test]
    fn asset_activation_requires_stdio_control_before_project_io() {
        let args = ProjectViewArgs {
            directory: std::path::PathBuf::from("/definitely/not/a/project"),
            clock: crate::project::ProjectClockArg::Realtime,
            audio: None,
            stdio_control: false,
            asset_preparation: false,
            asset_activation: true,
            controlled_entity: None,
            width: 1280,
            height: 720,
            seconds: None,
            frames: None,
            report: None,
        };
        let error = validate_args(&args).unwrap_err().to_string();
        assert!(error.contains("--stdio-control"));
    }

    #[test]
    fn asset_activation_rejects_audio_before_project_io() {
        let args = ProjectViewArgs {
            directory: std::path::PathBuf::from("/definitely/not/a/project"),
            clock: crate::project::ProjectClockArg::Realtime,
            audio: Some(crate::project::ProjectAudioArg::Offline),
            stdio_control: true,
            asset_preparation: false,
            asset_activation: true,
            controlled_entity: None,
            width: 1280,
            height: 720,
            seconds: None,
            frames: None,
            report: None,
        };
        let error = validate_args(&args).unwrap_err().to_string();
        assert!(error.contains("cannot be combined with --audio"));
    }

    #[test]
    fn asset_preparation_flag_is_allowed_with_activation() {
        let args = ProjectViewArgs {
            directory: std::path::PathBuf::from("/definitely/not/a/project"),
            clock: crate::project::ProjectClockArg::Realtime,
            audio: None,
            stdio_control: true,
            asset_preparation: true,
            asset_activation: true,
            controlled_entity: None,
            width: 1280,
            height: 720,
            seconds: None,
            frames: None,
            report: None,
        };
        assert!(validate_args(&args).is_ok());
    }

    #[test]
    fn saturated_window_queue_keeps_one_close_intent_until_it_can_send() {
        let (sender, receiver) = sync_channel(1);
        sender
            .try_send(PacketWindowMessage::SetLoading(true))
            .expect("occupy the bounded packet slot");
        let mut pending = true;

        send_pending_close(&sender, &mut pending);
        assert!(pending, "full queue retains the one close intent");
        assert!(matches!(
            receiver.try_recv(),
            Ok(PacketWindowMessage::SetLoading(true))
        ));

        send_pending_close(&sender, &mut pending);
        assert!(!pending, "successful send consumes the one close intent");
        send_pending_close(&sender, &mut pending);
        assert!(matches!(
            receiver.try_recv(),
            Ok(PacketWindowMessage::Close)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }
}
