"""H1: real headless engines, denied write attempts, and checkpoint-bound JSON."""

import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time

from acceptance import REVIEW_SCHEMA, validate_review, verify_claude_denial
from claude_fixture import ClaudeFixture
from model_fixture import ModelFixture


def claude_readonly_stream(argv, env, workspace, root):
    """SDK stdio permissions: automatically deny every mutating challenge tool."""
    stderr = open(root / "claude-review-stderr.log", "w")
    try:
        process = subprocess.Popen(argv, env=env, cwd=workspace, text=True, start_new_session=True,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr)
    except BaseException:
        stderr.close()
        raise
    messages = queue.Queue()
    frames = []
    denials = []

    def read():
        try:
            for line in process.stdout:
                messages.put(json.loads(line))
        except Exception as error:
            messages.put(error)
        finally:
            messages.put(EOFError("Claude stream closed"))

    def send(value):
        process.stdin.write(json.dumps(value) + "\n")
        process.stdin.flush()

    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    try:
        send({"type": "control_request", "request_id": "xaya-review-init", "request": {"subtype": "initialize"}})
        deadline = time.monotonic() + 45
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Claude read-only review timed out")
            try:
                frame = messages.get(timeout=remaining)
            except queue.Empty:
                raise TimeoutError("Claude read-only review timed out") from None
            if isinstance(frame, Exception):
                raise frame
            frames.append(frame)
            if frame.get("type") == "control_response" and frame["response"].get("request_id") == "xaya-review-init":
                if frame["response"].get("subtype") != "success":
                    raise RuntimeError("Claude SDK initialization failed")
                send({"type": "user", "message": {"role": "user", "content": "XAYA_CLAUDE_REVIEW"}})
            elif frame.get("type") == "control_request":
                request = frame["request"]
                if request.get("subtype") != "can_use_tool":
                    raise RuntimeError("Unexpected Claude control request")
                allow = request.get("tool_name") in ("Read", "StructuredOutput")
                response = {"behavior": "allow", "updatedInput": request.get("input", {})} if allow else {
                    "behavior": "deny", "message": "Denied by XaYa read-only policy"}
                if not allow:
                    denials.append(request)
                send({"type": "control_response", "response": {"subtype": "success",
                    "request_id": frame["request_id"], "response": response}})
            elif frame.get("type") == "result":
                process.stdin.close()
                process.wait(timeout=5)
                if process.returncode:
                    raise RuntimeError("Claude review exited unsuccessfully")
                return frame, denials
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        reader.join(timeout=2)
        if not process.stdin.closed:
            process.stdin.close()
        process.stdout.close()
        stderr.close()
        (root / "claude-review-events.jsonl").write_text("\n".join(json.dumps(f) for f in frames) + "\n")
        (root / "claude-review-control-denials.json").write_text(json.dumps(denials, indent=2))


def run(argv, env, cwd, timeout=45):
    result = subprocess.run(argv, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{Path(argv[0]).name} failed: {result.stderr[-2000:]} {result.stdout[-2000:]}")
    return result


def review_probe(codex, claude, env, root, checks):
    evidence = {}
    checks["H1"] = {"status": "partial", "evidence": evidence}
    schema = root / "review-schema.json"
    schema.write_text(json.dumps(REVIEW_SCHEMA))
    for engine in ("codex", "claude"):
        workspace = root / ("review-" + engine)
        workspace.mkdir()

        def git(*args):
            return run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost", *args], env, workspace).stdout.strip()

        git("init", "-q")
        (workspace / "README").write_text("Read-only review fixture\n")
        git("add", "--all")
        git("commit", "-qm", "review checkpoint")
        checkpoint = git("rev-parse", "HEAD")
        review = {"checkpoint": checkpoint, "forbidden": workspace / "forbidden.txt"}
        before = git("status", "--porcelain")
        if engine == "codex":
            with ModelFixture(review=review) as fixture:
                config = Path(env["CODEX_HOME"]) / "config.toml"
                config.write_text('model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
                    '[model_providers.probe]\nname="Local fixture"\n'
                    f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
                    '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
                result = run([codex, "--no-daemon", "-a", "never", "exec", "--sandbox", "read-only", "--json",
                    "--output-schema", str(schema), "-o", str(root / "codex-review.json"), "REVIEW_READONLY"], env, workspace)
                (root / "codex-review-events.jsonl").write_text(result.stdout)
                (root / "codex-review-stderr.log").write_text(result.stderr)
                value = json.loads((root / "codex-review.json").read_text())
                events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
                if not any(e.get("type") == "turn.completed" for e in events) or any(e.get("type") == "turn.failed" for e in events):
                    raise RuntimeError("Codex review turn did not complete")
                denied = [part for request in fixture.requests for part in request.get("input", [])
                    if part.get("type") == "function_call_output" and any(word in str(part.get("output", "")).lower()
                        for word in ("reject", "denied", "never"))]
                if not denied:
                    raise RuntimeError("Codex provided no evidence that the write attempt was rejected")
                if not any(request.get("text", {}).get("format", {}).get("schema") for request in fixture.requests):
                    raise RuntimeError("Codex did not send the output schema")
                (root / "codex-review-requests.json").write_text(json.dumps(fixture.requests, indent=2))
                if fixture.errors:
                    raise RuntimeError(str(fixture.errors))
        else:
            with ClaudeFixture(review=review) as fixture:
                local_env = dict(env, ANTHROPIC_BASE_URL=fixture.base_url,
                    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
                try:
                    output, control_denials = claude_readonly_stream([claude, "-p", "--model", "claude-sonnet-4-6",
                        "--tools", "Read,Write", "--permission-mode", "manual", "--permission-prompt-tool", "stdio",
                        "--setting-sources", "", "--strict-mcp-config",
                        "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                        "--json-schema", json.dumps(REVIEW_SCHEMA)], local_env, workspace, root)
                finally:
                    (root / "claude-review-requests.json").write_text(json.dumps(fixture.requests, indent=2))
                (root / "claude-review-result.json").write_text(json.dumps(output, indent=2))
                if output.get("is_error") or "structured_output" not in output:
                    raise RuntimeError("Claude did not produce a successful structured_output")
                value = output["structured_output"]
                denied = [part for request in fixture.requests for m in request.get("messages", [])
                    for part in (m.get("content") if isinstance(m.get("content"), list) else [])
                    if part.get("type") == "tool_result" and part.get("tool_use_id") == "toolu_readonly"
                    and part.get("is_error")]
                if not denied:
                    raise RuntimeError("Claude provided no error tool result for the denied Write")
                verify_claude_denial(output, "toolu_readonly", str(review["forbidden"]), control_denials, denied)
                if not any(any(tool.get("name") == "Write" for tool in request.get("tools", []))
                           for request in fixture.requests):
                    raise RuntimeError("Write was not actually offered as a tool")
                (root / "claude-review-requests.json").write_text(json.dumps(fixture.requests, indent=2))
                if fixture.errors:
                    raise RuntimeError(str(fixture.errors))
        validate_review(value, checkpoint)
        if review["forbidden"].exists() or git("status", "--porcelain") != before or git("rev-parse", "HEAD") != checkpoint:
            raise RuntimeError(f"{engine} read-only checkout changed")
        evidence[engine] = {"checkpoint": checkpoint, "schema_output_valid": True,
            "write_automatically_denied": True, "checkout_unchanged": True, "receipt": value,
            "policy": "read-only sandbox + never" if engine == "codex" else "manual + stdio host policy (deny mutating tools)"}
    checks["H1"]["status"] = "passed"
