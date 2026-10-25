#[cfg(not(target_family = "wasm"))]
use anyhow::Context as _;
#[cfg(not(target_family = "wasm"))]
use gpui_util::ResultExt;
#[cfg(not(target_family = "wasm"))]
use smallvec::SmallVec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wgpu::TextureFormat;

/// Environment variable that forces the HWND swapchain path on Windows.
pub const DISABLE_DIRECT_COMPOSITION_ENV: &str = "GPUI_DISABLE_DIRECT_COMPOSITION";

/// A single native graphics API that can back a [`WgpuContext`].
///
/// Keeping this as an enum, rather than passing a [`wgpu::Backends`] bit-set through the native
/// initialization path, makes it impossible for the renderer to accidentally initialize a
/// fallback API alongside the preferred one. In particular, constructing WGPU's GL backend
/// creates and retains EGL state even when a Vulkan adapter is eventually selected.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(not(target_family = "wasm"))]
pub enum NativeBackend {
    #[cfg(target_vendor = "apple")]
    Metal,
    #[cfg(target_os = "windows")]
    Dx12,
    #[cfg(not(target_vendor = "apple"))]
    Vulkan,
    #[cfg(not(target_vendor = "apple"))]
    Gl,
}

/// A WGPU instance paired with the single native API it was created to load.
///
/// Keeping the backend token attached prevents later initialization stages from
/// accidentally widening adapter discovery back to multiple APIs.
#[cfg(not(target_family = "wasm"))]
pub(crate) struct NativeInstance {
    pub(crate) backend: NativeBackend,
    pub(crate) raw: wgpu::Instance,
}

#[cfg(not(target_family = "wasm"))]
impl NativeBackend {
    #[cfg(target_vendor = "apple")]
    pub(crate) const PREFERENCE: &'static [Self] = &[Self::Metal];
    #[cfg(target_os = "windows")]
    pub(crate) const PREFERENCE: &'static [Self] = &[Self::Dx12, Self::Vulkan, Self::Gl];
    #[cfg(not(any(target_vendor = "apple", target_os = "windows")))]
    pub(crate) const PREFERENCE: &'static [Self] = &[Self::Vulkan, Self::Gl];

    pub(crate) fn instance(
        self,
        display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>,
    ) -> NativeInstance {
        #[cfg(not(target_os = "windows"))]
        let backend_options = wgpu::BackendOptions::default();
        #[cfg(target_os = "windows")]
        let backend_options = match self {
            Self::Dx12 => {
                let mut options = wgpu::BackendOptions::default();
                let direct_composition_disabled = std::env::var(DISABLE_DIRECT_COMPOSITION_ENV)
                    .is_ok_and(|value| value == "true" || value == "1");
                options.dx12.presentation_system = if direct_composition_disabled {
                    wgpu::Dx12SwapchainKind::DxgiFromHwnd
                } else {
                    wgpu::Dx12SwapchainKind::DxgiFromVisual
                };
                options
            }
            Self::Vulkan | Self::Gl => wgpu::BackendOptions::default(),
        };

        NativeInstance {
            backend: self,
            raw: wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: self.into(),
                flags: wgpu::InstanceFlags::default(),
                backend_options,
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                display,
            }),
        }
    }

    pub(crate) fn try_in_preference_order<T>(
        operation: &'static str,
        attempt: impl FnMut(Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        Self::try_in_order(operation, Self::PREFERENCE, attempt)
    }

    pub(crate) fn try_in_order<T>(
        operation: &'static str,
        backends: &[Self],
        mut attempt: impl FnMut(Self) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if backends.is_empty() {
            anyhow::bail!("no native GPU backends supplied for {operation}");
        }
        let mut failures = SmallVec::<[NativeBackendFailure; 3]>::new();
        for &backend in backends {
            match attempt(backend) {
                Ok(value) => return Ok(value),
                Err(source) => failures.push(NativeBackendFailure { backend, source }),
            }
        }
        Err(NativeBackendFallbackError {
            operation,
            failures,
        }
        .into())
    }
}

#[cfg(not(target_family = "wasm"))]
impl From<NativeBackend> for wgpu::Backends {
    fn from(backend: NativeBackend) -> Self {
        match backend {
            #[cfg(target_vendor = "apple")]
            NativeBackend::Metal => Self::METAL,
            #[cfg(target_os = "windows")]
            NativeBackend::Dx12 => Self::DX12,
            #[cfg(not(target_vendor = "apple"))]
            NativeBackend::Vulkan => Self::VULKAN,
            #[cfg(not(target_vendor = "apple"))]
            NativeBackend::Gl => Self::GL,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug)]
struct NativeBackendFailure {
    backend: NativeBackend,
    source: anyhow::Error,
}

#[cfg(not(target_family = "wasm"))]
#[derive(Debug)]
struct NativeBackendFallbackError {
    operation: &'static str,
    failures: SmallVec<[NativeBackendFailure; 3]>,
}

#[cfg(not(target_family = "wasm"))]
impl std::fmt::Display for NativeBackendFallbackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "failed to initialize {}; attempted backends in order: ",
            self.operation
        )?;
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{:?}: {:#}", failure.backend, failure.source)?;
        }
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
impl std::error::Error for NativeBackendFallbackError {}

#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SoftwareAdapterPolicy {
    Allow,
    Reject,
}

