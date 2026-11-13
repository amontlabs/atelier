# Worker rules

You build one item, in one worktree, on one branch, named in your task.

## Checkout
- Work only in your worktree. Commit as you go. Never push, never open a pull request, never touch another checkout.
- Commit only your own files, by path, in one step: `git commit -m "…" -- <paths>`. Never `git add -A` or `git commit -a`: another agent may be working in the same worktree.
- Amend, reorder and squash your own commits as you like (they are the ones above the base `atelier follow` prints). Never rebase below them by hand, and never `git merge` into your branch: newer work comes only through `atelier follow`.
- Getting newer work: commit, then `atelier follow`. It brings your branch up to date with what it is built on (another item, or `main` once that item is merged). On a conflict it stops with the rebase open: resolve, `git add`, `git rebase --continue`, then `atelier follow` once more.
{{#rust}}
- Use plain `cargo`. Never `+toolchain`, never `CARGO_TARGET_DIR`.
{{/rust}}

## Proof
- Choose the tests that prove your change: one test, one crate, the frontend files you touched. Run them yourself.
{{#rust}}
- After a fix, rerun the failing tests by name (`cargo nextest run -E 'test(=<name>)'`). Fixing a flaky test: prove it with `cargo nextest run --stress-count 50` on the crates you touched (`vitest --repeats 50` for frontend tests).
{{/rust}}
- Before your report: `atelier check --compile-only` on the committed tree (format, every target compiles, typecheck). Copy its `compile:` line into your report.
{{#multi_platform}}
- Another platform: when your change is about one the project lists, run your tests there with `atelier test <platform> -- <command>`, from the same folder you would run them in locally.
{{/multi_platform}}
- Temporary data: run the command that needs it as `atelier tmp -- <command>` (`--short` for a temp HOME with sockets). It gets the folder in `$ATELIER_TMP`. Nowhere else.

## Waiting
- Run long commands in the foreground. Stop every background command before your final report.

## Report (your final message, at most 400 words)
1. Branch and last commit.
2. What you built, in plain words.
3. Tests you ran and their results; the `compile:` line.
4. What is not done or not verified.
5. Broken things outside your scope; what the orchestrator should decide.
