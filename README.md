<p align="center">
    <img width="200" alt="XaYa icon" src="./extra/logo/xaya-icon.png">
</p>

<h1 align="center">XaYa</h1>
<p align="center">面向 AI 编码工作流的 macOS 高性能终端</p>

XaYa 让你在同时运行多个 Claude Code、Codex 会话时，看清任务进度、及时处理等待请求，并快速回到对应的工作现场。

由 **izenloading** 维护，fork 自 [Alacritty](https://github.com/alacritty/alacritty)。当前专注于 **macOS 与 Apple Silicon** 的体验、性能和稳定性优化。

- 当前版本：**0.0.1**（macOS build **1**）
- 系统要求：**Apple Silicon Mac，macOS 11 或更新版本**；暂不支持 Intel Mac
- 应用：`XaYa.app` · 命令：`xaya`

## 功能特点

### 掌握每个 Agent 的状态

窗口标题显示正在工作、等待处理、已完成或失败等状态；有进度信息时显示百分比。Agent 面板集中列出各窗口的状态、消息和等待时长，让你快速找到需要关注的任务。

- `Cmd+Shift+A`：打开 Agent 面板。
- `Cmd+J`：跳转到需要处理的窗口，优先等待请求，其次失败与完成。
- `xaya msg status`：从命令行查看状态，也支持 JSON、等待结束和持续监听。

状态感知基于 Program Status Protocol（OSC 7501）。Claude Code、Codex 等工具的状态展示取决于所用版本是否上报该协议。

### 需要你时，macOS 原生提醒

Agent 等待授权、提问、失败或完成时，通过原生桌面通知提醒你。点击通知可回到对应窗口或标签，Dock 徽标显示尚未处理的等待与失败数量。默认只在窗口不在前台时通知，减少打断。

收到提醒后，在 Agent 自己的界面中处理请求。对于普通终端里运行的 Agent，从通知或面板直接批准请求仍属于[协议扩展提案](docs/proposals/program-status-answers.zh-CN.md)；通过 `xaya run` 启动的托管会话可在面板中直接审批，见下文“托管会话”。

### 分屏与 Worktree，组织并行开发

- `Cmd+D`：右侧分屏，让 Agent、shell 或日志并排显示。
- `Cmd+Shift+D`：下方分屏。
- `Cmd+Shift+T`：创建独立 git worktree，在新标签中启动当前 Agent。
- 多个 Agent 共用同一个代码检出目录时给出提醒，帮助减少同时修改同一份文件的问题。

Worktree 用于隔离代码检出；合并和清理仍由用户管理。

### 重启后继续工作

自动记录 Agent 会话与工作目录，下次启动时重新打开并尝试恢复 Claude Code、Codex 会话。主动关闭的窗口不会恢复。

恢复能力受 Agent 的会话信息影响：Codex 会话存在歧义时会让你选择；分屏会恢复为多个标签，暂不还原原有分屏布局。

### 原生 Metal 渲染

macOS 默认使用 Metal 渲染文本与终端界面，保留 Alacritty 的终端仿真、搜索、vi 模式、选择和配置体系。Metal 不可用时自动回退 OpenGL，也可手动选择 OpenGL。

详见 [Metal 渲染器说明](docs/metal.md)。

### Shell 与编辑器衔接

安装 zsh、bash 或 fish 集成后，可以在命令提示符之间跳转，接收长命令完成通知，并提取上一条命令的输出。按住 `Cmd` 单击 Agent 输出中的文件路径即可打开文件；配置编辑器后可跳转到具体行列。

`xaya msg` 还提供窗口聚焦、文本读取、分屏及 worktree 创建等命令，方便接入自己的脚本。向终端发送文本需要显式开启配置。

### 托管会话（实验性）

`xaya run` 启动由 XaYa 托管的 Codex 或 Claude 会话：引擎运行在 XaYa 自有的 PTY 宿主中，关闭原生 TUI
后继续运行，可再次 `xaya attach`；服务崩溃时轮次不中断，重启后自动重新接管，已停止的会话可显式恢复。

```sh
xaya run "阅读项目并说明主要模块"                 # Codex，默认只读
xaya run --engine claude "阅读代码并解释这个项目"   # Claude
xaya attach <内部ID>
```

- **观察与控制**：`xaya agent session` 提供实时事件（落盘后可按游标续传）、程序发送、指定轮次中断、
  审批，以及只结束后台工具进程而保留会话的 `end-tools`。
- **面板**：Agent 面板按 `m` 切到托管视图，看板由服务推送；`c` 中断轮次，`a` 审批，`t` 结束工具进程，
  `v` 查看验证结果。会话与回执保存到 SQLite。
- **检查点 → 验证 → 合入**：`xaya checkpoint create` 把工作目录保存到私有 Git 引用，不改动分支和暂存区；
  `xaya check run`／`check project` 在绑定版本上验证，`check bind` 证明验证确实读取这棵树；
  `xaya land preview`／`verify`／`run` 生成 squash 候选、在独立检出中重跑验证，再通过比较交换推进本地
  目标分支，中断后可 `land reconcile`／`land resume`。Claude 会话需先停止并确认进程结束。
- **审查**：`xaya review run <checkpoint-id> --check <check-id>` 让另一个引擎在私有检出中只读审查，结论由
  服务按固定规则判定（approved／needs-fix／blocked／invalid），没有通过的验证不会得到 approved。
- **任务**：`xaya task promote <session-id> --goal … --criterion …` 记录目标和可检验的验收条件；之后的审查带上
  确定性的交接包，逐条判定验收条件；`xaya review send <review-id>` 把 needs-fix 的意见发回实现会话，任务上的
  发现台账跟踪各条问题是否再次出现。配置并信任项目验证计划后，可用 `xaya task cycle <task-id>`
  接续执行检查点、验证、审查和修复，直到通过或需要人工处理。创建任务时加 `--dual-review` 可要求两个引擎审查。

详见 [Codex 托管会话](docs/managed-codex.zh-CN.md)、[Claude 托管会话](docs/managed-claude.zh-CN.md)
与[两个引擎共用的能力](docs/managed-sessions.zh-CN.md)。

## 从源码安装

需要 Rust **1.88.0 或更新版本**、Xcode Command Line Tools，以及用于生成手册的 `scdoc`。使用 Homebrew 可安装 `scdoc`：

```sh
brew install scdoc

# 构建 macOS 应用
make app
cp -R target/release/osx/XaYa.app /Applications/

# 可选：生成 target/release/osx/XaYa.dmg
make dmg
```

也可用 `cargo build --release` 仅构建命令行程序，产物为 `target/release/xaya`。原生通知等 macOS 集成建议通过应用包使用。更多构建说明见 [INSTALL.md](INSTALL.md)。

打开 XaYa 后运行 `claude` 或 `codex`，即可开始使用。建议在 `~/.zshrc` 末尾加入 Shell 集成：

```sh
source /Applications/XaYa.app/Contents/Resources/shell-integration/xaya.zsh
```

bash、fish 的配置方法见[使用手册](docs/user-guide.zh-CN.md#shell-集成)。

## 配置与兼容

XaYa 不会自动创建配置文件，依次查找并使用第一个存在的配置：

1. `$XDG_CONFIG_HOME/xaya/xaya.toml`
2. `$HOME/.config/xaya/xaya.toml`
3. Alacritty 的兼容配置位置，例如 `$HOME/.config/alacritty/alacritty.toml`

仓库里的 [`xaya.example.toml`](xaya.example.toml) 是带中文说明的示例配置，列出了全部配置项及默认值，复制后按需修改即可：

```sh
mkdir -p ~/.config/xaya
cp xaya.example.toml ~/.config/xaya/xaya.toml
```

已有 Alacritty 配置、terminfo 和环境变量保持兼容。也可通过 `xaya --config-file <路径>` 指定配置，修改配置后自动生效。

代码保留 Linux、BSD 和 Windows 的平台实现，其他平台使用 OpenGL；**当前优化与验证重点为 macOS**，平台能力差异见使用手册和安装文档。

## 文档

- [中文使用手册](docs/user-guide.zh-CN.md) · [English user guide](docs/user-guide.md)
- [完整功能说明](docs/features.md)
- [功能与技术现状](docs/technical-overview.zh-CN.md) · [技术架构](docs/architecture.zh-CN.md)
- [用户使用说明（功能完整度与上手指南）](docs/usage-manual.zh-CN.md)
- [Metal 渲染器](docs/metal.md)
- [构建与安装](INSTALL.md) · [参与贡献](CONTRIBUTING.md)

## 许可证与来源

XaYa fork 自 [Alacritty](https://github.com/alacritty/alacritty)，采用 [Apache License 2.0](LICENSE-APACHE)。上游归属信息见 [NOTICE](NOTICE)，部分配置组件采用 MIT 或 Apache-2.0 双许可证，以各组件声明为准。
