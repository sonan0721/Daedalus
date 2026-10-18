//! Public SDK diagnostic for one real native managed window.
//!
//! This is an engine integration fixture, not a sample game. Run it on the main thread with an
//! existing prepared-project directory; a small controller thread observes the public host status
//! and requests a one-shot view close after the first actual game-frame present.

#[cfg(feature = "capture")]
use daedalus_project_host::{
    NativeArtifactAccessError, NativeCaptureArtifactReader, NativeCaptureEncoderState,
    NativeCaptureStatusError, NativeProjectCaptureLifecycle,
};
use daedalus_project_host::{
    NativePresentedSummary, NativeProjectHost, NativeProjectHostControl,
    NativeProjectHostLifecycle, NativeProjectHostOptions, NativeProjectHostShutdown,
    NativeProjectHostStatus, NativeProjectHostStatusRead, NativeProjectViewOptions,
};
#[cfg(feature = "capture")]
use daedalus_renderer::WindowInputPolicy;
use daedalus_runtime::CommandId;
use daedalus_runtime::{
    AssetActivationOperation, PreparationId, SessionClockMode, SessionControlClient,
    SessionControlConfig, SessionId, SessionOperation, SessionRequestError, SessionRequestResult,
    SessionStatus, WaitStage,
};
#[cfg(feature = "capture")]
use daedalus_runtime::{
    CaptureArtifactId, CaptureFrameMetadata, CapturePngFormat, RequestAckError, RequestTicket,
    RequestTicketId, SessionSubmitError,
};
#[cfg(feature = "capture")]
use std::fs::{self, OpenOptions};
#[cfg(feature = "capture")]
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(feature = "capture")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
const PRESENT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OBSERVE: Duration = Duration::from_millis(15_000);
const VIEW_HARD_TIMEOUT: Duration = Duration::from_secs(80);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(feature = "capture")]
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(feature = "capture")]
const CAPTURE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "capture")]
const CAPTURE_READY_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(feature = "capture")]
const CAPTURE_CONTROLLER_JOIN_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(feature = "capture")]
const CAPTURE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "capture")]
const CAPTURE_READER_HOLD_OBSERVE: Duration = Duration::from_millis(500);

#[cfg(feature = "capture")]
struct CaptureProof {
    metadata: CaptureFrameMetadata,
    output_path: PathBuf,
    copied_bytes: u64,
    spool_path: PathBuf,
    spool_root: PathBuf,
    reader: NativeCaptureArtifactReader,
}

fn request(
    client: &SessionControlClient,
    operation: SessionOperation,
) -> Result<Result<SessionRequestResult, SessionRequestError>, String> {
    let mut ticket = client
        .try_submit(operation)
        .map_err(|error| format!("request submit failed: {error}"))?;
    let progress = ticket
        .wait_for(WaitStage::Terminal, REQUEST_TIMEOUT)
        .map_err(|error| format!("request did not reach a terminal result: {error}"))?;
    let terminal = progress
        .terminal()
        .cloned()
        .ok_or_else(|| "terminal wait returned without a terminal result".to_owned())?;
    drop(progress);
    ticket
        .acknowledge()
        .map_err(|error| format!("request acknowledgement failed: {error}"))?;
    Ok(terminal)
}

fn request_success(
    client: &SessionControlClient,
    operation: SessionOperation,
) -> Result<SessionRequestResult, String> {
    request(client, operation)?.map_err(|error| format!("actor rejected request: {error}"))
}

fn authority_status(client: &SessionControlClient) -> Result<SessionStatus, String> {
    match request_success(client, SessionOperation::Status)? {
        SessionRequestResult::Status(status) => Ok(status),
        other => Err(format!("Status returned unexpected result: {other:?}")),
    }
}

fn host_status(
    control: &NativeProjectHostControl,
    timeout: Duration,
) -> Result<NativeProjectHostStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match control
            .try_status()
            .map_err(|error| format!("host status read failed: {error}"))?
        {
            NativeProjectHostStatusRead::Value(status) => return Ok(status),
            NativeProjectHostStatusRead::Busy if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            NativeProjectHostStatusRead::Busy => {
                return Err("host status stayed busy until the bounded deadline".to_owned());
            }
        }
    }
}

fn validated_present(
    status: &NativeProjectHostStatus,
    expected_session: SessionId,
) -> Result<Option<NativePresentedSummary>, String> {
    let Some(presented) = status.last_presented() else {
        return Ok(None);
    };
    let queued = status.last_queued_publication().ok_or_else(|| {
        "actual present has no corresponding queued publication metadata".to_owned()
    })?;
    let authority_sequence = presented
        .authority_snapshot_sequence()
        .ok_or_else(|| "last-presented metadata has no authority snapshot sequence".to_owned())?;
    if presented.session() != expected_session
        || queued.session() != expected_session
        || queued.project_generation() != presented.project_generation()
        || queued.source_digest() != presented.source_digest()
        || queued.publication_sequence() < presented.publication_sequence()
        || queued.authority_snapshot_sequence() < authority_sequence
    {
        return Err("queued and actually presented publication identities disagree".to_owned());
    }
    Ok(Some(presented.clone()))
}

