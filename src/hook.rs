//! `atelier hook stop`: the Stop and SubagentStop hook of Claude
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
//! the parent session's. Both harnesses take the same block: `{"decision": "block",
//! "reason": "…"}` on stdout, exit 0. Every other case, and any failure of the hook itself, lets
//! the agent stop: the hook never exits 2 (which both harnesses read as a block) and is a strong
//! nudge, not a guarantee; atelier's own commands still refuse what matters.
//!
//! The hook does not block forever. It counts, per session (`<state>/hooks/<session id>`), how
//! many times in a row it blocked for each condition; the count starts again with each turn
//! (`stop_hook_active` false) and for a condition that was gone. After [`MAX_BLOCKS`] blocks in
//! a row for one condition, and at once for a condition its advice cannot clear (a merge in
//! progress: a commit by path is refused during a merge; a submodule with changes: no commit in
//! this worktree cleans it), it lets the agent stop, prints a warning naming the condition on
//! stderr and records it in the session file.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::fsutil;
use crate::git::{self, Checkout};
use crate::records::{BranchRecord, BranchState};
use crate::state::{STATE_DIR, State};

/// The command the project hook files run. `|| true`: a pinned atelier older than the hook
/// fails on `hook` (clap's exit 2, which both harnesses read as a block); this way it lets the
/// agent stop instead.
pub const COMMAND: &str = "sh -c 'atelier hook stop || true'";

/// What [`COMMAND`] runs, recognised in a hook file with any path before `atelier`.
const HOOK_WORDS: &str = "atelier hook stop";

/// Blocks in a row for one condition before the hook lets the agent stop anyway.
pub const MAX_BLOCKS: u32 = 3;

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
fn atelier_worktree(cwd: &Path) -> Option<(Checkout, String, State)> {
    let checkout = Checkout::find(cwd).ok()??;
    if !checkout.common_dir.join(STATE_DIR).is_dir() {
        return None;
    }
    let branch = branch_of(&checkout)?;
    let state = State::open(&checkout).ok()?;
    let rec = BranchRecord::read(&state, &branch).ok()??;
    (rec.state == BranchState::Active && rec.starting.is_none())
        .then_some((checkout, branch, state))
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

fn merge_open(checkout: &Checkout) -> bool {
    checkout.git_dir.join("MERGE_HEAD").exists()
}

/// One changed path of `git status`: `XY path`, and whether it is a submodule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    line: String,
    submodule: bool,
}

/// The entries of `git status --porcelain=v2` output (`1`, `2`, `u` and `?` lines).
fn parse_status_v2(text: &str) -> Vec<Change> {
    let mut out = Vec::new();
    for l in text.lines() {
        // The fields before the path: 8 for `1`, 9 for `2` (its path is `path<TAB>orig`), 10
        // for `u`; `?` has the path at once.
        let (fields, n) = match l.as_bytes().first() {
            Some(b'1') => (l.splitn(9, ' ').collect::<Vec<_>>(), 9),
            Some(b'2') => (l.splitn(10, ' ').collect::<Vec<_>>(), 10),
            Some(b'u') => (l.splitn(11, ' ').collect::<Vec<_>>(), 11),
            Some(b'?') => {
                out.push(Change {
                    line: format!("?? {}", &l[2.min(l.len())..]),
                    submodule: false,
                });
                continue;
            }
            _ => continue,
        };
        if fields.len() != n {
            continue;
        }
        let path = fields[n - 1].split('\t').next().unwrap_or_default();
        out.push(Change {
            line: format!("{} {path}", fields[1].replace('.', " ")),
            submodule: fields[2].starts_with('S'),
        });
    }
    out
}

/// The worktree's changes, untracked files included; `None` when git fails.
fn changes(checkout: &Checkout) -> Option<Vec<Change>> {
    let out = git::output(
        &checkout.root,
        ["status", "--porcelain=v2", "--untracked-files=all"],
    )
    .ok()?;
    out.status
        .success()
        .then(|| parse_status_v2(&String::from_utf8_lossy(&out.stdout)))
}

fn shown(lines: &[&str]) -> String {
    let first: Vec<&str> = lines.iter().take(5).copied().collect();
    format!(
        "{}{}",
        first.join(", "),
        if lines.len() > first.len() {
            ", …"
        } else {
            ""
        }
    )
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

/// One condition the hook found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// `rebase`, `merge`, `uncommitted`, `submodule`, `background` or `compile`.
    pub key: &'static str,
    /// The advice, for the agent.
    pub text: String,
    /// False when following the advice cannot clear it.
    pub fixable: bool,
}

