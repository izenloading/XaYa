# xaya shell integration for zsh.
#
# Marks prompts and commands with OSC 133, so xaya can jump between prompts and report commands
# that run long. Source it at the end of ~/.zshrc, after any prompt theme:
#
#   source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.zsh
#
# Other terminals that understand OSC 133 use the marks too; the rest ignore them.

[[ -o interactive ]] || return 0
(( ${+_xaya_integrated} )) && return 0
typeset -g _xaya_integrated=1

# Whether a command ran since the last prompt, so `D` isn't sent for an empty line.
typeset -g _xaya_command_running=0

_xaya_precmd() {
  local exit_code=$?
  if (( _xaya_command_running )); then
    printf '\e]133;D;%s\a' "$exit_code"
    _xaya_command_running=0
  fi

  # Themes may rebuild the prompt before each command, so mark it every time it lacks the marks.
  # `A` and `B` live in the prompt itself so every redraw marks it again.
  if [[ $PS1 != *$'\e]133;A'* ]]; then
    PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
  fi
  return $exit_code
}

_xaya_preexec() {
  _xaya_command_running=1
  printf '\e]133;C\a'
}

autoload -Uz add-zsh-hook
# `precmd` runs last, after hooks that rebuild the prompt; `preexec` order doesn't matter.
add-zsh-hook precmd _xaya_precmd
add-zsh-hook preexec _xaya_preexec
