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
| `locks/gate.lock`, `locks/branch/<key>.lock`, `locks/worktree/<key>.lock` | empty; `flock` exclusive while an atelier holds the lock |
| `locks/…/<same name>.procs` | the lock record, below |
| `runs/<run id>/run.toml` | written when a run starts |
| `runs/<run id>/result` | written atomically when a run ends; absent for a run that was killed |
| `runs/<run id>/<step key>.log` | a step's output |
| `branches/<key>.toml` | one record per branch |
| `tmp/<lifetime id>/<entry id>.toml` | one registered temp folder |
| `versioned/` | the versioned layer, with its format number in `versioned/format`; only the version pinned on the integration branch uses it |

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

A process belongs to run `X` when `ATELIER_RUN=X`, or when `X` is an element of `ATELIER_RUNS`.
So an outer run, the gate for one, finds what an inner atelier left behind. A step cannot set
either variable (refused, 2).

## Lock records

`<name>.procs` is text. Each line is fields separated by one space, with no other whitespace:

```text
run <run id> [<later fields>...]
group <pgid> <run id> <leader start time> [<later fields>...]
```

- `run`: a run that holds, or held, the lock. It stays while processes carrying its marker may
  live: when survivors were found, and when the survivor scan failed.
- `group`: a step's process group, recorded before the step's program runs and removed once the
  group is empty. `<pgid>` is an integer greater than 1. `<leader start time>` is the start time
  of the group's leader (pid == pgid), or `-` when it could not be read:
  - macOS: `<seconds>.<microseconds>` of `p_starttime` (`sysctl` `KERN_PROC_PID`);
  - Linux: `<start time in clock ticks after boot>@<boot id>`: field 22 of `/proc/<pid>/stat` and
    `/proc/sys/kernel/random/boot_id`.
- Empty lines are ignored. Any other line makes the record unreadable: the lock refuses (4), and
  the file is left as it is.

Whoever takes the lock next reads the record and refuses (4) while any of these live, naming
them. It never signals them.

- A recorded group is alive and is still the one atelier started. If a process with pid == pgid
  exists and its start time differs from the recorded one, the pgid was reused: the line is not
  ours and is dropped. If no such process exists but the group has members, the group is ours (a
  pgid is not reallocated while its group has members). On Linux, a recorded boot id other than
  the current one means the group is gone.
- A process is marked with a recorded run id (`run` lines and the run ids of `group` lines).

When nothing lives, the record is removed.

## `run.toml`

| Field | Type | Meaning |
|---|---|---|
| `id` | string | the run id |
| `kind` | string | `check`, `check-compile`, `gate`, `test-full`, `test-narrow`, `follow`, `tmp`, or a later kind |
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
| `survivors` | array of strings | processes carrying the run's marker at its end |
| `test_results` | string, optional | the per-test results, relative to the run folder |
| `tests` | table, optional | `passed`, `failed`, `flaky` (failed, then passed when retried), `skipped`, `excluded` |
| `steps` | array of tables | `name`, `status` (`passed`, `failed`, `left-processes`, `interrupted`, `not-started`), `exit`, `signal`, `log`, `left`, `unstoppable` |

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
  failed) first reconciles it, under the branch's lock:
  - a registered worktree has the branch checked out (`git worktree list`, prunable ones
    included): if it is the recorded worktree, the ref exists, and this start asks for the same
    lower branch and place, `starting` is cleared and the start is finished; otherwise it
    refuses (4), changing nothing, and says how to finish it or undo it (remove the worktree);
  - no worktree has it, and the ref is absent: the record is removed;
  - no worktree has it, and the ref holds the recorded base: the ref is deleted (expected value
    the base), then the record is removed;
  - no worktree has it, and the ref holds another commit: refused (4), ref and record kept; the
    message says to delete the branch by hand if nothing in it is needed.

  After an undo the new start proceeds as if the name were free. The record a rolled-back start
  replaced is not restored: `start` replaces only a record that no active record names as its
  lower, so nothing depended on it. A step that fails while `start` is still running is undone
  the same way at once (the replaced record is then restored), except that a ref this `start`
  did not create is never deleted. A branch ref that a registered worktree has checked out is
  never deleted.
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

## Temp-folder registry entries

| Field | Type | Meaning |
|---|---|---|
| `path` | string | the folder's canonical path (no symbolic link in it); UTF-8 only |
| `dev`, `ino` | integers | its file-system identity when registered |

## The versioned layer's format number

`versioned/format` holds one decimal number and a newline. It is created whole (written, flushed,
hard-linked into place, folder flushed). A conversion from format `n` writes format `n + 1` into
`versioned.next/`, flushes it, and swaps the two folders in one rename (`renamex_np` with
`RENAME_SWAP` on macOS, `renameat2` with `RENAME_EXCHANGE` on Linux). A crash leaves the old layer
or the new one whole, and a leftover `versioned.next/` is removed by the next conversion.
