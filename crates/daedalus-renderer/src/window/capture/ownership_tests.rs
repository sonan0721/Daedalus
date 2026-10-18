//! CPU-only ownership regressions for the live-capture slot.
//!
//! The state-injection cases below deliberately create metadata-only cell states. They do not
//! create a `wgpu::Buffer`, submit a copy, complete a queue fence, or claim pixel evidence.

use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use daedalus_core::{SessionId, Tick};

use super::tests::actual_cpu_frame;
use super::{
    CapturePhase, ManagedCaptureCell, ManagedCaptureCleanup, ManagedCaptureClient,
    ManagedCaptureFailure, ManagedCaptureKey, ManagedCaptureReadback, ManagedCaptureReadyInfo,
    ManagedCaptureRequest, ManagedCaptureReturn, ManagedCaptureShared, ManagedCaptureSource,
    ManagedCaptureSubmitError, ManagedCaptureSurfaceFormat, ManagedCaptureWorker,
};
use crate::window::managed::{ManagedPresentedView, ManagedStageKey};
use crate::{Camera, DirectionalLight, RenderPacket};

const SESSION: SessionId = SessionId::from_raw(43);

fn key(raw: u64) -> ManagedCaptureKey {
    ManagedCaptureKey::new(raw).expect("nonzero capture key")
}

fn source(stage_key: ManagedStageKey, generation: u64, digest_seed: u64) -> ManagedCaptureSource {
    ManagedCaptureSource::new(
        stage_key,
        SESSION,
        generation,
        &format!("{digest_seed:064x}"),
    )
    .expect("valid bounded source identity")
}

fn request(key: ManagedCaptureKey, source: ManagedCaptureSource) -> Box<ManagedCaptureRequest> {
    ManagedCaptureRequest::new(
        key,
        source,
        Tick(7),
        Instant::now() + Duration::from_secs(30),
    )
    .expect("future bounded fixture deadline")
}

fn ready_capture() -> (
    ManagedCaptureClient,
    Arc<ManagedCaptureShared>,
    ManagedCaptureWorker,
) {
    let (client, shared) = ManagedCaptureClient::new();
    let worker = client
        .register_worker()
        .expect("register before window start");
    shared
        .try_set_ready(
            ManagedCaptureReadyInfo::new(ManagedCaptureSurfaceFormat::Rgba8UnormSrgb, 64, 64, 4096)
                .expect("finite small readiness limits"),
        )
        .expect("resolve readiness once");
    (client, shared, worker)
}

fn reject_busy_same_request(
    client: &ManagedCaptureClient,
    request: Box<ManagedCaptureRequest>,
) -> Box<ManagedCaptureRequest> {
    let original_address = &*request as *const ManagedCaptureRequest;
    let original_key = request.key();
    let original_source = request.source().clone();
    let (request, error) = match client.try_submit(request) {
        Ok(()) => panic!("request unexpectedly admitted while capture cell was unavailable"),
        Err((request, error)) => (request, error),
    };
    assert_eq!(error, ManagedCaptureSubmitError::Busy);
    assert!(std::ptr::eq(original_address, &*request));
    assert_eq!(request.key(), original_key);
    assert_eq!(request.source(), &original_source);
    request
}

fn take_failed_return(
    worker: &ManagedCaptureWorker,
    expected_key: ManagedCaptureKey,
    expected_failure: ManagedCaptureFailure,
) -> ManagedCaptureCleanup {
    match worker
        .try_take_return()
        .expect("nonblocking worker read succeeds")
        .expect("terminal failure is returned exactly once")
    {
        ManagedCaptureReturn::Failed {
            key: actual_key,
            failure,
            cleanup,
        } => {
            assert_eq!(actual_key, expected_key);
            assert_eq!(failure, expected_failure);
            cleanup
        }
        other => panic!("expected failed no-copy return, got {other:?}"),
    }
}

fn take_window_request(
    shared: &ManagedCaptureShared,
    expected_key: ManagedCaptureKey,
    expected_source: &ManagedCaptureSource,
) {
    let accepted = shared
        .try_take_request()
        .expect("request slot is not contended")
        .expect("accepted request remains owned until consumed");
    assert_eq!(accepted.key(), expected_key);
    assert_eq!(accepted.source(), expected_source);
}

