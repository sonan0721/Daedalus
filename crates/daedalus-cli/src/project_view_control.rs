//! Renderer-neutral bounded protocol-2 adapter for one live project session.
//!
//! The transport workers own raw bytes and process I/O only. The session owner
//! controls Close handling; this adapter owns the client and retained tickets.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;

use daedalus_content::asset_replacement::AssetReplacementMismatch;
use daedalus_content::assets::{AssetImportMemorySummary, AssetImportMetrics};
use daedalus_content::scene::StableId;
use daedalus_core::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};
#[cfg(feature = "window")]
use daedalus_project_host::{
    NativePresentedSummary, NativeProjectHostControl, NativeProjectHostLifecycle,
    NativeProjectHostStatus, NativeProjectHostStatusRead,
};
use daedalus_runtime::{
    ActivationId, AssetActivationCommandOutcome, AssetActivationOperation,
    AssetActivationRequestResult, AssetActivationState, AssetActivationStatus,
    AssetPreparationCommandOutcome, AssetPreparationCommandResult, AssetPreparationOperation,
    AssetPreparationResult, AssetPreparationStatus, AuthorityAdmission, FrameReadError,
    PreparationCandidateSummary, PreparationFailure, PreparationId, PreparationState,
    ProjectInfoPage, RequestAckError, RequestPhase, RequestProgress, RequestReadError,
    RequestTicket, SessionClockMode, SessionControlClient, SessionControlLimits, SessionOperation,
    SessionRequestError, SessionRequestResult, SessionStatus, SessionSubmitError,
};
#[cfg(feature = "audio")]
use daedalus_runtime::{
    OfflineAudioSummary, SessionAudioBackend, SessionAudioCommandOutcome, SessionAudioOperation,
    SessionAudioResult, SessionAudioVoiceId, SessionAudioVoiceState,
};
use serde::{Deserialize, Serialize};
#[cfg(feature = "window")]
use serde_json::{Value, json};

pub(crate) const MAX_PROTOCOL_LINE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PROTOCOL_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_PROTOCOL_TICKETS: usize = 8;
const MAX_PROTOCOL_NORMAL_TICKETS: usize = 7;
const MAX_PROTOCOL_QUEUE: usize = 16;
const MAX_PROTOCOL_OUTPUT_QUEUE: usize = 4;
const PROTOCOL: u32 = 2;
const BASE_OPERATIONS: [&str; 7] = [
    "project_info",
    "get_transform",
    "list_transforms",
    "set_transform",
    "spawn_transform",
    "step",
    "status",
];
const AUDIO_EXTENSION: [&str; 5] = [
    "audio_status",
    "audio_clips",
    "audio_voice",
    "audio_voices",
    "audio_command",
];
const PREPARATION_EXTENSION: [&str; 3] = [
    "asset_preparation_status",
    "asset_preparation_prepare",
    "asset_preparation_cancel",
];
const ACTIVATION_EXTENSION: [&str; 4] = [
    "asset_activation_status",
    "asset_activation_stage",
    "asset_activation_commit",
    "asset_activation_cancel",
];

fn operation_names(audio: bool, preparation: bool, activation: bool) -> Vec<&'static str> {
    let mut names = Vec::with_capacity(
        BASE_OPERATIONS.len()
            + if audio { AUDIO_EXTENSION.len() } else { 0 }
            + if preparation {
                PREPARATION_EXTENSION.len()
            } else {
                0
            }
            + if activation {
                ACTIVATION_EXTENSION.len()
            } else {
                0
            },
    );
    names.extend(BASE_OPERATIONS);
    if audio {
        names.extend(AUDIO_EXTENSION);
    }
    if preparation {
        names.extend(PREPARATION_EXTENSION);
    }
    if activation {
        names.extend(ACTIVATION_EXTENSION);
    }
    names
}

const TRANSPORT_RUNNING: u8 = 0;
const TRANSPORT_EOF: u8 = 1;
const TRANSPORT_OVERSIZE: u8 = 2;
const TRANSPORT_INPUT_FULL: u8 = 3;
const TRANSPORT_INPUT_ERROR: u8 = 4;

/// Raw process transport. Neither worker captures the service, renderer, frame, or ticket book.
pub(crate) struct StdioTransport {
    requests: Receiver<Vec<u8>>,
    pending_request: Option<Vec<u8>>,
    output: SyncSender<Vec<u8>>,
    state: Arc<AtomicU8>,
    writer_failed: Arc<AtomicU8>,
}

impl StdioTransport {
    pub(crate) fn start() -> io::Result<Self> {
        let (request_tx, requests) = mpsc::sync_channel(MAX_PROTOCOL_QUEUE);
        let (output, output_rx) = mpsc::sync_channel(MAX_PROTOCOL_OUTPUT_QUEUE);
        let state = Arc::new(AtomicU8::new(TRANSPORT_RUNNING));
        let reader_state = Arc::clone(&state);
        let writer_failed = Arc::new(AtomicU8::new(0));
        let writer_status = Arc::clone(&writer_failed);
        thread::Builder::new()
            .name("daedalus-project-view-stdout".into())
            .spawn(move || write_responses(std::io::stdout(), output_rx, writer_status))?;

        // Spawn the potentially blocking stdin worker last. If its spawn fails, dropping the
        // only output sender disconnects the already-started writer so it can exit on its own.
        thread::Builder::new()
            .name("daedalus-project-view-stdin".into())
            .spawn(move || read_jsonl(std::io::stdin(), request_tx, reader_state))?;

        Ok(Self {
            requests,
            pending_request: None,
            output,
            state,
            writer_failed,
        })
    }

