//! `atelier hook stop` with recorded and documented hook inputs (`tests/fixtures/hooks/`), for
//! each condition and harness, and the hook files `init` and `rules` write.

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
    for f in STOPS.iter().chain(&[
        "claude-subagent-stop-with-message.json",
        "codex-subagent-stop.json",
    ]) {
        assert_eq!(p.hook(&input(f, &p.wt, None)), None, "{f}");
    }
}

#[test]
fn no_compile_line_blocks() {
    let p = Proj::new();
    for f in STOPS.iter().chain(&[
        "claude-subagent-stop-with-message.json",
        "codex-subagent-stop.json",
    ]) {
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
    for f in [
        "claude-subagent-stop-with-message.json",
        "codex-subagent-stop.json",
    ] {
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
    // A shell task, as Claude Code reports it, running and then finished.
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
        p.hook(&input(
            "claude-subagent-stop-with-message.json",
            &p.wt,
            None
        )),
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

const COMMAND: &str = "sh -c 'atelier hook stop || true'";

#[test]
fn init_and_rules_create_missing_hook_files_and_never_rewrite_one() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    // An existing settings file without the hook: numbers, layout, other keys all stay.
    let claude_old = "{\n\t\"cleanupPeriodDays\": 1e2,\n\t\"big\": 18446744073709551616,\n\t\"name\": \"caf\u{e9}\",\n\t\"hooks\": {\"Stop\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"say done\"}]}]}\n}\n";
    repo.write(".claude/settings.json", claude_old);
    let out = repo.atelier(&["init", "--version", VERSION]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(repo.root.join(".claude/settings.json")).unwrap(),
        claude_old,
        "never rewritten"
    );
    // The exact entries to add are printed.
    let printed = stdout(&out);
    assert!(
        printed.contains("left .claude/settings.json as it is"),
        "{printed}"
    );
    assert!(
        printed.contains(&serde_json::to_string(COMMAND).unwrap()),
        "{printed}"
    );
    // The missing Codex file is created.
    let codex: Value =
        serde_json::from_str(&fs::read_to_string(repo.root.join(".codex/hooks.json")).unwrap())
            .unwrap();
    for ev in ["Stop", "SubagentStop"] {
        assert_eq!(codex["hooks"][ev][0]["hooks"][0]["command"], COMMAND);
        assert_eq!(codex["hooks"][ev][0]["hooks"][0]["timeout"], 30);
    }
    repo.commit_all("init");
    // Until the owner adds it, the rules are stale, naming the file.
    let out = repo.atelier(&["agents"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("rules are stale"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(".claude/settings.json: hook missing on Stop and SubagentStop"),
        "{}",
        stderr(&out)
    );
    // `rules` does not write it either.
    let out = repo.atelier(&["rules"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("left .claude/settings.json as it is"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".claude/settings.json")).unwrap(),
        claude_old
    );

    // Added by hand under another spelling (an absolute path), with a UTF-8 BOM: current, and
    // the file is left byte for byte.
    let by_hand = "\u{feff}{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"/Users/me/.local/bin/atelier hook stop\"}]}],\"SubagentStop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"atelier hook stop\"}]}]},\"n\":1e2}";
    repo.write(".claude/settings.json", by_hand);
    let out = repo.atelier(&["rules"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("rules are current"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".claude/settings.json")).unwrap(),
        by_hand
    );
    let out = repo.atelier(&["agents"]);
    assert!(
        !stderr(&out).contains("rules are stale"),
        "{}",
        stderr(&out)
    );

    // A file the harness could not read is left alone too; `rules` still writes the rest.
    repo.write(".codex/hooks.json", "{ not json");
    fs::remove_file(repo.root.join("AGENTS.md")).unwrap();
    let out = repo.atelier(&["rules"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("left .codex/hooks.json as it is (not JSON"),
        "{}",
        stdout(&out)
    );
    assert!(repo.root.join("AGENTS.md").is_file());
    assert_eq!(
        fs::read_to_string(repo.root.join(".codex/hooks.json")).unwrap(),
        "{ not json"
    );
    let out = repo.atelier(&["agents"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(".codex/hooks.json: hook missing (not JSON"),
        "{}",
        stderr(&out)
    );
}

/// Runs the hook as `hook_in` does, returning the block's reason and stderr.
fn hook_full(repo: &Repo, stdin: &Value) -> (Option<String>, String) {
    let mut child = repo
        .atelier_cmd(&repo.root, &["hook", "stop"], &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    let reason = (!text.trim().is_empty()).then(|| {
        let v: Value = serde_json::from_str(text.trim()).unwrap();
        v["reason"].as_str().unwrap().to_owned()
    });
    (reason, stderr(&out))
}

fn session_file(p: &Proj, id: &str) -> PathBuf {
    p.repo.root.join(".git/atelier/hooks").join(id)
}

#[test]
fn the_same_condition_blocks_three_times_in_a_row_then_lets_the_agent_stop() {
    let p = Proj::new();
    fs::write(p.wt.join("kept-recreated.txt"), "x").unwrap();
    let first = input("claude-stop.json", &p.wt, None);
    let id = first["session_id"].as_str().unwrap().to_owned();
    let again = with(first.clone(), "stop_hook_active", Value::from(true));
    for (n, v) in [&first, &again, &again].iter().enumerate() {
        let (reason, _) = hook_full(&p.repo, v);
        assert!(
            reason.is_some_and(|r| r.contains("uncommitted changes")),
            "block {}",
            n + 1
        );
    }
    let (reason, err) = hook_full(&p.repo, &again);
    assert_eq!(reason, None, "the fourth stop goes through");
    assert!(err.contains("after 3 blocks in a row"), "{err}");
    assert!(err.contains("uncommitted"), "{err}");
    let file = fs::read_to_string(session_file(&p, &id)).unwrap();
    assert!(file.contains("condition = \"uncommitted\""), "{file}");
    assert!(file.contains("why = \"repeated\""), "{file}");
    // The count starts again: the next turn blocks again.
    let (reason, _) = hook_full(&p.repo, &first);
    assert!(reason.is_some());
    // A condition that is gone stops its count; another condition counts on its own.
    fs::remove_file(p.wt.join("kept-recreated.txt")).unwrap();
    let lazy = with(again.clone(), "last_assistant_message", Value::from("done"));
    for _ in 0..3 {
        let (reason, _) = hook_full(&p.repo, &lazy);
        assert!(reason.is_some_and(|r| r.contains("compile:")));
    }
    let (reason, err) = hook_full(&p.repo, &lazy);
    assert_eq!(reason, None);
    assert!(err.contains("(compile)"), "{err}");
    // An input with no session id blocks every time, as it cannot be counted.
    let mut anon = lazy.clone();
    anon.as_object_mut().unwrap().remove("session_id");
    for _ in 0..5 {
        assert!(hook_full(&p.repo, &anon).0.is_some());
    }
}

#[test]
fn a_merge_in_progress_or_a_changed_submodule_lets_the_agent_stop_at_once() {
    let p = Proj::new();
    let wt = &p.wt;
    let v = input("claude-stop.json", wt, None);
    let id = v["session_id"].as_str().unwrap().to_owned();
    // A merge in progress: `git commit -- <paths>` is refused, so the advice cannot clear it.
    fs::write(p.repo.root.join("on-main.txt"), "m\n").unwrap();
    p.repo.commit_all("on main");
    let main = git(&p.repo.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    git(wt, &["merge", "--no-commit", "--no-ff", main.trim()]);
    assert!(
        git_cmd(wt)
            .args(["commit", "-m", "x", "--", "on-main.txt"])
            .output()
            .unwrap()
            .status
            .code()
            != Some(0)
    );
    let (reason, err) = hook_full(&p.repo, &v);
    assert_eq!(reason, None);
    assert!(err.contains("a merge is in progress"), "{err}");
    assert!(err.contains("cannot clear"), "{err}");
    git(wt, &["merge", "--abort"]);

    // A submodule with changes of its own.
    let sub_src = Repo::new();
    sub_src.write("s.txt", "s\n");
    sub_src.commit_all("s");
    git(
        wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_src.root.to_str().unwrap(),
            "sub",
        ],
    );
    git(wt, &["commit", "-q", "-m", "sub"]);
    assert_eq!(hook_full(&p.repo, &v).0, None);
    fs::write(wt.join("sub/s.txt"), "changed\n").unwrap();
    let (reason, err) = hook_full(&p.repo, &v);
    assert_eq!(reason, None, "not blocked for the submodule");
    assert!(err.contains("(submodule)") && err.contains("sub"), "{err}");
    let file = fs::read_to_string(session_file(&p, &id)).unwrap();
    assert!(file.contains("condition = \"submodule\""), "{file}");
    assert!(file.contains("condition = \"merge\""), "{file}");
    // With a fixable condition beside it, that one still blocks.
    fs::write(wt.join("mine.txt"), "x").unwrap();
    let (reason, _) = hook_full(&p.repo, &v);
    let reason = reason.unwrap();
    assert!(reason.contains("mine.txt"), "{reason}");
    assert!(!reason.contains("submodule"), "{reason}");
}

#[test]
fn a_pinned_atelier_without_the_hook_command_does_not_block() {
    let p = Proj::new();
    // An older version, pinned in this worktree, that has no `hook` command: clap's usage error.
    let old = p.repo.home.join(".local/share/atelier/0.0.1/bin");
    fs::create_dir_all(&old).unwrap();
    fs::write(
        old.join("atelier"),
        "#!/bin/sh\necho \"error: unrecognized subcommand 'hook'\" >&2\nexit 2\n",
    )
    .unwrap();
    fs::set_permissions(
        old.join("atelier"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let settings = p.wt.join(".atelier/atelier.toml");
    let text = fs::read_to_string(&settings).unwrap();
    fs::write(
        &settings,
        text.replace(&format!("atelier = \"{VERSION}\""), "atelier = \"0.0.1\""),
    )
    .unwrap();
    // The dispatcher on PATH, as the harness finds it.
    let bin = p.repo.tmp.path().join("path-bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(atelier_bin(), bin.join("atelier")).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let stdin = with(
        input("claude-stop.json", &p.wt, None),
        "last_assistant_message",
        Value::from("no line"),
    )
    .to_string();
    let run = |command: &str| {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", command])
            .current_dir(&p.wt)
            .env("HOME", &p.repo.home)
            .env("PATH", &path)
            .env_remove("ATELIER_DISPATCHED")
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
        child.wait_with_output().unwrap()
    };
    // The bare command exits 2, which both harnesses read as a block.
    let bare = run("atelier hook stop");
    assert_eq!(code(&bare), 2, "{}", stderr(&bare));
    assert!(stderr(&bare).contains("unrecognized subcommand"));
    // The registered one lets the agent stop.
    let registered: Value =
        serde_json::from_str(&fs::read_to_string(p.wt.join(".claude/settings.json")).unwrap())
            .unwrap();
    let command = registered["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(command, COMMAND);
    let out = run(&command);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
}
