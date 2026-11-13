//! `atelier clean [<branch>…] [--yes] [--discard <commit>…]`: deletes what
//! was merged and what atelier made, and nothing else.
//!
//! For each branch, under its lock taken exclusive (and the run lock of the worktree it deletes),
//! nothing is trusted that is not re-read from the disk now. The facts that grant authority are
//! the branch's record (state `merged`) and its [`MergeFacts`]: the commit that was merged and
//! the one worktree `approve` recorded, by canonical path and file-system identity. A branch
//! whose record is missing, active, or without those facts is never touched.
//!
//! The plan checks every condition of every step below before anything is deleted for the
//! branch, so a refusal (4) changes nothing. Without `--yes` it is printed. With it, the steps run
//! in this order, least destructive first, and each repeats its checks immediately before it
//! deletes:
//!
//! 1. each registered temp folder: its path has exactly the shape `tmp` gives it and is
//!    canonical, it is a real folder with the registered identity owned by this user, nothing in
//!    it is on another device (a mount), and its `leftovers` file (things created outside the
//!    folder) is empty or absent; removed through descriptors, never following a link and never
//!    entering another device ([`dirfd::remove_tree`]);
//! 2. the worktree: the recorded path is a real folder with the recorded identity and no link in
//!    its path, the top of a registered worktree of this repository that is not locked, not the
//!    main checkout and not the gate, with no worktree nested in it or around it, the branch
//!    checked out there at the merged commit, no operation in progress, no uncommitted and no
//!    untracked file, no local edit hidden by `--assume-unchanged` or `--skip-worktree`, and,
//!    walked without following links, no entry on another device and no nested repository (a
//!    `.git` below the top, ignored folders included). The plan names the ignored top-level
//!    entries that go with it. It is first moved to a random name beside it (`git worktree
//!    move`), so no other worktree can take its path meanwhile; its identity is checked there
//!    again, then it is removed with `git worktree remove`, never `--force`;
//! 3. the branch: a plain ref still at the merged commit, not the integration branch, not checked
//!    out or open in a rebase or bisect anywhere, and its reflog names no commit that would be
//!    lost: one no other ref reaches whose work the merged commit does not contain (work made
//!    after approval, then reset away). Such commits are listed; naming each with `--discard`
//!    lets them go. Deleted with `update-ref --no-deref -d` at the merged commit;
//! 4. `mbx clean --under <worktree>` when mbx is a prerequisite and the worktree's path is gone.
//!
//! After every branch, with `[prerequisites.mbx]`, one `mbx gc` runs as its own step (printed in
//! the plan, skipped without `--yes`). Its failure is reported (exit 1) and undoes nothing.
//!
//! Then the record becomes `cleaned` and its facts are removed. A step that fails (a file that
//! cannot be deleted, a check that no longer holds, a git error) stops `clean` for the branch
//! (exit 1): what was deleted stays deleted, the record stays `merged`, and running `clean`
//! again does the rest.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::dirfd;
use crate::exit::{Exit, Failure, Result};
pub use crate::gate::{gate_worktree, main_root};
use crate::git::{self, Checkout, canonical};
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::Context;
use crate::records::{BranchRecord, BranchState, MergeFacts, TmpEntry};
use crate::refs;
use crate::stack::Stack;

/// A reason a check refused.
type Check<T> = std::result::Result<T, String>;

/// A short id of this repository for temp folder paths: the FNV-1a hash of its common git
/// folder's canonical path.
pub fn repo_id(checkout: &Checkout) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical(&checkout.common_dir).to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The name of a short temp folder: `atl-<id>`.
pub fn short_name(id: &str) -> String {
    format!("atl-{id}")
}

struct Cx<'a> {
    ctx: &'a Context,
    stack: Stack<'a>,
    main: PathBuf,
    common: PathBuf,
    gate: PathBuf,
    repo_id: String,
    /// `/tmp`, canonical.
    short_root: PathBuf,
    uid: u32,
}

impl Cx<'_> {
    /// A folder of the clone where git runs: the main checkout `clean` runs in.
    fn dir(&self) -> &Path {
        &self.ctx.checkout.root
    }
}

/// One entry of `git worktree list`.
struct Listed {
    path: PathBuf,
    /// The full ref `HEAD` names, if it names one.
    branch: Option<String>,
}

