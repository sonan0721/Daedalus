//! Bounded, persistent JSON Lines control for one headless engine session.

use std::io::{self, BufRead, Write};

use daedalus_core::{CommandId, EntityHandle, Revision, Tick, Transform};
use daedalus_runtime::{Engine, EngineBuilder, EngineConfig};
use daedalus_world::{CommandSubmission, WorldCommand, WorldCommandKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONTROL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONTROL_LINE_BYTES: usize = 64 * 1024;
pub const MAX_CONTROL_STEPS: u32 = 120;
pub const MAX_CONTROL_ENTITIES: usize = 16_384;
pub const MAX_CONTROL_LIST_ENTITIES: usize = 1_024;
pub const MAX_CONTROL_RESPONSE_BYTES: usize = 1_048_576;
const MIN_CONTROL_RESPONSE_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug)]
pub struct ControlLimits {
    pub line_bytes: usize,
    pub steps_per_request: u32,
    pub entities_per_session: usize,
    pub entities_per_result: usize,
    pub response_bytes: usize,
}

impl Default for ControlLimits {
    fn default() -> Self {
        Self {
            line_bytes: MAX_CONTROL_LINE_BYTES,
            steps_per_request: MAX_CONTROL_STEPS,
            entities_per_session: MAX_CONTROL_ENTITIES,
            entities_per_result: MAX_CONTROL_LIST_ENTITIES,
            response_bytes: MAX_CONTROL_RESPONSE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    Text(String),
}

impl RequestId {
    fn is_valid(&self) -> bool {
        match self {
            Self::Number(_) => true,
            Self::Text(value) => !value.is_empty() && value.len() <= 128,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    protocol: u32,
    request_id: RequestId,
    request: Operation,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    SpawnTransform {
        transform: Transform,
    },
    GetTransform {
        entity: EntityHandle,
    },
    ListEntities {
        #[serde(default)]
        limit: Option<usize>,
    },
    SetTransform {
        target: EntityHandle,
        expected_revision: Revision,
        transform: Transform,
        #[serde(default)]
        command_id: Option<CommandId>,
        #[serde(default)]
        apply_at: Option<Tick>,
    },
    Step {
        #[serde(default = "one_step")]
        count: u32,
    },
    LatestSnapshot {
        #[serde(default)]
        limit: Option<usize>,
    },
    ProjectInfo {
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        limit: Option<usize>,
    },
    Exit,
}

fn one_step() -> u32 {
    1
}

#[derive(Serialize)]
struct Response {
    protocol: u32,
    request_id: Option<RequestId>,
    ok: bool,
    #[serde(skip)]
    state_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ResponseError>,
}

#[derive(Serialize)]
struct ResponseError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "is_false")]
    state_changed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

struct CommandIds(u128);

impl CommandIds {
    fn next(&mut self) -> Result<CommandId, ControlError> {
        let next = self
            .0
            .checked_add(1)
            .filter(|next| *next < (1_u128 << 127))
            .ok_or_else(|| {
                ControlError::new("command_id_exhausted", "command ID space exhausted")
            })?;
        let id = CommandId::from_raw(self.0 | (1_u128 << 127));
        self.0 = next;
        Ok(id)
    }
}

struct ControlError {
    code: &'static str,
    message: String,
    state_changed: bool,
}

impl ControlError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            state_changed: false,
        }
    }

    fn with_state_changed(mut self) -> Self {
        self.state_changed = true;
        self
    }
}

/// Serve JSON Lines requests until EOF or an `exit` operation. Every accepted request
/// gets one response, and worker shutdown runs after both normal and I/O-error exits.
pub fn serve_jsonl<R, W>(
    engine: &mut Engine,
    input: R,
    output: W,
    limits: ControlLimits,
) -> io::Result<usize>
where
    R: BufRead,
    W: Write,
{
    serve_jsonl_inner(engine, input, output, limits, None)
}

