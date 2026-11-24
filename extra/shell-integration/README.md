# Shell integration

These scripts mark prompts and commands with `OSC 133`, so XaYa can:

- scroll between prompts with `ScrollToPreviousPrompt` / `ScrollToNextPrompt`
  (`Command+Up/Down` on macOS, `Control+Shift+Up/Down` elsewhere);
- report commands that run longer than `notifications.command_threshold` seconds with ✅ or ❌ in
  the window title, on the Dock badge, to `SelectNextAttention` and in a desktop notification;
- end the status agents report once their shell is back at its prompt.

Source the script for your shell at the end of its startup file, after any prompt theme. With
XaYa installed in `/Applications`:

| Shell | File                        | Line                                                                         |
|-------|-----------------------------|------------------------------------------------------------------------------|
| zsh   | `~/.zshrc`                  | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.zsh`  |
| bash  | `~/.bashrc`                 | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.bash` |
| fish  | `~/.config/fish/config.fish`| `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.fish` |

Elsewhere, source the copy in this directory instead.

The marks are harmless in other terminals: those that understand `OSC 133` use them, the rest
ignore them. The bash script uses the `DEBUG` trap, replacing one set before it.
