ALACRITTY(5)

# NAME

Alacritty - TOML configuration file format.

# SYNTAX

Alacritty's configuration file uses the TOML format. The format's specification
can be found at _https://toml.io/en/v1.1.0_.

# LOCATION

Alacritty doesn't create the config file for you, but it looks for one in the
following locations on UNIX systems:

. _$XDG_CONFIG_HOME/xaya/xaya.toml_
. _$HOME/.config/xaya/xaya.toml_
. _$XDG_CONFIG_HOME/alacritty/alacritty.toml_
. _$XDG_CONFIG_HOME/alacritty.toml_
. _$HOME/.config/alacritty/alacritty.toml_
. _$HOME/.alacritty.toml_
. _/etc/alacritty/alacritty.toml_

On Windows, the config file will be looked for in:

. _%APPDATA%\\xaya\\xaya.toml_
. _%APPDATA%\\alacritty\\alacritty.toml_

# GENERAL

This section documents the *[general]* table of the configuration file.

*import* = [_"<string>"_,]

	Import additional configuration files.

	Imports are loaded in order, skipping all missing files, with the importing
	file being loaded last. If a field is already present in a previous import,
	it will be replaced.

	All imports must either be absolute paths starting with _/_, paths relative
	to the user's home directory starting with _~/_, or paths relative from the
	current config file.

	Example:
		import = [++
	_"~/.config/alacritty/base16-dark.toml"_,++
	_"~/.config/alacritty/keybindings.toml"_,++
	_"alacritty-theme/themes/gruvbox_dark.toml"_,++
]

*working_directory* = _"<string>"_ | _"None"_

	Directory the shell is started in. When this is unset, or _"None"_, the
	working directory of the parent process will be used.

	Default: _"None"_

*live_config_reload* = _true_ | _false_

	Live config reload (changes require restart)

	Default: _true_

*ipc_socket* = _true_ | _false_ # _(unix only)_

	Offer IPC using _alacritty msg_

	Default: _true_

*ipc_send_text* = _true_ | _false_ # _(unix only)_

	Let _xaya msg send-text_ type into windows. Any program running as you
	can then give instructions to the agents in XaYa. Only the configuration
	file turns it on; _msg config_ can't.

	Default: _false_

# ENV

All key-value pairs in the *[env]* section will be added as environment variables
for any process spawned by Alacritty, including its shell. Some entries may
override variables set by alacritty itself.

Example:
	*[env]*++
WINIT_X11_SCALE_FACTOR = _"1.0"_

# WINDOW

This section documents the *[window]* table of the configuration file.

*dimensions* = { columns = _<integer>_, lines = _<integer>_ }

	Window dimensions (changes require restart).

	Number of lines/columns (not pixels) in the terminal. Both lines and columns
	must be non-zero for this to take effect. The number of columns must be at
	least _2_, while using a value of _0_ for columns and lines will fall back
	to the window manager's recommended size

	Default: { columns = _0_, lines = _0_ }

*position* = _"None"_ | { x = _<integer>_, y = _<integer>_ } # _(has no effect on Wayland)_

	Window startup position.

	Specified in number of pixels.

	If the position is _"None"_, the window manager will handle placement.

	Default: _"None"_

*padding* = { x = _<integer>_, y = _<integer>_ }

	Blank space added around the window in pixels. This padding is scaled
	by DPI and the specified value is always added at both opposing sides.

	Default: { x = _0_, y = _0_ }

*dynamic_padding* = _true_ | _false_

	Spread additional padding evenly around the terminal content.

	Default: _false_

*decorations* = _"Full"_ | _"None"_ | _"Transparent"_ | _"Buttonless"_

	Window decorations.

	*Full*
		Borders and title bar.
	*None*
		Neither borders nor title bar.
	*Transparent* _(macOS only)_
		Title bar, transparent background and title bar buttons.
	*Buttonless* _(macOS only)_
		Title bar, transparent background and no title bar buttons.

	Default: _"Full"_

*opacity* = _<float>_

	Background opacity as a floating point number from _0.0_ to _1.0_. The value
	_0.0_ is completely transparent and _1.0_ is opaque.

	Default: _1.0_

*blur* = _true_ | _false_ # _(works on macOS/KDE Wayland)_

	Request compositor to blur content behind transparent windows.

	Default: _false_

*startup_mode* = _"Windowed"_ | _"Maximized"_ | _"Fullscreen"_ | _"SimpleFullscreen"_

	Startup mode (changes require restart)

	*Windowed*
		Regular window.
	*Maximized*
		The window will be maximized on startup.
	*Fullscreen*
		The window will be fullscreened on startup.
	*SimpleFullscreen* _(macOS only)_
		Same as _Fullscreen_, but you can stack windows on top.

	Default: _"Windowed"_

*title* = _"<string>"_

	Window title.

	Default: _"Alacritty"_

*dynamic_title* = _true_ | _false_

	Allow terminal applications to change Alacritty's window title.

	Default: _true_

*class* = { instance = _"<string>"_, general = _"<string>"_ } # _(Linux/BSD only)_

	Window class.

	On Wayland, *general* is used as _app\_id_ and *instance* is ignored.

	Default: { instance = _"Alacritty"_, general = _"Alacritty"_ }

