//! `atelier hook stop` (DESIGN §8.5, experiment P23): the Stop and SubagentStop hook of Claude
//! Code and Codex, and the project hook files that register it.
//!
//! The hook reads the harness's JSON on stdin and acts only when the session's folder (`cwd`) is
//! inside a worktree on a branch atelier started (an `active` branch record), whether or not a
//! rebase has detached `HEAD`. There it blocks the end of the turn, so the agent continues with
//! the reason, when:
//!
//! - a rebase is in progress in that worktree;
//! - the worktree has uncommitted changes (untracked files included);
//! - a background task the input lists is still running (Claude Code's `background_tasks`, a
//!   shell command or a subagent; Codex sends no such field);
//! - the last assistant message has no line starting `compile:`.
//!
//! The message is `last_assistant_message`. When the input has none (Claude Code 2.1.322 sends
//! none on `SubagentStop` when the subagent ended through its `SubagentHandback` tool), it is
//! the last report in the transcript the input names (`agent_transcript_path` for a subagent,
//! `transcript_path` otherwise): the last assistant entry's text, or its `SubagentHandback`
//! message. When neither can be read the message is not checked.
//!
//! For `SubagentStop` only the message check applies: the input's folder and background tasks are
//! the parent session's (P23). Both harnesses take the same block: `{"decision": "block",
//! "reason": "…"}` on stdout, exit 0. Every other case, and any failure of the hook itself, lets
//! the agent stop: the hook never exits 2 (which both harnesses read as a block) and is a strong
//! nudge, not a guarantee (Claude Code honours at most 8 blocks in a row); atelier's own commands
//! still refuse what matters.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::git::{self, Checkout};
use crate::records::{BranchRecord, BranchState};
use crate::state::{STATE_DIR, State};

/// The command the project hook files run.
pub const COMMAND: &str = "atelier hook stop";

/// Claude Code's project settings, relative to the worktree.
pub const CLAUDE_SETTINGS: &str = ".claude/settings.json";

/// Codex's project hook file, relative to the worktree.
pub const CODEX_HOOKS: &str = ".codex/hooks.json";

/// The events the hook is registered for.
pub const EVENTS: [&str; 2] = ["Stop", "SubagentStop"];

/// Which harness sent the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

/// The hook's verdict: `Some(reason)` blocks.
pub type Verdict = Option<String>;

/// The harness, told from the input: Codex sends `turn_id` (and `model`), Claude Code neither.
pub fn harness(input: &Value) -> Harness {
    if input.get("turn_id").is_some() || input.get("model").is_some() {
        Harness::Codex
    } else {
        Harness::ClaudeCode
    }
}

/// The worktree and branch of `cwd` when atelier started that branch: an `active` record, the
/// branch checked out or being rebased there.
fn atelier_worktree(cwd: &Path) -> Option<(Checkout, String)> {
    let checkout = Checkout::find(cwd).ok()??;
    if !checkout.common_dir.join(STATE_DIR).is_dir() {
        return None;
    }
    let branch = branch_of(&checkout)?;
    let state = State::open(&checkout).ok()?;
    let rec = BranchRecord::read(&state, &branch).ok()??;
    (rec.state == BranchState::Active && rec.starting.is_none()).then_some((checkout, branch))
}

/// The branch `HEAD` names, or the one an open rebase in this worktree is rebasing.
fn branch_of(checkout: &Checkout) -> Option<String> {
    let out = git::output(&checkout.root, ["symbolic-ref", "-q", "HEAD"]).ok()?;
    let full = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    } else {
        ["rebase-merge", "rebase-apply"]
            .iter()
            .find_map(|d| std::fs::read_to_string(checkout.git_dir.join(d).join("head-name")).ok())?
            .trim()
            .to_owned()
    };
    full.strip_prefix("refs/heads/").map(str::to_owned)
}

fn rebase_open(checkout: &Checkout) -> bool {
    ["rebase-merge", "rebase-apply"]
        .iter()
        .any(|d| checkout.git_dir.join(d).exists())
}

/// `git status --porcelain` lines; `None` when git fails.
fn uncommitted(checkout: &Checkout) -> Option<Vec<String>> {
    let out = git::output(
        &checkout.root,
        ["status", "--porcelain", "--untracked-files=all"],
    )
    .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    })
}

