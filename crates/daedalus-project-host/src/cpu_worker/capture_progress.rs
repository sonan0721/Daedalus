//! Durable live-capture progress owned by the existing CPU worker.
//!
//! This state is kept outside the worker's per-iteration unwind boundary. A capture request,
//! renderer readback/cleanup handle, or encoder result therefore survives an unrelated panic in
//! one poll iteration and can still be terminalized by the same owner.

use std::time::Instant;

use daedalus_renderer::{
    ManagedCaptureCancelStatus, ManagedCaptureCleanup, ManagedCaptureCleanupError,
    ManagedCaptureClient, ManagedCaptureFailure, ManagedCaptureKey, ManagedCapturePixels,
    ManagedCaptureReadback, ManagedCaptureReadbackError, ManagedCaptureRequest,
    ManagedCaptureReturn, ManagedCaptureSource, ManagedCaptureState, ManagedCaptureSubmitError,
    ManagedCaptureSurfaceFormat, ManagedCaptureUnsupportedReason, ManagedCaptureWorker,
    ManagedCaptureWorkerError, RenderProjection,
};
use daedalus_runtime::{
    CaptureCompletion, CaptureFailure, CaptureHandoffState, CapturePixelEncoding,
    CaptureProjection, CaptureReadyInfo, CaptureResponderError, CaptureSurfaceFormat,
    CaptureUnsupportedReason, CaptureView, ProjectPublicationIdentity,
};

use super::{CpuCapturePorts, CpuFailure, bounded_message};
use crate::capture::{
    CaptureCpuWorkerPort, CaptureEncodeJob, CaptureEncodeResult, CaptureEncoderShutdown,
    CaptureIngressError, CapturePublicationBinding, CaptureRegistryError, RoutedCapture,
};

/// All accepted capture state is owned here until either the runtime responder accepts one
/// completion or all renderer/encoder work has been drained after the responder closes.
pub(super) struct CaptureWorkerState {
    ports: CpuCapturePorts,
    pending: Option<PendingCapture>,
    next_renderer_key: u64,
    pending_renderer_state: Option<CaptureHandoffState>,
    renderer_detached: bool,
    encoder_stop_requested: bool,
    encoder_shutdown: Option<CaptureEncoderShutdown>,
    reported_failure: bool,
}

struct PendingCapture {
    request: Box<daedalus_runtime::CaptureRequest>,
    stage_key: daedalus_renderer::ManagedStageKey,
    renderer_key: Option<ManagedCaptureKey>,
    renderer_request: Option<Box<ManagedCaptureRequest>>,
    renderer_submitted: bool,
    renderer_submit_uncertain: bool,
    cancel_sent: bool,
    readback: Option<ManagedCaptureReadback>,
    cleanup: Option<ManagedCaptureCleanup>,
    pixels: Option<ManagedCapturePixels>,
    selected_publication: Option<ProjectPublicationIdentity>,
    selected_present_sequence: Option<u64>,
    encode_job: Option<Box<CaptureEncodeJob>>,
    encode_in_flight: bool,
    encode_submit_uncertain: bool,
    encode_result: Option<CaptureEncodeResult>,
    completion: Option<CaptureCompletion>,
    terminal_failure: Option<CaptureFailure>,
}

impl CaptureWorkerState {
    pub(super) fn new(ports: CpuCapturePorts) -> Self {
        Self {
            ports,
            pending: None,
            next_renderer_key: 1,
            pending_renderer_state: None,
            renderer_detached: false,
            encoder_stop_requested: false,
            encoder_shutdown: None,
            reported_failure: false,
        }
    }

    /// Performs one nonwaiting capture progression step. `failure` is the existing CPU worker's
    /// bounded failure lane, not a second capture result channel.
    pub(super) fn progress(&mut self, stopping: bool, failure: &mut Option<CpuFailure>) {
        {
            let Self {
                ports,
                pending_renderer_state,
                renderer_detached,
                reported_failure,
                ..
            } = self;
            publish_renderer_state(
                ports,
                pending_renderer_state,
                renderer_detached,
                stopping,
                reported_failure,
                failure,
            );
        }
        {
            let Self {
                ports,
                pending,
                next_renderer_key,
                reported_failure,
                ..
            } = self;
            admit_one_request(
                ports,
                pending,
                next_renderer_key,
                stopping,
                reported_failure,
                failure,
            );
        }

        if let Some(pending) = self.pending.as_mut() {
            observe_cancel_or_deadline(&self.ports.renderer_client, pending, stopping);
            reconcile_uncertain_renderer_submit(
                &self.ports.renderer_worker,
                pending,
                stopping,
                &mut self.reported_failure,
                failure,
            );
            reconcile_uncertain_encode_submit(
                &self.ports.pipeline,
                pending,
                stopping,
                &mut self.reported_failure,
                failure,
            );
            submit_renderer_request(
                &self.ports.renderer_client,
                pending,
                stopping,
                &mut self.reported_failure,
                failure,
            );
        }

        {
            let Self {
                ports,
                pending,
                reported_failure,
                ..
            } = self;
            take_renderer_return(ports, pending, reported_failure, failure);
        }

        if let Some(pending) = self.pending.as_mut() {
            progress_readback(
                &self.ports.pipeline,
                pending,
                stopping,
                &mut self.reported_failure,
                failure,
            );
            progress_cleanup(pending, &mut self.reported_failure, failure);
            prepare_encode_job(pending, &mut self.reported_failure, failure);
            submit_encode_job(
                &self.ports.pipeline,
                pending,
                stopping,
                &mut self.reported_failure,
                failure,
            );
        }

        {
            let Self {
                ports,
                pending,
                reported_failure,
                ..
            } = self;
            take_encoder_result(ports, pending, stopping, reported_failure, failure);
        }

        if let Some(pending) = self.pending.as_mut() {
            finish_completion(pending);
            if pending_is_response_ready(pending) {
                let completed = {
                    let completion = pending
                        .completion
                        .as_ref()
                        .expect("response-ready capture retains a completion");
                    matches!(
                        pending.request.responder_mut().try_complete(completion),
                        Ok(())
                            | Err(CaptureResponderError::AlreadyCompleted
                                | CaptureResponderError::Stopped)
                    )
                };
                if completed {
                    self.pending.take();
                }
            }
        }

        self.finish_encoder_if_stopped(stopping, failure);
    }

    /// A per-iteration panic terminalizes its request, while accepted renderer/encoder work
    /// remains represented by the durable pending state and is drained on later iterations.
    pub(super) fn iteration_panicked(&mut self) {
        if let Some(pending) = self.pending.as_mut() {
            pending.set_failure(CaptureFailure::ReadbackFailed);
        }
    }

    pub(super) fn drained(&self) -> bool {
        self.pending.is_none()
            && self.renderer_detached
            && self.encoder_stop_requested
            && self.ports.pipeline.is_ingress_drained()
            && self.encoder_shutdown.is_some()
    }

