Scripts
=======

## Flamegraph

Run the release version of Alacritty while recording call stacks. After the
Alacritty process exits, a flamegraph will be generated and it's URI printed
as the only output to STDOUT.

```sh
./create-flamegraph.sh
```

Running this script depends on an installation of `perf`.

## ANSI Color Tests

We include a few scripts for testing the color of text inside a terminal. The
first shows various foreground and background variants. The second enumerates
all the colors of a standard terminal. The third enumerates the 24-bit colors.

```sh
./fg-bg.sh
./colors.sh
./24-bit-colors.sh
```

## End-to-end tests

Drive a real XaYa through `xaya msg`: IPC, session restore, worktree tabs and split panes (on
macOS with both the Metal and OpenGL renderers).

```sh
python3 scripts/e2e/run.py                # build, then run every test
python3 scripts/e2e/run.py split restore  # only these
python3 scripts/e2e/run.py --no-build --xaya target/release/xaya
```

Each test opens its own XaYa windows for a few seconds, isolated from yours: its own config,
socket, state directory and, for session restore, `HOME` and `CODEX_HOME`, with fake `claude` and
`codex` executables. Files, logs and macOS window screenshots stay in `$TMPDIR/xaya-e2e/<test>`,
which must be outside any git repository since the tests create repositories and worktrees.
Nothing is typed into other applications. Screenshots and the resize check need macOS; the
resize also needs Accessibility permission for the terminal running the tests.

## Recording an agent

For isolated, account-free P0 transport and git probes, see
[`agent-probes/README.md`](agent-probes/README.md). These produce a capability matrix and do not
submit production model turns. The optional `--dual` mode drives the real Codex TUI with a
deterministic loopback model fixture.

`record-agent.py` runs Claude Code or Codex in a pseudo-terminal, answers its OSC 7501 probe, sends
one short prompt and saves everything it writes, then lists its status reports and whether it uses
the alternate screen and mouse reporting. Use it to see what a new agent release changed.

```sh
python3 scripts/record-agent.py claude
python3 scripts/record-agent.py codex --prompt "Reply with hi."
```

It starts the real agent with your account in the current directory, so the prompt uses your
quota.

