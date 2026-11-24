"""Process-ownership evidence for a PTY that XaYa itself owns (macOS/Linux `ps`).

Two independent signals are kept apart on purpose:
- `tree`: descendants of the PTY root, which misses processes reparented to init/launchd;
- `marked`: processes whose environment carries an unguessable launch marker, which survives
  reparenting but can be lost if a program scrubs its environment.
A process is identified by (pid, start time); a reused PID with another start time is never signalled.
"""

from dataclasses import dataclass
import os
import re
import signal
import subprocess


class OwnershipError(RuntimeError):
    pass


@dataclass(frozen=True)
class ProcessEntry:
    pid: int
    ppid: int
    pgid: int
    started: str
    command: str
    marked: bool


ROW = re.compile(r"^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\S+\s+\S+\s+\S+\s+\S+\s+\S+)\s+(.*)$")
MARKER = re.compile(r"^[A-Z_]+=[0-9a-f]{16,}$")


def parse_ps(text, marker):
    """Parse `ps -A -E -ww -o pid=,ppid=,pgid=,lstart=,command=` rows."""
    if not MARKER.match(marker):
        raise OwnershipError("Ownership marker must be NAME=<at least 16 hex digits>")
    entries = []
    for line in text.splitlines():
        if not line.strip():
            continue
        match = ROW.match(line)
        if not match:
            raise OwnershipError(f"Unparseable process row: {line[:80]!r}")
        pid, ppid, pgid, started, command = match.groups()
        # Environment words follow argv; only an exact whitespace-delimited word counts.
        entries.append(ProcessEntry(int(pid), int(ppid), int(pgid), " ".join(started.split()),
                                    command, marker in command.split()))
    return entries


def proc_snapshot(marker, proc="/proc"):
    """Linux: exact NUL-separated environment; start time in clock ticks since boot."""
    if not MARKER.match(marker):
        raise OwnershipError("Ownership marker must be NAME=<at least 16 hex digits>")
    entries = []
    for name in os.listdir(proc):
        if not name.isdigit():
            continue
        base = os.path.join(proc, name)
        try:
            with open(os.path.join(base, "stat"), "rb") as file:
                stat = file.read().decode(errors="replace")
            with open(os.path.join(base, "cmdline"), "rb") as file:
                command = file.read().replace(b"\0", b" ").decode(errors="replace").strip()
        except (FileNotFoundError, ProcessLookupError):
            continue  # Exited during the walk.
        fields = stat[stat.rindex(")") + 2:].split()
        try:
            with open(os.path.join(base, "environ"), "rb") as file:
                marked = marker.encode() in file.read().split(b"\0")
        except (PermissionError, FileNotFoundError, ProcessLookupError):
            marked = False  # Unreadable is not ownership evidence.
        entries.append(ProcessEntry(int(name), int(fields[1]), int(fields[2]), fields[19], command, marked))
    return entries


def snapshot(marker):
    if os.path.isdir("/proc/self"):
        return proc_snapshot(marker)
    result = subprocess.run(["ps", "-A", "-E", "-ww", "-o", "pid=,ppid=,pgid=,lstart=,command="],
                            env={"PATH": "/bin:/usr/bin", "LC_ALL": "C"},
                            capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise OwnershipError(f"ps failed: {result.stderr.strip()}")
    return parse_ps(result.stdout, marker)


def descendants(entries, root):
    children = {}
    for entry in entries:
        children.setdefault(entry.ppid, []).append(entry.pid)
    found, pending = set(), list(children.get(root, []))
    while pending:
        pid = pending.pop()
        if pid in found or pid == root:
            continue
        found.add(pid)
        pending.extend(children.get(pid, []))
    return found


def ownership(entries, root):
    alive = any(entry.pid == root for entry in entries)
    # Without a live root, a stale ppid is not tree evidence (it can only be PID reuse or a torn read).
    tree = descendants(entries, root) if alive else set()
    marked = {entry.pid for entry in entries if entry.marked and entry.pid != root}
    return {"root_alive": alive, "tree": tree, "marked": marked,
            "reparented": marked - tree, "unmarked_descendants": tree - marked}


def describe(entries, pids):
    return [{"pid": e.pid, "ppid": e.ppid, "pgid": e.pgid, "started": e.started,
             "argv_head": e.command.split(" ")[0], "marked": e.marked}
            for e in entries if e.pid in pids]


def terminate_owned(expected, marker, sig=signal.SIGKILL):
    """Signal only processes whose pid, start time and marker are re-observed unchanged."""
    current = {(entry.pid, entry.started): entry for entry in snapshot(marker)}
    killed, skipped = [], []
    for entry in expected:
        live = current.get((entry.pid, entry.started))
        if live is None or not live.marked:
            skipped.append(entry.pid)
            continue
        try:
            os.kill(entry.pid, sig)
            killed.append(entry.pid)
        except ProcessLookupError:
            skipped.append(entry.pid)
    return {"killed": killed, "skipped": skipped}