fn presented(source: &ManagedCaptureSource, publication: u64) -> ManagedPresentedView {
    let packet = RenderPacket::new(
        source.session(),
        Tick(publication),
        publication,
        source.project_generation(),
        source.source_digest().to_owned(),
        Camera::default(),
        DirectionalLight::default(),
        [0.1; 3],
        [0.02, 0.03, 0.04, 1.0],
        Vec::new(),
    );
    ManagedPresentedView::new(
        source.stage_key(),
        &packet,
        publication,
        64,
        64,
        packet.view(),
    )
    .expect("synthetic fixture view is valid metadata")
}

fn mark_failed_without_copy(
    cell: &ManagedCaptureCell,
    key: ManagedCaptureKey,
    failure: ManagedCaptureFailure,
) {
    // This is the CPU-side terminal path before buffer allocation or queue submission.
    let mut ui = cell
        .try_lock_for_ui(key)
        .expect("reserved cell belongs to the UI request");
    ui.mark_failed(failure);
}

#[test]
fn busy_submit_returns_same_box_and_same_key_can_be_retried() {
    let (client, shared, worker) = ready_capture();
    let source = source(ManagedStageKey::INITIAL, 1, 17);
    let request = request(key(1), source.clone());

    // Hold only the cell mutex to force the exact try-lock Busy branch.
    let held_cell = shared.cell.inner.lock().expect("fixture cell lock");
    let request = reject_busy_same_request(&client, request);
    drop(held_cell);

    client
        .try_submit(request)
        .expect("Busy did not consume the request key or box");
    take_window_request(&shared, key(1), &source);
    assert_eq!(
        client.request_cancel(key(1)).expect("cancel before copy"),
        super::ManagedCaptureCancelStatus::Accepted
    );
    let mut cleanup = take_failed_return(&worker, key(1), ManagedCaptureFailure::Cancelled);
    assert!(
        cleanup
            .try_finish()
            .expect("no-copy cleanup is nonblocking")
    );
}

#[test]
fn pre_copy_cancel_holds_the_slot_until_return_cleanup_then_admits_key_two() {
    let (client, shared, worker) = ready_capture();
    let first_source = source(ManagedStageKey::INITIAL, 1, 23);
    client
        .try_submit(request(key(1), first_source.clone()))
        .expect("first request admitted");
    take_window_request(&shared, key(1), &first_source);
    assert_eq!(
        client
            .request_cancel(key(1))
            .expect("cancel before any copy"),
        super::ManagedCaptureCancelStatus::Accepted
    );

    let second_source = source(
        ManagedStageKey::INITIAL.checked_next().expect("next key"),
        2,
        29,
    );
    let second = reject_busy_same_request(&client, request(key(2), second_source.clone()));
    let mut cleanup = take_failed_return(&worker, key(1), ManagedCaptureFailure::Cancelled);
    let second = reject_busy_same_request(&client, second);
    assert!(
        cleanup
            .try_finish()
            .expect("pre-copy cleanup releases immediately")
    );

    client
        .try_submit(second)
        .expect("key two becomes admissible after terminal return cleanup");
    take_window_request(&shared, key(2), &second_source);
    assert_eq!(
        client
            .request_cancel(key(2))
            .expect("cancel second before copy"),
        super::ManagedCaptureCancelStatus::Accepted
    );
    let mut cleanup = take_failed_return(&worker, key(2), ManagedCaptureFailure::Cancelled);
    assert!(
        cleanup
            .try_finish()
            .expect("second no-copy cleanup releases")
    );
}

#[test]
fn pre_copy_failure_return_is_consumed_and_cleaned_before_reusing_the_cell() {
    let (client, shared, worker) = ready_capture();
    let first_source = source(ManagedStageKey::INITIAL, 1, 31);
    client
        .try_submit(request(key(1), first_source.clone()))
        .expect("first request admitted");
    take_window_request(&shared, key(1), &first_source);

    // Explicit no-buffer UI failure injection; it does not represent a GPU copy or fence.
    mark_failed_without_copy(&shared.cell, key(1), ManagedCaptureFailure::CopyFailed);
    let second_source = source(
        ManagedStageKey::INITIAL.checked_next().expect("next key"),
        2,
        37,
    );
    let second = reject_busy_same_request(&client, request(key(2), second_source.clone()));
    let mut cleanup = take_failed_return(&worker, key(1), ManagedCaptureFailure::CopyFailed);
    let second = reject_busy_same_request(&client, second);
    assert!(
        cleanup
            .try_finish()
            .expect("failure has no buffer to unmap")
    );

    client
        .try_submit(second)
        .expect("failed request no longer reserves the capture cell");
    take_window_request(&shared, key(2), &second_source);
    mark_failed_without_copy(&shared.cell, key(2), ManagedCaptureFailure::Cancelled);
    let mut cleanup = take_failed_return(&worker, key(2), ManagedCaptureFailure::Cancelled);
    assert!(
        cleanup
            .try_finish()
            .expect("second no-copy cleanup releases")
    );
}

