# Claude 托管会话（L1，实验性）

Claude Code 作为**普通交互式 TUI**，运行在 XaYa 自己持有的 PTY 宿主中（不再使用 `--bg`
后台会话）。会话身份由 XaYa 选择，Mods 负责结构化事件与输入门禁，进程退出与归属由宿主和
启动标记确认。需要已安装并完成配置的 Claude Code；本轮验证版本为 **2.1.326**（此前批次为 2.1.325）。调用端设置了
`CLAUDE_CONFIG_DIR` 时沿用它（目录必须存在）；未设置时 XaYa 也不设置，并从 Claude 的环境中移除继承来的值，由 Claude
使用自己的默认目录。原因：只要设置了这个变量（即使指向 `~/.claude`），Claude 在 macOS 上就改用另一个钥匙串条目，
claude.ai 登录会失效（实测发现）。XaYa 不写入用户的
settings、登录信息或项目信任（Claude 自身的改写见下文“已知现象”）。

两个引擎共用的事件、面板、崩溃时序、
`end-tools` 与检查点 → 验证 → 合入流程见[托管会话：两个引擎共用的能力](managed-sessions.zh-CN.md)。

## 使用

```sh
# 首条提示在会话启动时提交，然后在当前终端打开 Claude 原生 TUI。
xaya run --engine claude "阅读代码并解释这个项目"

# 也可只启动，返回 XaYa 内部 ID。
xaya run --engine claude --detach "阅读代码并解释这个项目"
xaya agent session list --json
xaya agent session show <内部ID>
xaya agent session watch <内部ID>
xaya attach <内部ID>

# 结束该会话的所有进程（包括后台工具），确认后释放检出。
xaya agent session stop <内部ID>

# 在新的宿主中继续同一原生会话（--resume），不重放首条提示。
xaya attach <内部ID>                  # 已停止时先恢复，再打开原生 TUI
xaya agent session resume <内部ID>    # 只恢复，不打开 TUI
```

可用 `--claude /绝对路径/claude` 指定可执行文件。`--json` 与 `--detach` 均不会打开 TUI。
首条提示必须显式提供。

**脱离**：在 attach 的终端中按 `Ctrl+]` 或 `Ctrl+Z`，或直接关闭面板；Claude 继续运行，
可再次 `xaya attach`。`Ctrl+Z` 不会传给 Claude：宿主里没有 shell 能把挂起的程序恢复，
所以这里把它当作脱离键（与原先后台会话的行为一致）。宿主在 attach 时还会对程序发送
SIGCONT，防止它停在挂起状态。脱离是有序的：先关闭发送方向并读空数据，避免刚输入的内容丢失。

Agent 面板按 `m` 切到托管视图，会同时列出 Codex 与 Claude。Claude 的状态和文本来自
Mods。按 `c` 中断选中会话的当前轮次，按 `t` 在轮次结束后结束其后台工具进程，按 `a` 查看并回答
权限请求（见“面板审批”）。启动时显式选择
`manual` 权限模式。**manual 不是 read-only OS 沙箱**；Claude 路径拒绝所有 `--sandbox`
参数，避免与 Codex 权限混淆。

## 身份与进程归属

- XaYa 生成 UUID v4，以 `--session-id` 启动 Claude；该 ID 就是 `native_thread_id`。
  Mods 的第一个事件必须带同一 ID，否则观察流失效。`native_job_id` 始终为空：
  不存在 Claude 的后台 job，也不调用 `claude agents`、`attach`、`stop` 或 `daemon`。
- 每次启动生成一个 128 位随机标记，放在环境变量 `XAYA_OWNER` 中。宿主、Claude 和所有
  子孙进程都会继承它，包括 `setsid` 后脱离进程树、被重挂到 launchd 的进程。
  macOS 通过 `KERN_PROCARGS2` 读取精确的环境变量，Linux 读取 `/proc/<pid>/environ`；
  进程身份为 (pid, 启动时间)，发送信号前重新采样，不会误伤复用了 PID 的进程。
