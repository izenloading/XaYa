"""Split panes through `xaya msg split`, with screenshots on macOS.

Runs once per renderer given; on macOS both Metal and OpenGL, since they place panes differently.
"""

import time

from harness import FAKE_AGENT, MACOS, Xaya, fresh_dir, wait_until


def run(binary, work, checks):
    for renderer in ("metal", "glsl3") if MACOS else ("glsl3",):
        print(f"  [{renderer}]")
        run_with(binary, fresh_dir(work / renderer), checks, renderer)


def run_with(binary, work, checks, renderer):
    config = f"[window]\ndimensions = {{ columns = 100, lines = 24 }}\n[debug]\nrenderer = \"{renderer}\"\n"
    with Xaya(binary, work, send_text=True, config=config) as xaya:
        window = xaya.window_ids()[0]
        xaya.msg("send-text", "-w", window, "echo LEFT-PANE", "--enter")
        wait_until(lambda: "LEFT-PANE\n" in xaya.text(window))

        result = xaya.msg("split", "-w", window, "-c", "echo RIGHT-PANE")
        checks.check(f"{renderer}: split is accepted", result.returncode == 0, result)
        right = wait_until(lambda: "RIGHT-PANE\n" in xaya.text(window))
        checks.check(f"{renderer}: the new pane gets the focus",
                     right and "LEFT-PANE" not in xaya.text(window), xaya.text(window))
        checks.check(f"{renderer}: panes stay in one window", len(xaya.window_ids()) == 1,
                     xaya.windows())
        xaya.screenshot("1-right")

        agent = FAKE_AGENT.format(marker="DOWN-PANE")
        xaya.msg("split", "-w", window, "--down", "-c", agent)
        working = wait_until(lambda: (xaya.windows() or [{}])[0].get("state") == "working")
        status = xaya.windows()[0]
        checks.check(f"{renderer}: the window shows its panes' status",
                     working and status["app"] == "codex", status)
        checks.check(f"{renderer}: text comes from the focused pane",
                     wait_until(lambda: "DOWN-PANE" in xaya.text(window)), xaya.text(window))
        xaya.screenshot("2-down")

        # The focused pane runs sleep, so pasted text just shows there.
        xaya.msg("send-text", "-w", window, "typed-into-down")
        checks.check(f"{renderer}: typing reaches the focused pane",
                     wait_until(lambda: "typed-into-down" in xaya.text(window)))

        # A pane whose shell exits closes and gives its space back.
        # It may be gone before it's ever seen, so give it time rather than wait for a state.
        xaya.msg("split", "-w", window, "-c", "exit")
        time.sleep(3)
        checks.check(f"{renderer}: an exited pane closes, the window stays",
                     xaya.running() and len(xaya.window_ids()) == 1
                     and "DOWN-PANE" in xaya.text(window), xaya.text(window))
        xaya.screenshot("3-after-exit")

        if xaya.resize(1100, 700):
            time.sleep(1.5)
            checks.check(f"{renderer}: panes follow a resize", xaya.running())
            xaya.screenshot("4-resized")

        checks.check(f"{renderer}: no panics or errors logged", not xaya.problems(),
                     xaya.problems()[:5])