/// Serve the same JSON Lines operations as [`serve_jsonl`] with bounded project metadata.
/// The project value should contain an `entities` array of stable-ID/handle bindings and
/// source fields; individual `project_info` requests return only one bounded page.
pub fn serve_project_jsonl<R, W>(
    engine: &mut Engine,
    input: R,
    output: W,
    limits: ControlLimits,
    project_info: Value,
) -> io::Result<usize>
where
    R: BufRead,
    W: Write,
{
    serve_jsonl_inner(engine, input, output, limits, Some(project_info))
}

fn serve_jsonl_inner<R, W>(
    engine: &mut Engine,
    mut input: R,
    mut output: W,
    limits: ControlLimits,
    project_info: Option<Value>,
) -> io::Result<usize>
where
    R: BufRead,
    W: Write,
{
    if limits.line_bytes == 0
        || limits.steps_per_request == 0
        || limits.entities_per_session == 0
        || limits.entities_per_result == 0
        || limits.response_bytes < MIN_CONTROL_RESPONSE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JSONL control limits must be positive",
        ));
    }

    let loop_result = serve_loop_with_project_info(
        engine,
        &mut input,
        &mut output,
        limits,
        project_info.as_ref(),
    );
    let shutdown_result = engine
        .shutdown()
        .map_err(|error| io::Error::other(error.to_string()));
    match (loop_result, shutdown_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(requests), Ok(())) => Ok(requests),
    }
}

#[cfg(test)]
fn serve_loop<R, W>(
    engine: &mut Engine,
    input: &mut R,
    output: &mut W,
    limits: ControlLimits,
) -> io::Result<usize>
where
    R: BufRead,
    W: Write,
{
    serve_loop_with_project_info(engine, input, output, limits, None)
}

fn serve_loop_with_project_info<R: BufRead, W: Write>(
    engine: &mut Engine,
    input: &mut R,
    output: &mut W,
    limits: ControlLimits,
    project_info: Option<&Value>,
) -> io::Result<usize> {
    let mut request_count = 0;
    let mut command_ids = CommandIds(1);
    loop {
        let Some(line) = read_bounded_line(input, limits.line_bytes)? else {
            break;
        };
        request_count += 1;
        let (bytes, too_large) = line;
        if too_large {
            write_error(
                output,
                None,
                "request_too_large",
                format!("request line exceeds {} bytes", limits.line_bytes),
                false,
                limits.response_bytes,
            )?;
            continue;
        }
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(&bytes);
        let parsed_value = match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => value,
            Err(error) => {
                write_error(
                    output,
                    None,
                    "invalid_json",
                    error.to_string(),
                    false,
                    limits.response_bytes,
                )?;
                continue;
            }
        };
        let candidate_id = parsed_value
            .get("request_id")
            .cloned()
            .and_then(|value| serde_json::from_value::<RequestId>(value).ok())
            .filter(RequestId::is_valid);
        let envelope = match serde_json::from_value::<RequestEnvelope>(parsed_value) {
            Ok(envelope) if envelope.request_id.is_valid() => envelope,
            Ok(_) => {
                write_error(
                    output,
                    candidate_id,
                    "invalid_request_id",
                    "request_id must be a nonempty string of at most 128 bytes or an unsigned integer",
                    false,
                    limits.response_bytes,
                )?;
                continue;
            }
            Err(error) => {
                write_error(
                    output,
                    candidate_id,
                    "invalid_request",
                    error.to_string(),
                    false,
                    limits.response_bytes,
                )?;
                continue;
            }
        };
        if envelope.protocol != CONTROL_PROTOCOL_VERSION {
            write_error(
                output,
                Some(envelope.request_id),
                "unsupported_protocol",
                format!("supported protocol version is {CONTROL_PROTOCOL_VERSION}"),
                false,
                limits.response_bytes,
            )?;
            continue;
        }
        let request_id = envelope.request_id;
        let should_exit = matches!(&envelope.request, Operation::Exit);
        let (result, state_changed) = handle_request(
            engine,
            envelope.request,
            &mut command_ids,
            limits,
            project_info,
        );
        match result {
            Ok(value) => write_response(
                output,
                Response {
                    protocol: CONTROL_PROTOCOL_VERSION,
                    request_id: Some(request_id),
                    ok: true,
                    state_changed,
                    result: Some(value),
                    error: None,
                },
                limits.response_bytes,
            )?,
            Err(error) => write_error(
                output,
                Some(request_id),
                error.code,
                error.message,
                error.state_changed,
                limits.response_bytes,
            )?,
        }
        if should_exit {
            break;
        }
    }
    Ok(request_count)
}

