//! CLI lifecycle commands for external, data-only projects.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use daedalus_content::project::{PreparedProject, ProjectManifest, prepare_project_from_bytes};
use daedalus_content::scene::SceneDocument;
use daedalus_runtime::{EngineConfig, ProjectSession, ProjectSessionReport, StepReport};
use serde_json::{Value, json};

use super::{MAX_CLI_TICKS, ProjectCommands, print_json};

pub(super) fn run(command: ProjectCommands) -> Result<(), Box<dyn Error>> {
    match command {
        ProjectCommands::Init { directory, name } => {
            let result = create_empty_project(&directory, name.as_deref())?;
            print_json(&result)?;
        }
        ProjectCommands::Validate { directory } => {
            let prepared = super::project_assets::load_project_read_only(&directory)?;
            print_json(&validation_output(&prepared))?;
        }
        ProjectCommands::Cook {
            directory,
            cache_dir,
        } => super::project_assets::run(directory, cache_dir)?,
        ProjectCommands::Inspect { directory } => {
            let session = ProjectSession::new(
                super::project_assets::load_project_read_only(&directory)?,
                EngineConfig::default(),
            )?;
            let report = session.report()?;
            session.engine().shutdown()?;
            print_json(&json!({"command":"project_inspect","project":report}))?;
        }
        ProjectCommands::Run { directory, ticks } => {
            if ticks == 0 || ticks > MAX_CLI_TICKS {
                return Err(format!("tick count must be in 1..={MAX_CLI_TICKS}").into());
            }
            let mut session = ProjectSession::new(
                super::project_assets::load_project_runtime(&directory)?,
                EngineConfig::default(),
            )?;
            let mut steps = Vec::with_capacity(ticks as usize);
            for _ in 0..ticks {
                steps.push(session.step_tick()?);
            }
            let report = session.report()?;
            session.engine().shutdown()?;
            print_json(&run_output(report, steps))?;
        }
        ProjectCommands::Serve { directory } => {
            let mut session = ProjectSession::new(
                super::project_assets::load_project_runtime(&directory)?,
                EngineConfig::default(),
            )?;
            let info = project_info(&session);
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            super::control::serve_project_jsonl(
                session.engine_mut(),
                stdin.lock(),
                stdout.lock(),
                super::control::ControlLimits::default(),
                info,
            )?;
        }
        #[cfg(feature = "window")]
        ProjectCommands::View(args) => super::project_view::run(args)?,
        #[cfg(feature = "capture")]
        ProjectCommands::Capture {
            directory,
            output,
            width,
            height,
            ticks,
        } => capture_project(&directory, output, width, height, ticks)?,
    }
    Ok(())
}

fn create_empty_project(
    directory: &Path,
    requested_name: Option<&str>,
) -> Result<Value, Box<dyn Error>> {
    let target = resolve_new_directory(directory)?;
    let name = requested_name
        .map(str::to_owned)
        .or_else(|| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or("project name is empty")?;
    let manifest = ProjectManifest::new(name);
    let scene = SceneDocument::default();
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let scene_bytes = serde_json::to_vec_pretty(&scene)?;
    let prepared = prepare_project_from_bytes(
        &manifest_bytes,
        &scene_bytes,
        &BTreeMap::new(),
        daedalus_content::project::ProjectLimits::default(),
    )?;

    fs::create_dir(&target)?;
    let manifest_path = target.join("daedalus.project.json");
    if let Err(error) = write_new_file(&manifest_path, &manifest_bytes) {
        let _ = fs::remove_dir(&target);
        return Err(error);
    }
    if let Err(error) = write_new_file(&target.join("scene.json"), &scene_bytes) {
        let _ = fs::remove_file(target.join("daedalus.project.json"));
        let _ = fs::remove_dir(&target);
        return Err(error);
    }

    Ok(json!({
        "command":"project_init",
        "directory":target,
        "project_name":prepared.manifest().name,
        "schema_version":prepared.manifest().schema_version,
        "scene":prepared.manifest().scene,
        "entity_count":prepared.entities().len(),
        "asset_count":prepared.scene().assets.len(),
        "source_digest":prepared.digest(),
        "files":source_files(&prepared),
    }))
}

fn resolve_new_directory(directory: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let absolute = if directory.is_absolute() {
        directory.to_path_buf()
    } else {
        std::env::current_dir()?.join(directory)
    };
    let parent = absolute.parent().ok_or_else(|| {
        format!(
            "{} must name a directory under an existing parent",
            directory.display()
        )
    })?;
    let name = absolute
        .file_name()
        .ok_or_else(|| format!("{} must name a new project directory", directory.display()))?;
    let canonical_parent = fs::canonicalize(parent)?;
    let target = canonical_parent.join(name);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already exists; project init never overwrites",
            target.display()
        )
        .into());
    }
    Ok(target)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(Box::new(error));
    }
    Ok(())
}

