#!/usr/bin/env python3
"""Deterministic headless reviewer fixture. Never calls a model.

Plays `codex ... exec` (JSONL events, `-o` final message) or `claude -p` (stream-json with the
stdio permission channel), chosen by argv. What it answers comes from `behavior.json` in its
configuration directory (CODEX_HOME or CLAUDE_CONFIG_DIR):

  review         the structured review; relative `absolute_file_path`s are joined to the cwd
  criteria       per-criterion results added to the review (for task reviews)
  invalid        answer with text that is not the schema
  tamper         write a file into the checkout under review before answering
  write_attempt  (Claude) ask permission to Write first, and record the decision
  sleep          seconds to wait before answering
  usage          optional native token counters in the final event
  fail_times     exit with an error, without answering, on this many first calls
Every call is logged to `calls.jsonl`; the prompt to `prompt.txt`.
"""
import json
import os
from pathlib import Path
import sys
import time

args = sys.argv[1:]
codex = "exec" in args
# Without its configuration variable the engine uses its own default; the test names that.
config_env = os.environ.get("CODEX_HOME" if codex else "CLAUDE_CONFIG_DIR")
config = Path(config_env or os.environ["XAYA_REVIEWER_FIXTURE_HOME"])
behavior_file = config / "behavior.json"
behavior = json.loads(behavior_file.read_text()) if behavior_file.exists() else {}
with (config / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"engine": "codex" if codex else "claude", "argv": args,
                          "cwd": os.getcwd(), "pid": os.getpid(),
                          "owner": os.environ.get("XAYA_OWNER"),
                          "config_env": config_env}) + "\n")
if (config / "calls.jsonl").read_text().count("\n") <= behavior.get("fail_times", 0):
    print("fixture reviewer failure", file=sys.stderr)
    sys.exit(3)


def review_value():
    review = json.loads(json.dumps(behavior.get("review", {
        "findings": [], "overall_correctness": "patch is correct",
        "overall_explanation": "Fixture review.", "overall_confidence_score": 0.9})))
    if "criteria" in behavior:
        review["criteria"] = behavior["criteria"]
    for finding in review.get("findings", []):
        location = finding.get("code_location", {})
        path = location.get("absolute_file_path", "")
        if path and not path.startswith("/"):
            location["absolute_file_path"] = os.path.join(os.getcwd(), path)
    return review


def after_prompt():
    if behavior.get("tamper"):
        Path("tampered-by-reviewer.txt").write_text("changed\n")
    time.sleep(behavior.get("sleep", 0))


def pair(flag, value):
    return any(a == flag and b == value for a, b in zip(args, args[1:]))


if codex:
    if not (pair("--sandbox", "read-only") and pair("-a", "never") and "--output-schema" in args
            and "-o" in args):
        sys.exit("fixture codex: refusing to review without read-only flags and a schema")
    prompt = sys.stdin.read()
    (config / "prompt.txt").write_text(prompt)
    (config / "schema.json").write_text(Path(args[args.index("--output-schema") + 1]).read_text())
    print(json.dumps({"type": "thread.started", "thread_id": "fixture-codex-thread"}), flush=True)
    print(json.dumps({"type": "turn.started"}), flush=True)
    after_prompt()
    text = "not the schema" if behavior.get("invalid") else json.dumps(review_value())
    Path(args[args.index("-o") + 1]).write_text(text)
    print(json.dumps({"type": "item.completed",
                      "item": {"id": "item_1", "type": "agent_message", "text": text}}), flush=True)
    print(json.dumps({"type": "turn.completed", "usage": behavior.get("usage", {})}), flush=True)
    sys.exit(0)

# Claude: stream-json in and out, permissions answered over stdio by the caller.
tools = args[args.index("--tools") + 1].split(",") if "--tools" in args else []
if not (pair("--permission-prompt-tool", "stdio") and "--json-schema" in args
        and pair("--input-format", "stream-json") and pair("--output-format", "stream-json")
        and tools and not {"Write", "Edit", "Bash", "NotebookEdit"} & set(tools)):
    sys.exit("fixture claude: refusing to review without a read-only tool set and a schema")
(config / "schema.json").write_text(args[args.index("--json-schema") + 1])


def send(value):
    print(json.dumps(value), flush=True)


def receive():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line)


while True:
    message = receive()
    if message.get("type") == "control_request" and \
            message["request"].get("subtype") == "initialize":
        send({"type": "control_response", "response": {
            "subtype": "success", "request_id": message["request_id"], "response": {}}})
    elif message.get("type") == "user":
        content = message["message"]["content"]
        (config / "prompt.txt").write_text(content if isinstance(content, str) else json.dumps(content))
        break

if behavior.get("write_attempt"):
    send({"type": "control_request", "request_id": "fixture-write", "request": {
        "subtype": "can_use_tool", "tool_name": "Write",
        "input": {"file_path": os.path.join(os.getcwd(), "written.txt"), "content": "x"}}})
    answer = receive()
    with (config / "permissions.jsonl").open("a") as log:
        log.write(json.dumps(answer) + "\n")
    send({"type": "control_request", "request_id": "fixture-read", "request": {
        "subtype": "can_use_tool", "tool_name": "Read",
        "input": {"file_path": os.path.join(os.getcwd(), "src/stats.py")}}})
    answer = receive()
    with (config / "permissions.jsonl").open("a") as log:
        log.write(json.dumps(answer) + "\n")
after_prompt()
result = {"type": "result", "subtype": "success", "is_error": False,
          "session_id": "fixture-claude-session", "result": "done",
          "usage": behavior.get("usage")}
if not behavior.get("invalid"):
    result["structured_output"] = review_value()
send(result)
# Like Claude: stay until the caller closes stdin.
while sys.stdin.readline():
    pass
