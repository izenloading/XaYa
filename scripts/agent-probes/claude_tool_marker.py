#!/usr/bin/env python3
"""T1: can XaYa tell Claude's tool processes from Claude itself and its helpers?

A SessionStart command hook appends `export XAYA_TOOL=<marker>` to `$CLAUDE_ENV_FILE`. Claude
documents that file as sourced before every Bash tool command. The probe records which
processes then carry the marker: the background Bash tool, its setsid() grandchild, Claude
itself and a stdio MCP server started by Claude. Real installed Claude, loopback model, no account.
"""

import argparse
import asyncio
import json
import os
from pathlib import Path
import secrets
import shutil
import shlex
import subprocess
import sys
import time

from claude_fixture import ClaudeFixture
from codex_dual import Terminal, until
from run import isolated_env

TOOL = """import json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
read, write = os.pipe()
if os.fork() == 0:
    os.setsid()
    if os.fork() == 0:
        os.close(read)
        os.write(write, json.dumps({"pid": os.getpid(), "tool": os.environ.get("XAYA_TOOL")}).encode())
        os.close(write)
        time.sleep(60)
        os._exit(0)
    os._exit(0)
os.close(write)
grandchild = json.loads(os.read(read, 4096))
os.wait()
(root / "tool.json").write_text(json.dumps({"pid": os.getpid(), "tool": os.environ.get("XAYA_TOOL"),
    "owner": os.environ.get("XAYA_OWNER"), "grandchild": grandchild}))
time.sleep(60)
"""

# A stdio MCP server only has to start: it records its own environment, then idles.
MCP = """import json, os, pathlib, sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({"pid": os.getpid(), "tool": os.environ.get("XAYA_TOOL"),
    "owner": os.environ.get("XAYA_OWNER")}))
for line in sys.stdin:
    request = json.loads(line)
    if request.get("method") == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {
            "protocolVersion": request["params"]["protocolVersion"], "capabilities": {},
            "serverInfo": {"name": "probe", "version": "0"}}}), flush=True)
    elif "id" in request:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"tools": []}}), flush=True)
"""


def environment(pid):
    """The exact environment of a process on macOS (`ps -E`), or None once it is gone."""
    result = subprocess.run(["ps", "-E", "-ww", "-o", "command=", "-p", str(pid)],
                            capture_output=True, text=True)
    return result.stdout if result.returncode == 0 else None


async def probe(binary, root):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()
    (root / "tool.py").write_text(TOOL)
    (root / "mcp.py").write_text(MCP)
    tool_command = shlex.join([sys.executable, str(root / "tool.py"), str(root)])
    marker = secrets.token_hex(16)
    owner = secrets.token_hex(16)
    with ClaudeFixture(workspace, tool_command) as fixture:
        local_env = dict(env, ANTHROPIC_BASE_URL=fixture.base_url, XAYA_OWNER=owner,
                         CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
        config = Path(env["CLAUDE_CONFIG_DIR"])
        (config / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True, "theme": "dark",
            "autoUpdates": False, "customApiKeyResponses": {"approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}}}))
        settings = root / "settings.json"
        hook = f"printf 'export XAYA_TOOL=%s\\n' {marker} >> \"$CLAUDE_ENV_FILE\""
        settings.write_text(json.dumps({"permissions": {"defaultMode": "manual"},
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": hook}]}]}}))
        mcp = root / "mcp.json"
        mcp.write_text(json.dumps({"mcpServers": {"probe": {"command": sys.executable,
            "args": [str(root / "mcp.py"), str(root / "mcp.json.env")]}}}))
        terminal = Terminal([binary, "--model", "claude-sonnet-4-6", "--tools", "Bash",
                             "--allowedTools", "Bash", "--permission-mode", "manual",
                             "--setting-sources", "user", "--settings", str(settings),
                             "--mcp-config", str(mcp), "--strict-mcp-config",
                             "--debug-file", str(root / "debug.log")],
                            local_env, workspace, root / "tui.bin")
        try:
            await until(lambda: (root / "mcp.json.env").exists(), "MCP server started", timeout=40)
            await asyncio.sleep(2)
            terminal.write("XAYA_CLAUDE_CANCEL_TOOL")
            await asyncio.sleep(0.3)
            terminal.write("\r")
            await until(lambda: (root / "tool.json").exists(), "background tool started", timeout=60)
            tool = json.loads((root / "tool.json").read_text())
            server = json.loads((root / "mcp.json.env").read_text())
            # Claude itself (the PTY program after exec); its children include the tool shell.
            claude_env = environment(terminal.process.pid) or ""
            report = {
                "tool_marked": tool["tool"] == marker,
                "setsid_grandchild_marked": tool["grandchild"]["tool"] == marker,
                "tool_owned": tool["owner"] == owner,
                "mcp_server_unmarked": server["tool"] is None and server["owner"] == owner,
                "claude_unmarked": bool(claude_env) and f"XAYA_TOOL={marker}" not in claude_env
                                   and f"XAYA_OWNER={owner}" in claude_env,
                "evidence": {"tool": tool, "mcp": server, "claude_pid": terminal.process.pid},
            }
            report["status"] = "passed" if all(v for k, v in report.items() if k != "evidence") else "failed"
            return report
        finally:
            await terminal.close()
            for pid in [json.loads(p.read_text()).get("pid") for p in root.glob("*.json*")
                        if p.name in ("tool.json", "mcp.json.env")]:
                try:
                    os.kill(pid, 9)
                except (ProcessLookupError, TypeError):
                    pass
            if (root / "tool.json").exists():
                try:
                    os.kill(json.loads((root / "tool.json").read_text())["grandchild"]["pid"], 9)
                except ProcessLookupError:
                    pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    root = args.output.resolve()
    started = time.time()
    try:
        report = asyncio.run(probe(args.claude, root))
    except Exception as error:  # Record the failure as evidence, never as a pass.
        report = {"status": "failed", "error": f"{type(error).__name__}: {error}"}
    report["seconds"] = round(time.time() - started, 1)
    (root / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))
    sys.exit(0 if report["status"] == "passed" else 1)


if __name__ == "__main__":
    main()
