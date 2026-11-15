# atelier

**Several coding agents build one project in parallel, with no CI wait and no merge queue.**

atelier is a command-line tool the agents run themselves. An *orchestrator* plans, reviews and merges. *Workers* each build one branch in their own git worktree. It fixes four problems measured on earlier multi-agent builds:

| Problem | What atelier does |
|---|---|
| Agents waited on CI: 31-49% of build time. One CI job failed 52 of 166 runs. | `merge` checks the merged tree on your own machines, on every platform, and moves `main` only when green. |
| Items ran one at a time (1.4 workers on average). | One worktree per item. An item can build on an unmerged one (`start --on`, `follow`). Approved branches merge as one batch; a red batch is split and the good part lands. |
| Slow, untrusted test loops: full suites by reflex, retries hiding failures. | Workers run only the tests that prove their change. The full check runs once per merge, with no retry. A test that fails then passes on the same tree is listed as flaky. |
| Each agent followed its own rules. | `init` and `rules` generate the rules every agent reads. `task` renders the same prompts every time. Agents are chosen by role. |

`main` moves only to a tree that passed, and only with the exact commits that were reviewed. A platform with no machine fails; nothing is skipped or cached.

> **Status: early (0.1.0).** Used on one real project. macOS and Linux only.

## Rules that ship with it

- **Decide before the worker starts.** Items with written decisions left 5 problems after review; without, 13.
- **Two high-effort reviews for risky items.** They find 2-3x more than one default-effort review.
- **One model-based test per rule that must always hold.** One such test caught 4 of 7 high-severity past bugs that ordinary tests missed.

## Install

You need git and Rust 1.100.0 (via rustup).

```sh
# The version your project pins. Versions sit side by side.
cargo +1.100.0 install --git https://github.com/amontlabs/atelier --tag v0.1.0 --locked --root ~/.local/share/atelier/0.1.0
# The dispatcher, once.
mkdir -p ~/.local/bin && cp ~/.local/share/atelier/0.1.0/bin/atelier ~/.local/bin/atelier
```

`~/.local/bin` must be on `PATH` for you and your agents. The `atelier` on `PATH` reads the `atelier = "<version>"` pin from `.atelier/atelier.toml` and runs `~/.local/share/atelier/<version>/bin/atelier`. A missing version exits 4 with the install command; nothing installs itself.

## Quickstart

```sh
cd shop && atelier init --version 0.1.0
```

This writes `.atelier/`, the generated rules (`AGENTS.md`, `CLAUDE.md`, `.atelier/rules/`) and the stop hook (`.claude/settings.json`, `.codex/hooks.json`). In Codex, run `/hooks` once to trust the hook.

Say what your check is in `.atelier/atelier.toml`:

```toml
atelier = "0.1.0"
integration_branch = "main"
workspaces = ["."]            # Cargo workspaces: fmt, compile, nextest

[rust]
nextest = "0.9.151"           # exact cargo-nextest version

[[check.steps]]               # any command; the only check for non-Rust projects
name = "docs"
run = ["cargo", "doc", "--no-deps", "--locked"]
# parallel = false                # by default steps run one after another while the tests run;
                                  # false runs this one alone, after the tests and earlier steps

[platforms.macos-aarch64]     # must pass before main moves
run = "check"
```

A workspace needs a `rust-toolchain.toml` with an exact `channel`. Unknown keys are refused. Every key is in [tests/fixtures/silo-atelier.toml](tests/fixtures/silo-atelier.toml). After an edit, run `atelier rules` and commit `.atelier/`, `AGENTS.md`, `CLAUDE.md` and the two hook files. Your own rules go in `.atelier/project/{binding,worker,orchestrator}.md`.

Your roles, in `~/.config/atelier/agents.toml` (shared by all projects, never committed; `build`, `build-hard` and `review` are required):

```toml
[roles.build]
for = "an ordinary item or fix"
start = "how your harness starts this agent"
template = "build"
```

One item, end to end:

