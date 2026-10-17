use std::fmt;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use daedalus_core::SessionId;
#[cfg(feature = "native")]
use kira::backend::cpal::CpalBackend;
use kira::backend::{Backend, Renderer};
use kira::sound::PlaybackState;
use kira::sound::static_sound::{StaticSoundData, StaticSoundHandle, StaticSoundSettings};
use kira::track::MainTrackBuilder;
use kira::{AudioManager, AudioManagerSettings, Decibels, Frame, Panning, Tween};

/// Maximum configured active voices for one Daedalus audio service.
pub const MAX_AUDIO_VOICES: usize = 4096;
/// Maximum internal mixer buffer size in frames.
pub const MAX_AUDIO_BUFFER_FRAMES: usize = 8192;
/// Maximum offline render request in frames.
pub const MAX_OFFLINE_RENDER_FRAMES: usize = 1_048_576;
/// Maximum PCM frames kept in a single static clip.
pub const MAX_CLIP_FRAMES: usize = 10_000_000;
/// Largest absolute source sample accepted for playback. Finite PCM headroom above 1.0 is kept.
pub const MAX_PLAYBACK_SAMPLE_MAGNITUDE: f32 = 64.0;
/// Byte size of one Kira output frame, for bounded PCM accounting without a Kira dependency.
pub const PCM_FRAME_BYTES: usize = size_of::<Frame>();

/// Audio service limits shared by native and offline backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioConfig {
    /// Maximum simultaneously tracked voices.
    pub max_voices: usize,
    /// Kira mixer block size in frames.
    pub internal_buffer_frames: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            max_voices: 128,
            internal_buffer_frames: 128,
        }
    }
}

impl AudioConfig {
    /// Validates configured mixer limits against this crate's supported bounds.
    pub fn validate(self) -> Result<(), AudioError> {
        if !(1..=MAX_AUDIO_VOICES).contains(&self.max_voices) {
            return Err(AudioError::InvalidInput(
                "max_voices must be between 1 and 4,096",
            ));
        }
        if !(1..=MAX_AUDIO_BUFFER_FRAMES).contains(&self.internal_buffer_frames) {
            return Err(AudioError::InvalidInput(
                "internal_buffer_frames must be between 1 and 8,192",
            ));
        }
        Ok(())
    }
}

/// Owned interleaved mono or stereo floating-point PCM converted into Kira's stereo frames.
#[derive(Clone, Debug)]
pub struct PcmClip {
    sample_rate: u32,
    frames: Arc<[Frame]>,
}

impl PcmClip {
    /// Converts borrowed interleaved PCM samples into a bounded static clip.
    ///
    /// Mono is duplicated to both channels. Stereo is kept as supplied. Samples must be finite
    /// and have magnitude no greater than [`MAX_PLAYBACK_SAMPLE_MAGNITUDE`]. Values above 1.0 are
    /// retained as headroom and are not normalized. Validation completes before frame allocation.
    pub fn from_interleaved_slice(
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
    ) -> Result<Self, AudioError> {
        let frame_count = validate_interleaved(sample_rate, channels, samples)?;
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(frame_count)
            .map_err(|_| AudioError::AllocationFailed)?;
        if channels == 1 {
            frames.extend(
                samples
                    .iter()
                    .copied()
                    .map(|sample| Frame::new(sample, sample)),
            );
        } else {
            frames.extend(
                samples
                    .chunks_exact(2)
                    .map(|channels| Frame::new(channels[0], channels[1])),
            );
        }
        Ok(Self {
            sample_rate,
            frames: Arc::from(frames),
        })
    }

    /// Converts owned interleaved PCM samples into a bounded static clip.
    ///
    /// This compatibility constructor shares validation and conversion with
    /// [`from_interleaved_slice`](Self::from_interleaved_slice).
    pub fn from_interleaved(
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) -> Result<Self, AudioError> {
        Self::from_interleaved_slice(sample_rate, channels, &samples)
    }

    /// Sample rate of this clip.
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of mono-equivalent frames.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    fn sound_data(&self, volume: f32, pan: f32, looping: bool) -> StaticSoundData {
        let settings = StaticSoundSettings::new()
            .volume(gain_to_decibels(volume))
            .panning(Panning(pan));
        let settings = if looping {
            settings.loop_region(..)
        } else {
            settings
        };
        StaticSoundData {
            sample_rate: self.sample_rate,
            frames: Arc::clone(&self.frames),
            settings,
            slice: None,
        }
    }
}

/// Opaque identity for a playing sound, scoped to one service and generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VoiceHandle {
    session: SessionId,
    index: u32,
    generation: u32,
}

/// Audio service input or backend failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioError {
    /// An input value exceeded the service's supported range.
    InvalidInput(&'static str),
    /// The configured number of voices is active or Kira has not reclaimed a stopped sound yet.
    VoiceCapacityReached,
    /// A voice handle belongs to another service, was stopped, or was superseded.
    StaleVoiceHandle,
    /// A bounded allocation failed before the service changed state.
    AllocationFailed,
    /// Kira or the selected native audio backend rejected the operation.
    Backend(String),
    /// A native output device exists but could not report a name or supported output config.
    DeviceProbe(String),
    /// This build does not include the optional native CPAL backend.
    BackendUnavailable,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "invalid audio input: {reason}"),
            Self::VoiceCapacityReached => f.write_str("audio service voice capacity reached"),
            Self::StaleVoiceHandle => {
                f.write_str("audio voice handle is stale or belongs to another service")
            }
            Self::AllocationFailed => {
                f.write_str("audio service could not reserve bounded storage")
            }
            Self::Backend(message) => write!(f, "audio backend error: {message}"),
            Self::DeviceProbe(message) => write!(f, "audio device probe failed: {message}"),
            Self::BackendUnavailable => {
                f.write_str("native audio backend is unavailable in this build")
            }
        }
    }
}

impl std::error::Error for AudioError {}

/// A Kira audio manager with session-safe voice handles and an explicit voice cap.
pub struct AudioService<B: Backend> {
    manager: AudioManager<B>,
    voices: VoiceArena,
    max_voices: usize,
}

