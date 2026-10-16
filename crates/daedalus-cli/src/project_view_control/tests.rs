use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use daedalus_content::project::{PreparedProject, ProjectLimits, prepare_project_from_bytes};
use daedalus_content::scene::StableId;
use daedalus_core::{Revision, SessionId, Tick};
use daedalus_renderer::{Camera, DirectionalLight, PacketWindowMessage, RenderPacket};
use daedalus_runtime::{
    EngineConfig, RequestTicket, SessionClockMode, SessionControlClient, SessionControlConfig,
    SessionControlService, SessionEntityTarget, SessionOperation, SessionRequestResult,
    SessionSubmitError, WaitStage,
};
use serde_json::{Value, json};

use super::*;

const ONE_ENTITY_SCENE: &str = r#"{
    "schema_version": 1,
    "revision": 37,
    "entities": [{
        "id": "entity:hero",
        "components": [{
            "id": "component:transform",
            "type_id": "daedalus.transform",
            "data": {
                "translation": [2.0, 3.0, 4.0],
                "rotation": [0.0, 0.0, 0.0, 1.0],
                "scale": [1.0, 1.0, 1.0]
            }
        }]
    }],
    "assets": []
}"#;

fn prepared_fixture() -> PreparedProject {
    let manifest = br#"{"schema_version":1,"name":"control fixture","scene":"scene.json"}"#;
    prepare_project_from_bytes(
        manifest,
        ONE_ENTITY_SCENE.as_bytes(),
        &BTreeMap::new(),
        ProjectLimits::default(),
    )
    .expect("prepare in-memory project")
}

struct Harness {
    controller: ProtocolController,
    service: Option<SessionControlService>,
    _window_receiver: Receiver<PacketWindowMessage>,
}

type HarnessParts = (
    Harness,
    SyncSender<Vec<u8>>,
    Receiver<Vec<u8>>,
    SyncSender<Vec<u8>>,
);

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            let _ = service.shutdown();
        }
    }
}

fn harness(clock: SessionClockMode, output_capacity: usize) -> HarnessParts {
    let service = SessionControlService::start(
        prepared_fixture(),
        SessionControlConfig {
            engine: EngineConfig::default(),
            clock_mode: clock,
            ..SessionControlConfig::default()
        },
    )
    .expect("start in-memory session service");
    let client = service.attach().expect("attach protocol client");
    let (request_sender, requests) = mpsc::sync_channel(MAX_PROTOCOL_QUEUE);
    let (response_sender, responses) = mpsc::sync_channel(output_capacity);
    let (window_sender, window_receiver) = mpsc::sync_channel(1);
    let transport = StdioTransport {
        requests,
        pending_request: None,
        output: response_sender.clone(),
        state: Arc::new(AtomicU8::new(TRANSPORT_RUNNING)),
        writer_failed: Arc::new(AtomicU8::new(0)),
    };
    let controller = ProtocolController::new(
        transport,
        client,
        window_sender,
        clock,
        SessionControlLimits::default(),
    );
    (
        Harness {
            controller,
            service: Some(service),
            _window_receiver: window_receiver,
        },
        request_sender,
        responses,
        response_sender,
    )
}

fn request(
    harness: &mut Harness,
    input: &SyncSender<Vec<u8>>,
    output: &Receiver<Vec<u8>>,
    request_id: &str,
    request: Value,
) -> Value {
    let bytes = serde_json::to_vec(&json!({
        "protocol": 2,
        "request_id": request_id,
        "request": request,
    }))
    .expect("serialize request");
    input.try_send(bytes).expect("bounded test input has room");
    harness.controller.pump();
    serde_json::from_slice(&output.try_recv().expect("one response per request"))
        .expect("response is JSON")
}

