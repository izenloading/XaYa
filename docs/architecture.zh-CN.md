# XaYa 技术架构

本文描述 XaYa 当前（0.0.1）的技术架构，面向参与开发的人。使用方法见[用户手册](user-guide.zh-CN.md)，Metal 渲染器的细节见 [metal.md](metal.md)。

## 1. 定位与设计原则

XaYa 是 Alacritty 的分支，在保留其终端仿真、渲染与配置体系的基础上，为同时运行多个编码 agent（Claude Code、Codex）的用户增加“感知与调度”能力。

几条贯穿全局的原则：

- **理解 agent，而不包装 agent。** XaYa 不嵌入 agent 的界面，也不调用模型。它通过公开协议（OSC 7501、OSC 133）和 agent 留在磁盘上的元数据了解它们，agent 怎么升级都不影响终端。
- **不模拟按键去操作 agent。** 需要 agent 配合的能力（例如在终端里批准权限）走协议，见 [提案](proposals/program-status-answers.zh-CN.md)。唯一的例外是用户显式开启的 `msg send-text`。
- **单 pane 时行为与 Alacritty 一致。** 分屏、状态等能力都以“只有一个 pane 时完全不改变原有路径”为约束，包括损伤追踪和渲染视口。
- **兼容 Alacritty。** 配置文件、terminfo、环境变量（`ALACRITTY_WINDOW_ID`、`ALACRITTY_SOCKET`）保持可用。

## 2. 代码组织

Cargo workspace 由六个 crate 组成：

| crate | 作用 |
|---|---|
| `alacritty_terminal` | 与平台和渲染无关的终端核心：网格、VT 解析后的状态机、PTY、I/O 事件循环、选择、搜索、vi 模式。XaYa 在此增加 OSC 扫描与协议解析 |
| `alacritty`（二进制名 `xaya`） | 应用：窗口、事件循环、输入、显示与渲染器、配置、IPC，以及 XaYa 的全部上层功能 |
| `alacritty_config` / `alacritty_config_derive` | 配置反序列化框架与派生宏（`ConfigDeserialize`、`SerdeReplace`） |
| `xaya_agent_proto` | 不依赖运行时的 JSON-RPC 服务协议、会话和 attach 类型 |
| `xaya_agentd`（二进制名 `xaya-agentd`） | 独立服务、Codex 专属后端、Claude 原生后台与 Mods 桥、SQLite、检查点、验证及本地合入；Tokio 依赖只位于此 crate |

XaYa 新增或大幅改动的模块：

