"""Session restore, with fake `claude` and `codex` that leave what the real ones do on disk.

The fakes write Claude Code's `~/.claude/sessions/<pid>.json` and transcript, and Codex's rollout
with its session_meta line, report their app through OSC 7501, and log how they were started.
`HOME`, `CODEX_HOME` and the state directory are temporary, so no real session is touched.
"""

import json
import os

from harness import Xaya, fresh_dir, wait_until


def fake_agents(work, log):
    bin_dir = fresh_dir(work / "bin")
    (bin_dir / "claude").write_text(f"""#!/bin/bash
echo "claude $* in $(pwd -P)" >> {log}
mkdir -p "$HOME/.claude/sessions" "$HOME/.claude/projects/anything"
echo '{{"pid":'$$',"sessionId":"sess-A","cwd":"'"$(pwd -P)"'"}}' > "$HOME/.claude/sessions/$$.json"
touch "$HOME/.claude/projects/anything/sess-A.jsonl"
printf '\\033]7501;state=idle:app=claude-code\\007'
sleep 600
""")
    (bin_dir / "codex").write_text(f"""#!/bin/bash
echo "codex $* in $(pwd -P)" >> {log}
day="$CODEX_HOME/sessions/2026/11/11"; mkdir -p "$day"
echo '{{"type":"session_meta","payload":{{"id":"codex-B","cwd":"'"$(pwd -P)"'"}}}}' > "$day/rollout-codex-B.jsonl"
printf '\\033]7501;state=idle:app=codex\\007'
sleep 600
""")
    for script in bin_dir.iterdir():
        script.chmod(0o755)
    return bin_dir


def run(binary, work, checks):
    log = work / "agents.log"
    log.touch()
    bin_dir = fake_agents(work, log)
    dirs = {name: fresh_dir(work / "work" / name) for name in "ABC"}
    env = {
        "HOME": str(fresh_dir(work / "home")),
        "CODEX_HOME": str(fresh_dir(work / "codex")),
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
    }
    # Both runs share one state directory, where the snapshot lives.
    shared = {"env": env, "send_text": True}
    snapshot_path = work / "state" / "xaya" / "session.json"

    # First run: Claude Code in A, Codex in B, a plain shell in C.
    xaya = Xaya(binary, work, **shared).start()
    try:
        first = xaya.window_ids()[0]
        for name in "BC":
            xaya.msg("create-window", "--working-directory", dirs[name])
        wait_until(lambda: len(xaya.window_ids()) == 3)
        ids = xaya.window_ids()
        checks.check("three windows open", len(ids) == 3, ids)
        codex_window = next(id for id in ids if id != first)
        xaya.msg("send-text", "-w", first, f"cd {dirs['A']} && claude", "--enter")
        xaya.msg("send-text", "-w", codex_window, f"cd {dirs['B']} && codex", "--enter")

        def saved_agents():
            if not snapshot_path.exists():
                return []
            windows = json.loads(snapshot_path.read_text())["windows"]
            return sorted(json.dumps(w["agent"], sort_keys=True) for w in windows if w["agent"])

        # Snapshots refresh every 15 seconds.
        wait_until(lambda: len(saved_agents()) == 2, timeout=25, interval=1)
    finally:
        xaya.stop()

    expected = sorted([
        json.dumps({"app": "claude-code", "session_id": "sess-A"}, sort_keys=True),
        json.dumps({"app": "codex", "session_id": "codex-B"}, sort_keys=True),
    ])
    snapshot = json.loads(snapshot_path.read_text())
    agents = sorted(json.dumps(w["agent"], sort_keys=True) for w in snapshot["windows"] if w["agent"])
    checks.check("the snapshot names both agents' sessions", agents == expected, agents)
    checks.check("the snapshot keeps the plain window", len(snapshot["windows"]) == 3, snapshot)
    started_before = len(log.read_text().splitlines())

    # Second run: only the agents' windows come back, each resuming its session.
    with Xaya(binary, work, **shared) as xaya:
        resumed = wait_until(lambda: len(log.read_text().splitlines()) - started_before >= 2,
                             timeout=10)
        started = log.read_text().splitlines()[started_before:]
        checks.check("only the agents' windows come back", len(xaya.window_ids()) == 2,
                     xaya.windows())
        checks.check("both agents resume", resumed, started)
        checks.check("Claude Code resumes its session in A",
                     f"claude --resume sess-A in {dirs['A']}" in started, started)
        checks.check("Codex resumes its session in B",
                     f"codex resume codex-B in {dirs['B']}" in started, started)
        checks.check("each resumes once", len(started) == 2, started)
        checks.check("no panics or errors logged", not xaya.problems(), xaya.problems()[:5])

    after = json.loads(snapshot_path.read_text())
    agents = sorted(json.dumps(w["agent"], sort_keys=True) for w in after["windows"] if w["agent"])
    checks.check("restored sessions are saved again", agents == expected, after)
