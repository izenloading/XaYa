# xaya shell integration for bash.
#
# Marks prompts and commands with OSC 133, so xaya can jump between prompts and report commands
# that run long. Source it at the end of ~/.bashrc, after anything that sets PS1 or
# PROMPT_COMMAND:
#
#   source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.bash
#
# It uses the DEBUG trap to see commands start, replacing any DEBUG trap set before it. Other
# terminals that understand OSC 133 use the marks too; the rest ignore them.

[[ $- == *i* ]] || return 0
[[ -n ${_xaya_integrated-} ]] && return 0
_xaya_integrated=1

# Whether a command ran since the last prompt, so `D` isn't sent for an empty line.
_xaya_command_running=0
# Set from the end of PROMPT_COMMAND until the next command starts.
_xaya_at_prompt=0

_xaya_prompt_start() {
  local exit_code=$?
  _xaya_at_prompt=0
  if (( _xaya_command_running )); then
    printf '\e]133;D;%s\a' "$exit_code"
    _xaya_command_running=0
  fi
  return $exit_code
}

_xaya_prompt_end() {
  # Other PROMPT_COMMANDs may rebuild PS1, so mark it every time it lacks the marks.
  if [[ $PS1 != *'133;A'* ]]; then
    PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
  fi
  _xaya_at_prompt=1
}

# The first command after the prompt starts the output. PROMPT_COMMAND itself runs with the flag
# clear, except for `_xaya_prompt_start`, which runs first and clears it.
_xaya_debug_trap() {
  if (( _xaya_at_prompt )) && [[ $BASH_COMMAND != _xaya_prompt_start* ]]; then
    _xaya_at_prompt=0
    _xaya_command_running=1
    printf '\e]133;C\a'
  fi
}

# Run first to see the command's exit code, and last to mark the final PS1.
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
  PROMPT_COMMAND=(_xaya_prompt_start "${PROMPT_COMMAND[@]}" _xaya_prompt_end)
else
  PROMPT_COMMAND="_xaya_prompt_start${PROMPT_COMMAND:+; $PROMPT_COMMAND}; _xaya_prompt_end"
fi
trap _xaya_debug_trap DEBUG
