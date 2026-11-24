# P0 agent probes

## Tool-only marker (T1, T2)

```sh
/tmp/xaya-acceptance-venv/bin/python scripts/agent-probes/claude_tool_marker.py --output /tmp/xaya-t1-new
/tmp/xaya-acceptance-venv/bin/python scripts/agent-probes/codex_tool_marker.py --output /tmp/xaya-t2-new
```

Can a second marker reach an engine's tool processes and nothing else? T1 gives Claude a
SessionStart command hook that appends `export XAYA_TOOL=<marker>` to `$CLAUDE_ENV_FILE`; T2 passes
`-c shell_environment_policy.set.XAYA_TOOL=<marker>` to `codex exec`. Each records whether the tool
command and its `setsid` grandchild carry the marker, and whether the engine itself and a stdio MCP
server do not. Real Claude 2.1.326 and Codex 0.169 with loopback models: both passed twice. Codex
starts MCP servers with a filtered environment, so they also lack `XAYA_OWNER` (recorded as
`evidence_mcp_inherits_owner: false`).

## Owned-PTY Claude runtime (O1-O7)

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/claude_owned_pty.py --output /tmp/xaya-claude-owned-new-run
```

Runs a plain interactive Claude TUI in a PTY the probe owns (not `--bg`), with the probe Mods plugin,
an HTTP permission hook and a random `XAYA_OWNER` launch marker. It checks the honoured `--session-id`,
Mods streaming/program send, OSC 7501 beside Mods, background Bash ownership by process tree and marker
(including a `setsid` grandchild reparented to init), what TaskStop and PTY hangup leave alive,
identity-checked owner cleanup without late writes, and `--resume` in a fresh PTY. `owned_pty.py`
reads `ps -E` on macOS and `/proc` on Linux.

The Claude product lifecycle runs Claude as a plain TUI in an XaYa-owned PTY host:

```sh
cargo build --workspace --locked
/tmp/xaya-acceptance-venv/bin/python scripts/agent-probes/managed_claude.py \
  --xaya target/debug/xaya --output /tmp/xaya-claude-new-run
node --test scripts/agent-probes/test_managed_claude_mod.mjs
```

This uses the real installed Claude, the Rust service, a disposable configuration and loopback model.
It covers the XaYa-chosen `--session-id`, native input, exclusive attach and Ctrl+Z detach, permission
UI with a real file write, controller cancellation, a real backgrounded Bash tool whose `setsid`
grandchild must end on stop without a late write, an unmarked process in the same checkout left alone,
stop then checkpoint/check/land, input refusal after the service is killed, the restarted service
rejoining the same live TUI (same PID and native session, a native turn reaching the model, and the
refused input never sent), explicit stop releasing the checkout, and `xaya attach` resuming the stopped session with
`--resume` in a new owned host (same model session ID, full prior context). See [managed Claude scope](../../docs/managed-claude.zh-CN.md).

C6 checks that a dedicated `codex app-server --listen unix://` keeps a turn and its pending approval
when its only client disconnects, and re-sends that approval to a new client that resumes the thread:

```sh
python scripts/agent-probes/codex_reconnect.py --output /tmp/xaya-c6-run
```

C7 checks what a second client and a dropped native TUI connection do to one app-server: whether
turn events and approval requests reach every client subscribed to the thread, whether the others
see `serverRequest/resolved`, where a pending approval goes when its holder disconnects, and whether
a `--remote` TUI exits or reconnects (idle, mid-turn and after a 60 s outage):

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/codex_multiclient.py \
  --codex "$(command -v codex)" --output /tmp/xaya-c7-run
```

R1 compares Codex's native review entry points (`codex review --commit`, `codex exec review --json`
and app-server `review/start`) on one commit: whether they stay read-only and whether the structured
review (priority, confidence, location, overall verdict) is recoverable or only rendered text:

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/codex_native_review.py \
  --codex "$(command -v codex)" --output /tmp/xaya-r1-run
```

`managed_review.py` runs `xaya review run` with the real Codex and Claude CLIs as reviewers against
loopback models that first try to write and then answer in the review schema. It requires a P1
finding to give needs-fix and a clean review with a passing check to give approved, the writes to
be refused, the user's checkout to stay unchanged and the private review checkouts to be removed.
It then promotes the session to a task and reviews again: the session's task is picked, Codex gets
the criteria schema, an unmet criterion gives needs-fix, all criteria met gives approved, and both
reviews carry the same handoff bundle. Finally `xaya review send` resumes the stopped implementer
and delivers the fenced fixes (receipt succeeded), and `task show` has the finding in its ledger.
With a fix budget of 1, a second needs-fix review of the same checkpoint stops the loop (the round
changed nothing), the send is refused, and `task continue --rounds 1` lets it through:

