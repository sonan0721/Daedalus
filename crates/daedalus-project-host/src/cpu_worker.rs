#[cfg(feature = "window")]
use std::sync::OnceLock;
use std::{
    collections::BTreeMap,
    ops::Deref,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use daedalus_content::scene::StableId;
use daedalus_core::EntityHandle;
#[cfg(feature = "window")]
use daedalus_renderer::{
    CpuFrameHandle, PacketRendererDrainStatus, PacketStageInput, RenderView, WindowGpuOwner,
};
use daedalus_renderer::{
    CpuGenerationId, PacketCpuOwner, PacketCpuStatus, PacketRenderError, SourcePin,
};
#[cfg(feature = "window")]
use daedalus_runtime::ProjectPublicationIdentity;
use daedalus_runtime::SessionControlClient;
use daedalus_runtime::{
    ProjectConsumerStageFailurePhase, ProjectSourceIdentity, ProjectSourceLease,
};

use crate::host::HostLifecycleSignal;

const REQUEST_CAPACITY: usize = 1;
const OWNER_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Capacity-one, acknowledged result lane. A result is not considered observed when it is merely
/// copied out of the slot: its delivery guard must be dropped before this lane accepts another
/// result. This keeps one canonical CPU result outstanding per lane and gives stop a bounded,
/// owner-thread cleanup path.
pub(super) struct ResultMailbox<T> {
    shared: Arc<ResultMailboxShared<T>>,
}

struct ResultMailboxShared<T> {
    value: Mutex<Option<T>>,
    delivered: AtomicBool,
    acknowledged: AtomicBool,
}

impl<T> Clone for ResultMailbox<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Default for ResultMailbox<T> {
    fn default() -> Self {
        Self {
            shared: Arc::new(ResultMailboxShared {
                value: Mutex::new(None),
                delivered: AtomicBool::new(false),
                acknowledged: AtomicBool::new(false),
            }),
        }
    }
}

impl<T> ResultMailbox<T> {
    fn refresh_acknowledgement(&self) {
        if self.shared.delivered.load(Ordering::Acquire)
            && self.shared.acknowledged.swap(false, Ordering::AcqRel)
        {
            self.shared.delivered.store(false, Ordering::Release);
        }
    }

    fn can_publish(&self) -> bool {
        self.refresh_acknowledgement();
        if self.shared.delivered.load(Ordering::Acquire) {
            return false;
        }
        match self.shared.value.try_lock() {
            Ok(value) => slot_is_available(&value, &self.shared.delivered),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => false,
        }
    }

    fn try_publish(&self, value: T) -> Result<(), T> {
        self.refresh_acknowledgement();
        if self.shared.delivered.load(Ordering::Acquire) {
            return Err(value);
        }
        match self.shared.value.try_lock() {
            Ok(mut slot) => publish_into_available_slot(&mut slot, &self.shared.delivered, value),
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => Err(value),
        }
    }

    pub(super) fn try_take(&self) -> Option<ResultDelivery<T>> {
        self.refresh_acknowledgement();
        if self.shared.delivered.load(Ordering::Acquire) {
            return None;
        }
        let value = match self.shared.value.try_lock() {
            Ok(mut slot) => {
                if self.shared.delivered.load(Ordering::Acquire) {
                    return None;
                }
                let value = slot.take()?;
                self.shared.delivered.store(true, Ordering::Release);
                value
            }
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return None,
        };
        Some(ResultDelivery {
            shared: Arc::clone(&self.shared),
            value: Some(value),
        })
    }

    fn clear_on_owner(&self) {
        self.refresh_acknowledgement();
        if self.shared.delivered.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut slot) = self.shared.value.try_lock() {
            drop(slot.take());
        }
    }

    fn is_fully_acknowledged(&self) -> bool {
        self.refresh_acknowledgement();
        if self.shared.delivered.load(Ordering::Acquire) {
            return false;
        }
        matches!(self.shared.value.try_lock(), Ok(value) if slot_is_available(&value, &self.shared.delivered))
    }

    fn has_external_observers(&self) -> bool {
        Arc::strong_count(&self.shared) > 1
    }
}

fn slot_is_available<T>(slot: &Option<T>, delivered: &AtomicBool) -> bool {
    !delivered.load(Ordering::Acquire) && slot.is_none()
}

fn publish_into_available_slot<T>(
    slot: &mut Option<T>,
    delivered: &AtomicBool,
    value: T,
) -> Result<(), T> {
    if !slot_is_available(slot, delivered) {
        return Err(value);
    }
    *slot = Some(value);
    Ok(())
}

/// Result payload plus an explicit acknowledgment guard. Dropping it first drops the payload,
/// then frees the lane, so the CPU owner cannot retire a backing while a delivered alias remains
/// queued for another result.
pub(super) struct ResultDelivery<T> {
    shared: Arc<ResultMailboxShared<T>>,
    value: Option<T>,
}

impl<T> ResultDelivery<T> {
    pub(super) fn take_payload(&mut self) -> Option<T> {
        self.value.take()
    }
}

impl<T> Deref for ResultDelivery<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
            .as_ref()
            .expect("result delivery payload was already transferred")
    }
}

impl<T> Drop for ResultDelivery<T> {
    fn drop(&mut self) {
        drop(self.value.take());
        self.shared.acknowledged.store(true, Ordering::Release);
    }
}

#[derive(Clone, Debug)]
pub(super) struct CpuFailure {
    pub code: &'static str,
    pub message: String,
}

pub(super) struct PrepareRequest {
    pub lease: ProjectSourceLease,
    pub bindings: Arc<BTreeMap<StableId, EntityHandle>>,
    pub cancelled: Arc<AtomicBool>,
}

#[cfg(feature = "window")]
pub(super) struct StageInputRequest {
    pub generation: CpuGenerationId,
    pub identity: ProjectSourceIdentity,
}

#[cfg(feature = "window")]
pub(super) struct LatestFrameRequest {
    pub generation: CpuGenerationId,
    pub identity: ProjectSourceIdentity,
    /// Optional explicit SDK override. `None` preserves the authored project camera/projection.
    pub view: Option<RenderView>,
    pub cancelled: Arc<AtomicBool>,
}

pub(super) struct RetireRequest {
    pub generation: CpuGenerationId,
}

pub(super) enum PrepareResult {
    Prepared {
        identity: ProjectSourceIdentity,
        generation: CpuGenerationId,
    },
    Failed {
        identity: ProjectSourceIdentity,
        phase: ProjectConsumerStageFailurePhase,
        failure: CpuFailure,
    },
}

