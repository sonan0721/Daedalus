#![cfg(feature = "test-fixtures")]

use std::{collections::HashSet, fs::File, io::BufReader, path::PathBuf};

use daedalus_renderer::{CaptureOptions, Renderer, test_fixtures::metal_roughness_scene};

/// Run explicitly on a machine with a working GPU adapter. It writes a real
/// renderer artifact to the system temp directory and verifies decoded pixels.
#[test]
#[ignore = "requires a working wgpu GPU adapter"]
fn renders_metal_roughness_fixture_to_png() {
    let renderer = Renderer::new_offscreen_blocking().expect("a real wgpu adapter is required");
    let scene = metal_roughness_scene();
    let path: PathBuf = std::env::var_os("DAEDALUS_RENDERER_CAPTURE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("daedalus-e02-metal-roughness.png"));
    let artifact = renderer
        .render_png(&scene, &CaptureOptions::new(513, 289, &path))
        .expect("renderer should return a real PNG capture");
    assert_eq!(artifact.width, 513);
    assert_eq!(artifact.height, 289);
    assert_eq!(artifact.rgba8_bytes, 513 * 289 * 4);

    let mut reader = png::Decoder::new(BufReader::new(
        File::open(&path).expect("capture file should exist"),
    ))
    .read_info()
    .expect("capture should be a valid PNG");
    assert_eq!(reader.info().width, 513);
    assert_eq!(reader.info().height, 289);
    let mut pixels = vec![0; reader.output_buffer_size().expect("known output size")];
    let info = reader
        .next_frame(&mut pixels)
        .expect("capture pixels should decode");
    let unique_pixels: HashSet<[u8; 4]> = pixels[..info.buffer_size()]
        .chunks_exact(4)
        .map(|rgba| [rgba[0], rgba[1], rgba[2], rgba[3]])
        .collect();
    assert!(
        unique_pixels.len() > 64,
        "geometry and lighting should create varied pixels"
    );
    assert!(path.exists());
    println!(
        "capture={} adapter={} backend={} device_type={}",
        artifact.path.display(),
        artifact.renderer.adapter_name,
        artifact.renderer.backend,
        artifact.renderer.device_type,
    );
}