#[test]
fn worker_detach_drains_an_unclaimed_no_copy_request_without_leaking_the_slot() {
    let (client, shared, worker) = ready_capture();
    assert!(matches!(
        client.register_worker(),
        Err(super::ManagedCaptureEndpointError::AlreadyRegistered)
    ));
    shared.mark_window_started();
    assert!(matches!(
        client.register_worker(),
        Err(super::ManagedCaptureEndpointError::Detached)
    ));

    let first_source = source(ManagedStageKey::INITIAL, 1, 41);
    client
        .try_submit(request(key(1), first_source.clone()))
        .expect("request admitted while worker is registered");
    drop(worker); // Signals detach/abandon; no GPU handle or copy exists.

    assert!(
        shared
            .try_take_request()
            .expect("window request drain is nonblocking")
            .is_none(),
        "a detached worker cannot consume the accepted request"
    );
    assert!(
        shared
            .cell
            .try_abandoned_cleanup()
            .expect("detached no-copy cleanup is nonblocking"),
        "the detached owner reclaims the abandoned reservation"
    );
    assert!(shared.cell.is_released());
    assert!(
        shared
            .try_take_request()
            .expect("request slot remains usable")
            .is_none()
    );
    let rejected = request(key(2), source(ManagedStageKey::INITIAL, 1, 43));
    let (rejected, error) = match client.try_submit(rejected) {
        Err((request, error)) => (request, error),
        Ok(()) => panic!("detached worker must not accept a new request"),
    };
    assert_eq!(error, ManagedCaptureSubmitError::WorkerUnavailable);
    assert_eq!(rejected.key(), key(2));
}

#[test]
fn map_pending_is_not_reported_as_readable_and_map_failure_can_release_the_cell() {
    let (client, shared, worker) = ready_capture();
    let capture_source = source(ManagedStageKey::INITIAL, 1, 47);
    client
        .try_submit(request(key(1), capture_source.clone()))
        .expect("request admitted");
    take_window_request(&shared, key(1), &capture_source);
    let view = presented(&capture_source, 8);
    let selected_frame = actual_cpu_frame();

    // Test-controlled metadata-only readback state: no Buffer, COPY, or queue completion.
    {
        let mut cell = shared
            .cell
            .try_lock_for_ui(key(1))
            .expect("cell key is reserved");
        cell.install_selected_frame(Arc::clone(&selected_frame))
            .expect("real CPU frame backs the synthetic submitted metadata");
        cell.begin_metadata_install()
            .expect("begin synthetic metadata install");
        cell.set_dimensions(2, 2, 256, 512, ManagedCaptureSurfaceFormat::Rgba8UnormSrgb);
        cell.mark_map_pending(key(1));
        cell.mark_submitted_metadata(key(1), view.clone())
            .expect("synthetic metadata transition to submitted");
    }
    drop(selected_frame);
    shared.cell.set_map_pending(key(1));

    let mut readback = match worker
        .try_take_return()
        .expect("worker polls without blocking")
    {
        Some(ManagedCaptureReturn::Submitted(readback)) => readback,
        Some(other) => panic!("expected submitted metadata handle, got {other:?}"),
        None => panic!("submitted metadata return is available"),
    };
    let selected_frame = readback.selected_frame().clone();
    assert!(
        selected_frame.upgrade().is_some(),
        "the worker can resolve the exact CPU frame before reading pixels"
    );
    assert_eq!(
        worker.try_status(key(1)).unwrap(),
        super::ManagedCaptureCellStatus::MapPending
    );
    assert!(
        readback
            .try_read_rgba8()
            .expect("pending map is not a terminal readback result")
            .is_none()
    );
    assert_eq!(
        worker.try_status(key(1)).unwrap(),
        super::ManagedCaptureCellStatus::MapPending
    );

    // Simulate the asynchronous map callback reporting failure; this is not device evidence.
    shared.cell.set_map_result(key(1), false);
    assert!(matches!(
        readback.try_read_rgba8(),
        Err(super::ManagedCaptureReadbackError::MapFailed)
    ));
    let mut cleanup = readback.into_cleanup();
    assert!(
        cleanup
            .try_finish()
            .expect("no-buffer failure cleanup releases")
    );
    assert_eq!(
        worker.try_status(key(1)).unwrap(),
        super::ManagedCaptureCellStatus::Released
    );
    assert!(
        selected_frame.upgrade().is_none(),
        "worker cleanup releases the cell-owned frame pin"
    );
}

