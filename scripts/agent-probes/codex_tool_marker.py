#!/usr/bin/env python3
"""T2: can XaYa tell Codex's exec tool processes from Codex itself and its MCP servers?

`shell_environment_policy.set.XAYA_TOOL=<marker>` is passed as a config override. The probe
records which processes carry it: the exec command, its setsid() grandchild, Codex itself and a
stdio MCP server. Real installed Codex, loopback Responses model, no account.
"""

import argparse
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import subprocess
import sys
import time

from claude_tool_marker import MCP, TOOL, environment
from model_fixture import ModelFixture
from run import isolated_env


def wait(check, label, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if (value := check()):
            return value
        time.sleep(0.1)
    raise TimeoutError(label)


def probe(binary, root):
    env = isolated_env(root)
    workspace = root / "workspace"
    workspace.mkdir()
    (root / "tool.py").write_text(TOOL)
    (root / "mcp.py").write_text(MCP)
    marker, owner = secrets.token_hex(16), secrets.token_hex(16)
    tool_command = shlex.join([sys.executable, str(root / "tool.py"), str(root)])
    with ModelFixture(commands={"TOOL_MARKER": tool_command}) as fixture:
        (Path(env["CODEX_HOME"]) / "config.toml").write_text(
            'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
            '[model_providers.probe]\nname="Local fixture"\n'
            f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
            '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
        overrides = [
            f'shell_environment_policy.set.XAYA_TOOL="{marker}"',
            f'mcp_servers.probe.command={json.dumps(sys.executable)}',
            f'mcp_servers.probe.args={json.dumps([str(root / "mcp.py"), str(root / "mcp.env")])}',
        ]
        argv = [binary, "exec", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox"]
        for override in overrides:
            argv += ["-c", override]
        process = subprocess.Popen([*argv, "TOOL_MARKER"], env=dict(env, XAYA_OWNER=owner), cwd=workspace,
                                   stdin=subprocess.DEVNULL, stdout=open(root / "exec.log", "w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            wait(lambda: (root / "tool.json").exists(), "exec tool started", 60)
            tool = json.loads((root / "tool.json").read_text())
            server = json.loads(wait(lambda: (root / "mcp.env").exists() and (root / "mcp.env").read_text(),
                                     "MCP server started", 20))
            codex_env = environment(process.pid) or ""
            report = {
                "tool_marked": tool["tool"] == marker,
                "setsid_grandchild_marked": tool["grandchild"]["tool"] == marker,
                "tool_owned": tool["owner"] == owner,
                "mcp_server_unmarked": server["tool"] is None,
                # Evidence only: Codex starts MCP servers with a filtered environment, so they do
                # not inherit XAYA_OWNER either and are outside marker-based ownership.
                "evidence_mcp_inherits_owner": server["owner"] == owner,
                "codex_unmarked": f"XAYA_OWNER={owner}" in codex_env and f"XAYA_TOOL={marker}" not in codex_env,
                "evidence": {"tool": tool, "mcp": server, "codex_pid": process.pid},
            }
            report["status"] = "passed" if all(v for k, v in report.items()
                                               if not k.startswith("evidence")) else "failed"
            return report
        finally:
            try:
                os.killpg(process.pid, 9)
            except ProcessLookupError:
                pass
            for path, key in (("tool.json", None), ("tool.json", "grandchild"), ("mcp.env", None)):
                try:
                    value = json.loads((root / path).read_text())
                    os.kill((value[key] if key else value)["pid"], 9)
                except (OSError, ValueError, KeyError):
                    pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    root = args.output.resolve()
    started = time.time()
    try:
        report = probe(args.codex, root)
    except Exception as error:  # Record the failure as evidence, never as a pass.
        report = {"status": "failed", "error": f"{type(error).__name__}: {error}"}
    report["seconds"] = round(time.time() - started, 1)
    (root / "report.json").write_text(json.dumps(report, indent=2))
    print(json.dumps(report, indent=2))
    sys.exit(0 if report["status"] == "passed" else 1)


if __name__ == "__main__":
    main()