fn validation_output(prepared: &PreparedProject) -> Value {
    json!({
        "command":"project_validate",
        "valid":true,
        "project_name":prepared.manifest().name,
        "schema_version":prepared.manifest().schema_version,
        "source_scene_revision":prepared.scene().revision,
        "entity_count":prepared.entities().len(),
        "asset_count":prepared.scene().assets.len(),
        "source_digest":prepared.digest(),
        "files":source_files(prepared),
    })
}

fn run_output(report: ProjectSessionReport, steps: Vec<StepReport>) -> Value {
    json!({
        "command":"project_run",
        "project_name":report.project_name,
        "session":report.session,
        "tick":report.tick,
        "source_schema_version":report.source_schema_version,
        "source_scene_revision":report.source_scene_revision,
        "source_digest":report.source_digest,
        "entity_count":report.entities.len(),
        "entities":report.entities,
        "steps":steps,
    })
}

fn project_info(session: &ProjectSession) -> Value {
    let prepared = session.project();
    let entities = prepared
        .entities()
        .iter()
        .filter_map(|entity| {
            session.entity_handle(&entity.id).map(|handle| {
                json!({
                    "id":entity.id,
                    "handle":handle,
                    "name":entity.name,
                })
            })
        })
        .collect::<Vec<_>>();
    json!({
        "project_name":prepared.manifest().name,
        "source_schema_version":prepared.scene().schema_version,
        "source_scene_revision":prepared.scene().revision,
        "source_digest":prepared.digest(),
        "source_files":source_files(prepared),
        "entities":entities,
    })
}

fn source_files(prepared: &PreparedProject) -> Vec<Value> {
    prepared
        .files()
        .iter()
        .map(|file| {
            json!({
                "source_id":file.source_id,
                "content_sha256":file.content_sha256,
                "byte_len":file.byte_len,
            })
        })
        .collect()
}

#[cfg(feature = "capture")]
fn capture_project(
    directory: &Path,
    output: PathBuf,
    width: u32,
    height: u32,
    ticks: u32,
) -> Result<(), Box<dyn Error>> {
    if ticks == 0 || ticks > MAX_CLI_TICKS {
        return Err(format!("tick count must be in 1..={MAX_CLI_TICKS}").into());
    }
    let mut session = ProjectSession::new(
        super::project_assets::load_project_runtime(directory)?,
        EngineConfig::default(),
    )?;
    for _ in 0..ticks {
        session.step_tick()?;
    }
    let report = session.report()?;
    let snapshot = session
        .latest_snapshot()?
        .ok_or("project session did not publish a render snapshot after stepping")?;
    let prepared = session.project();
    let bindings = report
        .entities
        .iter()
        .map(|entity| (entity.id.clone(), entity.handle))
        .collect::<BTreeMap<_, _>>();
    let packet_limits = daedalus_renderer::PacketRendererLimits::default();
    let resources = daedalus_renderer::ProjectRenderResources::prepare(
        prepared,
        1,
        report.session,
        &bindings,
        packet_limits,
    )?;
    let packet = resources.extract(&snapshot, None)?;
    let projection_counts = packet_projection_counts(&packet)?;
    session.engine().shutdown()?;

    let renderer = daedalus_renderer::Renderer::new_offscreen_blocking()?;
    let mut packet_renderer = renderer.into_packet_renderer_with_asset_limits(
        packet_limits,
        daedalus_renderer::PacketAssetLimits::default(),
    )?;
    let artifact = packet_renderer.render_packet_png(
        &packet,
        &daedalus_renderer::CaptureOptions::new(width, height, &output),
    )?;
    let asset_resources =
        super::project_assets::asset_resource_stats_json(&packet_renderer.asset_resource_stats());
    let resources =
        super::project_assets::packet_resource_stats_json(&packet_renderer.resource_stats());
    let authority_entity_revisions = report
        .entities
        .iter()
        .map(|entity| json!({"id":entity.id,"revision":entity.revision}))
        .collect::<Vec<_>>();
    print_json(&json!({
        "command":"project_capture",
        "project_name":report.project_name,
        "session":packet.session(),
        "authority_tick":report.tick,
        "snapshot_tick":packet.tick(),
        "source_schema_version":report.source_schema_version,
        "source_scene_revision":report.source_scene_revision,
        "source_digest":report.source_digest,
        "source_files":source_files(prepared),
        "authority_entity_revisions":authority_entity_revisions,
        "projection":{
            "object_count":packet.instances().len(),
            "expanded_vertices":projection_counts.0,
            "expanded_indices":projection_counts.1,
            "expanded_geometry_bytes":projection_counts.2,
            "expanded_geometry_bytes_basis":"per_draw_packed_geometry_plus_legacy_object_record",
            "packet_expanded_gpu_geometry_bytes":projection_counts.3,
        },
        "resources":resources,
        "asset_resources":asset_resources,
        "capture":{
            "path":artifact.path,
            "width":artifact.width,
            "height":artifact.height,
            "rgba8_bytes":artifact.rgba8_bytes,
            "renderer":{
                "adapter_name":artifact.renderer.adapter_name,
                "backend":artifact.renderer.backend,
                "device_type":artifact.renderer.device_type,
            },
        },
        "note":"capture used a new project session; it is not a frame from another serve or visible window session"
    }))?;
    Ok(())
}