/// Background tasks the input lists as running, described.
fn running_tasks(input: &Value) -> Vec<String> {
    let Some(tasks) = input.get("background_tasks").and_then(Value::as_array) else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter(|t| {
            t.get("status")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("running"))
        })
        .map(|t| {
            ["command", "description", "id"]
                .iter()
                .find_map(|k| t.get(*k).and_then(Value::as_str))
                .unwrap_or("?")
                .to_owned()
        })
        .collect()
}

/// The message the `compile:` line is looked for in: `last_assistant_message`, else the last
/// report in the transcript; `None` when there is neither.
fn last_message(input: &Value, subagent: bool) -> Option<String> {
    if let Some(m) = input.get("last_assistant_message").and_then(Value::as_str) {
        return Some(m.to_owned());
    }
    let key = if subagent {
        "agent_transcript_path"
    } else {
        "transcript_path"
    };
    let path = input.get(key).and_then(Value::as_str)?;
    transcript_report(&std::fs::read_to_string(path).ok()?)
}

/// The last report of a Claude Code transcript (JSON lines): the text of the last assistant
/// entry that has text or a `SubagentHandback` call.
pub fn transcript_report(jsonl: &str) -> Option<String> {
    jsonl.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        let msg = v.get("message")?;
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            return None;
        }
        let blocks = msg.get("content")?.as_array()?;
        let mut text = Vec::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.extend(b.get("text").and_then(Value::as_str)),
                Some("tool_use")
                    if b.get("name").and_then(Value::as_str) == Some("SubagentHandback") =>
                {
                    text.extend(b.pointer("/input/message").and_then(Value::as_str))
                }
                _ => {}
            }
        }
        (!text.is_empty()).then(|| text.join("\n"))
    })
}

/// True when some line of the message starts with `compile:` (leading blanks allowed).
pub fn has_compile_line(message: &str) -> bool {
    message
        .lines()
        .any(|l| l.trim_start().starts_with("compile:"))
}

/// Decides for one hook input. `None` lets the agent stop.
pub fn decide(input: &Value) -> Verdict {
    let event = input.get("hook_event_name").and_then(Value::as_str)?;
    let subagent = match event {
        "Stop" => false,
        "SubagentStop" => true,
        _ => return None,
    };
    let cwd = PathBuf::from(input.get("cwd").and_then(Value::as_str)?);
    let (checkout, branch) = atelier_worktree(&cwd)?;
    let mut problems = Vec::new();
    if !subagent {
        if rebase_open(&checkout) {
            problems.push(
                "a rebase is in progress in this worktree: resolve it and `git rebase --continue` (then `atelier follow` again), or `git rebase --abort`"
                    .to_owned(),
            );
        }
        match uncommitted(&checkout) {
            Some(lines) if !lines.is_empty() => {
                let shown: Vec<&str> = lines.iter().take(5).map(String::as_str).collect();
                problems.push(format!(
                    "the worktree has uncommitted changes: commit your own files by path (`git commit -m \"…\" -- <paths>`); {}{}",
                    shown.join(", "),
                    if lines.len() > shown.len() { ", …" } else { "" }
                ));
            }
            _ => {}
        }
        let tasks = running_tasks(input);
        if !tasks.is_empty() {
            problems.push(format!(
                "background tasks are still running: {}; wait for them or stop them",
                tasks.join("; ")
            ));
        }
    }
    if last_message(input, subagent).is_some_and(|m| !has_compile_line(&m)) {
        problems.push(
            "your last message has no line starting `compile:`: run `atelier check --compile-only` on the committed tree and copy its `compile:` line into your report"
                .to_owned(),
        );
    }
    (!problems.is_empty()).then(|| {
        format!(
            "atelier: do not stop yet in branch {branch}:\n- {}",
            problems.join("\n- ")
        )
    })
}

/// The output that blocks, the same for both harnesses.
pub fn block_output(reason: &str) -> String {
    json!({ "decision": "block", "reason": reason }).to_string()
}

/// `atelier hook stop`: reads the input, prints a block when one is due. Always exit 0.
pub fn stop_main() -> i32 {
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("atelier hook stop: cannot read the hook input: {e}");
        return 0;
    }
    let input: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("atelier hook stop: the hook input is not JSON: {e}");
            return 0;
        }
    };
    if let Some(reason) = decide(&input) {
        println!("{}", block_output(&reason));
    }
    0
}

// ------------------------------------------------------------------ the project hook files

/// True when `group` (one matcher group of an event) runs [`COMMAND`].
fn runs_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| {
            hs.iter()
                .any(|h| h.get("command").and_then(Value::as_str) == Some(COMMAND))
        })
}

