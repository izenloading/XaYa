//! Opening windows and agent sessions again on start.

use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

/// Session restore configuration.
#[derive(ConfigDeserialize, Serialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    /// Which windows still open when xaya stopped to open again on start.
    pub restore: RestoreMode,
}

/// Which windows to open again on start.
#[derive(ConfigDeserialize, Serialize, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    /// Windows running Claude Code or Codex, resuming their sessions.
    #[default]
    Agents,
    /// Every window, in its directory, resuming agent sessions.
    All,
    /// None.
    Never,
}
