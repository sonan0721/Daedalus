use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

use daedalus_content::{
    ByteMapSourceResolver, ImportCancellation, ImportLimits, MemoryCookCache,
    prepare_project_with_assets, scene::StableId,
};
use daedalus_core::{
    EntityHandle, RenderEntitySnapshot, RenderSnapshot, SessionId, Tick, Transform,
};
use serde_json::json;

use super::super::PacketRenderer;
use super::*;
use crate::{
    CpuFrameHandle, PacketCpuLimits, PacketCpuOwner, PacketRendererDrainStatus, PacketStageError,
    PacketStageFrameSubmission, PacketStageId, PacketStageInput, PacketStageLimits,
    PacketStageState, SourcePin,
};

const STAGE_DEADLINE: Duration = Duration::from_secs(20);
const STAGE_PUMP_LIMIT: usize = 20_000;
const MODEL_VERTEX_SOURCE_BYTES: usize = 32;

fn imported_project(
    vertex_count: usize,
    position_scale: f32,
    odd_texture: bool,
) -> daedalus_content::PreparedProject {
    imported_project_with_primitives(vertex_count, position_scale, odd_texture, 1)
}

fn imported_project_with_primitives(
    vertex_count: usize,
    position_scale: f32,
    odd_texture: bool,
    primitive_count: usize,
) -> daedalus_content::PreparedProject {
    imported_project_with_texture_dimensions(
        vertex_count,
        position_scale,
        odd_texture.then_some((5, 3)),
        primitive_count,
    )
}

fn imported_project_with_texture_dimensions(
    vertex_count: usize,
    position_scale: f32,
    texture_dimensions: Option<(u32, u32)>,
    primitive_count: usize,
) -> daedalus_content::PreparedProject {
    assert!(
        primitive_count > 0,
        "fixture model must contain a primitive"
    );
    let entity = json!({
        "id": "entity:stage",
        "components": [
            {
                "id": "component:transform:stage",
                "type_id": "daedalus.transform",
                "asset_refs": [],
                "data": {
                    "translation": [0.0, 0.0, 0.0],
                    "rotation": [0.0, 0.0, 0.0, 1.0],
                    "scale": [1.0, 1.0, 1.0]
                }
            },
            {
                "id": "component:model:stage",
                "type_id": "daedalus.model_renderer",
                "asset_refs": ["asset:model"],
                "data": {"model": "asset:model"}
            }
        ]
    });
    let scene = serde_json::to_vec(&json!({
        "schema_version": 1,
        "revision": 1,
        "entities": [entity],
        "assets": [{
            "id": "asset:model",
            "kind": "daedalus.model",
            "uri": "models/mesh.gltf",
            "metadata": {}
        }]
    }))
    .expect("stage fixture scene should encode");

    let mut binary = Vec::with_capacity(vertex_count * MODEL_VERTEX_SOURCE_BYTES + 12);
    let position_offset = binary.len();
    let positions = [[-0.9_f32, -0.8, 0.0], [0.9, -0.8, 0.0], [0.0, 0.9, 0.0]];
    for index in 0..vertex_count {
        let mut position = positions[index % positions.len()];
        position[0] *= position_scale;
        position[1] *= position_scale;
        binary.extend_from_slice(&position[0].to_le_bytes());
        binary.extend_from_slice(&position[1].to_le_bytes());
        binary.extend_from_slice(&position[2].to_le_bytes());
    }
    let position_bytes = binary.len() - position_offset;

    let normal_offset = binary.len();
    for _ in 0..vertex_count {
        binary.extend_from_slice(&0.0_f32.to_le_bytes());
        binary.extend_from_slice(&0.0_f32.to_le_bytes());
        binary.extend_from_slice(&1.0_f32.to_le_bytes());
    }
    let normal_bytes = binary.len() - normal_offset;

    let uv_offset = binary.len();
    for index in 0..vertex_count {
        let uv = match index % 3 {
            0 => [0.0_f32, 0.0],
            1 => [1.0_f32, 0.0],
            _ => [0.5_f32, 1.0],
        };
        binary.extend_from_slice(&uv[0].to_le_bytes());
        binary.extend_from_slice(&uv[1].to_le_bytes());
    }
    let uv_bytes = binary.len() - uv_offset;

    let index_offset = binary.len();
    for index in [0_u32, 1, 2] {
        binary.extend_from_slice(&index.to_le_bytes());
    }
    let index_bytes = binary.len() - index_offset;

    let texture = texture_dimensions.map(|(width, height)| {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .expect("fixture PNG header should encode");
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                for x in 0..width {
                    let x = u8::try_from(x % u32::from(u8::MAX))
                        .expect("fixture x coordinate should fit u8");
                    let y = u8::try_from(y % u32::from(u8::MAX))
                        .expect("fixture y coordinate should fit u8");
                    rgba.extend_from_slice(&[
                        24_u8.saturating_add(x.saturating_mul(37)),
                        20_u8.saturating_add(y.saturating_mul(61)),
                        210_u8.saturating_sub(x.saturating_mul(23)),
                        255,
                    ]);
                }
            }
            writer
                .write_image_data(&rgba)
                .expect("fixture PNG should encode");
        }
        encoded
    });

    let mut model = json!({
        "asset": {"version": "2.0"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": [{
            "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
            "indices": 3
        }]}],
        "buffers": [{"uri": "mesh.bin", "byteLength": binary.len()}],
        "bufferViews": [
            {"buffer": 0, "byteOffset": position_offset, "byteLength": position_bytes},
            {"buffer": 0, "byteOffset": normal_offset, "byteLength": normal_bytes},
            {"buffer": 0, "byteOffset": uv_offset, "byteLength": uv_bytes},
            {"buffer": 0, "byteOffset": index_offset, "byteLength": index_bytes}
        ],
        "accessors": [
            {"bufferView": 0, "componentType": 5126, "count": vertex_count, "type": "VEC3",
             "min": [-0.9 * position_scale, -0.8 * position_scale, 0.0],
             "max": [0.9 * position_scale, 0.9 * position_scale, 0.0]},
            {"bufferView": 1, "componentType": 5126, "count": vertex_count, "type": "VEC3"},
            {"bufferView": 2, "componentType": 5126, "count": vertex_count, "type": "VEC2"},
            {"bufferView": 3, "componentType": 5125, "count": 3, "type": "SCALAR"}
        ]
    });
    if texture_dimensions.is_some() {
        model["images"] = json!([{"uri": "image.png"}]);
        model["samplers"] = json!([{
            "magFilter": 9729,
            "minFilter": 9987,
            "wrapS": 10497,
            "wrapT": 33648
        }]);
        model["textures"] = json!([{"sampler": 0, "source": 0}]);
        model["materials"] = json!([{
            "pbrMetallicRoughness": {
                "baseColorFactor": [0.8, 0.9, 1.0, 1.0],
                "metallicFactor": 0.0,
                "roughnessFactor": 0.7,
                "baseColorTexture": {"index": 0}
            }
        }]);
        model["meshes"][0]["primitives"][0]["material"] = json!(0);
    }
    let primitive = model["meshes"][0]["primitives"][0].clone();
    model["meshes"][0]["primitives"] = json!(vec![primitive; primitive_count]);

    let manifest = br#"{"schema_version":1,"name":"staged GPU fixture","scene":"scene.json","presentation":{"camera":{"position":[0.0,0.0,8.0],"target":[0.0,0.0,0.0],"up":[0.0,1.0,0.0],"vertical_fov_radians":0.9,"near":0.1,"far":100.0},"ambient":[0.12,0.14,0.16],"clear":[0.01,0.02,0.03,1.0]}}"#;
    let mut sources = BTreeMap::from([
        (
            "models/mesh.gltf".to_owned(),
            serde_json::to_vec(&model).expect("glTF fixture should encode"),
        ),
        ("models/mesh.bin".to_owned(), binary),
    ]);
    if let Some(texture) = texture {
        sources.insert("models/image.png".to_owned(), texture);
    }
    let mut resolver = ByteMapSourceResolver::from_bytes(sources)
        .expect("stage fixture source map should be valid");
    let mut cache = MemoryCookCache::default();
    prepare_project_with_assets(
        manifest,
        &scene,
        &mut resolver,
        &mut cache,
        ImportLimits::default(),
        &ImportCancellation::never(),
    )
    .expect("stage fixture should import through the public content path")
}

