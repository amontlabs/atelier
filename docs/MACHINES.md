# Machines and remote runs

How `atelier test` and the gate choose a machine, and how a run on another machine works.

## The machines file

`~/.config/atelier/machines.toml` plus one optional key for remote machines:
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
each machine listed with its reason. A remote machine can also turn out not ready after its
session started (a missing prerequisite, files in the way of its checkout): it is passed over
like a probe failure, and the next machine is tried, also after a wait.

The machine lock is `~/.local/state/atelier/machine.lock` (`flock`), with `machine.holder`
naming the holder for messages. It is held by the atelier that runs the steps: `test` on this
computer, `remote-run` on another.

## `atelier test`

Refuses (4) uncommitted changes (the tree id differs from `HEAD`'s; the files are listed), a
caller folder that is not a folder of the committed `HEAD` (an empty, untracked or ignored
folder: no other machine has it), a platform the project does not declare, and a Windows
platform.

**The tree stays the commit's.** `test` takes the commit and its tree at the start and checks
again before each platform, once this computer is taken as the machine (after any wait), and
after the last platform. A moved `HEAD` or a changed file (uncommitted changes) is RED: the
final line ends with `RED: the tree changed during the run` and the exit is 1. Each run records
`<commit>^{tree}`; a remote platform's run records the tree the machine reported. Platforms run one after the other
(`--all`), each as its own run of this worktree (kind `test-full` or `test-narrow`), holding the
worktree's run lock. On this computer the job runs in the worktree; the result has no `machine`.
On another machine this computer's run records the machine, the remote run ids
(`remote-runs`), the streamed log (`remote-<machine>.log`) and the remote failures.

The job: the `check.prepare` hooks that apply to the platform, then the platform's `prepare`,
then the command (in the caller's folder relative to the checkout root) or the platform's
`run`. Each step has `NEXTEST_RETRIES=0 NEXTEST_FLAKY_RESULT=fail`. The tree id is compared after
each preparation step and at the end; a change fails the run. When the platform's `run` is
`"check"`, the job's run holds only the platform's `prepare` and then the full `atelier check`
runs as its own run (kind `check`, `check::run`), on this computer or on the machine; its
`check.prepare` hooks run inside the check. The job's run is the platform's record: it is
finished with the check as its follow-up (`Run::finish_after`), so its `result` is written only
after the check, with the check's outcome (`failed`, "atelier check failed (run <id>)", when the
check is RED), and its folder names the check's run in `check-run`. Until then the run has no
result, so no reader takes it for a pass. The same holds on a remote machine.

Final line: `linux-x86_64: PASS on linux-box (mac-mini offline) · macos-aarch64: PASS on mac (local)`;
otherwise `FAIL`, `connection lost`, `no machine available`, `INTERRUPTED` or `ERROR` (the
platform could not run; the error is printed). The line is always printed, also after an error
in one platform of `--all`. Exit: 1 if the tree changed or any platform failed or was
interrupted, else an error's own code, else 3 if any had no machine or lost its connection,
else 0. Exit 4 is for the caller's own tree (uncommitted changes, a folder not in the commit),
before any machine is tried; a problem of one machine's checkout makes that machine `not
ready`, and the next one runs.

A stop signal (Ctrl-C) while machines are tried ends the choice: no further machine is
probed, pushed to or started.

## A remote run

1. **Probe**: `ssh -o BatchMode=yes -o ConnectTimeout=<t> …` runs the pinned atelier's
   `remote-run probe`. No answer (ssh exit 255) is `offline`. A login shell that did not run
   the command is `not ready` (see "The command line" below). The probe reports the program's
   identity (version and source hash; a different one is `not ready: … run atelier machines
   init`), checks the platform from `uname` and the checkout, and whether the machine lock is
   held: it reads `machine.holder` (a live process with the recorded start time) and never
   takes the lock, so a look never makes a real run elsewhere find the machine busy. The tool
   prerequisites depend on the commit's files, so they are checked when the run starts; for
   that reason `atelier machines` says `ready (prerequisites checked at run time)` for a remote
   machine. Each machine that turns out not ready then also cost a push and a session.
2. **Push**: `git push --no-verify --receive-pack='git receive-pack' <host>:<checkout>
   +<commit>:refs/atelier/test`, through the same ssh options. Nothing goes to GitHub.
3. **Session**: `ssh -tt` with `ServerAliveInterval=5`, `ServerAliveCountMax=3`, running
   `env PATH=<exactly the machine's path> ATELIER_DISPATCHED=1 <pinned atelier> remote-run run`.
   `--dir=<folder>` is passed for narrow runs only. On the machine: platform; checkout; the
   commit arrived; machine lock (`busy`); the checkout clean (`git status`; changes and unknown
   files are listed, never deleted: `not ready`); `git checkout --no-overwrite-ignore --detach
   <commit>` (below); the files are exactly the commit's tree (the tree id of every
   non-ignored file equals `<commit>^{tree}`; otherwise the checkout goes back to where it was
   and the machine is `not ready`, listing the files: an ignored file the commit no longer
   ignores); the command preflight; the machine preflight on the commit's files; the checkout's
   run lock (an earlier run's processes make it not ready); then `taken` and the job. Every
   failure before `taken` is `not ready`, and the caller tries its next machine.

   **Files in the way.** A plain `git checkout` overwrites or removes ignored files when it
   needs their place. With `--no-overwrite-ignore` git refuses instead, before it changes
   anything, and names each path ("untracked working tree files would be overwritten", or
   "Updating the following directories would lose untracked files in them"); atelier lists
   them in the `not ready` line and deletes nothing. git decides what is in the way, with its own
   rules for folders that a file replaces, ignored folders inside ignored folders, nested
   repositories, links, and volumes that ignore case (`core.ignorecase`: a tracked `Build` is in
   the way of an ignored `build/`, and a rename of `Foo` to `foo` is an ordinary checkout).
   Checked with git 2.50.1 (macOS, APFS) and 2.53.0 (the Linux test machine, an ext4-family filesystem). A refused
   checkout, and one sent back after the tree check, leaves `HEAD`, the index and every file as
   they were, so the same machine gives the same answer until its owner moves the files.
4. **Watcher**: the remote atelier reads its terminal, which this computer never writes. On
   end-of-input or a read error it sends itself `SIGHUP`, so the run stops its group (TERM,
   grace, KILL) as for the terminal's own hang-up, and the result says `connection lost`.
5. **Stream**: the terminal carries the step logs and protocol lines marked with a per-session
   nonce (`taken`, `run <id>`, `done`, `not-ready`, `busy`, `error`). The runs
   announced are the one `remote-run` started (by its id) and the `check` run it started (same
   pid and the same process start time), never a run folder that only shares the pid.
6. **Verdict**: each run's `result`, read with a second ssh. No `done`, an ssh that died (exit
   255 or a signal), or a remote result saying `connection lost` is `connection lost` (exit 3).
   After a session without `done`, the results are polled for up to 20 s so the remote account
   is kept. A pass needs at least one run, and each result must record this computer's `HEAD`
   commit, its tree and `uncommitted = false`; otherwise FAIL, `tested a different tree`.

