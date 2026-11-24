# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_xaya_global_optspecs
	string join \n print-events ref-test embed= socket= q v daemon working-directory= hold e/command= T/title= class= o/option= config-file= h/help V/version
end

function __fish_xaya_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_xaya_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_xaya_using_subcommand
	set -l cmd (__fish_xaya_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c xaya -n "__fish_xaya_needs_command" -l embed -d 'X11 window ID to embed Alacritty within (decimal or hexadecimal with "0x" prefix)' -r
complete -c xaya -n "__fish_xaya_needs_command" -l socket -d 'Path for IPC socket creation' -r -F
complete -c xaya -n "__fish_xaya_needs_command" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c xaya -n "__fish_xaya_needs_command" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c xaya -n "__fish_xaya_needs_command" -s T -l title -d 'Defines the window title [default: XaYa]' -r
complete -c xaya -n "__fish_xaya_needs_command" -l class -d 'Defines window class/app_id on X11/Wayland [default: xaya]' -r
complete -c xaya -n "__fish_xaya_needs_command" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c xaya -n "__fish_xaya_needs_command" -l config-file -d 'Specify alternative configuration file [default: $HOME/.config/xaya/xaya.toml].' -r -F
complete -c xaya -n "__fish_xaya_needs_command" -l print-events -d 'Print all events to STDOUT'
complete -c xaya -n "__fish_xaya_needs_command" -l ref-test -d 'Generates ref test'
complete -c xaya -n "__fish_xaya_needs_command" -s q -d 'Reduces the level of verbosity (the min level is -qq)'
complete -c xaya -n "__fish_xaya_needs_command" -s v -d 'Increases the level of verbosity (the max level is -vvv)'
complete -c xaya -n "__fish_xaya_needs_command" -l daemon -d 'Do not spawn an initial window'
complete -c xaya -n "__fish_xaya_needs_command" -l hold -d 'Remain open after child process exit'
complete -c xaya -n "__fish_xaya_needs_command" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_needs_command" -s V -l version -d 'Print version'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "msg" -d 'Send a message to the Alacritty socket'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "agent" -d 'Manage the local agent service (start, status, stop)'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "run" -d 'Start a managed Codex or Claude session in the current terminal'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "attach" -d 'Attach the native engine TUI to a managed session'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "checkpoint" -d 'Capture a managed workspace in a private Git reference'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "check" -d 'Run and inspect checkpoint verification commands'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "project" -d 'Inspect and trust repository verification commands'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "land" -d 'Preview, verify and land a squash candidate on a local branch'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "review" -d 'Review a checkpoint with a headless engine in a private, read-only checkout'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "task" -d 'Tasks: a goal and testable acceptance criteria for sessions, used by reviews'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "migrate" -d 'Migrate the configuration file'
complete -c xaya -n "__fish_xaya_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -s s -l socket -d 'IPC socket connection path override' -r -F
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "create-window" -d 'Create a new window in the same xaya process'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "config" -d 'Update the Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "get-config" -d 'Read runtime Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "status" -d 'Show what the agents in each window are doing'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "focus" -d 'Bring a window to the front'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "get-text" -d 'Print a window\'s text: its screen, its last lines or its last shell command'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "send-text" -d 'Type text into a window, the way pasting it would'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "create-worktree" -d 'Open a tab in a new git worktree of a window\'s repository, printing its path'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "split" -d 'Split a window\'s focused pane, starting a shell beside it'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config status focus get-text send-text create-worktree split help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s T -l title -d 'Defines the window title [default: XaYa]' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l class -d 'Defines window class/app_id on X11/Wayland [default: xaya]' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l hold -d 'Remain open after child process exit'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from config" -s w -l window-id -d 'Window ID for the new config' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from config" -s r -l reset -d 'Clear all runtime configuration changes'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s w -l window-id -d 'Window ID for the config request' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from status" -l json -d 'Print JSON instead of a table'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from status" -l wait -d 'Wait until no window has a program working, then print'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from status" -l watch -d 'Keep running and print each window as its program reports something new, one line per change; with `--json`, one JSON object per line'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from focus" -s w -l window-id -d 'Window ID, as `msg status` lists it; the focused window when absent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from focus" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s w -l window-id -d 'Window ID, as `msg status` lists it; the focused window when absent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-text" -l lines -d 'The last rows with content, scrollback included, rather than the screen' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-text" -l last-command -d 'The last shell command and its output, from shell integration'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from send-text" -s w -l window-id -d 'Window ID, as `msg status` lists it; the focused window when absent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from send-text" -l enter -d 'Press Enter after the text, submitting it'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from send-text" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-worktree" -s w -l window-id -d 'Window ID, as `msg status` lists it; the focused window when absent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-worktree" -s b -l branch -d 'Branch to create, or check out if it exists; by default the first free `agent-N`' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-worktree" -s c -l command -d 'Command to start in the new tab; by default `worktree.command`, or the window\'s agent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from create-worktree" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from split" -s w -l window-id -d 'Window ID, as `msg status` lists it; the focused window when absent' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from split" -s c -l command -d 'Command typed into the new pane\'s shell once it\'s ready' -r
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from split" -s d -l down -d 'Put the new pane below the focused one, rather than on its right'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from split" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "create-window" -d 'Create a new window in the same xaya process'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "config" -d 'Update the Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-config" -d 'Read runtime Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "status" -d 'Show what the agents in each window are doing'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "focus" -d 'Bring a window to the front'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-text" -d 'Print a window\'s text: its screen, its last lines or its last shell command'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "send-text" -d 'Type text into a window, the way pasting it would'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "create-worktree" -d 'Open a tab in a new git worktree of a window\'s repository, printing its path'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "split" -d 'Split a window\'s focused pane, starting a shell beside it'
complete -c xaya -n "__fish_xaya_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -s c -l config-file -d 'Path to the configuration file' -r -F
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -s d -l dry-run -d 'Only output TOML config to STDOUT'
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -s i -l skip-imports -d 'Do not recurse over imports'
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -l skip-renames -d 'Do not move renamed fields to their new location'
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -s s -l silent -d 'Do not output to STDOUT'
complete -c xaya -n "__fish_xaya_using_subcommand migrate" -s h -l help -d 'Print help'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "msg" -d 'Send a message to the Alacritty socket'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "agent" -d 'Manage the local agent service (start, status, stop)'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "run" -d 'Start a managed Codex or Claude session in the current terminal'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "attach" -d 'Attach the native engine TUI to a managed session'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "checkpoint" -d 'Capture a managed workspace in a private Git reference'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "check" -d 'Run and inspect checkpoint verification commands'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "project" -d 'Inspect and trust repository verification commands'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "land" -d 'Preview, verify and land a squash candidate on a local branch'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "review" -d 'Review a checkpoint with a headless engine in a private, read-only checkout'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "task" -d 'Tasks: a goal and testable acceptance criteria for sessions, used by reviews'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "migrate" -d 'Migrate the configuration file'
complete -c xaya -n "__fish_xaya_using_subcommand help; and not __fish_seen_subcommand_from msg agent run attach checkpoint check project land review task migrate help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-window" -d 'Create a new window in the same xaya process'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "config" -d 'Update the Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-config" -d 'Read runtime Alacritty configuration'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "status" -d 'Show what the agents in each window are doing'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "focus" -d 'Bring a window to the front'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-text" -d 'Print a window\'s text: its screen, its last lines or its last shell command'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "send-text" -d 'Type text into a window, the way pasting it would'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-worktree" -d 'Open a tab in a new git worktree of a window\'s repository, printing its path'
complete -c xaya -n "__fish_xaya_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "split" -d 'Split a window\'s focused pane, starting a shell beside it'