    pub(crate) fn try_request(&mut self) -> Result<Option<Vec<u8>>, TransportRead> {
        if let Some(request) = self.pending_request.take() {
            return Ok(Some(request));
        }
        match self.requests.try_recv() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(TransportRead::Detached),
        }
    }

    fn has_pending_request(&mut self) -> bool {
        if self.pending_request.is_some() {
            return true;
        }
        match self.requests.try_recv() {
            Ok(request) => {
                self.pending_request = Some(request);
                true
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => false,
        }
    }

    pub(crate) fn send(&self, response: Vec<u8>) -> Result<(), TransportWrite> {
        if self.writer_failed.load(Ordering::Acquire) != 0 {
            return Err(TransportWrite::BrokenPipe);
        }
        match self.output.try_send(response) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TransportWrite::Full),
            Err(TrySendError::Disconnected(_)) => Err(TransportWrite::BrokenPipe),
        }
    }

    pub(crate) fn terminal_reason(&self) -> Option<&'static str> {
        if self.writer_failed.load(Ordering::Acquire) != 0 {
            return Some("broken_stdout");
        }
        match self.state.load(Ordering::Acquire) {
            TRANSPORT_RUNNING => None,
            TRANSPORT_EOF => Some("stdin_eof"),
            TRANSPORT_OVERSIZE => Some("input_line_too_large"),
            TRANSPORT_INPUT_FULL => Some("input_queue_full"),
            TRANSPORT_INPUT_ERROR => Some("stdin_error"),
            _ => Some("transport_error"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportRead {
    Detached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransportWrite {
    Full,
    BrokenPipe,
}

fn read_jsonl<R: io::Read>(reader: R, requests: SyncSender<Vec<u8>>, state: Arc<AtomicU8>) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::with_capacity(4096);
    loop {
        let available = match reader.fill_buf() {
            Ok([]) => {
                if !line.is_empty() {
                    match requests.try_send(std::mem::take(&mut line)) {
                        Ok(()) => state.store(TRANSPORT_EOF, Ordering::Release),
                        Err(_) => state.store(TRANSPORT_INPUT_FULL, Ordering::Release),
                    }
                } else {
                    state.store(TRANSPORT_EOF, Ordering::Release);
                }
                return;
            }
            Ok(available) => available,
            Err(_) => {
                state.store(TRANSPORT_INPUT_ERROR, Ordering::Release);
                return;
            }
        };
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let line_bytes = newline.unwrap_or(consumed);
        if line.len().saturating_add(line_bytes) > MAX_PROTOCOL_LINE_BYTES {
            state.store(TRANSPORT_OVERSIZE, Ordering::Release);
            return;
        }
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            match requests.try_send(std::mem::take(&mut line)) {
                Ok(()) => {}
                Err(_) => {
                    state.store(TRANSPORT_INPUT_FULL, Ordering::Release);
                    return;
                }
            }
        }
    }
}

fn write_responses<W: Write>(mut writer: W, responses: Receiver<Vec<u8>>, status: Arc<AtomicU8>) {
    while let Ok(bytes) = responses.recv() {
        if writer
            .write_all(&bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            status.store(1, Ordering::Release);
            return;
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    protocol: u32,
    request_id: String,
    request: ClientRequest,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ClientRequest {
    Capabilities {},
    ViewStatus {},
    Submit { operation: WireOperation },
    Poll { ticket: String },
    Ack { ticket: String },
    Forget { ticket: String },
    LatestFrame {},
    Close {},
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireOperation {
    ProjectInfo {
        #[serde(default)]
        offset: usize,
        limit: Option<usize>,
    },
    GetTransform {
        target: WireTarget,
    },
    ListTransforms {
        #[serde(default)]
        offset: usize,
        limit: Option<usize>,
    },
    SetTransform {
        target: WireTarget,
        #[serde(default)]
        command_id: Option<DecimalU128>,
        expected_revision: DecimalU64,
        apply_at: Option<DecimalU64>,
        transform: Transform,
    },
    SpawnTransform {
        transform: Transform,
    },
    Step {
        count: u32,
    },
    Status,
    AssetPreparationStatus,
    AssetPreparationPrepare {
        #[serde(default)]
        command_id: Option<DecimalU128>,
    },
    AssetPreparationCancel {
        #[serde(default)]
        command_id: Option<DecimalU128>,
        candidate: WirePreparationId,
    },
    AssetActivationStatus,
    AssetActivationStage {
        #[serde(default)]
        command_id: Option<DecimalU128>,
        candidate: WirePreparationId,
        expected_active_generation: DecimalU64,
    },
    AssetActivationCommit {
        #[serde(default)]
        command_id: Option<DecimalU128>,
        activation: WireActivationId,
        expected_active_generation: DecimalU64,
    },
    AssetActivationCancel {
        #[serde(default)]
        command_id: Option<DecimalU128>,
        activation: WireActivationId,
    },
    AudioStatus,
    AudioClips {
        #[serde(default)]
        offset: usize,
        limit: Option<usize>,
    },
    AudioVoice {
        voice: WireVoiceId,
    },
    AudioVoices {
        #[serde(default)]
        offset: usize,
        limit: Option<usize>,
    },
    AudioCommand {
        #[serde(default)]
        command_id: Option<DecimalU128>,
        mutation: WireAudioMutation,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct WireTarget {
    stable_id: Option<StableId>,
    handle: Option<WireHandle>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct WireHandle {
    session: DecimalU64,
    index: DecimalU32,
    generation: DecimalU32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireVoiceId {
    session: DecimalU64,
    sequence: DecimalU64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct WirePreparationId {
    session: DecimalU64,
    epoch: DecimalU64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WireActivationId {
    session: DecimalU64,
    sequence: DecimalU64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireAudioMutation {
    Play {
        clip: StableId,
        volume: f32,
        pan: f32,
        looping: bool,
    },
    Stop {
        voice: WireVoiceId,
    },
    SetVolume {
        voice: WireVoiceId,
        volume: f32,
    },
    SetPan {
        voice: WireVoiceId,
        pan: f32,
    },
    RenderOffline {
        frames: usize,
    },
}

impl WireOperation {
    fn native(
        &self,
        audio_enabled: bool,
        asset_preparation_enabled: bool,
        asset_activation_enabled: bool,
    ) -> Result<SessionOperation, ProtocolError> {
        let operation = match self {
            Self::ProjectInfo { offset, limit } => {
                if *offset > 16_384 || limit.is_some_and(|value| !(1..=1024).contains(&value)) {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "project_info page is outside its bounds",
                    ));
                }
                SessionOperation::ProjectInfo {
                    offset: *offset,
                    limit: *limit,
                }
            }
            Self::GetTransform { target } => SessionOperation::GetTransform {
                target: target.native()?,
            },
            Self::ListTransforms { offset, limit } => {
                if *offset > 16_384 || limit.is_some_and(|value| !(1..=1024).contains(&value)) {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "list_transforms page is outside its bounds",
                    ));
                }
                SessionOperation::ListTransforms {
                    offset: *offset,
                    limit: *limit,
                }
            }
            Self::SetTransform {
                target,
                command_id,
                expected_revision,
                apply_at,
                transform,
            } => {
                if !transform.is_finite() {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "transform values must be finite",
                    ));
                }
                SessionOperation::SetTransform {
                    target: target.native()?,
                    command_id: command_id.map(|value| CommandId::from_raw(value.0)),
                    expected_revision: Revision(expected_revision.0),
                    apply_at: apply_at.map(|value| Tick(value.0)),
                    transform: *transform,
                }
            }
            Self::SpawnTransform { transform } => {
                if !transform.is_finite() {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "transform values must be finite",
                    ));
                }
                SessionOperation::SpawnTransform {
                    transform: *transform,
                }
            }
            Self::Step { count } if (1..=120).contains(count) => {
                SessionOperation::Step { count: *count }
            }
            Self::Step { .. } => {
                return Err(ProtocolError::new(
                    "invalid_request",
                    "step count must be in 1..=120",
                ));
            }
            Self::Status => SessionOperation::Status,
            Self::AssetPreparationStatus if asset_preparation_enabled => {
                SessionOperation::AssetPreparation(AssetPreparationOperation::Status)
            }
            Self::AssetPreparationPrepare { command_id } if asset_preparation_enabled => {
                SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                })
            }
            Self::AssetPreparationCancel {
                command_id,
                candidate,
            } if asset_preparation_enabled => {
                SessionOperation::AssetPreparation(AssetPreparationOperation::Cancel {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                    candidate: PreparationId::from_parts(
                        SessionId::from_raw(candidate.session.0),
                        candidate.epoch.0,
                    ),
                })
            }
            Self::AssetPreparationStatus
            | Self::AssetPreparationPrepare { .. }
            | Self::AssetPreparationCancel { .. } => {
                return Err(ProtocolError::new(
                    "asset_preparation_unavailable",
                    "asset preparation operations require an asset-preparation-enabled session",
                ));
            }
            Self::AssetActivationStatus if asset_activation_enabled => {
                SessionOperation::AssetActivation(AssetActivationOperation::Status)
            }
            Self::AssetActivationStage {
                command_id,
                candidate,
                expected_active_generation,
            } if asset_activation_enabled => {
                SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                    candidate: PreparationId::from_parts(
                        SessionId::from_raw(candidate.session.0),
                        candidate.epoch.0,
                    ),
                    expected_active_generation: expected_active_generation.0,
                })
            }
            Self::AssetActivationCommit {
                command_id,
                activation,
                expected_active_generation,
            } if asset_activation_enabled => {
                SessionOperation::AssetActivation(AssetActivationOperation::Commit {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                    activation: ActivationId::from_parts(
                        SessionId::from_raw(activation.session.0),
                        activation.sequence.0,
                    ),
                    expected_active_generation: expected_active_generation.0,
                })
            }
            Self::AssetActivationCancel {
                command_id,
                activation,
            } if asset_activation_enabled => {
                SessionOperation::AssetActivation(AssetActivationOperation::Cancel {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                    activation: ActivationId::from_parts(
                        SessionId::from_raw(activation.session.0),
                        activation.sequence.0,
                    ),
                })
            }
            Self::AssetActivationStatus
            | Self::AssetActivationStage { .. }
            | Self::AssetActivationCommit { .. }
            | Self::AssetActivationCancel { .. } => {
                return Err(ProtocolError::new(
                    "asset_activation_unavailable",
                    "asset activation operations require an asset-activation-enabled session",
                ));
            }
            Self::AudioStatus => audio_operation(audio_enabled, AudioWireOperation::Status)?,
            Self::AudioClips { offset, limit } => audio_operation(
                audio_enabled,
                AudioWireOperation::Clips {
                    offset: *offset,
                    limit: *limit,
                },
            )?,
            Self::AudioVoice { voice } => {
                audio_operation(audio_enabled, AudioWireOperation::Voice { voice: *voice })?
            }
            Self::AudioVoices { offset, limit } => audio_operation(
                audio_enabled,
                AudioWireOperation::Voices {
                    offset: *offset,
                    limit: *limit,
                },
            )?,
            Self::AudioCommand {
                command_id,
                mutation,
            } => audio_operation(
                audio_enabled,
                AudioWireOperation::Command {
                    command_id: *command_id,
                    mutation: mutation.clone(),
                },
            )?,
        };
        Ok(operation)
    }
}

#[cfg_attr(not(feature = "audio"), allow(dead_code))]
enum AudioWireOperation {
    Status,
    Clips {
        offset: usize,
        limit: Option<usize>,
    },
    Voice {
        voice: WireVoiceId,
    },
    Voices {
        offset: usize,
        limit: Option<usize>,
    },
    Command {
        command_id: Option<DecimalU128>,
        mutation: WireAudioMutation,
    },
}

fn audio_operation(
    audio_enabled: bool,
    operation: AudioWireOperation,
) -> Result<SessionOperation, ProtocolError> {
    if !audio_enabled {
        return Err(ProtocolError::new(
            "audio_unavailable",
            "audio operations require an audio-enabled session",
        ));
    }
    #[cfg(feature = "audio")]
    {
        let native_voice = |voice: WireVoiceId| {
            SessionAudioVoiceId::from_parts(SessionId::from_raw(voice.session.0), voice.sequence.0)
        };
        let operation = match operation {
            AudioWireOperation::Status => SessionAudioOperation::Status,
            AudioWireOperation::Clips { offset, limit } => {
                if limit.is_some_and(|limit| limit == 0 || limit > 1_024) {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "audio clip page limit must be in 1..=1024",
                    ));
                }
                SessionAudioOperation::Clips { offset, limit }
            }
            AudioWireOperation::Voice { voice } => SessionAudioOperation::Voice {
                voice: native_voice(voice),
            },
            AudioWireOperation::Voices { offset, limit } => {
                if limit.is_some_and(|limit| limit == 0 || limit > 1_024) {
                    return Err(ProtocolError::new(
                        "invalid_request",
                        "audio voice page limit must be in 1..=1024",
                    ));
                }
                SessionAudioOperation::Voices { offset, limit }
            }
            AudioWireOperation::Command {
                command_id,
                mutation,
            } => {
                let mutation = match mutation {
                    WireAudioMutation::Play {
                        clip,
                        volume,
                        pan,
                        looping,
                    } => daedalus_runtime::SessionAudioMutation::Play {
                        clip,
                        volume,
                        pan,
                        looping,
                    },
                    WireAudioMutation::Stop { voice } => {
                        daedalus_runtime::SessionAudioMutation::Stop {
                            voice: native_voice(voice),
                        }
                    }
                    WireAudioMutation::SetVolume { voice, volume } => {
                        daedalus_runtime::SessionAudioMutation::SetVolume {
                            voice: native_voice(voice),
                            volume,
                        }
                    }
                    WireAudioMutation::SetPan { voice, pan } => {
                        daedalus_runtime::SessionAudioMutation::SetPan {
                            voice: native_voice(voice),
                            pan,
                        }
                    }
                    WireAudioMutation::RenderOffline { frames } => {
                        if !(1..=4_096).contains(&frames) {
                            return Err(ProtocolError::new(
                                "invalid_request",
                                "offline render frames must be in 1..=4096",
                            ));
                        }
                        daedalus_runtime::SessionAudioMutation::RenderOffline { frames }
                    }
                };
                SessionAudioOperation::Command {
                    command_id: command_id.map(|id| CommandId::from_raw(id.0)),
                    mutation,
                }
            }
        };
        Ok(SessionOperation::Audio(operation))
    }
    #[cfg(not(feature = "audio"))]
    {
        let _ = operation;
        Err(ProtocolError::new(
            "audio_unavailable",
            "audio support is not compiled into this CLI",
        ))
    }
}

