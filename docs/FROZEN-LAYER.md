# The frozen layer

`.git/atelier/` holds the state every worktree of a clone shares. Every atelier version reads and writes it at the same time (a worker on an upgrade branch runs its own pinned version), so its formats never break.

## Compatibility

- Readers ignore fields they do not know. A later version may add fields; it never removes, renames or redefines one.
- Readers refuse (4, naming the file) what they cannot parse: not TOML, a missing required field, a wrong type, a lock-record line of an unknown shape.
- Writers keep every unknown field, and every unknown trailing field of a line.
- Enumerated values are closed, except run `kind`: an unknown kind is read as "other" and kept.

## Paths

| Path | Content |
|---|---|
| `state.lock` | `flock`: shared by every command, exclusive while the versioned layer converts |
| `locks/gate.lock`, `locks/branch/<key>.lock`, `locks/worktree/<key>.lock` | `flock`, exclusive while held. A branch lock is shared by `tmp` and `check`, exclusive for `clean`, `follow`, `start` |
| `locks/…/<name>.procs` | the lock record |
| `locks/update/…/<name>.lock` | `flock` while a shared holder rewrites the record |
| `runs/<run id>/run.toml` | written when a run starts |
| `runs/<run id>/result` | written atomically when a run ends; absent if killed |
| `runs/<run id>/<step key>.log` | a step's output |
| `runs/<run id>/coalitions` | macOS: the run's step jobs |
| `probes/coalition-<uid>.toml` | macOS: where the coalition probe passed |
| `hooks/<session id>` | the stop hook's counts for one harness session |
| `branches/<key>.toml` | one record per branch |
| `tmp/<lifetime id>/<entry id>.toml` | one registered temp folder |
| `versioned/` | the versioned layer; its format number in `versioned/format` ([MERGE.md](MERGE.md#versioned-layer-format-1)) |
| `empty-hooks/` | must stay empty: the hooks folder for the promotion push |

`<key>` is the branch name encoded by `names::key`. A worktree key is `@main` for the main checkout, else the encoded path of the worktree's git folder relative to the common one (`worktrees%2F<name>`). A key over 200 bytes is cut to 183 bytes plus `~` and the 16-hex-digit FNV-1a 64-bit hash of the full name.

## Run ids and markers

A run id is `YYYYMMDDTHHMMZ-` (UTC minute) and 16 lowercase hex digits of random bytes, unique across the machine.

Every process of a step carries these, set last (a step cannot set them: refused, 2):

- `ATELIER_RUN=<run id>`
- `ATELIER_RUNS=<id>:<id>:…`: the enclosing runs, outermost first, then its own. A nested atelier appends its id.
- macOS: `ATELIER_COALITIONS=<path>:…`: the coalitions files of those runs.

A process belongs to run `X` when `ATELIER_RUN=X` or `X` is in `ATELIER_RUNS`, so an outer run finds what an inner one left. A step may also get `ATELIER_RUN_ID=<run id>`: the id its `atelier check` takes, when free.

## Coalitions (macOS)

Each macOS step runs as a launchd job with its own resource coalition, which every process it starts keeps. A process also belongs to run `X` when its coalition is listed in `runs/X/coalitions`.

`runs/<run id>/coalitions`: one line per job, space-separated: `<coalition> <launchd label> [<later fields>...]`. A coalition is `<coalition id>@<boot session uuid>`. A step's line is added before its program runs, to its own run's file and every enclosing run's. Unreadable lines are skipped. A run without the file is checked by markers only.

`probes/coalition-<uid>.toml`: `macos` (`<product version> (<build>)`), `passed` (RFC 3339 UTC), `atelier_version`. Missing, or for another macOS version: the next run probes again. A failed probe refuses (4).

## Stop hook sessions

`hooks/<session id>` (TOML), for a `session_id` of ASCII letters, digits, `-`, `_`, `.` (at most 128 bytes, not starting with `.`); removed when empty.

- `blocks`: table, condition → blocks in a row in the current turn.
- `warning`: array, newest first, at most 20, of stops let through with a condition left: `at` (RFC 3339 UTC), `branch`, `condition` (`rebase`, `merge`, `uncommitted`, `submodule`, `background`, `compile`, or later), `why` (`repeated` after 3 blocks, or `unfixable`), `detail`.

## Lock records

`<name>.procs` is text, fields separated by one space:

```text
run <run id> [<later fields>...]
group <pgid> <run id> <leader start time> [<coalition> [<later fields>...]]
```

- `run`: a run that holds or held the lock; kept while processes with its marker may live.
- `group`: a step's process group, recorded before the program runs and removed once empty. `<pgid>` > 1. `<leader start time>` is `-` if unreadable, else:
  - macOS: `<seconds>.<microseconds>` of `p_starttime`;
  - Linux: `<start time in clock ticks after boot>@<boot id>` (field 22 of `/proc/<pid>/stat`, `/proc/sys/kernel/random/boot_id`).
- `<coalition>` (macOS): the step job's coalition. Without it, a plain process group.
- Empty lines are ignored. Any other line makes the record unreadable: the lock refuses (4) and the file is left as is.

The next exclusive taker refuses (4), naming them, while any of these live; it never signals them:

- a recorded group, still the one atelier started (a different start time on the leader means a reused pgid: dropped; a different Linux boot id means gone);
- a process marked with a recorded run id;
- macOS: a live member of a recorded coalition, or of any coalition in a recorded run's coalitions file.

Shared holders change only their own lines. When nothing lives, the record is removed.

## `run.toml`

| Field | Type | Meaning |
|---|---|---|
| `id` | string | the run id |
| `kind` | string | `check`, `check-compile`, `check-only`, `gate`, `gate-bisect`, `test-full`, `test-narrow`, `follow`, `tmp`, or later |
| `command` | string | for people |
| `lock` | string | lock key: `gate`, `branch/<key>`, `worktree/<key>` |
| `worktree` | path | where the run started |
| `branch` | string, optional | that worktree's branch |
| `started` | string | RFC 3339 UTC |
| `pid` | integer | the atelier process |
| `pid_start` | string, optional | its start time, as in lock records; a run without `result` is live only while this process exists |
| `atelier_version` | string | |
| `runs` | array of strings | the run's `ATELIER_RUNS` |

## `result`

| Field | Type | Meaning |
|---|---|---|
| `id`, `kind`, `command`, `branch`, `worktree`, `started`, `atelier_version` | | as in `run.toml` |
| `outcome` | string | `passed`, `failed` or `interrupted` |
| `tree` | string, optional | tree id tested |
| `commit` | string, optional | `HEAD` when the tree was recorded |
| `uncommitted` | boolean, optional | tree differs from `HEAD`'s |
| `platform` | string, optional | `<os>-<arch>` |
| `machine` | string, optional | `machines.toml` name; absent for this computer |
| `ended` | string | RFC 3339 UTC |
| `signal` | string, optional | `SIGTERM`, `SIGINT` or `SIGHUP` that ended the run |
| `failures` | array of strings | why the run failed |
| `survivors` | array of strings | processes left at the end: marked ones (never signalled), macOS coalition members (stopped) |
| `test_results` | string, optional | per-test results file, relative to the run folder |
| `tests` | table, optional | `passed`, `failed`, `flaky`, `skipped`, `excluded` |
| `steps` | array of tables | in the order steps ended (tests and project steps run at once): `name`, `status` (`passed`, `failed`, `left-processes`, `interrupted`, `not-started`), `exit`, `signal`, `log`, `left`, `unstoppable` (`<pid> <program>`, or `cannot stop <pid> (owned by <user>)`) |

## Branch records

| Field | Type | Meaning |
|---|---|---|
| `name` | string | the branch; must match the file's key |
| `lifetime` | string | lowercase hex id, new for every `start` |
| `lower` | string | the branch it builds on |
| `lower_is_integration` | boolean | `lower` is the integration branch |
| `lower_lifetime` | string, optional | present exactly when `lower_is_integration` is false |
| `base` | string | full commit id (40 or 64 lowercase hex) of what it builds on |
| `state` | string | `active`, `merged` or `cleaned` |
| `pending` | table, optional | an unsettled `follow`: `target`, `old_base` (full commit ids) |
| `merged` | table, optional | the merge facts below; removed at `cleaned` |
| `starting` | table, optional | an unfinished `start`: `worktree` (absolute UTF-8 path) and `new_worktree` (boolean: `start` created it, or switched the detached worktree it ran in) |

A writer holds the branch lock. Commit ids are checked on read and write.

**Lifecycle**, the same for every version:

- **`start`** writes a new record only when the branch does not exist and no `active` record names the old lifetime as its lower. Order: record with `starting`; branch ref at the base; checkout; record without `starting`. Commands refuse a record with `starting` (`run atelier start <branch> again`).
- **Restarting an unfinished `start`**, under the branch lock. The branch is *in use* when a worktree has it checked out or a rebase or bisect of it is open.
  - Checked out where recorded, ref exists, same lower and place: finished.
  - Otherwise in use: refused (4), naming the commands that finish the start or free the branch.
  - Not in use, ref absent or at the base: ref and record removed, then the start proceeds.
  - Not in use, ref at another commit: refused (4), naming how to keep that work or undo the start (`git branch -D <branch>`, then `atelier start <branch> --abandon`).
  A step that fails during `start` is undone at once and the replaced record restored. A ref `start` did not create, or one in use, is never deleted.
- **`atelier start <branch> --abandon`** undoes only an unfinished start whose branch is not in use and whose ref is absent or at the base. Otherwise refused (4).
- **`follow`** writes `pending` before its rebase and settles it from ancestry. The rebase's process group is recorded under the branch lock. Any open rebase of the branch makes every reader refuse.
- **A base the branch no longer contains** is refused (4) until the worker restores the branch. Once the lower branch is merged, `follow` sets `lower` to the integration branch.
- A record is removed only when `cleaned` and no `active` record names it as its lower.

## Merge facts

The `merged` table. `merge` writes it with `state = "merged"`, only while the branch is still at the merged commit with no other approval; `clean` removes it with `cleaned`. A `merged` record without it lets `clean` delete nothing.

| Field | Type | Meaning |
|---|---|---|
| `commit` | string | full commit id merged; the tip `approve` recorded |
| `worktree` | string | the one worktree `clean` may delete: absolute, canonical, UTF-8 |
| `dev`, `ino` | integers | that folder's identity at `approve` |

`clean` deletes the worktree only if it is still that folder with the branch at `commit`, and the branch only while its ref holds `commit`.

## Temp-folder entries

| Field | Type | Meaning |
|---|---|---|
| `path` | string | canonical UTF-8 path |
| `dev`, `ino` | integers | identity when registered |

File name `<entry id>.toml` (lowercase hex). The folder is `<temp dir>/atelier-<uid>/<repo id>/<branch key>/<entry id>/`, or with `--short` `/tmp/atl-<entry id>/`, mode 0700. `<temp dir>` is `$TMPDIR`, else the system's; `atelier-<uid>` is used only if this user owns it and others cannot enter; `<repo id>` is the FNV-1a 64-bit hash of the canonical common git folder (16 hex digits).

`clean` removes the folder only when its path has that shape, it is still the registered folder owned by this user, nothing inside is a mount, and its `leftovers` file (one line per thing made outside the folder) is empty or absent.

## Versioned format number

`versioned/format` holds one decimal number and a newline, written whole. A conversion from `n` writes `n + 1` into `versioned.next/` and swaps the two folders atomically. A crash leaves one layer whole; the next conversion removes a leftover `versioned.next/`.
