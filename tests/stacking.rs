//! `start`, `follow`, the branch record lifecycle and the dependent guard (DESIGN §2.4, §8.2), on
//! throwaway repositories. Each REVIEW-4 probe (findings 1–5, round 2's abort-then-amend) and
//! each S4 case is replayed against the real binary.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use atelier::git::Checkout;
use atelier::records::{BranchRecord, BranchState};
use atelier::stack::{Behind, Currency, Stack, Why};
use atelier::state::State;
use common::*;

struct Proj {
    repo: Repo,
    base: PathBuf,
}

impl Proj {
    fn new() -> Proj {
        Self::with_settings("")
    }

    /// A project whose committed settings end with `extra`.
    fn with_settings(extra: &str) -> Proj {
        let repo = Repo::new();
        repo.install_self(VERSION);
        repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\n\n[platforms.macos-aarch64]\nrun = \"check\"\n{extra}"
            ),
        );
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

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.repo.atelier_in(dir, args)
    }

    /// `atelier start <branch> [--on <lower>] --path <its worktree>`, from the main checkout.
    fn start(&self, branch: &str, on: Option<&str>) -> PathBuf {
        let wt = self.wt(branch);
        let mut args = vec!["start", branch];
        if let Some(l) = on {
            args.extend(["--on", l]);
        }
        let p = wt.to_str().unwrap().to_owned();
        args.extend(["--path", &p]);
        let out = self.run(self.main(), &args);
        assert_eq!(code(&out), 0, "start {branch}: {}", stderr(&out));
        wt
    }

    fn follow(&self, dir: &Path) -> Output {
        self.run(dir, &["follow"])
    }

    fn record(&self, branch: &str) -> BranchRecord {
        BranchRecord::read(&self.state(), branch).unwrap().unwrap()
    }

    fn state(&self) -> State {
        State::open_at(self.main().join(".git/atelier")).unwrap()
    }

    fn currency(&self, branch: &str) -> Currency {
        let checkout = Checkout::find(self.main()).unwrap().unwrap();
        let state = self.state();
        Stack::new(&checkout, &state, "main")
            .currency(branch)
            .unwrap()
    }

    fn with_stack<T>(&self, f: impl FnOnce(&Stack) -> T) -> T {
        let checkout = Checkout::find(self.main()).unwrap().unwrap();
        let state = self.state();
        f(&Stack::new(&checkout, &state, "main"))
    }

    /// Merges a branch into main with `--no-ff` in the main checkout and records it merged,
    /// as `merge` (3.4) will.
    fn land(&self, branch: &str) {
        git(
            self.main(),
            &["merge", "-q", "--no-ff", "-m", "land", branch],
        );
        let lifetime = self.record(branch).lifetime;
        self.with_stack(|s| s.set_state(branch, &lifetime, BranchState::Merged))
            .unwrap();
    }

    /// Removes a branch's worktree and the branch, as `clean` (3.5) will, and marks it cleaned.
    fn clean(&self, branch: &str) {
        self.drop_branch(branch);
        let lifetime = self.record(branch).lifetime;
        self.with_stack(|s| s.set_state(branch, &lifetime, BranchState::Cleaned))
            .unwrap();
    }

    /// Deletes a branch and its worktree by hand, without touching its record.
    fn drop_branch(&self, branch: &str) {
        let wt = self.wt(branch);
        git(self.main(), &["worktree", "remove", wt.to_str().unwrap()]);
        git(self.main(), &["branch", "-q", "-D", branch]);
    }
}

fn commit(dir: &Path, file: &str, contents: &str, msg: &str) -> String {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
    head(dir)
}

fn remove(dir: &Path, file: &str, msg: &str) -> String {
    git(dir, &["rm", "-q", file]);
    git(dir, &["commit", "-q", "-m", msg]);
    head(dir)
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"])
}

