//! `atelier status` and the conflict forecast (DESIGN §3 `status`, §8.1; module 3.8) on
//! throwaway repositories with the real binary. Runs of other times are fabricated as the run
//! folders `check` and `test` leave (`result`, `tests.tsv`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use atelier::ledger::{MergeLine, Promotion};
use atelier::locks::{HeldLock, LockKey};
use atelier::machines::MachineLock;
use atelier::platform::Platform;
use atelier::runs::{Outcome, RunKind, RunResult};
use atelier::state::{CURRENT_FORMAT, State};
use common::*;

fn this_platform() -> String {
    Platform::current().expect("a known platform").to_string()
}

const STEP: &str =
    "[[check.steps]]\nname = \"verify\"\nrun = [\"sh\", \"-c\", \"test ! -e BROKEN\"]\n";

struct Proj {
    repo: Repo,
    base: PathBuf,
}

impl Proj {
    fn new() -> Proj {
        Self::with_settings("")
    }

    fn with_settings(extra: &str) -> Proj {
        let repo = Repo::new();
        repo.install_self(VERSION);
        repo.write(".gitignore", "*.ignored\nbuild/\n");
        repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\npromotion_wait_seconds = 1\n\n[platforms.{}]\nrun = \"check\"\n\n{STEP}{extra}",
                this_platform()
            ),
        );
        repo.write_rules();
        repo.commit_all("pin");
        let base = repo.root.parent().unwrap().to_path_buf();
        Proj { repo, base }
    }

    fn main(&self) -> &Path {
        &self.repo.root
    }

    fn wt(&self, branch: &str) -> PathBuf {
        self.base.join(format!("wt-{}", branch.replace('/', "-")))
    }

    fn run(&self, args: &[&str]) -> Output {
        self.repo.atelier_in(self.main(), args)
    }

    fn status(&self) -> String {
        let out = self.run(&["status"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
        stdout(&out)
    }

    fn start(&self, branch: &str, on: Option<&str>) -> PathBuf {
        let wt = self.wt(branch);
        let mut args = vec!["start", branch];
        if let Some(l) = on {
            args.extend(["--on", l]);
        }
        let p = wt.to_str().unwrap().to_owned();
        args.extend(["--path", &p]);
        let out = self.run(&args);
        assert_eq!(code(&out), 0, "start {branch}: {}", stderr(&out));
        wt
    }

    /// Starts `branch`, commits `file` in its worktree, approves the tip.
    fn ready(&self, branch: &str, file: &str, contents: &str) -> String {
        let wt = self.start(branch, None);
        let c = commit(&wt, file, contents, branch);
        let out = self.run(&["approve", branch, &c]);
        assert_eq!(code(&out), 0, "approve {branch}: {}", stderr(&out));
        c
    }

    fn state(&self) -> State {
        State::open_at(self.main().join(".git/atelier")).unwrap()
    }

    fn promotion(&self) -> Option<Promotion> {
        let state = self.state();
        let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
        Promotion::read(&v).unwrap()
    }

    fn write_promotion(&self, rec: &Promotion) {
        let state = self.state();
        let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
        rec.write(&v).unwrap();
    }

    fn append_merge_line(&self, line: &MergeLine) {
        let state = self.state();
        let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
        line.append(&v).unwrap();
    }

    /// A run folder as a finished run leaves it.
    fn fabricate_run(&self, id: &str, kind: RunKind, outcome: Outcome, tree: &str, tsv: &str) {
        self.fabricate_run_on(id, kind, outcome, tree, &this_platform(), None, tsv);
    }

    #[allow(clippy::too_many_arguments)]
    fn fabricate_run_on(
        &self,
        id: &str,
        kind: RunKind,
        outcome: Outcome,
        tree: &str,
        platform: &str,
        machine: Option<&str>,
        tsv: &str,
    ) {
        let dir = self.main().join(".git/atelier/runs").join(id);
        fs::create_dir_all(&dir).unwrap();
        let time = format!(
            "{}-{}-{}T{}:{}:00Z",
            &id[0..4],
            &id[4..6],
            &id[6..8],
            &id[9..11],
            &id[11..13]
        );
        let has_tests = !tsv.is_empty();
        if has_tests {
            fs::write(dir.join("tests.tsv"), tsv).unwrap();
        }
        let result = RunResult {
            id: id.to_owned(),
            kind,
            command: "atelier check".to_owned(),
            outcome,
            tree: Some(tree.to_owned()),
            commit: None,
            uncommitted: None,
            branch: None,
            worktree: self.main().to_path_buf(),
            platform: Some(platform.to_owned()),
            machine: machine.map(str::to_owned),
            atelier_version: VERSION.to_owned(),
            started: time.clone(),
            ended: time,
            signal: None,
            failures: Vec::new(),
            survivors: Vec::new(),
            test_results: has_tests.then(|| "tests.tsv".to_owned()),
            tests: None,
            steps: Vec::new(),
        };
        fs::write(dir.join("result"), toml::to_string(&result).unwrap()).unwrap();
    }
}

fn run_id(minute: u32, n: u8) -> String {
    format!("20261113T10{minute:02}Z-{n:016x}")
}

fn commit(dir: &Path, file: &str, contents: &str, msg: &str) -> String {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["add", "-f", "--", file]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

/// The lines of one section of the text report.
fn section<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        if let Some(n) = l.strip_prefix("== ") {
            inside = n == name || n.starts_with(&format!("{name} ("));
            continue;
        }
        if inside {
            out.push(l);
        }
    }
    out
}

