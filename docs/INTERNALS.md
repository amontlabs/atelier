# Internals

What each part does, where it lives in `src/`, and what it depends on. The [README](../README.md) is the introduction; the other pages go deeper on one command each. Everything is tested on throwaway git repositories.

## The parts

- **Command line.** Every command runs its preflight first. Exit codes: 0, 1, 2, 3, 4, 5, 7.
- **Dispatcher** and the per-project version pin.
- **Settings.** Parsing of `.atelier/atelier.toml`, the owner's `machines.toml` and `agents.toml`.
- **Shared state** in `.git/atelier/`: a frozen layer ([FROZEN-LAYER.md](FROZEN-LAYER.md)), a versioned layer with a format number, and `state.lock`.
- **Plumbing.** Locks, runs, process groups and the survivor scan, stop signals, the tree id, plain refs.

### `start` and `follow`

Branch records with lifetime ids, the base kept in the branch, a pending `follow` rebase settled from ancestry alone, the transitive "current" guard, and optional Mergiraf resolution of conflicted code files.

### `init`, `rules`, `task` and `agents`

The generated rules (`WORKER.md`, `ORCHESTRATOR.md`, the `AGENTS.md` block, the `@AGENTS.md` import lines), which every command's preflight verifies byte for byte, and the task templates in `assets/tasks/`. `src/template.rs` is a small Mustache subset (`{{x}}`, `{{#x}}…{{/x}}`), written here because the Mustache crates render derived Rust structs and implement the whole specification, while these templates have run-time fields and anything else must be an error.

### `check`

`atelier check [--compile-only]` proves the tree in a worktree, in this order:
1. Tool prerequisites: Rust toolchain, nextest pin, declared tools. nextest-file refusals (default filters, retries, flaky passes, `on-timeout` other than `fail`, wrapper scripts; files are read again at the end). With `[prerequisites.mbx]`: the mbx version, settings, `.mbx.toml`, absolute `PATH` entries, `cargo` on `PATH`, bypass variables and the build folder cargo really uses (read again at the end).
2. The run lock and the tree id.
3. `check.prepare` hooks, format, compile.
4. nextest with the fixed no-retry flags, then `check.steps`.
5. The survivor scan.

Per-test results stay in the run folder (`tests.tsv`; nextest's libtest JSON in `tests-<workspace>.jsonl`).

### `machines` and `test`

The machines file, `atelier machines [init <name>]`, `atelier test <platform>|--all [-- <command>…]`, and remote runs over ssh (`ssh -tt`, the hang-up watcher, the result read with a second ssh). See [MACHINES.md](MACHINES.md).

### `tmp` and `clean`

`atelier tmp [--short] -- <command>` runs a command with a registered temp folder in `ATELIER_TMP`, for the branch checked out in the current worktree. It holds the branch lock shared until the command ends.

`atelier clean [<branch>…] [--yes] [--discard <commit>…]` prints a plan; `--yes` deletes what `merge` recorded as merged: the registered temp folders, the worktree `approve` recorded, the branch, and its mbx build, in that order, each condition re-checked at deletion (`src/clean.rs`). It refuses, changing nothing, when a mount, a nested repository or a hidden local edit is in what it would delete, or when the branch's reflog holds work only it remembers. It reads the merged commit and the recorded worktree from the branch record's `merged` table ([FROZEN-LAYER.md](FROZEN-LAYER.md)).

### `approve`, `merge` and `promote`

Approvals bound to one commit and one worktree; the gate worktree `<main checkout>.gate`; `--no-ff` merges, with conflicts in regenerated paths settled by the after-merge hook; the check through the merged commit's pinned atelier; mode A promotion (checks a–e, one local push in its own process group, the wait and exit 7, interrupted promotions reported path by path); red-batch bisection. Platforms of other machines run on machines chosen as `atelier test` chooses them, all at once. See [MERGE.md](MERGE.md).

### `status`

The one read-only view: see [STATUS.md](STATUS.md). `merge` prints the forecast's pairs that involve the batch as warnings and refuses nothing.

### launchd steps and the stop hook

- **macOS steps are launchd jobs.** Each is a `launchctl bootstrap` of a generated plist with a unique label, `ProcessType = Interactive`, `AbandonProcessGroup = true`; `gui/<uid>`, or `user/<uid>` with `LimitLoadToSessionType = Background` without a GUI login.
  - atelier is the job's shim: it sets exactly the step's environment, restores the caller's open-file limit and umask, and runs the program only once the step is recorded under the lock.
  - The job's resource coalition adds to the marker scan. What a step leaves behind (after `setsid`, a double fork, `env -i`, as a system program) and still alive 1 s after the step ends (`git commit`'s detached auto-maintenance usually ends sooner) is found by coalition, stopped (TERM, the grace, KILL, the coalition re-checked before each signal) and makes the run RED. `launchctl bootout` comes last. A member another user owns is reported at once ("cannot stop <pid> (owned by <user>)").
  - A nested atelier lists its jobs in every outer run's coalitions file (`ATELIER_COALITIONS`), whatever state it uses. The end-of-run scan also runs the marker scan.
  - Job folders live in the user's temporary folder with an owner file written before the bootstrap. Jobs left loaded by an atelier that died are booted out when a run starts or ends.
  - The exit status is read from `launchctl print`; a shim launchd cannot spawn is reported at once. Steps have no controlling terminal ([HOOKS.md](HOOKS.md)).
  - Before the first run on a macOS version, the preflight runs the `setsid` case once per user and refuses (4) if the coalition misses it. There is no fallback to the weaker scan.
  - Linux keeps process groups and the marker scan. On both systems, work handed to an already-running service (launchd agents, a daemon) is not caught, as the lock rules say.
