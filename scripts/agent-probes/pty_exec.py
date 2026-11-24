"""Acquire the slave PTY in a fresh session, then replace this helper with the CLI.

This avoids running a Python preexec_fn after fork in the multithreaded fixture process.
Invoked internally with stdin/stdout/stderr set to the slave and start_new_session=True.
"""

import fcntl
import os
import sys
import termios


if __name__ == "__main__":
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.execv(sys.argv[1], sys.argv[1:])
