# xaya shell integration for fish.
#
# Marks prompts and commands with OSC 133, so xaya can jump between prompts and report commands
# that run long. Source it from ~/.config/fish/config.fish, after anything that defines
# fish_prompt:
#
#   source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.fish
#
# Other terminals that understand OSC 133 use the marks too; the rest ignore them.

status is-interactive; or return
set -q _xaya_integrated; and return
set -g _xaya_integrated 1

function __xaya_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end

function __xaya_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $status
end

# Wrap the prompt so every repaint is marked, not only the first draw.
functions -q fish_prompt; and functions -c fish_prompt __xaya_original_prompt
function fish_prompt
    printf '\e]133;A\a'
    functions -q __xaya_original_prompt; and __xaya_original_prompt
    printf '\e]133;B\a'
end
