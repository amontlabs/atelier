# Machines and remote runs (module 3.6)

DESIGN §2.5 and §3 `test`; the remote mechanics of r4 §4.4 and experiment B3.

## The machines file

`~/.config/atelier/machines.toml` as in DESIGN §2.5, plus one optional key for remote machines:
`checkouts`, the folder that holds the projects' checkouts there (default
`~/.local/share/atelier/projects`). `path` entries are absolute or start with `~/`.

A project's checkout on a machine is `<checkouts>/<name>-<hash>`: the main checkout's folder
name and 8 hex digits of a hash of its git folder.

## Choosing a machine

`machines::candidates` and `machines::select`. Candidates: the machines of the platform, with a
GUI when the platform says `gui = true`, in the project's `machines` order when given (only the
machines it lists), else by `priority` then name. A machine the project names that cannot run
the platform, and a machine without the GUI the platform needs, is named in its place
(`is linux-x86_64`, `is not in machines.toml`, `has no GUI`). Each candidate is tried: connect,
preflight, machine lock; skips are `offline`, `not ready: <line>`, `busy`. If none takes the run
and at least one was only busy, atelier waits, retrying the busy ones in order every 5 s, and
takes the first that frees; one that turns offline or not ready is dropped. Otherwise exit 3,
each machine listed with its reason.

The machine lock is `~/.local/state/atelier/machine.lock` (`flock`), with `machine.holder`
naming the holder for messages. It is held by the atelier that runs the steps: `test` on this
computer, `remote-run` on another.

## `atelier test`

Refuses (4) uncommitted changes (the tree id differs from `HEAD`'s; the files are listed), a
platform the project does not declare, and a Windows platform. Platforms run one after the other
(`--all`), each as its own run of this worktree (kind `test-full` or `test-narrow`), holding the
worktree's run lock. On this computer the job runs in the worktree; the result has no `machine`.
On another machine this computer's run records the machine, the remote run ids
(`remote-runs`), the streamed log (`remote-<machine>.log`) and the remote failures.

The job: the `check.prepare` hooks that apply to the platform, then the platform's `prepare`,
then the command (in the caller's folder relative to the checkout root) or the platform's
`run`. Each step has `NEXTEST_RETRIES=0 NEXTEST_FLAKY_RESULT=fail`. The tree id is compared after
each preparation step and at the end; a change fails the run. When the platform's `run` is
`"check"`, the job's run holds only the platform's `prepare` and then `atelier check` runs as its
own run (`jobs::full_check`, which says `not built yet (module 3.2)` until 3.2 is merged).

Final line: `linux-x86_64: PASS on linux-box (mac-mini offline) · macos-aarch64: PASS on mac (local)`;
otherwise `FAIL`, `connection lost`, `no machine available` or `INTERRUPTED`. Exit: 1 if any
platform failed or was interrupted, else 3 if any had no machine or lost its connection, else 0.

## A remote run

1. **Probe**: `ssh -o BatchMode=yes -o ConnectTimeout=<t> …` runs the pinned atelier's
   `remote-run probe`. No answer (ssh exit 255) is `offline`. The probe reports the program's
   identity (version and source hash; a different one is `not ready: … run atelier machines
   init`), checks the platform from `uname`, the checkout, the machine preflight with this
   checkout's settings (sent on stdin), and whether the machine lock is free.
2. **Push**: `git push --no-verify --receive-pack='git receive-pack' <host>:<checkout>
   +<commit>:refs/atelier/test`, through the same ssh options. Nothing goes to GitHub.
3. **Session**: `ssh -tt` with `ServerAliveInterval=5`, `ServerAliveCountMax=3`, running
   `env PATH=<exactly the machine's path> ATELIER_DISPATCHED=1 <pinned atelier> remote-run run`.
   On the machine: platform; checkout; the commit arrived; machine preflight from the commit's
   settings (`git show`); machine lock (`busy`); the checkout clean (`git status`; changes and
   unknown files are listed, never deleted: `not ready`); `git checkout --detach <commit>`; the
   command preflight and the checkout's run lock (an earlier run's processes make it not
   ready); then `taken` and the job.
4. **Watcher**: the remote atelier reads its terminal, which this computer never writes. On
   end-of-input or a read error it sends itself `SIGHUP`, so the run stops its group (TERM,
   grace, KILL) as for the terminal's own hang-up, and the result says `connection lost`.
5. **Stream**: the terminal carries the step logs and protocol lines marked with a per-session
   nonce (`taken`, `run <id>`, `done`, `not-ready`, `busy`, `error`).
6. **Verdict**: each run's `result`, read with a second ssh. No `done`, an ssh that died (exit
   255 or a signal), or a remote result saying `connection lost` is `connection lost` (exit 3).
   After a session without `done`, the results are polled for up to 20 s so the remote account
   is kept.