fn submit(
    harness: &mut Harness,
    input: &SyncSender<Vec<u8>>,
    output: &Receiver<Vec<u8>>,
    request_id: &str,
    operation: Value,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let response = request(
            harness,
            input,
            output,
            request_id,
            json!({"op":"submit", "operation":operation.clone()}),
        );
        if response["error"]["code"] == "busy" && Instant::now() < deadline {
            thread::yield_now();
            continue;
        }
        assert_eq!(response["ok"], true, "submit response: {response}");
        assert_eq!(response["result"]["kind"], "submitted");
        return response["result"]["data"]["ticket"]
            .as_str()
            .expect("ticket token")
            .to_owned();
    }
}

fn poll(
    harness: &mut Harness,
    input: &SyncSender<Vec<u8>>,
    output: &Receiver<Vec<u8>>,
    ticket: &str,
) -> Value {
    request(
        harness,
        input,
        output,
        "poll",
        json!({"op":"poll", "ticket":ticket}),
    )
}

fn poll_until_phase(
    harness: &mut Harness,
    input: &SyncSender<Vec<u8>>,
    output: &Receiver<Vec<u8>>,
    ticket: &str,
    desired: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let response = poll(harness, input, output, ticket);
        if response["error"]["code"] == "busy" && Instant::now() < deadline {
            thread::yield_now();
            continue;
        }
        assert_eq!(response["ok"], true, "poll response: {response}");
        let phase = response["result"]["data"]["phase"]
            .as_str()
            .expect("poll phase");
        if phase == desired {
            return response;
        }
        assert_ne!(
            phase, "terminal",
            "ticket terminated before {desired}: {response}"
        );
        assert!(Instant::now() < deadline, "timed out waiting for {desired}");
        thread::yield_now();
    }
}

fn poll_until_terminal(
    harness: &mut Harness,
    input: &SyncSender<Vec<u8>>,
    output: &Receiver<Vec<u8>>,
    ticket: &str,
) -> Value {
    poll_until_phase(harness, input, output, ticket, "terminal")
}

fn native_submit(client: &SessionControlClient, operation: SessionOperation) -> RequestTicket {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match client.try_submit(operation.clone()) {
            Ok(ticket) => return ticket,
            Err(SessionSubmitError::Busy) if Instant::now() < deadline => thread::yield_now(),
            Err(error) => panic!("native admission failed: {error:?}"),
        }
    }
}

fn native_terminal(ticket: &RequestTicket) -> Arc<RequestProgress> {
    ticket
        .wait_for(WaitStage::Terminal, Duration::from_secs(3))
        .expect("native ticket should terminate")
}

fn set_operation(command_id: &str, expected_revision: &str) -> Value {
    json!({
        "op":"set_transform",
        "target":{"stable_id":"entity:hero"},
        "command_id":command_id,
        "expected_revision":expected_revision,
        "apply_at":null,
        "transform":{
            "translation":[9.0, 5.0, 6.0],
            "rotation":[0.0, 0.0, 0.0, 1.0],
            "scale":[1.0, 1.0, 1.0]
        }
    })
}