```sh
cargo build --workspace --locked
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/managed_review.py --output /tmp/xaya-review-run
```

The separate product smoke test exercises the **Rust** service and `xaya run`/`xaya attach`:

```sh
cargo build --workspace --locked
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/managed_codex.py --output /tmp/xaya-l1-new-run
```

It also kills the service while the native TUI is attached, without running any other command, and
requires `xaya attach` to start it again, the TUI to reconnect to the same backend and take a native
turn, and the TUI to end when the session is stopped. It uses the same optional `websockets`
dependency and isolated local-model setup described below.
The product smoke covers controller sends and streaming, contiguous event sequence numbers,
command-ID deduplication, command/file approvals, stale replies, turn interruption, native input
after interruption, detach/reattach, and backend cleanup. It verifies the approved file's contents.
It also restarts the product service, checks the persisted internal/native IDs, earlier command
receipt and a changed event epoch, explicitly attaches the same thread to a new backend, and
completes another native turn. Abrupt service-loss recovery is covered separately by fake-engine
Rust integration tests; the native smoke restarts the service gracefully.
The same run initializes a disposable Git repository with a staged user change and a divergent
target worktree. The native file approval feeds a private checkpoint and a hash-trusted two-step
project plan. It then previews, rechecks, verifies and lands the integrated candidate on both a
checked-out and an unattached target branch. Assertions cover target-only content, exact candidate
bindings, source HEAD/index preservation, an unrelated ignored target file, integration directory
cleanup, reconciliation and idempotent retries after service restart. `landing.json` preserves
each successful stage even when a later stage fails. These are real native-TUI/CLI checks, not
XaYa GUI keyboard or GPU checks, and do not measure model quality.

CI runs the account-free Python and Node fixture regressions; the installed-Codex smoke remains
explicitly opt-in:

```sh
python3 -m unittest discover -s scripts/agent-probes -p 'test_*.py' -v
node --test scripts/agent-probes/test_claude_mod.mjs
```

See [managed Codex sessions](../../docs/managed-codex.zh-CN.md) for the shipped scope and limitations.

Run the first transport and git probes on macOS/Linux with Python 3 and an installed Codex CLI:

```sh
python3 scripts/agent-probes/run.py --output /tmp/xaya-p0-run-1
python3 -m unittest discover -s scripts/agent-probes -p 'test_*.py'
```

The output directory must be new and outside every git checkout. It contains `report.json`,
`report.md`, stderr logs and isolated homes/workspace. Existing directories are never emptied.
`--codex /absolute/path/to/codex` selects another CLI; `--files 10000` changes the git fixture size.

