//! Engine-facing Kira audio service with native and offline output backends.

mod service;

#[cfg(feature = "native")]
pub use service::NativeAudioService;
pub use service::{
    AudioConfig, AudioDeviceInfo, AudioError, AudioService, MAX_AUDIO_BUFFER_FRAMES,
    MAX_AUDIO_VOICES, MAX_CLIP_FRAMES, MAX_OFFLINE_RENDER_FRAMES, MAX_PLAYBACK_SAMPLE_MAGNITUDE,
    OfflineBackend, OfflineBackendSettings, PCM_FRAME_BYTES, PcmClip, VoiceHandle,
    probe_default_output_device,
};