fn observe_present_and_close(
    control: NativeProjectHostControl,
    expected_session: SessionId,
    view_returned: Arc<AtomicBool>,
    observe: Duration,
) -> Result<NativePresentedSummary, String> {
    let deadline = Instant::now() + PRESENT_TIMEOUT;
    loop {
        if view_returned.load(Ordering::Acquire) {
            return Err(
                "view returned before an actual game-frame present was observed".to_owned(),
            );
        }
        match control.try_status() {
            Ok(NativeProjectHostStatusRead::Value(status)) => {
                if let Some(failure) = status.failure() {
                    control.request_view_close();
                    return Err(format!(
                        "host failed before presentation ({}): {}",
                        failure.code(),
                        failure.message()
                    ));
                }
                match validated_present(&status, expected_session) {
                    Ok(Some(mut observed)) => {
                        let observe_deadline = Instant::now() + observe;
                        while Instant::now() < observe_deadline {
                            if view_returned.load(Ordering::Acquire) {
                                return Err(
                                    "view returned during the bounded observation interval"
                                        .to_owned(),
                                );
                            }
                            match control.try_status() {
                                Ok(NativeProjectHostStatusRead::Value(latest)) => {
                                    if let Some(failure) = latest.failure() {
                                        control.request_view_close();
                                        return Err(format!(
                                            "host failed during presentation observation ({}): {}",
                                            failure.code(),
                                            failure.message()
                                        ));
                                    }
                                    if matches!(
                                        latest.lifecycle(),
                                        NativeProjectHostLifecycle::Stopping
                                            | NativeProjectHostLifecycle::Stopped
                                            | NativeProjectHostLifecycle::Failed
                                    ) {
                                        control.request_view_close();
                                        return Err(format!(
                                            "host reached {:?} during presentation observation",
                                            latest.lifecycle()
                                        ));
                                    }
                                    match validated_present(&latest, expected_session) {
                                        Ok(Some(next))
                                            if next.present_sequence()
                                                >= observed.present_sequence() =>
                                        {
                                            observed = next;
                                        }
                                        Ok(Some(_)) | Ok(None) => {}
                                        Err(error) => {
                                            control.request_view_close();
                                            return Err(error);
                                        }
                                    }
                                }
                                Ok(NativeProjectHostStatusRead::Busy) => {}
                                Err(error) => {
                                    control.request_view_close();
                                    return Err(format!(
                                        "host status polling failed during observation: {error}"
                                    ));
                                }
                            }
                            let remaining =
                                observe_deadline.saturating_duration_since(Instant::now());
                            thread::sleep(POLL_INTERVAL.min(remaining));
                        }
                        control.request_view_close();
                        return Ok(observed);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        control.request_view_close();
                        return Err(error);
                    }
                }
                if matches!(
                    status.lifecycle(),
                    NativeProjectHostLifecycle::Stopping
                        | NativeProjectHostLifecycle::Stopped
                        | NativeProjectHostLifecycle::Failed
                ) {
                    control.request_view_close();
                    return Err(format!(
                        "host reached {:?} before an actual present",
                        status.lifecycle()
                    ));
                }
            }
            Ok(NativeProjectHostStatusRead::Busy) => {}
            Err(error) => {
                control.request_view_close();
                return Err(format!("host status polling failed: {error}"));
            }
        }
        if Instant::now() >= deadline {
            control.request_view_close();
            return Err("timed out waiting for an actual game-frame present".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn exercise_live_view(host: &mut NativeProjectHost, observe: Duration) -> Result<(), String> {
    let client = host
        .attach_control()
        .map_err(|error| format!("could not attach controller to host: {error}"))?;
    let control = host.control();
    let startup = authority_status(&client)?;
    if startup.clock != SessionClockMode::Manual {
        return Err(format!(
            "host did not preserve the requested Manual clock: {:?}",
            startup.clock
        ));
    }
    let initial_host = host_status(&control, STATUS_TIMEOUT)?;
    if initial_host.session() != startup.session {
        return Err("host status and attached authority identify different sessions".to_owned());
    }

    let view_returned = Arc::new(AtomicBool::new(false));
    let observer_control = control.clone();
    let observer_returned = Arc::clone(&view_returned);
    let expected_session = startup.session;
    let observer = thread::Builder::new()
        .name("daedalus-native-host-smoke-observer".to_owned())
        .spawn(move || {
            observe_present_and_close(
                observer_control,
                expected_session,
                observer_returned,
                observe,
            )
        })
        .map_err(|error| format!("could not start bounded presentation observer: {error}"))?;

    let mut view_options = NativeProjectViewOptions::default();
    view_options.window.window.max_run_time = Some(VIEW_HARD_TIMEOUT);
    let view_result = host.run_view(view_options);
    view_returned.store(true, Ordering::Release);
    let observed = observer
        .join()
        .map_err(|_| "presentation observer thread panicked".to_owned())?;
    let outcome = view_result.map_err(|error| format!("managed native view failed: {error}"))?;
    let observed = observed?;
    if !outcome.status().view_closed() {
        return Err("run_view returned without reporting the one-shot view closed".to_owned());
    }

    let closed_host = host_status(&control, STATUS_TIMEOUT)?;
    if !closed_host.view_closed() || closed_host.session() != startup.session {
        return Err("host did not retain the same closed view/session state".to_owned());
    }
    let final_presented = closed_host
        .last_presented()
        .ok_or_else(|| "closed host has no actual-present metadata".to_owned())?;
    if final_presented.session() != startup.session
        || final_presented.present_sequence() < observed.present_sequence()
    {
        return Err("final host presentation regressed behind the observed present".to_owned());
    }

    let before_step = authority_status(&client)?;
    if before_step.session != startup.session || before_step.clock != SessionClockMode::Manual {
        return Err("view close changed the authority session or clock mode".to_owned());
    }
    match request_success(&client, SessionOperation::Step { count: 1 })? {
        SessionRequestResult::Step(reports) if reports.len() == 1 => {}
        other => return Err(format!("Manual Step returned unexpected result: {other:?}")),
    }
    let after_step = authority_status(&client)?;
    if after_step.session != startup.session
        || after_step.clock != SessionClockMode::Manual
        || after_step.tick <= before_step.tick
    {
        return Err("Manual Step did not advance the same authority after view close".to_owned());
    }

    // This candidate epoch is intentionally not prepared. Runtime checks the consumer's closed
    // state before candidate validity, so this must return the public typed unavailable error.
    match request(
        &client,
        SessionOperation::AssetActivation(AssetActivationOperation::Stage {
            command_id: Some(CommandId::from_raw(1)),
            candidate: PreparationId::from_parts(startup.session, 1),
            expected_active_generation: closed_host.active_generation(),
        }),
    )? {
        Err(SessionRequestError::AssetActivationUnavailable) => {}
        Err(error) => {
            return Err(format!(
                "closed consumer returned {error:?} instead of AssetActivationUnavailable"
            ));
        }
        Ok(result) => {
            return Err(format!(
                "closed consumer unexpectedly accepted stage request: {result:?}"
            ));
        }
    }

    let (width, height) = observed.physical_size();
    println!(
        "native_host_actual_present session={:?} tick={} publication={} authority_snapshot={} generation={} digest={} present={} physical={}x{}",
        observed.session(),
        observed.tick(),
        observed.publication_sequence(),
        observed.authority_snapshot_sequence().unwrap_or_default(),
        observed.project_generation(),
        observed.source_digest(),
        observed.present_sequence(),
        width,
        height,
    );
    println!(
        "native_host_view_closed=true consumer_stage_rejected=asset_activation_unavailable same_session_manual_step_tick={:?}->{:?}",
        before_step.tick, after_step.tick
    );
    drop(client);
    Ok(())
}

#[cfg(feature = "capture")]
fn wait_capture_ready(
    control: &NativeProjectHostControl,
    expected_session: SessionId,
    view_returned: &AtomicBool,
) -> Result<(), String> {
    let deadline = Instant::now() + CAPTURE_READY_TIMEOUT;
    loop {
        if view_returned.load(Ordering::Acquire) {
            return Err("view returned before capture became ready".to_owned());
        }
        match control.try_capture_status() {
            Ok(status) if status.session() != expected_session => {
                return Err("capture pipeline belongs to a different session".to_owned());
            }
            Ok(status) => match status.lifecycle() {
                NativeProjectCaptureLifecycle::Ready(_) => return Ok(()),
                NativeProjectCaptureLifecycle::Starting => {}
                NativeProjectCaptureLifecycle::Unsupported(reason) => {
                    return Err(format!("capture is unsupported: {reason:?}"));
                }
                NativeProjectCaptureLifecycle::Failed(failure) => {
                    return Err(format!(
                        "capture pipeline failed ({}): {}",
                        failure.code(),
                        failure.message()
                    ));
                }
                state => return Err(format!("capture entered terminal state {state:?}")),
            },
            Err(NativeCaptureStatusError::Busy) => {}
            Err(error) => return Err(format!("capture status read failed: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("capture did not become ready before the bounded deadline".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(feature = "capture")]
fn wait_actual_present(
    control: &NativeProjectHostControl,
    expected_session: SessionId,
    view_returned: &AtomicBool,
) -> Result<NativePresentedSummary, String> {
    let deadline = Instant::now() + PRESENT_TIMEOUT;
    loop {
        if view_returned.load(Ordering::Acquire) {
            return Err("view returned before an actual frame was presented".to_owned());
        }
        match control.try_status() {
            Ok(NativeProjectHostStatusRead::Value(status)) => {
                if let Some(failure) = status.failure() {
                    return Err(format!(
                        "host failed before capture ({}): {}",
                        failure.code(),
                        failure.message()
                    ));
                }
                if let Some(presented) = validated_present(&status, expected_session)? {
                    return Ok(presented);
                }
                if matches!(
                    status.lifecycle(),
                    NativeProjectHostLifecycle::Stopping
                        | NativeProjectHostLifecycle::Stopped
                        | NativeProjectHostLifecycle::Failed
                ) {
                    return Err(format!(
                        "host reached {:?} before the first actual present",
                        status.lifecycle()
                    ));
                }
            }
            Ok(NativeProjectHostStatusRead::Busy) => {}
            Err(error) => return Err(format!("host status read failed: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the initial actual present".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(feature = "capture")]
fn submit_capture(client: &SessionControlClient) -> Result<RequestTicket, String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        match client.try_submit(SessionOperation::CaptureFrame {
            at_least_tick: None,
            timeout: Some(CAPTURE_OPERATION_TIMEOUT),
        }) {
            Ok(ticket) => return Ok(ticket),
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                if Instant::now() < deadline =>
            {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(format!("capture request was rejected: {error}")),
        }
    }
}

#[cfg(feature = "capture")]
fn expect_capture_capacity(client: &SessionControlClient) -> Result<(), String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        match client.try_submit(SessionOperation::CaptureFrame {
            at_least_tick: None,
            timeout: Some(CAPTURE_OPERATION_TIMEOUT),
        }) {
            Err(SessionSubmitError::CaptureCapacity) => return Ok(()),
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull)
                if Instant::now() < deadline =>
            {
                thread::sleep(POLL_INTERVAL);
            }
            Ok(ticket) => {
                drop(ticket);
                return Err(
                    "capture slot admitted a second request while the first reader was live"
                        .to_owned(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "held-reader capture request returned {error:?}, expected CaptureCapacity"
                ));
            }
        }
    }
}

#[cfg(feature = "capture")]
fn wait_artifact_lease(
    control: &NativeProjectHostControl,
    result: &daedalus_runtime::CaptureFrameResult,
) -> Result<daedalus_project_host::NativeCaptureArtifactLease, String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        match control.try_acquire_capture_artifact(result.artifact()) {
            Ok(lease) => return Ok(lease),
            Err(NativeArtifactAccessError::Busy) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(format!("capture artifact acquisition failed: {error}")),
        }
    }
}

#[cfg(feature = "capture")]
fn wait_capture_ticket(
    mut ticket: RequestTicket,
    client: &SessionControlClient,
    control: &NativeProjectHostControl,
    expected_session: SessionId,
    output_dir: &Path,
    label: &str,
) -> Result<CaptureProof, String> {
    let ticket_id = ticket.id();
    if ticket_id.session() != expected_session
        || ticket_id.sequence() != 1
        || ticket_id.client() != client.id()
    {
        return Err(format!(
            "{label} capture did not use its client's first ticket (client={:?}, ticket={ticket_id:?})",
            client.id()
        ));
    }

    let progress = ticket
        .wait_for(WaitStage::Terminal, CAPTURE_TIMEOUT)
        .map_err(|error| format!("{label} capture did not complete: {error}"))?;
    let terminal = progress
        .terminal()
        .cloned()
        .ok_or_else(|| format!("{label} capture terminal wait returned no result"))?;
    drop(progress);
    let capture = match terminal {
        Ok(SessionRequestResult::CaptureFrame(capture)) => capture,
        Ok(other) => return Err(format!("{label} capture returned {other:?}")),
        Err(error) => return Err(format!("{label} capture actor failed: {error}")),
    };
    let artifact_id = capture.artifact().id();
    let metadata = capture.metadata().clone();
    capture_initial_metadata_is_consistent(&metadata, ticket_id)?;
    wait_capture_binding(control, &metadata)?;
    if capture.artifact().id().ticket() != ticket_id
        || capture.artifact().session() != ticket_id.session()
        || metadata.publication().session() != ticket_id.session()
        || metadata.view().physical_width() != metadata.png().width()
        || metadata.view().physical_height() != metadata.png().height()
        || metadata.png().format() != CapturePngFormat::Rgba8Srgb
    {
        return Err(format!(
            "{label} capture returned inconsistent image identity/metadata"
        ));
    }

    let lease = wait_artifact_lease(control, &capture)?;
    if lease.id() != capture.artifact().id() || lease.metadata() != capture.metadata() {
        return Err(format!(
            "{label} artifact lease changed the capture binding"
        ));
    }
    let spool_path = lease.local_spool_path().to_path_buf();
    let spool_root = spool_path
        .parent()
        .ok_or_else(|| "capture spool path has no private root directory".to_owned())?
        .to_path_buf();
    let mut reader = lease
        .open()
        .map_err(|error| format!("{label} artifact reader could not open: {error}"))?;
    let output_path = output_dir.join(format!("{label}.png"));
    let copied_bytes =
        copy_capture_to_fresh_file(&mut reader, &output_path, metadata.png().byte_length())?;
    drop(lease);
    drop(capture);
    acknowledge_capture_ticket(&mut ticket)?;
    drop(ticket);

    print_capture_record(
        label,
        ticket_id,
        artifact_id,
        &metadata,
        copied_bytes,
        &output_path,
    );
    Ok(CaptureProof {
        metadata,
        output_path,
        copied_bytes,
        spool_path,
        spool_root,
        reader,
    })
}

#[cfg(feature = "capture")]
fn copy_capture_to_fresh_file(
    reader: &mut NativeCaptureArtifactReader,
    output_path: &Path,
    expected_bytes: u64,
) -> Result<u64, String> {
    let mut created = false;
    let result = (|| {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output_path)
            .map_err(|error| format!("could not create fresh PNG copy: {error}"))?;
        created = true;
        let copied_bytes = io::copy(reader, &mut output)
            .map_err(|error| format!("could not copy capture PNG: {error}"))?;
        output
            .sync_all()
            .map_err(|error| format!("could not flush capture PNG copy: {error}"))?;
        if copied_bytes != expected_bytes
            || output.metadata().map_err(|error| error.to_string())?.len() != expected_bytes
        {
            return Err(format!(
                "copied PNG length {copied_bytes} differs from reported length {expected_bytes}"
            ));
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not rewind native artifact reader: {error}"))?;
        Ok(copied_bytes)
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(output_path);
    }
    result
}

#[cfg(feature = "capture")]
fn acknowledge_capture_ticket(ticket: &mut RequestTicket) -> Result<(), String> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    loop {
        match ticket.acknowledge() {
            Ok(()) => return Ok(()),
            Err(RequestAckError::Busy) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => return Err(format!("capture ticket acknowledgement failed: {error}")),
        }
    }
}

#[cfg(feature = "capture")]
fn wait_capture_binding(
    control: &NativeProjectHostControl,
    metadata: &CaptureFrameMetadata,
) -> Result<NativePresentedSummary, String> {
    let publication = metadata.publication();
    let deadline = Instant::now() + PRESENT_TIMEOUT;
    loop {
        match control.try_status() {
            Ok(NativeProjectHostStatusRead::Value(status)) => {
                if let Some(failure) = status.failure() {
                    return Err(format!(
                        "host failed while validating capture identity ({}): {}",
                        failure.code(),
                        failure.message()
                    ));
                }
                if let Some(presented) = validated_present(&status, publication.session())?
                    && presented.session() == publication.session()
                    && presented.tick() == publication.tick().0
                    && presented.publication_sequence() == publication.publication_sequence()
                    && presented.authority_snapshot_sequence()
                        == Some(publication.authority_snapshot_sequence())
                    && presented.project_generation() == publication.project_generation()
                    && presented.source_digest() == publication.source_digest()
                    && presented.present_sequence() >= metadata.present_sequence()
                    && presented.physical_size()
                        == (
                            metadata.view().physical_width(),
                            metadata.view().physical_height(),
                        )
                {
                    return Ok(presented);
                }
            }
            Ok(NativeProjectHostStatusRead::Busy) => {}
            Err(error) => return Err(format!("host status validation failed: {error}")),
        }
        if Instant::now() >= deadline {
            return Err("capture metadata did not match a real host present".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(feature = "capture")]
fn wait_spool_release(control: &NativeProjectHostControl, spool_path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + CAPTURE_SHUTDOWN_TIMEOUT;
    loop {
        let status = match control.try_capture_status() {
            Ok(status) => status,
            Err(NativeCaptureStatusError::Busy) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(format!("capture cleanup status failed: {error}")),
        };
        if status.pending_jobs() == 0
            && status.retained_png_bytes() == 0
            && !status.cleanup_pending()
            && !spool_path.exists()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "capture spool file did not disappear after its last reader dropped".to_owned(),
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(feature = "capture")]
fn capture_controller(
    control: NativeProjectHostControl,
    first_client: SessionControlClient,
    second_client: SessionControlClient,
    expected_session: SessionId,
    view_returned: Arc<AtomicBool>,
    output_dir: PathBuf,
    observe: Duration,
) -> Result<CaptureProof, String> {
    wait_capture_ready(&control, expected_session, &view_returned)?;
    wait_actual_present(&control, expected_session, &view_returned)?;

    let first_ticket = submit_capture(&first_client)?;
    let first = wait_capture_ticket(
        first_ticket,
        &first_client,
        &control,
        expected_session,
        &output_dir,
        "capture-1",
    )?;
    expect_capture_capacity(&second_client)?;

    let CaptureProof {
        metadata: first_metadata,
        output_path: first_path,
        copied_bytes: first_bytes,
        spool_path: first_spool,
        spool_root: _,
        reader: first_reader,
    } = first;
    drop(first_reader);
    wait_spool_release(&control, &first_spool)?;
    if !first_path.exists()
        || fs::metadata(&first_path)
            .map_err(|error| error.to_string())?
            .len()
            != first_bytes
    {
        return Err("first copied PNG was not retained with the reported length".to_owned());
    }
    println!(
        "native_capture_capacity_rejected=true first_reader_released=true first_spool_deleted=true first_publication={} first_generation={} first_digest={}",
        first_metadata.publication().publication_sequence(),
        first_metadata.publication().project_generation(),
        first_metadata.publication().source_digest(),
    );

    let second_ticket = submit_capture(&second_client)?;
    let mut second = wait_capture_ticket(
        second_ticket,
        &second_client,
        &control,
        expected_session,
        &output_dir,
        "capture-2",
    )?;
    let second_reader_position = second
        .reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("second reader could not rewind: {error}"))?;
    if second_reader_position != 0 {
        return Err("second reader did not rewind to byte zero".to_owned());
    }

    let observe_deadline = Instant::now() + observe;
    while Instant::now() < observe_deadline {
        if view_returned.load(Ordering::Acquire) {
            return Err("view returned during capture observation".to_owned());
        }
        match control.try_status() {
            Ok(NativeProjectHostStatusRead::Value(status)) => {
                if let Some(failure) = status.failure() {
                    return Err(format!(
                        "host failed while holding second reader ({}): {}",
                        failure.code(),
                        failure.message()
                    ));
                }
            }
            Ok(NativeProjectHostStatusRead::Busy) => {}
            Err(error) => {
                return Err(format!(
                    "host status read failed during observation: {error}"
                ));
            }
        }
        let remaining = observe_deadline.saturating_duration_since(Instant::now());
        thread::sleep(POLL_INTERVAL.min(remaining));
    }

    println!(
        "native_capture_reader_held_for_view_close=true second_publication={} second_present={} second_png={}x{} bytes={} sha256={}",
        second.metadata.publication().publication_sequence(),
        second.metadata.present_sequence(),
        second.metadata.png().width(),
        second.metadata.png().height(),
        second.metadata.png().byte_length(),
        hex_bytes(second.metadata.png().sha256()),
    );
    Ok(second)
}

#[cfg(feature = "capture")]
fn capture_initial_metadata_is_consistent(
    metadata: &CaptureFrameMetadata,
    ticket_id: RequestTicketId,
) -> Result<(), String> {
    if metadata.publication().session() != ticket_id.session()
        || metadata.publication().project_generation() == 0
        || metadata.publication().publication_sequence() == 0
        || metadata.publication().source_digest().is_empty()
        || metadata.present_sequence() == 0
    {
        return Err("capture metadata has an invalid session or publication identity".to_owned());
    }
    Ok(())
}

#[cfg(feature = "capture")]
fn print_capture_record(
    label: &str,
    ticket_id: RequestTicketId,
    artifact_id: CaptureArtifactId,
    metadata: &CaptureFrameMetadata,
    copied_bytes: u64,
    output_path: &Path,
) {
    let publication = metadata.publication();
    let view = metadata.view();
    let png = metadata.png();
    println!(
        "native_capture fixture=true actual_host_present_verified=true reader_retained_across_ack=true label={label} ticket_session={:?} ticket_client={:?} ticket_sequence={} artifact_session={:?} artifact_client={:?} artifact_sequence={} tick={} publication={} authority_snapshot={} generation={} digest={} present={} physical={}x{} camera_position={:?} camera_target={:?} camera_up={:?} fov={} near={} far={} projection={:?} view_projection={:?} surface={:?} encoding={:?} png_format={:?} png_bytes={} png_sha256={} copied_bytes={} copied_path={}",
        ticket_id.session(),
        ticket_id.client(),
        ticket_id.sequence(),
        artifact_id.session(),
        artifact_id.ticket().client(),
        artifact_id.ticket().sequence(),
        publication.tick().0,
        publication.publication_sequence(),
        publication.authority_snapshot_sequence(),
        publication.project_generation(),
        publication.source_digest(),
        metadata.present_sequence(),
        view.physical_width(),
        view.physical_height(),
        view.camera_position(),
        view.camera_target(),
        view.camera_up(),
        view.vertical_fov_radians(),
        view.near(),
        view.far(),
        view.projection(),
        view.view_projection(),
        metadata.surface_format(),
        metadata.surface_encoding(),
        png.format(),
        png.byte_length(),
        hex_bytes(png.sha256()),
        copied_bytes,
        output_path.display(),
    );
}

#[cfg(feature = "capture")]
fn hex_bytes(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

#[cfg(feature = "capture")]
fn exercise_live_capture_view(
    host: &mut NativeProjectHost,
    output_dir: PathBuf,
    observe: Duration,
) -> Result<CaptureProof, String> {
    let control = host.control();
    let initial = host_status(&control, STATUS_TIMEOUT)?;
    let initial_session = initial.session();
    let first_client = host
        .attach_control()
        .map_err(|error| format!("could not attach first capture-only client: {error}"))?;
    let second_client = host
        .attach_control()
        .map_err(|error| format!("could not attach second capture-only client: {error}"))?;

    let view_returned = Arc::new(AtomicBool::new(false));
    let observer_control = control.clone();
    let observer_returned = Arc::clone(&view_returned);
    let observer = thread::Builder::new()
        .name("daedalus-native-capture-observer".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture_controller(
                    observer_control.clone(),
                    first_client,
                    second_client,
                    initial_session,
                    observer_returned,
                    output_dir,
                    observe,
                )
            }));
            observer_control.request_view_close();
            match result {
                Ok(result) => result,
                Err(_) => Err("capture controller panicked".to_owned()),
            }
        })
        .map_err(|error| format!("could not start capture observer: {error}"))?;

    let mut view_options = NativeProjectViewOptions::default();
    view_options.window.window.max_run_time = Some(VIEW_HARD_TIMEOUT);
    let view_result =
        host.run_view_with_input_policy(view_options, WindowInputPolicy::ProtocolOnly);
    view_returned.store(true, Ordering::Release);
    let join_deadline = Instant::now() + CAPTURE_CONTROLLER_JOIN_TIMEOUT;
    while !observer.is_finished() && Instant::now() < join_deadline {
        thread::sleep(POLL_INTERVAL);
    }
    if !observer.is_finished() {
        host.request_stop();
        let shutdown_deadline = Instant::now() + CAPTURE_CONTROLLER_JOIN_TIMEOUT;
        while !observer.is_finished() && Instant::now() < shutdown_deadline {
            thread::sleep(POLL_INTERVAL);
        }
    }
    if !observer.is_finished() {
        return Err("capture observer did not finish within the bounded close deadline".to_owned());
    }
    let observer_result = observer
        .join()
        .map_err(|_| "capture observer thread panicked".to_owned())?;
    let outcome =
        view_result.map_err(|error| format!("managed native capture view failed: {error}"))?;
    let proof = observer_result?;
    if !outcome.status().view_closed() {
        return Err("capture view returned without reporting the one-shot view closed".to_owned());
    }
    if outcome.status().session() != initial_session {
        return Err("capture view changed the host session".to_owned());
    }
    Ok(proof)
}

#[cfg(feature = "capture")]
fn finish_capture_host(
    host: &mut NativeProjectHost,
    mut proof: CaptureProof,
) -> Result<(), String> {
    let control = host.control();
    host.request_stop();
    let cleanup_start_deadline = Instant::now() + CAPTURE_SHUTDOWN_TIMEOUT;
    loop {
        match host.try_shutdown().map_err(|error| {
            format!("host shutdown polling failed while waiting for encoder cleanup: {error}")
        })? {
            NativeProjectHostShutdown::Stopping => {}
            NativeProjectHostShutdown::Stopped => {
                return Err("host reported Stopped before reader cleanup began".to_owned());
            }
        }

        let status = match control.try_capture_status() {
            Ok(status) => status,
            Err(NativeCaptureStatusError::Busy) if Instant::now() < cleanup_start_deadline => {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            Err(NativeCaptureStatusError::Busy) => {
                return Err(
                    "capture status remained busy before reader-retention observation".to_owned(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "capture status failed while waiting for encoder cleanup: {error}"
                ));
            }
        };
        match status.lifecycle() {
            NativeProjectCaptureLifecycle::Stopping => {}
            NativeProjectCaptureLifecycle::Failed(failure) => {
                return Err(format!(
                    "capture failed before reader-retention observation ({}): {}",
                    failure.code(),
                    failure.message()
                ));
            }
            NativeProjectCaptureLifecycle::Stopped => {
                return Err("capture reported Stopped before reader cleanup began".to_owned());
            }
            lifecycle => {
                return Err(format!(
                    "capture lifecycle was {lifecycle:?} before reader-retention observation"
                ));
            }
        }
        if let Some(failure) = status.failure() {
            return Err(format!(
                "capture failed before reader-retention observation ({}): {}",
                failure.code(),
                failure.message()
            ));
        }

        let expected_png_bytes = proof.metadata.png().byte_length();
        let spool_bytes = fs::metadata(&proof.spool_path)
            .map_err(|error| format!("retained capture spool became unavailable: {error}"))?
            .len();
        if status.pending_jobs() != 0
            || status.retained_png_bytes() != expected_png_bytes
            || spool_bytes != expected_png_bytes
        {
            return Err(format!(
                "retained capture changed before reader-retention observation: pending_jobs={} retained_png_bytes={} expected_png_bytes={} spool_bytes={}",
                status.pending_jobs(),
                status.retained_png_bytes(),
                expected_png_bytes,
                spool_bytes
            ));
        }
        proof
            .reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("retained capture reader could not seek: {error}"))?;
        let mut signature = [0u8; 8];
        proof
            .reader
            .read_exact(&mut signature)
            .map_err(|error| format!("retained capture reader could not read: {error}"))?;
        if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
            return Err("retained reader no longer starts with the PNG signature".to_owned());
        }

        if status.encoder_state() == &NativeCaptureEncoderState::Stopping
            && status.cleanup_pending()
        {
            break;
        }
        if Instant::now() >= cleanup_start_deadline {
            return Err(
                "encoder cleanup did not become observable while the reader was retained"
                    .to_owned(),
            );
        }
        thread::sleep(POLL_INTERVAL);
    }

    let hold_deadline = Instant::now() + CAPTURE_READER_HOLD_OBSERVE;
    let mut observed_stopping = 0usize;
    while Instant::now() < hold_deadline {
        match host.try_shutdown().map_err(|error| {
            format!("host shutdown polling failed while reader was retained: {error}")
        })? {
            NativeProjectHostShutdown::Stopping => {}
            NativeProjectHostShutdown::Stopped => {
                return Err(
                    "host reported Stopped while a native artifact reader was retained".to_owned(),
                );
            }
        }
        let status = loop {
            match control.try_capture_status() {
                Ok(status) => break status,
                Err(NativeCaptureStatusError::Busy) if Instant::now() < hold_deadline => {
                    thread::sleep(POLL_INTERVAL);
                }
                Err(NativeCaptureStatusError::Busy) => {
                    return Err(
                        "capture status remained busy during reader retention proof".to_owned()
                    );
                }
                Err(error) => return Err(format!("capture status failed during stop: {error}")),
            }
        };
        match status.lifecycle() {
            NativeProjectCaptureLifecycle::Stopping => {}
            NativeProjectCaptureLifecycle::Failed(failure) => {
                return Err(format!(
                    "capture failed during stop ({}): {}",
                    failure.code(),
                    failure.message()
                ));
            }
            lifecycle => {
                return Err(format!(
                    "capture lifecycle was {lifecycle:?} while reader was retained"
                ));
            }
        }
        if status.pending_jobs() != 0
            || status.encoder_state() != &NativeCaptureEncoderState::Stopping
            || status.retained_png_bytes() != proof.metadata.png().byte_length()
            || !status.cleanup_pending()
            || fs::metadata(&proof.spool_path)
                .map(|metadata| metadata.len())
                .ok()
                != Some(proof.metadata.png().byte_length())
        {
            return Err(format!(
                "reader retention state changed unexpectedly: pending_jobs={} encoder_state={:?} retained_png_bytes={} expected_png_bytes={} cleanup_pending={} spool_bytes={:?}",
                status.pending_jobs(),
                status.encoder_state(),
                status.retained_png_bytes(),
                proof.metadata.png().byte_length(),
                status.cleanup_pending(),
                fs::metadata(&proof.spool_path)
                    .map(|metadata| metadata.len())
                    .ok()
            ));
        }
        proof
            .reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("reader could not seek after host stop request: {error}"))?;
        let mut signature = [0u8; 8];
        proof
            .reader
            .read_exact(&mut signature)
            .map_err(|error| format!("reader could not read after host stop request: {error}"))?;
        if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
            return Err("retained reader no longer starts with the PNG signature".to_owned());
        }
        observed_stopping += 1;
        let remaining = hold_deadline.saturating_duration_since(Instant::now());
        thread::sleep(POLL_INTERVAL.min(remaining));
    }
    if observed_stopping == 0 {
        return Err("reader retention interval produced no Stopping observations".to_owned());
    }
    println!(
        "native_capture_reader_retention_observations={} retained_png_bytes={} spool_exists=true cleanup_pending=true",
        observed_stopping,
        proof.metadata.png().byte_length(),
    );

    let mut reader = proof.reader;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("reader could not seek after host stop request: {error}"))?;
    let mut signature = [0u8; 8];
    reader
        .read_exact(&mut signature)
        .map_err(|error| format!("reader could not read after host stop request: {error}"))?;
    if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
        return Err("retained reader no longer starts with the PNG signature".to_owned());
    }
    drop(reader);
    wait_spool_release(&control, &proof.spool_path)?;
    if !proof.output_path.exists()
        || fs::metadata(&proof.output_path)
            .map_err(|error| error.to_string())?
            .len()
            != proof.copied_bytes
    {
        return Err("copied PNG was not preserved for independent verification".to_owned());
    }

    stop_and_observe_with_timeout(host, CAPTURE_SHUTDOWN_TIMEOUT)?;
    if proof.spool_root.exists() {
        return Err("private capture spool root remains after host shutdown".to_owned());
    }
    let status = control
        .try_capture_status()
        .map_err(|error| format!("capture status unavailable after shutdown: {error}"))?;
    if status.lifecycle() != &NativeProjectCaptureLifecycle::Stopped
        || status.pending_jobs() != 0
        || status.retained_png_bytes() != 0
        || status.cleanup_pending()
    {
        return Err("capture status did not report fully drained after reader release".to_owned());
    }
    println!(
        "native_capture_shutdown=stopped reader_survived_stop=true spool_deleted=true spool_root_deleted=true copied_pngs=2"
    );
    Ok(())
}

#[cfg(feature = "capture")]
fn cleanup_after_capture_error(host: &mut NativeProjectHost, error: String) -> Result<(), String> {
    host.request_stop();
    match stop_and_observe_with_timeout(host, CAPTURE_SHUTDOWN_TIMEOUT) {
        Ok(()) => Err(error),
        Err(shutdown_error) => Err(format!(
            "{error}; capture host cleanup also failed: {shutdown_error}"
        )),
    }
}

#[cfg(feature = "capture")]
fn stop_and_observe_with_timeout(
    host: &mut NativeProjectHost,
    timeout: Duration,
) -> Result<(), String> {
    host.request_stop();
    let deadline = Instant::now() + timeout;
    loop {
        match host
            .try_shutdown()
            .map_err(|error| format!("host shutdown polling failed: {error}"))?
        {
            NativeProjectHostShutdown::Stopped => break,
            NativeProjectHostShutdown::Stopping if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            NativeProjectHostShutdown::Stopping => {
                return Err("host remained Stopping past the capture shutdown deadline".to_owned());
            }
        }
    }
    let control = host.control();
    let status = host_status(&control, STATUS_TIMEOUT)?;
    if status.lifecycle() != NativeProjectHostLifecycle::Stopped {
        return Err(format!(
            "shutdown returned Stopped but host lifecycle is {:?}",
            status.lifecycle()
        ));
    }
    let report = host
        .shutdown_report()
        .ok_or_else(|| "Stopped host did not expose its actor shutdown report".to_owned())?;
    println!(
        "native_host_shutdown=stopped session={:?} termination={:?} final_tick={:?}",
        status.session(),
        report.termination,
        report.final_tick
    );
    Ok(())
}

fn stop_and_observe(host: &mut NativeProjectHost) -> Result<(), String> {
    host.request_stop();
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        match host
            .try_shutdown()
            .map_err(|error| format!("host shutdown polling failed: {error}"))?
        {
            NativeProjectHostShutdown::Stopped => break,
            NativeProjectHostShutdown::Stopping if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            NativeProjectHostShutdown::Stopping => {
                return Err("host remained Stopping past the bounded shutdown deadline".to_owned());
            }
        }
    }
    let control = host.control();
    let status = host_status(&control, STATUS_TIMEOUT)?;
    if status.lifecycle() != NativeProjectHostLifecycle::Stopped {
        return Err(format!(
            "shutdown returned Stopped but host lifecycle is {:?}",
            status.lifecycle()
        ));
    }
    let report = host
        .shutdown_report()
        .ok_or_else(|| "Stopped host did not expose its actor shutdown report".to_owned())?;
    println!(
        "native_host_shutdown=stopped session={:?} termination={:?} final_tick={:?}",
        status.session(),
        report.termination,
        report.final_tick
    );
    Ok(())
}

fn run(
    project: PathBuf,
    observe: Duration,
    capture_output_dir: Option<PathBuf>,
) -> Result<(), String> {
    #[cfg(feature = "capture")]
    if let Some(output_dir) = capture_output_dir.as_ref() {
        fs::create_dir(output_dir)
            .map_err(|error| format!("capture output directory must be fresh: {error}"))?;
    }
    #[cfg(not(feature = "capture"))]
    if capture_output_dir.is_some() {
        return Err(
            "--capture-out-dir requires building native_host_smoke with the capture feature"
                .to_owned(),
        );
    }

    let config = SessionControlConfig {
        clock_mode: SessionClockMode::Manual,
        ..SessionControlConfig::default()
    };
    let options = NativeProjectHostOptions::for_project(project).with_session_config(config);
    #[cfg(feature = "capture")]
    let options = if capture_output_dir.is_some() {
        options.with_live_capture()
    } else {
        options
    };
    let mut host = NativeProjectHost::start(options)
        .map_err(|error| format!("could not start native project host: {error}"))?;

    #[cfg(feature = "capture")]
    if let Some(output_dir) = capture_output_dir {
        return match exercise_live_capture_view(&mut host, output_dir, observe) {
            Ok(proof) => match finish_capture_host(&mut host, proof) {
                Ok(()) => Ok(()),
                Err(error) => cleanup_after_capture_error(&mut host, error),
            },
            Err(error) => cleanup_after_capture_error(&mut host, error),
        };
    }

    let exercise = exercise_live_view(&mut host, observe);
    let shutdown = stop_and_observe(&mut host);
    exercise?;
    shutdown
}

fn parse_args() -> Result<(PathBuf, Duration, Option<PathBuf>), String> {
    let mut args = std::env::args_os().skip(1);
    let mut project = None;
    let mut observe_ms = 0;
    let mut saw_observe = false;
    let mut capture_output_dir = None;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--observe-ms") if !saw_observe => {
                saw_observe = true;
                let raw_ms = args
                    .next()
                    .ok_or_else(|| "--observe-ms requires an integer in 0..=15000".to_owned())?;
                observe_ms = raw_ms
                    .to_str()
                    .ok_or_else(|| "--observe-ms value is not valid UTF-8".to_owned())?
                    .parse::<u64>()
                    .map_err(|_| "--observe-ms requires an integer in 0..=15000".to_owned())?;
            }
            Some("--capture-out-dir") if capture_output_dir.is_none() => {
                let path = args.next().ok_or_else(|| {
                    "--capture-out-dir requires a fresh directory path".to_owned()
                })?;
                capture_output_dir = Some(PathBuf::from(path));
            }
            Some(value) if value.starts_with("--") => {
                return Err(format!("unknown or repeated option: {value}"));
            }
            _ => {
                if project.replace(PathBuf::from(argument)).is_some() {
                    return Err(
                        "expected exactly one existing-project-directory argument".to_owned()
                    );
                }
            }
        }
    }
    let project = project.ok_or_else(|| {
        "usage: native_host_smoke [--observe-ms 0..15000] [--capture-out-dir <fresh-dir>] <existing-project-directory>".to_owned()
    })?;
    let observe = Duration::from_millis(observe_ms);
    if observe > MAX_OBSERVE {
        return Err("--observe-ms must be within 0..=15000".to_owned());
    }
    #[cfg(not(feature = "capture"))]
    if capture_output_dir.is_some() {
        return Err(
            "--capture-out-dir requires building native_host_smoke with the capture feature"
                .to_owned(),
        );
    }
    Ok((project, observe, capture_output_dir))
}

fn main() {
    let (project, observe, capture_output_dir) = match parse_args() {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("native host smoke: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(project, observe, capture_output_dir) {
        eprintln!("native host smoke failed: {error}");
        std::process::exit(1);
    }
}