/// Retains the legacy CLI projection estimate while separately reporting exact packed mesh bytes.
/// The compatibility value sums each draw's actual layout-aware geometry bytes plus the public
/// `RenderObject` record size; it does not recreate a `RenderScene` or infer an imported stride.
#[cfg(feature = "capture")]
fn packet_projection_counts(
    packet: &daedalus_renderer::RenderPacket,
) -> Result<(usize, usize, usize, usize), &'static str> {
    let object_record_bytes = std::mem::size_of::<daedalus_renderer::RenderObject>();
    packet
        .instances()
        .iter()
        .try_fold(
            (0usize, 0usize, 0usize, 0usize),
            |(vertices, indices, compatibility_bytes, gpu_geometry_bytes), instance| {
                let mesh_bytes = instance.mesh.gpu_bytes();
                Some((
                    vertices.checked_add(instance.mesh.vertex_count())?,
                    indices.checked_add(instance.mesh.index_count())?,
                    compatibility_bytes
                        .checked_add(mesh_bytes)?
                        .checked_add(object_record_bytes)?,
                    gpu_geometry_bytes.checked_add(mesh_bytes)?,
                ))
            },
        )
        .ok_or("project packet expanded geometry counters overflowed")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use daedalus_content::project::{ProjectError, load_project};

    use super::{ProjectCommands, create_empty_project};
    use crate::Args;

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("daedalus-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn project_run_parser_requires_a_bounded_explicit_tick_count() {
        assert!(
            Args::try_parse_from(["daedalus", "project", "run", "/tmp/example", "--ticks", "1"])
                .is_ok()
        );
        assert!(
            Args::try_parse_from(["daedalus", "project", "run", "/tmp/example", "--ticks", "0"])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "daedalus",
                "project",
                "run",
                "/tmp/example",
                "--ticks",
                "121"
            ])
            .is_err()
        );
    }

    #[test]
    fn init_writes_and_validates_an_empty_project_without_overwriting() {
        let path = temporary_directory("init");
        let output = create_empty_project(&path, Some("empty")).unwrap();
        assert_eq!(output["entity_count"], 0);
        assert_eq!(output["asset_count"], 0);
        let prepared = load_project(&path).unwrap();
        assert!(prepared.entities().is_empty());
        assert!(prepared.meshes().is_empty());
        assert!(create_empty_project(&path, Some("replacement")).is_err());
        assert_eq!(load_project(&path).unwrap().manifest().name, "empty");
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn shared_loader_reports_project_source_path_errors() {
        let path = temporary_directory("bad-source");
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("daedalus.project.json"),
            br#"{"schema_version":1,"name":"bad","scene":"../outside.json"}"#,
        )
        .unwrap();
        let error = load_project(&path).unwrap_err();
        assert!(matches!(
            error,
            ProjectError::Invalid {
                ref source_id,
                ref path,
                ..
            } if source_id == "daedalus.project.json" && path == "scene"
        ));
        assert!(error.to_string().contains("parent traversal"));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn project_command_surface_includes_all_lifecycle_actions() {
        let commands = [
            ["daedalus", "project", "init", "/tmp/new"],
            ["daedalus", "project", "validate", "/tmp/new"],
            ["daedalus", "project", "inspect", "/tmp/new"],
            ["daedalus", "project", "serve", "/tmp/new"],
            ["daedalus", "project", "cook", "/tmp/new"],
        ];
        for command in commands {
            assert!(Args::try_parse_from(command).is_ok());
        }
        assert!(matches!(
            Args::try_parse_from(["daedalus", "project", "init", "/tmp/new"])
                .unwrap()
                .command,
            super::super::Commands::Project {
                command: ProjectCommands::Init { .. }
            }
        ));
        assert!(matches!(
            Args::try_parse_from([
                "daedalus",
                "project",
                "cook",
                "/tmp/new",
                "--cache-dir",
                "/tmp/asset-cache"
            ])
            .unwrap()
            .command,
            super::super::Commands::Project {
                command: ProjectCommands::Cook { .. }
            }
        ));
    }

    #[cfg(feature = "capture")]
    #[test]
    fn project_capture_parser_bounds_resolution_and_snapshot_ticks() {
        assert!(
            Args::try_parse_from([
                "daedalus",
                "project",
                "capture",
                "/tmp/example",
                "--width",
                "4096",
                "--height",
                "16",
                "--ticks",
                "120"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "daedalus",
                "project",
                "capture",
                "/tmp/example",
                "--width",
                "15"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "daedalus",
                "project",
                "capture",
                "/tmp/example",
                "--ticks",
                "121"
            ])
            .is_err()
        );
    }

    #[cfg(feature = "capture")]
    #[test]
    fn packet_capture_projection_counts_match_legacy_for_mesh_project() {
        use std::collections::BTreeMap;

        use daedalus_content::project::{ProjectLimits, prepare_project_from_bytes};
        use daedalus_runtime::{EngineConfig, ProjectSession};

        use super::packet_projection_counts;

        let manifest = serde_json::json!({
            "schema_version": 1,
            "name": "capture-counter-regression",
            "scene": "scene.json"
        });
        let scene = serde_json::json!({
            "schema_version": 1,
            "revision": 0,
            "entities": [{
                "id": "entity:counter",
                "components": [
                    {
                        "id": "component:transform",
                        "type_id": "daedalus.transform",
                        "data": {
                            "translation": [0.0, 0.0, 0.0],
                            "rotation": [0.0, 0.0, 0.0, 1.0],
                            "scale": [1.0, 1.0, 1.0]
                        }
                    },
                    {
                        "id": "component:mesh-renderer",
                        "type_id": "daedalus.mesh_renderer",
                        "asset_refs": ["asset:mesh"],
                        "data": {
                            "mesh": "asset:mesh",
                            "material": {
                                "base_color": [0.4, 0.5, 0.6],
                                "metallic": 0.0,
                                "roughness": 0.8,
                                "emissive": [0.0, 0.0, 0.0]
                            }
                        }
                    }
                ]
            }],
            "assets": [{
                "id": "asset:mesh",
                "kind": "daedalus.mesh",
                "uri": "mesh.json",
                "metadata": {}
            }]
        });
        let mesh = serde_json::json!({
            "schema_version": 1,
            "vertices": [
                {"position": [-0.5, -0.5, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [0.5, -0.5, 0.0], "normal": [0.0, 0.0, 1.0]},
                {"position": [0.0, 0.5, 0.0], "normal": [0.0, 0.0, 1.0]}
            ],
            "indices": [0, 1, 2]
        });
        let mut assets = BTreeMap::new();
        assets.insert("mesh.json".to_owned(), serde_json::to_vec(&mesh).unwrap());
        let prepared = prepare_project_from_bytes(
            &serde_json::to_vec(&manifest).unwrap(),
            &serde_json::to_vec(&scene).unwrap(),
            &assets,
            ProjectLimits::default(),
        )
        .unwrap();
        let mut session = ProjectSession::new(prepared.clone(), EngineConfig::default()).unwrap();
        session.step_tick().unwrap();
        let report = session.report().unwrap();
        let snapshot = session.latest_snapshot().unwrap().unwrap();
        let bindings = report
            .entities
            .iter()
            .map(|entity| (entity.id.clone(), entity.handle))
            .collect::<BTreeMap<_, _>>();
        let packet_limits = daedalus_renderer::PacketRendererLimits::default();
        let resources = daedalus_renderer::ProjectRenderResources::prepare(
            &prepared,
            1,
            report.session,
            &bindings,
            packet_limits,
        )
        .unwrap();
        let packet = resources.extract(&snapshot, None).unwrap();
        let old =
            daedalus_renderer::project_snapshot(&prepared, report.session, &bindings, &snapshot)
                .unwrap();
        let counts = packet_projection_counts(&packet).unwrap();
        assert_eq!(packet.instances().len(), old.object_count);
        assert_eq!(counts.0, old.expanded_vertices);
        assert_eq!(counts.1, old.expanded_indices);
        assert_eq!(counts.2, old.expanded_geometry_bytes);
        assert_eq!(
            counts.3
                + packet.instances().len() * std::mem::size_of::<daedalus_renderer::RenderObject>(),
            counts.2
        );
        session.engine().shutdown().unwrap();
    }
}