#[cfg(not(target_family = "wasm"))]
impl SoftwareAdapterPolicy {
    fn accepts(self, device_type: wgpu::DeviceType) -> bool {
        self == Self::Allow || device_type != wgpu::DeviceType::Cpu
    }
}

struct CreatedDevice {
    device: wgpu::Device,
    queue: wgpu::Queue,
    dual_source_blending: bool,
    color_texture_format: TextureFormat,
    renderer_tier: RendererTier,
}

#[cfg(not(target_family = "wasm"))]
struct SelectedAdapter {
    adapter: wgpu::Adapter,
    device: CreatedDevice,
}

#[derive(Clone)]
pub struct WgpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    backend: WgpuBackend,
    dual_source_blending: bool,
    color_texture_format: wgpu::TextureFormat,
    renderer_tier: RendererTier,
    errors: Arc<DeviceErrorState>,
}

/// The resource transport selected for a device.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RendererTier {
    /// WebGPU-class devices use storage buffers for packed scene data.
    Modern,
    /// GLES 3.0 and WebGL2 use a build-generated data-texture transport.
    WebGl2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WgpuBackend {
    BrowserWebGpu,
    Gl,
    Native(wgpu::Backend),
}

#[cfg(target_family = "wasm")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WebBackendPreference {
    #[default]
    Auto,
    WebGpu,
    WebGl,
}

#[cfg(target_family = "wasm")]
fn web_backend_options(preference: WebBackendPreference) -> wgpu::BackendOptions {
    let mut options = wgpu::BackendOptions::default();
    if preference != WebBackendPreference::WebGpu {
        // Browser WebGL cannot block in clientWaitSync with a timeout. wgpu's surface
        // teardown/reconfigure waits for submitted work, so use its documented browser
        // workaround whenever GL is an available backend. Completion callbacks and device
        // maintenance may report work complete before it has physically finished; native GL
        // and Browser WebGPU keep defaults.
        options.gl.fence_behavior = wgpu::GlFenceBehavior::AutoFinish;
    }
    options
}

#[cfg(target_family = "wasm")]
pub struct PreparedWebGraphics {
    pub context: WgpuContext,
    pub surface: wgpu::Surface<'static>,
}

/// wgpu-core refuses to create a surface when neither the instance nor the surface
/// target carries a display handle, and `SurfaceTarget::Canvas` always passes `None`.
/// The WebGL2 backend never reads the handle (WebGPU bypasses wgpu-core entirely), so
/// a unit web display handle on the instance satisfies the check.
#[cfg(target_family = "wasm")]
#[derive(Debug)]
struct WebDisplaySource;

#[cfg(target_family = "wasm")]
impl raw_window_handle::HasDisplayHandle for WebDisplaySource {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(raw_window_handle::DisplayHandle::web())
    }
}

#[derive(Clone, Copy)]
pub struct CompositorGpuHint {
    pub vendor_id: u32,
    pub device_id: u32,
}

/// A typed view of the GPU resources shared with GPUI.
///
/// The device and queue are owned by GPUI and remain valid until
/// [`Self::device_lost`] returns `true`. A control should drop its GPU
/// resources and reacquire this handle after recovery.
#[derive(Clone)]
pub struct WgpuContextHandle {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    texture_format: wgpu::TextureFormat,
    adapter_info: wgpu::AdapterInfo,
    backend: WgpuBackend,
    features: wgpu::Features,
    limits: wgpu::Limits,
    device_lost: Arc<AtomicBool>,
}

impl WgpuContextHandle {
    pub(crate) fn from_resources(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        texture_format: wgpu::TextureFormat,
        adapter_info: wgpu::AdapterInfo,
        backend: WgpuBackend,
        device_lost: Arc<AtomicBool>,
    ) -> Self {
        let features = device.features();
        let limits = device.limits();
        Self {
            device,
            queue,
            texture_format,
            adapter_info,
            backend,
            features,
            limits,
            device_lost,
        }
    }

    /// Returns the shared device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Returns the shared queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Returns whether two handles refer to the same device instance.
    pub fn is_same_device(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.device, &other.device)
    }

    /// Returns the texture format used by GPUI's window surface.
    pub fn texture_format(&self) -> wgpu::TextureFormat {
        self.texture_format
    }

    /// Returns information about the selected adapter.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// Returns the backend selected for this window.
    pub fn backend(&self) -> WgpuBackend {
        self.backend
    }

    /// Returns the features enabled on the shared device.
    pub fn features(&self) -> wgpu::Features {
        self.features
    }

    /// Returns the limits used when creating the shared device.
    pub fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// Returns whether the device has been lost and custom resources must be
    /// recreated after GPUI recovers.
    pub fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Relaxed)
    }

    /// Returns the typed wgpu context associated with a GPUI window.
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        all(target_os = "macos", feature = "custom-gpu"),
        all(target_family = "wasm", feature = "custom-gpu")
    ))]
    pub fn from_window(window: &gpui::Window) -> Option<Self> {
        window
            .gpu_context_info()?
            .downcast::<Self>()
            .ok()
            .map(|context| *context)
    }
}

/// A reusable offscreen render target suitable for [`gpui::Window::paint_surface`].
pub struct WgpuRenderTarget {
    texture: Arc<wgpu::Texture>,
    view: wgpu::TextureView,
    size: gpui::Size<gpui::DevicePixels>,
    format: wgpu::TextureFormat,
}

