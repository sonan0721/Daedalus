//! Versioned, bounded C ABI for the .NET gameplay host.
//!
//! The ABI never treats a foreign opaque value as a pointer. Engine and output-buffer
//! handles are monotonically allocated registry tokens; stale or fabricated tokens fail
//! lookup. Returned byte pointers remain valid only until their matching buffer token is
//! released. The caller must pass readable input memory for the duration of the call and
//! must not read a returned pointer concurrently with releasing its token.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, OnceLock};

use daedalus_core::{CommandId, EntityHandle, Revision, Tick, Transform};
use daedalus_runtime::{Engine, EngineBuilder, EngineConfig};
use daedalus_world::{CommandSubmission, WorldCommand, WorldCommandKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ABI_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 1_048_576;
pub const MAX_RESPONSE_BYTES: usize = 8_388_608;
pub const MAX_OPERATIONS: usize = 256;
pub const MAX_ENTITIES: usize = 16_384;
pub const MAX_STEPS_PER_REQUEST: u32 = 120;
pub const MAX_ENGINE_INSTANCES: usize = 16;
pub const MAX_OUTSTANDING_BUFFERS: usize = 64;
pub const MAX_RETAINED_RESPONSE_BYTES: usize = 33_554_432;
const ENGINE_COMMAND_CAPACITY: usize = 256;

const STATUS_OK: i32 = 0;
const STATUS_INVALID_ARGUMENT: i32 = 1;
const STATUS_INVALID_HANDLE: i32 = 2;
const STATUS_INVALID_UTF8: i32 = 3;
const STATUS_TOO_LARGE: i32 = 4;
const STATUS_INVALID_JSON: i32 = 5;
const STATUS_OPERATION_ERROR: i32 = 6;
const STATUS_INTERNAL: i32 = 7;
const STATUS_RESOURCE_LIMIT: i32 = 8;
const STATUS_PANIC: i32 = 255;
const GENERATED_COMMAND_ID_BASE: u128 = 1_u128 << 127;

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

struct EngineSlot {
    engine: Engine,
    next_command_id: u128,
}

#[derive(Default)]
struct Registry {
    next_token: u64,
    engines: HashMap<u64, EngineSlot>,
    buffers: HashMap<u64, BufferAllocation>,
    retained_response_bytes: usize,
}

struct BufferAllocation {
    bytes: Box<[u8]>,
}

impl Registry {
    fn can_allocate_token(&self) -> bool {
        self.next_token == 0 || self.next_token.checked_add(1).is_some()
    }

    fn token(&mut self) -> Result<u64, BridgeError> {
        if self.next_token == 0 {
            self.next_token = 1;
        }
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .ok_or_else(|| BridgeError::new("token_exhausted", "bridge token space exhausted"))?;
        Ok(token)
    }
}

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

#[derive(Debug)]
struct BridgeError {
    code: &'static str,
    message: String,
}

impl BridgeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct ExchangeResponse {
    ok: bool,
    results: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ExchangeError>,
}

#[derive(Serialize)]
struct ExchangeError {
    code: &'static str,
    message: String,
    completed_operations: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangeRequest {
    operations: Vec<Operation>,
    #[serde(default)]
    atomic_commands: bool,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    SpawnTransform {
        transform: Transform,
    },
    GetTransform {
        entity: EntityHandle,
    },
    ListEntities,
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
    LatestSnapshot,
}

fn one_step() -> u32 {
    1
}

impl Operation {
    fn name(&self) -> &'static str {
        match self {
            Self::SpawnTransform { .. } => "spawn_transform",
            Self::GetTransform { .. } => "get_transform",
            Self::ListEntities => "list_entities",
            Self::SetTransform { .. } => "set_transform",
            Self::Step { .. } => "step",
            Self::LatestSnapshot => "latest_snapshot",
        }
    }
}

fn build_engine() -> Result<Engine, BridgeError> {
    let config = EngineConfig {
        command_capacity: ENGINE_COMMAND_CAPACITY,
        ..EngineConfig::default()
    };
    EngineBuilder::new(config)
        .build()
        .map_err(|error| BridgeError::new("engine_create_failed", error.to_string()))
}

fn next_command_id(slot: &mut EngineSlot) -> Result<CommandId, BridgeError> {
    let serial = slot.next_command_id;
    if serial == 0 || serial >= GENERATED_COMMAND_ID_BASE {
        return Err(BridgeError::new(
            "command_id_exhausted",
            "bridge-generated command IDs exhausted their reserved range",
        ));
    }
    slot.next_command_id = serial
        .checked_add(1)
        .ok_or_else(|| BridgeError::new("command_id_exhausted", "command ID space exhausted"))?;
    // Reserve the high half for bridge-generated IDs. Callers that supply their own IDs must
    // still follow the engine's normal de-duplication contract.
    Ok(CommandId::from_raw(GENERATED_COMMAND_ID_BASE | serial))
}

fn handle_operation(slot: &mut EngineSlot, operation: Operation) -> Result<Value, BridgeError> {
    match operation {
        Operation::SpawnTransform { transform } => {
            if slot.engine.world().entity_count() >= MAX_ENTITIES {
                return Err(BridgeError::new(
                    "entity_limit",
                    format!("engine entity limit ({MAX_ENTITIES}) reached"),
                ));
            }
            let entity = slot
                .engine
                .spawn_transform(transform)
                .map_err(|error| BridgeError::new("invalid_transform", error.to_string()))?;
            Ok(json!({ "entity": entity }))
        }
        Operation::GetTransform { entity } => {
            let (transform, revision) = slot
                .engine
                .world()
                .transform(entity)
                .map_err(|error| BridgeError::new("entity_read_failed", error.to_string()))?;
            Ok(json!({
                "entity": entity,
                "transform": transform,
                "revision": revision,
            }))
        }
        Operation::ListEntities => {
            let entities = slot.engine.world_mut().render_entities();
            if entities.len() > MAX_ENTITIES {
                return Err(BridgeError::new(
                    "entity_limit",
                    format!("entity list exceeds limit ({MAX_ENTITIES})"),
                ));
            }
            Ok(json!({ "entities": entities }))
        }
        Operation::SetTransform {
            target,
            expected_revision,
            transform,
            command_id,
            apply_at,
        } => {
            if !transform.is_finite() {
                return Err(BridgeError::new(
                    "invalid_transform",
                    "transform contains a non-finite value",
                ));
            }
            let current_tick = slot.engine.clock().current_tick();
            let apply_at = apply_at.unwrap_or_else(|| Tick(current_tick.0.saturating_add(1)));
            let command_id = match command_id {
                Some(command_id) if command_id.get() >= GENERATED_COMMAND_ID_BASE => {
                    return Err(BridgeError::new(
                        "command_id_reserved",
                        "command IDs with the high bit set are reserved for bridge-generated IDs",
                    ));
                }
                Some(command_id) => command_id,
                None => next_command_id(slot)?,
            };
            let command = WorldCommand {
                command_id,
                target,
                expected_revision,
                apply_at,
                kind: WorldCommandKind::SetTransform(transform),
            };
            let submission = slot
                .engine
                .try_command(command)
                .map_err(|error| BridgeError::new("command_rejected", error.to_string()))?;
            Ok(json!({
                "command_id": command_id,
                "submission": match submission {
                    CommandSubmission::Enqueued => "enqueued",
                    CommandSubmission::DuplicatePending => "duplicate_pending",
                    CommandSubmission::DuplicateCompleted(_) => "duplicate_completed",
                }
            }))
        }
        Operation::Step { count } => {
            if count > MAX_STEPS_PER_REQUEST {
                return Err(BridgeError::new(
                    "step_limit",
                    format!("step count exceeds per-request limit ({MAX_STEPS_PER_REQUEST})"),
                ));
            }
            let reports = slot
                .engine
                .run_ticks(count)
                .map_err(|error| BridgeError::new("step_failed", error.to_string()))?;
            Ok(json!({ "reports": reports }))
        }
        Operation::LatestSnapshot => {
            let snapshot = slot
                .engine
                .snapshot_consumer()
                .try_latest()
                .map_err(|error| BridgeError::new("snapshot_busy", error.to_string()))?;
            Ok(json!({ "snapshot": snapshot }))
        }
    }
}

fn handle_atomic_commands(
    slot: &mut EngineSlot,
    operations: Vec<Operation>,
) -> Result<Vec<Value>, BridgeError> {
    if operations.is_empty() {
        return Ok(Vec::new());
    }
    if slot.engine.world().pending_command_count() != 0 {
        return Err(BridgeError::new(
            "pending_commands",
            "atomic module batches require an empty command queue before commit",
        ));
    }
    let command_count = u128::try_from(operations.len()).unwrap_or(u128::MAX);
    if slot
        .next_command_id
        .checked_add(command_count)
        .is_none_or(|next| next >= (1_u128 << 127))
    {
        return Err(BridgeError::new(
            "command_id_exhausted",
            "bridge command ID range cannot cover the entire atomic batch",
        ));
    }
    let next_tick = slot
        .engine
        .clock()
        .current_tick()
        .0
        .checked_add(1)
        .map(Tick)
        .ok_or_else(|| BridgeError::new("tick_exhausted", "no future simulation tick exists"))?;
    let pending = slot.engine.world().pending_command_count();
    if pending.saturating_add(operations.len()) > ENGINE_COMMAND_CAPACITY {
        return Err(BridgeError::new(
            "command_queue_full",
            "atomic command batch does not fit in the bounded command queue",
        ));
    }

    let mut validated = Vec::with_capacity(operations.len());
    let mut targets = HashSet::with_capacity(operations.len());
    for operation in operations {
        let Operation::SetTransform {
            target,
            expected_revision,
            transform,
            command_id: None,
            apply_at: None,
        } = operation
        else {
            return Err(BridgeError::new(
                "invalid_atomic_batch",
                "atomic_commands accepts only set_transform operations without explicit IDs or ticks",
            ));
        };
        if !transform.is_finite() {
            return Err(BridgeError::new(
                "invalid_transform",
                "transform contains a non-finite value",
            ));
        }
        if !targets.insert(target) {
            return Err(BridgeError::new(
                "duplicate_batch_target",
                "an atomic transform batch may target each entity only once",
            ));
        }
        let (_, actual_revision) = slot
            .engine
            .world()
            .transform(target)
            .map_err(|error| BridgeError::new("entity_read_failed", error.to_string()))?;
        if actual_revision != expected_revision {
            return Err(BridgeError::new(
                "revision_conflict",
                format!(
                    "expected revision {}, found {}",
                    expected_revision.0, actual_revision.0
                ),
            ));
        }
        validated.push((target, expected_revision, transform));
    }

    // Preflight covers every failure exposed by Engine::try_command: future tick, available
    // capacity, valid target/revision data, and unique bridge IDs. Calls cannot interleave
    // because the registry mutex is held for the entire exchange.
    let mut accepted = Vec::with_capacity(validated.len());
    for (target, expected_revision, transform) in validated {
        let command_id = next_command_id(slot)?;
        let command = WorldCommand {
            command_id,
            target,
            expected_revision,
            apply_at: next_tick,
            kind: WorldCommandKind::SetTransform(transform),
        };
        match slot.engine.try_command(command) {
            Ok(CommandSubmission::Enqueued) => {
                accepted.push(json!({
                    "command_id": command_id,
                    "submission": "enqueued",
                    "apply_at": next_tick,
                }));
            }
            Ok(_) => {
                return Err(BridgeError::new(
                    "atomic_enqueue_invariant",
                    "bridge generated a duplicate command ID during atomic enqueue",
                ));
            }
            Err(error) => {
                return Err(BridgeError::new(
                    "atomic_enqueue_invariant",
                    format!("preflighted command enqueue failed: {error}"),
                ));
            }
        }
    }
    Ok(accepted)
}

fn exchange(registry: &mut Registry, handle: u64, request_bytes: &[u8]) -> (i32, Vec<u8>, bool) {
    if request_bytes.len() > MAX_REQUEST_BYTES {
        return (STATUS_TOO_LARGE, Vec::new(), false);
    }
    let request_text = match std::str::from_utf8(request_bytes) {
        Ok(text) => text,
        Err(_) => {
            return reserve_error(
                registry,
                STATUS_INVALID_UTF8,
                "invalid_utf8",
                "request is not valid UTF-8",
            );
        }
    };
    let request: ExchangeRequest = match serde_json::from_str(request_text) {
        Ok(request) => request,
        Err(error) => {
            return reserve_error(
                registry,
                STATUS_INVALID_JSON,
                "invalid_json",
                error.to_string(),
            );
        }
    };
    if request.operations.len() > MAX_OPERATIONS {
        return reserve_error(
            registry,
            STATUS_TOO_LARGE,
            "operation_limit",
            format!("operation count exceeds limit ({MAX_OPERATIONS})"),
        );
    }

    let budget = if let Some(slot) = registry.engines.get(&handle) {
        match response_budget(slot, &request) {
            Ok(budget) => budget,
            Err(error) => {
                return reserve_error(registry, STATUS_OPERATION_ERROR, error.code, error.message);
            }
        }
    } else {
        4_096
    };
    if !has_output_credit(registry, budget) {
        return (STATUS_RESOURCE_LIMIT, Vec::new(), false);
    }
    let Some(slot) = registry.engines.get_mut(&handle) else {
        return (
            STATUS_INVALID_HANDLE,
            error_bytes(
                "invalid_engine_handle",
                "engine handle is stale or unknown",
                0,
            ),
            true,
        );
    };

    let (results, failure) = if request.atomic_commands {
        match handle_atomic_commands(slot, request.operations) {
            Ok(results) => (results, None),
            Err(error) => (Vec::new(), Some((error, 0))),
        }
    } else {
        let mut results = Vec::with_capacity(request.operations.len());
        let mut failure = None;
        for (index, operation) in request.operations.into_iter().enumerate() {
            let operation_name = operation.name();
            match handle_operation(slot, operation) {
                Ok(result) => results.push(json!({ "op": operation_name, "result": result })),
                Err(error) => {
                    failure = Some((error, index));
                    break;
                }
            }
        }
        (results, failure)
    };

    let response = match failure {
        Some((error, completed_operations)) => ExchangeResponse {
            ok: false,
            results,
            error: Some(ExchangeError {
                code: error.code,
                message: error.message,
                completed_operations,
            }),
        },
        None => ExchangeResponse {
            ok: true,
            results,
            error: None,
        },
    };
    let status = if response.ok {
        STATUS_OK
    } else {
        STATUS_OPERATION_ERROR
    };
    match serde_json::to_vec(&response) {
        Ok(bytes) if bytes.len() <= budget && bytes.len() <= MAX_RESPONSE_BYTES => {
            (status, bytes, true)
        }
        Ok(_) => (
            STATUS_INTERNAL,
            error_bytes(
                "response_budget_invariant",
                "computed response exceeded its reserved output budget",
                response.results.len(),
            ),
            true,
        ),
        Err(error) => (
            STATUS_INTERNAL,
            error_bytes("response_serialize_failed", error.to_string(), 0),
            true,
        ),
    }
}

fn reserve_error(
    registry: &Registry,
    status: i32,
    code: &'static str,
    message: impl Into<String>,
) -> (i32, Vec<u8>, bool) {
    if !has_output_credit(registry, 4_096) {
        return (STATUS_RESOURCE_LIMIT, Vec::new(), false);
    }
    (status, error_bytes(code, message, 0), true)
}

fn has_output_credit(registry: &Registry, budget: usize) -> bool {
    registry.buffers.len() < MAX_OUTSTANDING_BUFFERS
        && registry
            .retained_response_bytes
            .checked_add(budget)
            .is_some_and(|total| total <= MAX_RETAINED_RESPONSE_BYTES)
        && registry.can_allocate_token()
}

fn response_budget(slot: &EngineSlot, request: &ExchangeRequest) -> Result<usize, BridgeError> {
    let large_operations = request
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                Operation::ListEntities | Operation::LatestSnapshot
            )
        })
        .count();
    if large_operations > 0 && request.operations.len() != 1 {
        return Err(BridgeError::new(
            "large_result_must_be_solo",
            "entity lists and snapshots must be requested in a single-operation exchange",
        ));
    }

    let spawn_count = request
        .operations
        .iter()
        .filter(|operation| matches!(operation, Operation::SpawnTransform { .. }))
        .count();
    let entity_count = slot
        .engine
        .world()
        .entity_count()
        .saturating_add(spawn_count);
    if entity_count > MAX_ENTITIES {
        return Err(BridgeError::new(
            "entity_limit",
            format!("exchange would exceed engine entity limit ({MAX_ENTITIES})"),
        ));
    }
    if large_operations > 0 {
        return Ok(entity_count
            .saturating_mul(384)
            .saturating_add(4_096)
            .min(MAX_RESPONSE_BYTES));
    }

    let total_steps = request
        .operations
        .iter()
        .try_fold(0_u32, |total, operation| {
            if let Operation::Step { count } = operation {
                total.checked_add(*count)
            } else {
                Some(total)
            }
        });
    let Some(total_steps) = total_steps else {
        return Err(BridgeError::new(
            "step_limit",
            "combined step count overflowed",
        ));
    };
    if total_steps > MAX_STEPS_PER_REQUEST {
        return Err(BridgeError::new(
            "step_limit",
            format!("combined step count exceeds per-request limit ({MAX_STEPS_PER_REQUEST})"),
        ));
    }

    let step_count = usize::try_from(total_steps).unwrap_or(usize::MAX);
    let possible_commands = slot
        .engine
        .world()
        .pending_command_count()
        .saturating_add(
            request
                .operations
                .iter()
                .filter(|operation| matches!(operation, Operation::SetTransform { .. }))
                .count(),
        )
        .min(ENGINE_COMMAND_CAPACITY);
    let report_budget = if step_count == 0 {
        0
    } else {
        possible_commands
            .saturating_mul(512)
            .saturating_add(step_count.saturating_mul(256))
    };
    Ok(4_096_usize
        .saturating_add(request.operations.len().saturating_mul(2_048))
        .saturating_add(report_budget)
        .min(MAX_RESPONSE_BYTES))
}