fn read(dir: &Path, file: &str) -> Option<String> {
    fs::read_to_string(dir.join(file)).ok()
}

fn short(dir: &Path, oid: &str) -> String {
    git(dir, &["rev-parse", "--short", oid])
}

/// Runs git with an editor that accepts the message (for `rebase --continue`).
fn git_edit(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir)
        .env("GIT_EDITOR", "true")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
}

fn rebase_open(dir: &Path) -> bool {
    let gd = git(dir, &["rev-parse", "--absolute-git-dir"]);
    Path::new(&gd).join("rebase-merge").exists() || Path::new(&gd).join("rebase-apply").exists()
}

fn behind(branch: &str, why: Why) -> Currency {
    Currency::Behind(Behind {
        branch: branch.into(),
        why,
    })
}

// ---------------------------------------------------------------------------------------------
// start

#[test]
fn start_records_the_base_and_creates_the_worktree() {
    let p = Proj::new();
    let main_tip = head(p.main());
    let wt = p.start("disk/api", None);
    assert_eq!(git(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/disk/api");
    assert_eq!(head(&wt), main_tip);
    let a = p.record("disk/api");
    assert_eq!(a.lower, "main");
    assert!(a.lower_is_integration);
    assert_eq!(a.lower_lifetime, None);
    assert_eq!(a.base, main_tip);
    assert_eq!(a.state, BranchState::Active);
    assert!(
        p.main()
            .join(".git/atelier/branches/disk%2Fapi.toml")
            .is_file()
    );

    commit(&wt, "api", "1\n", "api");
    let wt_b = p.start("disk/ui", Some("disk/api"));
    let b = p.record("disk/ui");
    assert_eq!(b.lower, "disk/api");
    assert!(!b.lower_is_integration);
    assert_eq!(b.lower_lifetime.as_deref(), Some(a.lifetime.as_str()));
    assert_eq!(b.base, head(&wt));
    assert_eq!(head(&wt_b), head(&wt));

    // The branch exists already; a lower that does not exist or was not started.
    let out = p.run(p.main(), &["start", "disk/api", "--path", "../x"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("branch disk/api exists"),
        "{}",
        stderr(&out)
    );
    let out = p.run(p.main(), &["start", "z", "--on", "nope", "--path", "../z"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    git(p.main(), &["branch", "plain"]);
    let out = p.run(p.main(), &["start", "z", "--on", "plain", "--path", "../z"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not started by atelier"),
        "{}",
        stderr(&out)
    );
    assert!(!p.base.join("z").exists());
    assert!(git(p.main(), &["branch", "--list", "z"]).is_empty());
    // Bad names are wrong usage.
    let out = p.run(p.main(), &["start", "a..b", "--path", "../y"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
}

#[test]
fn a_failed_worktree_creation_leaves_no_branch_and_no_record() {
    let p = Proj::new();
    fs::create_dir_all(p.base.join("busy")).unwrap();
    fs::write(p.base.join("busy/file"), "x").unwrap();
    let out = p.run(p.main(), &["start", "x", "--path", "../busy"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(git(p.main(), &["branch", "--list", "x"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "x").unwrap().is_none());
}

#[test]
fn start_inside_a_fresh_detached_worktree() {
    let p = Proj::new();
    let first = head(p.main());
    commit(p.main(), "m", "1\n", "main moves");
    // As T3 creates it: detached, here at an older commit than main's tip.
    let wt = p.base.join("t3-wt");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt.to_str().unwrap(),
            &first,
        ],
    );
    let out = p.run(&wt, &["start", "feat"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("start: feat on main at "),
        "{}",
        stdout(&out)
    );
    assert_eq!(git(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/feat");
    assert_eq!(head(&wt), head(p.main()));
    assert_eq!(p.record("feat").base, head(p.main()));

    // Not detached any more: refused, nothing created.
    let out = p.run(&wt, &["start", "other"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("this worktree is on feat"),
        "{}",
        stderr(&out)
    );
    // The main checkout needs --path.
    let out = p.run(p.main(), &["start", "other"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("needs --path"), "{}", stderr(&out));
    // A detached worktree with changes is not fresh.
    let wt2 = p.base.join("t3-wt2");
    git(
        p.main(),
        &["worktree", "add", "-q", "--detach", wt2.to_str().unwrap()],
    );
    fs::write(wt2.join("scratch"), "x").unwrap();
    let out = p.run(&wt2, &["start", "other"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("not fresh"), "{}", stderr(&out));
    assert!(git(p.main(), &["branch", "--list", "other"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "other").unwrap().is_none());
    // On another item, inside a detached worktree.
    fs::remove_file(wt2.join("scratch")).unwrap();
    let out = p.run(&wt2, &["start", "child", "--on", "feat"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.record("child").lower, "feat");
}

// ---------------------------------------------------------------------------------------------
// follow basics

#[test]
fn follow_moves_onto_the_moved_lower_branch() {
    let p = Proj::new();
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "b1");
    let m2 = commit(p.main(), "m", "2\n", "main moves");
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("follow: b now on {} (1 commits)\n", short(p.main(), &m2))
    );
    let r = p.record("b");
    assert_eq!(r.base, m2);
    assert_eq!(r.pending, None);
    assert_eq!(read(&wt, "m").as_deref(), Some("2\n"));
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!(
            "follow: b already on {} (1 commits)\n",
            short(p.main(), &m2)
        )
    );
}

#[test]
fn follow_keeps_record_fields_it_does_not_know() {
    let p = Proj::new();
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "b1");
    let path = p.main().join(".git/atelier/branches/b.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("reviewed_by = \"later\"\n{text}")).unwrap();
    commit(p.main(), "m", "2\n", "main moves");
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("reviewed_by = \"later\"")
    );
}

#[test]
fn follow_refuses_what_it_cannot_rebase() {
    let p = Proj::new();
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "b1");
    commit(p.main(), "m", "2\n", "main moves");
    let before = head(&wt);

    fs::write(wt.join("b"), "dirty\n").unwrap();
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("commit first"), "{}", stderr(&out));
    git(&wt, &["checkout", "--", "b"]);
    fs::write(wt.join("new-file"), "x").unwrap();
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("new-file"), "{}", stderr(&out));
    fs::remove_file(wt.join("new-file")).unwrap();

    // A merge commit of the branch's own.
    git(&wt, &["switch", "-q", "-c", "side"]);
    commit(&wt, "s", "1\n", "side");
    git(&wt, &["switch", "-q", "b"]);
    git(&wt, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("merge commit of its own"),
        "{}",
        stderr(&out)
    );
    git(&wt, &["reset", "-q", "--hard", &before]);

    // A detached HEAD; a branch atelier did not start.
    git(&wt, &["switch", "-q", "--detach"]);
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("detached"), "{}", stderr(&out));
    git(&wt, &["switch", "-q", "side"]);
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("start it with atelier start"),
        "{}",
        stderr(&out)
    );
    git(&wt, &["switch", "-q", "b"]);
    assert_eq!(head(&wt), before);
    assert_eq!(p.record("b").pending, None);
}

#[test]
fn main_merges_below_the_branch_are_not_its_own() {
    let p = Proj::new();
    // main gets a merge commit after b started; b's worker rebased onto main by hand.
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "b1");
    git(p.main(), &["switch", "-q", "-c", "topic"]);
    commit(p.main(), "t", "1\n", "topic");
    git(p.main(), &["switch", "-q", "main"]);
    git(
        p.main(),
        &["merge", "-q", "--no-ff", "-m", "topic", "topic"],
    );
    git(&wt, &["rebase", "-q", "main"]);
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.record("b").base, head(p.main()));
}

// ---------------------------------------------------------------------------------------------
// REVIEW-4 finding 1, round 2 finding 1, S4 cases 1 and 2: abort is never taken for a finish.

/// A at A2 (f = "2", a2 file), B on A2 with B1 (f = "2b"); then A reset to A1 (f = "1").
fn reset_under_dependent(p: &Proj) -> (PathBuf, PathBuf, String, String) {
    let wt_a = p.start("a", None);
    let a1 = commit(&wt_a, "f", "1\n", "A1");
    fs::write(wt_a.join("a2"), "api from A2\n").unwrap();
    let a2 = commit(&wt_a, "f", "2\n", "A2: api and f");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "f", "2b\n", "B1");
    git(&wt_a, &["reset", "-q", "--hard", &a1]);
    (wt_a, wt_b, a1, a2)
}

#[test]
fn an_aborted_follow_is_not_recorded_as_finished() {
    let p = Proj::new();
    let (_wt_a, wt_b, a1, a2) = reset_under_dependent(&p);
    assert_eq!(
        p.currency("b"),
        behind("b", Why::LowerRewritten { lower: "a".into() })
    );

    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("conflict rebasing b onto"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("\n  f\n"), "{}", stderr(&out));
    let r = p.record("b");
    let pending = r.pending.clone().unwrap();
    assert_eq!(
        (pending.target.as_str(), pending.old_base.as_str()),
        (a1.as_str(), a2.as_str())
    );
    // While the rebase is open: refused, and b is not current.
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("finish the rebase"),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.currency("b"), behind("b", Why::RebaseOpen));

    // REVIEW-4 finding 1: abort, then follow again. It must not settle as finished (that
    // returned 0 and kept A2's file).
    git(&wt_b, &["rebase", "--abort"]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 5, "abort taken for a finish: {}", stdout(&out));
    git(&wt_b, &["rebase", "--abort"]);

    // Round 2 finding 1 (S4 case 1): abort, amend B's own commit, follow again.
    fs::write(wt_b.join("f"), "2b amended\n").unwrap();
    git(
        &wt_b,
        &["commit", "-q", "-a", "--amend", "-m", "B1 amended"],
    );
    let out = p.follow(&wt_b);
    assert_eq!(
        code(&out),
        5,
        "abort-then-amend taken for a finish: {}",
        stdout(&out)
    );
    assert_eq!(p.record("b").pending.unwrap().old_base, a2);

    // S4 case 2: resolve and continue: finished.
    fs::write(wt_b.join("f"), "2b amended\n").unwrap();
    git(&wt_b, &["add", "f"]);
    git_edit(&wt_b, &["rebase", "--continue"]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let r = p.record("b");
    assert_eq!(r.base, a1);
    assert_eq!(r.pending, None);
    assert_eq!(read(&wt_b, "a2"), None, "A2's file was dropped with A2");
    assert_eq!(read(&wt_b, "f").as_deref(), Some("2b amended\n"));
    assert_eq!(p.currency("b"), Currency::Current);
}

#[test]
fn s4_case_3_a_lower_that_moved_forward() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "f", "2\n", "A2");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "f", "2b\n", "B1");
    let a3 = commit(&wt_a, "f", "3\n", "A3");
    // Moving forward keeps b current.
    assert_eq!(p.currency("b"), Currency::Current);

    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    git(&wt_b, &["rebase", "--abort"]);
    let out = p.follow(&wt_b);
    assert_eq!(
        code(&out),
        5,
        "aborted taken for finished: {}",
        stdout(&out)
    );
    fs::write(wt_b.join("f"), "3b\n").unwrap();
    git(&wt_b, &["add", "f"]);
    git_edit(&wt_b, &["rebase", "--continue"]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.record("b").base, a3);
}

