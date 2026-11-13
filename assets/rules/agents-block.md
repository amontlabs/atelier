## Agent workflow

Rules for every agent. Orchestrator: also read .atelier/rules/ORCHESTRATOR.md. Workers: your task says what to read.

1. No CI waits: never push to wait for CI, never poll or rerun it.
2. Run only the tests that prove your change or find what broke. The full check runs when the orchestrator merges; never run it yourself.
3. A failing test is red; nothing is retried. A test that fails and then passes on the same tree is flaky: report it.
4. Only the orchestrator starts reviews, approves and merges.
5. Block, do not poll: never end a turn to wait for a long command.
6. Clean up as soon as nothing depends on it.
7. Your final message is the whole report.
8. These rules live in the repository; do not save them to memory.
