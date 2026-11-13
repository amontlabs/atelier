# atelier

**Several coding agents build one project in parallel: no waiting on CI, no waiting on each other's merges.**

atelier is a command-line tool the agents run themselves: an *orchestrator* that plans, reviews
and merges, and *workers* that each build one branch in its own git worktree. It answers four
problems, measured on the multi-agent builds that came before it:

| The problem | What atelier does |
|---|---|
| **Agents waited on CI.** A third to a half of all build time was agents polling CI until green (31% in one analysis, 49% in another). One Linux CI job failed 52 of 166 runs, at 16 minutes each. | **No CI in the loop.** `merge` checks the merged tree on your own machines, on every platform the project lists: this computer, and others over ssh, with the next machine tried when one is offline. `main` moves only when it is green. Nothing waits on GitHub. |
| **Work ran one item at a time.** Builds averaged 1.4 workers at once, because each item waited for the previous one to merge. | **Parallel by default.** One worktree per item. An item that builds on another starts before that one is merged (`start --on`, `follow`). Approved branches merge as one batch; a red batch is split, and the branches before the one that breaks it land. `status` forecasts conflicts between open branches. |
| **Test loops were slow and not trusted.** Full suites run by reflex, retries that hid failures, flaky tests nobody fixed. | **Fast feedback you can trust.** A worker runs only the tests that prove its change, then `check --compile-only`. The full check runs once per merge. Nothing is retried: a test that fails, then passes on the same tree, is listed as flaky, to be fixed. |
| **Every agent followed its own idea of the rules.** Notes in several places, models picked ad hoc. | **One set of rules.** `init` and `rules` generate what agents read. `task` renders the same task templates every time. Agents are chosen by role, never by model name. |

A green `main` is the guard rail that lets agents merge with nobody watching: `merge` moves it
only to a tree that passed, and only for the exact commits that were reviewed (`approve`).
Nothing is best-effort: a platform with no machine is not passed, never silently skipped, and no
verdict comes from a cache; the full check runs on the exact tree that lands.

> **Status: early (0.1.0).** Used on one real project, Silo. The speed-up is not measured yet: a
> measured trial build is next. Expect rough edges and changes. macOS and Linux only.

## Who it's for

