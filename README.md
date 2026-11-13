# atelier

atelier keeps one project correct while several coding agents work on it at once, each in its
own git worktree. It records what each branch is built on and moves those branches when their
base moves. It binds "reviewed" to one exact commit. It merges through a gate that checks the
merged tree on every platform the project needs, and moves `main` only when that tree is green.
It deletes only what it made and what was merged. Agents get generated rules that cover only the
parts the project uses.

The design is kept outside this repository (`DESIGN.md`, revision 5, with mechanics in
`DESIGN-r4.md`). The formats every version shares are in [docs/FROZEN-LAYER.md](docs/FROZEN-LAYER.md).

## Status: skeleton (module 3.1)

Built so far, with tests on throwaway git repositories:

- the command line with every command declared. Each command runs its preflight, then exits 2
  with `not built yet (module N)`. The exit codes are 0, 1, 2, 3, 4, 5 and 7, as in the design;
- the dispatcher and the per-project version pin;
- parsing of `.atelier/atelier.toml`, the owner's `machines.toml` and the owner's `agents.toml`;
- the shared state in `.git/atelier/`: a frozen layer, a versioned layer with a format number,
  and `state.lock`;
- locks, runs, process groups and the survivor scan, stop signals, the tree id, and plain refs.

The command behaviour (`check`, `start`/`follow`, `merge`, `tmp`/`clean`, machines, rules,
`status`) comes in later modules.

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
| `runs`, `signals` | run ids, folders, steps, atomic results; TERM/INT/HUP |
| `tree` | tree id via a temporary index; submodule refusal |
| `refs`, `names`, `git` | plain refs and `--no-deref` updates; branch names and file keys; running git |
| `rules` | rules currency check (stub until module 3.7) |

## Dependencies

clap, serde and toml handle the command line and the files; tempfile is used by tests only.
Locks use the standard library's `File::lock` (`flock`, stable since Rust 1.89), so fs2 and
rustix are not needed. Two more crates:

- `libc`: `kill` on process groups, `setsid` in tests, and `sysctl(KERN_PROCARGS2)`, the source
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
