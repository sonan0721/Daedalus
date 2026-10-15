//! Engine-facing Kira audio service with native and offline output backends.

mod service;

pub use service::{
    AudioConfig, AudioDeviceInfo, AudioError, AudioService, OfflineBackend, OfflineBackendSettings,
    PcmClip, VoiceHandle, probe_default_output_device,
};