fn error_bytes(
    code: &'static str,
    message: impl Into<String>,
    completed_operations: usize,
) -> Vec<u8> {
    serde_json::to_vec(&ExchangeResponse {
        ok: false,
        results: Vec::new(),
        error: Some(ExchangeError {
            code,
            message: message.into(),
            completed_operations,
        }),
    })
    .unwrap_or_else(|_| br#"{"ok":false,"results":[],"error":{"code":"internal","message":"error serialization failed","completed_operations":0}}"#.to_vec())
}

fn ffi_guard(operation: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(status) => status,
        Err(_) => STATUS_PANIC,
    }
}

/// Return the bridge ABI version. This function does not access global state.
#[unsafe(no_mangle)]
pub extern "C" fn daedalus_bridge_abi_version() -> u32 {
    ABI_VERSION
}

/// Create a Rust-owned engine and write its opaque registry token to `out_handle`.
///
/// # Safety
/// `out_handle` must point to writable memory for one `u64` for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn daedalus_bridge_engine_create(out_handle: *mut u64) -> i32 {
    ffi_guard(|| {
        if out_handle.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the caller contract requires writable output storage; null was checked.
        unsafe { out_handle.write(0) };
        let mut registry = match registry().lock() {
            Ok(registry) => registry,
            Err(_) => return STATUS_INTERNAL,
        };
        if registry.engines.len() >= MAX_ENGINE_INSTANCES || !registry.can_allocate_token() {
            return STATUS_RESOURCE_LIMIT;
        }
        let engine = match build_engine() {
            Ok(engine) => engine,
            Err(_) => return STATUS_INTERNAL,
        };
        let token = match registry.token() {
            Ok(token) => token,
            Err(_) => return STATUS_INTERNAL,
        };
        registry.engines.insert(
            token,
            EngineSlot {
                engine,
                next_command_id: 1,
            },
        );
        // SAFETY: the caller contract requires writable output storage; null was checked.
        unsafe { out_handle.write(token) };
        STATUS_OK
    })
}

