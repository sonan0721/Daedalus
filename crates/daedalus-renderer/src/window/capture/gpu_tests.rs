use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::{Duration, Instant},
};

use wgpu::{Texture, TextureFormat, TextureUsages};

use super::{
    ManagedCaptureCell, ManagedCaptureCleanup, ManagedCaptureClient, ManagedCaptureKey,
    ManagedCapturePixels, ManagedCaptureReadyInfo, ManagedCaptureRequest, ManagedCaptureReturn,
    ManagedCaptureSource, ManagedCaptureSurfaceFormat,
};
use crate::{
    PacketRenderer, PacketRendererLimits, RenderPacket, Renderer,
    window::{ManagedPresentedView, ManagedStageKey},
};

const WIDTH: u32 = 3;
const HEIGHT: u32 = 2;
const CLEAR: [f32; 4] = [0.13, 0.34, 0.72, 0.37];
const GPU_TIMEOUT: Duration = Duration::from_secs(8);

struct CaptureFixture {
    renderer: PacketRenderer,
    client: ManagedCaptureClient,
    shared: Arc<super::ManagedCaptureShared>,
    worker: super::ManagedCaptureWorker,
    cell: Arc<ManagedCaptureCell>,
    frame: Arc<crate::CpuFrameHandle>,
    packet: Arc<RenderPacket>,
    texture: Texture,
    ready: ManagedCaptureReadyInfo,
    format: ManagedCaptureSurfaceFormat,
    next_key: u64,
}