fn worktree_list(dir: &Path) -> Check<Vec<Listed>> {
    let out = git::output(dir, ["worktree", "list", "--porcelain", "-z"]).map_err(|f| f.message)?;
    if !out.status.success() {
        return Err(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut list: Vec<Listed> = Vec::new();
    for field in out.stdout.split(|&b| b == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(p) = field.strip_prefix("worktree ") {
            list.push(Listed {
                path: PathBuf::from(p),
                branch: None,
            });
        } else if let Some(b) = field.strip_prefix("branch ")
            && let Some(last) = list.last_mut()
        {
            last.branch = Some(b.to_owned());
        }
    }
    Ok(list)
}

/// The recorded worktree as it is now.
enum Wt {
    /// Nothing at the recorded path.
    Absent,
    /// Something else is there: not what `approve` recorded. Left alone.
    Elsewhere,
    /// The recorded worktree, every condition holding.
    Ours(Checkout),
}

enum Br {
    Absent,
    Delete,
}

enum Tmp {
    Remove {
        file: PathBuf,
        entry: TmpEntry,
    },
    /// The registered folder is gone or is something else now; only the entry goes.
    Forget {
        file: PathBuf,
        why: String,
    },
}

/// A commit of the branch's reflog that deleting the branch would lose.
struct Lost {
    commit: String,
    subject: String,
    /// The commits lost with it (its own included).
    count: usize,
}

struct Plan {
    wt: Wt,
    /// The ignored top-level entries of the worktree, which `git worktree remove` deletes too.
    ignored: Vec<String>,
    br: Br,
    /// Lost commits the command line discards.
    discarded: Vec<Lost>,
    tmps: Vec<Tmp>,
    mbx: bool,
}

pub fn run(ctx: &Context, branches: &[String], yes: bool, discard: &[String]) -> Result<Exit> {
    let checkout = &ctx.checkout;
    let integration = &ctx.settings.integration_branch;
    let stack = Stack::new(checkout, &ctx.state, integration);
    let cx = Cx {
        ctx,
        stack,
        main: main_root(checkout),
        common: canonical(&checkout.common_dir),
        gate: gate_worktree(checkout),
        repo_id: repo_id(checkout),
        short_root: canonical(Path::new("/tmp")),
        // SAFETY: geteuid has no preconditions.
        uid: unsafe { libc::geteuid() },
    };

    let targets: Vec<String> = if branches.is_empty() {
        cx.stack
            .all_records()?
            .into_iter()
            .filter(|r| r.state == BranchState::Merged)
            .map(|r| r.name)
            .collect()
    } else {
        let mut seen: Vec<String> = Vec::new();
        for b in branches {
            names::validate_branch(&checkout.root, b)?;
            if !seen.contains(b) {
                seen.push(b.clone());
            }
        }
        seen
    };
    let mut discards: Vec<String> = Vec::new();
    for d in discard {
        if d.starts_with('-') {
            return Err(Failure::usage(format!(
                "clean: --discard {d} is not a commit"
            )));
        }
        let full = git::output(
            &checkout.root,
            ["rev-parse", "-q", "--verify", &format!("{d}^{{commit}}")],
        )?;
        if !full.status.success() {
            return Err(Failure::usage(format!(
                "clean: --discard {d} is not a commit"
            )));
        }
        discards.push(String::from_utf8_lossy(&full.stdout).trim().to_owned());
    }
    if targets.is_empty() {
        println!("clean: no merged branch");
        return Ok(Exit::Ok);
    }

    let (mut refused, mut failed) = (0, 0);
    let mut used: Vec<String> = Vec::new();
    for branch in &targets {
        println!("{branch}");
        match process(&cx, branch, yes, &discards, &mut used) {
            Outcome::Done => {}
            Outcome::Refused(reasons) => {
                refused += 1;
                for r in reasons {
                    println!("  refused: {r}");
                }
            }
            Outcome::Failed(why) => {
                failed += 1;
                println!("  failed: {why}");
            }
        }
    }
    for (d, given) in discards.iter().zip(discard) {
        if !used.contains(d) {
            println!("clean: --discard {given} names no lost commit of these branches: ignored");
        }
    }
    let mut gc_failed = false;
    if cx.ctx.settings.prerequisites.mbx.is_some() {
        if yes {
            println!("mbx gc");
            match Command::new("mbx").arg("gc").current_dir(&cx.main).status() {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    gc_failed = true;
                    println!("  failed: mbx gc failed ({s}); nothing else was undone");
                }
                Err(e) => {
                    gc_failed = true;
                    println!("  failed: cannot run mbx gc: {e}; nothing else was undone");
                }
            }
        } else {
            println!("run mbx gc once, after the branches");
        }
    }
    if !yes {
        println!("clean: this is the plan; nothing was changed. Run again with --yes to do it");
    }
    if failed == 0 && gc_failed {
        return Err(Failure::fail(
            "clean: mbx gc failed (the branches are cleaned)",
        ));
    }
    if failed > 0 {
        return Err(Failure::fail(format!(
            "clean: {failed} branch(es) not finished (the record stays merged; run clean again){}",
            if refused > 0 {
                format!(", {refused} refused")
            } else {
                String::new()
            }
        )));
    }
    if refused > 0 {
        return Err(Failure::refused(format!(
            "clean: {refused} branch(es) refused; nothing was deleted for them"
        )));
    }
    Ok(Exit::Ok)
}

enum Outcome {
    Done,
    Refused(Vec<String>),
    Failed(String),
}

fn process(
    cx: &Cx,
    branch: &str,
    yes: bool,
    discards: &[String],
    used: &mut Vec<String>,
) -> Outcome {
    let state = &cx.ctx.state;
    let _lock = match HeldLock::acquire(state, LockKey::branch(branch)) {
        Ok(l) => l,
        Err(f) => return Outcome::Refused(vec![f.message]),
    };
    let rec = match cx.stack.record(branch) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Outcome::Refused(vec![format!(
                "{branch} has no record: it is not touched (a branch atelier did not merge is never cleaned)"
            )]);
        }
        Err(f) => return Outcome::Refused(vec![f.message]),
    };
    match rec.state {
        BranchState::Active => {
            return Outcome::Refused(vec![format!(
                "{branch} is not merged (no merge record): it is not touched"
            )]);
        }
        BranchState::Cleaned => {
            println!("  already cleaned");
            if yes && let Err(f) = cx.stack.drop_if_unneeded(branch, &rec.lifetime) {
                return Outcome::Failed(f.message);
            }
            return Outcome::Done;
        }
        BranchState::Merged => {}
    }
    if rec.starting.is_some() {
        return Outcome::Refused(vec![format!(
            "{branch}: its start did not finish: run atelier start {branch} again"
        )]);
    }
    let facts = match MergeFacts::read(&rec) {
        Ok(Some(f)) => f,
        Ok(None) => {
            return Outcome::Refused(vec![format!(
                "{branch} is marked merged but has no merge facts (the merged commit and the worktree approve recorded): it is not touched"
            )]);
        }
        Err(f) => return Outcome::Refused(vec![f.message]),
    };

    let plan = match plan(cx, &rec, &facts, discards) {
        Ok(p) => p,
        Err(reasons) => return Outcome::Refused(reasons),
    };
    used.extend(plan.discarded.iter().map(|l| l.commit.clone()));
    // A `check` in the worktree holds its run lock: wait for none, refuse.
    let wt_lock = match &plan.wt {
        Wt::Ours(co) => match HeldLock::acquire(state, LockKey::worktree(co)) {
            Ok(l) => Some(l),
            Err(f) => {
                return Outcome::Refused(vec![format!(
                    "{}: {} (a check or another command is running there)",
                    facts.worktree.display(),
                    f.message
                )]);
            }
        },
        _ => None,
    };

    if !yes {
        for line in describe(&plan, &rec, &facts) {
            println!("  would {line}");
        }
        return Outcome::Done;
    }
    match execute(cx, &rec, &facts, &plan, discards) {
        Ok(()) => {
            drop(wt_lock);
            Outcome::Done
        }
        Err(why) => Outcome::Failed(why),
    }
}