#[test]
fn manual_protocol_flow_observes_tick_zero_authority_step_terminal_and_frame_one() {
    let (mut harness, input, output, _) = harness(SessionClockMode::Manual, 8);

    let initial_frame = request(
        &mut harness,
        &input,
        &output,
        "initial-frame",
        json!({"op":"latest_frame"}),
    );
    assert_eq!(initial_frame["ok"], true);
    assert_eq!(initial_frame["result"]["data"]["tick"], "0");

    let project_ticket = submit(
        &mut harness,
        &input,
        &output,
        "project-info",
        json!({"op":"project_info", "offset":0, "limit":1}),
    );
    let project = poll_until_terminal(&mut harness, &input, &output, &project_ticket);
    assert_eq!(
        project["result"]["data"]["terminal"]["result"]["data"]["source_scene_revision"],
        "37"
    );

    let get_ticket = submit(
        &mut harness,
        &input,
        &output,
        "initial-get",
        json!({
            "op":"get_transform",
            "target":{"stable_id":"entity:hero"}
        }),
    );
    let initial_get = poll_until_terminal(&mut harness, &input, &output, &get_ticket);
    assert_eq!(initial_get["result"]["data"]["terminal"]["ok"], true);
    assert_eq!(
        initial_get["result"]["data"]["terminal"]["result"]["data"]["observed_tick"],
        "0"
    );
    assert_eq!(
        initial_get["result"]["data"]["terminal"]["result"]["data"]["entity"]["revision"],
        "0"
    );

    let set_ticket = submit(
        &mut harness,
        &input,
        &output,
        "set-hero",
        set_operation("41", "0"),
    );
    let authority = poll_until_phase(
        &mut harness,
        &input,
        &output,
        &set_ticket,
        "authority_accepted",
    );
    assert_eq!(authority["result"]["data"]["authority"]["command_id"], "41");
    assert_eq!(authority["result"]["data"]["authority"]["apply_tick"], "1");

    let step_ticket = submit(
        &mut harness,
        &input,
        &output,
        "step-one",
        json!({"op":"step", "count":1}),
    );
    let step = poll_until_terminal(&mut harness, &input, &output, &step_ticket);
    assert_eq!(step["result"]["data"]["terminal"]["ok"], true);
    assert_eq!(
        step["result"]["data"]["terminal"]["result"]["data"][0]["tick"],
        "1"
    );

    let set = poll_until_terminal(&mut harness, &input, &output, &set_ticket);
    let operation_result = &set["result"]["data"]["terminal"]["result"];
    assert_eq!(operation_result["kind"], "command");
    let applied = &operation_result["data"];
    assert_eq!(applied["outcome"]["kind"], "applied");
    assert_eq!(applied["outcome"]["data"]["revision"], "1");
    assert_eq!(applied["resolve_tick"], "1");

    let get_ticket = submit(
        &mut harness,
        &input,
        &output,
        "get-after-step",
        json!({
            "op":"get_transform",
            "target":{"stable_id":"entity:hero"}
        }),
    );
    let get = poll_until_terminal(&mut harness, &input, &output, &get_ticket);
    let entity = &get["result"]["data"]["terminal"]["result"]["data"]["entity"];
    assert_eq!(
        get["result"]["data"]["terminal"]["result"]["data"]["observed_tick"],
        "1"
    );
    assert_eq!(entity["revision"], "1");
    assert_eq!(entity["transform"]["translation"][0], 9.0);

    let latest = request(
        &mut harness,
        &input,
        &output,
        "latest-after-step",
        json!({"op":"latest_frame"}),
    );
    assert_eq!(latest["result"]["data"]["tick"], "1");
}

#[test]
fn submit_replay_reuses_retained_ticket_and_rejects_payload_collision() {
    let (mut harness, input, output, _) = harness(SessionClockMode::Manual, 8);
    let operation = json!({"op":"project_info", "offset":0, "limit":1});
    let first = submit(
        &mut harness,
        &input,
        &output,
        "stable-request-id",
        operation.clone(),
    );
    let replay = request(
        &mut harness,
        &input,
        &output,
        "stable-request-id",
        json!({"op":"submit", "operation":operation}),
    );
    assert_eq!(replay["ok"], true);
    assert_eq!(replay["result"]["data"]["ticket"], first);
    assert_eq!(replay["result"]["data"]["request_id_reused"], true);

    let collision = request(
        &mut harness,
        &input,
        &output,
        "stable-request-id",
        json!({
            "op":"submit",
            "operation":{
                "op":"get_transform",
                "target":{"stable_id":"entity:hero"}
            }
        }),
    );
    assert_eq!(collision["ok"], false);
    assert_eq!(collision["error"]["code"], "request_id_collision");
}

