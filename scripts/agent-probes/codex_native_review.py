"""R1: what Codex's native review (`/review`) hands a program, and whether it stays read-only.

The local model answers the review request by first trying to write a file, then with a fixed
review in Codex's own schema. Three entry points are compared on the same commit:
  A. `codex review --commit SHA` (plain CLI output),
  B. `codex exec review --commit SHA --json` (JSONL events),
  C. app-server `review/start` with a commit target (protocol notifications).
For each: is the structured review (findings, priority, location, overall verdict) recoverable,
or only rendered text; and was the write refused. Real installed Codex, loopback model only.
"""

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile

from codex_dual import Rpc, connect, initialize, until
from model_fixture import ModelFixture

REVIEW_MARKER = "Review guidelines"


def review_json(file):
    return {
        "findings": [{
            "title": "[P1] Divide by zero when the list is empty",
            "body": "`mean([])` now divides by `len(values)`, which is 0.",
            "confidence_score": 0.9,
            "priority": 1,
            "code_location": {"absolute_file_path": str(file), "line_range": {"start": 2, "end": 2}},
        }],
        "overall_correctness": "patch is incorrect",
        "overall_explanation": "An empty input now raises ZeroDivisionError.",
        "overall_confidence_score": 0.85,
    }


def responder(file, forbidden):
    def respond(body, tool_done):
        if REVIEW_MARKER not in json.dumps(body):
            return None
        if not tool_done:
            return {"type": "function_call", "status": "completed", "name": "exec_command",
                    "arguments": json.dumps({"cmd": "printf forbidden > " + shlex.quote(str(forbidden)),
                                             "shell": "/bin/sh", "login": False})}
        return {"type": "message", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": json.dumps(review_json(file)),
                             "annotations": []}]}
    return respond


def structured_in(text):
    """Which parts of the fixed review survive in `text`."""
    return {"title": "Divide by zero when the list is empty" in text,
            "priority_tag": "[P1]" in text or '"priority": 1' in text or '"priority":1' in text,
            "confidence": "0.9" in text,
            "line_range": '"start": 2' in text or '"start":2' in text or ":2" in text,
            "overall_verdict": "patch is incorrect" in text or "incorrect" in text.lower(),
            "explanation": "ZeroDivisionError" in text}


def repository(root):
    repo = root / "repo"
    (repo / "src").mkdir(parents=True)
    git = lambda *a: subprocess.run(["git", "-c", "user.name=r", "-c", "user.email=r@r", *a],
                                    cwd=repo, check=True, capture_output=True, text=True).stdout.strip()
    git("init", "-q", "-b", "main")
    (repo / "src/stats.py").write_text("def mean(values):\n    return sum(values) / max(len(values), 1)\n")
    git("add", ".")
    git("commit", "-qm", "base")
    (repo / "src/stats.py").write_text("def mean(values):\n    return sum(values) / len(values)\n")
    git("commit", "-qam", "Simplify mean")
    return repo, git("rev-parse", "HEAD")


async def via_app_server(binary, env, repo, sha, root, sockets):
    endpoint = sockets / "s.sock"
    log = open(root / "c-backend.log", "w")
    backend = subprocess.Popen([binary, "app-server", "--listen", f"unix://{endpoint}"], env=env,
                               cwd=repo, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                               start_new_session=True)
    rpc = None
    try:
        await until(endpoint.exists, "socket")
        rpc = Rpc(await connect(endpoint))
        await initialize(rpc)
        thread = (await rpc.call("thread/start", {"cwd": str(repo), "approvalPolicy": "never",
                                                  "sandbox": "read-only"}))["thread"]["id"]
        started = await rpc.call("review/start", {"threadId": thread,
                                                  "target": {"type": "commit", "sha": sha}})
        await until(lambda: any(e.get("method") == "turn/completed" for e in rpc.events),
                    "review turn", timeout=90)
        items = [e["params"]["item"] for e in rpc.events if e.get("method") == "item/completed"]
        exited = [i for i in items if i.get("type") == "exitedReviewMode"]
        raw = json.dumps(rpc.events)
        (root / "c-events.json").write_text(json.dumps(rpc.events, indent=1))
        return {"review_start_result_keys": sorted(started.keys()),
                "item_types": sorted({i.get("type") for i in items}),
                "exited_review_text": exited[0]["review"] if exited else None,
                "findings_key_anywhere": '"findings"' in raw,
                "structured_in_text": structured_in(exited[0]["review"]) if exited else None}
    finally:
        with contextlib.suppress(Exception):
            if rpc:
                await rpc.close()
        with contextlib.suppress(ProcessLookupError):
            os.killpg(backend.pid, signal.SIGTERM)
        with contextlib.suppress(subprocess.TimeoutExpired):
            await asyncio.to_thread(backend.wait, 5)
        if backend.poll() is None:
            os.killpg(backend.pid, signal.SIGKILL)
        log.close()


