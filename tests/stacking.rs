//! `start`, `follow`, the branch record lifecycle and the dependent guard, on
//! throwaway repositories. Each probe found in review (abort then follow, abort then amend,
//! the base leaving the branch, name reuse and cycles) is replayed against the real binary.

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
        format!("follow: b now on {} (1 commit)\n", short(p.main(), &m2))
    );
    let r = p.record("b");
    assert_eq!(r.base, m2);
    assert_eq!(r.pending, None);
    assert_eq!(read(&wt, "m").as_deref(), Some("2\n"));
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("follow: b already on {} (1 commit)\n", short(p.main(), &m2))
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
// abort is never taken for a finish.

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

    // abort, then follow again. It must not settle as finished (that
    // returned 0 and kept A2's file).
    git(&wt_b, &["rebase", "--abort"]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 5, "abort taken for a finish: {}", stdout(&out));
    git(&wt_b, &["rebase", "--abort"]);

    // abort, amend B's own commit, follow again.
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

    // resolve and continue: finished.
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
// the base must stay in the branch.

/// The exact refusal for a base the branch no longer contains.
fn base_lost(base: &str) -> String {
    format!(
        "your branch no longer contains its base {base}: restore it (git reflog, then git reset --hard <the commit before your rebase>) and use atelier follow"
    )
}

#[test]
fn a_hand_rebase_onto_the_lower_tip_is_refused_until_restored() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let a1 = commit(&wt_a, "a1", "1\n", "A1");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    let b2 = commit(&wt_b, "b2", "1\n", "B2");
    // A rewritten (A1 amended to A2); the worker rebases B onto it by hand.
    fs::write(wt_a.join("a1"), "1 amended\n").unwrap();
    git(&wt_a, &["commit", "-q", "-a", "--amend", "-m", "A2"]);
    let a2 = head(&wt_a);
    git(&wt_b, &["rebase", "-q", "--onto", &a2, &a1]);
    let rebased = head(&wt_b);
    // No automatic repair: the base is not in the branch, so every reader refuses.
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(stderr(&out), format!("atelier: {}\n", base_lost(&a1)));
    assert_eq!(head(&wt_b), rebased);
    assert_eq!(p.record("b").base, a1);
    assert_eq!(
        p.currency("b"),
        behind("b", Why::BaseLost { base: a1.clone() })
    );
    // Restored as the message says, follow moves exactly B's two commits.
    git(&wt_b, &["reset", "-q", "--hard", &b2]);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("follow: b now on {} (2 commits)\n", short(p.main(), &a2))
    );
    assert_eq!(p.record("b").base, a2);
    assert_eq!(read(&wt_b, "a1").as_deref(), Some("1 amended\n"));
}

/// A wrong-range hand rebase replays a commit the lower branch dropped. The lower
/// tip is in the branch, but that does not make the range the worker's own: refused.
#[test]
fn a_wrong_hand_rebase_that_replays_dropped_lower_commits_is_refused() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let a1 = commit(&wt_a, "a1", "1\n", "A1");
    let a2 = commit(&wt_a, "removed", "1\n", "A2 adds removed");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "b", "1\n", "B1");
    // A drops A2 and moves on with A3.
    git(&wt_a, &["reset", "-q", "--hard", &a1]);
    commit(&wt_a, "a3", "1\n", "A3");
    // The worker rebases from A1 instead of from the recorded base A2: A2 is replayed.
    git(&wt_b, &["rebase", "-q", "--onto", "a", &a1]);
    assert_eq!(read(&wt_b, "removed").as_deref(), Some("1\n"));
    let before = head(&wt_b);
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert_eq!(stderr(&out), format!("atelier: {}\n", base_lost(&a2)));
    assert_eq!(head(&wt_b), before);
    assert_eq!(p.record("b").base, a2);
    assert_eq!(p.currency("b"), behind("b", Why::BaseLost { base: a2 }));
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
    assert!(stderr(&out).contains(&base_lost(&a1)), "{}", stderr(&out));
    assert_eq!(head(&wt_b), before);
    assert_eq!(p.record("b").base, a1);
    assert_eq!(
        p.currency("b"),
        behind("b", Why::BaseLost { base: a1.clone() })
    );
}

// ---------------------------------------------------------------------------------------------
// a lower branch gone without landing.

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
// Merged then cleaned: name reuse and cycles.

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

    // the name cannot be started again while b names the old a, on b
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
    // The base-approval rule is merge's; the chain gives merge what it needs: each
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
// Mergiraf.