impl WgpuRenderTarget {
    /// Creates a render target using GPUI's surface format.
    pub fn new(context: &WgpuContextHandle, size: gpui::Size<gpui::DevicePixels>) -> Self {
        Self::with_format(context, size, context.texture_format())
    }

    /// Creates a render target with an explicit texture format.
    pub fn with_format(
        context: &WgpuContextHandle,
        size: gpui::Size<gpui::DevicePixels>,
        format: wgpu::TextureFormat,
    ) -> Self {
        let size = normalize_target_size(size);
        let texture = create_render_target_texture(context.device(), size, format);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture: Arc::new(texture),
            view,
            size,
            format,
        }
    }

    /// Resizes the target, preserving its texture format.
    pub fn resize(&mut self, context: &WgpuContextHandle, size: gpui::Size<gpui::DevicePixels>) {
        let size = normalize_target_size(size);
        if self.size == size {
            return;
        }
        let texture = create_render_target_texture(context.device(), size, self.format);
        self.view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.texture = Arc::new(texture);
        self.size = size;
    }

    /// Returns the target texture for use as a GPUI surface.
    pub fn texture(&self) -> Arc<wgpu::Texture> {
        Arc::clone(&self.texture)
    }

    /// Creates a GPUI element that composites this target at its layout bounds.
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        all(target_os = "macos", feature = "custom-gpu"),
        all(target_family = "wasm", feature = "custom-gpu")
    ))]
    pub fn surface(&self) -> gpui::Surface {
        gpui::surface(gpui::SurfaceSource::Texture {
            texture: self.texture(),
            size: self.size,
        })
    }

    /// Returns the target texture view for rendering.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Returns the target dimensions in device pixels.
    pub fn size(&self) -> gpui::Size<gpui::DevicePixels> {
        self.size
    }

    /// Returns the target texture format.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }
}

fn normalize_target_size(size: gpui::Size<gpui::DevicePixels>) -> gpui::Size<gpui::DevicePixels> {
    gpui::size(
        gpui::DevicePixels(size.width.0.max(1)),
        gpui::DevicePixels(size.height.0.max(1)),
    )
}

