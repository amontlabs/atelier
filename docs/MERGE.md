# approve, merge, promote (module 3.4)

The design is DESIGN.md §3 (`approve`, `merge`, `promote`), §2.4 (stacked branches), §8.3
(bisection) and DESIGN-r4.md §3.8 (the gate, mode A, the failure table), with B8's measurements
and HF1's regenerated paths. This file records what the code does and the formats it writes.

## The versioned layer, format 1

Only the version pinned on the integration branch reads or writes these, in the main checkout
(`preflight::require_main_version`). A later format needs a converter (`state::Converter`).

| Path under `.git/atelier/versioned/` | Content |
|---|---|
| `approvals/<branch key>.toml` | one per branch, replaced by every `approve`, removed when the branch is recorded merged |
| `merges.tsv` | one line per merged branch, appended once, flushed |
| `promotion.toml` | a green gate's promotion, written before the push, removed once every branch is recorded |

**Approval** (TOML): `branch`, `lifetime` (the branch record's), `commit` (the approved tip),
`worktree` (canonical path), `dev`, `ino` (that folder's identity), `approved` (RFC 3339 UTC).

**`merges.tsv`**: tab-separated, one line per branch:
`time branch lifetime commit OLD M run platforms`. `platforms` is `;`-separated, each
`<platform>=pass@<machine>` (`local` for this computer) or `<platform>=waived:<owner's words>`.
`%`, tab, newline, carriage return and `;` inside a field are percent-encoded.

**Promotion** (TOML): `integration`, `old`, `m`, `run` (the gate's run id), `created`,
`push_started`, `push_pgid`, `push_ended`, `summary` (the GREEN line), `rules_changed`,
`branches` (each `name`, `lifetime`, `commit`, `worktree`, `dev`, `ino`) and `platforms` (each
`platform`, `result`, `machine`, `reason`, `run`).

When a promotion is recorded, each branch's frozen record gets, in one write, `state = "merged"`
and the `merged` table `clean` trusts (`records::MergeFacts::record`, docs/FROZEN-LAYER.md):
the merged commit and the worktree path and identity `approve` recorded.

## approve

Under the branch's lock: the record as every reading command sees it (pending follow settled,
base still in the branch); `<commit>` is the branch's tip; no merge commit of its own; current
(transitively); checked out in exactly one worktree that is neither the main checkout nor the
gate. Its canonical path and identity come from `MergeFacts::of_worktree`.

## merge

Under the gate lock, in this order:

1. A pending promotion is settled by the three questions (M in the integration branch: recorded;
   at OLD: refused as pending, or as interrupted; elsewhere: discarded). Mode A checks a and b.
   Per branch: approved, same lifetime, still at the approved commit, not behind a rewrite, no
   own merge commit, no rebase open. A stacked branch's lower branch is in the batch, earlier,
   approved at a commit containing the upper's base. A branch whose own commits (`base..commit`)
   touch `.atelier/`, `AGENTS.md` or an import file merges alone. Every platform has a machine or
   is waived (exit 3 otherwise, before the gate).
2. The gate `<main checkout>.gate` (`gate::gate_worktree`, the one function `approve`, `tmp` and
   `clean` use): created detached at OLD on first use; otherwise it must be registered at that
   exact path, detached, with no git operation in progress and no tracked change or untracked
   file git does not ignore. atelier lists what is there and refuses; it deletes nothing. Then
   `git checkout --detach --no-overwrite-ignore OLD`, and its tree id must be OLD's tree.
3. `git merge --no-ff --no-verify --no-gpg-sign --no-autostash --no-overwrite-ignore` of each
   approved commit, with `rerere.enabled=false`. When every conflicted path is named by some
   `[[merge.after]]` hook's `regenerates`, atelier takes **the incoming branch's side** for those
   paths (`git checkout --theirs`, or removes the path when that side deleted it), commits the
   merge, and the hook rewrites them in step 4; the check then proves the result. Any other
   conflicted path aborts the merge, returns the gate to OLD and exits 5:
   `conflict: <branch> with main` when the approved commit alone conflicts with OLD
   (`git merge-tree`), otherwise `with <earlier branches> (earlier in this batch)`, then the
   files (regenerated paths left out).
4. `[[merge.after]]` hooks from the merged settings, each a step of the gate run. A hook with
   `commit` has everything it changed committed (`git add -A`); any other change is RED naming
   the hook. The tree id must be M's tree.
5. Platforms (`gate::Platforms`). This computer's platform runs in the gate: `run = "check"`
   runs `<~/.local/share/atelier/<M's pin>/bin/atelier> check` as a step of the gate run, so its
   markers nest and its run is found by its `ATELIER_RUNS`; a command runs after the
   `check.prepare` hooks that apply and the platform's `prepare`.
6. RED: exit 1, the integration branch unmoved, the gate left at M. With more than one branch,
   bisection follows (below).
7. Green: tree id still M's, OLD an ancestor of M, the promotion record written, then mode A
   (`promote`), which prints the GREEN line, `merged <branch> <commit>` per branch, and
   `rules changed` when a rules item landed.

## promote (mode A)

`promote::promote` repeats, for `promotion_wait_seconds`: the three questions; the interrupted
check; checks a and b (refusal, 4); checks c, d, e (busy: wait). With all five passing, one push:

```sh
git push --porcelain --receive-pack='git -c receive.denyCurrentBranch=updateInstead -c receive.denyNonFastForwards=true receive-pack' --force-with-lease=refs/heads/main:<OLD> . <M>:refs/heads/main
```

started through the step trampoline as its own process group, recorded under the gate lock before
git runs, never signalled and never time-limited: atelier waits for the group to be empty. The
record holds `push_started` before and `push_ended` after a push that left the checkout as git
should. A stop signal ends only the wait. Exit 7 prints
`merge: GREEN, NOT PROMOTED — run <id>; <reasons>; run `atelier promote` when it is clean`.

An interrupted promotion (`push_started` without `push_ended`, the ref still at OLD, check d or e
failing) makes `merge` and `promote` refuse (4) with `git status`, the counts of paths holding
OLD's and M's content, every path holding something else with its size, the file
`runs/<gate run>/promotion-paths.tsv` (`path`, `old|m|other`, what is there), and the two resets
it does not run. `promote --discard` drops the record; approvals stay.

## Bisection (§8.3)

After a RED gate of more than one branch, under the same gate lock, in a run of kind
`gate-bisect`: prefixes longest first, each merged in the gate with the after hooks; a tree
already judged (the failed batch's included) takes its verdict. Otherwise only what failed runs:
`atelier check --only <file>` (run kind `check-only`) with the failed tests by name and the failed
steps whole; a failure that cannot be narrowed (a `check.prepare` hook, a failure that is no
step's, a command platform) runs the platform whole. nextest selects by test name
(`test(=<name>)`, the part after `$`), because the libtest-JSON prefix is not nextest's binary id;
only the listed full names are judged, and a listed test the prefix does not have is a failure
that does not appear (`--no-tests=pass`). The branch after the longest green prefix is the culprit
(`<branch> breaks: …`, or `fails only with the whole batch (<last>), or is flaky: …`). The green
prefix then goes through one full gate and lands if green; the rest stay approved and are listed.
`merge` still exits 1.

## Seams for module 3.6 (machines)

- `gate::remote_availability(platform)`: step 1's machine check for a platform this computer
  does not run. Today: no machine.
- `gate::remote_platform(gate, platform)`: step 5's run on the chosen machine, through M's pinned
  atelier there. Today: `NoMachine`.
- `gate::Platforms`: the trait `merge` runs platforms through. 3.6 replaces `LocalPlatforms` with
  one that chooses machines (offline, not ready, busy, the busy-only wait) and starts every
  platform at the same time, the local one in the gate worktree.
- `bisect::Search` refuses to re-run a failure that ran on another machine ("comes with module
  3.6").