#[test]
fn an_unsettleable_pending_follow_is_refused_naming_both_commits() {
    let p = Proj::new();
    let (_wt_a, wt_b, a1, a2) = reset_under_dependent(&p);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    git(&wt_b, &["rebase", "--abort"]);
    // The worker resets below the base: neither commit is in HEAD.
    let below = git(p.main(), &["rev-parse", "main"]);
    git(&wt_b, &["reset", "-q", "--hard", &below]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&a1) && stderr(&out).contains(&a2),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------------------------
// REVIEW-4 finding 2: the base must stay in the branch.

#[test]
fn a_hand_rebase_onto_the_lower_tip_repairs_the_base() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let a1 = commit(&wt_a, "a1", "1\n", "A1");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    // A rewritten (A1 amended to A2); the worker rebases B onto it by hand.
    fs::write(wt_a.join("a1"), "1 amended\n").unwrap();
    git(&wt_a, &["commit", "-q", "-a", "--amend", "-m", "A2"]);
    let a2 = head(&wt_a);
    git(&wt_b, &["rebase", "-q", "--onto", &a2, &a1]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("its base is now the a tip"),
        "{}",
        stdout(&out)
    );
    assert_eq!(p.record("b").base, a2);

    // A3 deletes A2's file; following keeps the deletion (it is not replayed as B's work).
    remove(&wt_a, "a1", "A3 deletes a1");
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(read(&wt_b, "a1"), None);
    assert_eq!(read(&wt_b, "b").as_deref(), Some("1\n"));
}

