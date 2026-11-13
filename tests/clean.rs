//! `atelier tmp` and `atelier clean` on throwaway repositories: the plan, the
//! deletions, and every destructive case found in review. Merged state is set the way `merge` will, through
//! `MergeFacts::record`.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use atelier::git::Checkout;
use atelier::locks::{HeldLock, LockKey};
use atelier::records::{BranchRecord, BranchState, MergeFacts, TmpEntry};
use atelier::state::State;
use common::*;

struct Proj {
    repo: Repo,
    base: PathBuf,
    /// The `TMPDIR` the programs under test get.
    tmpdir: PathBuf,
}

impl Proj {
    fn new() -> Proj {
        Self::with_settings("")
    }

    fn with_settings(extra: &str) -> Proj {
        let repo = Repo::new();
        repo.install_self(VERSION);
        repo.write(".gitignore", "target/\n");
        repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\n\n[platforms.macos-aarch64]\nrun = \"check\"\n{extra}"
            ),
        );
        repo.write_rules();
        repo.commit_all("pin");
        let base = repo.root.parent().unwrap().to_path_buf();
        let tmpdir = base.join("tmproot");
        fs::create_dir_all(&tmpdir).unwrap();
        Proj { repo, base, tmpdir }
    }

    fn main(&self) -> &Path {
        &self.repo.root
    }

    fn wt(&self, branch: &str) -> PathBuf {
        self.base.join(format!("wt-{}", branch.replace('/', "-")))
    }

    fn gate(&self) -> PathBuf {
        let mut n = self.main().file_name().unwrap().to_owned();
        n.push(".gate");
        self.main().with_file_name(n)
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let t = self.tmpdir.to_str().unwrap();
        self.repo.atelier_env(dir, args, &[("TMPDIR", t)])
    }

    fn clean(&self, args: &[&str]) -> Output {
        let mut a = vec!["clean"];
        a.extend(args);
        self.run(self.main(), &a)
    }

    /// `atelier start <branch> [--on <lower>] --path <its worktree>`, with one commit of work.
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
        commit(
            &wt,
            &format!("{}.txt", branch.replace('/', "-")),
            "work\n",
            "work",
        );
        wt
    }

    fn state(&self) -> State {
        State::open_at(self.main().join(".git/atelier")).unwrap()
    }

    fn record(&self, branch: &str) -> Option<BranchRecord> {
        BranchRecord::read(&self.state(), branch).unwrap()
    }

    /// Merges the branch into main with `--no-ff` and records it merged, with the worktree it is
    /// checked out in, as `approve` and `merge` will.
    fn land(&self, branch: &str, wt: &Path) -> String {
        let commit = git(self.main(), &["rev-parse", branch]);
        git(
            self.main(),
            &["merge", "-q", "--no-ff", "-m", "land", branch],
        );
        self.set_merged(branch, &commit, wt);
        commit
    }

    fn set_merged(&self, branch: &str, commit: &str, wt: &Path) {
        let lifetime = self.record(branch).unwrap().lifetime;
        MergeFacts::of_worktree(commit, wt)
            .unwrap()
            .record(&self.state(), branch, &lifetime)
            .unwrap();
    }

    fn branch_exists(&self, branch: &str) -> bool {
        git_cmd(self.main())
            .args([
                "rev-parse",
                "-q",
                "--verify",
                &format!("refs/heads/{branch}"),
            ])
            .output()
            .unwrap()
            .status
            .success()
    }

    /// `atelier tmp` in a worktree with a shell command.
    fn tmp(&self, wt: &Path, short: bool, script: &str) -> Output {
        let mut a = vec!["tmp"];
        if short {
            a.push("--short");
        }
        a.extend(["--", "sh", "-c", script]);
        self.run(wt, &a)
    }

    /// The folders registered under a branch's lifetime.
    fn folders(&self, branch: &str) -> Vec<TmpEntry> {
        let lifetime = self.record(branch).unwrap().lifetime;
        TmpEntry::list(&self.state(), &lifetime)
            .unwrap()
            .into_iter()
            .map(|(_, e)| e)
            .collect()
    }
}

fn commit(dir: &Path, file: &str, contents: &str, msg: &str) -> String {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

fn exclude(p: &Proj, line: &str) {
    let path = p.main().join(".git/info/exclude");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(line);
    text.push('\n');
    fs::write(path, text).unwrap();
}

fn refused(out: &Output, needle: &str) {
    assert_eq!(code(out), 4, "{}{}", stdout(out), stderr(out));
    let all = format!("{}{}", stdout(out), stderr(out));
    assert!(all.contains(needle), "wanted {needle:?} in:\n{all}");
}

// ---------------------------------------------------------------------------------------------
// clean: the plan and the deletions

#[test]
fn clean_prints_the_plan_and_deletes_nothing_without_yes() {
    let p = Proj::new();
    let wt = p.start("disk-api", None);
    let commit = p.land("disk-api", &wt);

    let out = p.clean(&[]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("would remove worktree"), "{text}");
    assert!(text.contains("would delete branch disk-api"), "{text}");
    assert!(text.contains("nothing was changed"), "{text}");
    assert!(wt.is_dir());
    assert!(p.branch_exists("disk-api"));
    assert_eq!(p.record("disk-api").unwrap().state, BranchState::Merged);
    assert_eq!(git(p.main(), &["rev-parse", "disk-api"]), commit);
}

#[test]
fn clean_removes_the_worktree_the_branch_the_folders_and_retires_the_record() {
    let p = Proj::new();
    let wt = p.start("disk-api", None);
    // Ignored build output goes with the worktree (git worktree remove deletes it).
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/out.o"), "x").unwrap();
    let out = p.tmp(&wt, false, "echo hello > \"$ATELIER_TMP/f\"");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let out = p.tmp(&wt, true, "touch \"$ATELIER_TMP/g\"");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let folders = p.folders("disk-api");
    assert_eq!(folders.len(), 2);
    p.land("disk-api", &wt);

    let out = p.clean(&["disk-api", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!wt.exists());
    assert!(!p.branch_exists("disk-api"));
    for f in &folders {
        assert!(!f.path.exists(), "{}", f.path.display());
    }
    assert!(
        p.record("disk-api").is_none(),
        "record kept with no dependent"
    );
    // main and its checkout are untouched.
    assert!(p.main().join("disk-api.txt").is_file());
    let out = p.clean(&[]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("no merged branch"));
}

#[test]
fn clean_keeps_the_cleaned_record_while_a_dependent_names_it() {
    let p = Proj::new();
    let a = p.start("a", None);
    let b = p.start("b", Some("a"));
    p.land("a", &a);
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!a.exists());
    assert!(b.is_dir());
    let rec = p.record("a").expect("kept for its dependent");
    assert_eq!(rec.state, BranchState::Cleaned);
    assert!(
        MergeFacts::read(&rec).unwrap().is_none(),
        "authority retired"
    );
    // A second clean of a cleaned branch deletes nothing and is not an error.
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("already cleaned"));
}

