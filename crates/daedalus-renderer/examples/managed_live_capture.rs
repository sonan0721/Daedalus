//! Opt-in native proof for the renderer-owned managed-window capture return.
//!
//! The project and snapshot are deliberately synthetic. This example verifies
//! renderer presentation/readback ownership; it does not prove runtime authority.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{Cursor, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use daedalus_content::{prepare_project_from_bytes, project::ProjectLimits};
use daedalus_core::{
    EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
};
use daedalus_renderer::{
    Camera, CpuFrameHandle, CpuGenerationId, ManagedCaptureClient, ManagedCaptureKey,
    ManagedCapturePixels, ManagedCaptureReadback, ManagedCaptureRequest, ManagedCaptureReturn,
    ManagedCaptureSource, ManagedCaptureState, ManagedCaptureSubmitError,
    ManagedCaptureSurfaceFormat, ManagedCaptureWorker, ManagedCaptureWorkerError,
    ManagedPacketWindowClient, ManagedPacketWindowInput, ManagedPacketWindowOptions,
    ManagedPresentedView, ManagedStageEvent, ManagedStageKey, ManagedWindowRead, ManagedWindowRun,
    PacketCpuLimits, PacketCpuOwner, PacketStageInput, PacketStageLimits, PacketStageState,
    RenderView, SourcePin, WindowGpuOwner, WindowInputPolicy, WindowOptions,
    managed_packet_window_capture_channel, run_packet_windowed_managed_with_input_policy,
};
use sha2::{Digest, Sha256};

const WINDOW_DEADLINE: Duration = Duration::from_secs(50);
const CAPTURE_DEADLINE: Duration = Duration::from_secs(32);
const WORKER_RETURN_DEADLINE: Duration = Duration::from_secs(55);
const DRAIN_DEADLINE: Duration = Duration::from_secs(18);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const CLEAR_COLOR: [f32; 4] = [0.03, 0.04, 0.06, 1.0];
const OWNER_AWAITING: u8 = 0;
const OWNER_HELD: u8 = 1;
const OWNER_DRAINED: u8 = 2;
const OWNER_RETAINED_UNRESOLVED: u8 = 3;

type HarnessResult<T> = Result<T, HarnessError>;

#[derive(Debug)]
struct HarnessError(String);

impl HarnessError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for HarnessError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scenario {
    Capture,
    CloseHeld,
    WorkerDrop,
}

impl Scenario {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "capture" => Some(Self::Capture),
            "close-held" => Some(Self::CloseHeld),
            "worker-drop" => Some(Self::WorkerDrop),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::CloseHeld => "close-held",
            Self::WorkerDrop => "worker-drop",
        }
    }
}

struct Args {
    out_dir: PathBuf,
    scenario: Scenario,
}

struct Fixture {
    cpu_owner: PacketCpuOwner,
    generation: CpuGenerationId,
    frame: Arc<CpuFrameHandle>,
    initial_stage: PacketStageInput,
    initial_source: ManagedCaptureSource,
}

struct WorkerContext {
    scenario: Scenario,
    out_dir: PathBuf,
    cpu_owner: PacketCpuOwner,
    generation: CpuGenerationId,
    frame: Arc<CpuFrameHandle>,
    initial_source: ManagedCaptureSource,
    capture_client: ManagedCaptureClient,
    capture_worker: Option<ManagedCaptureWorker>,
    window_client: ManagedPacketWindowClient,
    control: daedalus_renderer::ManagedPacketWindowControl,
    preflight: SyncSender<Result<(), String>>,
    held_readback: Option<ManagedCaptureReadback>,
}

impl WorkerContext {
    fn capture_worker(&self) -> &ManagedCaptureWorker {
        self.capture_worker
            .as_ref()
            .expect("capture worker is present until explicit detach")
    }

    fn detach_and_drop_capture_worker(&mut self) {
        if let Some(worker) = self.capture_worker.take() {
            worker.request_detach();
            drop(worker);
        }
    }
}

struct WindowSummary {
    outcome_error: Option<String>,
    exit_reason: Option<String>,
    lifecycle: String,
    rendered_frames: u64,
    last_presented: Option<ManagedPresentedView>,
    renderer: Option<daedalus_renderer::RendererInfo>,
}

struct CaptureRecord {
    key: u64,
    file_name: String,
    view: ManagedPresentedView,
    surface_format: ManagedCaptureSurfaceFormat,
    rgba_sha256: String,
    png_sha256: String,
    decoded_matches_rgba: bool,
    background_sample: PixelSample,
    overlay_sample: PixelSample,
    red_mesh_sample: PixelSample,
    red_mesh_pixel_count: usize,
}

#[derive(Clone)]
struct PixelSample {
    x: u32,
    y: u32,
    rgba: [u8; 4],
}

struct WorkerReport {
    success: bool,
    json: String,
}

struct ReportData {
    captures: Vec<CaptureRecord>,
    observed_view: Option<ManagedPresentedView>,
    capture_ready: Option<daedalus_renderer::ManagedCaptureReadyInfo>,
    readback_submitted: bool,
    drain_observations: Vec<String>,
    cleanup_complete: bool,
    unresolved_gpu_owner_retained: bool,
    window: Option<WindowSummary>,
    error: Option<String>,
}

/// A returned owner is only dropped after its nonblocking drain reports
/// `Drained`. If an unwind or deadline prevents that proof, this guard leaks
/// the owner on the designated worker instead of dropping live GPU roots.
struct WorkerGpuOwner {
    owner: Option<WindowGpuOwner>,
    disposition: Arc<AtomicU8>,
}

impl WorkerGpuOwner {
    fn new(owner: WindowGpuOwner, disposition: Arc<AtomicU8>) -> Self {
        disposition.store(OWNER_HELD, Ordering::Release);
        Self {
            owner: Some(owner),
            disposition,
        }
    }

    fn get_mut(&mut self) -> &mut WindowGpuOwner {
        self.owner
            .as_mut()
            .expect("worker GPU owner remains present until a proven drain")
    }

    fn drop_after_drained(mut self) {
        self.disposition.store(OWNER_DRAINED, Ordering::Release);
        drop(self.owner.take());
    }

    fn retain_unresolved(mut self) {
        self.disposition
            .store(OWNER_RETAINED_UNRESOLVED, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            std::mem::forget(owner);
        }
    }
}

impl Drop for WorkerGpuOwner {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            // A panic during cleanup must not release a device owner whose
            // queue/capture work has not been proven drained.
            self.disposition
                .store(OWNER_RETAINED_UNRESOLVED, Ordering::Release);
            std::mem::forget(owner);
        }
    }
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("managed_live_capture: {error}");
            std::process::exit(1);
        }
    }
}