**The command line.** ssh hands one string to the account's login shell. atelier sends
`exec /bin/sh -c 'eval "$(printf "%b" "<script>")"'`, where the script's letters, digits, spaces
and `_./,:=+@-` stay as they are and every other byte is written `\0ooo`. Inside the single
quotes there is no `'`, `!`, newline or `\\` pair, which sh, bash, zsh, csh, tcsh and fish all
pass on unchanged (csh and tcsh expand `!` and reject newlines even in single quotes; fish reads
`\\` and `\'` there as escapes). A `unit test` runs it through every one of these shells found
here; the stand-in ssh can run commands through tcsh (`FAKE_SSH_SHELL`). The probe's script
first prints a marker; a login shell that does not run it makes the machine `not ready`.
`checkouts` may not contain `!`, quotes or backslashes (git's own push command passes it
through the login shell). `ATELIER_SSH` replaces the `ssh` program (used by the tests).

**How long a narrow command can be.** The encoding writes each byte other than letters, digits
and `_./,:=+@-` and spaces as 5 characters, and ssh passes the whole line to the login shell as
one argument. Linux limits one argument to 128 KiB (`MAX_ARG_STRLEN`), so a narrow `--
<command>` of about 26 KB of quotes and brackets (about 128 KB of plain words) is too long: the
login shell cannot start, and each remote machine is `not ready: remote-run ended without a
status: … Argument list too long`. This limit is kept, not worked around. The session's input
cannot carry the command: it is a terminal (`ssh -tt`, line-length limits and echo in its
line discipline) and the watcher reads it to notice a lost connection. A file pushed with the
commit would change the tested tree, and a separate upload would add a fourth connection and a
cleanup. A narrow command is a test filter or a short script, far below the limit.

