//! Draws a scene on the CPU, for the first frames of a window while its GPU device is still
//! being created. It runs the renderer's own shader code (compiled for the host) and blends
//! into an 8-bit BGRA image the way the GPU pipelines do, so the GPU's first frame looks the
//! same. Scenes with primitives it does not draw make it return `None`; the window then
//! waits for the GPU.

use gpui::{
    AtlasTextureId, Background, Bounds, BorderStyle, PrimitiveBatch, RenderCommand, Scene,
    ScaledPixels, SceneHsla, TransformationMatrix,
};
use gpui_render::shaders::common::{
    self as sc, FONT_RASTERIZATION, FontRasterizationUniforms, GLOBALS, GlobalUniforms,
    ShaderBool,
};
use gpui_render::shaders::{quad, underline};
use wgsl_rs::std::{RuntimeArray, Vec2f, Vec4f, dot, vec2f, vec3f, vec4f};

use crate::{FontRasterizationSettings, SubpixelOrder, WgpuAtlas};

/// An 8-bit BGRA image, the CPU counterpart of the swapchain texture.
pub struct CpuFrame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Draws `scene` into a `width` x `height` image, or returns `None` if it contains
/// something the CPU renderer does not draw.
pub fn render_scene(
    scene: &Scene,
    atlas: &WgpuAtlas,
    width: usize,
    height: usize,
    font: FontRasterizationSettings,
) -> Option<CpuFrame> {
    if width == 0 || height == 0 {
        return None;
    }
    // Checked first, so an unsupported scene costs nothing.
    for command in scene.render_commands() {
        match command {
            RenderCommand::Batch(PrimitiveBatch::Quads { .. })
            | RenderCommand::Batch(PrimitiveBatch::Underlines(_))
            | RenderCommand::Batch(PrimitiveBatch::MonochromeSprites { .. })
            | RenderCommand::Batch(PrimitiveBatch::SubpixelSprites { .. }) => {}
            RenderCommand::Batch(PrimitiveBatch::PolychromeSprites { range, .. }) => {
                if scene.polychrome_sprites[range.clone()]
                    .iter()
                    .any(|sprite| sprite.corner_smoothing > 0.0)
                {
                    return None;
                }
            }
            _ => return None,
        }
    }

    // The shaders' uniforms and storage buffers are statics on the host: one scene at a time.
    static RENDERING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _rendering = RENDERING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    GLOBALS.set(GlobalUniforms {
        viewport_size: vec2f(width as f32, height as f32),
        premultiplied_alpha: ShaderBool::Disabled,
        padding: 0,
    });
    FONT_RASTERIZATION.set(FontRasterizationUniforms {
        gamma_ratios: vec4f(
            font.gamma_ratios[0],
            font.gamma_ratios[1],
            font.gamma_ratios[2],
            font.gamma_ratios[3],
        ),
        grayscale_enhanced_contrast: font.grayscale_enhanced_contrast,
        subpixel_enhanced_contrast: font.subpixel_enhanced_contrast,
        uses_blue_green_red_subpixel_order: ShaderBool::from(
            font.subpixel_order == SubpixelOrder::BlueGreenRed,
        ),
        padding: 0,
    });

    // The GPU clears to transparent black.
    let gamma = FONT_RASTERIZATION.get().gamma_ratios;
    let mut frame = CpuFrame {
        width,
        height,
        pixels: vec![0; width * height * 4],
    };
    for command in scene.render_commands() {
        let RenderCommand::Batch(batch) = command else {
            continue;
        };
        match batch {
            PrimitiveBatch::Quads { range, smoothed } => {
                for quad in &scene.quads[range.clone()] {
                    draw_quad(&mut frame, quad, *smoothed);
                }
            }
            PrimitiveBatch::Underlines(range) => {
                for line in &scene.underlines[range.clone()] {
                    draw_underline(&mut frame, line);
                }
            }
            PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                for sprite in &scene.monochrome_sprites[range.clone()] {
                    if !is_identity(&sprite.transformation) {
                        return None;
                    }
                    let color = sc::hsla_to_rgba(hsla(sprite.color));
                    let mask = sc::ContentMask::from(sprite.content_mask.clone());
                    let shade = |pos: Vec2f, texel: [f32; 4]| {
                        let corrected = sc::apply_contrast_and_gamma_correction(
                            texel[0],
                            color.rgb(),
                            font.grayscale_enhanced_contrast,
                            gamma,
                        );
                        let alpha = corrected * sc::ContentMask::alpha(mask, pos);
                        Blend::Over(sc::blend_color(color, alpha))
                    };
                    let tile = &sprite.tile;
                    let area = (sprite.bounds, sprite.content_mask.bounds);
                    draw_sprite(&mut frame, atlas, *texture_id, area, tile, shade)?;
                }
            }
            PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                let bgr = font.subpixel_order == SubpixelOrder::BlueGreenRed;
                for sprite in &scene.subpixel_sprites[range.clone()] {
                    if !is_identity(&sprite.transformation) {
                        return None;
                    }
                    let color = sc::hsla_to_rgba(hsla(sprite.color));
                    let mask = sc::ContentMask::from(sprite.content_mask.clone());
                    let shade = |pos: Vec2f, texel: [f32; 4]| {
                        // Texels are BGRA; `texel` holds them as RGBA.
                        let sample = if bgr {
                            vec3f(texel[2], texel[1], texel[0])
                        } else {
                            vec3f(texel[0], texel[1], texel[2])
                        };
                        let corrected = sc::apply_contrast_and_gamma_correction3(
                            sample,
                            color.rgb(),
                            font.subpixel_enhanced_contrast,
                            gamma,
                        );
                        let mask_alpha = sc::ContentMask::alpha(mask, pos);
                        Blend::Subpixel {
                            color: [color.x, color.y, color.z],
                            alpha: [
                                color.w * corrected.x * mask_alpha,
                                color.w * corrected.y * mask_alpha,
                                color.w * corrected.z * mask_alpha,
                            ],
                        }
                    };
                    let tile = &sprite.tile;
                    let area = (sprite.bounds, sprite.content_mask.bounds);
                    draw_sprite(&mut frame, atlas, *texture_id, area, tile, shade)?;
                }
            }
            PrimitiveBatch::PolychromeSprites { texture_id, range, .. } => {
                for sprite in &scene.polychrome_sprites[range.clone()] {
                    let bounds = sc::Bounds::from(sprite.bounds);
                    let radii = sc::Corners::from(sprite.corner_radii);
                    let mask = sc::ContentMask::from(sprite.content_mask.clone());
                    let grayscale = sprite.grayscale == gpui::ShaderBool::Enabled;
                    let opacity = sprite.opacity;
                    let shade = |pos: Vec2f, texel: [f32; 4]| {
                        let mut color = vec4f(texel[0], texel[1], texel[2], texel[3]);
                        if grayscale {
                            let gray = dot(color.rgb(), sc::LINEAR_RGB_LUMA_WEIGHTS);
                            color = vec4f(gray, gray, gray, color.w);
                        }
                        Blend::Over(sc::blend_color(
                            color,
                            opacity
                                * sc::antialiased_coverage(sc::rounded_rectangle_signed_distance(
                                    pos, bounds, radii,
                                ))
                                * sc::ContentMask::alpha(mask, pos),
                        ))
                    };
                    let tile = &sprite.tile;
                    let area = (sprite.bounds, sprite.content_mask.bounds);
                    draw_sprite(&mut frame, atlas, *texture_id, area, tile, shade)?;
                }
            }
            _ => return None,
        }
    }
    // An opaque window: the compositor must not see through it.
    for pixel in frame.pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Some(frame)
}