/// The events of a hook file's JSON text that do not run the hook yet; empty when none. `Err`
/// when the text is not a JSON object with the shapes the harnesses read.
pub fn missing_events(text: &str) -> std::result::Result<Vec<&'static str>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let root = v.as_object().ok_or("not a JSON object")?;
    let hooks = match root.get("hooks") {
        None => return Ok(EVENTS.to_vec()),
        Some(h) => h.as_object().ok_or("`hooks` is not an object")?,
    };
    let mut missing = Vec::new();
    for ev in EVENTS {
        match hooks.get(ev) {
            None => missing.push(ev),
            Some(groups) => {
                let groups = groups
                    .as_array()
                    .ok_or_else(|| format!("`hooks.{ev}` is not an array"))?;
                if !groups.iter().any(runs_hook) {
                    missing.push(ev);
                }
            }
        }
    }
    Ok(missing)
}

/// The hook file's text with the hook added for every event that lacks it, every other key and
/// entry kept as it was, in its order. `old` is `None` for a file that does not exist. An old
/// text that already runs the hook everywhere is returned unchanged. `Err` as
/// [`missing_events`].
pub fn merged(old: Option<&str>, harness: Harness) -> std::result::Result<String, String> {
    let mut root: Map<String, Value> = match old {
        None => Map::new(),
        Some(t) if t.trim().is_empty() => Map::new(),
        Some(t) => match serde_json::from_str::<Value>(t).map_err(|e| format!("not JSON: {e}"))? {
            Value::Object(m) => m,
            _ => return Err("not a JSON object".into()),
        },
    };
    let missing = missing_events(&Value::Object(root.clone()).to_string())?;
    if missing.is_empty()
        && let Some(t) = old
    {
        return Ok(t.to_owned());
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("`hooks` is not an object")?;
    let mut handler = json!({ "type": "command", "command": COMMAND });
    if harness == Harness::Codex {
        handler["timeout"] = json!(30);
    }
    for ev in missing {
        hooks
            .entry(ev)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.{ev}` is not an array"))?
            .push(json!({ "hooks": [handler.clone()] }));
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_lines_start_a_line() {
        assert!(has_compile_line("done\ncompile: ok (3 crates)\n"));
        assert!(has_compile_line("  compile: ok"));
        assert!(!has_compile_line("the compile: step passed"));
        assert!(!has_compile_line(""));
    }

    #[test]
    fn the_merge_keeps_other_keys_and_their_order() {
        let old = r#"{"permissions":{"allow":["Bash(ls)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"x"}]}],"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]},"model":"opus"}"#;
        let new = merged(Some(old), Harness::ClaudeCode).unwrap();
        let v: Value = serde_json::from_str(&new).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        assert_eq!(v["model"], "opus");
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "mine");
        assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"], COMMAND);
        assert_eq!(
            v["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
            COMMAND
        );
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["permissions", "hooks", "model"]);
        assert!(missing_events(&new).unwrap().is_empty());
        // Merging again changes nothing.
        assert_eq!(merged(Some(&new), Harness::ClaudeCode).unwrap(), new);
        // Codex handlers carry a timeout.
        let c: Value = serde_json::from_str(&merged(None, Harness::Codex).unwrap()).unwrap();
        assert_eq!(c["hooks"]["Stop"][0]["hooks"][0]["timeout"], 30);
        // Shapes the harness would not read are refused, not replaced.
        assert!(merged(Some("[1]"), Harness::ClaudeCode).is_err());
        assert!(merged(Some(r#"{"hooks":[]}"#), Harness::ClaudeCode).is_err());
        assert!(merged(Some(r#"{"hooks":{"Stop":{}}}"#), Harness::ClaudeCode).is_err());
        assert!(merged(Some("{"), Harness::ClaudeCode).is_err());
    }

    #[test]
    fn harnesses_are_told_apart() {
        assert_eq!(harness(&json!({"session_id": "s"})), Harness::ClaudeCode);
        assert_eq!(
            harness(&json!({"session_id": "s", "turn_id": "t"})),
            Harness::Codex
        );
    }

    #[test]
    fn inputs_outside_an_atelier_worktree_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let input = json!({
            "hook_event_name": "Stop",
            "cwd": dir.path(),
            "last_assistant_message": "no line",
        });
        assert_eq!(decide(&input), None);
        assert_eq!(decide(&json!({"hook_event_name": "Stop"})), None);
        assert_eq!(
            decide(&json!({"hook_event_name": "PreToolUse", "cwd": "/"})),
            None
        );
    }
}
