"""What an agent's turns change in its checkout, as `msg status` reports it."""

import subprocess

from harness import Xaya, fresh_dir, wait_until

# Two turns of a fake Codex: the first edits a file and adds one, the second changes nothing. The
# second waits for the test to create `go`, so the first's changes can be seen in between.
AGENT = """\
report() { printf '\\033]7501;state=%s:app=codex\\007' "$1"; }
report working
sleep 1
printf 'one\\ntwo\\n' > notes.md
echo more >> README
sleep 0.5
report done
echo FIRST-TURN-DONE
while [ ! -e {go} ]; do sleep 0.1; done
report working
sleep 1
report done
echo SECOND-TURN-DONE
exec sleep 600
"""


def git(*args, cwd):
    command = ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args]
    result = subprocess.run(command, cwd=cwd, check=True, capture_output=True, text=True)
    return result.stdout


def run(binary, work, checks):
    repo = fresh_dir(work / "project")
    git("init", "-q", "-b", "main", cwd=repo)
    (repo / "README").write_text("hi\n")
    git("add", ".", cwd=repo)
    git("commit", "-q", "-m", "init", cwd=repo)
    # Staged by the user before the agent's turn.
    (repo / "staged.txt").write_text("mine\n")
    git("add", "staged.txt", cwd=repo)

    go = work / "go"
    script = work / "agent.sh"
    script.write_text(AGENT.replace("{go}", str(go)))

    with Xaya(binary, work, send_text=True, args=["--working-directory", repo]) as xaya:
        window = xaya.window_ids()[0]
        xaya.msg("send-text", "-w", window, f"sh {script}", "--enter")

        def changes():
            return next((w.get("changes") for w in xaya.windows() if w["window_id"] == window),
                        None)

        first = wait_until(changes, timeout=20)
        checks.check("the first turn's changes are reported", first is not None, xaya.windows())
        if first:
            counts = (first["files"], first["insertions"], first["deletions"])
            checks.check("two files, three lines added", counts == (2, 3, 0), first)
            checks.check("in the checkout", first["root"] == str(repo), first)
            stat = git("diff", "--stat", first["before"], first["after"], cwd=repo)
            checks.check("the trees diff to the turn's files",
                         "README" in stat and "notes.md" in stat and "staged.txt" not in stat, stat)

        status = git("status", "--porcelain", cwd=repo)
        checks.check("what the user staged stays staged, the rest unstaged",
                     "A  staged.txt" in status and " M README" in status
                     and "?? notes.md" in status, status)

        go.write_text("")
        done = wait_until(lambda: "SECOND-TURN-DONE" in xaya.text(window), timeout=20)
        checks.check("the second turn ends", done, xaya.text(window))
        cleared = wait_until(lambda: changes() is None, timeout=10)
        checks.check("a turn that changed nothing clears them", cleared, changes())
        checks.check("no panics or errors logged", not xaya.problems(), xaya.problems()[:5])