The default run uses the Python standard library.
Protocol reference: [official Codex App Server documentation](https://learn.chatgpt.com/docs/app-server).

## Native Codex TUI and local model

```sh
python3 -m venv /tmp/xaya-probes-venv
/tmp/xaya-probes-venv/bin/python -m pip install -r scripts/agent-probes/requirements.txt
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/run.py --dual --output /tmp/xaya-p0-dual-run-1
/tmp/xaya-probes-venv/bin/python -m unittest discover -s scripts/agent-probes -p 'test_*.py'
```

`--dual` requires Python 3.11+ and the pinned `websockets` probe dependency. It runs the real
Codex app-server and native TUI in a private PTY, without opening a desktop window. A deterministic
HTTP SSE model fixture listens only on `127.0.0.1`; it never forwards requests. Its fixed commands
only print a marker, and its fixed patches only create files in the isolated workspace.

The Unix WebSocket proxy forwards the native TUI's connection and injects control requests on
that same backend connection with a separate ID prefix. It obtains the main thread ID from the
TUI's resume response. Both WebSocket ends disable message-size limits, allowing native plugin
metadata larger than the library's default 1 MiB. The proxy handles first-answer-wins approval
replies; later native or controller replies are dropped. This is a probe, not a product daemon.

Additional artifacts: `tui.bin` (raw terminal output), `dual-events.json`, `model-requests.json`,
`dual-backend.log`, and `dual-error.log` on failure. These contain only isolated fixture data.

The runner constructs an environment from a small allowlist, creates independent `HOME`,
`CODEX_HOME`, `CLAUDE_CONFIG_DIR` and XDG directories, and substitutes fake keys. Codex's model
provider points to an unreachable loopback address by default, or the local fixture with `--dual`.
No real account is used. Only probe-owned process groups are terminated during cleanup.

Default evidence:

- **C1 partial:** real app-server accepts an HTTP WebSocket upgrade over a custom Unix socket;
  stdio initialization uses the isolated Codex home. Native TUI and forwarding are still pending.
- **C2 partial:** `thread/start` returns the main thread ID in its response. No injected model
  turn or TUI visibility test yet. An empty thread is not evidence of persisted resume support.
- **G1 partial:** temporary-index snapshots preserve the user's staged index byte for byte and
  produce the expected diff in a synthetic repository. Reported time covers both snapshots;
  production large-repository benchmarking remains pending. This fixture reproduces the git
  algorithm; the Rust implementation itself is covered by unit tests and the `turns` E2E.
- Other P0 items are explicitly `not_run`.

With `--dual`, C1–C4 have passed on macOS with Codex 0.167.2:

- **C1:** native TUI connects through the Unix WebSocket proxy, resumes the exact thread and
  completes its own prompt.
- **C2:** injected turn output appears in the same TUI; native input works again afterward.
- **C3:** command and file approvals accept/decline correctly; native TUI and controller can each
  answer a forwarded command approval. Resolved requests reject duplicate replies. After the
  controller decides, the native prompt is usable again. The XaYa panel itself is not integrated.
- **C4:** a held stream emits a delta, `turn/interrupt` ends it as interrupted; after terminating
  and restarting the dedicated backend, the same thread resumes and completes another turn.
  All 13 user messages are verified in order after restart. Tool-process cancellation and
  crash recovery of a future agentd are separate reliability work.

These are protocol integration results with a deterministic fixture, not proof of production
model quality, full terminal rendering correctness, or a released managed-session feature.

Exit 0 means all implemented foundations ran successfully, **not** that P0 passed. Missing CLI,
RPC failure, timeout, unexpected transport or an incorrect git diff returns nonzero and writes a
failure report. A1's original UUID criterion remains unmet; H1/E1/G1 have opt-in probes below.
Without the optional dependency, the proxy unit tests are explicitly skipped; fixture and base
tests still run. Failures retain earlier stage evidence and add `dual_error` to the report.

## Daemon comparison and Claude background sessions

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/run.py \
  --dual --daemon --claude --output /tmp/xaya-p0-engines-run-1
```

`--daemon` compares a dedicated app-server with `codex app-server daemon start`. Each gets a
new, short, temporary CODEX_HOME; the probe verifies the reported home before creating a thread.
It holds a streaming response, disconnects all protocol clients for 0.5 seconds, then releases
the response and reconnects to inspect the final turn status and continue the same thread.
Both modes completed the disconnected turn on Codex 0.167.2. This does not simulate killing a
future agentd. No `daemon bootstrap`, system service or user daemon is used. The private managed
daemon is stopped before its home is removed; failed cleanup fails the run.

`--claude` (select another CLI with `--claude-bin`) starts a loopback Messages JSON/SSE fixture,
runs a headless control turn, then dispatches a tool-free background session. Fake key and local
model URL are written into the disposable settings `env` because the supervisor may scrub
inherited variables. The private config pre-trusts only the generated workspace and fake key.
Native attach, prompt, Ctrl+Z detach, reattach and another prompt verify the same native session
and worker PID, including retained model-request context. Cleanup stops the specific job and
private supervisor and confirms the worker exits. No private daemon wire protocol is used.

Claude 2.1.323 **ignores `--session-id` with `--bg`**: it allocates a new native UUID. A1 therefore
remains `partial` against the original specified-UUID criterion, even though background/attach
works. Reports record requested UUID, native UUID and short job ID separately. A future Rust
adapter must bind its internal session ID to the returned native ID rather than assume equality.
The `--claude` path does not run A2–A5; use the optional Mods probe below.

Artifacts include `daemon-*-requests.json`, `daemon-stop.log`, `claude-baseline.jsonl`,
`claude-background.txt`, `claude-agents.json`, `claude-attach.bin`, `claude-reattach.bin`,
`claude-model-requests.json`, and Claude job/supervisor stop logs. Failure tracebacks are saved
as `daemon-error.log` or `claude-error.log` and listed in both report formats.

See the [Claude public CLI reference](https://code.claude.com/docs/en/cli-reference).

## Claude Mods, hooks, sending and cancellation

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/run.py \
  --claude-mods --output /tmp/xaya-p0-mods-run-1
node --test scripts/agent-probes/test_claude_mod.mjs
```

`--claude-mods` injects a temporary plugin and HTTP PermissionRequest hooks into a new background
worker. The module is loaded directly by Claude, so the actual probe does not require Node;
Node is only needed for the separate JavaScript unit tests. The bridge uses a fresh per-run token,
loopback HTTP and explicit command types. No plugin is installed into the user's configuration.
Mods requires literal `on()` event names and module-level helpers receiving `$`; normal JavaScript
syntax checks alone do not establish that the module will load.

On Claude 2.1.323, A2–A5 passed:

- A2: the module's native ID matches the job listing; exact streamed text reaches the bridge
  before the model finishes. Event delivery is ordered; loss counters remain visible on failure.
- A3: allow/deny decisions match actual file writes. The native TUI can also answer while an HTTP
  hook is pending. Turn completion expires that hook; late replies are rejected.
- A4: `prompt.submit` and native input share the same worker. Native Esc refuses later program
  input with `drop`, and no model request is made for the dropped prompt. A probe-only private
  daemon `reply` recovers the same worker and allows subsequent program input.
- A5: streaming abort is acknowledged. A delayed background Bash child is stopped with TaskStop
  using the actual task ID from its native result, not a PID guess; the child exits and its write
  scheduled 15 seconds after launch never occurs. The same session then continues.

The private reply endpoint is derived only from the disposable config directory, checked against
`claude daemon status` and its owner, then authenticated using that config's generated control key.
This is version-specific feasibility evidence, not a supported public control API. A product must
offer explicit recovery after native Esc rather than silently replaying input.

Raw evidence is saved in `mods-events.json`, `mods-acks.json`, `mods-permissions.json`,
`mods-model-requests.json`, `mods-model-observations.json`, `mods-tui.bin`, `mods-debug.log`, and
`mods-cleanup.txt`. Failed stages preserve earlier evidence and add `mods_error` plus its traceback.
Cleanup stops the private supervisor and verifies worker/tool processes exited.

## Read-only review, OSC boundaries and real repositories

```sh
/tmp/xaya-probes-venv/bin/python scripts/agent-probes/run.py \
  --dual --claude-mods --review --boundaries \
  --benchmark-repo /path/to/real/repo --output /tmp/xaya-p0-acceptance-run-1
```

`--review` runs both real headless CLIs against a fresh committed fixture. Codex uses its read-only
sandbox and never-approve policy. Claude uses the SDK stdio permission channel with a host policy
that allows only Read and StructuredOutput, denying every mutating tool request. Its available Write
tool is deliberately challenged, and the exact native permission denial, tool ID and target path must
match; generic tool errors never count. The fixture then supplies checkpoint-bound schema output.
Claude's actual `structured_output` is required, not JSON extracted from prose. This verifies tool
policy enforcement, not OS-level isolation or model review quality. Bare mode was not used for this
challenge because it removed Write entirely; user/project settings and MCP discovery are disabled
explicitly within the isolated HOME/config instead. No user account or production model is used.

`--boundaries` drives a normal Claude TUI, while `--dual` captures Codex TUI boundaries. The PTY
scanner answers complete OSC 7501 probes, including split sequences, and records raw reports and
monotonic receive times. Any working/blocked record keeps a turn busy; progress or permission changes
do not split it. Counts, ordering, closure and start/end times must match native main-turn events
within 1.5 seconds. Raw timings include transport/rendering latency, not just engine event generation.

Claude 2.1.323 bg workers deliberately disable Program Status. `--claude-mods` records that mode as
`unsupported`, using Mods as its required fallback. An E1 overall pass requires both normal TUI engines
to match and never treats the background mode as passed. No worker environment flag is overridden.

`--benchmark-repo` is repeatable. It creates a local shared-object clone at the specified committed
HEAD, then performs all checkout, changes, staged content and snapshot writes inside that clone.
Source HEAD/index hashes and clone staged index bytes are verified unchanged. Four two-snapshot
samples are saved, including first/median/max times. Two recorded repositories (about 5,600–5,900
tracked files, 90–170 MB) passed the one-second threshold on the test machine. Caches are not flushed; the
report is limited to the recorded repositories and machine, not arbitrarily large repositories.

Artifacts: `*-review-*`, `codex-osc.json`, `claude-native-osc.json`, `claude-osc.json`, raw PTYs and
`git-benchmarks.json`. Failures write explicit error fields/tracebacks and return nonzero; an incomplete
engine pair stays partial. Benchmark clones are retained for inspection; the original checkout is
never edited.
