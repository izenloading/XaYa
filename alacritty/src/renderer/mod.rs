//! GPU backend selection and the rendering interface shared by the display.

use std::num::NonZeroU32;
use std::{fmt, mem};

use crossfont::Metrics;
use glutin::config::{Config, GetGlConfig};
use glutin::context::{NotCurrentContext, PossiblyCurrentContext};
use glutin::display::GetGlDisplay;
use glutin::error::ErrorKind;
use glutin::prelude::*;
use glutin::surface::{Surface, SwapInterval, WindowSurface};
use log::{debug, info};
use unicode_width::UnicodeWidthChar;
use winit::dpi::PhysicalSize;

use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;

use crate::config::debug::RendererPreference;
use crate::display::SizeInfo;
use crate::display::color::Rgb;
use crate::display::content::RenderableCell;
use crate::display::window::Window;
use rects::RenderRect;
use shader::ShaderError;
use text::glyph_cache::LoadGlyph;

#[cfg(target_os = "macos")]
mod metal;
pub mod metrics;
mod opengl;
pub mod platform;
pub mod rects;
mod shader;
mod text;

use opengl::GlExtensions;
pub use text::GlyphCache;

#[derive(Debug)]
pub enum Error {
    /// Shader error.
    Shader(ShaderError),

    /// Other error.
    Other(String),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Shader(err) => err.source(),
            Error::Other(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Shader(err) => {
                write!(f, "There was an error initializing the shaders: {err}")
            },
            Error::Other(err) => {
                write!(f, "{err}")
            },
        }
    }
}

impl From<ShaderError> for Error {
    fn from(val: ShaderError) -> Self {
        Error::Shader(val)
    }
}

impl From<String> for Error {
    fn from(val: String) -> Self {
        Error::Other(val)
    }
}

/// OpenGL resources must be destroyed with their owning context current.
pub struct GlBackend {
    renderer: Option<opengl::Renderer>,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    preference: Option<RendererPreference>,
}

impl GlBackend {
    fn renderer(&self) -> &opengl::Renderer {
        self.renderer.as_ref().expect("OpenGL renderer is initialized outside context recovery")
    }

    fn renderer_mut(&mut self) -> &mut opengl::Renderer {
        self.renderer.as_mut().expect("OpenGL renderer is initialized outside context recovery")
    }
}

impl Drop for GlBackend {
    fn drop(&mut self) {
        let renderer = self.renderer.take();
        if let Err(err) = self.context.make_current(&self.surface) {
            // Never delete another window's GL objects. The native context and
            // surface still drop normally; only the unusable GL wrapper is leaked.
            log::warn!("Unable to activate GL context during teardown: {err}");
            mem::forget(renderer);
        } else {
            drop(renderer);
        }
    }
}

pub enum Renderer {
    OpenGl(Box<GlBackend>),
    #[cfg(target_os = "macos")]
    Metal(Box<metal::MetalRenderer>),
}

impl Renderer {
    pub fn new(
        window: &Window,
        context: Option<NotCurrentContext>,
        preference: Option<RendererPreference>,
    ) -> Result<Self, crate::display::Error> {
        #[cfg(target_os = "macos")]
        if preference == Some(RendererPreference::Metal) {
            // Metal initialization does not attach its layer until all fallible GPU
            // setup has completed, so OpenGL can safely use the same window on failure.
            match metal::MetalRenderer::new(window) {
                Ok(renderer) => return Ok(Self::Metal(Box::new(renderer))),
                Err(err) => {
                    log::warn!("Metal initialization failed, falling back to OpenGL: {err}")
                },
            }
        }

        let context = match context {
            Some(context) => context,
            None => {
                #[cfg(target_os = "macos")]
                let raw_display = winit::raw_window_handle::RawDisplayHandle::AppKit(
                    winit::raw_window_handle::AppKitDisplayHandle::new(),
                );
                #[cfg(not(target_os = "macos"))]
                return Err(Error::Other("missing OpenGL context".into()).into());
                #[cfg(target_os = "macos")]
                {
                    let handle = Some(window.raw_window_handle());
                    let display = platform::create_gl_display(raw_display, handle, false)?;
                    let config =
                        platform::pick_gl_config(&display, handle).map_err(Error::Other)?;
                    platform::create_gl_context(&display, &config, handle)?
                }
            },
        };
        let surface =
            platform::create_gl_surface(&context, window.inner_size(), window.raw_window_handle())?;
        let context = context.make_current(&surface)?;
        let renderer = opengl::Renderer::new(&context, preference)?;
        if let Err(err) = surface.set_swap_interval(&context, SwapInterval::DontWait) {
            info!("Failed to disable vsync: {err}");
        }
        Ok(Self::OpenGl(Box::new(GlBackend {
            renderer: Some(renderer),
            context,
            surface,
            preference,
        })))
    }

    pub fn gl_config(&self) -> Option<Config> {
        match self {
            Self::OpenGl(gl) => Some(gl.context.config()),
            #[cfg(target_os = "macos")]
            Self::Metal(_) => None,
        }
    }

    pub fn make_not_current(&mut self) {
        match self {
            Self::OpenGl(gl) if gl.context.is_current() => {
                gl.context.make_not_current_in_place().expect("failed to disable context");
            },
            _ => (),
        }
    }

