# Worker rules

You build one item, in one worktree, on one branch, named in your task.

## Checkout
- Work only in your worktree. Commit as you go. Never push, never open a pull request, never touch another checkout.
- Commit only your own files, by path, in one step: `git commit -m "…" -- <paths>`. Never `git add -A` or `git commit -a`: another agent may be working in the same worktree.
- Amend, reorder and squash your own commits as you like (those above the base `atelier follow` prints). Never rebase below them by hand. Never `git merge` into your branch.
- For newer work, commit, then run `atelier follow`. It brings your branch up to date with what it is built on (another item, or `main` once that item is merged). On a conflict it stops with the rebase open: resolve, `git add`, `git rebase --continue`, then `atelier follow` again.
{{#rust}}
- Use plain `cargo`. Never `+toolchain`, never `CARGO_TARGET_DIR`.
{{/rust}}

## Decisions
- Where your task does not decide a behaviour, pick the safest one that keeps every existing rule (no wider access, no skipped approval, no lost data, older saved data still loads) and go on. List each under "Decisions I made" in your report.

## Proof
- Run the tests that prove your change: one test, one crate, the frontend files you touched.
- Every rule that must hold on every path gets one model-based test: random sequences of operations against a reference model, checked after every step. Such a test caught 4 of 7 high-severity past bugs that ordinary tests missed. A change that adds a path the rule covers adds it to that test.
{{#rust}}
- After a fix, rerun the failing tests by name (`cargo nextest run -E 'test(=<name>)'`). To fix a flaky test, prove it with `cargo nextest run --stress-count 50` on the crates you touched (`vitest --repeats 50` for frontend tests).
{{/rust}}
- Before your report, run `atelier check --compile-only` on the committed tree (format, every target compiles, typecheck). Copy its `compile:` line into your report.
{{#multi_platform}}
- Another platform: when your change is about one the project lists, run your tests there with `atelier test <platform> -- <command>`, from the folder you would use locally.
{{/multi_platform}}
- Temporary data: run the command that needs it as `atelier tmp -- <command>` (`--short` for a temp HOME with sockets). The folder is in `$ATELIER_TMP`. Use nowhere else.

## Waiting
- Run long commands in the foreground. Stop every background command before your final report.

## Report (your final message, at most 400 words)
The stop hook sends you back while a rebase is open, something is uncommitted, a background task runs, or your last message has no line starting `compile:`. Fix what it names, then report.
1. Branch and last commit.
2. What you built, in plain words.
3. Tests you ran and their results; the `compile:` line.
4. What is not done or not verified.
5. Decisions I made, or "none".
6. Broken things outside your scope, for the orchestrator to decide.