## Machine preflight

`jobs::machine_problems`: the platform from `uname`, then `atelier check`'s own prerequisites
(`prereq::verify`) for the commit's files: the pinned Rust toolchain per workspace,
the nextest version, each `[[prerequisites.tools]]` that applies to the platform, the nextest
configuration files, and with `[prerequisites.mbx]` mbx's version and settings, `.mbx.toml`,
the bypass variables, absolute `PATH` entries, cargo's target folder and plain-cargo build
folders. On a remote machine it runs after the checkout, so it sees the commit's files; on this
computer, on the worktree (which `test` requires to be `HEAD`). The first problem is the
`not ready` line.

## `atelier machines init <name> [--yes]`

Creates the project's checkout there (`git init`) and installs the pinned atelier at
`~/.local/share/atelier/<version>/`, nothing else. A local machine needs nothing. It prints its
plan first (the checkout: exists or created; atelier: installed already, installed, or which
install it replaces). That folder is also the one the machine's own projects run through the
dispatcher, so replacing what is there needs `--yes` (exit 4 without it, nothing changed).

**What counts as an install.** One rule, applied by this computer for the plan (in shell) and by
the new program before it swaps (`remote_run::install_state`): nothing at the version's path is
`none`; else the identity its `bin/atelier remote-run identity` prints; else, when that program
is missing, not executable, a broken link, fails or prints nothing, `broken`. A broken install
(an interrupted install leaves one) is named in the plan as such and replaced with `--yes`, like
an install of other source.

The build goes into a new folder beside it. The newly built program then swaps it in with
`remote-run install`: it takes the machine lock (a run in progress makes `init` exit 3, busy,
nothing replaced), checks that the install is still what the plan saw, and exchanges the two
folders in one atomic step (`src/exchange.rs`: `renamex_np(RENAME_SWAP)` on macOS,
`renameat2(RENAME_EXCHANGE)` on Linux), so there is no moment without an install. On Linux the
`renameat2` call needs **glibc 2.28 or newer** (2018); an older glibc cannot link the program.
The replaced folder is kept as `.<version>.replaced-<time>-<pid>` beside it, since a program may
still run from it; older replaced folders of the same version are removed then, under the
machine lock, so at most one is kept. If the kept folder cannot be renamed after the exchange,
the old install stays under the new folder's name, and `init` prints where: after the exchange
nothing is removed.

**No exchange, no replacement.** Some filesystems cannot exchange two folders (NFS, SMB, exFAT,
some older Linux filesystems). There is no fallback: two plain renames would leave a moment
without an install, or a half-done state if the second fails. When an install is to be
replaced, the install script first compiles `src/exchange.rs` on its own (`rustc`, which every
Rust install has; the file needs no crate) and runs it on `~/.local/share/atelier`: it exchanges
two empty folders it creates there and removes them. If that fails, `init` exits 4 before the
build, naming the error, and nothing is built or changed; the owner moves the old folder away
(then it is a fresh install, a plain rename) or keeps the install folder on a local filesystem.

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

**A pin change.** `init` installs the version of the atelier that runs it. A machine keeps
every version installed side by side, and `merge` runs each platform there through the merged
commit's own pinned atelier (docs/MERGE.md, "Platforms on machines"): its probe asks whether
that version is installed, and a machine without it is `not ready: atelier <pin> is not
installed there: run atelier machines init <name> with atelier <pin>`. So before merging a
commit that changes the pin, run `atelier machines init <name>` with the new version (the one
the commit pins, installed here first) for each machine that is to run its platforms. For
another version than its own, the probe can check only the version the installed program
reports, not its source.

