//! `atelier hook stop` with recorded and documented hook inputs (`tests/fixtures/hooks/`), for
//! each condition and harness, and the hook files `init` and `rules` write (DESIGN §8.5).

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;

use common::*;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hooks");

/// A project after `init`, committed, with branch `w` started in its own worktree.
struct Proj {
    repo: Repo,
    wt: PathBuf,
}

impl Proj {
    fn new() -> Proj {
        let repo = Repo::new();
        repo.install_self(VERSION);
        let out = repo.atelier(&["init", "--version", VERSION]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        repo.commit_all("init");
        let wt = repo.root.parent().unwrap().join("wt-w");
        let out = repo.atelier(&["start", "w", "--path", wt.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        Proj { repo, wt }
    }

    /// Runs the hook with `input` on stdin, from the main checkout (the harness's own folder
    /// does not matter: the input's `cwd` does). Returns the block's reason, if any.
    fn hook(&self, input: &Value) -> Option<String> {
        hook_in(&self.repo, &self.repo.root, &input.to_string())
    }
}

fn hook_in(repo: &Repo, dir: &Path, stdin: &str) -> Option<String> {
    let mut child = repo
        .atelier_cmd(dir, &["hook", "stop"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "the hook always exits 0: {}", stderr(&out));
    let text = stdout(&out);
    if text.trim().is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(v["decision"], "block", "{text}");
    Some(v["reason"].as_str().unwrap().to_owned())
}

/// A fixture with `@CWD@` (and `@TRANSCRIPT@`) replaced.
fn input(name: &str, cwd: &Path, transcript: Option<&Path>) -> Value {
    let text = fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap();
    let text = text.replace("@CWD@", cwd.to_str().unwrap()).replace(
        "@TRANSCRIPT@",
        transcript.map(|p| p.to_str().unwrap()).unwrap_or(""),
    );
    serde_json::from_str(&text).unwrap()
}

fn with(mut v: Value, key: &str, value: Value) -> Value {
    v[key] = value;
    v
}

const STOPS: [&str; 2] = ["claude-stop.json", "codex-stop.json"];

#[test]
fn a_finished_worker_may_stop_in_both_harnesses() {
    let p = Proj::new();
    for f in STOPS
        .iter()
        .chain(&["claude-subagent-stop-p23.json", "codex-subagent-stop.json"])
    {
        assert_eq!(p.hook(&input(f, &p.wt, None)), None, "{f}");
    }
}

#[test]
fn no_compile_line_blocks() {
    let p = Proj::new();
    for f in STOPS
        .iter()
        .chain(&["claude-subagent-stop-p23.json", "codex-subagent-stop.json"])
    {
        for msg in [
            Value::from("All done."),
            Value::from("the compile: step ran"),
        ] {
            let reason = p
                .hook(&with(input(f, &p.wt, None), "last_assistant_message", msg))
                .unwrap_or_else(|| panic!("{f}: not blocked"));
            assert!(reason.contains("compile:"), "{f}: {reason}");
            assert!(reason.contains("branch w"), "{f}: {reason}");
        }
    }
}

#[test]
fn uncommitted_changes_block_a_stop_but_not_a_subagent_stop() {
    let p = Proj::new();
    fs::write(p.wt.join("new.txt"), "x").unwrap();
    for f in STOPS {
        let reason = p.hook(&input(f, &p.wt, None)).expect(f);
        assert!(reason.contains("uncommitted changes"), "{f}: {reason}");
        assert!(reason.contains("new.txt"), "{f}: {reason}");
        assert!(!reason.contains("compile:` "), "{f}: {reason}");
    }
    // A subagent's input carries the parent's folder: only its message is checked.
    for f in ["claude-subagent-stop-p23.json", "codex-subagent-stop.json"] {
        assert_eq!(p.hook(&input(f, &p.wt, None)), None, "{f}");
    }
    // A tracked change counts too.
    fs::remove_file(p.wt.join("new.txt")).unwrap();
    fs::write(p.wt.join("README"), "changed\n").unwrap();
    assert!(
        p.hook(&input("claude-stop.json", &p.wt, None))
            .unwrap()
            .contains("README")
    );
}

#[test]
fn a_rebase_in_progress_blocks() {
    let p = Proj::new();
    let wt = &p.wt;
    git(wt, &["switch", "-q", "-c", "other"]);
    fs::write(wt.join("f"), "other\n").unwrap();
    git(wt, &["add", "f"]);
    git(wt, &["commit", "-q", "-m", "other"]);
    git(wt, &["switch", "-q", "w"]);
    fs::write(wt.join("f"), "mine\n").unwrap();
    git(wt, &["add", "f"]);
    git(wt, &["commit", "-q", "-m", "mine"]);
    let out = git_cmd(wt).args(["rebase", "other"]).output().unwrap();
    assert!(!out.status.success(), "the rebase stops on a conflict");
    // HEAD is detached; the branch is the one being rebased.
    for f in STOPS {
        let reason = p.hook(&input(f, wt, None)).expect(f);
        assert!(reason.contains("rebase is in progress"), "{f}: {reason}");
        assert!(reason.contains("branch w"), "{f}: {reason}");
    }
    git(wt, &["rebase", "--abort"]);
    assert_eq!(p.hook(&input("claude-stop.json", wt, None)), None);
}

#[test]
fn a_running_background_task_blocks_claude_code() {
    let p = Proj::new();
    // Recorded: the parent ends its turn while its subagent runs in the background.
    let reason = p
        .hook(&with(
            input("claude-stop-subagent-running.json", &p.wt, None),
            "last_assistant_message",
            Value::from("waiting\ncompile: ok"),
        ))
        .unwrap();
    assert!(reason.contains("background tasks"), "{reason}");
    assert!(reason.contains("Trivial compile check"), "{reason}");
    // A shell task, as P23 saw it, running and then finished.
    let running =
        serde_json::json!([{"type": "shell", "status": "running", "command": "sleep 240"}]);
    let reason = p
        .hook(&with(
            input("claude-stop.json", &p.wt, None),
            "background_tasks",
            running,
        ))
        .unwrap();
    assert!(reason.contains("sleep 240"), "{reason}");
    let done =
        serde_json::json!([{"type": "shell", "status": "completed", "command": "sleep 240"}]);
    assert_eq!(
        p.hook(&with(
            input("claude-stop.json", &p.wt, None),
            "background_tasks",
            done
        )),
        None
    );
    // A subagent's input lists the parent's tasks (itself included): not checked.
    assert_eq!(
        p.hook(&input("claude-subagent-stop-p23.json", &p.wt, None)),
        None
    );
}

#[test]
fn a_subagent_report_handed_back_is_read_from_its_transcript() {
    let p = Proj::new();
    let transcript = Path::new(FIXTURES).join("claude-agent-transcript.jsonl");
    // Recorded: no last_assistant_message; the report ("compile: ok") is in the transcript.
    let v = input("claude-subagent-stop.json", &p.wt, Some(&transcript));
    assert!(v.get("last_assistant_message").is_none());
    assert_eq!(p.hook(&v), None);
    // A handed-back report without the line blocks.
    let other = p.repo.tmp.path().join("t.jsonl");
    fs::write(
        &other,
        fs::read_to_string(&transcript)
            .unwrap()
            .replace("\"message\": \"compile: ok\"", "\"message\": \"done\""),
    )
    .unwrap();
    let reason = p
        .hook(&input("claude-subagent-stop.json", &p.wt, Some(&other)))
        .unwrap();
    assert!(reason.contains("compile:"), "{reason}");
    // No message and no readable transcript: nothing to check, the subagent may stop.
    assert_eq!(
        p.hook(&input(
            "claude-subagent-stop.json",
            &p.wt,
            Some(&p.repo.tmp.path().join("missing.jsonl"))
        )),
        None
    );
}

#[test]
fn outside_a_started_worktree_the_hook_lets_the_agent_stop() {
    let p = Proj::new();
    let lazy = |cwd: &Path| {
        with(
            input("claude-stop.json", cwd, None),
            "last_assistant_message",
            Value::from("no line"),
        )
    };
    // The main checkout is on the integration branch, which atelier did not start.
    fs::write(p.repo.root.join("dirty"), "x").unwrap();
    assert_eq!(p.hook(&lazy(&p.repo.root)), None);
    // A plain folder, and a git repository without atelier, with the hook run from there.
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(
        hook_in(&p.repo, plain.path(), &lazy(plain.path()).to_string()),
        None
    );
    let other = Repo::new();
    assert_eq!(
        hook_in(&p.repo, &other.root, &lazy(&other.root).to_string()),
        None
    );
    // A branch that is no longer active (merged) is not checked either.
    let state = atelier::state::State::open(&p.repo.checkout()).unwrap();
    let mut rec = atelier::records::BranchRecord::read(&state, "w")
        .unwrap()
        .unwrap();
    rec.state = atelier::records::BranchState::Merged;
    rec.write(&state).unwrap();
    assert_eq!(p.hook(&lazy(&p.wt)), None);
    // Input that is not JSON, or another event, is let through.
    assert_eq!(hook_in(&p.repo, &p.repo.root, "not json"), None);
    assert_eq!(
        p.hook(&with(
            lazy(&p.wt),
            "hook_event_name",
            Value::from("PreToolUse")
        )),
        None
    );
}

#[test]
fn init_and_rules_register_the_hook_and_keep_other_settings() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write(
        ".claude/settings.json",
        r#"{"permissions":{"allow":["Bash(ls)"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"say done"}]}]}}"#,
    );
    let out = repo.atelier(&["init", "--version", VERSION]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let claude: Value =
        serde_json::from_str(&fs::read_to_string(repo.root.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(claude["permissions"]["allow"][0], "Bash(ls)");
    assert_eq!(
        claude["hooks"]["Stop"][0]["hooks"][0]["command"],
        "say done"
    );
    assert_eq!(
        claude["hooks"]["Stop"][1]["hooks"][0]["command"],
        "atelier hook stop"
    );
    assert_eq!(
        claude["hooks"]["SubagentStop"][0]["hooks"][0]["command"],
        "atelier hook stop"
    );
    let codex: Value =
        serde_json::from_str(&fs::read_to_string(repo.root.join(".codex/hooks.json")).unwrap())
            .unwrap();
    for ev in ["Stop", "SubagentStop"] {
        assert_eq!(
            codex["hooks"][ev][0]["hooks"][0]["command"],
            "atelier hook stop"
        );
        assert_eq!(codex["hooks"][ev][0]["hooks"][0]["timeout"], 30);
    }
    repo.commit_all("init");

    // Removing the hook makes the rules stale; `atelier rules` puts it back and keeps the rest.
    repo.write(
        ".claude/settings.json",
        r#"{"permissions":{"allow":["Bash(ls)"]},"env":{"A":"1"}}"#,
    );
    let out = repo.atelier(&["agents"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("rules are stale"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(".claude/settings.json does not run"),
        "{}",
        stderr(&out)
    );
    let out = repo.atelier(&["rules"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("wrote .claude/settings.json"),
        "{}",
        stdout(&out)
    );
    let text = fs::read_to_string(repo.root.join(".claude/settings.json")).unwrap();
    let claude: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(claude["env"]["A"], "1");
    let keys: Vec<&String> = claude.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["permissions", "env", "hooks"]);
    // Current again; a second run writes nothing.
    let out = repo.atelier(&["rules"]);
    assert!(
        stdout(&out).contains("rules are current"),
        "{}",
        stdout(&out)
    );

    // A settings file the harness could not read is refused, never replaced.
    repo.write(".codex/hooks.json", "{ not json");
    let out = repo.atelier(&["rules"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(".codex/hooks.json"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".codex/hooks.json")).unwrap(),
        "{ not json"
    );
}