#[cfg(feature = "window")]
pub(super) struct StageInputResult {
    pub identity: ProjectSourceIdentity,
    pub generation: CpuGenerationId,
    pub result: Result<PacketStageInput, CpuFailure>,
}

#[cfg(feature = "window")]
pub(super) enum LatestFrameResult {
    Frame {
        identity: ProjectSourceIdentity,
        publication: ProjectPublicationIdentity,
        frame: Arc<CpuFrameHandle>,
    },
    NoNewFrame,
    Failed {
        identity: ProjectSourceIdentity,
        failure: CpuFailure,
    },
}

pub(super) struct CpuWorker {
    pub prepare: SyncSender<PrepareRequest>,
    #[cfg(feature = "window")]
    pub stage_input: SyncSender<StageInputRequest>,
    #[cfg(feature = "window")]
    pub latest_frame: SyncSender<LatestFrameRequest>,
    pub retire: SyncSender<RetireRequest>,
    pub prepared: ResultMailbox<PrepareResult>,
    #[cfg(feature = "window")]
    pub stage_input_result: ResultMailbox<StageInputResult>,
    #[cfg(feature = "window")]
    pub latest_frame_result: ResultMailbox<LatestFrameResult>,
    pub status: StatusMailbox,
    pub failure: ResultMailbox<CpuFailure>,
    #[cfg(feature = "window")]
    pub gpu_owner_return: GpuOwnerReturnSlot,
    pub join: JoinHandle<()>,
}

#[derive(Clone)]
pub(super) struct CpuWorkerClient {
    pub prepare: SyncSender<PrepareRequest>,
    #[cfg(feature = "window")]
    pub stage_input: SyncSender<StageInputRequest>,
    #[cfg(feature = "window")]
    pub latest_frame: SyncSender<LatestFrameRequest>,
    pub retire: SyncSender<RetireRequest>,
    pub prepared: ResultMailbox<PrepareResult>,
    #[cfg(feature = "window")]
    pub stage_input_result: ResultMailbox<StageInputResult>,
    #[cfg(feature = "window")]
    pub latest_frame_result: ResultMailbox<LatestFrameResult>,
    pub status: StatusMailbox,
    pub failure: ResultMailbox<CpuFailure>,
}

impl CpuWorker {
    pub(super) fn client(&self) -> CpuWorkerClient {
        CpuWorkerClient {
            prepare: self.prepare.clone(),
            #[cfg(feature = "window")]
            stage_input: self.stage_input.clone(),
            #[cfg(feature = "window")]
            latest_frame: self.latest_frame.clone(),
            retire: self.retire.clone(),
            prepared: self.prepared.clone(),
            #[cfg(feature = "window")]
            stage_input_result: self.stage_input_result.clone(),
            #[cfg(feature = "window")]
            latest_frame_result: self.latest_frame_result.clone(),
            status: self.status.clone(),
            failure: self.failure.clone(),
        }
    }
}

/// Coalescing, nonblocking owner status slot. It carries copy-only accounting snapshots.
#[derive(Clone)]
pub(super) struct StatusMailbox {
    shared: Arc<Mutex<Option<PacketCpuStatus>>>,
}

impl StatusMailbox {
    fn publish_latest(&self, status: PacketCpuStatus) {
        if let Ok(mut slot) = self.shared.try_lock() {
            *slot = Some(status);
        }
    }

    pub(super) fn try_take(&self) -> Option<PacketCpuStatus> {
        self.shared.try_lock().ok()?.take()
    }
}

/// One-shot OOB owner disposition. The slot is allocated before the window starts; the one
/// `run_view` invocation publishes exactly once, and the CPU owner polls it without waiting.
#[cfg(feature = "window")]
#[derive(Clone)]
pub(super) struct GpuOwnerReturnSlot {
    shared: Arc<OnceLock<Mutex<Option<WindowGpuOwner>>>>,
}

#[cfg(feature = "window")]
impl GpuOwnerReturnSlot {
    pub(super) fn new() -> Self {
        Self {
            shared: Arc::new(OnceLock::new()),
        }
    }

    /// Publishes the sole window owner. Failure means the one-shot invariant was violated; the
    /// caller receives the exact owner and must retain it while marking the host failed.
    pub(super) fn publish(&self, owner: WindowGpuOwner) -> Result<(), WindowGpuOwner> {
        match self.shared.set(Mutex::new(Some(owner))) {
            Ok(()) => Ok(()),
            Err(slot) => Err(slot
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("a rejected one-shot publication retains its owner")),
        }
    }

    fn try_take(&self) -> Option<WindowGpuOwner> {
        let slot = self.shared.get()?;
        let mut slot = slot.try_lock().ok()?;
        slot.take()
    }
}

struct PendingPrepare {
    lease: ProjectSourceLease,
    identity: ProjectSourceIdentity,
    bindings: Arc<BTreeMap<StableId, EntityHandle>>,
    cancelled: Arc<AtomicBool>,
    source_pin: SourcePin,
    last_busy_status: Option<PacketCpuStatus>,
}

struct Generation {
    id: CpuGenerationId,
    identity: ProjectSourceIdentity,
    _lease: ProjectSourceLease,
    #[cfg(feature = "window")]
    last_publication_sequence: u64,
}

#[cfg(feature = "window")]
struct PendingFrame {
    request: LatestFrameRequest,
    frame: Arc<daedalus_runtime::SessionFrame>,
    last_busy_status: Option<PacketCpuStatus>,
}

