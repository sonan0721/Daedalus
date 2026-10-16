use std::{error::Error, fs, io::Read, path::PathBuf};

use clap::{Parser, Subcommand};
use daedalus_core::{CommandId, EntityHandle, Revision, SessionId, Tick, Transform};
use daedalus_runtime::{EngineBuilder, EngineConfig};
use daedalus_world::{SimulationWorld, WorldCommand, WorldCommandKind};
use serde::Serialize;
use serde_json::json;

mod control;
mod project;
#[cfg(feature = "window")]
mod project_view;
#[cfg(feature = "window")]
mod project_view_control;
#[cfg(feature = "window")]
mod view;

const MAX_CLI_ENTITIES: u32 = 16_384;
const MAX_CLI_TICKS: u32 = 120;
const MAX_SCENE_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "daedalus", about = "Headless Daedalus engine control", version)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Read live ECS state from a bounded test session as JSON.
    Inspect {
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(0..=MAX_CLI_ENTITIES as i64))]
        entities: u32,
    },
    /// Run an exact number of fixed simulation ticks in a headless test session.
    Run {
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=MAX_CLI_TICKS as i64))]
        ticks: u32,
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(0..=MAX_CLI_ENTITIES as i64))]
        entities: u32,
    },
    /// Apply fixed steps one at a time and return each command outcome.
    Step {
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=MAX_CLI_TICKS as i64))]
        ticks: u32,
    },
    /// Keep one bare headless engine alive and exchange bounded JSON Lines requests over stdio.
    /// Project metadata requests are available from `project serve <directory>`.
    Serve,
    /// Print this binary's package version or machine-readable capabilities.
    Version {
        /// Emit the version and compiled capabilities as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Validate or transactionally patch a host-neutral scene document.
    Scene {
        #[command(subcommand)]
        command: SceneCommands,
    },
    /// Initialize, inspect, run, serve, or capture an external project.
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Capture the renderer's material test fixture (GPU-only diagnostic).
    #[cfg(feature = "capture")]
    Capture {
        #[arg(long, default_value = "capture.png")]
        output: PathBuf,
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
    },
    /// Open a native window over one live runtime session (requires `--features window`).
    #[cfg(feature = "window")]
    View(view::ViewArgs),
}

#[derive(Subcommand)]
enum ProjectCommands {
    /// Create an empty data-only project in a new external directory.
    Init {
        directory: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Load and validate a project using the shared content loader.
    Validate { directory: PathBuf },
    /// Inspect the project session's authored stable IDs and live entity state.
    Inspect { directory: PathBuf },
    /// Run an external project for an explicit bounded number of fixed ticks.
    Run {
        directory: PathBuf,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=MAX_CLI_TICKS as i64))]
        ticks: u32,
    },
    /// Serve the project session through the bounded JSON Lines control protocol.
    Serve { directory: PathBuf },
    /// Open the external project with a native window and optional protocol-2 control.
    #[cfg(feature = "window")]
    View(project_view::ProjectViewArgs),
    /// Render the external project's actual mesh and snapshot to PNG (requires `--features capture`).
    #[cfg(feature = "capture")]
    Capture {
        directory: PathBuf,
        #[arg(long, default_value = "capture.png")]
        output: PathBuf,
        #[arg(long, default_value_t = 640, value_parser = clap::value_parser!(u32).range(16..=4096))]
        width: u32,
        #[arg(long, default_value_t = 360, value_parser = clap::value_parser!(u32).range(16..=4096))]
        height: u32,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=MAX_CLI_TICKS as i64))]
        ticks: u32,
    },
}

#[derive(Subcommand)]
enum SceneCommands {
    /// Validate a scene document without modifying it.
    Validate { input: PathBuf },
    /// Apply a revision-checked atomic patch and write the resulting document.
    Patch {
        input: PathBuf,
        patch: PathBuf,
        output: PathBuf,
    },
}

