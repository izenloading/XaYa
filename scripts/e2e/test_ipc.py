"""`xaya msg`: status --watch, focus, get-text and send-text."""

import json
import subprocess
import time

from harness import Xaya, wait_until


def run(binary, work, checks):
    # send-text stays off unless the config file turns it on.
    with Xaya(binary, work / "off") as xaya:
        shell = xaya.window_ids()[0]
        result = xaya.msg("send-text", "-w", shell, "echo nope", "--enter")
        checks.check("send-text is refused while off",
                     result.returncode != 0 and "ipc_send_text" in result.stderr, result)
        result = xaya.msg("get-text", "-w", 12345)
        checks.check("an unknown window is an error",
                     result.returncode != 0 and "no window 12345" in result.stderr, result)

    with Xaya(binary, work / "on", send_text=True) as xaya:
        shell = xaya.window_ids()[0]
        watch = subprocess.Popen(
            [xaya.binary, "msg", "--socket", xaya.socket, "status", "--watch", "--json"],
            stdout=subprocess.PIPE, text=True, env=xaya.env,
        )
        try:
            # An agent that works, then finishes while another window has the focus: results in
            # the focused window count as seen at once and would clear instead.
            agent = work / "agent.sh"
            agent.write_text(
                "printf '\\033]7501;state=working:app=codex\\007'\nsleep 3\n"
                "printf '\\033]7501;state=done:app=codex\\007'\n"
                "printf 'agent screen text\\n'\nsleep 600\n"
            )
            xaya.msg("create-window", "-e", "sh", agent)
            wait_until(lambda: len(xaya.window_ids()) == 2)
            other = next(id for id in xaya.window_ids() if id != shell)
            xaya.msg("focus", "-w", shell)

            result = xaya.msg("send-text", "-w", shell, "echo from-ipc", "--enter")
            checks.check("send-text is accepted when on", result.returncode == 0, result)
            last = wait_until(lambda: xaya.text(shell, "--last-command").strip() ==
                              "$ echo from-ipc\nfrom-ipc")
            checks.check("get-text --last-command reads the command and its output", last,
                         xaya.text(shell, "--last-command"))
            lines = xaya.text(shell, "--lines", 2).strip().splitlines()
            checks.check("get-text --lines reads the last lines", len(lines) == 2, lines)

            screen = wait_until(lambda: "agent screen text" in xaya.text(other), timeout=8)
            checks.check("get-text reads another window's screen", screen)
            result = xaya.msg("get-text", "-w", other, "--last-command")
            checks.check("--last-command needs shell integration",
                         result.returncode != 0 and "shell integration" in result.stderr, result)

            focused = wait_until(lambda: [w["window_id"] for w in xaya.windows()
                                          if w["focused"]] == [shell])
            checks.check("focus moves the focus", focused, xaya.windows())

            # Let the agent finish, and --watch poll (twice a second) once more.
            wait_until(lambda: any(w["window_id"] == other and w["state"] == "done"
                                   for w in xaya.windows()))
            time.sleep(1)
        finally:
            watch.terminate()
            output = watch.communicate(timeout=10)[0]

        events = [json.loads(line) for line in output.splitlines()]
        checks.check("--watch starts with every window",
                     {e.get("window_id") for e in events[:1]} == {shell}, events[:1])
        states = [e.get("state") for e in events if e.get("window_id") == other]
        checks.check("--watch streams an agent's changes", states == ["working", "done"], events)
        checks.check("no panics or errors logged", not xaya.problems(), xaya.problems()[:5])
