use std::error::Error;

use clap::{Parser, Subcommand};
use daedalus_core::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};
use daedalus_runtime::EngineBuilder;
use daedalus_world::{SimulationWorld, WorldCommand, WorldCommandKind};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "daedalus", about = "Headless Daedalus engine control")]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Read a live ECS state as JSON.
    Inspect {
        #[arg(long, default_value_t = 1)]
        entities: u32,
    },
    /// Run an exact number of fixed simulation ticks without initializing a renderer.
    Run {
        #[arg(long, default_value_t = 3)]
        ticks: u32,
        #[arg(long, default_value_t = 2)]
        entities: u32,
    },
    /// Apply one simulation step at a time, returning each command and snapshot outcome.
    Step {
        #[arg(long, default_value_t = 1)]
        ticks: u32,
    },
}

#[derive(Serialize)]
struct EntityState {
    handle: EntityHandle,
    transform: Transform,
    revision: u64,
}

#[derive(Serialize)]
struct CliOutput {
    command: &'static str,
    session: SessionId,
    tick: Tick,
    entity_count: usize,
    entities: Vec<EntityState>,
    steps: Vec<daedalus_runtime::StepReport>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let output = match args.command {
        Commands::Inspect { entities } => {
            let mut engine = EngineBuilder::default().build()?;
            for index in 0..entities {
                engine.spawn_transform(Transform {
                    translation: [index as f32, 0.0, 0.0],
                    ..Transform::IDENTITY
                })?;
            }
            let tick = engine.clock().current_tick();
            make_output("inspect", engine.world_mut(), tick, Vec::new())
        }
        Commands::Run { ticks, entities } => {
            let mut engine = EngineBuilder::default().build()?;
            for index in 0..entities {
                let handle = engine.spawn_transform(Transform {
                    translation: [index as f32, 0.0, 0.0],
                    ..Transform::IDENTITY
                })?;
                engine.try_command(WorldCommand {
                    command_id: CommandId::from_raw(u128::from(index) + 1),
                    target: handle,
                    expected_revision: Revision(0),
                    apply_at: Tick(1),
                    kind: WorldCommandKind::SetTransform(Transform {
                        translation: [index as f32, 1.0, 0.0],
                        ..Transform::IDENTITY
                    }),
                })?;
            }
            let steps = engine.run_ticks(ticks)?;
            let tick = engine.clock().current_tick();
            make_output("run", engine.world_mut(), tick, steps)
        }
        Commands::Step { ticks } => {
            let mut engine = EngineBuilder::default().build()?;
            let handle = engine.spawn_transform(Transform::IDENTITY)?;
            engine.try_command(WorldCommand {
                command_id: CommandId::from_raw(1),
                target: handle,
                expected_revision: Revision(0),
                apply_at: Tick(1),
                kind: WorldCommandKind::SetTransform(Transform {
                    translation: [1.0, 0.0, 0.0],
                    ..Transform::IDENTITY
                }),
            })?;
            let mut steps = Vec::with_capacity(ticks as usize);
            for _ in 0..ticks {
                steps.push(engine.step_tick()?);
            }
            let tick = engine.clock().current_tick();
            make_output("step", engine.world_mut(), tick, steps)
        }
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn make_output(
    command: &'static str,
    world: &mut SimulationWorld,
    tick: Tick,
    steps: Vec<daedalus_runtime::StepReport>,
) -> CliOutput {
    let mut ecs_snapshot = world
        .render_entities()
        .into_iter()
        .map(|entity| {
            let revision = world
                .transform(entity.entity)
                .map_or(0, |(_, revision)| revision.0);
            EntityState {
                handle: entity.entity,
                transform: entity.transform,
                revision,
            }
        })
        .collect::<Vec<_>>();
    ecs_snapshot.sort_by_key(|entity| (entity.handle.index, entity.handle.generation));
    CliOutput {
        command,
        session: world.session(),
        tick,
        entity_count: world.entity_count(),
        entities: ecs_snapshot,
        steps,
    }
}