    fn finish_encoder_if_stopped(&mut self, stopping: bool, failure: &mut Option<CpuFailure>) {
        if !stopping || self.pending.is_some() || !self.ports.pipeline.is_ingress_drained() {
            return;
        }
        if !self.renderer_detached {
            self.ports.renderer_client.request_detach();
            self.renderer_detached = true;
            self.pending_renderer_state = Some(CaptureHandoffState::Detached);
        }
        if !self.encoder_stop_requested {
            self.ports.pipeline.request_encoder_stop();
            self.encoder_stop_requested = true;
        }
        match self.ports.pipeline.try_finish_encoder() {
            CaptureEncoderShutdown::Stopping => {}
            CaptureEncoderShutdown::Stopped => {
                self.encoder_shutdown = Some(CaptureEncoderShutdown::Stopped)
            }
            CaptureEncoderShutdown::Failed(error) => {
                self.encoder_shutdown = Some(CaptureEncoderShutdown::Failed(error));
                report_failure_once(
                    &mut self.reported_failure,
                    failure,
                    "capture_encoder_failed",
                    &format!("capture encoder failed: {error}"),
                );
            }
        }
    }
}

fn publish_renderer_state(
    ports: &mut CpuCapturePorts,
    pending_state: &mut Option<CaptureHandoffState>,
    renderer_detached: &mut bool,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if stopping && !*renderer_detached {
        ports.renderer_client.request_detach();
        *renderer_detached = true;
        *pending_state = Some(CaptureHandoffState::Detached);
    }

    if !stopping && pending_state.is_none() {
        match ports.renderer_client.try_status() {
            Ok(state) => *pending_state = Some(map_renderer_state(state)),
            Err(daedalus_renderer::ManagedCaptureEndpointError::Busy) => {}
            Err(daedalus_renderer::ManagedCaptureEndpointError::Detached) => {
                *pending_state = Some(CaptureHandoffState::Detached);
            }
            Err(daedalus_renderer::ManagedCaptureEndpointError::Poisoned) => report_failure_once(
                reported_failure,
                failure,
                "capture_renderer_status_poisoned",
                "renderer capture status could not be read",
            ),
            Err(daedalus_renderer::ManagedCaptureEndpointError::AlreadyRegistered) => {
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_renderer_status_invalid",
                    "renderer capture endpoint was unexpectedly re-registered",
                );
            }
        }
    }

    if let Some(state) = *pending_state {
        match ports.pipeline.try_publish_renderer_state(state) {
            Ok(()) => *pending_state = None,
            Err(crate::capture::CaptureStatusReadError::Busy) => {}
            Err(crate::capture::CaptureStatusReadError::Poisoned) => report_failure_once(
                reported_failure,
                failure,
                "capture_status_poisoned",
                "capture status could not be updated",
            ),
        }
    }
}

fn admit_one_request(
    ports: &CpuCapturePorts,
    pending: &mut Option<PendingCapture>,
    next_renderer_key: &mut u64,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if pending.is_some() {
        return;
    }
    let routed = match ports.pipeline.try_take_request() {
        Ok(Some(routed)) => routed,
        Ok(None) | Err(CaptureIngressError::Busy | CaptureIngressError::Closed) => return,
        Err(CaptureIngressError::Poisoned) => {
            report_failure_once(
                reported_failure,
                failure,
                "capture_ingress_poisoned",
                "capture request transfer slot was poisoned",
            );
            return;
        }
    };

    // Install the full Runtime request in durable worker state immediately after taking it.
    *pending = Some(PendingCapture::new(routed));
    let request = pending.as_mut().expect("routed request was installed");
    if stopping {
        request.set_failure(CaptureFailure::Detached);
    } else if request.request.is_cancelled() {
        request.set_failure(CaptureFailure::Cancelled);
    } else if request.request.deadline() <= Instant::now() {
        request.set_failure(CaptureFailure::TimedOut);
    } else {
        match allocate_key(next_renderer_key) {
            Some(key) => {
                if let Err(error) = request.create_renderer_request(key) {
                    request.set_failure(error);
                }
            }
            None => {
                request.set_failure(CaptureFailure::ArtifactFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_key_exhausted",
                    "renderer capture key sequence is exhausted",
                );
            }
        }
    }
}

fn observe_cancel_or_deadline(
    client: &ManagedCaptureClient,
    pending: &mut PendingCapture,
    stopping: bool,
) {
    // A successful or failed compact result becomes immutable once it is built. In particular,
    // a responder Busy retry must not rewrite a captured completion to TimedOut after the
    // actor's reserved response slot becomes available.
    if pending.completion.is_none() && pending.terminal_failure.is_none() {
        let terminal = if pending.request.is_cancelled() {
            Some(CaptureFailure::Cancelled)
        } else if pending.request.deadline() <= Instant::now() {
            Some(CaptureFailure::TimedOut)
        } else if stopping {
            Some(CaptureFailure::Detached)
        } else {
            None
        };
        if let Some(failure) = terminal {
            pending.set_failure(failure);
        }
    }

    if pending.terminal_failure.is_some() {
        pending.renderer_request.take();
        if pending.renderer_submitted
            && !pending.cancel_sent
            && let Some(key) = pending.renderer_key
        {
            match client.request_cancel(key) {
                Ok(
                    ManagedCaptureCancelStatus::Accepted
                    | ManagedCaptureCancelStatus::AlreadySubmitted
                    | ManagedCaptureCancelStatus::Unknown,
                )
                | Err(daedalus_renderer::ManagedCaptureCancelError::Detached) => {
                    pending.cancel_sent = true;
                }
                Err(
                    daedalus_renderer::ManagedCaptureCancelError::Busy
                    | daedalus_renderer::ManagedCaptureCancelError::Poisoned,
                ) => {}
            }
        }
        if pending.readback.is_some() {
            pending.move_readback_to_cleanup();
        }
        pending.pixels.take();
        pending.encode_job.take();
    }
}

fn submit_renderer_request(
    client: &ManagedCaptureClient,
    pending: &mut PendingCapture,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if pending.terminal_failure.is_some() || pending.renderer_submitted {
        return;
    }
    let Some(request) = pending.renderer_request.take() else {
        return;
    };
    // Set the durable submitted bit before the ownership call. The uncertainty wrapper clears
    // its flag only when the call returns; unwinding inside or immediately after acceptance keeps
    // the same key available for reconciliation instead of manufacturing a second request.
    pending.renderer_submitted = true;
    let result = with_uncertain_handoff(&mut pending.renderer_submit_uncertain, || {
        client.try_submit(request)
    });
    match result {
        Ok(()) => {
            pending.renderer_submit_uncertain = false;
        }
        Err((request, ManagedCaptureSubmitError::Busy | ManagedCaptureSubmitError::NotReady)) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_request = Some(request);
            pending.renderer_submitted = false;
        }
        Err((request, error)) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_submitted = false;
            let deadline_elapsed = request.deadline() <= Instant::now();
            pending.set_failure(
                expected_request_termination(pending, stopping)
                    .unwrap_or_else(|| map_submit_failure(error)),
            );
            match error {
                ManagedCaptureSubmitError::Unsupported | ManagedCaptureSubmitError::Detached => {}
                ManagedCaptureSubmitError::WorkerUnavailable => {
                    if !stopping && !deadline_elapsed && !pending.request.is_cancelled() {
                        report_failure_once(
                            reported_failure,
                            failure,
                            "capture_renderer_worker_unavailable",
                            "capture worker disappeared before accepting a request",
                        );
                    }
                }
                ManagedCaptureSubmitError::InvalidRequest => {
                    if !deadline_elapsed && !pending.request.is_cancelled() && !stopping {
                        report_failure_once(
                            reported_failure,
                            failure,
                            "capture_renderer_request_invalid",
                            "a validated runtime capture request was rejected as invalid",
                        );
                    }
                }
                ManagedCaptureSubmitError::KeyReused | ManagedCaptureSubmitError::Poisoned => {
                    report_failure_once(
                        reported_failure,
                        failure,
                        "capture_renderer_submit_failed",
                        "renderer capture admission violated its key or state invariant",
                    );
                }
                ManagedCaptureSubmitError::Busy | ManagedCaptureSubmitError::NotReady => {
                    unreachable!("retryable renderer errors are handled before this arm")
                }
            }
            drop(request);
        }
    }
}

