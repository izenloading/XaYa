"""Generate illustrative SVG mockups of the XaYa agent panel for docs/usage-manual.zh-CN.md.

The panel text mirrors the strings in alacritty/src/agent_panel.rs and managed_panel.rs.
"""
import sys
import unicodedata
from html import escape
from pathlib import Path

OUT = Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)

CW = 7.8  # px per terminal column
LH = 21  # px per line
FS = 13
MONO = "Menlo, 'SF Mono', Monaco, 'PingFang SC', monospace"
SANS = "-apple-system, 'PingFang SC', 'Hiragino Sans GB', 'Microsoft YaHei', sans-serif"

TONE = {
    "plain": "#c5c8c6",
    "dim": "#8a8f94",
    "blocked": "#f0c674",
    "error": "#e06c6c",
    "done": "#b5bd68",
    "working": "#81a2be",
    "white": "#ffffff",
}
BG = "#1d1f21"
HEADER_BG = "#3a3f45"
SELECTED_BG = "#28384a"
CHROME = "#2b2e31"


def cols(text):
    return sum(2 if unicodedata.east_asian_width(c) in "WF" else 1 for c in text)


def terminal(x, y, ncols, lines, title):
    """lines: list of (kind, segments); kind in header|row|sel|blank; segments: [(col, text, tone)]."""
    w = ncols * CW + 20
    h = 30 + len(lines) * LH + 14
    out = [
        f'<g transform="translate({x},{y})">',
        f'<rect width="{w}" height="{h}" rx="9" fill="{BG}" stroke="#0e0f10"/>',
        f'<path d="M0 9 a9 9 0 0 1 9 -9 h{w - 18} a9 9 0 0 1 9 9 v19 h-{w} z" fill="{CHROME}"/>',
    ]
    for i, color in enumerate(["#ff5f57", "#febc2e", "#28c840"]):
        out.append(f'<circle cx="{16 + i * 18}" cy="14" r="5.5" fill="{color}"/>')
    out.append(
        f'<text x="{w / 2}" y="18.5" text-anchor="middle" font-family="{SANS}" font-size="12" '
        f'fill="#9aa0a6">{escape(title)}</text>'
    )
    for i, (kind, segments) in enumerate(lines):
        ly = 34 + i * LH
        if kind == "header":
            out.append(f'<rect x="4" y="{ly}" width="{w - 8}" height="{LH}" fill="{HEADER_BG}"/>')
        elif kind == "sel":
            out.append(f'<rect x="4" y="{ly}" width="{w - 8}" height="{LH}" fill="{SELECTED_BG}"/>')
        for col, text, tone in segments:
            out.append(
                f'<text x="{10 + col * CW}" y="{ly + 15}" font-family="{MONO}" font-size="{FS}" '
                f'fill="{TONE[tone]}" xml:space="preserve">{escape(text)}</text>'
            )
    out.append("</g>")
    return "\n".join(out), w, h


def card(x, y, w, h, color, title, body):
    out = [
        f'<g transform="translate({x},{y})">',
        f'<rect width="{w}" height="{h}" rx="8" fill="{color}" fill-opacity="0.09" stroke="{color}" stroke-width="1.3"/>',
        f'<rect width="5" height="{h}" rx="2" fill="{color}"/>',
        f'<text x="18" y="26" font-family="{SANS}" font-size="15" font-weight="600" fill="{color}">{escape(title)}</text>',
    ]
    for i, line in enumerate(body):
        mono = line.startswith("$ ") or line.startswith("`")
        text = line.strip("`")
        family = MONO if mono else SANS
        size = 12.5 if mono else 13.5
        out.append(
            f'<text x="18" y="{50 + i * 22}" font-family="{family}" font-size="{size}" '
            f'fill="#2d3135" xml:space="preserve">{escape(text)}</text>'
        )
    out.append("</g>")
    return "\n".join(out)


