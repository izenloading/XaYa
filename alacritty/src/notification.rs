//! Desktop notifications requested by programs running in the terminal.

use std::time::Duration;

use alacritty_terminal::notification::Notification;

use crate::config::notifications::NotificationsConfig;

/// Minimum time between two notifications of one window, so a runaway program can't flood the
/// desktop.
pub const NOTIFICATION_COOLDOWN: Duration = Duration::from_millis(1000);

/// Title used when the program supplied only a body.
const DEFAULT_TITLE: &str = "XaYa";

/// How a notification gets shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Spawn `program` with `args`.
    Command { program: String, args: Vec<String> },

    /// Post through `UNUserNotificationCenter`, so macOS shows xaya's icon.
    #[cfg(target_os = "macos")]
    Native { title: String, body: String },
}

/// How to show `notification`, or `None` when this platform has no default notifier and none is
/// configured.
///
/// Title and body are always passed as separate arguments, never interpolated into a script, so
/// program output can't inject commands.
pub fn backend(config: &NotificationsConfig, notification: &Notification) -> Option<Backend> {
    let title = notification.title.clone().unwrap_or_else(|| DEFAULT_TITLE.into());
    let body = notification.body.clone();

    if let Some(program) = &config.command {
        let mut args = program.args().to_vec();
        args.extend([title, body]);
        return Some(Backend::Command { program: program.program().into(), args });
    }

    default_backend(title, body)
}

#[cfg(target_os = "macos")]
fn default_backend(title: String, body: String) -> Option<Backend> {
    if crate::macos::notification::is_available() {
        return Some(Backend::Native { title, body });
    }

    // Outside an app bundle macOS refuses native notifications; `osascript` still works, but
    // shows Script Editor's icon.
    let script = [
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
    ];
    let mut args: Vec<String> = script.iter().map(|arg| arg.to_string()).collect();
    args.extend([title, body]);
    Some(Backend::Command { program: "osascript".into(), args })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn default_backend(title: String, body: String) -> Option<Backend> {
    let args = vec![
        "--app-name=XaYa".into(),
        // Both match `extra/linux/xaya.desktop`, so the desktop shows xaya's icon and groups the
        // notification under the app.
        "--icon=xaya".into(),
        "--hint=string:desktop-entry:xaya".into(),
        "--".into(),
        title,
        body,
    ];
    Some(Backend::Command { program: "notify-send".into(), args })
}

#[cfg(not(unix))]
fn default_backend(_title: String, _body: String) -> Option<Backend> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ui_config::Program;

    fn notification(title: Option<&str>, body: &str) -> Notification {
        Notification { title: title.map(Into::into), body: body.into() }
    }

    /// The program and arguments of a backend that spawns a command.
    fn command(config: &NotificationsConfig, notification: &Notification) -> (String, Vec<String>) {
        match backend(config, notification) {
            Some(Backend::Command { program, args }) => (program, args),
            other => panic!("expected a command backend, got {other:?}"),
        }
    }

    #[test]
    fn custom_command_gets_title_and_body_appended() {
        let config = NotificationsConfig {
            command: Some(Program::WithArgs {
                program: "notifier".into(),
                args: vec!["--urgent".into()],
            }),
            ..Default::default()
        };
        let (program, args) = command(&config, &notification(Some("T"), "B"));
        assert_eq!(program, "notifier");
        assert_eq!(args, ["--urgent", "T", "B"]);
    }

    #[test]
    fn missing_title_falls_back_to_app_name() {
        let config = NotificationsConfig {
            command: Some(Program::Just("notifier".into())),
            ..Default::default()
        };
        let (_, args) = command(&config, &notification(None, "done"));
        assert_eq!(args, [DEFAULT_TITLE, "done"]);
    }

    #[cfg(unix)]
    #[test]
    fn default_command_passes_text_as_separate_arguments() {
        let hostile = "\" & do shell script \"rm -rf ~\" & \"";
        // Test binaries aren't app bundles, so macOS falls back to `osascript` here.
        let (_, args) =
            command(&NotificationsConfig::default(), &notification(Some(hostile), hostile));
        // The text is the trailing arguments verbatim and appears nowhere else.
        assert_eq!(&args[args.len() - 2..], [hostile, hostile]);
        assert!(args[..args.len() - 2].iter().all(|arg| !arg.contains("rm -rf")));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn notify_send_uses_the_desktop_entry_icon() {
        let (program, args) = command(&NotificationsConfig::default(), &notification(None, "hi"));
        assert_eq!(program, "notify-send");
        assert!(args.iter().any(|arg| arg == "--icon=xaya"));
        assert!(args.iter().any(|arg| arg == "--hint=string:desktop-entry:xaya"));
        // Options come before `--`, so the text can't be mistaken for one.
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&args[separator + 1..], [DEFAULT_TITLE, "hi"]);
    }
}
