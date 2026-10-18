#[cfg(any(feature = "window", test))]
use crate::{
    PacketAssetLimits, PacketRenderer, PacketRendererLimits, Renderer, RendererError,
    packet_renderer::PacketRendererInitOwner, renderer::RendererInitOwner,
};
use crate::{PacketRendererDrainError, PacketRendererDrainStatus};

/// Observable ownership phase for recoverable renderer initialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowGpuOwnerPhase {
    NoDevice,
    DeviceRequest,
    RendererPartial,
    RendererReady,
    PacketRendererPartial,
    PacketRendererReady,
}

// Keep one inline fixed resource-state union inside the owner’s preallocated
// box. Boxing each transition would allocate after GPU handles have returned.
#[allow(clippy::large_enum_variant)]
enum OwnerState {
    NoDevice,
    #[cfg(any(feature = "window", test))]
    DeviceRequest,
    #[cfg(any(feature = "window", test))]
    RendererPartial(RendererInitOwner),
    #[cfg(any(feature = "window", test))]
    RendererReady(Renderer),
    #[cfg(any(feature = "window", test))]
    PacketRendererPartial(PacketRendererInitOwner),
    #[cfg(any(feature = "window", test))]
    PacketRendererReady(PacketRenderer),
}

/// Opaque, move-only owner for GPU resources returned during window rendering.
/// It never owns a window or surface, but it retains the source pins required
/// by its internal staging records and submitted work until normal drain/drop.
pub struct WindowGpuOwner {
    state: Box<OwnerState>,
    #[cfg(test)]
    request_device_features: Option<wgpu::Features>,
    #[cfg(test)]
    panic_during_device_request: bool,
}

impl Default for WindowGpuOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowGpuOwner {
    pub fn new() -> Self {
        Self {
            state: Box::new(OwnerState::NoDevice),
            #[cfg(test)]
            request_device_features: None,
            #[cfg(test)]
            panic_during_device_request: false,
        }
    }

    pub fn phase(&self) -> WindowGpuOwnerPhase {
        match &*self.state {
            OwnerState::NoDevice => WindowGpuOwnerPhase::NoDevice,
            #[cfg(any(feature = "window", test))]
            OwnerState::DeviceRequest => WindowGpuOwnerPhase::DeviceRequest,
            #[cfg(any(feature = "window", test))]
            OwnerState::RendererPartial(_) => WindowGpuOwnerPhase::RendererPartial,
            #[cfg(any(feature = "window", test))]
            OwnerState::RendererReady(_) => WindowGpuOwnerPhase::RendererReady,
            #[cfg(any(feature = "window", test))]
            OwnerState::PacketRendererPartial(_) => WindowGpuOwnerPhase::PacketRendererPartial,
            #[cfg(any(feature = "window", test))]
            OwnerState::PacketRendererReady(_) => WindowGpuOwnerPhase::PacketRendererReady,
        }
    }

    pub fn has_device(&self) -> bool {
        match &*self.state {
            OwnerState::NoDevice => false,
            #[cfg(any(feature = "window", test))]
            OwnerState::DeviceRequest => false,
            #[cfg(any(feature = "window", test))]
            OwnerState::RendererPartial(_)
            | OwnerState::RendererReady(_)
            | OwnerState::PacketRendererPartial(_)
            | OwnerState::PacketRendererReady(_) => true,
        }
    }

    /// Completes base renderer setup in this owner. On request error the phase
    /// remains DeviceRequest; after the device pair returns, every later handle
    /// is retained in RendererPartial until the full renderer is promoted.
    #[cfg(any(feature = "window", test))]
    pub(crate) async fn initialize_renderer(
        &mut self,
        _instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        format: wgpu::TextureFormat,
    ) -> Result<(), RendererError> {
        match &*self.state {
            OwnerState::NoDevice | OwnerState::DeviceRequest => {
                *self.state = OwnerState::DeviceRequest;
                #[cfg(test)]
                let requested_features = self.request_device_features.take();
                #[cfg(test)]
                let panic_during_request = std::mem::take(&mut self.panic_during_device_request);
                #[cfg(test)]
                let request_result = if panic_during_request {
                    injected_device_request_panic().await
                } else if let Some(features) = requested_features {
                    RendererInitOwner::request_device_with_features(adapter, format, features).await
                } else {
                    RendererInitOwner::request_device(adapter, format).await
                };
                #[cfg(not(test))]
                let request_result = RendererInitOwner::request_device(adapter, format).await;
                let owner = request_result?;
                *self.state = OwnerState::RendererPartial(owner);
            }
            _ => return Err(RendererError::InitializationIncomplete),
        }

        self.finish_base_renderer_initialization()
    }