| 位置 | 内容 |
|---|---|
| `alacritty_terminal/src/notification.rs` | OSC 扫描器：从字节流中挑出 vte 不分发的 OSC 9/777/7501/133/21337 |
| `alacritty_terminal/src/program_status.rs` | OSC 7501 报告解析、`ProgramTracker` 记录树、探测限流 |
| `alacritty_terminal/src/shell_integration.rs` | OSC 133 标记解析 |
| `alacritty_terminal/src/term/mod.rs` | 提示符行标记（`Flags::PROMPT`）与 `prompt_line` 查找 |
| `alacritty/src/program_status.rs` | 每个终端的 `ProgramActivity`：徽标、通知时机、注意力排序、前台进程监测 |
| `alacritty/src/window_status.rs` | `WindowStatus`：`msg status`、面板、`--watch` 共用的窗口状态快照与变化检测 |
| `alacritty/src/agent_panel.rs` | agent 面板的纯布局逻辑 |
| `alacritty/src/pane.rs` | `Pane`：一个终端及其 PTY、搜索、agent 状态 |
| `alacritty/src/layout.rs` | 分屏布局树、`PaneId` |
| `alacritty/src/window_context.rs` | 窗口：pane 集合、布局、焦点、事件路由 |
| `alacritty/src/display/mod.rs` | 绘制流程（清屏 → 逐 pane → 覆盖层）、`SizeInfo` |
| `alacritty/src/renderer/metal/` | macOS 原生 Metal 渲染器 |
| `alacritty/src/file_link.rs` | 文件位置 hint 的正则与解析 |
| `alacritty/src/terminal_text.rs` | `msg get-text` 的文本提取 |
| `alacritty/src/session.rs` | 会话快照、Claude Code / Codex 会话识别 |
| `alacritty/src/turn_changes.rs` | 每轮改动：轮次边界、git 快照、每个 pane 的后台线程 |
| `alacritty/src/worktree.rs` | git worktree 创建 |
| `alacritty/src/agent.rs` | `agent`、`run`、`attach` 原样转交同目录的 `xaya-agentd`，不加载窗口系统 |
| `xaya_agentd/src/sessions.rs` | 会话注册表、持久身份恢复、原生 thread 绑定、专属进程组和原生 TUI 转发；见[使用说明](managed-codex.zh-CN.md) |
| `xaya_agentd/src/sessions/claude.rs` / `claude_mod.js` | Claude 公开后台 CLI、私有 Mods 观察桥、原生身份交叉绑定和连接持有的附着租约；首批拒绝控制／恢复及验证门禁，见[范围](managed-claude.zh-CN.md) |
| `xaya_agentd/src/landings.rs` | 私有 squash 候选、独立检出集成验证、源／目标门禁、版本及检出归属复核、未检出目标 CAS／已检出目标事务快进、碰撞检查、合入效果核对与持久回执 |
| `xaya_agentd/src/projects.rs` | `.xaya/project.toml` 严格解析、配置哈希、检出范围内的 cwd 与本地信任 |
| `xaya_agentd/src/checks/cleanup.rs` | 集成检出归属 marker、终态及进程组检查、有记录目录的保守清理 |
| `xaya_agentd/src/landings/transaction.rs` | 引用事务、目标 index 锁、忽略文件碰撞检查与私有 index 快进 |
| `xaya_agentd/src/checks.rs` | 检查点绑定的 argv／多步骤验证、日志分页、超时／取消、项目配置复核、查询时过期与持久去重 |
| `xaya_agentd/src/storage.rs` | SQLite WAL 会话、回执、检查点与验证记录、版本迁移、独占锁与进程组归属检查 |
| `alacritty/src/managed_panel.rs` / `managed_panel/bridge.rs` | 托管面板布局、审批预览和后台 companion CLI 桥接；应用只依赖协议 crate |
| `xaya_agentd/src/sessions/control.rs` | 程序发送、绑定 turn 的中断、审批决策、命令去重与回执 |
| `xaya_agentd/src/events.rs` | 同锁快照与订阅、连续事件序号、有限缓冲与慢订阅者失效 |
| `alacritty/src/polling/ipc.rs` | Unix socket IPC |
| `alacritty/src/macos/` | Dock 徽标、`UNUserNotificationCenter` 通知、进程信息 |
| `extra/shell-integration/` | zsh/bash/fish 的 OSC 133 脚本 |

## 3. 进程与线程模型

```
                    ┌──────────────────────── 主线程 ────────────────────────┐
                    │ winit 事件循环 → Processor                             │
  IPC / 信号线程 ──►│   ├─ WindowContext（每个窗口）                         │
  （polling）       │   │    ├─ Display + Renderer（Metal / OpenGL）         │
                    │   │    ├─ Layout + 焦点                                │
  配置监视线程 ────►│   │    └─ Pane × N ──── Arc<FairMutex<Term>> ◄──┐       │
  （notify）        │   ├─ Scheduler（定时器）                         │       │
                    │   └─ 会话快照、共用检出检查                       │       │
  git worktree ───►│                                                  │       │
  （一次性线程）    └──────────────────────────────────────────────────┼───────┘
                                                                       │
                     PTY I/O 线程（每个 pane 一个）── 读 PTY → 解析 → 写 Term
                                                    └─ OSC 扫描 → EventProxy
```