impl<B: Backend> AudioService<B>
where
    B::Settings: Default,
    B::Error: fmt::Debug,
{
    fn with_backend(
        config: AudioConfig,
        backend_settings: B::Settings,
    ) -> Result<Self, AudioError> {
        config.validate()?;
        let settings = AudioManagerSettings {
            internal_buffer_size: config.internal_buffer_frames,
            main_track_builder: MainTrackBuilder::new().sound_capacity(config.max_voices),
            backend_settings,
            ..AudioManagerSettings::default()
        };
        let manager = AudioManager::<B>::new(settings)
            .map_err(|error| AudioError::Backend(format!("{error:?}")))?;
        Ok(Self {
            manager,
            voices: VoiceArena::new(SessionId::fresh()),
            max_voices: config.max_voices,
        })
    }

    /// Plays an owned PCM clip. `volume` is linear gain in `[0, 1]`; `pan` is `-1` left to `1`
    /// right. Voice capacity is checked before Kira receives the sound.
    pub fn play(
        &mut self,
        clip: &PcmClip,
        volume: f32,
        pan: f32,
    ) -> Result<VoiceHandle, AudioError> {
        self.play_with_looping(clip, volume, pan, false)
    }

    /// Plays an owned PCM clip, optionally looping over the entire clip. `volume` is linear gain
    /// in `[0, 1]`; `pan` is `-1` left to `1` right. Capacity is checked before Kira receives it.
    pub fn play_with_looping(
        &mut self,
        clip: &PcmClip,
        volume: f32,
        pan: f32,
        looping: bool,
    ) -> Result<VoiceHandle, AudioError> {
        validate_voice_controls(volume, pan)?;
        self.reap_stopped()?;
        if self.voices.len() >= self.max_voices
            || self.manager.main_track().num_sounds() >= self.max_voices
        {
            return Err(AudioError::VoiceCapacityReached);
        }
        self.voices.prepare_insert()?;
        let sound = self
            .manager
            .play(clip.sound_data(volume, pan, looping))
            .map_err(|error| match error {
                kira::PlaySoundError::SoundLimitReached => AudioError::VoiceCapacityReached,
                other => AudioError::Backend(format!("{other:?}")),
            })?;
        Ok(self.voices.insert_prepared(sound))
    }

    /// Requests an immediate stop. The handle stays valid until Kira processes the stop command;
    /// call [`reap_stopped`](Self::reap_stopped) or render/process another block to release it.
    pub fn stop(&mut self, handle: VoiceHandle) -> Result<(), AudioError> {
        self.voices.get_mut(handle)?.stop(immediate_tween());
        Ok(())
    }

    /// Sets linear volume gain in `[0, 1]` without blocking the audio callback.
    pub fn set_volume(&mut self, handle: VoiceHandle, volume: f32) -> Result<(), AudioError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(AudioError::InvalidInput("volume gain must be in [0, 1]"));
        }
        self.voices
            .get_mut(handle)?
            .set_volume(gain_to_decibels(volume), immediate_tween());
        Ok(())
    }

    /// Sets stereo balance in `[-1, 1]` without blocking the audio callback.
    pub fn set_pan(&mut self, handle: VoiceHandle, pan: f32) -> Result<(), AudioError> {
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(AudioError::InvalidInput("pan must be in [-1, 1]"));
        }
        self.voices
            .get_mut(handle)?
            .set_panning(Panning(pan), immediate_tween());
        Ok(())
    }

    /// Number of voices currently tracked, including sounds whose stop command is still pending.
    pub fn active_voice_count(&self) -> usize {
        self.voices.len()
    }

    /// Returns whether this handle currently occupies a slot in this service.
    ///
    /// A stopped sound remains present until [`reap_stopped`](Self::reap_stopped) observes Kira's
    /// stopped state. This query does not reap or inspect audibility.
    pub fn contains_voice(&self, handle: VoiceHandle) -> bool {
        self.voices.contains(handle)
    }

    /// Releases handles for sounds Kira has finished processing.
    pub fn reap_stopped(&mut self) -> Result<usize, AudioError> {
        self.voices.reap_stopped()
    }
}

/// Service specialized to Kira's actual CPAL backend. Available only with the `native` feature.
#[cfg(feature = "native")]
pub type NativeAudioService = AudioService<CpalBackend>;

/// Constructs a service using Kira's actual CPAL default backend.
#[cfg(feature = "native")]
impl AudioService<CpalBackend>
where
    CpalBackend: Backend,
    <CpalBackend as Backend>::Settings: Default,
    <CpalBackend as Backend>::Error: fmt::Debug,
{
    /// Opens Kira's default CPAL output backend. The constructor may start a silent output stream;
    /// no sound is generated until a voice is played.
    pub fn native(config: AudioConfig) -> Result<Self, AudioError> {
        Self::with_backend(config, kira::backend::cpal::CpalBackendSettings::default())
    }
}

/// Settings for the offline mixer output backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflineBackendSettings {
    /// Sample rate passed to Kira's mixer.
    pub sample_rate: u32,
}

impl Default for OfflineBackendSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
        }
    }
}

/// A Kira backend that captures actual mixed PCM instead of opening a device.
pub struct OfflineBackend {
    renderer: Option<Renderer>,
}

impl Backend for OfflineBackend {
    type Settings = OfflineBackendSettings;
    type Error = &'static str;

    fn setup(
        settings: Self::Settings,
        _internal_buffer_size: usize,
    ) -> Result<(Self, u32), Self::Error> {
        if !(8_000..=384_000).contains(&settings.sample_rate) {
            return Err("offline sample rate must be in 8,000..=384,000 Hz");
        }
        Ok((Self { renderer: None }, settings.sample_rate))
    }

    fn start(&mut self, renderer: Renderer) -> Result<(), Self::Error> {
        self.renderer = Some(renderer);
        Ok(())
    }
}

impl AudioService<OfflineBackend> {
    /// Creates an offline Kira mixer that renders into returned interleaved stereo PCM.
    pub fn offline(config: AudioConfig, sample_rate: u32) -> Result<Self, AudioError> {
        Self::with_backend(config, OfflineBackendSettings { sample_rate })
    }