*decorations_theme_variant* = _"Dark"_ | _"Light"_ | _"None"_

	Override the variant of the System theme/GTK theme/Wayland client side
	decorations. Set this to _"None"_ to use the system's default theme variant.

	Default: _"None"_

*resize_increments* = _true_ | _false_ # _(works on macOS/X11/Wayland)_

	Prefer resizing window by discrete steps equal to cell dimensions.

	Default: _false_

*option_as_alt* = _"OnlyLeft"_ | _"OnlyRight"_ | _"Both"_ | _"None"_ # _(macOS only)_

	Make _Option_ key behave as _Alt_.

	Default: _"None"_

*level* = _"Normal"_ | _"AlwaysOnTop"_

	Sets window level.

	*Normal*
		Window adheres to system's default z-order.
	*AlwaysOnTop*
		Window is a toplevel window.

	Default: _"Normal"_

Example:
	*[window]*++
padding = { x = _3_, y = _3_ }++
dynamic_padding = _true_++
opacity = _0.9_


# SCROLLING

This section documents the *[scrolling]* table of the configuration file.

*history* = _<integer>_

	Maximum number of lines in the scrollback buffer.++
Specifying _0_ will disable scrolling.++
Limited to _100000_.

	Default: _10000_

*multiplier* = _<integer>_

	Number of line scrolled for every input scroll increment.

	Default: _3_

# FONT

This section documents the *[font]* table of the configuration file.

*normal* = { family = _"<string>"_, style = _"<string>"_ }

	Default:
		Linux/BSD: { family = _"monospace"_, style = _"Regular"_ }++
Windows:   { family = _"Consolas"_,  style = _"Regular"_ }++
macOS:     { family = _"Menlo"_,     style = _"Regular"_ }

*bold* = { family = _"<string>"_, style = _"<string>"_ }

	If the family is not specified, it will fall back to the value specified for
	the normal font.

	Default: { style = _"Bold"_ }

*italic* = { family = _"<string>"_, style = _"<string>"_ }

	If the family is not specified, it will fall back to the value specified for
	the normal font.

	Default: { style = _"Italic"_ }

*bold_italic* = { family = _"<string>"_, style = _"<string>"_ }

	If the family is not specified, it will fall back to the value specified for
	the normal font.

	Default: { style = _"Bold Italic"_ }

*size* = _<float>_

	Font size in points.

	Default: _11.25_

*offset* = { x = _<integer>_, y = _<integer>_ }

	Offset is the extra space around each character. _y_ can be thought of as
	modifying the line spacing, and _x_ as modifying the letter spacing.

	Default: { x = _0_, y = _0_ }

*glyph_offset* = { x = _<integer>_, y = _<integer>_ }

	Glyph offset determines the locations of the glyphs within their cells with
	the default being at the bottom. Increasing _x_ moves the glyph to the
	right, increasing _y_ moves the glyph upward.

*builtin_box_drawing* = _true_ | _false_

	When _true_, Alacritty will use a custom built-in font for box drawing
	characters (Unicode points _U+2500_ - _U+259F_), legacy computing symbols
	(_U+1FB00_ - _U+1FB3B_), and powerline symbols (_U+E0B0_ - _U+E0B3_).

	Default: _true_

# COLORS

This section documents the *[colors]* table of the configuration file.

Colors are specified using their hexadecimal values with a _#_ prefix: _#RRGGBB_.

*primary*

	This section documents the *[colors.primary]* table of the configuration file.

	*foreground* = _"<string>"_

		Default: _"#d8d8d8"_

	*background* = _"<string>"_

		Default: _"#181818"_

	*dim_foreground* = _"<string>"_

		If this is not set, the color is automatically calculated based on the
		foreground color.

		Default: _"#828482"_

	*bright_foreground* = _"<string>"_

		This color is only used when _draw\_bold\_text\_with\_bright\_colors_ is
		_true_.

		If this is not set, the normal foreground will be used.

		Default: _"None"_

*cursor* = { text = _"<string>"_, cursor = _"<string>"_ }

	Colors which should be used to draw the terminal cursor.

	Allowed values are hexadecimal colors like _#ff00ff_, or
	_CellForeground_/_CellBackground_, which references the affected cell.

	Default: { text = _"CellBackground"_, cursor = _"CellForeground"_ }

*vi_mode_cursor* = { text = _"<string>"_, cursor = _"<string>"_ }

	Colors for the cursor when the vi mode is active.

	Allowed values are hexadecimal colors like _#ff00ff_, or
	_CellForeground_/_CellBackground_, which references the affected cell.

	Default: { text = _"CellBackground"_, cursor = _"CellForeground"_ }

*search*

	This section documents the *[colors.search]* table of the configuration.

	Allowed values are hexadecimal colors like _#ff00ff_, or
	_CellForeground_/_CellBackground_, which references the affected cell.

	*matches* = { foreground = _"<string>"_, background = _"<string>"_ }

		Default: { foreground = _"#181818"_, background = _"#ac4242"_ }

	*focused_match* = { foreground = _"<string>"_, background = _"<string>"_ }

		Default: { foreground = _"#181818"_, background = _"#f4bf75"_ }

