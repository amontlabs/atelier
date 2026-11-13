# status and the conflict forecast (module 3.8)

DESIGN.md §3 `status` and §8.1. `atelier status [--json]` reads what the other commands recorded
and prints it. It never converts the state, takes no exclusive lock and no machine lock, and
writes nothing: git runs with `GIT_OPTIONAL_LOCKS=0` (no index refresh), and the forecast's objects
go to a temporary folder. (It opens `.git/atelier/` and takes `state.lock` shared, as every command
does, which creates that folder's empty layout in a clone atelier never ran in.) It exits 0
whatever it finds; only a usage error exits 2. Outside an atelier project the dispatcher refuses
(4), as for every command.

The preflight is reported, not enforced: each check is a line, `ok` or `FAILED`, and a failure
does not stop the others. The records below are read only when the preflight got far enough to
open the state and load the settings; the versioned records (approvals, merges, promotion) only
when the version pinned on the integration branch is this one and its format is current.

## Sections

Each section shows at most 10 entries, newest first where that means anything, then
`... N more not shown`. `--json` prints the same sections as `{"sections": [{"name", "lines",
"omitted"}]}`.

| Section | Content |
|---|---|
| `preflight` | project, state, version, settings, rules, the version pinned on the integration branch, the state format |
| `main` | the integration branch's tip; the main checkout's branch and tracked changes; the promotion record: `pending`, `INTERRUPTED` (a push started and did not end, the ref is at `OLD`, and the main checkout's check d or e fails: `promote::interrupted`), `stale` (the branch moved) or "happened but not recorded" |
| `approvals not yet merged` | one line per approval, with a note when the branch moved or is gone |
| `merges since the last clean` | `merges.tsv` lines whose branch record is still `merged` (`clean` has not retired it) |
| `last runs` | the newest runs: id, kind, outcome, branch, platform, machine, tree; a run without a result is `RUNNING` or `NO RESULT (killed)` (pid and start time, as the lock records do) |
| `platforms` | per platform of the integration branch's committed settings: the newest of its full runs (`check`, `test-full`, `gate`) and the merges that passed it, with the machine; then the waivers recorded since its last pass |
| `machines` | the table `atelier machines` prints (probes every machine; the machine lock is only read) |
| `must follow` | active branches that are not current (`Stack::currency`, the message `approve` and `merge` refuse with), a follow that cannot be settled, a start that did not finish, an unreadable record |
| `worktrees with no merge record` | linked worktrees (not the main checkout, not the gate) whose branch has no `merged` record naming that worktree: `active`, `no record`, `detached`. `clean` never touches them |
| `flaky tests` | a test that failed and later passed on the same tree and platform, in full runs only |
| `conflict forecast` | the pairs below |

Run folders are read newest first, at most 300; older ones are not read.

## Flaky tests

From each full run's `tests.tsv` (workspace, test, verdict, seconds). A run counts when its kind is
`check`, `test-full` or `gate`, it was not interrupted, and it names its tree and platform (a local
run without a recorded platform is this computer's). `check --compile-only`, `check --only`
(bisection) and narrow `test` runs never count. A test is flaky when, for one tree and platform,
a failing verdict is followed by a passing one in a later run. The same test failing on one tree
and passing on a changed tree is a fix, not a flaky test.

## Conflict forecast (DESIGN §8.1)

`src/forecast.rs`: `git merge-tree --write-tree --name-only` (the gate's merge configuration, so a
rerere, a renormalize or an attribute file of the owner cannot change the answer) for each open
branch (an active record whose start finished and whose branch exists) against the integration
branch, and for every pair of open branches. A conflict only in paths the project's `merge.after`
hooks regenerate is not listed, as the gate treats it. It writes no file in the repository: git is
given a temporary object folder with the repository's objects as an alternate. (git may refresh the
modification time of an object it finds again.) At most 300 calls; the rest are counted as "pairs
not forecast", as are pairs git cannot compute (unrelated histories). It runs in four threads.

`merge` runs it after its per-branch preconditions and prints a `warning: forecast: ...` line for
each conflict that involves a branch of the batch. It refuses nothing and does not change the exit
code.
