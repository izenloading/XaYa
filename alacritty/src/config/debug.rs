use log::LevelFilter;
use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

/// Debugging options.
#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Debug {
    pub log_level: LevelFilter,

    pub print_events: bool,

    /// Keep the log file after quitting.
    pub persistent_logging: bool,

    /// Should show render timer.
    pub render_timer: bool,

    /// Highlight damage information produced by alacritty.
    pub highlight_damage: bool,

    /// The renderer alacritty should be using; see [`Debug::renderer`] for the effective one.
    pub renderer: Option<RendererPreference>,

    /// Use EGL as display API if the current platform allows it.
    pub prefer_egl: bool,

    /// Record ref test.
    #[config(skip)]
    #[serde(skip_serializing)]
    pub ref_test: bool,
}

impl Default for Debug {
    fn default() -> Self {
        Self {
            log_level: LevelFilter::Warn,
            print_events: Default::default(),
            persistent_logging: Default::default(),
            render_timer: Default::default(),
            highlight_damage: Default::default(),
            ref_test: Default::default(),
            renderer: Default::default(),
            prefer_egl: Default::default(),
        }
    }
}

impl Debug {
    /// The renderer to use: the configured one, or Metal on macOS when none is configured.
    ///
    /// Metal falls back to OpenGL when it can't be used, so it's a safe default.
    pub fn renderer(&self) -> Option<RendererPreference> {
        #[cfg(target_os = "macos")]
        return Some(self.renderer.unwrap_or(RendererPreference::Metal));

        #[cfg(not(target_os = "macos"))]
        self.renderer
    }
}

/// The renderer configuration options.
#[derive(ConfigDeserialize, Serialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RendererPreference {
    /// Native Metal renderer (macOS only, the default there).
    #[cfg(target_os = "macos")]
    Metal,

    /// OpenGL 3.3 renderer.
    Glsl3,

    /// GLES 2 renderer, with optional extensions like dual source blending.
    Gles2,

    /// Pure GLES 2 renderer.
    Gles2Pure,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn metal_is_the_default_on_macos() {
        assert_eq!(Debug::default().renderer(), Some(RendererPreference::Metal));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn opengl_picks_the_renderer_elsewhere() {
        assert_eq!(Debug::default().renderer(), None);
    }

    #[test]
    fn configured_renderer_wins() {
        let debug = Debug { renderer: Some(RendererPreference::Glsl3), ..Default::default() };
        assert_eq!(debug.renderer(), Some(RendererPreference::Glsl3));
    }
}