#[test]
fn a_base_lost_below_a_newer_lower_tip_is_refused() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let a1 = commit(&wt_a, "a1", "1\n", "A1");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    fs::write(wt_a.join("a1"), "1 amended\n").unwrap();
    git(&wt_a, &["commit", "-q", "-a", "--amend", "-m", "A2"]);
    let a2 = head(&wt_a);
    git(&wt_b, &["rebase", "-q", "--onto", &a2, &a1]);
    // A3 exists before b follows: the lower tip is not in b, nothing can be repaired.
    remove(&wt_a, "a1", "A3 deletes a1");
    let before = head(&wt_b);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("your branch no longer contains its base {a1}")),
        "{}",
        stderr(&out)
    );
    assert_eq!(head(&wt_b), before);
    assert_eq!(p.record("b").base, a1);
    assert_eq!(
        p.currency("b"),
        behind("b", Why::BaseLost { base: a1.clone() })
    );
}

// ---------------------------------------------------------------------------------------------
// REVIEW-4 finding 3: a lower branch gone without landing.

#[test]
fn a_lower_branch_gone_unmerged_is_refused() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "api", "api\n", "A: api");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "caller", "calls api\n", "B: caller");
    commit(p.main(), "m", "2\n", "main moves");
    let before = head(&wt_b);

    // Renamed: the recorded name is gone.
    git(&wt_a, &["branch", "-m", "a", "a-renamed"]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a is gone and was never merged"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        p.currency("b"),
        behind("b", Why::LowerGone { lower: "a".into() })
    );
    git(&wt_a, &["branch", "-m", "a-renamed", "a"]);

    // Deleted.
    p.drop_branch("a");
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a is gone and was never merged"),
        "{}",
        stderr(&out)
    );
    assert_eq!(head(&wt_b), before);
    assert_eq!(read(&wt_b, "api").as_deref(), Some("api\n"));
    assert_eq!(p.record("b").pending, None);
}

