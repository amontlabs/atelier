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
        Proj::with_settings_in(Repo::new(), extra)
    }

    fn with_settings_in(repo: Repo, extra: &str) -> Proj {
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

fn merge_line(time: &str, branch: &str, run: &str, platforms: &[String]) -> MergeLine {
    MergeLine {
        time: time.into(),
        branch: branch.into(),
        lifetime: "aa".into(),
        commit: "a".repeat(40),
        old: "b".repeat(40),
        m: "c".repeat(40),
        run: run.into(),
        platforms: platforms.to_vec(),
    }
}

impl Proj {
    /// A gate run as `merge` leaves it.
    fn fabricate_gate(&self, id: &str, outcome: Outcome, branch: &str, failures: &[&str]) {
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
        let result = RunResult {
            id: id.to_owned(),
            kind: RunKind::Gate,
            command: format!("merge {branch}"),
            outcome,
            tree: Some("d".repeat(40)),
            commit: Some("e".repeat(40)),
            uncommitted: Some(false),
            branch: Some(branch.to_owned()),
            worktree: self.main().to_path_buf(),
            // The gate's run records this computer's platform for the whole gate.
            platform: Some(this_platform()),
            machine: None,
            atelier_version: VERSION.to_owned(),
            started: time.clone(),
            ended: time,
            signal: None,
            failures: failures.iter().map(|f| (*f).to_owned()).collect(),
            survivors: Vec::new(),
            test_results: None,
            tests: None,
            steps: Vec::new(),
        };
        fs::write(dir.join("result"), toml::to_string(&result).unwrap()).unwrap();
    }
}

#[test]
fn each_platform_shows_the_last_gate_in_which_it_ran_and_waivers_are_never_a_pass() {
    let p = Proj::with_settings("\n[platforms.linux-x86_64]\nrun = \"check\"\n");
    let this = this_platform();
    // The first gate: both platforms pass, the other on a machine.
    p.append_merge_line(&merge_line(
        "2026-11-13T10:00:00Z",
        "one",
        &run_id(0, 1),
        &[format!("{this}=pass@local"), "linux-x86_64=pass@box".into()],
    ));
    let platforms = section(&p.status(), "platforms").join("\n");
    assert!(
        platforms.contains(&format!(
            "{this}: PASS 2026-11-13T10:00Z on this computer (merge of one, ccccccc)"
        )),
        "{platforms}"
    );
    assert!(
        platforms.contains("linux-x86_64: PASS 2026-11-13T10:00Z on box (merge of one, ccccccc)"),
        "{platforms}"
    );
    // A later gate, green with linux waived: linux is waived, never PASS, and says what passed.
    p.append_merge_line(&merge_line(
        "2026-11-13T10:20:00Z",
        "two",
        &run_id(0, 2),
        &[
            format!("{this}=pass@local"),
            "linux-x86_64=waived:no box today".into(),
        ],
    ));
    // The gate's own run records this computer's platform; it is not a result for it.
    p.fabricate_gate(
        &run_id(30, 3),
        Outcome::Failed,
        "three",
        &["conflict: three with main"],
    );
    // A worker's failing check on its own branch is not the state of main.
    p.fabricate_run(
        &run_id(40, 4),
        RunKind::Check,
        Outcome::Failed,
        &"a".repeat(40),
        "",
    );
    let text = p.status();
    let platforms = section(&text, "platforms").join("\n");
    assert!(
        platforms.contains(&format!(
            "{this}: PASS 2026-11-13T10:20Z on this computer (merge of two, ccccccc)"
        )),
        "{platforms}"
    );
    assert!(
        platforms.contains(
            "linux-x86_64: WAIVED 2026-11-13T10:20Z (merge of two, ccccccc): no box today; last passed: PASS 2026-11-13T10:00Z on box (merge of one, ccccccc)"
        ),
        "{platforms}"
    );
    assert!(!platforms.contains("FAIL"), "{platforms}");
    let runs = section(&text, "last runs").join("\n");
    assert!(
        runs.contains(&format!("{} check FAIL", run_id(40, 4))),
        "{runs}"
    );
    assert!(
        runs.contains(&format!("{} gate FAIL three", run_id(30, 3))),
        "{runs}"
    );
    assert!(
        !runs.contains(&format!("gate FAIL three {this}")),
        "the gate has no platform of its own: {runs}"
    );

    // A gate that failed on a platform names it.
    p.fabricate_gate(
        &run_id(50, 5),
        Outcome::Failed,
        "four",
        &[&format!("{this}: check failed")],
    );
    let platforms = section(&p.status(), "platforms").join("\n");
    assert!(
        platforms.contains(&format!(
            "{this}: FAIL 2026-11-13T10:50Z (gate of four, run {}, eeeeeee, not merged); last passed: PASS 2026-11-13T10:20Z",
            run_id(50, 5)
        )),
        "{platforms}"
    );

    // A project with no gate at all says so.
    let q = Proj::with_settings("\n[platforms.linux-x86_64]\nrun = \"check\"\n");
    assert!(section(&q.status(), "platforms").contains(&"linux-x86_64: no gate has run it"));
}

#[test]
fn a_real_gate_that_ends_on_a_conflict_says_nothing_about_a_platform() {
    let p = Proj::new();
    commit(p.main(), "shared.txt", "one\ntwo\n", "shared");
    p.ready("a", "shared.txt", "A\ntwo\n");
    p.ready("b", "shared.txt", "B\ntwo\n");
    let out = p.run(&["merge", "a", "b"]);
    assert_eq!(code(&out), 5, "{}{}", stdout(&out), stderr(&out));
    let text = p.status();
    let platforms = section(&text, "platforms");
    assert_eq!(
        platforms,
        [format!("{}: no gate has run it", this_platform())],
        "{platforms:?}"
    );
    let runs = section(&text, "last runs");
    assert!(runs[0].contains(" gate FAIL a b"), "{runs:?}");
    assert!(!runs[0].contains(&this_platform()), "{runs:?}");
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

// ---------------------------------------------------------------------------------------------
// The review round (finding numbers in the names)

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn promotion_line(text: &str) -> String {
    section(text, "main")
        .iter()
        .find(|l| l.starts_with("promotion:"))
        .unwrap()
        .to_string()
}

#[test]
fn f1_a_promotion_in_progress_is_running_with_no_advice() {
    let p = Proj::new();
    let rec = pending(&p);
    let mut r = rec.clone();
    r.push_started = Some("2026-11-13T10:00:00Z".into());
    r.push_pgid = Some(999_999);
    r.push_ended = None;
    p.write_promotion(&r);
    git(p.main(), &["read-tree", "-u", "-m", &rec.m]);

    // The gate lock is held (merge, or a promote, is at work): never INTERRUPTED or pending.
    let state = p.state();
    let held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
    let line = promotion_line(&p.status());
    assert_eq!(line, "promotion: running (merge or promote in progress)");
    drop(held);
    // Not held, and the group is dead: it is interrupted again.
    assert!(promotion_line(&p.status()).starts_with("promotion: INTERRUPTED"));

    // Not held, but the recorded push group is alive with its recorded start time.
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = child.id() as i32;
    r.push_pgid = Some(pgid);
    p.write_promotion(&r);
    let mut lock = HeldLock::acquire(&state, LockKey::Gate).unwrap();
    lock.add_group(pgid, &rec.run, atelier::procs::start_time(pgid))
        .unwrap();
    drop(lock);
    let line = promotion_line(&p.status());
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(line, "promotion: running (merge or promote in progress)");
    assert!(p.promotion().is_some(), "status records nothing");
}

#[test]
fn f1_a_promotion_waiting_for_a_busy_checkout_while_merge_holds_the_lock_is_running() {
    let p = Proj::new();
    pending(&p);
    let state = p.state();
    let _held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
    assert_eq!(
        promotion_line(&p.status()),
        "promotion: running (merge or promote in progress)"
    );
}

const REGEN: &str = "\n[[merge.after]]\nname = \"regen\"\nrun = [\"sh\", \"-c\", \"true\"]\ncommit = \"Regenerate gen.txt\"\nregenerates = [\"gen.txt\"]\n";

fn regen_project() -> Proj {
    // The regenerated file exists before the settings name it.
    let repo = Repo::new();
    repo.write("gen.txt", "zero\n");
    repo.commit_all("gen");
    Proj::with_settings_in(repo, REGEN)
}

#[test]
fn f2_a_branch_that_changes_the_rules_is_never_filtered_and_others_still_are() {
    let p = regen_project();
    let a = p.start("a", None);
    commit(&a, "gen.txt", "zero\na\n", "a edits gen");
    commit(
        &a,
        ".atelier/hooks/note.sh",
        "echo hi\n",
        "a adds a hook file",
    );
    let b = p.start("b", None);
    commit(&b, "gen.txt", "zero\nb\n", "b edits gen");
    commit(p.main(), "gen.txt", "zero\nmain\n", "main edits gen");
    let text = p.status();
    let forecast = section(&text, "conflict forecast");
    // b only touches the regenerated path: the gate settles it. a changes .atelier/: it does not.
    assert_eq!(
        forecast,
        ["a x main: gen.txt", "a x b: gen.txt"],
        "{forecast:?}"
    );
    // The gate agrees: it settles nothing for a, and refuses on the conflict.
    let ca = git(&a, &["rev-parse", "HEAD"]);
    let out = p.run(&["approve", "a", &ca]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = p.run(&["merge", "a"]);
    assert!(
        stdout(&out).contains("warning: forecast: a x main: gen.txt"),
        "{}",
        stdout(&out)
    );
    assert_eq!(code(&out), 5, "{}{}", stdout(&out), stderr(&out));
}

#[test]
fn f2_a_branch_that_removes_the_hook_entry_is_never_filtered() {
    let p = regen_project();
    let a = p.start("a", None);
    let settings = fs::read_to_string(a.join(".atelier/atelier.toml")).unwrap();
    let cut = settings.find("\n[[merge.after]]").unwrap();
    commit(
        &a,
        ".atelier/atelier.toml",
        &settings[..cut],
        "a drops the hook",
    );
    commit(&a, "gen.txt", "zero\na\n", "a edits gen");
    commit(p.main(), "gen.txt", "zero\nmain\n", "main edits gen");
    let forecast = section(&p.status(), "conflict forecast").join("\n");
    assert_eq!(forecast, "a x main: gen.txt");
}

#[test]
fn f3_merge_forecasts_every_pair_of_its_batch_however_many_branches_are_open() {
    let p = Proj::new();
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    for i in 0..23 {
        let name = format!("b{i:02}");
        let wt = p.start(&name, None);
        commit(&wt, &format!("only-{name}.txt"), "x\n", &name);
    }
    let y = p.start("y", None);
    commit(&y, "shared.txt", "ONE\ntwo\nthree\n", "y edits");
    let z = p.start("z", None);
    let cz = commit(&z, "shared.txt", "uno\ntwo\nthree\n", "z edits");
    let out = p.run(&["approve", "z", &cz]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    // 25 branches: status stops at 300 calls and says so.
    let text = p.status();
    assert!(
        section(&text, "conflict forecast")[0].ends_with("pairs not forecast"),
        "{text}"
    );
    let out = p.run(&["merge", "z"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("warning: forecast: y x z: shared.txt"),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("not forecast"), "{}", stdout(&out));
}

#[test]
fn f4_f5_a_waiver_in_the_last_gate_is_never_shown_as_a_pass() {
    // See each_platform_shows_the_last_gate_in_which_it_ran_and_waivers_are_never_a_pass; this
    // one drives a real waived merge.
    let other = if this_platform() == "linux-x86_64" {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    };
    let p = Proj::with_settings(&format!(
        "\n[platforms.{other}]\nrun = [\"sh\", \"-c\", \"true\"]\n"
    ));
    p.ready("feat", "a.txt", "a\n");
    let out = p.run(&["merge", "feat", "--waive", other, "--reason", "no box"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let platforms = section(&p.status(), "platforms").join("\n");
    assert!(
        platforms.contains(&format!("{other}: WAIVED ")) && platforms.contains("no box"),
        "{platforms}"
    );
    assert!(
        !platforms.contains(&format!("{other}: PASS")),
        "{platforms}"
    );
    assert!(
        platforms.contains(&format!("{}: PASS ", this_platform())),
        "{platforms}"
    );
    assert!(platforms.contains("merge of feat, "), "{platforms}");
}

/// A repository where `shared.txt -merge` (git's binary merge: any change on both sides
/// conflicts) on main; `a` drops the attribute and edits the first line, `b` the last one.
fn attributes_project() -> Proj {
    let p = Proj::new();
    commit(
        p.main(),
        ".gitattributes",
        "shared.txt -merge\n",
        "attributes",
    );
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    git(&a, &["rm", "-q", ".gitattributes"]);
    commit(
        &a,
        "shared.txt",
        "ONE\ntwo\nthree\n",
        "a drops the attribute and edits",
    );
    let b = p.start("b", None);
    commit(&b, "shared.txt", "one\ntwo\ntres\n", "b edits");
    p
}

#[test]
fn f6_the_forecast_reads_the_integration_tips_attributes_from_any_checkout() {
    let p = attributes_project();
    let from_main = section(&p.status(), "conflict forecast").join("\n");
    let from_a = {
        let out = p.repo.atelier_in(&p.wt("a"), &["status"]);
        assert_eq!(code(&out), 0);
        section(&stdout(&out), "conflict forecast").join("\n")
    };
    assert_eq!(from_main, from_a);
    // main's attribute (-merge) is what the forecast uses, also from a's worktree, which has
    // no attribute file. A pair forecast cannot see that `merge a b` would hand a's removal to
    // b (documented).
    assert_eq!(from_main, "a x b: shared.txt");
    // And the working copy's attribute file does not matter either.
    fs::write(p.main().join(".gitattributes"), "shared.txt merge=text\n").unwrap();
    let again = section(&p.status(), "conflict forecast").join("\n");
    assert_eq!(again, from_main);
    git(p.main(), &["checkout", "--", ".gitattributes"]);
}

fn driver_project(marker: &Path) -> Proj {
    let p = Proj::new();
    git(
        p.main(),
        &[
            "config",
            "merge.probe.driver",
            &format!("sh -c 'pwd >> {}; exit 0' %O %A %B", marker.display()),
        ],
    );
    commit(
        p.main(),
        ".gitattributes",
        "shared.txt merge=probe\n",
        "attributes",
    );
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    commit(&a, "shared.txt", "ONE\ntwo\nthree\n", "a edits");
    let b = p.start("b", None);
    commit(&b, "shared.txt", "uno\ntwo\nthree\n", "b edits");
    p
}

#[test]
fn f7_a_pair_the_gate_would_refuse_for_a_merge_driver_is_reported_and_no_driver_runs() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("ran");
    let p = driver_project(&marker);
    let text = p.status();
    let forecast = section(&text, "conflict forecast");
    assert_eq!(
        forecast,
        ["a x b: would be refused: shared.txt uses merge driver probe"],
        "{forecast:?}"
    );
    assert!(!marker.exists(), "status ran the custom merge driver");
    // merge warns the same way, and still runs the driver for nothing either.
    let ca = git(&p.wt("a"), &["rev-parse", "HEAD"]);
    let out = p.run(&["approve", "a", &ca]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = p.run(&["merge", "a"]);
    assert!(
        stdout(&out).contains(
            "warning: forecast: a x b: would be refused: shared.txt uses merge driver probe"
        ),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        !marker.exists(),
        "merge's forecast ran the custom merge driver"
    );
}

#[test]
fn f7_a_pair_merged_by_union_is_reported_as_refused_as_the_gate_does() {
    let p = Proj::new();
    commit(
        p.main(),
        ".gitattributes",
        "shared.txt merge=union\n",
        "attributes",
    );
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    commit(&a, "shared.txt", "ONE\ntwo\nthree\n", "a edits");
    let b = p.start("b", None);
    commit(&b, "shared.txt", "uno\ntwo\nthree\n", "b edits");
    let text = p.status();
    let forecast = section(&text, "conflict forecast");
    assert_eq!(
        forecast,
        ["a x b: would be refused: shared.txt uses merge driver union"],
        "{forecast:?}"
    );
}

#[test]
fn f8_a_run_record_with_a_tree_that_is_not_an_object_id_is_unreadable_not_a_panic() {
    let p = Proj::new();
    p.fabricate_run(
        &run_id(1, 1),
        RunKind::Check,
        Outcome::Passed,
        "aaaaaaaaaaaébbb",
        "ws\tt\tpass\n",
    );
    p.fabricate_run(
        &run_id(2, 2),
        RunKind::Check,
        Outcome::Passed,
        &"a".repeat(40),
        "ws\tt\tfail\n",
    );
    // A result that is not even TOML.
    let dir = p.main().join(".git/atelier/runs").join(run_id(3, 3));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("result"), "not = [valid\n").unwrap();
    let out = p.run(&["status"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let runs = section(&stdout(&out), "last runs").join("\n");
    assert!(
        runs.contains(&format!("{} unreadable:", run_id(1, 1))),
        "{runs}"
    );
    assert!(
        runs.contains(&format!("{} unreadable:", run_id(3, 3))),
        "{runs}"
    );
    assert!(
        runs.contains(&format!("{} check PASS", run_id(2, 2))),
        "{runs}"
    );
}

#[test]
fn f9_merge_checks_machines_before_it_forecasts() {
    let other = if this_platform() == "linux-x86_64" {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    };
    let p = Proj::with_settings(&format!(
        "\n[platforms.{other}]\nrun = [\"sh\", \"-c\", \"true\"]\n"
    ));
    commit(p.main(), "shared.txt", "one\ntwo\nthree\n", "shared");
    let a = p.start("a", None);
    let ca = commit(&a, "shared.txt", "ONE\ntwo\nthree\n", "a edits");
    let b = p.start("b", None);
    commit(&b, "shared.txt", "uno\ntwo\nthree\n", "b edits");
    let out = p.run(&["approve", "a", &ca]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = p.run(&["merge", "a"]);
    assert_eq!(code(&out), 3, "{}{}", stdout(&out), stderr(&out));
    assert!(
        !stdout(&out).contains("forecast"),
        "no forecast before the machine check passes: {}",
        stdout(&out)
    );
}

#[test]
fn f10_lines_that_must_not_be_missed_are_never_cut_by_the_cap() {
    let many: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
    let s = atelier::status::Section::pinned("x", vec!["3 pairs not forecast".to_owned()], many);
    assert_eq!(s.lines.len(), 11);
    assert_eq!(s.lines[0], "3 pairs not forecast");
    assert_eq!(s.omitted, 2);
}

#[test]
fn f10_a_dead_process_forecast_folder_is_removed_by_the_next_forecast() {
    let p = Proj::new();
    let tmp = tempfile::tempdir().unwrap();
    let dead = {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        pid
    };
    let gone = tmp.path().join(format!("atelier-forecast-{dead}-0f0f0f0f"));
    let live = tmp
        .path()
        .join(format!("atelier-forecast-{}-0e0e0e0e", std::process::id()));
    for d in [&gone, &live] {
        fs::create_dir_all(d.join("objects")).unwrap();
        fs::write(d.join("objects/x"), "x").unwrap();
    }
    let t = tmp.path().to_str().unwrap();
    let out = p.repo.atelier_env(p.main(), &["status"], &[("TMPDIR", t)]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(!gone.exists(), "left by a dead process");
    assert!(live.exists(), "a live process's folder stays");
    let left: Vec<_> = fs::read_dir(tmp.path()).unwrap().flatten().collect();
    assert_eq!(left.len(), 1, "{left:?}");
}

#[test]
fn f10_a_broken_promotion_record_does_not_take_the_rest_of_main_with_it() {
    let p = Proj::new();
    pending(&p);
    let state = p.state();
    let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
    fs::write(v.dir().join("promotion.toml"), "this is = [not toml\n").unwrap();
    let text = p.status();
    let main = section(&text, "main");
    assert!(main[0].starts_with("main at "), "{main:?}");
    assert!(
        main.iter().any(|l| l.starts_with("main checkout:")),
        "{main:?}"
    );
    assert!(
        main.iter().any(|l| l.starts_with("promotion: unreadable:")),
        "{main:?}"
    );
}