fn take_renderer_return(
    ports: &mut CpuCapturePorts,
    pending: &mut Option<PendingCapture>,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    let Some(pending) = pending.as_mut() else {
        return;
    };
    if !pending.renderer_submitted {
        return;
    }
    match ports.renderer_worker.try_take_return() {
        Ok(Some(ManagedCaptureReturn::Submitted(readback))) => {
            if Some(readback.key()) != pending.renderer_key {
                pending.set_failure(CaptureFailure::StaleIdentity);
                pending.readback = Some(readback);
                pending.move_readback_to_cleanup();
                pending.renderer_submitted = false;
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_renderer_key_mismatch",
                    "renderer returned a capture with a different key",
                );
            } else {
                pending.readback = Some(readback);
                pending.renderer_submitted = false;
            }
        }
        Ok(Some(ManagedCaptureReturn::Cleanup(cleanup))) => {
            if Some(cleanup.key()) != pending.renderer_key {
                pending.set_failure(CaptureFailure::StaleIdentity);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_renderer_key_mismatch",
                    "renderer cleanup returned a different capture key",
                );
            }
            pending.cleanup = Some(cleanup);
            pending.renderer_submitted = false;
            if pending.terminal_failure.is_none() {
                pending.set_failure(CaptureFailure::ReadbackFailed);
            }
        }
        Ok(Some(ManagedCaptureReturn::Failed {
            key,
            failure: renderer_failure,
            cleanup,
        })) => {
            pending.cleanup = Some(cleanup);
            pending.renderer_submitted = false;
            if Some(key) != pending.renderer_key {
                pending.set_failure(CaptureFailure::StaleIdentity);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_renderer_key_mismatch",
                    "renderer returned a capture failure with a different key",
                );
            } else if pending.terminal_failure.is_none() {
                pending.set_failure(map_renderer_failure(renderer_failure));
            }
        }
        Ok(None) | Err(ManagedCaptureWorkerError::Busy) => {}
        Err(ManagedCaptureWorkerError::Poisoned) => report_failure_once(
            reported_failure,
            failure,
            "capture_renderer_worker_poisoned",
            "renderer capture return state was poisoned",
        ),
        Err(ManagedCaptureWorkerError::WrongKey | ManagedCaptureWorkerError::InvalidState) => {
            pending.set_failure(CaptureFailure::ReadbackFailed);
            report_failure_once(
                reported_failure,
                failure,
                "capture_renderer_return_invalid",
                "renderer capture return state did not match its accepted request",
            );
        }
        Err(ManagedCaptureWorkerError::Detached) => {
            pending.set_failure(CaptureFailure::Detached);
        }
    }
}

fn progress_readback(
    pipeline: &CaptureCpuWorkerPort,
    pending: &mut PendingCapture,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if pending.terminal_failure.is_some() {
        if pending.readback.is_some() {
            pending.move_readback_to_cleanup();
        }
        return;
    }
    if pending.readback.is_none() {
        return;
    }

    if pending.selected_publication.is_none() {
        let selected = pending
            .readback
            .as_ref()
            .expect("readback was checked above")
            .selected_frame();
        match pipeline.resolve_selected_frame(selected) {
            Ok(Some(binding)) => match pending.validate_binding(&binding) {
                Ok(()) => {
                    pending.selected_publication = Some(binding.publication().clone());
                }
                Err(capture_failure) => {
                    pending.set_failure(capture_failure);
                    pending.move_readback_to_cleanup();
                    return;
                }
            },
            Ok(None) | Err(CaptureRegistryError::Stale | CaptureRegistryError::Conflict) => {
                pending.set_failure(CaptureFailure::StaleIdentity);
                pending.move_readback_to_cleanup();
                return;
            }
            Err(CaptureRegistryError::Busy) => return,
            Err(CaptureRegistryError::Stopped) => {
                let terminal = expected_request_termination(pending, stopping)
                    .unwrap_or(CaptureFailure::ReadbackFailed);
                pending.set_failure(terminal);
                if terminal == CaptureFailure::ReadbackFailed {
                    report_failure_once(
                        reported_failure,
                        failure,
                        "capture_publication_lookup_stopped",
                        "capture publication lookup stopped before the request was terminal",
                    );
                }
                pending.move_readback_to_cleanup();
                return;
            }
            Err(CaptureRegistryError::Full | CaptureRegistryError::Poisoned) => {
                pending.set_failure(CaptureFailure::ReadbackFailed);
                pending.move_readback_to_cleanup();
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_publication_lookup_failed",
                    "selected frame could not be resolved to a registered publication",
                );
                return;
            }
        }
    }

    let read = pending
        .readback
        .as_mut()
        .expect("readback remains installed after publication validation")
        .try_read_rgba8();
    match read {
        Ok(Some(pixels)) => {
            let present_sequence = pixels.presented().present_sequence();
            pending.selected_present_sequence = Some(present_sequence);
            pending.readback.take();
            pending.pixels = Some(pixels);
        }
        Ok(None) | Err(ManagedCaptureReadbackError::Busy) => {}
        Err(ManagedCaptureReadbackError::Poisoned) => {
            pending.set_failure(CaptureFailure::ReadbackFailed);
            pending.move_readback_to_cleanup();
            report_failure_once(
                reported_failure,
                failure,
                "capture_renderer_cell_poisoned",
                "renderer capture cell was poisoned during readback",
            );
        }
        Err(error) => {
            pending.set_failure(map_readback_error(error));
            pending.move_readback_to_cleanup();
        }
    }
}

fn progress_cleanup(
    pending: &mut PendingCapture,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    let Some(cleanup) = pending.cleanup.as_mut() else {
        return;
    };
    match cleanup.try_finish() {
        Ok(true) => {
            pending.cleanup.take();
        }
        Ok(false) | Err(ManagedCaptureCleanupError::Busy) => {}
        Err(ManagedCaptureCleanupError::InvalidState) => report_failure_once(
            reported_failure,
            failure,
            "capture_renderer_cleanup_invalid",
            "renderer capture cleanup could not retire its cell",
        ),
        Err(ManagedCaptureCleanupError::DevicePollFailed) => report_failure_once(
            reported_failure,
            failure,
            "capture_renderer_cleanup_poll_failed",
            "renderer device polling failed during capture cleanup",
        ),
        Err(ManagedCaptureCleanupError::Poisoned) => report_failure_once(
            reported_failure,
            failure,
            "capture_renderer_cleanup_poisoned",
            "renderer capture cleanup state was poisoned",
        ),
    }
}