#[test]
fn a_branch_with_no_merge_record_is_never_touched() {
    let p = Proj::new();
    // Started by atelier, not merged.
    let active = p.start("active", None);
    // Not started by atelier at all, merged into main like any ancestor of it.
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
    commit(&stray, "s.txt", "s\n", "s");
    git(p.main(), &["merge", "-q", "--no-ff", "-m", "m", "stray"]);
    // Merged state without the facts `merge` records.
    let nofacts = p.start("nofacts", None);
    let lifetime = p.record("nofacts").unwrap().lifetime;
    let mut rec = p.record("nofacts").unwrap();
    rec.state = BranchState::Merged;
    rec.write(&p.state()).unwrap();
    assert_eq!(rec.lifetime, lifetime);

    refused(&p.clean(&["active", "--yes"]), "not merged");
    refused(&p.clean(&["stray", "--yes"]), "no record");
    refused(&p.clean(&["nofacts", "--yes"]), "no merge facts");
    // With no branch named, only merged records are considered, and the facts-less one refuses.
    refused(&p.clean(&["--yes"]), "no merge facts");
    for (w, b) in [
        (&active, "active"),
        (&stray, "stray"),
        (&nofacts, "nofacts"),
    ] {
        assert!(w.is_dir(), "{b}");
        assert!(p.branch_exists(b), "{b}");
    }
}

// ---------------------------------------------------------------------------------------------
// clean: the worktree

#[test]
fn a_worktree_switched_to_another_branch_with_committed_work_is_kept() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    git(&wt, &["switch", "-q", "-c", "other"]);
    let work = commit(&wt, "precious.txt", "p\n", "precious");

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "has refs/heads/other checked out, not a");
    assert!(wt.join("precious.txt").is_file());
    assert_eq!(git(&wt, &["rev-parse", "other"]), work);
    assert!(p.branch_exists("a"), "the branch stays too");
    assert_eq!(p.record("a").unwrap().state, BranchState::Merged);

    // The same with a clean tree and no new commit: a different branch is not the merged one.
    git(&wt, &["reset", "-q", "--hard", "a"]);
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "checked out, not a");
    assert!(wt.is_dir());
}

#[test]
fn a_detached_worktree_is_kept() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    git(&wt, &["switch", "-q", "--detach"]);
    refused(&p.clean(&["a", "--yes"]), "detached HEAD");
    assert!(wt.is_dir());
    assert!(p.branch_exists("a"));
}

#[test]
fn a_worktree_recreated_at_the_same_path_is_not_the_recorded_one() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let commit = p.land("a", &wt);
    // The original is moved aside (so the new folder cannot reuse its inode) and a different
    // worktree takes its place with the branch checked out at the merged commit.
    let aside = p.base.join("aside");
    git(
        p.main(),
        &[
            "worktree",
            "move",
            wt.to_str().unwrap(),
            aside.to_str().unwrap(),
        ],
    );
    git(&aside, &["switch", "-q", "--detach"]);
    git(
        p.main(),
        &["worktree", "add", "-q", wt.to_str().unwrap(), "a"],
    );
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), commit);
    fs::write(wt.join(".owners"), "mine").unwrap();
    git(&wt, &["add", ".owners"]);
    git(&wt, &["reset", "-q"]);
    fs::remove_file(wt.join(".owners")).unwrap();
    assert_ne!(
        fs::metadata(&wt).unwrap().ino(),
        MergeFacts::read(&p.record("a").unwrap())
            .unwrap()
            .unwrap()
            .ino
    );

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "is checked out in");
    assert!(wt.is_dir(), "the replacement is not deleted");
    assert_eq!(git(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/a");
    assert!(p.branch_exists("a"));
    assert_eq!(p.record("a").unwrap().state, BranchState::Merged);
}

#[test]
fn the_branch_checked_out_in_another_worktree_is_kept() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let commit = p.land("a", &wt);
    // The recorded worktree is removed by hand and the owner checks the branch out elsewhere.
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    let other = p.base.join("owners");
    git(
        p.main(),
        &["worktree", "add", "-q", other.to_str().unwrap(), "a"],
    );
    assert_eq!(git(&other, &["rev-parse", "HEAD"]), commit);

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "checked out in");
    assert!(other.is_dir());
    assert!(p.branch_exists("a"));
}

#[test]
fn a_nested_worktree_stops_the_deletion() {
    let p = Proj::new();
    let wt = p.start("outer", None);
    p.land("outer", &wt);
    // A worktree inside the recorded one, kept out of its status so only the nesting refuses.
    exclude(&p, "inner/");
    let inner = wt.join("inner");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inner",
            inner.to_str().unwrap(),
        ],
    );
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");

    let out = p.clean(&["outer", "--yes"]);
    refused(&out, "nested inside");
    assert!(inner.is_dir());
    assert!(wt.is_dir());
    assert!(p.branch_exists("outer"));
}

#[test]
fn a_worktree_inside_another_worktree_or_the_main_checkout_is_not_deleted() {
    let p = Proj::new();
    let outer = p.start("outer", None);
    exclude(&p, "in-outer/");
    exclude(&p, "in-main/");
    let in_outer = outer.join("in-outer");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b1",
            in_outer.to_str().unwrap(),
        ],
    );
    commit(&in_outer, "x.txt", "x\n", "x");
    let in_main = p.main().join("in-main");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b2",
            in_main.to_str().unwrap(),
        ],
    );
    commit(&in_main, "y.txt", "y\n", "y");
    // Records for them, as if atelier had started them.
    for (b, w) in [("b1", &in_outer), ("b2", &in_main)] {
        let base = git(p.main(), &["rev-parse", "main"]);
        BranchRecord {
            name: b.into(),
            lifetime: atelier::records::new_lifetime_id().unwrap(),
            lower: "main".into(),
            lower_is_integration: true,
            lower_lifetime: None,
            base,
            state: BranchState::Active,
            pending: None,
            starting: None,
            later: toml::Table::new(),
        }
        .write(&p.state())
        .unwrap();
        let commit = git(p.main(), &["rev-parse", b]);
        git(p.main(), &["merge", "-q", "--no-ff", "-m", "m", b]);
        p.set_merged(b, &commit, w);
    }
    refused(&p.clean(&["b1", "--yes"]), "nested inside");
    refused(&p.clean(&["b2", "--yes"]), "nested inside");
    assert!(in_outer.is_dir() && in_main.is_dir());
}