/// Every file and folder under `.git` with its size and modification time.
fn git_dir_snapshot(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![root.join(".git")];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let meta = fs::symlink_metadata(e.path()).unwrap();
            // git refreshes the modification time of an object it finds again.
            let when = if e.path().to_str().unwrap().contains("/.git/objects/") {
                std::time::UNIX_EPOCH
            } else {
                meta.modified().unwrap()
            };
            out.push((e.path(), meta.len(), when));
            if meta.is_dir() {
                todo.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn object_count(root: &Path) -> usize {
    git(root, &["count-objects", "-v"])
        .lines()
        .filter(|l| l.starts_with("count:") || l.starts_with("in-pack:"))
        .map(|l| {
            l.split(": ")
                .nth(1)
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        })
        .sum()
}

// ---------------------------------------------------------------------------------------------

#[test]
fn a_fresh_project_reports_every_section_and_exits_0() {
    let p = Proj::new();
    let text = p.status();
    for name in [
        "preflight",
        "main",
        "approvals not yet merged",
        "merges since the last clean",
        "last runs",
        "platforms",
        "machines",
        "must follow",
        "worktrees with no merge record",
        "flaky tests",
        "conflict forecast",
    ] {
        assert!(
            text.lines().any(|l| l == format!("== {name}")),
            "no section {name}:\n{text}"
        );
    }
    assert!(text.contains("ok     version"), "{text}");
    assert!(text.contains("ok     rules: current"), "{text}");
    assert!(text.contains("ok     integration: main pins"), "{text}");
    let main = section(&text, "main");
    assert!(main[0].starts_with("main at "), "{main:?}");
    assert!(main.contains(&"promotion: none"), "{main:?}");
    assert!(main.contains(&"main checkout: on main; no tracked changes"));
    assert_eq!(section(&text, "approvals not yet merged"), ["none"]);
    assert_eq!(section(&text, "last runs"), ["none"]);
    assert_eq!(section(&text, "must follow"), ["none"]);
    assert_eq!(section(&text, "flaky tests"), ["none"]);
    assert!(section(&text, "conflict forecast")[0].starts_with("no conflicts among 0 open"));
    let machines = section(&text, "machines");
    assert!(
        machines[0].starts_with("unavailable: no machines file"),
        "{machines:?}"
    );
}

#[test]
fn status_changes_nothing_and_a_usage_error_exits_2() {
    let p = Proj::new();
    p.ready("a", "a.txt", "a\n");
    p.ready("b", "b.txt", "b\n");
    let first = p.status();
    let before = git_dir_snapshot(p.main());
    let objects = object_count(p.main());
    let second = p.status();
    assert_eq!(first, second);
    let after = git_dir_snapshot(p.main());
    let changed: Vec<_> = after.iter().filter(|e| !before.contains(e)).collect();
    assert!(changed.is_empty(), "changed under .git: {changed:?}");
    assert_eq!(after.len(), before.len());
    assert_eq!(object_count(p.main()), objects, "no object was written");
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
    let out = p.run(&["status", "--bogus"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn json_has_the_same_sections() {
    let p = Proj::new();
    let out = p.run(&["status", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let names: Vec<&str> = v["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.first(), Some(&"preflight"));
    assert!(names.contains(&"flaky tests"));
    assert_eq!(v["sections"][0]["omitted"], 0);
}

#[test]
fn a_failed_preflight_still_exits_0_and_lists_every_failure() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write(
        ".atelier/atelier.toml",
        &format!(
            "atelier = \"{VERSION}\"\nbogus = true\n[platforms.macos-aarch64]\nrun = \"check\"\n"
        ),
    );
    let out = repo.atelier(&["status"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("FAILED settings"), "{text}");
    assert!(text.contains("== records"), "{text}");
}

#[test]
fn approvals_and_merges_are_listed_until_clean_deletes_them() {
    let p = Proj::new();
    let c = p.ready("feat", "f.txt", "f\n");
    let text = p.status();
    let approvals = section(&text, "approvals not yet merged");
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert!(
        approvals[0].starts_with(&format!("feat {}", &c[..7])),
        "{approvals:?}"
    );
    // Another branch moves after its approval.
    p.ready("other", "o.txt", "o\n");
    commit(&p.wt("other"), "o2.txt", "o2\n", "more");
    let text = p.status();
    let approvals = section(&text, "approvals not yet merged").join("\n");
    assert!(
        approvals.contains("other ") && approvals.contains("(the branch is now at "),
        "{approvals}"
    );

    let out = p.run(&["merge", "feat"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let text = p.status();
    let approvals = section(&text, "approvals not yet merged").join("\n");
    assert!(!approvals.contains("feat "), "{approvals}");
    let merges = section(&text, "merges since the last clean");
    assert_eq!(merges.len(), 1, "{merges:?}");
    assert!(
        merges[0].contains(" feat ") && merges[0].contains(&c[..7]),
        "{merges:?}"
    );
    // A merged branch's worktree has its merge record: not listed.
    let worktrees = section(&text, "worktrees with no merge record").join("\n");
    assert!(!worktrees.contains("wt-feat"), "{worktrees}");

    let out = p.run(&["clean", "feat", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let text = p.status();
    assert_eq!(section(&text, "merges since the last clean"), ["none"]);
}

/// A pending promotion: exit 7 on a tracked change, then the change undone.
fn pending(p: &Proj) -> Promotion {
    p.ready("feat", "new/added.txt", &"m content\n".repeat(50));
    fs::write(p.main().join("README"), "busy\n").unwrap();
    let out = p.run(&["merge", "feat"]);
    assert_eq!(code(&out), 7, "{}{}", stdout(&out), stderr(&out));
    git(p.main(), &["checkout", "--", "README"]);
    p.promotion().unwrap()
}

#[test]
fn a_pending_promotion_is_reported_and_nothing_is_touched() {
    let p = Proj::new();
    let rec = pending(&p);
    let before = git_dir_snapshot(p.main());
    let text = p.status();
    let main = section(&text, "main");
    let line = main.iter().find(|l| l.starts_with("promotion:")).unwrap();
    assert!(line.starts_with("promotion: pending, main "), "{line}");
    assert!(
        line.contains(&rec.run) && line.contains("atelier promote"),
        "{line}"
    );
    assert!(main.contains(&"  branches: feat"), "{main:?}");
    assert!(p.promotion().is_some());
    assert_eq!(git_dir_snapshot(p.main()), before);
}

#[test]
fn an_interrupted_promotion_is_reported_as_interrupted() {
    let p = Proj::new();
    let rec = pending(&p);
    let mut r = rec.clone();
    r.push_started = Some("2026-11-13T10:00:00Z".into());
    r.push_pgid = Some(999_999);
    r.push_ended = None;
    p.write_promotion(&r);
    // git left the files and the index at M, the ref at OLD.
    git(p.main(), &["read-tree", "-u", "-m", &rec.m]);
    let index_before = fs::read(p.main().join(".git/index")).unwrap();
    let text = p.status();
    let main = section(&text, "main");
    let line = main.iter().find(|l| l.starts_with("promotion:")).unwrap();
    assert!(line.starts_with("promotion: INTERRUPTED, main "), "{line}");
    assert!(
        main.iter()
            .any(|l| l.contains("push started 2026-11-13T10:00Z, not ended"))
    );
    assert_eq!(fs::read(p.main().join(".git/index")).unwrap(), index_before);
    assert_eq!(git(p.main(), &["rev-parse", "main"]), rec.old);
    assert!(p.promotion().is_some(), "status records nothing");
}

#[test]
fn a_promotion_that_happened_but_is_not_recorded_says_so() {
    let p = Proj::new();
    let rec = pending(&p);
    git(p.main(), &["merge", "-q", "--ff-only", &rec.m]);
    let text = p.status();
    let main = section(&text, "main");
    let line = main.iter().find(|l| l.starts_with("promotion:")).unwrap();
    assert!(line.contains("but not recorded"), "{line}");
    assert!(p.promotion().is_some(), "status does not record it");
}

#[test]
fn a_dependent_behind_a_rewritten_lower_branch_must_follow() {
    let p = Proj::new();
    let lower = p.start("lower", None);
    commit(&lower, "l.txt", "l\n", "lower work");
    let upper = p.start("upper", Some("lower"));
    commit(&upper, "u.txt", "u\n", "upper work");
    p.start("fresh", None);
    assert_eq!(section(&p.status(), "must follow"), ["none"]);

    // The lower branch is rewritten: the upper one is behind it.
    git(
        &lower,
        &["commit", "-q", "--amend", "-m", "lower work, reworded"],
    );
    let text = p.status();
    let follow = section(&text, "must follow");
    assert_eq!(follow.len(), 1, "{follow:?}");
    assert!(
        follow[0].starts_with("upper is behind a rewrite of lower: run atelier follow"),
        "{follow:?}"
    );

    // A start that did not finish is listed too.
    let state_dir = p.main().join(".git/atelier/branches");
    let rec = fs::read_to_string(state_dir.join("fresh.toml")).unwrap();
    fs::write(
        state_dir.join("fresh.toml"),
        format!("{rec}\n[starting]\nworktree = \"/nowhere\"\nnew_worktree = true\n"),
    )
    .unwrap();
    let follow = section(&p.status(), "must follow").join("\n");
    assert!(
        follow.contains("fresh: its start did not finish"),
        "{follow}"
    );
}

#[test]
fn worktrees_without_a_merge_record_are_listed() {
    let p = Proj::new();
    p.start("work", None);
    let stray = p.base.join("stray");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "stray",
            stray.to_str().unwrap(),
        ],
    );
    let detached = p.base.join("loose");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );
    let text = p.status();
    let list = section(&text, "worktrees with no merge record").join("\n");
    assert!(list.contains("wt-work work (active)"), "{list}");
    assert!(
        list.contains("stray stray (no record: not started by atelier)"),
        "{list}"
    );
    assert!(list.contains("loose - (detached)"), "{list}");
    assert!(!list.contains("repo.gate"), "{list}");
}

#[test]
fn a_flaky_test_is_one_that_failed_then_passed_on_the_same_tree_and_platform_in_full_runs() {
    let p = Proj::new();
    let (t1, t2, t3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let tsv = |cases: &[(&str, &str)]| {
        cases
            .iter()
            .map(|(n, v)| format!("w\tcrate::{n}\t{v}\t0.01\n"))
            .collect::<String>()
    };
    let pass = Outcome::Passed;
    let fail = Outcome::Failed;
    // Same tree and platform, full runs: failed, then passed.
    p.fabricate_run(
        &run_id(1, 1),
        RunKind::Check,
        fail,
        &t1,
        &tsv(&[("flaky", "fail"), ("steady", "pass")]),
    );
    p.fabricate_run(
        &run_id(2, 2),
        RunKind::Check,
        pass,
        &t1,
        &tsv(&[("flaky", "pass"), ("steady", "pass")]),
    );
    // Another tree: a fix, not a flaky test.
    p.fabricate_run(
        &run_id(3, 3),
        RunKind::Check,
        fail,
        &t2,
        &tsv(&[("fixed", "fail")]),
    );
    p.fabricate_run(
        &run_id(4, 4),
        RunKind::Check,
        pass,
        &t3,
        &tsv(&[("fixed", "pass")]),
    );
    // Narrow and compile-only runs never count.
    p.fabricate_run(
        &run_id(5, 5),
        RunKind::CheckOnly,
        fail,
        &t2,
        &tsv(&[("narrow", "fail")]),
    );
    p.fabricate_run(
        &run_id(6, 6),
        RunKind::TestNarrow,
        fail,
        &t2,
        &tsv(&[("narrow2", "fail")]),
    );
    p.fabricate_run(
        &run_id(7, 7),
        RunKind::Check,
        pass,
        &t2,
        &tsv(&[("narrow", "pass"), ("narrow2", "pass")]),
    );
    // Passed first, failed later: not "failed, then passed".
    p.fabricate_run(
        &run_id(8, 8),
        RunKind::Check,
        pass,
        &t2,
        &tsv(&[("regressed", "pass")]),
    );
    p.fabricate_run(
        &run_id(9, 9),
        RunKind::Check,
        fail,
        &t2,
        &tsv(&[("regressed", "fail")]),
    );
    // Same tree, other platform: not the same platform.
    p.fabricate_run_on(
        &run_id(10, 10),
        RunKind::TestFull,
        fail,
        &t1,
        "linux-x86_64",
        Some("box"),
        &tsv(&[("elsewhere", "fail")]),
    );
    p.fabricate_run(
        &run_id(11, 11),
        RunKind::Check,
        pass,
        &t1,
        &tsv(&[("elsewhere", "pass")]),
    );
    // On a platform's whole run, from another machine: counts.
    p.fabricate_run_on(
        &run_id(12, 12),
        RunKind::TestFull,
        fail,
        &t3,
        "linux-x86_64",
        Some("box"),
        &tsv(&[("remote", "fail")]),
    );
    p.fabricate_run_on(
        &run_id(13, 13),
        RunKind::TestFull,
        pass,
        &t3,
        "linux-x86_64",
        Some("box"),
        &tsv(&[("remote", "pass")]),
    );

    let text = p.status();
    let flaky = section(&text, "flaky tests");
    let all = flaky.join("\n");
    assert_eq!(flaky.len(), 2, "{all}");
    assert!(
        all.contains(&format!(
            "w crate::flaky: failed in {}, passed in {} ({}, tree {})",
            run_id(1, 1),
            run_id(2, 2),
            this_platform(),
            &t1[..12]
        )),
        "{all}"
    );
    assert!(
        all.contains("w crate::remote: failed in") && all.contains("(linux-x86_64, tree"),
        "{all}"
    );
    for not in ["steady", "fixed", "narrow", "regressed", "elsewhere"] {
        assert!(
            !all.contains(&format!("crate::{not}:")),
            "{not} is not flaky:\n{all}"
        );
    }
}

#[test]
fn last_runs_are_bounded_and_counted() {
    let p = Proj::new();
    for n in 0..12u8 {
        p.fabricate_run(
            &run_id(n as u32, n),
            RunKind::Check,
            Outcome::Passed,
            &"a".repeat(40),
            "",
        );
    }
    // A run that was killed leaves no result.
    let dir = p
        .main()
        .join(".git/atelier/runs")
        .join("20261113T1059Z-00000000000000ff");
    fs::create_dir_all(&dir).unwrap();
    let text = p.status();
    let runs = section(&text, "last runs");
    assert_eq!(runs.len(), 11, "{runs:?}");
    assert!(
        runs[0].starts_with("20261113T1059Z-00000000000000ff ?"),
        "{runs:?}"
    );
    assert!(runs[0].contains("NO RESULT (killed)"), "{runs:?}");
    assert!(runs[1].contains("check PASS"), "{runs:?}");
    assert_eq!(runs[10], "... 3 more not shown");
    let header = text
        .lines()
        .find(|l| l.starts_with("== last runs"))
        .unwrap();
    assert_eq!(header, "== last runs");
    // Every section carries the same bound.
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&p.run(&["status", "--json"]))).unwrap();
    let runs = v["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "last runs")
        .unwrap();
    assert_eq!(runs["lines"].as_array().unwrap().len(), 10);
    assert_eq!(runs["omitted"], 3);
}

#[test]
fn each_platform_shows_its_last_result_machine_and_waivers_since() {
    let p = Proj::with_settings("\n[platforms.linux-x86_64]\nrun = \"check\"\n");
    // The first gate: both platforms pass, the other on a machine.
    let mut line = MergeLine {
        time: "2026-11-13T10:00:00Z".into(),
        branch: "one".into(),
        lifetime: "aa".into(),
        commit: "a".repeat(40),
        old: "b".repeat(40),
        m: "c".repeat(40),
        run: run_id(0, 1),
        platforms: vec![
            format!("{}=pass@local", this_platform()),
            "linux-x86_64=pass@box".into(),
        ],
    };
    p.append_merge_line(&line);
    // Two later merges waive linux-x86_64.
    for (t, b) in [
        ("2026-11-13T11:00:00Z", "two"),
        ("2026-11-13T12:00:00Z", "three"),
    ] {
        line.time = t.into();
        line.branch = b.into();
        line.platforms = vec![
            format!("{}=pass@local", this_platform()),
            "linux-x86_64=waived:no box today".into(),
        ];
        p.append_merge_line(&line);
    }
    // This platform also failed a later check.
    p.fabricate_run(
        &run_id(30, 1),
        RunKind::Check,
        Outcome::Failed,
        &"a".repeat(40),
        "",
    );
    let text = p.status();
    let platforms = section(&text, "platforms").join("\n");
    assert!(
        platforms.contains(&format!(
            "{}: PASS 2026-11-13T12:00Z on this computer (merge of three)",
            this_platform()
        )),
        "{platforms}"
    );
    assert!(
        platforms.contains(
            "linux-x86_64: PASS 2026-11-13T10:00Z on box (merge of one); waived since: 2 (last 2026-11-13T12:00Z three: no box today)"
        ),
        "{platforms}"
    );
    // Later fabricated run (same day, after) shows as the platform's last result.
    p.fabricate_run(
        &format!("20261113T1330Z-{:016x}", 2),
        RunKind::Check,
        Outcome::Failed,
        &"b".repeat(40),
        "",
    );
    let platforms = section(&p.status(), "platforms").join("\n");
    assert!(
        platforms.contains(&format!(
            "{}: FAIL 2026-11-13T13:30Z on this computer (check)",
            this_platform()
        )),
        "{platforms}"
    );
    // A project with no records at all says so.
    let q = Proj::with_settings("\n[platforms.linux-x86_64]\nrun = \"check\"\n");
    let text = q.status();
    assert!(section(&text, "platforms").contains(&"linux-x86_64: no run recorded"));
}

#[test]
fn machines_are_listed_without_taking_the_machine_lock() {
    let p = Proj::new();
    let file = p.repo.home.join(".config/atelier/machines.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        format!(
            "[machines.here]\nlocal = true\nplatform = \"{}\"\ngui = true\npriority = 100\n",
            this_platform()
        ),
    )
    .unwrap();
    let text = p.status();
    let machines = section(&text, "machines");
    assert!(machines[0].starts_with("NAME"), "{machines:?}");
    assert!(machines[1].starts_with("here"), "{machines:?}");
    assert!(machines[1].contains("free"), "{machines:?}");

    // While another atelier holds the machine lock, status says busy, does not wait, and does
    // not take or release the lock.
    let lock = MachineLock::try_take(&p.repo.home, "a test")
        .unwrap()
        .unwrap();
    let text = p.status();
    let machines = section(&text, "machines");
    assert!(machines[1].contains("busy"), "{machines:?}");
    assert!(
        MachineLock::try_take(&p.repo.home, "again")
            .unwrap()
            .is_none(),
        "still held"
    );
    drop(lock);
    // A held gate lock does not stop status either.
    let state = p.state();
    let _gate = HeldLock::acquire(&state, LockKey::Gate).unwrap();
    assert_eq!(code(&p.run(&["status"])), 0);
}

// ---------------------------------------------------------------------------------------------
// The conflict forecast

#[test]
fn two_branches_conflicting_on_one_file_are_listed_and_disjoint_ones_are_not() {
    let p = Proj::new();
    // The shared file exists on main.
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    commit(&a, "shared.txt", "ONE\ntwo\nthree\n", "a edits");
    commit(&a, "only-a.txt", "a\n", "a file");
    let b = p.start("b", None);
    commit(&b, "shared.txt", "uno\ntwo\nthree\n", "b edits");
    let c = p.start("c", None);
    commit(&c, "only-c.txt", "c\n", "c file");
    let objects = object_count(p.main());

    let text = p.status();
    let forecast = section(&text, "conflict forecast");
    assert_eq!(forecast, ["a x b: shared.txt"], "{forecast:?}");
    assert_eq!(
        object_count(p.main()),
        objects,
        "the forecast wrote no object"
    );
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
}

#[test]
fn a_branch_that_conflicts_with_main_is_listed_with_its_files() {
    let p = Proj::new();
    commit(p.main(), "shared.txt", "one\ntwo\n", "shared");
    let a = p.start("a", None);
    commit(&a, "shared.txt", "ONE\ntwo\n", "a edits");
    commit(&a, "dir/x.txt", "x\n", "x");
    commit(p.main(), "shared.txt", "uno\ntwo\n", "main edits");
    // a is now behind main; the forecast still sees the conflict of their trees.
    let text = p.status();
    let forecast = section(&text, "conflict forecast");
    assert_eq!(forecast, ["a x main: shared.txt"], "{forecast:?}");
}

#[test]
fn merge_warns_about_a_conflict_with_another_open_branch_and_refuses_nothing() {
    let p = Proj::new();
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    let ca = commit(&a, "shared.txt", "ONE\ntwo\nthree\n", "a edits");
    let b = p.start("b", None);
    commit(&b, "shared.txt", "uno\ntwo\nthree\n", "b edits");
    let out = p.run(&["approve", "a", &ca]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = p.run(&["merge", "a"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("warning: forecast: a x b: shared.txt"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn merge_is_silent_when_no_open_branch_conflicts() {
    let p = Proj::new();
    p.ready("a", "a.txt", "a\n");
    let b = p.start("b", None);
    commit(&b, "b.txt", "b\n", "b");
    let out = p.run(&["merge", "a"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!stdout(&out).contains("forecast"), "{}", stdout(&out));
}