fn describe(plan: &Plan, rec: &BranchRecord, facts: &MergeFacts) -> Vec<String> {
    let mut lines = Vec::new();
    let p = facts.worktree.display();
    for t in &plan.tmps {
        match t {
            Tmp::Remove { entry, .. } => {
                lines.push(format!("remove temp folder {}", entry.path.display()))
            }
            Tmp::Forget { file, why } => lines.push(format!(
                "forget registered temp folder {} ({why})",
                file.display()
            )),
        }
    }
    match &plan.wt {
        Wt::Ours(_) if plan.ignored.is_empty() => lines.push(format!("remove worktree {p}")),
        Wt::Ours(_) => lines.push(format!(
            "remove worktree {p}, with its ignored files: {}",
            plan.ignored.join(", ")
        )),
        Wt::Absent => lines.push(format!("(worktree {p} is already gone)")),
        Wt::Elsewhere => lines.push(format!(
            "(worktree {p} is not the one approve recorded any more: left alone)"
        )),
    }
    if let Br::Delete = plan.br {
        lines.push(format!(
            "delete branch {} at {}",
            rec.name,
            &facts.commit[..12.min(facts.commit.len())]
        ));
        for l in &plan.discarded {
            lines.push(format!("  discarding {}", describe_lost(l)));
        }
    }
    if plan.mbx {
        lines.push(format!("run mbx clean --under {p}"));
    }
    lines.push(format!("mark {} cleaned", rec.name));
    lines
}

fn describe_lost(l: &Lost) -> String {
    let more = if l.count > 1 {
        format!(", with {} earlier commit(s) only it reaches", l.count - 1)
    } else {
        String::new()
    };
    format!("{} {}{more}", l.commit, l.subject)
}