#[test]
fn a_lower_branch_that_landed_moves_its_dependent_onto_main() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "api", "api\n", "A: api");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "caller", "calls api\n", "B: caller");
    p.land("a");
    assert_eq!(p.currency("b"), Currency::Current);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("a is merged; b is now built on main"),
        "{}",
        stdout(&out)
    );
    assert_eq!(read(&wt_b, "api").as_deref(), Some("api\n"));
    assert_eq!(read(&wt_b, "caller").as_deref(), Some("calls api\n"));
    let r = p.record("b");
    assert_eq!(
        (r.lower.as_str(), r.lower_is_integration, r.lower_lifetime),
        ("main", true, None)
    );
    assert_eq!(r.base, head(p.main()));
}

#[test]
fn a_lower_gone_whose_base_main_contains_moves_onto_main() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "api", "api\n", "A: api");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "caller", "calls api\n", "B: caller");
    // Landed by hand, without atelier, then deleted.
    git(p.main(), &["merge", "-q", "--no-ff", "-m", "by hand", "a"]);
    p.drop_branch("a");
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("a is gone, and main contains its base"),
        "{}",
        stdout(&out)
    );
    assert!(p.record("b").lower_is_integration);
}

// ---------------------------------------------------------------------------------------------
// Merged then cleaned; REVIEW-4 finding 5: name reuse and cycles.