fn hsla(color: SceneHsla) -> sc::Hsla {
    sc::Hsla {
        h: color.h,
        s: color.s,
        l: color.l,
        a: color.a,
    }
}

fn background(background: &Background) -> sc::Background {
    let stop = |stop: &gpui::LinearColorStop| sc::LinearColorStop {
        color: hsla(stop.color),
        percentage: stop.percentage,
    };
    sc::Background {
        tag: match background.tag as u32 {
            0 => sc::BackgroundTag::Solid,
            1 => sc::BackgroundTag::LinearGradient,
            2 => sc::BackgroundTag::PatternSlash,
            _ => sc::BackgroundTag::Checkerboard,
        },
        color_space: match background.color_space as u32 {
            0 => sc::ColorSpace::Srgb,
            _ => sc::ColorSpace::Oklab,
        },
        solid: hsla(background.solid),
        gradient_angle_or_pattern_height: background.gradient_angle_or_pattern_height,
        colors: [stop(&background.colors[0]), stop(&background.colors[1])],
        padding: 0,
    }
}

fn is_identity(transformation: &TransformationMatrix) -> bool {
    transformation.rotation_scale == [[1.0, 0.0], [0.0, 1.0]]
        && transformation.translation == [0.0, 0.0]
}

/// The pixels whose centers a rectangle covers, like the GPU's rasterizer: `start..end`.
fn covered(start: f32, end: f32, limit: usize) -> (usize, usize) {
    let first = (start - 0.5).ceil().max(0.0) as usize;
    let last = ((end - 0.5).ceil().max(0.0) as usize).min(limit);
    (first.min(last), last)
}

