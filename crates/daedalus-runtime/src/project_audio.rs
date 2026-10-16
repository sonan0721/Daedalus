//! Project-owned static PCM bank and audio operations for the common session actor.
//!
//! The bank is created once from an immutable [`PreparedProject`] and owns converted Kira clips
//! for the lifetime of the actor. This module does not create a second actor or expose PCM through
//! request results.

use std::mem::size_of;

use daedalus_audio::{
    AudioConfig, AudioError, AudioService, OfflineBackend, PCM_FRAME_BYTES, PcmClip, VoiceHandle,
};
use daedalus_content::{
    assets::{ImportedAsset, LeafCookKey, PreparedAssetCatalog},
    project::PreparedProject,
    scene::StableId,
};
use daedalus_core::{CommandId, SessionId, Tick};
use thiserror::Error;

#[cfg(feature = "native-audio")]
use daedalus_audio::NativeAudioService;

const HARD_MAX_AUDIO_ALIASES: usize = 256;
const HARD_MAX_UNIQUE_AUDIO_CLIPS: usize = 256;
const HARD_MAX_FRAME_RESIDENT_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_CONVERSION_SCRATCH_BYTES: u64 = 128 * 1024 * 1024;
const HARD_MAX_AUDIO_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ACTOR_OFFLINE_FRAMES: usize = 4_096;
const CONSERVATIVE_PAGE_ROW_JSON_BYTES: usize = 512;

/// Bank resources with independent admission limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioBankResource {
    Aliases,
    UniqueClips,
    FrameResidentBytes,
    ConversionScratchBytes,
    MetadataBytes,
}

impl AudioBankResource {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Aliases => "aliases",
            Self::UniqueClips => "unique_clips",
            Self::FrameResidentBytes => "frame_resident_bytes",
            Self::ConversionScratchBytes => "conversion_scratch_bytes",
            Self::MetadataBytes => "metadata_bytes",
        }
    }
}

/// Private-configured limits for one actor's immutable imported audio bank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioBankLimits {
    max_aliases: usize,
    max_unique_clips: usize,
    max_resident_frame_bytes: u64,
    max_conversion_scratch_bytes: u64,
    max_metadata_bytes: u64,
}

impl Default for AudioBankLimits {
    fn default() -> Self {
        Self {
            max_aliases: HARD_MAX_AUDIO_ALIASES,
            max_unique_clips: HARD_MAX_UNIQUE_AUDIO_CLIPS,
            max_resident_frame_bytes: HARD_MAX_FRAME_RESIDENT_BYTES,
            max_conversion_scratch_bytes: HARD_MAX_CONVERSION_SCRATCH_BYTES,
            max_metadata_bytes: HARD_MAX_AUDIO_METADATA_BYTES,
        }
    }
}

impl AudioBankLimits {
    /// Lower the number of asset IDs that can alias bank clips.
    pub fn with_max_aliases(mut self, limit: usize) -> Self {
        self.max_aliases = limit;
        self
    }

    /// Lower the number of unique cooked clips retained by this bank.
    pub fn with_max_unique_clips(mut self, limit: usize) -> Self {
        self.max_unique_clips = limit;
        self
    }

    /// Lower retained converted Kira frame payload bytes.
    pub fn with_max_resident_frame_bytes(mut self, limit: u64) -> Self {
        self.max_resident_frame_bytes = limit;
        self
    }

    /// Lower the extra peak backing permitted while one clip is converted.
    pub fn with_max_conversion_scratch_bytes(mut self, limit: u64) -> Self {
        self.max_conversion_scratch_bytes = limit;
        self
    }

    /// Lower logical bank and tracked-voice metadata bytes.
    pub fn with_max_metadata_bytes(mut self, limit: u64) -> Self {
        self.max_metadata_bytes = limit;
        self
    }

    pub const fn max_aliases(self) -> usize {
        self.max_aliases
    }

    pub const fn max_unique_clips(self) -> usize {
        self.max_unique_clips
    }

    pub const fn max_resident_frame_bytes(self) -> u64 {
        self.max_resident_frame_bytes
    }

    pub const fn max_conversion_scratch_bytes(self) -> u64 {
        self.max_conversion_scratch_bytes
    }

    pub const fn max_metadata_bytes(self) -> u64 {
        self.max_metadata_bytes
    }

    fn validate(self) -> Result<(), SessionAudioStartError> {
        if self.max_aliases > HARD_MAX_AUDIO_ALIASES
            || self.max_unique_clips > HARD_MAX_UNIQUE_AUDIO_CLIPS
            || self.max_resident_frame_bytes > HARD_MAX_FRAME_RESIDENT_BYTES
            || self.max_conversion_scratch_bytes > HARD_MAX_CONVERSION_SCRATCH_BYTES
            || self.max_metadata_bytes > HARD_MAX_AUDIO_METADATA_BYTES
        {
            return Err(SessionAudioStartError::InvalidOptions);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendSelection {
    Offline { sample_rate: u32 },
    Native,
}

/// Explicit startup choice for a project audio actor sidecar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionAudioOptions {
    backend: BackendSelection,
    max_voices: usize,
    internal_buffer_frames: usize,
    bank_limits: AudioBankLimits,
}

impl SessionAudioOptions {
    /// Request a real Kira offline mixer at the selected sample rate.
    pub fn offline(sample_rate: u32) -> Self {
        Self {
            backend: BackendSelection::Offline { sample_rate },
            max_voices: AudioConfig::default().max_voices,
            internal_buffer_frames: AudioConfig::default().internal_buffer_frames,
            bank_limits: AudioBankLimits::default(),
        }
    }

    /// Request the real native output backend; startup never falls back to offline playback.
    pub fn native() -> Self {
        Self {
            backend: BackendSelection::Native,
            max_voices: AudioConfig::default().max_voices,
            internal_buffer_frames: AudioConfig::default().internal_buffer_frames,
            bank_limits: AudioBankLimits::default(),
        }
    }

    /// Lower the active voice cap from the audio-service hard ceiling.
    pub fn with_max_voices(mut self, max_voices: usize) -> Self {
        self.max_voices = max_voices;
        self
    }

    /// Lower the Kira mixer block size from the audio-service hard ceiling.
    pub fn with_internal_buffer_frames(mut self, frames: usize) -> Self {
        self.internal_buffer_frames = frames;
        self
    }

    /// Apply explicit lower bank limits.
    pub fn with_bank_limits(mut self, limits: AudioBankLimits) -> Self {
        self.bank_limits = limits;
        self
    }

    pub const fn bank_limits(self) -> AudioBankLimits {
        self.bank_limits
    }

    pub const fn max_voices(self) -> usize {
        self.max_voices
    }

    pub const fn internal_buffer_frames(self) -> usize {
        self.internal_buffer_frames
    }

    fn validate(self) -> Result<(), SessionAudioStartError> {
        AudioConfig {
            max_voices: self.max_voices,
            internal_buffer_frames: self.internal_buffer_frames,
        }
        .validate()
        .map_err(|_| SessionAudioStartError::InvalidOptions)?;
        if let BackendSelection::Offline { sample_rate } = self.backend
            && !(8_000..=384_000).contains(&sample_rate)
        {
            return Err(SessionAudioStartError::InvalidOptions);
        }
        self.bank_limits.validate()
    }
}

/// Process-local voice identity scoped to one engine session and never reused within it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionAudioVoiceId {
    session: SessionId,
    sequence: u64,
}

impl SessionAudioVoiceId {
    /// Reconstruct an ID from its protocol-safe parts. The actor validates issuance before use.
    pub const fn from_parts(session: SessionId, sequence: u64) -> Self {
        Self { session, sequence }
    }

    pub const fn session(self) -> SessionId {
        self.session
    }

    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Typed inspect or mutation request accepted by the common project actor.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionAudioOperation {
    Status,
    Clips {
        offset: usize,
        limit: Option<usize>,
    },
    Voice {
        voice: SessionAudioVoiceId,
    },
    Voices {
        offset: usize,
        limit: Option<usize>,
    },
    Command {
        command_id: Option<CommandId>,
        mutation: SessionAudioMutation,
    },
}

/// One bounded audio-side effect. `RenderOffline` advances only the audio clock.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionAudioMutation {
    Play {
        clip: StableId,
        volume: f32,
        pan: f32,
        looping: bool,
    },
    Stop {
        voice: SessionAudioVoiceId,
    },
    SetVolume {
        voice: SessionAudioVoiceId,
        volume: f32,
    },
    SetPan {
        voice: SessionAudioVoiceId,
        pan: f32,
    },
    RenderOffline {
        frames: usize,
    },
}

/// Backend selection actually created by the actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAudioBackend {
    Offline { sample_rate: u32 },
    Native,
}

