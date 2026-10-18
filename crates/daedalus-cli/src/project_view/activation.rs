//! CLI adapter for the native managed project host.

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use daedalus_content::scene::StableId;
use daedalus_project_host::{
    NativeProjectHost, NativeProjectHostOptions, NativeProjectHostShutdown,
    NativeProjectHostStatus, NativeProjectHostStatusRead, NativeProjectViewOptions,
    NativeViewOutcome,
};
use daedalus_renderer::{
    ManagedPacketWindowOptions, WindowInput, WindowOptions, window_input_channel,
};
use daedalus_runtime::{
    EngineConfig, ProjectInfoPage, ProjectPublicationIdentity, RequestTicket, SessionControlClient,
    SessionControlConfig, SessionControlLimits, SessionEntityTarget, SessionOperation,
    SessionRequestResult,
};
use serde_json::{Value, json};

use super::super::project_view_control::{ProtocolController, StdioTransport};
use super::{
    CONTROL_TIMEOUT, COORDINATOR_SLEEP, CoordinatorReport, INPUT_QUEUE_CAPACITY, InputTicket,
    MAX_INPUT_EVENTS_PER_LOOP, ProjectViewArgs, handle_window_input, progress_input_ticket,
    progress_step_ticket, submit_wait,
};

const HOST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct InputControlPump {
    protocol_client: SessionControlClient,
    transport: StdioTransport,
    input_client: SessionControlClient,
    input_events: Receiver<WindowInput>,
    controlled_entity: Option<StableId>,
    control: daedalus_project_host::NativeProjectHostControl,
    view_status_session: daedalus_core::SessionId,
    clock: daedalus_runtime::SessionClockMode,
    limits: SessionControlLimits,
    stop: Arc<AtomicBool>,
}

struct ActivationExecution {
    project: Option<ProjectInfoPage>,
    view: Option<Result<NativeViewOutcome, String>>,
    input: Option<CoordinatorReport>,
    failure: Option<String>,
}

impl ActivationExecution {
    fn failed(project: Option<ProjectInfoPage>, error: impl Into<String>) -> Self {
        Self {
            project,
            view: None,
            input: None,
            failure: Some(error.into()),
        }
    }
}

pub(super) fn run(args: ProjectViewArgs) -> Result<(), Box<dyn Error>> {
    let clock = args.clock.runtime();
    let control_limits = SessionControlLimits::default();
    let session = SessionControlConfig {
        engine: EngineConfig::default(),
        clock_mode: clock,
        limits: control_limits,
    };
    let mut host = NativeProjectHost::start(
        NativeProjectHostOptions::for_project(&args.directory).with_session_config(session),
    )?;

    let execution = execute_view(&mut host, &args, clock, control_limits);
    host.request_stop();
    let shutdown_state = wait_for_host_shutdown(&mut host);
    let status = read_host_status(&host);
    let shutdown_report = host.shutdown_report();
    let execution_error = execution.failure.clone().or_else(|| {
        execution
            .view
            .as_ref()
            .and_then(|result| result.as_ref().err().cloned())
    });

    let report = build_report(
        &args,
        &execution,
        status.as_ref(),
        shutdown_report,
        shutdown_state.as_ref(),
    );
    super::emit_report(&args, report)?;

    if let Some(error) = execution_error {
        return Err(error.into());
    }
    match shutdown_state {
        Err(error) => return Err(error.into()),
        Ok(NativeProjectHostShutdown::Stopping) => {
            return Err("native project host is still stopping after the shutdown deadline".into());
        }
        Ok(NativeProjectHostShutdown::Stopped) => {}
    }
    let Some(shutdown_report) = shutdown_report else {
        return Err("native project host stopped without a shutdown report".into());
    };
    if let Some(error) = super::session_termination_error(&shutdown_report.termination) {
        return Err(error.into());
    }
    if let Some(failure) = status.as_ref().and_then(NativeProjectHostStatus::failure) {
        return Err(format!(
            "native project host failed ({}): {}",
            failure.code(),
            failure.message()
        )
        .into());
    }
    Ok(())
}

