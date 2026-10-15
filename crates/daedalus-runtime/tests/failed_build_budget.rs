use daedalus_runtime::{EngineBuildError, EngineBuilder, EngineConfig};

#[test]
fn failed_task_pool_build_does_not_initialize_the_process_global_ecs_pool() {
    let invalid = EngineConfig {
        compute_workers: 2,
        task_workers: 0,
        ..EngineConfig::default()
    };
    assert!(matches!(
        EngineBuilder::new(invalid).build(),
        Err(EngineBuildError::TaskPool(_))
    ));

    let valid = EngineConfig {
        compute_workers: 1,
        ..EngineConfig::default()
    };
    let engine = EngineBuilder::new(valid)
        .build()
        .expect("the first successful engine build should set the requested pool budget");
    assert_eq!(engine.cpu_budget().ecs_compute_workers, 1);
    engine.shutdown().unwrap();
}