    /// Runs Kira's real mixer for `frames` and returns interleaved stereo output samples.
    pub fn render_offline(&mut self, frames: usize) -> Result<Vec<f32>, AudioError> {
        if frames == 0 || frames > MAX_OFFLINE_RENDER_FRAMES {
            return Err(AudioError::InvalidInput(
                "offline render frames must be in 1..=1,048,576",
            ));
        }
        self.voices.prepare_reap_capacity(self.voices.slots.len())?;
        let samples_len = frames.checked_mul(2).ok_or(AudioError::AllocationFailed)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(samples_len)
            .map_err(|_| AudioError::AllocationFailed)?;
        samples.resize(samples_len, 0.0);
        let renderer = self
            .manager
            .backend_mut()
            .renderer
            .as_mut()
            .ok_or_else(|| AudioError::Backend("offline renderer was not started".to_owned()))?;
        renderer.on_start_processing();
        renderer.process(&mut samples, 2);
        self.voices.reap_stopped_prepared();
        Ok(samples)
    }
}

/// Information read from the host's selected default output device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceInfo {
    /// Host-reported device name.
    pub name: String,
    /// Host-reported default sample rate.
    pub sample_rate: u32,
    /// Host-reported output channel count.
    pub channels: u16,
}

/// Queries CPAL's default output device and supported configuration without opening a stream.
///
/// Returning `Ok(None)` means the host reported no default output device. This operation does not
/// generate sound. On builds without the CPAL feature, it returns an explicit backend error.
pub fn probe_default_output_device() -> Result<Option<AudioDeviceInfo>, AudioError> {
    #[cfg(not(feature = "native"))]
    {
        Err(AudioError::BackendUnavailable)
    }
    #[cfg(feature = "native")]
    {
        use kira::backend::cpal::cpal::traits::{DeviceTrait, HostTrait};

        let host = kira::backend::cpal::cpal::default_host();
        let Some(device) = host.default_output_device() else {
            return Ok(None);
        };
        let description = device
            .description()
            .map_err(|error| AudioError::DeviceProbe(format!("{error:?}")))?;
        let config = device
            .default_output_config()
            .map_err(|error| AudioError::DeviceProbe(format!("{error:?}")))?;
        Ok(Some(AudioDeviceInfo {
            name: description.name().to_owned(),
            sample_rate: config.sample_rate(),
            channels: config.channels(),
        }))
    }
}

fn validate_interleaved(
    sample_rate: u32,
    channels: u16,
    samples: &[f32],
) -> Result<usize, AudioError> {
    if !(8_000..=384_000).contains(&sample_rate) {
        return Err(AudioError::InvalidInput(
            "PCM sample rate must be in 8,000..=384,000 Hz",
        ));
    }
    if !(1..=2).contains(&channels) {
        return Err(AudioError::InvalidInput(
            "PCM clips must contain one or two channels",
        ));
    }
    if samples.is_empty() || !samples.len().is_multiple_of(usize::from(channels)) {
        return Err(AudioError::InvalidInput(
            "PCM sample count must be non-empty and divisible by channel count",
        ));
    }
    let frame_count = samples.len() / usize::from(channels);
    if frame_count > MAX_CLIP_FRAMES {
        return Err(AudioError::InvalidInput(
            "PCM clip exceeds the 10,000,000-frame limit",
        ));
    }
    if !samples
        .iter()
        .all(|sample| sample.is_finite() && sample.abs() <= MAX_PLAYBACK_SAMPLE_MAGNITUDE)
    {
        return Err(AudioError::InvalidInput(
            "PCM samples must be finite and have magnitude at most 64",
        ));
    }
    Ok(frame_count)
}

fn validate_voice_controls(volume: f32, pan: f32) -> Result<(), AudioError> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err(AudioError::InvalidInput("volume gain must be in [0, 1]"));
    }
    if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
        return Err(AudioError::InvalidInput("pan must be in [-1, 1]"));
    }
    Ok(())
}

fn gain_to_decibels(gain: f32) -> Decibels {
    if gain == 0.0 {
        Decibels::SILENCE
    } else {
        Decibels(20.0 * gain.log10())
    }
}

fn immediate_tween() -> Tween {
    Tween {
        duration: Duration::ZERO,
        ..Tween::default()
    }
}

struct VoiceArena {
    session: SessionId,
    slots: Vec<VoiceSlot>,
    free: Vec<u32>,
    live_count: usize,
}

struct VoiceSlot {
    generation: u32,
    voice: Option<StaticSoundHandle>,
    retired: bool,
}

impl VoiceArena {
    fn new(session: SessionId) -> Self {
        Self {
            session,
            slots: Vec::new(),
            free: Vec::new(),
            live_count: 0,
        }
    }

    fn len(&self) -> usize {
        self.live_count
    }

    fn prepare_insert(&mut self) -> Result<(), AudioError> {
        let needs_new_slot = self.free.is_empty();
        if needs_new_slot {
            if self.slots.len() >= u32::MAX as usize {
                return Err(AudioError::VoiceCapacityReached);
            }
            self.slots
                .try_reserve(1)
                .map_err(|_| AudioError::AllocationFailed)?;
        }
        let slot_count_after_insert = self
            .slots
            .len()
            .checked_add(usize::from(needs_new_slot))
            .ok_or(AudioError::AllocationFailed)?;
        self.prepare_reap_capacity(slot_count_after_insert)?;
        Ok(())
    }

