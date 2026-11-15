# The stop hook

`atelier hook stop` is the `Stop` and `SubagentStop` hook for Claude Code and Codex. It is a nudge, not a guarantee: atelier's commands still refuse what matters.

## Registration

`atelier init` and `atelier rules` register it on both events in:

- `.claude/settings.json`: `{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "sh -c 'atelier hook stop || true'"}]}], "SubagentStop": [...]}}`
- `.codex/hooks.json`: the same, each handler with `"timeout": 30`.

`|| true` lets the agent stop when the pinned atelier is too old to have `hook`.

atelier never rewrites an existing hook file. A missing file is created. A file that already runs `atelier hook stop` on both events is left alone. Otherwise `init` and `rules` print the JSON entries to add, and every preflight reports `rules are stale: run atelier rules` (`<file>: hook missing on Stop and SubagentStop`) until you add them. Commit both files.

**Codex:** run `/hooks` in the project once and trust the entries, and again after they change. Until then Codex skips them silently. The hook has not yet run against a live Codex model.

## What it checks

Only in a worktree whose branch has an `active` record from `atelier start`. Anywhere else the agent may stop.

| Condition | `Stop` | `SubagentStop` |
|---|---|---|
| a rebase is open | blocks | — |
| uncommitted changes, untracked files included | blocks | — |
| a `background_tasks` entry is `running` (Claude Code only) | blocks | — |
| no line of the last message starts with `compile:` | blocks | blocks |

The message is `last_assistant_message`, else the last report in the transcript (`agent_transcript_path` for a subagent, `transcript_path` otherwise). If neither can be read, it is not checked.

A block prints `{"decision": "block", "reason": "…"}` and exits 0; the reason lists every blocking condition. Any error of the hook itself lets the agent stop.

## Repeats

Per session, the hook counts blocks in a row for each condition, in `.git/atelier/hooks/<session id>`. The count restarts each turn.

- After 3 blocks in a row for one condition, the next stop goes through (Claude Code itself stops honouring blocks after 8).
- A merge in progress and a submodule with its own changes let the agent stop at once: the advice cannot clear them.
- A stop let through with a condition left is warned on stderr and recorded for `status`.

## Steps on macOS

`atelier check` runs steps as launchd jobs, so a step is not a child of your terminal:

- **No terminal.** Anything that prompts on `/dev/tty` fails: ssh passphrases and host-key questions, terminal gpg pinentry, `sudo`. Use an ssh agent (`SSH_AUTH_SOCK` is passed on), known hosts set up beforehand, a GUI pinentry or unlocked gpg agent, and no `sudo`.
- **Limits.** Only the open-file limit and umask come from your shell. Others are launchd's (stack 8176 KB, no core files); `ulimit` in your shell does not reach the step.
- The environment is exactly atelier's, stdin is `/dev/null`, SIGPIPE is default.
- In a protected folder (`~/Documents`, `~/Desktop`, iCloud Drive) macOS may ask once whether atelier may access it.