/// The columns and rows of `frame` whose pixel centers `bounds` covers.
fn covered_pixels(bounds: Bounds<ScaledPixels>, frame: &CpuFrame) -> [(usize, usize); 2] {
    let (x, y) = (bounds.origin.x.0, bounds.origin.y.0);
    [
        covered(x, x + bounds.size.width.0, frame.width),
        covered(y, y + bounds.size.height.0, frame.height),
    ]
}

enum Blend {
    /// Source-over with non-premultiplied source color.
    Over(Vec4f),
    /// The dual-source blend of subpixel glyphs: a per-channel alpha.
    Subpixel { color: [f32; 3], alpha: [f32; 3] },
}

fn unorm(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn blend_pixel(pixel: &mut [u8], blend: &Blend) {
    let dst = [0, 1, 2, 3].map(|i| pixel[i] as f32 / 255.0);
    let dst = |i: usize| dst[i];
    match blend {
        Blend::Over(src) => {
            let a = src.w;
            if a <= 0.0 {
                return;
            }
            // Pixels are BGRA.
            let (r, g, b, da) = (dst(2), dst(1), dst(0), dst(3));
            pixel[2] = unorm(src.x * a + r * (1.0 - a));
            pixel[1] = unorm(src.y * a + g * (1.0 - a));
            pixel[0] = unorm(src.z * a + b * (1.0 - a));
            pixel[3] = unorm(a + da * (1.0 - a));
        }
        Blend::Subpixel { color, alpha } => {
            // Color: src * src1 + dst * (1 - src1); alpha is not written.
            for (channel, index) in [(0, 2), (1, 1), (2, 0)] {
                let d = dst(index);
                pixel[index] = unorm(color[channel] * alpha[channel] + d * (1.0 - alpha[channel]));
            }
        }
    }
}

/// Draws an axis-aligned sprite from an atlas tile, clipped to `clip`; `shade` gets the pixel
/// center and the sampled texel as RGBA in 0..1 and returns how to blend it.
fn draw_sprite(
    frame: &mut CpuFrame,
    atlas: &WgpuAtlas,
    texture_id: AtlasTextureId,
    (bounds, clip): (Bounds<ScaledPixels>, Bounds<ScaledPixels>),
    tile: &gpui::AtlasTile,
    mut shade: impl FnMut(Vec2f, [f32; 4]) -> Blend,
) -> Option<()> {
    let [(x0, x1), (y0, y1)] = covered_pixels(bounds, frame);
    if x0 >= x1 || y0 >= y1 {
        return Some(());
    }
    let width = frame.width;
    let clip = sc::Bounds::from(clip);
    atlas.with_cpu_texture(texture_id, |texels, stride, bpp| {
        let tex_height = texels.len() / (stride * bpp);
        let tile_x = tile.bounds.origin.x.0 as f32;
        let tile_y = tile.bounds.origin.y.0 as f32;
        let scale_x = tile.bounds.size.width.0 as f32 / bounds.size.width.0;
        let scale_y = tile.bounds.size.height.0 as f32 / bounds.size.height.0;
        let fetch = |x: i64, y: i64| -> [f32; 4] {
            let x = x.clamp(0, stride as i64 - 1) as usize;
            let y = y.clamp(0, tex_height as i64 - 1) as usize;
            let at = (y * stride + x) * bpp;
            if bpp == 1 {
                let v = texels[at] as f32 / 255.0;
                [v, 0.0, 0.0, 1.0]
            } else {
                [
                    texels[at + 2] as f32 / 255.0,
                    texels[at + 1] as f32 / 255.0,
                    texels[at] as f32 / 255.0,
                    texels[at + 3] as f32 / 255.0,
                ]
            }
        };
        for y in y0..y1 {
            let py = y as f32 + 0.5;
            // Texel space, where texel centers sit at integers (linear filtering).
            let ty = tile_y + (py - bounds.origin.y.0) * scale_y - 0.5;
            let (ty0, fy) = (ty.floor(), ty - ty.floor());
            for x in x0..x1 {
                let px = x as f32 + 0.5;
                if sc::is_clipped(sc::clip_distances(vec2f(px, py), clip)) {
                    continue;
                }
                let tx = tile_x + (px - bounds.origin.x.0) * scale_x - 0.5;
                let (tx0, fx) = (tx.floor(), tx - tx.floor());
                let texel = if fx == 0.0 && fy == 0.0 {
                    fetch(tx0 as i64, ty0 as i64)
                } else {
                    let a = fetch(tx0 as i64, ty0 as i64);
                    let b = fetch(tx0 as i64 + 1, ty0 as i64);
                    let c = fetch(tx0 as i64, ty0 as i64 + 1);
                    let d = fetch(tx0 as i64 + 1, ty0 as i64 + 1);
                    std::array::from_fn(|i| {
                        let top = a[i] + (b[i] - a[i]) * fx;
                        let bottom = c[i] + (d[i] - c[i]) * fx;
                        top + (bottom - top) * fy
                    })
                };
                let blend = shade(vec2f(px, py), texel);
                let at = (y * width + x) * 4;
                blend_pixel(&mut frame.pixels[at..at + 4], &blend);
            }
        }
    })
}

/// Pixels of a run within which a quad's color does not change horizontally.
struct Span {
    start: usize,
    end: usize,
    /// Evaluate every pixel, near an edge or corner.
    varies: bool,
}

fn draw_quad(frame: &mut CpuFrame, scene_quad: &gpui::Quad, smoothed: bool) {
    let shader_quad = quad::Quad {
        order: scene_quad.order,
        border_style: match scene_quad.border_style {
            BorderStyle::Solid => sc::BorderStyle::Solid,
            BorderStyle::Dashed => sc::BorderStyle::Dashed,
        },
        border_dashed_length: scene_quad.border_dashed_length,
        border_dashed_gap: scene_quad.border_dashed_gap,
        bounds: scene_quad.bounds.into(),
        content_mask: scene_quad.content_mask.clone().into(),
        background: background(&scene_quad.background),
        border_color: background(&scene_quad.border_color),
        corner_radii: scene_quad.corner_radii.into(),
        border_widths: scene_quad.border_widths.into(),
        corner_smoothing: scene_quad.corner_smoothing,
        padding: 0,
    };
    quad::QUADS.set(RuntimeArray {
        data: vec![shader_quad],
    });

    let bounds = scene_quad.bounds;
    let mask = &scene_quad.content_mask;
    let left = bounds.origin.x.0.max(mask.bounds.origin.x.0);
    let top = bounds.origin.y.0.max(mask.bounds.origin.y.0);
    let right = (bounds.origin.x.0 + bounds.size.width.0)
        .min(mask.bounds.origin.x.0 + mask.bounds.size.width.0);
    let bottom = (bounds.origin.y.0 + bounds.size.height.0)
        .min(mask.bounds.origin.y.0 + mask.bounds.size.height.0);
    let [(x0, x1), (y0, y1)] = covered_pixels(bounds, frame);
    let (x0, x1) = (x0.max(left.floor().max(0.0) as usize), x1.min(right.ceil().max(0.0) as usize));
    let (y0, y1) = (y0.max(top.floor().max(0.0) as usize), y1.min(bottom.ceil().max(0.0) as usize));
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let flat = quad::vertex_quad(0, 0);
    let smoothed_flat = smoothed.then(|| quad::vertex_smoothed_quad(0, 0));
    let shade = |x: usize, y: usize| -> Vec4f {
        let position = vec4f(x as f32 + 0.5, y as f32 + 0.5, 0.0, 1.0);
        let clip_distances = sc::clip_distances(position.xy(), shader_quad.content_mask.bounds);
        match &smoothed_flat {
            Some(v) => quad::fragment_smoothed_quad(quad::SmoothedQuadVarying {
                position,
                clip_distances,
                border_solid: v.border_solid,
                quad_id: 0,
                fill_solid: v.fill_solid,
                fill_color0: v.fill_color0,
                fill_color1: v.fill_color1,
                horizontal_corner_reaches: v.horizontal_corner_reaches,
                vertical_corner_reaches: v.vertical_corner_reaches,
                corner_lengths: v.corner_lengths,
                smoothing_factors: v.smoothing_factors,
                superellipse_power: v.superellipse_power,
                border_color0: v.border_color0,
                border_color1: v.border_color1,
            }),
            None => quad::fragment_quad(quad::QuadVarying {
                position,
                clip_distances,
                border_solid: flat.border_solid,
                quad_id: 0,
                fill_solid: flat.fill_solid,
                fill_color0: flat.fill_color0,
                fill_color1: flat.fill_color1,
                border_color0: flat.border_color0,
                border_color1: flat.border_color1,
            }),
        }
    };

    // A solid quad's color depends on a pixel's distance to the nearest edges only, so away
    // from its edges and corners (and those of its mask) a run of pixels shares one color.
    let uniform = scene_quad.background.tag as u32 == 0
        && scene_quad.border_color.tag as u32 == 0
        && scene_quad.border_style == BorderStyle::Solid;
    let corners = &scene_quad.corner_radii;
    let widths = &scene_quad.border_widths;
    let reach = [
        corners.top_left.0,
        corners.top_right.0,
        corners.bottom_right.0,
        corners.bottom_left.0,
        widths.top.0,
        widths.right.0,
        widths.bottom.0,
        widths.left.0,
    ]
    .into_iter()
    .fold(0.0f32, f32::max)
        + 2.0;
    let fade = &mask.fade_out;
    // Where a pixel's color can change along one axis: near the quad's two edges, within
    // `reach` of them, and near the mask's edges, within their fade.
    let edge_zones = |edges: (f32, f32), mask_edges: (f32, f32), fades: (f32, f32)| {
        [
            (edges.0 - 2.0, edges.0 + reach),
            (edges.1 - reach, edges.1 + 2.0),
            (mask_edges.0 - 2.0, mask_edges.0 + fades.0 + 2.0),
            (mask_edges.1 - fades.1 - 2.0, mask_edges.1 + 2.0),
        ]
    };
    let spans = |start: usize, end: usize, zones: [(f32, f32); 4]| -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        for i in start..end {
            let center = i as f32 + 0.5;
            let varies = !uniform || zones.iter().any(|(a, b)| center >= *a && center <= *b);
            match spans.last_mut() {
                Some(span) if span.varies == varies && !varies => span.end = i + 1,
                _ => spans.push(Span { start: i, end: i + 1, varies }),
            }
        }
        spans
    };
    let x_spans = spans(
        x0,
        x1,
        edge_zones(
            (bounds.origin.x.0, bounds.origin.x.0 + bounds.size.width.0),
            (mask.bounds.origin.x.0, mask.bounds.origin.x.0 + mask.bounds.size.width.0),
            (fade.left.0, fade.right.0),
        ),
    );
    let y_spans = spans(
        y0,
        y1,
        edge_zones(
            (bounds.origin.y.0, bounds.origin.y.0 + bounds.size.height.0),
            (mask.bounds.origin.y.0, mask.bounds.origin.y.0 + mask.bounds.size.height.0),
            (fade.top.0, fade.bottom.0),
        ),
    );

    let width = frame.width;
    let mut row = vec![vec4f(0.0, 0.0, 0.0, 0.0); x1 - x0];
    for y_span in &y_spans {
        // One evaluated row serves every row of a uniform run.
        let mut fills = Vec::with_capacity(x_spans.len());
        for x_span in &x_spans {
            if x_span.varies {
                for x in x_span.start..x_span.end {
                    row[x - x0] = shade(x, y_span.start);
                }
                fills.push(None);
            } else {
                fills.push(Some(Fill::new(shade(x_span.start, y_span.start))));
            }
        }
        for y in y_span.start..y_span.end {
            let line = &mut frame.pixels[y * width * 4..(y + 1) * width * 4];
            for (x_span, fill) in x_spans.iter().zip(&fills) {
                let pixels = &mut line[x_span.start * 4..x_span.end * 4];
                match fill {
                    Some(fill) => fill.apply(pixels),
                    None => {
                        for (pixel, color) in pixels
                            .chunks_exact_mut(4)
                            .zip(&row[x_span.start - x0..x_span.end - x0])
                        {
                            blend_pixel(pixel, &Blend::Over(*color));
                        }
                    }
                }
            }
        }
    }
}