fn create_render_target_texture(
    device: &wgpu::Device,
    size: gpui::Size<gpui::DevicePixels>,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gpui-custom-render-target"),
        size: wgpu::Extent3d {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// Extra wgpu features and limits that an application can request on top of
/// gpui's baseline.  Pass an instance to the platform via
/// [`gpui::App::set_gpu_requirements`] *before* opening any windows.
#[derive(Clone, Debug, Default)]
pub struct WgpuDeviceRequirements {
    /// Additional [`wgpu::Features`] to enable.  These are OR-ed with gpui's
    /// own required features.
    pub features: wgpu::Features,
    /// Additional [`wgpu::Limits`] to request.  Each field is merged by taking
    /// `max(gpui_limit, app_limit)` for upper-bound limits and
    /// `min(gpui_limit, app_limit)` for alignment/lower-bound limits.
    pub limits: Option<wgpu::Limits>,
}

impl WgpuContext {
    pub(crate) fn handle(&self, texture_format: wgpu::TextureFormat) -> WgpuContextHandle {
        WgpuContextHandle::from_resources(
            Arc::clone(&self.device),
            Arc::clone(&self.queue),
            texture_format,
            self.adapter.get_info(),
            self.backend,
            Arc::clone(&self.errors.lost),
        )
    }

    /// Creates a native device without a presentation surface.
    ///
    /// Creating a device takes tens of milliseconds, and tests and benchmarks create a headless
    /// renderer for every window they open, so a device without extra requirements is shared by
    /// the whole process until it is lost. The cache is a static that is never dropped, because
    /// wgpu's queue reads its own thread-locals on drop, which panics during thread teardown.
    #[cfg(not(target_family = "wasm"))]
    pub fn new_headless(
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<Self> {
        static SHARED: Mutex<Option<WgpuContext>> = Mutex::new(None);
        if extra_requirements.is_some() {
            return Self::create_headless(extra_requirements);
        }
        let mut shared = SHARED.lock().unwrap();
        if let Some(context) = shared.as_ref()
            && !context.device_lost()
        {
            return Ok(context.clone());
        }
        let context = Self::create_headless(None)?;
        *shared = Some(context.clone());
        Ok(context)
    }

    #[cfg(not(target_family = "wasm"))]
    fn create_headless(
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<Self> {
        NativeBackend::try_in_preference_order("a headless GPU context", |backend| {
            let instance = backend.instance(None);
            let adapter =
                gpui::block_on(instance.raw.request_adapter(&wgpu::RequestAdapterOptions {
                    // LowPower avoids waking a discrete GPU just for snapshots on dual-GPU
                    // systems.
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                }))
                .map_err(|error| {
                    anyhow::anyhow!("failed to request headless GPU adapter: {error}")
                })?;
            let device = gpui::block_on(Self::create_device(&adapter, extra_requirements))?;
            Self::from_created_device(instance.raw, adapter, device)
        })
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn new_with_adapter_policy(
        instance: NativeInstance,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<CompositorGpuHint>,
        adapter_policy: SoftwareAdapterPolicy,
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<Self> {
        let device_id_filter = match std::env::var("ZED_DEVICE_ID") {
            Ok(val) => parse_pci_id(&val)
                .context("Failed to parse device ID from `ZED_DEVICE_ID` environment variable")
                .log_err(),
            Err(std::env::VarError::NotPresent) => None,
            err => {
                err.context("Failed to read value of `ZED_DEVICE_ID` environment variable")
                    .log_err();
                None
            }
        };

        // Select an adapter by actually testing surface configuration with the real device.
        // This is the only reliable way to determine compatibility on hybrid GPU systems.
        let selection = gpui::block_on(Self::select_adapter_and_device(
            &instance.raw,
            instance.backend,
            device_id_filter,
            surface,
            compositor_gpu.as_ref(),
            adapter_policy,
            extra_requirements,
        ))?;

        Self::from_created_device(instance.raw, selection.adapter, selection.device)
    }

    #[cfg(target_family = "wasm")]
    pub async fn new_web(
        canvas: &web_sys::HtmlCanvasElement,
        preference: WebBackendPreference,
    ) -> anyhow::Result<PreparedWebGraphics> {
        Self::new_web_with_backend(canvas, preference).await
    }

    #[cfg(target_family = "wasm")]
    #[allow(clippy::arc_with_non_send_sync)]
    async fn new_web_with_backend(
        canvas: &web_sys::HtmlCanvasElement,
        preference: WebBackendPreference,
    ) -> anyhow::Result<PreparedWebGraphics> {
        let backends = match preference {
            WebBackendPreference::Auto => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            WebBackendPreference::WebGpu => wgpu::Backends::BROWSER_WEBGPU,
            WebBackendPreference::WebGl => wgpu::Backends::GL,
        };
        let descriptor = wgpu::InstanceDescriptor {
            backends,
            flags: wgpu::InstanceFlags::default(),
            backend_options: web_backend_options(preference),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: Some(Box::new(WebDisplaySource)),
        };
        let instance = if preference == WebBackendPreference::Auto {
            wgpu::util::new_instance_with_webgpu_detection(descriptor).await
        } else {
            wgpu::Instance::new(descriptor)
        };
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|error| {
                anyhow::anyhow!("Failed to create browser graphics surface: {error}")
            })?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Failed to request a {preference:?} adapter compatible with the canvas: {error}"
                )
            })?;
        let device = Self::create_device(&adapter, None).await?;
        let context = Self::from_created_device(instance, adapter, device)?;
        log::info!(
            "Browser graphics initialized: requested={preference:?}, selected={backend:?}, \
             adapter={:?}, limits={:?}, dual_source_blending={dual_source_blending}",
            context.adapter.get_info().name,
            context.device.limits(),
            backend = context.backend,
            dual_source_blending = context.dual_source_blending,
        );
        Ok(PreparedWebGraphics { context, surface })
    }

    fn from_created_device(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: CreatedDevice,
    ) -> anyhow::Result<Self> {
        let errors = DeviceErrorState::install(&device.device);
        let info = adapter.get_info();
        log::info!("Selected GPU adapter: {:?} ({:?})", info.name, info.backend);
        #[cfg(target_family = "wasm")]
        let backend = match info.backend {
            wgpu::Backend::BrowserWebGpu => WgpuBackend::BrowserWebGpu,
            wgpu::Backend::Gl => WgpuBackend::Gl,
            backend => anyhow::bail!(
                "Browser graphics initialization selected unexpected backend {backend:?}"
            ),
        };
        #[cfg(not(target_family = "wasm"))]
        let backend = WgpuBackend::Native(info.backend);

        Ok(Self {
            instance,
            adapter,
            backend,
            device: Arc::new(device.device),
            queue: Arc::new(device.queue),
            dual_source_blending: device.dual_source_blending,
            color_texture_format: device.color_texture_format,
            renderer_tier: device.renderer_tier,
            errors,
        })
    }

    async fn create_device(
        adapter: &wgpu::Adapter,
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<CreatedDevice> {
        let renderer_tier = renderer_tier(adapter);
        // Our LCD shader uses storage buffers even when the adapter exposes dual-source
        // blending. Downlevel devices must use grayscale and the data-texture dialect.
        let dual_source_blending = renderer_tier == RendererTier::Modern
            && adapter
                .features()
                .contains(wgpu::Features::DUAL_SOURCE_BLENDING);

        let mut required_features = wgpu::Features::empty();
        if dual_source_blending {
            required_features |= wgpu::Features::DUAL_SOURCE_BLENDING;
        } else {
            log::warn!(
                "Dual-source blending not available on this GPU. \
                Subpixel text antialiasing will be disabled."
            );
        }

        let color_texture_format = Self::select_color_texture_format(adapter)?;

        let baseline = match renderer_tier {
            RendererTier::Modern => wgpu::Limits::downlevel_defaults(),
            RendererTier::WebGl2 => wgpu::Limits::downlevel_webgl2_defaults(),
        };
        let mut required_limits = baseline
            .using_resolution(adapter.limits())
            .using_alignment(adapter.limits());

        // Merge application-requested requirements.
        if let Some(reqs) = extra_requirements {
            required_features |= reqs.features;
            if let Some(limits) = &reqs.limits {
                required_limits = required_limits.or_better_values_from(limits);
            }
        }

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gpui_device"),
                required_features,
                required_limits,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create wgpu device: {e}"))?;

        Ok(CreatedDevice {
            device,
            queue,
            dual_source_blending,
            color_texture_format,
            renderer_tier,
        })
    }

    pub fn check_compatible_with_surface(&self, surface: &wgpu::Surface<'_>) -> anyhow::Result<()> {
        let caps = surface.get_capabilities(&self.adapter);
        if caps.formats.is_empty() {
            let info = self.adapter.get_info();
            anyhow::bail!(
                "Adapter {:?} (backend={:?}, device={:#06x}) is not compatible with the \
                 display surface for this window.",
                info.name,
                info.backend,
                info.device,
            );
        }
        Ok(())
    }

    /// Select an adapter and create a device, testing that the surface can actually be configured.
    /// This is the only reliable way to determine compatibility on hybrid GPU systems, where
    /// adapters may report surface compatibility via get_capabilities() but fail when actually
    /// configuring (e.g., NVIDIA reporting Vulkan Wayland support but failing because the
    /// Wayland compositor runs on the Intel GPU).
    #[cfg(not(target_family = "wasm"))]
    async fn select_adapter_and_device(
        instance: &wgpu::Instance,
        backend: NativeBackend,
        device_id_filter: Option<u32>,
        surface: &wgpu::Surface<'_>,
        compositor_gpu: Option<&CompositorGpuHint>,
        adapter_policy: SoftwareAdapterPolicy,
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<SelectedAdapter> {
        let mut adapters: Vec<_> = instance.enumerate_adapters(backend.into()).await;

        if adapters.is_empty() {
            anyhow::bail!("No GPU adapters found");
        }

        if let Some(device_id) = device_id_filter {
            log::info!("ZED_DEVICE_ID filter: {:#06x}", device_id);
        }

        // Sort adapters into a single priority order. Tiers (from highest to lowest):
        //
        // 1. ZED_DEVICE_ID match — explicit user override
        // 2. Compositor GPU match — the GPU the display server is rendering on
        // 3. Device type (Discrete > Integrated > Other > Virtual > Cpu).
        //    "Other" ranks above "Virtual" because OpenGL seems to count as "Other".
        // 4. Backend — prefer Vulkan/Metal/Dx12 over GL/etc.
        adapters.sort_by_key(|adapter| {
            let info = adapter.get_info();

            // Backends like OpenGL report device=0 for all adapters, so
            // device-based matching is only meaningful when non-zero.
            let device_known = info.device != 0;

            let user_override: u8 = match device_id_filter {
                Some(id) if device_known && info.device == id => 0,
                _ => 1,
            };

            let compositor_match: u8 = match compositor_gpu {
                Some(hint)
                    if device_known
                        && info.vendor == hint.vendor_id
                        && info.device == hint.device_id =>
                {
                    0
                }
                _ => 1,
            };

            #[cfg(target_vendor = "apple")]
            let type_priority: u8 = match info.device_type {
                // Preserves the native renderer's low-power preference on Intel Macs.
                wgpu::DeviceType::IntegratedGpu => 0,
                wgpu::DeviceType::DiscreteGpu => 1,
                wgpu::DeviceType::Other => 2,
                wgpu::DeviceType::VirtualGpu => 3,
                wgpu::DeviceType::Cpu => 4,
            };
            #[cfg(not(target_vendor = "apple"))]
            let type_priority: u8 = if info.device_type == wgpu::DeviceType::Cpu {
                4
            } else {
                match info.device_type {
                    wgpu::DeviceType::DiscreteGpu => 0,
                    wgpu::DeviceType::IntegratedGpu => 1,
                    wgpu::DeviceType::Other => 2,
                    wgpu::DeviceType::VirtualGpu => 3,
                    wgpu::DeviceType::Cpu => 4,
                }
            };

            let backend_priority: u8 = match info.backend {
                wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12 => 0,
                _ => 1,
            };

            (
                user_override,
                compositor_match,
                type_priority,
                backend_priority,
            )
        });

        // Log all available adapters (in sorted order)
        log::info!("Found {} GPU adapter(s):", adapters.len());
        for adapter in &adapters {
            let info = adapter.get_info();
            log::info!(
                "  - {} (vendor={:#06x}, device={:#06x}, backend={:?}, type={:?})",
                info.name,
                info.vendor,
                info.device,
                info.backend,
                info.device_type,
            );
        }

        // Test each adapter by creating a device and configuring the surface
        for adapter in adapters {
            let info = adapter.get_info();

            if !adapter_policy.accepts(info.device_type) {
                log::info!(
                    "Skipping software renderer: {} ({:?})",
                    info.name,
                    info.backend
                );
                continue;
            }

            log::info!("Testing adapter: {} ({:?})...", info.name, info.backend);

            match Self::try_adapter_with_surface(&adapter, surface, extra_requirements).await {
                Ok(device) => {
                    log::info!(
                        "Adapter passed surface configuration test: {} ({:?})",
                        info.name,
                        info.backend
                    );
                    return Ok(SelectedAdapter { adapter, device });
                }
                Err(e) => {
                    log::info!(
                        "  Adapter {} ({:?}) failed: {}, trying next...",
                        info.name,
                        info.backend,
                        e
                    );
                }
            }
        }

        anyhow::bail!("No GPU adapter found that can configure the display surface")
    }

    /// Try to use an adapter with a surface by creating a device and testing configuration.
    /// Returns the device and queue if successful, allowing them to be reused.
    #[cfg(not(target_family = "wasm"))]
    async fn try_adapter_with_surface(
        adapter: &wgpu::Adapter,
        surface: &wgpu::Surface<'_>,
        extra_requirements: Option<&WgpuDeviceRequirements>,
    ) -> anyhow::Result<CreatedDevice> {
        let caps = surface.get_capabilities(adapter);
        if caps.formats.is_empty() {
            anyhow::bail!("no compatible surface formats");
        }
        if caps.alpha_modes.is_empty() {
            anyhow::bail!("no compatible alpha modes");
        }

        let device = Self::create_device(adapter, extra_requirements).await?;
        let error_scope = device
            .device
            .push_error_scope(wgpu::ErrorFilter::Validation);

        let test_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: 64,
            height: 64,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device.device, &test_config);

        let error = error_scope.pop().await;
        if let Some(e) = error {
            anyhow::bail!("surface configuration failed: {e}");
        }

        Ok(device)
    }

    fn select_color_texture_format(adapter: &wgpu::Adapter) -> anyhow::Result<wgpu::TextureFormat> {
        let required_usages = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let bgra_features = adapter.get_texture_format_features(wgpu::TextureFormat::Bgra8Unorm);
        let rgba_features = adapter.get_texture_format_features(wgpu::TextureFormat::Rgba8Unorm);
        #[cfg(target_family = "wasm")]
        if adapter.get_info().backend == wgpu::Backend::Gl
            && rgba_features.allowed_usages.contains(required_usages)
        {
            return Ok(wgpu::TextureFormat::Rgba8Unorm);
        }
        if bgra_features.allowed_usages.contains(required_usages) {
            return Ok(wgpu::TextureFormat::Bgra8Unorm);
        }
        if rgba_features.allowed_usages.contains(required_usages) {
            let info = adapter.get_info();
            log::warn!(
                "Adapter {} ({:?}) does not support Bgra8Unorm atlas textures with usages {:?}; \
                 falling back to Rgba8Unorm atlas textures.",
                info.name,
                info.backend,
                required_usages,
            );
            return Ok(wgpu::TextureFormat::Rgba8Unorm);
        }

        let info = adapter.get_info();
        Err(anyhow::anyhow!(
            "Adapter {} ({:?}, device={:#06x}) does not support a usable color atlas texture \
             format with usages {:?}. Bgra8Unorm allowed usages: {:?}; \
             Rgba8Unorm allowed usages: {:?}.",
            info.name,
            info.backend,
            info.device,
            required_usages,
            bgra_features.allowed_usages,
            rgba_features.allowed_usages,
        ))
    }
    pub fn backend(&self) -> WgpuBackend {
        self.backend
    }

    pub fn uses_webgl_instance_data(&self) -> bool {
        matches!(self.backend, WgpuBackend::Gl) && cfg!(target_family = "wasm")
    }

    pub fn supports_dual_source_blending(&self) -> bool {
        self.dual_source_blending
    }

    pub fn color_texture_format(&self) -> wgpu::TextureFormat {
        self.color_texture_format
    }

    pub fn renderer_tier(&self) -> RendererTier {
        self.renderer_tier
    }

    /// Returns true if the GPU device was lost (driver crash, suspend/resume).
    /// When this returns true, the context should be recreated.
    pub fn device_lost(&self) -> bool {
        self.errors.device_lost()
    }

    /// Returns a clone of the device_lost flag for sharing with renderers.
    pub(crate) fn device_lost_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.errors.lost)
    }

    /// This device with the WebGL2 instance transport, to test it on any adapter.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn with_webgl2_transport(&self) -> Self {
        Self {
            renderer_tier: RendererTier::WebGl2,
            dual_source_blending: false,
            ..self.clone()
        }
    }

    /// Returns the device-wide error state, shared by all renderers using this device.
    pub fn errors(&self) -> &Arc<DeviceErrorState> {
        &self.errors
    }
}