fn stage_asset_limits(max_in_flight: usize) -> PacketAssetLimits {
    PacketAssetLimits {
        max_in_flight_submissions: max_in_flight,
        ..PacketAssetLimits::default()
    }
}

fn cpu_generation_with_limits(
    project: &daedalus_content::PreparedProject,
    generation: u64,
    session: SessionId,
    asset_limits: PacketAssetLimits,
) -> (
    PacketCpuOwner,
    crate::CpuGenerationId,
    PacketStageInput,
    Arc<CpuFrameHandle>,
) {
    let bindings = project
        .entities()
        .iter()
        .enumerate()
        .map(|(index, entity)| {
            (
                entity.id.clone(),
                EntityHandle {
                    session,
                    index: index as u32,
                    generation: 1,
                },
            )
        })
        .collect::<BTreeMap<StableId, EntityHandle>>();
    let mut owner = PacketCpuOwner::new(
        PacketCpuLimits::default(),
        PacketRendererLimits::default(),
        asset_limits,
    )
    .expect("CPU generation limits should be valid");
    let cancelled = AtomicBool::new(false);
    let id = owner
        .prepare_generation(
            project,
            generation,
            session,
            &bindings,
            SourcePin::new(Arc::new(())),
            &cancelled,
        )
        .expect("actual PreparedProject should prepare as a CPU generation");
    let input = owner
        .stage_input(id)
        .expect("generation should expose stage input");
    let snapshot = RenderSnapshot {
        sequence: generation,
        tick: Tick(generation),
        entities: project
            .entities()
            .iter()
            .map(|entity| RenderEntitySnapshot {
                entity: bindings[&entity.id],
                transform: Transform::IDENTITY,
            })
            .collect::<Vec<_>>()
            .into(),
    };
    let frame = owner
        .extract_validated(id, &snapshot, generation, None, &cancelled)
        .expect("actual snapshot should extract through CPU validation");
    (owner, id, input, frame)
}

fn frame_with_translation(
    owner: &mut PacketCpuOwner,
    project: &daedalus_content::PreparedProject,
    generation: crate::CpuGenerationId,
    session: SessionId,
    sequence: u64,
    translation_x: f32,
) -> Arc<CpuFrameHandle> {
    frame_with_translation_and_view(
        owner,
        project,
        generation,
        session,
        sequence,
        translation_x,
        None,
    )
}

fn frame_with_translation_and_view(
    owner: &mut PacketCpuOwner,
    project: &daedalus_content::PreparedProject,
    generation: crate::CpuGenerationId,
    session: SessionId,
    sequence: u64,
    translation_x: f32,
    view: Option<&RenderView>,
) -> Arc<CpuFrameHandle> {
    assert_eq!(project.entities().len(), 1, "stage fixture has one entity");
    let snapshot = RenderSnapshot {
        sequence,
        tick: Tick(sequence),
        entities: vec![RenderEntitySnapshot {
            entity: EntityHandle {
                session,
                index: 0,
                generation: 1,
            },
            transform: Transform {
                translation: [translation_x, 0.0, 0.0],
                ..Transform::IDENTITY
            },
        }]
        .into(),
    };
    owner
        .extract_validated(
            generation,
            &snapshot,
            sequence,
            view,
            &AtomicBool::new(false),
        )
        .expect("snapshot pose should extract from the existing generation")
}

fn packet_renderer(max_in_flight: usize) -> PacketRenderer {
    packet_renderer_with_asset_limits(stage_asset_limits(max_in_flight))
}

fn packet_renderer_with_asset_limits(asset_limits: PacketAssetLimits) -> PacketRenderer {
    let renderer = Renderer::new_offscreen_blocking()
        .expect("a real native wgpu adapter is required for this ignored test");
    eprintln!(
        "staged GPU test adapter: name={:?}, backend={:?}, device_type={:?}",
        renderer.info().adapter_name,
        renderer.info().backend,
        renderer.info().device_type
    );
    renderer
        .into_packet_renderer_with_asset_limits(PacketRendererLimits::default(), asset_limits)
        .expect("asset limits should support bounded staging")
}

fn target(
    renderer: &PacketRenderer,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = renderer.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("Daedalus staged GPU test target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

#[derive(Debug, Eq, PartialEq)]
struct ResourceMutationSnapshot {
    geometry: crate::PacketResourceStats,
    assets: crate::PacketAssetResourceStats,
    model_texture_layout_ready: bool,
    model_pipeline_ready: bool,
}

fn resource_mutation_snapshot(renderer: &PacketRenderer) -> ResourceMutationSnapshot {
    let geometry = renderer.resource_stats();
    let mut assets = renderer.asset_resource_stats();
    // Preparing a candidate chunk may update this high-water diagnostic before
    // submission admission. It is not a GPU/resource mutation.
    assets.cpu_upload_scratch_peak_bytes = 0;
    ResourceMutationSnapshot {
        geometry,
        assets,
        model_texture_layout_ready: renderer.renderer.model_textures_layout().is_some(),
        model_pipeline_ready: renderer.renderer.model_packet_mesh_pipeline().is_some(),
    }
}

fn fill_submission_ledger_with_future_entries(renderer: &mut PacketRenderer) -> u64 {
    assert!(
        renderer.staging_submissions.is_empty(),
        "test injection requires the real submission queue to be drained first"
    );
    assert_eq!(renderer.staging_in_flight_bytes, 0);
    let future_serial = renderer
        .submission_serial
        .checked_add(1)
        .expect("test ledger serial should remain available");
    assert!(
        future_serial
            > renderer
                .completion_watermark
                .load(std::sync::atomic::Ordering::Acquire),
        "injected entries must stay pending during this no-submit regression"
    );
    // This is an explicit test-only ledger saturation injection. No fake GPU
    // submission or completion is claimed; a future serial keeps these entries
    // pending so normal admission sees a full bounded ledger deterministically.
    for _ in 0..renderer.asset_limits.max_in_flight_submissions {
        renderer
            .staging_submissions
            .push_back(super::super::StagingSubmission {
                serial: future_serial,
                bytes: 0,
                _cpu_frame: None,
            });
    }
    future_serial
}

fn clear_injected_submission_ledger(renderer: &mut PacketRenderer) {
    renderer.staging_submissions.clear();
    renderer.staging_in_flight_bytes = 0;
    renderer.asset_stats.upload_staging_in_flight_bytes = 0;
}

fn empty_legacy_packet() -> Arc<RenderPacket> {
    Arc::new(RenderPacket::new(
        SessionId::from_raw(41_206),
        Tick(1),
        1,
        1,
        "0000000000000000000000000000000000000000000000000000000000000000",
        Camera::default(),
        DirectionalLight::default(),
        [0.12, 0.14, 0.16],
        [0.01, 0.02, 0.03, 1.0],
        Vec::new(),
    ))
}

fn drain_until_complete(renderer: &mut PacketRenderer) {
    let deadline = Instant::now() + STAGE_DEADLINE;
    for _ in 0..STAGE_PUMP_LIMIT {
        if renderer
            .try_drain_after_window()
            .expect("real GPU drain poll")
            == PacketRendererDrainStatus::Drained
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "real GPU submissions did not drain"
        );
        thread::yield_now();
    }
    panic!("real GPU drain exceeded bounded poll count");
}

fn wait_for_stage_submissions_to_complete(
    renderer: &mut PacketRenderer,
    stage: PacketStageId,
) -> crate::PacketStageStatus {
    let deadline = Instant::now() + STAGE_DEADLINE;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "real stage submissions did not complete while status was polled"
        );
        let status = renderer
            .try_stage_status(stage)
            .expect("real stage status should remain queryable while polling completion");
        if renderer.asset_resource_stats().in_flight_submissions == 0 {
            return status;
        }
        thread::yield_now();
    }
    panic!("stage completion polling exceeded bounded poll count");
}

