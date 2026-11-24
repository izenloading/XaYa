"""Worktree tabs and the shared checkout warning, against a temporary git repository."""

import subprocess

from harness import FAKE_AGENT, Xaya, fresh_dir, outside_git, wait_until


def git(*args, cwd):
    command = ["git", "-c", "user.name=t", "-c", "user.email=t@t", *args]
    return subprocess.run(command, cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def run(binary, work, checks):
    repo = fresh_dir(work / "project")
    plain = fresh_dir(work / "plain")
    git("init", "-q", "-b", "main", cwd=repo)
    (repo / "README").write_text("hi\n")
    git("add", ".", cwd=repo)
    git("commit", "-q", "-m", "init", cwd=repo)
    if not outside_git(plain):
        # create-worktree there would create a worktree of that repository.
        raise RuntimeError(f"{plain} is inside a git repository")

    with Xaya(binary, work, send_text=True, args=["--working-directory", repo]) as xaya:
        first = xaya.window_ids()[0]

        # Through IPC, starting a command in the new tab.
        result = xaya.msg("create-worktree", "-w", first, "--command", "echo started-in-$(pwd -P)")
        expected = work / "project.worktrees" / "agent-1"
        checks.check("create-worktree prints the worktree's path",
                     result.returncode == 0 and result.stdout.strip() == str(expected), result)
        checks.check("the worktree is on agent-1",
                     expected.is_dir() and git("branch", "--show-current", cwd=expected) == "agent-1")

        tabs = wait_until(lambda: [w for w in xaya.windows() if w["window_id"] != first])
        checks.check("a tab opens", len(tabs or []) == 1, xaya.windows())
        if tabs:
            tab = tabs[0]
            checks.check("its title starts with the branch", tab["title"].startswith("⎇ agent-1 · "),
                         tab["title"])
            ran = wait_until(lambda: f"started-in-{expected}" in xaya.text(tab["window_id"]))
            checks.check("the command runs in the worktree", ran, xaya.text(tab["window_id"]))

        # Outside a repository.
        xaya.msg("create-window", "--working-directory", plain)
        wait_until(lambda: len(xaya.window_ids()) == 3)
        known = {first, *[tab["window_id"] for tab in tabs or []]}
        outside = next(id for id in xaya.window_ids() if id not in known)
        result = xaya.msg("create-worktree", "-w", outside)
        checks.check("outside a repository it fails with git's reason",
                     result.returncode != 0 and "not a git repository" in result.stderr, result)

        # Two agents in the main checkout are warned about, once.
        agent = FAKE_AGENT.format(marker="AGENT")
        xaya.msg("send-text", "-w", first, agent, "--enter")
        xaya.msg("create-window", "--working-directory", repo)
        wait_until(lambda: len(xaya.window_ids()) == 4)
        second = next(id for id in xaya.window_ids() if id not in known | {outside})
        xaya.msg("send-text", "-w", second, agent, "--enter")

        # Checkouts are checked every 15 seconds.
        warned = wait_until(lambda: f"share the checkout {repo}" in xaya.log(), timeout=25,
                            interval=1)
        checks.check("agents sharing a checkout are warned about", warned)
        checks.check("only once", xaya.log().count("share the checkout") == 1,
                     xaya.log().count("share the checkout"))
        checks.check("no panics or errors logged", not xaya.problems(), xaya.problems()[:5])
