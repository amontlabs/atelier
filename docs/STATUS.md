# status and the conflict forecast

`atelier status [--json]` prints what the other commands recorded. It writes nothing, converts no state and takes no exclusive or machine lock: git runs with `GIT_OPTIONAL_LOCKS=0` and the forecast's objects go to a temporary folder. (Like every command it opens `.git/atelier/` and takes `state.lock` shared, which creates the empty folder layout in a clone atelier never ran in.) It exits 0 whatever it finds; only a usage error exits 2. Outside an atelier project the dispatcher refuses (4).

Preflight is reported, not enforced: each check is a line, `ok` or `FAILED`, and a failure does not stop the others. The records below are read only when preflight could open the state and load the settings. The versioned records (approvals, merges, promotion) are read only when the version pinned on the integration branch is this one and its format is current.

## Sections

Each section shows at most 10 entries, newest first where that means anything, then `... N more not shown`. Lines the reader must not miss (`record unreadable` in `must follow`, `N pairs not forecast`) come first and are never cut. `--json` prints `{"sections": [{"name", "lines", "omitted"}]}`.

| Section | Content |
|---|---|
| `preflight` | project, state, version, settings, rules, the version pinned on the integration branch, the state format |
| `main` | the integration branch's tip; the main checkout's branch and tracked changes; the promotion record (below) |
| `approvals not yet merged` | one line per approval, with a note when the branch moved or is gone |
| `merges since the last clean` | `merges.tsv` lines whose branch record is still `merged` (`clean` has not retired it) |
| `last runs` | the newest runs: id, kind, outcome, branch, platform, machine, tree. A run with no result is `RUNNING` or `NO RESULT (killed)`; an unreadable result, or one whose tree is not a git object id, is `<id> unreadable: ...`. The gate's run has no platform (its platforms are in `merges.tsv`). Workers' `check` and `test` runs appear here, with their branch, and nowhere else |
| `platforms` | per platform of the committed settings, the last gate that ran it, with the merged commit: `PASS` with the machine, `WAIVED` with the reason (never `PASS`), or `FAIL` (its failure line names it; nothing was merged). Then `last passed: ...` when the newest is not a pass. A gate that ended on a conflict or refusal ran no platform and does not count. `no gate has run it` otherwise |
| `machines` | the table `atelier machines` prints (probes every machine; the machine lock is only read) |
| `must follow` | active branches that are not current (the message `approve` and `merge` refuse with), a follow that cannot be settled, a start that did not finish, an unreadable record |
| `worktrees with no merge record` | linked worktrees (not the main checkout, not the gate) whose branch has no `merged` record naming that worktree: `active`, `no record`, `detached`. `clean` never touches them |
| `flaky tests` | a test that failed and later passed on the same tree and platform, in full runs only |
| `conflict forecast` | the pairs below |

**Promotion record** (in `main`):
- `running (merge or promote in progress)`, with no advice, when the gate lock is held or the push's process group is alive.
- `pending`.
- `INTERRUPTED`: a push started and did not end, the ref is at `OLD`, and the main checkout's check d or e fails (`promote::interrupted`).
- `stale`: the branch moved.
- "happened but not recorded".
- `promotion: unreadable: ...` for a record that cannot be read; the lines above it stay.

Run folders are read newest first, at most 300.

## Flaky tests

Read from each full run's `tests.tsv` (workspace, test, verdict, seconds). A run counts when its kind is `check`, `test-full` or `gate`, it was not interrupted, and it names its tree and platform (a local run with no recorded platform is this computer's). `check --compile-only`, `check --only` (bisection) and narrow `test` runs never count. A test is flaky when, for one tree and platform, a failing verdict is followed by a passing one in a later run. A test that fails on one tree and passes on a changed tree is a fix, not a flaky test.

## Conflict forecast

`src/forecast.rs` runs `git merge-tree --write-tree --name-only` with the gate's merge configuration (so the owner's rerere, renormalize or attribute files cannot change the answer). It covers each open branch (active record, finished start, branch exists) against the integration branch, and every pair of open branches.

- Attributes come from the integration branch's tip (`GIT_ATTR_SOURCE`, git 2.40 or later; with older git, the tip's `.gitattributes` files go to a temporary work tree with an empty index), so every checkout gets the same answer. A pair is forecast alone: it cannot see an attribute change that one branch of a batch hands to the next, which the gate merges in sequence. Such a forecast can be wrong either way.
- Before `merge-tree`, the gate's read-only merge-driver check runs (`gate::merge_driver_uses`: `git check-attr merge` on the paths both sides changed, with the tip's and both branches' attributes, and `merge.default`; `union` and any configured driver). A pair the gate would refuse is listed as `a x b: would be refused: <path> uses merge driver <name>`, and `merge-tree` does not run, so no configured driver ever runs.
- A conflict only in regenerated paths is not listed when the gate would settle it. For a branch whose own commits change `.atelier/`, `AGENTS.md` or an import file (`gate::changes_rules`), nothing is filtered. Otherwise only paths that both the tip's and the branch tip's committed settings name in a `merge.after` hook with `commit` are filtered. A pair filters only paths both branches would let the gate settle.

It writes no file in the repository: git gets a temporary object folder (`$TMPDIR/atelier-forecast-<pid>-<hex>`) with the repository's objects as an alternate. (git may refresh the modification time of an object it finds again.) The next forecast removes a folder whose process is gone. `status` makes at most 300 calls on four threads; the rest are counted as "pairs not forecast", as are pairs git cannot compute (unrelated histories) and pairs whose work panicked.

`merge` runs it after the machine check, for pairs involving the batch only (each branch against the integration branch and every other open branch, no limit). It prints `warning: forecast: ...` for each conflict or refusal, then `warning: forecast: N pairs not forecast` when a pair could not be forecast. It refuses nothing and does not change the exit code.