fn join_reported_worker(worker: thread::JoinHandle<()>) -> HarnessResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err(HarnessError::new(
                "capture owner sent its report but did not finish within the bounded join window",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
    worker
        .join()
        .map_err(|_| HarnessError::new("capture owner thread panicked after reporting"))
}

fn run() -> HarnessResult<bool> {
    let args = parse_args()?;
    let fixture = build_fixture()?;
    let Fixture {
        cpu_owner,
        generation,
        frame,
        initial_stage,
        initial_source,
    } = fixture;
    let (window_client, capture_client, server) = managed_packet_window_capture_channel();
    let capture_worker = capture_client
        .register_worker()
        .map_err(|error| HarnessError::new(format!("register capture worker: {error}")))?;
    let control = window_client.control();
    let (owner_tx, owner_rx) = mpsc::sync_channel(1);
    let (summary_tx, summary_rx) = mpsc::sync_channel(1);
    let (report_tx, report_rx) = mpsc::sync_channel(1);
    let (preflight_tx, preflight_rx) = mpsc::sync_channel(1);
    let owner_disposition = Arc::new(AtomicU8::new(OWNER_AWAITING));
    let window_started = Arc::new(AtomicBool::new(false));
    let worker_disposition = Arc::clone(&owner_disposition);
    let worker_window_started = Arc::clone(&window_started);

    let context = WorkerContext {
        scenario: args.scenario,
        out_dir: args.out_dir.clone(),
        cpu_owner,
        generation,
        frame,
        initial_source,
        capture_client,
        capture_worker: Some(capture_worker),
        window_client,
        control,
        preflight: preflight_tx,
        held_readback: None,
    };
    let worker_scenario = args.scenario;
    let worker_thread = thread::Builder::new()
        .name("managed-live-capture-owner".into())
        .spawn(move || {
            // Keep the owner receiver outside the per-scenario unwind boundary.
            // If scenario work panics, this same worker still receives and
            // drains the renderer owner after the main-thread window returns.
            let result = catch_unwind(AssertUnwindSafe(|| {
                worker_entry(
                    context,
                    &owner_rx,
                    &summary_rx,
                    Arc::clone(&worker_disposition),
                )
            }));
            let report = match result {
                Ok(report) => report,
                Err(_) => emergency_report_after_panic(
                    &owner_rx,
                    &owner_disposition,
                    &worker_window_started,
                    worker_scenario,
                ),
            };
            let _ = report_tx.send(report);
        })
        .map_err(|error| HarnessError::new(format!("start capture owner: {error}")))?;

    match preflight_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let report = report_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|recv_error| HarnessError::new(format!("output preflight failed ({error}) and worker report was unavailable: {recv_error}")))?;
            join_reported_worker(worker_thread)?;
            println!("{}", report.json);
            return Ok(false);
        }
        Err(RecvTimeoutError::Timeout) => {
            return Err(HarnessError::new(
                "capture worker did not finish bounded output-directory preflight",
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(HarnessError::new(
                "capture worker stopped during output-directory preflight",
            ));
        }
    }

    // The native event loop stays on the calling thread, as required by macOS.
    let options = ManagedPacketWindowOptions {
        window: WindowOptions {
            title: "Daedalus managed live capture fixture".into(),
            width: 640,
            height: 480,
            initial_loading: true,
            max_run_time: Some(WINDOW_DEADLINE),
            ..WindowOptions::default()
        },
        ..ManagedPacketWindowOptions::default()
    };
    window_started.store(true, Ordering::Release);
    let ManagedWindowRun {
        outcome,
        status,
        gpu_owner,
    } = run_packet_windowed_managed_with_input_policy(
        initial_stage,
        server,
        options,
        WindowInputPolicy::ProtocolOnly,
    );
    if let Err(send_error) = owner_tx.send(gpu_owner) {
        // The renderer is already outside its event loop, but this thread is
        // still the native window thread. Retain the unresolved owner instead
        // of dropping it on the wrong thread or claiming a completed drain.
        std::mem::forget(send_error.0);
        eprintln!(
            "managed_live_capture: fatal cleanup failure; returned GPU owner could not be transferred and was retained unresolved"
        );
        return Err(HarnessError::new(
            "capture owner exited before receiving the returned GPU owner; cleanup_complete=false, unresolved_gpu_owner_retained=true",
        ));
    }
    let summary = window_summary_from(outcome, status);
    if summary_tx.send(summary).is_err() {
        return Err(HarnessError::new(
            "capture owner exited before receiving window metadata",
        ));
    }

    let report_deadline = WINDOW_DEADLINE + WORKER_RETURN_DEADLINE + DRAIN_DEADLINE;
    let report = match report_rx.recv_timeout(report_deadline) {
        Ok(report) => report,
        Err(RecvTimeoutError::Timeout) => {
            return Err(HarnessError::new(
                "capture owner did not finish bounded readback cleanup in time",
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(HarnessError::new("capture owner stopped without a report"));
        }
    };
    join_reported_worker(worker_thread)?;
    println!("{}", report.json);
    Ok(report.success)
}

fn parse_args() -> HarnessResult<Args> {
    let mut out_dir = None;
    let mut scenario = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--out-dir" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| HarnessError::new("--out-dir requires a path"))?;
                if out_dir.replace(PathBuf::from(value)).is_some() {
                    return Err(HarnessError::new("--out-dir may be specified only once"));
                }
            }
            "--scenario" => {
                let value = arguments.next().ok_or_else(|| {
                    HarnessError::new("--scenario requires capture|close-held|worker-drop")
                })?;
                if scenario.replace(value).is_some() {
                    return Err(HarnessError::new("--scenario may be specified only once"));
                }
            }
            _ => return Err(HarnessError::new(format!("unknown argument: {argument}"))),
        }
    }
    let out_dir = out_dir.ok_or_else(|| HarnessError::new("--out-dir is required"))?;
    let scenario = scenario
        .as_deref()
        .and_then(Scenario::parse)
        .ok_or_else(|| {
            HarnessError::new("--scenario must be capture, close-held, or worker-drop")
        })?;
    Ok(Args { out_dir, scenario })
}

