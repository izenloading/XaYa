"""Acceptance: `xaya review run` with the real Codex and Claude CLIs as reviewers.

A checkpoint is made from a stopped managed session (the service's own Claude test fixture
plays the implementer; it is not under test). Real Codex then reviews it through `exec` in the
read-only sandbox, and real Claude through `-p` with read-only tools, both against loopback
models that first try to write a file and then answer in the review schema. Checks: the
structured answers arrive, the verdicts follow the service's rules (a P1 finding needs a fix;
a clean review with a passing check is approved), the writes never happen, the user's checkout
is untouched, and the private review checkouts are removed. No real account is used.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

from claude_fixture import ClaudeFixture
from model_fixture import ModelFixture
from run import isolated_env

ROOT = Path(__file__).resolve().parents[2]
MARKER = "You are reviewing a proposed code change"
BASE = "def mean(values):\n    return sum(values) / max(len(values), 1)\n"
CHANGED = "def mean(values):\n    return sum(values) / len(values)\n"


def checkout_of(body):
    """The private checkout path the service told the reviewer about."""
    match = re.search(r"repository checkout at (/[^,\"\\]+), which", json.dumps(body))
    return match.group(1) if match else "/nonexistent"


def codex_answer(forbidden):
    def respond(body, tool_done):
        if MARKER not in json.dumps(body):
            return None
        if not tool_done:
            return {"type": "function_call", "status": "completed", "name": "exec_command",
                    "arguments": json.dumps({"cmd": f"printf x > {forbidden}", "shell": "/bin/sh",
                                             "login": False})}
        review = {
            "findings": [{
                "title": "[P1] Divide by zero when the list is empty",
                "body": "`mean([])` divides by `len(values)`, which is 0.",
                "confidence_score": 0.9, "priority": 1,
                "code_location": {"absolute_file_path": checkout_of(body) + "/src/stats.py",
                                  "line_range": {"start": 2, "end": 2}}}],
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "An empty list now raises ZeroDivisionError.",
            "overall_confidence_score": 0.8}
        if "Acceptance criteria:" in json.dumps(body):
            review["criteria"] = [
                {"id": "AC1", "status": "unmet", "evidence": "mean([]) divides by zero."},
                {"id": "AC2", "status": "met", "evidence": "mean([2, 4]) is 6 / 2."}]
        return {"type": "message", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": json.dumps(review),
                             "annotations": []}]}
    return respond


def claude_answer(body):
    answer = {"findings": [], "overall_correctness": "patch is correct",
              "overall_explanation": "The change is straightforward.",
              "overall_confidence_score": 0.7}
    if "Acceptance criteria:" in json.dumps(body):
        answer["criteria"] = [{"id": f"AC{n}", "status": "met", "evidence": "Read the code."}
                              for n in (1, 2)]
    return answer


def run(output, xaya, codex, claude):
    root = Path(tempfile.mkdtemp(prefix="xrv-", dir="/tmp")).resolve()
    evidence = {}
    try:
        env = isolated_env(root)
        repo = root / "repo"
        (repo / "src").mkdir(parents=True)
        implementer = root / "implementer"
        shutil.copy(ROOT / "xaya_agentd/tests/fixtures/claude.py", implementer)
        implementer.chmod(0o700)
        implementer_config = root / "implementer-config"
        implementer_config.mkdir()
        forbidden_codex, forbidden_claude = root / "codex-wrote.txt", root / "claude-wrote.txt"
        with ModelFixture(respond=codex_answer(forbidden_codex)) as codex_model, \
                ClaudeFixture(review={"forbidden": forbidden_claude, "structured": claude_answer},
                              review_marker=MARKER) as claude_model:
            (Path(env["CODEX_HOME"]) / "config.toml").write_text(
                'model="gpt-5.4"\nmodel_provider="probe"\ncheck_for_update_on_startup=false\n'
                '[model_providers.probe]\nname="Local fixture"\n'
                f'base_url="{codex_model.base_url}"\nwire_api="responses"\n'
                'requires_openai_auth=false\n[analytics]\nenabled=false\n[feedback]\nenabled=false\n')
            env.update(XAYA_AGENT_RUNTIME_DIR=str(root / "run"),
                       ANTHROPIC_BASE_URL=claude_model.base_url,
                       ANTHROPIC_MODEL="claude-sonnet-4-6",
                       CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
            (Path(env["CLAUDE_CONFIG_DIR"]) / ".claude.json").write_text(json.dumps({
                "hasCompletedOnboarding": True, "theme": "dark", "autoUpdates": False,
                "customApiKeyResponses": {"approved": [env["ANTHROPIC_API_KEY"]],
                                          "rejected": []}}))

            def git(*args):
                return subprocess.run(["git", "-c", "user.name=r", "-c", "user.email=r@r", *args],
                                      cwd=repo, check=True, capture_output=True,
                                      text=True).stdout.strip()

            def cli(*args, check=True, extra=None, timeout=60):
                result = subprocess.run([xaya, *args], cwd=repo, env={**env, **(extra or {})},
                                        capture_output=True, text=True, timeout=timeout)
                if check and result.returncode:
                    raise RuntimeError(f"xaya {' '.join(args)}: {result.stderr[-1500:]}")
                return result

            git("init", "-q", "-b", "main")
            (repo / "src/stats.py").write_text(BASE)
            git("add", ".")
            git("commit", "-qm", "base")
            session = cli("run", "--engine", "claude", "--claude", str(implementer), "--detach",
                          "implement", extra={"CLAUDE_CONFIG_DIR": str(implementer_config)}
                          ).stdout.strip()
            deadline = time.monotonic() + 30
            while json.loads(cli("agent", "session", "show", session).stdout)["state"] != "ready":
                if time.monotonic() > deadline:
                    raise TimeoutError("implementer session never became ready")
                time.sleep(0.2)
            (repo / "src/stats.py").write_text(CHANGED)
            cli("agent", "session", "stop", session)
            checkpoint = json.loads(cli("checkpoint", "create", session, "--json").stdout)
            check = json.loads(cli("check", "run", checkpoint["id"], "--", "true").stdout)
            assert check["status"] == "succeeded", check
            status_before = git("status", "--porcelain")

            codex_run = cli("review", "run", checkpoint["id"], "--engine", "codex",
                            "--check", check["id"], "--codex", codex, "--timeout-secs", "180",
                            check=False, timeout=240)
            (output / "codex-review.json").write_text(codex_run.stdout)
            (output / "codex-review-stderr.txt").write_text(codex_run.stderr)
            codex_review = json.loads(codex_run.stdout)
            assert codex_run.returncode != 0, "needs-fix must exit non-zero"
            assert codex_review["status"] == "completed", codex_review
            assert codex_review["verdict"] == "needs-fix", codex_review
            finding = codex_review["findings"][0]
            assert (finding["path"], finding["start"], finding["blocking"]) == \
                ("src/stats.py", 2, True), finding
            assert codex_review["native_session_id"], codex_review
            assert any(r.get("text", {}).get("format", {}).get("schema")
                       for r in codex_model.requests), "Codex sent no output schema"
            write_results = [str(i.get("output")) for r in codex_model.requests
                             for i in r.get("input", []) if i.get("type") == "function_call_output"]
            assert write_results and not forbidden_codex.exists(), write_results
            evidence["codex"] = {"verdict": codex_review["verdict"],
                                 "reasons": codex_review["reasons"],
                                 "write_attempt": write_results[0][:200],
                                 "native_session_id": codex_review["native_session_id"]}

            claude_run = cli("review", "run", checkpoint["id"], "--engine", "claude",
                             "--check", check["id"], "--claude", claude, "--timeout-secs", "180",
                             check=False, timeout=240)
            (output / "claude-review.json").write_text(claude_run.stdout)
            (output / "claude-review-stderr.txt").write_text(claude_run.stderr)
            (output / "claude-model-requests.json").write_text(
                json.dumps(claude_model.requests, indent=1))
            claude_review = json.loads(claude_run.stdout)
            assert claude_run.returncode == 0, claude_review
            assert claude_review["verdict"] == "approved", claude_review
            assert claude_review["same_engine"] is True, "the implementer was Claude"
            assert claude_review["native_session_id"], claude_review
            refused = [part for r in claude_model.requests for m in r.get("messages", [])
                       for part in (m.get("content") if isinstance(m.get("content"), list) else [])
                       if part.get("type") == "tool_result" and part.get("is_error")]
            offered = {tool.get("name") for r in claude_model.requests for tool in r.get("tools", [])}
            assert refused and not forbidden_claude.exists(), "the Write attempt was not refused"
            assert not {"Write", "Edit", "Bash"} & offered, offered
            evidence["claude"] = {"verdict": claude_review["verdict"],
                                  "tools_offered": sorted(offered),
                                  "write_attempt": str(refused[0].get("content"))[:200],
                                  "native_session_id": claude_review["native_session_id"]}

            # With a task: the handoff bundle, the criteria in the schema, one result per criterion.
            task = json.loads(cli("task", "promote", session, "--title", "Mean of an empty list",
                                  "--goal", "mean() returns 0 for an empty list",
                                  "--criterion", "mean([]) returns 0",
                                  "--criterion", "mean([2, 4]) returns 3",
                                  "--fix-budget", "1").stdout)
            schemas_before = len([r for r in codex_model.requests
                                  if "criteria" in json.dumps(r.get("text", {}))])
            task_codex = cli("review", "run", checkpoint["id"], "--engine", "codex",
                             "--check", check["id"], "--codex", codex, "--timeout-secs", "180",
                             check=False, timeout=240)
            (output / "codex-task-review.json").write_text(task_codex.stdout)
            task_codex_review = json.loads(task_codex.stdout)
            assert f"task_id={task['id']}" in task_codex.stderr, "the session's task is picked"
            assert task_codex_review["verdict"] == "needs-fix", task_codex_review
            assert [c["status"] for c in task_codex_review["criteria"]] == ["unmet", "met"]
            assert "AC1" in json.dumps(task_codex_review["reasons"]), task_codex_review
            assert task_codex_review["handoff"]["files"][0]["path"] == "src/stats.py"
            assert len([r for r in codex_model.requests
                        if "criteria" in json.dumps(r.get("text", {}))]) > schemas_before, \
                "Codex was not given the criteria schema"
            task_claude = cli("review", "run", checkpoint["id"], "--engine", "claude",
                              "--check", check["id"], "--claude", claude, "--timeout-secs", "180",
                              check=False, timeout=240)
            (output / "claude-task-review.json").write_text(task_claude.stdout)
            task_claude_review = json.loads(task_claude.stdout)
            assert task_claude.returncode == 0, task_claude_review
            assert task_claude_review["verdict"] == "approved", task_claude_review
            assert [c["status"] for c in task_claude_review["criteria"]] == ["met", "met"]
            assert task_claude_review["handoff"]["hash"] == task_codex_review["handoff"]["hash"]
            # Send the needs-fix review back: the stopped implementer is resumed and receives it.
            sent = cli("review", "send", task_codex_review["id"], "--command-id", "fix-1",
                       timeout=120)
            receipt = json.loads(sent.stdout)
            assert receipt["status"] == "succeeded", (receipt, sent.stderr)
            assert f"resuming {session}" in sent.stderr, sent.stderr
            controls = [json.loads(line) for line in
                        (implementer_config / "controls.jsonl").read_text().splitlines()]
            fix = [c["text"] for c in controls if c.get("operation") == "send"]
            assert len(fix) == 1 and "AC1" in fix[0] and "Divide by zero" in fix[0], fix
            assert "BEGIN XAYA REVIEW" in fix[0], fix[0]
            shown = json.loads(cli("task", "show", task["id"]).stdout)
            ledger = shown["task"]["ledger"]
            # Codex reported it; the later Claude review did not, so it reads "resolved": only
            # that a later review no longer reported it, not that it was fixed.
            assert [(e["status"], e["seen"], e["blocking"]) for e in ledger
                    if "divide by zero" in e["key"]] == [("resolved", 1, True)], ledger
            assert len(shown["reviews"]) == 2, shown["reviews"]
            evidence["send_back"] = {"receipt": receipt["status"], "ledger": len(ledger)}

            # The loop's policy: reviewed again on the same tree after a round, it stops for a
            # person; nothing is sent until `task continue`, which can also add rounds.
            again = cli("review", "run", checkpoint["id"], "--engine", "codex",
                        "--check", check["id"], "--codex", codex, "--timeout-secs", "180",
                        check=False, timeout=240)
            again_review = json.loads(again.stdout)
            assert again_review["verdict"] == "needs-fix", again_review
            stuck = json.loads(cli("task", "show", task["id"]).stdout)["task"]
            assert "changed nothing" in (stuck["attention"] or {}).get("reason", ""), stuck
            refused = cli("review", "send", again_review["id"], "--command-id", "fix-2",
                          check=False, timeout=120)
            assert refused.returncode != 0 and "needs attention" in refused.stderr, refused.stderr
            continued = json.loads(cli("task", "continue", task["id"], "--rounds", "1").stdout)
            assert continued["attention"] is None and continued["fix_budget"] == 2, continued
            resent = json.loads(cli("review", "send", again_review["id"], "--command-id", "fix-2",
                                    timeout=120).stdout)
            assert resent["status"] == "succeeded", resent
            evidence["loop_policy"] = {"stopped_for": stuck["attention"]["reason"],
                                       "after_continue": resent["status"]}
            evidence["task"] = {"codex": task_codex_review["verdict"],
                                "claude": task_claude_review["verdict"],
                                "handoff_hash": task_claude_review["handoff"]["hash"]}

            cli("task", "close", task["id"], "--abandoned")
            dual_task = json.loads(cli(
                "task", "promote", session, "--title", "Review mean with both engines",
                "--goal", "mean() returns 0 for an empty list",
                "--criterion", "mean([]) returns 0", "--criterion", "mean([2, 4]) returns 3",
                "--dual-review",
            ).stdout)
            dual_run = cli("review", "run", checkpoint["id"], "--check", check["id"],
                           "--codex", codex, "--claude", claude, "--timeout-secs", "180",
                           check=False, timeout=240)
            (output / "dual-review.json").write_text(dual_run.stdout)
            (output / "dual-review-stderr.txt").write_text(dual_run.stderr)
            dual = json.loads(dual_run.stdout)
            assert dual_run.returncode != 0 and dual["verdict"] == "needs-fix", dual
            assert dual["task_id"] == dual_task["id"] and len(dual["findings"]) == 1, dual
            assert [c["status"] for c in dual["criteria"]] == ["unmet", "met"], dual
            assert [(r["engine"], r["verdict"]) for r in dual["reviewers"]] == [
                ("codex", "needs-fix"), ("claude", "approved")], dual
            assert all((r.get("usage") or {}).get("output_tokens", 0) > 0
                       for r in dual["reviewers"]), dual
            evidence["dual_review"] = {"verdict": dual["verdict"], "reviewers": dual["reviewers"]}

            assert git("status", "--porcelain") == status_before, "the user's checkout changed"
            integrations = root / "run/state/integrations"
            leftovers = [p.name for p in integrations.iterdir()] if integrations.exists() else []
            assert not [p for p in leftovers if p.startswith("review-")], leftovers
            evidence["user_checkout_unchanged"] = True
            evidence["review_checkouts_removed"] = True
            evidence["model_errors"] = codex_model.errors + claude_model.errors
            assert not evidence["model_errors"], evidence["model_errors"]
            evidence["status"] = "passed"
    finally:
        subprocess.run([xaya, "agent", "stop"], env=env, capture_output=True, timeout=30)
        shutil.rmtree(root, ignore_errors=True)
    return evidence


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--xaya", type=Path, default=ROOT / "target/debug/xaya")
    parser.add_argument("--codex", default=shutil.which("codex"))
    parser.add_argument("--claude", default=shutil.which("claude"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    try:
        evidence = run(args.output, str(args.xaya.resolve()), str(Path(args.codex).resolve()),
                       str(Path(args.claude).resolve()))
    except BaseException as error:
        (args.output / "report.json").write_text(
            json.dumps({"status": "failed", "error": repr(error)}, indent=2))
        raise
    (args.output / "report.json").write_text(json.dumps(evidence, indent=2))
    print(json.dumps(evidence, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