```console
$ atelier start cart --path ../shop-cart
$ atelier task build cart goal="Add a cart" files="src/cart.rs" done="total is tested" risks="none"
Read /Users/you/shop/.git/atelier/tasks/20261113T185543Z-ca5c7e2c611d79fa.md: it is your task.
# The worker commits, then:
$ atelier check --compile-only
compile: OK — run 20261113T1855Z-10d9ab5227b33d4d, commit dd9a596 (clean), 1s, log …
# The orchestrator reviews dd9a596, then:
$ atelier approve cart dd9a596
$ atelier merge cart
merge: GREEN — run 20261113T1855Z-462b4b3843b5b863, main ff7cf4c → 0ba07f4, macos-aarch64 PASS on this computer, 4s
merged cart dd9a596
$ atelier clean          # prints the plan
$ atelier clean --yes    # removes the worktree and branch
```

A branch is merged only when `merged <branch> <commit>` is printed. On any problem atelier stops, leaves `main` alone and says what to do.

## Commands

| Command | Run by | Does |
|---|---|---|
| `init --version <v>` | owner | Sets up a project |
| `rules` | owner | Regenerates the rules after a settings or `.atelier/project/` edit |
| `status [--json]` | anyone | Read-only overview ([STATUS.md](docs/STATUS.md)) |
| `agents` | orchestrator | Lists your roles |
| `start <branch> [--on <lower>] [--path <worktree>]` | orchestrator | Creates a branch and worktree |
| `task <role> <branch>\|- [field=value…] [--fixes]` | orchestrator | Writes a task file, prints the agent's prompt |
| `follow` | worker | Rebases the branch onto what it builds on |
| `check [--compile-only]` | worker, gate | Prerequisites, format, compile, tests (no retry), your steps |
| `test <platform>\|--all [-- <command>…]` | worker | Runs a platform's check, or one command, on a machine of it |
| `tmp [--short] -- <command>…` | worker | Runs a command with a temp folder in `$ATELIER_TMP` |
| `approve <branch> <commit>` | orchestrator | Marks this exact commit reviewed |
| `merge <branch>… [--waive <platform> --reason "<words>"]` | orchestrator | Merges, checks every platform, moves `main` if green |
| `promote [--discard]` | orchestrator | Finishes a green merge that found the main checkout busy |
| `clean [<branch>…] [--yes] [--discard <commit>…]` | orchestrator | Removes merged branches, worktrees and temp folders |
| `machines [init <name> [--yes]]` | owner | Lists machines; sets one up over ssh |
| `hook stop` | harness | Stop hook: sends a worker back while work is unfinished |

| Exit | Meaning |
|---|---|
| 0 | Done; GREEN / PASS |
| 1 | RED / FAIL |
| 2 | Wrong usage |
| 3 | No machine for a platform |
| 4 | Refused, nothing changed; the message says why |
| 5 | Merge conflict; files listed |
| 7 | GREEN, but the main checkout was busy: run `promote` |

## Limits

- No task board, no agent runner, no model choice: your harness starts agents.
- No GitHub, pull requests or CI. No retries.
- A conflict stops the merge, except in files a `merge.after` hook regenerates.
- It never deletes what it did not make, and installs nothing.
- Not a sandbox: agents' code runs as your user. The stop hook is a nudge, not a guarantee.
- No Windows.

## Docs

- [MERGE.md](docs/MERGE.md): `approve`, `merge`, `promote`, bisection.
- [MACHINES.md](docs/MACHINES.md): the machines file, `test`, remote runs, `machines init`.
- [STATUS.md](docs/STATUS.md): `status`, flaky tests, conflict forecast.
- [HOOKS.md](docs/HOOKS.md): the stop hook; steps on macOS.
- [FROZEN-LAYER.md](docs/FROZEN-LAYER.md): state formats in `.git/atelier/`.
- [INTERNALS.md](docs/INTERNALS.md): how commands work, source layout.

Working on atelier: `scripts/check.sh` runs format, clippy and every test.

## Licence

MIT. See [LICENSE](LICENSE).