impl CaptureFixture {
    fn new(texture_format: TextureFormat, format: ManagedCaptureSurfaceFormat) -> Self {
        let frame = super::tests::actual_cpu_frame();
        let packet = Arc::new(frame.packet().clone());
        assert_eq!(packet.clear_color(), CLEAR);
        let mut renderer = Renderer::new_offscreen_blocking_with_format_for_test(texture_format)
            .expect("test adapter supports the requested capture format")
            .into_packet_renderer(PacketRendererLimits::default())
            .expect("packet renderer initializes on the selected format");
        let texture = renderer.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("Daedalus live capture format test target"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture_format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        renderer
            .render_packet_view(&view, WIDTH, HEIGHT, Arc::clone(&packet), None, None)
            .expect("the real packet clear pass renders into the selected storage format");

        let ready = ManagedCaptureReadyInfo::new(format, WIDTH, HEIGHT, u64::from(WIDTH * HEIGHT))
            .expect("capture limits accept the small test target");
        let (client, shared) = ManagedCaptureClient::new();
        let worker = client
            .register_worker()
            .expect("worker registers before requests");
        shared
            .try_set_ready(ready)
            .expect("test surface is capture-ready");
        let cell = shared.cell();
        renderer.attach_managed_capture_cell(Arc::clone(&cell));
        Self {
            renderer,
            client,
            shared,
            worker,
            cell,
            frame,
            packet,
            texture,
            ready,
            format,
            next_key: 1,
        }
    }

    fn admit_request(&mut self) -> ManagedCaptureKey {
        let key = ManagedCaptureKey::new(self.next_key).expect("test keys are nonzero");
        self.next_key += 1;
        let source = ManagedCaptureSource::new(
            ManagedStageKey::INITIAL,
            self.packet.session(),
            self.packet.project_generation(),
            self.packet.source_digest(),
        )
        .expect("packet identity is a valid capture source");
        let request = ManagedCaptureRequest::new(
            key,
            source,
            self.packet.tick(),
            Instant::now() + GPU_TIMEOUT,
        )
        .expect("test request deadline is in the future");
        self.client
            .try_submit(request)
            .unwrap_or_else(|(_, error)| panic!("capture request admitted: {error:?}"));
        self.shared
            .try_take_request()
            .expect("request slot is available")
            .expect("accepted request is owned by the renderer test")
            .key()
    }

    fn submit_on_selected_frame(
        &mut self,
        key: ManagedCaptureKey,
    ) -> Result<(), super::ManagedCaptureFailure> {
        let mut guard = self
            .cell
            .try_lock_for_ui(key)
            .expect("the accepted request owns the reserved cell");
        let copied = self.renderer.try_submit_managed_capture_copy(
            &self.texture,
            WIDTH,
            HEIGHT,
            self.ready,
            self.format,
            &self.frame,
            &self.cell,
            &mut guard,
        )?;
        assert!(copied, "small test copy fits the staging ledger");
        let presented = ManagedPresentedView::new(
            ManagedStageKey::INITIAL,
            &self.packet,
            key.get(),
            WIDTH,
            HEIGHT,
            self.packet.view(),
        )
        .expect("selected view metadata is valid");
        guard
            .mark_submitted(key, presented)
            .expect("copy is eligible for the submitted state");
        guard.disarm_cleanup_on_drop();
        Ok(())
    }

    fn poll_gpu(&mut self) {
        self.renderer
            .device()
            .poll(wgpu::PollType::Poll)
            .expect("test GPU work completes before the bounded deadline");
        self.renderer
            .poll_managed_capture()
            .expect("capture retirement remains valid");
    }

    fn pause_between_nonblocking_polls() {
        std::thread::sleep(Duration::from_millis(1));
    }

    fn take_pixels(&mut self) -> ManagedCapturePixels {
        let deadline = Instant::now() + GPU_TIMEOUT;
        let mut readback = loop {
            self.poll_gpu();
            if let Some(result) = self
                .worker
                .try_take_return()
                .expect("worker polls its bounded cell without waiting")
            {
                match result {
                    ManagedCaptureReturn::Submitted(readback) => break readback,
                    ManagedCaptureReturn::Failed { failure, .. } => {
                        panic!("capture unexpectedly failed: {failure:?}")
                    }
                    ManagedCaptureReturn::Cleanup(_) => {
                        panic!("successful copy unexpectedly returned cleanup")
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "GPU capture return arrived before timeout"
            );
            Self::pause_between_nonblocking_polls();
        };
        loop {
            match readback
                .try_read_rgba8()
                .expect("real mapped target reads within the capture cell")
            {
                Some(pixels) => return pixels,
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "GPU map completed before timeout"
                    );
                    Self::pause_between_nonblocking_polls();
                    self.poll_gpu();
                }
            }
        }
    }

    fn finish_worker_cleanup(&mut self, cleanup: &mut ManagedCaptureCleanup) {
        let deadline = Instant::now() + GPU_TIMEOUT;
        loop {
            let _ = self
                .renderer
                .try_drain_after_window()
                .expect("GPU owner installs a missing post-submit queue fence");
            self.poll_gpu();
            if cleanup
                .try_finish()
                .expect("worker cleanup only releases after independent GPU completion")
            {
                assert!(self.cell.is_released());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "GPU cleanup completed before timeout"
            );
            Self::pause_between_nonblocking_polls();
        }
    }
}

fn texture_format_and_surface(format: ManagedCaptureSurfaceFormat) -> TextureFormat {
    match format {
        ManagedCaptureSurfaceFormat::Rgba8Unorm => TextureFormat::Rgba8Unorm,
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb => TextureFormat::Rgba8UnormSrgb,
        ManagedCaptureSurfaceFormat::Bgra8Unorm => TextureFormat::Bgra8Unorm,
        ManagedCaptureSurfaceFormat::Bgra8UnormSrgb => TextureFormat::Bgra8UnormSrgb,
    }
}

fn expected_srgb_byte(linear: f32) -> u8 {
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded.clamp(0.0, 1.0) * f32::from(u8::MAX)).round() as u8
}

