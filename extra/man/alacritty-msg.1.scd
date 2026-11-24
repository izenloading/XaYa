ALACRITTY-MSG(1)

# NAME

alacritty-msg - Send messages to Alacritty.

# SYNOPSIS

This command communicates with running Alacritty instances through a socket,
making it possible to control Alacritty without directly accessing it.

# OPTIONS

*-s, --socket* _<SOCKET>_

	Path for IPC socket communication.

# MESSAGES

*create-window*

	Create a new window in the same Alacritty process.

	*FLAGS*
		*--hold*

			Remain open after child process exits.

	*OPTIONS*
		*--working-directory* _<WORKING_DIRECTORY>_

			Start the shell in the specified working directory.

		*-T, --title* _<TITLE>_

			Defines the window title.

			Default: _Alacritty_

		*--class* _<GENERAL>_ | _<GENERAL>_,_<INSTANCE>_

			Defines window class/app_id on X11/Wayland.

			Default: _Alacritty,Alacritty_

		*-o, --option* _<OPTION>..._

			Override configuration file options.

			Example: _alacritty msg create-window -o 'cursor.style="Beam"'_

		*-e, --command* _<COMMAND>..._

			Command and args to execute (must be last argument).

*config*

	Update the Alacritty configuration.

	*ARGS*
		*<CONFIG_OPTIONS>...*

			Configuration file options.

			Example: _alacritty msg config 'cursor.style="Beam"'_

	*FLAGS*
		*-r, --reset*

			Clear all runtime configuration changes.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the new config.

			Use _-1_ to apply this change to all windows.

			Default: _$ALACRITTY_WINDOW_ID_

*get-config*

	Read runtime Alacritty configuration.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the config request.

			Use _-1_ to get the global config.

			Default: _$ALACRITTY_WINDOW_ID_

*status*

	Show what the agents in each window are doing, as reported through
	_terminal.program_status_. Windows are listed the way _SelectNextAttention_
	visits them: waiting on you, failed, finished, then working, longest
	waiting first. The focused window is marked with _\*_, and a program's
	message follows on its own line.

	*FLAGS*
		*--json*

			Print a JSON array instead of a table, with the fields
			_window_id_, _title_, _focused_, _state_ (_working_, _blocked_,
			_done_, _error_ or _idle_), _kind_ (_permission_, _question_ or
			_auth_), _app_, _message_, _progress_ and _seconds_ in the state.
			Missing values are _null_.

		*--wait*

			Wait until no window has a program working, then print. Programs
			waiting on you count as not working.

			Example: _xaya msg status --wait && say "agents are done"_

		*--watch*

			Keep running and print a line whenever a window's program reports
			something new, starting with every window as it is. With _--json_,
			each line is a JSON object: the fields above with _event_ set to
			_changed_, or _{"event": "closed", "window_id": ...}_ when a window
			closes. Ticking durations, focus and title changes aren't reported.
			Like the list, it shows what XaYa shows: a result in the focused
			window is seen at once, so it clears instead of being reported.

			Example: _xaya msg status --watch --json | jq -c 'select(.state == "blocked")'_

*focus*

	Bring a window to the front.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID, as _msg status_ lists it.

			Default: _$ALACRITTY_WINDOW_ID_, or the focused window

*get-text*

	Print a window's text: by default its screen as the program drew it,
	regardless of where the view is scrolled. Inside Claude Code and Codex,
	which draw in the alternate screen, that's what the agent shows.

	*FLAGS*
		*--last-command*

			The last shell command before the current prompt and its output.
			Needs shell integration (_OSC 133_) in that window.

	*OPTIONS*
		*--lines* _<LINES>_

			The last _LINES_ rows with content, scrollback included.

		*-w, --window-id* _<WINDOW_ID>_

			Default: _$ALACRITTY_WINDOW_ID_, or the focused window

*send-text*

	Type text into a window the way pasting it would, as a bracketed paste
	when the program asked for one. Off unless _general.ipc_send_text_ is set
	in the configuration file, since it lets any of your programs give
	instructions to the agents running in XaYa.

	*ARGS*
		*<TEXT>*

			Text to send.

	*FLAGS*
		*--enter*

			Press Enter after the text, submitting it.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Default: _$ALACRITTY_WINDOW_ID_, or the focused window

			Example: _xaya msg send-text -w 42 --enter "run the tests again"_

*split*

	Split a window's focused pane, starting a shell in its directory beside it.
	The new pane gets the focus, so _get-text_ and _send-text_ address it next.

	*FLAGS*
		*-d, --down*

			Put the new pane below the focused one, rather than on its right.

	*OPTIONS*
		*-c, --command* _<COMMAND>_

			Command typed into the new pane's shell once it's ready.

		*-w, --window-id* _<WINDOW_ID>_

			Default: _$ALACRITTY_WINDOW_ID_, or the focused window

			Example: _xaya msg split -c "codex"_

*create-worktree*

	Create a git worktree of a window's repository, open a tab in it and print
	its path. See *WORKTREE* in *alacritty*(5).

	*OPTIONS*
		*-b, --branch* _<BRANCH>_

			Branch to create, or check out if it exists.

			Default: the first free _agent-N_

		*-c, --command* _<COMMAND>_

			Command to start in the tab.

			Default: _worktree.command_, or the window's agent

		*-w, --window-id* _<WINDOW_ID>_

			Default: _$ALACRITTY_WINDOW_ID_, or the focused window

			Example: _xaya msg create-worktree -b fix/login -c codex_

# SEE ALSO

*alacritty*(1), *alacritty*(5), *alacritty-bindings*(5), *alacritty-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/izenloading/XaYa/issues_.

# MAINTAINERS

- izenloading

XaYa is a fork of Alacritty. See NOTICE for upstream attribution.