#[derive(Serialize)]
struct EntityState {
    handle: EntityHandle,
    transform: Transform,
    revision: Revision,
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
    match args.command {
        Commands::Inspect { entities } => {
            let mut engine = EngineBuilder::default().build()?;
            for index in 0..entities {
                engine.spawn_transform(Transform {
                    translation: [index as f32, 0.0, 0.0],
                    ..Transform::IDENTITY
                })?;
            }
            let tick = engine.clock().current_tick();
            let output = make_output("inspect", engine.world_mut(), tick, Vec::new());
            engine.shutdown()?;
            print_json(&output)?;
        }
        Commands::Run { ticks, entities } => {
            let mut engine = EngineBuilder::new(run_engine_config(entities)).build()?;
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
            let output = make_output("run", engine.world_mut(), tick, steps);
            engine.shutdown()?;
            print_json(&output)?;
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
            let output = make_output("step", engine.world_mut(), tick, steps);
            engine.shutdown()?;
            print_json(&output)?;
        }
        Commands::Serve => {
            let mut engine = control::create_control_engine()?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            control::serve_jsonl(
                &mut engine,
                stdin.lock(),
                stdout.lock(),
                control::ControlLimits::default(),
            )?;
        }
        Commands::Project { command } => project::run(command)?,
        #[cfg(feature = "capture")]
        Commands::Capture {
            output,
            width,
            height,
        } => {
            let renderer = daedalus_renderer::Renderer::new_offscreen_blocking()?;
            let scene = daedalus_renderer::test_fixtures::metal_roughness_scene();
            let artifact = renderer.render_png(
                &scene,
                &daedalus_renderer::CaptureOptions::new(width, height, &output),
            )?;
            print_json(&json!({
                "fixture": "metal_roughness",
                "path": artifact.path,
                "width": artifact.width,
                "height": artifact.height,
                "rgba8_bytes": artifact.rgba8_bytes,
                "object_count": scene.objects.len(),
                "renderer": {
                    "adapter_name": artifact.renderer.adapter_name,
                    "backend": artifact.renderer.backend,
                    "device_type": artifact.renderer.device_type,
                },
            }))?;
        }
        Commands::Version { json: as_json } => {
            let capabilities = cli_capabilities();
            if as_json {
                print_json(
                    &json!({"cli_version": env!("CARGO_PKG_VERSION"), "capabilities": capabilities}),
                )?;
            } else {
                println!("{}", env!("CARGO_PKG_VERSION"));
            }
        }
        Commands::Scene { command } => match command {
            SceneCommands::Validate { input } => {
                let document: daedalus_content::scene::SceneDocument = read_json(&input)?;
                match daedalus_content::scene::validate_scene(&document) {
                    Ok(()) => print_json(&json!({
                        "valid": true,
                        "schema_version": document.schema_version,
                        "revision": document.revision,
                        "entity_count": document.entities.len(),
                        "asset_count": document.assets.len(),
                    }))?,
                    Err(error) => print_json(&json!({
                        "valid": false,
                        "issues": error.issues.iter().map(|issue| json!({"path": issue.path, "message": issue.message})).collect::<Vec<_>>(),
                    }))?,
                }
            }
            SceneCommands::Patch {
                input,
                patch,
                output,
            } => {
                let document: daedalus_content::scene::SceneDocument = read_json(&input)?;
                let patch: daedalus_content::scene::ScenePatch = read_json(&patch)?;
                let mut store = daedalus_content::scene::SceneStore::new(document)?;
                let snapshot = store.apply_patch(patch)?;
                let bytes = serde_json::to_vec_pretty(&snapshot.document)?;
                if bytes.len() as u64 > MAX_SCENE_FILE_BYTES {
                    return Err("patched scene exceeds the 16 MiB output limit".into());
                }
                fs::write(&output, &bytes)?;
                print_json(&json!({
                    "written": output,
                    "revision": snapshot.document.revision,
                    "entity_count": snapshot.document.entities.len(),
                    "asset_count": snapshot.document.assets.len(),
                }))?;
            }
        },
        #[cfg(feature = "window")]
        Commands::View(options) => view::run(options)?,
    }
    Ok(())
}