*hints*

	This section documents the *[colors.hints]* table of the configuration.

	*start* = { foreground = _"<string>"_, background = _"<string>"_ }

		First character in the hint label.

		Allowed values are hexadecimal colors like _#ff00ff_, or
		_CellForeground_/_CellBackground_, which references the affected cell.

		Default: { foreground = _"#181818"_, background = _"#f4bf75"_ }

	*end* = { foreground = _"<string>"_, background = _"<string>"_ }

		All characters after the first one in the hint label.

		Allowed values are hexadecimal colors like _#ff00ff_, or
		_CellForeground_/_CellBackground_, which references the affected cell.

		Default: { foreground = _"#181818"_, background = _"#ac4242"_ }

*line_indicator* = { foreground = _"<string>"_, background = _"<string>"_ }

	Color used for the indicator displaying the position in history during
	search and vi mode.

	Setting this to _"None"_ will use the opposing primary color.

	Default: { foreground = _"None"_, background = _"None"_ }

*footer_bar* = { foreground = _"<string>"_, background = _"<string>"_ }

	Color used for the footer bar on the bottom, used by search regex input,
	hyperlink URI preview, etc.

	Default: { foreground = _"#181818"_, background = _"#d8d8d8"_ }

*selection* = { text = _"<string>"_, background = _"<string>"_ }

	Colors used for drawing selections.

	Allowed values are hexadecimal colors like _#ff00ff_, or
	_CellForeground_/_CellBackground_, which references the affected cell.

	Default: { text = _"CellBackground"_, background = _"CellForeground"_ }

*normal*

	This section documents the *[colors.normal]* table of the configuration.

	*black* = _"<string>"_
		Default: _"#181818"_
	*red* = _"<string>"_
		Default: _"#ac4242"_
	*green* = _"<string>"_
		Default: _"#90a959"_
	*yellow* = _"<string>"_
		Default: _"#f4bf75"_
	*blue* = _"<string>"_
		Default: _"#6a9fb5"_
	*magenta* = _"<string>"_
		Default: _"#aa759f"_
	*cyan* = _"<string>"_
		Default: _"#75b5aa"_
	*white* = _"<string>"_
		Default: _"#d8d8d8"_

*bright*

	This section documents the *[colors.bright]* table of the configuration.

	*black* = _"<string>"_
		Default: _"#6b6b6b"_
	*red* = _"<string>"_
		Default: _"#c55555"_
	*green* = _"<string>"_
		Default: _"#aac474"_
	*yellow* = _"<string>"_
		Default: _"#feca88"_
	*blue* = _"<string>"_
		Default: _"#82b8c8"_
	*magenta* = _"<string>"_
		Default: _"#c28cb8"_
	*cyan* = _"<string>"_
		Default: _"#93d3c3"_
	*white* = _"<string>"_
		Default: _"#f8f8f8"_

*dim*

	This section documents the *[colors.dim]* table of the configuration.

	If the dim colors are not set, they will be calculated automatically based
	on the _normal_ colors.

	*black* = _"<string>"_
		Default: _"#0f0f0f"_
	*red* = _"<string>"_
		Default: _"#712b2b"_
	*green* = _"<string>"_
		Default: _"#5f6f3a"_
	*yellow* = _"<string>"_
		Default: _"#a17e4d"_
	*blue* = _"<string>"_
		Default: _"#456877"_
	*magenta* = _"<string>"_
		Default: _"#704d68"_
	*cyan* = _"<string>"_
		Default: _"#4d7770"_
	*white* = _"<string>"_
		Default: _"#8e8e8e"_

*indexed_colors* = [{ index = _<integer>_, color = _"<string>"_ },]

	The indexed colors include all colors from 16 to 256.
	When these are not set, they're filled with sensible defaults.

	Default: _[]_

*transparent_background_colors* = _true_ | _false_

	Whether or not _window.opacity_ applies to all cell backgrounds, or only to
	the default background. When set to _true_ all cells will be transparent
	regardless of their background color.

	Default: _false_

*draw_bold_text_with_bright_colors* = _true_ | _false_

	When _true_, bold text is drawn using the bright color variants.

	Default: _false_

*minimum_contrast* = _<float>_

	Lowest WCAG contrast ratio between text and its background. Text whose
	colors were picked by the running program is brightened or darkened, keeping
	its hue, until it reaches this ratio. The default foreground on the default
	background, selections, hints and search matches are left untouched, as is
	text whose foreground equals its background.

	Values range from _1.0_ (disabled) to _21.0_ (always black or white); _3.0_
	is a good starting point for readable low-contrast program colors.

	Default: _1.0_

# BELL

This section documents the *[bell]* table of the configuration file.

*animation* = _"Ease"_ | _"EaseOut"_ | _"EaseOutSine"_ | _"EaseOutQuad"_ | _"EaseOutCubic"_
\| _"EaseOutQuart"_ | _"EaseOutQuint"_ | _"EaseOutExpo"_ | _"EaseOutCirc"_ | _"Linear"_

	Visual bell animation effect for flashing the screen when the visual bell is rung.

	Default: _"Linear"_