/// Source-over blending of one color into many pixels.
enum Fill {
    Skip,
    Opaque([u8; 4]),
    /// The result for every destination value, per BGRA channel.
    Table(Box<[[u8; 256]; 4]>),
}

impl Fill {
    fn new(color: Vec4f) -> Self {
        let a = color.w;
        if a <= 0.0 {
            return Fill::Skip;
        }
        if a >= 1.0 {
            return Fill::Opaque([unorm(color.z), unorm(color.y), unorm(color.x), 255]);
        }
        let source = [color.z * a, color.y * a, color.x * a, a];
        let mut table = Box::new([[0u8; 256]; 4]);
        for (channel, values) in table.iter_mut().enumerate() {
            for (dst, value) in values.iter_mut().enumerate() {
                *value = unorm(source[channel] + dst as f32 / 255.0 * (1.0 - a));
            }
        }
        Fill::Table(table)
    }

    fn apply(&self, pixels: &mut [u8]) {
        match self {
            Fill::Skip => {}
            Fill::Opaque(color) => {
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel.copy_from_slice(color);
                }
            }
            Fill::Table(table) => {
                for pixel in pixels.chunks_exact_mut(4) {
                    for channel in 0..4 {
                        pixel[channel] = table[channel][pixel[channel] as usize];
                    }
                }
            }
        }
    }
}