    #[cfg(any(feature = "window", test))]
    fn finish_base_renderer_initialization(&mut self) -> Result<(), RendererError> {
        let OwnerState::RendererPartial(owner) = &mut *self.state else {
            return Err(RendererError::InitializationIncomplete);
        };
        owner.initialize_base()?;
        if !owner.is_ready() {
            return Err(RendererError::InitializationIncomplete);
        }

        let state = std::mem::replace(&mut *self.state, OwnerState::NoDevice);
        let OwnerState::RendererPartial(owner) = state else {
            unreachable!("renderer phase was checked before promotion")
        };
        *self.state = OwnerState::RendererReady(owner.into_ready(true));
        Ok(())
    }

    #[cfg(any(feature = "window", test))]
    pub(crate) fn renderer(&self) -> Option<&Renderer> {
        match &*self.state {
            OwnerState::RendererReady(renderer) => Some(renderer),
            _ => None,
        }
    }

    /// Adds persistent packet resources while leaving any partial packet setup
    /// in the same owner if validation or initialization fails.
    #[cfg(any(feature = "window", test))]
    pub(crate) fn initialize_packet_renderer(
        &mut self,
        limits: PacketRendererLimits,
        asset_limits: PacketAssetLimits,
    ) -> Result<(), crate::PacketRenderError> {
        if matches!(&*self.state, OwnerState::RendererReady(_)) {
            let state = std::mem::replace(&mut *self.state, OwnerState::NoDevice);
            let OwnerState::RendererReady(renderer) = state else {
                unreachable!("renderer phase was checked before packet transition")
            };
            *self.state = OwnerState::PacketRendererPartial(PacketRendererInitOwner::new(
                renderer,
                limits,
                asset_limits,
            ));
        }
        let OwnerState::PacketRendererPartial(owner) = &mut *self.state else {
            return Err(crate::PacketRenderError::InvalidPacket(
                "packet initialization requires a ready renderer".into(),
            ));
        };
        owner.initialize()?;
        if !owner.is_ready() {
            return Err(crate::PacketRenderError::InvalidPacket(
                "packet renderer initialization is incomplete".into(),
            ));
        }

        let state = std::mem::replace(&mut *self.state, OwnerState::NoDevice);
        let OwnerState::PacketRendererPartial(owner) = state else {
            unreachable!("packet phase was checked before promotion")
        };
        *self.state = OwnerState::PacketRendererReady(owner.into_ready());
        Ok(())
    }

    #[cfg(any(feature = "window", test))]
    pub(crate) fn packet_renderer_mut(&mut self) -> Option<&mut PacketRenderer> {
        match &mut *self.state {
            OwnerState::PacketRendererReady(renderer) => Some(renderer),
            _ => None,
        }
    }

    /// Polls renderer completion without waiting. Earlier initialization phases
    /// have no submitted work and are already drained.
    pub fn try_drain_after_window(
        &mut self,
    ) -> Result<PacketRendererDrainStatus, PacketRendererDrainError> {
        match &mut *self.state {
            #[cfg(any(feature = "window", test))]
            OwnerState::PacketRendererReady(renderer) => renderer.try_drain_after_window(),
            _ => Ok(PacketRendererDrainStatus::Drained),
        }
    }

    #[cfg(test)]
    fn from_ready_renderer_for_test(renderer: Renderer) -> Self {
        let mut owner = Self::new();
        *owner.state = OwnerState::RendererReady(renderer);
        owner
    }

    #[cfg(test)]
    fn request_unsupported_features_for_test(&mut self, features: wgpu::Features) {
        self.request_device_features = Some(features);
    }

    #[cfg(test)]
    fn panic_during_device_request_for_test(&mut self) {
        self.panic_during_device_request = true;
    }
}