- **`atelier hook stop`**, registered by `init` and `rules`. See [HOOKS.md](HOOKS.md). Codex runs a project's hooks only after the owner trusts them once (`/hooks`); atelier cannot grant that.

## Layout

| Module | What it does |
|---|---|
| `cli` | clap definitions; runs each command's preflight |
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
| `tmp`, `clean` | the two commands; `clean` also holds the plan and every re-check |
| `rules` | the generated rules: writing them, and the byte-for-byte check every preflight runs |
| `task`, `template` | `init`, `task` and `agents`; the Mustache subset the task templates use |
| `approve`, `gate`, `promote`, `bisect` | `approve`; `merge` (preconditions, gate worktree, merges, hooks, platforms); mode A promotion and `promote`; red batch bisection |
| `gate_machines` | the gate's platforms on the owner's machines: availability first, every platform at once, remote runs bound to the merged commit |
| `ledger` | the versioned layer's approvals, `merges.tsv` and promotion record |
| `status`, `forecast` | `status`; the conflict forecast `status` and `merge` print |
| `check`, `prereq`, `nextest` | the `check` command; tool and mbx prerequisites; nextest flags, file refusals and result parsing |
| `testcmd` | `test`, `machines`, `machines init` |
| `jobs` | a platform's job (preparation, command or run) and the machine preflight |
| `remote`, `remote_run` | this computer's side of a remote run over ssh, and the machine's side (`remote-run`) |
| `source` | the program's own source, embedded, for `machines init` to build on another machine |
| `exchange` | exchanging two folders in one step (the install swap), also compiled on its own as `machines init`'s probe |

## Dependencies

clap, serde, serde_json (cargo metadata, nextest output, hook input and hook files) and toml handle the command line and the files; tempfile is for tests only. Locks use the standard library's `File::lock` (`flock`, stable since Rust 1.89), so fs2 and rustix are not needed. Two more crates:

- `libc`: `kill` on process groups, `setsid` in tests, `proc_listallpids` and
  `proc_pidinfo(PROC_PIDCOALITIONINFO)` (the coalition scan), `kqueue` (a job's exit), and `sysctl(KERN_PROCARGS2)`, the source `ps -E` reads on macOS. With it the survivor scan compares each environment entry exactly; `ps -E` output would also match a command line that merely contains the marker text.
- `signal-hook`: installs the TERM, INT and HUP handlers safely. During a run the handler only
  records the signal; outside runs it runs the signal's default action.

## Development

Rust 1.100.0 (pinned in `rust-toolchain.toml`). `scripts/check.sh` runs the format check, clippy and every test.