fn pump_once(
    renderer: &mut PacketRenderer,
    target: &wgpu::TextureView,
    stage: PacketStageId,
    frame: Option<Arc<CpuFrameHandle>>,
) -> Option<crate::PacketStageStatus> {
    match renderer.render_cpu_frame_with_stage(
        target,
        64,
        64,
        frame,
        &RenderView::perspective(Camera::default()),
        Some(stage),
        None,
    ) {
        Ok(submission) => submission.progress().cloned(),
        Err(PacketStageError::Render(PacketRenderError::ResidencyBusy)) => None,
        Err(error) => panic!("real staged frame pump failed: {error}"),
    }
}

fn pump_once_with_loading_overlay(
    renderer: &mut PacketRenderer,
    target: &wgpu::TextureView,
    stage: PacketStageId,
    frame: Option<Arc<CpuFrameHandle>>,
) -> Option<crate::PacketStageStatus> {
    match renderer.render_cpu_frame_with_stage(
        target,
        64,
        64,
        frame,
        &RenderView::perspective(Camera::default()),
        Some(stage),
        Some(0.25),
    ) {
        Ok(submission) => submission.progress().cloned(),
        Err(PacketStageError::Render(PacketRenderError::ResidencyBusy)) => None,
        Err(error) => panic!("real staged frame with loading overlay failed: {error}"),
    }
}

fn poll_until(
    renderer: &mut PacketRenderer,
    target: &wgpu::TextureView,
    stage: PacketStageId,
    frame: Option<Arc<CpuFrameHandle>>,
    wanted: PacketStageState,
) -> crate::PacketStageStatus {
    let deadline = Instant::now() + STAGE_DEADLINE;
    let mut previous_upload = renderer
        .try_stage_status(stage)
        .expect("stage status should be available before polling")
        .uploaded_bytes();
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "GPU stage did not reach {wanted:?}"
        );
        if let Some(status) = pump_once(renderer, target, stage, frame.clone()) {
            assert!(
                status.uploaded_bytes().saturating_sub(previous_upload)
                    <= PacketStageLimits::default().upload_bytes_per_submission(),
                "one real submit exceeded the raw upload quota"
            );
            previous_upload = status.uploaded_bytes();
            if status.state() == wanted {
                return status;
            }
            assert!(
                matches!(
                    status.state(),
                    PacketStageState::Admitted
                        | PacketStageState::Uploading
                        | PacketStageState::AwaitingCompletion
                        | PacketStageState::Ready
                ),
                "stage entered unexpected terminal state: {:?}",
                status.state()
            );
        }
        let polled = renderer
            .try_stage_status(stage)
            .unwrap_or_else(|error| panic!("real stage status poll failed: {error}"));
        assert!(
            polled.uploaded_bytes().saturating_sub(previous_upload)
                <= PacketStageLimits::default().upload_bytes_per_submission(),
            "one real submit exceeded the raw upload quota"
        );
        previous_upload = polled.uploaded_bytes();
        if polled.state() == wanted {
            return polled;
        }
        thread::yield_now();
    }
    panic!("GPU stage exceeded bounded pump count while waiting for {wanted:?}");
}

fn activate_frame(
    renderer: &mut PacketRenderer,
    target: &wgpu::TextureView,
    stage: PacketStageId,
    frame: Arc<CpuFrameHandle>,
) {
    let result = submit_frame_with_retry(renderer, target, Some(frame), Some(stage));
    assert!(
        result.frame().is_some(),
        "candidate activation must draw a real frame"
    );
    assert_eq!(
        result
            .progress()
            .expect("stage progress is returned")
            .state(),
        PacketStageState::Ready
    );
}

fn submit_frame_with_retry(
    renderer: &mut PacketRenderer,
    target: &wgpu::TextureView,
    frame: Option<Arc<CpuFrameHandle>>,
    stage: Option<PacketStageId>,
) -> PacketStageFrameSubmission {
    let deadline = Instant::now() + STAGE_DEADLINE;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(Instant::now() < deadline, "packet frame did not submit");
        match renderer.render_cpu_frame_with_stage(
            target,
            64,
            64,
            frame.clone(),
            &RenderView::perspective(Camera::default()),
            stage,
            None,
        ) {
            Ok(result) => return result,
            Err(PacketStageError::Render(PacketRenderError::ResidencyBusy)) => {
                thread::yield_now();
            }
            Err(error) => panic!("packet frame should submit: {error}"),
        }
    }
    panic!("packet frame exceeded bounded submission pump count");
}

#[test]
#[ignore = "requires a native wgpu adapter; uploads imported large geometry and odd-width mip rows"]
fn staged_upload_chunks_large_geometry_and_accounts_odd_mip_row_padding() {
    let vertex_count = 150_000;
    let project = imported_project(vertex_count, 1.0, true);
    let session = SessionId::from_raw(41_201);
    let (_owner, _generation, input, frame) =
        cpu_generation_with_limits(&project, 1, session, stage_asset_limits(2));
    let mut renderer = packet_renderer(2);
    let (texture, view) = target(&renderer, 64, 64);
    let stage = renderer
        .try_begin_stage(input, PacketStageLimits::default())
        .expect("large imported model stage should be admitted");
    let ready = poll_until(&mut renderer, &view, stage, None, PacketStageState::Ready);

    let expected_geometry_bytes = vertex_count * MODEL_VERTEX_SOURCE_BYTES + 3 * 4;
    let expected_mip_bytes = (5 * 3 + 2 + 1) * 4;
    assert!(ready.total_upload_bytes() > 4 * 1024 * 1024);
    assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
    let asset_stats = renderer.asset_resource_stats();
    let geometry_stats = renderer.resource_stats();
    assert_eq!(asset_stats.texture_upload_bytes, expected_mip_bytes as u64);
    assert_eq!(
        geometry_stats.geometry_upload_bytes as usize,
        expected_geometry_bytes
    );
    assert_eq!(
        ready.total_upload_bytes(),
        expected_geometry_bytes + expected_mip_bytes
    );
    // Five-by-three, two-by-one and one-by-one mip rows each require a padded
    // 256-byte upload pitch. The logical byte counter remains the unpadded size.
    assert!(ready.staging_bytes_submitted() >= expected_geometry_bytes + 5 * 256);
    assert!(ready.staging_bytes_submitted() > ready.uploaded_bytes());
    assert!(asset_stats.upload_staging_write_count >= 2);

    activate_frame(&mut renderer, &view, stage, frame);
    let _ = texture;
}

#[test]
#[ignore = "requires a native wgpu adapter; streams large geometry under the lowered 1 MiB CPU scratch cap"]
fn staged_upload_streams_large_geometry_under_lowered_cpu_scratch() {
    let vertex_count = 150_000;
    let project = imported_project(vertex_count, 1.0, false);
    let session = SessionId::from_raw(41_209);
    let mut asset_limits = stage_asset_limits(2);
    asset_limits.cpu_upload_scratch_bytes = 1024 * 1024;
    let (_owner, _generation, input, frame) =
        cpu_generation_with_limits(&project, 1, session, asset_limits);
    let mut renderer = packet_renderer_with_asset_limits(asset_limits);
    let (_texture, view) = target(&renderer, 64, 64);
    let stage = renderer
        .try_begin_stage(input, PacketStageLimits::default())
        .expect("default 4 MiB stage quota should admit geometry under 1 MiB scratch");
    let ready = poll_until(&mut renderer, &view, stage, None, PacketStageState::Ready);
    assert!(ready.total_upload_bytes() > 4 * 1024 * 1024);
    assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
    let stats = renderer.asset_resource_stats();
    assert!(stats.cpu_upload_scratch_peak_bytes > 0);
    assert!(stats.cpu_upload_scratch_peak_bytes <= 1024 * 1024);
    activate_frame(&mut renderer, &view, stage, frame);
}

