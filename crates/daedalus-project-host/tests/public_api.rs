use daedalus_project_host::{
    NativePresentedSummary, NativeProjectHost, NativeProjectHostFailure,
    NativeProjectHostLifecycle, NativeProjectHostOptions, NativeProjectHostShutdown,
    NativeProjectHostStatus, NativeProjectHostStatusRead, SessionShutdownReport,
};
use daedalus_runtime::{
    ProjectPublicationIdentity, RequestPhase, SessionClockMode, SessionControlConfig,
    SessionOperation, SessionRequestResult, SessionSubmitError,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[test]
fn status_accessor_types_are_publicly_nameable() {
    let _: fn(&NativeProjectHostStatus) -> Option<&NativeProjectHostFailure> =
        NativeProjectHostStatus::failure;
    let _: fn(&NativeProjectHostStatus) -> Option<&NativePresentedSummary> =
        NativeProjectHostStatus::last_presented;
    let _: fn(&NativeProjectHostStatus) -> Option<&ProjectPublicationIdentity> =
        NativeProjectHostStatus::last_queued_publication;
    let _: fn(&NativeProjectHost) -> Option<&SessionShutdownReport> =
        NativeProjectHost::shutdown_report;
}

static NEXT_PROJECT: AtomicU64 = AtomicU64::new(0);

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> Self {
        let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daedalus-project-host-public-api-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("daedalus.project.json"),
            br#"{"schema_version":1,"name":"public-host-api-test","scene":"scene.json"}"#,
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

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn execute(
    client: &daedalus_runtime::SessionControlClient,
    operation: SessionOperation,
) -> SessionRequestResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ticket = loop {
        match client.try_submit(operation.clone()) {
            Ok(ticket) => break ticket,
            Err(SessionSubmitError::Busy | SessionSubmitError::QueueFull) => {
                assert!(
                    Instant::now() < deadline,
                    "session request admission timed out"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("public session request was rejected: {error}"),
        }
    };
    loop {
        match ticket.try_state() {
            Ok(progress) if progress.phase() == RequestPhase::Terminal => {
                let terminal = progress
                    .terminal()
                    .cloned()
                    .expect("terminal result exists");
                drop(progress);
                ticket
                    .acknowledge()
                    .expect("terminal result is acknowledged");
                return terminal.expect("session request completed successfully");
            }
            Ok(_) | Err(daedalus_runtime::RequestReadError::Busy) => {
                assert!(
                    Instant::now() < deadline,
                    "session request completion timed out"
                );
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("public session request could not be read: {error}"),
        }
    }
}

fn shutdown(host: &mut NativeProjectHost) {
    host.request_stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match host
            .try_shutdown()
            .expect("public host shutdown stays healthy")
        {
            NativeProjectHostShutdown::Stopped => break,
            NativeProjectHostShutdown::Stopping => {
                assert!(Instant::now() < deadline, "public host did not stop");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn read_host_status(host: &NativeProjectHost) -> NativeProjectHostStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match host.try_status().expect("public host status is readable") {
            NativeProjectHostStatusRead::Value(status) => return status,
            NativeProjectHostStatusRead::Busy => {
                assert!(Instant::now() < deadline, "host status remained busy");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

#[test]
fn public_host_attaches_steps_reads_and_stops_the_same_manual_session() {
    let project = TempProject::new();
    let mut host = NativeProjectHost::start(NativeProjectHostOptions::for_project(project.path()))
        .expect("public SDK starts a valid native project host");
    let client = host
        .attach_control()
        .expect("public client attaches to the host's existing actor");

    let before = match execute(&client, SessionOperation::Status) {
        SessionRequestResult::Status(status) => status,
        other => panic!("status operation returned {other:?}"),
    };
    assert_eq!(before.clock, SessionClockMode::Manual);
    assert_eq!(
        read_host_status(&host).session(),
        before.session,
        "host and attached client identify the same session"
    );
    match execute(&client, SessionOperation::Step { count: 1 }) {
        SessionRequestResult::Step(reports) => assert_eq!(reports.len(), 1),
        other => panic!("manual step returned {other:?}"),
    }
    let after = match execute(&client, SessionOperation::Status) {
        SessionRequestResult::Status(status) => status,
        other => panic!("status operation returned {other:?}"),
    };
    assert!(
        after.tick > before.tick,
        "the attached actor advanced one step"
    );
    assert_eq!(after.session, before.session);

    drop(client);
    shutdown(&mut host);
    let stopped = read_host_status(&host);
    assert_eq!(stopped.lifecycle(), NativeProjectHostLifecycle::Stopped);
    assert_eq!(stopped.session(), before.session);
    assert_eq!(stopped.last_queued_publication(), None);
    assert!(host.shutdown_report().is_some());
}

#[test]
fn public_host_preserves_a_caller_selected_realtime_clock() {
    let project = TempProject::new();
    let config = SessionControlConfig {
        clock_mode: SessionClockMode::Realtime,
        ..SessionControlConfig::default()
    };
    let mut host = NativeProjectHost::start(
        NativeProjectHostOptions::for_project(project.path()).with_session_config(config),
    )
    .expect("public SDK starts the requested realtime session");
    let client = host
        .attach_control()
        .expect("public client attaches to the realtime actor");
    let before = match execute(&client, SessionOperation::Status) {
        SessionRequestResult::Status(status) => status,
        other => panic!("status operation returned {other:?}"),
    };
    assert_eq!(before.clock, SessionClockMode::Realtime);

    let deadline = Instant::now() + Duration::from_secs(3);
    let after = loop {
        thread::sleep(Duration::from_millis(20));
        let status = match execute(&client, SessionOperation::Status) {
            SessionRequestResult::Status(status) => status,
            other => panic!("status operation returned {other:?}"),
        };
        if status.tick > before.tick {
            break status;
        }
        assert!(Instant::now() < deadline, "realtime actor did not advance");
    };
    assert_eq!(after.session, before.session);
    drop(client);
    shutdown(&mut host);
}