*duration* = _<integer>_

	Duration of the visual bell flash in milliseconds. A `duration` of `0` will
	disable the visual bell animation.

	Default: _0_

*color* = _"<string>"_

	Visual bell animation color.

	Default: _"#ffffff"_

*command* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

	This program is executed whenever the bell is rung.

	When set to _"None"_, no command will be executed.

	Default: _"None"_

# SELECTION

This section documents the *[selection]* table of the configuration file.

*semantic_escape_chars* = _"<string>"_

	This string contains all characters that are used as separators for
	"semantic words" in Alacritty.

	Default: _",│`|:\\"' ()[]{}<>\\t"_

*save_to_clipboard* = _true_ | _false_

	When set to _true_, selected text will be copied to the primary clipboard.

	Default: _false_

# CURSOR

This section documents the *[cursor]* table of the configuration file.

*style* = { *<shape>*, *<blinking>* }

	*shape* = _"Block"_ | _"Underline"_ | _"Beam"_

		Default: _"Block"_

	*blinking* = _"Never"_ | _"Off"_ | _"On"_ | _"Always"_

		*Never*
			Prevent the cursor from ever blinking
		*Off*
			Disable blinking by default
		*On*
			Enable blinking by default
		*Always*
			Force the cursor to always blink

		Default: _"Off"_

*vi_mode_style* = { *<shape>*, *<blinking>* } | _"None"_

	If the vi mode cursor style is _"None"_ or not specified, it will fall back to
	the active value of the normal cursor.

	Default: _"None"_

*blink_interval* = _<integer>_

	Cursor blinking interval in milliseconds.

	Default: _750_

*blink_timeout* = _<integer>_

	Time after which cursor stops blinking, in seconds.

	Specifying _0_ will disable timeout for blinking.

	Default: _5_

*unfocused_hollow* = _true_ | _false_

	When this is _true_, the cursor will be rendered as a hollow box when the
	window is not focused.

	Default: _true_

*thickness* = _<float>_

	Thickness of the cursor relative to the cell width as floating point number
	from _0.0_ to _1.0_.

	Default: _0.15_

# TERMINAL

This section documents the *[terminal]* table of the configuration file.

*shell* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

	You can set _shell.program_ to the path of your favorite shell, e.g.
	_/bin/zsh_. Entries in _shell.args_ are passed as arguments to the shell.

	Default:
		Linux/BSD/macOS: _$SHELL_ or the user's login shell, if _$SHELL_ is unset++
Windows: _"powershell"_

	Example:
		*shell* = { program = _"/bin/zsh"_, args = [_"-l"_] }

*osc52* = _"Disabled"_ | _"OnlyCopy"_ | _"OnlyPaste"_ | _"CopyPaste"_

	Controls the ability to write to the system clipboard with the _OSC 52_
	escape sequence. While this escape sequence is useful to copy contents
	from the remote server, allowing any application to read from the clipboard
	can be easily abused while not providing significant benefits over
	explicitly pasting text.

	Default: _"OnlyCopy"_

*program_status* = _true_ | _false_

	Support the Program Status Protocol (_OSC 7501_), through which agents such
	as Claude Code and Codex report whether they're working, waiting on you or
	done.

	The window title is prefixed with the status: ⏳ while working (with its
	progress when reported), 🔔 while waiting on you, ✅ once done and ❌ on
	failure. Results stay until the window is focused. What was still running
	ends at a new shell prompt (_OSC 133 ; A_) or, on unix, once another job
	takes the terminal's foreground, such as the shell after a program was
	killed.

	On macOS, the Dock icon shows how many windows have a program waiting on
	you or a failure you haven't seen. macOS hides it unless _Badge
	application icon_ is on for XaYa in System Settings → Notifications.

	A desktop notification is shown when a program starts waiting on you,
	fails, or finishes after working, following the *[notifications]*
	settings. While a window shows a status, its _OSC 9_ and _OSC 777_
	notifications are dropped, since the program announces the same moments
	through them.

	Claude Code answers the probe and Codex reports unprompted, so both work
	without further setup.

	Default: _true_

*turn_changes* = _true_ | _false_

	Show what each turn of an agent changed in its git checkout. A turn runs
	from the agent reporting it works until it's done, failed or idle again
	(see _program_status_). At both ends XaYa snapshots the working tree as a
	git tree, through a copy of the checkout's index, so what you staged is
	left alone; ignored files don't count.

	The agent panel shows the last turn's changes, such as _3 files +48 -3_,
	and _d_ opens their diff in a pane below. _msg status --json_ reports them
	as _changes_, with the two trees for _git diff_. A turn that changed
	nothing clears them.

	Everything writing to the checkout during the turn counts as the agent's,
	and edits made before the first snapshot finished count as earlier ones.

	Default: _true_

# MOUSE

This section documents the *[mouse]* table of the configuration file.

*hide_when_typing* = _true_ | _false_

	When this is _true_, the cursor is temporarily hidden when typing.

	Default: _false_

