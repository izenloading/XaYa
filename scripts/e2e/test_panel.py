"""The Agent panel's managed view in a real window, driven by keystrokes; macOS only.

A deterministic Claude fixture (the agentd tests' own, never a model) runs under an isolated
service. The test types into xaya through System Events, so the app running it needs
Accessibility; keys are only sent while the test window is frontmost. Checks the board push
(`board --watch` child), its end after the panel closes, falling back and rebuilding the push
across a service crash, `Enter` attaching in a pane below, and `t` ending only tool processes.
"""

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time

from harness import MACOS, ROOT, Xaya, fresh_dir, keyable_copy, wait_until

FIXTURE = ROOT / "xaya_agentd/tests/fixtures/claude.py"
REVIEWER = ROOT / "xaya_agentd/tests/fixtures/reviewer.py"


def processes():
    """{pid: (ppid, command)} of every process."""
    out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,command="], capture_output=True,
                         text=True).stdout
    table = {}
    for line in out.splitlines():
        pid, ppid, command = line.split(None, 2)
        table[int(pid)] = (int(ppid), command)
    return table


def watches(xaya):
    """Pids of the board watches the xaya under test keeps."""
    return [pid for pid, (ppid, command) in processes().items()
            if ppid == xaya.process.pid and "board --watch" in command]


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A zombie still answers kill(0); it's gone for this test's purposes.
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return state.stdout.strip()[:1] not in ("", "Z")


def run(binary, work, checks):
    if not MACOS:
        print("  skipped: needs macOS System Events")
        return

    bin_dir = work / "bin"
    xaya_bin = keyable_copy(binary, bin_dir)
    config = fresh_dir(work / "claude-config")
    project = fresh_dir(work / "project")
    # A repository, so the session's work can be checkpointed and reviewed.
    for args in (["init", "-q"], ["add", "."], ["commit", "-qm", "base"]):
        if args[0] == "add":
            (project / "notes.txt").write_text("one\n")
        subprocess.run(["git", "-c", "user.name=e2e", "-c", "user.email=e2e@e2e", *args],
                       cwd=project, check=True, capture_output=True)
    claude = work / "claude"
    shutil.copy(FIXTURE, claude)
    claude.chmod(0o700)
    # Unix socket paths are short; keep the service directory under /tmp.
    runtime_parent = tempfile.mkdtemp(prefix="xaya-e2e-agent-", dir="/tmp")
    env = {
        "XAYA_AGENT_RUNTIME_DIR": os.path.join(runtime_parent, "run"),
        "XAYA_AGENT_STATE_DIR": str(work / "agent-state"),
        "CLAUDE_CONFIG_DIR": str(config),
    }
    full_env = dict(os.environ, **env)

    def agent(*args, timeout=30, extra=None):
        return subprocess.run([str(xaya_bin), *map(str, args)], capture_output=True, text=True,
                              env={**full_env, **(extra or {})}, timeout=timeout)

    def session():
        listed = agent("agent", "session", "list", "--json")
        sessions = json.loads(listed.stdout) if listed.returncode == 0 else []
        return sessions[0] if sessions else None

    def service_pid():
        return next((pid for pid, (_, command) in processes().items()
                     if "xaya-agentd" in command and " serve" in command
                     and env["XAYA_AGENT_RUNTIME_DIR"] in command), None)

    try:
        started = agent("run", "--engine", "claude", "--claude", claude, "-C", project,
                        "--detach", "first turn")
        checks.check("a fixture Claude session starts", started.returncode == 0, started)
        ready = wait_until(lambda: (s := session()) and s["state"] == "ready" and s, timeout=20)
        checks.check("it becomes ready", bool(ready), session())
        if not ready:
            return

        with Xaya(xaya_bin, work, send_text=True, env=env,
                  config="[window]\ndimensions = { columns = 110, lines = 30 }\n") as xaya:
            window = xaya.window_ids()[0]
            run_panel(xaya, window, checks, agent, session, service_pid, config)
            review_stage(xaya, checks, agent, session, config, project, work)
            checks.check("no panics or errors logged", not xaya.problems(), xaya.problems()[:5])
    finally:
        agent("agent", "session", "stop", (session() or {}).get("id", "none"))
        agent("agent", "stop")
        shutil.rmtree(config, ignore_errors=True)  # Ends the fixture's loop if anything's left.
        shutil.rmtree(runtime_parent, ignore_errors=True)


