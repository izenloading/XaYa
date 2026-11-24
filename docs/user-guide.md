# XaYa User Guide

*English | [简体中文](user-guide.zh-CN.md)*

XaYa is a GPU-accelerated terminal based on Alacritty, extended for people who run several coding
agents such as Claude Code and Codex at once. It doesn't replace the agents' own interfaces;
instead the terminal understands what they're doing, so you can move between sessions quickly,
wait less and lose less.

This guide covers what XaYa adds. For what it inherits from Alacritty (vi mode, search, hints,
selection), see [features.md](features.md) and `man 5 alacritty`.

## Contents

1. [Installation and configuration file](#installation-and-configuration-file)
2. [Getting started](#getting-started)
3. [Agent status: badges, notifications and the Dock](#agent-status-badges-notifications-and-the-dock)
4. [The agent panel](#the-agent-panel)
5. [Shell integration](#shell-integration)
6. [Opening file locations from agent output](#opening-file-locations-from-agent-output)
7. [Split panes](#split-panes)
8. [Worktree tabs: a checkout per agent](#worktree-tabs-a-checkout-per-agent)
9. [Session restore](#session-restore)
10. [Scripting with XaYa msg](#scripting-with-xaya-msg)
11. [Keyboard shortcuts](#keyboard-shortcuts)
12. [Configuration reference](#configuration-reference)
13. [Troubleshooting](#troubleshooting)

## Installation and configuration file

On macOS (Apple Silicon, macOS 11 or newer):

```sh
make app
cp -r target/release/osx/XaYa.app /Applications/
```

For other platforms, see [INSTALL.md](../INSTALL.md).

XaYa doesn't create a config file for you. It uses the first one it finds:

1. `$XDG_CONFIG_HOME/xaya/xaya.toml`
2. `~/.config/xaya/xaya.toml`
3. Legacy Alacritty locations, such as `~/.config/alacritty/alacritty.toml`

To start from scratch, copy [`xaya.example.toml`](../xaya.example.toml) from the repository root: it
lists every option with its default, commented in Chinese. Copied as is it changes nothing;
uncomment the lines you want to change.

Existing Alacritty configs keep working. Every setting in this guide goes in that file, and
changes apply as soon as you save.

## Getting started

1. Open XaYa and run `claude` or `codex` in it. There's nothing to set up: both report their status
   to XaYa on their own.
2. Let the agent work and do something else. When it needs you, XaYa posts a desktop
   notification, the window title shows 🔔 and the Dock icon counts the windows waiting.
3. Press `Cmd+J` to jump to the window that needs you most, or `Cmd+Shift+A` for the agent panel,
   listing every window at once.
4. To start another agent without both editing the same files, press `Cmd+Shift+T`: it opens a tab
   in a new git worktree and starts the same agent there.
5. To keep an agent beside a shell, split with `Cmd+D` (side by side) or `Cmd+Shift+D` (stacked).
6. Install [shell integration](#shell-integration) for prompt jumps and notifications when long
   commands finish.

> Shortcuts in this guide are for macOS; see [Keyboard shortcuts](#keyboard-shortcuts) for other
> platforms.

## Agent status: badges, notifications and the Dock

Claude Code and Codex tell the terminal what they're doing through the Program Status Protocol
(OSC 7501). XaYa shows it outside the window, so you don't have to watch every one.

**Title badges** in front of the window title:

| Badge | Meaning |
|---|---|
| ⏳ (or `⏳ 40%`) | Working, with progress when reported |
| 🔔 | Waiting on you: a permission, a question or a sign-in |
| ✅ | Finished |
| ❌ | Failed |

Finished and failed badges stay until you've seen the window (it gets focus). A working status
ends when the agent exits and the shell is back at its prompt.

**Desktop notifications** when an agent starts waiting on you, fails or finishes, with the agent's
name and its question, such as "Claude Code: Needs your permission — Run cargo test". On macOS,
clicking a notification brings its window or tab to the front. By default, notifications only
show while the window isn't in front.

**Dock count** (macOS): the number on the Dock icon counts windows waiting on you or with a failure
you haven't seen. If it doesn't show, enable Badges in System Settings → Notifications → XaYa.

**Jumping to what needs you**: `Cmd+J` (`Ctrl+Shift+J` elsewhere) goes to windows waiting on you
first, then failed, then finished, the one waiting longest first within each. Press it again for
the next one.

**Duplicate notifications**: while an agent reports its status, its own OSC 9 / OSC 777
notifications are replaced by XaYa's, so you're told once.

Related settings:

```toml
[terminal]
program_status = true          # On by default; off shows none of the above

[notifications]
enabled = true                 # Desktop notifications
only_when_unfocused = true     # Only while the window isn't in front
command_threshold = 10         # Seconds a plain command must run to notify when it ends
                               # (needs shell integration); 0 turns it off
```

## The agent panel

`Cmd+Shift+A` (`Ctrl+Shift+A` elsewhere) opens a panel over the current window listing every
window:

- each row shows a number, the state (such as `waiting: permission` or `working 40%`), the agent,
  how long it has been in that state, what its last turn changed, and the window title;
- an agent's message, such as the command it asks to run, follows on the next line;
- what the last turn changed, such as `3 files +48 -3`, is what the agent's turn changed in its
  git checkout (see below);
- rows are ordered like `Cmd+J`, what needs you most on top;
- it refreshes every second while open.

| Key | Action |
|---|---|
| `↑` `↓`, `j` `k`, `Tab` `Shift+Tab` | Select |
| `Enter` | Go to the selected window |
| `1` – `9` | Go to that row's window |
| `d` | Show the diff of the selected window's last turn, in a new pane below |
| `Esc`, `q`, or `Cmd+Shift+A` again | Close the panel |

### What each turn changed

When an agent reports it's working, XaYa snapshots its git checkout, and again once it's done,
failed or idle; the difference is what the turn changed:

- new untracked files count, files `.gitignore` ignores don't;
- snapshots go through a copy of the index, so what you staged is left alone and nothing is
  committed;
- a turn that changed nothing clears what's shown;
- `xaya msg status --json` reports it as `changes`, with the file and line counts and the two git
  trees, for `git diff <before> <after>`.

Whatever else writes to the checkout during the turn, you included, counts too; two agents sharing
a checkout can't be told apart, so give each a worktree (see below). Set
`terminal.turn_changes = false` to turn it off.

## Shell integration

Shell integration tells XaYa where prompts and commands start and end (OSC 133). With it:

- `Cmd+↑` / `Cmd+↓` jump between the prompts of past commands;
- commands running longer than `notifications.command_threshold` seconds show ✅ or ❌ in the
  title when they end, with a notification saying how long they took;
- an agent's status ends exactly when its shell is back at its prompt;
- `xaya msg get-text --last-command` prints the last command and its output;
- restored windows and worktree tabs type their command as soon as the shell is ready, rather
  than after two seconds.

Add one line at the **end** of your shell's startup file, after any prompt theme:

| Shell | File | Line |
|---|---|---|
| zsh | `~/.zshrc` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.zsh` |
| bash | `~/.bashrc` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.bash` |
| fish | `~/.config/fish/config.fish` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.fish` |

The marks are harmless in other terminals: those that understand OSC 133 use them, the rest
ignore them.

## Opening file locations from agent output

Agents and compilers print locations like `src/main.rs:12:5`. XaYa recognizes them:

- **`Cmd`-click** (`Ctrl`-click elsewhere) opens one. This works inside Claude Code and Codex too,
  even though they capture the mouse, without holding Shift.
- **`Ctrl+Shift+E`** labels every location on screen with a letter; type it to open that one.

Recognized forms include `src/main.rs:12:5`, `src/main.rs:12`, `./run.sh`, `../x/y.md`,
`~/notes/todo.md`, `/usr/local/bin/xaya` and `main.rs:12`.

- Relative paths are resolved against the directory of the program running in the terminal.
- Text that names no existing file does nothing, so words like `and/or` are harmless.
- Files open in their default application, without the position. To open them at the line and
  column, set your editor:

```toml
[hints]
editor = { program = "code", args = ["-g"] }      # VS Code
# editor = { program = "cursor", args = ["-g"] }  # Cursor
# editor = { program = "zed" }                    # Zed
# editor = { program = "subl" }                   # Sublime Text
```

The editor gets the location as `/absolute/path/src/main.rs:12:5`.

## Split panes

A window splits into panes, each a terminal of its own:

| Action | macOS | Elsewhere |
|---|---|---|
| Split, new pane on the right | `Cmd+D` | `Ctrl+Shift+D` |
| Split, new pane below | `Cmd+Shift+D` | `Ctrl+Shift+Alt+D` |
| Next pane | `Cmd+]` | `Ctrl+Shift+PageDown` |
| Previous pane | `Cmd+[` | `Ctrl+Shift+PageUp` |

- A new pane starts a shell in the focused pane's directory and gets the keyboard.
- Click a pane to focus it. The focused pane's cursor is solid, the others' hollow.
- When a pane's shell exits (or you press `Cmd+W` in it), only that pane closes, its neighbor
  taking the space. Closing the last pane closes the window.
- The window title is the focused pane's. The status badge, Dock count, agent panel and
  `msg status` look at every pane and show the one needing you most.
- A pane too small to split says so in the message bar.

Current limits: panes always split in half; they can't be resized by dragging or zoomed yet.

## Worktree tabs: a checkout per agent

Agents working in the same checkout edit each other's files. `Cmd+Shift+T` (`Ctrl+Shift+T`
elsewhere) does this in one step:

1. creates a git worktree of the current window's repository, checked out from the current
   commit, on a new branch `agent-1`, `agent-2`, … (the first one free);
2. puts it beside the repository in `<repo>.worktrees/<branch>`;
3. opens it in a new tab and starts the agent running in the current window (`claude` or `codex`).

The new tab's title starts with its branch, as in `⎇ agent-2 · …`, also in the panel and
`msg status`. If the worktree can't be created, for example outside a git repository, the reason
shows in the message bar.

**Shared checkout warning**: when agents in several windows or panes work in the same checkout,
XaYa says so once in the message bar and suggests a worktree tab.

Related settings:

```toml
[worktree]
command = "claude"                  # Command started in new tabs; by default the current agent
directory = "/Users/me/worktrees"   # Where worktrees go; by default <repo>.worktrees
warn_shared = true                  # Warn about agents sharing a checkout
```

Worktrees aren't deleted for you. Once merged, remove one with `git worktree remove <path>`.

## Session restore

Windows running an agent when XaYa quits, crashes or is updated come back on the next start:

- each opens in its directory (on macOS, as tabs of one window);
- once its shell is ready, XaYa runs `claude --resume <session>` or `codex resume <session>`;
- windows you closed yourself stay closed.

The first time this happens, windows resuming an agent say so in the message bar and how to
turn it off; later restores stay quiet.

Details:

- Claude Code sessions are matched exactly. Codex doesn't say which session a process runs, so
  XaYa takes the newest one in the window's directory; when two Codex windows share a directory,
  it runs plain `codex resume` so you can pick.
- Starting XaYa with `-e` or `--working-directory` skips restoring.
- Split windows come back as separate tabs; the split layout isn't restored.
- The snapshot is saved every 15 seconds, when windows open or close and on exit, to
  `~/.local/state/xaya/session.json` (following `$XDG_STATE_HOME`).

```toml
[session]
restore = "Agents"   # Default: windows that ran an agent
# restore = "All"    # Every window, in its directory
# restore = "Never"  # Restore nothing, save nothing
```

Session restore is available on Unix systems such as macOS and Linux.

## Scripting with XaYa msg

Commands running inside a XaYa window can use `xaya msg` directly. It addresses that window
(`$ALACRITTY_WINDOW_ID`), or else the focused one; `-w <window id>` picks another. `XaYa msg
status` lists the ids.

| Command | Does |
|---|---|
| `xaya msg status` | List every window's agent status as a table, `*` marking the focused one |
| `xaya msg status --json` | The same as JSON |
| `xaya msg status --wait` | Wait until no agent is working, then print |
| `xaya msg status --watch [--json]` | Keep printing changes, one line each |
| `xaya msg focus -w 42` | Bring window 42 to the front |
| `xaya msg get-text [-w 42]` | Print the window's screen (inside Claude Code or Codex, what the agent shows) |
| `xaya msg get-text --lines 200` | Print the last 200 lines, scrollback included |
| `xaya msg get-text --last-command` | Print the last shell command and its output (needs shell integration) |
| `xaya msg send-text [-w 42] [--enter] "text"` | Type text into the window as a paste, `--enter` pressing Enter after (must be turned on, see below) |
| `xaya msg split [--down] [-c command]` | Split, running a command in the new pane |
| `xaya msg create-worktree [-b branch] [-c command]` | Open a worktree tab, printing its path |

Examples:

```sh
# Tell me when every agent is done
xaya msg status --wait && say "agents are done"

# Follow only agents waiting on me
xaya msg status --watch --json | jq -c 'select(.state == "blocked")'

# Hand a failing test to the agent next door
xaya msg send-text -w 42 --enter "Fix this test failure: $(xaya msg get-text --last-command)"

# Start codex beside the shell
xaya msg split -c codex
```

**`send-text` is off by default**: once on, any program on your computer can give instructions to
your agents. Turn it on in the config file if you need it (`xaya msg config` can't):

```toml
[general]
ipc_send_text = true
```

Each `--watch --json` line is a JSON object: with `"event": "changed"`, the window's status fields
(`window_id`, `title`, `state`, `kind`, `app`, `message`, `progress`, `seconds`); when a window
closes, `{"event": "closed", "window_id": …}`. It reports what XaYa shows: a result in the focused
window counts as seen at once, so it clears rather than being reported as `done`.

The `xaya msg` commands are available on Unix systems such as macOS and Linux.

## Keyboard shortcuts

| Action | macOS | Elsewhere |
|---|---|---|
| Next window needing you | `Cmd+J` | `Ctrl+Shift+J` |
| Show or hide the agent panel | `Cmd+Shift+A` | `Ctrl+Shift+A` |
| Previous/next prompt (needs shell integration) | `Cmd+↑` / `Cmd+↓` | `Ctrl+Shift+↑` / `Ctrl+Shift+↓` |
| Open a file location | `Cmd`-click | `Ctrl`-click |
| Pick a file location by keyboard | `Ctrl+Shift+E` | `Ctrl+Shift+E` |
| Pick a link by keyboard | `Ctrl+Shift+O` | `Ctrl+Shift+O` |
| New worktree tab | `Cmd+Shift+T` | `Ctrl+Shift+T` |
| Split, new pane on the right | `Cmd+D` | `Ctrl+Shift+D` |
| Split, new pane below | `Cmd+Shift+D` | `Ctrl+Shift+Alt+D` |
| Next/previous pane | `Cmd+]` / `Cmd+[` | `Ctrl+Shift+PageDown` / `PageUp` |
| Close the pane (the window with its last one) | `Cmd+W` | `exit` in the shell |
| New tab | `Cmd+T` | — |
| Quit XaYa | `Cmd+Q` | — |

Every shortcut can be changed under `[keyboard]`. The actions are `SelectNextAttention`,
`ToggleAgentPanel`, `ScrollToPreviousPrompt`, `ScrollToNextPrompt`, `CreateWorktreeTab`,
`SplitRight`, `SplitDown`, `FocusNextPane` and `FocusPreviousPane`. For example:

```toml
[keyboard]
bindings = [
  { key = "P", mods = "Command|Shift", action = "ToggleAgentPanel" },
]
```

## Configuration reference

The settings XaYa adds, with their defaults:

```toml
[general]
ipc_send_text = false        # Allow xaya msg send-text

[terminal]
program_status = true        # Show agent status (OSC 7501)

[notifications]
enabled = true
only_when_unfocused = true
command_threshold = 10
# command = { program = "...", args = [...] }  # Custom notifier; title and body are appended

[colors]
minimum_contrast = 1.0       # Lowest contrast for program-chosen colors, such as 4.5; 1.0 is off

[hints]
# editor = { program = "code", args = ["-g"] }

[session]
restore = "Agents"           # "Agents" | "All" | "Never"

[worktree]
# command = "claude"
# directory = "/path/to/worktrees"
warn_shared = true
```

See `man 5 alacritty` for every option.

## Troubleshooting

**No status badge in the title.** Check that `terminal.program_status` isn't off. Claude Code
needs v2.1.295 or newer, Codex rust-v0.163.0 or newer.

**No notifications.** Check `notifications.enabled`, and on macOS that XaYa is allowed in System
Settings → Notifications. By default, nothing is posted while the window is in front.

**No count on the Dock icon.** System Settings → Notifications → XaYa → enable Badges.

**Clicking a file location does nothing.** Check that the file exists (relative paths resolve
against the running program's directory) and that `Cmd` (`Ctrl` elsewhere) is held. If
`hints.editor` is set but nothing opens, XaYa started from the Dock may not find the editor's
command; use its full path, such as `/usr/local/bin/code`.

**A restored window didn't start its agent.** Check `session.restore`, and that XaYa wasn't started
with `-e` or `--working-directory`. A Claude Code session that never got a message has no
conversation to resume, so only its window comes back.

**`send-text` fails with "sending text is off".** Set `general.ipc_send_text = true` in the config
file.

**"There's no room to split this pane".** A pane needs room for about 10 columns or 3 lines;
enlarge the window first.

**A window's result is missing from `msg status`.** A result in the focused window counts as seen
and clears at once; that's intended.