*bindings* = [{ *<mouse>*, *<mods>*, *<mode>*, *<command>* | *<chars>* | *<action>* },]

	See _keyboard.bindings_ for full documentation on _mods_, _mode_, _command_,
	_chars_, and _action_.

	When an application running within Alacritty captures the mouse, the `Shift`
	modifier can be used to suppress mouse reporting. If no action is found for
	the event, actions for the event without the `Shift` modifier are triggered
	instead.

	*mouse* = _"Middle"_ | _"Left"_ | _"Right"_ | _"Back"_ | _"Forward"_ | _"WheelUp"_ | _"WheelDown"_ | _<integer>_

		Mouse button which needs to be pressed to trigger this binding.

	*action* = *<keyboard.bindings.action>* | _"ExpandSelection"_

		*ExpandSelection*
			Expand the selection to the current mouse cursor location.

	Example:
		*[mouse]*++
bindings = [++
	{ mouse = _"Right"_, mods = _"Control"_, action = _"Paste"_ },++
]

# NOTIFICATIONS

This section documents the *[notifications]* table of the configuration file.

Programs can request desktop notifications with _OSC 9 ; <body> ST_ or
_OSC 777 ; notify ; <title> ; <body> ST_. ConEmu's numbered _OSC 9_
sub-commands, such as progress reports, are not treated as notifications. At
most one notification per window is shown each second.

*enabled* = _true_ | _false_

	Whether programs may post desktop notifications.

	Default: _true_

*only_when_unfocused* = _true_ | _false_

	Only show notifications while the window is unfocused. Notifications shown
	while unfocused also set the window's urgency hint.

	Default: _true_

*command* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

	Program used to show a notification. The title and body are appended as the
	last two arguments; the title falls back to _"XaYa"_ when the program only
	sent a body.

	When unset, macOS posts native notifications carrying XaYa's icon; the first
	one asks for permission, which can later be changed in System Settings →
	Notifications. A binary run outside of _XaYa.app_ can't post native
	notifications and uses _osascript_ instead, whose notifications show Script
	Editor's icon. Clicking a native notification brings the window or tab
	that posted it to the front. Other unix systems use _notify-send_ with the
	_XaYa_ icon and desktop entry. Windows has no default notifier.

	Default: _None_

*command_threshold* = _<integer>_

	Seconds a shell command must run for its finish to be reported, which
	needs shell integration marking prompts and commands with _OSC 133_: source
	_xaya.zsh_, _xaya.bash_ or _xaya.fish_ from
	_XaYa.app/Contents/Resources/shell-integration_ (_extra/shell-integration_
	in the source tree) at the end of your shell's startup file. The
	window title then shows ✅, or ❌ when the command failed, until the window
	is focused, and a notification says how long it ran. Commands that report
	their own status through _OSC 7501_, such as agents, aren't reported again.
	_0_ never reports commands.

	Default: _10_

# SESSION

This section documents the *[session]* table of the configuration file.

While XaYa runs, what's open in its windows is saved to
_$XDG_STATE_HOME/xaya/session.json_ (by default under _~/.local/state_). Windows
you close leave it; windows still open when XaYa stops, because it quit,
crashed or was updated, open again in their directory on the next start. Those
that ran Claude Code or Codex resume the session: once the shell is ready,
XaYa types _claude --resume <session>_ or _codex resume <session>_ and presses
Enter. Starting XaYa with a command or working directory skips this.

The first time a session is restored, windows resuming an agent explain it in
the message bar; _restore-notice-shown_ beside the snapshot remembers that.

Claude Code sessions are found exactly. Codex doesn't say which session a
process runs, so the newest one in the window's directory is taken; when two
Codex windows share a directory, Codex asks which session to resume.

*restore* = _"Agents"_ | _"All"_ | _"Never"_ # _(unix only)_

	Which windows to open again on start. With _"Agents"_, only those running
	Claude Code or Codex; with _"All"_, every window, in its directory; with
	_"Never"_, none, and nothing is saved.

	Default: _"Agents"_

# WORKTREE

This section documents the *[worktree]* table of the configuration file.

Agents working in the same checkout edit the same files. _CreateWorktreeTab_
and _xaya msg create-worktree_ give an agent its own checkout instead: a git
worktree of the repository on its own branch, checked out from the current
commit, in a tab whose title starts with the branch, such as _⎇ agent-2_.

*command* = _"<string>"_ # _(unix only)_

	Command started in a new worktree tab, typed into its shell.

	Default: the agent running in the window the tab is opened from (_claude_
	or _codex_), or none

*directory* = _"<string>"_ # _(unix only)_

	Directory new worktrees go to, each in a subdirectory named after its
	branch.

	Default: _<repo>.worktrees_ beside the repository

*warn_shared* = _true_ | _false_ # _(unix only)_

	Warn in the message bar when agents in several windows work in the same
	checkout. Each time they start sharing one, the warning shows once.

	Default: _true_

# HINTS

This section documents the *[hints]* table of the configuration file.

Terminal hints can be used to find text or hyperlinks in the visible part of the
terminal and pipe it to other applications.