#[test]
#[ignore = "requires a native wgpu adapter; streams default-quota geometry under 1 MiB staging with overlay headroom"]
fn staged_upload_reduces_default_quota_to_fit_global_staging_and_loading_overlay() {
    let vertex_count = 50_000;
    let project = imported_project(vertex_count, 1.0, false);
    let session = SessionId::from_raw(41_207);
    let mut asset_limits = stage_asset_limits(1);
    asset_limits.upload_staging_bytes = 1024 * 1024;
    let (_owner, _generation, input, _frame) =
        cpu_generation_with_limits(&project, 1, session, asset_limits);
    let mut renderer = packet_renderer_with_asset_limits(asset_limits);
    let (_texture, view) = target(&renderer, 64, 64);
    // Keep the stage's raw upload quota at its 4 MiB default. The renderer
    // must reduce each geometry chunk to fit the separate 1 MiB global staging
    // cap after reserving the real 16-byte overlay write for that same submit.
    let stage = renderer
        .try_begin_stage(input, PacketStageLimits::default())
        .expect("large geometry should be admitted under the default stage quota");
    let mut previous_uploaded = 0;
    let mut previous_overlay_writes = renderer.asset_resource_stats().overlay_write_count;
    let deadline = Instant::now() + STAGE_DEADLINE;
    let mut saw_geometry_upload = false;
    let mut ready = None;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "large geometry did not complete under lowered global staging"
        );
        if let Some(status) = pump_once_with_loading_overlay(&mut renderer, &view, stage, None) {
            let uploaded_delta = status.uploaded_bytes().saturating_sub(previous_uploaded);
            assert!(
                uploaded_delta <= PacketStageLimits::default().upload_bytes_per_submission(),
                "one combined submit exceeded the unchanged default raw stage quota"
            );
            if uploaded_delta > 0 {
                saw_geometry_upload = true;
                previous_uploaded = status.uploaded_bytes();
            }
            let stats = renderer.asset_resource_stats();
            assert_eq!(
                stats.overlay_write_count,
                previous_overlay_writes + 1,
                "every successful combined clear/stage submit must write its requested overlay"
            );
            previous_overlay_writes = stats.overlay_write_count;
            assert_eq!(
                stats.overlay_write_bytes,
                stats.overlay_write_count * 16,
                "overlay accounting should remain one 16-byte write per combined submit"
            );
            assert!(
                stats.upload_staging_peak_bytes <= asset_limits.upload_staging_bytes,
                "actual in-flight staging bytes must respect the reduced global cap"
            );
            assert!(
                stats.upload_staging_in_flight_bytes <= asset_limits.upload_staging_bytes,
                "current combined-submit staging bytes must respect the reduced global cap"
            );
            if status.state() == PacketStageState::Ready {
                ready = Some(status);
                break;
            }
            assert!(
                matches!(
                    status.state(),
                    PacketStageState::Admitted
                        | PacketStageState::Uploading
                        | PacketStageState::AwaitingCompletion
                ),
                "stage entered an unexpected state: {:?}",
                status.state()
            );
        }
        thread::yield_now();
    }
    let ready = ready.expect("lowered staging must eventually finish the large geometry");
    assert!(saw_geometry_upload);
    assert!(ready.total_upload_bytes() > 1024 * 1024);
    assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
    let stats = renderer.asset_resource_stats();
    assert!(stats.overlay_write_count > 0);
    assert!(stats.upload_staging_peak_bytes <= asset_limits.upload_staging_bytes);
}

#[test]
#[ignore = "requires a native wgpu adapter; rejects a padded texture row that cannot fit global staging before GPU effects"]
fn staged_admission_rejects_indivisible_padded_texture_without_gpu_mutation() {
    // A 65-pixel RGBA row is 260 raw bytes but requires a 512-byte aligned
    // staging row. The 400-byte cap still accommodates one model instance's
    // initial global/instance/material writes (304 bytes), isolating the
    // indivisible row rather than making the frame itself impossible.
    let project = imported_project_with_texture_dimensions(3, 1.0, Some((65, 3)), 1);
    let session = SessionId::from_raw(41_208);
    let mut asset_limits = stage_asset_limits(1);
    asset_limits.upload_staging_bytes = 400;
    let (_owner, _generation, input, _frame) =
        cpu_generation_with_limits(&project, 1, session, asset_limits);
    let mut renderer = packet_renderer_with_asset_limits(asset_limits);
    let before = resource_mutation_snapshot(&renderer);

    let rejected = renderer.try_begin_stage(input, PacketStageLimits::default());
    assert!(matches!(
        rejected,
        Err(PacketStageError::UploadUnitTooLarge {
            bytes: 512,
            limit: 400
        })
    ));
    assert_eq!(
        resource_mutation_snapshot(&renderer),
        before,
        "an indivisible padded row must be rejected before resource creation, eviction, or writes"
    );
    assert!(!renderer.stage_state.blocks_ordinary_rendering());
    assert!(renderer.ready_packet.is_none());
}