/// The conditions for one input, with the worktree's branch and state; `None` when the hook
/// does not act here.
fn find(input: &Value) -> Option<(String, State, Vec<Problem>)> {
    let event = input.get("hook_event_name").and_then(Value::as_str)?;
    let subagent = match event {
        "Stop" => false,
        "SubagentStop" => true,
        _ => return None,
    };
    let cwd = PathBuf::from(input.get("cwd").and_then(Value::as_str)?);
    let (checkout, branch, state) = atelier_worktree(&cwd)?;
    let mut problems = Vec::new();
    let mut add = |key, fixable, text: String| problems.push(Problem { key, text, fixable });
    if !subagent {
        let merging = merge_open(&checkout);
        if rebase_open(&checkout) {
            add(
                "rebase",
                true,
                "a rebase is in progress in this worktree: resolve it and `git rebase --continue` (then `atelier follow` again), or `git rebase --abort`"
                    .to_owned(),
            );
        }
        if merging {
            add(
                "merge",
                false,
                "a merge is in progress in this worktree (a commit by path is refused during a merge): conclude it with `git commit`, or `git merge --abort`"
                    .to_owned(),
            );
        }
        let all = changes(&checkout).unwrap_or_default();
        let subs: Vec<&str> = all
            .iter()
            .filter(|c| c.submodule)
            .map(|c| c.line.as_str())
            .collect();
        let files: Vec<&str> = all
            .iter()
            .filter(|c| !c.submodule)
            .map(|c| c.line.as_str())
            .collect();
        // During a merge the changes are the merge's: the merge condition names them.
        if !files.is_empty() && !merging {
            add(
                "uncommitted",
                true,
                format!(
                    "the worktree has uncommitted changes: commit your own files by path (`git commit -m \"…\" -- <paths>`); {}",
                    shown(&files)
                ),
            );
        }
        if !subs.is_empty() {
            add(
                "submodule",
                false,
                format!(
                    "a submodule has changes of its own, which no commit in this worktree cleans: commit or discard them inside the submodule; {}",
                    shown(&subs)
                ),
            );
        }
        let tasks = running_tasks(input);
        if !tasks.is_empty() {
            add(
                "background",
                true,
                format!(
                    "background tasks are still running: {}; wait for them or stop them",
                    tasks.join("; ")
                ),
            );
        }
    }
    if last_message(input, subagent).is_some_and(|m| !has_compile_line(&m)) {
        add(
            "compile",
            true,
            "your last message has no line starting `compile:`: run `atelier check --compile-only` on the committed tree and copy its `compile:` line into your report"
                .to_owned(),
        );
    }
    Some((branch, state, problems))
}

/// What the hook does for one input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// The reason, when it blocks.
    pub block: Option<String>,
    /// Conditions it lets the agent stop with, for stderr.
    pub warnings: Vec<String>,
}

/// The session file: `<state>/hooks/<session id>` (`docs/FROZEN-LAYER.md`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SessionFile {
    /// Blocks in a row, by condition.
    #[serde(default)]
    pub blocks: std::collections::BTreeMap<String, u32>,
    /// The stops let through with a condition, last first ([`MAX_WARNINGS`] at most).
    #[serde(default)]
    pub warning: Vec<Warning>,
}

/// A stop the hook let through while a condition remained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    /// RFC 3339 UTC.
    pub at: String,
    pub branch: String,
    /// The condition's key ([`Problem::key`]).
    pub condition: String,
    /// `repeated` after [`MAX_BLOCKS`] blocks in a row, `unfixable` at once.
    pub why: String,
    pub detail: String,
}

/// Warnings a session file keeps.
pub const MAX_WARNINGS: usize = 20;

/// The session file for `id`; `None` for an id that is not a plain name.
pub fn session_path(state: &State, id: &str) -> Option<PathBuf> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    ok.then(|| state.root().join("hooks").join(id))
}