fn draw_underline(frame: &mut CpuFrame, line: &gpui::Underline) {
    let shader_line = underline::Underline {
        order: line.order,
        padding: 0,
        bounds: line.bounds.into(),
        clip_bounds: line.clip_bounds.into(),
        content_mask: line.content_mask.clone().into(),
        color: hsla(line.color),
        thickness: line.thickness.0,
        wavy: ShaderBool::from(line.wavy == gpui::ShaderBool::Enabled),
    };
    underline::UNDERLINES.set(RuntimeArray {
        data: vec![shader_line],
    });
    let flat = underline::vertex_underline(0, 0);
    let bounds = line.bounds;
    let [(x0, x1), (y0, y1)] = covered_pixels(bounds, frame);
    for y in y0..y1 {
        for x in x0..x1 {
            let position = vec4f(x as f32 + 0.5, y as f32 + 0.5, 0.0, 1.0);
            let color = underline::fragment_underline(underline::UnderlineVarying {
                position,
                color: flat.color,
                underline_id: 0,
                clip_distances: sc::clip_distances(position.xy(), shader_line.clip_bounds),
            });
            let at = (y * frame.width + x) * 4;
            blend_pixel(&mut frame.pixels[at..at + 4], &Blend::Over(color));
        }
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use crate::WgpuHeadlessRenderer;
    use gpui::{
        AtlasKey, AtlasTile, ContentMask, Corners, DevicePixels, Edges, ImageId, MonochromeSprite,
        PlatformAtlas, PlatformHeadlessRenderer, Point, PolychromeSprite, Quad,
        RasterizedGlyphFormat, RenderImageParams, RenderSvgParams, Size, SubpixelSprite,
        Underline, hsla, linear_color_stop, linear_gradient, pattern_slash,
    };
    use std::borrow::Cow;

    const WIDTH: usize = 160;
    const HEIGHT: usize = 120;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: Point {
                x: ScaledPixels(x),
                y: ScaledPixels(y),
            },
            size: Size {
                width: ScaledPixels(width),
                height: ScaledPixels(height),
            },
        }
    }

    fn mask(bounds: Bounds<ScaledPixels>) -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds,
            ..ContentMask::default()
        }
    }

    fn everywhere() -> ContentMask<ScaledPixels> {
        mask(rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32))
    }

    fn color(h: f32, s: f32, l: f32, a: f32) -> SceneHsla {
        hsla(h, s, l, a).into()
    }

    fn tile(atlas: &dyn PlatformAtlas, key: AtlasKey, size: (i32, i32), bpp: usize) -> AtlasTile {
        let pixels = (0..size.0 * size.1)
            .flat_map(|i| {
                let v = ((i * 37) % 256) as u8;
                [v, v.wrapping_mul(3), v.wrapping_add(90), 255 - v][..bpp].to_vec()
            })
            .collect::<Vec<u8>>();
        let size = Size {
            width: DevicePixels(size.0),
            height: DevicePixels(size.1),
        };
        atlas
            .get_or_insert_with(key, &mut || Ok(Some((size, Cow::Owned(pixels.clone())))))
            .unwrap()
            .unwrap()
    }

    /// Renders the scene `build` makes (with its tiles in the given atlas) on the GPU and on
    /// the CPU, and checks that no channel differs by more than one step.
    fn assert_matches_gpu(build: impl Fn(&dyn PlatformAtlas) -> Scene) {
        assert_nearly_matches_gpu(build, 1, 0.0);
    }

    /// Like `assert_matches_gpu`, for shaders whose float math differs between GPU and CPU:
    /// a channel may differ by up to `max_difference` steps, and by more than one step in at
    /// most `share` of all channel values.
    fn assert_nearly_matches_gpu(
        build: impl Fn(&dyn PlatformAtlas) -> Scene,
        max_difference: u8,
        share: f32,
    ) {
        let mut gpu = WgpuHeadlessRenderer::new().expect("a GPU for the reference image");
        let gpu_atlas = gpu.sprite_atlas();
        let size = Size {
            width: DevicePixels(WIDTH as i32),
            height: DevicePixels(HEIGHT as i32),
        };
        let expected = gpu
            .render_scene_to_image(&build(gpu_atlas.as_ref()), size)
            .unwrap();

        let cpu_atlas = WgpuAtlas::new_deferred();
        let font = FontRasterizationSettings::from_environment();
        let frame = render_scene(&build(&cpu_atlas), &cpu_atlas, WIDTH, HEIGHT, font)
            .expect("the CPU renderer draws this scene");

        // Both images as PPM files, to look at a failure.
        if let Some(dir) = std::env::var_os("CPU_RENDERER_TEST_IMAGES") {
            let name = std::thread::current().name().unwrap_or("test").replace("::", "_");
            let header = format!("P6 {WIDTH} {HEIGHT} 255\n").into_bytes();
            let cpu = frame.pixels.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]);
            let gpu = expected.pixels().flat_map(|p| [p.0[0], p.0[1], p.0[2]]);
            for (side, pixels) in [("cpu", cpu.collect::<Vec<_>>()), ("gpu", gpu.collect())] {
                let path = std::path::Path::new(&dir).join(format!("{name}-{side}.ppm"));
                std::fs::write(path, [header.clone(), pixels].concat()).unwrap();
            }
        }
        let mut worst: Option<(u8, usize, usize, usize, u8, u8)> = None;
        let mut off = 0;
        let mut too_far = false;
        for (index, (cpu, gpu)) in frame
            .pixels
            .chunks_exact(4)
            .zip(expected.pixels())
            .enumerate()
        {
            for (channel, (cpu, gpu)) in [cpu[2], cpu[1], cpu[0]].into_iter().zip(gpu.0).enumerate() {
                let diff = cpu.abs_diff(gpu);
                too_far |= diff > max_difference;
                if diff > 1 {
                    off += 1;
                    if worst.is_none_or(|(d, ..)| diff > d) {
                        worst = Some((diff, index % WIDTH, index / WIDTH, channel, cpu, gpu));
                    }
                }
            }
        }
        let allowed = (share * (WIDTH * HEIGHT * 3) as f32) as usize;
        if let Some((diff, x, y, channel, cpu, gpu)) = worst
            && (too_far || off > allowed)
        {
            panic!(
                "{off} channel values differ by more than one step; worst at ({x}, {y}), \
                 channel {channel}: CPU {cpu}, GPU {gpu} ({diff})"
            );
        }
    }

    #[test]
    fn quads_match_the_gpu() {
        assert_matches_gpu(|_| {
            let mut scene = Scene::default();
            let quads = [
                Quad {
                    bounds: rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
                    background: hsla(0.6, 0.2, 0.15, 1.0).into(),
                    ..Quad::default()
                },
                Quad {
                    bounds: rect(8.3, 6.7, 70.0, 40.5),
                    background: hsla(0.1, 0.8, 0.5, 0.6).into(),
                    border_color: hsla(0.9, 0.7, 0.6, 1.0).into(),
                    border_widths: Edges {
                        top: ScaledPixels(1.0),
                        right: ScaledPixels(2.5),
                        bottom: ScaledPixels(3.0),
                        left: ScaledPixels(1.5),
                    },
                    corner_radii: Corners {
                        top_left: ScaledPixels(0.0),
                        top_right: ScaledPixels(6.0),
                        bottom_right: ScaledPixels(12.0),
                        bottom_left: ScaledPixels(3.5),
                    },
                    ..Quad::default()
                },
                Quad {
                    bounds: rect(50.0, 20.0, 60.0, 50.0),
                    content_mask: ContentMask {
                        bounds: rect(55.0, 25.0, 40.0, 35.0),
                        fade_out: Edges {
                            top: ScaledPixels(0.0),
                            right: ScaledPixels(8.0),
                            bottom: ScaledPixels(5.0),
                            left: ScaledPixels(0.0),
                        },
                    },
                    background: hsla(0.35, 0.6, 0.6, 0.8).into(),
                    ..Quad::default()
                },
                Quad {
                    bounds: rect(120.0, 5.0, 35.0, 45.0),
                    background: pattern_slash(hsla(0.8, 0.5, 0.7, 1.0), 2.0, 4.0),
                    ..Quad::default()
                },
            ];
            for mut quad in quads {
                if quad.content_mask.bounds.size.width.0 == 0.0 {
                    quad.content_mask = everywhere();
                }
                scene.insert_primitive(quad);
            }
            scene.finish();
            scene
        });
    }

    /// Gradients add position-based noise, and smoothed corners a curve, whose float math the
    /// GPU computes a little differently.
    #[test]
    fn gradients_and_smoothed_corners_nearly_match_the_gpu() {
        assert_nearly_matches_gpu(
            |_| {
                let mut scene = Scene::default();
                let quads = [
                    Quad {
                        bounds: rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
                        background: hsla(0.6, 0.2, 0.15, 1.0).into(),
                        ..Quad::default()
                    },
                    Quad {
                        bounds: rect(100.0, 60.0, 50.0, 50.0),
                        background: hsla(0.5, 0.5, 0.5, 1.0).into(),
                        corner_radii: Corners::all(ScaledPixels(14.0)),
                        corner_smoothing: 0.6,
                        ..Quad::default()
                    },
                    Quad {
                        bounds: rect(10.0, 60.0, 80.0, 50.0),
                        background: linear_gradient(
                            45.0,
                            linear_color_stop(hsla(0.0, 0.8, 0.5, 1.0), 0.0),
                            linear_color_stop(hsla(0.7, 0.8, 0.5, 0.5), 1.0),
                        ),
                        border_style: gpui::BorderStyle::Dashed,
                        border_color: hsla(0.15, 1.0, 0.7, 1.0).into(),
                        border_widths: Edges::all(ScaledPixels(2.0)),
                        corner_radii: Corners::all(ScaledPixels(5.0)),
                        ..Quad::default()
                    },
                ];
                for mut quad in quads {
                    quad.content_mask = everywhere();
                    scene.insert_primitive(quad);
                }
                scene.finish();
                scene
            },
            10,
            0.03,
        );
    }

    #[test]
    fn underlines_match_the_gpu() {
        assert_matches_gpu(|_| {
            let mut scene = Scene::default();
            for (y, wavy) in [(20.0, false), (50.3, true)] {
                let bounds = rect(10.0, y, 130.0, 6.0);
                scene.insert_primitive(Underline {
                    order: 0,
                    padding: 0,
                    bounds,
                    clip_bounds: rect(10.0, y - 4.0, 120.0, 14.0),
                    content_mask: everywhere(),
                    color: color(0.55, 0.8, 0.6, 1.0),
                    thickness: ScaledPixels(1.5),
                    wavy: if wavy {
                        gpui::ShaderBool::Enabled
                    } else {
                        gpui::ShaderBool::Disabled
                    },
                });
            }
            scene.finish();
            scene
        });
    }

    #[test]
    fn sprites_match_the_gpu() {
        assert_matches_gpu(|atlas| {
            let mut scene = Scene::default();
            scene.insert_primitive(Quad {
                bounds: rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
                content_mask: everywhere(),
                background: hsla(0.6, 0.2, 0.85, 1.0).into(),
                ..Quad::default()
            });
            let mono = tile(
                atlas,
                AtlasKey::Svg(RenderSvgParams {
                    path: "mono".into(),
                    size: Size {
                        width: DevicePixels(24),
                        height: DevicePixels(20),
                    },
                }),
                (24, 20),
                1,
            );
            scene.insert_primitive(MonochromeSprite {
                order: 0,
                padding: 0,
                bounds: rect(10.0, 10.0, 24.0, 20.0),
                content_mask: mask(rect(10.0, 10.0, 20.0, 20.0)),
                color: color(0.0, 0.7, 0.3, 1.0),
                tile: mono,
                transformation: TransformationMatrix::unit(),
            });
            let image = tile(
                atlas,
                AtlasKey::Image(RenderImageParams {
                    image_id: ImageId(1),
                    frame_index: 0,
                }),
                (30, 30),
                4,
            );
            for (x, grayscale) in [(50.0, false), (90.0, true)] {
                scene.insert_primitive(PolychromeSprite {
                    order: 0,
                    grayscale: if grayscale {
                        gpui::ShaderBool::Enabled
                    } else {
                        gpui::ShaderBool::Disabled
                    },
                    opacity: 0.8,
                    corner_smoothing: 0.0,
                    bounds: rect(x, 10.0, 30.0, 30.0),
                    content_mask: everywhere(),
                    corner_radii: Corners::all(ScaledPixels(6.0)),
                    tile: image,
                });
            }
            scene.finish();
            scene
        });
    }

    #[test]
    fn subpixel_glyphs_match_the_gpu() {
        assert_matches_gpu(|atlas| {
            let mut scene = Scene::default();
            scene.insert_primitive(Quad {
                bounds: rect(0.0, 0.0, WIDTH as f32, HEIGHT as f32),
                content_mask: everywhere(),
                background: hsla(0.62, 0.15, 0.12, 1.0).into(),
                ..Quad::default()
            });
            let params = gpui::RenderGlyphParams {
                font_id: gpui::FontId(0),
                glyph_id: gpui::GlyphId(1),
                font_size: gpui::px(14.0),
                subpixel_variant: Point::default(),
                scale_factor: 1.0,
                raster_style: gpui::PreparedRasterStyle {
                    mode: gpui::GlyphRenderMode::Subpixel,
                    color_effect: gpui::RasterColorEffect::Independent,
                    foreground_dependency: gpui::ForegroundDependency::Full,
                },
            };
            let glyph = tile(
                atlas,
                AtlasKey::from((params, RasterizedGlyphFormat::BgraSubpixelMask)),
                (12, 16),
                4,
            );
            for (x, l) in [(10.0, 0.9), (40.0, 0.4)] {
                scene.insert_primitive(SubpixelSprite {
                    order: 0,
                    padding: 0,
                    bounds: rect(x, 20.0, 12.0, 16.0),
                    content_mask: mask(rect(0.0, 0.0, WIDTH as f32, 30.0)),
                    color: color(0.1, 0.3, l, 1.0),
                    tile: glyph,
                    transformation: TransformationMatrix::unit(),
                });
            }
            scene.finish();
            scene
        });
    }
}
