//! Native macOS renderer. A complete frame is rebuilt for every drawable.
//!
//! All command buffers retain their resources. CPU upload/instance buffers are
//! immutable while in flight and recycled only after command completion. Atlas
//! writes are queued blits, ordered before reads on each window's command queue.

use std::mem::size_of_val;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Instant;

mod diagnostics;
mod pool;
mod resources;
use crate::renderer::metrics;
use diagnostics::Diagnostics;
use pool::FramePool;
use resources::SharedResources;

use crossfont::{BitmapBuffer, GlyphKey, Metrics, RasterizedGlyph};
use log::{debug, info, warn};
use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_foundation::{NSSize, NSString};
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use raw_window_metal::Layer;
use winit::raw_window_handle::RawWindowHandle;

use alacritty_terminal::term::cell::Flags;

use crate::display::SizeInfo;
use crate::display::color::Rgb;
use crate::display::content::RenderableCell;
use crate::display::window::Window;
use crate::renderer::Error;
use crate::renderer::rects::{RectKind, RenderRect};
use crate::renderer::text::glyph_cache::{Glyph, GlyphCache, LoadGlyph};

// Required by MTLCreateSystemDefaultDevice, independently of transitive linkage.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {}

const ATLAS_SIZE: usize = 1024;
const MAX_GLYPH_SIZE: usize = 4096;
const MAX_FRAMES_IN_FLIGHT: usize = 2;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    params: [f32; 4],
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Uniforms {
    size: [f32; 4],
    cell: [f32; 4],
    decoration: [f32; 4],
}

struct Draw {
    start: usize,
    count: usize,
    atlas: Option<usize>,
    clip: bool,
    uniforms: Uniforms,
}

/// Shelf allocator separated from GPU allocation for deterministic bounds tests.
#[derive(Default)]
struct Shelf {
    x: usize,
    y: usize,
    row_height: usize,
}

impl Shelf {
    fn insert(&mut self, width: usize, height: usize, size: usize) -> Option<(usize, usize)> {
        if width > size || height > size {
            return None;
        }
        let (mut x, mut y, mut row_height) = (self.x, self.y, self.row_height);
        if x + width > size {
            y += row_height;
            x = 0;
            row_height = 0;
        }
        if y + height > size {
            return None;
        }
        self.x = x + width;
        self.y = y;
        self.row_height = row_height.max(height);
        Some((x, y))
    }
}

struct Atlas {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    shelf: Shelf,
    size: usize,
}

struct Upload {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    bytes: Vec<u8>,
    origin: MTLOrigin,
    width: usize,
    height: usize,
    stride: usize,
}

pub struct MetalRenderer {
    shared: Rc<SharedResources>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    atlases: Vec<Atlas>,
    uploads: Vec<Upload>,
    instances: Vec<Instance>,
    draws: Vec<Draw>,
    frames: FramePool,
    glyph_scratch: Vec<(usize, Instance)>,
    size: Uniforms,
    background: MTLClearColor,
    failed: bool,
    diagnostics: Option<Diagnostics>,
}