/// Bounded audio counters and the selected backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionAudioStatus {
    session: SessionId,
    backend: SessionAudioBackend,
    alias_count: usize,
    unique_clip_count: usize,
    converted_clip_count: usize,
    converted_frame_bytes: u64,
    active_voice_count: usize,
    offline_progressed_frames: u64,
}

impl SessionAudioStatus {
    pub const fn session(&self) -> SessionId {
        self.session
    }
    pub const fn backend(&self) -> SessionAudioBackend {
        self.backend
    }
    pub const fn alias_count(&self) -> usize {
        self.alias_count
    }
    pub const fn unique_clip_count(&self) -> usize {
        self.unique_clip_count
    }
    pub const fn converted_clip_count(&self) -> usize {
        self.converted_clip_count
    }
    pub const fn converted_frame_bytes(&self) -> u64 {
        self.converted_frame_bytes
    }
    pub const fn active_voice_count(&self) -> usize {
        self.active_voice_count
    }
    pub const fn offline_progressed_frames(&self) -> u64 {
        self.offline_progressed_frames
    }
}

/// Summary for one asset-ID alias in the immutable bank.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAudioClipInfo {
    asset_id: StableId,
    key: LeafCookKey,
    sample_rate: u32,
    channels: u16,
    frames: u64,
}

impl SessionAudioClipInfo {
    pub fn asset_id(&self) -> &StableId {
        &self.asset_id
    }
    pub const fn key(&self) -> LeafCookKey {
        self.key
    }
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub const fn channels(&self) -> u16 {
        self.channels
    }
    pub const fn frames(&self) -> u64 {
        self.frames
    }
}

/// One bounded page of clip aliases.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionAudioClipPage {
    observed_tick: Tick,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    clips: Vec<SessionAudioClipInfo>,
}

impl SessionAudioClipPage {
    pub const fn observed_tick(&self) -> Tick {
        self.observed_tick
    }
    pub const fn total(&self) -> usize {
        self.total
    }
    pub const fn offset(&self) -> usize {
        self.offset
    }
    pub const fn limit(&self) -> usize {
        self.limit
    }
    pub const fn next_offset(&self) -> Option<usize> {
        self.next_offset
    }
    pub fn clips(&self) -> &[SessionAudioClipInfo] {
        &self.clips
    }
}

/// Runtime-observed voice state. Requested volume and pan are not callback-applied DSP values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAudioVoiceState {
    Playing,
    StopRequested,
    Ended,
}

/// Current voice state. Ended voices have no retained clip/control history.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionAudioVoiceInfo {
    voice: SessionAudioVoiceId,
    state: SessionAudioVoiceState,
    clip_id: Option<StableId>,
    requested_volume: Option<f32>,
    requested_pan: Option<f32>,
    looping: Option<bool>,
}

impl SessionAudioVoiceInfo {
    pub const fn voice(&self) -> SessionAudioVoiceId {
        self.voice
    }
    pub const fn state(&self) -> SessionAudioVoiceState {
        self.state
    }
    pub fn clip_id(&self) -> Option<&StableId> {
        self.clip_id.as_ref()
    }
    pub const fn requested_volume(&self) -> Option<f32> {
        self.requested_volume
    }
    pub const fn requested_pan(&self) -> Option<f32> {
        self.requested_pan
    }
    pub const fn looping(&self) -> Option<bool> {
        self.looping
    }
}

/// One bounded page of currently tracked (not historical) voices.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionAudioVoicePage {
    observed_tick: Tick,
    total: usize,
    offset: usize,
    limit: usize,
    next_offset: Option<usize>,
    voices: Vec<SessionAudioVoiceInfo>,
}

impl SessionAudioVoicePage {
    pub const fn observed_tick(&self) -> Tick {
        self.observed_tick
    }
    pub const fn total(&self) -> usize {
        self.total
    }
    pub const fn offset(&self) -> usize {
        self.offset
    }
    pub const fn limit(&self) -> usize {
        self.limit
    }
    pub const fn next_offset(&self) -> Option<usize> {
        self.next_offset
    }
    pub fn voices(&self) -> &[SessionAudioVoiceInfo] {
        &self.voices
    }
}

/// Stable machine-readable audio operation failure. Variants carry no unbounded backend text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SessionAudioError {
    #[error("audio operation has invalid parameters")]
    InvalidInput,
    #[error("audio operation references an unknown clip")]
    UnknownClip,
    #[error("voice identifier is malformed")]
    InvalidVoiceId,
    #[error("voice identifier belongs to another session")]
    WrongVoiceSession,
    #[error("voice identifier has not been issued")]
    VoiceNotIssued,
    #[error("voice has ended")]
    VoiceEnded,
    #[error("audio voice capacity is reached")]
    VoiceCapacityReached,
    #[error("audio operation is unsupported by the selected backend")]
    UnsupportedBackendOperation,
    #[error("audio service allocation failed")]
    AllocationFailed,
    #[error("audio service failed")]
    BackendFailure,
    #[error("audio frame counter overflowed")]
    CounterOverflow,
    #[error("offline mixer produced non-finite output")]
    NonFiniteOutput,
    #[error("audio result exceeds the configured bounded result size")]
    ResultTooLarge,
    #[error("audio query received a mutation operation")]
    WrongOperation,
}

impl SessionAudioError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::UnknownClip => "unknown_clip",
            Self::InvalidVoiceId => "invalid_voice_id",
            Self::WrongVoiceSession => "wrong_voice_session",
            Self::VoiceNotIssued => "voice_not_issued",
            Self::VoiceEnded => "voice_ended",
            Self::VoiceCapacityReached => "voice_capacity_reached",
            Self::UnsupportedBackendOperation => "unsupported_backend_operation",
            Self::AllocationFailed => "allocation_failed",
            Self::BackendFailure => "backend_failure",
            Self::CounterOverflow => "counter_overflow",
            Self::NonFiniteOutput => "non_finite_output",
            Self::ResultTooLarge => "result_too_large",
            Self::WrongOperation => "wrong_operation",
        }
    }

    /// Stable bounded description suitable for protocol output.
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidInput => "audio operation has invalid parameters",
            Self::UnknownClip => "audio operation references an unknown clip",
            Self::InvalidVoiceId => "voice identifier is malformed",
            Self::WrongVoiceSession => "voice identifier belongs to another session",
            Self::VoiceNotIssued => "voice identifier has not been issued",
            Self::VoiceEnded => "voice has ended",
            Self::VoiceCapacityReached => "audio voice capacity is reached",
            Self::UnsupportedBackendOperation => {
                "audio operation is unsupported by the selected backend"
            }
            Self::AllocationFailed => "audio service allocation failed",
            Self::BackendFailure => "audio service failed",
            Self::CounterOverflow => "audio frame counter overflowed",
            Self::NonFiniteOutput => "offline mixer produced non-finite output",
            Self::ResultTooLarge => "audio result exceeds the configured bounded result size",
            Self::WrongOperation => "audio query received a mutation operation",
        }
    }
}

/// Bounded typed failure returned by accepted startup options.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SessionAudioStartError {
    #[error("audio startup options are invalid or exceed hard limits")]
    InvalidOptions,
    #[error("native audio support was not compiled in")]
    NativeAudioFeatureUnavailable,
    #[error("no audio bank or playback backend is available")]
    BackendUnavailable,
    #[error("the selected audio backend could not be initialized")]
    BackendInitializationFailed,
    #[error("audio clip bank exceeds a configured resource limit")]
    BudgetExceeded {
        resource: AudioBankResource,
        required: u64,
        limit: u64,
    },
    #[error("audio bank allocation failed")]
    AllocationFailed,
    #[error("an imported clip violates playback sample admission")]
    PlaybackClipRejected,
    #[error("audio bank size arithmetic overflowed")]
    ArithmeticOverflow,
}

impl SessionAudioStartError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidOptions => "invalid_options",
            Self::NativeAudioFeatureUnavailable => "native_audio_feature_unavailable",
            Self::BackendUnavailable => "backend_unavailable",
            Self::BackendInitializationFailed => "backend_initialization_failed",
            Self::BudgetExceeded { .. } => "budget_exceeded",
            Self::AllocationFailed => "allocation_failed",
            Self::PlaybackClipRejected => "playback_clip_rejected",
            Self::ArithmeticOverflow => "arithmetic_overflow",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidOptions => "audio startup options are invalid or exceed hard limits",
            Self::NativeAudioFeatureUnavailable => "native audio support was not compiled in",
            Self::BackendUnavailable => "no audio bank or playback backend is available",
            Self::BackendInitializationFailed => {
                "the selected audio backend could not be initialized"
            }
            Self::BudgetExceeded { .. } => "audio clip bank exceeds a configured resource limit",
            Self::AllocationFailed => "audio bank allocation failed",
            Self::PlaybackClipRejected => "an imported clip violates playback sample admission",
            Self::ArithmeticOverflow => "audio bank size arithmetic overflowed",
        }
    }
}