/// Errors reported by wgpu's device-wide callbacks.
///
/// A device has exactly one lost callback and one uncaptured-error callback, so this
/// state is installed once per device and shared by every renderer using it.
#[derive(Default)]
pub struct DeviceErrorState {
    lost: Arc<AtomicBool>,
    last_error: Mutex<DeviceErrorData>,
}

#[derive(Default)]
struct DeviceErrorData {
    generation: u64,
    latest: Option<String>,
}

impl DeviceErrorState {
    fn install(device: &wgpu::Device) -> Arc<Self> {
        let errors = Arc::new(Self::default());
        device.set_device_lost_callback({
            let errors = Arc::clone(&errors);
            move |reason, message| {
                log::error!("wgpu device lost: reason={reason:?}, message={message}");
                if reason != wgpu::DeviceLostReason::Destroyed {
                    errors.lost.store(true, Ordering::Relaxed);
                }
            }
        });
        device.on_uncaptured_error(Arc::new({
            let errors = Arc::clone(&errors);
            move |error| errors.record(error.to_string())
        }));
        errors
    }

    /// Returns true if the GPU device was lost (e.g., due to driver crash, suspend/resume).
    pub fn device_lost(&self) -> bool {
        self.lost.load(Ordering::Relaxed)
    }

    fn record(&self, error: String) {
        let mut state = self.last_error.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        state.latest = Some(error);
    }