impl MetalRenderer {
    pub fn new(window: &Window) -> Result<Self, Error> {
        let main_thread = MainThreadMarker::new()
            .ok_or_else(|| Error::Other("Metal window setup requires the main thread".into()))?;
        let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
            return Err(Error::Other("Metal requires an AppKit window".into()));
        };
        let (shared, reused) = SharedResources::get(main_thread)?;
        let queue = shared
            .device
            .newCommandQueue()
            .ok_or_else(|| Error::Other("unable to create Metal command queue".into()))?;
        let pipeline_build_us = if reused { 0. } else { shared.build_us };
        // SAFETY: the framework's immutable sRGB name is initialized by CoreGraphics.
        let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .ok_or_else(|| Error::Other("unable to create sRGB color space".into()))?;
        // All fallible device/pipeline setup precedes attaching the native layer.
        // SAFETY: winit owns this NSView; setup runs on its main thread while the
        // Window remains alive. The helper preserves winit's root layer callbacks.
        let layer = unsafe { Layer::from_ns_view(handle.ns_view) };
        // SAFETY: into_raw transfers a non-null CAMetalLayer at +1 retain count.
        let layer = unsafe { Retained::from_raw(layer.into_raw().as_ptr().cast::<CAMetalLayer>()) }
            .ok_or_else(|| Error::Other("unable to retain Metal layer".into()))?;
        layer.setDevice(Some(&shared.device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        // Match the existing sRGB NSWindow without introducing sRGB texture
        // conversion (current GL shaders blend encoded terminal color values).
        layer.setColorspace(Some(&color_space));
        layer.setFramebufferOnly(true);
        layer.setAllowsNextDrawableTimeout(true);
        layer.setDisplaySyncEnabled(true);
        // clear() updates this when window opacity changes at runtime.
        layer.setOpaque(true);
        info!("Using native Metal renderer on {}", shared.device.name());
        Ok(Self {
            shared,
            queue,
            layer,
            atlases: Vec::new(),
            uploads: Vec::new(),
            instances: Vec::new(),
            draws: Vec::new(),
            frames: FramePool::default(),
            glyph_scratch: Vec::new(),
            size: Uniforms::default(),
            background: MTLClearColor { red: 0., green: 0., blue: 0., alpha: 1. },
            failed: false,
            diagnostics: metrics::enabled().then(|| Diagnostics {
                window: window.id().into(),
                pipeline_build_us,
                pipeline_reused: reused,
                ..Diagnostics::default()
            }),
        })
    }

    pub fn resize(&mut self, size: &SizeInfo) {
        self.size.size = [size.width(), size.height(), size.padding_x(), size.padding_y()];
        self.size.cell[0] = size.cell_width();
        self.size.cell[1] = size.cell_height();
        self.layer.setDrawableSize(NSSize::new(size.width().into(), size.height().into()));
    }

    pub fn clear(&mut self, color: Rgb, alpha: f32) {
        self.instances.clear();
        self.draws.clear();
        if (self.background.alpha >= 1.) != (alpha >= 1.) {
            self.layer.setOpaque(alpha >= 1.);
        }
        self.background = MTLClearColor {
            red: f64::from(color.r) / 255. * f64::from(alpha),
            green: f64::from(color.g) / 255. * f64::from(alpha),
            blue: f64::from(color.b) / 255. * f64::from(alpha),
            alpha: alpha.into(),
        };
    }

    pub fn draw_cells<I: Iterator<Item = RenderableCell>>(
        &mut self,
        size: &SizeInfo,
        cache: &mut GlyphCache,
        cells: I,
    ) {
        let mut glyphs = std::mem::take(&mut self.glyph_scratch);
        let background_start = self.instances.len();
        for mut cell in cells {
            let font_key = match cell.flags & Flags::BOLD_ITALIC {
                Flags::BOLD_ITALIC => cache.bold_italic_key,
                Flags::ITALIC => cache.italic_key,
                Flags::BOLD => cache.bold_key,
                _ => cache.font_key,
            };
            let hidden = cell.flags.contains(Flags::HIDDEN);
            if hidden || cell.character == '\t' {
                cell.character = ' ';
            }
            let x = size.padding_x() + cell.point.column.0 as f32 * size.cell_width();
            let y = size.padding_y() + cell.point.line as f32 * size.cell_height();
            if cell.bg_alpha != 0. {
                let columns = if cell.flags.contains(Flags::WIDE_CHAR) { 2. } else { 1. };
                self.instances.push(Instance {
                    rect: [x, y, size.cell_width() * columns, size.cell_height()],
                    uv: [0.; 4],
                    color: color(cell.bg, cell.bg_alpha),
                    params: [0.; 4],
                });
            }
            let mut key = GlyphKey { font_key, size: cache.font_size, character: cell.character };
            let glyph = cache.get(key, self, true);
            push_glyph(&mut glyphs, glyph, &cell, size, x, y);
            if !hidden
                && let Some(zerowidth) =
                    cell.extra.as_mut().and_then(|extra| extra.zerowidth.take())
            {
                for character in zerowidth {
                    key.character = character;
                    let glyph = cache.get(key, self, false);
                    push_glyph(&mut glyphs, glyph, &cell, size, x, y);
                }
            }
        }
        // Backgrounds use the dual-source pipeline with a mask of one, matching
        // OpenGL's replace operation for per-cell premultiplied backgrounds.
        self.add_draw(background_start, Some(0), true, self.size);
        for (atlas, instance) in glyphs.drain(..) {
            let start = self.instances.len();
            self.instances.push(instance);
            self.add_draw(start, Some(atlas), true, self.size);
        }
        self.glyph_scratch = glyphs;
    }

    fn add_draw(&mut self, start: usize, atlas: Option<usize>, clip: bool, uniforms: Uniforms) {
        let count = self.instances.len() - start;
        if count == 0 {
            return;
        }
        // Only merge draws with identical state; rectangle metrics can differ.
        if let Some(last) = self.draws.last_mut()
            && last.start + last.count == start
            && last.atlas == atlas
            && last.clip == clip
            && clip
        {
            last.count += count;
            return;
        }
        self.draws.push(Draw { start, count, atlas, clip, uniforms });
    }

    pub fn draw_rects(&mut self, _: &SizeInfo, metrics: &Metrics, rects: Vec<RenderRect>) {
        let mut uniforms = self.size;
        uniforms.cell[2] = metrics.descent.abs() - metrics.underline_position.abs();
        uniforms.cell[3] = metrics.underline_thickness;
        uniforms.decoration[0] = (0.5 * metrics.descent).abs();
        // Match OpenGL decoration ordering: normal overlays (e.g. bell) last.
        for kind in (0..RectKind::NumKinds as u8).rev() {
            let start = self.instances.len();
            for rect in rects.iter().filter(|rect| rect.kind as u8 == kind) {
                let shader_kind = match rect.kind {
                    RectKind::Normal => 0.,
                    RectKind::Undercurl => 3.,
                    RectKind::DottedUnderline => 4.,
                    RectKind::DashedUnderline => 5.,
                    RectKind::NumKinds => continue,
                };
                self.instances.push(Instance {
                    rect: [rect.x, rect.y, rect.width, rect.height],
                    uv: [0.; 4],
                    color: color(rect.color, rect.alpha),
                    params: [shader_kind, 0., 0., 0.],
                });
            }
            self.add_draw(start, None, false, uniforms);
        }
    }

    pub fn present(&mut self) -> Result<bool, Error> {
        autoreleasepool(|_| self.present_inner())
    }

    fn present_inner(&mut self) -> Result<bool, Error> {
        self.reap_completed();
        if self.failed {
            return Err(Error::Other("Metal GPU execution or resource allocation failed".into()));
        }
        if self.size.size[0] < 1. || self.size.size[1] < 1. {
            return Ok(true);
        }
        if self.frames.is_full() {
            if let Some(metrics) = &mut self.diagnostics {
                metrics.in_flight_retries += 1;
            }
            return Ok(false);
        }
        let acquire_start = self.diagnostics.as_ref().map(|_| Instant::now());
        let drawable = self.layer.nextDrawable();
        if let (Some(metrics), Some(start)) = (&mut self.diagnostics, acquire_start) {
            metrics.acquire.record(start.elapsed().as_secs_f64() * 1e6);
            if drawable.is_none() {
                metrics.drawable_retries += 1;
            }
        }
        let Some(drawable) = drawable else { return Ok(false) };
        let encode_start = self.diagnostics.as_ref().map(|_| Instant::now());
        let mut buffers = self
            .frames
            .acquire()
            .ok_or_else(|| Error::Other("Metal frame slot unavailable".into()))?;
        let command = self
            .queue
            .commandBuffer()
            .ok_or_else(|| Error::Other("Metal command buffer allocation failed".into()))?;

        let stats = buffers.prepare(&self.shared.device, &self.instances, &self.uploads)?;
        if let Some(metrics) = &mut self.diagnostics {
            metrics.instance_allocations += stats.instance_allocations;
            metrics.upload_allocations += stats.upload_allocations;
            metrics.buffer_reuses += stats.reuses;
            metrics.instance_bytes += stats.instance_bytes as u64;
            metrics.upload_bytes += stats.upload_bytes as u64;
        }
        if !self.uploads.is_empty() {
            let blit = command
                .blitCommandEncoder()
                .ok_or_else(|| Error::Other("Metal blit encoder allocation failed".into()))?;
            let buffer = buffers
                .uploads
                .as_ref()
                .ok_or_else(|| Error::Other("Metal upload buffer missing".into()))?;
            for (upload, &offset) in self.uploads.iter().zip(&buffers.upload_offsets) {
                // SAFETY: the buffer holds stride * height bytes. The destination
                // region was bounds-checked by Shelf; RGBA8 uses four bytes/pixel.
                unsafe {
                    blit.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                        buffer, offset, upload.stride, upload.stride * upload.height,
                        MTLSize { width: upload.width, height: upload.height, depth: 1 },
                        &upload.texture, 0, 0, upload.origin,
                    );
                }
            }
            blit.endEncoding();
        }
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: color attachment index zero is valid (Metal supports eight).
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&drawable.texture()));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(self.background);
        let encoder = command
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| Error::Other("Metal render encoder allocation failed".into()))?;
        if let Some(buffer) = &buffers.instances {
            for draw in &self.draws {
                let (width, height) = (self.size.size[0] as usize, self.size.size[1] as usize);
                let (x, y) = if draw.clip {
                    (self.size.size[2] as usize, self.size.size[3] as usize)
                } else {
                    (0, 0)
                };
                let clip_width = width.saturating_sub(x.saturating_mul(2));
                let clip_height = height.saturating_sub(y.saturating_mul(2));
                if clip_width == 0 || clip_height == 0 {
                    continue;
                }
                encoder.setScissorRect(MTLScissorRect {
                    x,
                    y,
                    width: clip_width,
                    height: clip_height,
                });
                let pipeline =
                    if draw.atlas.is_some() { &self.shared.text } else { &self.shared.rect };
                encoder.setRenderPipelineState(pipeline);
                // SAFETY: Instance/Uniforms match MSL layouts; offsets and counts
                // refer to initialized elements inside the immutable retained buffer.
                // Bytes APIs copy the uniforms synchronously. Each draw uses six
                // fixed vertices, and each instance indexes one buffer element.
                unsafe {
                    encoder.setVertexBuffer_offset_atIndex(
                        Some(buffer),
                        draw.start * std::mem::size_of::<Instance>(),
                        0,
                    );
                    encoder.setVertexBytes_length_atIndex(
                        NonNull::from(&draw.uniforms).cast(),
                        size_of_val(&draw.uniforms),
                        1,
                    );
                    encoder.setFragmentBytes_length_atIndex(
                        NonNull::from(&draw.uniforms).cast(),
                        size_of_val(&draw.uniforms),
                        0,
                    );
                    if let Some(atlas) = draw.atlas {
                        if let Some(atlas) = self.atlases.get(atlas) {
                            encoder.setFragmentTexture_atIndex(Some(&atlas.texture), 0);
                        } else {
                            continue;
                        }
                    }
                    encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                        MTLPrimitiveType::Triangle,
                        0,
                        6,
                        draw.count,
                    );
                }
            }
        }
        encoder.endEncoding();
        command.presentDrawable(ProtocolObject::from_ref(&*drawable));
        self.frames.submit(command, buffers);
        self.uploads.clear();
        if let (Some(metrics), Some(start)) = (&mut self.diagnostics, encode_start) {
            metrics.submitted += 1;
            metrics.encode.record(start.elapsed().as_secs_f64() * 1e6);
        }
        Ok(true)
    }

    fn reap_completed(&mut self) {
        let diagnostics = &mut self.diagnostics;
        if let Err(err) = self.frames.reap(|frame| {
            if let Some(metrics) = diagnostics {
                metrics.completed += 1;
                let start = frame.GPUStartTime();
                let end = frame.GPUEndTime();
                if start > 0. && end >= start {
                    metrics.gpu.record((end - start) * 1e6);
                }
            }
        }) {
            warn!("{err}");
            self.failed = true;
        }
    }

    pub fn finish(&self) {
        self.frames.finish();
    }

    fn allocate_atlas(&mut self, size: usize) -> Option<usize> {
        let descriptor = MTLTextureDescriptor::new();
        descriptor.setTextureType(MTLTextureType::Type2D);
        descriptor.setPixelFormat(MTLPixelFormat::RGBA8Unorm);
        // SAFETY: callers restrict size to a positive power of two <= 4096,
        // within the supported 2D texture dimensions on Metal-capable Macs.
        unsafe {
            descriptor.setWidth(size);
            descriptor.setHeight(size);
        }
        descriptor.setStorageMode(MTLStorageMode::Private);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        let texture = self.shared.device.newTextureWithDescriptor(&descriptor)?;
        let index = self.atlases.len();
        self.atlases.push(Atlas { texture, size, shelf: Shelf::default() });
        Some(index)
    }
}