#[test]
fn a_lower_merged_then_cleaned_keeps_its_record_until_no_dependent_names_it() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "api", "api\n", "A: api");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "caller", "calls api\n", "B: caller");
    let a_life = p.record("a").lifetime;
    p.land("a");
    p.clean("a");
    p.with_stack(|s| {
        assert_eq!(s.dependents("a", &a_life).unwrap(), vec!["b".to_owned()]);
        assert!(!s.drop_if_unneeded("a", &a_life).unwrap());
    });
    assert_eq!(p.currency("b"), Currency::Current);

    // REVIEW-4 finding 5: the name cannot be started again while b names the old a, on b
    // (a cycle) or anywhere else.
    for on in [Some("b"), None] {
        let mut args = vec!["start", "a"];
        if let Some(l) = on {
            args.extend(["--on", l]);
        }
        args.extend(["--path", "../wt-a2"]);
        let out = p.run(p.main(), &args);
        assert_eq!(code(&out), 4, "{}", stderr(&out));
        assert!(
            stderr(&out).contains("still the lower branch of b"),
            "{}",
            stderr(&out)
        );
        assert_eq!(p.record("a").lifetime, a_life);
        assert_eq!(p.record("a").state, BranchState::Cleaned);
    }

    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(p.record("b").lower_is_integration);
    p.with_stack(|s| {
        assert!(s.dependents("a", &a_life).unwrap().is_empty());
        assert!(s.drop_if_unneeded("a", &a_life).unwrap());
    });
    assert!(BranchRecord::read(&p.state(), "a").unwrap().is_none());

    // Now the name is free, even on b.
    let wt_a2 = p.start("a", Some("b"));
    let a2 = p.record("a");
    assert_ne!(a2.lifetime, a_life);
    assert_eq!(a2.lower, "b");
    assert_eq!(head(&wt_a2), head(&wt_b));
}

#[test]
fn a_name_with_a_kept_record_and_no_dependent_can_be_reused() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "x", "1\n", "x");
    let old = p.record("a").lifetime;
    p.land("a");
    p.clean("a");
    p.start("a", None);
    assert_ne!(p.record("a").lifetime, old);
    assert_eq!(p.record("a").state, BranchState::Active);
}

