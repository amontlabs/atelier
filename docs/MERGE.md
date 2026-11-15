# approve, merge, promote

## approve

`atelier approve <branch> <commit>`, under the branch's lock. Refused (4) unless `<commit>` is the branch's tip, the branch has no merge commit of its own, it is current (not behind a rewrite of what it builds on, transitively), and it is checked out in exactly one worktree that is neither the main checkout nor the gate.

## merge

`atelier merge <branch>… [--waive <platform> --reason "<words>"]…`, under the gate lock. Each `--waive` takes its own `--reason`, in the same order.

1. **Checks** (refused before anything merges):
   - A pending promotion is settled first (see promote).
   - Each branch: approved, still at the approved commit, same lifetime, current, no merge commit of its own, no rebase open. A stacked branch's lower branch must be earlier in the batch, approved at a commit containing the upper's base.
   - A *rules item* (own commits touch `.atelier/`, `AGENTS.md` or an import file) merges alone. Its committed settings are the batch's; otherwise the integration branch's.
   - Every platform has a machine or a waiver (else exit 3). A waiver names a platform of the old or the merged settings. This computer runs a `run = "check"` platform only with the pinned atelier installed (else 4, naming the install command).
   - A forecast of each batch branch against `main` and every open branch prints `warning: forecast: <branch> x <branch>: <files>` (or `would be refused: …`). It refuses nothing.
2. **The gate** is the worktree `<main checkout>.gate`, detached at `main` (OLD). If it holds a git operation, tracked changes or unignored untracked files, atelier lists them and refuses; it deletes nothing.
3. **Merges.** `git merge --no-ff` of each approved commit, with no hooks, signing, autostash, rerere, renormalization or user attribute files.
   - A path both sides changed that uses a custom merge driver, or git's `union`, refuses (4) before anything merges, naming the path and driver: such drivers settle conflicts silently.
   - A conflict only in *regenerated paths* (named in `regenerates` by a `[[merge.after]]` hook with `commit`, in both the old and merged settings) takes the incoming branch's side; the hook rewrites them in step 4. A rules item never has a conflict settled.
   - Any other conflict aborts, resets the gate and exits 5: `conflict: <branch> with main`, or `with <earlier branches> (earlier in this batch)`, then the files.
4. **After hooks** (`[[merge.after]]`, from the merged settings). A hook with `commit` has its changes committed; any other change is RED. Each settled path must hold what its hook produced, else RED (`merge.after <hook> did not regenerate <path> as merged`). Hand edits to regenerated files are lost. A `regenerates` hook must always rewrite its files; one that skips work by timestamp fails here.
5. **Platforms**, from the merged settings, all at once (below). `run = "check"` runs the merged commit's pinned `atelier check` in the gate. Another command runs after the `check.prepare` hooks and the platform's `prepare`. A waived platform prints `WAIVED (<words>)`.
6. **RED:** exit 1, `main` unmoved, the gate kept at the merge for inspection. With more than one branch, bisection follows.
7. **GREEN:** the promotion record is written and promotion runs. It prints the GREEN line, `merged <branch> <commit>` per branch, and `rules changed` when a rules item landed.

A branch that moved past its approved commit, or was approved again, still has the approved commit merged, but stays `active`: `<branch> moved to <tip> after its approval at <commit>: its later commits are not merged; it stays active (atelier follow in its worktree, then approve it again)`. `clean` deletes nothing of it.

### Platforms on machines

Machines are chosen as `atelier test` chooses them ([MACHINES.md](MACHINES.md)). Without a machines file, only this computer's platform has one.

- Step 1 only probes. Step 5 runs every platform at once.
- This computer runs its platform in the gate, under the machine lock (`~/.local/state/atelier/machine.lock`). While another atelier holds it, the gate prints `<platform>: waiting for this computer's machine lock: <holder>` once and waits.
- Another machine gets the merged commit pushed and runs the platform's `run`. It passes only if every run there records that commit, its tree and no uncommitted change. A lost connection is no machine: exit 3.
- If this computer's platform fails, the gate is RED at once; the others print `cancelled on <machine>, run <id>: the run on this computer failed`.
- Every machine runs the merged commit's pinned atelier. A machine without it is `not ready: atelier <pin> is not installed there: run atelier machines init <name> with atelier <pin>`.

## promote

After a green gate, `main` moves by one local push into the main checkout:

