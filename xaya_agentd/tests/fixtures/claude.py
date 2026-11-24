#!/usr/bin/env python3
"""Deterministic interactive Claude fixture for an XaYa-owned PTY. Never calls a model.

Behaves like a plain `claude --session-id <uuid> ... -- <prompt>` TUI: posts Mods events for the
chosen identity, accepts typed lines on its terminal, and can start a background child that
setsid()s away from every process tree (as a real backgrounded tool may).
"""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import threading
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

# Without CLAUDE_CONFIG_DIR, Claude uses its own default; the test names that directory.
config_env = os.environ.get("CLAUDE_CONFIG_DIR")
config = Path(config_env or os.environ["XAYA_CLAUDE_FIXTURE_HOME"])
(config / "config-env").write_text(config_env if config_env is not None else "<unset>")
args = sys.argv[1:]
with (config / "calls.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")
resumed = "--resume" in args
if resumed == ("--session-id" in args) or "--plugin-dir" not in args or "--bg" in args \
        or (resumed and "--" in args):
    sys.exit("Unexpected fixture CLI invocation")
session = args[args.index("--resume" if resumed else "--session-id") + 1]
if resumed and (config / "resume-wrong-id").exists():
    session = "00000000-0000-4000-8000-000000000000"
plugin = Path(args[args.index("--plugin-dir") + 1])
prompt = None if resumed else args[args.index("--") + 1]
(config / "owner").write_text(os.environ.get("XAYA_OWNER", ""))
# Like Claude: SessionStart command hooks may append to $CLAUDE_ENV_FILE, a script that the
# Bash tool's shell runs ahead of every command; Claude itself and its helpers never run it.
env_file = None
settings_file = Path(args[args.index("--settings") + 1])
for group in json.loads(settings_file.read_text()).get("hooks", {}).get("SessionStart", []):
    for hook in group.get("hooks", []):
        if hook.get("type") == "command":
            env_file = config / f"env-file-{os.getpid()}"
            env_file.write_text("")
            subprocess.run(["/bin/sh", "-c", hook["command"]], env={**os.environ,
                           "CLAUDE_ENV_FILE": str(env_file)}, check=True)


