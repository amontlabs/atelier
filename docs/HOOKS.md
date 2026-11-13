# The stop hook

`atelier hook stop` (DESIGN §8.5, experiment P23) is the `Stop` and `SubagentStop` hook of Claude
Code and Codex. It is a strong nudge, not a guarantee: atelier's own commands still refuse what
matters.

## Where it is registered

`atelier init` and `atelier rules` add the hook to both events in:

- `.claude/settings.json`: `{"hooks": {"Stop": [{"hooks": [{"type": "command", "command":
  "atelier hook stop"}]}], "SubagentStop": [...]}}`;
- `.codex/hooks.json`: the same, each handler with `"timeout": 30`.

The hook is added to what the files hold: every other key, event and hook stays, in its order. A
file that is not a JSON object, or whose `hooks` or event entries have another shape, is refused
(4) and left alone. Every other command's preflight counts a file without the hook on both events
as stale rules (`rules are stale: run atelier rules`). Commit both files with the rules.

**Codex runs a project's hooks only after the owner trusts them.** Run `/hooks` in Codex in the
project once and trust the two entries; trust is tied to the hook's hash, so trust them again
after they change. An untrusted hook is skipped silently (P23), so until then Codex workers get
no stop check. `atelier init` prints this reminder.

## What it does

The hook reads the harness's JSON on stdin. It acts only when the input's `cwd` is in a worktree
on a branch with an `active` record (a branch `atelier start` made), `HEAD` checked out or being
rebased there. Anywhere else (the main checkout on the integration branch, another repository, a
merged branch) it lets the agent stop.

| Condition | `Stop` | `SubagentStop` |
|---|---|---|
| a rebase is open in the worktree | blocks | not checked |
| uncommitted changes (untracked files included) | blocks | not checked |
| a task in `background_tasks` has status `running` (a shell command or a subagent; Claude Code only, Codex sends no such field) | blocks | not checked |
| no line of the last message starts with `compile:` (leading blanks allowed) | blocks | blocks |

A subagent's input carries the parent's folder and background tasks (P23), so only its message
is checked. The message is `last_assistant_message`; when the input has none, the last report in
the transcript the input names (`agent_transcript_path` for a subagent, `transcript_path`
otherwise): the last assistant entry's text, or its `SubagentHandback` message. Claude Code
2.1.322 sends no `last_assistant_message` on `SubagentStop` when the subagent ends through
`SubagentHandback` (recorded 2026-11-13, `tests/fixtures/hooks/`). When neither can be read, the
message is not checked.

A block is `{"decision": "block", "reason": "…"}` on stdout with exit 0, the format both harnesses
document; the reason lists every condition found. The hook always exits 0: a failure of its own
(unreadable input, git failing) lets the agent stop, because both harnesses read exit 2 as a
block. Outside a project the dispatcher runs `hook` in place instead of refusing.

## Limits

- Claude Code honours at most 8 blocks in a row, then ends the turn (P23).
- The worker rules must not contradict the hook, or the agent refuses until the cap. `WORKER.md`
  says the same four things.
- Codex: not yet run against a model (P23, quota). The input fields are Codex's documented ones.