- **主线程**：winit 事件循环。所有窗口、渲染、输入、IPC 回复都在这里，不需要跨线程同步窗口状态。
- **PTY I/O 线程**：每个 pane 一个（`alacritty_terminal::event_loop`）。它读 PTY 数据，交给 vte 解析器写入 `Term`（通过 `FairMutex` 与主线程共享），同时运行 OSC 扫描器，再通过 `EventProxy` 把事件发回主线程。
- **IPC/信号线程**（Unix）：用 `polling` 同时等待 socket 连接和 SIGINT/SIGTERM。读到请求后转成事件，连同 socket 一起交给主线程回复。
- **配置监视线程**：配置文件变化时发 `ConfigReload`。
- **一次性线程**：`git worktree add` 在独立线程运行，完成后发 `WorktreeReady`。
- **每轮改动线程**：每个 pane 的 agent 第一次开始一轮时启动一个，按顺序执行快照，结果以 `TurnChanged` 发回该 pane；pane 关闭后随之退出。

## 4. 事件流

所有跨线程通知都是 `Event { window_id, pane, payload: EventType }`，经 `EventLoopProxy` 进入主线程：

```
PTY 线程 ──EventProxy(window, pane)──► Processor::user_event
IPC 线程 ──Event(IpcRequest/IpcStatus…)──►     │
Scheduler 定时器 ──────────────────────────►    │
                                                 ├─ 应用级事件就地处理（创建窗口、IPC、面板刷新、worktree 完成…）
                                                 └─ 其余转给 WindowContext::handle_event
                                                        ├─ 窗口级拦截：SplitPane / FocusPane / 启动输入
                                                        ├─ 入队，等 AboutToWait 统一处理
                                                        └─ 按 pane 分批 → 每批一个 ActionContext → input::Processor
```

**事件归属（路由）规则**，见 `WindowContext::event_pane`：

| 事件 | 发给 |
|---|---|
| 带 `pane` 的用户事件（终端事件、前台检查等） | 该 pane |
| 鼠标移动 | 按住按键时为按下时所在的 pane，否则为鼠标下方的 pane |
| 鼠标按下 | 鼠标下方的 pane，并把焦点切过去 |
| 滚轮、触摸 | 鼠标下方的 pane |
| 其他（键盘、IME、窗口焦点、缩放等） | 焦点 pane |

处理一批事件时，`display.size_info` 临时设为该 pane 的几何信息，所以 `input` 与 `display` 中原有的坐标计算无需改动。处理完恢复为焦点 pane 的几何，再按所有 pane 汇总一次标题徽标。

## 5. 终端核心与协议处理

### 5.1 OSC 扫描

vte 会丢弃它不认识的 OSC，因此 PTY 线程在把字节交给解析器之前，先由 `NotificationScanner` 扫一遍：

| 序列 | 处理 |
|---|---|
| `OSC 9` / `OSC 777;notify` | 桌面通知请求 |
| `OSC 7501 ; ?` | 能力探测：先把探测之前的字节解析完，再写回 `SUPPORT_REPLY`，保证回复顺序与其他查询（如 DA1）一致；按 10 秒窗口限流 |
| `OSC 7501 ; <report>` | 状态报告，整体校验，任一字段非法则整条丢弃 |
| `OSC 133 ; A/B/C/D` | shell 集成标记；`A`/`B` 需要光标位置，因此在解析到该位置后立即标记提示符行 |
| `OSC 21337` | 选项卡状态，目前只记录日志 |

扫描器有大小上限，超长载荷整条丢弃；自由文本做 base64 解码，并过滤控制字符和双向文本控制字符，防止伪装显示内容。

### 5.2 Agent 状态