/// Reads a session file; an unreadable one counts as empty.
pub fn read_session(path: &Path) -> SessionFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Decides for one hook input, counting blocks in the session's file.
pub fn decide(input: &Value) -> Decision {
    let Some((branch, state, problems)) = find(input) else {
        return Decision::default();
    };
    let path = input
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|id| session_path(&state, id));
    let mut file = path.as_deref().map(read_session).unwrap_or_default();
    if input.get("stop_hook_active").and_then(Value::as_bool) == Some(false) {
        file.blocks.clear();
    }
    file.blocks
        .retain(|k, _| problems.iter().any(|p| p.fixable && p.key == k));
    let count = |k: &str, f: &SessionFile| f.blocks.get(k).copied().unwrap_or(0);
    let (blocking, through): (Vec<&Problem>, Vec<&Problem>) = problems
        .iter()
        .partition(|p| p.fixable && (path.is_none() || count(p.key, &file) < MAX_BLOCKS));
    let mut d = Decision::default();
    if blocking.is_empty() {
        let at = fsutil::rfc3339(fsutil::now_secs());
        for p in &through {
            let why = if p.fixable { "repeated" } else { "unfixable" };
            d.warnings.push(if p.fixable {
                format!(
                    "atelier: letting the agent stop in branch {branch} after {MAX_BLOCKS} blocks in a row for the same condition ({}): {}",
                    p.key, p.text
                )
            } else {
                format!(
                    "atelier: letting the agent stop in branch {branch} with a condition the hook's advice cannot clear ({}): {}",
                    p.key, p.text
                )
            });
            file.warning.insert(
                0,
                Warning {
                    at: at.clone(),
                    branch: branch.clone(),
                    condition: p.key.to_owned(),
                    why: why.to_owned(),
                    detail: p.text.clone(),
                },
            );
        }
        file.warning.truncate(MAX_WARNINGS);
        file.blocks.clear();
    } else {
        for p in &blocking {
            *file.blocks.entry(p.key.to_owned()).or_insert(0) += 1;
        }
        d.block = Some(format!(
            "atelier: do not stop yet in branch {branch}:\n- {}",
            blocking
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
                .join("\n- ")
        ));
    }
    if let Some(path) = &path {
        let keep = !file.blocks.is_empty() || !file.warning.is_empty();
        if keep {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(text) = toml::to_string(&file) {
                let _ = fsutil::write_atomic(path, text.as_bytes());
            }
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
    d
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
    let d = decide(&input);
    for w in &d.warnings {
        eprintln!("{w}");
    }
    if let Some(reason) = d.block {
        println!("{}", block_output(&reason));
    }
    0
}

// ------------------------------------------------------------------ the project hook files

/// True when a handler's command runs the hook: `atelier hook stop` with any path before
/// `atelier`, alone or inside a shell command ([`COMMAND`]).
pub fn runs_hook_command(command: &str) -> bool {
    command.match_indices(HOOK_WORDS).any(|(i, _)| {
        let before = command[..i].chars().next_back();
        let after = command[i + HOOK_WORDS.len()..].chars().next();
        before.is_none_or(|c| c.is_whitespace() || "/'\"(;&|".contains(c))
            && after.is_none_or(|c| c.is_whitespace() || "'\");&|".contains(c))
    })
}

/// True when `group` (one matcher group of an event) runs the hook.
fn runs_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(runs_hook_command)
            })
        })
}

/// The events of a hook file's JSON text (a UTF-8 BOM allowed) that do not run the hook yet;
/// empty when none. `Err` when the text is not a JSON object with the shapes the harnesses read.
pub fn missing_events(text: &str) -> std::result::Result<Vec<&'static str>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
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

/// The hook entries for `events`, as a hook file holds them: `{"hooks": {<event>: [...]}}`.
pub fn entries(events: &[&str], harness: Harness) -> Value {
    let mut handler = json!({ "type": "command", "command": COMMAND });
    if harness == Harness::Codex {
        handler["timeout"] = json!(30);
    }
    let mut hooks = Map::new();
    for ev in events {
        hooks.insert((*ev).to_owned(), json!([{ "hooks": [handler.clone()] }]));
    }
    json!({ "hooks": hooks })
}

/// What `init` and `rules` do with one hook file. An existing file is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFile {
    /// The file does not exist: create it with this text.
    Create(String),
    /// The file runs the hook on every event: nothing to do.
    Current,
    /// The file exists without the hook (on `missing`, or unreadable: `why`): it is left as it
    /// is, and the owner adds `snippet` by hand.
    AddByHand {
        missing: Vec<&'static str>,
        why: Option<String>,
        snippet: String,
    },
}

