use std::time::Duration;

use serde::Serialize;

use alacritty_config_derive::ConfigDeserialize;

use crate::config::ui_config::Program;

/// Desktop notifications requested by programs through `OSC 9` / `OSC 777`, or about their
/// status and long commands.
#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct NotificationsConfig {
    /// Whether programs may post desktop notifications.
    pub enabled: bool,

    /// Only notify while the window is unfocused.
    pub only_when_unfocused: bool,

    /// Command used to show a notification; the title and body are appended as arguments.
    ///
    /// Defaults to `osascript` on macOS and `notify-send` elsewhere on unix.
    pub command: Option<Program>,

    /// Seconds a command must run for its finish to be reported, through shell integration
    /// (`OSC 133`); `0` never reports commands.
    pub command_threshold: u64,
}

impl NotificationsConfig {
    /// How long a command must run for its finish to be reported, `None` when never.
    pub fn command_threshold(&self) -> Option<Duration> {
        (self.command_threshold > 0).then(|| Duration::from_secs(self.command_threshold))
    }
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { enabled: true, only_when_unfocused: true, command: None, command_threshold: 10 }
    }
}