fn read_bounded_line<R: BufRead>(
    input: &mut R,
    maximum_bytes: usize,
) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut line = Vec::new();
    let mut oversized = false;
    let mut saw_bytes = false;
    loop {
        let available = input.fill_buf()?;
        if available.is_empty() {
            return if saw_bytes {
                Ok(Some((line, oversized)))
            } else {
                Ok(None)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_bytes = newline.unwrap_or(available.len());
        if !oversized {
            if line.len().saturating_add(content_bytes) > maximum_bytes {
                oversized = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..content_bytes]);
            }
        }
        let consumed = content_bytes + usize::from(newline.is_some());
        input.consume(consumed);
        saw_bytes = true;
        if newline.is_some() {
            return Ok(Some((line, oversized)));
        }
    }
}

fn handle_request(
    engine: &mut Engine,
    operation: Operation,
    command_ids: &mut CommandIds,
    limits: ControlLimits,
    project_info: Option<&Value>,
) -> (Result<Value, ControlError>, bool) {
    match operation {
        Operation::SpawnTransform { transform } => {
            if engine.world().entity_count() >= limits.entities_per_session {
                return (
                    Err(ControlError::new(
                        "entity_limit",
                        format!(
                            "session entity limit ({}) reached",
                            limits.entities_per_session
                        ),
                    )),
                    false,
                );
            }
            let result = engine
                .spawn_transform(transform)
                .map(|entity| json!({"entity": entity}))
                .map_err(|error| ControlError::new("invalid_transform", error.to_string()));
            (result, false)
        }
        Operation::GetTransform { entity } => {
            let result = engine
                .world()
                .transform(entity)
                .map(|(transform, revision)| {
                    json!({"entity": entity, "transform": transform, "revision": revision})
                })
                .map_err(|error| ControlError::new("entity_read_failed", error.to_string()));
            (result, false)
        }
        Operation::ListEntities { limit } => {
            let maximum = requested_entity_limit(limit, limits);
            let total = engine.world().entity_count();
            let entities = engine.world_mut().render_entities_bounded(maximum);
            (
                Ok(json!({
                    "entities": entities,
                    "entity_count": total,
                    "truncated": total > maximum,
                    "limit": maximum,
                })),
                false,
            )
        }
        Operation::SetTransform {
            target,
            expected_revision,
            transform,
            command_id,
            apply_at,
        } => {
            if command_id.is_some_and(|command_id| command_id.get() >= (1_u128 << 127)) {
                return (
                    Err(ControlError::new(
                        "reserved_command_id",
                        "command IDs in the upper half of u128 are reserved for server-generated IDs",
                    )),
                    false,
                );
            }
            if !transform.is_finite() {
                return (
                    Err(ControlError::new(
                        "invalid_transform",
                        "transform contains a non-finite value",
                    )),
                    false,
                );
            }
            let current_tick = engine.clock().current_tick();
            let target_tick = match apply_at {
                Some(tick) => tick,
                None => match current_tick.0.checked_add(1).map(Tick) {
                    Some(tick) => tick,
                    None => {
                        return (
                            Err(ControlError::new(
                                "tick_exhausted",
                                "no future simulation tick exists",
                            )),
                            false,
                        );
                    }
                },
            };
            let command_id = match command_id {
                Some(command_id) => command_id,
                None => match command_ids.next() {
                    Ok(command_id) => command_id,
                    Err(error) => return (Err(error), false),
                },
            };
            let command = WorldCommand {
                command_id,
                target,
                expected_revision,
                apply_at: target_tick,
                kind: WorldCommandKind::SetTransform(transform),
            };
            let result = engine
                .try_command(command)
                .map(|submission| match submission {
                    CommandSubmission::Enqueued => json!({
                        "command_id": command_id,
                        "submission": "accepted",
                        "apply_at": target_tick,
                    }),
                    CommandSubmission::DuplicatePending => json!({
                        "command_id": command_id,
                        "submission": "duplicate_pending",
                    }),
                    CommandSubmission::DuplicateCompleted(command_result) => json!({
                        "command_id": command_id,
                        "submission": "already_completed",
                        "applied_result": command_result,
                    }),
                })
                .map_err(|error| ControlError::new("command_rejected", error.to_string()));
            (result, false)
        }
        Operation::Step { count } => {
            if count == 0 || count > limits.steps_per_request {
                return (
                    Err(ControlError::new(
                        "step_limit",
                        format!("step count must be in 1..={}", limits.steps_per_request),
                    )),
                    false,
                );
            }
            let mut reports = Vec::with_capacity(count as usize);
            for _ in 0..count {
                match engine.step_tick() {
                    Ok(report) => reports.push(report),
                    Err(error) => {
                        let mut failure = ControlError::new("step_failed", error.to_string());
                        if !reports.is_empty() {
                            failure = failure.with_state_changed();
                        }
                        return (Err(failure), !reports.is_empty());
                    }
                }
            }
            (Ok(json!({"reports": reports})), !reports.is_empty())
        }
        Operation::LatestSnapshot { limit } => {
            let maximum = requested_entity_limit(limit, limits);
            let result = engine
                .snapshot_consumer()
                .try_latest()
                .map_err(|error| ControlError::new("snapshot_busy", error.to_string()))
                .map(|snapshot| {
                    snapshot.map(|snapshot| {
                        json!({
                            "sequence": snapshot.sequence,
                            "tick": snapshot.tick,
                            "entity_count": snapshot.entities.len(),
                            "truncated": snapshot.entities.len() > maximum,
                            "entities": snapshot.entities.iter().take(maximum).collect::<Vec<_>>(),
                        })
                    })
                })
                .map(|snapshot| json!({"snapshot": snapshot}));
            (result, false)
        }
        Operation::ProjectInfo { offset, limit } => {
            let Some(project_info) = project_info else {
                return (
                    Err(ControlError::new(
                        "project_not_loaded",
                        "this JSONL engine session was started without an external project",
                    )),
                    false,
                );
            };
            let Some(all_entities) = project_info.get("entities").and_then(Value::as_array) else {
                return (
                    Err(ControlError::new(
                        "project_info_unavailable",
                        "the loaded project's stable entity map is unavailable",
                    )),
                    false,
                );
            };
            if limit == Some(0) {
                return (
                    Err(ControlError::new(
                        "project_info_limit",
                        "project_info limit must be positive",
                    )),
                    false,
                );
            }
            let page_limit = requested_entity_limit(limit, limits);
            let total = all_entities.len();
            let start = offset.min(total);
            let end = start.saturating_add(page_limit).min(total);
            let page = all_entities[start..end].to_vec();
            let next_offset = (end < total).then_some(end);
            if let Some(base) = project_info.as_object() {
                let mut object = base
                    .iter()
                    .filter(|(key, _)| key.as_str() != "entities")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<serde_json::Map<_, _>>();
                object.insert("session".into(), json!(engine.world().session()));
                object.insert("tick".into(), json!(engine.clock().current_tick()));
                object.insert("entities".into(), Value::Array(page));
                object.insert("entity_count".into(), json!(total));
                object.insert("offset".into(), json!(start));
                object.insert("limit".into(), json!(page_limit));
                object.insert("next_offset".into(), json!(next_offset));
                (Ok(Value::Object(object)), false)
            } else {
                (
                    Err(ControlError::new(
                        "project_info_unavailable",
                        "the loaded project's metadata is not a JSON object",
                    )),
                    false,
                )
            }
        }
        Operation::Exit => (Ok(json!({"closed": true})), true),
    }
}