fn prepare_encode_job(
    pending: &mut PendingCapture,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if pending.terminal_failure.is_some()
        || pending.encode_job.is_some()
        || pending.encode_in_flight
    {
        return;
    }
    let Some(pixels) = pending.pixels.take() else {
        return;
    };
    let Some(publication) = pending.selected_publication.as_ref() else {
        pending.set_failure(CaptureFailure::StaleIdentity);
        return;
    };
    let Some(present_sequence) = pending.selected_present_sequence else {
        pending.set_failure(CaptureFailure::StaleIdentity);
        return;
    };
    let (key, presented, format, rgba) = pixels.into_parts();
    if Some(key) != pending.renderer_key {
        pending.set_failure(CaptureFailure::StaleIdentity);
        report_failure_once(
            reported_failure,
            failure,
            "capture_renderer_key_mismatch",
            "renderer pixels did not match the accepted capture key",
        );
        return;
    }
    if !pending.request_is_current() {
        pending.set_failure(pending.current_request_failure());
        return;
    }
    let view = match capture_view(&presented) {
        Ok(view) => view,
        Err(_) => {
            pending.set_failure(CaptureFailure::ReadbackFailed);
            return;
        }
    };
    let surface_format = map_surface_format(format);
    let id = pending.request.id();
    let stage_key = pending.stage_key;
    let interest = pending.request.artifact().downgrade();
    let work_lease = pending.request.work_lease().clone();
    match CaptureEncodeJob::new(
        id,
        stage_key,
        publication.clone(),
        present_sequence,
        view,
        surface_format,
        CapturePixelEncoding::SrgbEncoded,
        interest,
        work_lease,
        rgba,
    ) {
        Ok(job) => pending.encode_job = Some(job),
        Err(crate::capture::CaptureEncodeJobError::InvalidPixels) => {
            pending.set_failure(CaptureFailure::TooLarge);
        }
        Err(crate::capture::CaptureEncodeJobError::InvalidEncoding) => {
            pending.set_failure(CaptureFailure::ReadbackFailed);
        }
        Err(crate::capture::CaptureEncodeJobError::ExpiredInterest) => {
            pending.set_failure(CaptureFailure::ResponderDropped);
        }
    }
}

fn submit_encode_job(
    pipeline: &CaptureCpuWorkerPort,
    pending: &mut PendingCapture,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if pending.terminal_failure.is_some() || pending.encode_in_flight {
        return;
    }
    let Some(job) = pending.encode_job.take() else {
        return;
    };
    pending.encode_in_flight = true;
    let result = with_uncertain_handoff(&mut pending.encode_submit_uncertain, || {
        pipeline.try_submit_encode(job)
    });
    match result {
        Ok(()) => {
            pending.encode_submit_uncertain = false;
        }
        Err((job, crate::capture::CaptureEncoderError::Busy)) => {
            pending.encode_submit_uncertain = false;
            pending.encode_job = Some(job);
            pending.encode_in_flight = false;
        }
        Err((job, crate::capture::CaptureEncoderError::Stopped)) => {
            pending.encode_submit_uncertain = false;
            drop(job);
            pending.encode_in_flight = false;
            if let Some(request_failure) = expected_request_termination(pending, stopping) {
                pending.set_failure(request_failure);
            } else {
                pending.set_failure(CaptureFailure::ArtifactFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_encoder_unavailable",
                    "capture encoder stopped before accepting a current request",
                );
            }
        }
        Err((job, crate::capture::CaptureEncoderError::Poisoned)) => {
            pending.encode_submit_uncertain = false;
            drop(job);
            pending.encode_in_flight = false;
            pending.set_failure(CaptureFailure::ArtifactFailed);
            report_failure_once(
                reported_failure,
                failure,
                "capture_encoder_poisoned",
                "capture encoder admission state was poisoned",
            );
        }
    }
}

/// Marks a consuming handoff uncertain only for the dynamic extent of a call that may transfer
/// ownership. If the call unwinds after accepting its payload, the flag remains set so the next
/// CPU-owner iteration reconciles the keyed destination before deciding whether work was lost.
fn with_uncertain_handoff<T>(uncertain: &mut bool, handoff: impl FnOnce() -> T) -> T {
    *uncertain = true;
    let result = handoff();
    *uncertain = false;
    result
}

fn reconcile_uncertain_renderer_submit(
    worker: &ManagedCaptureWorker,
    pending: &mut PendingCapture,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if !pending.renderer_submit_uncertain {
        return;
    }
    let Some(key) = pending.renderer_key else {
        pending.renderer_submit_uncertain = false;
        pending.set_failure(CaptureFailure::StaleIdentity);
        return;
    };
    match worker.try_status(key) {
        Ok(_) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_submitted = true;
        }
        Err(ManagedCaptureWorkerError::Busy) => {}
        Err(ManagedCaptureWorkerError::WrongKey | ManagedCaptureWorkerError::InvalidState) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_submitted = false;
            if let Some(request_failure) = expected_request_termination(pending, stopping) {
                pending.set_failure(request_failure);
            } else {
                pending.set_failure(CaptureFailure::ReadbackFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_renderer_submit_reconcile_failed",
                    "renderer could not reconcile an interrupted request handoff",
                );
            }
        }
        Err(ManagedCaptureWorkerError::Detached) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_submitted = false;
            pending.set_failure(CaptureFailure::Detached);
        }
        Err(ManagedCaptureWorkerError::Poisoned) => {
            pending.renderer_submit_uncertain = false;
            pending.renderer_submitted = false;
            pending.set_failure(CaptureFailure::ReadbackFailed);
            report_failure_once(
                reported_failure,
                failure,
                "capture_renderer_worker_poisoned",
                "renderer capture worker state was poisoned during reconciliation",
            );
        }
    }
}

fn reconcile_uncertain_encode_submit(
    pipeline: &CaptureCpuWorkerPort,
    pending: &mut PendingCapture,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if !pending.encode_submit_uncertain {
        return;
    }
    match pipeline.try_encode_job_pending(pending.request.id(), pending.stage_key) {
        Ok(true) => {
            pending.encode_submit_uncertain = false;
            pending.encode_in_flight = true;
        }
        Ok(false) => {
            pending.encode_submit_uncertain = false;
            pending.encode_in_flight = false;
            if let Some(request_failure) = expected_request_termination(pending, stopping) {
                pending.set_failure(request_failure);
            } else {
                pending.set_failure(CaptureFailure::ArtifactFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_encoder_submit_lost",
                    "an interrupted encoder handoff was not present in its keyed lane",
                );
            }
        }
        Err(crate::capture::CaptureEncoderError::Busy) => {}
        Err(crate::capture::CaptureEncoderError::Stopped) => {
            pending.encode_submit_uncertain = false;
            pending.encode_in_flight = false;
            if let Some(request_failure) = expected_request_termination(pending, stopping) {
                pending.set_failure(request_failure);
            } else {
                pending.set_failure(CaptureFailure::ArtifactFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_encoder_unavailable",
                    "capture encoder stopped while an accepted handoff was unresolved",
                );
            }
        }
        Err(crate::capture::CaptureEncoderError::Poisoned) => {
            pending.encode_submit_uncertain = false;
            pending.encode_in_flight = false;
            pending.set_failure(CaptureFailure::ArtifactFailed);
            report_failure_once(
                reported_failure,
                failure,
                "capture_encoder_poisoned",
                "capture encoder state was poisoned during reconciliation",
            );
        }
    }
}

