"""E1 normal Claude TUI; background workers intentionally disable OSC 7501."""

import asyncio
import json
from pathlib import Path
import shutil

from acceptance import match_boundaries
from claude_bridge import ClaudeBridge
from claude_fixture import ClaudeFixture
from codex_dual import Terminal, until


async def claude_boundaries(binary, env, root, checks):
    base = root / "osc-native-claude"
    base.mkdir()
    workspace = base / "workspace"
    workspace.mkdir()
    config = base / "config"
    config.mkdir()
    home = base / "home"
    home.mkdir()
    terminal = None
    with ClaudeBridge() as bridge, ClaudeFixture() as fixture:
        local_env = dict(env, HOME=str(home), CLAUDE_CONFIG_DIR=str(config),
            ANTHROPIC_BASE_URL=fixture.base_url, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
        (config / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True, "theme": "dark",
            "autoUpdates": False, "customApiKeyResponses": {"approved": [env["ANTHROPIC_API_KEY"]], "rejected": []},
            "projects": {str(workspace): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}}}))
        plugin = base / "plugin"
        (plugin / ".claude-plugin").mkdir(parents=True)
        (plugin / "hooks").mkdir()
        (plugin / ".claude-plugin/plugin.json").write_text('{"name":"xaya-osc-probe","version":"0.1.0"}')
        (plugin / "hooks/hooks.json").write_text('{"modules":["./register.js"]}')
        shutil.copyfile(Path(__file__).with_name("claude_mod.js"), plugin / "hooks/register.js")
        (plugin / "hooks/config.js").write_text(f"export const endpoint={json.dumps(bridge.endpoint)};\n"
            f"export const token={json.dumps(bridge.token)};\n")
        try:
            terminal = Terminal([binary, "--model", "claude-sonnet-4-6", "--tools", "", "--permission-mode", "manual",
                "--setting-sources", "user", "--strict-mcp-config", "--plugin-dir", str(plugin)],
                local_env, workspace, root / "claude-native-osc-tui.bin")
            identity = await until(lambda: next((e for e in bridge.events if e.get("type") == "session.start"), None),
                                   "normal Claude TUI Mods identity", timeout=30)
            native_id = identity["sessionId"]
            native_turns = []
            # Let startup terminal capability negotiation finish before the first prompt.
            await asyncio.sleep(0.5)
            for marker in ("XAYA_CLAUDE_OSC_FIRST", "XAYA_CLAUDE_OSC_SECOND"):
                terminal.write(marker)
                await asyncio.sleep(0.25)
                terminal.write("\r")
                start = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.start"
                    and e["data"].get("text") == marker), None), marker + " start")
                end = await until(lambda: next((e for e in bridge.events if e.get("type") == "turn.complete"
                    and e["data"].get("turnId") == start["data"]["turnId"]), None), marker + " end")
                await until(lambda: terminal.contains("ACK_" + marker + "_COMPLETE"), marker + " visible")
                native_turns.append({"id": start["data"]["turnId"], "start": start["received_at"], "end": end["received_at"]})
                await asyncio.sleep(0.3)
            e1 = checks["E1"]
            if e1["status"] == "not_run":
                e1.update(status="partial", evidence={})
            try:
                e1["evidence"]["claude_native"] = {"status": "passed", "native_session_id": native_id,
                    "matches": match_boundaries(terminal.osc, native_turns)}
            except ValueError as error:
                e1["evidence"]["claude_native"] = {"status": "failed", "error": str(error)}
            (root / "claude-native-osc.json").write_text(json.dumps({"reports": terminal.osc.reports,
                "cycles": terminal.osc.cycles, "native_turns": native_turns}, indent=2))
            if fixture.errors:
                raise RuntimeError(str(fixture.errors))
        finally:
            if terminal:
                await terminal.close()
            (root / "claude-native-osc-events.json").write_text(json.dumps(bridge.events, indent=2))
