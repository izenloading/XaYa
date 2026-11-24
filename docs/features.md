# Features

This document gives an overview over XaYa's features beyond its terminal
emulation capabilities. To get a list with supported control sequences take a
look at the [alacritty-escapes(7) manpage].

[alacritty-escapes(7) manpage]: ../extra/man/alacritty-escapes.7.scd

## Vi Mode

The vi mode allows moving around XaYa's viewport and scrollback using the
keyboard. It also serves as a jump-off point for other features like search and
opening URLs with the keyboard. By default you can launch it using
<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Space</kbd>.

### Motion

The cursor motions are setup by default to mimic vi, however they are fully
configurable. If you don't like vi's bindings, take a look at the configuration
file to change the various movements.

### Selection

One useful feature of vi mode is the ability to make selections and copy text to
the clipboard. By default you can start a selection using <kbd>v</kbd> and copy
it using <kbd>y</kbd>. All selection modes that are available with the mouse can
be accessed from vi mode, including the semantic (<kbd>Alt</kbd> <kbd>v</kbd>),
line (<kbd>Shift</kbd> <kbd>v</kbd>) and block selection (<kbd>Ctrl</kbd>
<kbd>v</kbd>). You can also toggle between them while the selection is still
active.

## Search

Search allows you to find anything in XaYa's scrollback buffer. You can
search forward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>f</kbd> (<kbd>Command</kbd> <kbd>f</kbd> on macOS) and
backward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>b</kbd> (<kbd>Command</kbd> <kbd>b</kbd> on macOS).

### Vi Search

In vi mode the search is bound to <kbd>/</kbd> for forward and <kbd>?</kbd> for
backward search. This allows you to move around quickly and help with selecting
content. The `SearchStart` and `SearchEnd` keybinding actions can be bound if
you're looking for a way to jump to the start or the end of a match.

### Normal Search

During normal search you don't have the opportunity to move around freely, but
you can still jump between matches using <kbd>Enter</kbd> and <kbd>Shift</kbd>
<kbd>Enter</kbd>. After leaving search with <kbd>Escape</kbd> your active match
stays selected, allowing you to easily copy it.

## Hints

Terminal hints allow easily interacting with visible text without having to
start vi mode. They consist of a regex that detects these text elements and then
either feeds them to an external application or triggers one of XaYa's
built-in actions.

Hints can also be triggered using the mouse or vi mode cursor. If a hint is
enabled for mouse interaction and recognized as such, it will be underlined when
the mouse or vi mode cursor is on top of it. Using the left mouse button or
<kbd>Enter</kbd> key in vi mode will then trigger the hint.

Hints can be configured in the `hints` and `colors.hints` sections in the
XaYa configuration file.

## Selection expansion

After making a selection, you can use the right mouse button to expand it.
Double-clicking will expand the selection semantically, while triple-clicking
will perform line selection. If you hold <kbd>Ctrl</kbd> while expanding the
selection, it will switch to the block selection mode.

## Opening URLs with the mouse

You can open URLs with your mouse by clicking on them. The modifiers required to
be held and program which should open the URL can be setup in the configuration
file. If an application captures your mouse clicks, which is indicated by a
change in mouse cursor shape, you're required to hold <kbd>Shift</kbd> to bypass
that.

## Split Panes

A window splits into panes, each a terminal of its own: <kbd>Command</kbd>
<kbd>d</kbd> puts a new one on the right of the focused pane and
<kbd>Command</kbd> <kbd>Shift</kbd> <kbd>d</kbd> below it (<kbd>Ctrl</kbd>
<kbd>Shift</kbd> <kbd>d</kbd> and <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Alt</kbd>
<kbd>d</kbd> elsewhere), starting in the same directory. Click a pane, or use
<kbd>Command</kbd> <kbd>]</kbd> and <kbd>[</kbd> (<kbd>Ctrl</kbd>
<kbd>Shift</kbd> <kbd>PageDown</kbd> and <kbd>PageUp</kbd>), to move the
keyboard between them; the window's title and agent status follow the panes.
A pane closes with its shell. `xaya msg split [--down] [-c <command>]` splits
from scripts, such as an agent beside the shell it's working for.

## Multi-Window

XaYa supports running multiple terminal emulators from the same XaYa
instance. New windows can be created either by using the `CreateNewWindow`
keybinding action, or by executing the `xaya msg create-window` subcommand.

## Agents

Claude Code and Codex report whether they're working, waiting on you or done
(`OSC 7501`). XaYa shows it as a badge in the window title, posts a desktop
notification when an agent starts waiting on you or finishes, and on macOS
counts waiting windows on the Dock icon.

The agent panel, toggled with <kbd>Command</kbd> <kbd>Shift</kbd> <kbd>a</kbd>
(<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>a</kbd> elsewhere), lists every window
with what its agents are doing, the ones waiting on you first, along with the
question or permission they wait for. Pick one with the arrow keys and
<kbd>Enter</kbd>, or press its number. <kbd>Command</kbd> <kbd>j</kbd>
(<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>j</kbd>) jumps straight to the next
window needing you, and `xaya msg status` prints the same list.

Each row also shows what the agent's last turn changed in its git checkout,
such as `3 files +48 -3`, and <kbd>d</kbd> opens that turn's diff in a pane
below. XaYa snapshots the working tree when a turn starts and ends, through a
copy of the index, so what you staged is left alone.

File locations agents print, like `src/main.rs:12:5`, open on
<kbd>Command</kbd>-click (<kbd>Ctrl</kbd>-click elsewhere) or through the
<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>e</kbd> hint mode. Set `hints.editor`,
for example to `{ program = "code", args = ["-g"] }`, to open them at their
line in your editor.

Scripts can follow and drive the agents too:

```sh
xaya msg status --watch --json            # one JSON line per change
xaya msg focus -w 42                      # bring window 42 to the front
xaya msg get-text -w 42                   # what the agent shows right now
xaya msg get-text --last-command          # last shell command and its output
xaya msg send-text -w 42 --enter "go on"  # needs general.ipc_send_text = true
```

If XaYa quits, crashes or is updated while agents run, their windows come back
on the next start, each in its directory, resuming its Claude Code or Codex
session. Windows you close yourself stay closed. Set `session.restore` to
`"All"` to bring back every window, or `"Never"` to turn this off.

To keep agents out of each other's way, <kbd>Command</kbd> <kbd>Shift</kbd>
<kbd>t</kbd> (<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>t</kbd> elsewhere) gives
the next one its own checkout: it creates a git worktree of the current
repository on a new `agent-N` branch, next to the repository in
`<repo>.worktrees`, and opens a tab there running the same agent. The tab's
title starts with its branch. `xaya msg create-worktree -b <branch> -c <command>`
does the same from scripts. When agents in several windows work in the same
checkout anyway, XaYa warns about it once.
