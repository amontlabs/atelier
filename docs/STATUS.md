# status and the conflict forecast

`atelier status [--json]` shows what the other commands recorded. It writes nothing in the repository, converts no state and takes no exclusive or machine lock. It exits 0 whatever it finds; a usage error exits 2. Outside an atelier project the dispatcher refuses (4).

Preflight is reported, not enforced: each check prints `ok` or `FAILED` and the others still run. The versioned records (approvals, merges, promotion) are read only when the version pinned on `main` is this one.

## Sections

Each section shows at most 10 entries, newest first, then `... N more not shown`. Lines that must not be missed (`record unreadable`, `N pairs not forecast`) come first and are never cut. `--json` prints `{"sections": [{"name", "lines", "omitted"}]}`. At most 300 run folders are read.

| Section | Content |
|---|---|
| `preflight` | project, state, version, settings, rules, the pin on `main`, the state format |
| `main` | `main`'s tip; the main checkout's branch and tracked changes; the promotion record |
| `approvals not yet merged` | one line per approval, noting a branch that moved or is gone |
| `merges since the last clean` | `merges.tsv` lines whose branch is still `merged` |
| `last runs` | id, kind, outcome, branch, platform, machine, tree. No result: `RUNNING` or `NO RESULT (killed)`. Bad result: `<id> unreadable: ...`. Workers' `check` and `test` runs appear only here |
| `platforms` | per platform, the last gate that ran it: `PASS` with the machine, `WAIVED` with the reason, or `FAIL`; then `last passed: ...` if that was not a pass; else `no gate has run it` |
| `machines` | the `atelier machines` table |
| `must follow` | active branches that are not current, a follow that cannot be settled, an unfinished start, an unreadable record; not throwaways, nor records whose branch is gone |
| `worktrees with no merge record` | linked worktrees (`active`, `no record`, `detached`) that `clean` will never touch; not a throwaway's |
| `throwaway branches` | each throwaway: its tip and the worktree its start recorded, which `atelier clean <branch> --yes` deletes |
| `records whose branch is gone` | active records whose branch was deleted by hand, which `atelier clean --yes` forgets |
| `flaky tests` | see below |
| `conflict forecast` | see below |

The promotion record shows as one of: `running (merge or promote in progress)`, `pending`, `INTERRUPTED` (see [MERGE.md](MERGE.md#promote)), `stale` (the branch moved), "happened but not recorded", or `promotion: unreadable: ...`.

## Flaky tests

A test is flaky when, for one tree and platform, it fails in one full run and passes in a later one. Full runs are kinds `check`, `test-full` and `gate`, not interrupted. `check --compile-only`, bisection and narrow `test` runs never count. A test that passes after the tree changed is a fix, not flaky.

## Conflict forecast

`git merge-tree --write-tree --name-only` with the gate's merge settings, for each open branch against `main` and every pair of open branches.

- Attributes come from `main`'s tip, so every checkout gets the same answer. A pair is judged alone; an attribute change one batch branch hands to the next is not seen.
- A pair the gate would refuse for a merge driver is listed as `a x b: would be refused: <path> uses merge driver <name>`. No configured driver ever runs.
- A conflict only in regenerated paths the gate would settle is not listed. Branches that change `.atelier/`, `AGENTS.md` or an import file get no such filter.
- Objects go to a temporary folder (`$TMPDIR/atelier-forecast-<pid>-<hex>`), removed by the next forecast once its process is gone.
- `status` makes at most 300 calls; the rest, unrelated histories and failed pairs, count as "pairs not forecast".

`merge` forecasts only the batch's pairs, with no limit. It prints `warning: forecast: ...` per conflict or refusal, then `warning: forecast: N pairs not forecast`. It never refuses or changes the exit code.
