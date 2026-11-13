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
the merged commit and the worktree path and identity `approve` recorded. A branch that moved
past its approved commit during the gate, or was approved again at another commit, is the
exception: its approved commit is merged (its `merges.tsv` line, `merged <branch> <commit>`),
but its record stays `active` without the `merged` table, and `merge` prints
`<branch> moved to <tip> after its approval at <commit>: its later commits are not merged; it
stays active (atelier follow in its worktree, then approve it again)`. The frozen rules give a
branch a new lifetime only through `start` of a name whose branch does not exist, so the record
keeps its lifetime; `follow` then rebases the later commits onto the integration branch (the
merged ones are upstream and dropped), and `clean` deletes nothing of it. Its newer approval
stays.

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
   touch `.atelier/`, `AGENTS.md` or an import file merges alone (a *rules item*). OLD's
   settings are read from OLD itself (`git show OLD:.atelier/atelier.toml`), never from the main
   checkout's file. The settings the batch produces are known before anything merges: a rules
   item merges alone, so its approved commit's committed settings are the merged ones
   (`gate::batch_settings`); any other batch keeps OLD's. Every platform of those settings has a
   machine or is waived (exit 3 otherwise, before the gate), and the pin is theirs: this
   computer takes a `run = "check"` platform only with that atelier installed here (refused, 4,
   naming the install command, when no other machine is listed), and another machine only when
   its probe says that atelier is installed there. `--waive <platform> --reason "<words>"`
   repeats, each `--waive` with its own `--reason` in the same order; a waiver names a platform
   of OLD's settings or of the merged ones, so a rules item that drops a platform whose machine
   is gone merges with or without a waiver for it.
2. The gate `<main checkout>.gate` (`gate::gate_worktree`, the one function `approve`, `tmp` and
   `clean` use): created detached at OLD on first use; otherwise it must be registered at that
   exact path, detached, with no git operation in progress and no tracked change or untracked
   file git does not ignore. atelier lists what is there and refuses; it deletes nothing. Then
   `git checkout --detach --no-overwrite-ignore OLD`, and its tree id must be OLD's tree.
