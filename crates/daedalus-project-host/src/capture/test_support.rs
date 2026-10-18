//! Real runtime actor fixtures for capture pipeline unit tests.
//!
//! The fixture exercises request admission and obtains opaque identities from a real
//! `SessionFrame`. It does not produce renderer CPU frames, GPU readbacks, or PNG pixels.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use daedalus_renderer::{
    CpuFrameHandle, CpuGenerationId, PacketAssetLimits, PacketCpuLimits, PacketCpuOwner,
    PacketRendererLimits, SourcePin,
};
use daedalus_runtime::{
    CaptureHandoffError, CaptureHandoffPort, CaptureReadyInfo, CaptureSurfaceFormat,
    NativeProjectActivationService, NativeProjectPreparationOptions, ProjectGenerationConsumerPort,
    ProjectPublicationIdentity, RequestTicket, SessionControlClient, SessionControlConfig,
    SessionOperation, SessionSubmitError,
};

const WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(2);
static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);
static ENGINE_START_LOCK: Mutex<()> = Mutex::new(());

struct EmptyProject(PathBuf);

impl EmptyProject {
    fn new() -> Self {
        let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "daedalus-host-capture-fixture-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create unique host capture fixture project");
        fs::write(
            root.join("daedalus.project.json"),
            br#"{"schema_version":1,"name":"capture-host-fixture","scene":"scene.json"}"#,
        )
        .expect("write capture fixture manifest");
        fs::write(
            root.join("scene.json"),
            br#"{"schema_version":1,"revision":0,"entities":[],"assets":[]}"#,
        )
        .expect("write empty capture fixture scene");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EmptyProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A runtime-owned request fixture for CPU pipeline tests. `request` may be moved into a
/// pending CPU capture by `take_request`; service shutdown remains bounded in either case.
pub(crate) struct RuntimeCaptureFixture {
    _project: EmptyProject,
    service: NativeProjectActivationService,
    _render_port: ProjectGenerationConsumerPort,
    handoff: CaptureHandoffPort,
    client: SessionControlClient,
    ticket: Option<RequestTicket>,
    request: Option<Box<daedalus_runtime::CaptureRequest>>,
    publication: ProjectPublicationIdentity,
}

impl RuntimeCaptureFixture {
    pub(crate) fn new() -> Self {
        let _engine_start_guard = ENGINE_START_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let project = EmptyProject::new();
        let (service, render_port, handoff) =
            NativeProjectActivationService::start_with_render_consumer_and_capture(
                NativeProjectPreparationOptions::new(project.path()),
                SessionControlConfig::default(),
            )
            .expect("start real capture-enabled runtime fixture");
        let ready = CaptureReadyInfo::new(CaptureSurfaceFormat::Rgba8UnormSrgb, 2, 2, 4)
            .expect("fixture readiness metadata is valid");
        handoff
            .try_set_ready(ready)
            .expect("resolve capture handoff readiness once");
        let client = service
            .session()
            .attach()
            .expect("attach real runtime fixture client");
        let publication = latest_frame(&client);
        let publication = ProjectPublicationIdentity::from_frame(&publication)
            .expect("capture fixture identity comes from actual SessionFrame");
        let ticket = submit_capture(&client);
        let request = take_request(&handoff, ticket.id());

        drop(_engine_start_guard);
        Self {
            _project: project,
            service,
            _render_port: render_port,
            handoff,
            client,
            ticket: Some(ticket),
            request: Some(Box::new(request)),
            publication,
        }
    }

    pub(crate) fn request(&self) -> &daedalus_runtime::CaptureRequest {
        self.request
            .as_deref()
            .expect("fixture request is still owned")
    }

    pub(crate) fn take_request(&mut self) -> Option<Box<daedalus_runtime::CaptureRequest>> {
        self.request.take()
    }

    pub(crate) fn take_ticket(&mut self) -> RequestTicket {
        self.ticket.take().expect("fixture ticket is still owned")
    }

    pub(crate) fn submit_next_capture(&mut self) -> daedalus_runtime::RequestTicketId {
        assert!(
            self.ticket.is_none(),
            "previous fixture ticket must be taken first"
        );
        assert!(
            self.request.is_none(),
            "previous fixture request must be taken first"
        );
        let ticket = submit_capture(&self.client);
        let request = take_request(&self.handoff, ticket.id());
        let id = ticket.id();
        self.ticket = Some(ticket);
        self.request = Some(Box::new(request));
        id
    }

    pub(crate) fn ticket_mut(&mut self) -> &mut RequestTicket {
        self.ticket.as_mut().expect("fixture ticket is still owned")
    }

    pub(crate) fn client(&self) -> &SessionControlClient {
        &self.client
    }

    pub(crate) fn publication(&self) -> &ProjectPublicationIdentity {
        &self.publication
    }