    /// Returns the latest error once per observer, without consuming shared state.
    /// Start each observer at zero, including after switching devices, so errors
    /// raised during construction are observed. Multiple errors between observations
    /// coalesce to the latest error; this is not an error queue.
    pub fn observe_error(&self, generation: &mut u64) -> Option<String> {
        let state = self.last_error.lock().unwrap();
        if *generation == state.generation {
            return None;
        }
        *generation = state.generation;
        state.latest.clone()
    }

    /// The generation an observer should start from to see only errors raised after
    /// this call, for a renderer joining a device other renderers already used.
    pub fn current_generation(&self) -> u64 {
        self.last_error.lock().unwrap().generation
    }
}

fn renderer_tier(adapter: &wgpu::Adapter) -> RendererTier {
    renderer_tier_for(
        &adapter.limits(),
        adapter.get_downlevel_capabilities().flags,
    )
}

fn renderer_tier_for(limits: &wgpu::Limits, flags: wgpu::DownlevelFlags) -> RendererTier {
    let modern_storage_flags = wgpu::DownlevelFlags::VERTEX_STORAGE
        | wgpu::DownlevelFlags::FRAGMENT_STORAGE
        | wgpu::DownlevelFlags::BUFFER_BINDINGS_NOT_16_BYTE_ALIGNED;
    if limits.max_storage_buffers_per_shader_stage > 0 && flags.contains(modern_storage_flags) {
        RendererTier::Modern
    } else {
        // The generated storage-buffer ABI includes 248-byte Quad records. Devices that
        // require 16-byte-multiple binding sizes must use the packed data-texture transport.
        RendererTier::WebGl2
    }
}