async def probe(binary, output):
    evidence = {"codex": subprocess.run([binary, "--version"], capture_output=True,
                                        text=True).stdout.strip()}
    with tempfile.TemporaryDirectory(prefix="xr1-", dir="/tmp") as tmp:
        root = Path(tmp).resolve()
        home = root / "home"
        home.mkdir()
        repo, sha = repository(root)
        forbidden = root / "forbidden.txt"
        with ModelFixture(respond=responder(repo / "src/stats.py", forbidden)) as fixture:
            (home / "config.toml").write_text(
                'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
                'approval_policy="never"\nsandbox_mode="read-only"\n'
                '[model_providers.probe]\nname="Local fixture"\n'
                f'base_url="{fixture.base_url}"\nwire_api="responses"\nrequires_openai_auth=false\n'
                f'[projects.{json.dumps(str(repo))}]\ntrust_level="trusted"\n'
                '[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
            env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX_", "OPENAI_"))}
            env.update(HOME=str(root), CODEX_HOME=str(home))

            def run(name, args):
                start = len(fixture.requests)
                result = subprocess.run([binary, *args], cwd=repo, env=env, capture_output=True,
                                        text=True, timeout=120, stdin=subprocess.DEVNULL)
                (output / f"{name}-stdout.txt").write_text(result.stdout)
                (output / f"{name}-stderr.txt").write_text(result.stderr)
                both = result.stdout + result.stderr
                return {"exit_code": result.returncode,
                        "model_requests": len(fixture.requests) - start,
                        "review_requests": sum(REVIEW_MARKER in json.dumps(b)
                                               for b in fixture.requests[start:]),
                        "structured_in_output": structured_in(both),
                        "stdout_tail": result.stdout[-600:]}

            evidence["A_cli_review"] = run("a", ["review", "--commit", sha])
            evidence["A_cli_review"]["forbidden_written"] = forbidden.exists()
            forbidden.unlink(missing_ok=True)
            b = run("b", ["exec", "review", "--commit", sha, "--json"])
            events = [json.loads(line) for line in (output / "b-stdout.txt").read_text().splitlines()
                      if line.startswith("{")]
            b["event_types"] = sorted({e.get("type", "") for e in events} |
                                      {e.get("item", {}).get("type", "") for e in events})
            b["findings_key_in_events"] = any('"findings"' in json.dumps(e) for e in events)
            b["forbidden_written"] = forbidden.exists()
            evidence["B_exec_review_json"] = b
            forbidden.unlink(missing_ok=True)
            try:
                evidence["C_app_server_review_start"] = await via_app_server(binary, env, repo, sha,
                                                                            output, root)
            except Exception as error:
                evidence["C_app_server_review_start"] = {"error": f"{type(error).__name__}: {error}"}
            evidence["C_app_server_review_start"]["forbidden_written"] = forbidden.exists()
            # What the sandbox told the model about the write attempt.
            outputs = [item for body in fixture.requests for item in body.get("input", [])
                       if item.get("type") == "function_call_output"]
            evidence["write_attempt_results"] = [str(o.get("output"))[:200] for o in outputs]
            evidence["model_errors"] = fixture.errors
    evidence["status"] = "completed"
    return evidence


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex", default="codex")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    evidence = asyncio.run(probe(args.codex, args.output))
    (args.output / "r1.json").write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2, ensure_ascii=False))
    sys.exit(0 if evidence.get("status") == "completed" else 1)


if __name__ == "__main__":
    main()