    /// Builds a real CPU frame handle from this fixture's actual initial runtime lease and
    /// latest SessionFrame. This proves CPU identity projection only; it is not GPU rendering.
    pub(crate) fn take_cpu_frame(&mut self) -> RuntimeCpuFrame {
        let initial = take_initial(&self._render_port);
        let identity = initial.identity().clone();
        let session = identity.session();
        let source_pin = SourcePin::new(initial.source_pin());
        let mut owner = PacketCpuOwner::new(
            PacketCpuLimits::default(),
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        )
        .expect("default CPU owner limits are valid");
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let generation = initial.with_source(|prepared| {
            owner
                .prepare_generation(
                    prepared,
                    identity.generation(),
                    session,
                    &BTreeMap::new(),
                    source_pin,
                    &cancelled,
                )
                .expect("prepare CPU generation from actual runtime lease")
        });
        let frame = latest_frame(&self.client);
        let cpu_frame = owner
            .extract_validated(
                generation,
                frame.snapshot(),
                frame.sequence(),
                None,
                &cancelled,
            )
            .expect("extract validated CPU frame from actual SessionFrame");
        let publication = ProjectPublicationIdentity::from_frame(&frame)
            .expect("CPU fixture identity comes from its actual SessionFrame");
        RuntimeCpuFrame {
            owner,
            generation,
            session_frame: frame,
            publication,
            frame: Some(cpu_frame),
        }
    }
}

pub(crate) struct RuntimeCpuFrame {
    owner: PacketCpuOwner,
    generation: CpuGenerationId,
    session_frame: std::sync::Arc<daedalus_runtime::SessionFrame>,
    pub(crate) publication: ProjectPublicationIdentity,
    frame: Option<std::sync::Arc<CpuFrameHandle>>,
}

impl RuntimeCpuFrame {
    pub(crate) fn take_frame(&mut self) -> std::sync::Arc<CpuFrameHandle> {
        self.frame.take().expect("CPU fixture frame is still owned")
    }

    pub(crate) fn extract_again(&mut self) -> std::sync::Arc<CpuFrameHandle> {
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        self.owner
            .extract_validated(
                self.generation,
                self.session_frame.snapshot(),
                self.session_frame.sequence(),
                None,
                &cancelled,
            )
            .expect("extract another CPU handle from the same actual SessionFrame")
    }
}

impl Drop for RuntimeCaptureFixture {
    fn drop(&mut self) {
        // Shutdown is bounded test cleanup. If an assertion unwinds while the request is still
        // pending, the runtime actor owns its terminal cancellation; dropping this helper never
        // fabricates renderer or encoder completion.
        self.service.request_stop();
        let deadline = Instant::now() + WAIT;
        loop {
            match self.service.try_shutdown() {
                Ok(daedalus_runtime::NativeProjectActivationShutdown::Stopped) => break,
                Ok(daedalus_runtime::NativeProjectActivationShutdown::Stopping) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(POLL);
                }
                Err(_) => break,
            }
        }
    }
}

fn latest_frame(client: &SessionControlClient) -> std::sync::Arc<daedalus_runtime::SessionFrame> {
    let deadline = Instant::now() + WAIT;
    loop {
        match client.try_latest_frame() {
            Ok(Some(frame)) => return frame,
            Ok(None) | Err(daedalus_runtime::FrameReadError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "runtime fixture did not publish a frame"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("runtime fixture frame read failed: {error}"),
        }
    }
}

fn submit_capture(client: &SessionControlClient) -> RequestTicket {
    let deadline = Instant::now() + WAIT;
    loop {
        match client.try_submit(SessionOperation::CaptureFrame {
            at_least_tick: None,
            timeout: Some(WAIT),
        }) {
            Ok(ticket) => return ticket,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull) => {
                assert!(
                    Instant::now() < deadline,
                    "runtime fixture admission stayed busy"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("runtime fixture capture admission failed: {error}"),
        }
    }
}

fn take_request(
    handoff: &CaptureHandoffPort,
    ticket_id: daedalus_runtime::RequestTicketId,
) -> daedalus_runtime::CaptureRequest {
    let deadline = Instant::now() + WAIT;
    loop {
        match handoff.try_take_request() {
            Ok(Some(request)) => {
                assert_eq!(request.id().ticket(), ticket_id);
                return request;
            }
            Ok(None) | Err(CaptureHandoffError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "runtime fixture request was not routed"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("runtime fixture handoff failed: {error}"),
        }
    }
}

fn take_initial(port: &ProjectGenerationConsumerPort) -> daedalus_runtime::ProjectSourceLease {
    let deadline = Instant::now() + WAIT;
    loop {
        match port.try_take_initial() {
            Ok(Some(lease)) => return lease,
            Ok(None) | Err(daedalus_runtime::ProjectConsumerError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "runtime fixture initial lease was not ready"
                );
                thread::sleep(POLL);
            }
            Err(error) => panic!("runtime fixture initial lease failed: {error}"),
        }
    }
}