```
OSC 7501 报告 ─► ProgramTracker（terminal crate）
                   记录树：id 用 "/" 分层（子 agent），最多 256 条
                   summary()：blocked > error > working > done > idle，同级取最近
                       │
                       ▼
               ProgramActivity（每个 pane）
                   ├─ 徽标 ⏳/🔔/✅/❌，结果在 pane 获得焦点时视为已读
                   ├─ 状态转换时生成通知（只在进入状态时，不对进度变化）
                   ├─ 注意力排序 Attention（SelectNextAttention、面板、Dock）
                   ├─ 前台进程组监测：报告“运行中”后每 2 秒检查一次，
                   │   前台作业变了就结束运行中的记录（应对被 kill 的 agent）
                   └─ 记住最近上报的 app（会话恢复、worktree 用来判断 agent 类型）
```

结束“运行中”状态有三条路径：新的 shell 提示符（OSC 133 A）、前台进程组变化、子进程退出。完成和失败的结果只在用户看过之后才清除。

### 5.3 提示符标记

收到 `OSC 133;A` 时记录光标所在行；到 `B` 时，在该行第一个单元格上设置 `Flags::PROMPT`（之所以等到 `B`，是因为绘制提示符会覆盖单元格的标志）。滚动时这个待定行号会跟着平移，滚出历史则作废。`prompt_line` 沿网格查找这些标志，用于提示符跳转和 `get-text --last-command`。

### 5.4 每轮改动

```
ProgramActivity::in_turn（有记录处于 working 或 blocked）
   │  ActionContext::track_turn 在状态更新、前台变化、子进程退出后检查
   ▼
TurnTracker（每个 pane）── observe 得出 Start / End，轮次编号递增
   │  Start：取前台进程 cwd，发 Job::Start；End：发 Job::End
   ▼
后台线程（每个 pane 一个，按顺序执行）
   快照：rev-parse --show-toplevel → 复制 --git-path index 到临时文件
        → GIT_INDEX_FILE=临时 index git add --all → git write-tree
   End：再快照一次，git diff --numstat -z --no-renames 两个 tree
   │  TurnChanged(轮次, 改动或 None) 发回该 pane
   ▼
WindowStatus.changes（最需要你的 pane，否则最近一次）→ 面板一列、msg status --json
```

- 复制真实 index 作为起点，`git add` 只重新读取变化过的文件；用户的暂存区不受影响。git 命令都带 `GIT_OPTIONAL_LOCKS=0`，不和用户自己的 git 命令争锁。
- 两次快照之间检出里的任何写入都算在这一轮里；快照完成前 agent 已经写入的内容算在之前。
- 结果只保存在内存里；tree 对象是 git 的松散对象，在 `gc` 清理前一直可以 `git diff`。
- 面板按 `d`：在选中窗口的焦点 pane 下方分屏，输入 `git -C <检出> diff <before> <after>`。

## 6. 窗口、Pane 与布局

```
WindowContext
├─ display: Display          窗口、渲染器、hint、面板、消息栏几何（window_size）
├─ layout: Layout            Pane(id) | Split { direction, first, second }
├─ panes: Vec<Pane>          每个 pane：Term、Notifier(PTY)、搜索、ProgramActivity、
│                            几何 size_info、标题、恢复信息、待输入命令
├─ focused: PaneId           键盘焦点
├─ mouse_pane                按键按下时“抓住”鼠标的 pane
└─ dividers                  分隔线（RenderRect）
```

- **布局**：二叉树，按单元格划分。每次对半分，中间留 1 列或 1 行作分隔；太小（少于约 10 列或 3 行）则拒绝分割。`Layout::rects` 计算每个 pane 的格子矩形，`layout_panes` 再换算成像素几何，并调整各自的 Term 和 PTY 尺寸。
- **几何**：`SizeInfo::pane(x, y, columns, lines)` 生成一个起点在窗口内任意位置、但宽高仍是整窗的 `SizeInfo`，并带有显式的网格区域 `grid_width/grid_height`。单 pane 时直接使用整窗几何，保证与 Alacritty 原行为一致。
- **尺寸变更**：`Display::handle_update` 只负责字体和窗口大小（扣除消息栏行数），各 pane 的大小由 `layout_panes` 决定。正在搜索的 pane 会多扣一行显示搜索栏。
- **焦点切换**：向旧 pane 补发 `Focused(false)`，向新 pane 补发 `Focused(true)`（仅当窗口本身有焦点时），这样焦点上报、光标样式、结果已读都沿用原有处理。窗口标题按 pane 保存：后台 pane 设置的标题先存下来，等它获得焦点时再显示。
- **关闭**：PTY 退出事件带 pane id；窗口还有多个 pane 时只关闭该 pane，兄弟节点接管它的空间；否则关闭窗口。