fn requested_entity_limit(requested: Option<usize>, limits: ControlLimits) -> usize {
    requested
        .unwrap_or(limits.entities_per_result)
        .min(limits.entities_per_result)
}

fn write_error<W: Write>(
    output: &mut W,
    request_id: Option<RequestId>,
    code: &'static str,
    message: impl Into<String>,
    state_changed: bool,
    maximum_response_bytes: usize,
) -> io::Result<()> {
    write_response(
        output,
        Response {
            protocol: CONTROL_PROTOCOL_VERSION,
            request_id,
            ok: false,
            state_changed,
            result: None,
            error: Some(ResponseError {
                code,
                message: message.into().chars().take(512).collect(),
                state_changed,
            }),
        },
        maximum_response_bytes,
    )
}

fn write_response<W: Write>(
    output: &mut W,
    response: Response,
    maximum_response_bytes: usize,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() > maximum_response_bytes {
        let fallback = Response {
            protocol: CONTROL_PROTOCOL_VERSION,
            request_id: response.request_id,
            ok: false,
            state_changed: response.state_changed,
            result: None,
            error: Some(ResponseError {
                code: "response_too_large",
                message: "response exceeds configured byte limit".into(),
                state_changed: response.state_changed,
            }),
        };
        bytes = serde_json::to_vec(&fallback).map_err(io::Error::other)?;
    }
    if bytes.len() > maximum_response_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configured response limit is too small for a bounded error response",
        ));
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

