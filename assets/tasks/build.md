+++
fields = ["goal", "files", "done", "risks"]
optional = ["notes", "decisions"]
+++
# Task: {{branch}}

Worktree: {{worktree}}. Branch: {{branch}}, built on {{lower}}. Read {{rules}}/WORKER.md first, then do this item.

**Goal.** {{goal}}

**Files.** {{files}}

**Done when.** {{done}}

**Risks.** {{risks}}
{{#decisions}}
**Decisions.** {{decisions}}
{{/decisions}}
{{#notes}}
**Notes.** {{notes}}
{{/notes}}
{{#stacked}}
Your branch is built on {{lower}}, which may still change. When it does, run `atelier follow`.
{{/stacked}}