Every remote command is `exec /bin/sh -c '<script>'`, so the login shell does not matter.
`ATELIER_SSH` replaces the `ssh` program (used by the tests).

## Machine preflight

`jobs::machine_problems`: the platform from `uname`; each `[[prerequisites.tools]]` that applies
to the platform (first output line starts with `expect`); with `[prerequisites.mbx]`:
`MBX_DISABLE`, `CARGO_TARGET_DIR`, `RUSTC_WRAPPER` unset, no relative `PATH` entry, the first
`cargo` on `PATH` is one of `mbx.cargo` (with `~` expanded on the machine), `mbx --version`
names the pinned version, each workspace's `target` absent or a link into mbx's storage. This
is a subset of module 3.2's prerequisites (`prereq::verify`, not on this branch): once 3.2 is
merged, the machine preflight should call it instead.

## `atelier machines init <name>`

Creates the project's checkout there (`git init`) and installs the pinned atelier at
`~/.local/share/atelier/<version>/`, nothing else. A local machine needs nothing.

The install builds there from **this binary's own source**, embedded in it (`source.rs`: every
file `cargo install --path` needs, kept equal to the tracked source by a test), sent as a tar
stream over ssh and built with `cargo install --locked` (`MBX_DISABLE=1`, a temporary target
folder, the machine's `PATH`), then moved into place. Why not the alternatives:

- copying a binary built here needs a cross linker and target libraries for each machine's
  platform on the Mac, which a plain Rust install does not have;
- `cargo install --git … --tag v<version>` needs the machine to reach a published tag, which
  does not exist yet, and would fetch the source from somewhere other than what runs here.

The source hash makes a development build at the same version count as different: a machine
with other source is `not ready` until `machines init` replaces it. The machine still needs Rust
(the version in `rust-toolchain.toml`) and access to crates.io or a mirror.

## Tests

- Unit (`machines.rs`): candidates by priority, by project order, the GUI filter, skips named,
  the busy-only wait, the first busy machine that frees, a waited machine going offline, a stop
  while waiting, the machine lock; `jobs.rs`, `remote.rs`, `source.rs` helpers.
- Integration (`tests/machines.rs`): this computer has no ssh server, so remote machines are
  played by `tests/fixtures/fake-ssh`, which runs the command here in the machine's own HOME
  with a fresh environment and relays stdin and stdout, so killing it ends the remote side's
  input as a cut connection does. It covers `machines`, `machines init` (with the binary under
  test pre-installed), pass, fail, the caller's folder, the order of preparation, tree changes,
  an unclean remote checkout, refusals, exit 3, busy, waiting, a stale remote atelier, and a
  killed connection.
- Real machine (`tests/remote_e2e.rs`, opt-in with `ATELIER_E2E_SSH=<host>`): `machines init`
  building from source, `machines`, full, narrow and failing runs, and a killed `ssh -tt`
  client (`connection lost`, exit 3, the remote step stopped).
