# Orchestrator rules

1. Start with `atelier status` and `atelier agents`. A failed preflight line is fixed first, as its own item.
2. Plan items, one topic each. For each: `atelier start <branch> --path <worktree>` (add `--on <lower branch>` when it builds on one), then `atelier task build <branch> goal=… files=… done=… risks=… decisions=…` (or `build-hard`). Start that role's agent with the printed line as the prompt. Start the next item once what it builds on has committed. When a worker finishes, look for work to start before you wait.
2b. Decide before the worker starts: workers miss what nobody decided, and review does not catch it (review still found 5 problems with written decisions, 13 without). `decisions=` says how the item behaves at every entry point it touches (API, CLI, background jobs, recovery, import/export, retry, approvals, deletion, limits) and with older saved data. Answer from the decision log, then the docs; ask `judge` what you cannot settle. Never stop the build to ask the owner.
   Keep a decision log, one line each: what, who decided (you, judge or worker), the item, whether the owner should look. Copy in each worker's "Decisions I made". The owner reads it at the end.
3. On each report, read its tests and its `compile:` line. Review by risk; skip review for a mechanical change.
   - Round 1: `atelier task review <branch> intent=… risks=…`. Round 2: `atelier task review <branch> --fixes findings=@<file>`.
   - Round 1 of a risky item (security, policy, persistence, recovery): two independent reviews at high effort. They found 2-3x more than one default-effort review.
   - Stop when a round reports nothing critical or high. Fix medium findings at your judgement. Low findings never start a round.
   - Reviewers get the task, which holds the decisions, not the decision log.
   - Fixes go back to the worker that wrote the code.
   - A stacked branch is reviewed for its own commits only; its lower branch is approved separately and merges in the same batch or before.
   - Then `atelier approve <branch> <the commit you reviewed>`.
4. Merge everything approved in one batch, as a background command: `atelier merge <branch>…`. It runs the full check on the merged tree{{#multi_platform}}, on every platform{{/multi_platform}}. A branch that changes `.atelier/` merges in its own batch. When a gate ends, the next batch takes everything approved since.
   - Exit 1, or 5 "with main": send the output to the worker who wrote the code; for a conflict it runs `atelier follow` and resolves.
   - Exit 5 "with <earlier branch>": merge again without the later branch. Once the earlier one has landed, the later branch's worker runs `atelier follow` and resolves.
   - Exit 7: don't touch the main checkout; run `atelier promote` once it is clean.
   - "main moved": run the same merge again.
   - "<branch> is behind a rewrite of <lower>": ask that branch's worker to run `atelier follow`. `atelier status` lists every such branch.
   - "gate worktree has unknown files": look at each, keep what someone saved, remove the rest, merge again.
   - "a promotion was interrupted": stop merging, change nothing in the main checkout, give the owner atelier's list.
   - "rules changed": re-read these rules; tell running workers to `atelier follow` and re-read.
{{#multi_platform}}
   - Exit 3, no machine for a platform: waive it (`--waive <platform>`) only on the owner's word, quoted in `--reason`.
{{/multi_platform}}
4b. Worktrees are the default, not a requirement; you decide.
   - Several small items with disjoint files may share one worktree and branch, reviewed, approved and merged as one. Tell each agent its files; one commit per item, so a failing one can be dropped.
   - A change only to files that no build, test or check reads (top-level `docs/` and Markdown outside the app; not bundled help) may be committed straight to `main` by one agent, after checking its relative links, and only while no merge is running: a commit on `main` voids a running merge.
   - Anything else goes through `atelier merge`, however small.
5. A test that `atelier status` lists as flaky (failed, then passed on the same tree and platform in full runs) is a fix item, with the failing output, before anything else.
6. After each merge: `atelier clean`, read the plan, then `atelier clean --yes`.
7. Done when everything is merged{{#single_platform}} and{{/single_platform}}{{#multi_platform}},{{/multi_platform}} no promotion is pending{{#multi_platform}}, and every platform passed on `main` or the owner waived it{{/multi_platform}}.

Choosing agents: by role, from `atelier agents`. Effort: low for lookups and mechanical edits; medium for an ordinary item; high for cross-crate, concurrency, migration or security work and reviews of risky diffs. Never name models, retries or CI in a task. Reuse a worker for its fixes while its session is small; otherwise start a fresh one in the same worktree with a short handover. A report that only points elsewhere ("the report above stands") is lost: ask for it in full.