#[cfg(not(target_family = "wasm"))]
fn parse_pci_id(id: &str) -> anyhow::Result<u32> {
    let mut id = id.trim();

    if id.starts_with("0x") || id.starts_with("0X") {
        id = &id[2..];
    }
    let is_hex_string = id.chars().all(|c| c.is_ascii_hexdigit());
    let is_4_chars = id.len() == 4;
    anyhow::ensure!(
        is_4_chars && is_hex_string,
        "Expected a 4 digit PCI ID in hexadecimal format"
    );

    u32::from_str_radix(id, 16).context("parsing PCI ID as hex")
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::{
        NativeBackend, RendererTier, SoftwareAdapterPolicy, parse_pci_id, renderer_tier_for,
    };

    #[test]
    fn device_errors_are_observed_independently_and_coalesce_to_latest() {
        let errors = super::DeviceErrorState::default();
        let mut first_generation = 0;
        let mut second_generation = 0;

        errors.record("first".to_owned());
        assert_eq!(
            errors.observe_error(&mut first_generation).as_deref(),
            Some("first")
        );
        assert_eq!(
            errors.observe_error(&mut second_generation).as_deref(),
            Some("first")
        );
        assert_eq!(errors.observe_error(&mut first_generation), None);
        assert_eq!(errors.observe_error(&mut second_generation), None);

        errors.record("intermediate".to_owned());
        errors.record("latest".to_owned());
        assert_eq!(errors.current_generation(), 3);
        assert_eq!(
            errors.observe_error(&mut first_generation).as_deref(),
            Some("latest")
        );
        assert_eq!(
            errors.observe_error(&mut second_generation).as_deref(),
            Some("latest")
        );
        assert_eq!(first_generation, 3);
        assert_eq!(second_generation, 3);
    }

    #[test]
    fn a_new_observer_can_join_after_existing_errors() {
        let errors = super::DeviceErrorState::default();
        errors.record("construction".to_owned());

        let mut catch_up = 0;
        assert_eq!(
            errors.observe_error(&mut catch_up).as_deref(),
            Some("construction")
        );

        let mut after_join = errors.current_generation();
        assert_eq!(errors.observe_error(&mut after_join), None);
        errors.record("after join".to_owned());
        assert_eq!(
            errors.observe_error(&mut after_join).as_deref(),
            Some("after join")
        );
    }

    #[test]
    fn replacement_device_uses_a_fresh_error_generation() {
        let old_device = super::DeviceErrorState::default();
        let mut renderer_generation = 0;
        old_device.record("old device error".to_owned());
        assert_eq!(
            old_device
                .observe_error(&mut renderer_generation)
                .as_deref(),
            Some("old device error")
        );
        assert_eq!(renderer_generation, 1);

        let recovered_device = super::DeviceErrorState::default();
        let mut stale_generation = renderer_generation;
        let mut recovered_renderer_generation = 0;
        recovered_device.record("recovery construction error".to_owned());
        assert_eq!(
            recovered_device.observe_error(&mut stale_generation),
            None,
            "reusing the old generation would miss the first error on the replacement device"
        );
        assert_eq!(
            recovered_device
                .observe_error(&mut recovered_renderer_generation)
                .as_deref(),
            Some("recovery construction error")
        );
        assert_eq!(recovered_renderer_generation, 1);
    }

    #[test]
    fn storage_tier_requires_unaligned_binding_size_support() {
        let mut limits = wgpu::Limits::default();
        limits.max_storage_buffers_per_shader_stage = 1;
        let storage_flags =
            wgpu::DownlevelFlags::VERTEX_STORAGE | wgpu::DownlevelFlags::FRAGMENT_STORAGE;

        assert_eq!(
            renderer_tier_for(&limits, storage_flags),
            RendererTier::WebGl2,
            "storage support alone cannot bind the 248-byte Quad record on a 16-byte-aligned device"
        );
        assert_eq!(
            renderer_tier_for(
                &limits,
                storage_flags | wgpu::DownlevelFlags::BUFFER_BINDINGS_NOT_16_BYTE_ALIGNED,
            ),
            RendererTier::Modern,
        );
    }

    #[test]
    fn native_backend_fallbacks_are_individually_initialized() {
        let backends = NativeBackend::PREFERENCE;
        assert!(!backends.is_empty());
        assert!(
            backends
                .iter()
                .all(|backend| wgpu::Backends::from(*backend).bits().count_ones() == 1),
            "each fallback step must initialize exactly one WGPU backend"
        );

        #[cfg(target_os = "linux")]
        assert_eq!(backends, &[NativeBackend::Vulkan, NativeBackend::Gl]);
        #[cfg(target_os = "windows")]
        assert_eq!(
            backends,
            &[
                NativeBackend::Dx12,
                NativeBackend::Vulkan,
                NativeBackend::Gl,
            ]
        );
        #[cfg(target_os = "macos")]
        assert_eq!(backends, &[NativeBackend::Metal]);
    }

    #[test]
    fn native_backend_fallback_preserves_ordered_failures() {
        let mut attempted = Vec::new();
        let error = NativeBackend::try_in_preference_order::<()>("test context", |backend| {
            attempted.push(backend);
            anyhow::bail!("unavailable")
        })
        .unwrap_err();

        assert_eq!(attempted, NativeBackend::PREFERENCE);
        let message = format!("{error:#}");
        let mut previous = 0;
        for backend in NativeBackend::PREFERENCE {
            let index = message[previous..]
                .find(&format!("{backend:?}: unavailable"))
                .expect("each backend failure should be reported in preference order");
            previous += index;
        }
    }

    #[test]
    fn software_adapter_policy_is_explicit() {
        assert!(SoftwareAdapterPolicy::Allow.accepts(wgpu::DeviceType::Cpu));
        assert!(!SoftwareAdapterPolicy::Reject.accepts(wgpu::DeviceType::Cpu));
        assert!(
            SoftwareAdapterPolicy::Reject.accepts(wgpu::DeviceType::IntegratedGpu),
            "rejecting software must not reject lower-power hardware adapters"
        );
    }

    #[test]
    fn test_parse_device_id() {
        assert!(parse_pci_id("0xABCD").is_ok());
        assert!(parse_pci_id("ABCD").is_ok());
        assert!(parse_pci_id("abcd").is_ok());
        assert!(parse_pci_id("1234").is_ok());
        assert!(parse_pci_id("123").is_err());
        assert_eq!(
            parse_pci_id(&format!("{:x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:X}", 0x1234)).unwrap(),
        );

        assert_eq!(
            parse_pci_id(&format!("{:#x}", 0x1234)).unwrap(),
            parse_pci_id(&format!("{:#X}", 0x1234)).unwrap(),
        );
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod web_backend_options_tests {
    use super::{WebBackendPreference, web_backend_options};

    #[test]
    fn browser_gl_uses_nonblocking_fence_workaround_but_webgpu_does_not() {
        for preference in [WebBackendPreference::Auto, WebBackendPreference::WebGl] {
            assert!(
                web_backend_options(preference)
                    .gl
                    .fence_behavior
                    .is_auto_finish()
            );
        }
        assert!(
            !web_backend_options(WebBackendPreference::WebGpu)
                .gl
                .fence_behavior
                .is_auto_finish()
        );
    }
}

#[cfg(not(target_family = "wasm"))]
impl WgpuContext {
    /// Picks an adapter like `select_adapter_and_device` does, without a surface to test:
    /// callers check the surface and fall back to the full selection if it does not fit.
    pub(crate) fn create_without_surface(
        compositor_gpu: Option<&CompositorGpuHint>,
    ) -> anyhow::Result<WgpuContext> {
        anyhow::ensure!(
            std::env::var_os("ZED_DEVICE_ID").is_none(),
            "ZED_DEVICE_ID needs the full adapter selection"
        );
        let instance = NativeBackend::Vulkan.instance(None);
        let mut adapters =
            gpui::block_on(instance.raw.enumerate_adapters(wgpu::Backends::VULKAN));
        // Software renderers are left to the full selection, which knows when to allow them.
        adapters.retain(|adapter| adapter.get_info().device_type != wgpu::DeviceType::Cpu);
        adapters.sort_by_key(|adapter| {
            let info = adapter.get_info();
            let compositor_match = compositor_gpu.is_some_and(|hint| {
                info.device != 0 && info.vendor == hint.vendor_id && info.device == hint.device_id
            });
            let type_priority = match info.device_type {
                wgpu::DeviceType::DiscreteGpu => 0,
                wgpu::DeviceType::IntegratedGpu => 1,
                wgpu::DeviceType::Other => 2,
                wgpu::DeviceType::VirtualGpu => 3,
                wgpu::DeviceType::Cpu => 4,
            };
            (!compositor_match, type_priority)
        });
        let adapter = adapters.into_iter().next().context("no GPU adapter")?;
        let device = gpui::block_on(Self::create_device(&adapter, None))?;
        Self::from_created_device(instance.raw, adapter, device)
    }

    /// Whether this context runs on the GPU the compositor uses, when that is known.
    pub(crate) fn matches_compositor(&self, compositor_gpu: Option<&CompositorGpuHint>) -> bool {
        let Some(hint) = compositor_gpu else {
            return true;
        };
        let info = self.adapter.get_info();
        info.vendor == hint.vendor_id && info.device == hint.device_id
    }
}
