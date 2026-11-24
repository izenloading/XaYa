# XaYa 使用手册

*[English](user-guide.md) | 简体中文*

XaYa 是基于 Alacritty 的 GPU 加速终端，专为同时运行多个 Claude Code、Codex 等编码 agent 的用户而增强。它不替代 agent 的界面，而是让终端看得懂 agent 在做什么，帮你在多个会话之间快速切换、少等待、少丢失。

本手册介绍 XaYa 新增的功能。Alacritty 原有的功能（vi 模式、搜索、hints、选择等）见 [features.md](features.md) 和 `man 5 alacritty`。

## 目录

1. [安装与配置文件](#安装与配置文件)
2. [快速上手](#快速上手)
3. [Agent 状态：徽标、通知与 Dock](#agent-状态徽标通知与-dock)
4. [Agent 面板](#agent-面板)
5. [Shell 集成](#shell-集成)
6. [打开 agent 输出里的文件位置](#打开-agent-输出里的文件位置)
7. [分屏](#分屏)
8. [Worktree 标签：让每个 agent 有自己的检出](#worktree-标签让每个-agent-有自己的检出)
9. [会话恢复](#会话恢复)
10. [命令行控制：XaYa msg](#命令行控制xaya-msg)
11. [快捷键一览](#快捷键一览)
12. [配置参考](#配置参考)
13. [常见问题](#常见问题)

## 安装与配置文件

macOS（仅支持 Apple Silicon，macOS 11 及以上）：

```sh
make app
cp -r target/release/osx/XaYa.app /Applications/
```

其他平台的构建方法见 [INSTALL.md](../INSTALL.md)。

XaYa 不会自动创建配置文件，它按顺序使用找到的第一个：

1. `$XDG_CONFIG_HOME/xaya/xaya.toml`
2. `~/.config/xaya/xaya.toml`
3. 旧的 Alacritty 位置，例如 `~/.config/alacritty/alacritty.toml`

想从头开始，可以复制仓库根目录的 [`xaya.example.toml`](../xaya.example.toml)：它列出了全部配置项、默认值和中文说明，原样复制不会改变任何行为，取消注释想改的那几行即可。

已有的 Alacritty 配置可以直接沿用。本手册中的配置都写在这个文件里，修改后自动生效。

## 快速上手

1. 安装后打开 XaYa，在里面运行 `claude` 或 `codex`。无需任何设置，它们会自动向 XaYa 上报状态。
2. 让 agent 去干活，切到别的窗口做别的事。agent 需要你时，XaYa 会发桌面通知，窗口标题出现 🔔，Dock 图标显示等待数。
3. 按 `Cmd+J` 直接跳到最需要你的那个窗口；按 `Cmd+Shift+A` 打开 agent 面板，一眼看全所有窗口。
4. 想再开一个 agent 又不想和当前这个改同一份代码：按 `Cmd+Shift+T`，在新的 git worktree 里开标签并启动同一个 agent。
5. 想让 agent 和 shell 并排：按 `Cmd+D` 右侧分屏，`Cmd+Shift+D` 下方分屏。
6. 建议安装 [shell 集成](#shell-集成)，以获得提示符跳转和长命令完成通知。

> 本手册的快捷键以 macOS 为准，其他平台的对应按键见[快捷键一览](#快捷键一览)。

## Agent 状态：徽标、通知与 Dock

Claude Code 和 Codex 会通过 Program Status Protocol（OSC 7501）告诉终端自己的状态。XaYa 把它显示在窗口外面，你不用盯着每个窗口。

**标题徽标**：窗口标题前会出现：

| 徽标 | 含义 |
|---|---|
| ⏳（或 `⏳ 40%`） | 正在工作，有进度时显示百分比 |
| 🔔 | 在等你：需要授权、有问题要问或需要登录 |
| ✅ | 已完成 |
| ❌ | 失败 |

完成和失败的徽标会一直保留到你看过这个窗口（窗口获得焦点）为止。正在工作的状态在 agent 退出、回到 shell 提示符时自动结束。

**桌面通知**：agent 开始等你、失败或完成时发通知，内容包括 agent 的名字和它的问题（例如 “Claude Code: Needs your permission — Run cargo test”）。在 macOS 上点击通知会切到对应窗口或标签。默认只在窗口不在前台时通知。

**Dock 计数**（macOS）：Dock 图标上的数字表示有多少个窗口在等你或失败了而你还没看。如果看不到数字，到 系统设置 → 通知 → XaYa 里打开“标记”。

**跳到需要你的窗口**：`Cmd+J`（其他平台 `Ctrl+Shift+J`）按紧急程度跳转：先等你的，再失败的，再完成的；同一级里等得最久的优先。再按一次跳到下一个。

**同时打开的通知**：agent 自己发的 OSC 9 / OSC 777 通知，在它上报状态期间会被 XaYa 的状态通知取代，避免重复。

相关配置：

```toml
[terminal]
program_status = true          # 默认开启；关闭后不显示以上任何状态

[notifications]
enabled = true                 # 是否发桌面通知
only_when_unfocused = true     # 只在窗口不在前台时通知
command_threshold = 10         # 普通命令运行超过多少秒在完成时通知（需 shell 集成），0 为关闭
```

## Agent 面板

`Cmd+Shift+A`（其他平台 `Ctrl+Shift+A`）在当前窗口上方打开面板，列出所有窗口：

- 每行显示序号、状态（例如 `waiting: permission`、`working 40%`）、agent 名称、持续时长、上一轮改动和窗口标题；
- agent 有消息时（比如要你批准的命令），显示在下一行；
- 上一轮改动例如 `3 files +48 -3`，是 agent 这一轮在 git 检出里改了什么（见下文）；
- 排序与 `Cmd+J` 相同，最需要你的在最上面；
- 打开期间每秒刷新。

操作：

| 按键 | 作用 |
|---|---|
| `↑` `↓`、`j` `k`、`Tab` `Shift+Tab` | 选择 |
| `Enter` | 切到选中的窗口 |
| `1` – `9` | 直接切到该行的窗口 |
| `d` | 在选中窗口下方新开一个 pane，显示上一轮改动的 diff |
| `Esc`、`q`、或再按一次 `Cmd+Shift+A` | 关闭面板 |
| `p` | 查看当前仓库的任务；在此视图 `↑`／`↓` 选择任务、`Enter` 打开验收页、`n` 交办新任务 |

### 交办新任务

在项目终端打开面板，按 `p` 进入当前仓库的任务列表，再按 `n`，在“What do you want done?”下输入一句需求（可用输入法，
`Cmd+V` 粘贴），按 `Enter` 交办，`Esc` 放弃。XaYa 会从你当前所在的分支为任务新建独立工作区，启动 Claude 实现，
再由 Codex 审查、按需发回修复，全程在后台进行；关掉面板或窗口也不影响。任务随即出现在列表里，可随时按 `p` 刷新查看
进展。输入框下一行写着由谁实现、谁审查，按 `Tab` 在“Claude 实现 · Codex 审查”与“Codex 实现 · Claude 审查”之间
切换，只对这一次交办有效。默认由 Claude 实现；想让这个仓库的任务默认交给 Codex，在仓库里运行一次
`xaya task engine . --set codex`（`--reset` 恢复 Claude），面板新任务就从 Codex 开始。交办失败时文字保留，原样再按 `Enter` 不会重复创建任务。细节见[一步交办](managed-sessions.zh-CN.md#一步交办xaya-task-delegate)。

需要先装好 `claude` 与 `codex`，且仓库的验证配置已被信任（`xaya project trust`）；从访达打开 XaYa 时，会读取登录 shell
的 `PATH` 来找到它们。

### 验收与合入

任务列表把需要处理的排在前面：先是需要关注的，然后是“ready to accept”（审查已通过、可以合入）和“landing”（正在合入）。
用 `↑`／`↓` 选中任务，按 `Enter` 打开验收页，依次显示：

- 能否合入：可以时写明目标分支，不能时写明原因，例如审查要求修改或还没审查；
- 验证结果、改动的文件及增删行数；
- 审查结论、验收标准和发现的问题；
- 完整 diff（超过 256 KiB 时截断，文件列表仍完整）。

想让它再改改，按 `c`，在“What should change?”下写下要改什么（可用输入法、`Cmd+V` 粘贴），按 `Enter` 发送、`Esc`
放弃。意见会发给原来实现它的会话，改完后照常重新验证、重新审查；这期间不能合入，页面显示“Changes under way”。
新的审查通过后，页面又会出现 `l`。只有交办的任务（服务在后台驱动的任务）能这样要求修改；发送结果不明时原样再按
`Enter` 不会重复发送。你提的修改不占用审查自动发回修复的轮次。

可以合入时按 `l`，服务在后台合入目标分支并把任务记为已验收，关掉面板也不影响；页面会跟着显示进度，最后为“Landed on
main · accepted”或失败原因。合入后 XaYa 停止实现它的会话并删除任务的工作区，页面显示“Workspace removed”；
如果合入后你又在工作区里改过东西，工作区会保留并写明原因。失败后处理好原因，再按 `l` 即重新合入。`Esc` 回到任务列表。细节见
[后台合入](managed-sessions.zh-CN.md#后台合入xaya-task-land)。

### 每一轮改了什么

agent 报告“工作中”时 XaYa 给它所在的 git 检出拍一次快照，完成、失败或回到空闲时再拍一次，两次之间的差别就是这一轮的改动：

- 包含新增的未跟踪文件，不含 `.gitignore` 忽略的文件；
- 快照用 index 的副本，不会动你暂存的内容，也不会产生提交；
- 没有改动的一轮会清掉显示；
- `xaya msg status --json` 的 `changes` 字段给出文件数、增删行数和前后两个 git tree，可以自己执行 `git diff <before> <after>`。

这一轮期间其他程序（包括你自己）写入该检出的文件也会算进去；两个 agent 共用一个检出时无法区分，建议用 worktree 分开（见下文）。不需要时设 `terminal.turn_changes = false`。

## Shell 集成

Shell 集成让 XaYa 知道提示符和命令的边界（OSC 133）。安装后可以：

- 用 `Cmd+↑` / `Cmd+↓` 在各次命令的提示符之间跳转；
- 运行超过 `notifications.command_threshold` 秒的命令完成时，标题显示 ✅ 或 ❌，并发通知说明用时；
- agent 退出回到提示符时，准确结束它的状态；
- 用 `xaya msg get-text --last-command` 读取上一条命令及其输出；
- 恢复会话和新开 worktree 标签时，在 shell 准备好的那一刻立即输入命令（没有集成时等待 2 秒）。

在 shell 启动文件**末尾**（主题配置之后）加一行：

| Shell | 文件 | 添加 |
|---|---|---|
| zsh | `~/.zshrc` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.zsh` |
| bash | `~/.bashrc` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.bash` |
| fish | `~/.config/fish/config.fish` | `source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.fish` |

这些标记对其他终端无害：支持 OSC 133 的终端会使用它们，不支持的会忽略。

## 打开 agent 输出里的文件位置

agent 和编译器常输出 `src/main.rs:12:5` 这样的位置。XaYa 能识别它们：

- **按住 `Cmd` 单击**（其他平台 `Ctrl`）直接打开。在 Claude Code、Codex 这类接管鼠标的程序里同样有效，无需再按 Shift。
- **`Ctrl+Shift+E`** 进入键盘选择模式，每个文件位置旁出现字母标签，按对应字母打开。

识别的格式包括：`src/main.rs:12:5`、`src/main.rs:12`、`./run.sh`、`../x/y.md`、`~/notes/todo.md`、`/usr/local/bin/xaya`、`main.rs:12`。

- 相对路径按终端前台程序的当前目录解析。
- 不存在的文件不会有任何动作，所以 `and/or` 这类文字点了也无害。
- 默认用系统默认程序打开文件，不定位到行。要跳到具体行列，配置编辑器：

```toml
[hints]
editor = { program = "code", args = ["-g"] }      # VS Code
# editor = { program = "cursor", args = ["-g"] }  # Cursor
# editor = { program = "zed" }                    # Zed
# editor = { program = "subl" }                   # Sublime Text
```

编辑器收到的参数形如 `/绝对路径/src/main.rs:12:5`。

## 分屏

一个窗口可以分成多个 pane，每个 pane 是一个独立的终端：

| 操作 | macOS | 其他平台 |
|---|---|---|
| 在右侧分屏 | `Cmd+D` | `Ctrl+Shift+D` |
| 在下方分屏 | `Cmd+Shift+D` | `Ctrl+Shift+Alt+D` |
| 下一个 pane | `Cmd+]` | `Ctrl+Shift+PageDown` |
| 上一个 pane | `Cmd+[` | `Ctrl+Shift+PageUp` |

- 新 pane 在当前 pane 的目录中启动 shell，并获得键盘焦点。
- 单击某个 pane 即可切过去。焦点 pane 显示实心光标，其他 pane 显示空心光标。
- pane 里的 shell 退出（或按 `Cmd+W`），只关闭这个 pane，它的空间还给相邻的 pane。关闭最后一个 pane 时窗口关闭。
- 窗口标题显示焦点 pane 的标题；标题徽标、Dock 计数、面板和 `msg status` 会综合所有 pane，取最需要你的那个。
- 太小的 pane 不能再分，会在消息栏提示。

当前限制：pane 总是平分，暂不支持拖动调整大小或放大单个 pane。

## Worktree 标签：让每个 agent 有自己的检出

多个 agent 在同一份代码里工作，会互相改到对方的文件。`Cmd+Shift+T`（其他平台 `Ctrl+Shift+T`）一步完成：

1. 为当前窗口所在的 git 仓库新建 worktree，从当前提交检出，新建分支 `agent-1`、`agent-2`……（取第一个未被占用的）；
2. worktree 放在仓库旁边的 `<仓库名>.worktrees/<分支名>`；
3. 在新标签里打开它，并启动当前窗口正在运行的 agent（`claude` 或 `codex`）。

新标签的标题以分支开头，例如 `⎇ agent-2 · …`，在面板和 `msg status` 中也一样。创建失败时（例如不在 git 仓库里），原因显示在消息栏。

**提醒共用检出**：当多个窗口或 pane 里的 agent 在同一个检出目录工作时，XaYa 会在消息栏提示一次，建议改用 worktree 标签。

相关配置：

```toml
[worktree]
command = "claude"                  # 新标签启动的命令；默认沿用当前窗口的 agent
directory = "/Users/me/worktrees"   # worktree 存放位置；默认 <仓库>.worktrees
warn_shared = true                  # 是否提醒共用检出
```

用完的 worktree 不会自动删除，合并后请用 `git worktree remove <路径>` 清理。

## 会话恢复

XaYa 退出、崩溃或升级时还在运行 agent 的窗口，下次启动时会自动回来：

- 窗口在原来的目录打开（macOS 上作为同一窗口的多个标签）；
- shell 准备好后，XaYa 自动输入并执行 `claude --resume <会话>` 或 `codex resume <会话>`；
- 你主动关闭的窗口不会恢复。

第一次恢复时，恢复了 agent 的窗口会在消息栏说明发生了什么、如何关闭；之后不再提示。

细节：

- Claude Code 的会话是精确对应的。Codex 不记录哪个进程对应哪个会话，XaYa 取该目录下最新的会话；两个 Codex 窗口在同一目录时，会改用 `codex resume`，让你自己选。
- 用 `-e` 或 `--working-directory` 启动 XaYa 时不会恢复。
- 分屏的窗口会恢复成多个标签，分屏布局不会还原。
- 快照每 15 秒、开关窗口时、退出时保存到 `~/.local/state/xaya/session.json`（遵循 `$XDG_STATE_HOME`）。

```toml
[session]
restore = "Agents"   # 默认：只恢复运行 agent 的窗口
# restore = "All"    # 所有窗口都回到原目录
# restore = "Never"  # 不恢复，也不保存快照
```

此功能仅支持 macOS 和 Linux 等 Unix 系统。

## 命令行控制：XaYa msg

在 XaYa 窗口里运行的命令可以直接使用 `xaya msg`；默认作用于所在窗口（`$ALACRITTY_WINDOW_ID`），否则作用于当前焦点窗口，也可以用 `-w <窗口 ID>` 指定。窗口 ID 可以从 `xaya msg status` 看到。

| 命令 | 作用 |
|---|---|
| `xaya msg status` | 以表格列出所有窗口的 agent 状态，`*` 标记焦点窗口 |
| `xaya msg status --json` | 输出 JSON |
| `xaya msg status --wait` | 等到没有 agent 在工作再输出 |
| `xaya msg status --watch [--json]` | 持续输出状态变化，每次变化一行 |
| `xaya msg focus -w 42` | 把窗口 42 切到前台 |
| `xaya msg get-text [-w 42]` | 输出窗口当前屏幕内容（在 Claude Code、Codex 里就是 agent 当前界面） |
| `xaya msg get-text --lines 200` | 输出最后 200 行，包含滚动历史 |
| `xaya msg get-text --last-command` | 输出上一条 shell 命令及其输出（需 shell 集成） |
| `xaya msg send-text [-w 42] [--enter] "文本"` | 以粘贴方式向窗口输入文本，`--enter` 再按回车（需开启，见下） |
| `xaya msg split [--down] [-c 命令]` | 分屏，并在新 pane 中运行命令 |
| `xaya msg create-worktree [-b 分支] [-c 命令]` | 新建 worktree 标签，并输出其路径 |

示例：

```sh
# 所有 agent 都完成后提醒我
xaya msg status --wait && say "agents are done"

# 只关注在等我的事件
xaya msg status --watch --json | jq -c 'select(.state == "blocked")'

# 把失败的测试输出交给隔壁窗口的 agent
xaya msg send-text -w 42 --enter "修复这个测试失败：$(xaya msg get-text --last-command)"

# 在 shell 旁边开一个 codex
xaya msg split -c codex
```

**`send-text` 默认关闭**：开启后，你电脑上的任何程序都可以向 agent 下指令。确有需要时在配置文件中开启（无法通过 `xaya msg config` 开启）：

```toml
[general]
ipc_send_text = true
```

`--watch --json` 每行是一个 JSON 对象：`"event": "changed"` 时带有窗口状态字段（`window_id`、`title`、`state`、`kind`、`app`、`message`、`progress`、`seconds`），窗口关闭时为 `{"event": "closed", "window_id": …}`。注意它报告的是 XaYa 显示的状态：焦点窗口里的结果会被立即视为已读，因此直接清空，不会报告 `done`。

`xaya msg` 系列命令仅在 macOS 和 Linux 等 Unix 系统上可用。

## 快捷键一览

| 功能 | macOS | 其他平台 |
|---|---|---|
| 跳到下一个需要你的窗口 | `Cmd+J` | `Ctrl+Shift+J` |
| 打开/关闭 agent 面板 | `Cmd+Shift+A` | `Ctrl+Shift+A` |
| 上/下一个提示符（需 shell 集成） | `Cmd+↑` / `Cmd+↓` | `Ctrl+Shift+↑` / `Ctrl+Shift+↓` |
| 打开文件位置 | `Cmd`+单击 | `Ctrl`+单击 |
| 键盘选择文件位置 | `Ctrl+Shift+E` | `Ctrl+Shift+E` |
| 键盘选择链接 | `Ctrl+Shift+O` | `Ctrl+Shift+O` |
| 新建 worktree 标签 | `Cmd+Shift+T` | `Ctrl+Shift+T` |
| 右侧分屏 | `Cmd+D` | `Ctrl+Shift+D` |
| 下方分屏 | `Cmd+Shift+D` | `Ctrl+Shift+Alt+D` |
| 下/上一个 pane | `Cmd+]` / `Cmd+[` | `Ctrl+Shift+PageDown` / `PageUp` |
| 关闭当前 pane（最后一个时关闭窗口） | `Cmd+W` | 在 shell 中 `exit` |
| 新标签 | `Cmd+T` | — |
| 退出 XaYa | `Cmd+Q` | — |

所有快捷键都可以在 `[keyboard]` 中修改，对应的动作名为 `SelectNextAttention`、`ToggleAgentPanel`、`ScrollToPreviousPrompt`、`ScrollToNextPrompt`、`CreateWorktreeTab`、`SplitRight`、`SplitDown`、`FocusNextPane`、`FocusPreviousPane`。例如：

```toml
[keyboard]
bindings = [
  { key = "P", mods = "Command|Shift", action = "ToggleAgentPanel" },
]
```

## 配置参考

XaYa 新增的配置项汇总（均为默认值）：

```toml
[general]
ipc_send_text = false        # 允许 xaya msg send-text

[terminal]
program_status = true        # 显示 agent 状态（OSC 7501）

[notifications]
enabled = true
only_when_unfocused = true
command_threshold = 10
# command = { program = "...", args = [...] }  # 自定义通知命令，标题和正文作为参数追加

[colors]
minimum_contrast = 1.0       # 程序指定颜色的最低对比度，例如 4.5；1.0 为关闭

[hints]
# editor = { program = "code", args = ["-g"] }

[session]
restore = "Agents"           # "Agents" | "All" | "Never"

[worktree]
# command = "claude"
# directory = "/path/to/worktrees"
warn_shared = true
```

完整说明见 `man 5 alacritty`。

## 常见问题

**标题没有状态徽标。** 确认 `terminal.program_status` 没有被关闭；Claude Code 需 v2.1.295 及以上，Codex 需 rust-v0.163.0 及以上。

**收不到通知。** 检查 `notifications.enabled`；macOS 上到 系统设置 → 通知 → XaYa 确认已允许。默认窗口在前台时不通知。

**Dock 上没有数字。** 系统设置 → 通知 → XaYa → 打开“标记”。

**单击文件路径没反应。** 确认文件存在（相对路径按前台程序的目录解析），并且按住了 `Cmd`（其他平台 `Ctrl`）。配置了 `hints.editor` 却打不开时，可能是从 Dock 启动的 XaYa 找不到编辑器命令，改用完整路径，例如 `/usr/local/bin/code`。

**恢复的窗口没有自动运行 agent。** 检查 `session.restore`；用 `-e` 或 `--working-directory` 启动时不恢复。对于还没发过消息的 Claude Code 会话，没有可恢复的对话，只会恢复窗口。

**`send-text` 报错 “sending text is off”。** 在配置文件中设置 `general.ipc_send_text = true`。

**分屏时提示没有空间。** pane 至少要能放下约 10 列或 3 行，先放大窗口。

**`msg status` 里看不到某个窗口的状态。** 焦点窗口里已完成的结果会被立即视为已读并清空；这是有意的设计。
