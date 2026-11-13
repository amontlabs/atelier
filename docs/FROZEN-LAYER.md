# The frozen layer

`.git/atelier/` holds the state every worktree of a clone shares. Its frozen layer is read and
written by every atelier version, old and new, at the same time: a worker on an upgrade branch
runs its pinned version beside the orchestrator's. These rules keep that safe.

## Compatibility rule

- **Readers ignore fields they do not know.** A later version may add fields. It never removes a
  field, renames one, or changes what a value means.
- **Readers refuse what they cannot parse at all**: a file that is not TOML, a missing required
  field, a value of the wrong type, or a lock-record line of a shape this document does not
  describe. The command refuses (4) and names the file.
- **Writers never drop what they did not parse.** A version that rewrites a file keeps every
  unknown field and every unknown trailing field of a line, as it found them.
- Enumerated values are closed, except the run `kind`: a kind this version does not know is read
  as "other" and kept.

## Paths

| Path | Content |
|---|---|
| `state.lock` | empty; `flock` shared by every command, exclusive during a conversion of the versioned layer |
| `locks/gate.lock`, `locks/branch/<key>.lock`, `locks/worktree/<key>.lock` | empty; `flock` exclusive while an atelier holds the lock; a branch lock is held shared by `tmp` and by `check` in the branch's worktree, exclusive by `clean`, `follow` and `start` |
| `locks/…/<same name>.procs` | the lock record, below |
| `locks/update/…/<same name>.lock` | empty; `flock` exclusive for the moment a shared holder rewrites the record |
| `runs/<run id>/run.toml` | written when a run starts |
| `runs/<run id>/result` | written atomically when a run ends; absent for a run that was killed |
| `runs/<run id>/<step key>.log` | a step's output |
| `runs/<run id>/coalitions` | macOS: one line per step job of the run and of every run nested in it (below) |
| `probes/coalition-<uid>.toml` | macOS: the macOS version on which the coalition probe passed for this user (below) |
| `hooks/<session id>` | the stop hook's counts and warnings for one harness session (below) |
| `branches/<key>.toml` | one record per branch |
| `tmp/<lifetime id>/<entry id>.toml` | one registered temp folder |
| `versioned/` | the versioned layer, with its format number in `versioned/format`; only the version pinned on the integration branch uses it |
| `empty-hooks/` | an empty folder, created when missing: the hooks folder of the receiving side of the promotion's push; anything in it makes the push refuse |

`<key>` is a branch name encoded by `names::key`. A worktree key is `@main` for the main checkout,
otherwise the encoded name of the worktree's git folder relative to the common git folder
(`worktrees%2F<name>`). A worktree key longer than 200 bytes is cut to 183 bytes and ends in `~`
and the 16 hex digits of the FNV-1a (64-bit) hash of the uncut name.

## Run ids and markers

A run id is the UTC minute and 8 random bytes: `YYYYMMDDTHHMMZ-` and 16 lowercase hex digits
(31 bytes). Ids are unique across every clone on a machine, because the marker scan covers the
whole machine.

Every process of a step carries, set last so that no step environment can replace them:

- `ATELIER_RUN=<run id>`;
- `ATELIER_RUNS=<id>:<id>:…`: the ids of the runs it is nested in, outermost first, then its own
  run's id. An atelier started inside a step appends its own id to the list it inherits.

