//! CLI lifecycle commands for external, data-only projects.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use daedalus_content::project::{
    PreparedProject, ProjectManifest, load_project, prepare_project_from_bytes,
};
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
            let prepared = load_project(&directory)?;
            print_json(&validation_output(&prepared))?;
        }
        ProjectCommands::Inspect { directory } => {
            let session = ProjectSession::new(load_project(&directory)?, EngineConfig::default())?;
            let report = session.report()?;
            session.engine().shutdown()?;
            print_json(&json!({"command":"project_inspect","project":report}))?;
        }
        ProjectCommands::Run { directory, ticks } => {
            if ticks == 0 || ticks > MAX_CLI_TICKS {
                return Err(format!("tick count must be in 1..={MAX_CLI_TICKS}").into());
            }
            let mut session =
                ProjectSession::new(load_project(&directory)?, EngineConfig::default())?;
            let mut steps = Vec::with_capacity(ticks as usize);
            for _ in 0..ticks {
                steps.push(session.step_tick()?);
            }
            let report = session.report()?;
            session.engine().shutdown()?;
            print_json(&run_output(report, steps))?;
        }
        ProjectCommands::Serve { directory } => {
            let mut session =
                ProjectSession::new(load_project(&directory)?, EngineConfig::default())?;
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
    let mut session = ProjectSession::new(load_project(directory)?, EngineConfig::default())?;
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
    let projection =
        daedalus_renderer::project_snapshot(prepared, report.session, &bindings, &snapshot)?;
    session.engine().shutdown()?;

    let renderer = daedalus_renderer::Renderer::new_offscreen_blocking()?;
    let artifact = renderer.render_png(
        &projection.scene,
        &daedalus_renderer::CaptureOptions::new(width, height, &output),
    )?;
    let authority_entity_revisions = report
        .entities
        .iter()
        .map(|entity| json!({"id":entity.id,"revision":entity.revision}))
        .collect::<Vec<_>>();
    print_json(&json!({
        "command":"project_capture",
        "project_name":report.project_name,
        "session":projection.session,
        "authority_tick":report.tick,
        "snapshot_tick":projection.snapshot_tick,
        "source_schema_version":report.source_schema_version,
        "source_scene_revision":report.source_scene_revision,
        "source_digest":report.source_digest,
        "source_files":source_files(prepared),
        "authority_entity_revisions":authority_entity_revisions,
        "projection":{
            "object_count":projection.object_count,
            "expanded_vertices":projection.expanded_vertices,
            "expanded_indices":projection.expanded_indices,
            "expanded_geometry_bytes":projection.expanded_geometry_bytes,
        },
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
}