#[test]
fn manual_step_has_reserved_capacity_and_ack_forget_release_protocol_slots() {
    let (mut harness, input, output, _) = harness(SessionClockMode::Manual, 8);
    let mut normal_tickets = Vec::new();
    for index in 0..MAX_PROTOCOL_NORMAL_TICKETS {
        normal_tickets.push(submit(
            &mut harness,
            &input,
            &output,
            &format!("page-{index}"),
            json!({"op":"project_info", "offset":0, "limit":1}),
        ));
    }
    for ticket in &normal_tickets {
        let terminal = poll_until_terminal(&mut harness, &input, &output, ticket);
        assert_eq!(terminal["result"]["data"]["terminal"]["ok"], true);
    }

    let step_ticket = submit(
        &mut harness,
        &input,
        &output,
        "reserved-step",
        json!({"op":"step", "count":1}),
    );
    let step = poll_until_terminal(&mut harness, &input, &output, &step_ticket);
    assert_eq!(step["result"]["data"]["terminal"]["ok"], true);
    assert_eq!(harness.controller.retained_tickets(), MAX_PROTOCOL_TICKETS);

    let full = request(
        &mut harness,
        &input,
        &output,
        "normal-at-capacity",
        json!({
            "op":"submit",
            "operation":{"op":"project_info", "offset":0, "limit":1}
        }),
    );
    assert_eq!(full["ok"], false);
    assert_eq!(full["error"]["code"], "result_capacity");

    let ack = request(
        &mut harness,
        &input,
        &output,
        "ack-one",
        json!({"op":"ack", "ticket":normal_tickets[0]}),
    );
    assert_eq!(ack["ok"], true);
    assert_eq!(ack["result"]["kind"], "acked");
    let after_ack = submit(
        &mut harness,
        &input,
        &output,
        "normal-after-ack",
        json!({"op":"project_info", "offset":0, "limit":1}),
    );

    let forget = request(
        &mut harness,
        &input,
        &output,
        "forget-one",
        json!({"op":"forget", "ticket":normal_tickets[1]}),
    );
    assert_eq!(forget["ok"], true);
    assert_eq!(forget["result"]["kind"], "forgotten");
    let after_forget = submit(
        &mut harness,
        &input,
        &output,
        "normal-after-forget",
        json!({"op":"project_info", "offset":0, "limit":1}),
    );
    assert_ne!(after_ack, after_forget);
    assert_eq!(harness.controller.retained_tickets(), MAX_PROTOCOL_TICKETS);
}