/// Compact success value cached in the shared command replay ledger.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionAudioCommandOutcome {
    Played {
        voice: SessionAudioVoiceId,
    },
    StopRequested {
        voice: SessionAudioVoiceId,
    },
    VolumeSet {
        voice: SessionAudioVoiceId,
        requested_volume: f32,
    },
    PanSet {
        voice: SessionAudioVoiceId,
        requested_pan: f32,
    },
    Offline(OfflineAudioSummary),
}

/// Compact terminal result for an accepted audio command, including replayable typed failures.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionAudioCommandResult {
    command_id: CommandId,
    observed_tick: Tick,
    outcome: Result<SessionAudioCommandOutcome, SessionAudioError>,
}

impl SessionAudioCommandResult {
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }
    pub const fn observed_tick(&self) -> Tick {
        self.observed_tick
    }
    pub fn outcome(&self) -> &Result<SessionAudioCommandOutcome, SessionAudioError> {
        &self.outcome
    }
}

/// Summary of actual offline mixer output; the PCM array is never retained in a ticket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OfflineAudioSummary {
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
    cumulative_frames: u64,
}

impl OfflineAudioSummary {
    pub const fn frames(self) -> usize {
        self.frames
    }
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }
    pub const fn channels(self) -> u16 {
        self.channels
    }
    pub const fn peak(self) -> f32 {
        self.peak
    }
    pub const fn rms(self) -> f32 {
        self.rms
    }
    pub const fn left_peak(self) -> f32 {
        self.left_peak
    }
    pub const fn left_rms(self) -> f32 {
        self.left_rms
    }
    pub const fn right_peak(self) -> f32 {
        self.right_peak
    }
    pub const fn right_rms(self) -> f32 {
        self.right_rms
    }
    pub const fn nonzero_samples(self) -> usize {
        self.nonzero_samples
    }
    pub const fn cumulative_frames(self) -> u64 {
        self.cumulative_frames
    }
}

/// Typed result for audio inspection and commands.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionAudioResult {
    Status(SessionAudioStatus),
    Clips(SessionAudioClipPage),
    Voice(SessionAudioVoiceInfo),
    Voices(SessionAudioVoicePage),
    Command(SessionAudioCommandResult),
}