**What the identity proves.** The identity is what the installed program prints about itself.
It catches a stale or different build, not a tampered one: a run executes the worker's commit
as the account that owns the install, and that code could replace the binary with one that
prints the expected identity and writes `outcome = "passed"`. atelier accepts this, as on this
computer, where a local run has the same reach over the local install: the trust boundary is
the account (same-user trust), not the binary. A machine whose account must not trust the
project's code should not be in `machines.toml`. Likewise `rust-toolchain.toml` pins the
compiler only where rustup reads it; without rustup the build uses whatever `rustc` is on the
machine's `path`, and the identity is still equal.

## Known limits

- **A file changed and changed back during a local run passes.** On this computer the job runs
  in the worktree itself. The tree is compared after the machine is taken, at the start and end
  of the steps, and at the end of `test`; a file changed after one comparison and restored before
  the next is not seen, and the run can pass although the commit's files would fail. Every
  result is written before the last comparison, which can only turn the exit RED. A detached
  `git worktree` at the commit would close this gap; it is not built. On a remote machine the
  checkout is atelier's own; a local edit during a remote run makes the whole result RED, which
  errs on the safe side.
- **A narrow command's length**: see "The command line".

## Tests

- Unit (`machines.rs`): candidates by priority, by project order, the GUI filter, skips named,
  the busy-only wait, the first busy machine that frees, a waited machine going offline, a stop
  while waiting or during an attempt, a machine whose checkout is in the way passed over, a
  leading `-` in a name, the machine lock and reading its holder without it. `remote.rs`: the
  command line through every login shell found here, the tree check of a result.
  `remote_run.rs`: the checkout refusing every layout found in review (an ignored file, an
  ignored folder that becomes a file, a folder inside an ignored folder, an ignored nested
  repository, a link to a folder outside, an ignored file in a tracked folder, no `HEAD`) with
  everything unchanged and the same answer twice, case-only differences on the volume at hand,
  files no longer ignored sending the checkout back (also without a commit), the install rule
  (none, identity, broken), the swap under the lock with one replaced folder kept, the old
  install kept when its rename fails, the exchange probe compiled on its own, run ids in the
  stream. `jobs.rs`, `source.rs` helpers.
- Integration (`tests/machines.rs`): this computer has no ssh server, so remote machines are
  played by `tests/fixtures/fake-ssh`, which runs the command here in the machine's own HOME
  with a fresh environment and relays stdin and stdout, so killing it ends the remote side's
  input as a cut connection does. It covers `machines`, `machines init` (with the binary under
  test pre-installed; its plan and the `--yes` refusal), pass, fail, the caller's folder, the
  order of preparation, tree changes (by a step, while waiting, during a remote run, a moved
  `HEAD`), an unclean remote checkout, files in the way (not ready, the next machine runs, the
  same answer twice), a remote tree that is not the commit's (sent back, the same answer
  twice), a result for another tree, a session without runs, a missing prerequisite found when
  the run starts (also after a wait), a folder not in the commit, exit 3, busy, waiting, a stale
  remote atelier, a killed connection, a tcsh login shell and one that cannot run the command, a
  leading `-` in a machine name, `--all` after an error, `run = "check"` here and on a machine
  with the check's outcome in the platform's run, and `machines init` with stand-ins for
  `cargo` and `rustc` (a broken install, a filesystem that cannot exchange, one replaced folder
  kept).
- Real machine (`tests/remote_e2e.rs`, opt-in with `ATELIER_E2E_SSH=<host>`): `machines init`
  building from source (replacing another install and a broken one, the exchange probe built
  there, one replaced folder kept), `machines`, full, narrow and failing runs, a killed `ssh -tt`
  client (`connection lost`, exit 3, the remote step stopped), and an ignored file in the way of
  the checkout (not ready, unchanged, the same answer twice).
