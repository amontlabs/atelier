//! `approve`, `merge` and `promote` (DESIGN §3, §2.4, §8.3; r4 §3.8 and its failure table; B8;
//! HF1), on throwaway repositories with the real binary. The project has no Rust workspace: its
//! one platform (this computer's) runs `atelier check`, whose only step fails when a file named
//! `BROKEN` is in the tree.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use atelier::ledger::{MergeLine, Promotion};
use atelier::platform::Platform;
use atelier::records::{BranchRecord, BranchState, MergeFacts};
use atelier::state::{CURRENT_FORMAT, State};
use common::*;

fn this_platform() -> String {
    Platform::current().expect("a known platform").to_string()
}

struct Proj {
    repo: Repo,
    base: PathBuf,
}

const STEP: &str =
    "[[check.steps]]\nname = \"verify\"\nrun = [\"sh\", \"-c\", \"test ! -e BROKEN\"]\n";

impl Proj {
    fn new() -> Proj {
        Self::with_settings("")
    }

    /// A project whose committed settings end with `extra`.
    fn with_settings(extra: &str) -> Proj {
        Self::with_files(&[], extra)
    }

    /// A project with these files committed beside its settings.
    fn with_files(files: &[(&str, &str)], extra: &str) -> Proj {
        let repo = Repo::new();
        for (f, c) in files {
            repo.write(f, c);
        }
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

    fn gate(&self) -> PathBuf {
        self.base.join("repo.gate")
    }

    fn wt(&self, branch: &str) -> PathBuf {
        self.base.join(format!("wt-{}", branch.replace('/', "-")))
    }

    fn run(&self, args: &[&str]) -> Output {
        self.repo.atelier_in(self.main(), args)
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

    /// Starts `branch`, commits `file` with `contents` in its worktree, approves the tip.
    fn ready(&self, branch: &str, file: &str, contents: &str) -> String {
        let wt = self.start(branch, None);
        let c = commit(&wt, file, contents, branch);
        self.approve(branch, &c);
        c
    }

    fn approve(&self, branch: &str, commit: &str) {
        let out = self.run(&["approve", branch, commit]);
        assert_eq!(code(&out), 0, "approve {branch}: {}", stderr(&out));
    }

    fn merge(&self, branches: &[&str]) -> Output {
        let mut args = vec!["merge"];
        args.extend(branches);
        self.run(&args)
    }

    fn main_tip(&self) -> String {
        git(self.main(), &["rev-parse", "main"])
    }

    fn state(&self) -> State {
        State::open_at(self.main().join(".git/atelier")).unwrap()
    }

    fn record(&self, branch: &str) -> BranchRecord {
        BranchRecord::read(&self.state(), branch).unwrap().unwrap()
    }

    fn approval(&self, branch: &str) -> Option<atelier::ledger::Approval> {
        let state = self.state();
        let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
        atelier::ledger::Approval::read(&v, branch).unwrap()
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

    fn merge_lines(&self) -> Vec<MergeLine> {
        let state = self.state();
        let v = state.versioned(CURRENT_FORMAT, &[]).unwrap();
        MergeLine::read_all(&v).unwrap()
    }

    /// Asserts the branch was recorded merged with `approve`'s worktree facts.
    fn assert_merged(&self, branch: &str, commit: &str) {
        let rec = self.record(branch);
        assert_eq!(rec.state, BranchState::Merged, "{branch}");
        let facts = MergeFacts::read(&rec).unwrap().expect("merge facts");
        assert_eq!(facts.commit, commit);
        assert_eq!(facts.worktree, fs::canonicalize(self.wt(branch)).unwrap());
        assert!(
            self.approval(branch).is_none(),
            "{branch}'s approval is used up"
        );
        assert!(
            self.merge_lines()
                .iter()
                .any(|l| l.branch == branch && l.commit == commit),
            "merges.tsv names {branch}"
        );
    }

    /// Status of the main checkout, porcelain, untracked and ignored included.
    fn main_status(&self) -> String {
        git(
            self.main(),
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored",
            ],
        )
    }
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

fn both(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

fn short(dir: &Path, c: &str) -> String {
    git(dir, &["rev-parse", "--short", c])
}

// ---------------------------------------------------------------------------------------------
// approve

#[test]
fn approve_records_the_commit_and_the_worktree_identity() {
    let p = Proj::new();
    let wt = p.start("feat", None);
    let c = commit(&wt, "a.txt", "a\n", "a");
    p.approve("feat", &c);
    let a = p.approval("feat").unwrap();
    assert_eq!(a.commit, c);
    assert_eq!(a.worktree, fs::canonicalize(&wt).unwrap());
    let meta = fs::metadata(&wt).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!((a.dev, a.ino), (meta.dev(), meta.ino()));
    assert_eq!(a.lifetime, p.record("feat").lifetime);
}

#[test]
fn approve_refuses_a_commit_that_is_not_the_tip_and_a_branch_not_in_one_worktree() {
    let p = Proj::new();
    let wt = p.start("feat", None);
    let c1 = commit(&wt, "a.txt", "a\n", "a");
    commit(&wt, "a.txt", "b\n", "b");
    let out = p.run(&["approve", "feat", &c1]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(stderr(&out).contains("approve the commit that was reviewed"));
    assert!(p.approval("feat").is_none());

    git(&wt, &["switch", "-q", "--detach"]);
    let tip = git(&wt, &["rev-parse", "HEAD"]);
    let out = p.run(&["approve", "feat", &tip]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("not checked out in any worktree"),
        "{}",
        stderr(&out)
    );

    // Only the main checkout may approve.
    git(&wt, &["switch", "-q", "feat"]);
    let out = p.repo.atelier_in(&wt, &["approve", "feat", &tip]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("only in the main checkout"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn approve_refuses_a_branch_behind_a_rewrite_of_its_lower() {
    let p = Proj::new();
    let a = p.start("a", None);
    commit(&a, "a.txt", "1\n", "a1");
    let b = p.start("b", Some("a"));
    let cb = commit(&b, "b.txt", "1\n", "b1");
    git(&a, &["commit", "-q", "--amend", "-m", "a1 rewritten"]);
    let out = p.run(&["approve", "b", &cb]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("b is behind a rewrite of a: run atelier follow"),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------------------------
// merge: green

#[test]
fn a_green_merge_moves_main_records_each_branch_and_updates_the_main_checkout() {
    let p = Proj::new();
    let ca = p.ready("disk-api", "api.txt", "api\n");
    let cb = p.ready("disk-ui", "ui/ui.txt", "ui\n");
    let old = p.main_tip();
    let out = p.merge(&["disk-api", "disk-ui"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    let m = p.main_tip();
    assert_ne!(m, old);
    assert!(
        so.contains(&format!("merge: GREEN — run {}", p.merge_lines()[0].run)),
        "{so}"
    );
    assert!(
        so.contains(&format!(
            "main {} → {}",
            short(p.main(), &old),
            short(p.main(), &m)
        )),
        "{so}"
    );
    assert!(
        so.contains(&format!("merged disk-api {}", short(p.main(), &ca))),
        "{so}"
    );
    assert!(
        so.contains(&format!("merged disk-ui {}", short(p.main(), &cb))),
        "{so}"
    );
    assert!(so.find("merge: GREEN").unwrap() < so.find("merged disk-api").unwrap());
    // Both merges are --no-ff merges of the approved commits, on top of OLD.
    let parents = git(p.main(), &["log", "--format=%P", "-2", &m]);
    assert!(
        parents.lines().all(|l| l.split(' ').count() == 2),
        "{parents}"
    );
    assert_eq!(git(p.main(), &["rev-parse", &format!("{m}^1^1")]), old);
    // The main checkout holds M, clean.
    assert_eq!(
        fs::read_to_string(p.main().join("ui/ui.txt")).unwrap(),
        "ui\n"
    );
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
    p.assert_merged("disk-api", &ca);
    p.assert_merged("disk-ui", &cb);
    let lines = p.merge_lines();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|l| l.old == old && l.m == m));
    assert!(
        lines[0].platforms[0].starts_with(&format!("{}=pass@local", this_platform())),
        "{:?}",
        lines[0].platforms
    );
    assert!(p.promotion().is_none());
    // The gate stays, detached at M, clean, kept warm.
    assert_eq!(git(&p.gate(), &["rev-parse", "HEAD"]), m);
    assert_eq!(git(&p.gate(), &["status", "--porcelain"]), "");
    // The inner check ran through the pinned atelier, nested in the gate's run.
    let inner = lines[0].platforms[0].clone();
    assert!(inner.contains('@'));
}

#[test]
fn a_rules_item_merges_alone_and_says_rules_changed() {
    let p = Proj::new();
    let r = p.start("rules-x", None);
    let cr = commit(&r, ".atelier/hooks/x.sh", "#!/bin/sh\n", "hook");
    p.approve("rules-x", &cr);
    p.ready("other", "o.txt", "o\n");
    let out = p.merge(&["other", "rules-x"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out)
            .contains("rules-x changes .atelier/ or the generated rules: merge rules-x on its own"),
        "{}",
        stderr(&out)
    );
    let out = p.merge(&["rules-x"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).trim_end().ends_with("rules changed"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_lower_branch_lands_only_in_the_batch_and_earlier() {
    let p = Proj::new();
    let a = p.start("a", None);
    let ca = commit(&a, "a.txt", "a\n", "a");
    let b = p.start("b", Some("a"));
    let cb = commit(&b, "b.txt", "b\n", "b");
    p.approve("a", &ca);
    p.approve("b", &cb);
    let out = p.merge(&["b"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out)
            .contains("b is built on a, which is not merged: merge a in the same batch, before b"),
        "{}",
        stderr(&out)
    );
    let out = p.merge(&["b", "a"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("must come earlier in the batch"),
        "{}",
        stderr(&out)
    );
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("a", &ca);
    p.assert_merged("b", &cb);
}

#[test]
fn a_branch_with_a_merge_commit_of_its_own_is_refused() {
    let p = Proj::new();
    let x = p.start("x", None);
    commit(&x, "x.txt", "x\n", "x");
    git(&x, &["switch", "-q", "-c", "side", "main"]);
    commit(&x, "s.txt", "s\n", "s");
    git(&x, &["switch", "-q", "x"]);
    git(&x, &["merge", "-q", "--no-ff", "-m", "own merge", "side"]);
    let tip = git(&x, &["rev-parse", "HEAD"]);
    let out = p.run(&["approve", "x", &tip]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("merge commit of its own"),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------------------------------------
// merge: refusals before the gate (r4 §3.8 failure table)

#[test]
fn a_branch_moved_since_approval_is_refused() {
    let p = Proj::new();
    let c = p.ready("feat", "a.txt", "a\n");
    commit(&p.wt("feat"), "a.txt", "later\n", "later");
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains(&format!(
            "feat moved since it was approved at {}",
            short(p.main(), &c)
        )),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.main_tip(), old);
    assert!(!p.gate().exists(), "nothing was started");
}

#[test]
fn an_unapproved_branch_is_refused() {
    let p = Proj::new();
    let wt = p.start("feat", None);
    commit(&wt, "a.txt", "a\n", "a");
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("feat is not approved"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_symbolic_integration_ref_or_branch_ref_is_refused() {
    let p = Proj::new();
    p.ready("feat", "a.txt", "a\n");
    // An approved branch whose ref became an alias.
    git(p.main(), &["branch", "-q", "other", "main"]);
    let feat_tip = git(p.main(), &["rev-parse", "feat"]);
    git(p.main(), &["update-ref", "-d", "refs/heads/feat"]);
    git(
        p.main(),
        &["symbolic-ref", "refs/heads/feat", "refs/heads/other"],
    );
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(stderr(&out).contains("symbolic ref"), "{}", stderr(&out));
    git(p.main(), &["symbolic-ref", "--delete", "refs/heads/feat"]);
    git(p.main(), &["update-ref", "refs/heads/feat", &feat_tip]);

    // The integration branch as an alias of another branch.
    let old = p.main_tip();
    git(p.main(), &["branch", "-q", "real", "main"]);
    git(p.main(), &["switch", "-q", "--detach"]);
    git(p.main(), &["update-ref", "-d", "refs/heads/main"]);
    git(
        p.main(),
        &["symbolic-ref", "refs/heads/main", "refs/heads/real"],
    );
    git(p.main(), &["switch", "-q", "main"]);
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert_eq!(
        git(p.main(), &["rev-parse", "real"]),
        old,
        "nothing moved through the alias"
    );
}

#[test]
fn a_gate_worktree_with_unknown_files_is_refused_and_nothing_is_deleted() {
    let p = Proj::new();
    p.ready("one", "1.txt", "1\n");
    let out = p.merge(&["one"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.ready("two", "2.txt", "2\n");
    fs::write(p.gate().join("repro.txt"), "keep me\n").unwrap();
    fs::write(p.gate().join("README"), "edited\n").unwrap();
    fs::write(p.gate().join("out.ignored"), "build output\n").unwrap();
    let old = p.main_tip();
    let out = p.merge(&["two"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    let e = stderr(&out);
    assert!(
        e.contains("the gate worktree") && e.contains("is not clean"),
        "{e}"
    );
    assert!(
        e.contains("repro.txt (untracked)") && e.contains("README (M)"),
        "{e}"
    );
    assert!(
        !e.contains("out.ignored"),
        "ignored files are not in the way: {e}"
    );
    assert_eq!(
        fs::read_to_string(p.gate().join("repro.txt")).unwrap(),
        "keep me\n"
    );
    assert_eq!(
        fs::read_to_string(p.gate().join("README")).unwrap(),
        "edited\n"
    );
    assert_eq!(p.main_tip(), old);
}

#[test]
fn a_folder_at_the_gate_path_that_is_not_its_worktree_is_refused() {
    let p = Proj::new();
    p.ready("one", "1.txt", "1\n");
    fs::create_dir_all(p.gate()).unwrap();
    fs::write(p.gate().join("mine"), "x").unwrap();
    let out = p.merge(&["one"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("is not a worktree of this repository"),
        "{}",
        stderr(&out)
    );
    assert!(p.gate().join("mine").exists());
}

#[test]
fn no_machine_for_a_platform_exits_3_unless_waived() {
    let other = if this_platform() == "linux-x86_64" {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    };
    let p = Proj::with_settings(&format!(
        "\n[platforms.{other}]\nrun = [\"sh\", \"-c\", \"true\"]\n"
    ));
    let c = p.ready("feat", "a.txt", "a\n");
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 3, "{}", both(&out));
    assert!(
        stderr(&out).contains(&format!("{other}: no machine")),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.main_tip(), old);
    assert!(!p.gate().exists(), "checked first, before the gate");
    let out = p.run(&[
        "merge",
        "feat",
        "--waive",
        other,
        "--reason",
        "owner: linux box away",
    ]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(stdout(&out).contains(&format!("{other} WAIVED (owner: linux box away)")));
    p.assert_merged("feat", &c);
    assert!(
        p.merge_lines()[0]
            .platforms
            .iter()
            .any(|x| x == &format!("{other}=waived:owner: linux box away"))
    );
}

// ---------------------------------------------------------------------------------------------
// merge: conflicts (step 3) and HF1's regenerated paths

#[test]
fn a_conflict_with_main_exits_5_naming_main_and_the_files() {
    let p = Proj::new();
    let wt = p.start("feat", None);
    let c = commit(&wt, "README", "feature\n", "feat");
    p.approve("feat", &c);
    commit(p.main(), "README", "main moved\n", "on main");
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    let e = stderr(&out);
    assert!(e.contains("conflict: feat with main\n  README"), "{e}");
    assert_eq!(p.main_tip(), old);
    assert_eq!(
        git(&p.gate(), &["rev-parse", "HEAD"]),
        old,
        "the gate is back at OLD"
    );
    assert_eq!(git(&p.gate(), &["status", "--porcelain"]), "");
    assert!(p.approval("feat").is_some());
}

#[test]
fn a_conflict_between_batch_branches_names_the_earlier_branch() {
    let p = Proj::new();
    p.ready("disk-api", "shared.txt", "api\n");
    p.ready("disk-ui", "shared.txt", "ui\n");
    let old = p.main_tip();
    let out = p.merge(&["disk-api", "disk-ui"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    assert!(
        stderr(&out)
            .contains("conflict: disk-ui with disk-api (earlier in this batch)\n  shared.txt"),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.main_tip(), old);
    assert_eq!(git(&p.gate(), &["rev-parse", "HEAD"]), old);
}

const REGEN: &str = "[[merge.after]]\nname = \"regen\"\nrun = [\"sh\", \"-c\", \"cat parts/* > gen.txt\"]\ncommit = \"Regenerate gen.txt\"\nregenerates = [\"gen.txt\"]\n";

#[test]
fn a_conflict_only_in_regenerated_paths_takes_the_incoming_side_and_the_hook_regenerates() {
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], REGEN);
    let a = p.start("a", None);
    fs::write(a.join("parts/a"), "a\n").unwrap();
    let ca = commit(&a, "gen.txt", "zero\na\n", "a");
    let b = p.start("b", None);
    fs::write(b.join("parts/b"), "b\n").unwrap();
    let cb = commit(&b, "gen.txt", "zero\nb\n", "b");
    p.approve("a", &ca);
    p.approve("b", &cb);
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).contains("conflict in gen.txt settled with b's side"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(p.main().join("gen.txt")).unwrap(),
        "zero\na\nb\n",
        "the hook regenerated it"
    );
    assert_eq!(
        git(p.main(), &["log", "-1", "--format=%s", "main"]),
        "Regenerate gen.txt"
    );
}

#[test]
fn a_conflict_also_outside_regenerated_paths_is_still_exit_5() {
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], REGEN);
    let a = p.start("a", None);
    fs::write(a.join("other.txt"), "a\n").unwrap();
    let ca = commit(&a, "gen.txt", "zero\na\n", "a");
    let b = p.start("b", None);
    fs::write(b.join("other.txt"), "b\n").unwrap();
    let cb = commit(&b, "gen.txt", "zero\nb\n", "b");
    p.approve("a", &ca);
    p.approve("b", &cb);
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    let e = stderr(&out);
    assert!(
        e.contains("conflict: b with a (earlier in this batch)\n  other.txt"),
        "{e}"
    );
    assert!(
        !e.contains("  gen.txt"),
        "only the paths that are a conflict: {e}"
    );
}

#[test]
fn regenerates_must_name_files_inside_the_project() {
    for (bad, why) in [
        ("/etc/passwd", "relative to the checkout root"),
        ("../x", "leaves the checkout"),
        ("missing.txt", "does not exist"),
    ] {
        let repo = Repo::new();
        repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\n[platforms.{}]\nrun = \"check\"\n[[merge.after]]\nname = \"g\"\nrun = [\"true\"]\nregenerates = [{bad:?}]\n",
                this_platform()
            ),
        );
        let e = atelier::settings::Settings::load(&repo.root).unwrap_err();
        assert!(e.message.contains(why), "{bad}: {}", e.message);
    }
    let repo = Repo::new();
    repo.write(
        ".atelier/atelier.toml",
        &format!(
            "atelier = \"{VERSION}\"\n[platforms.{}]\nrun = \"check\"\n[[merge.after]]\nname = \"g\"\nrun = [\"true\"]\nregenerated = [\"README\"]\n",
            this_platform()
        ),
    );
    assert!(
        atelier::settings::Settings::load(&repo.root).is_err(),
        "unknown key"
    );
}

// ---------------------------------------------------------------------------------------------
// merge: RED (step 6) and hooks (step 4)

#[test]
fn a_red_check_leaves_main_unmoved_and_the_gate_for_inspection() {
    let p = Proj::new();
    let c = p.ready("feat", "BROKEN", "x\n");
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("merge: RED — run"), "{so}");
    assert!(so.contains("verify"), "{so}");
    assert!(so.contains("main has not moved"), "{so}");
    assert!(!so.lines().any(|l| l.starts_with("merged ")), "{so}");
    assert_eq!(p.main_tip(), old);
    assert!(
        p.gate().join("BROKEN").exists(),
        "the gate holds M for inspection"
    );
    assert_eq!(p.record("feat").state, BranchState::Active);
    assert_eq!(p.approval("feat").unwrap().commit, c);
    assert!(p.merge_lines().is_empty());
}

#[test]
fn an_after_hook_change_without_commit_is_red_naming_the_hook() {
    let p = Proj::with_settings(
        "[[merge.after]]\nname = \"scribble\"\nrun = [\"sh\", \"-c\", \"echo x > stray.txt\"]\n",
    );
    p.ready("feat", "a.txt", "a\n");
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    assert!(
        stdout(&out).contains("merge.after scribble changed the tree and has no commit message"),
        "{}",
        stdout(&out)
    );
    assert_eq!(p.main_tip(), old);
}

#[test]
fn main_moved_during_the_gate_refuses_and_drops_the_promotion() {
    let flag = tempfile::tempdir().unwrap();
    let flag_file = flag.path().join("go");
    let p = Proj::with_settings(&format!(
        "[[check.steps]]\nname = \"sneak\"\nrun = [\"sh\", \"-c\", \"if [ -f {f} ]; then rm {f}; git -C {main} commit -q --allow-empty -m sneaked; fi\"]\n",
        f = flag_file.display(),
        main = "../repo"
    ));
    let c = p.ready("feat", "a.txt", "a\n");
    fs::write(&flag_file, "").unwrap();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("main moved: run the merge again"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        git(p.main(), &["log", "-1", "--format=%s", "main"]),
        "sneaked"
    );
    assert!(p.promotion().is_none(), "the record is discarded");
    assert_eq!(p.approval("feat").unwrap().commit, c, "approvals stay");
    // Run it again: green on the new main.
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
}

// ---------------------------------------------------------------------------------------------
// promotion, mode A: checks a–e (r4 §3.8 8A, B8)

#[test]
fn check_a_main_checked_out_in_another_worktree_refuses_before_the_gate() {
    let p = Proj::new();
    p.ready("feat", "a.txt", "a\n");
    let elsewhere = p.base.join("elsewhere");
    git(
        p.main(),
        &[
            "worktree",
            "add",
            "-q",
            "-f",
            elsewhere.to_str().unwrap(),
            "main",
        ],
    );
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("main is checked out in"),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.main_tip(), old);
    assert!(!p.gate().exists(), "refused before the gate");
}

#[test]
fn check_b_a_push_to_checkout_hook_refuses_before_the_gate() {
    let p = Proj::new();
    p.ready("feat", "a.txt", "a\n");
    let hook = p.main().join(".git/hooks/push-to-checkout");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    let old = p.main_tip();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("push-to-checkout hook"),
        "{}",
        stderr(&out)
    );
    assert_eq!(p.main_tip(), old);
}

/// Merges `feat` while `busy` holds the main checkout; expects exit 7 with `why` and the
/// promotion kept, then `undo` and `atelier promote` finishing it.
fn busy_then_promote(p: &Proj, busy: impl FnOnce(&Proj), why: &str, undo: impl FnOnce(&Proj)) {
    let c = p.ready("feat", "new/added.txt", "merged\n");
    let old = p.main_tip();
    busy(p);
    let before = p.main_status();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 7, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("merge: GREEN, NOT PROMOTED — run"), "{so}");
    assert!(so.contains(why), "{so}");
    assert!(
        so.contains("run `atelier promote` when it is clean"),
        "{so}"
    );
    assert_eq!(p.main_tip(), old, "main did not move");
    assert_eq!(p.main_status(), before, "the main checkout was not touched");
    let rec = p.promotion().expect("promotion kept");
    assert!(rec.push_started.is_none(), "no push was started");
    // While it is pending, merge refuses and names it.
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("a promotion is pending"),
        "{}",
        stderr(&out)
    );
    undo(p);
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(stdout(&out).contains("merged feat"), "{}", stdout(&out));
    assert_eq!(p.main_tip(), rec.m);
    assert_eq!(
        fs::read_to_string(p.main().join("new/added.txt")).unwrap(),
        "merged\n"
    );
    p.assert_merged("feat", &c);
}

#[test]
fn check_c_a_merge_in_progress_with_a_clean_index_waits_then_7() {
    let p = Proj::new();
    busy_then_promote(
        &p,
        |p| {
            let side = git(
                p.main(),
                &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "side"],
            );
            git(
                p.main(),
                &["merge", "-q", "-s", "ours", "--no-commit", "--no-ff", &side],
            );
            assert!(p.main().join(".git/MERGE_HEAD").exists());
        },
        "a merge is in progress in the main checkout",
        |p| {
            git(p.main(), &["merge", "--abort"]);
        },
    );
}

#[test]
fn check_c_a_stale_index_lock_waits_then_7() {
    let p = Proj::new();
    busy_then_promote(
        &p,
        |p| fs::write(p.main().join(".git/index.lock"), "").unwrap(),
        "index.lock exists",
        |p| fs::remove_file(p.main().join(".git/index.lock")).unwrap(),
    );
}

#[test]
fn check_d_a_tracked_change_waits_then_7() {
    let p = Proj::new();
    busy_then_promote(
        &p,
        |p| fs::write(p.main().join("README"), "owner's edit\n").unwrap(),
        "main checkout has changes: README",
        |p| {
            git(p.main(), &["checkout", "--", "README"]);
        },
    );
}

#[test]
fn check_e_an_ignored_file_at_an_added_path_waits_and_is_kept() {
    let p = Proj::with_settings("");
    p.repo.write(".gitignore", "*.ignored\nbuild/\nnew/\n");
    p.repo.commit_all("ignore new/");
    let mut kept = None;
    busy_then_promote(
        &p,
        |p| {
            fs::create_dir_all(p.main().join("new")).unwrap();
            fs::write(p.main().join("new/added.txt"), "PRECIOUS\n").unwrap();
        },
        "new/added.txt: a file is there, where the merge adds a file",
        |p| {
            kept = Some(fs::read_to_string(p.main().join("new/added.txt")).unwrap());
            fs::rename(p.main().join("new/added.txt"), p.base.join("precious")).unwrap();
        },
    );
    assert_eq!(kept.as_deref(), Some("PRECIOUS\n"), "never overwritten");
}

#[test]
fn check_e_an_ignored_link_at_an_added_folder_waits() {
    let p = Proj::with_settings("");
    p.repo.write(".gitignore", "*.ignored\nbuild/\nnew\n");
    p.repo.commit_all("ignore new");
    let target = p.base.join("link-target");
    fs::create_dir_all(&target).unwrap();
    busy_then_promote(
        &p,
        |p| std::os::unix::fs::symlink(&target, p.main().join("new")).unwrap(),
        "new/added.txt: a symbolic link is at new, where the merge needs a folder",
        |p| fs::remove_file(p.main().join("new")).unwrap(),
    );
    assert_eq!(
        fs::read_dir(&target).unwrap().count(),
        0,
        "nothing written through the link"
    );
}

#[test]
fn promote_discard_drops_the_record_and_keeps_the_approval() {
    let p = Proj::new();
    let c = p.ready("feat", "a.txt", "a\n");
    fs::write(p.main().join("README"), "busy\n").unwrap();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 7, "{}", both(&out));
    let out = p.run(&["promote", "--discard"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(p.promotion().is_none());
    assert_eq!(p.approval("feat").unwrap().commit, c);
    assert_eq!(
        fs::read_to_string(p.main().join("README")).unwrap(),
        "busy\n"
    );
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("no promotion is pending"));
}

// ---------------------------------------------------------------------------------------------
// The three questions after a crash, and interrupted promotions (B8)

/// A pending promotion (exit 7 on a tracked change, then the change undone).
fn pending(p: &Proj) -> (String, Promotion) {
    let c = p.ready("feat", "new/added.txt", &"m content\n".repeat(50));
    fs::write(p.main().join("README"), "busy\n").unwrap();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 7, "{}", both(&out));
    git(p.main(), &["checkout", "--", "README"]);
    (c, p.promotion().unwrap())
}

#[test]
fn after_a_crash_m_in_main_means_the_promotion_happened_and_is_recorded() {
    let p = Proj::new();
    let (c, rec) = pending(&p);
    // The push ran to its end; atelier died before writing the records.
    let mut started = rec.clone();
    started.push_started = Some("2026-11-13T10:00:00Z".into());
    p.write_promotion(&started);
    git(
        p.main(),
        &[
            "push",
            "-q",
            "--receive-pack=git -c receive.denyCurrentBranch=updateInstead receive-pack",
            ".",
            &format!("{}:refs/heads/main", rec.m),
        ],
    );
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(stdout(&out).contains("merged feat"), "{}", stdout(&out));
    p.assert_merged("feat", &c);
    assert!(p.promotion().is_none());
    // A later merge finds nothing pending.
    p.ready("next", "n.txt", "n\n");
    assert_eq!(code(&p.merge(&["next"])), 0);
}

#[test]
fn after_a_crash_main_elsewhere_discards_the_record() {
    let p = Proj::new();
    let (c, _) = pending(&p);
    commit(p.main(), "x.txt", "x\n", "direct commit");
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("main moved: run the merge again"),
        "{}",
        stderr(&out)
    );
    assert!(p.promotion().is_none());
    assert_eq!(p.approval("feat").unwrap().commit, c);
}

#[test]
fn after_a_crash_main_at_old_is_pending_and_a_merge_found_record_is_settled_first() {
    let p = Proj::new();
    let (c, rec) = pending(&p);
    // Merge finds a happened promotion and records it before its own gate.
    git(p.main(), &["merge", "-q", "--ff-only", &rec.m]);
    p.ready("next", "n.txt", "n\n");
    let out = p.merge(&["next"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("an earlier promotion of"), "{so}");
    assert!(
        so.contains("merged feat") && so.contains("merged next"),
        "{so}"
    );
    p.assert_merged("feat", &c);
}

/// Marks the pending promotion's push as started and not ended, its group (long gone)
/// recorded: git ran.
fn mark_push_started(p: &Proj, rec: &Promotion) {
    let mut r = rec.clone();
    r.push_started = Some("2026-11-13T10:00:00Z".into());
    r.push_pgid = Some(999_999);
    r.push_ended = None;
    p.write_promotion(&r);
}

/// Every file's bytes under the main checkout, `.git` aside.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let path = e.path();
            if path.file_name().unwrap() == ".git" {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                out.push((path.clone(), fs::read(&path).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn an_interrupted_promotion_after_receive_pack_was_killed_is_reported_and_nothing_reset() {
    // B8: receive-pack killed: ref at OLD, files and index at M.
    let p = Proj::new();
    let (_, rec) = pending(&p);
    mark_push_started(&p, &rec);
    git(p.main(), &["read-tree", "-u", "-m", &rec.m]);
    let status = p.main_status();
    let files = snapshot(p.main());
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    let e = stderr(&out);
    assert!(
        e.contains("a promotion was interrupted at 2026-11-13T10:00:00Z"),
        "{e}"
    );
    assert!(
        e.contains("new files that are untracked, and possibly an empty file"),
        "{e}"
    );
    assert!(e.contains("paths holding something else: none"), "{e}");
    assert!(e.contains(&format!("git reset --hard {}", rec.m)), "{e}");
    assert_eq!(p.main_status(), status, "nothing reset");
    assert_eq!(snapshot(p.main()), files);
    assert_eq!(p.main_tip(), rec.old);
    // merge refuses the same way.
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("a promotion was interrupted"));
    // The route the message names: reset to M, then promote records it.
    git(p.main(), &["reset", "-q", "--hard", &rec.m]);
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(stdout(&out).contains("merged feat"));
}

#[test]
fn an_interrupted_promotion_with_a_partial_checkout_lists_the_empty_file() {
    // B8: the process group killed while read-tree wrote files: ref and index at OLD, a file M
    // deletes already deleted, new files written untracked, one of them empty.
    let p = Proj::new();
    p.repo.write("gone.txt", "old\n");
    p.repo.commit_all("gone.txt");
    let wt = p.start("feat", None);
    fs::remove_file(wt.join("gone.txt")).unwrap();
    fs::create_dir_all(wt.join("new")).unwrap();
    fs::write(wt.join("new/one.txt"), "one\n").unwrap();
    fs::write(wt.join("new/two.txt"), "two two two\n").unwrap();
    fs::write(wt.join("README"), "changed by feat\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "feat"]);
    let c = git(&wt, &["rev-parse", "HEAD"]);
    p.approve("feat", &c);
    fs::write(p.main().join("README"), "busy\n").unwrap();
    assert_eq!(code(&p.merge(&["feat"])), 7);
    git(p.main(), &["checkout", "--", "README"]);
    let rec = p.promotion().unwrap();
    mark_push_started(&p, &rec);
    fs::remove_file(p.main().join("gone.txt")).unwrap();
    fs::create_dir_all(p.main().join("new")).unwrap();
    fs::write(p.main().join("new/one.txt"), "one\n").unwrap();
    fs::write(p.main().join("new/two.txt"), "").unwrap();
    let files = snapshot(p.main());
    let status = p.main_status();
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    let e = stderr(&out);
    assert!(
        e.contains("content (not yet written): 1"),
        "README is still OLD's: {e}"
    );
    assert!(
        e.contains("content (written): 2"),
        "gone.txt absent and one.txt: {e}"
    );
    assert!(e.contains("paths holding something else (1;"), "{e}");
    assert!(e.contains("  new/two.txt (0 bytes)"), "{e}");
    let file = e
        .lines()
        .find_map(|l| l.strip_prefix("every path, classified: "))
        .expect("classification file");
    let tsv = fs::read_to_string(file).unwrap();
    assert!(tsv.contains("new/one.txt\tm\t"), "{tsv}");
    assert!(tsv.contains("gone.txt\tm\tabsent"), "{tsv}");
    assert!(tsv.contains("README\told\t"), "{tsv}");
    assert!(tsv.contains("new/two.txt\tother\t0 bytes"), "{tsv}");
    assert_eq!(snapshot(p.main()), files, "nothing reset or removed");
    assert_eq!(p.main_status(), status);
    assert_eq!(p.main_tip(), rec.old);
}

#[test]
fn a_push_that_left_nothing_is_pending_and_promote_repeats_it() {
    // B8: the push killed at 0 ms: nothing changed. Not an interruption: the push is repeated.
    let p = Proj::new();
    let (c, rec) = pending(&p);
    mark_push_started(&p, &rec);
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
}

/// A `git` for `GIT_EXEC_PATH` that sleeps before `read-tree` (the step of the push that writes
/// the files), writing its pid first. It keeps `GIT_EXEC_PATH`, so the receiving side's own
/// git commands come through it too.
fn slow_read_tree(dir: &Path, pidfile: &Path) -> PathBuf {
    let real = git(dir, &["--exec-path"]);
    let shim = dir.parent().unwrap().join("shim");
    fs::create_dir_all(&shim).unwrap();
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do if [ \"$a\" = read-tree ]; then echo $$ > {pid}; sleep 2; break; fi; done\nexec {real}/git \"$@\"\n",
        pid = pidfile.display()
    );
    let git_shim = shim.join("git");
    fs::write(&git_shim, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&git_shim, fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn atelier_killed_during_the_push_the_push_finishes_and_promote_records_it() {
    let p = Proj::new();
    let (c, rec) = pending(&p);
    let pidfile = p.base.join("read-tree.pid");
    let shim = slow_read_tree(p.main(), &pidfile);
    let mut child = p
        .repo
        .atelier_cmd(
            p.main(),
            &["promote"],
            &[("GIT_EXEC_PATH", shim.to_str().unwrap())],
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("read-tree", || {
        pidfile.exists() && !fs::read_to_string(&pidfile).unwrap().is_empty()
    });
    // SIGKILL: atelier cannot clean up anything; the push is in its own group.
    child.kill().unwrap();
    child.wait().unwrap();
    let r = p.promotion().unwrap();
    assert!(r.push_started.is_some() && r.push_ended.is_none());
    wait_for("main at M", || p.main_tip() == rec.m);
    let pgid = r.push_pgid.expect("the push's group is recorded");
    wait_for(
        "the push's group to end",
        || unsafe { libc::kill(-pgid, 0) } != 0,
    );
    assert_eq!(
        git(p.main(), &["status", "--porcelain"]),
        "",
        "files at M, clean"
    );
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).contains("happened; recording it"),
        "{}",
        stdout(&out)
    );
    p.assert_merged("feat", &c);
}

// ---------------------------------------------------------------------------------------------
// Bisection (DESIGN §8.3)

#[test]
fn a_red_batch_names_the_culprit_and_the_green_prefix_lands_after_one_full_gate() {
    let p = Proj::new();
    let ca = p.ready("a", "a.txt", "a\n");
    p.ready("b", "BROKEN", "b\n");
    let cc = p.ready("c", "c.txt", "c\n");
    let out = p.merge(&["a", "b", "c"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("merge: RED"), "{so}");
    assert!(so.contains("prefix a, b: fails"), "{so}");
    assert!(so.contains("prefix a: none of the failures appear"), "{so}");
    assert!(
        so.contains(&format!(
            "merge: b breaks: on prefix a, b: {}: step verify (check run ",
            this_platform()
        )),
        "{so}"
    );
    assert!(so.contains("merged a"), "{so}");
    assert!(so.contains("still approved, not merged: b, c"), "{so}");
    p.assert_merged("a", &ca);
    assert_eq!(p.approval("c").unwrap().commit, cc);
    assert_eq!(p.record("b").state, BranchState::Active);
    // Prefixes re-ran only the failed step: check-only runs exist.
    let runs = fs::read_dir(p.main().join(".git/atelier/runs")).unwrap();
    let only = runs
        .flatten()
        .filter(|e| {
            fs::read_to_string(e.path().join("run.toml"))
                .is_ok_and(|t| t.contains("kind = \"check-only\""))
        })
        .count();
    assert_eq!(only, 2, "one narrow re-run per prefix tried");
}

#[test]
fn a_failure_only_with_the_whole_batch_says_whole_batch_or_flaky() {
    let p = Proj::new();
    p.ready("a", "a.txt", "a\n");
    p.ready("b", "b.txt", "b\n");
    p.ready("c", "BROKEN", "c\n");
    let out = p.merge(&["a", "b", "c"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(
        so.contains("fails only with the whole batch (c), or is flaky"),
        "{so}"
    );
    assert!(so.contains("merged a") && so.contains("merged b"), "{so}");
}

#[test]
fn a_tree_already_judged_is_not_run_again() {
    let p = Proj::new();
    p.ready("a", "BROKEN", "a\n");
    let e = p.start("e", None);
    git(&e, &["commit", "-q", "--allow-empty", "-m", "nothing"]);
    let ce = git(&e, &["rev-parse", "HEAD"]);
    p.approve("e", &ce);
    let old = p.main_tip();
    let out = p.merge(&["a", "e"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(
        so.contains("prefix a: the same tree as the failed batch, already judged: fails"),
        "{so}"
    );
    assert!(so.contains("merge: a breaks"), "{so}");
    assert!(so.contains("nothing lands"), "{so}");
    assert_eq!(p.main_tip(), old);
    let only = fs::read_dir(p.main().join(".git/atelier/runs"))
        .unwrap()
        .flatten()
        .filter(|e| {
            fs::read_to_string(e.path().join("run.toml"))
                .is_ok_and(|t| t.contains("kind = \"check-only\""))
        })
        .count();
    assert_eq!(only, 0, "no tree is tested twice");
}

// ---------------------------------------------------------------------------------------------
// Bisection on a Rust workspace: the failed tests re-run by name (`check --only`)

fn real_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

fn rust_env(cmd: &mut std::process::Command, plain: &Path) {
    let mut dirs = vec![plain.to_path_buf()];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    cmd.env("PATH", std::env::join_paths(dirs).unwrap())
        .env(
            "RUSTUP_HOME",
            std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| real_home().join(".rustup")),
        )
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| real_home().join(".cargo")),
        )
        .env_remove("ATELIER_RUNS")
        .env_remove("MBX_DISABLE")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("NEXTEST_RETRIES")
        .env_remove("NEXTEST_FLAKY_RESULT");
}

#[test]
fn bisection_reruns_the_failed_tests_by_name_on_a_rust_workspace() {
    let repo = Repo::new();
    let plain = repo.tmp.path().join("plainbin");
    fs::create_dir_all(&plain).unwrap();
    let proxy = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| real_home().join(".cargo"))
        .join("bin/cargo");
    if proxy.is_file() {
        std::os::unix::fs::symlink(&proxy, plain.join("cargo")).unwrap();
    }
    let mut v = std::process::Command::new("cargo");
    v.args(["nextest", "--version"]).current_dir(&repo.root);
    rust_env(&mut v, &plain);
    let v = v.output().unwrap();
    let nextest = stdout(&v)
        .split_whitespace()
        .nth(1)
        .expect("cargo-nextest is installed")
        .to_owned();
    let channel = {
        let text =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("rust-toolchain.toml"))
                .unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        t["toolchain"]["channel"].as_str().unwrap().to_owned()
    };
    repo.install_self(VERSION);
    repo.write(".gitignore", "target\n");
    repo.write(
        "rust-toolchain.toml",
        &format!("[toolchain]\nchannel = \"{channel}\"\n"),
    );
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\"]\nresolver = \"3\"\n",
    );
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write("a/src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n");
    repo.write(
        ".atelier/atelier.toml",
        &format!(
            "atelier = \"{VERSION}\"\nworkspaces = [\".\"]\npromotion_wait_seconds = 1\n\n[rust]\nnextest = \"{nextest}\"\n\n[platforms.{}]\nrun = \"check\"\n",
            this_platform()
        ),
    );
    repo.write_rules();
    let mut lock = std::process::Command::new("cargo");
    lock.arg("generate-lockfile").current_dir(&repo.root);
    rust_env(&mut lock, &plain);
    assert!(lock.output().unwrap().status.success());
    repo.commit_all("workspace");
    let base = repo.root.parent().unwrap().to_path_buf();
    let p = Proj { repo, base };
    let run = |args: &[&str]| {
        let mut c = p.repo.atelier_cmd(p.main(), args, &[]);
        rust_env(&mut c, &plain);
        c.output().unwrap()
    };
    let test_file = |name: &str, body: &str| format!("#[test]\nfn {name}() {{\n    {body}\n}}\n");
    for (b, file, body) in [
        (
            "a",
            "a/tests/ta.rs",
            test_file("passes_a", "assert_eq!(a::one(), 1);"),
        ),
        (
            "b",
            "a/tests/tb.rs",
            test_file("breaks_b", "assert_eq!(a::one(), 2);"),
        ),
        (
            "c",
            "a/tests/tc.rs",
            test_file("passes_c", "assert_eq!(a::one(), 1);"),
        ),
    ] {
        let wt = p.wt(b);
        let out = run(&["start", b, "--path", wt.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        let c = commit(&wt, file, &body, b);
        let out = run(&["approve", b, &c]);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
    }
    let out = run(&["merge", "a", "b", "c"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("prefix a, b: fails"), "{so}");
    assert!(so.contains("prefix a: none of the failures appear"), "{so}");
    assert!(
        so.contains(&format!(
            "merge: b breaks: on prefix a, b: {}: a::tb$breaks_b (check run ",
            this_platform()
        )),
        "{so}"
    );
    assert!(so.contains("merged a"), "{so}");
    assert_eq!(p.record("a").state, BranchState::Merged);
    assert_eq!(p.record("b").state, BranchState::Active);
}

// ---------------------------------------------------------------------------------------------
// 3.4 review, round 1: each probe of atelier-3.4-review.md, and the owner's decisions

fn other_platform() -> &'static str {
    if this_platform() == "linux-x86_64" {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    }
}

/// Writes the settings in a branch's worktree, regenerates the rules there and commits.
fn commit_settings(p: &Proj, wt: &Path, text: &str, msg: &str) -> String {
    fs::write(wt.join(".atelier/atelier.toml"), text).unwrap();
    let out = p.repo.atelier_in(wt, &["rules"]);
    assert_eq!(code(&out), 0, "rules: {}", both(&out));
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-q", "-m", msg]);
    git(wt, &["rev-parse", "HEAD"])
}

fn settings_text(dir: &Path) -> String {
    fs::read_to_string(dir.join(".atelier/atelier.toml")).unwrap()
}

/// The ids of the runs of `kind` in the main checkout's state.
fn runs_of_kind(p: &Proj, kind: &str) -> Vec<String> {
    let mut out = Vec::new();
    for e in fs::read_dir(p.main().join(".git/atelier/runs"))
        .unwrap()
        .flatten()
    {
        if fs::read_to_string(e.path().join("run.toml"))
            .is_ok_and(|t| t.contains(&format!("kind = \"{kind}\"")))
        {
            out.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    out
}

#[test]
fn r1_a_merge_driver_other_than_gits_own_for_a_path_both_sides_changed_is_refused() {
    let p = Proj::with_files(&[("x.txt", "base\n")], "");
    let attributes = p.main().join(".git/info/attributes");
    fs::create_dir_all(attributes.parent().unwrap()).unwrap();
    fs::write(&attributes, "x.txt merge=keepmine\n").unwrap();
    git(p.main(), &["config", "merge.keepmine.driver", "true"]);
    p.ready("a", "x.txt", "from a\n");
    p.ready("b", "x.txt", "from b\n");
    let old = p.main_tip();
    // The probe: the driver kept a's side and dropped b's without a word.
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    let e = stderr(&out);
    assert!(
        e.contains("merging b: paths changed on both sides would be merged by a merge driver other than git's own"),
        "{e}"
    );
    assert!(e.contains("x.txt: merge driver \"keepmine\""), "{e}");
    assert_eq!(p.main_tip(), old);

    // merge.default naming it, for a path with no merge attribute.
    fs::write(&attributes, "").unwrap();
    git(p.main(), &["config", "merge.default", "keepmine"]);
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("x.txt: merge driver \"keepmine\""),
        "{}",
        both(&out)
    );
    git(p.main(), &["config", "--unset", "merge.default"]);

    // A built-in name the configuration redefines.
    fs::write(&attributes, "x.txt merge=union\n").unwrap();
    git(p.main(), &["config", "merge.union.driver", "true"]);
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("which the git configuration redefines (merge.union.driver)"),
        "{}",
        both(&out)
    );
    git(p.main(), &["config", "--unset", "merge.union.driver"]);

    // A global attributes file is not read by the gate: git's own merge conflicts.
    fs::write(&attributes, "").unwrap();
    let global = p.base.join("global-attributes");
    fs::write(&global, "* merge=keepmine\n").unwrap();
    git(
        p.main(),
        &["config", "core.attributesFile", global.to_str().unwrap()],
    );
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    assert!(
        stderr(&out).contains("conflict: b with a"),
        "{}",
        both(&out)
    );
    assert_eq!(p.main_tip(), old);

    // git's own union merge is allowed.
    fs::write(&attributes, "x.txt merge=union\n").unwrap();
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let x = fs::read_to_string(p.main().join("x.txt")).unwrap();
    assert!(x.contains("from a") && x.contains("from b"), "{x}");
}

#[test]
fn r2_a_rules_item_never_has_a_conflict_settled() {
    // The probe: x removes the regen hook and commits a hand-edited gen.txt while a lands
    // with gen.txt regenerated; the conflict was settled with x's side and no hook ran.
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], REGEN);
    let x = p.start("x", None);
    fs::write(x.join("gen.txt"), "zero\nX-HAND\n").unwrap();
    let without = settings_text(&x).replace(REGEN, "");
    let cx = commit_settings(&p, &x, &without, "no regen hook, hand edit");
    p.approve("x", &cx);
    let a = p.start("a", None);
    fs::write(a.join("parts/a"), "a\n").unwrap();
    let ca = commit(&a, "gen.txt", "zero\na\n", "a");
    p.approve("a", &ca);
    let out = p.merge(&["a"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let old = p.main_tip();
    let out = p.merge(&["x"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    assert!(
        stderr(&out).contains("conflict: x with main\n  gen.txt"),
        "{}",
        both(&out)
    );
    assert!(
        !stdout(&out).contains("settled with x's side"),
        "{}",
        both(&out)
    );
    assert_eq!(p.main_tip(), old);
}

#[test]
fn r2_regenerates_comes_from_the_committed_settings_not_the_main_checkouts_file() {
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], "");
    let a = p.start("a", None);
    fs::write(a.join("parts/a"), "a\n").unwrap();
    let ca = commit(&a, "gen.txt", "zero\na\n", "a");
    let b = p.start("b", None);
    fs::write(b.join("parts/b"), "b\n").unwrap();
    let cb = commit(&b, "gen.txt", "zero\nb\n", "b");
    p.approve("a", &ca);
    p.approve("b", &cb);
    // An uncommitted edit in the main checkout names gen.txt.
    let settings = p.main().join(".atelier/atelier.toml");
    let mut text = fs::read_to_string(&settings).unwrap();
    text.push_str(REGEN);
    fs::write(&settings, text).unwrap();
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 5, "{}", both(&out));
    assert!(
        stderr(&out).contains("conflict: b with a (earlier in this batch)\n  gen.txt"),
        "{}",
        both(&out)
    );
}

#[test]
fn r2_a_regenerated_path_its_hook_does_not_rewrite_is_red_and_nobody_is_blamed() {
    // The hook names gen.txt and commits, but never writes it (a generator that skips work):
    // the incoming side would land as it is.
    let lazy = "[[merge.after]]\nname = \"regen\"\nrun = [\"true\"]\ncommit = \"Regenerate gen.txt\"\nregenerates = [\"gen.txt\"]\n";
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], lazy);
    let a = p.start("a", None);
    fs::write(a.join("parts/a"), "a\n").unwrap();
    let ca = commit(&a, "gen.txt", "zero\na\n", "a");
    let b = p.start("b", None);
    fs::write(b.join("parts/b"), "b\n").unwrap();
    let cb = commit(&b, "gen.txt", "zero\nb\n", "b");
    p.approve("a", &ca);
    p.approve("b", &cb);
    let old = p.main_tip();
    let out = p.merge(&["a", "b"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(
        so.contains("conflict in gen.txt settled with b's side"),
        "{so}"
    );
    assert!(
        so.contains("merge.after regen did not regenerate gen.txt as merged"),
        "{so}"
    );
    // Review 6: a RED that is no platform's failure is not searched.
    assert!(
        so.contains("merge: no search: the RED is not a platform's failure"),
        "{so}"
    );
    assert!(so.contains("no branch is blamed"), "{so}");
    assert!(!so.contains("breaks"), "{so}");
    assert_eq!(p.main_tip(), old);
    assert!(p.approval("a").is_some() && p.approval("b").is_some());
    // What atelier removed to run the hook again is back: the gate is clean.
    assert_eq!(git(&p.gate(), &["status", "--porcelain"]), "");
}

#[test]
fn r3_the_promotion_push_runs_no_hook_and_no_signing() {
    let p = Proj::new();
    let c = p.ready("feat", "a.txt", "a\n");
    let hooks = p.main().join(".git/hooks");
    fs::create_dir_all(&hooks).unwrap();
    let write_hook = |name: &str, body: &str| {
        let f = hooks.join(name);
        fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
    };
    // A pre-push hook that edits a tracked file and fails; receive hooks that refuse; a
    // reference-transaction hook that rejects main.
    write_hook("pre-push", "echo PRE-PUSH-RAN >> README\nexit 1");
    write_hook("pre-receive", "exit 1");
    write_hook("update", "exit 1");
    write_hook(
        "reference-transaction",
        "[ \"$1\" = prepared ] || exit 0\nif grep -q ' refs/heads/main$'; then exit 1; fi\nexit 0",
    );
    git(p.main(), &["config", "push.gpgSign", "true"]);
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
    assert_eq!(
        fs::read_to_string(p.main().join("README")).unwrap(),
        "hello\n"
    );
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
}

#[test]
fn r3_the_empty_hooks_folder_must_stay_empty() {
    let p = Proj::new();
    let c = p.ready("feat", "a.txt", "a\n");
    let empty = p.main().join(".git/atelier/empty-hooks");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("pre-receive"), "#!/bin/sh\nexit 1\n").unwrap();
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("must stay empty") && stderr(&out).contains("pre-receive"),
        "{}",
        both(&out)
    );
    let r = p.promotion().expect("the record is kept");
    assert!(r.push_started.is_none(), "no push started");
    fs::remove_file(empty.join("pre-receive")).unwrap();
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
}

/// A branch whose commit replaces OLD's tracked paths as `change` does, merged alone; returns
/// the merge's output.
fn replace_and_merge(p: &Proj, change: impl FnOnce(&Path)) -> (Output, String) {
    let wt = p.start("feat", None);
    change(&wt);
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "replace"]);
    let c = git(&wt, &["rev-parse", "HEAD"]);
    p.approve("feat", &c);
    (p.merge(&["feat"]), c)
}

#[test]
fn r4_a_file_that_becomes_a_folder_is_promoted() {
    let p = Proj::with_files(&[("foo", "file\n")], "");
    let (out, c) = replace_and_merge(&p, |wt| {
        fs::remove_file(wt.join("foo")).unwrap();
        fs::create_dir_all(wt.join("foo")).unwrap();
        fs::write(wt.join("foo/bar"), "bar\n").unwrap();
    });
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
    assert_eq!(
        fs::read_to_string(p.main().join("foo/bar")).unwrap(),
        "bar\n"
    );
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
}

#[test]
fn r4_a_folder_that_becomes_a_file_is_promoted() {
    let p = Proj::with_files(&[("d/x", "x\n"), ("d/y/z", "z\n")], "");
    let (out, c) = replace_and_merge(&p, |wt| {
        fs::remove_dir_all(wt.join("d")).unwrap();
        fs::write(wt.join("d"), "now a file\n").unwrap();
    });
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
    assert_eq!(
        fs::read_to_string(p.main().join("d")).unwrap(),
        "now a file\n"
    );
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
}

#[test]
fn r4_a_folder_that_becomes_a_file_waits_while_it_holds_an_ignored_file() {
    let p = Proj::with_files(&[("d/x", "x\n")], "");
    fs::write(p.main().join("d/keep.ignored"), "mine\n").unwrap();
    let (out, _) = replace_and_merge(&p, |wt| {
        fs::remove_dir_all(wt.join("d")).unwrap();
        fs::write(wt.join("d"), "now a file\n").unwrap();
    });
    assert_eq!(code(&out), 7, "{}", both(&out));
    assert!(
        stdout(&out)
            .contains("d: a folder is there, where the merge adds a file, holding d/keep.ignored"),
        "{}",
        both(&out)
    );
    assert_eq!(
        fs::read_to_string(p.main().join("d/keep.ignored")).unwrap(),
        "mine\n",
        "kept"
    );
}

#[test]
fn r4_a_case_only_rename_is_promoted() {
    let p = Proj::with_files(&[("Readme.md", "r\n")], "");
    let fold = fs::symlink_metadata(p.main().join(".GIT")).is_ok();
    if !fold {
        // On a volume that tells case apart, the two names are two files and nothing obstructs.
        eprintln!("this volume tells case apart: the rename is two paths");
    }
    let (out, c) = replace_and_merge(&p, |wt| {
        git(wt, &["mv", "Readme.md", "README.md"]);
    });
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
    let names: Vec<String> = fs::read_dir(p.main())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "README.md"), "{names:?}");
    assert!(!names.iter().any(|n| n == "Readme.md"), "{names:?}");
    assert_eq!(git(p.main(), &["status", "--porcelain"]), "");
}

#[test]
fn r5_a_prefix_that_leaves_the_gate_unclean_stops_the_search() {
    // The hook writes stray.txt only on the prefix a, b.
    let p = Proj::with_settings(
        "[[merge.after]]\nname = \"scribble\"\nrun = [\"sh\", \"-c\", \"if [ -e b.txt ] && [ ! -e BROKEN ]; then echo x > stray.txt; fi\"]\n",
    );
    p.ready("a", "a.txt", "a\n");
    p.ready("b", "b.txt", "b\n");
    p.ready("c", "BROKEN", "c\n");
    let old = p.main_tip();
    let out = p.merge(&["a", "b", "c"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains("merge: RED"), "{so}");
    assert!(
        so.contains("the search stopped (prefix a, b: the gate is not clean after its merge.after hooks: stray.txt (untracked)"),
        "{so}"
    );
    assert!(so.contains("the RED verdict stands"), "{so}");
    assert!(!so.contains("prefix a:"), "no shorter prefix: {so}");
    assert!(!so.contains(" breaks: "), "nobody blamed: {so}");
    assert_eq!(p.main_tip(), old);
    assert!(
        p.gate().join("stray.txt").is_file(),
        "atelier deletes nothing"
    );
}

#[test]
fn r7_no_recorded_push_group_means_git_never_ran() {
    let p = Proj::new();
    let (c, rec) = pending(&p);
    let mut r = rec.clone();
    r.push_started = Some("2026-11-13T10:00:00Z".into());
    r.push_pgid = None;
    r.push_ended = None;
    p.write_promotion(&r);
    // The owner's edit, which once read as an interrupted promotion.
    fs::write(p.main().join("README"), "owner\n").unwrap();
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 7, "{}", both(&out));
    assert!(
        !stderr(&out).contains("interrupted"),
        "not an interruption: {}",
        both(&out)
    );
    assert!(
        stdout(&out).contains("main checkout has changes: README"),
        "{}",
        both(&out)
    );
    assert_eq!(
        fs::read_to_string(p.main().join("README")).unwrap(),
        "owner\n"
    );
    git(p.main(), &["checkout", "--", "README"]);
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
}

#[test]
fn r7_a_clean_refusal_by_git_ends_the_push_even_when_the_checkout_changed() {
    let p = Proj::new();
    let (c, _) = pending(&p);
    // A `git` whose first `read-tree` (the push writing the files) edits a tracked file and
    // fails: git refuses the ref, and the checkout changed meanwhile.
    let real = git(p.main(), &["--exec-path"]);
    let shim = p.base.join("shim");
    fs::create_dir_all(&shim).unwrap();
    let mark = p.base.join("refused-once");
    let readme = p.main().join("README");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do if [ \"$a\" = read-tree ] && [ ! -e {mark} ]; then touch {mark}; echo owner > {readme}; exit 1; fi; done\nexec {real}/git \"$@\"\n",
        mark = mark.display(),
        readme = readme.display()
    );
    fs::write(shim.join("git"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(shim.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let out = p.repo.atelier_env(
        p.main(),
        &["promote"],
        &[("GIT_EXEC_PATH", shim.to_str().unwrap())],
    );
    assert_eq!(code(&out), 7, "{}", both(&out));
    assert!(mark.exists(), "git refused once");
    assert!(
        !stderr(&out).contains("interrupted"),
        "a refusal is not an interruption: {}",
        both(&out)
    );
    let r = p.promotion().unwrap();
    assert!(r.push_ended.is_some(), "the push ended: {r:?}");
    git(p.main(), &["checkout", "--", "README"]);
    let out = p.run(&["promote"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
}

#[test]
fn r8_a_branch_that_moved_during_the_gate_stays_active_and_says_so() {
    // The hook, once, adds a commit to feat in its worktree while the gate runs.
    let p = Proj::with_settings(
        "[[merge.after]]\nname = \"meanwhile\"\nrun = [\"sh\", \"-c\", \"[ -e ../moved ] && exit 0; touch ../moved; echo later > ../wt-feat/later.txt; git -C ../wt-feat add later.txt; git -C ../wt-feat -c core.hooksPath=/dev/null commit -q -m later\"]\n",
    );
    let c = p.ready("feat", "a.txt", "a\n");
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    let tip = git(&p.wt("feat"), &["rev-parse", "HEAD"]);
    assert_ne!(tip, c);
    assert!(
        so.contains(&format!("merged feat {}", short(p.main(), &c))),
        "{so}"
    );
    assert!(
        so.contains(&format!(
            "feat moved to {} after its approval at {}: its later commits are not merged; it stays active",
            short(p.main(), &tip),
            short(p.main(), &c)
        )),
        "{so}"
    );
    let rec = p.record("feat");
    assert_eq!(rec.state, BranchState::Active);
    assert!(
        MergeFacts::read(&rec).unwrap().is_none(),
        "clean trusts nothing"
    );
    assert!(
        p.merge_lines()
            .iter()
            .any(|l| l.branch == "feat" && l.commit == c)
    );
    assert!(p.approval("feat").is_none(), "the approval of {c} is used");
    assert!(git(p.main(), &["merge-base", "--is-ancestor", &c, "main"]).is_empty());

    // The branch stays usable: follow, approve, merge.
    let out = p.repo.atelier_in(&p.wt("feat"), &["follow"]);
    assert_eq!(code(&out), 0, "follow: {}", both(&out));
    let t2 = git(&p.wt("feat"), &["rev-parse", "HEAD"]);
    p.approve("feat", &t2);
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &t2);
    assert!(p.main().join("later.txt").is_file());
}

#[test]
fn r9_the_inner_check_is_the_run_the_gate_named_not_a_nested_one() {
    // A check.prepare hook runs atelier itself, under a run id that sorts first.
    let bin = atelier_bin();
    let p = Proj::with_settings(&format!(
        "[[check.prepare]]\nname = \"nested\"\nrun = [\"sh\", \"-c\", \"cd ../repo && ATELIER_RUN_ID=20000101T0000Z-0000000000000000 {} check --compile-only >/dev/null 2>&1; true\"]\n",
        bin.display()
    ));
    p.ready("feat", "a.txt", "a\n");
    let out = p.merge(&["feat"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    assert!(
        p.main()
            .join(".git/atelier/runs/20000101T0000Z-0000000000000000")
            .is_dir(),
        "the nested run took the id: {so}"
    );
    let line = so
        .lines()
        .find(|l| l.contains("PASS on this computer (check run "))
        .expect("the platform line");
    let id = line
        .rsplit("check run ")
        .next()
        .unwrap()
        .trim_end_matches(')');
    assert_ne!(id, "20000101T0000Z-0000000000000000", "{so}");
    assert!(runs_of_kind(&p, "check").contains(&id.to_owned()), "{so}");
}

#[test]
fn r10_a_platform_the_merged_settings_add_is_checked_for_a_machine_before_the_check() {
    let other = other_platform();
    let p = Proj::new();
    let x = p.start("x", None);
    let mut text = settings_text(&x);
    text.push_str(&format!(
        "\n[platforms.{other}]\nrun = [\"sh\", \"-c\", \"true\"]\n"
    ));
    let cx = commit_settings(&p, &x, &text, "another platform");
    p.approve("x", &cx);
    let old = p.main_tip();
    let out = p.merge(&["x"]);
    assert_eq!(code(&out), 3, "{}", both(&out));
    assert!(
        stderr(&out).contains(&format!("{other}: no machine")),
        "{}",
        both(&out)
    );
    assert!(runs_of_kind(&p, "check").is_empty(), "no check ran");
    assert_eq!(p.main_tip(), old);
    // Its waiver is accepted: the merged settings name the platform.
    let out = p.run(&["merge", "x", "--waive", other, "--reason", "away"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(stdout(&out).contains("rules changed"), "{}", both(&out));
}

#[test]
fn r10_waivers_repeat_each_with_its_reason() {
    let others: Vec<&str> = ["linux-x86_64", "linux-aarch64", "macos-x86_64"]
        .into_iter()
        .filter(|p| *p != this_platform())
        .take(2)
        .collect();
    let p = Proj::with_settings(&format!(
        "\n[platforms.{}]\nrun = [\"sh\", \"-c\", \"true\"]\n\n[platforms.{}]\nrun = [\"sh\", \"-c\", \"true\"]\n",
        others[0], others[1]
    ));
    let c = p.ready("feat", "a.txt", "a\n");
    let out = p.run(&["merge", "feat", "--waive", others[0], "--reason", "one"]);
    assert_eq!(code(&out), 3, "{}", both(&out));
    let out = p.run(&[
        "merge", "feat", "--waive", others[0], "--waive", others[1], "--reason", "one",
    ]);
    assert_eq!(code(&out), 2, "{}", both(&out));
    let out = p.run(&[
        "merge", "feat", "--waive", others[0], "--reason", "one", "--waive", others[1], "--reason",
        "two",
    ]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    assert!(so.contains(&format!("{} WAIVED (one)", others[0])), "{so}");
    assert!(so.contains(&format!("{} WAIVED (two)", others[1])), "{so}");
    p.assert_merged("feat", &c);
}

#[test]
fn r10_a_tmp_in_the_branchs_worktree_does_not_hold_up_the_merge() {
    let p = Proj::new();
    let c = p.ready("feat", "a.txt", "a\n");
    let started = p.base.join("tmp-started");
    let mut child = p
        .repo
        .atelier_cmd(
            &p.wt("feat"),
            &[
                "tmp",
                "--",
                "sh",
                "-c",
                &format!("touch {}; sleep 20", started.display()),
            ],
            &[],
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("tmp to start", || started.exists());
    let t = Instant::now();
    let out = p.merge(&["feat"]);
    let took = t.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(code(&out), 0, "{}", both(&out));
    p.assert_merged("feat", &c);
    assert!(
        took < Duration::from_secs(10),
        "no wait for the lock: {took:?}"
    );
}

#[test]
fn a_merged_commit_whose_pinned_atelier_is_not_installed_is_refused() {
    let p = Proj::new();
    let x = p.start("x", None);
    let text =
        settings_text(&x).replace(&format!("atelier = \"{VERSION}\""), "atelier = \"0.1.99\"");
    // Its rules cannot be regenerated here: 0.1.99 is not installed.
    fs::write(x.join(".atelier/atelier.toml"), &text).unwrap();
    let cx = commit(&x, ".atelier/atelier.toml", &text, "pin 0.1.99");
    p.approve("x", &cx);
    let old = p.main_tip();
    let out = p.merge(&["x"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("atelier 0.1.99, pinned by the merged commit"),
        "{}",
        both(&out)
    );
    assert!(stderr(&out).contains("is not installed"), "{}", both(&out));
    assert_eq!(p.main_tip(), old);
}

#[test]
fn follow_settles_a_conflict_only_in_regenerated_paths_with_the_branchs_side() {
    let p = Proj::with_files(&[("parts/0", "zero\n"), ("gen.txt", "zero\n")], REGEN);
    let wt = p.start("feat", None);
    fs::write(wt.join("parts/f"), "f\n").unwrap();
    commit(&wt, "gen.txt", "zero\nf\n", "feat");
    p.repo.write("parts/m", "m\n");
    p.repo.write("gen.txt", "zero\nm\n");
    p.repo.commit_all("main regenerates");
    let out = p.repo.atelier_in(&wt, &["follow"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let so = stdout(&out);
    assert!(
        so.contains("follow: conflict in gen.txt settled with feat's side"),
        "{so}"
    );
    assert!(so.contains("run the generator"), "{so}");
    assert_eq!(fs::read_to_string(wt.join("gen.txt")).unwrap(), "zero\nf\n");
    assert!(wt.join("parts/m").is_file());
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
}

// ---------------------------------------------------------------------------------------------
// Platforms on the owner's machines (module 3.6): a fake remote machine `box` of this platform,
// reached through the fake ssh, which runs its commands on this computer.

/// A machines file whose one machine is `box`, with the binary under test installed there and
/// the project's checkout created; returns the environment the program needs to reach it.
fn with_box(p: &Proj) -> Vec<(String, String)> {
    let machines = p.base.join("machines");
    let bin = machines.join(format!("box/.local/share/atelier/{VERSION}/bin"));
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(atelier_bin(), bin.join("atelier")).unwrap();
    let file = p.repo.home.join(".config/atelier/machines.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        &file,
        format!(
            "[machines.box]\nssh = \"box\"\nplatform = \"{}\"\npriority = 10\npath = [\"/usr/bin\", \"/bin\"]\ncheckouts = \"~/checkouts\"\n",
            this_platform()
        ),
    )
    .unwrap();
    let env = vec![
        (
            "ATELIER_SSH".to_owned(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-ssh")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "FAKE_SSH_ROOT".to_owned(),
            machines.to_string_lossy().into_owned(),
        ),
    ];
    let out = run_env(p, &["machines", "init", "box"], &env);
    assert_eq!(code(&out), 0, "machines init: {}", both(&out));
    env
}

fn run_env(p: &Proj, args: &[&str], env: &[(String, String)]) -> Output {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    p.repo.atelier_env(p.main(), args, &env)
}

/// The project's checkout on `box`.
fn box_checkout(p: &Proj) -> PathBuf {
    let dir = p.base.join("machines/box/checkouts");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries.pop().unwrap()
}

#[test]
fn the_gate_runs_a_platform_on_the_machine_chosen_for_it_bound_to_m() {
    let p = Proj::new();
    let env = with_box(&p);
    let c = p.ready("feat", "a.txt", "a\n");
    let out = run_env(&p, &["merge", "feat"], &env);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let here = this_platform();
    let so = stdout(&out);
    assert!(so.contains(&format!("  {here}: running on box")), "{so}");
    assert!(so.contains(&format!("  {here}: PASS on box")), "{so}");
    assert!(so.contains(&format!("{here} PASS on box")), "{so}");
    p.assert_merged("feat", &c);
    assert!(
        p.merge_lines()[0]
            .platforms
            .iter()
            .any(|x| x == &format!("{here}=pass@box")),
        "{:?}",
        p.merge_lines()
    );
    // box ran the merged commit itself: its checkout is at M, and its runs record M.
    let m = p.main_tip();
    let remote = box_checkout(&p);
    assert_eq!(git(&remote, &["rev-parse", "HEAD"]), m);
    let mut kinds = Vec::new();
    for e in fs::read_dir(remote.join(".git/atelier/runs"))
        .unwrap()
        .flatten()
    {
        if let Some(r) = atelier::runs::RunResult::read(&e.path()).unwrap() {
            assert_eq!(r.commit.as_deref(), Some(m.as_str()));
            kinds.push(r.kind.as_str().to_owned());
        }
    }
    kinds.sort();
    assert_eq!(kinds, ["check", "test-full"]);
    // Nothing ran the check on this computer.
    assert!(runs_of_kind(&p, "check").is_empty());

    // box away: exit 3 before the gate, naming why.
    p.ready("next", "b.txt", "b\n");
    fs::rename(
        p.base.join("machines/box"),
        p.base.join("machines/box-away"),
    )
    .unwrap();
    let gates = runs_of_kind(&p, "gate").len();
    let out = run_env(&p, &["merge", "next"], &env);
    assert_eq!(code(&out), 3, "{}", both(&out));
    assert!(
        stderr(&out).contains(&format!("{here}: box offline")),
        "{}",
        both(&out)
    );
    assert_eq!(runs_of_kind(&p, "gate").len(), gates, "checked first");
}

#[test]
fn a_red_on_another_machine_is_bisected_on_that_machine() {
    let p = Proj::new();
    let env = with_box(&p);
    let ca = p.ready("a", "a.txt", "a\n");
    p.ready("b", "BROKEN", "b\n");
    p.ready("c", "c.txt", "c\n");
    let out = run_env(&p, &["merge", "a", "b", "c"], &env);
    assert_eq!(code(&out), 1, "{}", both(&out));
    let here = this_platform();
    let so = stdout(&out);
    assert!(
        so.contains(&format!("  {here}: FAILED (on box, run ")),
        "{so}"
    );
    assert!(
        so.contains(&format!("re-running only {here}: its whole run (on box)")),
        "{so}"
    );
    assert!(so.contains("prefix a, b: fails"), "{so}");
    assert!(so.contains("prefix a: none of the failures appear"), "{so}");
    assert!(
        so.contains(&format!(
            "merge: b breaks: on prefix a, b: {here}: on box, run "
        )),
        "{so}"
    );
    assert!(so.contains("merged a"), "{so}");
    p.assert_merged("a", &ca);
    assert!(
        runs_of_kind(&p, "check").is_empty(),
        "everything ran on box"
    );
}