fn cli_capabilities() -> serde_json::Value {
    json!({
        "protocol_version": control::CONTROL_PROTOCOL_VERSION,
        "serve": true,
        "scene_validate": true,
        "scene_patch": true,
        "project": true,
        "capture": cfg!(feature = "capture"),
        "window": cfg!(feature = "window"),
        "renderer_window_compiled": cfg!(feature = "window"),
    })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &PathBuf) -> Result<T, Box<dyn Error>> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_SCENE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SCENE_FILE_BYTES {
        return Err(format!("{} exceeds the 16 MiB input limit", path.display()).into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn run_engine_config(entities: u32) -> EngineConfig {
    let requested_entities = entities as usize;
    let defaults = EngineConfig::default();
    EngineConfig {
        command_capacity: requested_entities.max(defaults.command_capacity),
        command_dedup_capacity: requested_entities.max(defaults.command_dedup_capacity),
        ..defaults
    }
}

fn print_json(value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string(value)?);
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
                .map_or(Revision(0), |(_, revision)| revision);
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

#[cfg(test)]
mod tests {
    use daedalus_core::{Revision, Tick, Transform};
    use daedalus_world::{CommandOutcome, CommandSubmission, WorldCommand, WorldCommandKind};

    use super::{MAX_CLI_ENTITIES, cli_capabilities, run_engine_config};
    use daedalus_runtime::{EngineBuilder, EngineConfig};

    #[test]
    fn run_queue_scales_to_the_bounded_requested_entity_count() {
        let requested = EngineConfig::default().command_capacity as u32 + 1;
        assert!(requested <= MAX_CLI_ENTITIES);
        let mut engine = EngineBuilder::new(run_engine_config(requested))
            .build()
            .unwrap();
        for index in 0..requested {
            let handle = engine.spawn_transform(Transform::IDENTITY).unwrap();
            assert_eq!(
                engine
                    .try_command(WorldCommand {
                        command_id: daedalus_core::CommandId::from_raw(u128::from(index) + 1),
                        target: handle,
                        expected_revision: Revision(0),
                        apply_at: Tick(1),
                        kind: WorldCommandKind::SetTransform(Transform {
                            translation: [0.0, 1.0, 0.0],
                            ..Transform::IDENTITY
                        }),
                    })
                    .unwrap(),
                CommandSubmission::Enqueued,
            );
        }
        let report = engine.step_tick().unwrap();
        assert_eq!(report.commands.len(), requested as usize);
        assert!(
            report
                .commands
                .iter()
                .all(|command| matches!(&command.outcome, CommandOutcome::Applied { .. }))
        );
        engine.shutdown().unwrap();
    }

    #[test]
    fn capabilities_report_only_compiled_renderer_features() {
        let capabilities = cli_capabilities();
        assert_eq!(capabilities["window"], cfg!(feature = "window"));
        assert_eq!(
            capabilities["renderer_window_compiled"],
            cfg!(feature = "window")
        );
        assert_eq!(capabilities["capture"], cfg!(feature = "capture"));
    }

    #[cfg(feature = "window")]
    #[test]
    fn view_parser_accepts_bounded_configuration_and_rejects_out_of_range_work() {
        use super::Args;
        use clap::Parser;
        assert!(
            Args::try_parse_from([
                "daedalus",
                "view",
                "--width",
                "4096",
                "--height",
                "64",
                "--seconds",
                "3600",
                "--work-iterations",
                "10000",
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["daedalus", "view", "--work-iterations", "10001",]).is_err());
        assert!(Args::try_parse_from(["daedalus", "view", "--seconds", "0"]).is_err());
    }
}