You run several coding agents (Claude Code, Codex) on one repository at the same time and want
their work to land on `main` quickly, without CI in the way and without breaking it. The
orchestrator can be an agent or you. Rust projects get format, compile and
[nextest](https://nexte.st/) built in; anything else runs as steps you declare.

## A build, end to end

`start` → the worker commits → `check --compile-only` → `approve` → `merge` → `clean`.
The output below is real (paths shortened), from a small Rust project in `/Users/you/shop`.

```console
# Orchestrator: one branch and one worktree per item, then a task file for the worker
$ atelier start cart --path ../shop-cart
start: cart on main at ff7cf4c
worktree: /Users/you/shop-cart

$ atelier task build cart goal="Add a cart with a total" files="src/cart.rs, src/lib.rs" done="cart total is tested" risks="none"
Read /Users/you/shop/.git/atelier/tasks/20261113T185543Z-ca5c7e2c611d79fa.md: it is your task.

# Worker, in ../shop-cart: commits, runs the tests that prove its change, then
$ atelier check --compile-only
  fmt: ok
  compile: ok
compile: OK — run 20261113T1855Z-10d9ab5227b33d4d, commit dd9a596 (clean), 1s, log /Users/you/shop/.git/atelier/runs/20261113T1855Z-10d9ab5227b33d4d

# Orchestrator: reviews dd9a596, approves exactly that commit, merges through the gate
$ atelier approve cart dd9a596
approved cart dd9a596 (worktree /Users/you/shop-cart)

$ atelier merge cart
merge: run 20261113T1855Z-462b4b3843b5b863, main ff7cf4c in /Users/you/shop.gate
  in the gate: cart dd9a596
  macos-aarch64: PASS on this computer (check run 20261113T1855Z-c429d33afbf6c82f)
merge: GREEN — run 20261113T1855Z-462b4b3843b5b863, main ff7cf4c → 0ba07f4, macos-aarch64 PASS on this computer, 4s
merged cart dd9a596

$ atelier clean
cart
  would remove worktree /Users/you/shop-cart, with its ignored files: target/
  would delete branch cart at dd9a59615083
  would mark cart cleaned
clean: this is the plan; nothing was changed. Run again with --yes to do it

$ atelier clean --yes
cart
  removed worktree /Users/you/shop-cart
  deleted branch cart
  cart is cleaned
```

A branch is merged only if `merged <branch> <commit>` was printed. When something is in the way,
atelier stops, leaves `main` where it was and says what to do:

```console
# A worker on another branch, after main moved
$ atelier follow
atelier: follow: conflict rebasing search onto 0ba07f4:
  src/lib.rs
finish the rebase (git rebase --continue or --abort), then run atelier follow again

# Not reviewed yet
$ atelier merge search
atelier: search is not approved: atelier approve search <the reviewed commit>

# Approved, but a test fails on the merged tree
$ atelier merge search
merge: run 20261113T1856Z-9af89f002dc2bc63, main 0ba07f4 in /Users/you/shop.gate
  in the gate: search c2955d6
  macos-aarch64: FAILED (check run 20261113T1856Z-5a5189101fc75cdc: tests)
merge: RED — run 20261113T1856Z-9af89f002dc2bc63, failed: macos-aarch64 (check run 20261113T1856Z-5a5189101fc75cdc: tests); main has not moved; the gate worktree /Users/you/shop.gate is kept for inspection, 4s; log /Users/you/shop/.git/atelier/runs/20261113T1856Z-9af89f002dc2bc63
```

`atelier status` shows where everything stands and changes nothing. Three of its sections:

```console
$ atelier status
…
== main
main at 0ba07f4 Merge branch 'cart'
main checkout: on main; no tracked changes
promotion: none
…
== must follow
search: a follow is in progress in /Users/you/shop-search
search has a rebase open: finish the rebase: git rebase --continue or --abort
…
== conflict forecast
search x main: src/lib.rs
```

## Commands

| Command | Run by | What it does |
|---|---|---|
| `atelier init --version <v>` | owner | Once per project: writes `.atelier/`, the generated rules and the stop hook |
| `atelier rules` | owner | Rewrites the generated rules after you edit the settings or `.atelier/project/` |
| `atelier status [--json]` | anyone | Read-only: preflight, `main`, approvals, runs, platforms, branches that must `follow`, flaky tests, conflict forecast |
| `atelier agents` | orchestrator | Prints your agent roles from `~/.config/atelier/agents.toml` |
| `atelier start <branch> [--on <lower>] [--path <worktree>]` | orchestrator | Creates a branch (and its worktree) and records what it is built on |
| `atelier task <role> <branch>\|- [field=value…] [--fixes]` | orchestrator | Renders a task template into a file and prints the one-line prompt for the agent |
| `atelier follow` | worker | Rebases this branch's own commits onto what it is built on |
| `atelier check [--compile-only]` | worker, gate | Proves the tree: prerequisites, format, compile, tests with no retry, your steps. `--compile-only` stops before the tests |
| `atelier test <platform>\|--all [-- <command>…]` | worker | Runs a platform's check, or one command, on a machine of that platform |
| `atelier tmp [--short] -- <command>…` | worker | Runs a command with a registered temp folder in `$ATELIER_TMP`, which `clean` removes later |
| `atelier approve <branch> <commit>` | orchestrator | Records that this exact commit was reviewed and may be merged |
| `atelier merge <branch>… [--waive <platform> --reason "<words>"]` | orchestrator | Merges the approved commits in the gate, runs every platform on the merged tree, moves `main` if green |
| `atelier promote [--discard]` | orchestrator | Finishes (or drops) a green merge that could not move `main` because the main checkout was busy |
| `atelier clean [<branch>…] [--yes] [--discard <commit>…]` | orchestrator | Prints what it would delete of merged branches; `--yes` does it |
| `atelier machines [init <name> [--yes]]` | owner | Lists your machines; `init` sets one up for this project over ssh |
| `atelier hook stop` | the harness | Claude Code's and Codex's stop hook: sends a worker back while its work is unfinished |

Exit codes, the same for every command:

| | |
|---|---|
| `0` | Done; GREEN / PASS |
| `1` | RED / FAIL |
| `2` | Wrong usage |
| `3` | No machine available for a platform; nothing passed |
| `4` | Refused, nothing changed; the message names what is not in place |
| `5` | Merge conflict; the files are listed |
| `7` | GREEN, but `main` was not moved because the main checkout was busy: `atelier promote` finishes it |

## Install

From source; you need git and Rust 1.100.0 (with rustup, which installs it on first use).

```sh
# 1. The version your project pins. Versions sit side by side.
cargo +1.100.0 install --git https://github.com/amontlabs/atelier --tag v0.1.0 --locked --root ~/.local/share/atelier/0.1.0

# 2. The dispatcher, once: the same program, on PATH.
mkdir -p ~/.local/bin && cp ~/.local/share/atelier/0.1.0/bin/atelier ~/.local/bin/atelier
```

`~/.local/bin` must be on `PATH`, for you and for your agents. `cargo install` does not read the
repository's `rust-toolchain.toml`, so the command names the pinned Rust (`+1.100.0`) itself.

**The pin.** `atelier` on `PATH` reads `atelier = "<version>"` from `.atelier/atelier.toml` of the
checkout it runs in and runs `~/.local/share/atelier/<version>/bin/atelier`. Each worktree runs
the version its own tree pins. If that version is missing, it prints the install command and
exits 4; nothing installs itself.

The command atelier prints for a missing version is this one with that version.

## Set up a project

```console
$ cd shop && atelier init --version 0.1.0
initialized atelier 0.1.0 in /Users/you/shop
  wrote .atelier/rules/ORCHESTRATOR.md
  wrote .atelier/rules/WORKER.md
  wrote AGENTS.md
  wrote CLAUDE.md
  wrote .claude/settings.json
  wrote .codex/hooks.json
Codex runs the project's hooks (.codex/hooks.json) only once you trust them: run /hooks in Codex in this project once.
```

It writes the minimal `.atelier/atelier.toml` (comments added here):

```toml
atelier = "0.1.0"                 # the pin
integration_branch = "main"

[platforms.macos-aarch64]         # what must pass before main moves
run = "check"                     # the full `atelier check`
```

On its own that runs no test: say what your check is. A Rust project with one extra step:

```toml
atelier = "0.1.0"
integration_branch = "main"
workspaces = ["."]                # folders holding a Cargo workspace: fmt, check, nextest

[rust]
nextest = "0.9.151"               # the exact cargo-nextest version every run must use

[[check.steps]]                   # any command; a project without Rust has only these
name = "docs"
run = ["cargo", "doc", "--no-deps", "--locked"]

[platforms.macos-aarch64]
run = "check"
```

A workspace needs a `rust-toolchain.toml` with an exact `channel`. After any edit, run
`atelier rules` and commit `.atelier/`, `AGENTS.md`, `CLAUDE.md`, `.claude/settings.json` and
`.codex/hooks.json`. Unknown keys are refused. The other keys (tool prerequisites,
`check.prepare`, `merge.after`, more platforms) are in a full annotated example:
[tests/fixtures/silo-atelier.toml](tests/fixtures/silo-atelier.toml).

What agents then read is generated, never hand-edited: the block in `AGENTS.md`, and
`.atelier/rules/WORKER.md` and `ORCHESTRATOR.md` (sources in [assets/rules/](assets/rules/)).
Your own rules go in `.atelier/project/{binding,worker,orchestrator}.md` and are appended. The
stop hook sends a worker back while a rebase is open, something is uncommitted, a background
task runs, or its report has no `compile:` line.

**Two files of your own**, never committed, shared by all your projects:

```toml
# ~/.config/atelier/agents.toml: your roles, for `atelier agents` and `atelier task`
[roles.build]
for = "an ordinary item or fix"
start = "how your harness starts this agent"
template = "build"                # also needed: roles.build-hard and roles.review
```

```toml
# ~/.config/atelier/machines.toml: where each platform runs, for `atelier test` and `merge`
[machines.mac]
local = true
platform = "macos-aarch64"
gui = true
priority = 100                    # lower is tried first

[machines.linux-box]
ssh = "linux-box.local"
platform = "linux-x86_64"
priority = 10
```

Without a machines file, `merge` still runs this computer's own platform. With a second
platform in the settings (`[platforms.linux-x86_64]`), `atelier machines init linux-box` sets the
machine up, `atelier test linux-x86_64 -- <command>` runs one command there, and `merge` runs
every platform at the same time. No machine means not passed (exit 3), never a silent skip.

## What it deliberately doesn't do

- **No task board.** The item is the branch; `approve` and `merge` write the only records.
- **It doesn't run agents or pick models.** `agents` prints your roles, `task` writes a prompt
  file; your harness starts the agent.
- **No GitHub, no pull requests, no CI.** Everything happens on your machines. `main` moves by a
  local push, and another machine gets the commit over ssh.
- **No retries.** A failing test is red.
- **No conflict resolution in the gate.** A conflict stops the merge (exit 5) with the files
  listed. The one exception is files you declare as regenerated by a `merge.after` hook.
- **It never deletes or overwrites what it did not make.** Unknown files, unmerged work and open
  edits are listed and the command refuses (exit 4).
- **It installs nothing by itself**, neither its own versions nor your tools. Prerequisites are
  checked, never set.
- **It is not a sandbox.** Agents' code runs as your user, here and on your machines. The stop
  hook is a strong nudge, not a guarantee, and has not yet been run against a live Codex model.
- **No Windows.**

## Docs

| Page | What is in it |
|---|---|
| [docs/MERGE.md](docs/MERGE.md) | `approve`, `merge`, `promote`: the gate step by step, promotion, bisection of a red batch |
| [docs/MACHINES.md](docs/MACHINES.md) | The machines file, `test`, runs on another machine over ssh, `machines init` |
| [docs/STATUS.md](docs/STATUS.md) | Every section of `status`, flaky tests, the conflict forecast |
| [docs/HOOKS.md](docs/HOOKS.md) | The stop hook, and what a step can and cannot do on macOS |
| [docs/FROZEN-LAYER.md](docs/FROZEN-LAYER.md) | The state in `.git/atelier/` that every version shares, format by format |
| [docs/INTERNALS.md](docs/INTERNALS.md) | How each command works, the source layout, the dependencies |

To work on atelier itself: Rust 1.100.0 is pinned, and `scripts/check.sh` runs the
format check, clippy and every test.

## Licence

MIT. See [LICENSE](LICENSE).
