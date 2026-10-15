use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use daedalus_core::SessionId;
use kira::backend::{Backend, DefaultBackend, Renderer};
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
    fn validate(self) -> Result<(), AudioError> {
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
    /// Converts interleaved PCM samples into a bounded static clip.
    ///
    /// Mono is duplicated to both channels. Stereo is kept as supplied. Samples must be finite;
    /// the Kira output renderer clamps the final mix to `[-1, 1]`.
    pub fn from_interleaved(
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
    ) -> Result<Self, AudioError> {
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
        if !samples.iter().all(|sample| sample.is_finite()) {
            return Err(AudioError::InvalidInput("PCM samples must be finite"));
        }
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(frame_count)
            .map_err(|_| AudioError::AllocationFailed)?;
        if channels == 1 {
            frames.extend(samples.into_iter().map(|sample| Frame::new(sample, sample)));
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

    /// Sample rate of this clip.
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of mono-equivalent frames.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    fn sound_data(&self, volume: f32, pan: f32) -> StaticSoundData {
        let settings = StaticSoundSettings::new()
            .volume(gain_to_decibels(volume))
            .panning(Panning(pan));
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
        validate_voice_controls(volume, pan)?;
        self.reap_stopped()?;
        if self.voices.len() >= self.max_voices
            || self.manager.main_track().num_sounds() >= self.max_voices
        {
            return Err(AudioError::VoiceCapacityReached);
        }
        self.voices.prepare_insert()?;
        let sound =
            self.manager
                .play(clip.sound_data(volume, pan))
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

    /// Releases handles for sounds Kira has finished processing.
    pub fn reap_stopped(&mut self) -> Result<usize, AudioError> {
        self.voices.reap_stopped()
    }
}

/// Constructs a service using Kira's actual CPAL default backend.
impl AudioService<DefaultBackend>
where
    DefaultBackend: Backend,
    <DefaultBackend as Backend>::Settings: Default,
    <DefaultBackend as Backend>::Error: fmt::Debug,
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
        self.reap_stopped()?;
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
        if self.free.is_empty() {
            if self.slots.len() >= u32::MAX as usize {
                return Err(AudioError::VoiceCapacityReached);
            }
            self.slots
                .try_reserve(1)
                .map_err(|_| AudioError::AllocationFailed)?;
        }
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
        let mut stopped = Vec::new();
        stopped
            .try_reserve_exact(self.slots.len())
            .map_err(|_| AudioError::AllocationFailed)?;
        stopped.extend(self.slots.iter().enumerate().filter_map(|(index, slot)| {
            slot.voice
                .as_ref()
                .is_some_and(|voice| voice.state() == PlaybackState::Stopped)
                .then_some(index)
        }));
        self.free
            .try_reserve(stopped.len())
            .map_err(|_| AudioError::AllocationFailed)?;
        for index in &stopped {
            let slot = &mut self.slots[*index];
            slot.voice.take();
            self.live_count -= 1;
            if let Some(generation) = slot.generation.checked_add(1) {
                slot.generation = generation;
                self.free.push(*index as u32);
            } else {
                slot.retired = true;
            }
        }
        Ok(stopped.len())
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
}

#[cfg(test)]
mod tests {
    use super::{AudioConfig, AudioError, AudioService, PcmClip, probe_default_output_device};

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
        assert_eq!(
            first.play(&clip, 1.0, 0.0),
            Err(AudioError::VoiceCapacityReached)
        );
        first.stop(voice).unwrap();
        first.render_offline(1024).unwrap();
        assert_eq!(first.active_voice_count(), 0);
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
        assert!(first.play(&clip, 1.0, 0.0).is_ok());
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
}
