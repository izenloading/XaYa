#!/usr/bin/env python3
"""Run xaya's end-to-end tests against a real xaya, driven through `xaya msg`.

    python3 scripts/e2e/run.py                 # build, then run every test
    python3 scripts/e2e/run.py split restore   # only these
    python3 scripts/e2e/run.py --no-build --xaya target/release/xaya

Each test starts its own isolated xaya windows (they appear on screen briefly) and leaves its
files, logs and, on macOS, window screenshots under $TMPDIR/xaya-e2e/<test>. Nothing is typed into other
applications. `panel` types keys through System Events, so on macOS the app running this needs
Accessibility; keys are sent only while the test window is frontmost. Exits non-zero when a check
fails.
"""

import argparse
import importlib
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from harness import ROOT, Checks, fresh_dir, outside_git  # noqa: E402

TESTS = ["ipc", "restore", "worktree", "split", "turns", "panel"]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tests", nargs="*", help=f"tests to run: {', '.join(TESTS)} (default all)")
    parser.add_argument("--xaya", type=Path, default=ROOT / "target/debug/xaya",
                        help="xaya binary to test")
    parser.add_argument("--no-build", action="store_true", help="don't run cargo build first")
    parser.add_argument("--work", type=Path, default=Path(tempfile.gettempdir()) / "xaya-e2e",
                        help="where tests keep their files; must be outside any git repository")
    args = parser.parse_args()
    unknown = [name for name in args.tests if name not in TESTS]
    if unknown:
        parser.error(f"unknown tests: {', '.join(unknown)}; choose from {', '.join(TESTS)}")

    if not args.no_build:
        subprocess.run(["cargo", "build", "--workspace"], cwd=ROOT, check=True)
    if not args.xaya.exists():
        sys.exit(f"{args.xaya} doesn't exist; build it or pass --xaya")

    # Tests create git repositories and worktrees; inside another repository they'd touch it.
    work_root = fresh_dir(args.work)
    if not outside_git(work_root):
        sys.exit(f"{work_root} is inside a git repository; pass --work elsewhere")

    failures = []
    for name in args.tests or TESTS:
        print(f"== {name}")
        checks = Checks(name)
        work = fresh_dir(work_root / name)
        start = time.monotonic()
        try:
            importlib.import_module(f"test_{name}").run(args.xaya.resolve(), work, checks)
        except Exception as error:  # A crashed test fails, the others still run.
            checks.check("runs to the end", False, repr(error))
        print(f"   {time.monotonic() - start:.0f}s, files in {work}")
        failures += checks.failures

    print()
    if failures:
        print(f"{len(failures)} failed:")
        for failure in failures:
            print(f"  {failure}")
        sys.exit(1)
    print("All checks passed.")


if __name__ == "__main__":
    main()