fn build_fixture() -> HarnessResult<Fixture> {
    let manifest = br#"{"schema_version":1,"name":"managed live capture fixture","scene":"scene.json","presentation":{"clear":[0.03,0.04,0.06,1.0]}}"#;
    let scene = br#"{
        "schema_version":1,
        "revision":1,
        "entities":[{
            "id":"entity:triangle",
            "components":[{
                "id":"component:triangle:mesh",
                "type_id":"daedalus.mesh_renderer",
                "asset_refs":["asset:triangle"],
                "data":{"mesh":"asset:triangle","material":{"base_color":[0.95,0.06,0.035],"metallic":0.0,"roughness":0.82}}
            }]
        }],
        "assets":[{"id":"asset:triangle","kind":"daedalus.mesh","uri":"triangle.json","metadata":{}}]
    }"#;
    let mesh = br#"{
        "schema_version":1,
        "vertices":[
            {"position":[-0.9,-0.75,0.0],"normal":[0.0,0.0,1.0]},
            {"position":[0.9,-0.75,0.0],"normal":[0.0,0.0,1.0]},
            {"position":[0.0,0.9,0.0],"normal":[0.0,0.0,1.0]}
        ],
        "indices":[0,1,2]
    }"#;
    let project = Arc::new(
        prepare_project_from_bytes(
            manifest,
            scene,
            &BTreeMap::from([("triangle.json".to_owned(), mesh.to_vec())]),
            ProjectLimits::default(),
        )
        .map_err(|error| HarnessError::new(format!("prepare fixture project: {error}")))?,
    );
    let session = SessionId::fresh();
    let entity = project
        .entities()
        .first()
        .ok_or_else(|| HarnessError::new("fixture project has no authored entity"))?;
    let entity_handle = EntityHandle {
        session,
        index: 0,
        generation: 1,
    };
    let authored_handles = BTreeMap::from([(entity.id.clone(), entity_handle)]);
    let mut cpu_owner = PacketCpuOwner::new(
        PacketCpuLimits::default(),
        daedalus_renderer::PacketRendererLimits::default(),
        daedalus_renderer::PacketAssetLimits::default(),
    )
    .map_err(|error| HarnessError::new(format!("create packet CPU owner: {error}")))?;
    let cancelled = AtomicBool::new(false);
    let source_pin: Arc<dyn Send + Sync> = project.clone();
    let generation = cpu_owner
        .prepare_generation(
            &project,
            1,
            session,
            &authored_handles,
            SourcePin::new(source_pin),
            &cancelled,
        )
        .map_err(|error| HarnessError::new(format!("prepare CPU generation: {error}")))?;
    let initial_stage = cpu_owner
        .stage_input(generation)
        .map_err(|error| HarnessError::new(format!("prepare initial GPU stage: {error}")))?;
    let initial_source = ManagedCaptureSource::new(
        ManagedStageKey::INITIAL,
        initial_stage.session(),
        initial_stage.project_generation(),
        initial_stage.source_digest(),
    )
    .map_err(|error| HarnessError::new(format!("invalid fixture capture source: {error}")))?;
    let snapshot = RenderSnapshot {
        sequence: 1,
        tick: Tick(1),
        entities: Arc::from([RenderEntitySnapshot {
            entity: entity_handle,
            transform: Transform::IDENTITY,
        }]),
    };
    let view = RenderView::perspective(Camera {
        position: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
        vertical_fov_radians: 48.0_f32.to_radians(),
        near: 0.1,
        far: 100.0,
    });
    let frame = cpu_owner
        .extract_validated(generation, &snapshot, 1, Some(&view), &cancelled)
        .map_err(|error| HarnessError::new(format!("extract fixture frame: {error}")))?;
    if frame.session() != session || frame.project_generation() != 1 {
        return Err(HarnessError::new(
            "fixture frame identity did not match its source",
        ));
    }
    Ok(Fixture {
        cpu_owner,
        generation,
        frame,
        initial_stage,
        initial_source,
    })
}

fn window_summary_from(
    outcome: Result<daedalus_renderer::ManagedWindowOutcome, daedalus_renderer::ManagedWindowError>,
    status: daedalus_renderer::ManagedWindowStatus,
) -> WindowSummary {
    let exit_reason = status.exit_reason().map(|reason| format!("{reason:?}"));
    let lifecycle = format!("{:?}", status.lifecycle());
    match outcome {
        Ok(outcome) => {
            let renderer = outcome.report().renderer.clone();
            let rendered_frames = outcome.report().rendered_frames;
            let last_presented = outcome.last_presented().cloned();
            WindowSummary {
                outcome_error: None,
                exit_reason,
                lifecycle,
                rendered_frames,
                last_presented,
                renderer,
            }
        }
        Err(error) => WindowSummary {
            outcome_error: Some(format!("{:?}: {}", error.code(), error.message())),
            exit_reason,
            lifecycle,
            rendered_frames: 0,
            last_presented: None,
            renderer: None,
        },
    }
}

fn worker_entry(
    mut context: WorkerContext,
    gpu_owner_return: &Receiver<WindowGpuOwner>,
    window_summary: &Receiver<WindowSummary>,
    owner_disposition: Arc<AtomicU8>,
) -> WorkerReport {
    let output_dir_result = fs::create_dir(&context.out_dir).map_err(|error| {
        format!(
            "cannot create fresh output directory {}: {error}",
            context.out_dir.display()
        )
    });
    let preflight_result = output_dir_result.as_ref().map_err(Clone::clone).copied();
    let _ = context.preflight.send(preflight_result);
    if let Err(error) = output_dir_result {
        return WorkerReport {
            success: false,
            json: format!(
                "{{\"schema_version\":1,\"fixture\":true,\"success\":false,\"cleanup_complete\":true,\"unresolved_gpu_owner_retained\":false,\"error\":{}}}",
                json_string(&error)
            ),
        };
    }
    let mut report = ReportData {
        captures: Vec::new(),
        observed_view: None,
        capture_ready: None,
        readback_submitted: false,
        drain_observations: Vec::new(),
        cleanup_complete: false,
        unresolved_gpu_owner_retained: false,
        window: None,
        error: None,
    };
    let open_result = catch_unwind(AssertUnwindSafe(|| {
        open_window_phase(&mut context, &mut report)
    }));
    match open_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report.error = Some(error.to_string()),
        Err(_) => report.error = Some("capture worker panicked during open-window phase".into()),
    }
    context.control.request_view_close();

    let gpu_owner = match gpu_owner_return.recv_timeout(WORKER_RETURN_DEADLINE) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => {
            set_error(&mut report, "timed out waiting for returned GPU owner");
            return finish_without_returned_owner(context, report);
        }
        Err(RecvTimeoutError::Disconnected) => {
            set_error(&mut report, "window return channel disconnected");
            return finish_without_returned_owner(context, report);
        }
    };
    let mut gpu_owner = WorkerGpuOwner::new(gpu_owner, owner_disposition);
    let summary = match window_summary.recv_timeout(Duration::from_secs(5)) {
        Ok(summary) => summary,
        Err(RecvTimeoutError::Timeout) => WindowSummary {
            outcome_error: Some(
                "window summary was not returned within the bounded deadline".into(),
            ),
            exit_reason: None,
            lifecycle: "unknown".into(),
            rendered_frames: 0,
            last_presented: None,
            renderer: None,
        },
        Err(RecvTimeoutError::Disconnected) => WindowSummary {
            outcome_error: Some("window summary channel disconnected".into()),
            exit_reason: None,
            lifecycle: "unknown".into(),
            rendered_frames: 0,
            last_presented: None,
            renderer: None,
        },
    };
    if let Some(error) = summary.outcome_error.as_ref() {
        set_error(&mut report, format!("managed window failed: {error}"));
    }
    report.window = Some(summary);

    let finish_result = catch_unwind(AssertUnwindSafe(|| {
        finish_with_returned_owner(&mut context, gpu_owner.get_mut(), &mut report)
    }));
    let owner_drained = match finish_result {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            set_error(&mut report, error.to_string());
            context.detach_and_drop_capture_worker();
            drop(context.held_readback.take());
            matches!(
                drain_owner(gpu_owner.get_mut(), &mut report.drain_observations),
                Ok(true)
            )
        }
        Err(_) => {
            set_error(
                &mut report,
                "capture worker panicked during returned-owner cleanup",
            );
            context.detach_and_drop_capture_worker();
            drop(context.held_readback.take());
            matches!(
                drain_owner(gpu_owner.get_mut(), &mut report.drain_observations),
                Ok(true)
            )
        }
    };
    report.cleanup_complete = owner_drained;
    if owner_drained {
        gpu_owner.drop_after_drained();
    } else {
        report.unresolved_gpu_owner_retained = true;
        set_error(
            &mut report,
            "returned GPU owner could not be proven drained; cleanup_complete=false, unresolved_gpu_owner_retained=true",
        );
        gpu_owner.retain_unresolved();
    }
    drop(context.frame);
    if let Err(error) = context.cpu_owner.retire_generation(context.generation) {
        set_error(
            &mut report,
            format!("retire CPU fixture generation: {error}"),
        );
    }

    let success = report.error.is_none();
    let json = match write_report(&context.out_dir, context.scenario, success, &report) {
        Ok(json) => json,
        Err(error) => {
            set_error(&mut report, format!("write structured report: {error}"));
            match write_report(&context.out_dir, context.scenario, false, &report) {
                Ok(json) => json,
                Err(report_error) => {
                    return WorkerReport {
                        success: false,
                        json: format!(
                            "{{\"schema_version\":1,\"fixture\":true,\"success\":false,\"cleanup_complete\":{},\"unresolved_gpu_owner_retained\":{},\"error\":{}}}",
                            report.cleanup_complete,
                            report.unresolved_gpu_owner_retained,
                            json_string(&report_error.to_string())
                        ),
                    };
                }
            }
        }
    };
    WorkerReport {
        success: report.error.is_none(),
        json,
    }
}

