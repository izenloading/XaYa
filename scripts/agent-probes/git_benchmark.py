"""G1 snapshots on a disposable local clone of a real committed repository."""

import hashlib
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import time


def benchmark_repo(source, env, root, number):
    source = source.resolve()
    destination = root / f"real-repo-{number}"
    local_env = dict(env, GIT_OPTIONAL_LOCKS="0")

    def git(directory, *args, index=None):
        git_env = dict(local_env)
        if index:
            git_env["GIT_INDEX_FILE"] = str(index)
        return subprocess.run(["git", "-c", "user.name=probe", "-c", "user.email=probe@localhost",
            "-C", str(directory), *args], env=git_env, check=True, capture_output=True, timeout=60).stdout

    source_head = git(source, "rev-parse", "HEAD").decode().strip()
    source_index = Path(git(source, "rev-parse", "--path-format=absolute", "--git-path", "index").decode().strip())
    source_hash = hashlib.sha256(source_index.read_bytes()).hexdigest() if source_index.exists() else None
    subprocess.run(["git", "clone", "--shared", "--local", "--no-checkout", "--", str(source), str(destination)],
                   env=local_env, check=True, capture_output=True, timeout=60)
    # Explicit commit avoids accidentally benchmarking a concurrently moved source branch.
    git(destination, "checkout", "--detach", source_head)
    files = git(destination, "ls-files", "-z").split(b"\0")[:-1]
    sizes = git(destination, "ls-tree", "-r", "-l", source_head).decode().splitlines()
    committed_bytes = sum(int(line.split()[3]) for line in sizes if line.split()[3].isdigit())
    (destination / "xaya-bench-staged.txt").write_text("user staged\n")
    git(destination, "add", "--", "xaya-bench-staged.txt")
    original_index = (destination / ".git/index").read_bytes()
    # Do not run repository-provided filters/hooks: clone does not copy those config entries.
    timings = []
    scratch = root / f"real-index-{number}"

    def snapshot():
        shutil.copyfile(destination / ".git/index", scratch)
        git(destination, "add", "--all", index=scratch)
        return git(destination, "write-tree", index=scratch).decode().strip()

    for sample in range(4):
        start = time.monotonic()
        before = snapshot()
        changed = destination / "xaya-bench-change.txt"
        changed.write_text(f"sample {sample}\n")
        after = snapshot()
        timings.append(time.monotonic() - start)
        numstat = git(destination, "diff", "--numstat", before, after).decode()
        if "xaya-bench-change.txt" not in numstat or "xaya-bench-staged.txt" in numstat:
            raise RuntimeError("Real-repository snapshot diff is incorrect")
        if (destination / ".git/index").read_bytes() != original_index:
            raise RuntimeError("Benchmark modified the staged index")
    if git(source, "rev-parse", "HEAD").decode().strip() != source_head:
        raise RuntimeError("Source HEAD changed concurrently")
    if source_index.exists() and hashlib.sha256(source_index.read_bytes()).hexdigest() != source_hash:
        raise RuntimeError("Source index changed concurrently")
    return {"source": str(source), "commit": source_head, "files": len(files), "committed_bytes": committed_bytes,
        "two_snapshot_seconds": [round(value, 4) for value in timings],
        "first_sample_seconds": round(timings[0], 4), "median_seconds": round(statistics.median(timings), 4),
        "max_seconds": round(max(timings), 4), "under_one_second": max(timings) < 1,
        "clone_index_unchanged": True, "source_head_and_index_unchanged": True,
        "scope": "Real committed checkout; filesystem caches not forcibly flushed"}