#[test]
fn the_main_checkout_and_the_gate_are_never_deleted() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    // Facts that name the main checkout.
    let lifetime = p.record("a").unwrap().lifetime;
    let commit = git(p.main(), &["rev-parse", "a"]);
    MergeFacts::of_worktree(&commit, p.main())
        .unwrap()
        .record(&p.state(), "a", &lifetime)
        .unwrap();
    refused(&p.clean(&["a", "--yes"]), "main checkout");
    assert!(p.main().join(".git").is_dir());
    assert!(wt.is_dir());

    // Facts that name the gate worktree (with the branch checked out there).
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    git(
        p.main(),
        &["worktree", "add", "-q", p.gate().to_str().unwrap(), "a"],
    );
    p.set_merged("a", &commit, &p.gate());
    refused(&p.clean(&["a", "--yes"]), "gate worktree");
    assert!(p.gate().is_dir());
    assert!(p.branch_exists("a"));
}

#[test]
fn uncommitted_and_untracked_files_stop_the_deletion_but_ignored_ones_do_not() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    fs::write(wt.join("a.txt"), "edited\n").unwrap();
    refused(
        &p.clean(&["a", "--yes"]),
        "uncommitted or untracked files (M a.txt)",
    );
    git(&wt, &["checkout", "-q", "a.txt"]);
    fs::write(wt.join("notes.md"), "mine\n").unwrap();
    refused(&p.clean(&["a", "--yes"]), "?? notes.md");
    assert!(wt.join("notes.md").is_file());
    fs::remove_file(wt.join("notes.md")).unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/o"), "x").unwrap();
    assert_eq!(code(&p.clean(&["a", "--yes"])), 0);
    assert!(!wt.exists());
}

#[test]
fn a_worktree_with_an_operation_in_progress_is_kept() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    let gd = git(&wt, &["rev-parse", "--absolute-git-dir"]);
    fs::write(
        Path::new(&gd).join("MERGE_HEAD"),
        git(&wt, &["rev-parse", "HEAD"]),
    )
    .unwrap();
    refused(&p.clean(&["a", "--yes"]), "operation in progress");
    assert!(wt.is_dir());
}

#[test]
fn a_running_check_in_the_worktree_holds_it() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    let co = Checkout::find(&wt).unwrap().unwrap();
    let state = p.state();
    let _held = HeldLock::acquire(&state, LockKey::worktree(&co)).unwrap();
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "held by another atelier command");
    assert!(wt.is_dir());
    assert!(p.branch_exists("a"));
}

// ---------------------------------------------------------------------------------------------
// clean: the branch

#[test]
fn a_branch_that_moved_after_the_merge_is_kept() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    // The worker commits on after the merge: the worktree holds unmerged work.
    let later = commit(&wt, "more.txt", "m\n", "more");
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "not at the merged commit");
    assert!(wt.is_dir());
    assert_eq!(git(p.main(), &["rev-parse", "a"]), later);

    // The worktree is gone, the branch was moved by hand: the ref is not deleted.
    git(
        p.main(),
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "moved after the merge");
    assert_eq!(git(p.main(), &["rev-parse", "a"]), later);
    assert_eq!(p.record("a").unwrap().state, BranchState::Merged);
}

#[test]
fn a_symbolic_branch_ref_is_refused_not_followed() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let commit = p.land("a", &wt);
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    git(p.main(), &["branch", "-q", "-D", "a"]);
    git(
        p.main(),
        &["symbolic-ref", "refs/heads/a", "refs/heads/main"],
    );
    let main_tip = git(p.main(), &["rev-parse", "main"]);
    assert_ne!(main_tip, commit);
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "symbolic ref");
    assert_eq!(git(p.main(), &["rev-parse", "main"]), main_tip);
}

#[test]
fn a_rebase_open_on_the_branch_blocks_the_clean() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    let gd = git(&wt, &["rev-parse", "--absolute-git-dir"]);
    let rm = Path::new(&gd).join("rebase-merge");
    fs::create_dir_all(&rm).unwrap();
    fs::write(rm.join("head-name"), "refs/heads/a\n").unwrap();
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 4, "{}{}", stdout(&out), stderr(&out));
    assert!(wt.is_dir());
    assert!(p.branch_exists("a"));
}

// ---------------------------------------------------------------------------------------------
// clean: the folders

fn registered_folder(p: &Proj, wt: &Path, branch: &str) -> TmpEntry {
    let out = p.tmp(wt, false, "echo data > \"$ATELIER_TMP/data\"");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let mut all = p.folders(branch);
    all.pop().unwrap()
}

#[test]
fn a_registry_entry_for_a_folder_atelier_did_not_make_is_refused() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let victim = p.base.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("keep"), "keep").unwrap();
    let lifetime = p.record("a").unwrap().lifetime;
    // A real folder with the right identity, at a path `tmp` would never make; and the same
    // spelled through `..`.
    let e = TmpEntry::of(&victim).unwrap();
    e.register(&p.state(), &lifetime, "aa11").unwrap();
    let mut dotted = e.clone();
    dotted.path = p.base.join("tmproot/../victim");
    dotted.register(&p.state(), &lifetime, "bb22").unwrap();
    p.land("a", &wt);

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "is not where atelier makes temp folders");
    assert_eq!(fs::read_to_string(victim.join("keep")).unwrap(), "keep");
    assert!(wt.is_dir(), "nothing else was deleted for the branch");
    assert!(p.branch_exists("a"));
}

#[test]
fn a_registered_folder_replaced_by_a_link_is_not_followed() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    let victim = p.base.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("keep"), "keep").unwrap();
    fs::remove_dir_all(&entry.path).unwrap();
    std::os::unix::fs::symlink(&victim, &entry.path).unwrap();
    p.land("a", &wt);

    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("left alone"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(victim.join("keep")).unwrap(), "keep");
    assert!(
        fs::symlink_metadata(&entry.path).unwrap().is_symlink(),
        "the link is not ours either"
    );
    assert!(!wt.exists());
}