fn take_encoder_result(
    ports: &mut CpuCapturePorts,
    pending: &mut Option<PendingCapture>,
    stopping: bool,
    reported_failure: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    let Some(pending) = pending.as_mut() else {
        return;
    };
    if !pending.encode_in_flight || pending.encode_result.is_some() {
        return;
    }
    match ports.pipeline.try_take_encode_result() {
        Ok(Some(result)) => {
            pending.encode_in_flight = false;
            // Install the owned result before interpreting it. If result validation or compact
            // completion construction unwinds, the encoder lane has already transferred its
            // only copy, so the durable pending state must retain it across the catch boundary.
            pending.encode_result = Some(result);
        }
        Ok(None) | Err(crate::capture::CaptureEncoderError::Busy) => {}
        Err(crate::capture::CaptureEncoderError::Stopped) => {
            pending.encode_in_flight = false;
            if let Some(request_failure) = expected_request_termination(pending, stopping) {
                pending.set_failure(request_failure);
            } else {
                pending.set_failure(CaptureFailure::ArtifactFailed);
                report_failure_once(
                    reported_failure,
                    failure,
                    "capture_encoder_result_unavailable",
                    "capture encoder stopped before returning an accepted result",
                );
            }
        }
        Err(crate::capture::CaptureEncoderError::Poisoned) => {
            pending.encode_in_flight = false;
            pending.set_failure(CaptureFailure::ArtifactFailed);
            report_failure_once(
                reported_failure,
                failure,
                "capture_encoder_result_poisoned",
                "capture encoder result lane was poisoned",
            );
        }
    }

    let Some(result) = pending.encode_result.as_ref() else {
        return;
    };
    if pending.completion.is_some() || pending.terminal_failure.is_some() {
        return;
    }
    if result.id() != pending.request.id() || result.stage_key() != pending.stage_key {
        pending.set_failure(CaptureFailure::StaleIdentity);
        report_failure_once(
            reported_failure,
            failure,
            "capture_encoder_result_mismatch",
            "capture encoder returned a result for a different request",
        );
        return;
    }
    match result.result() {
        Ok(metadata)
            if pending
                .selected_publication
                .as_ref()
                .is_some_and(|publication| metadata.publication() == publication)
                && Some(metadata.present_sequence()) == pending.selected_present_sequence =>
        {
            if pending.request_is_current() {
                pending.completion = Some(CaptureCompletion::Captured(Box::new(metadata.clone())));
            } else {
                pending.set_failure(pending.current_request_failure());
            }
        }
        Ok(_) => pending.set_failure(CaptureFailure::StaleIdentity),
        Err(error) => pending.set_failure(error),
    }
}

fn finish_completion(pending: &mut PendingCapture) {
    // Once a compact completion has been built, responder Busy retries must preserve that exact
    // result even if the capture deadline elapses while the actor's reserved slot is contended.
    if pending.completion.is_some() {
        return;
    }
    if pending.terminal_failure.is_none() {
        if pending.request.is_cancelled() {
            pending.set_failure(CaptureFailure::Cancelled);
        } else if pending.request.deadline() <= Instant::now() {
            pending.set_failure(CaptureFailure::TimedOut);
        }
    }
    if let Some(failure) = pending.terminal_failure {
        pending.completion = Some(CaptureCompletion::Failed(failure));
    }
}

fn pending_is_response_ready(pending: &PendingCapture) -> bool {
    pending.completion.is_some()
        && pending.renderer_request.is_none()
        && !pending.renderer_submit_uncertain
        && !pending.renderer_submitted
        && pending.readback.is_none()
        && pending.cleanup.is_none()
        && pending.pixels.is_none()
        && pending.encode_job.is_none()
        && !pending.encode_submit_uncertain
        && !pending.encode_in_flight
}

pub(super) fn allocate_key(next: &mut u64) -> Option<ManagedCaptureKey> {
    let key = ManagedCaptureKey::new(*next)?;
    *next = (*next).checked_add(1).unwrap_or(0);
    Some(key)
}

fn report_failure_once(
    reported: &mut bool,
    failure: &mut Option<CpuFailure>,
    code: &'static str,
    message: &str,
) {
    if !*reported {
        let candidate = CpuFailure {
            code,
            message: bounded_message(message.to_owned(), 2_048),
        };
        failure.get_or_insert(candidate);
        *reported = true;
    }
}

impl PendingCapture {
    fn new(routed: RoutedCapture) -> Self {
        Self {
            request: routed.request,
            stage_key: routed.stage_key,
            renderer_key: None,
            renderer_request: None,
            renderer_submitted: false,
            renderer_submit_uncertain: false,
            cancel_sent: false,
            readback: None,
            cleanup: None,
            pixels: None,
            selected_publication: None,
            selected_present_sequence: None,
            encode_job: None,
            encode_in_flight: false,
            encode_submit_uncertain: false,
            encode_result: None,
            completion: None,
            terminal_failure: None,
        }
    }

    fn create_renderer_request(&mut self, key: ManagedCaptureKey) -> Result<(), CaptureFailure> {
        let source = self.request.source();
        if source.session() != self.request.id().session() {
            return Err(CaptureFailure::StaleIdentity);
        }
        let renderer_source = ManagedCaptureSource::new(
            self.stage_key,
            source.session(),
            source.project_generation(),
            source.source_digest(),
        )
        .map_err(|_| CaptureFailure::StaleIdentity)?;
        let renderer_request = ManagedCaptureRequest::new(
            key,
            renderer_source,
            self.request.at_least_tick(),
            self.request.deadline(),
        )
        .map_err(|_| CaptureFailure::TimedOut)?;
        self.renderer_key = Some(key);
        self.renderer_request = Some(renderer_request);
        Ok(())
    }

    fn request_is_current(&self) -> bool {
        !self.request.is_cancelled() && self.request.deadline() > Instant::now()
    }

    fn current_request_failure(&self) -> CaptureFailure {
        if self.request.is_cancelled() {
            CaptureFailure::Cancelled
        } else {
            CaptureFailure::TimedOut
        }
    }

    fn set_failure(&mut self, failure: CaptureFailure) {
        if self.terminal_failure.is_none() {
            self.terminal_failure = Some(failure);
        }
    }

    fn move_readback_to_cleanup(&mut self) {
        if let Some(readback) = self.readback.take() {
            self.cleanup = Some(readback.into_cleanup());
        }
    }

