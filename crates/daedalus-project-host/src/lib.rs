//! A native project/session facade shared by SDKs and transport adapters.
//!
//! The host composes the existing runtime authority, project-generation consumer, and one
//! renderer CPU owner. It does not create a second engine/session or expose prepared project,
//! packet, source, or device backing through status values.

#[cfg(feature = "capture")]
mod capture;
mod cpu_worker;
mod host;

pub use host::{
    NativePresentedSummary, NativeProjectHost, NativeProjectHostControl, NativeProjectHostError,
    NativeProjectHostFailure, NativeProjectHostLifecycle, NativeProjectHostOptions,
    NativeProjectHostShutdown, NativeProjectHostStartError, NativeProjectHostStatus,
    NativeProjectHostStatusRead, NativeProjectHostStatusReadError,
};

pub use daedalus_runtime::SessionShutdownReport;

#[cfg(feature = "window")]
pub use host::{NativeProjectViewOptions, NativeViewError, NativeViewOutcome};

#[cfg(feature = "capture")]
pub use capture::{
    NativeArtifactAccessError, NativeArtifactIoError, NativeCaptureArtifactLease,
    NativeCaptureArtifactReader,
};

#[cfg(feature = "capture")]
pub use host::{
    NativeCaptureEncoderState, NativeCaptureFailure, NativeCaptureStatusError,
    NativeProjectCaptureLifecycle, NativeProjectCaptureStatus,
};