    fn insert_prepared(&mut self, voice: StaticSoundHandle) -> VoiceHandle {
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(VoiceSlot {
                generation: 0,
                voice: None,
                retired: false,
            });
            index
        };
        let slot = &mut self.slots[index as usize];
        debug_assert!(slot.voice.is_none() && !slot.retired);
        slot.voice = Some(voice);
        self.live_count += 1;
        VoiceHandle {
            session: self.session,
            index,
            generation: slot.generation,
        }
    }

    fn get_mut(&mut self, handle: VoiceHandle) -> Result<&mut StaticSoundHandle, AudioError> {
        self.checked_slot_mut(handle)?
            .voice
            .as_mut()
            .ok_or(AudioError::StaleVoiceHandle)
    }

    fn reap_stopped(&mut self) -> Result<usize, AudioError> {
        self.prepare_reap_capacity(self.slots.len())?;
        Ok(self.reap_stopped_prepared())
    }

    fn prepare_reap_capacity(&mut self, maximum_slot_count: usize) -> Result<(), AudioError> {
        let additional_free_slots = maximum_slot_count.saturating_sub(self.free.len());
        self.free
            .try_reserve(additional_free_slots)
            .map_err(|_| AudioError::AllocationFailed)
    }

    fn reap_stopped_prepared(&mut self) -> usize {
        debug_assert!(self.free.capacity() >= self.slots.len());
        let mut reaped = 0;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if !slot
                .voice
                .as_ref()
                .is_some_and(|voice| voice.state() == PlaybackState::Stopped)
            {
                continue;
            }

            slot.voice.take();
            self.live_count -= 1;
            if let Some(generation) = slot.generation.checked_add(1) {
                slot.generation = generation;
                self.free.push(index as u32);
            } else {
                slot.retired = true;
            }
            reaped += 1;
        }
        reaped
    }

    fn checked_slot_mut(&mut self, handle: VoiceHandle) -> Result<&mut VoiceSlot, AudioError> {
        if handle.session != self.session {
            return Err(AudioError::StaleVoiceHandle);
        }
        let slot = self
            .slots
            .get_mut(handle.index as usize)
            .ok_or(AudioError::StaleVoiceHandle)?;
        if slot.generation != handle.generation || slot.retired {
            return Err(AudioError::StaleVoiceHandle);
        }
        Ok(slot)
    }

    fn contains(&self, handle: VoiceHandle) -> bool {
        if handle.session != self.session {
            return false;
        }
        self.slots.get(handle.index as usize).is_some_and(|slot| {
            slot.generation == handle.generation && !slot.retired && slot.voice.is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioConfig, AudioError, AudioService, MAX_AUDIO_VOICES, MAX_PLAYBACK_SAMPLE_MAGNITUDE,
        PCM_FRAME_BYTES, PcmClip, probe_default_output_device,
    };
    use kira::Frame;
    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    use kira::sound::PlaybackState;
    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    use std::time::{Duration, Instant};

    fn constant_clip(value: f32, frames: usize) -> PcmClip {
        PcmClip::from_interleaved(48_000, 1, vec![value; frames]).unwrap()
    }

    #[test]
    fn actual_kira_mixer_renders_pcm_with_pan_and_volume() {
        let mut service = AudioService::offline(
            AudioConfig {
                max_voices: 2,
                internal_buffer_frames: 64,
            },
            48_000,
        )
        .unwrap();
        let clip = constant_clip(0.25, 4096);
        let voice = service.play(&clip, 0.5, 1.0).unwrap();
        let output = service.render_offline(128).unwrap();
        assert!(output.iter().any(|sample| sample.abs() > 0.01));
        let left_peak = output
            .iter()
            .step_by(2)
            .map(|sample| sample.abs())
            .fold(0.0, f32::max);
        let right_peak = output
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.abs())
            .fold(0.0, f32::max);
        assert!(
            right_peak > left_peak * 4.0,
            "left={left_peak}, right={right_peak}"
        );
        service.set_volume(voice, 0.25).unwrap();
        service.set_pan(voice, 0.0).unwrap();
        let centered = service.render_offline(128).unwrap();
        let center_left = centered
            .iter()
            .step_by(2)
            .map(|sample| sample.abs())
            .fold(0.0, f32::max);
        assert!(center_left > 0.001);
        assert!(service.stop(voice).is_ok());
        service.render_offline(128).unwrap();
        assert_eq!(service.active_voice_count(), 0);
        assert_eq!(
            service.set_pan(voice, 0.0),
            Err(AudioError::StaleVoiceHandle)
        );
    }

    #[test]
    fn voice_capacity_stale_sessions_and_input_limits_are_enforced() {
        let config = AudioConfig {
            max_voices: 1,
            internal_buffer_frames: 64,
        };
        let mut first = AudioService::offline(config, 48_000).unwrap();
        let mut second = AudioService::offline(config, 48_000).unwrap();
        let clip = constant_clip(0.1, 4096);
        let voice = first.play(&clip, 1.0, 0.0).unwrap();
        first.render_offline(128).unwrap();
        assert_eq!(
            second.set_volume(voice, 0.5),
            Err(AudioError::StaleVoiceHandle)
        );
        assert!(first.contains_voice(voice));
        assert!(!second.contains_voice(voice));
        assert_eq!(
            first.play(&clip, 1.0, 0.0),
            Err(AudioError::VoiceCapacityReached)
        );
        first.stop(voice).unwrap();
        first.render_offline(1024).unwrap();
        assert_eq!(first.active_voice_count(), 0);
        assert!(!first.contains_voice(voice));
        assert_eq!(
            first.set_volume(voice, 1.0),
            Err(AudioError::StaleVoiceHandle)
        );
        assert_eq!(
            first.play(&clip, 1.0, 0.0),
            Err(AudioError::VoiceCapacityReached)
        );
        // Kira removes a stopped sound from its mixer storage at the next processing boundary.
        first.render_offline(64).unwrap();
        let recycled_voice = first.play(&clip, 1.0, 0.0).unwrap();
        assert_ne!(recycled_voice, voice);
        assert!(!first.contains_voice(voice));
        assert!(first.contains_voice(recycled_voice));
        assert!(matches!(
            PcmClip::from_interleaved(48_000, 2, vec![0.0, f32::NAN]),
            Err(AudioError::InvalidInput(_))
        ));
        assert!(matches!(
            first.render_offline(super::MAX_OFFLINE_RENDER_FRAMES + 1),
            Err(AudioError::InvalidInput(_))
        ));
    }

    #[test]
    fn borrowed_pcm_enforces_magnitude_before_playback_and_loops_full_clip() {
        let edge = [
            -MAX_PLAYBACK_SAMPLE_MAGNITUDE,
            MAX_PLAYBACK_SAMPLE_MAGNITUDE,
            1.25,
            -1.25,
        ];
        let clip = PcmClip::from_interleaved_slice(48_000, 2, &edge).unwrap();
        assert_eq!(clip.frame_count(), 2);
        assert_eq!(clip.frames[0], Frame::new(-64.0, 64.0));
        assert_eq!(clip.frames[1], Frame::new(1.25, -1.25));
        assert_eq!(PCM_FRAME_BYTES, std::mem::size_of::<Frame>());

        for invalid in [
            64.000_01,
            -64.000_01,
            f32::MAX,
            -f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ] {
            assert!(matches!(
                PcmClip::from_interleaved_slice(48_000, 1, &[invalid]),
                Err(AudioError::InvalidInput(_))
            ));
            assert!(matches!(
                PcmClip::from_interleaved(48_000, 1, vec![invalid]),
                Err(AudioError::InvalidInput(_))
            ));
        }

        let short_clip = PcmClip::from_interleaved_slice(8_000, 1, &[0.25; 8]).unwrap();
        let mut service = AudioService::offline(
            AudioConfig {
                max_voices: 2,
                internal_buffer_frames: 64,
            },
            48_000,
        )
        .unwrap();
        let other_service = AudioService::offline(
            AudioConfig {
                max_voices: 1,
                internal_buffer_frames: 64,
            },
            48_000,
        )
        .unwrap();
        let looping = service
            .play_with_looping(&short_clip, 1.0, 0.0, true)
            .unwrap();
        let one_shot = service.play(&short_clip, 1.0, 0.0).unwrap();
        assert!(service.contains_voice(looping));
        assert!(service.contains_voice(one_shot));
        assert!(!other_service.contains_voice(looping));

        let rendered = service.render_offline(2_048).unwrap();
        assert!(rendered.iter().all(|sample| sample.is_finite()));
        assert!(service.contains_voice(looping));
        assert!(!service.contains_voice(one_shot));
        service.stop(looping).unwrap();
        service.render_offline(512).unwrap();
        assert!(!service.contains_voice(looping));
    }

    #[test]
    fn repeated_reaping_reuses_prepared_free_slot_storage() {
        let mut service = AudioService::offline(
            AudioConfig {
                max_voices: 4,
                internal_buffer_frames: 64,
            },
            48_000,
        )
        .unwrap();
        let clip = PcmClip::from_interleaved_slice(8_000, 1, &[0.5; 8]).unwrap();
        let voice = service.play(&clip, 1.0, 0.0).unwrap();
        let reserved_capacity = service.voices.free.capacity();
        assert!(reserved_capacity >= service.voices.slots.len());

        service.render_offline(2_048).unwrap();
        assert_eq!(service.reap_stopped().unwrap(), 0);
        assert!(!service.contains_voice(voice));
        let free_count = service.voices.free.len();
        let capacity_after_first_reap = service.voices.free.capacity();
        assert_eq!(free_count, 1);
        assert!(capacity_after_first_reap >= service.voices.slots.len());

        for _ in 0..128 {
            assert_eq!(service.reap_stopped().unwrap(), 0);
            assert_eq!(service.voices.free.capacity(), capacity_after_first_reap);
            assert_eq!(service.voices.free.len(), free_count);
        }
    }

    #[test]
    fn extreme_pcm_mix_is_finite_at_maximum_voice_count() {
        let mut same_sign_service = AudioService::offline(
            AudioConfig {
                max_voices: MAX_AUDIO_VOICES,
                internal_buffer_frames: 1,
            },
            48_000,
        )
        .unwrap();
        let same_sign_clip =
            PcmClip::from_interleaved_slice(48_000, 1, &[MAX_PLAYBACK_SAMPLE_MAGNITUDE; 2_048])
                .unwrap();
        for _ in 0..MAX_AUDIO_VOICES {
            same_sign_service.play(&same_sign_clip, 1.0, 0.0).unwrap();
        }
        let same_sign_output = same_sign_service.render_offline(128).unwrap();
        assert!(same_sign_output.iter().all(|sample| sample.is_finite()));
        assert!(same_sign_output.iter().any(|sample| sample.abs() > 0.99));

        let mut opposite_sign_service = AudioService::offline(
            AudioConfig {
                max_voices: MAX_AUDIO_VOICES,
                internal_buffer_frames: 1,
            },
            48_000,
        )
        .unwrap();
        let positive =
            PcmClip::from_interleaved_slice(48_000, 1, &[MAX_PLAYBACK_SAMPLE_MAGNITUDE; 2_048])
                .unwrap();
        let negative =
            PcmClip::from_interleaved_slice(48_000, 1, &[-MAX_PLAYBACK_SAMPLE_MAGNITUDE; 2_048])
                .unwrap();
        for _ in 0..(MAX_AUDIO_VOICES / 2) {
            opposite_sign_service.play(&positive, 1.0, -1.0).unwrap();
            opposite_sign_service.play(&negative, 1.0, 1.0).unwrap();
        }
        assert_eq!(opposite_sign_service.active_voice_count(), MAX_AUDIO_VOICES);
        let opposite_sign_output = opposite_sign_service.render_offline(128).unwrap();
        assert!(opposite_sign_output.iter().all(|sample| sample.is_finite()));
        assert!(opposite_sign_output.iter().any(|sample| *sample > 0.99));
        assert!(opposite_sign_output.iter().any(|sample| *sample < -0.99));
    }

    #[cfg(feature = "native")]
    #[test]
    fn native_device_probe_does_not_open_an_output_stream() {
        match probe_default_output_device() {
            Ok(Some(info)) => {
                assert!(!info.name.is_empty());
                assert!((8_000..=384_000).contains(&info.sample_rate));
                assert!(info.channels > 0);
            }
            Ok(None) | Err(AudioError::DeviceProbe(_)) => {}
            Err(error) => panic!("unexpected native device probe failure: {error}"),
        }
    }

    #[cfg(feature = "native")]
    #[test]
    #[ignore = "manually run when a physical or virtual output device is available"]
    fn inspect_native_default_output_device_without_playback() {
        match probe_default_output_device() {
            Ok(Some(info)) => println!(
                "default output: name={:?}, rate={} Hz, channels={}",
                info.name, info.sample_rate, info.channels
            ),
            Ok(None) => println!("no default output device is available"),
            Err(error) => println!("default output probe unavailable: {error}"),
        }
    }

    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    #[test]
    #[ignore = "requires an actual CPAL output device and is a diagnostic, not a CI test"]
    fn native_cpal_voice_progress_and_completion_diagnostic() {
        const SAMPLE_RATE: u32 = 8_000;
        const FRAME_COUNT: usize = 2_000;
        const POST_PLAY_TIMEOUT: Duration = Duration::from_secs(10);
        const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
        const MAX_OBSERVATIONS: usize = 205;
        const MAX_ERRORS_PER_OBSERVATION: usize = 4;
        const MAX_RETAINED_ERRORS: usize = 16;
        const MAX_RETAINED_CPU_SAMPLES: usize = 16;

        // Match scripts/smoke_project.py's PCM16 fixture exactly before the same signed-int16
        // normalization used by the imported WAV path: int() truncates toward zero.
        let pcm16: Vec<i16> = (0..FRAME_COUNT)
            .map(|frame| {
                (12_000.0
                    * (std::f64::consts::TAU * 440.0 * frame as f64 / SAMPLE_RATE as f64).sin())
                    as i16
            })
            .collect();
        let peak_pcm16 = pcm16
            .iter()
            .map(|sample| i32::from(*sample).unsigned_abs())
            .max()
            .unwrap_or_default();
        let normalized: Vec<f32> = pcm16
            .iter()
            .map(|sample| f32::from(*sample) / 32_768.0)
            .collect();
        let clip = PcmClip::from_interleaved_slice(SAMPLE_RATE, 1, &normalized)
            .expect("the bounded fixture must be valid PCM");

        let config = AudioConfig::default();
        let startup_started = Instant::now();
        let mut service = AudioService::<kira::backend::cpal::CpalBackend>::native(config)
            .unwrap_or_else(|error| {
                panic!(
                    "native CPAL service startup failed after {:?}: {error}",
                    startup_started.elapsed()
                )
            });
        let startup_elapsed = startup_started.elapsed();
        println!(
            "native_startup=ok startup_elapsed_ms={} sample_rate={} fixture_frames={} duration_ms=250 frequency_hz=440 pcm16_peak={} normalized_peak={:.6} config={{max_voices:{},internal_buffer_frames:{}}}",
            startup_elapsed.as_millis(),
            SAMPLE_RATE,
            FRAME_COUNT,
            peak_pcm16,
            peak_pcm16 as f64 / 32_768.0,
            config.max_voices,
            config.internal_buffer_frames,
        );

        let voice = service
            .play(&clip, 1.0, 0.0)
            .expect("native Play must be accepted");
        let playback_started = Instant::now();
        let deadline = playback_started + POST_PLAY_TIMEOUT;
        let mut observation_count = 0usize;
        let mut max_position_seconds = 0.0f64;
        let mut popped_error_count = 0usize;
        let mut retained_errors = Vec::with_capacity(MAX_RETAINED_ERRORS);
        let mut cpu_sample_count = 0usize;
        let mut retained_cpu_samples = Vec::with_capacity(MAX_RETAINED_CPU_SAMPLES);
        let mut last_discarded_error_count;
        let mut stopped_observed = false;

        loop {
            assert!(
                observation_count < MAX_OBSERVATIONS,
                "native diagnostic exceeded its bounded {MAX_OBSERVATIONS} observations"
            );
            let elapsed = playback_started.elapsed();
            let (state, position_seconds) = {
                let handle = service.voices.slots[voice.index as usize]
                    .voice
                    .as_ref()
                    .expect("voice must remain tracked until the explicit reap");
                (handle.state(), handle.position())
            };
            max_position_seconds = max_position_seconds.max(position_seconds);

            let (discarded_errors, cpu_usage) = {
                let backend = service.manager.backend_mut();
                for _ in 0..MAX_ERRORS_PER_OBSERVATION {
                    let Some(error) = backend.pop_error() else {
                        break;
                    };
                    popped_error_count = popped_error_count.saturating_add(1);
                    if retained_errors.len() < MAX_RETAINED_ERRORS {
                        retained_errors
                            .push(error.to_string().chars().take(256).collect::<String>());
                    }
                }
                let cpu_usage = backend.pop_cpu_usage();
                if cpu_usage.is_some() {
                    cpu_sample_count = cpu_sample_count.saturating_add(1);
                    if retained_cpu_samples.len() < MAX_RETAINED_CPU_SAMPLES {
                        retained_cpu_samples.push(cpu_usage.unwrap_or_default());
                    }
                }
                (backend.num_stream_errors_discarded(), cpu_usage)
            };
            last_discarded_error_count = discarded_errors;
            observation_count += 1;
            println!(
                "observation={observation_count} elapsed_ms={} state={state:?} position_ms={:.3} stream_errors_discarded={discarded_errors:?} cpu_usage={cpu_usage:?}",
                elapsed.as_millis(),
                position_seconds * 1_000.0,
            );

            if state == PlaybackState::Stopped {
                stopped_observed = true;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(SAMPLE_INTERVAL.min(deadline.saturating_duration_since(now)));
        }

        let final_elapsed = playback_started.elapsed();
        let final_state = service.voices.slots[voice.index as usize]
            .voice
            .as_ref()
            .expect("voice must still be tracked before the explicit reap")
            .state();
        println!(
            "native_playback_summary startup_elapsed_ms={} playback_elapsed_ms={} observations={} max_position_ms={:.3} final_state={final_state:?} stopped_observed={stopped_observed} errors_popped={} errors_retained_count={} errors_retained={:?} stream_errors_discarded={last_discarded_error_count:?} cpu_usage_samples={} cpu_usage_retained={:?}",
            startup_elapsed.as_millis(),
            final_elapsed.as_millis(),
            observation_count,
            max_position_seconds * 1_000.0,
            popped_error_count,
            retained_errors.len(),
            retained_errors,
            cpu_sample_count,
            retained_cpu_samples,
        );

        assert!(
            max_position_seconds > 0.01,
            "native voice made no observable playback progress in {final_elapsed:?}; final state={final_state:?}, observations={observation_count}, errors={retained_errors:?}, discarded={last_discarded_error_count:?}"
        );
        assert!(
            stopped_observed && final_state == PlaybackState::Stopped,
            "native finite voice did not reach Stopped within {POST_PLAY_TIMEOUT:?}; final state={final_state:?}, max position={max_position_seconds:.6}s, observations={observation_count}, errors={retained_errors:?}, discarded={last_discarded_error_count:?}"
        );

        let reaped = service
            .reap_stopped()
            .expect("observed stopped native voice must be safely reapable");
        assert_eq!(reaped, 1, "exactly the diagnostic voice should be reaped");
        assert_eq!(service.active_voice_count(), 0);
        assert!(!service.contains_voice(voice));
        println!("native_reap=ok reaped={reaped} active_voice_count=0 contains_voice=false");
    }

    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    #[test]
    #[ignore = "requires an actual CPAL default output device; direct-stream diagnostic only"]
    fn direct_cpal_default_stream_callback_progress_diagnostic() {
        use kira::backend::cpal::cpal;
        use kira::backend::cpal::cpal::traits::{DeviceTrait, HostTrait};
        let startup_started = Instant::now();
        let host = cpal::default_host();
        let host_id = host.id();
        let device = host
            .default_output_device()
            .expect("CPAL default output device is required for this diagnostic");
        let device_id = device
            .id()
            .expect("CPAL default output device ID must be available");
        assert_eq!(device_id.host(), host_id);
        let supported_config = device
            .default_output_config()
            .expect("default output configuration must be available");
        let sample_format = supported_config.sample_format();
        let config = supported_config.config();
        assert!(config.channels > 0, "default output must have channels");
        observe_direct_cpal_stream(
            "default_output",
            host_id,
            device_id,
            device,
            config,
            sample_format,
            startup_started.elapsed(),
        );
    }

    #[cfg(all(feature = "native", target_os = "macos", not(target_arch = "wasm32")))]
    #[test]
    #[ignore = "requires CoreAudio default output; same-device HAL route comparison"]
    fn direct_cpal_same_device_hal_callback_progress_diagnostic() {
        use kira::backend::cpal::cpal;
        use kira::backend::cpal::cpal::traits::{DeviceTrait, HostTrait};

        let startup_started = Instant::now();
        let host = cpal::default_host();
        let host_id = host.id();
        let default_device = host
            .default_output_device()
            .expect("CPAL default output device is required for the HAL comparison");
        let default_device_id = default_device
            .id()
            .expect("default output device ID must be available");
        assert_eq!(default_device_id.host(), host_id);
        let default_supported_config = default_device
            .default_output_config()
            .expect("default output config must be available");
        let default_sample_format = default_supported_config.sample_format();
        let baseline_config = default_supported_config.config();
        assert_eq!(
            baseline_config.buffer_size,
            cpal::BufferSize::Default,
            "the baseline request must retain CPAL's default buffer"
        );

        let devices = host
            .devices()
            .expect("CPAL output device enumeration must be available");
        let mut matching_device_count = 0usize;
        let mut enumerated_match = None;
        for device in devices {
            let device_id = device
                .id()
                .expect("every enumerated device must provide an ID for exact matching");
            if device_id == default_device_id {
                matching_device_count += 1;
                if enumerated_match.is_none() {
                    enumerated_match = Some(device);
                }
            }
        }
        assert_eq!(
            matching_device_count, 1,
            "same-device HAL comparison requires exactly one enumerated handle matching the default ID"
        );
        let enumerated_device =
            enumerated_match.expect("exactly one same-ID enumerated handle must be retained");
        let enumerated_device_id = enumerated_device
            .id()
            .expect("matching enumerated device ID must remain available");
        assert_eq!(enumerated_device_id, default_device_id);
        assert_eq!(enumerated_device_id.host(), host_id);

        let enumerated_supported_config = enumerated_device
            .default_output_config()
            .expect("same-ID enumerated device output config must be available");
        let enumerated_sample_format = enumerated_supported_config.sample_format();
        let enumerated_config = enumerated_supported_config.config();
        assert_eq!(
            enumerated_config.channels, baseline_config.channels,
            "same physical device reported a different output channel count"
        );
        assert_eq!(
            enumerated_config.sample_rate, baseline_config.sample_rate,
            "same physical device reported a different default output rate"
        );
        assert_eq!(
            enumerated_sample_format, default_sample_format,
            "same physical device reported a different sample format"
        );
        assert_eq!(
            enumerated_config.buffer_size, baseline_config.buffer_size,
            "same physical device reported a different default buffer request"
        );

        let current_default_id = host
            .default_output_device()
            .expect("default output device must remain available before HAL stream creation")
            .id()
            .expect("default output ID must remain available before HAL stream creation");
        assert_eq!(
            current_default_id, default_device_id,
            "system default output changed during same-device HAL comparison setup"
        );
        println!(
            "same_device_hal_comparison default_device_id={default_device_id} enumerated_device_id={enumerated_device_id} matching_handles={matching_device_count} default_config={baseline_config:?} default_reported_format={default_sample_format:?} enumerated_config={enumerated_config:?} enumerated_reported_format={enumerated_sample_format:?} default_unchanged=true"
        );

        observe_direct_cpal_stream(
            "same_id_enumerated_hal",
            host_id,
            enumerated_device_id,
            enumerated_device,
            baseline_config,
            default_sample_format,
            startup_started.elapsed(),
        );
    }

    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    fn observe_direct_cpal_stream(
        selection: &'static str,
        host_id: kira::backend::cpal::cpal::HostId,
        device_id: kira::backend::cpal::cpal::DeviceId,
        device: kira::backend::cpal::cpal::Device,
        config: kira::backend::cpal::cpal::StreamConfig,
        reported_sample_format: kira::backend::cpal::cpal::SampleFormat,
        selection_elapsed: Duration,
    ) {
        use kira::backend::cpal::cpal;
        use kira::backend::cpal::cpal::traits::{DeviceTrait, StreamTrait};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        const POST_PLAY_TIMEOUT: Duration = Duration::from_secs(10);
        const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
        const MINIMUM_CALLBACKS: u64 = 3;
        const MAX_OBSERVATIONS: usize = 205;

        struct CallbackCounts {
            callbacks: AtomicU64,
            frames: AtomicU64,
            errors_xrun: AtomicU64,
            errors_device_unavailable: AtomicU64,
            errors_device_changed: AtomicU64,
            errors_stream_invalidated: AtomicU64,
            errors_other: AtomicU64,
        }

        let counters = Arc::new(CallbackCounts {
            callbacks: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            errors_xrun: AtomicU64::new(0),
            errors_device_unavailable: AtomicU64::new(0),
            errors_device_changed: AtomicU64::new(0),
            errors_stream_invalidated: AtomicU64::new(0),
            errors_other: AtomicU64::new(0),
        });
        assert!(config.channels > 0, "output stream config needs channels");
        assert_eq!(config.buffer_size, cpal::BufferSize::Default);
        let channels = u64::from(config.channels);
        println!(
            "direct_cpal_{selection}_config host={host_id:?} device_id={device_id} sample_rate={} channels={} reported_sample_format={reported_sample_format:?} requested_output_format=f32 buffer_size={:?} selection_elapsed_ms={}",
            config.sample_rate,
            config.channels,
            config.buffer_size,
            selection_elapsed.as_millis(),
        );

        let callbacks_for_data = Arc::clone(&counters);
        let callbacks_for_error = Arc::clone(&counters);
        let startup_started = Instant::now();
        let stream = device
            .build_output_stream::<f32, _, _>(
                config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    data.fill(0.0);
                    callbacks_for_data.callbacks.fetch_add(1, Ordering::Relaxed);
                    callbacks_for_data
                        .frames
                        .fetch_add((data.len() as u64) / channels, Ordering::Relaxed);
                },
                move |error: cpal::Error| {
                    let counter = match error.kind() {
                        cpal::ErrorKind::Xrun => &callbacks_for_error.errors_xrun,
                        cpal::ErrorKind::DeviceNotAvailable => {
                            &callbacks_for_error.errors_device_unavailable
                        }
                        cpal::ErrorKind::DeviceChanged => {
                            &callbacks_for_error.errors_device_changed
                        }
                        cpal::ErrorKind::StreamInvalidated => {
                            &callbacks_for_error.errors_stream_invalidated
                        }
                        _ => &callbacks_for_error.errors_other,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                },
                None,
            )
            .expect("selected CPAL f32 output stream must build");
        let build_elapsed = startup_started.elapsed();
        let baseline_callbacks = counters.callbacks.load(Ordering::Relaxed);
        let baseline_frames = counters.frames.load(Ordering::Relaxed);
        let play_started = Instant::now();
        stream
            .play()
            .expect("selected CPAL output stream must start");
        let play_call_elapsed = play_started.elapsed();
        let observation_started = Instant::now();
        let deadline = observation_started + POST_PLAY_TIMEOUT;
        println!(
            "direct_cpal_{selection}_started build_elapsed_ms={} stream_play_call_ms={} callbacks_before_play={} frames_before_play={}",
            build_elapsed.as_millis(),
            play_call_elapsed.as_millis(),
            baseline_callbacks,
            baseline_frames,
        );

        let mut observations = 0usize;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            assert!(
                observations < MAX_OBSERVATIONS,
                "direct CPAL {selection} diagnostic exceeded its bounded {MAX_OBSERVATIONS} observations"
            );
            let elapsed = observation_started.elapsed();
            let callbacks = counters.callbacks.load(Ordering::Relaxed);
            let frames = counters.frames.load(Ordering::Relaxed);
            let xrun = counters.errors_xrun.load(Ordering::Relaxed);
            let device_unavailable = counters.errors_device_unavailable.load(Ordering::Relaxed);
            let device_changed = counters.errors_device_changed.load(Ordering::Relaxed);
            let stream_invalidated = counters.errors_stream_invalidated.load(Ordering::Relaxed);
            let other_errors = counters.errors_other.load(Ordering::Relaxed);
            observations += 1;
            println!(
                "direct_cpal_{selection}_observation={observations} elapsed_ms={} callbacks={} frames={} errors={{xrun:{xrun},device_unavailable:{device_unavailable},device_changed:{device_changed},stream_invalidated:{stream_invalidated},other:{other_errors}}}",
                elapsed.as_millis(),
                callbacks.saturating_sub(baseline_callbacks),
                frames.saturating_sub(baseline_frames),
            );

            let now = Instant::now();
            std::thread::sleep(SAMPLE_INTERVAL.min(deadline.saturating_duration_since(now)));
        }

        let final_callbacks = counters.callbacks.load(Ordering::Relaxed);
        let final_frames = counters.frames.load(Ordering::Relaxed);
        let error_counts = (
            counters.errors_xrun.load(Ordering::Relaxed),
            counters.errors_device_unavailable.load(Ordering::Relaxed),
            counters.errors_device_changed.load(Ordering::Relaxed),
            counters.errors_stream_invalidated.load(Ordering::Relaxed),
            counters.errors_other.load(Ordering::Relaxed),
        );
        let post_play_elapsed = observation_started.elapsed();
        let callback_delta = final_callbacks.saturating_sub(baseline_callbacks);
        let frame_delta = final_frames.saturating_sub(baseline_frames);
        println!(
            "direct_cpal_{selection}_summary selection_elapsed_ms={} build_elapsed_ms={} stream_play_call_ms={} post_play_elapsed_ms={} observations={} callbacks={} frames={} errors={{xrun:{},device_unavailable:{},device_changed:{},stream_invalidated:{},other:{}}}",
            selection_elapsed.as_millis(),
            build_elapsed.as_millis(),
            play_call_elapsed.as_millis(),
            post_play_elapsed.as_millis(),
            observations,
            callback_delta,
            frame_delta,
            error_counts.0,
            error_counts.1,
            error_counts.2,
            error_counts.3,
            error_counts.4,
        );

        assert!(
            callback_delta >= MINIMUM_CALLBACKS && frame_delta > 0,
            "direct CPAL {selection} stream made no sustained callback/frame progress during the bounded {POST_PLAY_TIMEOUT:?} observation: callbacks={callback_delta}, frames={frame_delta}, elapsed={post_play_elapsed:?}, errors={error_counts:?}"
        );
        drop(stream);
    }

    #[cfg(not(feature = "native"))]
    #[test]
    fn native_probe_is_explicitly_unavailable_without_feature() {
        assert_eq!(
            probe_default_output_device(),
            Err(AudioError::BackendUnavailable)
        );
    }
}