#[test]
fn strict_wire_parsing_rejects_wrong_session_numeric_wide_fields_and_unknown_fields() {
    let (mut manual, input, output, _) = harness(SessionClockMode::Manual, 8);

    let numeric_revision = request(
        &mut manual,
        &input,
        &output,
        "numeric-revision",
        json!({
            "op":"submit",
            "operation":{
                "op":"set_transform",
                "target":{"stable_id":"entity:hero"},
                "command_id":"55",
                "expected_revision":37,
                "apply_at":null,
                "transform":{"translation":[0,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}
            }
        }),
    );
    assert_eq!(numeric_revision["ok"], false);
    assert_eq!(numeric_revision["error"]["code"], "invalid_request");

    let unknown_field = request(
        &mut manual,
        &input,
        &output,
        "unknown-field",
        json!({"op":"capabilities", "surprise":true}),
    );
    assert_eq!(unknown_field["ok"], false);
    assert_eq!(unknown_field["error"]["code"], "invalid_request");

    let unknown_operation_field = request(
        &mut manual,
        &input,
        &output,
        "unknown-operation-field",
        json!({
            "op":"submit",
            "operation":{"op":"project_info", "offset":0, "limit":1, "surprise":true}
        }),
    );
    assert_eq!(unknown_operation_field["ok"], false);
    assert_eq!(unknown_operation_field["error"]["code"], "invalid_request");

    let wrong_session = submit(
        &mut manual,
        &input,
        &output,
        "wrong-session",
        json!({
            "op":"get_transform",
            "target":{"handle":{"session":"0", "index":"0", "generation":"0"}}
        }),
    );
    let terminal = poll_until_terminal(&mut manual, &input, &output, &wrong_session);
    assert_eq!(terminal["result"]["data"]["terminal"]["ok"], false);
    assert_eq!(
        terminal["result"]["data"]["terminal"]["error"]["code"],
        "wrong_session"
    );

    let (mut realtime, realtime_input, realtime_output, _) = harness(SessionClockMode::Realtime, 8);
    let manual_step = request(
        &mut realtime,
        &realtime_input,
        &realtime_output,
        "step-in-realtime",
        json!({"op":"submit", "operation":{"op":"step", "count":1}}),
    );
    assert_eq!(manual_step["ok"], false);
    assert_eq!(manual_step["error"]["code"], "clock_mode_mismatch");
    drop(manual);
}

#[test]
fn protocol_two_decimals_require_ascii_strings_and_targets_require_exactly_one_identity() {
    let parsed = serde_json::from_str::<DecimalU64>("\"18446744073709551615\"")
        .expect("full-width integer string should parse");
    assert_eq!(parsed.0, u64::MAX);
    assert!(serde_json::from_str::<DecimalU64>("18446744073709551615").is_err());
    assert!(serde_json::from_str::<DecimalU128>("\"+1\"").is_err());

    let stable = serde_json::from_value::<WireTarget>(json!({"stable_id":"entity:hero"}))
        .expect("stable-ID target parses");
    assert!(stable.native().is_ok());
    let handle = serde_json::from_value::<WireTarget>(json!({
        "handle":{"session":"0", "index":"0", "generation":"0"}
    }))
    .expect("handle target parses");
    assert!(handle.native().is_ok());

    let empty = WireTarget {
        stable_id: None,
        handle: None,
    };
    assert!(matches!(
        empty.native(),
        Err(ProtocolError { code, .. }) if code == "invalid_target"
    ));
    let ambiguous = WireTarget {
        stable_id: Some(StableId::new("entity:hero").unwrap()),
        handle: Some(WireHandle {
            session: DecimalU64(0),
            index: DecimalU32(0),
            generation: DecimalU32(0),
        }),
    };
    assert!(matches!(
        ambiguous.native(),
        Err(ProtocolError { code, .. }) if code == "invalid_target"
    ));
}

#[test]
fn response_encoder_caps_jsonl_bytes_and_keeps_the_newline_inside_the_limit() {
    let small = EnvelopeOut {
        protocol: 2,
        request_id: "small".to_owned(),
        ok: false,
        result: None,
        error: Some(WireError {
            code: "invalid_request".to_owned(),
            message: "bad request".to_owned(),
            retryable: None,
            details: None,
        }),
    };
    let encoded = encode_response("small", small).expect("small response fits");
    assert!(encoded.len() <= MAX_PROTOCOL_RESPONSE_BYTES);
    assert_eq!(encoded.last(), Some(&b'\n'));

    let oversized = EnvelopeOut {
        protocol: 2,
        request_id: "large".to_owned(),
        ok: false,
        result: None,
        error: Some(WireError {
            code: "invalid_request".to_owned(),
            message: "x".repeat(MAX_PROTOCOL_RESPONSE_BYTES),
            retryable: None,
            details: None,
        }),
    };
    assert!(encode_response("large", oversized).is_err());
}

#[test]
fn stale_revision_is_a_nested_terminal_command_rejection_in_both_ticket_results() {
    let (mut harness, input, output, _) = harness(SessionClockMode::Manual, 8);
    let set_ticket = submit(
        &mut harness,
        &input,
        &output,
        "stale-set",
        set_operation("63", "1"),
    );
    let authority = poll_until_phase(
        &mut harness,
        &input,
        &output,
        &set_ticket,
        "authority_accepted",
    );
    assert_eq!(
        authority["result"]["data"]["authority"]["submission"],
        "enqueued"
    );

    let step_ticket = submit(
        &mut harness,
        &input,
        &output,
        "step-stale-set",
        json!({"op":"step", "count":1}),
    );
    let step = poll_until_terminal(&mut harness, &input, &output, &step_ticket);
    let step_command = &step["result"]["data"]["terminal"]["result"]["data"][0]["commands"][0];
    assert_eq!(step_command["outcome"]["kind"], "rejected");
    assert_eq!(step_command["outcome"]["data"]["code"], "revision_conflict");
    assert_eq!(step_command["outcome"]["data"]["data"]["expected"], "1");
    assert_eq!(step_command["outcome"]["data"]["data"]["actual"], "0");

    let set = poll_until_terminal(&mut harness, &input, &output, &set_ticket);
    let operation_result = &set["result"]["data"]["terminal"]["result"];
    assert_eq!(operation_result["kind"], "command");
    let command = &operation_result["data"];
    assert_eq!(command["outcome"]["kind"], "rejected");
    assert_eq!(command["outcome"]["data"]["code"], "revision_conflict");
    assert_eq!(command["outcome"]["data"]["data"]["expected"], "1");
    assert_eq!(command["outcome"]["data"]["data"]["actual"], "0");
    assert_eq!(command["resolve_tick"], "1");
}

#[test]
fn jsonl_reader_enforces_line_limit_eof_and_bounded_input_queue() {
    let (request_sender, request_receiver) = mpsc::sync_channel(2);
    let state = Arc::new(AtomicU8::new(TRANSPORT_RUNNING));
    read_jsonl(
        Cursor::new(b"{\"protocol\":2}"),
        request_sender,
        Arc::clone(&state),
    );
    assert_eq!(state.load(Ordering::Acquire), TRANSPORT_EOF);
    assert_eq!(
        request_receiver
            .try_recv()
            .expect("partial EOF line queued"),
        b"{\"protocol\":2}"
    );

    let (request_sender, request_receiver) = mpsc::sync_channel(1);
    let state = Arc::new(AtomicU8::new(TRANSPORT_RUNNING));
    let too_long = vec![b'x'; MAX_PROTOCOL_LINE_BYTES + 1];
    read_jsonl(Cursor::new(too_long), request_sender, Arc::clone(&state));
    assert_eq!(state.load(Ordering::Acquire), TRANSPORT_OVERSIZE);
    assert!(matches!(
        request_receiver.try_recv(),
        Err(TryRecvError::Empty | TryRecvError::Disconnected)
    ));

    let (request_sender, _request_receiver) = mpsc::sync_channel(0);
    let state = Arc::new(AtomicU8::new(TRANSPORT_RUNNING));
    read_jsonl(Cursor::new(b"{}\n"), request_sender, Arc::clone(&state));
    assert_eq!(state.load(Ordering::Acquire), TRANSPORT_INPUT_FULL);
}

#[test]
fn detached_input_keeps_specific_eof_oversize_and_queue_full_reason() {
    for (state_value, expected) in [
        (TRANSPORT_EOF, "stdin_eof"),
        (TRANSPORT_OVERSIZE, "input_line_too_large"),
        (TRANSPORT_INPUT_FULL, "input_queue_full"),
    ] {
        let (mut harness, input, _output, _) = harness(SessionClockMode::Manual, 2);
        harness
            .controller
            .transport
            .state
            .store(state_value, Ordering::Release);
        drop(input);
        harness.controller.pump();
        assert_eq!(harness.controller.detach_reason(), Some(expected));
        assert_eq!(harness.controller.retained_tickets(), 0);
    }
}

#[derive(Clone, Copy)]
enum OutputFailure {
    BrokenPipe,
    Full,
}

fn assert_accepted_command_survives_output_detach(failure: OutputFailure) {
    let (mut harness, input, output, response_sender) = harness(
        SessionClockMode::Manual,
        if matches!(failure, OutputFailure::Full) {
            1
        } else {
            2
        },
    );
    let set_ticket = submit(
        &mut harness,
        &input,
        &output,
        "set-before-detach",
        set_operation("88", "0"),
    );
    poll_until_phase(
        &mut harness,
        &input,
        &output,
        &set_ticket,
        "authority_accepted",
    );

    let poll_request = serde_json::to_vec(&json!({
        "protocol":2,
        "request_id":"trigger-detach",
        "request":{"op":"poll", "ticket":set_ticket},
    }))
    .unwrap();
    match failure {
        OutputFailure::BrokenPipe => drop(output),
        OutputFailure::Full => response_sender
            .try_send(b"occupied".to_vec())
            .expect("fill bounded output queue"),
    }
    input.try_send(poll_request).expect("queue detach trigger");
    harness.controller.pump();
    assert_eq!(
        harness.controller.detach_reason(),
        Some(match failure {
            OutputFailure::BrokenPipe => "broken_stdout",
            OutputFailure::Full => "output_queue_full",
        })
    );
    assert_eq!(harness.controller.retained_tickets(), 0);

    let client = harness.service.as_ref().unwrap().attach().unwrap();
    let step = native_submit(&client, SessionOperation::Step { count: 1 });
    native_terminal(&step);
    let get = native_submit(
        &client,
        SessionOperation::GetTransform {
            target: SessionEntityTarget::StableId(
                StableId::new("entity:hero").expect("fixture ID"),
            ),
        },
    );
    let progress = native_terminal(&get);
    let Some(Ok(SessionRequestResult::Transform(transform))) = progress.terminal() else {
        panic!("accepted SetTransform should survive protocol output detach");
    };
    assert_eq!(transform.observed_tick, Tick(1));
    assert_eq!(transform.entity.revision, Revision(1));
    assert_eq!(transform.entity.transform.translation[0], 9.0);
}

#[test]
fn broken_pipe_releases_protocol_ticket_without_rolling_back_accepted_command() {
    assert_accepted_command_survives_output_detach(OutputFailure::BrokenPipe);
}

#[test]
fn full_output_queue_releases_protocol_ticket_without_rolling_back_accepted_command() {
    assert_accepted_command_survives_output_detach(OutputFailure::Full);
}

fn empty_packet() -> Arc<RenderPacket> {
    Arc::new(RenderPacket::new(
        SessionId::fresh(),
        Tick(0),
        0,
        0,
        "test-digest",
        Camera::default(),
        DirectionalLight::default(),
        [0.1, 0.1, 0.1],
        [0.0, 0.0, 0.0, 1.0],
        Vec::new(),
    ))
}

#[test]
fn close_survives_full_packet_queue_detach_and_dispatches_once_after_present_drains() {
    let (mut harness, input, output, _) = harness(SessionClockMode::Manual, 8);
    let present = PacketWindowMessage::Present {
        packet: empty_packet(),
        loading: false,
    };
    harness
        .controller
        .window
        .try_send(present)
        .expect("fill one-slot packet queue");

    let close = request(
        &mut harness,
        &input,
        &output,
        "close-window",
        json!({"op":"close"}),
    );
    assert_eq!(close["ok"], true);
    assert_eq!(close["result"]["kind"], "close_requested");
    assert!(harness.controller.closing());

    let repeated_close = request(
        &mut harness,
        &input,
        &output,
        "close-again",
        json!({"op":"close"}),
    );
    assert_eq!(repeated_close["ok"], true);
    assert_eq!(repeated_close["result"]["kind"], "close_requested");

    let rejected_submit = request(
        &mut harness,
        &input,
        &output,
        "submit-while-closing",
        json!({
            "op":"submit",
            "operation":{"op":"project_info", "offset":0, "limit":1}
        }),
    );
    assert_eq!(rejected_submit["ok"], false);
    assert_eq!(rejected_submit["error"]["code"], "window_closing");

    drop(input);
    harness
        .controller
        .transport
        .state
        .store(TRANSPORT_EOF, Ordering::Release);
    harness.controller.pump();
    assert_eq!(harness.controller.detach_reason(), Some("stdin_eof"));

    assert!(matches!(
        harness._window_receiver.try_recv(),
        Ok(PacketWindowMessage::Present { .. })
    ));
    harness.controller.pump();
    assert!(matches!(
        harness._window_receiver.try_recv(),
        Ok(PacketWindowMessage::Close)
    ));
    harness.controller.pump();
    assert!(matches!(
        harness._window_receiver.try_recv(),
        Err(TryRecvError::Empty)
    ));
}
