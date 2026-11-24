//! Worktree tabs, giving each agent its own checkout.

use std::path::PathBuf;

use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

/// Worktree tab configuration.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WorktreeConfig {
    /// Command started in a new worktree tab; by default the agent running in the window it was
    /// opened from.
    pub command: Option<String>,

    /// Directory new worktrees go to; by default `<repo>.worktrees` beside the repository.
    pub directory: Option<PathBuf>,

    /// Warn when agents in several windows work in the same checkout.
    pub warn_shared: bool,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self { command: None, directory: None, warn_shared: true }
    }
}