3. `git merge --no-ff --no-verify --no-gpg-sign --no-autostash --no-overwrite-ignore` of each
   approved commit, with `-c core.attributesFile=/dev/null -c rerere.enabled=false
   -c merge.renormalize=false -c merge.renames=true` and `GIT_ATTR_NOSYSTEM=1` (every git
   command of the gate's merges, `merge-tree` and `check-attr` included). Before each merge,
   the paths both sides changed are looked up with `git check-attr merge` (the checkout's
   `.gitattributes`, the incoming commit's with `--source`, and `info/attributes`): a merge
   driver other than git's `text` and `binary` merges (named by the attribute or by
   `merge.default`), or one of those names the git configuration redefines
   (`merge.<name>.driver`), refuses (4), naming each path and driver, before anything is merged.
   Such a driver (mergiraf's documented install is one) would settle a conflict without a word.
   So does git's own `union`, wherever it is set (a committed `.gitattributes` included): it
   never conflicts, keeping both sides' lines (`<path>: merge=union, which keeps both sides'
   lines and never conflicts`).

   When every conflicted path is a *regenerated path*, atelier takes **the incoming branch's
   side** for those paths (`git checkout --theirs`, or removes the path when that side deleted
   it), commits the merge, and the hook rewrites them in step 4. A regenerated path is one that
   a `[[merge.after]]` hook with `commit` names in `regenerates` both in OLD's committed settings
   and in the merged settings (the index's, after the merge). A rules item never has a
   conflict settled. Any other conflicted path aborts the merge, returns the gate to OLD and
   exits 5:
   `conflict: <branch> with main` when the approved commit alone conflicts with OLD
   (`git merge-tree`), otherwise `with <earlier branches> (earlier in this batch)`, then the
   files (regenerated paths left out).
4. `[[merge.after]]` hooks from the merged settings, each a step of the gate run. A hook with
   `commit` has everything it changed committed (`git add -A`); any other change is RED naming
   the hook. The tree id must be M's tree. Then each settled path must hold what a hook of the
   merged settings with `commit` that names it produced: that hook's commit changed it, or, with
   the path removed, running the hooks that name it again gives back exactly M's tree (steps
   `regenerate:<hook>`). Otherwise RED (`merge.after <hook> did not regenerate <path> as
   merged`); paths atelier removed and no hook wrote again are checked out from M. A hand edit
   on either side of a regenerated file is lost whenever the hook rewrites it, and the gate
   proves the generated file only if the project tests it against a fresh generation: a
   `regenerates` hook must always rewrite its files (one that skips work by timestamp is caught
   here as RED).
5. Platforms, from the merged settings: a waiver must name one of OLD's or theirs (refused, 4,
   otherwise; one the merged settings no longer have prints `WAIVED (<words>); the merged
   settings no longer have it`), and a platform that differs from step 1's settings (or every
   platform, when the pin differs) is checked for a machine first, as in step 1. Then every
   platform runs at the same time
   (`gate_machines::Machines`, below). `run = "check"` here runs
   `<~/.local/share/atelier/<M's pin>/bin/atelier> check` as a step of the gate run, passing it
   `ATELIER_RUN_ID=<a new id>`, the id its run takes, so the gate reads exactly that run's result
   (a nested atelier run under it is never taken for it); a pinned atelier that does not take
   the id is read through the one run directly under the gate's. A command runs after the
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
git -c push.gpgSign=false push --porcelain --no-verify --receive-pack='git -c receive.denyCurrentBranch=updateInstead -c receive.denyNonFastForwards=true -c core.hooksPath=<state>/empty-hooks receive-pack' --force-with-lease=refs/heads/main:<OLD> . <M>:refs/heads/main
```

No hook of the repository runs on either side: not `pre-push` (`--no-verify`), and not
`pre-receive`, `update`, `reference-transaction` or `push-to-checkout` (the receiving side's
hooks folder is `.git/atelier/empty-hooks`, which atelier creates and which must stay empty:
anything in it refuses, 4, naming it). git's own test that the main checkout is clean decides;
check b still refuses a `push-to-checkout` hook before the gate.

The push is started through the step trampoline as its own process group, recorded under the gate
lock (and its `push_pgid` in the record) before git runs, never signalled and never time-limited:
atelier waits for the group to be empty. The record holds `push_started` before and `push_ended`
after a push that ended cleanly: git accepted the ref, or the ref stayed at OLD and the main
checkout shows the push wrote nothing, judged by content and never by git's words (every path
that differs between OLD and M, and every other tracked path `git status` reports, holds OLD's
content, so none of M's added paths is there, and the index is OLD's tree), or, failing that,
the push left the checkout as checks d and e want it. git's words cannot tell: with
`updateInstead`, receive-pack runs `read-tree -u -m` before it moves the ref, and a failure part
way through (a folder without write permission) is reported as `! … [remote rejected] (Could not
update working tree to new HEAD)` and `Done`, with files already written. That case, and any
other change, is an interrupted promotion (below). A stop signal ends only the wait. Exit 7 prints
`merge: GREEN, NOT PROMOTED — run <id>; <reasons>; run `atelier promote` when it is clean`.

An interrupted promotion (`push_started` with a recorded `push_pgid` and without `push_ended`,
the ref still at OLD, check d or e failing) makes `merge` and `promote` refuse (4) with
`git status`, the counts of paths holding OLD's and M's content, every path holding something
else with its size, the file `runs/<gate run>/promotion-paths.tsv` (`path`, `old|m|other`, what
is there), and the two resets it does not run. Without a recorded `push_pgid`, atelier ended
before releasing the push and git never ran: no interruption. `promote --discard` drops the
record; approvals stay.

Check e does not count as an obstruction what git removes before it writes: a path OLD tracks
and M deletes (compared without case on a volume that ignores case, so a case-only rename
passes), or a folder holding only such paths (a folder that becomes a file). An ignored or
untracked file inside such a folder still obstructs, named.

Step 9 (`promote::finish`) appends the `merges.tsv` lines, then per branch, under its branch lock
taken shared (`tmp` and `check` hold it shared too; only `start`, `follow` and `clean` make it
wait, 10 s, after which the record is kept and the next command finishes): the record marked
merged (or kept active, above) and the approval removed. It prints the GREEN line,
`merged <branch> <commit>` per branch and `rules changed`, and only then removes the promotion
record, so a crash in between prints them again on the next `promote` or `merge`.

## Bisection (§8.3)

After a RED gate of more than one branch, under the same gate lock, in a run of kind
`gate-bisect`. A RED that is no platform's failure (the merged settings, the hooks, the tree
changing) is not searched: `merge: no search: the RED is not a platform's failure (<cause>)
…; no branch is blamed`. Otherwise prefixes longest first, each merged in the gate with the
after hooks (and the regenerated-path check); a prefix whose hooks fail is a failing prefix.
When a prefix leaves the gate unclean, the search stops: the leftovers would come into every
shorter prefix and atelier deletes nothing there (`the search stopped (prefix …: the gate is not
clean after its merge.after hooks: <paths>; …); the RED verdict stands`). A tree already judged
(the failed batch's included) takes its verdict. Otherwise only what failed runs:
`atelier check --only <file>` (run kind `check-only`) with the failed tests by name and the failed
steps whole; a failure that cannot be narrowed (a `check.prepare` hook, a failure that is no
step's, a command platform) runs the platform whole. nextest selects by test name
(`test(=<name>)`, the part after `$`), because the libtest-JSON prefix is not nextest's binary id;
only the listed full names are judged, and a listed test the prefix does not have is a failure
that does not appear (`--no-tests=pass`). The branch after the longest green prefix is the culprit
(`<branch> breaks: on prefix <…>: <what failed on that prefix>`, or `fails only with the whole
batch (<last>), or is flaky: <the batch's failures>`). A failure on another machine is re-run
whole, on that machine first, then the platform's other machines in order. The green
prefix then goes through one full gate and lands if green; the rest stay approved and are listed.
`merge` still exits 1.

## Platforms on machines (`gate_machines`, module 3.6)

Without a machines file (`~/.config/atelier/machines.toml`), the platform this computer runs
runs here and every other platform has no machine. With one, each platform's machine is chosen
as `atelier test` chooses it (`machines::candidates`, `machines::select`: the project's order or
the priorities, offline, not ready, busy, the wait for a busy one):

- step 1 is a probe only, with the settings the batch produces (step 1 above): this computer is
  available for its platform when it can run their pin; another machine is when its probe
  answers ready or busy;
- step 5 runs every platform at the same time: one thread per platform chooses its machine;
  this computer runs its platform in the gate worktree, as a step of the gate run (its machine
  preflight first, as `test`); another machine gets M pushed and runs the platform's whole `run`
  (`remote-run`), and passes only when every run there records M, M's tree and no uncommitted
  change (`remote::Session::finish`). A lost connection gives no verdict: no machine, exit 3.
  The record names the machine (`<platform>=pass@<machine>`);
- every run on this computer holds its machine lock (`~/.local/state/atelier/machine.lock`):
  the gate's, with or without a machines file, and bisection's re-runs. While another atelier
  holds it (`test`, a `remote-run` from another computer) the gate waits, printing
  `<platform>: waiting for this computer's machine lock: <holder>` once; a stop signal ends the
  wait;
- when the platform on this computer fails (or cannot run), the gate is RED whatever the other
  machines say: their sessions are stopped at once (closing the connection stops the remote
  run) and the gate does not wait for them. Each is printed and recorded as `cancelled on
  <machine>, run <id>: the run on this computer failed` (`cancelled, no machine started` when
  none had taken it yet), and bisection searches only this computer's failures;
- every machine runs M's own pinned atelier (DESIGN §3 step 5): `~/.local/share/atelier/<pin>/
  bin/atelier` there. The probe asks the machine whether that version is installed (for this
  program's own version, also built from this program's source); otherwise the machine is not
  ready: `atelier <pin> is not installed there: run atelier machines init <name> with atelier
  <pin>`. A merged commit that changes the pin therefore needs `atelier machines init <name>`,
  run with the new version, on each machine that is to run it, before the merge: step 1 finds
  out in seconds, before anything merges. This computer needs the pin installed only for a
  platform it runs itself.