def run_panel(xaya, window, checks, agent, session, service_pid, config):
    def open_managed():
        return xaya.press("a", "command", "shift") and time.sleep(0.5) is None \
            and xaya.press("m")

    # Opening the managed view starts a pushed board.
    checks.check("keys reach the test window", open_managed(), "not frontmost")
    first = wait_until(lambda: watches(xaya), timeout=10)
    checks.check("the managed view keeps a board watch", len(first or []) == 1, first)
    time.sleep(1)
    xaya.screenshot("1-managed")
    checks.check("panel keys don't reach the shell", "$ m" not in xaya.text(window),
                 xaya.text(window))

    # A service crash: the watch ends, the panel falls back, and the push comes back with the
    # service.
    old = service_pid()
    checks.check("the service is found", old is not None)
    if old:
        os.kill(old, signal.SIGKILL)
        gone = wait_until(lambda: not set(first or []) & set(watches(xaya)), timeout=10)
        checks.check("the watch ends with the service", gone, watches(xaya))
        restarted = agent("agent", "start")
        checks.check("the service restarts", restarted.returncode == 0, restarted)
        rejoined = wait_until(lambda: (s := session()) and s["state"] == "ready", timeout=30)
        checks.check("the session is rejoined", rejoined, session())
        rebuilt = wait_until(lambda: watches(xaya), timeout=15)
        checks.check("the panel rebuilds its board watch", len(rebuilt or []) == 1
                     and not set(rebuilt) & set(first or []), (first, rebuilt))
        time.sleep(1)
        xaya.screenshot("2-after-restart")

    # Closing the panel ends the watch within a few seconds.
    checks.check("Escape is sent", xaya.press("escape"))
    closed = wait_until(lambda: not watches(xaya), timeout=10)
    checks.check("closing the panel ends the watch", closed, watches(xaya))

    # Enter attaches the selected session in a pane below; start a background tool there.
    checks.check("the managed view opens again", open_managed())
    time.sleep(1.5)
    checks.check("Enter is sent", xaya.press("return"))
    attached = wait_until(lambda: (s := session()) and s.get("attached"), timeout=15)
    checks.check("Enter attaches the session", bool(attached), session())
    time.sleep(1)
    xaya.msg("send-text", "-w", window, "background", "--enter")
    started = wait_until(lambda: "BACKGROUND_STARTED" in xaya.text(window), timeout=15)
    checks.check("a background tool starts in the attached pane", started, xaya.text(window))
    pid_file = config / "background.pid"
    tool = wait_until(lambda: pid_file.exists() and pid_file.read_text().strip(), timeout=10)
    tool = int(tool) if tool else None
    claude = (session() or {}).get("backend_pid")
    xaya.screenshot("3-attached")

    # `t` ends the tool, not Claude, and the session stays ready.
    checks.check("the managed view opens over the pane", open_managed())
    time.sleep(1.5)
    checks.check("t is sent", xaya.press("t"))
    ended = tool is not None and wait_until(lambda: not alive(tool), timeout=20)
    checks.check("t ends the background tool", ended, tool)
    time.sleep(1)
    xaya.screenshot("4-end-tools")
    checks.check("Claude keeps running", claude is not None and alive(claude), claude)
    checks.check("the session stays ready", (session() or {}).get("state") == "ready", session())
    checks.check("the tool never wrote late", not (config / "background-late").exists())
    xaya.press("escape")