/// Destroy an engine. Repeated destruction and stale tokens return `STATUS_INVALID_HANDLE`.
#[unsafe(no_mangle)]
pub extern "C" fn daedalus_bridge_engine_destroy(handle: u64) -> i32 {
    ffi_guard(|| {
        let mut registry = match registry().lock() {
            Ok(registry) => registry,
            Err(_) => return STATUS_INTERNAL,
        };
        let Some(slot) = registry.engines.remove(&handle) else {
            return STATUS_INVALID_HANDLE;
        };
        match slot.engine.shutdown() {
            Ok(()) => STATUS_OK,
            Err(_) => STATUS_INTERNAL,
        }
    })
}

/// Execute one bounded UTF-8 JSON exchange against a Rust-owned engine.
///
/// A response allocation is returned for valid output slots even when the operation returns
/// a non-zero status. Copy `out_data[..out_len]` before calling `daedalus_bridge_buffer_release`
/// with `out_buffer`; do not retain the pointer after release.
///
/// # Safety
/// For `request_len > 0`, `request` must point to that many readable bytes for this call. Each
/// output pointer must be valid writable storage for its declared type. The pointer contents
/// returned in `out_data` are borrowed from the native allocation identified by `out_buffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn daedalus_bridge_engine_exchange_json(
    handle: u64,
    request: *const u8,
    request_len: usize,
    out_buffer: *mut u64,
    out_data: *mut *const u8,
    out_len: *mut usize,
) -> i32 {
    ffi_guard(|| {
        if out_buffer.is_null() || out_data.is_null() || out_len.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: all three output pointers were checked and caller invariants guarantee their
        // storage is writable for the duration of this call.
        unsafe {
            out_buffer.write(0);
            out_data.write(std::ptr::null());
            out_len.write(0);
        }
        if request_len > 0 && request.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        if request_len > MAX_REQUEST_BYTES {
            return STATUS_TOO_LARGE;
        }
        // SAFETY: the caller must provide request_len readable bytes; the length is bounded.
        let request_bytes = if request_len == 0 {
            &[]
        } else {
            // SAFETY: the caller must provide request_len readable bytes; the length is bounded.
            unsafe { std::slice::from_raw_parts(request, request_len) }
        };
        let mut registry = match registry().lock() {
            Ok(registry) => registry,
            Err(_) => return STATUS_INTERNAL,
        };
        let (status, response, allocate) = exchange(&mut registry, handle, request_bytes);
        if !allocate {
            return status;
        }
        let token = match registry.token() {
            Ok(token) => token,
            Err(_) => return STATUS_RESOURCE_LIMIT,
        };
        let allocation = response.into_boxed_slice();
        let data = allocation.as_ptr();
        let len = allocation.len();
        registry.retained_response_bytes = match registry.retained_response_bytes.checked_add(len) {
            Some(total) if total <= MAX_RETAINED_RESPONSE_BYTES => total,
            _ => return STATUS_INTERNAL,
        };
        registry
            .buffers
            .insert(token, BufferAllocation { bytes: allocation });
        // SAFETY: output slots were checked above and are writable by caller contract.
        unsafe {
            out_buffer.write(token);
            out_data.write(data);
            out_len.write(len);
        }
        status
    })
}