    fn validate_binding(&self, binding: &CapturePublicationBinding) -> Result<(), CaptureFailure> {
        let publication = binding.publication();
        let presented = self
            .readback
            .as_ref()
            .expect("binding validation is performed before readback")
            .presented();
        let source = self.request.source();
        if binding.stage_key() != self.stage_key
            || publication.session() != source.session()
            || publication.session() != self.request.id().session()
            || publication.project_generation() != source.project_generation()
            || publication.source_digest() != source.source_digest()
            || publication.tick() < self.request.at_least_tick()
            || presented.key() != binding.stage_key()
            || presented.session() != publication.session()
            || presented.tick() != publication.tick()
            || presented.publication_sequence() != publication.publication_sequence()
            || presented.authority_snapshot_sequence()
                != Some(publication.authority_snapshot_sequence())
            || presented.project_generation() != publication.project_generation()
            || presented.source_digest() != publication.source_digest()
        {
            return Err(
                if publication.project_generation() != source.project_generation() {
                    CaptureFailure::StaleGeneration
                } else {
                    CaptureFailure::StaleIdentity
                },
            );
        }
        Ok(())
    }
}

fn map_renderer_state(state: ManagedCaptureState) -> CaptureHandoffState {
    match state {
        ManagedCaptureState::Starting => CaptureHandoffState::Starting,
        ManagedCaptureState::Ready(info) => {
            let format = map_surface_format(info.surface_format());
            match CaptureReadyInfo::new(
                format,
                info.max_width(),
                info.max_height(),
                info.max_pixels(),
            ) {
                Ok(info) => CaptureHandoffState::Ready(info),
                Err(_) => CaptureHandoffState::Unsupported(
                    CaptureUnsupportedReason::SurfaceCopyUnavailable,
                ),
            }
        }
        ManagedCaptureState::Unsupported(reason) => {
            CaptureHandoffState::Unsupported(match reason {
                ManagedCaptureUnsupportedReason::SurfaceCopyUnavailable => {
                    CaptureUnsupportedReason::SurfaceCopyUnavailable
                }
                ManagedCaptureUnsupportedReason::UnsupportedSurfaceFormat => {
                    CaptureUnsupportedReason::UnsupportedSurfaceFormat
                }
                ManagedCaptureUnsupportedReason::CaptureDisabled => {
                    CaptureUnsupportedReason::CaptureDisabled
                }
            })
        }
        ManagedCaptureState::Detached => CaptureHandoffState::Detached,
    }
}

fn map_surface_format(format: ManagedCaptureSurfaceFormat) -> CaptureSurfaceFormat {
    match format {
        ManagedCaptureSurfaceFormat::Rgba8Unorm => CaptureSurfaceFormat::Rgba8Unorm,
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb => CaptureSurfaceFormat::Rgba8UnormSrgb,
        ManagedCaptureSurfaceFormat::Bgra8Unorm => CaptureSurfaceFormat::Bgra8Unorm,
        ManagedCaptureSurfaceFormat::Bgra8UnormSrgb => CaptureSurfaceFormat::Bgra8UnormSrgb,
    }
}

fn capture_view(
    presented: &daedalus_renderer::ManagedPresentedView,
) -> Result<CaptureView, daedalus_runtime::CaptureMetadataError> {
    let view = presented.view();
    let camera = view.camera();
    let projection = match view.projection() {
        RenderProjection::Perspective => CaptureProjection::Perspective,
        RenderProjection::Orthographic { vertical_size } => {
            CaptureProjection::Orthographic { vertical_size }
        }
    };
    CaptureView::new(
        presented.physical_width(),
        presented.physical_height(),
        camera.position,
        camera.target,
        camera.up,
        camera.vertical_fov_radians,
        camera.near,
        camera.far,
        projection,
        *presented.view_projection(),
    )
}

fn map_submit_failure(error: ManagedCaptureSubmitError) -> CaptureFailure {
    match error {
        ManagedCaptureSubmitError::Busy | ManagedCaptureSubmitError::NotReady => {
            CaptureFailure::ReadbackFailed
        }
        ManagedCaptureSubmitError::Unsupported => CaptureFailure::Unsupported,
        ManagedCaptureSubmitError::Detached | ManagedCaptureSubmitError::WorkerUnavailable => {
            CaptureFailure::Detached
        }
        ManagedCaptureSubmitError::InvalidRequest
        | ManagedCaptureSubmitError::KeyReused
        | ManagedCaptureSubmitError::Poisoned => CaptureFailure::ReadbackFailed,
    }
}

fn map_renderer_failure(failure: ManagedCaptureFailure) -> CaptureFailure {
    match failure {
        ManagedCaptureFailure::Cancelled => CaptureFailure::Cancelled,
        ManagedCaptureFailure::DeadlineElapsed => CaptureFailure::TimedOut,
        ManagedCaptureFailure::StaleSource => CaptureFailure::StaleIdentity,
        ManagedCaptureFailure::Unsupported => CaptureFailure::Unsupported,
        ManagedCaptureFailure::TooLarge | ManagedCaptureFailure::AllocationFailed => {
            CaptureFailure::TooLarge
        }
        ManagedCaptureFailure::CopyFailed
        | ManagedCaptureFailure::MapFailed
        | ManagedCaptureFailure::DevicePollFailed
        | ManagedCaptureFailure::CounterExhausted => CaptureFailure::ReadbackFailed,
    }
}

fn map_readback_error(error: ManagedCaptureReadbackError) -> CaptureFailure {
    match error {
        ManagedCaptureReadbackError::Cancelled => CaptureFailure::Cancelled,
        ManagedCaptureReadbackError::DeadlineElapsed => CaptureFailure::TimedOut,
        ManagedCaptureReadbackError::StaleSource | ManagedCaptureReadbackError::InvalidState => {
            CaptureFailure::StaleIdentity
        }
        ManagedCaptureReadbackError::TooLarge => CaptureFailure::TooLarge,
        ManagedCaptureReadbackError::AllocationFailed => CaptureFailure::TooLarge,
        ManagedCaptureReadbackError::UnsupportedFormat => CaptureFailure::Unsupported,
        ManagedCaptureReadbackError::CopyFailed
        | ManagedCaptureReadbackError::MapFailed
        | ManagedCaptureReadbackError::DevicePollFailed
        | ManagedCaptureReadbackError::CounterExhausted
        | ManagedCaptureReadbackError::Poisoned
        | ManagedCaptureReadbackError::InvalidMappedRange => CaptureFailure::ReadbackFailed,
        ManagedCaptureReadbackError::Busy => CaptureFailure::ReadbackFailed,
    }
}