impl LoadGlyph for MetalRenderer {
    fn load_glyph(&mut self, glyph: &RasterizedGlyph) -> Glyph {
        let mut result = Glyph {
            tex_id: 0,
            multicolor: matches!(glyph.buffer, BitmapBuffer::Rgba(_)),
            top: glyph.top as i16,
            left: glyph.left as i16,
            width: 0,
            height: 0,
            uv_bot: 0.,
            uv_left: 0.,
            uv_width: 0.,
            uv_height: 0.,
        };
        let (width, height) = (glyph.width.max(0) as usize, glyph.height.max(0) as usize);
        if width == 0 || height == 0 || width > MAX_GLYPH_SIZE || height > MAX_GLYPH_SIZE {
            return result;
        }
        let mut location = self.atlases.iter_mut().enumerate().rev().find_map(|(index, atlas)| {
            atlas.shelf.insert(width, height, atlas.size).map(|origin| (index, origin))
        });
        if location.is_none() {
            let size = ATLAS_SIZE.max(width.next_power_of_two()).max(height.next_power_of_two());
            if let Some(index) = self.allocate_atlas(size) {
                location = self.atlases[index]
                    .shelf
                    .insert(width, height, size)
                    .map(|origin| (index, origin));
            }
        }
        let Some((index, (x, y))) = location else {
            warn!("Metal glyph atlas allocation failed");
            self.failed = true;
            return result;
        };
        let Some((bytes, stride)) = upload_bytes(&glyph.buffer, width, height) else {
            self.failed = true;
            return result;
        };
        let atlas = &self.atlases[index];
        self.uploads.push(Upload {
            texture: atlas.texture.clone(),
            bytes,
            stride,
            width,
            height,
            origin: MTLOrigin { x, y, z: 0 },
        });
        result.tex_id = index as u32;
        result.width = width as i16;
        result.height = height as i16;
        result.uv_left = x as f32 / atlas.size as f32;
        result.uv_bot = y as f32 / atlas.size as f32;
        result.uv_width = width as f32 / atlas.size as f32;
        result.uv_height = height as f32 / atlas.size as f32;
        result
    }

