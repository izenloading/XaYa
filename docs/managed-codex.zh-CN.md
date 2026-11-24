# Codex 托管会话

XaYa 现在可以在已有工作目录启动一个专属 Codex app-server，并在当前终端运行 Codex 原生
TUI。关闭 TUI 后后端继续存活，可用 XaYa 会话 ID 重新 attach。当前在 macOS、Codex 0.169.0
上用真实 TUI 与本地确定性模型夹具验证，不需要额外配置模型 API；日常运行沿用用户自己的
Codex 登录与配置。

协议字段参考 [OpenAI Docs：Codex App Server](https://learn.chatgpt.com/docs/app-server)，
具体兼容行为以本批真实 CLI 验证为准。

两个引擎共用的事件与控制、面板、持久化、崩溃时序、`end-tools`，以及检查点 → 验证 → 合入流程，
见[托管会话：两个引擎共用的能力](managed-sessions.zh-CN.md)。

## 使用

同时构建并安装 `xaya` 与 `xaya-agentd`，两者必须在同一目录；`make app` 已包含二者。

```sh
cargo build --workspace --locked

# 当前目录启动，默认只读；首条消息也可以进入 TUI 后再输入。
xaya run "阅读项目并说明主要模块"

# 允许修改已有工作目录；审批仍由 Codex 原生界面处理。
xaya run --sandbox workspace-write -C /path/to/project "实现需求"

# 只启动后端，不打开 TUI；返回内部会话 ID。
xaya run --detach
xaya agent session list
xaya agent session list --json

# 关闭 TUI 后，继续原来的会话。
xaya attach codex-xxxxxx

# 查看会话与原生 thread ID，或停止整个后端。
xaya agent session show codex-xxxxxx
xaya agent session stop codex-xxxxxx
```

`xaya run --json` 与 `--detach` 一样只启动，不提交首条消息。与首条消息一起使用会返回错误。
`--codex /absolute/path/to/codex` 可选择引擎版本；默认通过调用者的 PATH 定位。
调用者的 `CODEX_HOME`（默认 `$HOME/.codex`）按会话绑定，后端和 attach TUI 使用同一配置目录。
除此之外，后台引擎继承 agentd 启动时的环境；修改通过环境变量传入的凭据或代理设置后，需先
停止现有会话与服务，再启动新的服务。

`xaya agent session attach-info <id>` 输出可执行文件、参数数组、cwd 和 CODEX_HOME，供后续
pane 接入使用。调用方应直接执行参数数组，不能把这些值拼接为 shell 代码。

## 生命周期与状态

- `starting`：后端启动中或等待原生 TUI 首次握手。
- `ready`：可交互；`working`：主 thread 正在运行一轮；`blocked`：主轮次等待原生审批。
- `failed`：后端异常结束，`last_error` 给出简短原因；`stopped`：用户显式停止。
- `interrupted`：从上次服务运行恢复出的未结束会话，后端无法确认仍在运行，也尚未启动替代后端。

`attached` 与上述业务状态分开：一个仍在工作的会话可以没有 TUI。每个会话同一时间只允许
一个原生 TUI。主 thread ID 只由原生 `thread/start` 或 `thread/resume` 的响应确定，辅助
thread 广播不会改变绑定。原生界面内的新建、切换到其他 thread 和 fork 不受此批支持，应启动
新的 XaYa 会话。重新 attach 时不携带权限覆盖参数，沿用已有远程 thread 的设置。

每个会话的 `codex app-server` 运行在 XaYa 自有的 PTY 宿主中（与 Claude 相同），监听宿主私有
目录里的 unix socket；服务作为 WebSocket 客户端连接它，再把原生 TUI 的连接转发过去。后端的
所有进程（含工具子进程）带有本次启动的归属标记。后端日志进入宿主的有界回放，不再写入
运行目录下的 `<session-id>.log`。

后端和 TUI 间的请求 ID 会重新映射，避免重连后的 native ID 重用收到旧连接的响应。首次握手
结果由转发器保存，重连时不会重复初始化同一个后端；待审批请求由 Codex 在 resume 时
重新发出，重复或已过期的原生答案会丢弃。

服务有活跃会话时不因空闲退出。`xaya agent stop` 或正常 SIGTERM/SIGINT 会停止所有托管后端：
依次发送 SIGHUP、SIGTERM、SIGKILL，再按归属标记结束脱离进程树的残留进程，确认全部结束后
才释放检出并移除 attach socket。`session stop` 是结束后端，不能代替保留会话的轮次中断。

### 服务崩溃时轮次继续

服务被杀或崩溃（没有走正常关闭）时，后端和正在运行的轮次不受影响。下一次启动服务时自动
重新接管仍存活的后端：用保存的原生握手参数在新连接上 `initialize`，`thread/resume` 主 thread，
再以 `thread/read` 判断最后一轮是否仍在运行，如实恢复为 `working`、`blocked` 或 `ready`。
后端的 PID 和轮次 ID 保持不变，不会启动第二个写入者。

- 未答复的审批由 Codex 在 resume 时重新发出，可继续在面板或原生 TUI 中答复。
- 崩溃时尚未确认的程序命令仍转为 `unknown`，不重放；事件流沿用原 epoch，序号接着已落盘的
  最后一条继续（见[事件历史与断点续传](managed-sessions.zh-CN.md#事件历史与断点续传)）。
- 服务停止期间后端已经退出的，重启后记为 `failed` 并给出退出码或信号，可显式 resume。
- 宿主状态无法确认时保持 `interrupted`，拒绝启动替代后端；`session stop` 按归属标记结束进程。
- 原生 TUI 连接经由服务转发，服务崩溃时连接断开。Codex 0.169.1 的 TUI 不退出，显示
  “Reconnecting to server…”并持续重试；端点路径（会话启动目录下的 `tui.sock`）在接管后不变。
  `xaya attach`（以及不带 `--detach` 的 `xaya run`）留作 TUI 的父进程，每秒检查一次：服务消失
  且后端仍在时重新启动服务（失败按 1 秒起、最多 30 秒退避），TUI 随即自动重连到同一后端并
  resume 同一 thread。会话停止、失败或被清除，或服务与后端都已不在时，结束 TUI 并提示
  `xaya attach <ID>`，不让它一直重连。主动 `xaya agent stop` 不会被重新拉起。attach 收到 SIGTERM／SIGHUP 时转发给
  TUI 并随之退出，会话随即可再次 attach；它重启出的服务进程退出后会被回收。等待期间 attach
  忽略 Ctrl+C／Ctrl+\\，这些键交给 TUI。较旧、不会重连的 Codex 版本仍需重新 `xaya attach`。
- 启动途中、停止途中等其余崩溃时序见[崩溃时序](managed-sessions.zh-CN.md#崩溃时序)。

## 控制与审批

通用的命令、回执与事件订阅见[事件与控制](managed-sessions.zh-CN.md#事件与控制)。Codex 特有的部分：

控制端只能继续已经由原生 TUI 建立并绑定 thread 的会话，不能替代首次 attach。发送必须在当前
轮次结束后进行；不会自动排队、重试或重放。TUI 与控制端共用同一后端连接，程序发送产生的
输出也显示在原生 TUI 中。TUI 脱离时，agentd 保留主 thread 的订阅。

`cancel` 绑定发起时观察到的 turn ID，处理时发现轮次已变化会拒绝，不会中断后来的一轮。
它只实现 `turn/interrupt` 与轮次状态跟踪；轮次结束后仍在运行的后台命令用
[`end-tools`](managed-sessions.zh-CN.md#结束工具进程end-tools) 结束。

审批目前支持命令与文件变更，决策可为 `accept`、`decline` 或 `cancel`，且必须属于该请求的
`availableDecisions`。`cancel` 可能中断整轮，不能把它当作 `decline` 的同义词。只支持本次
请求的决策，不支持“整个会话允许”或修改执行策略。原生 TUI 与控制端以先到服务的有效答复
为准，后到答复不会再次写入后端；轮次完成或 resolved 事件会使待答记录过期。

控制端最多展示 16 个待审批请求；其余在 `native_only_approvals` 中计数，需在原生 TUI 处理。
完整审批详情超过 2 KiB 时保留一条 `controllable: false` 提示，不能通过控制端批准截断信息。
文件审批会关联同一轮次、同一条目的文件与补丁；补丁不可获取时也交还原生 TUI，不能只凭授权根目录批准。

## 窗口自动恢复

XaYa 的已知 Codex thread 自动恢复通过 `xaya agent restore <native-thread-id>` 查询托管目录：
活跃托管会话仅重新 attach；inactive 托管会话提示显式恢复，不自动新建后端；未托管的 thread
继续走原生 `codex resume`。目录不可用时不会盲目回退。目录／进程到原生 ID 的发现仍保留原有
启发式限制，尚未实现精确 pane 身份绑定。

## 当前边界

只支持已有工作目录，最多保留 16 条会话记录（与 Claude 会话共用同一容量）。达到容量时先清除已结束记录；
16 个活跃会话时拒绝继续启动。socket 位于服务运行目录下的私有随机子目录，目录 0700、
socket 0600。控制接口仍为 64 KiB；Codex 转发帧独立限制为 64 MiB，允许大型原生插件元数据。
慢速 TUI 写入超时会断开该 TUI，后端继续读事件。

尚未接入：精确 pane 身份绑定、自动 worktree；没有 attach 的会话在服务崩溃后不会自动重启服务。

## 验证

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings

# 可选：真实 Codex 原生 TUI + 正式 Rust 服务 + 本地模型夹具。
cargo build --workspace --locked
python3 -m venv /tmp/xaya-l1-venv
/tmp/xaya-l1-venv/bin/python -m pip install -r scripts/agent-probes/requirements.txt
/tmp/xaya-l1-venv/bin/python scripts/agent-probes/managed_codex.py \
  --output /tmp/xaya-l1-new-run
```

输出目录必须不存在且位于 Git 仓库外。该验证使用隔离 HOME、CODEX_HOME、假 key、私有服务
目录和本地模型，不使用真实账户。产物包含原生 TUI 输出、会话状态、模型请求与 `landing.json`。
同一场景从原生审批写文件走到私有检查点、可信项目验证、独立集成验证和本地合入，覆盖已检出
及未检出目标、重启重试、源 HEAD/index 保持及无关忽略文件保留。证明协议和固定样本的 Git
执行链路，不证明模型质量，也不覆盖 XaYa GUI 键盘交互或 GPU 渲染。
