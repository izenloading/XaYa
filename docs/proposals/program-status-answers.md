# Extension proposal: answering waiting programs from the terminal (OSC 7501)

*English | [简体中文](program-status-answers.zh-CN.md)*

Status: draft, for the maintainers of the [Program Status Protocol][spec] and of
Claude Code and Codex. It extends revision 0.4 of the spec; whether and how it
becomes part of a revision is the spec maintainers' call. Nothing here is
implemented yet; XaYa implements revision 0.4 (status reports only).

[spec]: https://www.superlogical.com/rex/docs/build/program-status

## Problem

With the Program Status Protocol, a terminal knows when an agent is blocked on
the user, and why (`kind=permission|question|auth`, with the question as `msg`).
XaYa uses that for title badges, the Dock count, desktop notifications and an
agent panel listing every window.

But answering still means finding the window, focusing it and pressing the key
the agent's UI expects. With several agents running, most interruptions are
routine approvals the user could give from the notification or the panel,
without switching context.

Terminals can't do this today without guessing the agent's UI: typing `y` or
pressing Enter into a full-screen TUI depends on its current layout and focus,
and a wrong guess approves the wrong thing. Approval is a security boundary,
so it needs an explicit protocol, not keystroke emulation.

## Overview

1. The terminal announces it can send answers.
2. A program that's blocked may offer **choices**, under a **request id**.
3. When the user picks one in the terminal's UI, the terminal writes an
   **answer** to the program's input.
4. The program applies it as if the user had pressed the matching key in its
   own UI, or ignores it, and reports its new state as usual.

Programs and terminals that don't implement this keep working as in revision 0.4.

## Relation to the current security model

Revision 0.4 promises that the only bytes a terminal writes back are the
feature detection reply, which carries nothing from any report, and that ids,
titles and messages are never sent back. That keeps a program from using the
terminal to learn anything, and keeps the terminal from acting on the
program's behalf.

This extension deliberately adds a second thing a terminal writes back, so it
has to keep both properties:

- **Nothing from a report flows back except what the program chose to hand
  out for it.** An answer contains only the request id and a choice id the
  program itself offered, from a fixed vocabulary. Titles, messages and labels
  are never echoed, and no other record can be read.
- **The terminal never answers on its own.** Answers follow a direct user
  action in the terminal's UI; the program can't make the terminal answer, and
  other programs can't either unless the user turned that on.
- **Answers can only do what a key press in the program could.** The program
  validates the request id against the prompt it is showing and applies the
  choice exactly as its own UI would, so a stale, replayed or forged answer is
  inert.
- **It's opt-in on both sides.** Without `answers=1` from the terminal and
  `req`/`choices` from the program, nothing changes: the 0.4 guarantee holds
  for every session that doesn't use this.

## Wire format

The grammar is rev 0.4's: `OSC 7501 ; key=value:key=value… ST`, values limited
to `[A-Za-z0-9_.,+/=-]`, free text base64 encoded.

### Support

The terminal's reply to `OSC 7501 ; ?` gains an `answers` capability:

```
OSC 7501 ; ? : states=idle,working,done,blocked,error : kinds=permission,question,auth : answers=1 ST
```

A program only offers choices to a terminal that announced `answers=1`.

### Offering choices

A `blocked` report may carry:

| key       | value                                                                |
|-----------|----------------------------------------------------------------------|
| `req`     | Request id, `[A-Za-z0-9_.-]{1,32}`, unique per blocked prompt        |
| `choices` | Comma-separated choice ids, at most 9                                |
| `labels`  | Optional; base64 of the choices' labels, newline separated, in order |

Choice ids are `allow`, `deny` and `always` (allow and remember) for
`kind=permission`, and `opt1` … `opt9` for `kind=question`, in the order the
program shows them. Free text answers are out of scope: those need the
program's own input.

```
OSC 7501 ; state=blocked : app=claude-code : kind=permission : req=p42 :
  choices=allow,always,deny : labels=<base64 "Yes\nYes, don't ask again\nNo"> :
  msg=<base64 "Run: cargo test --workspace"> ST
```

### Answering

The terminal writes to the program's input, as it writes replies to other
queries:

```
OSC 7501 ; answer=p42 : choice=allow ST
```

The program reads it from its input stream, like the DA1 or OSC 11 replies it
already parses.

## Rules

For programs:

- Accept an answer only if `answer` names the request currently shown and
  `choice` was offered for it. Discard everything else silently: stale,
  replayed or forged answers do nothing.
- Apply an answer exactly as the corresponding key in the program's own UI,
  with the same consequences, and nothing more.
- After applying it, report the new state (`working`, or `blocked` with a new
  `req`). Terminals rely on that report, not on having sent the answer.
- A program may decline to offer choices for prompts it considers too
  sensitive, such as `kind=auth`.

For terminals:

- Send an answer only on a direct user action in the terminal's own UI: a
  notification button, a key in an agent panel. Answers from other programs,
  for example over the terminal's IPC, must be off by default.
- Show the program's `msg` and `labels` verbatim, with bidi controls removed
  (as rev 0.4 already requires), next to the choice the user picks.
- Never send an answer for a request the program no longer shows: after any
  later report from the program, the offered choices are gone.

## What XaYa would do

- Desktop notifications for `kind=permission` get the offered choices as
  buttons (macOS notification actions).
- The agent panel shows the choices of the selected window; their number keys
  answer, after the panel shows what will be answered.
- `xaya msg answer -w <window> <choice>`, off unless configured, like
  `msg send-text`.

## Open questions

- Should a program be able to require that answers come only while its window
  is visible to the user?
- Is `always` worth having in the shared vocabulary, or should programs keep
  persistent approvals to their own UI?
- Should the program acknowledge an answer explicitly (`answered=p42`) rather
  than through its next report?
