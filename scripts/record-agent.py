#!/usr/bin/env python3
"""Record what a coding agent writes to its terminal during one short turn.

    python3 scripts/record-agent.py claude
    python3 scripts/record-agent.py codex --output codex.bin --prompt "Reply with hi."

Runs the agent in a pseudo-terminal like xaya's, answers its OSC 7501 probe and DA1 query, types
one prompt, waits for the turn to finish (state=done), interrupts the agent and saves everything it
wrote. Then lists the OSC 7501 reports it sent and whether it used the alternate screen and mouse
reporting, which decide what xaya can do with it.

This starts the real agent in the current directory with your account: the prompt is sent to its
model and uses your quota. Keep it short and tool-free.
"""

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

SUPPORT_REPLY = (b"\x1b]7501;?:states=idle,working,done,blocked,error"
                 b":kinds=permission,question,auth\x1b\\")
DA1_REPLY = b"\x1b[?62;22c"


def record(command, prompt, timeout, columns, lines):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.execvp(command[0], command)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", lines, columns, 0, 0))

    output = bytearray()
    start = time.monotonic()
    sent = done = False
    interrupt_at = None
    while time.monotonic() - start < timeout:
        readable, _, _ = select.select([fd], [], [], 0.2)
        if readable:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            output += chunk
            if b"\x1b]7501;?" in chunk:
                os.write(fd, SUPPORT_REPLY)
            if b"\x1b[c" in chunk or b"\x1b[0c" in chunk:
                os.write(fd, DA1_REPLY)

        # Give the agent time to start, then type the prompt like a user would.
        if not sent and time.monotonic() - start > 6:
            for byte in prompt.encode():
                os.write(fd, bytes([byte]))
                time.sleep(0.01)
            time.sleep(0.5)
            os.write(fd, b"\r")
            sent = True
        if sent and not done and b"state=done" in output[-4096:]:
            done = True
            interrupt_at = time.monotonic() + 3
        if interrupt_at and time.monotonic() > interrupt_at:
            os.write(fd, b"\x03")
            time.sleep(0.3)
            os.write(fd, b"\x03")
            interrupt_at = None
    return bytes(output), done


def summarize(output):
    print("OSC 7501 reports:")
    for match in re.finditer(rb"\x1b\]7501;([^\x07\x1b]*)", output):
        print(f"  @{match.start():>7}  {match.group(1).decode(errors='replace')}")
    for name, pattern in [
        ("alternate screen (1049)", rb"\x1b\[\?1049h"),
        ("mouse reporting (1000)", rb"\x1b\[\?1000h"),
        ("any-motion mouse (1003)", rb"\x1b\[\?1003h"),
        ("synchronized output (2026)", rb"\x1b\[\?2026h"),
        ("shell integration (OSC 133)", rb"\x1b\]133;"),
    ]:
        print(f"{name:<30} {'yes' if re.search(pattern, output) else 'no'}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("agent", help="command to run, such as claude or codex")
    parser.add_argument("args", nargs="*", help="arguments for the agent")
    parser.add_argument("--prompt", default="Reply with just the word hi. Do not use any tools.")
    parser.add_argument("--output", help="file for the raw output (default <agent>.bin)")
    parser.add_argument("--timeout", type=float, default=90, help="seconds to record at most")
    parser.add_argument("--size", default="100x30", help="terminal size, COLUMNSxLINES")
    options = parser.parse_args()

    columns, lines = map(int, options.size.lower().split("x"))
    output, done = record([options.agent, *options.args], options.prompt, options.timeout,
                          columns, lines)
    path = options.output or f"{os.path.basename(options.agent)}.bin"
    with open(path, "wb") as file:
        file.write(output)

    print(f"{len(output)} bytes saved to {path}; turn finished: {'yes' if done else 'no'}")
    summarize(output)
    sys.exit(0 if done else 1)


if __name__ == "__main__":
    main()