def review_stage(xaya, checks, agent, session, config, project, work):
    """`r` shows the session's latest review; `s` sends a needs-fix one back, resuming it."""
    (project / "notes.txt").write_text("one\ntwo\n")
    sid = session()["id"]
    stopped = agent("agent", "session", "stop", sid)
    checks.check("the session stops for its checkpoint", stopped.returncode == 0, stopped.stderr)
    made = agent("checkpoint", "create", sid, "--json")
    checkpoint = json.loads(made.stdout) if made.returncode == 0 else {}
    checks.check("a checkpoint is captured", checkpoint.get("status") == "ready", made.stderr)
    if checkpoint.get("status") != "ready":
        return
    promoted = agent("task", "promote", sid, "--title", "Fix notes", "--goal",
                     "Correct the second line.", "--criterion", "The second line says three.",
                     "--dual-review")
    task = json.loads(promoted.stdout) if promoted.returncode == 0 else {}
    checks.check("the session is promoted to a task", bool(task.get("id")), promoted.stderr)
    if not task.get("id"):
        return
    reviewer = work / "reviewer"
    shutil.copy(REVIEWER, reviewer)
    reviewer.chmod(0o700)
    home = fresh_dir(work / "reviewer-home")
    (home / "behavior.json").write_text(json.dumps({"review": {
        "findings": [{
            "title": "[P1] The second line is wrong", "body": "It should say three.",
            "confidence_score": 0.9, "priority": 1,
            "code_location": {"absolute_file_path": "notes.txt",
                              "line_range": {"start": 2, "end": 2}}}],
        "overall_correctness": "patch is incorrect", "overall_explanation": "Line 2.",
        "overall_confidence_score": 0.8},
        "criteria": [{"id": "AC1", "status": "unmet", "evidence": "The second line says two."}]}))
    review_env = {"CODEX_HOME": str(home), "CLAUDE_CONFIG_DIR": str(home)}
    reviewed = agent("review", "run", checkpoint["id"], "--engine", "codex", "--codex", reviewer,
                     "--claude", reviewer, timeout=60, extra=review_env)
    review = json.loads(reviewed.stdout) if reviewed.stdout.strip() else {}
    checks.check("the review combines both engines",
                 [r["engine"] for r in review.get("reviewers", [])] == ["codex", "claude"]
                 and len(review.get("findings", [])) == 1, reviewed.stderr)
    checks.check("the review needs a fix", review.get("verdict") == "needs-fix", reviewed.stderr)
    if review.get("verdict") != "needs-fix":
        return

    checks.check("the managed view opens", xaya.press("a", "command", "shift")
                 and time.sleep(0.5) is None and xaya.press("m"))
    time.sleep(1.0)
    checks.check("r is sent", xaya.press("r"))
    time.sleep(2.0)
    xaya.screenshot("5-review")
    checks.check("s is sent", xaya.press("s"))
    controls = config / "controls.jsonl"

    def sent_text():
        if not controls.exists():
            return None
        lines = [json.loads(line) for line in controls.read_text().splitlines()]
        return next((c["text"] for c in lines if c.get("operation") == "send"), None)

    text = wait_until(sent_text, timeout=60)
    checks.check("s resumes the session and sends the fixes back",
                 bool(text) and "The second line is wrong" in text, text)
    shown = json.loads(agent("review", "show", review["id"]).stdout)
    checks.check("the review records the send", bool(shown.get("sent")), shown.get("sent"))
    time.sleep(1.5)
    xaya.screenshot("6-sent")
    xaya.press("escape")
    checked = agent("review", "run", checkpoint["id"], "--engine", "codex", "--codex", reviewer,
                    "--claude", reviewer, timeout=60, extra=review_env)
    followup = json.loads(checked.stdout) if checked.stdout.strip() else {}
    task = json.loads(agent("task", "show", task["id"]).stdout)["task"]
    attention = task.get("attention") or {}
    checks.check("the repeat review triggers task attention",
                 attention.get("review_id") == followup.get("id")
                 and "same tree" in attention.get("reason", ""), attention)
    if not attention or not followup.get("id"):
        return
    time.sleep(1.0)
    checks.check("r opens the attention review", xaya.press("r"))
    time.sleep(2.0)
    checks.check("the attention screenshot is captured", bool(xaya.screenshot("7-task-attention")))
    before = controls.read_text()
    checks.check("s is pressed on the attention review", xaya.press("s"))
    time.sleep(1.5)
    shown = json.loads(agent("review", "show", followup["id"]).stdout)
    checks.check("the follow-up is not sent from the panel",
                 not shown.get("sent") and controls.read_text() == before, shown.get("sent"))
    xaya.press("escape")