#[test]
fn cycles_are_refused() {
    let p = Proj::new();
    let out = p.run(p.main(), &["start", "x", "--on", "x", "--path", "../x"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));

    // Records that name each other (written by hand): refused by the guard and by start.
    let wt_p = p.start("p", None);
    let wt_q = p.start("q", Some("p"));
    let _ = (&wt_p, &wt_q);
    let q_life = p.record("q").lifetime;
    let mut rp = p.record("p");
    rp.lower = "q".into();
    rp.lower_is_integration = false;
    rp.lower_lifetime = Some(q_life);
    rp.write(&p.state()).unwrap();
    p.with_stack(|s| {
        let e = s.currency("q").unwrap_err();
        assert!(e.message.contains("cycle"), "{}", e.message);
        let e = s.chain("q").unwrap_err();
        assert!(e.message.contains("cycle"), "{}", e.message);
    });
    let out = p.run(p.main(), &["start", "r", "--on", "p", "--path", "../r"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("cycle"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------------------------
// The transitive guard: A → B → C, A rewritten.

#[test]
fn rewriting_the_bottom_of_a_stack_makes_every_branch_above_not_current() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "a", "1\n", "A1");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    let wt_c = p.start("c", Some("b"));
    commit(&wt_c, "c", "1\n", "C1");
    for x in ["a", "b", "c"] {
        assert_eq!(p.currency(x), Currency::Current, "{x}");
    }
    let names: Vec<String> = p.with_stack(|s| {
        s.chain("c")
            .unwrap()
            .into_iter()
            .map(|l| l.record.name)
            .collect()
    });
    assert_eq!(names, ["c", "b", "a"]);

    fs::write(wt_a.join("a"), "1 amended\n").unwrap();
    git(&wt_a, &["commit", "-q", "-a", "--amend", "-m", "A1'"]);
    assert_eq!(p.currency("a"), Currency::Current);
    let b_behind = behind("b", Why::LowerRewritten { lower: "a".into() });
    assert_eq!(p.currency("b"), b_behind);
    assert_eq!(
        p.currency("c"),
        b_behind,
        "c names the first branch that must follow"
    );
    let Currency::Behind(b) = p.currency("c") else {
        unreachable!()
    };
    assert_eq!(
        b.message(),
        "b is behind a rewrite of a: run atelier follow in its worktree"
    );
    let e = p.with_stack(|s| s.require_current("c")).unwrap_err();
    assert_eq!(e.exit, atelier::exit::Exit::Refused);

    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.currency("b"), Currency::Current);
    // b's follow rewrote b: now c is the one behind.
    assert_eq!(
        p.currency("c"),
        behind("c", Why::LowerRewritten { lower: "b".into() })
    );
    let out = p.follow(&wt_c);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.currency("c"), Currency::Current);
    assert_eq!(read(&wt_c, "a").as_deref(), Some("1 amended\n"));
    // The review diff of each upper branch covers its own commits only.
    let rc = p.record("c");
    let own = git(&wt_c, &["diff", "--name-only", &format!("{}..c", rc.base)]);
    assert_eq!(own, "c");
}

#[test]
fn an_upper_branch_names_its_unmerged_lower_branches() {
    // REVIEW-4 finding 4 is merge's rule (3.4); the chain gives merge what it needs: each
    // unmerged lower branch and the base the branch above it needs.
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "a", "1\n", "A1");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    let chain = p.with_stack(|s| s.chain("b").unwrap());
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].record.name, "a");
    assert_eq!(chain[0].record.base, chain[1].tip);
    p.land("a");
    let chain = p.with_stack(|s| s.chain("b").unwrap());
    assert_eq!(chain.len(), 1, "a merged lower branch ends the chain");
}

// ---------------------------------------------------------------------------------------------
// Mergiraf (DESIGN §8.2).