impl WireTarget {
    fn native(&self) -> Result<daedalus_runtime::SessionEntityTarget, ProtocolError> {
        match (&self.stable_id, self.handle) {
            (Some(id), None) => Ok(daedalus_runtime::SessionEntityTarget::StableId(id.clone())),
            (None, Some(handle)) => Ok(daedalus_runtime::SessionEntityTarget::Handle(
                EntityHandle {
                    session: SessionId::from_raw(handle.session.0),
                    index: handle.index.0,
                    generation: handle.generation.0,
                },
            )),
            _ => Err(ProtocolError::new(
                "invalid_target",
                "target must contain exactly one of stable_id or handle",
            )),
        }
    }
}

macro_rules! decimal_type {
    ($name:ident, $inner:ty) => {
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct $name($inner);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let text = String::deserialize(deserializer)?;
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(serde::de::Error::custom(
                        "decimal IDs must contain only ASCII digits",
                    ));
                }
                text.parse::<$inner>()
                    .map(Self)
                    .map_err(serde::de::Error::custom)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.0.to_string())
            }
        }
    };
}

decimal_type!(DecimalU32, u32);
decimal_type!(DecimalU64, u64);
decimal_type!(DecimalU128, u128);

#[derive(Serialize)]
struct EnvelopeOut {
    protocol: u32,
    request_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<WireResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum WireResult {
    Capabilities(Capabilities),
    #[cfg(feature = "window")]
    ViewStatus(serde_json::Value),
    Submitted(Submitted),
    Poll(Box<PollResponse>),
    Acked,
    Forgotten,
    LatestFrame(LatestFrame),
    CloseRequested,
}

#[derive(Serialize)]
struct Capabilities {
    clock: &'static str,
    operations: Vec<&'static str>,
    audio_enabled: bool,
    asset_preparation_enabled: bool,
    asset_activation_enabled: bool,
    view_status: bool,
    max_line_bytes: usize,
    max_response_bytes: usize,
    max_retained_tickets: usize,
    normal_tickets: usize,
    step_tickets: usize,
    native_command_capacity: usize,
    native_max_clients: usize,
    native_max_pending_per_client: usize,
    native_max_pending_total: usize,
    native_max_result_bytes_total: usize,
    native_max_page_entities: usize,
    native_max_steps_per_request: u32,
    native_max_retained_frames: usize,
    native_replay_ledger_capacity: usize,
    live_capture: bool,
    mcp: bool,
}

#[derive(Serialize)]
struct Submitted {
    ticket: String,
    request_id_reused: bool,
}

#[derive(Serialize)]
struct PollResponse {
    phase: &'static str,
    authority: Option<WireAuthority>,
    terminal: Option<WireTerminal>,
}

#[derive(Serialize)]
struct WireAuthority {
    command_id: DecimalU128,
    submission: &'static str,
    apply_tick: Option<DecimalU64>,
    duplicate_result: Option<WireCommandResult>,
}

#[derive(Serialize)]
struct WireTerminal {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<OperationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum OperationResult {
    ProjectInfo(ProjectInfoOut),
    Spawned(SpawnedOut),
    Transform(TransformOut),
    Transforms(TransformsOut),
    Command(WireCommandResult),
    Step(Vec<StepOut>),
    Status(StatusOut),
    AssetPreparation(AssetPreparationOut),
    AssetActivation(AssetActivationOut),
    #[cfg(feature = "audio")]
    Audio(AudioResultOut),
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AssetActivationOut {
    Status(Box<AssetActivationStatusOut>),
    Command(AssetActivationCommandOut),
}

#[derive(Serialize)]
struct AssetActivationStatusOut {
    session: DecimalU64,
    state: &'static str,
    active_generation: DecimalU64,
    active_source_digest: String,
    current_activation: Option<AssetActivationSummaryOut>,
    latest_failure: Option<AssetActivationFailureOut>,
    memory: AssetActivationMemoryOut,
}

#[derive(Serialize)]
struct AssetActivationSummaryOut {
    id: WireActivationId,
    candidate: PreparationIdOut,
    expected_active_generation: DecimalU64,
    source_digest: String,
    committed_generation: Option<DecimalU64>,
}

#[derive(Serialize)]
struct AssetActivationFailureOut {
    id: WireActivationId,
    code: String,
    message: String,
    compatibility: Option<CompatibilityFailureOut>,
}

#[derive(Serialize)]
struct CompatibilityFailureOut {
    kind: &'static str,
    entity_id: Option<String>,
    component_id: Option<String>,
    asset_id: Option<String>,
}

#[derive(Serialize)]
struct AssetActivationMemoryOut {
    active_cooked_accounted_bytes: u64,
    candidate_cooked_accounted_bytes: u64,
    retired_cooked_accounted_bytes: u64,
    total_cooked_accounted_bytes: u64,
    max_cooked_accounted_bytes: u64,
    retired_generation_count: usize,
}

#[derive(Serialize)]
struct AssetActivationCommandOut {
    command_id: DecimalU128,
    observed_tick: DecimalU64,
    outcome: AssetActivationCommandOutcomeOut,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AssetActivationCommandOutcomeOut {
    StageAccepted {
        activation: WireActivationId,
        candidate: PreparationIdOut,
    },
    CommitApplied {
        activation: WireActivationId,
        generation: DecimalU64,
    },
    CancelAccepted {
        activation: WireActivationId,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AssetPreparationOut {
    Status(Box<AssetPreparationStatusOut>),
    Command(AssetPreparationCommandOut),
}

#[derive(Serialize)]
struct AssetPreparationStatusOut {
    session: DecimalU64,
    active_generation: DecimalU64,
    active_source_digest: String,
    active_cooked_accounted_bytes: u64,
    desired: Option<PreparationIdOut>,
    running: Option<PreparationIdOut>,
    pending: Option<PreparationIdOut>,
    state: &'static str,
    ready_candidate: Option<PreparationCandidateOut>,
    latest_failure: Option<PreparationFailureOut>,
}

#[derive(Serialize)]
struct PreparationIdOut {
    session: DecimalU64,
    epoch: DecimalU64,
}

#[derive(Serialize)]
struct PreparationCandidateOut {
    id: PreparationIdOut,
    is_current: bool,
    source_digest: String,
    catalog_digest: String,
    source_scene_revision: DecimalU64,
    asset_count: usize,
    entity_count: usize,
    memory: AssetImportMemorySummary,
    metrics: AssetImportMetrics,
}

#[derive(Serialize)]
struct PreparationFailureOut {
    id: PreparationIdOut,
    code: String,
    message: String,
    source_id: Option<String>,
    path: Option<String>,
    diagnostic_truncated: bool,
}

#[derive(Serialize)]
struct AssetPreparationCommandOut {
    command_id: DecimalU128,
    observed_tick: DecimalU64,
    outcome: AssetPreparationCommandOutcomeOut,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AssetPreparationCommandOutcomeOut {
    PrepareAccepted { candidate: PreparationIdOut },
    CancelAccepted { candidate: PreparationIdOut },
}

#[derive(Serialize)]
struct ProjectInfoOut {
    session: DecimalU64,
    tick: DecimalU64,
    name: String,
    schema_version: u32,
    source_scene_revision: DecimalU64,
    source_digest: String,
    entity_count: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    source_files: Vec<SourceFileOut>,
    entities: Vec<BindingOut>,
}

#[derive(Serialize)]
struct SourceFileOut {
    source_id: String,
    content_sha256: String,
    byte_len: DecimalU64,
}

#[derive(Serialize)]
struct BindingOut {
    id: String,
    handle: HandleOut,
}

#[derive(Serialize)]
struct HandleOut {
    session: DecimalU64,
    index: DecimalU32,
    generation: DecimalU32,
}

#[derive(Serialize)]
struct SpawnedOut {
    handle: HandleOut,
    entity_count: usize,
}

#[derive(Serialize)]
struct TransformOut {
    observed_tick: DecimalU64,
    entity: EntityOut,
}

#[derive(Serialize)]
struct TransformsOut {
    observed_tick: DecimalU64,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    entities: Vec<EntityOut>,
}

#[derive(Serialize)]
struct EntityOut {
    stable_id: Option<String>,
    handle: HandleOut,
    transform: Transform,
    revision: DecimalU64,
}

#[derive(Serialize)]
struct WireCommandResult {
    command_id: DecimalU128,
    outcome: CommandOutcomeOut,
    resolve_tick: Option<DecimalU64>,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum CommandOutcomeOut {
    Applied { revision: DecimalU64 },
    Despawned,
    Rejected(CommandRejectOut),
}

#[derive(Serialize)]
#[serde(tag = "code", content = "data", rename_all = "snake_case")]
enum CommandRejectOut {
    WrongSession,
    StaleEntity,
    MissingTransform,
    MissingRevision,
    InvalidTransform,
    RevisionConflict {
        expected: DecimalU64,
        actual: DecimalU64,
    },
}

#[derive(Serialize)]
struct StepOut {
    tick: DecimalU64,
    commands: Vec<WireCommandResult>,
    snapshot: SnapshotOut,
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum SnapshotOut {
    Published { sequence: DecimalU64 },
    SkippedAtCapacity,
    SkippedAtEntityLimit { entity_count: usize, limit: usize },
    Busy,
    Poisoned,
    SequenceExhausted,
}

#[derive(Serialize)]
struct StatusOut {
    session: DecimalU64,
    clock: &'static str,
    tick: DecimalU64,
    entity_count: usize,
    pending_commands: usize,
    active_clients: usize,
    stopping: bool,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AudioResultOut {
    Status(AudioStatusOut),
    Clips(AudioClipPageOut),
    Voice(AudioVoiceOut),
    Voices(AudioVoicePageOut),
    Command(AudioCommandOut),
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AudioBackendOut {
    Offline { sample_rate: u32 },
    Native,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioStatusOut {
    session: DecimalU64,
    backend: AudioBackendOut,
    alias_count: usize,
    unique_clip_count: usize,
    converted_clip_count: usize,
    converted_frame_bytes: DecimalU64,
    active_voice_count: usize,
    offline_progressed_frames: DecimalU64,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioClipOut {
    asset_id: String,
    leaf_key: String,
    sample_rate: u32,
    channels: u16,
    frames: DecimalU64,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioClipPageOut {
    observed_tick: DecimalU64,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    clips: Vec<AudioClipOut>,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioVoiceOut {
    voice: WireVoiceId,
    state: &'static str,
    clip_id: Option<String>,
    requested_volume: Option<f32>,
    requested_pan: Option<f32>,
    looping: Option<bool>,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioVoicePageOut {
    observed_tick: DecimalU64,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    voices: Vec<AudioVoiceOut>,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct AudioCommandOut {
    command_id: DecimalU128,
    observed_tick: DecimalU64,
    outcome: AudioCommandTerminalOut,
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum AudioCommandTerminalOut {
    Played {
        voice: WireVoiceId,
    },
    StopRequested {
        voice: WireVoiceId,
    },
    VolumeSet {
        voice: WireVoiceId,
        requested_volume: f32,
    },
    PanSet {
        voice: WireVoiceId,
        requested_pan: f32,
    },
    Offline(OfflineAudioSummaryOut),
    Error {
        code: &'static str,
        message: &'static str,
    },
}

#[cfg(feature = "audio")]
#[derive(Serialize)]
struct OfflineAudioSummaryOut {
    frames: usize,
    sample_rate: u32,
    channels: u16,
    peak: f32,
    rms: f32,
    left_peak: f32,
    left_rms: f32,
    right_peak: f32,
    right_rms: f32,
    nonzero_samples: usize,
    cumulative_frames: DecimalU64,
}

#[derive(Serialize)]
struct LatestFrame {
    session: DecimalU64,
    tick: DecimalU64,
    sequence: DecimalU64,
    authority_snapshot_sequence: DecimalU64,
    project_generation: DecimalU64,
    source_scene_revision: DecimalU64,
    source_digest: String,
}

#[derive(Serialize, Clone, Debug)]
struct WireError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<ErrorDetails>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum ErrorDetails {
    RevisionConflict {
        expected: DecimalU64,
        actual: DecimalU64,
    },
    CommandId {
        command_id: DecimalU128,
    },
    ReplayWindowExpired {
        command_id: DecimalU128,
        high_water: DecimalU128,
    },
}

struct ProtocolError {
    code: String,
    message: String,
    retryable: Option<bool>,
    details: Option<ErrorDetails>,
}

impl ProtocolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: None,
            details: None,
        }
    }
    fn retryable(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: Some(true),
            details: None,
        }
    }
    fn wire(self) -> WireError {
        WireError {
            code: self.code,
            message: self.message,
            retryable: self.retryable,
            details: self.details,
        }
    }
}

struct TicketEntry {
    request_id: String,
    operation: SessionOperation,
    ticket: RequestTicket,
    step: bool,
}

/// The attachment-local ticket book. It deliberately has no unbounded request-ID ledger.
pub(crate) struct ProtocolController {
    transport: StdioTransport,
    client: SessionControlClient,
    clock: SessionClockMode,
    native_limits: SessionControlLimits,
    audio_enabled: bool,
    asset_preparation_enabled: bool,
    asset_activation_enabled: bool,
    #[cfg(feature = "window")]
    view_status: Option<ManagedViewStatusAttachment>,
    tickets: HashMap<String, TicketEntry>,
    submit_ids: HashMap<String, String>,
    detached_reason: Option<String>,
    accepted_requests: u64,
    duplicate_submissions: u64,
    errors: u64,
    closing: bool,
}

#[cfg(feature = "window")]
struct ManagedViewStatusAttachment {
    control: NativeProjectHostControl,
    session: SessionId,
}

impl ProtocolController {
    pub(crate) fn new(
        transport: StdioTransport,
        client: SessionControlClient,
        clock: SessionClockMode,
        native_limits: SessionControlLimits,
        audio_enabled: bool,
    ) -> Self {
        let asset_preparation_enabled = client.asset_preparation_enabled();
        let asset_activation_enabled = client.asset_activation_enabled();
        Self {
            transport,
            client,
            clock,
            native_limits,
            audio_enabled,
            asset_preparation_enabled,
            asset_activation_enabled,
            #[cfg(feature = "window")]
            view_status: None,
            tickets: HashMap::new(),
            submit_ids: HashMap::new(),
            detached_reason: None,
            accepted_requests: 0,
            duplicate_submissions: 0,
            errors: 0,
            closing: false,
        }
    }

    #[cfg(feature = "window")]
    pub(crate) fn attach_managed_view_status(
        &mut self,
        control: NativeProjectHostControl,
        session: SessionId,
    ) {
        self.view_status = Some(ManagedViewStatusAttachment { control, session });
    }

    /// Process a bounded batch. No blocking IO, runtime wait, or renderer callback occurs here.
    pub(crate) fn pump(&mut self) {
        if self.detached_reason.is_some() {
            return;
        }
        for _ in 0..MAX_PROTOCOL_QUEUE {
            let raw = match self.transport.try_request() {
                Ok(Some(raw)) => raw,
                Ok(None) => break,
                Err(TransportRead::Detached) => {
                    self.detach(self.transport.terminal_reason().unwrap_or("stdin_detached"));
                    break;
                }
            };
            self.accepted_requests = self.accepted_requests.saturating_add(1);
            let (request_id, response) = self.handle_line(&raw);
            if !response.ok {
                self.errors = self.errors.saturating_add(1);
            }
            let encoded = encode_response(&request_id, response);
            match encoded {
                Ok(bytes) => match self.transport.send(bytes) {
                    Ok(()) => {}
                    Err(TransportWrite::Full) => {
                        self.detach("output_queue_full");
                        break;
                    }
                    Err(TransportWrite::BrokenPipe) => {
                        self.detach("broken_stdout");
                        break;
                    }
                },
                Err(()) => {
                    let fallback = encode_error(
                        &request_id,
                        ProtocolError::new(
                            "response_too_large",
                            "response exceeds the 1 MiB JSONL limit",
                        ),
                    );
                    if fallback
                        .and_then(|bytes| self.transport.send(bytes).map_err(|_| ()))
                        .is_err()
                    {
                        self.detach("output_queue_full");
                        break;
                    }
                }
            }
        }
        if !self.transport.has_pending_request()
            && let Some(reason) = self.transport.terminal_reason()
        {
            self.detach(reason);
        }
    }

    pub(crate) fn closing(&self) -> bool {
        self.closing
    }

    pub(crate) fn session_stopping(&self) -> bool {
        self.client.is_stopping()
    }

    pub(crate) fn detach_reason(&self) -> Option<&str> {
        self.detached_reason.as_deref()
    }
    #[cfg(feature = "window")]
    pub(crate) fn accepted_requests(&self) -> u64 {
        self.accepted_requests
    }
    #[cfg(feature = "window")]
    pub(crate) fn duplicate_submissions(&self) -> u64 {
        self.duplicate_submissions
    }
    #[cfg(feature = "window")]
    pub(crate) fn errors(&self) -> u64 {
        self.errors
    }
    #[cfg(any(test, feature = "window"))]
    pub(crate) fn retained_tickets(&self) -> usize {
        self.tickets.len()
    }

    fn detach(&mut self, reason: &str) {
        if self.detached_reason.is_some() {
            return;
        }
        self.detached_reason = Some(reason.to_owned());
        self.tickets.clear();
        self.submit_ids.clear();
    }

    fn handle_request(&mut self, request: ClientRequest) -> Result<WireResult, ProtocolError> {
        match request {
            ClientRequest::Capabilities {} => Ok(WireResult::Capabilities(Capabilities {
                clock: clock_name(self.clock),
                operations: operation_names(
                    self.audio_enabled,
                    self.asset_preparation_enabled,
                    self.asset_activation_enabled,
                ),
                audio_enabled: self.audio_enabled,
                asset_preparation_enabled: self.asset_preparation_enabled,
                asset_activation_enabled: self.asset_activation_enabled,
                view_status: self.managed_view_status_enabled(),
                max_line_bytes: MAX_PROTOCOL_LINE_BYTES,
                max_response_bytes: MAX_PROTOCOL_RESPONSE_BYTES,
                max_retained_tickets: MAX_PROTOCOL_TICKETS,
                normal_tickets: MAX_PROTOCOL_NORMAL_TICKETS,
                step_tickets: 1,
                native_command_capacity: self.native_limits.command_capacity,
                native_max_clients: self.native_limits.max_clients,
                native_max_pending_per_client: self.native_limits.max_pending_per_client,
                native_max_pending_total: self.native_limits.max_pending_total,
                native_max_result_bytes_total: self.native_limits.max_result_bytes_total,
                native_max_page_entities: self.native_limits.max_page_entities,
                native_max_steps_per_request: self.native_limits.max_steps_per_request,
                native_max_retained_frames: self.native_limits.max_retained_frames,
                native_replay_ledger_capacity: self.native_limits.replay_ledger_capacity,
                live_capture: false,
                mcp: false,
            })),
            ClientRequest::ViewStatus {} => self.read_managed_view_status(),
            ClientRequest::Submit { .. } => Err(ProtocolError::new(
                "protocol_internal",
                "submit dispatch requires request_id",
            )),
            ClientRequest::Poll { ticket } => self.poll_ticket(&ticket),
            ClientRequest::Ack { ticket } => self.ack_ticket(&ticket),
            ClientRequest::Forget { ticket } => self.forget_ticket(&ticket),
            ClientRequest::LatestFrame {} => self.latest_frame(),
            ClientRequest::Close {} => {
                if !self.closing {
                    self.closing = true;
                }
                // This receipt records a bounded local intent. The owning serve/view loop
                // performs its own shutdown or renderer-message handoff.
                Ok(WireResult::CloseRequested)
            }
        }
    }

    fn managed_view_status_enabled(&self) -> bool {
        #[cfg(feature = "window")]
        {
            self.view_status.is_some()
        }
        #[cfg(not(feature = "window"))]
        {
            false
        }
    }

    fn read_managed_view_status(&self) -> Result<WireResult, ProtocolError> {
        #[cfg(feature = "window")]
        {
            let Some(attachment) = self.view_status.as_ref() else {
                return Err(ProtocolError::new(
                    "view_status_unavailable",
                    "live view status is available only on a managed project view attachment",
                ));
            };
            managed_view_status_result(attachment.control.try_status(), attachment.session)
        }
        #[cfg(not(feature = "window"))]
        {
            Err(ProtocolError::new(
                "view_status_unavailable",
                "live view status is available only on a managed project view attachment",
            ))
        }
    }

    fn submit_with_id(
        &mut self,
        request_id: String,
        operation: SessionOperation,
    ) -> Result<WireResult, ProtocolError> {
        if !valid_request_id(&request_id) {
            return Err(ProtocolError::new(
                "invalid_request_id",
                "request_id must be 1..=128 printable ASCII bytes",
            ));
        }
        if self.closing {
            return Err(ProtocolError::new(
                "window_closing",
                "new submissions are rejected after the project window starts closing",
            ));
        }
        if let Some(token) = self.submit_ids.get(&request_id) {
            let entry = self.tickets.get(token).ok_or_else(|| {
                ProtocolError::new(
                    "ticket_book_invariant",
                    "retained submit correlation has no ticket",
                )
            })?;
            if entry.operation == operation {
                self.duplicate_submissions = self.duplicate_submissions.saturating_add(1);
                return Ok(WireResult::Submitted(Submitted {
                    ticket: token.clone(),
                    request_id_reused: true,
                }));
            }
            return Err(ProtocolError::new(
                "request_id_collision",
                "request_id is retained for a different submit payload",
            ));
        }
        let step = matches!(operation, SessionOperation::Step { .. });
        if self.tickets.len() >= MAX_PROTOCOL_TICKETS
            || (!step
                && self.tickets.values().filter(|entry| !entry.step).count()
                    >= MAX_PROTOCOL_NORMAL_TICKETS)
            || (step && self.tickets.values().any(|entry| entry.step))
        {
            return Err(ProtocolError::new(
                "result_capacity",
                "protocol ticket retention limit is full; ack or forget a ticket",
            ));
        }
        let ticket = self
            .client
            .try_submit(operation.clone())
            .map_err(submit_error)?;
        let id = ticket.id();
        let token = format!(
            "t{}-{}-{}",
            id.session().get(),
            id.client().get(),
            id.sequence()
        );
        self.submit_ids.insert(request_id.clone(), token.clone());
        self.tickets.insert(
            token.clone(),
            TicketEntry {
                request_id,
                operation,
                ticket,
                step,
            },
        );
        Ok(WireResult::Submitted(Submitted {
            ticket: token,
            request_id_reused: false,
        }))
    }

    fn poll_ticket(&self, token: &str) -> Result<WireResult, ProtocolError> {
        let entry = self.tickets.get(token).ok_or_else(|| {
            ProtocolError::new(
                "unknown_ticket",
                "ticket is not retained by this protocol attachment",
            )
        })?;
        let progress = entry.ticket.try_state().map_err(read_error)?;
        Ok(WireResult::Poll(Box::new(poll_response(&progress))))
    }

    fn ack_ticket(&mut self, token: &str) -> Result<WireResult, ProtocolError> {
        let entry = self.tickets.get_mut(token).ok_or_else(|| {
            ProtocolError::new(
                "unknown_ticket",
                "ticket is not retained by this protocol attachment",
            )
        })?;
        entry.ticket.acknowledge().map_err(ack_error)?;
        let request_id = entry.request_id.clone();
        self.tickets.remove(token);
        self.submit_ids.remove(&request_id);
        Ok(WireResult::Acked)
    }

    fn forget_ticket(&mut self, token: &str) -> Result<WireResult, ProtocolError> {
        let entry = self.tickets.remove(token).ok_or_else(|| {
            ProtocolError::new(
                "unknown_ticket",
                "ticket is not retained by this protocol attachment",
            )
        })?;
        self.submit_ids.remove(&entry.request_id);
        // Drop is the runtime's explicit abandon path. It does not cancel accepted work.
        Ok(WireResult::Forgotten)
    }

    fn latest_frame(&self) -> Result<WireResult, ProtocolError> {
        let frame = self.client.try_latest_frame().map_err(frame_error)?;
        let Some(frame) = frame else {
            return Err(ProtocolError::retryable(
                "frame_unavailable",
                "no complete project frame is available yet",
            ));
        };
        Ok(WireResult::LatestFrame(LatestFrame {
            session: DecimalU64(frame.session().get()),
            tick: DecimalU64(frame.tick().get()),
            sequence: DecimalU64(frame.sequence()),
            authority_snapshot_sequence: DecimalU64(frame.authority_snapshot_sequence()),
            project_generation: DecimalU64(frame.project_generation()),
            source_scene_revision: DecimalU64(frame.source_scene_revision()),
            source_digest: frame.source_digest().to_owned(),
        }))
    }
}

#[cfg(feature = "window")]
fn managed_view_status_result(
    read: Result<
        NativeProjectHostStatusRead,
        daedalus_project_host::NativeProjectHostStatusReadError,
    >,
    expected_session: SessionId,
) -> Result<WireResult, ProtocolError> {
    let status = match read {
        Ok(NativeProjectHostStatusRead::Value(status)) => status,
        Ok(NativeProjectHostStatusRead::Busy) => {
            return Err(ProtocolError::retryable(
                "view_status_busy",
                "managed project view status is temporarily busy",
            ));
        }
        Err(_) => {
            return Err(ProtocolError::new(
                "view_status_poisoned",
                "managed project view status is unavailable after an internal failure",
            ));
        }
    };
    if status.session() != expected_session {
        return Err(ProtocolError::new(
            "view_status_session_mismatch",
            "managed project view status belongs to another session",
        ));
    }
    Ok(WireResult::ViewStatus(native_project_host_status_json(
        &status,
    )))
}

// The request ID is part of submit replay semantics; dispatch owns it so every submit is
// correlated before entering the bounded native ticket book.
impl ProtocolController {
    fn handle_line(&mut self, raw: &[u8]) -> (String, EnvelopeOut) {
        let parsed = std::str::from_utf8(raw)
            .map_err(|_| ProtocolError::new("invalid_json", "request line is not UTF-8"))
            .and_then(|text| {
                serde_json::from_str::<Envelope>(text).map_err(|error| {
                    ProtocolError::new(
                        "invalid_request",
                        format!("invalid protocol-2 request: {error}"),
                    )
                })
            });
        match parsed {
            Ok(envelope) => {
                let request_id = envelope.request_id;
                let result = if envelope.protocol != PROTOCOL {
                    Err(ProtocolError::new(
                        "unsupported_protocol",
                        "protocol must be 2",
                    ))
                } else if !valid_request_id(&request_id) {
                    Err(ProtocolError::new(
                        "invalid_request_id",
                        "request_id must be 1..=128 printable ASCII bytes",
                    ))
                } else {
                    match envelope.request {
                        ClientRequest::Submit { operation } => operation
                            .native(
                                self.audio_enabled,
                                self.asset_preparation_enabled,
                                self.asset_activation_enabled,
                            )
                            .and_then(|operation| {
                                self.submit_with_id(request_id.clone(), operation)
                            }),
                        other => self.handle_request(other),
                    }
                };
                let response = match result {
                    Ok(result) => EnvelopeOut {
                        protocol: PROTOCOL,
                        request_id: request_id.clone(),
                        ok: true,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => EnvelopeOut {
                        protocol: PROTOCOL,
                        request_id: request_id.clone(),
                        ok: false,
                        result: None,
                        error: Some(error.wire()),
                    },
                };
                (request_id, response)
            }
            Err(error) => (
                String::new(),
                EnvelopeOut {
                    protocol: PROTOCOL,
                    request_id: String::new(),
                    ok: false,
                    result: None,
                    error: Some(error.wire()),
                },
            ),
        }
    }
}

#[cfg(feature = "window")]
pub(crate) fn native_project_host_status_json(status: &NativeProjectHostStatus) -> Value {
    let cpu = status.cpu();
    json!({
        "lifecycle": match status.lifecycle() {
            NativeProjectHostLifecycle::Initializing => "initializing",
            NativeProjectHostLifecycle::InitialPreparing => "initial_preparing",
            NativeProjectHostLifecycle::CpuPrepared => "cpu_prepared",
            NativeProjectHostLifecycle::Ready => "ready",
            NativeProjectHostLifecycle::ViewRunning => "view_running",
            NativeProjectHostLifecycle::ViewClosed => "view_closed",
            NativeProjectHostLifecycle::Stopping => "stopping",
            NativeProjectHostLifecycle::Stopped => "stopped",
            NativeProjectHostLifecycle::Failed => "failed",
        },
        "session": status.session().get().to_string(),
        "view_started": status.view_started(),
        "view_closed": status.view_closed(),
        "active_generation": status.active_generation().to_string(),
        "active_source_digest": status.active_source_digest(),
        "queued_publication": status.last_queued_publication().map(project_publication_identity_json),
        "last_presented": status.last_presented().map(native_presented_summary_json),
        "cpu": {
            "generation_slots_used": cpu.generation_slots_used,
            "generation_slots_limit": cpu.generation_slots_limit,
            "frame_slots_used": cpu.frame_slots_used,
            "frame_slots_limit": cpu.frame_slots_limit,
            "prepared_metadata_bytes": cpu.prepared_metadata_bytes,
            "prepared_metadata_reserved_bytes": cpu.prepared_metadata_reserved_bytes,
            "legacy_mesh_copy_bytes": cpu.legacy_mesh_copy_bytes,
            "legacy_mesh_copy_reserved_bytes": cpu.legacy_mesh_copy_reserved_bytes,
            "frame_bytes": cpu.frame_bytes,
            "frame_reserved_bytes": cpu.frame_reserved_bytes,
            "scratch_current_bytes": cpu.scratch_current_bytes,
            "scratch_peak_bytes": cpu.scratch_peak_bytes,
        },
        "failure": status.failure().map(|failure| json!({
            "code":failure.code(),
            "message":failure.message(),
        })),
        "live_capture": false,
    })
}

#[cfg(feature = "window")]
pub(crate) fn project_publication_identity_json(
    identity: &daedalus_runtime::ProjectPublicationIdentity,
) -> Value {
    json!({
        "session":identity.session().get().to_string(),
        "tick":identity.tick().get().to_string(),
        "sequence":identity.publication_sequence().to_string(),
        "authority_snapshot_sequence":identity.authority_snapshot_sequence().to_string(),
        "project_generation":identity.project_generation().to_string(),
        "source_digest":identity.source_digest(),
        "scene_revision":identity.scene_revision().to_string(),
    })
}

#[cfg(feature = "window")]
pub(crate) fn native_presented_summary_json(summary: &NativePresentedSummary) -> Value {
    let (physical_width, physical_height) = summary.physical_size();
    json!({
        "session":summary.session().get().to_string(),
        "tick":summary.tick().to_string(),
        "sequence":summary.publication_sequence().to_string(),
        "authority_snapshot_sequence":summary.authority_snapshot_sequence().map(|value| value.to_string()),
        "project_generation":summary.project_generation().to_string(),
        "source_digest":summary.source_digest(),
        "present_sequence":summary.present_sequence().to_string(),
        "physical_width":physical_width,
        "physical_height":physical_height,
        "render_view":Value::Null,
        "view_projection":Value::Null,
        "actual_view_available":false,
    })
}

fn valid_request_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
}

fn encode_response(_request_id: &str, response: EnvelopeOut) -> Result<Vec<u8>, ()> {
    let mut writer = CappedWriter {
        bytes: Vec::with_capacity(512),
        cap: MAX_PROTOCOL_RESPONSE_BYTES - 1,
    };
    serde_json::to_writer(&mut writer, &response).map_err(|_| ())?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

fn encode_error(request_id: &str, error: ProtocolError) -> Result<Vec<u8>, ()> {
    encode_response(
        request_id,
        EnvelopeOut {
            protocol: PROTOCOL,
            request_id: request_id.to_owned(),
            ok: false,
            result: None,
            error: Some(error.wire()),
        },
    )
}

struct CappedWriter {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.cap {
            return Err(io::Error::other("response byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn clock_name(clock: SessionClockMode) -> &'static str {
    match clock {
        SessionClockMode::Manual => "manual",
        SessionClockMode::Realtime => "realtime",
    }
}

fn submit_error(error: SessionSubmitError) -> ProtocolError {
    let code = match error {
        SessionSubmitError::Busy => return ProtocolError::retryable("busy", error.to_string()),
        SessionSubmitError::QueueFull => "queue_full",
        SessionSubmitError::ResultCapacity => "result_capacity",
        SessionSubmitError::Detached => "client_detached",
        SessionSubmitError::Stopped => "session_stopped",
        SessionSubmitError::CapabilityDenied => "capability_denied",
        SessionSubmitError::ClockModeMismatch => "clock_mode_mismatch",
        SessionSubmitError::InvalidRequest => "invalid_request",
        SessionSubmitError::AssetPreparationUnavailable => "asset_preparation_unavailable",
        SessionSubmitError::AssetActivationUnavailable => "asset_activation_unavailable",
        #[cfg(feature = "audio")]
        SessionSubmitError::AudioUnavailable => "audio_unavailable",
        SessionSubmitError::ReservedCommandId => "reserved_command_id",
        SessionSubmitError::QueueSequenceExhausted | SessionSubmitError::TicketIdExhausted => {
            "id_exhausted"
        }
    };
    ProtocolError::new(code, error.to_string())
}

fn read_error(error: RequestReadError) -> ProtocolError {
    match error {
        RequestReadError::Busy => ProtocolError::retryable("busy", error.to_string()),
        RequestReadError::Acknowledged => ProtocolError::new("acknowledged", error.to_string()),
        RequestReadError::Abandoned => ProtocolError::new("abandoned", error.to_string()),
        RequestReadError::Poisoned => ProtocolError::new("ticket_failed", error.to_string()),
    }
}

fn ack_error(error: RequestAckError) -> ProtocolError {
    match error {
        RequestAckError::Busy => ProtocolError::retryable("busy", error.to_string()),
        RequestAckError::NotTerminal => ProtocolError::new("not_terminal", error.to_string()),
        RequestAckError::Read(error) => read_error(error),
    }
}

fn frame_error(error: FrameReadError) -> ProtocolError {
    match error {
        FrameReadError::Busy => ProtocolError::retryable("busy", error.to_string()),
        FrameReadError::Poisoned => ProtocolError::new("frame_failed", error.to_string()),
    }
}

fn poll_response(progress: &RequestProgress) -> PollResponse {
    let phase = match progress.phase() {
        RequestPhase::Queued => "queued",
        RequestPhase::AuthorityAccepted => "authority_accepted",
        RequestPhase::Terminal => "terminal",
    };
    let authority = progress.authority().map(authority_out);
    let terminal = progress.terminal().map(|terminal| match terminal {
        Ok(result) => WireTerminal {
            ok: true,
            result: Some(result_out(result)),
            error: None,
        },
        Err(error) => WireTerminal {
            ok: false,
            result: None,
            error: Some(request_error(error).wire()),
        },
    });
    PollResponse {
        phase,
        authority,
        terminal,
    }
}

fn authority_out(admission: &AuthorityAdmission) -> WireAuthority {
    let (submission, duplicate_result) = match &admission.submission {
        daedalus_world::CommandSubmission::Enqueued => ("enqueued", None),
        daedalus_world::CommandSubmission::DuplicatePending => ("duplicate_pending", None),
        daedalus_world::CommandSubmission::DuplicateCompleted(result) => {
            ("duplicate_completed", Some(command_out(result, None)))
        }
    };
    WireAuthority {
        command_id: DecimalU128(admission.command_id.get()),
        submission,
        apply_tick: admission.apply_tick.map(|tick| DecimalU64(tick.get())),
        duplicate_result,
    }
}

fn result_out(result: &SessionRequestResult) -> OperationResult {
    match result {
        SessionRequestResult::ProjectInfo(page) => {
            OperationResult::ProjectInfo(project_info_out(page))
        }
        SessionRequestResult::Spawned {
            handle,
            entity_count,
        } => OperationResult::Spawned(SpawnedOut {
            handle: handle_out(*handle),
            entity_count: *entity_count,
        }),
        SessionRequestResult::Transform(state) => OperationResult::Transform(TransformOut {
            observed_tick: DecimalU64(state.observed_tick.get()),
            entity: entity_out(&state.entity),
        }),
        SessionRequestResult::Transforms(page) => OperationResult::Transforms(TransformsOut {
            observed_tick: DecimalU64(page.observed_tick.get()),
            total: page.total,
            offset: page.offset,
            limit: page.limit,
            next_offset: page.next_offset,
            entities: page.entities.iter().map(entity_out).collect(),
        }),
        SessionRequestResult::Command(command) => {
            OperationResult::Command(command_out(&command.result, command.resolve_tick))
        }
        SessionRequestResult::Step(steps) => {
            OperationResult::Step(steps.iter().map(step_out).collect())
        }
        SessionRequestResult::Status(status) => OperationResult::Status(status_out(status)),
        SessionRequestResult::AssetPreparation(result) => {
            OperationResult::AssetPreparation(preparation_result_out(result))
        }
        SessionRequestResult::AssetActivation(result) => {
            OperationResult::AssetActivation(asset_activation_result_out(result))
        }
        #[cfg(feature = "audio")]
        SessionRequestResult::Audio(audio) => OperationResult::Audio(audio_result_out(audio)),
    }
}

#[cfg(feature = "audio")]
fn audio_result_out(result: &SessionAudioResult) -> AudioResultOut {
    match result {
        SessionAudioResult::Status(status) => {
            let backend = match status.backend() {
                SessionAudioBackend::Offline { sample_rate } => {
                    AudioBackendOut::Offline { sample_rate }
                }
                SessionAudioBackend::Native => AudioBackendOut::Native,
            };
            AudioResultOut::Status(AudioStatusOut {
                session: DecimalU64(status.session().get()),
                backend,
                alias_count: status.alias_count(),
                unique_clip_count: status.unique_clip_count(),
                converted_clip_count: status.converted_clip_count(),
                converted_frame_bytes: DecimalU64(status.converted_frame_bytes()),
                active_voice_count: status.active_voice_count(),
                offline_progressed_frames: DecimalU64(status.offline_progressed_frames()),
            })
        }
        SessionAudioResult::Clips(page) => AudioResultOut::Clips(AudioClipPageOut {
            observed_tick: DecimalU64(page.observed_tick().get()),
            total: page.total(),
            offset: page.offset(),
            limit: page.limit(),
            next_offset: page.next_offset(),
            clips: page
                .clips()
                .iter()
                .map(|clip| AudioClipOut {
                    asset_id: clip.asset_id().as_str().to_owned(),
                    leaf_key: clip.key().hex(),
                    sample_rate: clip.sample_rate(),
                    channels: clip.channels(),
                    frames: DecimalU64(clip.frames()),
                })
                .collect(),
        }),
        SessionAudioResult::Voice(voice) => AudioResultOut::Voice(audio_voice_out(voice)),
        SessionAudioResult::Voices(page) => AudioResultOut::Voices(AudioVoicePageOut {
            observed_tick: DecimalU64(page.observed_tick().get()),
            total: page.total(),
            offset: page.offset(),
            limit: page.limit(),
            next_offset: page.next_offset(),
            voices: page.voices().iter().map(audio_voice_out).collect(),
        }),
        SessionAudioResult::Command(command) => {
            let outcome = match command.outcome() {
                Ok(SessionAudioCommandOutcome::Played { voice }) => {
                    AudioCommandTerminalOut::Played {
                        voice: voice_out(*voice),
                    }
                }
                Ok(SessionAudioCommandOutcome::StopRequested { voice }) => {
                    AudioCommandTerminalOut::StopRequested {
                        voice: voice_out(*voice),
                    }
                }
                Ok(SessionAudioCommandOutcome::VolumeSet {
                    voice,
                    requested_volume,
                }) => AudioCommandTerminalOut::VolumeSet {
                    voice: voice_out(*voice),
                    requested_volume: *requested_volume,
                },
                Ok(SessionAudioCommandOutcome::PanSet {
                    voice,
                    requested_pan,
                }) => AudioCommandTerminalOut::PanSet {
                    voice: voice_out(*voice),
                    requested_pan: *requested_pan,
                },
                Ok(SessionAudioCommandOutcome::Offline(summary)) => {
                    AudioCommandTerminalOut::Offline(offline_summary_out(*summary))
                }
                Err(error) => AudioCommandTerminalOut::Error {
                    code: error.code(),
                    message: error.message(),
                },
            };
            AudioResultOut::Command(AudioCommandOut {
                command_id: DecimalU128(command.command_id().get()),
                observed_tick: DecimalU64(command.observed_tick().get()),
                outcome,
            })
        }
    }
}

#[cfg(feature = "audio")]
fn voice_out(voice: SessionAudioVoiceId) -> WireVoiceId {
    WireVoiceId {
        session: DecimalU64(voice.session().get()),
        sequence: DecimalU64(voice.sequence()),
    }
}

#[cfg(feature = "audio")]
fn audio_voice_out(voice: &daedalus_runtime::SessionAudioVoiceInfo) -> AudioVoiceOut {
    AudioVoiceOut {
        voice: voice_out(voice.voice()),
        state: match voice.state() {
            SessionAudioVoiceState::Playing => "playing",
            SessionAudioVoiceState::StopRequested => "stop_requested",
            SessionAudioVoiceState::Ended => "ended",
        },
        clip_id: voice.clip_id().map(|id| id.as_str().to_owned()),
        requested_volume: voice.requested_volume(),
        requested_pan: voice.requested_pan(),
        looping: voice.looping(),
    }
}

#[cfg(feature = "audio")]
fn offline_summary_out(summary: OfflineAudioSummary) -> OfflineAudioSummaryOut {
    OfflineAudioSummaryOut {
        frames: summary.frames(),
        sample_rate: summary.sample_rate(),
        channels: summary.channels(),
        peak: summary.peak(),
        rms: summary.rms(),
        left_peak: summary.left_peak(),
        left_rms: summary.left_rms(),
        right_peak: summary.right_peak(),
        right_rms: summary.right_rms(),
        nonzero_samples: summary.nonzero_samples(),
        cumulative_frames: DecimalU64(summary.cumulative_frames()),
    }
}

fn project_info_out(page: &ProjectInfoPage) -> ProjectInfoOut {
    ProjectInfoOut {
        session: DecimalU64(page.session.get()),
        tick: DecimalU64(page.tick.get()),
        name: page.name.clone(),
        schema_version: page.schema_version,
        source_scene_revision: DecimalU64(page.source_scene_revision),
        source_digest: page.source_digest.clone(),
        entity_count: page.entity_count,
        offset: page.offset,
        limit: page.limit,
        next_offset: page.next_offset,
        source_files: page
            .source_files
            .iter()
            .map(|file| SourceFileOut {
                source_id: file.source_id.clone(),
                content_sha256: file.content_sha256.clone(),
                byte_len: DecimalU64(file.byte_len),
            })
            .collect(),
        entities: page
            .entities
            .iter()
            .map(|entity| BindingOut {
                id: entity.id.to_string(),
                handle: handle_out(entity.handle),
            })
            .collect(),
    }
}

fn handle_out(handle: EntityHandle) -> HandleOut {
    HandleOut {
        session: DecimalU64(handle.session.get()),
        index: DecimalU32(handle.index),
        generation: DecimalU32(handle.generation),
    }
}

fn entity_out(entity: &daedalus_runtime::SessionEntityState) -> EntityOut {
    EntityOut {
        stable_id: entity.stable_id.as_ref().map(ToString::to_string),
        handle: handle_out(entity.handle),
        transform: entity.transform,
        revision: DecimalU64(entity.revision.get()),
    }
}

fn command_out(
    result: &daedalus_world::CommandResult,
    resolve_tick: Option<Tick>,
) -> WireCommandResult {
    let outcome = match &result.outcome {
        daedalus_world::CommandOutcome::Applied { revision } => CommandOutcomeOut::Applied {
            revision: DecimalU64(revision.get()),
        },
        daedalus_world::CommandOutcome::Despawned => CommandOutcomeOut::Despawned,
        daedalus_world::CommandOutcome::Rejected(error) => {
            CommandOutcomeOut::Rejected(match error {
                daedalus_world::CommandError::WrongSession => CommandRejectOut::WrongSession,
                daedalus_world::CommandError::StaleEntity => CommandRejectOut::StaleEntity,
                daedalus_world::CommandError::MissingTransform => {
                    CommandRejectOut::MissingTransform
                }
                daedalus_world::CommandError::MissingRevision => CommandRejectOut::MissingRevision,
                daedalus_world::CommandError::InvalidTransform => {
                    CommandRejectOut::InvalidTransform
                }
                daedalus_world::CommandError::RevisionConflict { expected, actual } => {
                    CommandRejectOut::RevisionConflict {
                        expected: DecimalU64(expected.get()),
                        actual: DecimalU64(actual.get()),
                    }
                }
            })
        }
    };
    WireCommandResult {
        command_id: DecimalU128(result.command_id.get()),
        outcome,
        resolve_tick: resolve_tick.map(|tick| DecimalU64(tick.get())),
    }
}

fn step_out(step: &daedalus_runtime::StepReport) -> StepOut {
    StepOut {
        tick: DecimalU64(step.tick.get()),
        commands: step
            .commands
            .iter()
            .map(|result| command_out(result, None))
            .collect(),
        snapshot: match &step.snapshot {
            daedalus_runtime::SnapshotPublication::Published { sequence } => {
                SnapshotOut::Published {
                    sequence: DecimalU64(*sequence),
                }
            }
            daedalus_runtime::SnapshotPublication::SkippedAtCapacity => {
                SnapshotOut::SkippedAtCapacity
            }
            daedalus_runtime::SnapshotPublication::SkippedAtEntityLimit {
                entity_count,
                limit,
            } => SnapshotOut::SkippedAtEntityLimit {
                entity_count: *entity_count,
                limit: *limit,
            },
            daedalus_runtime::SnapshotPublication::Busy => SnapshotOut::Busy,
            daedalus_runtime::SnapshotPublication::Poisoned => SnapshotOut::Poisoned,
            daedalus_runtime::SnapshotPublication::SequenceExhausted => {
                SnapshotOut::SequenceExhausted
            }
        },
    }
}

fn status_out(status: &SessionStatus) -> StatusOut {
    StatusOut {
        session: DecimalU64(status.session.get()),
        clock: clock_name(status.clock),
        tick: DecimalU64(status.tick.get()),
        entity_count: status.entity_count,
        pending_commands: status.pending_commands,
        active_clients: status.active_clients,
        stopping: status.stopping,
    }
}

fn preparation_id_out(id: PreparationId) -> PreparationIdOut {
    PreparationIdOut {
        session: DecimalU64(id.session().get()),
        epoch: DecimalU64(id.epoch()),
    }
}

fn preparation_state_name(state: PreparationState) -> &'static str {
    match state {
        PreparationState::Idle => "idle",
        PreparationState::Pending => "pending",
        PreparationState::Preparing => "preparing",
        PreparationState::CpuPrepared => "cpu_prepared",
        PreparationState::CancelRequested => "cancel_requested",
        PreparationState::Cancelled => "cancelled",
        PreparationState::Superseded => "superseded",
        PreparationState::Failed => "failed",
        PreparationState::Stopping => "stopping",
        PreparationState::Stopped => "stopped",
    }
}

fn preparation_candidate_out(candidate: &PreparationCandidateSummary) -> PreparationCandidateOut {
    PreparationCandidateOut {
        id: preparation_id_out(candidate.id()),
        is_current: candidate.is_current(),
        source_digest: candidate.source_digest().to_owned(),
        catalog_digest: candidate.catalog_digest().to_owned(),
        source_scene_revision: DecimalU64(candidate.source_scene_revision()),
        asset_count: candidate.asset_count(),
        entity_count: candidate.entity_count(),
        memory: candidate.memory().clone(),
        metrics: candidate.metrics().clone(),
    }
}

fn preparation_failure_out(failure: &PreparationFailure) -> PreparationFailureOut {
    PreparationFailureOut {
        id: preparation_id_out(failure.id()),
        code: failure.code().to_owned(),
        message: failure.message().to_owned(),
        source_id: failure.source_id().map(str::to_owned),
        path: failure.path().map(str::to_owned),
        diagnostic_truncated: failure.diagnostic_truncated(),
    }
}

fn preparation_status_out(status: &AssetPreparationStatus) -> AssetPreparationStatusOut {
    AssetPreparationStatusOut {
        session: DecimalU64(status.session().get()),
        active_generation: DecimalU64(status.active_generation()),
        active_source_digest: status.active_source_digest().to_owned(),
        active_cooked_accounted_bytes: status.active_cooked_accounted_bytes(),
        desired: status.desired().map(preparation_id_out),
        running: status.running().map(preparation_id_out),
        pending: status.pending().map(preparation_id_out),
        state: preparation_state_name(status.state()),
        ready_candidate: status.ready_candidate().map(preparation_candidate_out),
        latest_failure: status.latest_failure().map(preparation_failure_out),
    }
}

fn preparation_command_out(command: &AssetPreparationCommandResult) -> AssetPreparationCommandOut {
    let outcome = match command.outcome() {
        AssetPreparationCommandOutcome::PrepareAccepted { candidate } => {
            AssetPreparationCommandOutcomeOut::PrepareAccepted {
                candidate: preparation_id_out(*candidate),
            }
        }
        AssetPreparationCommandOutcome::CancelAccepted { candidate } => {
            AssetPreparationCommandOutcomeOut::CancelAccepted {
                candidate: preparation_id_out(*candidate),
            }
        }
    };
    AssetPreparationCommandOut {
        command_id: DecimalU128(command.command_id().get()),
        observed_tick: DecimalU64(command.observed_tick().get()),
        outcome,
    }
}

fn preparation_result_out(result: &AssetPreparationResult) -> AssetPreparationOut {
    match result {
        AssetPreparationResult::Command(command) => {
            AssetPreparationOut::Command(preparation_command_out(command))
        }
        AssetPreparationResult::Status(status) => {
            AssetPreparationOut::Status(Box::new(preparation_status_out(status)))
        }
    }
}

fn activation_id_out(id: ActivationId) -> WireActivationId {
    WireActivationId {
        session: DecimalU64(id.session().get()),
        sequence: DecimalU64(id.sequence()),
    }
}

fn activation_state_name(state: AssetActivationState) -> &'static str {
    match state {
        AssetActivationState::Idle => "idle",
        AssetActivationState::StagePending => "stage_pending",
        AssetActivationState::Staging => "staging",
        AssetActivationState::Staged => "staged",
        AssetActivationState::CancelRequested => "cancel_requested",
        AssetActivationState::Cancelled => "cancelled",
        AssetActivationState::Superseded => "superseded",
        AssetActivationState::Committed => "committed",
        AssetActivationState::Failed => "failed",
        AssetActivationState::Stopping => "stopping",
        AssetActivationState::Stopped => "stopped",
    }
}

fn compatibility_mismatch_name(kind: AssetReplacementMismatch) -> &'static str {
    match kind {
        AssetReplacementMismatch::Manifest => "manifest",
        AssetReplacementMismatch::SceneSchema => "scene_schema",
        AssetReplacementMismatch::AssetSet => "asset_set",
        AssetReplacementMismatch::AssetKind => "asset_kind",
        AssetReplacementMismatch::AssetMetadata => "asset_metadata",
        AssetReplacementMismatch::EntitySet => "entity_set",
        AssetReplacementMismatch::EntityName => "entity_name",
        AssetReplacementMismatch::EntityParent => "entity_parent",
        AssetReplacementMismatch::ComponentSet => "component_set",
        AssetReplacementMismatch::ComponentType => "component_type",
        AssetReplacementMismatch::ComponentAssetRefs => "component_asset_refs",
        AssetReplacementMismatch::ComponentData => "component_data",
        AssetReplacementMismatch::ComparisonCapacity => "comparison_capacity",
    }
}

fn activation_status_out(status: &AssetActivationStatus) -> AssetActivationStatusOut {
    let current_activation =
        status
            .current_activation()
            .map(|activation| AssetActivationSummaryOut {
                id: activation_id_out(activation.id()),
                candidate: preparation_id_out(activation.candidate()),
                expected_active_generation: DecimalU64(activation.expected_active_generation()),
                source_digest: activation.source_digest().to_owned(),
                committed_generation: activation.committed_generation().map(DecimalU64),
            });
    let latest_failure = status.latest_failure().map(|failure| {
        let compatibility = failure
            .compatibility()
            .map(|error| CompatibilityFailureOut {
                kind: compatibility_mismatch_name(error.kind()),
                entity_id: error.entity_id().map(ToString::to_string),
                component_id: error.component_id().map(ToString::to_string),
                asset_id: error.asset_id().map(ToString::to_string),
            });
        AssetActivationFailureOut {
            id: activation_id_out(failure.id()),
            code: failure.code().to_owned(),
            message: failure.message().to_owned(),
            compatibility,
        }
    });
    let memory = status.memory();
    AssetActivationStatusOut {
        session: DecimalU64(status.session().get()),
        state: activation_state_name(status.state()),
        active_generation: DecimalU64(status.active_generation()),
        active_source_digest: status.active_source_digest().to_owned(),
        current_activation,
        latest_failure,
        memory: AssetActivationMemoryOut {
            active_cooked_accounted_bytes: memory.active_cooked_accounted_bytes(),
            candidate_cooked_accounted_bytes: memory.candidate_cooked_accounted_bytes(),
            retired_cooked_accounted_bytes: memory.retired_cooked_accounted_bytes(),
            total_cooked_accounted_bytes: memory.total_cooked_accounted_bytes(),
            max_cooked_accounted_bytes: memory.max_cooked_accounted_bytes(),
            retired_generation_count: memory.retired_generation_count(),
        },
    }
}

fn activation_command_out(
    command: &daedalus_runtime::AssetActivationCommandResult,
) -> AssetActivationCommandOut {
    let outcome = match command.outcome() {
        AssetActivationCommandOutcome::StageAccepted {
            activation,
            candidate,
        } => AssetActivationCommandOutcomeOut::StageAccepted {
            activation: activation_id_out(*activation),
            candidate: preparation_id_out(*candidate),
        },
        AssetActivationCommandOutcome::CommitApplied {
            activation,
            generation,
        } => AssetActivationCommandOutcomeOut::CommitApplied {
            activation: activation_id_out(*activation),
            generation: DecimalU64(*generation),
        },
        AssetActivationCommandOutcome::CancelAccepted { activation } => {
            AssetActivationCommandOutcomeOut::CancelAccepted {
                activation: activation_id_out(*activation),
            }
        }
    };
    AssetActivationCommandOut {
        command_id: DecimalU128(command.command_id().get()),
        observed_tick: DecimalU64(command.observed_tick().get()),
        outcome,
    }
}

fn asset_activation_result_out(result: &AssetActivationRequestResult) -> AssetActivationOut {
    match result {
        AssetActivationRequestResult::Status(status) => {
            AssetActivationOut::Status(Box::new(activation_status_out(status)))
        }
        AssetActivationRequestResult::Command(command) => {
            AssetActivationOut::Command(activation_command_out(command))
        }
    }
}

fn request_error(error: &SessionRequestError) -> ProtocolError {
    use SessionRequestError as E;
    let mut result = match error {
        E::UnknownStableId(_) => ProtocolError::new("unknown_stable_id", error.to_string()),
        E::MissingBinding(_) => ProtocolError::new("missing_binding", error.to_string()),
        E::WorldRead(error) => ProtocolError::new(
            match error {
                daedalus_world::WorldReadError::WrongSession => "wrong_session",
                daedalus_world::WorldReadError::StaleEntity => "stale_entity",
                daedalus_world::WorldReadError::MissingTransform => "missing_transform",
                daedalus_world::WorldReadError::MissingRevision => "missing_revision",
            },
            error.to_string(),
        ),
        E::CommandRejected(message) if message.contains("expected revision") => {
            ProtocolError::new("revision_conflict", message.clone())
        }
        E::CommandRejected(_) => ProtocolError::new("command_rejected", error.to_string()),
        E::EntityLimit => ProtocolError::new("entity_limit", error.to_string()),
        E::SpawnFailed(_) => ProtocolError::new("spawn_failed", error.to_string()),
        E::ResultTooLarge => ProtocolError::new("result_too_large", error.to_string()),
        E::StepFailed { .. } => ProtocolError::new("step_failed", error.to_string()),
        E::StoppedBeforeAuthority => {
            ProtocolError::new("stopped_before_authority", error.to_string())
        }
        E::StoppedNotApplied { command_id } => {
            let mut error = ProtocolError::new("stopped_not_applied", error.to_string());
            error.details = Some(ErrorDetails::CommandId {
                command_id: DecimalU128(command_id.get()),
            });
            error
        }
        E::OutcomeUnknown { command_id } => {
            let mut error = ProtocolError::new("outcome_unknown", error.to_string());
            error.details = Some(ErrorDetails::CommandId {
                command_id: DecimalU128(command_id.get()),
            });
            error
        }
        E::ActorFailed(_) => ProtocolError::new("actor_failed", error.to_string()),
        E::ReplayWindowExpired {
            command_id,
            high_water,
        } => {
            let mut error = ProtocolError::new("replay_window_expired", error.to_string());
            error.details = Some(ErrorDetails::ReplayWindowExpired {
                command_id: DecimalU128(*command_id),
                high_water: DecimalU128(*high_water),
            });
            error
        }
        E::UnknownPendingCommand(_) => {
            ProtocolError::new("unknown_pending_command", error.to_string())
        }
        E::AssetPreparationUnavailable => {
            ProtocolError::new("asset_preparation_unavailable", error.to_string())
        }
        E::AssetActivationUnavailable => {
            ProtocolError::new("asset_activation_unavailable", error.to_string())
        }
        E::ActivationBusy => ProtocolError::retryable("busy", error.to_string()),
        E::InvalidActivation => ProtocolError::new("invalid_activation", error.to_string()),
        E::ActivationExpired => ProtocolError::new("activation_expired", error.to_string()),
        E::ActivationNotStaged => ProtocolError::new("activation_not_staged", error.to_string()),
        E::ActiveGenerationMismatch => {
            ProtocolError::new("active_generation_mismatch", error.to_string())
        }
        E::ActivationSequenceExhausted => {
            ProtocolError::new("activation_sequence_exhausted", error.to_string())
        }
        E::ProjectGenerationExhausted => {
            ProtocolError::new("project_generation_exhausted", error.to_string())
        }
        E::ActivationCapacity => ProtocolError::new("activation_capacity", error.to_string()),
        E::PreparationBusy => ProtocolError::retryable("busy", error.to_string()),
        E::PreparationCapacity => ProtocolError::new("preparation_capacity", error.to_string()),
        E::PreparationStopped => ProtocolError::new("preparation_stopped", error.to_string()),
        E::InvalidCandidate => ProtocolError::new("invalid_candidate", error.to_string()),
        E::CandidateExpired => ProtocolError::new("candidate_expired", error.to_string()),
        E::CandidateNotActive => ProtocolError::new("candidate_not_active", error.to_string()),
        E::PreparationEpochExhausted => {
            ProtocolError::new("preparation_epoch_exhausted", error.to_string())
        }
        #[cfg(feature = "audio")]
        E::AudioUnavailable => ProtocolError::new("audio_unavailable", error.to_string()),
        #[cfg(feature = "audio")]
        E::Audio(audio_error) => ProtocolError::new(audio_error.code(), audio_error.message()),
        E::ReplayStorageUnavailable => {
            ProtocolError::retryable("replay_storage_unavailable", error.to_string())
        }
    };
    if let E::CommandRejected(message) = error
        && let Some((expected, actual)) = parse_revision_conflict(message)
    {
        result.details = Some(ErrorDetails::RevisionConflict {
            expected: DecimalU64(expected),
            actual: DecimalU64(actual),
        });
    }
    result
}

fn parse_revision_conflict(message: &str) -> Option<(u64, u64)> {
    let rest = message.strip_prefix("expected revision ")?;
    let (expected, rest) = rest.split_once(", found ")?;
    Some((expected.parse().ok()?, rest.parse().ok()?))
}

#[cfg(test)]
mod tests;
