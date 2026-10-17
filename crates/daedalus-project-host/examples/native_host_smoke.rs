//! Public SDK diagnostic for one real native managed window.
//!
//! This is an engine integration fixture, not a sample game. Run it on the main thread with an
//! existing prepared-project directory; a small controller thread observes the public host status
//! and requests a one-shot view close after the first actual game-frame present.

use daedalus_project_host::{
    NativePresentedSummary, NativeProjectHost, NativeProjectHostControl,
    NativeProjectHostLifecycle, NativeProjectHostOptions, NativeProjectHostShutdown,
    NativeProjectHostStatus, NativeProjectHostStatusRead, NativeProjectViewOptions,
};
use daedalus_runtime::CommandId;
use daedalus_runtime::{
    AssetActivationOperation, PreparationId, SessionClockMode, SessionControlClient,
    SessionControlConfig, SessionId, SessionOperation, SessionRequestError, SessionRequestResult,
    SessionStatus, WaitStage,
};
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

fn run(project: PathBuf, observe: Duration) -> Result<(), String> {
    let config = SessionControlConfig {
        clock_mode: SessionClockMode::Manual,
        ..SessionControlConfig::default()
    };
    let mut host = NativeProjectHost::start(
        NativeProjectHostOptions::for_project(project).with_session_config(config),
    )
    .map_err(|error| format!("could not start native project host: {error}"))?;
    let exercise = exercise_live_view(&mut host, observe);
    let shutdown = stop_and_observe(&mut host);
    exercise?;
    shutdown
}

fn parse_args() -> Result<(PathBuf, Duration), String> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let first = args.next().ok_or_else(|| {
        "usage: native_host_smoke [--observe-ms 0..15000] <existing-project-directory>".to_owned()
    })?;
    let (project, observe_ms) = if first.to_str() == Some("--observe-ms") {
        let raw_ms = args
            .next()
            .ok_or_else(|| "--observe-ms requires an integer in 0..=15000".to_owned())?;
        let observe_ms = raw_ms
            .to_str()
            .ok_or_else(|| "--observe-ms value is not valid UTF-8".to_owned())?
            .parse::<u64>()
            .map_err(|_| "--observe-ms requires an integer in 0..=15000".to_owned())?;
        let project = args.next().ok_or_else(|| {
            "expected an existing-project-directory after --observe-ms".to_owned()
        })?;
        (project, observe_ms)
    } else {
        (first, 0)
    };
    let observe = Duration::from_millis(observe_ms);
    if observe > MAX_OBSERVE {
        return Err("--observe-ms must be within 0..=15000".to_owned());
    }
    if args.next().is_some() {
        return Err("expected exactly one existing-project-directory argument".to_owned());
    }
    Ok((PathBuf::from(project), observe))
}

fn main() {
    let (project, observe) = match parse_args() {
        Ok(arguments) => arguments,
        Err(error) => {
            eprintln!("native host smoke: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(project, observe) {
        eprintln!("native host smoke failed: {error}");
        std::process::exit(1);
    }
}