pub(super) fn spawn(
    owner: PacketCpuOwner,
    client: SessionControlClient,
    signal: Arc<HostLifecycleSignal>,
) -> std::io::Result<CpuWorker> {
    let (prepare_tx, prepare_rx) = mpsc::sync_channel(REQUEST_CAPACITY);
    #[cfg(feature = "window")]
    let (stage_tx, stage_rx) = mpsc::sync_channel(REQUEST_CAPACITY);
    #[cfg(feature = "window")]
    let (frame_tx, frame_rx) = mpsc::sync_channel(REQUEST_CAPACITY);
    let (retire_tx, retire_rx) = mpsc::sync_channel(REQUEST_CAPACITY);
    let prepared = ResultMailbox::<PrepareResult>::default();
    #[cfg(feature = "window")]
    let stage_input_result = ResultMailbox::<StageInputResult>::default();
    #[cfg(feature = "window")]
    let latest_frame_result = ResultMailbox::<LatestFrameResult>::default();
    let failure = ResultMailbox::<CpuFailure>::default();
    let status = StatusMailbox {
        shared: Arc::new(Mutex::new(None)),
    };
    #[cfg(feature = "window")]
    let gpu_owner_return = GpuOwnerReturnSlot::new();
    let join = thread::Builder::new()
        .name("daedalus-project-host-cpu".to_owned())
        .spawn({
            let prepared = prepared.clone();
            #[cfg(feature = "window")]
            let stage_input_result = stage_input_result.clone();
            #[cfg(feature = "window")]
            let latest_frame_result = latest_frame_result.clone();
            let failure = failure.clone();
            let status = status.clone();
            #[cfg(feature = "window")]
            let gpu_owner_return = gpu_owner_return.clone();
            move || {
                #[cfg(not(feature = "window"))]
                drop(client);
                run_owner(
                    owner,
                    #[cfg(feature = "window")]
                    client,
                    prepare_rx,
                    #[cfg(feature = "window")]
                    stage_rx,
                    #[cfg(feature = "window")]
                    frame_rx,
                    retire_rx,
                    prepared,
                    #[cfg(feature = "window")]
                    stage_input_result,
                    #[cfg(feature = "window")]
                    latest_frame_result,
                    failure,
                    status,
                    #[cfg(feature = "window")]
                    gpu_owner_return,
                    signal,
                );
            }
        })?;
    Ok(CpuWorker {
        prepare: prepare_tx,
        #[cfg(feature = "window")]
        stage_input: stage_tx,
        #[cfg(feature = "window")]
        latest_frame: frame_tx,
        retire: retire_tx,
        prepared,
        #[cfg(feature = "window")]
        stage_input_result,
        #[cfg(feature = "window")]
        latest_frame_result,
        status,
        failure,
        #[cfg(feature = "window")]
        gpu_owner_return,
        join,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_owner(
    mut owner: PacketCpuOwner,
    #[cfg(feature = "window")] client: SessionControlClient,
    prepare_rx: Receiver<PrepareRequest>,
    #[cfg(feature = "window")] stage_rx: Receiver<StageInputRequest>,
    #[cfg(feature = "window")] frame_rx: Receiver<LatestFrameRequest>,
    retire_rx: Receiver<RetireRequest>,
    prepared: ResultMailbox<PrepareResult>,
    #[cfg(feature = "window")] stage_results: ResultMailbox<StageInputResult>,
    #[cfg(feature = "window")] frame_results: ResultMailbox<LatestFrameResult>,
    failures: ResultMailbox<CpuFailure>,
    statuses: StatusMailbox,
    #[cfg(feature = "window")] gpu_owner_return: GpuOwnerReturnSlot,
    signal: Arc<HostLifecycleSignal>,
) {
    let mut generations = Vec::<Generation>::with_capacity(2);
    #[cfg(feature = "window")]
    let mut gpu_owner: Option<WindowGpuOwner> = None;
    let mut pending_prepare: Option<PendingPrepare> = None;
    #[cfg(feature = "window")]
    let mut pending_stage: Option<StageInputRequest> = None;
    #[cfg(feature = "window")]
    let mut pending_frame: Option<PendingFrame> = None;
    let mut pending_prepare_result: Option<PrepareResult> = None;
    #[cfg(feature = "window")]
    let mut pending_stage_result: Option<StageInputResult> = None;
    #[cfg(feature = "window")]
    let mut pending_frame_result: Option<LatestFrameResult> = None;
    let mut pending_failure: Option<CpuFailure> = None;
    let mut last_status = owner.status();
    let mut stop_started = false;
    #[cfg(feature = "window")]
    let mut drain_error_reported = false;
    let mut worker_panicked = false;

    loop {
        // Each iteration is guarded while the owner and device owner remain outside the unwind
        // boundary. An unexpected panic transitions to cleanup instead of unwinding/dropping them.
        let iteration = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(feature = "window")]
            poll_gpu_owner(
                &mut gpu_owner,
                &gpu_owner_return,
                &signal,
                &mut drain_error_reported,
                &mut pending_failure,
            );

            if signal.stop_requested.load(Ordering::Acquire) {
                stop_started = true;
            }

            if stop_started {
                pending_prepare.take();
                #[cfg(feature = "window")]
                pending_stage.take();
                #[cfg(feature = "window")]
                pending_frame.take();
                pending_prepare_result.take();
                #[cfg(feature = "window")]
                pending_stage_result.take();
                #[cfg(feature = "window")]
                pending_frame_result.take();
                while let Ok(request) = prepare_rx.try_recv() {
                    drop(request);
                }
                #[cfg(feature = "window")]
                while let Ok(request) = stage_rx.try_recv() {
                    drop(request);
                }
                #[cfg(feature = "window")]
                while let Ok(request) = frame_rx.try_recv() {
                    drop(request);
                }
                while let Ok(request) = retire_rx.try_recv() {
                    retire_one(&mut owner, &mut generations, request.generation);
                }
                for generation in generations.drain(..) {
                    let _ = owner.retire_generation(generation.id);
                    drop(generation);
                }
                prepared.clear_on_owner();
                #[cfg(feature = "window")]
                stage_results.clear_on_owner();
                #[cfg(feature = "window")]
                frame_results.clear_on_owner();
                if failures.has_external_observers() {
                    publish_pending(&failures, &mut pending_failure);
                } else {
                    discard_unobserved_failure(&failures, &mut pending_failure);
                }
            } else {
                if pending_prepare.is_none()
                    && prepared.can_publish()
                    && let Ok(request) = prepare_rx.try_recv()
                {
                    pending_prepare = Some(PendingPrepare {
                        identity: request.lease.identity().clone(),
                        source_pin: SourcePin::new(request.lease.source_pin()),
                        lease: request.lease,
                        bindings: request.bindings,
                        cancelled: request.cancelled,
                        last_busy_status: None,
                    });
                }
                #[cfg(feature = "window")]
                if pending_stage.is_none()
                    && stage_results.can_publish()
                    && let Ok(request) = stage_rx.try_recv()
                {
                    pending_stage = Some(request);
                }
                #[cfg(feature = "window")]
                if pending_frame.is_none()
                    && pending_frame_result.is_none()
                    && frame_results.can_publish()
                    && let Ok(request) = frame_rx.try_recv()
                {
                    let latest = client.try_latest_frame().ok().flatten();
                    if let Some(frame) = latest.filter(|frame| {
                        frame.session() == request.identity.session()
                            && frame.project_generation() == request.identity.generation()
                            && frame.source_digest() == request.identity.source_digest()
                            && generations.iter().any(|record| {
                                record.id == request.generation
                                    && record.identity == request.identity
                                    && frame.sequence() > record.last_publication_sequence
                            })
                    }) {
                        pending_frame = Some(PendingFrame {
                            request,
                            frame,
                            last_busy_status: None,
                        });
                    } else {
                        pending_frame_result = Some(LatestFrameResult::NoNewFrame);
                    }
                }

                while let Ok(request) = retire_rx.try_recv() {
                    retire_one(&mut owner, &mut generations, request.generation);
                }

                publish_pending(&prepared, &mut pending_prepare_result);
                #[cfg(feature = "window")]
                publish_pending(&stage_results, &mut pending_stage_result);
                #[cfg(feature = "window")]
                publish_pending(&frame_results, &mut pending_frame_result);
                publish_pending(&failures, &mut pending_failure);

                // Reclamation can make a held candidate lease admissible. It is retried with the
                // same lease, authored bindings, cancellation token, and one opaque source pin.
                let _ = owner.try_reclaim();
                let current_status = owner.status();
                advance_prepare(
                    &mut owner,
                    &mut generations,
                    &mut pending_prepare,
                    &mut pending_prepare_result,
                    &prepared,
                    current_status,
                );
                #[cfg(feature = "window")]
                advance_stage_input(
                    &owner,
                    &generations,
                    &mut pending_stage,
                    &mut pending_stage_result,
                    &stage_results,
                );
                #[cfg(feature = "window")]
                advance_latest_frame(
                    &mut owner,
                    &mut generations,
                    &mut pending_frame,
                    &mut pending_frame_result,
                    &frame_results,
                );

                publish_pending(&prepared, &mut pending_prepare_result);
                #[cfg(feature = "window")]
                publish_pending(&stage_results, &mut pending_stage_result);
                #[cfg(feature = "window")]
                publish_pending(&frame_results, &mut pending_frame_result);
                publish_pending(&failures, &mut pending_failure);
            }

            let status = owner.status();
            if status != last_status {
                statuses.publish_latest(status);
                last_status = status;
            }
            let _ = owner.try_reclaim();
        }));

        if iteration.is_err() {
            worker_panicked = true;
            stop_started = true;
            signal.stop_requested.store(true, Ordering::Release);
            let _ = failures.try_publish(CpuFailure {
                code: "cpu_owner_panicked",
                message: "CPU owner job panicked; cleanup remains on its owner thread".to_owned(),
            });
        }

        let status = owner.status();
        statuses.publish_latest(status);
        #[cfg(feature = "window")]
        let owner_waiting = window_owner_waiting(&signal);
        #[cfg(not(feature = "window"))]
        let owner_waiting = false;
        #[cfg(feature = "window")]
        let device_drained = gpu_owner.is_none();
        #[cfg(not(feature = "window"))]
        let device_drained = true;
        if stop_started
            && !owner_waiting
            && !signal.prepare_handoff_pending.load(Ordering::Acquire)
            && device_drained
            && status.generation_slots_used == 0
            && status.frame_slots_used == 0
            && status.scratch_current_bytes == 0
            && pending_prepare.is_none()
            && {
                #[cfg(feature = "window")]
                {
                    pending_stage.is_none() && pending_frame.is_none()
                }
                #[cfg(not(feature = "window"))]
                {
                    true
                }
            }
            && pending_prepare_result.is_none()
            && {
                #[cfg(feature = "window")]
                {
                    pending_stage_result.is_none() && pending_frame_result.is_none()
                }
                #[cfg(not(feature = "window"))]
                {
                    true
                }
            }
            && pending_failure.is_none()
            && prepared.is_fully_acknowledged()
            && {
                #[cfg(feature = "window")]
                {
                    stage_results.is_fully_acknowledged() && frame_results.is_fully_acknowledged()
                }
                #[cfg(not(feature = "window"))]
                {
                    true
                }
            }
            && failures.is_fully_acknowledged()
        {
            break;
        }
        if worker_panicked {
            // The panic is terminal for accepting host work, but OOB return/drain and registry
            // reclamation continue until every externally held alias is released.
            signal.stop_requested.store(true, Ordering::Release);
        }
        thread::sleep(OWNER_POLL_INTERVAL);
    }
}