#[test]
#[ignore = "requires a native wgpu adapter; defers a padded stage row behind dirty old-frame writes then resumes"]
fn staged_texture_row_defers_behind_dirty_frame_and_resumes_on_unchanged_frame() {
    let session = SessionId::from_raw(41_210);
    let mut asset_limits = stage_asset_limits(1);
    asset_limits.upload_staging_bytes = 400;
    let old_project = imported_project(3, 1.0, false);
    let candidate_project = imported_project_with_texture_dimensions(3, 1.0, Some((1, 1)), 1);
    let (mut old_owner, old_generation, old_input, old_frame) =
        cpu_generation_with_limits(&old_project, 1, session, asset_limits);
    let (_candidate_owner, _candidate_generation, candidate_input, candidate_frame) =
        cpu_generation_with_limits(&candidate_project, 2, session, asset_limits);
    let mut renderer = packet_renderer_with_asset_limits(asset_limits);
    let (_texture, target) = target(&renderer, 64, 64);

    let old_stage = renderer
        .try_begin_stage(old_input, PacketStageLimits::default())
        .expect("old untextured generation should fit the 400-byte staging cap");
    poll_until(
        &mut renderer,
        &target,
        old_stage,
        None,
        PacketStageState::Ready,
    );
    activate_frame(&mut renderer, &target, old_stage, Arc::clone(&old_frame));
    let geometry_uploads_before_candidate = renderer.resource_stats().geometry_upload_count;

    // The candidate has the exact same canonical geometry, but introduces a
    // real 1x1 PNG. Its 4 raw bytes require one indivisible 256-byte staging
    // row, which fits the global cap when the active frame is clean.
    let candidate_stage = renderer
        .try_begin_stage(candidate_input, PacketStageLimits::default())
        .expect("candidate texture row should fit the clean 400-byte cap");
    let before_create = renderer
        .try_stage_status(candidate_stage)
        .expect("candidate status should be visible after stage admission");
    assert_eq!(before_create.total_upload_bytes(), 4);
    assert_eq!(before_create.uploaded_bytes(), 0);
    assert_eq!(
        renderer.resource_stats().geometry_upload_count,
        geometry_uploads_before_candidate,
        "candidate must reuse the old generation's canonical geometry"
    );

    let created = submit_frame_with_retry(
        &mut renderer,
        &target,
        Some(Arc::clone(&old_frame)),
        Some(candidate_stage),
    );
    assert_eq!(
        created
            .frame()
            .expect("old generation should draw while candidate texture resources are created")
            .packet()
            .project_generation(),
        1
    );
    let texture_cursor = wait_for_stage_submissions_to_complete(&mut renderer, candidate_stage);
    assert_eq!(texture_cursor.uploaded_bytes(), 0);
    assert!(texture_cursor.create_calls_completed() > 0);
    assert_ne!(texture_cursor.state(), PacketStageState::Ready);
    assert_eq!(
        renderer.resource_stats().geometry_upload_count,
        geometry_uploads_before_candidate
    );

    let changed_view = RenderView::perspective(Camera {
        position: [0.0, 0.0, 8.0],
        target: [0.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
        ..Camera::default()
    });
    let dirty_old_frame = frame_with_translation_and_view(
        &mut old_owner,
        &old_project,
        old_generation,
        session,
        2,
        0.35,
        Some(&changed_view),
    );
    let before_dirty_stats = renderer.resource_stats();
    let before_dirty_assets = renderer.asset_resource_stats();
    let dirty_submission = renderer
        .render_cpu_frame_with_stage(
            &target,
            64,
            64,
            Some(Arc::clone(&dirty_old_frame)),
            &changed_view,
            Some(candidate_stage),
            None,
        )
        .expect("dirty old-generation frame should draw while texture upload is deferred");
    let dirty_progress = dirty_submission
        .progress()
        .expect("candidate stage progress accompanies the old-generation draw");
    let dirty_draw = dirty_submission
        .frame()
        .expect("dirty old-generation pose should be submitted");
    assert_eq!(dirty_draw.packet().project_generation(), 1);
    assert_eq!(dirty_draw.packet().sequence(), 2);
    assert_eq!(
        dirty_progress.uploaded_bytes(),
        texture_cursor.uploaded_bytes()
    );
    assert_eq!(
        dirty_progress.staging_bytes_submitted(),
        texture_cursor.staging_bytes_submitted(),
        "a 256-byte candidate row must not advance beside 256 bytes of dirty frame writes"
    );
    assert_eq!(
        dirty_progress.create_calls_completed(),
        texture_cursor.create_calls_completed()
    );
    let after_dirty_stats = renderer.resource_stats();
    assert_eq!(
        after_dirty_stats.instance_write_count - before_dirty_stats.instance_write_count,
        1,
        "the translation change must write one old-generation instance"
    );
    assert_eq!(
        after_dirty_stats.instance_write_bytes - before_dirty_stats.instance_write_bytes,
        128
    );
    assert_eq!(
        after_dirty_stats.global_write_count - before_dirty_stats.global_write_count,
        1,
        "the camera change must write old-generation globals"
    );
    assert_eq!(
        after_dirty_stats.global_write_bytes - before_dirty_stats.global_write_bytes,
        128
    );
    let after_dirty_assets = renderer.asset_resource_stats();
    assert_eq!(
        after_dirty_assets.upload_staging_write_count,
        before_dirty_assets.upload_staging_write_count + 2,
        "the deferred submit writes the dirty instance and globals, but no candidate texture row"
    );
    assert_eq!(
        after_dirty_assets.upload_staging_write_bytes,
        before_dirty_assets.upload_staging_write_bytes + 256,
        "the deferred submit writes 128 bytes each for the instance and camera globals"
    );
    assert_eq!(
        after_dirty_assets.texture_upload_count, before_dirty_assets.texture_upload_count,
        "the candidate texture row stays deferred"
    );
    assert_eq!(
        after_dirty_assets.texture_upload_bytes, before_dirty_assets.texture_upload_bytes,
        "the candidate texture row stays deferred"
    );
    assert!(after_dirty_assets.upload_staging_peak_bytes <= 400);

    let after_dirty_completion =
        wait_for_stage_submissions_to_complete(&mut renderer, candidate_stage);
    assert_eq!(after_dirty_completion.uploaded_bytes(), 0);
    assert_eq!(
        after_dirty_completion.staging_bytes_submitted(),
        texture_cursor.staging_bytes_submitted()
    );
    // The same frame and view are now clean. After the dirty-frame submission
    // actually completes, its 256-byte row can share a submit with the old
    // draw and the candidate may proceed to Ready.
    let clean_submission = renderer
        .render_cpu_frame_with_stage(
            &target,
            64,
            64,
            Some(Arc::clone(&dirty_old_frame)),
            &changed_view,
            Some(candidate_stage),
            None,
        )
        .expect("unchanged old frame should leave the full row staging headroom");
    assert_eq!(
        clean_submission
            .frame()
            .expect("old generation remains drawable during final candidate upload")
            .packet()
            .sequence(),
        2
    );
    let clean_progress = clean_submission
        .progress()
        .expect("candidate progress accompanies the unchanged old draw");
    assert!(clean_progress.uploaded_bytes() > after_dirty_completion.uploaded_bytes());
    assert_eq!(
        clean_progress.uploaded_bytes(),
        clean_progress.total_upload_bytes()
    );
    let ready = wait_for_stage_submissions_to_complete(&mut renderer, candidate_stage);
    assert_eq!(ready.state(), PacketStageState::Ready);
    assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
    assert_eq!(
        renderer.resource_stats().geometry_upload_count,
        geometry_uploads_before_candidate
    );
    activate_frame(
        &mut renderer,
        &target,
        candidate_stage,
        Arc::clone(&candidate_frame),
    );
    assert_eq!(
        renderer
            .ready_packet
            .as_ref()
            .expect("candidate becomes ready only after its upload completes")
            .project_generation(),
        2
    );
}

#[test]
#[ignore = "requires a native wgpu adapter; injects a full pending ledger and checks stage admission has no GPU/resource effects"]
fn full_submission_ledger_blocks_stage_without_mutation_for_bootstrap_and_old_ready() {
    let project = imported_project(6_000, 0.82, false);
    let session = SessionId::from_raw(41_206);
    let asset_limits = stage_asset_limits(1);

    // Bootstrap clear-only path: actual imported resources are admitted, but a
    // deterministic future-serial ledger entry prevents the first create batch.
    {
        let (_owner, _generation, input, _candidate_frame) =
            cpu_generation_with_limits(&project, 2, session, asset_limits);
        let mut renderer = packet_renderer(1);
        let (_texture, target) = target(&renderer, 64, 64);
        let stage = renderer
            .try_begin_stage(input, PacketStageLimits::default())
            .expect("actual imported candidate should be admitted before the ledger is filled");
        let status_before = renderer
            .try_stage_status(stage)
            .expect("admitted stage should expose progress");
        assert_eq!(status_before.state(), PacketStageState::Admitted);
        assert_eq!(status_before.create_calls_completed(), 0);
        assert_eq!(status_before.uploaded_bytes(), 0);
        assert!(status_before.total_upload_bytes() > 0);
        assert!(renderer.ready_packet.is_none());

        let future_serial = fill_submission_ledger_with_future_entries(&mut renderer);
        let resources_before = resource_mutation_snapshot(&renderer);
        let ledger_before = renderer
            .staging_submissions
            .iter()
            .map(|entry| (entry.serial, entry.bytes))
            .collect::<Vec<_>>();
        assert_eq!(ledger_before.len(), 1);
        assert!(
            ledger_before[0].0
                > renderer
                    .completion_watermark
                    .load(std::sync::atomic::Ordering::Acquire)
        );
        let rejected = renderer.render_cpu_frame_with_stage(
            &target,
            64,
            64,
            None,
            &RenderView::perspective(Camera::default()),
            Some(stage),
            None,
        );
        assert!(matches!(
            rejected,
            Err(PacketStageError::Render(PacketRenderError::ResidencyBusy))
        ));
        assert_eq!(
            renderer
                .try_stage_status(stage)
                .expect("stage remains queryable after admission refusal"),
            status_before,
            "create tasks, upload cursor and initialization progress must remain unchanged"
        );
        assert!(renderer.ready_packet.is_none());
        assert_eq!(resource_mutation_snapshot(&renderer), resources_before);
        assert_eq!(
            renderer
                .staging_submissions
                .iter()
                .map(|entry| (entry.serial, entry.bytes))
                .collect::<Vec<_>>(),
            ledger_before,
            "the rejected operation must not submit or consume the injected ledger entry"
        );
        assert_eq!(future_serial, ledger_before[0].0);

        clear_injected_submission_ledger(&mut renderer);
        let ready = poll_until(&mut renderer, &target, stage, None, PacketStageState::Ready);
        assert!(ready.create_calls_completed() > 0);
        assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
    }

    // Old-ready draw path: a real completed legacy packet remains current while
    // the same bounded injection prevents the candidate's first create batch.
    {
        let (_owner, _generation, input, _candidate_frame) =
            cpu_generation_with_limits(&project, 2, session, asset_limits);
        let mut renderer = packet_renderer(1);
        let (_texture, target) = target(&renderer, 64, 64);
        let old_ready = empty_legacy_packet();
        renderer
            .render_packet_view(&target, 64, 64, Arc::clone(&old_ready), None, None)
            .expect("empty legacy packet should become a real ready draw");
        drain_until_complete(&mut renderer);
        assert!(Arc::ptr_eq(
            renderer
                .ready_packet
                .as_ref()
                .expect("legacy packet is ready"),
            &old_ready
        ));

        let stage = renderer
            .try_begin_stage(input, PacketStageLimits::default())
            .expect("candidate should stage alongside the completed old packet");
        let status_before = renderer
            .try_stage_status(stage)
            .expect("admitted stage should expose progress");
        assert_eq!(status_before.state(), PacketStageState::Admitted);
        assert_eq!(status_before.create_calls_completed(), 0);
        assert_eq!(status_before.uploaded_bytes(), 0);

        let future_serial = fill_submission_ledger_with_future_entries(&mut renderer);
        let resources_before = resource_mutation_snapshot(&renderer);
        let ledger_before = renderer
            .staging_submissions
            .iter()
            .map(|entry| (entry.serial, entry.bytes))
            .collect::<Vec<_>>();
        assert_eq!(ledger_before.len(), 1);
        assert!(
            ledger_before[0].0
                > renderer
                    .completion_watermark
                    .load(std::sync::atomic::Ordering::Acquire)
        );
        let rejected = renderer.render_cpu_frame_with_stage(
            &target,
            64,
            64,
            None,
            &RenderView::perspective(Camera::default()),
            Some(stage),
            None,
        );
        assert!(matches!(
            rejected,
            Err(PacketStageError::Render(PacketRenderError::ResidencyBusy))
        ));
        assert_eq!(
            renderer
                .try_stage_status(stage)
                .expect("stage remains queryable after admission refusal"),
            status_before,
            "candidate work must remain queued until the old-ready draw can submit"
        );
        assert!(Arc::ptr_eq(
            renderer
                .ready_packet
                .as_ref()
                .expect("old ready remains installed"),
            &old_ready
        ));
        assert_eq!(resource_mutation_snapshot(&renderer), resources_before);
        assert_eq!(
            renderer
                .staging_submissions
                .iter()
                .map(|entry| (entry.serial, entry.bytes))
                .collect::<Vec<_>>(),
            ledger_before
        );
        assert_eq!(future_serial, ledger_before[0].0);

        clear_injected_submission_ledger(&mut renderer);
        let ready = poll_until(&mut renderer, &target, stage, None, PacketStageState::Ready);
        assert!(ready.create_calls_completed() > 0);
        assert_eq!(ready.uploaded_bytes(), ready.total_upload_bytes());
        assert!(Arc::ptr_eq(
            renderer
                .ready_packet
                .as_ref()
                .expect("old packet remains ready until activation"),
            &old_ready
        ));
    }
}

#[test]
#[ignore = "requires a native wgpu adapter; checks real atomic create quotas and max-in-flight progress"]
fn staged_create_quota_preflight_and_cache_hit_initialization_progress() {
    let old_project = imported_project(3, 1.0, true);
    let moved_project = imported_project(6_000, 0.82, true);
    let session = SessionId::from_raw(41_202);

    for (max_in_flight, quota) in [(1, 1), (1, 2), (1, 3), (2, 1), (2, 2), (2, 3)] {
        let asset_limits = stage_asset_limits(max_in_flight);
        let (_old_owner, _old_id, old_input, old_frame) =
            cpu_generation_with_limits(&old_project, 1, session, asset_limits);
        let (candidate_owner, candidate_generation, _candidate_input, candidate_frame) =
            cpu_generation_with_limits(&moved_project, 2, session, asset_limits);
        let mut renderer = packet_renderer(max_in_flight);
        let (_texture, target) = target(&renderer, 64, 64);
        let old_stage = renderer
            .try_begin_stage(old_input, PacketStageLimits::default())
            .expect("old managed generation should stage before the candidate");
        poll_until(
            &mut renderer,
            &target,
            old_stage,
            None,
            PacketStageState::Ready,
        );
        activate_frame(&mut renderer, &target, old_stage, Arc::clone(&old_frame));

        // Stage admission may poll real submission retirement. Drain the old
        // generation's activation submit before taking the no-mutation
        // baseline, so the comparison covers admission effects rather than a
        // legitimate completion decrement.
        wait_for_stage_submissions_to_complete(&mut renderer, old_stage);

        let before_reject = resource_mutation_snapshot(&renderer);
        let input = candidate_owner
            .stage_input(candidate_generation)
            .expect("candidate CPU generation remains available");
        let limits = PacketStageLimits::default().with_max_resource_creates_per_submission(quota);
        let admitted = renderer.try_begin_stage(input, limits);
        if quota == 1 {
            assert!(matches!(admitted, Err(PacketStageError::InvalidLimits)));
            let after_reject = resource_mutation_snapshot(&renderer);
            assert_eq!(
                after_reject, before_reject,
                "atomic preflight must be effect-free"
            );
            continue;
        }
        let stage = admitted.expect("a geometry's two-create atomic group fits quota 2 or 3");
        let ready = poll_until(
            &mut renderer,
            &target,
            stage,
            Some(Arc::clone(&old_frame)),
            PacketStageState::Ready,
        );
        assert!(ready.create_calls_completed() >= 2);
        assert_eq!(ready.create_calls_completed() % 2, 0);
        assert!(ready.uploaded_bytes() > 0);
        assert!(renderer.asset_resource_stats().in_flight_submissions <= max_in_flight);
        activate_frame(&mut renderer, &target, stage, Arc::clone(&candidate_frame));
    }

    // A new CPU token for unchanged resource keys is a real cache-hit stage.
    // Its initialization serial follows the existing resource initialization,
    // while old-ready draws are allowed to advance last-use independently.
    let same_project = imported_project(3, 1.0, true);
    for max_in_flight in [1, 2] {
        let asset_limits = stage_asset_limits(max_in_flight);
        let (_cache_old_owner, _cache_old_generation, cache_old_input, cache_old_frame) =
            cpu_generation_with_limits(&old_project, 1, session, asset_limits);
        let (same_owner, _same_generation, same_input, same_frame) =
            cpu_generation_with_limits(&same_project, 2, session, asset_limits);
        let mut renderer = packet_renderer(max_in_flight);
        let (_texture, target) = target(&renderer, 64, 64);
        let old_stage = renderer
            .try_begin_stage(cache_old_input, PacketStageLimits::default())
            .expect("old managed generation should stage before the cache hit");
        poll_until(
            &mut renderer,
            &target,
            old_stage,
            None,
            PacketStageState::Ready,
        );
        activate_frame(
            &mut renderer,
            &target,
            old_stage,
            Arc::clone(&cache_old_frame),
        );
        let stage = renderer
            .try_begin_stage(
                same_input,
                PacketStageLimits::default().with_max_resource_creates_per_submission(1),
            )
            .expect("same resource set should stage as a cache hit");
        let status = poll_until(
            &mut renderer,
            &target,
            stage,
            Some(Arc::clone(&cache_old_frame)),
            PacketStageState::Ready,
        );
        assert_eq!(status.total_upload_bytes(), 0);
        assert_eq!(status.uploaded_bytes(), 0);
        assert_eq!(status.create_calls_completed(), 0);
        let init = status
            .initialization_serial()
            .expect("cached resources have init serial");
        let last_use = renderer
            .geometry
            .values()
            .map(|resource| resource.last_submission)
            .max()
            .expect("old model geometry should be resident");
        assert!(
            last_use > init,
            "old-ready draw should advance last-use after init"
        );
        assert_eq!(
            renderer
                .try_stage_status(stage)
                .unwrap()
                .initialization_serial(),
            Some(init),
            "old draw last-use must not become candidate initialization serial"
        );
        activate_frame(&mut renderer, &target, stage, same_frame);
        drop(same_owner);
    }
}

#[test]
#[ignore = "requires a native wgpu adapter; checks the cold four-call model pipeline group"]
fn staged_cold_model_pipeline_group_is_atomic_at_four_creates() {
    let project = imported_project(3, 1.0, true);
    let session = SessionId::from_raw(41_205);
    let (owner, generation, _input, frame) =
        cpu_generation_with_limits(&project, 1, session, stage_asset_limits(2));

    for quota in [1, 2, 3] {
        let mut renderer = packet_renderer(2);
        let before = resource_mutation_snapshot(&renderer);
        assert!(!before.model_texture_layout_ready);
        assert!(!before.model_pipeline_ready);
        let input = owner
            .stage_input(generation)
            .expect("cold CPU generation remains available");
        let result = renderer.try_begin_stage(
            input,
            PacketStageLimits::default().with_max_resource_creates_per_submission(quota),
        );
        assert!(matches!(result, Err(PacketStageError::InvalidLimits)));
        assert_eq!(
            resource_mutation_snapshot(&renderer),
            before,
            "an undersized cold pipeline group must be rejected without GPU mutations"
        );
    }

    let mut renderer = packet_renderer(2);
    let (_texture, target) = target(&renderer, 64, 64);
    let input = owner
        .stage_input(generation)
        .expect("cold CPU generation remains available");
    let stage = renderer
        .try_begin_stage(
            input,
            PacketStageLimits::default().with_max_resource_creates_per_submission(4),
        )
        .expect("the complete cold model-pipeline group fits quota four");
    let ready = poll_until(&mut renderer, &target, stage, None, PacketStageState::Ready);
    assert!(ready.create_calls_completed() >= 4);
    activate_frame(&mut renderer, &target, stage, frame);
}

#[test]
#[ignore = "requires a native wgpu adapter; reads old-ready pixels while a different resource generation stages"]
fn staged_candidate_preserves_old_ready_and_rejects_foreign_resource_token() {
    let old_project = imported_project(3, 1.0, true);
    let candidate_project = imported_project_with_primitives(180_000, 0.62, true, 2);
    let session = SessionId::from_raw(41_203);
    let asset_limits = stage_asset_limits(2);
    let (mut old_owner, old_generation, _old_input, old_frame) =
        cpu_generation_with_limits(&old_project, 1, session, asset_limits);
    let (candidate_owner, _candidate_generation, candidate_input, candidate_frame) =
        cpu_generation_with_limits(&candidate_project, 2, session, asset_limits);
    assert_eq!(old_frame.packet().instances().len(), 1);
    assert_eq!(candidate_frame.packet().instances().len(), 2);
    let candidate_draw_ids = candidate_frame
        .packet()
        .instances()
        .iter()
        .map(|instance| instance.id.primitive)
        .collect::<Vec<_>>();
    assert_eq!(candidate_draw_ids.as_slice(), &[0, 1]);
    let (foreign_owner, _foreign_generation, _foreign_input, foreign_frame) =
        cpu_generation_with_limits(&candidate_project, 2, session, asset_limits);
    let mut renderer = packet_renderer(2);
    let (texture, target) = target(&renderer, 64, 64);

    let initial = renderer
        .try_begin_stage(
            old_owner.stage_input(old_generation).unwrap(),
            PacketStageLimits::default(),
        )
        .unwrap();
    poll_until(
        &mut renderer,
        &target,
        initial,
        None,
        PacketStageState::Ready,
    );
    activate_frame(&mut renderer, &target, initial, Arc::clone(&old_frame));
    let initial_path = capture_path("staged-initial-pose-before-candidate");
    let _ = std::fs::remove_file(&initial_path);
    capture_target(
        &renderer.renderer,
        &texture,
        &CaptureOptions::new(64, 64, &initial_path),
        "initial old-generation snapshot pose",
    );
    let initial_pixels = decoded_rgba(&initial_path);
    std::fs::remove_file(&initial_path).ok();

    let moved_frame = frame_with_translation(
        &mut old_owner,
        &old_project,
        old_generation,
        session,
        2,
        0.9,
    );
    let stage = renderer
        .try_begin_stage(
            candidate_input,
            PacketStageLimits::default().with_upload_bytes_per_submission(1024 * 1024),
        )
        .expect("candidate with new geometry should be admitted beside old-ready");

    // This newer old-generation pose is first presented through the combined
    // stage pump after candidate admission. It catches a regression where
    // stage admission freezes the cached old-ready packet identity.
    let deadline = Instant::now() + STAGE_DEADLINE;
    let mut moved_during_partial_upload = None;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "candidate did not reach a partial upload while presenting the moved old pose"
        );
        let submission = submit_frame_with_retry(
            &mut renderer,
            &target,
            Some(Arc::clone(&moved_frame)),
            Some(stage),
        );
        let progress = submission
            .progress()
            .expect("combined old-pose submission returns stage progress");
        assert_ne!(progress.state(), PacketStageState::Ready);
        assert_eq!(
            submission
                .frame()
                .expect("old generation remains drawable during candidate staging")
                .packet()
                .sequence(),
            moved_frame.packet().sequence()
        );
        if progress.uploaded_bytes() > 0
            && progress.uploaded_bytes() < progress.total_upload_bytes()
        {
            moved_during_partial_upload = Some(submission);
            break;
        }
        thread::yield_now();
    }
    let moved_during_stage = moved_during_partial_upload
        .expect("candidate should upload in chunks while presenting the moved old pose");
    let moved_stage_progress = moved_during_stage
        .progress()
        .expect("partial old-pose submission retains stage progress");
    assert!(moved_stage_progress.uploaded_bytes() > 0);
    assert!(moved_stage_progress.uploaded_bytes() < moved_stage_progress.total_upload_bytes());

    let old_path = capture_path("staged-old-ready-before");
    let _ = std::fs::remove_file(&old_path);
    capture_target(
        &renderer.renderer,
        &texture,
        &CaptureOptions::new(64, 64, &old_path),
        "latest old-generation snapshot pose during candidate upload",
    );
    let old_pixels = decoded_rgba(&old_path);
    assert_ne!(
        initial_pixels, old_pixels,
        "the old-ready probe must represent an observable newer snapshot pose"
    );

    // Supplying the candidate frame before readiness must still draw pose 2
    // from the active generation, never candidate resources.
    let candidate_frame_before_ready = submit_frame_with_retry(
        &mut renderer,
        &target,
        Some(Arc::clone(&candidate_frame)),
        Some(stage),
    );
    let candidate_frame_progress = candidate_frame_before_ready
        .progress()
        .expect("candidate submission returns stage progress");
    assert_ne!(
        candidate_frame_progress.state(),
        PacketStageState::Ready,
        "the large candidate must still be incomplete before its first draw"
    );
    assert!(
        candidate_frame_progress.uploaded_bytes() < candidate_frame_progress.total_upload_bytes(),
        "candidate frame attempt must not complete all resource uploads"
    );
    let still_presented_packet = candidate_frame_before_ready
        .frame()
        .expect("the prior generation remains drawable during candidate staging")
        .packet();
    assert_eq!(still_presented_packet.project_generation(), 1);
    assert_eq!(
        still_presented_packet.sequence(),
        moved_frame.packet().sequence(),
        "staging must keep presenting the latest old snapshot pose"
    );

    let deadline = Instant::now() + STAGE_DEADLINE;
    let mut partial = None;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "candidate did not submit a partial upload"
        );
        if let Some(status) = pump_once(
            &mut renderer,
            &target,
            stage,
            Some(Arc::clone(&moved_frame)),
        ) && status.uploaded_bytes() > 0
            && status.uploaded_bytes() < status.total_upload_bytes()
        {
            partial = Some(status);
            break;
        }
        thread::yield_now();
    }
    let partial = partial.expect("large candidate must expose a partial-upload interval");
    assert!(partial.uploaded_bytes() > 0);
    assert!(partial.uploaded_bytes() < partial.total_upload_bytes());

    let old_during_path = capture_path("staged-old-ready-during-candidate");
    let _ = std::fs::remove_file(&old_during_path);
    capture_target(
        &renderer.renderer,
        &texture,
        &CaptureOptions::new(64, 64, &old_during_path),
        "latest old-ready pose while candidate upload is partial",
    );
    assert_eq!(
        decoded_rgba(&old_during_path),
        old_pixels,
        "partial candidate upload must keep drawing the latest old-ready pose"
    );
    let ready = poll_until(
        &mut renderer,
        &target,
        stage,
        Some(Arc::clone(&moved_frame)),
        PacketStageState::Ready,
    );
    assert!(ready.uploaded_bytes() > 4 * 1024 * 1024);

    let before_foreign = resource_mutation_snapshot(&renderer);
    let rejected = renderer.render_cpu_frame_with_stage(
        &target,
        64,
        64,
        Some(foreign_frame),
        &RenderView::perspective(Camera::default()),
        Some(stage),
        None,
    );
    assert!(matches!(
        rejected,
        Err(PacketStageError::FrameResourcesUnavailable)
            | Err(PacketStageError::FrameIdentityMismatch)
    ));
    assert_eq!(
        resource_mutation_snapshot(&renderer),
        before_foreign,
        "foreign canonical token must fail before writes or cache mutation"
    );
    assert_eq!(
        renderer.try_stage_status(stage).unwrap().state(),
        PacketStageState::Ready
    );
    let old_after_rejection_path = capture_path("staged-old-ready-after-foreign-rejection");
    let _ = std::fs::remove_file(&old_after_rejection_path);
    capture_target(
        &renderer.renderer,
        &texture,
        &CaptureOptions::new(64, 64, &old_after_rejection_path),
        "old-ready after foreign token rejection",
    );
    assert_eq!(decoded_rgba(&old_after_rejection_path), old_pixels);

    let before_candidate_activation = renderer.resource_stats();
    activate_frame(&mut renderer, &target, stage, Arc::clone(&candidate_frame));
    let after_candidate_activation = renderer.resource_stats();
    assert!(
        after_candidate_activation
            .instance_write_count
            .saturating_sub(before_candidate_activation.instance_write_count)
            >= 2,
        "the expanded candidate generation must write slots for both draw IDs"
    );
    let candidate_path = capture_path("staged-candidate-after-ready");
    let _ = std::fs::remove_file(&candidate_path);
    capture_target(
        &renderer.renderer,
        &texture,
        &CaptureOptions::new(64, 64, &candidate_path),
        "staged candidate after full initialization",
    );
    assert_ne!(old_pixels, decoded_rgba(&candidate_path));
    let _ = std::fs::remove_file(old_path);
    let _ = std::fs::remove_file(old_during_path);
    let _ = std::fs::remove_file(old_after_rejection_path);
    let _ = std::fs::remove_file(candidate_path);
    drop(candidate_owner);
    drop(foreign_owner);
}

