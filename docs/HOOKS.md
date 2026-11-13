# The stop hook

`atelier hook stop` is the `Stop` and `SubagentStop` hook of Claude
Code and Codex. It is a strong nudge, not a guarantee: atelier's own commands still refuse what
matters.

## Where it is registered

`atelier init` and `atelier rules` register the hook on both events in:

- `.claude/settings.json`: `{"hooks": {"Stop": [{"hooks": [{"type": "command", "command":
  "sh -c 'atelier hook stop || true'"}]}], "SubagentStop": [...]}}`;
- `.codex/hooks.json`: the same, each handler with `"timeout": 30`.

The command is `sh -c 'atelier hook stop || true'`: inside a project the dispatcher runs the
pinned atelier, and a pinned version older than the hook has no `hook` command (a usage error,
exit 2, which both harnesses read as a block). With `|| true` it lets the agent stop instead.

atelier never rewrites an existing hook file:

- a missing file is created with the two entries;
- a file that already runs `atelier hook stop` on both events (with any path before `atelier`,
  alone or inside a shell command, with or without a UTF-8 BOM) is left alone;
- a file without the hook on an event, or that is not a JSON object with the shapes the harnesses
  read, is left as it is too: `init` and `rules` print the exact JSON entries to add into its
  `hooks`, and every other command's preflight reports the rules as stale (`rules are stale: run
  atelier rules`, with `<file>: hook missing on Stop and SubagentStop`) until the owner adds them.

Commit both files with the rules.

**Codex runs a project's hooks only after the owner trusts them.** Run `/hooks` in Codex in the
project once and trust the two entries; trust is tied to the hook's hash, so trust them again
after they change. An untrusted hook is skipped silently, so until then Codex workers get
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

A subagent's input carries the parent's folder and background tasks, so only its message
is checked. The message is `last_assistant_message`; when the input has none, the last report in
the transcript the input names (`agent_transcript_path` for a subagent, `transcript_path`
otherwise): the last assistant entry's text, or its `SubagentHandback` message. Claude Code
2.1.322 sends no `last_assistant_message` on `SubagentStop` when the subagent ends through
`SubagentHandback` (recorded 2026-11-13, `tests/fixtures/hooks/`). When neither can be read, the
message is not checked.

A block is `{"decision": "block", "reason": "…"}` on stdout with exit 0, the format both harnesses
document; the reason lists every condition that blocks. The hook always exits 0: a failure of its
own (unreadable input, git failing) lets the agent stop, because both harnesses read exit 2 as a
block. Outside a project the dispatcher runs `hook` in place instead of refusing.

## Repeats

A condition the agent cannot clear would block until the harness gives up (Claude Code after 8
blocks, as observed; Codex documents no cap). So the hook counts, per session, the blocks in a row for
each condition, in `.git/atelier/hooks/<session id>` (`docs/FROZEN-LAYER.md`):

- the count starts again at each turn (`stop_hook_active` false), and for a condition that was
  gone at the last stop;
- after 3 blocks in a row for the same condition, the next stop goes through;
- these conditions let the agent stop at once, because the advice cannot clear them: a merge in
  progress (`git commit -- <paths>` is refused during a merge; the changes of the merge are not
  reported as uncommitted), and a submodule with changes of its own (no commit in the worktree
  cleans it; its other changes still count).

When a stop goes through with a condition left, the hook prints a warning naming it on stderr
and records it (time, branch, condition, why: `repeated` or `unfixable`) in the session file, for
`status`. Another condition that the advice can clear still blocks meanwhile. An input without a
`session_id` is not counted.

## Limits

- Claude Code honours at most 8 blocks in a row, then ends the turn; the hook gives up after
  3 (above).
- The worker rules must not contradict the hook, or the agent refuses until the cap. `WORKER.md`
  says the same four things.
- Codex: not yet run against a Codex model. The input fields are Codex's documented ones.

## Steps on macOS

The checks the hook asks for (`atelier check`) run their steps as launchd jobs. A
step there is not the child of the terminal atelier runs in:

- **It has no controlling terminal.** Opening `/dev/tty` fails ("Device not configured"), so
  anything that prompts there fails instead of asking: an ssh passphrase or host-key question,
  `gpg`'s pinentry-tty or curses pinentry for signed commits (`commit.gpgsign`), `sudo`. Give
  steps non-interactive credentials: an ssh agent (`SSH_AUTH_SOCK` is passed on) and known
  hosts set up beforehand, a GUI pinentry or an unlocked agent for gpg, no `sudo`.
- **Only the open-file limit is restored** (`RLIMIT_NOFILE`, with the umask). The other limits
  are launchd's; they equal a login shell's (stack 8176 KB, no core files), so a limit changed in
  the calling shell (`ulimit -s unlimited`, `ulimit -c unlimited`) does not reach the step.
- The environment is exactly atelier's (with the run's variables), stdin is `/dev/null`, and
  SIGPIPE is at its default, as before.
- atelier's own files for the job (spec, FIFOs, plist, launchd's log) are in the user's temporary
  folder (`getconf DARWIN_USER_TEMP_DIR`), not in the project or the state, so a project under a
  folder macOS protects (`~/Documents`, `~/Desktop`, iCloud Drive) does not keep the job from
  starting. The step's own program still reads the project and writes its log in the state: a
  launchd job is its own responsible process for macOS's privacy checks, so in such a folder macOS
  may ask once whether atelier may access it. `tests/launchd.rs` has a test for a run under
  `~/Documents` that is not run by default (each new build may ask again, and no one can answer
  in a headless run): run it at the Mac with `cargo test --test launchd -- --ignored documents`.