#[test]
#[ignore = "requires a native wgpu adapter; checks actual linear/sRGB clear, raw copy, and channel order"]
fn managed_capture_formats_preserve_manual_srgb_clear_and_raw_rgba_bytes() {
    for format in [
        ManagedCaptureSurfaceFormat::Rgba8Unorm,
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
        ManagedCaptureSurfaceFormat::Bgra8Unorm,
        ManagedCaptureSurfaceFormat::Bgra8UnormSrgb,
    ] {
        let mut fixture = CaptureFixture::new(texture_format_and_surface(format), format);
        let key = fixture.admit_request();
        fixture
            .submit_on_selected_frame(key)
            .expect("capture copy submits for each supported linear/sRGB RGBA/BGRA target");
        let frame_weak = Arc::downgrade(&fixture.frame);
        let pixels = fixture.take_pixels();
        assert_eq!(pixels.key(), key);
        assert_eq!(pixels.surface_format(), format);
        assert_eq!(pixels.presented().physical_width(), WIDTH);
        assert_eq!(pixels.presented().physical_height(), HEIGHT);
        let pinned = pixels.rgba8().chunks_exact(4).collect::<Vec<_>>();
        assert_eq!(pinned.len(), (WIDTH * HEIGHT) as usize);
        let expected = [
            expected_srgb_byte(CLEAR[0]),
            expected_srgb_byte(CLEAR[1]),
            expected_srgb_byte(CLEAR[2]),
            u8::MAX,
        ];
        for pixel in pinned {
            for (actual, expected) in pixel.iter().zip(expected) {
                assert!(
                    actual.abs_diff(expected) <= 1,
                    "{format:?} raw-copy pixel {pixel:?} differs from sRGB clear {expected:?}"
                );
            }
        }
        assert!(frame_weak.upgrade().is_some());
        drop(fixture.frame);
        assert!(frame_weak.upgrade().is_none());
    }
}