fn execute_view(
    host: &mut NativeProjectHost,
    args: &ProjectViewArgs,
    clock: daedalus_runtime::SessionClockMode,
    limits: SessionControlLimits,
) -> ActivationExecution {
    let input_client = match host.attach_control() {
        Ok(client) => client,
        Err(error) => return ActivationExecution::failed(None, error.to_string()),
    };
    let project = match project_info(&input_client) {
        Ok(page) => page,
        Err(error) => return ActivationExecution::failed(None, error.to_string()),
    };

    let controlled_entity = match args
        .controlled_entity
        .as_ref()
        .map(|value| StableId::new(value.clone()))
        .transpose()
    {
        Ok(value) => value,
        Err(error) => return ActivationExecution::failed(Some(project), error.to_string()),
    };
    if let Some(stable_id) = controlled_entity.as_ref()
        && let Err(error) = validate_controlled_entity(&input_client, stable_id)
    {
        return ActivationExecution::failed(Some(project), error.to_string());
    }

    let protocol_client = match host.attach_control() {
        Ok(client) => client,
        Err(error) => return ActivationExecution::failed(Some(project), error.to_string()),
    };
    let transport = match StdioTransport::start() {
        Ok(transport) => transport,
        Err(error) => return ActivationExecution::failed(Some(project), error.to_string()),
    };
    let (input_sink, input_events) = match window_input_channel(INPUT_QUEUE_CAPACITY) {
        Ok(channel) => channel,
        Err(error) => return ActivationExecution::failed(Some(project), error.to_string()),
    };

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let view_control = host.control();
    let view_status_session = project.session;
    let pump = thread::Builder::new()
        .name("daedalus-project-view-control".to_owned())
        .spawn(move || {
            run_input_control_pump(InputControlPump {
                protocol_client,
                transport,
                input_client,
                input_events,
                controlled_entity,
                control: view_control,
                view_status_session,
                clock,
                limits,
                stop: thread_stop,
            })
        });
    let pump = match pump {
        Ok(pump) => pump,
        Err(error) => return ActivationExecution::failed(Some(project), error.to_string()),
    };

    let window = ManagedPacketWindowOptions {
        window: WindowOptions {
            title: format!("Daedalus — {}", project.name),
            width: args.width,
            height: args.height,
            input_events: Some(input_sink),
            max_run_time: args
                .seconds
                .map(|seconds| Duration::from_secs(u64::from(seconds))),
            max_frames: args.frames,
            initial_loading: true,
            fail_on_presentation_disconnect: true,
            ..WindowOptions::default()
        },
        ..ManagedPacketWindowOptions::default()
    };
    let view = host
        .run_view(NativeProjectViewOptions {
            window,
            view_override: None,
        })
        .map_err(|error| error.to_string());

    let (input, pump_failure) = stop_input_control_pump(host, stop.as_ref(), pump);

    ActivationExecution {
        project: Some(project),
        view: Some(view),
        input,
        failure: pump_failure,
    }
}

fn stop_input_control_pump(
    host: &NativeProjectHost,
    stop: &AtomicBool,
    pump: thread::JoinHandle<CoordinatorReport>,
) -> (Option<CoordinatorReport>, Option<String>) {
    stop.store(true, Ordering::Release);
    host.request_stop();
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    while !pump.is_finished() {
        if Instant::now() >= deadline {
            drop(pump);
            return (
                None,
                Some("project view control pump did not stop before its deadline".to_owned()),
            );
        }
        thread::sleep(COORDINATOR_SLEEP);
    }

    match pump.join() {
        Ok(report) => (Some(report), None),
        Err(_) => (None, Some("project view control pump panicked".to_owned())),
    }
}