fn discard_unobserved_failure(
    failures: &ResultMailbox<CpuFailure>,
    pending_failure: &mut Option<CpuFailure>,
) {
    if !failures.has_external_observers() {
        pending_failure.take();
        failures.clear_on_owner();
    }
}

#[cfg(feature = "window")]
fn poll_gpu_owner(
    owner: &mut Option<WindowGpuOwner>,
    returned: &GpuOwnerReturnSlot,
    signal: &HostLifecycleSignal,
    drain_error_reported: &mut bool,
    failure: &mut Option<CpuFailure>,
) {
    if owner.is_none() {
        *owner = returned.try_take();
        if owner.is_some() {
            signal.window_owner_observed.store(true, Ordering::Release);
        }
    }
    if let Some(device_owner) = owner.as_mut() {
        match device_owner.try_drain_after_window() {
            Ok(PacketRendererDrainStatus::Drained) => {
                // Device pins are gone before the CPU registry releases generation roots.
                drop(owner.take());
                *drain_error_reported = false;
            }
            Ok(PacketRendererDrainStatus::Pending) => {}
            Err(error) if !*drain_error_reported => {
                *failure = Some(CpuFailure {
                    code: "gpu_owner_drain_failed",
                    message: bounded_message(error.to_string(), 2_048),
                });
                *drain_error_reported = true;
            }
            Err(_) => {}
        }
    }
}

#[cfg(feature = "window")]
fn window_owner_waiting(signal: &HostLifecycleSignal) -> bool {
    signal.window_owner_expected.load(Ordering::Acquire)
        && !signal.window_owner_observed.load(Ordering::Acquire)
}

