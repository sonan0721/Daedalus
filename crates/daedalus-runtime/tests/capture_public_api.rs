use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use daedalus_runtime::{
    CaptureCompletion, CaptureFailure, CaptureFrameMetadata, CaptureHandoffPort,
    CaptureHandoffState, CapturePixelEncoding, CapturePngMetadata, CaptureProjection,
    CaptureReadyInfo, CaptureSurfaceFormat, CaptureView, NativeProjectActivationService,
    NativeProjectPreparationOptions, ProjectPublicationIdentity, RequestPhase, RequestProgress,
    RequestTicket, SessionControlConfig, SessionOperation, SessionRequestError,
    SessionRequestResult, SessionSubmitError, WaitStage,
};

const WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(2);

static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);
static ENGINE_START_LOCK: Mutex<()> = Mutex::new(());

struct EmptyProject(PathBuf);

impl EmptyProject {
    fn new() -> Self {
        let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daedalus-runtime-capture-public-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique empty project directory");
        fs::write(
            path.join("daedalus.project.json"),
            br#"{"schema_version":1,"name":"capture-public-api-test","scene":"scene.json"}"#,
        )
        .expect("write project manifest");
        fs::write(
            path.join("scene.json"),
            br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#,
        )
        .expect("write empty scene");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EmptyProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn submit_capture(client: &daedalus_runtime::SessionControlClient) -> RequestTicket {
    let operation = SessionOperation::CaptureFrame {
        at_least_tick: None,
        timeout: Some(WAIT),
    };
    let deadline = Instant::now() + WAIT;
    loop {
        match client.try_submit(operation.clone()) {
            Ok(ticket) => return ticket,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull) => {
                assert!(
                    Instant::now() < deadline,
                    "capture request admission timed out"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("capture request admission failed: {error}"),
        }
    }
}

fn assert_capture_capacity(client: &daedalus_runtime::SessionControlClient) {
    let operation = SessionOperation::CaptureFrame {
        at_least_tick: None,
        timeout: Some(WAIT),
    };
    let deadline = Instant::now() + WAIT;
    loop {
        match client.try_submit(operation.clone()) {
            Err(SessionSubmitError::CaptureCapacity) => return,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull) => {
                assert!(Instant::now() < deadline, "capture admission stayed busy");
                thread::sleep(POLL);
            }
            Ok(ticket) => {
                drop(ticket);
                panic!("a second capture was admitted while the image slot was retained");
            }
            Err(error) => panic!("unexpected capture admission result: {error}"),
        }
    }
}

fn take_capture_request(handoff: &CaptureHandoffPort) -> daedalus_runtime::CaptureRequest {
    let deadline = Instant::now() + WAIT;
    loop {
        match handoff.try_take_request() {
            Ok(Some(request)) => return request,
            Ok(None) | Err(daedalus_runtime::CaptureHandoffError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "actor did not dispatch capture request"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("capture handoff failed while taking request: {error}"),
        }
    }
}

fn latest_frame(
    client: &daedalus_runtime::SessionControlClient,
) -> std::sync::Arc<daedalus_runtime::SessionFrame> {
    let deadline = Instant::now() + WAIT;
    loop {
        match client.try_latest_frame() {
            Ok(Some(frame)) => return frame,
            Ok(None) | Err(daedalus_runtime::FrameReadError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "actor has not published its first frame"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("could not read actor frame: {error}"),
        }
    }
}

fn wait_terminal(ticket: &RequestTicket) -> std::sync::Arc<RequestProgress> {
    ticket
        .wait_for(WaitStage::Terminal, WAIT)
        .expect("actor should terminalize the request within its capture deadline")
}

fn complete_capture(
    request: &mut daedalus_runtime::CaptureRequest,
    completion: &CaptureCompletion,
) {
    let deadline = Instant::now() + WAIT;
    loop {
        match request.responder_mut().try_complete(completion) {
            Ok(()) => return,
            Err(daedalus_runtime::CaptureResponderError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "capture completion slot stayed busy"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("capture completion was not accepted: {error}"),
        }
    }
}

fn shutdown(service: &mut NativeProjectActivationService) {
    let deadline = Instant::now() + WAIT;
    loop {
        match service.try_shutdown() {
            Ok(daedalus_runtime::NativeProjectActivationShutdown::Stopped) => return,
            Ok(daedalus_runtime::NativeProjectActivationShutdown::Stopping) => {
                assert!(
                    Instant::now() < deadline,
                    "capture test service did not stop"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("capture test service shutdown failed: {error}"),
        }
    }
}

fn fake_ready_metadata(
    publication: ProjectPublicationIdentity,
    present_sequence: u64,
) -> (CaptureView, CapturePngMetadata, CaptureFrameMetadata) {
    // This is deliberately a trusted CPU handoff fixture. It supplies metadata only; it does
    // not render pixels, produce a PNG, exercise GPU readback, or verify PNG decoding.
    let view = CaptureView::new(
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
    .expect("fixture view is bounded and valid");
    let png =
        CapturePngMetadata::new(2, 2, 80, [0x5a; 32]).expect("fixture PNG metadata is bounded");
    let metadata = CaptureFrameMetadata::new(
        publication,
        present_sequence,
        view.clone(),
        CaptureSurfaceFormat::Rgba8UnormSrgb,
        CapturePixelEncoding::SrgbEncoded,
        png.clone(),
    )
    .expect("fixture metadata is internally consistent");
    (view, png, metadata)
}

fn flush_actor(client: &daedalus_runtime::SessionControlClient) -> u64 {
    let deadline = Instant::now() + WAIT;
    let mut ticket = loop {
        match client.try_submit(SessionOperation::Status) {
            Ok(ticket) => break ticket,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull) => {
                assert!(
                    Instant::now() < deadline,
                    "actor status admission timed out"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("actor status admission failed: {error}"),
        }
    };
    let sequence = ticket.id().sequence();
    let progress = wait_terminal(&ticket);
    assert_eq!(progress.phase(), RequestPhase::Terminal);
    assert!(matches!(
        progress.terminal(),
        Some(Ok(SessionRequestResult::Status(_)))
    ));
    drop(progress);
    ticket.acknowledge().expect("acknowledge status request");
    sequence
}

fn complete_captured_request(
    client: &daedalus_runtime::SessionControlClient,
    handoff: &CaptureHandoffPort,
    publication: &ProjectPublicationIdentity,
    ready: CaptureReadyInfo,
    present_sequence: u64,
) -> (
    RequestTicket,
    daedalus_runtime::CaptureRequest,
    std::sync::Arc<RequestProgress>,
) {
    let ticket = submit_capture(client);
    let ticket_id = ticket.id();
    let mut request = take_capture_request(handoff);
    assert_eq!(request.id().ticket(), ticket_id);
    assert_eq!(request.source().session(), publication.session());
    assert_eq!(
        request.source().project_generation(),
        publication.project_generation()
    );
    assert_eq!(
        request.source().source_digest(),
        publication.source_digest()
    );
    assert!(publication.tick() >= request.at_least_tick());

    let (view, png, metadata) = fake_ready_metadata(publication.clone(), present_sequence);
    complete_capture(
        &mut request,
        &CaptureCompletion::Captured(Box::new(metadata)),
    );
    let progress = wait_terminal(&ticket);
    assert_eq!(progress.id(), ticket_id);
    assert_eq!(progress.phase(), RequestPhase::Terminal);
    {
        let terminal = progress.terminal().expect("terminal result is present");
        let capture = match terminal {
            Ok(SessionRequestResult::CaptureFrame(capture)) => capture,
            other => panic!("expected a captured terminal result, got {other:?}"),
        };
        let artifact = capture.artifact();
        assert_eq!(artifact.session(), ticket_id.session());
        assert_eq!(artifact.id().ticket(), ticket_id);
        assert_eq!(artifact.id().ticket().client(), client.id());
        assert_eq!(artifact.id().ticket().sequence(), ticket_id.sequence());
        assert_eq!(capture.metadata().publication(), publication);
        assert_eq!(capture.metadata().view(), &view);
        assert_eq!(capture.metadata().png(), &png);
        assert_eq!(capture.metadata().present_sequence(), present_sequence);
        assert_eq!(capture.metadata().surface_format(), ready.surface_format());
        assert_eq!(
            capture.metadata().surface_encoding(),
            CapturePixelEncoding::SrgbEncoded
        );
    }
    (ticket, request, progress)
}

#[test]
fn capture_is_opt_in_and_starts_unresolved() {
    let _engine_guard = ENGINE_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project = EmptyProject::new();
    let options = NativeProjectPreparationOptions::new(project.path());

    let (mut capture_service, render_port, handoff) =
        NativeProjectActivationService::start_with_render_consumer_and_capture(
            options.clone(),
            SessionControlConfig::default(),
        )
        .expect("opt-in capture factory starts on an empty project");
    assert_eq!(handoff.try_status(), CaptureHandoffState::Starting);
    let client = capture_service
        .session()
        .attach()
        .expect("attach capture test client");
    assert!(matches!(
        client.try_submit(SessionOperation::CaptureFrame {
            at_least_tick: None,
            timeout: Some(WAIT),
        }),
        Err(SessionSubmitError::CaptureStarting)
    ));
    drop(client);
    drop(handoff);
    drop(render_port);
    shutdown(&mut capture_service);

    let mut legacy_service =
        NativeProjectActivationService::start_headless(options, SessionControlConfig::default())
            .expect("legacy no-capture factory still starts on the same empty project");
    let legacy_client = legacy_service
        .session()
        .attach()
        .expect("attach legacy test client");
    assert!(matches!(
        legacy_client.try_submit(SessionOperation::CaptureFrame {
            at_least_tick: None,
            timeout: Some(WAIT),
        }),
        Err(SessionSubmitError::CaptureUnavailable)
    ));
    drop(legacy_client);
    shutdown(&mut legacy_service);
}

#[test]
fn captured_terminal_uses_real_publication_identity_and_retained_owners_hold_image_slot() {
    let _engine_guard = ENGINE_START_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project = EmptyProject::new();
    let (mut service, render_port, handoff) =
        NativeProjectActivationService::start_with_render_consumer_and_capture(
            NativeProjectPreparationOptions::new(project.path()),
            SessionControlConfig::default(),
        )
        .expect("opt-in capture factory starts");
    let ready = CaptureReadyInfo::new(CaptureSurfaceFormat::Rgba8UnormSrgb, 2, 2, 4)
        .expect("fixture ready limit is valid");
    handoff
        .try_set_ready(ready)
        .expect("resolve capture handoff ready");
    assert_eq!(handoff.try_status(), CaptureHandoffState::Ready(ready));

    let client = service.session().attach().expect("attach capture client");
    let frame = latest_frame(&client);
    let publication = ProjectPublicationIdentity::from_frame(&frame)
        .expect("publication identity comes from the actor's actual frame");
    let (mut progress_ticket, progress_request, progress_only) =
        complete_captured_request(&client, &handoff, &publication, ready, 1);
    progress_ticket
        .acknowledge()
        .expect("acknowledge progress-retention capture");
    drop(progress_request);
    let mut last_successful_sequence = flush_actor(&client);
    assert_capture_capacity(&client);
    // With the request dropped and no explicit capability/work lease, this retained terminal
    // progress snapshot alone keeps its result capability and image slot alive after ACK.
    drop(progress_only);

    let (mut capability_ticket, capability_request, capability_progress) =
        complete_captured_request(&client, &handoff, &publication, ready, 2);
    assert_eq!(
        capability_ticket.id().sequence(),
        last_successful_sequence + 1,
        "progress-only retention blocks admission without consuming an ID"
    );
    let capability_only = capability_request.artifact().clone();
    capability_ticket
        .acknowledge()
        .expect("acknowledge capability-retention capture");
    drop(capability_progress);
    drop(capability_request);
    last_successful_sequence = flush_actor(&client);
    assert_capture_capacity(&client);
    // The terminal progress and request are gone; this explicit capability alone owns the slot.
    drop(capability_only);

    let (mut work_ticket, work_request, work_progress) =
        complete_captured_request(&client, &handoff, &publication, ready, 3);
    assert_eq!(
        work_ticket.id().sequence(),
        last_successful_sequence + 1,
        "capability-only retention blocks admission without consuming an ID"
    );
    let work_only = work_request.work_lease().clone();
    work_ticket
        .acknowledge()
        .expect("acknowledge work-retention capture");
    drop(work_progress);
    drop(work_request);
    last_successful_sequence = flush_actor(&client);
    assert_capture_capacity(&client);
    // After terminal progress, artifact capability, and request are gone, the work lease alone
    // keeps the slot reserved until the consumer relinquishes native work.
    drop(work_only);

    let mut next_ticket = submit_capture(&client);
    assert_eq!(
        next_ticket.id().sequence(),
        last_successful_sequence + 1,
        "work-only release restores admission and rejected attempts do not consume IDs"
    );
    let mut next_request = take_capture_request(&handoff);
    complete_capture(
        &mut next_request,
        &CaptureCompletion::Failed(CaptureFailure::Cancelled),
    );
    let next_progress = wait_terminal(&next_ticket);
    assert!(matches!(
        next_progress.terminal(),
        Some(Err(SessionRequestError::CaptureFailed(
            CaptureFailure::Cancelled
        )))
    ));
    drop(next_progress);
    next_ticket
        .acknowledge()
        .expect("acknowledge cancelled follow-up capture");

    drop(next_request);
    drop(client);
    drop(handoff);
    drop(render_port);
    shutdown(&mut service);
}