#[test]
fn a_registered_folder_behind_a_linked_parent_is_refused() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    // The branch's folder is moved away and a link left in its place: the registered path now
    // resolves (same identity) through a link.
    let parent = entry.path.parent().unwrap().to_path_buf();
    let moved = p.base.join("moved-away");
    fs::rename(&parent, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &parent).unwrap();
    p.land("a", &wt);

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "has a link in its path");
    assert!(
        moved
            .join(entry.path.file_name().unwrap())
            .join("data")
            .is_file()
    );
    assert!(wt.is_dir());
}

#[test]
fn a_folder_with_another_identity_at_the_registered_path_is_left_alone() {
    // The identity check: a folder at the registered path that is not the registered one.
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    let path = entry.path.clone();
    fs::remove_dir_all(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("theirs"), "t").unwrap();
    // On a file system that reuses the inode the folder is the same one to every check; the
    // contents are then ours. Skip the assertion about the file in that case.
    let same = fs::metadata(&path).unwrap().ino() == entry.ino;
    p.land("a", &wt);
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    if !same {
        assert!(
            path.join("theirs").is_file(),
            "a folder with another identity is left alone"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// tmp and clean together: consumers, leftovers

#[test]
fn a_running_tmp_command_holds_its_folder_against_clean() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let child = p
        .repo
        .atelier_cmd(
            &wt,
            &[
                "tmp",
                "--",
                "sh",
                "-c",
                "touch \"$ATELIER_TMP/started\"; while [ ! -e \"$ATELIER_TMP/stop\" ]; do sleep 0.05; done",
            ],
            &[("TMPDIR", p.tmpdir.to_str().unwrap())],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let folder = loop {
        if let Some(e) = p.folders("a").pop()
            && e.path.join("started").exists()
        {
            break e.path;
        }
        assert!(Instant::now() < deadline, "tmp command never started");
        std::thread::sleep(Duration::from_millis(50));
    };
    // Merged while the command still runs (the folder is in use).
    p.land("a", &wt);
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "held by another atelier command");
    assert!(folder.is_dir());
    assert!(wt.is_dir());
    assert!(p.branch_exists("a"));

    fs::write(folder.join("stop"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    // Once the command is done the folder is no longer held.
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(!folder.exists());
}

#[test]
fn leftovers_block_the_clean_and_are_printed() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let out = p.tmp(
        &wt,
        false,
        "echo vm-7f3a >> \"$ATELIER_TMP/leftovers\"; echo vm-9c0b >> \"$ATELIER_TMP/leftovers\"",
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let folder = p.folders("a").pop().unwrap().path;
    p.land("a", &wt);

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "vm-7f3a");
    assert!(stdout(&out).contains("vm-9c0b"), "{}", stdout(&out));
    assert!(folder.is_dir());
    assert!(
        wt.is_dir(),
        "nothing is deleted for the branch while something is left outside"
    );
    assert!(p.branch_exists("a"));

    // A leftovers file that is a link is refused, never read through.
    fs::remove_file(folder.join("leftovers")).unwrap();
    std::os::unix::fs::symlink("/etc/hosts", folder.join("leftovers")).unwrap();
    refused(&p.clean(&["a", "--yes"]), "not a regular file");

    // Emptied: the folder and the rest go.
    fs::remove_file(folder.join("leftovers")).unwrap();
    fs::write(folder.join("leftovers"), "").unwrap();
    assert_eq!(code(&p.clean(&["a", "--yes"])), 0);
    assert!(!folder.exists());
    assert!(!wt.exists());
}

#[test]
fn the_short_folder_is_registered_short_and_private() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let out = p.tmp(&wt, true, "echo \"$ATELIER_TMP\" > \"$ATELIER_TMP/where\"");
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let entry = p.folders("a").pop().unwrap();
    let said = fs::read_to_string(entry.path.join("where")).unwrap();
    let said = said.trim();
    assert!(said.starts_with("/tmp/atl-"), "{said}");
    assert!(said.len() < 40, "{said}");
    assert_eq!(fs::canonicalize(said).unwrap(), entry.path);
    let meta = fs::metadata(&entry.path).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o700);
    assert_eq!((meta.dev(), meta.ino()), (entry.dev, entry.ino));
    assert!(stdout(&out).contains("GREEN"), "{}", stdout(&out));
    p.land("a", &wt);
    assert_eq!(code(&p.clean(&["a", "--yes"])), 0);
    assert!(!entry.path.exists());
}

#[test]
fn clean_runs_mbx_clean_under_the_removed_worktree_and_a_failure_is_resumable() {
    let p = Proj::with_settings(
        "\n[prerequisites.mbx]\nversion = \"1.2.3\"\ncargo = [\"/usr/bin/cargo\"]\n",
    );
    let wt = p.start("a", None);
    p.land("a", &wt);
    let bin = p.base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = p.base.join("mbx.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {}\nif [ -e {}/fail ]; then echo mbx-says-no; exit 3; fi\nexit 0\n",
        log.display(),
        bin.display()
    );
    fs::write(bin.join("mbx"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("mbx"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(bin.join("fail"), "").unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |args: &[&str]| {
        p.repo.atelier_env(
            p.main(),
            args,
            &[("PATH", &path), ("TMPDIR", p.tmpdir.to_str().unwrap())],
        )
    };

    let out = run(&["clean", "a", "--yes"]);
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(format!("{}{}", stdout(&out), stderr(&out)).contains("mbx-says-no"));
    assert!(
        !wt.exists() && !p.branch_exists("a"),
        "the earlier steps were done"
    );
    assert_eq!(
        p.record("a").unwrap().state,
        BranchState::Merged,
        "the record stays merged"
    );

    fs::remove_file(bin.join("fail")).unwrap();
    let out = run(&["clean", "a", "--yes"]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(p.record("a").is_none());
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|l| *l == format!("clean --under {}", wt.display()))
            .count(),
        2,
        "{calls}"
    );
}

fn fake_mbx(p: &Proj, fail_gc: bool) -> (PathBuf, String) {
    let bin = p.base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = p.base.join("mbx.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = gc ] && [ {} = 1 ]; then echo gc-says-no; exit 3; fi\nexit 0\n",
        log.display(),
        u8::from(fail_gc)
    );
    fs::write(bin.join("mbx"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("mbx"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    (log, path)
}

const MBX_SETTINGS: &str =
    "\n[prerequisites.mbx]\nversion = \"1.2.3\"\ncargo = [\"/usr/bin/cargo\"]\n";

#[test]
fn clean_runs_mbx_gc_once_after_all_branches_and_prints_it_in_the_plan() {
    let p = Proj::with_settings(MBX_SETTINGS);
    for b in ["a", "b"] {
        let wt = p.start(b, None);
        p.land(b, &wt);
    }
    let (log, path) = fake_mbx(&p, false);
    let env = [
        ("PATH", path.as_str()),
        ("TMPDIR", p.tmpdir.to_str().unwrap()),
    ];

    let out = p.repo.atelier_env(p.main(), &["clean"], &env);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("run mbx gc"), "{}", stdout(&out));
    assert!(!log.exists(), "the plan runs nothing");

    let out = p.repo.atelier_env(p.main(), &["clean", "--yes"], &env);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    let calls = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.iter().filter(|l| **l == "gc").count(), 1, "{calls}");
    assert_eq!(lines.last(), Some(&"gc"), "gc runs last: {calls}");
    assert!(p.record("a").is_none() && p.record("b").is_none());
}

#[test]
fn a_failing_mbx_gc_is_reported_and_undoes_nothing() {
    let p = Proj::with_settings(MBX_SETTINGS);
    let wt = p.start("a", None);
    p.land("a", &wt);
    let (_log, path) = fake_mbx(&p, true);
    let out = p.repo.atelier_env(
        p.main(),
        &["clean", "--yes"],
        &[("PATH", &path), ("TMPDIR", p.tmpdir.to_str().unwrap())],
    );
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(format!("{}{}", stdout(&out), stderr(&out)).contains("gc-says-no"));
    assert!(!wt.exists() && !p.branch_exists("a"));
    assert!(p.record("a").is_none(), "the branch stays cleaned");
}

#[test]
fn mbx_is_not_run_on_a_path_that_something_else_occupies() {
    let p = Proj::with_settings(
        "\n[prerequisites.mbx]\nversion = \"1.2.3\"\ncargo = [\"/usr/bin/cargo\"]\n",
    );
    let wt = p.start("a", None);
    p.land("a", &wt);
    // Another folder takes the recorded path after the worktree was removed by hand.
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    git(p.main(), &["branch", "-q", "-D", "a"]);
    fs::create_dir_all(&wt).unwrap();
    let bin = p.base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = p.base.join("mbx.log");
    fs::write(
        bin.join("mbx"),
        format!("#!/bin/sh\necho \"$@\" >> {}\n", log.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("mbx"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = p
        .repo
        .atelier_env(p.main(), &["clean", "a", "--yes"], &[("PATH", &path)]);
    assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    assert!(wt.is_dir());
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !calls.contains("clean --under"),
        "mbx never cleaned a path that is not the recorded worktree: {calls}"
    );
}

// ---------------------------------------------------------------------------------------------
// tmp

#[test]
fn tmp_registers_the_folder_under_the_lifetime_and_runs_the_command_in_place() {
    let p = Proj::new();
    let wt = p.start("a", None);
    fs::create_dir_all(wt.join("sub")).unwrap();
    let out = p.tmp(
        &wt.join("sub"),
        false,
        "pwd; echo \"$ATELIER_TMP\"; test -d \"$ATELIER_TMP\"",
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    let mut lines = text.lines();
    assert_eq!(
        Path::new(lines.next().unwrap()),
        wt.join("sub").canonicalize().unwrap()
    );
    let said = lines.next().unwrap().to_owned();
    let entry = p.folders("a").pop().unwrap();
    assert_eq!(Path::new(&said), entry.path);
    let root = fs::canonicalize(&p.tmpdir).unwrap().join(user_root());
    assert!(entry.path.starts_with(&root), "{}", entry.path.display());
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    assert!(entry.still_matches());
    assert_eq!(fs::metadata(&entry.path).unwrap().mode() & 0o777, 0o700);
    assert!(text.contains("GREEN"), "{text}");
    assert!(entry.path.is_dir(), "the folder stays after the command");
    // A second call makes a second folder under the same lifetime.
    assert_eq!(code(&p.tmp(&wt, false, "true")), 0);
    assert_eq!(p.folders("a").len(), 2);
    // A failing command is RED.
    let out = p.tmp(&wt, false, "echo no >&2; exit 3");
    assert_eq!(code(&out), 1);
    assert!(
        stdout(&out).contains("RED") && stdout(&out).contains("exit 3"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn tmp_refuses_where_the_branch_is_not_ours_to_register_against() {
    let p = Proj::new();
    let wt = p.start("a", None);
    // The main checkout.
    refused(&p.tmp(p.main(), false, "true"), "not in the main checkout");
    // A detached HEAD.
    let detached = p.base.join("detached");
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
    refused(&p.tmp(&detached, false, "true"), "HEAD is detached");
    // The gate worktree.
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "gatebranch",
            p.gate().to_str().unwrap(),
        ],
    );
    refused(&p.tmp(&p.gate(), false, "true"), "gate worktree");
    // A branch atelier did not start.
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
    refused(&p.tmp(&stray, false, "true"), "was not started by atelier");
    // A merged branch has no active record.
    p.land("a", &wt);
    refused(&p.tmp(&wt, false, "true"), "was not started by atelier");
    // A program name that looks like an option.
    let out = p.run(&wt, &["tmp", "--", "-x"]);
    assert_eq!(code(&out), 2);
    // No command: usage.
    assert_eq!(code(&p.run(&wt, &["tmp"])), 2);
    // Nothing was registered anywhere.
    assert!(
        fs::read_dir(p.main().join(".git/atelier/tmp"))
            .map(|d| d.count())
            .unwrap_or(0)
            == 0
    );
}

#[test]
fn tmp_has_no_way_to_name_another_branch_and_holds_the_branch_lock() {
    let p = Proj::new();
    let wt_a = p.start("a", None);
    let wt_b = p.start("b", None);
    // Run in b's worktree: the folder is registered against b only.
    assert_eq!(code(&p.tmp(&wt_b, false, "true")), 0);
    assert_eq!(p.folders("b").len(), 1);
    assert!(p.folders("a").is_empty());
    // A second tmp for the same branch while the lock is held is refused.
    let state = p.state();
    let _held = HeldLock::acquire(&state, LockKey::branch("a")).unwrap();
    refused(
        &p.tmp(&wt_a, false, "true"),
        "held by another atelier command",
    );
}

#[test]
fn a_tmp_command_that_leaves_a_process_behind_is_red() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let out = p.tmp(&wt, false, "sleep 30 </dev/null >/dev/null 2>&1 &");
    assert_eq!(code(&out), 1, "{}{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("RED"), "{}", stdout(&out));
}

#[test]
fn merge_facts_round_trip_and_keep_what_they_do_not_know() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let commit = git(&wt, &["rev-parse", "HEAD"]);
    let lifetime = p.record("a").unwrap().lifetime;
    let mut facts = MergeFacts::of_worktree(&commit, &wt).unwrap();
    facts.later.insert("approved_by".into(), "owner".into());
    facts.record(&p.state(), "a", &lifetime).unwrap();
    let rec = p.record("a").unwrap();
    assert_eq!(rec.state, BranchState::Merged);
    assert_eq!(MergeFacts::read(&rec).unwrap().unwrap(), facts);
    // A record of another lifetime is not written.
    assert!(facts.record(&p.state(), "a", "00ff").is_err());
    MergeFacts::retire(&p.state(), "a", &lifetime).unwrap();
    let rec = p.record("a").unwrap();
    assert_eq!(rec.state, BranchState::Cleaned);
    assert!(MergeFacts::read(&rec).unwrap().is_none());
    // Unreadable facts refuse.
    let mut rec = rec;
    rec.later
        .insert("merged".into(), toml::Value::String("x".into()));
    assert!(MergeFacts::read(&rec).is_err());
}

// ---------------------------------------------------------------------------------------------
// Probes found in review

fn user_root() -> String {
    // SAFETY: geteuid has no preconditions.
    format!("atelier-{}", unsafe { libc::geteuid() })
}

/// A disk image mounted at a folder, detached when dropped. `None` (the test is skipped, and
/// says why) where an ordinary user cannot mount one: anywhere but macOS, or when `hdiutil`
/// fails.
struct Mount {
    at: PathBuf,
}

impl Mount {
    fn at(dir: &Path, images: &Path) -> Option<Mount> {
        if !cfg!(target_os = "macos") {
            eprintln!(
                "skipped: mounting a file system needs hdiutil (macOS); on Linux it needs root"
            );
            return None;
        }
        fs::create_dir_all(dir).unwrap();
        fs::create_dir_all(images).unwrap();
        let img = images.join(format!("img-{}.dmg", fs::read_dir(images).unwrap().count()));
        let made = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "4m", "-fs", "HFS+"])
            .args(["-volname", "atl", "-layout", "NONE"])
            .arg(&img)
            .status()
            .is_ok_and(|s| s.success());
        let mounted = made
            && Command::new("hdiutil")
                .args(["attach", "-quiet", "-nobrowse", "-noverify", "-noautoopen"])
                .arg("-mountpoint")
                .arg(dir)
                .arg(&img)
                .status()
                .is_ok_and(|s| s.success());
        if !mounted {
            eprintln!("skipped: hdiutil could not create and mount a disk image here");
            return None;
        }
        assert_ne!(
            fs::metadata(dir).unwrap().dev(),
            fs::metadata(dir.parent().unwrap()).unwrap().dev()
        );
        Some(Mount {
            at: dir.to_path_buf(),
        })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let detach = |force: bool| {
            let mut c = Command::new("hdiutil");
            c.args(["detach", "-quiet"]);
            if force {
                c.arg("-force");
            }
            c.arg(&self.at).status().is_ok_and(|s| s.success())
        };
        if !detach(false) {
            detach(true);
        }
    }
}

fn all_out(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

/// Nothing was deleted for the branch: its worktree is registered, it and its branch exist, its
/// record is merged.
fn nothing_deleted(p: &Proj, branch: &str, wt: &Path) {
    assert!(wt.join(".git").is_file(), "{}", wt.display());
    assert!(
        git(p.main(), &["worktree", "list", "--porcelain"])
            .contains(&format!("worktree {}", wt.display())),
        "the worktree is still registered"
    );
    assert!(p.branch_exists(branch));
    assert_eq!(p.record(branch).unwrap().state, BranchState::Merged);
}

// Finding 1: a mount inside a temp folder.
#[test]
fn a_mount_inside_a_temp_folder_stops_the_clean_and_the_mounted_files_stay() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    let mnt = entry.path.join("mnt");
    let Some(m) = Mount::at(&mnt, &p.base.join("images")) else {
        return;
    };
    fs::write(m.at.join("precious.txt"), "p").unwrap();
    p.land("a", &wt);

    for args in [&["a"][..], &["a", "--yes"]] {
        let out = p.clean(args);
        refused(&out, "mounted in it");
        assert!(all_out(&out).contains(&mnt.display().to_string()));
    }
    assert!(mnt.join("precious.txt").is_file());
    assert!(entry.path.join("data").is_file());
    nothing_deleted(&p, "a", &wt);

    // The deletion itself never enters the mount either.
    let err = atelier::dirfd::remove_tree(
        entry.path.parent().unwrap(),
        entry.path.file_name().unwrap(),
        (entry.dev, entry.ino),
    )
    .unwrap_err();
    assert!(err.to_string().contains("another device"), "{err}");
    assert!(mnt.join("precious.txt").is_file());

    drop(m);
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!entry.path.exists());
    assert!(!wt.exists());
}

// Finding 4: a mount inside the worktree.
#[test]
fn a_mount_inside_the_worktree_stops_the_clean_before_anything_is_deleted() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    let mnt = wt.join("target/mnt");
    let Some(m) = Mount::at(&mnt, &p.base.join("images")) else {
        return;
    };
    fs::write(m.at.join("keep.txt"), "k").unwrap();
    p.land("a", &wt);

    let out = p.clean(&["a", "--yes"]);
    refused(&out, "mounted in it");
    assert!(all_out(&out).contains(&mnt.display().to_string()));
    assert!(mnt.join("keep.txt").is_file());
    assert!(entry.path.join("data").is_file(), "the temp folder stays");
    nothing_deleted(&p, "a", &wt);

    // Once it is unmounted, clean finishes.
    drop(m);
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!wt.exists());
    assert!(!p.branch_exists("a"));
}

// Finding 2: edits hidden by --assume-unchanged and --skip-worktree.
#[test]
fn local_edits_hidden_by_index_flags_stop_the_clean() {
    let p = Proj::new();
    let wt = p.start("a", None);
    fs::write(wt.join("conf.txt"), "c\n").unwrap();
    commit(&wt, "sk.txt", "s\n", "config");
    p.land("a", &wt);
    git(&wt, &["update-index", "--assume-unchanged", "conf.txt"]);
    git(&wt, &["update-index", "--skip-worktree", "sk.txt"]);
    // Unchanged flagged files are no reason to keep the worktree.
    let out = p.clean(&["a"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));

    fs::write(wt.join("conf.txt"), "local\n").unwrap();
    fs::write(wt.join("sk.txt"), "local\n").unwrap();
    assert_eq!(
        git(&wt, &["status", "--porcelain"]),
        "",
        "git status sees nothing"
    );
    let out = p.clean(&["a", "--yes"]);
    refused(&out, "hidden by --assume-unchanged or --skip-worktree");
    let text = all_out(&out);
    assert!(
        text.contains("conf.txt") && text.contains("sk.txt"),
        "{text}"
    );
    assert_eq!(fs::read_to_string(wt.join("conf.txt")).unwrap(), "local\n");
    nothing_deleted(&p, "a", &wt);

    // Restored, or absent (as a sparse checkout leaves a skip-worktree file): nothing is lost.
    fs::write(wt.join("conf.txt"), "c\n").unwrap();
    fs::remove_file(wt.join("sk.txt")).unwrap();
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!wt.exists());
}

// Finding 3: a repository nested in an ignored folder.
#[test]
fn an_ignored_nested_repository_stops_the_clean_and_the_plan_names_the_ignored_entries() {
    let p = Proj::new();
    let wt = p.start("a", None);
    exclude(&p, "vendor/");
    exclude(&p, "NOTES.md");
    let up = wt.join("vendor/up");
    fs::create_dir_all(&up).unwrap();
    git(&up, &["init", "-q"]);
    let unpushed = commit(&up, "u.txt", "u\n", "unpushed");
    fs::write(wt.join("NOTES.md"), "mine\n").unwrap();
    fs::create_dir_all(wt.join("target")).unwrap();
    fs::write(wt.join("target/out.o"), "x").unwrap();
    p.land("a", &wt);
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");

    for args in [&["a"][..], &["a", "--yes"]] {
        let out = p.clean(args);
        refused(&out, "nested git repository");
        assert!(all_out(&out).contains(&up.display().to_string()));
    }
    assert_eq!(git(&up, &["rev-parse", "HEAD"]), unpushed);
    nothing_deleted(&p, "a", &wt);

    // Moved out: the plan says which ignored entries go with the worktree.
    fs::remove_dir_all(wt.join("vendor")).unwrap();
    let out = p.clean(&["a"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(
        stdout(&out).contains("with its ignored files: NOTES.md, target/"),
        "{}",
        stdout(&out)
    );
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!wt.exists());
}

/// A `git` on `PATH` that logs its arguments and, the first time it is asked to move `victim`,
/// first does what the review's probe did in the window: moves that worktree away and adds a new
/// worktree of branch `d` at its path, with an ignored file.
fn git_wrapper(p: &Proj, victim: &Path) -> (String, PathBuf) {
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();
    let bin = p.base.join("gitbin");
    fs::create_dir_all(&bin).unwrap();
    let log = p.base.join("git.log");
    let script = format!(
        "#!/bin/sh\n\
         echo \"$*\" >> '{log}'\n\
         if [ \"$1 $2 $3\" = 'worktree move {v}' ] && [ -e '{bin}/attack' ]; then\n\
           rm '{bin}/attack'\n\
           '{real}' -C '{main}' worktree move '{v}' '{v}-moved' &&\n\
           '{real}' -C '{main}' worktree add -q -b d '{v}' &&\n\
           mkdir -p '{v}/target' && echo theirs > '{v}/target/build'\n\
         fi\n\
         exec '{real}' \"$@\"\n",
        log = log.display(),
        v = victim.display(),
        bin = bin.display(),
        real = real,
        main = p.main().display(),
    );
    fs::write(bin.join("git"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    (
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        log,
    )
}

// Finding 5: the worktree is moved to a name no one knows, checked there, then removed.
#[test]
fn the_worktree_is_moved_aside_and_checked_again_before_git_removes_it() {
    let p = Proj::new();
    let wt = p.start("a", None);
    p.land("a", &wt);
    let (path, log) = git_wrapper(&p, &wt);
    let tmpdir = p.tmpdir.to_str().unwrap().to_owned();
    let run = |args: &[&str]| {
        p.repo
            .atelier_env(p.main(), args, &[("PATH", &path), ("TMPDIR", &tmpdir)])
    };

    // The review's probe: another worktree takes the path just before the removal.
    fs::write(p.base.join("gitbin/attack"), "").unwrap();
    let out = run(&["clean", "a", "--yes"]);
    assert_eq!(code(&out), 1, "{}", all_out(&out));
    assert!(
        all_out(&out).contains("is not the recorded worktree after the move"),
        "{}",
        all_out(&out)
    );
    // d's worktree, with its ignored file, is back at the path and whole.
    assert_eq!(git(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/d");
    assert_eq!(
        fs::read_to_string(wt.join("target/build")).unwrap(),
        "theirs\n"
    );
    let moved = PathBuf::from(format!("{}-moved", wt.display()));
    assert_eq!(git(&moved, &["symbolic-ref", "HEAD"]), "refs/heads/a");
    assert!(p.branch_exists("a"));
    assert_eq!(p.record("a").unwrap().state, BranchState::Merged);

    // Undo the probe; the clean then goes through the random name.
    git(p.main(), &["worktree", "remove", wt.to_str().unwrap()]);
    git(
        p.main(),
        &[
            "worktree",
            "move",
            moved.to_str().unwrap(),
            wt.to_str().unwrap(),
        ],
    );
    fs::write(&log, "").unwrap();
    let out = run(&["clean", "a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!wt.exists());
    let calls = fs::read_to_string(&log).unwrap();
    let prefix = format!(
        "worktree move {} {}.atelier-removing-",
        wt.display(),
        wt.display()
    );
    let mv = calls
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{calls}"));
    let aside = mv.rsplit(' ').next().unwrap();
    assert!(
        calls
            .lines()
            .any(|l| l == format!("worktree remove {aside}")),
        "{calls}"
    );
    assert!(
        !calls
            .lines()
            .any(|l| l == format!("worktree remove {}", wt.display())),
        "{calls}"
    );
    assert!(!Path::new(aside).exists());
}

// Finding 6: deletions run least destructive first, and a failure stops there.
#[test]
fn a_temp_folder_that_cannot_be_deleted_stops_the_clean_before_the_worktree_and_the_branch() {
    use std::os::unix::fs::PermissionsExt;
    let p = Proj::new();
    let wt = p.start("a", None);
    let entry = registered_folder(&p, &wt, "a");
    let ro = entry.path.join("ro");
    fs::create_dir(&ro).unwrap();
    fs::write(ro.join("f"), "f").unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();
    p.land("a", &wt);

    for _ in 0..2 {
        let out = p.clean(&["a", "--yes"]);
        assert_eq!(code(&out), 1, "{}", all_out(&out));
        assert!(
            all_out(&out).contains("Permission denied"),
            "{}",
            all_out(&out)
        );
        nothing_deleted(&p, "a", &wt);
        assert!(MergeFacts::read(&p.record("a").unwrap()).unwrap().is_some());
    }
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o700)).unwrap();
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!entry.path.exists() && !wt.exists() && !p.branch_exists("a"));
}

// Finding 7: work committed after approval and reset away.
#[test]
fn work_reset_away_after_approval_stops_the_branch_deletion_until_it_is_discarded() {
    let p = Proj::new();
    let wt = p.start("a", None);
    // An amend before approval replaces a commit: the old one is not work made after approval.
    commit(&wt, "b.txt", "b\n", "b");
    fs::write(wt.join("b.txt"), "b2\n").unwrap();
    git(&wt, &["commit", "-q", "-a", "--amend", "-m", "b2"]);
    let merged = p.land("a", &wt);
    let after = commit(&wt, "x.txt", "after\n", "after approval");
    git(&wt, &["reset", "-q", "--hard", &merged]);

    for args in [&["a"][..], &["a", "--yes"]] {
        let out = p.clean(args);
        refused(&out, "only it remembers");
        let text = all_out(&out);
        assert!(text.contains(&format!("{after} after approval")), "{text}");
        assert!(text.contains(&format!("--discard {after}")), "{text}");
        assert!(
            !text.contains(" b\n"),
            "the amended commit is not listed: {text}"
        );
    }
    nothing_deleted(&p, "a", &wt);

    // Another ref that keeps it: nothing is lost.
    git(p.main(), &["branch", "keep", &after]);
    assert_eq!(code(&p.clean(&["a"])), 0);
    git(p.main(), &["branch", "-q", "-D", "keep"]);

    // Something that is not a commit is a usage error.
    assert_eq!(code(&p.clean(&["a", "--yes", "--discard", "nope"])), 2);
    let out = p.clean(&["a", "--yes", "--discard", &after[..12]]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!wt.exists());
    assert!(!p.branch_exists("a"));
}

// Finding 8: the branch lock is shared by tmp and check, exclusive for clean.
#[test]
fn two_tmp_commands_of_one_branch_run_at_once() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let child = p
        .repo
        .atelier_cmd(
            &wt,
            &[
                "tmp",
                "--",
                "sh",
                "-c",
                "touch \"$ATELIER_TMP/started\"; while [ ! -e \"$ATELIER_TMP/stop\" ]; do sleep 0.05; done",
            ],
            &[("TMPDIR", p.tmpdir.to_str().unwrap())],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let folder = loop {
        if let Some(e) = p.folders("a").pop()
            && e.path.join("started").exists()
        {
            break e.path;
        }
        assert!(Instant::now() < deadline, "tmp command never started");
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = p.tmp(&wt, false, "true");
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert_eq!(p.folders("a").len(), 2);
    // start, which takes the lock exclusive, refuses meanwhile.
    p.land("a", &wt);
    refused(&p.clean(&["a", "--yes"]), "held by another atelier command");
    fs::write(folder.join("stop"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn check_holds_the_branch_lock_of_an_atelier_branch() {
    let p = Proj::new();
    let wt = p.start("a", None);
    let state = p.state();
    let held = HeldLock::acquire(&state, LockKey::branch("a")).unwrap();
    let out = p.run(&wt, &["check"]);
    refused(&out, "branch lock a is held by another atelier command");
    drop(held);
    // Shared with tmp: a shared holder does not stop it at the lock. Taken again with retries:
    // a child another test thread is spawning holds a copy of the released descriptor until it
    // execs.
    let deadline = Instant::now() + Duration::from_secs(5);
    let _shared = loop {
        match HeldLock::acquire_shared(&state, LockKey::branch("a")) {
            Err(e) if e.message.contains("held by another") && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            other => break other.unwrap(),
        }
    };
    let out = p.run(&wt, &["check"]);
    assert!(
        !all_out(&out).contains("branch lock a"),
        "{}",
        all_out(&out)
    );
}

// Finding 9: one temp root per user.
#[test]
fn the_temp_root_is_per_user_and_private() {
    use std::os::unix::fs::PermissionsExt;
    let p = Proj::new();
    let wt = p.start("a", None);
    // Another user's shared folder at the old place blocks nothing.
    fs::write(p.tmpdir.join("atelier"), "someone else's").unwrap();
    // A root of ours that others can enter is refused.
    let root = p.tmpdir.join(user_root());
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    refused(&p.tmp(&wt, false, "true"), "open to other users");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(code(&p.tmp(&wt, false, "true")), 0);
    let entry = p.folders("a").pop().unwrap();
    assert!(entry.path.starts_with(fs::canonicalize(&root).unwrap()));
    assert_eq!(
        fs::read_to_string(p.tmpdir.join("atelier")).unwrap(),
        "someone else's"
    );

    // TMPDIR unset: `<the system's temp dir>/atelier-<uid>`, `/tmp` on Linux (on macOS, std
    // falls back to the user's own temp dir).
    let before: Vec<PathBuf> = p.folders("a").into_iter().map(|e| e.path).collect();
    let out = p
        .repo
        .atelier_cmd(&wt, &["tmp", "--", "true"], &[])
        .env_remove("TMPDIR")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    let entry = p
        .folders("a")
        .into_iter()
        .find(|e| !before.contains(&e.path))
        .unwrap();
    let tmp_root = entry.path.ancestors().nth(3).unwrap().to_path_buf();
    assert_eq!(
        tmp_root.file_name().unwrap().to_str(),
        Some(user_root().as_str())
    );
    assert!(!tmp_root.starts_with(fs::canonicalize(&p.tmpdir).unwrap()));
    if cfg!(target_os = "linux") {
        assert_eq!(tmp_root, Path::new("/tmp").join(user_root()));
    }
    assert_eq!(fs::metadata(&tmp_root).unwrap().mode() & 0o777, 0o700);
    p.land("a", &wt);
    let out = p.clean(&["a", "--yes"]);
    assert_eq!(code(&out), 0, "{}", all_out(&out));
    assert!(!entry.path.exists());
    let _ = fs::remove_dir(entry.path.parent().unwrap().parent().unwrap());
}