fn advance_prepare(
    owner: &mut PacketCpuOwner,
    generations: &mut Vec<Generation>,
    pending: &mut Option<PendingPrepare>,
    result: &mut Option<PrepareResult>,
    result_slot: &ResultMailbox<PrepareResult>,
    current_status: PacketCpuStatus,
) {
    let Some(request) = pending.as_ref() else {
        return;
    };
    if request.cancelled.load(Ordering::Acquire) || !request.lease.is_current() {
        pending.take();
        return;
    }
    if result.is_some() || !result_slot.can_publish() {
        return;
    }
    if request.last_busy_status == Some(current_status) {
        return;
    }
    let prepared = request.lease.with_source(|project| {
        owner.prepare_generation(
            project,
            request.identity.generation(),
            request.identity.session(),
            &request.bindings,
            request.source_pin.clone(),
            &request.cancelled,
        )
    });
    match prepared {
        Err(PacketRenderError::CpuResidencyBusy) => {
            if let Some(request) = pending.as_mut() {
                request.last_busy_status = Some(owner.status());
            }
        }
        Err(error) => {
            let request = pending.take().expect("pending prepare exists");
            *result = Some(PrepareResult::Failed {
                identity: request.identity,
                phase: ProjectConsumerStageFailurePhase::CpuPrepare,
                failure: cpu_failure(error),
            });
        }
        Ok(generation) => {
            let request = pending.take().expect("pending prepare exists");
            if request.cancelled.load(Ordering::Acquire) || !request.lease.is_current() {
                let _ = owner.retire_generation(generation);
                return;
            }
            if generations
                .iter()
                .any(|record| record.identity == request.identity)
            {
                let _ = owner.retire_generation(generation);
                *result = Some(PrepareResult::Failed {
                    identity: request.identity,
                    phase: ProjectConsumerStageFailurePhase::CpuPrepare,
                    failure: CpuFailure {
                        code: "generation_identity_collision",
                        message: "CPU owner already tracks this project generation".to_owned(),
                    },
                });
                return;
            }
            generations.push(Generation {
                id: generation,
                identity: request.identity.clone(),
                _lease: request.lease,
                #[cfg(feature = "window")]
                last_publication_sequence: 0,
            });
            *result = Some(PrepareResult::Prepared {
                identity: request.identity,
                generation,
            });
        }
    }
}

#[cfg(feature = "window")]
fn advance_stage_input(
    owner: &PacketCpuOwner,
    generations: &[Generation],
    pending: &mut Option<StageInputRequest>,
    result: &mut Option<StageInputResult>,
    result_slot: &ResultMailbox<StageInputResult>,
) {
    if result.is_some() || !result_slot.can_publish() {
        return;
    }
    let Some(request) = pending.as_ref() else {
        return;
    };
    let generation = generations
        .iter()
        .find(|record| record.id == request.generation && record.identity == request.identity);
    match generation {
        Some(record) if !record._lease.is_current() => {
            pending.take();
        }
        Some(_) => match owner.stage_input(request.generation) {
            Ok(input) => {
                *result = Some(StageInputResult {
                    identity: request.identity.clone(),
                    generation: request.generation,
                    result: Ok(input),
                });
                pending.take();
            }
            Err(PacketRenderError::CpuResidencyBusy) => {}
            Err(error) => {
                *result = Some(StageInputResult {
                    identity: request.identity.clone(),
                    generation: request.generation,
                    result: Err(cpu_failure(error)),
                });
                pending.take();
            }
        },
        None => {
            *result = Some(StageInputResult {
                identity: request.identity.clone(),
                generation: request.generation,
                result: Err(CpuFailure {
                    code: "stale_cpu_generation",
                    message: "CPU generation is no longer available".to_owned(),
                }),
            });
            pending.take();
        }
    }
}

#[cfg(feature = "window")]
fn advance_latest_frame(
    owner: &mut PacketCpuOwner,
    generations: &mut [Generation],
    pending: &mut Option<PendingFrame>,
    result: &mut Option<LatestFrameResult>,
    result_slot: &ResultMailbox<LatestFrameResult>,
) {
    let Some(request) = pending.as_ref() else {
        return;
    };
    let record_index = generations.iter().position(|record| {
        record.id == request.request.generation && record.identity == request.request.identity
    });
    let current = record_index.is_some_and(|index| generations[index]._lease.is_current());
    if request.request.cancelled.load(Ordering::Acquire) || !current {
        pending.take();
        if result.is_none() {
            *result = Some(LatestFrameResult::NoNewFrame);
        }
        return;
    }
    if result.is_some() || !result_slot.can_publish() {
        return;
    }
    if request.last_busy_status == Some(owner.status()) {
        return;
    }
    let record_index = record_index.expect("current generation index was checked above");
    let frame = &request.frame;
    if frame.sequence() <= generations[record_index].last_publication_sequence {
        *result = Some(LatestFrameResult::NoNewFrame);
        pending.take();
        return;
    }
    let publication = match ProjectPublicationIdentity::from_frame(frame) {
        Ok(publication) => publication,
        Err(error) => {
            *result = Some(LatestFrameResult::Failed {
                identity: request.request.identity.clone(),
                failure: CpuFailure {
                    code: "invalid_publication",
                    message: bounded_message(error.to_string(), 2_048),
                },
            });
            pending.take();
            return;
        }
    };
    match owner.extract_validated(
        request.request.generation,
        frame.snapshot(),
        frame.sequence(),
        request.request.view.as_ref(),
        &request.request.cancelled,
    ) {
        Ok(cpu_frame) => {
            generations[record_index].last_publication_sequence = frame.sequence();
            *result = Some(LatestFrameResult::Frame {
                identity: request.request.identity.clone(),
                publication,
                frame: cpu_frame,
            });
            pending.take();
        }
        Err(PacketRenderError::CpuResidencyBusy) => {
            if let Some(request) = pending.as_mut() {
                request.last_busy_status = Some(owner.status());
            }
        }
        Err(error) => {
            *result = Some(LatestFrameResult::Failed {
                identity: request.request.identity.clone(),
                failure: cpu_failure(error),
            });
            pending.take();
        }
    }
}

fn retire_one(owner: &mut PacketCpuOwner, generations: &mut Vec<Generation>, id: CpuGenerationId) {
    if let Some(index) = generations.iter().position(|record| record.id == id) {
        let _ = owner.retire_generation(id);
        generations.remove(index);
    }
}

fn publish_pending<T>(slot: &ResultMailbox<T>, pending: &mut Option<T>) {
    if let Some(value) = pending.take()
        && let Err(value) = slot.try_publish(value)
    {
        *pending = Some(value);
    }
}

fn cpu_failure(error: PacketRenderError) -> CpuFailure {
    let code = match &error {
        PacketRenderError::CpuCancelled => "cancelled",
        PacketRenderError::CpuResidencyBusy => "cpu_residency_busy",
        PacketRenderError::CpuBudgetExceeded { .. } => "cpu_budget_exceeded",
        PacketRenderError::AssetByteBudgetExceeded { .. } => "cpu_budget_exceeded",
        PacketRenderError::CpuLimitExceeded { .. } => "cpu_limit_exceeded",
        PacketRenderError::AssetLimitExceeded { .. } => "cpu_limit_exceeded",
        PacketRenderError::CpuAllocationFailed => "cpu_allocation_failed",
        PacketRenderError::StaleCpuGeneration => "stale_cpu_generation",
        _ => "cpu_prepare_failed",
    };
    CpuFailure {
        code,
        message: bounded_message(error.to_string(), 2_048),
    }
}