    fn clear(&mut self) {
        // Old in-flight commands retain the textures they use. New glyphs get new
        // textures, so a font reset never mutates a texture a GPU frame is reading.
        self.atlases.clear();
        self.uploads.clear();
        self.instances.clear();
        self.draws.clear();
    }
}

impl Drop for MetalRenderer {
    fn drop(&mut self) {
        self.finish();
        self.reap_completed();
        if let Some(metrics) = &mut self.diagnostics {
            metrics.cached_buffer_bytes = self.frames.cached_bytes();
            metrics.log();
        }
        // The native window still exists here; disconnect observer-backed layer
        // before releasing GPU resources and before Window's destructor runs.
        self.layer.removeFromSuperlayer();
        debug!("Metal renderer destroyed");
    }
}

fn create_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    fragment: &str,
    dual_source: bool,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, Error> {
    let vertex = library
        .newFunctionWithName(&NSString::from_str("terminal_vertex"))
        .ok_or_else(|| Error::Other("missing Metal vertex function".into()))?;
    let fragment = library
        .newFunctionWithName(&NSString::from_str(fragment))
        .ok_or_else(|| Error::Other("missing Metal fragment function".into()))?;
    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vertex));
    descriptor.setFragmentFunction(Some(&fragment));
    // SAFETY: color attachment index zero is valid (Metal supports eight).
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    attachment.setBlendingEnabled(true);
    if dual_source {
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::Source1Color);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSource1Color);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::Source1Alpha);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSource1Alpha);
    } else {
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::SourceAlpha);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::One);
    }
    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|err| Error::Other(format!("Metal render pipeline creation failed: {err}")))
}

