# atelier

atelier keeps one project correct while several coding agents work on it at once, each in its
own git worktree. It records what each branch is built on and moves those branches when their
base moves. It binds "reviewed" to one exact commit. It merges through a gate that checks the
merged tree on every platform the project needs, and moves `main` only when that tree is green.
It deletes only what it made and what was merged. Agents get generated rules that cover only the
parts the project uses.

The design is kept outside this repository (`DESIGN.md`, revision 5, with mechanics in
`DESIGN-r4.md`). The formats every version shares are in [docs/FROZEN-LAYER.md](docs/FROZEN-LAYER.md).

## Status: modules 3.1 to 3.8 and 3.10

Built so far, with tests on throwaway git repositories:

- the command line with every command declared. Each command runs its preflight, then exits 2
  with `not built yet (module N)`. The exit codes are 0, 1, 2, 3, 4, 5 and 7, as in the design;
- the dispatcher and the per-project version pin;
- parsing of `.atelier/atelier.toml`, the owner's `machines.toml` and the owner's `agents.toml`;
- the shared state in `.git/atelier/`: a frozen layer, a versioned layer with a format number,
  and `state.lock`;
- locks, runs, process groups and the survivor scan, stop signals, the tree id, and plain refs;
- `start` and `follow` (module 3.3): branch records with lifetime ids, the base kept in the
  branch, `follow`'s pending rebase settled from ancestry alone, the transitive "current" guard,
  and optional Mergiraf resolution of conflicted code files in `follow`.

Module 3.7 adds `init`, `rules`, `task` and `agents`: the generated rules (`WORKER.md`,
`ORCHESTRATOR.md`, the `AGENTS.md` block, the `@AGENTS.md` import lines), verified byte for byte by
every command's preflight, and the task templates in `assets/tasks/`. `src/template.rs` is a
small Mustache subset (`{{x}}`, `{{#x}}…{{/x}}`) written here because the Mustache crates render
derived Rust structs and implement the whole specification, where these templates have run-time
fields and anything else must be an error.