```sh
git -c push.gpgSign=false push --porcelain --no-verify --receive-pack='git -c receive.denyCurrentBranch=updateInstead -c receive.denyNonFastForwards=true -c core.hooksPath=<state>/empty-hooks receive-pack' --force-with-lease=refs/heads/main:<OLD> . <M>:refs/heads/main
```

No repository hook runs. `.git/atelier/empty-hooks` must stay empty (anything in it: refused, 4). The push is never signalled or time-limited.

Before each push atelier checks:

| Check | Failure |
|---|---|
| a. `main` is a plain ref, checked out in the main checkout or nowhere | refused (4) |
| b. the repository has no `push-to-checkout` hook | refused (4) |
| c. no merge, rebase, cherry-pick, revert, bisect or `index.lock` in the main checkout; no rebase or bisect of `main` in any worktree | busy |
| d. no tracked change in the main checkout | busy |
| e. nothing stands where `M` adds a path, except paths `M` deletes (case-insensitively on such volumes) | busy |

While a check is busy, `merge` retries for `promotion_wait_seconds`, then exits 7:

```text
merge: GREEN, NOT PROMOTED — run <id>; <reasons>; run `atelier promote` when it is clean
```

`atelier promote` retries. `atelier promote --discard` drops the green result; approvals stay. A pending promotion is settled by `merge` and `promote` first: recorded if `main` already holds it, discarded if `main` moved elsewhere.

**Interrupted promotion.** A push that started, did not finish, and left the main checkout partly written. `merge` and `promote` refuse (4) and print `git status`, path counts, every path holding something unexpected, and `runs/<gate run>/promotion-paths.tsv` (`path`, `old|m|other`, what is there), with the two resets to choose from. atelier runs neither. Give this list to the owner.

Recording each branch as merged takes its branch lock shared, so `tmp` and `check` do not block it. `start`, `follow` and `clean` hold it exclusive; after 10 s the record is left for the next `promote` or `merge`. The output lines are printed again until the record is complete.

## Bisection

After a RED gate of several branches, in a run of kind `gate-bisect`.

- A RED that is not a platform failure (settings, hooks, a changed tree) is not searched: `merge: no search: the RED is not a platform's failure (<cause>) …; no branch is blamed`.
- Otherwise prefixes of the batch are tried, longest first. Only what failed is re-run: failed tests by name and failed steps whole (`atelier check --only`, run kind `check-only`); a failure that cannot be narrowed runs the platform whole. A remote failure is re-run on that machine first.
- A prefix whose hooks leave the gate unclean stops the search; the RED stands.
- The branch after the longest green prefix is blamed: `<branch> breaks: on prefix <…>: <what failed>`, or `fails only with the whole batch (<last>), or is flaky: <failures>`.
- The green prefix goes through one full gate and lands if green. The rest stay approved and are listed. `merge` still exits 1.

## Versioned layer, format 1

Under `.git/atelier/versioned/`. Only the version pinned on `main` reads or writes it, in the main checkout. A new format needs a converter.

| Path | Content |
|---|---|
| `approvals/<branch key>.toml` | one per branch; replaced by `approve`, removed when the branch is recorded merged |
| `merges.tsv` | one line per merged branch, appended once |
| `promotion.toml` | a green gate's promotion; written before the push, removed once every branch is recorded |

- **Approval** (TOML): `branch`, `lifetime`, `commit`, `worktree` (canonical path), `dev`, `ino` (its identity), `approved` (RFC 3339 UTC).
- **`merges.tsv`**, tab-separated: `time branch lifetime commit OLD M run platforms`. `platforms` is `;`-separated `<platform>=pass@<machine>` (`local` for this computer) or `<platform>=waived:<owner's words>`. `%`, tab, newline, carriage return and `;` in a field are percent-encoded.
- **Promotion** (TOML): `integration`, `old`, `m`, `run`, `created`, `push_started`, `push_pgid`, `push_ended`, `summary` (the GREEN line), `rules_changed`, `branches` (each `name`, `lifetime`, `commit`, `worktree`, `dev`, `ino`) and `platforms` (each `platform`, `result`, `machine`, `reason`, `run`).

When a branch is recorded merged, its branch record gets `state = "merged"` and the `merged` table `clean` trusts ([FROZEN-LAYER.md](FROZEN-LAYER.md#merge-facts)).