def bash_tool(command, *args):
    """Start `command` the way Claude's Bash tool does: a platform shell that first runs the
    session environment script. Further arguments are $1, $2, ... of the command."""
    script = (f'. "{env_file}" && ' if env_file else "") + command
    return subprocess.Popen(["/bin/sh", "-c", script, "sh", *args],
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
values = {}
for line in (plugin / "hooks/config.js").read_text().splitlines():
    name, raw = line.removeprefix("export const ").split(" = ", 1)
    values[name] = json.loads(raw.rstrip(";"))
sequence = 0
missed = 0
# Mirrors the Mods delivery queue: an undelivered event stays queued and is resent; admission
# is decided by the first delivery only. A restarted service answers 428 until it receives a
# snapshot of what this TUI knows (sequence, main turn, admitted tools, background tasks).
queue = []
active_turn = None
tools = {}
tasks = set()
posting = threading.RLock()


def send(path, value):
    request = Request(values["endpoint"] + path, json.dumps(value).encode(),
                      {"Content-Type": "application/json", "X-Xaya-Token": values["token"]})
    try:
        with urlopen(request, timeout=3) as response:
            return response.status, json.loads(response.read() or b"{}")
    except HTTPError as error:
        return error.code, {}


def sync():
    global missed, queue
    snapshot = {"sessionId": session, "sequence": sequence, "activeTurn": active_turn,
                "tools": [tool["identity"] for tool in tools.values() if tool["admitted"]],
                "tasks": sorted(tasks), "interrupted": False, "xayaGateVersion": 1,
                "xayaControlVersion": 1, "xayaSyncVersion": 1}
    status, _ = send("/sync", snapshot)
    if status == 200:
        # The snapshot covers every earlier event; their admissions already failed closed.
        queue = [item for item in queue if item["event"]["sequence"] > snapshot["sequence"]]
        missed = 0
        with (config / "syncs.jsonl").open("a") as log:
            log.write(json.dumps(snapshot) + "\n")


def flush():
    global missed, queue
    while queue:
        item = queue[0]
        try:
            status, _ = send("/events", {**item["event"], "missed": missed,
                                         **({"late": True} if item["late"] else {})})
        except OSError:
            status = None
        if status == 428:
            sync()
            return
        if status is None or (status != 409 and status >= 300):
            if time.monotonic() - item["since"] >= 5 or len(queue) > 256:
                missed += len(queue)
                queue = []
            return
        queue.pop(0)
        item["delivered"] = True
        item["allowed"] = status != 409
        if item["event"]["type"] == "tool.start" and item["allowed"]:
            tools[item["event"]["data"]["tool_use_id"]]["admitted"] = True


def post(kind, data):
    """Returns whether the service admitted it (meaningful for input/tool admission only)."""
    global sequence, active_turn
    with posting:
        sequence += 1
        if kind == "turn.start" and data.get("agentId") is None:
            active_turn = data["turnId"]
        elif kind == "turn.complete" and active_turn == data.get("turnId"):
            active_turn = None
        elif kind == "tool.start":
            tools[data["tool_use_id"]] = {"identity": data, "admitted": False}
        elif kind == "tool.complete":
            tools.pop(data["toolUseId"], None)
            task = data.get("result", {}).get("result", {}).get("backgroundTaskId")
            if task:
                tasks.add(task)
        item = {"event": {"sessionId": session, "sequence": sequence, "type": kind, "data": data},
                "since": time.monotonic(), "late": False, "delivered": False, "allowed": False}
        queue.append(item)
        flush()
        if not item["delivered"]:
            item["late"] = True  # Its hook decided without it; a resend is evidence only.
        if kind == "tool.start" and not item["allowed"]:
            tools.pop(data["tool_use_id"], None)
        return item["allowed"]


# Like a real TUI, a rejected Mods event does not end the program.
post("session.start", {"xayaGateVersion": 1, "xayaControlVersion": 1, "xayaSyncVersion": 1})
time.sleep(0.3)
if resumed:
    # A resumed TUI restores history and waits for native input; it starts no turn by itself.
    print("CLAUDE_RESUMED", session, flush=True)
else:
    post("turn.start", {"turnId": "turn-one", "text": prompt})
    # Opt-in: the first turn changes the checkout, as an implementer would.
    write = config / "first-turn-write.json"
    if write.exists():
        change = json.loads(write.read_text())
        target = Path(os.getcwd()) / change["path"]
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(change["content"])
    post("step.chunk", {"turnId": "turn-one", "chunk": {"kind": "text", "text": "CLAUDE_NATIVE_TEXT"}})
    post("turn.complete", {"turnId": "turn-one", "isAborted": False})
    print("CLAUDE_READY", session, flush=True)
active = False
next_poll = 0.0
permission = {"native": False}


def ask_permission(tool, tool_input):
    """A real PermissionRequest hook: blocks until XaYa answers or expires the request."""
    body = json.dumps({"hook_event_name": "PermissionRequest", "session_id": session,
                       "tool_name": tool, "tool_input": tool_input,
                       "permission_mode": "default"}).encode()  # Real payloads have no tool_use_id.
    request = Request(values["endpoint"] + "/hooks", body,
                      {"Content-Type": "application/json", "X-Xaya-Token": values["token"]})
    try:
        with urlopen(request, timeout=30) as response:
            answer = json.loads(response.read() or b"{}")
    except Exception as error:
        answer = {"transport_error": str(error)}
    with (config / "hook-responses.jsonl").open("a") as log:
        log.write(json.dumps(answer) + "\n")
    if permission["native"]:
        return  # The native TUI already answered; a later hook decision is not applied.
    decision = answer.get("hookSpecificOutput", {}).get("decision", {}).get("behavior")
    if decision is None:
        return  # No hook decision: the native prompt stays open for the user.
    result = {"result": {}} if decision == "allow" else {"deny": "native denial"}
    post("tool.complete", {"toolUseId": "perm-tool", "result": result})
    post("turn.complete", {"turnId": "perm-turn", "isAborted": False})

sends = 0


def control(route, value):
    status, body = send("/control/" + route, {"sessionId": session, **value})
    if status == 428:
        with posting:
            sync()
        return {}
    if status != 200:
        raise OSError(f"control {route} failed: {status}")
    return body


while config.exists():
    if select.select([sys.stdin], [], [], 0.03)[0]:
        line = sys.stdin.readline()
        if not line:
            break
        line = line.strip()
        if line == "background":
            # The grandchild leaves this process tree but inherits XAYA_OWNER.
            child = bash_tool('exec "$@"', sys.executable, "-c",
                "import os, time, sys\n"
                "if os.fork() == 0:\n"
                "    os.setsid()\n"
                "    open(sys.argv[1], 'w').write(str(os.getpid()))\n"
                "    time.sleep(60)\n"
                "    open(sys.argv[2], 'w').write('late')\n",
                str(config / "background.pid"), str(config / "background-late"))
            child.wait()
            print("BACKGROUND_STARTED", flush=True)
        elif line == "shell-tool":
            # A command of platform programs only: on macOS 27 neither the shell nor its
            # /bin/sleep child shows its environment.
            shell = bash_tool('/bin/sleep 60 & echo "$$ $!" > "$1"; wait',
                              str(config / "shell-tool.pids"))
            # Like Claude, reap the tool's shell once it ends.
            threading.Thread(target=shell.wait, daemon=True).start()
            print("SHELL_TOOL_STARTED", flush=True)
        elif line in ("permission", "large-permission"):
            permission["native"] = False
            post("turn.start", {"turnId": "perm-turn", "text": "needs permission"})
            post("tool.start", {"tool": "Write", "tool_use_id": "perm-tool"})
            tool_input = {"file_path": "allowed.txt", "content": "x" * (5000 if line == "large-permission" else 10)}
            threading.Thread(target=ask_permission, args=("Write", tool_input), daemon=True).start()
            print("PERMISSION_ASKED", flush=True)
        elif line == "native-answer":
            # The user answered in the native TUI: the tool runs and the turn ends.
            permission["native"] = True
            post("tool.complete", {"toolUseId": "perm-tool", "result": {"result": {}}})
            post("turn.complete", {"turnId": "perm-turn", "isAborted": False})
            print("NATIVE_ANSWERED", flush=True)
        elif line.startswith("tool "):
            tool = line.split()[1]
            admitted = post("tool.start", {"tool": "Bash", "tool_use_id": tool})
            print(("TOOL_ADMITTED " if admitted else "TOOL_DENIED ") + tool, flush=True)
        elif line.startswith("finish "):
            tool = line.split()[1]
            post("tool.complete", {"toolUseId": tool, "result": {"result": {}}})
            print("TOOL_FINISHED " + tool, flush=True)
        elif line.startswith("turn "):
            post("turn.start", {"turnId": line.split()[1], "text": "typed natively"})
            print("TURN_STARTED " + line.split()[1], flush=True)
        elif line.startswith("end "):
            post("turn.complete", {"turnId": line.split()[1], "isAborted": False})
            print("TURN_ENDED " + line.split()[1], flush=True)
        elif line == "helper":
            # Like an MCP server: started by Claude itself, so it carries no tool environment.
            helper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"],
                                      stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL)
            (config / "helper.pid").write_text(str(helper.pid))
            print("HELPER_STARTED", flush=True)
        elif line == "platform-helper":
            # A helper that is itself a platform program: hidden, and never a tool.
            helper = subprocess.Popen(["/bin/sleep", "60"], stdin=subprocess.DEVNULL,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            (config / "platform-helper.pid").write_text(str(helper.pid))
            print("PLATFORM_HELPER_STARTED", flush=True)
        elif line == "exit":
            sys.exit(0)
        elif line:
            (config / "typed.txt").write_text(line)
            print("TYPED " + line, flush=True)
    if (config / "activate-turn").exists() and not active:
        active = True
        post("turn.start", {"turnId": "cancel-turn", "text": "long native turn"})
    if time.monotonic() < next_poll:
        continue
    next_poll = time.monotonic() + 0.1
    try:
        command = control("poll", {}).get("command")
    except Exception:
        command = None
    if not command:
        continue
    with (config / "controls.jsonl").open("a") as log:
        log.write(json.dumps(command) + "\n")
    if command.get("operation") == "send":
        # Mirrors the Mods prompt.submit path: the turn starts before submit() resolves.
        sends += 1
        ack = {"command_id": command["command_id"], "status": "submitted"}
        if (config / "drop-send").exists():
            ack = {"command_id": command["command_id"], "status": "dropped",
                   "reason": "Claude stopped; the prompt was not submitted"}
        elif (config / "send-other-turn").exists():
            post("turn.start", {"turnId": f"other-{sends}", "text": "typed natively first"})
            post("turn.complete", {"turnId": f"other-{sends}", "isAborted": False})
        elif not (config / "silent-send").exists():
            post("turn.start", {"turnId": f"send-{sends}", "text": command["text"]})
            # Opt-in: a sent turn takes a while and changes the checkout.
            delay = config / "send-delay"
            if delay.exists():
                time.sleep(float(delay.read_text()))
            write = config / "send-write.json"
            if write.exists():
                change = json.loads(write.read_text())
                target = Path(os.getcwd()) / change["path"]
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(change["content"])
            post("turn.complete", {"turnId": f"send-{sends}", "isAborted": False})
        try:
            control("ack", ack)
        except Exception:
            pass  # A late acknowledgement is refused; the receipt was settled by the turn.
        continue
    if not active:
        continue
    post("turn.complete", {"turnId": "cancel-turn", "isAborted": True})
    if (config / "drop-control-ack").exists():
        continue
    control("ack", {"command_id": command["command_id"], "turn_id": command["turn_id"],
                    "status": "succeeded"})