`atelier check [--compile-only]` proves the tree in a worktree: tool prerequisites (Rust
toolchain, nextest pin, declared tools, nextest-file refusals (default filters, retries, flaky
passes, `on-timeout` other than `fail`, wrapper scripts; the files are read again at the end), and
with `[prerequisites.mbx]` the mbx version, settings, `.mbx.toml`, absolute `PATH` entries, `cargo`
on `PATH`, bypass variables and the build folder cargo really uses, read again at the end), the run lock, the tree id, `check.prepare` hooks, format, compile, nextest with
the fixed no-retry flags, `check.steps`, the survivor scan. Per-test results are kept in the run
folder (`tests.tsv`, nextest's libtest JSON in `tests-<workspace>.jsonl`).

Module 3.6, machines: the owner's machines file, `atelier machines`, `atelier machines init
<name>`, `atelier test <platform>|--all [-- <command>…]`, and remote runs over ssh (`ssh -tt`,
the hang-up watcher, the result read with a second ssh). See [docs/MACHINES.md](docs/MACHINES.md).

`atelier tmp [--short] -- <command>` (module 3.5) runs a command with a registered temp folder in
`ATELIER_TMP` for the branch checked out in the current worktree, holding the branch lock shared
until the command ends. `atelier clean [<branch>…] [--yes] [--discard <commit>…]` prints a plan,
or with `--yes` deletes what `merge` recorded as merged: its registered temp folders, the worktree
`approve` recorded, the branch, and its mbx build, in that order, each condition re-checked at the
moment of deletion (`src/clean.rs`). It refuses, changing nothing, when a mount, a nested
repository or a hidden local edit is in what it would delete, or when the branch's reflog holds
work only it remembers.
It reads the merged commit and the recorded worktree from the branch record's `merged` table
(`docs/FROZEN-LAYER.md`); module 3.4 writes it.

`approve`, `merge` and `promote` (module 3.4, [docs/MERGE.md](docs/MERGE.md)): approvals bound
to one commit and one worktree, the gate worktree `<main checkout>.gate`, `--no-ff` merges with
conflicts in regenerated paths settled by the after-merge hook, the check through the merged
commit's pinned atelier, mode A promotion (checks a–e, one local push in its own process group,
the wait and exit 7, interrupted promotions reported path by path), and red batch bisection.
Platforms of other machines run on machines chosen as `atelier test` chooses them, all at once.

`atelier status [--json]` (module 3.8, [docs/STATUS.md](docs/STATUS.md)) is the one read-only view:
the preflight lines, `main` and any pending or interrupted promotion, approvals not yet merged,
merges not yet cleaned, the last runs, each platform's last gate result on `main` (waived ones as waived), the
machines as `atelier machines` prints them, branches that must `follow`, worktrees with no merge
record, flaky tests, and the conflict forecast (`git merge-tree` for every pair of open branches
and each against `main`, touching no file). `merge` prints the forecast's pairs that involve the
batch as warnings and refuses nothing.

Module 3.10, launchd steps and the stop hook ([docs/HOOKS.md](docs/HOOKS.md) for the hook):

- On macOS every step of a run is a launchd job (`launchctl bootstrap` of a generated plist with a
  unique label, `ProcessType = Interactive`, `AbandonProcessGroup = true`; `gui/<uid>`, or
  `user/<uid>` with `LimitLoadToSessionType = Background` without a GUI login). atelier itself is
  the job's shim: it sets exactly the step's environment, restores the caller's open-file limit
  and umask, and runs the program only once the step is recorded under the lock. The job's
  resource coalition adds to the marker scan: what a step leaves behind (after `setsid`, a double
  fork, `env -i`, as a system program) and still alive 1 s after the step ends (`git commit`'s
  detached auto-maintenance usually ends sooner) is found by coalition, stopped (TERM,
  the grace, KILL, the coalition re-checked before each signal) and makes the run RED; `launchctl
  bootout` comes last; a member another user owns is reported at once ("cannot stop <pid> (owned
  by <user>)"). A nested atelier lists its jobs in every outer run's coalitions file
  (`ATELIER_COALITIONS`), whatever state it uses, and the end-of-run scan also runs the marker
  scan. Job folders live in the user's temporary folder with an owner file written before the
  bootstrap; jobs left loaded by an atelier that died are booted out when a run starts or ends.
  The exit status is read from `launchctl print`; a shim launchd cannot spawn is reported at
  once. Steps have no controlling terminal (docs/HOOKS.md). Before the first run on a
  macOS version, the preflight runs the `setsid` case once per user and refuses (4) if the
  coalition misses it; there is no fallback to the weaker scan. Linux keeps process groups and the
  marker scan. Not caught on either system: work handed to an already-running service (launchd
  agents, a daemon), as r4 §3.1 says.
- `atelier hook stop`, registered by `init` and `rules` in `.claude/settings.json` and
  `.codex/hooks.json` as `sh -c 'atelier hook stop || true'` (a missing file is created; an
  existing one is never rewritten: the entries to add are printed): Claude Code's and Codex's Stop and
  SubagentStop hook. In a worktree on a branch atelier started it blocks the end of a turn while
  a rebase is open, the tree has uncommitted changes, a background task the input lists runs, or
  the last message has no line starting `compile:` (for a subagent, the message only); after 3
  blocks in a row for one condition, and at once for a merge in progress or a changed submodule,
  it lets the agent stop with a warning. **Codex
  runs a project's hooks only after the owner trusts them once** (`/hooks` in Codex, in that
  project); atelier cannot grant that.

## Layout

| Module | What it does |
|---|---|
| `cli` | clap definitions; runs each command's preflight; stubs |
| `exit` | the exit codes and the `Failure` error type |
| `dispatch` | the dispatcher, the pin, the install command |
| `preflight` | per command: project, `state.lock`, version, settings, rules, versioned layer; `status` report |
| `settings`, `machines`, `agents`, `platform` | the three TOML files (unknown keys rejected) and platform names |
| `state` | `.git/atelier/` layers, `state.lock`, format number and conversion |
| `records` | frozen-layer branch records and temp-folder registry entries |
| `locks` | `flock` locks and their process records |
| `procs` | process groups, stopping a group, the `ATELIER_RUN` marker scan |
| `coalition`, `launchd` | macOS: coalition scan and stop, the probe and its record; steps as launchd jobs and the shim |
| `hook` | `atelier hook stop`, its per-session counts, and the hook files |
| `runs`, `signals` | run ids, folders, steps, atomic results; TERM/INT/HUP |
| `tree` | tree id via a temporary index; submodule refusal |
| `refs`, `names`, `git` | plain refs and `--no-deref` updates; branch names and file keys; running git |
| `stack` | branch record rules: settling `follow`, the base check, targets, "current", dependents |
| `start`, `follow` | the two commands |
| `tmp`, `clean` | the two commands (module 3.5); `clean` also holds the plan and every re-check |
| `rules` | rules currency check (stub until module 3.7) |
| `approve`, `gate`, `promote`, `bisect` | `approve`; `merge` (preconditions, gate worktree, merges, hooks, platforms); mode A promotion and `promote`; red batch bisection |
| `gate_machines` | the gate's platforms on the owner's machines: availability first, every platform at once, remote runs bound to the merged commit |
| `ledger` | the versioned layer's approvals, `merges.tsv` and promotion record |
| `status`, `forecast` | `status` (module 3.8); the conflict forecast `status` and `merge` print |
| `check`, `prereq`, `nextest` | the `check` command; tool and mbx prerequisites; nextest flags, file refusals and result parsing |
| `testcmd` | `test`, `machines`, `machines init` |
| `jobs` | a platform's job (preparation, command or run) and the machine preflight |
| `remote`, `remote_run` | this computer's side of a remote run over ssh, and the machine's side (`remote-run`) |
| `source` | the program's own source, embedded, for `machines init` to build on another machine |
| `exchange` | exchanging two folders in one step (the install swap), also compiled on its own as `machines init`'s probe |

## Dependencies

clap, serde, serde_json (cargo metadata, nextest output, hook input and the harnesses' hook files)
and toml handle the command line and the files; tempfile is used by tests only.
Locks use the standard library's `File::lock` (`flock`, stable since Rust 1.89), so fs2 and
rustix are not needed. Two more crates:

- `libc`: `kill` on process groups, `setsid` in tests, `proc_listallpids` and
  `proc_pidinfo(PROC_PIDCOALITIONINFO)` (the coalition scan), `kqueue` (a job's exit), and
  `sysctl(KERN_PROCARGS2)`, the source
  `ps -E` reads on macOS. With it, the survivor scan compares each environment entry exactly,
  where a command line that happens to contain the marker text would match `ps -E` output.
- `signal-hook`: installs the TERM, INT and HUP handlers safely. During a run the handler only
  records the signal; outside runs it runs the signal's default action.

## Install (once releases exist)

```sh
cargo install --git https://github.com/amontlabs/atelier --tag v<version> --locked --root ~/.local/share/atelier/<version>
```

The dispatcher is the same program, installed once on `PATH` (for example
`cargo install … --root ~/.local`, giving `~/.local/bin/atelier`). Started without
`ATELIER_DISPATCHED` in its environment, it reads the pin of the checkout it runs in and runs
`~/.local/share/atelier/<pin>/bin/atelier`. If that version is missing, it prints the install
command and exits 4.

## Development

Rust 1.100.0 (pinned in `rust-toolchain.toml`). `scripts/check.sh` runs the format check,
clippy and every test.