#[test]
#[ignore = "requires a native wgpu adapter; verifies partial cancellation, real drain and CPU frame retention"]
fn cancelled_stage_drains_submitted_chunk_without_promoting_partial_resource() {
    let old_project = imported_project(3, 1.0, true);
    let project = imported_project(180_000, 0.75, true);
    let session = SessionId::from_raw(41_204);
    let asset_limits = stage_asset_limits(1);
    let (old_owner, old_generation, _old_input, old_frame) =
        cpu_generation_with_limits(&old_project, 1, session, asset_limits);
    let (owner, _generation, input, candidate_frame) =
        cpu_generation_with_limits(&project, 2, session, asset_limits);
    assert!(Arc::ptr_eq(
        input.resource_set_token(),
        candidate_frame.resource_set_token()
    ));
    let mut renderer = packet_renderer(1);
    let (_texture, target) = target(&renderer, 64, 64);

    let initial = renderer
        .try_begin_stage(
            old_owner.stage_input(old_generation).unwrap(),
            PacketStageLimits::default(),
        )
        .expect("old generation should stage before cancellation coverage");
    poll_until(
        &mut renderer,
        &target,
        initial,
        None,
        PacketStageState::Ready,
    );
    activate_frame(&mut renderer, &target, initial, Arc::clone(&old_frame));
    drain_until_complete(&mut renderer);
    let frame_alias_baseline = Arc::strong_count(&old_frame);
    let before = renderer.resource_stats();
    let stage = renderer
        .try_begin_stage(input, PacketStageLimits::default())
        .expect("partial candidate should be admitted");
    let deadline = Instant::now() + STAGE_DEADLINE;
    let mut upload_status = None;
    for _ in 0..STAGE_PUMP_LIMIT {
        assert!(
            Instant::now() < deadline,
            "stage did not submit its first upload chunk"
        );
        if let Some(status) = pump_once(&mut renderer, &target, stage, Some(Arc::clone(&old_frame)))
            && status.uploaded_bytes() > 0
        {
            upload_status = Some(status);
            break;
        }
        thread::yield_now();
    }
    let submitted = upload_status.expect("large geometry must require a submitted upload range");
    assert!(submitted.uploaded_bytes() < submitted.total_upload_bytes());
    assert!(
        Arc::strong_count(&old_frame) > frame_alias_baseline,
        "the exact old CpuFrameHandle passed to the combined draw must stay alive through GPU use"
    );
    assert!(
        renderer.asset_resource_stats().in_flight_submissions > 0,
        "cancellation must start with a real submitted GPU operation still in the ledger"
    );

    let cancelled = renderer
        .try_cancel_stage(stage)
        .expect("known stage cancels");
    assert!(matches!(
        cancelled.state(),
        PacketStageState::Cancelling | PacketStageState::Retiring | PacketStageState::Cancelled
    ));
    let after_cancel = renderer.resource_stats();
    assert_eq!(after_cancel.active_entries, before.active_entries);
    assert_eq!(
        after_cancel.geometry_upload_count,
        before.geometry_upload_count
    );

    drain_until_complete(&mut renderer);
    assert_eq!(
        renderer.resource_stats().active_entries,
        before.active_entries
    );
    assert_eq!(
        renderer.resource_stats().geometry_upload_count,
        before.geometry_upload_count
    );
    assert_eq!(Arc::strong_count(&old_frame), frame_alias_baseline);
    assert!(owner.status().generation_slots_used > 0);
}