/// What to do with a hook file whose text is `old` (`None`: there is none).
pub fn plan(old: Option<&str>, harness: Harness) -> HookFile {
    let Some(old) = old else {
        let mut text = serde_json::to_string_pretty(&entries(&EVENTS, harness))
            .expect("JSON values serialize");
        text.push('\n');
        return HookFile::Create(text);
    };
    let (missing, why) = match missing_events(old) {
        Ok(m) if m.is_empty() => return HookFile::Current,
        Ok(m) => (m, None),
        Err(why) => (EVENTS.to_vec(), Some(why)),
    };
    let snippet =
        serde_json::to_string_pretty(&entries(&missing, harness)).expect("JSON values serialize");
    HookFile::AddByHand {
        missing,
        why,
        snippet,
    }
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
    fn an_existing_hook_file_is_never_rewritten() {
        // Missing: created, with both events; Codex handlers carry a timeout.
        let HookFile::Create(text) = plan(None, Harness::Codex) else {
            panic!("not created");
        };
        let c: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(c["hooks"]["Stop"][0]["hooks"][0]["command"], COMMAND);
        assert_eq!(c["hooks"]["SubagentStop"][0]["hooks"][0]["timeout"], 30);
        assert_eq!(plan(Some(&text), Harness::Codex), HookFile::Current);
        // Already there, under another spelling, with a BOM: nothing to do.
        let other = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/Users/me/.local/bin/atelier hook stop"}]}],"SubagentStop":[{"hooks":[{"type":"command","command":"atelier hook stop"}]}]}}"#;
        assert_eq!(plan(Some(other), Harness::ClaudeCode), HookFile::Current);
        assert_eq!(
            plan(Some(&format!("\u{feff}{other}")), Harness::ClaudeCode),
            HookFile::Current
        );
        // Without it, or unreadable: left alone, with the entries to add.
        let old = r#"{"cleanupPeriodDays": 1e2, "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "mine"}]}]}}"#;
        let HookFile::AddByHand {
            missing,
            why,
            snippet,
        } = plan(Some(old), Harness::ClaudeCode)
        else {
            panic!("not left alone");
        };
        assert_eq!(missing, EVENTS);
        assert_eq!(why, None);
        let v: Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], COMMAND);
        assert!(v["hooks"]["Stop"][0]["hooks"][0].get("timeout").is_none());
        let half = r#"{"hooks":{"Stop":[{"hooks":[{"command":"atelier hook stop"}]}]}}"#;
        let HookFile::AddByHand { missing, .. } = plan(Some(half), Harness::ClaudeCode) else {
            panic!("not left alone");
        };
        assert_eq!(missing, ["SubagentStop"]);
        for bad in [
            "[1]",
            r#"{"hooks":[]}"#,
            r#"{"hooks":{"Stop":{}}}"#,
            "{",
            "",
        ] {
            let HookFile::AddByHand { why, .. } = plan(Some(bad), Harness::ClaudeCode) else {
                panic!("{bad}: not left alone");
            };
            assert!(why.is_some(), "{bad}");
        }
    }

    #[test]
    fn the_hook_command_is_recognised_with_any_path() {
        for c in [
            "atelier hook stop",
            "/Users/me/.local/bin/atelier hook stop",
            COMMAND,
            "sh -c \"atelier hook stop\"",
            "atelier hook stop || true",
        ] {
            assert!(runs_hook_command(c), "{c}");
        }
        for c in [
            "notatelier hook stop",
            "atelier hook stopped",
            "atelier hook",
            "say done",
        ] {
            assert!(!runs_hook_command(c), "{c}");
        }
    }

    #[test]
    fn status_v2_tells_submodules_apart() {
        let text = "1 .M N... 100644 100644 100644 abc abc README\n\
                    1 .M S.M. 160000 160000 160000 abc abc sub\n\
                    2 R. N... 100644 100644 100644 abc abc R100 new name\told\n\
                    u UU N... 100644 100644 100644 100644 a b c both.txt\n\
                    ? un tracked.txt\n";
        let got = parse_status_v2(text);
        let lines: Vec<(&str, bool)> = got.iter().map(|c| (c.line.as_str(), c.submodule)).collect();
        assert_eq!(
            lines,
            [
                (" M README", false),
                (" M sub", true),
                ("R  new name", false),
                ("UU both.txt", false),
                ("?? un tracked.txt", false),
            ]
        );
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
        assert_eq!(decide(&input), Decision::default());
        assert_eq!(
            decide(&json!({"hook_event_name": "Stop"})),
            Decision::default()
        );
        assert_eq!(
            decide(&json!({"hook_event_name": "PreToolUse", "cwd": "/"})),
            Decision::default()
        );
    }
}