    /// Return whether the backend was recreated and the glyph cache needs rebuilding.
    pub fn make_current(&mut self, window: &Window, size: &SizeInfo) -> bool {
        let gl = match self {
            Self::OpenGl(gl) => gl,
            #[cfg(target_os = "macos")]
            Self::Metal(_) => return false,
        };
        let lost = if gl.context.is_current() {
            gl.renderer().was_context_reset()
        } else {
            match gl.context.make_current(&gl.surface) {
                Ok(()) => false,
                Err(err) if err.error_kind() == ErrorKind::ContextLost => true,
                Err(err) => panic!("failed to activate OpenGL context: {err}"),
            }
        };
        if !lost {
            return false;
        }
        let context = platform::create_gl_context(
            &gl.context.display(),
            &gl.context.config(),
            Some(window.raw_window_handle()),
        )
        .expect("failed to recreate context");
        // Take ownership before any fallible setup. If recovery panics, Drop
        // sees None and cannot destroy the old renderer a second time.
        let old_renderer = gl.renderer.take();
        if gl.context.is_current() {
            drop(old_renderer);
        } else {
            // A lost, non-current context cannot safely run GL destructors.
            mem::forget(old_renderer);
        }
        gl.context = context.treat_as_possibly_current();
        gl.context.make_current(&gl.surface).expect("failed to reactivate context");
        gl.renderer = Some(
            opengl::Renderer::new(&gl.context, gl.preference).expect("failed to recreate renderer"),
        );
        gl.renderer().resize(size);
        true
    }

    pub fn resize_surface(&mut self, size: PhysicalSize<u32>) {
        match self {
            Self::OpenGl(gl) => {
                if let (Some(width), Some(height)) =
                    (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                {
                    gl.surface.resize(&gl.context, width, height);
                }
            },
            #[cfg(target_os = "macos")]
            Self::Metal(_) => (),
        }
    }

    /// Present a complete frame. False means a drawable wasn't available; retry later.
    pub fn present(&mut self, _damage: Option<&[glutin::surface::Rect]>) -> Result<bool, Error> {
        match self {
            Self::OpenGl(gl) => {
                #[cfg(not(any(target_os = "macos", windows)))]
                let result = match (&gl.surface, &gl.context, _damage) {
                    (Surface::Egl(surface), PossiblyCurrentContext::Egl(context), Some(damage)) => {
                        surface.swap_buffers_with_damage(context, damage)
                    },
                    _ => gl.surface.swap_buffers(&gl.context),
                };
                #[cfg(any(target_os = "macos", windows))]
                let result = gl.surface.swap_buffers(&gl.context);
                if let Err(err) = result {
                    debug!("error calling swap_buffers: {err}");
                }
                Ok(true)
            },
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.present(),
        }
    }

    pub fn draw_cells<I: Iterator<Item = RenderableCell>>(
        &mut self,
        size: &SizeInfo,
        cache: &mut GlyphCache,
        cells: I,
    ) {
        match self {
            Self::OpenGl(gl) => gl.renderer_mut().draw_cells(size, cache, cells),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.draw_cells(size, cache, cells),
        }
    }

    /// Draw a string in a variable location. Used for printing the render timer, warnings and
    /// errors.
    pub fn draw_string(
        &mut self,
        point: Point<usize>,
        fg: Rgb,
        bg: Rgb,
        string_chars: impl Iterator<Item = char>,
        size_info: &SizeInfo,
        glyph_cache: &mut GlyphCache,
    ) {
        let mut wide_char_spacer = false;
        let cells = string_chars.enumerate().filter_map(|(i, character)| {
            let flags = if wide_char_spacer {
                wide_char_spacer = false;
                return None;
            } else if character.width() == Some(2) {
                // The spacer is always following the wide char.
                wide_char_spacer = true;
                Flags::WIDE_CHAR
            } else {
                Flags::empty()
            };

            Some(RenderableCell {
                point: Point::new(point.line, point.column + i),
                character,
                extra: None,
                flags,
                bg_alpha: 1.0,
                fg,
                bg,
                underline: fg,
            })
        });

        self.draw_cells(size_info, glyph_cache, cells);
    }

    pub fn with_loader<T>(&mut self, func: impl FnOnce(&mut dyn LoadGlyph) -> T) -> T {
        match self {
            Self::OpenGl(gl) => gl.renderer_mut().with_loader(|mut api| func(&mut api)),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => func(renderer.as_mut()),
        }
    }

    pub fn draw_rects(&mut self, size: &SizeInfo, metrics: &Metrics, rects: Vec<RenderRect>) {
        match self {
            Self::OpenGl(gl) => gl.renderer_mut().draw_rects(size, metrics, rects),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.draw_rects(size, metrics, rects),
        }
    }

    pub fn clear(&mut self, color: Rgb, alpha: f32) {
        match self {
            Self::OpenGl(gl) => gl.renderer().clear(color, alpha),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.clear(color, alpha),
        }
    }

    pub fn finish(&self) {
        match self {
            Self::OpenGl(gl) => gl.renderer().finish(),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.finish(),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn set_viewport(&self, size: &SizeInfo) {
        if let Self::OpenGl(gl) = self {
            gl.renderer().set_viewport(size);
        }
    }

    pub fn resize(&mut self, size: &SizeInfo) {
        match self {
            Self::OpenGl(gl) => gl.renderer().resize(size),
            #[cfg(target_os = "macos")]
            Self::Metal(renderer) => renderer.resize(size),
        }
    }
}