fn bounded_message(mut message: String, maximum: usize) -> String {
    if message.len() <= maximum {
        return message;
    }
    let mut end = maximum;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "window")]
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::AtomicU64,
        time::{Duration, Instant},
    };

    #[cfg(feature = "window")]
    use daedalus_renderer::{
        PacketAssetLimits, PacketCpuLimits, PacketCpuOwner, PacketRendererLimits, PacketStageInput,
        SourcePin,
    };
    #[cfg(feature = "window")]
    use daedalus_runtime::{
        AssetActivationCommandOutcome, AssetActivationOperation, AssetActivationRequestResult,
        AssetPreparationCommandOutcome, AssetPreparationOperation, AssetPreparationResult,
        NativeProjectActivationService, NativeProjectActivationShutdown,
        NativeProjectPreparationOptions, ProjectGenerationConsumerPort, RequestPhase,
        SessionControlConfig, SessionOperation, SessionRequestResult, WaitStage,
    };

    #[cfg(feature = "window")]
    static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);

    #[cfg(feature = "window")]
    struct TempProject(PathBuf);

    #[cfg(feature = "window")]
    impl TempProject {
        fn new() -> Self {
            let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "daedalus-host-cpu-worker-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("daedalus.project.json"),
                br#"{"schema_version":1,"name":"host-cpu-worker-test","scene":"scene.json"}"#,
            )
            .unwrap();
            fs::write(
                path.join("scene.json"),
                br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#,
            )
            .unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[cfg(feature = "window")]
    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(feature = "window")]
    struct BusyPrepareFixture {
        _project: TempProject,
        service: NativeProjectActivationService,
        consumer: ProjectGenerationConsumerPort,
        client: SessionControlClient,
        owner: PacketCpuOwner,
        held_stage_input: Option<PacketStageInput>,
        pending: Option<PendingPrepare>,
        generations: Vec<Generation>,
        result: Option<PrepareResult>,
        result_slot: ResultMailbox<PrepareResult>,
    }

    #[cfg(feature = "window")]
    impl BusyPrepareFixture {
        fn new() -> Self {
            let project = TempProject::new();
            let (service, consumer) = NativeProjectActivationService::start_with_render_consumer(
                NativeProjectPreparationOptions::new(project.path()),
                SessionControlConfig::default(),
            )
            .expect("fixture starts the real runtime project and actor");
            let deadline = Instant::now() + Duration::from_secs(5);
            let initial = loop {
                match consumer.try_take_initial() {
                    Ok(Some(lease)) => break lease,
                    Ok(None) | Err(daedalus_runtime::ProjectConsumerError::Busy) => {}
                    Err(error) => panic!("initial lease retrieval failed: {error}"),
                }
                assert!(Instant::now() < deadline, "initial lease was not published");
                thread::sleep(Duration::from_millis(2));
            };
            let session = initial.identity().session();
            let client = service
                .session()
                .attach()
                .expect("candidate operations use the same authority");
            let mut owner = PacketCpuOwner::new(
                PacketCpuLimits::default(),
                PacketRendererLimits::default(),
                PacketAssetLimits::default(),
            )
            .expect("default CPU owner limits are valid");
            let source_pin = SourcePin::new(initial.source_pin());
            let cancelled = AtomicBool::new(false);
            let (first, _second) = initial.with_source(|prepared| {
                let first = owner
                    .prepare_generation(
                        prepared,
                        1,
                        session,
                        &BTreeMap::new(),
                        source_pin.clone(),
                        &cancelled,
                    )
                    .expect("first CPU generation is prepared");
                let second = owner
                    .prepare_generation(
                        prepared,
                        2,
                        session,
                        &BTreeMap::new(),
                        source_pin,
                        &cancelled,
                    )
                    .expect("second CPU generation fills the bounded slots");
                (first, second)
            });
            drop(initial);

            let held_stage_input = owner
                .stage_input(first)
                .expect("real stage projection retains the first generation");
            owner
                .retire_generation(first)
                .expect("retiring the first generation is nonblocking");
            assert_eq!(owner.try_reclaim().pending_generations, 1);
            assert_eq!(owner.status().generation_slots_used, 2);

            let accepted = wait_terminal_result(
                &client,
                SessionOperation::AssetPreparation(AssetPreparationOperation::Prepare {
                    command_id: None,
                }),
            )
            .expect("runtime accepts candidate preparation");
            let candidate = match accepted {
                SessionRequestResult::AssetPreparation(AssetPreparationResult::Command(result)) => {
                    match result.outcome() {
                        AssetPreparationCommandOutcome::PrepareAccepted { candidate } => *candidate,
                        _ => panic!("prepare returns its candidate identity"),
                    }
                }
                _ => panic!("prepare returns a command receipt"),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match wait_terminal_result(
                    &client,
                    SessionOperation::AssetPreparation(AssetPreparationOperation::Status),
                )
                .expect("preparation status remains available")
                {
                    SessionRequestResult::AssetPreparation(AssetPreparationResult::Status(
                        status,
                    )) if status
                        .ready_candidate()
                        .is_some_and(|summary| summary.id() == candidate) =>
                    {
                        break;
                    }
                    _ => {
                        assert!(
                            Instant::now() < deadline,
                            "candidate CPU preparation timed out"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            let staged = wait_terminal_result(
                &client,
                SessionOperation::AssetActivation(AssetActivationOperation::Stage {
                    command_id: None,
                    candidate,
                    expected_active_generation: 1,
                }),
            )
            .expect("runtime accepts the prepared candidate for staging");
            let activation = match staged {
                SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(
                    result,
                )) => match result.outcome() {
                    AssetActivationCommandOutcome::StageAccepted {
                        activation,
                        candidate: staged_candidate,
                    } if *staged_candidate == candidate => *activation,
                    _ => panic!("stage returns its activation identity"),
                },
                _ => panic!("stage returns a command receipt"),
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            let lease = loop {
                match consumer.try_take_candidate() {
                    Ok(Some(lease)) => break lease,
                    Ok(None) | Err(daedalus_runtime::ProjectConsumerError::Busy) => {}
                    Err(error) => panic!("candidate lease retrieval failed: {error}"),
                }
                assert!(
                    Instant::now() < deadline,
                    "candidate lease was not published"
                );
                thread::sleep(Duration::from_millis(2));
            };
            assert!(lease.is_current());
            assert_eq!(lease.identity().activation(), Some(activation));
            let identity = lease.identity().clone();
            let request_cancelled = Arc::new(AtomicBool::new(false));
            let mut pending = Some(PendingPrepare {
                source_pin: SourcePin::new(lease.source_pin()),
                lease,
                identity,
                bindings: Arc::new(BTreeMap::new()),
                cancelled: request_cancelled,
                last_busy_status: None,
            });
            let mut result = None;
            let mut generations = Vec::new();
            let result_slot = ResultMailbox::default();
            let current_status = owner.status();
            advance_prepare(
                &mut owner,
                &mut generations,
                &mut pending,
                &mut result,
                &result_slot,
                current_status,
            );
            assert!(
                pending.is_some(),
                "a transient residency Busy retains the lease"
            );
            assert!(
                result.is_none(),
                "transient Busy is not published as failure"
            );
            assert_eq!(
                pending.as_ref().unwrap().last_busy_status,
                Some(owner.status()),
                "the same owner state is memoized to avoid a hot retry loop"
            );

            Self {
                _project: project,
                service,
                consumer,
                client,
                owner,
                held_stage_input: Some(held_stage_input),
                pending,
                generations,
                result,
                result_slot,
            }
        }
    }

    #[cfg(feature = "window")]
    fn wait_terminal_result(
        client: &SessionControlClient,
        operation: SessionOperation,
    ) -> Result<SessionRequestResult, daedalus_runtime::SessionRequestError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ticket = loop {
            match client.try_submit(operation.clone()) {
                Ok(ticket) => break ticket,
                Err(
                    daedalus_runtime::SessionSubmitError::Busy
                    | daedalus_runtime::SessionSubmitError::QueueFull,
                ) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("request admission failed: {error}"),
            }
        };
        let progress = ticket
            .wait_for(WaitStage::Terminal, Duration::from_secs(5))
            .expect("request reaches a terminal result");
        assert_eq!(progress.phase(), RequestPhase::Terminal);
        let result = progress
            .terminal()
            .expect("terminal request has a result")
            .clone();
        ticket
            .acknowledge()
            .expect("terminal result credit is acknowledged");
        result
    }

    #[cfg(feature = "window")]
    fn stop_activation_service(mut service: NativeProjectActivationService) {
        service.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service
                .try_shutdown()
                .expect("runtime service remains healthy")
            {
                NativeProjectActivationShutdown::Stopped => return,
                NativeProjectActivationShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "runtime service did not stop");
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    #[test]
    fn result_lane_remains_reserved_until_payload_is_dropped() {
        let slot = ResultMailbox::default();
        assert!(slot.can_publish());
        slot.try_publish(String::from("first"))
            .expect("empty result lane accepts one payload");
        let delivery = slot.try_take().expect("published payload is readable");
        assert_eq!(&**delivery, "first");
        assert!(!slot.can_publish(), "taking is not acknowledgment");
        assert_eq!(
            slot.try_publish(String::from("second")),
            Err(String::from("second"))
        );
        drop(delivery);
        assert!(slot.can_publish(), "dropping payload acknowledges the lane");
        slot.try_publish(String::from("second"))
            .expect("acknowledged lane accepts next payload");
        slot.clear_on_owner();
        assert!(slot.is_fully_acknowledged());
    }

    #[test]
    fn result_lane_rechecks_delivery_under_the_slot_lock() {
        // This is the state after a consumer takes the current payload and marks it delivered,
        // before its ResultDelivery guard is dropped to acknowledge it. A producer that read
        // `delivered == false` before taking the mutex must recheck while holding that mutex.
        let delivered = AtomicBool::new(true);
        let mut slot: Option<&str> = None;

        assert!(!slot_is_available(&slot, &delivered));
        assert_eq!(
            publish_into_available_slot(&mut slot, &delivered, "next"),
            Err("next"),
            "a stale pre-lock availability read must not publish over an unacknowledged result"
        );
        assert!(slot.is_none());

        delivered.store(false, Ordering::Release);
        assert!(slot_is_available(&slot, &delivered));
        publish_into_available_slot(&mut slot, &delivered, "next")
            .expect("the acknowledged lane accepts the next result");
        assert_eq!(slot, Some("next"));
    }

    #[test]
    #[cfg(feature = "window")]
    fn real_worker_drains_returned_owner_without_waiting_for_result_ack() {
        let project = TempProject::new();
        let (mut service, consumer) = NativeProjectActivationService::start_with_render_consumer(
            NativeProjectPreparationOptions::new(project.path()),
            SessionControlConfig::default(),
        )
        .expect("fixture starts the real runtime project and actor");
        let deadline = Instant::now() + Duration::from_secs(5);
        let lease = loop {
            match consumer.try_take_initial() {
                Ok(Some(lease)) => break lease,
                Ok(None) | Err(daedalus_runtime::ProjectConsumerError::Busy) => {}
                Err(error) => panic!("initial lease retrieval failed: {error}"),
            }
            assert!(Instant::now() < deadline, "initial lease was not published");
            thread::sleep(Duration::from_millis(2));
        };
        let client = service
            .session()
            .attach()
            .expect("CPU worker attaches to the same runtime actor");
        let signal = Arc::new(HostLifecycleSignal::new());
        let owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .expect("default CPU owner limits are valid");
        let worker = spawn(owner, client, Arc::clone(&signal)).expect("CPU owner thread starts");

        worker
            .prepare
            .try_send(PrepareRequest {
                lease,
                bindings: Arc::new(BTreeMap::new()),
                cancelled: Arc::new(AtomicBool::new(false)),
            })
            .expect("actual initial lease enters the owner request lane");
        let deadline = Instant::now() + Duration::from_secs(5);
        let delivery = loop {
            if let Some(delivery) = worker.prepared.try_take() {
                break delivery;
            }
            assert!(
                Instant::now() < deadline,
                "CPU worker did not prepare the project"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(&*delivery, PrepareResult::Prepared { .. }));
        // Keep the real preparation result delivered but unacknowledged. Stop and device-owner
        // disposition use independent lanes and must still progress on the CPU owner thread.
        signal.window_owner_expected.store(true, Ordering::Release);
        signal.stop_requested.store(true, Ordering::Release);
        worker
            .gpu_owner_return
            .publish(WindowGpuOwner::new())
            .unwrap_or_else(|_| panic!("the window can return the no-device owner once"));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !signal.window_owner_observed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "CPU owner did not take the returned device disposition"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            !worker.join.is_finished(),
            "worker retains the held result until its observer acknowledges it"
        );

        drop(delivery);
        let CpuWorker { join, .. } = worker;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !join.is_finished() {
            assert!(
                Instant::now() < deadline,
                "CPU owner did not finish shutdown"
            );
            thread::sleep(Duration::from_millis(2));
        }
        join.join().expect("owner thread exits without panic");

        service.request_stop();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match service
                .try_shutdown()
                .expect("runtime shutdown remains healthy")
            {
                NativeProjectActivationShutdown::Stopped => break,
                NativeProjectActivationShutdown::Stopping => {
                    assert!(Instant::now() < deadline, "runtime service did not stop");
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    #[test]
    fn dropped_facade_does_not_leave_failure_lane_blocking_owner_exit() {
        let failures = ResultMailbox::<CpuFailure>::default();
        let facade = failures.clone();
        failures
            .try_publish(CpuFailure {
                code: "test_failure",
                message: "preserve while the facade can observe it".to_owned(),
            })
            .expect("failure lane starts empty");
        let mut pending = Some(CpuFailure {
            code: "pending_failure",
            message: "also retained for an observer".to_owned(),
        });

        discard_unobserved_failure(&failures, &mut pending);
        assert!(pending.is_some());
        assert!(!failures.is_fully_acknowledged());
        let delivery = facade.try_take().expect("live facade can consume failure");
        assert_eq!(delivery.code, "test_failure");
        drop(delivery);

        drop(facade);
        discard_unobserved_failure(&failures, &mut pending);
        assert!(pending.is_none());
        assert!(failures.is_fully_acknowledged());
    }

    #[test]
    #[cfg(feature = "window")]
    fn owner_published_after_poll_is_observed_before_shutdown_can_finish() {
        let returned = GpuOwnerReturnSlot::new();
        let signal = HostLifecycleSignal::new();
        signal.window_owner_expected.store(true, Ordering::Release);
        let mut owner = None;
        let mut drain_error_reported = false;
        let mut failure = None;

        // This is the relevant interleaving: the worker's first poll sees no owner, then the
        // window publishes after that poll and before the stop predicate is evaluated.
        poll_gpu_owner(
            &mut owner,
            &returned,
            &signal,
            &mut drain_error_reported,
            &mut failure,
        );
        assert!(window_owner_waiting(&signal));
        returned
            .publish(WindowGpuOwner::new())
            .unwrap_or_else(|_| panic!("the sole view publishes its owner once"));
        assert!(window_owner_waiting(&signal));

        poll_gpu_owner(
            &mut owner,
            &returned,
            &signal,
            &mut drain_error_reported,
            &mut failure,
        );
        assert!(!window_owner_waiting(&signal));
        assert!(
            owner.is_none(),
            "the no-device owner drains on the CPU owner"
        );
        assert!(failure.is_none());
    }

    #[test]
    #[cfg(feature = "window")]
    fn cancelled_candidate_releases_the_same_lease_after_residency_busy() {
        let mut fixture = BusyPrepareFixture::new();
        let activation = fixture
            .pending
            .as_ref()
            .expect("busy request retains its lease")
            .lease
            .identity()
            .activation()
            .expect("staged candidate lease carries its activation ID");
        let cancelled = wait_terminal_result(
            &fixture.client,
            SessionOperation::AssetActivation(AssetActivationOperation::Cancel {
                command_id: None,
                activation,
            }),
        )
        .expect("runtime accepts activation cancellation");
        assert!(matches!(
            cancelled,
            SessionRequestResult::AssetActivation(AssetActivationRequestResult::Command(ref result))
                if matches!(result.outcome(), AssetActivationCommandOutcome::CancelAccepted { activation: id } if *id == activation)
        ));
        assert!(
            !fixture.pending.as_ref().unwrap().lease.is_current(),
            "the actual runtime cancellation invalidates the move-only lease"
        );

        let current_status = fixture.owner.status();
        let mut pending = fixture.pending;
        let mut result = fixture.result;
        let mut generations = fixture.generations;
        advance_prepare(
            &mut fixture.owner,
            &mut generations,
            &mut pending,
            &mut result,
            &fixture.result_slot,
            current_status,
        );
        assert!(pending.is_none(), "stale lease is released without a retry");
        assert!(
            result.is_none(),
            "cancel is not reported as a CPU preparation failure"
        );

        let BusyPrepareFixture {
            _project,
            service,
            consumer,
            client,
            owner,
            held_stage_input,
            result_slot,
            ..
        } = fixture;
        drop(pending);
        drop(result);
        drop(generations);
        drop(held_stage_input);
        drop(result_slot);
        drop(owner);
        drop(client);
        drop(consumer);
        stop_activation_service(service);
        drop(_project);
    }

    #[test]
    #[cfg(feature = "window")]
    fn residency_busy_resumes_same_candidate_after_retired_alias_is_reclaimed() {
        let mut fixture = BusyPrepareFixture::new();
        let candidate_identity = fixture
            .pending
            .as_ref()
            .expect("busy request retains its lease")
            .identity
            .clone();
        drop(fixture.held_stage_input);
        let reclaimed = fixture.owner.try_reclaim();
        assert_eq!(reclaimed.reclaimed_generations, 1);
        assert_eq!(fixture.owner.status().generation_slots_used, 1);

        let current_status = fixture.owner.status();
        let mut pending = fixture.pending;
        let mut result = fixture.result;
        let mut generations = fixture.generations;
        advance_prepare(
            &mut fixture.owner,
            &mut generations,
            &mut pending,
            &mut result,
            &fixture.result_slot,
            current_status,
        );
        assert!(
            pending.is_none(),
            "released capacity retries the retained lease"
        );
        let prepared_generation = match result.take() {
            Some(PrepareResult::Prepared {
                identity,
                generation,
            }) => {
                assert_eq!(identity, candidate_identity);
                generation
            }
            _ => panic!("same candidate did not prepare after capacity release"),
        };
        assert_eq!(generations.len(), 1);

        // Stage resupply addresses the canonical prepared generation, not a second import or a
        // reconstructed request payload.
        let mut stage_request = Some(StageInputRequest {
            generation: prepared_generation,
            identity: candidate_identity,
        });
        let mut stage_result = None;
        let stage_result_slot = ResultMailbox::default();
        advance_stage_input(
            &fixture.owner,
            &generations,
            &mut stage_request,
            &mut stage_result,
            &stage_result_slot,
        );
        assert!(stage_request.is_none());
        assert!(matches!(
            stage_result,
            Some(StageInputResult { result: Ok(_), .. })
        ));

        let BusyPrepareFixture {
            _project,
            service,
            consumer,
            client,
            owner,
            result_slot,
            ..
        } = fixture;
        drop(pending);
        drop(result);
        drop(stage_result);
        drop(stage_request);
        drop(stage_result_slot);
        drop(generations);
        drop(result_slot);
        drop(owner);
        drop(client);
        drop(consumer);
        stop_activation_service(service);
        drop(_project);
    }
}