## 7. 渲染管线

`Display::draw(panes, dividers, …)`：

1. `make_current`，用焦点 pane 终端的背景色清屏。
2. 逐个 pane：设置 `size_info`；分屏时调用 `renderer.resize(&pane)` 更新视口和投影；`draw_pane` 依次绘制网格、下划线等装饰、光标、vi 行号指示、搜索栏、IME 预编辑（仅焦点 pane）、hint 下划线（仅焦点 pane），最后提交该 pane 的矩形。
3. 恢复整窗几何，绘制覆盖层：分隔线、视觉铃、面板遮罩、消息栏及其文字、agent 面板、渲染计时器，以及焦点 pane 的超链接预览。
4. 提交（swap 或 present），按帧调度请求下一帧。

**两种渲染后端**（`renderer/mod.rs` 选择，macOS 默认 Metal，失败时回退到 OpenGL）：

| | Metal（`renderer/metal/`） | OpenGL（`renderer/opengl.rs`、`text/glsl3.rs`、`gles2.rs`） |
|---|---|---|
| 定位 | 单元格像素位置 = 网格起点 + 列 × 字宽，按整窗尺寸映射到 NDC | 视口设为网格区域，投影把 `[0, grid_width] × [grid_height, 0]` 映射到 NDC |
| 分屏支持 | 天然支持任意偏移 | 视口 y 按左下角原点换算：`height - padding_y - grid_height` |
| 资源 | 设备与管线全局缓存；每窗口独立命令队列、atlas、缓冲池；最多 2 个命令缓冲在途 | 每窗口一个 GL 上下文 |

**损伤追踪**：单 pane 时保留 Alacritty 的按行损伤；分屏时每帧标记为全损伤（全量重绘），用简单换取正确。损伤追踪器始终按整窗网格大小分配，以保证任何 pane 的行列索引都不越界。

## 8. 输入

- **键盘**：`input/keyboard.rs`。打开 agent 面板时键盘被面板接管，只允许它自己的快捷键穿透；hint 选择模式、内联搜索同理。快捷键由配置的 `KeyBinding` 列表匹配，动作在 `input/mod.rs` 的 `Execute` 中执行。新增的动作包括 `SelectNextAttention`、`ToggleAgentPanel`、`ScrollToPrevious/NextPrompt`、`CreateWorktreeTab`、`SplitRight/Down`、`FocusNext/PreviousPane`。
- **鼠标**：程序开启鼠标报告时，点击通常交给程序。例外是要求 `Command` 修饰键的 hint（`hint::takes_click`）：鼠标报告无法携带 Command，所以这类点击归 hint，文件路径因此可以在 Claude Code/Codex 中直接 Cmd+点击。
- **文件位置 hint**：`file_link::FILE_REGEX` 用 alacritty 自身的正则引擎匹配；触发时按前台进程的 cwd 解析路径、检查文件存在，再交给 `hints.editor`（追加 `path:line:col`）或系统默认程序打开。

## 9. IPC（`xaya msg`）

Unix socket（`$TMPDIR/Alacritty-*.sock`，路径写入环境变量 `ALACRITTY_SOCKET`）。客户端发一行 JSON（`SocketMessage`），服务端可选回复一行 JSON（`SocketReply`：`GetConfig`、`Status`、`Text`、`Error`）。