/// A stand-in for mergiraf 0.20 on `PATH`: `--version` works; `solve --stdout` prints the file
/// without its marker lines (both sides kept) and exits 0, except for a file containing
/// UNSOLVABLE, for which it prints garbage and exits 1. Without `--stdout` it fails (atelier
/// never lets mergiraf write). It reads through a symbolic link, as mergiraf does. With
/// `SWAP_FILE` set it first replaces the file by a link to that path; with `SWAP_FOLDER` set it
/// first moves the file's folder aside and puts a link to that path in its place (a process
/// racing in the worktree).
fn fake_mergiraf(dir: &Path) -> String {
    let bin = dir.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("mergiraf");
    fs::write(
        &script,
        r#"#!/bin/sh
[ "$1" = "--version" ] && { echo "mergiraf 0.0.0-fake"; exit 0; }
[ "$1" = "solve" ] && [ "$2" = "--stdout" ] || exit 3
for f; do :; done
echo "$f" >> "$(dirname "$0")/calls"
if [ -n "$SWAP_FILE" ]; then rm "$f"; ln -s "$SWAP_FILE" "$f"; fi
if [ -n "$SWAP_FOLDER" ]; then
  d=$(dirname "$f"); mv "$d" "$d-moved"; ln -s "$SWAP_FOLDER" "$d"; f="$d-moved/$(basename "$f")"
fi
if grep -q UNSOLVABLE "$f"; then echo partial; exit 1; fi
grep -v -e '^<<<<<<<' -e '^|||||||' -e '^=======$' -e '^>>>>>>>' "$f"
exit 0
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

/// Three files changed on both sides from a common base (ordinary three-way conflicts).
fn conflicting_pair(p: &Proj) -> PathBuf {
    commit(p.main(), "src/x.rs", "fn x() { 0 }\n", "base: x");
    commit(p.main(), "notes.md", "base notes\n", "base: notes");
    commit(p.main(), "y.rs", "UNSOLVABLE base\n", "base: y");
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
    commit(p.main(), "y.rs", "UNSOLVABLE base\n", "base: y");
    commit(p.main(), "notes.md", "base notes\n", "base: notes");
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

// ---------------------------------------------------------------------------------------------
// Probes found in review.

/// Review 1: a conflicted symbolic link is never given to mergiraf, and the file it points to
/// is never read or written; the link conflict stays git's.
#[test]
fn mergiraf_never_follows_a_conflicted_symbolic_link() {
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let path = fake_mergiraf(&p.base);
    let markers = "fn a() {}\n<<<<<<< left\nfn b() { 1 }\n=======\nfn b() { 2 }\n>>>>>>> right\n";
    commit(p.main(), "one.md", "one\n", "one");
    commit(p.main(), "two.md", "two\n", "two");
    commit(p.main(), "three.md", markers, "three");
    std::os::unix::fs::symlink("one.md", p.main().join("x.rs")).unwrap();
    git(p.main(), &["add", "x.rs"]);
    git(p.main(), &["commit", "-q", "-m", "x.rs -> one.md"]);
    let wt = p.start("b", None);
    fs::remove_file(wt.join("x.rs")).unwrap();
    std::os::unix::fs::symlink("two.md", wt.join("x.rs")).unwrap();
    git(&wt, &["add", "x.rs"]);
    git(&wt, &["commit", "-q", "-m", "b: x.rs -> two.md"]);
    fs::remove_file(p.main().join("x.rs")).unwrap();
    std::os::unix::fs::symlink("three.md", p.main().join("x.rs")).unwrap();
    git(p.main(), &["add", "x.rs"]);
    git(p.main(), &["commit", "-q", "-m", "main: x.rs -> three.md"]);

    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("\n  x.rs\n"), "{}", stderr(&out));
    assert!(!p.base.join("fake-bin/calls").exists(), "mergiraf ran");
    assert_eq!(read(&wt, "three.md").as_deref(), Some(markers));
    assert_eq!(
        git(&wt, &["diff", "--name-only", "--diff-filter=U"]),
        "x.rs"
    );
}

/// Review 4: a modify/delete conflict and an add/add without a base stay git's conflicts,
/// unmerged, even on code files the resolver would take.
#[test]
fn mergiraf_leaves_missing_side_conflicts_to_git() {
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let path = fake_mergiraf(&p.base);
    commit(p.main(), "x.rs", "fn x() -> i32 { 0 }\n", "base: x");
    let wt = p.start("b", None);
    fs::write(wt.join("x.rs"), "fn x() -> i32 { 2 }\n").unwrap();
    fs::write(wt.join("z.rs"), "fn z() -> i32 { 2 }\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "b: change x, add z"]);
    git(p.main(), &["rm", "-q", "x.rs"]);
    fs::write(p.main().join("z.rs"), "fn z() -> i32 { 1 }\n").unwrap();
    git(p.main(), &["add", "-A"]);
    git(p.main(), &["commit", "-q", "-m", "main: delete x, add z"]);

    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(
        stderr(&out).contains("\n  x.rs\n") && stderr(&out).contains("\n  z.rs\n"),
        "{}",
        stderr(&out)
    );
    assert!(!p.base.join("fake-bin/calls").exists(), "mergiraf ran");
    assert_eq!(
        git(&wt, &["diff", "--name-only", "--diff-filter=U"]),
        "x.rs\nz.rs"
    );
}

/// Review 2: a branch whose ref is an alias of another branch is refused, and the other
/// branch is left alone.
#[test]
fn follow_refuses_a_branch_that_is_an_alias_of_another() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let wt_b = p.start("b", None);
    git(p.main(), &["symbolic-ref", "refs/heads/b", "refs/heads/a"]);
    commit(p.main(), "m", "1\n", "main moves");
    let a_before = (head(&wt_a), p.record("a"));
    let b_before = p.record("b");
    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refs/heads/b is a symbolic ref"),
        "{}",
        stderr(&out)
    );
    assert_eq!((head(&wt_a), p.record("a")), a_before);
    assert_eq!(p.record("b"), b_before);
}

/// Review 3: a detached worktree with commits of its own is not fresh; `start` there would
/// drop them from its history.
#[test]
fn start_refuses_a_detached_worktree_with_its_own_commits() {
    let p = Proj::new();
    let wt = p.base.join("t3-own");
    git(
        p.main(),
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    let own = commit(&wt, "own", "1\n", "own work");
    let out = p.run(&wt, &["start", "b"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "it has commits no branch holds ({})",
            short(&wt, &own)
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(head(&wt), own);
    assert!(git(p.main(), &["branch", "--list", "b"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "b").unwrap().is_none());
}

/// Review 5: a hand rebase open in a lower branch's worktree, with no pending follow, makes
/// every branch above it not current, ends `chain`, and refuses `start --on` and `follow`.
#[test]
fn an_open_hand_rebase_of_a_lower_branch_is_seen_by_every_reader() {
    let p = Proj::new();
    commit(p.main(), "f", "root\n", "f");
    let wt_a = p.start("a", None);
    commit(&wt_a, "f", "a\n", "A: f");
    let wt_b = p.start("b", Some("a"));
    commit(&wt_b, "own", "1\n", "B: own");
    commit(p.main(), "f", "main\n", "main: f");
    let out = git_cmd(&wt_a).args(["rebase", "main"]).output().unwrap();
    assert!(!out.status.success());
    assert!(rebase_open(&wt_a));
    assert_eq!(p.record("a").pending, None);

    assert_eq!(p.currency("a"), behind("a", Why::RebaseOpen));
    assert_eq!(p.currency("b"), behind("a", Why::RebaseOpen));
    let e = p.with_stack(|s| s.chain("b")).unwrap_err();
    assert!(e.message.contains("a has a rebase open"), "{}", e.message);

    let wt_c = p.wt("c");
    let out = p.run(
        p.main(),
        &["start", "c", "--on", "a", "--path", wt_c.to_str().unwrap()],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("finish the rebase"),
        "{}",
        stderr(&out)
    );
    assert!(git(p.main(), &["branch", "--list", "c"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "c").unwrap().is_none());

    let out = p.follow(&wt_b);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a has a rebase open"),
        "{}",
        stderr(&out)
    );

    // Its own rebase open in another worktree counts too, with no pending follow.
    git(&wt_a, &["rebase", "--abort"]);
    assert_eq!(p.currency("b"), Currency::Current);
    let wt_b2 = p.base.join("b-second");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt_b2.to_str().unwrap(),
            "b",
        ],
    );
    git(&wt_b, &["switch", "-q", "--detach"]);
    git(&wt_b2, &["switch", "-q", "b"]);
    let out = git_cmd(&wt_b2)
        .args(["rebase", "-x", "false", "a~1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(rebase_open(&wt_b2));
    assert_eq!(p.currency("b"), behind("b", Why::RebaseOpen));
}

/// Writes an executable hook into the clone's hooks folder (the tests' own git commands do not
/// run hooks).
fn hook(p: &Proj, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let dir = p.main().join(".git/hooks");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn unhook(p: &Proj, name: &str) {
    fs::remove_file(p.main().join(".git/hooks").join(name)).unwrap();
}

fn worktree_listed(p: &Proj, wt: &Path) -> bool {
    git(p.main(), &["worktree", "list", "--porcelain"])
        .lines()
        .any(|l| l == format!("worktree {}", wt.display()))
}

/// Review 6: a checkout that fails after git registered the worktree on the branch keeps the
/// branch and its unfinished record; `start` again finishes it, or undoes it once the worktree
/// is removed.
#[test]
fn a_failed_checkout_on_the_branch_keeps_it_and_start_again_finishes_or_undoes() {
    let p = Proj::new();
    let base = head(p.main());
    hook(&p, "post-checkout", "exit 1");
    let wt = p.wt("b");
    let path = wt.to_str().unwrap();
    let out = p.run(p.main(), &["start", "b", "--path", path]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("b is checked out in {path}")),
        "{}",
        stderr(&out)
    );
    assert!(worktree_listed(&p, &wt));
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/b"]), base);
    let rec = p.record("b");
    assert_eq!(rec.starting.as_ref().unwrap().worktree, path);
    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("the start of b did not finish"),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.currency("b"), behind("b", Why::StartUnfinished));
    unhook(&p, "post-checkout");
    let out = p.run(p.main(), &["start", "b", "--path", path]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("finished the interrupted start of b"),
        "{}",
        stdout(&out)
    );
    let done = p.record("b");
    assert_eq!(done.starting, None);
    assert_eq!(done.lifetime, rec.lifetime);
    assert_eq!(code(&p.follow(&wt)), 0);

    // Undone instead: the worktree removed by hand, then start again.
    hook(&p, "post-checkout", "exit 1");
    let wt_c = p.wt("c");
    let path_c = wt_c.to_str().unwrap();
    assert_eq!(code(&p.run(p.main(), &["start", "c", "--path", path_c])), 4);
    unhook(&p, "post-checkout");
    git(p.main(), &["worktree", "remove", "--force", path_c]);
    let out = p.run(p.main(), &["start", "c", "--path", path_c]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("undid an earlier start of c")
            && stdout(&out).contains("start: c on main"),
        "{}",
        stdout(&out)
    );
    assert_eq!(p.record("c").starting, None);
}

/// Review 6, second variant: the failing hook commits first. The branch moved, so it is kept
/// with its record, and start again finishes it with the commit in it.
#[test]
fn a_failed_checkout_that_moved_the_branch_keeps_branch_and_record() {
    let p = Proj::new();
    let base = head(p.main());
    hook(
        &p,
        "post-checkout",
        "echo own > own && git add own && git commit -q -m own && exit 1",
    );
    let wt = p.wt("b");
    let path = wt.to_str().unwrap();
    let out = p.run(p.main(), &["start", "b", "--path", path]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    unhook(&p, "post-checkout");
    let tip = git(p.main(), &["rev-parse", "refs/heads/b"]);
    assert_ne!(tip, base);
    assert!(p.record("b").starting.is_some());
    let out = p.run(p.main(), &["start", "b", "--path", path]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(p.record("b").base, base);
    assert_eq!(p.record("b").starting, None);
    commit(p.main(), "m", "1\n", "main moves");
    let out = p.follow(&wt);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(read(&wt, "own").as_deref(), Some("own\n"));
}

/// A `git` on `PATH` that runs the real one and kills its caller (atelier) with SIGKILL just
/// before (`KILL_BEFORE`) or just after (`KILL_AFTER`) a call whose arguments contain the
/// variable's text.
fn killing_git(p: &Proj) -> String {
    use std::os::unix::fs::PermissionsExt;
    let real = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
    let bin = p.base.join("killing-bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("git");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
if [ -n "$KILL_BEFORE" ]; then case "$*" in *"$KILL_BEFORE"*) kill -9 $PPID; exit 1;; esac; fi
"{real}" "$@"; rc=$?
if [ -n "$KILL_AFTER" ]; then case "$*" in *"$KILL_AFTER"*) kill -9 $PPID; sleep 1;; esac; fi
exit $rc
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

fn killed(out: &Output) -> bool {
    use std::os::unix::process::ExitStatusExt;
    out.status.signal() == Some(9)
}

/// Review 7: atelier killed between the steps of `start` leaves an unfinished record that the
/// same `start` finishes or undoes.
#[test]
fn start_killed_between_its_steps_is_finished_or_undone_by_start_again() {
    let p = Proj::new();
    let base = head(p.main());
    let path = killing_git(&p);
    const CREATE_REF: &str = "update-ref --no-deref -m atelier start";

    // Killed before the ref exists: the record alone is undone.
    let wt = p.wt("b");
    let args = ["start", "b", "--path", wt.to_str().unwrap()];
    let out = p.repo.atelier_env(
        p.main(),
        &args,
        &[("PATH", &path), ("KILL_BEFORE", CREATE_REF)],
    );
    assert!(killed(&out), "{}", stderr(&out));
    assert!(p.record("b").starting.is_some());
    assert!(git(p.main(), &["branch", "--list", "b"]).is_empty());
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("undid an earlier start of b"),
        "{}",
        stdout(&out)
    );
    assert_eq!(p.record("b").starting, None);

    // Killed just after the ref was created (the review's probe): no worktree has it, it
    // still holds the base, so start again deletes it and starts afresh.
    let wt = p.wt("c");
    let args = ["start", "c", "--path", wt.to_str().unwrap()];
    let out = p.repo.atelier_env(
        p.main(),
        &args,
        &[("PATH", &path), ("KILL_AFTER", CREATE_REF)],
    );
    assert!(killed(&out), "{}", stderr(&out));
    let first = p.record("c");
    assert!(first.starting.is_some());
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/c"]), base);
    assert_eq!(p.currency("c"), behind("c", Why::StartUnfinished));
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("undid an earlier start of c")
            && stdout(&out).contains("start: c on main"),
        "{}",
        stdout(&out)
    );
    let second = p.record("c");
    assert_eq!(second.starting, None);
    assert_ne!(second.lifetime, first.lifetime);
    assert_eq!(git(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/c");

    // Killed after the worktree was created: start again finishes it.
    let wt = p.wt("d");
    let args = ["start", "d", "--path", wt.to_str().unwrap()];
    let out = p.repo.atelier_env(
        p.main(),
        &args,
        &[("PATH", &path), ("KILL_AFTER", "worktree add")],
    );
    assert!(killed(&out), "{}", stderr(&out));
    let first = p.record("d");
    assert!(first.starting.is_some());
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("finished the interrupted start of d"),
        "{}",
        stdout(&out)
    );
    assert_eq!(p.record("d").lifetime, first.lifetime);
    assert_eq!(p.record("d").starting, None);
    assert_eq!(code(&p.follow(&wt)), 0);
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready() {
        assert!(
            std::time::Instant::now() < until,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Review 8: atelier killed while follow's rebase runs. The rebase's process group is recorded
/// under the branch lock, so the next follow refuses while git lives, and settles once it ended.
#[test]
fn a_killed_follow_keeps_its_rebase_recorded_until_git_ends() {
    let p = Proj::new();
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "B1");
    let m = commit(p.main(), "m", "1\n", "main moves");
    let pid_file = p.base.join("hook.pid");
    let release = p.base.join("release");
    // Releases the paused hook however the test ends, before the temp folder goes.
    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }
    let _release = Release(release.clone());
    // Pauses the first rebase only, for at most a minute.
    hook(
        &p,
        "pre-rebase",
        &format!(
            "[ -e '{0}' ] && exit 0\necho $$ > '{0}'\nn=0\nwhile [ ! -e '{1}' ] && [ $n -lt 1200 ]; do sleep 0.05; n=$((n + 1)); done",
            pid_file.display(),
            release.display()
        ),
    );
    let mut first = p
        .repo
        .atelier_cmd(&wt, &["follow"], &[])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the pre-rebase hook", || pid_file.exists());
    first.kill().unwrap();
    first.wait().unwrap();

    let out = p.follow(&wt);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("processes of an earlier run are still alive"),
        "{}",
        stderr(&out)
    );
    assert!(p.record("b").pending.is_some());

    fs::write(&release, "").unwrap();
    let mut last = None;
    wait_for("the orphaned rebase to end", || {
        let out = p.follow(&wt);
        let done = code(&out) == 0;
        last = Some(out);
        done
    });
    unhook(&p, "pre-rebase");
    let out = last.unwrap();
    assert_eq!(
        stdout(&out),
        format!("follow: b already on {} (1 commit)\n", short(p.main(), &m))
    );
    let rec = p.record("b");
    assert_eq!(rec.pending, None);
    assert_eq!(rec.base, m);
}

// ---------------------------------------------------------------------------------------------
// More probes found in review.

const CREATE_REF: &str = "update-ref --no-deref -m atelier start";

/// Starts `branch` with `--path` and kills atelier just after `step` ran, leaving an unfinished
/// start. Returns the worktree path.
fn start_killed_after(p: &Proj, branch: &str, on: Option<&str>, step: &str) -> PathBuf {
    let path = killing_git(p);
    let wt = p.wt(branch);
    let mut args = vec!["start", branch];
    if let Some(l) = on {
        args.extend(["--on", l]);
    }
    args.extend(["--path", wt.to_str().unwrap()]);
    let out = p
        .repo
        .atelier_env(p.main(), &args, &[("PATH", &path), ("KILL_AFTER", step)]);
    assert!(killed(&out), "{}", stderr(&out));
    assert!(p.record(branch).starting.is_some());
    wt
}

/// a worktree with a rebase or a bisect of an unfinished branch open is listed as
/// detached, but the branch is in use: start again neither deletes its ref nor undoes it.
#[test]
fn an_unfinished_start_is_never_undone_while_a_rebase_or_bisect_of_it_is_open() {
    let p = Proj::new();
    commit(p.main(), "f", "1\n", "one");
    commit(p.main(), "f", "2\n", "two");
    let base = head(p.main());

    // The review's probe: a rebase of b stopped in wt-b.
    let wt_b = start_killed_after(&p, "b", None, "worktree add");
    let out = git_cmd(&wt_b)
        .args(["rebase", "--exec", "false", "HEAD~1"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(rebase_open(&wt_b));
    let other = p.base.join("wt-x");
    let out = p.run(p.main(), &["start", "b", "--path", other.to_str().unwrap()]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("a rebase of b is open in {}", wt_b.display())),
        "{}",
        stderr(&out)
    );
    assert!(!stdout(&out).contains("undid"), "{}", stdout(&out));
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/b"]), base);
    assert!(p.record("b").starting.is_some());
    assert!(!other.exists());
    let out = p.run(p.main(), &["start", "b", "--abandon"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a rebase of b is open in"),
        "{}",
        stderr(&out)
    );
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/b"]), base);
    // The worktree's rebase still finishes onto its branch.
    git_edit(&wt_b, &["rebase", "--continue"]);
    assert_eq!(git(&wt_b, &["symbolic-ref", "HEAD"]), "refs/heads/b");

    // A bisect started on c, with a commit of the range checked out (detached).
    let wt_c = start_killed_after(&p, "c", None, "worktree add");
    git(&wt_c, &["bisect", "start", "HEAD", "HEAD~2"]);
    let out = git_cmd(&wt_c)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the bisect detached HEAD");
    let out = p.run(p.main(), &["start", "c", "--path", other.to_str().unwrap()]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!("a bisect of c is open in {}", wt_c.display())),
        "{}",
        stderr(&out)
    );
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/c"]), base);
    assert!(p.record("c").starting.is_some());
    git(&wt_c, &["bisect", "reset"]);
    assert_eq!(git(&wt_c, &["symbolic-ref", "HEAD"]), "refs/heads/c");
}

/// a fresh worktree detached at the integration branch's tip, which has commits
/// the lower branch lacks, is fresh: those commits are main's, not the worktree's own.
#[test]
fn start_in_place_accepts_a_fresh_worktree_detached_at_a_newer_main() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let a_tip = commit(&wt_a, "a", "1\n", "A: own");
    commit(p.main(), "m", "1\n", "main-moved");
    let t3 = p.base.join("t3");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            t3.to_str().unwrap(),
            "main",
        ],
    );
    let out = p.run(&t3, &["start", "c", "--on", "a"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(git(&t3, &["symbolic-ref", "HEAD"]), "refs/heads/c");
    assert_eq!(head(&t3), a_tip);
    assert_eq!(p.record("c").base, a_tip);
}

/// an in-place start whose checkout failed prints commands that parse: the finish
/// command run from that worktree, and `git switch --detach` (never a worktree removal) to undo
/// it, followed by `--abandon`.
#[test]
fn an_unfinished_in_place_start_prints_runnable_commands() {
    let p = Proj::new();
    let base = head(p.main());
    let t3 = p.base.join("t3 b");
    git(
        p.main(),
        &["worktree", "add", "-q", "--detach", t3.to_str().unwrap()],
    );
    hook(&p, "post-checkout", "exit 1");
    let out = p.run(&t3, &["start", "b"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let quoted = format!("'{}'", t3.display());
    let err = stderr(&out);
    assert!(
        err.contains(&format!("run cd {quoted} && atelier start b;")),
        "{err}"
    );
    assert!(
        err.contains(&format!("git -C {quoted} switch --detach")),
        "{err}"
    );
    assert!(!err.contains("worktree remove"), "{err}");
    assert!(err.contains("atelier start b --abandon"), "{err}");
    unhook(&p, "post-checkout");

    // The undo: detach, then abandon.
    git(&t3, &["switch", "-q", "--detach"]);
    let out = p.run(p.main(), &["start", "b", "--abandon"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        stdout(&out).contains("abandoned the unfinished start of b"),
        "{}",
        stdout(&out)
    );
    assert!(git(p.main(), &["branch", "--list", "b"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "b").unwrap().is_none());
    assert_eq!(head(&t3), base);

    // The finish: the printed command, run as printed.
    hook(&p, "post-checkout", "exit 1");
    assert_eq!(code(&p.run(&t3, &["start", "b"])), 4);
    unhook(&p, "post-checkout");
    let like = p.repo.atelier_cmd(p.main(), &[], &[]);
    let mut sh = std::process::Command::new("sh");
    for (k, v) in like.get_envs() {
        match v {
            Some(v) => sh.env(k, v),
            None => sh.env_remove(k),
        };
    }
    let finish = sh
        .arg("-c")
        .arg(format!("cd {quoted} && \"$ATELIER\" start b"))
        .env("ATELIER", like.get_program())
        .output()
        .unwrap();
    assert_eq!(code(&finish), 0, "{}", stderr(&finish));
    assert!(
        stdout(&finish).contains("finished the interrupted start of b"),
        "{}",
        stdout(&finish)
    );
    assert_eq!(p.record("b").starting, None);
}

/// an unfinished branch that holds work and is checked out nowhere: the message
/// says how to keep the work and how to undo the start; `--abandon` refuses until the ref is
/// gone, and never deletes work.
#[test]
fn an_unfinished_branch_with_work_says_how_to_keep_it_and_abandon_refuses_it() {
    let p = Proj::new();
    let wt = start_killed_after(&p, "b", None, "worktree add");
    let own = commit(&wt, "own", "1\n", "own work");
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    let args = ["start", "b", "--path", wt.to_str().unwrap()];
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    let err = stderr(&out);
    for want in [
        format!("git worktree add {} b", wt.display()),
        format!("atelier start b --path {}", wt.display()),
        "git branch <new-name> b".to_owned(),
        "git branch -D b, then atelier start b --abandon".to_owned(),
    ] {
        assert!(err.contains(&want), "{want}: {err}");
    }
    let out = p.run(p.main(), &["start", "b", "--abandon"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("git branch <new-name> b"),
        "{}",
        stderr(&out)
    );
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/b"]), own);

    git(p.main(), &["branch", "keep", "b"]);
    git(p.main(), &["branch", "-q", "-D", "b"]);
    let out = p.run(p.main(), &["start", "b", "--abandon"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(BranchRecord::read(&p.state(), "b").unwrap().is_none());
    assert_eq!(git(p.main(), &["rev-parse", "refs/heads/keep"]), own);

    // Nothing to abandon now.
    let out = p.run(p.main(), &["start", "b", "--abandon"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("b has no unfinished start to abandon"),
        "{}",
        stderr(&out)
    );
}

/// an unfinished start of C on a merged and cleaned A keeps A's name taken; the
/// refusal names `--abandon`, which frees it without recreating C.
#[test]
fn abandon_frees_a_lower_name_held_by_an_unfinished_dependent() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    commit(&wt_a, "a", "1\n", "A");
    start_killed_after(&p, "c", Some("a"), CREATE_REF);
    p.land("a");
    p.clean("a");
    let wt_a = p.wt("a");
    let args = ["start", "a", "--path", wt_a.to_str().unwrap()];
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("the start of c did not finish: atelier start c --abandon undoes it"),
        "{}",
        stderr(&out)
    );
    let out = p.run(p.main(), &["start", "c", "--abandon"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(git(p.main(), &["branch", "--list", "c"]).is_empty());
    assert!(BranchRecord::read(&p.state(), "c").unwrap().is_none());
    let out = p.run(p.main(), &args);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
}

/// a rebase that finished is reported from the settled record (exit 0) even when
/// a hook left a background process, which is stopped and named in a warning.
#[test]
fn a_finished_rebase_whose_hook_left_a_process_is_reported_finished() {
    let p = Proj::new();
    let wt = p.start("b", None);
    commit(&wt, "b", "1\n", "B1");
    let m = commit(p.main(), "m", "1\n", "main moves");
    hook(&p, "post-rewrite", "(sleep 30 >/dev/null 2>&1 &)");
    let out = p.follow(&wt);
    unhook(&p, "post-rewrite");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        format!("follow: b now on {} (1 commit)\n", short(p.main(), &m))
    );
    assert!(
        stderr(&out).contains("follow: warning: the rebase (its hooks) left processes behind")
            && stderr(&out).contains("sleep 30"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("did not"), "{}", stderr(&out));
    let rec = p.record("b");
    assert_eq!((rec.base, rec.pending), (m, None));
}

/// the conflicted file, or a folder on its path, swapped for a link while
/// mergiraf runs: atelier writes nothing, through the link or anywhere else, and leaves the
/// conflict as git's.
#[test]
fn a_file_or_folder_swapped_during_mergiraf_is_never_written_through() {
    for swap in ["SWAP_FILE", "SWAP_FOLDER"] {
        let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
        let path = fake_mergiraf(&p.base);
        let wt = conflicting_pair(&p);
        let outside = p.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x.rs"), "outside\n").unwrap();
        let target = if swap == "SWAP_FILE" {
            outside.join("x.rs")
        } else {
            outside.clone()
        };
        let out = p.repo.atelier_env(
            &wt,
            &["follow"],
            &[("PATH", &path), (swap, target.to_str().unwrap())],
        );
        assert_eq!(code(&out), 5, "{swap}: {}", stderr(&out));
        assert_eq!(stdout(&out), "", "{swap}");
        assert!(
            stderr(&out).contains("\n  src/x.rs\n"),
            "{swap}: {}",
            stderr(&out)
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1, "{swap}");
        assert_eq!(
            read(&outside, "x.rs").as_deref(),
            Some("outside\n"),
            "{swap}"
        );
        let calls = fs::read_to_string(p.base.join("fake-bin/calls")).unwrap();
        assert_eq!(calls, "src/x.rs\n", "{swap}");
        if swap == "SWAP_FOLDER" {
            let moved = read(&wt, "src-moved/x.rs").unwrap();
            assert!(moved.contains("<<<<<<<"), "{moved}");
            assert_eq!(fs::read_dir(wt.join("src-moved")).unwrap().count(), 1);
        }
        let stages = git(&wt, &["ls-files", "-u", "--", "src/x.rs"]);
        assert_eq!(stages.lines().count(), 3, "{swap}: {stages}");
    }
}

/// the file keeps the permission bits git wrote (the merged mode: b removed the
/// executable bit), whether mergiraf resolves it or not.
#[test]
fn mergiraf_keeps_the_merged_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    for (contents, resolved) in [("fn x() {}", true), ("UNSOLVABLE", false)] {
        let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
        let path = fake_mergiraf(&p.base);
        commit(p.main(), "x.rs", &format!("{contents} base\n"), "base: x");
        fs::set_permissions(p.main().join("x.rs"), fs::Permissions::from_mode(0o755)).unwrap();
        git(p.main(), &["commit", "-q", "-am", "x executable"]);
        let wt = p.start("b", None);
        fs::set_permissions(wt.join("x.rs"), fs::Permissions::from_mode(0o644)).unwrap();
        commit(
            &wt,
            "x.rs",
            &format!("{contents} b\n"),
            "b: x not executable",
        );
        commit(p.main(), "x.rs", &format!("{contents} main\n"), "main: x");
        let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
        assert_eq!(code(&out), 5, "{}", stderr(&out));
        assert_eq!(
            stdout(&out) == "resolved by mergiraf: x.rs\n",
            resolved,
            "{}",
            stdout(&out)
        );
        let mode = fs::metadata(wt.join("x.rs")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "{contents}: {mode:o}");
        if resolved {
            assert!(git(&wt, &["ls-files", "-s", "x.rs"]).starts_with("100644 "));
        } else {
            assert!(read(&wt, "x.rs").unwrap().contains("<<<<<<<"));
        }
    }
}

/// main renamed the file while b changed it: git puts the conflict at the new
/// path, which the merge base does not have. Mergiraf is not run on it; it stays git's.
#[test]
fn mergiraf_is_not_run_on_a_renamed_file() {
    let p = Proj::with_settings("\n[merge]\nresolver = \"mergiraf\"\n");
    let path = fake_mergiraf(&p.base);
    let lines = |five: &str| {
        (1..=10)
            .map(|n| {
                if n == 5 {
                    format!("fn f5() {{ {five} }}\n")
                } else {
                    format!("fn f{n}() {{}}\n")
                }
            })
            .collect::<String>()
    };
    commit(p.main(), "old.rs", &lines("0"), "base: old");
    let wt = p.start("b", None);
    commit(&wt, "old.rs", &lines("2"), "b: old");
    git(p.main(), &["mv", "old.rs", "new.rs"]);
    commit(p.main(), "new.rs", &lines("1"), "main: rename to new");
    let out = p.repo.atelier_env(&wt, &["follow"], &[("PATH", &path)]);
    assert_eq!(code(&out), 5, "{}", stderr(&out));
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("\n  new.rs\n"), "{}", stderr(&out));
    assert_eq!(
        git(&wt, &["ls-files", "-u", "--", "new.rs"])
            .lines()
            .count(),
        3
    );
    assert!(read(&wt, "new.rs").unwrap().contains("<<<<<<<"));
    assert!(!p.base.join("fake-bin/calls").exists());
}