def figure(name, width, height, title, parts):
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-label="{escape(title)}">',
        f'<rect width="{width}" height="{height}" rx="12" fill="#f6f7f9"/>',
        f'<text x="24" y="34" font-family="{SANS}" font-size="18" font-weight="600" fill="#1f2328">{escape(title)}</text>',
        f'<text x="{width - 24}" y="34" text-anchor="end" font-family="{SANS}" font-size="12" fill="#8a8f94">示意图，文案取自面板源码</text>',
        *parts,
        "</svg>",
    ]
    (OUT / name).write_text("\n".join(svg), encoding="utf-8")


GREEN, RED, BLUE, AMBER = "#1a7f37", "#cf222e", "#0969da", "#9a6700"

# 1. Window view -----------------------------------------------------------------------------
lines = [
    ("header", [(0, " Agents   ↑↓ select · ⏎ go · d diff · p project · m managed · esc close", "plain")]),
    ("sel", [(1, "1", "dim"), (4, "waiting: permission", "blocked"), (24, "Claude Code", "plain"),
             (36, "3m", "plain"), (44, "2 files +31 -4", "dim"), (64, "shop — api", "plain")]),
    ("sel", [(4, "Needs your permission — Run cargo test --locked", "dim")]),
    ("row", [(1, "2", "dim"), (4, "failed", "error"), (24, "Codex", "plain"), (36, "12m", "plain"),
             (44, "1 file +3 -0", "dim"), (64, "shop — web", "plain")]),
    ("row", [(1, "3", "dim"), (4, "working 40%", "working"), (24, "Claude Code", "plain"),
             (36, "5m", "plain"), (64, "⎇ agent-2 · shop", "plain")]),
    ("row", [(1, "4", "dim"), (4, "done", "done"), (24, "Codex", "plain"), (36, "1m", "plain"),
             (44, "4 files +120 -18", "dim"), (64, "notes  (this window)", "plain")]),
]
term, tw, th = terminal(24, 54, 100, lines, "XaYa — Agent 面板 · 窗口视图（Cmd+Shift+A）")
cy = 54 + th + 18
cw = (tw - 2 * 16) / 3
figure("panel-windows.svg", tw + 48, cy + 150 + 20, "① 窗口视图：看状态、跳过去", [
    term,
    card(24, cy, cw, 150, GREEN, "能做", ["查看每个窗口的状态、提问、时长", "Enter 或 1–9：切到那个窗口", "d：下方分屏看这一轮的 diff"]),
    card(24 + cw + 16, cy, cw, 150, RED, "不能做", ["不能在这里批准第 1 行的请求", "也不能回复 agent 的问题", "普通终端里的 agent 只能看、只能跳"]),
    card(24 + 2 * (cw + 16), cy, cw, 150, BLUE, "要处理请求时", ["按 Enter 回到该窗口", "在 agent 界面里批准或回复", "托管会话可在 m 视图里审批"]),
])

# 2. Managed view ----------------------------------------------------------------------------
lines = [
    ("header", [(0, " Claude · p project · m windows · ↑↓ select · Enter attach · a approvals · c interrupt · t end tools · v checks · r review", "plain")]),
    ("sel", [(0, " 1 waiting      a1b2c3d4 · 1 approvals · detached · /Users/me/shop", "blocked")]),
    ("row", [(0, " 2 working      e5f6a7b8 · 0 approvals · attached · /Users/me/shop.xaya/task-1763712000", "plain")]),
    ("row", [(0, " 3 stopped      c9d0e1f2 · 0 approvals · detached · /Users/me/notes", "plain")]),
    ("blank", []),
    ("blank", [(0, " （列表下方没有输入框：选中会话后只能按上面这些键）", "dim")]),
]
term, tw, th = terminal(24, 54, 126, lines, "XaYa — Agent 面板 · 托管视图（按 m）")
cy = 54 + th + 18
cw = (tw - 2 * 16) / 3
figure("panel-managed.svg", tw + 48, cy + 172 + 20, "② 托管视图：能操作，但不能打字", [
    term,
    card(24, cy, cw, 172, GREEN, "按键操作（都可用）", ["c 中断当前轮次", "a 审批 → y 批准 · n 拒绝 · x 取消", "t 结束后台工具进程（会话保留）", "v 看验证 · r 看审查 → s 发回修复"]),
    card(24 + cw + 16, cy, cw, 172, RED, "做不到", ["不能给会话随手发一句话", "例如“先别管测试，看下这个报错”", "面板里没有自由文本输入框"]),
    card(24 + 2 * (cw + 16), cy, cw, 172, BLUE, "要打字，走这两条路", ["Enter：下方分屏打开原生界面", "或在任意终端执行：", '$ xaya agent session send <ID> "…"', "见下一张图"]),
])