/// A stand-in for mergiraf on `PATH`: `--version` works; `solve` drops the marker lines and
/// keeps both sides, except in a file containing UNSOLVABLE, which it garbles and fails.
fn fake_mergiraf(dir: &Path) -> String {
    let bin = dir.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("mergiraf");
    fs::write(
        &script,
        r#"#!/bin/sh
[ "$1" = "--version" ] && { echo "mergiraf 0.0.0-fake"; exit 0; }
[ "$1" = "solve" ] || exit 2
for f; do :; done
echo "$f" >> "$(dirname "$0")/calls"
if grep -q UNSOLVABLE "$f"; then echo partial > "$f"; exit 1; fi
grep -v -e '^<<<<<<<' -e '^|||||||' -e '^=======$' -e '^>>>>>>>' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

fn conflicting_pair(p: &Proj) -> PathBuf {
    let wt = p.start("b", None);
    commit(&wt, "src/x.rs", "fn x() { 2 }\n", "b: x");
    commit(&wt, "notes.md", "b notes\n", "b: notes");
    commit(&wt, "y.rs", "UNSOLVABLE b\n", "b: y");
    commit(p.main(), "src/x.rs", "fn x() { 1 }\n", "main: x");
    commit(p.main(), "notes.md", "main notes\n", "main: notes");
    commit(p.main(), "y.rs", "UNSOLVABLE main\n", "main: y");
    wt
}

#[test]
fn mergiraf_resolves_only_code_files_and_leaves_them_staged() {
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let path = fake_mergiraf(&p.base);
    let wt = conflicting_pair(&p);
    // b's three commits each conflict; the first stop is on x.rs (b: x).
    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(stdout(&out), "resolved by mergiraf: src/x.rs\n");
    assert!(
        stderr(&out).contains("every conflicted file was resolved by mergiraf"),
        "{}",
        stderr(&out)
    );
    assert!(rebase_open(&wt));
    assert!(git(&wt, &["diff", "--name-only", "--diff-filter=U"]).is_empty());
    assert_eq!(git(&wt, &["diff", "--cached", "--name-only"]), "src/x.rs");
    let x = read(&wt, "src/x.rs").unwrap();
    assert!(
        x.contains("fn x() { 1 }") && x.contains("fn x() { 2 }") && !x.contains("<<<<<<<"),
        "{x}"
    );
    // The next commit conflicts too: `--continue` stops again.
    let cont = git_cmd(&wt)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .output()
        .unwrap();
    assert!(!cont.status.success());

    // The rebase is still open on notes.md: never given to mergiraf.
    assert!(rebase_open(&wt));
    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("finish the rebase"),
        "{}",
        stderr(&out)
    );
    let calls = fs::read_to_string(p.base.join("fake-bin/calls")).unwrap();
    assert_eq!(calls, "src/x.rs\n");
}

#[test]
fn a_file_mergiraf_cannot_resolve_stays_as_gits_conflict() {
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let path = fake_mergiraf(&p.base);
    let wt = p.start("b", None);
    commit(&wt, "y.rs", "UNSOLVABLE b\n", "b: y");
    commit(&wt, "notes.md", "b notes\n", "b: notes");
    let both = |dir: &Path| {
        let p = dir.join("y.rs");
        fs::write(&p, "UNSOLVABLE main\n").unwrap();
        fs::write(dir.join("notes.md"), "main notes\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "main: y and notes"]);
    };
    both(p.main());
    // Squash b's two commits so both files conflict in one stop.
    git(&wt, &["reset", "-q", "--soft", "HEAD~2"]);
    git(&wt, &["commit", "-q", "-m", "b: y and notes"]);
    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(
        stderr(&out).contains("\n  notes.md\n") && stderr(&out).contains("\n  y.rs\n"),
        "{}",
        stderr(&out)
    );
    let y = read(&wt, "y.rs").unwrap();
    assert!(y.contains("<<<<<<<") && !y.contains("partial"), "{y}");
    let calls = fs::read_to_string(p.base.join("fake-bin/calls")).unwrap();
    assert_eq!(calls, "y.rs\n");
}

#[test]
fn mergiraf_is_off_by_default_and_required_when_on() {
    let p = Proj::new();
    let path = fake_mergiraf(&p.base);
    let wt = conflicting_pair(&p);
    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert!(stderr(&out).contains("\n  src/x.rs\n"), "{}", stderr(&out));
    assert!(!p.base.join("fake-bin/calls").exists());

    // On, but mergiraf is not installed: refused before anything moves.
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let wt = conflicting_pair(&p);
    let before = head(&wt);
    let out = p
        .repo
        .atelier_env(&wt, &["follow"], &[("PATH", "/usr/bin:/bin")]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("mergiraf"), "{}", stderr(&out));
    assert_eq!(head(&wt), before);
    assert!(!rebase_open(&wt));
    assert_eq!(p.record("b").pending, None);
}