- 标记和宿主目录在宿主启动**之前**写入 SQLite（`Launch::Owned`），因此即使服务在中间崩溃，
  也能按标记找到这次启动的所有进程。

## 停止与检出释放

`session stop` 依次执行：

1. 关闭 Mods 输入门禁。
2. 通过宿主向 Claude 的进程组发送 SIGHUP；10 秒内未退出则 SIGTERM，再 5 秒后 SIGKILL。
3. 扫描带标记的存活进程，逐个核对身份后 SIGKILL，最多三轮。
4. **没有任何带标记的进程存活**时才记为 `Ended`，此后同一检出才能 checkpoint/check/land。

Claude 自行退出（例如 `/exit`）时同样执行第 3、4 步：会话结束后不再留下仍在写入检出的
后台进程。退出码为 0 时状态为 `stopped`，否则为 `failed`，两者都释放检出。

这一判断以进程证据为准，取代了原先“Mods 工具台账为空”的要求：TaskStop 的成功回执
不能证明进程已退出（探针 O5 实测），
而标记扫描可以。如果 Mods 台账中仍有未完成的工具，但进程已全部确认结束，会在
`last_error` 中记录这一差异，不阻止释放。

## 服务崩溃与重新接管

宿主独立于服务，服务崩溃后 Claude 和当前轮次照常运行。重启后服务自动重新接管仍在运行的
Claude，不重启它、不重放首条提示或任何控制命令：

- **端点由宿主持有**：Mods 配置的 loopback HTTP 端口由服务创建，以继承的监听描述符交给
  PTY 宿主（对 Claude 设为 close-on-exec，Claude 自身不持有）。宿主把每个连接转发到启动私有
  目录中的 `m.sock`，由服务监听。服务停机期间转发直接断开，Mods 看到的就是服务不可用：
  `prompt.submit` 被 drop、`tool.call` 被 deny，与原来一样 fail closed。
- **桥接记录**：启动私有目录中的 `bridge.json`（0600，拒绝软链接）保存端点与 token，
  重启的服务用它重新监听 `m.sock`。没有这个记录的启动（例如旧版本服务启动的）无法接管，
  保持 `interrupted`，处理方式与以前相同：`session stop` 后再 `resume`。
- **快照对齐**：新服务在收到快照前对事件与控制请求一律回答 `428`，不准入任何输入或工具，
  权限请求只交给原生提示。Mods 收到 `428` 后发送一次快照：它已分配的最后序号、当前主轮次、
  已准入且未结束的工具、已启动的后台任务。快照与序号在 Mods 中同一步更新，所以两者一致；
  工具只有在准入回执送达后才计入。快照覆盖此前所有事件：队列中尚未送达的事件被丢弃（它们的
  hook 已经 fail closed），停机期间计入的丢失也清零，之后的事件从快照序号的下一号继续。
  快照身份不一致、出现被中断的工具退出、重复的工具 ID 或格式错误，都会使会话标为
  `interrupted`，进程仍归 XaYa 所有，由 `session stop` 结束。
- **接管后**：有主轮次时为 `working`，否则为 `ready`；程序发送、中断、面板审批、attach、
  stop 与新启动相同。接管前已准入的工具可以在接管后正常结束。25 秒内没有收到快照时，会话
  标为 `interrupted`。
- **停机期间 Claude 已退出**：重启时宿主的退出记录已存在，服务按标记结束残留进程（包括
  `setsid` 脱离的孙进程），确认后释放检出；退出码 0 为 `stopped`，否则为 `failed`。
- **宿主状态无法确认**（既没有退出记录，也无法连接，但仍有带标记的进程）：保持
  `interrupted`，由 stop 按标记处理。
