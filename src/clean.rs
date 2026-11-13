//! `atelier clean [<branch>…] [--yes]` (DESIGN §3, r4 §3.9): deletes what was merged and what
//! atelier made, and nothing else.
//!
//! For each branch, under its lock (and the run lock of the worktree it deletes), nothing is
//! trusted that is not re-read from the disk now. The facts that grant authority are the
//! branch's record (state `merged`) and its [`MergeFacts`]: the commit that was merged and the
//! one worktree `approve` recorded, by canonical path and file-system identity. A branch whose
//! record is missing, active, or without those facts is never touched.
//!
//! Without `--yes` the same checks run and the plan is printed. With it, each deletion repeats its
//! checks immediately before it happens:
//!
//! - the worktree: the recorded path is a real folder with the recorded identity and no link in
//!   its path, the top of a registered worktree of this repository, not the main checkout and
//!   not the gate, with nothing nested in it or around it, the branch checked out there at the
//!   merged commit, no operation in progress, no uncommitted and no untracked file; removed with
//!   `git worktree remove`, never `--force` (git itself refuses a dirty or locked worktree);
//! - the branch: a plain ref still at the merged commit, not the integration branch, not checked
//!   out or open in a rebase or bisect anywhere; deleted with `update-ref --no-deref -d` at that
//!   commit;
//! - each registered temp folder: its path has exactly the shape `tmp` gives it and is canonical,
//!   it is a real folder with the registered identity owned by this user, and its `leftovers`
//!   file (things created outside the folder) is empty or absent; removed without following
//!   links;
//! - `mbx clean --under <worktree>` when mbx is a prerequisite and the worktree's path is gone.
//!
//! Then the record becomes `cleaned` and its facts are removed. Every condition of every step is
//! checked before the first deletion of a branch, so a refusal changes nothing; a step that fails
//! later leaves the record `merged`, and running `clean` again does the rest.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::exit::{Exit, Failure, Result};
use crate::git::{self, Checkout, canonical};
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::Context;
use crate::records::{BranchRecord, BranchState, MergeFacts, TmpEntry};
use crate::refs;
use crate::stack::Stack;

/// A reason a check refused.
type Check<T> = std::result::Result<T, String>;

/// The folder `merge` checks `M` out in: `<main checkout>.gate`, beside the main checkout.
pub fn gate_worktree(checkout: &Checkout) -> PathBuf {
    let main = main_root(checkout);
    let mut name = main.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(".gate");
    main.with_file_name(name)
}

/// The main checkout's folder, canonical.
pub fn main_root(checkout: &Checkout) -> PathBuf {
    let common = canonical(&checkout.common_dir);
    common.parent().map(Path::to_path_buf).unwrap_or(common)
}

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

struct Plan {
    wt: Wt,
    br: Br,
    tmps: Vec<Tmp>,
    mbx: bool,
}

pub fn run(ctx: &Context, branches: &[String], yes: bool) -> Result<Exit> {
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
    if targets.is_empty() {
        println!("clean: no merged branch");
        return Ok(Exit::Ok);
    }

    let (mut refused, mut failed) = (0, 0);
    for branch in &targets {
        println!("{branch}");
        match process(&cx, branch, yes) {
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
    if !yes {
        println!("clean: this is the plan; nothing was changed. Run again with --yes to do it");
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

fn process(cx: &Cx, branch: &str, yes: bool) -> Outcome {
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

    let plan = match plan(cx, &rec, &facts) {
        Ok(p) => p,
        Err(reasons) => return Outcome::Refused(reasons),
    };
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
    match execute(cx, &rec, &facts, &plan) {
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
    match &plan.wt {
        Wt::Ours(_) => lines.push(format!("remove worktree {p}")),
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
    }
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
    if plan.mbx {
        lines.push(format!("run mbx clean --under {p}"));
    }
    lines.push(format!("mark {} cleaned", rec.name));
    lines
}

/// Everything that must hold, read now. Every refusal is listed.
fn plan(cx: &Cx, rec: &BranchRecord, facts: &MergeFacts) -> std::result::Result<Plan, Vec<String>> {
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
    let br = match inspect_branch(cx, rec, facts, removing) {
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
    Ok(Plan { wt, br, tmps, mbx })
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
    Ok(Tmp::Remove {
        file: file.to_path_buf(),
        entry: entry.clone(),
    })
}

/// The path is exactly what `tmp` makes: `<temp root>/atelier/<repo id>/<branch key>/<id>` or
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
        && *a == "atelier"
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
) -> std::result::Result<(), String> {
    let p = &facts.worktree;
    let branch = &rec.name;
    if let Wt::Ours(_) = plan.wt {
        match inspect_worktree(cx, rec, facts)? {
            Wt::Ours(_) => {}
            _ => {
                return Err(format!(
                    "{} changed since the plan: nothing deleted",
                    p.display()
                ));
            }
        }
        let out = git::output(
            &cx.main,
            [
                std::ffi::OsStr::new("worktree"),
                "remove".as_ref(),
                p.as_os_str(),
            ],
        )
        .map_err(|f| f.message)?;
        if !out.status.success() {
            return Err(format!(
                "git worktree remove {}: {}",
                p.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        println!("  removed worktree {}", p.display());
    }
    if let Br::Delete = plan.br {
        match inspect_branch(cx, rec, facts, None)? {
            Br::Delete => {
                refs::delete(cx.dir(), &names::branch_ref(branch), &facts.commit)
                    .map_err(|f| f.message)?;
                println!("  deleted branch {branch}");
            }
            Br::Absent => {}
        }
    }
    for t in &plan.tmps {
        match t {
            Tmp::Forget { file, why } => {
                remove_entry(file)?;
                println!("  forgot {} ({why})", file.display());
            }
            Tmp::Remove { file, entry } => {
                match inspect_tmp(cx, rec, file, entry)? {
                    Tmp::Remove { .. } => {
                        if !entry.still_matches() {
                            return Err(format!(
                                "{} changed since the check: not deleted",
                                entry.path.display()
                            ));
                        }
                        fs::remove_dir_all(&entry.path)
                            .map_err(|e| format!("{}: {e}", entry.path.display()))?;
                        println!("  removed temp folder {}", entry.path.display());
                        if let Some(parent) = entry.path.parent()
                            && parent.file_name().and_then(|n| n.to_str())
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

fn remove_entry(file: &Path) -> std::result::Result<(), String> {
    match fs::remove_file(file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}