fn emergency_report_after_panic(
    gpu_owner_return: &Receiver<WindowGpuOwner>,
    owner_disposition: &Arc<AtomicU8>,
    window_started: &AtomicBool,
    scenario: Scenario,
) -> WorkerReport {
    let mut drain_observations = Vec::new();
    let disposition = owner_disposition.load(Ordering::Acquire);
    let owner = if disposition == OWNER_DRAINED
        || disposition == OWNER_RETAINED_UNRESOLVED
        || !window_started.load(Ordering::Acquire)
    {
        None
    } else {
        match gpu_owner_return.recv_timeout(WORKER_RETURN_DEADLINE) {
            Ok(owner) => Some(WorkerGpuOwner::new(owner, Arc::clone(owner_disposition))),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    };
    let cleanup_complete = if let Some(mut owner) = owner {
        let drained = matches!(
            drain_owner(owner.get_mut(), &mut drain_observations),
            Ok(true)
        );
        if drained {
            owner.drop_after_drained();
        } else {
            owner.retain_unresolved();
        }
        drained
    } else {
        disposition == OWNER_DRAINED || !window_started.load(Ordering::Acquire)
    };
    let unresolved_gpu_owner_retained = disposition == OWNER_RETAINED_UNRESOLVED
        || (window_started.load(Ordering::Acquire) && !cleanup_complete);
    let json = serde_json::json!({
        "schema_version": 1,
        "fixture": true,
        "authority_kind": "synthetic_snapshot_fixture",
        "scenario": scenario.as_str(),
        "success": false,
        "error": "capture owner panicked; emergency cleanup ran on the owner worker",
        "cleanup_complete": cleanup_complete,
        "unresolved_gpu_owner_retained": unresolved_gpu_owner_retained,
        "drain_observations": drain_observations,
        "limitations": ["no runtime SessionControl authority claim", "no OS scanout claim"],
    })
    .to_string();
    WorkerReport {
        success: false,
        json,
    }
}

fn open_window_phase(context: &mut WorkerContext, report: &mut ReportData) -> HarnessResult<()> {
    let readiness = wait_until_ready(context)?;
    report.capture_ready = Some(readiness);
    let first_key = ManagedCaptureKey::new(1)
        .ok_or_else(|| HarnessError::new("capture key zero is invalid"))?;
    let first_request = ManagedCaptureRequest::new(
        first_key,
        context.initial_source.clone(),
        context.frame.tick(),
        Instant::now() + CAPTURE_DEADLINE,
    )
    .map_err(|error| HarnessError::new(format!("create first capture request: {error}")))?;
    submit_with_busy_retry(context, first_request)?;

    let initial_frame = Arc::clone(&context.frame);
    send_initial_frame(&context.window_client, initial_frame)?;

    match context.scenario {
        Scenario::Capture => {
            let first_pixels = wait_for_pixels(context, first_key, report)?;
            let first_record = capture_to_record(
                first_pixels,
                &context.out_dir,
                1,
                readiness.surface_format(),
                Some((&context.initial_source, context.frame.tick())),
            )?;
            report.observed_view = Some(first_record.view.clone());

            let second_source = ManagedCaptureSource::from_presented(&first_record.view);
            let second_key = ManagedCaptureKey::new(2)
                .ok_or_else(|| HarnessError::new("capture key two is invalid"))?;
            let second_request = ManagedCaptureRequest::new(
                second_key,
                second_source.clone(),
                first_record.view.tick(),
                Instant::now() + CAPTURE_DEADLINE,
            )
            .map_err(|error| {
                HarnessError::new(format!("create second capture request: {error}"))
            })?;
            submit_with_busy_retry(context, second_request)?;
            let second_pixels = wait_for_pixels(context, second_key, report)?;
            let second_record = capture_to_record(
                second_pixels,
                &context.out_dir,
                2,
                readiness.surface_format(),
                Some((&second_source, first_record.view.tick())),
            )?;
            validate_repeat_capture(&first_record, &second_record)?;
            report.captures.push(first_record);
            report.captures.push(second_record);
        }
        Scenario::CloseHeld | Scenario::WorkerDrop => {
            let readback = wait_for_readback(context, first_key)?;
            validate_view(
                readback.presented(),
                &context.initial_source,
                context.frame.tick(),
            )?;
            report.observed_view = Some(readback.presented().clone());
            report.readback_submitted = true;
            context.held_readback = Some(readback);
        }
    }
    Ok(())
}

fn wait_until_ready(
    context: &WorkerContext,
) -> HarnessResult<daedalus_renderer::ManagedCaptureReadyInfo> {
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    let mut stage_ready = false;
    let mut capture_ready = None;
    loop {
        if !stage_ready {
            match context.window_client.try_stage_event() {
                ManagedWindowRead::Value(ManagedStageEvent::Ready { key, status, .. }) => {
                    if key != ManagedStageKey::INITIAL
                        || status.state() != PacketStageState::Ready
                        || !status.initialization_complete()
                    {
                        return Err(HarnessError::new(
                            "initial packet stage did not reach Ready",
                        ));
                    }
                    stage_ready = true;
                }
                ManagedWindowRead::Value(ManagedStageEvent::Failed { error, .. }) => {
                    return Err(HarnessError::new(format!(
                        "initial packet stage failed: {}",
                        error.message()
                    )));
                }
                ManagedWindowRead::Value(ManagedStageEvent::Deferred { key, .. }) => {
                    if key != ManagedStageKey::INITIAL {
                        return Err(HarnessError::new(
                            "deferred event named a non-initial stage during bootstrap",
                        ));
                    }
                    let input =
                        context
                            .cpu_owner
                            .stage_input(context.generation)
                            .map_err(|error| {
                                HarnessError::new(format!(
                                    "rebuild deferred initial stage input: {error}"
                                ))
                            })?;
                    send_initial_stage(&context.window_client, input)?;
                }
                ManagedWindowRead::Value(ManagedStageEvent::Cancelled { .. }) => {
                    return Err(HarnessError::new(
                        "initial packet stage was cancelled before readiness",
                    ));
                }
                ManagedWindowRead::Empty | ManagedWindowRead::Busy => {}
                ManagedWindowRead::Closed => {
                    return Err(HarnessError::new(
                        "window closed before initial stage became ready",
                    ));
                }
            }
        }
        if capture_ready.is_none() {
            match context.capture_client.try_status() {
                Ok(ManagedCaptureState::Ready(info)) => capture_ready = Some(info),
                Ok(ManagedCaptureState::Starting) => {}
                Ok(ManagedCaptureState::Unsupported(reason)) => {
                    return Err(HarnessError::new(format!(
                        "capture surface unsupported: {reason:?}"
                    )));
                }
                Ok(ManagedCaptureState::Detached) => {
                    return Err(HarnessError::new(
                        "capture endpoint detached before readiness",
                    ));
                }
                Err(daedalus_renderer::ManagedCaptureEndpointError::Busy) => {}
                Err(error) => {
                    return Err(HarnessError::new(format!(
                        "read capture readiness: {error}"
                    )));
                }
            }
        }
        if stage_ready && let Some(info) = capture_ready {
            return Ok(info);
        }
        if context.control.is_closed() {
            return Err(HarnessError::new(
                "managed window closed before capture readiness",
            ));
        }
        if Instant::now() >= deadline {
            return Err(HarnessError::new(
                "timed out waiting for initial stage and capture readiness",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn submit_with_busy_retry(
    context: &WorkerContext,
    request: Box<daedalus_renderer::ManagedCaptureRequest>,
) -> HarnessResult<()> {
    let deadline = request.deadline();
    let mut pending = request;
    loop {
        match context.capture_client.try_submit(pending) {
            Ok(()) => return Ok(()),
            Err((request, ManagedCaptureSubmitError::Busy)) => {
                pending = request;
                if Instant::now() >= deadline {
                    return Err(HarnessError::new(
                        "capture request stayed Busy through its deadline",
                    ));
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err((_request, error)) => {
                return Err(HarnessError::new(format!(
                    "capture request rejected: {error}"
                )));
            }
        }
    }
}

fn send_initial_frame(
    client: &ManagedPacketWindowClient,
    frame: Arc<CpuFrameHandle>,
) -> HarnessResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut input = ManagedPacketWindowInput::Frame {
        key: ManagedStageKey::INITIAL,
        frame,
    };
    loop {
        match client.try_send(input) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                input = returned;
                if Instant::now() >= deadline {
                    return Err(HarnessError::new("initial frame lane remained full"));
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(HarnessError::new(
                    "managed window closed before initial frame admission",
                ));
            }
        }
    }
}

fn send_initial_stage(
    client: &ManagedPacketWindowClient,
    input: PacketStageInput,
) -> HarnessResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut message = ManagedPacketWindowInput::BeginStage {
        key: ManagedStageKey::INITIAL,
        input,
        limits: PacketStageLimits::default(),
    };
    loop {
        match client.try_send(message) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                message = returned;
                if Instant::now() >= deadline {
                    return Err(HarnessError::new(
                        "deferred initial stage retry lane remained full",
                    ));
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(HarnessError::new(
                    "managed window closed before deferred initial stage retry",
                ));
            }
        }
    }
}

fn wait_for_readback(
    context: &WorkerContext,
    key: ManagedCaptureKey,
) -> HarnessResult<ManagedCaptureReadback> {
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    loop {
        match context.capture_worker().try_take_return() {
            Ok(Some(ManagedCaptureReturn::Submitted(readback))) if readback.key() == key => {
                if !std::sync::Weak::ptr_eq(
                    readback.selected_frame(),
                    &Arc::downgrade(&context.frame),
                ) {
                    drop(readback);
                    return Err(HarnessError::new(
                        "capture readback selected a different CPU frame than the fixture submission",
                    ));
                }
                return Ok(readback);
            }
            Ok(Some(ManagedCaptureReturn::Submitted(_))) => {
                return Err(HarnessError::new("capture worker returned the wrong key"));
            }
            Ok(Some(ManagedCaptureReturn::Failed {
                key: returned,
                failure,
                cleanup,
            })) => {
                drop(cleanup);
                return Err(HarnessError::new(format!(
                    "capture {returned:?} failed before pixels: {failure}"
                )));
            }
            Ok(Some(ManagedCaptureReturn::Cleanup(cleanup))) => {
                let returned = cleanup.key();
                drop(cleanup);
                return Err(HarnessError::new(format!(
                    "capture {returned:?} required cleanup instead of pixels"
                )));
            }
            Ok(None) | Err(ManagedCaptureWorkerError::Busy) => {}
            Err(error) => return Err(HarnessError::new(format!("read capture return: {error}"))),
        }
        if context.control.is_closed() {
            return Err(HarnessError::new(
                "window closed before capture readback was returned",
            ));
        }
        if Instant::now() >= deadline {
            return Err(HarnessError::new(
                "timed out waiting for submitted capture return",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_pixels(
    context: &WorkerContext,
    key: ManagedCaptureKey,
    report: &mut ReportData,
) -> HarnessResult<ManagedCapturePixels> {
    let mut readback = wait_for_readback(context, key)?;
    report.readback_submitted = true;
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    loop {
        match readback.try_read_rgba8() {
            Ok(Some(pixels)) => return Ok(pixels),
            Ok(None) | Err(daedalus_renderer::ManagedCaptureReadbackError::Busy) => {}
            Err(error) => return Err(HarnessError::new(format!("read capture pixels: {error}"))),
        }
        if Instant::now() >= deadline {
            return Err(HarnessError::new(
                "capture pixels remained pending through the deadline",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn capture_to_record(
    pixels: ManagedCapturePixels,
    out_dir: &Path,
    ordinal: usize,
    expected_format: ManagedCaptureSurfaceFormat,
    expected_source: Option<(&ManagedCaptureSource, Tick)>,
) -> HarnessResult<CaptureRecord> {
    let (key, view, surface_format, rgba) = pixels.into_parts();
    if surface_format != expected_format {
        return Err(HarnessError::new(format!(
            "selected surface format changed from {expected_format:?} to {surface_format:?}"
        )));
    }
    if let Some((source, tick)) = expected_source {
        validate_view(&view, source, tick)?;
    }
    let width = view.physical_width();
    let height = view.physical_height();
    let expected_bytes = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| HarnessError::new("capture RGBA size overflow"))?;
    if width == 0 || height == 0 || rgba.len() != expected_bytes {
        return Err(HarnessError::new(format!(
            "capture returned {} RGBA bytes for {width}x{height}",
            rgba.len()
        )));
    }

    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .map_err(|error| HarnessError::new(format!("encode PNG header: {error}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|error| HarnessError::new(format!("encode PNG pixels: {error}")))?;
    }
    let file_name = format!("capture-{ordinal}.png");
    let output_path = out_dir.join(&file_name);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|error| HarnessError::new(format!("create {}: {error}", output_path.display())))?;
    output
        .write_all(&encoded)
        .and_then(|()| output.sync_all())
        .map_err(|error| HarnessError::new(format!("write {}: {error}", output_path.display())))?;

    let decoder = png::Decoder::new(Cursor::new(encoded.as_slice()));
    let mut reader = decoder
        .read_info()
        .map_err(|error| HarnessError::new(format!("decode PNG header: {error}")))?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| HarnessError::new("decoded PNG buffer size overflow"))?;
    let mut decoded = vec![0_u8; buffer_size];
    let decoded_info = reader
        .next_frame(&mut decoded)
        .map_err(|error| HarnessError::new(format!("decode PNG pixels: {error}")))?;
    decoded.truncate(decoded_info.buffer_size());
    if decoded_info.width != width
        || decoded_info.height != height
        || decoded_info.color_type != png::ColorType::Rgba
        || decoded_info.bit_depth != png::BitDepth::Eight
    {
        return Err(HarnessError::new(
            "decoded PNG format or dimensions changed",
        ));
    }
    let decoded_matches_rgba = decoded == rgba;
    if !decoded_matches_rgba {
        return Err(HarnessError::new(
            "PNG decode did not reproduce readback RGBA bytes",
        ));
    }

    let background_sample = sample_pixel(&rgba, width, height, 2, 2)?;
    let overlay_sample = sample_pixel(
        &rgba,
        width,
        height,
        width / 2,
        height.saturating_mul(88) / 100,
    )?;
    let (red_mesh_sample, red_mesh_pixel_count) = find_red_mesh_sample(&rgba, width, height)?;
    if red_mesh_pixel_count == 0 {
        return Err(HarnessError::new(
            "fixture triangle produced no red-dominant pixels",
        ));
    }

    Ok(CaptureRecord {
        key: key.get(),
        file_name,
        view,
        surface_format,
        rgba_sha256: sha256_hex(&rgba),
        png_sha256: sha256_hex(&encoded),
        decoded_matches_rgba,
        background_sample,
        overlay_sample,
        red_mesh_sample,
        red_mesh_pixel_count,
    })
}

fn validate_view(
    view: &ManagedPresentedView,
    source: &ManagedCaptureSource,
    tick: Tick,
) -> HarnessResult<()> {
    if view.key() != source.stage_key()
        || view.session() != source.session()
        || view.project_generation() != source.project_generation()
        || view.source_digest() != source.source_digest()
        || view.tick() != tick
        || view.physical_width() == 0
        || view.physical_height() == 0
        || view.present_sequence() == 0
    {
        return Err(HarnessError::new(
            "actual presented view does not match its capture request",
        ));
    }
    Ok(())
}

fn validate_repeat_capture(first: &CaptureRecord, second: &CaptureRecord) -> HarnessResult<()> {
    if first.view.key() != second.view.key()
        || first.view.session() != second.view.session()
        || first.view.project_generation() != second.view.project_generation()
        || first.view.source_digest() != second.view.source_digest()
        || first.view.publication_sequence() != second.view.publication_sequence()
        || first.view.authority_snapshot_sequence() != second.view.authority_snapshot_sequence()
        || first.view.tick() != second.view.tick()
        || first.view.view() != second.view.view()
        || first.view.view_projection() != second.view.view_projection()
        || second.view.present_sequence() <= first.view.present_sequence()
    {
        return Err(HarnessError::new(
            "repeat captures did not preserve source/frame identity or advance actual presents",
        ));
    }
    if first.surface_format != second.surface_format
        || first.view.physical_width() != second.view.physical_width()
        || first.view.physical_height() != second.view.physical_height()
    {
        return Err(HarnessError::new(
            "surface format or physical size changed between repeat captures",
        ));
    }
    if first.background_sample.rgba == second.background_sample.rgba
        || first.overlay_sample.rgba == second.overlay_sample.rgba
    {
        return Err(HarnessError::new(
            "first loading-overlay capture did not differ from the later clear frame",
        ));
    }
    let expected_clear = CLEAR_COLOR.map(linear_to_srgb_byte);
    if !near_rgba(second.background_sample.rgba, expected_clear, 3) {
        return Err(HarnessError::new(format!(
            "later background sample {:?} did not match authored clear color {:?}",
            second.background_sample.rgba, expected_clear
        )));
    }
    Ok(())
}

fn finish_with_returned_owner(
    context: &mut WorkerContext,
    gpu_owner: &mut WindowGpuOwner,
    report: &mut ReportData,
) -> HarnessResult<()> {
    let mut first_drain = None;
    if matches!(context.scenario, Scenario::CloseHeld | Scenario::WorkerDrop)
        && context.held_readback.is_some()
    {
        let status = gpu_owner
            .try_drain_after_window()
            .map_err(|error| HarnessError::new(format!("first returned-owner drain: {error}")))?;
        first_drain = Some(status);
        report
            .drain_observations
            .push(format!("first_held_readback_drain={status:?}"));
        if status != daedalus_renderer::PacketRendererDrainStatus::Pending {
            set_error(
                report,
                "returned GPU owner drained while a readback handle was still held",
            );
        }
    }

    if context.scenario == Scenario::WorkerDrop && context.held_readback.is_some() {
        context.detach_and_drop_capture_worker();
        report
            .drain_observations
            .push("capture_worker_endpoint_dropped_before_readback=true".into());
        drop(context.held_readback.take());
        report
            .drain_observations
            .push("readback_dropped_before_fallback_drain=true".into());
    } else if context.scenario == Scenario::CloseHeld && report.error.is_none() {
        let readback = context
            .held_readback
            .take()
            .ok_or_else(|| HarnessError::new("close-held scenario lost its readback handle"))?;
        let pixels = read_held_after_close(readback, gpu_owner)?;
        let expected_source = ManagedCaptureSource::from_presented(
            report
                .observed_view
                .as_ref()
                .ok_or_else(|| HarnessError::new("close-held scenario has no selected view"))?,
        );
        let ready_format = report
            .capture_ready
            .map(|info| info.surface_format())
            .ok_or_else(|| HarnessError::new("capture readiness disappeared after close"))?;
        let record = capture_to_record(
            pixels,
            &context.out_dir,
            1,
            ready_format,
            Some((&expected_source, expected_source_tick(report)?)),
        )?;
        report.captures.push(record);
        context.detach_and_drop_capture_worker();
        report
            .drain_observations
            .push("held_readback_completed_after_window_close=true".into());
    } else if context.held_readback.is_some() {
        context.detach_and_drop_capture_worker();
        drop(context.held_readback.take());
    } else {
        context.detach_and_drop_capture_worker();
    }

    let drained = drain_owner(gpu_owner, &mut report.drain_observations)?;
    if !drained {
        return Err(HarnessError::new(
            "returned GPU owner did not drain before deadline",
        ));
    }
    if first_drain == Some(daedalus_renderer::PacketRendererDrainStatus::Pending)
        && context.scenario == Scenario::CloseHeld
        && report.error.is_none()
    {
        report
            .drain_observations
            .push("held_capture_released_before_final_drain=true".into());
    }
    match context.scenario {
        Scenario::Capture if report.captures.len() != 2 => {
            return Err(HarnessError::new(
                "capture scenario did not produce two verified PNGs",
            ));
        }
        Scenario::CloseHeld if report.captures.len() != 1 || !report.readback_submitted => {
            return Err(HarnessError::new(
                "close-held scenario did not read one submitted capture",
            ));
        }
        Scenario::WorkerDrop
            if !report.readback_submitted
                || !report.drain_observations.iter().any(|value| {
                    value == "capture_worker_endpoint_dropped_before_readback=true"
                })
                || !report
                    .drain_observations
                    .iter()
                    .any(|value| value == "readback_dropped_before_fallback_drain=true")
                || first_drain != Some(daedalus_renderer::PacketRendererDrainStatus::Pending) =>
        {
            return Err(HarnessError::new(
                "worker-drop scenario did not verify held-handle fallback drain",
            ));
        }
        _ => {}
    }
    validate_window_summary(report)?;
    Ok(())
}

fn expected_source_tick(report: &ReportData) -> HarnessResult<Tick> {
    report
        .observed_view
        .as_ref()
        .map(ManagedPresentedView::tick)
        .ok_or_else(|| HarnessError::new("capture report has no observed view tick"))
}

fn read_held_after_close(
    mut readback: ManagedCaptureReadback,
    gpu_owner: &mut WindowGpuOwner,
) -> HarnessResult<ManagedCapturePixels> {
    let deadline = Instant::now() + DRAIN_DEADLINE;
    loop {
        match readback.try_read_rgba8() {
            Ok(Some(pixels)) => return Ok(pixels),
            Ok(None) | Err(daedalus_renderer::ManagedCaptureReadbackError::Busy) => {}
            Err(error) => {
                return Err(HarnessError::new(format!(
                    "read held pixels after close: {error}"
                )));
            }
        }
        let status = gpu_owner.try_drain_after_window().map_err(|error| {
            HarnessError::new(format!("poll returned owner for held readback: {error}"))
        })?;
        if status != daedalus_renderer::PacketRendererDrainStatus::Pending {
            return Err(HarnessError::new(
                "returned owner drained while the capture readback was still held",
            ));
        }
        if Instant::now() >= deadline {
            drop(readback.into_cleanup());
            return Err(HarnessError::new(
                "held readback remained pending after close",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn drain_owner(
    gpu_owner: &mut WindowGpuOwner,
    observations: &mut Vec<String>,
) -> HarnessResult<bool> {
    let deadline = Instant::now() + DRAIN_DEADLINE;
    let mut last = None;
    loop {
        let status = gpu_owner
            .try_drain_after_window()
            .map_err(|error| HarnessError::new(format!("poll returned GPU owner: {error}")))?;
        if last != Some(status) {
            observations.push(format!("returned_owner_drain={status:?}"));
            last = Some(status);
        }
        if status == daedalus_renderer::PacketRendererDrainStatus::Drained {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            observations.push("returned_owner_drain_deadline_elapsed=true".into());
            return Ok(false);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn validate_window_summary(report: &ReportData) -> HarnessResult<()> {
    let window = report
        .window
        .as_ref()
        .ok_or_else(|| HarnessError::new("window did not return a summary"))?;
    if report.error.is_some() {
        return Ok(());
    }
    if window.exit_reason.as_deref() != Some("ViewClosed") || window.rendered_frames == 0 {
        return Err(HarnessError::new(format!(
            "window did not close after a real presented frame (exit={:?}, frames={})",
            window.exit_reason, window.rendered_frames
        )));
    }
    let last = window
        .last_presented
        .as_ref()
        .ok_or_else(|| HarnessError::new("managed window reported no actual presented packet"))?;
    let selected = report
        .captures
        .last()
        .map(|capture| &capture.view)
        .or(report.observed_view.as_ref())
        .ok_or_else(|| HarnessError::new("scenario recorded no selected capture view"))?;
    if last.session() != selected.session()
        || last.key() != selected.key()
        || last.project_generation() != selected.project_generation()
        || last.source_digest() != selected.source_digest()
        || last.publication_sequence() != selected.publication_sequence()
        || last.authority_snapshot_sequence() != selected.authority_snapshot_sequence()
        || last.tick() != selected.tick()
        || last.physical_width() != selected.physical_width()
        || last.physical_height() != selected.physical_height()
        || last.present_sequence() < selected.present_sequence()
    {
        return Err(HarnessError::new(
            "final actual window identity disagrees with captured frame",
        ));
    }
    Ok(())
}

fn finish_without_returned_owner(
    mut context: WorkerContext,
    mut report: ReportData,
) -> WorkerReport {
    context.detach_and_drop_capture_worker();
    drop(context.held_readback.take());
    report.cleanup_complete = false;
    report.unresolved_gpu_owner_retained = true;
    set_error(
        &mut report,
        "window owner return was unavailable; cleanup_complete=false, unresolved_gpu_owner_retained=true",
    );
    let json = match write_report(&context.out_dir, context.scenario, false, &report) {
        Ok(json) => json,
        Err(error) => {
            set_error(&mut report, format!("write failure report: {error}"));
            format!(
                "{{\"schema_version\":1,\"fixture\":true,\"success\":false,\"cleanup_complete\":false,\"unresolved_gpu_owner_retained\":true,\"error\":{}}}",
                json_string(&error.to_string())
            )
        }
    };
    WorkerReport {
        success: false,
        json,
    }
}

fn set_error(report: &mut ReportData, error: impl Into<String>) {
    if report.error.is_none() {
        report.error = Some(error.into());
    }
}

fn write_report(
    out_dir: &Path,
    scenario: Scenario,
    success: bool,
    report: &ReportData,
) -> HarnessResult<String> {
    let captures = report.captures.iter().map(capture_json).collect::<Vec<_>>();
    let window = report.window.as_ref().map(window_json);
    let observed_view = report.observed_view.as_ref().map(presented_json);
    let renderer = report
        .window
        .as_ref()
        .and_then(|window| window.renderer.as_ref())
        .map(|info| {
            serde_json::json!({
                "adapter_name": info.adapter_name,
                "backend": info.backend,
                "device_type": info.device_type,
            })
        });
    let json = serde_json::json!({
        "schema_version": 1,
        "fixture": true,
        "authority_kind": "synthetic_snapshot_fixture",
        "scenario": scenario.as_str(),
        "success": success && report.error.is_none(),
        "error": report.error,
        "renderer": renderer,
        "capture_ready": report.capture_ready.map(|info| serde_json::json!({
            "surface_format": format_name(info.surface_format()),
            "max_width": info.max_width(),
            "max_height": info.max_height(),
            "max_pixels": info.max_pixels(),
        })),
        "window": window,
        "observed_capture_view": observed_view,
        "readback_submitted": report.readback_submitted,
        "cleanup_complete": report.cleanup_complete,
        "unresolved_gpu_owner_retained": report.unresolved_gpu_owner_retained,
        "captures": captures,
        "drain_observations": report.drain_observations,
        "limitations": ["no runtime SessionControl authority claim", "no resize or alternate-format claim", "no OS scanout claim"],
    });
    let text = serde_json::to_string_pretty(&json)
        .map_err(|error| HarnessError::new(format!("serialize report: {error}")))?;
    let path = out_dir.join("report.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| HarnessError::new(format!("create {}: {error}", path.display())))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| HarnessError::new(format!("write {}: {error}", path.display())))?;
    Ok(text)
}

fn window_json(window: &WindowSummary) -> serde_json::Value {
    serde_json::json!({
        "exit_reason": window.exit_reason,
        "lifecycle": window.lifecycle,
        "rendered_frames": window.rendered_frames,
        "last_presented": window.last_presented.as_ref().map(presented_json),
    })
}

fn capture_json(capture: &CaptureRecord) -> serde_json::Value {
    serde_json::json!({
        "key": capture.key.to_string(),
        "png_file": capture.file_name,
        "surface_format": format_name(capture.surface_format),
        "physical_width": capture.view.physical_width(),
        "physical_height": capture.view.physical_height(),
        "rgba_sha256": capture.rgba_sha256,
        "png_sha256": capture.png_sha256,
        "decoded_matches_rgba": capture.decoded_matches_rgba,
        "background_sample": sample_json(&capture.background_sample),
        "loading_overlay_probe": sample_json(&capture.overlay_sample),
        "red_mesh_sample": sample_json(&capture.red_mesh_sample),
        "red_mesh_pixel_count": capture.red_mesh_pixel_count,
        "presented": presented_json(&capture.view),
    })
}

fn presented_json(view: &ManagedPresentedView) -> serde_json::Value {
    let camera = view.view().camera();
    serde_json::json!({
        "stage_key": view.key().get().to_string(),
        "session": view.session().get().to_string(),
        "tick": view.tick().get().to_string(),
        "publication_sequence": view.publication_sequence().to_string(),
        "authority_snapshot_sequence": view.authority_snapshot_sequence().map(|value| value.to_string()),
        "project_generation": view.project_generation().to_string(),
        "source_digest": view.source_digest(),
        "present_sequence": view.present_sequence().to_string(),
        "physical_width": view.physical_width(),
        "physical_height": view.physical_height(),
        "camera": {
            "position": camera.position,
            "target": camera.target,
            "up": camera.up,
            "vertical_fov_radians": camera.vertical_fov_radians,
            "near": camera.near,
            "far": camera.far,
        },
        "view_projection": view.view_projection(),
    })
}

fn sample_json(sample: &PixelSample) -> serde_json::Value {
    serde_json::json!({ "x": sample.x, "y": sample.y, "rgba": sample.rgba })
}

fn sample_pixel(
    rgba: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
) -> HarnessResult<PixelSample> {
    if width == 0 || height == 0 || x >= width || y >= height {
        return Err(HarnessError::new(
            "pixel sample is outside the captured image",
        ));
    }
    let offset = usize::try_from((u64::from(y) * u64::from(width) + u64::from(x)) * 4)
        .map_err(|_| HarnessError::new("pixel sample offset overflow"))?;
    let bytes = rgba
        .get(offset..offset + 4)
        .ok_or_else(|| HarnessError::new("captured RGBA buffer is shorter than its dimensions"))?;
    Ok(PixelSample {
        x,
        y,
        rgba: [bytes[0], bytes[1], bytes[2], bytes[3]],
    })
}

fn find_red_mesh_sample(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> HarnessResult<(PixelSample, usize)> {
    let x0 = width / 5;
    let x1 = width.saturating_mul(4) / 5;
    let y0 = height / 5;
    let y1 = height.saturating_mul(4) / 5;
    let mut count = 0usize;
    let mut best = None::<PixelSample>;
    for y in y0..y1 {
        for x in x0..x1 {
            let sample = sample_pixel(rgba, width, height, x, y)?;
            let [red, green, blue, _] = sample.rgba;
            if red > green.saturating_add(20) && red > blue.saturating_add(20) {
                count = count.saturating_add(1);
                if best
                    .as_ref()
                    .is_none_or(|current| sample.rgba[0] > current.rgba[0])
                {
                    best = Some(sample);
                }
            }
        }
    }
    let sample = best.unwrap_or(sample_pixel(rgba, width, height, width / 2, height / 2)?);
    Ok((sample, count))
}

fn linear_to_srgb_byte(linear: f32) -> u8 {
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn near_rgba(actual: [u8; 4], expected: [u8; 4], tolerance: u8) -> bool {
    actual
        .into_iter()
        .zip(expected)
        .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
}

fn format_name(format: ManagedCaptureSurfaceFormat) -> &'static str {
    match format {
        ManagedCaptureSurfaceFormat::Rgba8Unorm => "rgba8_unorm",
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb => "rgba8_unorm_srgb",
        ManagedCaptureSurfaceFormat::Bgra8Unorm => "bgra8_unorm",
        ManagedCaptureSurfaceFormat::Bgra8UnormSrgb => "bgra8_unorm_srgb",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"report serialization failed\"".into())
}