- **启动途中或停止途中崩溃**：见[崩溃时序](managed-sessions.zh-CN.md#崩溃时序)，
  两个引擎规则相同。

停机期间被 drop 的输入，Claude 会放回自己的输入框（真实 2.1.326 验收中观察到）。这段文字
不会自动发送；用户下次回车会连同它一起提交，需要时先清空输入框。XaYa 不会重发它。

## 显式恢复

会话已停止、失败或中断后，`xaya attach <内部ID>` 或 `xaya agent session resume <内部ID>`
可以在原生 TUI 中继续同一会话：

- **前提**：上一次启动的标记扫描为空，即宿主、Claude 和所有子孙进程都已结束。只要还有一个带
  标记的进程（例如无法接管、仍在运行的 Claude），恢复就会被拒绝，需要先 `session stop`。
  已被重新接管的会话处于活动状态，resume 原样返回它。
  监督线程尚未结束时同样拒绝。工作区正在 checkpoint 时也拒绝。
- **启动**：`claude --resume <同一 UUID> --permission-mode manual --plugin-dir <新私有 mod>
  --settings <新私有设置>`，不带 `--session-id` 和首条提示。每次恢复都是新的启动：新的私有目录、
  新的 `XAYA_OWNER` 标记、新的 Mods token，`Launch::Owned` 同样在宿主启动前落盘。
  旧启动已确认结束，它的私有目录随即删除。
- **身份**：新启动的第一个 Mods 事件必须带同一 UUID，否则观察流失效、会话标为 `interrupted`，
  其进程仍归 XaYa 所有，由 `session stop` 结束。恢复的 TUI 不会自行开始轮次，Mods 核对
  `session.start` 后状态直接变为 `ready`；`xaya attach` 等到这一步才申请 attach 租约。
- **检出**：恢复后 Claude 重新占用检出，直到下一次 stop 或自行退出并确认无存活进程。
- 运行中的会话调用 resume 原样返回，不会再启动一次。

## 指定轮次中断

```sh
xaya agent session show <内部ID>
xaya agent session cancel <内部ID> --turn-id <turn_id> --command-id cancel-1
xaya agent session command <内部ID> cancel-1
```

省略 `--turn-id` 时，CLI 先读取当前轮次，再携带明确 ID 提交；不会中断读取后新开始的轮次。
回执可能先为 `pending`，按同一 command ID 查询结果。相同 ID 和参数只执行一次，改参数复用
ID 会报冲突；服务重启后仍可读取旧回执。面板按 `c` 使用同一链路。

- `succeeded` 只表示原生 `turn.abort` 接受请求；对应 `turn_completed` 的 `interrupted`
  事件才表示该轮次已结束，均不证明后台工具退出。轮次结束后用
  `xaya agent session end-tools <内部ID>` 结束后台 Bash 及其子孙而保留会话（见
  [结束工具进程](managed-sessions.zh-CN.md#结束工具进程end-tools)），或用 stop 结束整个会话。
- 已过期轮次在派发前拒绝；Mods 本地记录与原生 API 还会核对 turn ID。
- 排队和已派发请求各有 5 秒期限，由服务周期检查。未派发即超时标为 `failed`；已派发但
  没有确认标为 `unknown`。断线、桥失效和服务重启不会自动重放，也不接受迟到回执改写结果。

## 程序发送

```sh
xaya agent session send <内部ID> "根据审查意见修复测试" --command-id fix-1
xaya agent session command <内部ID> fix-1
```

- 只在会话为 `ready` 且没有进行中的轮次时接受；否则直接拒绝，不排队。
- 消息先落盘为 `pending`，再由 Mods 轮询**取走一次**，以 `$.prompt.submit({ text, asUser: true })`
  提交，在原生 TUI 中显示为用户输入。提交可能要等它开启的轮次结束才返回，所以不占用轮询，
  期间指定轮次中断照常可用。
- **以轮次为准**：只有当一个携带完全相同文本的主轮次开始时，回执才变为 `succeeded`，
  并记录该轮次的 `turn_id`。`submit()` 的返回值本身不算送达。
- **明确失败**：Claude 拒收（例如原生 Esc 之后，见“已知现象”）时为 `failed`，`error` 中带有
  Claude 给出的原因，并提示在原生 TUI 中继续；该消息没有进入模型。
- **不确定**：5 秒内没有对应轮次开始，或者先开始了另一个轮次，为 `unknown`，消息可能稍后仍会
  执行。
- 相同 command ID 加相同文本只执行一次，改文本复用 ID 会报冲突；服务重启后仍可读取旧回执。
  任何情况下都不会自动重发，迟到的确认也不会改写已有结果。

## 面板审批

```sh
xaya agent session approvals <内部ID>
xaya agent session respond <内部ID> <approval_id> accept --command-id allow-1   # 或 decline
```

- Claude 发出权限请求（PermissionRequest hook）时，原生 TUI 照常显示提示，hook 同时挂起，
  最长 290 秒（Claude 侧的 hook 超时为 300 秒），等待面板作答。会话状态为 `blocked`，审批中带
  工具名与输入，可选 `accept`／`decline`。
- **对应到工具**：真实的 hook 载荷没有 `tool_use_id`，服务把请求对应到 Mods 已登记、同名且尚无
  请求的那个进行中的主工具。找不到或有多个候选、详情超过 2 KiB、已有 3 个请求在等待、或没有
  进行中的轮次时，该请求只能在原生 TUI 中回答（`native_only_approvals`），与此前一致。
- **面板先答**：答复直接交给挂起的 hook，只投递一次；审批立即消失。allow／deny 写回 Claude 后
  回执为 `succeeded`，写回失败为 `unknown`。原生提示随之关闭，工具按答复执行或被拒绝。
- **原生先答**：该工具结束（或轮次结束、观察流失效、等待超时）即视为原生 TUI 已作答，hook 以
  空输出释放，审批过期；之后的面板答复以 stale 拒绝。两边几乎同时作答时，以 Claude 先收到的为准，
  工具结果反映最终决定。
- 相同 command ID 只执行一次，服务重启后仍可读取回执；任何答复都不会重放。服务崩溃会断开
  挂起的 hook，原生提示仍可作答。

## 连接与存储

- 插件、token、settings 与桥接记录写入本次启动的私有目录，配置文件为 0600；HTTP 仅监听
  loopback（端口由 PTY 宿主持有，转发到服务的 `m.sock`，同时最多 16 个转发连接），
  请求需带随机 token，连接数、帧大小和读取时间有界。插件在 `prompt.submit` 和 `tool.call`
  执行前请求服务授权；断线时分别返回明确的 drop／deny。
- 宿主目录 0700，socket、token 与记录文件 0600；同一时间只允许一个 attach。XaYa 的 attach
  租约与事件订阅共用最多 8 个长连接。由于终端由 XaYa 持有，不存在外部 `claude attach`
  绕过租约的问题。
- 事件丢失、乱序或身份变化后标为 interrupted，不用迟到完成事件猜测 ready。服务重启时的
  缺口由 Mods 快照对齐（见上文），不是由迟到事件推断。XaYa 自己发出的事件会落盘，可用
  `xaya agent session history` 查询、`watch --after` 续传（见
  [事件历史与断点续传](managed-sessions.zh-CN.md#事件历史与断点续传)）；但停机期间 Claude 产生、
  未送达服务的正文不会补发。
- **中断时的重投**：轮次被中断（原生 Esc）时，Claude 会取消该轮次各个 hook 中正在进行的
  `$.http.fetch`（`user-cancel`）。这时观察事件还不算丢失：它按顺序留在插件队列里，由下一个 hook
  或 100 ms 定时器重投。5 秒内仍送不到，或队列超过 256 条，才计为丢失，服务随即按真实丢失处理。
  服务把与上一条完全相同的重投视为无操作，并返回第一次的结果；同一序号但内容不同仍是错误。
  准入事件（`input.start`／`tool.start`）在自己的 hook 中仍按失败处理（drop／deny），重投时
  标记为 `late`，不再准入，也不登记工具。

## 已知现象

- **原生 Esc 之后程序发送被拒收**：在原生 TUI 中按 Esc 中断轮次后，Claude 会拒收下一条程序发送，
  原因为 “the session was stopped and nobody has sent input since”。回执为 `failed`，消息没有
  进入模型；在原生 TUI 中输入一次后即可恢复。XaYa 不会代为重发。
- **迟到的 Esc**：轮次刚开始、尚未开始输出时按下的 Esc，可能被原轮次忽略，转而中断下一个轮次
  （包括程序发送开启的轮次）。这时发送回执仍为 `succeeded`（轮次确实开始了），随后该轮次以
  `interrupted` 结束。

- **自动模式提示**：普通交互式 Claude 在会话空闲后会弹出“Make auto mode your default
  permission mode?”，并默认选中 Yes。验收中误输入的回车曾因此把会话切换为 auto，并把隔离
  用户配置的 `defaultMode` 改成 `auto`。XaYa 现在在私有 `--settings` 中同时写入
  `permissions.defaultMode: manual`（`--permission-mode` 仍决定起始模式），修复后该提示不再出现。
- **`manual` 被改写为 `default`**：验收结束后，隔离用户配置中的 `defaultMode` 由 `manual`
  变为 `default`。此前所有 `--bg` 验收产物中同样如此，说明这是 Claude 自身的行为，并非本批引入；
  两者表示同一种“每次询问”的模式。XaYa 本身从不写这个文件。

## 本批边界

1. **面板审批依赖工具对应。** 同名工具并行请求权限、或请求来自子 Agent 时，只能在原生 TUI
   中回答；审批不支持“本次会话始终允许”等范围选项。
2. **重新接管的范围。** 停机期间挂起的权限请求随连接断开交给原生提示，接管后不会重新出现在
   面板上；停机期间的正文、子 Agent 轮次和原生审批状态不在快照中，只恢复主轮次、工具与后台
   任务台账。宿主本身崩溃（不是服务）时无法接管，按标记 stop。
3. 主动清除自身环境变量的进程会丢失标记，又已离开进程树时无法被发现；这类进程不在保证内。
   `end-tools` 只识别经 Bash 运行的工具（`$CLAUDE_ENV_FILE`），并要求没有进行中的轮次。
4. 只在 macOS 上验证；Linux 的 `/proc` 路径有单元测试，未运行真实 Claude。
5. 宿主本身不是沙箱；外部编辑器与绕开 XaYa 的进程仍不在检出门禁保证内。

## 验收

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
node --test scripts/agent-probes/test_managed_claude_mod.mjs

# 可选：真实原生 CLI、Rust 服务与本地固定模型，不使用真实账户。
cargo build --workspace --locked
/tmp/xaya-acceptance-venv/bin/python scripts/agent-probes/managed_claude.py \
  --xaya target/debug/xaya --output /tmp/xaya-managed-claude-new-run
```

输出目录必须不存在且在 Git 仓库外。夹具单独创建 HOME、Claude 配置、假 key、onboarding
和项目信任，全部请求只到本地模型；这些设置只属于测试，不进入产品启动路径。

真实验收覆盖：首条提示与原生输入、独占 attach、`Ctrl+Z` 脱离与再次 attach、原生审批并
实际写文件、Mods 实时正文、指定轮次中断后继续输入、真实后台 Bash 工具及其 `setsid` 孙进程
在 stop 后结束且无延迟写入、同检出中无标记的进程不受影响、停止后 checkpoint/check/land、
服务被杀后 Claude 拒绝新输入、重启后同一进程被重新接管并可继续输入，`end-tools` 只结束后台
工具而保留会话，以及停止的会话经 `xaya attach`
以 `--resume` 在新宿主中继续（模型请求的会话 ID 不变，历史上下文完整）；程序发送由对应轮次
确认并显示在原生 TUI 中，相同 command ID 返回同一回执；原生先答使面板请求过期且迟到答复被拒绝，
面板拒绝不写文件、面板允许写入文件；原生 Esc 中断流式轮次后，下一条程序发送
被拒收为 `failed`，且没有进入模型。它不证明真实模型质量或 GUI/GPU 交互。