fn expected_request_termination(
    pending: &PendingCapture,
    stopping: bool,
) -> Option<CaptureFailure> {
    if pending.request.is_cancelled() {
        Some(CaptureFailure::Cancelled)
    } else if pending.request.deadline() <= Instant::now() {
        Some(CaptureFailure::TimedOut)
    } else if stopping {
        Some(CaptureFailure::Detached)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureCpuWorkerPort, CaptureEncoderState, CapturePipelineOwner};
    use daedalus_renderer::ManagedStageKey;
    use daedalus_runtime::{CaptureProjection, CaptureSurfaceFormat, CaptureView, WaitStage};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn renderer_capture_keys_are_channel_monotonic_and_exhaust_without_wrapping() {
        let mut next = 1;
        let first_client_key = allocate_key(&mut next).expect("first renderer key");
        let second_client_key = allocate_key(&mut next).expect("second renderer key");
        assert!(second_client_key.get() > first_client_key.get());
        assert_eq!(first_client_key.get(), 1);
        assert_eq!(second_client_key.get(), 2);

        next = u64::MAX;
        let last = allocate_key(&mut next).expect("last representable key");
        assert_eq!(last.get(), u64::MAX);
        assert!(allocate_key(&mut next).is_none());
    }

    #[test]
    fn reporting_capture_failure_preserves_an_existing_owner_failure() {
        let mut pending_failure = Some(CpuFailure {
            code: "existing_cpu_failure",
            message: "first owner failure".to_owned(),
        });
        let mut reported = false;

        report_failure_once(
            &mut reported,
            &mut pending_failure,
            "capture_failure",
            "capture-specific failure",
        );

        assert!(reported);
        let failure = pending_failure.expect("first bounded failure remains pending");
        assert_eq!(failure.code, "existing_cpu_failure");
        assert_eq!(failure.message, "first owner failure");
    }

    #[test]
    fn encoder_submit_panic_before_acceptance_reconciles_to_no_work() {
        let mut fixture = crate::capture::test_support::RuntimeCaptureFixture::new();
        let routed = routed_request(&mut fixture);
        let session = routed.request.id().session();
        let mut pipeline_owner = start_encoder(session);
        let mut ports = capture_ports(&mut pipeline_owner);
        wait_encoder_ready(&ports.pipeline);

        let mut pending = PendingCapture::new(routed);
        pending.renderer_key = ManagedCaptureKey::new(1);
        pending.selected_publication = Some(fixture.publication().clone());
        pending.selected_present_sequence = Some(1);
        pending.encode_job = Some(encode_job(&pending, fixture.publication()));
        pending.encode_in_flight = true;

        // Model unwinding after the caller moved its only Box into the handoff expression but
        // before the encoder lane accepted it. The same production uncertainty wrapper is used.
        let job = pending
            .encode_job
            .take()
            .expect("prepared real-request encoder job");
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            with_uncertain_handoff(&mut pending.encode_submit_uncertain, || {
                drop(job);
                panic!("injected before encoder-lane acceptance");
            });
        }));
        assert!(unwind.is_err());
        assert!(pending.encode_submit_uncertain);

        let reconcile_failure = reconcile_until_resolved(&ports.pipeline, &mut pending, false);
        assert!(!pending.encode_submit_uncertain);
        assert!(!pending.encode_in_flight);
        assert_eq!(
            pending.terminal_failure,
            Some(CaptureFailure::ArtifactFailed)
        );
        assert!(
            reconcile_failure.is_some(),
            "lost pre-acceptance ownership is reported as an internal handoff failure"
        );
        assert!(
            !encoder_job_pending(&ports.pipeline, pending.request.id(), pending.stage_key),
            "a pre-acceptance unwind leaves no hidden encoder job"
        );

        finish_runtime_request(
            &mut fixture,
            &mut pending,
            CaptureCompletion::Failed(CaptureFailure::ArtifactFailed),
        );
        drop(pending);
        stop_encoder(&mut ports.pipeline);
        drop(ports);
        drop(pipeline_owner);
    }

    #[test]
    fn encoder_submit_panic_after_acceptance_reconciles_and_keeps_the_result() {
        let mut fixture = crate::capture::test_support::RuntimeCaptureFixture::new();
        let routed = routed_request(&mut fixture);
        let session = routed.request.id().session();
        let mut pipeline_owner = start_encoder(session);
        let mut ports = capture_ports(&mut pipeline_owner);
        wait_encoder_ready(&ports.pipeline);

        let mut pending = PendingCapture::new(routed);
        pending.renderer_key = ManagedCaptureKey::new(1);
        pending.selected_publication = Some(fixture.publication().clone());
        pending.selected_present_sequence = Some(1);
        pending.encode_job = Some(encode_job(&pending, fixture.publication()));
        pending.encode_in_flight = true;

        // The real encoder accepts this job. The injected panic occurs before the caller can
        // observe `Ok(())`, so the uncertainty flag must survive and keyed lane status must find
        // the queued/working/result job rather than discarding it as a failed submission.
        let mut job = pending
            .encode_job
            .take()
            .expect("prepared real-request encoder job");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut returned_job = None;
            let mut outcome = None;
            let unwind = catch_unwind(AssertUnwindSafe(|| {
                with_uncertain_handoff(&mut pending.encode_submit_uncertain, || {
                    match ports.pipeline.try_submit_encode(job) {
                        Ok(()) => {
                            outcome = Some("accepted");
                            panic!("injected after encoder-lane acceptance");
                        }
                        Err((returned, crate::capture::CaptureEncoderError::Busy)) => {
                            returned_job = Some(returned);
                            outcome = Some("busy");
                        }
                        Err((returned, error)) => {
                            returned_job = Some(returned);
                            outcome = Some("rejected");
                            panic!("encoder rejected the test job: {error}");
                        }
                    }
                });
            }));
            match outcome {
                Some("busy") => {
                    assert!(unwind.is_ok(), "ordinary Busy must return the same job");
                    assert!(
                        !pending.encode_submit_uncertain,
                        "completed Busy admission does not leave an uncertain handoff"
                    );
                    job = returned_job.expect("Busy preserves the original encoder job");
                    assert!(
                        Instant::now() < deadline,
                        "encoder remained busy until timeout"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Some("accepted") => {
                    assert!(
                        unwind.is_err(),
                        "the injected unwind must happen after actual acceptance"
                    );
                    break;
                }
                Some("rejected") => panic!("encoder rejected the test job"),
                _ => panic!("encoder submission produced no outcome"),
            }
        }
        assert!(pending.encode_submit_uncertain);

        let reconcile_failure = reconcile_until_resolved(&ports.pipeline, &mut pending, false);
        assert!(!pending.encode_submit_uncertain);
        assert!(pending.encode_in_flight);
        assert_eq!(pending.terminal_failure, None);
        assert!(reconcile_failure.is_none());
        assert!(pending.encode_job.is_none());

        let mut pending_slot = Some(pending);
        let mut reported_failure = false;
        let mut failure = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending_slot
            .as_ref()
            .is_some_and(|pending| pending.encode_result.is_none())
        {
            take_encoder_result(
                &mut ports,
                &mut pending_slot,
                false,
                &mut reported_failure,
                &mut failure,
            );
            assert!(
                Instant::now() < deadline,
                "accepted encoder job did not return"
            );
            if pending_slot
                .as_ref()
                .is_some_and(|pending| pending.encode_result.is_none())
            {
                thread::sleep(Duration::from_millis(2));
            }
        }
        let mut pending = pending_slot.expect("capture remains pending through result transfer");
        assert!(pending.encode_result.as_ref().unwrap().result().is_ok());
        assert!(matches!(
            pending.completion.as_ref(),
            Some(CaptureCompletion::Captured(_))
        ));
        assert!(failure.is_none());

        // The fixture only validates real actor request/capability ownership and the encoder
        // lane. It intentionally does not claim that this synthetic CPU view was presented.
        finish_runtime_request(
            &mut fixture,
            &mut pending,
            CaptureCompletion::Failed(CaptureFailure::Cancelled),
        );
        drop(pending);
        stop_encoder(&mut ports.pipeline);
        drop(ports);
        drop(pipeline_owner);
    }

    #[test]
    fn encoder_stop_during_host_shutdown_is_capture_failure_not_owner_failure() {
        let mut fixture = crate::capture::test_support::RuntimeCaptureFixture::new();
        let routed = routed_request(&mut fixture);
        let session = routed.request.id().session();
        let mut pipeline_owner = start_encoder(session);
        let mut ports = capture_ports(&mut pipeline_owner);
        wait_encoder_ready(&ports.pipeline);

        let mut pending = PendingCapture::new(routed);
        pending.renderer_key = ManagedCaptureKey::new(1);
        pending.selected_publication = Some(fixture.publication().clone());
        pending.selected_present_sequence = Some(1);
        pending.encode_job = Some(encode_job(&pending, fixture.publication()));

        // This models the host stop becoming visible to the CPU owner in the same poll where
        // the encoder has already stopped. Submission must terminalize the request without
        // overwriting the host's ordinary shutdown status with a CPU-owner failure.
        stop_encoder(&mut ports.pipeline);
        let mut reported_failure = false;
        let mut owner_failure = None;
        submit_encode_job(
            &ports.pipeline,
            &mut pending,
            true,
            &mut reported_failure,
            &mut owner_failure,
        );
        assert_eq!(pending.terminal_failure, Some(CaptureFailure::Detached));
        assert!(!reported_failure);
        assert!(owner_failure.is_none());
        assert!(pending.encode_job.is_none());

        finish_runtime_request(
            &mut fixture,
            &mut pending,
            CaptureCompletion::Failed(CaptureFailure::Detached),
        );
        drop(pending);
        drop(ports);
        drop(pipeline_owner);
    }

    fn routed_request(
        fixture: &mut crate::capture::test_support::RuntimeCaptureFixture,
    ) -> RoutedCapture {
        RoutedCapture {
            request: fixture
                .take_request()
                .expect("fixture owns its actual actor request"),
            stage_key: ManagedStageKey::INITIAL,
        }
    }

    fn start_encoder(session: daedalus_core::SessionId) -> CapturePipelineOwner {
        crate::capture::start_capture_pipeline(session, 3)
            .expect("capture encoder starts for a real runtime request")
    }

    fn capture_ports(pipeline_owner: &mut CapturePipelineOwner) -> CpuCapturePorts {
        let pipeline = pipeline_owner
            .take_cpu_worker_port()
            .expect("encoder owner transfers its one CPU port");
        let (_window_client, renderer_client, _server) =
            daedalus_renderer::managed_packet_window_capture_channel();
        let renderer_worker = renderer_client
            .register_worker()
            .expect("test registers the non-started renderer worker");
        CpuCapturePorts {
            pipeline,
            renderer_client,
            renderer_worker,
        }
    }

    fn wait_encoder_ready(pipeline: &CaptureCpuWorkerPort) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pipeline.encoder_status() {
                CaptureEncoderState::Ready => return,
                CaptureEncoderState::Failed(error) => {
                    panic!("capture encoder failed during test setup: {error}");
                }
                CaptureEncoderState::Starting | CaptureEncoderState::Stopping => {}
                CaptureEncoderState::Stopped => panic!("encoder stopped before test setup"),
            }
            assert!(Instant::now() < deadline, "encoder did not become ready");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn encode_job(
        pending: &PendingCapture,
        publication: &ProjectPublicationIdentity,
    ) -> Box<CaptureEncodeJob> {
        let request = &pending.request;
        let view = CaptureView::new(
            2,
            2,
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            1.0,
            0.1,
            100.0,
            CaptureProjection::Perspective,
            [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
            ],
        )
        .expect("test's 2x2 encoder metadata is valid");
        CaptureEncodeJob::new(
            request.id(),
            pending.stage_key,
            publication.clone(),
            pending.selected_present_sequence.unwrap(),
            view,
            CaptureSurfaceFormat::Rgba8UnormSrgb,
            CapturePixelEncoding::SrgbEncoded,
            request.artifact().downgrade(),
            request.work_lease().clone(),
            vec![
                64, 128, 192, 255, 16, 32, 48, 255, 192, 128, 64, 255, 255, 255, 255, 255,
            ],
        )
        .expect("encoder job keeps the actual runtime request's artifact work alive")
    }

    fn finish_runtime_request(
        fixture: &mut crate::capture::test_support::RuntimeCaptureFixture,
        pending: &mut PendingCapture,
        completion: CaptureCompletion,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pending.request.responder_mut().try_complete(&completion) {
                Ok(())
                | Err(CaptureResponderError::AlreadyCompleted | CaptureResponderError::Stopped) => {
                    break;
                }
                Err(CaptureResponderError::Busy) => {
                    assert!(Instant::now() < deadline, "capture responder stayed busy");
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
        let progress = fixture
            .ticket_mut()
            .wait_for(WaitStage::Terminal, Duration::from_secs(5))
            .expect("real runtime capture request reaches terminal");
        assert!(progress.terminal().is_some());
        fixture
            .ticket_mut()
            .acknowledge()
            .expect("capture ticket result is acknowledged");
    }

    fn stop_encoder(pipeline: &mut CaptureCpuWorkerPort) {
        pipeline.request_encoder_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pipeline.try_finish_encoder() {
                crate::capture::CaptureEncoderShutdown::Stopped => return,
                crate::capture::CaptureEncoderShutdown::Failed(error) => {
                    panic!("encoder cleanup failed: {error}");
                }
                crate::capture::CaptureEncoderShutdown::Stopping => {}
            }
            assert!(Instant::now() < deadline, "capture encoder did not stop");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn reconcile_until_resolved(
        pipeline: &CaptureCpuWorkerPort,
        pending: &mut PendingCapture,
        stopping: bool,
    ) -> Option<CpuFailure> {
        let mut reported_failure = false;
        let mut failure = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.encode_submit_uncertain {
            reconcile_uncertain_encode_submit(
                pipeline,
                pending,
                stopping,
                &mut reported_failure,
                &mut failure,
            );
            assert!(
                Instant::now() < deadline,
                "encoder handoff reconciliation stayed Busy"
            );
            if pending.encode_submit_uncertain {
                thread::sleep(Duration::from_millis(2));
            }
        }
        failure
    }

    fn encoder_job_pending(
        pipeline: &CaptureCpuWorkerPort,
        id: daedalus_runtime::CaptureArtifactId,
        stage_key: daedalus_renderer::ManagedStageKey,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pipeline.try_encode_job_pending(id, stage_key) {
                Ok(pending) => return pending,
                Err(crate::capture::CaptureEncoderError::Busy) => {
                    assert!(Instant::now() < deadline, "encoder lane status stayed Busy");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("encoder lane status failed: {error}"),
            }
        }
    }
}