fn project_info(client: &SessionControlClient) -> Result<ProjectInfoPage, Box<dyn Error>> {
    match submit_wait(
        client,
        SessionOperation::ProjectInfo {
            offset: 0,
            limit: Some(1),
        },
        CONTROL_TIMEOUT,
    )? {
        SessionRequestResult::ProjectInfo(page) => Ok(page),
        _ => Err("project info request returned an unexpected result".into()),
    }
}

fn validate_controlled_entity(
    client: &SessionControlClient,
    stable_id: &StableId,
) -> Result<(), Box<dyn Error>> {
    match submit_wait(
        client,
        SessionOperation::GetTransform {
            target: SessionEntityTarget::StableId(stable_id.clone()),
        },
        CONTROL_TIMEOUT,
    )? {
        SessionRequestResult::Transform(_) => Ok(()),
        _ => Err(format!("controlled entity '{stable_id}' returned an unexpected result").into()),
    }
}

fn run_input_control_pump(pump: InputControlPump) -> CoordinatorReport {
    let InputControlPump {
        protocol_client,
        transport,
        input_client,
        input_events,
        controlled_entity,
        control,
        view_status_session,
        clock,
        limits,
        stop,
    } = pump;
    let mut protocol = ProtocolController::new(transport, protocol_client, clock, limits, false);
    protocol.attach_managed_view_status(control.clone(), view_status_session);
    let mut report = CoordinatorReport::default();
    let mut input_ticket: Option<InputTicket> = None;
    let mut step_ticket: Option<RequestTicket> = None;
    let mut close_requested = false;

    while !stop.load(Ordering::Acquire) && !protocol.session_stopping() {
        protocol.pump();
        let closing = protocol.closing();
        if closing && !close_requested {
            control.request_view_close();
            close_requested = true;
        }

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
                    &input_client,
                    &mut input_ticket,
                    &mut step_ticket,
                    &mut report,
                );
            }
        }
        progress_input_ticket(&input_client, &mut input_ticket, &mut report, !closing);
        progress_step_ticket(&mut step_ticket, &mut report);
        thread::sleep(COORDINATOR_SLEEP);
    }

    report.protocol_requests = protocol.accepted_requests();
    report.protocol_errors = protocol.errors();
    report.duplicate_submissions = protocol.duplicate_submissions();
    report.retained_protocol_tickets = protocol.retained_tickets();
    report.protocol_detach_reason = protocol.detach_reason().map(str::to_owned);
    report
}

