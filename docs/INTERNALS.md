# Internals

How the commands work and where they live in `src/`. Every command runs its preflight first (project, `state.lock`, version, settings, rules, versioned layer). Tests run on throwaway git repositories.

## Commands

- **`start`, `follow`.** Branch records with lifetime ids; the base kept in the branch; a pending `follow` rebase settled from ancestry; the transitive "current" check; optional Mergiraf for conflicted code files.
- **`init`, `rules`, `task`, `agents`.** Generate `WORKER.md`, `ORCHESTRATOR.md`, the `AGENTS.md` block and the `@AGENTS.md` import lines; every preflight checks them byte for byte. Task templates are in `assets/tasks/`; `src/template.rs` is a small Mustache subset (`{{x}}`, `{{#x}}…{{/x}}`) where any unknown name is an error.
- **`check [--compile-only]`**, in order:
  1. Prerequisites: Rust toolchain, nextest pin, declared tools; refusal of nextest settings that hide failures (default filters, retries, flaky passes, `on-timeout` other than `fail`, wrapper scripts); with `[prerequisites.mbx]`, mbx's version, settings and build folder. Files are read again at the end.
  2. The run lock and the tree id.
  3. `check.prepare` hooks, format, compile (`--compile-only` stops here).
  4. nextest with no retries, then `check.steps`.
  5. The survivor scan.

  Per-test results stay in the run folder: `tests.tsv`, and nextest's JSON in `tests-<workspace>.jsonl`.
- **`machines`, `test`.** See [MACHINES.md](MACHINES.md).
- **`tmp`.** Runs a command with a registered temp folder in `ATELIER_TMP`, holding the branch lock shared until it ends.
- **`clean`.** Prints a plan; `--yes` deletes, in order, the registered temp folders, the worktree `approve` recorded, the branch, and its mbx build, re-checking each at deletion. It refuses, changing nothing, when a mount, a nested repository or a hidden local edit is inside, or when the branch's reflog holds work nothing else has.
- **`approve`, `merge`, `promote`.** See [MERGE.md](MERGE.md).
- **`status`.** See [STATUS.md](STATUS.md).
- **`hook stop`.** See [HOOKS.md](HOOKS.md).

## Steps and survivors

Each step runs in its own process group, marked with `ATELIER_RUN` and `ATELIER_RUNS`. After a step, anything still alive with the run's marker is a survivor: the run is RED and survivors are named. On macOS each step is also a launchd job with its own resource coalition, which catches processes that left the group (`setsid`, double fork, `env -i`); those are stopped (TERM, grace, KILL). Before the first run on a macOS version, a probe checks the coalition works and refuses (4) if not. Work handed to an already-running service (a launchd agent, a daemon) is not caught.

## Layout

| Module | What it does |
|---|---|
| `cli`, `exit` | clap definitions and preflight; exit codes and `Failure` |
| `dispatch` | the dispatcher, the pin, the install command |
| `preflight` | per-command checks; the `status` preflight report |
| `settings`, `machines`, `agents`, `platform` | the three TOML files and platform names |
| `state`, `records`, `ledger` | `.git/atelier/` layers and conversion; branch records and temp entries; approvals, `merges.tsv`, promotion |
| `locks`, `procs`, `signals`, `runs` | `flock` locks and their records; process groups and the marker scan; TERM/INT/HUP; runs and results |
| `coalition`, `launchd` | macOS coalition scan and probe; steps as launchd jobs |
| `tree`, `refs`, `names`, `git` | tree ids; plain refs; branch names and keys; running git |
| `stack`, `start`, `follow` | branch record rules; the two commands |
| `tmp`, `clean` | the two commands |
| `rules`, `task`, `template` | generated rules; `init`, `task`, `agents`; the template engine |
| `approve`, `gate`, `gate_machines`, `promote`, `bisect` | `approve`; `merge`; its platforms on machines; promotion; red-batch bisection |
| `status`, `forecast` | `status`; the conflict forecast |
| `check`, `prereq`, `nextest` | `check`; prerequisites; nextest flags, refusals and results |
| `testcmd`, `jobs` | `test`, `machines`, `machines init`; a platform's job and machine preflight |
| `remote`, `remote_run` | both sides of a remote run over ssh |
| `source`, `exchange` | the embedded source for `machines init`; atomic folder exchange |
| `hook` | `atelier hook stop` and the hook files |

## Dependencies

clap, serde, serde_json and toml for the command line and files; `libc` for process groups, the macOS coalition and process-environment scans, and `kqueue`; `signal-hook` for signal handlers. Locks use the standard library's `File::lock`. tempfile is for tests only.

## Development

Rust 1.100.0, pinned in `rust-toolchain.toml`. `scripts/check.sh` runs the format check, clippy and every test.