#[test]
fn finished_old_readback_handles_cannot_mutate_a_reused_cell() {
    let (client, shared, worker) = ready_capture();
    let source_one = source(ManagedStageKey::INITIAL, 1, 53);
    client
        .try_submit(request(key(1), source_one.clone()))
        .expect("key one admitted");
    take_window_request(&shared, key(1), &source_one);
    let first_view = presented(&source_one, 9);
    let selected_frame = actual_cpu_frame();

    // A metadata-only Submitted return lets this test retain the selected identity. We then
    // explicitly advance the CPU fixture to Released; no map success or pixels are fabricated.
    {
        let mut cell = shared.cell.try_lock_for_ui(key(1)).unwrap();
        cell.install_selected_frame(Arc::clone(&selected_frame))
            .unwrap();
        cell.begin_metadata_install().unwrap();
        cell.set_dimensions(
            64,
            64,
            256,
            16_384,
            ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
        );
        cell.mark_map_pending(key(1));
        cell.mark_submitted_metadata(key(1), first_view.clone())
            .unwrap();
    }
    drop(selected_frame);
    let mut old = match worker.try_take_return().unwrap().unwrap() {
        ManagedCaptureReturn::Submitted(readback) => readback,
        other => panic!("expected first metadata return, got {other:?}"),
    };
    assert_eq!(old.presented(), &first_view);
    old.finished = true;
    {
        let mut inner = shared.cell.inner.lock().unwrap();
        inner.phase = CapturePhase::Released;
        inner.buffer = None;
        drop(inner.selected_frame.take());
    }
    shared.cell.clear_consumer_handle(key(1));

    let source_two = source(
        ManagedStageKey::INITIAL
            .checked_next()
            .expect("next stage key"),
        2,
        59,
    );
    client
        .try_submit(request(key(2), source_two.clone()))
        .expect("released cell admits key two while finished handle remains owned");
    take_window_request(&shared, key(2), &source_two);
    mark_failed_without_copy(&shared.cell, key(2), ManagedCaptureFailure::CopyFailed);
    let key_two_cleanup = take_failed_return(&worker, key(2), ManagedCaptureFailure::CopyFailed);
    client
        .request_cancel(key(2))
        .expect("current key cancellation is scoped to key two");
    let consumer_key_before = shared
        .cell
        .consumer_handle_key
        .load(std::sync::atomic::Ordering::Acquire);
    let cancel_key_before = shared
        .cell
        .cancel_key
        .load(std::sync::atomic::Ordering::Acquire);
    let abandon_key_before = shared
        .cell
        .abandon_key
        .load(std::sync::atomic::Ordering::Acquire);

    assert_eq!(
        old.presented(),
        &first_view,
        "old metadata remains immutable after reuse"
    );
    let already_finished_cleanup = old.into_cleanup();
    drop(already_finished_cleanup);

    // Separate stale Drop path: like `finished`, this is a CPU-only released-handle fixture.
    let mut stale_drop = ManagedCaptureReadback::new(
        Arc::clone(&shared.cell),
        key(1),
        first_view,
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
        Weak::new(),
    );
    stale_drop.finished = true;
    drop(stale_drop);

    assert_eq!(
        shared.cell.key.load(std::sync::atomic::Ordering::Acquire),
        key(2).get()
    );
    assert_eq!(
        shared
            .cell
            .consumer_handle_key
            .load(std::sync::atomic::Ordering::Acquire),
        consumer_key_before
    );
    assert_eq!(
        shared
            .cell
            .cancel_key
            .load(std::sync::atomic::Ordering::Acquire),
        cancel_key_before
    );
    assert_eq!(
        shared
            .cell
            .abandon_key
            .load(std::sync::atomic::Ordering::Acquire),
        abandon_key_before
    );
    assert_eq!(
        worker.try_status(key(2)).unwrap(),
        super::ManagedCaptureCellStatus::CleanupRequired
    );

    let mut key_two_cleanup = key_two_cleanup;
    assert!(
        key_two_cleanup
            .try_finish()
            .expect("key two cleanup still owns its cell")
    );
}