fn wait_for_host_shutdown(
    host: &mut NativeProjectHost,
) -> Result<NativeProjectHostShutdown, String> {
    let deadline = Instant::now() + HOST_SHUTDOWN_TIMEOUT;
    loop {
        match host.try_shutdown() {
            Ok(NativeProjectHostShutdown::Stopped) => {
                return Ok(NativeProjectHostShutdown::Stopped);
            }
            Ok(NativeProjectHostShutdown::Stopping) if Instant::now() < deadline => {
                thread::sleep(COORDINATOR_SLEEP);
            }
            Ok(NativeProjectHostShutdown::Stopping) => {
                return Ok(NativeProjectHostShutdown::Stopping);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn read_host_status(
    host: &NativeProjectHost,
) -> Option<daedalus_project_host::NativeProjectHostStatus> {
    let deadline = Instant::now() + CONTROL_TIMEOUT;
    loop {
        match host.try_status() {
            Ok(NativeProjectHostStatusRead::Value(status)) => return Some(status),
            Ok(NativeProjectHostStatusRead::Busy) if Instant::now() < deadline => {
                thread::sleep(COORDINATOR_SLEEP);
            }
            Ok(NativeProjectHostStatusRead::Busy) | Err(_) => return None,
        }
    }
}

fn build_report(
    args: &ProjectViewArgs,
    execution: &ActivationExecution,
    status: Option<&NativeProjectHostStatus>,
    shutdown: Option<&daedalus_runtime::SessionShutdownReport>,
    shutdown_state: Result<&NativeProjectHostShutdown, &String>,
) -> Value {
    let queued_publication = status
        .and_then(NativeProjectHostStatus::last_queued_publication)
        .map(publication_json);
    let view = execution
        .view
        .as_ref()
        .and_then(|result| result.as_ref().ok());
    let view_error = execution
        .view
        .as_ref()
        .and_then(|result| result.as_ref().err());
    let last_presented = match execution.view.as_ref() {
        Some(Ok(view)) => view.window().last_presented().map(managed_presented_json),
        Some(Err(_)) => status
            .and_then(NativeProjectHostStatus::last_presented)
            .map(summary_json),
        None => None,
    };

    let window = view.as_ref().map(|view| {
        let report = view.report();
        json!({
            "exit_reason":format!("{:?}", report.exit_reason),
            "rendered_frames":report.rendered_frames,
            "loading_frames":report.loading_frames,
            "input_events":report.input_events,
            "loading_input_events":report.loading_input_events,
            "key_events":report.key_events,
            "loading_key_events":report.loading_key_events,
            "mouse_button_events":report.mouse_button_events,
            "loading_mouse_button_events":report.loading_mouse_button_events,
            "cursor_events":report.cursor_events,
            "loading_cursor_events":report.loading_cursor_events,
            "wheel_events":report.wheel_events,
            "loading_wheel_events":report.loading_wheel_events,
            "resize_events":report.resize_events,
            "loading_resize_events":report.loading_resize_events,
            "focus_events":report.focus_events,
            "loading_focus_events":report.loading_focus_events,
            "local_camera_changes":report.local_camera_changes,
            "resize_count":report.resize_count,
            "physical_width":report.final_width,
            "physical_height":report.final_height,
            "frame_intervals":{
                "sample_capacity":report.frame_intervals.sample_capacity,
                "samples_ms":report.frame_intervals.samples_ms,
                "p50_ms":report.frame_intervals.p50_ms,
                "p95_ms":report.frame_intervals.p95_ms,
                "p99_ms":report.frame_intervals.p99_ms,
                "max_ms":report.frame_intervals.max_ms,
            },
            "message_source_disconnected":report.message_source_disconnected,
            "final_loading":report.final_loading,
            "window_title":report.window_title,
            "final_camera":super::super::project_assets::camera_json(&report.final_camera),
            "final_render_view":super::super::project_assets::render_view_json(view.window().final_view()),
        })
    });

    let project = execution.project.as_ref();
    let coordinator = execution.input.as_ref().map(|input| {
        json!({
            "input_events":input.input_events,
            "arrow_actions_submitted":input.arrow_actions_submitted,
            "arrow_actions_failed":input.arrow_actions_failed,
            "manual_steps_requested":input.manual_steps_requested,
            "manual_steps_failed":input.manual_steps_failed,
            "protocol_requests":input.protocol_requests,
            "protocol_errors":input.protocol_errors,
            "duplicate_submissions":input.duplicate_submissions,
            "retained_protocol_tickets":input.retained_protocol_tickets,
            "protocol_detach_reason":input.protocol_detach_reason,
        })
    });
    let mut output = json!({
        "command":"project_view",
        "report_version":2,
        "presentation_mode":"managed_activation",
        "project_name":project.as_ref().map(|page| page.name.as_str()),
        "source_digest":status.map(NativeProjectHostStatus::active_source_digest),
        "source_schema_version":project.as_ref().map(|page| page.schema_version),
        "source_scene_revision":project.as_ref().map(|page| page.source_scene_revision.to_string()),
        "source_entity_count":project.as_ref().map(|page| page.entity_count),
        "session":shutdown.and_then(|report| report.session).map(|id| id.get().to_string())
            .or_else(|| project.as_ref().map(|page| page.session.get().to_string())),
        "authority_final_tick":shutdown.map(|report| report.final_tick.get().to_string()),
        "termination":shutdown.map(|report| format!("{:?}", report.termination)),
        "shutdown":shutdown.map(|report| json!({
            "queued_requests_stopped":report.queued_requests_stopped,
            "commands_stopped_not_applied":report.commands_stopped_not_applied,
            "terminal_unacknowledged":report.terminal_unacknowledged,
            "abandoned_tickets":report.abandoned_tickets,
            "result_credit_tickets":report.result_credit_tickets,
            "result_credit_bytes":report.result_credit_bytes,
        })),
        "host_shutdown":match shutdown_state {
            Ok(NativeProjectHostShutdown::Stopped) => "stopped",
            Ok(NativeProjectHostShutdown::Stopping) => "stopping",
            Err(_) => "error",
        },
        "queued_publication":queued_publication,
        "last_presented":last_presented,
        "host":status.map(super::super::project_view_control::native_project_host_status_json),
        "window":window,
        "resources":view.as_ref().map(|view| super::super::project_assets::packet_resource_stats_json(view.window().resource_stats())),
        "asset_resources":view.as_ref().map(|view| super::super::project_assets::asset_resource_stats_json(view.window().asset_resource_stats())),
        "coordinator":coordinator,
        "limits":{"width":args.width,"height":args.height,"seconds":args.seconds,"frames":args.frames},
        "live_capture":false,
        "mcp":false,
    });
    if let Some(error) = execution.failure.as_ref() {
        output["adapter_error"] = json!(error);
    }
    if let Some(error) = view_error {
        output["view_error"] = json!(error);
    }
    if let Err(error) = shutdown_state {
        output["shutdown_error"] = json!(error);
    }
    output
}

fn publication_json(identity: &ProjectPublicationIdentity) -> Value {
    super::super::project_view_control::project_publication_identity_json(identity)
}

fn managed_presented_json(presented: &daedalus_renderer::ManagedPresentedView) -> Value {
    json!({
        "session":presented.session().get().to_string(),
        "tick":presented.tick().get().to_string(),
        "sequence":presented.publication_sequence().to_string(),
        "authority_snapshot_sequence":presented.authority_snapshot_sequence().map(|value| value.to_string()),
        "project_generation":presented.project_generation().to_string(),
        "source_digest":presented.source_digest(),
        "present_sequence":presented.present_sequence().to_string(),
        "physical_width":presented.physical_width(),
        "physical_height":presented.physical_height(),
        "render_view":super::super::project_assets::render_view_json(presented.view()),
        "view_projection":presented.view_projection(),
        "actual_view_available":true,
    })
}

fn summary_json(summary: &daedalus_project_host::NativePresentedSummary) -> Value {
    super::super::project_view_control::native_presented_summary_json(summary)
}

#[cfg(test)]
mod tests {
    use daedalus_project_host::NativeProjectHostShutdown;
    use serde_json::json;

    use super::{ActivationExecution, build_report};
    use crate::project::ProjectClockArg;
    use crate::project_view::ProjectViewArgs;

    fn args() -> ProjectViewArgs {
        ProjectViewArgs {
            directory: std::path::PathBuf::from("unused"),
            clock: ProjectClockArg::Manual,
            audio: None,
            stdio_control: true,
            asset_preparation: true,
            asset_activation: true,
            controlled_entity: None,
            width: 320,
            height: 180,
            seconds: Some(3),
            frames: None,
            report: None,
        }
    }

    #[test]
    fn report_v2_keeps_queue_and_present_semantics_separate_without_fabricated_data() {
        let execution = ActivationExecution::failed(None, "startup query failed");
        let stopping = NativeProjectHostShutdown::Stopping;
        let report = build_report(&args(), &execution, None, None, Ok(&stopping));

        assert_eq!(report["report_version"], json!(2));
        assert_eq!(report["presentation_mode"], json!("managed_activation"));
        assert!(report.get("accepted_packet").is_none());
        assert!(report["queued_publication"].is_null());
        assert!(report["last_presented"].is_null());
        assert!(report["host"].is_null());
        assert!(report["authority_final_tick"].is_null());
        assert!(report["shutdown"].is_null());
        assert!(report["coordinator"].is_null());
        assert_eq!(report["host_shutdown"], json!("stopping"));
        assert_eq!(report["adapter_error"], json!("startup query failed"));
    }
}