| 消息 | 处理位置 | 说明 |
|---|---|---|
| `CreateWindow`、`Config`、`GetConfig` | Processor | Alacritty 原有 |
| `Status` | Processor | 汇总各窗口 `WindowStatus` 并排序；`--wait`、`--watch` 由客户端每 0.5 秒轮询实现，`--watch` 只比较 agent 上报的字段 |
| `Focus`、`GetText`、`SendText`、`Split`、`CreateWorktree` | `Processor::ipc_request` | 目标窗口为 `-w`、`$ALACRITTY_WINDOW_ID` 或焦点窗口；作用于该窗口的焦点 pane |

安全约束：`send-text` 只能通过配置文件开启（`general.ipc_send_text`，不读运行时覆盖），并以粘贴方式写入（遵循 bracketed paste，过滤 ESC 和 `^C`）。读取请求设有 1 秒期限和 1 MiB 上限，防止慢客户端卡住共享的监听线程。

## 10. 会话恢复

```
保存（Processor::save_session）
  时机：每 15 秒 / 开关窗口 / 退出事件循环；内容不变不写盘；原子写入（.partial 后 rename）
  每个 pane → SessionWindow { cwd, agent, label }
    cwd    前台进程的工作目录
    agent  Claude Code：遍历 ~/.claude/sessions/<pid>.json，找 getpgid(pid) == 终端前台进程组的那个，
                       且对话记录 <sessionId>.jsonl 存在
           Codex：前台进程名为 codex，或该 pane 最近上报 app=codex；
                  会话取 $CODEX_HOME/sessions 下最近 7 天、cwd 相同的最新 rollout；
                  多个窗口解析到同一会话时置空，恢复时用交互选择
  文件：$XDG_STATE_HOME/xaya/session.json（默认 ~/.local/state）

恢复（Processor::restore_session，启动时；带 -e 或 --working-directory 时跳过）
  按 session.restore（Agents / All / Never）筛选，目录已不存在的丢弃 agent
  第一个作为初始窗口，其余作为同组标签（macOS）
  每个窗口的 WindowStart { input: 恢复命令, label, restore }
  shell 就绪（OSC 133 B）或 2 秒后，把命令加回车写入 PTY
  恢复后 30 秒内若尚未检测到 agent，快照继续沿用恢复前的记录，避免启动期间丢失
```

## 11. Worktree 与共用检出检测

- `worktree::create`：用 `git rev-parse --git-common-dir` 定位主检出（因此在 worktree 内再新建，仍放到主仓库旁边）；分支取第一个空闲的 `agent-N`（或指定分支：存在则检出，不存在则新建）；执行 `git worktree add`。在后台线程运行，结果经 `WorktreeReady` 回到主线程，用 `WindowStart`（标签 `⎇ 分支`、启动命令）打开新标签。
- **共用检出警告**：每 15 秒检查一次，对“上报过 app 的 pane”取前台 cwd，向上找到最近含 `.git` 的目录（worktree 中 `.git` 是文件，因此各 worktree 天然区分开）。同一检出有多个 agent 时，在相关窗口的消息栏提示一次。

## 12. 通知与 macOS 集成

- **通知后端**（`notification.rs`）：macOS 用 `UNUserNotificationCenter`，以 XaYa 自身身份发送，图标和设置项都归属 XaYa，点击会聚焦发送通知的窗口；其他 Unix 平台用 `notify-send`，也可自定义 `notifications.command`。
- **去重**：pane 处于 agent 状态上报期间，屏蔽它自己的 OSC 9/777 通知。
- **Dock 徽标**：`about_to_wait` 统计 `needs_attention` 的窗口数，有变化时更新。
- **签名**：`make app` 默认 ad-hoc 签名，也可设 `CODESIGN_IDENTITY` 使用开发者证书（带 entitlements）。

## 13. 配置

`UiConfig` 由 `ConfigDeserialize` 派生，未知字段只警告不报错，热重载会逐窗口应用（并保留 IPC 覆盖）。XaYa 新增的配置节：`terminal.program_status`、`notifications.*`、`colors.minimum_contrast`、`hints.editor`、`general.ipc_send_text`、`session.restore`、`worktree.*`。完整说明见 `extra/man/alacritty.5.scd`。