#[cfg(test)]
fn buffer<T>(
    device: &ProtocolObject<dyn MTLDevice>,
    values: &[T],
) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
    // SAFETY: Metal immediately copies size_of_val bytes from the initialized
    // slice. This helper is only used with byte arrays or repr(C) float-only
    // instances without padding. No borrowed pointer escapes this call.
    unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(values.as_ptr().cast_mut().cast::<std::ffi::c_void>())?,
            size_of_val(values),
            MTLResourceOptions::StorageModeShared,
        )
    }
}

fn color(rgb: Rgb, alpha: f32) -> [f32; 4] {
    [f32::from(rgb.r) / 255., f32::from(rgb.g) / 255., f32::from(rgb.b) / 255., alpha]
}

fn push_glyph(
    items: &mut Vec<(usize, Instance)>,
    glyph: Glyph,
    cell: &RenderableCell,
    size: &SizeInfo,
    x: f32,
    y: f32,
) {
    if glyph.width == 0 || glyph.height == 0 {
        return;
    }
    items.push((glyph.tex_id as usize, Instance {
        rect: [
            x + f32::from(glyph.left),
            y + size.cell_height() - f32::from(glyph.top),
            f32::from(glyph.width),
            f32::from(glyph.height),
        ],
        uv: [glyph.uv_left, glyph.uv_bot, glyph.uv_width, glyph.uv_height],
        color: color(cell.fg, 1.),
        params: [if glyph.multicolor { 2. } else { 1. }, 0., 0., 0.],
    }));
}

/// Metal blits require aligned rows on supported macOS GPUs. Pad each row to
/// 256 bytes, retaining RGB coverage independently for subpixel text blending.
fn upload_bytes(bitmap: &BitmapBuffer, width: usize, height: usize) -> Option<(Vec<u8>, usize)> {
    let stride = width.checked_mul(4)?.checked_add(255)? / 256 * 256;
    let mut output = vec![0; stride.checked_mul(height)?];
    let (source, channels) = match bitmap {
        BitmapBuffer::Rgb(bytes) => (bytes, 3),
        BitmapBuffer::Rgba(bytes) => (bytes, 4),
    };
    if source.len() != width.checked_mul(height)?.checked_mul(channels)? {
        return None;
    }
    for y in 0..height {
        for x in 0..width {
            let src = (y * width + x) * channels;
            let dst = y * stride + x * 4;
            output[dst..dst + 3].copy_from_slice(&source[src..src + 3]);
            output[dst + 3] = if channels == 4 { source[src + 3] } else { 255 };
        }
    }
    Some((output, stride))
}

#[cfg(test)]
mod tests;