#[test]
#[ignore = "requires a native wgpu adapter; checks real queue/map unwind and callback failure cleanup"]
fn managed_capture_unwind_and_aborted_map_keep_the_cell_until_real_completion() {
    let mut fixture = CaptureFixture::new(
        TextureFormat::Rgba8UnormSrgb,
        ManagedCaptureSurfaceFormat::Rgba8UnormSrgb,
    );

    let key = fixture.admit_request();
    let first_weak = Arc::downgrade(&fixture.frame);
    fixture.renderer.panic_after_capture_submit_for_test();
    let submit_panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = fixture.submit_on_selected_frame(key);
    }));
    assert!(
        submit_panic.is_err(),
        "post-submit fault is injected after actual queue submit"
    );
    assert!(first_weak.upgrade().is_some());
    let Some(ManagedCaptureReturn::Cleanup(mut cleanup)) = fixture
        .worker
        .try_take_return()
        .expect("post-submit unwind is visible as cleanup")
    else {
        panic!("post-submit unwind must preserve its one cleanup return");
    };
    assert!(
        fixture
            .cell
            .queue_complete_key
            .load(std::sync::atomic::Ordering::Acquire)
            < key.get(),
        "the injected panic occurs before the ordinary queue-completion callback is installed"
    );
    assert!(
        !cleanup
            .try_finish()
            .expect("pending post-submit cleanup is a normal incomplete result"),
        "the worker cannot release a possibly submitted copy before observing a queue fence"
    );
    assert!(!fixture.cell.is_released());
    assert!(first_weak.upgrade().is_some());
    fixture.finish_worker_cleanup(&mut cleanup);
    assert!(
        fixture
            .cell
            .queue_complete_key
            .load(std::sync::atomic::Ordering::Acquire)
            >= key.get()
    );
    assert_eq!(Arc::strong_count(&fixture.frame), 1);

    // The same durable cell is reused only after its worker-side GPU cleanup completed.
    let second_key = fixture.admit_request();
    let second_weak = Arc::downgrade(&fixture.frame);
    let second_count = Arc::strong_count(&fixture.frame);
    fixture
        .renderer
        .panic_after_capture_map_registration_for_test();
    let map_panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = fixture.submit_on_selected_frame(second_key);
    }));
    assert!(
        map_panic.is_err(),
        "post-map-registration fault is injected after map_async"
    );
    assert!(second_weak.upgrade().is_some());
    assert_eq!(Arc::strong_count(&fixture.frame), second_count + 1);
    let Some(ManagedCaptureReturn::Cleanup(mut cleanup)) = fixture
        .worker
        .try_take_return()
        .expect("post-map unwind is visible as cleanup")
    else {
        panic!("post-map unwind must preserve its one cleanup return");
    };
    assert_eq!(
        fixture
            .cell
            .map_registered_key
            .load(std::sync::atomic::Ordering::Acquire),
        second_key.get(),
        "the injected map unwind follows actual map_async registration"
    );
    let queue_pending = fixture
        .cell
        .queue_complete_key
        .load(std::sync::atomic::Ordering::Acquire)
        < second_key.get();
    let map_pending = fixture
        .cell
        .map_pending_key
        .load(std::sync::atomic::Ordering::Acquire)
        == second_key.get()
        && fixture
            .cell
            .map_success_key
            .load(std::sync::atomic::Ordering::Acquire)
            < second_key.get()
        && fixture
            .cell
            .map_failure_key
            .load(std::sync::atomic::Ordering::Acquire)
            < second_key.get();
    if queue_pending || map_pending {
        assert!(
            !cleanup
                .try_finish()
                .expect("pending actual map/queue work is a normal incomplete result"),
            "an observed pending queue or map flag keeps the cell reserved"
        );
        assert!(!fixture.cell.is_released());
    }
    fixture.finish_worker_cleanup(&mut cleanup);
    assert!(
        fixture
            .cell
            .queue_complete_key
            .load(std::sync::atomic::Ordering::Acquire)
            >= second_key.get()
    );
    assert!(
        fixture
            .cell
            .map_success_key
            .load(std::sync::atomic::Ordering::Acquire)
            >= second_key.get()
            || fixture
                .cell
                .map_failure_key
                .load(std::sync::atomic::Ordering::Acquire)
                >= second_key.get(),
        "the real map callback must report success or failure before releasing the cell"
    );
    assert_eq!(Arc::strong_count(&fixture.frame), second_count);

    // A real unmap before polling must produce wgpu's registered map callback error. Queue
    // completion remains separately observed before the worker releases the cell.
    let third_key = fixture.admit_request();
    fixture.renderer.abort_capture_map_before_poll_for_test();
    fixture
        .submit_on_selected_frame(third_key)
        .expect("map-abort copy submit reaches map_async and unmaps before poll");
    let deadline = Instant::now() + GPU_TIMEOUT;
    while fixture
        .cell
        .map_failure_key
        .load(std::sync::atomic::Ordering::Acquire)
        < third_key.get()
        || fixture
            .cell
            .queue_complete_key
            .load(std::sync::atomic::Ordering::Acquire)
            < third_key.get()
    {
        assert!(
            Instant::now() < deadline,
            "aborted map and queue fence complete before timeout"
        );
        fixture.poll_gpu();
        CaptureFixture::pause_between_nonblocking_polls();
    }
    assert_eq!(
        fixture
            .cell
            .map_failure_key
            .load(std::sync::atomic::Ordering::Acquire),
        third_key.get(),
        "the real wgpu map callback reports the induced unmap failure"
    );
    let Some(ManagedCaptureReturn::Submitted(readback)) = fixture
        .worker
        .try_take_return()
        .expect("submitted copy remains observable after map failure")
    else {
        panic!("map failure still returns the submitted buffer to worker cleanup");
    };
    let mut readback = readback;
    assert!(matches!(
        readback.try_read_rgba8(),
        Err(super::ManagedCaptureReadbackError::MapFailed)
    ));
    let mut cleanup = readback.into_cleanup();
    fixture.finish_worker_cleanup(&mut cleanup);
    assert!(fixture.cell.is_released());
}