/// Everything that must hold, read now. Every refusal is listed.
fn plan(
    cx: &Cx,
    rec: &BranchRecord,
    facts: &MergeFacts,
    discards: &[String],
) -> std::result::Result<Plan, Vec<String>> {
    let mut reasons: Vec<String> = Vec::new();
    let branch = &rec.name;
    for open in [
        cx.stack.rebase_open(branch).map(|o| ("a rebase", o)),
        cx.stack.bisect_open(branch).map(|o| ("a bisect", o)),
    ] {
        match open {
            Ok((what, Some(wt))) => reasons.push(format!(
                "{branch} has {what} open in {}: finish it first",
                wt.display()
            )),
            Ok(_) => {}
            Err(f) => reasons.push(f.message),
        }
    }

    // A recorded worktree that is refused is already reported: the branch being checked out
    // there is not a second reason.
    let mut wt_refused = false;
    let wt = match inspect_worktree(cx, rec, facts) {
        Ok(w) => w,
        Err(why) => {
            reasons.push(why);
            wt_refused = true;
            Wt::Elsewhere
        }
    };
    let removing = match &wt {
        Wt::Ours(_) => Some(facts.worktree.as_path()),
        _ if wt_refused => Some(facts.worktree.as_path()),
        _ => None,
    };
    let ignored = match &wt {
        Wt::Ours(_) => ignored_top(&facts.worktree).unwrap_or_else(|why| {
            reasons.push(why);
            Vec::new()
        }),
        _ => Vec::new(),
    };
    let mut discarded = Vec::new();
    let br = match inspect_branch(cx, rec, facts, removing) {
        Ok(Br::Delete) => {
            match lost_check(cx, rec, facts, discards) {
                Ok(d) => discarded = d,
                Err(why) => reasons.push(why),
            }
            Br::Delete
        }
        Ok(b) => b,
        Err(why) => {
            reasons.push(why);
            Br::Absent
        }
    };
    let mut tmps = Vec::new();
    match TmpEntry::list(&cx.ctx.state, &rec.lifetime) {
        Ok(list) => {
            for (file, entry) in list {
                match inspect_tmp(cx, rec, &file, &entry) {
                    Ok(t) => tmps.push(t),
                    Err(why) => reasons.push(why),
                }
            }
        }
        Err(f) => reasons.push(f.message),
    }
    if !reasons.is_empty() {
        return Err(reasons);
    }
    let mbx = cx.ctx.settings.prerequisites.mbx.is_some() && !matches!(wt, Wt::Elsewhere);
    Ok(Plan {
        wt,
        ignored,
        br,
        discarded,
        tmps,
        mbx,
    })
}

