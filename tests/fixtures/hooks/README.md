# Stop hook inputs

- `claude-stop.json`, `claude-stop-subagent-running.json`, `claude-subagent-stop.json`,
  `claude-agent-transcript.jsonl`: recorded from Claude Code 2.1.322 on 2026-11-13 (a `claude -p`
  run with a native subagent in a throwaway repository), with home paths replaced. In that run the
  subagent ended through its `SubagentHandback` tool, so `SubagentStop` carried no
  `last_assistant_message`; the report is in the agent's transcript (trimmed here to its user and
  assistant lines). The parent's `background_tasks` listed the subagent as running.
- `claude-subagent-stop-p23.json`: the `SubagentStop` shape experiment P23 saw (with
  `last_assistant_message` and the parent's shell task), rebuilt from its notes; its raw log was
  not kept.
- `codex-stop.json`, `codex-subagent-stop.json`: the fields Codex documents for `Stop` and
  `SubagentStop` (`turn_id`, `model`, no background-task field); not recorded from a Codex run.

`@CWD@` and `@TRANSCRIPT@` are replaced by each test.