## 14. 平台支持

| 能力 | macOS | Linux/BSD | Windows |
|---|---|---|---|
| 终端、分屏、agent 状态、面板、文件 hint | ✓ | ✓ | ✓ |
| Metal 渲染器 | ✓（默认） | — | — |
| 原生通知、点击聚焦、Dock 徽标 | ✓ | notify-send | 需配置命令 |
| 前台进程监测 | ✓ | ✓ | — |
| IPC（`xaya msg`）、会话恢复、worktree 标签 | ✓ | ✓ | — |

## 15. 测试

- **单元测试**：`cargo test -p alacritty`（约 160 个），覆盖协议解析、状态与注意力规则、面板布局、文本提取、会话识别（伪造的 `~/.claude`、`~/.codex` 目录）、worktree（真实 git 临时仓库）、布局树、pane 几何、hint 正则（使用终端自身的正则引擎）。
- **Metal**：`renderer::metal::tests::gpu_*` 做像素回读测试（需 Metal 设备，`--ignored`）；`scripts/metal-smoke.py`、`scripts/macos-native-smoke.py` 打开真实窗口验证。
- **端到端**：`python3 scripts/e2e/run.py` 驱动真实的 XaYa 进程，通过 IPC 操作并截取窗口图，检查 IPC、会话恢复（伪造的 `claude`/`codex`）、worktree、分屏（macOS 上 Metal 与 OpenGL 各跑一遍）。每个测试使用独立的配置、socket、`XDG_STATE_HOME`、`HOME`、`CODEX_HOME`，工作目录在仓库之外的 `$TMPDIR/xaya-e2e/`。`scripts/record-agent.py` 用于录制真实 agent 的终端输出，复查协议行为（会消耗额度，不属于测试套件）。
- **约定**：`cargo clippy --all-targets` 零警告；用 `rustfmt +nightly` 格式化；shell 补全由测试校验（`UPDATE_COMPLETIONS=1` 重新生成）。

## 16. 已知限制与技术债

- **分屏**：pane 只能对半分，不能拖动调整大小或放大；分屏时全量重绘；会话恢复不保留分屏布局（拆成多个标签）。
- **macOS 标签分组**：恢复时把所有窗口放进同一个标签组，因为无法读取原有分组。
- **Codex 会话识别是启发式的**：Codex 不暴露进程到会话的映射，同目录多会话时退化为交互选择。
- **`--watch` 是轮询**：客户端每 0.5 秒轮询一次，不是服务端推送；它报告的是 XaYa 显示的状态（焦点窗口的结果会立即视为已读）。
- **按键与鼠标路径缺少自动化测试**：端到端测试只能通过 IPC 驱动，因为测试实例拿不到前台焦点。
- **在终端里回答 agent**：等待 OSC 7501 扩展被上游接受，见[提案](proposals/program-status-answers.zh-CN.md)。

## 17. 扩展指南

- **新增窗口级操作**：定义 `Action` → 在 `ActionContext` 中实现（通常是发一个 `EventType` 给窗口）→ 在 `WindowContext::handle_event` 中拦截处理 → 需要 IPC 时，在 `SocketMessage` 加变体，并在 `Processor::ipc_request` 中处理。
- **新增协议**：在 `NotificationScanner` 中识别 → 定义 `Sequence` 变体 → 在 PTY 事件循环中决定是否需要先把之前的字节解析完（涉及光标位置或回复顺序时需要）→ 通过 `EventProxy` 发出。
- **新增按 pane 的状态**：放进 `Pane`，并在 `handle_pane_events` 构造 `ActionContext` 时传入；需要窗口级汇总时，参照 `show_status` 和 `urgent_activity`。
- **新增定时任务**：`Scheduler` 的 `TimerId(Topic, WindowId)`；按 pane 区分时把 `PaneId` 放进 `Topic`（参照 `ProgramForeground`），事件用 `Event::for_pane` 发出。