*alphabet* = _"<string>"_

	Keys used for the hint labels.

	Default: _"jfkdls;ahgurieowpq"_

*editor* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

	Editor the _OpenFile_ hint action opens files in. The file's absolute path
	is appended as the last argument, followed by _:<line>_ and _:<column>_
	when the hint names them, as in _src/main.rs:12:5_. This is the form VS
	Code (with _-g_), Cursor (with _-g_), Zed and Sublime Text accept.

	Without an editor, files open in the system's default application for
	them, without a position.

	Example: _{ program = "code", args = ["-g"] }_

	Default: _None_

*enabled* = [{ *<regex>*, *<hyperlinks>*, *<post_processing>*, *<persist>*, *<action>*, *<command>*, *<binding>*, *<mouse>* },]

Array with all available hints.

Each hint must have at least one of _regex_ or _hyperlinks_ and either an
_action_ or a _command_.

	*regex* = _"<string>"_

		Regex each line will be compared against.

	*hyperlinks* = _true_ | _false_

		When this is _true_, all OSC 8 escape sequence hyperlinks will be
		included in the hints.

	*post_processing* = _true_ | _false_

		When this is _true_, heuristics will be used to shorten the match if
		there are characters likely not to be part of the hint (e.g. a trailing
		_._). This is most useful for URIs and applies only to _regex_ matches.

	*persist* = _true_ | _false_

		When this is _true_, hints remain persistent after selection.

	*action* = _"Copy"_ | _"Paste"_ | _"Select"_ | _"MoveViModeCursor"_ | _"OpenFile"_

		*Copy*
			Copy the hint's text to the clipboard.
		*Paste*
			Paste the hint's text to the terminal or search.
		*Select*
			Select the hint's text.
		*MoveViModeCursor*
			Move the vi mode cursor to the beginning of the hint.
		*OpenFile*
			Open the hint's _path[:line[:column]]_ in _editor_. Relative paths
			are resolved against the working directory of the terminal's
			foreground program; text naming no existing file is ignored.

	*command* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

		Command which will be executed when the hint is clicked or selected with
		the _binding_.

		The hint's text is always attached as the last argument.

	*binding* = { key = _"<string>"_, mods = _"<string>"_, mode = _"<string>"_ }

		See _keyboard.bindings_ for documentation on available values.

		This controls which key binding is used to start the keyboard hint
		selection process.

	*mouse* = { mods = _"<string>"_, enabled = _true_ | _false_ }

		See _keyboard.bindings_ for documentation on available _mods_.

		The _enabled_ field controls if the hint should be underlined when
		hovering over the hint text with all _mods_ pressed.

		While a program captures the mouse, _Shift_ must be held as well, unless
		_mods_ include _Command_: mouse reports can't carry it, so those clicks
		go to the hint instead of the program.

	Default:
		*[[hints.enabled]]*++