A step may also be given `ATELIER_RUN_ID=<run id>`: the id the `atelier check` it starts takes
for its run when that id is free (the gate names its inner check's run this way). No other step
sees it.

A process belongs to run `X` when `ATELIER_RUN=X`, or when `X` is an element of `ATELIER_RUNS`.
So an outer run, the gate for one, finds what an inner atelier left behind.

On macOS every process of a step also carries `ATELIER_COALITIONS=<path>:<path>:…`: the absolute
paths of the coalitions files (below) of the runs it is nested in, then its own run's, whatever
state each run is in. A step cannot set any of the three variables (refused, 2).

## Coalitions (macOS)

On macOS each step is a launchd job with its own resource coalition, which every
process the step starts keeps (after `setsid`, a double fork or `env -i` too). There a process
belongs to run `X` when its coalition is listed in `runs/X/coalitions`, and also, as everywhere,
when it carries `X`'s marker: the end-of-run scan and the lock's next taker look for both, so
either catches a survivor. Members of the coalitions are stopped; marked processes outside them
are named and not signalled. A coalition is written
`<coalition id>@<boot session uuid>` (`kern.bootsessionuuid`): coalition ids only grow within a
boot, and a coalition of another boot has no members.

`runs/<run id>/coalitions` is text, one line per job, fields separated by one space:

```text
<coalition> <launchd label> [<later fields>...]
```

A step's line is appended before its program runs, to its run's file and to every outer run's
file: those its `ATELIER_COALITIONS` names (any state; only an existing file named `coalitions`
in a run folder with `run.toml`), and those of the runs named in its `ATELIER_RUNS` whose folder
is in the same state. So an outer run also finds what a nested atelier left when that atelier
died. Lines this version cannot read are skipped. A run without the file (Linux, or an older
version) is looked for by its markers only.

The job's label is `atelier.<run id>.<n>` (a step) or `atelier.probe.<pid>.<hex>` (the probe), in
`gui/<uid>` or `user/<uid>`. Its files (spec, FIFOs, plist, launchd's log) are in
`<user temp>/atelier-jobs/<label>/`, where `<user temp>` is `getconf DARWIN_USER_TEMP_DIR`, never in
the state. That folder is created before `launchctl bootstrap` with a file `owner`, `<pid> <start
time>\n` of the atelier process that starts the job (start time as in lock records, or `-`), and
is removed after `launchctl bootout`. Loaded jobs that atelier processes which died left behind
are removed (`launchctl bootout`) when a run starts, when it ends, and when the lock's next taker
finds nothing alive: every loaded `atelier.*` job whose process does not run and whose folder's
owner is gone; without a folder, a probe job whose pid is gone, or a step job whose run (in the
state doing the cleanup) has a `run.toml` whose process is gone. A job whose process still runs is
left until it ends (its processes are survivors, found as above). Folders of dead owners whose job
is not loaded are removed too.

`probes/coalition-<uid>.toml`: `macos` (`<product version> (<build>)`), `passed` (RFC 3339 UTC),
`atelier_version`. Written when the probe (a job that starts a sleeper with `setsid` and a double
fork, then exits 3) found the sleeper by coalition, stopped it, and read exit status 3 from
`launchctl print`. A record of another macOS version, or none, makes the next run probe again; a
failed probe refuses (4) and writes nothing.

## Stop hook sessions

`hooks/<session id>` is TOML, written by `atelier hook stop` for an input whose `session_id` is
made of ASCII letters, digits, `-`, `_` and `.` (128 bytes at most, not starting with `.`);
removed when it holds nothing.

- `blocks`: a table, condition → the number of blocks in a row for it in the current turn.
- `warning`: an array of tables, newest first, 20 at most: the stops the hook let through while a
  condition remained. `at` (RFC 3339 UTC), `branch`, `condition` (`rebase`, `merge`,
  `uncommitted`, `submodule`, `background`, `compile`, or a later one), `why` (`repeated`: after
  3 blocks in a row; `unfixable`: at once), `detail` (the advice).

## Lock records

`<name>.procs` is text. Each line is fields separated by one space, with no other whitespace:

```text
run <run id> [<later fields>...]
group <pgid> <run id> <leader start time> [<coalition> [<later fields>...]]
```

- `run`: a run that holds, or held, the lock. It stays while processes carrying its marker may
  live: when survivors were found, and when the survivor scan failed.
- `group`: a step's process group, recorded before the step's program runs and removed once the
  group is empty. `<pgid>` is an integer greater than 1. `<leader start time>` is the start time
  of the group's leader (pid == pgid), or `-` when it could not be read:
  - macOS: `<seconds>.<microseconds>` of `p_starttime` (`sysctl` `KERN_PROC_PID`);
  - Linux: `<start time in clock ticks after boot>@<boot id>`: field 22 of `/proc/<pid>/stat` and
    `/proc/sys/kernel/random/boot_id`.
  - `<coalition>` (fifth field, macOS): the step job's coalition, `<id>@<boot session uuid>`
    (above). The group's pgid is the job's pid. A line without it, or whose fifth field is not a
    coalition, is a plain process group.
- Empty lines are ignored. Any other line makes the record unreadable: the lock refuses (4), and
  the file is left as it is.

Whoever takes the lock exclusive next reads the record and refuses (4) while any of these
live, naming them. It never signals them. Shared holders of a branch lock run beside each other:
one never refuses for what the record lists, and changes only its own lines, re-reading the file
and writing it back under `locks/update/…/<same name>.lock`.

- A recorded group is alive and is still the one atelier started. If a process with pid == pgid
  exists and its start time differs from the recorded one, the pgid was reused: the line is not
  ours and is dropped. If no such process exists but the group has members, the group is ours (a
  pgid is not reallocated while its group has members). On Linux, a recorded boot id other than
  the current one means the group is gone.
- A process is marked with a recorded run id (`run` lines and the run ids of `group` lines).
- macOS: a `group` line with a coalition lives while that coalition has a live member; a recorded
  run with a coalitions file lives while any coalition listed there has one.

When nothing lives, the record is removed.

## `run.toml`

| Field | Type | Meaning |
|---|---|---|
| `id` | string | the run id |
| `kind` | string | `check`, `check-compile`, `check-only` (a red gate's failures re-run by name), `gate`, `test-full`, `test-narrow`, `follow`, `tmp`, or a later kind |
| `command` | string | the command, for people |
| `lock` | string | the lock's machine key: its path under `locks/` without the extension (`gate`, `branch/<key>`, `worktree/<key>`) |
| `worktree` | path | the worktree the run started in |
| `branch` | string, optional | the branch that worktree was on |
| `started` | string | RFC 3339 UTC |
| `pid` | integer | the atelier process that runs it |
| `pid_start` | string, optional | that process's start time, as in lock records: a run without `result` is live only while a process with this pid and start time exists |
| `atelier_version` | string | |
| `runs` | array of strings | the run's `ATELIER_RUNS` list |

## `result`

| Field | Type | Meaning |
|---|---|---|
| `id`, `kind`, `command`, `branch`, `worktree`, `started`, `atelier_version` | | as in `run.toml` (`branch` may be set by the command, for the gate) |
| `outcome` | string | `passed`, `failed` or `interrupted` |
| `tree` | string, optional | the tree id tested |
| `commit` | string, optional | `HEAD` when the tree was recorded |
| `uncommitted` | boolean, optional | the tree differs from `HEAD`'s |
| `platform` | string, optional | `<os>-<arch>` the steps ran on |
| `machine` | string, optional | the `machines.toml` name the steps ran on; absent for this computer |
| `ended` | string | RFC 3339 UTC |
| `signal` | string, optional | `SIGTERM`, `SIGINT` or `SIGHUP` when a stop signal ended the run |
| `failures` | array of strings | why the run failed |
| `survivors` | array of strings | processes carrying the run's marker at its end (never signalled), and on macOS live members of its coalitions (stopped) |
| `test_results` | string, optional | the per-test results, relative to the run folder |
| `tests` | table, optional | `passed`, `failed`, `flaky` (failed, then passed when retried), `skipped`, `excluded` |
| `steps` | array of tables | `name`, `status` (`passed`, `failed`, `left-processes`, `interrupted`, `not-started`), `exit`, `signal`, `log`, `left`, `unstoppable` (`<pid> <program>`, or `cannot stop <pid> (owned by <user>)` for a process that refused the signal) |

## Branch records

| Field | Type | Meaning |
|---|---|---|
| `name` | string | the branch; must match the file's key |
| `lifetime` | string | lowercase hex id, new for every `start` |
| `lower` | string | the branch it is built on |
| `lower_is_integration` | boolean | `lower` is the integration branch |
| `lower_lifetime` | string, optional | the lower branch's lifetime; present exactly when `lower_is_integration` is false |
| `base` | string | full commit id (40 or 64 lowercase hex digits) of what it is built on |
| `state` | string | `active`, `merged` or `cleaned` |
| `pending` | table, optional | a `follow` not settled yet: `target` and `old_base`, full commit ids |
| `merged` | table, optional | what `merge` recorded when it set `state` to `merged`, and `clean` trusts (below); removed when the state becomes `cleaned` |
| `starting` | table, optional | a `start` not finished yet: `worktree` (absolute path, UTF-8, where the branch is checked out) and `new_worktree` (boolean: `start` creates that worktree, or switches the detached worktree it ran in) |

Commit ids are checked when a record is read and when it is written.

Lifecycle, the same for every version:

- `start` writes a new record, with a new lifetime, only when the branch does not exist and no
  `active` record names the old lifetime as its lower; it then replaces the old record whole. Its
  steps, in this order: write the record with `starting` set; create the branch ref at the base
  (`update-ref --no-deref`, expected absent); check the branch out (`git worktree add`, or
  `git switch` in the detached worktree); write the record again without `starting`. A record
  with `starting` is not started: every command that reads records refuses it (`run atelier
  start <branch> again`), and it counts as a dependent of its lower branch.
- A `start` of a name whose record has `starting` (atelier died between the steps, or a step
  failed) first reconciles it, under the branch's lock. The branch is *in use* when a
  registered worktree has it checked out (`git worktree list`, prunable ones included) or when
  the git folder of any worktree names it in `rebase-merge/head-name`, `rebase-apply/head-name`
  or `BISECT_START` (git's own sense: `git branch -D` refuses such a branch):
  - a registered worktree has the branch checked out and nothing else uses it: if it is the
    recorded worktree, the ref exists, and this start asks for the same lower branch and place,
    `starting` is cleared and the start is finished;
  - otherwise, while the branch is in use: refused (4), changing nothing; the message gives the
    command that finishes the start (`atelier start <branch> --path <path>`, or `cd <path> &&
    atelier start <branch>` for a start in place) and the ones that free the branch (`git
    worktree remove <path>` for the worktree the start created, `git -C <path> switch
    --detach` for any other, finishing or aborting the rebase or bisect);
  - not in use, and the ref is absent: the record is removed;
  - not in use, and the ref holds the recorded base: the ref is deleted (expected value the
    base), then the record is removed;
  - not in use, and the ref holds another commit: refused (4), ref and record kept; the message
    says how to keep that work (check the branch out again where the start put it and finish
    the start, or `git branch <new-name> <branch>` first) and how to undo the start (`git
    branch -D <branch>`, then `atelier start <branch> --abandon`).

  After an undo the new start proceeds as if the name were free. The record a rolled-back start
  replaced is not restored: `start` replaces only a record that no active record names as its
  lower, so nothing depended on it. A step that fails while `start` is still running is undone
  the same way at once (the replaced record is then restored), except that a ref this `start`
  did not create is never deleted. A branch ref that is in use is never deleted.
- `atelier start <branch> --abandon` only undoes an unfinished start: under the branch's lock,
  when the record has `starting`, the branch is not in use and its ref is absent or holds the
  recorded base, it deletes the ref (expected value the base) and removes the record. It
  refuses (4), changing nothing, otherwise, saying why.
- `follow` writes `pending` before its rebase and settles it from ancestry alone. The rebase
  is the one step of a `follow` run held under the branch's lock, so its process group is
  recorded (`locks/branch/<key>.procs`) before git starts, and the next command that takes the
  lock refuses while that group lives, even when atelier died meanwhile. A rebase of the branch
  open in any worktree, whether `follow` started it or not, makes every reader refuse.
- No command repairs a base the branch no longer contains: it refuses (4) until the worker
  restores the branch. Once the lower
  branch is merged (or gone, with its base in the integration branch), `follow` sets `lower` to
  the integration branch (`lower_is_integration = true`, no `lower_lifetime`).
- A record is removed only in state `cleaned`, and only when no `active` record names it, by
  name and lifetime, as its lower.
- A writer holds the branch's lock (`locks/branch/<key>.lock`) while it reads and writes the
  record.

## Merge facts

The `merged` table of a branch record is written by `merge` in the same write that sets `state` to
`merged` (`records::MergeFacts::record`), and removed by `clean` in the write that sets `cleaned`
(`MergeFacts::retire`). A record in state `merged` without it grants `clean` nothing. `merge`
makes that write only while the branch is at the commit it merged and approved no other: a
branch that moved or was approved again meanwhile keeps its record `active`, as it is.

| Field | Type | Meaning |
|---|---|---|
| `commit` | string | full commit id the branch was merged at: its tip when `approve` recorded it |
| `worktree` | string | the one worktree `clean` may delete for this branch: absolute, canonical, UTF-8, recorded by `approve` |
| `dev`, `ino` | integers | that worktree folder's file-system identity when `approve` recorded it |

`clean` deletes only a worktree that is still that folder (identity and canonical path), with the
branch checked out at `commit`; it deletes the branch only while the ref still holds `commit`.

## Temp-folder registry entries

| Field | Type | Meaning |
|---|---|---|
| `path` | string | the folder's canonical path (no symbolic link in it); UTF-8 only |
| `dev`, `ino` | integers | its file-system identity when registered |

The entry's file name is `<entry id>.toml` (lowercase hex). `tmp` makes the folder at
`<temp dir>/atelier-<uid>/<repo id>/<branch key>/<entry id>/` (`<temp dir>`: `$TMPDIR`, or the
system's temp dir when it is unset, `/tmp` on Linux; `atelier-<uid>`: this user's root, mode 0700,
used only when it is a folder this user owns that others cannot enter; `<repo id>`: the 64-bit FNV-1a hash of the
canonical common git folder, 16 hex digits; `<branch key>`: `names::key` of the branch), or with
`--short` at `/tmp/atl-<entry id>/`, always mode 0700. `clean` removes a folder only when its
registered path has exactly that shape, is canonical and still names a real folder with the
registered identity owned by this user, nothing in it is on another device (a mount), and its
`leftovers` file (one line per thing created outside the folder) is empty or absent; it deletes
through folder descriptors and never enters another device.

## The versioned layer's format number

`versioned/format` holds one decimal number and a newline. It is created whole (written, flushed,
hard-linked into place, folder flushed). A conversion from format `n` writes format `n + 1` into
`versioned.next/`, flushes it, and swaps the two folders in one rename (`renamex_np` with
`RENAME_SWAP` on macOS, `renameat2` with `RENAME_EXCHANGE` on Linux). A crash leaves the old layer
or the new one whole, and a leftover `versioned.next/` is removed by the next conversion.