/// Release a response allocation by its opaque token.
#[unsafe(no_mangle)]
pub extern "C" fn daedalus_bridge_buffer_release(buffer: u64) -> i32 {
    ffi_guard(|| {
        let mut registry = match registry().lock() {
            Ok(registry) => registry,
            Err(_) => return STATUS_INTERNAL,
        };
        match registry.buffers.remove(&buffer) {
            Some(allocation) => {
                registry.retained_response_bytes = registry
                    .retained_response_bytes
                    .saturating_sub(allocation.bytes.len());
                STATUS_OK
            }
            None => STATUS_INVALID_HANDLE,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    static ABI_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn slot() -> EngineSlot {
        EngineSlot {
            engine: build_engine().unwrap(),
            next_command_id: 1,
        }
    }

    fn spawn(slot: &mut EngineSlot) -> EntityHandle {
        slot.engine
            .spawn_transform(Transform::IDENTITY)
            .expect("valid fixture transform")
    }

    #[test]
    fn atomic_batch_rejects_all_commands_when_one_revision_is_stale() {
        let mut slot = slot();
        let first = spawn(&mut slot);
        let second = spawn(&mut slot);
        let operations = vec![
            Operation::SetTransform {
                target: first,
                expected_revision: Revision(0),
                transform: Transform {
                    translation: [1.0, 0.0, 0.0],
                    ..Transform::IDENTITY
                },
                command_id: None,
                apply_at: None,
            },
            Operation::SetTransform {
                target: second,
                expected_revision: Revision(4),
                transform: Transform {
                    translation: [2.0, 0.0, 0.0],
                    ..Transform::IDENTITY
                },
                command_id: None,
                apply_at: None,
            },
        ];
        let error = handle_atomic_commands(&mut slot, operations).unwrap_err();
        assert_eq!(error.code, "revision_conflict");
        assert_eq!(slot.engine.world().pending_command_count(), 0);
        assert_eq!(
            slot.engine.world().transform(first).unwrap().0.translation[0],
            0.0
        );
    }

    #[test]
    fn atomic_batch_enqueues_into_the_actual_world_and_tick_applies_it() {
        let mut slot = slot();
        let entity = spawn(&mut slot);
        let results = handle_atomic_commands(
            &mut slot,
            vec![Operation::SetTransform {
                target: entity,
                expected_revision: Revision(0),
                transform: Transform {
                    translation: [3.5, 0.0, 0.0],
                    ..Transform::IDENTITY
                },
                command_id: None,
                apply_at: None,
            }],
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(slot.engine.world().pending_command_count(), 1);
        slot.engine.step_tick().unwrap();
        let (transform, revision) = slot.engine.world().transform(entity).unwrap();
        assert_eq!(transform.translation[0], 3.5);
        assert_eq!(revision, Revision(1));
    }

    fn create_engine() -> u64 {
        let mut handle = 0_u64;
        // SAFETY: handle is writable storage for one u64 for this call.
        assert_eq!(
            unsafe { daedalus_bridge_engine_create(&mut handle) },
            STATUS_OK
        );
        assert_ne!(handle, 0);
        handle
    }

    fn call_exchange(handle: u64, request: &[u8]) -> (i32, u64, Vec<u8>) {
        let mut token = 0_u64;
        let mut data = std::ptr::null();
        let mut length = 0_usize;
        // SAFETY: input bytes and output slots stay alive and valid through the call.
        let status = unsafe {
            daedalus_bridge_engine_exchange_json(
                handle,
                request.as_ptr(),
                request.len(),
                &mut token,
                &mut data,
                &mut length,
            )
        };
        if token == 0 {
            return (status, token, Vec::new());
        }
        assert!(!data.is_null());
        // SAFETY: the token owns this borrowed buffer until the matching release call.
        let response = unsafe { std::slice::from_raw_parts(data, length) }.to_vec();
        (status, token, response)
    }

    fn exchange_and_release(handle: u64, request: Value) -> (i32, Value) {
        let request = serde_json::to_vec(&request).unwrap();
        let (status, token, response) = call_exchange(handle, &request);
        assert_ne!(token, 0);
        assert_eq!(daedalus_bridge_buffer_release(token), STATUS_OK);
        (status, serde_json::from_slice(&response).unwrap())
    }

    #[test]
    fn actual_c_abi_round_trip_uses_rust_world_and_reports_errors() {
        let _guard = ABI_TEST_LOCK.lock().unwrap();
        let handle = create_engine();
        let (status, created) = exchange_and_release(
            handle,
            json!({
                "operations": [
                    {
                        "op": "spawn_transform",
                        "transform": Transform::IDENTITY
                    },
                    {
                        "op": "spawn_transform",
                        "transform": Transform::IDENTITY
                    }
                ]
            }),
        );
        assert_eq!(status, STATUS_OK);
        let entity: EntityHandle =
            serde_json::from_value(created["results"][0]["result"]["entity"].clone()).unwrap();
        let second: EntityHandle =
            serde_json::from_value(created["results"][1]["result"]["entity"].clone()).unwrap();

        let (status, _) = exchange_and_release(
            handle,
            json!({
                "operations": [{
                    "op": "set_transform",
                    "target": second,
                    "expected_revision": 0,
                    "transform": { "translation": [0.5, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0] }
                }]
            }),
        );
        assert_eq!(status, STATUS_OK);
        let (status, pending_error) = exchange_and_release(
            handle,
            json!({
                "atomic_commands": true,
                "operations": [{
                    "op": "set_transform",
                    "target": entity,
                    "expected_revision": 0,
                    "transform": Transform::IDENTITY
                }]
            }),
        );
        assert_eq!(status, STATUS_OPERATION_ERROR);
        assert_eq!(pending_error["error"]["code"], "pending_commands");
        let (status, _) = exchange_and_release(
            handle,
            json!({ "operations": [{ "op": "step", "count": 1 }] }),
        );
        assert_eq!(status, STATUS_OK);

        let stale_batch = json!({
            "atomic_commands": true,
            "operations": [
                {
                    "op": "set_transform",
                    "target": entity,
                    "expected_revision": 0,
                    "transform": { "translation": [1.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0] }
                },
                {
                    "op": "set_transform",
                    "target": second,
                    "expected_revision": 9,
                    "transform": { "translation": [2.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0] }
                }
            ]
        });
        let (status, rejected) = exchange_and_release(handle, stale_batch);
        assert_eq!(status, STATUS_OPERATION_ERROR);
        assert_eq!(rejected["error"]["code"], "revision_conflict");

        let (status, set) = exchange_and_release(
            handle,
            json!({
                "atomic_commands": true,
                "operations": [{
                    "op": "set_transform",
                    "target": entity,
                    "expected_revision": 0,
                    "transform": { "translation": [3.5, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0] }
                }]
            }),
        );
        assert_eq!(status, STATUS_OK);
        let command_id = set["results"][0]["command_id"].as_str().unwrap();
        let (status, step) = exchange_and_release(
            handle,
            json!({ "operations": [{ "op": "step", "count": 1 }] }),
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(
            step["results"][0]["result"]["reports"][0]["commands"][0]["command_id"],
            command_id
        );
        assert!(
            step["results"][0]["result"]["reports"][0]["commands"][0]["outcome"]
                .get("Applied")
                .is_some()
        );
        let (status, query) = exchange_and_release(
            handle,
            json!({ "operations": [{ "op": "get_transform", "entity": entity }] }),
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(
            query["results"][0]["result"]["transform"]["translation"][0],
            3.5
        );
        assert_eq!(query["results"][0]["result"]["revision"], 1);
        assert_eq!(daedalus_bridge_engine_destroy(handle), STATUS_OK);
        assert_eq!(
            daedalus_bridge_engine_destroy(handle),
            STATUS_INVALID_HANDLE
        );
    }

    #[test]
    fn actual_c_abi_rejects_reserved_caller_id_without_consuming_command_queue() {
        let _guard = ABI_TEST_LOCK.lock().unwrap();
        let handle = create_engine();
        let (status, created) = exchange_and_release(
            handle,
            json!({
                "operations": [{
                    "op": "spawn_transform",
                    "transform": Transform::IDENTITY
                }]
            }),
        );
        assert_eq!(status, STATUS_OK);
        let entity: EntityHandle =
            serde_json::from_value(created["results"][0]["result"]["entity"].clone()).unwrap();

        let (status, rejected) = exchange_and_release(
            handle,
            json!({
                "operations": [{
                    "op": "set_transform",
                    "target": entity,
                    "expected_revision": 0,
                    "command_id": ((1_u128 << 127) | 1).to_string(),
                    "transform": {
                        "translation": [9.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0]
                    }
                }]
            }),
        );
        assert_eq!(status, STATUS_OPERATION_ERROR);
        assert_eq!(rejected["error"]["code"], "command_id_reserved");
        assert_eq!(rejected["error"]["completed_operations"], 0);

        let (status, accepted) = exchange_and_release(
            handle,
            json!({
                "operations": [{
                    "op": "set_transform",
                    "target": entity,
                    "expected_revision": 0,
                    "transform": {
                        "translation": [2.0, 0.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0]
                    }
                }]
            }),
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(
            accepted["results"][0]["result"]["command_id"],
            ((1_u128 << 127) | 1).to_string()
        );

        let (status, stepped) = exchange_and_release(
            handle,
            json!({ "operations": [{ "op": "step", "count": 1 }] }),
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(
            stepped["results"][0]["result"]["reports"][0]["commands"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let (status, queried) = exchange_and_release(
            handle,
            json!({ "operations": [{ "op": "get_transform", "entity": entity }] }),
        );
        assert_eq!(status, STATUS_OK);
        assert_eq!(
            queried["results"][0]["result"]["transform"]["translation"][0],
            2.0
        );
        assert_eq!(queried["results"][0]["result"]["revision"], 1);
        assert_eq!(daedalus_bridge_engine_destroy(handle), STATUS_OK);
    }

    #[test]
    fn null_input_and_stale_tokens_return_errors_without_dereferencing_tokens() {
        let _guard = ABI_TEST_LOCK.lock().unwrap();
        let handle = create_engine();
        let mut token = u64::MAX;
        let mut data = std::ptr::null();
        let mut length = usize::MAX;
        // SAFETY: output pointers refer to valid local storage; null input with a nonzero
        // length must be rejected before the bridge reads it.
        let status = unsafe {
            daedalus_bridge_engine_exchange_json(
                handle,
                std::ptr::null(),
                1,
                &mut token,
                &mut data,
                &mut length,
            )
        };
        assert_eq!(status, STATUS_INVALID_ARGUMENT);
        assert_eq!(token, 0);
        assert!(data.is_null());
        assert_eq!(length, 0);
        assert_eq!(daedalus_bridge_engine_destroy(handle), STATUS_OK);
        let (status, token, _) = call_exchange(handle, br#"{"operations":[]}"#);
        assert_eq!(status, STATUS_INVALID_HANDLE);
        assert_ne!(token, 0);
        assert_eq!(daedalus_bridge_buffer_release(token), STATUS_OK);
        assert_eq!(daedalus_bridge_buffer_release(token), STATUS_INVALID_HANDLE);
        assert_eq!(
            daedalus_bridge_buffer_release(handle),
            STATUS_INVALID_HANDLE
        );
    }

    #[test]
    fn response_buffer_capacity_is_released_only_by_its_token() {
        let _guard = ABI_TEST_LOCK.lock().unwrap();
        let handle = create_engine();
        let request = br#"{"operations":[{"op":"latest_snapshot"}]}"#;
        let mut tokens = Vec::new();
        for _ in 0..MAX_OUTSTANDING_BUFFERS {
            let (status, token, response) = call_exchange(handle, request);
            assert_eq!(status, STATUS_OK);
            assert_ne!(token, 0);
            assert_eq!(
                serde_json::from_slice::<Value>(&response).unwrap()["ok"],
                Value::Bool(true)
            );
            tokens.push(token);
        }
        let (status, token, _) = call_exchange(handle, request);
        assert_eq!(status, STATUS_RESOURCE_LIMIT);
        assert_eq!(token, 0);
        assert_eq!(daedalus_bridge_buffer_release(tokens[0]), STATUS_OK);

        let (status, next_token, _) = call_exchange(handle, request);
        assert_eq!(status, STATUS_OK);
        assert_ne!(next_token, 0);
        assert_eq!(daedalus_bridge_buffer_release(next_token), STATUS_OK);
        for token in tokens.into_iter().skip(1) {
            assert_eq!(daedalus_bridge_buffer_release(token), STATUS_OK);
        }
        assert_eq!(daedalus_bridge_engine_destroy(handle), STATUS_OK);
    }

    #[test]
    fn retained_response_bytes_are_preflighted_and_freed_on_release() {
        let _guard = ABI_TEST_LOCK.lock().unwrap();
        let handle = create_engine();
        let prior_retained_bytes = {
            let mut registry = registry().lock().unwrap();
            assert!(registry.buffers.is_empty());
            let prior = registry.retained_response_bytes;
            registry.retained_response_bytes = MAX_RETAINED_RESPONSE_BYTES - 8_192;
            prior
        };
        let request = serde_json::to_vec(&json!({
            "operations": [
                { "op": "spawn_transform", "transform": Transform::IDENTITY },
                { "op": "spawn_transform", "transform": Transform::IDENTITY }
            ]
        }))
        .unwrap();

        let (status, token, response) = call_exchange(handle, &request);
        assert_eq!(status, STATUS_OK);
        assert_ne!(token, 0);
        let actual_bytes = response.len();
        assert!(actual_bytes > 0);
        assert_eq!(
            registry().lock().unwrap().retained_response_bytes,
            MAX_RETAINED_RESPONSE_BYTES - 8_192 + actual_bytes
        );

        let (status, rejected_token, _) = call_exchange(handle, &request);
        assert_eq!(status, STATUS_RESOURCE_LIMIT);
        assert_eq!(rejected_token, 0);
        assert_eq!(daedalus_bridge_buffer_release(token), STATUS_OK);
        assert_eq!(
            registry().lock().unwrap().retained_response_bytes,
            MAX_RETAINED_RESPONSE_BYTES - 8_192
        );

        let (status, next_token, _) = call_exchange(handle, &request);
        assert_eq!(status, STATUS_OK);
        assert_ne!(next_token, 0);
        assert_eq!(daedalus_bridge_buffer_release(next_token), STATUS_OK);
        assert_eq!(daedalus_bridge_engine_destroy(handle), STATUS_OK);
        let mut registry = registry().lock().unwrap();
        assert!(registry.buffers.is_empty());
        registry.retained_response_bytes = prior_retained_bytes;
    }
}