fn inspect_worktree(cx: &Cx, rec: &BranchRecord, facts: &MergeFacts) -> Check<Wt> {
    let p = &facts.worktree;
    let shown = p.display();
    let meta = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Wt::Absent),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    if !meta.is_dir() || meta.dev() != facts.dev || meta.ino() != facts.ino {
        return Ok(Wt::Elsewhere);
    }
    if fs::canonicalize(p).ok().as_deref() != Some(p.as_path()) {
        return Err(format!(
            "{shown} is not a canonical path any more (a link in it): it is not deleted"
        ));
    }
    let co = match Checkout::find(p) {
        Ok(Some(co)) => co,
        Ok(None) => return Err(format!("{shown} is not a git worktree")),
        Err(f) => return Err(format!("{shown}: {}", f.message)),
    };
    if canonical(&co.root) != *p {
        return Err(format!("{shown} is not the top folder of a worktree"));
    }
    if canonical(&co.common_dir) != cx.common {
        return Err(format!("{shown} belongs to another repository"));
    }
    if co.is_main_worktree() || *p == cx.main {
        return Err(format!("{shown} is the main checkout"));
    }
    if *p == cx.gate {
        return Err(format!("{shown} is the gate worktree"));
    }
    let list = worktree_list(cx.dir())?;
    if !list.iter().any(|w| canonical(&w.path) == *p) {
        return Err(format!(
            "{shown} is not a registered worktree of this repository"
        ));
    }
    for w in &list {
        let q = canonical(&w.path);
        if q == *p {
            continue;
        }
        if q.starts_with(p) {
            return Err(format!("{} is nested inside {shown}", q.display()));
        }
        if p.starts_with(&q) {
            return Err(format!("{shown} is nested inside {}", q.display()));
        }
    }
    for other in [&cx.common, &cx.gate] {
        if other.starts_with(p) {
            return Err(format!("{} is inside {shown}", other.display()));
        }
    }
    if p.starts_with(&cx.common) || p.starts_with(&cx.gate) {
        return Err(format!("{shown} is inside the state or the gate"));
    }
    if let Ok(entries) = TmpEntry::list(&cx.ctx.state, &rec.lifetime) {
        for (_, e) in entries {
            if e.path.starts_with(p) {
                return Err(format!(
                    "temp folder {} is inside {shown}",
                    e.path.display()
                ));
            }
        }
    }

    let refname = names::branch_ref(&rec.name);
    let head =
        git::output(p, ["symbolic-ref", "--no-recurse", "-q", "HEAD"]).map_err(|f| f.message)?;
    let on = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if !head.status.success() || on != refname {
        return Err(format!(
            "{shown} has {} checked out, not {}: it is not deleted",
            if on.is_empty() {
                "a detached HEAD".to_owned()
            } else {
                on
            },
            rec.name
        ));
    }
    let tip =
        git::output(p, ["rev-parse", "-q", "--verify", "HEAD^{commit}"]).map_err(|f| f.message)?;
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
    if tip != facts.commit {
        return Err(format!(
            "{shown} is at {tip}, not at the merged commit {}: it holds work that was not merged",
            facts.commit
        ));
    }
    if co.git_dir.join("locked").exists() {
        return Err(format!(
            "{shown} is locked (git worktree lock): unlock it first if it may go"
        ));
    }
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_START",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        if co.git_dir.join(name).exists() {
            return Err(format!("{shown} has an operation in progress ({name})"));
        }
    }
    let status = git::output(
        p,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
    .map_err(|f| f.message)?;
    if !status.status.success() {
        return Err(format!(
            "git status in {shown} failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    if !status.stdout.is_empty() {
        let files: Vec<String> = status
            .stdout
            .split(|&b| b == 0)
            .filter(|e| !e.is_empty())
            .take(8)
            .map(|e| String::from_utf8_lossy(e).trim_start().to_owned())
            .collect();
        return Err(format!(
            "{shown} has uncommitted or untracked files ({}): it is not deleted",
            files.join(", ")
        ));
    }
    let hidden = hidden_edits(p)?;
    if !hidden.is_empty() {
        return Err(format!(
            "{shown} has local edits git status does not show, hidden by --assume-unchanged or --skip-worktree ({}): it is not deleted",
            hidden.join(", ")
        ));
    }
    let found = dirfd::survey(p).map_err(|e| format!("{shown}: {e}"))?;
    if let Some(why) = survey_refusal(p, &found, true) {
        return Err(why);
    }
    Ok(Wt::Ours(co))
}

fn inspect_branch(
    cx: &Cx,
    rec: &BranchRecord,
    facts: &MergeFacts,
    removing: Option<&Path>,
) -> Check<Br> {
    let branch = &rec.name;
    if *branch == *cx.stack.integration() {
        return Err(format!("{branch} is the integration branch"));
    }
    let refname = names::branch_ref(branch);
    let Some(tip) = refs::read(cx.dir(), &refname).map_err(|f| f.message)? else {
        return Ok(Br::Absent);
    };
    if tip != facts.commit {
        return Err(format!(
            "{branch} moved after the merge: it is at {tip}, merged at {}; the branch is kept",
            facts.commit
        ));
    }
    for w in worktree_list(cx.dir())? {
        if w.branch.as_deref() == Some(refname.as_str()) {
            let at = canonical(&w.path);
            if removing.is_some_and(|r| r == at) {
                continue;
            }
            return Err(format!(
                "{branch} is checked out in {}: the branch is kept",
                w.path.display()
            ));
        }
    }
    Ok(Br::Delete)
}

fn inspect_tmp(cx: &Cx, rec: &BranchRecord, file: &Path, entry: &TmpEntry) -> Check<Tmp> {
    let shown = entry.path.display();
    let id = file
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{}: not an entry name", file.display()))?;
    if !shape_ok(cx, &rec.name, id, &entry.path) {
        return Err(format!(
            "{}: registered path {shown} is not where atelier makes temp folders for {}: remove that entry by hand if it is wrong",
            file.display(),
            rec.name
        ));
    }
    let meta = match fs::symlink_metadata(&entry.path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Ok(Tmp::Forget {
                file: file.to_path_buf(),
                why: "the folder is gone".to_owned(),
            });
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    if !meta.is_dir() || meta.dev() != entry.dev || meta.ino() != entry.ino {
        return Ok(Tmp::Forget {
            file: file.to_path_buf(),
            why: "that path is something else now: left alone".to_owned(),
        });
    }
    if fs::canonicalize(&entry.path).ok().as_deref() != Some(entry.path.as_path()) {
        return Err(format!("{shown} has a link in its path: it is not deleted"));
    }
    if meta.uid() != cx.uid {
        return Err(format!(
            "{shown} is not owned by this user: it is not deleted"
        ));
    }
    let leftovers = entry.path.join("leftovers");
    match fs::symlink_metadata(&leftovers) {
        Ok(m) if m.is_file() && m.len() == 0 => {}
        Ok(m) if m.is_file() => {
            let mut text = String::new();
            if let Ok(f) = fs::File::open(&leftovers) {
                let _ = f.take(8192).read_to_string(&mut text);
            }
            return Err(format!(
                "{shown} lists things it left outside (remove them, then their lines):\n{}",
                text.trim_end()
                    .lines()
                    .map(|l| format!("      {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        Ok(_) => return Err(format!("{} is not a regular file", leftovers.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", leftovers.display())),
    }
    let found = dirfd::survey(&entry.path).map_err(|e| format!("{shown}: {e}"))?;
    if let Some(why) = survey_refusal(&entry.path, &found, false) {
        return Err(why);
    }
    Ok(Tmp::Remove {
        file: file.to_path_buf(),
        entry: entry.clone(),
    })
}

/// The path is exactly what `tmp` makes: `<temp dir>/atelier-<uid>/<repo id>/<branch key>/<id>` or
/// `<canonical /tmp>/atl-<id>`, absolute, with no `.` or `..`.
fn shape_ok(cx: &Cx, branch: &str, id: &str, path: &Path) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let key = names::key(branch);
    if let [.., a, r, k, i] = comps.as_slice()
        && *a == format!("atelier-{}", cx.uid)
        && *r == cx.repo_id
        && *k == key
        && *i == id
    {
        return true;
    }
    path.parent() == Some(cx.short_root.as_path())
        && path.file_name().and_then(|n| n.to_str()) == Some(short_name(id).as_str())
}

fn execute(
    cx: &Cx,
    rec: &BranchRecord,
    facts: &MergeFacts,
    plan: &Plan,
    discards: &[String],
) -> std::result::Result<(), String> {
    let p = &facts.worktree;
    let branch = &rec.name;
    for t in &plan.tmps {
        match t {
            Tmp::Forget { file, why } => {
                remove_entry(file)?;
                println!("  forgot {} ({why})", file.display());
            }
            Tmp::Remove { file, entry } => {
                match inspect_tmp(cx, rec, file, entry)? {
                    Tmp::Remove { .. } => {
                        let (Some(parent), Some(name)) =
                            (entry.path.parent(), entry.path.file_name())
                        else {
                            return Err(format!("{}: not a folder path", entry.path.display()));
                        };
                        dirfd::remove_tree(parent, name, (entry.dev, entry.ino))
                            .map_err(|e| e.to_string())?;
                        println!("  removed temp folder {}", entry.path.display());
                        if parent.file_name().and_then(|n| n.to_str())
                            == Some(names::key(branch).as_str())
                        {
                            let _ = fs::remove_dir(parent);
                        }
                    }
                    Tmp::Forget { .. } => {}
                }
                remove_entry(file)?;
            }
        }
    }
    let _ = fs::remove_dir(cx.ctx.state.tmp_dir().join(&rec.lifetime));

    if let Wt::Ours(_) = plan.wt {
        match inspect_worktree(cx, rec, facts)? {
            Wt::Ours(_) => {}
            _ => {
                return Err(format!(
                    "{} changed since the plan: not deleted",
                    p.display()
                ));
            }
        }
        remove_worktree(cx, facts)?;
        println!("  removed worktree {}", p.display());
    }
    if let Br::Delete = plan.br {
        match inspect_branch(cx, rec, facts, None)? {
            Br::Delete => {
                lost_check(cx, rec, facts, discards)?;
                refs::delete(cx.dir(), &names::branch_ref(branch), &facts.commit)
                    .map_err(|f| f.message)?;
                println!("  deleted branch {branch}");
            }
            Br::Absent => {}
        }
    }
    if plan.mbx {
        match fs::symlink_metadata(p) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                println!("  mbx clean --under {}", p.display());
                let status = Command::new("mbx")
                    .args(["clean", "--under"])
                    .arg(p)
                    .current_dir(&cx.main)
                    .status()
                    .map_err(|e| format!("cannot run mbx: {e}"))?;
                if !status.success() {
                    return Err(format!(
                        "mbx clean --under {} failed ({status})",
                        p.display()
                    ));
                }
            }
            _ => println!(
                "  mbx clean skipped: {} exists (it is not ours to clean)",
                p.display()
            ),
        }
    }
    MergeFacts::retire(&cx.ctx.state, branch, &rec.lifetime).map_err(|f| f.message)?;
    cx.stack
        .drop_if_unneeded(branch, &rec.lifetime)
        .map_err(|f| f.message)?;
    println!("  {branch} is cleaned");
    Ok(())
}

/// Moves the checked worktree to a random name beside it, checks it is still the recorded folder
/// there, and removes it. A path no one else knows cannot be given to another worktree between
/// the check and the removal. On a failure after the move, it is moved back.
fn remove_worktree(cx: &Cx, facts: &MergeFacts) -> std::result::Result<(), String> {
    let p = &facts.worktree;
    let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
        return Err(format!("{}: not a folder path", p.display()));
    };
    let mut aside = name.to_owned();
    aside.push(".atelier-removing-");
    aside.push(crate::fsutil::random_hex(6).map_err(|e| e.to_string())?);
    let aside = parent.join(aside);
    let git_wt = |args: &[&std::ffi::OsStr]| -> std::result::Result<(), String> {
        let mut all: Vec<&std::ffi::OsStr> = vec!["worktree".as_ref()];
        all.extend(args);
        let out = git::output(&cx.main, &all).map_err(|f| f.message)?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git worktree {}: {}",
                args.iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    git_wt(&["move".as_ref(), p.as_os_str(), aside.as_os_str()])?;
    let back = |why: String| -> String {
        match git_wt(&["move".as_ref(), aside.as_os_str(), p.as_os_str()]) {
            Ok(()) => why,
            Err(e) => format!(
                "{why}; moving it back to {} failed too ({e}): it is at {}",
                p.display(),
                aside.display()
            ),
        }
    };
    match fs::symlink_metadata(&aside) {
        Ok(m) if m.is_dir() && m.dev() == facts.dev && m.ino() == facts.ino => {}
        _ => {
            return Err(back(format!(
                "{} is not the recorded worktree after the move: not deleted",
                aside.display()
            )));
        }
    }
    git_wt(&["remove".as_ref(), aside.as_os_str()]).map_err(back)
}

/// A refusal for what [`dirfd::survey`] found in `root`; nested repositories count when `repos`.
fn survey_refusal(root: &Path, found: &dirfd::Survey, repos: bool) -> Option<String> {
    let shown = root.display();
    let list = |v: &[PathBuf]| {
        v.iter()
            .take(8)
            .map(|r| root.join(r).display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !found.mounts.is_empty() {
        return Some(format!(
            "{shown} has something of another file system mounted in it ({}): unmount it first; nothing is deleted",
            list(&found.mounts)
        ));
    }
    if repos && !found.repos.is_empty() {
        return Some(format!(
            "{shown} holds a nested git repository ({}), whose commits would go with it: move it out first; nothing is deleted",
            list(&found.repos)
        ));
    }
    if let Some((at, e)) = found.unreadable.first() {
        return Some(format!(
            "{} cannot be read ({e}), so what is in it is unknown: nothing is deleted",
            root.join(at).display()
        ));
    }
    None
}

/// Tracked files flagged `--assume-unchanged` or `--skip-worktree` that exist and differ from
/// the index: local edits `git status` and `git worktree remove` do not see.
fn hidden_edits(p: &Path) -> Check<Vec<String>> {
    let out = git::output(p, ["ls-files", "-v", "-s", "-z"]).map_err(|f| f.message)?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files in {}: {}",
            p.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut differ: Vec<String> = Vec::new();
    // Regular files to hash: (path, the index's blob).
    let mut hash: Vec<(String, String)> = Vec::new();
    for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split(' ').collect();
        let [tag, mode, oid, _stage] = fields.as_slice() else {
            continue;
        };
        let flagged = tag.chars().all(|c| c.is_ascii_lowercase()) || *tag == "S";
        if !flagged || *mode == "160000" {
            continue;
        }
        let full = p.join(path);
        let m = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(_) => {
                differ.push(path.to_owned());
                continue;
            }
        };
        if *mode == "120000" {
            let same = m.is_symlink()
                && fs::read_link(&full).is_ok_and(|t| {
                    git::output(p, ["cat-file", "blob", oid])
                        .is_ok_and(|o| o.status.success() && o.stdout == t.as_os_str().as_bytes())
                });
            if !same {
                differ.push(path.to_owned());
            }
        } else if !m.is_file() {
            differ.push(path.to_owned());
        } else {
            hash.push((path.to_owned(), (*oid).to_owned()));
        }
    }
    for chunk in hash.chunks(256) {
        let mut args: Vec<&str> = vec!["hash-object", "--"];
        args.extend(chunk.iter().map(|(path, _)| path.as_str()));
        let out = git::output(p, &args).map_err(|f| f.message)?;
        let got = String::from_utf8_lossy(&out.stdout);
        let got: Vec<&str> = got.lines().collect();
        for (i, (path, oid)) in chunk.iter().enumerate() {
            if !out.status.success() || got.get(i) != Some(&oid.as_str()) {
                differ.push(path.clone());
            }
        }
    }
    differ.truncate(8);
    Ok(differ)
}

/// The top-level entries of the worktree that git ignores (folders end in `/`).
fn ignored_top(p: &Path) -> Check<Vec<String>> {
    let out = git::output(
        p,
        [
            "ls-files",
            "-o",
            "-i",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )
    .map_err(|f| f.message)?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files in {}: {}",
            p.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut top: Vec<String> = Vec::new();
    for e in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let e = String::from_utf8_lossy(e);
        let name = match e.split_once('/') {
            Some((first, _)) => format!("{first}/"),
            None => e.into_owned(),
        };
        if !top.contains(&name) {
            top.push(name);
        }
    }
    top.sort();
    Ok(top)
}

/// The commits of the branch's reflog, since it first held the merged commit, that deleting it
/// would lose: reached by no other ref, not in the merged commit, with work the merged commit
/// does not contain (a commit whose change the merged commit has, cherry-picked, is not lost). Refuses unless each is discarded on
/// the command line; returns the discarded ones.
fn lost_check(
    cx: &Cx,
    rec: &BranchRecord,
    facts: &MergeFacts,
    discards: &[String],
) -> Check<Vec<Lost>> {
    let lost = lost_commits(cx, rec, facts)?;
    let (discarded, kept): (Vec<Lost>, Vec<Lost>) =
        lost.into_iter().partition(|l| discards.contains(&l.commit));
    if kept.is_empty() {
        return Ok(discarded);
    }
    let all: Vec<&str> = kept
        .iter()
        .chain(&discarded)
        .map(|l| l.commit.as_str())
        .collect();
    Err(format!(
        "{}'s reflog holds work that only it remembers: no other ref reaches it and the merged commit does not contain it (made after approval, then reset away?):\n{}\n    keep it on a branch first (git branch <name> <commit>), or let it go: atelier clean {} --yes --discard {}",
        rec.name,
        kept.iter()
            .map(|l| format!("      {}", describe_lost(l)))
            .collect::<Vec<_>>()
            .join("\n"),
        rec.name,
        all.join(" ")
    ))
}

fn lost_commits(cx: &Cx, rec: &BranchRecord, facts: &MergeFacts) -> Check<Vec<Lost>> {
    let dir = cx.dir();
    let refname = names::branch_ref(&rec.name);
    let lines = |args: &[&str]| -> Check<Vec<String>> {
        let out = git::output(dir, args).map_err(|f| f.message)?;
        if !out.status.success() {
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .filter(|l| !l.is_empty())
            .collect())
    };
    let mut logged: Vec<String> = Vec::new();
    if git::output(dir, ["reflog", "exists", &refname]).is_ok_and(|o| o.status.success()) {
        // Newest first. Only what the branch held after it first reached the merged commit (the
        // tip approve recorded) is work made after approval; earlier states, an amended commit
        // for one, were superseded before anyone approved. With no such entry, all of it counts.
        let all = lines(&["reflog", "show", "--format=%H", &refname, "--"])?;
        let after = all
            .iter()
            .rposition(|c| *c == facts.commit)
            .unwrap_or(all.len());
        for c in &all[..after] {
            if *c != facts.commit && !logged.contains(c) {
                logged.push(c.clone());
            }
        }
    }
    if logged.is_empty() {
        return Ok(Vec::new());
    }
    // Every commit only the reflog reaches.
    let exclude = format!("--exclude={refname}");
    let mut args: Vec<&str> = vec!["rev-list", "--ignore-missing"];
    args.extend(logged.iter().map(String::as_str));
    args.extend(["--not", &facts.commit, &exclude, "--all"]);
    let unreached = lines(&args)?;
    if unreached.is_empty() {
        return Ok(Vec::new());
    }
    let mut lost: Vec<(String, usize)> = Vec::new();
    for c in logged.iter().filter(|c| unreached.contains(c)) {
        // Its commits whose change the merged commit's side has no equivalent of.
        let range = format!("{}...{c}", facts.commit);
        let unique = lines(&["rev-list", "--right-only", "--cherry-pick", &range])?;
        let n = unique.iter().filter(|u| unreached.contains(u)).count();
        if n > 0 {
            lost.push((c.clone(), n));
        }
    }
    if lost.is_empty() {
        return Ok(Vec::new());
    }
    // Only the tips: an earlier lost commit inside a later one's history goes with it.
    let mut args: Vec<&str> = vec!["merge-base", "--independent"];
    args.extend(lost.iter().map(|(c, _)| c.as_str()));
    let tips = lines(&args)?;
    let mut out = Vec::new();
    for (c, n) in lost.into_iter().filter(|(c, _)| tips.contains(c)) {
        let subject = lines(&["log", "-1", "--format=%s", &c])?
            .into_iter()
            .next()
            .unwrap_or_default();
        out.push(Lost {
            commit: c,
            subject,
            count: n,
        });
    }
    Ok(out)
}

fn remove_entry(file: &Path) -> std::result::Result<(), String> {
    match fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}