pub fn create_control_engine() -> Result<Engine, daedalus_runtime::EngineBuildError> {
    EngineBuilder::new(EngineConfig::default()).build()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use daedalus_core::{SessionId, Transform};
    use serde_json::{Value, json};

    use super::{ControlLimits, MIN_CONTROL_RESPONSE_BYTES, serve_jsonl, serve_project_jsonl};
    use daedalus_runtime::{EngineBuilder, EngineConfig};

    fn engine() -> daedalus_runtime::Engine {
        EngineBuilder::new(EngineConfig {
            session: SessionId::from_raw(71),
            ..EngineConfig::default()
        })
        .build()
        .unwrap()
    }

    #[test]
    fn persistent_session_preserves_real_engine_state_and_applied_results() {
        let mut engine = engine();
        let spawn = json!({"protocol":1,"request_id":"a","request":{"op":"spawn_transform","transform":Transform::IDENTITY}});
        let mut spawn_output = Vec::new();
        super::serve_loop(
            &mut engine,
            &mut Cursor::new(format!("{spawn}\n")),
            &mut spawn_output,
            ControlLimits::default(),
        )
        .unwrap();
        let spawned: Value =
            serde_json::from_slice(spawn_output.split(|byte| *byte == b'\n').next().unwrap())
                .unwrap();
        let entity = spawned["result"]["entity"].clone();
        let input = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            json!({"protocol":1,"request_id":"b","request":{"op":"set_transform","target":entity,"expected_revision":0,"command_id":"9","transform":{"translation":[5,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}}}),
            json!({"protocol":1,"request_id":"c","request":{"op":"step","count":1}}),
            json!({"protocol":1,"request_id":"d","request":{"op":"get_transform","entity":entity}}),
            json!({"protocol":1,"request_id":"e","request":{"op":"latest_snapshot"}}),
            json!({"protocol":1,"request_id":"f","request":{"op":"exit"}}),
            json!({"protocol":1,"request_id":"ignored","request":{"op":"step"}}),
        );
        let mut output = Vec::new();
        let requests = super::serve_loop(
            &mut engine,
            &mut Cursor::new(input),
            &mut output,
            ControlLimits::default(),
        )
        .unwrap();
        assert_eq!(requests, 5);
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 5);
        assert_eq!(
            responses[1]["result"]["reports"][0]["commands"][0]["outcome"]["Applied"]["revision"],
            1
        );
        assert_eq!(responses[2]["result"]["transform"]["translation"][0], 5.0);
        assert_eq!(responses[2]["result"]["revision"], 1);
        assert_eq!(responses[3]["result"]["snapshot"]["tick"], 1);
        let handle: daedalus_core::EntityHandle = serde_json::from_value(entity).unwrap();
        assert_eq!(
            engine.world().transform(handle).unwrap().0,
            Transform {
                translation: [5.0, 0.0, 0.0],
                ..Transform::IDENTITY
            }
        );
        engine.shutdown().unwrap();
        assert!(matches!(
            engine
                .tasks()
                .try_submit(|_| Ok::<_, daedalus_runtime::TaskFailure>(())),
            Err(daedalus_runtime::SubmitError::Closed)
        ));
    }

    #[test]
    fn malformed_and_oversized_lines_are_isolated_and_eof_shuts_down_workers() {
        let mut engine = engine();
        let limits = ControlLimits {
            line_bytes: 128,
            response_bytes: 2_048,
            ..ControlLimits::default()
        };
        let oversized = "x".repeat(129);
        let input = format!(
            "not-json\n{oversized}\n{{\"protocol\":1,\"request_id\":4,\"request\":{{\"op\":\"step\",\"count\":1}}}}\n"
        );
        let mut output = Vec::new();
        assert_eq!(
            serve_jsonl(&mut engine, Cursor::new(input), &mut output, limits).unwrap(),
            3
        );
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses[0]["error"]["code"], "invalid_json");
        assert_eq!(responses[1]["error"]["code"], "request_too_large");
        assert_eq!(responses[2]["request_id"], json!(4));
        assert_eq!(engine.clock().current_tick().0, 1);
        assert!(matches!(
            engine
                .tasks()
                .try_submit(|_| Ok::<_, daedalus_runtime::TaskFailure>(())),
            Err(daedalus_runtime::SubmitError::Closed)
        ));
    }

    #[test]
    fn entity_results_and_steps_are_capped_before_rendering_responses() {
        let mut engine = engine();
        for _ in 0..4 {
            engine.spawn_transform(Transform::IDENTITY).unwrap();
        }
        let input = concat!(
            r#"{"protocol":1,"request_id":"list","request":{"op":"list_entities","limit":99}}"#,
            "\n",
            r#"{"protocol":1,"request_id":"step","request":{"op":"step","count":3}}"#,
            "\n",
        );
        let limits = ControlLimits {
            entities_per_result: 2,
            steps_per_request: 2,
            ..ControlLimits::default()
        };
        let mut output = Vec::new();
        serve_jsonl(&mut engine, Cursor::new(input), &mut output, limits).unwrap();
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            responses[0]["result"]["entities"].as_array().unwrap().len(),
            2
        );
        assert_eq!(responses[0]["result"]["entity_count"], 4);
        assert_eq!(responses[0]["result"]["truncated"], true);
        assert_eq!(responses[1]["error"]["code"], "step_limit");
        assert_eq!(engine.clock().current_tick().0, 0);
    }

    #[test]
    fn oversized_step_response_reports_that_state_changed() {
        let mut engine = engine();
        let input = format!(
            "{}\n",
            json!({"protocol":1,"request_id":"bounded","request":{"op":"step","count":120}})
        );
        let limits = ControlLimits {
            response_bytes: MIN_CONTROL_RESPONSE_BYTES,
            ..ControlLimits::default()
        };
        let mut output = Vec::new();
        serve_jsonl(&mut engine, Cursor::new(input), &mut output, limits).unwrap();
        let response: Value = serde_json::from_slice(output.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(response["error"]["code"], "response_too_large");
        assert_eq!(response["error"]["state_changed"], true);
        assert_eq!(engine.clock().current_tick().0, 120);
    }

    #[test]
    fn explicit_command_ids_cannot_alias_pending_or_completed_server_ids() {
        let mut engine = engine();
        let entity = engine.spawn_transform(Transform::IDENTITY).unwrap();
        let target = serde_json::to_value(entity).unwrap();
        let reserved_id = (1_u128 << 127) + 1;
        let request = |request_id: &str, command_id: Option<String>| {
            let mut operation = json!({
                "op":"set_transform",
                "target":target,
                "expected_revision":0,
                "transform":{"translation":[1,0,0],"rotation":[0,0,0,1],"scale":[1,1,1]}
            });
            if let Some(command_id) = command_id {
                operation["command_id"] = json!(command_id);
            }
            json!({"protocol":1,"request_id":request_id,"request":operation})
        };
        let input = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            request("pending", None),
            request("pending_alias", Some(reserved_id.to_string())),
            json!({"protocol":1,"request_id":"step","request":{"op":"step","count":1}}),
            request("completed_alias", Some(reserved_id.to_string())),
            json!({"protocol":1,"request_id":"exit","request":{"op":"exit"}}),
        );
        let mut output = Vec::new();
        serve_jsonl(
            &mut engine,
            Cursor::new(input),
            &mut output,
            ControlLimits::default(),
        )
        .unwrap();
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses[0]["result"]["submission"], "accepted");
        assert_eq!(
            responses[0]["result"]["command_id"],
            reserved_id.to_string()
        );
        assert_eq!(responses[1]["error"]["code"], "reserved_command_id");
        assert_eq!(
            responses[2]["result"]["reports"][0]["commands"][0]["command_id"],
            reserved_id.to_string()
        );
        assert_eq!(responses[3]["error"]["code"], "reserved_command_id");
        assert_eq!(engine.clock().current_tick().0, 1);
    }

    #[test]
    fn project_info_is_explicitly_unavailable_in_bare_serve_and_paginated_in_project_serve() {
        let bare_input = format!(
            "{}\n",
            json!({"protocol":1,"request_id":"bare","request":{"op":"project_info"}})
        );
        let mut bare_engine = engine();
        let mut bare_output = Vec::new();
        serve_jsonl(
            &mut bare_engine,
            Cursor::new(bare_input),
            &mut bare_output,
            ControlLimits::default(),
        )
        .unwrap();
        let bare: Value = serde_json::from_slice(bare_output.strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(bare["error"]["code"], "project_not_loaded");

        let project_info = json!({
            "project_name":"three entities",
            "source_scene_revision":4,
            "source_digest":"sha256:abc",
            "tick":0,
            "entities":[
                {"id":"entity/one","handle":{"session":"71","index":0,"generation":0}},
                {"id":"entity/two","handle":{"session":"71","index":1,"generation":0}},
                {"id":"entity/last-authored-entity","handle":{"session":"71","index":2,"generation":0}}
            ]
        });
        let project_input = format!(
            "{}\n{}\n",
            json!({"protocol":1,"request_id":"step","request":{"op":"step","count":1}}),
            json!({"protocol":1,"request_id":"last","request":{"op":"project_info","offset":2,"limit":99}}),
        );
        let limits = ControlLimits {
            entities_per_result: 2,
            ..ControlLimits::default()
        };
        let mut project_engine = engine();
        let mut project_output = Vec::new();
        serve_project_jsonl(
            &mut project_engine,
            Cursor::new(project_input),
            &mut project_output,
            limits,
            project_info,
        )
        .unwrap();
        let last: Value = serde_json::from_slice(
            project_output
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .nth(1)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(last["result"]["entity_count"], 3);
        assert_eq!(last["result"]["entities"].as_array().unwrap().len(), 1);
        assert_eq!(
            last["result"]["entities"][0]["id"],
            "entity/last-authored-entity"
        );
        assert_eq!(last["result"]["next_offset"], Value::Null);
        assert_eq!(last["result"]["tick"], 1);
        assert!(last["result"]["entities"][0].get("transform").is_none());
    }
}