impl SessionAudioResult {
    pub fn as_status(&self) -> Option<&SessionAudioStatus> {
        if let Self::Status(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_clips(&self) -> Option<&SessionAudioClipPage> {
        if let Self::Clips(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_voice(&self) -> Option<&SessionAudioVoiceInfo> {
        if let Self::Voice(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_voices(&self) -> Option<&SessionAudioVoicePage> {
        if let Self::Voices(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn as_command(&self) -> Option<&SessionAudioCommandResult> {
        if let Self::Command(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

impl SessionAudioCommandOutcome {
    pub const fn voice(&self) -> Option<SessionAudioVoiceId> {
        match self {
            Self::Played { voice } | Self::StopRequested { voice } => Some(*voice),
            Self::VolumeSet { voice, .. } | Self::PanSet { voice, .. } => Some(*voice),
            Self::Offline(_) => None,
        }
    }
}

struct BankAlias {
    id: StableId,
    clip_index: usize,
}

struct BankClip {
    key: LeafCookKey,
    sample_rate: u32,
    channels: u16,
    frames: u64,
    pcm: PcmClip,
}

struct ProjectAudioBank {
    aliases: Vec<BankAlias>,
    clips: Vec<BankClip>,
    converted_frame_bytes: u64,
}

impl ProjectAudioBank {
    fn prepare(
        project: &PreparedProject,
        limits: AudioBankLimits,
        max_voices: usize,
        backend: BackendSelection,
    ) -> Result<Self, SessionAudioStartError> {
        let catalog = project.asset_catalog();
        let plan = BankPlan::preflight(catalog, limits, max_voices, backend)?;
        let mut aliases = Vec::new();
        aliases
            .try_reserve_exact(plan.aliases)
            .map_err(|_| SessionAudioStartError::AllocationFailed)?;
        let mut clips: Vec<BankClip> = Vec::new();
        clips
            .try_reserve_exact(plan.unique_clips)
            .map_err(|_| SessionAudioStartError::AllocationFailed)?;

        if let Some(catalog) = catalog {
            for (id, asset) in catalog.assets() {
                let ImportedAsset::Audio(audio) = asset else {
                    continue;
                };
                let clip_index =
                    if let Some(index) = clips.iter().position(|clip| clip.key == audio.key()) {
                        index
                    } else {
                        let pcm = PcmClip::from_interleaved_slice(
                            audio.sample_rate(),
                            audio.channels(),
                            audio.samples(),
                        )
                        .map_err(map_start_audio_error)?;
                        let index = clips.len();
                        clips.push(BankClip {
                            key: audio.key(),
                            sample_rate: audio.sample_rate(),
                            channels: audio.channels(),
                            frames: audio.frames(),
                            pcm,
                        });
                        index
                    };
                aliases.push(BankAlias {
                    id: try_clone_stable_id(id)?,
                    clip_index,
                });
            }
        }

        debug_assert_eq!(aliases.len(), plan.aliases);
        debug_assert_eq!(clips.len(), plan.unique_clips);
        Ok(Self {
            aliases,
            clips,
            converted_frame_bytes: plan.resident_frame_bytes,
        })
    }

    fn alias_index(&self, id: &StableId) -> Option<usize> {
        self.aliases.binary_search_by(|alias| alias.id.cmp(id)).ok()
    }

    fn clip_info(&self, alias_index: usize) -> Result<SessionAudioClipInfo, SessionAudioError> {
        let alias = &self.aliases[alias_index];
        let clip = &self.clips[alias.clip_index];
        Ok(SessionAudioClipInfo {
            asset_id: try_clone_stable_id_operation(&alias.id)?,
            key: clip.key,
            sample_rate: clip.sample_rate,
            channels: clip.channels,
            frames: clip.frames,
        })
    }
}

struct BankPlan {
    aliases: usize,
    unique_clips: usize,
    resident_frame_bytes: u64,
}

impl BankPlan {
    fn preflight(
        catalog: Option<&PreparedAssetCatalog>,
        limits: AudioBankLimits,
        max_voices: usize,
        backend: BackendSelection,
    ) -> Result<Self, SessionAudioStartError> {
        let mut aliases = 0_usize;
        let mut unique_clips = 0_usize;
        let mut resident_frame_bytes = 0_u64;
        let mut conversion_scratch_bytes = 0_u64;
        let backend_owner_bytes = backend_owner_metadata_bytes(backend)?;
        let mut metadata_bytes = checked_size_of::<ProjectAudioBank>()?
            .checked_add(checked_size_of::<ProjectAudioRuntime>()?)
            .and_then(|value| value.checked_add(backend_owner_bytes))
            .and_then(|value| {
                value.checked_add(
                    u64::try_from(max_voices)
                        .ok()?
                        .checked_mul(checked_size_of::<TrackedVoice>().ok()?)?,
                )
            })
            .ok_or(SessionAudioStartError::ArithmeticOverflow)?;

        if let Some(catalog) = catalog {
            for (id, asset) in catalog.assets() {
                let ImportedAsset::Audio(audio) = asset else {
                    continue;
                };
                aliases = aliases
                    .checked_add(1)
                    .ok_or(SessionAudioStartError::ArithmeticOverflow)?;
                if aliases > limits.max_aliases {
                    return Err(budget_error(
                        AudioBankResource::Aliases,
                        aliases,
                        limits.max_aliases,
                    ));
                }
                metadata_bytes = metadata_bytes
                    .checked_add(size_of::<BankAlias>() as u64)
                    .and_then(|value| value.checked_add(id.as_str().len() as u64))
                    .ok_or(SessionAudioStartError::ArithmeticOverflow)?;

                if is_first_key_occurrence(catalog, id, audio.key()) {
                    unique_clips = unique_clips
                        .checked_add(1)
                        .ok_or(SessionAudioStartError::ArithmeticOverflow)?;
                    if unique_clips > limits.max_unique_clips {
                        return Err(budget_error(
                            AudioBankResource::UniqueClips,
                            unique_clips,
                            limits.max_unique_clips,
                        ));
                    }
                    let frame_bytes = audio
                        .frames()
                        .checked_mul(PCM_FRAME_BYTES as u64)
                        .ok_or(SessionAudioStartError::ArithmeticOverflow)?;
                    resident_frame_bytes = resident_frame_bytes
                        .checked_add(frame_bytes)
                        .ok_or(SessionAudioStartError::ArithmeticOverflow)?;
                    conversion_scratch_bytes = conversion_scratch_bytes.max(frame_bytes);
                    metadata_bytes = metadata_bytes
                        .checked_add(size_of::<BankClip>() as u64)
                        .and_then(|value| value.checked_add((size_of::<usize>() * 3) as u64))
                        .ok_or(SessionAudioStartError::ArithmeticOverflow)?;
                }
            }
        }

        check_budget(
            AudioBankResource::FrameResidentBytes,
            resident_frame_bytes,
            limits.max_resident_frame_bytes,
        )?;
        check_budget(
            AudioBankResource::ConversionScratchBytes,
            conversion_scratch_bytes,
            limits.max_conversion_scratch_bytes,
        )?;
        check_budget(
            AudioBankResource::MetadataBytes,
            metadata_bytes,
            limits.max_metadata_bytes,
        )?;
        Ok(Self {
            aliases,
            unique_clips,
            resident_frame_bytes,
        })
    }
}

fn is_first_key_occurrence(
    catalog: &PreparedAssetCatalog,
    current_id: &StableId,
    key: LeafCookKey,
) -> bool {
    for (id, asset) in catalog.assets() {
        if id >= current_id {
            break;
        }
        if matches!(asset, ImportedAsset::Audio(audio) if audio.key() == key) {
            return false;
        }
    }
    true
}

fn checked_size_of<T>() -> Result<u64, SessionAudioStartError> {
    u64::try_from(size_of::<T>()).map_err(|_| SessionAudioStartError::ArithmeticOverflow)
}

fn budget_error(
    resource: AudioBankResource,
    required: usize,
    limit: usize,
) -> SessionAudioStartError {
    SessionAudioStartError::BudgetExceeded {
        resource,
        required: required as u64,
        limit: limit as u64,
    }
}

fn check_budget(
    resource: AudioBankResource,
    required: u64,
    limit: u64,
) -> Result<(), SessionAudioStartError> {
    if required > limit {
        return Err(SessionAudioStartError::BudgetExceeded {
            resource,
            required,
            limit,
        });
    }
    Ok(())
}

fn try_clone_stable_id(id: &StableId) -> Result<StableId, SessionAudioStartError> {
    let mut value = String::new();
    value
        .try_reserve_exact(id.as_str().len())
        .map_err(|_| SessionAudioStartError::AllocationFailed)?;
    value.push_str(id.as_str());
    StableId::new(value).map_err(|_| SessionAudioStartError::InvalidOptions)
}

fn try_clone_stable_id_operation(id: &StableId) -> Result<StableId, SessionAudioError> {
    let mut value = String::new();
    value
        .try_reserve_exact(id.as_str().len())
        .map_err(|_| SessionAudioError::AllocationFailed)?;
    value.push_str(id.as_str());
    StableId::new(value).map_err(|_| SessionAudioError::BackendFailure)
}

fn map_start_audio_error(error: AudioError) -> SessionAudioStartError {
    match error {
        AudioError::InvalidInput(_) => SessionAudioStartError::PlaybackClipRejected,
        AudioError::AllocationFailed => SessionAudioStartError::AllocationFailed,
        AudioError::BackendUnavailable => SessionAudioStartError::BackendUnavailable,
        AudioError::VoiceCapacityReached | AudioError::StaleVoiceHandle => {
            SessionAudioStartError::BackendInitializationFailed
        }
        AudioError::Backend(_) | AudioError::DeviceProbe(_) => {
            SessionAudioStartError::BackendInitializationFailed
        }
    }
}

fn map_operation_audio_error(error: AudioError) -> SessionAudioError {
    match error {
        AudioError::InvalidInput(_) => SessionAudioError::InvalidInput,
        AudioError::VoiceCapacityReached => SessionAudioError::VoiceCapacityReached,
        AudioError::StaleVoiceHandle => SessionAudioError::VoiceEnded,
        AudioError::AllocationFailed => SessionAudioError::AllocationFailed,
        AudioError::BackendUnavailable | AudioError::Backend(_) | AudioError::DeviceProbe(_) => {
            SessionAudioError::BackendFailure
        }
    }
}

enum RuntimeAudioBackend {
    Offline {
        service: Box<AudioService<OfflineBackend>>,
        sample_rate: u32,
    },
    #[cfg(feature = "native-audio")]
    Native(Box<NativeAudioService>),
}

fn backend_owner_metadata_bytes(
    selection: BackendSelection,
) -> Result<u64, SessionAudioStartError> {
    let bytes = match selection {
        BackendSelection::Offline { .. } => size_of::<AudioService<OfflineBackend>>(),
        BackendSelection::Native => {
            #[cfg(feature = "native-audio")]
            {
                size_of::<NativeAudioService>()
            }
            #[cfg(not(feature = "native-audio"))]
            {
                return Err(SessionAudioStartError::NativeAudioFeatureUnavailable);
            }
        }
    };
    u64::try_from(bytes).map_err(|_| SessionAudioStartError::ArithmeticOverflow)
}

impl RuntimeAudioBackend {
    fn start(options: SessionAudioOptions) -> Result<Self, SessionAudioStartError> {
        let config = AudioConfig {
            max_voices: options.max_voices,
            internal_buffer_frames: options.internal_buffer_frames,
        };
        match options.backend {
            BackendSelection::Offline { sample_rate } => {
                AudioService::<OfflineBackend>::offline(config, sample_rate)
                    .map(|service| Self::Offline {
                        service: Box::new(service),
                        sample_rate,
                    })
                    .map_err(map_start_audio_error)
            }
            BackendSelection::Native => {
                #[cfg(feature = "native-audio")]
                {
                    NativeAudioService::native(config)
                        .map(|service| Self::Native(Box::new(service)))
                        .map_err(map_start_audio_error)
                }
                #[cfg(not(feature = "native-audio"))]
                {
                    let _ = config;
                    Err(SessionAudioStartError::NativeAudioFeatureUnavailable)
                }
            }
        }
    }

    fn selected(&self) -> SessionAudioBackend {
        match self {
            Self::Offline { sample_rate, .. } => SessionAudioBackend::Offline {
                sample_rate: *sample_rate,
            },
            #[cfg(feature = "native-audio")]
            Self::Native(_) => SessionAudioBackend::Native,
        }
    }

    fn play(
        &mut self,
        clip: &PcmClip,
        volume: f32,
        pan: f32,
        looping: bool,
    ) -> Result<VoiceHandle, AudioError> {
        match self {
            Self::Offline { service, .. } => service.play_with_looping(clip, volume, pan, looping),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.play_with_looping(clip, volume, pan, looping),
        }
    }

    fn stop(&mut self, voice: VoiceHandle) -> Result<(), AudioError> {
        match self {
            Self::Offline { service, .. } => service.stop(voice),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.stop(voice),
        }
    }

    fn set_volume(&mut self, voice: VoiceHandle, volume: f32) -> Result<(), AudioError> {
        match self {
            Self::Offline { service, .. } => service.set_volume(voice, volume),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.set_volume(voice, volume),
        }
    }

    fn set_pan(&mut self, voice: VoiceHandle, pan: f32) -> Result<(), AudioError> {
        match self {
            Self::Offline { service, .. } => service.set_pan(voice, pan),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.set_pan(voice, pan),
        }
    }

    fn reap_stopped(&mut self) -> Result<(), AudioError> {
        match self {
            Self::Offline { service, .. } => service.reap_stopped().map(|_| ()),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.reap_stopped().map(|_| ()),
        }
    }

    fn contains_voice(&self, voice: VoiceHandle) -> bool {
        match self {
            Self::Offline { service, .. } => service.contains_voice(voice),
            #[cfg(feature = "native-audio")]
            Self::Native(service) => service.contains_voice(voice),
        }
    }

    fn render_offline(&mut self, frames: usize) -> Result<Vec<f32>, SessionAudioError> {
        match self {
            Self::Offline { service, .. } => service
                .render_offline(frames)
                .map_err(map_operation_audio_error),
            #[cfg(feature = "native-audio")]
            Self::Native(_) => Err(SessionAudioError::UnsupportedBackendOperation),
        }
    }
}

struct TrackedVoice {
    sequence: u64,
    handle: VoiceHandle,
    alias_index: usize,
    requested_volume: f32,
    requested_pan: f32,
    looping: bool,
    stop_requested: bool,
}

/// Actor-confined bank, backend, and bounded active-voice sidecar.
pub(crate) struct ProjectAudioRuntime {
    // Preserve this order: automatic field drop releases the backend before PCM bank storage.
    backend: Option<RuntimeAudioBackend>,
    bank: Option<ProjectAudioBank>,
    session: SessionId,
    voices: Vec<TrackedVoice>,
    max_voices: usize,
    next_voice_sequence: Option<u64>,
    last_issued_voice_sequence: Option<u64>,
    offline_progressed_frames: u64,
}

impl ProjectAudioRuntime {
    pub(crate) fn start(
        project: &PreparedProject,
        session: SessionId,
        options: SessionAudioOptions,
    ) -> Result<Self, SessionAudioStartError> {
        options.validate()?;
        if options.backend == BackendSelection::Native && !cfg!(feature = "native-audio") {
            return Err(SessionAudioStartError::NativeAudioFeatureUnavailable);
        }
        let bank = ProjectAudioBank::prepare(
            project,
            options.bank_limits,
            options.max_voices,
            options.backend,
        )?;
        let mut voices = Vec::new();
        voices
            .try_reserve_exact(options.max_voices)
            .map_err(|_| SessionAudioStartError::AllocationFailed)?;
        // Bank conversion and all budget admission happen before a native stream is opened.
        let backend = RuntimeAudioBackend::start(options)?;
        Ok(Self {
            backend: Some(backend),
            bank: Some(bank),
            session,
            voices,
            max_voices: options.max_voices,
            next_voice_sequence: Some(1),
            last_issued_voice_sequence: None,
            offline_progressed_frames: 0,
        })
    }

    pub(crate) fn reconcile(&mut self) -> Result<(), SessionAudioError> {
        let backend = self
            .backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?;
        backend.reap_stopped().map_err(map_operation_audio_error)?;
        self.voices
            .retain(|voice| backend.contains_voice(voice.handle));
        Ok(())
    }

    pub(crate) fn query(
        &mut self,
        operation: SessionAudioOperation,
        tick: Tick,
        max_entities: usize,
        max_bytes: usize,
    ) -> Result<SessionAudioResult, SessionAudioError> {
        self.reconcile()?;
        match operation {
            SessionAudioOperation::Status => Ok(SessionAudioResult::Status(self.status()?)),
            SessionAudioOperation::Clips { offset, limit } => self
                .clip_page(offset, limit, tick, max_entities, max_bytes)
                .map(SessionAudioResult::Clips),
            SessionAudioOperation::Voice { voice } => self
                .voice_info(voice, max_bytes)
                .map(SessionAudioResult::Voice),
            SessionAudioOperation::Voices { offset, limit } => self
                .voice_page(offset, limit, tick, max_entities, max_bytes)
                .map(SessionAudioResult::Voices),
            SessionAudioOperation::Command { .. } => Err(SessionAudioError::WrongOperation),
        }
    }

    pub(crate) fn execute(
        &mut self,
        command_id: CommandId,
        mutation: SessionAudioMutation,
        observed_tick: Tick,
    ) -> SessionAudioCommandResult {
        let outcome = self.execute_inner(mutation);
        SessionAudioCommandResult {
            command_id,
            observed_tick,
            outcome,
        }
    }

    pub(crate) fn shutdown(mut self) {
        drop(self.backend.take());
        drop(self.bank.take());
    }

    fn execute_inner(
        &mut self,
        mutation: SessionAudioMutation,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        self.reconcile()?;
        match mutation {
            SessionAudioMutation::Play {
                clip,
                volume,
                pan,
                looping,
            } => self.play(clip, volume, pan, looping),
            SessionAudioMutation::Stop { voice } => self.stop(voice),
            SessionAudioMutation::SetVolume { voice, volume } => self.set_volume(voice, volume),
            SessionAudioMutation::SetPan { voice, pan } => self.set_pan(voice, pan),
            SessionAudioMutation::RenderOffline { frames } => self.render_offline(frames),
        }
    }

    fn status(&self) -> Result<SessionAudioStatus, SessionAudioError> {
        let bank = self
            .bank
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?;
        let backend = self
            .backend
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?;
        Ok(SessionAudioStatus {
            session: self.session,
            backend: backend.selected(),
            alias_count: bank.aliases.len(),
            unique_clip_count: bank.clips.len(),
            converted_clip_count: bank.clips.len(),
            converted_frame_bytes: bank.converted_frame_bytes,
            active_voice_count: self.voices.len(),
            offline_progressed_frames: self.offline_progressed_frames,
        })
    }

    fn clip_page(
        &self,
        offset: usize,
        requested_limit: Option<usize>,
        tick: Tick,
        max_entities: usize,
        max_bytes: usize,
    ) -> Result<SessionAudioClipPage, SessionAudioError> {
        let bank = self
            .bank
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?;
        let limit = bounded_limit(requested_limit, max_entities)?;
        let start = offset.min(bank.aliases.len());
        let end = start.saturating_add(limit).min(bank.aliases.len());
        preflight_clip_page(&bank.aliases[start..end], max_bytes)?;
        let mut clips = Vec::new();
        clips
            .try_reserve_exact(end - start)
            .map_err(|_| SessionAudioError::AllocationFailed)?;
        for index in start..end {
            clips.push(bank.clip_info(index)?);
        }
        Ok(SessionAudioClipPage {
            observed_tick: tick,
            total: bank.aliases.len(),
            offset: start,
            limit,
            next_offset: (end < bank.aliases.len()).then_some(end),
            clips,
        })
    }

    fn voice_info(
        &self,
        voice_id: SessionAudioVoiceId,
        max_bytes: usize,
    ) -> Result<SessionAudioVoiceInfo, SessionAudioError> {
        let index = self.resolve_voice_index(voice_id)?;
        let Some(index) = index else {
            preflight_voice_info(None, max_bytes)?;
            return Ok(SessionAudioVoiceInfo {
                voice: voice_id,
                state: SessionAudioVoiceState::Ended,
                clip_id: None,
                requested_volume: None,
                requested_pan: None,
                looping: None,
            });
        };
        let voice = &self.voices[index];
        let alias = self
            .bank
            .as_ref()
            .and_then(|bank| bank.aliases.get(voice.alias_index))
            .ok_or(SessionAudioError::BackendFailure)?;
        preflight_voice_info(Some(alias.id.as_str()), max_bytes)?;
        self.active_voice_info(index)
    }

    fn voice_page(
        &self,
        offset: usize,
        requested_limit: Option<usize>,
        tick: Tick,
        max_entities: usize,
        max_bytes: usize,
    ) -> Result<SessionAudioVoicePage, SessionAudioError> {
        let limit = bounded_limit(requested_limit, max_entities)?;
        let start = offset.min(self.voices.len());
        let end = start.saturating_add(limit).min(self.voices.len());
        let bank = self
            .bank
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?;
        preflight_voice_page(&self.voices[start..end], bank, max_bytes)?;
        let mut voices = Vec::new();
        voices
            .try_reserve_exact(end - start)
            .map_err(|_| SessionAudioError::AllocationFailed)?;
        for index in start..end {
            voices.push(self.active_voice_info(index)?);
        }
        Ok(SessionAudioVoicePage {
            observed_tick: tick,
            total: self.voices.len(),
            offset: start,
            limit,
            next_offset: (end < self.voices.len()).then_some(end),
            voices,
        })
    }

    fn active_voice_info(&self, index: usize) -> Result<SessionAudioVoiceInfo, SessionAudioError> {
        let tracked = &self.voices[index];
        let bank = self
            .bank
            .as_ref()
            .expect("active audio bank remains present");
        let alias = bank
            .aliases
            .get(tracked.alias_index)
            .expect("every bank clip has at least one alias");
        Ok(SessionAudioVoiceInfo {
            voice: SessionAudioVoiceId::from_parts(self.session, tracked.sequence),
            state: if tracked.stop_requested {
                SessionAudioVoiceState::StopRequested
            } else {
                SessionAudioVoiceState::Playing
            },
            clip_id: Some(try_clone_stable_id_operation(&alias.id)?),
            requested_volume: Some(tracked.requested_volume),
            requested_pan: Some(tracked.requested_pan),
            looping: Some(tracked.looping),
        })
    }

    fn resolve_voice_index(
        &self,
        voice: SessionAudioVoiceId,
    ) -> Result<Option<usize>, SessionAudioError> {
        if voice.sequence == 0 {
            return Err(SessionAudioError::InvalidVoiceId);
        }
        if voice.session != self.session {
            return Err(SessionAudioError::WrongVoiceSession);
        }
        if self
            .last_issued_voice_sequence
            .is_none_or(|last| voice.sequence > last)
        {
            return Err(SessionAudioError::VoiceNotIssued);
        }
        Ok(self
            .voices
            .binary_search_by_key(&voice.sequence, |tracked| tracked.sequence)
            .ok())
    }

    fn play(
        &mut self,
        clip_id: StableId,
        volume: f32,
        pan: f32,
        looping: bool,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        if !volume.is_finite()
            || !(0.0..=1.0).contains(&volume)
            || !pan.is_finite()
            || !(-1.0..=1.0).contains(&pan)
        {
            return Err(SessionAudioError::InvalidInput);
        }
        if self.voices.len() >= self.max_voices {
            return Err(SessionAudioError::VoiceCapacityReached);
        }
        let sequence = self
            .next_voice_sequence
            .ok_or(SessionAudioError::CounterOverflow)?;
        let bank = self
            .bank
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?;
        let alias_index = bank
            .alias_index(&clip_id)
            .ok_or(SessionAudioError::UnknownClip)?;
        let clip_index = bank.aliases[alias_index].clip_index;
        let clip = &bank.clips[clip_index].pcm;
        let backend = self
            .backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?;
        let handle = backend
            .play(clip, volume, pan, looping)
            .map_err(map_operation_audio_error)?;
        // The vector is reserved to the configured voice cap before backend startup.
        self.voices.push(TrackedVoice {
            sequence,
            handle,
            alias_index,
            requested_volume: volume,
            requested_pan: pan,
            looping,
            stop_requested: false,
        });
        self.last_issued_voice_sequence = Some(sequence);
        self.next_voice_sequence = sequence.checked_add(1);
        Ok(SessionAudioCommandOutcome::Played {
            voice: SessionAudioVoiceId::from_parts(self.session, sequence),
        })
    }

    fn stop(
        &mut self,
        voice: SessionAudioVoiceId,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        let Some(index) = self.resolve_voice_index(voice)? else {
            return Err(SessionAudioError::VoiceEnded);
        };
        self.backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?
            .stop(self.voices[index].handle)
            .map_err(map_operation_audio_error)?;
        self.voices[index].stop_requested = true;
        Ok(SessionAudioCommandOutcome::StopRequested { voice })
    }

    fn set_volume(
        &mut self,
        voice: SessionAudioVoiceId,
        volume: f32,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(SessionAudioError::InvalidInput);
        }
        let Some(index) = self.resolve_voice_index(voice)? else {
            return Err(SessionAudioError::VoiceEnded);
        };
        self.backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?
            .set_volume(self.voices[index].handle, volume)
            .map_err(map_operation_audio_error)?;
        self.voices[index].requested_volume = volume;
        Ok(SessionAudioCommandOutcome::VolumeSet {
            voice,
            requested_volume: volume,
        })
    }

    fn set_pan(
        &mut self,
        voice: SessionAudioVoiceId,
        pan: f32,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(SessionAudioError::InvalidInput);
        }
        let Some(index) = self.resolve_voice_index(voice)? else {
            return Err(SessionAudioError::VoiceEnded);
        };
        self.backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?
            .set_pan(self.voices[index].handle, pan)
            .map_err(map_operation_audio_error)?;
        self.voices[index].requested_pan = pan;
        Ok(SessionAudioCommandOutcome::PanSet {
            voice,
            requested_pan: pan,
        })
    }

    fn render_offline(
        &mut self,
        frames: usize,
    ) -> Result<SessionAudioCommandOutcome, SessionAudioError> {
        if !(1..=MAX_ACTOR_OFFLINE_FRAMES).contains(&frames) {
            return Err(SessionAudioError::InvalidInput);
        }
        let cumulative_frames = self
            .offline_progressed_frames
            .checked_add(u64::try_from(frames).map_err(|_| SessionAudioError::CounterOverflow)?)
            .ok_or(SessionAudioError::CounterOverflow)?;
        let samples = self
            .backend
            .as_mut()
            .ok_or(SessionAudioError::BackendFailure)?
            .render_offline(frames)?;
        let SessionAudioBackend::Offline { sample_rate } = self
            .backend
            .as_ref()
            .ok_or(SessionAudioError::BackendFailure)?
            .selected()
        else {
            return Err(SessionAudioError::UnsupportedBackendOperation);
        };
        if samples.len() != frames.saturating_mul(2)
            || samples.iter().any(|sample| !sample.is_finite())
        {
            self.offline_progressed_frames = cumulative_frames;
            return Err(SessionAudioError::NonFiniteOutput);
        }
        let mut peak = 0.0_f64;
        let mut sum_squares = 0.0_f64;
        let mut left_peak = 0.0_f64;
        let mut left_squares = 0.0_f64;
        let mut right_peak = 0.0_f64;
        let mut right_squares = 0.0_f64;
        let mut nonzero_samples = 0_usize;
        for frame in samples.chunks_exact(2) {
            let left = f64::from(frame[0]);
            let right = f64::from(frame[1]);
            peak = peak.max(left.abs()).max(right.abs());
            sum_squares += left * left + right * right;
            left_peak = left_peak.max(left.abs());
            left_squares += left * left;
            right_peak = right_peak.max(right.abs());
            right_squares += right * right;
            if left != 0.0 {
                nonzero_samples += 1;
            }
            if right != 0.0 {
                nonzero_samples += 1;
            }
        }
        if !peak.is_finite()
            || !sum_squares.is_finite()
            || !left_squares.is_finite()
            || !right_squares.is_finite()
        {
            self.offline_progressed_frames = cumulative_frames;
            return Err(SessionAudioError::NonFiniteOutput);
        }
        self.offline_progressed_frames = cumulative_frames;
        self.reconcile()?;
        let rms = (sum_squares / samples.len() as f64).sqrt();
        let left_rms = (left_squares / frames as f64).sqrt();
        let right_rms = (right_squares / frames as f64).sqrt();
        Ok(SessionAudioCommandOutcome::Offline(OfflineAudioSummary {
            frames,
            sample_rate,
            channels: 2,
            peak: peak as f32,
            rms: rms as f32,
            left_peak: left_peak as f32,
            left_rms: left_rms as f32,
            right_peak: right_peak as f32,
            right_rms: right_rms as f32,
            nonzero_samples,
            cumulative_frames,
        }))
    }
}

fn bounded_limit(
    requested: Option<usize>,
    max_entities: usize,
) -> Result<usize, SessionAudioError> {
    if max_entities == 0 {
        return Err(SessionAudioError::InvalidInput);
    }
    let limit = requested.unwrap_or(max_entities).min(max_entities);
    if limit == 0 {
        return Err(SessionAudioError::InvalidInput);
    }
    Ok(limit)
}

fn preflight_clip_page(aliases: &[BankAlias], max_bytes: usize) -> Result<(), SessionAudioError> {
    let required = aliases.iter().try_fold(256_usize, |bytes, alias| {
        bytes
            .checked_add(CONSERVATIVE_PAGE_ROW_JSON_BYTES)
            .and_then(|value| value.checked_add(alias.id.as_str().len().checked_mul(6)?))
    });
    if required.is_none_or(|required| required > max_bytes) {
        return Err(SessionAudioError::ResultTooLarge);
    }
    Ok(())
}

fn preflight_voice_info(clip_id: Option<&str>, max_bytes: usize) -> Result<(), SessionAudioError> {
    let required = CONSERVATIVE_PAGE_ROW_JSON_BYTES
        .checked_add(
            clip_id
                .unwrap_or_default()
                .len()
                .checked_mul(6)
                .ok_or(SessionAudioError::ResultTooLarge)?,
        )
        .ok_or(SessionAudioError::ResultTooLarge)?;
    if required > max_bytes {
        return Err(SessionAudioError::ResultTooLarge);
    }
    Ok(())
}

fn preflight_voice_page(
    voices: &[TrackedVoice],
    bank: &ProjectAudioBank,
    max_bytes: usize,
) -> Result<(), SessionAudioError> {
    let mut required = 256_usize;
    for voice in voices {
        let id = bank
            .aliases
            .get(voice.alias_index)
            .map(|alias| alias.id.as_str())
            .ok_or(SessionAudioError::BackendFailure)?;
        required = required
            .checked_add(CONSERVATIVE_PAGE_ROW_JSON_BYTES)
            .and_then(|value| value.checked_add(id.len().checked_mul(6)?))
            .ok_or(SessionAudioError::ResultTooLarge)?;
    }
    if required > max_bytes {
        return Err(SessionAudioError::ResultTooLarge);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_wav_project() -> PreparedProject {
    tests::wav_project()
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::BTreeMap;

    use daedalus_content::{
        assets::{ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache},
        prepare_project_with_assets,
        project::{ProjectLimits, prepare_project_from_bytes},
    };

    use super::*;

    const MANIFEST: &[u8] =
        br#"{"schema_version":1,"name":"audio-bank-test","scene":"scene.json"}"#;

    pub(super) fn wav_project() -> PreparedProject {
        let wav = mono_pcm16_wav(8_000, 16_384);
        let scene = br#"{"schema_version":1,"revision":0,"entities":[],"assets":[{"id":"clip_a","kind":"daedalus.audio","uri":"audio/test.wav","metadata":{"schema_version":1}},{"id":"clip_b","kind":"daedalus.audio","uri":"audio/test.wav","metadata":{"schema_version":1}}]}"#;
        let mut resolver =
            ByteMapSourceResolver::from_bytes(BTreeMap::from([("audio/test.wav".to_owned(), wav)]))
                .unwrap();
        let mut cache = MemoryCookCache::default();
        prepare_project_with_assets(
            MANIFEST,
            scene,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap()
    }

    fn mono_pcm16_wav(frames: usize, sample: i16) -> Vec<u8> {
        let data_bytes = u32::try_from(frames * 2).unwrap();
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&16_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for _ in 0..frames {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn stereo_float32_wav(sample_rate: u32, frames: usize, left: f32, right: f32) -> Vec<u8> {
        let data_bytes = u32::try_from(frames * 8).unwrap();
        let mut bytes = Vec::with_capacity(44 + data_bytes as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 8).to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for _ in 0..frames {
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&right.to_le_bytes());
        }
        bytes
    }

    fn project_with_wav(source_id: &str, wav: Vec<u8>, scene: &[u8]) -> PreparedProject {
        let mut resolver =
            ByteMapSourceResolver::from_bytes(BTreeMap::from([(source_id.to_owned(), wav)]))
                .unwrap();
        let mut cache = MemoryCookCache::default();
        prepare_project_with_assets(
            MANIFEST,
            scene,
            &mut resolver,
            &mut cache,
            ImportLimits::default(),
            &ImportCancellation::never(),
        )
        .unwrap()
    }

    #[test]
    fn imported_wav_aliases_share_one_converted_clip_and_offline_mixer_reports_channels() {
        let project = wav_project();
        let session = SessionId::fresh();
        let mut runtime = ProjectAudioRuntime::start(
            &project,
            session,
            SessionAudioOptions::offline(8_000).with_max_voices(4),
        )
        .unwrap();

        let bank = runtime.bank.as_ref().unwrap();
        assert_eq!(bank.aliases.len(), 2);
        assert_eq!(bank.clips.len(), 1);
        assert_eq!(bank.aliases[0].clip_index, bank.aliases[1].clip_index);
        let status = runtime.status().unwrap();
        assert_eq!(status.alias_count(), 2);
        assert_eq!(status.unique_clip_count(), 1);
        assert_eq!(status.converted_clip_count(), 1);
        assert!(status.converted_frame_bytes() > 0);
        let clips = runtime
            .query(
                SessionAudioOperation::Clips {
                    offset: 0,
                    limit: None,
                },
                Tick(0),
                16,
                4_096,
            )
            .unwrap();
        let clip_rows = clips.as_clips().unwrap().clips();
        assert_eq!(clip_rows.len(), 2);
        assert_eq!(clip_rows[0].key(), clip_rows[1].key());
        assert_eq!(clip_rows[0].asset_id().as_str(), "clip_a");
        assert_eq!(clip_rows[1].asset_id().as_str(), "clip_b");
        assert_eq!(
            runtime
                .query(
                    SessionAudioOperation::Clips {
                        offset: 0,
                        limit: Some(2),
                    },
                    Tick(0),
                    16,
                    32,
                )
                .unwrap_err(),
            SessionAudioError::ResultTooLarge
        );

        let command_id = CommandId::from_raw(1);
        let played = runtime.execute(
            command_id,
            SessionAudioMutation::Play {
                clip: StableId::new("clip_b").unwrap(),
                volume: 0.8,
                pan: -1.0,
                looping: false,
            },
            Tick(0),
        );
        assert_eq!(played.command_id(), command_id);
        assert_eq!(played.observed_tick(), Tick(0));
        let Ok(SessionAudioCommandOutcome::Played { voice }) = played.outcome() else {
            panic!("imported WAV did not start in the offline mixer");
        };
        let voice = *voice;
        let voice_result = runtime
            .query(SessionAudioOperation::Voice { voice }, Tick(0), 16, 4_096)
            .unwrap();
        assert_eq!(
            voice_result.as_voice().unwrap().clip_id().unwrap().as_str(),
            "clip_b"
        );

        // Kira applies parameter changes across a render chunk, even with a zero-duration
        // Tween. Discard the first chunk after Play so the baseline measures steady-state gain.
        let initial_settle = runtime.execute(
            CommandId::from_raw(2),
            SessionAudioMutation::RenderOffline { frames: 256 },
            Tick(0),
        );
        let Ok(SessionAudioCommandOutcome::Offline(initial_settle)) = initial_settle.outcome()
        else {
            panic!("offline backend did not return an initial settling block");
        };
        assert_eq!(initial_settle.frames(), 256);
        assert_eq!(initial_settle.cumulative_frames(), 256);

        let baseline = runtime.execute(
            CommandId::from_raw(3),
            SessionAudioMutation::RenderOffline { frames: 256 },
            Tick(0),
        );
        let Ok(SessionAudioCommandOutcome::Offline(summary)) = baseline.outcome() else {
            panic!("offline backend did not return a bounded baseline summary");
        };
        assert_eq!(summary.frames(), 256);
        assert_eq!(summary.sample_rate(), 8_000);
        assert_eq!(summary.channels(), 2);
        assert!(summary.nonzero_samples() > 0);
        assert!(summary.left_peak() > 0.0);
        assert!(summary.left_rms() > 0.0);
        assert!(summary.right_peak() <= 0.000_01);
        let first_left_peak = summary.left_peak();
        assert_eq!(summary.cumulative_frames(), 512);
        assert_eq!(runtime.status().unwrap().offline_progressed_frames(), 512);

        assert!(matches!(
            runtime
                .execute(
                    CommandId::from_raw(4),
                    SessionAudioMutation::SetVolume { voice, volume: 0.4 },
                    Tick(0),
                )
                .outcome(),
            Ok(SessionAudioCommandOutcome::VolumeSet { .. })
        ));

        // The first block after SetVolume interpolates from the prior gain. Allow that actual
        // render interval to pass, then compare peaks from the following steady-state block.
        let changed_gain_settle = runtime.execute(
            CommandId::from_raw(5),
            SessionAudioMutation::RenderOffline { frames: 256 },
            Tick(1),
        );
        let Ok(SessionAudioCommandOutcome::Offline(changed_gain_settle)) =
            changed_gain_settle.outcome()
        else {
            panic!("offline backend did not return a gain settling block");
        };
        assert_eq!(changed_gain_settle.frames(), 256);
        assert_eq!(changed_gain_settle.cumulative_frames(), 768);

        let quieter = runtime.execute(
            CommandId::from_raw(6),
            SessionAudioMutation::RenderOffline { frames: 256 },
            Tick(1),
        );
        let Ok(SessionAudioCommandOutcome::Offline(quieter)) = quieter.outcome() else {
            panic!("offline volume update did not return a summary");
        };
        let volume_ratio = quieter.left_peak() / first_left_peak;
        assert!((volume_ratio - 0.5).abs() < 0.02);
        assert_eq!(quieter.cumulative_frames(), 1_024);

        let invalid_frames = runtime.execute(
            CommandId::from_raw(7),
            SessionAudioMutation::RenderOffline { frames: 4_097 },
            Tick(2),
        );
        assert_eq!(
            invalid_frames.outcome(),
            &Err(SessionAudioError::InvalidInput)
        );
        assert_eq!(runtime.status().unwrap().offline_progressed_frames(), 1_024);

        let wrong_session = SessionAudioVoiceId::from_parts(SessionId::fresh(), voice.sequence());
        assert_eq!(
            runtime
                .query(
                    SessionAudioOperation::Voice {
                        voice: wrong_session
                    },
                    Tick(0),
                    16,
                    4_096,
                )
                .unwrap_err(),
            SessionAudioError::WrongVoiceSession
        );
    }

    #[test]
    fn startup_preflights_alias_limit_and_legacy_project_has_an_empty_bank() {
        let project = wav_project();
        let limits = AudioBankLimits::default().with_max_aliases(1);
        let error = match ProjectAudioRuntime::start(
            &project,
            SessionId::fresh(),
            SessionAudioOptions::offline(8_000).with_bank_limits(limits),
        ) {
            Ok(_) => panic!("over-budget imported audio bank started"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            SessionAudioStartError::BudgetExceeded {
                resource: AudioBankResource::Aliases,
                required: 2,
                limit: 1,
            }
        );

        let resource_limits = [
            (
                AudioBankLimits::default().with_max_unique_clips(0),
                AudioBankResource::UniqueClips,
            ),
            (
                AudioBankLimits::default().with_max_resident_frame_bytes(1),
                AudioBankResource::FrameResidentBytes,
            ),
            (
                AudioBankLimits::default().with_max_conversion_scratch_bytes(1),
                AudioBankResource::ConversionScratchBytes,
            ),
            (
                AudioBankLimits::default().with_max_metadata_bytes(1),
                AudioBankResource::MetadataBytes,
            ),
        ];
        for (limits, resource) in resource_limits {
            let result = ProjectAudioRuntime::start(
                &project,
                SessionId::fresh(),
                SessionAudioOptions::offline(8_000).with_bank_limits(limits),
            );
            assert!(matches!(
                result,
                Err(SessionAudioStartError::BudgetExceeded {
                    resource: actual,
                    ..
                }) if actual == resource
            ));
        }

        let scene = br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#;
        let legacy =
            prepare_project_from_bytes(MANIFEST, scene, &BTreeMap::new(), ProjectLimits::default())
                .unwrap();
        let options = SessionAudioOptions::offline(8_000);
        let expected_empty_bank_metadata = size_of::<ProjectAudioBank>() as u64
            + size_of::<ProjectAudioRuntime>() as u64
            + (options.max_voices() * size_of::<TrackedVoice>()) as u64
            + size_of::<AudioService<OfflineBackend>>() as u64;
        let metadata_limited = match ProjectAudioRuntime::start(
            &legacy,
            SessionId::fresh(),
            options.with_bank_limits(
                AudioBankLimits::default()
                    .with_max_metadata_bytes(expected_empty_bank_metadata - 1),
            ),
        ) {
            Ok(_) => panic!("empty audio bank ignored backend-owner metadata"),
            Err(error) => error,
        };
        assert_eq!(
            metadata_limited,
            SessionAudioStartError::BudgetExceeded {
                resource: AudioBankResource::MetadataBytes,
                required: expected_empty_bank_metadata,
                limit: expected_empty_bank_metadata - 1,
            }
        );
        let runtime = ProjectAudioRuntime::start(&legacy, SessionId::fresh(), options).unwrap();
        let status = runtime.status().unwrap();
        assert_eq!(status.alias_count(), 0);
        assert_eq!(status.unique_clip_count(), 0);
        assert_eq!(status.converted_frame_bytes(), 0);
    }

    #[test]
    fn looping_voice_capacity_stop_and_issued_id_lifecycle_are_bounded() {
        let project = wav_project();
        let session = SessionId::fresh();
        let mut runtime = ProjectAudioRuntime::start(
            &project,
            session,
            SessionAudioOptions::offline(8_000).with_max_voices(1),
        )
        .unwrap();
        let played = runtime.execute(
            CommandId::from_raw(10),
            SessionAudioMutation::Play {
                clip: StableId::new("clip_a").unwrap(),
                volume: 1.0,
                pan: 0.0,
                looping: true,
            },
            Tick(0),
        );
        let Ok(SessionAudioCommandOutcome::Played { voice }) = played.outcome() else {
            panic!("looping WAV did not start");
        };
        let voice = *voice;
        assert_eq!(voice.sequence(), 1);

        for (bad_voice, expected) in [
            (
                SessionAudioVoiceId::from_parts(session, 0),
                SessionAudioError::InvalidVoiceId,
            ),
            (
                SessionAudioVoiceId::from_parts(session, 2),
                SessionAudioError::VoiceNotIssued,
            ),
        ] {
            assert_eq!(
                runtime
                    .query(
                        SessionAudioOperation::Voice { voice: bad_voice },
                        Tick(0),
                        16,
                        4_096,
                    )
                    .unwrap_err(),
                expected
            );
        }
        assert_eq!(
            runtime
                .execute(
                    CommandId::from_raw(11),
                    SessionAudioMutation::Play {
                        clip: StableId::new("clip_b").unwrap(),
                        volume: 1.0,
                        pan: 0.0,
                        looping: false,
                    },
                    Tick(0),
                )
                .outcome(),
            &Err(SessionAudioError::VoiceCapacityReached)
        );

        for id in [12_u64, 13] {
            assert!(matches!(
                runtime
                    .execute(
                        CommandId::from_raw(u128::from(id)),
                        SessionAudioMutation::RenderOffline { frames: 4_096 },
                        Tick(id),
                    )
                    .outcome(),
                Ok(SessionAudioCommandOutcome::Offline(_))
            ));
        }
        let voice_state = runtime
            .query(SessionAudioOperation::Voice { voice }, Tick(2), 16, 4_096)
            .unwrap();
        assert_eq!(
            voice_state.as_voice().unwrap().state(),
            SessionAudioVoiceState::Playing,
            "loop remains active after more than one clip duration"
        );

        assert_eq!(
            runtime
                .execute(
                    CommandId::from_raw(14),
                    SessionAudioMutation::Stop { voice },
                    Tick(3),
                )
                .outcome(),
            &Ok(SessionAudioCommandOutcome::StopRequested { voice })
        );
        assert!(matches!(
            runtime
                .execute(
                    CommandId::from_raw(15),
                    SessionAudioMutation::RenderOffline { frames: 256 },
                    Tick(4),
                )
                .outcome(),
            Ok(SessionAudioCommandOutcome::Offline(_))
        ));
        let ended = runtime
            .query(SessionAudioOperation::Voice { voice }, Tick(5), 16, 4_096)
            .unwrap();
        assert_eq!(
            ended.as_voice().unwrap().state(),
            SessionAudioVoiceState::Ended
        );
    }

    #[test]
    fn imported_float_stereo_wav_preserves_channel_order_in_offline_output() {
        let wav = stereo_float32_wav(8_000, 512, 0.25, -0.5);
        let scene = br#"{"schema_version":1,"revision":0,"entities":[],"assets":[{"id":"stereo","kind":"daedalus.audio","uri":"audio/stereo.wav","metadata":{"schema_version":1}}]}"#;
        let project = project_with_wav("audio/stereo.wav", wav, scene);
        let mut runtime = ProjectAudioRuntime::start(
            &project,
            SessionId::fresh(),
            SessionAudioOptions::offline(8_000),
        )
        .unwrap();
        assert_eq!(runtime.bank.as_ref().unwrap().clips.len(), 1);
        assert_eq!(runtime.bank.as_ref().unwrap().clips[0].channels, 2);
        assert!(matches!(
            runtime
                .execute(
                    CommandId::from_raw(20),
                    SessionAudioMutation::Play {
                        clip: StableId::new("stereo").unwrap(),
                        volume: 1.0,
                        pan: 0.0,
                        looping: false,
                    },
                    Tick(0),
                )
                .outcome(),
            Ok(SessionAudioCommandOutcome::Played { .. })
        ));
        let rendered = runtime.execute(
            CommandId::from_raw(21),
            SessionAudioMutation::RenderOffline { frames: 256 },
            Tick(0),
        );
        let Ok(SessionAudioCommandOutcome::Offline(summary)) = rendered.outcome() else {
            panic!("float stereo import did not render");
        };
        assert!(summary.left_peak() > 0.0);
        assert!(summary.right_peak() > summary.left_peak());
        assert!((summary.right_peak() / summary.left_peak() - 2.0).abs() < 0.05);
    }

    #[test]
    fn finite_float_pcm_above_playback_limit_is_preserved_by_import_but_rejected_for_bank() {
        let wav = stereo_float32_wav(8_000, 16, 65.0, -0.25);
        let scene = br#"{"schema_version":1,"revision":0,"entities":[],"assets":[{"id":"loud","kind":"daedalus.audio","uri":"audio/loud.wav","metadata":{"schema_version":1}}]}"#;
        let project = project_with_wav("audio/loud.wav", wav, scene);
        let asset = project
            .asset_catalog()
            .unwrap()
            .asset(&StableId::new("loud").unwrap())
            .unwrap();
        let ImportedAsset::Audio(asset) = asset else {
            panic!("audio importer returned a different kind");
        };
        assert_eq!(asset.samples()[0], 65.0);
        let result = ProjectAudioRuntime::start(
            &project,
            SessionId::fresh(),
            SessionAudioOptions::offline(8_000),
        );
        assert_eq!(
            result.err(),
            Some(SessionAudioStartError::PlaybackClipRejected)
        );
    }

    #[cfg(not(feature = "native-audio"))]
    #[test]
    fn native_backend_request_fails_explicitly_without_fallback() {
        let project = wav_project();
        assert_eq!(
            ProjectAudioRuntime::start(
                &project,
                SessionId::fresh(),
                SessionAudioOptions::native(),
            )
            .err(),
            Some(SessionAudioStartError::NativeAudioFeatureUnavailable)
        );
    }
}