# 3. Two ways to type ------------------------------------------------------------------------
left = [
    ("row", [(0, "~/shop $ ", "dim")]),
    ("row", [(0, "", "plain")]),
    ("header", [(0, " xaya attach a1b2c3d4 · Claude 原生界面", "plain")]),
    ("row", [(0, " ⏺ Running cargo test --locked …", "dim")]),
    ("row", [(0, "   3 passed, 1 failed", "error")]),
    ("row", [(0, "", "plain")]),
    ("sel", [(0, " › 先别管测试，看下这个报错▏", "white")]),
    ("row", [(0, "", "plain")]),
]
right = [
    ("row", [(0, "~/shop $ xaya agent session send a1b2c3d4 \\", "plain")]),
    ("row", [(0, "    \"先别管测试，看下这个报错\"", "plain")]),
    ("row", [(0, "# stderr 先打印生成的 command_id", "dim")]),
    ("row", [(0, "# 返回命令回执：pending → succeeded", "dim")]),
    ("row", [(0, "", "plain")]),
    ("row", [(0, "~/shop $ xaya agent session command \\", "plain")]),
    ("row", [(0, "    a1b2c3d4 <command_id>", "plain")]),
    ("row", [(0, "# 回执是 unknown 时先查，不要重发", "dim")]),
]
t1, w1, h1 = terminal(24, 54, 52, left, "方式一：托管视图里按 Enter（下方分屏）")
t2, w2, h2 = terminal(24 + w1 + 20, 54, 52, right, "方式二：命令行发送")
cy = 54 + max(h1, h2) + 18
total = w1 + 20 + w2
figure("panel-typing.svg", total + 48, cy + 106 + 20, "③ 想对托管会话说一句话：在这里输入", [
    t1, t2,
    card(24, cy, w1, 106, BLUE, "原生界面里直接输入", ["和平时用 claude / codex 一样", "关掉这个分屏，会话仍在后台运行"]),
    card(24 + w1 + 20, cy, w2, 106, BLUE, "命令行发送（适合脚本）", ["同一 --command-id 重复发送不会执行两次", "可在任何终端里操作同一个会话"]),
])

# 4. Where the panel does accept text --------------------------------------------------------
left = [
    ("header", [(0, " New task · ⏎ hand over · tab switch engine · esc cancel", "plain")]),
    ("row", [(0, " What do you want done?", "plain")]),
    ("sel", [(0, " › 让 mean() 对空列表返回 0▏", "working")]),
    ("row", [(0, " Claude implements · Codex reviews", "dim")]),
]
right = [
    ("header", [(0, " Request changes · ⏎ send · esc cancel", "plain")]),
    ("row", [(0, " What should change?", "plain")]),
    ("sel", [(0, " › 再补一个空列表的单元测试▏", "working")]),
    ("row", [(0, "", "plain")]),
]
t1, w1, h1 = terminal(24, 54, 58, left, "任务视图：p → n 交办新任务")
t2, w2, h2 = terminal(24 + w1 + 20, 54, 58, right, "验收页：Enter → c 要求修改")
cy = 54 + max(h1, h2) + 18
total = w1 + 20 + w2
figure("panel-project-input.svg", total + 48, cy + 128 + 20, "④ 面板里可以打字的两处（任务视图）", [
    t1, t2,
    card(24, cy, w1, 128, GREEN, "输入的是一个新任务", ["服务建独立工作区、启动实现引擎", "之后验证、审查、发回修复都在后台", "Tab 切换由谁实现，只对这一次有效"]),
    card(24 + w1 + 20, cy, w2, 128, AMBER, "输入的是修改意见", ["发给原来实现它的会话", "改完自动重新验证和审查", "不是和某个会话的即时对话"]),
])
print("ok", sorted(p.name for p in OUT.iterdir()))