command         = _"xdg-open"_ # On Linux/BSD++
# command       = _"open"_ # On macOS++
# command       = { program = _"cmd"_, args = [ _"/c"_, _"start"_, _""_ ] } # On Windows++
hyperlinks      = _true_++
post_processing = _true_++
persist         = _false_++
mouse.enabled   = _true_++
binding         = { key = _"O"_, mods = _"Control|Shift"_ }++
regex =
_"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file:|git://|ssh:|ftp://)[^\\u0000-\\u001F\\u007F-\\u009F<>\\"\\\\s{-}\\\\^⟨⟩`\\\\\\\\]+"_

	*[[hints.enabled]]*++
action          = _"OpenFile"_++
hyperlinks      = _false_++
post_processing = _false_++
persist         = _false_++
mouse           = { enabled = _true_, mods = _"Command"_ } # _"Control"_ off macOS++
binding         = { key = _"E"_, mods = _"Control|Shift"_ }++
regex           = File locations such as _src/main.rs:12:5_, _./run.sh_ or _main.rs:12_

# KEYBOARD

This section documents the *[keyboard]* table of the configuration file.

*bindings* = [{ *<key>*, *<mods>*, *<mode>*, *<command>* | *<chars>* | *<action>* },]

	To unset a default binding, you can use the action _"ReceiveChar"_ to remove
	it or _"None"_ to inhibit any action.

	Multiple keybindings can be triggered by a single key press and will be
	executed in the order they are defined in.

	*key* = _"<string>"_

		The regular keys like _"A"_, _"0"_, and _"Я"_ can be mapped directly
		without any special syntax. Full list of named keys like _"F1"_ and the
		syntax for dead keys can be found here:

		_https://docs.rs/winit/latest/winit/keyboard/enum.NamedKey.html_++
_https://docs.rs/winit/latest/winit/keyboard/enum.Key.html#variant.Dead_

		Numpad keys are prefixed by _Numpad_: _"NumpadEnter"_ | _"NumpadAdd"_ |
		_"NumpadComma"_ | _"NumpadDecimal"_ | _"NumpadDivide"_ | _"NumpadEquals"_ |
		_"NumpadSubtract"_ | _"NumpadMultiply"_ | _"Numpad[0-9]"_.

		The _key_ field also supports using scancodes, which are specified as a
		decimal number.

	*mods* = _"Command"_ | _"Control"_ | _"Option"_ | _"Super"_ | _"Shift"_ | _"Alt"_

		Multiple modifiers can be combined using _|_, like this: _"Control |
		Shift"_.

	*mode* = _"AppCursor"_ | _"AppKeypad"_ | _"Search"_ | _"Alt"_ | _"Vi"_

		This defines a terminal mode which must be active for this binding to
		have an effect.

		Prepending _~_ to a mode will require the mode to *not* = be active for
		the binding to take effect.

		Multiple modes can be combined using _|_, like this: _"~Vi|Search"_.

	*command* = _"<string>"_ | { program = _"<string>"_, args = [_"<string>"_,] }

		Fork and execute the specified command.

	*chars* = _"<string>"_

		Writes the specified string to the terminal.

	*action*

		*ReceiveChar*
			Allow receiving char input.
		*None*
			No action.
		*Paste*
			Paste contents of system clipboard.
		*Copy*
			Store current selection into clipboard.
		*IncreaseFontSize*
			Increase font size.
		*DecreaseFontSize*
			Decrease font size.
		*ResetFontSize*
			Reset font size to the config value.
		*ScrollPageUp*
			Scroll exactly one page up.
		*ScrollPageDown*
			Scroll exactly one page down.
		*ScrollHalfPageUp*
			Scroll half a page up.
		*ScrollHalfPageDown*
			Scroll half a page down.
		*ScrollLineUp*
			Scroll one line up.
		*ScrollLineDown*
			Scroll one line down.
		*ScrollToPreviousPrompt*
			Scroll the previous shell prompt to the top of the screen. Needs shell
			integration marking prompts with _OSC 133 ; A_ and _B_.
		*ScrollToNextPrompt*
			Scroll the next shell prompt to the top of the screen, or back to the
			bottom past the last one.
		*ScrollToTop*
			Scroll all the way to the top.
		*ScrollToBottom*
			Scroll all the way to the bottom.
		*ClearHistory*
			Clear the display buffer(s) to remove history.
		*Hide*
			Hide the Alacritty window.
		*Minimize*
			Minimize the Alacritty window.
		*Quit*
			Quit Alacritty.
		*ClearLogNotice*
			Clear warning and error notices.
		*SpawnNewInstance*
			Spawn a new instance of Alacritty.
		*CreateNewWindow*
			Create a new Alacritty window.
		*SelectNextAttention*
			Select the next window or tab whose program waits on you, failed, or
			finished unseen, the most urgent and longest waiting first. Pressed
			again, it cycles through them. Needs _terminal.program_status_.
		*ToggleAgentPanel*
			Show or hide the agent panel over the terminal: every window with
			what its agents report, those waiting on you first. Arrows, _j_/_k_
			or _Tab_ select a window, _Enter_ or its number goes to it, _d_
			opens the diff of its agent's last turn (see
			_terminal.turn_changes_) in a pane below, and _Escape_ or _q_ closes
			the panel. Needs _terminal.program_status_.
		*SplitRight*
			Split the focused pane, starting a shell in its directory on its
			right. Each pane is a terminal of its own; closing its shell, or
			_Quit_ in it, closes only the pane until it's the window's last.
		*SplitDown*
			Split the focused pane, starting a shell below it.
		*FocusNextPane*
			Move the keyboard to the next pane, left to right and top to bottom.
			Clicking a pane focuses it too.
		*FocusPreviousPane*
			Move the keyboard to the previous pane.
		*CreateWorktreeTab* _(unix only)_
			Create a git worktree of the current window's repository on a new
			_agent-N_ branch, open a tab in it and start _worktree.command_, or
			else the agent running in the current window. See *WORKTREE*.
		*ToggleFullscreen*
			Toggle fullscreen.
		*ToggleMaximized*
			Toggle maximized.
		*ClearSelection*
			Clear active selection.
		*ToggleViMode*
			Toggle vi mode.
		*SearchForward*
			Start a forward buffer search.
		*SearchBackward*
			Start a backward buffer search.

		_Vi mode actions:_

		*Up*
			Move up.
		*Down*
			Move down.
		*Left*
			Move left.
		*Right*
			Move right.
		*First*
			First column, or beginning of the line when already at the first column.
		*Last*
			Last column, or beginning of the line when already at the last column.
		*FirstOccupied*
			First non-empty cell in this terminal row, or first non-empty cell
			of the line when already at the first cell of the row.
		*High*
			Move to top of screen.
		*Middle*
			Move to center of screen.
		*Low*
			Move to bottom of screen.
		*SemanticLeft*
			Move to start of semantically separated word.
		*SemanticRight*
			Move to start of next semantically separated word.
		*SemanticLeftEnd*
			Move to end of previous semantically separated word.
		*SemanticRightEnd*
			Move to end of semantically separated word.
		*WordLeft*
			Move to start of whitespace separated word.
		*WordRight*
			Move to start of next whitespace separated word.
		*WordLeftEnd*
			Move to end of previous whitespace separated word.
		*WordRightEnd*
			Move to end of whitespace separated word.
		*Bracket*
			Move to opposing bracket.
		*ParagraphUp*
			Move above the current paragraph.
		*ParagraphDown*
			Move below the current paragraph.
		*ToggleNormalSelection*
			Toggle normal vi selection.
		*ToggleLineSelection*
			Toggle line vi selection.
		*ToggleBlockSelection*
			Toggle block vi selection.
		*ToggleSemanticSelection*
			Toggle semantic vi selection.
		*SearchNext*
			Jump to the beginning of the next match.
		*SearchPrevious*
			Jump to the beginning of the previous match.
		*SearchStart*
			Jump to the next start of a match to the left of the origin.
		*SearchEnd*
			Jump to the next end of a match to the right of the origin.
		*Open*
			Launch the URL below the vi mode cursor.
		*CenterAroundViCursor*
			Centers the screen around the vi mode cursor.
		*InlineSearchForward*
			Search forward within the current line.
		*InlineSearchBackward*
			Search backward within the current line.
		*InlineSearchForwardShort*
			Search forward within the current line, stopping just short of the character.
		*InlineSearchBackwardShort*
			Search backward within the current line, stopping just short of the character.
		*InlineSearchNext*
			Jump to the next inline search match.
		*InlineSearchPrevious*
			Jump to the previous inline search match.
		*SemanticSearchForward*
			Search forward for selection or word under the cursor.
		*SemanticSearchBackward*
			Search backward for selection or word under the cursor.

		_Search actions:_

		*SearchFocusNext*
			Move the focus to the next search match.
		*SearchFocusPrevious*
			Move the focus to the previous search match.
		*SearchConfirm*
			Confirm the active search.
		*SearchCancel*
			Cancel the active search.
		*SearchClear*
			Reset the search regex.
		*SearchDeleteWord*
			Delete the last word in the search regex.
		*SearchHistoryPrevious*
			Go to the previous regex in the search history.
		*SearchHistoryNext*
			Go to the next regex in the search history.

		_macOS exclusive:_

		*ToggleSimpleFullscreen*
			Enter fullscreen without occupying another space.
		*HideOtherApplications*
			Hide all windows other than Alacritty.
		*CreateNewTab*
			Create new window in a tab.
		*SelectNextTab*
			Select next tab.
		*SelectPreviousTab*
			Select previous tab.
		*SelectTab1*
			Select the first tab.
		*SelectTab2*
			Select the second tab.
		*SelectTab3*
			Select the third tab.
		*SelectTab4*
			Select the fourth tab.
		*SelectTab5*
			Select the fifth tab.
		*SelectTab6*
			Select the sixth tab.
		*SelectTab7*
			Select the seventh tab.
		*SelectTab8*
			Select the eighth tab.
		*SelectTab9*
			Select the ninth tab.
		*SelectLastTab*
			Select the last tab.

		_Linux/BSD exclusive:_

		*CopySelection*
			Copy from the selection buffer.
		*PasteSelection*
			Paste from the selection buffer.

Default: See *alacritty-bindings*(5)

Example:
	*[keyboard]*++
bindings = [++
	{ key = _"N"_, mods = _"Control|Shift"_, action = _"CreateNewWindow"_ },++
	{ key = _"L"_, mods = _"Control|Shift"_, chars = _"l"_ },++
]


# DEBUG

This section documents the *[debug]* table of the configuration file.

Debug options are meant to help troubleshoot issues with Alacritty. These can
change or be removed entirely without warning, so their stability shouldn't be
relied upon.

*render_timer* = _true_ | _false_

	Display the time it takes to draw each frame.

	Default: _false_

*persistent_logging* = _true_ | _false_

	Keep the log file after quitting Alacritty.

	Default: _false_

*log_level* = _"Off"_ | _"Error"_ | _"Warn"_ | _"Info"_ | _"Debug"_ | _"Trace"_

	Default: _"Warn"_

	To add extra libraries to logging _ALACRITTY_EXTRA_LOG_TARGETS_ variable
	can be used.

	Example:
		_ALACRITTY_EXTRA_LOG_TARGETS="winit;vte" alacritty -vvv_

*renderer* = _"metal"_ | _"glsl3"_ | _"gles2"_ | _"gles2pure"_ | _"None"_

	Force use of a specific renderer. _"metal"_ is only available on macOS,
	where it is also used for _"None"_, falling back to OpenGL when Metal is
	unavailable. Elsewhere _"None"_ uses the highest available OpenGL renderer.

	Default: _"None"_

*print_events* = _true_ | _false_

	Log all received window events.

	Default: _false_

*highlight_damage* = _true_ | _false_

	Highlight window damage information.

	Default: _false_

*prefer_egl* = _true_ | _false_

	Use EGL as display API if the current platform allows it. Note that
	transparency may not work with EGL on Linux/BSD.

	Default: _false_

# SEE ALSO

*alacritty*(1), *alacritty-msg*(1), *alacritty-bindings*(5), *alacritty-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/izenloading/XaYa/issues_.

# MAINTAINERS

- izenloading

XaYa is a fork of Alacritty. See NOTICE for upstream attribution.