#[cfg(test)]
async fn injected_device_request_panic() -> Result<RendererInitOwner, RendererError> {
    std::future::ready(()).await;
    panic!("injected device request future panic before device return");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::mpsc,
        thread::{self, ThreadId},
        time::Duration,
    };

    fn assert_send<T: Send>() {}

    #[test]
    fn owner_renderer_and_packet_renderer_are_send() {
        assert_send::<WindowGpuOwner>();
        assert_send::<Renderer>();
        assert_send::<PacketRenderer>();
    }

    #[test]
    fn no_device_owner_is_already_drained() {
        let mut owner = WindowGpuOwner::new();
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::NoDevice);
        assert!(!owner.has_device());
        assert_eq!(
            owner.try_drain_after_window().unwrap(),
            PacketRendererDrainStatus::Drained
        );
    }

    fn native_adapter() -> (wgpu::Instance, wgpu::Adapter) {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("offscreen adapter");
        (instance, adapter)
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; requests one feature that adapter does not support"]
    fn device_request_error_keeps_owner_before_any_device_exists() {
        let (instance, adapter) = native_adapter();
        let unsupported = wgpu::Features::all() & !adapter.features();
        assert!(
            !unsupported.is_empty(),
            "the selected adapter must expose an unsupported feature for this request-error test"
        );
        let mut owner = WindowGpuOwner::new();
        owner.request_unsupported_features_for_test(unsupported);

        let result = pollster::block_on(owner.initialize_renderer(
            instance,
            adapter,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ));
        assert!(
            matches!(result, Err(RendererError::RequestDevice(_))),
            "an unsupported required feature must fail before a device is returned"
        );
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::DeviceRequest);
        assert!(!owner.has_device());
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; unwinds inside the device-request future"]
    fn device_request_future_panic_keeps_owner_in_request_phase() {
        let (instance, adapter) = native_adapter();
        let mut owner = WindowGpuOwner::new();
        owner.panic_during_device_request_for_test();

        let result = catch_unwind(AssertUnwindSafe(|| {
            pollster::block_on(owner.initialize_renderer(
                instance,
                adapter,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ))
        }));
        assert!(
            result.is_err(),
            "the request future must panic before a device"
        );
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::DeviceRequest);
        assert!(!owner.has_device());
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; validates an existing renderer before packet promotion"]
    fn invalid_capture_dimensions_leave_renderer_ready_owner_unchanged() {
        let (instance, adapter) = native_adapter();
        let mut owner = WindowGpuOwner::new();
        pollster::block_on(owner.initialize_renderer(
            instance,
            adapter,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ))
        .expect("base renderer should initialize");
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::RendererReady);

        let result = owner
            .renderer()
            .expect("ready renderer remains in the owner")
            .render_png(
                &crate::RenderScene::default(),
                &crate::CaptureOptions::new(0, 1, "unused.png"),
            );
        assert!(matches!(
            result,
            Err(RendererError::InvalidCaptureSize { .. })
        ));
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::RendererReady);
        assert!(owner.has_device());
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; injects unwind after device and first base handle"]
    fn base_initialization_unwind_keeps_device_and_first_layout_in_owner() {
        let (_instance, adapter) = native_adapter();
        let mut initialization = pollster::block_on(RendererInitOwner::request_device(
            adapter,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ))
        .expect("renderer device");
        initialization.panic_after_globals_layout_for_test();
        let mut owner = WindowGpuOwner::new();
        *owner.state = OwnerState::RendererPartial(initialization);

        let result = catch_unwind(AssertUnwindSafe(|| {
            owner.finish_base_renderer_initialization()
        }));
        assert!(result.is_err(), "the configured panic point must execute");
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::RendererPartial);
        assert!(owner.has_device());
        let OwnerState::RendererPartial(initialization) = &*owner.state else {
            panic!("partial base owner must remain installed after unwind");
        };
        assert!(initialization.retains_first_base_layout_for_test());
    }

    fn managed_renderer() -> Renderer {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("offscreen adapter");
        let mut initialization = pollster::block_on(RendererInitOwner::request_device(
            adapter,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ))
        .expect("renderer device");
        initialization
            .initialize_base()
            .expect("base renderer initialization");
        let mut renderer = initialization.into_ready(true);
        assert_eq!(renderer.retained_init_handle_count(), 5);
        renderer
            .ensure_model_packet_pipeline()
            .expect("managed model packet pipeline");
        renderer
            .ensure_sprite_packet_pipeline()
            .expect("managed sprite packet pipeline");
        assert_eq!(renderer.retained_init_handle_count(), 9);
        renderer
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; verifies ready owner drop on a worker"]
    fn ready_renderer_owner_drops_on_designated_worker() {
        let renderer = managed_renderer();
        let mut owner = WindowGpuOwner::from_ready_renderer_for_test(renderer);
        owner
            .initialize_packet_renderer(
                PacketRendererLimits::default(),
                PacketAssetLimits::default(),
            )
            .expect("packet renderer initialization");
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::PacketRendererReady);
        assert!(owner.has_device());
        assert_eq!(
            owner
                .packet_renderer_mut()
                .expect("packet renderer remains in owner")
                .retained_init_handle_count(),
            9,
            "base and lazy pipeline handles remain in the managed renderer"
        );
        assert_eq!(
            owner.try_drain_after_window().unwrap(),
            PacketRendererDrainStatus::Drained
        );
        let (sender, receiver) = mpsc::channel::<ThreadId>();
        let worker = thread::spawn(move || {
            struct DropOnWorker {
                owner: Option<WindowGpuOwner>,
                sender: mpsc::Sender<ThreadId>,
            }
            impl Drop for DropOnWorker {
                fn drop(&mut self) {
                    drop(self.owner.take());
                    let _ = self.sender.send(thread::current().id());
                }
            }
            drop(DropOnWorker {
                owner: Some(owner),
                sender,
            });
        });
        let dropped_on = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("worker dropped the GPU owner");
        assert_eq!(dropped_on, worker.thread().id());
        worker.join().expect("worker completed");
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; checks owner retention after initialization unwind"]
    fn packet_initialization_panic_keeps_returned_buffer_in_owner() {
        let renderer = managed_renderer();
        let mut owner = WindowGpuOwner::from_ready_renderer_for_test(renderer);
        let state = std::mem::replace(&mut *owner.state, OwnerState::NoDevice);
        let OwnerState::RendererReady(renderer) = state else {
            panic!("test renderer should be ready");
        };
        let mut packet_owner = PacketRendererInitOwner::new(
            renderer,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        );
        packet_owner.panic_after_instance_buffer_for_test();
        *owner.state = OwnerState::PacketRendererPartial(packet_owner);

        let result = catch_unwind(AssertUnwindSafe(|| {
            owner.initialize_packet_renderer(
                PacketRendererLimits::default(),
                PacketAssetLimits::default(),
            )
        }));
        assert!(result.is_err(), "the configured panic point must execute");
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::PacketRendererPartial);
        assert!(owner.has_device());
        let OwnerState::PacketRendererPartial(packet_owner) = &*owner.state else {
            panic!("partial packet owner must remain installed after unwind");
        };
        assert!(packet_owner.retains_only_first_buffer_for_test());
    }

    #[test]
    #[ignore = "requires a real wgpu adapter; checks later packet handles remain owned after unwind"]
    fn packet_initialization_panic_keeps_first_bind_group_in_owner() {
        let renderer = managed_renderer();
        let mut owner = WindowGpuOwner::from_ready_renderer_for_test(renderer);
        let state = std::mem::replace(&mut *owner.state, OwnerState::NoDevice);
        let OwnerState::RendererReady(renderer) = state else {
            panic!("test renderer should be ready");
        };
        let mut packet_owner = PacketRendererInitOwner::new(
            renderer,
            PacketRendererLimits::default(),
            PacketAssetLimits::default(),
        );
        packet_owner.panic_after_globals_bind_group_for_test();
        *owner.state = OwnerState::PacketRendererPartial(packet_owner);

        let result = catch_unwind(AssertUnwindSafe(|| {
            owner.initialize_packet_renderer(
                PacketRendererLimits::default(),
                PacketAssetLimits::default(),
            )
        }));
        assert!(result.is_err(), "the configured panic point must execute");
        assert_eq!(owner.phase(), WindowGpuOwnerPhase::PacketRendererPartial);
        let OwnerState::PacketRendererPartial(packet_owner) = &*owner.state else {
            panic!("partial packet owner must remain installed after unwind");
        };
        assert!(packet_owner.retains_buffers_and_first_bind_group_for_test());
    }
}
