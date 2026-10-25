#[cfg(not(target_family = "wasm"))]
mod cpu_renderer;
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;

#[cfg(not(target_family = "wasm"))]
pub use cpu_renderer::{CpuFrame, render_scene};
pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
#[cfg(not(target_family = "wasm"))]
pub use wgpu_renderer::PendingRenderer;
#[cfg(all(feature = "test-support", not(target_family = "wasm")))]
pub use wgpu_renderer::WgpuHeadlessRenderer;
pub use wgpu_renderer::{
    FontRasterizationSettings, GpuContext, SubpixelOrder, WgpuRenderer, WgpuSurfaceConfig,
};
