# Machines and remote runs

How `atelier test` and `merge` choose a machine, and how a run on another machine works.

## The machines file

`~/.config/atelier/machines.toml`, yours, shared by all projects, never committed. Unknown keys are refused.

```toml
[machines.mac]
local = true
platform = "macos-aarch64"
gui = true
priority = 100                  # lower is tried first

[machines.linux-box]
ssh = "linux-box.local"
platform = "linux-x86_64"
priority = 10
```

| Key | Meaning |
|---|---|
| `local` | this computer |
| `ssh` | ssh destination of a remote machine |
| `platform` | `<os>-<arch>`, e.g. `linux-x86_64`; Windows is refused |
| `gui` | the machine has a GUI session (default false) |
| `priority` | required; lower is tried first |
| `connect_timeout_seconds` | remote only; default 5 |
| `path` | remote only; the exact `PATH` there, entries absolute or starting with `~/` |
| `checkouts` | remote only; folder holding projects' checkouts (default `~/.local/share/atelier/projects`); no `!`, quotes or backslashes |

A project's checkout on a machine is `<checkouts>/<name>-<hash>`: the main checkout's folder name and 8 hex digits of a hash of its git folder. Without a machines file, only this computer's own platform has a machine.

## Choosing a machine

- **Candidates:** the platform's machines (with a GUI when the platform sets `gui = true`), in the project's `machines` order when given (only those), else by `priority`, then name. A named machine that cannot serve is reported in its place (`is linux-x86_64`, `is not in machines.toml`, `has no GUI`).
- **Each is tried in turn:** connect, preflight, machine lock. Skips print as `offline`, `not ready: <line>` or `busy`. A machine found not ready after its session started is passed over the same way.
- **Waiting:** if none takes the run and some were only busy, they are retried every 5 s; the first free one wins. One that goes offline or not ready is dropped.
- **Otherwise** exit 3, each machine listed with its reason.
- A stop signal (Ctrl-C) ends the choice: nothing more is probed, pushed or started.

The machine lock is `~/.local/state/atelier/machine.lock` (`flock`), with `machine.holder` naming the holder. Whoever runs the steps holds it: `test` or the gate on this computer, `remote-run` on another.

## `atelier test <platform>|--all [-- <command>…]`

Refuses (4), before any machine is tried: uncommitted changes (listed); a current folder that is not part of the committed `HEAD`; a platform the project does not declare; a Windows platform.

- **The tree must stay the commit's.** It is checked before each platform and after the last. A moved `HEAD` or changed file ends the line with `RED: the tree changed during the run`, exit 1.
- **Each platform** runs in turn as its own run (kind `test-full`, or `test-narrow` with a command), holding the worktree's run lock.
- **The job:** the `check.prepare` hooks for the platform, the platform's `prepare`, then the command (run from the same folder relative to the checkout root) or the platform's `run`. Every step gets `NEXTEST_RETRIES=0 NEXTEST_FLAKY_RESULT=fail`. With `run = "check"`, the full `atelier check` runs as its own run (kind `check`) and the platform's run takes its outcome.
- **Final line**, always printed: `linux-x86_64: PASS on linux-box (mac-mini offline) · macos-aarch64: PASS on mac (local)`. Other outcomes: `FAIL`, `connection lost`, `no machine available`, `INTERRUPTED`, `ERROR`.
- **Exit:** 1 if the tree changed or any platform failed or was interrupted; else an error's own code; else 3 if any had no machine or lost its connection; else 0.

A remote run records the machine, the remote run ids (`remote-runs`), the streamed log (`remote-<machine>.log`) and the remote failures.

## A remote run

1. **Probe.** `ssh -o BatchMode=yes -o ConnectTimeout=<t>` runs the pinned atelier's `remote-run probe`. No answer (ssh exit 255) is `offline`. It checks the program's version and source hash (different: `not ready: … run atelier machines init`), the platform, the checkout and the lock holder, without taking the lock. Tool prerequisites depend on the commit, so `atelier machines` shows `ready (prerequisites checked at run time)`.
2. **Push.** `git push --no-verify <host>:<checkout> +<commit>:refs/atelier/test`, over the same ssh options. Nothing goes to GitHub.
3. **Session.** `ssh -tt` (`ServerAliveInterval=5`, `ServerAliveCountMax=3`) runs `env PATH=<machine path> ATELIER_DISPATCHED=1 <pinned atelier> remote-run run`. There, in order: platform, checkout, commit arrived, machine lock, checkout clean, `git checkout --no-overwrite-ignore --detach <commit>`, files exactly the commit's tree, preflight on the commit's files, run lock, then the job. Any failure before the job starts is `not ready`, and the next machine is tried. Nothing in the checkout is ever deleted: files in the way (ignored files git would overwrite) are listed in the `not ready` line and left for the owner.
4. **Connection loss.** The remote side watches its terminal; when the connection drops it stops its steps (TERM, grace, KILL) and the result is `connection lost` (exit 3).
5. **Verdict.** Read with a second ssh. A pass needs at least one run, and every result must record this computer's `HEAD`, its tree and `uncommitted = false`; otherwise FAIL, `tested a different tree`.

**Command length.** ssh passes the command to the login shell (sh, bash, zsh, csh, tcsh and fish all work) as one argument. Linux caps one argument at 128 KiB, and each byte other than letters, digits, spaces and `_./,:=+@-` takes 5 characters. A narrow command over about 26 KB of symbols (128 KB of plain words) fails with `not ready: … Argument list too long`. Keep narrow commands to a test filter or a short script.

`ATELIER_SSH` replaces the `ssh` program.

## Machine preflight

Before the job, on the commit's files: the platform from `uname`, then `atelier check`'s prerequisites (pinned Rust toolchain per workspace, nextest version, `[[prerequisites.tools]]` for the platform, nextest configuration files, and with `[prerequisites.mbx]` its version, settings and build folders). The first problem is the `not ready` line.

## `atelier machines init <name> [--yes]`

Creates the project's checkout on the machine (`git init`) and installs the running atelier's version at `~/.local/share/atelier/<version>/`. A local machine needs nothing. It prints its plan first. Replacing an existing install (other source, or `broken`: missing, not executable or silent) needs `--yes`; without it, exit 4 and nothing changes.

- **Source.** It builds from the source embedded in this binary, sent over ssh and built with `cargo install --locked`. The machine needs Rust (the `rust-toolchain.toml` version) and crates.io or a mirror.
- **Swap.** The new build replaces the old in one atomic folder exchange, under the machine lock (a run in progress: exit 3). On Linux this needs glibc 2.28 or newer. The replaced folder is kept once as `.<version>.replaced-<time>-<pid>`.
- **No exchange, no replacement.** Filesystems that cannot exchange two folders (NFS, SMB, exFAT, some older Linux filesystems) make `init` exit 4 before building. Move the old folder away or keep `~/.local/share/atelier` on a local filesystem.
- **Pin changes.** A machine keeps every version side by side, and runs each merge with the merged commit's pin. Before merging a pin change, run `atelier machines init <name>` with the new version for each machine that runs a platform. Otherwise the machine is `not ready: atelier <pin> is not installed there`.

**Trust.** The identity check catches a stale or different build, not a tampered one. Tested code runs as the machine's account and could replace the binary. Only list machines whose account may run the project's code.

## Known limit

During a local run the job runs in the worktree itself. A file changed and changed back between two tree checks is not seen. Remote runs use atelier's own checkout and are not affected.
