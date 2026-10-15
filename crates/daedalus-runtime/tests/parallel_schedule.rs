use std::sync::{
    Condvar, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use daedalus_runtime::{EngineBuilder, EngineConfig, bevy_ecs::prelude::*};

#[derive(Resource, Default)]
struct OverlapProbe {
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    reached_two: Mutex<bool>,
    pair_ready: Condvar,
    timed_out: AtomicBool,
}

fn rendezvous(probe: &OverlapProbe) {
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum_active.fetch_max(active, Ordering::AcqRel);
    if active == 2 {
        let mut reached = probe.reached_two.lock().unwrap();
        *reached = true;
        probe.pair_ready.notify_all();
    } else {
        let reached = probe.reached_two.lock().unwrap();
        let (reached, timeout) = probe
            .pair_ready
            .wait_timeout_while(reached, Duration::from_secs(2), |reached| !*reached)
            .unwrap();
        if timeout.timed_out() && !*reached {
            probe.timed_out.store(true, Ordering::Release);
        }
    }
    probe.active.fetch_sub(1, Ordering::AcqRel);
}

fn independent_left(probe: Res<OverlapProbe>) {
    rendezvous(&probe);
}

fn independent_right(probe: Res<OverlapProbe>) {
    rendezvous(&probe);
}

#[derive(Resource, Default)]
struct ExclusiveProbe {
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    runs: AtomicUsize,
}

fn mutate(probe: &ExclusiveProbe) {
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum_active.fetch_max(active, Ordering::AcqRel);
    std::thread::sleep(Duration::from_millis(5));
    probe.runs.fetch_add(1, Ordering::Relaxed);
    probe.active.fetch_sub(1, Ordering::AcqRel);
}

fn conflicting_left(probe: ResMut<ExclusiveProbe>) {
    mutate(&probe);
}

fn conflicting_right(probe: ResMut<ExclusiveProbe>) {
    mutate(&probe);
}

#[test]
fn independent_systems_overlap_and_conflicting_mutable_access_serializes() {
    let mut builder = EngineBuilder::new(EngineConfig {
        // This integration test has its own test process and initializes the
        // Bevy process-global pool with two workers before running any schedule.
        compute_workers: 2,
        ..EngineConfig::default()
    });
    builder.add_gameplay_systems((independent_left, independent_right));
    builder.add_gameplay_systems((conflicting_left, conflicting_right));
    let mut engine = builder.build().unwrap();
    engine
        .world_mut()
        .ecs_mut()
        .insert_resource(OverlapProbe::default());
    engine
        .world_mut()
        .ecs_mut()
        .insert_resource(ExclusiveProbe::default());
    engine.step_tick().unwrap();

    let overlap = engine.world().ecs().resource::<OverlapProbe>();
    assert!(!overlap.timed_out.load(Ordering::Acquire));
    assert!(
        overlap.maximum_active.load(Ordering::Acquire) >= 2,
        "independent systems should execute at the same time"
    );
    let exclusive = engine.world().ecs().resource::<ExclusiveProbe>();
    assert_eq!(exclusive.runs.load(Ordering::Relaxed), 2);
    assert_eq!(exclusive.maximum_active.load(Ordering::Acquire), 1);
}
